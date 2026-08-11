//! Tauri command handlers + the shared start/stop logic used by the tray
//! and the meeting watcher. Never touches WASAPI directly — capture runs on
//! its own threads owned by the recorder.

use crate::db::{Meeting, SearchHit, Segment, Speaker};
use crate::events;
use crate::models::ModelInfo;
use crate::pipeline::Job;
use crate::settings::{Engine, Settings};
use crate::state::{AppState, RecorderState};
use crate::{models, recorder, tray};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

fn err<E: std::fmt::Display>(e: E) -> String {
    format!("{e:#}")
}

pub fn toast(app: &AppHandle, body: &str) {
    let _ = app
        .notification()
        .builder()
        .title("Witness")
        .body(body)
        .show();
}

// ---------- shared recording control ----------

/// Identity of a capture problem, independent of the numbers in its message.
/// Health is re-evaluated every 500 ms and the dropped-audio counters only
/// grow, so a message-level comparison would treat every tick of an ongoing
/// problem as new — deduplicating the OS toast needs this stable key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarningKind {
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
fn capture_warning(health: &crate::recorder::RecordingHealth) -> Option<(WarningKind, String)> {
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

pub fn do_start_recording(app: &AppHandle, trigger: &'static str) -> Result<i64, String> {
    let state = app.state::<AppState>();
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
        let live_app = app.clone();
        let status_app = app.clone();
        let tx = crate::live_transcribe::spawn(
            models_dir,
            junk_phrases,
            move |track, start_ms, end_ms, text| {
                let _ = live_app.emit(
                    events::LIVE_TRANSCRIPT,
                    events::LiveTranscript {
                        meeting_id: id,
                        track,
                        start_ms,
                        end_ms,
                        text,
                    },
                );
            },
            move |result| {
                let active = result.is_ok();
                let status_state = status_app.state::<AppState>();
                if status_state
                    .live_captions_meeting_id
                    .load(Ordering::Relaxed)
                    != id
                {
                    return;
                }
                status_state.live_captions.store(active, Ordering::Relaxed);
                let error = result.err();
                let _ = status_app.emit(
                    events::LIVE_CAPTIONS_STATUS,
                    events::LiveCaptionsStatus {
                        meeting_id: id,
                        active,
                        error: error.clone(),
                    },
                );
                if active && overlay {
                    open_captions_overlay(&status_app);
                } else if let Some(error) = error {
                    toast(
                        &status_app,
                        &format!("Live captions could not start: {error}"),
                    );
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
    let level_app = app.clone();
    let health_app = app.clone();
    *state.recording_health.lock().unwrap() = None;
    let handle = recorder::start_with_health(
        &rec_dir,
        id,
        mic_device,
        loopback_device,
        live_tx,
        move |mic_rms, loopback_rms, elapsed_ms| {
            let _ = level_app.emit(
                events::RECORDING_LEVEL,
                events::RecordingLevel {
                    mic_rms,
                    loopback_rms,
                    elapsed_ms,
                },
            );
        },
        move |health| {
            let health_state = health_app.state::<AppState>();
            let previous = health_state
                .recording_health
                .lock()
                .unwrap()
                .replace(health.clone());
            if (health.mic.live_disconnected || health.loopback.live_disconnected)
                && health_state.live_captions.swap(false, Ordering::Relaxed)
            {
                let _ = health_app.emit(
                    events::LIVE_CAPTIONS_STATUS,
                    events::LiveCaptionsStatus {
                        meeting_id: id,
                        active: false,
                        error: Some("The live-caption worker disconnected".into()),
                    },
                );
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
                tray::set_recording_warning(&health_app, text);
            }
            if warning.as_ref().map(|(kind, _)| *kind)
                != previous_warning.as_ref().map(|(kind, _)| *kind)
            {
                if let Some(text) = text {
                    toast(&health_app, text);
                }
            }
            let _ = health_app.emit(events::RECORDING_HEALTH, health);
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
            close_captions_overlay(app);
            *state.recorder.lock().unwrap() = RecorderState::Idle;
            return Err(err(e));
        }
    };
    *state.recording_trigger.lock().unwrap() = trigger;
    *state.recorder.lock().unwrap() = RecorderState::Recording(handle);

    let live_started = state.live_captions.load(Ordering::Relaxed);

    let _ = app.emit(
        events::RECORDING_STARTED,
        events::RecordingStarted {
            meeting_id: id,
            trigger,
            started_at: now.to_rfc3339(),
            live_captions: live_started,
        },
    );
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    tray::update(app, true);

    // Auto-recorded meetings: try to pick up the meeting subject from the
    // Teams window title (it settles a little after joining).
    if trigger == "auto" {
        let title_app = app.clone();
        std::thread::Builder::new()
            .name("meeting-title".into())
            .spawn(move || {
                for delay_s in [12u64, 30, 60] {
                    std::thread::sleep(std::time::Duration::from_secs(delay_s));
                    let state = title_app.state::<AppState>();
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
                        let _ = title_app.emit(events::MEETINGS_CHANGED, ());
                        return;
                    }
                }
            })
            .ok();
    }

    log::info!("recording started: meeting {id} ({trigger})");
    Ok(id)
}

const CAPTIONS_WINDOW: &str = "captions";

/// Outer position/size of the captions overlay, persisted per data_dir so
/// the user's arrangement survives across recordings and restarts. Not a
/// Settings field: update_settings round-trips through the frontend and
/// would silently reset fields it does not know about.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct OverlayGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

static OVERLAY_GEOMETRY: std::sync::Mutex<Option<OverlayGeometry>> = std::sync::Mutex::new(None);

fn overlay_geometry_path(app: &AppHandle) -> std::path::PathBuf {
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    settings.data_dir().join("captions-overlay.toml")
}

/// Track the overlay's outer geometry while it is open (moved/resized).
pub fn remember_overlay_geometry(
    position: tauri::PhysicalPosition<i32>,
    size: tauri::PhysicalSize<u32>,
) {
    *OVERLAY_GEOMETRY.lock().unwrap() = Some(OverlayGeometry {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    });
}

/// Write the last known overlay geometry to disk (also called from the
/// Destroyed window event, which covers the Escape / close-button path).
pub fn save_overlay_geometry(app: &AppHandle) {
    let Some(geometry) = *OVERLAY_GEOMETRY.lock().unwrap() else {
        return;
    };
    match toml::to_string(&geometry) {
        Ok(text) => {
            if let Err(e) = std::fs::write(overlay_geometry_path(app), text) {
                log::warn!("could not save overlay geometry: {e}");
            }
        }
        Err(e) => log::warn!("could not serialize overlay geometry: {e}"),
    }
}

fn load_overlay_geometry(app: &AppHandle) -> Option<OverlayGeometry> {
    if let Some(geometry) = *OVERLAY_GEOMETRY.lock().unwrap() {
        return Some(geometry);
    }
    let text = std::fs::read_to_string(overlay_geometry_path(app)).ok()?;
    toml::from_str(&text).ok()
}

fn open_captions_overlay(app: &AppHandle) {
    if app.get_webview_window(CAPTIONS_WINDOW).is_some() {
        return;
    }
    // Restore the last arrangement, clamped to its monitor's work area (same
    // rule as the tray flyout) so a display change cannot strand it offscreen.
    let restore = load_overlay_geometry(app).and_then(|g| {
        let center_x = g.x as f64 + g.width as f64 / 2.0;
        let center_y = g.y as f64 + g.height as f64 / 2.0;
        let monitor = app.monitor_from_point(center_x, center_y).ok().flatten()?;
        let area = monitor.work_area();
        let (ax, ay) = (area.position.x, area.position.y);
        let (aw, ah) = (area.size.width as i32, area.size.height as i32);
        let width = (g.width as i32).min(aw).max(1);
        let height = (g.height as i32).min(ah).max(1);
        Some(OverlayGeometry {
            x: g.x.clamp(ax, (ax + aw - width).max(ax)),
            y: g.y.clamp(ay, (ay + ah - height).max(ay)),
            width: width as u32,
            height: height as u32,
        })
    });
    let result = tauri::WebviewWindowBuilder::new(
        app,
        CAPTIONS_WINDOW,
        tauri::WebviewUrl::App("index.html#captions".into()),
    )
    .title("Witness — live captions")
    .inner_size(560.0, 150.0)
    .min_inner_size(320.0, 90.0)
    .always_on_top(true)
    .decorations(false)
    .skip_taskbar(true)
    // Auto-record fires right as the user joins a call; never take the
    // keyboard focus away from Teams.
    .focused(false)
    .zoom_hotkeys_enabled(true)
    .background_color(tauri::webview::Color(12, 13, 16, 255))
    .build();
    match result {
        Ok(window) => {
            if let Some(g) = restore {
                let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize {
                    width: g.width,
                    height: g.height,
                }));
                let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                    x: g.x,
                    y: g.y,
                }));
            }
        }
        Err(e) => log::warn!("captions overlay failed to open: {e}"),
    }
}

