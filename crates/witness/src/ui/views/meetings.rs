//! Meetings list + recycle bin (map-ui-meetings §1).
//!
//! STUB (F0.2): the library-views unit (U4) implements it.

use crate::ui::UiCtx;
use eframe::egui;

pub struct MeetingsView {}

impl MeetingsView {
    pub fn new() -> Self {
        MeetingsView {}
    }

    pub fn show(&mut self, ui: &mut egui::Ui, _cx: &mut UiCtx) {
        super::not_ported(ui, "Meetings");
    }

    /// Nothing to persist.
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
    }
}
