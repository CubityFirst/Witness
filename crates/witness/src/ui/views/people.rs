//! People: per-person stats, weekly trend and voice prints
//! (map-ui-meetings §3).
//!
//! STUB (F0.2): the library-views unit (U4) implements it.

use crate::ui::UiCtx;
use eframe::egui;

pub struct PeopleView {}

impl PeopleView {
    pub fn new() -> Self {
        PeopleView {}
    }

    pub fn show(&mut self, ui: &mut egui::Ui, _cx: &mut UiCtx) {
        super::not_ported(ui, "People");
    }

    /// Nothing to persist.
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
    }
}
