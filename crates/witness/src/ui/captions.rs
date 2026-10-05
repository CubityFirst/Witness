//! The always-on-top live-caption overlay: a deferred viewport declared
//! every pass while open (design §4.3, map-ui-settings §3).
//!
//! STUB (F0.2): the API is final and every call is a no-op apart from
//! bookkeeping; the app-chrome unit (U7) implements the viewport, geometry
//! persistence (`captions-overlay.toml`), per-meeting reset, focus rules,
//! drag/resize and overlay zoom.

use eframe::egui;
use std::path::{Path, PathBuf};
use witness_core::events::LiveTranscript;

/// Why the overlay is being opened (focus rules differ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenReason {
    /// `CoreEvent::OpenCaptionsOverlay`: never takes focus.
    Auto,
    /// The CC button: focuses an already-open overlay.
    User,
}

pub struct CaptionsOverlay {
    #[allow(dead_code)] // TODO(U7): geometry file location.
    data_dir: PathBuf,
    open: bool,
    meeting_id: Option<i64>,
}

impl CaptionsOverlay {
    pub fn new(data_dir: &Path) -> Self {
        CaptionsOverlay {
            data_dir: data_dir.to_path_buf(),
            open: false,
            meeting_id: None,
        }
    }

    /// Open (or re-open) for `meeting_id`. TODO(U7)
    pub fn open(&mut self, meeting_id: i64, why: OpenReason) {
        log::debug!(
            "captions overlay open for meeting {meeting_id} ({why:?}): not implemented yet"
        );
        self.open = true;
        self.meeting_id = Some(meeting_id);
    }

    /// A new recording started: start empty ("Listening…"). TODO(U7)
    pub fn reset_for(&mut self, meeting_id: i64) {
        self.meeting_id = Some(meeting_id);
    }

    /// Close and save the geometry. TODO(U7)
    pub fn close(&mut self, _data_dir: &Path) {
        self.open = false;
    }

    /// Append a caption line (keeps the last 20). TODO(U7)
    pub fn push_line(&mut self, _line: &LiveTranscript) {}

    /// Show (Some) or clear (None) the overlay's error banner. TODO(U7)
    pub fn set_error(&mut self, _error: Option<String>) {}

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The overlay's OS window exists and is visible. TODO(U7)
    pub fn is_visible(&self) -> bool {
        false
    }

    /// Declare the deferred viewport (every pass while open). TODO(U7)
    pub fn show(&mut self, _ctx: &egui::Context) {}
}
