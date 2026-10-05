//! In-app updates from GitHub Releases, replacing tauri-plugin-updater with
//! the same feed, the same minisign key and the same NSIS installer flags.
//!
//! STUB (F0.1): the signatures and constants are final; the updater unit
//! implements the feed check, download, verification and installer launch
//! (design §8.5). Until then every operation reports "not implemented".

use super::{AppCore, CoreResult, UpdateInfo};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `latest.json` of the newest (non-prerelease) GitHub release.
pub const UPDATE_ENDPOINT: &str =
    "https://github.com/CubityFirst/Witness/releases/latest/download/latest.json";

/// minisign public key (key ID 5A06F76D00E6ACD8), verbatim from tauri.conf.json.
pub const UPDATER_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDVBMDZGNzZEMDBFNkFDRDgKUldUWXJPWUFiZmNHV2l6NDROVllaYWN2R0pMYmY3Rzg4N2EwUTh3bWN3RUN2ci9rRVhQZHgwcGEK";

/// Delay before the automatic check, so it never competes with startup
/// recovery, model preflight, or an auto-started recording.
#[allow(dead_code)] // TODO(U2): used by spawn_startup_check.
pub(crate) const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(30);

const NOT_IMPLEMENTED: &str = "Updater not implemented yet";

/// A release offered by the feed (kept as `AppCore::pending_update`).
#[allow(dead_code)] // TODO(U2): filled by parse_feed.
#[derive(Debug, Clone)]
pub(crate) struct Release {
    pub(crate) version: semver::Version,
    pub(crate) notes: Option<String>,
    pub(crate) date: Option<String>,
    pub(crate) url: String,
    pub(crate) signature: String,
}

/// Parse `latest.json`; `Some` only when the feed version is strictly newer.
#[allow(dead_code)] // TODO(U2)
pub(crate) fn parse_feed(
    _json: &str,
    _current: &semver::Version,
) -> anyhow::Result<Option<Release>> {
    anyhow::bail!(NOT_IMPLEMENTED)
}

/// Verify a downloaded installer against the minisign signature from the
/// feed (and, when required, the signed `version:` in its trusted comment).
#[allow(dead_code)] // TODO(U2)
pub(crate) fn verify(
    _installer: &[u8],
    _signature_b64: &str,
    _pubkey_b64: &str,
    _expected_version: &semver::Version,
    _require_signed_version: bool,
) -> anyhow::Result<()> {
    anyhow::bail!(NOT_IMPLEMENTED)
}

/// A downloaded, verified installer ready to launch.
#[derive(Debug, Clone)]
pub struct PreparedUpdate {
    pub version: String,
    pub installer: PathBuf,
}

impl PreparedUpdate {
    /// ShellExecuteW "open" installer "/P /UPDATE /R /ARGS <escaped args>".
    /// Err when the launch fails; the process keeps running in that case.
    pub fn launch(&self, _relaunch_args: &[OsString]) -> CoreResult<()> {
        Err(NOT_IMPLEMENTED.into())
    }
}

/// Called by `AppCore::start`: when `check_for_updates` is on, check once
/// after `STARTUP_CHECK_DELAY` and toast only when there is news.
pub(crate) fn spawn_startup_check(_core: Arc<AppCore>) {}

impl AppCore {
    /// Fetch the feed and remember a newer release as pending. ⏳
    pub fn check_for_update(&self) -> CoreResult<Option<UpdateInfo>> {
        super::assert_off_ui_thread("check_for_update");
        Err(NOT_IMPLEMENTED.into())
    }

    /// The release found by the most recent check (startup or manual). [ui-ok]
    pub fn pending_update(&self) -> Option<UpdateInfo> {
        None
    }

    /// Download and verify the pending installer into %TEMP%, emitting
    /// `UpdateProgress`. Fails fast while the recorder is busy. ⏳
    pub fn download_update(&self) -> CoreResult<PreparedUpdate> {
        super::assert_off_ui_thread("download_update");
        Err(NOT_IMPLEMENTED.into())
    }
}
