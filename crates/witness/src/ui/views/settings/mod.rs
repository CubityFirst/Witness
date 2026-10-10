//! Settings, organised like Windows 11's: seven categories
//! ([`SettingsPage`]) — General, Recording, Transcription, AI assistant,
//! Voices, Storage and About — one file each, plus the shared row kit
//! (`kit.rs`) and the navigation chrome (`nav.rs`).
//!
//! - **Layout** (docs/UI.md): from [`nav::WIDE_MIN`] of content width a
//!   navigation rail (icon + name, the selected category a neutral fill)
//!   sits left of the selected category's page, which scrolls on its own;
//!   narrower (the tray flyout included) an index lists the categories with
//!   a one-line live summary each, and opening one shows its page with a
//!   Back button. A page is a title and its sections: a header and one
//!   raised group of [`row`]s — title + muted description on the left, the
//!   control (switch, select, button, badge) on the right, stacking under
//!   the text when the row gets narrow. Long names and paths elide with a
//!   tooltip; destructive actions sit last in their group.
//! - **Routing**: the category shown last (and whether a narrow window had
//!   it open) is remembered for the session in `ViewMemory`.
//! - **Saving** goes through the app-owned `SettingsStore` (`cx.settings`):
//!   the view renders from its optimistic copy, writes are serialised
//!   (≤1 in flight + 1 queued, latest wins) and survive leaving Settings,
//!   and a failed write reverts with "Could not save settings: …". Every
//!   edit starts from the store's *current* value, so two commits in one
//!   pass (a text field blurring because a checkbox was clicked) compose.
//! - **Loads** (models, cache size, GPU, GPU libraries, people, devices,
//!   autostart, diagnostics, Windows' accent) run in parallel as view-owned
//!   `Task`s whatever the category (the summaries need them); leaving the
//!   view drops them. Download progress is global (`shell.download`), so
//!   badges survive leaving and returning; when a download finishes
//!   (`shell.download_done_tick`) the model, cache, GPU and GPU-library rows
//!   reload, and a saved data_dir change (`shell.settings_tick`) reloads
//!   diagnostics and shows the restart row.
//! - **Actions** whose outcome is reported globally (backup, delete model
//!   cache, forget voice, downloads, diagnostics export) use `spawn_op`, so
//!   their toasts appear even if the user navigates away; the view keeps
//!   the `OpHandle` for its local state ("Deleting…", reloads).
//! - Native dialogs block, so they only run inside those worker closures,
//!   parented through `cx.dialog_owner()` (which pins the tray flyout).

mod about;
mod ai;
mod general;
mod kit;
mod nav;
mod recording;
mod search;
mod storage;
mod transcription;
mod updates;
mod voices;

pub(crate) use general::{accent_row, theme_row, SystemLook};
pub(crate) use kit::*;
pub(crate) use recording::{device_row, hotkey_label, DeviceRow};
pub(crate) use storage::{
    download_active, download_fraction, download_progress_text, start_model_download,
};

use crate::bridge::{OpHandle, Task};
use crate::platform::autostart;
use crate::ui::icons::ph;
use crate::ui::theme;
use crate::ui::UiCmd;
use crate::ui::UiCtx;
use ai::AiSection;
use eframe::egui;
use recording::{parse_patterns, patterns_id};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use storage::{cache_confirm_id, restart_confirm_id};
use transcription::{junk_id, parse_lines};
use updates::UpdatesSection;
use voices::forget_confirm_id;
use witness_core::backup::BackupSummary;
use witness_core::models::ModelInfo;
use witness_core::service::{AudioDevices, Diagnostics, GpuLibsInfo, GpuStatus, PersonInfo};
use witness_core::settings::Settings;
use witness_core::{AppCore, CoreResult};

