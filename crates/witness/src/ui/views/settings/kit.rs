//! The Settings row kit, shared by every category page, the first-run
//! setup (`ui/onboarding.rs`) and the AI assistant / updates sections:
//! sections and groups, the measured [`row`] (text left, control right,
//! stacking when narrow), switches, notes, the segmented choice, progress
//! bars, key caps, paths and the `spawn_op` helpers.

use crate::bridge::{OpDone, OpHandle};
use crate::ui::theme::{self, pal};
use crate::ui::toasts::Notice;
use crate::ui::widgets::{self, BadgeKind};
use crate::ui::UiCtx;
use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText, Stroke};
use std::sync::{Arc, Mutex};
use witness_core::settings::Settings;
use witness_core::AppCore;

/// A `spawn_op` action whose toast is reported globally but whose result
/// the view also wants (the backup summary, the new autostart state): the
/// op's completion flag plus the slot its worker fills on success (read
/// back here; dropped harmlessly if the view is gone).
pub(crate) struct OpRun<T> {
    handle: OpHandle,
    result: Arc<Mutex<Option<T>>>,
}

impl<T> OpRun<T> {
    /// Run `f` as an op; it gets the slot to fill and returns the `OpDone`.
    pub(crate) fn spawn(
        cx: &UiCtx,
        name: &'static str,
        f: impl FnOnce(&AppCore, &Mutex<Option<T>>) -> OpDone + Send + 'static,
    ) -> Self
    where
        T: Send + 'static,
    {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let handle = cx.tasks.spawn_op(name, move |c| f(c, &slot));
        OpRun { handle, result }
    }

    /// Once the op finished: `Some(result)` (`None` inside when it failed).
    pub(crate) fn finished(slot: &mut Option<Self>) -> Option<Option<T>> {
        let run = slot.take_if(|run| run.handle.is_done())?;
        Some(run.result.lock().ok().and_then(|mut r| r.take()))
    }
}

/// Fill an `OpRun` slot from its worker.
pub(crate) fn fill<T>(slot: &Mutex<Option<T>>, value: T) {
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(value);
    }
}

// ----------------------------------------------------------------------
// Action helpers

/// Save `edit` applied to the store's current (optimistic) settings.
pub(crate) fn save(cx: &mut UiCtx, edit: impl FnOnce(&mut Settings)) {
    let mut next = cx.settings.current().clone();
    edit(&mut next);
    cx.settings.save(next, cx.tasks);
}

pub(crate) fn quiet() -> OpDone {
    OpDone::Notice {
        notice: None,
        refresh: false,
    }
}

pub(crate) fn notice_done(notice: Notice) -> OpDone {
    OpDone::Notice {
        notice: Some(notice),
        refresh: false,
    }
}

/// One option of a [`segmented`] control.
pub(crate) struct Segment<T> {
    value: T,
    text: &'static str,
    lead: Option<Lead>,
}

/// What sits before a segment's text.
#[derive(Clone, Copy)]
enum Lead {
    Icon(&'static str),
    Swatch(egui::Color32),
}

impl<T> Segment<T> {
    pub(crate) fn new(value: T, text: &'static str) -> Self {
        Segment {
            value,
            text,
            lead: None,
        }
    }

    pub(crate) fn icon(mut self, glyph: &'static str) -> Self {
        self.lead = Some(Lead::Icon(glyph));
        self
    }

