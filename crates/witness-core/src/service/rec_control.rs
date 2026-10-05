//! Ordered recording control for every UI origin (tray menu, hotkeys, top
//! bar). One "rec-control" thread serves a FIFO, so a double press runs
//! stop-then-start exactly like the old main-thread handlers did — but the
//! UI thread never joins capture threads itself. The meeting watcher keeps
//! calling the direct `AppCore` methods on its own thread, as before.

use super::{AppCore, CoreResult, Trigger};
use crossbeam_channel::{Receiver, Sender};
use std::panic::AssertUnwindSafe;
use std::sync::Weak;

/// A recording-control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecCmd {
    Start(Trigger),
    Stop {
        by_user: bool,
    },
    /// Tray "Record now ⇄ Stop recording" and the record hotkey: stop if
    /// recording, else start a manual recording; failures are toasted.
    ToggleWithToast,
    BookmarkNow,
}

/// What a successfully executed [`RecCmd`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecOutcome {
    Started(i64),
    Stopped,
    /// Toggle always reports this; its errors were already toasted.
    Toggled,
    /// Bookmark position in ms.
    Bookmarked(i64),
}

/// One queued command and where its reply goes (a dropped receiver is fine).
pub(crate) type Request = (RecCmd, Sender<CoreResult<RecOutcome>>);

/// The recording operations the worker drives. `AppCore` is the real one;
/// tests substitute a fake recorder.
pub(crate) trait RecTarget: Send + Sync + 'static {
    fn is_recording(&self) -> bool;
    fn start_recording(&self, trigger: Trigger) -> CoreResult<i64>;
    fn stop_recording(&self, by_user: bool) -> CoreResult<()>;
    fn bookmark_now(&self) -> CoreResult<i64>;
    fn toast(&self, body: &str);
}

impl RecTarget for AppCore {
    fn is_recording(&self) -> bool {
        AppCore::is_recording(self)
    }
    fn start_recording(&self, trigger: Trigger) -> CoreResult<i64> {
        AppCore::start_recording(self, trigger)
    }
    fn stop_recording(&self, by_user: bool) -> CoreResult<()> {
        AppCore::stop_recording(self, by_user)
    }
    fn bookmark_now(&self) -> CoreResult<i64> {
        AppCore::bookmark_now(self)
    }
    fn toast(&self, body: &str) {
        AppCore::toast(self, body)
    }
}

/// Stop if recording (by the user), else start a manual recording. The
/// record hotkey and tray item are often used with the window hidden, so a
/// silent failure would look like success: errors are logged and toasted.
pub(crate) fn toggle_with_toast<T: RecTarget + ?Sized>(target: &T) {
    let recording = target.is_recording();
    let result = if recording {
        target.stop_recording(true)
    } else {
        target.start_recording(Trigger::Manual).map(|_| ())
    };
    if let Err(e) = result {
        log::warn!("record toggle: {e}");
        let body = if recording {
            e
        } else {
            format!("Recording could not start: {e}")
        };
        target.toast(&body);
    }
}

fn execute<T: RecTarget + ?Sized>(target: &T, cmd: RecCmd) -> CoreResult<RecOutcome> {
    match cmd {
        RecCmd::Start(trigger) => target.start_recording(trigger).map(RecOutcome::Started),
        RecCmd::Stop { by_user } => target.stop_recording(by_user).map(|()| RecOutcome::Stopped),
        RecCmd::ToggleWithToast => {
            toggle_with_toast(target);
            Ok(RecOutcome::Toggled)
        }
        RecCmd::BookmarkNow => target.bookmark_now().map(RecOutcome::Bookmarked),
    }
}

/// Start the "rec-control" thread. It ends when every sender is dropped
/// (i.e. with the core).
pub(crate) fn spawn<T: RecTarget>(target: Weak<T>) -> Sender<Request> {
    let (tx, rx): (Sender<Request>, Receiver<Request>) = crossbeam_channel::unbounded();
    let spawned = std::thread::Builder::new()
        .name("rec-control".into())
        .spawn(move || {
            for (cmd, reply) in rx {
                let result = match target.upgrade() {
                    Some(target) => {
                        // A panic in start/stop must not kill the queue:
                        // later commands (and tray Quit) still need it.
                        std::panic::catch_unwind(AssertUnwindSafe(|| execute(&*target, cmd)))
                            .unwrap_or_else(|panic| {
                                Err(format!("internal error: {}", panic_message(&panic)))
                            })
                    }
                    None => Err("Witness is shutting down".into()),
                };
                let _ = reply.send(result);
            }
        });
    if let Err(error) = spawned {
        // Without the worker every submit answers with this error (the
        // receiver is dropped, so sends fail and `submit` reports it).
        log::error!("could not start the rec-control thread: {error}");
    }
    tx
}

