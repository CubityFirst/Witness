//! Witness build, signing and release tooling.
//!
//! Run as `cargo run -p xtask -- <command>` (no cargo alias: `.cargo/config.toml`
//! is machine-local and gitignored). This replaces the Tauri-era
//! `scripts/set-version.mjs`, `check-release-config.mjs` and
//! `build-windows-release.ps1`.
//!
//! The release contract this tooling guards (design §8.1) is what every
//! installed 0.2.x copy relies on: the same feed URL and schema, signatures
//! from the same minisign key, and an NSIS installer that honours
//! `/P /UPDATE /R /ARGS`, installs into the same per-user `$INSTDIR`, keeps
//! `witness.exe` and never deletes user data.

mod check;
mod dist;
mod nsis;
#[cfg(test)]
mod sig_compat;
mod sign;
mod workspace;

use anyhow::{bail, Context, Result};

/// Values 0.2.x clients have baked in. They can never change: a different
/// endpoint or key strands every installed copy (design §8.1).
pub mod legacy {
    /// The feed every installed copy polls (`releases/latest` skips drafts
    /// and prereleases).
    pub const UPDATE_ENDPOINT: &str =
        "https://github.com/CubityFirst/Witness/releases/latest/download/latest.json";
    /// minisign public key box, base64, verbatim from 0.2.1's tauri.conf.json.
    pub const UPDATER_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDVBMDZGNzZEMDBFNkFDRDgKUldUWXJPWUFiZmNHV2l6NDROVllaYWN2R0pMYmY3Rzg4N2EwUTh3bWN3RUN2ci9rRVhQZHgwcGEK";
    /// Key ID of `UPDATER_PUBKEY` (`~/.tauri/witness-updater.key`).
    pub const UPDATER_KEY_ID: &str = "5A06F76D00E6ACD8";
    /// Release download base; installers are `{base}/v{v}/{asset}`.
    pub const RELEASE_DOWNLOAD_BASE: &str =
        "https://github.com/CubityFirst/Witness/releases/download";
    /// NSIS installer identity (installer/witness.nsi must match exactly).
    pub const MANUFACTURER: &str = "github";
    pub const PRODUCTNAME: &str = "Witness";
    pub const MAINBINARYNAME: &str = "witness";
    pub const BUNDLEID: &str = "com.github.cubityfirst.witness";
    pub const UNINSTKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Witness";
    pub const MANUPRODUCTKEY: &str = r"Software\github\Witness";
    /// The ort CUDA provider DLLs that must sit next to `witness.exe`
    /// (gpu.rs `preflight()` looks in the app dir first).
    pub const RUNTIME_DLLS: [&str; 2] = [
        "onnxruntime_providers_cuda.dll",
        "onnxruntime_providers_shared.dll",
    ];
}

const USAGE: &str = "\
usage: cargo run -p xtask -- <command>

commands:
  version X.Y.Z
      Set the workspace version (root Cargo.toml [workspace.package] and the
      workspace packages' Cargo.lock entries).
  check [--tag vX.Y.Z]
      Verify the release configuration: installer identity and file list,
      updater endpoint/pubkey, workspace versions (and the tag, if given).
  dist [--sign] [--notes TEXT]
      Build the release witness.exe and the NSIS installer into
      <target>/release/bundle/nsis/. --sign also writes the .sig and
      latest.json (needs TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]).
  sign <file> --version X.Y.Z [--feed PATH] [--notes TEXT] [--url URL] [--pubkey B64]
      Write <file>.sig (and, with --feed, latest.json for that file). Works on
      any installer, including a Tauri-built rollback. --url overrides the
      installer download URL; --pubkey self-checks against a non-release key
      (throwaway-key rehearsals only).
  package --srcdir DIR [--version X.Y.Z] [--out FILE]
      Development only: wrap prebuilt binaries in DIR with installer/witness.nsi
      (no cargo build, no freshness checks, never signed). Used to iterate on
      the script and to rehearse the upgrade smoke test.
  gen-test-key <dir>
      Write a throwaway updater key pair (test-updater.key / test-updater.pub,
      in the TAURI_SIGNING_PRIVATE_KEY / pubkey formats) for rehearsals.";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "help" | "-h" | "--help") {
        println!("{USAGE}");
        return;
    }
    let command = args.remove(0);
    if let Err(e) = run(&command, args) {
        eprintln!("xtask {command}: error: {e:#}");
        std::process::exit(1);
    }
}

