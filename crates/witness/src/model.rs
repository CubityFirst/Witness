//! Global UI state (app.tsx's `App` state, map-ui-shell §3) and the pure
//! event → state reducer. `ShellModel::apply` implements the event table
//! exactly; it never calls a blocking core method — slow follow-ups come
//! back as [`Effect`]s that `app.rs` runs (coalesced) through `spawn_op`.
//! Also home to [`UpdateFlow`], the app-owned Settings → Updates state
//! machine, so a download survives leaving Settings.

use crate::bridge::{OpDone, Tasks};
use crate::ui::toasts::Notice;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;
use witness_core::events::{
    AiOutcome, AiTask, LiveTranscript, ModelDownloadProgress, RecordingLevel,
    TranscriptionProgress, UpdateProgress,
};
use witness_core::recorder::RecordingHealth;
use witness_core::service::{AppStatus, UpdateInfo};
use witness_core::{AppCore, CoreEvent};

/// Live caption lines kept for the transcript feed (`prev.slice(-499)`).
pub const LIVE_LINES_CAP: usize = 500;

/// Follow-up work `apply` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Show an in-app toast.
    Notice(Notice),
    /// Re-read `AppCore::status` (coalesced: ≤1 in flight + a dirty flag).
    RefreshStatus,
    /// Re-read the model list for the first-run banner (coalesced).
    RecheckModels,
}

/// The shell's state, shared by the top bar and the views.
#[derive(Default)]
pub struct ShellModel {
    /// Refreshed only through `Effect::RefreshStatus`.
    pub status: Option<AppStatus>,
    /// Latest 2 Hz level/elapsed sample of the running recording.
    pub level: Option<RecordingLevel>,
    pub health: Option<RecordingHealth>,
    pub progress: Option<TranscriptionProgress>,
    /// Provisional captions of the active (or last) recording, capped at 500.
    pub live_lines: VecDeque<LiveTranscript>,
    pub live_meeting_id: Option<i64>,
    /// Whether the live worker runs; None = unknown (use `status.live_captions`).
    pub live_captions: Option<bool>,
    /// Sync mirror of the recording meeting id, used to filter events.
    pub active_recording_id: Option<i64>,
    /// In-flight (or failed) model / GPU-library download.
    pub download: Option<ModelDownloadProgress>,
    /// Bumped on every finished download, success or failure.
    pub download_done_tick: u64,
    /// Bumped when a data_dir change saves.
    pub settings_tick: u64,
    pub update: UpdateFlow,
    /// Bumped to tell views to reload their lists.
    pub refresh_tick: u64,
    /// Tray-flyout mode (window controls hidden); mirrors `FlyoutState`.
    pub popup_mode: bool,
    /// The live-caption overlay is open; mirrors `CaptionsOverlay::is_open`.
    pub captions_open: bool,
    /// Top-bar start/stop in flight (spinner, debounce).
    pub recording_action: bool,
    /// Tray Quit is stopping the recording and exiting: no new recordings.
    pub quitting: bool,
    /// First-run banner: Parakeet models not installed.
    pub models_missing: bool,
    pub record_hotkey: Option<String>,
    pub bookmark_hotkey: Option<String>,
    /// Notes writer acks per meeting: (last acked seq, last error).
    pub notes_acks: HashMap<i64, (u64, Option<String>)>,
    /// When the transcript's notes autosave fires (published via `UiCtx`).
    pub notes_deadline: Option<Instant>,
    /// The AI assistant's tasks per meeting: queued/running (with the
    /// latest stage) or failed. Finished tasks are removed (the meeting
    /// reload shows their result).
    pub ai_tasks: HashMap<(i64, AiTask), AiTaskState>,
    /// Whether a watcher status has been logged yet (acceptance aid).
    watcher_seen: bool,
}

/// One AI task of one meeting, as the transcript view shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiTaskState {
    /// Queued or running; the stage text ("Queued", "Writing notes"…).
    Busy(String),
    /// The last attempt failed.
    Failed(String),
}

