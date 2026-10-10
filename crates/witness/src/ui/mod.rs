//! The native UI: navigation (`View`, replacing app.tsx's `View` union),
//! the per-pass context handed to views (`UiCtx`), app-owned view memory,
//! and the shared kit (theme, fonts, icons, widgets, formatting, confirm
//! modal, toasts). Views live in `views/`; the top bar and the captions
//! overlay have their own modules.

pub mod captions;
pub mod confirm;
pub mod fonts;
pub mod format;
pub mod icons;
pub mod onboarding;
pub mod palette;
pub mod theme;
pub mod toasts;
pub mod topbar;
pub mod views;
pub mod widgets;

use crate::bridge::{OpDone, RecOrigin, Tasks};
use crate::model::ShellModel;
use crate::platform::window::{FlyoutState, MainWindow};
use crate::platform::OwnerHwnd;
use crate::settings_store::SettingsStore;
use crate::ui_state::UiPrefs;
use confirm::Confirm;
use eframe::egui;
use fonts::FontLoader;
use std::sync::Arc;
use std::time::Instant;
use toasts::Toasts;
use views::meetings::MeetingsView;
use views::people::PeopleView;
use views::search::SearchView;
pub use views::settings::SettingsRoute;
use views::settings::SettingsView;
use views::transcript::TranscriptView;
use witness_core::player::Player;
use witness_core::{AppCore, RecCmd};

/// The current screen. Initial: `Meetings`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum View {
    #[default]
    Meetings,
    People,
    /// `from` is where Back returns to (Meetings when absent). Openers set
    /// it; the recording pill uses the current transcript's own `from`, so
    /// transcripts never nest.
    Transcript {
        meeting_id: i64,
        segment_id: Option<i64>,
        highlight: Option<String>,
        from: Option<Box<View>>,
    },
    Search {
        query: String,
    },
    Settings,
}

/// `WITNESS_DEBUG_START_VIEW` (debug builds only) for passive visual
/// checks: `meetings`, `people`, `settings`, `search:<query>` or
/// `transcript:<meeting id>` (opened from Meetings, so a wide window shows
/// it split beside the list). Anything else (or unset) is Meetings.
pub fn debug_start_view() -> View {
    #[cfg(debug_assertions)]
    if let Some(spec) = std::env::var_os("WITNESS_DEBUG_START_VIEW") {
        let spec = spec.to_string_lossy();
        match parse_start_view(&spec) {
            Some(view) => return view,
            None => log::warn!("WITNESS_DEBUG_START_VIEW={spec:?} not understood; ignored"),
        }
    }
    View::default()
}

/// The `WITNESS_DEBUG_START_VIEW` grammar.
#[cfg_attr(not(any(debug_assertions, test)), allow(dead_code))]
fn parse_start_view(spec: &str) -> Option<View> {
    let (kind, arg) = match spec.split_once(':') {
        Some((kind, arg)) => (kind, Some(arg)),
        None => (spec, None),
    };
    match (kind.trim(), arg) {
        ("meetings", None) => Some(View::Meetings),
        ("people", None) => Some(View::People),
        ("settings", None) => Some(View::Settings),
        ("search", Some(query)) => Some(View::Search {
            query: query.to_owned(),
        }),
        ("transcript", Some(id)) => id.trim().parse().ok().map(|meeting_id| View::Transcript {
            meeting_id,
            segment_id: None,
            highlight: None,
            from: Some(Box::new(View::Meetings)),
        }),
        _ => None,
    }
}

impl View {
    /// The remount key: the kind, or `transcript-<id>`. A new view value is
    /// created whenever it changes.
    ///
    /// Search's key ignores the query (as in app.tsx, a new query updates
    /// props without a remount), so `SearchView` must read the live query
    /// from `cx.view` (`View::Search { query }`) on every pass; the value
    /// `SearchView::new` received is only the first one. Likewise a
    /// transcript's `segment_id` / `highlight` / `from` can change without
    /// a remount.
    pub fn key(&self) -> String {
        match self {
            View::Meetings => "meetings".into(),
            View::People => "people".into(),
            View::Transcript { meeting_id, .. } => format!("transcript-{meeting_id}"),
            View::Search { .. } => "search".into(),
            View::Settings => "settings".into(),
        }
    }

