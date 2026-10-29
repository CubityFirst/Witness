//! Settings → Storage: the data folder (change, restart to apply), backup,
//! and the downloaded models (download / repair each, delete the cache).
//! The download helpers are shared with Transcription and the first-run
//! setup.

use super::{
    button, fill, footnote, note, notice_done, path_box, progress_bar, quiet, row, section,
    status_note, title, OpRun, SettingsState, Slot,
};
use crate::platform::dialogs;
use crate::ui::theme::{self, pal};
use crate::ui::toasts::Notice;
use crate::ui::widgets::{self, BadgeKind, Politeness};
use crate::ui::{UiCmd, UiCtx};
use eframe::egui::{self, RichText};
use witness_core::backup::BackupSummary;
use witness_core::events::ModelDownloadProgress;
use witness_core::models::ModelInfo;
use witness_core::settings::Engine;

/// The backup folder picker's title (commands.rs `create_backup`).
const BACKUP_DIALOG_TITLE: &str = "Choose where to create the Witness backup";

impl SettingsState {
    /// "Models 4.3 GB · data in D:\Witness".
    pub(super) fn storage_summary(&self, cx: &UiCtx) -> String {
        storage_summary(
            self.cache_bytes,
            cx.settings.current().data_dir.as_deref(),
            self.restart_pending(),
        )
    }

    pub(super) fn storage_page(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        self.data_section(ui, cx);
        self.models_section(ui, cx);
    }

    /// A data_dir change was saved and applies after a restart.
    fn restart_pending(&self) -> bool {
        self.needs_restart
            || self
                .diagnostics
                .as_ref()
                .is_some_and(|d| d.restart_required)
    }

    fn data_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let data_dir = cx.settings.current().data_dir.clone();
        cx.fonts.note_texts(ui.ctx(), data_dir.as_deref());
        if let Some(backup) = &self.last_backup {
            cx.fonts.note_texts(ui.ctx(), [backup.path.as_str()]);
        }
        let running = self.backup.is_some();
        let restart_pending = self.restart_pending();
        let last_backup = self.last_backup.as_ref();

