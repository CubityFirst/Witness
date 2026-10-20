//! People: per-person stats, the 12-week trend and voice prints
//! (map-ui-meetings §3, people.tsx).
//!
//! Three loads run in parallel on mount and on every `refresh_tick`;
//! any failure fails the whole load. The trend reads only the meetings of
//! the last 12 weeks (`meetings_started_since`) instead of paging through
//! every meeting like the TSX did.
//!
//! ↑/↓ (j/k, Home/End) select a person and Enter / Space opens or closes
//! their detail, as in the Meetings list. The first load shows a skeleton
//! of the trend and the rows; no people a designed empty state.
//!
//! The shell draws a name filter above the view, where Meetings has its
//! search box ([`search_box`]; app-owned text, so it survives leaving
//! People). It filters the loaded people as you type: the list and the
//! keyboard selection skip the rest; the trend stays whole.

use crate::bridge::{OpDone, Task};
use crate::ui::format::{fmt_date, fmt_date_short, fmt_duration, fmt_thousands};
use crate::ui::icons;
use crate::ui::icons::ph;
use crate::ui::theme::{self, pal, S1, S2, S3, S4};
use crate::ui::toasts::Notice;
use crate::ui::views::meetings::{
    debug_flag, debug_index, empty_hero, follow_focus, list_keys_free, open_transcript,
    read_list_keys, skeleton_at, skeleton_due, step_selection, ListKeys,
};
use crate::ui::views::search::search_field;
use crate::ui::widgets::{self, KeyScroll};
use crate::ui::{UiCtx, View};
use chrono::{DateTime, Datelike, Days, Local, NaiveDate};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Rect, Response, Sense, Stroke, WidgetInfo,
    WidgetType,
};
use std::sync::Arc;
use witness_core::db::{Meeting, PersonMeetingStat};
use witness_core::service::PersonInfo;
use witness_core::CoreResult;

/// Buckets in the trend: the current week and the 11 before it.
const WEEKS: usize = 12;
/// The bar area's height inside the trend surface (axis labels below).
const CHART_HEIGHT: f32 = 56.0;
/// The widest a bar gets: past this, wider windows add air, not fat bars.
const BAR_MAX_WIDTH: f32 = 28.0;
/// Person avatars in the list.
const AVATAR: f32 = 32.0;
/// The expand chevron's box.
const CHEVRON: f32 = 16.0;
/// The one confirmation this view can have open.
const CONFIRM_ID: &str = "people-confirm";
const HINT: &str = "\"Speaking\" totals come from transcribed segments; word counts are approximate. Unnamed speakers (Speaker 1…) aren't tracked across meetings — rename them to a person to include them here. Enrolled voices can also be managed in Settings.";

// ----------------------------------------------------------------------
// Weekly trend (pure)

/// One trend bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Week {
    /// The Monday the week starts on (local).
    from: NaiveDate,
    count: usize,
    ms: i64,
}

/// The Monday of the week containing `day`.
fn week_start(day: NaiveDate) -> NaiveDate {
    day - Days::new(u64::from(day.weekday().num_days_from_monday()))
}

/// The oldest bucket's Monday: 11 weeks before this week's.
fn trend_start(today: NaiveDate) -> NaiveDate {
    week_start(today) - Days::new(7 * (WEEKS as u64 - 1))
}

/// The local calendar day a meeting started on.
fn local_day(started_at: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(started_at)
        .ok()
        .map(|at| at.with_timezone(&Local).date_naive())
}

/// Count meetings and sum their durations per week, oldest week first.
/// Meetings still recording (no duration) count but add 0 ms.
fn bucket_weeks(today: NaiveDate, meetings: &[Meeting]) -> Vec<Week> {
    let start = trend_start(today);
    let mut weeks: Vec<Week> = (0..WEEKS as u64)
        .map(|i| Week {
            from: start + Days::new(7 * i),
            count: 0,
            ms: 0,
        })
        .collect();
    for meeting in meetings {
        let Some(day) = local_day(&meeting.started_at) else {
            continue;
        };
        let from = week_start(day);
        if let Some(week) = weeks.iter_mut().find(|w| w.from == from) {
            week.count += 1;
            week.ms += meeting.duration_ms.unwrap_or(0);
        }
    }
    weeks
}

/// A bar's height as a fraction of the chart: total meeting time over the
/// busiest week's, at least 6 % so a short week stays visible; 0 for an
/// empty week (drawn as a stub on the baseline).
fn bar_fraction(ms: i64, max_ms: i64) -> f32 {
    if ms > 0 {
        (ms as f32 / max_ms.max(1) as f32).clamp(0.06, 1.0)
    } else {
        0.0
    }
}

/// `Week of {date}: {n} meeting(s), {duration}`.
fn week_text(week: &Week) -> String {
    let s = if week.count == 1 { "" } else { "s" };
    format!(
        "Week of {}: {} meeting{s}, {}",
        fmt_date_short(week.from),
        week.count,
        fmt_duration(Some(week.ms))
    )
}

/// `Last 12 weeks — {n} meetings, {duration} total` (always "meetings").
fn trend_title(weeks: &[Week]) -> String {
    let count: usize = weeks.iter().map(|w| w.count).sum();
    let ms: i64 = weeks.iter().map(|w| w.ms).sum();
    format!(
        "Last {WEEKS} weeks — {count} meetings, {} total",
        fmt_duration(Some(ms))
    )
}

// ----------------------------------------------------------------------
// Per-person aggregation (pure)

/// One person card's data.
#[derive(Debug, Clone)]
struct PersonAgg {
    /// `"me"` or `"p{id}"`: stable across reloads (the open card survives).
    key: String,
    person_id: Option<i64>,
    name: String,
    is_me: bool,
    /// Newest first (the backend's order).
    meetings: Vec<PersonMeetingStat>,
    meeting_count: usize,
    /// Total duration of the meetings shared with them.
    together_ms: i64,
    talk_ms: i64,
    words: i64,
}

