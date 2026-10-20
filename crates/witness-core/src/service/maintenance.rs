//! Settings, models, GPU libraries, audio devices, diagnostics, backup and
//! the restart/update exit guard.

use super::{
    assert_off_ui_thread, err, AppCore, AudioDevices, CoreEvent, CoreResult, DiagnosticDatabase,
    DiagnosticGpu, DiagnosticModel, Diagnostics, ExitGuard, ExitPurpose, GpuLibsInfo, GpuStatus,
};
use crate::backup::BackupSummary;
use crate::events;
use crate::models::{self, ModelInfo};
use crate::settings::{Engine, Settings};
use crate::state::{AppState, RecorderState};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

/// Clears `backup_in_progress` however the backup ends.
struct BackupActivityGuard<'a>(&'a Mutex<bool>);

impl Drop for BackupActivityGuard<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = false;
    }
}

fn check_database(path: &Path) -> DiagnosticDatabase {
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
        .map(Path::new)
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

/// Progress event for a download job (`done` is the terminal event).
fn download_progress(
    model_id: &str,
    file: &str,
    downloaded: u64,
    total: Option<u64>,
    file_index: u32,
    file_count: u32,
) -> CoreEvent {
    CoreEvent::ModelDownloadProgress(events::ModelDownloadProgress {
        model_id: model_id.to_string(),
        file: file.to_string(),
        downloaded_bytes: downloaded,
        total_bytes: total,
        file_index,
        file_count,
        done: false,
        error: None,
    })
}

fn download_done(model_id: String, error: Option<String>) -> CoreEvent {
    CoreEvent::ModelDownloadProgress(events::ModelDownloadProgress {
        model_id,
        file: String::new(),
        downloaded_bytes: 0,
        total_bytes: None,
        file_index: 0,
        file_count: 0,
        done: true,
        error,
    })
}

impl AppCore {
    /// The settings as Settings shows them: `data_dir` is the configured
    /// value, which may differ from the runtime one until a restart. [ui-ok]
    pub fn settings(&self) -> Settings {
        let mut settings = self.state.settings.lock().unwrap().clone();
        settings.data_dir = self.state.configured_data_dir.lock().unwrap().clone();
        settings
    }

    /// Normalize, persist and apply. A changed `data_dir` is only recorded
    /// for the next launch (see below).
    pub fn update_settings(&self, settings: Settings) -> CoreResult<()> {
        let state = &self.state;
        let settings = settings.normalized()?;
        settings.save()?;
        state
            .watcher
            .enabled
            .store(settings.auto_record, Ordering::Relaxed);
        state.watcher.set_patterns(&settings.watch_patterns);

        // A data-directory switch must be all-or-nothing.  Persist the requested
        // value for the next launch, but keep every live path (DB, recordings,
        // models and log) rooted at the directory opened at startup.
        // Applying only some of them immediately can strand a new recording in a
        // directory whose DB row still lives in the old database.
        *state.configured_data_dir.lock().unwrap() = settings.data_dir.clone();
        let (active_data_dir, old_ai) = {
            let current = state.settings.lock().unwrap();
            (current.data_dir.clone(), current.ai.clone())
        };
        let new_ai = settings.ai.clone();
        let mut active_settings = settings;
        active_settings.data_dir = active_data_dir;
        *state.settings.lock().unwrap() = active_settings;
        // Switching the AI assistant off (or to another service) stops its
        // running job before its next request.
        self.ai_settings_changed(&old_ai, &new_ai);
        Ok(())
    }

    /// Manifest read + fingerprint I/O. ⏳
    pub fn model_status(&self) -> Vec<ModelInfo> {
        assert_off_ui_thread("model_status");
        let models_dir = self.state.settings.lock().unwrap().models_dir();
        models::status(&models_dir)
    }

    /// Bytes the model cache occupies, including superseded downloads that
    /// no longer show up as a model row (directory walk). ⏳
    pub fn model_cache_size(&self) -> u64 {
        assert_off_ui_thread("model_cache_size");
        let models_dir = self.state.settings.lock().unwrap().models_dir();
        models::cache_size(&models_dir)
    }

    /// Deletes every downloaded model and returns the bytes freed. Refused
    /// while anything may hold a model file open (Windows can't delete open
    /// files): a download, a recording (live captions run Parakeet), or a
    /// pipeline job. ⏳
    pub fn delete_model_cache(&self) -> CoreResult<u64> {
        assert_off_ui_thread("delete_model_cache");
        let state = &self.state;
        if state.recorder.lock().unwrap().is_busy() {
            return Err("Stop the recording before deleting models".into());
        }
        if state.pipeline.current_meeting().is_some() {
            return Err(
                "Wait for the current transcription to finish before deleting models".into(),
            );
        }
        // Claim the download flag so no download can start while files go.
        if state.downloading.swap(true, Ordering::SeqCst) {
            return Err("Wait for the download to finish before deleting models".into());
        }
        let models_dir = state.settings.lock().unwrap().models_dir();
        let result = models::delete_cache(&models_dir).map_err(err);
        state.downloading.store(false, Ordering::SeqCst);
        result
    }

    /// Start downloading `engine`'s models on a "model-download" thread;
    /// returns at once. Progress arrives as `ModelDownloadProgress`.
    pub fn download_models(&self, engine: Engine) -> CoreResult<()> {
        let state = &self.state;
        if state.downloading.swap(true, Ordering::SeqCst) {
            return Err("A download is already in progress".into());
        }
        let models_dir = state.settings.lock().unwrap().models_dir();
        let thread_state = state.clone();
        let sink = self.sink.clone();
        let notifier = self.notifier.clone();
        let spawned = std::thread::Builder::new()
            .name("model-download".into())
            .spawn(move || {
                let emit = |model_id: &str,
                            file: &str,
                            downloaded: u64,
                            total: Option<u64>,
                            file_index: u32,
                            file_count: u32| {
                    sink.emit(download_progress(
                        model_id, file, downloaded, total, file_index, file_count,
                    ));
                };
                let result = models::download(engine, &models_dir, emit);
                thread_state.downloading.store(false, Ordering::SeqCst);
                sink.emit(download_done(
                    engine.to_string(),
                    result.as_ref().err().map(|e| format!("{e:#}")),
                ));
                match result {
                    Ok(()) => notifier.toast("Model download complete."),
                    Err(e) => notifier.toast(&format!("Model download failed: {e:#}")),
                }
            });
        if let Err(error) = spawned {
            state.downloading.store(false, Ordering::SeqCst);
            return Err(err(error));
        }
        Ok(())
    }

    /// Full CUDA preflight (DLL probing, may repair PATH). ⏳
    pub fn gpu_status(&self) -> GpuStatus {
        assert_off_ui_thread("gpu_status");
        // Full preflight, not just the driver: an unresolvable cuDNN/CUDA runtime
        // DLL makes ort fall back to CPU even with a working driver, and that is
        // by far the most common way GPU transcription silently degrades.
        let status = crate::gpu::preflight();
        let detail = if !status.driver_present {
            "No NVIDIA driver found — transcription will run on CPU".into()
        } else if status.missing_dlls.is_empty() {
            "NVIDIA driver and CUDA/cuDNN runtime found — Parakeet will attempt GPU acceleration"
                .into()
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

    /// Managed GPU-library install state + preflight. ⏳
    pub fn gpu_libs_status(&self) -> GpuLibsInfo {
        assert_off_ui_thread("gpu_libs_status");
        let cuda_dir = self.state.settings.lock().unwrap().cuda_dir();
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

    /// Download the pinned CUDA runtime + cuDNN DLLs into `data_dir/cuda` on
    /// a "gpu-libs-download" thread. Progress reuses the model-download event
    /// stream with id "gpu-libs".
    pub fn download_gpu_libs(&self) -> CoreResult<()> {
        let state = &self.state;
        if state.downloading.swap(true, Ordering::SeqCst) {
            return Err("A download is already in progress".into());
        }
        let cuda_dir = state.settings.lock().unwrap().cuda_dir();
        let thread_state = state.clone();
        let sink = self.sink.clone();
        let notifier = self.notifier.clone();
        let spawned = std::thread::Builder::new()
            .name("gpu-libs-download".into())
            .spawn(move || {
                let emit = |_: &str,
                            file: &str,
                            downloaded: u64,
                            total: Option<u64>,
                            file_index: u32,
                            file_count: u32| {
                    sink.emit(download_progress(
                        "gpu-libs", file, downloaded, total, file_index, file_count,
                    ));
                };
                let result = crate::gpu_libs::download(&cuda_dir, emit);
                thread_state.downloading.store(false, Ordering::SeqCst);
                if result.is_ok() {
                    // Put the fresh install on the loader path right away — the
                    // next transcription uses the GPU without a restart.
                    crate::gpu::preflight();
                }
                sink.emit(download_done(
                    "gpu-libs".into(),
                    result.as_ref().err().map(|e| format!("{e:#}")),
                ));
                match result {
                    Ok(()) => notifier.toast("GPU libraries installed."),
                    Err(e) => notifier.toast(&format!("GPU library download failed: {e:#}")),
                }
            });
        if let Err(error) = spawned {
            state.downloading.store(false, Ordering::SeqCst);
            return Err(err(error));
        }
        Ok(())
    }

    /// Active endpoints for the Settings device pickers. Future selections
    /// store the stable endpoint ID; capture still accepts legacy
    /// friendly-name values. Initialises COM (MTA) on the calling thread,
    /// so never call it on the UI thread. ⏳
    pub fn list_audio_devices(&self) -> CoreResult<AudioDevices> {
        assert_off_ui_thread("list_audio_devices");
        let (render, capture) = crate::audio_capture::list_devices()?;
        Ok(AudioDevices { render, capture })
    }

    /// Support snapshot: quick_check, model fingerprints, GPU preflight. ⏳
    pub fn diagnostics(&self) -> Diagnostics {
        assert_off_ui_thread("diagnostics");
        collect_diagnostics(&self.state)
    }

    /// Save a privacy-conscious support report. It contains paths and
    /// operational state, but never transcript text, notes, audio, voice
    /// prints, or settings. ⏳
    pub fn export_diagnostics(&self, dest: &Path) -> CoreResult<()> {
        assert_off_ui_thread("export_diagnostics");
        let diagnostics = collect_diagnostics(&self.state);
        std::fs::write(dest, diagnostics.report).map_err(err)
    }

    /// Default file name for the diagnostics save dialog.
    pub fn diagnostics_file_name() -> String {
        format!(
            "Witness-diagnostics-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        )
    }

    /// Publish a complete, checksummed recovery snapshot under `parent`.
    /// The native models are intentionally omitted; pinned model provenance
    /// is included and the binaries can be re-downloaded after a restore. ⏳
    pub fn create_backup(&self, parent: &Path) -> CoreResult<BackupSummary> {
        assert_off_ui_thread("create_backup");
        let state = &self.state;
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
            parent,
            &crate::backup::BackupInput {
                db: &state.db,
                settings: &settings,
                data_dir: &data_dir,
                model_status: &model_status,
                app_version: env!("CARGO_PKG_VERSION"),
            },
        )
        .map_err(err)
    }

    /// Restart, part 1: finalize any active recording first and preserve an
    /// actionable error instead of restarting over a failed finalization. ⏳
    pub fn prepare_restart(&self) -> CoreResult<()> {
        assert_off_ui_thread("prepare_restart");
        let recording = self.state.recorder.lock().unwrap().is_recording();
        if recording {
            self.stop_recording(true)?;
        }
        Ok(())
    }

    /// Restart part 2 / install-update final check, on the main thread right
    /// before process exit. Synchronizes the last check with both recording
    /// start and backup start and holds both locks (recorder →
    /// backup_in_progress) until the guard drops, so neither can win a
    /// check-to-exit race. [ui-ok: exit only]
    pub fn exit_guard(&self, purpose: ExitPurpose) -> CoreResult<ExitGuard<'_>> {
        let recorder = self.state.recorder.lock().unwrap();
        let backup_in_progress = self.state.backup_in_progress.lock().unwrap();
        if recorder.is_busy() {
            return Err(match purpose {
                ExitPurpose::Restart => "Wait for recording to stop before restarting Witness",
                ExitPurpose::Update => "Stop the recording before installing the update",
            }
            .into());
        }
        if *backup_in_progress {
            return Err(match purpose {
                ExitPurpose::Restart => "Wait for the backup to finish before restarting Witness",
                ExitPurpose::Update => "Wait for the backup to finish before installing the update",
            }
            .into());
        }
        Ok(ExitGuard {
            _recorder: recorder,
            _backup_in_progress: backup_in_progress,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_file_name_shape() {
        let name = AppCore::diagnostics_file_name();
        assert!(name.starts_with("Witness-diagnostics-"), "{name}");
        assert!(name.ends_with(".txt"), "{name}");
        // Witness-diagnostics-YYYYMMDD-HHMMSS.txt
        assert_eq!(name.len(), "Witness-diagnostics-".len() + 15 + 4);
    }

    #[test]
    fn download_progress_events_are_high_rate_until_done() {
        assert!(download_progress("parakeet", "a.onnx", 1, Some(2), 1, 3).is_high_rate());
        assert!(!download_done("parakeet".into(), None).is_high_rate());
    }
}
