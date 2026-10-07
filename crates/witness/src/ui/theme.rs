//! The palette, the egui `Visuals` mapped to it, and app-wide style
//! options. Two palettes — dark and light (docs/UI.md) — chosen by the
//! Appearance setting (System follows Windows' "app mode"), with the accent
//! taken from Windows' accent colour unless the user picks Witness blue.
//! Colours are read through [`pal`] at paint time, so a theme change applies
//! on the next frame. The live-caption overlay stays dark in both themes.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke};
use std::sync::RwLock;

/// Every colour the UI paints with. Copy it out with [`pal`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// The window background behind every view (the shell's panels and
    /// `clear_color`); opaque masks use `bg`.
    pub window_bg: Color32,
    pub bg: Color32,
    pub bg_raised: Color32,
    pub bg_hover: Color32,
    /// Pressed / selected rows.
    pub bg_active: Color32,
    pub border: Color32,
    /// Input and secondary-button borders.
    pub border_strong: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    /// Tertiary text: timestamps, placeholders, hints.
    pub text_faint: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    /// Accent tint: selected nav, active segment, search hits.
    pub accent_soft: Color32,
    /// Text / glyphs on an accent fill (white or near-black by contrast).
    pub on_accent: Color32,
    pub rec: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub focus: Color32,
    /// Red text: record button, recording pill, failed badges, live header.
    pub rec_light: Color32,
    /// Hover fill of stop / danger buttons.
    pub rec_hover: Color32,
    /// Green text: transcribed badge, captions "Them".
    pub ok_light: Color32,
    /// Live-feed remote ("Them") chip.
    pub them_chip: Color32,
    /// Search-hit text.
    pub mark_text: Color32,
    pub caption_text: Color32,
    pub caption_err: Color32,
    pub caption_bg: Color32,
    pub white: Color32,
    /// Recording pill background.
    pub pill_rec_bg: Color32,
    /// Busy pill background.
    pub pill_busy_bg: Color32,
    /// Inline warning fill / border.
    pub warn_bg: Color32,
    pub warn_border: Color32,
    /// Search-hit background.
    pub mark_bg: Color32,
    /// Active transcript segment fill / outline.
    pub active_seg_bg: Color32,
    pub active_seg_stroke: Color32,
    /// Bookmark row tint.
    pub bookmark_row_bg: Color32,
    /// Error box fill / border.
    pub view_error_bg: Color32,
    pub view_error_border: Color32,
    /// Badge tints (red, warn, ok).
    pub badge_rec_bg: Color32,
    pub badge_warn_bg: Color32,
    pub badge_ok_bg: Color32,
    /// Modal backdrop.
    pub backdrop: Color32,
}

/// Witness blue: the accent when Windows' is not used.
pub const WITNESS_ACCENT: Color32 = Color32::from_rgb(0x6e, 0x8e, 0xff);
/// Witness blue for light backgrounds.
const WITNESS_ACCENT_LIGHT: Color32 = Color32::from_rgb(0x3d, 0x5c, 0xe0);

const fn rgba(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(c.r(), c.g(), c.b(), a)
}

