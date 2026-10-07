//! Settings: Startup, Updates, Storage, Recording, Transcription, Models,
//! Diagnostics and Voice prints sections (map-ui-settings §1, settings.tsx).
//!
//! - **Layout** (design/ui-rework.md): a sticky page title, then per section
//!   a header and one raised group of [`row`]s — title + muted description
//!   on the left, the control (switch, select, button, badge) on the right,
//!   stacking under the text when the row gets narrow. Long names and paths
//!   elide with a tooltip.
//! - **Saving** goes through the app-owned `SettingsStore` (`cx.settings`):
//!   the view renders from its optimistic copy, writes are serialised
//!   (≤1 in flight + 1 queued, latest wins) and survive leaving Settings,
//!   and a failed write reverts with "Could not save settings: …". Every
//!   edit starts from the store's *current* value, so two commits in one
//!   pass (a text field blurring because a checkbox was clicked) compose.
//! - **Loads** (models, cache size, GPU, GPU libraries, people, devices,
//!   autostart, diagnostics) run in parallel as view-owned `Task`s; leaving
//!   the view drops them. Download progress is global (`shell.download`),
//!   so badges survive leaving and returning; when a download finishes
//!   (`shell.download_done_tick`) the model, cache, GPU and GPU-library rows
//!   reload, and a saved data_dir change (`shell.settings_tick`) reloads
//!   diagnostics and shows the restart row.
//! - **Actions** whose outcome is reported globally (backup, delete model
//!   cache, forget voice, downloads, diagnostics export) use `spawn_op`, so
//!   their toasts appear even if the user navigates away; the view keeps
//!   the `OpHandle` for its local state ("Deleting…", reloads).
//! - Native dialogs block, so they only run inside those worker closures,
//!   parented through `cx.dialog_owner()` (which pins the tray flyout).

use super::updates::UpdatesSection;
use crate::bridge::{OpDone, OpHandle, Task};
use crate::platform::{autostart, dialogs};
use crate::ui::icons::ph;
use crate::ui::theme::{self, pal};
use crate::ui::toasts::Notice;
use crate::ui::widgets::{self, BadgeKind, KeyScroll, Politeness};
use crate::ui::{UiCmd, UiCtx};
use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText, Stroke};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use witness_core::audio_capture::AudioDevice;
use witness_core::backup::BackupSummary;
use witness_core::events::{ModelDownloadProgress, WatcherStatus};
use witness_core::models::ModelInfo;
use witness_core::service::{AudioDevices, Diagnostics, GpuLibsInfo, GpuStatus, PersonInfo};
use witness_core::settings::{Engine, Settings};
use witness_core::{AppCore, CoreResult};

/// The voice-match slider's UI range (the backend accepts 0–1).
const THRESHOLD_MIN: f32 = 0.40;
const THRESHOLD_MAX: f32 = 0.80;
const THRESHOLD_STEP: f32 = 0.05;
/// What the voice-match slider trades off.
const THRESHOLD_HELP: &str = "Lower = more auto-labels (more mistakes), higher = fewer.";
/// What the diagnostics report holds (and does not).
const DIAGNOSTICS_PRIVACY: &str = "This report contains operational state and local paths, but no audio, transcript text, notes, voice prints, or private settings.";
/// "Copied ✓" stays this long.
const COPIED_FLASH: Duration = Duration::from_millis(1500);
/// The backup folder picker's title (commands.rs `create_backup`).
const BACKUP_DIALOG_TITLE: &str = "Choose where to create the Witness backup";
/// The GPU-library download's progress id.
const GPU_LIBS_ID: &str = "gpu-libs";

/// A `spawn_op` action whose toast is reported globally but whose result
/// the view also wants (the backup summary, the new autostart state): the
/// op's completion flag plus the slot its worker fills on success (read
/// back here; dropped harmlessly if the view is gone).
struct OpRun<T> {
    handle: OpHandle,
    result: Arc<Mutex<Option<T>>>,
}

impl<T> OpRun<T> {
    /// Run `f` as an op; it gets the slot to fill and returns the `OpDone`.
    fn spawn(
        cx: &UiCtx,
        name: &'static str,
        f: impl FnOnce(&AppCore, &Mutex<Option<T>>) -> OpDone + Send + 'static,
    ) -> Self
    where
        T: Send + 'static,
    {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let handle = cx.tasks.spawn_op(name, move |c| f(c, &slot));
        OpRun { handle, result }
    }

    /// Once the op finished: `Some(result)` (`None` inside when it failed).
    fn finished(slot: &mut Option<Self>) -> Option<Option<T>> {
        let run = slot.take_if(|run| run.handle.is_done())?;
        Some(run.result.lock().ok().and_then(|mut r| r.take()))
    }
}