        let (change, backup, restart) = section(ui, "Data", |ui| {
            let change = row(
                ui,
                "data-dir",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Data directory");
                    note(
                        ui,
                        "Holds the database, recordings and models. Changing it does not move existing data.",
                    );
                    path_box(ui, data_dir.as_deref().unwrap_or("(next to witness.exe)"));
                    id
                },
                |ui, _| widgets::btn(ui, "Change…").clicked(),
            );
            let mut restart = false;
            if restart_pending {
                widgets::group_divider(ui);
                restart = row(
                    ui,
                    "restart",
                    Slot::Fit,
                    |ui| {
                        let id = title(ui, "Restart required");
                        note(ui, "The new data directory applies after a restart.");
                        id
                    },
                    |ui, _| widgets::btn_primary(ui, "Restart Witness").clicked(),
                );
            }
            widgets::group_divider(ui);
            let backup = row(
                ui,
                "backup",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Backup");
                    note(
                        ui,
                        "A verified copy of the database, recordings and settings (not models). Not while recording or transcribing.",
                    );
                    if let Some(backup) = last_backup {
                        // "Backup saved to <path> (N files, X MB). Models …"
                        ui.add_space(theme::S1);
                        note(ui, "Backup saved to");
                        path_box(ui, &backup.path);
                        note(ui, &backup_details(backup));
                    }
                    id
                },
                |ui, _| {
                    let label = if running {
                        "Creating backup..."
                    } else {
                        "Create backup..."
                    };
                    button(ui, !running, label).clicked()
                },
            );
            (change, backup, restart)
        });

        if let Some(backup) = &self.last_backup {
            // `<p role="status">`: screen readers announce the finished backup.
            widgets::announce(
                ui.ctx(),
                egui::Id::new("settings-backup-status"),
                &format!("Backup saved to {} {}", backup.path, backup_details(backup)),
                Politeness::Polite,
            );
        }
        if change && self.loads.pick_dir.is_none() {
            let owner = cx.dialog_owner();
            self.loads.pick_dir = Some(cx.tasks.spawn("pick-data-dir", move |_| {
                dialogs::pick_folder_blocking(owner, None)
            }));
        }
        if backup {
            self.start_backup(cx);
        }
        if restart {
            self.restart(cx);
        }
    }

    fn models_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let dl = cx.shell.download.clone();
        let active = download_active(dl.as_ref());
        let deleting = self.deleting_cache.is_some();
        let cache_bytes = self.cache_bytes;
        let models = &self.models;

        let (model_action, delete) = section(ui, "Models", |ui| {
            let mut model_action = None;
            for model in models {
                if let Some(engine) = model_row(ui, model, dl.as_ref(), active) {
                    model_action = Some((engine, model.display_name.clone()));
                }
                widgets::group_divider(ui);
            }
            let delete = row(
                ui,
                "model-cache",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Model cache");
                    note(ui, &cache_text(cache_bytes));
                    id
                },
                |ui, _| {
                    let enabled = !active && !deleting && cache_bytes.is_some_and(|b| b > 0);
                    let label = if deleting {
                        "Deleting…"
                    } else {
                        "Delete model cache"
                    };
                    // The tooltip shows while disabled too, like the button's title.
                    let tip = "Deletes every downloaded model, including old versions (Shift-click skips confirmation)";
                    ui.add_enabled_ui(enabled, |ui| widgets::btn_danger(ui, label))
                        .inner
                        .on_hover_text(tip)
                        .on_disabled_hover_text(tip)
                        .clicked()
                },
            );
            (model_action, delete)
        });
        footnote(
            ui,
            "Recording works without models; they're only needed for transcription.",
        );

        if let Some((engine, name)) = model_action {
            // A retry clears the failed badge first.
            cx.shell.download = None;
            start_model_download(cx, engine, format!("Could not download {name}"));
        }
        if delete {
            if ui.input(|i| i.modifiers.shift) {
                self.delete_cache(cx);
            } else {
                cx.confirm.ask(
                    cache_confirm_id(),
                    "Delete all downloaded models?\nTranscription stops working until you download them again.",
                    "Delete models",
                );
                self.asked_cache = true;
            }
        }
    }

    fn start_backup(&mut self, cx: &mut UiCtx) {
        let owner = cx.dialog_owner();
        self.backup = Some(OpRun::spawn(cx, "create-backup", move |c, slot| {
            // Cancel returns quietly, like the command's `None`.
            let Some(parent) = dialogs::pick_folder_blocking(owner, Some(BACKUP_DIALOG_TITLE))
            else {
                return quiet();
            };
            let notice = match c.create_backup(&parent) {
                Ok(backup) => {
                    fill(slot, backup);
                    Notice::success("Backup created")
                }
                Err(error) => Notice::error("Could not create backup", error),
            };
            notice_done(notice)
        }));
    }

    pub(super) fn delete_cache(&mut self, cx: &mut UiCtx) {
        if self.deleting_cache.is_some() {
            return;
        }
        let handle = cx.tasks.spawn_op("delete-model-cache", |c| {
            notice_done(match c.delete_model_cache() {
                Ok(freed) => Notice::success(format!(
                    "Deleted model cache, freed {:.2} GB",
                    freed as f64 / 1e9
                )),
                Err(error) => Notice::error("Could not delete the model cache", error),
            })
        });
        self.deleting_cache = Some(handle);
    }

    /// Restart Witness after a data_dir change. The decision reads the
    /// cached status; a running recording needs an explicit confirmation
    /// (Shift does not skip it). `logic()` runs the restart (§4.5).
    fn restart(&mut self, cx: &mut UiCtx) {
        match cx.shell.status.as_ref() {
            None => cx.toasts.error(
                "Could not check recording status; restart cancelled",
                "status not loaded yet",
            ),
            Some(status) if status.recording => {
                cx.confirm.ask(
                    restart_confirm_id(),
                    "Restarting will stop and save the current recording. Restart now?",
                    "Stop & restart",
                );
                self.asked_restart = true;
            }
            Some(_) => cx.cmds.push(UiCmd::Restart),
        }
    }
}

