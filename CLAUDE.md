# Witness

Local-only meeting recorder & transcriber (Windows 11, Tauri 2 + Preact).
Detects Teams calls, records mic + system loopback, transcribes on-device
with speaker labels, archives everything searchable. See PLAN.md for the
original design; this file records what matters when changing code.

## Architecture (single process, tray-first)

One visible tray process — **no detached daemon, no self-relaunch** (AV
heuristics; lesson inherited from vigil). Closing the window hides to tray;
tray Quit finalizes any recording and exits. Interrupted work is recovered
at startup from db statuses + `rec-tmp/` sidecars (`recover_orphans` in
`main.rs`).

Data flow:

```
meeting_watcher (registry poll, 2 s)          commands.rs (UI)
        └─ do_start/do_stop_recording ────────────┘
                recorder.rs: 2 wasapi capture threads → writer thread
                → rec-tmp/{id}-mic.wav + {id}-loopback.wav (48 kHz mono i16)
        stop → pipeline.rs (single FIFO worker):
                WAVs → 16 kHz → vad chunks → ASR (both tracks)
                → Sortformer diarize (loopback) → merge → db (FTS5)
                → encoder.rs (stereo Ogg Opus, invertible comfort mix
                   L=0.75·mic+0.25·loop / R=0.25·mic+0.75·loop, tagged
                   WITNESS_MIX=75 in OpusTags) → delete WAVs
```

## Non-obvious constraints (do not "simplify" these away)

- **Never use per-process loopback** on ms-teams.exe — Windows bug records
  silence. Device-level loopback on the default render device only.
- **wasapi 0.23**: loopback = *render* device opened with
  `Direction::Capture` + `StreamMode::EventsShared`. COM (`initialize_mta`)
  per capture thread; wasapi objects are `!Send` — never touch them outside
  their thread. Loopback delivers **no packets while the system is silent**;
  the writer fills gaps from QPC timestamps (also covers device switches
  and >250 ms drift).
- **VAD is earshot, not Silero** — `voice_activity_detector` pins
  ort =2.0.0-rc.10, parakeet-rs needs rc.12. Frames are exactly 256 samples
  @ 16 kHz.
- **TDT ~4–5 min inference cap** → vad.rs hard-splits chunks at 240 s.
- **Sortformer output units = samples @ 16 kHz** (÷16 → ms). Max 4 speakers.
- **hf-hub stays on 0.4.x** — 1.0 is an incompatible rewrite. Models live in
  the hf-hub cache under `data_dir/models/hf-cache`; presence checks via
  `Cache` (offline), never `Api` (would hit network).
- **FTS5 passthrough probe needs `LIMIT 1`** — with `LIMIT 0` SQLite never
  parses the MATCH expression and invalid syntax slips through.
- Search snippets use `\x01`/`\x02` markers (control chars can't appear in
  transcripts); frontend renders them as `<mark>` — never send raw HTML.
- Engines load per pipeline job and drop after (frees VRAM). ort silently
  falls back CUDA→CPU; `get_gpu_status` is only a driver-presence hint.
- **Speaker ID (`speaker_id.rs`)**: our own ort session (dep pinned
  `=2.0.0-rc.12` to match parakeet-rs — never drift these apart). Kaldi
  fbank front-end (80 mel, 25/10 ms, povey, CMN) feeds a WeSpeaker
  embedding model; cosine ≥ MATCH_THRESHOLD auto-labels. Invariant:
  auto-matches never mutate voice prints; only explicit renames
  (commands::rename_speaker) enroll or update people. Per-meeting speaker
  embeddings are stored on the speakers row so late renames can enroll
  without recomputing.
- Whisper CUDA is gated behind the `whisper-cuda` cargo feature (needs CUDA
  toolkit at build time); default build is CPU whisper + CUDA parakeet.
- **Live captions (`live_transcribe.rs`)**: recorder's writer thread tees
  48 kHz audio (and silence gaps) into a worker that VAD-gates and runs
  Parakeet on speech pauses (~0.7 s) or 15 s buffers. Provisional lines via
  `live-transcript` events; the pipeline's final pass replaces them. Worker
  loads its own engine per recording and exits (freeing VRAM) when the
  recorder drops the channel. Timeline stays aligned with the WAVs because
  silence insertions are forwarded as `LiveMsg::Silence`.
- Capture devices are selectable by friendly name (settings `mic_device` /
  `loopback_device`, resolved exact-then-substring). A configured name that
  isn't present is an error (capture retries; writer pads silence) — never
  silently fall back to default, that could record the wrong GoXLR channel.
  On this machine: GoXLR "Chat"/"Chat Mic" isolate Teams from other audio.
- Retranscribing preserves user-set speaker names by label (pipeline copies
  them forward before `replace_transcript`); only auto labels are re-derived.
- `speaker_id::rematch_all` retro-labels stored voice prints after any
  enrollment change (rename_speaker / delete_person spawn it) — pure DB
  cosine math, no audio.
- Meeting auto-naming reads Teams *window titles* only (meeting_title.rs),
  polls 12/30/60 s after auto-start, and never overwrites a user rename.
- Captions overlay = second webview window "captions" (#captions hash
  route); must stay listed in capabilities/default.json `windows`.
- Global record hotkey walks a candidate chain (ctrl+alt+r → w →
  shift+r) because other apps hold combos system-wide; the bound one is in
  AppState.hotkey (get_hotkey command). On this machine ctrl+alt+r is taken.
- witness.log rotates at 5 MB (one .old generation). Crash recovery patches
  WAV RIFF/data sizes from file length before reading (repair_wav_header).

## Building

cmake + libclang aren't on PATH; `.cargo/config.toml` `[env]` points cargo's
build scripts at them (VS BuildTools' bundled cmake.exe, `C:\Program
Files\LLVM\bin`), so plain `npm run tauri dev` / `cargo build` just work.
If those tools move, update `.cargo/config.toml`.

LLVM must be **19.x** — bindgen 0.71 (whisper-rs-sys) generates broken
bindings with LLVM 22 (struct size assertions fail).

- `cargo test` (in src-tauri) — fast unit tests.
- `cargo test capture_smoke -- --ignored --nocapture` — records 4 s from the
  real default devices.
- `cargo test cuda_smoke -- --ignored --nocapture` — downloads Parakeet +
  Sortformer into `target/debug/models` and runs a CUDA inference.
- `npm run tauri dev` / `npm run tauri build` (NSIS).

## Settings layering (vigil pattern)

`WITNESS_SETTINGS` env var → `witness-settings.toml` (next to exe) →
`data_dir` (db, audio/, rec-tmp/, models/, witness.log). Changing data_dir
in the UI takes effect for db/log on next launch.