fn close_captions_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(CAPTIONS_WINDOW) {
        // Persist synchronously: on tray Quit the Destroyed event may never
        // be processed before the process exits.
        if let (Ok(position), Ok(size)) = (w.outer_position(), w.outer_size()) {
            remember_overlay_geometry(position, size);
        }
        save_overlay_geometry(app);
        let _ = w.close();
    }
}

/// Re-open (or surface) the captions overlay after it was closed mid-meeting.
#[tauri::command]
pub fn show_captions_overlay(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    if !state.recorder.lock().unwrap().is_recording() {
        return Err("Not recording".into());
    }
    if !state.live_captions.load(Ordering::Relaxed) {
        return Err("Live captions are not available for this recording".into());
    }
    if let Some(w) = app.get_webview_window(CAPTIONS_WINDOW) {
        let _ = w.show();
        let _ = w.set_focus();
    } else {
        open_captions_overlay(&app);
    }
    Ok(())
}

pub fn do_stop_recording(app: &AppHandle, by_user: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
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
                return Err("Recording is still starting".into());
            }
            stopping @ RecorderState::Stopping { .. } => {
                *recorder = stopping;
                return Err("Recording is already stopping".into());
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
            tray::update(app, false);
            close_captions_overlay(app);
            let _ = app.emit(
                events::RECORDING_STOPPED,
                events::RecordingStopped { meeting_id: id },
            );
            let _ = app.emit(events::MEETINGS_CHANGED, ());
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
        tray::update(app, false);
        close_captions_overlay(app);
        let _ = app.emit(
            events::RECORDING_STOPPED,
            events::RecordingStopped { meeting_id: id },
        );
        let _ = app.emit(events::MEETINGS_CHANGED, ());
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
    let _ = app.emit(
        events::RECORDING_STOPPED,
        events::RecordingStopped { meeting_id: id },
    );
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    tray::update(app, false);
    close_captions_overlay(app);

    enqueue_result.map_err(|error| {
            format!(
                "Recording was saved, but processing could not be queued: {error:#}. Restart Witness to retry it."
            )
        })?;
    log::info!("recording stopped: meeting {id} ({} ms)", stats.duration_ms);
    Ok(())
}

// ---------- status ----------

