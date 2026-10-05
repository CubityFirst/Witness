//! Meetings list + recycle bin (map-ui-meetings §1, meetings.tsx).
//!
//! Loads are view-owned [`Task`]s (dropping one discards a stale result,
//! like the TSX generation counters); every action — rename, delete,
//! restore, purge, empty bin, retranscribe — goes through
//! `Tasks::spawn_op`, so its toast and list refresh survive navigating away
//! mid-action. The list reloads whenever `shell.refresh_tick` changes.
//!
//! Rows have a uniform height, so the list is one allocation and only the
//! rows inside the clip rect are laid out (hundreds of loaded rows stay
//! cheap). The recycle bin is a separate, normally laid-out section after
//! the list.

use crate::bridge::{OpDone, Task};
use crate::ui::format::{fmt_date, fmt_duration};
use crate::ui::icons::{self, ph};
use crate::ui::theme::{
    self, ACCENT, BG, BG_HOVER, BG_RAISED, BORDER, FOCUS, OK, REC, TEXT, TEXT_MUTED, WARN,
};
use crate::ui::toasts::{Notice, ToastAction};
use crate::ui::widgets::{self, KeyScroll};
use crate::ui::{UiCtx, View};
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
/// A single click opens after this delay unless a double-click (rename)
/// arrives first.
const OPEN_DELAY: Duration = Duration::from_millis(250);
/// `.meeting-list { gap: 6px }`.
const ROW_GAP: f32 = 6.0;
/// `.meeting-row { padding: 10px 14px }` plus its 1 px border.
const ROW_PAD: egui::Vec2 = egui::vec2(15.0, 11.0);
/// `.meeting-row { gap: 10px }`.
const COL_GAP: f32 = 10.0;
/// Status and action glyphs.
const ICON: f32 = 16.0;
/// `.meeting-main { gap: 2px }` between the title and the meta line.
const TITLE_META_GAP: f32 = 2.0;
/// `.rename-input { padding: 3px 6px }`.
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
        _ => (ph::CHECK_CIRCLE, OK),
    }
}

/// `"{date} · {duration}"`, plus `" · auto"` for auto-started meetings.
fn meta_line(m: &Meeting) -> String {
    let auto = if m.trigger == "auto" { " · auto" } else { "" };
    format!(
        "{} · {}{auto}",
        fmt_date(&m.started_at),
        fmt_duration(m.duration_ms)
    )
}

