//! Witness — local meeting recorder & transcriber.
//! Single visible-tray process, no detached daemon (AV heuristics — vigil's
//! lesson). Closing the window minimizes to the tray; Quit finalizes any
//! active recording and exits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod asr;
mod asr_parakeet;
mod asr_whisper;
mod audio_capture;
mod commands;
mod db;
mod diarize;
mod encoder;
mod events;
mod live_transcribe;
mod meeting_title;
mod meeting_watcher;
mod ml_scheduler;
mod models;
mod pipeline;
mod recorder;
mod resample;
mod settings;
mod speaker_id;
mod state;
mod transcript_export;
mod tray;
mod vad;

use crate::db::Db;
use crate::meeting_watcher::{WatcherCommand, WatcherControl};
use crate::pipeline::{Job, PipelineEvent};
use crate::settings::Settings;
use crate::state::{AppState, RecorderState};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

fn init_logging(data_dir: &std::path::Path) {
    use simplelog::{
        ColorChoice, CombinedLogger, Config, LevelFilter, SharedLogger, TermLogger, TerminalMode,
        WriteLogger,
    };
    let log_path = data_dir.join("witness.log");
    // Simple size-capped rotation: keep one previous generation.
    if std::fs::metadata(&log_path)
        .map(|m| m.len() > LOG_ROTATE_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&log_path, data_dir.join("witness.log.old"));
    }
    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![TermLogger::new(
        LevelFilter::Info,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )];
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        loggers.push(WriteLogger::new(LevelFilter::Info, Config::default(), file));
    }
    let _ = CombinedLogger::init(loggers);
}

/// A crash can leave a WAV whose header undercounts what's on disk (the
/// header is only refreshed on the periodic flush). Patch the RIFF/data
/// sizes from the real file size so nothing recorded is lost.
fn repair_wav_header(path: &std::path::Path) {
    use std::io::{Seek, SeekFrom, Write};
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return;
    };
    let Ok(meta) = file.metadata() else { return };
    let size = meta.len();
    // hound writes the canonical 44-byte PCM header.
    if size <= 44 {
        return;
    }
    let riff = ((size - 8) as u32).to_le_bytes();
    let data = ((size - 44) as u32).to_le_bytes();
    let _ = file
        .seek(SeekFrom::Start(4))
        .and_then(|_| file.write_all(&riff));
    let _ = file
        .seek(SeekFrom::Start(40))
        .and_then(|_| file.write_all(&data));
}

/// Re-enqueue work interrupted by a crash or quit:
/// - meetings stuck in 'recording' with leftover rec-tmp WAVs → close them
///   out (duration from the WAV) and queue processing;
/// - meetings stuck in 'processing' → queue transcription again;
/// - 'recorded' meetings never encoded → queue (encode-only unless
///   auto-transcribe is on).
fn recover_orphans(db: &Db, settings: &Settings, pipeline: &pipeline::Pipeline) {
    let rec_dir = settings.rec_tmp_dir();
    if let Ok(entries) = std::fs::read_dir(&rec_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(meta) = toml::from_str::<recorder::RecMeta>(&text) else {
                continue;
            };
            let Ok(Some(meeting)) = db.get_meeting(meta.meeting_id) else {
                // Meeting row is gone; clean up strays.
                let (mic, lop, meta_path) = recorder::wav_paths(&rec_dir, meta.meeting_id);
                for p in [mic, lop, meta_path] {
                    let _ = std::fs::remove_file(p);
                }
                continue;
            };
            if meeting.status == "recording" {
                let (mic, lop, _) = recorder::wav_paths(&rec_dir, meta.meeting_id);
                repair_wav_header(&mic);
                repair_wav_header(&lop);
                let duration_ms = hound::WavReader::open(&mic)
                    .map(|r| r.duration() as i64 * 1000 / r.spec().sample_rate as i64)
                    .unwrap_or(0);
                let ended = chrono::Local::now().to_rfc3339();
                let _ = db.finish_recording(meta.meeting_id, &ended, duration_ms);
                log::info!(
                    "recovered interrupted recording: meeting {} ({duration_ms} ms)",
                    meta.meeting_id
                );
                if let Err(error) = pipeline.enqueue(Job {
                    meeting_id: meta.meeting_id,
                    transcribe: settings.auto_transcribe,
                    engine: None,
                }) {
                    log::error!(
                        "could not queue recovered meeting {}: {error:#}",
                        meta.meeting_id
                    );
                }
            }
        }
    }
    if let Ok(processing) = db.meetings_with_status(&["processing"]) {
        for m in processing {
            log::info!("re-queueing interrupted transcription: meeting {}", m.id);
            if let Err(error) = pipeline.enqueue(Job {
                meeting_id: m.id,
                transcribe: true,
                engine: None,
            }) {
                log::error!("could not re-queue meeting {}: {error:#}", m.id);
            }
        }
    }
    if let Ok(recorded) = db.meetings_with_status(&["recorded"]) {
        for m in recorded.into_iter().filter(|m| m.audio_path.is_none()) {
            // WAVs must still exist for these to be processable.
            let (mic, lop, _) = recorder::wav_paths(&rec_dir, m.id);
            if mic.exists() && lop.exists() {
                log::info!("re-queueing unencoded recording: meeting {}", m.id);
                if let Err(error) = pipeline.enqueue(Job {
                    meeting_id: m.id,
                    transcribe: settings.auto_transcribe,
                    engine: None,
                }) {
                    log::error!("could not re-queue meeting {}: {error:#}", m.id);
                }
            }
        }
    }
}

