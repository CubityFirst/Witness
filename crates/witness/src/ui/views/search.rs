//! Full-text search results (map-ui-meetings §2, search.tsx) and the
//! shell's search box (app.tsx `onSearchInput`).
//!
//! The box edits the app-owned `cx.memory.search_box`, so the top bar's
//! Meetings button can clear it and cancel its 250 ms debounce. The view
//! reads its query from the current `View::Search { query }` (a new query
//! does not remount it) and re-searches whenever the query or an applied
//! filter changes; old results are dropped first so they never show under
//! a new heading. Search never auto-refreshes on `refresh_tick` (parity).

use crate::bridge::Task;
use crate::ui::format::{fmt_date, fmt_ts, parse_snippet, SnippetPart};
use crate::ui::icons::ph;
use crate::ui::theme::{
    self, ACCENT, BG, BG_HOVER, BOOKMARK_GLYPH, BORDER, MARK_BG, MARK_TEXT, REC, TEXT, TEXT_MUTED,
    WARN,
};
use crate::ui::views::meetings::paint_focus_ring;
use crate::ui::widgets::{self, KeyScroll, Politeness};
use crate::ui::{SearchFilters, UiCtx, View, Who};
use chrono::NaiveDate;
use eframe::egui::{
    self, text::LayoutJob, CornerRadius, CursorIcon, FontId, Key, Margin, Rect, Sense, Stroke,
    TextFormat, WidgetInfo, WidgetType,
};
use std::ops::Range;
use std::time::{Duration, Instant};
use witness_core::db::SearchHit;
use witness_core::service::{PersonInfo, SearchQuery};
use witness_core::CoreResult;

/// The backend's fixed page size for transcript hits.
const PAGE: usize = 40;
/// Typing settles for this long before the query runs.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// `.search-hit { padding: 7px 10px }`.
const HIT_PAD: egui::Vec2 = egui::vec2(10.0, 7.0);
/// `.search-hit { line-height: 1.5 }` at 14 px.
const HIT_LINE: f32 = 21.0;

// ----------------------------------------------------------------------
// The search box

/// The search box above Meetings and Search: `<input type=search>` with a
/// 250 ms debounce. Escape and the × button clear it (and so, after the
/// debounce, leave Search). Ctrl+F / `/` set `request_focus`.
pub fn search_box(ui: &mut egui::Ui, cx: &mut UiCtx, current: &View) {
    let id = egui::Id::new("search-box");
    let focused = ui.memory(|m| m.has_focus(id));
    let search = &mut cx.memory.search_box;
    let has_text = !search.text.is_empty();
    let margin = Margin {
        left: 12,
        right: if has_text { 32 } else { 12 },
        top: 7,
        bottom: 7,
    };
    // `.search-input`: --bg fill, 1 px border that turns accent on focus.
    let frame = egui::Frame::new()
        .fill(BG)
        .stroke(Stroke::new(1.0, if focused { ACCENT } else { BORDER }))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(margin);
    let response = ui.add(
        egui::TextEdit::singleline(&mut search.text)
            .id(id)
            .hint_text("Search transcripts…")
            .font(theme::regular(14.0))
            .margin(margin)
            .frame(frame)
            .desired_width(ui.available_width().min(420.0)),
    );
    widgets::text_edit_menu(&response);
    ui.ctx().accesskit_node_builder(id, |node| {
        node.set_label("Search meeting transcripts");
    });
    if std::mem::take(&mut search.request_focus) {
        response.request_focus();
    }

    let mut changed = response.changed();
    // Like `<input type=search>`, Escape clears the box (egui has already
    // taken the focus away; give it back so typing can continue).
    if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) && has_text {
        search.text.clear();
        changed = true;
        response.request_focus();
    }
    if has_text && clear_button(ui, response.rect) {
        search.text.clear();
        changed = true;
        response.request_focus();
    }

    let now = Instant::now();
    if changed {
        search.debounce_at = Some(now + DEBOUNCE);
    }
    if let Some(at) = search.debounce_at {
        if now >= at {
            search.debounce_at = None;
            let query = search.text.clone();
            if !query.trim().is_empty() {
                cx.nav.go(View::Search { query });
            } else if matches!(current, View::Search { .. }) {
                cx.nav.go(View::Meetings);
            }
        } else {
            ui.ctx().request_repaint_after(at - now);
        }
    }
}

