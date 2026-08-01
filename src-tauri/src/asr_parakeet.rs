//! Parakeet TDT 0.6B v3 via parakeet-rs (ONNX Runtime). CUDA EP requested;
//! ort silently falls back to CPU if CUDA init fails, so we log a visible
//! warning up front when the CUDA runtime looks absent.

use crate::asr::{AsrEngine, AsrSegment};
use anyhow::{Context, Result};
use parakeet_rs::{ExecutionConfig, ExecutionProvider, ParakeetTDT, TimestampMode, Transcriber};
use std::path::Path;

pub struct ParakeetEngine {
    inner: ParakeetTDT,
}

impl ParakeetEngine {
    pub fn new(models_dir: &Path) -> Result<ParakeetEngine> {
        let model_dir = crate::models::parakeet_dir(models_dir)
            .context("Parakeet model not downloaded — fetch it in Settings → Models")?;
        let config = ExecutionConfig::new().with_execution_provider(ExecutionProvider::Cuda);
        let inner = ParakeetTDT::from_pretrained(&model_dir, Some(config))
            .with_context(|| format!("loading Parakeet from {}", model_dir.display()))?;
        Ok(ParakeetEngine { inner })
    }
}

impl AsrEngine for ParakeetEngine {
    fn transcribe(&mut self, samples_16k: &[f32], offset_ms: u64) -> Result<Vec<AsrSegment>> {
        let result = self
            .inner
            .transcribe_samples(
                samples_16k.to_vec(),
                16_000,
                1,
                Some(TimestampMode::Sentences),
            )
            .context("Parakeet inference")?;

        let mut segments: Vec<AsrSegment> = result
            .tokens
            .iter()
            .filter(|t| !t.text.trim().is_empty())
            .map(|t| AsrSegment {
                start_ms: offset_ms + (t.start.max(0.0) * 1000.0) as u64,
                end_ms: offset_ms + (t.end.max(0.0) * 1000.0) as u64,
                text: t.text.trim().to_string(),
            })
            .collect();

        // Timestamped tokens can come back empty even when text exists —
        // fall back to one segment spanning the chunk.
        if segments.is_empty() && !result.text.trim().is_empty() {
            segments.push(AsrSegment {
                start_ms: offset_ms,
                end_ms: offset_ms + (samples_16k.len() as u64 / 16),
                text: result.text.trim().to_string(),
            });
        }
        Ok(segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase-4 de-risk smoke test: downloads the Parakeet + Sortformer models
    /// (~3 GB on first run, cached afterwards) into the dev data dir, loads
    /// the model with the CUDA EP and runs one inference.
    /// `cargo test cuda_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn cuda_smoke() {
        let models_dir = std::env::var("WITNESS_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/debug/models"));
        std::fs::create_dir_all(&models_dir).unwrap();

        crate::models::download(
            crate::settings::Engine::Parakeet,
            &models_dir,
            |m, f, d, t| {
                if let Some(t) = t {
                    println!("{m}/{f}: {} / {} MB", d / 1_000_000, t / 1_000_000);
                }
            },
        )
        .unwrap();

        let mut engine = ParakeetEngine::new(&models_dir).unwrap();
        // 2 s of silence — content doesn't matter, this exercises EP init +
        // the full encoder/decoder graph.
        let silence = vec![0.0f32; 32_000];
        let segments = engine.transcribe(&silence, 0).unwrap();
        println!("segments from silence: {segments:?}");

        // Sortformer too: feed 2 s, expect no crash.
        let diar = crate::diarize::diarize(&models_dir, &silence, |_| {}).unwrap();
        println!("diarization of silence: {diar:?}");
    }

    /// End-to-end ASR check on real speech. Generate the input first:
    /// PowerShell SAPI TTS → %TEMP%\witness-tts.wav (16 kHz mono i16),
    /// speaking "The quick brown fox jumps over the lazy dog. ...".
    /// `cargo test tts_transcribe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn tts_transcribe() {
        let models_dir = std::env::var("WITNESS_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/debug/models"));
        let wav = std::env::temp_dir().join("witness-tts.wav");
        let mut reader = hound::WavReader::open(&wav).expect("run the TTS generation step first");
        assert_eq!(reader.spec().sample_rate, 16_000);
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();

        // Through the real pipeline stages: VAD chunking, then ASR.
        let chunks = crate::vad::chunk_speech(&samples);
        assert!(!chunks.is_empty(), "VAD found no speech in TTS audio");
        let mut engine = ParakeetEngine::new(&models_dir).unwrap();
        let mut text = String::new();
        for chunk in &chunks {
            for seg in engine
                .transcribe(chunk.samples(&samples), chunk.start_ms)
                .unwrap()
            {
                println!("[{} - {} ms] {}", seg.start_ms, seg.end_ms, seg.text);
                text.push_str(&seg.text);
                text.push(' ');
            }
        }
        let lower = text.to_lowercase();
        assert!(
            lower.contains("fox") && lower.contains("transcription"),
            "unexpected transcript: {text}"
        );
    }
}
