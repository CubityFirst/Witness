//! The Ctrl+K command palette: jump to any meeting or person, search
//! transcripts, or run an action (record, settings, theme…), from anywhere.
//!
//! The shell owns one [`CommandPalette`], toggles it on Ctrl+K and draws it
//! above the views with the same [`UiCtx`] they get; everything it does goes
//! through `cx` (navigation, recording control, prefs, tasks).
//!
//! Opening it (re)loads the meeting list and the people off the UI thread;
//! the previous lists stay on screen meanwhile, so a second open never
//! flashes empty. Results are ranked by a small fuzzy matcher (subsequence
//! with word-start and run bonuses, see [`fuzzy`]) and grouped under
//! section headers; the groups follow their best match, and "Search
//! transcripts for …" is always the last row. The pure parts — matching,
//! ranking, grouping, action availability, selection — are plain functions,
//! so the tests cover them without an `AppCore`.

use super::icons::{self, ph};
use super::theme::{self, pal, S1, S2, S3, S4};
use super::widgets;
use super::{UiCtx, View};
use crate::bridge::{OpDone, Task};
use crate::ui_state::Appearance;
use chrono::{DateTime, Datelike, Local, NaiveDate};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Key, Margin, Modifiers, Rect,
    RichText, Sense, Stroke, WidgetInfo, WidgetType,
};
use witness_core::db::Meeting;
use witness_core::service::PersonInfo;
use witness_core::{CoreResult, RecCmd, Trigger};

/// The surface's widest (it is `window − 32` below that).
const MAX_WIDTH: f32 = 600.0;
/// The search row's height.
const INPUT_H: f32 = 52.0;
/// One result row.
const ROW_H: f32 = 36.0;
/// A section header above a group of rows.
const HEADER_H: f32 = 28.0;
/// The footer with the key hints.
const FOOTER_H: f32 = 36.0;
/// Inset of the rows from the surface edges (and above / below the list).
const LIST_PAD: f32 = 6.0;
/// Rows visible before the list scrolls.
const VISIBLE_ROWS: f32 = 8.0;
/// The glyph / avatar box at the start of each row.
const GLYPH_BOX: f32 = 22.0;
/// Where the glyphs centre and the titles start, from the surface's left
/// edge (the search field lines up with the rows).
const GLYPH_X: f32 = LIST_PAD + S3 + GLYPH_BOX / 2.0;
const TEXT_X: f32 = LIST_PAD + S3 + GLYPH_BOX + S3 - 2.0;
/// Meetings shown with an empty query.
const RECENT_MEETINGS: usize = 6;
/// The most rows a group shows for a query.
const MAX_MEETINGS: usize = 20;
const MAX_PEOPLE: usize = 8;
/// How many meetings the palette knows (the core's page cap).
const MEETINGS_LOADED: i64 = 500;
/// Matching looks at this many characters of a title at most.
const MATCH_CHARS: usize = 256;
/// Below this surface width the placeholder shortens.
const NARROW_SURFACE: f32 = 440.0;

fn input_id() -> egui::Id {
    egui::Id::new("command-palette-input")
}

fn area_id() -> egui::Id {
    egui::Id::new("command-palette")
}

// ----------------------------------------------------------------------
// Fuzzy matching (pure)

/// Score per matched character.
const SCORE_MATCH: i32 = 10;
/// A match at the start of a word ("Go to |Settings", "|Leon").
const BONUS_WORD: i32 = 20;
/// A match at the very start of the text, on top of the word bonus.
const BONUS_FIRST: i32 = 10;
/// A match right after the previous one ("se|t|t").
const BONUS_RUN: i32 = 15;
/// Cost per character skipped between two matches.
const PENALTY_GAP: i32 = 3;
/// Cost per character skipped before the first match, and its cap.
const PENALTY_LEAD: i32 = 1;
const PENALTY_LEAD_MAX: i32 = 15;

/// A fuzzy match: its score (higher is better) and the matched characters'
/// indices into the text (in `char`s), for highlighting.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Match {
    score: i32,
    hits: Vec<usize>,
}

/// Whether the character at `j` starts a word: the first one, one after a
/// separator, or a capital after a lower-case letter.
fn word_start(text: &[char], j: usize) -> bool {
    if j == 0 {
        return true;
    }
    let (prev, cur) = (text[j - 1], text[j]);
    !prev.is_alphanumeric() || (prev.is_lowercase() && cur.is_uppercase())
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Match `query` (spaces ignored, case-insensitive) against `text` as a
/// subsequence and score the best alignment: every matched character
/// scores, word starts and runs score more, gaps cost. `None` when the
/// query is not a subsequence, or when the best alignment is so scattered
/// that it scores nothing. An empty query matches with score 0.
///
/// The best alignment comes from dynamic programming over (query char,
/// text position) in O(query × text): the gap cost is linear, so the best
/// earlier position is a running maximum.
fn fuzzy(query: &str, text: &str) -> Option<Match> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(fold)
        .collect();
    if q.is_empty() {
        return Some(Match {
            score: 0,
            hits: Vec::new(),
        });
    }
    let original: Vec<char> = text.chars().take(MATCH_CHARS).collect();
    let t: Vec<char> = original.iter().copied().map(fold).collect();
    let (n, m) = (q.len(), t.len());
    if n > m {
        return None;
    }
    const NONE: i32 = i32::MIN / 2;
    let bonus = |j: usize| {
        let mut b = SCORE_MATCH;
        if word_start(&original, j) {
            b += BONUS_WORD;
        }
        if j == 0 {
            b += BONUS_FIRST;
        }
        b
    };
    // score[i][j]: the best alignment of q[..=i] with q[i] matched at t[j];
    // from[i][j]: where q[i - 1] was matched in it.
    let mut score = vec![vec![NONE; m]; n];
    let mut from = vec![vec![0usize; m]; n];
    for j in 0..m {
        if t[j] == q[0] {
            let lead = (j as i32 * PENALTY_LEAD).min(PENALTY_LEAD_MAX);
            score[0][j] = bonus(j) - lead;
        }
    }
    for i in 1..n {
        // The max over k ≤ j − 2 of score[i − 1][k] + GAP·k, and its k.
        let mut best_gap: Option<(i32, usize)> = None;
        for j in i..m {
            if j >= 2 {
                let k = j - 2;
                if score[i - 1][k] > NONE {
                    let v = score[i - 1][k] + PENALTY_GAP * k as i32;
                    if best_gap.is_none_or(|(b, _)| v > b) {
                        best_gap = Some((v, k));
                    }
                }
            }
            if t[j] != q[i] {
                continue;
            }
            let run =
                (score[i - 1][j - 1] > NONE).then(|| (score[i - 1][j - 1] + BONUS_RUN, j - 1));
            let gap = best_gap.map(|(v, k)| (v - PENALTY_GAP * (j as i32 - 1), k));
            let best = match (run, gap) {
                (Some(r), Some(g)) => Some(if r.0 >= g.0 { r } else { g }),
                (r, g) => r.or(g),
            };
            if let Some((s, k)) = best {
                score[i][j] = s + bonus(j);
                from[i][j] = k;
            }
        }
    }
    // The best end; on a tie the earliest.
    let (end, &best) = score[n - 1]
        .iter()
        .enumerate()
        .max_by_key(|&(j, s)| (*s, std::cmp::Reverse(j)))?;
    if best <= 0 {
        return None;
    }
    let mut hits = vec![end; n];
    for i in (1..n).rev() {
        hits[i - 1] = from[i][hits[i]];
    }
    Some(Match { score: best, hits })
}