/// A Settings category, in navigation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SettingsPage {
    /// Appearance and startup.
    #[default]
    General,
    /// Meeting detection, devices, hotkeys.
    Recording,
    /// Engine, GPU, live captions, noise filter.
    Transcription,
    /// The opt-in AI service.
    Ai,
    /// Voice matching and enrolled voice prints.
    Voices,
    /// Data folder, backup, models.
    Storage,
    /// Version and updates, privacy, diagnostics.
    About,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 7] = [
        SettingsPage::General,
        SettingsPage::Recording,
        SettingsPage::Transcription,
        SettingsPage::Ai,
        SettingsPage::Voices,
        SettingsPage::Storage,
        SettingsPage::About,
    ];

    /// The page title and navigation label.
    pub fn title(self) -> &'static str {
        match self {
            SettingsPage::General => "General",
            SettingsPage::Recording => "Recording",
            SettingsPage::Transcription => "Transcription",
            SettingsPage::Ai => "AI assistant",
            SettingsPage::Voices => "Voices",
            SettingsPage::Storage => "Storage",
            SettingsPage::About => "About",
        }
    }

    /// The Phosphor glyph beside the title.
    pub fn icon(self) -> &'static str {
        match self {
            SettingsPage::General => ph::SLIDERS_HORIZONTAL,
            SettingsPage::Recording => ph::MICROPHONE,
            SettingsPage::Transcription => ph::CLOSED_CAPTIONING,
            SettingsPage::Ai => ph::SPARKLE,
            SettingsPage::Voices => ph::USER_SOUND,
            SettingsPage::Storage => ph::HARD_DRIVES,
            SettingsPage::About => ph::INFO,
        }
    }

    /// A stable lower-case name (debug hooks, ids).
    pub fn slug(self) -> &'static str {
        match self {
            SettingsPage::General => "general",
            SettingsPage::Recording => "recording",
            SettingsPage::Transcription => "transcription",
            SettingsPage::Ai => "ai",
            SettingsPage::Voices => "voices",
            SettingsPage::Storage => "storage",
            SettingsPage::About => "about",
        }
    }

    /// The page named `slug` (case-insensitive).
    pub fn from_slug(slug: &str) -> Option<SettingsPage> {
        let slug = slug.trim();
        Self::ALL
            .into_iter()
            .find(|p| p.slug().eq_ignore_ascii_case(slug))
    }
}

/// Which category Settings shows; remembered for the session in
/// `ViewMemory::settings_route`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SettingsRoute {
    /// The category shown beside the rail, or last opened from the index.
    pub page: SettingsPage,
    /// Narrow windows show `page` (with Back) instead of the index.
    pub open: bool,
}

impl SettingsRoute {
    /// `page`, opened (a rail click, an index row, a search hit, a link).
    pub fn opened(page: SettingsPage) -> SettingsRoute {
        SettingsRoute { page, open: true }
    }

    /// Back to the index (the page stays the wide window's selection).
    pub fn back(self) -> SettingsRoute {
        SettingsRoute {
            open: false,
            ..self
        }
    }
}

/// The parallel mount-time loads (settings.tsx `useEffect`). A `None` slot
/// is idle; replacing a slot discards the older request's result.
#[derive(Default)]
struct Loads {
    models: Option<Task<Vec<ModelInfo>>>,
    cache: Option<Task<u64>>,
    gpu: Option<Task<GpuStatus>>,
    gpu_libs: Option<Task<GpuLibsInfo>>,
    people: Option<Task<CoreResult<Vec<PersonInfo>>>>,
    devices: Option<Task<CoreResult<AudioDevices>>>,
    autostart: Option<Task<Result<bool, String>>>,
    diagnostics: Option<Task<Diagnostics>>,
    pick_dir: Option<Task<Option<PathBuf>>>,
    look: Option<Task<SystemLook>>,
}

/// The view handle: its state is boxed so `ActiveView` stays small.
pub struct SettingsView {
    state: Box<SettingsState>,
}

