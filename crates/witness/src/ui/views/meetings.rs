//! Meetings list + recycle bin (map-ui-meetings §1, meetings.tsx).
//!
//! Loads are view-owned [`Task`]s (dropping one discards a stale result,
//! like the TSX generation counters); every action — rename, delete,
//! restore, purge, empty bin, retranscribe — goes through
//! `Tasks::spawn_op`, so its toast and list refresh survive navigating away
//! mid-action. The list reloads whenever `shell.refresh_tick` changes.
//!
//! The list is flat rows (no per-row cards) grouped under day headers
//! ("Today", "Yesterday", "Thu 23 Jul"). Rows show a title and a one-line
//! meta (time · duration · trigger); a status icon only for non-default
//! states; rename / delete appear on hover or keyboard focus.
//!
//! Rows have a uniform height and day headers another, so the list is one
//! allocation and only the items inside the clip rect (plus a row either
//! side, the focused row and the row being renamed) are laid out: hundreds
//! of loaded rows stay cheap while Tab can still walk the whole list. The
//! recycle bin is a separate, normally laid-out section after the list.
//!
//! While nothing holds the keyboard focus, ↑/↓ (j/k) and Home/End move a
//! selection highlight over the rows (and the open bin's), scrolling it
//! into view; Enter opens, Delete moves to the bin, F2 renames. In a wide
//! window's split (`PaneRole::List`) the meeting open beside the list is
//! marked with the accent pill and takes the selection. A first load shows
//! a row-shaped skeleton; an empty library a designed empty state. The
//! selection, skeleton and empty-state helpers are shared with Search and
//! People.

use crate::bridge::{OpDone, Task};
use crate::ui::format::fmt_duration;
use crate::ui::icons::{self, ph};
use crate::ui::theme::{self, pal};
use crate::ui::toasts::{Notice, ToastAction};
use crate::ui::widgets::{self, KeyScroll};
use crate::ui::{PaneRole, UiCtx, View};
use chrono::{DateTime, Datelike, Local, NaiveDate};
use eframe::egui::{
    self, text::LayoutJob, text::TextWrapping, Align, Color32, CornerRadius, CursorIcon, FontId,
    Key, Layout, Margin, PointerButton, Rect, Response, Sense, Stroke, StrokeKind, UiBuilder,
    WidgetInfo, WidgetType,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use witness_core::db::Meeting;
use witness_core::service::DeletedMeeting;
use witness_core::{CoreResult, RecCmd, Trigger};

/// Rows per page (Load more) and the minimum reload size.
const PAGE: usize = 50;
/// `AppCore::list_meetings` clamps its limit to this; larger reloads are
/// fetched in chunks.
const BACKEND_MAX: usize = 500;
/// A single click opens after this delay unless a double-click (rename)
/// arrives first.
const OPEN_DELAY: Duration = Duration::from_millis(250);
/// Between two flat rows: just enough that adjacent hover fills don't merge.
const ROW_GAP: f32 = 2.0;
/// Row padding (`widgets::list_row`'s 12 / 8).
const ROW_PAD: egui::Vec2 = egui::vec2(12.0, 8.0);
/// Between two trailing icon buttons.
const COL_GAP: f32 = 2.0;
/// Between the title block and the first trailing icon.
const MAIN_GAP: f32 = 8.0;
/// Status and action glyphs.
const ICON: f32 = 16.0;
/// Between the title and the meta line.
const TITLE_META_GAP: f32 = 2.0;
/// Below a day header, before its first row.
const HEADER_BELOW: f32 = 4.0;
/// The rename input's padding.
const EDIT_MARGIN: Margin = Margin {
    left: 6,
    right: 6,
    top: 3,
    bottom: 3,
};

/// The one confirmation this view can have open at a time.
const CONFIRM_ID: &str = "meetings-confirm";

/// Status words (tooltip and accessible name): `STATUS_LABEL` in meetings.tsx.
pub(crate) fn status_label(status: &str) -> &str {
    match status {
        "recording" => "Recording",
        "recorded" => "Recorded — not transcribed yet",
        "processing" => "Transcribing…",
        "transcribed" => "Transcribed",
        "failed" => "Transcription failed",
        other => other,
    }
}

/// Glyph and colour of the `StatusIcon` for `status`.
fn status_glyph(status: &str) -> (&'static str, Color32) {
    match status {
        "recording" => (ph::RECORD, pal().rec),
        "recorded" => (ph::WAVEFORM, pal().warn),
        "processing" => (ph::CIRCLE_NOTCH, pal().warn),
        "failed" => (ph::WARNING_CIRCLE, pal().rec),
        _ => (ph::CHECK_CIRCLE, theme::pal().ok),
    }
}

/// Whether a row shows a trailing status: only the non-default states (a
/// transcribed meeting — the normal case — shows nothing).
fn shows_status(status: &str) -> bool {
    matches!(status, "recording" | "recorded" | "processing" | "failed")
}

/// `started_at` in local time; None when it doesn't parse.
fn local_start(rfc3339: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|at| at.with_timezone(&Local))
}

/// The day group a meeting falls in (local date of its start).
fn local_day(rfc3339: &str) -> Option<NaiveDate> {
    local_start(rfc3339).map(|at| at.date_naive())
}

/// A day header: "Today", "Yesterday", "Thu 23 Jul", or "Thu 23 Jul 2025"
/// outside the current year; "Unknown date" for an unparseable start.
fn day_label(day: Option<NaiveDate>, today: NaiveDate) -> String {
    let Some(day) = day else {
        return "Unknown date".into();
    };
    if day == today {
        "Today".into()
    } else if today.pred_opt() == Some(day) {
        "Yesterday".into()
    } else if day.year() == today.year() {
        day.format("%a %-d %b").to_string()
    } else {
        day.format("%a %-d %b %Y").to_string()
    }
}

/// `"14:07"` local time (the date is the row's day header); unparseable
/// input is returned unchanged.
fn fmt_time(rfc3339: &str) -> String {
    local_start(rfc3339).map_or_else(|| rfc3339.to_owned(), |at| at.format("%H:%M").to_string())
}

/// A short local date + time for rows outside the day groups (the bin):
/// `"Thu 23 Jul, 14:07"`, with the year when it isn't this one.
fn fmt_short_date(rfc3339: &str, today: NaiveDate) -> String {
    match local_start(rfc3339) {
        Some(at) if at.year() == today.year() => at.format("%a %-d %b, %H:%M").to_string(),
        Some(at) => at.format("%a %-d %b %Y, %H:%M").to_string(),
        None => rfc3339.to_owned(),
    }
}

/// `"{time} · {duration}"`, plus `" · auto"` for auto-started meetings.
fn meta_line(m: &Meeting) -> String {
    let auto = if m.trigger == "auto" { " · auto" } else { "" };
    format!(
        "{} · {}{auto}",
        fmt_time(&m.started_at),
        fmt_duration(m.duration_ms)
    )
}

/// The recycle-bin row's meta line (the 30-day rule is said once, in the
/// bin's caption, so the line stays short enough not to elide).
fn bin_meta_line(d: &DeletedMeeting, today: NaiveDate) -> String {
    format!(
        "{} · {} · deleted {}",
        fmt_short_date(&d.meeting.started_at, today),
        fmt_duration(d.meeting.duration_ms),
        in_sentence(day_label(local_day(&d.deleted_at), today))
    )
}

/// A day label inside a sentence: the relative words lower-cased
/// ("deleted yesterday"), dates as they are ("deleted Thu 23 Jul").
fn in_sentence(label: String) -> String {
    match label.as_str() {
        "Today" | "Yesterday" | "Unknown date" => label.to_lowercase(),
        _ => label,
    }
}

/// Open the transcript of `meeting_id`; Back returns to `from`.
pub(crate) fn open_transcript(cx: &mut UiCtx, meeting_id: i64, from: View) {
    cx.nav.go(View::Transcript {
        meeting_id,
        segment_id: None,
        highlight: None,
        from: Some(Box::new(from)),
    });
}

/// A single-line galley truncated with an ellipsis at `width`
/// (`white-space: nowrap; text-overflow: ellipsis`).
pub(crate) fn truncated(
    ui: &egui::Ui,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping::truncate_at_width(width.max(1.0));
    ui.painter().layout_job(job)
}

/// Paint the `:focus-visible` ring (2 px, 2 px outside) around `rect`.
pub(crate) fn paint_focus_ring(ui: &egui::Ui, rect: Rect, radius: u8) {
    ui.painter().rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius + 2),
        Stroke::new(2.0, pal().focus),
        StrokeKind::Outside,
    );
}

/// The row-style focus ring (inside the row, like `widgets::list_row`).
fn paint_row_focus(ui: &egui::Ui, rect: Rect) {
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(theme::R_MD),
        Stroke::new(2.0, pal().focus),
        StrokeKind::Inside,
    );
}

/// Scroll a widget that just took keyboard focus (Tab) into view, as a
/// browser does; egui only scrolls for AccessKit requests. Focus taken by a
/// pointer press doesn't scroll.
pub(crate) fn follow_focus(response: &Response) {
    let by_pointer = response
        .ctx
        .input(|i| i.pointer.any_pressed() || i.pointer.any_released());
    if response.gained_focus() && !by_pointer {
        response.scroll_to_me(None);
    }
}

// ----------------------------------------------------------------------
// Keyboard selection, loading and empty states (shared with Search and
// People, which build their lists on the same model)

/// How a list key moves the selection highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    /// ↓ / j (positive) and ↑ / k (negative), net of this pass's presses.
    By(i32),
    /// Home.
    First,
    /// End.
    Last,
}

/// The item `motion` selects in `items` (in display order) from `current`.
/// Without a current item (or one no longer listed) a step lands on the
/// first item, End on the last; steps stop at either end. None only for an
/// empty list.
pub(crate) fn step_selection<T: PartialEq + Clone>(
    items: &[T],
    current: Option<&T>,
    motion: Motion,
) -> Option<T> {
    let last = items.len().checked_sub(1)?;
    let at = current.and_then(|c| items.iter().position(|item| item == c));
    let index = match (motion, at) {
        (Motion::First, _) => 0,
        (Motion::Last, _) => last,
        (Motion::By(_), None) => 0,
        (Motion::By(n), Some(at)) => (at as i64 + i64::from(n)).clamp(0, last as i64) as usize,
    };
    items.get(index).cloned()
}

/// The list keys pressed this pass (consumed, so the view's scroller does
/// not also scroll by them).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListKeys {
    pub motion: Option<Motion>,
    /// Enter (and Space where the view lets it): open / toggle.
    pub activate: bool,
    /// Delete: `Some(shift)` (Shift skips the confirmation, like Shift-click).
    pub delete: Option<bool>,
    /// F2: rename.
    pub rename: bool,
}

/// Whether the list keys drive the selection now: nothing holds the
/// keyboard focus — a focused text field types, a focused button takes
/// Enter / Space itself and egui moves the focus with the arrows — and no
/// modal is open.
pub(crate) fn list_keys_free(ctx: &egui::Context, modal_open: bool) -> bool {
    !modal_open && ctx.memory(|m| m.focused().is_none())
}

/// Read (and consume) the list keys: ↑/↓ and j/k step, Home/End jump,
/// Enter activates, and so does Space when `space` (else Space is left to
/// the scroller's page down). `edit_keys` also reads Delete and F2.
pub(crate) fn read_list_keys(ctx: &egui::Context, space: bool, edit_keys: bool) -> ListKeys {
    use egui::Modifiers;
    ctx.input_mut(|i| {
        let mut count = |key| i.count_and_consume_key(Modifiers::NONE, key) as i32;
        let down = count(Key::ArrowDown) + count(Key::J);
        let up = count(Key::ArrowUp) + count(Key::K);
        let home = count(Key::Home) > 0;
        let end = count(Key::End) > 0;
        let mut activate = count(Key::Enter) > 0;
        if space {
            activate |= count(Key::Space) > 0;
        }
        // From the press itself: Shift may be up again by the time the
        // pass runs (and tests feed modifiers only on the event).
        let shift = i.modifiers.shift
            || i.events.iter().any(|e| {
                matches!(e, egui::Event::Key { key: Key::Delete, pressed: true, modifiers, .. } if modifiers.shift)
            });
        let (delete, rename) = if edit_keys {
            (
                (i.count_and_consume_key(Modifiers::NONE, Key::Delete) > 0).then_some(shift),
                i.count_and_consume_key(Modifiers::NONE, Key::F2) > 0,
            )
        } else {
            (None, false)
        };
        let motion = if home {
            Some(Motion::First)
        } else if end {
            Some(Motion::Last)
        } else {
            (down != up).then_some(Motion::By(down - up))
        };
        ListKeys {
            motion,
            activate,
            delete,
            rename,
        }
    })
}

/// The selected-row fill and, for the meeting open beside the list (a
/// split's `PaneRole::List`), the accent indicator at the row's left edge
/// (the Windows 11 list-selection pill).
pub(crate) fn paint_selection(ui: &egui::Ui, rect: Rect, open: bool) {
    ui.painter()
        .rect_filled(rect, CornerRadius::same(theme::R_MD), pal().bg_active);
    if open {
        paint_open_pill(ui, rect);
    }
}

/// Only the accent pill of [`paint_selection`] (rows that fill themselves).
pub(crate) fn paint_open_pill(ui: &egui::Ui, rect: Rect) {
    let height = (rect.height() * 0.4).clamp(12.0, 18.0);
    let pill = Rect::from_center_size(
        egui::pos2(rect.left() + 4.0, rect.center().y),
        egui::vec2(3.0, height),
    );
    ui.painter()
        .rect_filled(pill, CornerRadius::same(2), pal().accent);
}