    pub(crate) fn swatch(mut self, color: egui::Color32) -> Self {
        self.lead = Some(Lead::Swatch(color));
        self
    }
}

/// The gap between a segmented control's track and its segments.
const SEGMENT_INSET: f32 = 3.0;

/// A segmented choice (Windows 11's selector bar): an inset track as wide
/// as the slot, equal segments, the picked one an accent tint with accent
/// text. Each segment is a focusable radio button named after its text,
/// the group labelled by the row title `label`. Returns whether the value
/// changed.
pub(crate) fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    salt: &str,
    label: egui::Id,
    value: &mut T,
    options: &[Segment<T>],
) -> bool {
    let height = ui.spacing().interact_size.y;
    let (rect, group) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let group = group.labelled_by(label);
    let enabled = ui.is_enabled();
    let p = pal();
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::R_MD),
        p.bg,
        Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink(SEGMENT_INSET);
    let width = inner.width() / options.len().max(1) as f32;
    let mut changed = false;
    for (i, option) in options.iter().enumerate() {
        let seg = egui::Rect::from_min_size(
            inner.min + egui::vec2(width * i as f32, 0.0),
            egui::vec2(width, inner.height()),
        );
        let response = ui.interact(seg, group.id.with((salt, i)), egui::Sense::click());
        if enabled && response.clicked() && option.value != *value {
            *value = option.value;
            changed = true;
        }
        let selected = option.value == *value;
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::RadioButton,
                enabled,
                selected,
                option.text,
            )
        });
        let ring = response.has_focus() && widgets::focus_visible(ui.ctx());
        let hovered = enabled && (response.hovered() || ring);
        let radius = CornerRadius::same(theme::R_SM);
        if selected {
            ui.painter().rect_filled(seg, radius, p.bg_active);
        } else if hovered {
            ui.painter().rect_filled(seg, radius, p.bg_hover);
        }
        let (font, color) = if selected {
            (theme::semibold(13.0), p.text)
        } else if hovered {
            (theme::regular(13.0), p.text)
        } else {
            (theme::regular(13.0), p.text_muted)
        };
        paint_segment(ui, seg, option, font, color);
        if ring {
            ui.painter().rect_stroke(
                seg.expand(1.0),
                radius,
                Stroke::new(2.0, p.focus),
                egui::StrokeKind::Outside,
            );
        }
    }
    if !enabled {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::R_MD),
            theme::alpha(p.bg_raised, 0.45),
        );
    }
    changed
}

/// A segment's lead (icon or swatch) and text, centred; the text elides
/// when the segment is narrow.
fn paint_segment<T>(
    ui: &egui::Ui,
    seg: egui::Rect,
    option: &Segment<T>,
    font: egui::FontId,
    color: egui::Color32,
) {
    const LEAD: f32 = 14.0;
    const GAP: f32 = 6.0;
    let lead_w = if option.lead.is_some() {
        LEAD + GAP
    } else {
        0.0
    };
    let room = (seg.width() - 2.0 * theme::S2 - lead_w).max(8.0);
    let mut job = egui::text::LayoutJob::simple_singleline(option.text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(room);
    let galley = ui.painter().layout_job(job);
    let left = seg.center().x - (lead_w + galley.size().x) / 2.0;
    let painter = ui.painter();
    let lead_center = egui::pos2(left + LEAD / 2.0, seg.center().y);
    match option.lead {
        Some(Lead::Icon(glyph)) => {
            painter.text(
                lead_center,
                egui::Align2::CENTER_CENTER,
                glyph,
                egui::FontId::proportional(LEAD),
                color,
            );
        }
        Some(Lead::Swatch(fill)) => {
            painter.circle_filled(lead_center, 6.0, fill);
            // A hairline keeps a pale swatch visible on the light track.
            painter.circle_stroke(
                lead_center,
                6.0,
                Stroke::new(1.0, theme::alpha(pal().text, 0.15)),
            );
        }
        None => {}
    }
    let pos = egui::pos2(left + lead_w, seg.center().y - galley.size().y / 2.0);
    painter.galley(pos, galley, color);
}

// ----------------------------------------------------------------------
// Layout helpers (shared with updates.rs and the first-run setup)

/// Below this a row's text column would be squeezed: the control stacks
/// under the text instead.
const TEXT_MIN_WIDTH: f32 = 200.0;
/// Room for a select / text field / slider beside its text.
pub(crate) const FIELD_WIDTH: f32 = 260.0;
/// The gap between a row's text and its control.
const ROW_GAP: f32 = theme::S4;
/// Diagnostics key column; under this much row width keys stack.
const KEY_WIDTH: f32 = 128.0;
const KV_SIDE_BY_SIDE: f32 = 400.0;

/// A settings section: its header and one group holding `add`'s rows.
pub(crate) fn section<R>(
    ui: &mut egui::Ui,
    title: &str,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    widgets::section_header(ui, title);
    let group = ui.scope(|ui| widgets::group(ui, add));
    mark_section(ui, section_anchor_id(title), group.response.rect);
    group.inner
}

// ----------------------------------------------------------------------
// Search hits: scrolled into view and briefly highlighted

/// How long a search hit's highlight stays at full strength, then fades.
const MARK_HOLD: f32 = 0.8;
const MARK_FADE: f32 = 1.0;
/// For this long the hit is kept scrolled into view (rows measure
/// themselves, and loads can still move it, in the first passes).
const MARK_SCROLL: f32 = 0.5;

/// The id a [`row`] with this `salt` answers to as a search anchor.
pub(crate) fn row_anchor_id(salt: &str) -> egui::Id {
    egui::Id::new(("settings-row", salt))
}

/// The id a [`section`] titled `title` answers to as a search anchor.
pub(crate) fn section_anchor_id(title: &str) -> egui::Id {
    egui::Id::new(("settings-section", title))
}

/// A search hit being shown.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mark {
    anchor: egui::Id,
    /// `InputState::time` when it was opened.
    at: f64,
    /// Debug snapshots: never fades.
    frozen: bool,
}

