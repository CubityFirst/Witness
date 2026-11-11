//! In-app updates from GitHub Releases, replacing tauri-plugin-updater 2.12.0
//! with the same feed, the same minisign key and the same NSIS installer
//! flags — every 0.2.x install updates through its Tauri updater, and every
//! native build must keep consuming the very same `latest.json` (design §8.1).
//!
//! Flow:
//! - **Check** (`check_for_update`, also run once 30 s after startup when
//!   `check_for_updates` is on): GET `latest.json`, parse it exactly like the
//!   plugin (`name` alias, leading `v`, RFC 3339 `pub_date`,
//!   `windows-x86_64-nsis` then `windows-x86_64`), and remember a strictly
//!   newer release as `pending_update`.
//! - **Download** (`download_update`, always a click in Settings → About → Updates):
//!   stream the installer into `%TEMP%\Witness-{ver}-updater-XXXXXX\`, then
//!   check the `MZ` header and the minisign signature against the embedded
//!   key, *requiring* the signed `version:` to match the feed. Any failure
//!   removes the temp dir, so no unverified executable is ever left behind.
//! - **Launch** (`PreparedUpdate::launch`, on the main thread right before
//!   exit): ShellExecute the installer with the plugin's passive-update
//!   arguments `/P /UPDATE /R /ARGS <current args>`. The installer kills this
//!   process (if it is still running) and relaunches it with those args.
//!
//! Network reach matches the plugin's reqwest defaults: TLS through SChannel
//! (the Windows certificate store, so enterprise roots pushed by GPO and
//! TLS-inspecting proxies work) and the WinINet system proxy (honouring
//! `ProxyOverride`), otherwise the `HTTPS_PROXY`/`ALL_PROXY` environment
//! (honouring `NO_PROXY`). PAC/WPAD and NTLM proxy auth are unsupported — as
//! they were with the plugin.

use super::{AppCore, CoreEvent, CoreResult, UpdateInfo};
use crate::events;
use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `latest.json` of the newest (non-prerelease) GitHub release.
pub const UPDATE_ENDPOINT: &str =
    "https://github.com/CubityFirst/Witness/releases/latest/download/latest.json";

/// minisign public key (key ID 5A06F76D00E6ACD8), verbatim from tauri.conf.json.
pub const UPDATER_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDVBMDZGNzZEMDBFNkFDRDgKUldUWXJPWUFiZmNHV2l6NDROVllaYWN2R0pMYmY3Rzg4N2EwUTh3bWN3RUN2ci9rRVhQZHgwcGEK";

/// Delay before the automatic check, so it never competes with startup
/// recovery, model preflight, or an auto-started recording.
pub(crate) const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(30);

/// Feed platform keys, most specific first — the plugin's lookup order for
/// a binary stamped as an NSIS bundle.
const PLATFORM_KEYS: [&str; 2] = ["windows-x86_64-nsis", "windows-x86_64"];

/// Prefix of the temp dir and installer names (the plugin used the product
/// name, so both look exactly like they did under 0.2.x).
const APP_NAME: &str = "Witness";

/// `UpdateProgress` granularity; ~1 MiB keeps the event stream light.
const PROGRESS_STEP: u64 = 1 << 20;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Per read, not for the whole transfer: a slow link may take minutes for
/// the ~100 MB installer, but a stalled one fails within a minute.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Hosts the check and download talk to: the feed and the release download
/// on github.com, and the CDN hosts GitHub redirects release assets to. A
/// WinINet `ProxyOverride` bypasses the proxy only when it covers them all
/// (one agent serves the whole redirect chain).
const UPDATE_HOSTS: [&str; 3] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// The running build's version (the workspace version).
fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A release offered by the feed (kept as `AppCore::pending_update`).
#[derive(Debug, Clone)]
pub(crate) struct Release {
    pub(crate) version: semver::Version,
    /// Release notes; blank notes are already `None`.
    pub(crate) notes: Option<String>,
    /// Date part (`YYYY-MM-DD`) of `pub_date`.
    pub(crate) date: Option<String>,
    pub(crate) url: String,
    /// The `.sig` file contents: base64 of the minisign signature box.
    pub(crate) signature: String,
}

impl Release {
    fn info(&self) -> UpdateInfo {
        UpdateInfo {
            current_version: current_version().to_string(),
            version: self.version.to_string(),
            notes: self.notes.clone(),
            date: self.date.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Feed
// ---------------------------------------------------------------------------

/// `latest.json`, deserialized like the plugin's `RemoteRelease`: either a
/// static `platforms` map or a "dynamic" response with top-level url and
/// signature (our feed is always static; dynamic costs nothing to accept).
#[derive(Deserialize)]
struct Feed {
    #[serde(alias = "name")]
    version: String,
    notes: Option<String>,
    pub_date: Option<String>,
    platforms: Option<HashMap<String, FeedPlatform>>,
    url: Option<String>,
    signature: Option<String>,
}

#[derive(Deserialize)]
struct FeedPlatform {
    signature: String,
    url: String,
}

/// Parse `latest.json`; `Some` only when the feed version is strictly newer.
pub(crate) fn parse_feed(json: &str, current: &semver::Version) -> anyhow::Result<Option<Release>> {
    let feed: Feed = serde_json::from_str(json).context("invalid update feed")?;
    let version = semver::Version::parse(feed.version.trim_start_matches('v'))
        .with_context(|| format!("invalid version `{}` in the update feed", feed.version))?;
    // Like the plugin, a malformed date rejects the whole feed.
    let date = feed
        .pub_date
        .as_deref()
        .map(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .map(|date| date.date_naive().to_string())
                .map_err(|e| anyhow!("invalid value for `pub_date`: {e}"))
        })
        .transpose()?;
    if version <= *current {
        return Ok(None);
    }

    let (url, signature) = match feed.platforms {
        Some(mut platforms) => {
            let platform = PLATFORM_KEYS
                .iter()
                .find_map(|key| platforms.remove(*key))
                .ok_or_else(|| {
                    anyhow!(
                        "None of the fallback platforms `{PLATFORM_KEYS:?}` were found in the response `platforms` object"
                    )
                })?;
            (platform.url, platform.signature)
        }
        None => (
            feed.url
                .ok_or_else(|| anyhow!("the `url` field was not set on the updater response"))?,
            feed.signature.ok_or_else(|| {
                anyhow!("the `signature` field was not set on the updater response")
            })?,
        ),
    };
    Ok(Some(Release {
        version,
        notes: feed.notes.filter(|notes| !notes.trim().is_empty()),
        date,
        url,
        signature,
    }))
}

/// GET the feed and parse it against `current`.
fn fetch_release(
    agent: &ureq::Agent,
    endpoint: &str,
    current: &semver::Version,
) -> anyhow::Result<Option<Release>> {
    log::debug!("checking for updates at {endpoint}");
    let response = agent
        .get(endpoint)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| http_error("Update check", e))?;
    // The plugin's convention for "no update" from a dynamic server.
    if response.status() == 204 {
        return Ok(None);
    }
    let body = response
        .into_string()
        .context("reading the update feed failed")?;
    parse_feed(&body, current)
}

/// ureq reports 4xx/5xx as errors; say which status it was, in the plugin's
/// wording (`what` is "Update check" or "Download").
///
/// The URL stays in the log only: after GitHub's redirect it is a signed CDN
/// URL (SAS query string), which has no place in a toast. ureq's transport
/// errors print their URL too, so they are rebuilt without it.
fn http_error(what: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, response) => {
            log::warn!(
                "{what} request to {} failed with status {code}",
                response.get_url()
            );
            anyhow!(
                "{what} request failed with status: {code} {}",
                response.status_text()
            )
        }
        ureq::Error::Transport(transport) => {
            log::warn!("{what} request failed: {transport}");
            let mut message = transport.kind().to_string();
            if let Some(detail) = transport.message() {
                message.push_str(&format!(": {detail}"));
            }
            if let Some(source) = std::error::Error::source(&transport) {
                message.push_str(&format!(": {source}"));
            }
            anyhow!("{what} request failed: {message}")
        }
    }
}

// ---------------------------------------------------------------------------
// Signature
// ---------------------------------------------------------------------------

