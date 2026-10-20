//! The transcript view (map-ui-transcript §1–§9): header (Back, title with
//! rename, meta line, Export and Retranscribe menus), the speaker summary
//! (one stacked proportion bar + a wrapping legend), the provisional
//! live-caption feed while a meeting records, the collapsible notes,
//! segments interleaved with bookmarks, the "Jump to current" pill, and the
//! player bar docked at the bottom.
//!
//! Layout (design/ui-rework.md): the scroller spans the window (its bar at
//! the edge) with the content in the page's padded, centred column; the
//! player bar uses the same column. Segments are grouped by speaker turn:
//! a turn's first row carries a header (the speaker's name in its colour,
//! which opens the rename, and the time, which seeks); the rows after it
//! are just text — click to play, the time is in the tooltip, and a
//! focusable seek target sits in the row's left padding for the keyboard.
//!
//! Threading (design §3.4): the meeting, its audio path, the people list
//! and the clipboard text are view-owned `Task`s, so a remount drops stale
//! results. Every mutation (renames, bookmarks, retranscribe, exports) is a
//! `spawn_op`, so its toast survives navigation and a hidden window; the
//! view patches its copy optimistically and a failed op bumps
//! `refresh_tick`, which reloads the truth. Notes never write to the
//! database from here: every save goes through `AppCore::queue_notes`
//! (ordered, latest-wins), and the "Saving…/Saved" indicator follows the
//! writer's acks in `ShellModel::notes_acks`.
//!
//! Long meetings: the row list is virtualised. Rows outside the viewport
//! are replaced by blank space whose height comes from a per-row cache
//! (measured when a row was last drawn, estimated before that), so a
//! multi-hour transcript lays out only the few dozen rows on screen.
//! Follow-along, deep links and the Jump pill scroll to a row's cached
//! position, re-aiming once the real heights around it are measured.
//!
//! Selection limitation (accepted, R12): drag-selecting text only spans
//! rows that are laid out; "Copy to clipboard" and the exports cover the
//! whole transcript.
//!
//! Find (Ctrl+F) searches the data, not the drawn rows: every shown
//! segment's text, case- and (Latin) accent-insensitively; the current hit
//! scrolls into view through the same scroll requests as a deep link. The
//! speaker filter (a click in the legend or on the proportion bar; Ctrl
//! adds speakers) rebuilds the row list with each run of hidden lines
//! collapsed into one faint "n lines from others" row, which stands in for
//! them when playback follows along — and find searches only the shown
//! lines.

use super::ai_summary::{self, SummaryAction, SummaryInput};
use super::player_bar::{self, PlayerBarState, Timeline, TimelineSpan, TimelineSpeaker};
use crate::bridge::{OpDone, Task};
use crate::model::AiTaskState;
use crate::platform::dialogs::{self, SaveFileRequest};
use crate::ui::format::{
    fmt_date, fmt_duration, fmt_thousands, fmt_ts, highlight_ranges, highlight_tokens,
};
use crate::ui::icons::{self, ph};
use crate::ui::theme::{self, pal, speaker_text};
use crate::ui::toasts::Notice;
use crate::ui::widgets::{self, KeyScroll, Politeness};
use crate::ui::{PaneRole, QueuedNotes, SettingsPage, UiCtx, View};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Key, Layout, Margin, Modifiers,
    Order, Popup, Rect, RectAlign, Response, Sense, Stroke,
};
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use witness_core::db::{Bookmark, Meeting, Segment, Speaker};
use witness_core::events::{AiTask, LiveTranscript};
use witness_core::player::{PlayerLoad, PlayerSnapshot};
use witness_core::service::{MeetingDetail, PersonInfo};
use witness_core::settings::Engine;
use witness_core::transcript_export::ExportFormat;
use witness_core::CoreResult;

/// Notes autosave debounce.
const NOTES_DEBOUNCE: Duration = Duration::from_millis(800);
/// How long the Export button shows the check mark after a copy.
const COPIED_FOR: Duration = Duration::from_millis(1500);
/// Repaint interval while audio plays (position, active row, follow).
const PLAY_FRAME: Duration = Duration::from_millis(33);
/// ←/→ while nothing has focus.
const KEY_SKIP_MS: i64 = 5_000;
/// Space under every list row.
const ROW_GAP: f32 = 1.0;
/// A row's padding: its hover / active fill reaches this far past the text.
const ROW_PAD_X: f32 = 10.0;
const ROW_PAD_Y: f32 = 4.0;
/// Extra space above a speaker turn (outside its first row's fill), and
/// above a bookmark row. The list's first row has none.
const TURN_GAP: f32 = 12.0;
const BOOKMARK_GAP: f32 = 6.0;
/// A turn header's line (name + time) and the gap under it.
const HEADER_H: f32 = 20.0;
const HEADER_GAP: f32 = 2.0;
/// The speaker's name in a turn header.
const NAME_SIZE: f32 = 13.0;
/// A bookmark row's line: as tall as its trash button.
const BOOKMARK_LINE: f32 = 28.0;
/// Transcript text: 14 pt at `line-height: 1.45`.
const TEXT_SIZE: f32 = 14.0;
const LINE_HEIGHT: f32 = TEXT_SIZE * 1.45;
/// Estimating an unmeasured row's text height (calibrated on measured rows
/// at 380–800 pt, unbiased to within a point): a 14 pt character's average
/// advance, the characters word wrapping leaves unused per line, and the
/// line as rendered (the 20.3 pt line lands on whole pixels).
const EST_CHAR_W: f32 = 6.3;
const EST_WRAP_LOSS: f32 = 3.0;
const EST_LINE: f32 = 20.0;
/// Passes after a width change that keep restoring the reading anchor
/// while newly drawn rows replace their estimates with measurements.
const RESIZE_SETTLE_PASSES: u8 = 6;
/// A row measured at a width this far from the current one is estimated
/// instead (see `RowList::heights`).
const STALE_WIDTH: f32 = 8.0;
/// The speaker summary's proportion bar: thickness, the gap between two
/// speakers' segments, and the narrowest a (tiny) share is drawn.
const SUMMARY_BAR_H: f32 = 8.0;
const SUMMARY_GAP: f32 = 2.0;
const SUMMARY_MIN_SEG: f32 = 3.0;
/// Rows laid out beyond the visible area (smooth wheel scrolling).
const OVERSCAN: f32 = 600.0;
/// A deep link re-aims until the row sits this close to the centre, for
/// at most `SCROLL_SETTLE_FRAMES` passes.
const SCROLL_SETTLED_PX: f32 = 24.0;
const SCROLL_SETTLE_FRAMES: u8 = 20;
/// Speaker-rename suggestions shown at once.
const MAX_SUGGESTIONS: usize = 8;
/// `.live-feed` border: the record red at this strength.
const LIVE_BORDER_ALPHA: f32 = 0.35;
/// The "n lines from others" row standing in for filtered-out lines: its
/// line, and the space above it (none as the list's first row).
const HIDDEN_LINE: f32 = 22.0;
const HIDDEN_GAP: f32 = 6.0;
/// Legend items and proportion-bar segments of speakers outside the filter.
const FILTERED_OUT_ALPHA: f32 = 0.4;
/// The find bar's height.
const FIND_BAR_H: f32 = 36.0;

// ---------------------------------------------------------------------------
// Pure helpers

/// Binary search: the last segment with `start_ms <= t` (so the highlight
/// persists through gaps until the next segment starts; `end_ms` is never
/// used). Segments are ordered by `start_ms`.
pub(crate) fn segment_at(segments: &[Segment], t_ms: i64) -> Option<usize> {
    segments
        .partition_point(|segment| segment.start_ms <= t_ms)
        .checked_sub(1)
}

/// One list row: a segment (index into `segments`), a bookmark, or a run
/// of `count` consecutive segments from `first` that the speaker filter
/// hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Seg(usize),
    Bm(usize),
    Hidden { first: usize, count: usize },
}

/// A row's identity across reloads (height cache, widget ids). A hidden
/// run is known by its first segment's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RowKey {
    Seg(i64),
    Bm(i64),
    Hidden(i64),
}

/// Segments and bookmarks interleaved by time; a stable sort, so on equal
/// times the segment comes before the bookmark. Segments `shown` rejects
/// collapse into [`Row::Hidden`] runs (a shown row or a bookmark ends a
/// run).
fn build_rows(detail: &MeetingDetail, shown: impl Fn(usize) -> bool) -> Vec<Row> {
    let mut timed: Vec<(i64, Row)> =
        Vec::with_capacity(detail.segments.len() + detail.bookmarks.len());
    timed.extend(
        detail
            .segments
            .iter()
            .enumerate()
            .map(|(i, segment)| (segment.start_ms, Row::Seg(i))),
    );
    timed.extend(
        detail
            .bookmarks
            .iter()
            .enumerate()
            .map(|(i, bookmark)| (bookmark.at_ms, Row::Bm(i))),
    );
    timed.sort_by_key(|(at, _)| *at);
    let mut rows: Vec<Row> = Vec::with_capacity(timed.len());
    for (_, row) in timed {
        match (row, rows.last_mut()) {
            (Row::Seg(i), Some(Row::Hidden { first, count }))
                if !shown(i) && *first + *count == i =>
            {
                *count += 1;
            }
            (Row::Seg(i), _) if !shown(i) => rows.push(Row::Hidden { first: i, count: 1 }),
            (row, _) => rows.push(row),
        }
    }
    rows
}

/// Who a segment belongs to for turn grouping: its speaker, or — while
/// unassigned — its track (shown as `me` / `S?`).
fn turn_key(seg: &Segment) -> (Option<i64>, &str) {
    match seg.speaker_id {
        Some(id) => (Some(id), ""),
        None => (None, seg.track.as_str()),
    }
}

/// Which rows start a speaker turn (and so carry the header): a segment
/// whose speaker differs from the previous row's, or whose previous row is
/// a bookmark or hidden lines (they interrupt the turn). Those rows are
/// never one.
fn turn_starts(detail: &MeetingDetail, rows: &[Row]) -> Vec<bool> {
    let mut previous = None;
    rows.iter()
        .map(|row| match *row {
            Row::Seg(i) => {
                let key = turn_key(&detail.segments[i]);
                let head = previous != Some(key);
                previous = Some(key);
                head
            }
            Row::Bm(_) | Row::Hidden { .. } => {
                previous = None;
                false
            }
        })
        .collect()
}

/// Who a segment belongs to for the speaker filter and the summary: its
/// speaker, or — while unassigned — its track (mic = `me`, else `S?`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum SpeakerKey {
    Speaker(i64),
    Unassigned { mic: bool },
}

fn speaker_key(seg: &Segment) -> SpeakerKey {
    match seg.speaker_id {
        Some(id) => SpeakerKey::Speaker(id),
        None => SpeakerKey::Unassigned {
            mic: seg.track == "mic",
        },
    }
}

/// The speakers whose lines show: everyone when `filter` is empty.
fn is_shown(filter: &BTreeSet<SpeakerKey>, seg: &Segment) -> bool {
    filter.is_empty() || filter.contains(&speaker_key(seg))
}

/// A click on a speaker in the legend / proportion bar: alone, show only
/// them — or everyone again when they were the only one shown; with Ctrl,
/// add or remove them (removing the last one shows everyone).
fn toggle_filter(filter: &mut BTreeSet<SpeakerKey>, key: SpeakerKey, add: bool) {
    if add {
        if !filter.remove(&key) {
            filter.insert(key);
        }
    } else if filter.len() == 1 && filter.contains(&key) {
        filter.clear();
    } else {
        filter.clear();
        filter.insert(key);
    }
}

/// How a segment's speaker is shown.
struct Who<'a> {
    /// Immutable label (`me`, `S1`…, or `S?` for an unassigned loopback
    /// segment): drives the chip colour.
    label: &'a str,
    name: &'a str,
    auto: bool,
}

fn speaker_of<'a>(
    detail: &'a MeetingDetail,
    index: &HashMap<i64, usize>,
    speaker_id: Option<i64>,
) -> Option<&'a Speaker> {
    speaker_id
        .and_then(|id| index.get(&id))
        .map(|&i| &detail.speakers[i])
}

fn who<'a>(detail: &'a MeetingDetail, index: &HashMap<i64, usize>, seg: &'a Segment) -> Who<'a> {
    let speaker = speaker_of(detail, index, seg.speaker_id);
    let fallback = if seg.track == "mic" { "me" } else { "S?" };
    let label = speaker.map_or(fallback, |s| s.label.as_str());
    let name = speaker
        .map(|s| s.display_name.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(label);
    Who {
        label,
        name,
        auto: speaker.is_some_and(|s| s.auto_labeled),
    }
}

/// One entry of the speaker stats bar.
#[derive(Debug, Clone, PartialEq)]
struct SpeakerStat {
    key: SpeakerKey,
    name: String,
    label: String,
    talk_ms: i64,
    words: i64,
    /// Percent of all talk time.
    share: f64,
}

/// Per-speaker talk time, words and share, by talk time descending. Keyed
/// by speaker id, or by track for unassigned segments ([`SpeakerKey`]).
fn speaker_stats(detail: &MeetingDetail, index: &HashMap<i64, usize>) -> Vec<SpeakerStat> {
    let mut stats: Vec<SpeakerStat> = Vec::new();
    let mut by_key: HashMap<SpeakerKey, usize> = HashMap::new();
    for seg in &detail.segments {
        let who = who(detail, index, seg);
        let key = speaker_key(seg);
        let slot = *by_key.entry(key).or_insert_with(|| {
            stats.push(SpeakerStat {
                key,
                name: who.name.to_string(),
                label: who.label.to_string(),
                talk_ms: 0,
                words: 0,
                share: 0.0,
            });
            stats.len() - 1
        });
        stats[slot].talk_ms += seg.end_ms - seg.start_ms;
        stats[slot].words += seg.text.split_whitespace().count() as i64;
    }
    stats.sort_by_key(|stat| std::cmp::Reverse(stat.talk_ms));
    let total: i64 = stats.iter().map(|s| s.talk_ms).sum();
    let total = if total == 0 { 1 } else { total };
    for stat in &mut stats {
        stat.share = stat.talk_ms as f64 / total as f64 * 100.0;
    }
    stats
}

/// `{fmtDuration(talk)} · {words} words · {share}%` (the full figures, in
/// the summary's tooltips).
fn stat_text(stat: &SpeakerStat) -> String {
    format!(
        "{} · {} words · {}%",
        fmt_duration(Some(stat.talk_ms)),
        fmt_thousands(stat.words),
        stat.share.round()
    )
}

/// The legend's short figures: `{fmtDuration(talk)} · {share}%`.
fn legend_text(stat: &SpeakerStat) -> String {
    format!(
        "{} · {}%",
        fmt_duration(Some(stat.talk_ms)),
        stat.share.round()
    )
}

/// The proportion bar's segment widths for `shares` (percent) in `width`:
/// every speaker gets at least `SUMMARY_MIN_SEG` (a 0 % voice still shows),
/// `SUMMARY_GAP` separates neighbours, and the widths always fill `width`
/// exactly.
fn summary_widths(shares: &[f64], width: f32) -> Vec<f32> {
    let n = shares.len();
    if n == 0 {
        return Vec::new();
    }
    let total: f64 = shares.iter().sum();
    let fixed = SUMMARY_GAP * (n - 1) as f32 + SUMMARY_MIN_SEG * n as f32;
    let spread = (width - fixed).max(0.0);
    shares
        .iter()
        .map(|share| {
            let t = if total > 0.0 {
                (share / total) as f32
            } else {
                1.0 / n as f32
            };
            SUMMARY_MIN_SEG + t * spread
        })
        .collect()
}

/// The scrubber's speaker timeline: one timeline speaker per summary entry
/// (dimmed when the filter hides them), one span per segment.
fn build_timeline(
    detail: &MeetingDetail,
    stats: &[SpeakerStat],
    filter: &BTreeSet<SpeakerKey>,
    version: u64,
) -> Timeline {
    let slot: HashMap<SpeakerKey, u16> = stats
        .iter()
        .enumerate()
        .filter_map(|(i, stat)| Some((stat.key, u16::try_from(i).ok()?)))
        .collect();
    Timeline {
        version,
        speakers: stats
            .iter()
            .map(|stat| TimelineSpeaker {
                name: stat.name.clone(),
                label: stat.label.clone(),
                dimmed: !filter.is_empty() && !filter.contains(&stat.key),
            })
            .collect(),
        spans: detail
            .segments
            .iter()
            .filter_map(|seg| {
                Some(TimelineSpan {
                    start_ms: seg.start_ms,
                    end_ms: seg.end_ms,
                    speaker: *slot.get(&speaker_key(seg))?,
                })
            })
            .collect(),
    }
}

/// "Paul Gordon", "Paul Gordon and Bob", "3 speakers": who the filter
/// shows, in summary order.
fn filter_names(stats: &[SpeakerStat], filter: &BTreeSet<SpeakerKey>) -> String {
    let names: Vec<&str> = stats
        .iter()
        .filter(|stat| filter.contains(&stat.key))
        .map(|stat| stat.name.as_str())
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} and {b}"),
        many => format!("{} speakers", many.len()),
    }
}

// ---------------------------------------------------------------------------
// Find

/// A Latin letter without its accent (Latin-1 Supplement and Latin
/// Extended-A, lowercase in); anything else unchanged. Cheap, and covers
/// the names and loanwords a transcript meets ("Zoë", "café", "Łukasz").
fn fold_char(c: char) -> char {
    // U+0100–U+017F in order; `_` keeps the character (ligatures).
    const EXTENDED_A: &str = "aaaaaaccccccccddddeeeeeeeeeegggggggghhhhiiiiiiiiii__jjkkklllllllllln\
                              nnnnnnnnoooooo__rrrrrrssssssssttttttuuuuuuuuuuuuwwyyyzzzzzzs";
    match c {
        'à'..='å' => 'a',
        'ç' => 'c',
        'è'..='ë' => 'e',
        'ì'..='ï' => 'i',
        'ñ' => 'n',
        'ò'..='ö' | 'ø' => 'o',
        'ù'..='ü' => 'u',
        'ý' | 'ÿ' => 'y',
        '\u{100}'..='\u{17f}' => match EXTENDED_A.chars().nth(c as usize - 0x100) {
            Some('_') | None => c,
            Some(base) => base,
        },
        _ => c,
    }
}

/// Combining accents (U+0300–U+036F): dropped, so decomposed text matches.
fn is_combining(c: char) -> bool {
    ('\u{300}'..='\u{36f}').contains(&c)
}

/// `text` lowercased and accent-folded for find, with each folded byte's
/// source character as a byte range of `text`.
fn fold_with_origin(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut folded = String::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len());
    for (start, c) in text.char_indices() {
        let end = start + c.len_utf8();
        for lower in c.to_lowercase() {
            if is_combining(lower) {
                continue;
            }
            let before = folded.len();
            folded.push(fold_char(lower));
            origin.extend(std::iter::repeat_n((start, end), folded.len() - before));
        }
    }
    (folded, origin)
}

/// The query as find compares it (trimmed, folded); empty finds nothing.
fn find_needle(query: &str) -> String {
    fold_with_origin(query.trim()).0
}

/// Byte ranges of `text` where `needle` (already [`find_needle`]d) occurs,
/// left to right, not overlapping.
fn find_in(text: &str, needle: &str) -> Vec<Range<usize>> {
    if needle.is_empty() || text.is_empty() {
        return Vec::new();
    }
    let (folded, origin) = fold_with_origin(text);
    folded
        .match_indices(needle)
        .map(|(at, hit)| {
            // The hit takes the accents that follow its last letter.
            let mut end = origin[at + hit.len() - 1].1;
            while let Some(c) = text[end..].chars().next().filter(|c| is_combining(*c)) {
                end += c.len_utf8();
            }
            origin[at].0..end
        })
        .collect()
}

/// One find hit: a segment and the hit's byte range in its text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FindHit {
    seg: usize,
    range: Range<usize>,
}

/// Every hit of `query` in the shown segments, in transcript order.
fn find_hits(detail: &MeetingDetail, shown: impl Fn(usize) -> bool, query: &str) -> Vec<FindHit> {
    let needle = find_needle(query);
    if needle.is_empty() {
        return Vec::new();
    }
    detail
        .segments
        .iter()
        .enumerate()
        .filter(|(i, _)| shown(*i))
        .flat_map(|(seg, s)| {
            find_in(&s.text, &needle)
                .into_iter()
                .map(move |range| FindHit { seg, range })
        })
        .collect()
}

/// The hit to start from: the first at or after segment `from` (the row at
/// the top of the view), wrapping to the first.
fn first_hit_from(hits: &[FindHit], from: usize) -> Option<usize> {
    if hits.is_empty() {
        return None;
    }
    Some(hits.partition_point(|h| h.seg < from) % hits.len())
}

/// The next (or previous) hit after `current`, wrapping around.
fn step_hit(len: usize, current: Option<usize>, back: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, back) {
        (None, false) => 0,
        (None, true) => len - 1,
        (Some(at), false) => (at + 1) % len,
        (Some(at), true) => (at + len - 1) % len,
    })
}

/// "3 of 17", "No matches", or nothing for an empty query.
fn find_count_text(query: &str, hits: usize, current: Option<usize>) -> String {
    if find_needle(query).is_empty() {
        String::new()
    } else if hits == 0 {
        "No matches".to_owned()
    } else {
        format!("{} of {}", current.map_or(0, |c| c + 1), hits)
    }
}

/// Ctrl+F's find bar and its hits.
#[derive(Debug, Default)]
struct FindState {
    query: String,
    hits: Vec<FindHit>,
    current: Option<usize>,
    /// (query, data generation) the hits were computed for.
    computed: Option<(String, u64)>,
    /// Focus the field (and select its text) on the next draw.
    focus: bool,
    /// The widget that had focus when find opened: it gets it back on Esc.
    return_focus: Option<Id>,
}

impl FindState {
    /// The hits for segment `seg` (a slice of `hits`) and the index of the
    /// first of them in `hits`.
    fn hits_in(&self, seg: usize) -> (usize, &[FindHit]) {
        let lo = self.hits.partition_point(|h| h.seg < seg);
        let hi = lo + self.hits[lo..].partition_point(|h| h.seg == seg);
        (lo, &self.hits[lo..hi])
    }
}

/// The status text under an empty transcript (map §4.6).
fn empty_message(
    detail: &MeetingDetail,
    progress: Option<(&str, f32)>,
) -> (Vec<String>, bool /* Retry */) {
    match detail.meeting.status.as_str() {
        "recorded" => (
            vec![if detail.pipeline_queued {
                "Queued for transcription".into()
            } else {
                "Not transcribed yet.".into()
            }],
            false,
        ),
        "processing" => (
            vec![match progress {
                Some((stage, pct)) => format!("Transcribing ({stage}) {}%", pct.round()),
                None => "Transcription in progress…".into(),
            }],
            false,
        ),
        "failed" => (
            vec![match &detail.last_error {
                Some(error) => format!("Transcription failed: {error}"),
                None => "Transcription failed.".into(),
            }],
            true,
        ),
        "transcribed" => (
            vec![
                "Transcription finished, but no speech was detected in this recording.".into(),
                "If people were talking, check the microphone / meeting-audio devices in Settings."
                    .into(),
            ],
            false,
        ),
        _ => (vec!["No transcript.".into()], false),
    }
}

/// How a row is drawn, beyond its data: whether it starts a speaker turn
/// (header line + turn gap) and whether it is the list's first row (no gap
/// above). A measured height is only reused for the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowShape {
    head: bool,
    first: bool,
}

/// A rough row height before the row has been measured: the text wraps in
/// the row's width (a bookmark's note beside its time and trash button);
/// a turn's first row adds its header line and the turn gap.
fn estimate_height(detail: &MeetingDetail, row: Row, shape: RowShape, width: f32) -> f32 {
    let above = |gap: f32| if shape.first { 0.0 } else { gap };
    let lines = |chars: usize, reserved: f32| {
        let text_width = (width - 2.0 * ROW_PAD_X - reserved).max(80.0);
        let per_line = (text_width / EST_CHAR_W - EST_WRAP_LOSS).max(8.0);
        (chars as f32 / per_line).ceil().max(1.0)
    };
    match row {
        Row::Seg(i) => {
            let text = lines(detail.segments[i].text.chars().count(), 0.0) * EST_LINE;
            let header = if shape.head {
                above(TURN_GAP) + HEADER_H + HEADER_GAP
            } else {
                0.0
            };
            header + text + 2.0 * ROW_PAD_Y + ROW_GAP
        }
        Row::Bm(i) => {
            let note = lines(detail.bookmarks[i].note.chars().count().max(30), 110.0);
            let line = (note * EST_LINE).max(BOOKMARK_LINE);
            above(BOOKMARK_GAP) + line + 2.0 * ROW_PAD_Y + ROW_GAP
        }
        // Fixed: one line, whatever the width.
        Row::Hidden { .. } => above(HIDDEN_GAP) + HIDDEN_LINE + ROW_GAP,
    }
}

/// Names offered while renaming a speaker: case-insensitive substring
/// matches (every person when the field is empty), the exact current text
/// excluded.
fn suggestions<'a>(people: &'a [PersonInfo], text: &str) -> Vec<&'a str> {
    let needle = text.trim().to_lowercase();
    people
        .iter()
        .map(|p| p.name.as_str())
        .filter(|name| name.to_lowercase().contains(&needle) && *name != text.trim())
        .take(MAX_SUGGESTIONS)
        .collect()
}

/// A segment's text with `marks` highlighted (`<mark>` runs: search terms
/// or find hits); the mark at index `current` is the current find hit, in
/// the accent itself. `marks` are sorted and do not overlap.
fn text_job(text: &str, marks: &[Range<usize>], current: Option<usize>) -> egui::text::LayoutJob {
    let plain = egui::TextFormat {
        font_id: theme::regular(TEXT_SIZE),
        color: pal().text,
        line_height: Some(LINE_HEIGHT),
        ..Default::default()
    };
    let mark = egui::TextFormat {
        background: pal().mark_bg,
        color: pal().mark_text,
        ..plain.clone()
    };
    let strong = egui::TextFormat {
        background: pal().accent,
        color: pal().on_accent,
        ..plain.clone()
    };
    let mut job = egui::text::LayoutJob::default();
    let mut at = 0;
    for (i, range) in marks.iter().enumerate() {
        if range.start < at || range.end > text.len() {
            continue;
        }
        if range.start > at {
            job.append(&text[at..range.start], 0.0, plain.clone());
        }
        let format = if current == Some(i) { &strong } else { &mark };
        job.append(&text[range.clone()], 0.0, format.clone());
        at = range.end;
    }
    if at < text.len() || text.is_empty() {
        job.append(&text[at..], 0.0, plain);
    }
    job
}

// ---------------------------------------------------------------------------
// Notes autosave state

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum NotesStatus {
    #[default]
    Idle,
    Saving,
    Saved,
}

