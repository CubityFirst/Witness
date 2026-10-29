//! The transcript's player bar (map-ui-transcript §10.4): a raised surface
//! docked at the bottom of the view, in the page's padded column. Row 1 is
//! the scrubber between the current and the total time; row 2 is the
//! transport on the left (±10 s around a round play/pause, add bookmark)
//! and speed, volume and "Save a copy…" on the right. When the bar is too
//! narrow for the volume slider it collapses to an icon button with a
//! popover slider, so nothing ever wraps.
//!
//! The scrubber's track is a speaker timeline: who spoke when, in speaker
//! colours (full strength where already played, faded ahead of the thumb),
//! silence neutral, bookmark markers on top. Thousands of segments are
//! reduced to one dominant speaker per point of track width and merged
//! into runs, cached per width ([`timeline_runs`]); hovering the track
//! names the time and the speaker.
//!
//! The bar only drives `witness_core::player::Player` (every call is cheap
//! and non-blocking); it never touches the database. Scrubbing previews the
//! dragged position and seeks at most every 100 ms, then once more on
//! release, so a drag does not flood the decoder with seeks. Volume and
//! speed persist in `UiPrefs` (they replaced the `witness-volume` /
//! `witness-speed` localStorage keys).

use crate::bridge::OpDone;
use crate::platform::dialogs::{self, SaveFileRequest};
use crate::ui::format::fmt_ts;
use crate::ui::icons::ph;
use crate::ui::theme::{self, pal};
use crate::ui::toasts::Notice;
use crate::ui::widgets;
use crate::ui::UiCtx;
use eframe::egui::{
    self, Align, CornerRadius, FontId, Frame, Key, Layout, Margin, Popup, PopupCloseBehavior, Rect,
    RectAlign, Response, Sense, Stroke,
};
use std::time::{Duration, Instant};
use witness_core::player::PlayerSnapshot;
use witness_core::service::MeetingDetail;

/// The speed menu (HTML `playbackRate` options).
pub const SPEEDS: [f32; 6] = [0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// While scrubbing, seek at most this often (the release always seeks).
const SCRUB_SEEK_INTERVAL: Duration = Duration::from_millis(100);
/// The ±10 s buttons.
const SKIP_BUTTON_MS: i64 = 10_000;
/// ←/→ on the focused scrubber (same step as the view's global keys).
const SKIP_KEY_MS: i64 = 5_000;
/// Scrubber row height: the 24 pt marker hit targets fit inside it.
const SCRUB_HEIGHT: f32 = 24.0;
/// Seek-track thickness and thumb radius (resting / hovered or held).
const TRACK_HEIGHT: f32 = 4.0;
const THUMB_RADIUS: f32 = 7.0;
const THUMB_RADIUS_REST: f32 = 6.0;
/// The speaker-timeline strip under the track, and the gap above it.
const TIMELINE_HEIGHT: f32 = 3.0;
const TIMELINE_GAP: f32 = 4.0;
/// Speaker colour strength on the strip (quiet beside the accent track).
const TIMELINE_ALPHA: f32 = 0.75;
/// A timeline column shows its dominant speaker once speech covers this
/// share of it; below that it is silence.
const COLUMN_SPEECH_MIN: f64 = 0.3;
/// Runs narrower than this many columns are smoothed away when they are
/// a pause between two lines or an interjection inside someone else's
/// turn: one-point slivers only made the track look noisy.
const MIN_RUN: usize = 3;
/// Bookmark marker hit target (centred on the point) and dot radius; the
/// dot sits in a ring of the surface colour so it reads on the track.
const MARKER_HIT: f32 = 24.0;
const MARKER_RADIUS: f32 = 3.0;
/// Transport buttons (ghost squares) and the round play/pause.
const BTN_SIZE: f32 = 32.0;
const PLAY_SIZE: f32 = 36.0;
/// Gap between the controls of one cluster.
const CONTROL_GAP: f32 = 4.0;
/// Narrower than this (the bar's content width) the volume slider
/// collapses into a popover: both clusters with the slider need ~330 pt.
const FULL_CONTROLS_MIN: f32 = 360.0;
/// The inline volume slider and the popover's.
const VOLUME_WIDTH: f32 = 80.0;
const VOLUME_POPOVER_WIDTH: f32 = 140.0;

/// Per-transcript player bar state (scrub preview, throttling).
#[derive(Debug, Default)]
pub struct PlayerBarState {
    /// While the scrubber is held: the previewed position.
    scrub_ms: Option<u64>,
    /// When the last throttled scrub seek went out.
    last_scrub_seek: Option<Instant>,
    /// Open the compact volume popover on the next pass (debug snapshots).
    open_volume_once: bool,
    /// The timeline's runs for (timeline version, columns, duration).
    timeline_cache: Option<((u64, usize, u64), Vec<Run>)>,
}

impl PlayerBarState {
    /// A fresh state; `open_volume` opens the compact volume popover on the
    /// first pass (debug snapshots).
    pub fn with_volume_open(open_volume: bool) -> Self {
        PlayerBarState {
            open_volume_once: open_volume,
            ..PlayerBarState::default()
        }
    }
}

/// One speaker on the scrubber's timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineSpeaker {
    /// The name the hover tooltip shows.
    pub name: String,
    /// The immutable label: picks the colour (`theme::speaker_text`).
    pub label: String,
    /// Filtered out of the transcript: drawn like silence, so the speakers
    /// being shown stand out.
    pub dimmed: bool,
}

/// One segment on the timeline: start and end (ms) and its speaker (an
/// index into [`Timeline::speakers`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineSpan {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker: u16,
}

