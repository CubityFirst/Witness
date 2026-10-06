//! The workspace version (single source: root `Cargo.toml`
//! `[workspace.package] version`), its mirror in `Cargo.lock`, and the
//! resolved cargo target dir.
//!
//! Edits are line-based on purpose: they touch exactly one line per entry and
//! keep comments, ordering and line endings, like the old `set-version.mjs`.

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Repo root (the xtask lives in `<root>/xtask`).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits one level below the workspace root")
        .to_path_buf()
}

/// The cargo that is running us (`cargo run` sets `CARGO`), else PATH's.
pub fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// Parse and validate a release version. NSIS `VIProductVersion` needs a
/// numeric `X.X.X.X` (each part a 16-bit number), so prerelease/build
/// suffixes such as `0.3.0-rc.1` are refused; use a GitHub prerelease of a
/// plain version instead (design §8.4).
pub fn parse_release_version(s: &str) -> Result<semver::Version> {
    let v = semver::Version::parse(s.trim())
        .with_context(|| format!("`{s}` is not a semver version (X.Y.Z)"))?;
    ensure!(
        v.pre.is_empty() && v.build.is_empty(),
        "`{s}`: prerelease/build suffixes are not allowed (VIProductVersion must be numeric); \
         ship a GitHub prerelease of a plain X.Y.Z instead"
    );
    ensure!(
        v.major <= 65535 && v.minor <= 65535 && v.patch <= 65535,
        "`{s}`: each version part must fit in 16 bits for VIProductVersion"
    );
    Ok(v)
}

/// `[workspace.package] version` from the root `Cargo.toml` text.
pub fn workspace_version_in(manifest: &str) -> Result<String> {
    let (_, value) = find_workspace_version_line(manifest)?;
    Ok(value)
}

/// Read and validate the workspace version.
pub fn workspace_version(root: &Path) -> Result<semver::Version> {
    let path = root.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_release_version(&workspace_version_in(&text)?)
}

/// (line index, current value) of `version = "…"` inside `[workspace.package]`.
fn find_workspace_version_line(manifest: &str) -> Result<(usize, String)> {
    let mut in_section = false;
    for (i, line) in manifest.lines().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t == "[workspace.package]";
            continue;
        }
        if in_section {
            if let Some(value) = key_string_value(t, "version") {
                return Ok((i, value));
            }
        }
    }
    bail!("no `version = \"…\"` in [workspace.package] of the root Cargo.toml")
}