/// A first load shows nothing for this long, then its skeleton: most
/// loads finish well within it, and a flash of placeholders reads as
/// flicker.
const SKELETON_DELAY: Duration = Duration::from_millis(120);

/// Whether a first load that started at `started` has run long enough to
/// show its skeleton (else a repaint is scheduled for when it will have).
pub(crate) fn skeleton_due(ui: &egui::Ui, started: Instant) -> bool {
    if debug_flag("WITNESS_DEBUG_LOADING") {
        return true;
    }
    let due = started + SKELETON_DELAY;
    let now = Instant::now();
    if now >= due {
        true
    } else {
        ui.ctx().request_repaint_after(due - now);
        false
    }
}

/// A designed empty state: a large muted Phosphor icon, a title, one
/// helpful sentence and up to two buttons (a primary and a ghost one).
/// `compact` is the inline variant (an emptied recycle bin). Returns the
/// index of the clicked button.
pub(crate) fn empty_hero(
    ui: &mut egui::Ui,
    glyph: &str,
    title: &str,
    body: &str,
    buttons: &[&str],
    compact: bool,
) -> Option<usize> {
    let (pad, icon, title_size) = if compact {
        (theme::S4, 28.0, 14.0)
    } else {
        (48.0, 44.0, 16.0)
    };
    let mut clicked = None;
    ui.add_space(pad);
    ui.vertical_centered(|ui| {
        // Sentences read best at a measure, not the column's full width.
        let measure = ui.available_width().min(340.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(icons::icon(glyph, icon).color(pal().text_faint));
        ui.add_space(if compact { theme::S2 } else { theme::S3 });
        ui.scope(|ui| {
            ui.set_max_width(measure);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(title)
                        .font(theme::semibold(title_size))
                        .color(pal().text),
                )
                .wrap(),
            );
            ui.add_space(theme::S1 + 2.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(body)
                        .font(theme::regular(13.0))
                        .color(pal().text_muted),
                )
                .wrap(),
            );
        });
        if !buttons.is_empty() {
            ui.add_space(theme::S4);
            // The centred layout centres a block of the row's exact width.
            let size = egui::vec2(
                hero_buttons_width(ui, buttons).min(ui.available_width()),
                ui.spacing().interact_size.y,
            );
            ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = theme::S2;
                for (index, label) in buttons.iter().enumerate() {
                    let response = if index == 0 {
                        widgets::btn_primary(ui, *label)
                    } else {
                        widgets::btn_ghost(ui, *label)
                    };
                    follow_focus(&response);
                    if response.clicked() {
                        clicked = Some(index);
                    }
                }
            });
        }
    });
    ui.add_space(pad);
    clicked
}

/// The width of [`empty_hero`]'s button row (labels + button padding +
/// gaps), for centring it.
fn hero_buttons_width(ui: &egui::Ui, buttons: &[&str]) -> f32 {
    let pad = 2.0 * ui.spacing().button_padding.x;
    let widths: f32 = buttons
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let font = if index == 0 {
                theme::semibold(theme::BASE_SIZE)
            } else {
                theme::regular(theme::BASE_SIZE)
            };
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font, pal().text)
                .size()
                .x
                + pad
        })
        .sum();
    widths + theme::S2 * buttons.len().saturating_sub(1) as f32
}

/// A placeholder bar at an absolute `rect` (a child ui so the kit's
/// [`widgets::skeleton`] can allocate it).
pub(crate) fn skeleton_at(ui: &mut egui::Ui, rect: Rect) {
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect));
    widgets::skeleton(&mut child, rect.size());
}

/// Debug builds only: whether the env var `name` is `1` (passive snapshot
/// hooks such as `WITNESS_DEBUG_LOADING`, which pins every first-load
/// skeleton).
pub(crate) fn debug_flag(name: &str) -> bool {
    debug_value(name).is_some_and(|v| v == "1")
}

/// Debug builds only: the env var `name`'s value.
pub(crate) fn debug_value(name: &str) -> Option<String> {
    #[cfg(debug_assertions)]
    {
        std::env::var(name).ok()
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
        None
    }
}

/// Debug builds only: `name` as an index (`WITNESS_DEBUG_SELECT=<n>`, …).
pub(crate) fn debug_index(name: &str) -> Option<usize> {
    debug_value(name).and_then(|v| v.trim().parse().ok())
}

/// Fetch `count` rows from `offset` with `fetch(offset, limit)`, in chunks
/// of at most [`BACKEND_MAX`]; a short chunk means the end of the list.
fn fetch_chunked<T, E>(
    offset: usize,
    count: usize,
    mut fetch: impl FnMut(usize, usize) -> Result<Vec<T>, E>,
) -> Result<Vec<T>, E> {
    let mut rows = Vec::new();
    while rows.len() < count {
        let want = (count - rows.len()).min(BACKEND_MAX);
        let chunk = fetch(offset + rows.len(), want)?;
        let short = chunk.len() < want;
        rows.extend(chunk);
        if short {
            break;
        }
    }
    Ok(rows)
}

/// Paint `glyph` centred (on its ink box) in `rect`.
fn paint_glyph(ui: &egui::Ui, rect: Rect, glyph: &str, color: Color32) {
    icons::paint_rotated_glyph(ui.painter(), rect.center(), glyph, ICON, color, 0.0);
}

/// Side of the square `widgets::icon_btn` box for a 16 pt glyph (the
/// kit's formula, so status icons and buttons line up).
fn icon_slot(ui: &egui::Ui) -> f32 {
    let galley =
        ui.painter()
            .layout_no_wrap(ph::TRASH.to_owned(), theme::regular(ICON), pal().text);
    (galley.size().x.max(ICON) + 12.0).max(28.0)
}

/// Vertical metrics of a list row.
#[derive(Debug, Clone, Copy)]
struct RowMetrics {
    /// Height of the title line: the title's own row, or the rename input
    /// while editing.
    line: f32,
    title: f32,
    /// The rename input's height (body row + padding + border). Rows are
    /// not sized for it: the one row being edited spends that extra ~8 pt
    /// of its padding.
    edit: f32,
    /// Height of the (single-line) meta line.
    meta: f32,
    /// Outer row height.
    height: f32,
    /// Icon button side.
    slot: f32,
}

impl RowMetrics {
    fn measure(ui: &egui::Ui) -> RowMetrics {
        let (title, body, meta) = ui.ctx().fonts_mut(|f| {
            (
                f.row_height(&theme::semibold(14.0)),
                f.row_height(&theme::regular(14.0)),
                f.row_height(&theme::regular(12.0)),
            )
        });
        let slot = icon_slot(ui);
        let edit = body + f32::from(EDIT_MARGIN.top + EDIT_MARGIN.bottom) + 2.0;
        let content = (title + TITLE_META_GAP + meta).max(slot);
        RowMetrics {
            line: title,
            title,
            edit,
            meta,
            height: content + 2.0 * ROW_PAD.y,
            slot,
        }
    }

    /// The metrics of the row being renamed: same outer height, the title
    /// line as tall as the editor.
    fn editing(self) -> RowMetrics {
        RowMetrics {
            line: self.edit.max(self.title),
            ..self
        }
    }
}

/// Where a row's parts go: the title block on the left; from the right the
/// status (when shown), delete and rename. The block only gives way to the
/// actions while they are revealed, so idle titles use the whole row.
struct RowRects {
    main: Rect,
    status: Option<Rect>,
    pencil: Rect,
    delete: Rect,
}

impl RowRects {
    fn new(rect: Rect, metrics: RowMetrics, status: bool, revealed: bool) -> RowRects {
        let inner = rect.shrink2(ROW_PAD);
        let slot = metrics.slot;
        let slot_rect = |n: f32| {
            let right = inner.right() - n * (slot + COL_GAP);
            Rect::from_center_size(
                egui::pos2(right - slot / 2.0, inner.center().y),
                egui::vec2(slot, slot),
            )
        };
        let mut used = 0.0;
        let status = status.then(|| {
            used += 1.0;
            slot_rect(0.0)
        });
        let delete = slot_rect(used);
        let pencil = slot_rect(used + 1.0);
        if revealed {
            used += 2.0;
        }
        let trailing = if used > 0.0 {
            used * slot + (used - 1.0) * COL_GAP + MAIN_GAP
        } else {
            0.0
        };
        let block_height = metrics.line + TITLE_META_GAP + metrics.meta;
        let main = Rect::from_min_size(
            egui::pos2(inner.left(), inner.center().y - block_height / 2.0),
            egui::vec2((inner.width() - trailing).max(1.0), block_height),
        );
        RowRects {
            main,
            status,
            pencil,
            delete,
        }
    }
}

/// One item of the grouped list.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Item {
    /// The day header above the group starting at this meeting index.
    Header(usize),
    Row(usize),
}

/// The grouped list's items with their offsets from the list's top.
#[derive(Debug, Default)]
struct ListLayout {
    /// (item, top, height).
    items: Vec<(Item, f32, f32)>,
    height: f32,
}

/// Lay out rows `row_height` tall under day headers: a header wherever the
/// day changes, the first `first_header` tall, later ones `header` tall
/// (they carry the gap between groups). Rows are [`ROW_GAP`] apart.
fn list_layout(
    days: &[Option<NaiveDate>],
    row_height: f32,
    first_header: f32,
    header: f32,
) -> ListLayout {
    let mut layout = ListLayout::default();
    let mut y = 0.0;
    let mut prev_row = false;
    for (index, day) in days.iter().enumerate() {
        if index == 0 || days[index - 1] != *day {
            let h = if index == 0 { first_header } else { header };
            layout.items.push((Item::Header(index), y, h));
            y += h;
            prev_row = false;
        }
        if prev_row {
            y += ROW_GAP;
        }
        layout.items.push((Item::Row(index), y, row_height));
        y += row_height;
        prev_row = true;
    }
    layout.height = y;
    layout
}

/// Heights of the first and the later day headers: the kit header (its own
/// space above + the 12 pt label) plus the gap to the first row; later
/// headers add the space between groups.
fn header_heights(ui: &egui::Ui) -> (f32, f32) {
    let label = ui.ctx().fonts_mut(|f| f.row_height(&theme::semibold(12.0)));
    let first = theme::S2 + label + HEADER_BELOW;
    (first, first + theme::S3)
}

/// The inline rename editor (one at a time).
struct Editing {
    id: i64,
    /// The title when editing started (only a changed title is saved).
    original: String,
    text: String,
    /// Take focus on the next draw — once, not every frame.
    focus_pending: bool,
    /// Escape was pressed: the coming focus loss cancels instead of
    /// committing (egui may report the loss a frame after the key).
    escaped: bool,
}

/// A list (re)load in flight. A newer request replaces (and so discards) it.
struct ListLoad {
    task: Task<CoreResult<Vec<Meeting>>>,
    append: bool,
    limit: usize,
    /// When it was requested (a first load's skeleton waits a moment).
    started: Instant,
}

/// A keyboard-selectable item: a meeting of the list or a recycle-bin row
/// (while the bin is open), in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sel {
    Meeting(i64),
    Binned(i64),
}

/// What the empty library offers, refreshed from the shell every pass.
#[derive(Debug, Default, Clone)]
struct EmptyEnv {
    auto_record: bool,
    /// A recording is running or starting (no "Start recording" then).
    recording: bool,
    /// The bound recording hotkey (`ctrl+alt+r`), if any.
    record_hotkey: Option<String>,
}

/// What the open confirmation is for.
enum ConfirmFor {
    Delete { id: i64 },
    Purge { id: i64 },
    EmptyBin,
}

/// Something the user asked for during the pass; applied after drawing.
#[derive(Debug, Clone, PartialEq)]
enum RowAction {
    /// Keyboard (Enter/Space) or accessibility activation: open now.
    OpenNow(i64),
    /// Pointer click: open after [`OPEN_DELAY`] unless a double-click follows.
    OpenLater(i64),
    StartEdit {
        id: i64,
        title: String,
    },
    /// Commit / cancel the editor of this meeting — ignored once another
    /// row's editor has replaced it.
    CommitEdit(i64),
    CancelEdit(i64),
    Retranscribe(i64),
    Delete {
        id: i64,
        title: String,
        skip: bool,
    },
    Restore(i64),
    Purge {
        id: i64,
        title: String,
        skip: bool,
    },
    ToggleBin,
    EmptyBin {
        skip: bool,
    },
    LoadMore,
    TryAgain,
    /// The empty library's "Start recording" (the top bar's record path).
    StartRecording,
    /// The empty library's way to the auto-record setting.
    OpenSettings,
}

