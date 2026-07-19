//! Event names + payload types emitted to the frontend. Keep in sync with
//! `src/lib/events.ts`.

use serde::Serialize;

pub const RECORDING_STARTED: &str = "recording-started";
pub const RECORDING_STOPPED: &str = "recording-stopped";
pub const RECORDING_LEVEL: &str = "recording-level";
pub const WATCHER_STATUS: &str = "watcher-status";
pub const TRANSCRIPTION_PROGRESS: &str = "transcription-progress";
pub const TRANSCRIPTION_COMPLETE: &str = "transcription-complete";
pub const TRANSCRIPTION_FAILED: &str = "transcription-failed";
pub const MODEL_DOWNLOAD_PROGRESS: &str = "model-download-progress";
pub const MEETINGS_CHANGED: &str = "meetings-changed";
pub const LIVE_TRANSCRIPT: &str = "live-transcript";

#[derive(Debug, Clone, Serialize)]
pub struct RecordingStarted {
    pub meeting_id: i64,
    pub trigger: &'static str,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingStopped {
    pub meeting_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingLevel {
    pub mic_rms: f32,
    pub loopback_rms: f32,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WatcherStatus {
    pub enabled: bool,
    pub teams_key_found: bool,
    pub mic_in_use: bool,
    pub suppressed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptionProgress {
    pub meeting_id: i64,
    pub stage: &'static str, // vad | asr | diarize | encode
    pub pct: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptionDone {
    pub meeting_id: i64,
    pub error: Option<String>,
}

/// Provisional live-caption line; replaced by the full-quality transcript
/// once the meeting ends and the pipeline runs.
#[derive(Debug, Clone, Serialize)]
pub struct LiveTranscript {
    pub meeting_id: i64,
    pub track: &'static str, // mic | loopback
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelDownloadProgress {
    pub model_id: String,
    pub file: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub done: bool,
    pub error: Option<String>,
}
