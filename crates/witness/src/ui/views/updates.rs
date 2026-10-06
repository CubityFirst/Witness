//! Settings → Updates (updates.tsx, map-ui-settings §2): a pure renderer of
//! the app-owned `UpdateFlow` (`cx.shell.update`), so a check or an install
//! keeps going when the user leaves Settings or hides the window. Buttons
//! push `UiCmd::{CheckUpdate, InstallUpdate}`; `logic()` runs them.
//!
//! Unlike updates.tsx, the phase does not reset to Idle on every mount: the
//! flow is app-owned and preloaded from `core.pending_update()`, so the
//! Install button shows as soon as the startup check has found a release.

use super::settings::{badge, ghost_enabled, muted_paragraph, primary, row, section};
use crate::model::{UpdateFlow, UpdatePhase};
use crate::ui::theme::TEXT;
use crate::ui::widgets::{self, BadgeKind};
use crate::ui::{UiCmd, UiCtx};
use eframe::egui::{self, RichText};

/// What the version row's trailing widget is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Primary "Install {version} & restart".
    Install(String),
    /// The busy badge while downloading / verifying.
    Progress(String),
    /// Ghost "Check for updates" ("Checking…", disabled, while checking).
    Check { checking: bool },
}

#[derive(Debug, Default)]
pub struct UpdatesSection {}

impl UpdatesSection {
    pub fn new() -> Self {
        UpdatesSection {}
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        section(ui, "Updates", |ui| {
            let flow = &cx.shell.update;
            let label = version_label(env!("CARGO_PKG_VERSION"), &flow.phase);
            let action = action(flow);
            let notes = release_notes(&flow.phase).map(str::to_owned);
            row(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(label).color(TEXT)).wrap());
                match action {
                    Action::Install(version) => {
                        if primary(ui, &format!("Install {version} & restart")).clicked() {
                            cx.cmds.push(UiCmd::InstallUpdate);
                        }
                    }
                    Action::Progress(text) => {
                        badge(ui, BadgeKind::Busy, &text);
                    }
                    Action::Check { checking } => {
                        let text = if checking {
                            "Checking…"
                        } else {
                            "Check for updates"
                        };
                        if ghost_enabled(ui, !checking, text).clicked() {
                            cx.cmds.push(UiCmd::CheckUpdate);
                        }
                    }
                }
            });
            // Plain text, like the `<p>` (no markdown rendering).
            if let Some(notes) = notes {
                muted_paragraph(ui, &notes);
            }
            let mut check = cx.settings.current().check_for_updates;
            if widgets::checkbox(
                ui,
                true,
                &mut check,
                "Check for updates at startup (no meeting data is sent)",
            )
            .changed()
            {
                let mut next = cx.settings.current().clone();
                next.check_for_updates = check;
                cx.settings.save(next, cx.tasks);
            }
        });
    }
}

/// "Witness 0.3.0", plus " — up to date" after a check found nothing.
fn version_label(version: &str, phase: &UpdatePhase) -> String {
    let current = if matches!(phase, UpdatePhase::Current) {
        " — up to date"
    } else {
        ""
    };
    format!("Witness {version}{current}")
}

fn action(flow: &UpdateFlow) -> Action {
    match &flow.phase {
        UpdatePhase::Available(info) => Action::Install(info.version.clone()),
        UpdatePhase::Installing(_) => Action::Progress(flow.progress_text()),
        UpdatePhase::Checking => Action::Check { checking: true },
        UpdatePhase::Idle | UpdatePhase::Current => Action::Check { checking: false },
    }
}

/// The release notes while an update is available or installing.
fn release_notes(phase: &UpdatePhase) -> Option<&str> {
    match phase {
        UpdatePhase::Available(info) | UpdatePhase::Installing(info) => {
            info.notes.as_deref().filter(|n| !n.trim().is_empty())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use witness_core::events::UpdateProgress;
    use witness_core::service::UpdateInfo;

    fn info(notes: Option<&str>) -> UpdateInfo {
        UpdateInfo {
            current_version: "0.2.1".into(),
            version: "0.3.0".into(),
            notes: notes.map(String::from),
            date: Some("2026-10-05".into()),
        }
    }

    #[test]
    fn version_row_follows_the_phase() {
        assert_eq!(version_label("0.3.0", &UpdatePhase::Idle), "Witness 0.3.0");
        assert_eq!(
            version_label("0.3.0", &UpdatePhase::Current),
            "Witness 0.3.0 — up to date"
        );

        let mut flow = UpdateFlow::default();
        assert_eq!(action(&flow), Action::Check { checking: false });
        flow.phase = UpdatePhase::Checking;
        assert_eq!(action(&flow), Action::Check { checking: true });
        flow.phase = UpdatePhase::Available(info(None));
        assert_eq!(action(&flow), Action::Install("0.3.0".into()));
        flow.phase = UpdatePhase::Installing(info(None));
        assert_eq!(action(&flow), Action::Progress("starting…".into()));
        flow.progress = Some(UpdateProgress {
            downloaded: 3_000_000,
            total: Some(12_000_000),
        });
        assert_eq!(action(&flow), Action::Progress("downloading 25%".into()));
    }

    #[test]
    fn notes_show_only_with_a_release() {
        assert_eq!(release_notes(&UpdatePhase::Current), None);
        assert_eq!(
            release_notes(&UpdatePhase::Available(info(Some("Fixes\n- a")))),
            Some("Fixes\n- a")
        );
        assert_eq!(
            release_notes(&UpdatePhase::Installing(info(Some("  ")))),
            None
        );
        assert_eq!(release_notes(&UpdatePhase::Available(info(None))), None);
    }
}
