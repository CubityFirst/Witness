//! Stats: archive-wide insights, for fun. Headline tiles, who you meet
//! most, the talk balance (share, pace, questions), filler words, a word
//! cloud for each side, the week grid of when meetings start and a couple
//! of records. Everything comes from one `AppCore::stats_report` pass
//! (witness_core::stats), reloaded on every `refresh_tick`; the last report
//! stays on screen while a reload runs.

use crate::bridge::Task;
use crate::ui::format::{fmt_duration, fmt_thousands};
use crate::ui::icons::ph;
use crate::ui::theme::{self, pal, S1, S2, S3, S4};
use crate::ui::views::meetings::{
    empty_hero, open_transcript, paint_focus_ring, skeleton_due, truncated,
};
use crate::ui::views::people::stats_grid;
use crate::ui::widgets::{self, KeyScroll};
use crate::ui::{UiCtx, View};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, Response, Sense, Stroke, WidgetInfo,
    WidgetType,
};
use std::time::Instant;
use witness_core::stats::{FillerCount, StatsReport, Voice, WordCount};
use witness_core::CoreResult;

/// People in "Who you talk to most".
const TOP_PEOPLE: usize = 8;
/// Bar thickness in the horizontal bar charts.
const BAR: f32 = 8.0;
/// Rows of the bar charts.
const ROW_H: f32 = 28.0;
/// The word cloud's height (taller when narrow, where words wrap less).
const CLOUD_H: f32 = 190.0;
const CLOUD_H_NARROW: f32 = 240.0;
const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const HINT: &str = "Stats cover every meeting outside the recycle bin. \u{201c}You\u{201d} is your microphone track, \u{201c}others\u{201d} everything Witness heard from the call. Filler counts include every use of a word, so \u{201c}like\u{201d} also counts \u{201c}I\u{2019}d like to\u{201d}.";

#[derive(Debug, Clone, PartialEq)]
enum Action {
    OpenPerson(i64),
    OpenMeeting(i64),
    TryAgain,
    GoMeetings,
}

/// A word placed in a cloud, relative to the cloud's top-left corner.
#[derive(Debug, Clone, PartialEq)]
struct Placed {
    index: usize,
    size: f32,
    rect: Rect,
}

/// A cloud laid out for one width.
struct Cloud {
    width: f32,
    words: Vec<Placed>,
}

pub struct StatsView {
    /// Boxed: the report is large and `ActiveView` holds views inline.
    report: Option<Box<StatsReport>>,
    load: Option<(Task<CoreResult<StatsReport>>, Instant)>,
    load_error: Option<String>,
    seen_tick: Option<u64>,
    /// Your cloud and theirs, laid out for the last width drawn.
    clouds: [Option<Cloud>; 2],
}

impl Default for StatsView {
    fn default() -> Self {
        Self::new()
    }
}

impl StatsView {
    pub fn new() -> Self {
        StatsView {
            report: None,
            load: None,
            load_error: None,
            seen_tick: None,
            clouds: [None, None],
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if self.seen_tick != Some(cx.shell.refresh_tick) {
            self.seen_tick = Some(cx.shell.refresh_tick);
            self.start(cx);
        }
        self.poll(ui, cx);
        let mut actions = Vec::new();
        widgets::scroll_column(ui, "stats-scroll", KeyScroll { space: true }, |ui| {
            self.draw(ui, &mut actions)
        });
        for action in actions {
            match action {
                Action::OpenPerson(id) => {
                    cx.memory.expand_person = Some(id);
                    cx.nav.go(View::People);
                }
                Action::OpenMeeting(id) => open_transcript(cx, id, View::Stats),
                Action::TryAgain => self.start(cx),
                Action::GoMeetings => cx.nav.go(View::Meetings),
            }
        }
    }

    /// Nothing to persist.
    pub fn flush(&mut self, _cx: &mut UiCtx) {}

    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
    }

    fn start(&mut self, cx: &UiCtx) {
        let task = cx.tasks.spawn("stats-report", |c| c.stats_report());
        self.load = Some((task, Instant::now()));
    }