// ----------------------------------------------------------------------
// Results (pure)

/// A result row's group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Actions,
    Meetings,
    People,
    /// The "Search transcripts for …" fallback (no header).
    Transcripts,
}

impl Section {
    fn title(self) -> Option<&'static str> {
        match self {
            Section::Actions => Some("Actions"),
            Section::Meetings => Some("Meetings"),
            Section::People => Some("People"),
            Section::Transcripts => None,
        }
    }
}

/// What running a row does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    StartRecording,
    StopRecording,
    AddBookmark,
    ShowCaptions,
    /// Open the transcript of the meeting being recorded.
    OpenRecording(i64),
    GoMeetings,
    GoPeople,
    GoSettings,
    /// Switch Appearance to dark (`true`) or light.
    Theme {
        dark: bool,
    },
    SearchTranscripts(String),
    OpenMeeting(i64),
    OpenPerson(i64),
}

impl Command {
    /// Acts on the recording (ignored while the debug hook fakes one).
    fn touches_recording(&self) -> bool {
        matches!(
            self,
            Command::StartRecording
                | Command::StopRecording
                | Command::AddBookmark
                | Command::ShowCaptions
                | Command::OpenRecording(_)
        )
    }
}

/// What a row shows before its title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Glyph {
    Icon(&'static str),
    /// A record-red icon (record / stop, a meeting being recorded).
    RecIcon(&'static str),
    /// A person's initials, tinted by person id like People's avatars.
    Avatar(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    command: Command,
    title: String,
    /// Matched character indices into `title` (highlighted).
    hits: Vec<usize>,
    /// Right-aligned meta: a date · duration, or a hotkey.
    accessory: String,
    glyph: Glyph,
    score: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    section: Section,
    items: Vec<Item>,
}

/// What decides which actions exist right now (plain data from `cx`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ActionState {
    recording: bool,
    /// A live-caption session runs (the top bar's CC button shows then).
    live_captions: bool,
    /// The meeting being recorded, when known.
    recording_meeting: Option<i64>,
    /// Tray Quit is running: nothing may start a recording.
    quitting: bool,
    /// The palette in use is dark (the theme action offers light).
    dark: bool,
    record_hotkey: Option<String>,
    bookmark_hotkey: Option<String>,
}

impl ActionState {
    fn of(cx: &UiCtx) -> ActionState {
        let recording = cx.shell.recording();
        let mut state = ActionState {
            recording,
            live_captions: recording && cx.shell.live_captions_active(),
            recording_meeting: cx
                .shell
                .status
                .as_ref()
                .and_then(|s| s.meeting_id)
                .filter(|_| recording),
            quitting: cx.shell.quitting,
            dark: pal().dark,
            record_hotkey: cx.shell.record_hotkey.clone(),
            bookmark_hotkey: cx.shell.bookmark_hotkey.clone(),
        };
        if debug_fake_recording() {
            state.recording = true;
            state.live_captions = true;
        }
        state
    }
}

/// One action before matching; `keywords` also find it ("theme",
/// "preferences"…) but are not shown.
struct ActionSpec {
    command: Command,
    title: &'static str,
    keywords: &'static str,
    glyph: Glyph,
    accessory: String,
}

/// `"ctrl+alt+r"` → `"Ctrl+Alt+R"`.
fn fmt_hotkey(combo: &str) -> String {
    combo
        .split('+')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// The actions available in `state`, in their empty-query order.
fn actions(state: &ActionState) -> Vec<ActionSpec> {
    let hotkey = |h: &Option<String>| h.as_deref().map(fmt_hotkey).unwrap_or_default();
    let spec = |command, title, keywords, glyph| ActionSpec {
        command,
        title,
        keywords,
        glyph,
        accessory: String::new(),
    };
    let mut out = Vec::new();
    if state.recording {
        out.push(ActionSpec {
            accessory: hotkey(&state.record_hotkey),
            ..spec(
                Command::StopRecording,
                "Stop recording",
                "end finish record",
                Glyph::RecIcon(ph::STOP),
            )
        });
        out.push(ActionSpec {
            accessory: hotkey(&state.bookmark_hotkey),
            ..spec(
                Command::AddBookmark,
                "Add bookmark",
                "mark moment",
                Glyph::Icon(ph::BOOKMARK_SIMPLE),
            )
        });
        if let Some(id) = state.recording_meeting {
            out.push(spec(
                Command::OpenRecording(id),
                "Open the recording's transcript",
                "current live meeting",
                Glyph::Icon(ph::FILE_TEXT),
            ));
        }
        if state.live_captions {
            out.push(spec(
                Command::ShowCaptions,
                "Show live captions",
                "overlay subtitles cc",
                Glyph::Icon(ph::CLOSED_CAPTIONING),
            ));
        }
    } else if !state.quitting {
        out.push(ActionSpec {
            accessory: hotkey(&state.record_hotkey),
            ..spec(
                Command::StartRecording,
                "Start recording",
                "record new meeting capture",
                Glyph::RecIcon(ph::RECORD),
            )
        });
    }
    out.push(spec(
        Command::GoMeetings,
        "Go to Meetings",
        "list home recordings",
        Glyph::Icon(ph::WAVEFORM),
    ));
    out.push(spec(
        Command::GoPeople,
        "Go to People",
        "speakers voices stats",
        Glyph::Icon(ph::USERS),
    ));
    out.push(spec(
        Command::GoSettings,
        "Go to Settings",
        "preferences options configure",
        Glyph::Icon(ph::GEAR_SIX),
    ));
    out.push(if state.dark {
        spec(
            Command::Theme { dark: false },
            "Switch to light theme",
            "appearance mode colour color day",
            Glyph::Icon(ph::SUN),
        )
    } else {
        spec(
            Command::Theme { dark: true },
            "Switch to dark theme",
            "appearance mode colour color night",
            Glyph::Icon(ph::MOON),
        )
    });
    out
}

/// A meeting as the palette lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MeetingEntry {
    id: i64,
    title: String,
    started_at: String,
    duration_ms: Option<i64>,
    recording: bool,
}

impl From<Meeting> for MeetingEntry {
    fn from(m: Meeting) -> Self {
        MeetingEntry {
            id: m.id,
            title: if m.title.trim().is_empty() {
                "Untitled meeting".into()
            } else {
                m.title
            },
            recording: m.status == "recording",
            started_at: m.started_at,
            duration_ms: m.duration_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersonEntry {
    id: i64,
    name: String,
}

/// `"Today, 14:07"`, `"Yesterday, 09:30"`, `"Thu 23 Jul"`, `"23 Jul 2025"`.
fn fmt_when(rfc3339: &str, today: NaiveDate) -> String {
    let Ok(at) = DateTime::parse_from_rfc3339(rfc3339) else {
        return String::new();
    };
    let at = at.with_timezone(&Local);
    let day = at.date_naive();
    if day == today {
        at.format("Today, %H:%M").to_string()
    } else if today.pred_opt() == Some(day) {
        at.format("Yesterday, %H:%M").to_string()
    } else if day.year() == today.year() {
        at.format("%a %-d %b").to_string()
    } else {
        at.format("%-d %b %Y").to_string()
    }
}

/// A meeting row's meta: when, and how long (or that it is recording).
fn meeting_meta(m: &MeetingEntry, today: NaiveDate) -> String {
    if m.recording {
        return "Recording".into();
    }
    let when = fmt_when(&m.started_at, today);
    match m.duration_ms {
        Some(_) if !when.is_empty() => {
            format!("{when} · {}", super::format::fmt_duration(m.duration_ms))
        }
        _ => when,
    }
}

/// The best of matching the title (highlighted) and each keyword (scored a
/// little lower, nothing to highlight).
fn match_action(query: &str, spec: &ActionSpec) -> Option<Match> {
    let title = fuzzy(query, spec.title);
    let keyword = spec
        .keywords
        .split_whitespace()
        .filter_map(|k| fuzzy(query, k))
        .map(|m| Match {
            score: m.score - 10,
            hits: Vec::new(),
        })
        .max_by_key(|m| m.score);
    match (title, keyword) {
        (Some(t), Some(k)) if k.score > t.score => Some(k),
        (Some(t), _) => Some(t),
        (None, k) => k,
    }
}

/// Sort best first (stable: equal scores keep the source order, so newer
/// meetings win) and keep `max`.
fn ranked(mut items: Vec<Item>, max: usize) -> Vec<Item> {
    items.sort_by_key(|item| std::cmp::Reverse(item.score));
    items.truncate(max);
    items
}

/// The rows for `query`, grouped.
///
/// Empty query: every action, then the most recent meetings. Otherwise the
/// matching actions, meetings and people, the groups ordered by their best
/// match, then "Search transcripts for …".
fn build(
    query: &str,
    state: &ActionState,
    meetings: &[MeetingEntry],
    people: &[PersonEntry],
    today: NaiveDate,
) -> Vec<Group> {
    let query = query.trim();
    let action_item = |spec: ActionSpec, m: Match| Item {
        command: spec.command,
        title: spec.title.to_owned(),
        hits: m.hits,
        accessory: spec.accessory,
        glyph: spec.glyph,
        score: m.score,
    };
    let meeting_item = |entry: &MeetingEntry, m: Match| Item {
        command: Command::OpenMeeting(entry.id),
        title: entry.title.clone(),
        hits: m.hits,
        accessory: meeting_meta(entry, today),
        glyph: if entry.recording {
            Glyph::RecIcon(ph::RECORD)
        } else {
            Glyph::Icon(ph::WAVEFORM)
        },
        score: m.score,
    };
    let unranked = || Match {
        score: 0,
        hits: Vec::new(),
    };

    if query.is_empty() {
        let mut groups = vec![Group {
            section: Section::Actions,
            items: actions(state)
                .into_iter()
                .map(|spec| action_item(spec, unranked()))
                .collect(),
        }];
        let recent: Vec<Item> = meetings
            .iter()
            .take(RECENT_MEETINGS)
            .map(|m| meeting_item(m, unranked()))
            .collect();
        if !recent.is_empty() {
            groups.push(Group {
                section: Section::Meetings,
                items: recent,
            });
        }
        return groups;
    }

    let action_items = ranked(
        actions(state)
            .into_iter()
            .filter_map(|spec| match_action(query, &spec).map(|m| action_item(spec, m)))
            .collect(),
        usize::MAX,
    );
    let meeting_items = ranked(
        meetings
            .iter()
            .filter_map(|e| fuzzy(query, &e.title).map(|m| meeting_item(e, m)))
            .collect(),
        MAX_MEETINGS,
    );
    let people_items = ranked(
        people
            .iter()
            .filter_map(|p| {
                fuzzy(query, &p.name).map(|m| Item {
                    command: Command::OpenPerson(p.id),
                    title: p.name.clone(),
                    hits: m.hits,
                    accessory: String::new(),
                    glyph: Glyph::Avatar(p.id),
                    score: m.score,
                })
            })
            .collect(),
        MAX_PEOPLE,
    );
    let mut groups: Vec<Group> = [
        (Section::Actions, action_items),
        (Section::Meetings, meeting_items),
        (Section::People, people_items),
    ]
    .into_iter()
    .filter(|(_, items)| !items.is_empty())
    .map(|(section, items)| Group { section, items })
    .collect();
    // Stable: on a tie the fixed order (Actions, Meetings, People) holds.
    groups.sort_by_key(|g| std::cmp::Reverse(g.items[0].score));
    groups.push(Group {
        section: Section::Transcripts,
        items: vec![Item {
            command: Command::SearchTranscripts(query.to_owned()),
            title: format!("Search transcripts for \u{201c}{query}\u{201d}"),
            hits: Vec::new(),
            accessory: String::new(),
            glyph: Glyph::Icon(ph::MAGNIFYING_GLASS),
            score: 0,
        }],
    });
    groups
}

/// All rows in display order.
fn flat(groups: &[Group]) -> impl Iterator<Item = &Item> {
    groups.iter().flat_map(|g| g.items.iter())
}

/// Move the selection by `delta` rows, wrapping at both ends.
fn step(selected: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as i64 + i64::from(delta)).rem_euclid(len as i64) as usize
}

/// Move by a page (no wrap: PageDown near the end stops on the last row).
fn page(selected: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as i64 + i64::from(delta)).clamp(0, len as i64 - 1) as usize
}

/// The palette's keys, taken from the pass's input before the field sees
/// them (it would move its cursor on ↑/↓ and drop focus on Enter / Esc).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Keys {
    up: bool,
    down: bool,
    page_up: bool,
    page_down: bool,
    enter: bool,
    escape: bool,
}

impl Keys {
    fn take(i: &mut egui::InputState) -> Keys {
        Keys {
            up: i.consume_key(Modifiers::NONE, Key::ArrowUp),
            down: i.consume_key(Modifiers::NONE, Key::ArrowDown),
            page_up: i.consume_key(Modifiers::NONE, Key::PageUp),
            page_down: i.consume_key(Modifiers::NONE, Key::PageDown),
            enter: i.consume_key(Modifiers::NONE, Key::Enter),
            escape: i.consume_key(Modifiers::NONE, Key::Escape),
        }
    }
}

/// Debug builds only: `WITNESS_DEBUG_FAKE_RECORDING=1|warn` (the top bar's
/// snapshot hook) shows the recording actions here too. Like the top bar's
/// buttons, they do nothing while faked.
#[cfg(debug_assertions)]
fn debug_fake_recording() -> bool {
    static FAKE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FAKE.get_or_init(|| {
        matches!(
            std::env::var("WITNESS_DEBUG_FAKE_RECORDING").as_deref(),
            Ok("1" | "warn")
        )
    })
}

#[cfg(not(debug_assertions))]
fn debug_fake_recording() -> bool {
    false
}

// ----------------------------------------------------------------------
// The palette

/// The meeting / people loads started by an open.
#[derive(Default)]
struct Load {
    meetings: Option<Task<CoreResult<Vec<Meeting>>>>,
    people: Option<Task<CoreResult<Vec<PersonInfo>>>>,
}

/// What the cached `results` were built from.
#[derive(Debug, Clone, PartialEq)]
struct ResultsKey {
    query: String,
    generation: u64,
    state: ActionState,
    today: NaiveDate,
}

/// What a drawn pass asks for.
#[derive(Debug, Default)]
struct DrawOutput {
    /// A row was clicked (its index).
    clicked: Option<usize>,
    /// The backdrop was clicked.
    dismiss: bool,
}

#[derive(Default)]
pub struct CommandPalette {
    open: bool,
    query: String,
    /// The selected row (index into the flattened results).
    selected: usize,
    /// Scroll the selected row into view on the next draw (keyboard moves).
    reveal: bool,
    /// Start the loads on the next `show` (set by opening).
    needs_load: bool,
    load: Load,
    /// Kept between opens; every open refreshes them.
    meetings: Vec<MeetingEntry>,
    people: Vec<PersonEntry>,
    /// Bumped whenever `meetings` / `people` change.
    generation: u64,
    results: Vec<Group>,
    results_key: Option<ResultsKey>,
    /// `WITNESS_DEBUG_PALETTE_SELECT`: the row to select once results exist.
    #[cfg(debug_assertions)]
    debug_select: Option<usize>,
}

impl CommandPalette {
    pub fn new() -> CommandPalette {
        #[allow(unused_mut)]
        let mut palette = CommandPalette::default();
        #[cfg(debug_assertions)]
        palette.debug_open();
        palette
    }

    /// Debug builds only, for passive snapshots (the shell builds the
    /// palette once, at startup): `WITNESS_DEBUG_PALETTE=<query>` opens it
    /// with that query from the first frame — `-` for no query, since
    /// PowerShell cannot set an empty variable — and
    /// `WITNESS_DEBUG_PALETTE_SELECT=<row>` moves the selection there.
    #[cfg(debug_assertions)]
    fn debug_open(&mut self) {
        let Ok(query) = std::env::var("WITNESS_DEBUG_PALETTE") else {
            return;
        };
        self.open_fresh();
        if query != "-" {
            self.query = query;
        }
        if let Some(row) = std::env::var("WITNESS_DEBUG_PALETTE_SELECT")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            self.debug_select = Some(row);
        }
    }

    /// Open (Ctrl+K pressed while closed) or close it.
    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open_fresh();
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open with an empty query and the first row selected; reload the data.
    fn open_fresh(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.reveal = true;
        self.needs_load = true;
        self.results_key = None;
    }

    fn close(&mut self) {
        self.open = false;
        // Late replies are not wanted; the next open asks again.
        self.load = Load::default();
    }

    /// Draw it when open, above everything but the confirm modal.
    pub fn show(&mut self, ctx: &egui::Context, cx: &mut UiCtx) {
        if !self.open {
            return;
        }
        self.start_load(cx);
        self.poll(ctx, cx);
        let state = ActionState::of(cx);
        self.refresh(&state, Local::now().date_naive());

        let keys = ctx.input_mut(Keys::take);
        let mut chosen = self.handle_keys(keys);
        if self.open && chosen.is_none() {
            let before = self.query.clone();
            let out = self.draw(ctx, self.load_in_flight());
            if self.query != before {
                // Re-rank next pass, from the top.
                self.selected = 0;
                self.reveal = true;
                cx.fonts.note_texts(ctx, [self.query.as_str()]);
                ctx.request_repaint();
            }
            if let Some(index) = out.clicked {
                chosen = flat(&self.results)
                    .nth(index)
                    .map(|item| item.command.clone());
            }
            if out.dismiss {
                self.close();
            }
        }
        if let Some(command) = chosen {
            self.close();
            if !(command.touches_recording() && debug_fake_recording()) {
                run(command, cx);
            }
        }
        if !self.open {
            ctx.request_repaint();
        }
    }

    /// Apply the palette's keys: Esc closes, ↑/↓ move (wrapping), PageUp /
    /// PageDown jump, Enter returns the selected row's command.
    fn handle_keys(&mut self, keys: Keys) -> Option<Command> {
        if keys.escape {
            self.close();
            return None;
        }
        let len = flat(&self.results).count();
        let before = self.selected;
        if keys.up {
            self.selected = step(self.selected, len, -1);
        }
        if keys.down {
            self.selected = step(self.selected, len, 1);
        }
        let jump = VISIBLE_ROWS as i32 - 1;
        if keys.page_up {
            self.selected = page(self.selected, len, -jump);
        }
        if keys.page_down {
            self.selected = page(self.selected, len, jump);
        }
        if self.selected != before {
            self.reveal = true;
        }
        if keys.enter {
            return flat(&self.results)
                .nth(self.selected)
                .map(|item| item.command.clone());
        }
        None
    }

    fn load_in_flight(&self) -> bool {
        self.load.meetings.is_some() || self.load.people.is_some()
    }

    /// Start the meeting and people loads an open asked for.
    fn start_load(&mut self, cx: &UiCtx) {
        if !std::mem::take(&mut self.needs_load) {
            return;
        }
        self.load = Load {
            meetings: Some(
                cx.tasks
                    .spawn("palette-meetings", |c| c.list_meetings(0, MEETINGS_LOADED)),
            ),
            people: Some(cx.tasks.spawn("palette-people", |c| c.list_people())),
        };
    }

    /// Take finished loads. A failure keeps the previous lists (the views
    /// report load errors; the palette stays quiet).
    fn poll(&mut self, ctx: &egui::Context, cx: &mut UiCtx) {
        if let Some(result) = self.load.meetings.as_mut().and_then(|t| t.poll_flat()) {
            self.load.meetings = None;
            match result {
                Ok(rows) => {
                    cx.fonts
                        .note_texts(ctx, rows.iter().map(|m| m.title.as_str()));
                    self.meetings = rows.into_iter().map(MeetingEntry::from).collect();
                    self.generation += 1;
                }
                Err(error) => log::warn!("command palette: could not load meetings: {error}"),
            }
        }
        if let Some(result) = self.load.people.as_mut().and_then(|t| t.poll_flat()) {
            self.load.people = None;
            match result {
                Ok(rows) => {
                    cx.fonts
                        .note_texts(ctx, rows.iter().map(|p| p.name.as_str()));
                    self.people = rows
                        .into_iter()
                        .map(|p| PersonEntry {
                            id: p.id,
                            name: p.name,
                        })
                        .collect();
                    self.generation += 1;
                }
                Err(error) => log::warn!("command palette: could not load people: {error}"),
            }
        }
    }

    /// Rebuild the results when the query, the data or the actions changed.
    fn refresh(&mut self, state: &ActionState, today: NaiveDate) {
        let key = ResultsKey {
            query: self.query.clone(),
            generation: self.generation,
            state: state.clone(),
            today,
        };
        if self.results_key.as_ref() == Some(&key) {
            return;
        }
        self.results = build(&self.query, state, &self.meetings, &self.people, today);
        self.results_key = Some(key);
        let len = flat(&self.results).count();
        #[cfg(debug_assertions)]
        if let Some(row) = self.debug_select.filter(|&row| row < len) {
            self.selected = row;
            self.reveal = true;
            self.debug_select = None;
        }
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    // ------------------------------------------------------------------
    // Drawing

    /// The overlay: a dimmed backdrop and the raised surface near the top
    /// centre, `min(600, window − 32)` wide: the search row, the grouped
    /// results (scrolling past ~8 rows) and the key hints.
    fn draw(&mut self, ctx: &egui::Context, loading: bool) -> DrawOutput {
        let p = pal();
        let screen = ctx.content_rect();
        let width = (screen.width() - 2.0 * S4).clamp(0.0, MAX_WIDTH);
        let top = (screen.height() * 0.12).clamp(S4, 96.0);
        let area =
            egui::Modal::default_area(area_id()).anchor(Align2::CENTER_TOP, egui::vec2(0.0, top));
        let frame = Frame::new()
            .fill(p.bg_raised)
            .stroke(Stroke::new(
                1.0,
                if p.dark { p.border_strong } else { p.border },
            ))
            .corner_radius(CornerRadius::same(theme::R_LG))
            .shadow(ctx.global_style().visuals.window_shadow);
        // Room for the list between the search row and the footer, with
        // the window's bottom 16 pt clear.
        let list_room = screen.bottom() - top - INPUT_H - FOOTER_H - 4.0 - S4 - 2.0 * LIST_PAD;
        let list_max = (VISIBLE_ROWS * ROW_H + HEADER_H).min(list_room).max(ROW_H);

        let mut out = DrawOutput::default();
        let modal = egui::Modal::new(area_id())
            .area(area)
            .frame(frame)
            .backdrop_color(p.backdrop)
            .show(ctx, |ui| {
                let inner = (width - 2.0).max(0.0); // inside the 1 pt stroke
                ui.set_width(inner);
                ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                self.draw_input(ui, inner, loading);
                divider(ui);
                out.clicked = self.draw_list(ui, inner, list_max);
                divider(ui);
                draw_footer(ui, inner);
            });
        out.dismiss = modal.backdrop_response.clicked();
        out
    }

    /// The search row: a leading magnifier, the field, a spinner while the
    /// lists load. The field keeps the keyboard focus while open.
    fn draw_input(&mut self, ui: &mut egui::Ui, width: f32, loading: bool) {
        let p = pal();
        let (row, _) = ui.allocate_exact_size(egui::vec2(width, INPUT_H), Sense::hover());
        icons::paint_rotated_glyph(
            ui.painter(),
            egui::pos2(row.left() + GLYPH_X, row.center().y),
            ph::MAGNIFYING_GLASS,
            18.0,
            p.text_muted,
            0.0,
        );
        let right_pad = if loading { S4 + 16.0 + S2 } else { S4 };
        let field = Rect::from_min_max(
            egui::pos2(row.left() + TEXT_X, row.top()),
            egui::pos2(row.right() - right_pad, row.bottom()),
        );
        let hint = if width < NARROW_SURFACE {
            "Search or run a command…"
        } else {
            "Search meetings, people and commands…"
        };
        let edit = egui::TextEdit::singleline(&mut self.query)
            .id(input_id())
            .hint_text(RichText::new(hint).color(p.text_faint))
            .font(theme::regular(16.0))
            .text_color(p.text)
            .frame(Frame::new())
            .margin(Margin::ZERO)
            .vertical_align(Align::Center)
            .desired_width(f32::INFINITY)
            .return_key(None::<egui::KeyboardShortcut>);
        let response = ui.put(field, edit);
        if !response.has_focus() {
            response.request_focus();
        }
        widgets::text_edit_menu(&response);
        ui.ctx().accesskit_node_builder(input_id(), |node| {
            node.set_label("Command palette: search meetings, people and commands");
        });
        if loading {
            let spinner = Rect::from_center_size(
                egui::pos2(row.right() - S4 - 8.0, row.center().y),
                egui::vec2(16.0, 16.0),
            );
            icons::paint_spinner(ui, spinner, 15.0, p.text_faint);
        }
    }

    /// The grouped rows in a scroller; returns the clicked row's index.
    fn draw_list(&mut self, ui: &mut egui::Ui, width: f32, max_height: f32) -> Option<usize> {
        let moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let reveal = std::mem::take(&mut self.reveal);
        let selected = self.selected;
        let mut clicked = None;
        let mut hovered_row = None;
        ui.add_space(LIST_PAD);
        egui::ScrollArea::vertical()
            .id_salt("command-palette-results")
            .max_height(max_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                let mut index = 0;
                for group in &self.results {
                    let header = group.section.title().map(|t| section_header(ui, t));
                    if header.is_none() && index > 0 {
                        // The header-less transcript search: set apart
                        // from the group above by a hairline.
                        separator(ui);
                    }
                    for (i, item) in group.items.iter().enumerate() {
                        let is_selected = index == selected;
                        let response = draw_row(ui, item, is_selected);
                        if reveal && is_selected {
                            // A group's first row brings its header along.
                            let rect = match header {
                                Some(h) if i == 0 => response.rect.union(h),
                                _ => response.rect,
                            };
                            ui.scroll_to_rect(rect, None);
                        }
                        if response.hovered() && moved {
                            hovered_row = Some(index);
                        }
                        if response.clicked() {
                            clicked = Some(index);
                        }
                        index += 1;
                    }
                }
            });
        ui.add_space(LIST_PAD);
        // Moving the pointer over a row selects it; a still pointer under
        // a keyboard-scrolled list does not steal the selection.
        if let Some(index) = hovered_row {
            self.selected = index;
        }
        clicked
    }
}

