//! Settings: Startup, Updates, Storage, Recording, Transcription, Models,
//! Diagnostics and People sections (map-ui-settings §1, settings.tsx).
//!
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
use crate::ui::theme::{self, BG, OK, TEXT, TEXT_MUTED};
use crate::ui::toasts::Notice;
use crate::ui::widgets::{self, BadgeKind, KeyScroll, Politeness};
use crate::ui::{format, UiCmd, UiCtx};
use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText};
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

/// `.setting-row > label:first-child { min-width: 140px }`.
const LABEL_WIDTH: f32 = 140.0;
/// `.text-input { min-width: 220px }`.
const INPUT_WIDTH: f32 = 220.0;
/// The voice-match slider's UI range (the backend accepts 0–1).
const THRESHOLD_MIN: f32 = 0.40;
const THRESHOLD_MAX: f32 = 0.80;
const THRESHOLD_STEP: f32 = 0.05;
/// "Copied ✓" stays this long.
const COPIED_FLASH: Duration = Duration::from_millis(1500);
/// The backup folder picker's title (commands.rs `create_backup`).
const BACKUP_DIALOG_TITLE: &str = "Choose where to create the Witness backup";
/// The GPU-library download's progress id.
const GPU_LIBS_ID: &str = "gpu-libs";

/// A background backup: its completion flag plus the slot its worker fills
/// with the summary (read back here; dropped harmlessly if the view is gone).
struct BackupRun {
    handle: OpHandle,
    summary: Arc<Mutex<Option<BackupSummary>>>,
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
    autostart_set: Option<Task<(bool, Result<(), String>)>>,
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

    /// Commit a text-field draft that has not been blurred yet (the view is
    /// being left, or the window hidden). Other edits already went through
    /// `SettingsStore`, whose queue outlives the view.
    pub fn flush(&mut self, cx: &mut UiCtx) {
        self.state.commit_drafts(cx);
    }

