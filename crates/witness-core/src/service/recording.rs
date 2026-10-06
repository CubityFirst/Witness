//! Recording control shared by the watcher thread, the rec-control worker,
//! restart/quit and the panic hook (formerly `do_start_recording` /
//! `do_stop_recording` / `do_bookmark_now` in commands.rs). Never touches
//! WASAPI directly — capture runs on its own threads owned by the recorder.

use super::{assert_off_ui_thread, err, AppCore, AppStatus, CoreEvent, CoreResult, Trigger};
use crate::events;
use crate::pipeline::Job;
use crate::recorder::{self, RecordingHealth};
use crate::state::RecorderState;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// `stop_recording`'s answer while another thread (the watcher) is still
/// starting the recording. Transient: tray Quit waits it out.
pub(crate) const STILL_STARTING: &str = "Recording is still starting";
/// `stop_recording`'s answer while another thread is finalizing the WAVs.
/// Transient: tray Quit waits it out.
pub(crate) const ALREADY_STOPPING: &str = "Recording is already stopping";

/// Identity of a capture problem, independent of the numbers in its message.
/// Health is re-evaluated every 500 ms and the dropped-audio counters only
/// grow, so a message-level comparison would treat every tick of an ongoing
/// problem as new — deduplicating the OS toast needs this stable key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WarningKind {
    WriterFailed,
    CaptureFailed(&'static str),
    DeviceLost(&'static str),
    CaptureDropped(&'static str),
    LiveDropped(&'static str),
    LiveDisconnected(&'static str),
}

/// The first user-visible problem in a degraded capture state, if any, as
/// (kind, message). Device loss is distinguished from "not connected yet"
/// via the loss count.
pub(crate) fn capture_warning(health: &RecordingHealth) -> Option<(WarningKind, String)> {
    if let Some(error) = &health.writer_error {
        return Some((
            WarningKind::WriterFailed,
            format!("Recording failed: {error}"),
        ));
    }
    for (name, track) in [
        ("Microphone", &health.mic),
        ("System audio", &health.loopback),
    ] {
        if let Some(error) = &track.fatal_error {
            return Some((
                WarningKind::CaptureFailed(name),
                format!("{name} capture failed: {error}"),
            ));
        }
        if !track.connected && track.device_loss_count > 0 {
            return Some((
                WarningKind::DeviceLost(name),
                format!("{name} device lost — recording silence until it returns"),
            ));
        }
        if track.capture_overflow_count > 0 {
            return Some((
                WarningKind::CaptureDropped(name),
                format!(
                    "{name} capture dropped {} ms of audio",
                    track.capture_dropped_ms
                ),
            ));
        }
        if track.live_dropped_ms > 0 {
            return Some((
                WarningKind::LiveDropped(name),
                format!(
                    "Live captions skipped {} ms to keep recording responsive",
                    track.live_dropped_ms
                ),
            ));
        }
        if track.live_disconnected {
            return Some((
                WarningKind::LiveDisconnected(name),
                format!("Live captions stopped receiving {}", name.to_lowercase()),
            ));
        }
    }
    None
}

impl AppCore {
    /// Create the meeting row and start capture (+ live captions when
    /// enabled and downloaded). ⏳
    pub fn start_recording(&self, trigger: Trigger) -> CoreResult<i64> {
        assert_off_ui_thread("start_recording");
        let trigger = trigger.as_str();
        let state = &self.state;
        let mut rec = state.recorder.lock().unwrap();
        if *state.backup_in_progress.lock().unwrap() {
            return Err("A backup is in progress".into());
        }
        if rec.is_busy() {
            return Err("Already recording".into());
        }
        *rec = RecorderState::Starting;
        drop(rec);

        let now = chrono::Local::now();
        let title = format!("Meeting {}", now.format("%Y-%m-%d %H:%M"));
        let id = match state.db.create_meeting(&title, &now.to_rfc3339(), trigger) {
            Ok(id) => id,
            Err(e) => {
                *state.recorder.lock().unwrap() = RecorderState::Idle;
                return Err(err(e));
            }
        };
        let (rec_dir, mic_device, loopback_device, live_enabled, models_dir, overlay, junk_phrases) = {
            let s = state.settings.lock().unwrap();
            (
                s.rec_tmp_dir(),
                s.mic_device.clone(),
                s.loopback_device.clone(),
                s.live_transcribe,
                s.models_dir(),
                s.caption_overlay,
                s.junk_phrases.clone(),
            )
        };

        // Live captions: best-effort — needs the Parakeet model on disk. The
        // worker reports readiness only after its engine and track state exist.
        state.live_captions.store(false, Ordering::Relaxed);
        state.live_captions_meeting_id.store(id, Ordering::Relaxed);
        let live_tx = if live_enabled {
            let line_sink = self.sink.clone();
            let status_sink = self.sink.clone();
            let status_notifier = self.notifier.clone();
            let status_state = state.clone();
            let tx = crate::live_transcribe::spawn(
                models_dir,
                junk_phrases,
                move |track, start_ms, end_ms, text| {
                    line_sink.emit(CoreEvent::LiveTranscript(events::LiveTranscript {
                        meeting_id: id,
                        track,
                        start_ms,
                        end_ms,
                        text,
                    }));
                },
                move |result| {
                    let active = result.is_ok();
                    // Generation token: a worker that finishes initializing
                    // after its recording ended must not publish anything.
                    if status_state
                        .live_captions_meeting_id
                        .load(Ordering::Relaxed)
                        != id
                    {
                        return;
                    }
                    status_state.live_captions.store(active, Ordering::Relaxed);
                    let error = result.err();
                    status_sink.emit(CoreEvent::LiveCaptionsStatus(events::LiveCaptionsStatus {
                        meeting_id: id,
                        active,
                        error: error.clone(),
                    }));
                    if active && overlay {
                        status_sink.emit(CoreEvent::OpenCaptionsOverlay { meeting_id: id });
                    } else if let Some(error) = error {
                        status_notifier.toast(&format!("Live captions could not start: {error}"));
                    }
                },
            );
            if tx.is_none() {
                log::info!("live captions off: Parakeet model not downloaded");
            }
            tx
        } else {
            None
        };
        let level_sink = self.sink.clone();
        let health_sink = self.sink.clone();
        let health_notifier = self.notifier.clone();
        let health_state = state.clone();
        *state.recording_health.lock().unwrap() = None;
        let handle = recorder::start_with_health(
            &rec_dir,
            id,
            mic_device,
            loopback_device,
            live_tx,
            move |mic_rms, loopback_rms, elapsed_ms| {
                level_sink.emit(CoreEvent::RecordingLevel(events::RecordingLevel {
                    mic_rms,
                    loopback_rms,
                    elapsed_ms,
                }));
            },
            move |health| {
                let previous = health_state
                    .recording_health
                    .lock()
                    .unwrap()
                    .replace(health.clone());
                if (health.mic.live_disconnected || health.loopback.live_disconnected)
                    && health_state.live_captions.swap(false, Ordering::Relaxed)
                {
                    health_sink.emit(CoreEvent::LiveCaptionsStatus(events::LiveCaptionsStatus {
                        meeting_id: id,
                        active: false,
                        error: Some("The live-caption worker disconnected".into()),
                    }));
                }
                // Surface each new degradation outside the window too — it is
                // usually hidden to the tray while an auto-recording runs. The
                // tooltip follows the live numbers; the OS toast fires only when
                // the *kind* of problem changes, otherwise an ongoing overflow
                // would raise one notification per 500 ms health tick.
                let warning = capture_warning(&health);
                let previous_warning = previous.as_ref().and_then(capture_warning);
                let text = warning.as_ref().map(|(_, text)| text.as_str());
                if text != previous_warning.as_ref().map(|(_, text)| text.as_str()) {
                    health_sink.emit(CoreEvent::TrayWarning(text.map(str::to_string)));
                }
                if warning.as_ref().map(|(kind, _)| *kind)
                    != previous_warning.as_ref().map(|(kind, _)| *kind)
                {
                    if let Some(text) = text {
                        health_notifier.toast(text);
                    }
                }
                health_sink.emit(CoreEvent::RecordingHealth(health));
            },
        );
        let handle = match handle {
            Ok(handle) => handle,
            Err(e) => {
                // Recorder startup may have created a sidecar or one WAV before a
                // later step failed.  Roll back both filesystem and DB state so a
                // failed Start never leaves a phantom active meeting.
                let (mic, lop, meta) = recorder::wav_paths(&rec_dir, id);
                for path in [mic, lop, meta] {
                    if let Err(remove_err) = std::fs::remove_file(&path) {
                        if remove_err.kind() != std::io::ErrorKind::NotFound {
                            log::warn!("failed to roll back {}: {remove_err}", path.display());
                        }
                    }
                }
                if let Err(db_err) = state.db.delete_meeting(id) {
                    log::error!("failed to roll back meeting {id}: {db_err:#}");
                }
                state.live_captions.store(false, Ordering::Relaxed);
                state.live_captions_meeting_id.store(0, Ordering::Relaxed);
                self.emit(CoreEvent::CloseCaptionsOverlay);
                *state.recorder.lock().unwrap() = RecorderState::Idle;
                return Err(err(e));
            }
        };
        *state.recording_trigger.lock().unwrap() = trigger;
        *state.recorder.lock().unwrap() = RecorderState::Recording(handle);

        let live_started = state.live_captions.load(Ordering::Relaxed);

        self.emit_started(events::RecordingStarted {
            meeting_id: id,
            trigger,
            started_at: now.to_rfc3339(),
            live_captions: live_started,
        });

        // Auto-recorded meetings: try to pick up the meeting subject from the
        // Teams window title (it settles a little after joining).
        if trigger == "auto" {
            self.spawn_meeting_title(id);
        }

        log::info!("recording started: meeting {id} ({trigger})");
        Ok(id)
    }

    /// Rename an auto-started meeting from the Teams call window's title
    /// (12/30/60 s after start; never overwrites a user rename).
    fn spawn_meeting_title(&self, id: i64) {
        let state = self.state.clone();
        let sink = self.sink.clone();
        std::thread::Builder::new()
            .name("meeting-title".into())
            .spawn(move || {
                for delay_s in [12u64, 30, 60] {
                    std::thread::sleep(Duration::from_secs(delay_s));
                    // Stop if the recording ended or the user renamed it.
                    if state.recorder.lock().unwrap().meeting_id() != Some(id) {
                        return;
                    }
                    let Ok(Some(meeting)) = state.db.get_meeting(id) else {
                        return;
                    };
                    if !meeting.title.starts_with("Meeting 2") {
                        return; // user already renamed
                    }
                    let pre_call = state.watcher.idle_teams_windows.lock().unwrap().clone();
                    if let Some(title) = crate::meeting_title::find_teams_meeting_title(&pre_call) {
                        log::info!("meeting {id}: auto-named from Teams window: {title}");
                        let _ = state.db.rename_meeting(id, &title);
                        sink.emit(CoreEvent::MeetingsChanged);
                        return;
                    }
                }
            })
            .ok();
    }

    /// Emitted by a successful start, in this order.
    fn emit_started(&self, started: events::RecordingStarted) {
        self.emit(CoreEvent::RecordingStarted(started));
        self.emit(CoreEvent::MeetingsChanged);
        self.emit(CoreEvent::TrayRecording(true));
    }

    /// Emitted by every stop exit path, in this order.
    fn emit_stopped(&self, meeting_id: i64) {
        self.emit(CoreEvent::RecordingStopped(events::RecordingStopped {
            meeting_id,
        }));
        self.emit(CoreEvent::MeetingsChanged);
        self.emit(CoreEvent::TrayRecording(false));
        self.emit(CoreEvent::CloseCaptionsOverlay);
    }

    /// Finalize the WAVs (joins the capture and writer threads), close the
    /// meeting row and queue processing. ⏳
    pub fn stop_recording(&self, by_user: bool) -> CoreResult<()> {
        assert_off_ui_thread("stop_recording");
        let state = &self.state;
        let handle = {
            let mut recorder = state.recorder.lock().unwrap();
            match std::mem::replace(&mut *recorder, RecorderState::Idle) {
                RecorderState::Recording(handle) => {
                    *recorder = RecorderState::Stopping {
                        meeting_id: handle.meeting_id,
                        started_at: handle.started_at,
                    };
                    handle
                }
                RecorderState::Starting => {
                    *recorder = RecorderState::Starting;
                    return Err(STILL_STARTING.into());
                }
                stopping @ RecorderState::Stopping { .. } => {
                    *recorder = stopping;
                    return Err(ALREADY_STOPPING.into());
                }
                RecorderState::Idle => return Err("Not recording".into()),
            }
        };
        let id = handle.meeting_id;
        state.live_captions.store(false, Ordering::Relaxed);
        state.live_captions_meeting_id.store(0, Ordering::Relaxed);

        // Manual stop during a live call: don't auto-restart until the call ends.
        if by_user && state.last_watcher_status.lock().unwrap().mic_in_use {
            state.watcher.suppressed.store(true, Ordering::Relaxed);
        }

        let stats = match handle.stop() {
            Ok(stats) => stats,
            Err(e) => {
                *state.recorder.lock().unwrap() = RecorderState::Idle;
                self.emit_stopped(id);
                return Err(format!(
                    "Recording stopped but its WAV files could not be finalized: {:#}. Restart Witness to recover them.",
                    e
                ));
            }
        };
        *state.recording_health.lock().unwrap() = Some(stats.health.clone());
        if let Err(e) = state.db.finish_recording(
            id,
            &chrono::Local::now().to_rfc3339(),
            stats.duration_ms as i64,
        ) {
            *state.recorder.lock().unwrap() = RecorderState::Idle;
            self.emit_stopped(id);
            return Err(format!(
                "Recording stopped but could not be saved to the database: {:#}. Restart Witness to recover it.",
                e
            ));
        }
        let transcribe = state.settings.lock().unwrap().auto_transcribe;
        // Keep the Idle transition and durable pipeline enqueue atomic with
        // backup start. The global lock order is recorder -> backup_in_progress.
        // Otherwise a backup could observe Idle + an empty queue in this narrow
        // gap and copy the WAVs while the new encode job starts removing them.
        let enqueue_result = {
            let mut recorder = state.recorder.lock().unwrap();
            let backup_in_progress = state.backup_in_progress.lock().unwrap();
            *recorder = RecorderState::Idle;
            let result = state.pipeline.enqueue(Job {
                meeting_id: id,
                transcribe,
                engine: None,
            });
            drop(backup_in_progress);
            result
        };
        self.emit_stopped(id);

        enqueue_result.map_err(|error| {
            format!(
                "Recording was saved, but processing could not be queued: {error:#}. Restart Witness to retry it."
            )
        })?;
        log::info!("recording stopped: meeting {id} ({} ms)", stats.duration_ms);
        Ok(())
    }

    /// Flag "this moment" in the active recording (hotkey + top-bar button).
    /// `Err("Not recording")` when idle — callers log that at debug. Returns
    /// the bookmark position in ms. ⏳
    pub fn bookmark_now(&self) -> CoreResult<i64> {
        assert_off_ui_thread("bookmark_now");
        let state = &self.state;
        let (meeting_id, at_ms) = {
            let rec = state.recorder.lock().unwrap();
            let RecorderState::Recording(handle) = &*rec else {
                return Err("Not recording".into());
            };
            let elapsed = (chrono::Local::now() - handle.started_at)
                .num_milliseconds()
                .max(0);
            (handle.meeting_id, elapsed)
        };
        state.db.add_bookmark(meeting_id, at_ms, "").map_err(err)?;
        self.emit(CoreEvent::MeetingsChanged);
        let secs = at_ms / 1000;
        self.toast(&format!("Bookmarked {}:{:02}", secs / 60, secs % 60));
        Ok(at_ms)
    }

    /// Recording + processing snapshot for the status pill. ⏳ (recorder and
    /// `Db` locks — the pipeline queue length is a DB count).
    pub fn status(&self) -> AppStatus {
        assert_off_ui_thread("status");
        let state = &self.state;
        let rec = state.recorder.lock().unwrap();
        let progress = state.pipeline.progress();
        AppStatus {
            recording: rec.is_recording(),
            meeting_id: rec.meeting_id(),
            recording_since: rec.started_at().map(|t| t.to_rfc3339()),
            live_captions: rec.is_recording() && state.live_captions.load(Ordering::Relaxed),
            transcribing_meeting_id: state.pipeline.current_meeting(),
            queue_len: state.pipeline.queue_len(),
            processing_stage: progress.as_ref().map(|progress| progress.stage.clone()),
            processing_pct: progress.map(|progress| progress.pct),
            watcher: state.last_watcher_status.lock().unwrap().clone(),
        }
    }

    /// ⏳ (recorder lock, which a stop holds across a DB write).
    pub fn is_recording(&self) -> bool {
        assert_off_ui_thread("is_recording");
        self.state.recorder.lock().unwrap().is_recording()
    }

    /// Validation behind the CC button: the UI opens its own overlay when
    /// this succeeds. ⏳
    pub fn check_captions_overlay(&self) -> CoreResult<()> {
        assert_off_ui_thread("check_captions_overlay");
        if !self.state.recorder.lock().unwrap().is_recording() {
            return Err("Not recording".into());
        }
        if !self.state.live_captions.load(Ordering::Relaxed) {
            return Err("Live captions are not available for this recording".into());
        }
        Ok(())
    }

    /// Exit paths: the meeting watcher stops auto-starting recordings for
    /// the rest of this process (lock-free; settings are not touched, and a
    /// later settings write cannot re-enable it). [ui-ok]
    pub fn suspend_auto_record(&self) {
        self.state.watcher.exiting.store(true, Ordering::SeqCst);
    }

    /// Panic hook only: stop the recording on a fresh helper thread and wait
    /// up to `timeout`. True when a recording was stopped and saved. Never
    /// panics and never takes a lock on the calling thread.
    pub fn emergency_stop(&self, timeout: Duration) -> bool {
        // The process is dying (possibly behind a message box): no new
        // auto-recordings that the exit would cut off.
        self.suspend_auto_record();
        let Some(core) = self.this.upgrade() else {
            return false;
        };
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("emergency-stop".into())
            .spawn(move || {
                let stopped = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    core.stop_recording(true)
                })) {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        if e != "Not recording" {
                            log::error!("emergency stop: {e}");
                        }
                        false
                    }
                    Err(_) => {
                        log::error!("emergency stop panicked");
                        false
                    }
                };
                let _ = done_tx.send(stopped);
            });
        match spawned {
            Ok(_) => done_rx.recv_timeout(timeout).unwrap_or_else(|_| {
                log::error!("emergency stop did not finish within {timeout:?}");
                false
            }),
            Err(error) => {
                log::error!("emergency stop: could not spawn a thread: {error}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::TrackHealth;
    use crate::service::testing::scratch_core;

    /// Compact names for the lifecycle events §3.2 orders.
    fn lifecycle(events: &[CoreEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                CoreEvent::RecordingStarted(e) => format!("started({})", e.meeting_id),
                CoreEvent::RecordingStopped(e) => format!("stopped({})", e.meeting_id),
                CoreEvent::MeetingsChanged => "meetings".into(),
                CoreEvent::TrayRecording(on) => format!("tray({on})"),
                CoreEvent::CloseCaptionsOverlay => "close-overlay".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn start_and_stop_emit_lifecycle_events_in_order() {
        let (core, sink, _dir) = scratch_core("lifecycle");
        let before = sink.events().len();
        core.emit_started(events::RecordingStarted {
            meeting_id: 42,
            trigger: "manual",
            started_at: String::new(),
            live_captions: false,
        });
        // Every stop exit path (normal, WAV failure, DB failure) goes
        // through emit_stopped.
        core.emit_stopped(42);
        assert_eq!(
            lifecycle(&sink.events()[before..]),
            [
                "started(42)",
                "meetings",
                "tray(true)",
                "stopped(42)",
                "meetings",
                "tray(false)",
                "close-overlay",
            ]
        );
    }

    #[test]
    fn capture_warning_kind_is_stable_while_counters_grow() {
        let mut health = RecordingHealth::default();
        health.mic.connected = true;
        health.loopback.connected = true;
        assert!(capture_warning(&health).is_none());

        health.loopback.capture_overflow_count = 1;
        health.loopback.capture_dropped_ms = 20;
        let (first_kind, first_text) = capture_warning(&health).unwrap();
        health.loopback.capture_dropped_ms = 60;
        let (second_kind, second_text) = capture_warning(&health).unwrap();
        assert_eq!(first_kind, second_kind);
        assert_ne!(first_text, second_text);
        assert_eq!(second_text, "System audio capture dropped 60 ms of audio");

        // Writer failure outranks every track problem.
        health.writer_error = Some("disk full".into());
        assert_eq!(
            capture_warning(&health),
            Some((
                WarningKind::WriterFailed,
                "Recording failed: disk full".into()
            ))
        );
    }

    #[test]
    fn device_loss_needs_a_loss_count() {
        let health = RecordingHealth {
            mic: TrackHealth {
                connected: false,
                ..TrackHealth::default()
            },
            ..RecordingHealth::default()
        };
        // Not connected yet is not "lost".
        assert!(capture_warning(&health).is_none());
        let lost = RecordingHealth {
            mic: TrackHealth {
                connected: false,
                device_loss_count: 1,
                ..TrackHealth::default()
            },
            ..RecordingHealth::default()
        };
        assert_eq!(
            capture_warning(&lost).unwrap().1,
            "Microphone device lost — recording silence until it returns"
        );
    }

    #[test]
    fn exit_suspension_of_auto_record_survives_settings_writes() {
        let (core, _sink, _dir) = scratch_core("suspend");
        let watcher = &core.state.watcher;
        watcher.enabled.store(true, Ordering::Relaxed);
        assert!(watcher.auto_record());
        core.suspend_auto_record();
        assert!(!watcher.auto_record());
        // What update_settings does on every write (e.g. flush_for_exit
        // after a Quit's shutdown): it must not undo the suspension.
        watcher.enabled.store(true, Ordering::Relaxed);
        assert!(!watcher.auto_record());
        assert!(watcher.exiting());
    }
}