/// An inset hairline between groups, 9 pt tall.
fn separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(
        rect.x_range().shrink(LIST_PAD + S3),
        rect.center().y,
        Stroke::new(1.0, pal().border),
    );
}

/// A full-width hairline.
fn divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::ZERO, pal().border);
}

/// A group header, 12 pt semibold faint, lined up with the rows' glyphs.
/// Returns its rect.
fn section_header(ui: &mut egui::Ui, title: &str) -> Rect {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), HEADER_H), Sense::hover());
    ui.painter().text(
        egui::pos2(
            rect.left() + GLYPH_X - GLYPH_BOX / 2.0,
            rect.bottom() - S2 + 1.0,
        ),
        Align2::LEFT_BOTTOM,
        title,
        theme::semibold(12.0),
        pal().text_faint,
    );
    rect
}

/// The title on one line with the matched characters picked out (semibold,
/// `hit` colour), elided with `…` at `width`.
fn title_job(item: &Item, width: f32, color: Color32, hit: Color32) -> egui::text::LayoutJob {
    fn push(job: &mut egui::text::LayoutJob, run: &mut String, font: FontId, color: Color32) {
        if !run.is_empty() {
            job.append(run, 0.0, egui::TextFormat::simple(font, color));
            run.clear();
        }
    }
    let format = |is_hit: bool| {
        if is_hit {
            (theme::semibold(14.0), hit)
        } else {
            (theme::regular(14.0), color)
        }
    };
    let mut job = egui::text::LayoutJob::default();
    let mut hits = item.hits.iter().copied().peekable();
    let mut run = String::new();
    let mut run_hit = false;
    for (i, c) in item.title.chars().enumerate() {
        let is_hit = hits.peek() == Some(&i);
        if is_hit {
            hits.next();
        }
        if is_hit != run_hit {
            let (font, color) = format(run_hit);
            push(&mut job, &mut run, font, color);
            run_hit = is_hit;
        }
        run.push(c);
    }
    let (font, color) = format(run_hit);
    push(&mut job, &mut run, font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    job
}

/// One result row: glyph, highlighted title (elided), right-aligned meta.
/// The selected row takes the accent tint. Rows sense clicks but never take
/// the focus: the field keeps it.
fn draw_row(ui: &mut egui::Ui, item: &Item, selected: bool) -> egui::Response {
    let p = pal();
    let (full, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::CLICK);
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::Button, true, &item.title);
        info.selected = Some(selected);
        info
    });
    if !ui.is_rect_visible(full) {
        return response;
    }
    let rect = full.shrink2(egui::vec2(LIST_PAD, 0.0));
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, CornerRadius::same(theme::R_MD), p.accent_soft);
    } else if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(theme::R_MD), p.bg_hover);
    }

    let glyph_center = egui::pos2(full.left() + GLYPH_X, rect.center().y);
    match item.glyph {
        Glyph::Icon(glyph) => {
            let color = if selected { p.text } else { p.text_muted };
            icons::paint_rotated_glyph(painter, glyph_center, glyph, 17.0, color, 0.0);
        }
        Glyph::RecIcon(glyph) => {
            icons::paint_rotated_glyph(painter, glyph_center, glyph, 17.0, p.rec, 0.0);
        }
        Glyph::Avatar(id) => {
            // People's avatar colours: one of the speaker colours by id.
            let tint = theme::chip_color(&format!("S{}", id.rem_euclid(8) + 1));
            painter.circle_filled(
                glyph_center,
                GLYPH_BOX / 2.0,
                theme::alpha(tint, if p.dark { 0.24 } else { 0.16 }),
            );
            painter.text(
                glyph_center,
                Align2::CENTER_CENTER,
                widgets::initials(&item.title),
                theme::semibold(9.5),
                if p.dark {
                    theme::lighten(tint, 0.45)
                } else {
                    theme::darken(tint, 0.15)
                },
            );
        }
    }

    let text_left = full.left() + TEXT_X;
    let mut text_right = rect.right() - S3;
    if !item.accessory.is_empty() {
        // The meta never takes more than ~45 % of the row.
        let meta_color = if selected { p.text_muted } else { p.text_faint };
        let mut job = egui::text::LayoutJob::simple_singleline(
            item.accessory.clone(),
            theme::regular(12.0),
            meta_color,
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() * 0.45).max(40.0));
        let meta = painter.layout_job(job);
        let pos = egui::pos2(
            text_right - meta.size().x,
            rect.center().y - meta.size().y / 2.0,
        );
        text_right = pos.x - S3;
        painter.galley(pos, meta, meta_color);
    }

    let hit = if p.dark {
        theme::lighten(p.accent, 0.3)
    } else {
        p.accent
    };
    let galley = painter.layout_job(title_job(item, text_right - text_left, p.text, hit));
    let elided = galley.elided;
    painter.galley(
        egui::pos2(text_left, rect.center().y - galley.size().y / 2.0),
        galley,
        p.text,
    );
    if elided {
        response.on_hover_text(&item.title)
    } else {
        response
    }
}

