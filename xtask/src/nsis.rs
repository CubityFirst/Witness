//! The NSIS toolchain: fetched once into `%LOCALAPPDATA%\witness-build\`,
//! pinned by SHA-256 (a mismatch is a hard error; there is no trust on first
//! use), and the makensis invocation with the full `/D` list (design §8.2).
//!
//! These are the same two artefacts the Tauri CLI 2.11.4 used for every 0.2.x
//! installer, so the compiled installer behaves the same.

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// NSIS 3.11 as packaged by Tauri (SHA1 EF7FF767… is Tauri's own pin).
pub const NSIS_URL: &str =
    "https://github.com/tauri-apps/binary-releases/releases/download/nsis-3.11/nsis-3.11.zip";
pub const NSIS_SHA256: &str = "c7d27f780ddb6cffb4730138cd1591e841f4b7edb155856901cdf5f214394fa1";
const NSIS_DIR: &str = "nsis-3.11";

/// `FindProcessCurrentUser` / `KillProcessCurrentUser` / `RunAsUser` /
/// `SemverCompare`; byte-identical to the copy inside the shipped 0.2.1
/// installer.
pub const TAURI_UTILS_URL: &str = "https://github.com/tauri-apps/nsis-tauri-utils/releases/download/nsis_tauri_utils-v0.5.3/nsis_tauri_utils.dll";
pub const TAURI_UTILS_SHA256: &str =
    "5ba143b5db4a87d32d6e7802e033330aae56cbceabe0d1e3ba41948385ad4709";
const TAURI_UTILS_DIR: &str = "nsis_tauri_utils-0.5.3";

/// Paths into the verified toolchain.
pub struct Toolchain {
    pub makensis: PathBuf,
    /// Directory holding `nsis_tauri_utils.dll` (`/DPLUGINSDIR`).
    pub plugins_dir: PathBuf,
}

/// `%WITNESS_BUILD_CACHE%`, else `%LOCALAPPDATA%\witness-build` (CI caches
/// this dir keyed on the hashes above).
pub fn cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("WITNESS_BUILD_CACHE") {
        return Ok(dir.into());
    }
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local).join("witness-build"))
}

/// Hex SHA-256 of a reader.
pub fn sha256_hex(mut reader: impl Read) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    sha256_hex(File::open(path).with_context(|| format!("open {}", path.display()))?)
}

