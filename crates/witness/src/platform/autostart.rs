//! Start with Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! value `Witness` = `"<exe>" --minimized` (the same value name
//! tauri-plugin-autostart used), honouring Task Manager's StartupApproved
//! switch. Written only when the user toggles the checkbox — never repaired
//! at startup (design §7).
//!
//! Compatibility with 0.2.x (tauri-plugin-autostart 2.5.1 → auto-launch
//! 0.5.0) is the whole point of this module, so its semantics are mirrored
//! exactly:
//! - the value name, key and REG_SZ type are the same, so an entry written by
//!   0.2.x reads as enabled here with no migration;
//! - the data is now **quoted** (`"C:\…\witness.exe" --minimized`) where the
//!   plugin wrote it unquoted. Nothing parses it except Windows itself and
//!   our own diagnostics ([`registered_exe`] accepts both forms);
//! - enable also writes the 12-byte "enabled" StartupApproved value, but only
//!   when that key already exists, which clears a Task-Manager "Disable";
//! - `is_enabled` = the Run value exists as a string AND the StartupApproved
//!   value is absent, shorter than 8 bytes, or its last 8 bytes (the FILETIME
//!   Task Manager stamps on "Disable") are all zero;
//! - disable deletes only the Run value (StartupApproved is left alone, as
//!   the plugin did) but, unlike the plugin, tolerates it already being gone.
//!
//! The value is per user, not per install, which is why there is no startup
//! repair: a dev or portable build that "fixed" a path mismatch would take
//! over the installed app's autostart. Never touches HKLM.

use std::io;
use std::path::{Path, PathBuf};
use winreg::enums::{RegType, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::{RegKey, RegValue};

/// The Run key (under HKCU).
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Task Manager's per-entry enable/disable switch (under HKCU).
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
/// The value name: the product name, exactly as the Tauri plugin used it.
const VALUE_NAME: &str = "Witness";
/// Makes the autostarted instance start hidden in the tray.
const LAUNCH_ARG: &str = "--minimized";
/// What auto-launch writes to StartupApproved on enable: `02` = enabled,
/// followed by a zero FILETIME.
const APPROVED_ENABLED: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Where the two values live. Production uses the real HKCU keys; the tests
/// point this at a scratch key so they never touch the user's autostart.
#[derive(Debug, Clone, Copy)]
struct Keys<'a> {
    run: &'a str,
    approved: &'a str,
}

const REAL_KEYS: Keys<'static> = Keys {
    run: RUN_KEY,
    approved: APPROVED_KEY,
};

/// Whether Witness starts with Windows.
pub fn is_enabled() -> Result<bool, String> {
    let enabled = is_enabled_at(REAL_KEYS).map_err(|e| format!("Could not read autostart: {e}"))?;
    if enabled {
        // Diagnostics only (no repair, see the module docs): a Run value
        // left by another build of Witness still counts as enabled.
        if let (Ok(Some(registered)), Ok(current)) =
            (registered_exe_at(REAL_KEYS), std::env::current_exe())
        {
            if !same_path(&registered, &current) {
                log::debug!(
                    "autostart points at {} (this build is {})",
                    registered.display(),
                    current.display()
                );
            }
        }
    }
    Ok(enabled)
}

/// Turn starting with Windows on or off. Only ever called from the Settings
/// checkbox.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled {
        let exe =
            std::env::current_exe().map_err(|e| format!("Could not locate witness.exe: {e}"))?;
        let data = run_command(&exe);
        enable_at(REAL_KEYS, &data).map_err(|e| format!("Could not enable autostart: {e}"))?;
        log::info!("autostart enabled: {data}");
    } else {
        disable_at(REAL_KEYS).map_err(|e| format!("Could not disable autostart: {e}"))?;
        log::info!("autostart disabled");
    }
    Ok(())
}

/// The Run value's data for `exe`: quoted path plus the hidden-start arg.
fn run_command(exe: &Path) -> String {
    format!("\"{}\" {LAUNCH_ARG}", exe.display())
}