/// The key hints, as keycaps: `↑↓ to move · Enter to open · Esc to close`.
/// Hints that do not fit are left out.
fn draw_footer(ui: &mut egui::Ui, width: f32) {
    let p = pal();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, FOOTER_H), Sense::hover());
    let painter = ui.painter();
    let y = rect.center().y;
    let mut x = rect.left() + S4;
    for (i, (key, label)) in [("↑↓", "to move"), ("Enter", "to open"), ("Esc", "to close")]
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            x += S4;
        }
        let cap = painter.layout_no_wrap(key.to_owned(), theme::regular(11.5), p.text_muted);
        let text = painter.layout_no_wrap(label.to_owned(), theme::regular(12.0), p.text_faint);
        let cap_w = cap.size().x + 2.0 * 6.0;
        if x + cap_w + S1 + 2.0 + text.size().x > rect.right() - S4 {
            break;
        }
        let cap_rect = Rect::from_min_size(egui::pos2(x, y - 10.0), egui::vec2(cap_w, 20.0));
        painter.rect(
            cap_rect,
            CornerRadius::same(4),
            p.bg_hover,
            Stroke::new(1.0, p.border),
            egui::StrokeKind::Inside,
        );
        painter.galley(cap_rect.center() - cap.size() / 2.0, cap, p.text_muted);
        x = cap_rect.right() + S1 + 2.0;
        let text_w = text.size().x;
        painter.galley(egui::pos2(x, y - text.size().y / 2.0), text, p.text_faint);
        x += text_w;
    }
}

