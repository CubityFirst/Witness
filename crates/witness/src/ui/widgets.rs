//! The shared widget kit: the CSS button family (`.btn`, `.btn-ghost`,
//! `.btn-danger`, `.btn-record` / `.btn-stop`, `.icon-btn`, `.nav-btn`,
//! `.win-btn`), pills, chips, badges, warning/error boxes, layout helpers,
//! keyboard scrolling, the text-field context menu and AccessKit live
//! regions. Views use these rather than fighting the global `Visuals`
//! (map-ui-shell §9) — and do not fork them.

use super::theme::{self, narrow, pal};
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
    /// Content height for icon-only buttons: the SVG's size (+ the CSS
    /// border), not the glyph galley's text row height (Segoe's 1.33x),
    /// and no interact-size minimum. None = the galley's height.
    content_height: Option<f32>,
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
        fill: pal().bg_hover,
        hover_fill: pal().bg_active,
        stroke: pal().border_strong,
        hover_stroke: pal().border_strong,
        text: pal().text,
        hover_text: pal().text,
        padding: btn_padding(ui),
        radius: theme::R_MD,
        font: Font::Regular(theme::BASE_SIZE),
        fixed_size: None,
        content_height: None,
    }
}

fn danger_style(ui: &egui::Ui) -> ButtonStyle {
    ButtonStyle {
        fill: pal().rec,
        hover_fill: pal().rec_hover,
        stroke: pal().rec,
        hover_stroke: pal().rec_light,
        text: pal().white,
        hover_text: pal().white,
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
        match style.content_height {
            Some(h) => egui::vec2(s.x, h + style.padding.y * 2.0),
            None => egui::vec2(s.x, s.y.max(ui.spacing().interact_size.y)),
        }
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
                Stroke::new(2.0, theme::pal().focus),
                egui::StrokeKind::Outside,
            );
        }
    }
    if enabled {
        response
    } else {
        response.on_hover_cursor(egui::CursorIcon::NotAllowed)
    }
}

