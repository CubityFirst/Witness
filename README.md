<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/witness-banner-dark.png">
    <img alt="Witness" src="assets/brand/witness-banner-light.png" width="640">
  </picture>
</h1>

Witness is a local meeting recorder and transcriber for Windows 11. It
records Teams calls automatically (or on demand), transcribes them on your
computer, recognises recurring speakers, and keeps a searchable archive.
Nothing leaves your machine unless you turn on the optional AI assistant —
see [Privacy](PRIVACY.md).

> Witness is pre-1.0 software. Keep independent backups of anything you
> can't replace.

## Features

- Records your mic and the call's audio as separate tracks.
- On-device transcription (Parakeet on NVIDIA GPUs, or Whisper), with live captions.
- Tells speakers apart and recognises people across meetings.
- Full-text search, bookmarks, notes, and Markdown/text/SRT/WebVTT export.
- Optional AI titles and meeting notes via any OpenAI-compatible service.

## Install

Download `Witness_<version>_x64-setup.exe` from
[Releases](https://github.com/CubityFirst/Witness/releases) (no admin rights
needed). Witness lives in the tray; pick your microphone and output device
under **Settings > Recording** and download models under **Settings >
Storage**. Use a headset: there is no echo cancellation.

| Model | Used for | Size |
| --- | --- | ---: |
| Parakeet TDT 0.6B v3 | Transcription, live captions | 2.6 GB |
| Nemotron 3 Diarization | Telling speakers apart | 400 MB |
| ERes2Net | Recognising people | 26 MB |
| Whisper large-v3-turbo | Alternative transcription | 575 MB |

## Build

```powershell
cargo test --workspace
cargo run -p witness
cargo run -p xtask -- dist   # NSIS installer
```

Requirements and workflow are in [Contributing](CONTRIBUTING.md).

## More

[Privacy](PRIVACY.md) · [Troubleshooting](docs/TROUBLESHOOTING.md) ·
[Backup and recovery](docs/BACKUP_AND_RECOVERY.md) · [Security](SECURITY.md) ·
[MIT License](LICENSE)
