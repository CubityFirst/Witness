//! App-level preferences, persisted next to the executable in
//! `witness-settings.toml` (vigil's layering: `WITNESS_SETTINGS` env var →
//! settings file → `data_dir` holding db/audio/models/log).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_OPUS_BITRATE_KBPS: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Parakeet,
    Whisper,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::Parakeet
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Engine::Parakeet => write!(f, "parakeet"),
            Engine::Whisper => write!(f, "whisper"),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_patterns() -> Vec<String> {
    vec!["MSTeams".to_string()]
}

fn default_bitrate() -> u32 {
    DEFAULT_OPUS_BITRATE_KBPS
}

fn default_threshold() -> f32 {
    0.6
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Directory holding the db, audio, models and log. None = next to the exe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,

    /// ASR engine used for new transcriptions.
    #[serde(default)]
    pub engine: Engine,

    /// Start recording automatically when a watched app holds the mic.
    #[serde(default = "default_true")]
    pub auto_record: bool,

    /// Queue a transcription job as soon as a recording finishes.
    #[serde(default = "default_true")]
    pub auto_transcribe: bool,

    /// Show provisional captions while recording (Parakeet on speech
    /// pauses; the final pass still replaces them).
    #[serde(default = "default_true")]
    pub live_transcribe: bool,

    /// Float a small always-on-top captions window while recording.
    #[serde(default = "default_true")]
    pub caption_overlay: bool,

    /// Cosine similarity needed to auto-label a speaker from a voice print.
    #[serde(default = "default_threshold")]
    pub speaker_match_threshold: f32,

    /// Substrings matched (case-insensitively) against mic-consent-store app
    /// names; any match counts as "in a meeting".
    #[serde(default = "default_patterns")]
    pub watch_patterns: Vec<String>,

    /// Target bitrate for the archived stereo Ogg Opus file.
    #[serde(default = "default_bitrate")]
    pub opus_bitrate_kbps: u32,

    /// Capture device (friendly name) for the mic track; None = default.
    /// Useful with per-app routing, e.g. a headset's "Chat Mic".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_device: Option<String>,

    /// Render device (friendly name) whose loopback is the "them" track;
    /// None = default output. Pointing this at a dedicated device that only
    /// Teams outputs to (e.g. "Chat") isolates meeting audio from music etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loopback_device: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            data_dir: None,
            engine: Engine::default(),
            auto_record: true,
            auto_transcribe: true,
            live_transcribe: true,
            caption_overlay: true,
            speaker_match_threshold: 0.6,
            watch_patterns: default_patterns(),
            opus_bitrate_kbps: DEFAULT_OPUS_BITRATE_KBPS,
            mic_device: None,
            loopback_device: None,
        }
    }
}

impl Settings {
    /// Location of the settings file. Overridable with the `WITNESS_SETTINGS`
    /// environment variable (a file path, or a directory to hold the default
    /// filename); otherwise it sits next to the executable.
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("WITNESS_SETTINGS") {
            let p = p.trim();
            if !p.is_empty() {
                let pb = PathBuf::from(p);
                return if pb.is_dir() { pb.join("witness-settings.toml") } else { pb };
            }
        }
        exe_dir().join("witness-settings.toml")
    }

    pub fn load() -> Settings {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())
    }

    /// The effective data directory (configured one, or the exe's folder).
    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().map(PathBuf::from).unwrap_or_else(exe_dir)
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.data_dir().join("audio")
    }

    pub fn rec_tmp_dir(&self) -> PathBuf {
        self.data_dir().join("rec-tmp")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.data_dir().join("models")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("witness.db")
    }
}

/// Directory containing the running executable (falls back to the cwd).
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}
