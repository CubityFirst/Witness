//! Where the AI service's API key lives: Windows Credential Manager (a
//! generic credential, target [`AI_KEY_TARGET`], persisted for this user on
//! this machine and protected by Windows with the user's logon), never
//! `witness-settings.toml`, the database, logs, backups or the diagnostics
//! report.
//!
//! [`CredentialStore`] is the seam: the core reads and writes through it,
//! unit tests use [`MemoryCredentials`], and only an `#[ignore]` test
//! touches the real store. Errors name the Win32 failure, never the key.

use crate::ai_client::ApiKey;
use std::sync::Mutex;

/// The Credential Manager entry (Control Panel → Credential Manager →
/// Windows Credentials → "Witness/ai-api-key").
pub const AI_KEY_TARGET: &str = "Witness/ai-api-key";

/// Reads, writes and deletes the one stored API key.
pub trait CredentialStore: Send + Sync + 'static {
    fn read(&self) -> Result<Option<ApiKey>, String>;
    fn write(&self, key: &ApiKey) -> Result<(), String>;
    /// Deleting a key that is not there succeeds.
    fn delete(&self) -> Result<(), String>;
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryCredentials(Mutex<Option<ApiKey>>);

impl MemoryCredentials {
    pub fn with(key: &str) -> Self {
        MemoryCredentials(Mutex::new(ApiKey::new(key)))
    }
}

impl CredentialStore for MemoryCredentials {
    fn read(&self) -> Result<Option<ApiKey>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&self, key: &ApiKey) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(key.clone());
        Ok(())
    }
    fn delete(&self) -> Result<(), String> {
        *self.0.lock().unwrap() = None;
        Ok(())
    }
}

/// Windows Credential Manager (CredReadW / CredWriteW / CredDeleteW).
pub struct WindowsCredentials {
    target: String,
}

impl WindowsCredentials {
    /// The app's entry.
    pub fn new() -> Self {
        Self::with_target(AI_KEY_TARGET)
    }

    /// Another entry (the ignored round-trip test uses a throwaway one).
    pub fn with_target(target: &str) -> Self {
        WindowsCredentials {
            target: target.to_string(),
        }
    }
}

impl Default for WindowsCredentials {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A Win32 failure as text, without anything the caller passed in.
#[cfg(windows)]
fn win32_error(action: &str, error: &windows::core::Error) -> String {
    format!(
        "Windows Credential Manager could not {action} the key ({})",
        error.code()
    )
}

#[cfg(windows)]
impl CredentialStore for WindowsCredentials {
    fn read(&self) -> Result<Option<ApiKey>, String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_NOT_FOUND;
        use windows::Win32::Security::Credentials::{
            CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
        };

        let target = wide(&self.target);
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `target` is NUL-terminated and outlives the call; on
        // success `credential` points at a block we free with CredFree.
        let result = unsafe {
            CredReadW(
                PCWSTR(target.as_ptr()),
                CRED_TYPE_GENERIC,
                None,
                &mut credential,
            )
        };
        if let Err(error) = result {
            if error.code() == ERROR_NOT_FOUND.to_hresult() {
                return Ok(None);
            }
            return Err(win32_error("read", &error));
        }
        // SAFETY: CredReadW succeeded, so `credential` is valid until freed;
        // the blob is `CredentialBlobSize` bytes long.
        let key = unsafe {
            let blob = std::slice::from_raw_parts(
                (*credential).CredentialBlob,
                (*credential).CredentialBlobSize as usize,
            );
            let mut bytes = blob.to_vec();
            let key = std::str::from_utf8(&bytes).ok().and_then(ApiKey::new);
            bytes.fill(0);
            CredFree(credential as *const _);
            key
        };
        Ok(key)
    }

    fn write(&self, key: &ApiKey) -> Result<(), String> {
        use windows::core::PWSTR;
        use windows::Win32::Security::Credentials::{
            CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        };

        let mut target = wide(&self.target);
        let mut user = wide("Witness");
        let mut comment = wide("API key for the AI service used by Witness");
        let mut blob = key.expose().as_bytes().to_vec();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            Comment: PWSTR(comment.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            // This user, this machine; never roams with the profile.
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(user.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: every pointer references a live buffer for the call.
        let result = unsafe { CredWriteW(&credential, 0) };
        blob.fill(0);
        result.map_err(|error| win32_error("save", &error))
    }

    fn delete(&self) -> Result<(), String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_NOT_FOUND;
        use windows::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};

        let target = wide(&self.target);
        // SAFETY: `target` is NUL-terminated and outlives the call.
        match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
            Err(error) => Err(win32_error("delete", &error)),
        }
    }
}

#[cfg(not(windows))]
impl CredentialStore for WindowsCredentials {
    fn read(&self) -> Result<Option<ApiKey>, String> {
        Ok(None)
    }
    fn write(&self, _key: &ApiKey) -> Result<(), String> {
        Err("Credential Manager is only available on Windows".into())
    }
    fn delete(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips() {
        let store = MemoryCredentials::default();
        assert_eq!(store.read().unwrap(), None);
        store.write(&ApiKey::new("sk-one").unwrap()).unwrap();
        assert_eq!(store.read().unwrap().unwrap().expose(), "sk-one");
        store.delete().unwrap();
        store.delete().unwrap();
        assert_eq!(store.read().unwrap(), None);
    }

    /// Writes, reads and deletes a throwaway entry in the REAL Credential
    /// Manager of the user running the test (never the app's own entry).
    #[test]
    #[ignore = "touches the real Windows Credential Manager"]
    fn real_credential_manager_round_trip() {
        let target = format!("Witness/test-ai-api-key-{}", std::process::id());
        let store = WindowsCredentials::with_target(&target);
        assert_eq!(store.read().unwrap(), None);
        let key = ApiKey::new("sk-witness-test-ünïcødé").unwrap();
        store.write(&key).unwrap();
        assert_eq!(store.read().unwrap().as_ref(), Some(&key));
        let replaced = ApiKey::new("sk-witness-test-2").unwrap();
        store.write(&replaced).unwrap();
        assert_eq!(store.read().unwrap().as_ref(), Some(&replaced));
        store.delete().unwrap();
        assert_eq!(store.read().unwrap(), None);
        store.delete().unwrap();
    }
}