/// Verify a downloaded installer against the minisign signature from the
/// feed, and the signed `version:` in its trusted comment against
/// `expected_version` (port of the plugin's `verify_signature`).
///
/// A signed version that differs is always rejected; a missing one only
/// when `require_signed_version` — the native updater requires it, because
/// every release from 0.3.0 on is signed by the xtask with `version:`.
pub(crate) fn verify(
    installer: &[u8],
    signature_b64: &str,
    pubkey_b64: &str,
    expected_version: &semver::Version,
    require_signed_version: bool,
) -> anyhow::Result<()> {
    let public_key = minisign_verify::PublicKey::decode(&base64_to_string(pubkey_b64)?)
        .context("invalid updater public key")?;
    let signature = minisign_verify::Signature::decode(&base64_to_string(signature_b64)?)
        .context("invalid update signature")?;
    // allow_legacy = true, exactly as the plugin verified.
    public_key
        .verify(installer, &signature, true)
        .context("the update signature does not match the downloaded installer")?;
    // Only now is the trusted comment usable: the global signature that
    // `verify` just checked is what covers it.
    verify_signed_version(
        signature.trusted_comment(),
        &expected_version.to_string(),
        require_signed_version,
    )
}

/// The plugin's `verify_signed_version`: the feed itself is unsigned, so the
/// version the artifact was signed for is what stops a tampered feed from
/// pairing a new version number with an older (vulnerable) installer.
fn verify_signed_version(
    trusted_comment: &str,
    announced_version: &str,
    require_signed_version: bool,
) -> anyhow::Result<()> {
    let Some(signed_version) = signed_version(trusted_comment) else {
        return if require_signed_version {
            Err(anyhow!(
                "The update signature does not specify the version it was signed for, which the updater requires."
            ))
        } else {
            Ok(())
        };
    };
    // Compare as semver so `1.2.3` and `v1.2.3` match; literal otherwise.
    let matches = match (
        semver::Version::parse(signed_version.trim_start_matches('v')),
        semver::Version::parse(announced_version.trim_start_matches('v')),
    ) {
        (Ok(signed), Ok(announced)) => signed == announced,
        _ => signed_version == announced_version,
    };
    if matches {
        Ok(())
    } else {
        Err(anyhow!(
            "The update was signed for version {signed_version} but the update endpoint announced version {announced_version}. The endpoint response may have been tampered with to force installing a different release."
        ))
    }
}

/// The `version` field of a trusted comment written as tab-separated
/// `key:value` pairs, e.g. `timestamp:1700000000\tfile:app.exe\tversion:1.2.3`.
fn signed_version(trusted_comment: &str) -> Option<&str> {
    trusted_comment
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"))
}

/// Keys and `.sig` files are base64 of the minisign box text.
fn base64_to_string(b64: &str) -> anyhow::Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("invalid base64")?;
    String::from_utf8(bytes).map_err(|_| {
        anyhow!("The signature {b64} could not be decoded, please check if it is a valid base64 string.")
    })
}

// ---------------------------------------------------------------------------
// HTTP agent: SChannel TLS + system proxy
// ---------------------------------------------------------------------------

/// One agent for the feed and the installer download (redirects followed).
fn agent() -> anyhow::Result<ureq::Agent> {
    Ok(agent_builder("updater", &UPDATE_HOSTS)?
        .timeout_read(READ_TIMEOUT)
        .build())
}

/// The shared HTTPS client setup (the updater's, reused by the AI client):
/// SChannel TLS (Windows certificate store), connect timeout, user agent,
/// and the WinINet system proxy (else `HTTPS_PROXY`/`ALL_PROXY`) chosen for
/// `hosts`. Loopback hosts always connect directly. `label` prefixes the
/// proxy log lines. Callers add their own read/overall timeouts.
pub(crate) fn agent_builder(label: &str, hosts: &[&str]) -> anyhow::Result<ureq::AgentBuilder> {
    let tls = native_tls::TlsConnector::new().context("TLS setup failed")?;
    let mut builder = ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        .timeout_connect(CONNECT_TIMEOUT)
        .user_agent(&format!("{APP_NAME}/{}", current_version()))
        // We pick the proxy ourselves: ureq's own environment lookup also
        // takes HTTP_PROXY for HTTPS requests and ignores NO_PROXY.
        .try_proxy_from_env(false);
    if !hosts.is_empty() && hosts.iter().all(|host| is_loopback_host(host)) {
        return Ok(builder);
    }
    let environment = || {
        env_proxy(label, |name| std::env::var(name).ok(), hosts)
            .and_then(|address| usable_proxy(label, "environment", &address))
    };
    let proxy = match system_proxy(hosts) {
        // An unusable system proxy (e.g. an `https://` one, which ureq
        // cannot speak): the environment or a direct connection is the best
        // remaining try.
        SystemProxy::Use(address) => usable_proxy(label, "system", &address).or_else(environment),
        SystemProxy::Direct => {
            // WinINet says go direct; an environment proxy must not override it.
            log::info!(
                "{label}: the system proxy is bypassed for {}",
                hosts.join(", ")
            );
            None
        }
        SystemProxy::None => environment(),
    };
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

/// `localhost`, `127.x.x.x` or `::1` (brackets allowed).
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `address` as a ureq proxy, or None (logged) when ureq cannot use it.
fn usable_proxy(label: &str, source: &str, address: &str) -> Option<ureq::Proxy> {
    match ureq::Proxy::new(address) {
        Ok(proxy) => {
            log::info!("{label}: using the {source} proxy {address}");
            Some(proxy)
        }
        Err(e) => {
            log::warn!("{label}: ignoring the unusable {source} proxy {address:?}: {e}");
            None
        }
    }
}

/// The environment proxy for HTTPS requests — `HTTPS_PROXY`, then
/// `ALL_PROXY` (each upper case first, as reqwest read them) — unless
/// `NO_PROXY` covers every one of `hosts`. `var` reads a variable (injected
/// so tests need not touch the process environment).
fn env_proxy(label: &str, var: impl Fn(&str) -> Option<String>, hosts: &[&str]) -> Option<String> {
    let first_set = |names: &[&str]| {
        names
            .iter()
            .filter_map(|name| var(name))
            .map(|value| value.trim().to_string())
            .find(|value| !value.is_empty())
    };
    let address = first_set(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"])?;
    let no_proxy = first_set(&["NO_PROXY", "no_proxy"]).unwrap_or_default();
    if hosts.iter().all(|host| no_proxy_matches(&no_proxy, host)) {
        log::info!(
            "{label}: NO_PROXY bypasses the environment proxy for {}",
            hosts.join(", ")
        );
        return None;
    }
    Some(address)
}

/// `NO_PROXY` with reqwest's (and curl's) domain rules: comma-separated;
/// `*` matches everything; `example.com` matches it and its subdomains;
/// `.example.com` (or `*.example.com`) matches its subdomains and itself.
fn no_proxy_matches(no_proxy: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    no_proxy
        .split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            let domain = entry.trim_start_matches('*').trim_start_matches('.');
            !domain.is_empty()
                && (host == domain
                    || host
                        .strip_suffix(domain)
                        .is_some_and(|prefix| prefix.ends_with('.')))
        })
}

/// What the WinINet (Internet Options) settings say about the update hosts.
#[derive(Debug, PartialEq, Eq)]
enum SystemProxy {
    /// Connect through this proxy address.
    Use(String),
    /// A proxy is configured, but `ProxyOverride` bypasses every update host.
    Direct,
    /// No usable WinINet proxy (disabled, or no HTTP/HTTPS entry).
    None,
}

/// The WinINet proxy setting for `hosts`.
#[cfg(windows)]
fn system_proxy(hosts: &[&str]) -> SystemProxy {
    use winreg::enums::HKEY_CURRENT_USER;
    let Ok(settings) = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
    else {
        return SystemProxy::None;
    };
    let enabled: u32 = settings.get_value("ProxyEnable").unwrap_or(0);
    if enabled == 0 {
        return SystemProxy::None;
    }
    let Ok(server) = settings.get_value::<String, _>("ProxyServer") else {
        return SystemProxy::None;
    };
    let overrides: String = settings.get_value("ProxyOverride").unwrap_or_default();
    wininet_proxy(&server, &overrides, hosts)
}