/// The notes draft and its save state (transcript.tsx `flushNotes`), with
/// the writer queue in place of the in-flight request: a newer draft simply
/// supersedes an older queued one, so an older write can never land last.
#[derive(Debug, Default)]
struct Notes {
    /// Initialised from the first load only, so reloads never clobber
    /// what the user is typing.
    loaded: bool,
    draft: String,
    /// The newest text handed to the writer (or the loaded text).
    queued: String,
    /// The newest text the writer confirmed (or the loaded text).
    confirmed: String,
    /// Seq of the newest queued write (0 = none yet).
    seq: u64,
    /// The seq whose ack has been handled.
    handled_seq: u64,
    status: NotesStatus,
    /// The `<details>` disclosure.
    open: bool,
}

impl Notes {
    /// The first load: `text` from the database, `queued` the newest draft
    /// an earlier visit handed to the writer (already matched to this
    /// meeting's `started_at`), `ack` the writer's newest ack for the id AS
    /// IT WAS WHEN THE READ WAS SPAWNED — not when its result arrives:
    /// `meeting_detail` is several separately locked reads, so the writer
    /// can land the draft and ack it after the notes were read but before
    /// the result is consumed. That draft wins unless it was already saved
    /// before the read began: its write was then pending (the read may have
    /// beaten it) or it failed. Only this view writes notes, so it is then
    /// the newest text, and its ack settles it like any other write.
    fn load(
        &mut self,
        text: &str,
        queued: Option<(u64, String)>,
        ack: Option<&(u64, Option<String>)>,
    ) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        self.draft = text.to_string();
        self.queued = text.to_string();
        self.confirmed = text.to_string();
        let unsaved = |seq: u64| match ack {
            None => true,
            Some((acked, error)) => *acked < seq || (*acked == seq && error.is_some()),
        };
        if let Some((seq, newer)) = queued.filter(|(seq, newer)| newer != text && unsaved(*seq)) {
            self.draft = newer.clone();
            self.queued = newer;
            self.seq = seq;
        }
        self.open = !self.draft.is_empty();
    }

    /// The user typed: "Saving…" unless the text is back to the stored one.
    fn edited(&mut self) {
        if self.draft != self.confirmed {
            self.status = NotesStatus::Saving;
        }
    }

    /// The text to hand to the writer now, if it differs from the last
    /// queued one. `acked` is the writer's newest ack for this meeting.
    fn take_flush(&mut self, acked: u64) -> Option<String> {
        if !self.loaded {
            return None;
        }
        if self.draft == self.queued {
            if self.status == NotesStatus::Saving && self.seq <= acked {
                self.status = NotesStatus::Saved;
            }
            return None;
        }
        self.queued = self.draft.clone();
        self.status = NotesStatus::Saving;
        Some(self.queued.clone())
    }

    fn sent(&mut self, seq: u64) {
        self.seq = seq;
    }

    /// Fold in the writer's ack (`notes_acks[meeting]`); returns the error
    /// to toast, once per failed write.
    fn on_ack(&mut self, ack: Option<&(u64, Option<String>)>) -> Option<String> {
        let (acked, error) = ack?;
        if self.seq == 0 || *acked < self.seq || self.handled_seq == self.seq {
            return None;
        }
        self.handled_seq = self.seq;
        let settled = self.draft == self.queued;
        match error {
            None => {
                self.confirmed = self.queued.clone();
                if settled && self.status == NotesStatus::Saving {
                    self.status = NotesStatus::Saved;
                }
                None
            }
            Some(error) => {
                // The next flush retries even if the text does not change.
                self.queued = self.confirmed.clone();
                if settled {
                    self.status = NotesStatus::Idle;
                }
                Some(error.clone())
            }
        }
    }

    fn indicator(&self) -> Option<&'static str> {
        match self.status {
            NotesStatus::Idle => None,
            NotesStatus::Saving => Some("Saving…"),
            NotesStatus::Saved => Some("Saved"),
        }
    }
}

/// An earlier visit's queued draft for this id, only if it belongs to the
/// same meeting (`started_at`): a purged meeting's id can be reused, and its
/// notes must never surface in the successor.
fn queued_notes_for(queued: Option<&QueuedNotes>, started_at: &str) -> Option<(u64, String)> {
    queued
        .filter(|q| q.started_at == started_at)
        .map(|q| (q.seq, q.text.clone()))
}

/// "Copy to notes": the notes with the AI summary appended after a blank
/// line (the user's text is never replaced), or None when the notes
/// already contain it (a second click must not append it twice).
fn notes_with_summary(notes: &str, summary: &str) -> Option<String> {
    let summary = summary.trim();
    if summary.is_empty() || notes.contains(summary) {
        return None;
    }
    let notes = notes.trim_end();
    Some(if notes.is_empty() {
        summary.to_string()
    } else {
        format!("{notes}\n\n{summary}")
    })
}

// ---------------------------------------------------------------------------
// Editors

/// Result of an inline editor this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditOutcome {
    Open,
    /// Enter or focus loss: commit exactly once.
    Commit,
    /// Escape: a true cancel (no commit through the focus loss).
    Cancel,
}

/// An inline text field being edited (title, bookmark note).
#[derive(Debug)]
struct LineEdit {
    text: String,
    focus_pending: bool,
}

impl LineEdit {
    fn new(text: &str) -> Self {
        LineEdit {
            text: text.to_string(),
            focus_pending: true,
        }
    }
}

#[derive(Debug)]
struct SpeakerEdit {
    speaker_id: i64,
    /// The row whose chip was clicked: other rows of the speaker stay closed.
    seg_id: i64,
    edit: LineEdit,
    /// Where the suggestion list was drawn last frame (a click there must
    /// not commit the half-typed text through the focus loss).
    suggest_rect: Option<Rect>,
    /// The field lost focus toward the list and that commit was held back
    /// pending a pick.
    blur_swallowed: bool,
}

impl SpeakerEdit {
    fn new(speaker_id: i64, seg_id: i64, name: &str) -> Self {
        SpeakerEdit {
            speaker_id,
            seg_id,
            edit: LineEdit::new(name),
            suggest_rect: None,
            blur_swallowed: false,
        }
    }
}

#[derive(Debug)]
struct BookmarkEdit {
    id: i64,
    edit: LineEdit,
}

/// An inline rename field: BG fill, 1 px accent border, radius 6, 3/8
/// padding (`INLINE_EDIT_MARGIN`). Takes focus once when opened; reports
/// Enter / focus loss as Commit and Escape as Cancel. A focus loss into the
/// field's own right-click menu is neither. `label` is the field's
/// accessible name (the 0.2.x aria-label).
fn inline_editor(
    ui: &mut egui::Ui,
    id: Id,
    edit: &mut LineEdit,
    font: FontId,
    width: f32,
    hint: &str,
    label: &str,
) -> (Response, EditOutcome) {
    let response = Frame::new()
        .fill(pal().bg)
        .stroke(Stroke::new(1.0, pal().accent))
        .corner_radius(CornerRadius::same(theme::R_SM))
        .inner_margin(INLINE_EDIT_MARGIN)
        .show(ui, |ui| {
            let mut field = egui::TextEdit::singleline(&mut edit.text)
                .id(id)
                .font(font)
                .frame(Frame::NONE)
                .margin(Margin::ZERO)
                .desired_width(width);
            if !hint.is_empty() {
                field = field.hint_text(hint);
            }
            ui.add(field)
        })
        .inner;
    set_accessible_label(ui.ctx(), response.id, label);
    let menu_open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&response));
    let outcome = if response.lost_focus() && !menu_open {
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            EditOutcome::Cancel
        } else {
            EditOutcome::Commit
        }
    } else {
        EditOutcome::Open
    };
    widgets::text_edit_menu(&response);
    if edit.focus_pending {
        response.request_focus();
        edit.focus_pending = false;
    }
    (response, outcome)
}

/// [`inline_editor`]'s padding inside its 1 pt border.
const INLINE_EDIT_MARGIN: Margin = Margin::symmetric(8, 3);

/// Name a widget for screen readers (egui's TextEdit only exposes its hint
/// as a placeholder).
fn set_accessible_label(ctx: &egui::Context, id: Id, label: &str) {
    ctx.accesskit_node_builder(id, |node| node.set_label(label));
}

// ---------------------------------------------------------------------------
// Rows

/// What a row asked for this frame; applied after the list is drawn.
///
/// The editor results name the editor they close (`seg_id` / `id`): a
/// click on another row's chip or note blurs the open editor in the SAME
/// pass, and that row may sit above the editor's, so its Open action can be
/// pushed first. [`editor_results_first`] applies the closing results
/// before any Open, and each result only touches the editor it names.
#[derive(Debug, Clone, PartialEq)]
enum RowAction {
    /// Timestamp or text click: seek, play, follow.
    SeekPlay(usize),
    /// Chip click; the prefill is read when applied (after any commit).
    OpenSpeaker {
        speaker_id: i64,
        seg_id: i64,
    },
    SpeakerDone {
        seg_id: i64,
        outcome: EditOutcome,
    },
    /// A suggestion was picked: rename to it.
    SpeakerPick {
        seg_id: i64,
        name: String,
    },
    /// Bookmark time click: seek, play, follow (active row unchanged).
    BookmarkSeek(i64),
    /// Note click; the prefill is read when applied (after any commit).
    EditBookmark(i64),
    BookmarkDone {
        id: i64,
        outcome: EditOutcome,
    },
    DeleteBookmark(i64),
}

impl RowAction {
    /// An inline editor's result (commit, cancel, pick).
    fn closes_editor(&self) -> bool {
        matches!(
            self,
            RowAction::SpeakerDone { .. }
                | RowAction::SpeakerPick { .. }
                | RowAction::BookmarkDone { .. }
        )
    }
}

/// Editor results before everything else, otherwise in draw order: the
/// TSX commits the old editor on the mousedown blur, before the click
/// that opens the next one.
fn editor_results_first(actions: Vec<RowAction>) -> Vec<RowAction> {
    let (mut ordered, rest): (Vec<_>, Vec<_>) =
        actions.into_iter().partition(RowAction::closes_editor);
    ordered.extend(rest);
    ordered
}

/// What routing an editor action asks of the view.
#[derive(Debug)]
enum EditorEffect {
    /// Rename the closed editor's speaker to this name.
    CommitSpeaker(SpeakerEdit, String),
    /// Save the closed editor's bookmark note.
    CommitBookmark(BookmarkEdit),
    /// A rename opened: refresh the people suggestions.
    LoadPeople,
}

/// Take `slot`'s editor if it is the one `matches` names (a stale result
/// for an editor that is no longer open does nothing).
fn take_editor<T>(slot: &mut Option<T>, matches: impl FnOnce(&T) -> bool) -> Option<T> {
    if slot.as_ref().is_some_and(matches) {
        slot.take()
    } else {
        None
    }
}

/// What the rows need to draw, borrowed from the view for one pass.
struct RowCtx<'a> {
    detail: &'a MeetingDetail,
    speakers: &'a HashMap<i64, usize>,
    tokens: &'a [String],
    active: Option<usize>,
    speaker_edit: &'a mut Option<SpeakerEdit>,
    bookmark_edit: &'a mut Option<BookmarkEdit>,
    people: &'a [PersonInfo],
    /// A deep link's segment whose seek button takes keyboard focus when
    /// its row is laid out (cleared once taken).
    focus_seg: &'a mut Option<i64>,
    /// Find's hits while it has a query (they replace the search terms'
    /// highlighting).
    find: Option<&'a FindState>,
}

impl RowCtx<'_> {
    /// The highlighted ranges in segment `seg`'s text and which of them is
    /// the current find hit.
    fn marks(&self, seg: usize) -> (Vec<Range<usize>>, Option<usize>) {
        match self.find {
            Some(find) => {
                let (first, hits) = find.hits_in(seg);
                let current = find
                    .current
                    .and_then(|c| c.checked_sub(first))
                    .filter(|c| *c < hits.len());
                (hits.iter().map(|h| h.range.clone()).collect(), current)
            }
            None => (
                highlight_ranges(&self.detail.segments[seg].text, self.tokens),
                None,
            ),
        }
    }
}

/// A scroll the list should perform: to a segment's row, "nearest"
/// (follow-along) or centred (deep link, Jump pill).
#[derive(Debug, Clone, Copy, PartialEq)]
struct ScrollReq {
    seg: usize,
    center: bool,
    animate: bool,
    /// Passes since the request was made.
    frames: u8,
}

impl ScrollReq {
    fn new(seg: usize, center: bool, animate: bool) -> Self {
        ScrollReq {
            seg,
            center,
            animate,
            frames: 0,
        }
    }

    /// The request for the next pass, if it still needs one. Animated and
    /// "nearest" scrolls are one-shot. A centred jump (deep link) re-aims
    /// until its row is laid out near the centre: heights around it go from
    /// estimated to measured, and the first passes after a load are still
    /// clamped to the previous content size.
    fn next(self, out: &ListOutput) -> Option<ScrollReq> {
        let settled = out
            .target_error
            .is_some_and(|e| e.abs() <= SCROLL_SETTLED_PX);
        if self.animate || !self.center || settled || self.frames >= SCROLL_SETTLE_FRAMES {
            None
        } else {
            Some(ScrollReq {
                frames: self.frames + 1,
                ..self
            })
        }
    }
}

#[derive(Debug, Default)]
struct ListOutput {
    actions: Vec<RowAction>,
    /// Rows laid out this pass (the rest are blank space).
    rendered: usize,
    /// For a centred request whose row was laid out: how far its centre is
    /// from the viewport centre this pass.
    target_error: Option<f32>,
}

/// One pass's layout inputs for the stand-in space.
struct ListPass {
    /// Every row's height (measured, else estimated) at pass start.
    heights: Vec<f32>,
    /// Rows to lay out wherever they are ([`RowList::editor_rows`]).
    editors: Vec<usize>,
    width: f32,
}

/// The virtualised segment + bookmark list with its height cache.
#[derive(Debug, Default)]
struct RowList {
    rows: Vec<Row>,
    keys: Vec<RowKey>,
    /// Whether each row starts a speaker turn ([`turn_starts`]).
    heads: Vec<bool>,
    /// Row index of each segment: its own row, or the hidden run standing
    /// in for it.
    seg_row: Vec<usize>,
    /// The rows wholly inside the viewport last pass (empty when none).
    visible: Range<usize>,
    /// The width each row was measured at and its outer height (incl. the
    /// gaps). Within `STALE_WIDTH` of the current width (the scroll bar
    /// fading in) a measurement beats the estimate; past it the text has
    /// reflowed and the estimate at the new width is the better guess,
    /// until the row is redrawn.
    heights: Vec<Option<(f32, f32)>>,
    /// The first row visible at the viewport top and how far its top sat
    /// above it, so a width change (rows reflow, estimates change) keeps
    /// the reader's place instead of drifting — CSS scroll anchoring.
    anchor: Option<(RowKey, f32)>,
    /// The width the rows were laid out at last pass.
    last_width: Option<f32>,
    /// Passes left in which a width change keeps re-applying `anchor`:
    /// rows first measured after the resize pass (estimated until then)
    /// would otherwise shift the reader by their estimate error.
    resize_settle: u8,
    /// A resize shift was issued last pass and has not been applied yet.
    shift_pending: bool,
}

impl RowList {
    /// Rebuild the rows for new data (or a new speaker filter: `shown`),
    /// keeping measured heights by key — but only for rows whose shape is
    /// unchanged: a bookmark added or removed before a row can make it
    /// start (or stop starting) a turn, which adds or drops its header
    /// line. A reading anchor whose row went (its line was hidden, or its
    /// hidden run was shown) moves to the row that holds that line now.
    fn rebuild(&mut self, detail: &MeetingDetail, shown: impl Fn(usize) -> bool) {
        let old: HashMap<RowKey, (RowShape, (f32, f32))> = (0..self.keys.len())
            .filter_map(|r| self.heights[r].map(|h| (self.keys[r], (self.shape(r), h))))
            .collect();
        self.rows = build_rows(detail, shown);
        self.heads = turn_starts(detail, &self.rows);
        self.keys = self
            .rows
            .iter()
            .map(|row| match *row {
                Row::Seg(i) => RowKey::Seg(detail.segments[i].id),
                Row::Bm(i) => RowKey::Bm(detail.bookmarks[i].id),
                Row::Hidden { first, .. } => RowKey::Hidden(detail.segments[first].id),
            })
            .collect();
        self.seg_row = vec![0; detail.segments.len()];
        for (r, row) in self.rows.iter().enumerate() {
            match *row {
                Row::Seg(i) => self.seg_row[i] = r,
                Row::Hidden { first, count } => self.seg_row[first..first + count].fill(r),
                Row::Bm(_) => {}
            }
        }
        self.heights = (0..self.keys.len())
            .map(|r| {
                old.get(&self.keys[r])
                    .filter(|(shape, _)| *shape == self.shape(r))
                    .map(|(_, h)| *h)
            })
            .collect();
        self.visible = 0..0;
        if let Some((key, _)) = self.anchor.filter(|(key, _)| !self.keys.contains(key)) {
            let seg_id = match key {
                RowKey::Seg(id) | RowKey::Hidden(id) => Some(id),
                RowKey::Bm(_) => None,
            };
            self.anchor = seg_id
                .and_then(|id| detail.segments.iter().position(|s| s.id == id))
                .map(|i| (self.keys[self.seg_row[i]], 0.0));
        }
    }

    /// The rows changed under the reader (the speaker filter): keep the
    /// row at the top of the view in place over the next passes, as after
    /// a resize.
    fn keep_reading_position(&mut self) {
        if self.anchor.is_some() {
            self.resize_settle = RESIZE_SETTLE_PASSES;
        }
    }

    /// The segment the reader is at: the first one in or after the row at
    /// the top of the view (0 while the list starts below the top).
    fn reading_segment(&self) -> usize {
        let Some(row) = self
            .anchor
            .and_then(|(key, _)| self.keys.iter().position(|k| *k == key))
        else {
            return 0;
        };
        self.rows[row..]
            .iter()
            .find_map(|row| match *row {
                Row::Seg(i) | Row::Hidden { first: i, .. } => Some(i),
                Row::Bm(_) => None,
            })
            .unwrap_or(0)
    }

    /// Whether segment `seg`'s row was wholly on screen last pass.
    fn seg_visible(&self, seg: usize) -> bool {
        self.seg_row
            .get(seg)
            .is_some_and(|r| self.visible.contains(r))
    }

    fn shape(&self, row: usize) -> RowShape {
        RowShape {
            head: self.heads[row],
            first: row == 0,
        }
    }

    fn height(&self, detail: &MeetingDetail, row: usize, width: f32) -> f32 {
        match self.heights[row] {
            Some((measured_at, h)) if (measured_at - width).abs() <= STALE_WIDTH => h,
            _ => estimate_height(detail, self.rows[row], self.shape(row), width),
        }
    }

    /// Rows holding an open inline editor, ascending. They are laid out
    /// even when scrolled far away: egui drops the focus of a widget that
    /// is not laid out, which would strand the editor (open, unfocused,
    /// never committing) and hand its keystrokes to the player keys.
    fn editor_rows(&self, rc: &RowCtx) -> Vec<usize> {
        let keys = [
            rc.speaker_edit.as_ref().map(|e| RowKey::Seg(e.seg_id)),
            rc.bookmark_edit.as_ref().map(|e| RowKey::Bm(e.id)),
        ];
        let mut rows: Vec<usize> = keys
            .into_iter()
            .flatten()
            .filter_map(|key| self.keys.iter().position(|k| *k == key))
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// Draw row `r` at the cursor and record its measured height.
    fn draw_row(
        &mut self,
        ui: &mut egui::Ui,
        rc: &mut RowCtx,
        r: usize,
        width: f32,
        actions: &mut Vec<RowAction>,
    ) -> f32 {
        let row_top = ui.cursor().top();
        let key = self.keys[r];
        let row = self.rows[r];
        let shape = self.shape(r);
        ui.push_id(key, |ui| match row {
            Row::Seg(i) => {
                if shape.head && !shape.first {
                    ui.add_space(TURN_GAP);
                }
                segment_row(ui, rc, i, shape.head, actions);
            }
            Row::Bm(i) => {
                if !shape.first {
                    ui.add_space(BOOKMARK_GAP);
                }
                bookmark_row(ui, rc, i, actions);
            }
            Row::Hidden { first, count } => {
                if !shape.first {
                    ui.add_space(HIDDEN_GAP);
                }
                hidden_row(ui, rc, first, count);
            }
        });
        ui.add_space(ROW_GAP);
        let h = ui.cursor().top() - row_top;
        self.heights[r] = Some((width, h));
        h
    }

    /// Stand-in space for the rows in `range`, except the editor rows among
    /// them, which are drawn in place.
    fn space_or_editors(
        &mut self,
        ui: &mut egui::Ui,
        rc: &mut RowCtx,
        range: std::ops::Range<usize>,
        pass: &ListPass,
        out: &mut ListOutput,
    ) {
        let mut from = range.start;
        for &r in pass.editors.iter().filter(|r| range.contains(r)) {
            let space: f32 = pass.heights[from..r].iter().sum();
            if space > 0.0 {
                ui.add_space(space);
            }
            self.draw_row(ui, rc, r, pass.width, &mut out.actions);
            out.rendered += 1;
            from = r + 1;
        }
        let space: f32 = pass.heights[from..range.end].iter().sum();
        if space > 0.0 {
            ui.add_space(space);
        }
    }

    /// Lay out the rows that intersect the viewport (plus overscan) and
    /// stand-in space for the rest; perform `req`'s scroll.
    fn show(&mut self, ui: &mut egui::Ui, rc: &mut RowCtx, req: Option<ScrollReq>) -> ListOutput {
        let mut out = ListOutput::default();
        let n = self.rows.len();
        let width = ui.available_width();
        let pass = ListPass {
            heights: (0..n).map(|r| self.height(rc.detail, r, width)).collect(),
            editors: self.editor_rows(rc),
            width,
        };
        let heights = &pass.heights;
        let clip = ui.clip_rect();
        let (lo, hi) = (clip.top() - OVERSCAN, clip.bottom() + OVERSCAN);
        let top = ui.cursor().top();
        let left = ui.cursor().left();
        let target = req.and_then(|r| self.seg_row.get(r.seg).copied());

        // Width changed: shift the scroll so the anchor row stays put (the
        // shift applies next pass; a scroll request repositions anyway).
        // Computed after drawing: the rows drawn this pass are re-measured
        // (reflowed), which the pass-start heights do not know yet.
        let resized = self.last_width.is_some_and(|w| (w - width).abs() > 0.5);
        self.last_width = Some(width);
        if resized {
            self.resize_settle = RESIZE_SETTLE_PASSES;
        }
        let resizing = self.resize_settle > 0;
        self.resize_settle = self.resize_settle.saturating_sub(1);
        if req.is_some() {
            // A scroll request repositions anyway; nothing to keep.
            self.resize_settle = 0;
        }
        // The pass after a shift still sees the old offset: skip it.
        let waiting = std::mem::take(&mut self.shift_pending);
        let resize_anchor = match (resizing && !waiting, req, self.anchor) {
            (true, None, Some((key, above))) => self
                .keys
                .iter()
                .position(|k| *k == key)
                .map(|row| (row, above)),
            _ => None,
        };

        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut first = 0;
            let mut y = top;
            while first < n && y + heights[first] < lo {
                y += heights[first];
                first += 1;
            }
            let mut target_rect = target.filter(|t| *t < first).map(|t| {
                let y0 = top + heights[..t].iter().sum::<f32>();
                Rect::from_min_size(egui::pos2(left, y0), egui::vec2(width, heights[t]))
            });
            self.space_or_editors(ui, rc, 0..first, &pass, &mut out);
            let mut anchor = None;
            let mut visible: Option<Range<usize>> = None;
            let mut next = first;
            while next < n && ui.cursor().top() < hi {
                let row_top = ui.cursor().top();
                let key = self.keys[next];
                let h = self.draw_row(ui, rc, next, width, &mut out.actions);
                if row_top <= clip.top() && row_top + h > clip.top() {
                    anchor = Some((key, clip.top() - row_top));
                }
                if row_top >= clip.top() - 0.5 && row_top + h - ROW_GAP <= clip.bottom() + 0.5 {
                    let start = visible.as_ref().map_or(next, |v| v.start);
                    visible = Some(start..next + 1);
                }
                if target == Some(next) {
                    let rect = Rect::from_min_size(
                        egui::pos2(left, row_top),
                        egui::vec2(width, h - ROW_GAP),
                    );
                    target_rect = Some(rect);
                    out.target_error = Some(rect.center().y - clip.center().y);
                }
                out.rendered += 1;
                next += 1;
            }
            let below_top = ui.cursor().top();
            if let Some(t) = target.filter(|t| *t >= next) {
                let y0 = below_top + heights[next..t].iter().sum::<f32>();
                target_rect = Some(Rect::from_min_size(
                    egui::pos2(left, y0),
                    egui::vec2(width, heights[t]),
                ));
            }
            self.space_or_editors(ui, rc, next..n, &pass, &mut out);
            if let Some((row, above)) = resize_anchor {
                // Where the anchor row lands next pass: the rows drawn this
                // pass count at their reflowed heights now.
                let y_now = top
                    + (0..row)
                        .map(|r| self.height(rc.detail, r, width))
                        .sum::<f32>();
                let shift = y_now - (clip.top() - above);
                if shift.abs() > 0.5 {
                    ui.scroll_with_delta_animation(
                        egui::vec2(0.0, -shift),
                        egui::style::ScrollAnimation::none(),
                    );
                    // egui applies it next pass: measure again after that.
                    self.shift_pending = true;
                }
            }
            // No anchor while the list starts below the viewport top (the
            // header is in view and reflows with the rest). Resize passes
            // (and the settle passes after them) keep the one they restore:
            // what they saw at the top was laid out before the shift.
            if !resizing {
                self.anchor = anchor;
            }
            self.visible = visible.unwrap_or(0..0);
            if let (Some(req), Some(rect)) = (req, target_rect) {
                let animation = if req.animate && icons::animations_enabled() {
                    ui.style().scroll_animation
                } else {
                    egui::style::ScrollAnimation::none()
                };
                let align = req.center.then_some(Align::Center);
                ui.scroll_to_rect_animation(rect, align, animation);
            }
        });
        out
    }
}

/// Font metrics shared by the row painters.
fn row_height(ui: &egui::Ui, font: &FontId) -> f32 {
    ui.fonts_mut(|f| f.row_height(font))
}

/// `text` on one line, truncated with `…` at `max_width`.
fn elided_galley(
    ui: &egui::Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width.max(10.0));
    ui.painter().layout_job(job)
}