    fn poll(&mut self, ui: &egui::Ui, cx: &mut UiCtx) {
        let Some(result) = self.load.as_mut().and_then(|(task, _)| task.poll_flat()) else {
            return;
        };
        self.load = None;
        match result {
            Ok(report) => {
                cx.fonts.note_texts(
                    ui.ctx(),
                    report
                        .people
                        .iter()
                        .map(|p| p.name.as_str())
                        .chain(report.my_words.iter().map(|w| w.word.as_str()))
                        .chain(report.their_words.iter().map(|w| w.word.as_str()))
                        .chain(report.longest_meeting.iter().map(|r| r.title.as_str()))
                        .chain(report.longest_monologue.iter().map(|r| r.title.as_str())),
                );
                self.report = Some(Box::new(report));
                self.clouds = [None, None];
                self.load_error = None;
            }
            Err(error) => {
                cx.toasts.error("Could not load stats", &error);
                self.load_error = Some(error);
            }
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.ctx()
            .accesskit_node_builder(ui.id().with("stats-heading"), |node| {
                node.set_role(egui::accesskit::Role::Heading);
                node.set_label("Stats");
            });
        let StatsView {
            report,
            load,
            load_error,
            clouds,
            ..
        } = self;
        let Some(r) = report.as_ref() else {
            if let Some(error) = load_error {
                let message = format!("Could not load stats: {error}");
                if widgets::view_error(ui, &message, &["Try again"]).is_some() {
                    actions.push(Action::TryAgain);
                }
            } else if load.as_ref().is_none_or(|(_, at)| skeleton_due(ui, *at)) {
                ui.add_space(S4);
                widgets::skeleton_rows(ui, 6);
            }
            return;
        };
        if r.meetings == 0 {
            let clicked = empty_hero(
                ui,
                ph::CHART_BAR,
                "No stats yet",
                "Stats appear once you have recorded and transcribed a meeting.",
                &["Go to meetings"],
                false,
            );
            if clicked.is_some() {
                actions.push(Action::GoMeetings);
            }
            return;
        }

        ui.add_space(S4);
        tiles(ui, r);
        widgets::section_header(ui, "Who you talk to most");
        widgets::group(ui, |ui| people_chart(ui, r, actions));
        widgets::section_header(ui, "How you talk");
        widgets::group(ui, |ui| balance(ui, r));
        widgets::section_header(ui, "Filler words");
        widgets::group(ui, |ui| fillers(ui, r));
        let [mine, theirs] = clouds;
        widgets::section_header(ui, "Your words");
        widgets::group(ui, |ui| {
            word_cloud(ui, "my-words", &r.my_words, mine, "Nothing you said yet.")
        });
        widgets::section_header(ui, "Their words");
        widgets::group(ui, |ui| {
            word_cloud(
                ui,
                "their-words",
                &r.their_words,
                theirs,
                "Nothing the others said yet.",
            )
        });
        widgets::section_header(ui, "When you meet");
        widgets::group(ui, |ui| week_grid(ui, &r.week_hours));
        records(ui, r, actions);
        ui.add_space(S4);
        faint_note(ui, HINT);
    }
}

// ----------------------------------------------------------------------
// Pure helpers

/// `part` of `whole` as a rounded percentage.
fn percent(part: i64, whole: i64) -> i64 {
    (part as f64 / whole.max(1) as f64 * 100.0).round() as i64
}

/// A pace, or a dash before there is a minute of speech.
fn pace_text(voice: &Voice) -> String {
    voice
        .wpm()
        .map_or_else(|| "\u{2014}".into(), |w| format!("{} wpm", w.round()))
}

/// Uses per 1,000 words, one decimal.
fn per_thousand(count: u64, words: u64) -> String {
    format!("{:.1}", count as f64 * 1000.0 / words.max(1) as f64)
}

/// `n time(s)`.
fn times(n: u64) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{} time{s}", fmt_thousands(n as i64))
}

/// The filler rows worth showing: any use at all, most yours first.
fn filler_rows(fillers: &[FillerCount]) -> Vec<FillerCount> {
    let mut rows: Vec<FillerCount> = fillers
        .iter()
        .copied()
        .filter(|f| f.me + f.others > 0)
        .collect();
    rows.sort_by(|a, b| b.me.cmp(&a.me).then_with(|| b.others.cmp(&a.others)));
    rows
}