impl ShellModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a recording is running, from the cached status copy.
    pub fn recording(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.recording)
    }

    /// Whether the pipeline is transcribing: a progress event since the
    /// last completion, or the status naming a meeting (the status covers a
    /// job that has not reported progress yet).
    pub fn transcribing(&self) -> bool {
        self.progress.is_some()
            || self
                .status
                .as_ref()
                .is_some_and(|s| s.transcribing_meeting_id.is_some())
    }

    /// Live captions are running for the active recording.
    pub fn live_captions_active(&self) -> bool {
        self.live_captions
            .or_else(|| self.status.as_ref().map(|s| s.live_captions))
            .unwrap_or(false)
    }

    /// `refreshStatus()`'s result.
    pub fn set_status(&mut self, status: AppStatus) {
        self.active_recording_id = if status.recording {
            status.meeting_id
        } else {
            None
        };
        self.status = Some(status);
    }

    /// The app.tsx event table. Returns the follow-ups to run.
    pub fn apply(&mut self, event: &CoreEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            CoreEvent::RecordingStarted(e) => {
                self.active_recording_id = Some(e.meeting_id);
                self.health = None;
                self.live_lines.clear();
                self.live_meeting_id = Some(e.meeting_id);
                self.live_captions = Some(e.live_captions);
                effects.push(Effect::RefreshStatus);
            }
            CoreEvent::LiveTranscript(line) => {
                if Some(line.meeting_id) == self.active_recording_id {
                    while self.live_lines.len() >= LIVE_LINES_CAP {
                        self.live_lines.pop_front();
                    }
                    self.live_lines.push_back(line.clone());
                }
            }
            CoreEvent::LiveCaptionsStatus(e) => {
                if Some(e.meeting_id) == self.active_recording_id {
                    self.live_captions = Some(e.active);
                    if let Some(status) = self.status.as_mut() {
                        if status.meeting_id == Some(e.meeting_id) {
                            status.live_captions = e.active;
                        }
                    }
                    if let Some(error) = &e.error {
                        effects.push(Effect::Notice(Notice::error(
                            "Live captions stopped",
                            error,
                        )));
                    }
                }
            }
            CoreEvent::RecordingStopped(e) => {
                if self.active_recording_id == Some(e.meeting_id) {
                    self.active_recording_id = None;
                }
                // live_lines / live_meeting_id are kept so the transcript can
                // still show them until the next start.
                self.level = None;
                self.health = None;
                self.live_captions = None;
                self.refresh_tick += 1;
                effects.push(Effect::RefreshStatus);
            }
            CoreEvent::RecordingLevel(level) => self.level = Some(level.clone()),
            CoreEvent::RecordingHealth(health) => {
                if health.mic.live_disconnected || health.loopback.live_disconnected {
                    self.live_captions = Some(false);
                }
                self.health = Some(health.clone());
            }
            CoreEvent::WatcherStatus(w) => {
                if !self.watcher_seen {
                    self.watcher_seen = true;
                    log::info!("watcher status reached the shell: {w:?}");
                } else {
                    log::debug!("watcher status: {w:?}");
                }
                if let Some(status) = self.status.as_mut() {
                    status.watcher = w.clone();
                }
            }
            CoreEvent::TranscriptionProgress(p) => self.progress = Some(p.clone()),
            CoreEvent::TranscriptionComplete(_) => {
                self.progress = None;
                self.refresh_tick += 1;
                effects.push(Effect::RefreshStatus);
            }
            CoreEvent::TranscriptionFailed(e) => {
                self.progress = None;
                self.refresh_tick += 1;
                effects.push(Effect::RefreshStatus);
                effects.push(Effect::Notice(Notice::error(
                    "Transcription failed",
                    e.error.as_deref().unwrap_or("Unknown error"),
                )));
            }
            CoreEvent::ModelDownloadProgress(p) => {
                self.download = if p.done && p.error.is_none() {
                    None
                } else {
                    Some(p.clone())
                };
                if p.done {
                    self.download_done_tick += 1;
                    match &p.error {
                        Some(error) => effects.push(Effect::Notice(Notice::error(
                            "Model download failed",
                            error,
                        ))),
                        None => effects.push(Effect::RecheckModels),
                    }
                }
            }
            CoreEvent::MeetingsChanged => self.refresh_tick += 1,
            CoreEvent::AiProgress(p) => {
                self.ai_tasks
                    .insert((p.meeting_id, p.task), AiTaskState::Busy(p.stage.clone()));
            }
            CoreEvent::AiDone(done) => {
                let key = (done.meeting_id, done.task);
                let what = match done.task {
                    AiTask::Title => "title",
                    AiTask::Summary => "summary",
                };
                match &done.outcome {
                    AiOutcome::Applied => {
                        self.ai_tasks.remove(&key);
                        if !done.automatic {
                            effects.push(Effect::Notice(Notice::success(match done.task {
                                AiTask::Title => "AI title applied",
                                AiTask::Summary => "AI summary ready",
                            })));
                        }
                    }
                    AiOutcome::Skipped(why) => {
                        self.ai_tasks.remove(&key);
                        // Automatic skips are routine (feature off, the user's
                        // own title); a manual one says why nothing happened.
                        if !done.automatic {
                            effects
                                .push(Effect::Notice(Notice::info(format!("No AI {what}: {why}"))));
                        }
                    }
                    AiOutcome::Failed(error) => {
                        self.ai_tasks
                            .insert(key, AiTaskState::Failed(error.clone()));
                        effects.push(Effect::Notice(Notice::error(
                            &format!("The AI {what} failed"),
                            error,
                        )));
                    }
                }
            }
            CoreEvent::UpdateProgress(p) => {
                if matches!(self.update.phase, UpdatePhase::Installing(_)) {
                    self.update.progress = Some(p.clone());
                }
            }
            CoreEvent::NotesSaved { meeting_id, seq } => {
                self.ack_notes(*meeting_id, *seq, None);
            }
            CoreEvent::NotesSaveFailed {
                meeting_id,
                seq,
                error,
            } => {
                // notes.tsx toasted every failed save, whether or not the
                // transcript was still open.
                if self.ack_notes(*meeting_id, *seq, Some(error.clone())) {
                    effects.push(Effect::Notice(Notice::error("Could not save notes", error)));
                }
            }
            // Shell side effects: handled by app.rs (tray, captions overlay).
            CoreEvent::TrayRecording(_)
            | CoreEvent::TrayWarning(_)
            | CoreEvent::OpenCaptionsOverlay { .. }
            | CoreEvent::CloseCaptionsOverlay => {}
        }
        effects
    }

    /// Record an ack; false if it is older than one already recorded.
    fn ack_notes(&mut self, meeting_id: i64, seq: u64, error: Option<String>) -> bool {
        let entry = self.notes_acks.entry(meeting_id).or_insert((0, None));
        // Acks arrive in seq order per meeting; never move backwards.
        if seq >= entry.0 {
            *entry = (seq, error);
            return true;
        }
        false
    }
}

