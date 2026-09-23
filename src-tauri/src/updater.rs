//! In-app updates for the installed (NSIS) build via tauri-plugin-updater.
//!
//! The feed is `latest.json` on the newest GitHub release; every installer is
//! minisign-signed in CI and verified against the public key in
//! tauri.conf.json before it runs. Checking is automatic (setting
//! `check_for_updates`) but installing always needs a click. On Windows the
//! plugin launches the installer and calls `std::process::exit(0)`, which
//! skips the tray-Quit finalization path — so install refuses while a
//! recording or backup is active. Queued pipeline jobs are persisted in the
//! db and resume on the relaunch.

use crate::commands::toast;
use crate::events;
use crate::state::AppState;
use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Delay before the automatic check, so it never competes with startup
/// recovery, model preflight, or an auto-started recording.
const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(30);

/// The update found by the most recent check, installed by `install_update`.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
}

impl From<&Update> for UpdateInfo {
    fn from(update: &Update) -> Self {
        UpdateInfo {
            current_version: update.current_version.clone(),
            version: update.version.clone(),
            notes: update.body.clone().filter(|notes| !notes.trim().is_empty()),
            date: update.date.map(|date| date.date().to_string()),
        }
    }
}

async fn check(app: &AppHandle) -> anyhow::Result<Option<UpdateInfo>> {
    let update = app.updater()?.check().await?;
    let info = update.as_ref().map(UpdateInfo::from);
    *app.state::<PendingUpdate>().0.lock().unwrap() = update;
    Ok(info)
}

/// Spawned from setup: one quiet check, a toast only when there is news.
pub fn spawn_startup_check(app: AppHandle) {
    let enabled = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .check_for_updates;
    if !enabled {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(STARTUP_CHECK_DELAY);
        match tauri::async_runtime::block_on(check(&app)) {
            Ok(Some(info)) => {
                log::info!(
                    "update available: {} -> {}",
                    info.current_version,
                    info.version
                );
                toast(
                    &app,
                    &format!(
                        "Witness {} is available — install it from Settings → Updates.",
                        info.version
                    ),
                );
            }
            Ok(None) => log::info!("update check: up to date"),
            // Offline, feed not published yet, etc. — never worth a toast.
            Err(e) => log::warn!("update check failed: {e:#}"),
        }
    });
}

#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    check(&app).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = app
        .state::<PendingUpdate>()
        .0
        .lock()
        .unwrap()
        .clone()
        .ok_or("No update is pending — check for updates first")?;

    // Fail fast before a ~100 MB download; re-checked under the locks below.
    if app.state::<AppState>().recorder.lock().unwrap().is_busy() {
        return Err("Stop the recording before installing the update".into());
    }

    let mut downloaded = 0u64;
    let mut last_emit = 0u64;
    let bytes = update
        .download(
            |chunk, total| {
                downloaded += chunk as u64;
                // ~1 MB granularity keeps the event stream light.
                if downloaded - last_emit >= 1 << 20 || Some(downloaded) == total {
                    last_emit = downloaded;
                    let _ = app.emit(
                        events::UPDATE_PROGRESS,
                        events::UpdateProgress { downloaded, total },
                    );
                }
            },
            || {},
        )
        .await
        .map_err(|e| format!("Downloading the update failed: {e:#}"))?;

    // Same guard pattern as restart_app: hold both locks across the
    // diverging install so no recording or backup can start in between.
    let state = app.state::<AppState>();
    let recorder = state.recorder.lock().unwrap();
    let backup_in_progress = state.backup_in_progress.lock().unwrap();
    if recorder.is_busy() {
        return Err("Stop the recording before installing the update".into());
    }
    if *backup_in_progress {
        return Err("Wait for the backup to finish before installing the update".into());
    }
    log::info!(
        "installing update {} -> {}",
        update.current_version,
        update.version
    );
    // On success this never returns: the installer is launched and the
    // process exits; the installer relaunches Witness when it is done.
    update
        .install(bytes)
        .map_err(|e| format!("Installing the update failed: {e:#}"))
}
