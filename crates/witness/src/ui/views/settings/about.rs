//! Settings → About: the version and updates, a privacy summary (what stays
//! on this computer, what uses the network, where the data lives) and the
//! diagnostics report (copy / export / refresh).

use super::{
    footnote, kv_row, note, notice_done, path_box, path_value, quiet, row, section, title, updates,
    value, SettingsPage, SettingsState, Slot,
};
use crate::platform::dialogs;
use crate::ui::theme;
use crate::ui::toasts::Notice;
use crate::ui::widgets::{self, BadgeKind};
use crate::ui::UiCtx;
use eframe::egui;
use std::time::{Duration, Instant};
use witness_core::ai_client::BaseUrl;
use witness_core::service::Diagnostics;
use witness_core::settings::Settings;
use witness_core::AppCore;

/// What the diagnostics report holds (and does not).
const DIAGNOSTICS_PRIVACY: &str = "This report contains operational state and local paths, but no audio, transcript text, notes, voice prints, or private settings.";
/// "Copied ✓" stays this long.
const COPIED_FLASH: Duration = Duration::from_millis(1500);

impl SettingsState {
    /// "Witness 0.3.0 · up to date".
    pub(super) fn about_summary(&self, cx: &UiCtx) -> String {
        updates::summary(env!("CARGO_PKG_VERSION"), &cx.shell.update)
    }

    pub(super) fn about_page(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        self.updates.show(ui, cx);
        self.privacy_section(ui, cx);
        self.diagnostics_section(ui, cx);
    }

    /// What stays here, what uses the network, and where the data lives.
    fn privacy_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let settings = cx.settings.current();
        let network = network_text(settings);
        let dir = match &self.diagnostics {
            Some(d) => d.runtime_data_dir.clone(),
            None => settings
                .data_dir
                .clone()
                .unwrap_or_else(|| "(next to witness.exe)".into()),
        };
        cx.fonts.note_texts(ui.ctx(), [dir.as_str()]);
        let storage = section(ui, "Privacy", |ui| {
            row(
                ui,
                "privacy-local",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "On this computer");
                    note(
                        ui,
                        "Recording, transcription and voice matching run on-device. Audio and voice prints never leave it.",
                    );
                    id
                },
                |_, _| {},
            );
            widgets::group_divider(ui);
            row(
                ui,
                "privacy-network",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Network use");
                    note(ui, &network);
                    id
                },
                |_, _| {},
            );
            widgets::group_divider(ui);
            row(
                ui,
                "privacy-data",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Where your data lives");
                    note(
                        ui,
                        "The database, recordings, models and log, stored unencrypted.",
                    );
                    path_box(ui, &dir);
                    id
                },
                |ui, _| widgets::btn(ui, "Storage settings").clicked(),
            )
        });
        if storage {
            self.open_page(cx, SettingsPage::Storage);
        }
    }

    fn diagnostics_section(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let Some(d) = self.diagnostics.clone() else {
            let error = self.diagnostics_error.clone();
            section(ui, "Diagnostics", |ui| match &error {
                Some(error) => {
                    widgets::inline_warning(ui, &format!("Diagnostics unavailable: {error}"));
                }
                None => note(ui, "Checking…"),
            });
            footnote(ui, DIAGNOSTICS_PRIVACY);
            return;
        };
        // User-profile paths may hold any script: load its fallback font.
        cx.fonts.note_texts(
            ui.ctx(),
            [
                d.runtime_data_dir.as_str(),
                d.database.path.as_str(),
                d.log_path.as_str(),
            ],
        );
        let copied = self
            .diagnostics_copied_at
            .is_some_and(|at| at.elapsed() < COPIED_FLASH);
        let error = self.diagnostics_error.clone();

        let (refresh, copy, export) = section(ui, "Diagnostics", |ui| {
            if let Some(error) = &error {
                widgets::inline_warning(ui, &format!("Diagnostics unavailable: {error}"));
            }
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = theme::S2;
                kv_row(ui, "Build", |ui| {
                    value(ui, &format!("Witness {} · {}", d.app_version, d.platform));
                });
                kv_row(ui, "Runtime data", |ui| {
                    let pending = d.restart_required.then_some("restart pending");
                    path_value(
                        ui,
                        &d.runtime_data_dir,
                        pending.map(|t| (BadgeKind::Busy, t)),
                    );
                });
                kv_row(ui, "Database", |ui| {
                    let badge = if d.database.healthy {
                        (BadgeKind::Ok, "healthy")
                    } else {
                        (BadgeKind::Failed, "check failed")
                    };
                    if let Some(r) = path_value(ui, &d.database.path, Some(badge)) {
                        r.on_hover_text(&d.database.detail);
                    }
                });
                kv_row(ui, "Log", |ui| {
                    path_value(ui, &d.log_path, None);
                });
                for (label, text) in [
                    ("Recording", recording_text(&d)),
                    ("Processing", processing_text(&d)),
                    ("Capture health", capture_health_text(&d)),
                    ("Model integrity", model_integrity_text(&d)),
                    ("GPU runtime", gpu_runtime_text(&d)),
                ] {
                    kv_row(ui, label, |ui| value(ui, &text));
                }
            });
            widgets::group_divider(ui);
            row(
                ui,
                "diagnostics-actions",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Report");
                    note(
                        ui,
                        &format!("generated {}", fmt_generated_at(&d.generated_at)),
                    );
                    id
                },
                |ui, _| {
                    let refresh = widgets::btn(ui, "Refresh").clicked();
                    let label = if copied { "Copied ✓" } else { "Copy report" };
                    let copy = widgets::btn(ui, label).clicked();
                    let export = widgets::btn(ui, "Export…").clicked();
                    (refresh, copy, export)
                },
            )
        });
        footnote(ui, DIAGNOSTICS_PRIVACY);

        if let Some(at) = self.diagnostics_copied_at {
            match COPIED_FLASH.checked_sub(at.elapsed()) {
                Some(left) => ui.ctx().request_repaint_after(left),
                None => self.diagnostics_copied_at = None,
            }
        }
        if refresh {
            self.refresh_diagnostics(cx);
        }
        if copy {
            ui.ctx().copy_text(d.report.clone());
            self.diagnostics_copied_at = Some(Instant::now());
            cx.toasts.success("Diagnostic report copied");
        }
        if export {
            export_diagnostics(cx);
        }
    }
}