/// Who spoke when, for the scrubber. The transcript rebuilds it when its
/// data, a name or the speaker filter changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timeline {
    /// Bumped on every rebuild: the bar's cache key.
    pub version: u64,
    pub speakers: Vec<TimelineSpeaker>,
    /// Ordered by start.
    pub spans: Vec<TimelineSpan>,
}

impl Timeline {
    /// Who speaks at `ms`: the last segment started by then, if it has not
    /// ended (gaps between segments are silence).
    fn speaker_at(&self, ms: i64) -> Option<&TimelineSpeaker> {
        let i = self
            .spans
            .partition_point(|s| s.start_ms <= ms)
            .checked_sub(1)?;
        let span = self.spans[i];
        (ms < span.end_ms)
            .then(|| self.speakers.get(usize::from(span.speaker)))
            .flatten()
    }
}

/// A stretch of track columns with one dominant speaker (`None`: silence,
/// or only filtered-out speakers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    from: usize,
    to: usize,
    who: Option<u16>,
}

/// The timeline reduced to `columns` columns (one per point of track
/// width) over `duration_ms`: each column takes the speaker who covers most
/// of it — or silence when speech covers less than `COLUMN_SPEECH_MIN` of
/// it — and neighbouring columns with the same result merge into one run
/// ([`smooth_runs`] then bridges short pauses and interjections).
/// O(segments + columns), so a multi-hour meeting paints a few hundred
/// rectangles at most.
fn timeline_runs(timeline: &Timeline, duration_ms: u64, columns: usize) -> Vec<Run> {
    let n = timeline.speakers.len();
    if columns == 0 || duration_ms == 0 || n == 0 {
        return Vec::new();
    }
    let duration = duration_ms as f64;
    let col_ms = duration / columns as f64;
    // Milliseconds of speech per (column, speaker).
    let mut cover = vec![0f64; columns * n];
    for span in &timeline.spans {
        let who = usize::from(span.speaker);
        if timeline.speakers.get(who).is_none_or(|s| s.dimmed) {
            continue;
        }
        let start = span.start_ms.max(0) as f64;
        let end = (span.end_ms as f64).min(duration);
        if end <= start {
            continue;
        }
        let first = ((start / col_ms) as usize).min(columns - 1);
        let last = ((end / col_ms).ceil() as usize).clamp(first + 1, columns);
        for column in first..last {
            let lo = column as f64 * col_ms;
            let overlap = end.min(lo + col_ms) - start.max(lo);
            if overlap > 0.0 {
                cover[column * n + who] += overlap;
            }
        }
    }
    let mut runs: Vec<Run> = Vec::new();
    for (column, row) in cover.chunks_exact(n).enumerate() {
        let speech: f64 = row.iter().sum();
        let who = (speech >= COLUMN_SPEECH_MIN * col_ms)
            .then(|| {
                row.iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .and_then(|(i, _)| u16::try_from(i).ok())
            })
            .flatten();
        match runs.last_mut() {
            Some(run) if run.who == who => run.to = column + 1,
            _ => runs.push(Run {
                from: column,
                to: column + 1,
                who,
            }),
        }
    }
    smooth_runs(&runs)
}

/// Bridge runs narrower than `MIN_RUN` columns into the run before them
/// when they are a pause followed by more speech, or another speaker's
/// interjection inside one speaker's turn. A short line alone in silence
/// stays: it is the only trace of that speaker there.
fn smooth_runs(runs: &[Run]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::with_capacity(runs.len());
    for (i, run) in runs.iter().enumerate() {
        let next = runs.get(i + 1).map(|r| r.who);
        if let Some(prev) = out.last_mut() {
            let short = run.to - run.from < MIN_RUN;
            let pause = run.who.is_none() && next.is_some_and(|w| w.is_some());
            let interjection = run.who.is_some() && next == Some(prev.who);
            let bridge = short && prev.who.is_some() && (pause || interjection);
            if bridge || prev.who == run.who {
                prev.to = run.to;
                continue;
            }
        }
        out.push(*run);
    }
    out
}

/// What the bar asks the transcript view to do.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlayerBarOutput {
    /// "Add bookmark" at this position.
    pub add_bookmark_at_ms: Option<i64>,
    /// Play/Pause was pressed (a play failure toasts "Could not play
    /// recording").
    pub toggled: bool,
}

/// The nearest offered speed (a hand-edited `ui-state.toml` may hold any
/// value in 0.75–2; NaN falls back to 1×).
pub fn snap_speed(speed: f32) -> f32 {
    if !speed.is_finite() {
        return 1.0;
    }
    let mut best = 1.0;
    let mut best_distance = f32::INFINITY;
    for candidate in SPEEDS {
        let distance = (candidate - speed).abs();
        if distance < best_distance {
            best = candidate;
            best_distance = distance;
        }
    }
    best
}

/// "1×", "1.25×" … (the `<option>` labels).
fn speed_label(speed: f32) -> String {
    format!("{speed}×")
}

/// `fmtClock`: `m:ss`, or `h:mm:ss` from an hour.
fn clock(ms: u64) -> String {
    fmt_ts(i64::try_from(ms).unwrap_or(i64::MAX))
}

/// `text`'s width in `font` with every digit measured as `0` (as the top
/// bar measures its timer), so a time label keeps one width while its
/// digits change.
pub(crate) fn stable_width(ui: &egui::Ui, text: &str, font: FontId) -> f32 {
    let template: String = text
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect();
    ui.painter()
        .layout_no_wrap(template, font, pal().text)
        .size()
        .x
        .ceil()
}

/// The docked panel around the bar: the page background, so rows never
/// show through, with breathing room above and below the surface.
pub fn bar_frame() -> Frame {
    Frame::new().fill(pal().bg).inner_margin(Margin {
        left: 0,
        right: 0,
        top: 8,
        bottom: 12,
    })
}

