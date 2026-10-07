//! The service layer: one `AppCore` method per former Tauri command.
//!
//! `AppCore` owns the shared [`AppState`] and talks to the UI only through
//! two traits: [`EventSink`] receives every [`CoreEvent`] (including the
//! former direct shell side effects — tray state, the captions overlay) and
//! [`Notifier`] raises OS toasts. Nothing here knows about windows.
//!
//! Threading contract: methods marked ⏳ in the docs start with
//! [`assert_off_ui_thread`] — they take the recorder or `Db` mutex, do I/O,
//! or join threads, so the UI must run them on a worker (`Tasks`). The rest
//! are safe on the UI thread because they only touch short-lived locks no
//! thread holds across I/O, or a channel.

pub(crate) mod level_check;
pub(crate) mod library;
pub(crate) mod maintenance;
pub(crate) mod notes;
pub(crate) mod rec_control;
pub(crate) mod recording;
pub(crate) mod startup;
pub mod testing;
pub mod types;
pub mod updater;

pub use level_check::LevelCheck;
pub use rec_control::{RecCmd, RecOutcome};
pub use types::*;
pub use updater::PreparedUpdate;

use crate::db::Db;
use crate::events;
use crate::meeting_watcher::{WatcherCommand, WatcherControl};
use crate::pipeline::PipelineEvent;
use crate::settings::Settings;
use crate::state::{AppState, RecorderState};
use crossbeam_channel::Sender;
use std::cell::Cell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

/// User-facing error messages, kept verbatim from the Tauri commands.
pub type CoreResult<T> = Result<T, String>;

/// Everything the core reports to the UI. Payloads are the existing
/// `events.rs` structs; the shell variants replace what used to be direct
/// tray / window calls from backend threads.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    RecordingStarted(events::RecordingStarted),
    RecordingStopped(events::RecordingStopped),
    /// 2 Hz, coalescable.
    RecordingLevel(events::RecordingLevel),
    /// On change.
    RecordingHealth(crate::recorder::RecordingHealth),
    /// 0.5 Hz, coalescable.
    WatcherStatus(events::WatcherStatus),
    /// Bursty, coalescable.
    TranscriptionProgress(events::TranscriptionProgress),
    TranscriptionComplete(events::TranscriptionDone),
    TranscriptionFailed(events::TranscriptionDone),
    /// Per chunk; coalescable unless `done`.
    ModelDownloadProgress(events::ModelDownloadProgress),
    MeetingsChanged,
    LiveTranscript(events::LiveTranscript),
    LiveCaptionsStatus(events::LiveCaptionsStatus),
    /// Coalescable.
    UpdateProgress(events::UpdateProgress),
    /// Tray menu/tooltip/icon reflect recording state (was `tray::update`).
    TrayRecording(bool),
    /// Capture problem text for the tray tooltip, or None once it clears
    /// (was `tray::set_recording_warning`; emitted only when the text changes).
    TrayWarning(Option<String>),
    /// The live-caption worker is ready and the overlay setting is on.
    OpenCaptionsOverlay {
        meeting_id: i64,
    },
    /// Every stop path and the start rollback.
    CloseCaptionsOverlay,
    /// Notes writer results (`service/notes.rs`).
    NotesSaved {
        meeting_id: i64,
        seq: u64,
    },
    NotesSaveFailed {
        meeting_id: i64,
        seq: u64,
        error: String,
    },
}

impl CoreEvent {
    /// Events that may arrive in bursts; the UI coalesces their repaints.
    pub fn is_high_rate(&self) -> bool {
        match self {
            CoreEvent::RecordingLevel(_)
            | CoreEvent::WatcherStatus(_)
            | CoreEvent::TranscriptionProgress(_)
            | CoreEvent::UpdateProgress(_) => true,
            CoreEvent::ModelDownloadProgress(progress) => !progress.done,
            _ => false,
        }
    }
}

/// Receives every [`CoreEvent`]. Called from arbitrary backend threads.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: CoreEvent);
}

/// Raises an OS toast. Called from arbitrary backend threads.
pub trait Notifier: Send + Sync + 'static {
    fn toast(&self, body: &str);
}

/// What started a recording. Stored internally as the "auto" / "manual"
/// strings the database and events already use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Auto,
    Manual,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Auto => "auto",
            Trigger::Manual => "manual",
        }
    }
}

/// Why the process is about to exit; selects the [`AppCore::exit_guard`]
/// refusal messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitPurpose {
    Restart,
    Update,
}

/// Holds the recorder and backup locks (in the global lock order
/// recorder → backup_in_progress) so no recording or backup can start
/// between the final check and process exit.
#[must_use = "the locks are released as soon as the guard drops"]
pub struct ExitGuard<'a> {
    _recorder: MutexGuard<'a, RecorderState>,
    _backup_in_progress: MutexGuard<'a, bool>,
}