/// Group the stats rows per person; Me first, then by time together.
fn aggregate(stats: &[PersonMeetingStat]) -> Vec<PersonAgg> {
    let mut people: Vec<PersonAgg> = Vec::new();
    for row in stats {
        let key = row
            .person_id
            .map_or_else(|| "me".to_string(), |id| format!("p{id}"));
        let index = match people.iter().position(|p| p.key == key) {
            Some(index) => index,
            None => {
                people.push(PersonAgg {
                    key,
                    person_id: row.person_id,
                    name: row.person_name.clone(),
                    is_me: row.person_id.is_none(),
                    meetings: Vec::new(),
                    meeting_count: 0,
                    together_ms: 0,
                    talk_ms: 0,
                    words: 0,
                });
                people.len() - 1
            }
        };
        let agg = &mut people[index];
        agg.meetings.push(row.clone());
        agg.meeting_count += 1;
        agg.together_ms += row.duration_ms;
        agg.talk_ms += row.talk_ms;
        agg.words += row.words;
    }
    // Stable, like Array.prototype.sort.
    people.sort_by(|a, b| {
        b.is_me
            .cmp(&a.is_me)
            .then_with(|| b.together_ms.cmp(&a.together_ms))
    });
    people
}

/// Whether `p` passes the name filter: the filter (trimmed) appears in
/// their name, ignoring case. A blank filter keeps everyone.
fn name_matches(p: &PersonAgg, filter: &str) -> bool {
    let filter = filter.trim();
    filter.is_empty() || p.name.to_lowercase().contains(&filter.to_lowercase())
}

/// `Math.round(part / whole * 100)`.
fn percent(part: i64, whole: i64) -> i64 {
    (part as f64 / whole.max(1) as f64 * 100.0).round() as i64
}

/// The `.person-stats` line. `sample_seconds` is the enrolled print's
/// speech, when the person has one.
fn stats_text(p: &PersonAgg, sample_seconds: Option<f64>) -> String {
    let s = if p.meeting_count == 1 { "" } else { "s" };
    let mut text = format!("{} meeting{s}", p.meeting_count);
    if !p.is_me {
        text += &format!(" · {} together", fmt_duration(Some(p.together_ms)));
    }
    text += &format!(
        " · {} speaking · {} words",
        fmt_duration(Some(p.talk_ms)),
        fmt_thousands(p.words)
    );
    if p.together_ms > 0 {
        text += &format!(" · {}% of meeting time", percent(p.talk_ms, p.together_ms));
    }
    if let Some(seconds) = sample_seconds {
        text += &format!(
            " · {} min of speech learned",
            (seconds / 60.0).round() as i64
        );
    }
    text
}

/// A person's meeting row meta line.
fn meeting_meta(m: &PersonMeetingStat) -> String {
    let mut text = format!(
        "{} · {} meeting · {} speaking · {} words",
        fmt_date(&m.started_at),
        fmt_duration(Some(m.duration_ms)),
        fmt_duration(Some(m.talk_ms)),
        fmt_thousands(m.words)
    );
    if m.duration_ms > 0 {
        text += &format!(" · {}%", percent(m.talk_ms, m.duration_ms));
    }
    text
}

/// The row's one-line summary: `{n} meeting(s) · {duration} speaking`.
fn summary_text(p: &PersonAgg) -> String {
    let s = if p.meeting_count == 1 { "" } else { "s" };
    format!(
        "{} meeting{s} · {} speaking",
        p.meeting_count,
        fmt_duration(Some(p.talk_ms))
    )
}

/// The expanded detail's key/value pairs: the same facts as
/// [`stats_text`], under the same conditions.
fn stat_pairs(p: &PersonAgg, sample_seconds: Option<f64>) -> Vec<(&'static str, String)> {
    let mut pairs = vec![("Meetings", p.meeting_count.to_string())];
    if !p.is_me {
        pairs.push(("Time together", fmt_duration(Some(p.together_ms))));
    }
    pairs.push(("Speaking time", fmt_duration(Some(p.talk_ms))));
    pairs.push(("Words", fmt_thousands(p.words)));
    if p.together_ms > 0 {
        pairs.push((
            "Share of meeting time",
            format!("{}%", percent(p.talk_ms, p.together_ms)),
        ));
    }
    if let Some(seconds) = sample_seconds {
        pairs.push((
            "Speech learned",
            format!("{} min", (seconds / 60.0).round() as i64),
        ));
    }
    pairs
}

/// A person's avatar colour: Me's speaker colour, everyone else one of the
/// eight speaker colours by person id — stable across reloads and renames,
/// and distinct for up to eight consecutive enrolments.
fn person_color(p: &PersonAgg) -> Color32 {
    match p.person_id {
        None => theme::chip_color("me"),
        Some(id) => theme::chip_color(&format!("S{}", id.rem_euclid(8) + 1)),
    }
}

// ----------------------------------------------------------------------
// The view

/// The three parallel loads of one (re)load. Replacing it discards them.
struct PeopleLoad {
    stats: Task<CoreResult<Vec<PersonMeetingStat>>>,
    meetings: Task<CoreResult<Vec<Meeting>>>,
    prints: Task<CoreResult<Vec<PersonInfo>>>,
    got_stats: Option<CoreResult<Vec<PersonMeetingStat>>>,
    got_meetings: Option<CoreResult<Vec<Meeting>>>,
    got_prints: Option<CoreResult<Vec<PersonInfo>>>,
    /// The day the buckets are computed for.
    today: NaiveDate,
    /// When it was requested (the first load's skeleton waits a moment).
    started: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Toggle(String),
    Open(i64),
    Forget {
        id: i64,
        name: String,
        skip: bool,
    },
    TryAgain,
    /// The empty state's way to the transcripts (where speakers are named).
    GoMeetings,
    /// "Clear search" when the name filter matches no one.
    ClearSearch,
}

/// The name filter above People, where Meetings has its search box (the
/// shell draws it; Ctrl+F and `/` focus it).
pub fn search_box(ui: &mut egui::Ui, cx: &mut UiCtx) {
    search_field(
        ui,
        egui::Id::new("people-search"),
        &mut cx.memory.people_search,
        "Search people…",
        "Search people by name",
    );
}

pub struct PeopleView {
    people: Vec<PersonAgg>,
    weeks: Vec<Week>,
    prints: Vec<PersonInfo>,
    /// The key of the one open card (accordion); survives reloads.
    expanded: Option<String>,
    load: Option<PeopleLoad>,
    load_error: Option<String>,
    /// Who "Forget voice…" is asking about.
    confirm_for: Option<(i64, String)>,
    seen_tick: Option<u64>,
    /// The keyboard selection: a person's key (↑/↓, j/k, Home/End; Enter
    /// or Space opens / closes their detail).
    cursor: Option<String>,
    /// Scroll the selected person into view on the next draw.
    reveal: bool,
    /// `WITNESS_DEBUG_SELECT` has been applied.
    debug_selected: bool,
    /// `WITNESS_DEBUG_PEOPLE_SEARCH` has been applied.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    debug_searched: bool,
}

impl Default for PeopleView {
    fn default() -> Self {
        Self::new()
    }
}

