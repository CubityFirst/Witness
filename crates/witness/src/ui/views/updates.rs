//! Settings → Updates (updates.tsx): a pure renderer of the app-owned
//! `UpdateFlow` (`cx.shell.update`), so a check or an install keeps going
//! when the user leaves Settings or hides the window. Buttons push
//! `UiCmd::{CheckUpdate, InstallUpdate}`; `logic()` runs them.
//!
//! F0.2 renders the phases; the settings unit (U6) owns the final layout.

use crate::model::UpdatePhase;
use crate::ui::theme::TEXT_MUTED;
use crate::ui::widgets::{self, BadgeKind};
use crate::ui::{UiCmd, UiCtx};
use eframe::egui;

#[derive(Debug, Default)]
pub struct UpdatesSection {}

impl UpdatesSection {
    pub fn new() -> Self {
        UpdatesSection {}
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        ui.label(egui::RichText::new("UPDATES").color(TEXT_MUTED));
        let flow = &cx.shell.update;
        let version = env!("CARGO_PKG_VERSION");
        ui.horizontal_wrapped(|ui| {
            let current = matches!(flow.phase, UpdatePhase::Current);
            ui.label(format!(
                "Witness {version}{}",
                if current { " — up to date" } else { "" }
            ));
            match &flow.phase {
                UpdatePhase::Available(info) => {
                    if widgets::btn(ui, format!("Install {} & restart", info.version)).clicked() {
                        cx.cmds.push(UiCmd::InstallUpdate);
                    }
                }
                UpdatePhase::Installing(_) => {
                    widgets::badge(ui, BadgeKind::Busy, &flow.progress_text());
                }
                UpdatePhase::Checking => {
                    ui.add_enabled_ui(false, |ui| widgets::btn_ghost(ui, "Checking…"));
                }
                UpdatePhase::Idle | UpdatePhase::Current => {
                    if widgets::btn_ghost(ui, "Check for updates").clicked() {
                        cx.cmds.push(UiCmd::CheckUpdate);
                    }
                }
            }
        });
        if let UpdatePhase::Available(info) | UpdatePhase::Installing(info) = &flow.phase {
            if let Some(notes) = info.notes.as_deref().filter(|n| !n.is_empty()) {
                ui.label(egui::RichText::new(notes).color(TEXT_MUTED));
            }
        }
        let mut check = cx.settings.current().check_for_updates;
        if ui
            .checkbox(
                &mut check,
                "Check for updates at startup (no meeting data is sent)",
            )
            .changed()
        {
            let mut next = cx.settings.current().clone();
            next.check_for_updates = check;
            cx.settings.save(next, cx.tasks);
        }
    }
}