/// Fill an `OpRun` slot from its worker.
fn fill<T>(slot: &Mutex<Option<T>>, value: T) {
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(value);
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
    loads: Loads,
    /// First pass done (loads started, ticks recorded).
    mounted: bool,
    seen_download_tick: u64,
    seen_settings_tick: u64,

    models: Vec<ModelInfo>,
    cache_bytes: Option<u64>,
    gpu: Option<GpuStatus>,
    gpu_libs: Option<GpuLibsInfo>,
    people: Vec<PersonInfo>,
    devices: Option<AudioDevices>,
    devices_error: Option<String>,
    /// None until loaded (the switch is disabled meanwhile).
    autostart: Option<bool>,
    diagnostics: Option<Diagnostics>,
    diagnostics_error: Option<String>,
    diagnostics_copied_at: Option<Instant>,

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

    /// The scroller's content width last pass (the header matches it).
    content_width: Option<f32>,
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
            loads: Loads::default(),
            mounted: false,
            seen_download_tick: 0,
            seen_settings_tick: 0,
            models: Vec::new(),
            cache_bytes: None,
            gpu: None,
            gpu_libs: None,
            people: Vec::new(),
            devices: None,
            devices_error: None,
            autostart: None,
            diagnostics: None,
            diagnostics_error: None,
            diagnostics_copied_at: None,
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
            content_width: None,
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

        // The title (and its "saved ✓" flash) stays above the scroller, so
        // the flash shows wherever the edit was made. It is laid out as wide
        // as the scroller's content was (without its bar), so it lines up
        // with the sections.
        let width = self
            .content_width
            .unwrap_or(ui.available_width())
            .min(ui.available_width());
        ui.allocate_ui_with_layout(egui::vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(width);
            widgets::padded_column(ui, |ui| self.header(ui, cx));
        });
        let out = widgets::scroll_column(ui, "settings", KeyScroll { space: true }, |ui| {
            // The column helpers zero the horizontal spacing; rows of
            // buttons need theirs back.
            ui.spacing_mut().item_spacing.x = theme::S2;
            self.startup(ui, cx);
            self.updates.show(ui, cx);
            self.storage(ui, cx);
            self.recording(ui, cx);
            self.transcription(ui, cx);
            self.models_section(ui, cx);
            self.diagnostics_section(ui, cx);
            self.people_section(ui, cx);
        });
        if self.content_width != Some(out.inner_rect.width()) {
            self.content_width = Some(out.inner_rect.width());
            ui.ctx().request_repaint();
        }
        // A hairline under the title once the sections scroll beneath it.
        if out.state.offset.y > 0.5 {
            ui.painter().hline(
                ui.max_rect().x_range(),
                out.inner_rect.top(),
                Stroke::new(1.0, pal().border),
            );
        }
        #[cfg(debug_assertions)]
        self.debug_scroll(ui.ctx(), &out);
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

    fn commit_patterns(&mut self, cx: &mut UiCtx, patterns: Vec<String>) {
        self.patterns_dirty = false;
        self.patterns_source = patterns.clone();
        if patterns != cx.settings.current().watch_patterns {
            save(cx, |s| s.watch_patterns = patterns);
        }
    }

    fn commit_junk(&mut self, cx: &mut UiCtx, phrases: Vec<String>) {
        self.junk_dirty = false;
        self.junk_source = phrases.clone();
        if phrases != cx.settings.current().junk_phrases {
            save(cx, |s| s.junk_phrases = phrases);
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

    // ------------------------------------------------------------------
    // Header and sections. Each section is a header plus one group of
    // `row`s; a row's controls report what was clicked and the section
    // acts on it afterwards (the closures cannot borrow `self` mutably).

    fn header(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S3;
            ui.label(
                RichText::new("Settings")
                    .font(theme::semibold(20.0))
                    .color(pal().text),
            );
            if let Some(at) = cx.settings.saved_flash() {
                ui.label(RichText::new("saved ✓").size(12.5).color(pal().ok));
                widgets::announce(
                    ui.ctx(),
                    egui::Id::new("settings-saved-flash"),
                    "saved ✓",
                    Politeness::Polite,
                );
                let left = crate::settings_store::SAVED_FLASH.saturating_sub(at.elapsed());
                ui.ctx().request_repaint_after(left);
            }
        });
    }

    fn startup(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let mut enabled = self.autostart.unwrap_or(false);
        let loaded = self.autostart.is_some() && self.autostart_write.is_none();
        let changed = section(ui, "Startup", |ui| {
            toggle(
                ui,
                loaded,
                &mut enabled,
                "Start Witness with Windows",
                "Opens minimized to the tray.",
            )
            .changed()
        });
        if changed {
            // The switch shows the new value once the registry write
            // succeeds; a failure toasts even if Settings was left.
            self.autostart_write =
                Some(OpRun::spawn(
                    cx,
                    "autostart-write",
                    move |_, slot| match autostart::set_enabled(enabled) {
                        Ok(()) => {
                            fill(slot, enabled);
                            quiet()
                        }
                        Err(error) => {
                            notice_done(Notice::error("Could not update autostart", error))
                        }
                    },
                ));
        }
    }

    fn storage(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let data_dir = cx.settings.current().data_dir.clone();
        cx.fonts.note_texts(ui.ctx(), data_dir.as_deref());
        if let Some(backup) = &self.last_backup {
            cx.fonts.note_texts(ui.ctx(), [backup.path.as_str()]);
        }
        let running = self.backup.is_some();
        let restart_pending = self.needs_restart
            || self
                .diagnostics
                .as_ref()
                .is_some_and(|d| d.restart_required);
        let last_backup = self.last_backup.as_ref();

        let (change, backup, restart) = section(ui, "Storage", |ui| {
            let change = row(
                ui,
                "data-dir",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Data directory");
                    note(
                        ui,
                        "Holds the database, recordings and models. Changing it does not move existing data.",
                    );
                    path_box(ui, data_dir.as_deref().unwrap_or("(next to witness.exe)"));
                    id
                },
                |ui, _| widgets::btn(ui, "Change…").clicked(),
            );
            widgets::group_divider(ui);
            let backup = row(
                ui,
                "backup",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Backup");
                    note(
                        ui,
                        "Complete snapshot; recording and processing must be idle.",
                    );
                    if let Some(backup) = last_backup {
                        // "Backup saved to <path> (N files, X MB). Models …"
                        ui.add_space(theme::S1);
                        note(ui, "Backup saved to");
                        path_box(ui, &backup.path);
                        note(ui, &backup_details(backup));
                    }
                    id
                },
                |ui, _| {
                    let label = if running {
                        "Creating backup..."
                    } else {
                        "Create backup..."
                    };
                    button(ui, !running, label).clicked()
                },
            );
            let mut restart = false;
            if restart_pending {
                widgets::group_divider(ui);
                restart = row(
                    ui,
                    "restart",
                    Slot::Fit,
                    |ui| {
                        let id = title(ui, "Restart required");
                        note(ui, "The new data directory applies after a restart.");
                        id
                    },
                    |ui, _| widgets::btn_primary(ui, "Restart Witness").clicked(),
                );
            }
            (change, backup, restart)
        });

        if let Some(backup) = &self.last_backup {
            // `<p role="status">`: screen readers announce the finished backup.
            widgets::announce(
                ui.ctx(),
                egui::Id::new("settings-backup-status"),
                &format!("Backup saved to {} {}", backup.path, backup_details(backup)),
                Politeness::Polite,
            );
        }
        if change && self.loads.pick_dir.is_none() {
            let owner = cx.dialog_owner();
            self.loads.pick_dir = Some(cx.tasks.spawn("pick-data-dir", move |_| {
                dialogs::pick_folder_blocking(owner, None)
            }));
        }
        if backup {
            self.start_backup(cx);
        }
        if restart {
            self.restart(cx);
        }
    }

    fn recording(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let settings = cx.settings.current().clone();
        let watcher = watcher_text(cx.shell.status.as_ref().map(|s| &s.watcher));
        let record_hotkey = cx.shell.record_hotkey.clone();
        let bookmark_hotkey = cx.shell.bookmark_hotkey.clone();
        let mut auto_record = settings.auto_record;
        let failed = self.devices_error.is_some();
        let devices = self.devices.as_ref();
        let devices_error = self.devices_error.as_deref();
        let patterns_text = &mut self.patterns_text;

        let (auto, patterns, mic, loopback, refresh) = section(ui, "Recording", |ui| {
            let auto = toggle(
                ui,
                true,
                &mut auto_record,
                "Auto-record",
                "Start recording when a meeting is detected.",
            )
            .changed();
            widgets::group_divider(ui);
            let patterns = row(
                ui,
                "watch-patterns",
                Slot::Fill(FIELD_WIDTH),
                |ui| {
                    let id = title(ui, "Watch patterns");
                    note(ui, "Comma-separated, matched against mic-using app names.");
                    id
                },
                |ui, label| {
                    let field = widgets::text_input(
                        ui,
                        patterns_id(),
                        egui::TextEdit::singleline(patterns_text).desired_width(f32::INFINITY),
                    );
                    let response = ui.add(field).labelled_by(label);
                    widgets::text_edit_menu(&response);
                    response
                },
            );
            widgets::group_divider(ui);
            let mic = device_row(
                ui,
                &DeviceRow {
                    label: "Microphone",
                    id: "mic-device",
                    default_label: "Default input device",
                    missing_badge: "configured microphone is not connected",
                    help: None,
                    configured: settings.mic_device.as_deref(),
                    devices: devices.map(|d| d.capture.as_slice()),
                    list_failed: failed,
                },
            );
            widgets::group_divider(ui);
            let loopback = device_row(
                ui,
                &DeviceRow {
                    label: "Meeting audio",
                    id: "loopback-device",
                    default_label: "Default output device",
                    missing_badge: "configured output is not connected",
                    help: Some(
                        "The output Teams plays through — pick a dedicated one (e.g. \"Chat\") to keep music & game audio out of recordings.",
                    ),
                    configured: settings.loopback_device.as_deref(),
                    devices: devices.map(|d| d.render.as_slice()),
                    list_failed: failed,
                },
            );
            if let Some(error) = devices_error {
                widgets::inline_warning(ui, &format!("Audio devices could not be listed: {error}"));
            }
            widgets::group_divider(ui);
            let refresh = row(
                ui,
                "refresh-devices",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Audio devices");
                    note(ui, "Re-scan after plugging a device in.");
                    id
                },
                |ui, _| widgets::btn(ui, "Refresh devices").clicked(),
            );
            widgets::group_divider(ui);
            hotkey_row(
                ui,
                "Recording hotkey",
                "Starts/stops recording.",
                record_hotkey.as_deref(),
            );
            widgets::group_divider(ui);
            hotkey_row(
                ui,
                "Bookmark hotkey",
                "Bookmarks the current moment.",
                bookmark_hotkey.as_deref(),
            );
            (auto, patterns, mic, loopback, refresh)
        });
        footnote(ui, &watcher);

        if auto {
            save(cx, |s| s.auto_record = auto_record);
        }
        self.patterns_dirty |= patterns.changed();
        // Enter surrenders focus too, so this covers blur and Enter. A clean
        // buffer was already committed (or re-synced): skipping it keeps a
        // draft that `flush` committed — and the store reverted — from
        // failing a second time when the field finally blurs.
        if patterns.lost_focus() && self.patterns_dirty {
            self.commit_patterns(cx, parse_patterns(&self.patterns_text));
        }
        if let Some(value) = mic {
            save(cx, |s| s.mic_device = value);
        }
        if let Some(value) = loopback {
            save(cx, |s| s.loopback_device = value);
        }
        if refresh {
            self.refresh_devices(cx);
        }
    }

    fn transcription(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let settings = cx.settings.current().clone();
        let download_active = download_active(cx.shell.download.as_ref());
        let missing = self
            .models
            .iter()
            .any(|m| m.engine == settings.engine && !m.present);
        let gpu = self
            .gpu
            .as_ref()
            .map_or_else(|| "checking…".to_string(), |g| g.detail.clone());
        let mut auto_transcribe = settings.auto_transcribe;
        let mut live = settings.live_transcribe;
        let mut overlay = settings.caption_overlay;
        let mut engine = settings.engine;
        let local_threshold = &mut self.local_threshold;
        let junk_text = &mut self.junk_text;

        let out = section(ui, "Transcription", |ui| {
            let auto = toggle(
                ui,
                true,
                &mut auto_transcribe,
                "Auto-transcribe",
                "Runs after each recording.",
            )
            .changed();
            widgets::group_divider(ui);
            let live_changed = toggle(
                ui,
                true,
                &mut live,
                "Live captions",
                "Parakeet, while recording. Provisional — the final pass replaces them.",
            )
            .changed();
            widgets::group_divider(ui);
            // Disabled without live captions, but its value is kept.
            let overlay_changed = toggle(
                ui,
                settings.live_transcribe,
                &mut overlay,
                "Floating caption window",
                "On top of other apps while recording.",
            )
            .changed();
            widgets::group_divider(ui);
            let threshold = threshold_row(ui, local_threshold, settings.speaker_match_threshold);
            widgets::group_divider(ui);
            let junk = row(
                ui,
                "junk-phrases",
                Slot::Below,
                |ui| {
                    let id = title(ui, "Discarded noise phrases");
                    note(
                        ui,
                        "One exact phrase per line; clear the list to disable phrase filtering.",
                    );
                    id
                },
                |ui, label| {
                    let field = widgets::text_input(
                        ui,
                        junk_id(),
                        egui::TextEdit::multiline(junk_text)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                    let response = ui.add(field).labelled_by(label);
                    widgets::text_edit_menu(&response);
                    response
                },
            );
            widgets::group_divider(ui);
            row(
                ui,
                "engine",
                Slot::Fill(FIELD_WIDTH),
                |ui| title(ui, "Engine"),
                |ui, label| engine_picker(ui, label, &mut engine),
            );
            let download = missing
                && warning_strip(
                    ui,
                    "Model not downloaded — transcription will fail",
                    "Download",
                    !download_active,
                );
            widgets::group_divider(ui);
            row(
                ui,
                "gpu",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "GPU");
                    note(ui, &gpu);
                    id
                },
                |_, _| {},
            );
            (
                auto,
                live_changed,
                overlay_changed,
                threshold,
                junk,
                download,
            )
        });
        footnote(
            ui,
            "Speaker separation covers up to 8 remote speakers (NVIDIA Nemotron 3 Diarization).",
        );

        let (auto, live_changed, overlay_changed, threshold, junk, download) = out;
        if auto {
            save(cx, |s| s.auto_transcribe = auto_transcribe);
        }
        if live_changed {
            save(cx, |s| s.live_transcribe = live);
        }
        if overlay_changed {
            save(cx, |s| s.caption_overlay = overlay);
        }
        if let Some(threshold) = threshold {
            save(cx, |s| s.speaker_match_threshold = threshold);
        }
        self.junk_dirty |= junk.changed();
        // Blur only (Enter inserts a line); a clean buffer is skipped as for
        // the watch patterns.
        if junk.lost_focus() && self.junk_dirty {
            self.commit_junk(cx, parse_lines(&self.junk_text));
        }
        if engine != settings.engine {
            save(cx, |s| s.engine = engine);
        }
        if download {
            start_model_download(cx, settings.engine, "Could not download models".into());
        }
    }

    fn models_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let dl = cx.shell.download.clone();
        let active = download_active(dl.as_ref());
        let deleting = self.deleting_cache.is_some();
        let cache_bytes = self.cache_bytes;
        let models = &self.models;
        let gpu_libs = self.gpu_libs.as_ref().filter(|l| l.driver_present);

        let (model_action, libs_action, delete) = section(ui, "Models", |ui| {
            let mut model_action = None;
            for model in models {
                if let Some(engine) = model_row(ui, model, dl.as_ref(), active) {
                    model_action = Some((engine, model.display_name.clone()));
                }
                widgets::group_divider(ui);
            }
            let libs_action = gpu_libs.and_then(|libs| {
                let action = gpu_libs_row(ui, libs, dl.as_ref(), active);
                widgets::group_divider(ui);
                action
            });
            let delete = row(
                ui,
                "model-cache",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Model cache");
                    note(ui, &cache_text(cache_bytes));
                    id
                },
                |ui, _| {
                    let enabled = !active && !deleting && cache_bytes.is_some_and(|b| b > 0);
                    let label = if deleting {
                        "Deleting…"
                    } else {
                        "Delete model cache"
                    };
                    // The tooltip shows while disabled too, like the button's title.
                    let tip = "Deletes every downloaded model, including old versions — Shift-click to skip confirmation";
                    ui.add_enabled_ui(enabled, |ui| widgets::btn_danger(ui, label))
                        .inner
                        .on_hover_text(tip)
                        .on_disabled_hover_text(tip)
                        .clicked()
                },
            );
            (model_action, libs_action, delete)
        });
        footnote(
            ui,
            "Recording works without models — they're only needed for transcription.",
        );

        if let Some((engine, name)) = model_action {
            // A retry clears the failed badge first.
            cx.shell.download = None;
            start_model_download(cx, engine, format!("Could not download {name}"));
        }
        if libs_action.is_some() {
            cx.shell.download = None;
            start_gpu_libs_download(cx);
        }
        if delete {
            if ui.input(|i| i.modifiers.shift) {
                self.delete_cache(cx);
            } else {
                cx.confirm.ask(
                    cache_confirm_id(),
                    "Delete all downloaded models?\nTranscription stops working until you download them again.",
                    "Delete models",
                );
                self.asked_cache = true;
            }
        }
    }

    fn diagnostics_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let Some(d) = self.diagnostics.clone() else {
            let error = self.diagnostics_error.clone();
            section(ui, "Diagnostics", |ui| match &error {
                Some(error) => {
                    widgets::inline_warning(ui, &format!("Diagnostics unavailable: {error}"));
                }
                None => note(ui, "Checking…"),
            });
            footnote(ui, DIAGNOSTICS_PRIVACY);
            return;
        };
        // User-profile paths may hold any script: load its fallback font.
        cx.fonts.note_texts(
            ui.ctx(),
            [
                d.runtime_data_dir.as_str(),
                d.database.path.as_str(),
                d.log_path.as_str(),
            ],
        );
        let copied = self
            .diagnostics_copied_at
            .is_some_and(|at| at.elapsed() < COPIED_FLASH);
        let error = self.diagnostics_error.clone();

        let (refresh, copy, export) = section(ui, "Diagnostics", |ui| {
            if let Some(error) = &error {
                widgets::inline_warning(ui, &format!("Diagnostics unavailable: {error}"));
            }
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = theme::S2;
                kv_row(ui, "Build", |ui| {
                    value(ui, &format!("Witness {} · {}", d.app_version, d.platform));
                });
                kv_row(ui, "Runtime data", |ui| {
                    let pending = d.restart_required.then_some("restart pending");
                    path_value(
                        ui,
                        &d.runtime_data_dir,
                        pending.map(|t| (BadgeKind::Busy, t)),
                    );
                });
                kv_row(ui, "Database", |ui| {
                    let badge = if d.database.healthy {
                        (BadgeKind::Ok, "healthy")
                    } else {
                        (BadgeKind::Failed, "check failed")
                    };
                    if let Some(r) = path_value(ui, &d.database.path, Some(badge)) {
                        r.on_hover_text(&d.database.detail);
                    }
                });
                kv_row(ui, "Log", |ui| {
                    path_value(ui, &d.log_path, None);
                });
                for (label, text) in [
                    ("Recording", recording_text(&d)),
                    ("Processing", processing_text(&d)),
                    ("Capture health", capture_health_text(&d)),
                    ("Model integrity", model_integrity_text(&d)),
                    ("GPU runtime", gpu_runtime_text(&d)),
                ] {
                    kv_row(ui, label, |ui| value(ui, &text));
                }
            });
            widgets::group_divider(ui);
            row(
                ui,
                "diagnostics-actions",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Report");
                    note(
                        ui,
                        &format!("generated {}", fmt_generated_at(&d.generated_at)),
                    );
                    id
                },
                |ui, _| {
                    let refresh = widgets::btn(ui, "Refresh").clicked();
                    let label = if copied { "Copied ✓" } else { "Copy report" };
                    let copy = widgets::btn(ui, label).clicked();
                    let export = widgets::btn(ui, "Export…").clicked();
                    (refresh, copy, export)
                },
            )
        });
        footnote(ui, DIAGNOSTICS_PRIVACY);

        if let Some(at) = self.diagnostics_copied_at {
            match COPIED_FLASH.checked_sub(at.elapsed()) {
                Some(left) => ui.ctx().request_repaint_after(left),
                None => self.diagnostics_copied_at = None,
            }
        }
        if refresh {
            self.refresh_diagnostics(cx);
        }
        if copy {
            ui.ctx().copy_text(d.report.clone());
            self.diagnostics_copied_at = Some(Instant::now());
            cx.toasts.success("Diagnostic report copied");
        }
        if export {
            export_diagnostics(cx);
        }
    }

    fn people_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let people = &self.people;
        let forget = section(ui, "Voice prints", |ui| {
            if people.is_empty() {
                note(
                    ui,
                    "No one enrolled yet. Rename a speaker in any transcript to teach Witness their voice — future meetings will label them automatically.",
                );
                return None;
            }
            let mut forget = None;
            for (i, person) in people.iter().enumerate() {
                if i > 0 {
                    widgets::group_divider(ui);
                }
                let clicked = row(
                    ui,
                    ("person", person.id),
                    Slot::Fit,
                    |ui| {
                        let id = title(ui, &person.name);
                        note(ui, &speech_learned(person.sample_seconds));
                        id
                    },
                    |ui, _| {
                        widgets::icon_btn_danger(
                            ui,
                            ph::TRASH,
                            15.0,
                            "Forget this voice — Shift-click to skip confirmation",
                        )
                        .clicked()
                    },
                );
                if clicked {
                    let shift = ui.input(|i| i.modifiers.shift);
                    forget = Some((person.id, person.name.clone(), shift));
                }
            }
            forget
        });
        if !self.people.is_empty() {
            footnote(
                ui,
                "Speakers matched by voice show a ≈ before their name; renaming them confirms or corrects the match.",
            );
        }
        if let Some((id, name, shift)) = forget {
            if shift {
                self.forget(cx, id, name);
            } else {
                cx.confirm.ask(
                    forget_confirm_id(id),
                    format!("Forget {name}'s voice?\nExisting transcripts keep their names."),
                    "Forget voice",
                );
                self.confirm_forget = Some((id, name));
            }
        }
    }

    // ------------------------------------------------------------------
    // Actions

    fn start_backup(&mut self, cx: &mut UiCtx) {
        let owner = cx.dialog_owner();
        self.backup = Some(OpRun::spawn(cx, "create-backup", move |c, slot| {
            // Cancel returns quietly, like the command's `None`.
            let Some(parent) = dialogs::pick_folder_blocking(owner, Some(BACKUP_DIALOG_TITLE))
            else {
                return quiet();
            };
            let notice = match c.create_backup(&parent) {
                Ok(backup) => {
                    fill(slot, backup);
                    Notice::success("Backup created")
                }
                Err(error) => Notice::error("Could not create backup", error),
            };
            notice_done(notice)
        }));
    }

    fn delete_cache(&mut self, cx: &mut UiCtx) {
        if self.deleting_cache.is_some() {
            return;
        }
        let handle = cx.tasks.spawn_op("delete-model-cache", |c| {
            notice_done(match c.delete_model_cache() {
                Ok(freed) => Notice::success(format!(
                    "Deleted model cache — freed {:.2} GB",
                    freed as f64 / 1e9
                )),
                Err(error) => Notice::error("Could not delete the model cache", error),
            })
        });
        self.deleting_cache = Some(handle);
    }

    fn forget(&mut self, cx: &mut UiCtx, person_id: i64, name: String) {
        let handle = cx
            .tasks
            .spawn_op("forget-voice", move |c| match c.delete_person(person_id) {
                Ok(()) => quiet(),
                Err(error) => {
                    notice_done(Notice::error(&format!("Could not forget {name}"), error))
                }
            });
        // Hide the row now; the reload after the op restores it on failure.
        self.people.retain(|p| p.id != person_id);
        self.forgetting.push(handle);
    }

    /// Restart Witness after a data_dir change. The decision reads the
    /// cached status; a running recording needs an explicit confirmation
    /// (Shift does not skip it). `logic()` runs the restart (§4.5).
    fn restart(&mut self, cx: &mut UiCtx) {
        match cx.shell.status.as_ref() {
            None => cx.toasts.error(
                "Could not check recording status; restart cancelled",
                "status not loaded yet",
            ),
            Some(status) if status.recording => {
                cx.confirm.ask(
                    restart_confirm_id(),
                    "Restarting will stop and save the current recording. Restart now?",
                    "Stop & restart",
                );
                self.asked_restart = true;
            }
            Some(_) => cx.cmds.push(UiCmd::Restart),
        }
    }
}