    /// Meetings and Search show the search box above the view.
    pub fn has_search_box(&self) -> bool {
        self.is_list()
    }

    /// The views that list meetings: Meetings and Search.
    pub fn is_list(&self) -> bool {
        matches!(self, View::Meetings | View::Search { .. })
    }

    /// The list this view sits beside in a split: a list is its own, and a
    /// transcript opened from Meetings or Search has that list (one without
    /// a `from` returns to Meetings, so it has Meetings). None for the rest
    /// (People, Settings, a transcript opened from People…).
    pub fn split_list(&self) -> Option<View> {
        match self {
            View::Meetings | View::Search { .. } => Some(self.clone()),
            View::Transcript { from: None, .. } => Some(View::Meetings),
            View::Transcript {
                from: Some(from), ..
            } if from.is_list() => Some((**from).clone()),
            _ => None,
        }
    }
}

/// Below this content width the shell shows one view at a time.
pub const SPLIT_MIN_WIDTH: f32 = 900.0;
/// The split's list column: its default width and the drag limits.
pub const LIST_WIDTH: f32 = 360.0;
pub const LIST_MIN_WIDTH: f32 = 300.0;
pub const LIST_MAX_WIDTH: f32 = 520.0;
/// Dragging the divider never leaves the detail column narrower than this.
pub const DETAIL_MIN_WIDTH: f32 = 480.0;

/// How the content area is divided for a view at a width.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneLayout {
    /// One view, the whole width (narrow windows, and every view that has
    /// no list beside it).
    Single,
    /// The meeting list on the left, the transcript of `selected` (or a
    /// "Select a meeting" placeholder) on the right.
    Split { list: View, selected: Option<i64> },
}

impl PaneLayout {
    /// The layout for `view` in a content area `width` points wide.
    pub fn of(view: &View, width: f32) -> PaneLayout {
        if width < SPLIT_MIN_WIDTH {
            return PaneLayout::Single;
        }
        match view.split_list() {
            Some(list) => PaneLayout::Split {
                list,
                selected: match view {
                    View::Transcript { meeting_id, .. } => Some(*meeting_id),
                    _ => None,
                },
            },
            None => PaneLayout::Single,
        }
    }

    pub fn is_split(&self) -> bool {
        matches!(self, PaneLayout::Split { .. })
    }

    /// What is on screen, for transitions: the page (a split's list) and
    /// the detail pane's transcript. The shell animates a change of the
    /// first as a page change and of the second as a quick fade. Meetings
    /// and Search are one page (the search box filters it; a fade under
    /// the box being typed in would only distract).
    pub fn keys(&self, view: &View) -> (String, Option<String>) {
        let page = |view: &View| {
            if view.is_list() {
                "lists".to_owned()
            } else {
                view.key()
            }
        };
        match self {
            PaneLayout::Single => (page(view), None),
            PaneLayout::Split { list, selected } => (page(list), selected.map(|_| view.key())),
        }
    }
}

/// The list column's width in a content area `content` points wide, from
/// the user's preferred `width`: within the drag limits, and never so wide
/// that the detail column drops below [`DETAIL_MIN_WIDTH`].
pub fn list_column_width(width: f32, content: f32) -> f32 {
    let max = (content - DETAIL_MIN_WIDTH).min(LIST_MAX_WIDTH);
    width.min(max).max(LIST_MIN_WIDTH)
}

/// Navigation requests made during a pass; applied by `app.rs` after the
/// views have drawn (the old view gets `on_leave` first).
#[derive(Debug, Default)]
pub struct Nav {
    pub(crate) pending: Option<View>,
}

impl Nav {
    /// Go to `view` (the last request in a pass wins).
    pub fn go(&mut self, view: View) {
        self.pending = Some(view);
    }

    /// Transcript Back: to `from`, or Meetings when there is none.
    pub fn back(&mut self, from: Option<Box<View>>) {
        self.go(from.map(|v| *v).unwrap_or(View::Meetings));
    }

    /// The pending navigation, if any (taken once).
    pub fn take(&mut self) -> Option<View> {
        self.pending.take()
    }
}