pub struct MeetingsView {
    meetings: Vec<Meeting>,
    has_more: bool,
    /// Shown only while the list is empty; a populated list keeps its
    /// stale rows and the error is toast-only.
    load_error: Option<String>,
    list_load: Option<ListLoad>,
    binned: Vec<DeletedMeeting>,
    bin_load: Option<Task<CoreResult<Vec<DeletedMeeting>>>>,
    /// Collapsed again on every remount (parity).
    bin_open: bool,
    /// Debug snapshots: scroll the (debug-opened) bin into view once.
    scroll_to_bin: bool,
    editing: Option<Editing>,
    pending_open: Option<(i64, Instant)>,
    confirm_for: Option<ConfirmFor>,
    /// The row holding keyboard focus last pass: it is drawn even when
    /// scrolled out of view, since egui drops the focus of an undrawn widget.
    focused_row: Option<i64>,
    /// Each meeting's local start day (its group), parsed once per load.
    /// None after the rows change.
    days: Option<Vec<Option<NaiveDate>>>,
    /// The `shell.refresh_tick` the data was requested for; None until the
    /// first pass, so mounting loads.
    seen_tick: Option<u64>,
    /// The keyboard selection highlight (↑/↓, j/k, Home/End; Enter opens).
    /// Follows clicks, keyboard focus and the meeting open beside the list.
    cursor: Option<Sel>,
    /// Scroll this item into view on the next draw.
    reveal: Option<Sel>,
    /// The meeting open in a split's detail pane (`PaneRole::List`).
    pane_open: Option<i64>,
    /// `pane_open` as last seen, so only a change moves the cursor.
    pane_seen: Option<i64>,
    env: EmptyEnv,
    /// `WITNESS_DEBUG_SELECT` has been applied (once, after the first load).
    debug_selected: bool,
}

impl Default for MeetingsView {
    fn default() -> Self {
        Self::new()
    }
}

impl MeetingsView {
    pub fn new() -> Self {
        MeetingsView {
            meetings: Vec::new(),
            has_more: false,
            load_error: None,
            list_load: None,
            binned: Vec::new(),
            bin_load: None,
            bin_open: debug_bin_open(),
            scroll_to_bin: debug_bin_open(),
            editing: None,
            pending_open: None,
            confirm_for: None,
            focused_row: None,
            days: None,
            seen_tick: None,
            cursor: None,
            reveal: None,
            pane_open: None,
            pane_seen: None,
            env: EmptyEnv::default(),
            debug_selected: false,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if self.seen_tick != Some(cx.shell.refresh_tick) {
            self.seen_tick = Some(cx.shell.refresh_tick);
            self.reload(cx);
            self.load_bin(cx);
        }
        self.poll(ui, cx);
        self.debug_states();
        self.fire_pending_open(ui, cx);
        self.take_confirm_answer(cx);
        self.env = EmptyEnv {
            auto_record: cx.settings.current().auto_record,
            recording: cx.shell.recording() || cx.shell.recording_action,
            record_hotkey: cx.shell.record_hotkey.clone(),
        };
        let open = self.pane_selection(cx.pane);
        self.follow_pane(open);

        // The list keys go before the scroller, which would otherwise
        // scroll by the arrows / Home / End.
        let mut actions = Vec::new();
        if list_keys_free(ui.ctx(), cx.confirm.is_open()) && !self.nav_items().is_empty() {
            let keys = read_list_keys(ui.ctx(), false, true);
            actions.extend(self.key_actions(keys));
        }
        widgets::scroll_column(ui, "meetings-scroll", KeyScroll { space: true }, |ui| {
            self.draw(ui, &mut actions)
        });
        for action in actions {
            self.apply(action, ui, cx);
        }
    }

    /// Commit an open rename: egui keeps the editor's focus when the window
    /// hides or the app quits, where the web input would blur and commit.
    pub fn flush(&mut self, cx: &mut UiCtx) {
        if let Some((id, title)) = self.take_edit() {
            rename(cx, id, title);
        }
    }

    /// Leaving the list drops a pending single-click open (the TSX timer
    /// is cleared on unmount) and a pending search-box debounce, which
    /// would otherwise fire on a later visit.
    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
        self.pending_open = None;
        cx.memory.search_box.debounce_at = None;
    }

    // ------------------------------------------------------------------
    // Loads

    /// `loading` in meetings.tsx: a list request is in flight.
    fn loading(&self) -> bool {
        self.list_load.is_some()
    }

    /// `load(offset, append, limit)`: asks for `limit + 1` rows to learn
    /// whether there is another page (chunked past the backend's cap, so a
    /// reload of 500+ rows keeps them all, and keeps Load more).
    fn load(&mut self, cx: &UiCtx, offset: usize, append: bool, limit: usize) {
        let task = cx.tasks.spawn("list-meetings", move |c| {
            fetch_chunked(offset, limit + 1, |offset, limit| {
                c.list_meetings(offset as i64, limit as i64)
            })
        });
        self.list_load = Some(ListLoad {
            task,
            append,
            limit,
            started: Instant::now(),
        });
    }

    /// Re-request everything already loaded, so background refreshes and
    /// Back don't reset pagination to page 1.
    fn reload(&mut self, cx: &UiCtx) {
        let limit = PAGE.max(cx.memory.meetings_loaded);
        self.load(cx, 0, false, limit);
    }

    fn load_bin(&mut self, cx: &UiCtx) {
        self.bin_load = Some(
            cx.tasks
                .spawn("list-deleted-meetings", |c| c.list_deleted_meetings()),
        );
    }

    /// Take finished loads.
    fn poll(&mut self, ui: &egui::Ui, cx: &mut UiCtx) {
        let finished = self
            .list_load
            .as_mut()
            .and_then(|load| load.task.poll_flat());
        if let Some(result) = finished {
            let ListLoad { append, limit, .. } = self.list_load.take().expect("load in flight");
            match result {
                Ok(mut rows) => {
                    self.has_more = rows.len() > limit;
                    rows.truncate(limit);
                    cx.fonts
                        .note_texts(ui.ctx(), rows.iter().map(|m| m.title.as_str()));
                    if append {
                        self.meetings.extend(rows);
                    } else {
                        self.meetings = rows;
                    }
                    self.days = None;
                    self.load_error = None;
                    // The editor goes away with its row.
                    let editing_gone = self
                        .editing
                        .as_ref()
                        .is_some_and(|e| !self.meetings.iter().any(|m| m.id == e.id));
                    if editing_gone {
                        self.editing = None;
                    }
                }
                Err(error) => {
                    cx.toasts.error("Could not load meetings", &error);
                    self.load_error = Some(error);
                }
            }
            if !self.meetings.is_empty() {
                cx.memory.meetings_loaded = self.meetings.len();
            }
        }

        let finished = self.bin_load.as_mut().and_then(|task| task.poll_flat());
        if let Some(result) = finished {
            self.bin_load = None;
            match result {
                Ok(rows) => {
                    cx.fonts
                        .note_texts(ui.ctx(), rows.iter().map(|d| d.meeting.title.as_str()));
                    self.binned = rows;
                }
                Err(error) => cx.toasts.error("Could not load the recycle bin", error),
            }
        }
    }

    // ------------------------------------------------------------------
    // Selection

    /// The meeting open beside the list: a split's `PaneRole::List`
    /// selection (debug builds: `WITNESS_DEBUG_PANE=<row index>` fakes one).
    fn pane_selection(&self, pane: PaneRole) -> Option<i64> {
        if let Some(index) = debug_index("WITNESS_DEBUG_PANE") {
            return self.meetings.get(index).map(|m| m.id);
        }
        match pane {
            PaneRole::List { selected } => selected,
            PaneRole::Full | PaneRole::Detail => None,
        }
    }

    /// Track the meeting open beside the list: when it changes (the shell
    /// opened another), the keyboard selection moves to it and it scrolls
    /// into view — once it is loaded.
    fn follow_pane(&mut self, open: Option<i64>) {
        self.pane_open = open;
        if open != self.pane_seen {
            self.pane_seen = open;
            if let Some(id) = open {
                self.cursor = Some(Sel::Meeting(id));
                self.reveal = self.cursor;
            }
        }
    }

    /// The keyboard-selectable items in display order: the meetings, then
    /// the recycle-bin rows while the bin is open.
    fn nav_items(&self) -> Vec<Sel> {
        let mut items: Vec<Sel> = self.meetings.iter().map(|m| Sel::Meeting(m.id)).collect();
        if self.bin_open {
            items.extend(self.binned.iter().map(|d| Sel::Binned(d.meeting.id)));
        }
        items
    }

    /// Apply the list keys to the selection; returns what they ask for:
    /// Enter opens a meeting (restores a binned one), Delete moves it to the
    /// bin (deletes a binned one for good) after the usual confirmation, F2
    /// renames. Keys on a selection that is no longer listed do nothing.
    fn key_actions(&mut self, keys: ListKeys) -> Vec<RowAction> {
        let items = self.nav_items();
        if let Some(motion) = keys.motion {
            self.cursor = step_selection(&items, self.cursor.as_ref(), motion);
            self.reveal = self.cursor;
        }
        let Some(sel) = self.cursor.filter(|sel| items.contains(sel)) else {
            return Vec::new();
        };
        let title = |id: i64| {
            match sel {
                Sel::Meeting(_) => self.meetings.iter().find(|m| m.id == id),
                Sel::Binned(_) => self.binned.iter().map(|d| &d.meeting).find(|m| m.id == id),
            }
            .map(|m| m.title.clone())
            .unwrap_or_default()
        };
        let mut actions = Vec::new();
        match sel {
            Sel::Meeting(id) => {
                if keys.activate {
                    actions.push(RowAction::OpenNow(id));
                }
                if let Some(skip) = keys.delete {
                    let title = title(id);
                    actions.push(RowAction::Delete { id, title, skip });
                }
                if keys.rename {
                    let title = title(id);
                    actions.push(RowAction::StartEdit { id, title });
                }
            }
            Sel::Binned(id) => {
                if keys.activate {
                    actions.push(RowAction::Restore(id));
                }
                if let Some(skip) = keys.delete {
                    let title = title(id);
                    actions.push(RowAction::Purge { id, title, skip });
                }
            }
        }
        actions
    }

    /// Debug builds only, for passive snapshots: `WITNESS_DEBUG_EMPTY=1`
    /// shows an empty library (`=bin`: just an empty recycle bin; with
    /// `WITNESS_DEBUG_BIN=1` it shows open) and `WITNESS_DEBUG_SELECT=<n>`
    /// puts the keyboard selection on the n-th meeting once the list loads
    /// (`=bin:<n>`: on the n-th recycle-bin row).
    fn debug_states(&mut self) {
        match debug_value("WITNESS_DEBUG_EMPTY").as_deref() {
            Some("1") => {
                self.meetings.clear();
                self.binned.clear();
                self.has_more = false;
                self.days = None;
            }
            Some("bin") => self.binned.clear(),
            _ => {}
        }
        if self.debug_selected || self.meetings.is_empty() || self.bin_load.is_some() {
            return;
        }
        let Some(spec) = debug_value("WITNESS_DEBUG_SELECT") else {
            return;
        };
        self.debug_selected = true;
        let index = |n: &str| n.trim().parse::<usize>().ok();
        self.cursor = match spec.split_once(':') {
            Some(("bin", n)) => index(n)
                .and_then(|n| self.binned.get(n))
                .map(|d| Sel::Binned(d.meeting.id)),
            _ => index(&spec)
                .and_then(|n| self.meetings.get(n))
                .map(|m| Sel::Meeting(m.id)),
        };
        self.reveal = self.cursor;
    }

    // ------------------------------------------------------------------
    // Actions

    /// Navigate once the single-click delay has passed. Held while a
    /// confirmation is open: navigating would remount this view and drop
    /// the confirmation's answer (closing the modal repaints).
    fn fire_pending_open(&mut self, ui: &egui::Ui, cx: &mut UiCtx) {
        if cx.confirm.is_open() {
            return;
        }
        if let Some((id, at)) = self.pending_open {
            let now = Instant::now();
            if now >= at {
                self.pending_open = None;
                // `apply_navigation` repaints after the pass.
                open_transcript(cx, id, View::Meetings);
            } else {
                ui.ctx().request_repaint_after(at - now);
            }
        }
    }

    fn take_confirm_answer(&mut self, cx: &mut UiCtx) {
        let Some(answer) = cx.confirm.answer(egui::Id::new(CONFIRM_ID)) else {
            return;
        };
        let Some(what) = self.confirm_for.take() else {
            return;
        };
        if answer {
            match what {
                ConfirmFor::Delete { id } => delete(cx, id),
                ConfirmFor::Purge { id } => purge(cx, id),
                ConfirmFor::EmptyBin => empty_bin(cx),
            }
        }
    }

    /// Open the confirmation. A pending single-click open is dropped: the
    /// click on a row button supersedes it.
    fn ask(&mut self, cx: &mut UiCtx, what: ConfirmFor, message: String, ok_label: &str) {
        self.pending_open = None;
        self.confirm_for = Some(what);
        cx.confirm.ask(egui::Id::new(CONFIRM_ID), message, ok_label);
    }

    /// Close the open editor; returns the trimmed title to save when it is
    /// non-empty and changed (an empty one silently reverts). No optimistic
    /// update: the list reloads when the rename returns.
    fn take_edit(&mut self) -> Option<(i64, String)> {
        let editing = self.editing.take()?;
        let title = editing.text.trim().to_owned();
        (!title.is_empty() && title != editing.original).then_some((editing.id, title))
    }

    fn editing_id(&self) -> Option<i64> {
        self.editing.as_ref().map(|e| e.id)
    }

