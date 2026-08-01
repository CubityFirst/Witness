//! Shared application state managed by Tauri.

use crate::db::Db;
use crate::events::WatcherStatus;
use crate::meeting_watcher::WatcherControl;
use crate::pipeline::Pipeline;
use crate::recorder::RecorderHandle;
use crate::settings::Settings;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Recording lifecycle guarded by one mutex.  The transitional variants are
/// deliberately explicit: starting and stopping both involve fallible I/O and
/// may take long enough for a tray click, hotkey, watcher event, or frontend
/// command to arrive concurrently.
pub enum RecorderState {
    Idle,
    Starting,
    Recording(RecorderHandle),
    Stopping {
        meeting_id: i64,
        started_at: chrono::DateTime<chrono::Local>,
    },
}

impl RecorderState {
    /// True while a new start/stop request must be rejected.
    pub fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// True once a meeting row exists and until its recorder is finalized.
    pub fn is_recording(&self) -> bool {
        matches!(self, Self::Recording(_) | Self::Stopping { .. })
    }

    pub fn meeting_id(&self) -> Option<i64> {
        match self {
            Self::Recording(handle) => Some(handle.meeting_id),
            Self::Stopping { meeting_id, .. } => Some(*meeting_id),
            Self::Idle | Self::Starting => None,
        }
    }

    pub fn started_at(&self) -> Option<&chrono::DateTime<chrono::Local>> {
        match self {
            Self::Recording(handle) => Some(&handle.started_at),
            Self::Stopping { started_at, .. } => Some(started_at),
            Self::Idle | Self::Starting => None,
        }
    }
}

pub struct AppState {
    pub db: Arc<Db>,
    /// Settings used by live services in this process. `data_dir` remains the
    /// startup directory until restart so DB/audio/model paths cannot split.
    pub settings: Arc<Mutex<Settings>>,
    /// Persisted data-directory choice shown by Settings, which may differ
    /// from the active directory while a restart is pending.
    pub configured_data_dir: Mutex<Option<String>>,
    pub recorder: Mutex<RecorderState>,
    pub recording_trigger: Mutex<&'static str>,
    pub pipeline: Pipeline,
    pub watcher: Arc<WatcherControl>,
    pub last_watcher_status: Mutex<WatcherStatus>,
    pub downloading: AtomicBool,
    /// Which global record hotkey actually bound (others may own our
    /// preferred combos system-wide).
    pub hotkey: Mutex<Option<String>>,
    /// Which bookmark-this-moment hotkey bound.
    pub bookmark_hotkey: Mutex<Option<String>>,
}
