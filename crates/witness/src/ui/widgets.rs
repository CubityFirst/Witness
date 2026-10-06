//! The shared widget kit: the CSS button family (`.btn`, `.btn-ghost`,
//! `.btn-danger`, `.btn-record` / `.btn-stop`, `.icon-btn`, `.nav-btn`,
//! `.win-btn`), pills, chips, badges, warning/error boxes, layout helpers,
//! keyboard scrolling, the text-field context menu and AccessKit live
//! regions. Views use these rather than fighting the global `Visuals`
//! (map-ui-shell §9) — and do not fork them.

use super::theme::{
    self, narrow, BADGE_OK_BG, BADGE_REC_BG, BADGE_WARN_BG, BG_HOVER, BG_RAISED, BORDER, OK_LIGHT,
    PILL_BUSY_BG, PILL_REC_BG, REC, REC_HOVER, REC_LIGHT, TEXT, TEXT_MUTED, VIEW_ERROR_BG,
    VIEW_ERROR_BORDER, WARN, WARN_BG, WARN_BORDER, WHITE,
};
use eframe::egui::{
    self, Color32, CornerRadius, FontId, Frame, Margin, Response, Sense, Stroke, TextWrapMode,
    WidgetText,
};

// ---------------------------------------------------------------------------
// Buttons

/// Colours and metrics of one button family member.
#[derive(Clone, Copy, Debug)]
struct ButtonStyle {
    fill: Color32,
    hover_fill: Color32,
    stroke: Color32,
    hover_stroke: Color32,
    text: Color32,
    hover_text: Color32,
    padding: egui::Vec2,
    radius: u8,
    font: Font,
    /// Fixed outer size (window buttons); otherwise content + padding.
    fixed_size: Option<egui::Vec2>,
}

#[derive(Clone, Copy, Debug)]
enum Font {
    Regular(f32),
    Semibold(f32),
}

impl Font {
    fn id(self) -> FontId {
        match self {
            Font::Regular(size) => theme::regular(size),
            Font::Semibold(size) => theme::semibold(size),
        }
    }
}

/// `.btn` padding: 6/14, or 6/10 at the (default) narrow width.
fn btn_padding(ui: &egui::Ui) -> egui::Vec2 {
    if narrow(ui) {
        egui::vec2(10.0, 6.0)
    } else {
        egui::vec2(14.0, 6.0)
    }
}

fn btn_style(ui: &egui::Ui) -> ButtonStyle {
    ButtonStyle {
        fill: BG_HOVER,
        hover_fill: BG_HOVER,
        stroke: BORDER,
        hover_stroke: theme::ACCENT,
        text: TEXT,
        hover_text: TEXT,
        padding: btn_padding(ui),
        radius: 6,
        font: Font::Regular(theme::BASE_SIZE),
        fixed_size: None,
    }
}

fn danger_style(ui: &egui::Ui) -> ButtonStyle {
    ButtonStyle {
        fill: REC,
        hover_fill: REC_HOVER,
        stroke: REC,
        hover_stroke: REC_LIGHT,
        text: WHITE,
        hover_text: WHITE,
        font: Font::Semibold(theme::BASE_SIZE),
        ..btn_style(ui)
    }
}

/// Lay out `content`, allocate it with `style`'s padding and paint it. The
/// widget's accessible name is its text (or `label` for icon-only buttons).
fn styled_button(
    ui: &mut egui::Ui,
    style: ButtonStyle,
    content: WidgetText,
    label: Option<&str>,
) -> Response {
    let galley = content.into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        style.font.id(),
    );
    let size = style.fixed_size.unwrap_or_else(|| {
        let s = galley.size() + style.padding * 2.0;
        egui::vec2(s.x, s.y.max(ui.spacing().interact_size.y))
    });
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let name = label
        .map(str::to_owned)
        .unwrap_or_else(|| galley.text().to_owned());
    let enabled = ui.is_enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &name));

    if ui.is_rect_visible(rect) {
        // Keyboard focus looks like hover (`:focus-visible`); focus that came
        // with a click, or programmatic focus after one, does not.
        let ring = response.has_focus() && focus_visible(ui.ctx());
        let hovered = enabled && (response.hovered() || ring);
        let (fill, stroke, text) = if hovered {
            (style.hover_fill, style.hover_stroke, style.hover_text)
        } else {
            (style.fill, style.stroke, style.text)
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(style.radius),
            fill,
            Stroke::new(
                if stroke == Color32::TRANSPARENT {
                    0.0
                } else {
                    1.0
                },
                stroke,
            ),
            egui::StrokeKind::Inside,
        );
        let pos = rect.center() - galley.size() / 2.0;
        painter.galley(pos, galley, text);
        if ring {
            // `:focus-visible`: 2 px focus ring, 2 px outside.
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(style.radius + 2),
                Stroke::new(2.0, theme::FOCUS),
                egui::StrokeKind::Outside,
            );
        }
    }
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response.on_hover_cursor(egui::CursorIcon::NotAllowed)
    }
}