/// A turn header's time (`m:ss`, 12 pt muted, brighter on hover): the
/// row's seek button, as tall as the header line and as wide as the time
/// with `0` digits, so headers line up whatever the digits.
fn ts_button(ui: &mut egui::Ui, ms: i64, label: &str) -> Response {
    let text = fmt_ts(ms);
    let width = player_bar::stable_width(ui, &text, theme::regular(12.0));
    let galley = ui
        .painter()
        .layout_no_wrap(text, theme::regular(12.0), pal().text_muted);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, HEADER_H), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if ui.is_rect_visible(rect) {
        let color = if response.hovered() {
            pal().text
        } else {
            pal().text_muted
        };
        let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
        ui.painter()
            .galley_with_override_text_color(pos, galley, color);
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            focus_ring(ui, rect, 4);
        }
    }
    response
}

fn focus_ring(ui: &egui::Ui, rect: Rect, radius: u8) {
    ui.painter().rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius),
        Stroke::new(2.0, pal().focus),
        egui::StrokeKind::Outside,
    );
}

/// Start a list row's frame (padding, radius 6); the fill is decided after
/// the content, once the row's rect is known.
fn begin_row(ui: &mut egui::Ui) -> egui::containers::frame::Prepared {
    let mut prepared = Frame::new()
        .inner_margin(Margin::symmetric(ROW_PAD_X as i8, ROW_PAD_Y as i8))
        .corner_radius(CornerRadius::same(theme::R_SM))
        .begin(ui);
    let content = &mut prepared.content_ui;
    content.set_min_width(content.available_width());
    content.spacing_mut().item_spacing = egui::vec2(8.0, HEADER_GAP);
    prepared
}

/// The outer rect of a row being drawn (content + padding).
fn row_rect(prepared: &egui::containers::frame::Prepared) -> Rect {
    prepared
        .content_ui
        .min_rect()
        .expand2(egui::vec2(ROW_PAD_X, ROW_PAD_Y))
}

/// A segment: the turn header when `head`, then the text (selectable; a
/// click that is not a drag selection plays from here), then the speaker
/// rename field while it is open on this row. Active = the accent tint.
fn segment_row(
    ui: &mut egui::Ui,
    rc: &mut RowCtx,
    idx: usize,
    head: bool,
    actions: &mut Vec<RowAction>,
) {
    let detail = rc.detail;
    let seg = &detail.segments[idx];
    let active = rc.active == Some(idx);
    let seek_label = format!("Play at {}", fmt_ts(seg.start_ms));
    let mut prepared = begin_row(ui);
    let mut seek = None;
    {
        let ui = &mut prepared.content_ui;
        if head {
            seek = Some(turn_header(ui, rc, seg, &seek_label, actions));
        }
        let (marks, current) = rc.marks(idx);
        let text = ui.add(
            egui::Label::new(text_job(&seg.text, &marks, current))
                .wrap()
                .selectable(true),
        );
        // No header, so no visible time: the tooltip has it.
        let text = if head {
            text
        } else {
            text.on_hover_text(format!("{} · click to play", fmt_ts(seg.start_ms)))
        };
        if text.clicked() {
            actions.push(RowAction::SeekPlay(idx));
        }
        let editing = rc
            .speaker_edit
            .as_ref()
            .is_some_and(|edit| edit.seg_id == seg.id);
        if editing {
            ui.add_space(4.0);
            speaker_editor(ui, rc, actions);
        }
    }
    let rect = row_rect(&prepared);
    if active {
        prepared.frame.fill = pal().active_seg_bg;
    } else if ui.rect_contains_pointer(rect) {
        prepared.frame.fill = pal().bg_hover;
    }
    let response = prepared.end(ui);
    if active {
        // The theme's outline for the active row: painted over the row,
        // never part of its layout (a frame stroke would grow the active
        // row and shift every row below).
        ui.painter().rect_stroke(
            response.rect,
            CornerRadius::same(theme::R_SM),
            Stroke::new(1.0, pal().active_seg_stroke),
            egui::StrokeKind::Inside,
        );
    }

    // A row without a header still needs a seek button for the keyboard
    // (Tab, a deep link's focus): a strip in the row's left padding,
    // beside the text and never over it (that would take the text's drag
    // selection). Its focus ring outlines the whole row.
    let seek = seek.unwrap_or_else(|| {
        let row = response.rect;
        let strip = Rect::from_min_max(
            row.left_top(),
            egui::pos2(row.left() + ROW_PAD_X - 2.0, row.bottom()),
        );
        let seek = ui.interact(strip, ui.id().with("seek"), Sense::click());
        seek.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &seek_label));
        if seek.has_focus() && widgets::focus_visible(ui.ctx()) {
            focus_ring(ui, row, theme::R_SM);
        }
        seek
    });
    if *rc.focus_seg == Some(seg.id) {
        // 0.2.x focused the hit's seek button: Tab continues from the hit
        // and Space/Enter plays from it.
        seek.request_focus();
        *rc.focus_seg = None;
    }
    if seek.clicked() {
        actions.push(RowAction::SeekPlay(idx));
    }
}

/// A turn's header line: the speaker's name in its colour — a button that
/// opens the rename when the segment has a speaker, `≈` marking an
/// auto-matched voice — and the time. Returns the time's response (the
/// row's seek button).
fn turn_header(
    ui: &mut egui::Ui,
    rc: &RowCtx,
    seg: &Segment,
    seek_label: &str,
    actions: &mut Vec<RowAction>,
) -> Response {
    let who = who(rc.detail, rc.speakers, seg);
    let name = if who.auto {
        format!("≈ {}", who.name)
    } else {
        who.name.to_string()
    };
    let tooltip = if who.auto {
        format!(
            "{} — auto-matched by voice, click to rename/confirm",
            who.name
        )
    } else {
        format!("{} — click to rename", who.name)
    };
    let width = ui.available_width();
    let ts_width = player_bar::stable_width(ui, &fmt_ts(seg.start_ms), theme::regular(12.0));
    ui.allocate_ui_with_layout(
        egui::vec2(width, HEADER_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let color = speaker_text(who.label);
            let max = width - ts_width - 8.0;
            match seg.speaker_id {
                Some(speaker_id) => {
                    let click = name_button(ui, &name, color, max, true);
                    click.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name)
                    });
                    let click = click.on_hover_text(tooltip);
                    if click.clicked() {
                        actions.push(RowAction::OpenSpeaker {
                            speaker_id,
                            seg_id: seg.id,
                        });
                    }
                }
                // No speaker to rename: the name only.
                None => {
                    let label = name_button(ui, &name, color, max, false);
                    label.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &name)
                    });
                    label.on_hover_text(tooltip);
                }
            }
            ts_button(ui, seg.start_ms, seek_label)
        },
    )
    .inner
}

/// A speaker's name (13 pt semibold, `color`), elided at `max_width`; when
/// `clickable`, a button with a soft tint of its colour on hover / focus.
fn name_button(
    ui: &mut egui::Ui,
    name: &str,
    color: Color32,
    max_width: f32,
    clickable: bool,
) -> Response {
    let galley = elided_galley(ui, name, theme::semibold(NAME_SIZE), color, max_width);
    let sense = if clickable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(galley.size().x, HEADER_H), sense);
    if ui.is_rect_visible(rect) {
        let ring = clickable && response.has_focus() && widgets::focus_visible(ui.ctx());
        // The tint reaches into the row padding, not into the time.
        let tint = rect.expand2(egui::vec2(4.0, -1.0));
        if clickable && (response.hovered() || ring) {
            ui.painter().rect_filled(
                tint,
                CornerRadius::same(theme::R_SM),
                theme::alpha(color, 0.14),
            );
        }
        let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(pos, galley, color);
        if ring {
            focus_ring(ui, tint, theme::R_SM);
        }
    }
    response
}

/// The rename field under a speaker's turn header, its hint and the people
/// suggestions ("a typo would fork a new person").
fn speaker_editor(ui: &mut egui::Ui, rc: &mut RowCtx, actions: &mut Vec<RowAction>) {
    let Some(state) = rc.speaker_edit.as_mut() else {
        return;
    };
    let seg_id = state.seg_id;
    // The field, then its hint on a line of its own (beside the field it
    // wrapped into a ragged column at narrow widths).
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 4.0);
        let id = Id::new(("speaker-rename", seg_id));
        // `Rename ${display_name ?? label}`, as the header shows it.
        let name = speaker_of(rc.detail, rc.speakers, Some(state.speaker_id))
            .map(|s| {
                if s.display_name.is_empty() {
                    s.label.as_str()
                } else {
                    s.display_name.as_str()
                }
            })
            .unwrap_or("speaker");
        let width = (ui.available_width() - 2.0 * f32::from(INLINE_EDIT_MARGIN.left) - 2.0)
            .clamp(80.0, 280.0);
        let (field, outcome) = inline_editor(
            ui,
            id,
            &mut state.edit,
            theme::regular(TEXT_SIZE),
            width,
            "",
            &format!("Rename {name}"),
        );
        let shown = state.edit.text.trim();
        let hint = format!(
            "Saves this voice as {} for future meetings",
            if shown.is_empty() { "this name" } else { shown }
        );
        ui.add(
            egui::Label::new(egui::RichText::new(hint).size(12.0).color(pal().text_muted)).wrap(),
        );

        // A press on the suggestion list blurs the field first; that blur
        // must not commit the half-typed text. It is remembered instead:
        // if the press ends without a pick, the edit commits then.
        let into_suggestions = ui.input(|i| {
            i.pointer
                .interact_pos()
                .is_some_and(|p| state.suggest_rect.is_some_and(|r| r.contains(p)))
        });
        match outcome {
            EditOutcome::Commit if into_suggestions => state.blur_swallowed = true,
            EditOutcome::Open => {}
            outcome => actions.push(RowAction::SpeakerDone { seg_id, outcome }),
        }
        if field.has_focus() {
            state.blur_swallowed = false;
        }

        // Only while the field is on screen: a focused editor in a row
        // scrolled away must not float its list over the rest of the view.
        let names = suggestions(rc.people, &state.edit.text);
        let on_screen = ui.clip_rect().intersects(field.rect);
        state.suggest_rect = None;
        let mut picked = false;
        if !names.is_empty() && on_screen && (field.has_focus() || into_suggestions) {
            let area = egui::Area::new(id.with("suggestions"))
                .order(Order::Foreground)
                .fixed_pos(field.rect.left_bottom() + egui::vec2(0.0, 4.0))
                .show(ui.ctx(), |ui| {
                    menu_frame(ui).show(ui, |ui| {
                        ui.set_min_width(field.rect.width());
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for name in names {
                            if menu_item(ui, name).clicked() {
                                picked = true;
                                actions.push(RowAction::SpeakerPick {
                                    seg_id,
                                    name: name.to_string(),
                                });
                            }
                        }
                    });
                });
            state.suggest_rect = Some(area.response.rect);
        }

        // The press on the list was dragged off it and released elsewhere:
        // no pick will come, and the field cannot lose focus again.
        let released = !ui.input(|i| i.pointer.any_down());
        if state.blur_swallowed && !picked && released && !field.has_focus() {
            state.blur_swallowed = false;
            actions.push(RowAction::SpeakerDone {
                seg_id,
                outcome: EditOutcome::Commit,
            });
        }
    });
}

/// A bookmark: its seek button (warn glyph + time), the note (a button
/// that edits it, or the open editor) and the remove button, on a faint
/// warn tint.
fn bookmark_row(ui: &mut egui::Ui, rc: &mut RowCtx, idx: usize, actions: &mut Vec<RowAction>) {
    let detail = rc.detail;
    let bookmark = &detail.bookmarks[idx];
    let mut prepared = begin_row(ui);
    {
        let ui = &mut prepared.content_ui;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let ts = fmt_ts(bookmark.at_ms);
            if bookmark_seek(ui, &ts).clicked() {
                actions.push(RowAction::BookmarkSeek(bookmark.at_ms));
            }
            let trash_width = BOOKMARK_LINE;
            let note_width = (ui.available_width() - trash_width - 8.0).max(60.0);
            let editing = rc
                .bookmark_edit
                .as_ref()
                .is_some_and(|edit| edit.id == bookmark.id);
            ui.allocate_ui_with_layout(
                egui::vec2(note_width, BOOKMARK_LINE),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_min_width(note_width);
                    if editing {
                        if let Some(state) = rc.bookmark_edit.as_mut() {
                            let (_, outcome) = inline_editor(
                                ui,
                                Id::new(("bookmark-note", bookmark.id)),
                                &mut state.edit,
                                theme::regular(TEXT_SIZE),
                                (note_width - 18.0).max(60.0),
                                "what happened here?",
                                "Bookmark note",
                            );
                            if outcome != EditOutcome::Open {
                                actions.push(RowAction::BookmarkDone {
                                    id: bookmark.id,
                                    outcome,
                                });
                            }
                        }
                    } else {
                        // The note's first line centred on the 28 pt line.
                        ui.add_space((BOOKMARK_LINE - LINE_HEIGHT) / 2.0);
                        bookmark_note(ui, bookmark, actions);
                    }
                },
            );
            let trash = widgets::icon_btn_danger(ui, ph::TRASH, 14.0, "Remove bookmark");
            trash.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    format!("Remove bookmark at {ts}"),
                )
            });
            if trash.clicked() {
                actions.push(RowAction::DeleteBookmark(bookmark.id));
            }
        });
    }
    let rect = row_rect(&prepared);
    prepared.frame.fill = if ui.rect_contains_pointer(rect) {
        pal().bg_hover
    } else {
        pal().bookmark_row_bg
    };
    prepared.end(ui);
}

/// A bookmark's seek button: the warn bookmark glyph and the time (12 pt
/// muted, brighter on hover), on the row's 28 pt line.
fn bookmark_seek(ui: &mut egui::Ui, ts: &str) -> Response {
    let glyph = ui.painter().layout_no_wrap(
        ph::BOOKMARK_SIMPLE.to_owned(),
        theme::regular(15.0),
        pal().warn,
    );
    let time = ui
        .painter()
        .layout_no_wrap(ts.to_owned(), theme::regular(12.0), pal().text_muted);
    let time_width = player_bar::stable_width(ui, ts, theme::regular(12.0));
    let size = egui::vec2(glyph.size().x + 6.0 + time_width, BOOKMARK_LINE);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let label = format!("Play recording at bookmark {ts}");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let cy = rect.center().y;
        let time_x = rect.left() + glyph.size().x + 6.0;
        let time_y = cy - time.size().y / 2.0;
        painter.galley(
            egui::pos2(rect.left(), cy - glyph.size().y / 2.0),
            glyph,
            pal().warn,
        );
        let color = if response.hovered() {
            pal().text
        } else {
            pal().text_muted
        };
        painter.galley_with_override_text_color(egui::pos2(time_x, time_y), time, color);
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            focus_ring(ui, rect, 4);
        }
    }
    response
}

/// The stand-in for `count` lines the speaker filter hides: a faint
/// centred "n lines from others" between hairlines. While playback is in
/// those lines it takes the active row's tint (follow-along scrolls to it).
fn hidden_row(ui: &mut egui::Ui, rc: &RowCtx, first: usize, count: usize) {
    let active = rc
        .active
        .is_some_and(|a| (first..first + count).contains(&a));
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), HIDDEN_LINE),
        Sense::hover(),
    );
    let text = if count == 1 {
        "1 line from others".to_owned()
    } else {
        format!("{} lines from others", fmt_thousands(count as i64))
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, CornerRadius::same(theme::R_SM), pal().active_seg_bg);
    }
    let color = if active {
        pal().text_muted
    } else {
        pal().text_faint
    };
    let galley = painter.layout_no_wrap(text, theme::regular(12.0), color);
    let cx = rect.center().x;
    let half = galley.size().x / 2.0 + theme::S2;
    painter.galley(
        egui::pos2(
            cx - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );
    let stroke = Stroke::new(1.0, pal().border);
    let inset = rect.left() + ROW_PAD_X;
    if cx - half > inset {
        painter.hline(inset..=cx - half, rect.center().y, stroke);
        painter.hline(
            cx + half..=rect.right() - ROW_PAD_X,
            rect.center().y,
            stroke,
        );
    }
}

/// The note as a button (a faint placeholder when empty); a click edits it.
fn bookmark_note(ui: &mut egui::Ui, bookmark: &Bookmark, actions: &mut Vec<RowAction>) {
    let (text, color) = if bookmark.note.is_empty() {
        ("bookmark — click to add a note", pal().text_faint)
    } else {
        (bookmark.note.as_str(), pal().text)
    };
    let job = egui::text::LayoutJob::single_section(
        text.to_string(),
        egui::TextFormat {
            font_id: theme::regular(TEXT_SIZE),
            color,
            line_height: Some(LINE_HEIGHT),
            ..Default::default()
        },
    );
    let label = ui.add(egui::Label::new(job).wrap());
    let full = Rect::from_min_size(
        label.rect.min,
        egui::vec2(
            ui.available_width().max(label.rect.width()),
            label.rect.height(),
        ),
    );
    let click = ui.interact(full, ui.id().with("note"), Sense::click());
    click.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
    if click.has_focus() && widgets::focus_visible(ui.ctx()) {
        focus_ring(ui, full, 4);
    }
    let click = click
        .on_hover_cursor(egui::CursorIcon::Text)
        .on_hover_text("Click to edit note");
    if click.clicked() {
        actions.push(RowAction::EditBookmark(bookmark.id));
    }
}

// ---------------------------------------------------------------------------
// Menus, pill

/// The header's icon buttons: an 18 pt glyph in a 30 pt ghost square.
const HEADER_ICON: f32 = 18.0;

/// A popup menu: raised fill, 1 pt border, radius 8, popup shadow, 4 pt
/// inset around its rounded items.
fn menu_frame(ui: &egui::Ui) -> Frame {
    Frame::popup(ui.style())
        .inner_margin(Margin::same(4))
        .fill(pal().bg_raised)
        .stroke(Stroke::new(1.0, pal().border))
        .corner_radius(CornerRadius::same(theme::R_MD))
}

/// A menu item: 13 pt, 8/10 padding, full width, rounded hover fill.
fn menu_item(ui: &mut egui::Ui, text: &str) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::regular(13.0), pal().text);
    let width = ui.available_width().max(galley.size().x + 20.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, galley.size().y + 16.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
    if ui.is_rect_visible(rect) {
        // Hover only: the first item's programmatic focus on open must not
        // leave a second highlighted row after a mouse open.
        if response.hovered() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(theme::R_SM), pal().bg_hover);
        }
        let pos = egui::pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(pos, galley, pal().text);
        if response.has_focus() && widgets::focus_visible(ui.ctx()) {
            // The global `:focus-visible` outline, kept inside the popup.
            ui.painter().rect_stroke(
                rect.shrink(2.0),
                CornerRadius::same(4),
                Stroke::new(2.0, pal().focus),
                egui::StrokeKind::Inside,
            );
        }
    }
    response
}

/// A header menu: an icon button toggling a right-aligned popup whose
/// first item takes focus on open; Escape closes it and refocuses the
/// trigger. Opening one menu closes the other (egui keeps one popup open).
/// `open` opens it this pass (debug snapshots). Returns the index of the
/// chosen item.
fn header_menu(
    ui: &mut egui::Ui,
    glyph: &str,
    tooltip: &str,
    label: &str,
    enabled: bool,
    items: &[&str],
    open: bool,
) -> Option<usize> {
    // A fixed id: the popup is keyed by the trigger's, and auto ids in the
    // scroller shift when the player bar appears, which closed an open menu.
    let scope = egui::UiBuilder::new().id(Id::new(("transcript-menu", label)));
    let trigger = ui
        .scope_builder(scope, |ui| {
            ui.add_enabled_ui(enabled, |ui| {
                widgets::icon_btn(ui, glyph, HEADER_ICON, tooltip)
            })
            .inner
        })
        .inner;
    trigger.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let ctx = ui.ctx().clone();
    let popup_id = Popup::default_response_id(&trigger);
    let was_open = Popup::is_id_open(&ctx, popup_id);
    if open && enabled {
        Popup::open_id(&ctx, popup_id);
    }
    let mut chosen = None;
    Popup::menu(&trigger)
        .align(RectAlign::BOTTOM_END)
        .gap(4.0)
        .width(180.0)
        .frame(menu_frame(ui))
        .show(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut ids = Vec::with_capacity(items.len());
            for (index, text) in items.iter().enumerate() {
                let item = menu_item(ui, text);
                if index == 0 && !was_open {
                    item.request_focus();
                }
                if item.clicked() {
                    chosen = Some(index);
                }
                ids.push(item.id);
            }
            menu_keys(ui, &ids);
        });
    if chosen.is_some() {
        // Space/Enter on a focused item is not a pointer click: close here.
        Popup::close_id(&ctx, popup_id);
    } else if was_open
        && !Popup::is_id_open(&ctx, popup_id)
        && ctx.input(|i| i.key_pressed(Key::Escape))
    {
        trigger.request_focus();
    }
    chosen
}

/// Where an in-menu key moves focus from item `at` of `len`
/// (`handleMenuKey`): ↓/↑ cycle with wrap, Home/End jump to the ends.
fn menu_key_target(key: Key, at: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match key {
        Key::ArrowDown => Some((at + 1) % len),
        Key::ArrowUp => Some((at + len - 1) % len),
        Key::Home => Some(0),
        Key::End => Some(len - 1),
        _ => None,
    }
}

/// The in-menu keys while one of `ids` has focus. The arrows are consumed
/// and egui's own arrow navigation is cancelled for the pass (it searches
/// every layer, so it would carry focus out of the open menu).
fn menu_keys(ui: &mut egui::Ui, ids: &[Id]) {
    let Some(at) = ids.iter().position(|id| ui.memory(|m| m.has_focus(*id))) else {
        return;
    };
    let pressed = ui.input_mut(|i| {
        [
            Key::ArrowDown,
            Key::ArrowUp,
            Key::Home,
            Key::End,
            Key::ArrowLeft,
            Key::ArrowRight,
        ]
        .into_iter()
        .filter(|key| i.consume_key(Modifiers::NONE, *key))
        .collect::<Vec<_>>()
    });
    if pressed.is_empty() {
        return;
    }
    let target = pressed
        .iter()
        .rev()
        .find_map(|key| menu_key_target(*key, at, ids.len()));
    ui.memory_mut(|m| {
        // ←/→ do nothing in the menu (and must not leave it either).
        m.move_focus(egui::FocusDirection::None);
        if let Some(target) = target {
            m.request_focus(ids[target]);
        }
    });
}

/// The floating "Jump to current" pill (`.jump-pill`).
fn jump_pill(ctx: &egui::Context, id: Id, anchor: egui::Pos2) -> bool {
    egui::Area::new(id)
        .order(Order::Foreground)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(anchor)
        .show(ctx, |ui| {
            let galley = ui.painter().layout_no_wrap(
                "Jump to current".to_owned(),
                theme::regular(12.0),
                pal().text,
            );
            let size = galley.size() + egui::vec2(24.0, 10.0);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Jump to current")
            });
            let hot = response.hovered() || response.has_focus();
            let radius = CornerRadius::same(255);
            let painter = ui.painter();
            // The theme's popup shadow (much lighter in the light theme).
            painter.add(ui.visuals().popup_shadow.as_shape(rect, radius));
            painter.rect(
                rect,
                radius,
                if hot { pal().bg_hover } else { pal().bg_raised },
                Stroke::new(1.0, if hot { pal().accent } else { pal().border }),
                egui::StrokeKind::Inside,
            );
            painter.galley(rect.center() - galley.size() / 2.0, galley, pal().text);
            response.clicked()
        })
        .inner
}

// ---------------------------------------------------------------------------
// The view

/// The archived audio for the player.
enum Audio {
    /// No `audio_path` yet (recording, or not encoded yet).
    None,
    Resolving(Task<CoreResult<PathBuf>>),
    Ready(PathBuf),
    Failed(String),
}

/// What the player can do for this meeting this frame.
#[derive(Debug, Clone, PartialEq)]
enum Playback {
    /// The player has our file (loading or ready): the bar shows.
    Playable,
    /// "Audio playback is unavailable: …".
    Failed(String),
    /// Nothing to play (yet).
    Unavailable,
}

/// Which control last started playback: the failure toasts differ
/// (map §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayOrigin {
    /// The player bar's Play button or the Space key (player.tsx).
    Bar,
    /// A segment's timestamp/text or a bookmark's time (transcript.tsx).
    Seek,
}

impl PlayOrigin {
    fn failure(self) -> &'static str {
        match self {
            PlayOrigin::Bar => "Could not play recording",
            PlayOrigin::Seek => "Could not play the recording",
        }
    }
}

/// Why the last `play()` failed, taken once.
fn take_play_error(player: &witness_core::player::Player) -> Option<String> {
    player.take_play_error()
}

/// The transcript view. The state is boxed: it is far larger than the
/// other views, and `ActiveView` holds one of them inline.
pub struct TranscriptView(Box<TranscriptState>);

impl TranscriptView {
    pub fn new(meeting_id: i64, focus_segment_id: Option<i64>, highlight: Option<String>) -> Self {
        TranscriptView(Box::new(TranscriptState::new(
            meeting_id,
            focus_segment_id,
            highlight,
        )))
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        self.0.show(ui, cx);
    }

    /// Persist anything unsaved WITHOUT tearing down: hand the newest notes
    /// draft to the ordered writer. Never blocks; the player keeps playing.
    pub fn flush(&mut self, cx: &mut UiCtx) {
        self.0.flush(cx);
    }

    /// `flush()`, save an open bookmark note, and release the audio device
    /// (the view is being replaced).
    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        self.0.leave(cx);
        cx.player.unload();
    }
}

struct TranscriptState {
    meeting_id: i64,
    focus_segment_id: Option<i64>,
    highlight: Option<String>,
    /// `highlight_tokens(highlight)`.
    tokens: Vec<String>,
    /// Debug builds: `WITNESS_DEBUG_SYNTHETIC=1` shows a generated
    /// 3,000-segment meeting instead of the database's.
    synthetic: bool,

    detail: Option<MeetingDetail>,
    load: Option<Task<CoreResult<MeetingDetail>>>,
    /// `notes_acks[meeting_id]` when `load` was spawned (see `Notes::load`).
    load_ack: Option<(u64, Option<String>)>,
    load_error: Option<String>,
    /// `refresh_tick` the current load answers.
    seen_refresh: Option<u64>,
    /// Bumped by "Try again" (re-resolves the audio too).
    reload_gen: u64,
    speakers: HashMap<i64, usize>,
    stats: Vec<SpeakerStat>,
    list: RowList,
    /// The speakers whose lines show (empty: everyone).
    filter: BTreeSet<SpeakerKey>,
    /// A legend / proportion-bar click this pass (the speaker, Ctrl held),
    /// or the filter chip's Clear (`None`): applied after the list is
    /// drawn, so an editor blurred by the click commits first.
    filter_click: Option<Option<(SpeakerKey, bool)>>,
    /// Ctrl+F's find bar while open.
    find: Option<FindState>,
    /// Bumped whenever the rows are rebuilt (new data, the filter): find
    /// recomputes its hits.
    data_gen: u64,
    /// Who spoke when, for the scrubber.
    timeline: Timeline,

