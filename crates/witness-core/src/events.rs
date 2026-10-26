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
    /// The last call was seen only through audio sessions: Windows' consent
    /// store stopped recording mic use. Cleared once it reports use again.
    pub consent_store_stale: bool,
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
    /// Bytes of the whole job so far: every earlier file counts as done
    /// (fetched or already cached), plus the current file's progress.
    pub job_downloaded_bytes: u64,
    /// Pinned size of every file in the job (None when done).
    pub job_total_bytes: Option<u64>,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// What the AI assistant writes for a meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum AiTask {
    Title,
    /// The notes (`meetings.ai_summary`).
    Summary,
}

/// An AI job for one meeting was queued or moved on. `stage` is a short
/// human-readable step ("Queued", "Reading part 2 of 5", "Writing notes").
#[derive(Debug, Clone, Serialize)]
pub struct AiProgress {
    pub meeting_id: i64,
    pub task: AiTask,
    pub stage: String,
}

/// How an AI task ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AiOutcome {
    /// Written to the meeting.
    Applied,
    /// Not run or not written, and that is fine (feature off, the user
    /// renamed the meeting meanwhile, nothing to summarise…); the reason is
    /// for manual requests' feedback.
    Skipped(String),
    /// A failure worth telling the user (no secrets in it).
    Failed(String),
}

/// An AI task finished. `automatic` = started by a finished transcription,
/// not by the user.
#[derive(Debug, Clone, Serialize)]
pub struct AiDone {
    pub meeting_id: i64,
    pub task: AiTask,
    pub automatic: bool,
    pub outcome: AiOutcome,
}
