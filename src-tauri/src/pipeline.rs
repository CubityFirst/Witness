//! Post-meeting processing: a single FIFO worker thread that turns raw WAVs
//! into transcript + archived Opus. Engines are loaded per job and dropped
//! right after so VRAM is freed between meetings.

use crate::db::{Db, NewSegment, NewSpeaker};
use crate::settings::{Engine, Settings};
use crate::{asr, diarize, encoder, models, resample, speaker_id, vad};
use anyhow::{bail, Context, Result};
use crossbeam_channel::{Receiver, Sender};
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
    Progress { meeting_id: i64, stage: &'static str, pct: f32 },
    /// `rtf` = audio seconds per wall-clock second of transcription (a low
    /// number on a GPU box usually means the CUDA EP silently fell back to
    /// CPU — surfaced so that failure mode is visible).
    Complete { meeting_id: i64, rtf: Option<f32> },
    Failed { meeting_id: i64, error: String },
}

pub struct Pipeline {
    tx: Sender<Job>,
    current: Arc<Mutex<Option<i64>>>,
}

impl Pipeline {
    pub fn enqueue(&self, job: Job) {
        let _ = self.tx.send(job);
    }

    pub fn current_meeting(&self) -> Option<i64> {
        *self.current.lock().unwrap()
    }

    pub fn queue_len(&self) -> usize {
        self.tx.len()
    }
}

pub fn spawn(
    db: Arc<Db>,
    settings: Arc<Mutex<Settings>>,
    on_event: impl Fn(PipelineEvent) + Send + 'static,
) -> Pipeline {
    let (tx, rx): (Sender<Job>, Receiver<Job>) = crossbeam_channel::unbounded();
    let current = Arc::new(Mutex::new(None));
    let current_worker = current.clone();

    std::thread::Builder::new()
        .name("pipeline".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                let meeting_id = job.meeting_id;
                *current_worker.lock().unwrap() = Some(meeting_id);
                let result = process(&db, &settings, &job, &on_event);
                *current_worker.lock().unwrap() = None;
                match result {
                    Ok(rtf) => on_event(PipelineEvent::Complete { meeting_id, rtf }),
                    Err(e) => {
                        log::error!("pipeline job for meeting {meeting_id} failed: {e:#}");
                        let _ = db.set_status(meeting_id, "failed");
                        on_event(PipelineEvent::Failed {
                            meeting_id,
                            error: format!("{e:#}"),
                        });
                    }
                }
            }
        })
        .expect("spawn pipeline thread");

    Pipeline { tx, current }
}

fn load_wav_f32(path: &PathBuf) -> Result<Vec<f32>> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let samples: Result<Vec<i16>, _> = reader.samples::<i16>().collect();
    Ok(samples?.into_iter().map(|s| s as f32 / 32768.0).collect())
}