/// `key = "value"` → `value` (no escapes: versions never need them).
fn key_string_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// Replace the quoted value on line `index`, keeping everything else
/// (indentation, trailing comment, line ending).
fn replace_quoted_on_line(text: &str, index: usize, new_value: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if i == index {
            let open = line.find('"').expect("line has a quoted value");
            let close = open + 1 + line[open + 1..].find('"').expect("closing quote");
            out.push_str(&line[..open + 1]);
            out.push_str(new_value);
            out.push_str(&line[close..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// New root `Cargo.toml` text with the workspace version set to `version`.
pub fn with_workspace_version(manifest: &str, version: &str) -> Result<String> {
    let (index, _) = find_workspace_version_line(manifest)?;
    Ok(replace_quoted_on_line(manifest, index, version))
}

/// Line index of the `version = "…"` line of the `[[package]]` entry
/// `name` in `Cargo.lock` (workspace members have no `source`, so the
/// version line directly follows the name).
fn lock_version_line(lock: &str, name: &str) -> Result<Option<(usize, String)>> {
    let lines: Vec<&str> = lock.lines().collect();
    let wanted = format!("name = \"{name}\"");
    let mut found = None;
    for i in 1..lines.len() {
        if lines[i].trim_end() == wanted && lines[i - 1].trim_end() == "[[package]]" {
            let next = lines.get(i + 1).copied().unwrap_or_default();
            let value = key_string_value(next.trim(), "version")
                .with_context(|| format!("Cargo.lock entry `{name}` has no version line"))?;
            ensure!(
                found.is_none(),
                "Cargo.lock has more than one `{name}` entry"
            );
            found = Some((i + 1, value));
        }
    }
    Ok(found)
}

/// The version `Cargo.lock` records for package `name`.
pub fn lock_version(lock: &str, name: &str) -> Result<Option<String>> {
    Ok(lock_version_line(lock, name)?.map(|(_, v)| v))
}

/// New `Cargo.lock` text with each of `names` set to `version`. Every name
/// must have exactly one entry.
pub fn with_lock_versions(lock: &str, names: &[String], version: &str) -> Result<String> {
    let mut text = lock.to_string();
    for name in names {
        let (index, _) = lock_version_line(&text, name)?
            .with_context(|| format!("Cargo.lock has no entry for workspace package `{name}`"))?;
        text = replace_quoted_on_line(&text, index, version);
    }
    Ok(text)
}

/// `cargo run -p xtask -- version X.Y.Z`.
pub fn set_version(version: &str) -> Result<()> {
    let v = parse_release_version(version)?;
    let version = v.to_string();
    let root = repo_root();
    let names = metadata(&root)?.workspace_package_names();

    let manifest_path = root.join("Cargo.toml");
    let lock_path = root.join("Cargo.lock");
    let manifest = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let lock = std::fs::read_to_string(&lock_path)
        .with_context(|| format!("read {}", lock_path.display()))?;
    let old = workspace_version_in(&manifest)?;

    // Compute both edits before writing either, so a bad lockfile leaves
    // nothing half-bumped.
    let new_manifest = with_workspace_version(&manifest, &version)?;
    let new_lock = with_lock_versions(&lock, &names, &version)?;
    std::fs::write(&manifest_path, new_manifest)
        .with_context(|| format!("write {}", manifest_path.display()))?;
    std::fs::write(&lock_path, new_lock)
        .with_context(|| format!("write {}", lock_path.display()))?;
    println!(
        "version {old} -> {version} (Cargo.toml [workspace.package]; Cargo.lock: {})",
        names.join(", ")
    );
    println!("next: commit, then tag v{version}");
    Ok(())
}

/// The parts of `cargo metadata --format-version 1 --no-deps` we use.
#[derive(Debug, Deserialize)]
pub struct Metadata {
    /// Honours `CARGO_TARGET_DIR`, `build.target-dir` in any config, etc.
    pub target_directory: PathBuf,
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
}

impl Metadata {
    /// With `--no-deps`, `packages` is exactly the workspace members.
    pub fn workspace_package_names(&self) -> Vec<String> {
        self.packages.iter().map(|p| p.name.clone()).collect()
    }

    pub fn has_package(&self, name: &str) -> bool {
        self.packages.iter().any(|p| p.name == name)
    }
}

/// Run `cargo metadata --no-deps` in the repo root. Never assume `target\`:
/// the target dir comes from here (design §8.3).
pub fn metadata(root: &Path) -> Result<Metadata> {
    let out = Command::new(cargo())
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .context("run cargo metadata")?;
    if !out.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parse cargo metadata output")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "[workspace]\r\nresolver = \"2\"\r\nmembers = [\"a\"]\r\n\r\n\
        [workspace.package]\r\nversion = \"0.2.1\" # the one source\r\nedition = \"2021\"\r\n\r\n\
        [profile.release]\r\nlto = true\r\n";

    #[test]
    fn reads_and_sets_workspace_version_preserving_layout() {
        assert_eq!(workspace_version_in(MANIFEST).unwrap(), "0.2.1");
        let bumped = with_workspace_version(MANIFEST, "0.3.0").unwrap();
        assert_eq!(
            bumped,
            MANIFEST.replace("version = \"0.2.1\"", "version = \"0.3.0\"")
        );
        assert!(workspace_version_in("[package]\nversion = \"1.0.0\"\n").is_err());
    }

    #[test]
    fn version_in_other_sections_is_ignored() {
        let text = "[package]\nversion = \"9.9.9\"\n[workspace.package]\nversion = \"0.2.1\"\n";
        assert_eq!(workspace_version_in(text).unwrap(), "0.2.1");
    }

    const LOCK: &str = "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n\
        source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
        [[package]]\nname = \"witness\"\nversion = \"0.2.1\"\ndependencies = [\n \"witness-core\",\n]\n\n\
        [[package]]\nname = \"witness-core\"\nversion = \"0.2.1\"\n\n\
        [[package]]\nname = \"xtask\"\nversion = \"0.2.1\"\n";

    #[test]
    fn bumps_only_the_named_lock_entries() {
        let names = vec!["witness".to_string(), "witness-core".into(), "xtask".into()];
        let out = with_lock_versions(LOCK, &names, "0.3.0").unwrap();
        for n in &names {
            assert_eq!(lock_version(&out, n).unwrap().as_deref(), Some("0.3.0"));
        }
        assert_eq!(
            lock_version(&out, "serde").unwrap().as_deref(),
            Some("1.0.0")
        );
        // A dependency-list mention ` "witness-core",` is not an entry.
        assert_eq!(out.matches("0.3.0").count(), 3);
        assert!(with_lock_versions(LOCK, &["missing".to_string()], "0.3.0").is_err());
    }

    #[test]
    fn release_versions_must_be_plain_numeric() {
        assert_eq!(parse_release_version("0.3.0").unwrap().to_string(), "0.3.0");
        assert!(parse_release_version("0.3.0-rc.1").is_err());
        assert!(parse_release_version("0.3.0+build").is_err());
        assert!(parse_release_version("v0.3.0").is_err());
        assert!(parse_release_version("0.3").is_err());
        assert!(parse_release_version("70000.0.0").is_err());
    }

    #[test]
    fn real_workspace_is_consistent() {
        // The real files: one version, mirrored in the lockfile.
        let root = repo_root();
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
        let v = workspace_version_in(&manifest).unwrap();
        parse_release_version(&v).unwrap();
        for name in ["witness-core", "xtask"] {
            assert_eq!(lock_version(&lock, name).unwrap().as_deref(), Some(&*v));
        }
    }
}