    audio: Audio,
    /// The `(audio_path, reload_gen)` the audio state belongs to.
    audio_key: Option<(String, u64)>,
    /// Deep link: position the audio here once it is loaded.
    pending_seek_ms: Option<u64>,

    title_edit: Option<LineEdit>,
    speaker_edit: Option<SpeakerEdit>,
    bookmark_edit: Option<BookmarkEdit>,
    people: Vec<PersonInfo>,
    people_task: Option<Task<CoreResult<Vec<PersonInfo>>>>,
    copy_task: Option<Task<CoreResult<String>>>,
    copied_until: Option<Instant>,
    /// Bookmarks added through the player bar: (id, at_ms), sent by the op.
    bookmark_tx: Sender<(i64, i64)>,
    bookmark_rx: Receiver<(i64, i64)>,

    notes: Notes,

    /// The highlighted segment (follows playback; set by seeks/deep links).
    active: Option<usize>,
    /// Follow-along auto-scroll; suspended when the user scrolls.
    follow: bool,
    /// The (generation, position) the active row was last computed for.
    last_position: Option<(u64, u64)>,
    focused_once: bool,
    /// The deep-linked segment whose seek button gets keyboard focus once
    /// the centring scroll has settled.
    focus_pending: Option<i64>,
    scroll_req: Option<ScrollReq>,
    /// The scroll area's content viewport last frame (Jump pill anchor).
    scroll_inner: Option<Rect>,
    /// The whole scroll area last frame, scroll bar included (wheel
    /// detection).
    scroll_outer: Option<Rect>,
    /// Where the vertical scroll bar was last frame, while it showed.
    scroll_bar: Option<Rect>,
    first_frame: bool,
    /// Some widget had keyboard focus at the end of the last pass (an Esc
    /// that blurs it is not one that closes find).
    focus_last_pass: bool,
    player_bar: PlayerBarState,
    /// Picks the toast when the player reports a play failure.
    play_origin: PlayOrigin,
    /// The live-caption feed's stick-to-bottom tracking.
    live_feed: LiveFeedState,
    /// The Summary disclosure; None until the first load decides (open
    /// when there is a summary).
    summary_open: Option<bool>,
    /// An AI summary task was showing last pass (a new one opens the panel).
    summary_seen_task: bool,
    /// The summary parsed once per text (not every frame).
    summary_md: ai_summary::MdCache,
    /// Debug builds: snapshot states asked for by environment variables.
    debug: DebugHooks,
}

impl TranscriptState {
    fn new(meeting_id: i64, focus_segment_id: Option<i64>, highlight: Option<String>) -> Self {
        let (bookmark_tx, bookmark_rx) = crossbeam_channel::unbounded();
        let debug = debug_hooks();
        // Tokens only: `highlight` must keep matching the view's, or
        // `sync_view_args` would reset them.
        let tokens_from = highlight.as_deref().or(debug.highlight.as_deref());
        TranscriptState {
            meeting_id,
            focus_segment_id,
            tokens: tokens_from.map(highlight_tokens).unwrap_or_default(),
            highlight,
            synthetic: synthetic_enabled(),
            detail: None,
            load: None,
            load_ack: None,
            load_error: None,
            seen_refresh: None,
            reload_gen: 0,
            speakers: HashMap::new(),
            stats: Vec::new(),
            list: RowList::default(),
            filter: BTreeSet::new(),
            filter_click: None,
            find: None,
            data_gen: 0,
            timeline: Timeline::default(),
            audio: Audio::None,
            audio_key: None,
            pending_seek_ms: None,
            title_edit: None,
            speaker_edit: None,
            bookmark_edit: None,
            people: Vec::new(),
            people_task: None,
            copy_task: None,
            copied_until: None,
            bookmark_tx,
            bookmark_rx,
            notes: Notes::default(),
            active: None,
            follow: true,
            last_position: None,
            focused_once: false,
            focus_pending: None,
            scroll_req: None,
            scroll_inner: None,
            scroll_outer: None,
            scroll_bar: None,
            first_frame: true,
            focus_last_pass: false,
            player_bar: PlayerBarState::with_volume_open(debug.volume),
            play_origin: PlayOrigin::Bar,
            live_feed: LiveFeedState::default(),
            summary_open: None,
            summary_seen_task: false,
            summary_md: ai_summary::MdCache::default(),
            debug,
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let ctx = ui.ctx().clone();
        self.sync_view_args(cx);
        self.poll(&ctx, cx);

        if let Some(error) = self.load_error.clone() {
            let message = format!("Could not load meeting: {error}");
            let choice = widgets::padded_column(ui, |ui| {
                widgets::view_error(ui, &message, &["Back", "Try again"])
            });
            match choice {
                Some(0) => self.back(cx),
                Some(_) => self.reload(cx, true),
                None => {}
            }
            return;
        }
        let layout = header_layout(self.pane(cx));
        if self.detail.is_none() || self.debug.loading {
            widgets::padded_column(ui, |ui| loading_skeleton(ui, layout));
            return;
        }

        let snap = cx.player.snapshot();
        let playback = self.playback(&snap);
        let playable = playback == Playback::Playable;
        // Before the player keys: Enter in the find field steps through
        // the hits (and must not reach anything else).
        self.find_keys(&ctx, cx);
        self.update_find();
        if playable {
            self.track_position(&snap);
            self.player_keys(&ctx, cx);
        }
        self.detect_user_scroll(&ctx);

        // The player bar, docked under the scrolling content. It lines up
        // with the text column, which last pass's scroll bar narrowed.
        let right = ui.max_rect().right();
        let gutter = self
            .scroll_inner
            .map_or(0.0, |inner| (right - inner.right()).clamp(0.0, 20.0));
        if playable {
            let mut bar_out = player_bar::PlayerBarOutput::default();
            let detail = self.detail.as_ref().expect("checked above");
            let (timeline, bar_state) = (&self.timeline, &mut self.player_bar);
            egui::Panel::bottom(Id::new(("transcript-player", self.meeting_id)))
                .frame(player_bar::bar_frame())
                .resizable(false)
                .show_separator_line(false)
                .show(ui, |ui| {
                    bar_out = player_bar::show(ui, cx, detail, timeline, bar_state, gutter);
                });
            if bar_out.toggled {
                self.play_origin = PlayOrigin::Bar;
            }
            if let Some(at_ms) = bar_out.add_bookmark_at_ms {
                self.add_bookmark(cx, at_ms);
            }
        }

        // The find bar and the speaker filter's chip, docked over the
        // transcript column: they stay put while the lines scroll by, and
        // side by side they say that find searches the shown lines only.
        let filtering = !self.filter.is_empty();
        if self.find.is_some() || filtering {
            let field = self.find_field_id();
            let names = filter_names(&self.stats, &self.filter);
            let find = &mut self.find;
            let (mut outcome, mut clear) = (FindOutcome::None, false);
            egui::Panel::top(Id::new(("transcript-find-bar", self.meeting_id)))
                .frame(Frame::new().fill(pal().bg).inner_margin(Margin {
                    left: 0,
                    right: 0,
                    top: 8,
                    bottom: 4,
                }))
                .resizable(false)
                .show_separator_line(false)
                .show(ui, |ui| {
                    player_bar::bar_column(ui, gutter, |ui| {
                        ui.spacing_mut().item_spacing.y = theme::S2;
                        if let Some(find) = find.as_mut() {
                            outcome = find_bar(ui, find, field);
                        }
                        if filtering {
                            clear = filter_chip(ui, &names).clicked();
                        }
                    });
                });
            if clear {
                self.filter_click = Some(None);
            }
            match outcome {
                FindOutcome::None => {}
                FindOutcome::Step { back } => self.step_find(back),
                FindOutcome::Close => self.close_find(&ctx),
            }
        }

        let mut scroll = egui::ScrollArea::vertical()
            .id_salt(("transcript-scroll", self.meeting_id))
            .auto_shrink(false);
        if std::mem::take(&mut self.first_frame) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        let mut list_out = ListOutput::default();
        // Like `widgets::scroll_column`, but `show_viewport` (the list
        // virtualises against the clip rect itself).
        let output = scroll.show_viewport(ui, |ui, _viewport| {
            widgets::keyboard_scroll(ui, KeyScroll { space: false });
            widgets::padded_column(ui, |ui| {
                list_out = self.content(ui, cx, &playback);
            });
            ui.add_space(theme::S4);
        });
        self.record_scroll_rects(ui, &output);
        self.settle_scroll(&ctx, &list_out);
        self.apply_actions(cx, playable, std::mem::take(&mut list_out.actions));
        self.apply_filter_click();
        if let Some(error) = take_play_error(cx.player) {
            cx.toasts.error(self.play_origin.failure(), error);
        }

        if !self.follow && playable && self.active.is_some() {
            let anchor = egui::pos2(
                output.inner_rect.center().x,
                output.inner_rect.bottom() - 12.0,
            );
            if jump_pill(&ctx, Id::new(("jump-pill", self.meeting_id)), anchor) {
                self.follow = true;
                if let Some(seg) = self.active {
                    self.scroll_req = Some(ScrollReq::new(seg, true, true));
                }
            }
        }

        self.focus_last_pass = ctx.memory(|m| m.focused().is_some());
        if snap.playing && cx.window.is_shown() && !cx.window.is_minimized() {
            ctx.request_repaint_after(PLAY_FRAME);
        }
        if let Some(until) = self.copied_until {
            let now = Instant::now();
            if now >= until {
                self.copied_until = None;
            } else {
                ctx.request_repaint_after(until - now);
            }
        }
    }

    /// Hand the newest notes draft to the ordered writer (never blocks).
    fn flush(&mut self, cx: &mut UiCtx) {
        let acked = self.acked(cx);
        if let Some(text) = self.notes.take_flush(acked) {
            if self.synthetic {
                // Debug data: never write it over a real meeting's notes.
                self.notes.confirmed = text;
            } else {
                let seq = cx.core.queue_notes(self.meeting_id, text.clone());
                self.notes.sent(seq);
                let started_at = self
                    .detail
                    .as_ref()
                    .map(|d| d.meeting.started_at.clone())
                    .unwrap_or_default();
                cx.memory.queued_notes.insert(
                    self.meeting_id,
                    QueuedNotes {
                        started_at,
                        seq,
                        text,
                    },
                );
            }
        }
        cx.set_notes_deadline(None);
    }

    /// The view is going: an open bookmark-note editor saves, as its blur
    /// would. A half-typed speaker rename is dropped: renaming enrols a
    /// voice print, so it takes Enter, a pick or a click elsewhere.
    fn leave(&mut self, cx: &mut UiCtx) {
        self.flush(cx);
        if let Some(state) = self.bookmark_edit.take() {
            self.commit_bookmark(cx, state);
        }
    }

    // ------------------------------------------------------------------
    // Loading

    fn acked(&self, cx: &UiCtx) -> u64 {
        cx.shell
            .notes_acks
            .get(&self.meeting_id)
            .map_or(0, |(seq, _)| *seq)
    }

    /// A re-navigation to this meeting with another deep link keeps the
    /// view (same key): reset the deep-link state like the TSX effect on
    /// `[meetingId, focusSegmentId]`.
    fn sync_view_args(&mut self, cx: &UiCtx) {
        let View::Transcript {
            segment_id,
            highlight,
            ..
        } = cx.view
        else {
            return;
        };
        if *highlight != self.highlight {
            self.highlight = highlight.clone();
            self.tokens = highlight
                .as_deref()
                .map(highlight_tokens)
                .unwrap_or_default();
        }
        if *segment_id != self.focus_segment_id {
            self.focus_segment_id = *segment_id;
            self.focused_once = false;
            self.focus_pending = None;
            self.pending_seek_ms = None;
            self.active = None;
            self.follow = true;
            self.deep_link();
        }
    }

    fn reload(&mut self, cx: &mut UiCtx, try_again: bool) {
        if try_again {
            self.reload_gen += 1;
            self.load_error = None;
        }
        let id = self.meeting_id;
        // Before the read starts: an ack seen now is for a write the read
        // will see.
        self.load_ack = cx.shell.notes_acks.get(&id).cloned();
        self.load = Some(if self.synthetic {
            cx.tasks
                .spawn("meeting-detail", move |_| Ok(synthetic_detail(id, 3_000)))
        } else {
            cx.tasks
                .spawn("meeting-detail", move |core| core.meeting_detail(id))
        });
    }

    fn poll(&mut self, ctx: &egui::Context, cx: &mut UiCtx) {
        // (Re)load on mount and on every refresh tick; the old detail stays
        // on screen meanwhile.
        if self.seen_refresh != Some(cx.shell.refresh_tick) {
            self.seen_refresh = Some(cx.shell.refresh_tick);
            self.reload(cx, false);
        }
        if let Some(result) = self.load.as_mut().and_then(Task::poll_flat) {
            self.load = None;
            match result {
                Ok(detail) => self.set_detail(ctx, cx, detail),
                Err(error) => {
                    cx.toasts.error("Could not load the meeting", &error);
                    self.detail = None;
                    self.load_error = Some(error);
                }
            }
        }

        if let Some(result) = self.people_task.as_mut().and_then(Task::poll_flat) {
            self.people_task = None;
            if let Ok(people) = result {
                cx.fonts
                    .note_texts(ctx, people.iter().map(|p| p.name.as_str()));
                self.people = people;
            }
        }

        if let Some(result) = self.copy_task.as_mut().and_then(Task::poll_flat) {
            self.copy_task = None;
            match result {
                Ok(text) => {
                    ctx.copy_text(text);
                    cx.toasts.success("Transcript copied to the clipboard");
                    self.copied_until = Some(Instant::now() + COPIED_FOR);
                }
                Err(error) => cx.toasts.error("Could not copy the transcript", error),
            }
        }

        // Bookmarks added from the player bar: show the row and open its
        // note editor at once (the reload from MeetingsChanged follows).
        while let Ok((id, at_ms)) = self.bookmark_rx.try_recv() {
            if let Some(detail) = self.detail.as_mut() {
                if !detail.bookmarks.iter().any(|b| b.id == id) {
                    detail.bookmarks.push(Bookmark {
                        id,
                        meeting_id: self.meeting_id,
                        at_ms,
                        note: String::new(),
                    });
                    self.rebuild_list();
                }
                self.bookmark_edit = Some(BookmarkEdit {
                    id,
                    edit: LineEdit::new(""),
                });
            }
        }

        // The ack only drives the indicator and the retry: the shell toasts
        // "Could not save notes" for every failed write (`ShellModel::apply`),
        // including one queued by `on_leave` after this view was gone.
        let _ = self.notes.on_ack(cx.shell.notes_acks.get(&self.meeting_id));

        self.poll_audio(cx);
    }

    fn set_detail(&mut self, ctx: &egui::Context, cx: &mut UiCtx, mut detail: MeetingDetail) {
        self.debug.ai.apply(&mut detail.meeting);
        if self.summary_open.is_none() {
            // Open when there is a summary (or a debug state to show).
            self.summary_open =
                Some(!detail.meeting.ai_summary.trim().is_empty() || self.debug.ai.state.is_some());
        }
        cx.fonts.note_texts(
            ctx,
            std::iter::once(detail.meeting.title.as_str())
                .chain(std::iter::once(detail.meeting.notes.as_str()))
                .chain(std::iter::once(detail.meeting.ai_summary.as_str()))
                .chain(detail.speakers.iter().map(|s| s.display_name.as_str()))
                .chain(detail.segments.iter().map(|s| s.text.as_str()))
                .chain(detail.bookmarks.iter().map(|b| b.note.as_str())),
        );
        let queued = if self.synthetic {
            None
        } else {
            queued_notes_for(
                cx.memory.queued_notes.get(&self.meeting_id),
                &detail.meeting.started_at,
            )
        };
        let load_ack = self.load_ack.take();
        self.notes
            .load(&detail.meeting.notes, queued, load_ack.as_ref());
        self.load_error = None;
        self.speakers = detail
            .speakers
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        self.stats = speaker_stats(&detail, &self.speakers);
        // Speakers gone after a retranscription leave the filter.
        let stats = &self.stats;
        self.filter
            .retain(|key| stats.iter().any(|s| s.key == *key));
        // New segments (retranscribe): re-derive a shown highlight from the
        // current position, without scrolling.
        if self.active.is_some() {
            self.active = match self.last_position {
                Some((_, position)) => segment_at(
                    &detail.segments,
                    i64::try_from(position).unwrap_or(i64::MAX),
                ),
                None => self.active.filter(|i| *i < detail.segments.len()),
            };
        }
        self.detail = Some(detail);
        self.rebuild_list();
        self.rebuild_timeline();
        self.deep_link();
        self.apply_debug_hooks();
    }

    /// Rebuild the rows for the data and the speaker filter (find
    /// recomputes its hits on the next pass).
    fn rebuild_list(&mut self) {
        if let Some(detail) = self.detail.as_ref() {
            let filter = &self.filter;
            self.list
                .rebuild(detail, |i| is_shown(filter, &detail.segments[i]));
        }
        self.data_gen += 1;
    }

    /// Rebuild the scrubber's timeline (data, names or filter changed).
    fn rebuild_timeline(&mut self) {
        if let Some(detail) = self.detail.as_ref() {
            let version = self.timeline.version + 1;
            self.timeline = build_timeline(detail, &self.stats, &self.filter, version);
        }
    }

    /// Show only `filter`'s speakers (everyone when empty), keeping the
    /// line at the top of the view in place.
    fn set_filter(&mut self, filter: BTreeSet<SpeakerKey>) {
        if filter == self.filter {
            return;
        }
        self.filter = filter;
        self.rebuild_list();
        self.list.keep_reading_position();
        self.rebuild_timeline();
        // A rename open on a line that is now hidden would lose its field
        // (and with it every way to commit): it closes.
        let hidden = |seg_id: i64| {
            self.detail.as_ref().is_some_and(|d| {
                d.segments
                    .iter()
                    .find(|s| s.id == seg_id)
                    .is_some_and(|s| !is_shown(&self.filter, s))
            })
        };
        if self.speaker_edit.as_ref().is_some_and(|e| hidden(e.seg_id)) {
            self.speaker_edit = None;
        }
    }

    /// Apply this pass's legend / proportion-bar click or chip Clear.
    fn apply_filter_click(&mut self) {
        let Some(click) = self.filter_click.take() else {
            return;
        };
        let mut filter = self.filter.clone();
        match click {
            Some((key, add)) => toggle_filter(&mut filter, key, add),
            None => filter.clear(),
        }
        self.set_filter(filter);
    }

    /// Where the view sits in the shell (debug builds can force the split
    /// layout's detail pane).
    fn pane(&self, cx: &UiCtx) -> PaneRole {
        self.debug.pane.unwrap_or(cx.pane)
    }

    // ------------------------------------------------------------------
    // Find

    fn find_field_id(&self) -> Id {
        Id::new(("transcript-find", self.meeting_id))
    }

    /// Ctrl+F opens find (or refocuses it, selecting the query); F3 /
    /// Shift+F3 step through the hits (opening find when closed); Enter /
    /// Shift+Enter step while the field has focus. Ctrl+F works from
    /// inside a text field too, as in a browser. Esc closes find from the
    /// field (`find_bar`) or while nothing has focus — not when it only
    /// blurs another field, nor while a menu is open.
    fn find_keys(&mut self, ctx: &egui::Context, cx: &UiCtx) {
        if cx.confirm.is_open() {
            return;
        }
        let field = self.find_field_id();
        let in_field = ctx.memory(|m| m.has_focus(field));
        let idle = !self.focus_last_pass && !Popup::is_any_open(ctx);
        let open = self.find.is_some();
        let (ctrl_f, f3, enter, escape) = ctx.input_mut(|i| {
            (
                consume_press(i, Key::F, |m| m.command && !m.alt && !m.shift).is_some(),
                consume_press(i, Key::F3, |m| !m.command && !m.alt),
                if in_field {
                    consume_press(i, Key::Enter, |m| !m.command && !m.alt)
                } else {
                    None
                },
                open && idle && consume_press(i, Key::Escape, |m| !m.any()).is_some(),
            )
        });
        if ctrl_f || (f3.is_some() && !open) {
            self.open_find(ctx);
        } else if let Some(m) = f3.or(enter) {
            self.step_find(m.shift);
        } else if escape {
            self.close_find(ctx);
        }
    }

    fn open_find(&mut self, ctx: &egui::Context) {
        let field = self.find_field_id();
        let focused = ctx.memory(|m| m.focused()).filter(|id| *id != field);
        let find = self.find.get_or_insert_with(FindState::default);
        find.focus = true;
        if focused.is_some() {
            find.return_focus = focused;
        }
    }

    /// Esc / ×: close find and give the focus back.
    fn close_find(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.find.take().and_then(|f| f.return_focus) {
            ctx.memory_mut(|m| m.request_focus(id));
        }
    }

    /// The next (or previous) hit, scrolled into view.
    fn step_find(&mut self, back: bool) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        find.current = step_hit(find.hits.len(), find.current, back);
        self.reveal_find_hit();
    }

    /// Recompute the hits when the query or the rows changed. A new query
    /// starts from the current hit's line (or the line at the top of the
    /// view) and scrolls to its first hit; new data keeps the place.
    fn update_find(&mut self) {
        let (Some(find), Some(detail)) = (self.find.as_mut(), self.detail.as_ref()) else {
            return;
        };
        let key = (find.query.clone(), self.data_gen);
        if find.computed.as_ref() == Some(&key) {
            return;
        }
        let new_query = find.computed.as_ref().is_none_or(|(q, _)| *q != key.0);
        let from = find
            .current
            .and_then(|c| find.hits.get(c))
            .map_or_else(|| self.list.reading_segment(), |h| h.seg);
        let filter = &self.filter;
        find.hits = find_hits(
            detail,
            |i| is_shown(filter, &detail.segments[i]),
            &find.query,
        );
        find.current = first_hit_from(&find.hits, from);
        find.computed = Some(key);
        if new_query {
            self.reveal_find_hit();
        }
    }

    /// Scroll the current hit's line into view (centred, unless it is
    /// already wholly on screen); follow-along lets go, so playback does
    /// not scroll it away again (the Jump pill brings it back).
    fn reveal_find_hit(&mut self) {
        let Some(seg) = self
            .find
            .as_ref()
            .and_then(|f| f.current.and_then(|c| f.hits.get(c)))
            .map(|h| h.seg)
        else {
            return;
        };
        if !self.list.seg_visible(seg) {
            self.follow = false;
            self.scroll_req = Some(ScrollReq::new(seg, true, false));
        }
    }

    /// The first load's share of the [`DebugHooks`] (each applies once).
    fn apply_debug_hooks(&mut self) {
        if self.detail.is_none() {
            return;
        }
        if std::mem::take(&mut self.debug.notes_open) {
            self.notes.open = true;
        }
        if let Some(labels) = self.debug.filter.take() {
            // Speakers by label (`me`, `S1`…), as the legend colours them.
            let filter = self
                .stats
                .iter()
                .filter(|stat| labels.split(',').any(|l| l.trim() == stat.label))
                .map(|stat| stat.key)
                .collect();
            self.set_filter(filter);
        }
        if let Some((query, index)) = self.debug.find.take() {
            self.find = Some(FindState {
                query,
                focus: true,
                ..FindState::default()
            });
            self.update_find();
            if let (Some(index), Some(find)) = (index, self.find.as_mut()) {
                if index < find.hits.len() {
                    find.current = Some(index);
                }
                self.reveal_find_hit();
            }
        }
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        if let Some(idx) = self.debug.segment.take() {
            // A deep link by index, without `focus_segment_id` (which
            // `sync_view_args` keeps equal to the view's).
            if let Some(seg) = detail.segments.get(idx) {
                self.active = Some(idx);
                self.pending_seek_ms = Some(u64::try_from(seg.start_ms).unwrap_or(0));
                self.scroll_req = Some(ScrollReq::new(idx, true, false));
            }
        }
        if std::mem::take(&mut self.debug.rename) {
            // The first turn header with a speaker to rename.
            let first = self
                .list
                .rows
                .iter()
                .zip(&self.list.heads)
                .find_map(|(row, head)| {
                    let Row::Seg(i) = *row else { return None };
                    let seg = &detail.segments[i];
                    (*head)
                        .then_some(seg)
                        .and_then(|s| Some((s.speaker_id?, s.id)))
                });
            if let Some((speaker_id, seg_id)) = first {
                let name = speaker_of(detail, &self.speakers, Some(speaker_id))
                    .map(|s| s.display_name.clone())
                    .unwrap_or_default();
                self.speaker_edit = Some(SpeakerEdit::new(speaker_id, seg_id, &name));
            }
        }
    }

    /// Search deep link: once the segments are there, mark the row active,
    /// queue the audio position (without playing) and centre the row.
    fn deep_link(&mut self) {
        if self.focused_once {
            return;
        }
        let (Some(target), Some(detail)) = (self.focus_segment_id, self.detail.as_ref()) else {
            return;
        };
        let Some(idx) = detail.segments.iter().position(|s| s.id == target) else {
            return;
        };
        self.focused_once = true;
        self.active = Some(idx);
        self.pending_seek_ms = Some(u64::try_from(detail.segments[idx].start_ms).unwrap_or(0));
        self.scroll_req = Some(ScrollReq::new(idx, true, false));
        self.focus_pending = Some(target);
    }

    /// Resolve the archive path when `audio_path` appears (or on Try
    /// again), then hand it to the player.
    fn poll_audio(&mut self, cx: &mut UiCtx) {
        let want = self
            .detail
            .as_ref()
            .and_then(|d| d.meeting.audio_path.clone())
            .map(|path| (path, self.reload_gen));
        if want != self.audio_key {
            self.audio_key = want;
            self.audio = match &self.audio_key {
                None => Audio::None,
                Some(_) => {
                    let id = self.meeting_id;
                    Audio::Resolving(
                        cx.tasks
                            .spawn("audio-path", move |core| core.audio_path(id)),
                    )
                }
            };
        }
        if let Audio::Resolving(task) = &mut self.audio {
            if let Some(result) = task.poll_flat() {
                self.audio = match result {
                    Ok(path) => Audio::Ready(path),
                    Err(error) => Audio::Failed(error),
                };
            }
        }
        if let Audio::Ready(path) = &self.audio {
            let snap = cx.player.snapshot();
            if snap.path.as_ref() != Some(path) {
                cx.player.load(path.clone());
                cx.player.set_speed(player_bar::snap_speed(cx.prefs.speed));
                cx.player.set_volume(cx.prefs.volume.clamp(0.0, 1.0));
            }
            if let Some(ms) = self.pending_seek_ms.take() {
                cx.player.seek_ms(ms);
            }
        }
    }

    fn playback(&self, snap: &PlayerSnapshot) -> Playback {
        match &self.audio {
            Audio::Ready(path) if snap.path.as_ref() == Some(path) => match &snap.load {
                PlayerLoad::Error(error) => Playback::Failed(error.clone()),
                _ => Playback::Playable,
            },
            Audio::Failed(error) => Playback::Failed(error.clone()),
            _ => Playback::Unavailable,
        }
    }