/// Save dialog → `export_diagnostics`; a cancel is silent.
fn export_diagnostics(cx: &mut UiCtx) {
    let owner = cx.dialog_owner();
    cx.tasks.spawn_op("export-diagnostics", move |c| {
        let request = dialogs::SaveFileRequest {
            file_name: AppCore::diagnostics_file_name(),
            filter_name: "Text report".into(),
            extensions: vec!["txt".into()],
        };
        let Some(dest) = dialogs::save_file_blocking(owner, request) else {
            return quiet();
        };
        notice_done(match c.export_diagnostics(&dest) {
            Ok(()) => Notice::success("Diagnostic report exported"),
            Err(error) => Notice::error("Could not export diagnostics", error),
        })
    });
}

/// The diagnostics snapshot time, local, with seconds (`toLocaleString()`
/// in en-GB: two refreshes within a minute must still look different).
/// Unparseable input is returned unchanged.
fn fmt_generated_at(rfc3339: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(at) => at
            .with_timezone(&chrono::Local)
            .format("%d/%m/%Y, %H:%M:%S")
            .to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

fn recording_text(d: &Diagnostics) -> String {
    match d.recording_meeting_id {
        Some(id) => format!("{} · meeting {id}", d.recording_state),
        None => d.recording_state.clone(),
    }
}

fn processing_text(d: &Diagnostics) -> String {
    let head = match d.processing_meeting_id {
        None => "idle".to_string(),
        Some(id) => format!(
            "meeting {id} · {}{}",
            d.processing_stage.as_deref().unwrap_or("starting"),
            d.processing_pct
                .map_or_else(String::new, |pct| format!(" · {pct:.1}%"))
        ),
    };
    format!("{head} · {} queued", d.queue_len)
}

fn capture_health_text(d: &Diagnostics) -> String {
    let Some(health) = &d.capture_health else {
        return "not available in this session".into();
    };
    let connected = |c: bool| if c { "connected" } else { "disconnected" };
    format!(
        "{}: mic {}, {} ms dropped · output {}, {} ms dropped",
        if d.capture_health_is_current {
            "current"
        } else {
            "last recording"
        },
        connected(health.mic.connected),
        health.mic.capture_dropped_ms,
        connected(health.loopback.connected),
        health.loopback.capture_dropped_ms
    )
}

fn model_integrity_text(d: &Diagnostics) -> String {
    let present = d.models.iter().filter(|m| m.present).count();
    // Only installed models can fail a check (a never-downloaded one has no
    // manifest entry, which the core also reports as an integrity error).
    let failed = d
        .models
        .iter()
        .any(|m| m.installed_revision.is_some() && m.integrity_error.is_some());
    format!(
        "{present}/{} verified{}",
        d.models.len(),
        if failed {
            " · one or more checks failed"
        } else {
            ""
        }
    )
}

fn gpu_runtime_text(d: &Diagnostics) -> String {
    let gpu = &d.gpu;
    let head = if gpu.cuda_ready {
        "CUDA runtime ready".to_string()
    } else if gpu.driver_present {
        let missing = if gpu.missing_dlls.is_empty() {
            "runtime libraries".to_string()
        } else {
            gpu.missing_dlls.join(", ")
        };
        format!("CPU fallback · missing {missing}")
    } else {
        "CPU fallback · no NVIDIA driver".to_string()
    };
    let tail = match &gpu.managed_libraries_integrity_error {
        Some(error) => format!(" · managed install check failed: {error}"),
        None if gpu.managed_libraries_present => " · managed libraries verified".into(),
        None => String::new(),
    };
    format!("{head}{tail}")
}

/// The privacy group's "Network use" line, from the current settings.
fn network_text(settings: &Settings) -> String {
    let updates = if settings.check_for_updates {
        "on"
    } else {
        "off"
    };
    let ai = if settings.ai.enabled {
        match BaseUrl::parse(&settings.ai.base_url) {
            Ok(base) if base.is_local() => "on, a server on this computer".to_string(),
            Ok(base) => format!("on, sends transcript text to {}", base.host()),
            Err(_) => "on, not set up".to_string(),
        }
    } else {
        "off".to_string()
    };
    format!(
        "Only downloads you start (models, GPU libraries), the update check ({updates}) and the AI assistant ({ai})."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use witness_core::recorder::{RecordingHealth, TrackHealth};
    use witness_core::service::{DiagnosticDatabase, DiagnosticGpu, DiagnosticModel};

    #[test]
    fn network_line_follows_the_settings() {
        let mut settings = Settings {
            check_for_updates: true,
            ..Settings::default()
        };
        settings.ai.enabled = false;
        assert_eq!(
            network_text(&settings),
            "Only downloads you start (models, GPU libraries), the update check (on) and the AI assistant (off)."
        );
        settings.check_for_updates = false;
        settings.ai.enabled = true;
        settings.ai.base_url = "https://api.openai.com/v1".into();
        assert!(network_text(&settings)
            .ends_with("update check (off) and the AI assistant (on, sends transcript text to api.openai.com)."));
        settings.ai.base_url = "http://localhost:11434/v1".into();
        assert!(network_text(&settings).ends_with("(on, a server on this computer)."));
    }

    #[test]
    fn diagnostics_time_keeps_seconds() {
        let text = fmt_generated_at("2026-10-06T10:05:33+00:00");
        // Local time zone varies; the shape and the seconds do not.
        assert_eq!(text.len(), "06/10/2026, 10:05:33".len(), "{text}");
        assert!(text.ends_with(":33") && text.contains("/2026, "), "{text}");
        assert_eq!(fmt_generated_at("not a date"), "not a date");
    }

    fn diagnostics() -> Diagnostics {
        Diagnostics {
            generated_at: "2026-10-05T10:00:00+01:00".into(),
            app_version: "0.2.1".into(),
            platform: "windows / x86_64".into(),
            runtime_data_dir: "C:\\data".into(),
            configured_data_dir: None,
            restart_required: false,
            database: DiagnosticDatabase {
                path: "C:\\data\\witness.db".into(),
                healthy: true,
                detail: "ok".into(),
            },
            log_path: "C:\\data\\witness.log".into(),
            recording_state: "idle".into(),
            recording_meeting_id: None,
            recording_since: None,
            capture_health: None,
            capture_health_is_current: false,
            processing_meeting_id: None,
            processing_stage: None,
            processing_pct: None,
            queue_len: 0,
            models: vec![
                DiagnosticModel {
                    id: "parakeet".into(),
                    present: true,
                    expected_revision: "a".into(),
                    installed_revision: Some("a".into()),
                    integrity_error: None,
                },
                DiagnosticModel {
                    id: "whisper".into(),
                    present: false,
                    expected_revision: "b".into(),
                    installed_revision: None,
                    integrity_error: None,
                },
            ],
            gpu: DiagnosticGpu {
                driver_present: false,
                cuda_ready: false,
                missing_dlls: vec![],
                managed_libraries_present: false,
                managed_libraries_integrity_error: None,
            },
            report: String::new(),
        }
    }

    #[test]
    fn diagnostics_rows() {
        let mut d = diagnostics();
        assert_eq!(recording_text(&d), "idle");
        assert_eq!(processing_text(&d), "idle · 0 queued");
        assert_eq!(capture_health_text(&d), "not available in this session");
        assert_eq!(model_integrity_text(&d), "1/2 verified");
        assert_eq!(gpu_runtime_text(&d), "CPU fallback · no NVIDIA driver");

        d.recording_state = "recording".into();
        d.recording_meeting_id = Some(12);
        d.processing_meeting_id = Some(11);
        d.processing_pct = Some(42.25);
        d.queue_len = 2;
        d.capture_health = Some(RecordingHealth {
            mic: TrackHealth {
                connected: true,
                capture_dropped_ms: 40,
                ..Default::default()
            },
            loopback: TrackHealth::default(),
            writer_error: None,
        });
        d.capture_health_is_current = true;
        d.models[1].integrity_error = Some("bad".into());
        assert_eq!(model_integrity_text(&d), "1/2 verified", "never installed");
        d.models[1].installed_revision = Some("old".into());
        d.gpu.driver_present = true;
        d.gpu.managed_libraries_present = true;
        assert_eq!(recording_text(&d), "recording · meeting 12");
        assert_eq!(
            processing_text(&d),
            "meeting 11 · starting · 42.2% · 2 queued"
        );
        assert_eq!(
            capture_health_text(&d),
            "current: mic connected, 40 ms dropped · output disconnected, 0 ms dropped"
        );
        assert_eq!(
            model_integrity_text(&d),
            "1/2 verified · one or more checks failed"
        );
        assert_eq!(
            gpu_runtime_text(&d),
            "CPU fallback · missing runtime libraries · managed libraries verified"
        );
        d.gpu.missing_dlls = vec!["cudnn64_9.dll".into(), "cufft64_12.dll".into()];
        d.gpu.managed_libraries_integrity_error = Some("hash".into());
        assert_eq!(
            gpu_runtime_text(&d),
            "CPU fallback · missing cudnn64_9.dll, cufft64_12.dll · managed install check failed: hash"
        );
        d.gpu.cuda_ready = true;
        d.gpu.managed_libraries_integrity_error = None;
        assert_eq!(
            gpu_runtime_text(&d),
            "CUDA runtime ready · managed libraries verified"
        );
    }
}