/// The recycle-bin row's meta line.
fn bin_meta_line(d: &DeletedMeeting) -> String {
    format!(
        "{} · {} · deleted {} · auto-removed after 30 days",
        fmt_date(&d.meeting.started_at),
        fmt_duration(d.meeting.duration_ms),
        fmt_date(&d.deleted_at)
    )
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

/// Paint `glyph` centred (on its ink box) in `rect`.
fn paint_glyph(ui: &egui::Ui, rect: Rect, glyph: &str, color: Color32) {
    icons::paint_rotated_glyph(ui.painter(), rect.center(), glyph, ICON, color, 0.0);
}

/// Side of the square `.icon-btn` box for a 16 pt glyph (the formula of
/// `widgets::icon_btn`, so status icons and buttons line up).
fn icon_slot(ui: &egui::Ui) -> f32 {
    let galley = ui
        .painter()
        .layout_no_wrap(ph::TRASH.to_owned(), theme::regular(ICON), TEXT);
    galley.size().x.max(ICON) + 8.0
}

/// Vertical metrics of a list row.
#[derive(Debug, Clone, Copy)]
struct RowMetrics {
    /// Height of the title line (the title, or the taller rename input).
    line: f32,
    title: f32,
    meta: f32,
    /// Outer row height, border included.
    height: f32,
    /// `.icon-btn` box side.
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
        let line = title.max(edit);
        let content = (line + TITLE_META_GAP + meta).max(slot);
        RowMetrics {
            line,
            title,
            meta,
            height: content + 2.0 * ROW_PAD.y,
            slot,
        }
    }
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
enum RowAction {
    /// Keyboard (Enter/Space) or accessibility activation: open now.
    OpenNow(i64),
    /// Pointer click: open after [`OPEN_DELAY`] unless a double-click follows.
    OpenLater(i64),
    StartEdit {
        id: i64,
        title: String,
    },
    CommitEdit,
    CancelEdit,
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
    editing: Option<Editing>,
    pending_open: Option<(i64, Instant)>,
    confirm_for: Option<ConfirmFor>,
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
            bin_open: false,
            editing: None,
            pending_open: None,
            confirm_for: None,
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
        egui::ScrollArea::vertical()
            .id_salt("meetings-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::keyboard_scroll(ui, KeyScroll { space: true });
                widgets::centered_column(ui, 900.0, |ui| self.draw(ui, &mut actions));
            });
        for action in actions {
            self.apply(action, ui, cx);
        }
    }

    /// Nothing to persist.
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

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
    /// whether there is another page.
    fn load(&mut self, cx: &UiCtx, offset: usize, append: bool, limit: usize) {
        let task = cx.tasks.spawn("list-meetings", move |c| {
            c.list_meetings(offset as i64, (limit + 1) as i64)
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

    /// Navigate once the single-click delay has passed.
    fn fire_pending_open(&mut self, ui: &egui::Ui, cx: &mut UiCtx) {
        if let Some((id, at)) = self.pending_open {
            let now = Instant::now();
            if now >= at {
                self.pending_open = None;
                open_transcript(cx, id, View::Meetings);
                // Navigation applies after this pass; nothing else (no
                // input) would draw the next one.
                // TODO(I1): app.rs could request this after any navigation.
                ui.ctx().request_repaint();
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

    fn ask(&mut self, cx: &mut UiCtx, what: ConfirmFor, message: String, ok_label: &str) {
        self.confirm_for = Some(what);
        cx.confirm.ask(egui::Id::new(CONFIRM_ID), message, ok_label);
    }

    /// Commit the open editor: trim and close; save only a non-empty,
    /// changed title (an empty one silently reverts). No optimistic update:
    /// the list reloads when the rename returns.
    fn commit_edit(&mut self, cx: &mut UiCtx) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let title = editing.text.trim().to_owned();
        if title.is_empty() || title == editing.original {
            return;
        }
        let id = editing.id;
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
            RowAction::StartEdit { id, title } => {
                self.pending_open = None;
                if self.editing.as_ref().is_some_and(|e| e.id == id) {
                    return;
                }
                // Opening another editor blurs (and so commits) this one.
                self.commit_edit(cx);
                self.editing = Some(Editing {
                    id,
                    original: title.clone(),
                    text: title,
                    focus_pending: true,
                    escaped: false,
                });
            }
            RowAction::CommitEdit => self.commit_edit(cx),
            RowAction::CancelEdit => self.editing = None,
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
            ui.add_space(ROW_GAP);
            let loading = self.loading();
            let label = if loading { "Loading…" } else { "Load more" };
            let clicked = ui
                .add_enabled_ui(!loading, |ui| widgets::btn_ghost(ui, label))
                .inner
                .clicked();
            if clicked {
                actions.push(RowAction::LoadMore);
            }
        }
        self.draw_bin(ui, actions);
    }

    /// The uniform-height list: one allocation; rows are drawn only when
    /// they intersect the clip rect.
    fn draw_list(&mut self, ui: &mut egui::Ui, actions: &mut Vec<RowAction>) {
        let metrics = RowMetrics::measure(ui);
        let pitch = metrics.height + ROW_GAP;
        let count = self.meetings.len();
        let total = count as f32 * pitch - ROW_GAP;
        let (_, list_rect) = ui.allocate_space(egui::vec2(ui.available_width(), total));
        for (index, meeting) in self.meetings.iter().enumerate() {
            let rect = Rect::from_min_size(
                egui::pos2(list_rect.left(), list_rect.top() + index as f32 * pitch),
                egui::vec2(list_rect.width(), metrics.height),
            );
            let editing = self.editing.as_mut().filter(|e| e.id == meeting.id);
            // The editor's row is always drawn: a TextEdit that is not
            // drawn loses its focus, which would commit the rename.
            if editing.is_none() && !ui.is_rect_visible(rect) {
                continue;
            }
            meeting_row(ui, rect, metrics, meeting, editing, actions);
        }
    }

    fn draw_bin(&mut self, ui: &mut egui::Ui, actions: &mut Vec<RowAction>) {
        if self.binned.is_empty() {
            return;
        }
        // `.recycle-bin`: margin-top 18, 1 px dashed top border, padding-top 10.
        ui.add_space(18.0);
        let (_, line) = ui.allocate_space(egui::vec2(ui.available_width(), 1.0));
        ui.painter().extend(egui::Shape::dashed_line(
            &[line.left_center(), line.right_center()],
            Stroke::new(1.0, BORDER),
            3.0,
            3.0,
        ));
        ui.add_space(9.0);

        let open = self.bin_open;
        let count = self.binned.len();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let empty_width = if open {
                empty_bin_width(ui) + ui.spacing().item_spacing.x
            } else {
                0.0
            };
            let head_width = (ui.available_width() - empty_width).max(60.0);
            if bin_head(ui, head_width, count, open).clicked() {
                actions.push(RowAction::ToggleBin);
            }
            if open {
                let label = egui::RichText::new("Empty bin").size(12.0);
                if widgets::btn_ghost(ui, label).clicked() {
                    let skip = ui.input(|i| i.modifiers.shift);
                    actions.push(RowAction::EmptyBin { skip });
                }
            }
        });
        if !open {
            return;
        }
        let slot = icon_slot(ui);
        ui.spacing_mut().item_spacing.y = 0.0;
        for deleted in &self.binned {
            ui.add_space(ROW_GAP);
            bin_row(ui, slot, deleted, actions);
        }
    }
}

// ----------------------------------------------------------------------
// Actions (spawn_op: the toast and the refresh survive navigation)

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

/// One meeting row at `rect` (uniform height).
fn meeting_row(
    ui: &mut egui::Ui,
    rect: Rect,
    metrics: RowMetrics,
    m: &Meeting,
    editing: Option<&mut Editing>,
    actions: &mut Vec<RowAction>,
) {
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .id_salt(("meeting-row", m.id))
            .layout(Layout::left_to_right(Align::Center)),
    );
    // `.meeting-row:hover` also applies while a child button is hovered.
    let hovered = row.rect_contains_pointer(rect);
    row.painter().rect(
        rect,
        CornerRadius::same(8),
        if hovered { BG_HOVER } else { BG_RAISED },
        Stroke::new(1.0, BORDER),
        StrokeKind::Inside,
    );

    // Columns from the right: delete, rename, status; the main block
    // takes the rest. Widgets are still added left to right (tab order).
    let inner = rect.shrink2(ROW_PAD);
    let slot = metrics.slot;
    let slot_rect = |n: f32| {
        let right = inner.right() - n * (slot + COL_GAP);
        Rect::from_center_size(
            egui::pos2(right - slot / 2.0, inner.center().y),
            egui::vec2(slot, slot),
        )
    };
    let delete_rect = slot_rect(0.0);
    let pencil_rect = slot_rect(1.0);
    let status_rect = slot_rect(2.0);
    let block_height = metrics.line + TITLE_META_GAP + metrics.meta;
    let main_rect = Rect::from_min_size(
        egui::pos2(inner.left(), inner.center().y - block_height / 2.0),
        egui::vec2(
            (status_rect.left() - COL_GAP - inner.left()).max(1.0),
            block_height,
        ),
    );
    let is_editing = editing.is_some();

    // 1. Main block: the open / double-click-rename button, or the editor.
    match editing {
        None => main_block(&mut row, main_rect, metrics, m, actions),
        Some(editing) => rename_editor(
            &mut row,
            main_rect,
            pencil_rect,
            metrics,
            m,
            editing,
            actions,
        ),
    }

    // 2. Status: a retry button for failed / recorded rows, else the icon.
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
        if status_button(&mut status_ui, status_rect, &m.status, tooltip, &name).clicked() {
            actions.push(RowAction::Retranscribe(m.id));
        }
    } else {
        status_icon(&mut status_ui, status_rect, &m.status);
    }

    // 3. Rename / Done.
    let mut pencil_ui = row.new_child(UiBuilder::new().max_rect(pencil_rect));
    let (tooltip, name) = if is_editing {
        ("Done", format!("Save {}", m.title))
    } else {
        ("Rename", format!("Rename {}", m.title))
    };
    let pencil = widgets::icon_btn(&mut pencil_ui, ph::PENCIL_SIMPLE, ICON, tooltip);
    pencil.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    if pencil.clicked() {
        actions.push(if is_editing {
            RowAction::CommitEdit
        } else {
            RowAction::StartEdit {
                id: m.id,
                title: m.title.clone(),
            }
        });
    }

    // 4. Delete (move to the recycle bin).
    let mut delete_ui = row.new_child(UiBuilder::new().max_rect(delete_rect));
    let delete = widgets::icon_btn_danger(&mut delete_ui, ph::TRASH, ICON, "Delete");
    let name = format!("Move {} to the recycle bin", m.title);
    delete.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    if delete.clicked() {
        actions.push(RowAction::Delete {
            id: m.id,
            title: m.title.clone(),
            skip: delete_ui.input(|i| i.modifiers.shift),
        });
    }
}