    // ------------------------------------------------------------------
    // Playback-linked behaviour

    /// Recompute the active row when the position moves (the old
    /// `timeupdate`); follow-along scrolls it into view ("nearest"). The
    /// first position seen after the audio loads only primes this: loading
    /// fired no `timeupdate` either, so nothing is highlighted until
    /// playback or a seek (a deep link has already set its row).
    fn track_position(&mut self, snap: &PlayerSnapshot) {
        let key = (snap.generation, snap.position_ms);
        let previous = self.last_position.replace(key);
        if previous.is_none_or(|(generation, _)| generation != snap.generation)
            || previous == Some(key)
        {
            return;
        }
        let Some(detail) = &self.detail else { return };
        let t = i64::try_from(snap.position_ms).unwrap_or(i64::MAX);
        let idx = segment_at(&detail.segments, t);
        if idx != self.active {
            self.active = idx;
            if let (true, Some(seg)) = (self.follow, idx) {
                self.scroll_req = Some(ScrollReq::new(seg, false, true));
            }
        }
    }

    /// Space / ← / → — only when no widget has focus (a focused button
    /// turns Space into a click; a text field types), and no menu or modal
    /// is open.
    fn player_keys(&mut self, ctx: &egui::Context, cx: &mut UiCtx) {
        if ctx.memory(|m| m.focused().is_some()) || cx.confirm.is_open() || Popup::is_any_open(ctx)
        {
            return;
        }
        let (space, left, right) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Space),
                i.consume_key(Modifiers::NONE, Key::ArrowLeft),
                i.consume_key(Modifiers::NONE, Key::ArrowRight),
            )
        });
        if space {
            self.play_origin = PlayOrigin::Bar;
            cx.player.toggle();
        }
        if left {
            cx.player.skip_ms(-KEY_SKIP_MS);
        }
        if right {
            cx.player.skip_ms(KEY_SKIP_MS);
        }
    }

    /// `play()` on behalf of `origin` (which picks the failure toast).
    fn play(&mut self, cx: &mut UiCtx, origin: PlayOrigin) {
        self.play_origin = origin;
        cx.player.play();
    }

    /// Remember where the scroll area and its bar are, for next frame's
    /// [`Self::detect_user_scroll`]. `inner_rect` excludes a solid bar,
    /// which egui places to the right of it; a floating bar overlays the
    /// content's right edge instead.
    fn record_scroll_rects<R>(
        &mut self,
        ui: &egui::Ui,
        output: &egui::scroll_area::ScrollAreaOutput<R>,
    ) {
        let inner = output.inner_rect;
        let style = ui.spacing().scroll;
        let bar_width = style.bar_inner_margin + style.bar_width + style.bar_outer_margin;
        let (bar_x, outer) = if style.floating {
            ((inner.right() - bar_width)..=inner.right(), inner)
        } else {
            let right = inner.right() + style.allocated_width();
            (inner.right()..=right, inner.with_max_x(right))
        };
        let scrollable = output.content_size.y > inner.height() + 0.5;
        self.scroll_inner = Some(inner);
        self.scroll_outer = Some(outer);
        self.scroll_bar = scrollable.then(|| Rect::from_x_y_ranges(bar_x, inner.y_range()));
    }

    /// A wheel over the transcript or a press on its scroll bar (a thumb
    /// drag or a track click) suspends follow-along; programmatic scrolls
    /// and presses on the content do not.
    fn detect_user_scroll(&mut self, ctx: &egui::Context) {
        let Some(outer) = self.scroll_outer else {
            return;
        };
        let bar = self.scroll_bar;
        let scrolled = ctx.input(|i| {
            let wheel = i.events.iter().any(|event| {
                matches!(event, egui::Event::MouseWheel { delta, modifiers, .. }
                    if delta.y != 0.0 && !modifiers.command)
            }) && i.pointer.hover_pos().is_some_and(|p| outer.contains(p));
            let thumb = i.pointer.primary_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| bar.is_some_and(|bar| bar.contains(p)));
            wheel || thumb
        });
        if scrolled {
            self.follow = false;
            if self.scroll_req.is_some_and(|r| r.animate) {
                self.scroll_req = None;
            }
        }
    }

    /// Keep re-aiming a centred jump until the row sits near the centre.
    fn settle_scroll(&mut self, ctx: &egui::Context, out: &ListOutput) {
        self.scroll_req = self.scroll_req.and_then(|req| req.next(out));
        if self.scroll_req.is_some() {
            ctx.request_repaint();
        }
    }

    // ------------------------------------------------------------------
    // Drawing

    /// Everything inside the scroll area; returns the list's output.
    fn content(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx, playback: &Playback) -> ListOutput {
        self.header(ui, cx);
        self.speaker_summary(ui);

        let status = self
            .detail
            .as_ref()
            .map(|d| d.meeting.status.clone())
            .unwrap_or_default();
        let segments_empty = self.detail.as_ref().is_none_or(|d| d.segments.is_empty());
        let live_meeting = cx.shell.live_meeting_id.or_else(|| {
            cx.shell
                .status
                .as_ref()
                .filter(|s| s.recording)
                .and_then(|s| s.meeting_id)
        });
        let live_here = live_meeting == Some(self.meeting_id);
        let lines = &cx.shell.live_lines;
        let show_live = live_here
            && segments_empty
            && (status == "recording"
                || ((status == "processing" || status == "recorded") && !lines.is_empty()));
        if show_live {
            let active = cx.shell.live_captions_active();
            live_feed(
                ui,
                &mut self.live_feed,
                self.meeting_id,
                status == "recording",
                active,
                lines,
            );
        }

        self.notes_panel(ui, cx);
        self.summary_section(ui, cx);

        let has_audio_path = self
            .detail
            .as_ref()
            .is_some_and(|d| d.meeting.audio_path.is_some());
        if let (Playback::Failed(error), true) = (playback, has_audio_path) {
            widgets::inline_warning(ui, &format!("Audio playback is unavailable: {error}"));
            widgets::gap(ui, theme::S2);
        }

        let Some(detail) = self.detail.as_ref() else {
            return ListOutput::default();
        };
        if detail.segments.is_empty() && !show_live && detail.bookmarks.is_empty() {
            let progress = transcription_progress(cx, self.meeting_id);
            let (lines, retry) =
                empty_message(detail, progress.as_ref().map(|(s, p)| (s.as_str(), *p)));
            ui.add_space(48.0);
            ui.vertical_centered(|ui| {
                for line in &lines {
                    ui.add(
                        egui::Label::new(egui::RichText::new(line).color(pal().text_muted)).wrap(),
                    );
                }
                if retry && widgets::btn(ui, "Retry").clicked() {
                    self.retranscribe(cx, None);
                }
            });
            ui.add_space(48.0);
            return ListOutput::default();
        }

        // The deep link's focus lands once its centred scroll has settled
        // (the row is laid out on screen then); one attempt only.
        let mut focus_seg = if self.scroll_req.is_none() {
            self.focus_pending.take()
        } else {
            None
        };
        let mut rc = RowCtx {
            detail,
            speakers: &self.speakers,
            tokens: &self.tokens,
            active: self.active,
            speaker_edit: &mut self.speaker_edit,
            bookmark_edit: &mut self.bookmark_edit,
            people: &self.people,
            focus_seg: &mut focus_seg,
            find: self
                .find
                .as_ref()
                .filter(|f| !find_needle(&f.query).is_empty()),
        };
        self.list.show(ui, &mut rc, self.scroll_req)
    }

    /// Back, the title (elided; double-click or the pencil renames) over
    /// its meta line, and the Export / Retranscribe menus top-right. One
    /// row at every width: the title block takes what the buttons leave.
    fn header(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        let meeting = &detail.meeting;
        let title = meeting.title.clone();
        let mut meta = format!(
            "{} · {}",
            fmt_date(&meeting.started_at),
            fmt_duration(meeting.duration_ms)
        );
        if let Some(engine) = &meeting.engine {
            meta.push_str(&format!(" · {engine}"));
        }
        if matches!(self.ai_task(cx, AiTask::Title), Some(AiTaskState::Busy(_))) {
            meta.push_str(" · writing a title…");
        }
        let has_segments = !detail.segments.is_empty();
        let has_summary = !meeting.ai_summary.trim().is_empty();
        let ai = self.ai_configured(cx);
        let copied = self.copied_until.is_some_and(|t| Instant::now() < t);
        let open_menu = self.debug.menu.take();
        let layout = header_layout(self.pane(cx));

        let mut back = false;
        let mut export_choice = None;
        let mut engine_choice = None;
        let mut ai_choice = None;
        let mut start_edit = false;
        let mut title_done = None;
        // The row is as tall as the title block, and the buttons centre on
        // it: the title line is the pencil's 28 pt (the rename field's
        // height while editing), then the 12 pt meta line.
        let title_font = theme::semibold(layout.title_size);
        let title_h = if self.title_edit.is_some() {
            // The field's 3 + 3 padding and 1 + 1 border.
            row_height(ui, &title_font) + 8.0
        } else {
            row_height(ui, &title_font).max(PENCIL_SIZE)
        };
        let meta_font = theme::regular(12.0);
        let block_h = title_h + 2.0 + row_height(ui, &meta_font);
        let width = ui.available_width();
        ui.allocate_ui_with_layout(
            egui::vec2(width, block_h),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 2.0);
                if layout.back {
                    back = widgets::icon_btn(ui, ph::ARROW_LEFT, HEADER_ICON, "Back").clicked();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    // Only with a configured AI service (Settings → AI assistant).
                    if ai {
                        ai_choice = header_menu(
                            ui,
                            ph::SPARKLE,
                            "AI assistant",
                            "AI assistant",
                            has_segments,
                            &[
                                "Generate title",
                                if has_summary {
                                    "Regenerate summary"
                                } else {
                                    "Generate summary"
                                },
                            ],
                            open_menu.as_deref() == Some("ai"),
                        );
                    }
                    engine_choice = header_menu(
                        ui,
                        ph::ARROWS_CLOCKWISE,
                        "Retranscribe",
                        "Retranscribe",
                        true,
                        &["with Parakeet", "with Whisper"],
                        open_menu.as_deref() == Some("retranscribe"),
                    );
                    let (glyph, tip) = if copied {
                        (ph::CHECK_CIRCLE, "Copied to clipboard")
                    } else {
                        (ph::EXPORT, "Export transcript")
                    };
                    export_choice = header_menu(
                        ui,
                        glyph,
                        tip,
                        "Export transcript",
                        has_segments,
                        &[
                            "Copy to clipboard",
                            "Markdown…",
                            "Plain text…",
                            "Subtitles (SRT)…",
                            "Subtitles (WebVTT)…",
                        ],
                        open_menu.as_deref() == Some("export"),
                    );
                    ui.add_space(6.0);
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let block = egui::vec2(ui.available_width(), block_h);
                        ui.allocate_ui_with_layout(block, Layout::top_down(Align::Min), |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            if let Some(edit) = self.title_edit.as_mut() {
                                let margins = 2.0 * f32::from(INLINE_EDIT_MARGIN.left) + 2.0;
                                let width = (ui.available_width() - margins).max(80.0);
                                let (_, outcome) = inline_editor(
                                    ui,
                                    Id::new(("title-edit", self.meeting_id)),
                                    edit,
                                    title_font.clone(),
                                    width,
                                    "",
                                    "Meeting title",
                                );
                                if outcome != EditOutcome::Open {
                                    title_done = Some(outcome);
                                }
                            } else {
                                start_edit = title_row(ui, &title, layout.title_size);
                            }
                            widgets::elide(ui, &meta, meta_font.clone(), pal().text_muted);
                        });
                    });
                });
            },
        );
        widgets::gap(ui, layout.gap_below);

        if back {
            self.back(cx);
        }
        if start_edit {
            self.title_edit = Some(LineEdit::new(&title));
        }
        match title_done {
            Some(EditOutcome::Commit) => self.commit_title(cx),
            Some(EditOutcome::Cancel) => self.title_edit = None,
            _ => {}
        }
        match export_choice {
            Some(0) => self.copy_transcript(cx),
            Some(1) => self.export_transcript(cx, ExportFormat::Markdown),
            Some(2) => self.export_transcript(cx, ExportFormat::Text),
            Some(3) => self.export_transcript(cx, ExportFormat::Srt),
            Some(4) => self.export_transcript(cx, ExportFormat::Vtt),
            _ => {}
        }
        match ai_choice {
            Some(0) => self.ai_generate(cx, AiTask::Title),
            Some(1) => self.ai_generate(cx, AiTask::Summary),
            _ => {}
        }
        match engine_choice {
            Some(0) => self.retranscribe(cx, Some(Engine::Parakeet)),
            Some(1) => self.retranscribe(cx, Some(Engine::Whisper)),
            _ => {}
        }
    }

    /// The speaker summary: one stacked proportion bar (each speaker's
    /// share of the talk time, in their colour) over a wrapping legend
    /// ("● Name 16 min · 57%"). Hovering either shows the full figures; a
    /// click on either filters the transcript to that speaker (Ctrl+click
    /// adds one; the docked "Showing … · Clear" chip resets it).
    fn speaker_summary(&mut self, ui: &mut egui::Ui) {
        if self.stats.is_empty() {
            return;
        }
        let filtering = !self.filter.is_empty();
        let mut click = None;
        let width = ui.available_width();
        let (bar, _) = ui.allocate_exact_size(egui::vec2(width, SUMMARY_BAR_H), Sense::hover());
        let shares: Vec<f64> = self.stats.iter().map(|s| s.share).collect();
        let widths = summary_widths(&shares, width);
        let last = widths.len() - 1;
        let round = (SUMMARY_BAR_H / 2.0) as u8;
        let mut x = bar.left();
        for (i, (stat, w)) in self.stats.iter().zip(&widths).enumerate() {
            let rect = Rect::from_min_size(egui::pos2(x, bar.top()), egui::vec2(*w, bar.height()));
            x += w + SUMMARY_GAP;
            // Only the bar's two ends are round; inner joins stay crisp.
            let (left, right) = (
                if i == 0 { round } else { 1 },
                if i == last { round } else { 1 },
            );
            let radius = CornerRadius {
                nw: left,
                sw: left,
                ne: right,
                se: right,
            };
            let segment = ui.interact(rect, ui.id().with(("summary-bar", i)), Sense::click());
            let selected = self.filter.contains(&stat.key);
            let full = format!("{} — {}", stat.name, stat_text(stat));
            segment.widget_info(|| {
                egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &full)
            });
            let color = speaker_text(&stat.label);
            let fill = match (filtering && !selected, segment.hovered()) {
                (false, false) => color,
                (false, true) => theme::lighten(color, 0.25),
                (true, false) => theme::alpha(color, FILTERED_OUT_ALPHA),
                (true, true) => theme::alpha(color, 0.7),
            };
            ui.painter().rect_filled(rect, radius, fill);
            let segment =
                segment.on_hover_text(format!("{full}\n{}", filter_hint(&self.filter, stat.key)));
            if segment.clicked() {
                click = Some(stat.key);
            }
        }
        widgets::gap(ui, 10.0);
        ui.scope(|ui| {
            // Rows as tall as the items, not the 32 pt interact height.
            ui.spacing_mut().interact_size.y = 0.0;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(16.0, 6.0);
                for stat in &self.stats {
                    let selected = self.filter.contains(&stat.key);
                    let dimmed = filtering && !selected;
                    let hint = filter_hint(&self.filter, stat.key);
                    let item = legend_item(ui, stat, width, selected, dimmed, hint);
                    if item.clicked() {
                        click = Some(stat.key);
                    }
                }
            });
        });
        if let Some(key) = click {
            let add = ui.input(|i| i.modifiers.command);
            self.filter_click = Some(Some((key, add)));
        }
        widgets::gap(ui, theme::S3);
    }

    /// The notes disclosure: a full-width row (caret, "Notes", the save
    /// state, and a faint first line while closed) over the notes field.
    fn notes_panel(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let indicator = self.notes.indicator();
        let open = self.notes.open;
        let preview = if open {
            None
        } else {
            self.notes
                .draft
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_owned)
        };
        let (rect, summary) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), NOTES_ROW_H),
            Sense::click(),
        );
        summary.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::CollapsingHeader, true, open, "Notes")
        });
        if ui.is_rect_visible(rect) {
            let ring = summary.has_focus() && widgets::focus_visible(ui.ctx());
            let hot = summary.hovered() || ring;
            let painter = ui.painter();
            if hot {
                painter.rect_filled(rect, CornerRadius::same(theme::R_MD), pal().bg_hover);
            }
            let cy = rect.center().y;
            let mut x = rect.left() + ROW_PAD_X;
            let caret = if open {
                ph::CARET_DOWN
            } else {
                ph::CARET_RIGHT
            };
            let mut place = |galley: std::sync::Arc<egui::Galley>, gap: f32| {
                painter.galley(
                    egui::pos2(x, cy - galley.size().y / 2.0),
                    galley.clone(),
                    pal().text,
                );
                x += galley.size().x + gap;
            };
            place(
                painter.layout_no_wrap(caret.into(), theme::regular(12.0), pal().text_muted),
                6.0,
            );
            let label_color = if hot { pal().text } else { pal().text_muted };
            place(
                painter.layout_no_wrap("Notes".into(), theme::semibold(13.0), label_color),
                8.0,
            );
            if let Some(text) = indicator {
                place(
                    painter.layout_no_wrap(text.into(), theme::regular(12.0), pal().text_faint),
                    12.0,
                );
            }
            let room = rect.right() - ROW_PAD_X - x;
            if let Some(line) = preview.filter(|_| room > 40.0) {
                let galley = elided_galley(ui, &line, theme::regular(12.5), pal().text_faint, room);
                painter.galley(
                    egui::pos2(x, cy - galley.size().y / 2.0),
                    galley,
                    pal().text_faint,
                );
            }
            if ring {
                focus_ring(ui, rect, theme::R_MD);
            }
        }
        if summary.clicked() {
            self.notes.open = !self.notes.open;
        }
        widgets::announce(
            ui.ctx(),
            Id::new(("notes-status", self.meeting_id)),
            indicator.unwrap_or(""),
            Politeness::Polite,
        );

        if self.notes.open {
            widgets::gap(ui, theme::S1);
            // Raised fill, 1 pt border (accent while focused), radius 8.
            let field = egui::TextEdit::multiline(&mut self.notes.draft)
                .font(theme::regular(13.5))
                .desired_width(f32::INFINITY)
                .desired_rows(3)
                .hint_text("Meeting notes… (searchable)");
            let response = ui.add(widgets::framed_text_edit(
                ui,
                Id::new(("notes", self.meeting_id)),
                field,
                pal().bg_raised,
                theme::R_MD,
                Margin::symmetric(12, 10),
            ));
            set_accessible_label(ui.ctx(), response.id, "Meeting notes");
            widgets::text_edit_menu(&response);
            if response.changed() {
                self.notes.edited();
                cx.set_notes_deadline(Some(Instant::now() + NOTES_DEBOUNCE));
            }
            if response.lost_focus() {
                self.flush(cx);
            }
            widgets::gap(ui, theme::S3);
        } else {
            widgets::gap(ui, theme::S1);
        }
    }

    /// The AI Summary disclosure under the notes (`ai_summary.rs`).
    fn summary_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let configured = self.ai_configured(cx);
        let task = self.ai_task(cx, AiTask::Summary);
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        let meeting = &detail.meeting;
        let input = SummaryInput {
            meeting_id: self.meeting_id,
            summary: &meeting.ai_summary,
            stale: meeting.ai_summary_stale,
            generated_at: meeting.ai_summary_at.as_deref(),
            has_transcript: !detail.segments.is_empty(),
            configured,
            task: task.as_ref(),
        };
        if !ai_summary::shows(&input) {
            return;
        }
        // A job that starts opens the panel, so its progress is visible.
        let open = self.summary_open.get_or_insert(false);
        if task.is_some() && !self.summary_seen_task {
            *open = true;
        }
        self.summary_seen_task = task.is_some();
        let action = ai_summary::summary_panel(ui, open, &input, &mut self.summary_md);
        match action {
            SummaryAction::None => {}
            SummaryAction::Generate => self.ai_generate(cx, AiTask::Summary),
            SummaryAction::CopyToNotes => self.copy_summary_to_notes(cx),
            SummaryAction::OpenSettings => cx.open_settings(SettingsPage::Ai),
        }
    }

    /// AI actions show only with the assistant on and set up.
    fn ai_configured(&self, cx: &UiCtx) -> bool {
        self.debug
            .ai
            .configured(cx.settings.current().ai.is_configured())
    }

    /// The meeting's queued / running / failed AI task, if any.
    fn ai_task(&self, cx: &UiCtx, task: AiTask) -> Option<AiTaskState> {
        self.debug
            .ai
            .task(task)
            .or_else(|| cx.shell.ai_tasks.get(&(self.meeting_id, task)).cloned())
    }

    /// Queue an AI title / summary (the core's worker does the request).
    fn ai_generate(&mut self, cx: &mut UiCtx, task: AiTask) {
        if self.synthetic {
            return;
        }
        if task == AiTask::Summary {
            self.summary_open = Some(true);
        }
        if let Err(error) = cx.core.ai_generate(self.meeting_id, task) {
            cx.toasts.error("Could not ask the AI assistant", error);
        }
    }

    /// Append the summary to the user's notes (never replacing them),
    /// through the same ordered writer as typing.
    fn copy_summary_to_notes(&mut self, cx: &mut UiCtx) {
        let Some(summary) = self
            .detail
            .as_ref()
            .map(|d| d.meeting.ai_summary.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        if !self.notes.loaded {
            return;
        }
        let Some(draft) = notes_with_summary(&self.notes.draft, &summary) else {
            self.notes.open = true;
            cx.toasts
                .push(Notice::info("The summary is already in your notes"));
            return;
        };
        self.notes.draft = draft;
        self.notes.edited();
        self.notes.open = true;
        self.flush(cx);
        cx.toasts.success("Summary added to your notes");
    }

    // ------------------------------------------------------------------
    // Actions

    fn back(&mut self, cx: &mut UiCtx) {
        let from = match cx.view {
            View::Transcript { from, .. } => from.clone(),
            _ => None,
        };
        cx.nav.back(from);
    }

    fn apply_actions(&mut self, cx: &mut UiCtx, playable: bool, actions: Vec<RowAction>) {
        let actions = editor_results_first(actions);
        // A note blurred by a click on its own trash button: the row is
        // going, so saving the note would only race the delete.
        let deleted: Vec<i64> = actions
            .iter()
            .filter_map(|a| match a {
                RowAction::DeleteBookmark(id) => Some(*id),
                _ => None,
            })
            .collect();
        for action in actions {
            match action {
                RowAction::SeekPlay(idx) => {
                    let Some(start) = self
                        .detail
                        .as_ref()
                        .and_then(|d| d.segments.get(idx))
                        .map(|s| s.start_ms)
                    else {
                        continue;
                    };
                    // Without audio there is nothing to seek (as before).
                    if playable {
                        cx.player.seek_ms(u64::try_from(start).unwrap_or(0));
                        self.play(cx, PlayOrigin::Seek);
                        self.active = Some(idx);
                        self.follow = true;
                    }
                }
                RowAction::BookmarkSeek(at_ms) => {
                    if playable {
                        cx.player.seek_ms(u64::try_from(at_ms).unwrap_or(0));
                        self.play(cx, PlayOrigin::Seek);
                        self.follow = true;
                    }
                }
                RowAction::DeleteBookmark(id) => self.delete_bookmark(cx, id),
                editor => match self.route_editor(editor, &deleted) {
                    Some(EditorEffect::CommitSpeaker(state, name)) => {
                        self.commit_speaker(cx, state, name);
                    }
                    Some(EditorEffect::CommitBookmark(state)) => self.commit_bookmark(cx, state),
                    Some(EditorEffect::LoadPeople) => {
                        // Refreshed each time a rename opens.
                        self.people_task =
                            Some(cx.tasks.spawn("list-people", |core| core.list_people()));
                    }
                    None => {}
                },
            }
        }
    }

    /// Open, close or commit the inline editors (no side effects here; the
    /// returned effect is the caller's to run). A result only touches the
    /// editor it names, and an Open prefills from the current data, so it
    /// sees a commit applied just before it. `deleted`: bookmarks removed
    /// this pass, whose note is not saved.
    fn route_editor(&mut self, action: RowAction, deleted: &[i64]) -> Option<EditorEffect> {
        match action {
            RowAction::OpenSpeaker { speaker_id, seg_id } => {
                let speaker = self
                    .detail
                    .as_ref()?
                    .speakers
                    .iter()
                    .find(|s| s.id == speaker_id)?;
                let name = if speaker.display_name.is_empty() {
                    &speaker.label
                } else {
                    &speaker.display_name
                };
                self.speaker_edit = Some(SpeakerEdit::new(speaker_id, seg_id, name));
                Some(EditorEffect::LoadPeople)
            }
            RowAction::SpeakerDone { seg_id, outcome } => {
                let state = take_editor(&mut self.speaker_edit, |e| e.seg_id == seg_id)?;
                (outcome == EditOutcome::Commit).then(|| {
                    let name = state.edit.text.clone();
                    EditorEffect::CommitSpeaker(state, name)
                })
            }
            RowAction::SpeakerPick { seg_id, name } => {
                let state = take_editor(&mut self.speaker_edit, |e| e.seg_id == seg_id)?;
                Some(EditorEffect::CommitSpeaker(state, name))
            }
            RowAction::EditBookmark(id) => {
                let note = self
                    .detail
                    .as_ref()?
                    .bookmarks
                    .iter()
                    .find(|b| b.id == id)?
                    .note
                    .clone();
                self.bookmark_edit = Some(BookmarkEdit {
                    id,
                    edit: LineEdit::new(&note),
                });
                None
            }
            RowAction::BookmarkDone { id, outcome } => {
                let state = take_editor(&mut self.bookmark_edit, |e| e.id == id)?;
                (outcome == EditOutcome::Commit && !deleted.contains(&id))
                    .then_some(EditorEffect::CommitBookmark(state))
            }
            RowAction::SeekPlay(_) | RowAction::BookmarkSeek(_) | RowAction::DeleteBookmark(_) => {
                None
            }
        }
    }

    fn commit_title(&mut self, cx: &mut UiCtx) {
        let Some(edit) = self.title_edit.take() else {
            return;
        };
        let title = edit.text.trim().to_string();
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        if title.is_empty() || title == detail.meeting.title {
            return;
        }
        detail.meeting.title = title.clone();
        if self.synthetic {
            return;
        }
        let id = self.meeting_id;
        cx.tasks.spawn_op("rename-meeting", move |core| {
            failure_notice(
                core.rename_meeting(id, &title),
                "Could not rename the meeting",
            )
        });
    }

    /// Commit rules (map §4.4): empty is a no-op; the same name on a
    /// confirmed speaker is a no-op, but re-entering it on an auto-matched
    /// (≈) one confirms it. `state` is the closed editor, `name` the typed
    /// text or the picked suggestion.
    fn commit_speaker(&mut self, cx: &mut UiCtx, state: SpeakerEdit, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        if let Some(speaker) = detail
            .speakers
            .iter_mut()
            .find(|s| s.id == state.speaker_id)
        {
            if name == speaker.display_name.trim() && !speaker.auto_labeled {
                return;
            }
            speaker.display_name = name.clone();
            speaker.auto_labeled = false;
        }
        self.stats = speaker_stats(detail, &self.speakers);
        // The scrubber's tooltip names them too.
        self.rebuild_timeline();
        if self.synthetic {
            return;
        }
        let speaker_id = state.speaker_id;
        cx.tasks.spawn_op("rename-speaker", move |core| {
            failure_notice(
                core.rename_speaker(speaker_id, &name),
                "Could not rename the speaker",
            )
        });
    }

    /// Enter / blur saves the trimmed note (even unchanged or empty).
    fn commit_bookmark(&mut self, cx: &mut UiCtx, state: BookmarkEdit) {
        let note = state.edit.text.trim().to_string();
        if let Some(bookmark) = self
            .detail
            .as_mut()
            .and_then(|d| d.bookmarks.iter_mut().find(|b| b.id == state.id))
        {
            bookmark.note = note.clone();
        }
        if self.synthetic {
            return;
        }
        let id = state.id;
        cx.tasks.spawn_op("bookmark-note", move |core| {
            failure_notice(
                core.set_bookmark_note(id, &note),
                "Could not save the bookmark",
            )
        });
    }

    /// No confirmation (as before).
    fn delete_bookmark(&mut self, cx: &mut UiCtx, id: i64) {
        if let Some(detail) = self.detail.as_mut() {
            detail.bookmarks.retain(|b| b.id != id);
            self.rebuild_list();
        }
        if self.bookmark_edit.as_ref().is_some_and(|e| e.id == id) {
            self.bookmark_edit = None;
        }
        if self.synthetic {
            return;
        }
        cx.tasks.spawn_op("delete-bookmark", move |core| {
            failure_notice(core.delete_bookmark(id), "Could not remove the bookmark")
        });
    }

    /// The player bar's bookmark button. The op reports the new id back so
    /// the row appears with its note editor open.
    fn add_bookmark(&mut self, cx: &mut UiCtx, at_ms: i64) {
        if self.synthetic {
            return;
        }
        let id = self.meeting_id;
        let tx = self.bookmark_tx.clone();
        cx.tasks.spawn_op("add-bookmark", move |core| {
            let notice = match core.add_bookmark(id, at_ms, "") {
                Ok(bookmark_id) => {
                    let _ = tx.send((bookmark_id, at_ms));
                    None
                }
                Err(error) => Some(Notice::error("Could not add the bookmark", error)),
            };
            // add_bookmark emits MeetingsChanged, which reloads.
            OpDone::Notice {
                notice,
                refresh: false,
            }
        });
    }

    /// No confirmation (as before); user-set names carry forward by label.
    fn retranscribe(&mut self, cx: &mut UiCtx, engine: Option<Engine>) {
        if self.synthetic {
            return;
        }
        let id = self.meeting_id;
        cx.tasks
            .spawn_op("retranscribe", move |core| OpDone::Notice {
                notice: Some(match core.retranscribe(id, engine) {
                    Ok(()) => Notice::info("Transcription queued"),
                    Err(error) => Notice::error("Could not queue transcription", error),
                }),
                refresh: true,
            });
    }

    fn copy_transcript(&mut self, cx: &mut UiCtx) {
        let id = self.meeting_id;
        self.copy_task = Some(cx.tasks.spawn("copy-transcript", move |core| {
            core.transcript_text(id, ExportFormat::Text)
        }));
    }

    /// Render first, then the save dialog, then write — all on a worker
    /// (the old `export_transcript` command's order). Cancel is silent.
    fn export_transcript(&mut self, cx: &mut UiCtx, format: ExportFormat) {
        let owner = cx.dialog_owner();
        let id = self.meeting_id;
        cx.tasks.spawn_op("export-transcript", move |core| {
            let result = (|| {
                let text = core.transcript_text(id, format)?;
                let stem = core.suggested_file_stem(id)?;
                let ext = format.extension();
                let request = SaveFileRequest {
                    file_name: format!("{stem}.{ext}"),
                    filter_name: format!("{} transcript", ext.to_uppercase()),
                    extensions: vec![ext.to_string()],
                };
                match dialogs::save_file_blocking(owner, request) {
                    Some(dest) => std::fs::write(&dest, text).map_err(|e| e.to_string()),
                    None => Ok(()),
                }
            })();
            OpDone::Notice {
                notice: result
                    .err()
                    .map(|error| Notice::error("Could not export the transcript", error)),
                refresh: false,
            }
        });
    }
}