/// "Most meetings start on Tuesdays around 14:00." (None without any.)
fn busiest_text(grid: &[[u32; 24]; 7]) -> Option<String> {
    let day_totals: Vec<u32> = grid.iter().map(|row| row.iter().sum()).collect();
    let hour_totals: Vec<u32> = (0..24)
        .map(|h| grid.iter().map(|row| row[h]).sum())
        .collect();
    // The first maximum: Monday before Sunday, morning before evening.
    let first_max = |totals: &[u32]| {
        let max = *totals.iter().max()?;
        (max > 0).then(|| totals.iter().position(|&t| t == max).expect("max"))
    };
    let day = first_max(&day_totals)?;
    let hour = first_max(&hour_totals)?;
    const PLURAL: [&str; 7] = [
        "Mondays",
        "Tuesdays",
        "Wednesdays",
        "Thursdays",
        "Fridays",
        "Saturdays",
        "Sundays",
    ];
    Some(format!(
        "Most meetings start on {} and around {hour:02}:00.",
        PLURAL[day]
    ))
}

/// Lay a word cloud out in `area`: the most used word in the middle, each
/// next one at the first free spot along an Archimedean spiral (stretched
/// to the area's shape), sized by the square root of its count. Words that
/// find no room are left out. `measure` gives a word's size at a font size.
fn layout_cloud(
    words: &[WordCount],
    area: egui::Vec2,
    mut measure: impl FnMut(&str, f32) -> egui::Vec2,
) -> Vec<Placed> {
    let (Some(first), Some(last)) = (words.first(), words.last()) else {
        return Vec::new();
    };
    let (max_c, min_c) = (first.count as f32, last.count as f32);
    let max_size = (area.y / 5.0).min(area.x / 14.0).clamp(16.0, 34.0);
    let min_size = 11.5;
    let bounds = Rect::from_min_size(egui::Pos2::ZERO, area);
    let center = bounds.center();
    let stretch = (area.x / area.y.max(1.0)).clamp(1.0, 3.0);
    let mut placed: Vec<Placed> = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let t = if max_c > min_c {
            ((word.count as f32 - min_c) / (max_c - min_c)).sqrt()
        } else {
            1.0
        };
        let size = min_size + (max_size - min_size) * t;
        let dims = measure(&word.word, size);
        if dims.x > area.x || dims.y > area.y {
            continue;
        }
        let mut theta = 0.0_f32;
        while theta < 140.0 {
            let r = 1.6 * theta;
            let offset = egui::vec2(r * theta.cos() * stretch, r * theta.sin());
            let rect = Rect::from_center_size(center + offset, dims);
            if bounds.contains_rect(rect)
                && !placed
                    .iter()
                    .any(|p| p.rect.expand2(egui::vec2(5.0, 1.0)).intersects(rect))
            {
                placed.push(Placed { index, size, rect });
                break;
            }
            theta += 0.1;
        }
    }
    placed
}

// ----------------------------------------------------------------------
// Widgets

fn faint_note(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .font(theme::regular(12.0))
                .color(pal().text_faint),
        )
        .wrap(),
    );
}

/// "Others" in two-series charts: a neutral beside the accent's "You".
fn others_color() -> Color32 {
    pal().text_faint
}

/// A bar growing right from `left`, its data end rounded.
fn paint_bar(ui: &egui::Ui, left: f32, center_y: f32, width: f32, height: f32, color: Color32) {
    if width <= 0.0 {
        return;
    }
    let rect = Rect::from_min_size(
        egui::pos2(left, center_y - height / 2.0),
        egui::vec2(width.max(3.0), height),
    );
    let end = (height / 2.0).min(4.0) as u8;
    let radius = CornerRadius {
        nw: 1,
        sw: 1,
        ne: end,
        se: end,
    };
    ui.painter().rect_filled(rect, radius, color);
}

/// The widest of `texts` in `font`.
fn widest(ui: &egui::Ui, texts: impl IntoIterator<Item = String>, font: &FontId) -> f32 {
    texts
        .into_iter()
        .map(|t| {
            ui.painter()
                .layout_no_wrap(t, font.clone(), pal().text)
                .size()
                .x
        })
        .fold(0.0, f32::max)
        .ceil()
}

