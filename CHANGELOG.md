# Changelog

All notable changes to Witness are documented here. The project is pre-1.0;
database and backup formats remain versioned, but user-visible behavior can
still evolve quickly.

## Unreleased

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