/// `.btn`: raised fill, 1 px border that turns accent on hover.
pub fn btn(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> Response {
    let style = btn_style(ui);
    styled_button(ui, style, text.into(), None)
}

/// Ghost button: text only, a hover fill, no border.
pub fn btn_ghost(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> Response {
    let style = ButtonStyle {
        fill: Color32::TRANSPARENT,
        hover_fill: pal().bg_hover,
        stroke: Color32::TRANSPARENT,
        hover_stroke: Color32::TRANSPARENT,
        ..btn_style(ui)
    };
    styled_button(ui, style, text.into(), None)
}

/// Make the gap between the last widget and the next one `gap` pt: a CSS
/// margin is the whole gap, while `add_space` adds to `item_spacing`.
/// Only between two widgets of the same `ui` (a trailing gap at the end of
/// a child ui gets the parent's spacing on top again).
pub fn gap(ui: &mut egui::Ui, gap: f32) {
    let spacing = ui.spacing().item_spacing.y;
    ui.add_space(gap - spacing);
}

/// `.btn.btn-danger`: red, white semibold text, darker red on hover.
pub fn btn_danger(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> Response {
    let style = danger_style(ui);
    styled_button(ui, style, text.into(), None)
}

fn icon_style(size: f32, hover_text: Color32) -> ButtonStyle {
    ButtonStyle {
        fill: Color32::TRANSPARENT,
        hover_fill: pal().bg_hover,
        stroke: Color32::TRANSPARENT,
        hover_stroke: Color32::TRANSPARENT,
        text: pal().text_muted,
        hover_text,
        padding: egui::vec2(4.0, 4.0),
        radius: theme::R_MD,
        font: Font::Regular(size),
        fixed_size: None,
        content_height: None,
    }
}

/// Square, padded size for an icon-only button (ignores interact height).
fn icon_size(ui: &egui::Ui, glyph: &str, size: f32) -> egui::Vec2 {
    let galley = ui
        .painter()
        .layout_no_wrap(glyph.to_owned(), theme::regular(size), pal().text);
    let side = (galley.size().x.max(size) + 12.0).max(28.0);
    egui::vec2(side, side)
}

/// `.icon-btn`: a bare muted glyph (`size` pt) that gets a hover background.
/// `label` is the tooltip and accessible name.
pub fn icon_btn(ui: &mut egui::Ui, glyph: &str, size: f32, label: &str) -> Response {
    let style = ButtonStyle {
        fixed_size: Some(icon_size(ui, glyph, size)),
        ..icon_style(size, pal().text)
    };
    styled_button(ui, style, glyph.into(), Some(label)).on_hover_text(label)
}

/// `.icon-btn.icon-btn-danger`: like [`icon_btn`] but red on hover.
pub fn icon_btn_danger(ui: &mut egui::Ui, glyph: &str, size: f32, label: &str) -> Response {
    let style = ButtonStyle {
        fixed_size: Some(icon_size(ui, glyph, size)),
        ..icon_style(size, pal().rec)
    };
    styled_button(ui, style, glyph.into(), Some(label)).on_hover_text(label)
}

/// `.win-btn` 32×30 (minimize / maximize / close-to-tray). The close button
/// turns red with a white glyph on hover.
pub fn win_btn(ui: &mut egui::Ui, glyph: &str, size: f32, label: &str, close: bool) -> Response {
    let style = ButtonStyle {
        fill: Color32::TRANSPARENT,
        hover_fill: if close { pal().rec } else { pal().bg_hover },
        stroke: Color32::TRANSPARENT,
        hover_stroke: Color32::TRANSPARENT,
        text: pal().text_muted,
        hover_text: if close { pal().white } else { pal().text },
        padding: egui::Vec2::ZERO,
        radius: 6,
        font: Font::Regular(size),
        fixed_size: Some(egui::vec2(32.0, 30.0)),
        content_height: None,
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
            PillKind::Idle => (pal().bg_hover, pal().text_muted),
            PillKind::Busy => (pal().pill_busy_bg, pal().warn),
            PillKind::Recording => (pal().pill_rec_bg, pal().rec_light),
        }
    }
}

/// The `.pill` padding: 5/9 narrow, 5/12 wide.
pub fn pill_padding(ui: &egui::Ui) -> egui::Vec2 {
    egui::vec2(if narrow(ui) { 9.0 } else { 12.0 }, 5.0)
}

/// Allocate a pill around `content` (padding added) and paint its fully
/// rounded fill. Sized exactly and never stretched to the row's height (a
/// `Frame` in a centred row would be), so it hugs its 13 pt text like the
/// CSS inline-flex pill. Returns the pill's rect.
pub fn pill_shell(ui: &mut egui::Ui, kind: PillKind, content: egui::Vec2) -> egui::Rect {
    let (fill, _) = kind.colors();
    let size = content + pill_padding(ui) * 2.0;
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(255), fill);
    }
    rect
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
    let rect = pill_shell(ui, kind, galley.size());
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley.clone(), color);
    let sense = if kind == PillKind::Recording {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, ui.id().with(("pill", kind as u8)), sense);
    let name = galley.text().to_owned();
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
        BadgeKind::Ok => (pal().badge_ok_bg, pal().ok_light),
        BadgeKind::Busy => (pal().badge_warn_bg, pal().warn),
        BadgeKind::Failed => (pal().badge_rec_bg, pal().rec_light),
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
        .fill(pal().warn_bg)
        .stroke(Stroke::new(1.0, pal().warn_border))
        .corner_radius(CornerRadius::same(theme::R_MD))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(egui::RichText::new(text).color(pal().warn))
                    .wrap()
                    .selectable(true),
            );
        })
        .response
}

