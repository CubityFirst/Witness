//! `WitnessApp`: the eframe app (design §4.2).
//!
//! `logic()` runs on every repaint — shown, hidden or minimized — so it owns
//! everything that must keep working without a visible window: draining the
//! core/op/shell channel, the tray, the captions overlay's lifecycle, the
//! notes autosave deadline, close-to-tray and the exit paths. `ui()` only
//! draws (top bar, banners, the active view, modal, toasts, overlay) and
//! records what the user asked for.
//!
//! Exit paths (design §4.5) are split into a reversible `prepare_exit` and
//! an irreversible `teardown_and_exit`, so a failed restart spawn or
//! installer launch leaves the app fully running.

use crate::bridge::{wake_root, OpDone, RecOrigin, Tasks, UiMsg, UiSink};
use crate::model::{Effect, ShellModel, UpdatePhase};
use crate::platform::hotkeys::Hotkeys;
use crate::platform::single_instance::InstanceGuard;
use crate::platform::system;
use crate::platform::tray::Tray;
use crate::platform::window::{FlyoutState, MainWindow, StartupVisibility};
use crate::platform::ShellEvent;
use crate::settings_store::SettingsStore;
use crate::ui::captions::{CaptionsOverlay, OpenReason};
use crate::ui::confirm::Confirm;
use crate::ui::fonts::FontLoader;
use crate::ui::toasts::{Notice, ToastAction, Toasts};
use crate::ui::{
    icons, submit_recording, theme, topbar, views, widgets, ActiveView, Nav, UiCmd, UiCtx, View,
    ViewMemory,
};
use crate::ui_state::{PrefsWriter, UiPrefs};
use crossbeam_channel::Receiver;
use eframe::egui::{self, Event, Key, Margin, Modifiers};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use witness_core::player::Player;
use witness_core::service::PreparedUpdate;
use witness_core::settings::Engine;
use witness_core::{AppCore, CoreEvent, ExitPurpose, RecCmd};