/// The page's padded, centred column, less `gutter` on the right: the
/// transcript's scroll bar takes that much width from its content, and the
/// bar lines up with the text above it rather than with the window edge.
pub fn bar_column<R>(ui: &mut egui::Ui, gutter: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = (ui.available_width() - gutter).max(0.0);
    let pad = widgets::pad_x(width);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(pad);
        ui.vertical(|ui| {
            ui.set_width((width - 2.0 * pad).max(0.0));
            widgets::content_column(ui, add)
        })
        .inner
    })
    .inner
}

/// Draw the bar for `detail`'s archived audio, with `timeline` on the
/// scrubber. `gutter`: see [`bar_column`].
pub fn show(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    detail: &MeetingDetail,
    timeline: &Timeline,
    st: &mut PlayerBarState,
    gutter: f32,
) -> PlayerBarOutput {
    let mut out = PlayerBarOutput::default();
    let snap = cx.player.snapshot();
    sync_preferences(cx, &snap);

    bar_column(ui, gutter, |ui| {
        Frame::new()
            .fill(pal().bg_raised)
            .stroke(Stroke::new(1.0, pal().border))
            .corner_radius(CornerRadius::same(theme::R_LG))
            .inner_margin(Margin {
                left: 12,
                right: 12,
                top: 6,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 2.0);
                time_row(ui, cx, detail, timeline, st, &snap);

                let mut volume = cx.prefs.volume.clamp(0.0, 1.0);
                let mut speed = snap_speed(cx.prefs.speed);
                let open_volume = std::mem::take(&mut st.open_volume_once);
                let (controls, _) =
                    controls_row(ui, snap.playing, &mut volume, &mut speed, open_volume);
                apply_controls(cx, detail, &snap, controls, volume, speed, &mut out);
            });
    });
    out
}

/// Keep the engine on the persisted volume and speed (re-applied after a
/// load, like the TSX effect keyed on `src`).
fn sync_preferences(cx: &mut UiCtx, snap: &PlayerSnapshot) {
    let speed = snap_speed(cx.prefs.speed);
    if (snap.speed - speed).abs() > 0.001 {
        cx.player.set_speed(speed);
    }
    let volume = cx.prefs.volume.clamp(0.0, 1.0);
    if (snap.volume - volume).abs() > 0.001 {
        cx.player.set_volume(volume);
    }
}

/// Paint a 12 pt muted time label in a `min_width` (or wider) ×
/// `SCRUB_HEIGHT` box, right- or left-aligned, and expose it to screen
/// readers.
fn time_label(ui: &mut egui::Ui, text: String, min_width: f32, right_align: bool) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.clone(), theme::regular(12.0), pal().text_muted);
    let width = galley.size().x.max(min_width);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, SCRUB_HEIGHT), Sense::hover());
    let x = if right_align {
        rect.right() - galley.size().x
    } else {
        rect.left()
    };
    let pos = egui::pos2(x, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley, pal().text_muted);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
}

/// Row 1: current time, scrubber + markers, total time. Both times are
/// as wide as the total measured with `0` digits ([`stable_width`]), so
/// the scrubber never shifts while the current time counts.
fn time_row(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    detail: &MeetingDetail,
    timeline: &Timeline,
    st: &mut PlayerBarState,
    snap: &PlayerSnapshot,
) {
    let current = st
        .scrub_ms
        .unwrap_or(snap.position_ms)
        .min(snap.duration_ms);
    let total = clock(snap.duration_ms);
    let time_width = stable_width(ui, &total, theme::regular(12.0));
    ui.horizontal(|ui| {
        time_label(ui, clock(current), time_width, true);
        let scrub_width =
            (ui.available_width() - time_width - ui.spacing().item_spacing.x).max(60.0);
        scrubber(ui, cx, detail, timeline, st, snap, scrub_width);
        time_label(ui, total, time_width, false);
    });
}