fn setup(app: &tauri::App) -> anyhow::Result<()> {
    let settings = Settings::load_result().map_err(anyhow::Error::msg)?;
    let data_dir = settings.data_dir();
    for dir in [
        data_dir.clone(),
        settings.rec_tmp_dir(),
        settings.audio_dir(),
        settings.models_dir(),
    ] {
        std::fs::create_dir_all(&dir)?;
    }
    init_logging(&data_dir);
    log::info!("Witness starting; data dir: {}", data_dir.display());

    let db = Arc::new(Db::open(&settings.db_path())?);

    // Make archived audio playable through the asset protocol.
    app.asset_protocol_scope()
        .allow_directory(settings.audio_dir(), true)?;

    // Pipeline worker: forwards its events to the frontend + toasts.
    let settings = Arc::new(Mutex::new(settings));
    let pipe_app = app.handle().clone();
    let pipe_db = db.clone();
    let pipeline = pipeline::spawn(db.clone(), settings.clone(), move |event| {
        match event {
            PipelineEvent::Progress {
                meeting_id,
                stage,
                pct,
            } => {
                let _ = pipe_app.emit(
                    events::TRANSCRIPTION_PROGRESS,
                    events::TranscriptionProgress {
                        meeting_id,
                        stage,
                        pct,
                    },
                );
            }
            PipelineEvent::Complete { meeting_id, rtf } => {
                let _ = pipe_app.emit(
                    events::TRANSCRIPTION_COMPLETE,
                    events::TranscriptionDone {
                        meeting_id,
                        error: None,
                    },
                );
                let _ = pipe_app.emit(events::MEETINGS_CHANGED, ());
                // Only toast when a transcript was actually produced. The
                // realtime factor doubles as a GPU sanity check: single-digit
                // numbers on this box mean ort fell back to CPU.
                if let Ok(Some(m)) = pipe_db.get_meeting(meeting_id) {
                    if m.status == "transcribed" {
                        let speed = rtf
                            .map(|r| format!(" ({r:.0}× realtime)"))
                            .unwrap_or_default();
                        commands::toast(
                            &pipe_app,
                            &format!("Transcript ready: {}{speed}", m.title),
                        );
                    }
                }
            }
            PipelineEvent::Failed { meeting_id, error } => {
                let _ = pipe_app.emit(
                    events::TRANSCRIPTION_FAILED,
                    events::TranscriptionDone {
                        meeting_id,
                        error: Some(error.clone()),
                    },
                );
                let _ = pipe_app.emit(events::MEETINGS_CHANGED, ());
                commands::toast(
                    &pipe_app,
                    "Transcription failed — you can retry from the meeting page.",
                );
            }
        }
    });

    let (auto_record, patterns, configured_data_dir) = {
        let s = settings.lock().unwrap();
        (s.auto_record, s.watch_patterns.clone(), s.data_dir.clone())
    };
    let watcher = WatcherControl::new(auto_record, &patterns);

    app.manage(AppState {
        db: db.clone(),
        settings: settings.clone(),
        configured_data_dir: Mutex::new(configured_data_dir),
        recorder: Mutex::new(RecorderState::Idle),
        recording_health: Mutex::new(None),
        recording_trigger: Mutex::new("manual"),
        pipeline,
        watcher: watcher.clone(),
        last_watcher_status: Mutex::new(events::WatcherStatus {
            enabled: auto_record,
            teams_key_found: false,
            mic_in_use: false,
            suppressed: false,
        }),
        downloading: std::sync::atomic::AtomicBool::new(false),
        hotkey: Mutex::new(None),
        bookmark_hotkey: Mutex::new(None),
    });

    tray::build(app)?;

    // Global hotkeys: record toggle + bookmark-this-moment. Other apps may
    // own combos system-wide, so each walks a candidate chain; per-shortcut
    // handlers keep the two actions apart.
    {
        use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
        app.handle()
            .plugin(tauri_plugin_global_shortcut::Builder::new().build())?;

        for candidate in ["ctrl+alt+r", "ctrl+alt+w", "ctrl+alt+shift+r"] {
            let result = app
                .global_shortcut()
                .on_shortcut(candidate, |app, _sc, event| {
                    if event.state() == ShortcutState::Pressed {
                        let recording = app
                            .state::<AppState>()
                            .recorder
                            .lock()
                            .unwrap()
                            .is_recording();
                        let result = if recording {
                            commands::do_stop_recording(app, true)
                        } else {
                            commands::do_start_recording(app, "manual").map(|_| ())
                        };
                        if let Err(e) = result {
                            log::warn!("hotkey record toggle: {e}");
                        }
                    }
                });
            match result {
                Ok(()) => {
                    log::info!("record hotkey bound: {candidate}");
                    *app.state::<AppState>().hotkey.lock().unwrap() = Some(candidate.to_string());
                    break;
                }
                Err(e) => log::warn!("hotkey {candidate} unavailable: {e}"),
            }
        }

        for candidate in ["ctrl+alt+b", "ctrl+alt+m"] {
            let result = app
                .global_shortcut()
                .on_shortcut(candidate, |app, _sc, event| {
                    if event.state() == ShortcutState::Pressed {
                        // "Not recording" is the normal no-op case.
                        if let Err(e) = commands::do_bookmark_now(app) {
                            log::debug!("bookmark hotkey: {e}");
                        }
                    }
                });
            match result {
                Ok(()) => {
                    log::info!("bookmark hotkey bound: {candidate}");
                    *app.state::<AppState>().bookmark_hotkey.lock().unwrap() =
                        Some(candidate.to_string());
                    break;
                }
                Err(e) => log::warn!("hotkey {candidate} unavailable: {e}"),
            }
        }
    }

    // Launched at login (autostart passes --minimized): start in the tray.
    if std::env::args().any(|a| a == "--minimized") {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.hide();
        }
    }

    {
        let state = app.state::<AppState>();
        let s = settings.lock().unwrap();
        recover_orphans(&db, &s, &state.pipeline);
        drop(s);

        // Recycle bin: purge anything deleted more than 30 days ago.
        let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
        if let Ok(deleted) = db.list_deleted_meetings() {
            for (meeting, deleted_at) in deleted {
                let expired = chrono::DateTime::parse_from_rfc3339(&deleted_at)
                    .map(|t| t.with_timezone(&chrono::Utc) < cutoff)
                    .unwrap_or(false);
                if expired {
                    log::info!(
                        "recycle bin: purging meeting {} ({})",
                        meeting.id,
                        meeting.title
                    );
                    if let Err(e) = commands::purge_meeting_data(&state, meeting.id) {
                        log::warn!("purge of meeting {} failed: {e}", meeting.id);
                    }
                }
            }
        }
    }

    // Meeting watcher → auto record.
    let watch_app: AppHandle = app.handle().clone();
    meeting_watcher::spawn(watcher, move |cmd| {
        match cmd {
            WatcherCommand::StartMeeting => {
                match commands::do_start_recording(&watch_app, "auto") {
                    Ok(_) => commands::toast(&watch_app, "Witness is recording this meeting."),
                    Err(e) => log::warn!("auto-record start failed: {e}"),
                }
            }
            WatcherCommand::StopMeeting => {
                // The meeting is over: stop whatever recording is running,
                // auto- or manually started. This only fires after a watched
                // meeting was detected and ended, so a manual recording made
                // outside any call is never touched.
                let state = watch_app.state::<AppState>();
                let trigger = *state.recording_trigger.lock().unwrap();
                let recording = state.recorder.lock().unwrap().is_recording();
                if recording {
                    log::info!("meeting ended: stopping {trigger} recording");
                    if let Err(e) = commands::do_stop_recording(&watch_app, false) {
                        log::warn!("auto-record stop failed: {e}");
                    }
                }
            }
            WatcherCommand::Status(status) => {
                let state = watch_app.state::<AppState>();
                *state.last_watcher_status.lock().unwrap() = status.clone();
                let _ = watch_app.emit(events::WATCHER_STATUS, status);
            }
        }
        true
    });

    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("tray-popup", false);
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .setup(|app| {
            setup(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                // Close button minimizes to tray; recording continues.
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    if window.label() == "main" {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
                // Tray-flyout mode: dismiss on focus loss, like the native
                // volume/Wi-Fi popups — unless the cursor is on the window
                // itself (grabbing a resize border blurs BEFORE any resize
                // event fires); then pin it instead. Normal opens unaffected.
                tauri::WindowEvent::Focused(false) => {
                    if window.label() == "main" && tray::is_popup_mode() {
                        let on_window = (|| -> Option<bool> {
                            let cursor = window.cursor_position().ok()?;
                            let pos = window.outer_position().ok()?;
                            let size = window.outer_size().ok()?;
                            let margin = 16.0;
                            Some(
                                cursor.x >= pos.x as f64 - margin
                                    && cursor.x <= (pos.x + size.width as i32) as f64 + margin
                                    && cursor.y >= pos.y as f64 - margin
                                    && cursor.y <= (pos.y + size.height as i32) as f64 + margin,
                            )
                        })()
                        .unwrap_or(false);
                        if on_window {
                            if let Some(webview) = window.app_handle().get_webview_window("main") {
                                tray::pin_popup(&webview);
                            }
                        } else if tray::take_popup_mode() {
                            let _ = window.hide();
                        }
                    }
                }
                // Resizing/moving the flyout pins it (the resize grab itself
                // blurs the window, which used to dismiss it mid-drag).
                tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Moved(_)
                    if window.label() == "main" =>
                {
                    tray::maybe_pin_popup(window.app_handle());
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_diagnostics,
            commands::export_diagnostics,
            commands::start_recording,
            commands::stop_recording,
            commands::list_meetings,
            commands::get_meeting,
            commands::delete_meeting,
            commands::restore_meeting,
            commands::list_deleted_meetings,
            commands::purge_meeting,
            commands::empty_recycle_bin,
            commands::rename_meeting,
            commands::set_meeting_notes,
            commands::bookmark_now,
            commands::add_bookmark,
            commands::set_bookmark_note,
            commands::delete_bookmark,
            commands::rename_speaker,
            commands::list_people,
            commands::delete_person,
            commands::get_people_stats,
            commands::search,
            commands::retranscribe,
            commands::get_settings,
            commands::update_settings,
            commands::pick_data_dir,
            commands::get_model_status,
            commands::download_models,
            commands::get_gpu_status,
            commands::list_audio_devices,
            commands::get_audio_url,
            commands::export_audio,
            commands::get_transcript_text,
            commands::export_transcript,
            commands::confirm_dialog,
            commands::get_hotkey,
            commands::get_autostart,
            commands::set_autostart,
            commands::restart_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Witness");
}