// ----------------------------------------------------------------------
// Action helpers

/// Save `edit` applied to the store's current (optimistic) settings.
fn save(cx: &mut UiCtx, edit: impl FnOnce(&mut Settings)) {
    let mut next = cx.settings.current().clone();
    edit(&mut next);
    cx.settings.save(next, cx.tasks);
}

fn quiet() -> OpDone {
    OpDone::Notice {
        notice: None,
        refresh: false,
    }
}

fn notice_done(notice: Notice) -> OpDone {
    OpDone::Notice {
        notice: Some(notice),
        refresh: false,
    }
}

/// `download_models(engine)` (returns at once; progress is global).
fn start_model_download(cx: &mut UiCtx, engine: Engine, failure: String) {
    cx.tasks.spawn_op("download-models", move |c| {
        match c.download_models(engine) {
            Ok(()) => quiet(),
            Err(error) => notice_done(Notice::error(&failure, error)),
        }
    });
}

fn start_gpu_libs_download(cx: &mut UiCtx) {
    cx.tasks
        .spawn_op("download-gpu-libs", |c| match c.download_gpu_libs() {
            Ok(()) => quiet(),
            Err(error) => notice_done(Notice::error("Could not download GPU libraries", error)),
        });
}

/// Save dialog → `export_diagnostics`; a cancel is silent.
fn export_diagnostics(cx: &mut UiCtx) {
    let owner = cx.dialog_owner();
    cx.tasks.spawn_op("export-diagnostics", move |c| {
        let request = dialogs::SaveFileRequest {
            file_name: AppCore::diagnostics_file_name(),
            filter_name: "Text report".into(),
            extensions: vec!["txt".into()],
        };
        let Some(dest) = dialogs::save_file_blocking(owner, request) else {
            return quiet();
        };
        notice_done(match c.export_diagnostics(&dest) {
            Ok(()) => Notice::success("Diagnostic report exported"),
            Err(error) => Notice::error("Could not export diagnostics", error),
        })
    });
}

/// `poll` a load slot; clears it once the result is in.
fn take<T>(slot: &mut Option<Task<T>>) -> Option<Result<T, String>> {
    let result = slot.as_mut()?.poll()?;
    *slot = None;
    Some(result)
}

fn patterns_id() -> egui::Id {
    egui::Id::new("settings-watch-patterns")
}

fn junk_id() -> egui::Id {
    egui::Id::new("settings-junk-phrases")
}

fn cache_confirm_id() -> egui::Id {
    egui::Id::new("settings-delete-model-cache")
}

fn restart_confirm_id() -> egui::Id {
    egui::Id::new("settings-restart")
}

fn forget_confirm_id(person_id: i64) -> egui::Id {
    egui::Id::new(("settings-forget-voice", person_id))
}

/// One audio-device picker row (settings.tsx's two `<select>`s).
struct DeviceRow<'a> {
    label: &'static str,
    id: &'static str,
    default_label: &'static str,
    missing_badge: &'static str,
    help: Option<&'static str>,
    configured: Option<&'a str>,
    /// None while loading or after listing failed.
    devices: Option<&'a [AudioDevice]>,
    list_failed: bool,
}