/// `.view-error`: a red-tinted box (max 720, centred) with `message` and a
/// row of buttons; returns the index of the button clicked this frame.
pub fn view_error(ui: &mut egui::Ui, message: &str, buttons: &[&str]) -> Option<usize> {
    let mut clicked = None;
    ui.add_space(24.0);
    centered_column(ui, 720.0, |ui| {
        Frame::new()
            .fill(pal().view_error_bg)
            .stroke(Stroke::new(1.0, pal().view_error_border))
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
    let spacing_x = ui.spacing().item_spacing.x;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(inset);
        ui.vertical(|ui| {
            // The inset needed no gap; the content gets the normal one back.
            ui.spacing_mut().item_spacing.x = spacing_x;
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

fn view_viewport_id() -> egui::Id {
    egui::Id::new("witness-view-viewport-right")
}

/// Forget the noted viewport (before a view draws; it notes it again).
pub fn clear_view_viewport(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<f32>(view_viewport_id()));
}

/// Keyboard scrolling for a view's main `ScrollArea` when nothing has focus
/// (WebView2 scrolled the focused `<main>`): ↑/↓ 40 pt, PageUp/PageDown 90 %
/// of the viewport, Home/End, optionally Space/Shift+Space. Call once,
/// inside the `ScrollArea`. Also notes the viewport's right edge for
/// [`view_scroll_gutter`].
pub fn keyboard_scroll(ui: &mut egui::Ui, opts: KeyScroll) {
    let viewport = ui.max_rect().right();
    ui.ctx()
        .data_mut(|d| d.insert_temp(view_viewport_id(), viewport));
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

/// Range-input visuals: a visible `--border` rail and an accent fill up to
/// the value ([`slider`] paints the solid accent thumb itself, since egui
/// paints the resting handle in the rail colour).
fn slider_visuals(style: &mut egui::Style) {
    style.visuals.slider_trailing_fill = true;
    style.visuals.selection.bg_fill = theme::pal().accent;
    let w = &mut style.visuals.widgets;
    w.inactive.bg_fill = pal().border;
    w.inactive.fg_stroke = Stroke::new(2.0, theme::pal().accent);
    for state in [&mut w.hovered, &mut w.active] {
        state.bg_fill = theme::pal().accent;
        state.fg_stroke = Stroke::new(2.0, theme::pal().focus);
    }
}

/// `<input type=range>` (`accent-color: var(--accent)`): a 4 pt rail
/// (`--border`, accent up to the value) and a round, solid accent thumb
/// ~15 pt across, like Chromium's; `width` overrides the default length.
pub fn slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f64,
    width: Option<f32>,
) -> Response {
    let (lo, hi) = (*range.start(), *range.end());
    ui.scope(|ui| {
        slider_visuals(ui.style_mut());
        let spacing = &mut ui.style_mut().spacing;
        spacing.slider_rail_height = SLIDER_RAIL;
        // The slider is then as tall as a line of body text, and egui's
        // handle radius (height / 2.5) about 7.5 pt.
        spacing.interact_size.y = 0.0;
        if let Some(width) = width {
            spacing.slider_width = width;
        }
        let response = ui.add(
            egui::Slider::new(value, range)
                .step_by(step)
                .show_value(false)
                .handle_shape(egui::style::HandleShape::Circle),
        );
        if ui.is_rect_visible(response.rect) {
            let (center, radius) = slider_thumb(response.rect, *value, lo, hi);
            let painter = ui.painter();
            painter.circle_filled(center, radius, theme::pal().accent);
            if response.hovered() || response.dragged() {
                painter.circle_stroke(center, radius, Stroke::new(2.0, theme::pal().focus));
            }
        }
        response
    })
    .inner
}

/// The range input's rail thickness.
const SLIDER_RAIL: f32 = 4.0;

/// Where egui draws a horizontal slider's handle in `rect` for `value` in
/// `lo..=hi`: (centre, radius).
fn slider_thumb(rect: egui::Rect, value: f32, lo: f32, hi: f32) -> (egui::Pos2, f32) {
    let radius = rect.height() / 2.5;
    let t = if hi > lo {
        ((value - lo) / (hi - lo)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (left, right) = (rect.left() + radius, rect.right() - radius);
    (
        egui::pos2(left + (right - left) * t, rect.center().y),
        radius,
    )
}

/// Which CSS `<select>` a [`select`] imitates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectKind {
    /// `select.text-input`: `--bg` fill, 1 px border, 6/10 padding.
    TextInput,
    /// `select.player-speed`: the same look at 4/6 padding for 12 px text
    /// (about 24 pt tall).
    Compact,
}

/// The button of a select: `--bg` fill and a 1 px `--border` border that
/// turns accent while focused or open (not on hover) — an input, not a
/// raised `.btn`. Only the button: popups draw with the context's style.
pub fn select_style(style: &mut egui::Style, kind: SelectKind) {
    let w = &mut style.visuals.widgets;
    for (state, border) in [
        (&mut w.inactive, pal().border_strong),
        (&mut w.hovered, pal().border_strong),
        (&mut w.active, theme::pal().accent),
        (&mut w.open, theme::pal().accent),
    ] {
        state.weak_bg_fill = pal().bg_raised;
        state.bg_stroke = Stroke::new(1.0, border);
        state.corner_radius = CornerRadius::same(theme::R_MD);
    }
    match kind {
        SelectKind::TextInput => style.spacing.button_padding = egui::vec2(10.0, 6.0),
        SelectKind::Compact => {
            style.spacing.button_padding = egui::vec2(6.0, 4.0);
            style.spacing.interact_size.y = 0.0;
        }
    }
}

/// Run `add` (an `egui::ComboBox`) with [`select_style`] applied.
pub fn select<R>(ui: &mut egui::Ui, kind: SelectKind, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.scope(|ui| {
        select_style(ui.style_mut(), kind);
        add(ui)
    })
    .inner
}

/// `.text-input` padding (6 px / 10 px).
pub const TEXT_INPUT_MARGIN: Margin = Margin::symmetric(10, 6);

/// `.text-input`: `--bg` fill, 1 px `--border` that turns accent on focus,
/// radius 6, 6/10 padding. Gives `edit` its `id` (needed to know focus).
pub fn text_input<'t>(ui: &egui::Ui, id: egui::Id, edit: egui::TextEdit<'t>) -> egui::TextEdit<'t> {
    framed_text_edit(
        ui,
        id,
        edit,
        pal().bg_raised,
        theme::R_MD,
        TEXT_INPUT_MARGIN,
    )
}

/// A text field with its own frame: `fill`, a 1 px `--border` that turns
/// accent only while focused (no hover change, no FOCUS-blue selection
/// stroke), `radius`, `margin` padding. Gives `edit` its `id`.
pub fn framed_text_edit<'t>(
    ui: &egui::Ui,
    id: egui::Id,
    edit: egui::TextEdit<'t>,
    fill: Color32,
    radius: u8,
    margin: Margin,
) -> egui::TextEdit<'t> {
    let focused = ui.memory(|m| m.has_focus(id));
    let border = if focused {
        theme::pal().accent
    } else {
        pal().border_strong
    };
    edit.id(id).margin(margin).frame(
        Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(CornerRadius::same(radius))
            .inner_margin(margin),
    )
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

// ---------------------------------------------------------------------------
// Rework kit (design/ui-rework.md): flat rows, switches, setting rows,
// groups, eliding labels. Views build on these rather than on cards.

/// Below [`theme::COMPACT_BELOW`] of available width: stacked / icon-only
/// variants.
pub fn is_compact(ui: &egui::Ui) -> bool {
    ui.available_width() < theme::COMPACT_BELOW
}

/// Primary button: accent fill, white semibold text.
pub fn btn_primary(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> Response {
    let style = ButtonStyle {
        fill: theme::pal().accent,
        hover_fill: theme::pal().accent_hover,
        stroke: Color32::TRANSPARENT,
        hover_stroke: Color32::TRANSPARENT,
        text: pal().on_accent,
        hover_text: pal().on_accent,
        font: Font::Semibold(theme::BASE_SIZE),
        ..btn_style(ui)
    };
    styled_button(ui, style, text.into(), None)
}

/// The switch track size.
const SWITCH: egui::Vec2 = egui::vec2(36.0, 20.0);

/// A Windows-11-style toggle switch (no label): accent track when on, a
/// hairline track when off; the knob slides. Disabled when `enabled` is
/// false (the value is kept). `label` is the accessible name.
pub fn switch(ui: &mut egui::Ui, enabled: bool, on: &mut bool, label: &str) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(SWITCH, Sense::click());
    if enabled && response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let value = *on;
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, value, label)
    });
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, value);
        let hovered = enabled && response.hovered();
        let painter = ui.painter();
        let radius = CornerRadius::same(255);
        let track = if value {
            if hovered {
                theme::pal().accent_hover
            } else {
                theme::pal().accent
            }
        } else if hovered {
            pal().bg_active
        } else {
            pal().bg_hover
        };
        let stroke = if value {
            Stroke::NONE
        } else {
            Stroke::new(
                1.0,
                if hovered {
                    pal().text_muted
                } else {
                    pal().border_strong
                },
            )
        };
        painter.rect(rect, radius, track, stroke, egui::StrokeKind::Inside);
        let knob_r = if value { 7.0 } else { 6.0 };
        let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
        let knob = if value { pal().white } else { pal().text_muted };
        painter.circle_filled(egui::pos2(x, rect.center().y), knob_r, knob);
        if !enabled {
            painter.rect_filled(rect, radius, theme::alpha(theme::pal().bg, 0.45));
        }
        if response.has_focus() && focus_visible(ui.ctx()) {
            painter.rect_stroke(
                rect.expand(2.0),
                radius,
                Stroke::new(2.0, theme::pal().focus),
                egui::StrokeKind::Outside,
            );
        }
    }
    response
}

