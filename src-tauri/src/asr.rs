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
        Engine::Parakeet => Ok(Box::new(crate::asr_parakeet::ParakeetEngine::new(
            models_dir,
        )?)),
        Engine::Whisper => Ok(Box::new(crate::asr_whisper::WhisperEngine::new(
            models_dir,
        )?)),
    }
}

/// Phrases the engines invent for noise-only audio (pops/hum the VAD let
/// through). Whisper's YouTube-training artifacts are well documented;
/// lone "You" is its most common noise output. Compared after normalization,
/// so punctuation/case variants all match. Deliberately short — a real
/// "thank you all for joining" must never die here.
const HALLUCINATIONS: &[&str] = &[
    "you",
    "thank you",
    "thanks for watching",
    "thank you for watching",
    "please subscribe",
    "subtitles by the amara org community",
];

/// True when ASR output for a chunk is noise fallout rather than speech:
/// empty/punctuation-only text, or a known hallucination phrase.
pub fn is_junk_text(text: &str) -> bool {
    let normalized = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    normalized.is_empty() || HALLUCINATIONS.contains(&normalized.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_filter() {
        assert!(is_junk_text(""));
        assert!(is_junk_text(" . "));
        assert!(is_junk_text("You"));
        assert!(is_junk_text("Thank you."));
        assert!(is_junk_text("Thanks for watching!"));
        assert!(is_junk_text("Subtitles by the Amara.org community"));

        assert!(!is_junk_text("Thank you all for joining today."));
        assert!(!is_junk_text("Can you see my screen?"));
        assert!(!is_junk_text("OK."));
    }
}