impl PeopleView {
    pub fn new() -> Self {
        PeopleView {
            people: Vec::new(),
            weeks: Vec::new(),
            prints: Vec::new(),
            expanded: None,
            load: None,
            load_error: None,
            confirm_for: None,
            seen_tick: None,
            cursor: None,
            reveal: false,
            debug_selected: false,
            debug_searched: false,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if self.seen_tick != Some(cx.shell.refresh_tick) {
            self.seen_tick = Some(cx.shell.refresh_tick);
            self.start(cx);
        }
        self.poll(ui, cx);
        self.take_confirm_answer(cx);

        // The list keys go before the scroller, which would otherwise
        // scroll by the arrows / Home / End (and page by Space).
        let filter = cx.memory.people_search.text.clone();
        let mut actions = Vec::new();
        if list_keys_free(ui.ctx(), cx.confirm.is_open()) && !self.people.is_empty() {
            // Space toggles only once there is a selection to toggle;
            // before that it still pages down.
            let keys = read_list_keys(ui.ctx(), self.cursor.is_some(), false);
            actions.extend(self.key_actions(keys, &filter));
        }
        widgets::scroll_column(ui, "people-scroll", KeyScroll { space: true }, |ui| {
            self.draw(ui, &filter, &mut actions)
        });
        for action in actions {
            match action {
                Action::Toggle(key) => {
                    self.cursor = Some(key.clone());
                    self.expanded = if self.expanded.as_ref() == Some(&key) {
                        None
                    } else {
                        Some(key)
                    };
                }
                Action::GoMeetings => cx.nav.go(View::Meetings),
                Action::ClearSearch => {
                    cx.memory.people_search.clear();
                    cx.memory.people_search.request_focus = true;
                }
                Action::Open(meeting_id) => open_transcript(cx, meeting_id, View::People),
                Action::Forget { id, name, skip } => {
                    if skip {
                        forget(cx, id, name);
                    } else {
                        cx.confirm.ask(
                            egui::Id::new(CONFIRM_ID),
                            format!(
                                "Forget {name}'s voice?\nExisting transcripts keep their names."
                            ),
                            "Forget voice",
                        );
                        self.confirm_for = Some((id, name));
                    }
                }
                Action::TryAgain => self.start(cx),
            }
        }
    }

    /// Nothing to persist.
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
    }

    /// Start the three loads (stats, the trend's meetings, voice prints).
    fn start(&mut self, cx: &UiCtx) {
        let today = Local::now().date_naive();
        // One extra day so a meeting stored with another UTC offset (DST)
        // near the boundary still reaches the bucketing, which filters.
        let since = (trend_start(today) - Days::new(1))
            .format("%Y-%m-%d")
            .to_string();
        self.load = Some(PeopleLoad {
            stats: cx.tasks.spawn("people-stats", |c| c.people_stats()),
            meetings: cx
                .tasks
                .spawn("people-trend", move |c| c.meetings_started_since(&since)),
            prints: cx.tasks.spawn("list-people", |c| c.list_people()),
            got_stats: None,
            got_meetings: None,
            got_prints: None,
            today,
            started: std::time::Instant::now(),
        });
    }

    /// Apply the list keys to the selection (the people `filter` shows);
    /// Enter (or Space, once there is a selection) opens or closes the
    /// selected person's detail.
    fn key_actions(&mut self, keys: ListKeys, filter: &str) -> Vec<Action> {
        let keys_in_order: Vec<String> = self
            .people
            .iter()
            .filter(|p| name_matches(p, filter))
            .map(|p| p.key.clone())
            .collect();
        if let Some(motion) = keys.motion {
            self.cursor = step_selection(&keys_in_order, self.cursor.as_ref(), motion);
            self.reveal = true;
        }
        match self
            .cursor
            .as_ref()
            .filter(|key| keys_in_order.contains(key))
        {
            Some(key) if keys.activate => vec![Action::Toggle(key.clone())],
            _ => Vec::new(),
        }
    }

