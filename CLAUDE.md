# Witness

Local-only meeting recorder & transcriber for Windows 11 (Tauri 2, Rust
backend, Preact frontend). Detects Teams calls via the mic consent store,
records mic + system loopback as separate tracks, live-captions while
recording, transcribes on-device (Parakeet TDT on CUDA / Whisper on CPU)
with diarization and cross-meeting voice identification, and keeps a
full-text-searchable archive. Meeting content stays on the machine; only
explicit model/GPU-library downloads and the (optional) update check use
the network. It follows the same
single-process, tray-first philosophy as the sibling Vigil project.

PLAN.md is the original (implemented) design; this file is the living map.

## Architecture

One visible process, tray-first, **no detached daemon, no self-relaunch**
(AV heuristics — vigil's lesson). Closing the window hides to tray; tray
Quit finalizes any recording and exits. Interrupted work is recovered at
startup from db statuses + `rec-tmp/` sidecars.

```
meeting_watcher.rs ──(registry poll 2s, debounce 2/5 ticks)──┐
tray / hotkeys / UI commands ────────────────────────────────┤
                                                             ▼
                       commands::do_start_recording / do_stop_recording
                                                             │
        ┌────────────────────────────────────────────────────┤
        ▼                                                    ▼
audio_capture.rs (2 threads: mic + loopback,        live_transcribe.rs
  MTA COM, event-driven, f32 autoconvert)             (VAD-gated Parakeet on
        │ CaptureMsg (samples + QPC)                   speech pauses → live
        ▼                                              caption events + the
recorder.rs writer thread                              floating overlay)
  (48 kHz mono i16 WAVs, QPC alignment,                     ▲
   gap fill + drift trim, level meter,  ──── tee 48 kHz ────┘
   5s flush + crash sidecar)
        │ stop
        ▼
pipeline.rs (single FIFO worker; engines loaded per job, dropped after)
  WAV/Opus → 16 kHz → vad.rs chunks (≤240 s) → asr_{parakeet,whisper}.rs
  → diarize.rs (Nemotron 3 Diarization, loopback) → speaker_id.rs (voice prints,
  auto-label vs enrolled people) → db.rs transaction (FTS via triggers)
  → encoder.rs (stereo Ogg Opus comfort mix) → WAV cleanup → events
```

Frontend: Vite + Preact, no router — a `View` enum in `app.tsx`. Views:
Meetings (+ recycle bin), People (stats + weekly trend), Transcript
(player, live captions, bookmarks, notes), Search (filters), Settings.
Typed IPC wrappers in `src/lib/api.ts` / `events.ts`; Phosphor icons
inlined in `src/lib/icons.tsx`; in-app confirm modal in `src/lib/confirm.tsx`.
The window is chrome-less: the top bar is the drag region + custom
min/max/close (close hides to tray). A second webview window "captions"
(`#captions` hash route) is the always-on-top live-caption overlay.

## Data model (SQLite, WAL, `PRAGMA user_version` = 5)

- `meetings(id, started_at, ended_at, title, audio_path, duration_ms,
  status, engine, trigger, notes, deleted_at)` — status:
  recording→recorded→processing→transcribed|failed; `deleted_at` = recycle
  bin (purged after 30 days at startup); `notes` has its own FTS table.
- `speakers(id, meeting_id, label, display_name, person_id, auto_labeled,
  embedding, emb_seconds)` — label is immutable ('me', 'S1'..'S8');
  `embedding` = this speaker's voice print in this meeting.
- `people(id, name UNIQUE, embedding, sample_seconds, created_at)` —
  enrolled voice prints, L2-normalized f32 LE blobs.
- `segments(id, meeting_id, speaker_id, track, start_ms, end_ms, text)` +
  external-content FTS5 (`segments_fts`, porter unicode61).
- `bookmarks(id, meeting_id, at_ms, note)`.

## Design decisions (and why)

- **Device-level loopback, never per-process loopback on ms-teams.exe** —
  documented Windows bug records silence (Windows-classic-samples#414).
  Teams isolation instead comes from routing Teams to a dedicated output
  endpoint and selecting that stable endpoint in Witness.
- **Two tracks (mic + loopback) = free Me/Them separation**; recorded as
  two mono WAVs, archived as one stereo Opus with an **invertible comfort
  mix** (L=0.75·mic+0.25·loop, R mirrored, `WITNESS_MIX=75` in OpusTags):
  pleasant on headphones, still separable for retranscription. Legacy
  untagged files decode as hard-panned.
- **QPC timestamps drive track alignment**: writer pre-pads late starters,
  fills gaps >250 ms (loopback sends nothing while the system is silent —
  normal, not an error), and trims when a device clock runs fast.
- **Configured capture devices are strict**: if the named device is
  missing, capture retries and the writer pads silence — never fall back
  to default, which could silently record the wrong GoXLR channel.
- **Engines load per pipeline job and drop after** → VRAM freed between
  meetings. Live captions hold their own Parakeet instance only while
  recording. ort silently falls back CUDA→CPU; the realtime factor in the
  completion toast/log is the tell (CPU fallback is substantially slower).
  gpu.rs `preflight()` makes the fallback loud: it verifies the CUDA EP's
  load-time imports resolve (app dir → System32 → PATH), prepends known
  cuDNN install dirs (`CUDNN_PATH`, `NVIDIA\CUDNN` under Program Files /
  LOCALAPPDATA) to the process PATH when that repairs a gap, and warns
  naming exactly which DLLs are missing. Called at startup, before each
  Parakeet pipeline job / live session, and by `get_gpu_status` (Settings).
  gpu_libs.rs backs the Settings "Download GPU libraries" button (~1 GB):
  pinned NVIDIA PyPI wheels (immutable files.pythonhosted.org URLs +
  SHA-256), Range-resumable downloads, DLL extraction by basename into
  `data_dir/cuda/`, an atomic install manifest with quick fingerprints, and
  Repair — the verified dir is preflight's first candidate, so GPU kicks in
  without a restart.
- **Voice identification invariants**: auto-matches never mutate stored
  prints; only explicit renames enroll/update people. Renaming always
  creates/links a person (even print-less) so People stats unify.
  `speaker_id::rematch_all` retro-labels all stored per-speaker embeddings
  after any enrollment change — pure cosine over the DB, no audio.
  Retranscription carries user-set names forward by label.
- **Recycle bin over hard delete**: delete = `deleted_at` timestamp
  (excluded from list/search/stats/recovery), restore/purge in the bin UI,
  30-day auto-purge. Shift-click skips confirmations (in-app modal, not
  the OS dialog — WebView2 suppresses `window.confirm`).
- **Tray flyout**: left-click opens the main window anchored above the
  tray (clamped to the monitor work area), chrome-less, dismissed on blur.
  Focus loss with the cursor still on the window means a border grab →
  the flyout "pins" into a normal window instead of dismissing (resize
  grabs blur before any resize event fires).
- **Meeting auto-naming reads Teams window titles only** (12/30/60 s after
  auto-start; never overwrites a user rename). No process memory, no UIA
  scraping, no Graph API. The watcher snapshots Teams window handles while
  the mic is idle; naming prefers windows that appeared since (the call
  window) over the long-lived main window, whose title is just the open
  chat/tab. Falls back to all windows if no new ones survive filtering.
- **Meeting end stops any running recording**, not just auto-started ones —
  StopMeeting only fires after a watched meeting was detected and ended, so
  manual recordings made outside a call are never touched.
- **Live captions are provisional by design** — VAD-cut chunks on ~0.7 s
  pauses (15 s cap), tagged Me/Them only; the final pass replaces them
  with the diarized transcript. Timeline stays aligned with the WAVs
  because writer silence insertions are forwarded as `LiveMsg::Silence`.
- **Phantom-line defense is two-layered** (mic pops / AC hum used to become
  transcript lines): vad.rs demands ≥5 consecutive speech frames (~80 ms,
  `MIN_SPEECH_FRAMES`) at a 0.65 threshold before a span counts as speech —
  transients light up 1–2 frames; and `asr::is_junk_text` drops known
  noise hallucinations (lone "You", "Thank you.", YouTube-outro phrases)
  plus empty/punctuation-only output, in both the pipeline and live
  captions. Both constants are shared by live_transcribe.rs, which also
  refuses to cut a chunk until a run confirms real speech. Retranscribing
  an old meeting re-runs VAD, so it cleans up past phantom lines too.
- Search snippets use `\x01`/`\x02` markers (control chars can't occur in
  transcripts) rendered as `<mark>` by the frontend — never raw HTML.
- Exports (md/txt/srt/vtt) render backend-side; "download" actions go
  through native save dialogs (`<a download>` is unreliable in WebView2).

## Crate/version constraints (do not "upgrade" casually)

- **VAD is earshot, not Silero** — `voice_activity_detector` pins ort
  =2.0.0-rc.10; parakeet-rs needs rc.13. earshot frames: exactly 256
  samples @ 16 kHz.
- **Our direct `ort` dep is pinned `=2.0.0-rc.13`** to match parakeet-rs
  (shared ort-sys). Its CUDA provider hard-imports `cublas64_13` /
  `cublasLt64_13` and loads `cudnn64_9` / `cufft64_12` at runtime (CUDA 13
  runtime + cuDNN 9) — mirrored in gpu.rs `CUDA_EP_DLLS`; re-check the
  provider's imports and embedded DLL names when bumping ort. ort-sys only
  copies its DLLs into a target dir when they are *absent* (cross-drive =
  copy, not symlink), so after an ort bump delete the old
  `onnxruntime_providers_*.dll` from local target dirs or they go stale. speaker_id.rs runs its own CPU session (3D-Speaker
  ERes2Net, input `x` [1,T,80] kaldi fbank — 80 mel, 25/10 ms, povey, CMN,
  samples in [-1,1]; output `embedding` [1,192], L2-normalize ourselves;
  threshold user-tunable, default 0.6).
- **hf-hub stays on 0.4.x** — 1.0 is an incompatible rewrite. Models live
  in the hf-hub cache under `data_dir/models/hf-cache`; presence checks
  via `Cache` (offline), never `Api` (network).
- **wasapi 0.23**: loopback = *render* device opened with
  `Direction::Capture` + `StreamMode::EventsShared`; `initialize_mta()`
  per thread; wasapi objects are `!Send`.
- **Diarization = NVIDIA Nemotron 3 Diarization** (streaming Sortformer v3,
  OpenMDW-1.1) via parakeet-rs, ONNX from `altunenes/parakeet-rs`
  (`nemotron-3-diarization/`). Output units = samples @ 16 kHz (÷16 → ms);
  ≤8 speakers; runs on CPU (no execution config), offline profile from the
  ONNX metadata. **Never call `Sortformer::flush()`** — in 0.3.8 it hands
  ORT a column-major mel array whenever the tail is a whole number of 80 ms
  frames (~1 in 8 recordings); diarize.rs instead feeds one model window of
  silence and clamps segments. Remove the workaround once upstream fixes it
  (`diarize_tail_frame_multiple` is the regression test). Compared with v2
  on real meetings, v3 stops splitting one person into two speakers, but it
  merges the two stock SAPI voices (Hazel/Zira) — the TTS dialogue test
  pitches one down.
- **parakeet-rs is a git pin** (0.3.8 isn't on crates.io yet) — swap to the
  crates.io release when it lands. 0.3.8 dropped Sortformer v2 support.
  **TDT ~4–5 min inference cap** → vad.rs hard-splits at 240 s.
- **FTS5 probe queries need `LIMIT 1`** — with `LIMIT 0` the MATCH
  expression is never parsed and invalid syntax slips through.
- Whisper CUDA is behind the `whisper-cuda` cargo feature (needs the CUDA
  toolkit at build time); default build = CPU Whisper + CUDA Parakeet.
- Tauri frontend window-state calls (minimize/hide/drag…) need explicit ACL
  grants. Keep main-window commands in `capabilities/default.json`; the
  captions window remains isolated in `capabilities/captions.json`.

## Platform quirks encoded here

- Global hotkeys walk candidate chains (record: ctrl+alt+r→w→shift+r;
  bookmark: ctrl+alt+b→m) because other apps can own combos system-wide;
  bound combos live in AppState and are exposed through status commands.
- Toasts are attributed to PowerShell in dev — unpackaged apps have no
  AUMID registration; the NSIS install shows "Witness" correctly.
- `backgroundColor` on windows kills the white flash when resizing.
- **No large stack arrays.** Sync Tauri commands run on the main thread,
  which has Windows' 1 MB stack, and release inlining hoists a callee's
  array into the caller's frame even when that branch never runs: a 1 MB
  hash buffer in `digest_reader` made `get_model_status` overflow at launch
  once models were installed (0.2.0). Buffers go on the heap (`vec!`).
- witness.log rotates at 5 MB (one `.old`). Crash recovery patches WAV
  RIFF/data sizes from file length (`repair_wav_header`) before reading.
- NSIS bundles the ort CUDA provider DLLs via `bundle.resources` — they
  sit next to the exe in target/release but the bundler won't pick them up
  otherwise.

## Building & testing

Install CMake and libclang through Visual Studio Build Tools/LLVM, put CMake
on `PATH`, and set `LIBCLANG_PATH` when needed. Do not commit machine-local
tool paths. LLVM stays on **19.x** because bindgen 0.71 (whisper-rs-sys) is
not validated with newer LLVM releases.

- `cargo test` (in src-tauri) — fast unit suite. Use
  `CARGO_TARGET_DIR=target-test` when `npm run tauri dev` is running (the
  dev app locks the ort DLLs the build script copies).
  When ort-sys has to *copy* (not symlink) its DLLs it skips `deps/`, so
  test binaries can't see `onnxruntime_providers_cuda.dll` and `cuda_smoke`
  silently runs on CPU — copy the two provider DLLs into
  `<target>/<profile>/deps/` for a real GPU check.
- Ignored, real-world tests: `capture_smoke` (records 4 s from the real
  devices; WITNESS_TEST_MIC/LOOPBACK env override), `cuda_smoke`
  (downloads Parakeet+diarization to target/debug/models, CUDA inference),
  `tts_dialogue_diarization` / `diarize_tail_frame_multiple` (diarization),
  `tts_transcribe` / `whisper_tts` (SAPI speech end-to-end, both engines),
  `voice_print_discrimination` (two TTS voices, same/cross cosine),
  `live_captions` (streams TTS through the live worker),
  `list_devices_smoke`.
- `npm run tauri dev` for development; `npm run release:windows` for the
  (unsigned) NSIS installer.

## Releases & updates

Installed builds update themselves from GitHub Releases (updater.rs,
tauri-plugin-updater). The NSIS installer is per-user (`currentUser`), so
updates never need elevation. To release: `npm run version:set -- X.Y.Z`
(bumps package.json, tauri.conf.json, Cargo.toml and both lockfiles),
commit, push tag `vX.Y.Z` → `.github/workflows/release.yml` runs
`build-windows-release.ps1 -Sign` (adds `tauri.updater.conf.json` →
installer `.sig`, writes `latest.json`) and publishes the release. The
updater reads `releases/latest/download/latest.json` and verifies the
minisign signature against `plugins.updater.pubkey`; the private key lives
outside the repo (`~/.tauri/witness-updater.key` + the
`TAURI_SIGNING_PRIVATE_KEY` repo secret) — losing it means shipping a new
pubkey through a manual reinstall. Startup check after 30 s
(`check_for_updates` setting) only toasts; installing is always a click
in Settings → Updates and is refused while recording or backing up,
because on Windows the plugin launches the installer and
`process::exit(0)`s (skipping tray-Quit finalization); persisted pipeline
jobs resume on relaunch.

## Settings & files

`WITNESS_SETTINGS` env var → `witness-settings.toml` (next to exe) →
`data_dir`: `witness.db`, `audio/{id}.opus`, `rec-tmp/` (in-flight WAVs +
`.meta.toml` crash sidecars), `models/hf-cache/`, `cuda/` (optional managed
GPU DLLs), `witness.log`.
data_dir changes apply to db/log on restart (Settings offers the button).

## Known limitations (accepted)

- No echo cancellation: speakers instead of a headset can bleed remote audio
  into the mic track; a headset is recommended.
- Diarization caps at 8 remote speakers; voice matching on compressed
  Teams audio is good-but-not-perfect (hence the ≈ marker + rename flow).
- DB and audio are unencrypted at rest (local-only threat model).
- Live captions lost on page reload aren't backfilled (the final
  transcript covers everything regardless).