/// A grouped surface whose rows are separated by [`group_divider`]: one
/// raised, rounded block (Settings sections, detail panels).
pub fn group<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    Frame::new()
        .fill(pal().bg_raised)
        .stroke(Stroke::new(1.0, pal().border))
        .corner_radius(CornerRadius::same(theme::R_LG))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = theme::S3;
            add(ui)
        })
        .inner
}

/// A hairline between two rows of a [`group`], spanning its content width.
pub fn group_divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0, pal().border),
    );
}

/// A section header above a group or list: 12.5 pt semibold muted.
pub fn section_header(ui: &mut egui::Ui, text: &str) -> Response {
    ui.add_space(theme::S3);
    let r = ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .font(theme::semibold(12.5))
                .color(pal().text_muted),
        )
        .truncate(),
    );
    ui.add_space(theme::S1 - ui.spacing().item_spacing.y + 2.0);
    r
}

/// A list group header ("Today", "Yesterday", "Thu 23 Jul"): 12 pt
/// semibold faint, with breathing room above.
pub fn day_header(ui: &mut egui::Ui, text: &str) {
    ui.add_space(theme::S2);
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .font(theme::semibold(12.0))
                .color(pal().text_faint),
        )
        .truncate(),
    );
}

/// A single-line label that elides with `…` at the available width and
/// shows the whole text as a tooltip when it had to. Use for every title,
/// name or path that can be long.
pub fn elide(ui: &mut egui::Ui, text: &str, font: FontId, color: Color32) -> Response {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(ui.available_width().max(10.0));
    let galley = ui.painter().layout_job(job);
    let elided = galley.elided;
    let response = ui.add(egui::Label::new(galley).selectable(false));
    if elided {
        response.on_hover_text(text)
    } else {
        response
    }
}

