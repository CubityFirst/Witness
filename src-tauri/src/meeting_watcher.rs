//! Detects Teams (or any watched app) meetings by polling the Windows
//! Capability Access Manager consent store: an app whose `LastUsedTimeStart`
//! is non-zero with `LastUsedTimeStop == 0` currently holds the microphone.
//! Teams keeps the mic open for the whole call (even muted), so this tracks
//! meeting membership exactly.

use crate::events::WatcherStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use winreg::enums::HKEY_CURRENT_USER;
#[cfg(windows)]
use winreg::RegKey;

const CONSENT_STORE_MIC: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Consecutive "active" ticks before starting (~4 s) — filters the brief mic
/// grab of Teams' settings-page mic test.
const START_TICKS: u32 = 2;
/// Consecutive "inactive" ticks before stopping (~10 s) — survives a quick
/// drop/rejoin.
const STOP_TICKS: u32 = 5;

/// Commands the watcher sends to the app.
#[derive(Debug)]
pub enum WatcherCommand {
    StartMeeting,
    StopMeeting,
    Status(WatcherStatus),
}

/// Shared knobs the app can flip at runtime.
pub struct WatcherControl {
    /// Auto-record enabled (mirrors settings; updated by update_settings).
    pub enabled: AtomicBool,
    /// Set when the user manually stopped during a call: suppresses
    /// auto-restart until the mic goes idle again.
    pub suppressed: AtomicBool,
    /// Lowercased patterns matched against consent-store subkey names.
    pub patterns: Mutex<Vec<String>>,
    /// Teams window handles seen while no meeting was active — lets meeting
    /// naming tell the call window (spawned with the call) from the main app
    /// window, whose title is just whatever chat/tab is open.
    pub idle_teams_windows: Mutex<Vec<isize>>,
}

impl WatcherControl {
    pub fn new(enabled: bool, patterns: &[String]) -> Arc<Self> {
        Arc::new(WatcherControl {
            enabled: AtomicBool::new(enabled),
            suppressed: AtomicBool::new(false),
            patterns: Mutex::new(patterns.iter().map(|p| p.to_lowercase()).collect()),
            idle_teams_windows: Mutex::new(Vec::new()),
        })
    }

    pub fn set_patterns(&self, patterns: &[String]) {
        *self.patterns.lock().unwrap() = patterns.iter().map(|p| p.to_lowercase()).collect();
    }
}

/// One registry scan: does any watched app currently hold the mic, and did we
/// see a watched key at all?
#[cfg(windows)]
fn scan(patterns: &[String]) -> (bool, bool) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(root) = hkcu.open_subkey(CONSENT_STORE_MIC) else {
        return (false, false);
    };
    let mut key_found = false;
    let mut active = false;

    let mut check_key = |container: &RegKey, name: &str| {
        let lower = name.to_lowercase();
        if !patterns.iter().any(|p| lower.contains(p.as_str())) {
            return;
        }
        key_found = true;
        if let Ok(sub) = container.open_subkey(name) {
            let start: u64 = sub.get_value("LastUsedTimeStart").unwrap_or(0u64);
            let stop: u64 = sub.get_value("LastUsedTimeStop").unwrap_or(0u64);
            if start != 0 && stop == 0 {
                active = true;
            }
        }
    };

    // MSIX-packaged apps (new Teams: MSTeams_8wekyb3d8bbwe) are direct subkeys.
    for name in root.enum_keys().flatten() {
        if name != "NonPackaged" {
            check_key(&root, &name);
        }
    }
    // Classic apps live under NonPackaged with exe paths ('\' encoded as '#').
    if let Ok(nonpackaged) = root.open_subkey("NonPackaged") {
        for name in nonpackaged.enum_keys().flatten() {
            check_key(&nonpackaged, &name);
        }
    }
    (active, key_found)
}

#[cfg(not(windows))]
fn scan(_patterns: &[String]) -> (bool, bool) {
    (false, false)
}

/// Spawns the polling thread. `send` delivers commands to the app; returning
/// `false` from it stops the thread (channel closed on shutdown).
pub fn spawn(
    control: Arc<WatcherControl>,
    send: impl Fn(WatcherCommand) -> bool + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("meeting-watcher".into())
        .spawn(move || {
            let mut active_ticks = 0u32;
            let mut inactive_ticks = 0u32;
            let mut in_meeting = false;
            let mut last_status: Option<WatcherStatus> = None;

            loop {
                let patterns = control.patterns.lock().unwrap().clone();
                let (mic_active, key_found) = scan(&patterns);
                let enabled = control.enabled.load(Ordering::Relaxed);

                if mic_active {
                    active_ticks += 1;
                    inactive_ticks = 0;
                } else {
                    inactive_ticks += 1;
                    active_ticks = 0;
                }

                // While idle, keep a fresh baseline of Teams windows so that
                // once a call starts, naming can prefer the windows the call
                // spawned. Frozen from the first active tick onward.
                if !in_meeting && !mic_active {
                    *control.idle_teams_windows.lock().unwrap() =
                        crate::meeting_title::teams_window_handles();
                }

                if !in_meeting && mic_active && active_ticks >= START_TICKS {
                    in_meeting = true;
                    if enabled && !control.suppressed.load(Ordering::Relaxed) {
                        if !send(WatcherCommand::StartMeeting) {
                            return;
                        }
                    }
                } else if in_meeting && !mic_active && inactive_ticks >= STOP_TICKS {
                    in_meeting = false;
                    // Meeting over: lift any manual-stop suppression.
                    control.suppressed.store(false, Ordering::Relaxed);
                    if !send(WatcherCommand::StopMeeting) {
                        return;
                    }
                }

                let status = WatcherStatus {
                    enabled,
                    teams_key_found: key_found,
                    mic_in_use: mic_active,
                    suppressed: control.suppressed.load(Ordering::Relaxed),
                };
                let changed = last_status.as_ref().map_or(true, |s| {
                    s.enabled != status.enabled
                        || s.teams_key_found != status.teams_key_found
                        || s.mic_in_use != status.mic_in_use
                        || s.suppressed != status.suppressed
                });
                if changed {
                    last_status = Some(status.clone());
                    if !send(WatcherCommand::Status(status)) {
                        return;
                    }
                }

                std::thread::sleep(POLL_INTERVAL);
            }
        })
        .expect("spawn meeting-watcher thread")
}