/// `.btn`: raised fill, 1 px border that turns accent on hover.
pub fn btn(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> Response {
    let style = btn_style(ui);
    styled_button(ui, style, text.into(), None)
}

/// `.btn.btn-ghost`: transparent fill, border kept.
pub fn btn_ghost(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> Response {
    let style = ButtonStyle {
        fill: Color32::TRANSPARENT,
        hover_fill: Color32::TRANSPARENT,
        ..btn_style(ui)
    };
    styled_button(ui, style, text.into(), None)
}

/// `.btn.btn-danger`: red, white semibold text, darker red on hover.
pub fn btn_danger(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> Response {
    let style = danger_style(ui);
    styled_button(ui, style, text.into(), None)
}

/// The top bar's record button: `.btn-record` (light-red semibold glyph on
/// the normal fill) while idle, `.btn-stop` (danger look) while recording.
/// `content` is the glyph; `label` its accessible name ("Start recording" /
/// "Stop recording").
pub fn btn_record(
    ui: &mut egui::Ui,
    recording: bool,
    content: impl Into<WidgetText>,
    label: &str,
) -> Response {
    let style = if recording {
        danger_style(ui)
    } else {
        ButtonStyle {
            text: REC_LIGHT,
            hover_text: REC_LIGHT,
            font: Font::Semibold(theme::BASE_SIZE),
            ..btn_style(ui)
        }
    };
    styled_button(ui, style, content.into(), Some(label)).on_hover_text(label)
}

fn icon_style(size: f32, hover_text: Color32) -> ButtonStyle {
    ButtonStyle {
        fill: Color32::TRANSPARENT,
        hover_fill: BG_HOVER,
        stroke: Color32::TRANSPARENT,
        hover_stroke: Color32::TRANSPARENT,
        text: TEXT_MUTED,
        hover_text,
        padding: egui::vec2(4.0, 4.0),
        radius: 6,
        font: Font::Regular(size),
        fixed_size: None,
    }
}

/// Square, padded size for an icon-only button (ignores interact height).
fn icon_size(ui: &egui::Ui, glyph: &str, size: f32) -> egui::Vec2 {
    let galley = ui
        .painter()
        .layout_no_wrap(glyph.to_owned(), theme::regular(size), TEXT);
    let side = galley.size().x.max(size) + 8.0;
    egui::vec2(side, side)
}

/// `.icon-btn`: a bare muted glyph (`size` pt) that gets a hover background.
/// `label` is the tooltip and accessible name.
pub fn icon_btn(ui: &mut egui::Ui, glyph: &str, size: f32, label: &str) -> Response {
    let style = ButtonStyle {
        fixed_size: Some(icon_size(ui, glyph, size)),
        ..icon_style(size, TEXT)
    };
    styled_button(ui, style, glyph.into(), Some(label)).on_hover_text(label)
}

/// `.icon-btn.icon-btn-danger`: like [`icon_btn`] but red on hover.
pub fn icon_btn_danger(ui: &mut egui::Ui, glyph: &str, size: f32, label: &str) -> Response {
    let style = ButtonStyle {
        fixed_size: Some(icon_size(ui, glyph, size)),
        ..icon_style(size, REC)
    };
    styled_button(ui, style, glyph.into(), Some(label)).on_hover_text(label)
}

fn nav_style(selected: bool, padding: egui::Vec2, font: Font) -> ButtonStyle {
    ButtonStyle {
        fill: if selected {
            BG_HOVER
        } else {
            Color32::TRANSPARENT
        },
        hover_fill: BG_HOVER,
        stroke: Color32::TRANSPARENT,
        hover_stroke: Color32::TRANSPARENT,
        text: if selected { TEXT } else { TEXT_MUTED },
        hover_text: if selected { TEXT } else { TEXT_MUTED },
        padding,
        radius: 6,
        font,
        fixed_size: None,
    }
}

/// `.nav-btn` (7/12 padding); `selected` = `.active` (text colour + hover bg).
pub fn nav_btn(ui: &mut egui::Ui, text: impl Into<WidgetText>, selected: bool) -> Response {
    let style = nav_style(
        selected,
        egui::vec2(12.0, 7.0),
        Font::Regular(theme::BASE_SIZE),
    );
    let response = styled_button(ui, style, text.into(), None);
    if selected {
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Button, true, true, "current page")
        });
    }
    response
}