#[derive(Debug, Serialize)]
pub struct AppStatus {
    pub recording: bool,
    pub meeting_id: Option<i64>,
    pub recording_since: Option<String>,
    /// True iff the active recording has a live-caption session.
    pub live_captions: bool,
    pub transcribing_meeting_id: Option<i64>,
    pub queue_len: usize,
    pub processing_stage: Option<String>,
    pub processing_pct: Option<f32>,
    pub watcher: events::WatcherStatus,
}

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> AppStatus {
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

#[derive(Debug, Serialize)]
pub struct DiagnosticDatabase {
    pub path: String,
    pub healthy: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticModel {
    pub id: String,
    pub present: bool,
    pub expected_revision: String,
    pub installed_revision: Option<String>,
    pub integrity_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticGpu {
    pub driver_present: bool,
    pub cuda_ready: bool,
    pub missing_dlls: Vec<String>,
    pub managed_libraries_present: bool,
    pub managed_libraries_integrity_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Diagnostics {
    pub generated_at: String,
    pub app_version: String,
    pub platform: String,
    pub runtime_data_dir: String,
    pub configured_data_dir: Option<String>,
    pub restart_required: bool,
    pub database: DiagnosticDatabase,
    pub log_path: String,
    pub recording_state: String,
    pub recording_meeting_id: Option<i64>,
    pub recording_since: Option<String>,
    pub capture_health: Option<crate::recorder::RecordingHealth>,
    pub capture_health_is_current: bool,
    pub processing_meeting_id: Option<i64>,
    pub processing_stage: Option<String>,
    pub processing_pct: Option<f32>,
    pub queue_len: usize,
    pub models: Vec<DiagnosticModel>,
    pub gpu: DiagnosticGpu,
    /// A transcript/audio-free text representation suitable for support.
    pub report: String,
}

fn check_database(path: &std::path::Path) -> DiagnosticDatabase {
    use rusqlite::OpenFlags;

    let result = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|connection| connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0)));
    match result {
        Ok(detail) => DiagnosticDatabase {
            path: path.display().to_string(),
            healthy: detail == "ok",
            detail,
        },
        Err(error) => DiagnosticDatabase {
            path: path.display().to_string(),
            healthy: false,
            detail: error.to_string(),
        },
    }
}

fn render_diagnostics(diagnostics: &Diagnostics) -> String {
    use std::fmt::Write;

    let mut text = String::new();
    let _ = writeln!(text, "Witness diagnostics");
    let _ = writeln!(text, "Generated: {}", diagnostics.generated_at);
    let _ = writeln!(text, "Version: {}", diagnostics.app_version);
    let _ = writeln!(text, "Platform: {}", diagnostics.platform);
    let _ = writeln!(
        text,
        "Runtime data directory: {}",
        diagnostics.runtime_data_dir
    );
    let _ = writeln!(
        text,
        "Configured data directory: {}",
        diagnostics
            .configured_data_dir
            .as_deref()
            .unwrap_or("default")
    );
    let _ = writeln!(text, "Restart required: {}", diagnostics.restart_required);
    let _ = writeln!(text, "Database: {}", diagnostics.database.path);
    let _ = writeln!(
        text,
        "Database health: {} ({})",
        if diagnostics.database.healthy {
            "ok"
        } else {
            "failed"
        },
        diagnostics.database.detail
    );
    let _ = writeln!(text, "Log: {}", diagnostics.log_path);
    let _ = writeln!(text, "Recording state: {}", diagnostics.recording_state);
    let _ = writeln!(
        text,
        "Recording meeting: {}",
        diagnostics
            .recording_meeting_id
            .map_or_else(|| "none".into(), |id| id.to_string())
    );
    let _ = writeln!(
        text,
        "Processing: meeting={}, stage={}, progress={}, queued={}",
        diagnostics
            .processing_meeting_id
            .map_or_else(|| "none".into(), |id| id.to_string()),
        diagnostics.processing_stage.as_deref().unwrap_or("idle"),
        diagnostics
            .processing_pct
            .map_or_else(|| "n/a".into(), |pct| format!("{pct:.1}%")),
        diagnostics.queue_len
    );
    if let Some(health) = &diagnostics.capture_health {
        let _ = writeln!(
            text,
            "Capture health ({}): mic connected={}, losses={}, dropped={} ms; loopback connected={}, losses={}, dropped={} ms; writer error={}",
            if diagnostics.capture_health_is_current { "current" } else { "last recording" },
            health.mic.connected,
            health.mic.device_loss_count,
            health.mic.capture_dropped_ms,
            health.loopback.connected,
            health.loopback.device_loss_count,
            health.loopback.capture_dropped_ms,
            health.writer_error.as_deref().unwrap_or("none")
        );
    } else {
        let _ = writeln!(text, "Capture health: not available in this session");
    }
    let _ = writeln!(text, "Models:");
    for model in &diagnostics.models {
        let _ = writeln!(
            text,
            "- {}: {}, expected={}, installed={}, integrity={}",
            model.id,
            if model.present { "present" } else { "missing" },
            model.expected_revision,
            model.installed_revision.as_deref().unwrap_or("none"),
            model.integrity_error.as_deref().unwrap_or("ok")
        );
    }
    let _ = writeln!(
        text,
        "GPU: driver={}, CUDA ready={}, missing DLLs={}, managed libraries={}, managed integrity={}",
        diagnostics.gpu.driver_present,
        diagnostics.gpu.cuda_ready,
        if diagnostics.gpu.missing_dlls.is_empty() {
            "none".into()
        } else {
            diagnostics.gpu.missing_dlls.join(", ")
        },
        diagnostics.gpu.managed_libraries_present,
        diagnostics
            .gpu
            .managed_libraries_integrity_error
            .as_deref()
            .unwrap_or("ok")
    );
    text
}

fn collect_diagnostics(state: &AppState) -> Diagnostics {
    let (runtime_data_dir, db_path, models_dir, cuda_dir) = {
        let settings = state.settings.lock().unwrap();
        (
            settings.data_dir(),
            settings.db_path(),
            settings.models_dir(),
            settings.cuda_dir(),
        )
    };
    let configured_data_dir = state.configured_data_dir.lock().unwrap().clone();
    let restart_required = configured_data_dir
        .as_deref()
        .map(std::path::Path::new)
        .is_some_and(|configured| configured != runtime_data_dir);
    let (recording_state, recording_meeting_id, recording_since, recording) = {
        let recorder = state.recorder.lock().unwrap();
        let name = match &*recorder {
            RecorderState::Idle => "idle",
            RecorderState::Starting => "starting",
            RecorderState::Recording(_) => "recording",
            RecorderState::Stopping { .. } => "stopping",
        };
        (
            name.to_string(),
            recorder.meeting_id(),
            recorder.started_at().map(|started| started.to_rfc3339()),
            recorder.is_recording(),
        )
    };
    let progress = state.pipeline.progress();
    let models = models::status(&models_dir)
        .into_iter()
        .map(|model| DiagnosticModel {
            id: model.id,
            present: model.present,
            expected_revision: model.expected_revision,
            installed_revision: model.installed_revision,
            integrity_error: model.integrity_error,
        })
        .collect();
    let gpu_preflight = crate::gpu::preflight();
    let managed_gpu = crate::gpu_libs::status(&cuda_dir);
    let mut diagnostics = Diagnostics {
        generated_at: chrono::Local::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        platform: format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH),
        runtime_data_dir: runtime_data_dir.display().to_string(),
        configured_data_dir,
        restart_required,
        database: check_database(&db_path),
        log_path: runtime_data_dir.join("witness.log").display().to_string(),
        recording_state,
        recording_meeting_id,
        recording_since,
        capture_health: state.recording_health.lock().unwrap().clone(),
        capture_health_is_current: recording,
        processing_meeting_id: state.pipeline.current_meeting(),
        processing_stage: progress.as_ref().map(|progress| progress.stage.clone()),
        processing_pct: progress.map(|progress| progress.pct),
        queue_len: state.pipeline.queue_len(),
        models,
        gpu: DiagnosticGpu {
            driver_present: gpu_preflight.driver_present,
            cuda_ready: gpu_preflight.ready(),
            missing_dlls: gpu_preflight.missing_dlls,
            managed_libraries_present: managed_gpu.present,
            managed_libraries_integrity_error: managed_gpu.integrity_error,
        },
        report: String::new(),
    };
    diagnostics.report = render_diagnostics(&diagnostics);
    diagnostics
}

