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
        content_height: None,
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

/// `.btn.btn-with-icon` holding only an `icon` pt glyph: as tall as the
/// SVG + padding + the 1 px border (30 pt for a 16 pt icon), like CSS.
pub fn btn_icon_only(ui: &mut egui::Ui, glyph: impl Into<WidgetText>, icon: f32) -> Response {
    let style = ButtonStyle {
        content_height: Some(icon + 2.0),
        ..btn_style(ui)
    };
    styled_button(ui, style, glyph.into(), None)
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

/// `.btn.btn-ghost.btn-with-icon` holding only an `icon` pt glyph: sized
/// like [`btn_icon_only`] (30 pt for a 16 pt icon), not by Segoe's row.
pub fn btn_ghost_icon(ui: &mut egui::Ui, glyph: impl Into<WidgetText>, icon: f32) -> Response {
    let style = ButtonStyle {
        fill: Color32::TRANSPARENT,
        hover_fill: Color32::TRANSPARENT,
        content_height: Some(icon + 2.0),
        ..btn_style(ui)
    };
    styled_button(ui, style, glyph.into(), None)
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

/// The top bar's record button: `.btn-record` (light-red semibold glyph on
/// the normal fill) while idle, `.btn-stop` (danger look) while recording.
/// `content` is the glyph (`icon` pt, sized like [`btn_icon_only`]);
/// `label` its accessible name ("Start recording" / "Stop recording").
pub fn btn_record(
    ui: &mut egui::Ui,
    recording: bool,
    content: impl Into<WidgetText>,
    icon: f32,
    label: &str,
) -> Response {
    let base = if recording {
        danger_style(ui)
    } else {
        ButtonStyle {
            text: REC_LIGHT,
            hover_text: REC_LIGHT,
            font: Font::Semibold(theme::BASE_SIZE),
            ..btn_style(ui)
        }
    };
    let style = ButtonStyle {
        content_height: Some(icon + 2.0),
        ..base
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
        content_height: None,
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
        content_height: None,
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

/// `.nav-btn.nav-btn-icon` (7/9 padding) for the Settings gear: the 18 pt
/// SVG + padding = 32 pt tall (no border).
pub fn nav_icon_btn(ui: &mut egui::Ui, glyph: &str, selected: bool, label: &str) -> Response {
    let style = ButtonStyle {
        content_height: Some(18.0),
        ..nav_style(selected, egui::vec2(9.0, 7.0), Font::Regular(18.0))
    };
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
            PillKind::Idle => (BG_HOVER, TEXT_MUTED),
            PillKind::Busy => (PILL_BUSY_BG, WARN),
            PillKind::Recording => (PILL_REC_BG, REC_LIGHT),
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

/// A chip's padding (8/2 per side).
const CHIP_PADDING: egui::Vec2 = egui::vec2(16.0, 4.0);

fn chip_galley(ui: &egui::Ui, text: &str, max_width_em: f32) -> std::sync::Arc<egui::Galley> {
    let font = theme::semibold(11.0);
    let max_text = (max_width_em * 11.0 - 16.0).max(11.0);
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, WHITE);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_text);
    ui.painter().layout_job(job)
}

/// The size [`chip`] will allocate, for laying out around it first.
pub fn chip_size(ui: &egui::Ui, text: &str, max_width_em: f32) -> egui::Vec2 {
    chip_galley(ui, text, max_width_em).size() + CHIP_PADDING
}

/// A speaker chip: fully rounded `color` fill, white semibold 11 pt text,
/// truncated with an ellipsis at `max_width_em` em (CSS `max-width: 16em`).
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32, max_width_em: f32) -> Response {
    let galley = chip_galley(ui, text, max_width_em);
    let size = galley.size() + CHIP_PADDING;
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

fn view_viewport_id() -> egui::Id {
    egui::Id::new("witness-view-viewport-right")
}

/// How far the current view's scroll bar pulled its content in from
/// `right` (the content panel's right edge), as of the last view drawn: 0
/// without a scroll bar, or when no view noted its viewport. In 0.2.x the
/// model banner and search box lived in the view's scroller, so they share
/// this right edge with the view's cards and rows.
pub fn view_scroll_gutter(ctx: &egui::Context, right: f32) -> f32 {
    ctx.data(|d| d.get_temp::<f32>(view_viewport_id()))
        .map_or(0.0, |viewport| (right - viewport).clamp(0.0, 20.0))
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
        let response = ui.add_enabled(enabled, egui::Checkbox::new(checked, text));
        toggle_focus_ring(ui, &response, false);
        response
    })
    .inner
}

/// The box (or circle) of a checkbox / radio laid out in `rect`: egui puts
/// it `icon_width` square at the left, vertically centred.
fn toggle_box(ui: &egui::Ui, rect: egui::Rect) -> egui::Rect {
    let side = ui.spacing().icon_width;
    egui::Rect::from_center_size(
        egui::pos2(rect.left() + side / 2.0, rect.center().y),
        egui::vec2(side, side),
    )
}

/// `:focus-visible` on a checkbox / radio: the 2 px FOCUS outline, 2 pt
/// outside the box (or circle). egui draws focus only as the active
/// visuals, which a checked toggle shares with its other states.
fn toggle_focus_ring(ui: &egui::Ui, response: &Response, round: bool) {
    if !(response.has_focus() && focus_visible(ui.ctx())) {
        return;
    }
    let target = toggle_box(ui, response.rect);
    let stroke = Stroke::new(2.0, theme::FOCUS);
    if round {
        // Centre of a 2 px stroke drawn 2 pt outside the circle.
        ui.painter()
            .circle_stroke(target.center(), target.width() / 2.0 + 3.0, stroke);
    } else {
        let radius = ui
            .visuals()
            .widgets
            .inactive
            .corner_radius
            .nw
            .saturating_add(2);
        ui.painter().rect_stroke(
            target.expand(2.0),
            CornerRadius::same(radius),
            stroke,
            egui::StrokeKind::Outside,
        );
    }
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
        let response = ui.radio_value(current, alternative, text);
        toggle_focus_ring(ui, &response, true);
        response
    })
    .inner
}