    fn poll(&mut self, ui: &egui::Ui, cx: &mut UiCtx) {
        let Some(load) = self.load.as_mut() else {
            return;
        };
        if load.got_stats.is_none() {
            load.got_stats = load.stats.poll_flat();
        }
        if load.got_meetings.is_none() {
            load.got_meetings = load.meetings.poll_flat();
        }
        if load.got_prints.is_none() {
            load.got_prints = load.prints.poll_flat();
        }
        let done =
            load.got_stats.is_some() && load.got_meetings.is_some() && load.got_prints.is_some();
        if !done {
            return;
        }
        let load = self.load.take().expect("load in flight");
        let today = load.today;
        let results = (
            load.got_stats.expect("stats"),
            load.got_meetings.expect("meetings"),
            load.got_prints.expect("prints"),
        );
        match results {
            (Ok(stats), Ok(meetings), Ok(prints)) => {
                cx.fonts.note_texts(
                    ui.ctx(),
                    stats
                        .iter()
                        .flat_map(|s| [s.person_name.as_str(), s.meeting_title.as_str()]),
                );
                self.people = aggregate(&stats);
                self.weeks = bucket_weeks(today, &meetings);
                self.prints = prints;
                self.load_error = None;
                #[cfg(debug_assertions)]
                self.debug_expand();
                // Debug builds only: `WITNESS_DEBUG_PEOPLE_SEARCH=<text>`
                // types into the name filter, once.
                #[cfg(debug_assertions)]
                if let Ok(text) = std::env::var("WITNESS_DEBUG_PEOPLE_SEARCH") {
                    if !std::mem::replace(&mut self.debug_searched, true) {
                        cx.memory.people_search.text = text;
                    }
                }
                self.expand_requested(cx);
                // Debug builds only: `WITNESS_DEBUG_EMPTY=1` shows the empty
                // state, `WITNESS_DEBUG_SELECT=<n>` selects the n-th person.
                if debug_flag("WITNESS_DEBUG_EMPTY") {
                    self.people.clear();
                }
                if let Some(n) = debug_index("WITNESS_DEBUG_SELECT") {
                    if !std::mem::replace(&mut self.debug_selected, true) {
                        self.cursor = self.people.get(n).map(|p| p.key.clone());
                        self.reveal = true;
                    }
                }
            }
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                cx.toasts.error("Could not load people statistics", &error);
                self.load_error = Some(error);
            }
        }
    }

    /// Open the person the command palette asked for (once, after a load).
    fn expand_requested(&mut self, cx: &mut UiCtx) {
        let Some(id) = cx.memory.expand_person.take() else {
            return;
        };
        let key = format!("p{id}");
        if let Some(person) = self.people.iter().find(|p| p.key == key) {
            // A filter that hides them would hide what was asked for.
            if !name_matches(person, &cx.memory.people_search.text) {
                cx.memory.people_search.clear();
            }
            self.expanded = Some(key);
        }
    }

    /// Debug builds only: `WITNESS_DEBUG_EXPAND_PERSON=<index>` opens that
    /// person's detail after the first load, for passive snapshots.
    #[cfg(debug_assertions)]
    fn debug_expand(&mut self) {
        if self.expanded.is_some() {
            return;
        }
        let index = std::env::var("WITNESS_DEBUG_EXPAND_PERSON")
            .ok()
            .and_then(|v| v.parse::<usize>().ok());
        if let Some(person) = index.and_then(|i| self.people.get(i)) {
            self.expanded = Some(person.key.clone());
        }
    }

    fn take_confirm_answer(&mut self, cx: &mut UiCtx) {
        let Some(answer) = cx.confirm.answer(egui::Id::new(CONFIRM_ID)) else {
            return;
        };
        if let Some((id, name)) = self.confirm_for.take() {
            if answer {
                forget(cx, id, name);
            }
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui, filter: &str, actions: &mut Vec<Action>) {
        ui.ctx()
            .accesskit_node_builder(ui.id().with("people-heading"), |node| {
                node.set_role(egui::accesskit::Role::Heading);
                node.set_label("People");
            });
        if (self.load.is_some() && self.people.is_empty()) || debug_flag("WITNESS_DEBUG_LOADING") {
            let due = self
                .load
                .as_ref()
                .is_none_or(|load| skeleton_due(ui, load.started));
            if due {
                people_skeleton(ui);
            }
            return;
        }
        if self.people.is_empty() {
            if let Some(error) = &self.load_error {
                let message = format!("Could not load people: {error}");
                if widgets::view_error(ui, &message, &["Try again"]).is_some() {
                    actions.push(Action::TryAgain);
                }
                return;
            }
            let clicked = empty_hero(
                ui,
                ph::USERS_THREE,
                "No people yet",
                "Speaking stats appear once a meeting is transcribed. Rename a speaker in a transcript to a person, and Witness recognises their voice in later meetings.",
                &["Go to meetings"],
                false,
            );
            if clicked.is_some() {
                actions.push(Action::GoMeetings);
            }
            return;
        }

        ui.add_space(S4);
        trend_card(ui, &self.weeks);
        widgets::section_header(ui, "People");
        ui.spacing_mut().item_spacing.y = 2.0;
        let mut shown = self
            .people
            .iter()
            .filter(|p| name_matches(p, filter))
            .peekable();
        if shown.peek().is_none() {
            let body = format!("No one's name contains \u{201c}{}\u{201d}.", filter.trim());
            if empty_hero(
                ui,
                ph::MAGNIFYING_GLASS,
                "No matching people",
                &body,
                &["Clear search"],
                true,
            )
            .is_some()
            {
                actions.push(Action::ClearSearch);
            }
        }
        for person in shown {
            let print = person
                .person_id
                .and_then(|id| self.prints.iter().find(|p| p.id == id));
            let open = self.expanded.as_deref() == Some(person.key.as_str());
            let selected = self.cursor.as_deref() == Some(person.key.as_str());
            let header = person_entry(ui, person, print, open, selected, actions);
            // Keyboard focus (Tab, egui's arrow moves) carries the
            // selection with it.
            if header.has_focus() && !selected {
                self.cursor = Some(person.key.clone());
            }
            if selected && std::mem::take(&mut self.reveal) {
                header.scroll_to_me(None);
            }
        }
        ui.add_space(S4);
        ui.add(
            egui::Label::new(
                egui::RichText::new(HINT)
                    .font(theme::regular(12.0))
                    .color(pal().text_faint),
            )
            .wrap(),
        );
    }
}

/// "Forget voice": delete the person (their stats rows vanish), then reload.
fn forget(cx: &mut UiCtx, id: i64, name: String) {
    cx.tasks
        .spawn_op("forget-voice", move |c| match c.delete_person(id) {
            Ok(()) => OpDone::Notice {
                notice: None,
                refresh: true,
            },
            Err(error) => OpDone::Notice {
                notice: Some(Notice::error(&format!("Could not forget {name}"), error)),
                refresh: false,
            },
        });
}

// ----------------------------------------------------------------------
// Widgets

/// The trend surface: the caption, then 12 bars of total meeting time per
/// week over a faint baseline, with the range's ends labelled below.
fn trend_card(ui: &mut egui::Ui, weeks: &[Week]) {
    widgets::group(ui, |ui| {
        ui.spacing_mut().item_spacing.y = S2;
        ui.add(
            egui::Label::new(
                egui::RichText::new(trend_title(weeks))
                    .font(theme::regular(12.5))
                    .color(pal().text_muted),
            )
            .wrap(),
        );
        ui.add_space(S1);
        trend_chart(ui, weeks);
        if let Some(first) = weeks.first() {
            // Axis ends: the oldest week's Monday and the current week.
            let height = ui.fonts_mut(|f| f.row_height(&axis_font()));
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::hover());
            let painter = ui.painter();
            painter.text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                first.from.format("%-d %b").to_string(),
                axis_font(),
                pal().text_faint,
            );
            painter.text(
                rect.right_center(),
                egui::Align2::RIGHT_CENTER,
                "This week",
                axis_font(),
                pal().text_faint,
            );
        }
    });
}