    pub fn on_leave(&mut self, cx: &mut UiCtx) {
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
    /// None until loaded (the checkbox is disabled meanwhile).
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
    backup: Option<BackupRun>,
    last_backup: Option<BackupSummary>,
    deleting_cache: Option<OpHandle>,
    forgetting: Vec<OpHandle>,
    /// The person a pending "Forget voice" confirmation is about.
    confirm_forget: Option<(i64, String)>,
    /// This view asked the "Delete models" / "Stop & restart" confirms.
    /// The modal is app-level and outlives the view (Ctrl+F navigates while
    /// it shows), so an answer given after leaving must not fire on a later
    /// visit: answers are drained every pass and acted on only when asked.
    asked_cache: bool,
    asked_restart: bool,
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
            backup: None,
            last_backup: None,
            deleting_cache: None,
            forgetting: Vec::new(),
            confirm_forget: None,
            asked_cache: false,
            asked_restart: false,
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

        // The sticky header stays above the scrolling sections.
        widgets::centered_column(ui, 720.0, |ui| self.header(ui, cx));
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::keyboard_scroll(ui, KeyScroll { space: true });
                widgets::centered_column(ui, 720.0, |ui| {
                    section(ui, "Startup", |ui| self.startup(ui, cx));
                    self.updates.show(ui, cx);
                    section(ui, "Storage", |ui| self.storage(ui, cx));
                    section(ui, "Recording", |ui| self.recording(ui, cx));
                    section(ui, "Transcription", |ui| self.transcription(ui, cx));
                    section(ui, "Models", |ui| self.models_section(ui, cx));
                    section(ui, "Diagnostics", |ui| self.diagnostics_section(ui, cx));
                    section(ui, "People (voice prints)", |ui| {
                        self.people_section(ui, cx)
                    });
                });
            });
    }

    /// Save the watch patterns / noise phrases if they were typed into
    /// since their last commit.
    fn commit_drafts(&mut self, cx: &mut UiCtx) {
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
        if let Some(result) = take(&mut self.loads.autostart_set) {
            match result {
                Ok((enabled, Ok(()))) => self.autostart = Some(enabled),
                Ok((_, Err(error))) | Err(error) => {
                    cx.toasts.error("Could not update autostart", error)
                }
            }
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

        if let Some(run) = self.backup.take_if(|run| run.handle.is_done()) {
            if let Some(summary) = run.summary.lock().ok().and_then(|mut s| s.take()) {
                self.last_backup = Some(summary);
            }
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
    // Header and sections

    fn header(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.label(
                RichText::new("Settings")
                    .font(theme::semibold(17.0))
                    .color(TEXT),
            );
            if let Some(at) = cx.settings.saved_flash() {
                ui.label(RichText::new("saved ✓").size(13.0).color(OK));
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
        ui.add_space(12.0);
    }

    fn startup(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let mut enabled = self.autostart.unwrap_or(false);
        let loaded = self.autostart.is_some() && self.loads.autostart_set.is_none();
        let response = ui.add_enabled(
            loaded,
            egui::Checkbox::new(
                &mut enabled,
                "Start Witness with Windows (minimized to tray)",
            ),
        );
        if response.changed() {
            // The checkbox shows the new value once the registry write succeeds.
            self.loads.autostart_set = Some(cx.tasks.spawn("autostart-write", move |_| {
                (enabled, autostart::set_enabled(enabled))
            }));
        }
    }

    fn storage(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let data_dir = cx.settings.current().data_dir.clone();
        row(ui, |ui| {
            row_label(ui, "Data directory");
            path_code(ui, data_dir.as_deref().unwrap_or("(next to witness.exe)"));
            if ghost(ui, "Change…").clicked() && self.loads.pick_dir.is_none() {
                let owner = cx.dialog_owner();
                self.loads.pick_dir = Some(cx.tasks.spawn("pick-data-dir", move |_| {
                    dialogs::pick_folder_blocking(owner, None)
                }));
            }
        });
        muted_paragraph(
            ui,
            "Holds the database, recordings and models. Changing it does not move existing data.",
        );

        let running = self.backup.is_some();
        row(ui, |ui| {
            let label = if running {
                "Creating backup..."
            } else {
                "Create backup..."
            };
            if ghost_enabled(ui, !running, label).clicked() {
                self.start_backup(cx);
            }
            muted(
                ui,
                "Complete snapshot; recording and processing must be idle.",
            );
        });
        if let Some(backup) = &self.last_backup {
            // "Backup saved to <path> (N files, X MB). Models can be …"
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                muted(ui, "Backup saved to");
                path_code(ui, &backup.path);
                muted(ui, &backup_details(backup));
            });
        }

        let restart_pending = self.needs_restart
            || self
                .diagnostics
                .as_ref()
                .is_some_and(|d| d.restart_required);
        if restart_pending {
            row(ui, |ui| {
                muted(ui, "The new data directory applies after a restart.");
                if primary(ui, "Restart Witness").clicked() {
                    self.restart(cx);
                }
            });
        }
    }

    fn recording(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let mut auto_record = cx.settings.current().auto_record;
        if ui
            .checkbox(&mut auto_record, "Auto-record when a meeting is detected")
            .changed()
        {
            save(cx, |s| s.auto_record = auto_record);
        }

        row(ui, |ui| {
            let label = row_label(ui, "Watch patterns");
            let width = input_width(ui, INPUT_WIDTH);
            wrap_for(ui, width);
            let response = ui
                .add(
                    egui::TextEdit::singleline(&mut self.patterns_text)
                        .id(patterns_id())
                        .desired_width(width),
                )
                .labelled_by(label);
            widgets::text_edit_menu(&response);
            self.patterns_dirty |= response.changed();
            // Enter surrenders focus too, so this covers blur and Enter. A
            // clean buffer was already committed (or re-synced): skipping it
            // keeps a draft that `flush` committed — and the store reverted
            // — from failing a second time when the field finally blurs.
            if response.lost_focus() && self.patterns_dirty {
                self.commit_patterns(cx, parse_patterns(&self.patterns_text));
            }
            muted(ui, "comma-separated, matched against mic-using app names");
        });

        let settings = cx.settings.current().clone();
        let failed = self.devices_error.is_some();
        device_row(
            ui,
            cx,
            &DeviceRow {
                label: "Microphone",
                id: "mic-device",
                default_label: "Default input device",
                missing_badge: "configured microphone is not connected",
                help: None,
                configured: settings.mic_device.as_deref(),
                devices: self.devices.as_ref().map(|d| d.capture.as_slice()),
                list_failed: failed,
            },
            |s, value| s.mic_device = value,
        );
        device_row(
            ui,
            cx,
            &DeviceRow {
                label: "Meeting audio",
                id: "loopback-device",
                default_label: "Default output device",
                missing_badge: "configured output is not connected",
                help: Some(
                    "the output Teams plays through — pick a dedicated one (e.g. \"Chat\") to keep music & game audio out of recordings",
                ),
                configured: settings.loopback_device.as_deref(),
                devices: self.devices.as_ref().map(|d| d.render.as_slice()),
                list_failed: failed,
            },
            |s, value| s.loopback_device = value,
        );
        if let Some(error) = &self.devices_error {
            widgets::inline_warning(ui, &format!("Audio devices could not be listed: {error}"));
        }
        row(ui, |ui| {
            if ghost(ui, "Refresh devices").clicked() {
                self.refresh_devices(cx);
            }
            muted(ui, "re-scan after plugging a device in");
        });
        muted_paragraph(
            ui,
            &watcher_text(cx.shell.status.as_ref().map(|s| &s.watcher)),
        );
    }

    fn transcription(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let settings = cx.settings.current().clone();

        let mut auto_transcribe = settings.auto_transcribe;
        if ui
            .checkbox(&mut auto_transcribe, "Auto-transcribe after each recording")
            .changed()
        {
            save(cx, |s| s.auto_transcribe = auto_transcribe);
        }
        let mut live = settings.live_transcribe;
        if ui
            .checkbox(
                &mut live,
                "Live captions while recording (Parakeet; provisional — the final pass replaces them)",
            )
            .changed()
        {
            save(cx, |s| s.live_transcribe = live);
        }
        // Disabled without live captions, but its value is kept.
        let mut overlay = settings.caption_overlay;
        if ui
            .add_enabled(
                settings.live_transcribe,
                egui::Checkbox::new(
                    &mut overlay,
                    "Floating caption window on top of other apps while recording",
                ),
            )
            .changed()
        {
            save(cx, |s| s.caption_overlay = overlay);
        }

        row(ui, |ui| {
            row_label(ui, "Voice match strictness");
            let shown_before = self
                .local_threshold
                .unwrap_or(settings.speaker_match_threshold);
            let mut value = shown_before;
            let enabled = ui.is_enabled();
            let response = ui.add(
                egui::Slider::new(&mut value, THRESHOLD_MIN..=THRESHOLD_MAX)
                    .step_by(THRESHOLD_STEP as f64)
                    .show_value(false),
            );
            let shown = value;
            response.widget_info(|| {
                egui::WidgetInfo::slider(
                    enabled,
                    shown as f64,
                    format!("Voice match strictness: {}", threshold_value_text(shown)),
                )
            });
            if let Some(edit) = threshold_edit(shown_before, value, response.changed()) {
                self.local_threshold = Some(edit);
            }
            // Commit once the pointer lets go (a keyboard change commits at
            // once), like the range input's `onChange`.
            if let Some(local) = self.local_threshold {
                if !response.dragged() && !response.is_pointer_button_down_on() {
                    self.local_threshold = None;
                    if local != settings.speaker_match_threshold {
                        save(cx, |s| s.speaker_match_threshold = local);
                    }
                }
            }
            let readout = self
                .local_threshold
                .unwrap_or(settings.speaker_match_threshold);
            muted(ui, &threshold_readout(readout));
        });

        // `.setting-row-top`: the label sits beside the textarea's top when
        // both fit, otherwise above it (flex-wrap).
        let label = "Discarded noise phrases";
        let label_width = text_width(ui, label, theme::regular(theme::BASE_SIZE)).max(LABEL_WIDTH);
        let side_by_side = label_width + 10.0 + INPUT_WIDTH <= row_width(ui);
        let layout = if side_by_side {
            Layout::left_to_right(Align::Min)
        } else {
            Layout::top_down(Align::Min)
        };
        ui.with_layout(layout, |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let label = row_label(ui, label);
            let room = if side_by_side {
                ui.available_width()
            } else {
                row_width(ui)
            };
            let width = room.min(320.0);
            let response = ui
                .add(
                    egui::TextEdit::multiline(&mut self.junk_text)
                        .id(junk_id())
                        .desired_rows(4)
                        .desired_width(width),
                )
                .labelled_by(label);
            widgets::text_edit_menu(&response);
            self.junk_dirty |= response.changed();
            // Blur only (Enter inserts a line); a clean buffer is skipped as
            // for the watch patterns.
            if response.lost_focus() && self.junk_dirty {
                self.commit_junk(cx, parse_lines(&self.junk_text));
            }
        });
        muted(
            ui,
            "one exact phrase per line; clear the list to disable phrase filtering",
        );

        muted_paragraph(
            ui,
            &hotkeys_text(
                cx.shell.record_hotkey.as_deref(),
                cx.shell.bookmark_hotkey.as_deref(),
            ),
        );

        let download_active = download_active(cx.shell.download.as_ref());
        row(ui, |ui| {
            row_label(ui, "Engine");
            let mut engine = settings.engine;
            ui.radio_value(&mut engine, Engine::Parakeet, "Parakeet TDT v3 (fast, GPU)");
            ui.radio_value(&mut engine, Engine::Whisper, "Whisper large-v3-turbo");
            if engine != settings.engine {
                save(cx, |s| s.engine = engine);
            }
            let missing = self
                .models
                .iter()
                .any(|m| m.engine == settings.engine && !m.present);
            if missing {
                warning_chip(ui, "model not downloaded — transcription will fail");
                if ghost_enabled(ui, !download_active, "Download").clicked() {
                    start_model_download(cx, settings.engine, "Could not download models".into());
                }
            }
        });
        muted_paragraph(
            ui,
            &format!(
                "GPU: {}",
                self.gpu.as_ref().map_or("checking…", |g| g.detail.as_str())
            ),
        );
        muted_paragraph(
            ui,
            "Speaker separation covers up to 8 remote speakers (NVIDIA Nemotron 3 Diarization).",
        );
    }

    fn models_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let dl = cx.shell.download.clone();
        let active = download_active(dl.as_ref());
        for model in &self.models {
            row(ui, |ui| {
                row_label(ui, &model.display_name);
                let failure = format!("Could not download {}", model.display_name);
                match model_state(model, dl.as_ref()) {
                    ModelState::Installed { text, tooltip } => {
                        badge(ui, BadgeKind::Ok, &text).on_hover_text(tooltip);
                    }
                    ModelState::Failed(error) => {
                        badge(ui, BadgeKind::Failed, &format!("failed: {error}"));
                        if ghost(ui, "Retry").clicked() {
                            cx.shell.download = None;
                            start_model_download(cx, model.engine, failure);
                        }
                    }
                    ModelState::Progress(text) => {
                        badge(ui, BadgeKind::Busy, &text);
                    }
                    ModelState::Waiting => {
                        badge(ui, BadgeKind::Busy, "waiting…");
                    }
                    ModelState::Idle { integrity_error } => {
                        if let Some(error) = &integrity_error {
                            badge(ui, BadgeKind::Failed, "integrity check failed")
                                .on_hover_text(error);
                        }
                        let label = if integrity_error.is_some() {
                            "Repair"
                        } else {
                            "Download"
                        };
                        if ghost_enabled(ui, !active, label).clicked() {
                            start_model_download(cx, model.engine, failure);
                        }
                    }
                }
                let short: String = model.expected_revision.chars().take(8).collect();
                ui.label(
                    RichText::new(format!("rev {short}"))
                        .monospace()
                        .color(TEXT_MUTED),
                )
                .on_hover_text(format!("Expected revision {}", model.expected_revision));
            });
        }

        if let Some(libs) = self.gpu_libs.as_ref().filter(|l| l.driver_present) {
            row(ui, |ui| {
                row_label(ui, "CUDA runtime + cuDNN (GPU)");
                match gpu_libs_state(libs, dl.as_ref()) {
                    GpuLibsState::Installed(text) => {
                        badge(ui, BadgeKind::Ok, &text);
                    }
                    GpuLibsState::Failed(error) => {
                        badge(ui, BadgeKind::Failed, &format!("failed: {error}"));
                        if ghost(ui, "Retry").clicked() {
                            cx.shell.download = None;
                            start_gpu_libs_download(cx);
                        }
                    }
                    GpuLibsState::Progress(text) => {
                        badge(ui, BadgeKind::Busy, &text);
                    }
                    GpuLibsState::System => muted(ui, "using system CUDA libraries"),
                    GpuLibsState::Idle {
                        integrity_error,
                        label,
                    } => {
                        if let Some(error) = &integrity_error {
                            badge(ui, BadgeKind::Failed, "integrity check failed")
                                .on_hover_text(error);
                        }
                        if ghost_enabled(ui, !active, label.as_str()).clicked() {
                            start_gpu_libs_download(cx);
                        }
                    }
                }
            });
        }

        let deleting = self.deleting_cache.is_some();
        row(ui, |ui| {
            row_label(ui, "Model cache");
            muted(ui, &cache_text(self.cache_bytes));
            let enabled = !active && !deleting && self.cache_bytes.is_some_and(|b| b > 0);
            let label = if deleting {
                "Deleting…"
            } else {
                "Delete model cache"
            };
            let response = ghost_enabled(ui, enabled, label)
                .on_hover_text(
                    "Deletes every downloaded model, including old versions — Shift-click to skip confirmation",
                );
            if response.clicked() {
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
        });
        muted_paragraph(
            ui,
            "Recording works without models — they're only needed for transcription.",
        );
    }

    fn diagnostics_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        muted_paragraph(
            ui,
            "This report contains operational state and local paths, but no audio, transcript text, notes, voice prints, or private settings.",
        );
        if let Some(error) = &self.diagnostics_error {
            widgets::inline_warning(ui, &format!("Diagnostics unavailable: {error}"));
        }
        let Some(d) = self.diagnostics.clone() else {
            if self.diagnostics_error.is_none() {
                muted_paragraph(ui, "Checking…");
            }
            return;
        };
        row(ui, |ui| {
            row_label(ui, "Build");
            value(ui, &format!("Witness {} · {}", d.app_version, d.platform));
        });
        row(ui, |ui| {
            row_label(ui, "Runtime data");
            path_code(ui, &d.runtime_data_dir);
            if d.restart_required {
                badge(ui, BadgeKind::Busy, "restart pending");
            }
        });
        row(ui, |ui| {
            row_label(ui, "Database");
            path_code(ui, &d.database.path);
            let (kind, text) = if d.database.healthy {
                (BadgeKind::Ok, "healthy")
            } else {
                (BadgeKind::Failed, "check failed")
            };
            badge(ui, kind, text).on_hover_text(&d.database.detail);
        });
        row(ui, |ui| {
            row_label(ui, "Log");
            path_code(ui, &d.log_path);
        });
        for (label, text) in [
            ("Recording", recording_text(&d)),
            ("Processing", processing_text(&d)),
            ("Capture health", capture_health_text(&d)),
            ("Model integrity", model_integrity_text(&d)),
            ("GPU runtime", gpu_runtime_text(&d)),
        ] {
            row(ui, |ui| {
                row_label(ui, label);
                value(ui, &text);
            });
        }
        row(ui, |ui| {
            if ghost(ui, "Refresh").clicked() {
                self.refresh_diagnostics(cx);
            }
            let copied = self
                .diagnostics_copied_at
                .is_some_and(|at| at.elapsed() < COPIED_FLASH);
            let label = if copied { "Copied ✓" } else { "Copy report" };
            if ghost(ui, label).clicked() {
                ui.ctx().copy_text(d.report.clone());
                self.diagnostics_copied_at = Some(Instant::now());
                cx.toasts.success("Diagnostic report copied");
            }
            if let Some(at) = self.diagnostics_copied_at {
                match COPIED_FLASH.checked_sub(at.elapsed()) {
                    Some(left) => ui.ctx().request_repaint_after(left),
                    None => self.diagnostics_copied_at = None,
                }
            }
            if ghost(ui, "Export…").clicked() {
                export_diagnostics(cx);
            }
            muted(
                ui,
                &format!("generated {}", format::fmt_date(&d.generated_at)),
            );
        });
    }

    fn people_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        if self.people.is_empty() {
            muted_paragraph(
                ui,
                "No one enrolled yet. Rename a speaker in any transcript to teach Witness their voice — future meetings will label them automatically.",
            );
            return;
        }
        let mut forget = None;
        for person in &self.people {
            row(ui, |ui| {
                row_label(ui, &person.name);
                muted(ui, &speech_learned(person.sample_seconds));
                if widgets::icon_btn_danger(
                    ui,
                    ph::TRASH,
                    15.0,
                    "Forget this voice — Shift-click to skip confirmation",
                )
                .clicked()
                {
                    let shift = ui.input(|i| i.modifiers.shift);
                    forget = Some((person.id, person.name.clone(), shift));
                }
            });
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
        muted_paragraph(
            ui,
            "Speakers matched by voice show a ≈ before their name; renaming them confirms or corrects the match.",
        );
    }

    // ------------------------------------------------------------------
    // Actions

    fn start_backup(&mut self, cx: &mut UiCtx) {
        let summary = Arc::new(Mutex::new(None));
        let slot = summary.clone();
        let owner = cx.dialog_owner();
        let handle = cx.tasks.spawn_op("create-backup", move |c| {
            // Cancel returns quietly, like the command's `None`.
            let Some(parent) = dialogs::pick_folder_blocking(owner, Some(BACKUP_DIALOG_TITLE))
            else {
                return quiet();
            };
            let notice = match c.create_backup(&parent) {
                Ok(backup) => {
                    if let Ok(mut slot) = slot.lock() {
                        *slot = Some(backup);
                    }
                    Notice::success("Backup created")
                }
                Err(error) => Notice::error("Could not create backup", error),
            };
            notice_done(notice)
        });
        self.backup = Some(BackupRun { handle, summary });
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

fn device_row(
    ui: &mut egui::Ui,
    cx: &mut UiCtx,
    spec: &DeviceRow,
    set: impl FnOnce(&mut Settings, Option<String>),
) {
    let options = device_options(
        spec.configured,
        spec.devices,
        spec.list_failed,
        spec.default_label,
    );
    let kind = configured_device(spec.configured, spec.devices.unwrap_or(&[]));
    row(ui, |ui| {
        let label = row_label(ui, spec.label);
        let selected = spec.configured.unwrap_or("");
        if let Some(value) = device_picker(ui, spec.id, label, &options, selected) {
            let value = (!value.is_empty()).then_some(value);
            save(cx, |s| set(s, value));
        }
        if spec.devices.is_some() {
            match kind {
                Some(ConfiguredDevice::Missing) => {
                    badge(ui, BadgeKind::Failed, spec.missing_badge);
                }
                Some(ConfiguredDevice::Legacy(_)) => muted(
                    ui,
                    "Stored by its old friendly name; reselect it to use the stable ID.",
                ),
                _ => {}
            }
        }
        if let Some(help) = spec.help {
            muted(ui, help);
        }
    });
}

// ----------------------------------------------------------------------
// Layout helpers (shared with updates.rs)

/// A settings section card: raised fill, 1 px border, radius 8, padding
/// 14×18, an uppercase muted heading, 14 pt below.
pub(super) fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    widgets::card_frame()
        .inner_margin(Margin::symmetric(18, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut job = egui::text::LayoutJob::default();
            job.append(
                &title.to_uppercase(),
                0.0,
                egui::TextFormat {
                    font_id: theme::semibold(14.0),
                    color: TEXT_MUTED,
                    extra_letter_spacing: 0.5,
                    ..Default::default()
                },
            );
            ui.add(egui::Label::new(job).wrap());
            ui.add_space(2.0);
            add(ui);
        });
    ui.add_space(14.0);
}