/// Settings → Updates phases (updates.tsx `Phase`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    /// Checked: this is the newest release.
    Current,
    Available(UpdateInfo),
    /// Downloading/verifying, then the installer takes over.
    Installing(UpdateInfo),
}

/// App-owned update state: survives leaving Settings and hiding the window.
#[derive(Debug, Default)]
pub struct UpdateFlow {
    pub phase: UpdatePhase,
    /// Download progress while Installing.
    pub progress: Option<UpdateProgress>,
}

impl UpdateFlow {
    /// Show the startup check's result (Install button). The startup check
    /// runs 30 s after launch and emits no event, so `app.rs` calls this
    /// while Settings is showing (`pending_update` is [ui-ok]). Applies only
    /// from Idle / Current: never interrupts a check or an install.
    pub fn init(&mut self, core: &AppCore) {
        self.preload(core.pending_update());
    }

    fn preload(&mut self, pending: Option<UpdateInfo>) {
        if let Some(info) = pending {
            if matches!(self.phase, UpdatePhase::Idle | UpdatePhase::Current) {
                self.phase = UpdatePhase::Available(info);
            }
        }
    }

    /// Check for updates (no-op while checking or installing).
    pub fn check(&mut self, tasks: &Tasks) {
        if self.begin_check() {
            tasks.spawn_op_or(
                "update-check",
                |c| OpDone::UpdateChecked(c.check_for_update()),
                |error| OpDone::UpdateChecked(Err(error)),
            );
        }
    }