fn mark_key() -> egui::Id {
    egui::Id::new("settings-search-mark")
}

/// Scroll the row or section `anchor` into view and highlight it briefly.
pub(crate) fn show_anchor(ctx: &egui::Context, anchor: egui::Id, frozen: bool) {
    let at = ctx.input(|i| i.time);
    ctx.data_mut(|d| d.insert_temp(mark_key(), Mark { anchor, at, frozen }));
    ctx.request_repaint();
}

/// How strong the highlight is `age` seconds after the hit was opened:
/// full, then a linear fade (gone at once without animations).
pub(crate) fn mark_strength(age: f32, animate: bool) -> f32 {
    if age < MARK_HOLD {
        1.0
    } else if !animate {
        if age < MARK_HOLD + MARK_FADE / 2.0 {
            1.0
        } else {
            0.0
        }
    } else {
        (1.0 - (age - MARK_HOLD) / MARK_FADE).clamp(0.0, 1.0)
    }
}

/// The current mark for `anchor` and its strength; keeps it scrolled into
/// view at first, repaints while it fades and forgets it once gone.
fn active_mark(ui: &egui::Ui, anchor: egui::Id, rect: egui::Rect) -> Option<f32> {
    let mark = ui
        .data(|d| d.get_temp::<Mark>(mark_key()))
        .filter(|m| m.anchor == anchor)?;
    let age = (ui.input(|i| i.time) - mark.at) as f32;
    let animate = crate::ui::icons::animations_enabled();
    let strength = if mark.frozen {
        1.0
    } else {
        mark_strength(age, animate)
    };
    if age < MARK_SCROLL || mark.frozen {
        ui.scroll_to_rect(rect, Some(Align::Center));
    }
    if strength <= 0.0 {
        ui.data_mut(|d| d.remove::<Mark>(mark_key()));
        return None;
    }
    if !mark.frozen {
        if animate && age >= MARK_HOLD {
            crate::ui::icons::request_animation_frame(ui.ctx());
        } else {
            let left = (MARK_HOLD + MARK_FADE / 2.0 - age).max(0.05);
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f32(left));
        }
    }
    Some(strength)
}

/// A row hit: a search-hit tint behind the row (into the group's padding).
fn mark_anchor(ui: &egui::Ui, anchor: egui::Id, rect: egui::Rect, slot: egui::layers::ShapeIdx) {
    let Some(strength) = active_mark(ui, anchor, rect) else {
        return;
    };
    let fill = pal().mark_bg.gamma_multiply(strength);
    ui.painter().set(
        slot,
        egui::epaint::RectShape::filled(
            rect.expand2(egui::vec2(theme::S2, 6.0)),
            CornerRadius::same(theme::R_MD),
            fill,
        ),
    );
}