/// `.nav-btn.nav-btn-icon` (7/9 padding) for the Settings gear.
pub fn nav_icon_btn(ui: &mut egui::Ui, glyph: &str, selected: bool, label: &str) -> Response {
    let style = nav_style(selected, egui::vec2(9.0, 7.0), Font::Regular(18.0));
    styled_button(ui, style, glyph.into(), Some(label)).on_hover_text(label)
}

/// `.win-btn` 32×30 (minimize / maximize / close-to-tray). The close button
/// turns red with a white glyph on hover.
pub fn win_btn(ui: &mut egui::Ui, glyph: &str, size: f32, label: &str, close: bool) -> Response {
    let style = ButtonStyle {
        fill: Color32::TRANSPARENT,
        hover_fill: if close { REC } else { BG_HOVER },
        stroke: Color32::TRANSPARENT,
        hover_stroke: Color32::TRANSPARENT,
        text: TEXT_MUTED,
        hover_text: if close { WHITE } else { TEXT },
        padding: egui::Vec2::ZERO,
        radius: 6,
        font: Font::Regular(size),
        fixed_size: Some(egui::vec2(32.0, 30.0)),
    };
    styled_button(ui, style, glyph.into(), Some(label)).on_hover_text(label)
}

// ---------------------------------------------------------------------------
// Pills, chips, badges

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillKind {
    /// `.pill-idle`: hover fill, muted text.
    Idle,
    /// `.pill-busy`: warn tint, warn text.
    Busy,
    /// `.pill-recording`: red tint, light-red text; clickable.
    Recording,
}

impl PillKind {
    pub fn colors(self) -> (Color32, Color32) {
        match self {
            PillKind::Idle => (BG_HOVER, TEXT_MUTED),
            PillKind::Busy => (PILL_BUSY_BG, WARN),
            PillKind::Recording => (PILL_REC_BG, REC_LIGHT),
        }
    }
}

/// The `.pill` frame (fully rounded, 5/9 padding narrow, 5/12 wide), for
/// pills with custom content such as the recording pill.
pub fn pill_frame(ui: &egui::Ui, kind: PillKind) -> Frame {
    let (fill, _) = kind.colors();
    let h = if narrow(ui) { 9 } else { 12 };
    Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(h, 5))
}

/// A text pill (13 pt). Recording pills sense clicks; the others only hover.
pub fn pill(ui: &mut egui::Ui, kind: PillKind, text: impl Into<WidgetText>) -> Response {
    let (_, color) = kind.colors();
    let text: WidgetText = text.into();
    let galley = text.into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        theme::regular(13.0),
    );
    let frame = pill_frame(ui, kind);
    let response = frame
        .show(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(galley.size(), Sense::hover());
            ui.painter().galley(rect.min, galley.clone(), color);
        })
        .response;
    let sense = if kind == PillKind::Recording {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(response.rect, response.id.with("pill"), sense);
    let name = galley.text().to_owned();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &name));
    response
}

/// A speaker chip: fully rounded `color` fill, white semibold 11 pt text,
/// truncated with an ellipsis at `max_width_em` em (CSS `max-width: 16em`).
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32, max_width_em: f32) -> Response {
    let font = theme::semibold(11.0);
    let max_text = (max_width_em * 11.0 - 16.0).max(11.0);
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, WHITE);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_text);
    let galley = ui.painter().layout_job(job);
    let size = galley.size() + egui::vec2(16.0, 4.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(255), color);
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, WHITE);
    }
    let name = text.to_owned();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &name));
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    /// `.badge-transcribed`: green tint, light-green text.
    Ok,
    /// `.badge-processing`: warn tint, warn text.
    Busy,
    /// `.badge-failed`: red tint, light-red text.
    Failed,
}