fn run(command: &str, mut args: Vec<String>) -> Result<()> {
    match command {
        "version" => {
            let version = take_positional(&mut args, "X.Y.Z")?;
            no_more(&args)?;
            workspace::set_version(&version)
        }
        "check" => {
            let tag = take_value(&mut args, "--tag")?;
            no_more(&args)?;
            check::run(tag.as_deref())
        }
        "dist" => {
            let sign = take_flag(&mut args, "--sign");
            let notes = take_value(&mut args, "--notes")?;
            no_more(&args)?;
            dist::dist(sign, notes)
        }
        "sign" => {
            let version = take_value(&mut args, "--version")?
                .context("--version X.Y.Z is required (it goes into the signed trusted comment)")?;
            let feed = take_value(&mut args, "--feed")?;
            let notes = take_value(&mut args, "--notes")?;
            let url = take_value(&mut args, "--url")?;
            let pubkey = take_value(&mut args, "--pubkey")?;
            let file = take_positional(&mut args, "<file>")?;
            no_more(&args)?;
            sign::sign_command(sign::SignRequest {
                file: file.into(),
                version,
                feed: feed.map(Into::into),
                notes,
                url,
                pubkey,
            })
        }
        "package" => {
            let srcdir = take_value(&mut args, "--srcdir")?.context("--srcdir DIR is required")?;
            let version = take_value(&mut args, "--version")?;
            let out = take_value(&mut args, "--out")?;
            no_more(&args)?;
            dist::package(srcdir.into(), version, out.map(Into::into))
        }
        "gen-test-key" => {
            let dir = take_positional(&mut args, "<dir>")?;
            no_more(&args)?;
            sign::gen_test_key(dir.as_ref())
        }
        other => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

/// Remove `flag` from `args`; true if it was present.
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

/// Remove `flag VALUE` (or `flag=VALUE`) from `args`.
fn take_value(args: &mut Vec<String>, flag: &str) -> Result<Option<String>> {
    let prefix = format!("{flag}=");
    if let Some(i) = args.iter().position(|a| a.starts_with(&prefix)) {
        let value = args.remove(i)[prefix.len()..].to_string();
        return Ok(Some(value));
    }
    let Some(i) = args.iter().position(|a| a == flag) else {
        return Ok(None);
    };
    if i + 1 >= args.len() {
        bail!("{flag} needs a value");
    }
    let value = args.remove(i + 1);
    args.remove(i);
    Ok(Some(value))
}

/// Remove the first argument that is not a flag.
fn take_positional(args: &mut Vec<String>, what: &str) -> Result<String> {
    match args.iter().position(|a| !a.starts_with("--")) {
        Some(i) => Ok(args.remove(i)),
        None => bail!("missing {what}"),
    }
}

fn no_more(args: &[String]) -> Result<()> {
    if args.is_empty() {
        Ok(())
    } else {
        bail!("unexpected argument(s): {}\n\n{USAGE}", args.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arg_helpers() {
        let mut a = v(&[
            "x.exe",
            "--version",
            "0.3.0",
            "--feed=out/latest.json",
            "--sign",
        ]);
        assert!(take_flag(&mut a, "--sign"));
        assert!(!take_flag(&mut a, "--sign"));
        assert_eq!(
            take_value(&mut a, "--version").unwrap().as_deref(),
            Some("0.3.0")
        );
        assert_eq!(
            take_value(&mut a, "--feed").unwrap().as_deref(),
            Some("out/latest.json")
        );
        assert_eq!(take_value(&mut a, "--notes").unwrap(), None);
        assert_eq!(take_positional(&mut a, "<file>").unwrap(), "x.exe");
        assert!(no_more(&a).is_ok());

        let mut dangling = v(&["--notes"]);
        assert!(take_value(&mut dangling, "--notes").is_err());
        assert!(no_more(&v(&["--bogus"])).is_err());
    }
}
