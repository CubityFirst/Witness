# Changelog

All notable changes to Witness are documented here. The project is pre-1.0;
database and backup formats remain versioned, but user-visible behavior can
still evolve quickly.

## Unreleased

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