/// A device picker row; returns the newly picked value (None inside =
/// the default device).
fn device_row(ui: &mut egui::Ui, spec: &DeviceRow) -> Option<Option<String>> {
    let options = device_options(
        spec.configured,
        spec.devices,
        spec.list_failed,
        spec.default_label,
    );
    let kind = configured_device(spec.configured, spec.devices.unwrap_or(&[]));
    let selected = spec.configured.unwrap_or("");
    let picked = row(
        ui,
        spec.id,
        Slot::Fill(FIELD_WIDTH),
        |ui| {
            let id = title(ui, spec.label);
            if let Some(help) = spec.help {
                note(ui, help);
            }
            if spec.devices.is_some() {
                match kind {
                    Some(ConfiguredDevice::Missing) => {
                        ui.add_space(theme::S1);
                        badge_or_text(ui, BadgeKind::Failed, spec.missing_badge);
                    }
                    Some(ConfiguredDevice::Legacy(_)) => note(
                        ui,
                        "Stored by its old friendly name; reselect it to use the stable ID.",
                    ),
                    _ => {}
                }
            }
            id
        },
        |ui, label| device_picker(ui, spec.id, label, &options, selected),
    );
    picked.map(|value| (!value.is_empty()).then_some(value))
}

/// The voice-match slider row: the value readout, then the slider. Returns
/// the threshold to save once an edit is finished.
fn threshold_row(ui: &mut egui::Ui, local: &mut Option<f32>, stored: f32) -> Option<f32> {
    row(
        ui,
        "threshold",
        Slot::Fill(FIELD_WIDTH),
        |ui| {
            let id = title(ui, "Voice match strictness");
            note(ui, THRESHOLD_HELP);
            id
        },
        |ui, _| {
            let shown_before = local.unwrap_or(stored);
            let mut value = shown_before;
            let enabled = ui.is_enabled();
            // The readout's place; painted after the slider moved.
            let (readout, _) = ui.allocate_exact_size(
                egui::vec2(34.0, ui.spacing().interact_size.y),
                egui::Sense::hover(),
            );
            let width = ui.available_width().clamp(60.0, 360.0);
            let response = widgets::slider(
                ui,
                &mut value,
                THRESHOLD_MIN..=THRESHOLD_MAX,
                THRESHOLD_STEP as f64,
                Some(width),
            );
            let shown = value;
            response.widget_info(|| {
                egui::WidgetInfo::slider(
                    enabled,
                    shown as f64,
                    format!("Voice match strictness: {}", threshold_value_text(shown)),
                )
            });
            let interacted = response.is_pointer_button_down_on()
                || response.dragged()
                || response.drag_stopped()
                || response.clicked()
                || (response.has_focus()
                    && ui.input(|i| {
                        i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowRight)
                    }));
            if let Some(edit) = threshold_edit(shown_before, value, response.changed(), interacted)
            {
                *local = Some(edit);
            }
            // Commit once the pointer lets go (a keyboard change commits at
            // once), like the range input's `onChange`.
            let mut commit = None;
            if let Some(value) = *local {
                if !response.dragged() && !response.is_pointer_button_down_on() {
                    *local = None;
                    commit = (value != stored).then_some(value);
                }
            }
            ui.painter().text(
                egui::pos2(readout.left(), response.rect.center().y),
                egui::Align2::LEFT_CENTER,
                threshold_readout(local.or(commit).unwrap_or(stored)),
                theme::regular(theme::BASE_SIZE),
                pal().text,
            );
            commit
        },
    )
}

/// The engine `<select>`.
fn engine_picker(ui: &mut egui::Ui, label: egui::Id, engine: &mut Engine) {
    let width = ui.available_width();
    widgets::select(ui, widgets::SelectKind::TextInput, |ui| {
        egui::ComboBox::from_id_salt("settings-engine")
            .width(width)
            .selected_text(engine_label(*engine))
            .truncate()
            .show_ui(ui, |ui| {
                for option in [Engine::Parakeet, Engine::Whisper] {
                    ui.selectable_value(engine, option, engine_label(option));
                }
            })
            .response
            .labelled_by(label);
    });
}

fn engine_label(engine: Engine) -> &'static str {
    match engine {
        Engine::Parakeet => "Parakeet TDT v3 (fast, GPU)",
        Engine::Whisper => "Whisper large-v3-turbo",
    }
}

/// A model's row; returns the engine to download when its Download /
/// Repair / Retry button was clicked.
fn model_row(
    ui: &mut egui::Ui,
    model: &ModelInfo,
    dl: Option<&ModelDownloadProgress>,
    active: bool,
) -> Option<Engine> {
    let state = model_state(model, dl);
    let fraction = dl
        .filter(|d| d.model_id == model.id)
        .and_then(download_fraction);
    let clicked = row(
        ui,
        ("model", &model.id),
        Slot::Fit,
        |ui| {
            let id = title(ui, &model.display_name);
            let short: String = model.expected_revision.chars().take(8).collect();
            ui.label(
                RichText::new(format!("rev {short}"))
                    .font(egui::FontId::monospace(11.5))
                    .color(pal().text_muted),
            )
            .on_hover_text(format!("Expected revision {}", model.expected_revision));
            match &state {
                ModelState::Failed(error) => {
                    status_note(ui, theme::pal().rec_light, &format!("failed: {error}"));
                }
                ModelState::Progress(text) => {
                    status_note(ui, theme::pal().warn, text);
                    if let Some(f) = fraction {
                        progress_bar(ui, f);
                    }
                }
                ModelState::Idle {
                    integrity_error: Some(error),
                } => {
                    status_note(ui, theme::pal().rec_light, "integrity check failed")
                        .on_hover_text(error);
                }
                _ => {}
            }
            id
        },
        |ui, _| match &state {
            ModelState::Installed { text, tooltip } => {
                widgets::badge(ui, BadgeKind::Ok, text).on_hover_text(tooltip);
                false
            }
            ModelState::Failed(_) => widgets::btn(ui, "Retry").clicked(),
            ModelState::Progress(_) => {
                widgets::badge(ui, BadgeKind::Busy, "downloading");
                false
            }
            ModelState::Waiting => {
                widgets::badge(ui, BadgeKind::Busy, "waiting…");
                false
            }
            ModelState::Idle { integrity_error } => {
                let label = if integrity_error.is_some() {
                    "Repair"
                } else {
                    "Download"
                };
                button(ui, !active, label).clicked()
            }
        },
    );
    clicked.then_some(model.engine)
}

/// The GPU-library row; `Some(())` when its download / repair / retry
/// button was clicked.
fn gpu_libs_row(
    ui: &mut egui::Ui,
    libs: &GpuLibsInfo,
    dl: Option<&ModelDownloadProgress>,
    active: bool,
) -> Option<()> {
    let state = gpu_libs_state(libs, dl);
    let fraction = dl
        .filter(|d| d.model_id == GPU_LIBS_ID)
        .and_then(download_fraction);
    let clicked = row(
        ui,
        "gpu-libs",
        Slot::Fit,
        |ui| {
            let id = title(ui, "CUDA runtime + cuDNN (GPU)");
            match &state {
                GpuLibsState::Failed(error) => {
                    status_note(ui, theme::pal().rec_light, &format!("failed: {error}"));
                }
                GpuLibsState::Progress(text) => {
                    status_note(ui, theme::pal().warn, text);
                    if let Some(f) = fraction {
                        progress_bar(ui, f);
                    }
                }
                GpuLibsState::System => note(ui, "using system CUDA libraries"),
                GpuLibsState::Idle {
                    integrity_error: Some(error),
                    ..
                } => {
                    status_note(ui, theme::pal().rec_light, "integrity check failed")
                        .on_hover_text(error);
                }
                _ => {}
            }
            id
        },
        |ui, _| match &state {
            GpuLibsState::Installed(text) => {
                widgets::badge(ui, BadgeKind::Ok, text);
                false
            }
            GpuLibsState::Failed(_) => widgets::btn(ui, "Retry").clicked(),
            GpuLibsState::Progress(_) => {
                widgets::badge(ui, BadgeKind::Busy, "downloading");
                false
            }
            GpuLibsState::System => false,
            GpuLibsState::Idle { label, .. } => button(ui, !active, label).clicked(),
        },
    );
    clicked.then_some(())
}

/// A global-hotkey row: the bound combo as a key cap, or "Unavailable".
fn hotkey_row(ui: &mut egui::Ui, name: &str, what: &str, combo: Option<&str>) {
    let (description, key) = hotkey_parts(what, combo);
    row(
        ui,
        name,
        Slot::Fit,
        |ui| {
            let id = title(ui, name);
            note(ui, &description);
            id
        },
        |ui, _| match &key {
            Some(key) => keycap(ui, key),
            None => {
                ui.label(RichText::new("Unavailable").color(pal().text_muted));
            }
        },
    );
}

// ----------------------------------------------------------------------
// Layout helpers (shared with updates.rs)

/// Below this a row's text column would be squeezed: the control stacks
/// under the text instead.
const TEXT_MIN_WIDTH: f32 = 200.0;
/// Room for a select / text field / slider beside its text.
const FIELD_WIDTH: f32 = 260.0;
/// The gap between a row's text and its control.
const ROW_GAP: f32 = theme::S4;
/// Diagnostics key column; under this much row width keys stack.
const KEY_WIDTH: f32 = 128.0;
const KV_SIDE_BY_SIDE: f32 = 400.0;

/// A settings section: its header and one group holding `add`'s rows.
pub(super) fn section<R>(
    ui: &mut egui::Ui,
    title: &str,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    widgets::section_header(ui, title);
    widgets::group(ui, add)
}

/// How a [`row`]'s control is sized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Slot {
    /// As wide as its content (switches, buttons, badges): measured, so the
    /// control stays beside the text whenever the text keeps
    /// [`TEXT_MIN_WIDTH`].
    Fit,
    /// A field that fills the given width beside the text, or the whole
    /// row once stacked (selects, text inputs, sliders).
    Fill(f32),
    /// Always under the text, full width (multi-line inputs).
    Below,
}

/// What a [`row`] measured last pass, to centre and right-align its parts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct RowMetrics {
    text_height: f32,
    /// The title's one-line width: a control never squeezes it into `…`.
    title_width: f32,
    /// The control's natural (one-line) width.
    control_width: f32,
}