/// The headline numbers: four tiles in a row, two by two when narrow.
fn tiles(ui: &mut egui::Ui, r: &StatsReport) {
    let talk = r.me.talk_ms + r.others.talk_ms;
    let share = if talk > 0 {
        format!("{}%", percent(r.me.talk_ms, talk))
    } else {
        "\u{2014}".into()
    };
    let items = [
        (fmt_thousands(r.meetings as i64), "Meetings"),
        (fmt_duration(Some(r.recorded_ms)), "Recorded"),
        (
            fmt_thousands((r.me.words + r.others.words) as i64),
            "Words transcribed",
        ),
        (share, "Your share of talk"),
    ];
    let width = ui.available_width();
    let cols = if width >= 560.0 { 4 } else { 2 };
    let tile_w = (width - S2 * (cols - 1) as f32) / cols as f32;
    let value_font = theme::semibold(20.0);
    let label_font = theme::regular(12.0);
    let (value_h, label_h) =
        ui.fonts_mut(|f| (f.row_height(&value_font), f.row_height(&label_font)));
    let tile_h = S3 + value_h + 2.0 + label_h + S3;
    for (row, chunk) in items.chunks(cols).enumerate() {
        if row > 0 {
            ui.add_space(S2 - ui.spacing().item_spacing.y);
        }
        let (line, _) = ui.allocate_exact_size(egui::vec2(width, tile_h), Sense::hover());
        for (col, (value, label)) in chunk.iter().enumerate() {
            let rect = Rect::from_min_size(
                line.min + egui::vec2(col as f32 * (tile_w + S2), 0.0),
                egui::vec2(tile_w, tile_h),
            );
            let response = ui.interact(rect, ui.id().with(("stat-tile", *label)), Sense::hover());
            let inner = tile_w - 2.0 * S3;
            let value_g = truncated(ui, value, value_font.clone(), pal().text, inner);
            let label_g = truncated(ui, label, label_font.clone(), pal().text_muted, inner);
            let painter = ui.painter();
            painter.rect(
                rect,
                CornerRadius::same(theme::R_LG),
                pal().bg_raised,
                Stroke::new(1.0, pal().border),
                egui::StrokeKind::Inside,
            );
            let x = rect.left() + S3;
            painter.galley(egui::pos2(x, rect.top() + S3), value_g, pal().text);
            painter.galley(
                egui::pos2(x, rect.top() + S3 + value_h + 2.0),
                label_g,
                pal().text_muted,
            );
            let name = format!("{label}: {value}");
            response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &name));
        }
    }
}

/// Up to [`TOP_PEOPLE`] people by time spent in meetings together; a row
/// opens the person in People.
fn people_chart(ui: &mut egui::Ui, r: &StatsReport, actions: &mut Vec<Action>) {
    if r.people.is_empty() {
        faint_note(
            ui,
            "Rename a speaker in a transcript to a person to see who you meet most.",
        );
        return;
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    let top = &r.people[..r.people.len().min(TOP_PEOPLE)];
    let max = top.iter().map(|p| p.together_ms).max().unwrap_or(0).max(1);
    let width = ui.available_width();
    let name_font = theme::regular(13.5);
    let value_font = theme::regular(12.5);
    let values: Vec<String> = top
        .iter()
        .map(|p| fmt_duration(Some(p.together_ms)))
        .collect();
    let value_w = widest(ui, values.iter().cloned(), &value_font);
    let name_w = (width * 0.3).clamp(72.0, 160.0);
    let bar_left = name_w + S3;
    let bar_room = (width - bar_left - S3 - value_w).max(0.0);
    for (p, value) in top.iter().zip(&values) {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::click());
        let hot = response.hovered() || response.has_focus();
        if hot {
            ui.painter().rect_filled(
                rect.expand2(egui::vec2(S1, 0.0)),
                CornerRadius::same(theme::R_SM),
                pal().bg_hover,
            );
        }
        let mid = rect.center().y;
        let name_g = truncated(ui, &p.name, name_font.clone(), pal().text, name_w);
        ui.painter().galley(
            egui::pos2(rect.left(), mid - name_g.size().y / 2.0),
            name_g,
            pal().text,
        );
        let color = if hot {
            pal().accent_hover
        } else {
            pal().accent
        };
        let fraction = p.together_ms as f32 / max as f32;
        paint_bar(
            ui,
            rect.left() + bar_left,
            mid,
            bar_room * fraction,
            BAR,
            color,
        );
        ui.painter().text(
            egui::pos2(rect.right(), mid),
            Align2::RIGHT_CENTER,
            value,
            value_font.clone(),
            pal().text_muted,
        );
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            paint_focus_ring(ui, rect, theme::R_SM);
        }
        let s = if p.meetings == 1 { "" } else { "s" };
        let tip = format!(
            "{}: {} meeting{s}, {} together; they spoke for {}",
            p.name,
            p.meetings,
            fmt_duration(Some(p.together_ms)),
            fmt_duration(Some(p.talk_ms))
        );
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &tip));
        if response.on_hover_text(&tip).clicked() {
            actions.push(Action::OpenPerson(p.person_id));
        }
    }
    if r.people.len() > TOP_PEOPLE {
        ui.add_space(S2);
        faint_note(
            ui,
            &format!("+ {} more in People", r.people.len() - TOP_PEOPLE),
        );
    }
}

