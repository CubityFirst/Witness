//! Detects Teams (or any watched app) meetings by polling two independent
//! signals that a watched app holds the microphone:
//!
//! - the Windows Capability Access Manager consent store: an app whose
//!   `LastUsedTimeStart` is non-zero with `LastUsedTimeStop == 0`;
//! - the capture endpoints' WASAPI audio sessions: a running (`Active`)
//!   session owned by a watched process (exe path or MSIX package).
//!
//! Either one counts. The consent store alone proved fragile — Windows can
//! stop updating it for every app (seen after a restart on a 26H2 build), and
//! every call then went unrecorded. Teams keeps the mic open for the whole
//! call (even muted), so this tracks meeting membership exactly.

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
/// Consecutive "active" ticks (~10 s) after which a signal that never saw the
/// call is reported as missing it.
const CROSS_CHECK_TICKS: u32 = 5;

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
    /// An exit path suspended auto-record for the rest of this process.
    /// Sticky: unlike `enabled`, no settings write can clear it.
    pub exiting: AtomicBool,
    /// Set when the user manually stopped during a call: suppresses
    /// auto-restart until the mic goes idle again.
    pub suppressed: AtomicBool,
    /// Lowercased patterns matched against consent-store subkey names and
    /// capture-session owners (exe path, '#'-encoded path, package family).
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
            exiting: AtomicBool::new(false),
            suppressed: AtomicBool::new(false),
            patterns: Mutex::new(patterns.iter().map(|p| p.to_lowercase()).collect()),
            idle_teams_windows: Mutex::new(Vec::new()),
        })
    }

    /// Auto-record may start a recording: enabled in settings and not
    /// suspended by an exit path.
    pub fn auto_record(&self) -> bool {
        self.enabled.load(Ordering::Relaxed) && !self.exiting()
    }

    /// An exit path has suspended auto-record for good.
    pub fn exiting(&self) -> bool {
        self.exiting.load(Ordering::SeqCst)
    }

    pub fn set_patterns(&self, patterns: &[String]) {
        *self.patterns.lock().unwrap() = patterns.iter().map(|p| p.to_lowercase()).collect();
    }
}

fn matches(patterns: &[String], name: &str) -> bool {
    let lower = name.to_lowercase();
    patterns.iter().any(|p| lower.contains(p.as_str()))
}

/// Does a capture session's owner match a watched pattern? Checked against
/// the same names the consent store would key it by: the exe path (also
/// '#'-encoded, as under `NonPackaged`) and the MSIX package family.
fn owner_matches(patterns: &[String], exe: &str, package: &str) -> bool {
    matches(patterns, exe)
        || matches(patterns, &exe.replace('\\', "#"))
        || (!package.is_empty() && matches(patterns, package))
}

/// One registry scan: does any watched app currently hold the mic, and did we
/// see a watched key at all?
#[cfg(windows)]
fn scan_registry(patterns: &[String]) -> (bool, bool) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(root) = hkcu.open_subkey(CONSENT_STORE_MIC) else {
        return (false, false);
    };
    let mut key_found = false;
    let mut active = false;

    let mut check_key = |container: &RegKey, name: &str| {
        if !matches(patterns, name) {
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
fn scan_registry(_patterns: &[String]) -> (bool, bool) {
    (false, false)
}

/// One audio session on a capture endpoint.
#[cfg(windows)]
#[derive(Debug)]
struct CaptureSession {
    pid: u32,
    /// At least one stream in the session is running.
    active: bool,
}

/// Every per-process session on every active capture endpoint. Sessions
/// outlive their streams (Inactive) for as long as the process runs.
#[cfg(windows)]
fn capture_sessions() -> Result<Vec<CaptureSession>, wasapi::WasapiError> {
    let devices =
        wasapi::DeviceEnumerator::new()?.get_device_collection(&wasapi::Direction::Capture)?;
    let mut out = Vec::new();
    // By index: the crate's collection iterator unwraps its count.
    for index in 0..devices.get_nbr_devices()? {
        // A device unplugged mid-scan must not hide the others.
        let Ok(manager) = devices
            .get_device_at_index(index)
            .and_then(|device| device.get_iaudiosessionmanager())
        else {
            continue;
        };
        let Ok(sessions) = manager.get_audiosessionenumerator() else {
            continue;
        };
        for i in 0..sessions.get_count().unwrap_or(0) {
            let Ok(session) = sessions.get_session(i) else {
                continue;
            };
            // Cross-process sessions (system sounds) have no single owner (0).
            let pid = session.get_process_id().unwrap_or(0);
            if pid == 0 {
                continue;
            }
            let active = matches!(session.get_state(), Ok(wasapi::SessionState::Active));
            out.push(CaptureSession { pid, active });
        }
    }
    Ok(out)
}

/// A process's full exe path and MSIX package family name ('' when the
/// process is unpackaged). `None` if the process can't be opened (exited).
#[cfg(windows)]
fn process_identity(pid: u32) -> Option<(String, String)> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
    use windows::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut exe_buf = vec![0u16; 1024];
        let mut exe_len = exe_buf.len() as u32;
        let exe = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(exe_buf.as_mut_ptr()),
            &mut exe_len,
        )
        .ok()
        .map(|_| String::from_utf16_lossy(&exe_buf[..exe_len as usize]));
        let mut pkg_buf = vec![0u16; 256];
        let mut pkg_len = pkg_buf.len() as u32;
        let package = if GetPackageFamilyName(
            handle,
            &mut pkg_len,
            Some(windows::core::PWSTR(pkg_buf.as_mut_ptr())),
        ) == ERROR_SUCCESS
        {
            // The length includes the terminating NUL.
            String::from_utf16_lossy(&pkg_buf[..pkg_len.saturating_sub(1) as usize])
        } else {
            String::new()
        };
        let _ = CloseHandle(handle);
        Some((exe.unwrap_or_default(), package))
    }
}

