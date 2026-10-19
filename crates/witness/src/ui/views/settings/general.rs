//! Settings → General: Appearance (theme, accent colour, tray icon — UI
//! preferences, applied at once) and Startup (start with Windows, the setup
//! assistant).
//! The appearance rows are shared with the first-run setup.

use super::{
    fill, note, notice_done, quiet, row, section, segmented, title, toggle, OpRun, Segment,
    SettingsState, Slot, FIELD_WIDTH,
};
use crate::bridge::Task;
use crate::platform::{autostart, system};
use crate::ui::icons::ph;
use crate::ui::theme::{self, pal};
use crate::ui::toasts::Notice;
use crate::ui::widgets;
use crate::ui::{UiCmd, UiCtx};
use crate::ui_state::{AccentSource, Appearance, TrayIconStyle, UiPrefs};
use eframe::egui;

impl SettingsState {
    pub(super) fn general_page(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        self.appearance(ui, cx);
        self.startup(ui, cx);
    }

    /// "System theme · Windows accent · starts with Windows".
    pub(super) fn general_summary(&self, cx: &UiCtx) -> String {
        general_summary(cx.prefs, self.autostart)
    }

    /// Theme, accent and tray icon. UI preferences (`cx.prefs`), not
    /// settings: the shell persists them and re-applies them every pass, so
    /// they apply at once.
    fn appearance(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let look = self.look;
        let prefs = &mut *cx.prefs;
        section(ui, "Appearance", |ui| {
            theme_row(ui, prefs);
            widgets::group_divider(ui);
            accent_row(ui, prefs, look.as_ref());
            widgets::group_divider(ui);
            tray_icon_row(ui, prefs);
        });
    }

    fn startup(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let mut enabled = self.autostart.unwrap_or(false);
        let loaded = self.autostart.is_some() && self.autostart_write.is_none();
        let (changed, setup) = section(ui, "Startup", |ui| {
            let changed = toggle(
                ui,
                loaded,
                &mut enabled,
                "Start Witness with Windows",
                "Opens minimized to the tray.",
            )
            .changed();
            widgets::group_divider(ui);
            let setup = row(
                ui,
                "run-setup",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Setup assistant");
                    note(ui, "Walk through devices, models and preferences again.");
                    id
                },
                |ui, _| widgets::btn(ui, "Run setup").clicked(),
            );
            (changed, setup)
        });
        if setup {
            cx.cmds.push(UiCmd::RunSetup);
        }
        if changed {
            // The switch shows the new value once the registry write
            // succeeds; a failure toasts even if Settings was left.
            self.autostart_write =
                Some(OpRun::spawn(
                    cx,
                    "autostart-write",
                    move |_, slot| match autostart::set_enabled(enabled) {
                        Ok(()) => {
                            fill(slot, enabled);
                            quiet()
                        }
                        Err(error) => {
                            notice_done(Notice::error("Could not update autostart", error))
                        }
                    },
                ));
        }
    }
}

/// What Windows offers the Appearance rows: its accent shades (for the
/// "Windows" swatch, shown even while Witness blue is picked). Registry
/// reads, so loaded off the UI thread.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct SystemLook {
    /// `AccentPalette`: light3 … base … dark3.
    pub accent: Option<[egui::Color32; 7]>,
}

impl SystemLook {
    pub(crate) fn load(cx: &UiCtx) -> Task<SystemLook> {
        cx.tasks.spawn("system-look", |_| SystemLook {
            accent: system::accent_palette()
                .map(|shades| shades.map(|[r, g, b]| egui::Color32::from_rgb(r, g, b))),
        })
    }

    /// The Windows accent shade the current theme uses (as
    /// `Palette::resolve` picks it).
    fn windows_accent(&self) -> Option<egui::Color32> {
        let shades = self.accent?;
        Some(if pal().dark { shades[1] } else { shades[4] })
    }
}