thread_local! {
    static UI_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Mark the calling thread as the UI thread. The bin calls this first thing
/// in `main()`; afterwards every ⏳ method asserts it is not called there.
pub fn mark_ui_thread() {
    UI_THREAD.with(|flag| flag.set(true));
}

/// Whether the calling thread is the one [`mark_ui_thread`] marked.
pub fn is_ui_thread() -> bool {
    UI_THREAD.with(Cell::get)
}

/// Debug-build guard at the top of every blocking (⏳) method.
pub fn assert_off_ui_thread(name: &str) {
    debug_assert!(
        !is_ui_thread(),
        "AppCore::{name} blocks and must not run on the UI thread"
    );
}

pub struct AppCore {
    pub(crate) state: Arc<AppState>,
    pub(crate) sink: Arc<dyn EventSink>,
    pub(crate) notifier: Arc<dyn Notifier>,
    /// The release found by the most recent update check (service/updater.rs).
    pub(crate) pending_update: Mutex<Option<updater::Release>>,
    rec_tx: Sender<rec_control::Request>,
    notes: notes::NotesWriter,
    relaunch_args: Vec<OsString>,
    /// Self-reference for work that must outlive a `&self` borrow
    /// (`emergency_stop`'s helper thread).
    this: Weak<AppCore>,
}

impl AppCore {
    /// Startup minus every shell concern: data dirs, logging, GPU preflight,
    /// the database, the pipeline worker, crash recovery, the 30-day bin
    /// purge, the meeting watcher, the rec-control and notes-writer threads,
    /// and the delayed update check. Never initialises COM on the calling
    /// thread (winit needs the main thread's apartment for itself).
    pub fn start(
        settings: Settings,
        sink: Arc<dyn EventSink>,
        notifier: Arc<dyn Notifier>,
    ) -> anyhow::Result<Arc<AppCore>> {
        let data_dir = settings.data_dir();
        for dir in [
            data_dir.clone(),
            settings.rec_tmp_dir(),
            settings.audio_dir(),
            settings.models_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        startup::init_logging(&data_dir);
        log::info!("Witness starting; data dir: {}", data_dir.display());
        // Surface CUDA problems at startup, where they are easy to spot — ort
        // would otherwise fall back to CPU without a word (gpu.rs). The managed
        // GPU-library dir must register first so preflight can use it.
        crate::gpu::set_managed_libs_dir(settings.cuda_dir());
        crate::gpu::preflight();

        let db = Arc::new(Db::open(&settings.db_path())?);

        // Pipeline worker: forwards its events to the UI + toasts.
        let settings = Arc::new(Mutex::new(settings));
        let pipeline = crate::pipeline::spawn(
            db.clone(),
            settings.clone(),
            pipeline_adapter(db.clone(), sink.clone(), notifier.clone()),
        );

        let (auto_record, patterns, configured_data_dir) = {
            let s = settings.lock().unwrap();
            (s.auto_record, s.watch_patterns.clone(), s.data_dir.clone())
        };
        let watcher = WatcherControl::new(auto_record, &patterns);

        let state = Arc::new(AppState {
            db: db.clone(),
            settings: settings.clone(),
            configured_data_dir: Mutex::new(configured_data_dir),
            recorder: Mutex::new(RecorderState::Idle),
            recording_health: Mutex::new(None),
            recording_trigger: Mutex::new(Trigger::Manual.as_str()),
            live_captions: AtomicBool::new(false),
            live_captions_meeting_id: AtomicI64::new(0),
            last_saved_recording: AtomicI64::new(0),
            pipeline,
            watcher: watcher.clone(),
            last_watcher_status: Mutex::new(events::WatcherStatus {
                enabled: auto_record,
                teams_key_found: false,
                mic_in_use: false,
                suppressed: false,
            }),
            downloading: AtomicBool::new(false),
            hotkey: Mutex::new(None),
            bookmark_hotkey: Mutex::new(None),
            backup_in_progress: Mutex::new(false),
        });

        let core = Arc::new_cyclic(|weak: &Weak<AppCore>| AppCore {
            state: state.clone(),
            sink,
            notifier,
            pending_update: Mutex::new(None),
            rec_tx: rec_control::spawn(weak.clone()),
            notes: notes::NotesWriter::spawn(weak.clone()),
            relaunch_args: std::env::args_os().skip(1).collect(),
            this: weak.clone(),
        });

        {
            let s = settings.lock().unwrap().clone();
            startup::recover_orphans(&db, &s, &state.pipeline);
        }
        // Recycle bin: purge anything deleted more than 30 days ago.
        startup::purge_expired_bin(&state);

        // Meeting watcher → auto record. Runs start/stop synchronously on the
        // watcher thread, exactly as before; the Weak lets the thread end if
        // the core is ever dropped.
        let watch_core = Arc::downgrade(&core);
        crate::meeting_watcher::spawn(watcher, move |cmd| match watch_core.upgrade() {
            Some(core) => {
                core.on_watcher_command(cmd);
                true
            }
            None => false,
        });

        updater::spawn_startup_check(core.clone());
        Ok(core)
    }

    /// Escape hatch for tests and diagnostics. [ui-ok]
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Raise an OS toast. [ui-ok]
    pub fn toast(&self, body: &str) {
        self.notifier.toast(body);
    }

    pub(crate) fn emit(&self, event: CoreEvent) {
        self.sink.emit(event);
    }

    /// The data directory opened at startup (db, audio, log live here until
    /// a restart applies a changed setting). [ui-ok]
    pub fn runtime_data_dir(&self) -> PathBuf {
        self.state.settings.lock().unwrap().data_dir()
    }

    /// Record which global hotkeys actually bound (the shell walks the
    /// candidate chains; other apps may own our preferred combos). [ui-ok]
    pub fn set_bound_hotkeys(&self, record: Option<String>, bookmark: Option<String>) {
        *self.state.hotkey.lock().unwrap() = record;
        *self.state.bookmark_hotkey.lock().unwrap() = bookmark;
    }

    /// (record, bookmark) hotkeys that bound, None = every candidate taken. [ui-ok]
    pub fn hotkeys(&self) -> (Option<String>, Option<String>) {
        (
            self.state.hotkey.lock().unwrap().clone(),
            self.state.bookmark_hotkey.lock().unwrap().clone(),
        )
    }

    /// `std::env::args_os().skip(1)` captured at start; the relaunch
    /// arguments for both Restart and Update. [ui-ok]
    pub fn relaunch_args(&self) -> Vec<OsString> {
        self.relaunch_args.clone()
    }

    /// Enqueue the newest notes draft for `meeting_id`. Latest wins per
    /// meeting: an older queued draft is replaced and a draft is never
    /// written after a newer one. Returns the draft's (monotonic) seq; the
    /// writer answers with `NotesSaved` / `NotesSaveFailed`. [ui-ok]
    pub fn queue_notes(&self, meeting_id: i64, text: String) -> u64 {
        self.notes.queue(meeting_id, text)
    }

    /// Block until the notes writer is empty and idle, or `timeout` passes
    /// (false). Exit paths only. [ui-ok: exit only]
    pub fn flush_notes(&self, timeout: Duration) -> bool {
        let flushed = self.notes.flush(timeout);
        if !flushed {
            log::warn!("notes writer did not finish within {timeout:?}");
        }
        flushed
    }

    /// Watcher thread → recording control (formerly main.rs:410-447).
    fn on_watcher_command(&self, cmd: WatcherCommand) {
        match cmd {
            // Sent just before an exit path suspended auto-record: a
            // recording started now would be cut off unfinalized.
            WatcherCommand::StartMeeting if self.state.watcher.exiting() => {
                log::info!("meeting detected while exiting: not auto-recording");
            }
            WatcherCommand::StartMeeting => match self.start_recording(Trigger::Auto) {
                Ok(_) => self.toast("Witness is recording this meeting."),
                // Lost the race with an exit path (re-checked under the
                // recorder lock): same as the guard above.
                Err(e) if e == recording::EXITING => {
                    log::info!("meeting detected while exiting: not auto-recording");
                }
                Err(e) => {
                    // The user is in a call believing this is recorded.
                    log::warn!("auto-record start failed: {e}");
                    self.toast(&format!("Recording could not start: {e}"));
                }
            },
            WatcherCommand::StopMeeting => {
                // The meeting is over: stop whatever recording is running,
                // auto- or manually started. This only fires after a watched
                // meeting was detected and ended, so a manual recording made
                // outside any call is never touched.
                let trigger = *self.state.recording_trigger.lock().unwrap();
                let recording = self.state.recorder.lock().unwrap().is_recording();
                if recording {
                    log::info!("meeting ended: stopping {trigger} recording");
                    if let Err(e) = self.stop_recording(false) {
                        // The stop errors are self-describing ("Restart
                        // Witness to recover…") — show them verbatim.
                        log::warn!("auto-record stop failed: {e}");
                        self.toast(&e);
                    }
                }
            }
            WatcherCommand::Status(status) => {
                *self.state.last_watcher_status.lock().unwrap() = status.clone();
                self.emit(CoreEvent::WatcherStatus(status));
            }
        }
    }
}

/// Pipeline events → UI events + toasts (formerly main.rs:216-271).
fn pipeline_adapter(
    db: Arc<Db>,
    sink: Arc<dyn EventSink>,
    notifier: Arc<dyn Notifier>,
) -> impl Fn(PipelineEvent) + Send + 'static {
    move |event| match event {
        PipelineEvent::Progress {
            meeting_id,
            stage,
            pct,
        } => {
            sink.emit(CoreEvent::TranscriptionProgress(
                events::TranscriptionProgress {
                    meeting_id,
                    stage,
                    pct,
                },
            ));
        }
        PipelineEvent::Complete { meeting_id, rtf } => {
            sink.emit(CoreEvent::TranscriptionComplete(
                events::TranscriptionDone {
                    meeting_id,
                    error: None,
                },
            ));
            sink.emit(CoreEvent::MeetingsChanged);
            // Only toast when a transcript was actually produced. The
            // realtime factor doubles as a GPU sanity check: single-digit
            // numbers on this box mean ort fell back to CPU.
            if let Ok(Some(m)) = db.get_meeting(meeting_id) {
                if m.status == "transcribed" {
                    let speed = rtf
                        .map(|r| format!(" ({r:.0}× realtime)"))
                        .unwrap_or_default();
                    notifier.toast(&format!("Transcript ready: {}{speed}", m.title));
                }
            }
        }
        PipelineEvent::Failed { meeting_id, error } => {
            sink.emit(CoreEvent::TranscriptionFailed(events::TranscriptionDone {
                meeting_id,
                error: Some(error),
            }));
            sink.emit(CoreEvent::MeetingsChanged);
            notifier.toast("Transcription failed — you can retry from the meeting page.");
        }
    }
}

/// `format!("{e:#}")` — the alternate form keeps anyhow context chains.
pub(crate) fn err<E: std::fmt::Display>(e: E) -> String {
    format!("{e:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_rate_classification() {
        let progress = |done| {
            CoreEvent::ModelDownloadProgress(events::ModelDownloadProgress {
                model_id: "parakeet".into(),
                file: String::new(),
                downloaded_bytes: 0,
                total_bytes: None,
                file_index: 0,
                file_count: 0,
                done,
                error: None,
            })
        };
        assert!(progress(false).is_high_rate());
        assert!(!progress(true).is_high_rate());
        assert!(CoreEvent::RecordingLevel(events::RecordingLevel {
            mic_rms: 0.0,
            loopback_rms: 0.0,
            elapsed_ms: 0,
        })
        .is_high_rate());
        assert!(!CoreEvent::MeetingsChanged.is_high_rate());
        assert!(!CoreEvent::TrayRecording(true).is_high_rate());
    }

    /// End-to-end through `AppCore::start` on a scratch data dir (auto-record
    /// off, so the real watcher never starts a capture).
    #[test]
    fn core_starts_and_serves_notes_status_and_recording_control() {
        let (core, sink, dir) = testing::scratch_core("start");
        assert_eq!(core.runtime_data_dir(), dir);
        assert!(dir.join("witness.db").exists());

        let id = core
            .state()
            .db
            .create_meeting("Notes test", "2026-07-18T10:00:00+01:00", "manual")
            .unwrap();
        let seq = core.queue_notes(id, "remember the budget".into());
        assert!(core.flush_notes(Duration::from_secs(10)));
        assert_eq!(
            core.meeting_detail(id).unwrap().meeting.notes,
            "remember the budget"
        );
        assert!(sink.events().iter().any(|event| matches!(
            event,
            CoreEvent::NotesSaved { meeting_id, seq: s } if *meeting_id == id && *s == seq
        )));

        assert!(!core.status().recording);
        assert_eq!(
            core.submit_recording(RecCmd::BookmarkNow)
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            Err("Not recording".into())
        );
        core.state
            .watcher
            .enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(core.state.watcher.auto_record());
        assert!(!core.emergency_stop(Duration::from_secs(5)));
        assert!(!core.state.watcher.auto_record());
        // A settings write (update_settings stores `enabled`) cannot undo
        // the exit's suspension.
        core.state
            .watcher
            .enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!core.state.watcher.auto_record());
        core.shutdown();
        assert!(!core.state.watcher.auto_record());
        drop(
            core.exit_guard(ExitPurpose::Restart)
                .expect("idle core may exit"),
        );
        assert_eq!(core.check_captions_overlay(), Err("Not recording".into()));
    }

    #[test]
    fn trigger_strings_match_the_database() {
        assert_eq!(Trigger::Auto.as_str(), "auto");
        assert_eq!(Trigger::Manual.as_str(), "manual");
    }

    #[test]
    fn ui_thread_flag_is_per_thread() {
        std::thread::spawn(|| {
            mark_ui_thread();
            assert!(is_ui_thread());
        })
        .join()
        .unwrap();
        assert!(!UI_THREAD.with(Cell::get));
        assert_off_ui_thread("test");
    }
}
