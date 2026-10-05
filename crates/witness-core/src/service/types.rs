//! DTOs returned by `AppCore` (formerly the Tauri command return types).
//! Shapes are unchanged; `Serialize` stays for diagnostics and tests.

use crate::db::{Bookmark, Meeting, Segment, Speaker};
use crate::events;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AppStatus {
    pub recording: bool,
    pub meeting_id: Option<i64>,
    pub recording_since: Option<String>,
    /// True iff the active recording has a live-caption session.
    pub live_captions: bool,
    pub transcribing_meeting_id: Option<i64>,
    pub queue_len: usize,
    pub processing_stage: Option<String>,
    pub processing_pct: Option<f32>,
    pub watcher: events::WatcherStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingDetail {
    pub meeting: Meeting,
    pub speakers: Vec<Speaker>,
    pub segments: Vec<Segment>,
    pub bookmarks: Vec<Bookmark>,
    /// Why the most recent pipeline job for this meeting failed (None when
    /// there is none or it succeeded).
    pub last_error: Option<String>,
    /// This meeting has a runnable job waiting in the durable pipeline queue.
    pub pipeline_queued: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeletedMeeting {
    #[serde(flatten)]
    pub meeting: Meeting,
    pub deleted_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersonInfo {
    pub id: i64,
    pub name: String,
    pub sample_seconds: f64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticDatabase {
    pub path: String,
    pub healthy: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticModel {
    pub id: String,
    pub present: bool,
    pub expected_revision: String,
    pub installed_revision: Option<String>,
    pub integrity_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticGpu {
    pub driver_present: bool,
    pub cuda_ready: bool,
    pub missing_dlls: Vec<String>,
    pub managed_libraries_present: bool,
    pub managed_libraries_integrity_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostics {
    pub generated_at: String,
    pub app_version: String,
    pub platform: String,
    pub runtime_data_dir: String,
    pub configured_data_dir: Option<String>,
    pub restart_required: bool,
    pub database: DiagnosticDatabase,
    pub log_path: String,
    pub recording_state: String,
    pub recording_meeting_id: Option<i64>,
    pub recording_since: Option<String>,
    pub capture_health: Option<crate::recorder::RecordingHealth>,
    pub capture_health_is_current: bool,
    pub processing_meeting_id: Option<i64>,
    pub processing_stage: Option<String>,
    pub processing_pct: Option<f32>,
    pub queue_len: usize,
    pub models: Vec<DiagnosticModel>,
    pub gpu: DiagnosticGpu,
    /// A transcript/audio-free text representation suitable for support.
    pub report: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuLibsInfo {
    /// Verified app-managed install in `data_dir/cuda`.
    pub present: bool,
    pub size_mb: Option<f64>,
    pub integrity_error: Option<String>,
    /// NVIDIA driver present — without it the download is pointless.
    pub driver_present: bool,
    /// The CUDA EP already resolves everything (system install or managed).
    pub cuda_ready: bool,
    /// Total download size of the pinned archives, for the button label.
    pub download_mb: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuStatus {
    pub cuda_available: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioDevices {
    pub render: Vec<crate::audio_capture::AudioDevice>,
    pub capture: Vec<crate::audio_capture::AudioDevice>,
}

/// One page (40 hits) of full-text search with the Search view's filters.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub query: String,
    pub offset: i64,
    pub person_id: Option<i64>,
    /// "mic" | "loopback"; anything else means both tracks.
    pub track: Option<&'static str>,
    /// Inclusive `YYYY-MM-DD` bounds; empty strings are ignored.
    pub date_from: Option<String>,
    pub date_to: Option<String>,
}

/// A newer release found by the update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub version: String,
    pub notes: Option<String>,
    /// Date part (`YYYY-MM-DD`) of the feed's `pub_date`.
    pub date: Option<String>,
}
