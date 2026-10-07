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

use crate::bridge::{OpDone, Task};
use crate::ui::format::fmt_duration;
use crate::ui::icons::{self, ph};
use crate::ui::theme::{
    self, ACCENT, BG, BG_HOVER, FOCUS, REC, TEXT, TEXT_FAINT, TEXT_MUTED, WARN,
};
use crate::ui::toasts::{Notice, ToastAction};
use crate::ui::widgets::{self, KeyScroll};
use crate::ui::{UiCtx, View};
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
use witness_core::CoreResult;

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
        "recording" => (ph::RECORD, REC),
        "recorded" => (ph::WAVEFORM, WARN),
        "processing" => (ph::CIRCLE_NOTCH, WARN),
        "failed" => (ph::WARNING_CIRCLE, REC),
        _ => (ph::CHECK_CIRCLE, theme::OK),
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
        Stroke::new(2.0, FOCUS),
        StrokeKind::Outside,
    );
}

/// The row-style focus ring (inside the row, like `widgets::list_row`).
fn paint_row_focus(ui: &egui::Ui, rect: Rect) {
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(theme::R_MD),
        Stroke::new(2.0, FOCUS),
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
    let galley = ui
        .painter()
        .layout_no_wrap(ph::TRASH.to_owned(), theme::regular(ICON), TEXT);
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
}

/// What the open confirmation is for.
enum ConfirmFor {
    Delete { id: i64 },
    Purge { id: i64 },
    EmptyBin,
}

/// Something the user asked for during the pass; applied after drawing.
#[derive(Debug, PartialEq)]
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
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if self.seen_tick != Some(cx.shell.refresh_tick) {
            self.seen_tick = Some(cx.shell.refresh_tick);
            self.reload(cx);
            self.load_bin(cx);
        }
        self.poll(ui, cx);
        self.fire_pending_open(ui, cx);
        self.take_confirm_answer(cx);

        let mut actions = Vec::new();
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
                open_transcript(cx, id, View::Meetings);
            }
            RowAction::OpenLater(id) => {
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

        if self.loading() && self.meetings.is_empty() {
            widgets::empty_state(ui, "Loading meetings…", None);
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
            widgets::empty_state(
                ui,
                "No meetings yet.",
                Some("Join a meeting or hit record."),
            );
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
        for &(item, top, height) in &layout.items {
            let rect = Rect::from_min_size(
                egui::pos2(list_rect.left(), list_rect.top() + top),
                egui::vec2(list_rect.width(), height),
            );
            match item {
                Item::Header(first) => {
                    if !band.intersects(rect) {
                        continue;
                    }
                    drawn += 1;
                    let label = day_label(days.get(first).copied().flatten(), today);
                    day_header_item(ui, rect, first, &label);
                }
                Item::Row(index) => {
                    let meeting = &self.meetings[index];
                    let editing = self.editing.as_mut().filter(|e| e.id == meeting.id);
                    // The editor's and the focused row are always drawn: a
                    // widget that is not drawn loses its focus (and a rename
                    // would commit).
                    let kept = keep == Some(meeting.id);
                    if editing.is_none() && !kept && !band.intersects(rect) {
                        continue;
                    }
                    drawn += 1;
                    if meeting_row(ui, rect, metrics, meeting, editing, kept, actions) {
                        self.focused_row = Some(meeting.id);
                    }
                }
            }
        }
        // Each drawn item took one auto id from `ui`; take the skipped
        // items' too, so the ids of Load more and the bin (and so their
        // keyboard focus) don't depend on how far the list is scrolled.
        ui.skip_ahead_auto_ids(layout.items.len() - drawn);
    }

    fn draw_bin(&mut self, ui: &mut egui::Ui, actions: &mut Vec<RowAction>) {
        if self.binned.is_empty() {
            return;
        }
        widgets::gap(ui, theme::S5);
        let open = self.bin_open;
        let count = self.binned.len();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S2;
            let empty_width = if open {
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
            if open {
                let label = egui::RichText::new("Empty bin")
                    .size(13.0)
                    .color(TEXT_MUTED);
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
                            .color(TEXT_FAINT),
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
            bin_row(ui, slot, deleted, today, actions);
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

/// One meeting row at `rect` (uniform height): a flat row that fills on
/// hover. The whole row opens the meeting; rename and delete show while
/// the row is hovered, holds the keyboard focus (`focus_kept`: it did last
/// pass) or is being renamed — they stay in the Tab order when hidden.
/// True when one of its widgets has the keyboard focus.
fn meeting_row(
    ui: &mut egui::Ui,
    rect: Rect,
    metrics: RowMetrics,
    m: &Meeting,
    editing: Option<&mut Editing>,
    focus_kept: bool,
    actions: &mut Vec<RowAction>,
) -> bool {
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
    if hovered {
        row.painter()
            .rect_filled(rect, CornerRadius::same(theme::R_MD), BG_HOVER);
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
            TEXT,
            main_rect.width(),
        );
        elided = title.elided;
        let title_top = main_rect.top() + (metrics.line - metrics.title) / 2.0;
        title_rect = Rect::from_min_size(egui::pos2(main_rect.left(), title_top), title.size());
        row.painter().galley(title_rect.min, title, TEXT);
    }
    let meta = truncated(
        row,
        &meta_line(m),
        theme::regular(12.0),
        TEXT_MUTED,
        main_rect.width(),
    );
    let meta_top = main_rect.top() + metrics.line + TITLE_META_GAP;
    row.painter()
        .galley(egui::pos2(main_rect.left(), meta_top), meta, TEXT_MUTED);

    let name = format!("Open {}, {}", m.title, status_label(&m.status));
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    follow_focus(response);
    let focused = response.has_focus();
    if focused && widgets::focus_visible(row.ctx()) {
        paint_row_focus(row, rect);
    }
    let response = response.clone().on_hover_cursor(CursorIcon::PointingHand);
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
        .fill(BG)
        .stroke(Stroke::new(1.0, ACCENT))
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
        ui.painter()
            .rect_filled(rect, CornerRadius::same(theme::R_MD), theme::BG_ACTIVE);
    }
    paint_glyph(ui, rect, glyph, if hot { TEXT } else { color });
    if response.has_focus() && widgets::focus_visible(ui.ctx()) {
        paint_focus_ring(ui, rect, theme::R_MD);
    }
    follow_focus(&response);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, name));
    response
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tooltip)
}