impl SettingsView {
    pub fn new() -> Self {
        SettingsView {
            state: Box::new(SettingsState::new()),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        self.state.show(ui, cx);
    }

    /// Commit a text-field draft that has not been blurred yet, or a slider
    /// drag that has not been released (the view is being left, or the
    /// window hidden). Other edits already went through `SettingsStore`,
    /// whose queue outlives the view.
    pub fn flush(&mut self, cx: &mut UiCtx) {
        self.state.commit_drafts(cx);
    }

    pub fn on_leave(&mut self, cx: &mut UiCtx) {
        // An answer given in the pass that also navigated away still acts.
        self.state.answer_confirms(cx);
        self.flush(cx);
    }
}

impl Default for SettingsView {
    fn default() -> Self {
        Self::new()
    }
}

struct SettingsState {
    updates: UpdatesSection,
    ai: AiSection,
    loads: Loads,
    /// First pass done (loads started, ticks recorded).
    mounted: bool,
    seen_download_tick: u64,
    seen_settings_tick: u64,

    /// The category shown, and whether a narrow window has it open.
    route: SettingsRoute,
    /// The remembered category was picked up (first pass).
    routed: bool,
    /// The settings search as typed, and the result ↑/↓ selected.
    search_text: String,
    search_selected: usize,
    /// Focus the search field when it next draws (Ctrl+F, `/`).
    search_focus: bool,
    /// Debug builds: `WITNESS_DEBUG_SETTINGS_RESULT` (opened once).
    #[cfg(debug_assertions)]
    debug_result: Option<usize>,

    models: Vec<ModelInfo>,
    cache_bytes: Option<u64>,
    gpu: Option<GpuStatus>,
    gpu_libs: Option<GpuLibsInfo>,
    people: Vec<PersonInfo>,
    /// The people list has loaded once (the Voices summary waits for it).
    people_loaded: bool,
    devices: Option<AudioDevices>,
    devices_error: Option<String>,
    /// None until loaded (the switch is disabled meanwhile).
    autostart: Option<bool>,
    diagnostics: Option<Diagnostics>,
    diagnostics_error: Option<String>,
    diagnostics_copied_at: Option<Instant>,
    /// Windows' accent shades (None until read).
    look: Option<SystemLook>,

    /// Watch patterns as typed, and the settings value the text reflects
    /// (re-synced when the stored value moves away, e.g. a failed save).
    patterns_text: String,
    patterns_source: Vec<String>,
    /// Typed into since the last commit / re-sync.
    patterns_dirty: bool,
    junk_text: String,
    junk_source: Vec<String>,
    junk_dirty: bool,
    /// The slider value while it is being dragged (committed on release).
    local_threshold: Option<f32>,

    /// A data_dir change was saved while this view was open.
    needs_restart: bool,
    /// The registry write after toggling autostart (the switch is
    /// disabled meanwhile); an op, so a failure toasts after leaving too.
    autostart_write: Option<OpRun<bool>>,
    backup: Option<OpRun<BackupSummary>>,
    last_backup: Option<BackupSummary>,
    deleting_cache: Option<OpHandle>,
    forgetting: Vec<OpHandle>,
    /// The person a pending "Forget voice" confirmation is about.
    confirm_forget: Option<(i64, String)>,
    /// This view asked the "Delete models" / "Stop & restart" confirms.
    /// The modal is app-level and outlives the view (Ctrl+F navigates while
    /// it shows), so an answer given after leaving must not fire on a later
    /// visit: answers are drained every pass (and on leave) and acted on
    /// only when asked. A modal still open after leaving answers into the
    /// void — closing it on leave needs a `Confirm` API (integration).
    asked_cache: bool,
    asked_restart: bool,