/// App-level commands pushed by views. Handled in `logic()` (never in
/// `ui()`), so they run whether or not the window is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCmd {
    /// CC button, after `check_captions_overlay` succeeded.
    ShowCaptionsOverlay,
    /// Settings → Storage → Restart Witness (after any confirmation).
    Restart,
    /// Settings → About → Check for updates.
    CheckUpdate,
    /// Settings → About → Install.
    InstallUpdate,
    /// Settings → General → Run setup: show the first-run wizard.
    RunSetup,
}

/// Search "Who" filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Who {
    #[default]
    Anyone,
    Me,
    Them,
    Person(i64),
}

/// Search filters; app-owned so they survive leaving the Search view.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SearchFilters {
    pub who: Who,
    /// `YYYY-MM-DD` text as typed (validated by the Search view).
    pub date_from: String,
    pub date_to: String,
}

/// The shell's search box. Owned by the app (not by SearchView) so the top
/// bar's Meetings button can clear it and cancel its debounce (parity:
/// app.tsx:510-516).
#[derive(Debug, Default)]
pub struct SearchBox {
    pub text: String,
    /// Ctrl+F or `/` asked for focus; the box takes it on its next draw.
    pub request_focus: bool,
    /// When the 250 ms debounce fires.
    pub debounce_at: Option<Instant>,
}

impl SearchBox {
    /// Empty the box and cancel a pending debounce.
    pub fn clear(&mut self) {
        self.text.clear();
        self.debounce_at = None;
    }
}

/// State that outlives a view instance.
#[derive(Debug, Default)]
pub struct ViewMemory {
    /// How many meetings the list had loaded (a reload fetches at least this many).
    pub meetings_loaded: usize,
    pub search_filters: SearchFilters,
    pub search_box: SearchBox,
    /// meeting id → the newest notes draft handed to the writer this
    /// session, so reopening a meeting cannot adopt a database read that
    /// raced the previous visit's last write (transcript.rs `Notes::load`).
    pub queued_notes: std::collections::BTreeMap<i64, QueuedNotes>,
    /// A person to open expanded when People next loads (the command
    /// palette's person rows); taken by `PeopleView`.
    pub expand_person: Option<i64>,
    /// The Settings category shown last this session (and whether a narrow
    /// window had it open); Settings reopens there.
    pub settings_route: Option<SettingsRoute>,
}

/// A notes draft handed to the writer. `started_at` pins it to one meeting:
/// ids are reused after the newest meeting is purged, and a purged
/// meeting's draft must never surface in its successor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedNotes {
    pub started_at: String,
    pub seq: u64,
    pub text: String,
}

/// Everything a view may touch during a pass. Built fresh by `app.rs` for
/// each `ui()` and `flush()` call.
pub struct UiCtx<'a> {
    pub core: &'a Arc<AppCore>,
    pub tasks: &'a Tasks,
    pub shell: &'a mut ShellModel,
    pub settings: &'a mut SettingsStore,
    pub toasts: &'a mut Toasts,
    pub confirm: &'a mut Confirm,
    pub nav: &'a mut Nav,
    /// The view being shown (e.g. the transcript's `from` for Back).
    pub view: &'a View,
    pub player: &'a Player,
    pub prefs: &'a mut UiPrefs,
    pub memory: &'a mut ViewMemory,
    pub window: &'a MainWindow,
    pub flyout: &'a mut FlyoutState,
    pub cmds: &'a mut Vec<UiCmd>,
    pub fonts: &'a FontLoader,
    /// Where the view being drawn sits (wide windows split the meeting
    /// list and the open transcript side by side). In a split the list is
    /// drawn with `cx.view` = the list's own view (the transcript's `from`)
    /// and the transcript with its `View::Transcript`.
    pub pane: PaneRole,
}

/// The role of the view being drawn in the shell's layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneRole {
    /// The whole content area (narrow windows, and every non-split view).
    #[default]
    Full,
    /// The list column of a split: `selected` is the meeting open on the
    /// right (highlight it; opening another replaces it).
    List { selected: Option<i64> },
    /// The detail column of a split (no Back button: the list is beside it).
    Detail,
}

