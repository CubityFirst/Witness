//! Full-text search results (map-ui-meetings §2, search.tsx) and the
//! shell's search box (app.tsx `onSearchInput`).
//!
//! The box edits the app-owned `cx.memory.search_box`, so the top bar's
//! Meetings button can clear it and cancel its 250 ms debounce. The view
//! reads its query from the current `View::Search { query }` (a new query
//! does not remount it) and re-searches whenever the query or an applied
//! filter changes; old results are dropped first so they never show under
//! a new heading. Search never auto-refreshes on `refresh_tick` (parity).
//!
//! The hits take the Meetings list's keyboard selection (↑/↓, j/k,
//! Home/End, Enter opens) in display order; in a split the meeting open
//! beside the results has its group marked. The first page shows a
//! skeleton of the result layout, and no results a designed empty state.

use crate::bridge::Task;
use crate::ui::format::{fmt_ts, parse_snippet, SnippetPart};
use crate::ui::icons::{self, ph};
use crate::ui::theme::{self, pal};
use crate::ui::views::meetings::{
    debug_flag, debug_index, empty_hero, follow_focus, list_keys_free, load_more_button,
    paint_focus_ring, paint_selection, read_list_keys, skeleton_at, skeleton_due, step_selection,
    ListKeys,
};
use crate::ui::widgets::{self, KeyScroll, Politeness};
use crate::ui::{PaneRole, SearchFilters, UiCtx, View, Who};
use chrono::{DateTime, Local, NaiveDate};
use eframe::egui::{
    self, text::LayoutJob, text::TextWrapping, CornerRadius, CursorIcon, Key, Margin, Rect, Sense,
    Stroke, StrokeKind, TextFormat, WidgetInfo, WidgetType,
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
/// Snippet line height (14 pt text, 1.45).
const HIT_LINE: f32 = 20.0;
/// Row text inset (`widgets::list_row`'s 12 pt): group headers align with it.
const ROW_INSET: f32 = 12.0;
/// The search box's left padding, which holds the magnifier.
const BOX_ICON_PAD: i8 = 34;
/// Filter controls' widths in the wrapping (wide) filter row.
const WHO_WIDTH: f32 = 160.0;
const DATE_WIDTH: f32 = 112.0;
/// The label column of the stacked (compact) filters.
const STACK_LABEL: f32 = 64.0;

// ----------------------------------------------------------------------
// The search box

/// The search box above Meetings and Search: `<input type=search>` with a
/// 250 ms debounce. Escape and the × button clear it (and so, after the
/// debounce, leave Search). Ctrl+F / `/` set `request_focus`.
pub fn search_box(ui: &mut egui::Ui, cx: &mut UiCtx, current: &View) {
    let id = egui::Id::new("search-box");
    let focused = ui.memory(|m| m.has_focus(id));
    let modal_open = cx.confirm.is_open();
    let search = &mut cx.memory.search_box;
    let has_text = !search.text.is_empty();
    // One input across the column: a leading magnifier in the left
    // padding, the × in the right one while there is text.
    let margin = Margin {
        left: BOX_ICON_PAD,
        right: if has_text { BOX_ICON_PAD } else { 12 },
        top: 7,
        bottom: 7,
    };
    let edit = egui::TextEdit::singleline(&mut search.text)
        .hint_text("Search transcripts…")
        .font(theme::regular(14.0))
        // `<input type=search>` keeps focus on Enter.
        .return_key(None::<egui::KeyboardShortcut>)
        .desired_width(f32::INFINITY);
    let edit = widgets::framed_text_edit(ui, id, edit, pal().bg_raised, theme::R_MD, margin);
    let response = ui.add(edit);
    icons::paint_rotated_glyph(
        ui.painter(),
        egui::pos2(
            response.rect.left() + f32::from(BOX_ICON_PAD) / 2.0 + 1.0,
            response.rect.center().y,
        ),
        ph::MAGNIFYING_GLASS,
        15.0,
        if focused {
            pal().text_muted
        } else {
            pal().text_faint
        },
        0.0,
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
    // `focused` is from before the field ran, so this catches Escape both
    // when egui drops the focus up front and when the field surrenders it.
    let escape = (focused || response.has_focus() || response.lost_focus())
        && ui.input(|i| i.key_pressed(Key::Escape));
    if escape && has_text {
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
    // Held while a confirmation is open: navigating would remount the view
    // that asked and drop its answer (closing the modal repaints).
    if let Some(at) = search.debounce_at.filter(|_| !modal_open) {
        if now >= at {
            search.debounce_at = None;
            // `apply_navigation` repaints after the pass.
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
        egui::vec2(24.0, 24.0),
    );
    let response = ui.interact(rect, egui::Id::new("search-box-clear"), Sense::click());
    let hot = response.hovered() || response.has_focus();
    if hot {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(theme::R_SM), pal().bg_hover);
    }
    crate::ui::icons::paint_rotated_glyph(
        ui.painter(),
        rect.center(),
        ph::X,
        14.0,
        if hot { pal().text } else { pal().text_muted },
        0.0,
    );
    if response.has_focus() {
        paint_focus_ring(ui, rect, theme::R_SM);
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

/// The applied bound for a date field: only a valid date applies; empty
/// and invalid (e.g. half-typed) text apply none, like the empty value of
/// an incomplete `<input type=date>`. The field shows invalid text red.
fn applied_date(text: &str) -> Option<String> {
    match parse_date(text) {
        DateInput::Valid(date) => Some(date),
        DateInput::Empty | DateInput::Invalid => None,
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
    (heading_main(query), format!(" — {count}{plus} {noun}"))
}

/// The heading while the first page is on its way (no count yet).
const SEARCHING: &str = " — searching…";

/// `Results for “{q}”`.
fn heading_main(query: &str) -> String {
    format!("Results for \u{201c}{query}\u{201d}")
}

/// The no-results state: (title, sentence, button). With filters set, the
/// way out is clearing them; otherwise a hint at what search covers.
fn no_results(query: &str, filtered: bool) -> (String, &'static str, Option<&'static str>) {
    // Long queries are cut: the title is a heading, not a transcript.
    const MAX: usize = 32;
    let mut shown: String = query.trim().chars().take(MAX).collect();
    if query.trim().chars().count() > MAX {
        shown.push('…');
    }
    let title = format!("No matches for \u{201c}{shown}\u{201d}");
    if filtered {
        (
            title,
            "Nothing matches with the current filters. Clear them to search every meeting.",
            Some("Clear filters"),
        )
    } else {
        (
            title,
            "Search covers every transcript and your meeting notes. Try another word, or fewer of them.",
            None,
        )
    }
}

// ----------------------------------------------------------------------
// Hit layout

/// One hit's snippet as a layout job wrapped at `wrap_width`, plus the
/// char ranges of the marked (matching) runs, which get rounded accent
/// backgrounds painted behind them. The timestamp and speaker go on the
/// row's meta line ([`hit_meta_job`]).
fn hit_job(hit: &SearchHit, wrap_width: f32) -> (LayoutJob, Vec<Range<usize>>) {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let format = |color| TextFormat {
        font_id: theme::regular(14.0),
        color,
        line_height: Some(HIT_LINE),
        ..Default::default()
    };
    let mut marks = Vec::new();
    let mut chars = 0usize;
    for part in parse_snippet(&hit.snippet) {
        let (text, mark) = match part {
            SnippetPart::Plain(text) => (text, false),
            SnippetPart::Mark(text) => (text, true),
        };
        if text.is_empty() {
            continue;
        }
        let n = text.chars().count();
        if mark {
            marks.push(chars..chars + n);
        }
        chars += n;
        job.append(
            &text,
            0.0,
            format(if mark { pal().mark_text } else { pal().text }),
        );
    }
    (job, marks)
}

/// The meta line above a snippet: the muted timestamp and the speaker
/// (`"1:37  Christopher Warriner"`), or "Meeting notes" for a notes hit;
/// one line, elided at `width`.
fn hit_meta_job(hit: &SearchHit, width: f32) -> LayoutJob {
    let mut job = LayoutJob {
        wrap: TextWrapping::truncate_at_width(width.max(1.0)),
        ..Default::default()
    };
    if is_note(hit) {
        job.append(
            "Meeting notes",
            0.0,
            TextFormat::simple(theme::semibold(12.0), pal().text_muted),
        );
        return job;
    }
    job.append(
        &fmt_ts(hit.start_ms),
        0.0,
        TextFormat::simple(theme::regular(12.0), pal().text_faint),
    );
    if let Some(speaker) = hit.speaker_name.as_deref().filter(|s| !s.is_empty()) {
        job.append(
            speaker,
            8.0,
            TextFormat::simple(theme::semibold(12.0), pal().text_muted),
        );
    }
    job
}

/// A hit's accessible name: `"[1:05] Alice: the budget plan"`, or
/// `"note Notes: …"` for a notes hit (as in 0.2.x).
fn hit_label(hit: &SearchHit) -> String {
    let mut label = if is_note(hit) {
        "note ".to_owned()
    } else {
        format!("[{}] ", fmt_ts(hit.start_ms))
    };
    if let Some(speaker) = hit.speaker_name.as_deref().filter(|s| !s.is_empty()) {
        label.push_str(speaker);
        label.push_str(": ");
    }
    for part in parse_snippet(&hit.snippet) {
        match part {
            SnippetPart::Plain(text) | SnippetPart::Mark(text) => label.push_str(&text),
        }
    }
    label.trim().to_owned()
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

/// One hit: a flat [`widgets::list_row`] with the meta line over the
/// wrapped snippet, filled while it has the keyboard selection; clicked
/// when activated (click, or Enter / Space while focused).
fn hit_row(ui: &mut egui::Ui, index: usize, hit: &SearchHit, selected: bool) -> egui::Response {
    let (response, ()) = widgets::list_row(ui, ("search-hit", index), selected, |ui, _| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let width = ui.available_width();
        let meta = ui.painter().layout_job(hit_meta_job(hit, width));
        ui.add(egui::Label::new(meta).selectable(false));

        let (job, marks) = hit_job(hit, width.max(40.0));
        let galley = ui.painter().layout_job(job);
        let (rect, _) = ui.allocate_exact_size(galley.size(), Sense::hover());
        if ui.is_rect_visible(rect) {
            // Matches: an accent tint behind, radius 3, 2 pt either side.
            for mark in mark_rects(&galley, &marks) {
                ui.painter().rect_filled(
                    mark.translate(rect.min.to_vec2())
                        .expand2(egui::vec2(2.0, 0.0)),
                    CornerRadius::same(3),
                    pal().mark_bg,
                );
            }
            ui.painter().galley(rect.min, galley, pal().text);
        }
    });
    let label = hit_label(hit);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &label));
    follow_focus(&response);
    response
}

/// A meeting group's header: the meeting title as an elided link (opens
/// the meeting) with its short date muted on the right. `open`: the
/// meeting is open beside the results (a split), marked with the selected
/// fill and the accent pill. Returns the header's rect and whether the
/// link was activated.
fn group_title(ui: &mut egui::Ui, hit: &SearchHit, today: NaiveDate, open: bool) -> (Rect, bool) {
    let width = ui.available_width();
    let inner = (width - 2.0 * ROW_INSET).max(40.0);
    let date = ui.painter().layout_no_wrap(
        group_date(&hit.started_at, today),
        theme::regular(12.0),
        pal().text_muted,
    );
    // The date gives way first on very narrow columns.
    let show_date = date.size().x + theme::S3 <= inner * 0.45;
    let title_width = if show_date {
        inner - date.size().x - theme::S3
    } else {
        inner
    };
    let font = theme::semibold(14.0);
    let mut job = LayoutJob::simple_singleline(hit.meeting_title.clone(), font, pal().text);
    job.wrap = TextWrapping::truncate_at_width(title_width.max(1.0));
    let title = ui.painter().layout_job(job);
    let height = title.size().y.max(date.size().y) + 8.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
    let title_rect = Rect::from_min_size(
        egui::pos2(
            rect.left() + ROW_INSET,
            rect.center().y - title.size().y / 2.0,
        ),
        title.size(),
    );
    let id = ui.id().with(("search-group", hit.meeting_id));
    let response = ui.interact(title_rect, id, Sense::click());
    let hot = response.hovered();
    let color = if hot { pal().accent } else { pal().text };
    if ui.is_rect_visible(rect) {
        if open {
            paint_selection(ui, rect, true);
        }
        let painter = ui.painter();
        let elided = title.elided;
        painter.galley(title_rect.min, title, color);
        if hot {
            let y = title_rect.bottom() - 1.0;
            painter.hline(title_rect.x_range(), y, Stroke::new(1.0, pal().accent));
        }
        if show_date {
            painter.galley(
                egui::pos2(
                    rect.right() - ROW_INSET - date.size().x,
                    rect.center().y - date.size().y / 2.0,
                ),
                date,
                pal().text_muted,
            );
        }
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            paint_focus_ring(ui, title_rect.expand2(egui::vec2(2.0, 0.0)), theme::R_SM);
        }
        if elided {
            response.clone().on_hover_text(&hit.meeting_title);
        }
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Link, true, &hit.meeting_title));
    follow_focus(&response);
    (
        rect,
        response.on_hover_cursor(CursorIcon::PointingHand).clicked(),
    )
}

/// The first page's placeholder, on the real layout: meeting groups (a
/// title bar with the date bar on the right) of hit rows (a short meta bar
/// over two snippet lines).
fn results_skeleton(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let inner = (width - 2.0 * ROW_INSET).max(40.0);
    let measure = inner.min(560.0);
    let (title_h, meta_h) = ui.fonts_mut(|f| {
        (
            f.row_height(&theme::semibold(14.0)),
            f.row_height(&theme::regular(12.0)),
        )
    });
    let header_h = title_h + 8.0;
    // list_row's 8 pt padding, the meta line, 2 pt, two snippet lines.
    let hit_h = 8.0 + meta_h + 2.0 + 2.0 * HIT_LINE + 8.0;
    let bar = |left: f32, center_y: f32, w: f32, h: f32| {
        Rect::from_min_size(egui::pos2(left, center_y - h / 2.0), egui::vec2(w, h))
    };
    ui.spacing_mut().item_spacing.y = 0.0;
    for (group, hits) in [2usize, 1, 2].into_iter().enumerate() {
        if group > 0 {
            ui.add_space(theme::S4);
        }
        let (header, _) = ui.allocate_exact_size(egui::vec2(width, header_h), Sense::hover());
        let left = header.left() + ROW_INSET;
        let title = measure * [0.52, 0.4, 0.6][group];
        skeleton_at(ui, bar(left, header.center().y, title, 12.0));
        // The date gives way first on narrow columns, as in `group_title`.
        if 96.0 + theme::S3 <= inner * 0.45 {
            let date = bar(
                header.right() - ROW_INSET - 96.0,
                header.center().y,
                96.0,
                9.0,
            );
            skeleton_at(ui, date);
        }
        for hit in 0..hits {
            ui.add_space(2.0);
            let (row, _) = ui.allocate_exact_size(egui::vec2(width, hit_h), Sense::hover());
            let mut y = row.top() + 8.0 + meta_h / 2.0;
            skeleton_at(ui, bar(left, y, [88.0_f32, 64.0][hit % 2].min(inner), 9.0));
            y += meta_h / 2.0 + 2.0 + HIT_LINE / 2.0;
            for line in [0.94, [0.58, 0.72][(group + hit) % 2]] {
                skeleton_at(ui, bar(left, y, inner * line, 11.0));
                y += HIT_LINE;
            }
        }
    }
}

/// A result group's short date: `"Wed 22 Jul, 12:10"` (local), with the
/// year when it isn't this one; unparseable input unchanged.
fn group_date(rfc3339: &str, today: NaiveDate) -> String {
    use chrono::Datelike;
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(at) => {
            let at = at.with_timezone(&Local);
            if at.year() == today.year() {
                at.format("%a %-d %b, %H:%M").to_string()
            } else {
                at.format("%a %-d %b %Y, %H:%M").to_string()
            }
        }
        Err(_) => rfc3339.to_owned(),
    }
}

/// The results heading (`Results for “q”` + the muted `suffix`), wrapping
/// onto at most two lines. A query too long for that is shortened inside
/// its quotes (`“very long que…”`) so the suffix (the match count) always
/// shows; the tooltip has the whole heading.
fn heading_galley(
    ui: &egui::Ui,
    query: &str,
    suffix: &str,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let layout = |query: &str| {
        let main = heading_main(query);
        let mut job = LayoutJob::default();
        job.append(
            &main,
            0.0,
            TextFormat::simple(theme::semibold(17.0), pal().text),
        );
        job.append(
            suffix,
            0.0,
            TextFormat::simple(theme::semibold(17.0), pal().text_muted),
        );
        job.wrap = TextWrapping {
            max_width: width.max(40.0),
            max_rows: 2,
            break_anywhere: false,
            overflow_character: Some('…'),
        };
        ui.painter().layout_job(job)
    };
    let galley = layout(query);
    if !galley.elided {
        return galley;
    }
    // The longest prefix of the query that fits (binary search on chars).
    let chars: Vec<char> = query.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let short: String = chars[..mid].iter().collect::<String>() + "…";
        if layout(&short).elided {
            hi = mid - 1;
        } else {
            lo = mid;
        }
    }
    let short: String = chars[..lo].iter().collect::<String>() + "…";
    layout(&short)
}

