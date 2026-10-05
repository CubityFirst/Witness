//! Full-text search results and the shell's search box
//! (map-ui-meetings §2, map-ui-shell §2).
//!
//! STUB (F0.2): the library-views unit (U4) implements the debounce,
//! Escape/× clearing, filters and results.

use crate::ui::{UiCtx, View};
use eframe::egui;

pub struct SearchView {
    query: String,
}

impl SearchView {
    pub fn new(query: String) -> Self {
        SearchView { query }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, _cx: &mut UiCtx) {
        super::not_ported(ui, &format!("Search \u{201c}{}\u{201d}", self.query.trim()));
    }

    /// Nothing to persist.
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
    }
}

/// The search box shown above Meetings and Search. Edits the app-owned
/// `cx.memory.search_box`. STUB: plain editing only (no debounce yet).
pub fn search_box(ui: &mut egui::Ui, cx: &mut UiCtx, _current: &View) {
    let search = &mut cx.memory.search_box;
    let response = ui.add(
        egui::TextEdit::singleline(&mut search.text)
            .hint_text("Search transcripts…")
            .desired_width(ui.available_width().min(420.0)),
    );
    if std::mem::take(&mut search.request_focus) {
        response.request_focus();
    }
    crate::ui::widgets::text_edit_menu(&response);
}