/// A full-width clickable list row: radius 8, hover fill, `selected` fill,
/// keyboard focus ring, 8/12 padding. `add` lays out the row content and
/// gets whether the row is hovered (for hover-revealed actions). Returns
/// the row's own response (click = open) and `add`'s result.
pub fn list_row<R>(
    ui: &mut egui::Ui,
    id_salt: impl egui::AsIdSalt,
    selected: bool,
    add: impl FnOnce(&mut egui::Ui, bool) -> R,
) -> (Response, R) {
    let id = ui.id().with(id_salt);
    // The row senses through its scope, which egui registers *before* the
    // content (from the previous pass's rect): buttons inside the row are
    // on top and keep their clicks; everywhere else the row gets them.
    let builder = egui::UiBuilder::new().id_salt(id).sense(Sense::click());
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let scope = ui.scope_builder(builder, |ui| {
        let hovered = ui.response().contains_pointer();
        Frame::new()
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui, hovered)
            })
            .inner
    });
    let response = scope.response;
    let rect = response.rect;
    let hovered = response.contains_pointer();
    if selected {
        ui.painter().set(
            bg_idx,
            egui::epaint::RectShape::filled(rect, CornerRadius::same(theme::R_MD), pal().bg_active),
        );
    } else if hovered {
        paint_hover_pill(ui, rect);
    }
    if response.has_focus() && focus_visible(ui.ctx()) {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(theme::R_MD),
            Stroke::new(2.0, theme::pal().focus),
            egui::StrokeKind::Inside,
        );
    }
    (response, scope.inner)
}

