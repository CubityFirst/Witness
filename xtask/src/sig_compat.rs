//! `cargo test -p xtask sig_compat`: proves that signatures produced by this
//! xtask pass the exact checks installed 0.2.x clients run.
//!
//! `verify_signature`, `verify_signed_version`, `signed_version` and
//! `base64_to_string` below are copied **verbatim** from
//! tauri-plugin-updater 2.12.0 (`src/updater.rs:1530-1615`, MIT/Apache-2.0).
//! Only the plugin's `Error`/`Result` types are replaced by the minimal
//! local equivalent below. Do not "tidy" the copied bodies: their value is
//! that they are the client's code.

use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use std::str::FromStr;

/// The variants of tauri-plugin-updater's `Error` that the copied code uses.
#[derive(Debug)]
#[allow(dead_code)] // fields are read through Debug in assertion messages
enum Error {
    Minisign(minisign_verify::Error),
    Base64(base64::DecodeError),
    SignatureUtf8(String),
    MissingSignedVersion,
    SignedVersionMismatch { signed: String, announced: String },
}

impl From<minisign_verify::Error> for Error {
    fn from(e: minisign_verify::Error) -> Self {
        Error::Minisign(e)
    }
}

impl From<base64::DecodeError> for Error {
    fn from(e: base64::DecodeError) -> Self {
        Error::Base64(e)
    }
}

type Result<T> = std::result::Result<T, Error>;

// ---- verbatim from tauri-plugin-updater 2.12.0 src/updater.rs ----

// Validate signature
fn verify_signature(
    data: &[u8],
    release_signature: &str,
    pub_key: &str,
    announced_version: &str,
    require_signed_version: bool,
) -> Result<()> {
    // we need to convert the pub key
    let pub_key_decoded = base64_to_string(pub_key)?;
    let public_key = PublicKey::decode(&pub_key_decoded)?;
    let signature_base64_decoded = base64_to_string(release_signature)?;
    let signature = Signature::decode(&signature_base64_decoded)?;

    // Validate signature or bail out
    public_key.verify(data, &signature, true)?;

    // Only now is the trusted comment usable: minisign's global signature covers it, and
    // `verify` above is what checks that global signature. Reading it before this point would
    // be trusting attacker controlled data.
    verify_signed_version(
        signature.trusted_comment(),
        announced_version,
        require_signed_version,
    )
}

/// Checks the version the artifact was signed for against the version the update endpoint
/// announced.
///
/// The endpoint response is not signed, so its `version` field on its own does not prove which
/// release the `url` and `signature` actually point at. Comparing it against the signed version
/// is what stops a tampered response from pairing a new version number with an older release.
fn verify_signed_version(
    trusted_comment: &str,
    announced_version: &str,
    require_signed_version: bool,
) -> Result<()> {
    let Some(signed_version) = signed_version(trusted_comment) else {
        // Signatures produced before the Tauri CLI started recording the version carry none, so
        // this can only be enforced when the app opts in. Note that leaving it off means an
        // attacker can bypass the check outright by serving one of those older signatures.
        return if require_signed_version {
            Err(Error::MissingSignedVersion)
        } else {
            Ok(())
        };
    };

    // compare as semver so that equivalent spellings like `1.2.3` and `v1.2.3` match, falling
    // back to a literal comparison for versions that are not valid semver
    let matches = match (
        Version::from_str(signed_version.trim_start_matches('v')),
        Version::from_str(announced_version.trim_start_matches('v')),
    ) {
        (Ok(signed), Ok(announced)) => signed == announced,
        _ => signed_version == announced_version,
    };

    if matches {
        Ok(())
    } else {
        Err(Error::SignedVersionMismatch {
            signed: signed_version.to_string(),
            announced: announced_version.to_string(),
        })
    }
}

/// Reads the `version` field out of a signature's trusted comment, which the Tauri CLI writes as
/// tab separated `key:value` pairs, e.g. `timestamp:1700000000\tfile:app.tar.gz\tversion:1.2.3`.
fn signed_version(trusted_comment: &str) -> Option<&str> {
    trusted_comment
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"))
}

fn base64_to_string(base64_string: &str) -> Result<String> {
    let decoded_string = &base64::engine::general_purpose::STANDARD.decode(base64_string)?;
    let result = std::str::from_utf8(decoded_string)
        .map_err(|_| Error::SignatureUtf8(base64_string.into()))?
        .to_string();
    Ok(result)
}