/// Queue `cmd`; the reply arrives on the returned receiver. Never blocks.
pub(crate) fn submit(tx: &Sender<Request>, cmd: RecCmd) -> Receiver<CoreResult<RecOutcome>> {
    let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
    if let Err(error) = tx.send((cmd, reply_tx)) {
        let (_, reply_tx) = error.into_inner();
        let _ = reply_tx.send(Err("Recording control is unavailable".into()));
    }
    reply_rx
}

/// Tray Quit: stop the recording through the queue — behind any pending
/// toggle — and wait for it. "Not recording" is the normal idle answer;
/// other failures are logged, never returned (quit proceeds regardless).
pub(crate) fn shutdown(tx: &Sender<Request>) {
    match submit(tx, RecCmd::Stop { by_user: true }).recv() {
        Ok(Ok(_)) => {}
        Ok(Err(e)) if e == "Not recording" => {}
        Ok(Err(e)) => log::error!("stopping recording on quit: {e}"),
        Err(_) => log::error!("stopping recording on quit: recording control is unavailable"),
    }
}

pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "panic".into()
    }
}

impl AppCore {
    /// ORDERED recording control for every UI origin. Never blocks; the
    /// reply arrives on the returned receiver. [ui-ok]
    pub fn submit_recording(&self, cmd: RecCmd) -> Receiver<CoreResult<RecOutcome>> {
        submit(&self.rec_tx, cmd)
    }

    /// Tray Quit: stop the recording if one runs (failures are logged, not
    /// returned). Queued behind any pending toggle and blocks until done. ⏳
    pub fn shutdown(&self) {
        super::assert_off_ui_thread("shutdown");
        shutdown(&self.rec_tx);
    }