// ----------------------------------------------------------------------
// The view

struct SearchLoad {
    task: Task<CoreResult<Vec<SearchHit>>>,
    append: bool,
    /// When it was requested (the first page's skeleton waits a moment).
    started: Instant,
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Open {
        meeting_id: i64,
        segment_id: Option<i64>,
        /// The hit opened (None: a group title), which takes the selection.
        hit: Option<usize>,
    },
    LoadMore,
    TryAgain,
    /// The no-results state's way out.
    ClearFilters,
}

/// The `Open` action for hit `index`: its meeting, seeking to its segment
/// (a notes hit opens the meeting without a seek).
fn open_hit(hits: &[SearchHit], index: usize) -> Action {
    let hit = &hits[index];
    Action::Open {
        meeting_id: hit.meeting_id,
        segment_id: (!is_note(hit)).then_some(hit.segment_id),
        hit: Some(index),
    }
}

pub struct SearchView {
    /// The query of the current results.
    query: String,
    /// The query + filters last searched; None until the first pass.
    searched: Option<(String, Applied)>,
    hits: Vec<SearchHit>,
    has_more: bool,
    error: Option<String>,
    load: Option<SearchLoad>,
    people: Vec<PersonInfo>,
    people_load: Option<Task<CoreResult<Vec<PersonInfo>>>>,
    people_requested: bool,
    /// The people list arrived (so a Who id missing from it is stale).
    people_loaded: bool,
    /// The compact layout's "Filters" disclosure is open.
    filters_open: bool,
    /// The keyboard selection: an index into `hits` (↑/↓ walk the hits in
    /// display order, Enter opens). Reset with the results.
    cursor: Option<usize>,
    /// Scroll the selected hit into view on the next draw.
    reveal: bool,
    /// The meeting open beside the results (a split's `PaneRole::List`).
    pane_open: Option<i64>,
    /// `pane_open` as last seen, so only a change scrolls to it.
    pane_seen: Option<i64>,
    /// Scroll this meeting's group into view (once its results are in).
    reveal_group: Option<i64>,
    /// `WITNESS_DEBUG_SELECT` has been applied.
    debug_selected: bool,
}