/// An uppercase 11 pt badge (3/8 padding, 0.4 px letter spacing).
pub fn badge(ui: &mut egui::Ui, kind: BadgeKind, text: &str) -> Response {
    let (fill, color) = match kind {
        BadgeKind::Ok => (BADGE_OK_BG, OK_LIGHT),
        BadgeKind::Busy => (BADGE_WARN_BG, WARN),
        BadgeKind::Failed => (BADGE_REC_BG, REC_LIGHT),
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        egui::TextFormat {
            font_id: theme::regular(11.0),
            color,
            extra_letter_spacing: 0.4,
            ..Default::default()
        },
    );
    let galley = ui.painter().layout_job(job);
    let size = galley.size() + egui::vec2(16.0, 6.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(255), fill);
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    let name = text.to_owned();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &name));
    response
}

// ---------------------------------------------------------------------------
// Boxes and layout

/// `.inline-warning`: warn text on a warn tint, selectable (role=alert).
pub fn inline_warning(ui: &mut egui::Ui, text: &str) -> Response {
    Frame::new()
        .fill(WARN_BG)
        .stroke(Stroke::new(1.0, WARN_BORDER))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(egui::RichText::new(text).color(WARN))
                    .wrap()
                    .selectable(true),
            );
        })
        .response
}

/// Cards and rows (`.meeting-row`, `.person-card`, settings sections…):
/// raised fill, 1 px border, radius 8.
pub fn card_frame() -> Frame {
    Frame::new()
        .fill(BG_RAISED)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(14, 12))
}

/// `.view-error`: a red-tinted box (max 720, centred) with `message` and a
/// row of buttons; returns the index of the button clicked this frame.
pub fn view_error(ui: &mut egui::Ui, message: &str, buttons: &[&str]) -> Option<usize> {
    let mut clicked = None;
    ui.add_space(24.0);
    centered_column(ui, 720.0, |ui| {
        Frame::new()
            .fill(VIEW_ERROR_BG)
            .stroke(Stroke::new(1.0, VIEW_ERROR_BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(18))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(egui::Label::new(message).wrap().selectable(true));
                if !buttons.is_empty() {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        for (index, label) in buttons.iter().enumerate() {
                            if btn(ui, *label).clicked() {
                                clicked = Some(index);
                            }
                        }
                    });
                }
            });
    });
    ui.add_space(24.0);
    clicked
}

/// `.empty`: centred, 60 pt vertical padding; an optional muted second line.
pub fn empty_state(ui: &mut egui::Ui, title: &str, muted: Option<&str>) {
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        if !title.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new(title).color(TEXT)).wrap());
        }
        if let Some(muted) = muted {
            ui.add(egui::Label::new(egui::RichText::new(muted).color(TEXT_MUTED)).wrap());
        }
    });
    ui.add_space(60.0);
}

/// A column of at most `max_width` centred in the available width (the
/// CSS `max-width: 900px; margin: 0 auto` convention).
pub fn centered_column<R>(
    ui: &mut egui::Ui,
    max_width: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let available = ui.available_width();
    let width = available.min(max_width);
    let inset = ((available - width) / 2.0).max(0.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(inset);
        ui.vertical(|ui| {
            ui.set_width(width);
            add(ui)
        })
        .inner
    })
    .inner
}

/// Options for [`keyboard_scroll`].
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyScroll {
    /// Map Space / Shift+Space to page down / up (not in the transcript,
    /// where the player owns Space).
    pub space: bool,
}

/// Keyboard scrolling for a view's main `ScrollArea` when nothing has focus
/// (WebView2 scrolled the focused `<main>`): ↑/↓ 40 pt, PageUp/PageDown 90 %
/// of the viewport, Home/End, optionally Space/Shift+Space. Call once,
/// inside the `ScrollArea`.
pub fn keyboard_scroll(ui: &mut egui::Ui, opts: KeyScroll) {
    if ui.ctx().memory(|m| m.focused().is_some()) {
        return;
    }
    let page = ui.clip_rect().height() * 0.9;
    let delta = ui.input_mut(|i| {
        use egui::{Key, Modifiers};
        let mut delta = 0.0;
        if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
            delta -= 40.0;
        }
        if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
            delta += 40.0;
        }
        if i.consume_key(Modifiers::NONE, Key::PageDown) {
            delta -= page;
        }
        if i.consume_key(Modifiers::NONE, Key::PageUp) {
            delta += page;
        }
        if i.consume_key(Modifiers::NONE, Key::End) {
            delta -= 1.0e7;
        }
        if i.consume_key(Modifiers::NONE, Key::Home) {
            delta += 1.0e7;
        }
        if opts.space {
            if i.consume_key(Modifiers::SHIFT, Key::Space) {
                delta += page;
            }
            if i.consume_key(Modifiers::NONE, Key::Space) {
                delta -= page;
            }
        }
        delta
    });
    if delta != 0.0 {
        // Positive y moves the content down, i.e. scrolls up.
        ui.scroll_with_delta(egui::vec2(0.0, delta));
    }
}