/// A section hit: an accent ring around its group.
fn mark_section(ui: &egui::Ui, anchor: egui::Id, rect: egui::Rect) {
    let Some(strength) = active_mark(ui, anchor, rect) else {
        return;
    };
    ui.painter().rect_stroke(
        rect.expand(1.0),
        CornerRadius::same(theme::R_LG + 1),
        Stroke::new(2.0, theme::alpha(pal().accent, strength)),
        egui::StrokeKind::Outside,
    );
}

/// How a [`row`]'s control is sized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Slot {
    /// As wide as its content (switches, buttons, badges): measured, so the
    /// control stays beside the text whenever the text keeps
    /// [`TEXT_MIN_WIDTH`].
    Fit,
    /// A field that fills the given width beside the text, or the whole
    /// row once stacked (selects, text inputs, sliders).
    Fill(f32),
    /// Always under the text, full width (multi-line inputs).
    Below,
}

/// What a [`row`] measured last pass, to centre and right-align its parts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct RowMetrics {
    text_height: f32,
    /// The title's one-line width: a control never squeezes it into `…`.
    title_width: f32,
    /// The control's natural (one-line) width.
    control_width: f32,
}

/// A settings row (the kit's `setting_row`, with what Settings needs on
/// top): `text` draws the title + description column and returns the
/// title's id, `control` gets that id (for `labelled_by`) and is placed on
/// the right in a control-high band, vertically centred on the text. When
/// the text would get narrower than [`TEXT_MIN_WIDTH`] the control stacks
/// under it, so a switch stays beside its label at 380 pt while a select
/// stacks; stacked controls wider than the row wrap.
///
/// Immediate mode cannot centre or right-align before it knows the sizes,
/// so the row remembers last pass's text height and control width under
/// `salt` and discards the pass when they change (no visible jump).
pub(crate) fn row<R>(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    slot: Slot,
    text: impl FnOnce(&mut egui::Ui) -> egui::Id,
    control: impl FnOnce(&mut egui::Ui, egui::Id) -> R,
) -> R {
    let id = egui::Id::new(("settings-row", salt));
    let last: Option<RowMetrics> = ui.data(|d| d.get_temp(id));
    // Behind the row: a search hit's highlight (`mark_anchor`).
    let mark = ui.painter().add(egui::Shape::Noop);
    let full = ui.available_width();
    let band = ui.spacing().interact_size.y;
    let control_width = match slot {
        Slot::Fit => last.map(|m| m.control_width),
        Slot::Fill(width) => Some(width),
        Slot::Below => None,
    };
    let title_width = last.map_or(0.0, |m| m.title_width);
    let beside = control_width.filter(|w| {
        let text = full - w - ROW_GAP;
        text >= TEXT_MIN_WIDTH && text >= title_width
    });
    let text_block = |ui: &mut egui::Ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        text(ui)
    };

    let (out, now, rect) = match beside {
        Some(width) => {
            let text_height = last.map_or(0.0, |m| m.text_height);
            let height = text_height.max(band);
            let origin = ui.cursor().min;
            let text_width = full - width - ROW_GAP;
            let mut text_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt((id, "text"))
                    .max_rect(egui::Rect::from_min_size(
                        origin + egui::vec2(0.0, (height - text_height) / 2.0),
                        egui::vec2(text_width, f32::INFINITY),
                    ))
                    .layout(Layout::top_down(Align::Min)),
            );
            let label = text_block(&mut text_ui);
            let mut control_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt((id, "control"))
                    .max_rect(egui::Rect::from_min_size(
                        origin + egui::vec2(full - width, (height - band) / 2.0),
                        egui::vec2(width, band),
                    ))
                    .layout(Layout::left_to_right(Align::Center)),
            );
            let out = control(&mut control_ui, label);
            let (text_rect, control_rect) = (text_ui.min_rect(), control_ui.min_rect());
            let bottom = text_rect.bottom().max(control_rect.bottom());
            let rect = egui::Rect::from_min_max(origin, egui::pos2(origin.x + full, bottom));
            ui.advance_cursor_after_rect(rect);
            let now = RowMetrics {
                text_height: text_rect.height(),
                title_width: natural_title_width(ui, label),
                control_width: match slot {
                    Slot::Fit => control_rect.width(),
                    _ => width,
                },
            };
            (out, now, rect)
        }
        None => {
            let scope = ui.scope_builder(egui::UiBuilder::new().id_salt((id, "stacked")), |ui| {
                let label = ui.scope(text_block).inner;
                widgets::gap(ui, theme::S2);
                // Wrap (several buttons) only once the controls are known
                // to be wider than the row; otherwise lay them out on one
                // line, so what is measured is their natural width. Fields
                // never wrap: they size to the row (in a wrapping layout
                // the available width would be the whole row again).
                let wrap = slot == Slot::Fit && last.is_some_and(|m| m.control_width > full);
                let layout = Layout::left_to_right(Align::Center).with_main_wrap(wrap);
                let inner = ui.allocate_ui_with_layout(egui::vec2(full, band), layout, |ui| {
                    control(ui, label)
                });
                let now = RowMetrics {
                    // Kept for when the row goes back beside.
                    text_height: last.map_or(0.0, |m| m.text_height),
                    title_width: natural_title_width(ui, label),
                    control_width: match (slot, last) {
                        (Slot::Fit, Some(m)) if wrap => m.control_width,
                        (Slot::Fit, _) => inner.response.rect.width(),
                        // Only a `Fit` control's width is ever used.
                        _ => 0.0,
                    },
                };
                (inner.inner, now)
            });
            let (out, now) = scope.inner;
            (out, now, scope.response.rect)
        }
    };
    mark_anchor(ui, id, rect, mark);

    let changed = last.is_none_or(|m| {
        (m.text_height - now.text_height).abs() > 0.5
            || (m.title_width - now.title_width).abs() > 0.5
            || (m.control_width - now.control_width).abs() > 0.5
    });
    if changed {
        ui.data_mut(|d| d.insert_temp(id, now));
        // Re-run the pass with the new sizes before anything is shown (and
        // repaint, should this pass already be the last one allowed).
        ui.ctx().request_discard("settings row measured");
        ui.ctx().request_repaint();
    }
    out
}

