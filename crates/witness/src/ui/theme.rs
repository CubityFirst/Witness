//! The dark palette from `styles.css` `:root`, the egui `Visuals` mapped to
//! it (map-ui-shell §9), and app-wide style options. Dark only: there is no
//! light theme, and the palette does not follow Windows high-contrast themes
//! in 0.3.0 (accepted regression R21).

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x0f, 0x10, 0x13);
pub const BG_RAISED: Color32 = Color32::from_rgb(0x17, 0x18, 0x1c);
pub const BG_HOVER: Color32 = Color32::from_rgb(0x1f, 0x21, 0x26);
/// Pressed / selected rows.
pub const BG_ACTIVE: Color32 = Color32::from_rgb(0x26, 0x29, 0x30);
pub const BORDER: Color32 = Color32::from_rgb(0x26, 0x29, 0x2f);
/// Input and secondary-button borders.
pub const BORDER_STRONG: Color32 = Color32::from_rgb(0x34, 0x38, 0x41);
pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xee);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x96, 0x9b, 0xa5);
/// Tertiary text: timestamps, placeholders, hints.
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x6b, 0x70, 0x7a);
pub const ACCENT: Color32 = Color32::from_rgb(0x6e, 0x8e, 0xff);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x8a, 0xa3, 0xff);
/// Accent @ 16 %: selected nav, active segment, search hits.
pub const ACCENT_SOFT: Color32 = Color32::from_rgba_unmultiplied_const(0x6e, 0x8e, 0xff, 41);
pub const REC: Color32 = Color32::from_rgb(0xf0, 0x52, 0x52);
pub const OK: Color32 = Color32::from_rgb(0x3f, 0xb9, 0x50);
pub const WARN: Color32 = Color32::from_rgb(0xe0, 0xa5, 0x26);
pub const FOCUS: Color32 = Color32::from_rgb(0xa8, 0xb9, 0xff);
/// Light red text: record button, recording pill, failed badges, live header.
pub const REC_LIGHT: Color32 = Color32::from_rgb(0xff, 0x85, 0x89);
/// Hover background of stop / danger buttons.
pub const REC_HOVER: Color32 = Color32::from_rgb(0xd1, 0x35, 0x39);
/// Light green text: transcribed badge, captions "Them".
pub const OK_LIGHT: Color32 = Color32::from_rgb(0x6f, 0xcf, 0x82);
/// Live-feed remote ("Them") chip.
pub const THEM_CHIP: Color32 = Color32::from_rgb(0x2f, 0x7c, 0x3d);
/// Search-hit `<mark>` text.
pub const MARK_TEXT: Color32 = Color32::from_rgb(0xdd, 0xe4, 0xff);
pub const CAPTION_TEXT: Color32 = Color32::from_rgb(0xee, 0xf1, 0xf5);
pub const CAPTION_ERR: Color32 = Color32::from_rgb(0xff, 0xb4, 0xb7);
pub const CAPTION_BG: Color32 = Color32::from_rgb(0x0c, 0x0d, 0x10);
pub const WHITE: Color32 = Color32::WHITE;

