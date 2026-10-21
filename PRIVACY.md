# Privacy

Witness records, transcribes and stores meetings entirely on your computer.
There is no account, telemetry, analytics or cloud service. Audio,
transcripts, notes and voice prints never leave the machine unless you turn
on the AI assistant.

## What Witness downloads

Witness only connects to the internet for these:

- **Updates** — checks GitHub Releases for a signed update (on by default;
  switch off under Settings > About). Installing one is always your click.
- **Speech models** — downloaded from Hugging Face when you click
  **Download** or **Repair** under Settings > Storage.
- **GPU libraries** (optional) — NVIDIA's CUDA/cuDNN files from PyPI
  (`files.pythonhosted.org`), only when you click **Download** under
  Settings > Transcription.

None of these requests contain meeting content. As with any download, the
host can see your IP address and what was fetched.

## AI assistant (off by default)

Witness sends meeting content anywhere only if you turn on **Settings > AI
assistant** and configure a service. Even then it sends only to that service
(a hosted one such as OpenAI, or a local server such as Ollama), and only:

- the transcript text and speaker names, and
- the meeting title, if you or Teams named it.

It never sends audio, voice prints, notes or bookmarks. While the switch is
off, nothing is sent, not even a connection test. A hosted service handles
that text under its own data policy. The API key is kept in Windows
Credential Manager and is only ever sent to the host it was saved for.

## Your responsibility

You are responsible for getting participants' consent to be recorded. The
archive is not encrypted by Witness; use BitLocker if it is sensitive.