#[cfg(not(windows))]
fn system_proxy(_hosts: &[&str]) -> SystemProxy {
    SystemProxy::None
}

/// Pure part of the WinINet lookup (ProxyEnable already checked): the proxy
/// address to use for `hosts`, `Direct` when the `ProxyOverride` list
/// bypasses every one of them, `None` when there is no HTTP(S) proxy.
fn wininet_proxy(server: &str, overrides: &str, hosts: &[&str]) -> SystemProxy {
    let Some(address) = https_proxy_server(server) else {
        return SystemProxy::None;
    };
    if hosts.iter().all(|host| bypasses_proxy(overrides, host)) {
        return SystemProxy::Direct;
    }
    SystemProxy::Use(address)
}

/// `ProxyServer` is either one `host:port` for every protocol, or the
/// per-protocol `http=host:port;https=host:port;…` form. We only ever make
/// HTTPS requests, so `https=` wins; `http=` is the fallback.
fn https_proxy_server(server: &str) -> Option<String> {
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    if !server.contains('=') {
        return Some(server.to_string());
    }
    let entry = |scheme: &str| {
        server.split(';').find_map(|part| {
            let (key, value) = part.split_once('=')?;
            let value = value.trim();
            (key.trim().eq_ignore_ascii_case(scheme) && !value.is_empty())
                .then(|| value.to_string())
        })
    };
    entry("https").or_else(|| entry("http"))
}

/// `ProxyOverride`: `;`-separated, case-insensitive globs (`*`, `?`) against
/// the host name; `<local>` matches dotless (intranet) host names.
fn bypasses_proxy(overrides: &str, host: &str) -> bool {
    overrides
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry.eq_ignore_ascii_case("<local>") {
                return !host.contains('.');
            }
            let pattern = entry
                .strip_prefix("https://")
                .or_else(|| entry.strip_prefix("http://"))
                .unwrap_or(entry);
            glob_match(&pattern.to_ascii_lowercase(), &host.to_ascii_lowercase())
        })
}

/// Iterative wildcard match with backtracking to the last `*`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((star_pi, star_ti)) = star {
            pi = star_pi + 1;
            ti = star_ti + 1;
            star = Some((star_pi, star_ti + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

/// GET `release.url` and hand the body to [`save_and_verify`].
fn download_release(
    agent: &ureq::Agent,
    release: &Release,
    temp_root: &Path,
    pubkey_b64: &str,
    require_signed_version: bool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> anyhow::Result<PathBuf> {
    log::info!(
        "downloading update {} from {}",
        release.version,
        release.url
    );
    let response = agent
        .get(&release.url)
        .set("Accept", "application/octet-stream")
        .call()
        .map_err(|e| http_error("Download", e))?;
    let total = response
        .header("Content-Length")
        .and_then(|value| value.trim().parse::<u64>().ok());
    save_and_verify(
        &mut response.into_reader(),
        total,
        release,
        temp_root,
        pubkey_b64,
        require_signed_version,
        progress,
    )
}

/// Stream `body` into `temp_root\Witness-{ver}-updater-XXXXXX\Witness-{ver}-installer.exe`
/// (the plugin's layout), then check the `MZ` header and the signature.
/// On ANY error once the dir exists — a network read, a short body, a bad
/// header or signature — the dir is removed before returning, so no
/// unverified executable stays on disk. It survives only on success,
/// because the installer must still exist when ShellExecute runs.
fn save_and_verify(
    body: &mut dyn Read,
    total: Option<u64>,
    release: &Release,
    temp_root: &Path,
    pubkey_b64: &str,
    require_signed_version: bool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> anyhow::Result<PathBuf> {
    let dir = create_temp_dir(temp_root, &release.version)?;
    let installer = dir.join(format!("{APP_NAME}-{}-installer.exe", release.version));
    let result = write_body(body, total, &installer, progress).and_then(|()| {
        // Read back what is on disk: that file, not our buffer, is what runs.
        let bytes = std::fs::read(&installer).context("reading the downloaded installer")?;
        if !bytes.starts_with(b"MZ") {
            bail!("the downloaded file is not a Windows installer");
        }
        verify(
            &bytes,
            &release.signature,
            pubkey_b64,
            &release.version,
            require_signed_version,
        )
    });
    match result {
        Ok(()) => Ok(installer),
        Err(e) => {
            if let Err(cleanup) = std::fs::remove_dir_all(&dir) {
                log::warn!("could not remove {}: {cleanup}", dir.display());
            }
            Err(e)
        }
    }
}

/// Copy `body` to `path`, reporting progress every [`PROGRESS_STEP`] bytes
/// and once at the end.
fn write_body(
    body: &mut dyn Read,
    total: Option<u64>,
    path: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> anyhow::Result<()> {
    let mut file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    // Heap buffer: this may run on any worker thread (CLAUDE.md: no large
    // stack arrays).
    let mut buf = vec![0u8; 64 * 1024];
    let (mut downloaded, mut last_emit) = (0u64, 0u64);
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow::Error::new(e).context("the download was interrupted")),
        };
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", path.display()))?;
        downloaded += n as u64;
        if downloaded - last_emit >= PROGRESS_STEP {
            last_emit = downloaded;
            progress(downloaded, total);
        }
    }
    file.sync_all()
        .with_context(|| format!("writing {}", path.display()))?;
    if let Some(total) = total {
        if downloaded != total {
            bail!("the download ended early ({downloaded} of {total} bytes)");
        }
    }
    if downloaded != last_emit || downloaded == 0 {
        progress(downloaded, total);
    }
    Ok(())
}

/// A fresh, uniquely named `Witness-{ver}-updater-XXXXXX` dir (tempfile's
/// naming, as the plugin used). `create_dir` fails on an existing name, so
/// a collision just draws another suffix.
fn create_temp_dir(root: &Path, version: &semver::Version) -> anyhow::Result<PathBuf> {
    let mut attempt = 0;
    loop {
        let dir = root.join(format!(
            "{APP_NAME}-{version}-updater-{}",
            random_suffix(attempt)
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 32 => {
                attempt += 1;
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("creating a temporary folder in {}", root.display())))
            }
        }
    }
}

/// Six alphanumerics. `RandomState` is seeded from the OS RNG, which is all
/// the randomness a temp-dir name needs (no extra crate).
fn random_suffix(salt: u32) -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u32(salt);
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    let mut n = hasher.finish();
    (0..6)
        .map(|_| {
            let c = ALPHABET[(n % ALPHABET.len() as u64) as usize];
            n /= ALPHABET.len() as u64;
            c as char
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

/// A downloaded, verified installer ready to launch.
///
/// Dropping it without a successful [`launch`](Self::launch) — the exit
/// guard refused (a recording started during the download, a backup), the
/// launch failed, or the app quit first — removes its temp dir, so the
/// ~100 MB installer never piles up in %TEMP% across retries (0.2.x checked
/// those guards before writing any file). After a successful launch the
/// installer must outlive this process, so it is kept (and `process::exit`
/// skips destructors anyway). Not `Clone`: one value owns the dir.
#[derive(Debug)]
pub struct PreparedUpdate {
    pub version: String,
    pub installer: PathBuf,
    /// The `Witness-{ver}-updater-XXXXXX` dir to remove on drop; `None`
    /// once launched (or for a value that owns no dir).
    temp_dir: Mutex<Option<PathBuf>>,
}

impl Drop for PreparedUpdate {
    fn drop(&mut self) {
        let dir = self
            .temp_dir
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(dir) = dir {
            log::info!("discarding the unlaunched update {}", self.version);
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                log::warn!("could not remove {}: {e}", dir.display());
            }
        }
    }
}

impl PreparedUpdate {
    /// Owns `installer`'s parent dir, a fresh `create_temp_dir` result.
    fn owning_temp_dir(version: String, installer: PathBuf) -> Self {
        let temp_dir = installer.parent().map(Path::to_path_buf);
        Self {
            version,
            installer,
            temp_dir: Mutex::new(temp_dir),
        }
    }

    /// ShellExecuteW "open" installer "/P /UPDATE /R /ARGS <escaped args>":
    /// passive install over the current `$INSTDIR`, no shortcut/AUMID
    /// rewrite, relaunch with this process's arguments (e.g. `--minimized`).
    ///
    /// Err (the bare reason; the caller prefixes "Installing the update
    /// failed: ") when ShellExecute reports failure (≤ 32) — the process
    /// keeps running in that case (design §4.5).
    pub fn launch(&self, relaunch_args: &[OsString]) -> CoreResult<()> {
        let parameters = nsis_parameters(relaunch_args);
        log::info!(
            "launching the {} installer: {} {}",
            self.version,
            self.installer.display(),
            parameters.to_string_lossy()
        );
        shell_execute_open(&self.installer, &parameters)?;
        // Launched: the installer must survive this value.
        self.temp_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Ok(())
    }
}

/// The plugin's NSIS argument string for `installMode: "passive"` with
/// restart-after-install: `/P /UPDATE /R /ARGS` then each argument escaped,
/// all joined by single spaces.
fn nsis_parameters(relaunch_args: &[OsString]) -> OsString {
    let mut parameters = OsString::from("/P /UPDATE /R /ARGS");
    for arg in relaunch_args {
        parameters.push(" ");
        parameters.push(escape_nsis_current_exe_arg(arg));
    }
    parameters
}

// Ported verbatim from tauri-plugin-updater 2.12.0 (updater.rs:1642-1680),
// itself adapted from
// https://github.com/rust-lang/rust/blob/1c047506f94cd2d05228eb992b0a6bbed1942349/library/std/src/sys/args/windows.rs#L174
#[cfg(windows)]
fn escape_nsis_current_exe_arg(arg: impl AsRef<OsStr>) -> OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let arg = arg.as_ref();
    let mut cmd: Vec<u16> = Vec::new();

    // compared to std we additionally escape `/` so that nsis won't interpret them as a beginning of an nsis argument.
    let quote = arg
        .as_encoded_bytes()
        .iter()
        .any(|c| *c == b' ' || *c == b'\t' || *c == b'/')
        || arg.is_empty();
    let escape = true;
    if quote {
        cmd.push('"' as u16);
    }
    let mut backslashes: usize = 0;
    for x in arg.encode_wide() {
        if escape {
            if x == '\\' as u16 {
                backslashes += 1;
            } else {
                if x == '"' as u16 {
                    // Add n+1 backslashes to total 2n+1 before internal '"'.
                    cmd.extend((0..=backslashes).map(|_| '\\' as u16));
                }
                backslashes = 0;
            }
        }
        cmd.push(x);
    }
    if quote {
        // Add n backslashes to total 2n before ending '"'.
        cmd.extend((0..backslashes).map(|_| '\\' as u16));
        cmd.push('"' as u16);
    }
    OsString::from_wide(&cmd)
}

#[cfg(not(windows))]
fn escape_nsis_current_exe_arg(arg: impl AsRef<OsStr>) -> OsString {
    arg.as_ref().to_os_string()
}

#[cfg(windows)]
fn shell_execute_open(file: &Path, parameters: &OsStr) -> CoreResult<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;

    let wide = |s: &OsStr| -> Vec<u16> { s.encode_wide().chain(std::iter::once(0)).collect() };
    let file = wide(file.as_os_str());
    let parameters = wide(parameters);
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(file.as_ptr()),
            PCWSTR(parameters.as_ptr()),
            PCWSTR::null(),
            SW_SHOW,
        )
    };
    let code = result.0 as isize;
    if code <= 32 {
        let error = std::io::Error::last_os_error();
        log::error!("ShellExecuteW returned {code}: {error}");
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn shell_execute_open(_file: &Path, _parameters: &OsStr) -> CoreResult<()> {
    Err("Installing updates is only supported on Windows".into())
}

// ---------------------------------------------------------------------------
// AppCore surface + startup check
// ---------------------------------------------------------------------------

/// Called by `AppCore::start`: when `check_for_updates` is on, check once
/// after `STARTUP_CHECK_DELAY` and toast only when there is news.
pub(crate) fn spawn_startup_check(core: Arc<AppCore>) {
    let enabled = core.state.settings.lock().unwrap().check_for_updates;
    if !enabled {
        return;
    }
    // A Weak, so a core dropped during the delay (tests) is not kept alive.
    let core = Arc::downgrade(&core);
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_CHECK_DELAY);
            let Some(core) = core.upgrade() else { return };
            match core.check_for_update() {
                Ok(Some(info)) => {
                    log::info!(
                        "update available: {} -> {}",
                        info.current_version,
                        info.version
                    );
                    core.toast(&format!(
                        "Witness {} is available. Install it from Settings → About.",
                        info.version
                    ));
                }
                Ok(None) => log::info!("update check: up to date"),
                // Offline, feed not published yet, etc. — never worth a toast.
                Err(e) => log::warn!("update check failed: {e}"),
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the update check: {e}");
    }
}

