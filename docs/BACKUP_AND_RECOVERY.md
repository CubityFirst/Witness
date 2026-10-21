# Backup and recovery

## Create a backup

1. Wait until nothing is recording or transcribing.
2. Open **Settings > Storage > Create backup** and pick a folder outside the
   Witness data directory (ideally an encrypted disk that isn't cloud-synced).
3. Witness writes a `Witness-backup-YYYYMMDD-HHMMSS` folder containing the
   database (transcripts, notes, voice prints), audio, recoverable in-progress
   recordings and your settings, with a SHA-256 checksum for every file.

Models and GPU libraries are not included; download them again after a
restore. A folder ending in `.incomplete` is a failed backup, not a backup.

## Verify (optional)

Checks every file against the backup's manifest:

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

## Restore

1. In Settings, choose a new, **empty** data directory. Don't restart yet.
2. Quit Witness from the tray.
3. Copy `witness.db`, `audio/` and `rec-tmp/` from the backup into the new
   directory.
4. Start Witness, re-download your models, and check a few meetings before
   deleting the old data directory.

Never restore into a non-empty directory; Witness can't merge archives.
Restore with the same or a newer version of Witness: a backup from 0.3.x
won't open in 0.2.x.
