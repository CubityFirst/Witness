//! Settings → Recording: meeting detection (auto-record, watch patterns),
//! the two capture devices and the global hotkeys. The device rows are
//! shared with the first-run setup.

use super::{
    badge_or_text, footnote, keycap, note, row, save, section, text_width, title, toggle,
    SettingsState, Slot, FIELD_WIDTH,
};
use crate::ui::theme::{self, pal};
use crate::ui::widgets::{self, BadgeKind};
use crate::ui::UiCtx;
use eframe::egui::{self, RichText};
use witness_core::audio_capture::AudioDevice;
use witness_core::events::WatcherStatus;

/// Under the hotkey rows.
const HOTKEYS_HELP: &str = "They work in any app, even with Witness in the tray. If another app owns a combination, the next free one is used.";

impl SettingsState {
    /// "Auto-record on · Headset Microphone".
    pub(super) fn recording_summary(&self, cx: &UiCtx) -> String {
        let settings = cx.settings.current();
        recording_summary(
            settings.auto_record,
            settings.mic_device.as_deref(),
            self.devices.as_ref().map(|d| d.capture.as_slice()),
        )
    }

    pub(super) fn recording_page(&mut self, ui: &mut egui::Ui, cx: &mut UiCtx) {
        let settings = cx.settings.current().clone();
        let watcher = watcher_text(cx.shell.status.as_ref().map(|s| &s.watcher));
        let record_hotkey = cx.shell.record_hotkey.clone();
        let bookmark_hotkey = cx.shell.bookmark_hotkey.clone();
        let mut auto_record = settings.auto_record;
        let failed = self.devices_error.is_some();
        let devices = self.devices.as_ref();
        let devices_error = self.devices_error.as_deref();
        let patterns_text = &mut self.patterns_text;

        let (auto, patterns) = section(ui, "Meeting detection", |ui| {
            let auto = toggle(
                ui,
                true,
                &mut auto_record,
                "Auto-record",
                "Records while a watched app is using the microphone.",
            )
            .changed();
            widgets::group_divider(ui);
            let patterns = row(
                ui,
                "watch-patterns",
                Slot::Fill(FIELD_WIDTH),
                |ui| {
                    let id = title(ui, "Watch patterns");
                    note(
                        ui,
                        "Comma-separated. An app counts if its name or path contains one.",
                    );
                    id
                },
                |ui, label| {
                    let field = widgets::text_input(
                        ui,
                        patterns_id(),
                        egui::TextEdit::singleline(patterns_text).desired_width(f32::INFINITY),
                    );
                    let response = ui.add(field).labelled_by(label);
                    widgets::text_edit_menu(&response);
                    response
                },
            );
            (auto, patterns)
        });
        footnote(ui, &watcher);

        let (mic, loopback, refresh) = section(ui, "Audio devices", |ui| {
            let mic = device_row(
                ui,
                &DeviceRow {
                    label: "Microphone",
                    id: "mic-device",
                    default_label: "Default input device",
                    missing_badge: "configured microphone is not connected",
                    help: Some("Recorded as you."),
                    configured: settings.mic_device.as_deref(),
                    devices: devices.map(|d| d.capture.as_slice()),
                    list_failed: failed,
                },
            );
            widgets::group_divider(ui);
            let loopback = device_row(
                ui,
                &DeviceRow {
                    label: "Meeting audio",
                    id: "loopback-device",
                    default_label: "Default output device",
                    missing_badge: "configured output is not connected",
                    help: Some(
                        "Recorded as everyone else: pick the output Teams plays through. A dedicated one (e.g. \"Chat\") keeps music and game audio out.",
                    ),
                    configured: settings.loopback_device.as_deref(),
                    devices: devices.map(|d| d.render.as_slice()),
                    list_failed: failed,
                },
            );
            if let Some(error) = devices_error {
                widgets::inline_warning(ui, &format!("Audio devices could not be listed: {error}"));
            }
            widgets::group_divider(ui);
            let refresh = row(
                ui,
                "refresh-devices",
                Slot::Fit,
                |ui| {
                    let id = title(ui, "Missing a device?");
                    note(ui, "Re-scan after plugging one in.");
                    id
                },
                |ui, _| widgets::btn(ui, "Refresh devices").clicked(),
            );
            (mic, loopback, refresh)
        });

        section(ui, "Global hotkeys", |ui| {
            hotkey_row(
                ui,
                "Recording hotkey",
                "Starts or stops recording.",
                record_hotkey.as_deref(),
            );
            widgets::group_divider(ui);
            hotkey_row(
                ui,
                "Bookmark hotkey",
                "Marks the current moment of a recording.",
                bookmark_hotkey.as_deref(),
            );
        });
        footnote(ui, HOTKEYS_HELP);

        if auto {
            save(cx, |s| s.auto_record = auto_record);
        }
        self.patterns_dirty |= patterns.changed();
        // Enter surrenders focus too, so this covers blur and Enter. A clean
        // buffer was already committed (or re-synced): skipping it keeps a
        // draft that `flush` committed — and the store reverted — from
        // failing a second time when the field finally blurs.
        if patterns.lost_focus() && self.patterns_dirty {
            self.commit_patterns(cx, parse_patterns(&self.patterns_text));
        }
        if let Some(value) = mic {
            save(cx, |s| s.mic_device = value);
        }
        if let Some(value) = loopback {
            save(cx, |s| s.loopback_device = value);
        }
        if refresh {
            self.refresh_devices(cx);
        }
    }

