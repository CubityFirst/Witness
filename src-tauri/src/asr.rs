//! ASR engine abstraction: both engines take 16 kHz mono f32 and return
//! sentence-ish segments with absolute (track-relative) timestamps.

use crate::settings::Engine;
use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct AsrSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

pub trait AsrEngine: Send {
    /// Transcribe one chunk; `offset_ms` is the chunk's position within the
    /// track and is added to all returned timestamps.
    fn transcribe(&mut self, samples_16k: &[f32], offset_ms: u64) -> Result<Vec<AsrSegment>>;
}

/// Loads the requested engine (models must already be downloaded). Engines
/// are created per job and dropped afterwards so VRAM is freed between runs.
pub fn create_engine(engine: Engine, models_dir: &Path) -> Result<Box<dyn AsrEngine>> {
    match engine {
        Engine::Parakeet => Ok(Box::new(crate::asr_parakeet::ParakeetEngine::new(models_dir)?)),
        Engine::Whisper => Ok(Box::new(crate::asr_whisper::WhisperEngine::new(models_dir)?)),
    }
}
