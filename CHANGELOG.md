# Changelog

All notable changes to Witness are documented here. The project is pre-1.0;
database and backup formats remain versioned, but user-visible behavior can
still evolve quickly.

## 0.3.0 — 2026-10-12

### AI assistant (opt-in)

- New Settings → AI assistant section, off by default: connect any
  OpenAI-compatible service (default base URL `https://api.openai.com/v1`;
  local servers such as Ollama or LM Studio via `http://localhost…`), pick a
  model from the service's list or type its id, and test the connection.
  The settings say exactly what is sent: transcript text, speaker names and
  the meeting title — never audio. Plain `http://` is only allowed for
  localhost.
- The API key is stored in Windows Credential Manager
  (`Witness/ai-api-key`), never in the settings file, database, logs,
  backups or diagnostics. It is bound to the host it was entered for and is
  never sent to another server after the base URL changes.
- Turning the assistant off, removing the key, changing the service or
  model, turning an automatic feature off, or deleting the meeting stops a
  running request before its next step; nothing more is sent or written.
- Transcript → Summary: Generate / Regenerate notes (summary, key points,
  decisions, action items with owners when stated), "Copy to notes" (appends
  to your notes), progress and errors in place. The ✦ menu and the command
  palette (Ctrl+K) also offer "Generate title" and "Generate summary".
- Optional automation after each transcription: "Title new meetings
  automatically" (only titles Witness or Teams chose — a title you typed is
  never replaced, even one typed while the request runs) and "Write meeting
  notes automatically". Long meetings are summarised in parts. Failures are
  reported once, never retried in a loop.
- Markdown and plain-text exports include the summary when there is one
  (Markdown keeps only plain formatting: no images, HTML or live links).
- Database schema 6 (additive): meetings gain `title_source`, `ai_summary`,
  `ai_summary_at`, `ai_summary_stale` and `transcript_rev`. Existing
  timestamp titles are marked automatic; every other existing title is
  treated as yours. Retranscribing keeps a summary but marks it as written
  for the previous transcript.
- **Downgrades and old backups:** Witness 0.2.x cannot open a database this
  version has opened, and 0.3.x backups cannot be restored into 0.2.x.
  Keep a backup made with 0.2.x if you may need to go back; any official
  rollback build is patched to open the new schema.

### Settings, reorganised

- Settings is now seven categories, Windows 11–style: General (theme,
  accent, start with Windows, setup assistant), Recording (auto-record,
  watch patterns, devices, hotkeys), Transcription (engine, GPU and GPU
  libraries, auto-transcribe, live captions, noise phrases), AI assistant,
  Voices (match strictness, voice prints), Storage (data folder, backup,
  models) and About (updates, a privacy summary, diagnostics). Every
  setting, confirmation and save behaves as before.
- Wide windows show a navigation rail beside the selected page; narrow
  ones (the tray flyout) list the categories with a one-line summary of
  their current values and open each with a Back button. Settings reopens
  on the category you used last.
- Search: type in "Find a setting" (or press Ctrl+F or / on Settings) to
  filter every setting by name, description or related words ("shortcut",
  "cudnn", "openai key"…); ↑/↓ and Enter pick a result, which opens its
  category and briefly highlights the setting. Esc clears.
- Links go straight to the right place: "Set up the AI assistant…" opens
  AI assistant, the empty library's "Open Settings" opens Recording, and
  the command palette (Ctrl+K) finds "Settings: <category>".
- The saved API key's Remove button has its own row at the end of the AI
  connection group, like the other destructive actions.

### Native UI

- The user interface is rewritten natively in Rust with egui/eframe. Tauri,
  WebView2 (and its helper processes), Node.js, npm, Vite, Preact and
  TypeScript are gone; Witness is a single `witness.exe` with a far smaller
  dependency tree. The backend, database, archive format, settings file and
  data directory are unchanged, so existing data opens as before.