/// A boolean setting: title + muted description, the switch on the right.
pub(crate) fn toggle(
    ui: &mut egui::Ui,
    enabled: bool,
    on: &mut bool,
    name: &str,
    description: &str,
) -> egui::Response {
    row(
        ui,
        name,
        Slot::Fit,
        |ui| {
            if !enabled {
                ui.disable();
            }
            let id = title(ui, name);
            note(ui, description);
            id
        },
        |ui, _| widgets::switch(ui, enabled, on, name),
    )
}

/// A row title: body text, one line, elided with a tooltip when long. Its
/// one-line width is noted for [`row`].
pub(crate) fn title(ui: &mut egui::Ui, text: &str) -> egui::Id {
    let font = theme::regular(theme::BASE_SIZE);
    let natural = text_width(ui, text, font.clone());
    let id = widgets::elide(ui, text, font, pal().text).id;
    ui.data_mut(|d| d.insert_temp(id.with("natural-width"), natural));
    id
}

/// What [`title`] noted for the title `id` (0 for other row texts).
fn natural_title_width(ui: &egui::Ui, id: egui::Id) -> f32 {
    ui.data(|d| d.get_temp(id.with("natural-width")))
        .unwrap_or(0.0)
}

/// A row description: muted 12.5 pt, wrapping.
pub(crate) fn note(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::regular(12.5))
                .color(pal().text_muted),
        )
        .wrap(),
    );
}