fn process(
    db: &Db,
    settings: &Mutex<Settings>,
    job: &Job,
    on_event: &(impl Fn(PipelineEvent) + Send),
) -> Result<Option<f32>> {
    let meeting_id = job.meeting_id;
    let progress = |stage: &'static str, pct: f32| {
        on_event(PipelineEvent::Progress { meeting_id, stage, pct: pct * 100.0 });
    };

    let meeting = db
        .get_meeting(meeting_id)?
        .with_context(|| format!("meeting {meeting_id} not in database"))?;

    let (rec_dir, audio_dir, models_dir, default_engine, bitrate, threshold) = {
        let s = settings.lock().unwrap();
        (
            s.rec_tmp_dir(),
            s.audio_dir(),
            s.models_dir(),
            s.engine,
            s.opus_bitrate_kbps,
            s.speaker_match_threshold,
        )
    };
    let mut rtf: Option<f32> = None;
    let (mic_wav, loop_wav, meta_path) = crate::recorder::wav_paths(&rec_dir, meeting_id);

    // ---- source audio: prefer raw WAVs, else decode the archived Opus ----
    let have_wavs = mic_wav.exists() && loop_wav.exists();
    let (mic_48k, loop_48k) = if have_wavs {
        (load_wav_f32(&mic_wav)?, load_wav_f32(&loop_wav)?)
    } else if let Some(rel) = &meeting.audio_path {
        let path = audio_dir.join(rel);
        if !path.exists() {
            bail!("no source audio: WAVs cleaned up and {} missing", path.display());
        }
        encoder::decode_opus(&path)?
    } else {
        bail!("no source audio for meeting {meeting_id}");
    };

    if job.transcribe {
        db.set_status(meeting_id, "processing")?;
        let engine_kind = job.engine.unwrap_or(default_engine);
        let work_started = std::time::Instant::now();

        // ---- VAD ----
        progress("vad", 0.0);
        let mic_16k = resample::resample_all(&mic_48k, 48_000, 16_000)?;
        let loop_16k = resample::resample_all(&loop_48k, 48_000, 16_000)?;
        let mic_chunks = vad::chunk_speech(&mic_16k);
        progress("vad", 0.5);
        let loop_chunks = vad::chunk_speech(&loop_16k);
        progress("vad", 1.0);
        log::info!(
            "meeting {meeting_id}: VAD kept {:.1}s mic / {:.1}s loopback of {:.1}s",
            vad::total_ms(&mic_chunks) as f64 / 1000.0,
            vad::total_ms(&loop_chunks) as f64 / 1000.0,
            mic_48k.len() as f64 / 48_000.0
        );

        // ---- ASR (both tracks) ----
        progress("asr", 0.0);
        let total_ms = (vad::total_ms(&mic_chunks) + vad::total_ms(&loop_chunks)).max(1);
        let mut done_ms = 0u64;
        let mut mic_segments = Vec::new();
        let mut loop_segments = Vec::new();
        {
            let mut engine = asr::create_engine(engine_kind, &models_dir)?;
            for (chunks, out) in [
                (&mic_chunks, &mut mic_segments),
                (&loop_chunks, &mut loop_segments),
            ] {
                for chunk in chunks.iter() {
                    // Noise chunks the VAD let through make the engines
                    // hallucinate ("Thank you." on a mic pop) — drop those.
                    out.extend(
                        engine
                            .transcribe(&chunk.samples, chunk.start_ms)?
                            .into_iter()
                            .filter(|s| !asr::is_junk_text(&s.text)),
                    );
                    done_ms += chunk.samples.len() as u64 / 16;
                    progress("asr", done_ms as f32 / total_ms as f32);
                }
            }
        } // engine dropped here → VRAM freed before diarization

        // ---- diarization (loopback only; graceful without the model) ----
        progress("diarize", 0.0);
        let diar = if loop_segments.is_empty() {
            Vec::new()
        } else if models::sortformer_path(&models_dir).is_some() {
            diarize::diarize(&models_dir, &loop_16k, |p| progress("diarize", p))?
        } else {
            log::warn!("Sortformer model missing — labelling all remote speech S1");
            Vec::new()
        };

        // ---- merge segments ----
        let mut seen = [false; 4];
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
            let spk = spk.min(3);
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
        let mut spk_audio: [Vec<f32>; 4] = Default::default();
        if !diar.is_empty() {
            for d in &diar {
                if d.speaker >= 4 || spk_audio[d.speaker].len() >= max_samples {
                    continue;
                }
                let s = (d.start_ms as usize * 16).min(loop_16k.len());
                let e = (d.end_ms as usize * 16).min(loop_16k.len());
                if e > s {
                    spk_audio[d.speaker].extend_from_slice(&loop_16k[s..e]);
                }
            }
        } else if seen[0] {
            for chunk in &loop_chunks {
                if spk_audio[0].len() >= max_samples {
                    break;
                }
                spk_audio[0].extend_from_slice(&chunk.samples);
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
                    match speaker_id::Embedder::new(&models_dir) {
                        Ok(e) => embedder = Some(e),
                        Err(e) => log::warn!("speaker-ID model unavailable: {e:#}"),
                    }
                }
                if let Some(emb) = embedder.as_mut() {
                    match emb.embed(&spk_audio[i]) {
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
        let audio_secs = mic_48k.len() as f32 / 48_000.0;
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
            let _ = std::fs::remove_file(&mic_wav);
            let _ = std::fs::remove_file(&loop_wav);
            let _ = std::fs::remove_file(&meta_path);
        }
    }
    Ok(rtf)
}