/// Right-click menu for a `TextEdit` (Chromium gave every input one):
/// Cut / Copy / Paste / Select all. Pass the TextEdit's response.
pub fn text_edit_menu(response: &Response) {
    let id = response.id;
    response.context_menu(|ui| {
        let ctx = ui.ctx().clone();
        // The command reaches the focused widget next frame, so give focus
        // back to the field first.
        let run = |ui: &mut egui::Ui, label: &str, command: Option<egui::ViewportCommand>| {
            if ui.button(label).clicked() {
                ctx.memory_mut(|m| m.request_focus(id));
                match command {
                    Some(command) => ctx.send_viewport_cmd(command),
                    None => {
                        if let Some(mut state) = egui::TextEdit::load_state(&ctx, id) {
                            state
                                .cursor
                                .set_char_range(Some(egui::text::CCursorRange::two(
                                    egui::text::CCursor::new(0),
                                    egui::text::CCursor::new(usize::MAX),
                                )));
                            state.store(&ctx, id);
                        }
                    }
                }
                ui.close();
            }
        };
        run(ui, "Cut", Some(egui::ViewportCommand::RequestCut));
        run(ui, "Copy", Some(egui::ViewportCommand::RequestCopy));
        run(ui, "Paste", Some(egui::ViewportCommand::RequestPaste));
        ui.separator();
        run(ui, "Select all", None);
    });
}

// ---------------------------------------------------------------------------
// Form controls

/// Checkbox / radio visuals (CSS `accent-color: var(--accent)`): an empty
/// box with a visible muted outline (accent on hover), or an accent-filled
/// box with a white mark when checked. The label keeps the text colour.
fn toggle_visuals(style: &mut egui::Style, checked: bool) {
    style.visuals.override_text_color = Some(TEXT);
    let w = &mut style.visuals.widgets;
    for (state, hover) in [
        (&mut w.inactive, false),
        (&mut w.hovered, true),
        (&mut w.active, true),
    ] {
        if checked {
            state.bg_fill = theme::ACCENT;
            state.bg_stroke = Stroke::new(1.0, theme::ACCENT);
            state.fg_stroke = Stroke::new(2.0, WHITE);
        } else {
            state.bg_fill = theme::BG;
            state.bg_stroke = Stroke::new(1.0, if hover { theme::ACCENT } else { TEXT_MUTED });
        }
    }
}

/// `<input type=checkbox>` with the app's accent look; disabled when
/// `enabled` is false (the value is kept).
pub fn checkbox(
    ui: &mut egui::Ui,
    enabled: bool,
    checked: &mut bool,
    text: impl Into<WidgetText>,
) -> Response {
    let on = *checked;
    ui.scope(|ui| {
        toggle_visuals(ui.style_mut(), on);
        ui.add_enabled(enabled, egui::Checkbox::new(checked, text))
    })
    .inner
}

/// `<input type=radio>` with the app's accent look: selects `alternative`.
pub fn radio_value<T: PartialEq>(
    ui: &mut egui::Ui,
    current: &mut T,
    alternative: T,
    text: impl Into<WidgetText>,
) -> Response {
    let on = *current == alternative;
    ui.scope(|ui| {
        toggle_visuals(ui.style_mut(), on);
        ui.radio_value(current, alternative, text)
    })
    .inner
}