/// The first load's placeholder, on the real layout: the trend surface (a
/// caption bar, twelve bars on the baseline, the axis ends), the "People"
/// header and person rows (an avatar disc, a name bar over a summary bar).
fn people_skeleton(ui: &mut egui::Ui) {
    const BARS: [f32; WEEKS] = [
        0.35, 0.6, 0.2, 0.75, 0.5, 0.0, 0.4, 0.9, 0.3, 0.55, 0.65, 0.45,
    ];
    let bar = |left: f32, center_y: f32, w: f32, h: f32| {
        Rect::from_min_size(egui::pos2(left, center_y - h / 2.0), egui::vec2(w, h))
    };
    ui.add_space(S4);
    widgets::group(ui, |ui| {
        ui.spacing_mut().item_spacing.y = S2;
        let width = ui.available_width();
        let caption_h = ui.fonts_mut(|f| f.row_height(&theme::regular(12.5)));
        let (caption, _) = ui.allocate_exact_size(egui::vec2(width, caption_h), Sense::hover());
        let caption_w = (width * 0.7).min(280.0);
        skeleton_at(ui, bar(caption.left(), caption.center().y, caption_w, 10.0));
        ui.add_space(S1);
        let (chart, _) = ui.allocate_exact_size(egui::vec2(width, CHART_HEIGHT), Sense::hover());
        ui.painter().hline(
            chart.x_range(),
            chart.bottom() - 0.5,
            Stroke::new(1.0, pal().border),
        );
        let slot = chart.width() / WEEKS as f32;
        let bar_w = (slot * 0.6).clamp(3.0, BAR_MAX_WIDTH);
        for (i, fraction) in BARS.into_iter().enumerate() {
            let center_x = chart.left() + (i as f32 + 0.5) * slot;
            let h = (fraction * (chart.height() - S1)).max(3.0);
            let rect = Rect::from_min_max(
                egui::pos2(center_x - bar_w / 2.0, chart.bottom() - h),
                egui::pos2(center_x + bar_w / 2.0, chart.bottom()),
            );
            skeleton_at(ui, rect);
        }
        let axis_h = ui.fonts_mut(|f| f.row_height(&axis_font()));
        let (axis, _) = ui.allocate_exact_size(egui::vec2(width, axis_h), Sense::hover());
        skeleton_at(ui, bar(axis.left(), axis.center().y, 40.0, 8.0));
        skeleton_at(ui, bar(axis.right() - 56.0, axis.center().y, 56.0, 8.0));
    });
    widgets::section_header(ui, "People");
    ui.spacing_mut().item_spacing.y = 2.0;
    // `person_row`'s metrics: list_row padding 12 / 8 around the avatar and
    // the two text lines.
    let text_h = ui
        .fonts_mut(|f| f.row_height(&theme::semibold(14.0)) + f.row_height(&theme::regular(12.0)))
        + 1.0;
    let row_h = text_h.max(AVATAR) + 16.0;
    let width = ui.available_width();
    let measure = (width - 2.0 * 12.0 - AVATAR - S3 - CHEVRON - S3).max(40.0);
    for row in 0..5 {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_h), Sense::hover());
        let avatar = egui::pos2(rect.left() + 12.0 + AVATAR / 2.0, rect.center().y);
        ui.painter()
            .circle_filled(avatar, AVATAR / 2.0, pal().bg_hover);
        let left = rect.left() + 12.0 + AVATAR + S3;
        let name_w = measure.min(220.0) * [0.7, 0.55, 0.85, 0.6, 0.75][row];
        let summary_w = measure.min(220.0) * [0.9, 0.8, 0.95, 0.85, 0.8][row];
        skeleton_at(ui, bar(left, rect.center().y - 8.0, name_w, 12.0));
        skeleton_at(ui, bar(left, rect.center().y + 9.0, summary_w, 9.0));
    }
}

/// The chart's axis labels.
fn axis_font() -> FontId {
    theme::regular(11.5)
}

/// The bars: one slot per week across the available width (bars capped at
/// [`BAR_MAX_WIDTH`], centred in their slot), the current week and the
/// hovered one in full accent; each slot's tooltip is its week's summary.
fn trend_chart(ui: &mut egui::Ui, weeks: &[Week]) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), CHART_HEIGHT),
        Sense::hover(),
    );
    let slot = rect.width() / WEEKS as f32;
    let hovered = match response.hover_pos() {
        Some(pos) if !weeks.is_empty() => {
            Some((((pos.x - rect.left()) / slot).floor().max(0.0) as usize).min(weeks.len() - 1))
        }
        _ => None,
    };
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        // The baseline the bars stand on.
        painter.hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            Stroke::new(1.0, pal().border),
        );
        let max_ms = weeks.iter().map(|w| w.ms).max().unwrap_or(0).max(1);
        let bar_w = (slot * 0.6).clamp(3.0, BAR_MAX_WIDTH);
        let current = weeks.len().saturating_sub(1);
        for (i, week) in weeks.iter().enumerate() {
            let center_x = rect.left() + (i as f32 + 0.5) * slot;
            let emphasised = i == current || hovered == Some(i);
            let fraction = bar_fraction(week.ms, max_ms);
            if fraction == 0.0 {
                // An empty week: a short stub on the baseline keeps the slot.
                let stub = Rect::from_center_size(
                    egui::pos2(center_x, rect.bottom() - 2.0),
                    egui::vec2(bar_w, 3.0),
                );
                let color = if emphasised {
                    pal().text_faint
                } else {
                    pal().border_strong
                };
                painter.rect_filled(stub, CornerRadius::same(1), color);
                continue;
            }
            let h = fraction * (rect.height() - S1);
            let bar = Rect::from_min_max(
                egui::pos2(center_x - bar_w / 2.0, rect.bottom() - h),
                egui::pos2(center_x + bar_w / 2.0, rect.bottom()),
            );
            let color = if hovered == Some(i) {
                pal().accent_hover
            } else if emphasised {
                pal().accent
            } else {
                theme::alpha(pal().accent, 0.7)
            };
            let radius = CornerRadius {
                nw: 3,
                ne: 3,
                sw: 0,
                se: 0,
            };
            painter.rect_filled(bar, radius, color);
        }
    }
    let summary = weeks.iter().map(week_text).collect::<Vec<_>>().join("; ");
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &summary));
    match hovered {
        Some(index) => response.on_hover_text_at_pointer(week_text(&weeks[index])),
        None => response,
    }
}

/// A single-line galley elided with `…` at `width` (`galley.elided` says
/// whether it had to).
fn elided(
    ui: &egui::Ui,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(1.0));
    ui.painter().layout_job(job)
}

/// The expand chevron: a caret pointing right, turned down by `t` (0..1).
fn paint_chevron(ui: &egui::Ui, rect: Rect, t: f32, color: Color32) {
    let angle = t * std::f32::consts::FRAC_PI_2;
    let (sin, cos) = angle.sin_cos();
    let c = rect.center();
    let points = [(-2.0, -4.5), (2.5, 0.0), (-2.0, 4.5)]
        .map(|(x, y): (f32, f32)| egui::pos2(c.x + x * cos - y * sin, c.y + x * sin + y * cos));
    ui.painter()
        .add(egui::Shape::line(points.to_vec(), Stroke::new(1.5, color)));
}