/// How long `ensure_ui_pass` may wait for the overlay before giving up.
const UI_PASS_TIMEOUT: Duration = Duration::from_secs(2);
/// Animation frame interval while something visible animates.
const ANIMATION_FRAME: Duration = Duration::from_millis(33);
/// How long exit paths wait for the notes writer.
const NOTES_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long exit paths wait for the ops their own `flush` started (a
/// Meetings rename commit).
const OPS_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long exit paths wait for a pending settings write.
const SETTINGS_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// How soon a minimize the app performed itself must show up in the
/// viewport state to be recognised as such.
const APP_MINIMIZE_WINDOW: Duration = Duration::from_secs(1);
/// Keyboard zoom step (WebView2 used ~10 % steps).
const ZOOM_STEP: f32 = 1.1;
/// Delay before changed `UiPrefs` are written, so a volume drag (a change
/// every frame) writes at most twice a second.
const PREFS_SAVE_DELAY: Duration = Duration::from_millis(500);
/// How often the Settings view re-reads `pending_update` (the startup check
/// stores its result without an event).
const PENDING_UPDATE_POLL: Duration = Duration::from_secs(5);

/// What `main.rs` hands the app creator.
pub struct StartParts {
    pub core: Arc<AppCore>,
    pub sink: UiSink,
    pub rx: Receiver<UiMsg>,
    pub instance_guard: InstanceGuard,
    /// Started with `--minimized` (autostart): never show a window.
    pub minimized: bool,
}

/// The OS-shell objects released at the point of no return of an exit path.
pub struct ShellObjects {
    /// None if the notification-area icon could not be created.
    pub tray: Option<Tray>,
    pub hotkeys: Hotkeys,
    pub instance_guard: InstanceGuard,
}

pub struct WitnessApp {
    core: Arc<AppCore>,
    tasks: Tasks,
    rx: Receiver<UiMsg>,
    /// Taken at the point of no return (`teardown_and_exit`).
    shell_objects: Option<ShellObjects>,
    window: MainWindow,
    flyout: FlyoutState,
    startup: StartupVisibility,
    shell: ShellModel,
    settings: SettingsStore,
    toasts: Toasts,
    confirm: Confirm,
    nav: Nav,
    view: View,
    active: ActiveView,
    memory: ViewMemory,
    prefs: UiPrefs,
    /// What is on disk (or queued for the writer); `prefs` differing from
    /// it starts the save debounce.
    saved_prefs: UiPrefs,
    prefs_deadline: Option<Instant>,
    prefs_writer: Arc<PrefsWriter>,
    player: Player,
    captions: CaptionsOverlay,
    fonts: FontLoader,
    cmds: Vec<UiCmd>,
    data_dir: PathBuf,

    // Effect coalescing: ≤1 op in flight per kind + a dirty flag.
    status_in_flight: bool,
    status_dirty: bool,
    models_in_flight: bool,
    models_dirty: bool,
    /// A top-bar Start/Stop reply arrived: clear `shell.recording_action`
    /// once a status read issued *after* it lands (app.tsx awaited
    /// `refreshStatus()` before `setRecordingAction(false)`), so the bar
    /// never offers the pre-click action again for a few frames.
    action_clear_pending: bool,
    /// The status read in flight was issued after that reply.
    action_clear_on_status: bool,

    /// Tray Quit is running (shutdown in flight).
    quitting: bool,
    /// Last `logic()`'s minimized / focused state (edge detection).
    was_minimized: bool,
    /// The app minimized the window itself — the minimize button (which
    /// already ran `will_hide`) or `finish_ui_pass` re-minimizing after the
    /// overlay's pass — until this deadline: the next not-minimized →
    /// minimized edge is not an outside minimize. A deadline rather than a
    /// flag, so a minimize that never shows up cannot swallow a later real
    /// one.
    minimized_by_app: Option<Instant>,
    was_focused: bool,
    /// Since when `ensure_ui_pass` has been waiting for the overlay.
    ui_pass_since: Option<Instant>,
    /// Ctrl +/−/0 and Ctrl+wheel zoom of the main window.
    user_zoom: f32,
    /// The Windows "Text size" setting (re-read on focus gain).
    text_scale: f32,
    /// Debug builds: Ctrl+Alt+Shift+F12 panics (WITNESS_DEBUG_PANIC_HOTKEY=1).
    debug_panic_hotkey: bool,
}

impl WitnessApp {
    /// The app creator's body (design §4.1 step 9). Runs on the main thread
    /// after the event loop started and before the first frame.
    pub fn new(cc: &eframe::CreationContext<'_>, parts: StartParts) -> anyhow::Result<WitnessApp> {
        let StartParts {
            core,
            sink,
            rx,
            instance_guard,
            minimized,
        } = parts;
        let ctx = cc.egui_ctx.clone();

        theme::install(&ctx);
        if system::high_contrast_on() {
            log::info!(
                "Windows high contrast is on; the custom dark palette does not follow it yet"
            );
        }
        sink.set_context(ctx.clone());

        let window = MainWindow::from_creation_context(cc)?;
        let tray = match Tray::build(sink.clone()) {
            Ok(tray) => {
                crate::panic::register_tray(tray.notify_ids());
                Some(tray)
            }
            Err(error) => {
                log::error!("could not create the tray icon: {error:#}");
                None
            }
        };
        let hotkeys = Hotkeys::register(sink.clone());
        core.set_bound_hotkeys(hotkeys.record_combo(), hotkeys.bookmark_combo());
        instance_guard.attach(sink.clone());
        let startup = StartupVisibility::new(minimized, &window);

        let data_dir = core.runtime_data_dir();
        let prefs = UiPrefs::load(&data_dir);
        let repaint_ctx = ctx.clone();
        let player = Player::spawn(Arc::new(move || wake_root(&repaint_ctx)));

        let tasks = Tasks::new(core.clone(), ctx.clone(), &sink);
        let mut shell = ShellModel::new();
        shell.record_hotkey = hotkeys.record_combo();
        shell.bookmark_hotkey = hotkeys.bookmark_combo();
        shell.update.init(&core);
        let settings = SettingsStore::new(&core);
        let view = crate::ui::debug_start_view();
        let active = ActiveView::for_view(&view);

        let mut app = WitnessApp {
            captions: CaptionsOverlay::new(&data_dir),
            core,
            tasks,
            rx,
            shell_objects: Some(ShellObjects {
                tray,
                hotkeys,
                instance_guard,
            }),
            window,
            flyout: FlyoutState::new(),
            startup,
            shell,
            settings,
            toasts: Toasts::new(),
            confirm: Confirm::new(),
            nav: Nav::default(),
            view,
            active,
            memory: ViewMemory::default(),
            saved_prefs: prefs.clone(),
            prefs_deadline: None,
            prefs_writer: Arc::new(PrefsWriter::default()),
            prefs,
            player,
            fonts: FontLoader::new(),
            cmds: Vec::new(),
            data_dir,
            status_in_flight: false,
            status_dirty: false,
            models_in_flight: false,
            models_dirty: false,
            action_clear_pending: false,
            action_clear_on_status: false,
            quitting: false,
            was_minimized: false,
            minimized_by_app: None,
            was_focused: false,
            ui_pass_since: None,
            user_zoom: 1.0,
            text_scale: system::text_scale_factor(),
            debug_panic_hotkey: cfg!(debug_assertions)
                && std::env::var_os("WITNESS_DEBUG_PANIC_HOTKEY").is_some_and(|v| v == "1"),
        };
        app.apply_zoom(&ctx);
        app.debug_surfaces();
        // Mount-time calls (app.tsx:193-215).
        let mut effects = vec![Effect::RefreshStatus];
        if !app.prefs.model_banner_dismissed {
            effects.push(Effect::RecheckModels);
        }
        app.run_effects(effects);
        Ok(app)
    }

    /// Build the per-pass view context and run `f` with the active view.
    fn with_view<R>(&mut self, f: impl FnOnce(&mut ActiveView, &mut UiCtx) -> R) -> R {
        let mut cx = UiCtx {
            core: &self.core,
            tasks: &self.tasks,
            shell: &mut self.shell,
            settings: &mut self.settings,
            toasts: &mut self.toasts,
            confirm: &mut self.confirm,
            nav: &mut self.nav,
            view: &self.view,
            player: &self.player,
            prefs: &mut self.prefs,
            memory: &mut self.memory,
            window: &self.window,
            flyout: &mut self.flyout,
            cmds: &mut self.cmds,
            fonts: &self.fonts,
        };
        f(&mut self.active, &mut cx)
    }

    /// The single hook for every way the main window stops being visible:
    /// enqueue the newest notes draft. Never blocks; the player keeps playing.
    fn will_hide(&mut self) {
        self.with_view(|view, cx| view.flush(cx));
    }

    // ------------------------------------------------------------------
    // Messages

    fn handle_msg(&mut self, ctx: &egui::Context, msg: UiMsg) {
        match msg {
            UiMsg::Core(event) => self.handle_core(ctx, event),
            UiMsg::Op(done) => self.handle_op(done),
            UiMsg::Shell(event) => self.handle_shell(event),
        }
    }

    fn handle_core(&mut self, ctx: &egui::Context, event: CoreEvent) {
        let effects = self.shell.apply(&event);
        self.run_effects(effects);
        match event {
            CoreEvent::TrayRecording(recording) => {
                if let Some(tray) = self.tray_mut() {
                    tray.set_recording(recording);
                }
            }
            CoreEvent::TrayWarning(warning) => {
                if let Some(tray) = self.tray_mut() {
                    tray.set_warning(warning);
                }
            }
            CoreEvent::OpenCaptionsOverlay { meeting_id } => {
                self.captions.open(meeting_id, OpenReason::Auto);
                self.ensure_ui_pass();
            }
            CoreEvent::CloseCaptionsOverlay => self.captions.close(&self.data_dir),
            CoreEvent::RecordingStarted(started) => self.captions.reset_for(started.meeting_id),
            CoreEvent::LiveTranscript(line) => {
                self.fonts.note_texts(ctx, [line.text.as_str()]);
                self.captions.push_line(&line);
            }
            // captions.tsx: `setError(status.error ?? "Live captions stopped")`.
            // Always `Some`, so the overlay never keeps saying "Listening…"
            // after captions died without a reason; the banner is cleared
            // only by `reset_for` / `open`.
            CoreEvent::LiveCaptionsStatus(status) if !status.active => {
                self.captions.set_error(Some(
                    status
                        .error
                        .unwrap_or_else(|| "Live captions stopped".into()),
                ));
            }
            _ => {}
        }
    }

    fn tray_mut(&mut self) -> Option<&mut Tray> {
        self.shell_objects.as_mut().and_then(|s| s.tray.as_mut())
    }

    fn run_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Notice(notice) => self.toasts.push(notice),
                Effect::RefreshStatus => self.request_status(),
                Effect::RecheckModels => self.request_models(),
            }
        }
    }

    /// `refreshStatus()`: at most one status read in flight, plus a dirty flag.
    fn request_status(&mut self) {
        if self.status_in_flight {
            self.status_dirty = true;
            return;
        }
        self.status_in_flight = true;
        self.action_clear_on_status |= std::mem::take(&mut self.action_clear_pending);
        self.tasks.spawn_op_or(
            "status",
            |c| OpDone::Status(c.status()),
            OpDone::StatusFailed,
        );
    }

    /// Re-check the transcription models (first-run banner), coalesced.
    fn request_models(&mut self) {
        if self.models_in_flight {
            self.models_dirty = true;
            return;
        }
        self.models_in_flight = true;
        self.tasks.spawn_op_or(
            "model-status",
            |c| OpDone::Models(c.model_status()),
            OpDone::ModelsFailed,
        );
    }

    /// A status read finished (or failed): end a pending top-bar spinner,
    /// then issue the read that was asked for meanwhile.
    fn status_landed(&mut self) {
        self.status_in_flight = false;
        if std::mem::take(&mut self.action_clear_on_status) {
            self.shell.recording_action = false;
        }
        if std::mem::take(&mut self.status_dirty) {
            self.request_status();
        }
    }

    fn handle_op(&mut self, done: OpDone) {
        match done {
            OpDone::Notice { notice, refresh } => {
                if let Some(notice) = notice {
                    self.toasts.push(notice);
                }
                if refresh {
                    self.shell.refresh_tick += 1;
                }
            }
            OpDone::Status(status) => {
                self.shell.set_status(status);
                self.status_landed();
            }
            OpDone::StatusFailed(error) => {
                log::error!("status refresh failed: {error}");
                self.status_landed();
            }
            OpDone::Models(models) => {
                self.shell.models_missing = models
                    .iter()
                    .any(|m| m.engine == Engine::Parakeet && !m.present);
                self.models_in_flight = false;
                if std::mem::take(&mut self.models_dirty) {
                    self.request_models();
                }
            }
            OpDone::ModelsFailed(error) => {
                log::error!("model check failed: {error}");
                self.models_in_flight = false;
                if std::mem::take(&mut self.models_dirty) {
                    self.request_models();
                }
            }
            OpDone::SettingsWritten { seq, result } => {
                let outcome = self
                    .settings
                    .on_written(seq, result, &mut self.toasts, &self.tasks);
                if outcome.data_dir_saved {
                    self.shell.settings_tick += 1;
                }
            }
            OpDone::UpdateChecked(_) => {
                for notice in self.shell.update.on_op(&done) {
                    self.toasts.push(notice);
                }
            }
            OpDone::UpdateDownloaded(result) => match result {
                Ok(prepared) => self.install_update(prepared),
                Err(error) => {
                    let failed = OpDone::UpdateDownloaded(Err(error));
                    for notice in self.shell.update.on_op(&failed) {
                        self.toasts.push(notice);
                    }
                }
            },
            OpDone::RestartPrepared(Ok(())) => self.restart(),
            OpDone::RestartPrepared(Err(error)) => {
                self.toasts.error("Could not restart Witness", error);
            }
            OpDone::ShutdownDone => {
                log::info!("recording control shut down; exiting");
                self.prepare_exit();
                self.exit_now(0);
            }
            OpDone::Recording {
                origin,
                cmd,
                result,
            } => self.on_recording_reply(origin, cmd, result),
            OpDone::CaptionsCheck(Ok(())) => self.cmds.push(UiCmd::ShowCaptionsOverlay),
            OpDone::CaptionsCheck(Err(error)) => {
                self.toasts
                    .error("Could not show the caption overlay", error);
            }
        }
    }

    fn on_recording_reply(
        &mut self,
        origin: RecOrigin,
        cmd: RecCmd,
        result: witness_core::CoreResult<witness_core::RecOutcome>,
    ) {
        match origin {
            RecOrigin::TopBar => match cmd {
                RecCmd::BookmarkNow => match result {
                    Ok(_) => self.toasts.success("Bookmark added"),
                    Err(error) => self.toasts.error("Could not add bookmark", error),
                },
                RecCmd::Start(_) | RecCmd::Stop { .. } | RecCmd::ToggleWithToast => {
                    match result {
                        // Keep the spinner until the refreshed status shows
                        // the new state (cleared in `status_landed`).
                        Ok(_) => self.action_clear_pending = true,
                        Err(error) => {
                            self.shell.recording_action = false;
                            let message = if matches!(cmd, RecCmd::Stop { .. }) {
                                "Could not stop recording"
                            } else {
                                "Could not start recording"
                            };
                            self.toasts.error(message, error);
                        }
                    }
                    self.request_status();
                }
            },
            // The core already OS-toasted tray / hotkey failures.
            RecOrigin::Tray | RecOrigin::Hotkey => match result {
                Err(error) if error == "Not recording" => {
                    log::debug!("{origin:?} {cmd:?} ignored: not recording");
                }
                Err(error) => log::warn!("{origin:?} {cmd:?} failed: {error}"),
                Ok(outcome) => log::debug!("{origin:?} {cmd:?}: {outcome:?}"),
            },
        }
    }

    fn handle_shell(&mut self, event: ShellEvent) {
        if self.quitting && ignored_while_quitting(&event) {
            // Quit's stop is queued (or running) in rec-control; a toggle
            // behind it would start a recording that exit then kills.
            log::debug!("{event:?} ignored: quitting");
            return;
        }
        match event {
            ShellEvent::TrayClick { x, y } => self.flyout.open(&self.window, x, y),
            ShellEvent::TrayMenuOpen | ShellEvent::SecondInstance => {
                self.flyout.close_mode();
                self.window.show_normal();
            }
            ShellEvent::TrayMenuToggleRecord => {
                submit_recording(&self.tasks, RecCmd::ToggleWithToast, RecOrigin::Tray);
            }
            ShellEvent::HotkeyRecord => {
                submit_recording(&self.tasks, RecCmd::ToggleWithToast, RecOrigin::Hotkey);
            }
            ShellEvent::HotkeyBookmark => {
                submit_recording(&self.tasks, RecCmd::BookmarkNow, RecOrigin::Hotkey);
            }
            ShellEvent::TrayMenuQuit => self.begin_quit(),
        }
        self.shell.popup_mode = self.flyout.is_popup();
    }

    /// Debug builds only, for passive visual checks (no input needed):
    /// `WITNESS_DEBUG_TOASTS=1` shows one toast of each kind and
    /// `WITNESS_DEBUG_CONFIRM=1` opens a sample confirm (its answer is
    /// ignored). See also `WITNESS_DEBUG_START_VIEW` (`ui::debug_start_view`).
    fn debug_surfaces(&mut self) {
        #[cfg(debug_assertions)]
        {
            let on = |name: &str| std::env::var_os(name).is_some_and(|v| v == "1");
            if on("WITNESS_DEBUG_TOASTS") {
                self.toasts.push(Notice::info("Debug: an info toast"));
                self.toasts.success("Debug: a success toast");
                self.toasts
                    .error("Debug: an error toast", "with its reason after the colon");
            }
            if on("WITNESS_DEBUG_CONFIRM") {
                self.confirm.ask(
                    egui::Id::new("debug-confirm"),
                    "Debug: move this meeting to the recycle bin?",
                    "Delete",
                );
            }
        }
    }

    /// App-level commands pushed by views during `ui()`.
    fn process_cmds(&mut self) {
        for cmd in std::mem::take(&mut self.cmds) {
            match cmd {
                UiCmd::ShowCaptionsOverlay => {
                    let meeting = self
                        .shell
                        .status
                        .as_ref()
                        .filter(|s| s.recording)
                        .and_then(|s| s.meeting_id)
                        .or(self.shell.active_recording_id);
                    if let Some(meeting_id) = meeting {
                        self.captions.open(meeting_id, OpenReason::User);
                        self.ensure_ui_pass();
                    }
                }
                UiCmd::Restart => {
                    self.tasks.spawn_op_or(
                        "restart",
                        |c| OpDone::RestartPrepared(c.prepare_restart()),
                        |error| OpDone::RestartPrepared(Err(error)),
                    );
                }
                UiCmd::CheckUpdate => self.shell.update.check(&self.tasks),
                UiCmd::InstallUpdate => self.shell.update.install(&self.tasks),
            }
        }
    }

    /// The app is about to minimize the window itself; see `minimized_by_app`.
    fn expect_app_minimize(&mut self) {
        self.minimized_by_app = Some(Instant::now() + APP_MINIMIZE_WINDOW);
    }

    /// While minimized, a deferred viewport can only be created after the
    /// root is briefly restored (cloaked) for one pass (design §4.3, R1).
    fn ensure_ui_pass(&mut self) {
        self.window.ensure_ui_pass();
        if self.window.ui_pass_in_progress() {
            self.ui_pass_since = Some(Instant::now());
        }
    }

    // ------------------------------------------------------------------
    // Exit paths (design §4.5)

    /// Tray Quit: stop any recording through rec-control (behind a queued
    /// toggle), then exit on `ShutdownDone`.
    fn begin_quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.shell.quitting = true;
        log::info!("quit requested");
        // Quit is irreversible (a failed shutdown still exits), so refuse
        // second launches from now on: a launch during the stop below (up to
        // SHUTDOWN_WAIT) keeps waiting for the mutex and takes over, instead
        // of being answered "shown" by a process about to disappear.
        if let Some(shell) = &self.shell_objects {
            shell.instance_guard.begin_exit();
        }
        // A failed shutdown (logged by `spawn_op_or`) still exits: the user
        // asked to quit, and startup recovery finishes an interrupted stop.
        self.tasks.spawn_op_or(
            "quit",
            |c| {
                c.shutdown();
                OpDone::ShutdownDone
            },
            |_| OpDone::ShutdownDone,
        );
    }

    /// Reversible: safe to run and then keep running if a later step fails.
    fn prepare_exit(&mut self) {
        let mark = self.tasks.op_mark();
        self.will_hide();
        if !self.tasks.wait_ops_since(mark, OPS_FLUSH_TIMEOUT) {
            log::warn!("exiting with an operation still running");
        }
        self.core.flush_notes(NOTES_FLUSH_TIMEOUT);
        // A queued settings write only starts from `on_written`, which never
        // runs once the process has exited.
        self.settings
            .flush_for_exit(&self.core, SETTINGS_FLUSH_TIMEOUT);
        // Persists the overlay's placement. Restart and update are refused
        // while recording, so no overlay a failed step would need is lost.
        self.captions.close(&self.data_dir);
        // The player is left alone: a failed restart/installer launch keeps
        // the app running, playback included (`exit_now` stops it).
        self.prefs_writer.write_now(&self.prefs, &self.data_dir);
        self.saved_prefs = self.prefs.clone();
        self.prefs_deadline = None;
    }

    /// The point of no return after `prepare_exit` succeeded.
    fn exit_now(&mut self, code: i32) -> ! {
        self.player.unload();
        teardown_and_exit(self.shell_objects.take(), code)
    }

    /// Persist `UiPrefs` shortly after they change (views mutate
    /// `cx.prefs` directly), off the UI thread.
    fn persist_prefs(&mut self, ctx: &egui::Context) {
        if self.prefs == self.saved_prefs {
            self.prefs_deadline = None;
            return;
        }
        let now = Instant::now();
        let due = *self.prefs_deadline.get_or_insert(now + PREFS_SAVE_DELAY);
        if now < due {
            ctx.request_repaint_after(due - now);
            return;
        }
        self.prefs_deadline = None;
        self.saved_prefs = self.prefs.clone();
        self.prefs_writer.queue(self.prefs.clone());
        let writer = self.prefs_writer.clone();
        let data_dir = self.data_dir.clone();
        self.tasks
            .fire("ui-prefs-write", move |_| writer.write_pending(&data_dir));
    }

    /// Restart after a data_dir change: spawn the new process first; only on
    /// success release the shell and exit.
    fn restart(&mut self) {
        let core = self.core.clone();
        let guard = match core.exit_guard(ExitPurpose::Restart) {
            Ok(guard) => guard,
            Err(error) => {
                self.toasts.error("Could not restart Witness", error);
                return;
            }
        };
        self.prepare_exit();
        let spawned = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .args(core.relaunch_args())
                .spawn()
        });
        match spawned {
            Ok(_) => {
                log::info!("restarting Witness");
                self.exit_now(0);
            }
            Err(error) => {
                drop(guard);
                self.toasts.error("Could not restart Witness", error);
            }
        }
    }

    /// Launch the verified installer; on success release the shell and exit
    /// (the installer waits for, or kills, this process before copying).
    fn install_update(&mut self, prepared: PreparedUpdate) {
        let core = self.core.clone();
        let guard = match core.exit_guard(ExitPurpose::Update) {
            Ok(guard) => guard,
            Err(error) => {
                self.toasts.error("Could not install the update", error);
                self.shell.update.install_failed();
                return;
            }
        };
        self.prepare_exit();
        match prepared.launch(&core.relaunch_args()) {
            Ok(()) => {
                log::info!("installing Witness {}", prepared.version);
                self.exit_now(0);
            }
            Err(error) => {
                drop(guard);
                // Same wording as 0.2.x: the caller's prefix around the
                // core's "Installing the update failed: …".
                self.toasts.error(
                    "Could not install the update",
                    format!("Installing the update failed: {error}"),
                );
                self.shell.update.install_failed();
            }
        }
    }

    // ------------------------------------------------------------------
    // Input

    fn apply_zoom(&self, ctx: &egui::Context) {
        ctx.set_zoom_factor((self.user_zoom * self.text_scale).clamp(0.5, 3.0));
    }

    /// Root-only zoom: Ctrl + `=` / `−` / `0` and Ctrl + mouse wheel.
    fn handle_zoom(&mut self, ctx: &egui::Context) {
        let (zoom_in, zoom_out, reset, wheel) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND, Key::Equals)
                    || i.consume_key(Modifiers::COMMAND, Key::Plus),
                i.consume_key(Modifiers::COMMAND, Key::Minus),
                i.consume_key(Modifiers::COMMAND, Key::Num0),
                i.zoom_delta(),
            )
        });
        let before = self.user_zoom;
        if zoom_in {
            self.user_zoom *= ZOOM_STEP;
        }
        if zoom_out {
            self.user_zoom /= ZOOM_STEP;
        }
        if reset {
            self.user_zoom = 1.0;
        }
        if wheel != 1.0 {
            self.user_zoom *= wheel;
        }
        // Keep the user factor where the effective zoom can still move.
        self.user_zoom = self
            .user_zoom
            .clamp(0.5 / self.text_scale, 3.0 / self.text_scale);
        if self.user_zoom != before {
            self.apply_zoom(ctx);
        }
    }

    /// Ctrl+F anywhere, or `/` outside a text field: go to the search box.
    /// Modifiers are matched exactly as app.tsx does (no Alt for either, no
    /// Ctrl for `/`): egui's `consume_key` ignores an extra Alt, which would
    /// steal Ctrl+Alt+F / AltGr+F and Alt+/.
    fn handle_search_shortcuts(&mut self, ctx: &egui::Context) {
        // The confirmation is modal: keys do not reach the page behind it.
        if self.confirm.is_open() {
            return;
        }
        let typing = ctx.egui_wants_keyboard_input();
        let jump = ctx.input_mut(|i| {
            let ctrl_f = consume_key_exact(i, Key::F, |m| m.command && !m.alt);
            let slash = !typing && consume_key_exact(i, Key::Slash, |m| !m.command && !m.alt);
            ctrl_f || slash
        });
        if jump {
            if !self.view.has_search_box() {
                self.nav.go(View::Meetings);
            }
            self.memory.search_box.request_focus = true;
        }
    }

    /// Apply a pending navigation: `on_leave` the old view, then remount
    /// when the view key changes.
    ///
    /// Always asks for another pass: the views have already drawn this one,
    /// and a navigation fired by a timer (the delayed open in Meetings, the
    /// search debounce) has no input that would schedule a follow-up pass,
    /// so the old view would otherwise stay on screen until the mouse moved.
    fn apply_navigation(&mut self, ctx: &egui::Context) {
        let Some(next) = self.nav.take() else { return };
        if next.key() != self.view.key() {
            // `on_leave` still reads an answer given in this pass; a modal
            // left open would outlive the view that asked (its answer would
            // do nothing), so it closes with the view.
            self.with_view(|view, cx| view.on_leave(cx));
            self.confirm.withdraw();
            if matches!(next, View::Settings) {
                // settings.tsx re-read get_settings on every mount, so it
                // shows the stored (normalized) values after a round trip.
                self.settings.reload(&self.core);
            }
            self.active = ActiveView::for_view(&next);
        }
        self.view = next;
        ctx.request_repaint();
    }

    fn handle_toast_action(&mut self, action: ToastAction) {
        match action {
            ToastAction::RestoreMeeting(id) => {
                self.tasks
                    .spawn_op("restore-meeting", move |c| OpDone::Notice {
                        notice: Some(match c.restore_meeting(id) {
                            Ok(()) => Notice::success("Meeting restored"),
                            Err(error) => Notice::error("Could not restore the meeting", error),
                        }),
                        refresh: true,
                    });
            }
        }
    }
}