/// `download_models(engine)` (returns at once; progress is global).
pub(crate) fn start_model_download(cx: &mut UiCtx, engine: Engine, failure: String) {
    cx.tasks.spawn_op("download-models", move |c| {
        match c.download_models(engine) {
            Ok(()) => quiet(),
            Err(error) => notice_done(Notice::error(&failure, error)),
        }
    });
}

pub(super) fn cache_confirm_id() -> egui::Id {
    egui::Id::new("settings-delete-model-cache")
}

pub(super) fn restart_confirm_id() -> egui::Id {
    egui::Id::new("settings-restart")
}

/// A model's row; returns the engine to download when its Download /
/// Repair / Retry button was clicked.
fn model_row(
    ui: &mut egui::Ui,
    model: &ModelInfo,
    dl: Option<&ModelDownloadProgress>,
    active: bool,
) -> Option<Engine> {
    let state = model_state(model, dl);
    let fraction = dl
        .filter(|d| d.model_id == model.id)
        .and_then(download_fraction);
    let clicked = row(
        ui,
        ("model", &model.id),
        Slot::Fit,
        |ui| {
            let id = title(ui, &model.display_name);
            let short: String = model.expected_revision.chars().take(8).collect();
            ui.label(
                RichText::new(format!("rev {short}"))
                    .font(egui::FontId::monospace(11.5))
                    .color(pal().text_muted),
            )
            .on_hover_text(format!("Expected revision {}", model.expected_revision));
            match &state {
                ModelState::Failed(error) => {
                    status_note(ui, theme::pal().rec_light, &format!("failed: {error}"));
                }
                ModelState::Progress(text) => {
                    status_note(ui, theme::pal().warn, text);
                    if let Some(f) = fraction {
                        progress_bar(ui, f);
                    }
                }
                ModelState::Idle {
                    integrity_error: Some(error),
                } => {
                    status_note(ui, theme::pal().rec_light, "integrity check failed")
                        .on_hover_text(error);
                }
                _ => {}
            }
            id
        },
        |ui, _| match &state {
            ModelState::Installed { text, tooltip } => {
                widgets::badge(ui, BadgeKind::Ok, text).on_hover_text(tooltip);
                false
            }
            ModelState::Failed(_) => widgets::btn(ui, "Retry").clicked(),
            ModelState::Progress(_) => {
                widgets::badge(ui, BadgeKind::Busy, "downloading");
                false
            }
            ModelState::Waiting => {
                widgets::badge(ui, BadgeKind::Busy, "waiting…");
                false
            }
            ModelState::Idle { integrity_error } => {
                let label = if integrity_error.is_some() {
                    "Repair"
                } else {
                    "Download"
                };
                button(ui, !active, label).clicked()
            }
        },
    );
    clicked.then_some(model.engine)
}

/// `dl != null && !dl.done`: a failed download keeps its badge but no
/// longer disables the buttons.
pub(crate) fn download_active(dl: Option<&ModelDownloadProgress>) -> bool {
    dl.is_some_and(|d| !d.done)
}

/// The progress badge text (map-ui-settings §1.9): the percentage and size
/// are the whole job's, falling back to the current file's.
pub(crate) fn download_progress_text(p: &ModelDownloadProgress) -> String {
    let (downloaded, total) = match p.job_total_bytes.filter(|t| *t > 0) {
        Some(total) => (p.job_downloaded_bytes, Some(total)),
        None => (p.downloaded_bytes, p.total_bytes.filter(|t| *t > 0)),
    };
    match total {
        Some(total) => format!(
            "file {} of {} · {}% of {} MB",
            p.file_index,
            p.file_count,
            (downloaded as f64 / total as f64 * 100.0).round(),
            (total as f64 / 1e6).round()
        ),
        None => format!(
            "file {} of {} · {} MB…",
            p.file_index,
            p.file_count,
            (p.downloaded_bytes as f64 / 1e6).round()
        ),
    }
}

