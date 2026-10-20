//! OS toast notifications (the `Notifier` the app runs with).
//!
//! Toasts are attributed through an AppUserModelID. The NSIS installer
//! creates the Start-menu shortcut with `com.github.cubityfirst.witness`, so
//! the installed build uses that ID and Windows shows "Witness". Unpackaged
//! dev builds have no shortcut, so they borrow PowerShell's registered ID —
//! the documented dev quirk, exactly like tauri-plugin-notification did.
//!
//! The process AUMID is deliberately never set
//! (`SetCurrentProcessExplicitAppUserModelID`): 0.2.x did not set one, and
//! doing so would split taskbar pins made from the running window.

use crate::service::Notifier;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The bundle identifier NSIS writes onto the Start-menu shortcut.
pub const APP_ID: &str = "com.github.cubityfirst.witness";

/// Registry key whose default value NSIS sets to `$INSTDIR`.
#[cfg(windows)]
const INSTALL_KEY: &str = r"Software\github\Witness";

/// `Notifier` backed by WinRT toasts. Each toast is shown on its own short
/// thread so a slow WinRT call never stalls the pipeline, writer or watcher
/// thread that raised it; failures are ignored (a toast is best-effort).
#[derive(Debug, Default, Clone, Copy)]
pub struct WinToastNotifier;

impl Notifier for WinToastNotifier {
    fn toast(&self, body: &str) {
        let body = body.to_string();
        let spawned = std::thread::Builder::new()
            .name("toast".into())
            .spawn(move || show_toast(&body));
        if let Err(error) = spawned {
            log::warn!("could not show a notification: {error}");
        }
    }
}

#[cfg(windows)]
fn show_toast(body: &str) {
    use tauri_winrt_notification::{Duration, Toast};
    // `.sound(None)` writes `<audio silent="true"/>`. 0.2.x toasts were
    // silent (notify-rust maps "no sound name" to None), and the crate's
    // default chime would play on the default render endpoint mid-call —
    // where device-level loopback can record it into the meeting.
    if let Err(error) = Toast::new(app_id())
        .title("Witness")
        .text1(body)
        .duration(Duration::Short)
        .sound(None)
        .show()
    {
        log::debug!("toast failed: {error:?}");
    }
}

#[cfg(not(windows))]
fn show_toast(body: &str) {
    log::info!("notification: {body}");
}

/// The AUMID toasts are raised under (see the module docs).
pub fn app_id() -> &'static str {
    #[cfg(windows)]
    {
        if is_installed_build() {
            APP_ID
        } else {
            tauri_winrt_notification::Toast::POWERSHELL_APP_ID
        }
    }
    #[cfg(not(windows))]
    {
        APP_ID
    }
}

/// True when this exe runs from the NSIS install directory. Computed once.
///
/// The install dir is authoritative: `HKCU\Software\github\Witness`
/// (default value) names `$INSTDIR`, so a dev build in any target dir —
/// shared, `target-test`, a worktree — is never mistaken for the installed
/// app while one is installed. Without that key the Tauri plugin's rule
/// applies (installed unless the exe sits in `…\target\debug` or
/// `…\target\release`), widened to any cargo profile dir — one holding
/// `deps\` or `.fingerprint\`, which an NSIS install dir never does — so
/// shared and `target-test` target dirs stay on the PowerShell ID too.
pub fn is_installed_build() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let Some(exe_dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        else {
            return false;
        };
        let installed = installed_by_rule(&exe_dir, registry_install_dir().as_deref());
        log::debug!(
            "toast identity: {} ({})",
            if installed { "installed" } else { "dev" },
            exe_dir.display()
        );
        installed
    })
}

/// `$INSTDIR` of the installed app, when one is installed.
#[cfg(windows)]
pub(crate) fn registry_install_dir() -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(INSTALL_KEY)
        .ok()?;
    let dir: String = key.get_value("").ok()?;
    let dir = dir.trim();
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

#[cfg(not(windows))]
pub(crate) fn registry_install_dir() -> Option<PathBuf> {
    None
}

/// The decision behind [`is_installed_build`], separated for tests.
fn installed_by_rule(exe_dir: &Path, install_dir: Option<&Path>) -> bool {
    match install_dir {
        Some(install_dir) => same_dir(exe_dir, install_dir),
        None => {
            // tauri-plugin-notification 2.3.3 desktop.rs:197-205.
            let sep = std::path::MAIN_SEPARATOR;
            let dir = exe_dir.display().to_string();
            let tauri_dev = dir.ends_with(&format!("{sep}target{sep}debug"))
                || dir.ends_with(&format!("{sep}target{sep}release"));
            // Any other cargo profile dir (shared CARGO_TARGET_DIR, target-test).
            let cargo_profile =
                exe_dir.join("deps").is_dir() || exe_dir.join(".fingerprint").is_dir();
            !(tauri_dev || cargo_profile)
        }
    }
}

/// Case-insensitive directory equality after canonicalising both sides
/// (falls back to the literal path when one does not exist).
fn same_dir(a: &Path, b: &Path) -> bool {
    fn key(path: &Path) -> String {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let text = path.to_string_lossy().replace('/', "\\");
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        text.trim_end_matches('\\').to_lowercase()
    }
    key(a) == key(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_install_dir_decides_when_present() {
        let dir = std::env::temp_dir();
        let upper = PathBuf::from(dir.to_string_lossy().to_uppercase());
        assert!(installed_by_rule(&dir, Some(&upper)));
        let with_slash = PathBuf::from(format!("{}\\", dir.display()));
        assert!(installed_by_rule(&dir, Some(&with_slash)));
        // A dev build while the app is installed elsewhere is not installed,
        // even outside a target dir.
        assert!(!installed_by_rule(
            Path::new(r"C:\portable\Witness"),
            Some(Path::new(r"C:\Users\x\AppData\Local\Witness"))
        ));
    }

    #[test]
    fn suffix_rule_applies_without_registry() {
        assert!(!installed_by_rule(
            Path::new(r"G:\Scripts\witness\target\debug"),
            None
        ));
        assert!(!installed_by_rule(
            Path::new(r"C:\witness-port\target\f0\target\release"),
            None
        ));
        assert!(installed_by_rule(
            Path::new(r"C:\Users\x\AppData\Local\Witness"),
            None
        ));
    }

    #[test]
    fn cargo_profile_dirs_are_dev_without_registry() {
        // A shared target dir's profile is outside the plugin's suffix rule
        // but holds `deps\` — e.g. the profile dir this test binary sits under.
        let deps_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let profile_dir = deps_dir.parent().unwrap();
        assert!(profile_dir.join("deps").is_dir());
        assert!(!installed_by_rule(profile_dir, None));

        let scratch = std::env::temp_dir().join(format!(
            "witness-notify-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&scratch).unwrap();
        assert!(installed_by_rule(&scratch, None));
        std::fs::create_dir(scratch.join(".fingerprint")).unwrap();
        assert!(!installed_by_rule(&scratch, None));
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