/// `.setting-row`: a wrapping row with 10 pt gaps.
pub(super) fn row<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        add(ui)
    })
    .inner
}

/// The width of a row (not what is left of the current line).
fn row_width(ui: &egui::Ui) -> f32 {
    ui.max_rect().width()
}

/// CSS `flex-wrap`: move to a new line unless an item `width` wide fits in
/// what is left of this one. An item wider than the whole row gets a line
/// of its own (and wraps its text there).
pub(super) fn wrap_for(ui: &mut egui::Ui, width: f32) {
    // `available_width()` of a wrapping layout is the whole row; what is
    // left of the current line is the rect before the wrap.
    let at_line_start = ui.cursor().min.x <= ui.max_rect().min.x + 0.5;
    if !at_line_start && width > ui.available_rect_before_wrap().width() {
        ui.end_row();
    }
}

/// Single-line width of `text` in `font`.
fn text_width(ui: &egui::Ui, text: &str, font: egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, TEXT)
        .size()
        .x
}

/// The row's leading label, at least 140 pt wide (a pseudo label column
/// that grows with longer labels, like `min-width`). Returns the label's
/// id, so a control can name itself after it (`<label for=…>`) through
/// `Response::labelled_by`.
pub(super) fn row_label(ui: &mut egui::Ui, text: &str) -> egui::Id {
    let needed = text_width(ui, text, theme::regular(theme::BASE_SIZE));
    let width = needed.max(LABEL_WIDTH).min(row_width(ui));
    ui.allocate_ui_with_layout(
        egui::vec2(width, ui.spacing().interact_size.y),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.add(egui::Label::new(RichText::new(text).color(TEXT)).wrap())
                .id
        },
    )
    .inner
}