impl UiCtx<'_> {
    /// The owner for a native dialog. Opening a dialog pins the tray flyout
    /// first, so the dialog's focus change does not dismiss the window.
    pub fn dialog_owner(&mut self) -> OwnerHwnd {
        self.flyout.pin();
        self.window.owner()
    }

    /// The top bar's Meetings button: clear the search box (cancelling a
    /// pending debounce) and show the unfiltered list.
    pub fn go_meetings_clear_search(&mut self) {
        self.memory.search_box.clear();
        self.nav.go(View::Meetings);
    }

    /// Publish (or clear) the notes autosave deadline; `logic()` fires it
    /// even while `ui()` does not run (minimized).
    pub fn set_notes_deadline(&mut self, at: Option<Instant>) {
        self.shell.notes_deadline = at;
    }

    /// Top-bar recording control through the ordered rec-control worker.
    /// Start/Stop are ignored while one is in flight (the spinner shows);
    /// the reply arrives as `OpDone::Recording { origin: TopBar, .. }`.
    pub fn submit_rec(&mut self, cmd: RecCmd) {
        if self.shell.quitting && starts_recording(cmd) {
            log::debug!("top-bar {cmd:?} ignored: quitting");
            return;
        }
        let start_stop = matches!(cmd, RecCmd::Start(_) | RecCmd::Stop { .. });
        if start_stop {
            if self.shell.recording_action {
                return;
            }
            self.shell.recording_action = true;
        }
        submit_recording(self.tasks, cmd, RecOrigin::TopBar);
    }
}

/// Whether `cmd` can start a recording. While Tray Quit runs, those are
/// dropped: queued behind Quit's stop, they would start a recording that the
/// exit then kills without finalizing it.
pub fn starts_recording(cmd: RecCmd) -> bool {
    matches!(cmd, RecCmd::Start(_) | RecCmd::ToggleWithToast)
}

/// Submit `cmd` to the core's rec-control FIFO (never blocks) and forward
/// the reply to `logic()` as `OpDone::Recording`.
pub fn submit_recording(tasks: &Tasks, cmd: RecCmd, origin: RecOrigin) {
    let rx = tasks.core().submit_recording(cmd);
    tasks.forward_reply("rec-reply", rx, move |reply| OpDone::Recording {
        origin,
        cmd,
        result: reply.unwrap_or_else(|| Err("internal error: the recording worker stopped".into())),
    });
}

/// The view instances the shell keeps: the one for the current [`View`]
/// (`main`, remounted when [`View::key`] changes) and, while a transcript
/// is open, the list it was opened from. That list is drawn beside the
/// transcript in a split and kept behind it otherwise, so its state (scroll
/// position, loaded rows, the search results) survives selecting other
/// meetings, resizing across [`SPLIT_MIN_WIDTH`] and Back.
///
/// Lifecycle: `on_leave` runs when an instance is dropped and when the kept
/// list goes off screen (a narrow window, as when the single-view shell
/// replaced it); a list view shown again after its `on_leave` simply
/// continues (MeetingsView / SearchView only commit an edit and drop
/// pending timers there). Generic over the instance so the bookkeeping is
/// testable without real views; the app uses [`ActiveView`].
pub struct Screens<V> {
    main: V,
    list: Option<Kept<V>>,
}

/// The list kept beside / behind a transcript.
struct Kept<V> {
    /// Its view's [`View::key`].
    key: String,
    view: V,
    /// Drawn in a split. A hidden list has had its `on_leave`.
    shown: bool,
}

impl<V> Screens<V> {
    pub fn new(main: V) -> Screens<V> {
        Screens { main, list: None }
    }

    /// The instance for the current view.
    pub fn main(&mut self) -> &mut V {
        &mut self.main
    }

    /// Both instances for a split pass of `current`: the list and the
    /// transcript (`None` while the current view is the list itself). The
    /// list is created for `list` if none is kept, e.g. for a transcript
    /// opened by the recording pill with no list behind it.
    pub fn split(
        &mut self,
        current: &View,
        list: &View,
        make: impl FnOnce(&View) -> V,
    ) -> (&mut V, Option<&mut V>) {
        if current.is_list() {
            return (&mut self.main, None);
        }
        if !matches!(self.list.as_ref(), Some(kept) if kept.key == list.key()) {
            // `navigate` drops a list that is not this transcript's, so this
            // only fills an empty slot.
            self.list = Some(Kept {
                key: list.key(),
                view: make(list),
                shown: true,
            });
        }
        let kept = self.list.as_mut().expect("list slot just filled");
        kept.shown = true;
        (&mut kept.view, Some(&mut self.main))
    }