/// A legend chip: a colour dot and its label.
fn legend(ui: &mut egui::Ui, items: &[(Color32, String)]) {
    let font = theme::regular(12.0);
    let height = ui.fonts_mut(|f| f.row_height(&font));
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::hover());
    let mut x = rect.left();
    for (color, label) in items {
        let painter = ui.painter();
        painter.circle_filled(egui::pos2(x + 4.0, rect.center().y), 4.0, *color);
        let galley = painter.layout_no_wrap(label.clone(), font.clone(), pal().text_muted);
        let w = galley.size().x;
        painter.galley(
            egui::pos2(x + 8.0 + S1 + 2.0, rect.center().y - galley.size().y / 2.0),
            galley,
            pal().text_muted,
        );
        x += 8.0 + S1 + 2.0 + w + S4;
    }
}

/// Who did the talking: the split bar, then pace, questions and words.
fn balance(ui: &mut egui::Ui, r: &StatsReport) {
    let total = r.me.talk_ms + r.others.talk_ms;
    if total == 0 {
        faint_note(ui, "No speech transcribed yet.");
        return;
    }
    let mine = percent(r.me.talk_ms, total);
    legend(
        ui,
        &[
            (
                pal().accent,
                format!("You {mine}% \u{b7} {}", fmt_duration(Some(r.me.talk_ms))),
            ),
            (
                others_color(),
                format!(
                    "Others {}% \u{b7} {}",
                    100 - mine,
                    fmt_duration(Some(r.others.talk_ms))
                ),
            ),
        ],
    );
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 12.0), Sense::hover());
    let split = rect.width() * r.me.talk_ms as f32 / total as f32;
    let painter = ui.painter();
    let gap = if split > 0.0 && split < rect.width() {
        1.0
    } else {
        0.0
    };
    if split > 0.0 {
        let left = Rect::from_min_max(
            rect.min,
            egui::pos2(rect.left() + split - gap, rect.bottom()),
        );
        painter.rect_filled(left, CornerRadius::same(4), pal().accent);
    }
    if split < rect.width() {
        let right = Rect::from_min_max(egui::pos2(rect.left() + split + gap, rect.top()), rect.max);
        painter.rect_filled(right, CornerRadius::same(4), others_color());
    }
    let name = format!("You spoke {mine}% of the time, others {}%", 100 - mine);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &name));
    ui.add_space(S1);
    stats_grid(
        ui,
        &[
            ("Your pace", pace_text(&r.me)),
            ("Others' pace", pace_text(&r.others)),
            ("Questions you asked", fmt_thousands(r.me.questions as i64)),
            (
                "Questions others asked",
                fmt_thousands(r.others.questions as i64),
            ),
            ("Words you said", fmt_thousands(r.me.words as i64)),
            ("Words others said", fmt_thousands(r.others.words as i64)),
        ],
    );
}