/// The title + meta button. A pointer click opens after 250 ms (a
/// double-click cancels that and renames); Enter/Space opens at once.
fn main_block(
    row: &mut egui::Ui,
    main_rect: Rect,
    metrics: RowMetrics,
    m: &Meeting,
    actions: &mut Vec<RowAction>,
) {
    let response = row.interact(
        main_rect,
        egui::Id::new(("meeting-open", m.id)),
        Sense::click(),
    );
    let title = truncated(
        row,
        &m.title,
        theme::semibold(14.0),
        TEXT,
        main_rect.width(),
    );
    let title_top = main_rect.top() + (metrics.line - metrics.title) / 2.0;
    let title_rect = Rect::from_min_size(egui::pos2(main_rect.left(), title_top), title.size());
    row.painter().galley(title_rect.min, title, TEXT);
    paint_meta(row, main_rect, metrics, &meta_line(m));

    let name = format!("Open {}, {}", m.title, status_label(&m.status));
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    if response.has_focus() {
        paint_focus_ring(row, main_rect, 4);
    }
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    // The tooltip sits on the title only, like the TSX `title` attribute.
    let on_title = row
        .ctx()
        .pointer_hover_pos()
        .is_some_and(|p| title_rect.contains(p));
    if on_title {
        response.clone().on_hover_text("Double-click to rename");
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
}

/// The inline rename input with the meta line kept under it. Enter or
/// focus loss commits; Escape cancels.
fn rename_editor(
    row: &mut egui::Ui,
    main_rect: Rect,
    pencil_rect: Rect,
    metrics: RowMetrics,
    m: &Meeting,
    editing: &mut Editing,
    actions: &mut Vec<RowAction>,
) {
    let edit_rect = Rect::from_min_size(main_rect.min, egui::vec2(main_rect.width(), metrics.line));
    let mut edit_ui = row.new_child(
        UiBuilder::new()
            .max_rect(edit_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let frame = egui::Frame::new()
        .fill(BG)
        .stroke(Stroke::new(1.0, ACCENT))
        .corner_radius(CornerRadius::same(4))
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
    paint_meta(row, main_rect, metrics, &meta_line(m));

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
                RowAction::CancelEdit
            } else {
                RowAction::CommitEdit
            });
        }
    }
}