#[tauri::command]
pub fn get_diagnostics(state: State<'_, AppState>) -> Diagnostics {
    collect_diagnostics(&state)
}

/// Save a privacy-conscious support report. It contains paths and operational
/// state, but never transcript text, notes, audio, voice prints, or settings.
#[tauri::command]
pub async fn export_diagnostics(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    let diagnostics = collect_diagnostics(&app.state::<AppState>());
    let filename = format!(
        "Witness-diagnostics-{}.txt",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let picked = app
        .dialog()
        .file()
        .set_file_name(filename)
        .add_filter("Text report", &["txt"])
        .blocking_save_file();
    let Some(destination) = picked.and_then(|file| file.into_path().ok()) else {
        return Ok(false);
    };
    std::fs::write(destination, diagnostics.report).map_err(err)?;
    Ok(true)
}

struct BackupActivityGuard<'a>(&'a std::sync::Mutex<bool>);

impl Drop for BackupActivityGuard<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = false;
    }
}

/// Pick a parent folder and publish a complete, checksummed recovery snapshot.
/// The native models are intentionally omitted; pinned model provenance is
/// included and the binaries can be re-downloaded after a restore.
#[tauri::command]
pub async fn create_backup(app: AppHandle) -> Result<Option<crate::backup::BackupSummary>, String> {
    use tauri_plugin_dialog::DialogExt;

    let picked = app
        .dialog()
        .file()
        .set_title("Choose where to create the Witness backup")
        .blocking_pick_folder();
    let Some(parent) = picked.and_then(|folder| folder.into_path().ok()) else {
        return Ok(None);
    };

    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let recorder = state.recorder.lock().unwrap();
        let mut backup_in_progress = state.backup_in_progress.lock().unwrap();
        if *backup_in_progress {
            return Err("A backup is already in progress".into());
        }
        if recorder.is_busy() {
            return Err("Stop the current recording before creating a backup".into());
        }
        *backup_in_progress = true;
        drop(backup_in_progress);
        drop(recorder);
        let _activity = BackupActivityGuard(&state.backup_in_progress);

        if state.pipeline.current_meeting().is_some() || state.pipeline.queue_len() != 0 {
            return Err(
                "Wait for queued meeting processing to finish before creating a backup".into(),
            );
        }

        let (mut settings, data_dir, models_dir) = {
            let settings = state.settings.lock().unwrap();
            (settings.clone(), settings.data_dir(), settings.models_dir())
        };
        settings.data_dir = state.configured_data_dir.lock().unwrap().clone();
        let model_status = models::status(&models_dir);
        crate::backup::create(
            &parent,
            &crate::backup::BackupInput {
                db: &state.db,
                settings: &settings,
                data_dir: &data_dir,
                model_status: &model_status,
                app_version: env!("CARGO_PKG_VERSION"),
            },
        )
        .map(Some)
        .map_err(err)
    })
    .await
    .map_err(|error| format!("backup task failed: {error}"))?
}

// ---------- recording commands ----------

#[tauri::command]
pub async fn start_recording(app: AppHandle) -> Result<i64, String> {
    do_start_recording(&app, "manual")
}

#[tauri::command]
pub async fn stop_recording(app: AppHandle) -> Result<(), String> {
    do_stop_recording(&app, true)
}

// ---------- meetings ----------

#[tauri::command]
pub fn list_meetings(
    state: State<'_, AppState>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Meeting>, String> {
    state
        .db
        .list_meetings(offset, limit.clamp(1, 500))
        .map_err(err)
}

#[derive(Debug, Serialize)]
pub struct MeetingDetail {
    pub meeting: Meeting,
    pub speakers: Vec<Speaker>,
    pub segments: Vec<Segment>,
    pub bookmarks: Vec<crate::db::Bookmark>,
    /// Why the most recent pipeline job for this meeting failed (None when
    /// there is none or it succeeded).
    pub last_error: Option<String>,
    /// This meeting has a runnable job waiting in the durable pipeline queue.
    pub pipeline_queued: bool,
}