/// Each filler phrase: your count over theirs as two thin bars.
fn fillers(ui: &mut egui::Ui, r: &StatsReport) {
    let rows = filler_rows(&r.fillers);
    if rows.is_empty() {
        faint_note(ui, "No filler words yet. Impressive.");
        return;
    }
    legend(
        ui,
        &[
            (pal().accent, "You".to_owned()),
            (others_color(), "Others".to_owned()),
        ],
    );
    ui.spacing_mut().item_spacing.y = 0.0;
    let width = ui.available_width();
    let label_font = theme::regular(13.5);
    let value_font = theme::regular(12.5);
    let labels: Vec<String> = rows
        .iter()
        .map(|f| format!("\u{201c}{}\u{201d}", f.phrase))
        .collect();
    let values: Vec<String> = rows
        .iter()
        .map(|f| {
            format!(
                "{} \u{b7} {}",
                fmt_thousands(f.me as i64),
                fmt_thousands(f.others as i64)
            )
        })
        .collect();
    let label_w = widest(ui, labels.iter().cloned(), &label_font);
    let value_w = widest(ui, values.iter().cloned(), &value_font);
    let bar_left = label_w + S3;
    let bar_room = (width - bar_left - S3 - value_w).max(0.0);
    let max = rows
        .iter()
        .map(|f| f.me.max(f.others))
        .max()
        .unwrap_or(0)
        .max(1) as f32;
    for ((f, label), value) in rows.iter().zip(&labels).zip(&values) {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::hover());
        let mid = rect.center().y;
        let painter = ui.painter();
        painter.text(
            egui::pos2(rect.left(), mid),
            Align2::LEFT_CENTER,
            label,
            label_font.clone(),
            pal().text,
        );
        let left = rect.left() + bar_left;
        paint_bar(
            ui,
            left,
            mid - 4.0,
            bar_room * f.me as f32 / max,
            6.0,
            pal().accent,
        );
        paint_bar(
            ui,
            left,
            mid + 4.0,
            bar_room * f.others as f32 / max,
            6.0,
            others_color(),
        );
        ui.painter().text(
            egui::pos2(rect.right(), mid),
            Align2::RIGHT_CENTER,
            value,
            value_font.clone(),
            pal().text_muted,
        );
        let tip = format!(
            "You said {label} {} ({} per 1,000 words); others {} ({} per 1,000 words)",
            times(f.me),
            per_thousand(f.me, r.me.words),
            times(f.others),
            per_thousand(f.others, r.others.words),
        );
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &tip));
        response.on_hover_text(tip);
    }
}

/// A word cloud of `words` (laid out once per width into `cache`).
fn word_cloud(
    ui: &mut egui::Ui,
    id: &str,
    words: &[WordCount],
    cache: &mut Option<Cloud>,
    empty: &str,
) {
    if words.is_empty() {
        faint_note(ui, empty);
        return;
    }
    let width = ui.available_width();
    let height = if width < 420.0 {
        CLOUD_H_NARROW
    } else {
        CLOUD_H
    };
    if cache.as_ref().is_none_or(|c| (c.width - width).abs() > 0.5) {
        let painter = ui.painter().clone();
        let measure = |text: &str, size: f32| {
            painter
                .layout_no_wrap(text.to_owned(), theme::semibold(size), pal().text)
                .size()
        };
        *cache = Some(Cloud {
            width,
            words: layout_cloud(words, egui::vec2(width, height), measure),
        });
    }
    let cloud = cache.as_ref().expect("laid out");
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
    let hovered = response.hover_pos().and_then(|pos| {
        cloud
            .words
            .iter()
            .find(|p| p.rect.translate(rect.min.to_vec2()).contains(pos))
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        for p in &cloud.words {
            let color = if hovered == Some(p) {
                pal().accent_hover
            } else if p.index < 3 {
                pal().accent
            } else if p.index < 15 {
                pal().text
            } else {
                pal().text_muted
            };
            painter.text(
                rect.min + p.rect.center().to_vec2(),
                Align2::CENTER_CENTER,
                &words[p.index].word,
                theme::semibold(p.size),
                color,
            );
        }
    }
    let summary = format!(
        "Most used words ({id}): {}",
        words
            .iter()
            .take(10)
            .map(|w| format!("{} ({})", w.word, w.count))
            .collect::<Vec<_>>()
            .join(", ")
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &summary));
    if let Some(p) = hovered {
        let w = &words[p.index];
        response.on_hover_text_at_pointer(format!(
            "\u{201c}{}\u{201d}: {}",
            w.word,
            times(w.count)
        ));
    }
}

