//! Updater signatures and the update feed (design §8.3).
//!
//! Signing is byte-compatible with tauri-cli 2.11.4 (same `minisign =0.7.3`,
//! same key decoding, same untrusted comment, same base64 wrapping), with one
//! deliberate addition: the trusted comment carries `version:{v}`. 0.2.x
//! clients (tauri-plugin-updater 2.12.0) enforce that field only when it is
//! present, and the native updater requires it, so every signature from here
//! on carries it. Before anything is written, each signature is verified with
//! `minisign-verify 0.2.5` (the clients' verifier) against the release key.

use crate::legacy;
use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// tauri-cli's untrusted comment, kept so `.sig` files look the same.
pub const UNTRUSTED_COMMENT: &str = "signature from tauri secret key";

/// Platform keys written into `latest.json`. 0.2.x looks up
/// `windows-x86_64-nsis` first, then `windows-x86_64` (design §8.1).
pub const FEED_PLATFORMS: [&str; 2] = ["windows-x86_64", "windows-x86_64-nsis"];

/// Decode `TAURI_SIGNING_PRIVATE_KEY` (key text, or a path to a file holding
/// it, as tauri-cli accepts) or `TAURI_SIGNING_PRIVATE_KEY_PATH`, with
/// `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (empty when unset: the release key
/// has no password).
pub fn load_secret_key() -> Result<minisign::SecretKey> {
    let raw = match std::env::var("TAURI_SIGNING_PRIVATE_KEY") {
        Ok(v) if !v.trim().is_empty() => {
            let p = Path::new(v.trim());
            if p.is_file() {
                std::fs::read_to_string(p)
                    .with_context(|| format!("read the key file {}", p.display()))?
            } else {
                v
            }
        }
        _ => match std::env::var("TAURI_SIGNING_PRIVATE_KEY_PATH") {
            Ok(path) if !path.trim().is_empty() => std::fs::read_to_string(path.trim())
                .with_context(|| format!("read TAURI_SIGNING_PRIVATE_KEY_PATH {path}"))?,
            _ => bail!(
                "signing needs TAURI_SIGNING_PRIVATE_KEY (or TAURI_SIGNING_PRIVATE_KEY_PATH): \
                 the base64 minisign secret key, ~/.tauri/witness-updater.key"
            ),
        },
    };
    let password = std::env::var("TAURI_SIGNING_PRIVATE_KEY_PASSWORD").unwrap_or_default();
    decode_secret_key(&raw, &password)
}

/// base64 → UTF-8 minisign secret-key box → secret key (tauri-cli's
/// `secret_key()`).
pub fn decode_secret_key(key_b64: &str, password: &str) -> Result<minisign::SecretKey> {
    let text = String::from_utf8(
        BASE64
            .decode(key_b64.trim())
            .context("the signing key is not base64")?,
    )
    .context("the signing key is not UTF-8 after base64 decoding")?;
    let sk_box = minisign::SecretKeyBox::from_string(&text)
        .map_err(|e| anyhow::anyhow!("not a minisign secret key: {e}"))?;
    sk_box
        .into_secret_key(Some(password.to_string()))
        .map_err(|e| anyhow::anyhow!("cannot decrypt the signing key (wrong password?): {e}"))
}

/// Key ID (upper-case hex, as minisign prints it) of a key number.
fn key_id_hex(keynum_le: &[u8]) -> String {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&keynum_le[..8]);
    format!("{:016X}", u64::from_le_bytes(bytes))
}

/// Key ID of a base64 public-key box (the `pubkey` format of tauri.conf.json).
pub fn pubkey_key_id(pubkey_b64: &str) -> Result<String> {
    let text = String::from_utf8(BASE64.decode(pubkey_b64.trim())?)?;
    let line = text
        .lines()
        .nth(1)
        .context("public key box has no key line")?;
    let raw = BASE64.decode(line.trim())?;
    ensure!(
        raw.len() == 42 && &raw[..2] == b"Ed",
        "not an Ed25519 minisign public key"
    );
    Ok(key_id_hex(&raw[2..10]))
}

/// Key ID of a secret key.
pub fn secret_key_id(sk: &minisign::SecretKey) -> String {
    key_id_hex(sk.keynum())
}