/// The seek slider (`<input type=range step=0.1>`) over the speaker
/// timeline, plus the bookmark markers on top of it. Markers are
/// registered after the track, so they win the click where they overlap
/// it; the thumb is painted last.
fn scrubber(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    detail: &MeetingDetail,
    timeline: &Timeline,
    st: &mut PlayerBarState,
    snap: &PlayerSnapshot,
    width: f32,
) {
    let duration = snap.duration_ms;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, SCRUB_HEIGHT), Sense::click_and_drag());
    // The thumb's centre travels between these x positions.
    let track = egui::Rangef::new(rect.left() + THUMB_RADIUS, rect.right() - THUMB_RADIUS);
    let to_ms = |x: f32| -> u64 {
        if duration == 0 || track.span() <= 0.0 {
            return 0;
        }
        let t = ((x - track.min) / track.span()).clamp(0.0, 1.0);
        (f64::from(t) * duration as f64).round() as u64
    };
    let to_x = |ms: u64| -> f32 {
        if duration == 0 {
            return track.min;
        }
        let t = (ms as f64 / duration as f64).clamp(0.0, 1.0) as f32;
        track.min + t * track.span()
    };

    // Pointer: preview while held, throttled seeks, a final seek on release.
    let now = Instant::now();
    let held = response.is_pointer_button_down_on();
    if duration > 0 {
        if held {
            if let Some(pos) = response.interact_pointer_pos() {
                let ms = to_ms(pos.x);
                st.scrub_ms = Some(ms);
                let due = st
                    .last_scrub_seek
                    .is_none_or(|at| now.duration_since(at) >= SCRUB_SEEK_INTERVAL);
                if due {
                    cx.player.seek_ms(ms);
                    st.last_scrub_seek = Some(now);
                }
            }
        }
        if response.drag_stopped() || response.clicked() {
            // A press and release in the same pass (a touchpad tap, a fast
            // click on an idle window) was never `held`: seek to where it
            // landed. A keyboard "click" has no pointer position.
            let landed = response.interact_pointer_pos().map(|pos| to_ms(pos.x));
            if let Some(ms) = st.scrub_ms.or(landed) {
                cx.player.seek_ms(ms);
            }
        }
    }
    if !held {
        st.scrub_ms = None;
        st.last_scrub_seek = None;
    }

    // Keyboard on the focused scrubber: ←/→ ±5 s, Home/End. Like
    // egui's Slider, it keeps the arrows: otherwise egui's arrow-key
    // navigation also moves focus off it, and the next press would land
    // on a button.
    if response.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    ..Default::default()
                },
            );
        });
    }
    if response.has_focus() && duration > 0 {
        let (left, right, home, end) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowLeft),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowRight),
                i.consume_key(egui::Modifiers::NONE, Key::Home),
                i.consume_key(egui::Modifiers::NONE, Key::End),
            )
        });
        if left || right {
            // The lock filter applies from the pass after focus arrived.
            ui.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
        }
        if left {
            cx.player.skip_ms(-SKIP_KEY_MS);
        }
        if right {
            cx.player.skip_ms(SKIP_KEY_MS);
        }
        if home {
            cx.player.seek_ms(0);
        }
        if end {
            cx.player.seek_ms(duration);
        }
    }

    let value = st.scrub_ms.unwrap_or(snap.position_ms).min(duration);
    response.widget_info(|| egui::WidgetInfo::slider(true, value as f64 / 1000.0, "Seek"));

    let visible = ui.is_rect_visible(rect);
    let cy = rect.center().y;
    let has_timeline = duration > 0 && !timeline.spans.is_empty();
    if visible {
        // The seek track: a plain rail, the played part in the accent.
        let painter = ui.painter();
        let bar = egui::Rect::from_x_y_ranges(
            track.min..=track.max,
            (cy - TRACK_HEIGHT / 2.0)..=(cy + TRACK_HEIGHT / 2.0),
        );
        painter.rect_filled(bar, CornerRadius::same(2), pal().border_strong);
        let filled = egui::Rect::from_x_y_ranges(bar.left()..=to_x(value), bar.y_range());
        painter.rect_filled(filled, CornerRadius::same(2), pal().accent);
    }
    if visible && has_timeline {
        // Who spoke when: its own thin strip under the track, so the seek
        // bar stays a clean progress bar.
        let columns = track.span().round().max(0.0) as usize;
        let key = (timeline.version, columns, duration);
        if st.timeline_cache.as_ref().is_none_or(|(k, _)| *k != key) {
            st.timeline_cache = Some((key, timeline_runs(timeline, duration, columns)));
        }
        let runs = st.timeline_cache.as_ref().map_or(&[][..], |(_, r)| r);
        let top = cy + TRACK_HEIGHT / 2.0 + TIMELINE_GAP;
        let strip =
            egui::Rect::from_x_y_ranges(track.min..=track.max, top..=(top + TIMELINE_HEIGHT));
        paint_timeline(ui.painter(), strip, runs, columns, timeline);
    }

    // Bookmark markers (only once the duration is known): 24×24
    // transparent targets around a warn dot.
    if duration > 0 {
        for bookmark in &detail.bookmarks {
            bookmark_marker(ui, cx, bookmark, egui::pos2(to_x(at_ms(bookmark)), cy));
        }
    }

    if visible {
        let hot = response.hovered() || held;
        let radius = if hot { THUMB_RADIUS } else { THUMB_RADIUS_REST };
        let painter = ui.painter();
        let thumb = egui::pos2(to_x(value), cy);
        painter.circle_filled(thumb, radius, pal().accent);
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(6),
                Stroke::new(2.0, pal().focus),
                egui::StrokeKind::Outside,
            );
        }
    }
    if duration > 0 {
        // The time under the pointer, and who speaks there.
        let hover = response
            .hover_pos()
            .filter(|_| response.hovered())
            .map(|pos| hover_text(timeline, to_ms(pos.x)));
        let response = response;
        if let Some(text) = hover {
            response.on_hover_text_at_pointer(text);
        }
    }
}

/// The scrubber's hover tooltip at `ms`: the time, and the speaker when
/// someone speaks there.
fn hover_text(timeline: &Timeline, ms: u64) -> String {
    let at = i64::try_from(ms).unwrap_or(i64::MAX);
    match timeline.speaker_at(at) {
        Some(speaker) => format!("{} · {}", clock(ms), speaker.name),
        None => clock(ms),
    }
}

/// Paint the timeline `runs` (over `columns` equal columns of `strip`):
/// each speaker's runs in their colour, silence (and filtered-out
/// speakers) left empty. Only a run's outer ends at the strip's two ends
/// are round.
fn paint_timeline(
    painter: &egui::Painter,
    strip: Rect,
    runs: &[Run],
    columns: usize,
    timeline: &Timeline,
) {
    if columns == 0 {
        return;
    }
    let column = strip.width() / columns as f32;
    let round = (strip.height() / 2.0).round() as u8;
    for run in runs {
        let Some(speaker) = run
            .who
            .and_then(|i| timeline.speakers.get(usize::from(i)))
            .filter(|s| !s.dimmed)
        else {
            continue;
        };
        let x0 = strip.left() + run.from as f32 * column;
        let x1 = strip.left() + run.to as f32 * column;
        if x1 - x0 <= 0.01 {
            continue;
        }
        let rect = Rect::from_x_y_ranges(x0..=x1, strip.y_range());
        let color = theme::alpha(theme::speaker_text(&speaker.label), TIMELINE_ALPHA);
        painter.rect_filled(rect, CornerRadius::same(round), color);
    }
}