    /// The rename-editor half of [`apply`](Self::apply), free of `UiCtx`
    /// (so it is testable): updates `editing`, returning a title to save.
    fn edit(&mut self, action: RowAction) -> Option<(i64, String)> {
        match action {
            RowAction::StartEdit { id, title } => {
                self.pending_open = None;
                if self.editing_id() == Some(id) {
                    return None;
                }
                // Opening another editor blurs (and so commits) this one.
                let save = self.take_edit();
                self.editing = Some(Editing {
                    id,
                    original: title.clone(),
                    text: title,
                    focus_pending: true,
                    escaped: false,
                });
                save
            }
            // A press on the pencil of a row *above* the editor queues that
            // row's StartEdit before the old editor reports its focus loss
            // (rows push in draw order), so a commit or cancel only applies
            // to the editor it came from.
            RowAction::CommitEdit(id) if self.editing_id() == Some(id) => self.take_edit(),
            RowAction::CancelEdit(id) if self.editing_id() == Some(id) => {
                self.editing = None;
                None
            }
            _ => None,
        }
    }

    fn apply(&mut self, action: RowAction, ui: &egui::Ui, cx: &mut UiCtx) {
        match action {
            RowAction::OpenNow(id) => {
                self.pending_open = None;
                self.cursor = Some(Sel::Meeting(id));
                open_transcript(cx, id, View::Meetings);
            }
            RowAction::OpenLater(id) => {
                self.cursor = Some(Sel::Meeting(id));
                self.pending_open = Some((id, Instant::now() + OPEN_DELAY));
                ui.ctx().request_repaint_after(OPEN_DELAY);
            }
            action @ (RowAction::StartEdit { .. }
            | RowAction::CommitEdit(_)
            | RowAction::CancelEdit(_)) => {
                if let Some((id, title)) = self.edit(action) {
                    rename(cx, id, title);
                }
            }
            RowAction::Retranscribe(id) => retranscribe(cx, id),
            RowAction::Delete { id, title, skip } => {
                if skip {
                    delete(cx, id);
                } else {
                    self.ask(
                        cx,
                        ConfirmFor::Delete { id },
                        format!(
                            "Move \"{title}\" to the recycle bin?\nItems there are permanently deleted after 30 days."
                        ),
                        "Move to bin",
                    );
                }
            }
            RowAction::Restore(id) => restore(cx, id),
            RowAction::Purge { id, title, skip } => {
                if skip {
                    purge(cx, id);
                } else {
                    self.ask(
                        cx,
                        ConfirmFor::Purge { id },
                        format!(
                            "Permanently delete \"{title}\" (audio + transcript)?\nThis cannot be undone."
                        ),
                        "Delete forever",
                    );
                }
            }
            RowAction::ToggleBin => self.bin_open = !self.bin_open,
            RowAction::EmptyBin { skip } => {
                if skip {
                    empty_bin(cx);
                } else {
                    let n = self.binned.len();
                    self.ask(
                        cx,
                        ConfirmFor::EmptyBin,
                        format!(
                            "Permanently delete all {n} meeting(s) in the recycle bin?\nThis cannot be undone."
                        ),
                        "Empty bin",
                    );
                }
            }
            RowAction::LoadMore => {
                let offset = self.meetings.len();
                self.load(cx, offset, true, PAGE);
            }
            // `load(0, false)`: the default page size, not loadedCount.
            RowAction::TryAgain => self.load(cx, 0, false, PAGE),
            // The top bar's path: its pill shows the start, and the new
            // meeting arrives through the usual refresh.
            RowAction::StartRecording => cx.submit_rec(RecCmd::Start(Trigger::Manual)),
            RowAction::OpenSettings => cx.nav.go(View::Settings),
        }
    }

    // ------------------------------------------------------------------
    // Drawing

    fn draw(&mut self, ui: &mut egui::Ui, actions: &mut Vec<RowAction>) {
        // The sr-only "Meetings" heading.
        ui.ctx()
            .accesskit_node_builder(ui.id().with("meetings-heading"), |node| {
                node.set_role(egui::accesskit::Role::Heading);
                node.set_label("Meetings");
            });

        if (self.loading() && self.meetings.is_empty()) || debug_flag("WITNESS_DEBUG_LOADING") {
            let due = self
                .list_load
                .as_ref()
                .is_none_or(|load| skeleton_due(ui, load.started));
            if due {
                list_skeleton(ui);
            }
            return;
        }
        if self.meetings.is_empty() {
            if let Some(error) = &self.load_error {
                let message = format!("Could not load meetings: {error}");
                if widgets::view_error(ui, &message, &["Try again"]).is_some() {
                    actions.push(RowAction::TryAgain);
                }
                return;
            }
            let (body, buttons) = empty_library(&self.env);
            let labels: Vec<&str> = buttons.iter().map(|(label, _)| *label).collect();
            let clicked = empty_hero(ui, ph::WAVEFORM, "No meetings yet", &body, &labels, false);
            if let Some(index) = clicked {
                actions.push(buttons[index].1.clone());
            }
            self.draw_bin(ui, actions);
            return;
        }

        self.draw_list(ui, actions);
        if self.has_more {
            widgets::gap(ui, theme::S2);
            let loading = self.loading();
            let label = if loading { "Loading…" } else { "Load more" };
            if load_more_button(ui, label, !loading).clicked() {
                actions.push(RowAction::LoadMore);
            }
        }
        self.draw_bin(ui, actions);
    }

    /// The grouped list for the current rows (day keys parsed once per load).
    fn layout(&mut self, ui: &egui::Ui, metrics: RowMetrics) -> ListLayout {
        let stale = self
            .days
            .as_ref()
            .is_none_or(|days| days.len() != self.meetings.len());
        if stale {
            self.days = Some(
                self.meetings
                    .iter()
                    .map(|m| local_day(&m.started_at))
                    .collect(),
            );
        }
        let (first, header) = header_heights(ui);
        list_layout(
            self.days.as_deref().unwrap_or_default(),
            metrics.height,
            first,
            header,
        )
    }

    /// The uniform-height list: one allocation; items are drawn only when
    /// they are within a row of the clip rect, and rows also when they hold
    /// the keyboard focus or are being renamed. The extra row either side
    /// gives Tab / Shift+Tab a next row to move to, which [`follow_focus`]
    /// then scrolls into view.
    fn draw_list(&mut self, ui: &mut egui::Ui, actions: &mut Vec<RowAction>) {
        let width = ui.available_width();
        let metrics = RowMetrics::measure(ui);
        let layout = self.layout(ui, metrics);
        let (_, list_rect) = ui.allocate_space(egui::vec2(width, layout.height));
        let band = ui
            .clip_rect()
            .expand2(egui::vec2(0.0, metrics.height + ROW_GAP));
        let keep = self.focused_row.take();
        let today = Local::now().date_naive();
        let days = self.days.as_deref().unwrap_or_default();
        let mut drawn = 0;
        // The current group's header (its first row index and rect): a
        // revealed first row brings its day header into view with it.
        let mut header: Option<(usize, Rect)> = None;
        for &(item, top, height) in &layout.items {
            let rect = Rect::from_min_size(
                egui::pos2(list_rect.left(), list_rect.top() + top),
                egui::vec2(list_rect.width(), height),
            );
            match item {
                Item::Header(first) => {
                    header = Some((first, rect));
                    if !band.intersects(rect) {
                        continue;
                    }
                    drawn += 1;
                    let label = day_label(days.get(first).copied().flatten(), today);
                    day_header_item(ui, rect, first, &label);
                }
                Item::Row(index) => {
                    let meeting = &self.meetings[index];
                    let sel = Sel::Meeting(meeting.id);
                    if self.reveal == Some(sel) {
                        self.reveal = None;
                        let target = match header {
                            Some((first, header)) if first == index => header.union(rect),
                            _ => rect,
                        };
                        ui.scroll_to_rect(target, None);
                    }
                    let editing = self.editing.as_mut().filter(|e| e.id == meeting.id);
                    // The editor's and the focused row are always drawn: a
                    // widget that is not drawn loses its focus (and a rename
                    // would commit).
                    let kept = keep == Some(meeting.id);
                    if editing.is_none() && !kept && !band.intersects(rect) {
                        continue;
                    }
                    drawn += 1;
                    let mark = RowMark {
                        selected: self.cursor == Some(sel),
                        open: self.pane_open == Some(meeting.id),
                        focus_kept: kept,
                    };
                    let focused = meeting_row(ui, rect, metrics, meeting, editing, mark, actions);
                    if focused {
                        self.focused_row = Some(meeting.id);
                        // Keyboard focus (Tab, egui's arrow moves) carries
                        // the selection with it.
                        self.cursor = Some(sel);
                    }
                }
            }
        }
        // A meeting to reveal that the loaded list doesn't have (the open
        // one is older than the loaded pages) is dropped, so it can't jump
        // the list later on Load more.
        if matches!(self.reveal, Some(Sel::Meeting(_))) && !self.loading() {
            self.reveal = None;
        }
        // Each drawn item took one auto id from `ui`; take the skipped
        // items' too, so the ids of Load more and the bin (and so their
        // keyboard focus) don't depend on how far the list is scrolled.
        ui.skip_ahead_auto_ids(layout.items.len() - drawn);
    }

    /// The recycle bin: shown while it has meetings, and while open (so
    /// emptying it shows its empty state rather than whisking it away).
    fn draw_bin(&mut self, ui: &mut egui::Ui, actions: &mut Vec<RowAction>) {
        if self.binned.is_empty() && !self.bin_open {
            return;
        }
        widgets::gap(ui, theme::S5);
        let open = self.bin_open;
        let count = self.binned.len();
        let can_empty = open && count > 0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S2;
            let empty_width = if can_empty {
                empty_bin_width(ui) + ui.spacing().item_spacing.x
            } else {
                0.0
            };
            let head_width = (ui.available_width() - empty_width).max(60.0);
            let head = bin_head(ui, head_width, count, open);
            follow_focus(&head);
            if std::mem::take(&mut self.scroll_to_bin) {
                head.scroll_to_me(Some(Align::TOP));
            }
            if head.clicked() {
                actions.push(RowAction::ToggleBin);
            }
            if can_empty {
                let label = egui::RichText::new("Empty bin")
                    .size(13.0)
                    .color(pal().text_muted);
                let button = widgets::btn_ghost(ui, label);
                follow_focus(&button);
                if button.clicked() {
                    let skip = ui.input(|i| i.modifiers.shift);
                    actions.push(RowAction::EmptyBin { skip });
                }
            }
        });
        if !open {
            return;
        }
        if count == 0 {
            empty_hero(
                ui,
                ph::TRASH,
                "The recycle bin is empty",
                "Deleted meetings wait here for 30 days before they are removed for good.",
                &[],
                true,
            );
            return;
        }
        // The 30-day rule, said once for every row (aligned with row text).
        widgets::gap(ui, theme::S1);
        egui::Frame::new()
            .inner_margin(Margin {
                left: ROW_PAD.x as i8,
                right: ROW_PAD.x as i8,
                ..Margin::ZERO
            })
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Meetings here are deleted for good after 30 days.")
                            .font(theme::regular(12.0))
                            .color(pal().text_faint),
                    )
                    .wrap(),
                );
            });
        widgets::gap(ui, theme::S2);
        let slot = icon_slot(ui);
        let today = Local::now().date_naive();
        ui.spacing_mut().item_spacing.y = 0.0;
        for (index, deleted) in self.binned.iter().enumerate() {
            if index > 0 {
                ui.add_space(ROW_GAP);
            }
            let sel = Sel::Binned(deleted.meeting.id);
            let selected = self.cursor == Some(sel);
            let row = bin_row(ui, slot, deleted, today, selected, actions);
            if self.reveal == Some(sel) {
                self.reveal = None;
                ui.scroll_to_rect(row.rect, None);
            }
            if row.has_focus {
                self.cursor = Some(sel);
            }
        }
    }
}

/// `WITNESS_DEBUG_BIN=1` (debug builds) mounts the list with the recycle
/// bin open and scrolled into view, so off-screen snapshots can show it
/// without input.
fn debug_bin_open() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var_os("WITNESS_DEBUG_BIN").is_some_and(|v| v == "1")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

