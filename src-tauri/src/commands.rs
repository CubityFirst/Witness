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
use serde::Serialize;
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

pub fn do_start_recording(app: &AppHandle, trigger: &'static str) -> Result<i64, String> {
    let state = app.state::<AppState>();
    let mut rec = state.recorder.lock().unwrap();
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
    let (rec_dir, mic_device, loopback_device, live_enabled, models_dir, overlay) = {
        let s = state.settings.lock().unwrap();
        (
            s.rec_tmp_dir(),
            s.mic_device.clone(),
            s.loopback_device.clone(),
            s.live_transcribe,
            s.models_dir(),
            s.caption_overlay,
        )
    };

    // Live captions: best-effort — needs the Parakeet model on disk.
    let live_tx = if live_enabled {
        let live_app = app.clone();
        let tx = crate::live_transcribe::spawn(models_dir, move |track, start_ms, end_ms, text| {
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
        });
        if tx.is_none() {
            log::info!("live captions off: Parakeet model not downloaded");
        }
        tx
    } else {
        None
    };
    let live_started = live_tx.is_some();

    let level_app = app.clone();
    let handle = recorder::start(
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
            *state.recorder.lock().unwrap() = RecorderState::Idle;
            return Err(err(e));
        }
    };
    *state.recording_trigger.lock().unwrap() = trigger;
    *state.recorder.lock().unwrap() = RecorderState::Recording(handle);

    let _ = app.emit(
        events::RECORDING_STARTED,
        events::RecordingStarted {
            meeting_id: id,
            trigger,
            started_at: now.to_rfc3339(),
        },
    );
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    tray::update(app, true);

    if overlay && live_started {
        open_captions_overlay(app);
    }

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

fn open_captions_overlay(app: &AppHandle) {
    if app.get_webview_window(CAPTIONS_WINDOW).is_some() {
        return;
    }
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
    .background_color(tauri::webview::Color(12, 13, 16, 255))
    .build();
    if let Err(e) = result {
        log::warn!("captions overlay failed to open: {e}");
    }
}

fn close_captions_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(CAPTIONS_WINDOW) {
        let _ = w.close();
    }
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
    *state.recorder.lock().unwrap() = RecorderState::Idle;
    let _ = app.emit(
        events::RECORDING_STOPPED,
        events::RecordingStopped { meeting_id: id },
    );
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    tray::update(app, false);
    close_captions_overlay(app);

    let transcribe = state.settings.lock().unwrap().auto_transcribe;
    state.pipeline.enqueue(Job {
        meeting_id: id,
        transcribe,
        engine: None,
    });
    log::info!("recording stopped: meeting {id} ({} ms)", stats.duration_ms);
    Ok(())
}

// ---------- status ----------

#[derive(Debug, Serialize)]
pub struct AppStatus {
    pub recording: bool,
    pub meeting_id: Option<i64>,
    pub recording_since: Option<String>,
    pub transcribing_meeting_id: Option<i64>,
    pub queue_len: usize,
    pub watcher: events::WatcherStatus,
}

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> AppStatus {
    let rec = state.recorder.lock().unwrap();
    AppStatus {
        recording: rec.is_recording(),
        meeting_id: rec.meeting_id(),
        recording_since: rec.started_at().map(|t| t.to_rfc3339()),
        transcribing_meeting_id: state.pipeline.current_meeting(),
        queue_len: state.pipeline.queue_len(),
        watcher: state.last_watcher_status.lock().unwrap().clone(),
    }
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
    let _ = app.emit(events::MEETINGS_CHANGED, ());
    Ok(())
}