    /// Download + verify the available release; the install exit path
    /// runs when `OpDone::UpdateDownloaded(Ok(..))` arrives.
    pub fn install(&mut self, tasks: &Tasks) {
        if self.begin_install() {
            tasks.spawn_op_or(
                "update-download",
                |c| OpDone::UpdateDownloaded(c.download_update()),
                |error| OpDone::UpdateDownloaded(Err(error)),
            );
        }
    }

    fn begin_check(&mut self) -> bool {
        if matches!(
            self.phase,
            UpdatePhase::Checking | UpdatePhase::Installing(_)
        ) {
            return false;
        }
        self.phase = UpdatePhase::Checking;
        true
    }

    fn begin_install(&mut self) -> bool {
        let UpdatePhase::Available(info) = &self.phase else {
            return false;
        };
        self.phase = UpdatePhase::Installing(info.clone());
        self.progress = None;
        true
    }

    /// Apply a finished check/download. Returns the toasts to show. A
    /// successful download leaves the phase at Installing; `app.rs` then
    /// runs the install path and calls [`UpdateFlow::install_failed`] if
    /// it cannot launch the installer.
    pub fn on_op(&mut self, done: &OpDone) -> Vec<Notice> {
        let mut notices = Vec::new();
        match done {
            OpDone::UpdateChecked(Ok(Some(info))) => {
                self.phase = UpdatePhase::Available(info.clone());
            }
            OpDone::UpdateChecked(Ok(None)) => self.phase = UpdatePhase::Current,
            OpDone::UpdateChecked(Err(error)) => {
                self.phase = UpdatePhase::Idle;
                notices.push(Notice::error("Could not check for updates", error));
            }
            OpDone::UpdateDownloaded(Err(error)) => {
                self.install_failed();
                notices.push(Notice::error("Could not install the update", error));
            }
            _ => {}
        }
        notices
    }

    /// Back to Available after a failed download or launch.
    pub fn install_failed(&mut self) {
        if let UpdatePhase::Installing(info) = &self.phase {
            self.phase = UpdatePhase::Available(info.clone());
        }
        self.progress = None;
    }

