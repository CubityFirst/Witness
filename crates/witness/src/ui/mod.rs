//! The native UI: navigation (`View`, replacing app.tsx's `View` union),
//! the per-pass context handed to views (`UiCtx`), app-owned view memory,
//! and the shared kit (theme, fonts, icons, widgets, formatting, confirm
//! modal, toasts). Views live in `views/`; the top bar and the captions
//! overlay have their own modules.

// The kit (F0) is complete but mostly consumed by the views; `captions`
// is still the F0 stub. TODO(I1): drop these allows after the merge.
#[allow(dead_code)]
pub mod captions;
#[allow(dead_code)]
pub mod confirm;
pub mod fonts;
#[allow(dead_code)]
pub mod format;
#[allow(dead_code)]
pub mod icons;
#[allow(dead_code)]
pub mod theme;
#[allow(dead_code)]
pub mod toasts;
pub mod topbar;
pub mod views;
#[allow(dead_code)]
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
use views::settings::SettingsView;
use views::transcript::TranscriptView;
use witness_core::player::Player;
use witness_core::{AppCore, RecCmd};

/// The current screen. Initial: `Meetings`.
#[allow(dead_code)] // TODO(I1): consumers land with the units.
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
        matches!(self, View::Meetings | View::Search { .. })
    }
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
#[allow(dead_code)] // TODO(I1): consumers land with the units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCmd {
    /// CC button, after `check_captions_overlay` succeeded.
    ShowCaptionsOverlay,
    /// Settings → Restart Witness (after any confirmation).
    Restart,
    /// Settings → Updates → Check for updates.
    CheckUpdate,
    /// Settings → Updates → Install.
    InstallUpdate,
    /// Quit the app (same path as tray Quit).
    Quit,
}

/// Search "Who" filter.
#[allow(dead_code)] // TODO(I1): consumers land with the units.
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
#[allow(dead_code)] // TODO(I1): consumers land with the units.
#[derive(Debug, Default)]
pub struct ViewMemory {
    /// How many meetings the list had loaded (a reload fetches at least this many).
    pub meetings_loaded: usize,
    pub search_filters: SearchFilters,
    pub search_box: SearchBox,
}

/// Everything a view may touch during a pass. Built fresh by `app.rs` for
/// each `ui()` and `flush()` call.
#[allow(dead_code)] // TODO(I1): consumers land with the units.
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
}

#[allow(dead_code)] // TODO(I1): consumers land with the units.
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

/// The view instance on screen. Remounted when [`View::key`] changes.
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