/// The 12 pt muted meta line under the title line, kept to one line.
fn paint_meta(ui: &egui::Ui, main_rect: Rect, metrics: RowMetrics, text: &str) {
    let galley = truncated(
        ui,
        text,
        theme::regular(12.0),
        TEXT_MUTED,
        main_rect.width(),
    );
    let top = main_rect.top() + metrics.line + TITLE_META_GAP;
    ui.painter()
        .galley(egui::pos2(main_rect.left(), top), galley, TEXT_MUTED);
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

/// `.icon-btn.status-failed|recorded`: the status glyph in its colour,
/// turning to the text colour on hover; a click queues a transcription.
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
            .rect_filled(rect, CornerRadius::same(6), BG_HOVER);
    }
    paint_glyph(ui, rect, glyph, if hot { TEXT } else { color });
    if response.has_focus() {
        paint_focus_ring(ui, rect, 6);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, name));
    response
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tooltip)
}

/// Width of the "Empty bin" ghost button (12 pt text + `.btn` padding).
fn empty_bin_width(ui: &egui::Ui) -> f32 {
    let galley = ui
        .painter()
        .layout_no_wrap("Empty bin".into(), theme::regular(12.0), TEXT);
    let pad = if theme::narrow(ui) { 10.0 } else { 14.0 };
    galley.size().x + 2.0 * pad
}