impl SearchView {
    pub fn new(query: String) -> Self {
        SearchView {
            query,
            searched: None,
            hits: Vec::new(),
            has_more: false,
            error: None,
            load: None,
            people: Vec::new(),
            people_load: None,
            people_requested: false,
            people_loaded: false,
            filters_open: debug_filters_open(),
            cursor: None,
            reveal: false,
            pane_open: None,
            pane_seen: None,
            reveal_group: None,
            debug_selected: false,
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

        // A person filter whose person is gone (forgotten since it was
        // chosen) would filter by a stale id under an "Anyone"-looking
        // control: drop it once the people list says so.
        if let Who::Person(id) = cx.memory.search_filters.who {
            if self.people_loaded && !self.people.iter().any(|p| p.id == id) {
                cx.memory.search_filters.who = Who::Anyone;
            }
        }

        // Re-search when the query or an applied filter changes.
        let filters = &cx.memory.search_filters;
        let applied = Applied {
            who: filters.who,
            date_from: applied_date(&filters.date_from),
            date_to: applied_date(&filters.date_to),
        };
        let key = (self.query.clone(), applied);
        if self.searched.as_ref() != Some(&key) {
            self.searched = Some(key);
            self.hits.clear();
            self.has_more = false;
            self.cursor = None;
            self.start(cx, 0, false);
        }
        self.debug_select();
        let open = self.pane_selection(cx.pane);
        self.follow_pane(open);

        // The list keys go before the scroller, which would otherwise
        // scroll by the arrows / Home / End.
        let mut actions = Vec::new();
        if list_keys_free(ui.ctx(), cx.confirm.is_open()) && !self.hits.is_empty() {
            let keys = read_list_keys(ui.ctx(), false, false);
            actions.extend(self.key_actions(keys));
        }
        widgets::scroll_column(ui, "search-scroll", KeyScroll { space: true }, |ui| {
            self.draw(ui, cx, &mut actions)
        });
        for action in actions {
            match action {
                Action::Open {
                    meeting_id,
                    segment_id,
                    hit,
                } => {
                    if hit.is_some() {
                        self.cursor = hit;
                    }
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
                Action::ClearFilters => cx.memory.search_filters = SearchFilters::default(),
            }
        }
    }

    /// The hit indices in display order (grouped by meeting).
    fn order(&self) -> Vec<usize> {
        group_hits(&self.hits).concat()
    }

    /// Apply the list keys to the selection; Enter opens the selected hit.
    fn key_actions(&mut self, keys: ListKeys) -> Vec<Action> {
        let order = self.order();
        if let Some(motion) = keys.motion {
            self.cursor = step_selection(&order, self.cursor.as_ref(), motion);
            self.reveal = true;
        }
        match self.cursor.filter(|index| order.contains(index)) {
            Some(index) if keys.activate => vec![open_hit(&self.hits, index)],
            _ => Vec::new(),
        }
    }

    /// The meeting open beside the results: a split's `PaneRole::List`
    /// selection (debug builds: `WITNESS_DEBUG_PANE=<hit index>` fakes one).
    fn pane_selection(&self, pane: PaneRole) -> Option<i64> {
        if let Some(index) = debug_index("WITNESS_DEBUG_PANE") {
            return self.hits.get(index).map(|h| h.meeting_id);
        }
        match pane {
            PaneRole::List { selected } => selected,
            PaneRole::Full | PaneRole::Detail => None,
        }
    }

    /// When the meeting open beside the results changes, its group scrolls
    /// into view.
    fn follow_pane(&mut self, open: Option<i64>) {
        self.pane_open = open;
        if open != self.pane_seen {
            self.pane_seen = open;
            self.reveal_group = open;
        }
    }

    /// Debug builds only: `WITNESS_DEBUG_SELECT=<n>` selects the n-th hit
    /// (display order) once the results are in, for passive snapshots.
    fn debug_select(&mut self) {
        if self.debug_selected || self.hits.is_empty() || self.loading() {
            return;
        }
        if let Some(n) = debug_index("WITNESS_DEBUG_SELECT") {
            self.debug_selected = true;
            self.cursor = self.order().get(n).copied();
            self.reveal = true;
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
        self.load = Some(SearchLoad {
            task,
            append,
            started: Instant::now(),
        });
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
                    self.people_loaded = true;
                }
                Err(error) => cx
                    .toasts
                    .error("Could not load people for search filters", error),
            }
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx, actions: &mut Vec<Action>) {
        // h2.search-heading (aria-live polite). No count while the first
        // page is on its way ("0 matches" would be a lie).
        let loading = self.loading();
        let first_page = (loading && self.hits.is_empty()) || debug_flag("WITNESS_DEBUG_LOADING");
        let (main, suffix) = if first_page {
            (heading_main(&self.query), SEARCHING.to_owned())
        } else {
            heading(&self.query, self.hits.len(), self.has_more)
        };
        let full = format!("{main}{suffix}");
        let galley = heading_galley(ui, &self.query, &suffix, ui.available_width());
        let elided = galley.text() != full;
        let response = ui.add(egui::Label::new(galley).selectable(false));
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &full));
        widgets::announce(
            ui.ctx(),
            response.id.with("live"),
            &full,
            Politeness::Polite,
        );
        if elided {
            response.on_hover_text(&full);
        }
        widgets::gap(ui, theme::S3);

