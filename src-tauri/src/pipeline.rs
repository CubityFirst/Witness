//! Post-meeting processing: a single FIFO worker thread that turns raw WAVs
//! into transcript + archived Opus. Engines are loaded per job and dropped
//! right after so VRAM is freed between meetings.

use crate::db::{Db, NewSegment, NewSpeaker};
use crate::settings::{Engine, Settings};
use crate::{asr, diarize, encoder, ml_scheduler, models, resample, speaker_id, vad};
use anyhow::{bail, Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Job {
    pub meeting_id: i64,
    /// false = encode-only (auto_transcribe off).
    pub transcribe: bool,
    /// Engine override (retranscribe menu); None = settings default.
    pub engine: Option<Engine>,
}

#[derive(Debug)]
pub enum PipelineEvent {
    Progress {
        meeting_id: i64,
        stage: &'static str,
        pct: f32,
    },
    /// `rtf` = audio seconds per wall-clock second of transcription (a low
    /// number on a GPU box usually means the CUDA EP silently fell back to
    /// CPU — surfaced so that failure mode is visible).
    Complete {
        meeting_id: i64,
        rtf: Option<f32>,
    },
    Failed {
        meeting_id: i64,
        error: String,
    },
}

pub struct Pipeline {
    wake: Sender<()>,
    current: Arc<Mutex<Option<i64>>>,
    progress: Arc<Mutex<Option<PipelineProgress>>>,
    control: Arc<Mutex<()>>,
    db: Arc<Db>,
}

#[derive(Debug, Clone)]
pub struct PipelineProgress {
    pub stage: String,
    pub pct: f32,
}

impl Pipeline {
    /// The database is the source of truth; the bounded channel only wakes
    /// the worker. Re-enqueueing upgrades an encode-only job and increments
    /// its revision, so a user retranscription request cannot be swallowed by
    /// work that was already queued or running.
    pub fn enqueue(&self, job: Job) -> Result<()> {
        let engine = job.engine.map(|engine| engine.to_string());
        self.db
            .save_pipeline_job(job.meeting_id, job.transcribe, engine.as_deref())?;
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Disconnected(())) => bail!("pipeline worker is unavailable"),
        }
    }

    pub fn current_meeting(&self) -> Option<i64> {
        *self.current.lock().unwrap()
    }

    /// Most recent processing stage, retained for status polling when a UI
    /// opens after the corresponding progress event was emitted.
    pub fn progress(&self) -> Option<PipelineProgress> {
        self.progress.lock().unwrap().clone()
    }

    /// Cancel queued/failed work. The dequeue gate closes the race where a
    /// worker could otherwise claim the job between a purge check and delete.
    pub fn cancel(&self, meeting_id: i64) -> Result<bool> {
        let _control = self.control.lock().unwrap();
        if self.current_meeting() == Some(meeting_id) {
            bail!("meeting {meeting_id} is currently being processed");
        }
        self.db.cancel_pipeline_job(meeting_id)
    }

    pub fn queue_len(&self) -> usize {
        let count = self.db.pipeline_job_count().unwrap_or_default();
        count.saturating_sub(usize::from(self.current_meeting().is_some()))
    }
}

fn persisted_job(job: &crate::db::PersistedPipelineJob) -> Job {
    Job {
        meeting_id: job.meeting_id,
        transcribe: job.transcribe,
        engine: match job.engine.as_deref() {
            Some("parakeet") => Some(Engine::Parakeet),
            Some("whisper") => Some(Engine::Whisper),
            Some(other) => {
                log::warn!(
                    "pipeline job {} has unknown engine {other:?}; using the configured default",
                    job.meeting_id
                );
                None
            }
            None => None,
        },
    }
}