#[tauri::command]
pub fn get_meeting(state: State<'_, AppState>, id: i64) -> Result<MeetingDetail, String> {
    let meeting = state
        .db
        .get_meeting(id)
        .map_err(err)?
        .ok_or_else(|| format!("meeting {id} not found"))?;
    Ok(MeetingDetail {
        speakers: state.db.get_speakers(id).map_err(err)?,
        segments: state.db.get_segments(id).map_err(err)?,
        bookmarks: state.db.list_bookmarks(id).map_err(err)?,
        last_error: state.db.pipeline_job_error(id).map_err(err)?,
        pipeline_queued: state.db.pipeline_job_queued(id).map_err(err)?,
        meeting,
    })
}

#[tauri::command]
pub fn set_meeting_notes(
    state: State<'_, AppState>,
    meeting_id: i64,
    notes: String,
) -> Result<(), String> {
    state.db.set_notes(meeting_id, &notes).map_err(err)
}

// ---------- bookmarks ----------

/// Flag "this moment" in the active recording (hotkey + top-bar button).
pub fn do_bookmark_now(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
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
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    let secs = at_ms / 1000;
    toast(app, &format!("Bookmarked {}:{:02}", secs / 60, secs % 60));
    Ok(())
}

#[tauri::command]
pub fn bookmark_now(app: AppHandle) -> Result<(), String> {
    do_bookmark_now(&app)
}

#[tauri::command]
pub fn add_bookmark(
    app: AppHandle,
    state: State<'_, AppState>,
    meeting_id: i64,
    at_ms: i64,
    note: String,
) -> Result<i64, String> {
    let id = state
        .db
        .add_bookmark(meeting_id, at_ms, &note)
        .map_err(err)?;
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    Ok(id)
}

#[tauri::command]
pub fn set_bookmark_note(
    state: State<'_, AppState>,
    bookmark_id: i64,
    note: String,
) -> Result<(), String> {
    state.db.set_bookmark_note(bookmark_id, &note).map_err(err)
}

#[tauri::command]
pub fn delete_bookmark(state: State<'_, AppState>, bookmark_id: i64) -> Result<(), String> {
    state.db.delete_bookmark(bookmark_id).map_err(err)
}

/// "Delete" = move to the recycle bin (auto-purged after 30 days).
#[tauri::command]
pub fn delete_meeting(app: AppHandle, state: State<'_, AppState>, id: i64) -> Result<(), String> {
    if state.recorder.lock().unwrap().meeting_id() == Some(id) {
        return Err("Stop the recording before deleting this meeting".into());
    }
    state.db.soft_delete_meeting(id).map_err(err)?;
    if let Err(error) = state.pipeline.cancel(id) {
        // A worker already owns this meeting. Keep it visible and let the
        // user retry deletion after processing reaches a safe boundary.
        let _ = state.db.restore_meeting(id);
        return Err(format!("Cannot delete this meeting yet: {error:#}"));
    }
    rematch_speakers(app.clone());
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    Ok(())
}

#[tauri::command]
pub fn restore_meeting(app: AppHandle, state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.db.restore_meeting(id).map_err(err)?;
    rematch_speakers(app.clone());
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct DeletedMeeting {
    #[serde(flatten)]
    pub meeting: Meeting,
    pub deleted_at: String,
}

#[tauri::command]
pub fn list_deleted_meetings(state: State<'_, AppState>) -> Result<Vec<DeletedMeeting>, String> {
    Ok(state
        .db
        .list_deleted_meetings()
        .map_err(err)?
        .into_iter()
        .map(|(meeting, deleted_at)| DeletedMeeting {
            meeting,
            deleted_at,
        })
        .collect())
}

/// Permanent removal: audio + WAVs + rows. Shared by the bin UI and the
/// 30-day auto-purge.
fn remove_if_exists(path: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("deleting {}: {e}", path.display())),
    }
}

pub(crate) fn safe_archive_path(
    root: &std::path::Path,
    relative: &str,
) -> Result<std::path::PathBuf, String> {
    use std::path::{Component, Path};

    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("meeting has an invalid archived-audio path".into());
    }
    Ok(root.join(relative))
}

pub fn purge_meeting_data(state: &AppState, id: i64) -> Result<(), String> {
    let is_binned = state
        .db
        .list_deleted_meetings()
        .map_err(err)?
        .iter()
        .any(|(meeting, _)| meeting.id == id);
    if !is_binned {
        return Err("Only meetings in the recycle bin can be permanently deleted".into());
    }
    state
        .pipeline
        .cancel(id)
        .map_err(|error| format!("Cannot purge this meeting yet: {error:#}"))?;
    let meeting = state
        .db
        .get_meeting(id)
        .map_err(err)?
        .ok_or_else(|| format!("meeting {id} not found"))?;
    let s = state.settings.lock().unwrap();
    if let Some(rel) = meeting.audio_path {
        remove_if_exists(&safe_archive_path(&s.audio_dir(), &rel)?)?;
    }
    let (mic, lop, meta) = recorder::wav_paths(&s.rec_tmp_dir(), id);
    drop(s);
    for p in [mic, lop, meta] {
        remove_if_exists(&p)?;
    }
    // Only forget the row after every known copy is gone.  If Windows has a
    // file locked, the binned row remains available for a later retry rather
    // than falsely reporting a permanent deletion.
    state.db.delete_meeting(id).map_err(err)
}

#[tauri::command]
pub fn purge_meeting(app: AppHandle, state: State<'_, AppState>, id: i64) -> Result<(), String> {
    purge_meeting_data(&state, id)?;
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    Ok(())
}

#[tauri::command]
pub fn empty_recycle_bin(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    for (meeting, _) in state.db.list_deleted_meetings().map_err(err)? {
        purge_meeting_data(&state, meeting.id)?;
    }
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    Ok(())
}

#[tauri::command]
pub fn rename_meeting(state: State<'_, AppState>, id: i64, title: String) -> Result<(), String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("Title cannot be empty".into());
    }
    state.db.rename_meeting(id, title).map_err(err)
}