/// `path` if it exists with the pinned hash; otherwise download it from
/// `url` (via `<path>.part`) and verify. A freshly downloaded file with the
/// wrong hash is an error, never used.
fn fetch_verified(url: &str, sha256: &str, path: &Path) -> Result<()> {
    if path.is_file() && sha256_file(path)? == sha256 {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    println!("downloading {url}");
    let part = path.with_extension("part");
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("download {url}"))?;
    let mut reader = response.into_reader();
    let mut out = File::create(&part).with_context(|| format!("create {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    out.flush()?;
    drop(out);
    let got = format!("{:x}", hasher.finalize());
    if got != sha256 {
        let _ = std::fs::remove_file(&part);
        bail!("SHA-256 mismatch for {url}: expected {sha256}, got {got}; refusing to use it");
    }
    std::fs::rename(&part, path).with_context(|| format!("move into {}", path.display()))?;
    Ok(())
}

/// Find `makensis.exe` at the root of an extracted NSIS tree (or one level
/// down, if the zip had a top-level folder).
fn find_makensis(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join("makensis.exe");
    if direct.is_file() {
        return Some(direct);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path().join("makensis.exe"))
        .find(|p| p.is_file())
}

/// Download (if needed), verify and unpack NSIS + the Tauri plugin.
pub fn ensure_toolchain() -> Result<Toolchain> {
    let cache = cache_dir()?;
    std::fs::create_dir_all(&cache).with_context(|| format!("create {}", cache.display()))?;

    // NSIS: the zip is re-verified on every run; the tree is (re)extracted
    // whenever it was not produced from a zip with exactly this hash.
    let zip_path = cache.join(format!("{NSIS_DIR}.zip"));
    fetch_verified(NSIS_URL, NSIS_SHA256, &zip_path)?;
    let tree = cache.join(NSIS_DIR);
    let marker = tree.join(".witness-verified-sha256");
    let extracted_ok = std::fs::read_to_string(&marker)
        .map(|m| m.trim() == NSIS_SHA256)
        .unwrap_or(false);
    if !extracted_ok || find_makensis(&tree).is_none() {
        if tree.exists() {
            std::fs::remove_dir_all(&tree)
                .with_context(|| format!("remove stale {}", tree.display()))?;
        }
        let mut archive = zip::ZipArchive::new(File::open(&zip_path)?)
            .with_context(|| format!("open {}", zip_path.display()))?;
        archive
            .extract(&tree)
            .with_context(|| format!("extract into {}", tree.display()))?;
        std::fs::write(&marker, NSIS_SHA256)?;
    }
    let makensis =
        find_makensis(&tree).with_context(|| format!("no makensis.exe in {}", tree.display()))?;

    let plugins_dir = cache.join(TAURI_UTILS_DIR);
    fetch_verified(
        TAURI_UTILS_URL,
        TAURI_UTILS_SHA256,
        &plugins_dir.join("nsis_tauri_utils.dll"),
    )?;
    Ok(Toolchain {
        makensis,
        plugins_dir,
    })
}

/// The Tauri bundler's `ESTIMATEDSIZE`: the installed files' sizes in bytes,
/// summed, integer-divided by 1024 (checked against the regenerated 0.2.1
/// script, see installer/reference/PROVENANCE.md). Sizes are read through
/// open handles so symlinked ort DLLs report their target's size.
pub fn estimated_size_kib(files: &[PathBuf]) -> Result<u64> {
    let mut total = 0u64;
    for f in files {
        let len = File::open(f)
            .and_then(|h| h.metadata())
            .with_context(|| format!("stat {}", f.display()))?
            .len();
        total += len;
    }
    Ok(total / 1024)
}

/// Everything the script takes through `/D`.
pub struct Inputs<'a> {
    pub version: &'a semver::Version,
    pub srcdir: &'a Path,
    pub outfile: &'a Path,
    pub estimated_size_kib: u64,
}

/// The `/D` arguments, in a stable order.
pub fn define_args(inputs: &Inputs, toolchain_plugins: &Path) -> Vec<String> {
    let v = inputs.version;
    vec![
        format!("/DVERSION={v}"),
        format!("/DVERSIONWITHBUILD={}.{}.{}.0", v.major, v.minor, v.patch),
        format!("/DESTIMATEDSIZE={}", inputs.estimated_size_kib),
        format!("/DSRCDIR={}", inputs.srcdir.display()),
        format!("/DOUTFILE={}", inputs.outfile.display()),
        format!("/DPLUGINSDIR={}", toolchain_plugins.display()),
    ]
}

/// `makensis /V3 -INPUTCHARSET UTF8 /D... installer\witness.nsi`.
pub fn makensis(toolchain: &Toolchain, script: &Path, inputs: &Inputs) -> Result<()> {
    ensure!(script.is_file(), "missing {}", script.display());
    if let Some(dir) = inputs.outfile.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let status = Command::new(&toolchain.makensis)
        .arg("/V3")
        .args(["-INPUTCHARSET", "UTF8"])
        .args(define_args(inputs, &toolchain.plugins_dir))
        .arg(script)
        .status()
        .with_context(|| format!("run {}", toolchain.makensis.display()))?;
    ensure!(status.success(), "makensis failed ({status})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_sizes() {
        assert_eq!(
            sha256_hex(&b"abc"[..]).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let dir = std::env::temp_dir().join(format!("xtask-nsis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, vec![0u8; 3000]).unwrap();
        std::fs::write(&b, vec![0u8; 100]).unwrap();
        // (3000 + 100) / 1024 = 3, truncated like the Tauri bundler.
        assert_eq!(estimated_size_kib(&[a.clone(), b]).unwrap(), 3);
        assert!(estimated_size_kib(&[dir.join("missing")]).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn defines_cover_the_script_inputs() {
        let v = semver::Version::new(0, 3, 0);
        let args = define_args(
            &Inputs {
                version: &v,
                srcdir: Path::new(r"C:\t\release"),
                outfile: Path::new(r"C:\t\release\bundle\nsis\Witness_0.3.0_x64-setup.exe"),
                estimated_size_kib: 102522,
            },
            Path::new(r"C:\cache\nsis_tauri_utils-0.5.3"),
        );
        assert_eq!(
            args,
            [
                "/DVERSION=0.3.0",
                "/DVERSIONWITHBUILD=0.3.0.0",
                "/DESTIMATEDSIZE=102522",
                r"/DSRCDIR=C:\t\release",
                r"/DOUTFILE=C:\t\release\bundle\nsis\Witness_0.3.0_x64-setup.exe",
                r"/DPLUGINSDIR=C:\cache\nsis_tauri_utils-0.5.3",
            ]
        );
        // Every define the script demands is passed.
        let nsi =
            std::fs::read_to_string(crate::workspace::repo_root().join("installer/witness.nsi"))
                .unwrap();
        for line in nsi
            .lines()
            .filter_map(|l| l.trim().strip_prefix("!ifndef "))
        {
            assert!(
                args.iter()
                    .any(|a| a.starts_with(&format!("/D{}=", line.trim()))),
                "script requires {line} but the xtask does not pass it"
            );
        }
    }
}