/// The × inside the right edge of the search box; true when clicked.
fn clear_button(ui: &mut egui::Ui, field: Rect) -> bool {
    let rect = Rect::from_center_size(
        egui::pos2(field.right() - 17.0, field.center().y),
        egui::vec2(22.0, 22.0),
    );
    let response = ui.interact(rect, egui::Id::new("search-box-clear"), Sense::click());
    let hot = response.hovered() || response.has_focus();
    if hot {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), BG_HOVER);
    }
    crate::ui::icons::paint_rotated_glyph(
        ui.painter(),
        rect.center(),
        ph::X,
        14.0,
        if hot { TEXT } else { TEXT_MUTED },
        0.0,
    );
    if response.has_focus() {
        paint_focus_ring(ui, rect, 4);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Clear search"));
    response
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Clear search")
        .clicked()
}

// ----------------------------------------------------------------------
// Filters

/// A `YYYY-MM-DD` filter field as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DateInput {
    Empty,
    /// Normalized to zero-padded `YYYY-MM-DD` (the backend compares strings).
    Valid(String),
    Invalid,
}

fn parse_date(text: &str) -> DateInput {
    let text = text.trim();
    if text.is_empty() {
        return DateInput::Empty;
    }
    match NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        Ok(date) => DateInput::Valid(date.format("%Y-%m-%d").to_string()),
        Err(_) => DateInput::Invalid,
    }
}

/// The filters actually sent to the backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Applied {
    who: Who,
    date_from: Option<String>,
    date_to: Option<String>,
}

/// The new applied bound for a date field: empty clears it, a valid date
/// replaces it, and invalid (e.g. half-typed) text keeps the last one.
fn applied_date(text: &str, previous: &Option<String>) -> Option<String> {
    match parse_date(text) {
        DateInput::Empty => None,
        DateInput::Valid(date) => Some(date),
        DateInput::Invalid => previous.clone(),
    }
}

fn search_query(query: &str, offset: usize, applied: &Applied) -> SearchQuery {
    SearchQuery {
        query: query.to_owned(),
        offset: offset as i64,
        person_id: match applied.who {
            Who::Person(id) => Some(id),
            _ => None,
        },
        track: match applied.who {
            Who::Me => Some("mic"),
            Who::Them => Some("loopback"),
            _ => None,
        },
        date_from: applied.date_from.clone(),
        date_to: applied.date_to.clone(),
    }
}

/// A notes hit (no segment; opens the meeting without a seek).
fn is_note(hit: &SearchHit) -> bool {
    hit.segment_id <= 0
}

/// Hits grouped by meeting in order of first appearance (relevance order),
/// as indices into `hits`. Later hits of a meeting join its group.
fn group_hits(hits: &[SearchHit]) -> Vec<Vec<usize>> {
    let mut groups: Vec<(i64, Vec<usize>)> = Vec::new();
    for (index, hit) in hits.iter().enumerate() {
        match groups.iter_mut().find(|(id, _)| *id == hit.meeting_id) {
            Some((_, group)) => group.push(index),
            None => groups.push((hit.meeting_id, vec![index])),
        }
    }
    groups.into_iter().map(|(_, group)| group).collect()
}

/// `Results for “{q}” — {n}{+} match(es)`: (main, muted suffix).
fn heading(query: &str, count: usize, has_more: bool) -> (String, String) {
    let plus = if has_more { "+" } else { "" };
    let noun = if count == 1 { "match" } else { "matches" };
    (
        format!("Results for \u{201c}{query}\u{201d}"),
        format!(" — {count}{plus} {noun}"),
    )
}

// ----------------------------------------------------------------------
// Hit layout