/// A settings row (the kit's `setting_row`, with what Settings needs on
/// top): `text` draws the title + description column and returns the
/// title's id, `control` gets that id (for `labelled_by`) and is placed on
/// the right in a control-high band, vertically centred on the text. When
/// the text would get narrower than [`TEXT_MIN_WIDTH`] the control stacks
/// under it, so a switch stays beside its label at 380 pt while a select
/// stacks; stacked controls wider than the row wrap.
///
/// Immediate mode cannot centre or right-align before it knows the sizes,
/// so the row remembers last pass's text height and control width under
/// `salt` and discards the pass when they change (no visible jump).
pub(super) fn row<R>(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    slot: Slot,
    text: impl FnOnce(&mut egui::Ui) -> egui::Id,
    control: impl FnOnce(&mut egui::Ui, egui::Id) -> R,
) -> R {
    let id = egui::Id::new(("settings-row", salt));
    let last: Option<RowMetrics> = ui.data(|d| d.get_temp(id));
    let full = ui.available_width();
    let band = ui.spacing().interact_size.y;
    let control_width = match slot {
        Slot::Fit => last.map(|m| m.control_width),
        Slot::Fill(width) => Some(width),
        Slot::Below => None,
    };
    let title_width = last.map_or(0.0, |m| m.title_width);
    let beside = control_width.filter(|w| {
        let text = full - w - ROW_GAP;
        text >= TEXT_MIN_WIDTH && text >= title_width
    });
    let text_block = |ui: &mut egui::Ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        text(ui)
    };

    let (out, now) = match beside {
        Some(width) => {
            let text_height = last.map_or(0.0, |m| m.text_height);
            let height = text_height.max(band);
            let origin = ui.cursor().min;
            let text_width = full - width - ROW_GAP;
            let mut text_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt((id, "text"))
                    .max_rect(egui::Rect::from_min_size(
                        origin + egui::vec2(0.0, (height - text_height) / 2.0),
                        egui::vec2(text_width, f32::INFINITY),
                    ))
                    .layout(Layout::top_down(Align::Min)),
            );
            let label = text_block(&mut text_ui);
            let mut control_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt((id, "control"))
                    .max_rect(egui::Rect::from_min_size(
                        origin + egui::vec2(full - width, (height - band) / 2.0),
                        egui::vec2(width, band),
                    ))
                    .layout(Layout::left_to_right(Align::Center)),
            );
            let out = control(&mut control_ui, label);
            let (text_rect, control_rect) = (text_ui.min_rect(), control_ui.min_rect());
            let bottom = text_rect.bottom().max(control_rect.bottom());
            ui.advance_cursor_after_rect(egui::Rect::from_min_max(
                origin,
                egui::pos2(origin.x + full, bottom),
            ));
            let now = RowMetrics {
                text_height: text_rect.height(),
                title_width: natural_title_width(ui, label),
                control_width: match slot {
                    Slot::Fit => control_rect.width(),
                    _ => width,
                },
            };
            (out, now)
        }
        None => {
            ui.scope_builder(egui::UiBuilder::new().id_salt((id, "stacked")), |ui| {
                let label = ui.scope(text_block).inner;
                widgets::gap(ui, theme::S2);
                // Wrap (several buttons) only once the controls are known
                // to be wider than the row; otherwise lay them out on one
                // line, so what is measured is their natural width. Fields
                // never wrap: they size to the row (in a wrapping layout
                // the available width would be the whole row again).
                let wrap = slot == Slot::Fit && last.is_some_and(|m| m.control_width > full);
                let layout = Layout::left_to_right(Align::Center).with_main_wrap(wrap);
                let inner = ui.allocate_ui_with_layout(egui::vec2(full, band), layout, |ui| {
                    control(ui, label)
                });
                let now = RowMetrics {
                    // Kept for when the row goes back beside.
                    text_height: last.map_or(0.0, |m| m.text_height),
                    title_width: natural_title_width(ui, label),
                    control_width: match (slot, last) {
                        (Slot::Fit, Some(m)) if wrap => m.control_width,
                        (Slot::Fit, _) => inner.response.rect.width(),
                        // Only a `Fit` control's width is ever used.
                        _ => 0.0,
                    },
                };
                (inner.inner, now)
            })
            .inner
        }
    };

    let changed = last.is_none_or(|m| {
        (m.text_height - now.text_height).abs() > 0.5
            || (m.title_width - now.title_width).abs() > 0.5
            || (m.control_width - now.control_width).abs() > 0.5
    });
    if changed {
        ui.data_mut(|d| d.insert_temp(id, now));
        // Re-run the pass with the new sizes before anything is shown (and
        // repaint, should this pass already be the last one allowed).
        ui.ctx().request_discard("settings row measured");
        ui.ctx().request_repaint();
    }
    out
}

/// A boolean setting: title + muted description, the switch on the right.
pub(super) fn toggle(
    ui: &mut egui::Ui,
    enabled: bool,
    on: &mut bool,
    name: &str,
    description: &str,
) -> egui::Response {
    row(
        ui,
        name,
        Slot::Fit,
        |ui| {
            if !enabled {
                ui.disable();
            }
            let id = title(ui, name);
            note(ui, description);
            id
        },
        |ui, _| widgets::switch(ui, enabled, on, name),
    )
}

/// A row title: body text, one line, elided with a tooltip when long. Its
/// one-line width is noted for [`row`].
pub(super) fn title(ui: &mut egui::Ui, text: &str) -> egui::Id {
    let font = theme::regular(theme::BASE_SIZE);
    let natural = text_width(ui, text, font.clone());
    let id = widgets::elide(ui, text, font, pal().text).id;
    ui.data_mut(|d| d.insert_temp(id.with("natural-width"), natural));
    id
}

/// What [`title`] noted for the title `id` (0 for other row texts).
fn natural_title_width(ui: &egui::Ui, id: egui::Id) -> f32 {
    ui.data(|d| d.get_temp(id.with("natural-width")))
        .unwrap_or(0.0)
}

/// A row description: muted 12.5 pt, wrapping.
pub(super) fn note(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::regular(12.5))
                .color(pal().text_muted),
        )
        .wrap(),
    );
}

/// A coloured status line under a row title (download progress, errors).
fn status_note(ui: &mut egui::Ui, color: egui::Color32, text: &str) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(text).font(theme::regular(12.5)).color(color)).wrap())
}

/// Help text under a group, aligned with its section header.
pub(super) fn footnote(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::regular(12.5))
                .color(pal().text_muted),
        )
        .wrap(),
    );
}

/// A thin accent progress bar across the available width (`fraction`
/// 0..1).
pub(super) fn progress_bar(ui: &mut egui::Ui, fraction: f32) {
    ui.add_space(theme::S1);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
    let radius = CornerRadius::same(2);
    ui.painter()
        .rect_filled(rect, radius, theme::pal().border_strong);
    let mut done = rect;
    done.set_width(rect.width() * fraction.clamp(0.0, 1.0));
    if done.width() > 0.0 {
        ui.painter().rect_filled(done, radius, theme::pal().accent);
    }
}

/// A secondary button, disabled unless `enabled`.
pub(super) fn button(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| widgets::btn(ui, text))
        .inner
}

/// A path in a small inset box: monospace, one line, elided (the tooltip
/// has the whole path).
fn path_box(ui: &mut egui::Ui, text: &str) {
    ui.add_space(theme::S1);
    egui::Frame::new()
        .fill(pal().bg)
        .corner_radius(CornerRadius::same(theme::R_SM))
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            widgets::elide(ui, text, egui::FontId::monospace(12.0), pal().text);
        });
}

/// A key cap for a hotkey combo.
fn keycap(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(pal().bg)
        .stroke(Stroke::new(1.0, theme::pal().border_strong))
        .corner_radius(CornerRadius::same(theme::R_SM))
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .font(egui::FontId::monospace(12.0))
                    .color(pal().text),
            );
        });
}

/// A warning line inside a group (warn tint) with one action button on the
/// right; true when it was clicked.
fn warning_strip(ui: &mut egui::Ui, text: &str, action: &str, enabled: bool) -> bool {
    egui::Frame::new()
        .fill(theme::pal().warn_bg)
        .stroke(Stroke::new(1.0, theme::pal().warn_border))
        .corner_radius(CornerRadius::same(theme::R_MD))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            row(
                ui,
                ("warning", text),
                Slot::Fit,
                |ui| {
                    ui.add(egui::Label::new(RichText::new(text).color(theme::pal().warn)).wrap())
                        .id
                },
                |ui, _| button(ui, enabled, action).clicked(),
            )
        })
        .inner
}

/// The uppercase badge when it fits the width left, else its text in the
/// badge's colour, wrapping (a long "not connected" in a narrow column).
fn badge_or_text(ui: &mut egui::Ui, kind: BadgeKind, text: &str) {
    let upper = text.to_uppercase();
    let width =
        text_width(ui, &upper, theme::regular(11.0)) + 0.4 * upper.chars().count() as f32 + 16.0;
    if width <= ui.available_width() {
        widgets::badge(ui, kind, text);
    } else {
        let color = match kind {
            BadgeKind::Ok => theme::pal().ok_light,
            BadgeKind::Busy => theme::pal().warn,
            BadgeKind::Failed => theme::pal().rec_light,
        };
        status_note(ui, color, text);
    }
}

/// Single-line width of `text` in `font`.
fn text_width(ui: &egui::Ui, text: &str, font: egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, pal().text)
        .size()
        .x
}

/// A diagnostics key/value line: the key in a muted column beside the
/// value, or above it when the group is narrow.
fn kv_row(ui: &mut egui::Ui, key: &str, value: impl FnOnce(&mut egui::Ui)) {
    let full = ui.available_width();
    let key_label = |ui: &mut egui::Ui| {
        ui.add(egui::Label::new(RichText::new(key).color(pal().text_muted)).wrap());
    };
    if full >= KV_SIDE_BY_SIDE {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S3;
            ui.allocate_ui_with_layout(
                egui::vec2(KEY_WIDTH, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(KEY_WIDTH);
                    key_label(ui);
                },
            );
            let width = ui.available_width();
            ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(width);
                    value(ui);
                },
            );
        });
    } else {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(
                egui::Label::new(
                    RichText::new(key)
                        .font(theme::regular(12.5))
                        .color(pal().text_muted),
                )
                .wrap(),
            );
            value(ui);
        });
    }
}

/// A diagnostics value: body text, wrapping.
fn value(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(pal().text)).wrap());
}

/// A diagnostics path (monospace, elided) with an optional trailing badge;
/// returns the badge's response.
fn path_value(
    ui: &mut egui::Ui,
    path: &str,
    badge: Option<(BadgeKind, &str)>,
) -> Option<egui::Response> {
    // 12 pt monospace is a little shorter than the body-text key beside it.
    ui.add_space(2.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = theme::S2;
        let badge_width = badge.map_or(0.0, |(_, text)| {
            let upper = text.to_uppercase();
            text_width(ui, &upper, theme::regular(11.0))
                + 0.4 * upper.chars().count() as f32
                + 16.0
                + theme::S2
        });
        let path_width = (ui.available_width() - badge_width).max(40.0);
        ui.allocate_ui_with_layout(
            egui::vec2(path_width, 0.0),
            Layout::left_to_right(Align::Min),
            |ui| {
                widgets::elide(ui, path, egui::FontId::monospace(12.0), pal().text);
            },
        );
        badge.map(|(kind, text)| widgets::badge(ui, kind, text))
    })
    .inner
}