/// An op result whose success is silent; a failure toasts and reloads
/// (reverting the view's optimistic patch).
fn failure_notice(result: CoreResult<()>, message: &str) -> OpDone {
    match result {
        Ok(()) => OpDone::Notice {
            notice: None,
            refresh: false,
        },
        Err(error) => OpDone::Notice {
            notice: Some(Notice::error(message, error)),
            refresh: true,
        },
    }
}

/// `{stage, pct}` from the progress event for this meeting, else from the
/// status copy (stage "starting" / 0 % until the first event).
fn transcription_progress(cx: &UiCtx, meeting_id: i64) -> Option<(String, f32)> {
    if let Some(progress) = cx
        .shell
        .progress
        .as_ref()
        .filter(|p| p.meeting_id == meeting_id)
    {
        return Some((progress.stage.to_string(), progress.pct));
    }
    cx.shell
        .status
        .as_ref()
        .filter(|s| s.transcribing_meeting_id == Some(meeting_id))
        .map(|s| {
            (
                s.processing_stage
                    .clone()
                    .unwrap_or_else(|| "starting".into()),
                s.processing_pct.unwrap_or(0.0),
            )
        })
}

/// The meeting title (18 pt semibold; 16 in the split layout's detail
/// pane, whose column is narrower).
const TITLE_SIZE: f32 = 18.0;
const TITLE_SIZE_DETAIL: f32 = 16.0;

/// How the header is laid out for the view's place in the shell.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HeaderLayout {
    /// The Back button (the detail pane has the list beside it instead).
    back: bool,
    title_size: f32,
    /// The space under the header.
    gap_below: f32,
}

fn header_layout(pane: PaneRole) -> HeaderLayout {
    match pane {
        PaneRole::Detail => HeaderLayout {
            back: false,
            title_size: TITLE_SIZE_DETAIL,
            gap_below: theme::S3,
        },
        PaneRole::Full | PaneRole::List { .. } => HeaderLayout {
            back: true,
            title_size: TITLE_SIZE,
            gap_below: theme::S4,
        },
    }
}

/// The first load's placeholder, shaped like the view: the header (Back,
/// the title over its meta line), the proportion bar and legend, the notes
/// row, then a few speaker turns — shimmering until the meeting arrives.
fn loading_skeleton(ui: &mut egui::Ui, layout: HeaderLayout) {
    let width = ui.available_width();
    let sk = |ui: &mut egui::Ui, w: f32, h: f32| {
        widgets::skeleton(ui, egui::vec2(w.max(12.0), h));
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if layout.back {
            sk(ui, PENCIL_SIZE, PENCIL_SIZE);
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            sk(ui, width * 0.5, layout.title_size - 2.0);
            sk(ui, width * 0.32, 10.0);
        });
    });
    widgets::gap(ui, layout.gap_below);
    sk(ui, width, SUMMARY_BAR_H);
    widgets::gap(ui, 12.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S4;
        for w in [96.0f32, 120.0, 84.0] {
            sk(ui, w.min(width / 3.5), 10.0);
        }
    });
    widgets::gap(ui, theme::S3);
    // The notes row.
    ui.horizontal(|ui| {
        ui.set_height(NOTES_ROW_H);
        ui.add_space(ROW_PAD_X);
        sk(ui, 56.0, 11.0);
    });
    widgets::gap(ui, theme::S1);
    // Speaker turns: a name and time over one to three lines of text.
    let text_w = width - 2.0 * ROW_PAD_X;
    for (turn, lines) in [3usize, 1, 2, 3, 1].into_iter().enumerate() {
        if turn > 0 {
            ui.add_space(TURN_GAP);
        }
        ui.horizontal(|ui| {
            ui.add_space(ROW_PAD_X);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = LINE_HEIGHT - 11.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    sk(ui, [72.0, 96.0, 64.0][turn % 3], 11.0);
                    sk(ui, 28.0, 9.0);
                });
                for line in 0..lines {
                    let share = if line + 1 == lines {
                        [0.62, 0.48, 0.71][(turn + line) % 3]
                    } else {
                        0.96
                    };
                    sk(ui, text_w * share, 11.0);
                }
            });
        });
    }
}
/// The rename pencil's square (a 14 pt glyph): the title line's height.
const PENCIL_SIZE: f32 = 28.0;
/// The notes disclosure row.
const NOTES_ROW_H: f32 = 32.0;

/// The title (one line, elided with the full title in its tooltip) and the
/// inline pencil. Returns true when a rename should start (double-click or
/// pencil).
fn title_row(ui: &mut egui::Ui, title: &str, size: f32) -> bool {
    let mut start = false;
    // As tall as the title line: `horizontal` would reserve the 32 pt
    // interact height, padding the title down and away from its meta line.
    let interact = ui.spacing().interact_size.y;
    ui.spacing_mut().interact_size.y = 0.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().interact_size.y = interact;
        ui.spacing_mut().item_spacing.x = 2.0;
        let max = (ui.available_width() - PENCIL_SIZE - 2.0).max(40.0);
        let galley = elided_galley(ui, title, theme::semibold(size), pal().text, max);
        let tooltip = if galley.elided {
            format!("{title}\nDouble-click to rename")
        } else {
            "Double-click to rename".to_owned()
        };
        let label = ui.add(
            egui::Label::new(galley)
                .selectable(false)
                .sense(Sense::click()),
        );
        if label.on_hover_text(tooltip).double_clicked() {
            start = true;
        }
        if widgets::icon_btn(ui, ph::PENCIL_SIMPLE, 14.0, "Rename meeting").clicked() {
            start = true;
        }
    });
    ui.spacing_mut().interact_size.y = interact;
    start
}

/// What a click on a speaker in the summary will do (its tooltip's second
/// line).
fn filter_hint(filter: &BTreeSet<SpeakerKey>, key: SpeakerKey) -> &'static str {
    if filter.len() == 1 && filter.contains(&key) {
        "Click to show everyone"
    } else if filter.contains(&key) {
        "Click to show only them · Ctrl+click to hide them"
    } else {
        "Click to show only their lines · Ctrl+click to add them"
    }
}

/// One speaker in the summary's legend: a dot in their colour, the name
/// (elided so the item never outgrows `max_width`) and the short figures;
/// claimed as one item, so the wrapping legend breaks between speakers,
/// never inside one. A toggle for the speaker filter: `selected` speakers
/// sit on an accent tint, `dimmed` ones (filtered out) fade.
fn legend_item(
    ui: &mut egui::Ui,
    stat: &SpeakerStat,
    max_width: f32,
    selected: bool,
    dimmed: bool,
    hint: &str,
) -> Response {
    const DOT: f32 = 8.0;
    const GAP: f32 = 6.0;
    // The tint reaches this far past the item (inside the 16 pt spacing).
    const PAD: egui::Vec2 = egui::vec2(6.0, 3.0);
    let (name_color, figures_color) = if dimmed {
        (pal().text_faint, pal().text_faint)
    } else {
        (pal().text, pal().text_muted)
    };
    let figures =
        ui.painter()
            .layout_no_wrap(legend_text(stat), theme::regular(12.0), figures_color);
    let name_max = (max_width - DOT - 2.0 * GAP - figures.size().x - 2.0 * PAD.x).max(40.0);
    let name = elided_galley(ui, &stat.name, theme::regular(13.0), name_color, name_max);
    let height = name.size().y.max(figures.size().y);
    let size = egui::vec2(DOT + GAP + name.size().x + GAP + figures.size().x, height);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let full = format!("{} — {}", stat.name, stat_text(stat));
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &full)
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let ring = response.has_focus() && widgets::focus_visible(ui.ctx());
        let tint = rect.expand2(PAD);
        let radius = CornerRadius::same(theme::R_SM);
        if selected {
            painter.rect_filled(tint, radius, pal().accent_soft);
        }
        if response.hovered() || ring {
            painter.rect_filled(tint, radius, pal().bg_hover);
        }
        let cy = rect.center().y;
        let dot = speaker_text(&stat.label);
        painter.circle_filled(
            egui::pos2(rect.left() + DOT / 2.0, cy),
            DOT / 2.0,
            if dimmed {
                theme::alpha(dot, FILTERED_OUT_ALPHA)
            } else {
                dot
            },
        );
        let name_x = rect.left() + DOT + GAP;
        let figures_x = name_x + name.size().x + GAP;
        painter.galley(
            egui::pos2(name_x, cy - name.size().y / 2.0),
            name,
            name_color,
        );
        painter.galley(
            egui::pos2(figures_x, cy - figures.size().y / 2.0),
            figures,
            figures_color,
        );
        if ring {
            focus_ring(ui, tint, theme::R_SM);
        }
    }
    response.on_hover_text(format!("{full}\n{hint}"))
}

/// "Showing Paul Gordon · Clear": the speaker filter is on; a click shows
/// everyone again. The names elide to the column.
fn filter_chip(ui: &mut egui::Ui, names: &str) -> Response {
    const PAD_X: f32 = 10.0;
    const HEIGHT: f32 = 26.0;
    let font = theme::regular(12.5);
    let clear =
        ui.painter()
            .layout_no_wrap("·  Clear".to_owned(), theme::semibold(12.5), pal().accent);
    let room = ui.available_width() - 2.0 * PAD_X - clear.size().x - theme::S2;
    let shown = elided_galley(ui, &format!("Showing {names}"), font, pal().text, room);
    let size = egui::vec2(
        PAD_X + shown.size().x + theme::S2 + clear.size().x + PAD_X,
        HEIGHT,
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let label = format!("Clear the speaker filter (showing {names})");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let ring = response.has_focus() && widgets::focus_visible(ui.ctx());
        let fill = if response.hovered() || ring {
            theme::alpha(pal().accent, 0.26)
        } else {
            pal().accent_soft
        };
        let radius = CornerRadius::same(255);
        painter.rect_filled(rect, radius, fill);
        let cy = rect.center().y;
        let x = rect.left() + PAD_X;
        let clear_x = x + shown.size().x + theme::S2;
        painter.galley(egui::pos2(x, cy - shown.size().y / 2.0), shown, pal().text);
        painter.galley(
            egui::pos2(clear_x, cy - clear.size().y / 2.0),
            clear,
            pal().accent,
        );
        if ring {
            painter.rect_stroke(
                rect.expand(2.0),
                radius,
                Stroke::new(2.0, pal().focus),
                egui::StrokeKind::Outside,
            );
        }
    }
    response.on_hover_text("Show every speaker's lines")
}

/// What the find bar asked for this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindOutcome {
    None,
    Step { back: bool },
    Close,
}

/// The find bar: a raised field (accent border while focused) holding the
/// magnifier, the query, "3 of 17" in a fixed-width slot, previous / next
/// and close. The query updates the hits live (the view recomputes them);
/// Esc in the field or × closes it.
fn find_bar(ui: &mut egui::Ui, find: &mut FindState, field_id: Id) -> FindOutcome {
    let mut outcome = FindOutcome::None;
    let focused = ui.memory(|m| m.has_focus(field_id));
    let count = find_count_text(&find.query, find.hits.len(), find.current);
    let count_font = theme::regular(12.0);
    // As wide as "17 of 17" with `0` digits: stepping never moves the
    // buttons (only a new hit count can).
    let total = find.hits.len().max(1).to_string();
    let count_w = player_bar::stable_width(ui, &format!("{total} of {total}"), count_font.clone())
        .max(player_bar::stable_width(ui, &count, count_font.clone()));
    Frame::new()
        .fill(pal().bg_raised)
        .stroke(Stroke::new(
            1.0,
            if focused {
                pal().accent
            } else {
                pal().border_strong
            },
        ))
        .corner_radius(CornerRadius::same(theme::R_MD))
        .inner_margin(Margin {
            left: 10,
            right: 2,
            top: 0,
            bottom: 0,
        })
        .show(ui, |ui| {
            let width = ui.available_width();
            ui.allocate_ui_with_layout(
                egui::vec2(width, FIND_BAR_H - 2.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_min_size(egui::vec2(width, FIND_BAR_H - 2.0));
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(
                        egui::RichText::new(ph::MAGNIFYING_GLASS)
                            .size(15.0)
                            .color(pal().text_muted),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        if widgets::icon_btn(ui, ph::X, 14.0, "Close find (Esc)").clicked() {
                            outcome = FindOutcome::Close;
                        }
                        let any = !find.hits.is_empty();
                        let next = ui
                            .add_enabled_ui(any, |ui| {
                                widgets::icon_btn(ui, ph::CARET_DOWN, 14.0, "Next match (Enter)")
                            })
                            .inner;
                        if next.clicked() {
                            outcome = FindOutcome::Step { back: false };
                        }
                        let previous = ui
                            .add_enabled_ui(any, |ui| {
                                widgets::icon_btn(
                                    ui,
                                    ph::CARET_UP,
                                    14.0,
                                    "Previous match (Shift+Enter)",
                                )
                            })
                            .inner;
                        if previous.clicked() {
                            outcome = FindOutcome::Step { back: true };
                        }
                        ui.add_space(6.0);
                        let color = if any {
                            pal().text_muted
                        } else {
                            pal().text_faint
                        };
                        let galley = ui
                            .painter()
                            .layout_no_wrap(count.clone(), count_font, color);
                        let (rect, response) =
                            ui.allocate_exact_size(egui::vec2(count_w, 20.0), Sense::hover());
                        ui.painter().galley(
                            egui::pos2(
                                rect.right() - galley.size().x,
                                rect.center().y - galley.size().y / 2.0,
                            ),
                            galley,
                            color,
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &count)
                        });
                        ui.add_space(6.0);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let edit = egui::TextEdit::singleline(&mut find.query)
                                .id(field_id)
                                .font(theme::regular(TEXT_SIZE))
                                .frame(Frame::NONE)
                                .margin(Margin::ZERO)
                                // Enter steps through the hits (the view
                                // reads it) instead of ending the edit.
                                .return_key(None)
                                .hint_text("Find in transcript")
                                .desired_width(ui.available_width());
                            let response = ui.add(edit);
                            set_accessible_label(ui.ctx(), response.id, "Find in transcript");
                            widgets::text_edit_menu(&response);
                            if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                                outcome = FindOutcome::Close;
                            }
                            if std::mem::take(&mut find.focus) {
                                response.request_focus();
                                select_all(ui.ctx(), field_id, &find.query);
                            }
                        });
                    });
                },
            );
        });
    widgets::announce(ui.ctx(), field_id.with("count"), &count, Politeness::Polite);
    outcome
}

/// Select all of a text field's `text` (Ctrl+F on an open find bar).
fn select_all(ctx: &egui::Context, id: Id, text: &str) {
    use egui::text::{CCursor, CCursorRange};
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    let end = CCursor::new(text.chars().count());
    state
        .cursor
        .set_char_range(Some(CCursorRange::two(CCursor::new(0), end)));
    state.store(ctx, id);
}

/// Consume every press of `key` whose modifiers `accept` takes, returning
/// the modifiers of the last one. (egui's `consume_key` ignores an extra
/// Shift, so Enter would also swallow Shift+Enter.)
fn consume_press(
    input: &mut egui::InputState,
    key: Key,
    accept: impl Fn(Modifiers) -> bool,
) -> Option<Modifiers> {
    let mut found = None;
    input.events.retain(|event| match event {
        egui::Event::Key {
            key: k,
            pressed: true,
            modifiers,
            ..
        } if *k == key && accept(*modifiers) => {
            found = Some(*modifiers);
            false
        }
        _ => true,
    });
    found
}

/// A new caption scrolls the feed to the bottom only if the reader was
/// within this distance of it (transcript.tsx, tracked on scroll).
const LIVE_STICK_PX: f32 = 40.0;

/// The live feed's stick-to-bottom state.
#[derive(Debug)]
struct LiveFeedState {
    /// The reader was within `LIVE_STICK_PX` of the bottom last frame.
    near_bottom: bool,
    /// The newest caption seen (track, start), so only a new one scrolls.
    newest: Option<(&'static str, u64)>,
}

impl Default for LiveFeedState {
    fn default() -> Self {
        LiveFeedState {
            near_bottom: true,
            newest: None,
        }
    }
}

impl LiveFeedState {
    /// Whether this frame brings a new caption to scroll down to.
    fn stick(&mut self, newest: Option<(&'static str, u64)>) -> bool {
        let arrived = newest.is_some() && newest != self.newest;
        self.newest = newest;
        arrived && self.near_bottom
    }

    /// After drawing: how far the view sits above the end of the feed.
    fn scrolled(&mut self, stuck: bool, distance_from_bottom: f32) {
        // A frame that just scrolled down may still report the old offset;
        // it is at the bottom by construction.
        self.near_bottom = stuck || distance_from_bottom < LIVE_STICK_PX;
    }
}

/// The provisional caption feed (map §5): a raised surface with a faint
/// red outline, its own scroll area that follows new captions while the
/// reader is within 40 pt of its bottom. Lines are grouped into Me / Them
/// turns like the transcript.
fn live_feed(
    ui: &mut egui::Ui,
    state: &mut LiveFeedState,
    meeting_id: i64,
    recording: bool,
    captions_active: bool,
    lines: &std::collections::VecDeque<LiveTranscript>,
) {
    let mut sorted: Vec<&LiveTranscript> = lines.iter().collect();
    sorted.sort_by_key(|line| line.start_ms);
    let max_height = ui.ctx().content_rect().height() * 0.6;
    let stick = state.stick(lines.back().map(|line| (line.track, line.start_ms)));
    Frame::new()
        .fill(pal().bg_raised)
        .stroke(Stroke::new(1.0, theme::alpha(pal().rec, LIVE_BORDER_ALPHA)))
        .corner_radius(CornerRadius::same(theme::R_LG))
        .inner_margin(Margin::symmetric(8, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_space(ROW_PAD_X - 8.0);
                if recording {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
                    let alpha = icons::pulse_alpha(ui.ctx());
                    ui.painter()
                        .circle_filled(rect.center(), 4.0, pal().rec.gamma_multiply(alpha));
                }
                ui.label(
                    egui::RichText::new("Live captions")
                        .font(theme::semibold(13.0))
                        .color(pal().rec_light),
                );
                ui.label(
                    egui::RichText::new("provisional")
                        .size(12.0)
                        .color(pal().text_faint),
                );
            });
            widgets::gap(ui, 4.0);
            let output = egui::ScrollArea::vertical()
                .id_salt(("live-feed", meeting_id))
                .max_height((max_height - 40.0).max(80.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if sorted.is_empty() {
                        let text = if captions_active {
                            "Listening… captions appear a few seconds after speech."
                        } else {
                            "Live captions are off — enable them in Settings / download the \
                             transcription models. The full transcript is produced after the \
                             recording."
                        };
                        ui.horizontal(|ui| {
                            ui.add_space(ROW_PAD_X - ui.spacing().item_spacing.x);
                            ui.add(
                                egui::Label::new(egui::RichText::new(text).color(pal().text_muted))
                                    .wrap(),
                            );
                        });
                        return;
                    }
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let mut previous = None;
                    for line in &sorted {
                        let head = previous != Some(line.track);
                        previous = Some(line.track);
                        ui.push_id((line.track, line.start_ms), |ui| live_line(ui, line, head));
                    }
                    if stick {
                        ui.scroll_to_cursor(Some(Align::Max));
                    }
                });
            let distance =
                output.content_size.y - output.state.offset.y - output.inner_rect.height();
            state.scrolled(stick, distance);
        });
    if let Some(newest) = lines.back() {
        widgets::announce(
            ui.ctx(),
            Id::new(("live-caption", meeting_id)),
            &newest.text,
            Politeness::Polite,
        );
    }
    widgets::gap(ui, theme::S3);
}

/// A caption line: a "Me" / "Them" header with the time when the track
/// changes (not clickable), then the text.
fn live_line(ui: &mut egui::Ui, line: &LiveTranscript, head: bool) {
    // Text colours, made readable from the chips like `theme::speaker_text`.
    let (name, color) = if line.track == "mic" {
        ("Me", speaker_text("me"))
    } else {
        ("Them", theme::ink(pal().them_chip))
    };
    if head {
        ui.add_space(6.0);
    }
    let mut prepared = begin_row(ui);
    {
        let ui = &mut prepared.content_ui;
        if head {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), HEADER_H),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(
                        egui::RichText::new(name)
                            .font(theme::semibold(NAME_SIZE))
                            .color(color),
                    );
                    ui.label(
                        egui::RichText::new(fmt_ts(i64::try_from(line.start_ms).unwrap_or(0)))
                            .size(12.0)
                            .color(pal().text_muted),
                    );
                },
            );
        }
        ui.add(egui::Label::new(text_job(&line.text, &[], None)).wrap());
    }
    let rect = row_rect(&prepared);
    if ui.rect_contains_pointer(rect) {
        prepared.frame.fill = pal().bg_hover;
    }
    prepared.end(ui);
    ui.add_space(ROW_GAP);
}

/// Debug builds: states for passive snapshots (`snap.ps1`), each from an
/// environment variable and applied once; all off in release builds.
#[derive(Debug, Default, Clone)]
struct DebugHooks {
    /// `WITNESS_DEBUG_TRANSCRIPT_NOTES=1`: the notes open on load.
    notes_open: bool,
    /// `WITNESS_DEBUG_TRANSCRIPT_SEGMENT=<index>`: deep link to the n-th
    /// segment (active, centred, the audio positioned there).
    segment: Option<usize>,
    /// `WITNESS_DEBUG_TRANSCRIPT_HIGHLIGHT=<terms>`: search highlighting.
    highlight: Option<String>,
    /// `WITNESS_DEBUG_TRANSCRIPT_MENU=export|retranscribe`: that menu open.
    menu: Option<String>,
    /// `WITNESS_DEBUG_TRANSCRIPT_VOLUME=1`: the compact volume popover open.
    volume: bool,
    /// `WITNESS_DEBUG_TRANSCRIPT_RENAME=1`: the speaker rename open on the
    /// first turn with a speaker.
    rename: bool,
    /// `WITNESS_DEBUG_TRANSCRIPT_FIND=<query>[,<hit index>]`: find open
    /// with that query (and that hit current).
    find: Option<(String, Option<usize>)>,
    /// `WITNESS_DEBUG_TRANSCRIPT_FILTER=<label>[,<label>…]`: only those
    /// speakers' lines (`me`, `S1`…).
    filter: Option<String>,
    /// `WITNESS_DEBUG_TRANSCRIPT_LOADING=1`: the loading skeleton forever.
    loading: bool,
    /// `WITNESS_DEBUG_TRANSCRIPT_PANE=detail`: drawn as the split
    /// layout's detail pane (no Back button).
    pane: Option<PaneRole>,
    /// `WITNESS_DEBUG_AI_SUMMARY` / `WITNESS_DEBUG_AI_STATE`.
    ai: AiDebug,
}

