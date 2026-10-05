//! Event payload types. They travel inside `service::CoreEvent` to the UI;
//! the `Serialize` derives stay for diagnostics and tests.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct RecordingStarted {
    pub meeting_id: i64,
    pub trigger: &'static str,
    pub started_at: String,
    /// True iff the live-caption worker actually started for this recording.
    pub live_captions: bool,
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
pub struct LiveCaptionsStatus {
    pub meeting_id: i64,
    pub active: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelDownloadProgress {
    pub model_id: String,
    pub file: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    /// 1-based position of the current file in the download job (0 when done).
    pub file_index: u32,
    /// Total files in the download job (0 when done).
    pub file_count: u32,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}