        self.draw_filters(ui, cx);
        widgets::gap(ui, theme::S4);

        if let Some(error) = &self.error {
            let message = format!("Search failed: {error}");
            if widgets::view_error(ui, &message, &["Try again"]).is_some() {
                actions.push(Action::TryAgain);
            }
            return;
        }
        if first_page {
            let due = self
                .load
                .as_ref()
                .is_none_or(|load| skeleton_due(ui, load.started));
            if due {
                results_skeleton(ui);
            }
            return;
        }
        if self.hits.is_empty() {
            let filtered = active_filters(&cx.memory.search_filters) > 0;
            let (title, body, button) = no_results(&self.query, filtered);
            let buttons: Vec<&str> = button.into_iter().collect();
            let glyph = if filtered {
                ph::FUNNEL_X
            } else {
                ph::MAGNIFYING_GLASS
            };
            if empty_hero(ui, glyph, &title, body, &buttons, false).is_some() {
                actions.push(Action::ClearFilters);
            }
            return;
        }

        let today = Local::now().date_naive();
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut group_found = false;
        for (n, group) in group_hits(&self.hits).into_iter().enumerate() {
            if n > 0 {
                ui.add_space(theme::S4);
            }
            let first = &self.hits[group[0]];
            let open = self.pane_open == Some(first.meeting_id);
            let (header, clicked) = group_title(ui, first, today, open);
            if clicked {
                actions.push(Action::Open {
                    meeting_id: first.meeting_id,
                    segment_id: None,
                    hit: None,
                });
            }
            if self.reveal_group == Some(first.meeting_id) {
                group_found = true;
                self.reveal_group = None;
                ui.scroll_to_rect(header, None);
            }
            for &index in &group {
                let hit = &self.hits[index];
                ui.add_space(2.0);
                let selected = self.cursor == Some(index);
                let response = hit_row(ui, index, hit, selected);
                if response.clicked() {
                    actions.push(open_hit(&self.hits, index));
                }
                // Keyboard focus (Tab, egui's arrow moves) carries the
                // selection with it.
                if response.has_focus() {
                    self.cursor = Some(index);
                }
                if selected && std::mem::take(&mut self.reveal) {
                    response.scroll_to_me(None);
                }
            }
        }
        // An open meeting without results here: nothing to scroll to.
        if !group_found && !loading {
            self.reveal_group = None;
        }