/// Names that shouldn't create voice-print enrollments.
fn is_generic_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "me"
        || lower == "them"
        || lower == "unknown"
        || (lower.starts_with("speaker") && lower[7..].trim().chars().all(|c| c.is_ascii_digit()))
}

fn rematch_speakers(app: AppHandle) {
    std::thread::Builder::new()
        .name("rematch".into())
        .spawn(move || {
            let state = app.state::<AppState>();
            let threshold = state.settings.lock().unwrap().speaker_match_threshold;
            match crate::speaker_id::rematch_all(&state.db, threshold) {
                Ok(0) => {}
                Ok(n) => {
                    log::info!("retroactive voice re-match updated {n} speaker label(s)");
                    let _ = app.emit(events::MEETINGS_CHANGED, ());
                }
                Err(e) => log::warn!("retroactive re-match failed: {e:#}"),
            }
        })
        .ok();
}

/// Renaming a remote speaker is also the enrollment gesture. Its person's
/// print is rebuilt from every explicit link in the same transaction, so
/// retries and reassignment cannot count the same speaker twice.
#[tauri::command]
pub fn rename_speaker(
    app: AppHandle,
    state: State<'_, AppState>,
    speaker_id: i64,
    name: String,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Name cannot be empty".into());
    }
    state
        .db
        .rename_speaker(speaker_id, name, !is_generic_name(name))
        .map_err(err)?;
    rematch_speakers(app);
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct PersonInfo {
    pub id: i64,
    pub name: String,
    pub sample_seconds: f64,
    pub created_at: String,
}

#[tauri::command]
pub fn list_people(state: State<'_, AppState>) -> Result<Vec<PersonInfo>, String> {
    Ok(state
        .db
        .list_people()
        .map_err(err)?
        .into_iter()
        .map(|p| PersonInfo {
            id: p.id,
            name: p.name,
            sample_seconds: p.sample_seconds,
            created_at: p.created_at,
        })
        .collect())
}

#[tauri::command]
pub fn delete_person(
    app: AppHandle,
    state: State<'_, AppState>,
    person_id: i64,
) -> Result<(), String> {
    state.db.delete_person(person_id).map_err(err)?;
    // Revert or reassign automatic labels that pointed at the deleted person.
    rematch_speakers(app);
    Ok(())
}

#[tauri::command]
pub fn get_people_stats(
    state: State<'_, AppState>,
) -> Result<Vec<crate::db::PersonMeetingStat>, String> {
    state.db.people_meeting_stats().map_err(err)
}

// ---------- search ----------

#[tauri::command]
pub fn search(
    state: State<'_, AppState>,
    query: String,
    offset: i64,
    person_id: Option<i64>,
    track: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
) -> Result<Vec<SearchHit>, String> {
    state
        .db
        .search(
            &query,
            offset,
            40,
            person_id,
            track.as_deref().filter(|t| *t == "mic" || *t == "loopback"),
            date_from.as_deref().filter(|s| !s.is_empty()),
            date_to.as_deref().filter(|s| !s.is_empty()),
        )
        .map_err(err)
}

// ---------- transcript export ----------

/// Rendered transcript text (frontend copies it to the clipboard).
#[tauri::command]
pub fn get_transcript_text(
    state: State<'_, AppState>,
    meeting_id: i64,
    format: String,
) -> Result<String, String> {
    let format = crate::transcript_export::ExportFormat::parse(&format).map_err(err)?;
    crate::transcript_export::render(&state.db, meeting_id, format).map_err(err)
}

/// Save dialog + write of the rendered transcript. False = cancelled.
#[tauri::command]
pub async fn export_transcript(
    app: AppHandle,
    meeting_id: i64,
    format: String,
) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;
    let format = crate::transcript_export::ExportFormat::parse(&format).map_err(err)?;
    let (text, title) = {
        let state = app.state::<AppState>();
        let meeting = state
            .db
            .get_meeting(meeting_id)
            .map_err(err)?
            .ok_or_else(|| format!("meeting {meeting_id} not found"))?;
        (
            crate::transcript_export::render(&state.db, meeting_id, format).map_err(err)?,
            meeting.title,
        )
    };
    let safe_title: String = title
        .chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let ext = format.extension();
    let picked = app
        .dialog()
        .file()
        .set_file_name(format!("{safe_title}.{ext}"))
        .add_filter(format!("{} transcript", ext.to_uppercase()), &[ext])
        .blocking_save_file();
    let Some(dest) = picked.and_then(|f| f.into_path().ok()) else {
        return Ok(false);
    };
    std::fs::write(&dest, text).map_err(err)?;
    Ok(true)
}

// ---------- app management ----------

/// Native yes/no confirmation (WebView2 suppresses JS confirm()).
#[tauri::command]
pub async fn confirm_dialog(app: AppHandle, message: String) -> Result<bool, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    Ok(app
        .dialog()
        .message(message)
        .title("Witness")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::YesNo)
        .blocking_show())
}

/// Which global record hotkey bound at startup (None = all were taken).
#[tauri::command]
pub fn get_hotkey(state: State<'_, AppState>) -> Option<String> {
    state.hotkey.lock().unwrap().clone()
}

/// Which bookmark-this-moment hotkey bound at startup (None = all taken).
#[tauri::command]
pub fn get_bookmark_hotkey(state: State<'_, AppState>) -> Option<String> {
    state.bookmark_hotkey.lock().unwrap().clone()
}

#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(err)
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    if enabled {
        app.autolaunch().enable().map_err(err)
    } else {
        app.autolaunch().disable().map_err(err)
    }
}

