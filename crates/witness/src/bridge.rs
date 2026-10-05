//! The UI ↔ core bridge (design §3.4).
//!
//! Everything that reaches the UI thread — core events, finished app-level
//! operations, shell events from the tray / hotkeys / single-instance window
//! — goes through ONE unbounded crossbeam channel of [`UiMsg`], drained in
//! `WitnessApp::logic()` (which runs on every repaint, hidden or minimized).
//! Each send is followed by a repaint request so the drain happens promptly.
//! Those requests always target the ROOT viewport ([`wake_root`]): a plain
//! `request_repaint()` from another thread resolves to whichever viewport is
//! mid-pass, so while the captions overlay renders it would wake only the
//! overlay, and the root's `logic()` would not drain the channel.
//!
//! Work never runs on the UI thread:
//! - **Loads** (`Tasks::spawn`) return a view-owned [`Task`]; dropping the
//!   task discards a stale result, like the TSX generation counters.
//! - **Actions** (`Tasks::spawn_op`) return an [`OpDone`] that is handled
//!   globally in `logic()`, so its toast / refresh survives navigation and a
//!   hidden window, like the TSX module-global notify store.

use crate::platform::ShellEvent;
use crate::ui::toasts::Notice;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use eframe::egui;
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use witness_core::models::ModelInfo;
use witness_core::service::{AppStatus, PreparedUpdate, UpdateInfo};
use witness_core::{AppCore, CoreEvent, CoreResult, EventSink, RecCmd, RecOutcome};

/// One message for the UI thread.
pub enum UiMsg {
    Core(CoreEvent),
    Shell(ShellEvent),
    Op(OpDone),
}

/// Results of app-level operations, handled in `WitnessApp::logic()`
/// whatever view is showing and whether or not the window is visible.
pub enum OpDone {
    /// Generic completion: optional toast, optional list refresh (bumps
    /// `shell.refresh_tick`).
    Notice {
        notice: Option<Notice>,
        refresh: bool,
    },
    /// `Effect::RefreshStatus` result.
    Status(AppStatus),
    /// `Effect::RecheckModels` result → `shell.models_missing`.
    Models(Vec<ModelInfo>),
    /// A `SettingsStore` write finished.
    SettingsWritten { seq: u64, result: CoreResult<()> },
    /// A `RefreshStatus` op that panicked or could not start: clears the
    /// in-flight flag (the error is logged, not toasted).
    StatusFailed(String),
    /// Same for `RecheckModels`.
    ModelsFailed(String),
    /// `UpdateFlow::check`.
    UpdateChecked(CoreResult<Option<UpdateInfo>>),
    /// `UpdateFlow::install` → the install exit path (§4.5).
    UpdateDownloaded(CoreResult<PreparedUpdate>),
    /// → the restart exit path (§4.5).
    RestartPrepared(CoreResult<()>),
    /// → the quit exit path (§4.5).
    ShutdownDone,
    /// A rec-control reply: top-bar spinner off + in-app error toast for
    /// `TopBar`; Tray/Hotkey errors were already OS-toasted by the core.
    Recording {
        origin: RecOrigin,
        cmd: RecCmd,
        result: CoreResult<RecOutcome>,
    },
    /// CC button → `UiCmd::ShowCaptionsOverlay` or an error toast.
    CaptionsCheck(CoreResult<()>),
}

/// Who asked for a recording command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecOrigin {
    TopBar,
    Tray,
    Hotkey,
}

/// Completion flag of a `spawn_op`, so a view can show a spinner without
/// owning the result.
#[derive(Clone, Debug, Default)]
pub struct OpHandle(Arc<AtomicBool>);

impl OpHandle {
    /// An already-finished handle (e.g. when the thread could not start).
    pub fn done() -> OpHandle {
        OpHandle(Arc::new(AtomicBool::new(true)))
    }

