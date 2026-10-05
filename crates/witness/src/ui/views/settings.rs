//! Settings: Startup, Updates, Storage, Recording, Transcription, Models,
//! Diagnostics and People sections (map-ui-settings §1).
//!
//! STUB (F0.2): the settings unit (U6) implements it. The Updates section
//! (`updates.rs`) is already usable.

use super::updates::UpdatesSection;
use crate::ui::{widgets, UiCtx};
use eframe::egui;

pub struct SettingsView {
    updates: UpdatesSection,
}

impl SettingsView {
    pub fn new() -> Self {
        SettingsView {
            updates: UpdatesSection::new(),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        super::not_ported(ui, "Settings");
        ui.add_space(16.0);
        widgets::centered_column(ui, 720.0, |ui| {
            widgets::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.updates.show(ui, cx);
            });
        });
    }

    /// Nothing to persist (settings writes go through `SettingsStore`).
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
    }
}