    /// The Installing badge text.
    pub fn progress_text(&self) -> String {
        match &self.progress {
            Some(p) => match p.total.filter(|t| *t > 0) {
                Some(total) => format!(
                    "downloading {}%",
                    ((p.downloaded as f64 / total as f64) * 100.0).round()
                ),
                None => format!("downloading {} MB…", (p.downloaded as f64 / 1e6).round()),
            },
            None => "starting…".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::run_guarded;
    use witness_core::events::{
        LiveCaptionsStatus, RecordingStarted, RecordingStopped, TranscriptionDone, WatcherStatus,
    };

    fn watcher() -> WatcherStatus {
        WatcherStatus {
            enabled: true,
            teams_key_found: false,
            mic_in_use: false,
            suppressed: false,
        }
    }

    fn status(recording: bool, meeting_id: Option<i64>) -> AppStatus {
        AppStatus {
            recording,
            meeting_id,
            recording_since: None,
            live_captions: false,
            transcribing_meeting_id: None,
            queue_len: 0,
            processing_stage: None,
            processing_pct: None,
            watcher: watcher(),
        }
    }

    fn started(id: i64) -> CoreEvent {
        CoreEvent::RecordingStarted(RecordingStarted {
            meeting_id: id,
            trigger: "manual",
            started_at: "2026-10-05T10:00:00+01:00".into(),
            live_captions: true,
        })
    }

    fn line(id: i64, n: u64) -> CoreEvent {
        CoreEvent::LiveTranscript(LiveTranscript {
            meeting_id: id,
            track: "mic",
            start_ms: n,
            end_ms: n + 1,
            text: format!("line {n}"),
        })
    }

    #[test]
    fn recording_lifecycle_follows_the_event_table() {
        let mut m = ShellModel::new();
        assert_eq!(m.apply(&started(5)), vec![Effect::RefreshStatus]);
        assert_eq!(m.active_recording_id, Some(5));
        assert_eq!(m.live_captions, Some(true));
        m.apply(&line(5, 1));
        m.apply(&line(4, 2)); // another meeting: ignored
        assert_eq!(m.live_lines.len(), 1);

        let tick = m.refresh_tick;
        let effects = m.apply(&CoreEvent::RecordingStopped(RecordingStopped {
            meeting_id: 5,
        }));
        assert_eq!(effects, vec![Effect::RefreshStatus]);
        assert_eq!(m.active_recording_id, None);
        assert_eq!(m.live_captions, None);
        assert_eq!(m.refresh_tick, tick + 1);
        assert_eq!(m.live_lines.len(), 1, "lines are kept after stop");
        assert_eq!(m.live_meeting_id, Some(5));
    }

    #[test]
    fn live_lines_are_capped() {
        let mut m = ShellModel::new();
        m.apply(&started(1));
        for n in 0..(LIVE_LINES_CAP as u64 + 20) {
            m.apply(&line(1, n));
        }
        assert_eq!(m.live_lines.len(), LIVE_LINES_CAP);
        assert_eq!(m.live_lines.front().unwrap().start_ms, 20);
    }

    #[test]
    fn live_captions_status_patches_status_and_toasts_errors() {
        let mut m = ShellModel::new();
        m.set_status(status(true, Some(3)));
        assert_eq!(m.active_recording_id, Some(3));
        let effects = m.apply(&CoreEvent::LiveCaptionsStatus(LiveCaptionsStatus {
            meeting_id: 3,
            active: false,
            error: Some("model missing".into()),
        }));
        assert_eq!(
            effects,
            vec![Effect::Notice(Notice::error_plain(
                "Live captions stopped: model missing"
            ))]
        );
        assert_eq!(m.live_captions, Some(false));
        assert!(!m.live_captions_active());
        // Another meeting's status is ignored.
        assert!(m
            .apply(&CoreEvent::LiveCaptionsStatus(LiveCaptionsStatus {
                meeting_id: 9,
                active: true,
                error: None,
            }))
            .is_empty());
        assert_eq!(m.live_captions, Some(false));
    }

    #[test]
    fn transcription_events() {
        let mut m = ShellModel::new();
        let effects = m.apply(&CoreEvent::TranscriptionFailed(TranscriptionDone {
            meeting_id: 1,
            error: None,
        }));
        assert_eq!(
            effects,
            vec![
                Effect::RefreshStatus,
                Effect::Notice(Notice::error_plain("Transcription failed: Unknown error"))
            ]
        );
        assert_eq!(m.refresh_tick, 1);
        assert_eq!(
            m.apply(&CoreEvent::TranscriptionComplete(TranscriptionDone {
                meeting_id: 1,
                error: None,
            })),
            vec![Effect::RefreshStatus]
        );
    }

    #[test]
    fn transcribing_follows_progress_and_the_status() {
        let mut m = ShellModel::new();
        assert!(!m.transcribing());
        m.apply(&CoreEvent::TranscriptionProgress(TranscriptionProgress {
            meeting_id: 1,
            stage: "asr",
            pct: 0.5,
        }));
        assert!(m.transcribing());
        m.apply(&CoreEvent::TranscriptionComplete(TranscriptionDone {
            meeting_id: 1,
            error: None,
        }));
        assert!(!m.transcribing());
        m.set_status(AppStatus {
            transcribing_meeting_id: Some(2),
            ..status(false, None)
        });
        assert!(m.transcribing());
    }

    #[test]
    fn downloads_bump_the_done_tick_and_recheck_models() {
        let mut m = ShellModel::new();
        let p = |done, error: Option<&str>| {
            CoreEvent::ModelDownloadProgress(ModelDownloadProgress {
                model_id: "parakeet".into(),
                file: "encoder.onnx".into(),
                downloaded_bytes: 1,
                total_bytes: Some(2),
                file_index: 1,
                file_count: 3,
                done,
                error: error.map(String::from),
            })
        };
        assert!(m.apply(&p(false, None)).is_empty());
        assert!(m.download.is_some());
        assert_eq!(m.apply(&p(true, None)), vec![Effect::RecheckModels]);
        assert!(m.download.is_none());
        assert_eq!(m.download_done_tick, 1);
        assert_eq!(
            m.apply(&p(true, Some("offline"))),
            vec![Effect::Notice(Notice::error_plain(
                "Model download failed: offline"
            ))]
        );
        assert!(m.download.is_some(), "a failed download stays visible");
        assert_eq!(m.download_done_tick, 2);
    }

    #[test]
    fn watcher_status_patches_a_loaded_status_only() {
        let mut m = ShellModel::new();
        let mut w = watcher();
        w.mic_in_use = true;
        m.apply(&CoreEvent::WatcherStatus(w.clone()));
        assert!(m.status.is_none());
        m.set_status(status(false, None));
        m.apply(&CoreEvent::WatcherStatus(w));
        assert!(m.status.as_ref().unwrap().watcher.mic_in_use);
    }

    #[test]
    fn ai_events_track_tasks_and_toast_what_matters() {
        use witness_core::events::{AiDone, AiProgress};
        let mut m = ShellModel::new();
        let progress = |stage: &str| {
            CoreEvent::AiProgress(AiProgress {
                meeting_id: 4,
                task: AiTask::Summary,
                stage: stage.into(),
            })
        };
        let done = |automatic, outcome| {
            CoreEvent::AiDone(AiDone {
                meeting_id: 4,
                task: AiTask::Summary,
                automatic,
                outcome,
            })
        };
        assert!(m.apply(&progress("Queued")).is_empty());
        m.apply(&progress("Writing notes"));
        assert_eq!(
            m.ai_tasks.get(&(4, AiTask::Summary)),
            Some(&AiTaskState::Busy("Writing notes".into()))
        );
        // A manual success clears the task and says so.
        let effects = m.apply(&done(false, AiOutcome::Applied));
        assert!(m.ai_tasks.is_empty());
        assert!(matches!(&effects[..], [Effect::Notice(n)] if n.message == "AI summary ready"));
        // Automatic success and skips are quiet.
        m.apply(&progress("Queued"));
        assert!(m.apply(&done(true, AiOutcome::Applied)).is_empty());
        assert!(m
            .apply(&done(true, AiOutcome::Skipped("off".into())))
            .is_empty());
        // A manual skip explains itself.
        let effects = m.apply(&done(false, AiOutcome::Skipped("no transcript".into())));
        assert!(
            matches!(&effects[..], [Effect::Notice(n)] if n.message == "No AI summary: no transcript")
        );
        // Failures stay visible on the meeting and toast.
        let effects = m.apply(&done(true, AiOutcome::Failed("429".into())));
        assert_eq!(
            m.ai_tasks.get(&(4, AiTask::Summary)),
            Some(&AiTaskState::Failed("429".into()))
        );
        assert!(
            matches!(&effects[..], [Effect::Notice(n)] if n.message == "The AI summary failed: 429")
        );
        // A new attempt replaces the failure.
        m.apply(&progress("Queued"));
        assert!(matches!(
            m.ai_tasks.get(&(4, AiTask::Summary)),
            Some(AiTaskState::Busy(_))
        ));
    }

    #[test]
    fn notes_acks_never_go_backwards() {
        let mut m = ShellModel::new();
        m.apply(&CoreEvent::NotesSaved {
            meeting_id: 1,
            seq: 4,
        });
        m.apply(&CoreEvent::NotesSaveFailed {
            meeting_id: 1,
            seq: 3,
            error: "late".into(),
        });
        assert_eq!(m.notes_acks[&1], (4, None));
        let effects = m.apply(&CoreEvent::NotesSaveFailed {
            meeting_id: 1,
            seq: 5,
            error: "disk full".into(),
        });
        assert_eq!(m.notes_acks[&1], (5, Some("disk full".into())));
        assert_eq!(
            effects,
            vec![Effect::Notice(Notice::error_plain(
                "Could not save notes: disk full"
            ))]
        );
    }

    fn info(version: &str) -> UpdateInfo {
        UpdateInfo {
            current_version: "0.2.1".into(),
            version: version.into(),
            notes: Some("Fixes".into()),
            date: None,
        }
    }

    #[test]
    fn update_flow_phases() {
        let mut flow = UpdateFlow::default();
        assert!(!flow.begin_install(), "nothing to install yet");
        assert!(flow.begin_check());
        assert!(!flow.begin_check(), "one check at a time");
        assert!(flow.on_op(&OpDone::UpdateChecked(Ok(None))).is_empty());
        assert_eq!(flow.phase, UpdatePhase::Current);

        assert!(flow.begin_check());
        let notices = flow.on_op(&OpDone::UpdateChecked(Err("offline".into())));
        assert_eq!(
            notices,
            vec![Notice::error_plain("Could not check for updates: offline")]
        );
        assert_eq!(flow.phase, UpdatePhase::Idle);

        assert!(flow.begin_check());
        flow.on_op(&OpDone::UpdateChecked(Ok(Some(info("0.3.0")))));
        assert_eq!(flow.phase, UpdatePhase::Available(info("0.3.0")));
        assert!(flow.begin_install());
        assert_eq!(flow.phase, UpdatePhase::Installing(info("0.3.0")));
        assert_eq!(flow.progress_text(), "starting…");

        let mut m = ShellModel::new();
        m.update = flow;
        m.apply(&CoreEvent::UpdateProgress(UpdateProgress {
            downloaded: 5_000_000,
            total: Some(20_000_000),
        }));
        assert_eq!(m.update.progress_text(), "downloading 25%");
        m.update.progress = Some(UpdateProgress {
            downloaded: 7_400_000,
            total: None,
        });
        assert_eq!(m.update.progress_text(), "downloading 7 MB…");

        let notices = m
            .update
            .on_op(&OpDone::UpdateDownloaded(Err("bad signature".into())));
        assert_eq!(
            notices,
            vec![Notice::error_plain(
                "Could not install the update: bad signature"
            )]
        );
        assert_eq!(m.update.phase, UpdatePhase::Available(info("0.3.0")));
        assert!(m.update.progress.is_none());
    }

    #[test]
    fn preload_never_interrupts_a_check_or_an_install() {
        let mut flow = UpdateFlow::default();
        flow.preload(None);
        assert_eq!(flow.phase, UpdatePhase::Idle);
        flow.preload(Some(info("0.3.0")));
        assert_eq!(flow.phase, UpdatePhase::Available(info("0.3.0")));

        assert!(flow.begin_install());
        flow.preload(Some(info("0.3.0")));
        assert_eq!(flow.phase, UpdatePhase::Installing(info("0.3.0")));

        let mut flow = UpdateFlow::default();
        assert!(flow.begin_check());
        flow.preload(Some(info("0.3.0")));
        assert_eq!(flow.phase, UpdatePhase::Checking);
        flow.on_op(&OpDone::UpdateChecked(Ok(None)));
        flow.preload(Some(info("0.3.1")));
        assert_eq!(flow.phase, UpdatePhase::Available(info("0.3.1")));
    }

    #[test]
    fn panicking_update_ops_release_the_flow() {
        let check = |_: &()| -> OpDone { panic!("feed parser") };
        let mut flow = UpdateFlow::default();
        assert!(flow.begin_check());
        let done = run_guarded("update-check", &(), check, |e| {
            OpDone::UpdateChecked(Err(e))
        });
        let notices = flow.on_op(&done);
        assert_eq!(flow.phase, UpdatePhase::Idle, "Check button back");
        assert_eq!(
            notices,
            vec![Notice::error_plain(
                "Could not check for updates: internal error: feed parser"
            )]
        );

        let download = |_: &()| -> OpDone { panic!("zip") };
        flow.preload(Some(info("0.3.0")));
        assert!(flow.begin_install());
        let done = run_guarded("update-download", &(), download, |e| {
            OpDone::UpdateDownloaded(Err(e))
        });
        flow.on_op(&done);
        assert_eq!(flow.phase, UpdatePhase::Available(info("0.3.0")));
    }
}