/// Theme: System / Light / Dark. Applies on the next frame.
pub(crate) fn theme_row(ui: &mut egui::Ui, prefs: &mut UiPrefs) {
    row(
        ui,
        "appearance-theme",
        Slot::Fill(FIELD_WIDTH),
        |ui| {
            let id = title(ui, "Theme");
            note(ui, "System follows Windows' light or dark app mode.");
            id
        },
        |ui, label| {
            segmented(
                ui,
                "appearance-theme",
                label,
                &mut prefs.appearance,
                &[
                    Segment::new(Appearance::System, "System").icon(ph::MONITOR),
                    Segment::new(Appearance::Light, "Light").icon(ph::SUN),
                    Segment::new(Appearance::Dark, "Dark").icon(ph::MOON),
                ],
            );
        },
    );
}

/// Accent: Windows' accent colour or Witness blue, each with its swatch.
pub(crate) fn accent_row(ui: &mut egui::Ui, prefs: &mut UiPrefs, look: Option<&SystemLook>) {
    let windows = look.and_then(SystemLook::windows_accent);
    // Unknown until read; a Windows without an accent palette falls back
    // to Witness blue anyway (`Palette::resolve`).
    let description = match look {
        Some(look) if look.accent.is_none() => {
            "Windows has no accent colour to offer, so Witness blue is used."
        }
        _ => "Buttons, selection, focus and progress.",
    };
    row(
        ui,
        "appearance-accent",
        Slot::Fill(FIELD_WIDTH),
        |ui| {
            let id = title(ui, "Accent colour");
            note(ui, description);
            id
        },
        |ui, label| {
            let mut windows_option = Segment::new(AccentSource::Windows, "Windows");
            if let Some(color) = windows {
                windows_option = windows_option.swatch(color);
            }
            segmented(
                ui,
                "appearance-accent",
                label,
                &mut prefs.accent,
                &[
                    windows_option,
                    Segment::new(AccentSource::Witness, "Witness blue")
                        .swatch(theme::Palette::resolve(pal().dark, None).accent),
                ],
            );
        },
    );
}

/// Tray icon: monochrome (follows the taskbar's light or dark theme) or the
/// full-colour app icon.
fn tray_icon_row(ui: &mut egui::Ui, prefs: &mut UiPrefs) {
    row(
        ui,
        "appearance-tray-icon",
        Slot::Fill(FIELD_WIDTH),
        |ui| {
            let id = title(ui, "Tray icon");
            note(ui, "Monochrome matches the taskbar's light or dark theme.");
            id
        },
        |ui, label| {
            segmented(
                ui,
                "appearance-tray-icon",
                label,
                &mut prefs.tray_icon,
                &[
                    Segment::new(TrayIconStyle::Monochrome, "Monochrome"),
                    Segment::new(TrayIconStyle::Colour, "Colour"),
                ],
            );
        },
    );
}

/// The General category's one-line summary.
fn general_summary(prefs: &UiPrefs, autostart: Option<bool>) -> String {
    let theme = match prefs.appearance {
        Appearance::System => "System theme",
        Appearance::Light => "Light theme",
        Appearance::Dark => "Dark theme",
    };
    let accent = match prefs.accent {
        AccentSource::Windows => "Windows accent",
        AccentSource::Witness => "Witness blue",
    };
    match autostart {
        Some(true) => format!("{theme} · {accent} · starts with Windows"),
        _ => format!("{theme} · {accent}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_names_theme_accent_and_autostart() {
        let mut prefs = UiPrefs {
            appearance: Appearance::Dark,
            accent: AccentSource::Witness,
            ..UiPrefs::default()
        };
        assert_eq!(
            general_summary(&prefs, Some(true)),
            "Dark theme · Witness blue · starts with Windows"
        );
        prefs.appearance = Appearance::System;
        prefs.accent = AccentSource::Windows;
        assert_eq!(
            general_summary(&prefs, Some(false)),
            "System theme · Windows accent"
        );
    }
}