/// A full-width, flat "Load more" (accent text, hover fill), as wide as
/// the rows above it; disabled while a load runs. Shared with Search.
pub(crate) fn load_more_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> Response {
    let color = if enabled { ACCENT } else { TEXT_MUTED };
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
                .rect_filled(rect, CornerRadius::same(theme::R_MD), BG_HOVER);
        }
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            paint_row_focus(ui, rect);
        }
    }
    follow_focus(&response);
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response.on_hover_cursor(CursorIcon::NotAllowed)
    }
}

/// Width of the "Empty bin" ghost button (13 pt text + button padding).
fn empty_bin_width(ui: &egui::Ui) -> f32 {
    let galley = ui
        .painter()
        .layout_no_wrap("Empty bin".into(), theme::regular(13.0), TEXT);
    galley.size().x + 2.0 * ui.spacing().button_padding.x
}

/// The "▸ 🗑 Recycle bin  3" disclosure: a flat row-style button, `width`
/// wide, styled like a section header.
fn bin_head(ui: &mut egui::Ui, width: f32, count: usize, open: bool) -> Response {
    let text = "Recycle bin";
    let font = theme::semibold(12.5);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), TEXT_MUTED);
    let count_galley =
        ui.painter()
            .layout_no_wrap(count.to_string(), theme::regular(12.0), TEXT_FAINT);
    let height = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    if ui.is_rect_visible(rect) {
        let hot = response.hovered();
        if hot {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(theme::R_MD), BG_HOVER);
        }
        let color = if hot { TEXT } else { TEXT_MUTED };
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
            TEXT_FAINT,
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
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// One recycle-bin row: a flat row (hover fill) with the title and meta
/// elided, then Restore and Delete forever. Not clickable itself.
fn bin_row(
    ui: &mut egui::Ui,
    slot: f32,
    d: &DeletedMeeting,
    today: NaiveDate,
    actions: &mut Vec<RowAction>,
) {
    let m = &d.meeting;
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
                    widgets::elide(ui, &m.title, theme::semibold(14.0), TEXT);
                    widgets::elide(
                        ui,
                        &bin_meta_line(d, today),
                        theme::regular(12.0),
                        TEXT_MUTED,
                    );
                });
                ui.add_space(MAIN_GAP);
                let restore = widgets::icon_btn(ui, ph::ARROW_COUNTER_CLOCKWISE, ICON, "Restore");
                let name = format!("Restore {}", m.title);
                restore.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
                follow_focus(&restore);
                if restore.clicked() {
                    actions.push(RowAction::Restore(m.id));
                }
                ui.add_space(COL_GAP);
                let purge = widgets::icon_btn_danger(ui, ph::TRASH, ICON, "Delete forever");
                let name = format!("Permanently delete {}", m.title);
                purge.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
                follow_focus(&purge);
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
    if ui.rect_contains_pointer(row.rect) {
        ui.painter().set(
            background,
            egui::epaint::RectShape::filled(row.rect, CornerRadius::same(theme::R_MD), BG_HOVER),
        );
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
        assert_eq!(status_glyph("recording"), (ph::RECORD, REC));
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
                meeting_row(ui, rect, metrics, &m, None, false, &mut actions);
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
                true,
                &mut actions,
            );
            assert!(!editing.focus_pending, "focus is requested once");
            let binned = DeletedMeeting {
                meeting: m,
                deleted_at: "2026-10-06T10:00:00+01:00".into(),
            };
            let today = Local::now().date_naive();
            bin_row(ui, metrics.slot, &binned, today, &mut actions);
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
}