/// Shell events dropped once Tray Quit has started: the recording toggles
/// (a toggle queued behind Quit's stop would start a recording the exit then
/// kills unfinalized) and the bookmark hotkey. Opening the window still works.
fn ignored_while_quitting(event: &ShellEvent) -> bool {
    matches!(
        event,
        ShellEvent::TrayMenuToggleRecord | ShellEvent::HotkeyRecord | ShellEvent::HotkeyBookmark
    )
}

/// Consume presses of `key` whose modifiers satisfy `accept`; true if any.
fn consume_key_exact(
    input: &mut egui::InputState,
    key: Key,
    accept: impl Fn(Modifiers) -> bool,
) -> bool {
    let mut found = false;
    input.events.retain(|event| {
        let hit = matches!(
            event,
            Event::Key { key: k, pressed: true, modifiers, .. } if *k == key && accept(*modifiers)
        );
        found |= hit;
        !hit
    });
    found
}

/// Irreversible: release the shell objects and exit. A free function so it
/// can run while an `ExitGuard` borrowed from a cloned `Arc<AppCore>` lives.
fn teardown_and_exit(shell: Option<ShellObjects>, code: i32) -> ! {
    if let Some(shell) = shell {
        // Refuse second launches first: NIM_DELETE below waits on Explorer
        // and would otherwise answer a racing launch "shown".
        shell.instance_guard.begin_exit();
        drop(shell.tray); // NIM_DELETE
        drop(shell.hotkeys); // UnregisterHotKey
        shell.instance_guard.release(); // ReleaseMutex, CloseHandle, DestroyWindow
    }
    log::info!("Witness exiting ({code})");
    log::logger().flush();
    std::process::exit(code)
}

