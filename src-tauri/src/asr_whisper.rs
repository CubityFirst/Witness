//! Whisper large-v3-turbo via whisper-rs (whisper.cpp). CPU by default;
//! build with `--features whisper-cuda` for GPU (needs the CUDA toolkit).

use crate::asr::{AsrEngine, AsrSegment};
use anyhow::{Context, Result};
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub struct WhisperEngine {
    ctx: WhisperContext,
}

impl WhisperEngine {
    pub fn new(models_dir: &Path) -> Result<WhisperEngine> {
        let model_path = crate::models::whisper_path(models_dir)
            .context("Whisper model not downloaded — fetch it in Settings → Models")?;
        let ctx = WhisperContext::new_with_params(
            model_path.to_str().context("model path is not UTF-8")?,
            WhisperContextParameters::default(),
        )
        .context("loading Whisper model")?;
        Ok(WhisperEngine { ctx })
    }
}

impl AsrEngine for WhisperEngine {
    fn transcribe(&mut self, samples_16k: &[f32], offset_ms: u64) -> Result<Vec<AsrSegment>> {
        let mut state = self.ctx.create_state().context("creating Whisper state")?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("auto"));
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4);
        params.set_n_threads(threads.min(8));

        state
            .full(params, samples_16k)
            .context("Whisper inference")?;

        let n = state.full_n_segments().context("segment count")?;
        let mut segments = Vec::with_capacity(n as usize);
        for i in 0..n {
            let text = state.full_get_segment_text(i).context("segment text")?;
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            // whisper timestamps are in 10 ms units.
            let t0 = state.full_get_segment_t0(i).context("segment t0")? as u64 * 10;
            let t1 = state.full_get_segment_t1(i).context("segment t1")? as u64 * 10;
            segments.push(AsrSegment {
                start_ms: offset_ms + t0,
                end_ms: offset_ms + t1,
                text,
            });
        }
        Ok(segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whisper engine end-to-end on the same TTS WAV as the Parakeet test
    /// (downloads ggml-large-v3-turbo-q5_0 ~570 MB on first run, CPU).
    /// `cargo test whisper_tts -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn whisper_tts() {
        let models_dir = std::env::var("WITNESS_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/debug/models"));
        std::fs::create_dir_all(&models_dir).unwrap();
        crate::models::download(
            crate::settings::Engine::Whisper,
            &models_dir,
            |m, f, d, t| {
                if let Some(t) = t {
                    println!("{m}/{f}: {} / {} MB", d / 1_000_000, t / 1_000_000);
                }
            },
        )
        .unwrap();

        let wav = std::env::temp_dir().join("witness-tts.wav");
        let mut reader = hound::WavReader::open(&wav).expect("run the TTS generation step first");
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();

        let mut engine = WhisperEngine::new(&models_dir).unwrap();
        let mut text = String::new();
        for seg in engine.transcribe(&samples, 0).unwrap() {
            println!("[{} - {} ms] {}", seg.start_ms, seg.end_ms, seg.text);
            text.push_str(&seg.text);
            text.push(' ');
        }
        let lower = text.to_lowercase();
        assert!(
            lower.contains("fox") && lower.contains("transcription"),
            "unexpected transcript: {text}"
        );
    }
}