        if self.has_more {
            ui.add_space(theme::S3);
            let label = if loading { "Loading…" } else { "Load more" };
            if load_more_button(ui, label, !loading).clicked() {
                actions.push(Action::LoadMore);
            }
        }
    }

    /// The filters: speaker, from, to and Clear filters in one row that
    /// wraps; in a compact column, a "Filters" disclosure that shows them
    /// stacked at full width.
    fn draw_filters(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let people = &self.people;
        // A chosen person not in the list: the list is still loading (Back
        // from a transcript) or failed to load. A loaded list without them
        // has already reset the filter to Anyone.
        let unknown_person = if self.people_load.is_some() {
            "Person…"
        } else {
            "Unknown person"
        };
        let filters: &mut SearchFilters = &mut cx.memory.search_filters;
        let who = WhoOptions {
            people,
            unknown_person,
        };
        let height = ui.spacing().interact_size.y;

        if widgets::is_compact(ui) {
            let toggle = filters_toggle(ui, self.filters_open, active_filters(filters));
            follow_focus(&toggle);
            if toggle.clicked() {
                self.filters_open = !self.filters_open;
            }
            if !self.filters_open {
                return;
            }
            widgets::gap(ui, theme::S2);
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = theme::S2;
                stacked(ui, "Speaker", height, |ui| {
                    let width = ui.available_width();
                    who.select(ui, &mut filters.who, width);
                });
                stacked(ui, "From", height, |ui| {
                    date_field(ui, "from", "search-date-from", &mut filters.date_from, None);
                });
                stacked(ui, "To", height, |ui| {
                    date_field(ui, "to", "search-date-to", &mut filters.date_to, None);
                });
                if active_filters(filters) > 0 {
                    ui.horizontal(|ui| {
                        ui.add_space(STACK_LABEL);
                        if widgets::btn_ghost(ui, "Clear filters").clicked() {
                            *filters = SearchFilters::default();
                        }
                    });
                }
            });
            return;
        }

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(theme::S3, theme::S2);
            // Each group is allocated at its exact width, so the row wraps
            // between groups instead of letting one run past the edge.
            ui.allocate_ui(egui::vec2(WHO_WIDTH, height), |ui| {
                who.select(ui, &mut filters.who, WHO_WIDTH);
            });
            for (label, name, id, text) in [
                ("From", "from", "search-date-from", &mut filters.date_from),
                ("To", "to", "search-date-to", &mut filters.date_to),
            ] {
                let label_width = ui
                    .painter()
                    .layout_no_wrap(label.into(), theme::regular(13.0), pal().text_muted)
                    .size()
                    .x;
                let width = label_width + theme::S2 + DATE_WIDTH;
                ui.allocate_ui(egui::vec2(width, height), |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = theme::S2;
                        ui.label(
                            egui::RichText::new(label)
                                .font(theme::regular(13.0))
                                .color(pal().text_muted),
                        );
                        date_field(ui, name, id, text, Some(DATE_WIDTH));
                    });
                });
            }
            let any = active_filters(filters) > 0;
            if any && widgets::btn_ghost(ui, "Clear filters").clicked() {
                *filters = SearchFilters::default();
            }
        });
    }
}