/// When meetings start: a 7 × 24 grid (weekday × local hour), shaded by
/// count, with the busiest day and hour as its caption.
fn week_grid(ui: &mut egui::Ui, grid: &[[u32; 24]; 7]) {
    if let Some(text) = busiest_text(grid) {
        ui.add(
            egui::Label::new(
                egui::RichText::new(text)
                    .font(theme::regular(12.5))
                    .color(pal().text_muted),
            )
            .wrap(),
        );
    }
    let axis = theme::regular(11.0);
    let label_w = widest(ui, DAYS.iter().map(|d| d.to_string()), &axis) + S2;
    let width = ui.available_width();
    let gap = 2.0;
    let cell_w = ((width - label_w - gap * 23.0) / 24.0).max(2.0);
    let cell_h = cell_w.clamp(10.0, 16.0);
    let height = 7.0 * cell_h + 6.0 * gap;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
    let max = grid.iter().flatten().copied().max().unwrap_or(0).max(1) as f32;
    let cell = |day: usize, hour: usize| {
        Rect::from_min_size(
            egui::pos2(
                rect.left() + label_w + hour as f32 * (cell_w + gap),
                rect.top() + day as f32 * (cell_h + gap),
            ),
            egui::vec2(cell_w, cell_h),
        )
    };
    let hovered = response.hover_pos().and_then(|pos| {
        (0..7)
            .flat_map(|d| (0..24).map(move |h| (d, h)))
            .find(|&(d, h)| cell(d, h).expand(gap / 2.0).contains(pos))
    });
    let painter = ui.painter();
    for (day, row) in grid.iter().enumerate() {
        painter.text(
            egui::pos2(rect.left(), cell(day, 0).center().y),
            Align2::LEFT_CENTER,
            DAYS[day],
            axis.clone(),
            pal().text_faint,
        );
        for (hour, &count) in row.iter().enumerate() {
            let color = if count == 0 {
                pal().bg_hover
            } else {
                theme::alpha(pal().accent, 0.25 + 0.75 * count as f32 / max)
            };
            let r = cell(day, hour);
            painter.rect_filled(r, CornerRadius::same(2), color);
            if hovered == Some((day, hour)) {
                painter.rect_stroke(
                    r,
                    CornerRadius::same(2),
                    Stroke::new(1.5, pal().text),
                    egui::StrokeKind::Outside,
                );
            }
        }
    }
    // Hour axis: every sixth hour under its column.
    let axis_h = ui.fonts_mut(|f| f.row_height(&axis));
    let (axis_rect, _) = ui.allocate_exact_size(egui::vec2(width, axis_h), Sense::hover());
    for hour in [0, 6, 12, 18] {
        ui.painter().text(
            egui::pos2(cell(0, hour).left(), axis_rect.center().y),
            Align2::LEFT_CENTER,
            format!("{hour:02}"),
            axis.clone(),
            pal().text_faint,
        );
    }
    let summary = busiest_text(grid).unwrap_or_else(|| "No meetings yet.".into());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &summary));
    if let Some((day, hour)) = hovered {
        let n = grid[day][hour];
        let s = if n == 1 { "" } else { "s" };
        response.on_hover_text_at_pointer(format!(
            "{} {hour:02}:00\u{2013}{:02}:00: {n} meeting{s} started",
            DAYS[day],
            (hour + 1) % 24
        ));
    }
}

/// The records: the longest meeting and the longest monologue, each a row
/// that opens the transcript.
fn records(ui: &mut egui::Ui, r: &StatsReport, actions: &mut Vec<Action>) {
    let mut rows = Vec::new();
    if let Some(m) = &r.longest_meeting {
        rows.push(("Longest meeting".to_owned(), m));
    }
    if let Some(m) = &r.longest_monologue {
        let who = m.speaker.as_deref().unwrap_or("Someone");
        rows.push((format!("Longest monologue \u{b7} {who}"), m));
    }
    if rows.is_empty() {
        return;
    }
    widgets::section_header(ui, "Records");
    ui.spacing_mut().item_spacing.y = 2.0;
    for (kind, record) in rows {
        let meta = format!("{kind} \u{b7} {}", fmt_duration(Some(record.ms)));
        let row = record_row(ui, &kind, &record.title, &meta);
        if row.clicked() {
            actions.push(Action::OpenMeeting(record.meeting_id));
        }
    }
}

