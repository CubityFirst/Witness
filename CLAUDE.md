# Witness

Local-only meeting recorder & transcriber for Windows 11 (pure Rust: native
egui/eframe UI over a UI-agnostic backend library). Detects Teams calls via
the mic consent store, records mic + system loopback as separate tracks,
live-captions while recording, transcribes on-device (Parakeet TDT on CUDA /
Whisper on CPU) with diarization and cross-meeting voice identification, and
keeps a full-text-searchable archive. Meeting content stays on the machine;
only explicit model/GPU-library downloads, the (optional) update check and the
opt-in AI assistant (off by default; sends transcript text, speaker names and
the title — never audio — to the OpenAI-compatible service the user
configured) use the network. It follows the same single-process, tray-first
philosophy as the sibling Vigil project.

PLAN.md is the original (implemented, Tauri-era) design; this file is the
living map. 0.3.0 replaced Tauri/WebView2/Preact/Node with the native UI; the
backend, database, archive, settings file, installer identity and update feed
carried over unchanged.

## Architecture

Cargo workspace at the repo root: `crates/witness-core` (lib `witness_core`:
all backend + the `service` layer + the audio player), `crates/witness` (bin
`witness.exe`: UI + Windows shell), `xtask` (version/check/dist/sign).

One visible process, tray-first, **no detached daemon, no self-relaunch**
(AV heuristics — vigil's lesson); the only relaunches are the user-initiated
Restart (after a data_dir change) and update install. Closing the window
hides to tray; tray Quit finalizes any recording and exits. Interrupted work
is recovered at startup from db statuses + `rec-tmp/` sidecars.

```
witness.exe main (winit) thread: eframe WitnessApp (app.rs) + tray + hotkeys
  + single-instance window. logic() runs on EVERY repaint (hidden/minimized
  too) and drains one crossbeam channel of UiMsg:
     Core(CoreEvent) ◄─ UiSink (EventSink impl; wakes the root viewport)
     Shell(ShellEvent) ◄─ tray / hotkeys / second instance
     Op(OpDone) ◄─ Tasks::spawn_op workers
  ui() draws top bar + active view; views call AppCore via Tasks (workers)
                                                             │
meeting_watcher.rs ──(registry poll 2s, debounce 2/5 ticks)──┤
tray / hotkeys / top bar ──► service::rec_control (one ordered worker)
                                                             ▼
         witness_core::AppCore (Arc) — service/recording.rs start/stop
                                                             │
        ┌────────────────────────────────────────────────────┤
        ▼                                                    ▼
audio_capture.rs (2 threads: mic + loopback,        live_transcribe.rs
  MTA COM, event-driven, f32 autoconvert)             (VAD-gated Parakeet on
        │ CaptureMsg (samples + QPC)                   speech pauses → live
        ▼                                              caption events + the
recorder.rs writer thread                              captions overlay)
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

Core seams (`service/mod.rs`): `AppCore` has one method per former Tauri
command; events go out as `CoreEvent` through `trait EventSink`, OS toasts
through `trait Notifier` (`notify::WinToastNotifier`). Nothing in
witness-core depends on egui or windowing. `service/testing.rs` has
`CollectSink`/`NullNotifier` for unit tests. Startup work (logging, WAV
repair, orphan recovery, bin purge) is `service/startup.rs`.

UI (`crates/witness`): eframe 0.36 (glow renderer), no router — a `View` enum
in `ui/mod.rs` (`ActiveView` owns the live view). Views in `ui/views/`:
Meetings (+ recycle bin), People (stats + weekly trend + name filter), Transcript (+
`player_bar.rs`: player, live captions, bookmarks, notes; `ai_summary.rs`: the
AI Summary panel), Search (filters),
Settings (`views/settings/`: `mod.rs` view + routing, `nav.rs` rail / index,
`search.rs` settings search index, `kit.rs` shared row helpers — also used by
`ui/onboarding.rs` —, one file per category: `general`, `recording`,
`transcription`, `ai`, `voices`, `storage`, `about` + `updates`), plus `ui/palette.rs` (Ctrl+K command palette)
and `ui/onboarding.rs` (first-run setup). The look follows **docs/UI.md**
(flat rows, grouped surfaces, elide-don't-overflow, nothing wraps or clips
at ≥ 380 pt, light + dark themes, the split layout from 900 pt). Colours
are a runtime `theme::Palette` read with `pal()` at paint time — never put
a colour in a const/static. Shared kit (use it, don't fork it): `ui/theme.rs`
(tokens: palette, spacing `S1..S5`, radii, `CONTENT_MAX`, `COMPACT_BELOW`,
speaker colours; fonts; Visuals with floating scroll bars), `icons.rs`
(egui-phosphor regular), `widgets.rs` (buttons, pills, `list_row`, `group`,
`switch`, `elide`, `avatar`, `scroll_column`/`padded_column`,
`keyboard_scroll`, `announce` = AccessKit live region), `fonts.rs` (Segoe UI
eager + lazy script fallbacks via `note_texts`), `format.rs`, `confirm.rs`
(in-app confirm modal), `toasts.rs` (in-app notices). Global UI state is
`model.rs` `ShellModel` (`apply(CoreEvent) -> Vec<Effect>`, update flow);
settings edits go through `settings_store.rs` (optimistic copy + ordered
write queue that survives leaving Settings); UI prefs persist in
`ui_state.rs`. The window is undecorated: `ui/topbar.rs` is the drag region +
custom min/max/close (close hides to tray) + resize edges + model/health
banners. The always-on-top live-caption overlay is a **deferred viewport**
(`ui/captions.rs`, id `witness-captions`). Windows shell code lives in
`platform/` (window/flyout, tray, hotkeys, single_instance, autostart,
dialogs, system metrics). Playback is `witness_core::player`: a seekable Ogg
Opus reader (raw stereo exactly as stored), a WASAPI shared-mode render
thread and an in-house WSOLA time-stretch (speed) — no extra crates.

## Data model (SQLite, WAL, `PRAGMA user_version` = 6)

- `meetings(id, started_at, ended_at, title, audio_path, duration_ms,
  status, engine, trigger, notes, deleted_at, title_source, ai_summary,
  ai_summary_at, ai_summary_stale, transcript_rev)` — status:
  recording→recorded→processing→transcribed|failed; `deleted_at` = recycle
  bin (purged after 30 days at startup); `notes` has its own FTS table.
  `title_source` (`db::TitleSource`): `auto` (the "Meeting <timestamp>" a
  recording starts with) | `teams` | `ai` | `user` (default; every rename).
  `ai_summary` = the AI assistant's Markdown notes ('' = none, not in FTS),
  `ai_summary_stale` = written for an earlier transcript (set by
  `replace_transcript`, cleared when notes are written),
  `transcript_rev` = bumped by every `replace_transcript`; notes are stored
  only if it still matches the revision they were written from. Both AI
  writes also match `started_at` (ids are reused after a purge). v5→v6 is
  additive: five `ADD COLUMN`s, and only exact `Meeting YYYY-MM-DD HH:MM`
  titles are backfilled to `auto` (a v5 db can't tell a Teams name from a
  rename, so those stay protected as `user`). Stock 0.2.x refuses a v6 db
  and then panics at launch: rollbacks must carry
  `docs/rollback/v0.2.1-open-schema-6.patch` (docs/RELEASE_CHECKLIST.md).
- `pipeline_jobs(meeting_id, transcribe, engine, queued_at, attempts,
  last_error, revision)` — the durable processing queue.
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
  untagged files decode as hard-panned. The player plays the stored stereo
  as-is (no unmix).
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
  Parakeet pipeline job / live session, and by `AppCore::gpu_status`
  (Settings). gpu_libs.rs backs the Settings "Download GPU libraries"
  button (~1 GB): pinned NVIDIA PyPI wheels (immutable
  files.pythonhosted.org URLs + SHA-256), Range-resumable downloads, DLL
  extraction by basename into `data_dir/cuda/`, an atomic install manifest
  with quick fingerprints, and Repair — the verified dir is preflight's
  first candidate, so GPU kicks in without a restart.
- **Voice identification invariants**: auto-matches never mutate stored
  prints; only explicit renames enroll/update people. Renaming always
  creates/links a person (even print-less) so People stats unify.
  `speaker_id::rematch_all` retro-labels all stored per-speaker embeddings
  after any enrollment change — pure cosine over the DB, no audio.
  Retranscription carries user-set names forward by label.
- **Recycle bin over hard delete**: delete = `deleted_at` timestamp
  (excluded from list/search/stats/recovery), restore/purge in the bin UI,
  30-day auto-purge. Shift-click skips confirmations (in-app modal,
  `ui/confirm.rs`).
- **The UI thread never blocks.** `main()` calls `mark_ui_thread()`; every
  blocking (⏳) `AppCore` method starts with `assert_off_ui_thread` (a debug
  assert); only the `[ui-ok]` methods may run inside `logic()`/`ui()`.
  Loads use `Tasks::spawn` → a view-owned `Task<T>` (dropped with the view,
  so stale results vanish). Actions whose outcome must survive navigation or
  a hidden window (saves, renames, deletes, backups, updates) use
  `Tasks::spawn_op` → `OpDone` handled globally in `logic()`. Recording
  control from tray, hotkeys and top bar goes through `submit_rec` → the
  core's single ordered `rec_control` worker (shutdown queues behind any
  pending toggle). Notes go through `core.queue_notes` → the ordered
  latest-wins `notes-writer` thread; `flush_notes` runs on every exit path,
  and `will_hide()` (every way the window stops being visible) enqueues the
  current draft. No backend side effect depends on the UI loop: toasts and
  recording start/stop come from core threads.
- **Exit paths are two-phase** (`app.rs`): reversible `prepare_exit()`
  (flush notes ≤2 s, save captions geometry, unload player), then — only
  after the irreversible step succeeded (restart spawn / installer launch)
  — `teardown_and_exit()` (tray NIM_DELETE, unregister hotkeys, release the
  instance mutex, flush log, `process::exit`). If a spawn/launch fails, the
  app keeps running intact. Restart/update hold `core.exit_guard(..)`
  (recorder + backup locks) so nothing starts between the check and exit.
- **Panic policy** (`crates/witness/src/panic.rs`): UI code now shares the
  event-loop thread, so a main-thread panic is fatal. The hook logs (with
  backtrace) for every thread; worker panics are contained by `catch_unwind`
  in `Tasks`. On the main thread, once: `emergency_stop` (stop + save the
  recording, 10 s cap, never locks on the panicking thread), remove the tray
  icon, release the instance mutex, flush the log, MessageBox, exit 101.
- **Tray flyout** (`platform/window.rs` `FlyoutState`): left-click opens
  the main window anchored above the tray (sized at the target monitor's
  DPI, clamped to its work area), chrome-less, dismissed on blur. Focus
  loss with the cursor still on the window means a border grab → the
  flyout "pins" into a normal window instead of dismissing (resize grabs
  blur before any resize event fires); our own drag/resize and opening a
  native dialog pin immediately.
- **Meeting auto-naming reads Teams window titles only** (12/30/60 s after
  auto-start; never overwrites a user rename). No process memory, no UIA
  scraping, no Graph API. The watcher snapshots Teams window handles while
  the mic is idle; naming prefers windows that appeared since (the call
  window) over the long-lived main window, whose title is just the open
  chat/tab. Falls back to all windows if no new ones survive filtering.
- **"Never overwrite a user rename" is `title_source`.** `rename_meeting`
  (every user rename) sets `user`; automation writes only through
  `Db::replace_title_if` — a compare-and-swap on (started_at, title,
  title_source) as read before the slow part — so a rename made while Teams naming or an AI
  request runs always wins. Teams naming needs `auto`; automatic AI titling
  needs `auto`/`teams`; a manual "Generate title" may replace anything (the
  user asked), still via the CAS.
- **The AI assistant is opt-in and lives on its own thread**
  (`service/ai.rs` "ai-worker", `ai_client.rs`, `credentials.rs`). Master
  switch `settings.ai.enabled` (default off) gates *every* request: a job's
  gate re-reads the switch, base URL, model, the key epoch (bumped by
  `set_ai_key` and by `update_settings` → `ai.settings_changed`), the
  automatic feature switch and `is_same_live_meeting` before EVERY request
  and every DB write, and stops with `Skipped`; switching off also drops
  waiting jobs, and bin/purge call `ai.cancel_meeting` (drops its jobs and
  clears the UI's task state via `AiDone`). Settings' test/model-list are
  refused while off. The pipeline adapter calls `ai.on_transcribed` after a
  finished transcription; manual requests come from `AppCore::ai_generate`
  ([ui-ok]: a queue push). Requests merge only into *waiting* jobs — a
  request for a task that is running queues again (a retranscription or a
  Regenerate must not be swallowed). Jobs run once (no retries), have a
  request budget (parts + 8) and cap replies (part 8 KB, notes 32 KB, title
  512 B), report `CoreEvent::AiProgress`/`AiDone` (+ `MeetingsChanged` on
  success), and automatic failures OS-toast at most every 15 min. Sent: transcript
  as `[mm:ss] Speaker: text` turns, speaker names, the title unless it is
  the timestamp — nothing else. Over 32 000 chars the transcript is read in
  parts (map → reduce; the title then comes from the notes). The client
  speaks OpenAI Chat Completions through an `AiTransport` seam (tests use
  fakes; `UreqTransport` reuses the updater's `agent_builder`: SChannel +
  system proxy, loopback direct), redirects off, https-only except
  loopback (`ai_client::BaseUrl`), `User-Agent: Witness`. The key is an
  `ApiKey` (redacted `Debug`; our copies wiped best-effort with
  `ai_client::wipe`) in Windows Credential Manager (`Witness/ai-api-key`,
  user name = the host it is bound to; `key_for` attaches it only to that
  host — a local server gets none, another remote host is refused;
  `CredentialStore` seam; unit tests — including `AppCore::start` under
  `cfg(test)` — use `MemoryCredentials`); server messages are scrubbed of
  key-like text. AI titles drop control/bidi characters; Markdown exports
  run the summary through `transcript_export::safe_markdown` (no images,
  HTML or live links). Retranscription keeps the summary and flags it
  stale (automatic notes then rewrite it); purge deletes it with the row;
  exports (md/txt) include it.
- **Meeting end stops any running recording**, not just auto-started ones —
  StopMeeting only fires after a watched meeting was detected and ended, so
  manual recordings made outside a call are never touched.
- **Live captions are provisional by design** — VAD-cut chunks on ~0.7 s
  pauses (15 s cap), tagged Me/Them only; the final pass replaces them
  with the diarized transcript. Timeline stays aligned with the WAVs
  because writer silence insertions are forwarded as `LiveMsg::Silence`.
  The in-app feed (`ShellModel::live_lines`) keeps the latest 500 lines of
  the current recording across view changes; the overlay keeps the last 20.
- **Phantom-line defense is two-layered** (mic pops / AC hum used to become
  transcript lines): vad.rs demands ≥5 consecutive speech frames (~80 ms,
  `MIN_SPEECH_FRAMES`) at a 0.5 threshold (earshot 1.2; 1.1 needed 0.65)
  before a span counts as speech —
  transients light up 1–2 frames; and `asr::is_junk_text` drops known
  noise hallucinations (lone "You", "Thank you.", YouTube-outro phrases)
  plus empty/punctuation-only output, in both the pipeline and live
  captions. Both constants are shared by live_transcribe.rs, which also
  refuses to cut a chunk until a run confirms real speech. Retranscribing
  an old meeting re-runs VAD, so it cleans up past phantom lines too.
- Search snippets use `\x01`/`\x02` markers (control chars can't occur in
  transcripts), turned into highlighted text runs by `ui/format.rs` — never
  parsed as markup.
- Exports (md/txt/srt/vtt) render backend-side; "save a copy" actions go
  through native save dialogs (rfd, parented to the main window → modal).
  Transcript export renders first, then asks for the path; audio export
  checks the archive exists before any dialog.
- **Transcript list is virtualised** (height cache keyed by row + width), so
  drag-selection only spans laid-out rows — Export → "Copy to clipboard" is
  the workaround (accepted).

## Crate/version constraints (do not "upgrade" casually)

- **egui/eframe `=0.36.2`** move in lockstep and break their API every
  minor release; bump the pins together, deliberately. **egui-phosphor
  `=0.14.0`** (every glyph in `ui/icons.rs` verified present). **winit
  0.30.13** must be eframe's own copy (`WindowExtWindows`, dpi types).
  **wgpu 30** is only the fallback renderer, built with no default
  backends (DX12 only) via the `wgpu-fallback` feature
  (`eframe/wgpu_no_default_features` + `dep:wgpu`) — never enable
  `eframe/wgpu`, which turns on every wgpu backend. Dropping the fallback
  (`default = []`) costs a window on GL-1.1-only machines (RDP, GPU-less
  VMs).
- **minisign `=0.7.3`** (xtask signer; what tauri-cli used — 0.7.4–0.8.0 were
  broken, 0.9+ pulls getrandom 0.4) and **minisign-verify `=0.2.5`** (exactly
  the verifier installed 0.2.x clients run; the native updater and the
  xtask self-check use it too). A drift is invisible to CI, which would sign
  and verify with the bumped pair.
- tray-icon 0.26 (re-exports muda), global-hotkey 0.8 (same combo parser
  as tauri-plugin-global-shortcut, so bound-combo strings are unchanged),
  rfd 0.17, tauri-winrt-notification 0.8.1 (the standalone toast crate the
  Tauri plugin wrapped — its `Toast::new` defaults to a sound; we pass
  `.sound(None)`, since 0.2.x toasts were silent and a chime can leak into
  the loopback track). The updater uses ureq 2 (hf-hub's major) with
  native-tls (SChannel → Windows certificate store); the AI client shares its
  agent setup (`updater::agent_builder`). The API-key store uses the
  `windows` crate's `Win32_Security_Credentials` feature — no new crates.
- `.github/dependabot.yml` ignores mirror this list.
- **VAD is earshot, not Silero** — `voice_activity_detector` pins ort
  =2.0.0-rc.10; parakeet-rs needs rc.13. earshot frames: exactly 256
  samples @ 16 kHz. Its minor releases retrain the model, so a bump means
  re-tuning `vad::THRESHOLD` against real recordings: compare speech kept,
  transcript lines a chunking no longer covers, and chunks that transcribe
  to nothing (1.1 → 1.2 moved it from 0.65 to 0.5).
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
- **wasapi 0.26** (≥0.25 fixes RUSTSEC-2026-0332): loopback = *render* device opened with
  `Direction::Capture` + `StreamMode::EventsShared`; `initialize_mta()`
  per thread; wasapi objects are `!Send`. The player's render thread uses
  the same crate.
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
- **parakeet-rs is a git pin** (rev `4fa6bdae`; 0.3.8 isn't on crates.io
  yet) — swap to the crates.io release when it lands. 0.3.8 dropped
  Sortformer v2 support. **TDT ~4–5 min inference cap** → vad.rs
  hard-splits at 240 s.
- **FTS5 probe queries need `LIMIT 1`** — with `LIMIT 0` the MATCH
  expression is never parsed and invalid syntax slips through.
- Whisper CUDA is behind witness-core's `whisper-cuda` cargo feature (needs
  the CUDA toolkit at build time; `--features witness-core/whisper-cuda`);
  default build = CPU Whisper + CUDA Parakeet.
- Lockfile discipline: after dependency moves run `cargo check --workspace`
  (not `cargo update`) and diff `Cargo.lock` for the pinned ML crates.

## Platform quirks encoded here

- **A hidden root still runs full `ui()` passes; only a minimized root skips
  them** (eframe 0.36.2 + winit 0.30.13: Windows never emits `Occluded`, so
  a hidden window counts as visible). Hence: drain the channel in
  `logic()` (runs on every repaint in every state), and gate every
  animation/periodic repaint (pulse, spinners, player 30 Hz) on
  `is_shown() && !is_minimized()` — a hidden window pays a full layout per
  repaint. A deferred viewport can't be *created* while the root is
  minimized, so `MainWindow::ensure_ui_pass()` restores the root
  DWM-cloaked without activation for the pass that declares the captions
  overlay, then `finish_ui_pass()` re-minimizes and uncloaks (2 s cap).
- **`--minimized` start (autostart) never flashes**: eframe force-shows the
  root after its first frame, so `StartupVisibility` DWM-cloaks it in the
  creator, hides it after that first frame, then uncloaks.
- **Renderer is explicitly `Renderer::Glow`** (eframe picks wgpu when both
  are compiled). If glow fails *before the app creator ran*, retry once on
  wgpu (DX12, LowPower); never after, because the tray, hotkeys and
  single-instance attach are once-per-process. `WITNESS_RENDERER=glow|wgpu`
  overrides (wgpu only with the feature).
- **`with_drag_and_drop(false)` on every viewport, and no COM init on the
  main thread before winit**: winit's `OleInitialize` (STA) must not fight
  the core's MTA COM. rfd dialogs run on their own short-lived thread
  (inside a `Tasks` worker), because rfd silently reports "cancelled" on a
  thread that already joined the MTA (any WASAPI call does).
- **Tray: no `with_guid`, created once per process** — Explorer keys the
  "always show" promotion on exe path + uID, and the first tray-icon is uID
  2, as in 0.2.x. **No process AUMID** (`SetCurrentProcessExplicitAppUserModelID`
  is never called; 0.2.x didn't, and it would split taskbar pins).
  Its glyph (idle / recording / transcribing) is the colour app tile or, by
  default (Settings → General → Tray icon), monochrome inked for the
  taskbar's theme, which follows `SystemUsesLightTheme`, not the app mode. A thread
  parked in `RegNotifyChangeKeyValue` reports flips as
  `ShellEvent::TaskbarTheme`, because `logic()` does not run while hidden.
- Window operations go through `platform::window::MainWindow` (winit/Win32
  directly: synchronous, physical pixels, independent of the pass state),
  not `ViewportCommand` (`OuterPosition` uses the wrong DPI on mixed-DPI
  setups). Cross-thread repaint requests must target the root viewport
  (`bridge::wake_root`): a plain `request_repaint()` while the overlay is
  mid-pass wakes only the overlay.
- egui's zoom factor is shared by every viewport, so keyboard zoom is off
  and `app.rs` zooms the root only (Ctrl +/−/0, Ctrl+wheel, times the
  Windows text-size setting); the overlay has its own saved scale.
- egui turns Space/Enter on a focused button into a click, so player keys
  (Space, ←/→) and `keyboard_scroll` act only when nothing has focus.
- Single instance keeps 0.2.x's names (mutex `…witness-sim`, window class
  `…witness-sic`, title `…witness-siw`, `WM_COPYDATA` dwData 1542), so a
  lingering Tauri process and the native app recognise each other. The
  payload is only logged; a second launch just shows the window.
- Global hotkeys walk candidate chains (record: ctrl+alt+r→w→shift+r;
  bookmark: ctrl+alt+b→m) because other apps can own combos system-wide;
  bound combos go to `core.set_bound_hotkeys` and are exposed through
  status.
- Toasts: the installed build (exe dir == `HKCU\Software\github\Witness`
  default; cargo profile dirs never count) uses the AUMID
  `com.github.cubityfirst.witness` that NSIS writes on the shortcut and
  shows "Witness"; dev builds borrow PowerShell's AUMID and are attributed
  to it.
- Root viewport `clear_color` = `BG` (no white flash on resize);
  `persist_window: false`.
- **No large stack arrays.** The main thread runs the whole UI and has
  Windows' 1 MB stack, and release inlining hoists a callee's array into
  the caller's frame even when that branch never runs: a 1 MB hash buffer in
  `digest_reader` made model-status loading overflow at launch once models
  were installed (0.2.0). Buffers go on the heap (`vec!`).
- witness.log rotates at 5 MB (one `.old`). Crash recovery patches WAV
  RIFF/data sizes from file length (`repair_wav_header`) before reading.
- The installer ships the ort CUDA provider DLLs because they are listed in
  `installer/witness.nsi` (`xtask check` asserts exactly those two).
- **Accepted UI regressions** (0.3.0): no high-contrast palette (detected
  and logged only — the palette is `const`s), no bidi reordering for RTL
  scripts (egui shapes but doesn't reorder), no notes spellcheck, fixed
  day-month-year dates.

## Building & testing

Install CMake and libclang through Visual Studio Build Tools/LLVM, put CMake
on `PATH`, and set `LIBCLANG_PATH` when needed (a machine-local,
gitignored `.cargo/config.toml` is fine). Do not commit machine-local tool
paths. LLVM stays on **19.x** because bindgen 0.71 (whisper-rs-sys) is not
validated with newer LLVM releases. The toolchain is pinned in
`rust-toolchain.toml`. Everything runs from the repo root:

- `cargo test --workspace` — fast unit suite. Gate for every commit:
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` (CI adds `--locked`, `cargo audit`
  and `cargo run -p xtask -- check`).
- `cargo run -p witness` for development. Use `CARGO_TARGET_DIR=target-test`
  for tests while a dev `witness.exe` is running (it locks the ort DLLs the
  build script copies).
- `cargo run -p xtask -- <cmd>` (no cargo alias — `.cargo/config.toml` is
  machine-local): `version X.Y.Z`, `check [--tag]`, `dist [--sign]`, `sign`,
  `package` (dev-only re-wrap of prebuilt binaries), `gen-test-key`. `dist`
  resolves the target dir from `cargo metadata` (never assumes `target\`)
  and fetches the hash-pinned NSIS toolchain into
  `%LOCALAPPDATA%\witness-build` (`WITNESS_BUILD_CACHE` overrides).
- **Dev data dirs from the Tauri era live in `src-tauri/target*`** (real
  databases, audio, 4 GB of models); never delete them — point
  `data_dir`/`WITNESS_SETTINGS` at them (the untracked
  `target\{debug,release}\witness-settings.toml` files do). Test launches use
  their own `WITNESS_SETTINGS` + empty `data_dir`.
- When ort-sys has to *copy* (not symlink) its DLLs it skips `deps/`, so
  test binaries can't see `onnxruntime_providers_cuda.dll` and `cuda_smoke`
  silently runs on CPU — copy the two provider DLLs into
  `<target>/<profile>/deps/` for a real GPU check.
- Ignored, real-world tests (models resolve to `<target>/debug/models`, or
  `WITNESS_MODELS_DIR`): `capture_smoke` (records 4 s from the real
  devices; WITNESS_TEST_MIC/LOOPBACK env override), `cuda_smoke`
  (downloads Parakeet+diarization, CUDA inference),
  `tts_dialogue_diarization` / `diarize_tail_frame_multiple` (diarization),
  `tts_transcribe` / `whisper_tts` / `vad_keeps_tts_speech` (SAPI speech
  end-to-end, both engines), `voice_print_discrimination` (two TTS voices,
  same/cross cosine), `live_captions` (streams TTS through the live
  worker), `list_devices_smoke`, `playback_smoke` (plays through the
  default device), `gpu_libs_real_download`, `live_feed_check` /
  `real_021_installer_verifies` (updater against the real feed),
  `real_registry_round_trip` (the user's real `Run` key, restored after),
  `dialogs_are_modal_over_the_owner_from_a_worker` (shows real dialogs),
  `real_credential_manager_round_trip` (a throwaway entry in the user's
  real Credential Manager, deleted after), `live_chat_round_trip` (a real
  OpenAI-compatible server: `WITNESS_AI_LIVE_BASE` / `_MODEL` / `_KEY`).
  Never make real AI requests or touch the real `Witness/ai-api-key` entry
  from anything else; unit tests use the transport / credential fakes.
- **GUI verification must be passive — never synthetic mouse/keyboard
  input** (no SendInput/SetCursorPos/SendKeys, posted WM_KEY*/mouse/hotkey
  messages, SetForegroundWindow on test windows, UI Automation Invoke, or
  screen-wide CopyFromScreen): the developer uses the machine concurrently.
  Preferred: the debug-only **off-screen snapshot mode**
  (`WITNESS_DEBUG_SNAPSHOT=<out.bmp>` + `WITNESS_DEBUG_SIZE=<w>x<h>`, see
  docs/UI.md) — cloaked, never activated, no tray/hotkeys, per-process
  instance names, exits by itself; check UI changes at 380/460/640/1000.
  Otherwise launch the test exe, wait, capture *its own* window with
  `PrintWindow(PW_RENDERFULLCONTENT)`, kill it. Reach states through
  debug-build-only env hooks instead of clicks (full list in docs/UI.md):
  `WITNESS_DEBUG_START_VIEW`
  (`meetings|people|settings|search:<q>|transcript:<id>`),
  `WITNESS_DEBUG_TOASTS=1`, `WITNESS_DEBUG_CONFIRM=1`,
  `WITNESS_DEBUG_CAPTIONS=1` (overlay with sample lines),
  `WITNESS_DEBUG_SYNTHETIC=1` (3,000-segment transcript),
  `WITNESS_DEBUG_SETTINGS_PAGE=<category>|index`,
  `WITNESS_DEBUG_SETTINGS_SEARCH=<q>` (+ `WITNESS_DEBUG_SETTINGS_RESULT=<n>`),
  `WITNESS_DEBUG_AI_SETTINGS=on|error|other`, `WITNESS_DEBUG_AI_SUMMARY=sample|<md>`
  + `WITNESS_DEBUG_AI_STATE=busy|failed|stale|off` (AI states, never saved),
  `WITNESS_DEBUG_PANIC_HOTKEY=1` (Ctrl+Alt+Shift+F12 panics in `ui()` — for
  a human). Anything that truly needs interaction goes on a human checklist;
  add a new env hook rather than driving the UI.
- `cargo run -p xtask -- dist` for the (unsigned) NSIS installer.

## Releases & updates

Installed builds update themselves from GitHub Releases through the native
updater (`crates/witness-core/src/service/updater.rs`), which kept
tauri-plugin-updater's contract so 0.2.x installs update into native builds
through their old Tauri updater. The NSIS installer
(`installer/witness.nsi`, hand-maintained from the Tauri 2.11.4 template;
verbatim reference in `installer/reference/`, attribution in
`installer/NOTICE`) is per-user (`currentUser`), so updates never need
elevation. **Never change**: the feed
`releases/latest/download/latest.json` and its schema, the minisign key
(ID `5A06F76D00E6ACD8`), the install identity (`HKCU\Software\github\Witness`,
`%LOCALAPPDATA%\Witness`, uninstall key, `com.github.cubityfirst.witness`
shortcut AUMID), `witness.exe`, and honouring `/P /UPDATE /R /ARGS /S /NS`
(0.2.x runs `/P /UPDATE /R /ARGS <args>` and exits). `xtask check` asserts
these; CI's `upgrade-smoke` installs 0.2.1 and updates over it on every PR.

To release: `cargo run -p xtask -- version X.Y.Z` (root `Cargo.toml`
`[workspace.package]` + the workspace entries in `Cargo.lock`; numeric only,
never `-rc` — `VIProductVersion`), commit, push tag `vX.Y.Z` →
`.github/workflows/release.yml` runs `xtask check --tag`, the tests and
`xtask dist --sign` (installer `.sig` + `latest.json` with both
`windows-x86_64` and `-nsis` entries) and publishes. Signatures carry
`version:X.Y.Z` in the trusted comment; the native updater *requires* it to
match the feed (0.2.x enforces it only when present), so every installer —
even a Tauri-built rollback — is signed by the xtask. The private key lives
outside the repo (`~/.tauri/witness-updater.key` + the
`TAURI_SIGNING_PRIVATE_KEY` repo secret) — losing it means shipping a new
pubkey through a manual reinstall. **Prerelease first** for risky releases
(all of 0.3.x until promoted): tag pushes publish a GitHub prerelease while
the repo variable `RELEASE_PRERELEASE` isn't `false`; `releases/latest`
ignores prereleases; promote with `gh release edit vX.Y.Z
--prerelease=false --latest`. Rollback runbook and the continuity checks are
in `docs/RELEASE_CHECKLIST.md`.

Startup check after 30 s (`check_for_updates` setting) only toasts;
installing is always a click in Settings → About → Updates and is refused while
recording or backing up. The download goes to
`%TEMP%\Witness-{v}-updater-*`, is verified (MZ + signature + version), and
removed on any failure; the app then launches the installer with
`/P /UPDATE /R /ARGS <current args>` and exits through the normal exit path
(notes flushed). Network: SChannel + the WinINet system proxy (else
`HTTPS_PROXY`/`ALL_PROXY`); PAC/WPAD and NTLM proxies unsupported, as in
0.2.x. Persisted pipeline jobs resume on relaunch.

## Settings & files

`WITNESS_SETTINGS` env var → `witness-settings.toml` (next to exe) →
`data_dir`: `witness.db`, `audio/{id}.opus`, `rec-tmp/` (in-flight WAVs +
`.meta.toml` crash sidecars), `models/hf-cache/`, `cuda/` (optional managed
GPU DLLs), `witness.log`, `ui-state.toml` (player volume/speed,
model-banner dismissal; replaced WebView2 localStorage, reset once in
0.3.0), `captions-overlay.toml` (overlay geometry in physical px + scale).
data_dir changes apply to db/log on restart (Settings offers the button).
Debug builds read models from `WITNESS_MODELS_DIR`, else the installed
app's `models/` (`settings::dev_models_dir`; skipped when `WITNESS_SETTINGS`
is set, so test/snapshot launches stay isolated) — "Delete model cache" in
a debug build therefore wipes the installed app's models.
The AI assistant's settings are the `[ai]` table (`enabled`, `base_url`,
`model` — no default —, `auto_title`, `auto_notes`; kept last in `Settings`
because TOML writes tables after values); its API key is never in any file
of ours — Windows Credential Manager, generic credential
`Witness/ai-api-key`. An invalid base URL still saves (other settings must
keep saving); it is refused at request time and shown in Settings.
Settings → Storage → "Delete model cache" wipes `models/` (hf-cache +
install manifest, superseded revisions included); refused while recording,
transcribing or downloading, since Windows can't delete open model files.
The installer removes the old WebView2 profile
(`%LOCALAPPDATA%\com.github.cubityfirst.witness\EBWebView`).
`assets/brand/icons.mjs` is the source of every icon: it draws the shield
and writes `crates/witness/icons/` (exe icon resource 32512, window icon,
`tray-<state>-<light|dark>.ico`), `app-icon.png` and `assets/icon.svg`. The
16–32 px frames are
separately grid-fitted, so edit the script and rerun it
(`node assets/brand/icons.mjs`) rather than resizing a PNG. The README
banners (`witness-banner-{light,dark}.png`) come from
`assets/brand/banner.mjs`, which reuses `assets/icon.svg`.

## Known limitations (accepted)

- No echo cancellation: speakers instead of a headset can bleed remote audio
  into the mic track; a headset is recommended.
- Diarization caps at 8 remote speakers; voice matching on compressed
  Teams audio is good-but-not-perfect (hence the ≈ marker + rename flow).
- DB and audio are unencrypted at rest (local-only threat model).
- AI jobs are not persisted: quitting drops queued or running ones (the
  user generates again); AI summaries are not in full-text search.
- UI regressions listed under Platform quirks (high contrast, RTL, notes
  spellcheck, cross-row selection, fixed date format).