impl eframe::App for WitnessApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.fonts.install_pending(ctx);
        while let Ok(msg) = self.rx.try_recv() {
            self.handle_msg(ctx, msg);
        }
        self.process_cmds();
        self.startup.tick(&self.window, ctx);

        let tick = self.flyout.tick(ctx, &self.window);
        if tick.dismiss {
            self.will_hide();
            self.window.hide_to_tray();
        }
        self.shell.popup_mode = self.flyout.is_popup();

        // Undo `ensure_ui_pass` once the overlay exists (or after 2 s).
        if let Some(since) = self.ui_pass_since {
            let timed_out = since.elapsed() >= UI_PASS_TIMEOUT;
            if self.captions.is_visible() || timed_out {
                if timed_out && !self.captions.is_visible() {
                    log::warn!("captions overlay did not appear within {UI_PASS_TIMEOUT:?}");
                }
                // The window state only reports minimized on a later
                // `logic()`, after `ui_pass_in_progress()` has ended, so the
                // edge detector is told explicitly (design §4.3: no
                // `will_hide` for this internal minimize).
                self.expect_app_minimize();
                self.window.finish_ui_pass();
                self.ui_pass_since = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }

        // A minimize from outside the app (Win+Down, taskbar) hides the window.
        let (minimized, focused, close_requested) = ctx.input(|i| {
            let vp = i.viewport();
            (
                vp.minimized.unwrap_or(false),
                vp.focused.unwrap_or(false),
                vp.close_requested(),
            )
        });
        if minimized && !self.was_minimized {
            let by_app = self
                .minimized_by_app
                .take()
                .is_some_and(|until| Instant::now() <= until);
            if !by_app && !self.window.ui_pass_in_progress() {
                self.will_hide();
            }
        }
        self.was_minimized = minimized;

        // Notes autosave deadline (fires even while minimized).
        if let Some(at) = self.shell.notes_deadline {
            let now = Instant::now();
            if now >= at {
                self.shell.notes_deadline = None;
                self.will_hide();
            } else {
                ctx.request_repaint_after(at - now);
            }
        }

        // Close (× or Alt+F4) hides to the tray; only Quit exits. While a
        // Quit is shutting down the close is still cancelled (eframe ending
        // the loop would skip `prepare_exit`), but the window is left alone.
        if close_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.quitting {
                self.will_hide();
                self.window.hide_to_tray();
            }
        }

        // Settings → Updates: pick up the startup check's result, which is
        // stored without an event (never interrupts a check or install).
        if matches!(self.view, View::Settings) {
            self.shell.update.init(&self.core);
            if matches!(
                self.shell.update.phase,
                UpdatePhase::Idle | UpdatePhase::Current
            ) && self.window.is_shown()
                && !minimized
            {
                ctx.request_repaint_after(PENDING_UPDATE_POLL);
            }
        }

        self.persist_prefs(ctx);

        // Re-read the Windows text size whenever the window gains focus.
        if focused && !self.was_focused {
            let scale = system::text_scale_factor();
            if scale != self.text_scale {
                self.text_scale = scale;
                self.apply_zoom(ctx);
            }
        }
        self.was_focused = focused;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        widgets::track_focus_visible(&ctx);
        if self.debug_panic_hotkey
            && ctx.input(|i| {
                i.modifiers.ctrl && i.modifiers.alt && i.modifiers.shift && i.key_pressed(Key::F12)
            })
        {
            panic!("debug panic hotkey (WITNESS_DEBUG_PANIC_HOTKEY=1)");
        }
        self.handle_zoom(&ctx);
        self.handle_search_shortcuts(&ctx);

        let mut topbar_out = topbar::TopBarOutput::default();
        // F0 → U7/U4 contract: besides the documented `show`, `resize_edges`
        // and the banners, this calls `topbar::frame()` and
        // `topbar::paint_bottom_border(ui, rect)` (U7 keeps both), and it
        // owns the bar's panel and the search box's 900 px column + 12 px
        // gap (U4's `search_box` draws inside it). Changing that layout is
        // an I1 edit here, not a unit edit.
        self.with_view(|view, cx| {
            topbar::resize_edges(ui, cx);
            let bar = egui::Panel::top("topbar")
                .frame(topbar::frame())
                .resizable(false)
                .show_separator_line(false)
                .show(ui, |ui| topbar::show(ui, cx));
            topbar::paint_bottom_border(ui, bar.response.rect);
            topbar_out = bar.inner;
            topbar::health_banner(ui, cx);

            let content = egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(Margin::symmetric(20, 16));
            egui::CentralPanel::default().frame(content).show(ui, |ui| {
                // The banner and search box end where the view's cards do:
                // its scroll bar (when shown) takes that width from both,
                // as it did from the one 0.2.x scroller.
                let right = ui.max_rect().right();
                let gutter = widgets::view_scroll_gutter(ui.ctx(), right);
                widgets::clear_view_viewport(ui.ctx());
                // CSS margins are whole gaps: `.model-banner` 14 below,
                // `.list-toolbar` 12 below. An empty scope would still
                // advance by the item spacing, so it only opens with content.
                let banner = topbar::has_model_banner(cx);
                let search = cx.view.has_search_box();
                if banner || search {
                    ui.scope(|ui| {
                        ui.set_max_width((ui.available_width() - gutter).max(0.0));
                        topbar::model_banner(ui, cx);
                        if search {
                            if banner {
                                widgets::gap(ui, 14.0);
                            }
                            let current = cx.view.clone();
                            widgets::centered_column(ui, 900.0, |ui| {
                                views::search::search_box(ui, cx, &current);
                            });
                        }
                    });
                    widgets::gap(ui, if search { 12.0 } else { 14.0 });
                }
                view.show(ui, cx);
                if (widgets::view_scroll_gutter(ui.ctx(), right) - gutter).abs() > 0.5 {
                    ui.ctx().request_discard("view scroll bar changed");
                }
            });
        });

        self.confirm.show(&ctx);
        if let Some(action) = self.toasts.show(&ctx) {
            self.handle_toast_action(action);
        }
        self.captions.show(&ctx);

        if topbar_out.minimize {
            self.will_hide();
            self.expect_app_minimize();
            self.window.minimize();
        }
        if topbar_out.close {
            self.will_hide();
            self.window.hide_to_tray();
        }
        self.apply_navigation(&ctx);
        // Start the save timer in the pass that changed the prefs.
        self.persist_prefs(&ctx);

        // Never animate a hidden or minimized window (design §4.2).
        if icons::take_animation_request(&ctx)
            && self.window.is_shown()
            && !self.window.is_minimized()
        {
            ctx.request_repaint_after(ANIMATION_FRAME);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::clear_color()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::starts_recording;
    use witness_core::Trigger;

    #[test]
    fn quitting_drops_recording_toggles_but_not_window_events() {
        for event in [
            ShellEvent::TrayMenuToggleRecord,
            ShellEvent::HotkeyRecord,
            ShellEvent::HotkeyBookmark,
        ] {
            assert!(ignored_while_quitting(&event), "{event:?}");
        }
        for event in [
            ShellEvent::TrayClick { x: 1.0, y: 2.0 },
            ShellEvent::TrayMenuOpen,
            ShellEvent::SecondInstance,
            ShellEvent::TrayMenuQuit,
        ] {
            assert!(!ignored_while_quitting(&event), "{event:?}");
        }
    }

    #[test]
    fn only_starting_commands_are_refused_while_quitting() {
        assert!(starts_recording(RecCmd::Start(Trigger::Manual)));
        assert!(starts_recording(RecCmd::ToggleWithToast));
        assert!(!starts_recording(RecCmd::Stop { by_user: true }));
        assert!(!starts_recording(RecCmd::BookmarkNow));
    }
}