fn at_ms(bookmark: &witness_core::db::Bookmark) -> u64 {
    u64::try_from(bookmark.at_ms).unwrap_or(0)
}

/// One bookmark marker on the scrubber: seeks (without playing) on click.
fn bookmark_marker(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    bookmark: &witness_core::db::Bookmark,
    center: egui::Pos2,
) {
    let at = at_ms(bookmark);
    let hit = egui::Rect::from_center_size(center, egui::vec2(MARKER_HIT, MARKER_HIT));
    let marker = ui.interact(
        hit,
        ui.id().with(("seek-marker", bookmark.id)),
        Sense::click(),
    );
    let time = clock(at);
    let (tooltip, label) = if bookmark.note.is_empty() {
        (time.clone(), format!("Go to bookmark at {time}"))
    } else {
        (
            format!("{time} · {}", bookmark.note),
            format!("Go to bookmark at {time}: {}", bookmark.note),
        )
    };
    marker.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    let painter = ui.painter();
    let radius = if marker.hovered() {
        MARKER_RADIUS + 1.0
    } else {
        MARKER_RADIUS
    };
    painter.circle_filled(center, radius + 1.5, pal().bg_raised);
    painter.circle_filled(center, radius, pal().warn);
    if marker.has_focus() && widgets::focus_visible(ui.ctx()) {
        painter.rect_stroke(
            hit,
            CornerRadius::same(6),
            Stroke::new(2.0, pal().focus),
            egui::StrokeKind::Outside,
        );
    }
    let marker = marker.on_hover_text(tooltip);
    if marker.clicked() {
        // Markers seek without playing.
        cx.player.seek_ms(at);
    }
}

/// What row 2's controls were asked to do this pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Controls {
    back: bool,
    toggle: bool,
    forward: bool,
    bookmark: bool,
    save: bool,
    volume_changed: bool,
    speed_changed: bool,
}

/// Where row 2's two clusters were laid out (for the layout tests).
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
struct ControlsLayout {
    left: Rect,
    right: Rect,
}

/// Row 2: the transport on the left, speed / volume / "Save a copy…" on
/// the right, one line at every width (the volume slider collapses into a
/// popover when the bar is narrow). Pure layout: the caller applies the
/// returned intents.
fn controls_row(
    ui: &mut egui::Ui,
    playing: bool,
    volume: &mut f32,
    speed: &mut f32,
    open_volume: bool,
) -> (Controls, ControlsLayout) {
    let compact = ui.available_width() < FULL_CONTROLS_MIN;
    let mut c = Controls::default();
    // Both clusters in a row exactly as tall as the play button, so the
    // shorter speed select and slider centre on the same line as it.
    let cluster = |ui: &egui::Ui| egui::vec2(ui.available_width(), PLAY_SIZE);
    let (left, right) = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = CONTROL_GAP;
            let left = ui
                .allocate_ui_with_layout(cluster(ui), Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = CONTROL_GAP;
                    transport(ui, playing, &mut c);
                })
                .response
                .rect;
            let right = ui
                .allocate_ui_with_layout(cluster(ui), Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = CONTROL_GAP;
                    c.save =
                        player_btn(ui, ph::DOWNLOAD_SIMPLE, 17.0, "Save a copy", "Save a copy…")
                            .clicked();
                    c.volume_changed = volume_control(ui, volume, compact, open_volume);
                    ui.add_space(CONTROL_GAP);
                    c.speed_changed = speed_select_centred(ui, speed);
                })
                .response
                .rect;
            (left, right)
        })
        .inner;
    (c, ControlsLayout { left, right })
}

/// Skip back, play/pause, skip forward, add bookmark.
fn transport(ui: &mut egui::Ui, playing: bool, c: &mut Controls) {
    c.back = player_btn(
        ui,
        ph::REWIND,
        16.0,
        "Back 10 seconds",
        "Back 10 s (←: 5 s)",
    )
    .clicked();
    let (label, tip) = if playing {
        ("Pause", "Pause (space)")
    } else {
        ("Play", "Play (space)")
    };
    c.toggle = play_btn(ui, playing, label, tip).clicked();
    c.forward = player_btn(
        ui,
        ph::FAST_FORWARD,
        16.0,
        "Forward 10 seconds",
        "Forward 10 s (→: 5 s)",
    )
    .clicked();
    ui.add_space(CONTROL_GAP);
    c.bookmark = player_btn(
        ui,
        ph::BOOKMARK_SIMPLE,
        16.0,
        "Add bookmark at the current position",
        "Add bookmark here",
    )
    .clicked();
}

/// The speaker glyph for a volume level.
fn volume_glyph(volume: f32) -> &'static str {
    if volume <= 0.0 {
        ph::SPEAKER_X
    } else if volume < 0.5 {
        ph::SPEAKER_LOW
    } else {
        ph::SPEAKER_HIGH
    }
}