/// Inline muted text (`span.muted`): next to the previous item when it
/// fits, otherwise on its own line.
pub(super) fn muted(ui: &mut egui::Ui, text: &str) {
    let width = text_width(ui, text, theme::regular(theme::BASE_SIZE));
    wrap_for(ui, width);
    ui.add(egui::Label::new(RichText::new(text).color(TEXT_MUTED)).wrap());
}

/// A row's value text: beside the label when it fits, else on its own line.
fn value(ui: &mut egui::Ui, text: &str) {
    wrap_for(ui, text_width(ui, text, theme::regular(theme::BASE_SIZE)));
    ui.add(egui::Label::new(RichText::new(text).color(TEXT)).wrap());
}

/// A muted help paragraph (`p.muted`).
pub(super) fn muted_paragraph(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.add(egui::Label::new(RichText::new(text).color(TEXT_MUTED)).wrap());
    ui.add_space(4.0);
}

/// `<code class="path">`: 12 pt monospace on the page background.
fn path_code(ui: &mut egui::Ui, text: &str) {
    let font = egui::FontId::monospace(12.0);
    wrap_for(ui, text_width(ui, text, font.clone()) + 16.0);
    egui::Frame::new()
        .fill(BG)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(text).font(font).color(TEXT)).wrap());
        });
}