/// Range-input visuals: a visible `--border` rail and an accent fill up to
/// the value ([`slider`] paints the solid accent thumb itself, since egui
/// paints the resting handle in the rail colour).
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
            painter.circle_filled(center, radius, theme::ACCENT);
            if response.hovered() || response.dragged() {
                painter.circle_stroke(center, radius, Stroke::new(2.0, theme::FOCUS));
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
        (&mut w.inactive, BORDER),
        (&mut w.hovered, BORDER),
        (&mut w.active, theme::ACCENT),
        (&mut w.open, theme::ACCENT),
    ] {
        state.weak_bg_fill = theme::BG;
        state.bg_stroke = Stroke::new(1.0, border);
        state.corner_radius = CornerRadius::same(6);
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
    framed_text_edit(ui, id, edit, theme::BG, 6, TEXT_INPUT_MARGIN)
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
    let border = if focused { theme::ACCENT } else { BORDER };
    edit.id(id).margin(margin).frame(
        Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(CornerRadius::same(radius))
            .inner_margin(margin),
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
            btn_record(ui, false, "●", 17.0, "Start recording");
            btn_record(ui, true, "■", 17.0, "Stop recording");
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
            let response = slider(ui, &mut value, 0.3..=0.9, 0.05, Some(80.0));
            assert_eq!(response.rect.width(), 80.0);
            let mut text = String::from("teams");
            let id = egui::Id::new("text-input");
            let edit = text_input(ui, id, egui::TextEdit::singleline(&mut text));
            ui.add(edit);
            let card = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(100.0, 40.0));
            paint_left_edge(ui, card, 3.0, 8, theme::ACCENT);
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
                if c.fill == theme::ACCENT && c.center == center && c.radius == radius)
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
                egui::Shape::Rect(r) if r.stroke.color == BORDER => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(buttons.len(), 2, "{shapes:?}");
        assert!(buttons.iter().all(|r| r.fill == theme::BG));
        // `.player-speed` (12 px, 4/6 padding) is shorter than a text input.
        assert!(heights[1] <= 26.0 && heights[1] < heights[0], "{heights:?}");
    }

    #[test]
    fn view_scroll_gutter_is_the_scroll_bars_width_only_while_it_shows() {
        let ctx = theme::test_context();
        for (rows, expect_bar) in [(200, true), (2, false)] {
            let mut right = 0.0;
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(460.0, 400.0),
                    )),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        right = ui.max_rect().right();
                        clear_view_viewport(ui.ctx());
                        egui::ScrollArea::vertical()
                            .id_salt(("gutter", rows))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                keyboard_scroll(ui, KeyScroll::default());
                                for i in 0..rows {
                                    ui.label(format!("row {i}"));
                                }
                            });
                    });
                });
                output.textures_delta.clear();
            }
            let gutter = view_scroll_gutter(&ctx, right);
            if expect_bar {
                let s = ctx.global_style().spacing.scroll;
                assert!(gutter >= s.bar_width, "gutter {gutter}");
                assert!(gutter <= s.allocated_width() + 0.5, "gutter {gutter}");
            } else {
                assert_eq!(gutter, 0.0);
            }
        }
        clear_view_viewport(&ctx);
        assert_eq!(view_scroll_gutter(&ctx, 400.0), 0.0);
    }

    /// Paint shapes of one pass with a FOCUS-coloured stroke.
    fn focus_strokes(shapes: &[egui::epaint::ClippedShape]) -> usize {
        shapes
            .iter()
            .filter(|clipped| match &clipped.shape {
                egui::Shape::Rect(rect) => rect.stroke.color == theme::FOCUS,
                egui::Shape::Circle(circle) => circle.stroke.color == theme::FOCUS,
                _ => false,
            })
            .count()
    }

    #[test]
    fn checked_toggles_show_keyboard_focus() {
        for radio in [false, true] {
            let ctx = theme::test_context();
            ctx.data_mut(|d| d.insert_temp(focus_visible_id(), true));
            let mut rings = Vec::new();
            for _ in 0..3 {
                let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let response = if radio {
                            let mut engine = 1;
                            radio_value(ui, &mut engine, 1, "Parakeet")
                        } else {
                            let mut on = true;
                            checkbox(ui, true, &mut on, "Auto-record")
                        };
                        response.request_focus();
                    });
                });
                output.textures_delta.clear();
                rings.push(focus_strokes(&output.shapes));
            }
            assert_eq!(rings.last(), Some(&1), "radio={radio}: {rings:?}");

            // Not focused: no ring.
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.memory_mut(|m| m.surrender_focus(m.focused().unwrap_or(egui::Id::NULL)));
                egui::CentralPanel::default().show(ui, |ui| {
                    let mut on = true;
                    checkbox(ui, true, &mut on, "Other");
                });
            });
            output.textures_delta.clear();
            assert_eq!(focus_strokes(&output.shapes), 0);
        }
    }

    /// CSS margins are whole gaps: `gap` subtracts the item spacing egui
    /// adds after every widget, between siblings and after a child scope.
    #[test]
    fn gaps_are_exact_css_margins() {
        run(|ui| {
            let a = card_frame().show(ui, |ui| ui.label("a")).response.rect;
            gap(ui, 14.0);
            let b = card_frame().show(ui, |ui| ui.label("b")).response.rect;
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