/// `WITNESS_DEBUG_FILTERS=1` (debug builds) mounts Search with the compact
/// filters open, so off-screen snapshots can show them without input.
fn debug_filters_open() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var_os("WITNESS_DEBUG_FILTERS").is_some_and(|v| v == "1")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

/// How many filters are set (the compact toggle's count).
fn active_filters(filters: &SearchFilters) -> usize {
    usize::from(filters.who != Who::Anyone)
        + usize::from(!filters.date_from.is_empty())
        + usize::from(!filters.date_to.is_empty())
}

/// The compact "Filters" disclosure button: funnel, label, the number of
/// active filters, caret.
fn filters_toggle(ui: &mut egui::Ui, open: bool, active: usize) -> egui::Response {
    let caret = if open { ph::CARET_UP } else { ph::CARET_DOWN };
    let count = if active > 0 {
        format!(" · {active}")
    } else {
        String::new()
    };
    let text = egui::RichText::new(format!("{}  Filters{count}  {caret}", ph::FUNNEL))
        .size(13.0)
        .color(if active > 0 {
            pal().accent
        } else {
            pal().text_muted
        });
    let response = widgets::btn_ghost(ui, text);
    let name = if active > 0 {
        format!("Filters ({active} active)")
    } else {
        "Filters".to_owned()
    };
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::Button, true, &name);
        info.selected = Some(open); // aria-expanded
        info
    });
    response
}

/// A stacked filter row: a fixed label column, the control filling the rest.
fn stacked(ui: &mut egui::Ui, label: &str, height: f32, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.allocate_ui_with_layout(
            egui::vec2(STACK_LABEL, height),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_size(egui::vec2(STACK_LABEL, height));
                ui.label(
                    egui::RichText::new(label)
                        .font(theme::regular(13.0))
                        .color(pal().text_muted),
                );
            },
        );
        control(ui);
    });
}

/// The speaker ("Who said it") select's options.
struct WhoOptions<'a> {
    people: &'a [PersonInfo],
    unknown_person: &'a str,
}

impl WhoOptions<'_> {
    fn label(&self, who: Who) -> String {
        match who {
            Who::Anyone => "Anyone".into(),
            Who::Me => "Me".into(),
            Who::Them => "Them".into(),
            Who::Person(id) => self
                .people
                .iter()
                .find(|p| p.id == id)
                .map_or_else(|| self.unknown_person.into(), |p| p.name.clone()),
        }
    }

    /// The select, `width` wide (the selected name elides inside it).
    fn select(&self, ui: &mut egui::Ui, who: &mut Who, width: f32) {
        let combo = widgets::select(ui, widgets::SelectKind::TextInput, |ui| {
            egui::ComboBox::from_id_salt("search-who")
                .selected_text(egui::RichText::new(self.label(*who)).size(13.0))
                .width(width)
                .truncate()
                .show_ui(ui, |ui| {
                    let mut option = |ui: &mut egui::Ui, value: Who| {
                        ui.selectable_value(who, value, self.label(value));
                    };
                    option(ui, Who::Anyone);
                    option(ui, Who::Me);
                    option(ui, Who::Them);
                    for person in self.people {
                        option(ui, Who::Person(person.id));
                    }
                })
        });
        let combo = combo.response.on_hover_text("Who said it");
        ui.ctx().accesskit_node_builder(combo.id, |node| {
            node.set_label("Filter by speaker");
        });
    }
}

