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

Security-sensitive changes should preserve these invariants:

- no meeting content in telemetry or diagnostics exports;
- no unverified model is loaded;
- database migrations are transactional and forward-only;
- paths originating in the database cannot escape their designated root;
- incomplete writes are never published as settings, recordings, models, or
  backups; and
- Tauri capabilities and content security policy stay least-privilege.

## Temporary advisory acceptance

As of 2026-08-11, `npm audit` reports the moderate
[PostCSS source-map advisory GHSA-fxqj-rqcc-2cmp](https://github.com/advisories/GHSA-fxqj-rqcc-2cmp)
through Vite, with no compatible fix available. Witness accepts this finding
temporarily because PostCSS is a development-only build dependency, packaged
applications do not include the build tool, and CI processes only
repository-controlled CSS with an explicit source path. CI continues to fail
on high or critical npm advisories, Dependabot tracks upstream releases, and
the waiver must be removed when a compatible patched dependency is available.
Rust dependencies are checked against RustSec on every pull request.

The RustSec gate denies new unsoundness findings. It explicitly ignores
`RUSTSEC-2026-0221` (`event-listener`) and `RUSTSEC-2024-0429` (`glib`) because
neither crate is present in the resolved Windows target dependency tree; they
remain in the cross-platform lockfile through upstream packages. The audit also
reports several unmaintained transitive crates, including the current Opus
binding. These are maintenance warnings rather than known vulnerabilities and
remain tracked through dependency updates. Recheck both exceptions whenever
Tauri, audio, or target dependencies change.
