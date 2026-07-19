//! Live (provisional) transcription while recording: the recorder's writer
//! thread tees each track's 48 kHz audio here; this worker resamples to
//! 16 kHz, VAD-gates it, and transcribes buffered speech with Parakeet
//! whenever the speaker pauses (~0.7 s) or the buffer hits 15 s. Lines go
//! out via the callback as captions — the end-of-meeting pipeline replaces
//! them with the full-quality diarized transcript.
//!
//! The engine loads once per recording inside this thread and drops when
//! the recorder shuts the channel, freeing VRAM between meetings.

use crate::asr::AsrEngine;
use crate::audio_capture::TrackKind;
use crate::resample::StreamResampler;
use crate::settings::Engine;
use anyhow::Result;
use crossbeam_channel::Sender;
use earshot::Detector;
use std::path::PathBuf;

pub enum LiveMsg {
    Audio { kind: TrackKind, samples_48k: Vec<f32> },
    /// Timeline gap (silence the writer inserted), in 48 kHz samples.
    Silence { kind: TrackKind, count_48k: u64 },
}

const FRAME: usize = 256; // earshot frame @ 16 kHz = 16 ms
const FRAME_MS: u64 = 16;
/// Cut and transcribe once the speaker has paused this long.
const SILENCE_CUT_FRAMES: usize = (700 / FRAME_MS) as usize;
/// Hard cut mid-speech at this buffer size (keeps latency bounded).
const MAX_BUFFER: usize = 15 * 16_000;
/// Trailing pad kept after the last voiced frame when cutting.
const PAD_FRAMES: usize = 12;
/// Drop speechless audio once it exceeds this (keep a small tail).
const IDLE_DROP: usize = 3 * 16_000;

struct TrackState {
    kind: TrackKind,
    resampler: StreamResampler,
    detector: Detector,
    pending: Vec<f32>,   // 16 kHz samples awaiting transcription
    pending_start: u64,  // absolute 16 kHz sample index of pending[0]
    frames_checked: usize,
    last_voice_frame: Option<usize>,
}

impl TrackState {
    fn new(kind: TrackKind) -> Result<TrackState> {
        Ok(TrackState {
            kind,
            resampler: StreamResampler::new(48_000, 16_000)?,
            detector: Detector::const_default(),
            pending: Vec::new(),
            pending_start: 0,
            frames_checked: 0,
            last_voice_frame: None,
        })
    }

    fn reset_vad(&mut self) {
        self.frames_checked = 0;
        self.last_voice_frame = None;
        self.detector.reset();
    }

    /// VAD any newly complete frames, then return the cut point (in samples)
    /// if a chunk is ready to transcribe.
    fn scan(&mut self) -> Option<usize> {
        let total_frames = self.pending.len() / FRAME;
        while self.frames_checked < total_frames {
            let i = self.frames_checked;
            let frame = &self.pending[i * FRAME..(i + 1) * FRAME];
            if self.detector.predict_f32(frame) > 0.5 {
                self.last_voice_frame = Some(i);
            }
            self.frames_checked += 1;
        }
        match self.last_voice_frame {
            Some(last) => {
                let trailing_silence = total_frames.saturating_sub(last + 1);
                if trailing_silence >= SILENCE_CUT_FRAMES {
                    Some(((last + 1 + PAD_FRAMES).min(total_frames)) * FRAME)
                } else if self.pending.len() >= MAX_BUFFER {
                    Some(self.pending.len())
                } else {
                    None
                }
            }
            None => {
                if self.pending.len() > IDLE_DROP {
                    // Nothing voiced — slide the window, keep a short tail.
                    let keep = 8 * FRAME;
                    let drop = self.pending.len() - keep;
                    self.pending.drain(..drop);
                    self.pending_start += drop as u64;
                    self.reset_vad();
                }
                None
            }
        }
    }

    fn take_chunk(&mut self, cut: usize) -> (u64, Vec<f32>) {
        let chunk: Vec<f32> = self.pending.drain(..cut).collect();
        let start = self.pending_start;
        self.pending_start += cut as u64;
        self.reset_vad();
        (start, chunk)
    }
}

