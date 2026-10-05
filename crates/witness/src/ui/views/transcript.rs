//! Transcript: header, speaker stats, live feed, notes, segments and
//! bookmarks, with the player bar (map-ui-transcript).
//!
//! STUB (F0.2): the transcript unit (U5) implements it.

use crate::ui::UiCtx;
use eframe::egui;

pub struct TranscriptView {
    meeting_id: i64,
    #[allow(dead_code)] // TODO(U5): deep link to this segment.
    focus_segment_id: Option<i64>,
    #[allow(dead_code)] // TODO(U5): highlight these search terms.
    highlight: Option<String>,
}

impl TranscriptView {
    pub fn new(meeting_id: i64, focus_segment_id: Option<i64>, highlight: Option<String>) -> Self {
        TranscriptView {
            meeting_id,
            focus_segment_id,
            highlight,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if crate::ui::widgets::btn_ghost(ui, "Back").clicked() {
            let from = match cx.view {
                crate::ui::View::Transcript { from, .. } => from.clone(),
                _ => None,
            };
            cx.nav.back(from);
        }
        super::not_ported(ui, &format!("Transcript {}", self.meeting_id));
    }

    /// Enqueue the newest notes draft (`core.queue_notes`). Never blocks.
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

    /// `flush()` + release the audio device.
    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
        cx.player.unload();
    }
}
