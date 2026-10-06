//! `cargo run -p xtask -- check [--tag vX.Y.Z]`: the release configuration
//! guard (replaces `check-release-config.mjs`).
//!
//! Everything here is a pure function over file text, so the unit tests run
//! the same checks against the real files in the repo on every `cargo test`.

use crate::legacy;
use crate::workspace;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Where the updater constants live (read as source text; the xtask does not
/// link the core crate).
pub const UPDATER_SOURCE: &str = "crates/witness-core/src/service/updater.rs";
pub const NSI_PATH: &str = "installer/witness.nsi";
pub const UTILS_PATH: &str = "installer/utils.nsh";

pub fn run(tag: Option<&str>) -> Result<()> {
    let root = workspace::repo_root();
    let metadata = workspace::metadata(&root)?;
    let manifest = read(&root, "Cargo.toml")?;
    let lock = read(&root, "Cargo.lock")?;
    let nsi = read(&root, NSI_PATH)?;
    let utils = read(&root, UTILS_PATH)?;
    let updater = read(&root, UPDATER_SOURCE)?;

    let mut problems = Vec::new();
    let version = match workspace::workspace_version_in(&manifest)
        .and_then(|v| workspace::parse_release_version(&v))
    {
        Ok(v) => Some(v.to_string()),
        Err(e) => {
            problems.push(format!("{e:#}"));
            None
        }
    };
    if let Some(version) = &version {
        let packages: Vec<(String, String)> = metadata
            .packages
            .iter()
            .map(|p| (p.name.clone(), p.version.clone()))
            .collect();
        problems.extend(check_versions(version, &packages, &lock, tag));
    } else if tag.is_some() {
        problems.push("cannot compare the tag: the workspace version is invalid".into());
    }
    problems.extend(check_installer(&nsi, &utils));
    problems.extend(check_updater_constants(&updater));
    problems.extend(check_pubkey(legacy::UPDATER_PUBKEY));

    if problems.is_empty() {
        println!(
            "release configuration OK (version {}{})",
            version.unwrap_or_default(),
            tag.map(|t| format!(", tag {t}")).unwrap_or_default()
        );
        Ok(())
    } else {
        for p in &problems {
            eprintln!("  - {p}");
        }
        bail!("{} release configuration problem(s)", problems.len())
    }
}

/// Read a repo file with LF line endings: the checks look for multi-line
/// text, and an autocrlf=true checkout (GitHub's Windows runners) may hand
/// us CRLF.
fn read(root: &Path, rel: &str) -> Result<String> {
    let path = root.join(rel);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(text.replace("\r\n", "\n"))
}

/// Workspace members share the workspace version, the lockfile mirrors it,
/// and a release tag names it.
pub fn check_versions(
    version: &str,
    packages: &[(String, String)],
    lock: &str,
    tag: Option<&str>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, pkg_version) in packages {
        if pkg_version != version {
            problems.push(format!(
                "package `{name}` is {pkg_version}, not the workspace version {version} \
                 (use `version.workspace = true`)"
            ));
        }
        match workspace::lock_version(lock, name) {
            Ok(Some(v)) if v == version => {}
            Ok(Some(v)) => problems.push(format!(
                "Cargo.lock records `{name}` {v}, expected {version} \
                 (run `cargo run -p xtask -- version {version}`)"
            )),
            Ok(None) => problems.push(format!("Cargo.lock has no entry for `{name}`")),
            Err(e) => problems.push(format!("{e:#}")),
        }
    }
    if let Some(tag) = tag {
        if tag != format!("v{version}") {
            problems.push(format!(
                "tag {tag} does not match the workspace version {version} (expected v{version})"
            ));
        }
    }
    problems
}