/// Write the Run value and, if Task Manager's key exists, mark the entry
/// approved (auto-launch 0.5 `enable`).
fn enable_at(keys: Keys<'_>, data: &str) -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(keys.run, KEY_SET_VALUE)?
        .set_value(VALUE_NAME, &data)?;
    // The key only exists once Explorer has created it; never create it.
    if let Ok(approved) = hkcu.open_subkey_with_flags(keys.approved, KEY_SET_VALUE) {
        approved.set_raw_value(
            VALUE_NAME,
            &RegValue {
                vtype: RegType::REG_BINARY,
                bytes: APPROVED_ENABLED.to_vec(),
            },
        )?;
    }
    Ok(())
}

/// Delete the Run value; already absent is fine. StartupApproved is left
/// alone, as auto-launch does.
fn disable_at(keys: Keys<'_>) -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run = match hkcu.open_subkey_with_flags(keys.run, KEY_SET_VALUE) {
        Ok(run) => run,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    match run.delete_value(VALUE_NAME) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// auto-launch 0.5 `is_enabled`: a string Run value, not switched off in
/// Task Manager.
fn is_enabled_at(keys: Keys<'_>) -> io::Result<bool> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run = match hkcu.open_subkey_with_flags(keys.run, KEY_READ) {
        Ok(run) => run,
        // No Run key at all: nothing can be registered.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    // Any read failure (absent, or not a string type) counts as "not
    // registered", like the plugin's `.is_ok()`.
    let registered = run.get_value::<String, _>(VALUE_NAME).is_ok();
    Ok(registered && approved_at(&hkcu, keys).unwrap_or(true))
}

/// Task Manager's verdict: None when there is no (usable) StartupApproved
/// value, which means "not overridden".
fn approved_at(hkcu: &RegKey, keys: Keys<'_>) -> Option<bool> {
    let value = hkcu
        .open_subkey_with_flags(keys.approved, KEY_READ)
        .ok()?
        .get_raw_value(VALUE_NAME)
        .ok()?;
    approved_bytes_enabled(&value.bytes)
}

/// The last 8 bytes are the FILETIME Task Manager writes when the entry is
/// disabled; all zero = enabled. Under 8 bytes = no verdict.
fn approved_bytes_enabled(bytes: &[u8]) -> Option<bool> {
    if bytes.len() < 8 {
        return None;
    }
    Some(bytes[bytes.len() - 8..].iter().all(|b| *b == 0))
}

/// The exe the Run value launches, if there is one.
fn registered_exe_at(keys: Keys<'_>) -> io::Result<Option<PathBuf>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let data: String = match hkcu
        .open_subkey_with_flags(keys.run, KEY_READ)
        .and_then(|run| run.get_value(VALUE_NAME))
    {
        Ok(data) => data,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(registered_exe(&data))
}

/// Split the exe path off a Run command line. Accepts our quoted form
/// (`"C:\a b\witness.exe" --minimized`) and the plugin's unquoted 0.2.x form
/// (`C:\a b\witness.exe --minimized`, where the path may contain spaces, so
/// it ends at the last `.exe` that is followed by a space or the end).
fn registered_exe(data: &str) -> Option<PathBuf> {
    let data = data.trim();
    if let Some(rest) = data.strip_prefix('"') {
        let end = rest.find('"')?;
        let path = &rest[..end];
        return (!path.is_empty()).then(|| PathBuf::from(path));
    }
    let lower = data.to_ascii_lowercase();
    let end = lower
        .match_indices(".exe")
        .map(|(i, _)| i + ".exe".len())
        .filter(|&end| end == data.len() || data[end..].starts_with(' '))
        .last()
        .unwrap_or(data.len());
    let path = data[..end].trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Windows paths compare case-insensitively.
fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch subtree under `HKCU\Software\WitnessTest`, deleted on
    /// drop (and `WitnessTest` itself once no other test still uses it).
    struct Scratch {
        root: String,
        run: String,
        approved: String,
    }

    impl Scratch {
        /// `with_approved`: whether Explorer's StartupApproved key "exists".
        fn new(tag: &str, with_approved: bool) -> Scratch {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = format!(r"Software\WitnessTest\{tag}-{}-{nanos}", std::process::id());
            let scratch = Scratch {
                run: format!(r"{root}\Run"),
                approved: format!(r"{root}\StartupApproved\Run"),
                root,
            };
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            hkcu.create_subkey(&scratch.run).unwrap();
            if with_approved {
                hkcu.create_subkey(&scratch.approved).unwrap();
            }
            scratch
        }

        fn keys(&self) -> Keys<'_> {
            Keys {
                run: &self.run,
                approved: &self.approved,
            }
        }

        fn hkcu(&self) -> RegKey {
            RegKey::predef(HKEY_CURRENT_USER)
        }

        fn run_data(&self) -> Option<String> {
            self.hkcu()
                .open_subkey(&self.run)
                .unwrap()
                .get_value(VALUE_NAME)
                .ok()
        }

        fn set_run_raw(&self, value: &RegValue) {
            self.hkcu()
                .open_subkey_with_flags(&self.run, KEY_SET_VALUE)
                .unwrap()
                .set_raw_value(VALUE_NAME, value)
                .unwrap();
        }

        fn set_run_data(&self, data: &str) {
            self.hkcu()
                .open_subkey_with_flags(&self.run, KEY_SET_VALUE)
                .unwrap()
                .set_value(VALUE_NAME, &data)
                .unwrap();
        }

        fn approved_bytes(&self) -> Option<Vec<u8>> {
            let key = self.hkcu().open_subkey(&self.approved).ok()?;
            key.get_raw_value(VALUE_NAME).ok().map(|v| v.bytes)
        }

        fn set_approved_bytes(&self, bytes: &[u8]) {
            self.hkcu()
                .open_subkey_with_flags(&self.approved, KEY_SET_VALUE)
                .unwrap()
                .set_raw_value(
                    VALUE_NAME,
                    &RegValue {
                        vtype: RegType::REG_BINARY,
                        bytes: bytes.to_vec(),
                    },
                )
                .unwrap();
        }

        fn approved_key_exists(&self) -> bool {
            self.hkcu().open_subkey(&self.approved).is_ok()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let _ = hkcu.delete_subkey_all(&self.root);
            // Non-recursive: fails (harmlessly) while a parallel test's
            // subtree is still there; the last one out removes it.
            let _ = hkcu.delete_subkey(r"Software\WitnessTest");
        }
    }

    /// What Task Manager writes on "Disable": 03 00 00 00 + a FILETIME.
    const TASK_MANAGER_DISABLED: [u8; 12] = [
        0x03, 0, 0, 0, 0x5a, 0x1c, 0x7e, 0x41, 0x3b, 0x2f, 0xdb, 0x01,
    ];

    #[test]
    fn enable_writes_quoted_command_and_approves() {
        let s = Scratch::new("enable", true);
        assert!(!is_enabled_at(s.keys()).unwrap());
        let data = run_command(Path::new(r"C:\Users\A B\AppData\Local\Witness\witness.exe"));
        assert_eq!(
            data,
            r#""C:\Users\A B\AppData\Local\Witness\witness.exe" --minimized"#
        );
        enable_at(s.keys(), &data).unwrap();
        assert_eq!(s.run_data().as_deref(), Some(data.as_str()));
        assert_eq!(s.approved_bytes(), Some(APPROVED_ENABLED.to_vec()));
        assert!(is_enabled_at(s.keys()).unwrap());
        assert_eq!(
            registered_exe_at(s.keys()).unwrap(),
            Some(PathBuf::from(
                r"C:\Users\A B\AppData\Local\Witness\witness.exe"
            ))
        );
    }

    #[test]
    fn enable_never_creates_the_startup_approved_key() {
        let s = Scratch::new("noapproved", false);
        enable_at(s.keys(), r#""C:\w\witness.exe" --minimized"#).unwrap();
        assert!(!s.approved_key_exists());
        assert!(is_enabled_at(s.keys()).unwrap());
    }

    #[test]
    fn task_manager_disable_wins_until_reenabled() {
        let s = Scratch::new("taskmgr", true);
        enable_at(s.keys(), r#""C:\w\witness.exe" --minimized"#).unwrap();
        s.set_approved_bytes(&TASK_MANAGER_DISABLED);
        assert!(
            !is_enabled_at(s.keys()).unwrap(),
            "non-zero FILETIME = disabled"
        );
        // Toggling on in Settings clears Task Manager's switch, like the plugin.
        enable_at(s.keys(), r#""C:\w\witness.exe" --minimized"#).unwrap();
        assert_eq!(s.approved_bytes(), Some(APPROVED_ENABLED.to_vec()));
        assert!(is_enabled_at(s.keys()).unwrap());
    }

    #[test]
    fn startup_approved_byte_semantics() {
        assert_eq!(approved_bytes_enabled(&APPROVED_ENABLED), Some(true));
        assert_eq!(approved_bytes_enabled(&TASK_MANAGER_DISABLED), Some(false));
        // Only the last 8 bytes matter (the leading state DWORD does not).
        assert_eq!(
            approved_bytes_enabled(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some(true)
        );
        assert_eq!(
            approved_bytes_enabled(&[0, 0, 0, 0, 0, 0, 0, 0]),
            Some(true)
        );
        assert_eq!(
            approved_bytes_enabled(&[0, 0, 0, 0, 0, 0, 0, 1]),
            Some(false)
        );
        // Too short to judge: no verdict, which reads as enabled.
        assert_eq!(approved_bytes_enabled(&[0x03, 1, 2]), None);
        assert_eq!(approved_bytes_enabled(&[]), None);
    }

    #[test]
    fn short_startup_approved_value_reads_as_enabled() {
        let s = Scratch::new("short", true);
        s.set_run_data(r#""C:\w\witness.exe" --minimized"#);
        s.set_approved_bytes(&[0x03, 0xff]);
        assert!(is_enabled_at(s.keys()).unwrap());
    }

    #[test]
    fn approved_without_run_value_is_disabled() {
        let s = Scratch::new("approvedonly", true);
        s.set_approved_bytes(&APPROVED_ENABLED);
        assert!(!is_enabled_at(s.keys()).unwrap());
    }

    #[test]
    fn non_string_run_value_is_not_registered() {
        let s = Scratch::new("binary", false);
        s.set_run_raw(&RegValue {
            vtype: RegType::REG_BINARY,
            bytes: vec![1, 2, 3],
        });
        assert!(!is_enabled_at(s.keys()).unwrap());
    }

    #[test]
    fn disable_deletes_run_value_and_tolerates_absence() {
        let s = Scratch::new("disable", true);
        enable_at(s.keys(), r#""C:\w\witness.exe" --minimized"#).unwrap();
        disable_at(s.keys()).unwrap();
        assert_eq!(s.run_data(), None);
        assert!(!is_enabled_at(s.keys()).unwrap());
        // StartupApproved is left alone, as auto-launch does.
        assert_eq!(s.approved_bytes(), Some(APPROVED_ENABLED.to_vec()));
        // Already off: no error (the plugin errored here).
        disable_at(s.keys()).unwrap();
        assert_eq!(registered_exe_at(s.keys()).unwrap(), None);
    }

    #[test]
    fn reads_unquoted_entry_written_by_0_2_x() {
        let s = Scratch::new("legacy", true);
        // Exactly what tauri-plugin-autostart 2.5.1 wrote (path with a space).
        let legacy = r"C:\Users\Jo Bloggs\AppData\Local\Witness\witness.exe --minimized";
        s.set_run_data(legacy);
        s.set_approved_bytes(&APPROVED_ENABLED);
        assert!(is_enabled_at(s.keys()).unwrap());
        assert_eq!(
            registered_exe_at(s.keys()).unwrap(),
            Some(PathBuf::from(
                r"C:\Users\Jo Bloggs\AppData\Local\Witness\witness.exe"
            ))
        );
        // Disabling removes the legacy entry too; re-enabling writes ours.
        disable_at(s.keys()).unwrap();
        assert!(!is_enabled_at(s.keys()).unwrap());
        enable_at(s.keys(), r#""C:\w\witness.exe" --minimized"#).unwrap();
        assert!(is_enabled_at(s.keys()).unwrap());
    }

    #[test]
    fn registered_exe_parses_both_forms() {
        let exe = |s: &str| registered_exe(s).map(|p| p.display().to_string());
        assert_eq!(
            exe(r#""C:\a b\witness.exe" --minimized"#).as_deref(),
            Some(r"C:\a b\witness.exe")
        );
        assert_eq!(
            exe(r#""C:\a b\witness.exe""#).as_deref(),
            Some(r"C:\a b\witness.exe")
        );
        assert_eq!(
            exe(r"C:\a b\witness.exe --minimized").as_deref(),
            Some(r"C:\a b\witness.exe")
        );
        assert_eq!(
            exe(r"C:\a b\WITNESS.EXE").as_deref(),
            Some(r"C:\a b\WITNESS.EXE")
        );
        // ".exe" inside a directory name is not the end of the path.
        assert_eq!(
            exe(r"C:\tools.exe dir\witness.exe --minimized").as_deref(),
            Some(r"C:\tools.exe dir\witness.exe")
        );
        // No ".exe" at all: the whole command line, as a best effort.
        assert_eq!(
            exe(r"C:\w\witness --minimized").as_deref(),
            Some(r"C:\w\witness --minimized")
        );
        assert_eq!(exe(r#""""#), None);
        assert_eq!(exe(r#""C:\unterminated"#), None);
        assert_eq!(exe("   "), None);
    }

    #[test]
    fn same_path_ignores_case() {
        assert!(same_path(
            Path::new(r"C:\Users\A\witness.exe"),
            Path::new(r"c:\users\a\WITNESS.exe")
        ));
        assert!(!same_path(
            Path::new(r"C:\a\witness.exe"),
            Path::new(r"C:\b\witness.exe")
        ));
    }

    /// The public API against the user's REAL Run key (manual acceptance:
    /// the value `reg query` shows). Whatever was there before — a 0.2.x
    /// entry, Task Manager's switch, or nothing — is put back afterwards.
    #[test]
    #[ignore]
    fn real_registry_round_trip() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let read = |key: &str| {
            hkcu.open_subkey(key)
                .ok()
                .and_then(|k| k.get_raw_value(VALUE_NAME).ok())
        };
        let restore = |key: &str, value: Option<RegValue>| {
            if let Ok(k) = hkcu.open_subkey_with_flags(key, KEY_SET_VALUE) {
                match value {
                    Some(value) => k.set_raw_value(VALUE_NAME, &value).unwrap(),
                    None => {
                        let _ = k.delete_value(VALUE_NAME);
                    }
                }
            }
        };
        let (old_run, old_approved) = (read(RUN_KEY), read(APPROVED_KEY));

        set_enabled(true).unwrap();
        assert!(is_enabled().unwrap());
        let data: String = hkcu
            .open_subkey(RUN_KEY)
            .unwrap()
            .get_value(VALUE_NAME)
            .unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(data, format!("\"{}\" --minimized", exe.display()));
        // What `reg query` (i.e. Windows' own view of the value) shows.
        let query = std::process::Command::new("reg")
            .args(["query", &format!(r"HKCU\{RUN_KEY}"), "/v", VALUE_NAME])
            .output()
            .unwrap();
        let shown = String::from_utf8_lossy(&query.stdout);
        println!("{shown}");
        assert!(shown.contains(&data), "reg query shows the quoted command");
        set_enabled(false).unwrap();
        assert!(!is_enabled().unwrap());
        set_enabled(false).unwrap();

        restore(RUN_KEY, old_run);
        restore(APPROVED_KEY, old_approved);
    }
}
