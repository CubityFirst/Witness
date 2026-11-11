//! Settings → General: Appearance (theme, accent colour, tray icon — UI
//! preferences, applied at once) and Startup (start with Windows, the setup
//! assistant).
//! The appearance rows are shared with the first-run setup.

use super::{
    fill, note, notice_done, quiet, row, section, segmented, title, toggle, OpRun, Segment,
    SettingsState, Slot, FIELD_WIDTH, SEGMENT_INSET,
};
use crate::bridge::Task;
use crate::platform::{autostart, system, tray};
use crate::ui::icons::ph;
use crate::ui::theme::{self, pal};
use crate::ui::toasts::Notice;
use crate::ui::widgets;
use crate::ui::{UiCmd, UiCtx};
use crate::ui_state::{AccentSource, Appearance, TrayIconStyle, UiPrefs};
use eframe::egui;
use eframe::emath::GuiRounding as _;

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
            tray_preview_row(ui, look.is_some_and(|look| look.light_taskbar));
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
                "Starts in the tray when you sign in.",
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
    /// The taskbar is light (`SystemUsesLightTheme`; unreadable = dark, as
    /// the tray assumes): what the tray icon preview sits on.
    pub light_taskbar: bool,
}

impl SystemLook {
    pub(crate) fn load(cx: &UiCtx) -> Task<SystemLook> {
        cx.tasks.spawn("system-look", |_| SystemLook {
            accent: system::accent_palette()
                .map(|shades| shades.map(|[r, g, b]| egui::Color32::from_rgb(r, g, b))),
            light_taskbar: system::system_uses_light_theme().unwrap_or(false),
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

/// Under the tray icon choice: each set's idle, recording and transcribing
/// glyphs on the taskbar's colour, each tile under its segment.
fn tray_preview_row(ui: &mut egui::Ui, light_taskbar: bool) {
    row(
        ui,
        "appearance-tray-preview",
        Slot::Fill(FIELD_WIDTH),
        |ui| {
            let id = title(ui, "Preview");
            note(ui, "Idle, recording and transcribing, on your taskbar.");
            id
        },
        |ui, label| {
            let height = ui.spacing().interact_size.y;
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), height),
                egui::Sense::hover(),
            );
            response.labelled_by(label);
            // The taskbar's own colours, not the app palette's.
            let taskbar = if light_taskbar {
                egui::Color32::from_rgb(238, 238, 238)
            } else {
                egui::Color32::from_rgb(32, 32, 32)
            };
            let ppp = ui.ctx().pixels_per_point();
            // The frame the tray would pick for a 16 pt small icon.
            let px = (16.0 * ppp).round().max(1.0) as u32;
            let inner = rect.shrink(SEGMENT_INSET);
            let width = inner.width() / 2.0;
            for (i, colour) in [false, true].into_iter().enumerate() {
                let tile = egui::Rect::from_min_size(
                    inner.min + egui::vec2(width * i as f32, 0.0),
                    egui::vec2(width, inner.height()),
                )
                .shrink2(egui::vec2(SEGMENT_INSET, 0.0));
                ui.painter().rect(
                    tile,
                    egui::CornerRadius::same(theme::R_SM),
                    taskbar,
                    egui::Stroke::new(1.0, pal().border),
                    egui::StrokeKind::Inside,
                );
                let Some(glyphs) = tray_textures(ui.ctx(), colour, light_taskbar, px) else {
                    continue;
                };
                const GAP: f32 = 12.0;
                let sides = glyphs.each_ref().map(|t| t.size_vec2() / ppp);
                let total = sides.iter().map(|s| s.x).sum::<f32>() + GAP * 2.0;
                let mut x = tile.center().x - total / 2.0;
                for ((texture, side), name) in
                    glyphs
                        .iter()
                        .zip(sides)
                        .zip(["Idle", "Recording", "Transcribing"])
                {
                    let min = egui::pos2(x, tile.center().y - side.y / 2.0);
                    let at = egui::Rect::from_min_size(min.round_to_pixels(ppp), side);
                    ui.painter().image(
                        texture.id(),
                        at,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                    ui.interact(
                        at,
                        ui.id().with(("tray-preview", i, name)),
                        egui::Sense::hover(),
                    )
                    .on_hover_text(name);
                    x += side.x + GAP;
                }
            }
        },
    );
}

/// The preview's textures for one icon set at `px`, decoded once per
/// context (a failure is logged once and leaves the tile empty).
fn tray_textures(
    ctx: &egui::Context,
    colour: bool,
    light_taskbar: bool,
    px: u32,
) -> Option<[egui::TextureHandle; 3]> {
    let id = egui::Id::new(("tray-preview", colour, light_taskbar, px));
    if let Some(cached) = ctx.data(|d| d.get_temp::<Option<[egui::TextureHandle; 3]>>(id)) {
        return cached;
    }
    let loaded = match tray::preview_glyphs(colour, light_taskbar, px) {
        Ok(glyphs) => {
            let mut i = 0;
            Some(glyphs.map(|glyph| {
                i += 1;
                ctx.load_texture(
                    format!("tray-preview-{colour}-{light_taskbar}-{px}-{i}"),
                    egui::ColorImage::from_rgba_unmultiplied(
                        [glyph.width as usize, glyph.height as usize],
                        &glyph.rgba,
                    ),
                    egui::TextureOptions::LINEAR,
                )
            }))
        }
        Err(error) => {
            log::warn!("tray icon preview: {error:#}");
            None
        }
    };
    ctx.data_mut(|d| d.insert_temp(id, loaded.clone()));
    loaded
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