/// A device `<select>` named by the row title `label_id`, as wide as the
/// slot; returns the newly picked value ("" = default).
fn device_picker(
    ui: &mut egui::Ui,
    id: &str,
    label_id: egui::Id,
    options: &[DeviceOption],
    selected: &str,
) -> Option<String> {
    let label = options
        .iter()
        .find(|o| o.value == selected)
        .map_or(selected, |o| o.label.as_str());
    let width = ui.available_width();
    // The button plus its arrow need ~40 pt besides the text.
    let truncated = text_width(ui, label, theme::regular(theme::BASE_SIZE)) + 40.0 > width;
    let mut picked = None;
    widgets::select(ui, widgets::SelectKind::TextInput, |ui| {
        let response = egui::ComboBox::from_id_salt(id)
            .width(width)
            .selected_text(label)
            .truncate()
            .show_ui(ui, |ui| {
                for option in options {
                    let current = option.value == selected;
                    if ui
                        .selectable_label(current, option.label.as_str())
                        .clicked()
                        && !current
                    {
                        picked = Some(option.value.clone());
                    }
                }
            })
            .response
            .labelled_by(label_id);
        if truncated {
            response.on_hover_text(label);
        }
    });
    picked
}

// ----------------------------------------------------------------------
// Pure helpers (unit-tested)

/// Watch patterns: split on `,`, trim, drop empties.
fn parse_patterns(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

/// Junk phrases: one per line, trimmed, empties dropped.
fn parse_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

/// `ctrl+alt+r` → `CTRL + ALT + R`.
fn hotkey_label(combo: &str) -> String {
    combo.to_uppercase().replace('+', " + ")
}

/// A global-hotkey row's description and key cap: `what` it does and the
/// bound combo, or why there is none.
fn hotkey_parts(what: &str, combo: Option<&str>) -> (String, Option<String>) {
    match combo {
        Some(combo) => (what.to_string(), Some(hotkey_label(combo))),
        None => (
            format!("{what} All candidate combos are taken by other apps."),
            None,
        ),
    }
}

fn watcher_text(watcher: Option<&WatcherStatus>) -> String {
    match watcher {
        Some(w) => format!(
            "Watcher: Teams key found: {} · mic in use: {}{}",
            yes_no(w.teams_key_found),
            yes_no(w.mic_in_use),
            if w.suppressed {
                " · auto-restart suppressed"
            } else {
                ""
            }
        ),
        None => "Watcher: …".into(),
    }
}

/// The diagnostics snapshot time, local, with seconds (`toLocaleString()`
/// in en-GB: two refreshes within a minute must still look different).
/// Unparseable input is returned unchanged.
fn fmt_generated_at(rfc3339: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(at) => at
            .with_timezone(&chrono::Local)
            .format("%d/%m/%Y, %H:%M:%S")
            .to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// Snap a slider value to the 0.05 grid inside 0.40–0.80 (no float noise
/// like 0.6500001 reaches the settings file).
fn snap_threshold(value: f32) -> f32 {
    let clamped = value.clamp(THRESHOLD_MIN, THRESHOLD_MAX);
    let snapped = (clamped / THRESHOLD_STEP).round() * THRESHOLD_STEP;
    (snapped * 100.0).round() / 100.0
}

/// The user's slider edit, if any. egui's `Slider` (clamping `Always` plus
/// `step_by`) rounds an off-grid value such as a hand-edited 0.62 to the
/// step as it draws and reports that as a change; the range input never
/// rewrote the value untouched. So a change counts as an edit when the
/// pointer or keyboard was on the slider this pass (`interacted` — even a
/// click on the 0.60 the handle already shows over a stored 0.62), or,
/// without such input (e.g. a screen reader's increment), when it moved to
/// a different grid position than the one shown.
fn threshold_edit(shown: f32, value: f32, changed: bool, interacted: bool) -> Option<f32> {
    let snapped = snap_threshold(value);
    (changed && (interacted || snapped != snap_threshold(shown))).then_some(snapped)
}

/// The value shown beside the slider.
fn threshold_readout(t: f32) -> String {
    format!("{t:.2}")
}

/// The slider's accessible value text (not shown visually).
fn threshold_value_text(t: f32) -> String {
    let word = if t < 0.55 {
        "lenient"
    } else if t < 0.7 {
        "balanced"
    } else {
        "strict"
    };
    format!("{t:.2} — {word}")
}

/// `dl != null && !dl.done`: a failed download keeps its badge but no
/// longer disables the buttons.
fn download_active(dl: Option<&ModelDownloadProgress>) -> bool {
    dl.is_some_and(|d| !d.done)
}

/// The progress badge text (map-ui-settings §1.9).
fn download_progress_text(p: &ModelDownloadProgress) -> String {
    match p.total_bytes.filter(|t| *t > 0) {
        Some(total) => format!(
            "file {} of {} — {}% of {} MB",
            p.file_index,
            p.file_count,
            (p.downloaded_bytes as f64 / total as f64 * 100.0).round(),
            (total as f64 / 1e6).round()
        ),
        None => format!(
            "file {} of {} — {} MB…",
            p.file_index,
            p.file_count,
            (p.downloaded_bytes as f64 / 1e6).round()
        ),
    }
}

/// How far a download is overall (0..1), when its size is known: the
/// finished files plus the current file's share.
fn download_fraction(p: &ModelDownloadProgress) -> Option<f32> {
    let total = p.total_bytes.filter(|t| *t > 0)?;
    let count = p.file_count.max(1) as f32;
    let file = (p.downloaded_bytes as f32 / total as f32).clamp(0.0, 1.0);
    let done = (p.file_index.max(1) - 1) as f32;
    Some(((done + file) / count).clamp(0.0, 1.0))
}

fn installed_text(size_mb: Option<f64>) -> String {
    match size_mb {
        Some(mb) => format!("installed · {} MB", mb.round()),
        None => "installed".into(),
    }
}

/// A model row's state widget, in settings.tsx's decision order.
#[derive(Debug, PartialEq)]
enum ModelState {
    Installed {
        text: String,
        tooltip: String,
    },
    Failed(String),
    Progress(String),
    /// A sibling model of the engine being downloaded.
    Waiting,
    /// Download / Repair button, with an optional integrity badge.
    Idle {
        integrity_error: Option<String>,
    },
}

fn model_state(model: &ModelInfo, dl: Option<&ModelDownloadProgress>) -> ModelState {
    if model.present {
        return ModelState::Installed {
            text: installed_text(model.size_mb),
            tooltip: format!(
                "Verified revision {}",
                model.installed_revision.as_deref().unwrap_or("unknown")
            ),
        };
    }
    if let Some(dl) = dl.filter(|d| d.model_id == model.id) {
        return match &dl.error {
            Some(error) => ModelState::Failed(error.clone()),
            None => ModelState::Progress(download_progress_text(dl)),
        };
    }
    if download_active(dl) && dl.is_some_and(|d| d.model_id == model.engine.to_string()) {
        return ModelState::Waiting;
    }
    ModelState::Idle {
        integrity_error: model.integrity_error.clone(),
    }
}

#[derive(Debug, PartialEq)]
enum GpuLibsState {
    Installed(String),
    Failed(String),
    Progress(String),
    /// System CUDA libraries already resolve; no button.
    System,
    Idle {
        integrity_error: Option<String>,
        label: String,
    },
}

fn gpu_libs_state(libs: &GpuLibsInfo, dl: Option<&ModelDownloadProgress>) -> GpuLibsState {
    if libs.present {
        return GpuLibsState::Installed(installed_text(libs.size_mb));
    }
    if let Some(dl) = dl.filter(|d| d.model_id == GPU_LIBS_ID) {
        return match &dl.error {
            Some(error) => GpuLibsState::Failed(error.clone()),
            None => GpuLibsState::Progress(download_progress_text(dl)),
        };
    }
    if libs.cuda_ready {
        return GpuLibsState::System;
    }
    let label = if libs.integrity_error.is_some() {
        "Repair".into()
    } else {
        format!("Download (~{:.1} GB)", libs.download_mb as f64 / 1000.0)
    };
    GpuLibsState::Idle {
        integrity_error: libs.integrity_error.clone(),
        label,
    }
}

fn cache_text(bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) => format!("{:.2} GB on disk", bytes as f64 / 1e9),
        None => "…".into(),
    }
}

/// "(N files, X MB). Models can be re-downloaded and are not copied."
fn backup_details(backup: &BackupSummary) -> String {
    format!(
        "({} files, {:.1} MB). Models can be re-downloaded and are not copied.",
        backup.file_count,
        backup.total_bytes as f64 / 1_048_576.0
    )
}

fn speech_learned(sample_seconds: f64) -> String {
    format!("{} min of speech learned", (sample_seconds / 60.0).round())
}

/// How a configured device value relates to the listed endpoints.
#[derive(Debug, PartialEq)]
enum ConfiguredDevice<'a> {
    /// Matched by endpoint ID.
    Id,
    /// Stored by its friendly name (older settings).
    Legacy(&'a AudioDevice),
    Missing,
}

fn configured_device<'a>(
    configured: Option<&str>,
    devices: &'a [AudioDevice],
) -> Option<ConfiguredDevice<'a>> {
    let configured = configured.filter(|c| !c.is_empty())?;
    if devices.iter().any(|d| d.id == configured) {
        return Some(ConfiguredDevice::Id);
    }
    let lower = configured.to_lowercase();
    Some(
        match devices.iter().find(|d| d.name.to_lowercase() == lower) {
            Some(device) => ConfiguredDevice::Legacy(device),
            None => ConfiguredDevice::Missing,
        },
    )
}

#[derive(Debug, Clone, PartialEq)]
struct DeviceOption {
    value: String,
    label: String,
}

/// The picker's options: the default, a synthetic entry for a configured
/// value that no endpoint ID matches, then every device.
fn device_options(
    configured: Option<&str>,
    devices: Option<&[AudioDevice]>,
    list_failed: bool,
    default_label: &str,
) -> Vec<DeviceOption> {
    let mut options = vec![DeviceOption {
        value: String::new(),
        label: default_label.into(),
    }];
    let list = devices.unwrap_or(&[]);
    if let Some(value) = configured.filter(|c| !c.is_empty()) {
        let label = match configured_device(Some(value), list) {
            Some(ConfiguredDevice::Id) | None => None,
            Some(ConfiguredDevice::Legacy(device)) => Some(format!(
                "{} (legacy name — reselect to pin this endpoint)",
                device.name
            )),
            Some(ConfiguredDevice::Missing) => Some(if devices.is_some() {
                format!("Missing configured device — {value}")
            } else if list_failed {
                "Configured device — device list unavailable".into()
            } else {
                "Configured device — checking availability…".into()
            }),
        };
        if let Some(label) = label {
            options.push(DeviceOption {
                value: value.into(),
                label,
            });
        }
    }
    options.extend(list.iter().map(|d| DeviceOption {
        value: d.id.clone(),
        label: d.name.clone(),
    }));
    options
}