    /// Tray menu + record hotkey: stop if recording (by the user), else start
    /// a manual recording; errors are logged and toasted. ⏳
    pub fn toggle_recording_with_toast(&self) {
        super::assert_off_ui_thread("toggle_recording_with_toast");
        toggle_with_toast(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events;
    use crate::service::testing::CollectSink;
    use crate::service::{CoreEvent, EventSink};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A recorder that logs every call and emits the same lifecycle events
    /// the real one does, so ordering is visible both ways.
    struct FakeRecorder {
        recording: Mutex<bool>,
        calls: Mutex<Vec<String>>,
        toasts: Mutex<Vec<String>>,
        sink: CollectSink,
        fail_start: bool,
    }

    impl FakeRecorder {
        fn new(recording: bool) -> Arc<Self> {
            Self::with(recording, false)
        }
        fn with(recording: bool, fail_start: bool) -> Arc<Self> {
            Arc::new(FakeRecorder {
                recording: Mutex::new(recording),
                calls: Mutex::new(Vec::new()),
                toasts: Mutex::new(Vec::new()),
                sink: CollectSink::default(),
                fail_start,
            })
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RecTarget for FakeRecorder {
        fn is_recording(&self) -> bool {
            *self.recording.lock().unwrap()
        }
        fn start_recording(&self, trigger: Trigger) -> CoreResult<i64> {
            // Slow start: an unordered implementation would let the next
            // command overtake this one.
            std::thread::sleep(Duration::from_millis(30));
            self.calls
                .lock()
                .unwrap()
                .push(format!("start({})", trigger.as_str()));
            if self.fail_start {
                return Err("no microphone".into());
            }
            let mut recording = self.recording.lock().unwrap();
            if *recording {
                return Err("Already recording".into());
            }
            *recording = true;
            self.sink
                .emit(CoreEvent::RecordingStarted(events::RecordingStarted {
                    meeting_id: 7,
                    trigger: trigger.as_str(),
                    started_at: String::new(),
                    live_captions: false,
                }));
            Ok(7)
        }
        fn stop_recording(&self, by_user: bool) -> CoreResult<()> {
            std::thread::sleep(Duration::from_millis(10));
            self.calls.lock().unwrap().push(format!("stop({by_user})"));
            let mut recording = self.recording.lock().unwrap();
            if !*recording {
                return Err("Not recording".into());
            }
            *recording = false;
            self.sink
                .emit(CoreEvent::RecordingStopped(events::RecordingStopped {
                    meeting_id: 7,
                }));
            Ok(())
        }
        fn bookmark_now(&self) -> CoreResult<i64> {
            self.calls.lock().unwrap().push("bookmark".into());
            if *self.recording.lock().unwrap() {
                Ok(1_500)
            } else {
                Err("Not recording".into())
            }
        }
        fn toast(&self, body: &str) {
            self.toasts.lock().unwrap().push(body.to_string());
        }
    }

    fn wait(rx: Receiver<CoreResult<RecOutcome>>) -> CoreResult<RecOutcome> {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("rec-control reply")
    }

    fn event_names(sink: &CollectSink) -> Vec<&'static str> {
        sink.events()
            .iter()
            .map(|event| match event {
                CoreEvent::RecordingStarted(_) => "started",
                CoreEvent::RecordingStopped(_) => "stopped",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn commands_run_in_fifo_order() {
        let fake = FakeRecorder::new(false);
        let tx = spawn(Arc::downgrade(&fake));
        let replies = [
            submit(&tx, RecCmd::Start(Trigger::Manual)),
            submit(&tx, RecCmd::BookmarkNow),
            submit(&tx, RecCmd::Stop { by_user: true }),
            submit(&tx, RecCmd::BookmarkNow),
            submit(&tx, RecCmd::Start(Trigger::Auto)),
        ];
        let results: Vec<_> = replies.into_iter().map(wait).collect();
        assert_eq!(
            results,
            vec![
                Ok(RecOutcome::Started(7)),
                Ok(RecOutcome::Bookmarked(1_500)),
                Ok(RecOutcome::Stopped),
                Err("Not recording".into()),
                Ok(RecOutcome::Started(7)),
            ]
        );
        assert_eq!(
            fake.calls(),
            [
                "start(manual)",
                "bookmark",
                "stop(true)",
                "bookmark",
                "start(auto)"
            ]
        );
        assert_eq!(event_names(&fake.sink), ["started", "stopped", "started"]);
    }

    #[test]
    fn double_toggle_while_recording_stops_then_starts() {
        let fake = FakeRecorder::new(true);
        let tx = spawn(Arc::downgrade(&fake));
        let first = submit(&tx, RecCmd::ToggleWithToast);
        let second = submit(&tx, RecCmd::ToggleWithToast);
        assert_eq!(wait(first), Ok(RecOutcome::Toggled));
        assert_eq!(wait(second), Ok(RecOutcome::Toggled));
        assert_eq!(fake.calls(), ["stop(true)", "start(manual)"]);
        assert_eq!(event_names(&fake.sink), ["stopped", "started"]);
        assert!(fake.toasts.lock().unwrap().is_empty());
    }

    #[test]
    fn toggle_start_failure_is_toasted() {
        let fake = FakeRecorder::with(false, true);
        let tx = spawn(Arc::downgrade(&fake));
        assert_eq!(
            wait(submit(&tx, RecCmd::ToggleWithToast)),
            Ok(RecOutcome::Toggled)
        );
        assert_eq!(
            *fake.toasts.lock().unwrap(),
            ["Recording could not start: no microphone"]
        );
    }

    #[test]
    fn shutdown_runs_behind_a_pending_toggle() {
        let fake = FakeRecorder::new(false);
        let tx = spawn(Arc::downgrade(&fake));
        // A toggle (start) is still queued when Quit arrives: the quit stop
        // must run after it, so the new recording is finalized too.
        let _dropped_reply = submit(&tx, RecCmd::ToggleWithToast);
        shutdown(&tx);
        assert_eq!(fake.calls(), ["start(manual)", "stop(true)"]);
        assert!(!fake.is_recording());
        // Idle quit: "Not recording" is fine and nothing else happens.
        shutdown(&tx);
        assert_eq!(fake.calls().len(), 3);
    }

    #[test]
    fn dropped_target_answers_with_an_error() {
        let fake = FakeRecorder::new(false);
        let tx = spawn(Arc::downgrade(&fake));
        drop(fake);
        assert_eq!(
            wait(submit(&tx, RecCmd::BookmarkNow)),
            Err("Witness is shutting down".into())
        );
    }
}