#[tauri::command]
pub fn restore_meeting(app: AppHandle, state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.db.restore_meeting(id).map_err(err)?;
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

fn safe_archive_path(root: &std::path::Path, relative: &str) -> Result<std::path::PathBuf, String> {
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

/// Renaming a remote speaker is also the enrollment gesture: the speaker's
/// stored voice print is folded into (or creates) the named person, so
/// future meetings auto-label them. Auto-matches never touch prints —
/// only these explicit renames do.
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
    let mut person_id = None;
    if !is_generic_name(name) {
        if let Some((label, embedding, secs)) =
            state.db.get_speaker_embedding(speaker_id).map_err(err)?
        {
            if label != "me" {
                // Always link a person (so stats unify across meetings);
                // fold in the voice print only when this speaker has one.
                let existing = state.db.get_person_by_name(name).map_err(err)?;
                person_id = Some(match (existing, embedding) {
                    (Some(person), Some(embedding)) => {
                        let (merged, total) = if person.embedding.is_empty() {
                            (embedding, secs)
                        } else {
                            (
                                crate::speaker_id::merge_embeddings(
                                    &person.embedding,
                                    person.sample_seconds,
                                    &embedding,
                                    secs,
                                ),
                                person.sample_seconds + secs,
                            )
                        };
                        state
                            .db
                            .update_person_embedding(person.id, &merged, total)
                            .map_err(err)?;
                        log::info!("folded {secs:.0}s of speech into voice print for {name}");
                        person.id
                    }
                    (Some(person), None) => person.id,
                    (None, Some(embedding)) => {
                        log::info!("enrolled new person {name} ({secs:.0}s of speech)");
                        state
                            .db
                            .create_person(name, &embedding, secs)
                            .map_err(err)?
                    }
                    (None, None) => state.db.create_person(name, &[], 0.0).map_err(err)?,
                });
            }
        }
    }
    state
        .db
        .rename_speaker(speaker_id, name, person_id)
        .map_err(err)?;

    // Enrollment changed the people set: retroactively re-match all stored
    // voice prints in the background (cheap — pure cosine over the DB).
    if person_id.is_some() {
        let rematch_app = app.clone();
        std::thread::Builder::new()
            .name("rematch".into())
            .spawn(move || {
                let state = rematch_app.state::<AppState>();
                let threshold = state.settings.lock().unwrap().speaker_match_threshold;
                match crate::speaker_id::rematch_all(&state.db, threshold) {
                    Ok(0) => {}
                    Ok(n) => {
                        log::info!("retroactive voice re-match updated {n} speaker label(s)");
                        let _ = rematch_app.emit(events::MEETINGS_CHANGED, ());
                    }
                    Err(e) => log::warn!("retroactive re-match failed: {e:#}"),
                }
            })
            .ok();
    }
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
    // Revert any auto labels that pointed at the deleted person.
    let threshold = state.settings.lock().unwrap().speaker_match_threshold;
    std::thread::Builder::new()
        .name("rematch".into())
        .spawn(move || {
            let state = app.state::<AppState>();
            if let Ok(n) = crate::speaker_id::rematch_all(&state.db, threshold) {
                if n > 0 {
                    let _ = app.emit(events::MEETINGS_CHANGED, ());
                }
            }
        })
        .ok();
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
pub fn restart_app(app: AppHandle) {
    // Finalize any active recording first.
    if app
        .state::<AppState>()
        .recorder
        .lock()
        .unwrap()
        .is_recording()
    {
        let _ = do_stop_recording(&app, true);
    }
    app.restart();
}

// ---------- transcription ----------

#[tauri::command]
pub fn retranscribe(
    state: State<'_, AppState>,
    meeting_id: i64,
    engine: Option<Engine>,
) -> Result<(), String> {
    let meeting = state
        .db
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or_else(|| format!("meeting {meeting_id} not found"))?;
    if meeting.status == "recording" {
        return Err("Meeting is still recording".into());
    }
    state.pipeline.enqueue(Job {
        meeting_id,
        transcribe: true,
        engine,
    });
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
    std::thread::Builder::new()
        .name("model-download".into())
        .spawn(move || {
            let emit = |model_id: &str, file: &str, downloaded: u64, total: Option<u64>| {
                let _ = thread_app.emit(
                    events::MODEL_DOWNLOAD_PROGRESS,
                    events::ModelDownloadProgress {
                        model_id: model_id.to_string(),
                        file: file.to_string(),
                        downloaded_bytes: downloaded,
                        total_bytes: total,
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
                    done: true,
                    error: result.as_ref().err().map(|e| format!("{e:#}")),
                },
            );
            match result {
                Ok(()) => toast(&thread_app, "Model download complete."),
                Err(e) => toast(&thread_app, &format!("Model download failed: {e:#}")),
            }
        })
        .map_err(err)?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct AudioDevices {
    pub render: Vec<String>,
    pub capture: Vec<String>,
}

/// Active endpoint names for the Settings device pickers.
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
    // Cheap proxy: the NVIDIA CUDA driver DLL. The real test happens when a
    // session is created; ort falls back to CPU (with a log warning) if the
    // CUDA EP can't initialize.
    let cuda = std::path::Path::new(r"C:\Windows\System32\nvcuda.dll").exists();
    GpuStatus {
        cuda_available: cuda,
        detail: if cuda {
            "NVIDIA driver found — CUDA is attempted for Parakeet (CPU fallback with a logged warning)".into()
        } else {
            "No NVIDIA driver found — transcription will run on CPU".into()
        },
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
        (audio_dir.join(rel), meeting.title)
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
    let path = state.settings.lock().unwrap().audio_dir().join(rel);
    if !path.exists() {
        return Err("Audio file is missing".into());
    }
    Ok(path.to_string_lossy().into_owned())
}
