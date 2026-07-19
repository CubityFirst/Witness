//! Shared application state managed by Tauri.

use crate::db::Db;
use crate::events::WatcherStatus;
use crate::meeting_watcher::WatcherControl;
use crate::pipeline::Pipeline;
use crate::recorder::RecorderHandle;
use crate::settings::Settings;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub struct AppState {
    pub db: Arc<Db>,
    pub settings: Arc<Mutex<Settings>>,
    pub recorder: Mutex<Option<RecorderHandle>>,
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