/// A coloured status line under a row title (download progress, errors).
pub(crate) fn status_note(ui: &mut egui::Ui, color: egui::Color32, text: &str) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(text).font(theme::regular(12.5)).color(color)).wrap())
}

/// Help text under a group, aligned with its section header.
pub(crate) fn footnote(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::regular(12.5))
                .color(pal().text_muted),
        )
        .wrap(),
    );
}

/// A thin accent progress bar across the available width (`fraction`
/// 0..1).
pub(crate) fn progress_bar(ui: &mut egui::Ui, fraction: f32) {
    ui.add_space(theme::S1);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
    let radius = CornerRadius::same(2);
    ui.painter()
        .rect_filled(rect, radius, theme::pal().border_strong);
    let mut done = rect;
    done.set_width(rect.width() * fraction.clamp(0.0, 1.0));
    if done.width() > 0.0 {
        ui.painter().rect_filled(done, radius, theme::pal().accent);
    }
}

/// A secondary button, disabled unless `enabled`.
pub(crate) fn button(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| widgets::btn(ui, text))
        .inner
}

/// A path in a small inset box: monospace, one line, elided (the tooltip
/// has the whole path).
pub(crate) fn path_box(ui: &mut egui::Ui, text: &str) {
    ui.add_space(theme::S1);
    egui::Frame::new()
        .fill(pal().bg)
        .corner_radius(CornerRadius::same(theme::R_SM))
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            widgets::elide(ui, text, egui::FontId::monospace(12.0), pal().text);
        });
}

/// A key cap for a hotkey combo.
pub(crate) fn keycap(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(pal().bg)
        .stroke(Stroke::new(1.0, theme::pal().border_strong))
        .corner_radius(CornerRadius::same(theme::R_SM))
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .font(egui::FontId::monospace(12.0))
                    .color(pal().text),
            );
        });
}

/// A warning line inside a group (warn tint) with one action button on the
/// right; true when it was clicked.
pub(crate) fn warning_strip(ui: &mut egui::Ui, text: &str, action: &str, enabled: bool) -> bool {
    egui::Frame::new()
        .fill(theme::pal().warn_bg)
        .stroke(Stroke::new(1.0, theme::pal().warn_border))
        .corner_radius(CornerRadius::same(theme::R_MD))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            row(
                ui,
                ("warning", text),
                Slot::Fit,
                |ui| {
                    ui.add(egui::Label::new(RichText::new(text).color(theme::pal().warn)).wrap())
                        .id
                },
                |ui, _| button(ui, enabled, action).clicked(),
            )
        })
        .inner
}

/// The uppercase badge when it fits the width left, else its text in the
/// badge's colour, wrapping (a long "not connected" in a narrow column).
pub(crate) fn badge_or_text(ui: &mut egui::Ui, kind: BadgeKind, text: &str) {
    let upper = text.to_uppercase();
    let width =
        text_width(ui, &upper, theme::regular(11.0)) + 0.4 * upper.chars().count() as f32 + 16.0;
    if width <= ui.available_width() {
        widgets::badge(ui, kind, text);
    } else {
        let color = match kind {
            BadgeKind::Ok => theme::pal().ok_light,
            BadgeKind::Busy => theme::pal().warn,
            BadgeKind::Failed => theme::pal().rec_light,
        };
        status_note(ui, color, text);
    }
}

/// Single-line width of `text` in `font`.
pub(crate) fn text_width(ui: &egui::Ui, text: &str, font: egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, pal().text)
        .size()
        .x
}