impl AppCore {
    /// Fetch the feed and remember a newer release as pending (or clear the
    /// pending one when up to date). ⏳
    pub fn check_for_update(&self) -> CoreResult<Option<UpdateInfo>> {
        super::assert_off_ui_thread("check_for_update");
        let current = semver::Version::parse(current_version()).map_err(super::err)?;
        let release = agent()
            .and_then(|agent| fetch_release(&agent, UPDATE_ENDPOINT, &current))
            .map_err(super::err)?;
        let info = release.as_ref().map(Release::info);
        *self.pending_update.lock().unwrap() = release;
        Ok(info)
    }

    /// The release found by the most recent check (startup or manual). [ui-ok]
    pub fn pending_update(&self) -> Option<UpdateInfo> {
        self.pending_update
            .lock()
            .unwrap()
            .as_ref()
            .map(Release::info)
    }

    /// Download and verify the pending installer into %TEMP%, emitting
    /// `UpdateProgress`. Fails fast while the recorder is busy (re-checked
    /// under the locks by `exit_guard` before launch). ⏳
    pub fn download_update(&self) -> CoreResult<PreparedUpdate> {
        super::assert_off_ui_thread("download_update");
        let release = self
            .pending_update
            .lock()
            .unwrap()
            .clone()
            .ok_or("No update is pending. Check for updates first")?;
        // Fail fast before a ~100 MB download.
        if self.state.recorder.lock().unwrap().is_busy() {
            return Err("Stop the recording before installing the update".into());
        }
        let sink = self.sink.clone();
        let mut progress = move |downloaded: u64, total: Option<u64>| {
            sink.emit(CoreEvent::UpdateProgress(events::UpdateProgress {
                downloaded,
                total,
            }));
        };
        let installer = agent()
            .and_then(|agent| {
                download_release(
                    &agent,
                    &release,
                    &std::env::temp_dir(),
                    UPDATER_PUBKEY,
                    true,
                    &mut progress,
                )
            })
            .map_err(|e| format!("Downloading the update failed: {e:#}"))?;
        log::info!(
            "update {} downloaded and verified: {}",
            release.version,
            installer.display()
        );
        Ok(PreparedUpdate::owning_temp_dir(
            release.version.to_string(),
            installer,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    const FEED: &str = r#"{
      "version": "0.3.0",
      "notes": "See https://github.com/CubityFirst/Witness/releases/tag/v0.3.0",
      "pub_date": "2026-10-01T22:22:14Z",
      "platforms": {
        "windows-x86_64": { "signature": "plain-sig", "url": "https://example.com/plain.exe" },
        "windows-x86_64-nsis": { "signature": "nsis-sig", "url": "https://example.com/nsis.exe" }
      }
    }"#;

    // --- feed parsing ------------------------------------------------------

    #[test]
    fn feed_prefers_the_nsis_platform_key() {
        let release = parse_feed(FEED, &v("0.2.1")).unwrap().unwrap();
        assert_eq!(release.version, v("0.3.0"));
        assert_eq!(release.url, "https://example.com/nsis.exe");
        assert_eq!(release.signature, "nsis-sig");
        assert_eq!(release.date.as_deref(), Some("2026-10-01"));
        assert_eq!(
            release.notes.as_deref(),
            Some("See https://github.com/CubityFirst/Witness/releases/tag/v0.3.0")
        );
    }

    #[test]
    fn feed_falls_back_to_the_plain_platform_key() {
        // The shape of the live 0.2.x feed.
        let json = r#"{"version":"0.3.0","pub_date":"2026-09-23T22:22:14Z",
            "platforms":{"windows-x86_64":{"signature":"s","url":"https://example.com/a.exe"}}}"#;
        let release = parse_feed(json, &v("0.2.1")).unwrap().unwrap();
        assert_eq!(release.url, "https://example.com/a.exe");
        assert_eq!(release.notes, None);
    }

    #[test]
    fn feed_accepts_name_alias_and_v_prefix() {
        let json =
            r#"{"name":"v0.3.1","platforms":{"windows-x86_64":{"signature":"s","url":"u"}}}"#;
        let release = parse_feed(json, &v("0.3.0")).unwrap().unwrap();
        assert_eq!(release.version, v("0.3.1"));
        assert_eq!(release.date, None);
        let json =
            r#"{"version":"v0.3.1","platforms":{"windows-x86_64":{"signature":"s","url":"u"}}}"#;
        assert!(parse_feed(json, &v("0.3.0")).unwrap().is_some());
    }

    #[test]
    fn feed_rejects_a_bad_date() {
        let json = r#"{"version":"0.3.0","pub_date":"yesterday",
            "platforms":{"windows-x86_64":{"signature":"s","url":"u"}}}"#;
        let err = parse_feed(json, &v("0.2.1")).unwrap_err().to_string();
        assert!(err.contains("pub_date"), "{err}");
        // A bare date is not RFC 3339 either.
        let json = r#"{"version":"0.3.0","pub_date":"2026-10-01",
            "platforms":{"windows-x86_64":{"signature":"s","url":"u"}}}"#;
        assert!(parse_feed(json, &v("0.2.1")).is_err());
    }

    #[test]
    fn feed_date_keeps_the_feed_offset_date() {
        let json = r#"{"version":"0.3.0","pub_date":"2026-10-01T23:30:00-05:00",
            "platforms":{"windows-x86_64":{"signature":"s","url":"u"}}}"#;
        let release = parse_feed(json, &v("0.2.1")).unwrap().unwrap();
        assert_eq!(release.date.as_deref(), Some("2026-10-01"));
    }

    #[test]
    fn feed_offers_only_strictly_newer_versions() {
        assert!(parse_feed(FEED, &v("0.3.0")).unwrap().is_none());
        assert!(parse_feed(FEED, &v("0.3.1")).unwrap().is_none());
        assert!(parse_feed(FEED, &v("1.0.0")).unwrap().is_none());
        assert!(parse_feed(FEED, &v("0.2.9")).unwrap().is_some());
    }

    #[test]
    fn feed_blank_notes_become_none() {
        let json = r#"{"version":"0.3.0","notes":"  \n ",
            "platforms":{"windows-x86_64":{"signature":"s","url":"u"}}}"#;
        assert_eq!(parse_feed(json, &v("0.2.1")).unwrap().unwrap().notes, None);
    }

    #[test]
    fn feed_without_our_platform_is_an_error() {
        let json =
            r#"{"version":"0.3.0","platforms":{"darwin-aarch64":{"signature":"s","url":"u"}}}"#;
        let err = parse_feed(json, &v("0.2.1")).unwrap_err().to_string();
        assert!(err.contains("windows-x86_64"), "{err}");
    }

    #[test]
    fn feed_accepts_the_dynamic_form() {
        let json = r#"{"version":"0.3.0","url":"https://example.com/d.exe","signature":"d"}"#;
        let release = parse_feed(json, &v("0.2.1")).unwrap().unwrap();
        assert_eq!(release.url, "https://example.com/d.exe");
        assert_eq!(release.signature, "d");
        assert!(parse_feed(r#"{"version":"0.3.0","url":"u"}"#, &v("0.2.1")).is_err());
    }

    #[test]
    fn feed_rejects_garbage() {
        assert!(parse_feed("<html>", &v("0.2.1")).is_err());
        assert!(parse_feed(r#"{"version":"latest"}"#, &v("0.2.1")).is_err());
    }

    // --- signatures --------------------------------------------------------

    /// A throwaway key pair, plus its public key as base64 of the key box —
    /// the form the feed config (and `UPDATER_PUBKEY`) carries.
    fn throwaway_key() -> (minisign::KeyPair, String) {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let pk_box = pair.pk.to_box().unwrap().into_string();
        let b64 = base64::engine::general_purpose::STANDARD.encode(pk_box);
        (pair, b64)
    }

    /// `.sig` contents (base64 of the signature box), as the xtask writes them.
    fn sign(pair: &minisign::KeyPair, data: &[u8], trusted_comment: &str) -> String {
        let sig = minisign::sign(
            Some(&pair.pk),
            &pair.sk,
            std::io::Cursor::new(data),
            Some(trusted_comment),
            Some("signature from tauri secret key"),
        )
        .unwrap();
        base64::engine::general_purpose::STANDARD.encode(sig.into_string())
    }

    const DATA: &[u8] = b"MZ pretend installer";

    #[test]
    fn signature_good_with_signed_version() {
        let (pair, pk) = throwaway_key();
        let sig = sign(&pair, DATA, "timestamp:1\tfile:x.exe\tversion:0.3.0");
        verify(DATA, &sig, &pk, &v("0.3.0"), true).unwrap();
        verify(DATA, &sig, &pk, &v("0.3.0"), false).unwrap();
    }

    #[test]
    fn signature_rejects_tampered_data() {
        let (pair, pk) = throwaway_key();
        let sig = sign(&pair, DATA, "timestamp:1\tfile:x.exe\tversion:0.3.0");
        let mut tampered = DATA.to_vec();
        tampered[5] ^= 1;
        assert!(verify(&tampered, &sig, &pk, &v("0.3.0"), true).is_err());
    }

    #[test]
    fn signature_rejects_a_tampered_trusted_comment() {
        // Rewriting the trusted comment (to claim another version) must break
        // the global signature.
        let (pair, pk) = throwaway_key();
        let sig = sign(&pair, DATA, "timestamp:1\tfile:x.exe\tversion:0.3.0");
        let text = base64_to_string(&sig)
            .unwrap()
            .replace("version:0.3.0", "version:0.9.0");
        let forged = base64::engine::general_purpose::STANDARD.encode(text);
        assert!(verify(DATA, &forged, &pk, &v("0.9.0"), true).is_err());
    }

    #[test]
    fn signature_rejects_the_wrong_key() {
        let (pair, _) = throwaway_key();
        let (_, other_pk) = throwaway_key();
        let sig = sign(&pair, DATA, "timestamp:1\tfile:x.exe\tversion:0.3.0");
        assert!(verify(DATA, &sig, &other_pk, &v("0.3.0"), true).is_err());
        // ...including the real key: no throwaway key may pass as ours.
        assert!(verify(DATA, &sig, UPDATER_PUBKEY, &v("0.3.0"), false).is_err());
    }

    #[test]
    fn signature_rejects_a_version_mismatch() {
        let (pair, pk) = throwaway_key();
        let sig = sign(&pair, DATA, "timestamp:1\tfile:x.exe\tversion:0.3.0");
        // Rejected regardless of the opt-in: the signature says what it covers.
        assert!(verify(DATA, &sig, &pk, &v("0.3.1"), true).is_err());
        assert!(verify(DATA, &sig, &pk, &v("0.3.1"), false).is_err());
    }

    #[test]
    fn signature_missing_version_only_passes_when_not_required() {
        // tauri-cli 2.11.4's comment carries no version (the live 0.2.1 .sig).
        let (pair, pk) = throwaway_key();
        let sig = sign(
            &pair,
            DATA,
            "timestamp:1790202134\tfile:Witness_0.2.1_x64-setup.exe",
        );
        verify(DATA, &sig, &pk, &v("0.2.1"), false).unwrap();
        let err = verify(DATA, &sig, &pk, &v("0.2.1"), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not specify the version"), "{err}");
    }

    #[test]
    fn signature_rejects_malformed_input() {
        let (_, pk) = throwaway_key();
        assert!(verify(DATA, "not base64!", &pk, &v("0.3.0"), false).is_err());
        let junk = base64::engine::general_purpose::STANDARD.encode("junk");
        assert!(verify(DATA, &junk, &pk, &v("0.3.0"), false).is_err());
    }

    #[test]
    fn pubkey_constant_is_key_5a06f76d00e6acd8() {
        let text = base64_to_string(UPDATER_PUBKEY).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("untrusted comment: minisign public key: 5A06F76D00E6ACD8")
        );
        // The key line is algorithm (2 bytes) + key id (8 bytes, LE) + key.
        let raw = base64::engine::general_purpose::STANDARD
            .decode(lines.next().unwrap())
            .unwrap();
        assert_eq!(&raw[..2], b"Ed");
        let key_id = u64::from_le_bytes(raw[2..10].try_into().unwrap());
        assert_eq!(format!("{key_id:016X}"), "5A06F76D00E6ACD8");
        minisign_verify::PublicKey::decode(&text).expect("decodes as a minisign key");
    }

    // Ported from tauri-plugin-updater 2.12.0's own tests.
    const CURRENT: &str = "timestamp:1700000000\tfile:app_1.2.3_x64.msi.zip\tversion:1.2.3";
    // signatures produced before the CLI started embedding the version
    const LEGACY: &str = "timestamp:1600000000\tfile:app_1.0.0_x64.msi.zip";

    #[test]
    fn reads_the_signed_version() {
        assert_eq!(signed_version(CURRENT), Some("1.2.3"));
        assert_eq!(signed_version(LEGACY), None);
        // must not match on a field that merely ends in `version:`
        assert_eq!(signed_version("timestamp:1\tfile:app-version:2.zip"), None);
    }

    #[test]
    fn accepts_a_matching_version() {
        assert!(verify_signed_version(CURRENT, "1.2.3", true).is_ok());
        assert!(verify_signed_version(CURRENT, "1.2.3", false).is_ok());
        // the endpoint and the CLI may spell the same version differently
        assert!(verify_signed_version(CURRENT, "v1.2.3", true).is_ok());
    }

    #[test]
    fn rejects_a_version_the_artifact_was_not_signed_for() {
        // the rollback the check exists to stop: an old artifact announced as a new version
        let err = verify_signed_version(CURRENT, "9.9.9", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("signed for version 1.2.3") && err.contains("announced version 9.9.9"),
            "unexpected error: {err}"
        );
        assert!(verify_signed_version(CURRENT, "9.9.9", true).is_err());
        assert!(verify_signed_version(CURRENT, "1.2.4", true).is_err());
    }

    #[test]
    fn only_requires_a_signed_version_when_configured() {
        assert!(verify_signed_version(LEGACY, "9.9.9", false).is_ok());
        assert!(verify_signed_version(LEGACY, "9.9.9", true).is_err());
    }

    #[test]
    fn compares_non_semver_versions_literally() {
        let comment = "timestamp:1700000000\tfile:app.zip\tversion:2024-01-01";
        assert!(verify_signed_version(comment, "2024-01-01", true).is_ok());
        assert!(verify_signed_version(comment, "2024-01-02", true).is_err());
    }

    // --- installer arguments -----------------------------------------------

    // The plugin's own test vectors (updater.rs `it_escapes_correctly_for_nsis`).
    #[test]
    #[cfg(windows)]
    fn it_escapes_correctly_for_nsis() {
        let cases = [
            "something",
            "--flag",
            "--empty=",
            "--arg=value",
            "some space",                     // This simulates `./my-app "some string"`.
            "--arg value", // -> This simulates `./my-app "--arg value"`. Same as above but it triggers the startsWith(`-`) logic.
            "--arg=unwrapped space", // `./my-app --arg="unwrapped space"`
            "--arg=\"wrapped\"", // `./my-app --args=""wrapped""`
            "--arg=\"wrapped space\"", // `./my-app --args=""wrapped space""`
            "--arg=midword\"wrapped space\"", // `./my-app --args=midword""wrapped""`
            "",            // `./my-app '""'`
        ];
        // Note: These may not be the results we actually want (monitor this!).
        // We only make sure the implementation doesn't unintentionally change.
        let cases_escaped = [
            "something",
            "--flag",
            "--empty=",
            "--arg=value",
            "\"some space\"",
            "\"--arg value\"",
            "\"--arg=unwrapped space\"",
            "--arg=\\\"wrapped\\\"",
            "\"--arg=\\\"wrapped space\\\"\"",
            "\"--arg=midword\\\"wrapped space\\\"\"",
            "\"\"",
        ];

        // Just to be sure we didn't mess that up
        assert_eq!(cases.len(), cases_escaped.len());

        for (orig, escaped) in cases.iter().zip(cases_escaped) {
            assert_eq!(escape_nsis_current_exe_arg(OsStr::new(orig)), escaped);
        }
    }

    #[test]
    #[cfg(windows)]
    fn escapes_slashes_and_trailing_backslashes() {
        // `/` would read as an NSIS switch; inside quotes a trailing
        // backslash is doubled so it cannot escape the closing quote.
        assert_eq!(escape_nsis_current_exe_arg("/S"), "\"/S\"");
        assert_eq!(escape_nsis_current_exe_arg(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(escape_nsis_current_exe_arg(r"C:\ab\"), r"C:\ab\");
    }

    #[test]
    fn nsis_parameters_match_the_plugin() {
        assert_eq!(nsis_parameters(&[]), "/P /UPDATE /R /ARGS");
        assert_eq!(
            nsis_parameters(&["--minimized".into()]),
            "/P /UPDATE /R /ARGS --minimized"
        );
        #[cfg(windows)]
        assert_eq!(
            nsis_parameters(&["--minimized".into(), "a b".into()]),
            "/P /UPDATE /R /ARGS --minimized \"a b\""
        );
    }

    // --- WinINet proxy -----------------------------------------------------

    #[test]
    fn proxy_server_forms() {
        assert_eq!(
            https_proxy_server("proxy:8080").as_deref(),
            Some("proxy:8080")
        );
        assert_eq!(
            https_proxy_server(" http://proxy.corp:3128 ").as_deref(),
            Some("http://proxy.corp:3128")
        );
        assert_eq!(
            https_proxy_server("http=web:80;https=secure:443;ftp=f:21").as_deref(),
            Some("secure:443")
        );
        assert_eq!(
            https_proxy_server("HTTPS=secure:443;http=web:80").as_deref(),
            Some("secure:443")
        );
        assert_eq!(
            https_proxy_server("http=web:80;ftp=f:21").as_deref(),
            Some("web:80")
        );
        assert_eq!(https_proxy_server("ftp=f:21;socks=s:1080"), None);
        assert_eq!(https_proxy_server("https=;http="), None);
        assert_eq!(https_proxy_server("  "), None);
    }

    #[test]
    fn proxy_override_globs() {
        let overrides = "*.corp.example;10.*;<local>; INTRANET ;";
        assert!(bypasses_proxy(overrides, "build.corp.example"));
        assert!(!bypasses_proxy(overrides, "corp.example"));
        assert!(bypasses_proxy(overrides, "10.1.2.3"));
        assert!(bypasses_proxy(overrides, "intranet"));
        assert!(bypasses_proxy(overrides, "wiki")); // <local>: dotless
        assert!(!bypasses_proxy(overrides, "github.com"));
        assert!(bypasses_proxy("*github*", "objects.githubusercontent.com"));
        assert!(bypasses_proxy("GitHub.com", "github.com"));
        assert!(bypasses_proxy("https://github.com", "github.com"));
        assert!(bypasses_proxy("git?ub.com", "github.com"));
        assert!(!bypasses_proxy("", "github.com"));
        assert!(bypasses_proxy("*", "github.com"));
    }

    #[test]
    fn wininet_proxy_bypasses_only_when_every_update_host_matches() {
        let proxy = SystemProxy::Use("proxy:8080".into());
        assert_eq!(wininet_proxy("proxy:8080", "<local>", &UPDATE_HOSTS), proxy);
        // github.com alone bypassed: the CDN still needs the proxy.
        assert_eq!(
            wininet_proxy("proxy:8080", "github.com", &UPDATE_HOSTS),
            proxy
        );
        // Every host bypassed: an explicit "go direct", which the agent
        // must not turn into an environment proxy.
        assert_eq!(
            wininet_proxy(
                "proxy:8080",
                "github.com;*.githubusercontent.com",
                &UPDATE_HOSTS
            ),
            SystemProxy::Direct
        );
        assert_eq!(
            wininet_proxy("ftp=f:21", "", &UPDATE_HOSTS),
            SystemProxy::None
        );
    }

    // --- environment proxy -------------------------------------------------

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn env_proxy_reads_https_then_all_proxy_only() {
        let hosts = &UPDATE_HOSTS;
        assert_eq!(env_proxy("test", env_of(&[]), hosts), None);
        // HTTP_PROXY is for plain-HTTP requests; ours are all HTTPS.
        assert_eq!(
            env_proxy("test", env_of(&[("HTTP_PROXY", "h:1")]), hosts),
            None
        );
        assert_eq!(
            env_proxy(
                "test",
                env_of(&[("ALL_PROXY", "a:1"), ("HTTPS_PROXY", "s:1")]),
                hosts
            )
            .as_deref(),
            Some("s:1")
        );
        assert_eq!(
            env_proxy(
                "test",
                env_of(&[("all_proxy", " a:1 "), ("HTTPS_PROXY", "  ")]),
                hosts
            )
            .as_deref(),
            Some("a:1")
        );
        assert_eq!(
            env_proxy(
                "test",
                env_of(&[("https_proxy", "l:1"), ("HTTPS_PROXY", "u:1")]),
                hosts
            )
            .as_deref(),
            Some("u:1")
        );
    }

    #[test]
    fn env_proxy_honours_no_proxy_for_every_update_host() {
        let hosts = &UPDATE_HOSTS;
        let with = |no_proxy: &'static str| {
            env_proxy(
                "test",
                env_of(&[("HTTPS_PROXY", "p:1"), ("NO_PROXY", no_proxy)]),
                hosts,
            )
        };
        assert_eq!(with("*"), None);
        assert_eq!(with("github.com, githubusercontent.com"), None);
        assert_eq!(with("GitHub.com,.githubusercontent.com"), None);
        assert_eq!(with("github.com,*.githubusercontent.com"), None);
        // Only part of the redirect chain bypassed: keep the proxy.
        assert_eq!(with("github.com").as_deref(), Some("p:1"));
        assert_eq!(with("localhost,127.0.0.1").as_deref(), Some("p:1"));
        assert_eq!(
            env_proxy(
                "test",
                env_of(&[("HTTPS_PROXY", "p:1"), ("no_proxy", "*")]),
                hosts
            ),
            None
        );
    }

    #[test]
    fn no_proxy_domain_rules() {
        assert!(no_proxy_matches("github.com", "github.com"));
        assert!(no_proxy_matches("github.com", "api.github.com"));
        assert!(!no_proxy_matches("github.com", "notgithub.com"));
        assert!(no_proxy_matches(".github.com", "github.com"));
        assert!(no_proxy_matches(".github.com", "a.b.github.com"));
        assert!(no_proxy_matches("*.github.com", "a.github.com"));
        assert!(!no_proxy_matches("hub.com", "github.com"));
        assert!(!no_proxy_matches("", "github.com"));
        assert!(!no_proxy_matches(" , .", "github.com"));
        assert!(no_proxy_matches("x , * ", "github.com"));
    }

    // --- user-facing HTTP errors -------------------------------------------

    #[test]
    fn status_errors_use_the_plugin_wording_without_the_url() {
        let response = ureq::Response::new(403, "Forbidden", "denied").unwrap();
        let error = http_error("Download", ureq::Error::Status(403, response));
        assert_eq!(
            format!("{error:#}"),
            "Download request failed with status: 403 Forbidden"
        );
    }

    #[test]
    fn transport_errors_name_the_cause_once_and_no_url() {
        // Nothing listens on port 1: connection refused, fast and offline.
        let agent = ureq::AgentBuilder::new().try_proxy_from_env(false).build();
        let error = agent
            .get("http://127.0.0.1:1/asset?sp=r&sig=SECRET")
            .call()
            .unwrap_err();
        let source = std::error::Error::source(&error)
            .expect("a transport error has a cause")
            .to_string();
        let text = format!("{:#}", http_error("Update check", error));
        assert!(text.starts_with("Update check request failed: "), "{text}");
        assert!(
            !text.contains("http://") && !text.contains("SECRET"),
            "{text}"
        );
        assert_eq!(text.matches(source.as_str()).count(), 1, "{text}");
    }

    #[test]
    fn glob_edge_cases() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "a"));
        assert!(glob_match("**", ""));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("*.com", "x.y.com"));
    }

    // --- download into %TEMP% ----------------------------------------------

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "witness-updater-{tag}-{}-{}",
            std::process::id(),
            random_suffix(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect()
    }

    fn release_for(signature: String, version: &str) -> Release {
        Release {
            version: v(version),
            notes: None,
            date: None,
            url: "http://127.0.0.1:9/unused".into(),
            signature,
        }
    }

    #[test]
    fn temp_dir_is_removed_after_a_verification_failure() {
        let root = scratch_dir("badsig");
        let (pair, pk) = throwaway_key();
        // Signed for another payload.
        let sig = sign(&pair, b"MZ something else", "timestamp:1\tversion:0.3.0");
        let err = save_and_verify(
            &mut std::io::Cursor::new(DATA),
            Some(DATA.len() as u64),
            &release_for(sig, "0.3.0"),
            &root,
            &pk,
            true,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("signature"), "{err:#}");
        assert!(
            entries(&root).is_empty(),
            "left behind: {:?}",
            entries(&root)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn temp_dir_is_removed_for_a_non_exe_and_a_short_body() {
        let root = scratch_dir("notexe");
        let (pair, pk) = throwaway_key();
        let html = b"<html>rate limited</html>";
        let sig = sign(&pair, html, "timestamp:1\tversion:0.3.0");
        let err = save_and_verify(
            &mut std::io::Cursor::new(html),
            None,
            &release_for(sig, "0.3.0"),
            &root,
            &pk,
            true,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a Windows installer"), "{err}");
        assert!(entries(&root).is_empty());

        // The connection closed before Content-Length bytes arrived.
        let sig = sign(&pair, DATA, "timestamp:1\tversion:0.3.0");
        let err = save_and_verify(
            &mut std::io::Cursor::new(DATA),
            Some(DATA.len() as u64 + 10),
            &release_for(sig, "0.3.0"),
            &root,
            &pk,
            true,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("ended early"), "{err}");
        assert!(entries(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A body that fails mid-stream, like a reset connection.
    struct Broken(usize);
    impl Read for Broken {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ));
            }
            let n = self.0.min(buf.len());
            buf[..n].fill(b'M');
            self.0 -= n;
            Ok(n)
        }
    }

    #[test]
    fn temp_dir_is_removed_after_a_network_error() {
        let root = scratch_dir("reset");
        let (_, pk) = throwaway_key();
        let err = save_and_verify(
            &mut Broken(100_000),
            None,
            &release_for("unused".into(), "0.3.0"),
            &root,
            &pk,
            true,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("interrupted"), "{err:#}");
        assert!(entries(&root).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn verified_installer_is_kept_with_the_plugin_layout() {
        let root = scratch_dir("good");
        let (pair, pk) = throwaway_key();
        // 2.5 MiB: two step events plus the final one.
        let mut payload = b"MZ".to_vec();
        payload.resize(5 << 19, 7);
        let sig = sign(&pair, &payload, "timestamp:1\tfile:x.exe\tversion:0.3.0");
        let mut events = Vec::new();
        let installer = save_and_verify(
            &mut std::io::Cursor::new(&payload),
            Some(payload.len() as u64),
            &release_for(sig, "0.3.0"),
            &root,
            &pk,
            true,
            &mut |done, total| events.push((done, total)),
        )
        .unwrap();
        let total = Some(payload.len() as u64);
        assert_eq!(
            events,
            vec![
                (1 << 20, total),
                (2 << 20, total),
                (payload.len() as u64, total)
            ]
        );
        assert_eq!(std::fs::read(&installer).unwrap(), payload);
        assert_eq!(
            installer.file_name().unwrap().to_string_lossy(),
            "Witness-0.3.0-installer.exe"
        );
        let dir = installer.parent().unwrap();
        let dir_name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(dir_name.starts_with("Witness-0.3.0-updater-"), "{dir_name}");
        assert_eq!(dir_name.len(), "Witness-0.3.0-updater-".len() + 6);
        assert_eq!(dir.parent().unwrap(), root);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn temp_dir_names_do_not_collide() {
        let root = scratch_dir("names");
        let a = create_temp_dir(&root, &v("1.0.0")).unwrap();
        let b = create_temp_dir(&root, &v("1.0.0")).unwrap();
        assert_ne!(a, b);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Serve `body` once over plain HTTP on localhost; returns its URL.
    fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/Witness_0.3.0_x64-setup.exe",
            listener.local_addr().unwrap()
        );
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Consume the request head before answering.
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        });
        url
    }

    #[test]
    fn download_release_streams_verifies_and_reports_progress() {
        let root = scratch_dir("http");
        let (pair, pk) = throwaway_key();
        let mut payload = b"MZ".to_vec();
        payload.resize(3 << 20, 1);
        let sig = sign(&pair, &payload, "timestamp:1\tversion:0.3.0");
        let mut release = release_for(sig, "0.3.0");
        release.url = serve_once(payload.clone());
        // A bare agent: the local server must not go through any proxy.
        let agent = ureq::AgentBuilder::new().try_proxy_from_env(false).build();
        let mut events = Vec::new();
        let installer = download_release(&agent, &release, &root, &pk, true, &mut |d, t| {
            events.push((d, t))
        })
        .unwrap();
        assert_eq!(std::fs::read(&installer).unwrap(), payload);
        // A socket read can return less than the 64 KiB buffer, so emits
        // land at least PROGRESS_STEP apart, not on exact 1 MiB boundaries.
        let done: Vec<u64> = events.iter().map(|&(d, _)| d).collect();
        assert!(
            events.iter().all(|&(_, t)| t == Some(3 << 20)),
            "{events:?}"
        );
        assert_eq!(done.len(), 3, "{events:?}");
        assert!(
            (1 << 20..(1 << 20) + 64 * 1024).contains(&done[0]),
            "{events:?}"
        );
        assert!(done[1] - done[0] >= PROGRESS_STEP, "{events:?}");
        assert_eq!(done[2], 3 << 20);
        std::fs::remove_dir_all(&root).unwrap();
    }

    // --- AppCore surface ---------------------------------------------------

    /// Best-effort cleanup of `scratch_core` data dirs. The core's worker
    /// threads keep its database open (and the process-wide logger the first
    /// core's witness.log) until the test process exits, so this run's dir
    /// usually survives; the dirs that earlier runs of this `tag` left are
    /// free by now and removed here, so at most one stays in %TEMP%.
    fn discard_scratch_core(core: Arc<AppCore>, dir: PathBuf, tag: &str) {
        drop(core);
        let _ = std::fs::remove_dir_all(&dir);
        let prefix = format!("witness-core-{tag}-");
        let own_pid = std::process::id().to_string();
        let Ok(siblings) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in siblings.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((pid, _nanos)) = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.split_once('-'))
            else {
                continue;
            };
            if pid != own_pid && pid.bytes().all(|b| b.is_ascii_digit()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    #[test]
    fn core_pending_update_and_download_preconditions() {
        let (core, _sink, dir) = super::super::testing::scratch_core("updater");
        assert!(core.pending_update().is_none());
        assert_eq!(
            core.download_update().unwrap_err(),
            "No update is pending. Check for updates first"
        );
        *core.pending_update.lock().unwrap() = parse_feed(FEED, &v("0.0.1")).unwrap();
        let info = core.pending_update().unwrap();
        assert_eq!(info.version, "0.3.0");
        assert_eq!(info.current_version, env!("CARGO_PKG_VERSION"));
        // A busy recorder refuses before any network traffic.
        *core.state.recorder.lock().unwrap() = crate::state::RecorderState::Starting;
        assert_eq!(
            core.download_update().unwrap_err(),
            "Stop the recording before installing the update"
        );
        *core.state.recorder.lock().unwrap() = crate::state::RecorderState::Idle;
        discard_scratch_core(core, dir, "updater");
    }

    /// A downloaded-and-verified layout under `root`, owned like
    /// `download_update` returns it.
    fn prepared_in(root: &Path) -> PreparedUpdate {
        let dir = create_temp_dir(root, &v("0.3.0")).unwrap();
        let installer = dir.join("Witness-0.3.0-installer.exe");
        std::fs::write(&installer, b"MZ").unwrap();
        PreparedUpdate::owning_temp_dir("0.3.0".into(), installer)
    }

    #[test]
    fn unlaunched_update_removes_its_temp_dir_on_drop() {
        let root = scratch_dir("discard");
        let prepared = prepared_in(&root);
        assert!(prepared.installer.exists());
        drop(prepared);
        assert!(
            entries(&root).is_empty(),
            "left behind: {:?}",
            entries(&root)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn failed_launch_keeps_ownership_until_drop() {
        let root = scratch_dir("launchfail");
        let prepared = prepared_in(&root);
        // Make ShellExecute fail: the installer is gone, its dir remains.
        std::fs::remove_file(&prepared.installer).unwrap();
        let dir = prepared.installer.parent().unwrap().to_path_buf();
        assert!(prepared.launch(&[]).is_err());
        assert!(dir.exists());
        drop(prepared);
        assert!(!dir.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    // --- network (ignored) -------------------------------------------------

    /// The live feed through the real agent (SChannel + system proxy).
    #[test]
    #[ignore = "network: GitHub releases"]
    fn live_feed_check() {
        let agent = agent().unwrap();
        let feed: serde_json::Value = serde_json::from_str(
            &agent
                .get(UPDATE_ENDPOINT)
                .call()
                .unwrap()
                .into_string()
                .unwrap(),
        )
        .unwrap();
        let live = v(feed["version"].as_str().unwrap().trim_start_matches('v'));
        let current = v(env!("CARGO_PKG_VERSION"));

        let found = fetch_release(&agent, UPDATE_ENDPOINT, &current).unwrap();
        assert_eq!(
            found.is_some(),
            live > current,
            "live {live}, current {current}"
        );
        // From an older client the live release is always offered.
        let old = fetch_release(&agent, UPDATE_ENDPOINT, &v("0.1.0"))
            .unwrap()
            .unwrap();
        assert_eq!(old.version, live);
        assert!(old
            .url
            .starts_with("https://github.com/CubityFirst/Witness/releases/download/"));

        // And through AppCore, which also records the pending release.
        let (core, _sink, dir) = super::super::testing::scratch_core("live-update");
        let info = core.check_for_update().unwrap();
        assert_eq!(info.is_some(), live > current);
        assert_eq!(
            core.pending_update().map(|i| i.version),
            info.map(|i| i.version)
        );
        discard_scratch_core(core, dir, "live-update");
    }

    /// Downloads the real, Tauri-signed 0.2.1 installer and verifies it
    /// against the embedded key: proves the pubkey constant and the verifier
    /// on a genuine artefact. Its signature carries no `version:`, so the
    /// native updater's strict mode must refuse it.
    #[test]
    #[ignore = "network: downloads the ~100 MB 0.2.1 installer"]
    fn real_021_installer_verifies() {
        let agent = agent().unwrap();
        // The v0.2.1 release's own feed, so this keeps working after newer
        // releases become `latest`.
        let json = agent
            .get("https://github.com/CubityFirst/Witness/releases/download/v0.2.1/latest.json")
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        let release = parse_feed(&json, &v("0.0.1")).unwrap().unwrap();
        assert_eq!(release.version, v("0.2.1"));
        let root = scratch_dir("real021");
        let mut last = None;
        let installer = download_release(
            &agent,
            &release,
            &root,
            UPDATER_PUBKEY,
            false,
            &mut |d, t| last = Some((d, t)),
        )
        .unwrap();
        let size = std::fs::metadata(&installer).unwrap().len();
        assert_eq!(last, Some((size, Some(size))));
        let bytes = std::fs::read(&installer).unwrap();
        assert!(verify(
            &bytes,
            &release.signature,
            UPDATER_PUBKEY,
            &release.version,
            true
        )
        .is_err());
        let mut tampered = bytes;
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0x55;
        assert!(verify(
            &tampered,
            &release.signature,
            UPDATER_PUBKEY,
            &release.version,
            false
        )
        .is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