/// `.recycle-head`: the "🗑 Recycle bin (n) ▸/▾" disclosure button.
fn bin_head(ui: &mut egui::Ui, width: f32, count: usize, open: bool) -> Response {
    let font = theme::regular(13.0);
    let text = format!("Recycle bin ({count})");
    let galley = ui
        .painter()
        .layout_no_wrap(text.clone(), font.clone(), TEXT_MUTED);
    let height = galley.size().y + 12.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    if ui.is_rect_visible(rect) {
        if response.hovered() || response.has_focus() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(6), BG_HOVER);
        }
        let cy = rect.center().y;
        let mut x = rect.left() + 10.0;
        icons::paint_rotated_glyph(
            ui.painter(),
            egui::pos2(x + 7.0, cy),
            ph::TRASH,
            14.0,
            TEXT_MUTED,
            0.0,
        );
        x += 14.0 + 8.0;
        let text_width = galley.size().x;
        ui.painter().galley(
            egui::pos2(x, cy - galley.size().y / 2.0),
            galley,
            TEXT_MUTED,
        );
        x += text_width + 8.0;
        ui.painter().text(
            egui::pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            if open { "▾" } else { "▸" },
            font,
            TEXT_MUTED,
        );
        if response.has_focus() {
            paint_focus_ring(ui, rect, 6);
        }
    }
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::Button, true, &text);
        info.selected = Some(open); // aria-expanded
        info
    });
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// One recycle-bin row: the meeting-row box at 0.75 opacity with a
/// non-clickable main block (the meta wraps), Restore and Delete forever.
fn bin_row(ui: &mut egui::Ui, slot: f32, d: &DeletedMeeting, actions: &mut Vec<RowAction>) {
    let m = &d.meeting;
    ui.scope(|ui| {
        ui.set_opacity(0.75);
        let background = ui.painter().add(egui::Shape::Noop);
        let width = ui.available_width();
        let main_width = (width - 2.0 * ROW_PAD.x - 2.0 * (slot + COL_GAP)).max(40.0);
        let row = ui
            .scope_builder(
                UiBuilder::new().layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(ROW_PAD.x);
                    ui.vertical(|ui| {
                        ui.set_width(main_width);
                        ui.spacing_mut().item_spacing.y = TITLE_META_GAP;
                        ui.add_space(ROW_PAD.y);
                        let title =
                            truncated(ui, &m.title, theme::semibold(14.0), TEXT, main_width);
                        let (rect, _) = ui.allocate_exact_size(title.size(), Sense::hover());
                        ui.painter().galley(rect.min, title, TEXT);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(bin_meta_line(d))
                                    .size(12.0)
                                    .color(TEXT_MUTED),
                            )
                            .wrap(),
                        );
                        ui.add_space(ROW_PAD.y);
                    });
                    ui.add_space(COL_GAP);
                    let restore =
                        widgets::icon_btn(ui, ph::ARROW_COUNTER_CLOCKWISE, ICON, "Restore");
                    let name = format!("Restore {}", m.title);
                    restore.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
                    if restore.clicked() {
                        actions.push(RowAction::Restore(m.id));
                    }
                    ui.add_space(COL_GAP);
                    let purge = widgets::icon_btn_danger(ui, ph::TRASH, ICON, "Delete forever");
                    let name = format!("Permanently delete {}", m.title);
                    purge.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
                    if purge.clicked() {
                        actions.push(RowAction::Purge {
                            id: m.id,
                            title: m.title.clone(),
                            skip: ui.input(|i| i.modifiers.shift),
                        });
                    }
                },
            )
            .response;
        let rect = row.rect;
        let hovered = ui.rect_contains_pointer(rect);
        ui.painter().set(
            background,
            egui::epaint::RectShape::new(
                rect,
                CornerRadius::same(8),
                if hovered { BG_HOVER } else { BG_RAISED },
                Stroke::new(1.0, BORDER),
                StrokeKind::Inside,
            ),
        );
    });
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
        let manual = meeting(2, "recording", "manual", None);
        assert!(meta_line(&manual).ends_with(" · —"));
        let binned = DeletedMeeting {
            meeting: manual,
            deleted_at: "2026-10-06T10:00:00+01:00".into(),
        };
        let line = bin_meta_line(&binned);
        assert!(line.contains(" · — · deleted "), "{line}");
        assert!(line.ends_with(" · auto-removed after 30 days"));
    }

    #[test]
    fn rows_have_room_for_the_editor_and_icons() {
        run(|ui| {
            let m = RowMetrics::measure(ui);
            assert!(m.line >= m.title);
            assert!(m.height >= m.slot + 2.0 * ROW_PAD.y);
            assert!(m.height >= m.line + m.meta + 2.0 * ROW_PAD.y);
        });
    }

    #[test]
    fn rows_lay_out_in_every_state() {
        run(|ui| {
            let metrics = RowMetrics::measure(ui);
            let mut actions = Vec::new();
            let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(420.0, metrics.height));
            for status in [
                "recording",
                "recorded",
                "processing",
                "transcribed",
                "failed",
            ] {
                let m = meeting(3, status, "auto", Some(1000));
                meeting_row(ui, rect, metrics, &m, None, &mut actions);
            }
            let m = meeting(4, "transcribed", "manual", None);
            let mut editing = Editing {
                id: 4,
                original: m.title.clone(),
                text: "New".into(),
                focus_pending: true,
                escaped: false,
            };
            meeting_row(ui, rect, metrics, &m, Some(&mut editing), &mut actions);
            assert!(!editing.focus_pending, "focus is requested once");
            let binned = DeletedMeeting {
                meeting: m,
                deleted_at: "2026-10-06T10:00:00+01:00".into(),
            };
            bin_row(ui, metrics.slot, &binned, &mut actions);
            bin_head(ui, 300.0, 3, true);
            assert!(actions.is_empty(), "no input, no actions");
        });
    }
}