/// Range-input visuals: a visible `--border` rail, an accent fill up to the
/// value, and an accent handle (a ring at rest, since egui paints the
/// resting handle in the rail colour; filled on hover / drag).
fn slider_visuals(style: &mut egui::Style) {
    style.visuals.slider_trailing_fill = true;
    style.visuals.selection.bg_fill = theme::ACCENT;
    let w = &mut style.visuals.widgets;
    w.inactive.bg_fill = BORDER;
    w.inactive.fg_stroke = Stroke::new(2.0, theme::ACCENT);
    for state in [&mut w.hovered, &mut w.active] {
        state.bg_fill = theme::ACCENT;
        state.fg_stroke = Stroke::new(2.0, theme::FOCUS);
    }
}

/// `<input type=range>` (`accent-color: var(--accent)`).
pub fn slider(ui: &mut egui::Ui, slider: egui::Slider<'_>) -> Response {
    ui.scope(|ui| {
        slider_visuals(ui.style_mut());
        ui.add(slider)
    })
    .inner
}

/// `.text-input` padding (6 px / 10 px).
pub const TEXT_INPUT_MARGIN: Margin = Margin::symmetric(10, 6);

/// `.text-input`: `--bg` fill, 1 px `--border` that turns accent on focus,
/// radius 6, 6/10 padding. Gives `edit` its `id` (needed to know focus).
pub fn text_input<'t>(ui: &egui::Ui, id: egui::Id, edit: egui::TextEdit<'t>) -> egui::TextEdit<'t> {
    let focused = ui.memory(|m| m.has_focus(id));
    let border = if focused { theme::ACCENT } else { BORDER };
    edit.id(id).margin(TEXT_INPUT_MARGIN).frame(
        Frame::new()
            .fill(theme::BG)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(TEXT_INPUT_MARGIN),
    )
}

/// CSS `border-left: Npx solid` on a card with rounded corners: the band
/// follows the card's left corner arcs instead of sticking out past them.
pub fn paint_left_edge(ui: &egui::Ui, card: egui::Rect, width: f32, radius: u8, color: Color32) {
    let band = egui::Rect::from_min_max(
        card.left_top(),
        egui::pos2(card.left() + width, card.bottom()),
    );
    ui.painter()
        .with_clip_rect(band.intersect(ui.clip_rect()))
        .rect_filled(card, CornerRadius::same(radius), color);
}

// ---------------------------------------------------------------------------
// Focus visibility

fn focus_visible_id() -> egui::Id {
    egui::Id::new("witness-focus-visible")
}

/// CSS `:focus-visible` from this pass's input: a key press turns it on, a
/// pointer press turns it off; `None` = no change.
fn focus_visible_change(events: &[egui::Event]) -> Option<bool> {
    events.iter().rev().find_map(|event| match event {
        egui::Event::Key { pressed: true, .. } => Some(true),
        egui::Event::PointerButton { pressed: true, .. } => Some(false),
        _ => None,
    })
}

/// Track whether focus should be drawn (Chromium's `:focus-visible`
/// heuristic): after keyboard input yes, after a click no, and programmatic
/// focus (the confirm modal's Cancel) follows the last input. Call once per
/// pass, before any widget.
pub fn track_focus_visible(ctx: &egui::Context) {
    if let Some(visible) = ctx.input(|i| focus_visible_change(&i.events)) {
        ctx.data_mut(|d| d.insert_temp(focus_visible_id(), visible));
    }
}

/// Whether a focused widget should show its focus ring now.
pub fn focus_visible(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(focus_visible_id()))
        .unwrap_or(false)
}

/// How urgently a live region is announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Politeness {
    Polite,
    Assertive,
}