    /// `WITNESS_DEBUG_SCROLL` was applied (or is not set).
    #[cfg(debug_assertions)]
    debug_scrolled: bool,
    #[cfg(debug_assertions)]
    started: Instant,
}

impl SettingsState {
    fn new() -> Self {
        SettingsState {
            updates: UpdatesSection::new(),
            ai: AiSection::new(),
            loads: Loads::default(),
            mounted: false,
            seen_download_tick: 0,
            seen_settings_tick: 0,
            route: SettingsRoute::default(),
            routed: false,
            search_text: String::new(),
            search_selected: 0,
            search_focus: false,
            #[cfg(debug_assertions)]
            debug_result: None,
            models: Vec::new(),
            cache_bytes: None,
            gpu: None,
            gpu_libs: None,
            people: Vec::new(),
            people_loaded: false,
            devices: None,
            devices_error: None,
            autostart: None,
            diagnostics: None,
            diagnostics_error: None,
            diagnostics_copied_at: None,
            look: None,
            patterns_text: String::new(),
            patterns_source: Vec::new(),
            patterns_dirty: false,
            junk_text: String::new(),
            junk_source: Vec::new(),
            junk_dirty: false,
            local_threshold: None,
            needs_restart: false,
            autostart_write: None,
            backup: None,
            last_backup: None,
            deleting_cache: None,
            forgetting: Vec::new(),
            confirm_forget: None,
            asked_cache: false,
            asked_restart: false,
            #[cfg(debug_assertions)]
            debug_scrolled: false,
            #[cfg(debug_assertions)]
            started: Instant::now(),
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if !self.mounted {
            self.mount(cx);
        }
        self.poll(ui.ctx(), cx);
        self.watch_ticks(cx);
        self.answer_confirms(cx);
        self.sync_buffers(ui.ctx(), cx.settings.current());
        self.route(cx);
        let wide = nav::is_wide(ui.available_width());
        if std::mem::take(&mut cx.memory.settings_search_focus) {
            // Ctrl+F / `/`: the field is on the rail, or on the index.
            self.search_focus = true;
            if !wide && self.route.open {
                self.close_page(cx);
            }
        }
        #[cfg(debug_assertions)]
        if let Some(n) = self.debug_result.take() {
            let flat = search::flatten(&search::search(&self.search_text));
            match flat.get(n) {
                Some(&index) => self.open_hit(ui.ctx(), cx, index, true),
                None => log::warn!("WITNESS_DEBUG_SETTINGS_RESULT={n}: no such result"),
            }
        }
        if wide {
            self.wide(ui, cx);
        } else {
            self.narrow(ui, cx);
        }
    }

    /// First pass: pick up the category remembered for this session (or
    /// the debug hook's).
    fn route(&mut self, cx: &mut UiCtx) {
        if !self.routed {
            self.routed = true;
            if let Some(route) = cx.memory.settings_route {
                self.route = route;
            }
            #[cfg(debug_assertions)]
            if let Ok(spec) = std::env::var("WITNESS_DEBUG_SETTINGS_PAGE") {
                match nav::parse_debug_page(&spec) {
                    Some(Some(page)) => self.open_page(cx, page),
                    Some(None) => self.close_page(cx),
                    None => log::warn!("WITNESS_DEBUG_SETTINGS_PAGE={spec:?} not understood"),
                }
            }
            #[cfg(debug_assertions)]
            if let Ok(query) = std::env::var("WITNESS_DEBUG_SETTINGS_SEARCH") {
                // Shown with focus, on the index when narrow.
                self.search_text = query;
                self.search_focus = true;
                self.route.open = false;
                self.debug_result = std::env::var("WITNESS_DEBUG_SETTINGS_RESULT")
                    .ok()
                    .and_then(|n| n.trim().parse().ok());
            }
        }
    }

    /// Show `page` (and, in a narrow window, open it over the index).
    fn open_page(&mut self, cx: &mut UiCtx, page: SettingsPage) {
        self.route = SettingsRoute::opened(page);
        cx.memory.settings_route = Some(self.route);
    }

    /// Narrow windows: back from a page to the index.
    fn close_page(&mut self, cx: &mut UiCtx) {
        self.route = self.route.back();
        cx.memory.settings_route = Some(self.route);
    }

    /// The page's sections.
    fn page_body(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx, page: SettingsPage) {
        // The column helpers zero the horizontal spacing; rows of buttons
        // need theirs back.
        ui.spacing_mut().item_spacing.x = theme::S2;
        match page {
            SettingsPage::General => self.general_page(ui, cx),
            SettingsPage::Recording => self.recording_page(ui, cx),
            SettingsPage::Transcription => self.transcription_page(ui, cx),
            SettingsPage::Ai => self.ai.show(ui, cx),
            SettingsPage::Voices => self.voices_page(ui, cx),
            SettingsPage::Storage => self.storage_page(ui, cx),
            SettingsPage::About => self.about_page(ui, cx),
        }
    }

    /// The category's one-line summary of its current values.
    fn summary(&self, cx: &UiCtx, page: SettingsPage) -> String {
        match page {
            SettingsPage::General => self.general_summary(cx),
            SettingsPage::Recording => self.recording_summary(cx),
            SettingsPage::Transcription => self.transcription_summary(cx),
            SettingsPage::Ai => self.ai.summary(cx),
            SettingsPage::Voices => self.voices_summary(cx),
            SettingsPage::Storage => self.storage_summary(cx),
            SettingsPage::About => self.about_summary(cx),
        }
    }

    /// Debug builds: `WITNESS_DEBUG_SCROLL=<pt>` opens Settings scrolled
    /// down that far, once the page is tall enough (off-screen snapshots of
    /// the lower sections; the window cannot be taller than the screen).
    #[cfg(debug_assertions)]
    fn debug_scroll<R>(
        &mut self,
        ctx: &egui::Context,
        out: &egui::scroll_area::ScrollAreaOutput<R>,
    ) {
        if self.debug_scrolled {
            return;
        }
        let Some(y) = std::env::var("WITNESS_DEBUG_SCROLL")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
        else {
            self.debug_scrolled = true;
            return;
        };
        // Wait for the loads: before them the page is too short to scroll.
        let max = out.content_size.y - out.inner_rect.height();
        if max >= y || self.started.elapsed() > Duration::from_millis(1500) {
            self.debug_scrolled = true;
            let mut state = out.state;
            state.offset.y = y.min(max.max(0.0));
            state.store(ctx, out.id);
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    /// Save the watch patterns / noise phrases if they were typed into
    /// since their last commit, and a slider value still being dragged.
    fn commit_drafts(&mut self, cx: &mut UiCtx) {
        self.ai.commit_drafts(cx);
        if let Some(threshold) = self.local_threshold.take() {
            if threshold != cx.settings.current().speaker_match_threshold {
                save(cx, |s| s.speaker_match_threshold = threshold);
            }
        }
        if self.patterns_dirty {
            self.commit_patterns(cx, parse_patterns(&self.patterns_text));
        }
        if self.junk_dirty {
            self.commit_junk(cx, parse_lines(&self.junk_text));
        }
    }

    // ------------------------------------------------------------------
    // Loading

    fn mount(&mut self, cx: &mut UiCtx) {
        self.mounted = true;
        self.seen_download_tick = cx.shell.download_done_tick;
        self.seen_settings_tick = cx.shell.settings_tick;
        let settings = cx.settings.current();
        self.patterns_text = settings.watch_patterns.join(", ");
        self.patterns_source = settings.watch_patterns.clone();
        self.junk_text = settings.junk_phrases.join("\n");
        self.junk_source = settings.junk_phrases.clone();
        self.refresh_models(cx);
        self.refresh_gpu(cx);
        self.refresh_people(cx);
        self.refresh_devices(cx);
        // A registry read; off the UI thread like every other load.
        self.loads.autostart = Some(
            cx.tasks
                .spawn("autostart-read", |_| autostart::is_enabled()),
        );
        self.refresh_diagnostics(cx);
        self.loads.look = Some(SystemLook::load(cx));
    }

    fn refresh_models(&mut self, cx: &UiCtx) {
        self.loads.models = Some(cx.tasks.spawn("model-status", AppCore::model_status));
        self.loads.cache = Some(
            cx.tasks
                .spawn("model-cache-size", AppCore::model_cache_size),
        );
    }

    fn refresh_gpu(&mut self, cx: &UiCtx) {
        self.loads.gpu = Some(cx.tasks.spawn("gpu-status", AppCore::gpu_status));
        self.loads.gpu_libs = Some(cx.tasks.spawn("gpu-libs-status", AppCore::gpu_libs_status));
    }

    fn refresh_people(&mut self, cx: &UiCtx) {
        self.loads.people = Some(cx.tasks.spawn("list-people", AppCore::list_people));
    }

    fn refresh_devices(&mut self, cx: &UiCtx) {
        self.devices_error = None;
        self.loads.devices = Some(cx.tasks.spawn("list-devices", AppCore::list_audio_devices));
    }

    fn refresh_diagnostics(&mut self, cx: &UiCtx) {
        self.diagnostics_error = None;
        self.loads.diagnostics = Some(cx.tasks.spawn("diagnostics", AppCore::diagnostics));
    }

    /// Collect finished loads and actions.
    fn poll(&mut self, ctx: &egui::Context, cx: &mut UiCtx) {
        if let Some(result) = take(&mut self.loads.models) {
            match result {
                Ok(models) => self.models = models,
                Err(error) => cx.toasts.error("Could not load model status", error),
            }
        }
        if let Some(result) = take(&mut self.loads.cache) {
            match result {
                Ok(bytes) => self.cache_bytes = Some(bytes),
                Err(error) => cx.toasts.error("Could not measure the model cache", error),
            }
        }
        if let Some(result) = take(&mut self.loads.gpu) {
            match result {
                Ok(gpu) => self.gpu = Some(gpu),
                Err(error) => cx.toasts.error("Could not check GPU status", error),
            }
        }
        if let Some(result) = take(&mut self.loads.gpu_libs) {
            match result {
                Ok(libs) => self.gpu_libs = Some(libs),
                Err(error) => cx.toasts.error("Could not check GPU library status", error),
            }
        }
        if let Some(result) = take(&mut self.loads.people) {
            match result.and_then(|inner| inner) {
                Ok(people) => {
                    cx.fonts
                        .note_texts(ctx, people.iter().map(|p| p.name.as_str()));
                    self.people = people;
                    self.people_loaded = true;
                }
                Err(error) => cx.toasts.error("Could not load enrolled people", error),
            }
        }
        if let Some(result) = take(&mut self.loads.devices) {
            match result.and_then(|inner| inner) {
                Ok(devices) => {
                    let names = devices.capture.iter().chain(&devices.render);
                    cx.fonts.note_texts(ctx, names.map(|d| d.name.as_str()));
                    self.devices = Some(devices);
                }
                Err(error) => self.devices_error = Some(error),
            }
        }
        if let Some(result) = take(&mut self.loads.autostart) {
            match result.and_then(|inner| inner) {
                Ok(enabled) => self.autostart = Some(enabled),
                Err(error) => cx.toasts.error("Could not read autostart status", error),
            }
        }
        // A failed write was toasted by the op; the switch stays as it was.
        if let Some(Some(enabled)) = OpRun::finished(&mut self.autostart_write) {
            self.autostart = Some(enabled);
        }
        if let Some(result) = take(&mut self.loads.diagnostics) {
            match result {
                Ok(diagnostics) => self.diagnostics = Some(diagnostics),
                Err(error) => self.diagnostics_error = Some(error),
            }
        }
        if let Some(result) = take(&mut self.loads.pick_dir) {
            match result {
                Ok(Some(dir)) => {
                    let dir = dir.to_string_lossy().into_owned();
                    if cx.settings.current().data_dir.as_deref() != Some(dir.as_str()) {
                        // On success `shell.settings_tick` moves (watch_ticks):
                        // diagnostics reload and the restart row shows.
                        save(cx, |s| s.data_dir = Some(dir));
                    }
                }
                Ok(None) => {}
                Err(error) => cx.toasts.error("Could not choose a data directory", error),
            }
        }

        if let Some(result) = take(&mut self.loads.look) {
            // A failed read only costs the accent swatch.
            self.look = Some(result.unwrap_or_default());
        }

        if let Some(Some(summary)) = OpRun::finished(&mut self.backup) {
            self.last_backup = Some(summary);
        }
        if self
            .deleting_cache
            .take_if(|handle| handle.is_done())
            .is_some()
        {
            self.refresh_models(cx);
        }
        let before = self.forgetting.len();
        self.forgetting.retain(|handle| !handle.is_done());
        if self.forgetting.len() != before {
            // A forget finished (or failed): show the list as it now is.
            self.refresh_people(cx);
        }
    }

    /// React to app-level state the view depends on.
    fn watch_ticks(&mut self, cx: &mut UiCtx) {
        if cx.shell.download_done_tick != self.seen_download_tick {
            self.seen_download_tick = cx.shell.download_done_tick;
            // A finished GPU-library install flips the GPU status at once —
            // the backend re-runs its preflight after extraction.
            self.refresh_models(cx);
            self.refresh_gpu(cx);
        }
        if cx.shell.settings_tick != self.seen_settings_tick {
            self.seen_settings_tick = cx.shell.settings_tick;
            self.needs_restart = true;
            self.refresh_diagnostics(cx);
        }
    }

    fn answer_confirms(&mut self, cx: &mut UiCtx) {
        if let Some(yes) = cx.confirm.answer(cache_confirm_id()) {
            if std::mem::take(&mut self.asked_cache) && yes {
                self.delete_cache(cx);
            }
        }
        if let Some(yes) = cx.confirm.answer(restart_confirm_id()) {
            if std::mem::take(&mut self.asked_restart) && yes {
                cx.cmds.push(UiCmd::Restart);
            }
        }
        if let Some((id, name)) = self.confirm_forget.clone() {
            match cx.confirm.answer(forget_confirm_id(id)) {
                Some(true) => {
                    self.confirm_forget = None;
                    self.forget(cx, id, name);
                }
                Some(false) => self.confirm_forget = None,
                None => {}
            }
        }
    }

    /// Re-sync a text buffer when the stored value moved away from what the
    /// buffer reflects while the field is not being edited — e.g. a failed
    /// save reverted it (settings.tsx left the stale text; fixed here).
    fn sync_buffers(&mut self, ctx: &egui::Context, settings: &Settings) {
        let focused = |id: egui::Id| ctx.memory(|m| m.has_focus(id));
        if settings.watch_patterns != self.patterns_source && !focused(patterns_id()) {
            self.patterns_text = settings.watch_patterns.join(", ");
            self.patterns_source = settings.watch_patterns.clone();
            self.patterns_dirty = false;
        }
        if settings.junk_phrases != self.junk_source && !focused(junk_id()) {
            self.junk_text = settings.junk_phrases.join("\n");
            self.junk_source = settings.junk_phrases.clone();
            self.junk_dirty = false;
        }
    }
}

/// `poll` a load slot; clears it once the result is in.
fn take<T>(slot: &mut Option<Task<T>>) -> Option<Result<T, String>> {
    let result = slot.as_mut()?.poll()?;
    *slot = None;
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_round_trip_their_slugs() {
        for page in SettingsPage::ALL {
            assert_eq!(SettingsPage::from_slug(page.slug()), Some(page));
            assert!(!page.title().is_empty());
        }
        assert_eq!(SettingsPage::from_slug(" AI "), Some(SettingsPage::Ai));
        assert_eq!(SettingsPage::from_slug("models"), None);
        assert_eq!(SettingsPage::default(), SettingsPage::General);
    }

    #[test]
    fn routes_open_pages_and_go_back_to_the_index() {
        let start = SettingsRoute::default();
        assert_eq!(start.page, SettingsPage::General);
        assert!(!start.open, "a narrow window starts on the index");
        let voices = SettingsRoute::opened(SettingsPage::Voices);
        assert!(voices.open);
        let back = voices.back();
        assert!(!back.open);
        assert_eq!(
            back.page,
            SettingsPage::Voices,
            "the rail keeps it selected"
        );
    }
}
