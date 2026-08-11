# Contributing

Witness is a Windows-first Rust/Tauri application. Keep changes small enough to
review, preserve local-data safety, and add regression tests for failure paths.

## Development setup

Install Rust stable with the MSVC target, Node.js 24.15.x (`>=24.15 <25`), Visual Studio Build Tools
with Desktop C++ and CMake, and LLVM/libclang 19.x. Put CMake on `PATH`; set
`LIBCLANG_PATH` to the LLVM `bin` directory if bindgen cannot find it. Developer
machines must supply their own paths rather than committing machine-specific
tool locations.

```powershell
npm ci
npm run tauri dev
```

Model integration tests download gigabytes and real-device tests interact with
Windows audio. They are ignored by default; run them deliberately and never put
private meeting audio into fixtures or bug reports.

## Before opening a pull request

```powershell
npm run check
npm audit --audit-level=high
Set-Location src-tauri
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo audit --deny unsound --ignore RUSTSEC-2026-0221 --ignore RUSTSEC-2024-0429
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