impl Palette {
    /// The dark palette around `accent` (a light-enough accent shade).
    pub fn dark(accent: Color32) -> Palette {
        // Neutral greys (no blue cast next to Windows' accent).
        let bg = Color32::from_rgb(0x13, 0x13, 0x13);
        Palette {
            dark: true,
            window_bg: bg,
            bg,
            bg_raised: Color32::from_rgb(0x1c, 0x1c, 0x1c),
            bg_hover: Color32::from_rgb(0x25, 0x25, 0x25),
            bg_active: Color32::from_rgb(0x2c, 0x2c, 0x2c),
            border: Color32::from_rgb(0x2a, 0x2a, 0x2a),
            border_strong: Color32::from_rgb(0x3b, 0x3b, 0x3b),
            text: Color32::from_rgb(0xec, 0xec, 0xec),
            text_muted: Color32::from_rgb(0xa3, 0xa3, 0xa3),
            text_faint: Color32::from_rgb(0x72, 0x72, 0x72),
            accent,
            accent_hover: lighten(accent, 0.18),
            accent_soft: rgba(accent, 41),
            on_accent: on(accent),
            rec: Color32::from_rgb(0xf0, 0x52, 0x52),
            ok: Color32::from_rgb(0x3f, 0xb9, 0x50),
            warn: Color32::from_rgb(0xe0, 0xa5, 0x26),
            focus: lighten(accent, 0.35),
            rec_light: Color32::from_rgb(0xff, 0x85, 0x89),
            rec_hover: Color32::from_rgb(0xd1, 0x35, 0x39),
            ok_light: Color32::from_rgb(0x6f, 0xcf, 0x82),
            them_chip: Color32::from_rgb(0x2f, 0x7c, 0x3d),
            mark_text: lighten(accent, 0.75),
            caption_text: Color32::from_rgb(0xee, 0xf1, 0xf5),
            caption_err: Color32::from_rgb(0xff, 0xb4, 0xb7),
            caption_bg: Color32::from_rgb(0x0c, 0x0d, 0x10),
            white: Color32::WHITE,
            pill_rec_bg: Color32::from_rgb(0x3a, 0x25, 0x2b),
            pill_busy_bg: Color32::from_rgb(0x38, 0x32, 0x24),
            warn_bg: Color32::from_rgb(0x23, 0x21, 0x1b),
            warn_border: Color32::from_rgb(0x58, 0x46, 0x1c),
            mark_bg: rgba(accent, 66),
            active_seg_bg: rgba(accent, 28),
            active_seg_stroke: rgba(accent, 0),
            bookmark_row_bg: Color32::from_rgba_unmultiplied_const(0xd5, 0xa0, 0x21, 15),
            view_error_bg: Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 20),
            view_error_border: Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 128),
            badge_rec_bg: Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 46),
            badge_warn_bg: Color32::from_rgba_unmultiplied_const(0xd5, 0xa0, 0x21, 38),
            badge_ok_bg: Color32::from_rgba_unmultiplied_const(0x46, 0xa7, 0x58, 38),
            backdrop: Color32::from_rgba_unmultiplied_const(0, 0, 0, 140),
        }
    }

    /// The light palette around `accent` (a dark-enough accent shade).
    pub fn light(accent: Color32) -> Palette {
        let bg = Color32::from_rgb(0xf6, 0xf6, 0xf6);
        Palette {
            dark: false,
            window_bg: bg,
            bg,
            bg_raised: Color32::from_rgb(0xff, 0xff, 0xff),
            bg_hover: Color32::from_rgb(0xec, 0xec, 0xec),
            bg_active: Color32::from_rgb(0xe3, 0xe3, 0xe3),
            border: Color32::from_rgb(0xe5, 0xe5, 0xe5),
            border_strong: Color32::from_rgb(0xcc, 0xcc, 0xcc),
            text: Color32::from_rgb(0x1b, 0x1b, 0x1b),
            text_muted: Color32::from_rgb(0x5f, 0x5f, 0x5f),
            text_faint: Color32::from_rgb(0x8a, 0x8a, 0x8a),
            accent,
            accent_hover: darken(accent, 0.12),
            accent_soft: rgba(accent, 33),
            on_accent: on(accent),
            rec: Color32::from_rgb(0xd9, 0x36, 0x36),
            ok: Color32::from_rgb(0x1a, 0x7f, 0x37),
            warn: Color32::from_rgb(0xa1, 0x6c, 0x00),
            focus: accent,
            rec_light: Color32::from_rgb(0xc4, 0x24, 0x24),
            rec_hover: Color32::from_rgb(0xb4, 0x23, 0x23),
            ok_light: Color32::from_rgb(0x1a, 0x7f, 0x37),
            them_chip: Color32::from_rgb(0x2f, 0x7c, 0x3d),
            mark_text: darken(accent, 0.55),
            caption_text: Color32::from_rgb(0xee, 0xf1, 0xf5),
            caption_err: Color32::from_rgb(0xff, 0xb4, 0xb7),
            caption_bg: Color32::from_rgb(0x0c, 0x0d, 0x10),
            white: Color32::WHITE,
            pill_rec_bg: Color32::from_rgb(0xfd, 0xe6, 0xe6),
            pill_busy_bg: Color32::from_rgb(0xfb, 0xf0, 0xd9),
            warn_bg: Color32::from_rgb(0xfe, 0xf7, 0xe6),
            warn_border: Color32::from_rgb(0xec, 0xd2, 0x8c),
            mark_bg: rgba(accent, 51),
            active_seg_bg: rgba(accent, 24),
            active_seg_stroke: rgba(accent, 0),
            bookmark_row_bg: Color32::from_rgba_unmultiplied_const(0xd5, 0xa0, 0x21, 22),
            view_error_bg: Color32::from_rgba_unmultiplied_const(0xd9, 0x36, 0x36, 15),
            view_error_border: Color32::from_rgba_unmultiplied_const(0xd9, 0x36, 0x36, 102),
            badge_rec_bg: Color32::from_rgba_unmultiplied_const(0xd9, 0x36, 0x36, 31),
            badge_warn_bg: Color32::from_rgba_unmultiplied_const(0xd5, 0xa0, 0x21, 38),
            badge_ok_bg: Color32::from_rgba_unmultiplied_const(0x1a, 0x7f, 0x37, 31),
            backdrop: Color32::from_rgba_unmultiplied_const(0, 0, 0, 90),
        }
    }

    /// The palette for `dark` with an optional Windows accent palette
    /// (`AccentPalette`: light3, light2, light1, base, dark1, dark2, dark3).
    /// Dark mode takes a light shade, light mode a dark one, as Windows does.
    pub fn resolve(dark: bool, windows_accent: Option<&[Color32; 7]>) -> Palette {
        match (dark, windows_accent) {
            (true, Some(shades)) => Palette::dark(shades[1]),
            (false, Some(shades)) => Palette::light(shades[4]),
            (true, None) => Palette::dark(WITNESS_ACCENT),
            (false, None) => Palette::light(WITNESS_ACCENT_LIGHT),
        }
    }
}