    /// Whether a list is kept (beside or behind the main view).
    #[cfg(test)]
    fn kept(&self) -> Option<(&str, bool)> {
        self.list.as_ref().map(|k| (k.key.as_str(), k.shown))
    }

    /// The kept list goes off screen (the window became too narrow for a
    /// split): it gets its `on_leave`, as a replaced list did before.
    pub fn hide_list(&mut self, mut leave: impl FnMut(&mut V)) {
        if let Some(kept) = self.list.as_mut().filter(|k| k.shown) {
            kept.shown = false;
            leave(&mut kept.view);
        }
    }

    /// `f` over every instance that is on screen or holds unsaved work
    /// (the main view, and the kept list while shown), for `flush`.
    pub fn each_live(&mut self, mut f: impl FnMut(&mut V)) {
        f(&mut self.main);
        if let Some(kept) = self.list.as_mut().filter(|k| k.shown) {
            f(&mut kept.view);
        }
    }

    /// Navigate from `current` to `next`; `split` says whether the next pass
    /// shows a split. Returns whether the main instance was replaced.
    ///
    /// - Opening a transcript from a list keeps that list (it stays on
    ///   screen in a split, and gets `on_leave` when it goes behind).
    /// - Returning to the kept list (Back, a new search query, the Meetings
    ///   button) reuses it instead of mounting a fresh one.
    /// - Everything replaced gets `on_leave` (the transcript's notes flush
    ///   and player unload included); a list no longer behind the current
    ///   transcript is dropped.
    pub fn navigate(
        &mut self,
        current: &View,
        next: &View,
        split: bool,
        mut make: impl FnMut(&View) -> V,
        mut leave: impl FnMut(&mut V),
    ) -> bool {
        let next_key = next.key();
        if next_key == current.key() {
            self.sync_list(next, &mut leave);
            return false;
        }
        let restored = match &self.list {
            Some(kept) if kept.key == next_key => self.list.take(),
            _ => None,
        };
        let main = match restored {
            Some(kept) => kept.view,
            None => make(next),
        };
        let mut old = std::mem::replace(&mut self.main, main);
        let keep_old = current.is_list()
            && matches!(next, View::Transcript { .. })
            && next
                .split_list()
                .is_some_and(|list| list.key() == current.key());
        if keep_old {
            if !split {
                leave(&mut old);
            }
            let stale = self.list.replace(Kept {
                key: current.key(),
                view: old,
                shown: split,
            });
            if let Some(mut stale) = stale.filter(|s| s.shown) {
                leave(&mut stale.view);
            }
        } else {
            leave(&mut old);
        }
        self.sync_list(next, &mut leave);
        true
    }

    /// Drop a kept list that is not `view`'s (only a transcript keeps one).
    fn sync_list(&mut self, view: &View, leave: &mut impl FnMut(&mut V)) {
        let wanted = match view {
            View::Transcript { .. } => view.split_list().map(|list| list.key()),
            _ => None,
        };
        if self.list.as_ref().map(|k| &k.key) != wanted.as_ref() {
            if let Some(mut stale) = self.list.take().filter(|s| s.shown) {
                leave(&mut stale.view);
            }
        }
    }
}

/// A view instance. Remounted when [`View::key`] changes.
pub enum ActiveView {
    Meetings(MeetingsView),
    People(PeopleView),
    Transcript(TranscriptView),
    Search(SearchView),
    Settings(SettingsView),
}