/// Debug builds: the AI summary states without a service.
/// `WITNESS_DEBUG_AI_SUMMARY=sample|<markdown>` shows that summary (never
/// written); `WITNESS_DEBUG_AI_STATE=busy|failed|stale|off` shows a task in
/// progress, a failure, a summary from an earlier transcript, or the
/// not-configured hint. Either one alone counts as "configured".
#[derive(Debug, Default, Clone)]
struct AiDebug {
    summary: Option<String>,
    state: Option<String>,
}

impl AiDebug {
    fn active(&self) -> bool {
        self.summary.is_some() || self.state.is_some()
    }

    fn apply(&self, meeting: &mut Meeting) {
        if let Some(summary) = &self.summary {
            meeting.ai_summary = if summary == "sample" {
                ai_summary::SAMPLE_SUMMARY.to_string()
            } else {
                summary.replace("\\n", "\n")
            };
            meeting.ai_summary_at = Some(meeting.started_at.clone());
        }
        if self.state.as_deref() == Some("stale") {
            meeting.ai_summary_stale = true;
        }
    }

    fn configured(&self, real: bool) -> bool {
        if self.state.as_deref() == Some("off") {
            false
        } else {
            real || self.active()
        }
    }

    fn task(&self, task: AiTask) -> Option<AiTaskState> {
        if task != AiTask::Summary {
            return None;
        }
        match self.state.as_deref() {
            Some("busy") => Some(AiTaskState::Busy("Writing notes".into())),
            Some("failed") => Some(AiTaskState::Failed(
                "The AI service rejected the API key (401). Check the key in Settings → AI assistant."
                    .into(),
            )),
            _ => None,
        }
    }
}

/// `<query>[,<index>]`: a trailing number after the last comma is the hit
/// index; anything else is all query.
#[cfg_attr(not(any(debug_assertions, test)), allow(dead_code))]
fn parse_debug_find(spec: &str) -> (String, Option<usize>) {
    match spec.rsplit_once(',') {
        Some((query, index)) if index.trim().parse::<usize>().is_ok() => {
            (query.to_owned(), index.trim().parse().ok())
        }
        _ => (spec.to_owned(), None),
    }
}

#[cfg(debug_assertions)]
fn debug_hooks() -> DebugHooks {
    let var = |name: &str| {
        std::env::var(format!("WITNESS_DEBUG_TRANSCRIPT_{name}"))
            .ok()
            .filter(|v| !v.is_empty())
    };
    let on = |name: &str| var(name).is_some_and(|v| v == "1");
    DebugHooks {
        notes_open: on("NOTES"),
        segment: var("SEGMENT").and_then(|v| v.trim().parse().ok()),
        highlight: var("HIGHLIGHT"),
        menu: var("MENU"),
        volume: on("VOLUME"),
        rename: on("RENAME"),
        find: var("FIND").map(|spec| parse_debug_find(&spec)),
        filter: var("FILTER"),
        loading: on("LOADING"),
        pane: var("PANE")
            .filter(|v| v == "detail")
            .map(|_| PaneRole::Detail),
        ai: AiDebug {
            summary: std::env::var("WITNESS_DEBUG_AI_SUMMARY")
                .ok()
                .filter(|v| !v.is_empty()),
            state: std::env::var("WITNESS_DEBUG_AI_STATE")
                .ok()
                .filter(|v| !v.is_empty()),
        },
    }
}

#[cfg(not(debug_assertions))]
fn debug_hooks() -> DebugHooks {
    DebugHooks::default()
}

// ---------------------------------------------------------------------------
// Synthetic data (debug builds only)

#[cfg(debug_assertions)]
fn synthetic_enabled() -> bool {
    std::env::var_os("WITNESS_DEBUG_SYNTHETIC").is_some_and(|v| v == "1")
}

#[cfg(not(debug_assertions))]
fn synthetic_enabled() -> bool {
    false
}