/// One hit's text as a layout job, plus the char ranges of the marked
/// (matching) runs, which get rounded backgrounds painted behind them.
fn hit_job(hit: &SearchHit, wrap_width: f32) -> (LayoutJob, Vec<Range<usize>>) {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let format = |font: FontId, color| TextFormat {
        font_id: font,
        color,
        line_height: Some(HIT_LINE),
        ..Default::default()
    };
    let mut marks = Vec::new();
    let mut chars = 0usize;
    let mut push = |job: &mut LayoutJob, text: &str, fmt: TextFormat, mark: bool| {
        if text.is_empty() {
            return;
        }
        let n = text.chars().count();
        if mark {
            marks.push(chars..chars + n);
        }
        chars += n;
        job.append(text, 0.0, fmt);
    };
    if !is_note(hit) {
        push(
            &mut job,
            &format!("[{}]", fmt_ts(hit.start_ms)),
            format(theme::regular(12.0), TEXT_MUTED),
            false,
        );
    }
    push(&mut job, " ", format(theme::regular(14.0), TEXT), false);
    if let Some(speaker) = hit.speaker_name.as_deref().filter(|s| !s.is_empty()) {
        push(
            &mut job,
            &format!("{speaker}: "),
            format(theme::regular(14.0), TEXT_MUTED),
            false,
        );
    }
    for part in parse_snippet(&hit.snippet) {
        match part {
            SnippetPart::Plain(text) => {
                push(&mut job, &text, format(theme::regular(14.0), TEXT), false)
            }
            SnippetPart::Mark(text) => push(
                &mut job,
                &text,
                format(theme::regular(14.0), MARK_TEXT),
                true,
            ),
        }
    }
    (job, marks)
}

/// The rects (galley-relative) behind each marked run, one per row it spans.
fn mark_rects(galley: &egui::Galley, marks: &[Range<usize>]) -> Vec<Rect> {
    let mut rects = Vec::new();
    if marks.is_empty() {
        return rects;
    }
    let mut index = 0usize;
    for placed in &galley.rows {
        let mut run: Option<Rect> = None;
        for glyph in &placed.row.glyphs {
            let marked = marks.iter().any(|m| m.contains(&index));
            index += 1;
            if marked {
                let top = glyph.pos.y - glyph.font_ascent;
                let rect = Rect::from_min_max(
                    egui::pos2(glyph.pos.x, top),
                    egui::pos2(glyph.max_x(), top + glyph.font_height),
                )
                .translate(placed.pos.to_vec2());
                run = Some(run.map_or(rect, |r| r.union(rect)));
            } else if let Some(r) = run.take() {
                rects.push(r);
            }
        }
        rects.extend(run);
        if placed.ends_with_newline {
            index += 1;
        }
    }
    rects
}

/// The "note" chip (`.chip.chip-bookmark`): size, then painting.
fn note_chip(ui: &egui::Ui) -> std::sync::Arc<egui::Galley> {
    ui.painter()
        .layout_no_wrap("note".into(), theme::semibold(11.0), BOOKMARK_GLYPH)
}

/// One `.search-hit` button; true when activated.
fn hit_row(ui: &mut egui::Ui, hit: &SearchHit) -> bool {
    let width = ui.available_width();
    let chip = is_note(hit).then(|| note_chip(ui));
    let chip_size = chip
        .as_ref()
        .map(|g| g.size() + egui::vec2(14.0, 6.0))
        .unwrap_or_default();
    let (mut job, marks) = hit_job(hit, (width - 2.0 * HIT_PAD.x).max(40.0));
    if let Some(first) = job.sections.first_mut() {
        first.leading_space = chip_size.x;
    }
    let plain = job.text.clone();
    let galley = ui.painter().layout_job(job);
    let size = egui::vec2(width, galley.size().y + 2.0 * HIT_PAD.y);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if response.hovered() || response.has_focus() {
            painter.rect_filled(rect, CornerRadius::same(6), BG_HOVER);
        }
        let origin = rect.min + HIT_PAD;
        // `mark`: rgba(213,160,33,.35) behind, radius 3, padding 0 2px.
        for mark in mark_rects(&galley, &marks) {
            painter.rect_filled(
                mark.translate(origin.to_vec2())
                    .expand2(egui::vec2(2.0, 0.0)),
                CornerRadius::same(3),
                MARK_BG,
            );
        }
        if let (Some(chip), Some(first)) = (chip, galley.rows.first()) {
            let row = first.rect().translate(origin.to_vec2());
            let chip_rect = Rect::from_min_size(
                egui::pos2(origin.x, row.center().y - chip_size.y / 2.0),
                chip_size,
            );
            painter.rect_filled(chip_rect, CornerRadius::same(255), WARN);
            painter.galley(chip_rect.center() - chip.size() / 2.0, chip, BOOKMARK_GLYPH);
        }
        painter.galley(origin, galley, TEXT);
        if response.has_focus() {
            paint_focus_ring(ui, rect, 6);
        }
    }
    let label = if is_note(hit) {
        format!("note{plain}")
    } else {
        plain
    };
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label.trim()));
    response.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

