# Witness — Local Meeting Recorder & Transcriber

> **Handoff note:** This plan was researched and approved in a prior Claude Code session. Nothing has been implemented yet — this file is the only thing in the repo. Start at Phase 1 (bottom of this file). All decisions below were confirmed with the user; items marked ⚠ VERIFY need checking against current crate docs before use.

## Context

A **local-only** voice transcription tool for meetings (primarily Microsoft Teams on Windows 11): detect when the user is in a meeting, record an audio copy, auto-transcribe with speaker labels, and provide a browsable, full-text-searchable archive of all past meetings. No cloud services — everything on-device. Named **Witness**, a sibling to the user's process-watcher **Vigil** (`G:\Scripts\vigil`).

Confirmed decisions:
- **Name:** Witness, project root `G:\Scripts\Witness`
- **UI:** Tauri 2 (Rust backend + webview frontend)
- **ASR:** switchable engines — **Parakeet TDT 0.6B v3** primary (via `parakeet-rs`, ONNX Runtime + CUDA), **Whisper large-v3-turbo** secondary (via `whisper-rs`)
- **Recording:** auto-starts when a Teams call is detected (with tray notification); manual record button too
- **Hardware:** RTX 4080 (16 GB), Ryzen 7800X3D, Windows 11 build 26200

## Key research findings (bake into implementation)

