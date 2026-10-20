//! `cargo run -p xtask -- dist [--sign] [--notes TEXT]` (design §8.3) and the
//! development-only `package` command.
//!
//! dist never trusts what happens to be lying in the target dir: it forces
//! cargo to relink `witness.exe`, then refuses an exe older than the build
//! or one whose VERSIONINFO is not the workspace version, and an installer
//! older than the build.

use crate::nsis::{self, Inputs};
use crate::sign;
use crate::workspace;
use anyhow::{bail, ensure, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// Filesystem timestamps can be coarser than the clock; anything this much
/// older than the build start was certainly not produced by it.
const MTIME_SLACK: Duration = Duration::from_secs(2);

/// `Witness_{v}_x64-setup.exe`, the asset name every feed URL points at.
pub fn installer_name(version: &semver::Version) -> String {
    format!("Witness_{version}_x64-setup.exe")
}

/// The files the installer ships (besides the uninstaller it writes).
fn payload(srcdir: &Path) -> Vec<PathBuf> {
    std::iter::once("witness.exe")
        .chain(crate::legacy::RUNTIME_DLLS)
        .map(|f| srcdir.join(f))
        .collect()
}

pub fn dist(sign_it: bool, notes: Option<String>) -> Result<()> {
    let root = workspace::repo_root();
    let version = workspace::workspace_version(&root)?;
    let metadata = workspace::metadata(&root)?;
    ensure!(
        metadata.has_package("witness"),
        "the workspace has no `witness` package yet (the native bin crate)"
    );
    // Never assume `target\`: CARGO_TARGET_DIR / config decide.
    let release = metadata.target_directory.join("release");
    println!("target dir: {}", metadata.target_directory.display());

    // Load and vet the key before the long build, so a missing or wrong
    // secret fails fast.
    let secret_key = if sign_it {
        let sk = sign::load_secret_key()?;
        sign::ensure_release_key(&sk)?;
        Some(sk)
    } else {
        None
    };

    let start = SystemTime::now();
    // A no-op `cargo build` leaves an old exe in place, which the freshness
    // check below would (rightly) refuse. Cleaning just this package makes
    // cargo relink it, so the installer always wraps an exe built from the
    // current tree. Dependencies (whisper.cpp, ort) stay cached.
    cargo(&root, &["clean", "--release", "-p", "witness"])?;
    cargo(&root, &["build", "--release", "--locked", "-p", "witness"])?;

    let exe = release.join("witness.exe");
    ensure_fresh(&exe, start)?;
    let exe_bytes = std::fs::read(&exe).with_context(|| format!("read {}", exe.display()))?;
    ensure!(
        has_product_version(&exe_bytes, &version.to_string()),
        "{} does not carry VERSIONINFO ProductVersion {version}; refusing a stale or foreign exe",
        exe.display()
    );
    drop(exe_bytes);
    for dll in crate::legacy::RUNTIME_DLLS {
        ensure_nonempty(&release.join(dll))?;
    }

    let toolchain = nsis::ensure_toolchain()?;
    let out_dir = release.join("bundle").join("nsis");
    let outfile = out_dir.join(installer_name(&version));
    // Also drop a .sig / latest.json of an earlier signed run, so they can
    // never sit next to (and be mistaken for) this build's installer.
    let feed = out_dir.join("latest.json");
    for stale in [outfile.clone(), sig_path(&outfile), feed.clone()] {
        remove_if_present(&stale)?;
    }
    let files = payload(&release);
    nsis::makensis(
        &toolchain,
        &root.join("installer").join("witness.nsi"),
        &Inputs {
            version: &version,
            srcdir: &release,
            outfile: &outfile,
            estimated_size_kib: nsis::estimated_size_kib(&files)?,
        },
    )?;
    ensure_fresh(&outfile, start)?;
    ensure_nonempty(&outfile)?;

    let mut outputs = vec![outfile.clone()];
    if let Some(sk) = secret_key {
        let sig = sign::sign_installer(
            &sk,
            &sign::SignRequest {
                file: outfile.clone(),
                version: version.to_string(),
                feed: Some(feed.clone()),
                notes,
                url: None,
                pubkey: None,
                force_version: false,
            },
        )?;
        outputs.push(sig);
        outputs.push(feed);
    } else if notes.is_some() {
        eprintln!("note: --notes only goes into latest.json, which needs --sign");
    }

    println!("\nWitness {version} installer ready:");
    for f in &outputs {
        println!("  {}  {}", nsis::sha256_file(f)?, f.display());
    }
    Ok(())
}

/// Development only: run the installer script over prebuilt binaries
/// (`witness.exe` + the two ort DLLs in `srcdir`). No cargo build, no
/// freshness checks, never signed; the output is not a release artefact.
pub fn package(srcdir: PathBuf, version: Option<String>, out: Option<PathBuf>) -> Result<()> {
    let root = workspace::repo_root();
    let version = match version {
        Some(v) => workspace::parse_release_version(&v)?,
        None => workspace::workspace_version(&root)?,
    };
    let srcdir = std::path::absolute(&srcdir)?;
    let files = payload(&srcdir);
    for f in &files {
        ensure_nonempty(f)?;
    }
    let outfile = match out {
        Some(p) => std::path::absolute(p)?,
        None => srcdir
            .join("bundle")
            .join("nsis")
            .join(installer_name(&version)),
    };
    remove_if_present(&outfile)?;
    let toolchain = nsis::ensure_toolchain()?;
    nsis::makensis(
        &toolchain,
        &root.join("installer").join("witness.nsi"),
        &Inputs {
            version: &version,
            srcdir: &srcdir,
            outfile: &outfile,
            estimated_size_kib: nsis::estimated_size_kib(&files)?,
        },
    )?;
    ensure_nonempty(&outfile)?;
    println!(
        "\n(development package, not a release artefact)\n  {}  {}",
        nsis::sha256_file(&outfile)?,
        outfile.display()
    );
    Ok(())
}

fn cargo(root: &Path, args: &[&str]) -> Result<()> {
    println!("> cargo {}", args.join(" "));
    let status = Command::new(workspace::cargo())
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("run cargo {}", args.join(" ")))?;
    if !status.success() {
        bail!("cargo {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// `<file>.sig`, where `sign` writes the signature.
pub(crate) fn sig_path(file: &Path) -> PathBuf {
    let mut p = file.as_os_str().to_owned();
    p.push(".sig");
    PathBuf::from(p)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove the old {}", path.display())),
    }
}

/// Refuse a file that was not written by this run.
pub fn ensure_fresh(path: &Path, build_start: SystemTime) -> Result<()> {
    let modified = std::fs::metadata(path)
        .with_context(|| format!("{} was not produced", path.display()))?
        .modified()?;
    if modified + MTIME_SLACK < build_start {
        bail!(
            "{} is stale (modified {}, before this build started at {}); refusing to package it",
            path.display(),
            chrono::DateTime::<chrono::Local>::from(modified).format("%Y-%m-%d %H:%M:%S"),
            chrono::DateTime::<chrono::Local>::from(build_start).format("%Y-%m-%d %H:%M:%S"),
        );
    }
    Ok(())
}

/// Open the file (following symlinks: ort-sys symlinks its DLLs when
/// Developer Mode allows) and require content.
pub fn ensure_nonempty(path: &Path) -> Result<()> {
    let len = std::fs::File::open(path)
        .and_then(|f| f.metadata())
        .with_context(|| format!("missing {}", path.display()))?
        .len();
    ensure!(len > 0, "{} is empty", path.display());
    Ok(())
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Does the PE's VERSIONINFO hold `ProductVersion` = `version`? A raw byte
/// search (no PE parser): in a StringFileInfo `String` block the UTF-16LE key
/// is NUL-terminated and padded to a 32-bit boundary, then the value follows,
/// itself NUL-terminated (so 0.2.1 does not match 0.2.10).
pub fn has_product_version(exe: &[u8], version: &str) -> bool {
    let key = utf16le("ProductVersion");
    let mut want = utf16le(version);
    want.extend_from_slice(&[0, 0]);
    let mut from = 0;
    while let Some(pos) = find(&exe[from..], &key) {
        let mut i = from + pos + key.len();
        // Key terminator + alignment padding: at most 3 UTF-16 NULs.
        let mut zeros = 0;
        while zeros < 6 && exe.get(i) == Some(&0) && exe.get(i + 1) == Some(&0) {
            i += 2;
            zeros += 2;
        }
        if zeros > 0 && exe[i..].starts_with(&want) {
            return true;
        }
        from += pos + 1;
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_block(key: &str, value: &str) -> Vec<u8> {
        // wLength, wValueLength, wType (dummy), key + NUL, pad, value + NUL.
        let mut b = vec![0x30, 0x00, 0x06, 0x00, 0x01, 0x00];
        b.extend(utf16le(key));
        b.extend([0, 0]);
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.extend(utf16le(value));
        b.extend([0, 0]);
        b
    }

    #[test]
    fn finds_product_version_exactly() {
        let mut exe = b"MZ....".to_vec();
        exe.extend(string_block("FileVersion", "0.2.1"));
        exe.extend(string_block("ProductVersion", "0.2.10"));
        assert!(!has_product_version(&exe, "0.2.1"));
        assert!(has_product_version(&exe, "0.2.10"));
        // FileVersion alone does not count.
        let mut only_file = b"MZ".to_vec();
        only_file.extend(string_block("FileVersion", "0.3.0"));
        assert!(!has_product_version(&only_file, "0.3.0"));
    }

    #[test]
    fn real_versioninfo_resources() {
        // VS_VERSIONINFO resources cut verbatim from real 0.2.1 binaries (see
        // installer/reference/PROVENANCE.md): the tauri-build/winres exe dist
        // guards, and the NSIS installer (VIAddVersionKey) sign guards.
        for blob in [
            &include_bytes!("../fixtures/versioninfo-witness-0.2.1.bin")[..],
            &include_bytes!("../fixtures/versioninfo-setup-0.2.1.bin")[..],
        ] {
            let mut pe = b"MZ\x90\x00 header bytes".to_vec();
            pe.extend_from_slice(blob);
            pe.extend_from_slice(b"\x00\x00 trailing sections");
            assert!(has_product_version(&pe, "0.2.1"));
            assert!(!has_product_version(&pe, "0.2.2"));
            assert!(!has_product_version(&pe, "0.2"));
        }
    }

    #[test]
    fn sig_path_appends() {
        assert_eq!(
            sig_path(Path::new(r"C:\o\Witness_0.3.0_x64-setup.exe")),
            PathBuf::from(r"C:\o\Witness_0.3.0_x64-setup.exe.sig")
        );
    }

    #[test]
    fn stale_files_are_refused() {
        let dir = std::env::temp_dir().join(format!("xtask-dist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("witness.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let start = SystemTime::now();
        ensure_fresh(&exe, start).unwrap(); // within the slack
        let old = start - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&exe)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let err = ensure_fresh(&exe, start).unwrap_err().to_string();
        assert!(err.contains("stale"), "{err}");
        assert!(ensure_fresh(&dir.join("missing.exe"), start).is_err());

        std::fs::write(dir.join("empty.dll"), b"").unwrap();
        assert!(ensure_nonempty(&dir.join("empty.dll")).is_err());
        assert!(ensure_nonempty(&exe).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn installer_name_matches_the_feed_assets() {
        assert_eq!(
            installer_name(&semver::Version::new(0, 2, 1)),
            "Witness_0.2.1_x64-setup.exe"
        );
    }
}