/// The hover mark of a list row: a short neutral pill at its left edge
/// (selected rows fill instead, and an open meeting's pill is the accent).
pub fn paint_hover_pill(ui: &egui::Ui, rect: egui::Rect) {
    let height = (rect.height() * 0.4).clamp(12.0, 18.0);
    let pill = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 4.0, rect.center().y),
        egui::vec2(3.0, height),
    );
    ui.painter()
        .rect_filled(pill, CornerRadius::same(2), pal().text_faint);
}

/// Up to two initials of `name` ("Paul Gordon" → "PG", "me" → "ME").
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = String::new();
    match words.as_slice() {
        [] => {}
        [one] => out.extend(one.chars().take(2)),
        [first, .., last] => {
            out.extend(first.chars().take(1));
            out.extend(last.chars().take(1));
        }
    }
    out.to_uppercase()
}

/// A round avatar with `name`'s initials, tinted by `color`.
pub fn avatar(ui: &mut egui::Ui, name: &str, color: Color32, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let dark = pal().dark;
        let (fill, ink) = if dark {
            (theme::alpha(color, 0.22), theme::lighten(color, 0.45))
        } else {
            (theme::alpha(color, 0.16), theme::darken(color, 0.25))
        };
        painter.circle_filled(rect.center(), size / 2.0, fill);
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initials(name),
            theme::semibold((size * 0.38).max(10.0)),
            ink,
        );
    }
    response
}

/// Horizontal page padding: 16 pt up to 640 pt of width, 24 beyond.
pub fn pad_x(width: f32) -> f32 {
    if width <= 640.0 {
        theme::S4
    } else {
        theme::S5
    }
}

/// [`content_column`] with the responsive side padding: what the shell
/// uses for the banner / search box and views use inside their scroller.
pub fn padded_column<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let pad = pad_x(ui.available_width());
    let spacing_x = ui.spacing().item_spacing.x;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(pad);
        let inner_w = (ui.available_width() - pad).max(0.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.x = spacing_x;
            ui.set_width(inner_w);
            content_column(ui, add)
        })
        .inner
    })
    .inner
}

/// A view's main scroller: full window width (its bar at the window edge),
/// the content in a [`padded_column`], keyboard scrolling when nothing has
/// focus. `id_salt` keeps each view's scroll offset separate.
pub fn scroll_column<R>(
    ui: &mut egui::Ui,
    id_salt: impl egui::AsIdSalt,
    keys: KeyScroll,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            keyboard_scroll(ui, keys);
            let out = padded_column(ui, add);
            ui.add_space(theme::S5);
            out
        })
}

/// A loading placeholder block: a rounded `bg_hover` bar with a soft
/// shimmer sweeping across it (static when Windows animations are off).
pub fn skeleton(ui: &mut egui::Ui, size: egui::Vec2) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let p = pal();
        let radius = CornerRadius::same((size.y / 2.0).min(6.0) as u8);
        ui.painter().rect_filled(rect, radius, p.bg_hover);
        if super::icons::animations_enabled() {
            // A band 40 % of the screen wide travelling every 1.4 s, the
            // same phase for every block so a list shimmers as one.
            let t = ui.input(|i| i.time) as f32 % 1.4 / 1.4;
            let screen = ui.ctx().content_rect();
            let band = screen.width() * 0.4;
            let x = screen.left() - band + (screen.width() + band * 2.0) * t;
            let glow = egui::Rect::from_x_y_ranges(x..=x + band, rect.y_range()).intersect(rect);
            if glow.width() > 0.0 {
                let shine = if p.dark {
                    Color32::from_white_alpha(10)
                } else {
                    Color32::from_white_alpha(140)
                };
                ui.painter()
                    .with_clip_rect(rect)
                    .rect_filled(glow, radius, shine);
            }
            super::icons::request_animation_frame(ui.ctx());
        }
    }
    response
}