pub fn spawn(
    db: Arc<Db>,
    settings: Arc<Mutex<Settings>>,
    on_event: impl Fn(PipelineEvent) + Send + 'static,
) -> Pipeline {
    let (wake, rx): (Sender<()>, Receiver<()>) = crossbeam_channel::bounded(1);
    let current = Arc::new(Mutex::new(None));
    let current_worker = current.clone();
    let progress = Arc::new(Mutex::new(None));
    let progress_worker = progress.clone();
    let control = Arc::new(Mutex::new(()));
    let worker_control = control.clone();
    let worker_db = db.clone();

    std::thread::Builder::new()
        .name("pipeline".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                loop {
                    let saved = {
                        let _control = worker_control.lock().unwrap();
                        match worker_db.next_pipeline_job() {
                            Ok(Some(job)) => {
                                *current_worker.lock().unwrap() = Some(job.meeting_id);
                                job
                            }
                            Ok(None) => break,
                            Err(error) => {
                                log::error!("could not read the durable pipeline queue: {error:#}");
                                break;
                            }
                        }
                    };
                    let job = persisted_job(&saved);
                    let meeting_id = job.meeting_id;
                    *progress_worker.lock().unwrap() = None;
                    let emit = |event: PipelineEvent| {
                        match &event {
                            PipelineEvent::Progress { stage, pct, .. } => {
                                *progress_worker.lock().unwrap() = Some(PipelineProgress {
                                    stage: (*stage).to_string(),
                                    pct: *pct,
                                });
                            }
                            PipelineEvent::Complete { .. } | PipelineEvent::Failed { .. } => {
                                *progress_worker.lock().unwrap() = None;
                            }
                        }
                        on_event(event);
                    };
                    let result = process(
                        &worker_db,
                        &settings,
                        &job,
                        saved.revision,
                        &emit,
                    );
                    *current_worker.lock().unwrap() = None;
                    match result {
                        Ok(rtf) => match worker_db
                            .complete_pipeline_job(meeting_id, saved.revision)
                        {
                            Ok(true) => {
                                emit(PipelineEvent::Complete { meeting_id, rtf });
                            }
                            Ok(false) => {
                                log::info!(
                                    "pipeline job {meeting_id} was updated while running; processing the new revision"
                                );
                            }
                            Err(error) => {
                                log::error!(
                                    "pipeline job {meeting_id} completed but its queue record could not be removed: {error:#}"
                                );
                                emit(PipelineEvent::Failed {
                                    meeting_id,
                                    error: format!("processing completed but queue commit failed: {error:#}"),
                                });
                                break;
                            }
                        },
                        Err(error) => {
                            let message = format!("{error:#}");
                            log::error!("pipeline job for meeting {meeting_id} failed: {message}");
                            match worker_db.fail_pipeline_job(
                                meeting_id,
                                saved.revision,
                                &message,
                            ) {
                                Ok(true) => {
                                    let transcript_committed = worker_db
                                        .get_meeting(meeting_id)
                                        .ok()
                                        .flatten()
                                        .is_some_and(|meeting| meeting.status == "transcribed");
                                    if !transcript_committed {
                                        let _ = worker_db.set_status(meeting_id, "failed");
                                    }
                                    emit(PipelineEvent::Failed {
                                        meeting_id,
                                        error: message,
                                    });
                                }
                                Ok(false) => log::info!(
                                    "failed pipeline job {meeting_id} was superseded; retrying the new revision"
                                ),
                                Err(db_error) => {
                                    let _ = worker_db.set_status(meeting_id, "failed");
                                    emit(PipelineEvent::Failed {
                                        meeting_id,
                                        error: format!(
                                            "{message}; additionally could not save failure state: {db_error:#}"
                                        ),
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        })
        .expect("spawn pipeline thread");

    if db.pipeline_job_count().unwrap_or_default() > 0 {
        let _ = wake.try_send(());
    }

    Pipeline {
        wake,
        current,
        progress,
        control,
        db,
    }
}

fn load_wav_16k(path: &PathBuf) -> Result<(Vec<f32>, f32)> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    anyhow::ensure!(spec.channels == 1, "{} is not mono", path.display());
    anyhow::ensure!(
        spec.sample_format == hound::SampleFormat::Int && spec.bits_per_sample == 16,
        "{} is not 16-bit PCM",
        path.display()
    );
    let audio_secs = reader.duration() as f32 / spec.sample_rate as f32;
    let mut resampler = resample::StreamResampler::new(spec.sample_rate, 16_000)?;
    let mut input = Vec::with_capacity(16_384);
    let mut output = Vec::with_capacity((audio_secs * 16_000.0) as usize);
    for sample in reader.samples::<i16>() {
        input.push(sample? as f32 / 32768.0);
        if input.len() == input.capacity() {
            output.append(&mut resampler.push(&input)?);
            input.clear();
        }
    }
    output.append(&mut resampler.push(&input)?);
    output.append(&mut resampler.finish()?);
    Ok((output, audio_secs))
}

fn transcribe_chunks(
    engine: &mut dyn asr::AsrEngine,
    track: &[f32],
    chunks: &[vad::SpeechChunk],
    progress_base: f32,
    progress_span: f32,
    junk_phrases: &[String],
    progress: &impl Fn(&'static str, f32),
) -> Result<Vec<asr::AsrSegment>> {
    let total_ms = vad::total_ms(chunks).max(1);
    let mut done_ms = 0u64;
    let mut segments = Vec::new();
    for chunk in chunks {
        let samples = chunk.samples(track);
        let transcribed = {
            let _permit = ml_scheduler::batch();
            engine.transcribe(samples, chunk.start_ms)?
        };
        segments.extend(
            transcribed
                .into_iter()
                .filter(|segment| !asr::is_junk_text_with(&segment.text, junk_phrases)),
        );
        done_ms += samples.len() as u64 / 16;
        progress(
            "asr",
            progress_base + progress_span * done_ms as f32 / total_ms as f32,
        );
    }
    Ok(segments)
}

fn extend_capped(output: &mut Vec<f32>, input: &[f32], max_samples: usize) {
    let remaining = max_samples.saturating_sub(output.len());
    output.extend_from_slice(&input[..input.len().min(remaining)]);
}

fn process(
    db: &Db,
    settings: &Mutex<Settings>,
    job: &Job,
    revision: i64,
    on_event: &impl Fn(PipelineEvent),
) -> Result<Option<f32>> {
    let meeting_id = job.meeting_id;
    let progress = |stage: &'static str, pct: f32| {
        on_event(PipelineEvent::Progress {
            meeting_id,
            stage,
            pct: pct * 100.0,
        });
    };

    let meeting = db
        .get_meeting(meeting_id)?
        .with_context(|| format!("meeting {meeting_id} not in database"))?;

    let (rec_dir, audio_dir, models_dir, default_engine, bitrate, threshold, junk_phrases) = {
        let s = settings.lock().unwrap();
        (
            s.rec_tmp_dir(),
            s.audio_dir(),
            s.models_dir(),
            s.engine,
            s.opus_bitrate_kbps,
            s.speaker_match_threshold,
            s.junk_phrases.clone(),
        )
    };
    let mut rtf: Option<f32> = None;
    let (mic_wav, loop_wav, meta_path) = crate::recorder::wav_paths(&rec_dir, meeting_id);

    let have_wavs = mic_wav.exists() && loop_wav.exists();

    if job.transcribe {
        db.set_status(meeting_id, "processing")?;
        let engine_kind = job.engine.unwrap_or(default_engine);
        let work_started = std::time::Instant::now();

        // ---- source audio + resampling ----
        // Raw recordings are loaded and downsampled one track at a time so a
        // long meeting never holds both 48 kHz WAVs and both 16 kHz copies in
        // memory simultaneously. Encode-only jobs do not load audio at all.
        progress("vad", 0.0);
        let archive_path = if have_wavs {
            None
        } else if let Some(relative) = &meeting.audio_path {
            let path = crate::commands::safe_archive_path(&audio_dir, relative)
                .map_err(anyhow::Error::msg)?;
            if !path.exists() {
                bail!(
                    "no source audio: WAVs cleaned up and {} missing",
                    path.display()
                );
            }
            Some(path)
        } else {
            bail!("no source audio for meeting {meeting_id}");
        };
        let (mic_16k, audio_secs) = if have_wavs {
            load_wav_16k(&mic_wav)?
        } else {
            let samples = encoder::decode_opus_track_16k(
                archive_path.as_ref().unwrap(),
                encoder::ArchiveTrack::Mic,
            )?;
            let seconds = samples.len() as f32 / 16_000.0;
            (samples, seconds)
        };

        // ---- VAD ----
        let mic_chunks = vad::chunk_speech(&mic_16k);
        progress("vad", 0.5);
        let mic_kept_ms = vad::total_ms(&mic_chunks);

        // ---- ASR (both tracks) ----
        progress("asr", 0.0);
        if engine_kind == Engine::Parakeet {
            // Repair/flag CUDA library resolution before ort can silently
            // fall back to CPU; the warning lands next to this job's rtf line.
            crate::gpu::preflight();
        }
        let (mic_segments, loop_segments, loop_16k, loop_chunks, loop_kept_ms) = {
            let mut engine = {
                let _permit = ml_scheduler::batch();
                asr::create_engine(engine_kind, &models_dir)?
            };
            let mic_segments = transcribe_chunks(
                engine.as_mut(),
                &mic_16k,
                &mic_chunks,
                0.0,
                0.5,
                &junk_phrases,
                &progress,
            )?;
            drop(mic_chunks);
            drop(mic_16k);

            let loop_16k = if have_wavs {
                load_wav_16k(&loop_wav)?.0
            } else {
                encoder::decode_opus_track_16k(
                    archive_path.as_ref().unwrap(),
                    encoder::ArchiveTrack::Loopback,
                )?
            };
            let loop_chunks = vad::chunk_speech(&loop_16k);
            progress("vad", 1.0);
            let loop_kept_ms = vad::total_ms(&loop_chunks);
            let loop_segments = transcribe_chunks(
                engine.as_mut(),
                &loop_16k,
                &loop_chunks,
                0.5,
                0.5,
                &junk_phrases,
                &progress,
            )?;
            (
                mic_segments,
                loop_segments,
                loop_16k,
                loop_chunks,
                loop_kept_ms,
            )
        }; // engine dropped here → VRAM freed before diarization
        log::info!(
            "meeting {meeting_id}: VAD kept {:.1}s mic / {:.1}s loopback of {:.1}s",
            mic_kept_ms as f64 / 1000.0,
            loop_kept_ms as f64 / 1000.0,
            audio_secs
        );

        // ---- diarization (loopback only; graceful without the model) ----
        progress("diarize", 0.0);
        let diar = if loop_segments.is_empty() {
            Vec::new()
        } else if models::sortformer_path(&models_dir).is_some() {
            diarize::diarize(&models_dir, &loop_16k, |p| progress("diarize", p))?
        } else {
            log::warn!("Diarization model missing — labelling all remote speech S1");
            Vec::new()
        };

        // ---- merge segments ----
        let mut seen = [false; diarize::MAX_SPEAKERS];
        let mut segments: Vec<NewSegment> = Vec::new();
        for seg in &mic_segments {
            segments.push(NewSegment {
                speaker_label: Some("me".into()),
                track: "mic".into(),
                start_ms: seg.start_ms as i64,
                end_ms: seg.end_ms as i64,
                text: seg.text.clone(),
            });
        }
        for seg in &loop_segments {
            let spk = diarize::assign_speaker(&diar, seg.start_ms, seg.end_ms).unwrap_or(0);
            let spk = spk.min(diarize::MAX_SPEAKERS - 1);
            seen[spk] = true;
            segments.push(NewSegment {
                speaker_label: Some(format!("S{}", spk + 1)),
                track: "loopback".into(),
                start_ms: seg.start_ms as i64,
                end_ms: seg.end_ms as i64,
                text: seg.text.clone(),
            });
        }
        segments.sort_by_key(|s| (s.start_ms, s.end_ms));

        // ---- voice prints & auto-labeling ----
        // Gather each remote speaker's audio (from diarization when present,
        // else everything the VAD kept — single-speaker fallback).
        let max_samples = (speaker_id::MAX_SPEECH_SECONDS * 16_000.0) as usize;
        let mut spk_audio: [Vec<f32>; diarize::MAX_SPEAKERS] = Default::default();
        if !diar.is_empty() {
            for d in &diar {
                if d.speaker >= diarize::MAX_SPEAKERS || spk_audio[d.speaker].len() >= max_samples {
                    continue;
                }
                let s = (d.start_ms as usize * 16).min(loop_16k.len());
                let e = (d.end_ms as usize * 16).min(loop_16k.len());
                if e > s {
                    extend_capped(&mut spk_audio[d.speaker], &loop_16k[s..e], max_samples);
                }
            }
        } else if seen[0] {
            for chunk in &loop_chunks {
                if spk_audio[0].len() >= max_samples {
                    break;
                }
                extend_capped(&mut spk_audio[0], chunk.samples(&loop_16k), max_samples);
            }
        }

        let mut speakers: Vec<NewSpeaker> = vec![NewSpeaker {
            label: "me".into(),
            display_name: "Me".into(),
            embedding: None,
            emb_seconds: 0.0,
            person_id: None,
            auto_labeled: false,
        }];
        let people = db.list_people()?;
        let mut embedder: Option<speaker_id::Embedder> = None;
        for (i, seen) in seen.iter().enumerate() {
            if !*seen {
                continue;
            }
            let secs = spk_audio[i].len() as f64 / 16_000.0;
            let mut speaker = NewSpeaker {
                label: format!("S{}", i + 1),
                display_name: format!("Speaker {}", i + 1),
                embedding: None,
                emb_seconds: secs,
                person_id: None,
                auto_labeled: false,
            };
            if secs >= speaker_id::MIN_SPEECH_SECONDS
                && models::speaker_id_path(&models_dir).is_some()
            {
                if embedder.is_none() {
                    let created = {
                        let _permit = ml_scheduler::batch();
                        speaker_id::Embedder::new(&models_dir)
                    };
                    match created {
                        Ok(e) => embedder = Some(e),
                        Err(e) => log::warn!("speaker-ID model unavailable: {e:#}"),
                    }
                }
                if let Some(emb) = embedder.as_mut() {
                    let embedded = {
                        let _permit = ml_scheduler::batch();
                        emb.embed(&spk_audio[i])
                    };
                    match embedded {
                        Ok(embedding) => {
                            if let Some((pid, name, sim)) =
                                speaker_id::best_match(&people, &embedding, threshold)
                            {
                                log::info!(
                                    "meeting {meeting_id}: auto-labeled {} as {} (cosine {:.2})",
                                    speaker.label,
                                    name,
                                    sim
                                );
                                speaker.display_name = name;
                                speaker.person_id = Some(pid);
                                speaker.auto_labeled = true;
                            }
                            speaker.embedding = Some(embedding);
                        }
                        Err(e) => log::warn!("voice print for {} failed: {e:#}", speaker.label),
                    }
                }
            }
            speakers.push(speaker);
        }
        drop(embedder);

        // Retranscribing must not lose names the user set by hand: carry
        // manual labels (and their person links) forward by label.
        for old in db.get_speakers(meeting_id)? {
            if old.auto_labeled {
                continue; // auto labels may be re-derived freely
            }
            let default_name = if old.label == "me" {
                "Me".to_string()
            } else {
                old.label
                    .strip_prefix('S')
                    .map(|n| format!("Speaker {n}"))
                    .unwrap_or_else(|| old.label.clone())
            };
            if old.display_name == default_name && old.person_id.is_none() {
                continue; // never customized
            }
            if let Some(new) = speakers.iter_mut().find(|s| s.label == old.label) {
                new.display_name = old.display_name.clone();
                new.person_id = old.person_id;
                new.auto_labeled = false;
            }
        }

        db.replace_transcript(meeting_id, &speakers, &segments, &engine_kind.to_string())?;
        // Transcript and archive are separate durable phases. If encoding or
        // cleanup fails from here on, the retry must not repeat expensive ASR.
        db.mark_pipeline_transcription_complete(meeting_id, revision)?;
        let wall = work_started.elapsed().as_secs_f32().max(0.001);
        rtf = Some(audio_secs / wall);
        log::info!(
            "meeting {meeting_id}: transcribed {audio_secs:.0}s of audio in {wall:.1}s ({:.1}x realtime)",
            audio_secs / wall
        );
    }

    // ---- Opus archive (idempotent: skipped when already encoded) ----
    if meeting.audio_path.is_none() {
        if !have_wavs {
            bail!("cannot encode: WAV files missing");
        }
        progress("encode", 0.0);
        let file_name = format!("{meeting_id}.opus");
        let out_path = audio_dir.join(&file_name);
        encoder::encode_opus(&mic_wav, &loop_wav, &out_path, bitrate, |p| {
            progress("encode", p)
        })?;
        db.set_audio_path(meeting_id, &file_name)?;
    }

    // WAVs are only removed once transcript (if requested) and Opus both exist.
    if have_wavs {
        let transcribed_ok = !job.transcribe
            || db
                .get_meeting(meeting_id)?
                .map(|m| m.status == "transcribed")
                .unwrap_or(false);
        if transcribed_ok {
            let mut cleanup_errors = Vec::new();
            for path in [&mic_wav, &loop_wav, &meta_path] {
                if let Err(error) = std::fs::remove_file(path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        cleanup_errors.push(format!("{}: {error}", path.display()));
                    }
                }
            }
            if !cleanup_errors.is_empty() {
                bail!(
                    "archive committed but raw recording cleanup failed: {}",
                    cleanup_errors.join("; ")
                );
            }
        }
    }
    Ok(rtf)
}

#[cfg(test)]
mod tests {
    use super::extend_capped;

    #[test]
    fn speaker_audio_never_exceeds_exact_cap() {
        let mut output = vec![1.0; 7];
        extend_capped(&mut output, &[2.0; 20], 10);
        assert_eq!(output.len(), 10);
        assert_eq!(&output[7..], &[2.0; 3]);
        extend_capped(&mut output, &[3.0; 5], 10);
        assert_eq!(output.len(), 10);
    }
}