/// The volume slider (step 0.05): inline after a speaker glyph, or — when
/// `compact` — in a popover above a speaker button. Returns whether the
/// value changed. In a right-to-left layout (the slider sits right).
fn volume_control(ui: &mut egui::Ui, volume: &mut f32, compact: bool, open: bool) -> bool {
    // A slider is as tall as a line of body text.
    let slider = |ui: &mut egui::Ui, volume: &mut f32, width: f32| {
        let height = ui.text_style_height(&egui::TextStyle::Body);
        let response = centred(ui, height, |ui| {
            widgets::slider(ui, volume, 0.0..=1.0, 0.05, Some(width))
        })
        .on_hover_text("Volume");
        let value = *volume;
        response.widget_info(|| egui::WidgetInfo::slider(true, f64::from(value) * 100.0, "Volume"));
        response.changed()
    };
    if !compact {
        let mut changed = false;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            changed = slider(ui, volume, VOLUME_WIDTH);
            ui.label(
                egui::RichText::new(volume_glyph(*volume))
                    .size(17.0)
                    .color(pal().text_muted),
            )
            .on_hover_text("Volume");
        });
        return changed;
    }
    let button = player_btn(ui, volume_glyph(*volume), 17.0, "Volume", "Volume");
    let popup_id = Popup::default_response_id(&button);
    if open {
        Popup::open_id(ui.ctx(), popup_id);
    }
    let mut changed = false;
    Popup::from_toggle_button_response(&button)
        .align(RectAlign::TOP)
        .gap(6.0)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .frame(
            Frame::popup(ui.style())
                .fill(pal().bg_raised)
                .stroke(Stroke::new(1.0, pal().border))
                .corner_radius(CornerRadius::same(theme::R_MD))
                .inner_margin(Margin::symmetric(12, 10)),
        )
        .show(|ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                changed = slider(ui, volume, VOLUME_POPOVER_WIDTH);
                let percent = format!("{}%", (*volume * 100.0).round());
                ui.add_sized(
                    [30.0, 16.0],
                    egui::Label::new(
                        egui::RichText::new(percent)
                            .size(12.0)
                            .color(pal().text_muted),
                    ),
                );
            });
        });
    changed
}

/// The speed select (`.player-speed`). Returns whether it changed.
fn speed_select(ui: &mut egui::Ui, speed: &mut f32) -> bool {
    let before = *speed;
    widgets::select(ui, widgets::SelectKind::Compact, |ui| {
        egui::ComboBox::from_id_salt("player-speed")
            .selected_text(egui::RichText::new(speed_label(*speed)).size(12.0))
            .width(52.0)
            .show_ui(ui, |ui| {
                for option in SPEEDS {
                    ui.selectable_value(
                        speed,
                        option,
                        egui::RichText::new(speed_label(option)).size(12.0),
                    );
                }
            })
            .response
            .on_hover_text("Playback speed")
    });
    (*speed - before).abs() > f32::EPSILON
}

/// [`speed_select`] centred on the row's line (the compact select: a
/// 12 pt line + 4 pt padding above and below).
fn speed_select_centred(ui: &mut egui::Ui, speed: &mut f32) -> bool {
    let height = ui.fonts_mut(|f| f.row_height(&theme::regular(12.0))) + 8.0;
    centred(ui, height, |ui| speed_select(ui, speed))
}

/// Run `add` in a box exactly `height` tall, centred on the row's line, in
/// the row's direction. egui's ComboBox and Slider lay out inside a nested
/// `horizontal` from the top of the space they get, so in a centred row
/// their top would sit on the centre line, the control hanging below it.
fn centred<R>(ui: &mut egui::Ui, height: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let free = ui.available_rect_before_wrap();
    let cy = free.center().y;
    let rect = Rect::from_x_y_ranges(free.x_range(), (cy - height / 2.0)..=(cy + height / 2.0));
    let layout = if ui.layout().main_dir() == egui::Direction::RightToLeft {
        Layout::right_to_left(Align::Min)
    } else {
        Layout::left_to_right(Align::Min)
    };
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    let inner = add(&mut child);
    ui.advance_cursor_after_rect(child.min_rect());
    inner
}

/// Run row 2's intents against the player, the prefs and the view.
fn apply_controls(
    cx: &mut UiCtx,
    detail: &MeetingDetail,
    snap: &PlayerSnapshot,
    c: Controls,
    volume: f32,
    speed: f32,
    out: &mut PlayerBarOutput,
) {
    if c.back {
        cx.player.skip_ms(-SKIP_BUTTON_MS);
    }
    if c.toggle {
        cx.player.toggle();
        out.toggled = true;
    }
    if c.forward {
        cx.player.skip_ms(SKIP_BUTTON_MS);
    }
    if c.bookmark {
        out.add_bookmark_at_ms = Some(i64::try_from(snap.position_ms).unwrap_or(i64::MAX));
    }
    if c.volume_changed {
        cx.prefs.volume = volume;
        cx.player.set_volume(volume);
    }
    if c.speed_changed {
        cx.prefs.speed = speed;
        cx.player.set_speed(speed);
    }
    if c.save {
        save_copy(cx, detail.meeting.id);
    }
}

/// "Save a copy…": check the archive exists BEFORE any dialog (the old
/// `export_audio` command's order), then the save dialog, then the copy —
/// all in one op on a worker, so its error toast survives leaving the
/// transcript. Cancelling the dialog is silent.
fn save_copy(cx: &mut UiCtx, meeting_id: i64) {
    let owner = cx.dialog_owner();
    cx.tasks.spawn_op("export-audio", move |core| {
        let result = (|| {
            core.audio_path(meeting_id)?;
            let stem = core.suggested_file_stem(meeting_id)?;
            let request = SaveFileRequest {
                file_name: format!("{stem}.opus"),
                filter_name: "Ogg Opus audio".into(),
                extensions: vec!["opus".into()],
            };
            match dialogs::save_file_blocking(owner, request) {
                Some(dest) => core.export_audio(meeting_id, &dest),
                None => Ok(()),
            }
        })();
        OpDone::Notice {
            notice: result
                .err()
                .map(|error| Notice::error("Could not export the audio", error)),
            refresh: false,
        }
    });
}