/// Applies a changed data_dir immediately (db/log paths bind at startup).
#[tauri::command]
pub fn restart_app(app: AppHandle) -> Result<(), String> {
    let recording = {
        let state = app.state::<AppState>();
        let recording = state.recorder.lock().unwrap().is_recording();
        recording
    };
    // Finalize any active recording first and preserve an actionable error
    // instead of restarting over failed recorder finalization.
    if recording {
        do_stop_recording(&app, true)?;
    }

    // Synchronize the last check with both recording start and backup start,
    // then hold both guards across the diverging restart call. This prevents
    // either operation from winning a check-to-restart race.
    let state = app.state::<AppState>();
    let recorder = state.recorder.lock().unwrap();
    let backup_in_progress = state.backup_in_progress.lock().unwrap();
    if recorder.is_busy() {
        return Err("Wait for recording to stop before restarting Witness".into());
    }
    if *backup_in_progress {
        return Err("Wait for the backup to finish before restarting Witness".into());
    }
    app.restart()
}

// ---------- transcription ----------

#[tauri::command]
pub fn retranscribe(
    state: State<'_, AppState>,
    meeting_id: i64,
    engine: Option<Engine>,
) -> Result<(), String> {
    let backup_in_progress = state.backup_in_progress.lock().unwrap();
    if *backup_in_progress {
        return Err("A backup is in progress".into());
    }
    let meeting = state
        .db
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or_else(|| format!("meeting {meeting_id} not found"))?;
    if meeting.status == "recording" {
        return Err("Meeting is still recording".into());
    }
    state
        .pipeline
        .enqueue(Job {
            meeting_id,
            transcribe: true,
            engine,
        })
        .map_err(err)?;
    drop(backup_in_progress);
    Ok(())
}

// ---------- settings ----------

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Settings {
    let mut settings = state.settings.lock().unwrap().clone();
    settings.data_dir = state.configured_data_dir.lock().unwrap().clone();
    settings
}

#[tauri::command]
pub fn update_settings(
    _app: AppHandle,
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    let settings = settings.normalized()?;
    settings.save()?;
    state
        .watcher
        .enabled
        .store(settings.auto_record, Ordering::Relaxed);
    state.watcher.set_patterns(&settings.watch_patterns);

    // A data-directory switch must be all-or-nothing.  Persist the requested
    // value for the next launch, but keep every live path (DB, recordings,
    // models, log, and asset scope) rooted at the directory opened at startup.
    // Applying only some of them immediately can strand a new recording in a
    // directory whose DB row still lives in the old database.
    *state.configured_data_dir.lock().unwrap() = settings.data_dir.clone();
    let active_data_dir = state.settings.lock().unwrap().data_dir.clone();
    let mut active_settings = settings;
    active_settings.data_dir = active_data_dir;
    *state.settings.lock().unwrap() = active_settings;
    Ok(())
}