    pub(super) fn commit_patterns(&mut self, cx: &mut UiCtx, patterns: Vec<String>) {
        self.patterns_dirty = false;
        self.patterns_source = patterns.clone();
        if patterns != cx.settings.current().watch_patterns {
            save(cx, |s| s.watch_patterns = patterns);
        }
    }
}

pub(super) fn patterns_id() -> egui::Id {
    egui::Id::new("settings-watch-patterns")
}

/// One audio-device picker row (settings.tsx's two `<select>`s).
pub(crate) struct DeviceRow<'a> {
    pub label: &'static str,
    pub id: &'static str,
    pub default_label: &'static str,
    pub missing_badge: &'static str,
    pub help: Option<&'static str>,
    pub configured: Option<&'a str>,
    /// None while loading or after listing failed.
    pub devices: Option<&'a [AudioDevice]>,
    pub list_failed: bool,
}

/// A device picker row; returns the newly picked value (None inside =
/// the default device).
pub(crate) fn device_row(ui: &mut egui::Ui, spec: &DeviceRow) -> Option<Option<String>> {
    let options = device_options(
        spec.configured,
        spec.devices,
        spec.list_failed,
        spec.default_label,
    );
    let kind = configured_device(spec.configured, spec.devices.unwrap_or(&[]));
    let selected = spec.configured.unwrap_or("");
    let picked = row(
        ui,
        spec.id,
        Slot::Fill(FIELD_WIDTH),
        |ui| {
            let id = title(ui, spec.label);
            if let Some(help) = spec.help {
                note(ui, help);
            }
            if spec.devices.is_some() {
                match kind {
                    Some(ConfiguredDevice::Missing) => {
                        ui.add_space(theme::S1);
                        badge_or_text(ui, BadgeKind::Failed, spec.missing_badge);
                    }
                    Some(ConfiguredDevice::Legacy(_)) => note(
                        ui,
                        "Stored by its old friendly name; reselect it to use the stable ID.",
                    ),
                    _ => {}
                }
            }
            id
        },
        |ui, label| device_picker(ui, spec.id, label, &options, selected),
    );
    picked.map(|value| (!value.is_empty()).then_some(value))
}

/// A global-hotkey row: the bound combo as a key cap, or "Unavailable".
fn hotkey_row(ui: &mut egui::Ui, name: &str, what: &str, combo: Option<&str>) {
    let (description, key) = hotkey_parts(what, combo);
    row(
        ui,
        name,
        Slot::Fit,
        |ui| {
            let id = title(ui, name);
            note(ui, &description);
            id
        },
        |ui, _| match &key {
            Some(key) => keycap(ui, key),
            None => {
                ui.label(RichText::new("Unavailable").color(pal().text_muted));
            }
        },
    );
}

/// A device `<select>` named by the row title `label_id`, as wide as the
/// slot; returns the newly picked value ("" = default).
fn device_picker(
    ui: &mut egui::Ui,
    id: &str,
    label_id: egui::Id,
    options: &[DeviceOption],
    selected: &str,
) -> Option<String> {
    let label = options
        .iter()
        .find(|o| o.value == selected)
        .map_or(selected, |o| o.label.as_str());
    let width = ui.available_width();
    // The button plus its arrow need ~40 pt besides the text.
    let truncated = text_width(ui, label, theme::regular(theme::BASE_SIZE)) + 40.0 > width;
    let mut picked = None;
    widgets::select(ui, widgets::SelectKind::TextInput, |ui| {
        let response = egui::ComboBox::from_id_salt(id)
            .width(width)
            .selected_text(label)
            .truncate()
            .show_ui(ui, |ui| {
                for option in options {
                    let current = option.value == selected;
                    if ui
                        .selectable_label(current, option.label.as_str())
                        .clicked()
                        && !current
                    {
                        picked = Some(option.value.clone());
                    }
                }
            })
            .response
            .labelled_by(label_id);
        if truncated {
            response.on_hover_text(label);
        }
    });
    picked
}

