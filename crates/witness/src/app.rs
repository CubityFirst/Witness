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

use crate::bridge::{OpDone, RecOrigin, Tasks, UiMsg, UiSink};
use crate::model::{Effect, ShellModel};
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
use crate::ui_state::UiPrefs;
use crossbeam_channel::Receiver;
use eframe::egui::{self, Key, Margin, Modifiers};
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
/// Keyboard zoom step (WebView2 used ~10 % steps).
const ZOOM_STEP: f32 = 1.1;

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

    /// Tray Quit is running (shutdown in flight).
    quitting: bool,
    /// Last `logic()`'s minimized / focused state (edge detection).
    was_minimized: bool,
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
        let player = Player::spawn(Arc::new(move || repaint_ctx.request_repaint()));

        let tasks = Tasks::new(core.clone(), ctx.clone(), &sink);
        let mut shell = ShellModel::new();
        shell.record_hotkey = hotkeys.record_combo();
        shell.bookmark_hotkey = hotkeys.bookmark_combo();
        shell.update.init(&core);
        let settings = SettingsStore::new(&core);
        let view = View::default();
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
            prefs,
            player,
            fonts: FontLoader::new(),
            cmds: Vec::new(),
            data_dir,
            status_in_flight: false,
            status_dirty: false,
            models_in_flight: false,
            models_dirty: false,
            quitting: false,
            was_minimized: false,
            was_focused: false,
            ui_pass_since: None,
            user_zoom: 1.0,
            text_scale: system::text_scale_factor(),
            debug_panic_hotkey: cfg!(debug_assertions)
                && std::env::var_os("WITNESS_DEBUG_PANIC_HOTKEY").is_some_and(|v| v == "1"),
        };
        app.apply_zoom(&ctx);
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
            CoreEvent::LiveCaptionsStatus(status) if !status.active => {
                self.captions.set_error(status.error);
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
        self.tasks
            .spawn_op("status", |c| OpDone::Status(c.status()));
    }

    /// Re-check the transcription models (first-run banner), coalesced.
    fn request_models(&mut self) {
        if self.models_in_flight {
            self.models_dirty = true;
            return;
        }
        self.models_in_flight = true;
        self.tasks
            .spawn_op("model-status", |c| OpDone::Models(c.model_status()));
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
                self.status_in_flight = false;
                if std::mem::take(&mut self.status_dirty) {
                    self.request_status();
                }
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
                self.prepare_exit();
                teardown_and_exit(self.shell_objects.take(), 0);
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
                    self.shell.recording_action = false;
                    self.request_status();
                    if let Err(error) = result {
                        let message = if matches!(cmd, RecCmd::Stop { .. }) {
                            "Could not stop recording"
                        } else {
                            "Could not start recording"
                        };
                        self.toasts.error(message, error);
                    }
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
                    self.tasks
                        .spawn_op("restart", |c| OpDone::RestartPrepared(c.prepare_restart()));
                }
                UiCmd::CheckUpdate => self.shell.update.check(&self.tasks),
                UiCmd::InstallUpdate => self.shell.update.install(&self.tasks),
                UiCmd::Quit => self.begin_quit(),
            }
        }
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
        self.tasks.spawn_op("quit", |c| {
            c.shutdown();
            OpDone::ShutdownDone
        });
    }

    /// Reversible: safe to run and then keep running if a later step fails.
    fn prepare_exit(&mut self) {
        self.will_hide();
        self.core.flush_notes(NOTES_FLUSH_TIMEOUT);
        self.captions.close(&self.data_dir);
        self.player.unload();
        if let Err(error) = self.prefs.save(&self.data_dir) {
            log::warn!("{error}");
        }
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
                teardown_and_exit(self.shell_objects.take(), 0);
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
                teardown_and_exit(self.shell_objects.take(), 0);
            }
            Err(error) => {
                drop(guard);
                self.toasts.error("Installing the update failed", error);
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
    fn handle_search_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let jump = ctx.input_mut(|i| {
            let ctrl_f = i.consume_key(Modifiers::COMMAND, Key::F);
            let slash = !typing && i.consume_key(Modifiers::NONE, Key::Slash);
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
    fn apply_navigation(&mut self) {
        let Some(next) = self.nav.take() else { return };
        if next.key() != self.view.key() {
            self.with_view(|view, cx| view.on_leave(cx));
            self.active = ActiveView::for_view(&next);
        }
        self.view = next;
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

/// Irreversible: release the shell objects and exit. A free function so it
/// can run while an `ExitGuard` borrowed from a cloned `Arc<AppCore>` lives.
fn teardown_and_exit(shell: Option<ShellObjects>, code: i32) -> ! {
    if let Some(shell) = shell {
        drop(shell.tray); // NIM_DELETE
        drop(shell.hotkeys); // UnregisterHotKey
        shell.instance_guard.release(); // ReleaseMutex, CloseHandle, DestroyWindow
    }
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
        if minimized && !self.was_minimized && !self.window.ui_pass_in_progress() {
            self.will_hide();
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

        // Close (× or Alt+F4) hides to the tray; only Quit exits.
        if close_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.will_hide();
            self.window.hide_to_tray();
        }

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
                topbar::model_banner(ui, cx);
                if cx.view.has_search_box() {
                    let current = cx.view.clone();
                    widgets::centered_column(ui, 900.0, |ui| {
                        views::search::search_box(ui, cx, &current);
                    });
                    ui.add_space(12.0);
                }
                view.show(ui, cx);
            });
        });

        self.confirm.show(&ctx);
        if let Some(action) = self.toasts.show(&ctx) {
            self.handle_toast_action(action);
        }
        self.captions.show(&ctx);

        if topbar_out.minimize {
            self.will_hide();
            self.window.minimize();
        }
        if topbar_out.close {
            self.will_hide();
            self.window.hide_to_tray();
        }
        self.apply_navigation();

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