#[tauri::command]
pub async fn pick_data_dir(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let folder = app.dialog().file().blocking_pick_folder();
    Ok(folder
        .and_then(|f| f.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

// ---------- models ----------

#[tauri::command]
pub fn get_model_status(state: State<'_, AppState>) -> Vec<ModelInfo> {
    models::status(&state.settings.lock().unwrap().models_dir())
}

#[tauri::command]
pub fn download_models(app: AppHandle, engine: Engine) -> Result<(), String> {
    let state = app.state::<AppState>();
    if state.downloading.swap(true, Ordering::SeqCst) {
        return Err("A download is already in progress".into());
    }
    let models_dir = state.settings.lock().unwrap().models_dir();
    let thread_app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("model-download".into())
        .spawn(move || {
            let emit = |model_id: &str,
                        file: &str,
                        downloaded: u64,
                        total: Option<u64>,
                        file_index: u32,
                        file_count: u32| {
                let _ = thread_app.emit(
                    events::MODEL_DOWNLOAD_PROGRESS,
                    events::ModelDownloadProgress {
                        model_id: model_id.to_string(),
                        file: file.to_string(),
                        downloaded_bytes: downloaded,
                        total_bytes: total,
                        file_index,
                        file_count,
                        done: false,
                        error: None,
                    },
                );
            };
            let result = models::download(engine, &models_dir, emit);
            let state = thread_app.state::<AppState>();
            state.downloading.store(false, Ordering::SeqCst);
            let _ = thread_app.emit(
                events::MODEL_DOWNLOAD_PROGRESS,
                events::ModelDownloadProgress {
                    model_id: engine.to_string(),
                    file: String::new(),
                    downloaded_bytes: 0,
                    total_bytes: None,
                    file_index: 0,
                    file_count: 0,
                    done: true,
                    error: result.as_ref().err().map(|e| format!("{e:#}")),
                },
            );
            match result {
                Ok(()) => toast(&thread_app, "Model download complete."),
                Err(e) => toast(&thread_app, &format!("Model download failed: {e:#}")),
            }
        });
    if let Err(error) = spawned {
        state.downloading.store(false, Ordering::SeqCst);
        return Err(err(error));
    }
    Ok(())
}

// ---------- GPU libraries ----------

#[derive(Debug, Serialize)]
pub struct GpuLibsInfo {
    /// Verified app-managed install in `data_dir/cuda`.
    pub present: bool,
    pub size_mb: Option<f64>,
    pub integrity_error: Option<String>,
    /// NVIDIA driver present — without it the download is pointless.
    pub driver_present: bool,
    /// The CUDA EP already resolves everything (system install or managed).
    pub cuda_ready: bool,
    /// Total download size of the pinned archives, for the button label.
    pub download_mb: u64,
}

#[tauri::command]
pub fn get_gpu_libs_status(state: State<'_, AppState>) -> GpuLibsInfo {
    let cuda_dir = state.settings.lock().unwrap().cuda_dir();
    let libs = crate::gpu_libs::status(&cuda_dir);
    let preflight = crate::gpu::preflight();
    GpuLibsInfo {
        present: libs.present,
        size_mb: libs.size_mb,
        integrity_error: libs.integrity_error,
        driver_present: preflight.driver_present,
        cuda_ready: preflight.ready(),
        download_mb: crate::gpu_libs::download_size_bytes() / 1_000_000,
    }
}

/// Download the pinned CUDA runtime + cuDNN DLLs into `data_dir/cuda`.
/// Progress reuses the model-download event stream with id "gpu-libs".
#[tauri::command]
pub fn download_gpu_libs(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    if state.downloading.swap(true, Ordering::SeqCst) {
        return Err("A download is already in progress".into());
    }
    let cuda_dir = state.settings.lock().unwrap().cuda_dir();
    let thread_app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("gpu-libs-download".into())
        .spawn(move || {
            let emit = |_: &str,
                        file: &str,
                        downloaded: u64,
                        total: Option<u64>,
                        file_index: u32,
                        file_count: u32| {
                let _ = thread_app.emit(
                    events::MODEL_DOWNLOAD_PROGRESS,
                    events::ModelDownloadProgress {
                        model_id: "gpu-libs".into(),
                        file: file.to_string(),
                        downloaded_bytes: downloaded,
                        total_bytes: total,
                        file_index,
                        file_count,
                        done: false,
                        error: None,
                    },
                );
            };
            let result = crate::gpu_libs::download(&cuda_dir, emit);
            let state = thread_app.state::<AppState>();
            state.downloading.store(false, Ordering::SeqCst);
            if result.is_ok() {
                // Put the fresh install on the loader path right away — the
                // next transcription uses the GPU without a restart.
                crate::gpu::preflight();
            }
            let _ = thread_app.emit(
                events::MODEL_DOWNLOAD_PROGRESS,
                events::ModelDownloadProgress {
                    model_id: "gpu-libs".into(),
                    file: String::new(),
                    downloaded_bytes: 0,
                    total_bytes: None,
                    file_index: 0,
                    file_count: 0,
                    done: true,
                    error: result.as_ref().err().map(|e| format!("{e:#}")),
                },
            );
            match result {
                Ok(()) => toast(&thread_app, "GPU libraries installed."),
                Err(e) => toast(&thread_app, &format!("GPU library download failed: {e:#}")),
            }
        });
    if let Err(error) = spawned {
        state.downloading.store(false, Ordering::SeqCst);
        return Err(err(error));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct AudioDevices {
    pub render: Vec<crate::audio_capture::AudioDevice>,
    pub capture: Vec<crate::audio_capture::AudioDevice>,
}

/// Active endpoints for the Settings device pickers. Future selections store
/// the stable endpoint ID; capture still accepts legacy friendly-name values.
#[tauri::command]
pub async fn list_audio_devices() -> Result<AudioDevices, String> {
    let (render, capture) = crate::audio_capture::list_devices()?;
    Ok(AudioDevices { render, capture })
}

#[derive(Debug, Serialize)]
pub struct GpuStatus {
    pub cuda_available: bool,
    pub detail: String,
}

#[tauri::command]
pub fn get_gpu_status() -> GpuStatus {
    // Full preflight, not just the driver: an unresolvable cuDNN/CUDA runtime
    // DLL makes ort fall back to CPU even with a working driver, and that is
    // by far the most common way GPU transcription silently degrades.
    let status = crate::gpu::preflight();
    let detail = if !status.driver_present {
        "No NVIDIA driver found — transcription will run on CPU".into()
    } else if status.missing_dlls.is_empty() {
        "NVIDIA driver and CUDA/cuDNN runtime found — Parakeet will attempt GPU acceleration".into()
    } else {
        format!(
            "NVIDIA driver found, but {} missing — Parakeet will run on CPU. \
             Use Download GPU libraries below, or install cuDNN 9 for CUDA 13 and set CUDNN_PATH; only system-level changes may require a restart.",
            status.missing_dlls.join(", ")
        )
    };
    GpuStatus {
        cuda_available: status.ready(),
        detail,
    }
}

// ---------- playback ----------

/// "Download": native save dialog + copy of the archived audio.
/// Returns false if the user cancelled.
#[tauri::command]
pub async fn export_audio(app: AppHandle, meeting_id: i64) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;
    let (src, title) = {
        let state = app.state::<AppState>();
        let meeting = state
            .db
            .get_meeting(meeting_id)
            .map_err(err)?
            .ok_or_else(|| format!("meeting {meeting_id} not found"))?;
        let rel = meeting.audio_path.ok_or("No audio for this meeting yet")?;
        let audio_dir = state.settings.lock().unwrap().audio_dir();
        (safe_archive_path(&audio_dir, &rel)?, meeting.title)
    };
    if !src.exists() {
        return Err("Audio file is missing".into());
    }
    let safe_title: String = title
        .chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let picked = app
        .dialog()
        .file()
        .set_file_name(format!("{safe_title}.opus"))
        .add_filter("Ogg Opus audio", &["opus"])
        .blocking_save_file();
    let Some(dest) = picked.and_then(|f| f.into_path().ok()) else {
        return Ok(false);
    };
    std::fs::copy(&src, &dest).map_err(err)?;
    log::info!("exported meeting {meeting_id} audio to {}", dest.display());
    Ok(true)
}

#[tauri::command]
pub fn get_audio_url(state: State<'_, AppState>, meeting_id: i64) -> Result<String, String> {
    let meeting = state
        .db
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or_else(|| format!("meeting {meeting_id} not found"))?;
    let rel = meeting.audio_path.ok_or("No audio for this meeting yet")?;
    let path = safe_archive_path(&state.settings.lock().unwrap().audio_dir(), &rel)?;
    if !path.exists() {
        return Err("Audio file is missing".into());
    }
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::safe_archive_path;
    use std::path::Path;

    #[test]
    fn archived_audio_paths_cannot_escape_the_audio_directory() {
        let root = Path::new("audio-root");
        assert_eq!(
            safe_archive_path(root, "2026/meeting.opus").unwrap(),
            root.join("2026/meeting.opus")
        );
        for invalid in [
            "",
            ".",
            "../private.txt",
            "folder/../private.txt",
            "/absolute.opus",
            r"C:\\absolute.opus",
        ] {
            assert!(
                safe_archive_path(root, invalid).is_err(),
                "accepted invalid archive path {invalid:?}"
            );
        }
    }
}
