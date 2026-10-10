//! Settings → Updates (updates.tsx, map-ui-settings §2): a pure renderer of
//! the app-owned `UpdateFlow` (`cx.shell.update`), so a check or an install
//! keeps going when the user leaves Settings or hides the window. Buttons
//! push `UiCmd::{CheckUpdate, InstallUpdate}`; `logic()` runs them.
//!
//! Unlike updates.tsx, the phase does not reset to Idle on every mount: the
//! flow is app-owned and preloaded from `core.pending_update()`, so the
//! Install button shows as soon as the startup check has found a release.

use super::{button, note, progress_bar, row, section, title, toggle, Slot};
use crate::model::{UpdateFlow, UpdatePhase};
use crate::ui::widgets::{self, BadgeKind};
use crate::ui::{UiCmd, UiCtx};
use eframe::egui;

/// What the version row's trailing widget is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Primary "Install {version} & restart".
    Install(String),
    /// The busy badge while downloading / verifying.
    Progress(String),
    /// "Check for updates" ("Checking…", disabled, while checking).
    Check { checking: bool },
}

#[derive(Debug, Default)]
pub struct UpdatesSection {}

impl UpdatesSection {
    pub fn new() -> Self {
        UpdatesSection {}
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let flow = &cx.shell.update;
        let label = version_label(env!("CARGO_PKG_VERSION"), &flow.phase);
        let action = action(flow);
        let notes = release_notes(&flow.phase).map(str::to_owned);
        let fraction = install_fraction(flow);
        let mut check = cx.settings.current().check_for_updates;

        let (clicked, check_changed) = section(ui, "Updates", |ui| {
            let clicked = row(
                ui,
                "update-version",
                Slot::Fit,
                |ui| {
                    let id = title(ui, &label);
                    // Plain text, like the `<p>` (no markdown rendering).
                    if let Some(notes) = &notes {
                        note(ui, notes);
                    }
                    if let Some(fraction) = fraction {
                        progress_bar(ui, fraction);
                    }
                    id
                },
                |ui, _| match &action {
                    Action::Install(version) => {
                        widgets::btn_primary(ui, format!("Install {version} & restart")).clicked()
                    }
                    Action::Progress(text) => {
                        widgets::badge(ui, BadgeKind::Busy, text);
                        false
                    }
                    Action::Check { checking } => {
                        let text = if *checking {
                            "Checking…"
                        } else {
                            "Check for updates"
                        };
                        button(ui, !checking, text).clicked()
                    }
                },
            );
            widgets::group_divider(ui);
            let check_changed = toggle(
                ui,
                true,
                &mut check,
                "Check for updates at startup",
                "No meeting data is sent.",
            )
            .changed();
            (clicked, check_changed)
        });

        if clicked {
            cx.cmds.push(match action {
                Action::Install(_) => UiCmd::InstallUpdate,
                _ => UiCmd::CheckUpdate,
            });
        }
        if check_changed {
            let mut next = cx.settings.current().clone();
            next.check_for_updates = check;
            cx.settings.save(next, cx.tasks);
        }
    }
}

/// The installer download's progress (0..1) once its size is known.
fn install_fraction(flow: &UpdateFlow) -> Option<f32> {
    if !matches!(flow.phase, UpdatePhase::Installing(_)) {
        return None;
    }
    let p = flow.progress.as_ref()?;
    let total = p.total.filter(|t| *t > 0)?;
    Some((p.downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32)
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
        assert_eq!(install_fraction(&flow), Some(0.25));
        flow.phase = UpdatePhase::Available(info(None));
        assert_eq!(install_fraction(&flow), None, "only while installing");
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