// ----------------------------------------------------------------------
// The view

struct SearchLoad {
    task: Task<CoreResult<Vec<SearchHit>>>,
    append: bool,
}

enum Action {
    Open {
        meeting_id: i64,
        segment_id: Option<i64>,
    },
    LoadMore,
    TryAgain,
}

pub struct SearchView {
    /// The query of the current results.
    query: String,
    /// Last valid dates (invalid text keeps these applied).
    date_from: Option<String>,
    date_to: Option<String>,
    /// The query + filters last searched; None until the first pass.
    searched: Option<(String, Applied)>,
    hits: Vec<SearchHit>,
    has_more: bool,
    error: Option<String>,
    load: Option<SearchLoad>,
    people: Vec<PersonInfo>,
    people_load: Option<Task<CoreResult<Vec<PersonInfo>>>>,
    people_requested: bool,
}

impl SearchView {
    pub fn new(query: String) -> Self {
        SearchView {
            query,
            date_from: None,
            date_to: None,
            searched: None,
            hits: Vec::new(),
            has_more: false,
            error: None,
            load: None,
            people: Vec::new(),
            people_load: None,
            people_requested: false,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if !self.people_requested {
            self.people_requested = true;
            self.people_load = Some(cx.tasks.spawn("list-people", |c| c.list_people()));
        }
        if let View::Search { query } = cx.view {
            if *query != self.query {
                self.query = query.clone();
            }
        }
        self.poll(ui, cx);

        // Re-search when the query or an applied filter changes.
        let filters = &cx.memory.search_filters;
        self.date_from = applied_date(&filters.date_from, &self.date_from);
        self.date_to = applied_date(&filters.date_to, &self.date_to);
        let applied = Applied {
            who: filters.who,
            date_from: self.date_from.clone(),
            date_to: self.date_to.clone(),
        };
        let key = (self.query.clone(), applied);
        if self.searched.as_ref() != Some(&key) {
            self.searched = Some(key);
            self.hits.clear();
            self.has_more = false;
            self.start(cx, 0, false);
        }

        let mut actions = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("search-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::keyboard_scroll(ui, KeyScroll { space: true });
                widgets::centered_column(ui, 900.0, |ui| self.draw(ui, cx, &mut actions));
            });
        for action in actions {
            match action {
                Action::Open {
                    meeting_id,
                    segment_id,
                } => {
                    let query = self.query.clone();
                    cx.nav.go(View::Transcript {
                        meeting_id,
                        segment_id,
                        highlight: Some(query.clone()),
                        from: Some(Box::new(View::Search { query })),
                    });
                }
                Action::LoadMore => {
                    let offset = self.hits.iter().filter(|h| !is_note(h)).count();
                    self.start(cx, offset, true);
                }
                Action::TryAgain => self.start(cx, 0, false),
            }
        }
    }

    /// Nothing to persist.
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

