# Witness

Local-only meeting recorder & transcriber for Windows 11 (Tauri 2, Rust
backend, Preact frontend). Detects Teams calls via the mic consent store,
records mic + system loopback as separate tracks, live-captions while
recording, transcribes on-device (Parakeet TDT on CUDA / Whisper on CPU)
with diarization and cross-meeting voice identification, and keeps a
full-text-searchable archive. Nothing leaves the machine. Sibling project
to Vigil (`G:\Scripts\vigil`) — same single-process, tray-first philosophy.

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
  → diarize.rs (Sortformer, loopback) → speaker_id.rs (voice prints,
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

## Data model (SQLite, WAL, `PRAGMA user_version` = 4)

- `meetings(id, started_at, ended_at, title, audio_path, duration_ms,
  status, engine, trigger, notes, deleted_at)` — status:
  recording→recorded→processing→transcribed|failed; `deleted_at` = recycle
  bin (purged after 30 days at startup); `notes` has its own FTS table.
- `speakers(id, meeting_id, label, display_name, person_id, auto_labeled,
  embedding, emb_seconds)` — label is immutable ('me', 'S1'..'S4');
  `embedding` = this speaker's voice print in this meeting.
- `people(id, name UNIQUE, embedding, sample_seconds, created_at)` —
  enrolled voice prints, L2-normalized f32 LE blobs.
- `segments(id, meeting_id, speaker_id, track, start_ms, end_ms, text)` +
  external-content FTS5 (`segments_fts`, porter unicode61).
- `bookmarks(id, meeting_id, at_ms, note)`.

## Design decisions (and why)

- **Device-level loopback, never per-process loopback on ms-teams.exe** —
  documented Windows bug records silence (Windows-classic-samples#414).
  Teams isolation instead comes from per-app audio routing: the GoXLR
  "Chat" output carries only Teams; Witness captures that device by name.
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
  completion toast/log is the tell (single digits on this box = CPU).
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
  =2.0.0-rc.10; parakeet-rs needs rc.12. earshot frames: exactly 256
  samples @ 16 kHz.
- **Our direct `ort` dep is pinned `=2.0.0-rc.12`** to match parakeet-rs
  (shared ort-sys). speaker_id.rs runs its own CPU session (3D-Speaker
  ERes2Net, input `x` [1,T,80] kaldi fbank — 80 mel, 25/10 ms, povey, CMN,
  samples in [-1,1]; output `embedding` [1,192], L2-normalize ourselves;
  threshold user-tunable, default 0.6).
- **hf-hub stays on 0.4.x** — 1.0 is an incompatible rewrite. Models live
  in the hf-hub cache under `data_dir/models/hf-cache`; presence checks
  via `Cache` (offline), never `Api` (network).
- **wasapi 0.23**: loopback = *render* device opened with
  `Direction::Capture` + `StreamMode::EventsShared`; `initialize_mta()`
  per thread; wasapi objects are `!Send`.
- **Sortformer output units = samples @ 16 kHz** (÷16 → ms); ≤4 speakers.
  **TDT ~4–5 min inference cap** → vad.rs hard-splits at 240 s.
- **FTS5 probe queries need `LIMIT 1`** — with `LIMIT 0` the MATCH
  expression is never parsed and invalid syntax slips through.
- Whisper CUDA is behind the `whisper-cuda` cargo feature (needs the CUDA
  toolkit at build time); default build = CPU Whisper + CUDA Parakeet.
- Tauri frontend window-state calls (minimize/hide/drag…) need explicit
  ACL grants in `capabilities/default.json` — `core:default` is read-only.
  The "captions" window must stay in that file's `windows` list.

## Platform quirks encoded here

- Global hotkeys walk candidate chains (record: ctrl+alt+r→w→shift+r;
  bookmark: ctrl+alt+b→m) because other apps own combos system-wide; bound
  combos live in AppState (get_hotkey). Ctrl+alt+r is taken on this box.
- Toasts are attributed to PowerShell in dev — unpackaged apps have no
  AUMID registration; the NSIS install shows "Witness" correctly.
- `backgroundColor` on windows kills the white flash when resizing.
- witness.log rotates at 5 MB (one `.old`). Crash recovery patches WAV
  RIFF/data sizes from file length (`repair_wav_header`) before reading.
- NSIS bundles the ort CUDA provider DLLs via `bundle.resources` — they
  sit next to the exe in target/release but the bundler won't pick them up
  otherwise.

## Building & testing

cmake + libclang aren't on PATH; `.cargo/config.toml` `[env]` points cargo
at them (VS BuildTools' cmake.exe, `C:\Program Files\LLVM\bin`). LLVM must
stay **19.x** — bindgen 0.71 (whisper-rs-sys) miscompiles under LLVM 22.

- `cargo test` (in src-tauri) — fast unit suite. Use
  `CARGO_TARGET_DIR=target-test` when `npm run tauri dev` is running (the
  dev app locks the ort DLLs the build script copies).
- Ignored, real-world tests: `capture_smoke` (records 4 s from the real
  devices; WITNESS_TEST_MIC/LOOPBACK env override), `cuda_smoke`
  (downloads Parakeet+Sortformer to target/debug/models, CUDA inference),
  `tts_transcribe` / `whisper_tts` (SAPI speech end-to-end, both engines),
  `voice_print_discrimination` (two TTS voices, same/cross cosine),
  `live_captions` (streams TTS through the live worker),
  `list_devices_smoke`.
- `npm run tauri dev` / `npm run tauri build` (NSIS installer).

## Settings & files (vigil layering)

`WITNESS_SETTINGS` env var → `witness-settings.toml` (next to exe) →
`data_dir`: `witness.db`, `audio/{id}.opus`, `rec-tmp/` (in-flight WAVs +
`.meta.toml` crash sidecars), `models/hf-cache/`, `witness.log`.
data_dir changes apply to db/log on restart (Settings offers the button).
On this machine the GoXLR devices are configured: mic "Chat Mic
(TC-HELICON GoXLR)", loopback "Chat (TC-HELICON GoXLR)".

## Known limitations (accepted)

- No echo cancellation: speakers instead of a headset would bleed remote
  audio into the mic track (fine on the GoXLR headset).
- Diarization caps at 4 remote speakers; voice matching on compressed
  Teams audio is good-but-not-perfect (hence the ≈ marker + rename flow).
- DB and audio are unencrypted at rest (local-only threat model).
- Live captions lost on page reload aren't backfilled (the final
  transcript covers everything regardless).
