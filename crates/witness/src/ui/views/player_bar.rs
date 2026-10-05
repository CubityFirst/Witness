//! The transcript's player bar: time, scrubber with bookmark markers,
//! ±10 s, play/pause, add bookmark, speed, volume, "Save a copy…"
//! (map-ui-transcript §9).
//!
//! STUB (F0.2): the transcript unit (U5) implements it.

use crate::ui::UiCtx;
use eframe::egui;
use witness_core::service::MeetingDetail;

/// Per-transcript player bar state (scrub preview, throttling).
#[derive(Debug, Default)]
pub struct PlayerBarState {}

/// What the bar asks the transcript view to do.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlayerBarOutput {
    /// "Add bookmark" at this position.
    pub add_bookmark_at_ms: Option<i64>,
}

pub fn show(
    ui: &mut egui::Ui,
    _cx: &mut UiCtx,
    _detail: &MeetingDetail,
    _st: &mut PlayerBarState,
) -> PlayerBarOutput {
    ui.label("Player — not ported yet");
    PlayerBarOutput::default()
}