static CURRENT: RwLock<Option<Palette>> = RwLock::new(None);

/// The palette in use (dark Witness blue until [`apply`] runs).
pub fn pal() -> Palette {
    CURRENT
        .read()
        .ok()
        .and_then(|p| *p)
        .unwrap_or_else(|| Palette::dark(WITNESS_ACCENT))
}

/// Make `palette` current and restyle `ctx` (next frame repaints with it).
pub fn apply(ctx: &egui::Context, palette: Palette) {
    if let Ok(mut current) = CURRENT.write() {
        if *current == Some(palette) {
            return;
        }
        *current = Some(palette);
    }
    ctx.set_theme(if palette.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    ctx.all_styles_mut(style);
    ctx.request_repaint();
}

/// White or near-black, whichever reads better on `fill`.
fn on(fill: Color32) -> Color32 {
    let lum = |v: u8| {
        let c = v as f32 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let l = 0.2126 * lum(fill.r()) + 0.7152 * lum(fill.g()) + 0.0722 * lum(fill.b());
    // Contrast with white vs. with #111: pick the larger.
    let with_white = 1.05 / (l + 0.05);
    let with_black = (l + 0.05) / (0.0056 + 0.05);
    if with_white >= with_black {
        Color32::WHITE
    } else {
        Color32::from_rgb(0x11, 0x12, 0x14)
    }
}

/// Speaker chip colour by (immutable) label, so colours are stable across
/// reloads: `me`, S1–S8; anything else uses S1's.
pub fn chip_color(label: &str) -> Color32 {
    match label {
        "me" => Color32::from_rgb(0x2f, 0x6f, 0xb2),
        "S2" => Color32::from_rgb(0xb0, 0x58, 0x2a),
        "S3" => Color32::from_rgb(0x23, 0x73, 0x5d),
        "S4" => Color32::from_rgb(0xb0, 0x4a, 0x7e),
        "S5" => Color32::from_rgb(0x8a, 0x6a, 0x12),
        "S6" => Color32::from_rgb(0x1d, 0x6a, 0x86),
        "S7" => Color32::from_rgb(0x55, 0x57, 0xb0),
        "S8" => Color32::from_rgb(0x7b, 0x5a, 0x3a),
        _ => Color32::from_rgb(0x7a, 0x4d, 0xbd), // S1
    }
}

/// Body text size (CSS 14 px; egui points equal CSS px at the same zoom).
pub const BASE_SIZE: f32 = 14.0;

/// The family registered for Segoe UI Semibold (CSS font-weight 600);
/// egui has no synthetic bold.
pub const SEMIBOLD: &str = "semibold";

/// Segoe UI Semibold at `size` points.
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

/// Segoe UI (regular) at `size` points.
pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

/// The `@media (max-width: 620px)` switch. At the default 460 px window the
/// narrow look is the normal look. Inside a split column it measures the
/// column ([`set_pane_width`]), not the window.
pub fn narrow(ui: &egui::Ui) -> bool {
    let ctx = ui.ctx();
    let width = ctx
        .data(|d| d.get_temp::<f32>(pane_width_id()))
        .unwrap_or_else(|| ctx.content_rect().width());
    width <= 620.0
}

fn pane_width_id() -> egui::Id {
    egui::Id::new("witness-pane-width")
}

/// The shell draws a split column `width` wide (`None` after it).
pub fn set_pane_width(ctx: &egui::Context, width: Option<f32>) {
    ctx.data_mut(|d| match width {
        Some(w) => {
            d.insert_temp(pane_width_id(), w);
        }
        None => {
            d.remove::<f32>(pane_width_id());
        }
    });
}

/// Spacing scale (pt).
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;
pub const S5: f32 = 24.0;
/// Corner radii: small chips, controls/rows, surfaces/modals.
pub const R_SM: u8 = 6;
pub const R_MD: u8 = 8;
pub const R_LG: u8 = 12;

/// The content column's maximum width.
pub const CONTENT_MAX: f32 = 760.0;
/// Below this width layouts switch to stacked / icon-only variants.
pub const COMPACT_BELOW: f32 = 460.0;

/// A speaker's colour as readable text on the page background: the
/// stable chip colour lifted towards white (dark) or deepened (light).
pub fn speaker_text(label: &str) -> Color32 {
    ink(chip_color(label))
}

/// `c` made readable as text / glyphs on the current page background.
pub fn ink(c: Color32) -> Color32 {
    if pal().dark {
        lighten(c, 0.35)
    } else {
        darken(c, 0.2)
    }
}

/// Mix `c` towards white by `t` (0..1).
pub fn lighten(c: Color32, t: f32) -> Color32 {
    let mix = |v: u8| (v as f32 + (255.0 - v as f32) * t).round() as u8;
    Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

/// Mix `c` towards black by `t` (0..1).
pub fn darken(c: Color32, t: f32) -> Color32 {
    let mix = |v: u8| (v as f32 * (1.0 - t)).round() as u8;
    Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

/// `c` at `alpha` (0..1) as an unmultiplied colour.
pub fn alpha(c: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha * 255.0).round() as u8)
}

/// Install the theme: eager fonts, visuals, spacing and the app-wide
/// options (no keyboard zoom — `app.rs` zooms the root viewport only — and
/// no Ctrl+Q quit shortcut, which the web app never had).
pub fn install(ctx: &egui::Context) {
    super::fonts::install_eager(ctx);
    ctx.options_mut(|o| {
        o.zoom_with_keyboard = false;
        o.quit_shortcuts.clear();
        // The app follows its own Appearance setting, not egui's.
        o.theme_preference = egui::ThemePreference::Dark;
    });
    let p = pal();
    ctx.set_theme(if p.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    ctx.all_styles_mut(style);
    super::icons::set_animations(crate::platform::system::client_area_animations());
}

pub(crate) fn style(s: &mut egui::Style) {
    use egui::TextStyle;
    let p = pal();
    s.text_styles = [
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Body, FontId::proportional(BASE_SIZE)),
        (TextStyle::Button, FontId::proportional(BASE_SIZE)),
        (TextStyle::Heading, semibold(20.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();

    // Text selection is off app-wide (`user-select: none`); toasts, inline
    // warnings and transcript text opt back in per widget.
    s.interaction.selectable_labels = false;

    s.spacing.item_spacing = egui::vec2(8.0, 8.0);
    s.spacing.button_padding = egui::vec2(10.0, 6.0);
    s.spacing.interact_size.y = 32.0;
    s.spacing.menu_margin = Margin::same(6);
    // Floating bars: they overlay the page padding instead of taking
    // width, so a view's scrolled column lines up exactly with the shell's
    // search box and banners. Thin and visible at rest, wider on hover.
    s.spacing.scroll = egui::style::ScrollStyle {
        floating: true,
        bar_width: 8.0,
        floating_width: 4.0,
        floating_allocated_width: 0.0,
        bar_outer_margin: 2.0,
        foreground_color: false,
        dormant_background_opacity: 0.0,
        dormant_handle_opacity: 0.7,
        ..egui::style::ScrollStyle::solid()
    };
    // No edge fade: it dimmed the rows at the edges (the last meeting row,
    // the segment above the player bar).
    s.spacing.scroll.fade.strength = 0.0;

    let v = &mut s.visuals;
    v.dark_mode = p.dark;
    v.override_text_color = None;
    v.weak_text_color = Some(p.text_muted);
    v.panel_fill = p.bg;
    v.window_fill = p.bg_raised;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(R_LG);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 12],
        blur: 40,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 128 } else { 40 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 115 } else { 36 }),
    };
    v.extreme_bg_color = p.bg_raised; // text-edit background
    v.text_edit_bg_color = Some(p.bg_raised);
    v.faint_bg_color = p.bg_raised;
    v.code_bg_color = p.bg_raised;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.rec;
    v.selection.bg_fill = alpha(p.accent, 0.35);
    v.selection.stroke = Stroke::new(1.0, p.focus);
    v.text_cursor.stroke = Stroke::new(2.0, p.text);
    v.disabled_alpha = 0.55;

    let radius = CornerRadius::same(R_MD);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = radius;
    for state in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        // Buttons and combo boxes fill with `weak_bg_fill`. The strong
        // `bg_fill` is egui's for scroll handles and slider rails (plus
        // checkbox / radio boxes and slider handles, which `widgets` styles
        // itself): the `--border` thumb, `--text-muted` on hover / drag.
        state.bg_fill = p.text_muted;
        state.weak_bg_fill = p.bg_hover;
        state.bg_stroke = Stroke::new(1.0, p.border);
        state.fg_stroke = Stroke::new(1.0, p.text);
        state.corner_radius = radius;
        state.expansion = 0.0;
    }
    w.inactive.bg_fill = p.border_strong;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.hovered.weak_bg_fill = p.bg_active;
    // Pressed / focused controls: the border turns accent.
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
}

