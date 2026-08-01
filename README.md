# Witness

Local-only meeting recorder & transcriber for Windows 11. Witness detects
when you're in a Microsoft Teams call, records a copy of the audio (your mic
and everything you hear, as separate tracks), transcribes it on-device with
speaker labels, and keeps a browsable, full-text-searchable archive of every
meeting. Nothing ever leaves your machine.

Sibling project to Vigil — same single-process, tray-first philosophy.

## Features

- **Auto-record Teams calls** — watches the Windows microphone consent store;
  recording starts ~4 s after you join and stops ~10 s after you leave, with
  a tray notification. Manual record button too.
- **Two tracks** — default mic (you) + device loopback (everyone else), so
  "Me vs Them" separation is free. Note: device loopback captures *all*
  system audio during the meeting (notification dings, music).
- **On-device ASR** — Parakeet TDT 0.6B v3 (ONNX Runtime + CUDA, very fast)
  or Whisper large-v3-turbo (CPU by default). Switchable per meeting.
- **Speaker diarization** — Sortformer labels up to 4 remote speakers;
  rename them per meeting.
- **Voice recognition across meetings** — rename a speaker once and Witness
  learns their voice (a local voice-print embedding); future meetings label
  them automatically (shown with a ≈ until you confirm). Manage or forget
  enrolled voices in Settings → People. Auto-matches never alter a stored
  voice print — only your explicit renames do.
- **Archive** — stereo Ogg Opus (~18 MB/h), instant seeking, click any
  transcript line to hear it.
- **Full-text search** — SQLite FTS5 across all meetings, with snippets and
  deep links into transcripts.

## Building

Requirements:
- Rust (MSVC toolchain) + Node 20+
- Visual Studio Build Tools with C++ **and CMake**, plus LLVM 19.x for
  libclang (bindgen). Neither needs to be on `PATH` — `.cargo/config.toml`
  points the build at them; adjust the paths there if your installs differ.
- For GPU transcription at runtime: NVIDIA driver + **cuDNN 9** (the ONNX
  Runtime CUDA binaries are downloaded automatically at build time). If the
  CUDA provider fails to initialize, transcription falls back to CPU with a
  logged warning.

```powershell
npm install
npm run tauri dev      # development
npm run tauri build    # release (NSIS installer)
```

`whisper-rs` builds CPU-only by default; add `--features whisper-cuda` (and
have the CUDA toolkit installed) to build Whisper with CUDA.

## Data layout & settings

Settings live in `witness-settings.toml` next to the exe (override location
with the `WITNESS_SETTINGS` env var). The data directory (configurable in
Settings) holds:

```
witness.db      # meetings, transcripts, FTS index
audio/          # archived {meeting}.opus files
rec-tmp/        # in-flight WAVs (removed after encoding)
models/         # downloaded ASR/diarization models
witness.log
```

Models are downloaded on demand from Hugging Face (Settings → Models); the
app records fine with zero models installed.

## Notes

- Witness is a single tray process — no background daemon. Quit from the
  tray stops any active recording cleanly; interrupted transcriptions resume
  on next launch.
- Per-process loopback capture of `ms-teams.exe` is deliberately not used
  (known Windows bug records silence); Witness captures the default render
  device instead.
- Diarization covers up to 4 remote speakers.

## License

MIT