/// A diagnostics key/value line: the key in a muted column beside the
/// value, or above it when the group is narrow.
pub(crate) fn kv_row(ui: &mut egui::Ui, key: &str, value: impl FnOnce(&mut egui::Ui)) {
    let full = ui.available_width();
    let key_label = |ui: &mut egui::Ui| {
        ui.add(egui::Label::new(RichText::new(key).color(pal().text_muted)).wrap());
    };
    if full >= KV_SIDE_BY_SIDE {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S3;
            ui.allocate_ui_with_layout(
                egui::vec2(KEY_WIDTH, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(KEY_WIDTH);
                    key_label(ui);
                },
            );
            let width = ui.available_width();
            ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(width);
                    value(ui);
                },
            );
        });
    } else {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(
                egui::Label::new(
                    RichText::new(key)
                        .font(theme::regular(12.5))
                        .color(pal().text_muted),
                )
                .wrap(),
            );
            value(ui);
        });
    }
}

/// A diagnostics value: body text, wrapping.
pub(crate) fn value(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(pal().text)).wrap());
}

/// A diagnostics path (monospace, elided) with an optional trailing badge;
/// returns the badge's response.
pub(crate) fn path_value(
    ui: &mut egui::Ui,
    path: &str,
    badge: Option<(BadgeKind, &str)>,
) -> Option<egui::Response> {
    // 12 pt monospace is a little shorter than the body-text key beside it.
    ui.add_space(2.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S2;
        let badge_width = badge.map_or(0.0, |(_, text)| {
            let upper = text.to_uppercase();
            text_width(ui, &upper, theme::regular(11.0))
                + 0.4 * upper.chars().count() as f32
                + 16.0
                + theme::S2
        });
        let path_width = (ui.available_width() - badge_width).max(40.0);
        ui.allocate_ui_with_layout(
            egui::vec2(path_width, 0.0),
            Layout::left_to_right(Align::Min),
            |ui| {
                widgets::elide(ui, path, egui::FontId::monospace(12.0), pal().text);
            },
        );
        badge.map(|(kind, text)| widgets::badge(ui, kind, text))
    })
    .inner
}

/// Tests: lay out `f` in a `width` pt panel for a few passes (rows measure
/// themselves on the first) and return what it produced last.
#[cfg(test)]
pub(crate) fn run_at<R>(width: f32, mut f: impl FnMut(&mut egui::Ui) -> R) -> R {
    let ctx = theme::test_context();
    let mut out = None;
    for _ in 0..4 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 2000.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| out = Some(f(ui)));
        });
        output.textures_delta.clear(); // no renderer to upload the font atlas
    }
    out.expect("ran")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A switch stays beside its label even in the narrowest window, while
    /// a field wide enough to squeeze the text stacks under it.
    #[test]
    fn rows_keep_switches_beside_and_stack_fields() {
        for width in [380.0, 460.0, 640.0, 1000.0] {
            let (title_rect, switch_rect, field_label, field) = run_at(width, |ui| {
                let mut on = true;
                let mut title_rect = egui::Rect::NOTHING;
                let switch = row(
                    ui,
                    "switch",
                    Slot::Fit,
                    |ui| {
                        let r = widgets::elide(ui, "Auto-record", theme::regular(14.0), pal().text);
                        title_rect = r.rect;
                        r.id
                    },
                    |ui, _| widgets::switch(ui, true, &mut on, "Auto-record"),
                );
                let mut label_rect = egui::Rect::NOTHING;
                let field = row(
                    ui,
                    "field",
                    Slot::Fill(FIELD_WIDTH),
                    |ui| {
                        let r =
                            widgets::elide(ui, "Watch patterns", theme::regular(14.0), pal().text);
                        label_rect = r.rect;
                        r.id
                    },
                    |ui, _| ui.label("field").rect,
                );
                (title_rect, switch.rect, label_rect, field)
            });
            // Beside, right-aligned, vertically centred on the title.
            assert!(switch_rect.left() > title_rect.right(), "{width}");
            assert!(
                (switch_rect.center().y - title_rect.center().y).abs() < 1.5,
                "{width}"
            );
            let stacked = field.top() > field_label.bottom();
            // The panel has 8 pt margins; the field stacks below 2*8 + 260 +
            // 16 + 200 pt.
            assert_eq!(stacked, width < 492.0, "{width}");
        }
    }
}