/// A themed context for widget tests (fonts registered, like the app).
#[cfg(test)]
pub fn test_context() -> egui::Context {
    let ctx = egui::Context::default();
    install(&ctx);
    ctx
}

/// `App::clear_color`: the window background in normalized gamma space
/// (no white resize gaps).
pub fn clear_color() -> [f32; 4] {
    pal().window_bg.to_normalized_gamma_f32()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_colours_by_label() {
        assert_eq!(chip_color("me"), Color32::from_rgb(0x2f, 0x6f, 0xb2));
        assert_eq!(chip_color("S1"), Color32::from_rgb(0x7a, 0x4d, 0xbd));
        assert_eq!(chip_color("S8"), Color32::from_rgb(0x7b, 0x5a, 0x3a));
        assert_eq!(chip_color("S9"), chip_color("S1"));
        assert_eq!(chip_color(""), chip_color("S1"));
    }

    #[test]
    fn scroll_thumbs_follow_the_spec() {
        let mut s = egui::Style::default();
        style(&mut s);
        let w = &s.visuals.widgets;
        assert!(!s.spacing.scroll.foreground_color);
        assert_eq!(w.inactive.bg_fill, pal().border_strong);
        assert_eq!(w.hovered.bg_fill, pal().text_muted);
        assert_eq!(w.active.bg_fill, pal().text_muted);
        // Secondary controls: hover fill, a step lighter on hover.
        assert_eq!(w.inactive.weak_bg_fill, pal().bg_hover);
        assert_eq!(w.hovered.weak_bg_fill, pal().bg_active);
        // No edge fade (CSS scrollers have none).
        assert_eq!(s.spacing.scroll.fade.strength, 0.0);
    }

    #[test]
    fn tints_have_the_css_alpha() {
        // Search hits and the active segment are accent tints.
        assert_eq!(pal().mark_bg.a(), 66);
        assert_eq!(pal().backdrop.a(), (0.55f32 * 255.0).round() as u8);
        assert_eq!(pal().active_seg_bg.a(), 28);
        assert_eq!(pal().accent_soft.a(), 41);
    }
}
