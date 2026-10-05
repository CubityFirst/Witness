//! Start with Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! value `Witness` = `"<exe>" --minimized` (the same value name
//! tauri-plugin-autostart used), honouring Task Manager's StartupApproved
//! switch. Written only when the user toggles the checkbox — never repaired
//! at startup (design §7).
//!
//! STUB (F0.2): the signatures are final; the OS-services unit (U9)
//! implements them with `winreg`.

/// Whether Witness starts with Windows. TODO(U9)
pub fn is_enabled() -> Result<bool, String> {
    Err("not implemented".into())
}

/// Turn starting with Windows on or off. TODO(U9)
pub fn set_enabled(_enabled: bool) -> Result<(), String> {
    Err("not implemented".into())
}