// ----------------------------------------------------------------------
// Running a row

/// Do what `command` says, through the paths the rest of the shell uses
/// (the top bar's recording control, `nav`, prefs).
fn run(command: Command, cx: &mut UiCtx) {
    match command {
        Command::StartRecording => cx.submit_rec(RecCmd::Start(Trigger::Manual)),
        Command::StopRecording => cx.submit_rec(RecCmd::Stop { by_user: true }),
        // `app.rs` toasts "Bookmark added" or the error on the reply.
        Command::AddBookmark => cx.submit_rec(RecCmd::BookmarkNow),
        // As the top bar's CC button: the check runs as an op, and
        // `OpDone::CaptionsCheck` shows the overlay or toasts the error.
        Command::ShowCaptions => {
            cx.tasks.spawn_op_or(
                "captions-check",
                |c| OpDone::CaptionsCheck(c.check_captions_overlay()),
                |error| OpDone::CaptionsCheck(Err(error)),
            );
        }
        Command::OpenRecording(id) | Command::OpenMeeting(id) => open_transcript(cx, id),
        Command::GoMeetings => cx.go_meetings_clear_search(),
        // TODO(v2): expand that person once PeopleView can be asked to.
        Command::GoPeople | Command::OpenPerson(_) => cx.nav.go(View::People),
        Command::GoSettings => cx.nav.go(View::Settings),
        Command::Theme { dark } => {
            cx.prefs.appearance = if dark {
                Appearance::Dark
            } else {
                Appearance::Light
            };
        }
        Command::SearchTranscripts(query) => {
            // The shell's search box shows the query, as if typed there.
            cx.memory.search_box.text = query.clone();
            cx.memory.search_box.debounce_at = None;
            cx.nav.go(View::Search { query });
        }
    }
}

