//! Where the AI service's API key lives: Windows Credential Manager (a
//! generic credential, target [`AI_KEY_TARGET`], persisted for this user on
//! this machine and protected by Windows with the user's logon), never
//! `witness-settings.toml`, the database, logs, backups or the diagnostics
//! report.
//!
//! The key is **bound to the host** it was entered for: the credential's
//! user name holds that host, and the core attaches the key only to
//! requests for the same host — changing the base URL to another server
//! never sends it there (the user enters a key for the new host instead).
//!
//! [`CredentialStore`] is the seam: the core reads and writes through it,
//! unit tests use [`MemoryCredentials`], and only an `#[ignore]` test
//! touches the real store. Errors name the Win32 failure, never the key.
//! Our copies of the secret are overwritten when done (best effort: the
//! buffer Windows hands back is zeroed before it is freed).

use crate::ai_client::ApiKey;
use std::sync::Mutex;

/// The Credential Manager entry (Control Panel → Credential Manager →
/// Windows Credentials → "Witness/ai-api-key").
pub const AI_KEY_TARGET: &str = "Witness/ai-api-key";

/// The stored key and the host it belongs to (lower case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredKey {
    pub key: ApiKey,
    pub host: String,
}

/// Reads, writes and deletes the one stored API key.
pub trait CredentialStore: Send + Sync + 'static {
    fn read(&self) -> Result<Option<StoredKey>, String>;
    /// The host of the stored key, if one is stored — without copying the
    /// secret anywhere (presence checks in Settings).
    fn stored_host(&self) -> Result<Option<String>, String>;
    fn write(&self, key: &ApiKey, host: &str) -> Result<(), String>;
    /// Deleting a key that is not there succeeds.
    fn delete(&self) -> Result<(), String>;
}

/// Normalised host for binding: lower case, no surrounding whitespace.
pub fn binding_host(host: &str) -> String {
    host.trim().to_ascii_lowercase()
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryCredentials(Mutex<Option<StoredKey>>);

impl MemoryCredentials {
    pub fn with(key: &str, host: &str) -> Self {
        MemoryCredentials(Mutex::new(ApiKey::new(key).map(|key| StoredKey {
            key,
            host: binding_host(host),
        })))
    }
}

impl CredentialStore for MemoryCredentials {
    fn read(&self) -> Result<Option<StoredKey>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn stored_host(&self) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap().as_ref().map(|s| s.host.clone()))
    }
    fn write(&self, key: &ApiKey, host: &str) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(StoredKey {
            key: key.clone(),
            host: binding_host(host),
        });
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
impl WindowsCredentials {
    /// CredReadW; `take` sees the credential (and may copy the secret out),
    /// then the secret is zeroed in Windows' buffer and the buffer freed.
    fn with_credential<T>(
        &self,
        take: impl FnOnce(&windows::Win32::Security::Credentials::CREDENTIALW) -> T,
    ) -> Result<Option<T>, String> {
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
        // SAFETY: CredReadW succeeded, so `credential` is valid until freed,
        // and the blob is `CredentialBlobSize` writable bytes.
        unsafe {
            let value = take(&*credential);
            let blob = (*credential).CredentialBlob;
            if !blob.is_null() {
                std::ptr::write_bytes(blob, 0, (*credential).CredentialBlobSize as usize);
            }
            CredFree(credential as *const _);
            Ok(Some(value))
        }
    }
}

/// The credential's user name (the host the key is bound to).
#[cfg(windows)]
fn user_name(credential: &windows::Win32::Security::Credentials::CREDENTIALW) -> String {
    if credential.UserName.is_null() {
        return String::new();
    }
    // SAFETY: a non-null UserName from CredReadW is NUL-terminated.
    binding_host(&unsafe { credential.UserName.to_string() }.unwrap_or_default())
}

#[cfg(windows)]
impl CredentialStore for WindowsCredentials {
    fn read(&self) -> Result<Option<StoredKey>, String> {
        let found = self.with_credential(|credential| {
            if credential.CredentialBlob.is_null() {
                return None;
            }
            // SAFETY: the blob is `CredentialBlobSize` bytes (see above).
            let blob = unsafe {
                std::slice::from_raw_parts(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
            };
            let mut text = String::from_utf8_lossy(blob).into_owned();
            let key = ApiKey::new(&text);
            crate::ai_client::wipe(&mut text);
            key.map(|key| StoredKey {
                key,
                host: user_name(credential),
            })
        })?;
        Ok(found.flatten())
    }

    fn stored_host(&self) -> Result<Option<String>, String> {
        self.with_credential(user_name)
    }

    fn write(&self, key: &ApiKey, host: &str) -> Result<(), String> {
        use windows::core::PWSTR;
        use windows::Win32::Security::Credentials::{
            CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        };

        let mut target = wide(&self.target);
        let mut user = wide(&binding_host(host));
        let mut comment =
            wide("API key for the AI service used by Witness (user name = the service's host)");
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
    fn read(&self) -> Result<Option<StoredKey>, String> {
        Ok(None)
    }
    fn stored_host(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn write(&self, _key: &ApiKey, _host: &str) -> Result<(), String> {
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
    fn memory_store_round_trips_with_its_host() {
        let store = MemoryCredentials::default();
        assert_eq!(store.read().unwrap(), None);
        assert_eq!(store.stored_host().unwrap(), None);
        store
            .write(&ApiKey::new("sk-one").unwrap(), " API.OpenAI.com ")
            .unwrap();
        let stored = store.read().unwrap().unwrap();
        assert_eq!(stored.key.expose(), "sk-one");
        assert_eq!(stored.host, "api.openai.com");
        assert_eq!(
            store.stored_host().unwrap().as_deref(),
            Some("api.openai.com")
        );
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
        store.write(&key, "api.example.com").unwrap();
        let stored = store.read().unwrap().unwrap();
        assert_eq!(stored.key, key);
        assert_eq!(stored.host, "api.example.com");
        assert_eq!(
            store.stored_host().unwrap().as_deref(),
            Some("api.example.com")
        );
        let replaced = ApiKey::new("sk-witness-test-2").unwrap();
        store.write(&replaced, "localhost").unwrap();
        assert_eq!(store.read().unwrap().unwrap().key, replaced);
        store.delete().unwrap();
        assert_eq!(store.read().unwrap(), None);
        store.delete().unwrap();
    }
}