/// `!define NAME "value"` / `!define NAME value` lines (not the `/ifndef`,
/// `/math` … forms), in order.
pub fn nsi_defines(nsi: &str) -> BTreeMap<String, String> {
    let mut defines = BTreeMap::new();
    for line in code_lines(nsi) {
        let Some(rest) = line.strip_prefix("!define ") else {
            continue;
        };
        let rest = rest.trim();
        if rest.starts_with('/') {
            continue;
        }
        let (name, value) = match rest.split_once(char::is_whitespace) {
            Some((n, v)) => (n, v.trim()),
            None => (rest, ""),
        };
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        defines.insert(name.to_string(), value.to_string());
    }
    defines
}

/// Expand `${NAME}` references to other defines (unknown ones stay as-is).
pub fn expand(value: &str, defines: &BTreeMap<String, String>) -> String {
    let mut out = value.to_string();
    for _ in 0..8 {
        let mut changed = false;
        for (name, v) in defines {
            let token = format!("${{{name}}}");
            if out.contains(&token) {
                out = out.replace(&token, v);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// Non-comment, trimmed lines.
fn code_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('#'))
}

/// Collapse runs of whitespace so formatting changes don't break matching.
fn squash(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The body of the first `<header>` … `<end>` block, as code lines.
fn block<'a>(nsi: &'a str, header: &str, end: &str) -> Vec<&'a str> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in code_lines(nsi) {
        if !inside {
            inside = squash(line) == header;
        } else if line == end {
            break;
        } else {
            out.push(line);
        }
    }
    out
}

/// The body of `Section <name>` … `SectionEnd`, as code lines.
fn section<'a>(nsi: &'a str, name: &str) -> Vec<&'a str> {
    block(nsi, &format!("Section {name}"), "SectionEnd")
}

/// The body of `Function <name>` … `FunctionEnd`, squashed.
fn function(nsi: &str, name: &str) -> Vec<String> {
    block(nsi, &format!("Function {name}"), "FunctionEnd")
        .into_iter()
        .map(squash)
        .collect()
}

/// The lines of the `${If}` block that starts at `lines[start]` (nested
/// `${If}`s included), or nothing if `start` is not such a line.
fn if_block(lines: &[String], start: usize) -> &[String] {
    let mut depth = 0usize;
    for (i, line) in lines.iter().enumerate().skip(start) {
        if line.starts_with("${If}") || line.starts_with("${IfNot}") {
            depth += 1;
        } else if line == "${EndIf}" {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return &lines[start..=i];
            }
        }
        if depth == 0 {
            break;
        }
    }
    &[]
}

/// The target of an `RMDir /r` line with defines expanded, lowercased and
/// without quotes or a trailing backslash.
fn recursive_rmdir_target(line: &str, defines: &BTreeMap<String, String>) -> Option<String> {
    let expanded = expand(line, defines).to_ascii_lowercase();
    let target = expanded.strip_prefix("rmdir /r ")?;
    Some(
        target
            .trim()
            .trim_matches('"')
            .trim_end_matches('\\')
            .to_string(),
    )
}

/// The destination file name of a `File` instruction.
fn installed_name(line: &str, defines: &BTreeMap<String, String>) -> Option<String> {
    let rest = line.strip_prefix("File ")?;
    if let Some(i) = rest.find("/oname=") {
        let name = rest[i + "/oname=".len()..]
            .split('"')
            .next()
            .unwrap_or_default();
        return Some(expand(name, defines));
    }
    let source = rest.rsplit('"').nth(1).unwrap_or(rest.trim());
    let source = expand(source, defines);
    source.rsplit(['\\', '/']).next().map(str::to_string)
}

/// A function body and the `(switch, variable)` pairs it must parse.
type FlagSet<'a> = (&'a str, &'a Vec<String>, &'a [(&'a str, &'a str)]);

