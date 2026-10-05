//! People: per-person stats, the 12-week trend and voice prints
//! (map-ui-meetings §3, people.tsx).
//!
//! Three loads run in parallel on mount and on every `refresh_tick`;
//! any failure fails the whole load. The trend reads only the meetings of
//! the last 12 weeks (`meetings_started_since`) instead of paging through
//! every meeting like the TSX did.

use crate::bridge::{OpDone, Task};
use crate::ui::format::{fmt_date, fmt_date_short, fmt_duration, fmt_thousands};
use crate::ui::theme::{self, ACCENT, BG_HOVER, BG_RAISED, BORDER, TEXT, TEXT_MUTED};
use crate::ui::toasts::Notice;
use crate::ui::views::meetings::{open_transcript, paint_focus_ring, truncated};
use crate::ui::widgets::{self, KeyScroll};
use crate::ui::{UiCtx, View};
use chrono::{DateTime, Datelike, Days, Local, NaiveDate};
use eframe::egui::{
    self, Align, CornerRadius, CursorIcon, Layout, Rect, Response, Sense, Stroke, UiBuilder,
    WidgetInfo, WidgetType,
};
use witness_core::db::{Meeting, PersonMeetingStat};
use witness_core::service::PersonInfo;
use witness_core::CoreResult;

/// Buckets in the trend: the current week and the 11 before it.
const WEEKS: usize = 12;
/// The chart's rendered height (the SVG's 100×34 box at 68 px).
const CHART_HEIGHT: f32 = 68.0;
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

/// Bar height in the chart's 34-unit box: total meeting time scaled to
/// 30 units (at least 2), or a 0.8-unit stub for an empty week.
fn bar_height(ms: i64, max_ms: i64) -> f32 {
    if ms > 0 {
        (ms as f32 / max_ms.max(1) as f32 * 30.0).max(2.0)
    } else {
        0.8
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
}

enum Action {
    Toggle(String),
    Open(i64),
    Forget { id: i64, name: String, skip: bool },
    TryAgain,
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
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if self.seen_tick != Some(cx.shell.refresh_tick) {
            self.seen_tick = Some(cx.shell.refresh_tick);
            self.start(cx);
        }
        self.poll(ui, cx);
        self.take_confirm_answer(cx);

        let mut actions = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("people-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::keyboard_scroll(ui, KeyScroll { space: true });
                widgets::centered_column(ui, 900.0, |ui| self.draw(ui, &mut actions));
            });
        for action in actions {
            match action {
                Action::Toggle(key) => {
                    self.expanded = if self.expanded.as_ref() == Some(&key) {
                        None
                    } else {
                        Some(key)
                    };
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
        });
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
            }
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                cx.toasts.error("Could not load people statistics", &error);
                self.load_error = Some(error);
            }
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

    fn draw(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.ctx()
            .accesskit_node_builder(ui.id().with("people-heading"), |node| {
                node.set_role(egui::accesskit::Role::Heading);
                node.set_label("People");
            });
        if self.load.is_some() && self.people.is_empty() {
            widgets::empty_state(ui, "Loading people…", None);
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
            widgets::empty_state(
                ui,
                "No people yet.",
                Some("Stats appear once meetings are transcribed. Rename speakers in a transcript to track individual people across meetings."),
            );
            return;
        }

        ui.spacing_mut().item_spacing.y = 8.0;
        trend_card(ui, &self.weeks);
        for person in &self.people {
            let print = person
                .person_id
                .and_then(|id| self.prints.iter().find(|p| p.id == id));
            let open = self.expanded.as_deref() == Some(person.key.as_str());
            person_card(ui, person, print, open, actions);
        }
        ui.add(egui::Label::new(egui::RichText::new(HINT).size(12.0).color(TEXT_MUTED)).wrap());
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

/// `.person-card` frame (raised, 1 px border, radius 8, no padding).
fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BG_RAISED)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
}

/// The trend card: title plus 12 bars of total meeting time per week.
fn trend_card(ui: &mut egui::Ui, weeks: &[Week]) {
    card_frame()
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.add(
                egui::Label::new(
                    egui::RichText::new(trend_title(weeks))
                        .size(12.0)
                        .color(TEXT_MUTED),
                )
                .wrap(),
            );
            trend_chart(ui, weeks);
        });
}

/// The bars, drawn in the SVG's 100×34 logical box stretched to the
/// available width × 68 pt; each slot's tooltip is its week's summary.
fn trend_chart(ui: &mut egui::Ui, weeks: &[Week]) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), CHART_HEIGHT),
        Sense::hover(),
    );
    let slot = rect.width() / WEEKS as f32;
    let unit_y = rect.height() / 34.0;
    let max_ms = weeks.iter().map(|w| w.ms).max().unwrap_or(0).max(1);
    if ui.is_rect_visible(rect) {
        for (i, week) in weeks.iter().enumerate() {
            let h = bar_height(week.ms, max_ms);
            let bar = Rect::from_min_size(
                egui::pos2(
                    rect.left() + i as f32 * slot + 0.15 * slot,
                    rect.top() + (32.0 - h) * unit_y,
                ),
                egui::vec2(0.7 * slot, h * unit_y),
            );
            let color = if week.ms > 0 { ACCENT } else { BORDER };
            let radius = if week.ms > 0 { 2 } else { 1 };
            ui.painter()
                .rect_filled(bar, CornerRadius::same(radius), color.gamma_multiply(0.85));
        }
    }
    let summary = weeks.iter().map(week_text).collect::<Vec<_>>().join("; ");
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &summary));
    match response.hover_pos() {
        Some(pos) if !weeks.is_empty() => {
            let index =
                (((pos.x - rect.left()) / slot).floor().max(0.0) as usize).min(weeks.len() - 1);
            response.on_hover_text_at_pointer(week_text(&weeks[index]))
        }
        _ => response,
    }
}