/// A person's list row: avatar, name and the one-line summary, and the
/// chevron that turns when the detail is open; filled while it has the
/// keyboard selection. Returns the row's response (click = toggle) and the
/// content's rect.
fn person_row(ui: &mut egui::Ui, p: &PersonAgg, open: bool, selected: bool) -> (Response, Rect) {
    let summary = summary_text(p);
    let (row, content) = widgets::list_row(ui, ("person-row", &p.key), selected, |ui, hovered| {
        let name_font = theme::semibold(14.0);
        let meta_font = theme::regular(12.0);
        let text_h = ui.fonts_mut(|f| f.row_height(&name_font) + f.row_height(&meta_font)) + 1.0;
        let height = text_h.max(AVATAR);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), height),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = S3;
                widgets::avatar(ui, &p.name, person_color(p), AVATAR);
                let text_w = (ui.available_width() - CHEVRON - S3).max(0.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(text_w, text_h),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(text_w);
                        ui.spacing_mut().item_spacing.y = 1.0;
                        widgets::elide(ui, &p.name, name_font, pal().text);
                        widgets::elide(ui, &summary, meta_font, pal().text_muted);
                    },
                );
                let (mark, _) =
                    ui.allocate_exact_size(egui::vec2(CHEVRON, CHEVRON), Sense::hover());
                let t = if icons::animations_enabled() {
                    let id = egui::Id::new(("person-chevron", &p.key));
                    ui.ctx().animate_bool_responsive(id, open)
                } else if open {
                    1.0
                } else {
                    0.0
                };
                let color = if hovered || open {
                    pal().text
                } else {
                    pal().text_muted
                };
                paint_chevron(ui, mark, t, color);
            },
        )
        .response
        .rect
    });
    follow_focus(&row);
    (row, content)
}

/// The detail's key/value grid: two columns when there is room, one
/// otherwise; keys elide (full key as tooltip), values never do.
fn stats_grid(ui: &mut egui::Ui, pairs: &[(&'static str, String)]) {
    const GAP_X: f32 = 24.0;
    let key_font = theme::regular(12.5);
    let value_font = theme::semibold(13.0);
    let width = ui.available_width();
    let cols = if width >= 400.0 { 2 } else { 1 };
    let cell_w = (width - GAP_X * (cols - 1) as f32) / cols as f32;
    let row_h = ui.fonts_mut(|f| f.row_height(&value_font).max(f.row_height(&key_font)));
    ui.spacing_mut().item_spacing.y = 10.0;
    for chunk in pairs.chunks(cols) {
        // One exact-height line per grid row (`ui.horizontal` would pad it
        // to the 32 pt control height); cells are placed inside it.
        let (line, _) = ui.allocate_exact_size(egui::vec2(width, row_h), Sense::hover());
        for (col, (key, value)) in chunk.iter().enumerate() {
            let cell = Rect::from_min_size(
                line.min + egui::vec2(col as f32 * (cell_w + GAP_X), 0.0),
                egui::vec2(cell_w, row_h),
            );
            let response = ui.interact(cell, ui.id().with(("stat", *key)), Sense::hover());
            let value_g =
                ui.painter()
                    .layout_no_wrap(value.clone(), value_font.clone(), pal().text);
            let key_w = cell_w - value_g.size().x - S2;
            let key_g = elided(ui, key, key_font.clone(), pal().text_muted, key_w);
            let key_elided = key_g.elided;
            let painter = ui.painter();
            painter.galley(
                egui::pos2(cell.left(), cell.center().y - key_g.size().y / 2.0),
                key_g,
                pal().text_muted,
            );
            painter.galley(
                egui::pos2(
                    cell.right() - value_g.size().x,
                    cell.center().y - value_g.size().y / 2.0,
                ),
                value_g,
                pal().text,
            );
            let name = format!("{key}: {value}");
            response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &name));
            if key_elided {
                response.on_hover_text(*key);
            }
        }
    }
}

/// One meeting shared with the person: title and meta line, both elided
/// (the tooltip and accessible name carry the whole text). Click opens it.
fn meeting_row(ui: &mut egui::Ui, p: &PersonAgg, m: &PersonMeetingStat) -> Response {
    let meta = meeting_meta(m);
    let (row, ()) = widgets::list_row(
        ui,
        ("person-meeting", &p.key, m.meeting_id),
        false,
        |ui, _| {
            ui.spacing_mut().item_spacing.y = 1.0;
            widgets::elide(ui, &m.meeting_title, theme::regular(14.0), pal().text);
            widgets::elide(ui, &meta, theme::regular(12.0), pal().text_muted);
        },
    );
    let name = format!("{}, {meta}", m.meeting_title);
    row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    follow_focus(&row);
    row
}