/// A `YYYY-MM-DD` field (replacing `<input type=date>`), `width` wide or
/// filling the row. Invalid input gets a red border and is not applied.
/// `name` ("from" / "to") is the accessible name's stem.
fn date_field(ui: &mut egui::Ui, name: &str, id: &str, text: &mut String, width: Option<f32>) {
    let id = egui::Id::new(id);
    let invalid = parse_date(text) == DateInput::Invalid;
    let edit = egui::TextEdit::singleline(text)
        .hint_text("YYYY-MM-DD")
        .font(theme::regular(13.0))
        .char_limit(10)
        .desired_width(width.unwrap_or(f32::INFINITY));
    let edit = widgets::framed_text_edit(
        ui,
        id,
        edit,
        pal().bg_raised,
        theme::R_MD,
        widgets::TEXT_INPUT_MARGIN,
    );
    let response = ui.add(edit);
    if invalid {
        // Red over the kit's border (accent while focused).
        ui.painter().rect_stroke(
            response.rect,
            CornerRadius::same(theme::R_MD),
            Stroke::new(1.0, pal().rec),
            StrokeKind::Inside,
        );
    }
    widgets::text_edit_menu(&response);
    let label = if invalid {
        format!("{name} (YYYY-MM-DD, invalid date)")
    } else {
        format!("{name} (YYYY-MM-DD)")
    };
    ui.ctx().accesskit_node_builder(id, |node| {
        node.set_label(label.as_str());
    });
    if invalid {
        response.on_hover_text("Enter a date as YYYY-MM-DD");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::views::meetings::Motion;

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
    fn only_valid_dates_apply_a_bound() {
        assert_eq!(applied_date("2026-1"), None);
        assert_eq!(applied_date("nope"), None);
        assert_eq!(applied_date(""), None);
        assert_eq!(applied_date("2026-11-1"), Some("2026-11-01".into()));
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
    fn long_queries_shorten_inside_the_heading() {
        let ctx = theme::test_context();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let short = heading_galley(ui, "server", &heading("server", 11, false).1, 300.0);
            assert_eq!(
                short.text(),
                "Results for \u{201c}server\u{201d} — 11 matches"
            );
            assert!(!short.elided);

            // Long (and unbroken) queries: two rows at most, the count kept.
            for query in ["budget ".repeat(30), "x".repeat(200)] {
                let galley = heading_galley(ui, &query, &heading(&query, 3, true).1, 300.0);
                let text = galley.text();
                assert!(galley.rows.len() <= 2, "{text}");
                assert!(!galley.elided, "{text}");
                assert!(text.ends_with("\u{2026}\u{201d} — 3+ matches"), "{text}");
                assert!(galley.size().x <= 300.5, "{}", galley.size().x);
            }
        });
        output.textures_delta.clear();
    }

    #[test]
    fn keys_walk_the_hits_in_display_order() {
        let mut view = SearchView::new("q".into());
        // Meeting 2's hits group together: display order 0, 2, 1, 3.
        view.hits = vec![
            hit(2, 10, "a"),
            hit(1, 11, "b"),
            hit(2, 12, "c"),
            hit(1, -1, "n"),
        ];
        let step = |motion| ListKeys {
            motion: Some(motion),
            ..ListKeys::default()
        };
        let mut walk = Vec::new();
        for _ in 0..5 {
            assert!(view.key_actions(step(Motion::By(1))).is_empty());
            walk.push(view.cursor.unwrap());
        }
        assert_eq!(walk, [0, 2, 1, 3, 3], "grouped order, stopping at the end");
        assert!(view.reveal);
        let enter = ListKeys {
            activate: true,
            ..ListKeys::default()
        };
        // A notes hit opens its meeting without a seek.
        assert_eq!(
            view.key_actions(enter),
            vec![Action::Open {
                meeting_id: 1,
                segment_id: None,
                hit: Some(3),
            }]
        );
        view.key_actions(step(Motion::First));
        assert_eq!(
            view.key_actions(enter),
            vec![Action::Open {
                meeting_id: 2,
                segment_id: Some(10),
                hit: Some(0),
            }]
        );
        // New results drop a selection that no longer points anywhere.
        view.hits.truncate(0);
        assert!(view.key_actions(enter).is_empty());
    }

    #[test]
    fn the_open_meeting_scrolls_its_group_into_view_once() {
        let mut view = SearchView::new("q".into());
        view.follow_pane(Some(2));
        assert_eq!((view.pane_open, view.reveal_group), (Some(2), Some(2)));
        view.reveal_group = None;
        view.follow_pane(Some(2));
        assert_eq!(view.reveal_group, None);
        assert_eq!(
            view.pane_selection(PaneRole::List { selected: Some(5) }),
            Some(5)
        );
        assert_eq!(view.pane_selection(PaneRole::Full), None);
    }

    #[test]
    fn no_results_suggest_a_way_out() {
        let (title, body, button) = no_results("budget", false);
        assert_eq!(title, "No matches for \u{201c}budget\u{201d}");
        assert!(body.contains("every transcript"), "{body}");
        assert_eq!(button, None);
        let (_, body, button) = no_results("budget", true);
        assert!(body.contains("filters"), "{body}");
        assert_eq!(button, Some("Clear filters"));
        // Long queries are cut in the title.
        let (title, ..) = no_results(&"x".repeat(100), false);
        assert_eq!(
            title.chars().count(),
            "No matches for “”…".chars().count() + 32
        );
        assert!(title.ends_with("x…\u{201d}"), "{title}");
        // While the first page loads, the heading has no count.
        assert_eq!(heading_main("q"), heading("q", 0, false).0);
        assert!(!SEARCHING.contains('0'));
    }

    #[test]
    fn the_results_skeleton_fits_narrow_columns() {
        let ctx = theme::test_context();
        for width in [380.0, 1000.0] {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 760.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, results_skeleton);
            output.textures_delta.clear();
            let bars: Vec<Rect> = output
                .shapes
                .iter()
                .map(|c| c.shape.visual_bounding_rect())
                .filter(|r| r.is_positive())
                .collect();
            assert!(bars.len() >= 15, "{} bars at {width}", bars.len());
            assert!(bars.iter().all(|r| r.right() <= width + 0.5), "at {width}");
        }
    }

    #[test]
    fn group_dates_are_short() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
        let date = group_date("2026-07-22T12:10:00+01:00", today);
        assert!(date.starts_with("Wed 22 Jul, "), "{date}");
        let next_year = NaiveDate::from_ymd_opt(2027, 2, 1).unwrap();
        let date = group_date("2026-07-22T12:10:00+01:00", next_year);
        assert!(date.starts_with("Wed 22 Jul 2026, "), "{date}");
        assert_eq!(group_date("nope", today), "nope");
    }

    #[test]
    fn hit_text_marks_the_matches() {
        let (job, marks) = hit_job(&hit(1, 5, "the \x01budget\x02 plan"), 300.0);
        assert_eq!(job.text, "the budget plan");
        assert_eq!(marks.len(), 1);
        let marked: String = job
            .text
            .chars()
            .skip(marks[0].start)
            .take(marks[0].len())
            .collect();
        assert_eq!(marked, "budget");
        // The timestamp and speaker moved to the meta line; the accessible
        // name keeps the 0.2.x wording.
        let budget = hit(1, 5, "the \x01budget\x02 plan");
        assert_eq!(hit_label(&budget), "[1:05] Alice: the budget plan");
        assert_eq!(hit_meta_job(&budget, 300.0).text, "1:05Alice");

        let note = hit(1, -1, "\x01notes\x02");
        let (job, marks) = hit_job(&note, 300.0);
        assert_eq!(job.text, "notes");
        assert_eq!(marks, vec![0..5]);
        assert_eq!(hit_label(&note), "note Notes: notes");
        assert_eq!(hit_meta_job(&note, 300.0).text, "Meeting notes");
    }

    /// The expected highlight of `range` per row, from epaint's own cursor
    /// positions (independent of `mark_rects`' glyph walk): (left, right,
    /// row top, row bottom).
    fn cursor_spans(galley: &egui::Galley, range: Range<usize>) -> Vec<(f32, f32, f32, f32)> {
        use egui::text::CCursor;
        let mut spans: Vec<(f32, f32, f32, f32)> = Vec::new();
        for i in range {
            let mut start = CCursor::new(i);
            start.prefer_next_row = true;
            let start = galley.pos_from_cursor(start);
            // The right edge of char i, on its own row even at a wrap.
            let end = galley.pos_from_cursor(CCursor::new(i + 1));
            match spans.last_mut() {
                Some(span) if span.2 == start.top() => span.1 = end.left(),
                _ => spans.push((start.left(), end.left(), start.top(), start.bottom())),
            }
        }
        spans
    }

    /// Within a pixel: cursor x positions are pixel-snapped, glyph extents
    /// are not; a glyph is ~7 px wide, so an off-by-one glyph still fails.
    fn assert_covers(rects: &[Rect], spans: &[(f32, f32, f32, f32)]) {
        assert_eq!(rects.len(), spans.len(), "{rects:?} vs {spans:?}");
        for (rect, &(left, right, top, bottom)) in rects.iter().zip(spans) {
            assert!((rect.left() - left).abs() <= 1.0, "{rect:?} left {left}");
            assert!(
                (rect.right() - right).abs() <= 1.0,
                "{rect:?} right {right}"
            );
            assert!(rect.top() >= top - 0.5 && rect.bottom() <= bottom + 0.5);
        }
    }

    #[test]
    fn mark_rects_cover_exactly_the_marked_glyphs() {
        let ctx = theme::test_context();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            // A one-row mark after several wrapped rows (the leading
            // timestamp and speaker shift the char indices).
            let snippet = "word ".repeat(30) + "\x01match\x02 tail";
            let (job, marks) = hit_job(&hit(1, 5, &snippet), 120.0);
            let start = job.text.chars().count() - "match tail".chars().count();
            assert_eq!(marks, vec![start..start + 5]);
            let galley = ui.painter().layout_job(job);
            assert!(galley.rows.len() > 2, "the snippet wraps");
            let rects = mark_rects(&galley, &marks);
            assert_covers(&rects, &cursor_spans(&galley, marks[0].clone()));
            assert!(rects[0].top() > galley.rows[1].pos.y, "on a later row");

            // A marked run that itself wraps: one rect per row it spans.
            let snippet = "lead \x01".to_string() + &"match ".repeat(12) + "end\x02 tail";
            let (job, marks) = hit_job(&hit(1, 5, &snippet), 120.0);
            let galley = ui.painter().layout_job(job);
            let rects = mark_rects(&galley, &marks);
            let spans = cursor_spans(&galley, marks[0].clone());
            assert!(spans.len() >= 2, "the mark wraps: {spans:?}");
            assert_covers(&rects, &spans);

            // Two marks on one row stay separate; a note hit has no
            // timestamp but a chip's leading space.
            let (mut job, marks) = hit_job(&hit(1, -1, "\x01a\x02 b \x01c\x02"), 300.0);
            job.sections[0].leading_space = 30.0;
            let galley = ui.painter().layout_job(job);
            let rects = mark_rects(&galley, &marks);
            let spans: Vec<_> = marks
                .iter()
                .flat_map(|m| cursor_spans(&galley, m.clone()))
                .collect();
            assert_covers(&rects, &spans);

            assert!(!hit_row(ui, 0, &hit(1, -1, "\x01n\x02"), false).clicked());
            assert!(!hit_row(ui, 1, &hit(1, 5, &"long ".repeat(80)), true).clicked());
            let today = NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
            assert!(!group_title(ui, &hit(1, 5, "x"), today, true).1);
        });
        output.textures_delta.clear();
    }
}