impl ActiveView {
    /// A fresh view instance for `view`.
    pub fn for_view(view: &View) -> ActiveView {
        match view {
            View::Meetings => ActiveView::Meetings(MeetingsView::new()),
            View::People => ActiveView::People(PeopleView::new()),
            View::Transcript {
                meeting_id,
                segment_id,
                highlight,
                ..
            } => ActiveView::Transcript(TranscriptView::new(
                *meeting_id,
                *segment_id,
                highlight.clone(),
            )),
            View::Search { query } => ActiveView::Search(SearchView::new(query.clone())),
            View::Settings => ActiveView::Settings(SettingsView::new()),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        match self {
            ActiveView::Meetings(v) => v.show(ui, cx),
            ActiveView::People(v) => v.show(ui, cx),
            ActiveView::Transcript(v) => v.show(ui, cx),
            ActiveView::Search(v) => v.show(ui, cx),
            ActiveView::Settings(v) => v.show(ui, cx),
        }
    }

    /// Persist anything unsaved WITHOUT tearing down (notes →
    /// `core.queue_notes`). Called by `will_hide()`, by the notes deadline
    /// in `logic()`, and by `prepare_exit()`. Must not block.
    pub fn flush(&mut self, cx: &mut UiCtx) {
        match self {
            ActiveView::Meetings(v) => v.flush(cx),
            ActiveView::People(v) => v.flush(cx),
            ActiveView::Transcript(v) => v.flush(cx),
            ActiveView::Search(v) => v.flush(cx),
            ActiveView::Settings(v) => v.flush(cx),
        }
    }

    /// `flush()` + teardown (TranscriptView: `player.unload()`). Called only
    /// when the view is replaced.
    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        match self {
            ActiveView::Meetings(v) => v.on_leave(cx),
            ActiveView::People(v) => v.on_leave(cx),
            ActiveView::Transcript(v) => v.on_leave(cx),
            ActiveView::Search(v) => v.on_leave(cx),
            ActiveView::Settings(v) => v.on_leave(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_start_view_grammar() {
        assert_eq!(parse_start_view("people"), Some(View::People));
        assert_eq!(parse_start_view("settings"), Some(View::Settings));
        assert_eq!(parse_start_view("meetings"), Some(View::Meetings));
        assert_eq!(
            parse_start_view("search:budget review"),
            Some(View::Search {
                query: "budget review".into()
            })
        );
        assert_eq!(
            parse_start_view("transcript:42"),
            Some(View::Transcript {
                meeting_id: 42,
                segment_id: None,
                highlight: None,
                from: Some(Box::new(View::Meetings)),
            })
        );
        assert_eq!(parse_start_view("transcript:x"), None);
        assert_eq!(parse_start_view("people:1"), None);
        assert_eq!(parse_start_view(""), None);
    }

    #[test]
    fn view_keys_remount_per_transcript() {
        let t = |id| View::Transcript {
            meeting_id: id,
            segment_id: None,
            highlight: None,
            from: None,
        };
        assert_eq!(View::Meetings.key(), "meetings");
        assert_eq!(View::Search { query: "a".into() }.key(), "search");
        assert_eq!(t(7).key(), "transcript-7");
        assert_ne!(t(7).key(), t(8).key());
        assert!(View::Meetings.has_search_box());
        assert!(!t(1).has_search_box());
    }

    #[test]
    fn back_defaults_to_meetings() {
        let mut nav = Nav::default();
        nav.back(None);
        assert_eq!(nav.take(), Some(View::Meetings));
        nav.back(Some(Box::new(View::Search { query: "q".into() })));
        assert_eq!(nav.take(), Some(View::Search { query: "q".into() }));
        assert_eq!(nav.take(), None);
    }

    fn transcript(id: i64, from: Option<View>) -> View {
        View::Transcript {
            meeting_id: id,
            segment_id: None,
            highlight: None,
            from: from.map(Box::new),
        }
    }

    fn search(query: &str) -> View {
        View::Search {
            query: query.into(),
        }
    }

    #[test]
    fn wide_windows_split_lists_and_their_transcripts() {
        let wide = SPLIT_MIN_WIDTH;
        let split = |list: View, selected| PaneLayout::Split { list, selected };
        assert_eq!(
            PaneLayout::of(&View::Meetings, wide),
            split(View::Meetings, None)
        );
        assert_eq!(
            PaneLayout::of(&search("q"), 1400.0),
            split(search("q"), None)
        );
        assert_eq!(
            PaneLayout::of(&transcript(7, Some(View::Meetings)), wide),
            split(View::Meetings, Some(7))
        );
        assert_eq!(
            PaneLayout::of(&transcript(7, Some(search("q"))), wide),
            split(search("q"), Some(7))
        );
        // No `from`: Back goes to Meetings, so Meetings sits beside it.
        assert_eq!(
            PaneLayout::of(&transcript(7, None), wide),
            split(View::Meetings, Some(7))
        );
        // Opened from elsewhere, or a page without a list: one view.
        for view in [
            transcript(7, Some(View::People)),
            View::People,
            View::Settings,
        ] {
            assert_eq!(
                PaneLayout::of(&view, 2000.0),
                PaneLayout::Single,
                "{view:?}"
            );
        }
        // Narrow: always one view.
        for view in [View::Meetings, transcript(7, Some(View::Meetings))] {
            assert_eq!(PaneLayout::of(&view, wide - 1.0), PaneLayout::Single);
        }
    }

    #[test]
    fn pane_keys_tell_page_changes_from_selection_changes() {
        let t7 = transcript(7, Some(View::Meetings));
        let t8 = transcript(8, Some(View::Meetings));
        let keys = |view: &View, width| PaneLayout::of(view, width).keys(view);
        // Split: the list is the page; the transcript is the detail.
        assert_eq!(keys(&t7, 1400.0).0, keys(&t8, 1400.0).0);
        assert_ne!(keys(&t7, 1400.0).1, keys(&t8, 1400.0).1);
        assert_eq!(keys(&View::Meetings, 1400.0), ("lists".into(), None));
        // Single: the transcript is the page.
        assert_eq!(keys(&t7, 600.0), ("transcript-7".into(), None));
        // Typing a query does not change the page.
        assert_eq!(keys(&View::Meetings, 600.0), keys(&search("q"), 600.0));
        assert_eq!(keys(&View::Meetings, 1400.0), keys(&search("q"), 1400.0));
        assert_ne!(keys(&View::People, 600.0), keys(&View::Meetings, 600.0));
    }

    #[test]
    fn list_column_stays_within_its_limits() {
        assert_eq!(list_column_width(LIST_WIDTH, 1400.0), LIST_WIDTH);
        assert_eq!(list_column_width(10.0, 1400.0), LIST_MIN_WIDTH);
        assert_eq!(list_column_width(5000.0, 1400.0), LIST_MAX_WIDTH);
        // The detail column keeps its minimum first…
        assert_eq!(
            list_column_width(LIST_MAX_WIDTH, 900.0),
            900.0 - DETAIL_MIN_WIDTH
        );
        // …but the list never drops below its own.
        assert_eq!(list_column_width(LIST_WIDTH, 700.0), LIST_MIN_WIDTH);
    }

    /// A stand-in view instance: its key and a serial number.
    #[derive(Debug, Clone, PartialEq)]
    struct Fake(String, usize);

    /// Drives `Screens<Fake>` like the shell does, logging `on_leave`s.
    struct Harness {
        screens: Screens<Fake>,
        view: View,
        made: usize,
        left: Vec<Fake>,
    }

    impl Harness {
        fn new(view: View) -> Harness {
            Harness {
                screens: Screens::new(Fake(view.key(), 0)),
                view,
                made: 1,
                left: Vec::new(),
            }
        }

        fn go(&mut self, next: View, split: bool) -> bool {
            let made = &mut self.made;
            let left = &mut self.left;
            let replaced = self.screens.navigate(
                &self.view,
                &next,
                split,
                |v| {
                    *made += 1;
                    Fake(v.key(), *made - 1)
                },
                |f| left.push(f.clone()),
            );
            self.view = next;
            replaced
        }

        /// A split pass: (list, detail) as drawn.
        fn split_pass(&mut self) -> (Fake, Option<Fake>) {
            let list = self.view.split_list().expect("a split view");
            let made = &mut self.made;
            let (l, d) = self.screens.split(&self.view, &list, |v| {
                *made += 1;
                Fake(v.key(), *made - 1)
            });
            (l.clone(), d.cloned())
        }

        fn main(&mut self) -> Fake {
            self.screens.main().clone()
        }

        fn left_keys(&mut self) -> Vec<String> {
            std::mem::take(&mut self.left)
                .into_iter()
                .map(|f| f.0)
                .collect()
        }
    }

    #[test]
    fn a_split_keeps_its_list_while_the_selection_changes() {
        let mut h = Harness::new(View::Meetings);
        assert_eq!(h.split_pass(), (Fake("meetings".into(), 0), None));
        assert!(h.go(transcript(7, Some(View::Meetings)), true));
        // The list stays mounted and on screen: no on_leave.
        assert_eq!(h.left_keys(), Vec::<String>::new());
        let (list, detail) = h.split_pass();
        assert_eq!(list, Fake("meetings".into(), 0));
        assert_eq!(detail.map(|d| d.0), Some("transcript-7".into()));
        // Another meeting: only the old transcript leaves (notes flush).
        assert!(h.go(transcript(8, Some(View::Meetings)), true));
        assert_eq!(h.left_keys(), ["transcript-7"]);
        assert_eq!(h.split_pass().0, Fake("meetings".into(), 0));
        // Back to the list: the same instance becomes the main view.
        assert!(h.go(View::Meetings, true));
        assert_eq!(h.left_keys(), ["transcript-8"]);
        assert_eq!(h.main(), Fake("meetings".into(), 0));
        assert_eq!(h.screens.kept(), None);
    }

    #[test]
    fn a_narrow_window_leaves_the_list_but_back_restores_it() {
        let mut h = Harness::new(View::Meetings);
        h.go(transcript(7, Some(View::Meetings)), false);
        // As the single-view shell did: the list gets its on_leave…
        assert_eq!(h.left_keys(), ["meetings"]);
        assert_eq!(h.screens.kept(), Some(("meetings", false)));
        // …but is kept, so Back returns to it (scroll position and all).
        h.go(View::Meetings, false);
        assert_eq!(h.left_keys(), ["transcript-7"]);
        assert_eq!(h.main(), Fake("meetings".into(), 0));
    }

    #[test]
    fn resizing_across_the_threshold_keeps_both_views() {
        let mut h = Harness::new(View::Meetings);
        h.go(transcript(7, Some(View::Meetings)), true);
        let transcript_instance = h.main();
        // Narrower: the list goes off screen (on_leave once)…
        h.screens.hide_list(|f| h.left.push(f.clone()));
        h.screens.hide_list(|f| h.left.push(f.clone()));
        assert_eq!(h.left_keys(), ["meetings"]);
        // …wider again: the very same instances come back.
        let (list, detail) = h.split_pass();
        assert_eq!(list, Fake("meetings".into(), 0));
        assert_eq!(detail, Some(transcript_instance));
        assert_eq!(h.screens.kept(), Some(("meetings", true)));
        assert_eq!(h.left_keys(), Vec::<String>::new());
    }

    #[test]
    fn a_new_query_reuses_the_search_list_behind_a_transcript() {
        let mut h = Harness::new(search("budget"));
        h.go(transcript(3, Some(search("budget"))), true);
        // Typing a new query: the transcript leaves, the list stays.
        assert!(h.go(search("budget review"), true));
        assert_eq!(h.left_keys(), ["transcript-3"]);
        assert_eq!(h.main(), Fake("search".into(), 0));
    }

    #[test]
    fn leaving_the_lists_drops_the_kept_one() {
        let mut h = Harness::new(View::Meetings);
        h.go(transcript(7, Some(View::Meetings)), true);
        h.go(View::People, true);
        assert_eq!(h.left_keys(), ["transcript-7", "meetings"]);
        assert_eq!(h.screens.kept(), None);
        // A transcript from People has no list; nothing is kept.
        h.go(transcript(9, Some(View::People)), true);
        assert_eq!(h.left_keys(), ["people"]);
        assert_eq!(h.screens.kept(), None);
    }

    #[test]
    fn a_hidden_list_is_not_left_twice() {
        let mut h = Harness::new(View::Meetings);
        h.go(transcript(7, Some(View::Meetings)), false);
        assert_eq!(h.left_keys(), ["meetings"]);
        h.go(View::Settings, false);
        assert_eq!(h.left_keys(), ["transcript-7"]);
        assert_eq!(h.screens.kept(), None);
    }

    #[test]
    fn a_transcript_without_a_list_gets_one_when_split() {
        // The recording pill opens a transcript with no list behind it.
        let mut h = Harness::new(transcript(4, None));
        let (list, detail) = h.split_pass();
        assert_eq!(list.0, "meetings");
        assert_eq!(detail.map(|d| d.0), Some("transcript-4".into()));
        // A transcript from another list replaces the kept one.
        h.go(transcript(5, Some(search("x"))), true);
        assert_eq!(h.left_keys(), ["transcript-4", "meetings"]);
        assert_eq!(h.split_pass().0 .0, "search");
    }

    #[test]
    fn search_box_clear_cancels_the_debounce() {
        let mut b = SearchBox {
            text: "budget".into(),
            request_focus: false,
            debounce_at: Some(Instant::now()),
        };
        b.clear();
        assert!(b.text.is_empty() && b.debounce_at.is_none());
    }
}