/// Spawns the live worker; audio goes in through the returned sender, and
/// the thread exits (dropping the engine) when all senders are dropped.
/// Returns None when the Parakeet model isn't downloaded.
pub fn spawn(
    models_dir: PathBuf,
    on_line: impl Fn(&'static str, u64, u64, String) + Send + 'static,
) -> Option<Sender<LiveMsg>> {
    crate::models::parakeet_dir(&models_dir)?;
    let (tx, rx) = crossbeam_channel::unbounded::<LiveMsg>();

    std::thread::Builder::new()
        .name("live-transcribe".into())
        .spawn(move || {
            let mut engine: Box<dyn AsrEngine> =
                match crate::asr::create_engine(Engine::Parakeet, &models_dir) {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("live transcription disabled: {e:#}");
                        return; // channel closes; recorder sends fail silently
                    }
                };
            log::info!("live transcription ready");

            let mut mic = match TrackState::new(TrackKind::Mic) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("live transcription disabled: {e:#}");
                    return;
                }
            };
            let mut lop = match TrackState::new(TrackKind::Loopback) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("live transcription disabled: {e:#}");
                    return;
                }
            };

            let mut transcribe = |track: &mut TrackState, cut: usize| {
                let (start, chunk) = track.take_chunk(cut);
                let start_ms = start * 1000 / 16_000;
                match engine.transcribe(&chunk, start_ms) {
                    Ok(segments) => {
                        for seg in segments {
                            if !seg.text.is_empty() {
                                on_line(track.kind.name(), seg.start_ms, seg.end_ms, seg.text);
                            }
                        }
                    }
                    Err(e) => log::warn!("live chunk failed: {e:#}"),
                }
            };

            while let Ok(msg) = rx.recv() {
                match msg {
                    LiveMsg::Audio { kind, samples_48k } => {
                        let track = if kind == TrackKind::Mic { &mut mic } else { &mut lop };
                        match track.resampler.push(&samples_48k) {
                            Ok(out) => track.pending.extend_from_slice(&out),
                            Err(e) => {
                                log::warn!("live resample failed: {e:#}");
                                continue;
                            }
                        }
                        if let Some(cut) = track.scan() {
                            transcribe(track, cut);
                        }
                    }
                    LiveMsg::Silence { kind, count_48k } => {
                        // A timeline gap: flush any pending speech, then jump.
                        let track = if kind == TrackKind::Mic { &mut mic } else { &mut lop };
                        if track.last_voice_frame.is_some() {
                            let cut = track.pending.len();
                            transcribe(track, cut);
                        }
                        track.pending_start += track.pending.len() as u64 + count_48k / 3;
                        track.pending.clear();
                        track.reset_vad();
                    }
                }
            }

            // Recording ended: flush whatever speech is left on both tracks.
            for track in [&mut mic, &mut lop] {
                if track.last_voice_frame.is_some() {
                    let cut = track.pending.len();
                    transcribe(track, cut);
                }
            }
            log::info!("live transcription stopped");
        })
        .ok()?;

    Some(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end live-caption test: streams the TTS WAV through the live
    /// channel in 100 ms chunks and expects caption lines back.
    /// Needs %TEMP%\witness-tts.wav and the Parakeet model (cached by the
    /// earlier smoke tests). `cargo test live_captions -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_captions() {
        let models_dir = std::env::var("WITNESS_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/debug/models"));
        let wav = std::env::temp_dir().join("witness-tts.wav");
        let mut reader = hound::WavReader::open(&wav).expect("generate TTS wav first");
        let samples_16k: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        let samples_48k = crate::resample::resample_all(&samples_16k, 16_000, 48_000).unwrap();

        let (line_tx, line_rx) = crossbeam_channel::unbounded();
        let tx = spawn(models_dir, move |track, start_ms, end_ms, text| {
            let _ = line_tx.send((track, start_ms, end_ms, text));
        })
        .expect("Parakeet model missing — run cuda_smoke first");

        for chunk in samples_48k.chunks(4800) {
            tx.send(LiveMsg::Audio {
                kind: TrackKind::Mic,
                samples_48k: chunk.to_vec(),
            })
            .unwrap();
        }
        // A trailing second of silence lets the VAD cut the last utterance.
        for _ in 0..12 {
            tx.send(LiveMsg::Audio { kind: TrackKind::Mic, samples_48k: vec![0.0; 4800] })
                .unwrap();
        }
        drop(tx); // worker flushes and exits

        let mut all_text = String::new();
        while let Ok((track, start_ms, end_ms, text)) = line_rx.recv_timeout(std::time::Duration::from_secs(60)) {
            println!("live [{track} {start_ms}-{end_ms} ms] {text}");
            all_text.push_str(&text);
            all_text.push(' ');
        }
        let lower = all_text.to_lowercase();
        assert!(
            lower.contains("fox") && lower.contains("transcription"),
            "unexpected live captions: {all_text}"
        );
    }
}