/// A clickable full-width area with a hover background behind `add`'s
/// content (the background corners follow `radius`).
fn hover_area(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    margin: egui::Margin,
    radius: CornerRadius,
    add: impl FnOnce(&mut egui::Ui),
) -> Response {
    let background = ui.painter().add(egui::Shape::Noop);
    let response = ui
        .scope_builder(
            UiBuilder::new().id_salt(id_salt).sense(Sense::click()),
            |ui| {
                egui::Frame::new().inner_margin(margin).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    add(ui);
                });
            },
        )
        .response;
    if response.hovered() || response.has_focus() {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(response.rect, radius, BG_HOVER),
        );
    }
    if response.has_focus() {
        paint_focus_ring(ui, response.rect.shrink(2.0), 6);
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// One person card: the header (chip, stats, chevron) toggles the
/// accordion; the open card lists their meetings and "Forget voice…".
fn person_card(
    ui: &mut egui::Ui,
    p: &PersonAgg,
    print: Option<&PersonInfo>,
    open: bool,
    actions: &mut Vec<Action>,
) {
    let stats = stats_text(p, print.map(|x| x.sample_seconds));
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let header_radius = if open {
            CornerRadius {
                nw: 8,
                ne: 8,
                sw: 0,
                se: 0,
            }
        } else {
            CornerRadius::same(8)
        };
        let header = hover_area(
            ui,
            ("person-head", &p.key),
            egui::Margin::symmetric(14, 10),
            header_radius,
            |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let color = theme::chip_color(if p.is_me { "me" } else { "S1" });
                    widgets::chip(ui, &p.name, color, 16.0).on_hover_text(&p.name);
                    let chevron = if open { "▾" } else { "▸" };
                    let chevron_width = 12.0;
                    let stats_width = (ui.available_width() - chevron_width - 10.0).max(40.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(stats_width, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(stats_width);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&stats).size(13.0).color(TEXT_MUTED),
                                )
                                .wrap(),
                            );
                        },
                    );
                    ui.label(egui::RichText::new(chevron).color(TEXT_MUTED));
                });
            },
        );
        let name = format!("{}: {stats}", p.name);
        header.widget_info(|| {
            let mut info = WidgetInfo::labeled(WidgetType::Button, true, &name);
            info.selected = Some(open); // aria-expanded
            info
        });
        if header.clicked() {
            actions.push(Action::Toggle(p.key.clone()));
        }
        if !open {
            return;
        }

        // `.person-meetings`: 1 px top border, one row per meeting.
        let (_, line) = ui.allocate_space(egui::vec2(ui.available_width(), 1.0));
        ui.painter().line_segment(
            [line.left_center(), line.right_center()],
            Stroke::new(1.0, BORDER),
        );
        let forget = !p.is_me && p.person_id.is_some();
        let count = p.meetings.len();
        for (index, m) in p.meetings.iter().enumerate() {
            let last = !forget && index + 1 == count;
            let radius = if last {
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 8,
                    se: 8,
                }
            } else {
                CornerRadius::ZERO
            };
            let row = hover_area(
                ui,
                ("person-meeting", &p.key, m.meeting_id),
                egui::Margin {
                    left: 24,
                    right: 14,
                    top: 8,
                    bottom: 8,
                },
                radius,
                |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let width = ui.available_width();
                    let title = truncated(ui, &m.meeting_title, theme::semibold(14.0), TEXT, width);
                    let (rect, _) = ui.allocate_exact_size(title.size(), Sense::hover());
                    ui.painter().galley(rect.min, title, TEXT);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(meeting_meta(m))
                                .size(12.0)
                                .color(TEXT_MUTED),
                        )
                        .wrap(),
                    );
                },
            );
            let name = format!("{}, {}", m.meeting_title, meeting_meta(m));
            row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
            if row.clicked() {
                actions.push(Action::Open(m.meeting_id));
            }
        }
        if let (true, Some(id)) = (forget, p.person_id) {
            let button = widgets::btn_ghost(ui, "Forget voice…")
                .on_hover_text("Forget this voice — Shift-click to skip confirmation");
            if button.clicked() {
                actions.push(Action::Forget {
                    id,
                    name: p.name.clone(),
                    skip: ui.input(|i| i.modifiers.shift),
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(bar_height(0, 100), 0.8);
        assert_eq!(bar_height(100, 100), 30.0);
        assert_eq!(bar_height(1, 1_000_000), 2.0);
        assert_eq!(bar_height(50, 100), 15.0);
        assert_eq!(bar_height(5, 0), 30.0 * 5.0); // max clamps to 1
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
    fn cards_and_chart_lay_out() {
        let ctx = theme::test_context();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let stats = vec![
                stat(None, "Me", 1, 600_000, 300_000),
                stat(Some(2), "Bob", 1, 600_000, 150_000),
            ];
            let people = aggregate(&stats);
            let weeks = bucket_weeks(day(2026, 10, 7), &[]);
            trend_card(ui, &weeks);
            let mut actions = Vec::new();
            person_card(ui, &people[0], None, true, &mut actions);
            person_card(ui, &people[1], None, true, &mut actions);
            person_card(ui, &people[1], None, false, &mut actions);
            assert!(actions.is_empty());
        });
        output.textures_delta.clear();
    }
}
