# Release checklist

Releases are built, signed and published by `.github/workflows/release.yml`
using the `xtask` crate (`cargo run -p xtask -- help` lists its commands).
Every release must keep installed copies able to update: the feed URL
(`releases/latest/download/latest.json`) and its schema, the minisign key
(key ID `5A06F76D00E6ACD8`; private key `~/.tauri/witness-updater.key`, repo
secret `TAURI_SIGNING_PRIVATE_KEY`, never rotated), a single NSIS `.exe` that
honours `/P /UPDATE /R /ARGS /S /NS`, the per-user install location
(`HKCU\Software\github\Witness`, default `%LOCALAPPDATA%\Witness`), the
`witness.exe` name, and never deleting user data in the install directory.

## Scope and metadata

- [ ] Review every change since the prior tag and update `CHANGELOG.md`.
- [ ] Set the version with `cargo run -p xtask -- version X.Y.Z` (root
      `Cargo.toml` `[workspace.package]` plus the workspace entries in
      `Cargo.lock`) and commit it. Use a plain numeric `X.Y.Z`: never a semver
      prerelease such as `0.3.0-rc.1`, because the installer's
      `VIProductVersion` must be numeric.
- [ ] Confirm README, Privacy, Security, backup/recovery, and model-size details
      match actual behavior.
- [ ] Confirm all model repositories, immutable revisions, sizes, hashes,
      licenses, and attribution.

## Automated gates

- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --workspace --locked --all-targets -- -D warnings` passes.
- [ ] `cargo test --workspace --locked` passes; ignored hardware/model tests
      have an explicit run/waiver record.
- [ ] `cargo run --locked -p xtask -- check --tag vX.Y.Z` passes: the tag
      matches the workspace version, `installer/witness.nsi` keeps the
      install identity and lists exactly the two ONNX Runtime provider DLLs,
      and the updater endpoint and public key equal the legacy values.
- [ ] The documented `cargo audit --deny unsound …` gate passes and every
      ignored/non-blocking warning is rechecked against the Windows tree.
- [ ] `cargo run -p xtask -- dist` succeeds from a clean checkout with no
      machine-specific absolute tool paths; it verifies the fresh
      `witness.exe` version resource and that both provider DLLs are present
      and non-empty before running `makensis`.
- [ ] CI's `upgrade-smoke` job is green on the release commit: it installs the
      shipped 0.2.1, updates over it with `/S /UPDATE` and
      `/P /UPDATE /R /ARGS --minimized`, and checks the registry keys,
      `Run\Witness`, shortcut AUMID, surviving user data, the removed WebView2
      profile, the relaunch, and uninstall. Its `sig_compat` test checks that
      xtask signatures pass the 0.2.x updater's own verification.

## Windows manual matrix

- [ ] Fresh install, first launch, single-instance behavior, tray close/quit,
      tray flyout, autostart, and uninstall on a clean supported Windows 11
      account.
- [ ] Manual record/stop, auto Teams detection, mic and loopback playback,
      device disconnect/reconnect, capture-loss warning, and crash recovery.
- [ ] Parakeet and Whisper transcription, live captions (in-app feed and
      overlay, including while the main window is minimized or hidden),
      diarization, voice enrollment/forgetting, retranscription, search, and
      exports.
- [ ] Model first download, cancellation/failure recovery, repair, offline
      verified startup, and deliberate corrupted-file rejection.
- [ ] Recycle-bin restore, permanent purge, 30-day retention, and missing-audio
      error paths.
- [ ] Backup create/cancel/failure, checksum verification, and non-destructive
      restore into a new empty data directory.
- [ ] Keyboard-only UI, visible focus, screen-reader names/status (Narrator),
      the Windows text-size setting, reduced motion, compact window, and
      error/retry states. High-contrast themes are not followed (known
      limitation).
- [ ] Diagnostics report reviewed to ensure it contains no meeting content,
      notes, voice prints, or private settings.

## Update continuity (first native release, and after any installer, updater or signing change)

- [ ] Upgrade with real state: install over 0.2.1 that has models installed
      (GPU and CPU) and a customised `data_dir`. Launch both with
      `--minimized` (as autostart does) and without. The app starts, shows its
      data, and does not crash at launch.
- [ ] Clean Windows user, manual upgrade: install the real
      `Witness_0.2.1_x64-setup.exe`, enable autostart, record a meeting and
      create a desktop shortcut, then run
      `Witness_X.Y.Z_x64-setup.exe /P /UPDATE /R /ARGS --minimized`. Verify the
      same install directory, `DisplayVersion` = X.Y.Z, data and settings
      intact, shortcuts and the `Run\Witness` value unchanged (autostart
      still shows on in Settings), toasts attributed to "Witness", the app
      relaunched minimized, and the old process gone.
- [ ] End to end through the real 0.2.x updater: build 0.2.1 (`87820ee`) with
      only the endpoint pointed at a test repository's `latest.json`, publish
      the new artifacts there, and click Install in Settings → Updates.
- [ ] GUI install over 0.2.1; then uninstall and confirm the data in
      `%LOCALAPPDATA%\Witness` survives; then reinstall and confirm a custom
      install directory is restored.
- [ ] Native updater: from an installed native build, update to a higher test
      build published to the test repository, once behind a system-configured
      or TLS-inspecting proxy if one is available.

## Artifact and publication

- [ ] Verify the installer contains `witness.exe`, the two ONNX Runtime
      provider DLLs and the uninstaller, and no recordings, databases, logs,
      local paths, caches, or developer secrets.
- [ ] Scan the installer according to the project's release policy and record
      its SHA-256 (`xtask dist` prints it).
- [ ] Install and launch the exact final artifact before tagging.
- [ ] Push the tag `vX.Y.Z`. The release workflow runs `xtask check --tag`,
      the tests, and `xtask dist --sign`, which signs with the trusted comment
      `timestamp:…\tfile:…\tversion:X.Y.Z` (native clients require the
      `version:` field), self-verifies the signature against the pinned key,
      and writes `latest.json` with both `windows-x86_64` and
      `windows-x86_64-nsis` entries.
- [ ] Prerelease first for risky releases (always for the first native
      release): tag pushes publish a GitHub **prerelease** while the
      repository variable `RELEASE_PRERELEASE` is not `false`; manual runs use
      the `prerelease` input. `releases/latest` ignores prereleases, so
      installed copies do not see it. Dogfood it on real meetings for at
      least a few days, then promote with
      `gh release edit vX.Y.Z --prerelease=false --latest`. Never publish a
      draft.
- [ ] Publish release notes with known limitations, privacy/consent reminder,
      upgrade/rollback notes, and installer checksum. For the first native
      release they must also say: the UI was rewritten; player volume, speed
      and the model-banner dismissal reset once; the leftover WebView2 profile
      is removed; the accepted regressions (high-contrast palette,
      right-to-left display, notes spellcheck, cross-row selection in long
      transcripts — use Export → Copy to clipboard); and that choosing "uninstall first"
      in the GUI installer over 0.2.1 removes shortcuts and autostart, as it
      always has.
- [ ] Preserve build logs and test evidence.
- [ ] Monitor private security reports and critical install/model regressions.

## Rollback runbook

If a promoted release is broken, ship a higher version built from the last
good tag so every client updates "forward" to it. For a broken 0.3.0:

1. `git worktree add .worktrees/rollback v0.2.1`.
2. In that worktree, bump to `0.3.1` with that tag's own tooling:
   `npm ci`, then `npm run version:set -- 0.3.1` (needs Node.js 24.15.x).
3. Build it unsigned with the old flow: `npm run release:windows`.
4. From the current branch, sign it with the xtask so the signature carries
   `version:` (which native clients require):
   `cargo run -p xtask -- sign <Witness_0.3.1_x64-setup.exe> --version 0.3.1 --feed latest.json --url https://github.com/CubityFirst/Witness/releases/download/v0.3.1/Witness_0.3.1_x64-setup.exe`
   (`--url` is the installer asset URL, not the release page).
5. Publish the installer, `.sig` and `latest.json` as a normal (latest)
   release `v0.3.1`.

0.3.0 users then update forward to the rollback build, and 0.2.x users also
receive it. For later releases, build the rollback from the last good native
tag with `cargo run -p xtask -- version` and `xtask dist --sign` instead.
