# Backup and recovery

Witness backups are directory snapshots intended for disaster recovery, not
for merging two archives or transferring live state while the app runs.

## Create a backup

1. Wait until no recording or transcription is active and the processing queue
   is empty.
2. Open **Settings > Storage > Create backup**.
3. Choose an existing folder outside the active Witness data directory. Prefer
   an encrypted local/removable disk that is not automatically cloud-synced.
4. Wait for the success notification and record the reported destination.

Witness first creates a uniquely named hidden-ish `.incomplete` sibling. It
copies archived audio and recoverable recordings, serializes the current saved
settings and model status, creates a consistent database snapshot with SQLite's
online backup API, calculates SHA-256 for every payload, writes
`backup-manifest-v1.json`, and finally renames the directory to
`Witness-backup-YYYYMMDD-HHMMSS`. It never overwrites an existing backup.

If any read, write, integrity check, or publication step fails, no final backup
directory is published. Witness attempts to remove only the incomplete staging
directory it created. The live archive is never modified.

Model binaries and the optional `cuda/` GPU libraries are excluded because they
are large and reproducibly downloadable. `model-status.json` records expected
speech-model revisions and integrity state; GPU libraries must be downloaded
again from Settings after a restore. Voice prints are inside `witness.db` and
therefore are included in the backup. The manifest records both exclusions
explicitly.

## Verify a backup

Verification is recommended before deleting source data or disconnecting a
backup disk. In PowerShell, run the following with the exact backup path:

```powershell
$witnessBackup = 'D:\Backups\Witness-backup-20260801-120000'
$manifest = Get-Content -LiteralPath (Join-Path $witnessBackup 'backup-manifest-v1.json') -Raw | ConvertFrom-Json
foreach ($entry in $manifest.files) {
  $file = Join-Path $witnessBackup ($entry.path -replace '/', '\')
  if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing $($entry.path)" }
  $actual = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($actual -ne $entry.sha256) { throw "Checksum mismatch: $($entry.path)" }
}
'Witness backup verified'
```

This detects missing or changed payload files. It does not prove that the
backup medium itself will remain reliable; keep more than one generation when
the archive matters.

## Restore without overwriting the live archive

The safest restore uses a new empty data directory and keeps the old one intact
until the recovered archive is verified.

1. Verify the backup checksums as above.
2. In Witness Settings, choose a new, empty data directory. Do not restart yet.
3. Quit Witness from the tray. Confirm that recording and processing have
   stopped. Never copy database files while Witness is running.
4. Copy `witness.db`, `audio/`, and `rec-tmp/` from the backup into the new data
   directory. Do not copy `backup-manifest-v1.json` as application data.
5. Start Witness. It opens the new directory, validates/migrates the database,
   and queues any recoverable `rec-tmp` recordings.
6. Re-download the selected models in Settings and confirm their integrity.
7. Check several meetings, transcripts, voice labels, and recordings before
   disposing of the old data directory.

The backup's `witness-settings.toml` is a reference copy of saved preferences.
Normally, keep the live settings file and only change its data directory through
the UI. If Witness cannot start, copy the backup settings file to the location
identified by `WITNESS_SETTINGS` (or beside `witness.exe`) and edit `data_dir`
to the absolute new recovery directory before launching. Keep an untouched copy
of the backup while editing.

Do not restore into a non-empty data directory: colliding meeting IDs, audio
filenames, model cache state, or WAL sidecars can produce an incoherent archive.
Witness does not support merging databases.

## Partial and crash recovery

- A directory whose name ends in `.incomplete` is not a backup and should not
  be restored. It was never published or checksummed completely.
- `rec-tmp` WAV/sidecar pairs are included so an interrupted active archive can
  be recovered on next startup. Preserve both mic and loopback WAVs plus the
  `.meta.toml` sidecar.
- If a backup verifies but SQLite later reports a health failure, retain every
  copy and work on a duplicate. Do not run repair tools against the only copy.
- Backups can contain meetings currently in the recycle bin. Their original
  deletion timestamps and future retention behavior are preserved.