- **Do NOT use per-process loopback** (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`) on `ms-teams.exe` — documented bug records silence (microsoft/Windows-classic-samples#414). Use **device-level WASAPI loopback** on the default render device instead, plus a separate default-mic capture. Two tracks = free "Me vs Them" separation.
- **Meeting detection:** Windows Capability Access Manager consent store, `HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone` — new Teams is MSIX-packaged (`MSTeams_8wekyb3d8bbwe`) so it's a direct subkey (classic apps under `NonPackaged\`, exe paths with `\`→`#`). `LastUsedTimeStart != 0 && LastUsedTimeStop == 0` (REG_QWORD FILETIMEs) ⇔ mic actively in use. Teams holds the mic open for the whole call even while muted — tracks meeting membership exactly.
- **`parakeet-rs` crate** (github.com/altunenes/parakeet-rs, actively maintained): Parakeet TDT via ONNX Runtime + **Sortformer speaker diarization (up to 4 speakers)**. Parakeet TDT v3: better English WER than Whisper large-v3 (6.3% vs 7.4%), ~40–50× faster, 25 European languages. TDT has a **~4–5 min per-inference audio limit** → VAD-based chunking with a hard 240 s cap.
- **Vigil conventions to copy** (`G:\Scripts\vigil\CLAUDE.md`, `src/settings.rs`): single process + tray, **no detached windowless daemon** (trips AV heuristics), settings layering env var → `witness-settings.toml` next to exe → `data_dir`, flat module-per-file `src/`, release profile `lto=true, codegen-units=1, strip=true`, MIT license.

## Stack

**src-tauri deps:** `tauri = "2"` (tray-icon), `tauri-plugin-notification`, `tauri-plugin-single-instance`, `tauri-plugin-dialog`, `serde`/`serde_json`/`toml`, `chrono`, `anyhow`/`thiserror`, `log` + `simplelog`, `crossbeam-channel`, `rusqlite` (bundled → FTS5), `hound` (WAV), `rubato` (resampling), `audiopus` + `ogg` (Opus encode; fallback `flacenc` if Ogg muxing fights back), `parakeet-rs` (cuda), `whisper-rs` (behind a `whisper-cuda` feature; default CPU build), `voice_activity_detector` (Silero VAD, model embedded), `hf-hub` (model downloads), `tokio`; Windows-only: `wasapi`, `winreg`.
⚠ VERIFY at implementation time: `parakeet-rs` version/features/model API and its `ort` version vs `voice_activity_detector` (if conflicting, run silero via parakeet-rs's ort re-export or swap to `earshot`); `hf-hub` progress API; `wasapi` loopback API.

**Frontend:** Vite + TypeScript + **Preact** (React API, 4 KB, `@preact/preset-vite`). No router — 4 views via a state enum. Hand-written dark-theme `styles.css`.

```
Witness/
  package.json  vite.config.ts  index.html
  src/                      # frontend
    main.tsx  app.tsx
    views/{meetings,transcript,search,settings}.tsx
    lib/api.ts              # typed invoke() wrappers
    lib/events.ts           # typed listen() wrappers
    styles.css
  src-tauri/src/            # flat, module-per-file (vigil style)
```

## Rust modules (`src-tauri/src/`)

| File | Responsibility |
|---|---|
| `main.rs` | Tauri builder, plugins, tray, spawn watcher, close-to-tray, `AppState` (Mutex&lt;Settings&gt;, Db, RecorderHandle, pipeline JobSender, watcher status) |
| `settings.rs` | `WITNESS_SETTINGS` env → `witness-settings.toml` → data_dir (copy vigil's pattern). Fields: `data_dir`, `engine`, `auto_record`, `watch_patterns` (default `["MSTeams"]`), `auto_transcribe`, `opus_bitrate_kbps` (40) |
| `db.rs` | rusqlite, WAL, `PRAGMA user_version` migrations, all queries |
| `meeting_watcher.rs` | registry polling thread + debounce |
| `audio_capture.rs` | two WASAPI capture threads (mic + render loopback), per-thread MTA COM init, event-driven, downmix to mono f32, no I/O in capture threads |
| `resample.rs` | rubato wrappers: device-rate→48 kHz streaming; 48→16 kHz for ASR |
| `recorder.rs` | state machine Idle→Recording→Finalizing; owns capture + writer threads; produces two 48 kHz mono WAVs in `data_dir/rec-tmp/` |
| `encoder.rs` | two WAVs → **stereo Ogg Opus** (L=mic, R=loopback, ~40 kbps VBR ≈18 MB/h) → `data_dir/audio/{id}.opus`; delete WAVs on success |
| `vad.rs` | Silero VAD → speech regions → chunks ≤ 240 s (cut at longest silence) |
| `asr.rs` | `trait AsrEngine { fn transcribe(&mut self, samples_16k: &[f32], offset_ms: u64) -> Result<Vec<AsrSegment>> }` + factory |
| `asr_parakeet.rs` / `asr_whisper.rs` | engine impls (CUDA EP with CPU fallback + visible warning) |
| `diarize.rs` | Sortformer on loopback track → speaker segments (S1..S4) |
| `pipeline.rs` | single worker thread, FIFO: load WAVs → 16 kHz → VAD → ASR both tracks → diarize loopback → merge → DB insert → Opus encode → cleanup; progress events; engines loaded per-job, dropped after (frees VRAM) |
| `models.rs` | model registry, presence checks, hf-hub downloads with progress, `*.part` + rename |
| `commands.rs` / `events.rs` / `tray.rs` | see below |

## SQLite schema (external-content FTS5)

```sql
CREATE TABLE meetings (id INTEGER PRIMARY KEY, started_at TEXT NOT NULL, ended_at TEXT,
  title TEXT NOT NULL, audio_path TEXT, duration_ms INTEGER,
  status TEXT NOT NULL DEFAULT 'recording',  -- recording|recorded|processing|transcribed|failed
  engine TEXT, trigger TEXT NOT NULL DEFAULT 'auto');
CREATE TABLE speakers (id INTEGER PRIMARY KEY, meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  label TEXT NOT NULL,          -- 'me' | 'S1'..'S4' (immutable)
  display_name TEXT NOT NULL,   -- user-renameable
  UNIQUE(meeting_id, label));
CREATE TABLE segments (id INTEGER PRIMARY KEY, meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
  speaker_id INTEGER REFERENCES speakers(id), track TEXT NOT NULL,  -- 'mic'|'loopback'
  start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL, text TEXT NOT NULL);
CREATE INDEX idx_segments_meeting ON segments(meeting_id, start_ms);
CREATE VIRTUAL TABLE segments_fts USING fts5(text, content='segments', content_rowid='id',
  tokenize='porter unicode61 remove_diacritics 2');
-- + standard external-content sync triggers (AFTER INSERT/DELETE/UPDATE)
```

Search: `snippet(segments_fts,0,'<mark>','</mark>',' … ',12)` + `ORDER BY bm25(segments_fts)`, JOIN back to segments/meetings/speakers. Sanitize input: quote each token, `*` on last for type-ahead; pass raw through only if it parses. In-transcript highlight via `highlight()`.

## Recording pipeline

**Watcher** (2 s poll): scan `microphone` + `microphone\NonPackaged` subkeys; active ⇔ name contains any watch pattern (case-insensitive) ∧ Start≠0 ∧ Stop=0. Debounce: 2 consecutive active ticks (~4 s) → start (filters Teams' settings-page mic test); 5 inactive ticks (~10 s) → stop (survives drop/rejoin). Manual stop during a call suppresses auto-restart until the call ends. Auto-start fires a toast: "Witness is recording this meeting."

**Capture:** per-thread `initialize_mta()`; mic thread (default capture device) + loopback thread (default render device, loopback flag), shared mode, event-driven, device mix format; each packet → mono f32 + QPC timestamp → crossbeam channel. **Writer thread:** streaming-resample to 48 kHz i16, append via hound; record per-track start QPC and pre-pad later track with silence; flush ~5 s + `{id}.meta.toml` sidecar for crash recovery; emit `recording-level` RMS every 500 ms. Device invalidated (BT headset switch) → reopen current default, insert silence for the gap.

**Post-meeting** (per job, progress events per stage): load WAVs → 16 kHz f32 → VAD chunks (skip silence — loopback is silent while user talks, big throughput win) → ASR each chunk (`offset_ms` added to engine timestamps) → Sortformer on loopback → create speakers (`me`/"Me", `S1`/"Speaker 1"…) → assign loopback segments by max time-overlap with diarization → merge both tracks sorted by `start_ms` → single-transaction DB insert (triggers fill FTS) → Opus encode → delete WAVs → toast. Failures → `status='failed'`, WAVs kept, `retranscribe` retries. Startup: orphaned `rec-tmp` WAVs re-enqueued.

## Tauri commands & events

Commands: `get_status`, `start_recording`, `stop_recording`, `list_meetings(offset,limit)`, `get_meeting(id)`, `delete_meeting(id)`, `rename_meeting(id,title)`, `rename_speaker(speaker_id,name)`, `search(query,offset)`, `retranscribe(meeting_id, engine?)`, `get_settings`/`update_settings`, `get_model_status`, `download_models(engine)`, `get_audio_url(meeting_id)`.

Playback: enable asset protocol, at startup `allow_directory(data_dir/audio)` (runtime scope — ⚠ VERIFY exact Tauri 2 API); frontend `<audio src={convertFileSrc(...)}>`; range requests make `currentTime` seeking work in WebView2 (plays Ogg Opus natively).

Events: `recording-started/stopped`, `recording-level`, `watcher-status`, `transcription-progress {stage: vad|asr|diarize|encode, pct}`, `transcription-complete/failed`, `model-download-progress`, `meetings-changed`.

## UI (4 views + persistent top bar)

Top bar: record/stop, status pill (Idle / ● Recording 12:34 + level meter / Transcribing 43%), search box.
1. **Meetings** — reverse-chron list (title inline-rename, date, duration, status badge), click → Transcript, delete w/ confirm.
2. **Transcript** — sticky audio player; segments: colored speaker chip (click → rename dialog), `[mm:ss]`, text; click segment → seek+play; follow-along highlight on `timeupdate` (binary search); "Retranscribe with…" menu.
3. **Search** — global FTS, grouped by meeting, `<mark>` snippets, click → Transcript scrolled to segment.
4. **Settings** — data dir, engine radio, auto-record/auto-transcribe toggles, watch patterns, model manager (status/download/progress), GPU status, live watcher status ("Teams key found: yes/no").

Tray: Open / Record now ⇄ Stop / status / Quit (stops recording cleanly, finalizes, exits; pending transcription resumes next launch).

## Models (`data_dir/models/`, downloaded on demand; app is record-only capable with zero models)

| Model | Source | ~Size |
|---|---|---|
| Parakeet TDT 0.6B v3 ONNX | repo per parakeet-rs README (e.g. `istupakov/parakeet-tdt-0.6b-v3-onnx`) — ⚠ VERIFY | 0.7–2.4 GB |
| Sortformer diar 4spk ONNX | per parakeet-rs README — ⚠ VERIFY | ~0.5 GB |
| Whisper large-v3-turbo | `ggerganov/whisper.cpp` `ggml-large-v3-turbo(-q5_0).bin` | 0.6–1.6 GB |
| Silero VAD | embedded in crate | — |

## Risks

1. **ORT CUDA DLLs** (top risk): use ort `download-binaries`+`cuda`; on EP init failure fall back to CPU with visible warning; document cuDNN 9 in README. **De-risk first in Phase 4** with a throwaway CUDA-transcribe test.
2. `whisper-rs` CUDA needs CUDA toolkit at build → gate behind `whisper-cuda` feature, default CPU.
3. `ort` version conflict parakeet-rs vs VAD crate → check lockfile early; fallbacks listed above.
4. WASAPI COM: MTA per capture thread, objects thread-local, never call wasapi from command handlers.
5. Device loopback captures ALL system audio (dings, music) — accepted; note in README.
6. Clock drift mic vs loopback → compare sample count vs QPC; correct if >250 ms.
7. Sortformer 4-speaker cap — accepted; UI says "up to 4 remote speakers".
8. AV heuristics: single visible-tray process, no daemon, no self-relaunch (vigil's lesson).

## Phases (each verifiable)

1. **Skeleton + manual record** — scaffold, settings, db (meetings only), tray, capture→WAVs, record button, level meter. *Verify: record YouTube+talking 2 min; two WAVs play in VLC (mic vs system); clean quit-while-recording.*
2. **Meeting watcher** — polling, debounce, auto start/stop, toast. *Verify: real Teams call auto-records in ~5 s, stops ~10 s after leave; mic-test doesn't trigger; manual stop stays stopped.*
3. **Opus + playback** — encoder, asset protocol, minimal player, WAV cleanup + recovery. *Verify: 1 h ≈ 15–25 MB; instant seeking; WAVs deleted.*
4. **Transcription (Parakeet)** — models UI, VAD, ASR, diarize, pipeline, DB insert. **Start with throwaway CUDA smoke test.** *Verify: 2-person Teams meeting → "Me" vs "Speaker 1" correct; click segment → hear those words; GPU active; 1 h transcribes in minutes.*
5. **Search + full UI** — search view, transcript polish, renames, deep-links, settings. *Verify: search word → marked snippet → correct segment plays; renames persist.*
6. **Whisper engine + hardening** — second engine, retranscribe, device-change/drift, failure/retry UX, release build, README, icon. *Verify: retranscribe comparison; headset-yank survival; clean-machine release run incl. model download.*

## Reference files
- `G:\Scripts\vigil\src\settings.rs` — settings layering to copy (s/VIGIL/WITNESS/)
- `G:\Scripts\vigil\CLAUDE.md` — single-process/tray rationale, AV lesson
- `G:\Scripts\toktrack\Cargo.toml` — release-profile & Rust conventions reference
