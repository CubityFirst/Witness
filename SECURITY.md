# Security policy

## Supported versions

Witness is pre-1.0. Security fixes are made on the current default branch and
the newest published release only. Older builds should be upgraded rather than
treated as supported.

## Reporting a vulnerability

Prefer a private GitHub Security Advisory for this repository. Include the
affected version or commit, impact, reproduction steps, and any suggested
mitigation. If private reporting is unavailable, open a minimal public issue
requesting a private contact channel; do not publish exploit details, meeting
content, tokens, private paths, or other people's data.

Please allow maintainers reasonable time to reproduce and correct a report
before public disclosure. Do not access data that is not yours, disrupt other
systems, or use social engineering while researching Witness.

## Security model

Witness verifies pinned model downloads and constrains archived-audio paths,
but it is not a sandbox or a secrets vault. The local Windows account is the
trust boundary. Data is not encrypted at the application layer, and code
running as the user (or as administrator) can read recordings, transcripts,
voice prints, settings, logs, and backups. See [Privacy](PRIVACY.md).

### Optional AI assistant

Network use for AI is opt-in: the master switch "Use an AI service" is off by
default, and while it is off the core makes no AI request of any kind (the
worker re-checks it before every job). When it is on, requests go only to
the configured OpenAI-compatible base URL, which must be `https://` — plain
`http://` is accepted only for `localhost`, `127.0.0.1` and `[::1]`. TLS uses
SChannel (the Windows certificate store) and the system proxy, like the
updater; loopback connects directly. Redirects are not followed, so a request
and its key are never forwarded to another host, and non-loopback requests
refuse any downgrade to HTTP.

The API key is stored only in Windows Credential Manager as a generic
credential (`Witness/ai-api-key`, persisted for the current user on this
machine and protected by Windows with the user's logon credentials). It is
never written to `witness-settings.toml`, the database, logs, backups or the
diagnostics report; Settings never reads it back (it only learns whether one
is stored). In memory it is wrapped in a type whose `Debug` output is
redacted and whose bytes are zeroed when dropped. Error messages never carry
request headers or URLs, and text from the service (including its error
messages) is scrubbed of the key and of anything that looks like one before
it is shown or logged. As with the rest of Witness, code running as the user
can read the key.

Security-sensitive changes should preserve these invariants:

- no meeting content in telemetry or diagnostics exports;
- no AI request unless the user turned the AI assistant on, never plain HTTP
  to a non-loopback host, and the API key nowhere but Credential Manager and
  the request's `Authorization` header;
- no unverified model is loaded;
- database migrations are transactional and forward-only;
- paths originating in the database cannot escape their designated root;
- incomplete writes are never published as settings, recordings, models, or
  backups;
- there is no IPC surface: the UI is native code in the same process, with no
  embedded browser, local web server, or custom URL protocol (the only
  cross-process channel is the single-instance message, which only shows
  the window and is logged, never interpreted as a command); and
- the updater installs only an installer whose minisign signature verifies
  against the pinned public key and whose signed trusted comment carries the
  same `version:` as the update feed.

## Temporary advisory acceptance

Rust dependencies are checked against RustSec on every pull request. The gate
denies new unsoundness findings. It explicitly ignores `RUSTSEC-2024-0429`
(`glib`) because that crate is reached only through the tray library's Linux
GTK backend and is never compiled for Windows; it remains in the
cross-platform lockfile through upstream packages. The audit also reports
several unmaintained transitive crates, including the current Opus binding.
These are maintenance warnings rather than known vulnerabilities and remain
tracked through dependency updates. Recheck the exception whenever the UI,
tray, audio, or target dependencies change.
