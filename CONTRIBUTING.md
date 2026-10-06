# Contributing

Witness is a Windows-first Rust application with a native egui UI. Keep
changes small enough to review, preserve local-data safety, and add regression
tests for failure paths.

## Development setup

Install Rust with the MSVC target (`rust-toolchain.toml` pins the version),
Visual Studio Build Tools with Desktop C++ and CMake, and LLVM/libclang 19.x.
Put CMake on `PATH`; set `LIBCLANG_PATH` to the LLVM `bin` directory if
bindgen cannot find it. Developer machines must supply their own paths (for
example in an untracked `.cargo/config.toml`) rather than committing
machine-specific tool locations. No Node.js or npm is involved.

The repository root is a Cargo workspace: `crates/witness-core` (backend
library), `crates/witness` (the `witness.exe` app) and `xtask` (release
tooling, run as `cargo run -p xtask -- <command>`).

```powershell
cargo run -p witness
```

To keep a development build away from your real recordings, point
`WITNESS_SETTINGS` at a scratch `witness-settings.toml` whose `data_dir` is an
empty folder.

Model integration tests download gigabytes and real-device tests interact with
Windows audio. They are ignored by default; run them deliberately and never put
private meeting audio into fixtures or bug reports. Their models resolve to
`<target>\debug\models`, or to `WITNESS_MODELS_DIR` when set.

## Before opening a pull request

```powershell
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo run --locked -p xtask -- check
cargo audit --deny unsound --ignore RUSTSEC-2024-0429
```

Also perform the smallest relevant manual test: capture and playback for audio
changes, an offline/model-integrity test for model changes, database upgrade and
recovery for migrations, keyboard-only navigation for UI changes, and backup
verification for storage changes.

Do not commit recordings, model binaries, databases, logs, diagnostics reports,
machine-specific paths, credentials, or generated build directories. Update the
README, Privacy document, changelog, and release checklist when behavior or data
handling changes.

## Design expectations

- Keep meeting content and inference local. Any new network behavior requires a
  clear opt-in, UI disclosure, privacy-document update, and threat review.
- Treat recordings, transcripts, and voice prints as sensitive data.
- Prefer atomic publication and recoverable operations for persistent state.
- Validate paths before filesystem access; never join an untrusted absolute or
  parent-traversal path beneath an application root.
- Avoid unbounded queues and long uninterruptible inference calls.
- Preserve the single-process, tray-first design and explicit recorder state
  transitions.
- Never block the UI thread: blocking `AppCore` calls run through the app's
  task helpers (`Tasks::spawn` for loads, `Tasks::spawn_op` for actions), and
  debug builds assert this.
- Changes to the installer, update feed, signing, or `service/updater.rs` must
  keep installed 0.2.x and later copies able to update (see the release
  checklist); `cargo run -p xtask -- check` guards the fixed values.