    pub fn is_done(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn finish(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Wake the root viewport (whose `logic()` drains the channel) from any
/// thread.
pub fn wake_root(ctx: &egui::Context) {
    ctx.request_repaint_of(egui::ViewportId::ROOT);
}

/// [`wake_root`] after at most `delay`.
pub fn wake_root_after(ctx: &egui::Context, delay: Duration) {
    ctx.request_repaint_after_for(delay, egui::ViewportId::ROOT);
}

/// The core's `EventSink` and the shell handlers' way into the UI thread.
#[derive(Clone)]
pub struct UiSink {
    tx: Sender<UiMsg>,
    ctx: Arc<OnceLock<egui::Context>>,
}

impl UiSink {
    /// An unbounded channel; the receiver goes to `WitnessApp`.
    pub fn new() -> (UiSink, Receiver<UiMsg>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (
            UiSink {
                tx,
                ctx: Arc::new(OnceLock::new()),
            },
            rx,
        )
    }

    /// Called once in the app creator; events queued before it are drained
    /// by the first pass.
    pub fn set_context(&self, ctx: egui::Context) {
        if self.ctx.set(ctx.clone()).is_err() {
            log::warn!("UI sink context set twice");
        }
        wake_root(&ctx);
    }

    /// Forward a tray / hotkey / single-instance event (any thread).
    pub fn shell(&self, event: ShellEvent) {
        let _ = self.tx.send(UiMsg::Shell(event));
        self.repaint();
    }

    pub(crate) fn sender(&self) -> Sender<UiMsg> {
        self.tx.clone()
    }

    fn repaint(&self) {
        if let Some(ctx) = self.ctx.get() {
            wake_root(ctx);
        }
    }
}

impl EventSink for UiSink {
    fn emit(&self, event: CoreEvent) {
        let high_rate = event.is_high_rate();
        let _ = self.tx.send(UiMsg::Core(event));
        if let Some(ctx) = self.ctx.get() {
            if high_rate {
                // One pass drains a whole burst.
                wake_root_after(ctx, Duration::from_millis(100));
            } else {
                wake_root(ctx);
            }
        }
    }
}

/// "internal error: <panic message>" — the text a panicking worker reports.
pub fn internal_error(panic: &(dyn Any + Send)) -> String {
    let message = panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    format!("internal error: {message}")
}

/// The result of one background load, owned by the view that asked for it.
/// Dropping it discards the result.
pub struct Task<T> {
    rx: Receiver<std::thread::Result<T>>,
    done: bool,
}

impl<T> Task<T> {
    /// `Some` exactly once: `Ok(value)`, or `Err("internal error: …")` if
    /// the worker panicked (or could not start).
    pub fn poll(&mut self) -> Option<Result<T, String>> {
        if self.done {
            return None;
        }
        match self.rx.try_recv() {
            Ok(result) => {
                self.done = true;
                Some(result.map_err(|panic| internal_error(panic.as_ref())))
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                Some(Err("internal error: the task ended without a result".into()))
            }
        }
    }

    /// True until `poll` has returned the result.
    pub fn is_pending(&self) -> bool {
        !self.done
    }
}

impl<U> Task<CoreResult<U>> {
    /// `poll` with the panic error folded into the core's error type.
    pub fn poll_flat(&mut self) -> Option<CoreResult<U>> {
        self.poll().map(|result| result.and_then(|inner| inner))
    }
}

impl<T: Send + 'static> Task<T> {
    /// Wrap a plain reply channel (e.g. `AppCore::submit_recording`): a tiny
    /// waiter thread forwards the value and wakes the UI.
    pub fn from_receiver(rx: Receiver<T>, ctx: egui::Context) -> Task<T> {
        let (tx, task_rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("task-reply".into())
            .spawn(move || {
                if let Ok(value) = rx.recv() {
                    let _ = tx.send(Ok(value));
                }
                wake_root(&ctx);
            });
        if let Err(error) = spawned {
            log::error!("could not start a reply waiter: {error}");
        }
        Task {
            rx: task_rx,
            done: false,
        }
    }
}

/// Runs core work off the UI thread.
#[derive(Clone)]
pub struct Tasks {
    core: Arc<AppCore>,
    ctx: egui::Context,
    tx: Sender<UiMsg>,
}

impl Tasks {
    pub fn new(core: Arc<AppCore>, ctx: egui::Context, sink: &UiSink) -> Tasks {
        Tasks {
            core,
            ctx,
            tx: sink.sender(),
        }
    }

    pub fn core(&self) -> &Arc<AppCore> {
        &self.core
    }

    /// LOADS: one named thread per call, the closure under `catch_unwind`,
    /// a repaint on completion.
    pub fn spawn<T: Send + 'static>(
        &self,
        name: &'static str,
        f: impl FnOnce(&AppCore) -> T + Send + 'static,
    ) -> Task<T> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let core = self.core.clone();
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| f(&core)));
                let _ = tx.send(result);
                wake_root(&ctx);
            });
        if let Err(error) = spawned {
            // The dropped sender makes the task report an internal error.
            log::error!("could not start task {name}: {error}");
        }
        Task { rx, done: false }
    }

    /// ACTIONS: like `spawn`, but the `OpDone` goes to `logic()` as
    /// `UiMsg::Op`. A panic becomes an "internal error: …" error toast.
    ///
    /// Only for ops whose `OpDone` releases no waiting state. An op that a
    /// state machine waits for (the status/models in-flight flags,
    /// `SettingsStore`, `UpdateFlow`, quit, restart) uses
    /// [`Tasks::spawn_op_or`] with its own typed failure instead.
    pub fn spawn_op(
        &self,
        name: &'static str,
        f: impl FnOnce(&AppCore) -> OpDone + Send + 'static,
    ) -> OpHandle {
        self.spawn_op_or(name, f, |error| OpDone::Notice {
            notice: Some(Notice::error_plain(error)),
            refresh: false,
        })
    }

    /// `spawn_op` with the op's own failure result: when the closure panics,
    /// or its thread cannot start, `on_failure("internal error: …")` is sent
    /// in place of the closure's `OpDone`, so whatever waits for that op is
    /// always released.
    pub fn spawn_op_or<F>(
        &self,
        name: &'static str,
        f: impl FnOnce(&AppCore) -> OpDone + Send + 'static,
        on_failure: F,
    ) -> OpHandle
    where
        F: FnOnce(String) -> OpDone + Clone + Send + 'static,
    {
        let handle = OpHandle::default();
        let finished = handle.clone();
        let core = self.core.clone();
        let ctx = self.ctx.clone();
        let tx = self.tx.clone();
        // The worker gets a copy; the original covers a failed spawn.
        let on_panic = on_failure.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let done = run_guarded(name, &*core, f, on_panic);
                finished.finish();
                let _ = tx.send(UiMsg::Op(done));
                wake_root(&ctx);
            });
        match spawned {
            Ok(_) => handle,
            Err(error) => {
                log::error!("could not start operation {name}: {error}");
                let done = on_failure(format!("internal error: could not start {name}: {error}"));
                let _ = self.tx.send(UiMsg::Op(done));
                wake_root(&self.ctx);
                OpHandle::done()
            }
        }
    }

    /// Fire and forget (the closure reports through core events, if at all).
    /// A panic is logged by the panic hook and otherwise contained.
    pub fn fire(&self, name: &'static str, f: impl FnOnce(&AppCore) + Send + 'static) {
        let core = self.core.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _ = catch_unwind(AssertUnwindSafe(|| f(&core)));
            });
        if let Err(error) = spawned {
            log::error!("could not start {name}: {error}");
        }
    }

    /// Wait for a reply channel on a worker and turn it into an `OpDone`.
    /// The submit itself stays on the caller's thread, so commands keep the
    /// order in which the user issued them (rec_control is a FIFO).
    pub fn forward_reply<T: Send + 'static>(
        &self,
        name: &'static str,
        rx: Receiver<T>,
        map: impl FnOnce(Option<T>) -> OpDone + Send + 'static,
    ) {
        let ctx = self.ctx.clone();
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let done = map(rx.recv().ok());
                let _ = tx.send(UiMsg::Op(done));
                wake_root(&ctx);
            });
        if let Err(error) = spawned {
            log::error!("could not start {name}: {error}");
        }
    }
}