fn record_row(ui: &mut egui::Ui, kind: &str, detail: &str, meta: &str) -> Response {
    let (row, ()) = widgets::list_row(ui, ("stats-record", kind), false, |ui, _| {
        ui.spacing_mut().item_spacing.y = 1.0;
        widgets::elide(ui, detail, theme::regular(14.0), pal().text);
        widgets::elide(ui, meta, theme::regular(12.0), pal().text_muted);
    });
    let name = format!("{meta}: {detail}");
    row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use witness_core::stats::{PersonTotal, Record, FILLERS};

    fn word(w: &str, count: u64) -> WordCount {
        WordCount {
            word: w.into(),
            count,
        }
    }

    #[test]
    fn clouds_place_words_inside_without_overlap() {
        // Long-tailed, as real word counts are.
        let words: Vec<WordCount> = (0..60)
            .map(|i| word(&format!("word{i}"), 600 / (i + 1)))
            .collect();
        for area in [
            egui::vec2(700.0, CLOUD_H),
            egui::vec2(340.0, CLOUD_H_NARROW),
        ] {
            // A stand-in font: 0.6 em per character.
            let placed = layout_cloud(&words, area, |text, size| {
                egui::vec2(text.len() as f32 * size * 0.6, size * 1.2)
            });
            assert_eq!(placed[0].index, 0, "the top word always fits");
            assert!(placed.len() >= 40, "{} placed in {area:?}", placed.len());
            let bounds = Rect::from_min_size(egui::Pos2::ZERO, area);
            for (i, a) in placed.iter().enumerate() {
                assert!(bounds.contains_rect(a.rect));
                assert!(placed[i + 1..].iter().all(|b| !a.rect.intersects(b.rect)));
            }
            assert!(placed[0].size > placed.last().unwrap().size);
        }
        assert!(layout_cloud(&[], egui::vec2(100.0, 100.0), |_, _| egui::Vec2::ZERO).is_empty());
    }

    #[test]
    fn busiest_slot_and_filler_order() {
        let mut grid = [[0u32; 24]; 7];
        assert_eq!(busiest_text(&grid), None);
        grid[1][14] = 3;
        grid[4][9] = 2;
        grid[4][14] = 0;
        assert_eq!(
            busiest_text(&grid).as_deref(),
            Some("Most meetings start on Tuesdays and around 14:00.")
        );
        let fillers = [
            FillerCount {
                phrase: "so",
                me: 1,
                others: 9,
            },
            FillerCount {
                phrase: "um",
                me: 0,
                others: 0,
            },
            FillerCount {
                phrase: "like",
                me: 5,
                others: 1,
            },
        ];
        let rows: Vec<_> = filler_rows(&fillers).iter().map(|f| f.phrase).collect();
        assert_eq!(rows, ["like", "so"]);
        assert_eq!(per_thousand(3, 2_000), "1.5");
        assert_eq!(times(1), "1 time");
        assert_eq!(times(1_200), "1,200 times");
    }

    fn sample() -> StatsReport {
        let mut week_hours = [[0; 24]; 7];
        week_hours[2][10] = 4;
        StatsReport {
            meetings: 12,
            recorded_ms: 36_000_000,
            me: Voice {
                words: 12_000,
                talk_ms: 4_000_000,
                questions: 40,
            },
            others: Voice {
                words: 30_000,
                talk_ms: 9_000_000,
                questions: 70,
            },
            people: (0..10)
                .map(|i| PersonTotal {
                    person_id: i,
                    name: format!("Person with a rather long name number {i}"),
                    meetings: 3,
                    together_ms: 10_000_000 - i * 900_000,
                    talk_ms: 1_000_000,
                })
                .collect(),
            fillers: FILLERS
                .iter()
                .enumerate()
                .map(|(i, &phrase)| FillerCount {
                    phrase,
                    me: i as u64 * 30,
                    others: 1_234,
                })
                .collect(),
            my_words: (0..60)
                .map(|i| word(&format!("roadmap{i}"), 200 - i))
                .collect(),
            their_words: vec![word("budget", 3)],
            longest_meeting: Some(Record {
                meeting_id: 1,
                title: "A very long meeting title that has to elide somewhere".into(),
                speaker: None,
                ms: 5_400_000,
            }),
            longest_monologue: Some(Record {
                meeting_id: 2,
                title: "Planning".into(),
                speaker: Some("You".into()),
                ms: 300_000,
            }),
            week_hours,
        }
    }

    /// Every section stays inside the window at the narrowest and a wide width.
    #[test]
    fn the_page_fits_at_every_width() {
        let ctx = theme::test_context();
        for width in [380.0, 460.0, 640.0, 1000.0] {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 4000.0),
                )),
                ..Default::default()
            };
            let mut view = StatsView::new();
            view.report = Some(Box::new(sample()));
            let mut actions = Vec::new();
            let mut output = ctx.run_ui(input, |ui| view.draw(ui, &mut actions));
            output.textures_delta.clear();
            assert!(actions.is_empty());
            for clipped in &output.shapes {
                let rect = clipped.shape.visual_bounding_rect();
                if rect.is_positive() {
                    assert!(rect.right() <= width + 0.5, "{rect:?} at {width}");
                    assert!(rect.left() >= -0.5, "{rect:?} at {width}");
                }
            }
            assert!(view.clouds.iter().all(Option::is_some), "clouds laid out");
        }
    }
}