/// `rows` list-row placeholders (a title bar and a shorter meta bar each),
/// widths varied so it reads as content, for a view's first load.
pub fn skeleton_rows(ui: &mut egui::Ui, rows: usize) {
    let width = ui.available_width();
    for row in 0..rows {
        let title = width * [0.62, 0.48, 0.71, 0.55, 0.66, 0.44][row % 6];
        let meta = width * [0.28, 0.22, 0.31, 0.25][row % 4];
        Frame::new()
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                skeleton(ui, egui::vec2(title.max(60.0), 12.0));
                skeleton(ui, egui::vec2(meta.max(40.0), 9.0));
            });
    }
}

/// The content column every view lives in: at most
/// [`theme::CONTENT_MAX`] wide, centred.
pub fn content_column<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    centered_column(ui, theme::CONTENT_MAX, add)
}

#[cfg(test)]
mod tests {
    #[test]
    fn initials_of_names() {
        assert_eq!(super::initials("Paul Gordon"), "PG");
        assert_eq!(super::initials("me"), "ME");
        assert_eq!(super::initials("Kate Maguire-Rawlinson"), "KR");
        assert_eq!(super::initials(""), "");
    }

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
            btn_primary(ui, "Download now");
            icon_btn(ui, "x", 16.0, "Rename");
            icon_btn_danger(ui, "x", 16.0, "Delete");
            win_btn(ui, "x", 14.0, "Close to tray", true);
            pill(ui, PillKind::Idle, "Idle");
            pill(ui, PillKind::Recording, "0:12");
            elide(
                ui,
                "A very long meeting title that will be elided",
                theme::semibold(14.0),
                pal().text,
            );
            avatar(ui, "Paul Gordon", theme::chip_color("S2"), 32.0);
            section_header(ui, "Recording");
            day_header(ui, "Today");
            group(ui, |ui| {
                ui.label("row");
                group_divider(ui);
                ui.label("row");
            });
            list_row(ui, "row", false, |ui, _| ui.label("Meeting"));
            badge(ui, BadgeKind::Ok, "Transcribed");
            inline_warning(ui, "Microphone device was lost during recording");
            assert_eq!(view_error(ui, "Could not load", &["Try again"]), None);
            centered_column(ui, 900.0, |ui| ui.label("column"));
            keyboard_scroll(ui, KeyScroll { space: true });
            let edit = ui.text_edit_singleline(&mut String::new());
            text_edit_menu(&edit);
            announce(ui.ctx(), egui::Id::new("a"), "Idle", Politeness::Polite);
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
    fn form_controls_lay_out() {
        run(|ui| {
            let mut on = true;
            assert_eq!(switch(ui, true, &mut on, "Auto-record").rect.size(), SWITCH);
            switch(ui, false, &mut on, "Disabled");
            let mut value = 0.6_f32;
            let response = slider(ui, &mut value, 0.3..=0.9, 0.05, Some(80.0));
            assert_eq!(response.rect.width(), 80.0);
            let mut text = String::from("teams");
            let id = egui::Id::new("text-input");
            let edit = text_input(ui, id, egui::TextEdit::singleline(&mut text));
            ui.add(edit);
        });
    }