/// How far a download is overall (0..1), when its size is known: by bytes
/// across the whole job, else the finished files plus the current file's
/// share.
pub(crate) fn download_fraction(p: &ModelDownloadProgress) -> Option<f32> {
    if let Some(total) = p.job_total_bytes.filter(|t| *t > 0) {
        return Some((p.job_downloaded_bytes as f64 / total as f64).clamp(0.0, 1.0) as f32);
    }
    let total = p.total_bytes.filter(|t| *t > 0)?;
    let count = p.file_count.max(1) as f32;
    let file = (p.downloaded_bytes as f32 / total as f32).clamp(0.0, 1.0);
    let done = (p.file_index.max(1) - 1) as f32;
    Some(((done + file) / count).clamp(0.0, 1.0))
}

pub(super) fn installed_text(size_mb: Option<f64>) -> String {
    match size_mb {
        Some(mb) => format!("installed · {} MB", mb.round()),
        None => "installed".into(),
    }
}

/// A model row's state widget, in settings.tsx's decision order.
#[derive(Debug, PartialEq)]
enum ModelState {
    Installed {
        text: String,
        tooltip: String,
    },
    Failed(String),
    Progress(String),
    /// A sibling model of the engine being downloaded.
    Waiting,
    /// Download / Repair button, with an optional integrity badge.
    Idle {
        integrity_error: Option<String>,
    },
}

fn model_state(model: &ModelInfo, dl: Option<&ModelDownloadProgress>) -> ModelState {
    if model.present {
        return ModelState::Installed {
            text: installed_text(model.size_mb),
            tooltip: format!(
                "Verified revision {}",
                model.installed_revision.as_deref().unwrap_or("unknown")
            ),
        };
    }
    if let Some(dl) = dl.filter(|d| d.model_id == model.id) {
        return match &dl.error {
            Some(error) => ModelState::Failed(error.clone()),
            None => ModelState::Progress(download_progress_text(dl)),
        };
    }
    if download_active(dl) && dl.is_some_and(|d| d.model_id == model.engine.to_string()) {
        return ModelState::Waiting;
    }
    // A model that was never installed has no manifest entry and so an
    // "integrity" error too ("… has no verified installation manifest"):
    // that is plain "not downloaded", not a failed check to repair.
    ModelState::Idle {
        integrity_error: model
            .installed_revision
            .as_ref()
            .and(model.integrity_error.clone()),
    }
}

fn cache_text(bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) => format!("{:.2} GB on disk", bytes as f64 / 1e9),
        None => "…".into(),
    }
}

/// "(N files, X MB). Models can be re-downloaded and are not copied."
fn backup_details(backup: &BackupSummary) -> String {
    format!(
        "({} files, {:.1} MB). Models can be re-downloaded and are not copied.",
        backup.file_count,
        backup.total_bytes as f64 / 1_048_576.0
    )
}

