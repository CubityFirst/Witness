//! Settings → Transcription: the engine and what runs it (GPU status, the
//! managed CUDA libraries), when transcription runs (after recording, the
//! "transcript ready" notification, live captions, the floating caption
//! window) and the noise-phrase filter.

use super::storage::installed_text;
use super::{
    button, download_active, download_fraction, download_progress_text, footnote, note,
    notice_done, progress_bar, quiet, row, save, section, start_model_download, status_note, title,
    toggle, warning_strip, SettingsState, Slot, FIELD_WIDTH,
};
use crate::ui::theme;
use crate::ui::toasts::Notice;
use crate::ui::widgets::{self, BadgeKind};
use crate::ui::UiCtx;
use eframe::egui;
use witness_core::events::ModelDownloadProgress;
use witness_core::service::{GpuLibsInfo, GpuStatus};
use witness_core::settings::Engine;

/// Under the engine group.
const DIARIZATION_NOTE: &str =
    "Speaker separation covers up to 8 remote speakers (NVIDIA Nemotron 3 Diarization).";

impl SettingsState {
    /// "Parakeet · GPU" / "Whisper · model not downloaded".
    pub(super) fn transcription_summary(&self, cx: &UiCtx) -> String {
        let engine = cx.settings.current().engine;
        let missing = self.models.iter().any(|m| m.engine == engine && !m.present);
        transcription_summary(engine, missing, self.gpu.as_ref())
    }

    pub(super) fn transcription_page(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let settings = cx.settings.current().clone();
        let dl = cx.shell.download.clone();
        let download_active = download_active(dl.as_ref());
        let missing = self
            .models
            .iter()
            .any(|m| m.engine == settings.engine && !m.present);
        let gpu = self
            .gpu
            .as_ref()
            .map_or_else(|| "checking…".to_string(), |g| g.detail.clone());
        let gpu_libs = self.gpu_libs.as_ref().filter(|l| l.driver_present);
        let mut auto_transcribe = settings.auto_transcribe;
        let mut notify = settings.notify_transcribed;
        let mut live = settings.live_transcribe;
        let mut overlay = settings.caption_overlay;
        let mut engine = settings.engine;
        let junk_text = &mut self.junk_text;

        let (download, libs_action) = section(ui, "Engine", |ui| {
            row(
                ui,
                "engine",
                Slot::Fill(FIELD_WIDTH),
                |ui| {
                    let id = title(ui, "Engine");
                    note(
                        ui,
                        "Parakeet needs an NVIDIA GPU to be fast; Whisper runs on the CPU.",
                    );
                    id
                },
                |ui, label| engine_picker(ui, label, &mut engine),
            );
            let download = missing
                && warning_strip(
                    ui,
                    "Model not downloaded, so transcription will fail",
                    "Download",
                    !download_active,
                );
            widgets::group_divider(ui);
            row(
                ui,
                "gpu",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "GPU");
                    note(ui, &gpu);
                    id
                },
                |_, _| {},
            );
            let libs_action = gpu_libs.and_then(|libs| {
                widgets::group_divider(ui);
                gpu_libs_row(ui, libs, dl.as_ref(), download_active)
            });
            (download, libs_action)
        });
        footnote(ui, DIARIZATION_NOTE);

        let (auto, notify_changed, live_changed, overlay_changed) = section(
            ui,
            "When to transcribe",
            |ui| {
                let auto = toggle(
                    ui,
                    true,
                    &mut auto_transcribe,
                    "Auto-transcribe",
                    "Transcribes each recording as soon as it stops.",
                )
                .changed();
                widgets::group_divider(ui);
                let notify_changed = toggle(
                    ui,
                    true,
                    &mut notify,
                    "Notify when transcribed",
                    "A Windows notification when a transcript is ready.",
                )
                .changed();
                widgets::group_divider(ui);
                let live_changed = toggle(
                    ui,
                    true,
                    &mut live,
                    "Live captions",
                    "Rough captions while recording (Parakeet). The finished transcript replaces them.",
                )
                .changed();
                widgets::group_divider(ui);
                // Disabled without live captions, but its value is kept.
                let overlay_changed = toggle(
                    ui,
                    settings.live_transcribe,
                    &mut overlay,
                    "Open caption window automatically",
                    "Always on top, opened when recording starts. The CC button opens it anytime.",
                )
                .changed();
                (auto, notify_changed, live_changed, overlay_changed)
            },
        );

        let junk = section(ui, "Noise filter", |ui| {
            row(
                ui,
                "junk-phrases",
                Slot::Below,
                |ui| {
                    let id = title(ui, "Discarded noise phrases");
                    note(
                        ui,
                        "Lines that say only one of these are dropped, ignoring case and punctuation. One per line.",
                    );
                    id
                },
                |ui, label| {
                    let field = widgets::text_input(
                        ui,
                        junk_id(),
                        egui::TextEdit::multiline(junk_text)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                    let response = ui.add(field).labelled_by(label);
                    widgets::text_edit_menu(&response);
                    response
                },
            )
        });

        if auto {
            save(cx, |s| s.auto_transcribe = auto_transcribe);
        }
        if notify_changed {
            save(cx, |s| s.notify_transcribed = notify);
        }
        if live_changed {
            save(cx, |s| s.live_transcribe = live);
        }
        if overlay_changed {
            save(cx, |s| s.caption_overlay = overlay);
        }
        self.junk_dirty |= junk.changed();
        // Blur only (Enter inserts a line); a clean buffer is skipped as for
        // the watch patterns.
        if junk.lost_focus() && self.junk_dirty {
            self.commit_junk(cx, parse_lines(&self.junk_text));
        }
        if engine != settings.engine {
            save(cx, |s| s.engine = engine);
        }
        if download {
            start_model_download(cx, settings.engine, "Could not download models".into());
        }
        if libs_action.is_some() {
            // A retry clears the failed badge first.
            cx.shell.download = None;
            start_gpu_libs_download(cx);
        }
    }

    pub(super) fn commit_junk(&mut self, cx: &mut UiCtx, phrases: Vec<String>) {
        self.junk_dirty = false;
        self.junk_source = phrases.clone();
        if phrases != cx.settings.current().junk_phrases {
            save(cx, |s| s.junk_phrases = phrases);
        }
    }
}