/// One audio-session scan: does a watched process have a running capture
/// stream? Only active sessions' owners are looked up.
#[cfg(windows)]
fn scan_sessions(patterns: &[String]) -> Result<bool, wasapi::WasapiError> {
    let own_pid = std::process::id();
    Ok(capture_sessions()?.iter().any(|s| {
        s.active
            && s.pid != own_pid
            && process_identity(s.pid)
                .is_some_and(|(exe, package)| owner_matches(patterns, &exe, &package))
    }))
}

#[cfg(not(windows))]
fn scan_sessions(_patterns: &[String]) -> Result<bool, String> {
    Ok(false)
}

/// Per-activity-run bookkeeping for the two signals: which of them has seen
/// the current run, and whether the consent store is known to be missing calls.
#[derive(Debug, Default)]
struct CrossCheck {
    registry_seen: bool,
    sessions_seen: bool,
    /// Sticky until the consent store reports mic use again.
    consent_store_stale: bool,
}

impl CrossCheck {
    /// Feed one tick; `active_ticks` is the current run length (0 = idle).
    fn tick(&mut self, registry: bool, sessions: bool, active_ticks: u32) {
        if active_ticks == 0 {
            self.registry_seen = false;
            self.sessions_seen = false;
        } else {
            self.registry_seen |= registry;
            self.sessions_seen |= sessions;
        }
        if registry {
            if self.consent_store_stale {
                log::info!("watcher: Windows' mic consent store is reporting mic use again");
            }
            self.consent_store_stale = false;
        }
        if active_ticks != CROSS_CHECK_TICKS {
            return;
        }
        if !self.registry_seen && !self.consent_store_stale {
            self.consent_store_stale = true;
            log::warn!(
                "watcher: a watched app is capturing audio but Windows' mic consent store \
                 does not show it (its usage tracking appears stalled); detecting calls \
                 through audio sessions"
            );
        }
        if !self.sessions_seen {
            log::info!(
                "watcher: the consent store shows a watched app using the mic, but no \
                 matching capture session is running"
            );
        }
    }
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
            // Audio-session enumeration is COM; WASAPI everywhere here is MTA.
            #[cfg(windows)]
            let _ = wasapi::initialize_mta();
            let mut active_ticks = 0u32;
            let mut inactive_ticks = 0u32;
            let mut in_meeting = false;
            let mut last_status: Option<WatcherStatus> = None;
            let mut cross_check = CrossCheck::default();
            let mut session_error_logged = false;