- A cleaner, modern look: flat meeting rows grouped by day with actions on
  hover, a one-row top bar that compacts instead of wrapping, transcript
  lines grouped by speaker turn with a speaker-share bar, a two-row player,
  People rows with avatars and a one-line summary, Settings as grouped rows
  with toggle switches. Nothing overflows or clips down to the new 380 pt
  minimum window width; long titles and paths are shortened with "…" and a
  tooltip.
- Light and dark themes (Settings → Appearance: follow Windows, light or
  dark) with Windows' accent colour and Segoe UI Variable.
- Wide windows split into the meeting list and the open transcript side
  by side; the divider can be dragged and is remembered.
- Ctrl+K command palette: jump to any meeting or person, or run an action
  (record, bookmark, settings, theme, search transcripts).
- Transcript: who-spoke-when timeline on the scrubber, Ctrl+F find with
  next/previous, and click a speaker in the legend to show only their
  lines.
- Keyboard navigation in lists (↑/↓, j/k, Enter, F2, Delete), loading
  placeholders, and helpful empty states.
- First-run setup assistant (devices with a live level check, models,
  preferences); Settings → Startup → Run setup again.
- Updates continue through the same feed and signing key: 0.2.x installs
  update into this release through their built-in updater, into the same
  per-user install location, keeping shortcuts, the autostart entry and all
  data. The installer now removes the leftover WebView2 profile
  (`%LOCALAPPDATA%\com.github.cubityfirst.witness\EBWebView`).
- The built-in updater is native: it verifies the minisign signature and the
  signed version against the pinned key, uses the Windows certificate store,
  and honours the system (WinINet) and environment proxy settings.
- The player volume, playback speed and the dismissed model banner are now
  stored in `ui-state.toml` in the data directory; they reset once when
  upgrading from 0.2.x.
- New "Copy to clipboard" item in the transcript Export menu.
- Native save and folder dialogs are now modal over the Witness window.
- Startup failures (for example an unreadable settings file) and internal
  errors now show a message instead of exiting silently; an internal error
  saves the recording in progress before closing.
- Known regressions, accepted for this release:
  - high-contrast themes are detected but the app keeps its own dark
    palette;
  - right-to-left scripts (Arabic, Hebrew, Persian) display in logical
    order rather than being reordered;
  - no spellcheck in meeting notes;
  - in long transcripts, a drag selection only spans the rows currently on
    screen — use Export → Copy to clipboard instead;
  - dates use a fixed day-month-year format.

### Added

- Durable, revisioned processing jobs with crash recovery and live-work
  prioritization.
- Verified immutable model provenance, resumable downloads, and repair status.
- Transactional voice enrollment and reversible speaker identity updates.
- Accessible notifications, error recovery, keyboard interactions, focus
  handling, high-contrast support, and reduced-motion behavior.
- Capture/device health and privacy-safe diagnostics.
- CUDA runtime preflight: missing cuDNN/CUDA DLLs are named in the log and in
  the Settings GPU status instead of silently degrading to CPU transcription,
  and known cuDNN install locations are added to the loader path
  automatically.
- One-click "Download GPU libraries" in Settings → Models: pinned,
  SHA-256-verified NVIDIA-published CUDA runtime, cuDNN, and nvJitLink wheels
  hosted by PyPI install into the data directory (resumable, repairable, no
  admin rights), enabling GPU transcription on machines without a CUDA
  developer setup.
- Atomic, checksummed backup snapshots plus documented recovery procedures.
- Windows CI/release guidance and expanded privacy/security documentation.

### Changed

- Bounded capture/live queues report dropped time instead of growing without
  limit.
- Audio devices persist stable endpoint IDs and report missing configured
  devices explicitly.
- Audio processing streams long recordings with bounded inference chunks.
- Settings and model installs use validated atomic persistence.

### Fixed

- Recording start/stop rollback, partial archive publication, deletion versus
  processing races, stale frontend requests, pagination/search races, and
  database-sourced archive path traversal.
