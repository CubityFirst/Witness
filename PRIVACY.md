# Privacy

Witness is designed for local processing, but a meeting archive is highly
sensitive. This document describes the current application behavior; it is not
legal advice.

## Recording and consent

Witness can automatically record when a configured application uses the
microphone. The person operating Witness is responsible for determining which
laws, employment policies, contracts, and platform rules apply and for getting
every required participant's consent. Requirements vary by country, state, and
organization. A tray notification informs the local user; it does not notify
remote participants for you.

The loopback track records every sound sent to the selected output endpoint,
not just Teams. Notifications, media, accessibility audio, and other calls can
therefore enter a meeting archive. Use a dedicated endpoint where appropriate.

## Data Witness stores

- Microphone and output audio, first as recoverable WAV files and then as an
  Ogg Opus archive.
- Transcript text, speaker labels, timestamps, notes, bookmarks, and titles.
- Voice-print embeddings for diarized speakers and enrolled people. These are
  numerical representations derived from speech and may be considered
  biometric or similarly sensitive data under some policies or laws.
- Operational queue, capture-health, and model-provenance state.
- Preferences, audio endpoint identifiers, and watched-application patterns.
- A rotating local log containing operational messages and identifiers.
- If you use the optional AI assistant: the summaries it wrote (stored with
  the meeting, like notes) and its settings (base URL, model, feature
  switches). Its API key is not stored by Witness at all, but in Windows
  Credential Manager.

Voice prints are used only for on-device speaker matching. Explicitly renaming
a speaker enrolls or updates that person's print. Forgetting a person removes
the enrolled identity; meeting-level speaker data can remain until those
meetings are purged. Existing text labels are not rewritten merely by forgetting
the print.

## Network behavior

Witness has no account, analytics, advertising, telemetry, cloud transcript,
or cloud backup service. Meeting content is processed locally — unless you
turn on the optional AI assistant described below, which sends transcript
text to the service you configure.

When the user explicitly downloads or repairs a speech model, Witness connects
to the model host (currently Hugging Face). When the user downloads or repairs
the optional GPU libraries, Witness connects to PyPI's
`files.pythonhosted.org` host for pinned NVIDIA-published wheels. Those hosts
and ordinary network intermediaries can observe network metadata such as IP
address, time, requested package or repository, revision, and file. Audio,
transcripts, notes, and voice prints are not part of either request.

### Optional AI assistant (off by default)

Settings → AI assistant can connect Witness to an OpenAI-compatible service
of your choice — a hosted one such as OpenAI, or a server on your own
computer (Ollama, LM Studio, llama.cpp…) — to title meetings and write their
notes. Nothing is sent while **Use an AI service** is off: no requests, not
even a connection test. When it is on, Witness sends, to the configured base
URL only:

- the meeting's transcript text, as `[mm:ss] Speaker: text` lines;
- the speaker names shown in the transcript (including names you gave);
- the meeting title, when you or Teams set it (not the default
  "Meeting <date>" title);
- the API key, as the request's `Authorization` header, if you saved one.

It never sends audio, voice prints, your own notes, bookmarks, file paths or
other meetings. Requests happen when you click **Generate summary** /
**Generate title**, use **Load models** or **Test connection**, or — only if
you turned these switches on — after each transcription ("Title new meetings
automatically", "Write meeting notes automatically"). A hosted service
processes and may retain that text under its own terms; check its data
policy before turning the assistant on. Plain `http://` is accepted only for
`localhost`, `127.0.0.1` and `[::1]`, so the text never crosses the network
unencrypted; with a local server it does not leave the computer at all.

## Retention and deletion

Deleting a meeting moves it to the recycle bin. Binned meetings are purged
after 30 days at application startup, or immediately when the user empties the
bin or permanently purges an item. Purging removes its database rows and known
audio/recovery files. Storage media, filesystem journals, OS backups, forensic
recovery, and third-party synchronization can retain copies outside Witness's
control.

Backups created in Settings are independent complete copies. Deleting data from
the live archive does not delete it from an existing backup. Dispose of backups
and exported audio/transcripts separately.

## Security boundary

Witness does not encrypt its database, recordings, settings, logs, or backups.
Anyone who can read the Windows account's files can read or copy the archive.
An administrator, malware, disk-imaging tool, or configured backup/sync product
may also access it. Use BitLocker/device encryption, appropriate Windows account
controls, and a protected backup location. Avoid placing the data directory or
backup destination in a cloud-synchronized folder unless that is intentional.

The privacy-safe Diagnostics export excludes meeting content, notes, voice
prints, and private settings, but includes local filesystem paths and
operational details. Review it before sharing.

## User controls

- Disable automatic recording or transcription in Settings.
- Select exact audio endpoints and watched-application patterns.
- Disable live captions.
- Forget an enrolled person under Settings > People.
- Delete and then purge individual meetings, or empty the recycle bin.
- Keep the archive in a user-chosen local data directory.
- Create and independently protect checksummed backups.
- Leave the AI assistant off (the default), point it at a local server, or
  turn it off at any time; remove its API key with Settings → AI assistant →
  Remove (or from Windows Credential Manager, entry `Witness/ai-api-key`).
  Existing summaries stay with their meetings until the meetings are purged.

For a complete removal, quit Witness, remove its active data directory, remove
`witness-settings.toml`, and separately remove all exports and backups. Verify
the exact active paths in Diagnostics first.