// ---- end of the verbatim copy ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{legacy, sign};

    const INSTALLER: &[u8] = b"MZ\x90\x00 pretend NSIS installer bytes";
    const NAME: &str = "Witness_0.3.0_x64-setup.exe";

    fn throwaway() -> (minisign::SecretKey, String) {
        let (sk_b64, pk_b64) = sign::generate_test_key().unwrap();
        // Decoded exactly as the release secret is (base64 box, "" password).
        (sign::decode_secret_key(&sk_b64, "").unwrap(), pk_b64)
    }

    #[test]
    fn xtask_signature_with_version_passes_the_0_2_x_client() {
        let (sk, pk) = throwaway();
        let sig = sign::sign_bytes(&sk, INSTALLER, NAME, Some("0.3.0"), 1790202134).unwrap();

        // 0.2.x clients: require_signed_version = false (Witness never set it).
        verify_signature(INSTALLER, &sig, &pk, "0.3.0", false).unwrap();
        verify_signature(INSTALLER, &sig, &pk, "v0.3.0", false).unwrap();
        // The native updater's stricter mode passes too.
        verify_signature(INSTALLER, &sig, &pk, "0.3.0", true).unwrap();

        // A feed announcing another version is rejected because of `version:`.
        let err = verify_signature(INSTALLER, &sig, &pk, "0.3.1", false).unwrap_err();
        assert!(
            matches!(&err, Error::SignedVersionMismatch { signed, announced }
                if signed == "0.3.0" && announced == "0.3.1"),
            "{err:?}"
        );
    }

    #[test]
    fn signature_without_version_like_tauri_cli_2_11_4() {
        // What tauri-cli 2.11.4 wrote for 0.2.x: no `version:` field.
        let (sk, pk) = throwaway();
        let sig = sign::sign_bytes(&sk, INSTALLER, NAME, None, 1790202134).unwrap();
        verify_signature(INSTALLER, &sig, &pk, "0.3.0", false).unwrap();
        let err = verify_signature(INSTALLER, &sig, &pk, "0.3.0", true).unwrap_err();
        assert!(matches!(err, Error::MissingSignedVersion), "{err:?}");
    }

    #[test]
    fn tampered_data_and_wrong_key_fail() {
        let (sk, pk) = throwaway();
        let sig = sign::sign_bytes(&sk, INSTALLER, NAME, Some("0.3.0"), 1).unwrap();
        let mut tampered = INSTALLER.to_vec();
        tampered[5] ^= 1;
        assert!(matches!(
            verify_signature(&tampered, &sig, &pk, "0.3.0", false),
            Err(Error::Minisign(_))
        ));
        let (_, other_pk) = throwaway();
        assert!(matches!(
            verify_signature(INSTALLER, &sig, &other_pk, "0.3.0", false),
            Err(Error::Minisign(_))
        ));
        assert!(matches!(
            verify_signature(INSTALLER, &sig, legacy::UPDATER_PUBKEY, "0.3.0", false),
            Err(Error::Minisign(_))
        ));
        assert!(matches!(
            verify_signature(INSTALLER, "%%% not base64", &pk, "0.3.0", false),
            Err(Error::Base64(_))
        ));
    }

    #[test]
    fn shipped_0_2_1_signature_is_from_the_release_key_and_unversioned() {
        // The real 0.2.1 feed: its signature decodes, names the release key,
        // and carries no `version:` (so the native updater rejects it and
        // rollbacks are re-signed by the xtask, design §8.4).
        let feed: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/latest-0.2.1.json")).unwrap();
        let sig_b64 = feed["platforms"]["windows-x86_64"]["signature"]
            .as_str()
            .unwrap();
        let text = base64_to_string(sig_b64).unwrap();
        let sig = Signature::decode(&text).unwrap();
        assert_eq!(
            sig.trusted_comment(),
            "timestamp:1790202134\tfile:Witness_0.2.1_x64-setup.exe"
        );
        assert_eq!(signed_version(sig.trusted_comment()), None);
        verify_signed_version(sig.trusted_comment(), "0.2.1", false).unwrap();
        assert!(verify_signed_version(sig.trusted_comment(), "0.2.1", true).is_err());

        // Key ID inside the signature line == the pubkey's key ID.
        let sig_line = text.lines().nth(1).unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(sig_line)
            .unwrap();
        assert_eq!(
            &raw[..2],
            b"ED",
            "prehashed signature, like every tauri-cli .sig"
        );
        let mut id = [0u8; 8];
        id.copy_from_slice(&raw[2..10]);
        assert_eq!(
            format!("{:016X}", u64::from_le_bytes(id)),
            legacy::UPDATER_KEY_ID
        );
        PublicKey::decode(&base64_to_string(legacy::UPDATER_PUBKEY).unwrap()).unwrap();
    }
}