    /// Shapes painted by one pass of `f` (fonts uploaded and discarded).
    fn shapes(f: impl FnMut(&mut egui::Ui)) -> Vec<egui::Shape> {
        let ctx = theme::test_context();
        let mut f = f;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| f(ui));
        });
        output.textures_delta.clear();
        output.shapes.into_iter().map(|c| c.shape).collect()
    }

    #[test]
    fn slider_thumb_is_a_round_accent_disc_on_a_thin_rail() {
        let mut thumb = None;
        let shapes = shapes(|ui| {
            let mut value = 0.5_f32;
            let response = slider(ui, &mut value, 0.0..=1.0, 0.05, Some(80.0));
            thumb = Some(slider_thumb(response.rect, value, 0.0, 1.0));
        });
        let (center, radius) = thumb.unwrap();
        assert!((6.5..=8.5).contains(&radius), "radius {radius}");
        let disc = shapes.iter().any(|shape| {
            matches!(shape, egui::Shape::Circle(c)
                if c.fill == theme::pal().accent && c.center == center && c.radius == radius)
        });
        assert!(disc, "no solid accent thumb: {shapes:?}");
        // The rail: 4 pt tall, no 18x24 rectangular handle anywhere.
        let rects: Vec<_> = shapes
            .iter()
            .filter_map(|shape| match shape {
                // Not the panel's own background.
                egui::Shape::Rect(r) if r.rect.width() < 1000.0 => Some(r.rect),
                _ => None,
            })
            .collect();
        assert!(rects
            .iter()
            .any(|r| (r.height() - SLIDER_RAIL).abs() < 0.01));
        assert!(rects.iter().all(|r| r.height() < 10.0), "{rects:?}");
    }

    #[test]
    fn selects_look_like_inputs() {
        let mut heights = Vec::new();
        let shapes = shapes(|ui| {
            for kind in [SelectKind::TextInput, SelectKind::Compact] {
                let response = select(ui, kind, |ui| {
                    egui::ComboBox::from_id_salt(("select", kind as u8))
                        .selected_text(egui::RichText::new("1x").size(12.0))
                        .show_ui(ui, |_| {})
                        .response
                });
                heights.push(response.rect.height());
            }
        });
        let buttons: Vec<_> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Rect(r) if r.stroke.color == pal().border_strong => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(buttons.len(), 2, "{shapes:?}");
        assert!(buttons.iter().all(|r| r.fill == pal().bg_raised));
        // `.player-speed` (12 px, 4/6 padding) is shorter than a text input.
        assert!(heights[1] <= 26.0 && heights[1] < heights[0], "{heights:?}");
    }

    /// CSS margins are whole gaps: `gap` subtracts the item spacing egui
    /// adds after every widget, between siblings and after a child scope.
    /// A button inside a `list_row` keeps its click; the row only gets
    /// clicks that land outside its inner widgets.
    #[test]
    fn list_rows_leave_inner_buttons_their_clicks() {
        let ctx = theme::test_context();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0));
        // (row clicked, button clicked, row rect, button rect)
        let pass = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut out = (false, false, egui::Rect::NOTHING, egui::Rect::NOTHING);
            let mut output = ctx.run_ui(input, |ui| {
                let (row, button) = list_row(ui, "row", false, |ui, _| {
                    ui.horizontal(|ui| {
                        ui.label("Meeting title");
                        btn(ui, "Delete")
                    })
                    .inner
                });
                out = (row.clicked(), button.clicked(), row.rect, button.rect);
            });
            output.textures_delta.clear();
            out
        };
        let click_at = |p: egui::Pos2| {
            vec![
                egui::Event::PointerMoved(p),
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ]
        };
        pass(vec![]);
        let (_, _, row_rect, button_rect) = pass(vec![]);
        let (row, button, ..) = pass(click_at(button_rect.center()));
        assert_eq!((row, button), (false, true));
        let empty = egui::pos2(row_rect.right() - 4.0, row_rect.center().y);
        assert!(!button_rect.contains(empty));
        let (row, button, ..) = pass(click_at(empty));
        assert_eq!((row, button), (true, false));
    }

    #[test]
    fn gaps_are_exact_css_margins() {
        run(|ui| {
            let a = ui.label("a").rect;
            gap(ui, 14.0);
            let b = ui.label("b").rect;
            assert!((b.top() - a.bottom() - 14.0).abs() < 0.01);

            // The shell: a scope holding the search box, then 12 pt.
            let field = ui
                .scope(|ui| centered_column(ui, 900.0, |ui| ui.label("search")))
                .inner
                .rect;
            gap(ui, 12.0);
            let first = ui.label("first row").rect;
            assert!((first.top() - field.bottom() - 12.0).abs() < 0.01);

            // A gap smaller than the item spacing.
            gap(ui, 6.0);
            let next = ui.label("next row").rect;
            assert!((next.top() - first.bottom() - 6.0).abs() < 0.01);
        });
    }
}