/// Installer identity, file list, flags and data safety (design §8.1/§8.2).
pub fn check_installer(nsi: &str, utils: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let defines = nsi_defines(nsi);
    let get = |name: &str| defines.get(name).map(|v| expand(v, &defines));

    let expected = [
        ("MANUFACTURER", legacy::MANUFACTURER),
        ("PRODUCTNAME", legacy::PRODUCTNAME),
        ("MAINBINARYNAME", legacy::MAINBINARYNAME),
        ("BUNDLEID", legacy::BUNDLEID),
        ("INSTALLMODE", "currentUser"),
        ("UNINSTKEY", legacy::UNINSTKEY),
        ("MANUPRODUCTKEY", legacy::MANUPRODUCTKEY),
        ("STARTMENUFOLDER", ""),
        ("ALLOWDOWNGRADES", "true"),
    ];
    for (name, want) in expected {
        match get(name) {
            Some(v) if v == want => {}
            Some(v) => problems.push(format!("{NSI_PATH}: {name} is \"{v}\", must be \"{want}\"")),
            None => problems.push(format!("{NSI_PATH}: missing !define {name}")),
        }
    }

    let lines: Vec<String> = code_lines(nsi).map(squash).collect();
    let has = |needle: &str| lines.iter().any(|l| l == needle);
    let contains = |needle: &str| lines.iter().any(|l| l.contains(needle));

    if !has("RequestExecutionLevel user") || contains("RequestExecutionLevel admin") {
        problems.push(format!(
            "{NSI_PATH}: must be `RequestExecutionLevel user` (per-user install, never elevated)"
        ));
    }
    let on_init = function(nsi, ".onInit");
    let on_install_success = function(nsi, ".onInstSuccess");
    let install: Vec<String> = section(nsi, "Install").into_iter().map(squash).collect();
    let uninstall_lines: Vec<String> = section(nsi, "Uninstall").into_iter().map(squash).collect();

    if !on_init
        .iter()
        .any(|l| l == r#"StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}""#)
        || !on_init
            .iter()
            .any(|l| l == "Call RestorePreviousInstallLocation")
    {
        problems.push(format!(
            "{NSI_PATH}: .onInit must default $INSTDIR to $LOCALAPPDATA\\${{PRODUCTNAME}}, \
             restored from {} on reinstall",
            legacy::MANUPRODUCTKEY
        ));
    }

    // Exactly witness.exe + the two ort provider DLLs, and the uninstaller
    // deletes exactly what was installed.
    let installed: Vec<String> = section(nsi, "Install")
        .into_iter()
        .filter_map(|l| installed_name(l, &defines))
        .collect();
    let mut want_files: Vec<String> = vec![format!("{}.exe", legacy::MAINBINARYNAME)];
    want_files.extend(legacy::RUNTIME_DLLS.iter().map(|s| s.to_string()));
    let mut got = installed.clone();
    got.sort();
    let mut want_sorted = want_files.clone();
    want_sorted.sort();
    if got != want_sorted {
        problems.push(format!(
            "{NSI_PATH}: Install section must copy exactly {want_files:?}, found {installed:?}"
        ));
    }
    let uninstall: Vec<String> = section(nsi, "Uninstall")
        .into_iter()
        .map(|l| expand(&squash(l), &defines))
        .collect();
    for file in want_files
        .iter()
        .chain(std::iter::once(&"uninstall.exe".to_string()))
    {
        let line = format!(r#"Delete "$INSTDIR\{file}""#);
        if !uninstall.contains(&line) {
            problems.push(format!("{NSI_PATH}: Uninstall section must `{line}`"));
        }
    }

    // Data safety: by default $INSTDIR is also the data dir.
    for line in &lines {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("rmdir /r") && lower.contains("$instdir") {
            problems.push(format!(
                "{NSI_PATH}: recursive delete of $INSTDIR: `{line}`"
            ));
        }
        if lower.starts_with("delete") && lower.contains("$instdir") && line.contains('*') {
            problems.push(format!("{NSI_PATH}: wildcard delete in $INSTDIR: `{line}`"));
        }
    }

    // The update flags 0.2.x passes, each parsed where it takes effect: the
    // installer's own .onInit (un.onInit has the same lines for the
    // uninstaller, which must not satisfy this) and .onInstSuccess.
    let flag_sets: [FlagSet; 2] = [
        (
            ".onInit",
            &on_init,
            &[
                ("/P", "$PassiveMode"),
                ("/NS", "$NoShortcutMode"),
                ("/UPDATE", "$UpdateMode"),
            ],
        ),
        (
            ".onInstSuccess",
            &on_install_success,
            &[("/R", "$R0"), ("/ARGS", "$R0")],
        ),
    ];
    for (name, body, flags) in flag_sets {
        for (flag, var) in flags {
            let parse = format!(r#"${{GetOptions}} $CMDLINE "{flag}" {var}"#);
            let set = format!("StrCpy {var} 1");
            let parsed = body.contains(&parse);
            let stored = *var == "$R0" || body.contains(&set);
            if !parsed || !stored {
                problems.push(format!(
                    "{NSI_PATH}: Function {name} must handle the {flag} switch (into {var})"
                ));
            }
        }
    }
    let args_at = on_install_success
        .iter()
        .position(|l| l == r#"${GetOptions} $CMDLINE "/ARGS" $R0"#);
    let relaunch_at = on_install_success
        .iter()
        .position(|l| l == r#"nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" "$R0""#);
    if !matches!((args_at, relaunch_at), (Some(a), Some(r)) if a < r) {
        problems.push(format!(
            "{NSI_PATH}: .onInstSuccess must relaunch witness.exe on /R with the /ARGS value \
             via RunAsUser"
        ));
    }

    // The running app is closed before any file is replaced or deleted.
    let app_check = r#"!insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}""#;
    for (name, body, first_touch) in [
        ("Install", &install, "File "),
        ("Uninstall", &uninstall_lines, "Delete "),
    ] {
        let checks: Vec<usize> = body
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_str() == app_check)
            .map(|(i, _)| i)
            .collect();
        let touch = body.iter().position(|l| l.starts_with(first_touch));
        let guarded = checks.len() == 1 && touch.is_some_and(|t| checks[0] < t);
        if !guarded {
            problems.push(format!(
                "{NSI_PATH}: Section {name} must run CheckIfAppIsRunning exactly once, before \
                 its first `{}` (found {})",
                first_touch.trim(),
                checks.len()
            ));
        }
    }
    let utils_squashed: Vec<String> = code_lines(utils).map(squash).collect();
    for needle in [
        r#"nsis_tauri_utils::FindProcessCurrentUser "${executableName}""#,
        r#"nsis_tauri_utils::KillProcessCurrentUser "${executableName}""#,
        r#"System::Call 'Oleaut32::SysAllocString(w "${BUNDLEID}") i.r3'"#,
    ] {
        if !utils_squashed.iter().any(|l| l == needle) {
            problems.push(format!("{UTILS_PATH}: missing `{needle}`"));
        }
    }
    if !contains("!insertmacro SetLnkAppUserModelId") {
        problems.push(format!(
            "{NSI_PATH}: shortcuts must carry the {} AUMID (SetLnkAppUserModelId)",
            legacy::BUNDLEID
        ));
    }
    if !has("WriteUninstaller \"$INSTDIR\\uninstall.exe\"") {
        problems.push(format!("{NSI_PATH}: must write $INSTDIR\\uninstall.exe"));
    }

    // The native app needs no WebView2; its old profile is removed.
    for gone in ["Section WebView2", "MicrosoftEdgeWebview2Setup", "NSISdl::"] {
        if contains(gone) {
            problems.push(format!(
                "{NSI_PATH}: WebView2 installation remnant `{gone}`"
            ));
        }
    }
    // ... in the Install section, after the copy (the app is closed by then),
    // and only that subdirectory: the parent goes non-recursively.
    let ebwebview = r#"RMDir /r "$LOCALAPPDATA\${BUNDLEID}\EBWebView""#;
    let parent = r#"RMDir "$LOCALAPPDATA\${BUNDLEID}""#;
    let last_file = install.iter().rposition(|l| l.starts_with("File "));
    match install.iter().position(|l| l == ebwebview) {
        None => problems.push(format!(
            "{NSI_PATH}: Section Install must remove the old EBWebView profile (`{ebwebview}`)"
        )),
        Some(i) => {
            if last_file.is_none_or(|f| i < f) {
                problems.push(format!(
                    "{NSI_PATH}: Section Install must remove EBWebView after copying the files"
                ));
            }
            if install.get(i + 1).map(String::as_str) != Some(parent) {
                problems.push(format!(
                    "{NSI_PATH}: the EBWebView removal must be followed by `{parent}`"
                ));
            }
        }
    }
    // A recursive delete of the app-data dirs is only allowed in the
    // uninstaller's opt-in "Delete app data" block.
    let app_data_dirs = ["$LOCALAPPDATA", "$APPDATA"]
        .map(|base| expand(&format!(r"{base}\${{BUNDLEID}}"), &defines).to_ascii_lowercase());
    let wipes_app_data = |l: &String| {
        recursive_rmdir_target(l, &defines).is_some_and(|t| app_data_dirs.contains(&t))
    };
    let opt_in = uninstall_lines
        .iter()
        .position(|l| l == "${If} $DeleteAppDataCheckboxState = 1")
        .map(|start| if_block(&uninstall_lines, start))
        .unwrap_or_default();
    let allowed = opt_in.iter().filter(|l| wipes_app_data(l)).count();
    let total = lines.iter().filter(|l| wipes_app_data(l)).count();
    if total > allowed {
        problems.push(format!(
            "{NSI_PATH}: recursive delete of the app data dir outside the uninstaller's \
             \"Delete app data\" block"
        ));
    }

    // No machine-local paths: everything comes in through /D defines.
    for line in &lines {
        let bytes = line.as_bytes();
        let absolute = bytes
            .windows(3)
            .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && w[2] == b'\\');
        if absolute {
            problems.push(format!("{NSI_PATH}: hard-coded absolute path: `{line}`"));
        }
    }
    for include in ["!include \"utils.nsh\"", "!include \"English.nsh\""] {
        if !has(include) {
            problems.push(format!("{NSI_PATH}: must `{include}` (relative)"));
        }
    }
    problems
}

/// The string literal assigned to `pub const <name>: &str =` in Rust source.
pub fn rust_str_const(source: &str, name: &str) -> Option<String> {
    let start = source.find(&format!("pub const {name}: &str"))?;
    let after_eq = start + source[start..].find('=')? + 1;
    let open = after_eq + source[after_eq..].find('"')? + 1;
    let close = open + source[open..].find('"')?;
    Some(source[open..close].to_string())
}

/// The core updater must poll the legacy feed and trust the legacy key.
pub fn check_updater_constants(updater_source: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, want) in [
        ("UPDATE_ENDPOINT", legacy::UPDATE_ENDPOINT),
        ("UPDATER_PUBKEY", legacy::UPDATER_PUBKEY),
    ] {
        match rust_str_const(updater_source, name) {
            Some(v) if v == want => {}
            Some(v) => problems.push(format!(
                "{UPDATER_SOURCE}: {name} = \"{v}\" differs from the legacy value \"{want}\""
            )),
            None => problems.push(format!("{UPDATER_SOURCE}: no `pub const {name}: &str`")),
        }
    }
    problems
}

/// The embedded pubkey must be the release key (ID 5A06F76D00E6ACD8).
pub fn check_pubkey(pubkey_b64: &str) -> Vec<String> {
    match crate::sign::pubkey_key_id(pubkey_b64) {
        Ok(id) if id == legacy::UPDATER_KEY_ID => Vec::new(),
        Ok(id) => vec![format!(
            "updater pubkey has key ID {id}, expected {}",
            legacy::UPDATER_KEY_ID
        )],
        Err(e) => vec![format!("updater pubkey does not decode: {e:#}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through `read`, so fixture strings match however the checkout
    /// converted line endings.
    fn repo_file(rel: &str) -> String {
        read(&workspace::repo_root(), rel).unwrap()
    }

    #[test]
    fn real_installer_script_passes() {
        let problems = check_installer(&repo_file(NSI_PATH), &repo_file(UTILS_PATH));
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn real_updater_constants_and_pubkey_pass() {
        let problems = check_updater_constants(&repo_file(UPDATER_SOURCE));
        assert!(problems.is_empty(), "{problems:#?}");
        assert!(check_pubkey(legacy::UPDATER_PUBKEY).is_empty());
    }

    #[test]
    fn the_reference_script_is_what_we_diverged_from() {
        // The verbatim 0.2.1 output fails exactly on the deliberate edits
        // (absolute paths, WebView2, no EBWebView cleanup, no /D inputs), which
        // proves the checks look at the right things.
        let reference = repo_file("installer/reference/installer.nsi");
        let problems = check_installer(&reference, &repo_file("installer/reference/utils.nsh"));
        let joined = problems.join("\n");
        assert!(joined.contains("absolute path"), "{joined}");
        assert!(joined.contains("WebView2 installation remnant"), "{joined}");
        assert!(joined.contains("EBWebView"), "{joined}");
        // ... but its identity and file list are the contract we keep.
        assert!(!joined.contains("UNINSTKEY"), "{joined}");
        assert!(!joined.contains("must copy exactly"), "{joined}");
        assert!(!joined.contains("switch"), "{joined}");
    }

    fn real_nsi_with(from: &str, to: &str) -> Vec<String> {
        let nsi = repo_file(NSI_PATH);
        assert!(
            nsi.contains(from),
            "fixture text `{from}` not in the script"
        );
        check_installer(&nsi.replacen(from, to, 1), &repo_file(UTILS_PATH))
    }

    #[test]
    fn identity_drift_is_caught() {
        let p = real_nsi_with(
            r#"!define BUNDLEID "com.github.cubityfirst.witness""#,
            r#"!define BUNDLEID "com.example.witness""#,
        );
        assert!(p.iter().any(|m| m.contains("BUNDLEID")), "{p:#?}");
        let p = real_nsi_with(
            r#"!define MANUFACTURER "github""#,
            r#"!define MANUFACTURER "CubityFirst""#,
        );
        // MANUPRODUCTKEY derives from MANUFACTURER, so both are reported.
        assert!(p.iter().any(|m| m.contains("MANUPRODUCTKEY")), "{p:#?}");
        let p = real_nsi_with(
            r#"!define INSTALLMODE "currentUser""#,
            r#"!define INSTALLMODE "perMachine""#,
        );
        assert!(p.iter().any(|m| m.contains("INSTALLMODE")), "{p:#?}");
    }

    #[test]
    fn file_list_and_data_safety_are_caught() {
        let p = real_nsi_with(
            "    File /a \"/oname=onnxruntime_providers_shared.dll\" \"${SRCDIR}\\onnxruntime_providers_shared.dll\"\n",
            "",
        );
        assert!(p.iter().any(|m| m.contains("must copy exactly")), "{p:#?}");
        let p = real_nsi_with("  RMDir \"$INSTDIR\"\n", "  RMDir /r \"$INSTDIR\"\n");
        assert!(p.iter().any(|m| m.contains("recursive delete")), "{p:#?}");
        let p = real_nsi_with(
            "  Delete \"$INSTDIR\\uninstall.exe\"\n",
            "  Delete \"$INSTDIR\\*.*\"\n",
        );
        assert!(p.iter().any(|m| m.contains("wildcard")), "{p:#?}");
        assert!(p.iter().any(|m| m.contains("uninstall.exe")), "{p:#?}");
    }

    /// Apply several `(from, to)` edits (first occurrence each) in order.
    fn real_nsi_edited(edits: &[(&str, &str)]) -> Vec<String> {
        let mut nsi = repo_file(NSI_PATH);
        for (from, to) in edits {
            assert!(
                nsi.contains(from),
                "fixture text `{from}` not in the script"
            );
            nsi = nsi.replacen(from, to, 1);
        }
        check_installer(&nsi, &repo_file(UTILS_PATH))
    }

    #[test]
    fn flags_and_relaunch_are_caught() {
        let p = real_nsi_with(
            r#"${GetOptions} $CMDLINE "/ARGS" $R0"#,
            r#"${GetOptions} $CMDLINE "/ARGUMENTS" $R0"#,
        );
        assert!(p.iter().any(|m| m.contains("/ARGS switch")), "{p:#?}");
        // The first /UPDATE parse is the installer's .onInit; un.onInit's
        // identical line must not stand in for it.
        let p = real_nsi_with(r#"  ${GetOptions} $CMDLINE "/UPDATE" $UpdateMode"#, "");
        assert!(
            p.iter()
                .any(|m| m.contains(".onInit must handle the /UPDATE")),
            "{p:#?}"
        );
        let p = real_nsi_with("    StrCpy $PassiveMode 1\n", "    StrCpy $PassiveMode 0\n");
        assert!(p.iter().any(|m| m.contains("/P switch")), "{p:#?}");
        let p = real_nsi_with("RequestExecutionLevel user", "RequestExecutionLevel admin");
        assert!(
            p.iter().any(|m| m.contains("RequestExecutionLevel")),
            "{p:#?}"
        );
    }

    #[test]
    fn misplaced_cleanup_and_app_checks_are_caught() {
        let ebwebview = "  RMDir /r \"$LOCALAPPDATA\\${BUNDLEID}\\EBWebView\"\n";
        // Moved into the uninstaller.
        let p = real_nsi_edited(&[
            (ebwebview, ""),
            (
                "  RMDir \"$INSTDIR\"\n",
                &format!("  RMDir \"$INSTDIR\"\n{ebwebview}"),
            ),
        ]);
        assert!(
            p.iter()
                .any(|m| m.contains("must remove the old EBWebView")),
            "{p:#?}"
        );
        // Moved before the copy.
        let p = real_nsi_edited(&[(ebwebview, ""), ("  ; Copy main executable\n", ebwebview)]);
        assert!(p.iter().any(|m| m.contains("after copying")), "{p:#?}");
        // A recursive parent would wipe the user's app data on every update.
        let p = real_nsi_with(
            "  RMDir \"$LOCALAPPDATA\\${BUNDLEID}\"\n",
            "  RMDir /r \"$LOCALAPPDATA\\${BUNDLEID}\"\n",
        );
        assert!(p.iter().any(|m| m.contains("followed by")), "{p:#?}");
        assert!(
            p.iter().any(|m| m.contains("outside the uninstaller")),
            "{p:#?}"
        );
        // The uninstaller's opt-in wipe is fine, but not outside its ${If}.
        let p = real_nsi_with(
            "    RmDir /r \"$LOCALAPPDATA\\${BUNDLEID}\"\n  ${EndIf}\n",
            "  ${EndIf}\n    RmDir /r \"$LOCALAPPDATA\\${BUNDLEID}\"\n",
        );
        assert!(
            p.iter().any(|m| m.contains("outside the uninstaller")),
            "{p:#?}"
        );
        // Both app checks in one section.
        let app_check =
            "  !insertmacro CheckIfAppIsRunning \"${MAINBINARYNAME}.exe\" \"${PRODUCTNAME}\"\n";
        let p = real_nsi_edited(&[
            (
                &format!("\n{app_check}\n  ; Delete the app directory"),
                "\n\n  ; Delete the app directory",
            ),
            (app_check, &format!("{app_check}{app_check}")),
        ]);
        assert!(
            p.iter().any(|m| m.contains("Section Install must run")),
            "{p:#?}"
        );
        assert!(
            p.iter().any(|m| m.contains("Section Uninstall must run")),
            "{p:#?}"
        );
    }

    #[test]
    fn defines_parse_and_expand() {
        let nsi = "!define A \"x\"\n; !define B \"commented\"\n!define C \"${A}\\y\"\n\
                   !define /ifndef D 1\n!define E bare\n";
        let d = nsi_defines(nsi);
        assert_eq!(d.get("A").map(String::as_str), Some("x"));
        assert!(!d.contains_key("B") && !d.contains_key("D"));
        assert_eq!(expand(&d["C"], &d), r"x\y");
        assert_eq!(d.get("E").map(String::as_str), Some("bare"));
    }

    #[test]
    fn updater_constants_are_read_from_source() {
        let src = "pub const UPDATE_ENDPOINT: &str =\n    \"https://x/latest.json\";\n\
                   pub const UPDATER_PUBKEY: &str = \"abc\";\n";
        assert_eq!(
            rust_str_const(src, "UPDATE_ENDPOINT").as_deref(),
            Some("https://x/latest.json")
        );
        assert_eq!(
            rust_str_const(src, "UPDATER_PUBKEY").as_deref(),
            Some("abc")
        );
        assert_eq!(check_updater_constants(src).len(), 2);
        assert_eq!(check_updater_constants("").len(), 2);
    }

    #[test]
    fn workflows_use_the_xtask_and_no_node() {
        for wf in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
            let text = repo_file(wf);
            for node in [
                "npm",
                "setup-node",
                "node-version",
                ".nvmrc",
                "tauri bundle",
            ] {
                assert!(!text.contains(node), "{wf} still references `{node}`");
            }
            // The NSIS cache key names the pinned hashes (a stale key would
            // only cost a download, but keep it honest).
            assert!(
                text.contains(crate::nsis::NSIS_SHA256),
                "{wf}: NSIS cache key"
            );
            assert!(
                text.contains(crate::nsis::TAURI_UTILS_SHA256),
                "{wf}: nsis_tauri_utils cache key"
            );
        }
        let ci = repo_file(".github/workflows/ci.yml");
        for step in [
            "cargo run --locked -p xtask -- check",
            "cargo run --locked -p xtask -- dist",
            "./installer/tests/upgrade-smoke.ps1",
            "cargo test --locked -p xtask sig_compat",
        ] {
            assert!(ci.contains(step), "ci.yml must run `{step}`");
        }
        let release = repo_file(".github/workflows/release.yml");
        assert!(release.contains("-- check --tag"));
        assert!(release.contains("-- dist --sign --notes"));
        assert!(!release.contains("--draft"), "a draft is never served");
    }

    #[test]
    fn upgrade_smoke_pins_the_shipped_021_installer() {
        // The SHA-256 of the v0.2.1 release asset, as PROVENANCE.md records it.
        let script = repo_file("installer/tests/upgrade-smoke.ps1");
        let provenance = repo_file("installer/reference/PROVENANCE.md");
        let pin = "f586931a520fd6a16a587a3d44dad129dbb73a2900643afa3646573d927a8e6d";
        assert!(script.contains(&format!("$OldSha256 = '{pin}'")));
        assert!(provenance.contains(pin));
        assert!(script.contains(&format!("$BundleId = '{}'", legacy::BUNDLEID)));
        assert!(script.contains(&format!("$ProductKey = '{}'", legacy::MANUPRODUCTKEY)));
        assert!(script.contains(&format!("$UninstKey = '{}'", legacy::UNINSTKEY)));
    }

    #[test]
    fn versions_and_tag() {
        let lock = "[[package]]\nname = \"witness-core\"\nversion = \"0.3.0\"\n\n\
                    [[package]]\nname = \"xtask\"\nversion = \"0.2.1\"\n";
        let pkgs = vec![
            ("witness-core".to_string(), "0.3.0".to_string()),
            ("xtask".to_string(), "0.3.0".to_string()),
        ];
        let p = check_versions("0.3.0", &pkgs, lock, Some("v0.3.0"));
        assert_eq!(p.len(), 1, "{p:#?}");
        assert!(p[0].contains("Cargo.lock records `xtask` 0.2.1"));
        let p = check_versions("0.3.0", &pkgs[..1], lock, Some("v0.3.1"));
        assert_eq!(p.len(), 1, "{p:#?}");
        assert!(p[0].contains("tag v0.3.1"));
    }
}