/// The engine row's inline `.inline-warning` (sized to its text).
fn warning_chip(ui: &mut egui::Ui, text: &str) {
    wrap_for(
        ui,
        text_width(ui, text, theme::regular(theme::BASE_SIZE)) + 22.0,
    );
    egui::Frame::new()
        .fill(theme::WARN_BG)
        .stroke(egui::Stroke::new(1.0, theme::WARN_BORDER))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(text).color(theme::WARN)).wrap());
        });
}

/// Width of a `.btn` labelled `text` (padding 6/14 plus the border).
fn button_width(ui: &egui::Ui, text: &str) -> f32 {
    text_width(ui, text, theme::regular(theme::BASE_SIZE)) + 30.0
}

/// `.btn.btn-ghost` that moves to a new line when it does not fit.
pub(super) fn ghost(ui: &mut egui::Ui, text: &str) -> egui::Response {
    wrap_for(ui, button_width(ui, text));
    widgets::btn_ghost(ui, text)
}

/// [`ghost`], disabled unless `enabled`.
pub(super) fn ghost_enabled(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    wrap_for(ui, button_width(ui, text));
    ui.add_enabled_ui(enabled, |ui| widgets::btn_ghost(ui, text))
        .inner
}

/// The primary `.btn` that moves to a new line when it does not fit.
pub(super) fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
    wrap_for(ui, button_width(ui, text));
    widgets::btn(ui, text)
}

