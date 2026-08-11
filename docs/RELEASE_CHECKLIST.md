# Release checklist

## Scope and metadata

- [ ] Review every change since the prior tag and update `CHANGELOG.md`.
- [ ] Update the version consistently in `package.json`, `src-tauri/Cargo.toml`,
      and `src-tauri/tauri.conf.json`; refresh lockfiles intentionally.
- [ ] Confirm README, Privacy, Security, backup/recovery, and model-size details
      match actual behavior.
- [ ] Confirm all model repositories, immutable revisions, sizes, hashes,
      licenses, and attribution.

## Automated gates

- [ ] `npm ci`, `npm run check`, and `npm audit --audit-level=high` pass from a
      clean checkout; re-review every documented advisory waiver.
- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --locked --all-targets -- -D warnings` passes.
- [ ] `cargo test --locked` passes; ignored hardware/model tests have an
      explicit run/waiver record.
- [ ] The documented `cargo audit --deny unsound …` gate passes and every
      ignored/non-blocking warning is rechecked against the Windows tree.
- [ ] A release build succeeds with no machine-specific absolute tool paths.

## Windows manual matrix

- [ ] Fresh install, first launch, single-instance behavior, tray close/quit,
      autostart, and uninstall on a clean supported Windows 11 account.
- [ ] Manual record/stop, auto Teams detection, mic and loopback playback,
      device disconnect/reconnect, capture-loss warning, and crash recovery.
- [ ] Parakeet and Whisper transcription, live captions, diarization, voice
      enrollment/forgetting, retranscription, search, and exports.
- [ ] Model first download, cancellation/failure recovery, repair, offline
      verified startup, and deliberate corrupted-file rejection.
- [ ] Recycle-bin restore, permanent purge, 30-day retention, and missing-audio
      error paths.
- [ ] Backup create/cancel/failure, checksum verification, and non-destructive
      restore into a new empty data directory.
- [ ] Keyboard-only UI, visible focus, screen-reader names/status, high contrast,
      forced colors, reduced motion, compact window, and error/retry states.
- [ ] Diagnostics report reviewed to ensure it contains no meeting content,
      notes, voice prints, or private settings.

## Artifact and publication

- [ ] Verify the NSIS bundle contains the expected runtime provider DLLs and no
      recordings, databases, logs, local paths, caches, or developer secrets.
- [ ] Scan/sign the installer according to the project's release policy and
      record its SHA-256.
- [ ] Install and launch the exact final artifact before tagging.
- [ ] Publish release notes with known limitations, privacy/consent reminder,
      upgrade/rollback notes, and installer checksum.
- [ ] Tag the reviewed commit; preserve build logs and test evidence.
- [ ] Monitor private security reports and critical install/model regressions.