/// `ctrl+alt+r` → `Ctrl+Alt+R`.
fn hotkey_text(combo: &str) -> String {
    combo
        .split('+')
        .map(|part| {
            let mut chars = part.trim().chars();
            chars.next().map_or_else(String::new, |first| {
                first
                    .to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect()
            })
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// The empty library's sentence and buttons: how meetings get here (auto-
/// record on or off, the hotkey when one is bound), "Start recording"
/// unless one is already starting, and Settings while auto-record is off.
fn empty_library(env: &EmptyEnv) -> (String, Vec<(&'static str, RowAction)>) {
    let hotkey = env
        .record_hotkey
        .as_deref()
        .map(|combo| format!(" ({})", hotkey_text(combo)))
        .unwrap_or_default();
    let body = if env.auto_record {
        format!(
            "Witness records Teams calls by itself as they begin. To capture anything else, start a recording yourself{hotkey}."
        )
    } else {
        format!(
            "Start a recording yourself{hotkey}, or turn on auto-record in Settings to capture Teams calls as they begin."
        )
    };
    let mut buttons = Vec::new();
    if !env.recording {
        buttons.push(("Start recording", RowAction::StartRecording));
    }
    if !env.auto_record {
        buttons.push(("Open Settings", RowAction::OpenSettings));
    }
    (body, buttons)
}

/// The first load's placeholder: day groups of row-shaped skeletons on the
/// real rows' metrics (a title bar over a meta bar), enough to fill the
/// viewport.
fn list_skeleton(ui: &mut egui::Ui) {
    let metrics = RowMetrics::measure(ui);
    let (first, header) = header_heights(ui);
    let pitch = metrics.height + ROW_GAP;
    let rows = ((ui.clip_rect().height() / pitch).ceil() as usize).clamp(4, 16);
    // Fake days: groups of 2, 4, then the rest, like a working week.
    let days: Vec<Option<NaiveDate>> = (0..rows)
        .map(|row| NaiveDate::from_ymd_opt(2000, 1, 1 + u32::from(row >= 2) + u32::from(row >= 6)))
        .collect();
    let layout = list_layout(&days, metrics.height, first, header);
    let width = ui.available_width();
    let (_, list_rect) = ui.allocate_space(egui::vec2(width, layout.height));
    // Real titles rarely run past ~480 pt, whatever the column's width.
    let measure = (width - 2.0 * ROW_PAD.x).min(480.0);
    let bar = |left: f32, center_y: f32, w: f32, h: f32| {
        Rect::from_min_size(egui::pos2(left, center_y - h / 2.0), egui::vec2(w, h))
    };
    for (n, &(item, top, height)) in layout.items.iter().enumerate() {
        let rect = Rect::from_min_size(
            egui::pos2(list_rect.left(), list_rect.top() + top),
            egui::vec2(list_rect.width(), height),
        );
        let left = rect.left() + ROW_PAD.x;
        match item {
            Item::Header(_) => {
                // The label sits at the header's foot, above the gap.
                let y = rect.bottom() - HEADER_BELOW - 7.0;
                let w = [44.0, 76.0, 68.0][n % 3];
                skeleton_at(ui, bar(left, y, w, 9.0));
            }
            Item::Row(row) => {
                let inner = rect.shrink2(ROW_PAD);
                let block = metrics.line + TITLE_META_GAP + metrics.meta;
                let top = inner.center().y - block / 2.0;
                let title = measure * [0.62, 0.48, 0.71, 0.55, 0.66, 0.44][row % 6];
                let meta: f32 = [120.0, 96.0, 136.0, 108.0][row % 4];
                skeleton_at(ui, bar(left, top + metrics.title / 2.0, title, 12.0));
                let meta_y = top + metrics.line + TITLE_META_GAP + metrics.meta / 2.0;
                skeleton_at(ui, bar(left, meta_y, meta.min(measure), 9.0));
            }
        }
    }
}

// ----------------------------------------------------------------------
// Actions (spawn_op: the toast and the refresh survive navigation)

fn rename(cx: &mut UiCtx, id: i64, title: String) {
    cx.tasks.spawn_op("rename-meeting", move |c| {
        match c.rename_meeting(id, &title) {
            Ok(()) => OpDone::Notice {
                notice: None,
                refresh: true,
            },
            Err(error) => OpDone::Notice {
                notice: Some(Notice::error("Could not rename the meeting", error)),
                refresh: false,
            },
        }
    });
}

fn retranscribe(cx: &mut UiCtx, id: i64) {
    cx.tasks
        .spawn_op("retranscribe", move |c| match c.retranscribe(id, None) {
            Ok(()) => OpDone::Notice {
                notice: Some(Notice::success("Transcription queued")),
                refresh: false,
            },
            Err(error) => OpDone::Notice {
                notice: Some(Notice::error("Could not queue transcription", error)),
                refresh: false,
            },
        });
}

fn delete(cx: &mut UiCtx, id: i64) {
    cx.tasks
        .spawn_op("delete-meeting", move |c| match c.delete_meeting(id) {
            Ok(()) => OpDone::Notice {
                notice: Some(
                    Notice::success("Meeting moved to the recycle bin")
                        .with_action("Undo", ToastAction::RestoreMeeting(id)),
                ),
                refresh: true,
            },
            Err(error) => OpDone::Notice {
                notice: Some(Notice::error(
                    "Could not move the meeting to the recycle bin",
                    error,
                )),
                refresh: false,
            },
        });
}

fn restore(cx: &mut UiCtx, id: i64) {
    cx.tasks
        .spawn_op("restore-meeting", move |c| match c.restore_meeting(id) {
            Ok(()) => OpDone::Notice {
                notice: Some(Notice::success("Meeting restored")),
                refresh: true,
            },
            Err(error) => OpDone::Notice {
                notice: Some(Notice::error("Could not restore the meeting", error)),
                refresh: false,
            },
        });
}

fn purge(cx: &mut UiCtx, id: i64) {
    cx.tasks
        .spawn_op("purge-meeting", move |c| match c.purge_meeting(id) {
            Ok(()) => OpDone::Notice {
                notice: Some(Notice::success("Meeting permanently deleted")),
                refresh: true,
            },
            Err(error) => OpDone::Notice {
                notice: Some(Notice::error(
                    "Could not permanently delete the meeting",
                    error,
                )),
                refresh: false,
            },
        });
}

fn empty_bin(cx: &mut UiCtx) {
    cx.tasks
        .spawn_op("empty-recycle-bin", |c| match c.empty_recycle_bin() {
            Ok(()) => OpDone::Notice {
                notice: Some(Notice::success("Recycle bin emptied")),
                refresh: true,
            },
            // Purging stops at the first error: reload to show what is left.
            Err(error) => OpDone::Notice {
                notice: Some(Notice::error("Could not empty the recycle bin", error)),
                refresh: true,
            },
        });
}

// ----------------------------------------------------------------------
// Row widgets

/// A day header ("Today", "Thu 23 Jul") at `rect`, aligned with the row
/// text. One child of `ui` with an id of its own (see `draw_list`).
fn day_header_item(ui: &mut egui::Ui, rect: Rect, first: usize, label: &str) {
    let mut header = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.with_min_x(rect.left() + ROW_PAD.x))
            .id(ui.id().with(("day-header", first)))
            .layout(Layout::top_down(Align::Min)),
    );
    header.spacing_mut().item_spacing.y = 0.0;
    if first > 0 {
        // The space between two day groups.
        header.add_space(theme::S3);
    }
    widgets::day_header(&mut header, label);
}

/// How a meeting row is marked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RowMark {
    /// The keyboard selection: the selected-row fill.
    selected: bool,
    /// Open in the split's detail pane: the fill plus the accent pill.
    open: bool,
    /// One of its widgets held the keyboard focus last pass.
    focus_kept: bool,
}

impl RowMark {
    /// The row's fill: selected (or open) rows only. Hover shows as a
    /// neutral pill at the left edge instead ([`widgets::paint_hover_pill`]).
    fn fill(self) -> Option<Color32> {
        (self.selected || self.open).then(|| pal().bg_active)
    }
}

/// One meeting row at `rect` (uniform height): a flat row that fills on
/// hover, and stays filled while selected or open beside the list. The
/// whole row opens the meeting; rename and delete show while the row is
/// hovered, holds the keyboard focus (`mark.focus_kept`: it did last pass)
/// or is being renamed — they stay in the Tab order when hidden. True when
/// one of its widgets has the keyboard focus.
fn meeting_row(
    ui: &mut egui::Ui,
    rect: Rect,
    metrics: RowMetrics,
    m: &Meeting,
    editing: Option<&mut Editing>,
    mark: RowMark,
    actions: &mut Vec<RowAction>,
) -> bool {
    let focus_kept = mark.focus_kept;
    // An id that doesn't depend on how many rows were drawn before this one:
    // an auto id would change on scrolling, and egui would drop the focus
    // of the row's buttons.
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .id(ui.id().with(("meeting-row", m.id)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    // The row's own click target goes in first, so the buttons on top of
    // it win the pointer (egui hit-tests the last-added widget first).
    let open = row.interact(rect, egui::Id::new(("meeting-open", m.id)), Sense::click());
    // Hover also counts while a child button is hovered.
    let hovered = row.rect_contains_pointer(rect);
    let is_editing = editing.is_some();
    let ring = widgets::focus_visible(row.ctx());
    let revealed = hovered || is_editing || (focus_kept && ring);
    if let Some(fill) = mark.fill() {
        row.painter()
            .rect_filled(rect, CornerRadius::same(theme::R_MD), fill);
    }
    if mark.open {
        paint_open_pill(&row, rect);
    } else if hovered && !mark.selected {
        widgets::paint_hover_pill(&row, rect);
    }

    let metrics = if is_editing {
        metrics.editing()
    } else {
        metrics
    };
    let RowRects {
        main: main_rect,
        status: status_rect,
        pencil: pencil_rect,
        delete: delete_rect,
    } = RowRects::new(rect, metrics, shows_status(&m.status), revealed);

    // 1. The title block (the row button), or the rename editor.
    let mut focused = open_row(&row, &open, rect, main_rect, metrics, m, actions);
    if let Some(editing) = editing {
        focused |= rename_editor(
            &mut row,
            main_rect,
            pencil_rect,
            metrics,
            m,
            editing,
            actions,
        );
    }

    // 2. Rename / Done, then delete (move to the recycle bin): painted
    // only while revealed, but always present for Tab and screen readers.
    let mut hidden_focus = false;
    let mut pencil_ui = row.new_child(UiBuilder::new().max_rect(pencil_rect));
    let mut delete_ui = row.new_child(UiBuilder::new().max_rect(delete_rect));
    if !revealed {
        pencil_ui.set_opacity(0.0);
        delete_ui.set_opacity(0.0);
    }
    let (glyph, tooltip, name) = if is_editing {
        (ph::CHECK, "Done", format!("Save {}", m.title))
    } else {
        (ph::PENCIL_SIMPLE, "Rename", format!("Rename {}", m.title))
    };
    let pencil = widgets::icon_btn(&mut pencil_ui, glyph, ICON, tooltip);
    pencil.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    follow_focus(&pencil);
    focused |= pencil.has_focus();
    hidden_focus |= pencil.has_focus();
    if pencil.clicked() {
        actions.push(if is_editing {
            RowAction::CommitEdit(m.id)
        } else {
            RowAction::StartEdit {
                id: m.id,
                title: m.title.clone(),
            }
        });
    }

    let delete = widgets::icon_btn_danger(&mut delete_ui, ph::TRASH, ICON, "Delete");
    let name = format!("Move {} to the recycle bin", m.title);
    delete.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    follow_focus(&delete);
    focused |= delete.has_focus();
    hidden_focus |= delete.has_focus();
    if delete.clicked() {
        actions.push(RowAction::Delete {
            id: m.id,
            title: m.title.clone(),
            skip: delete_ui.input(|i| i.modifiers.shift),
        });
    }
    // Tab reached a hidden action: show it on the next pass.
    if hidden_focus && !revealed {
        row.ctx().request_repaint();
    }

    // 3. Status (non-default states only): a retry button for failed /
    // recorded rows, else the icon.
    if let Some(status_rect) = status_rect {
        let mut status_ui = row.new_child(UiBuilder::new().max_rect(status_rect));
        if m.status == "failed" || m.status == "recorded" {
            let (tooltip, name) = if m.status == "failed" {
                (
                    "Transcription failed — click to retry",
                    format!("Retry transcription of {}", m.title),
                )
            } else {
                (
                    "Not transcribed — click to transcribe",
                    format!("Transcribe {}", m.title),
                )
            };
            let button = status_button(&mut status_ui, status_rect, &m.status, tooltip, &name);
            focused |= button.has_focus();
            if button.clicked() {
                actions.push(RowAction::Retranscribe(m.id));
            }
        } else {
            status_icon(&mut status_ui, status_rect, &m.status);
        }
    }
    focused
}

/// The row button's content and clicks: the title (elided) over the meta
/// line. A pointer click opens after 250 ms (a double-click cancels that
/// and renames); Enter/Space opens at once. While renaming, the editor
/// covers the title and only the meta line is painted here. True while
/// the row button has the keyboard focus.
fn open_row(
    row: &egui::Ui,
    response: &Response,
    rect: Rect,
    main_rect: Rect,
    metrics: RowMetrics,
    m: &Meeting,
    actions: &mut Vec<RowAction>,
) -> bool {
    let editing = metrics.line > metrics.title;
    let mut title_rect = Rect::NOTHING;
    let mut elided = false;
    if !editing {
        let title = truncated(
            row,
            &m.title,
            theme::semibold(14.0),
            pal().text,
            main_rect.width(),
        );
        elided = title.elided;
        let title_top = main_rect.top() + (metrics.line - metrics.title) / 2.0;
        title_rect = Rect::from_min_size(egui::pos2(main_rect.left(), title_top), title.size());
        row.painter().galley(title_rect.min, title, pal().text);
    }
    let meta = truncated(
        row,
        &meta_line(m),
        theme::regular(12.0),
        pal().text_muted,
        main_rect.width(),
    );
    let meta_top = main_rect.top() + metrics.line + TITLE_META_GAP;
    row.painter().galley(
        egui::pos2(main_rect.left(), meta_top),
        meta,
        pal().text_muted,
    );

    let name = format!("Open {}, {}", m.title, status_label(&m.status));
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    follow_focus(response);
    let focused = response.has_focus();
    if focused && widgets::focus_visible(row.ctx()) {
        paint_row_focus(row, rect);
    }
    let response = response.clone();
    // The tooltip sits on the title only, like the TSX `title` attribute
    // (with the whole title first when it had to be cut).
    let on_title = row
        .ctx()
        .pointer_hover_pos()
        .is_some_and(|p| title_rect.contains(p));
    if on_title {
        let tip = if elided {
            format!("{}\nDouble-click to rename", m.title)
        } else {
            "Double-click to rename".to_owned()
        };
        response.clone().on_hover_text(tip);
    }

    if editing {
        return focused;
    }
    if response.double_clicked() {
        actions.push(RowAction::StartEdit {
            id: m.id,
            title: m.title.clone(),
        });
    } else if response.clicked() {
        if !response.clicked_by(PointerButton::Primary) {
            actions.push(RowAction::OpenNow(m.id));
        } else if !response.triple_clicked() {
            actions.push(RowAction::OpenLater(m.id));
        }
    }
    focused
}

/// The inline rename input over the title line. Enter or focus loss
/// commits; Escape cancels. True while it has the focus.
fn rename_editor(
    row: &mut egui::Ui,
    main_rect: Rect,
    pencil_rect: Rect,
    metrics: RowMetrics,
    m: &Meeting,
    editing: &mut Editing,
    actions: &mut Vec<RowAction>,
) -> bool {
    let edit_rect = Rect::from_min_size(main_rect.min, egui::vec2(main_rect.width(), metrics.line));
    let mut edit_ui = row.new_child(
        UiBuilder::new()
            .max_rect(edit_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let frame = egui::Frame::new()
        .fill(pal().bg)
        .stroke(Stroke::new(1.0, pal().accent))
        .corner_radius(CornerRadius::same(theme::R_SM))
        .inner_margin(EDIT_MARGIN);
    if row.input(|i| i.key_pressed(Key::Escape)) {
        editing.escaped = true;
    }
    let response = edit_ui.add(
        egui::TextEdit::singleline(&mut editing.text)
            .id(egui::Id::new(("meeting-rename", m.id)))
            .font(theme::regular(14.0))
            .margin(EDIT_MARGIN)
            .frame(frame)
            .desired_width(f32::INFINITY),
    );
    widgets::text_edit_menu(&response);
    let name = format!("Rename {}", m.title);
    row.ctx().accesskit_node_builder(response.id, |node| {
        node.set_label(name.as_str());
    });

    if std::mem::take(&mut editing.focus_pending) {
        response.request_focus();
    } else if !response.has_focus() {
        // Enter, Escape or a press elsewhere took the focus. A press on
        // this row's pencil is left to the pencil's click (which commits),
        // so the editor doesn't commit here and then reopen — the TSX
        // `onMouseDown={preventDefault}` ordering.
        let on_pencil = row.rect_contains_pointer(pencil_rect)
            && row.input(|i| i.pointer.any_down() || i.pointer.any_released());
        if !on_pencil {
            actions.push(if editing.escaped {
                RowAction::CancelEdit(m.id)
            } else {
                RowAction::CommitEdit(m.id)
            });
        }
    }
    response.has_focus()
}

/// The non-interactive `StatusIcon` (the tooltip carries the words).
/// Recording pulses and processing spins, unless Windows animations are off.
fn status_icon(ui: &mut egui::Ui, rect: Rect, status: &str) -> Response {
    let label = status_label(status).to_owned();
    let response = ui.allocate_rect(rect, Sense::hover());
    let (glyph, color) = status_glyph(status);
    match status {
        "processing" => icons::paint_spinner(ui, rect, ICON, color),
        "recording" => {
            let alpha = icons::pulse_alpha(ui.ctx());
            paint_glyph(ui, rect, glyph, color.gamma_multiply(alpha));
        }
        _ => paint_glyph(ui, rect, glyph, color),
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &label));
    response.on_hover_text(label)
}

/// The failed / recorded status as a button: the status glyph in its
/// colour, turning to the text colour on hover; a click queues a
/// transcription.
fn status_button(
    ui: &mut egui::Ui,
    rect: Rect,
    status: &str,
    tooltip: &str,
    name: &str,
) -> Response {
    let response = ui.allocate_rect(rect, Sense::click());
    let (glyph, color) = status_glyph(status);
    let hot = response.hovered() || response.has_focus();
    if hot {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::R_MD),
            theme::pal().bg_active,
        );
    }
    paint_glyph(ui, rect, glyph, if hot { pal().text } else { color });
    if response.has_focus() && widgets::focus_visible(ui.ctx()) {
        paint_focus_ring(ui, rect, theme::R_MD);
    }
    follow_focus(&response);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, name));
    response.on_hover_text(tooltip)
}