/// Watch patterns: split on `,`, trim, drop empties.
pub(super) fn parse_patterns(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

/// `ctrl+alt+r` → `CTRL + ALT + R`.
pub(crate) fn hotkey_label(combo: &str) -> String {
    combo.to_uppercase().replace('+', " + ")
}

/// A global-hotkey row's description and key cap: `what` it does and the
/// bound combo, or why there is none.
fn hotkey_parts(what: &str, combo: Option<&str>) -> (String, Option<String>) {
    match combo {
        Some(combo) => (what.to_string(), Some(hotkey_label(combo))),
        None => (
            format!("{what} Every combination is taken by another app."),
            None,
        ),
    }
}

fn watcher_text(watcher: Option<&WatcherStatus>) -> String {
    match watcher {
        Some(w) => format!(
            "Watcher: Teams key found: {} · mic in use: {}{}",
            yes_no(w.teams_key_found),
            yes_no(w.mic_in_use),
            if w.suppressed {
                " · auto-restart suppressed"
            } else {
                ""
            }
        ),
        None => "Watcher: …".into(),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// How a configured device value relates to the listed endpoints.
#[derive(Debug, PartialEq)]
enum ConfiguredDevice<'a> {
    /// Matched by endpoint ID.
    Id,
    /// Stored by its friendly name (older settings).
    Legacy(&'a AudioDevice),
    Missing,
}

fn configured_device<'a>(
    configured: Option<&str>,
    devices: &'a [AudioDevice],
) -> Option<ConfiguredDevice<'a>> {
    let configured = configured.filter(|c| !c.is_empty())?;
    if devices.iter().any(|d| d.id == configured) {
        return Some(ConfiguredDevice::Id);
    }
    let lower = configured.to_lowercase();
    Some(
        match devices.iter().find(|d| d.name.to_lowercase() == lower) {
            Some(device) => ConfiguredDevice::Legacy(device),
            None => ConfiguredDevice::Missing,
        },
    )
}

#[derive(Debug, Clone, PartialEq)]
struct DeviceOption {
    value: String,
    label: String,
}

/// The picker's options: the default, a synthetic entry for a configured
/// value that no endpoint ID matches, then every device.
fn device_options(
    configured: Option<&str>,
    devices: Option<&[AudioDevice]>,
    list_failed: bool,
    default_label: &str,
) -> Vec<DeviceOption> {
    let mut options = vec![DeviceOption {
        value: String::new(),
        label: default_label.into(),
    }];
    let list = devices.unwrap_or(&[]);
    if let Some(value) = configured.filter(|c| !c.is_empty()) {
        let label = match configured_device(Some(value), list) {
            Some(ConfiguredDevice::Id) | None => None,
            Some(ConfiguredDevice::Legacy(device)) => Some(format!(
                "{} (old name; reselect it to pin this device)",
                device.name
            )),
            Some(ConfiguredDevice::Missing) => Some(if devices.is_some() {
                format!("Not connected: {value}")
            } else if list_failed {
                "Configured device (device list unavailable)".into()
            } else {
                "Configured device (checking…)".into()
            }),
        };
        if let Some(label) = label {
            options.push(DeviceOption {
                value: value.into(),
                label,
            });
        }
    }
    options.extend(list.iter().map(|d| DeviceOption {
        value: d.id.clone(),
        label: d.name.clone(),
    }));
    options
}

/// The Recording category's summary: auto-record and the microphone.
fn recording_summary(
    auto_record: bool,
    mic: Option<&str>,
    devices: Option<&[AudioDevice]>,
) -> String {
    let auto = if auto_record {
        "Auto-record on"
    } else {
        "Auto-record off"
    };
    let mic = match (configured_device(mic, devices.unwrap_or(&[])), devices) {
        (None, _) => "default microphone".to_string(),
        (Some(ConfiguredDevice::Legacy(device)), _) => device.name.clone(),
        (Some(ConfiguredDevice::Id), Some(list)) => list
            .iter()
            .find(|d| Some(d.id.as_str()) == mic)
            .map_or_else(|| "chosen microphone".into(), |d| d.name.clone()),
        (Some(_), None) => "chosen microphone".to_string(),
        (Some(ConfiguredDevice::Missing), Some(_)) => "microphone not connected".to_string(),
    };
    format!("{auto} · {mic}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::views::settings::{
        kv_row, path_box, path_value, progress_bar, run_at, section, warning_strip,
    };

    fn device(id: &str, name: &str) -> AudioDevice {
        AudioDevice {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn text_fields_parse_like_settings_tsx() {
        assert_eq!(
            parse_patterns(" MSTeams , ,Zoom,"),
            vec!["MSTeams".to_string(), "Zoom".to_string()]
        );
        assert!(parse_patterns("  ,  ").is_empty());
    }

    #[test]
    fn hotkey_rows() {
        assert_eq!(hotkey_label("ctrl+alt+r"), "CTRL + ALT + R");
        assert_eq!(
            hotkey_parts("Starts or stops recording.", Some("ctrl+alt+r")),
            (
                "Starts or stops recording.".to_string(),
                Some("CTRL + ALT + R".to_string())
            )
        );
        assert_eq!(
            hotkey_parts("Marks the current moment of a recording.", None),
            (
                "Marks the current moment of a recording. Every combination is taken by another app."
                    .to_string(),
                None
            )
        );
    }

    #[test]
    fn watcher_row() {
        assert_eq!(watcher_text(None), "Watcher: …");
        let w = WatcherStatus {
            enabled: true,
            teams_key_found: true,
            mic_in_use: false,
            suppressed: true,
        };
        assert_eq!(
            watcher_text(Some(&w)),
            "Watcher: Teams key found: yes · mic in use: no · auto-restart suppressed"
        );
    }

    #[test]
    fn device_options_cover_legacy_and_missing_values() {
        let list = [
            device("{0.0.1}.{aaa}", "Chat (GoXLR)"),
            device("{0.0.1}.{bbb}", "Speakers"),
        ];
        let labels = |o: Vec<DeviceOption>| o.into_iter().map(|o| o.label).collect::<Vec<_>>();

        // Not configured: default + devices.
        assert_eq!(
            labels(device_options(
                None,
                Some(&list),
                false,
                "Default output device"
            )),
            vec!["Default output device", "Chat (GoXLR)", "Speakers"]
        );
        // Matched by ID: no synthetic option.
        assert_eq!(
            device_options(Some("{0.0.1}.{bbb}"), Some(&list), false, "D").len(),
            3
        );
        // Legacy friendly name (case-insensitive).
        let legacy = device_options(Some("chat (goxlr)"), Some(&list), false, "D");
        assert_eq!(
            legacy[1],
            DeviceOption {
                value: "chat (goxlr)".into(),
                label: "Chat (GoXLR) (old name; reselect it to pin this device)".into()
            }
        );
        assert!(matches!(
            configured_device(Some("chat (goxlr)"), &list),
            Some(ConfiguredDevice::Legacy(_))
        ));
        // Missing once loaded / while loading / when listing failed.
        assert_eq!(
            device_options(Some("Old headset"), Some(&list), false, "D")[1].label,
            "Not connected: Old headset"
        );
        assert_eq!(
            device_options(Some("Old headset"), None, false, "D")[1].label,
            "Configured device (checking…)"
        );
        assert_eq!(
            device_options(Some("Old headset"), None, true, "D")[1].label,
            "Configured device (device list unavailable)"
        );
        assert_eq!(
            configured_device(Some("Old headset"), &list),
            Some(ConfiguredDevice::Missing)
        );
        assert_eq!(configured_device(None, &list), None);
    }

    #[test]
    fn summary_names_the_microphone() {
        let list = [device("{a}", "Headset Microphone")];
        assert_eq!(
            recording_summary(true, None, Some(&list)),
            "Auto-record on · default microphone"
        );
        assert_eq!(
            recording_summary(false, Some("{a}"), Some(&list)),
            "Auto-record off · Headset Microphone"
        );
        assert_eq!(
            recording_summary(true, Some("headset microphone"), Some(&list)),
            "Auto-record on · Headset Microphone"
        );
        assert_eq!(
            recording_summary(true, Some("{gone}"), Some(&list)),
            "Auto-record on · microphone not connected"
        );
        assert_eq!(
            recording_summary(true, Some("{a}"), None),
            "Auto-record on · chosen microphone"
        );
    }

    /// The section helpers lay out in a themed context without panicking.
    #[test]
    fn layout_helpers_render() {
        run_at(460.0, |ui| {
            section(ui, "Models", |ui| {
                row(
                    ui,
                    "test-row",
                    Slot::Fill(FIELD_WIDTH),
                    |ui| {
                        let id = title(ui, "Parakeet TDT 0.6B v3 (ONNX)");
                        note(ui, "muted");
                        path_box(ui, "C:\\Users\\me\\AppData\\Local\\Witness");
                        progress_bar(ui, 0.4);
                        id
                    },
                    |ui, label| {
                        let options = device_options(None, Some(&[]), false, "Default");
                        assert_eq!(device_picker(ui, "test-picker", label, &options, ""), None);
                    },
                );
                widgets::group_divider(ui);
                assert!(!warning_strip(
                    ui,
                    "model not downloaded, so transcription will fail",
                    "Download",
                    true
                ));
                kv_row(ui, "Database", |ui| {
                    path_value(ui, "C:\\data\\witness.db", Some((BadgeKind::Ok, "healthy")));
                });
                hotkey_row(
                    ui,
                    "Recording hotkey",
                    "Starts or stops recording.",
                    Some("ctrl+alt+r"),
                );
                keycap(ui, "CTRL + ALT + B");
            });
            footnote(ui, "paragraph");
        });
    }
}