/// A `.badge` that moves to a new line when it does not fit.
pub(super) fn badge(ui: &mut egui::Ui, kind: BadgeKind, text: &str) -> egui::Response {
    let upper = text.to_uppercase();
    let width =
        text_width(ui, &upper, theme::regular(11.0)) + 0.4 * upper.chars().count() as f32 + 16.0;
    wrap_for(ui, width);
    widgets::badge(ui, kind, text)
}

/// A `.text-input` width: 220–`max` pt, never wider than the row.
fn input_width(ui: &egui::Ui, max: f32) -> f32 {
    max.min(row_width(ui)).max(INPUT_WIDTH.min(row_width(ui)))
}

/// A device `<select>` named by the row label `label`; returns the newly
/// picked value ("" = default).
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
    // Sized to the selected text (min 220, like `.text-input`), truncated
    // at the row width; the button plus its arrow need ~40 pt more.
    let needed = text_width(ui, label, theme::regular(theme::BASE_SIZE)) + 40.0;
    let width = needed.max(INPUT_WIDTH).min(row_width(ui));
    wrap_for(ui, width);
    let mut picked = None;
    // A fixed-width child keeps the truncation inside `width`.
    ui.allocate_ui(egui::vec2(width, ui.spacing().interact_size.y), |ui| {
        egui::ComboBox::from_id_salt(id)
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

fn hotkeys_text(record: Option<&str>, bookmark: Option<&str>) -> String {
    let record = match record {
        Some(combo) => format!(
            "Global hotkey: {} starts/stops recording.",
            hotkey_label(combo)
        ),
        None => "Global record hotkey unavailable (all candidate combos are taken by other apps)."
            .into(),
    };
    let bookmark = match bookmark {
        Some(combo) => format!("{} bookmarks the current moment.", hotkey_label(combo)),
        None => {
            "Global bookmark hotkey unavailable (all candidate combos are taken by other apps)."
                .into()
        }
    };
    format!("{record} {bookmark}")
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
/// rewrote the value untouched. The slider already *shows* the snapped
/// value, so a real edit always lands on a different grid position.
fn threshold_edit(shown: f32, value: f32, changed: bool) -> Option<f32> {
    let snapped = snap_threshold(value);
    (changed && snapped != snap_threshold(shown)).then_some(snapped)
}

fn threshold_readout(t: f32) -> String {
    format!("{t:.2} — lower = more auto-labels (more mistakes), higher = fewer")
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
    fn hotkey_paragraph() {
        assert_eq!(
            hotkeys_text(Some("ctrl+alt+r"), Some("ctrl+alt+b")),
            "Global hotkey: CTRL + ALT + R starts/stops recording. CTRL + ALT + B bookmarks the current moment."
        );
        assert_eq!(
            hotkeys_text(None, None),
            "Global record hotkey unavailable (all candidate combos are taken by other apps). \
             Global bookmark hotkey unavailable (all candidate combos are taken by other apps)."
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
        assert_eq!(
            threshold_readout(0.6),
            "0.60 — lower = more auto-labels (more mistakes), higher = fewer"
        );
        assert_eq!(threshold_value_text(0.5), "0.50 — lenient");
        assert_eq!(threshold_value_text(0.55), "0.55 — balanced");
        assert_eq!(threshold_value_text(0.7), "0.70 — strict");
    }

    #[test]
    fn slider_rounding_an_off_grid_value_is_not_an_edit() {
        // egui snapped a hand-edited 0.62 to 0.60 while drawing and
        // reported a change: not the user's doing, so nothing is saved.
        assert_eq!(threshold_edit(0.62, 0.6, true), None);
        assert_eq!(threshold_edit(0.6, 0.6, true), None);
        assert_eq!(threshold_edit(0.62, 0.65, false), None);
        // Real edits, also away from an off-grid start.
        assert_eq!(threshold_edit(0.62, 0.650_000_1, true), Some(0.65));
        assert_eq!(threshold_edit(0.62, 0.55, true), Some(0.55));
        assert_eq!(threshold_edit(0.6, 0.8, true), Some(0.8));
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

    /// The section helpers lay out in a themed context without panicking.
    #[test]
    fn layout_helpers_render() {
        let ctx = theme::test_context();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                section(ui, "Models", |ui| {
                    row(ui, |ui| {
                        let label = row_label(ui, "Parakeet TDT 0.6B v3 (ONNX)");
                        path_code(ui, "C:\\Users\\me\\AppData\\Local\\Witness");
                        warning_chip(ui, "model not downloaded — transcription will fail");
                        let options = device_options(None, Some(&[]), false, "Default");
                        assert_eq!(device_picker(ui, "test-picker", label, &options, ""), None);
                        muted(ui, "muted");
                    });
                    muted_paragraph(ui, "paragraph");
                });
            });
        });
        output.textures_delta.clear(); // no renderer to upload the font atlas
    }
}
