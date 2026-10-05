//! The five views plus the transcript's player bar and the Settings →
//! Updates section. Each view is a struct remounted when the view key
//! changes (`ui::View::key`); its loads are view-owned `Task`s and its
//! actions go through `Tasks::spawn_op`.

pub mod meetings;
pub mod people;
pub mod player_bar;
pub mod search;
pub mod settings;
pub mod transcript;
pub mod updates;

use super::theme::TEXT_MUTED;
use super::widgets;
use eframe::egui;

/// Placeholder body for a view that has not been ported yet.
pub(crate) fn not_ported(ui: &mut egui::Ui, name: &str) {
    widgets::centered_column(ui, 900.0, |ui| {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.heading(name);
            ui.label(egui::RichText::new(format!("{name} — not ported yet")).color(TEXT_MUTED));
        });
    });
}