            loop {
                let patterns = control.patterns.lock().unwrap().clone();
                let (registry_active, key_found) = scan_registry(&patterns);
                let session_active = match scan_sessions(&patterns) {
                    Ok(active) => {
                        session_error_logged = false;
                        active
                    }
                    Err(e) => {
                        if !session_error_logged {
                            session_error_logged = true;
                            log::warn!("watcher: audio session scan failed: {e}");
                        }
                        false
                    }
                };
                let mic_active = registry_active || session_active;
                let enabled = control.auto_record();

                if mic_active {
                    active_ticks += 1;
                    inactive_ticks = 0;
                } else {
                    inactive_ticks += 1;
                    active_ticks = 0;
                }
                cross_check.tick(registry_active, session_active, active_ticks);

                // While idle, keep a fresh baseline of Teams windows so that
                // once a call starts, naming can prefer the windows the call
                // spawned. Frozen from the first active tick onward.
                if !in_meeting && !mic_active {
                    *control.idle_teams_windows.lock().unwrap() =
                        crate::meeting_title::teams_window_handles();
                }

                if !in_meeting && mic_active && active_ticks >= START_TICKS {
                    in_meeting = true;
                    if enabled
                        && !control.suppressed.load(Ordering::Relaxed)
                        && !send(WatcherCommand::StartMeeting)
                    {
                        return;
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
                    consent_store_stale: cross_check.consent_store_stale,
                };
                let changed = last_status.as_ref().is_none_or(|s| {
                    s.enabled != status.enabled
                        || s.teams_key_found != status.teams_key_found
                        || s.mic_in_use != status.mic_in_use
                        || s.suppressed != status.suppressed
                        || s.consent_store_stale != status.consent_store_stale
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

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_lowercase()).collect()
    }

    #[test]
    fn owner_matches_exe_path_encoded_path_and_package() {
        let teams = patterns(&["MSTeams"]);
        // New Teams: the WindowsApps install dir carries the package name.
        assert!(owner_matches(
            &teams,
            r"C:\Program Files\WindowsApps\MSTeams_26260.1717.5229.7049_x64__8wekyb3d8bbwe\ms-teams.exe",
            "",
        ));
        // A packaged helper outside the install dir matches by package.
        assert!(owner_matches(
            &teams,
            r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application\141.0\msedgewebview2.exe",
            "MSTeams_8wekyb3d8bbwe",
        ));
        assert!(!owner_matches(
            &teams,
            r"C:\Users\Admin\AppData\Local\Discord\app-1.0.9265\Discord.exe",
            "",
        ));
        // Patterns written against NonPackaged consent keys ('\' as '#').
        let classic = patterns(&["#Teams#current#Teams.exe"]);
        assert!(owner_matches(
            &classic,
            r"C:\Users\Admin\AppData\Local\Microsoft\Teams\current\Teams.exe",
            "",
        ));
    }

    #[test]
    fn sessions_only_call_marks_the_consent_store_stale_until_it_reports_again() {
        let mut check = CrossCheck::default();
        for run in 1..=CROSS_CHECK_TICKS + 3 {
            check.tick(false, true, run);
        }
        assert!(check.consent_store_stale);
        // Idle ticks keep the flag: it describes the store, not the call.
        check.tick(false, false, 0);
        assert!(check.consent_store_stale);
        check.tick(true, true, 1);
        assert!(!check.consent_store_stale);
    }

    #[test]
    fn consent_store_seen_once_in_a_run_is_not_stale() {
        let mut check = CrossCheck::default();
        // Registry reports a tick late, then keeps up.
        check.tick(false, true, 1);
        for run in 2..=CROSS_CHECK_TICKS + 2 {
            check.tick(true, true, run);
        }
        assert!(!check.consent_store_stale);
        // A short session-only blip (mic test) never reaches the check.
        let mut check = CrossCheck::default();
        for run in 1..CROSS_CHECK_TICKS {
            check.tick(false, true, run);
        }
        check.tick(false, false, 0);
        assert!(!check.consent_store_stale);
    }

    /// Lists every per-process session on the real capture endpoints with
    /// its owner — shows which process a call's mic stream belongs to.
    #[cfg(windows)]
    #[test]
    #[ignore = "reads the real audio endpoints"]
    fn list_capture_sessions_smoke() {
        let _ = wasapi::initialize_mta();
        let sessions = capture_sessions().expect("enumerate capture sessions");
        for s in &sessions {
            let (exe, package) = process_identity(s.pid).unwrap_or_default();
            println!(
                "pid {:>6} {:<8} pkg={:<28} {}",
                s.pid,
                if s.active { "ACTIVE" } else { "inactive" },
                package,
                exe
            );
        }
        println!("{} sessions", sessions.len());
    }
}