/// The Storage category's summary: the model cache size and where the data
/// lives (or that a restart is pending).
fn storage_summary(cache_bytes: Option<u64>, data_dir: Option<&str>, restart: bool) -> String {
    let models = match cache_bytes {
        Some(bytes) => format!("Models {:.1} GB", bytes as f64 / 1e9),
        None => "Models …".to_string(),
    };
    if restart {
        return format!("{models} · restart to apply the new data folder");
    }
    match data_dir {
        Some(dir) => format!("{models} · data in {dir}"),
        None => format!("{models} · data next to witness.exe"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_has_the_cache_and_the_data_folder() {
        assert_eq!(
            storage_summary(Some(4_312_000_000), Some("D:\\Witness"), false),
            "Models 4.3 GB · data in D:\\Witness"
        );
        assert_eq!(
            storage_summary(None, None, false),
            "Models … · data next to witness.exe"
        );
        assert_eq!(
            storage_summary(Some(0), None, true),
            "Models 0.0 GB · restart to apply the new data folder"
        );
    }

    fn progress(model_id: &str, done: bool, error: Option<&str>) -> ModelDownloadProgress {
        ModelDownloadProgress {
            model_id: model_id.into(),
            file: "encoder.onnx".into(),
            downloaded_bytes: 250_000_000,
            total_bytes: Some(1_000_000_000),
            file_index: 2,
            file_count: 5,
            job_downloaded_bytes: 0,
            job_total_bytes: None,
            done,
            error: error.map(String::from),
        }
    }

    fn model(id: &str, engine: Engine, present: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            engine,
            display_name: id.into(),
            present,
            size_mb: Some(612.4),
            expected_revision: "0123456789abcdef".into(),
            installed_revision: present.then(|| "0123456789abcdef".into()),
            integrity_error: None,
        }
    }

    #[test]
    fn progress_text_with_and_without_a_total() {
        let mut p = progress("parakeet", false, None);
        assert_eq!(download_progress_text(&p), "file 2 of 5 · 25% of 1000 MB");
        p.total_bytes = None;
        assert_eq!(download_progress_text(&p), "file 2 of 5 · 250 MB…");
        p.total_bytes = Some(0);
        assert_eq!(download_progress_text(&p), "file 2 of 5 · 250 MB…");
    }

    #[test]
    fn progress_text_and_fraction_cover_the_whole_job() {
        let mut p = progress("parakeet", false, None);
        p.job_downloaded_bytes = 1_500_000_000;
        p.job_total_bytes = Some(3_000_000_000);
        assert_eq!(download_progress_text(&p), "file 2 of 5 · 50% of 3000 MB");
        assert_eq!(download_fraction(&p), Some(0.5));
    }

    #[test]
    fn download_fraction_spans_all_files() {
        let mut p = progress("parakeet", false, None);
        // File 2 of 5, a quarter through: (1 + 0.25) / 5.
        assert_eq!(download_fraction(&p), Some(0.25));
        p.file_index = 5;
        p.downloaded_bytes = 1_000_000_000;
        assert_eq!(download_fraction(&p), Some(1.0));
        p.total_bytes = None;
        assert_eq!(download_fraction(&p), None);
    }

    #[test]
    fn model_badges_follow_the_decision_order() {
        let installed = model("parakeet", Engine::Parakeet, true);
        assert_eq!(
            model_state(&installed, None),
            ModelState::Installed {
                text: "installed · 612 MB".into(),
                tooltip: "Verified revision 0123456789abcdef".into()
            }
        );
        let parakeet = model("parakeet", Engine::Parakeet, false);
        let sortformer = model("sortformer", Engine::Parakeet, false);
        let whisper = model("whisper", Engine::Whisper, false);

        let running = progress("parakeet", false, None);
        assert_eq!(
            model_state(&parakeet, Some(&running)),
            ModelState::Progress("file 2 of 5 · 25% of 1000 MB".into())
        );
        // Siblings wait while the engine-named model downloads.
        assert_eq!(
            model_state(&sortformer, Some(&running)),
            ModelState::Waiting
        );
        assert_eq!(
            model_state(&whisper, Some(&running)),
            ModelState::Idle {
                integrity_error: None
            }
        );
        // The failed done event carries the engine id: only that row fails.
        let failed = progress("parakeet", true, Some("offline"));
        assert_eq!(
            model_state(&parakeet, Some(&failed)),
            ModelState::Failed("offline".into())
        );
        assert_eq!(
            model_state(&sortformer, Some(&failed)),
            ModelState::Idle {
                integrity_error: None
            }
        );
        assert!(!download_active(Some(&failed)));
        assert!(download_active(Some(&running)));

        let mut broken = model("whisper", Engine::Whisper, false);
        broken.installed_revision = Some("0123456789abcdef".into());
        broken.integrity_error = Some("hash mismatch".into());
        assert_eq!(
            model_state(&broken, None),
            ModelState::Idle {
                integrity_error: Some("hash mismatch".into())
            }
        );
        // Never downloaded: Download, not Repair.
        let mut absent = model("whisper", Engine::Whisper, false);
        absent.integrity_error = Some("whisper has no verified installation manifest".into());
        assert_eq!(
            model_state(&absent, None),
            ModelState::Idle {
                integrity_error: None
            }
        );
    }

    #[test]
    fn small_formatters() {
        assert_eq!(cache_text(None), "…");
        assert_eq!(cache_text(Some(4_312_000_000)), "4.31 GB on disk");
        assert_eq!(installed_text(None), "installed");
        let backup = BackupSummary {
            path: "D:\\b".into(),
            created_at: String::new(),
            file_count: 37,
            total_bytes: 36_175_872,
        };
        assert_eq!(
            backup_details(&backup),
            "(37 files, 34.5 MB). Models can be re-downloaded and are not copied."
        );
    }
}