/// A transport button: a 32×32 ghost square (radius 8) whose muted glyph
/// brightens on hover. `label` is the accessible name, `tooltip` the CSS
/// `title`.
fn player_btn(ui: &mut egui::Ui, glyph: &str, size: f32, label: &str, tooltip: &str) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(glyph.to_owned(), theme::regular(size), pal().text);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(BTN_SIZE, BTN_SIZE), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if ui.is_rect_visible(rect) {
        let ring = response.has_focus() && widgets::focus_visible(ui.ctx());
        let hot = response.hovered() || ring;
        let painter = ui.painter();
        if hot {
            painter.rect_filled(rect, CornerRadius::same(theme::R_MD), pal().bg_hover);
        }
        let color = if hot { pal().text } else { pal().text_muted };
        painter.galley(rect.center() - galley.size() / 2.0, galley, color);
        if ring {
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(theme::R_MD + 2),
                Stroke::new(2.0, pal().focus),
                egui::StrokeKind::Outside,
            );
        }
    }
    response.on_hover_text(tooltip)
}

/// The primary play/pause: an accent disc with a solid white triangle or
/// pause bars (Phosphor's regular set has outline glyphs only).
fn play_btn(ui: &mut egui::Ui, playing: bool, label: &str, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(PLAY_SIZE, PLAY_SIZE), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if ui.is_rect_visible(rect) {
        let ring = response.has_focus() && widgets::focus_visible(ui.ctx());
        let hot = response.hovered() || ring;
        let painter = ui.painter();
        let c = rect.center();
        let radius = PLAY_SIZE / 2.0;
        painter.circle_filled(
            c,
            radius,
            if hot {
                pal().accent_hover
            } else {
                pal().accent
            },
        );
        if playing {
            for dx in [-3.5, 3.5] {
                let bar = Rect::from_center_size(c + egui::vec2(dx, 0.0), egui::vec2(3.5, 13.0));
                painter.rect_filled(bar, CornerRadius::same(1), pal().white);
            }
        } else {
            // Optically centred: the triangle's mass sits right of its box.
            let points = vec![
                c + egui::vec2(-4.0, -7.0),
                c + egui::vec2(8.0, 0.0),
                c + egui::vec2(-4.0, 7.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                points,
                pal().white,
                Stroke::NONE,
            ));
        }
        if ring {
            painter.circle_stroke(c, radius + 3.0, Stroke::new(2.0, pal().focus));
        }
    }
    response.on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds_snap_to_the_menu() {
        assert_eq!(snap_speed(1.0), 1.0);
        assert_eq!(snap_speed(1.3), 1.25);
        assert_eq!(snap_speed(0.1), 0.75);
        assert_eq!(snap_speed(9.0), 2.0);
        assert_eq!(snap_speed(f32::NAN), 1.0);
    }

    #[test]
    fn speed_labels_match_the_options() {
        let labels: Vec<String> = SPEEDS.iter().map(|s| speed_label(*s)).collect();
        assert_eq!(labels, ["0.75×", "1×", "1.25×", "1.5×", "1.75×", "2×"]);
    }

    #[test]
    fn clock_formats_like_fmt_clock() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(61_999), "1:01");
        assert_eq!(clock(3_600_000), "1:00:00");
    }

    fn timeline(spans: &[(i64, i64, u16)], dimmed: &[u16]) -> Timeline {
        Timeline {
            version: 1,
            speakers: (0..3)
                .map(|i| TimelineSpeaker {
                    name: format!("P{i}"),
                    label: format!("S{}", i + 1),
                    dimmed: dimmed.contains(&i),
                })
                .collect(),
            spans: spans
                .iter()
                .map(|&(start_ms, end_ms, speaker)| TimelineSpan {
                    start_ms,
                    end_ms,
                    speaker,
                })
                .collect(),
        }
    }

    fn summary(runs: &[Run]) -> Vec<(usize, usize, Option<u16>)> {
        runs.iter().map(|r| (r.from, r.to, r.who)).collect()
    }

    #[test]
    fn timeline_runs_merge_columns_by_dominant_speaker() {
        // 20 columns of 1 s: P0 for 0–3 s, a breath, 4–7.5 s; silence; P1
        // for 12–20 s with a 2 s P2 interjection and a 0.3 s one.
        let t = timeline(
            &[
                (0, 3_000, 0),
                (4_000, 7_500, 0),
                (12_000, 14_000, 1),
                (14_000, 16_000, 2),
                (16_000, 20_000, 1),
                (17_100, 17_400, 2),
            ],
            &[],
        );
        let runs = timeline_runs(&t, 20_000, 20);
        // Column 7 is half P0 (≥ 30 % speech): still P0. The breath and
        // the interjections are bridged; the 4.5 s silence stays.
        assert_eq!(
            summary(&runs),
            [(0, 8, Some(0)), (8, 12, None), (12, 20, Some(1))],
            "{runs:?}"
        );
        // The runs tile the track exactly.
        assert_eq!(runs.first().map(|r| r.from), Some(0));
        assert!(runs.windows(2).all(|w| w[0].to == w[1].from));
        assert_eq!(runs.last().map(|r| r.to), Some(20));
    }

    #[test]
    fn a_short_line_alone_in_silence_stays() {
        let t = timeline(&[(5_000, 6_000, 0), (12_000, 20_000, 1)], &[]);
        assert_eq!(
            summary(&timeline_runs(&t, 20_000, 20)),
            [
                (0, 5, None),
                (5, 6, Some(0)),
                (6, 12, None),
                (12, 20, Some(1))
            ]
        );
    }

    #[test]
    fn timeline_runs_drop_filtered_speakers_and_bad_spans() {
        let t = timeline(
            &[(0, 5_000, 0), (5_000, 10_000, 1), (8_000, 7_000, 2)],
            &[1],
        );
        // P1 is filtered out (neutral), the inverted span ignored.
        assert_eq!(
            summary(&timeline_runs(&t, 10_000, 4)),
            [(0, 2, Some(0)), (2, 4, None)]
        );
        assert!(timeline_runs(&t, 0, 4).is_empty());
        assert!(timeline_runs(&t, 10_000, 0).is_empty());
        // Spans past the end (a duration shorter than the transcript) clamp.
        let late = timeline(&[(0, 50_000, 2)], &[]);
        let runs = timeline_runs(&late, 10_000, 5);
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].from, runs[0].to, runs[0].who), (0, 5, Some(2)));
    }

    #[test]
    fn thousands_of_segments_become_few_runs() {
        // Three hours, 3,000 segments alternating speakers every 10.
        let spans: Vec<(i64, i64, u16)> = (0..3_000i64)
            .map(|i| (i * 3_600, i * 3_600 + 3_200, ((i / 10) % 3) as u16))
            .collect();
        let t = timeline(&spans, &[]);
        let runs = timeline_runs(&t, 3_000 * 3_600, 700);
        assert!(runs.len() <= 700, "{}", runs.len());
        assert!(runs.iter().all(|r| r.who.is_some()), "no false silences");
    }

    #[test]
    fn hovering_names_the_speaker_or_just_the_time() {
        let t = timeline(&[(1_000, 4_000, 1), (6_000, 9_000, 0)], &[]);
        assert_eq!(hover_text(&t, 500), "0:00");
        assert_eq!(hover_text(&t, 2_000), "0:02 · P1");
        // In the gap after a segment: silence.
        assert_eq!(hover_text(&t, 5_000), "0:05");
        assert_eq!(hover_text(&t, 61_000), "1:01");
        assert_eq!(hover_text(&t, 8_999), "0:08 · P0");
    }

    #[test]
    fn time_labels_keep_one_width_while_digits_change() {
        let ctx = theme::test_context();
        let mut widths = Vec::new();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            for text in ["1:11", "0:00", "8:48", "1:00:00"] {
                widths.push(stable_width(ui, text, theme::regular(12.0)));
            }
        });
        output.textures_delta.clear();
        assert_eq!(widths[0], widths[1]);
        assert_eq!(widths[1], widths[2]);
        assert!(widths[3] > widths[0]);
    }

    #[test]
    fn volume_glyph_follows_the_level() {
        assert_eq!(volume_glyph(0.0), ph::SPEAKER_X);
        assert_eq!(volume_glyph(0.3), ph::SPEAKER_LOW);
        assert_eq!(volume_glyph(1.0), ph::SPEAKER_HIGH);
    }

    /// Lay out row 2 in a `width`-wide column (two passes, so the combo box
    /// knows its size); returns the clusters and the column's rect.
    fn controls_at(width: f32) -> (ControlsLayout, Rect) {
        let ctx = theme::test_context();
        let mut layout = None;
        let mut column = Rect::NOTHING;
        for _ in 0..2 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 100.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(width);
                        column = ui.max_rect();
                        let (mut volume, mut speed) = (0.8, 1.25);
                        layout = Some(controls_row(ui, false, &mut volume, &mut speed, false).1);
                    },
                );
            });
            output.textures_delta.clear();
        }
        (layout.unwrap(), column)
    }

    /// The bar's content width at a window width (16 pt page padding, a
    /// 10 pt scroll gutter, 12 pt surface padding).
    fn content_width(window: f32) -> f32 {
        window - 10.0 - 2.0 * 16.0 - 2.0 * 12.0
    }

    #[test]
    fn controls_stay_on_one_line_down_to_the_minimum_width() {
        for window in [380.0, 460.0, 640.0] {
            let width = content_width(window);
            let (layout, column) = controls_at(width);
            assert!(
                layout.left.right() + CONTROL_GAP <= layout.right.left() + 0.5,
                "{window}: clusters overlap: {layout:?}"
            );
            assert!(
                layout.right.right() <= column.right() + 0.5,
                "{window}: right cluster overflows: {layout:?} in {column:?}"
            );
            assert!(
                (layout.left.center().y - layout.right.center().y).abs() < 1.0,
                "{window}: clusters on different lines: {layout:?}"
            );
        }
    }

    #[test]
    fn the_volume_slider_collapses_only_when_narrow() {
        let (narrow, _) = controls_at(content_width(380.0));
        let (wide, _) = controls_at(content_width(460.0));
        // The inline slider adds its 80 pt (+ gap) to the right cluster.
        assert!(
            wide.right.width() - narrow.right.width() > VOLUME_WIDTH - BTN_SIZE,
            "{narrow:?} vs {wide:?}"
        );
    }

    #[test]
    fn the_bar_column_lines_up_with_the_padded_column_beside_a_scroll_bar() {
        let ctx = theme::test_context();
        let (mut bar, mut page) = (Rect::NOTHING, Rect::NOTHING);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(1000.0, 100.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(1000.0);
                    bar = bar_column(ui, 10.0, |ui| ui.max_rect());
                    ui.allocate_ui_with_layout(
                        egui::vec2(990.0, 50.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(990.0);
                            page = widgets::padded_column(ui, |ui| ui.max_rect());
                        },
                    );
                },
            );
        });
        output.textures_delta.clear();
        assert!(
            (bar.left() - page.left()).abs() < 0.5,
            "{bar:?} vs {page:?}"
        );
        assert!(
            (bar.right() - page.right()).abs() < 0.5,
            "{bar:?} vs {page:?}"
        );
    }
}