/// Expose `text` as an AccessKit live region under `id` (screen readers
/// announce changes): toasts, the status pill, the newest live caption,
/// the notes save indicator. A no-op while no screen reader is active.
pub fn announce(ctx: &egui::Context, id: egui::Id, text: &str, politeness: Politeness) {
    use egui::accesskit::{Live, Role};
    ctx.accesskit_node_builder(id, |node| {
        node.set_role(Role::Status);
        node.set_live(match politeness {
            Politeness::Polite => Live::Polite,
            Politeness::Assertive => Live::Assertive,
        });
        node.set_label(text.to_owned());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: impl FnMut(&mut egui::Ui)) {
        let ctx = theme::test_context();
        let mut f = f;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| f(ui));
        });
        // No renderer here to upload the font atlas.
        output.textures_delta.clear();
    }

    #[test]
    fn widgets_lay_out_without_panicking() {
        run(|ui| {
            btn(ui, "Save");
            btn_ghost(ui, "Later");
            btn_danger(ui, "Delete");
            btn_record(ui, false, "●", "Start recording");
            btn_record(ui, true, "■", "Stop recording");
            icon_btn(ui, "x", 16.0, "Rename");
            icon_btn_danger(ui, "x", 16.0, "Delete");
            nav_btn(ui, "Meetings", true);
            nav_icon_btn(ui, "x", false, "Settings");
            win_btn(ui, "x", 14.0, "Close to tray", true);
            pill(ui, PillKind::Idle, "Idle");
            pill(ui, PillKind::Recording, "0:12");
            chip(
                ui,
                "A very long speaker name that will be truncated",
                Color32::RED,
                16.0,
            );
            badge(ui, BadgeKind::Ok, "Transcribed");
            inline_warning(ui, "Microphone device was lost during recording");
            card_frame().show(ui, |ui| ui.label("card"));
            assert_eq!(view_error(ui, "Could not load", &["Try again"]), None);
            empty_state(
                ui,
                "No meetings yet.",
                Some("Join a meeting or hit record."),
            );
            centered_column(ui, 900.0, |ui| ui.label("column"));
            keyboard_scroll(ui, KeyScroll { space: true });
            let edit = ui.text_edit_singleline(&mut String::new());
            text_edit_menu(&edit);
            announce(ui.ctx(), egui::Id::new("a"), "Idle", Politeness::Polite);
        });
    }

    #[test]
    fn chip_truncates_at_its_max_width() {
        run(|ui| {
            let short = chip(ui, "S1", Color32::RED, 16.0);
            let long = chip(ui, &"long ".repeat(40), Color32::RED, 16.0);
            assert!(short.rect.width() < long.rect.width());
            assert!(long.rect.width() <= 16.0 * 11.0 + 0.5);
        });
    }

    #[test]
    fn focus_is_visible_after_keys_and_hidden_after_clicks() {
        let key = egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let click = egui::Event::PointerButton {
            pos: egui::Pos2::ZERO,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        };
        let release = egui::Event::PointerButton {
            pos: egui::Pos2::ZERO,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert_eq!(focus_visible_change(&[]), None);
        assert_eq!(focus_visible_change(std::slice::from_ref(&release)), None);
        assert_eq!(focus_visible_change(std::slice::from_ref(&key)), Some(true));
        assert_eq!(
            focus_visible_change(&[key.clone(), click.clone()]),
            Some(false)
        );
        assert_eq!(focus_visible_change(&[click, release, key]), Some(true));

        let ctx = theme::test_context();
        assert!(!focus_visible(&ctx), "hidden until a key is pressed");
    }

    #[test]
    fn form_controls_are_visible_on_raised_cards() {
        let mut style = egui::Style::default();
        theme::style(&mut style);
        toggle_visuals(&mut style, false);
        let idle = style.visuals.widgets.inactive;
        assert_eq!(idle.bg_stroke.color, TEXT_MUTED, "unchecked outline");
        toggle_visuals(&mut style, true);
        let on = style.visuals.widgets.inactive;
        assert_eq!((on.bg_fill, on.fg_stroke.color), (theme::ACCENT, WHITE));
        assert_eq!(style.visuals.override_text_color, Some(TEXT));

        let mut style = egui::Style::default();
        theme::style(&mut style);
        slider_visuals(&mut style);
        assert!(style.visuals.slider_trailing_fill);
        assert_eq!(style.visuals.selection.bg_fill, theme::ACCENT);
        assert_eq!(style.visuals.widgets.inactive.bg_fill, BORDER, "rail");
    }

    #[test]
    fn form_controls_lay_out() {
        run(|ui| {
            let mut on = true;
            assert!(checkbox(ui, true, &mut on, "Auto-record").rect.height() > 0.0);
            checkbox(ui, false, &mut on, "Disabled");
            let mut pick = 1;
            radio_value(ui, &mut pick, 1, "One");
            radio_value(ui, &mut pick, 2, "Two");
            let mut value = 0.6_f32;
            slider(
                ui,
                egui::Slider::new(&mut value, 0.3..=0.9).show_value(false),
            );
            let mut text = String::from("teams");
            let id = egui::Id::new("text-input");
            let edit = text_input(ui, id, egui::TextEdit::singleline(&mut text));
            ui.add(edit);
            let card = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(100.0, 40.0));
            paint_left_edge(ui, card, 3.0, 8, theme::ACCENT);
        });
    }
}