/// An op body under `catch_unwind`: a panic is logged and mapped through
/// `on_panic("internal error: …")`.
pub(crate) fn run_guarded<A: ?Sized>(
    name: &str,
    arg: &A,
    f: impl FnOnce(&A) -> OpDone,
    on_panic: impl FnOnce(String) -> OpDone,
) -> OpDone {
    catch_unwind(AssertUnwindSafe(|| f(arg))).unwrap_or_else(|panic| {
        let error = internal_error(panic.as_ref());
        log::error!("operation {name} failed: {error}");
        on_panic(error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait<T>(task: &mut Task<T>) -> Result<T, String> {
        for _ in 0..500 {
            if let Some(result) = task.poll() {
                return result;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("task did not finish");
    }

    fn task_from<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Task<T> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(catch_unwind(AssertUnwindSafe(f)));
        });
        Task { rx, done: false }
    }

    #[test]
    fn task_yields_its_value_once() {
        let mut task = task_from(|| 41 + 1);
        assert_eq!(wait(&mut task), Ok(42));
        assert!(!task.is_pending());
        assert!(task.poll().is_none(), "Some exactly once");
    }

    #[test]
    fn panicking_task_reports_an_internal_error() {
        let mut task = task_from(|| -> u8 { panic!("boom") });
        assert_eq!(wait(&mut task), Err("internal error: boom".into()));
    }

    #[test]
    fn run_guarded_maps_a_panic_to_the_ops_own_failure() {
        let ok = run_guarded(
            "status",
            &7u8,
            |n| OpDone::StatusFailed(n.to_string()),
            |_| unreachable!("no panic"),
        );
        assert!(matches!(ok, OpDone::StatusFailed(s) if s == "7"));
        let failed = run_guarded(
            "status",
            &(),
            |_| -> OpDone { panic!("recorder mutex poisoned") },
            OpDone::StatusFailed,
        );
        assert!(matches!(
            failed,
            OpDone::StatusFailed(e) if e == "internal error: recorder mutex poisoned"
        ));
        let quit = run_guarded(
            "quit",
            &(),
            |_| -> OpDone { panic!("x") },
            |_| OpDone::ShutdownDone,
        );
        assert!(matches!(quit, OpDone::ShutdownDone), "quit still exits");
    }

    #[test]
    fn poll_flat_folds_core_errors() {
        let mut ok: Task<CoreResult<u8>> = task_from(|| Ok(1));
        let mut err: Task<CoreResult<u8>> = task_from(|| Err("nope".to_string()));
        let flat = |t: &mut Task<CoreResult<u8>>| loop {
            if let Some(r) = t.poll_flat() {
                break r;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(flat(&mut ok), Ok(1));
        assert_eq!(flat(&mut err), Err("nope".into()));
    }

    #[test]
    fn from_receiver_forwards_the_reply() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut task = Task::from_receiver(rx, egui::Context::default());
        assert!(task.poll().is_none());
        tx.send("done").unwrap();
        assert_eq!(wait(&mut task), Ok("done"));
    }

    #[test]
    fn dropped_sender_ends_the_task_with_an_error() {
        let (tx, rx) = crossbeam_channel::bounded::<std::thread::Result<u8>>(1);
        drop(tx);
        let mut task = Task { rx, done: false };
        assert!(task
            .poll()
            .unwrap()
            .unwrap_err()
            .starts_with("internal error"));
    }

    #[test]
    fn sink_forwards_core_and_shell_messages() {
        let (sink, rx) = UiSink::new();
        sink.emit(CoreEvent::MeetingsChanged);
        sink.shell(ShellEvent::SecondInstance);
        assert!(matches!(
            rx.try_recv(),
            Ok(UiMsg::Core(CoreEvent::MeetingsChanged))
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UiMsg::Shell(ShellEvent::SecondInstance))
        ));
    }
}