/// Refuse any key but the release key: installed copies trust only that one.
pub fn ensure_release_key(sk: &minisign::SecretKey) -> Result<()> {
    let id = secret_key_id(sk);
    ensure!(
        id == legacy::UPDATER_KEY_ID,
        "the signing key has ID {id}, but installed copies trust only {} \
         (~/.tauri/witness-updater.key); never rotate it",
        legacy::UPDATER_KEY_ID
    );
    Ok(())
}

/// `timestamp:{t}\tfile:{name}[\tversion:{v}]`. tauri-cli 2.11.4 wrote no
/// `version:`; `None` reproduces that for the compatibility tests only.
pub fn trusted_comment(timestamp: u64, file_name: &str, version: Option<&str>) -> Result<String> {
    // Tabs and newlines would corrupt the tab-separated fields / the box.
    for (what, value) in [("file name", file_name), ("version", version.unwrap_or(""))] {
        ensure!(
            !value.contains(['\t', '\n', '\r']),
            "the {what} must not contain tabs or newlines: {value:?}"
        );
    }
    Ok(match version {
        Some(v) => format!("timestamp:{timestamp}\tfile:{file_name}\tversion:{v}"),
        None => format!("timestamp:{timestamp}\tfile:{file_name}"),
    })
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Sign `data` (the installer bytes, named `file_name`) and return the
/// `.sig` text: base64 of the minisign signature box, exactly like
/// tauri-cli's `sign_file`.
pub fn sign_bytes(
    sk: &minisign::SecretKey,
    data: &[u8],
    file_name: &str,
    version: Option<&str>,
    timestamp: u64,
) -> Result<String> {
    let comment = trusted_comment(timestamp, file_name, version)?;
    let sig_box = minisign::sign(None, sk, data, Some(&comment), Some(UNTRUSTED_COMMENT))
        .map_err(|e| anyhow::anyhow!("minisign: {e}"))?;
    Ok(BASE64.encode(sig_box.into_string()))
}

/// Verify a `.sig` the way the clients will: minisign-verify 0.2.5 with
/// legacy signatures allowed (plugin 2.12.0), then the signed `version:`
/// must be present and equal `version` (the native updater's stricter rule).
pub fn self_check(data: &[u8], sig_b64: &str, pubkey_b64: &str, version: &str) -> Result<()> {
    let pk_text = String::from_utf8(BASE64.decode(pubkey_b64.trim())?)?;
    let pk = minisign_verify::PublicKey::decode(&pk_text)
        .map_err(|e| anyhow::anyhow!("bad public key: {e}"))?;
    let sig_text = String::from_utf8(BASE64.decode(sig_b64.trim())?)?;
    let sig = minisign_verify::Signature::decode(&sig_text)
        .map_err(|e| anyhow::anyhow!("bad signature: {e}"))?;
    pk.verify(data, &sig, true)
        .map_err(|e| anyhow::anyhow!("signature does not verify against the public key: {e}"))?;
    let signed = sig
        .trusted_comment()
        .split('\t')
        .find_map(|f| f.strip_prefix("version:"))
        .context("the trusted comment carries no version")?;
    let parse = |s: &str| semver::Version::parse(s.trim_start_matches('v'));
    ensure!(
        parse(signed)? == parse(version)?,
        "signed version {signed} != feed version {version}"
    );
    Ok(())
}

/// One platform entry of `latest.json`.
#[derive(Debug, Clone, Serialize)]
pub struct FeedPlatform {
    pub signature: String,
    pub url: String,
}

/// `latest.json`, field order as the 0.2.x release script wrote it.
#[derive(Debug, Clone, Serialize)]
pub struct Feed {
    pub version: String,
    pub notes: String,
    pub pub_date: String,
    pub platforms: BTreeMap<String, FeedPlatform>,
}

impl Feed {
    pub fn new(version: &str, notes: &str, pub_date: String, entry: FeedPlatform) -> Self {
        let platforms = FEED_PLATFORMS
            .iter()
            .map(|k| (k.to_string(), entry.clone()))
            .collect();
        Self {
            version: version.to_string(),
            notes: notes.to_string(),
            pub_date,
            platforms,
        }
    }

    /// The exact bytes of the 0.2.x feeds (PowerShell 7 `ConvertTo-Json`):
    /// two-space indent, CRLF, no trailing newline, UTF-8 without BOM (the
    /// updater's JSON parser rejects a BOM).
    pub fn render(&self) -> String {
        let pretty = serde_json::to_string_pretty(self).expect("feed serializes");
        // Raw newlines only occur as formatting: JSON escapes them in strings.
        pretty.replace('\n', "\r\n")
    }
}

/// RFC 3339 UTC, second precision (`2026-09-23T22:22:14Z`).
pub fn pub_date_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Release asset URL. GitHub serves asset names with spaces as dots.
pub fn installer_url(version: &str, file_name: &str) -> String {
    format!(
        "{}/v{version}/{}",
        legacy::RELEASE_DOWNLOAD_BASE,
        file_name.replace(' ', ".")
    )
}

/// A `--url` must be the installer asset itself: an https URL whose last
/// path segment is the file name (or GitHub's space-to-dot form of it). The
/// release *page* URL (`.../releases/tag/vX`) would serve HTML, every client
/// would fail signature verification, and the release would silently do
/// nothing.
pub fn validate_installer_url(url: &str, file_name: &str) -> Result<()> {
    let hint = "omit --url to use the default release asset URL";
    ensure!(
        url.starts_with("https://"),
        "--url {url} is not an https URL; {hint}"
    );
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or("");
    ensure!(
        last == file_name || last == file_name.replace(' ', "."),
        "--url {url} does not point at {file_name} (it must be the release asset's download \
         URL, not the release page); {hint}"
    );
    Ok(())
}

/// Inputs for [`sign_installer`].
pub struct SignRequest {
    pub file: PathBuf,
    pub version: String,
    /// Also write `latest.json` here.
    pub feed: Option<PathBuf>,
    pub notes: Option<String>,
    /// Installer download URL for the feed (default: the release asset URL).
    pub url: Option<String>,
    /// Self-check against this public key instead of the release key
    /// (throwaway-key rehearsals only).
    pub pubkey: Option<String>,
    /// Sign even though the file's VERSIONINFO does not say `version`.
    pub force_version: bool,
}

/// `cargo run -p xtask -- sign`.
pub fn sign_command(req: SignRequest) -> Result<()> {
    let sk = load_secret_key()?;
    sign_installer(&sk, &req)?;
    Ok(())
}

/// Sign `req.file`, self-check, write `<file>.sig` and optionally the feed.
/// Returns the `.sig` path.
pub fn sign_installer(sk: &minisign::SecretKey, req: &SignRequest) -> Result<PathBuf> {
    // The same plain X.Y.Z rule as `version`/`dist` (design §8.4): a semver
    // prerelease would sort below its release on clients and can't be a
    // numeric VIProductVersion anyway.
    let version = crate::workspace::parse_release_version(req.version.trim_start_matches('v'))
        .with_context(|| format!("--version {}", req.version))?
        .to_string();
    let file_name = req
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .context("the file to sign needs a UTF-8 file name")?
        .to_string();
    let data = std::fs::read(&req.file).with_context(|| format!("read {}", req.file.display()))?;
    ensure!(!data.is_empty(), "{} is empty", req.file.display());
    // A signature for the wrong file is self-consistent (signed version ==
    // feed version), so clients would install it, still report the old
    // version, and be offered the same update forever. NSIS installers (ours
    // and Tauri's) carry VIAddVersionKey ProductVersion = the app version.
    if !crate::dist::has_product_version(&data, &version) {
        ensure!(
            req.force_version,
            "{} does not carry VERSIONINFO ProductVersion {version}; is it the right (and a \
             freshly rebuilt) installer? Pass --force-version only if you are sure",
            req.file.display()
        );
        eprintln!(
            "warning: {} has no ProductVersion {version}; signing anyway (--force-version)",
            req.file.display()
        );
    }
    match (&req.url, &req.feed) {
        (Some(url), Some(_)) => validate_installer_url(url, &file_name)?,
        (Some(_), None) => bail!("--url only goes into latest.json; it needs --feed"),
        _ => {}
    }

    let pubkey = match &req.pubkey {
        Some(pk) => {
            eprintln!(
                "warning: self-checking against a NON-RELEASE public key (key ID {}); \
                 installed copies will reject this signature",
                pubkey_key_id(pk)?
            );
            pk.as_str()
        }
        None => {
            ensure_release_key(sk)?;
            legacy::UPDATER_PUBKEY
        }
    };

    let sig = sign_bytes(sk, &data, &file_name, Some(&version), unix_now())?;
    self_check(&data, &sig, pubkey, &version).context("self-check of the new signature failed")?;

    let sig_path = crate::dist::sig_path(&req.file);
    std::fs::write(&sig_path, &sig).with_context(|| format!("write {}", sig_path.display()))?;
    println!(
        "signed {} (version {version}) -> {}",
        file_name,
        sig_path.display()
    );

    if let Some(feed_path) = &req.feed {
        let url = req
            .url
            .clone()
            .unwrap_or_else(|| installer_url(&version, &file_name));
        let feed = Feed::new(
            &version,
            req.notes.as_deref().unwrap_or(""),
            pub_date_now(),
            FeedPlatform {
                signature: sig.clone(),
                url,
            },
        );
        if let Some(dir) = feed_path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(feed_path, feed.render())
            .with_context(|| format!("write {}", feed_path.display()))?;
        println!("update feed -> {}", feed_path.display());
    }
    Ok(sig_path)
}

/// A throwaway key pair in the release formats: the secret key as
/// `TAURI_SIGNING_PRIVATE_KEY` holds it (base64 of an encrypted box with an
/// empty password), the public key as tauri.conf.json's `pubkey`.
pub fn generate_test_key() -> Result<(String, String)> {
    let kp = minisign::KeyPair::generate_encrypted_keypair(Some(String::new()))
        .map_err(|e| anyhow::anyhow!("generate key: {e}"))?;
    let sk_box = kp
        .sk
        .to_box(None)
        .map_err(|e| anyhow::anyhow!("secret key box: {e}"))?;
    let pk_box = kp
        .pk
        .to_box()
        .map_err(|e| anyhow::anyhow!("public key box: {e}"))?;
    Ok((
        BASE64.encode(sk_box.into_string()),
        BASE64.encode(pk_box.into_string()),
    ))
}

/// `cargo run -p xtask -- gen-test-key <dir>`.
pub fn gen_test_key(dir: &Path) -> Result<()> {
    let (sk, pk) = generate_test_key()?;
    std::fs::create_dir_all(dir)?;
    let sk_path = dir.join("test-updater.key");
    let pk_path = dir.join("test-updater.pub");
    std::fs::write(&sk_path, &sk)?;
    std::fs::write(&pk_path, &pk)?;
    println!(
        "throwaway key {} (NOT the release key):\n  secret: {}\n  public: {}\n\
         use: TAURI_SIGNING_PRIVATE_KEY=<secret file> cargo run -p xtask -- sign <file> \
         --version X.Y.Z --feed latest.json --pubkey <contents of the public file>",
        pubkey_key_id(&pk)?,
        sk_path.display(),
        pk_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 0.2.1 release asset, byte for byte (CRLF, no BOM, no newline at EOF).
    const FEED_021: &str = include_str!("../fixtures/latest-0.2.1.json");

    #[test]
    fn feed_bytes_match_the_shipped_021_feed() {
        let shipped: serde_json::Value = serde_json::from_str(FEED_021).unwrap();
        let entry = &shipped["platforms"]["windows-x86_64"];
        let mut feed = Feed::new(
            "0.2.1",
            "See https://github.com/CubityFirst/Witness/releases/tag/v0.2.1",
            "2026-09-23T22:22:14Z".into(),
            FeedPlatform {
                signature: entry["signature"].as_str().unwrap().into(),
                url: installer_url("0.2.1", "Witness_0.2.1_x64-setup.exe"),
            },
        );
        // 0.2.1 published only the generic key; with it alone we must
        // reproduce the shipped bytes exactly.
        feed.platforms.remove("windows-x86_64-nsis");
        assert_eq!(feed.render(), FEED_021);
    }

    #[test]
    fn feed_has_both_platform_keys_and_no_bom() {
        let feed = Feed::new(
            "0.3.0",
            "",
            "2026-10-05T12:00:00Z".into(),
            FeedPlatform {
                signature: "c2ln".into(),
                url: installer_url("0.3.0", "Witness_0.3.0_x64-setup.exe"),
            },
        );
        let text = feed.render();
        assert!(!text.starts_with('\u{feff}'));
        assert!(!text.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], "0.3.0");
        assert_eq!(v["pub_date"], "2026-10-05T12:00:00Z");
        for key in FEED_PLATFORMS {
            assert_eq!(
                v["platforms"][key]["url"],
                "https://github.com/CubityFirst/Witness/releases/download/v0.3.0/Witness_0.3.0_x64-setup.exe"
            );
            assert_eq!(v["platforms"][key]["signature"], "c2ln");
        }
        // Field order of the 0.2.x feeds.
        let order: Vec<_> = ["\"version\"", "\"notes\"", "\"pub_date\"", "\"platforms\""]
            .iter()
            .map(|k| text.find(k).unwrap())
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]));
        chrono::DateTime::parse_from_rfc3339(&pub_date_now()).unwrap();
    }

    #[test]
    fn asset_url_replaces_spaces() {
        assert_eq!(
            installer_url("1.0.0", "Witness Setup.exe"),
            "https://github.com/CubityFirst/Witness/releases/download/v1.0.0/Witness.Setup.exe"
        );
    }

    #[test]
    fn trusted_comment_format() {
        assert_eq!(
            trusted_comment(1790202134, "Witness_0.3.0_x64-setup.exe", Some("0.3.0")).unwrap(),
            "timestamp:1790202134\tfile:Witness_0.3.0_x64-setup.exe\tversion:0.3.0"
        );
        assert_eq!(
            trusted_comment(1, "a.exe", None).unwrap(),
            "timestamp:1\tfile:a.exe"
        );
        assert!(trusted_comment(1, "a\tb.exe", None).is_err());
        assert!(trusted_comment(1, "a.exe", Some("0.3.0\n")).is_err());
    }

    #[test]
    fn release_pubkey_is_the_known_key() {
        assert_eq!(
            pubkey_key_id(legacy::UPDATER_PUBKEY).unwrap(),
            legacy::UPDATER_KEY_ID
        );
    }

    #[test]
    fn sign_then_self_check() {
        let (sk_b64, pk_b64) = generate_test_key().unwrap();
        let sk = decode_secret_key(&sk_b64, "").unwrap();
        assert_eq!(secret_key_id(&sk), pubkey_key_id(&pk_b64).unwrap());
        let data = b"MZ fake installer";
        let sig = sign_bytes(&sk, data, "Witness_0.3.0_x64-setup.exe", Some("0.3.0"), 1).unwrap();
        self_check(data, &sig, &pk_b64, "0.3.0").unwrap();
        self_check(data, &sig, &pk_b64, "v0.3.0").unwrap();
        assert!(self_check(data, &sig, &pk_b64, "0.3.1").is_err());
        assert!(self_check(b"MZ tampered", &sig, &pk_b64, "0.3.0").is_err());
        assert!(self_check(data, &sig, legacy::UPDATER_PUBKEY, "0.3.0").is_err());
        let unversioned = sign_bytes(&sk, data, "x.exe", None, 1).unwrap();
        assert!(self_check(data, &unversioned, &pk_b64, "0.3.0").is_err());
        // The untrusted comment matches tauri-cli's.
        let text = String::from_utf8(BASE64.decode(&sig).unwrap()).unwrap();
        assert!(text.starts_with("untrusted comment: signature from tauri secret key\n"));
    }

    #[test]
    fn wrong_password_is_reported() {
        let (sk_b64, _) = generate_test_key().unwrap();
        assert!(decode_secret_key(&sk_b64, "not-the-password").is_err());
        assert!(decode_secret_key("not base64!", "").is_err());
    }

    /// Fake installer bytes around the real VERSIONINFO resource of the
    /// shipped 0.2.1 installer (ProductVersion 0.2.1).
    fn fake_021_installer() -> Vec<u8> {
        let mut data = b"MZ installer bytes".to_vec();
        data.extend_from_slice(include_bytes!("../fixtures/versioninfo-setup-0.2.1.bin"));
        data
    }

    #[test]
    fn sign_installer_refuses_a_foreign_key_and_writes_sig_and_feed() {
        let dir = std::env::temp_dir().join(format!("xtask-sign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Witness_0.2.1_x64-setup.exe");
        let data = fake_021_installer();
        std::fs::write(&file, &data).unwrap();
        let (sk_b64, pk_b64) = generate_test_key().unwrap();
        let sk = decode_secret_key(&sk_b64, "").unwrap();

        // Without --pubkey only the release key is acceptable.
        let req = |pubkey: Option<String>| SignRequest {
            file: file.clone(),
            version: "0.2.1".into(),
            feed: Some(dir.join("latest.json")),
            notes: Some("notes".into()),
            url: None,
            pubkey,
            force_version: false,
        };
        let err = sign_installer(&sk, &req(None)).unwrap_err().to_string();
        assert!(err.contains(legacy::UPDATER_KEY_ID), "{err}");
        assert!(ensure_release_key(&sk).is_err());

        let sig_path = sign_installer(&sk, &req(Some(pk_b64.clone()))).unwrap();
        let sig = std::fs::read_to_string(&sig_path).unwrap();
        self_check(&data, &sig, &pk_b64, "0.2.1").unwrap();
        let feed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("latest.json")).unwrap()).unwrap();
        assert_eq!(
            feed["platforms"]["windows-x86_64"]["signature"],
            sig.as_str()
        );
        assert_eq!(feed["notes"], "notes");
        assert_eq!(
            feed["platforms"]["windows-x86_64"]["url"],
            installer_url("0.2.1", "Witness_0.2.1_x64-setup.exe")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sign_installer_refuses_a_version_the_file_does_not_carry() {
        let dir = std::env::temp_dir().join(format!("xtask-signv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Witness_0.2.1_x64-setup.exe");
        std::fs::write(&file, fake_021_installer()).unwrap();
        let (sk_b64, pk_b64) = generate_test_key().unwrap();
        let sk = decode_secret_key(&sk_b64, "").unwrap();
        let req = |version: &str, force_version: bool| SignRequest {
            file: file.clone(),
            version: version.into(),
            feed: None,
            notes: None,
            url: None,
            pubkey: Some(pk_b64.clone()),
            force_version,
        };
        // The rollback mistake: an old installer signed as a newer version.
        let err = sign_installer(&sk, &req("0.3.1", false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ProductVersion 0.3.1"), "{err}");
        assert!(!dir.join("Witness_0.2.1_x64-setup.exe.sig").exists());
        // Prerelease/build versions are refused even when forced.
        for bad in ["0.3.1-rc.1", "0.3.1+x", "v0.3.1-beta"] {
            let err = format!("{:#}", sign_installer(&sk, &req(bad, true)).unwrap_err());
            assert!(err.contains("prerelease/build"), "{bad}: {err}");
        }
        assert!(!dir.join("Witness_0.2.1_x64-setup.exe.sig").exists());
        // ... unless explicitly forced.
        sign_installer(&sk, &req("0.3.1", true)).unwrap();
        assert!(dir.join("Witness_0.2.1_x64-setup.exe.sig").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sign_installer_validates_the_url() {
        let dir = std::env::temp_dir().join(format!("xtask-signu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Witness_0.2.1_x64-setup.exe");
        std::fs::write(&file, fake_021_installer()).unwrap();
        let (sk_b64, pk_b64) = generate_test_key().unwrap();
        let sk = decode_secret_key(&sk_b64, "").unwrap();
        let req = |url: &str, feed: bool| SignRequest {
            file: file.clone(),
            version: "0.2.1".into(),
            feed: feed.then(|| dir.join("latest.json")),
            notes: None,
            url: Some(url.into()),
            pubkey: Some(pk_b64.clone()),
            force_version: false,
        };
        let page = "https://github.com/CubityFirst/Witness/releases/tag/v0.2.1";
        let err = sign_installer(&sk, &req(page, true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("omit --url"), "{err}");
        assert!(!dir.join("latest.json").exists());
        let asset = installer_url("0.2.1", "Witness_0.2.1_x64-setup.exe");
        assert!(
            sign_installer(&sk, &req(&asset, false)).is_err(),
            "--url needs --feed"
        );
        sign_installer(&sk, &req(&asset, true)).unwrap();
        assert!(dir.join("latest.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn installer_urls() {
        let name = "Witness_0.3.1_x64-setup.exe";
        validate_installer_url(&installer_url("0.3.1", name), name).unwrap();
        validate_installer_url(
            "https://example.com/a/Witness.Setup.exe",
            "Witness Setup.exe",
        )
        .unwrap();
        validate_installer_url(&format!("https://example.com/{name}?x=1"), name).unwrap();
        for bad in [
            "https://github.com/CubityFirst/Witness/releases/tag/v0.3.1",
            "https://github.com/CubityFirst/Witness/releases/download/v0.3.1/",
            "http://github.com/CubityFirst/Witness/releases/download/v0.3.1/Witness_0.3.1_x64-setup.exe",
            "https://example.com/Witness_0.3.0_x64-setup.exe",
        ] {
            assert!(validate_installer_url(bad, name).is_err(), "{bad}");
        }
    }
}