/// A full-width, flat "Load more" (accent text, hover fill), as wide as
/// the rows above it; disabled while a load runs. Shared with Search.
pub(crate) fn load_more_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> Response {
    let color = if enabled {
        pal().accent
    } else {
        pal().text_muted
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::semibold(13.0), color);
    let height = 36.0_f32.max(galley.size().y + 12.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), sense);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let hot = enabled && response.hovered();
        if hot {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(theme::R_MD), pal().bg_hover);
        }
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            paint_row_focus(ui, rect);
        }
    }
    follow_focus(&response);
    if enabled {
        response
    } else {
        response.on_hover_cursor(CursorIcon::NotAllowed)
    }
}

/// Width of the "Empty bin" ghost button (13 pt text + button padding).
fn empty_bin_width(ui: &egui::Ui) -> f32 {
    let galley = ui
        .painter()
        .layout_no_wrap("Empty bin".into(), theme::regular(13.0), pal().text);
    galley.size().x + 2.0 * ui.spacing().button_padding.x
}

/// The "▸ 🗑 Recycle bin  3" disclosure: a flat row-style button, `width`
/// wide, styled like a section header.
fn bin_head(ui: &mut egui::Ui, width: f32, count: usize, open: bool) -> Response {
    let text = "Recycle bin";
    let font = theme::semibold(12.5);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), pal().text_muted);
    let count_galley =
        ui.painter()
            .layout_no_wrap(count.to_string(), theme::regular(12.0), pal().text_faint);
    let height = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    if ui.is_rect_visible(rect) {
        let hot = response.hovered();
        if hot {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(theme::R_MD), pal().bg_hover);
        }
        let color = if hot { pal().text } else { pal().text_muted };
        let cy = rect.center().y;
        let mut x = rect.left() + ROW_PAD.x;
        let caret = if open {
            ph::CARET_DOWN
        } else {
            ph::CARET_RIGHT
        };
        icons::paint_rotated_glyph(
            ui.painter(),
            egui::pos2(x + 6.0, cy),
            caret,
            12.0,
            color,
            0.0,
        );
        x += 12.0 + 6.0;
        icons::paint_rotated_glyph(
            ui.painter(),
            egui::pos2(x + 7.0, cy),
            ph::TRASH,
            14.0,
            color,
            0.0,
        );
        x += 14.0 + 8.0;
        let text_width = galley.size().x;
        ui.painter()
            .galley(egui::pos2(x, cy - galley.size().y / 2.0), galley, color);
        x += text_width + 8.0;
        ui.painter().galley(
            egui::pos2(x, cy - count_galley.size().y / 2.0),
            count_galley,
            pal().text_faint,
        );
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            paint_row_focus(ui, rect);
        }
    }
    let name = format!("{text} ({count})");
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::Button, true, &name);
        info.selected = Some(open); // aria-expanded
        info
    });
    response
}

/// What a drawn recycle-bin row reports.
struct BinRowOut {
    rect: Rect,
    /// Restore or Delete forever holds the keyboard focus.
    has_focus: bool,
}