    /// Leaving Search drops a pending search-box debounce, which would
    /// otherwise fire on a later visit.
    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
        cx.memory.search_box.debounce_at = None;
    }

    fn loading(&self) -> bool {
        self.load.is_some()
    }

    /// `load(offset, append)` with the current query and filters.
    fn start(&mut self, cx: &UiCtx, offset: usize, append: bool) {
        let Some((query, applied)) = &self.searched else {
            return;
        };
        let q = search_query(query, offset, applied);
        let task = cx.tasks.spawn("search", move |c| c.search(&q));
        self.load = Some(SearchLoad { task, append });
    }

    fn poll(&mut self, ui: &egui::Ui, cx: &mut UiCtx) {
        let finished = self.load.as_mut().and_then(|l| l.task.poll_flat());
        if let Some(result) = finished {
            let append = self.load.take().is_some_and(|l| l.append);
            match result {
                Ok(rows) => {
                    let transcript_hits = rows.iter().filter(|h| !is_note(h)).count();
                    cx.fonts.note_texts(
                        ui.ctx(),
                        rows.iter().flat_map(|h| {
                            [
                                h.meeting_title.as_str(),
                                h.snippet.as_str(),
                                h.speaker_name.as_deref().unwrap_or(""),
                            ]
                        }),
                    );
                    self.error = None;
                    self.has_more = transcript_hits >= PAGE;
                    if append {
                        self.hits.extend(rows);
                    } else {
                        self.hits = rows;
                    }
                }
                Err(error) => {
                    cx.toasts.error("Could not search transcripts", &error);
                    self.error = Some(error);
                }
            }
        }
        let finished = self.people_load.as_mut().and_then(|t| t.poll_flat());
        if let Some(result) = finished {
            self.people_load = None;
            match result {
                Ok(people) => {
                    cx.fonts
                        .note_texts(ui.ctx(), people.iter().map(|p| p.name.as_str()));
                    self.people = people;
                }
                Err(error) => cx
                    .toasts
                    .error("Could not load people for search filters", error),
            }
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx, actions: &mut Vec<Action>) {
        // h2.search-heading (aria-live polite).
        let (main, suffix) = heading(&self.query, self.hits.len(), self.has_more);
        let mut job = LayoutJob::default();
        job.append(&main, 0.0, TextFormat::simple(theme::semibold(17.0), TEXT));
        job.append(
            &suffix,
            0.0,
            TextFormat::simple(theme::semibold(17.0), TEXT_MUTED),
        );
        let response = ui.add(egui::Label::new(job).wrap());
        widgets::announce(
            ui.ctx(),
            response.id.with("live"),
            &format!("{main}{suffix}"),
            Politeness::Polite,
        );
        ui.add_space(4.0);

        self.draw_filters(ui, cx);
        ui.add_space(10.0);

        if let Some(error) = &self.error {
            let message = format!("Search failed: {error}");
            if widgets::view_error(ui, &message, &["Try again"]).is_some() {
                actions.push(Action::TryAgain);
            }
            return;
        }
        let loading = self.loading();
        if self.hits.is_empty() {
            let text = if loading {
                "Searching…"
            } else {
                "No matches."
            };
            widgets::empty_state(ui, "", Some(text));
            return;
        }

        for group in group_hits(&self.hits) {
            let first = &self.hits[group[0]];
            group_title(ui, first);
            ui.spacing_mut().item_spacing.y = 0.0;
            for &index in &group {
                let hit = &self.hits[index];
                if hit_row(ui, hit) {
                    actions.push(Action::Open {
                        meeting_id: hit.meeting_id,
                        segment_id: (!is_note(hit)).then_some(hit.segment_id),
                    });
                }
            }
            ui.add_space(18.0);
        }

        if self.has_more {
            let label = if loading { "Loading…" } else { "Load more" };
            let clicked = ui
                .add_enabled_ui(!loading, |ui| widgets::btn_ghost(ui, label))
                .inner
                .clicked();
            if clicked {
                actions.push(Action::LoadMore);
            }
        }
    }

    /// `.search-filters`: Who, from, to, Clear filters (gap 12, wraps).
    fn draw_filters(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let people = &self.people;
        let filters: &mut SearchFilters = &mut cx.memory.search_filters;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let who_label = |who: Who| -> String {
                match who {
                    Who::Anyone => "Anyone".into(),
                    Who::Me => "Me".into(),
                    Who::Them => "Them".into(),
                    Who::Person(id) => people
                        .iter()
                        .find(|p| p.id == id)
                        .map_or_else(|| "Anyone".into(), |p| p.name.clone()),
                }
            };
            let combo = egui::ComboBox::from_id_salt("search-who")
                .selected_text(egui::RichText::new(who_label(filters.who)).size(12.0))
                .width(110.0)
                .show_ui(ui, |ui| {
                    let mut option = |ui: &mut egui::Ui, value: Who| {
                        ui.selectable_value(&mut filters.who, value, who_label(value));
                    };
                    option(ui, Who::Anyone);
                    option(ui, Who::Me);
                    option(ui, Who::Them);
                    for person in people {
                        option(ui, Who::Person(person.id));
                    }
                });
            let combo = combo.response.on_hover_text("Who said it");
            ui.ctx().accesskit_node_builder(combo.id, |node| {
                node.set_label("Filter by speaker");
            });

            date_field(ui, "from", "search-date-from", &mut filters.date_from);
            date_field(ui, "to", "search-date-to", &mut filters.date_to);

            let any = filters.who != Who::Anyone
                || !filters.date_from.is_empty()
                || !filters.date_to.is_empty();
            if any && widgets::btn_ghost(ui, "Clear filters").clicked() {
                *filters = SearchFilters::default();
            }
        });
    }
}