fn recording_text(d: &Diagnostics) -> String {
    match d.recording_meeting_id {
        Some(id) => format!("{} · meeting {id}", d.recording_state),
        None => d.recording_state.clone(),
    }
}

fn processing_text(d: &Diagnostics) -> String {
    let head = match d.processing_meeting_id {
        None => "idle".to_string(),
        Some(id) => format!(
            "meeting {id} · {}{}",
            d.processing_stage.as_deref().unwrap_or("starting"),
            d.processing_pct
                .map_or_else(String::new, |pct| format!(" · {pct:.1}%"))
        ),
    };
    format!("{head} · {} queued", d.queue_len)
}

fn capture_health_text(d: &Diagnostics) -> String {
    let Some(health) = &d.capture_health else {
        return "not available in this session".into();
    };
    let connected = |c: bool| if c { "connected" } else { "disconnected" };
    format!(
        "{}: mic {}, {} ms dropped · output {}, {} ms dropped",
        if d.capture_health_is_current {
            "current"
        } else {
            "last recording"
        },
        connected(health.mic.connected),
        health.mic.capture_dropped_ms,
        connected(health.loopback.connected),
        health.loopback.capture_dropped_ms
    )
}

fn model_integrity_text(d: &Diagnostics) -> String {
    let present = d.models.iter().filter(|m| m.present).count();
    let failed = d.models.iter().any(|m| m.integrity_error.is_some());
    format!(
        "{present}/{} verified{}",
        d.models.len(),
        if failed {
            " · one or more checks failed"
        } else {
            ""
        }
    )
}

