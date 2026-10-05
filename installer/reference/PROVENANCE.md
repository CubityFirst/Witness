# Provenance of `installer/reference/`

These four files are the **verbatim** NSIS sources that `@tauri-apps/cli`
2.11.4 generated for Witness **v0.2.1**, the last Tauri release. They are kept
unedited (UTF-8 with BOM, CRLF, absolute build paths included) so that
`installer/witness.nsi`, `utils.nsh` and `English.nsh` can always be diffed
against the template that produced every shipped 0.2.x installer.

| File | SHA-256 |
|---|---|
| `installer.nsi` | `4b402ffb68ced1ef49e0058633da8d929634ba4a7452c976c1be39388e9fad3c` |
| `utils.nsh` | `fd766caf19dd635793d14e7a4247c454fbf7a179d50b80947845e0abb30cf35f` |
| `English.nsh` | `79c5775a167222dd72eda103861d55581cddc58735d65f535a3dead9320c537c` |
| `FileAssociation.nsh` | `e933990b06dceac358fc1a4e71cb55c7dc9783ad4df8fc2c6248118a7ad4d348` |

## How they were regenerated (2026-10-05)

1. `git worktree add --detach <dir> v0.2.1` (commit `87820ee`, "chore: release 0.2.1").
2. In that worktree: `npm ci` (lockfile pins `@tauri-apps/cli` **2.11.4**,
   integrity `sha512-R8xGtMpw…6zqTnH6hqQ==`), then
   `npx tauri build --no-bundle --ci`, then
   `npx tauri bundle --config src-tauri/tauri.release.conf.json --ci`. This is
   the same two-phase flow `scripts/build-windows-release.ps1` ran in the 0.2.x
   release workflow, without the updater overlay (signing does not change the
   script).
3. Copied `src-tauri/target/release/nsis/x64/{installer.nsi,utils.nsh,English.nsh,FileAssociation.nsh}`
   here, replacing the foundation unit's seed.

Toolchain the CLI used: NSIS **v3.11** (`makensis -VERSION`) and
`nsis_tauri_utils.dll` v0.5.3, SHA-256
`5ba143b5db4a87d32d6e7802e033330aae56cbceabe0d1e3ba41948385ad4709`. That DLL
is byte-identical to the `$PLUGINSDIR\nsis_tauri_utils.dll` unpacked from the
**shipped** `Witness_0.2.1_x64-setup.exe` release asset (SHA-256
`f586931a520fd6a16a587a3d44dad129dbb73a2900643afa3646573d927a8e6d`), and the
shipped installer installs the same four files (`witness.exe`, both
`onnxruntime_providers_*.dll`, `uninstall.exe`).

## Difference from the seed (local 0.1.0 build, 2026-09-23)

The foundation unit seeded this directory from a local pre-release build
(`!define VERSION "0.1.0"`, made before the updater commit `0f09f59`). Diffing
that seed against the regenerated 0.2.1 output shows **no template
difference**: `utils.nsh`, `English.nsh` and `FileAssociation.nsh` are
byte-identical, and `installer.nsi` differs only in build-specific values:

| Line | 0.1.0 seed | 0.2.1 regenerated |
|---|---|---|
| 36–37 | `VERSION "0.1.0"`, `VERSIONWITHBUILD "0.1.0.0"` | `"0.2.1"`, `"0.2.1.0"` |
| 47 | `MAINBINARYSRCPATH "G:\Scripts\Witness\…\witness.exe"` | `"C:\witness-port\wt\ref-021\…\witness.exe"` |
| 64 | `ESTIMATEDSIZE "134121"` | `"102522"` |
| 465 | `!include "G:\…\nsis\x64\English.nsh"` | `!include "C:\…\nsis\x64\English.nsh"` |
| 642–643 | `File /a "/oname=…dll" "G:\…\release\…dll"` | same with the `C:\…` build path |

So the template behind the shipped 0.2.0/0.2.1 installers is the one recorded
here (same CLI version at every 0.2.x commit, same output).

`ESTIMATEDSIZE` is the Tauri bundler's sum of the installed files' sizes in
bytes, divided by 1024 (integer): `(42695680 + 62276096 + 10752) / 1024 =
102522` for this build. `cargo run -p xtask -- dist` computes it the same way.

## VERSIONINFO test fixtures (`xtask/fixtures/versioninfo-*.bin`)

The `VS_VERSIONINFO` resources, cut verbatim (from the `VS_VERSION_INFO` key
back to its `wLength` header, `wLength` bytes) out of real 0.2.1 binaries.
They let the `dist` / `sign` ProductVersion guards be tested against real
resource layouts without checking in the binaries:

| Fixture | Cut from | Source SHA-256 |
|---|---|---|
| `versioninfo-witness-0.2.1.bin` (464 bytes, tauri-build/winres) | `witness.exe` of the reference build above | `ae5c09dae193da59bc5294753d84933160ac94509e07660b234d429aff4892a5` |
| `versioninfo-setup-0.2.1.bin` (456 bytes, NSIS `VIAddVersionKey`) | the shipped `Witness_0.2.1_x64-setup.exe` | `f586931a520fd6a16a587a3d44dad129dbb73a2900643afa3646573d927a8e6d` |