/// Recording pill background: red 15 % over `BG_RAISED`.
pub const PILL_REC_BG: Color32 = Color32::from_rgb(0x3a, 0x25, 0x2b);
/// Busy pill background: warn 15 % over `BG_RAISED`.
pub const PILL_BUSY_BG: Color32 = Color32::from_rgb(0x38, 0x32, 0x24);
/// Inline warning (warn 8 % / 35 % over `BG`).
pub const WARN_BG: Color32 = Color32::from_rgb(0x23, 0x21, 0x1b);
pub const WARN_BORDER: Color32 = Color32::from_rgb(0x58, 0x46, 0x1c);
/// `<mark>` background: warn @ 0.35.
pub const MARK_BG: Color32 = Color32::from_rgba_unmultiplied_const(0x6e, 0x8e, 0xff, 66);
/// Active transcript segment: accent @ 0.12 fill, @ 0.35 outline.
pub const ACTIVE_SEG_BG: Color32 = Color32::from_rgba_unmultiplied_const(0x6e, 0x8e, 0xff, 28);
pub const ACTIVE_SEG_STROKE: Color32 = Color32::from_rgba_unmultiplied_const(0x6e, 0x8e, 0xff, 0);
/// Bookmark row: warn @ 0.06.
pub const BOOKMARK_ROW_BG: Color32 = Color32::from_rgba_unmultiplied_const(0xd5, 0xa0, 0x21, 15);
/// `.view-error`: red @ 0.08 fill, @ 0.5 border.
pub const VIEW_ERROR_BG: Color32 = Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 20);
pub const VIEW_ERROR_BORDER: Color32 = Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 128);
/// Badge tints (red .18, warn .15, ok .15).
pub const BADGE_REC_BG: Color32 = Color32::from_rgba_unmultiplied_const(0xe5, 0x48, 0x4d, 46);
pub const BADGE_WARN_BG: Color32 = Color32::from_rgba_unmultiplied_const(0xd5, 0xa0, 0x21, 38);
pub const BADGE_OK_BG: Color32 = Color32::from_rgba_unmultiplied_const(0x46, 0xa7, 0x58, 38);
/// Modal backdrop: black @ 0.55.
pub const BACKDROP: Color32 = Color32::from_rgba_unmultiplied_const(0, 0, 0, 140);

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
/// narrow look is the normal look.
pub fn narrow(ui: &egui::Ui) -> bool {
    ui.ctx().content_rect().width() <= 620.0
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

/// A speaker's colour as readable text / avatar fill on the dark
/// background: the stable chip colour, lifted towards white.
pub fn speaker_text(label: &str) -> Color32 {
    lighten(chip_color(label), 0.35)
}

/// Mix `c` towards white by `t` (0..1).
pub fn lighten(c: Color32, t: f32) -> Color32 {
    let mix = |v: u8| (v as f32 + (255.0 - v as f32) * t).round() as u8;
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
    ctx.set_theme(egui::Theme::Dark);
    ctx.options_mut(|o| {
        o.zoom_with_keyboard = false;
        o.quit_shortcuts.clear();
    });
    ctx.all_styles_mut(style);
    super::icons::set_animations(crate::platform::system::client_area_animations());
}

pub(crate) fn style(s: &mut egui::Style) {
    use egui::TextStyle;
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
    s.spacing.scroll = egui::style::ScrollStyle {
        bar_width: 6.0,
        ..egui::style::ScrollStyle::solid()
    };
    // No edge fade: CSS scrollers have none, and it dimmed the rows at the
    // edges (the last meeting row, the segment above the player bar).
    s.spacing.scroll.fade.strength = 0.0;

    let v = &mut s.visuals;
    v.dark_mode = true;
    v.override_text_color = None;
    v.weak_text_color = Some(TEXT_MUTED);
    v.panel_fill = BG;
    v.window_fill = BG_RAISED;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(R_LG);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 12],
        blur: 40,
        spread: 0,
        color: Color32::from_black_alpha(128),
    };
    v.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(115),
    };
    v.extreme_bg_color = BG_RAISED; // text-edit background
    v.text_edit_bg_color = Some(BG_RAISED);
    v.faint_bg_color = BG_RAISED;
    v.code_bg_color = BG_RAISED;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = REC;
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(0x6e, 0x8e, 0xff, 89);
    v.selection.stroke = Stroke::new(1.0, FOCUS);
    v.text_cursor.stroke = Stroke::new(2.0, TEXT);
    v.disabled_alpha = 0.55;

    let radius = CornerRadius::same(R_MD);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BG;
    w.noninteractive.weak_bg_fill = BG;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = radius;
    for state in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        // Buttons and combo boxes fill with `weak_bg_fill`. The strong
        // `bg_fill` is egui's for scroll handles and slider rails (plus
        // checkbox / radio boxes and slider handles, which `widgets` styles
        // itself): the `--border` thumb, `--text-muted` on hover / drag.
        state.bg_fill = TEXT_MUTED;
        state.weak_bg_fill = BG_HOVER;
        state.bg_stroke = Stroke::new(1.0, BORDER);
        state.fg_stroke = Stroke::new(1.0, TEXT);
        state.corner_radius = radius;
        state.expansion = 0.0;
    }
    w.inactive.bg_fill = BORDER_STRONG;
    w.inactive.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    w.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    w.hovered.weak_bg_fill = BG_ACTIVE;
    // Pressed / focused controls: the border turns accent.
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
}

/// A themed context for widget tests (fonts registered, like the app).
#[cfg(test)]
pub fn test_context() -> egui::Context {
    let ctx = egui::Context::default();
    install(&ctx);
    ctx
}

/// `App::clear_color`: BG in normalized gamma space (no white resize gaps).
pub fn clear_color() -> [f32; 4] {
    BG.to_normalized_gamma_f32()
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
        assert_eq!(w.inactive.bg_fill, BORDER_STRONG);
        assert_eq!(w.hovered.bg_fill, TEXT_MUTED);
        assert_eq!(w.active.bg_fill, TEXT_MUTED);
        // Secondary controls: hover fill, a step lighter on hover.
        assert_eq!(w.inactive.weak_bg_fill, BG_HOVER);
        assert_eq!(w.hovered.weak_bg_fill, BG_ACTIVE);
        // No edge fade (CSS scrollers have none).
        assert_eq!(s.spacing.scroll.fade.strength, 0.0);
    }

    #[test]
    fn tints_have_the_css_alpha() {
        // Search hits and the active segment are accent tints.
        assert_eq!(MARK_BG.a(), 66);
        assert_eq!(BACKDROP.a(), (0.55f32 * 255.0).round() as u8);
        assert_eq!(ACTIVE_SEG_BG.a(), 28);
        assert_eq!(ACCENT_SOFT.a(), 41);
    }
}