/// `from [YYYY-MM-DD]`: a validated text field replacing `<input
/// type=date>`. Invalid input gets a red border and is not applied.
fn date_field(ui: &mut egui::Ui, label: &str, id: &str, text: &mut String) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(egui::RichText::new(label).color(TEXT_MUTED));
        let id = egui::Id::new(id);
        let focused = ui.memory(|m| m.has_focus(id));
        let invalid = parse_date(text) == DateInput::Invalid;
        let border = if invalid {
            REC
        } else if focused {
            ACCENT
        } else {
            BORDER
        };
        let margin = Margin::symmetric(8, 4);
        let response = ui.add(
            egui::TextEdit::singleline(text)
                .id(id)
                .hint_text("YYYY-MM-DD")
                .font(theme::regular(12.0))
                .char_limit(10)
                .margin(margin)
                .frame(
                    egui::Frame::new()
                        .fill(BG)
                        .stroke(Stroke::new(1.0, border))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(margin),
                )
                .desired_width(104.0),
        );
        widgets::text_edit_menu(&response);
        let name = if invalid {
            format!("{label} (YYYY-MM-DD, invalid date)")
        } else {
            format!("{label} (YYYY-MM-DD)")
        };
        ui.ctx().accesskit_node_builder(id, |node| {
            node.set_label(name.as_str());
        });
        if invalid {
            response.on_hover_text("Enter a date as YYYY-MM-DD");
        }
    });
}