/// One person: the row, and when open the detail under it: the stats
/// grid, their meetings and "Forget voice…" (named people only). Returns
/// the row's response.
fn person_entry(
    ui: &mut egui::Ui,
    p: &PersonAgg,
    print: Option<&PersonInfo>,
    open: bool,
    selected: bool,
    actions: &mut Vec<Action>,
) -> Response {
    let sample_seconds = print.map(|x| x.sample_seconds);
    let (header, _) = person_row(ui, p, open, selected);
    let name = format!("{}: {}", p.name, stats_text(p, sample_seconds));
    header.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::Button, true, &name);
        info.selected = Some(open); // aria-expanded
        info
    });
    if header.clicked() {
        actions.push(Action::Toggle(p.key.clone()));
    }
    if !open {
        return header;
    }

    // The detail hangs under the name (flush left when compact).
    let indent = if widgets::is_compact(ui) {
        0.0
    } else {
        AVATAR + S3
    };
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(indent);
        let width = ui.available_width();
        ui.vertical(|ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add_space(S1);
            widgets::group(ui, |ui| stats_grid(ui, &stat_pairs(p, sample_seconds)));
            ui.add_space(S2);
            for m in &p.meetings {
                if meeting_row(ui, p, m).clicked() {
                    actions.push(Action::Open(m.meeting_id));
                }
            }
            if let (false, Some(id)) = (p.is_me, p.person_id) {
                // Destructive but secondary: a ghost button in light red, its
                // text roughly in line with the meeting titles above.
                ui.add_space(S1);
                let button = widgets::btn_ghost(
                    ui,
                    egui::RichText::new("Forget voice…").color(pal().rec_light),
                )
                .on_hover_text("Forget this voice — Shift-click to skip confirmation");
                follow_focus(&button);
                if button.clicked() {
                    actions.push(Action::Forget {
                        id,
                        name: p.name.clone(),
                        skip: ui.input(|i| i.modifiers.shift),
                    });
                }
            }
            ui.add_space(S3);
        });
    });
    header
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::views::meetings::Motion;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn meeting_on(date: NaiveDate, hour: u32, duration_ms: Option<i64>) -> Meeting {
        let at = date
            .and_hms_opt(hour, 30, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap();
        Meeting {
            id: 1,
            started_at: at.to_rfc3339(),
            ended_at: None,
            title: "m".into(),
            audio_path: None,
            duration_ms,
            status: "transcribed".into(),
            engine: None,
            trigger: "manual".into(),
            notes: String::new(),
            ..Meeting::default()
        }
    }

    fn stat(
        person: Option<i64>,
        name: &str,
        meeting: i64,
        duration: i64,
        talk: i64,
    ) -> PersonMeetingStat {
        PersonMeetingStat {
            person_id: person,
            person_name: name.into(),
            meeting_id: meeting,
            meeting_title: format!("Meeting {meeting}"),
            started_at: "2026-10-05T09:00:00+01:00".into(),
            duration_ms: duration,
            talk_ms: talk,
            words: 1234,
            segments: 3,
        }
    }

    #[test]
    fn weeks_start_on_monday() {
        // 2026-10-05 is a Monday.
        assert_eq!(week_start(day(2026, 10, 5)), day(2026, 10, 5));
        assert_eq!(week_start(day(2026, 10, 11)), day(2026, 10, 5));
        assert_eq!(week_start(day(2026, 10, 12)), day(2026, 10, 12));
        assert_eq!(trend_start(day(2026, 10, 7)), day(2026, 7, 20));
    }

    #[test]
    fn meetings_bucket_into_twelve_weeks() {
        let today = day(2026, 10, 7);
        let meetings = vec![
            meeting_on(day(2026, 10, 5), 0, Some(60_000)), // this week, local midnight-ish
            meeting_on(day(2026, 10, 11), 23, None),       // this week, still recording
            meeting_on(day(2026, 7, 20), 9, Some(1_000)),  // oldest bucket
            meeting_on(day(2026, 7, 19), 9, Some(5_000)),  // too old
        ];
        let weeks = bucket_weeks(today, &meetings);
        assert_eq!(weeks.len(), WEEKS);
        assert_eq!(weeks[0].from, day(2026, 7, 20));
        assert_eq!(weeks[11].from, day(2026, 10, 5));
        assert_eq!((weeks[11].count, weeks[11].ms), (2, 60_000));
        assert_eq!((weeks[0].count, weeks[0].ms), (1, 1_000));
        assert_eq!(weeks.iter().map(|w| w.count).sum::<usize>(), 3);
        assert_eq!(
            trend_title(&weeks),
            "Last 12 weeks — 3 meetings, 1 min total"
        );
        assert_eq!(week_text(&weeks[0]), "Week of 20/07/2026: 1 meeting, 1 s");
        assert_eq!(week_text(&weeks[1]), "Week of 27/07/2026: 0 meetings, 0 s");
    }

    #[test]
    fn bars_scale_to_total_time() {
        assert_eq!(bar_fraction(0, 100), 0.0);
        assert_eq!(bar_fraction(100, 100), 1.0);
        assert_eq!(bar_fraction(1, 1_000_000), 0.06);
        assert_eq!(bar_fraction(50, 100), 0.5);
        assert_eq!(bar_fraction(5, 0), 1.0); // never taller than the chart
    }

    #[test]
    fn summary_and_detail_pairs() {
        let stats = vec![
            stat(None, "Me", 1, 600_000, 300_000),
            stat(Some(2), "Bob", 1, 600_000, 150_000),
            stat(Some(2), "Bob", 2, 0, 0),
        ];
        let people = aggregate(&stats);
        assert_eq!(summary_text(&people[0]), "1 meeting · 5 min speaking");
        assert_eq!(summary_text(&people[1]), "2 meetings · 2 min speaking");
        let keys = |pairs: Vec<(&'static str, String)>| -> Vec<&'static str> {
            pairs.into_iter().map(|(k, _)| k).collect()
        };
        assert_eq!(
            keys(stat_pairs(&people[0], None)),
            [
                "Meetings",
                "Speaking time",
                "Words",
                "Share of meeting time"
            ]
        );
        assert_eq!(
            stat_pairs(&people[1], Some(89.0)),
            [
                ("Meetings", "2".to_string()),
                ("Time together", "10 min".to_string()),
                ("Speaking time", "2 min".to_string()),
                ("Words", "2,468".to_string()),
                ("Share of meeting time", "25%".to_string()),
                ("Speech learned", "1 min".to_string()),
            ]
        );
        // Avatar colours: Me's speaker colour; others stable by id.
        assert_eq!(person_color(&people[0]), theme::chip_color("me"));
        assert_eq!(person_color(&people[1]), theme::chip_color("S3"));
    }

    #[test]
    fn people_aggregate_with_me_first() {
        let stats = vec![
            stat(Some(2), "Bob", 1, 1_000, 100),
            stat(None, "Me", 1, 1_000, 500),
            stat(Some(3), "Cara", 2, 9_000, 100),
            stat(Some(2), "Bob", 2, 9_000, 200),
        ];
        let people = aggregate(&stats);
        let keys: Vec<_> = people.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["me", "p2", "p3"]);
        let bob = &people[1];
        assert_eq!(
            (bob.meeting_count, bob.together_ms, bob.talk_ms, bob.words),
            (2, 10_000, 300, 2468)
        );
        assert_eq!(bob.meetings[0].meeting_id, 1, "backend order kept");
    }

    #[test]
    fn stats_text_matches_the_tsx() {
        let stats = vec![
            stat(None, "Me", 1, 600_000, 300_000),
            stat(Some(2), "Bob", 1, 600_000, 150_000),
        ];
        let people = aggregate(&stats);
        assert_eq!(
            stats_text(&people[0], None),
            "1 meeting · 5 min speaking · 1,234 words · 50% of meeting time"
        );
        assert_eq!(
            stats_text(&people[1], Some(89.0)),
            "1 meeting · 10 min together · 2 min speaking · 1,234 words · 25% of meeting time · 1 min of speech learned"
        );
        let mut zero = people[1].clone();
        zero.together_ms = 0;
        zero.meeting_count = 2;
        assert_eq!(
            stats_text(&zero, Some(10.0)),
            "2 meetings · 0 s together · 2 min speaking · 1,234 words · 0 min of speech learned"
        );
    }

    #[test]
    fn meeting_meta_percent_only_with_duration() {
        let with = stat(Some(2), "Bob", 1, 600_000, 150_000);
        assert!(
            meeting_meta(&with).ends_with(" · 10 min meeting · 2 min speaking · 1,234 words · 25%")
        );
        let without = stat(Some(2), "Bob", 1, 0, 0);
        assert!(meeting_meta(&without).ends_with(" · 0 s meeting · 0 s speaking · 1,234 words"));
    }

    #[test]
    fn rows_detail_and_chart_lay_out() {
        let ctx = theme::test_context();
        for width in [1000.0, 380.0] {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 2000.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let stats = vec![
                    stat(None, "Me", 1, 600_000, 300_000),
                    stat(Some(2), "Bob", 1, 600_000, 150_000),
                ];
                let people = aggregate(&stats);
                let weeks = bucket_weeks(day(2026, 10, 7), &[]);
                trend_card(ui, &weeks);
                let mut actions = Vec::new();
                person_entry(ui, &people[0], None, true, true, &mut actions);
                person_entry(ui, &people[1], None, true, false, &mut actions);
                person_entry(ui, &people[1], None, false, true, &mut actions);
                assert!(actions.is_empty());
                // Nothing grew past the window.
                assert!(ui.min_rect().right() <= width + 0.5, "at {width}");
            });
            output.textures_delta.clear();
        }
    }

    /// The row never overflows its width (long names elide), and the
    /// avatar and chevron centre on it.
    #[test]
    fn person_row_fits_and_centres_avatar_and_chevron() {
        let ctx = theme::test_context();
        let person = PersonAgg {
            key: "p7".into(),
            person_id: Some(7),
            name: "Christopher Warriner-Montgomery of the Very Long Name".into(),
            is_me: false,
            meetings: Vec::new(),
            meeting_count: 12,
            together_ms: 3_600_000,
            talk_ms: 1_200_000,
            words: 4_321,
        };
        for width in [900.0, 300.0] {
            let mut rows = Vec::new();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 400.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    let (row, content) = person_row(ui, &person, false, true);
                    rows.push((row.rect, content));
                },
            );
            output.textures_delta.clear();
            let (row, content) = rows[0];
            assert!(row.right() <= width + 0.5, "{row:?} at {width}");
            for clipped in output.shapes {
                let rect = clipped.shape.visual_bounding_rect();
                if !rect.is_positive() || !row.intersects(rect) {
                    continue;
                }
                assert!(
                    rect.left() >= row.left() - 0.5 && rect.right() <= row.right() + 0.5,
                    "{rect:?} overflows {row:?} at {width}"
                );
                if matches!(clipped.shape, egui::Shape::Circle(_) | egui::Shape::Path(_)) {
                    assert!(
                        (rect.center().y - content.center().y).abs() < 1.5,
                        "{rect:?} off {content:?} at {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn keys_select_people_and_toggle_their_detail() {
        let mut view = PeopleView::new();
        view.people = aggregate(&[
            stat(None, "Me", 1, 1_000, 500),
            stat(Some(2), "Bob", 1, 1_000, 100),
            stat(Some(3), "Cara", 2, 9_000, 100),
        ]);
        let order: Vec<String> = view.people.iter().map(|p| p.key.clone()).collect();
        let step = |motion| ListKeys {
            motion: Some(motion),
            ..ListKeys::default()
        };
        let toggle = ListKeys {
            activate: true,
            ..ListKeys::default()
        };
        // Nothing selected: Enter does nothing.
        assert!(view.key_actions(toggle, "").is_empty());
        view.key_actions(step(Motion::By(1)), "");
        assert_eq!(view.cursor.as_ref(), Some(&order[0]));
        assert!(view.reveal);
        view.key_actions(step(Motion::Last), "");
        assert_eq!(
            view.key_actions(toggle, ""),
            vec![Action::Toggle(order[2].clone())]
        );
        view.key_actions(step(Motion::By(-1)), "");
        assert_eq!(view.cursor.as_ref(), Some(&order[1]));
        // A forgotten person's selection is inert.
        view.people.truncate(1);
        assert!(view.key_actions(toggle, "").is_empty());
    }

    #[test]
    fn the_name_filter_ignores_case_and_blanks() {
        let people = aggregate(&[
            stat(None, "Me", 1, 1_000, 500),
            stat(Some(2), "Bob Ávila", 1, 1_000, 100),
            stat(Some(3), "Cara", 2, 9_000, 100),
        ]);
        let names = |filter: &str| -> Vec<&str> {
            people
                .iter()
                .filter(|p| name_matches(p, filter))
                .map(|p| p.name.as_str())
                .collect()
        };
        assert_eq!(names(""), ["Me", "Cara", "Bob Ávila"]);
        assert_eq!(names("   "), names(""));
        assert_eq!(names("ca"), ["Cara"]);
        assert_eq!(names(" AVI"), Vec::<&str>::new(), "accents are kept");
        assert_eq!(names("ávila "), ["Bob Ávila"]);
        assert_eq!(names("ÁVILA"), ["Bob Ávila"]);
        assert!(names("zed").is_empty());
    }

    #[test]
    fn keys_walk_only_the_people_the_filter_shows() {
        let mut view = PeopleView::new();
        view.people = aggregate(&[
            stat(None, "Me", 1, 1_000, 500),
            stat(Some(2), "Bob", 1, 1_000, 100),
            stat(Some(3), "Cara", 2, 9_000, 100),
            stat(Some(4), "Carl", 2, 5_000, 100),
        ]);
        let step = |motion| ListKeys {
            motion: Some(motion),
            ..ListKeys::default()
        };
        let toggle = ListKeys {
            activate: true,
            ..ListKeys::default()
        };
        view.key_actions(step(Motion::By(1)), "car");
        assert_eq!(view.cursor.as_deref(), Some("p3"));
        view.key_actions(step(Motion::Last), "car");
        assert_eq!(view.cursor.as_deref(), Some("p4"));
        assert_eq!(
            view.key_actions(toggle, "car"),
            vec![Action::Toggle("p4".into())]
        );
        // A selection the filter now hides is inert.
        assert!(view.key_actions(toggle, "bob").is_empty());
    }

    #[test]
    fn the_people_skeleton_fits() {
        let ctx = theme::test_context();
        for width in [380.0, 1000.0] {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 760.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, people_skeleton);
            output.textures_delta.clear();
            let shapes: Vec<Rect> = output
                .shapes
                .iter()
                .map(|c| c.shape.visual_bounding_rect())
                .filter(|r| r.is_positive())
                .collect();
            // The caption, 12 bars, two axis ends, five avatars + ten bars.
            assert!(shapes.len() >= 30, "{} shapes at {width}", shapes.len());
            assert!(
                shapes.iter().all(|r| r.right() <= width + 0.5),
                "at {width}"
            );
        }
    }
}