pub(super) fn junk_id() -> egui::Id {
    egui::Id::new("settings-junk-phrases")
}

fn start_gpu_libs_download(cx: &mut UiCtx) {
    cx.tasks
        .spawn_op("download-gpu-libs", |c| match c.download_gpu_libs() {
            Ok(()) => quiet(),
            Err(error) => notice_done(Notice::error("Could not download GPU libraries", error)),
        });
}

/// The engine `<select>`.
fn engine_picker(ui: &mut egui::Ui, label: egui::Id, engine: &mut Engine) {
    let width = ui.available_width();
    widgets::select(ui, widgets::SelectKind::TextInput, |ui| {
        egui::ComboBox::from_id_salt("settings-engine")
            .width(width)
            .selected_text(engine_label(*engine))
            .truncate()
            .show_ui(ui, |ui| {
                for option in [Engine::Parakeet, Engine::Whisper] {
                    ui.selectable_value(engine, option, engine_label(option));
                }
            })
            .response
            .labelled_by(label);
    });
}

fn engine_label(engine: Engine) -> &'static str {
    match engine {
        Engine::Parakeet => "Parakeet TDT v3 (fast, GPU)",
        Engine::Whisper => "Whisper large-v3-turbo",
    }
}

/// The GPU-library row; `Some(())` when its download / repair / retry
/// button was clicked.
fn gpu_libs_row(
    ui: &mut egui::Ui,
    libs: &GpuLibsInfo,
    dl: Option<&ModelDownloadProgress>,
    active: bool,
) -> Option<()> {
    let state = gpu_libs_state(libs, dl);
    let fraction = dl
        .filter(|d| d.model_id == GPU_LIBS_ID)
        .and_then(download_fraction);
    let clicked = row(
        ui,
        "gpu-libs",
        Slot::Fit,
        |ui| {
            let id = title(ui, "CUDA runtime + cuDNN (GPU)");
            match &state {
                GpuLibsState::Failed(error) => {
                    status_note(ui, theme::pal().rec_light, &format!("failed: {error}"));
                }
                GpuLibsState::Progress(text) => {
                    status_note(ui, theme::pal().warn, text);
                    if let Some(f) = fraction {
                        progress_bar(ui, f);
                    }
                }
                GpuLibsState::System => note(ui, "Using the system's CUDA libraries."),
                GpuLibsState::Idle {
                    integrity_error: Some(error),
                    ..
                } => {
                    status_note(ui, theme::pal().rec_light, "integrity check failed")
                        .on_hover_text(error);
                }
                _ => note(ui, "Lets Parakeet run on an NVIDIA GPU."),
            }
            id
        },
        |ui, _| match &state {
            GpuLibsState::Installed(text) => {
                widgets::badge(ui, BadgeKind::Ok, text);
                false
            }
            GpuLibsState::Failed(_) => widgets::btn(ui, "Retry").clicked(),
            GpuLibsState::Progress(_) => {
                widgets::badge(ui, BadgeKind::Busy, "downloading");
                false
            }
            GpuLibsState::System => false,
            GpuLibsState::Idle { label, .. } => button(ui, !active, label).clicked(),
        },
    );
    clicked.then_some(())
}