/// A generated meeting with `count` segments of varied length in turns of
/// four speakers (one auto-matched) and a bookmark every 150 segments, for
/// checking that long transcripts scroll smoothly.
fn synthetic_detail(meeting_id: i64, count: usize) -> MeetingDetail {
    const WORDS: &[&str] = &[
        "budget",
        "roadmap",
        "quarter",
        "we",
        "should",
        "probably",
        "ship",
        "the",
        "native",
        "player",
        "before",
        "review",
        "and",
        "then",
        "check",
        "latency",
        "numbers",
        "again",
        "customers",
        "asked",
        "about",
        "exports",
        "subtitles",
        "notes",
        "search",
        "is",
        "fast",
    ];
    let speakers = vec![
        ("me", "Me", false),
        ("S1", "Alice", false),
        ("S2", "Bob", true),
        ("S3", "S3", false),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (label, name, auto))| Speaker {
        id: i as i64 + 1,
        meeting_id,
        label: label.into(),
        display_name: name.into(),
        person_id: None,
        auto_labeled: auto,
    })
    .collect::<Vec<_>>();
    let mut segments = Vec::with_capacity(count);
    let mut bookmarks = Vec::new();
    let mut at = 0i64;
    let mut speaker = 0;
    for i in 0..count {
        let words = 3 + (i * 7919) % 57;
        let text = (0..words)
            .map(|w| WORDS[(i * 31 + w * 17) % WORDS.len()])
            .collect::<Vec<_>>()
            .join(" ");
        // Turns of one to a few lines: the speaker changes on ~40 % of
        // lines, to one of the other three.
        if i > 0 && (i * 37 + 11) % 5 < 2 {
            speaker = (speaker + 1 + i % 3) % speakers.len();
        }
        let length = 1_500 + (words as i64) * 350;
        segments.push(Segment {
            id: i as i64 + 1,
            meeting_id,
            speaker_id: Some(speakers[speaker].id),
            track: if speaker == 0 { "mic" } else { "loopback" }.into(),
            start_ms: at,
            end_ms: at + length,
            text,
        });
        if i % 150 == 75 {
            bookmarks.push(Bookmark {
                id: bookmarks.len() as i64 + 1,
                meeting_id,
                at_ms: at + 200,
                note: if bookmarks.len() % 2 == 0 {
                    format!("checkpoint {}", bookmarks.len() + 1)
                } else {
                    String::new()
                },
            });
        }
        at += length + 400;
    }
    MeetingDetail {
        meeting: Meeting {
            id: meeting_id,
            started_at: "2026-10-05T09:00:00+00:00".into(),
            ended_at: Some("2026-10-05T12:00:00+00:00".into()),
            title: format!(
                "Synthetic transcript ({} segments)",
                fmt_thousands(count as i64)
            ),
            audio_path: None,
            duration_ms: Some(at),
            status: "transcribed".into(),
            engine: Some("parakeet".into()),
            trigger: "manual".into(),
            notes: String::new(),
            ..Meeting::default()
        },
        speakers,
        segments,
        bookmarks,
        last_error: None,
        pipeline_queued: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::ShellModel;
    use witness_core::CoreEvent;

    fn seg(id: i64, start_ms: i64, text: &str) -> Segment {
        Segment {
            id,
            meeting_id: 1,
            speaker_id: None,
            track: "mic".into(),
            start_ms,
            end_ms: start_ms + 1_000,
            text: text.into(),
        }
    }

    #[test]
    fn segment_at_is_the_last_started_segment() {
        let segments = vec![seg(1, 0, "a"), seg(2, 5_000, "b"), seg(3, 9_000, "c")];
        assert_eq!(segment_at(&segments, -1), None);
        assert_eq!(segment_at(&segments, 0), Some(0));
        // Gaps keep the previous segment highlighted.
        assert_eq!(segment_at(&segments, 4_999), Some(0));
        assert_eq!(segment_at(&segments, 5_000), Some(1));
        assert_eq!(segment_at(&segments, 60_000), Some(2));
        assert_eq!(segment_at(&[], 10), None);
    }

    #[test]
    fn rows_interleave_with_segments_first_on_ties() {
        let mut detail = synthetic_detail(1, 3);
        detail.segments[0].start_ms = 0;
        detail.segments[1].start_ms = 1_000;
        detail.segments[2].start_ms = 2_000;
        detail.bookmarks = vec![
            Bookmark {
                id: 7,
                meeting_id: 1,
                at_ms: 1_000,
                note: String::new(),
            },
            Bookmark {
                id: 8,
                meeting_id: 1,
                at_ms: 500,
                note: String::new(),
            },
        ];
        assert_eq!(
            build_rows(&detail, |_| true),
            vec![
                Row::Seg(0),
                Row::Bm(1),
                Row::Seg(1),
                Row::Bm(0),
                Row::Seg(2)
            ]
        );
    }

    #[test]
    fn speaker_stats_share_talk_time() {
        let mut detail = synthetic_detail(1, 0);
        detail.segments = vec![
            Segment {
                speaker_id: Some(2),
                track: "loopback".into(),
                end_ms: 3_000,
                ..seg(1, 0, "one two three")
            },
            Segment {
                end_ms: 4_000,
                ..seg(2, 3_000, "four")
            },
            Segment {
                speaker_id: None,
                track: "loopback".into(),
                end_ms: 5_000,
                ..seg(3, 4_000, "")
            },
        ];
        let index = detail
            .speakers
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        let stats = speaker_stats(&detail, &index);
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].name, "Alice");
        assert_eq!(stats[0].label, "S1");
        assert_eq!(stats[0].talk_ms, 3_000);
        assert_eq!(stats[0].words, 3);
        assert!((stats[0].share - 60.0).abs() < 1e-9);
        // Unassigned: `me` for the mic track, `S?` for loopback.
        assert_eq!(stats[1].label, "me");
        assert_eq!(stats[2].label, "S?");
        assert_eq!(stat_text(&stats[0]), "3 s · 3 words · 60%");
        assert_eq!(legend_text(&stats[0]), "3 s · 60%");
        assert!(speaker_stats(&synthetic_detail(1, 0), &index).is_empty());
    }

    #[test]
    fn empty_states_follow_the_meeting_status() {
        let mut detail = synthetic_detail(1, 0);
        let text = |d: &MeetingDetail, p| empty_message(d, p).0.join(" / ");
        detail.meeting.status = "recorded".into();
        assert_eq!(text(&detail, None), "Not transcribed yet.");
        detail.pipeline_queued = true;
        assert_eq!(text(&detail, None), "Queued for transcription");
        detail.meeting.status = "processing".into();
        assert_eq!(text(&detail, None), "Transcription in progress…");
        assert_eq!(text(&detail, Some(("asr", 41.6))), "Transcribing (asr) 42%");
        detail.meeting.status = "failed".into();
        assert_eq!(
            empty_message(&detail, None),
            (vec!["Transcription failed.".into()], true)
        );
        detail.last_error = Some("boom".into());
        assert_eq!(text(&detail, None), "Transcription failed: boom");
        detail.meeting.status = "recording".into();
        assert_eq!(text(&detail, None), "No transcript.");
        detail.meeting.status = "transcribed".into();
        assert!(text(&detail, None).starts_with("Transcription finished, but no speech"));
    }

    #[test]
    fn suggestions_match_case_insensitively() {
        let person = |id, name: &str| PersonInfo {
            id,
            name: name.into(),
            sample_seconds: 0.0,
            created_at: String::new(),
        };
        let people = vec![person(1, "Alice"), person(2, "Bob"), person(3, "Malik")];
        assert_eq!(suggestions(&people, "LI"), vec!["Alice", "Malik"]);
        assert_eq!(suggestions(&people, ""), vec!["Alice", "Bob", "Malik"]);
        assert_eq!(suggestions(&people, "Bob"), Vec::<&str>::new());
    }

    #[test]
    fn highlighted_text_keeps_every_character() {
        let tokens = highlight_tokens("budget");
        let text = "The Budget and the budget.";
        let job = text_job(text, &highlight_ranges(text, &tokens), None);
        assert_eq!(job.text, "The Budget and the budget.");
        let marks = job
            .sections
            .iter()
            .filter(|s| s.format.background == pal().mark_bg)
            .count();
        assert_eq!(marks, 2);
    }

    #[test]
    fn copy_to_notes_appends_and_never_replaces() {
        assert_eq!(
            notes_with_summary("", "## Summary\nx").as_deref(),
            Some("## Summary\nx")
        );
        let once = notes_with_summary("my notes\n\n", "## Summary\nx").unwrap();
        assert_eq!(once, "my notes\n\n## Summary\nx");
        // A second "Copy to notes" does not append it again.
        assert_eq!(notes_with_summary(&once, "## Summary\nx\n"), None);
        assert_eq!(notes_with_summary("notes", "  "), None);
        let mut debug = AiDebug::default();
        assert!(!debug.configured(false) && debug.configured(true));
        debug.state = Some("off".into());
        assert!(!debug.configured(true));
        debug.state = Some("busy".into());
        assert!(debug.configured(false));
        assert!(matches!(
            debug.task(AiTask::Summary),
            Some(AiTaskState::Busy(_))
        ));
        assert_eq!(debug.task(AiTask::Title), None);
    }

    #[test]
    fn notes_save_through_the_writer_with_acks() {
        let mut notes = Notes::default();
        notes.load("old", None, None);
        assert!(notes.open);
        assert_eq!(notes.take_flush(0), None, "nothing changed");
        notes.load("ignored on reload", None, None);
        assert_eq!(notes.draft, "old");

        notes.draft = "new".into();
        notes.edited();
        assert_eq!(notes.indicator(), Some("Saving…"));
        assert_eq!(notes.take_flush(0).as_deref(), Some("new"));
        notes.sent(5);
        assert_eq!(notes.take_flush(0), None, "already queued");
        assert_eq!(notes.indicator(), Some("Saving…"), "write in flight");
        assert_eq!(notes.on_ack(Some(&(4, None))), None);
        assert_eq!(notes.indicator(), Some("Saving…"), "older ack");
        assert_eq!(notes.on_ack(Some(&(5, None))), None);
        assert_eq!(notes.indicator(), Some("Saved"));
        assert_eq!(notes.confirmed, "new");

        // A failed write toasts once and is retried by the next flush.
        notes.draft = "newer".into();
        notes.edited();
        assert_eq!(notes.take_flush(5).as_deref(), Some("newer"));
        notes.sent(6);
        let ack = (6, Some("disk full".to_string()));
        assert_eq!(notes.on_ack(Some(&ack)).as_deref(), Some("disk full"));
        assert_eq!(notes.on_ack(Some(&ack)), None, "toasted once");
        assert_eq!(notes.indicator(), None);
        assert_eq!(notes.take_flush(6).as_deref(), Some("newer"), "retried");
    }

    #[test]
    fn notes_saved_state_when_typing_back() {
        let mut notes = Notes::default();
        notes.load("", None, None);
        assert!(!notes.open);
        notes.draft = "x".into();
        notes.edited();
        notes.draft.clear();
        notes.edited();
        // Back to the stored text: the flush has nothing to write.
        assert_eq!(notes.take_flush(0), None);
        assert_eq!(notes.indicator(), Some("Saved"));
    }

    #[test]
    fn synthetic_meeting_is_well_formed() {
        let detail = synthetic_detail(9, 3_000);
        assert_eq!(detail.segments.len(), 3_000);
        assert!(detail
            .segments
            .windows(2)
            .all(|w| w[0].start_ms < w[1].start_ms));
        assert_eq!(detail.bookmarks.len(), 20);
        assert!(detail.speakers.iter().any(|s| s.auto_labeled));
    }

    /// Run the virtualised list for `frames` passes in a 800×600 window.
    fn run_list(
        list: &mut RowList,
        detail: &MeetingDetail,
        req: Option<ScrollReq>,
        frames: usize,
    ) -> ListOutput {
        run_list_sized(list, detail, req, frames, 800.0)
    }

    thread_local! {
        /// One context per test thread, so successive runs share scroll state.
        static CTX: egui::Context = theme::test_context();
    }

    fn run_list_sized(
        list: &mut RowList,
        detail: &MeetingDetail,
        req: Option<ScrollReq>,
        frames: usize,
        width: f32,
    ) -> ListOutput {
        run_list_editing(list, detail, req, frames, width, &mut None)
    }

    /// `run_list_sized` with a speaker editor that may be open; the
    /// output's actions are every pass's.
    fn run_list_editing(
        list: &mut RowList,
        detail: &MeetingDetail,
        req: Option<ScrollReq>,
        frames: usize,
        width: f32,
        speaker_edit: &mut Option<SpeakerEdit>,
    ) -> ListOutput {
        run_list_full(list, detail, req, frames, width, speaker_edit, None)
    }

    /// `run_list_editing` with find's hits highlighted.
    fn run_list_full(
        list: &mut RowList,
        detail: &MeetingDetail,
        mut req: Option<ScrollReq>,
        frames: usize,
        width: f32,
        speaker_edit: &mut Option<SpeakerEdit>,
        find: Option<&FindState>,
    ) -> ListOutput {
        let ctx = CTX.with(|ctx| ctx.clone());
        let speakers: HashMap<i64, usize> = detail
            .speakers
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        let mut last = ListOutput::default();
        let mut settled_error = None;
        let mut actions = Vec::new();
        for _ in 0..frames {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 600.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("test-list")
                    .auto_shrink(false)
                    .show_viewport(ui, |ui, _| {
                        let mut bookmark_edit = None;
                        let mut rc = RowCtx {
                            detail,
                            speakers: &speakers,
                            tokens: &[],
                            active: None,
                            speaker_edit: &mut *speaker_edit,
                            bookmark_edit: &mut bookmark_edit,
                            people: &[],
                            focus_seg: &mut None,
                            find,
                        };
                        last = list.show(ui, &mut rc, req);
                    });
            });
            output.textures_delta.clear();
            actions.append(&mut last.actions);
            if last.target_error.is_some() {
                settled_error = last.target_error;
            }
            // The view's settle logic.
            req = req.and_then(|r| r.next(&last));
        }
        last.target_error = settled_error;
        last.actions = actions;
        last
    }

    #[test]
    fn long_transcripts_lay_out_only_visible_rows() {
        let detail = synthetic_detail(1, 3_000);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        let started = Instant::now();
        let out = run_list(&mut list, &detail, None, 3);
        assert!(out.rendered > 0);
        assert!(
            out.rendered < 120,
            "virtualisation laid out {} rows",
            out.rendered
        );
        let measured = list.heights.iter().filter(|h| h.is_some()).count();
        assert!(measured < 150, "measured {measured} rows");
        // Debug build, three passes: generous bound, catches O(n) layout.
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn deep_link_centres_a_far_row() {
        let detail = synthetic_detail(1, 3_000);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        let req = ScrollReq::new(2_400, true, false);
        // The view re-aims for up to `SCROLL_SETTLE_FRAMES` passes (the scroll
        // bar animating in narrows the rows, and egui applies some scrolls a
        // pass late).
        let out = run_list(&mut list, &detail, Some(req), SCROLL_SETTLE_FRAMES.into());
        let error = out.target_error.expect("target row laid out");
        assert!(
            error.abs() <= SCROLL_SETTLED_PX * 2.0,
            "off centre by {error}"
        );
    }

    #[test]
    fn a_resize_keeps_the_reading_position() {
        let detail = synthetic_detail(1, 3_000);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        let req = ScrollReq::new(1_500, true, false);
        run_list_sized(&mut list, &detail, Some(req), 12, 800.0);
        let (key, above) = list.anchor.expect("anchored after the jump");
        // Narrower: every row above reflows taller.
        run_list_sized(&mut list, &detail, None, 6, 420.0);
        let (after, above_after) = list.anchor.expect("still anchored");
        let index = |k: RowKey| list.keys.iter().position(|x| *x == k).unwrap();
        // The same row stays at the top, at the same offset: rows drawn in
        // the resize pass count at their reflowed heights, and rows
        // measured at the old width are estimated at the new one.
        assert_eq!(index(after), index(key), "drifted");
        assert!(
            (above_after - above).abs() <= 2.0,
            "{above} -> {above_after}"
        );
    }

    #[test]
    fn a_measurement_at_another_width_yields_to_the_estimate() {
        let detail = synthetic_detail(1, 10);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        list.heights[4] = Some((800.0, 999.0));
        // The scroll bar fading in: still the measurement.
        assert_eq!(list.height(&detail, 4, 795.0), 999.0);
        // Reflowed: the estimate at the new width.
        let estimate = estimate_height(&detail, list.rows[4], list.shape(4), 420.0);
        assert_eq!(list.height(&detail, 4, 420.0), estimate);
    }

    #[test]
    fn height_cache_survives_a_rebuild() {
        let mut detail = synthetic_detail(1, 50);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        list.heights[3] = Some((700.0, 99.0));
        let key = list.keys[3];
        detail.bookmarks.clear();
        list.rebuild(&detail, |_| true);
        let row = list.keys.iter().position(|k| *k == key).unwrap();
        assert_eq!(list.heights[row], Some((700.0, 99.0)));
        assert_eq!(list.seg_row.len(), 50);
    }

    /// Route a pass's row actions the way `apply_actions` does.
    fn route_all(state: &mut TranscriptState, actions: Vec<RowAction>) -> Vec<EditorEffect> {
        let deleted: Vec<i64> = actions
            .iter()
            .filter_map(|a| match a {
                RowAction::DeleteBookmark(id) => Some(*id),
                _ => None,
            })
            .collect();
        editor_results_first(actions)
            .into_iter()
            .filter_map(|action| state.route_editor(action, &deleted))
            .collect()
    }

    #[test]
    fn a_click_on_a_row_above_commits_the_open_rename_first() {
        let mut state = TranscriptState::new(1, None, None);
        // Speaker 2 is Alice, speaker 3 Bob (≈); the seg ids name the rows
        // whose names were clicked.
        state.detail = Some(synthetic_detail(1, 10));
        let mut open = SpeakerEdit::new(2, 6, "Alice");
        open.edit.text = "Alicia".into();
        state.speaker_edit = Some(open);
        // Bob's chip sits above: its Open was pushed before the blur.
        let effects = route_all(
            &mut state,
            vec![
                RowAction::OpenSpeaker {
                    speaker_id: 3,
                    seg_id: 3,
                },
                RowAction::SpeakerDone {
                    seg_id: 6,
                    outcome: EditOutcome::Commit,
                },
            ],
        );
        assert_eq!(effects.len(), 2, "{effects:?}");
        match &effects[0] {
            EditorEffect::CommitSpeaker(closed, name) => {
                assert_eq!((closed.speaker_id, closed.seg_id), (2, 6));
                assert_eq!(name, "Alicia");
            }
            other => panic!("expected Alice's commit, got {other:?}"),
        }
        assert!(matches!(effects[1], EditorEffect::LoadPeople));
        // Bob's editor is open with his name; he was not renamed/confirmed.
        let bob = state.speaker_edit.as_ref().expect("Bob's editor open");
        assert_eq!((bob.speaker_id, bob.seg_id), (3, 3));
        assert_eq!(bob.edit.text, "Bob");
    }

    #[test]
    fn an_editor_result_only_closes_its_own_editor() {
        let mut state = TranscriptState::new(1, None, None);
        // Bookmarks 1–3 (one every 150 segments).
        state.detail = Some(synthetic_detail(1, 400));
        state.speaker_edit = Some(SpeakerEdit::new(3, 3, "Bob"));
        let stale = route_all(
            &mut state,
            vec![RowAction::SpeakerPick {
                seg_id: 6,
                name: "Carol".into(),
            }],
        );
        assert!(stale.is_empty());
        assert_eq!(state.speaker_edit.as_ref().map(|e| e.seg_id), Some(3));

        // A note blurred by its own trash button is not saved.
        state.bookmark_edit = Some(BookmarkEdit {
            id: 1,
            edit: LineEdit::new("typed"),
        });
        let effects = route_all(
            &mut state,
            vec![
                RowAction::BookmarkDone {
                    id: 1,
                    outcome: EditOutcome::Commit,
                },
                RowAction::DeleteBookmark(1),
            ],
        );
        assert!(effects.is_empty(), "{effects:?}");
        assert!(state.bookmark_edit.is_none());

        // Another note's click commits the open one, then opens its own.
        state.bookmark_edit = Some(BookmarkEdit {
            id: 1,
            edit: LineEdit::new("typed"),
        });
        let effects = route_all(
            &mut state,
            vec![
                RowAction::EditBookmark(2),
                RowAction::BookmarkDone {
                    id: 1,
                    outcome: EditOutcome::Commit,
                },
            ],
        );
        assert!(
            matches!(&effects[..], [EditorEffect::CommitBookmark(b)] if b.id == 1 && b.edit.text == "typed")
        );
        assert_eq!(state.bookmark_edit.as_ref().map(|e| e.id), Some(2));
    }

    #[test]
    fn an_open_editor_keeps_focus_when_its_row_scrolls_far_away() {
        let detail = synthetic_detail(1, 3_000);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        let mut edit = Some(SpeakerEdit::new(3, 3, "Bob"));
        let top = Some(ScrollReq::new(0, false, false));
        run_list_editing(&mut list, &detail, top, 3, 800.0, &mut edit);
        let field = Id::new(("speaker-rename", 3_i64));
        let focused = || CTX.with(|ctx| ctx.memory(|m| m.focused()));
        assert_eq!(focused(), Some(field));

        let far = Some(ScrollReq::new(1_000, true, false));
        let out = run_list_editing(&mut list, &detail, far, 8, 800.0, &mut edit);
        assert!(out.target_error.is_some(), "jumped to the far row");
        assert_eq!(focused(), Some(field), "the editor kept its focus");
        assert!(
            out.actions.is_empty(),
            "nothing committed: {:?}",
            out.actions
        );
        assert!(edit.is_some());
    }

    #[test]
    fn menu_keys_wrap_and_jump() {
        assert_eq!(menu_key_target(Key::ArrowDown, 4, 5), Some(0));
        assert_eq!(menu_key_target(Key::ArrowUp, 0, 5), Some(4));
        assert_eq!(menu_key_target(Key::ArrowDown, 1, 5), Some(2));
        assert_eq!(menu_key_target(Key::Home, 3, 5), Some(0));
        assert_eq!(menu_key_target(Key::End, 0, 2), Some(1));
        assert_eq!(menu_key_target(Key::ArrowLeft, 1, 2), None);
        assert_eq!(menu_key_target(Key::ArrowDown, 0, 0), None);
    }

    #[test]
    fn reopening_prefers_the_newest_queued_notes() {
        // The read raced the last visit's write: the queued draft wins and
        // its (successful) ack confirms it quietly.
        let mut notes = Notes::default();
        notes.load("db", Some((7, "newer".into())), None);
        assert_eq!(notes.draft, "newer");
        assert!(notes.open);
        assert_eq!(notes.take_flush(0), None, "already queued");
        assert_eq!(notes.on_ack(Some(&(7, None))), None);
        assert_eq!(notes.confirmed, "newer");
        assert_eq!(notes.indicator(), None);

        // That write failed after the view was gone: report it now, and
        // the next flush retries the draft.
        let mut notes = Notes::default();
        let ack = (7, Some("disk full".to_string()));
        notes.load("db", Some((7, "newer".into())), Some(&ack));
        assert_eq!(notes.draft, "newer");
        assert_eq!(notes.on_ack(Some(&ack)).as_deref(), Some("disk full"));
        assert_eq!(notes.take_flush(7).as_deref(), Some("newer"));

        // The database already has it: a plain load.
        let mut notes = Notes::default();
        notes.load("same", Some((3, "same".into())), None);
        assert_eq!((notes.seq, notes.draft.as_str()), (0, "same"));

        // Its write succeeded before the read began: the database is the
        // truth, even when it differs.
        let mut notes = Notes::default();
        notes.load("db", Some((7, "newer".into())), Some(&(7, None)));
        assert_eq!((notes.seq, notes.draft.as_str()), (0, "db"));
        assert_eq!(notes.take_flush(7), None);
    }

    #[test]
    fn an_ack_landing_after_the_read_never_lets_the_stale_read_win() {
        // Visit 1 queued seq 7; the reopen spawns its read before the ack.
        let mut shell = ShellModel::new();
        let at_spawn = shell.notes_acks.get(&1).cloned();
        // The read got the old text, then the writer saved seq 7 and its
        // ack was applied before the view consumed the read.
        shell.apply(&CoreEvent::NotesSaved {
            meeting_id: 1,
            seq: 7,
        });
        let mut notes = Notes::default();
        notes.load("X", Some((7, "X Y".into())), at_spawn.as_ref());
        assert_eq!(notes.draft, "X Y", "the saved draft, not the stale read");
        assert_eq!(notes.on_ack(shell.notes_acks.get(&1)), None);
        assert_eq!(notes.confirmed, "X Y");
        // Typing on builds on the saved text.
        notes.draft = "X Y Z".into();
        notes.edited();
        assert_eq!(notes.take_flush(7).as_deref(), Some("X Y Z"));
    }

    #[test]
    fn a_purged_meetings_queued_notes_stay_out_of_its_id_successor() {
        let queued = QueuedNotes {
            started_at: "2026-10-01T09:00:00+01:00".into(),
            seq: 7,
            text: "purged meeting's notes".into(),
        };
        assert_eq!(
            queued_notes_for(Some(&queued), "2026-10-01T09:00:00+01:00"),
            Some((7, "purged meeting's notes".into()))
        );
        // The successor's write is still unacked and the draft differs, so
        // only the started_at match keeps it out.
        let mut notes = Notes::default();
        notes.load(
            "",
            queued_notes_for(Some(&queued), "2026-10-06T14:00:00+01:00"),
            None,
        );
        assert_eq!((notes.seq, notes.draft.as_str()), (0, ""));
        assert!(!notes.open);
    }

    #[test]
    fn live_feed_follows_only_from_near_the_bottom() {
        let mut feed = LiveFeedState::default();
        assert!(feed.stick(Some(("mic", 1_000))), "first caption");
        feed.scrolled(true, 300.0);
        assert!(!feed.stick(Some(("mic", 1_000))), "nothing new");
        // The reader scrolled up 100 px: new captions leave them there.
        feed.scrolled(false, 100.0);
        assert!(!feed.stick(Some(("loopback", 2_000))));
        // Back within 40 px: the next caption scrolls again.
        feed.scrolled(false, 30.0);
        assert!(feed.stick(Some(("mic", 3_000))));
    }

    /// One pass with AccessKit on; returns the tree update.
    fn accesskit_pass(
        ctx: &egui::Context,
        f: impl FnMut(&mut egui::Ui),
    ) -> egui::accesskit::TreeUpdate {
        ctx.enable_accesskit();
        let mut output = ctx.run_ui(egui::RawInput::default(), f);
        output.textures_delta.clear();
        output
            .platform_output
            .accesskit_update
            .expect("accesskit update")
    }

    fn node_label(update: &egui::accesskit::TreeUpdate, id: Id) -> Option<String> {
        let wanted = id.accesskit_id();
        update
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == wanted)
            .and_then(|(_, node)| node.label().map(str::to_owned))
    }

    #[test]
    fn inline_editors_have_accessible_names() {
        let ctx = theme::test_context();
        let mut edit = LineEdit::new("");
        let id = Id::new("title-under-test");
        let update = accesskit_pass(&ctx, |ui| {
            inline_editor(
                ui,
                id,
                &mut edit,
                theme::regular(14.0),
                200.0,
                "",
                "Meeting title",
            );
        });
        assert_eq!(node_label(&update, id).as_deref(), Some("Meeting title"));
    }

    #[test]
    fn deep_link_focuses_the_hits_seek_button() {
        let detail = synthetic_detail(1, 6);
        let speakers: HashMap<i64, usize> = detail
            .speakers
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        // Both seek buttons: a turn header's time and the left-padding
        // target of a row without a header.
        let (head, plain) = (2, 3);
        assert!(list.heads[list.seg_row[head]] && !list.heads[list.seg_row[plain]]);
        for idx in [head, plain] {
            let ctx = theme::test_context();
            let mut focus_seg = Some(detail.segments[idx].id);
            let mut update = None;
            for _ in 0..2 {
                update = Some(accesskit_pass(&ctx, |ui| {
                    let (mut speaker_edit, mut bookmark_edit) = (None, None);
                    let mut rc = RowCtx {
                        detail: &detail,
                        speakers: &speakers,
                        tokens: &[],
                        active: Some(idx),
                        speaker_edit: &mut speaker_edit,
                        bookmark_edit: &mut bookmark_edit,
                        people: &[],
                        focus_seg: &mut focus_seg,
                        find: None,
                    };
                    egui::ScrollArea::vertical().show_viewport(ui, |ui, _| {
                        list.show(ui, &mut rc, None);
                    });
                }));
            }
            assert_eq!(focus_seg, None, "{idx}: the focus request was not taken");
            let update = update.unwrap();
            let focused = update
                .nodes
                .iter()
                .find(|(id, _)| *id == update.focus)
                .and_then(|(_, node)| node.label().map(str::to_owned));
            let expected = format!("Play at {}", fmt_ts(detail.segments[idx].start_ms));
            assert_eq!(focused.as_deref(), Some(expected.as_str()), "{idx}");
        }
    }

    #[test]
    fn turns_start_on_a_new_speaker_or_after_a_bookmark() {
        let mut detail = synthetic_detail(1, 0);
        let on = |id, speaker: Option<i64>, track: &str, start| Segment {
            speaker_id: speaker,
            track: track.into(),
            ..seg(id, start, "text")
        };
        detail.segments = vec![
            on(1, Some(2), "loopback", 0),
            on(2, Some(2), "loopback", 1_000),
            on(3, Some(3), "loopback", 2_000),
            on(4, None, "mic", 3_000),
            on(5, None, "mic", 4_000),
            on(6, None, "loopback", 5_000),
            on(7, None, "loopback", 7_000),
        ];
        detail.bookmarks = vec![Bookmark {
            id: 1,
            meeting_id: 1,
            at_ms: 6_000,
            note: String::new(),
        }];
        let rows = build_rows(&detail, |_| true);
        assert_eq!(rows[6], Row::Bm(0));
        assert_eq!(
            turn_starts(&detail, &rows),
            // An unassigned segment groups by its track; a bookmark breaks
            // the turn and never starts one itself.
            [true, false, true, true, false, true, false, true]
        );
    }

    #[test]
    fn a_cached_height_is_dropped_when_the_row_gains_a_header() {
        let mut detail = synthetic_detail(1, 20);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        // A row in the middle of a turn, measured.
        let r = (1..list.rows.len())
            .find(|&r| !list.heads[r])
            .expect("a row without a header");
        let Row::Seg(i) = list.rows[r] else {
            panic!("segment expected")
        };
        list.heights[r] = Some((700.0, 42.0));
        let other = list.keys[r + 1];
        list.heights[r + 1] = Some((700.0, 43.0));
        // A bookmark lands just before it: it now starts a turn.
        detail.bookmarks.push(Bookmark {
            id: 99,
            meeting_id: 1,
            at_ms: detail.segments[i].start_ms - 1,
            note: String::new(),
        });
        list.rebuild(&detail, |_| true);
        let row = list.seg_row[i];
        assert!(list.heads[row]);
        assert_eq!(list.heights[row], None, "measured without its header");
        // Rows whose shape did not change keep theirs.
        let kept = list.keys.iter().position(|k| *k == other).unwrap();
        assert_eq!(list.heights[kept], Some((700.0, 43.0)));
    }

    #[test]
    fn a_turns_first_row_is_taller_by_its_header() {
        let mut detail = synthetic_detail(1, 0);
        detail.segments = (0..4)
            .map(|i| Segment {
                speaker_id: Some(if i < 2 { 2 } else { 3 }),
                track: "loopback".into(),
                ..seg(i + 1, i * 1_000, "same short line")
            })
            .collect();
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        run_list(&mut list, &detail, None, 2);
        let h = |r: usize| list.heights[r].expect("measured").1;
        // Row 0: header, no turn gap (first row); row 1: text only; row 2:
        // a new turn, so the gap too.
        assert!(
            (h(0) - h(1) - (HEADER_H + HEADER_GAP)).abs() < 1.0,
            "{} {}",
            h(0),
            h(1)
        );
        assert!((h(2) - h(0) - TURN_GAP).abs() < 1.0, "{} {}", h(2), h(0));
        // The estimates have the same shape.
        let est = |r| estimate_height(&detail, list.rows[r], list.shape(r), 800.0);
        assert!((est(0) - h(0)).abs() < 3.0, "{} vs {}", est(0), h(0));
        assert!((est(1) - h(1)).abs() < 3.0, "{} vs {}", est(1), h(1));
        assert!((est(2) - h(2)).abs() < 3.0, "{} vs {}", est(2), h(2));
    }

    #[test]
    fn summary_segments_fill_the_bar() {
        let widths = summary_widths(&[57.0, 33.0, 9.0, 1.0, 0.0], 338.0);
        let total: f32 = widths.iter().sum::<f32>() + SUMMARY_GAP * 4.0;
        assert!((total - 338.0).abs() < 0.01, "{total}");
        // Ordered like the shares, and a 0 % voice still shows.
        assert!(widths.windows(2).all(|w| w[0] >= w[1]), "{widths:?}");
        assert!((widths[4] - SUMMARY_MIN_SEG).abs() < 0.01);
        assert_eq!(summary_widths(&[], 100.0), Vec::<f32>::new());
        assert_eq!(summary_widths(&[0.0, 0.0], 100.0), vec![49.0, 49.0]);
    }

    #[test]
    fn the_summary_fits_at_the_narrowest_width() {
        // The 380 pt window's column: 380 − 2 × 16 padding − the scroll bar.
        let width = 338.0;
        let ctx = theme::test_context();
        let detail = synthetic_detail(1, 4);
        let mut state = TranscriptState::new(1, None, None);
        state.speakers = detail
            .speakers
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        state.stats = speaker_stats(&detail, &state.speakers);
        state.stats[0].name = "Somebody With An Extremely Long Display Name".into();
        state.detail = Some(detail);
        let mut column = Rect::NOTHING;
        let mut content = Rect::NOTHING;
        for _ in 0..2 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 600.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(width);
                        column = ui.max_rect();
                        state.speaker_summary(ui);
                        content = ui.min_rect();
                    },
                );
            });
            output.textures_delta.clear();
        }
        assert!(
            content.right() <= column.right() + 0.5,
            "{content:?} in {column:?}"
        );
    }

    #[test]
    fn menu_item_focus_without_keyboard_is_not_highlighted() {
        let ctx = theme::test_context();
        let id = Id::new("menu-under-test");
        let mut fills = 0;
        for pass in 0..2 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.push_id(id, |ui| {
                    let item = menu_item(ui, "Markdown…");
                    if pass == 0 {
                        item.request_focus();
                    }
                });
            });
            output.textures_delta.clear();
            if pass == 1 {
                fills = output
                    .shapes
                    .iter()
                    .filter(|clipped| match &clipped.shape {
                        egui::Shape::Rect(rect) => rect.fill == pal().bg_hover,
                        _ => false,
                    })
                    .count();
            }
        }
        assert!(ctx.memory(|m| m.focused().is_some()), "item not focused");
        assert_eq!(fills, 0, "a focused, unhovered item was highlighted");
    }

    #[test]
    fn find_folds_case_and_accents() {
        let needle = find_needle("  CAFE ");
        assert_eq!(needle, "cafe");
        let text = "Café crème, then the CAFE again";
        let hits = find_in(text, &needle);
        assert_eq!(
            hits.iter().map(|r| &text[r.clone()]).collect::<Vec<_>>(),
            ["Café", "CAFE"]
        );
        // Decomposed accents match too, and the hit covers the mark.
        let text = "cafe\u{301} au lait";
        let hits = find_in(text, &find_needle("café"));
        assert_eq!((hits.len(), &hits[0]), (1, &(0..6)));
        // The accent in the query is optional the other way round as well.
        let zoe = find_in("Zoë and Łukasz", &find_needle("zoe"));
        assert_eq!((zoe.len(), &zoe[0]), (1, &(0..4)));
        assert_eq!(find_in("Zoë and Łukasz", &find_needle("lukasz")).len(), 1);
        // Not overlapping; empty finds nothing.
        assert_eq!(find_in("aaaa", "aa"), [0..2, 2..4]);
        assert!(find_in("text", "").is_empty());
        assert!(find_needle(" \u{301} ").is_empty());
    }

    #[test]
    fn latin_extended_a_folds_to_its_base_letters() {
        let cases = [
            ('ā', 'a'),
            ('č', 'c'),
            ('đ', 'd'),
            ('ę', 'e'),
            ('ğ', 'g'),
            ('ħ', 'h'),
            ('ı', 'i'),
            ('ĵ', 'j'),
            ('ķ', 'k'),
            ('ł', 'l'),
            ('ŋ', 'n'),
            ('ő', 'o'),
            ('ř', 'r'),
            ('ś', 's'),
            ('ť', 't'),
            ('ů', 'u'),
            ('ŵ', 'w'),
            ('ŷ', 'y'),
            ('ž', 'z'),
            ('ſ', 's'),
            // Ligatures stay.
            ('ĳ', 'ĳ'),
            ('œ', 'œ'),
        ];
        for (c, base) in cases {
            assert_eq!(fold_char(c), base, "{c}");
        }
    }

    #[test]
    fn find_steps_wrap_and_start_from_the_reading_position() {
        assert_eq!(step_hit(3, None, false), Some(0));
        assert_eq!(step_hit(3, None, true), Some(2));
        assert_eq!(step_hit(3, Some(2), false), Some(0));
        assert_eq!(step_hit(3, Some(0), true), Some(2));
        assert_eq!(step_hit(0, Some(0), false), None);
        let hit = |seg| FindHit { seg, range: 0..1 };
        let hits = [hit(2), hit(2), hit(9), hit(40)];
        assert_eq!(first_hit_from(&hits, 0), Some(0));
        assert_eq!(first_hit_from(&hits, 3), Some(2));
        assert_eq!(first_hit_from(&hits, 9), Some(2));
        assert_eq!(first_hit_from(&hits, 41), Some(0), "wraps");
        assert_eq!(first_hit_from(&[], 3), None);
        assert_eq!(find_count_text("", 0, None), "");
        assert_eq!(find_count_text("zzz", 0, None), "No matches");
        assert_eq!(find_count_text("the", 17, Some(2)), "3 of 17");
    }

    #[test]
    fn find_searches_only_the_shown_lines() {
        let detail = synthetic_detail(1, 300);
        let all = find_hits(&detail, |_| true, "BUDGET");
        assert!(!all.is_empty());
        assert!(all
            .windows(2)
            .all(|w| (w[0].seg, w[0].range.start) < (w[1].seg, w[1].range.start)));
        // Only Bob (speaker 3).
        let filter = BTreeSet::from([SpeakerKey::Speaker(3)]);
        let bob = find_hits(
            &detail,
            |i| is_shown(&filter, &detail.segments[i]),
            "budget",
        );
        assert!(!bob.is_empty() && bob.len() < all.len());
        assert!(bob
            .iter()
            .all(|h| detail.segments[h.seg].speaker_id == Some(3)));
        // A row's hits, and which of them is current.
        let find = FindState {
            query: "budget".into(),
            current: Some(1),
            hits: all.clone(),
            ..FindState::default()
        };
        let (first, in_seg) = find.hits_in(all[1].seg);
        assert!(first <= 1 && first + in_seg.len() > 1);
        assert!(in_seg.iter().all(|h| h.seg == all[1].seg));
    }

    #[test]
    fn the_current_hit_is_emphasised_and_others_marked() {
        let text = "the cat and the hat";
        let job = text_job(text, &[0..3, 12..15], Some(1));
        assert_eq!(job.text, text);
        let backgrounds: Vec<Color32> = job.sections.iter().map(|s| s.format.background).collect();
        assert!(backgrounds.contains(&pal().mark_bg));
        let strong = job
            .sections
            .iter()
            .find(|s| s.format.background == pal().accent)
            .expect("the current hit");
        let range = strong.byte_range.start.0..strong.byte_range.end.0;
        assert_eq!(range, 12..15);
    }

    #[test]
    fn find_scrolls_a_far_hit_into_the_virtualised_list() {
        let detail = synthetic_detail(1, 3_000);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        let hits = find_hits(&detail, |_| true, "latency");
        // Far down, but not so near the end that it cannot be centred.
        let current = hits.partition_point(|h| h.seg < 2_200);
        let target = hits[current].seg;
        assert!(target < 2_600, "a far hit: {target}");
        let find = FindState {
            query: "latency".into(),
            current: Some(current),
            hits,
            ..FindState::default()
        };
        run_list(&mut list, &detail, None, 2);
        assert!(!list.seg_visible(target), "starts off screen");
        let req = Some(ScrollReq::new(target, true, false));
        let out = run_list_full(
            &mut list,
            &detail,
            req,
            SCROLL_SETTLE_FRAMES.into(),
            800.0,
            &mut None,
            Some(&find),
        );
        let error = out.target_error.expect("the hit's row was laid out");
        assert!(
            error.abs() <= SCROLL_SETTLED_PX * 2.0,
            "off centre by {error}"
        );
        assert!(out.rendered < 120, "still virtualised: {}", out.rendered);
        // One more pass at rest: the hit's row is wholly on screen now.
        run_list_full(&mut list, &detail, None, 1, 800.0, &mut None, Some(&find));
        assert!(list.seg_visible(target));
    }

    #[test]
    fn filtered_rows_collapse_hidden_lines_into_runs() {
        let mut detail = synthetic_detail(1, 0);
        let on = |id, speaker: i64, start| Segment {
            speaker_id: Some(speaker),
            track: "loopback".into(),
            ..seg(id, start, "text")
        };
        detail.segments = vec![
            on(1, 2, 0),
            on(2, 3, 1_000),
            on(3, 3, 2_000),
            on(4, 2, 3_000),
            on(5, 3, 4_000),
            on(6, 3, 6_000),
            on(7, 2, 7_000),
        ];
        detail.bookmarks = vec![Bookmark {
            id: 1,
            meeting_id: 1,
            at_ms: 5_000,
            note: String::new(),
        }];
        let filter = BTreeSet::from([SpeakerKey::Speaker(2)]);
        let shown = |i: usize| is_shown(&filter, &detail.segments[i]);
        let rows = build_rows(&detail, shown);
        assert_eq!(
            rows,
            [
                Row::Seg(0),
                Row::Hidden { first: 1, count: 2 },
                Row::Seg(3),
                // A bookmark splits a hidden run.
                Row::Hidden { first: 4, count: 1 },
                Row::Bm(0),
                Row::Hidden { first: 5, count: 1 },
                Row::Seg(6),
            ]
        );
        // Every line after hidden ones starts a turn again.
        assert_eq!(
            turn_starts(&detail, &rows),
            [true, false, true, false, false, false, true]
        );
        let mut list = RowList::default();
        list.rebuild(&detail, shown);
        // Hidden segments point at the row standing in for them.
        assert_eq!(list.seg_row, [0, 1, 1, 2, 3, 5, 6]);
        assert_eq!(list.keys[1], RowKey::Hidden(2));
    }

    #[test]
    fn a_filtered_list_stays_virtualised_and_follows_into_hidden_lines() {
        let detail = synthetic_detail(1, 3_000);
        let filter = BTreeSet::from([SpeakerKey::Speaker(3)]);
        let mut list = RowList::default();
        list.rebuild(&detail, |i| is_shown(&filter, &detail.segments[i]));
        let hidden = list
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Hidden { .. }))
            .count();
        assert!(hidden > 100, "{hidden} hidden runs");
        // Follow-along to a segment of someone else's, far down: the list
        // centres the run that stands in for it.
        let far = (2_000..3_000)
            .find(|&i| !is_shown(&filter, &detail.segments[i]))
            .unwrap();
        assert!(matches!(list.rows[list.seg_row[far]], Row::Hidden { .. }));
        let req = Some(ScrollReq::new(far, true, false));
        let out = run_list(&mut list, &detail, req, SCROLL_SETTLE_FRAMES.into());
        let error = out.target_error.expect("laid out");
        assert!(
            error.abs() <= SCROLL_SETTLED_PX * 2.0,
            "off centre by {error}"
        );
        assert!(out.rendered < 160, "virtualised: {}", out.rendered);
    }

    #[test]
    fn the_reading_anchor_moves_to_the_row_holding_its_line() {
        let detail = synthetic_detail(1, 400);
        let mut list = RowList::default();
        list.rebuild(&detail, |_| true);
        // Reading Bob's (speaker 3) line; then only speaker 4 is shown.
        let bob = detail
            .segments
            .iter()
            .position(|s| s.speaker_id == Some(3) && s.start_ms > 100_000)
            .unwrap();
        list.anchor = Some((RowKey::Seg(detail.segments[bob].id), 12.0));
        let filter = BTreeSet::from([SpeakerKey::Speaker(4)]);
        list.rebuild(&detail, |i| is_shown(&filter, &detail.segments[i]));
        let (key, above) = list.anchor.expect("still anchored");
        assert_eq!(key, list.keys[list.seg_row[bob]]);
        assert!(matches!(key, RowKey::Hidden(_)));
        assert_eq!(above, 0.0);
        list.keep_reading_position();
        assert_eq!(list.resize_settle, RESIZE_SETTLE_PASSES);
        // Shown again: back on the line itself.
        list.rebuild(&detail, |_| true);
        assert!(matches!(list.anchor, Some((RowKey::Seg(_), _))));
        assert!(list.reading_segment() <= bob);
    }

    #[test]
    fn legend_clicks_toggle_the_filter() {
        let (a, b) = (SpeakerKey::Speaker(1), SpeakerKey::Unassigned { mic: true });
        let mut filter = BTreeSet::new();
        toggle_filter(&mut filter, a, false);
        assert_eq!(filter, BTreeSet::from([a]));
        // Another speaker alone replaces it; Ctrl adds.
        toggle_filter(&mut filter, b, false);
        assert_eq!(filter, BTreeSet::from([b]));
        toggle_filter(&mut filter, a, true);
        assert_eq!(filter, BTreeSet::from([a, b]));
        // A plain click on a selected one of several: only them.
        toggle_filter(&mut filter, a, false);
        assert_eq!(filter, BTreeSet::from([a]));
        // Again: everyone.
        toggle_filter(&mut filter, a, false);
        assert!(filter.is_empty());
        // Ctrl-removing the last one shows everyone too.
        toggle_filter(&mut filter, b, true);
        toggle_filter(&mut filter, b, true);
        assert!(filter.is_empty());
        assert_eq!(
            filter_hint(&BTreeSet::from([a]), a),
            "Click to show everyone"
        );
    }

    #[test]
    fn the_timeline_follows_the_summary_and_the_filter() {
        let detail = synthetic_detail(1, 40);
        let index: HashMap<i64, usize> = detail
            .speakers
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        let stats = speaker_stats(&detail, &index);
        let filter = BTreeSet::from([stats[1].key]);
        let timeline = build_timeline(&detail, &stats, &filter, 7);
        assert_eq!(timeline.version, 7);
        assert_eq!(timeline.spans.len(), 40);
        assert_eq!(timeline.speakers.len(), stats.len());
        for (speaker, stat) in timeline.speakers.iter().zip(&stats) {
            assert_eq!(speaker.name, stat.name);
            assert_eq!(speaker.dimmed, stat.key != stats[1].key);
        }
        // Each span points at its segment's speaker.
        for (span, seg) in timeline.spans.iter().zip(&detail.segments) {
            assert_eq!(stats[usize::from(span.speaker)].key, speaker_key(seg));
        }
        assert_eq!(filter_names(&stats, &filter), stats[1].name);
        let two = BTreeSet::from([stats[0].key, stats[1].key]);
        assert_eq!(
            filter_names(&stats, &two),
            format!("{} and {}", stats[0].name, stats[1].name)
        );
    }

    #[test]
    fn the_detail_pane_header_has_no_back_button() {
        let full = header_layout(PaneRole::Full);
        assert!(full.back);
        assert_eq!(full.title_size, TITLE_SIZE);
        assert_eq!(header_layout(PaneRole::List { selected: None }), full);
        let detail = header_layout(PaneRole::Detail);
        assert!(!detail.back);
        assert!(detail.title_size < full.title_size);
        assert!(detail.gap_below < full.gap_below);
    }

    #[test]
    fn debug_find_specs_split_an_index_off() {
        assert_eq!(parse_debug_find("the,5"), ("the".into(), Some(5)));
        assert_eq!(parse_debug_find("the"), ("the".into(), None));
        assert_eq!(parse_debug_find("a, b"), ("a, b".into(), None));
    }

    #[test]
    fn find_keys_match_modifiers_exactly() {
        let press = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let mut input = egui::InputState::default();
        input.events = vec![
            press(Key::Enter, Modifiers::SHIFT),
            press(Key::F, Modifiers::COMMAND | Modifiers::ALT),
        ];
        let shift = consume_press(&mut input, Key::Enter, |m| !m.command && !m.alt);
        assert!(shift.is_some_and(|m| m.shift));
        // Ctrl+Alt+F (AltGr+F) is not Ctrl+F.
        assert!(consume_press(&mut input, Key::F, |m| m.command && !m.alt).is_none());
        assert_eq!(input.events.len(), 1);
    }
}