/// One recycle-bin row: a flat row (hover fill; the selected fill while it
/// has the keyboard selection) with the title and meta elided, then Restore
/// and Delete forever. Not clickable itself.
fn bin_row(
    ui: &mut egui::Ui,
    slot: f32,
    d: &DeletedMeeting,
    today: NaiveDate,
    selected: bool,
    actions: &mut Vec<RowAction>,
) -> BinRowOut {
    let m = &d.meeting;
    let mut has_focus = false;
    let background = ui.painter().add(egui::Shape::Noop);
    let row = egui::Frame::new()
        .inner_margin(Margin::symmetric(ROW_PAD.x as i8, ROW_PAD.y as i8))
        .show(ui, |ui| {
            let width = ui.available_width();
            ui.set_width(width);
            let main_width = (width - 2.0 * slot - COL_GAP - MAIN_GAP).max(40.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.vertical(|ui| {
                    ui.set_width(main_width);
                    ui.spacing_mut().item_spacing.y = TITLE_META_GAP;
                    widgets::elide(ui, &m.title, theme::semibold(14.0), pal().text);
                    widgets::elide(
                        ui,
                        &bin_meta_line(d, today),
                        theme::regular(12.0),
                        pal().text_muted,
                    );
                });
                ui.add_space(MAIN_GAP);
                let restore = widgets::icon_btn(ui, ph::ARROW_COUNTER_CLOCKWISE, ICON, "Restore");
                let name = format!("Restore {}", m.title);
                restore.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
                follow_focus(&restore);
                has_focus |= restore.has_focus();
                if restore.clicked() {
                    actions.push(RowAction::Restore(m.id));
                }
                ui.add_space(COL_GAP);
                let purge = widgets::icon_btn_danger(ui, ph::TRASH, ICON, "Delete forever");
                let name = format!("Permanently delete {}", m.title);
                purge.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
                follow_focus(&purge);
                has_focus |= purge.has_focus();
                if purge.clicked() {
                    actions.push(RowAction::Purge {
                        id: m.id,
                        title: m.title.clone(),
                        skip: ui.input(|i| i.modifiers.shift),
                    });
                }
            });
        })
        .response;
    let mark = RowMark {
        selected,
        ..RowMark::default()
    };
    if let Some(fill) = mark.fill() {
        ui.painter().set(
            background,
            egui::epaint::RectShape::filled(row.rect, CornerRadius::same(theme::R_MD), fill),
        );
    } else if ui.rect_contains_pointer(row.rect) {
        widgets::paint_hover_pill(ui, row.rect);
    }
    BinRowOut {
        rect: row.rect,
        has_focus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meeting(id: i64, status: &str, trigger: &str, duration_ms: Option<i64>) -> Meeting {
        Meeting {
            id,
            started_at: "2026-10-05T09:07:00+01:00".into(),
            ended_at: None,
            title: format!("Meeting {id}"),
            audio_path: None,
            duration_ms,
            status: status.into(),
            engine: None,
            trigger: trigger.into(),
            notes: String::new(),
        }
    }

    fn run(f: impl FnMut(&mut egui::Ui)) {
        let ctx = theme::test_context();
        let mut f = f;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| f(ui));
        // No renderer here to upload the font atlas.
        output.textures_delta.clear();
    }

    #[test]
    fn status_words_match_the_tsx_labels() {
        assert_eq!(status_label("recording"), "Recording");
        assert_eq!(status_label("recorded"), "Recorded — not transcribed yet");
        assert_eq!(status_label("processing"), "Transcribing…");
        assert_eq!(status_label("transcribed"), "Transcribed");
        assert_eq!(status_label("failed"), "Transcription failed");
        assert_eq!(status_label("weird"), "weird");
        assert_eq!(status_glyph("weird").0, ph::CHECK_CIRCLE);
        assert_eq!(status_glyph("recording"), (ph::RECORD, pal().rec));
    }

    #[test]
    fn meta_lines() {
        let auto = meeting(1, "transcribed", "auto", Some(65 * 60 * 1000));
        let line = meta_line(&auto);
        assert!(line.ends_with(" · 1 h 5 min · auto"), "{line}");
        // The time only: the date is the day header's.
        let time = fmt_time(&auto.started_at);
        assert!(line.starts_with(&format!("{time} · ")), "{line}");
        assert_eq!(time.len(), 5, "{time}");
        let manual = meeting(2, "recording", "manual", None);
        assert!(meta_line(&manual).ends_with(" · —"));
        assert_eq!(fmt_time("garbage"), "garbage");

        let binned = DeletedMeeting {
            meeting: manual,
            deleted_at: "2026-10-06T10:00:00+01:00".into(),
        };
        let today = local_day("2026-10-07T12:00:00+01:00").unwrap();
        let line = bin_meta_line(&binned, today);
        assert!(line.contains(" · — · deleted "), "{line}");
        assert!(line.ends_with(" · deleted yesterday"), "{line}");
        assert!(line.starts_with("Mon 5 Oct, "), "{line}");
        let last_year = NaiveDate::from_ymd_opt(2027, 1, 2).unwrap();
        assert!(bin_meta_line(&binned, last_year).starts_with("Mon 5 Oct 2026, "));
    }

    #[test]
    fn day_labels() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
        let day = |m, d| NaiveDate::from_ymd_opt(2026, m, d);
        assert_eq!(day_label(Some(today), today), "Today");
        assert_eq!(day_label(day(10, 6), today), "Yesterday");
        assert_eq!(day_label(day(7, 23), today), "Thu 23 Jul");
        assert_eq!(day_label(day(10, 8), today), "Thu 8 Oct", "future");
        let new_year = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        assert_eq!(day_label(day(12, 31), new_year), "Yesterday");
        assert_eq!(day_label(day(12, 30), new_year), "Wed 30 Dec 2026");
        assert_eq!(day_label(None, today), "Unknown date");
        assert_eq!(in_sentence("Yesterday".into()), "yesterday");
        assert_eq!(in_sentence("Thu 23 Jul".into()), "Thu 23 Jul");
    }

    #[test]
    fn rows_are_grouped_under_day_headers() {
        let d = |n| NaiveDate::from_ymd_opt(2026, 7, n);
        let days = [d(24), d(24), d(23), None, None, d(22)];
        let layout = list_layout(&days, 50.0, 30.0, 42.0);
        let items: Vec<Item> = layout.items.iter().map(|&(item, ..)| item).collect();
        use Item::{Header as H, Row as R};
        assert_eq!(
            items,
            [H(0), R(0), R(1), H(2), R(2), H(3), R(3), R(4), H(5), R(5)]
        );
        // Offsets: a 30 pt first header, rows ROW_GAP apart within a group,
        // later headers 42 pt; the items tile the list exactly.
        let tops: Vec<f32> = layout.items.iter().map(|&(_, top, _)| top).collect();
        assert_eq!(tops[1], 30.0);
        assert_eq!(tops[2], 30.0 + 50.0 + ROW_GAP);
        assert_eq!(tops[3], tops[2] + 50.0);
        assert_eq!(tops[4], tops[3] + 42.0);
        let (_, last_top, last_h) = *layout.items.last().unwrap();
        assert_eq!(layout.height, last_top + last_h);
        assert!(list_layout(&[], 50.0, 30.0, 42.0).items.is_empty());
    }

    /// A fake `list_meetings` over `total` rows that clamps like the backend.
    fn fake_list(
        total: usize,
        calls: &mut Vec<(usize, usize)>,
        offset: usize,
        limit: usize,
    ) -> Vec<usize> {
        calls.push((offset, limit));
        let limit = limit.clamp(1, BACKEND_MAX);
        (offset..(offset + limit).min(total)).collect()
    }

    #[test]
    fn reloads_past_the_backend_cap_are_chunked() {
        // 550 loaded + 1 probe row over 1000 meetings: all 551 arrive, so
        // the list keeps its length and Load more stays.
        let mut calls = Vec::new();
        let rows: Result<Vec<usize>, ()> =
            fetch_chunked(0, 551, |o, l| Ok(fake_list(1000, &mut calls, o, l)));
        let rows = rows.unwrap();
        assert_eq!(rows.len(), 551);
        assert_eq!(rows, (0..551).collect::<Vec<_>>());
        assert_eq!(calls, vec![(0, 500), (500, 51)]);

        // A short chunk ends the fetch (exactly 500 meetings: no Load more).
        let mut calls = Vec::new();
        let rows: Result<Vec<usize>, ()> =
            fetch_chunked(0, 501, |o, l| Ok(fake_list(500, &mut calls, o, l)));
        assert_eq!(rows.unwrap().len(), 500);
        assert_eq!(calls, vec![(0, 500), (500, 1)]);

        let mut calls = Vec::new();
        let rows: Result<Vec<usize>, ()> =
            fetch_chunked(50, 51, |o, l| Ok(fake_list(60, &mut calls, o, l)));
        assert_eq!(rows.unwrap(), (50..60).collect::<Vec<_>>());
        assert_eq!(calls, vec![(50, 51)]);

        let failed: Result<Vec<usize>, &str> = fetch_chunked(0, 10, |_, _| Err("db"));
        assert_eq!(failed, Err("db"));
    }

    #[test]
    fn rows_have_room_for_the_editor_and_icons() {
        run(|ui| {
            let m = RowMetrics::measure(ui);
            assert_eq!(m.line, m.title, "rows are sized for the title line");
            assert!(m.height >= m.slot + 2.0 * ROW_PAD.y);
            assert!(m.height >= m.line + TITLE_META_GAP + m.meta + 2.0 * ROW_PAD.y);
            // A compact flat row: 8 + title + 2 + meta + 8, at least the
            // 44 pt minimum row height of the design.
            assert!((44.0..56.0).contains(&m.height), "row height {}", m.height);
            // The row being renamed keeps its height and fits its editor
            // inside the border.
            let e = m.editing();
            assert_eq!(e.height, m.height);
            assert!(e.line >= m.edit);
            let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(420.0, m.height));
            let main = RowRects::new(rect, e, false, true).main;
            assert!(main.top() >= 1.0 && main.bottom() <= m.height - 1.0);
            assert_eq!(main.height(), e.line + TITLE_META_GAP + e.meta);

            // Idle rows give the title the whole width; revealed actions
            // (and a status) take theirs from the right, never overlapping.
            let idle = RowRects::new(rect, m, false, false);
            assert_eq!(idle.main.width(), 420.0 - 2.0 * ROW_PAD.x);
            assert!(idle.status.is_none());
            let busy = RowRects::new(rect, m, true, true);
            let status = busy.status.unwrap();
            assert!(busy.main.right() < busy.pencil.left());
            assert!(busy.pencil.right() <= busy.delete.left());
            assert!(busy.delete.right() <= status.left());
            assert!(status.right() <= rect.right() - ROW_PAD.x + 0.01);
            // The buttons don't move when they are revealed.
            let hidden = RowRects::new(rect, m, true, false);
            assert_eq!(hidden.pencil, busy.pencil);
            assert!(hidden.main.width() > busy.main.width());
        });
    }

    #[test]
    fn rows_lay_out_in_every_state() {
        run(|ui| {
            let metrics = RowMetrics::measure(ui);
            let mut actions = Vec::new();
            let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(420.0, metrics.height));
            for (id, status) in [
                "recording",
                "recorded",
                "processing",
                "transcribed",
                "failed",
            ]
            .into_iter()
            .enumerate()
            {
                let m = meeting(10 + id as i64, status, "auto", Some(1000));
                meeting_row(
                    ui,
                    rect,
                    metrics,
                    &m,
                    None,
                    RowMark::default(),
                    &mut actions,
                );
            }
            let m = meeting(4, "transcribed", "manual", None);
            let mut editing = Editing {
                id: 4,
                original: m.title.clone(),
                text: "New".into(),
                focus_pending: true,
                escaped: false,
            };
            meeting_row(
                ui,
                rect,
                metrics,
                &m,
                Some(&mut editing),
                RowMark {
                    selected: true,
                    open: true,
                    focus_kept: true,
                },
                &mut actions,
            );
            assert!(!editing.focus_pending, "focus is requested once");
            let binned = DeletedMeeting {
                meeting: m,
                deleted_at: "2026-10-06T10:00:00+01:00".into(),
            };
            let today = Local::now().date_naive();
            bin_row(ui, metrics.slot, &binned, today, true, &mut actions);
            bin_head(ui, 300.0, 3, true);
            assert!(actions.is_empty(), "no input, no actions");
        });
    }

    // ------------------------------------------------------------------
    // The list driven pass by pass, as `show` draws it.

    fn list_view(n: i64) -> MeetingsView {
        let mut view = MeetingsView::new();
        view.meetings = (1..=n)
            .map(|id| meeting(id, "transcribed", "manual", Some(60_000)))
            .collect();
        view
    }

    /// Where the list starts and how its items are laid out, as last drawn.
    struct ListGeometry {
        origin: egui::Pos2,
        width: f32,
        metrics: RowMetrics,
        layout: ListLayout,
    }

    impl ListGeometry {
        /// Row `index`'s parts (the button positions don't depend on
        /// whether the actions are revealed).
        fn row(&self, index: usize) -> RowRects {
            let &(_, top, height) = self
                .layout
                .items
                .iter()
                .find(|(item, ..)| *item == Item::Row(index))
                .expect("row laid out");
            let rect = Rect::from_min_size(
                self.origin + egui::vec2(0.0, top),
                egui::vec2(self.width, height),
            );
            RowRects::new(rect, self.metrics, false, true)
        }
    }

    /// One pass of `view` in a 300 pt tall scroll area (scrolled to
    /// `offset` when given), fed `events`; returns its actions.
    fn pass(
        ctx: &egui::Context,
        view: &mut MeetingsView,
        events: Vec<egui::Event>,
        offset: Option<f32>,
    ) -> (Vec<RowAction>, ListGeometry) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 400.0),
            )),
            events,
            ..Default::default()
        };
        let mut actions = Vec::new();
        let mut geometry = None;
        let mut output = ctx.run_ui(input, |ui| {
            let mut area = egui::ScrollArea::vertical()
                .id_salt("meetings-scroll")
                .max_height(300.0)
                .auto_shrink([false, false]);
            if let Some(offset) = offset {
                area = area.vertical_scroll_offset(offset);
            }
            area.show(ui, |ui| {
                let metrics = RowMetrics::measure(ui);
                geometry = Some(ListGeometry {
                    origin: ui.cursor().min,
                    width: ui.available_width(),
                    metrics,
                    layout: view.layout(ui, metrics),
                });
                view.draw(ui, &mut actions);
            });
        });
        output.textures_delta.clear();
        (actions, geometry.expect("drawn"))
    }

    fn click_events(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    fn tab() -> Vec<egui::Event> {
        vec![egui::Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]
    }

    fn focused(ctx: &egui::Context) -> Option<egui::Id> {
        ctx.memory(|m| m.focused())
    }

    /// Press and release the pencil of row `target` while row `editing`
    /// holds an edited editor; returns the view and the saves it asked for.
    fn pencil_while_editing(editing: usize, target: usize) -> (MeetingsView, Vec<(i64, String)>) {
        let ctx = theme::test_context();
        let mut view = list_view(4);
        let edited = view.meetings[editing].id;
        let mut saves = Vec::new();
        let start = RowAction::StartEdit {
            id: edited,
            title: view.meetings[editing].title.clone(),
        };
        assert_eq!(view.edit(start), None);
        view.editing.as_mut().unwrap().text = "Renamed".into();
        let mut geometry = None;
        for _ in 0..2 {
            let (actions, g) = pass(&ctx, &mut view, Vec::new(), None);
            assert!(actions.is_empty(), "{actions:?}");
            geometry = Some(g);
        }
        let rename_id = egui::Id::new(("meeting-rename", edited));
        assert_eq!(focused(&ctx), Some(rename_id), "the editor took focus");

        let pencil = geometry.unwrap().row(target).pencil.center();
        for events in [
            click_events(pencil, true),
            click_events(pencil, false),
            Vec::new(),
            Vec::new(),
        ] {
            let (actions, _) = pass(&ctx, &mut view, events, None);
            saves.extend(actions.into_iter().filter_map(|a| view.edit(a)));
        }
        (view, saves)
    }

    #[test]
    fn the_pencil_of_another_row_saves_this_one_and_opens_that_one() {
        // Map §1.4: starting to edit another row commits the open editor
        // and opens the new one — above the editor (its StartEdit arrives
        // before the old editor's focus-loss commit) and below it.
        for (editing, target) in [(1, 0), (1, 3)] {
            let (view, saves) = pencil_while_editing(editing, target);
            let edited = view.meetings[editing].id;
            let clicked = view.meetings[target].id;
            assert_eq!(saves, vec![(edited, "Renamed".to_owned())]);
            assert_eq!(view.editing_id(), Some(clicked), "{editing} -> {target}");
        }
        // The own pencil ("Done") saves and closes.
        let (view, saves) = pencil_while_editing(2, 2);
        assert_eq!(saves, vec![(view.meetings[2].id, "Renamed".to_owned())]);
        assert_eq!(view.editing_id(), None);
    }

    #[test]
    fn stale_editor_actions_are_ignored() {
        let mut view = list_view(3);
        let start = |id: i64| RowAction::StartEdit {
            id,
            title: format!("Meeting {id}"),
        };
        view.edit(start(1));
        view.editing.as_mut().unwrap().text = "  New  ".into();
        // Another row's editor opens: this one is saved (trimmed).
        assert_eq!(view.edit(start(2)), Some((1, "New".to_owned())));
        assert_eq!(view.edit(RowAction::CommitEdit(1)), None);
        assert_eq!(view.edit(RowAction::CancelEdit(1)), None);
        assert_eq!(view.editing_id(), Some(2));
        // Unchanged and empty titles close without saving.
        assert_eq!(view.edit(RowAction::CommitEdit(2)), None);
        assert_eq!(view.editing_id(), None);
        view.edit(start(3));
        view.editing.as_mut().unwrap().text = "   ".into();
        assert_eq!(view.take_edit(), None);
        view.edit(start(3));
        assert_eq!(view.edit(RowAction::CancelEdit(3)), None);
        assert_eq!(view.editing_id(), None);
    }

    #[test]
    fn tab_walks_the_whole_list_while_it_scrolls() {
        let ctx = theme::test_context();
        let mut view = list_view(12);
        view.has_more = true;
        view.binned = vec![DeletedMeeting {
            meeting: meeting(99, "transcribed", "manual", None),
            deleted_at: "2026-10-06T10:00:00+01:00".into(),
        }];
        pass(&ctx, &mut view, Vec::new(), None);
        // Main, rename and delete per row, then Load more and the bin head;
        // follow_focus scrolls each one into view as it is reached.
        let stops = 12 * 3 + 2;
        let mut seen = Vec::new();
        for n in 0..stops {
            pass(&ctx, &mut view, tab(), None);
            pass(&ctx, &mut view, Vec::new(), None);
            let id = focused(&ctx).unwrap_or_else(|| panic!("focus lost at Tab {}", n + 1));
            assert!(!seen.contains(&id), "Tab {} revisited a widget", n + 1);
            seen.push(id);
        }
    }

    #[test]
    fn focus_survives_scrolling_the_list() {
        let ctx = theme::test_context();
        let mut view = list_view(40);
        view.has_more = true;
        pass(&ctx, &mut view, Vec::new(), None);
        let (_, geometry) = pass(&ctx, &mut view, Vec::new(), None);
        let pitch = geometry.metrics.height + ROW_GAP;

        // Rename, then Delete, of row 2 (auto ids), and row 2's main block.
        for tabs in [8, 9, 7] {
            let ctx = theme::test_context();
            pass(&ctx, &mut view, Vec::new(), Some(0.0));
            for _ in 0..tabs {
                pass(&ctx, &mut view, tab(), None);
                pass(&ctx, &mut view, Vec::new(), None);
            }
            let before = focused(&ctx);
            assert!(before.is_some());
            for rows in [1.0, 3.0, 10.0, 0.0] {
                pass(&ctx, &mut view, Vec::new(), Some(rows * pitch));
                pass(&ctx, &mut view, Vec::new(), None);
                assert_eq!(focused(&ctx), before, "{tabs} tabs, scrolled {rows} rows");
            }
        }

        // Load more, after the last row: scrolling back to the top draws
        // more rows before it.
        let ctx = theme::test_context();
        let mut view = list_view(8);
        view.has_more = true;
        pass(&ctx, &mut view, Vec::new(), None);
        for _ in 0..8 * 3 + 1 {
            pass(&ctx, &mut view, tab(), None);
            pass(&ctx, &mut view, Vec::new(), None);
        }
        let before = focused(&ctx);
        assert!(before.is_some());
        pass(&ctx, &mut view, Vec::new(), Some(0.0));
        pass(&ctx, &mut view, Vec::new(), None);
        assert_eq!(focused(&ctx), before, "Load more kept the focus");
    }

    #[test]
    fn narrow_rows_elide_instead_of_overflowing() {
        // A 200 pt list: long titles and meta lines are cut with "…", rows
        // keep their one height, and no text runs past the list's edge.
        let ctx = theme::test_context();
        let mut view = list_view(3);
        view.meetings[1].title = "A very long meeting title that cannot fit ".repeat(4);
        view.meetings[2].trigger = "auto".into();
        view.meetings[2].status = "failed".into();
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(200.0, 400.0),
            )),
            ..Default::default()
        };
        let mut right = 0.0;
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut output = ctx.run_ui(input(), |ui| {
                right = ui.max_rect().right();
                let mut actions = Vec::new();
                view.draw(ui, &mut actions);
            });
            output.textures_delta.clear();
            texts = output
                .shapes
                .into_iter()
                .filter_map(|clipped| match clipped.shape {
                    egui::Shape::Text(text) => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>();
        }
        assert!(
            texts.iter().any(|t| t.galley.elided),
            "the long title elides"
        );
        for text in &texts {
            assert_eq!(text.galley.rows.len(), 1, "{:?}", text.galley.text());
            let end = text.pos.x + text.galley.size().x;
            assert!(end <= right + 0.5, "{:?} ends at {end}", text.galley.text());
        }
    }

    // ------------------------------------------------------------------
    // Keyboard selection, the split's open meeting, loading / empty states

    fn key(key: Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// `read_list_keys` over one pass fed `events`.
    fn keys_of(events: Vec<egui::Event>, space: bool, edit_keys: bool) -> ListKeys {
        let ctx = theme::test_context();
        let mut keys = ListKeys::default();
        let input = egui::RawInput {
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            keys = read_list_keys(ui.ctx(), space, edit_keys)
        });
        output.textures_delta.clear();
        keys
    }

    #[test]
    fn selection_steps_and_stops_at_the_ends() {
        let items = [10, 20, 30, 40];
        let step = |current: Option<i32>, motion| step_selection(&items, current.as_ref(), motion);
        // No selection (or a stale one): a step lands on the first item.
        assert_eq!(step(None, Motion::By(1)), Some(10));
        assert_eq!(step(None, Motion::By(-1)), Some(10));
        assert_eq!(step(Some(99), Motion::By(1)), Some(10));
        assert_eq!(step(None, Motion::Last), Some(40));
        assert_eq!(step(Some(20), Motion::By(1)), Some(30));
        assert_eq!(step(Some(20), Motion::By(-1)), Some(10));
        assert_eq!(step(Some(20), Motion::By(5)), Some(40), "clamped");
        assert_eq!(step(Some(10), Motion::By(-1)), Some(10));
        assert_eq!(step(Some(30), Motion::First), Some(10));
        assert_eq!(step(Some(10), Motion::Last), Some(40));
        assert_eq!(step_selection::<i32>(&[], None, Motion::By(1)), None);
    }

    #[test]
    fn list_keys_are_read_and_netted() {
        use egui::Modifiers as M;
        let none = M::NONE;
        let down = keys_of(
            vec![key(Key::ArrowDown, none), key(Key::J, none)],
            false,
            false,
        );
        assert_eq!(down.motion, Some(Motion::By(2)));
        let net = keys_of(
            vec![key(Key::ArrowDown, none), key(Key::K, none)],
            false,
            false,
        );
        assert_eq!(net.motion, None, "down and up cancel out");
        let home = keys_of(
            vec![key(Key::ArrowDown, none), key(Key::Home, none)],
            false,
            false,
        );
        assert_eq!(home.motion, Some(Motion::First));
        assert_eq!(
            keys_of(vec![key(Key::End, none)], false, false).motion,
            Some(Motion::Last)
        );
        assert!(keys_of(vec![key(Key::Enter, none)], false, false).activate);
        // Space activates only where the view lets it.
        assert!(!keys_of(vec![key(Key::Space, none)], false, false).activate);
        assert!(keys_of(vec![key(Key::Space, none)], true, false).activate);
        // Delete / F2 only with the edit keys; Shift+Delete skips confirming.
        let del = vec![key(Key::Delete, M::SHIFT), key(Key::F2, none)];
        assert_eq!(keys_of(del.clone(), false, false), ListKeys::default());
        let edit = keys_of(del, false, true);
        assert_eq!((edit.delete, edit.rename), (Some(true), true));
        // Ctrl+J / Ctrl+K belong to someone else (Ctrl+K: the palette).
        assert_eq!(
            keys_of(vec![key(Key::K, M::COMMAND)], false, false).motion,
            None
        );
    }

    #[test]
    fn list_keys_wait_for_a_free_keyboard() {
        let ctx = theme::test_context();
        assert!(list_keys_free(&ctx, false));
        assert!(!list_keys_free(&ctx, true), "a modal is open");
        ctx.memory_mut(|m| m.request_focus(egui::Id::new("search-box")));
        assert!(!list_keys_free(&ctx, false), "a widget has the focus");
    }

    #[test]
    fn keys_move_the_selection_and_act_on_it() {
        let mut view = list_view(3);
        view.binned = vec![DeletedMeeting {
            meeting: meeting(99, "transcribed", "manual", None),
            deleted_at: "2026-10-06T10:00:00+01:00".into(),
        }];
        let keys = |motion| ListKeys {
            motion: Some(motion),
            ..ListKeys::default()
        };
        assert!(view.key_actions(keys(Motion::By(1))).is_empty());
        assert_eq!(view.cursor, Some(Sel::Meeting(1)));
        assert_eq!(view.reveal, view.cursor, "a keyboard move scrolls");
        view.key_actions(keys(Motion::By(1)));
        let act = ListKeys {
            activate: true,
            delete: Some(true),
            rename: true,
            ..ListKeys::default()
        };
        assert_eq!(
            view.key_actions(act),
            vec![
                RowAction::OpenNow(2),
                RowAction::Delete {
                    id: 2,
                    title: "Meeting 2".into(),
                    skip: true
                },
                RowAction::StartEdit {
                    id: 2,
                    title: "Meeting 2".into()
                },
            ]
        );
        // The closed bin is not in the walk; the open one follows the list.
        view.key_actions(keys(Motion::Last));
        assert_eq!(view.cursor, Some(Sel::Meeting(3)));
        view.bin_open = true;
        view.key_actions(keys(Motion::By(1)));
        assert_eq!(view.cursor, Some(Sel::Binned(99)));
        let act = ListKeys {
            activate: true,
            delete: Some(false),
            rename: true,
            ..ListKeys::default()
        };
        assert_eq!(
            view.key_actions(act),
            vec![
                RowAction::Restore(99),
                RowAction::Purge {
                    id: 99,
                    title: "Meeting 99".into(),
                    skip: false
                },
            ],
            "Enter restores, Delete purges, F2 does nothing in the bin"
        );
        // A selection that is gone (restored, purged, bin closed) is inert.
        view.bin_open = false;
        let enter = ListKeys {
            activate: true,
            ..ListKeys::default()
        };
        assert!(view.key_actions(enter).is_empty());
    }

    #[test]
    fn the_open_meeting_takes_the_selection_once() {
        let mut view = list_view(5);
        view.follow_pane(Some(4));
        assert_eq!(view.pane_open, Some(4));
        assert_eq!(
            (view.cursor, view.reveal),
            (Some(Sel::Meeting(4)), Some(Sel::Meeting(4)))
        );
        // The user moves on; the same open meeting doesn't pull them back.
        view.cursor = Some(Sel::Meeting(1));
        view.reveal = None;
        view.follow_pane(Some(4));
        assert_eq!((view.cursor, view.reveal), (Some(Sel::Meeting(1)), None));
        // Full width again: nothing is open; the selection stays.
        view.follow_pane(None);
        assert_eq!((view.pane_open, view.cursor), (None, Some(Sel::Meeting(1))));
        assert_eq!(
            view.pane_selection(PaneRole::List { selected: Some(7) }),
            Some(7)
        );
        assert_eq!(view.pane_selection(PaneRole::Full), None);
        assert_eq!(view.pane_selection(PaneRole::Detail), None);
    }

    #[test]
    fn selected_and_open_rows_keep_their_fill() {
        let selected = RowMark {
            selected: true,
            ..RowMark::default()
        };
        let open = RowMark {
            open: true,
            ..RowMark::default()
        };
        assert_eq!(selected.fill(), Some(pal().bg_active));
        assert_eq!(open.fill(), Some(pal().bg_active));
        // Hover never fills (it shows a pill at the left edge).
        assert_eq!(RowMark::default().fill(), None);
    }

    /// The texts `view` draws in one 600 × 300 pass (after a settling
    /// pass), with the list scrolled as its reveal asks.
    fn drawn_texts(ctx: &egui::Context, view: &mut MeetingsView) -> Vec<String> {
        // Test time stands still: scroll at once.
        ctx.all_styles_mut(|s| s.scroll_animation = egui::style::ScrollAnimation::none());
        let mut texts = Vec::new();
        for _ in 0..3 {
            let (_, _) = pass(ctx, view, Vec::new(), None);
        }
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 400.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("meetings-scroll")
                .max_height(300.0)
                .auto_shrink([false, false])
                .show(ui, |ui| view.draw(ui, &mut Vec::new()));
        });
        output.textures_delta.clear();
        for clipped in output.shapes {
            if let egui::Shape::Text(text) = clipped.shape {
                if clipped.clip_rect.intersects(text.visual_bounding_rect()) {
                    texts.push(text.galley.text().to_owned());
                }
            }
        }
        texts
    }

    #[test]
    fn a_selection_off_screen_scrolls_into_view() {
        let ctx = theme::test_context();
        let mut view = list_view(40);
        assert!(!drawn_texts(&ctx, &mut view).contains(&"Meeting 40".to_owned()));
        view.key_actions(ListKeys {
            motion: Some(Motion::Last),
            ..ListKeys::default()
        });
        let texts = drawn_texts(&ctx, &mut view);
        assert!(texts.contains(&"Meeting 40".to_owned()), "{texts:?}");
        assert_eq!(view.reveal, None, "revealed once");
    }

    #[test]
    fn empty_library_offers_a_way_to_record() {
        let env = EmptyEnv {
            auto_record: false,
            recording: false,
            record_hotkey: Some("ctrl+alt+r".into()),
        };
        let (body, buttons) = empty_library(&env);
        assert!(body.contains("(Ctrl+Alt+R)"), "{body}");
        assert!(body.contains("auto-record"), "{body}");
        assert_eq!(
            buttons,
            vec![
                ("Start recording", RowAction::StartRecording),
                ("Open Settings", RowAction::OpenSettings),
            ]
        );
        // Auto-record on, already recording, no hotkey bound.
        let env = EmptyEnv {
            auto_record: true,
            recording: true,
            record_hotkey: None,
        };
        let (body, buttons) = empty_library(&env);
        assert!(
            body.starts_with("Witness records Teams calls by itself"),
            "{body}"
        );
        assert!(!body.contains('('), "{body}");
        assert!(buttons.is_empty());
        assert_eq!(hotkey_text("ctrl+shift+R"), "Ctrl+Shift+R");
    }

    #[test]
    fn first_load_skeleton_then_the_empty_state() {
        let ctx = theme::test_context();
        // A first load: a skeleton, no text at all.
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(380.0, 760.0),
            )),
            ..Default::default()
        };
        let mut skeleton = Vec::new();
        let mut output = ctx.run_ui(input(), list_skeleton);
        output.textures_delta.clear();
        for clipped in &output.shapes {
            assert!(!matches!(clipped.shape, egui::Shape::Text(_)));
            let rect = clipped.shape.visual_bounding_rect();
            if rect.is_positive() {
                skeleton.push(rect);
            }
        }
        assert!(skeleton.len() >= 8, "{} bars", skeleton.len());
        assert!(skeleton.iter().all(|r| r.right() <= 380.5), "no overflow");

        // Loaded and empty: the designed empty state.
        let mut view = MeetingsView::new();
        let texts = drawn_texts(&ctx, &mut view);
        assert!(texts.contains(&"No meetings yet".to_owned()), "{texts:?}");
        assert!(texts.contains(&"Start recording".to_owned()), "{texts:?}");
        // An open bin that was emptied keeps its head and says so.
        let mut view = list_view(1);
        view.bin_open = true;
        let texts = drawn_texts(&ctx, &mut view);
        assert!(
            texts.contains(&"The recycle bin is empty".to_owned()),
            "{texts:?}"
        );
        assert!(!texts.contains(&"Empty bin".to_owned()), "{texts:?}");
    }
}