/// Junk phrases: one per line, trimmed, empties dropped.
pub(super) fn parse_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

#[derive(Debug, PartialEq)]
enum GpuLibsState {
    Installed(String),
    Failed(String),
    Progress(String),
    /// System CUDA libraries already resolve; no button.
    System,
    Idle {
        integrity_error: Option<String>,
        label: String,
    },
}

fn gpu_libs_state(libs: &GpuLibsInfo, dl: Option<&ModelDownloadProgress>) -> GpuLibsState {
    if libs.present {
        return GpuLibsState::Installed(installed_text(libs.size_mb));
    }
    if let Some(dl) = dl.filter(|d| d.model_id == GPU_LIBS_ID) {
        return match &dl.error {
            Some(error) => GpuLibsState::Failed(error.clone()),
            None => GpuLibsState::Progress(download_progress_text(dl)),
        };
    }
    if libs.cuda_ready {
        return GpuLibsState::System;
    }
    let label = if libs.integrity_error.is_some() {
        "Repair".into()
    } else {
        format!("Download (~{:.1} GB)", libs.download_mb as f64 / 1000.0)
    };
    GpuLibsState::Idle {
        integrity_error: libs.integrity_error.clone(),
        label,
    }
}

/// The GPU-library download's progress id.
const GPU_LIBS_ID: &str = "gpu-libs";

/// The Transcription category's summary: the engine, and whether it can
/// run (model present) and where (GPU or CPU).
fn transcription_summary(engine: Engine, missing: bool, gpu: Option<&GpuStatus>) -> String {
    let name = match engine {
        Engine::Parakeet => "Parakeet",
        Engine::Whisper => "Whisper",
    };
    let state = if missing {
        "model not downloaded"
    } else {
        match (engine, gpu) {
            (Engine::Whisper, _) => "CPU",
            (Engine::Parakeet, Some(g)) if g.cuda_available => "GPU",
            (Engine::Parakeet, Some(_)) => "CPU fallback",
            (Engine::Parakeet, None) => "checking GPU…",
        }
    };
    format!("{name} · {state}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(model_id: &str, done: bool, error: Option<&str>) -> ModelDownloadProgress {
        ModelDownloadProgress {
            model_id: model_id.into(),
            file: "cudnn.whl".into(),
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

    #[test]
    fn gpu_libs_row_states() {
        let mut libs = GpuLibsInfo {
            present: false,
            size_mb: Some(1043.6),
            integrity_error: None,
            driver_present: true,
            cuda_ready: false,
            download_mb: 1234,
        };
        assert_eq!(
            gpu_libs_state(&libs, None),
            GpuLibsState::Idle {
                integrity_error: None,
                label: "Download (~1.2 GB)".into()
            }
        );
        let running = progress(GPU_LIBS_ID, false, None);
        assert!(matches!(
            gpu_libs_state(&libs, Some(&running)),
            GpuLibsState::Progress(_)
        ));
        let failed = progress(GPU_LIBS_ID, true, Some("disk full"));
        assert_eq!(
            gpu_libs_state(&libs, Some(&failed)),
            GpuLibsState::Failed("disk full".into())
        );
        // Another download's progress does not touch this row.
        let other = progress("parakeet", false, None);
        libs.cuda_ready = true;
        assert_eq!(gpu_libs_state(&libs, Some(&other)), GpuLibsState::System);
        libs.cuda_ready = false;
        libs.integrity_error = Some("missing cudnn64_9.dll".into());
        assert!(matches!(
            gpu_libs_state(&libs, None),
            GpuLibsState::Idle { label, .. } if label == "Repair"
        ));
        libs.present = true;
        assert_eq!(
            gpu_libs_state(&libs, None),
            GpuLibsState::Installed("installed · 1044 MB".into())
        );
    }

    #[test]
    fn noise_phrases_parse_one_per_line() {
        assert_eq!(
            parse_lines("you\n\n  thank you.  \r\n"),
            vec!["you".to_string(), "thank you.".to_string()]
        );
    }

    #[test]
    fn summary_says_where_the_engine_runs() {
        let gpu = |cuda_available| GpuStatus {
            cuda_available,
            detail: String::new(),
        };
        assert_eq!(
            transcription_summary(Engine::Parakeet, false, Some(&gpu(true))),
            "Parakeet · GPU"
        );
        assert_eq!(
            transcription_summary(Engine::Parakeet, false, Some(&gpu(false))),
            "Parakeet · CPU fallback"
        );
        assert_eq!(
            transcription_summary(Engine::Whisper, true, None),
            "Whisper · model not downloaded"
        );
        assert_eq!(
            transcription_summary(Engine::Whisper, false, Some(&gpu(true))),
            "Whisper · CPU"
        );
    }
}