fn gpu_runtime_text(d: &Diagnostics) -> String {
    let gpu = &d.gpu;
    let head = if gpu.cuda_ready {
        "CUDA runtime ready".to_string()
    } else if gpu.driver_present {
        let missing = if gpu.missing_dlls.is_empty() {
            "runtime libraries".to_string()
        } else {
            gpu.missing_dlls.join(", ")
        };
        format!("CPU fallback · missing {missing}")
    } else {
        "CPU fallback · no NVIDIA driver".to_string()
    };
    let tail = match &gpu.managed_libraries_integrity_error {
        Some(error) => format!(" · managed install check failed: {error}"),
        None if gpu.managed_libraries_present => " · managed libraries verified".into(),
        None => String::new(),
    };
    format!("{head}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use witness_core::recorder::{RecordingHealth, TrackHealth};
    use witness_core::service::{DiagnosticDatabase, DiagnosticGpu, DiagnosticModel};

    fn device(id: &str, name: &str) -> AudioDevice {
        AudioDevice {
            id: id.into(),
            name: name.into(),
        }
    }

    fn progress(model_id: &str, done: bool, error: Option<&str>) -> ModelDownloadProgress {
        ModelDownloadProgress {
            model_id: model_id.into(),
            file: "encoder.onnx".into(),
            downloaded_bytes: 250_000_000,
            total_bytes: Some(1_000_000_000),
            file_index: 2,
            file_count: 5,
            done,
            error: error.map(String::from),
        }
    }

    fn model(id: &str, engine: Engine, present: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            engine,
            display_name: id.into(),
            present,
            size_mb: Some(612.4),
            expected_revision: "0123456789abcdef".into(),
            installed_revision: present.then(|| "0123456789abcdef".into()),
            integrity_error: None,
        }
    }

    #[test]
    fn text_fields_parse_like_settings_tsx() {
        assert_eq!(
            parse_patterns(" MSTeams , ,Zoom,"),
            vec!["MSTeams".to_string(), "Zoom".to_string()]
        );
        assert!(parse_patterns("  ,  ").is_empty());
        assert_eq!(
            parse_lines("you\n\n  thank you.  \r\n"),
            vec!["you".to_string(), "thank you.".to_string()]
        );
    }

    #[test]
    fn hotkey_rows() {
        assert_eq!(hotkey_label("ctrl+alt+r"), "CTRL + ALT + R");
        assert_eq!(
            hotkey_parts("Starts/stops recording.", Some("ctrl+alt+r")),
            (
                "Starts/stops recording.".to_string(),
                Some("CTRL + ALT + R".to_string())
            )
        );
        assert_eq!(
            hotkey_parts("Bookmarks the current moment.", None),
            (
                "Bookmarks the current moment. All candidate combos are taken by other apps."
                    .to_string(),
                None
            )
        );
    }

    #[test]
    fn watcher_row() {
        assert_eq!(watcher_text(None), "Watcher: …");
        let w = WatcherStatus {
            enabled: true,
            teams_key_found: true,
            mic_in_use: false,
            suppressed: true,
        };
        assert_eq!(
            watcher_text(Some(&w)),
            "Watcher: Teams key found: yes · mic in use: no · auto-restart suppressed"
        );
    }

    #[test]
    fn threshold_snaps_and_describes() {
        assert_eq!(snap_threshold(0.650_000_1), 0.65);
        assert_eq!(snap_threshold(0.1), 0.4);
        assert_eq!(snap_threshold(0.93), 0.8);
        assert_eq!(snap_threshold(0.62), 0.6);
        assert_eq!(threshold_readout(0.6), "0.60");
        assert_eq!(threshold_value_text(0.5), "0.50 — lenient");
        assert_eq!(threshold_value_text(0.55), "0.55 — balanced");
        assert_eq!(threshold_value_text(0.7), "0.70 — strict");
    }

    #[test]
    fn slider_rounding_an_off_grid_value_is_not_an_edit() {
        // egui snapped a hand-edited 0.62 to 0.60 while drawing and
        // reported a change: not the user's doing, so nothing is saved.
        assert_eq!(threshold_edit(0.62, 0.6, true, false), None);
        assert_eq!(threshold_edit(0.6, 0.6, true, false), None);
        assert_eq!(threshold_edit(0.62, 0.65, false, false), None);
        assert_eq!(threshold_edit(0.62, 0.65, false, true), None);
        // Real edits, also away from an off-grid start.
        assert_eq!(threshold_edit(0.62, 0.650_000_1, true, true), Some(0.65));
        assert_eq!(threshold_edit(0.62, 0.55, true, true), Some(0.55));
        assert_eq!(threshold_edit(0.6, 0.8, true, true), Some(0.8));
        // A non-pointer change (screen-reader increment) to a new position.
        assert_eq!(threshold_edit(0.6, 0.65, true, false), Some(0.65));
    }

    #[test]
    fn diagnostics_time_keeps_seconds() {
        let text = fmt_generated_at("2026-10-06T10:05:33+00:00");
        // Local time zone varies; the shape and the seconds do not.
        assert_eq!(text.len(), "06/10/2026, 10:05:33".len(), "{text}");
        assert!(text.ends_with(":33") && text.contains("/2026, "), "{text}");
        assert_eq!(fmt_generated_at("not a date"), "not a date");
    }

    #[test]
    fn clicking_the_shown_grid_value_over_an_off_grid_one_saves_it() {
        // The handle shows 0.60 over a stored 0.62; clicking it there is the
        // user picking 0.60.
        assert_eq!(threshold_edit(0.62, 0.6, true, true), Some(0.6));
    }

    /// The real egui slider, untouched, over an off-grid stored value: it
    /// rewrites the value and reports a change, which `threshold_edit`
    /// must ignore.
    #[test]
    fn untouched_slider_over_an_off_grid_value_saves_nothing() {
        let ctx = theme::test_context();
        let mut edit = Some(f32::NAN);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let shown = 0.62_f32;
                let mut value = shown;
                let response = ui.add(
                    egui::Slider::new(&mut value, THRESHOLD_MIN..=THRESHOLD_MAX)
                        .step_by(THRESHOLD_STEP as f64)
                        .show_value(false),
                );
                assert!(response.changed(), "egui reports its own rounding");
                assert!((value - 0.6).abs() < 1e-6, "rounded to {value}");
                let interacted = response.is_pointer_button_down_on()
                    || response.dragged()
                    || response.clicked();
                assert!(!interacted);
                edit = threshold_edit(shown, value, response.changed(), interacted);
            });
        });
        output.textures_delta.clear(); // no renderer to upload the font atlas
        assert_eq!(edit, None);
    }

    #[test]
    fn progress_text_with_and_without_a_total() {
        let mut p = progress("parakeet", false, None);
        assert_eq!(download_progress_text(&p), "file 2 of 5 — 25% of 1000 MB");
        p.total_bytes = None;
        assert_eq!(download_progress_text(&p), "file 2 of 5 — 250 MB…");
        p.total_bytes = Some(0);
        assert_eq!(download_progress_text(&p), "file 2 of 5 — 250 MB…");
    }

    #[test]
    fn download_fraction_spans_all_files() {
        let mut p = progress("parakeet", false, None);
        // File 2 of 5, a quarter through: (1 + 0.25) / 5.
        assert_eq!(download_fraction(&p), Some(0.25));
        p.file_index = 5;
        p.downloaded_bytes = 1_000_000_000;
        assert_eq!(download_fraction(&p), Some(1.0));
        p.total_bytes = None;
        assert_eq!(download_fraction(&p), None);
    }

    #[test]
    fn model_badges_follow_the_decision_order() {
        let installed = model("parakeet", Engine::Parakeet, true);
        assert_eq!(
            model_state(&installed, None),
            ModelState::Installed {
                text: "installed · 612 MB".into(),
                tooltip: "Verified revision 0123456789abcdef".into()
            }
        );
        let parakeet = model("parakeet", Engine::Parakeet, false);
        let sortformer = model("sortformer", Engine::Parakeet, false);
        let whisper = model("whisper", Engine::Whisper, false);

        let running = progress("parakeet", false, None);
        assert_eq!(
            model_state(&parakeet, Some(&running)),
            ModelState::Progress("file 2 of 5 — 25% of 1000 MB".into())
        );
        // Siblings wait while the engine-named model downloads.
        assert_eq!(
            model_state(&sortformer, Some(&running)),
            ModelState::Waiting
        );
        assert_eq!(
            model_state(&whisper, Some(&running)),
            ModelState::Idle {
                integrity_error: None
            }
        );
        // The failed done event carries the engine id: only that row fails.
        let failed = progress("parakeet", true, Some("offline"));
        assert_eq!(
            model_state(&parakeet, Some(&failed)),
            ModelState::Failed("offline".into())
        );
        assert_eq!(
            model_state(&sortformer, Some(&failed)),
            ModelState::Idle {
                integrity_error: None
            }
        );
        assert!(!download_active(Some(&failed)));
        assert!(download_active(Some(&running)));

        let mut broken = model("whisper", Engine::Whisper, false);
        broken.integrity_error = Some("hash mismatch".into());
        assert_eq!(
            model_state(&broken, None),
            ModelState::Idle {
                integrity_error: Some("hash mismatch".into())
            }
        );
    }

    #[test]
    fn gpu_libs_row_states() {
        let mut libs = GpuLibsInfo {
            present: false,
            size_mb: Some(1043.6),
            integrity_error: None,
            driver_present: true,
            cuda_ready: false,
            download_mb: 1234,
        };
        assert_eq!(
            gpu_libs_state(&libs, None),
            GpuLibsState::Idle {
                integrity_error: None,
                label: "Download (~1.2 GB)".into()
            }
        );
        let running = progress(GPU_LIBS_ID, false, None);
        assert!(matches!(
            gpu_libs_state(&libs, Some(&running)),
            GpuLibsState::Progress(_)
        ));
        let failed = progress(GPU_LIBS_ID, true, Some("disk full"));
        assert_eq!(
            gpu_libs_state(&libs, Some(&failed)),
            GpuLibsState::Failed("disk full".into())
        );
        // Another download's progress does not touch this row.
        let other = progress("parakeet", false, None);
        libs.cuda_ready = true;
        assert_eq!(gpu_libs_state(&libs, Some(&other)), GpuLibsState::System);
        libs.cuda_ready = false;
        libs.integrity_error = Some("missing cudnn64_9.dll".into());
        assert!(matches!(
            gpu_libs_state(&libs, None),
            GpuLibsState::Idle { label, .. } if label == "Repair"
        ));
        libs.present = true;
        assert_eq!(
            gpu_libs_state(&libs, None),
            GpuLibsState::Installed("installed · 1044 MB".into())
        );
    }

    #[test]
    fn device_options_cover_legacy_and_missing_values() {
        let list = [
            device("{0.0.1}.{aaa}", "Chat (GoXLR)"),
            device("{0.0.1}.{bbb}", "Speakers"),
        ];
        let labels = |o: Vec<DeviceOption>| o.into_iter().map(|o| o.label).collect::<Vec<_>>();

        // Not configured: default + devices.
        assert_eq!(
            labels(device_options(
                None,
                Some(&list),
                false,
                "Default output device"
            )),
            vec!["Default output device", "Chat (GoXLR)", "Speakers"]
        );
        // Matched by ID: no synthetic option.
        assert_eq!(
            device_options(Some("{0.0.1}.{bbb}"), Some(&list), false, "D").len(),
            3
        );
        // Legacy friendly name (case-insensitive).
        let legacy = device_options(Some("chat (goxlr)"), Some(&list), false, "D");
        assert_eq!(
            legacy[1],
            DeviceOption {
                value: "chat (goxlr)".into(),
                label: "Chat (GoXLR) (legacy name — reselect to pin this endpoint)".into()
            }
        );
        assert!(matches!(
            configured_device(Some("chat (goxlr)"), &list),
            Some(ConfiguredDevice::Legacy(_))
        ));
        // Missing once loaded / while loading / when listing failed.
        assert_eq!(
            device_options(Some("Old headset"), Some(&list), false, "D")[1].label,
            "Missing configured device — Old headset"
        );
        assert_eq!(
            device_options(Some("Old headset"), None, false, "D")[1].label,
            "Configured device — checking availability…"
        );
        assert_eq!(
            device_options(Some("Old headset"), None, true, "D")[1].label,
            "Configured device — device list unavailable"
        );
        assert_eq!(
            configured_device(Some("Old headset"), &list),
            Some(ConfiguredDevice::Missing)
        );
        assert_eq!(configured_device(None, &list), None);
    }

    fn diagnostics() -> Diagnostics {
        Diagnostics {
            generated_at: "2026-10-05T10:00:00+01:00".into(),
            app_version: "0.2.1".into(),
            platform: "windows / x86_64".into(),
            runtime_data_dir: "C:\\data".into(),
            configured_data_dir: None,
            restart_required: false,
            database: DiagnosticDatabase {
                path: "C:\\data\\witness.db".into(),
                healthy: true,
                detail: "ok".into(),
            },
            log_path: "C:\\data\\witness.log".into(),
            recording_state: "idle".into(),
            recording_meeting_id: None,
            recording_since: None,
            capture_health: None,
            capture_health_is_current: false,
            processing_meeting_id: None,
            processing_stage: None,
            processing_pct: None,
            queue_len: 0,
            models: vec![
                DiagnosticModel {
                    id: "parakeet".into(),
                    present: true,
                    expected_revision: "a".into(),
                    installed_revision: Some("a".into()),
                    integrity_error: None,
                },
                DiagnosticModel {
                    id: "whisper".into(),
                    present: false,
                    expected_revision: "b".into(),
                    installed_revision: None,
                    integrity_error: None,
                },
            ],
            gpu: DiagnosticGpu {
                driver_present: false,
                cuda_ready: false,
                missing_dlls: vec![],
                managed_libraries_present: false,
                managed_libraries_integrity_error: None,
            },
            report: String::new(),
        }
    }

    #[test]
    fn diagnostics_rows() {
        let mut d = diagnostics();
        assert_eq!(recording_text(&d), "idle");
        assert_eq!(processing_text(&d), "idle · 0 queued");
        assert_eq!(capture_health_text(&d), "not available in this session");
        assert_eq!(model_integrity_text(&d), "1/2 verified");
        assert_eq!(gpu_runtime_text(&d), "CPU fallback · no NVIDIA driver");

        d.recording_state = "recording".into();
        d.recording_meeting_id = Some(12);
        d.processing_meeting_id = Some(11);
        d.processing_pct = Some(42.25);
        d.queue_len = 2;
        d.capture_health = Some(RecordingHealth {
            mic: TrackHealth {
                connected: true,
                capture_dropped_ms: 40,
                ..Default::default()
            },
            loopback: TrackHealth::default(),
            writer_error: None,
        });
        d.capture_health_is_current = true;
        d.models[1].integrity_error = Some("bad".into());
        d.gpu.driver_present = true;
        d.gpu.managed_libraries_present = true;
        assert_eq!(recording_text(&d), "recording · meeting 12");
        assert_eq!(
            processing_text(&d),
            "meeting 11 · starting · 42.2% · 2 queued"
        );
        assert_eq!(
            capture_health_text(&d),
            "current: mic connected, 40 ms dropped · output disconnected, 0 ms dropped"
        );
        assert_eq!(
            model_integrity_text(&d),
            "1/2 verified · one or more checks failed"
        );
        assert_eq!(
            gpu_runtime_text(&d),
            "CPU fallback · missing runtime libraries · managed libraries verified"
        );
        d.gpu.missing_dlls = vec!["cudnn64_9.dll".into(), "cufft64_12.dll".into()];
        d.gpu.managed_libraries_integrity_error = Some("hash".into());
        assert_eq!(
            gpu_runtime_text(&d),
            "CPU fallback · missing cudnn64_9.dll, cufft64_12.dll · managed install check failed: hash"
        );
        d.gpu.cuda_ready = true;
        d.gpu.managed_libraries_integrity_error = None;
        assert_eq!(
            gpu_runtime_text(&d),
            "CUDA runtime ready · managed libraries verified"
        );
    }

    #[test]
    fn small_formatters() {
        assert_eq!(cache_text(None), "…");
        assert_eq!(cache_text(Some(4_312_000_000)), "4.31 GB on disk");
        assert_eq!(speech_learned(150.0), "3 min of speech learned");
        assert_eq!(installed_text(None), "installed");
        let backup = BackupSummary {
            path: "D:\\b".into(),
            created_at: String::new(),
            file_count: 37,
            total_bytes: 36_175_872,
        };
        assert_eq!(
            backup_details(&backup),
            "(37 files, 34.5 MB). Models can be re-downloaded and are not copied."
        );
    }

    /// Lay out `f` in a `width` pt panel for a few passes (rows measure
    /// themselves on the first) and return what it produced last.
    fn run_at<R>(width: f32, mut f: impl FnMut(&mut egui::Ui) -> R) -> R {
        let ctx = theme::test_context();
        let mut out = None;
        for _ in 0..4 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 2000.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| out = Some(f(ui)));
            });
            output.textures_delta.clear(); // no renderer to upload the font atlas
        }
        out.expect("ran")
    }

    /// The section helpers lay out in a themed context without panicking.
    #[test]
    fn layout_helpers_render() {
        run_at(460.0, |ui| {
            section(ui, "Models", |ui| {
                row(
                    ui,
                    "test-row",
                    Slot::Fill(FIELD_WIDTH),
                    |ui| {
                        let id = title(ui, "Parakeet TDT 0.6B v3 (ONNX)");
                        note(ui, "muted");
                        path_box(ui, "C:\\Users\\me\\AppData\\Local\\Witness");
                        progress_bar(ui, 0.4);
                        id
                    },
                    |ui, label| {
                        let options = device_options(None, Some(&[]), false, "Default");
                        assert_eq!(device_picker(ui, "test-picker", label, &options, ""), None);
                    },
                );
                widgets::group_divider(ui);
                assert!(!warning_strip(
                    ui,
                    "model not downloaded — transcription will fail",
                    "Download",
                    true
                ));
                kv_row(ui, "Database", |ui| {
                    path_value(ui, "C:\\data\\witness.db", Some((BadgeKind::Ok, "healthy")));
                });
                hotkey_row(
                    ui,
                    "Recording hotkey",
                    "Starts/stops recording.",
                    Some("ctrl+alt+r"),
                );
                keycap(ui, "CTRL + ALT + B");
            });
            footnote(ui, "paragraph");
        });
    }

    /// A switch stays beside its label even in the narrowest window, while
    /// a field wide enough to squeeze the text stacks under it.
    #[test]
    fn rows_keep_switches_beside_and_stack_fields() {
        for width in [380.0, 460.0, 640.0, 1000.0] {
            let (title_rect, switch_rect, field_label, field) = run_at(width, |ui| {
                let mut on = true;
                let mut title_rect = egui::Rect::NOTHING;
                let switch = row(
                    ui,
                    "switch",
                    Slot::Fit,
                    |ui| {
                        let r = widgets::elide(ui, "Auto-record", theme::regular(14.0), pal().text);
                        title_rect = r.rect;
                        r.id
                    },
                    |ui, _| widgets::switch(ui, true, &mut on, "Auto-record"),
                );
                let mut label_rect = egui::Rect::NOTHING;
                let field = row(
                    ui,
                    "field",
                    Slot::Fill(FIELD_WIDTH),
                    |ui| {
                        let r =
                            widgets::elide(ui, "Watch patterns", theme::regular(14.0), pal().text);
                        label_rect = r.rect;
                        r.id
                    },
                    |ui, _| ui.label("field").rect,
                );
                (title_rect, switch.rect, label_rect, field)
            });
            // Beside, right-aligned, vertically centred on the title.
            assert!(switch_rect.left() > title_rect.right(), "{width}");
            assert!(
                (switch_rect.center().y - title_rect.center().y).abs() < 1.5,
                "{width}"
            );
            let stacked = field.top() > field_label.bottom();
            // The panel has 8 pt margins; the field stacks below 2*8 + 260 +
            // 16 + 200 pt.
            assert_eq!(stacked, width < 492.0, "{width}");
        }
    }
}