/// `h3.search-group-title`: title + muted ` · date`, wrapping, 1 px rule.
fn group_title(ui: &mut egui::Ui, hit: &SearchHit) {
    ui.add_space(6.0);
    let mut job = LayoutJob::default();
    job.append(
        &hit.meeting_title,
        0.0,
        TextFormat::simple(theme::semibold(14.0), TEXT),
    );
    job.append(
        &format!(" · {}", fmt_date(&hit.started_at)),
        0.0,
        TextFormat::simple(theme::semibold(14.0), TEXT_MUTED),
    );
    let response = ui.add(egui::Label::new(job).wrap());
    response
        .widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, hit.meeting_title.as_str()));
    ui.add_space(6.0);
    let (_, line) = ui.allocate_space(egui::vec2(ui.available_width(), 1.0));
    ui.painter().line_segment(
        [line.left_center(), line.right_center()],
        Stroke::new(1.0, BORDER),
    );
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(meeting_id: i64, segment_id: i64, snippet: &str) -> SearchHit {
        SearchHit {
            meeting_id,
            meeting_title: format!("Meeting {meeting_id}"),
            started_at: "2026-10-05T09:00:00+01:00".into(),
            segment_id,
            start_ms: 65_000,
            speaker_name: if segment_id > 0 {
                Some("Alice".into())
            } else {
                Some("Notes".into())
            },
            snippet: snippet.into(),
        }
    }

    #[test]
    fn dates_validate_and_normalize() {
        assert_eq!(parse_date(""), DateInput::Empty);
        assert_eq!(parse_date("  "), DateInput::Empty);
        assert_eq!(
            parse_date("2026-10-05"),
            DateInput::Valid("2026-10-05".into())
        );
        assert_eq!(
            parse_date("2026-1-5"),
            DateInput::Valid("2026-01-05".into())
        );
        assert_eq!(parse_date("2026-10"), DateInput::Invalid);
        assert_eq!(parse_date("2026-02-30"), DateInput::Invalid);
        assert_eq!(parse_date("05/10/2026"), DateInput::Invalid);
    }

    #[test]
    fn half_typed_dates_keep_the_last_applied_bound() {
        let last = Some("2026-10-05".to_string());
        assert_eq!(applied_date("2026-1", &last), last);
        assert_eq!(applied_date("", &last), None);
        assert_eq!(applied_date("2026-11-01", &last), Some("2026-11-01".into()));
        assert_eq!(applied_date("nope", &None), None);
    }

    #[test]
    fn who_maps_to_track_or_person() {
        let mut applied = Applied::default();
        let q = search_query("budget", 0, &applied);
        assert_eq!((q.person_id, q.track), (None, None));
        applied.who = Who::Me;
        assert_eq!(search_query("b", 0, &applied).track, Some("mic"));
        applied.who = Who::Them;
        assert_eq!(search_query("b", 0, &applied).track, Some("loopback"));
        applied.who = Who::Person(7);
        applied.date_to = Some("2026-10-05".into());
        let q = search_query("b", 40, &applied);
        assert_eq!((q.person_id, q.track, q.offset), (Some(7), None, 40));
        assert_eq!(q.date_to.as_deref(), Some("2026-10-05"));
        assert_eq!(q.date_from, None);
    }

    #[test]
    fn groups_keep_first_appearance_order() {
        let hits = vec![
            hit(2, 10, "a"),
            hit(1, 11, "b"),
            hit(2, 12, "c"),
            hit(3, -1, "note"),
            hit(1, -1, "note"),
        ];
        assert_eq!(group_hits(&hits), vec![vec![0, 2], vec![1, 4], vec![3]]);
        assert!(group_hits(&[]).is_empty());
    }

    #[test]
    fn heading_counts_and_plurals() {
        assert_eq!(
            heading("budget ", 1, false),
            (
                "Results for \u{201c}budget \u{201d}".into(),
                " — 1 match".into()
            )
        );
        assert_eq!(heading("q", 0, false).1, " — 0 matches");
        assert_eq!(heading("q", 40, true).1, " — 40+ matches");
    }

    #[test]
    fn hit_text_marks_the_matches() {
        let (job, marks) = hit_job(&hit(1, 5, "the \x01budget\x02 plan"), 300.0);
        assert_eq!(job.text, "[1:05] Alice: the budget plan");
        assert_eq!(marks.len(), 1);
        let marked: String = job
            .text
            .chars()
            .skip(marks[0].start)
            .take(marks[0].len())
            .collect();
        assert_eq!(marked, "budget");

        let (job, marks) = hit_job(&hit(1, -1, "\x01notes\x02"), 300.0);
        assert_eq!(job.text, " Notes: notes");
        assert_eq!(marks, vec![8..13]);
    }

    #[test]
    fn mark_rects_follow_wrapped_rows() {
        let ctx = theme::test_context();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let snippet = "word ".repeat(30) + "\x01match\x02 tail";
            let (job, marks) = hit_job(&hit(1, 5, &snippet), 120.0);
            let galley = ui.painter().layout_job(job);
            assert!(galley.rows.len() > 2, "the snippet wraps");
            let rects = mark_rects(&galley, &marks);
            assert_eq!(rects.len(), 1);
            let second_row = galley.rows[1].pos.y;
            assert!(rects[0].top() > second_row, "on a later row");
            assert!(rects[0].width() > 0.0);
            assert!(!hit_row(ui, &hit(1, -1, "\x01n\x02")));
        });
        output.textures_delta.clear();
    }
}