/// Open a transcript; Back returns to the current view. Transcripts never
/// nest: from a transcript, Back goes where that one came from (as the
/// recording pill does).
fn open_transcript(cx: &mut UiCtx, meeting_id: i64) {
    let from = match cx.view {
        View::Transcript { from, .. } => from.clone(),
        other => Some(Box::new(other.clone())),
    };
    cx.nav.go(View::Transcript {
        meeting_id,
        segment_id: None,
        highlight: None,
        from,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 24).unwrap()
    }

    fn meeting(id: i64, title: &str) -> MeetingEntry {
        MeetingEntry {
            id,
            title: title.into(),
            started_at: "2026-07-24T14:07:03+01:00".into(),
            duration_ms: Some(352_100),
            recording: false,
        }
    }

    fn person(id: i64, name: &str) -> PersonEntry {
        PersonEntry {
            id,
            name: name.into(),
        }
    }

    fn sample() -> (Vec<MeetingEntry>, Vec<PersonEntry>) {
        (
            vec![
                meeting(18, "Calendar | 2nd/3rd Line"),
                meeting(12, "Chat | Leon Bradley"),
                meeting(10, "3rd Line Catchup"),
                meeting(8, "Chat | Leon Bradley"),
                meeting(5, "Blob Storage Handover"),
                meeting(4, "Server migration"),
                meeting(3, "Weekly sync"),
            ],
            vec![person(7, "Leon Bradley"), person(6, "Paul Gordon")],
        )
    }

    fn sections(groups: &[Group]) -> Vec<Section> {
        groups.iter().map(|g| g.section).collect()
    }

    fn score(query: &str, text: &str) -> i32 {
        fuzzy(query, text).map_or(i32::MIN, |m| m.score)
    }

    #[test]
    fn fuzzy_is_a_case_insensitive_subsequence() {
        assert_eq!(
            fuzzy("SETT", "Go to Settings").unwrap().hits,
            vec![6, 7, 8, 9]
        );
        assert!(fuzzy("xyz", "Go to Settings").is_none());
        assert!(fuzzy("settingsx", "Settings").is_none());
        // Spaces in the query are ignored.
        assert_eq!(
            fuzzy("leon b", "Leon Bradley").unwrap().hits,
            vec![0, 1, 2, 3, 5]
        );
        // An empty query matches everything and highlights nothing.
        assert_eq!(
            fuzzy("  ", "x"),
            Some(Match {
                score: 0,
                hits: vec![]
            })
        );
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        // A prefix beats the same word later on…
        assert!(score("leon", "Leon Bradley") > score("leon", "Chat | Leon Bradley"));
        // …and a word start beats the letters inside a word.
        assert!(score("line", "3rd Line") > score("line", "Pipeline"));
        // Initials find words.
        assert_eq!(fuzzy("gts", "Go to Settings").unwrap().hits, vec![0, 3, 6]);
        // The alignment takes the word start over an earlier inner letter.
        assert_eq!(fuzzy("s", "Last Stand").unwrap().hits, vec![5]);
        // A contiguous run beats a scattered one.
        assert!(score("sync", "Weekly sync") > score("sync", "Some young cats"));
        // Hopelessly scattered matches are dropped.
        let scattered = format!("a{0}a{0}a{0}a", ".".repeat(25));
        assert!(fuzzy("aaaa", &scattered).is_none());
    }

    #[test]
    fn empty_query_lists_actions_then_recent_meetings() {
        let (meetings, people) = sample();
        let groups = build("", &ActionState::default(), &meetings, &people, today());
        assert_eq!(sections(&groups), vec![Section::Actions, Section::Meetings]);
        assert_eq!(groups[1].items.len(), RECENT_MEETINGS);
        assert_eq!(groups[1].items[0].command, Command::OpenMeeting(18));
        assert!(flat(&groups).all(|i| i.hits.is_empty()));
        // No meetings yet: actions only.
        let groups = build("  ", &ActionState::default(), &[], &people, today());
        assert_eq!(sections(&groups), vec![Section::Actions]);
    }

    #[test]
    fn a_query_ranks_groups_by_their_best_match() {
        let (meetings, people) = sample();
        let groups = build("leon", &ActionState::default(), &meetings, &people, today());
        // The person is a prefix match: People leads.
        assert_eq!(
            sections(&groups),
            vec![Section::People, Section::Meetings, Section::Transcripts]
        );
        assert_eq!(groups[0].items[0].command, Command::OpenPerson(7));
        // Equal scores keep the newer meeting first.
        let meetings: Vec<&Command> = groups[1].items.iter().map(|i| &i.command).collect();
        assert_eq!(
            meetings,
            vec![&Command::OpenMeeting(12), &Command::OpenMeeting(8)]
        );
        // The transcript search comes last, with the trimmed query.
        let last = flat(&groups).last().unwrap();
        assert_eq!(last.command, Command::SearchTranscripts("leon".into()));

        let (meetings, people) = sample();
        let groups = build(
            " sett ",
            &ActionState::default(),
            &meetings,
            &people,
            today(),
        );
        assert_eq!(groups[0].section, Section::Actions);
        assert_eq!(groups[0].items[0].command, Command::GoSettings);
    }

    #[test]
    fn keywords_find_actions_without_highlights() {
        let (meetings, people) = sample();
        let state = ActionState::default();
        let first = |q| build(q, &state, &meetings, &people, today())[0].items[0].clone();
        assert_eq!(first("theme").command, Command::Theme { dark: true });
        let settings = first("preferences");
        assert_eq!(settings.command, Command::GoSettings);
        assert!(
            settings.hits.is_empty(),
            "keyword matches highlight nothing"
        );
    }

    #[test]
    fn only_the_transcript_search_is_left_when_nothing_matches() {
        let (meetings, people) = sample();
        let groups = build("qqzz", &ActionState::default(), &meetings, &people, today());
        assert_eq!(sections(&groups), vec![Section::Transcripts]);
        assert_eq!(
            groups[0].items[0].title,
            "Search transcripts for \u{201c}qqzz\u{201d}"
        );
    }

    #[test]
    fn actions_follow_the_recording_state() {
        let has = |state: &ActionState, c: &Command| actions(state).iter().any(|a| &a.command == c);
        let while_recording = [
            Command::StopRecording,
            Command::AddBookmark,
            Command::ShowCaptions,
            Command::OpenRecording(3),
        ];
        let idle = ActionState::default();
        assert!(has(&idle, &Command::StartRecording));
        for c in &while_recording {
            assert!(!has(&idle, c), "{c:?} while idle");
        }
        // Dark offers light, and the other way round.
        let dark = ActionState {
            dark: true,
            ..idle.clone()
        };
        assert!(has(&dark, &Command::Theme { dark: false }));
        assert!(has(&idle, &Command::Theme { dark: true }));

        let recording = ActionState {
            recording: true,
            live_captions: true,
            recording_meeting: Some(3),
            record_hotkey: Some("ctrl+alt+r".into()),
            ..ActionState::default()
        };
        assert!(!has(&recording, &Command::StartRecording));
        for c in &while_recording {
            assert!(has(&recording, c), "{c:?} while recording");
        }
        let stop = &actions(&recording)[0];
        assert_eq!(stop.command, Command::StopRecording);
        assert_eq!(stop.accessory, "Ctrl+Alt+R");
        // No caption session: no captions action.
        let no_captions = ActionState {
            live_captions: false,
            ..recording.clone()
        };
        assert!(!has(&no_captions, &Command::ShowCaptions));
        // Quitting: nothing starts a recording.
        let quitting = ActionState {
            quitting: true,
            ..idle
        };
        assert!(!has(&quitting, &Command::StartRecording));
        assert!(has(&quitting, &Command::GoSettings));
    }

    #[test]
    fn selection_wraps_and_pages_clamp() {
        assert_eq!(step(0, 5, -1), 4);
        assert_eq!(step(4, 5, 1), 0);
        assert_eq!(step(2, 5, 1), 3);
        assert_eq!(step(0, 0, 1), 0);
        assert_eq!(page(1, 5, 7), 4);
        assert_eq!(page(3, 5, -7), 0);
        assert_eq!(page(0, 0, 7), 0);
    }

    fn palette_with(query: &str) -> CommandPalette {
        let (meetings, people) = sample();
        let mut palette = CommandPalette::default();
        palette.open_fresh();
        palette.query = query.into();
        palette.meetings = meetings;
        palette.people = people;
        palette.refresh(&ActionState::default(), today());
        palette
    }

    #[test]
    fn keys_move_wrap_and_run() {
        let mut palette = palette_with("leon");
        let len = flat(&palette.results).count();
        let key = |f: fn(&mut Keys)| {
            let mut keys = Keys::default();
            f(&mut keys);
            keys
        };
        let (up, down, enter) = (
            key(|k| k.up = true),
            key(|k| k.down = true),
            key(|k| k.enter = true),
        );
        assert_eq!(palette.handle_keys(enter), Some(Command::OpenPerson(7)));
        assert_eq!(palette.handle_keys(up), None);
        assert_eq!(palette.selected, len - 1, "↑ on the first row wraps");
        assert_eq!(
            palette.handle_keys(enter),
            Some(Command::SearchTranscripts("leon".into()))
        );
        palette.handle_keys(down);
        assert_eq!(palette.selected, 0, "↓ on the last row wraps");
        palette.handle_keys(down);
        assert_eq!(palette.handle_keys(enter), Some(Command::OpenMeeting(12)));
        assert!(palette.is_open());
        assert_eq!(palette.handle_keys(key(|k| k.escape = true)), None);
        assert!(!palette.is_open());
    }

    #[test]
    fn toggle_reopens_fresh() {
        let mut palette = palette_with("leon");
        palette.selected = 2;
        palette.needs_load = false;
        palette.toggle();
        assert!(!palette.is_open());
        palette.toggle();
        assert!(palette.is_open());
        assert!(palette.query.is_empty());
        assert_eq!(palette.selected, 0);
        assert!(palette.needs_load, "every open reloads");
    }

    #[test]
    fn hotkeys_read_like_shortcuts() {
        assert_eq!(fmt_hotkey("ctrl+alt+r"), "Ctrl+Alt+R");
        assert_eq!(fmt_hotkey("ctrl+alt+shift+r"), "Ctrl+Alt+Shift+R");
    }

    #[test]
    fn meeting_meta_reads_naturally() {
        let now = Local::now();
        let mut m = meeting(1, "x");
        m.started_at = now.to_rfc3339();
        let meta = meeting_meta(&m, now.date_naive());
        assert!(
            meta.starts_with("Today, ") && meta.ends_with(" · 5 min"),
            "{meta}"
        );
        m.recording = true;
        assert_eq!(meeting_meta(&m, now.date_naive()), "Recording");
    }

    #[test]
    fn surface_fits_every_width() {
        for width in [380.0, 640.0, 1000.0] {
            let ctx = theme::test_context();
            let mut palette = palette_with("");
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 760.0),
                    )),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| {
                    palette.draw(ui.ctx(), false);
                });
                output.textures_delta.clear(); // no renderer to upload the font atlas
            }
            let card = ctx
                .memory(|m| m.area_rect(area_id()))
                .expect("palette area");
            assert!(card.left() >= S4 - 0.5, "{width}: {card:?}");
            assert!(card.right() <= width - S4 + 0.5, "{width}: {card:?}");
            assert!(card.width() <= MAX_WIDTH + 0.5, "{width}: {card:?}");
            assert!(
                card.top() > 0.0 && card.bottom() < 760.0,
                "{width}: {card:?}"
            );
            assert!(
                ctx.memory(|m| m.has_focus(input_id())),
                "the field takes the focus"
            );
        }
    }
}
