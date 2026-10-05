<#
.SYNOPSIS
  Upgrade smoke test: install the shipped Witness 0.2.1, then the new installer
  over it exactly as the 0.2.x updater does, and assert the update contract
  (design §8.1 / §8.4) still holds.

.DESCRIPTION
  Steps (the CI `upgrade-smoke` job runs this on a fresh windows-latest VM):
   1. Fetch Witness_0.2.1_x64-setup.exe from the v0.2.1 release (or use
      -OldInstaller) and check its SHA-256.
   2. Install it with /S and read $inst from HKCU\Software\github\Witness.
   3. Seed state: the autostart Run value (unquoted, as the 0.2.x plugin wrote
      it), fake user data in $inst, and an orphaned EBWebView profile.
   4. Run the new installer with /S /UPDATE, then again with
      /P /UPDATE /R /ARGS --minimized (what 0.2.x's updater runs). The
      relaunch is watched for while that installer runs and up to
      -RelaunchSeconds after it exits; it must be seen, with --minimized on
      its command line, or the test fails (-AllowMissingRelaunch downgrades
      that to a note). It is then stopped.
   5. Assert: DisplayVersion is the new version; InstallLocation,
      MainBinaryName and the install-dir key are unchanged; Run\Witness is
      untouched; the Start-menu shortcut still targets $inst\witness.exe with
      AUMID com.github.cubityfirst.witness; the seeded data survives; the
      EBWebView profile is gone.
   6. Uninstall with /S: the data still survives and Run\Witness is gone.
   7. Fresh-install the new installer with /S into the same dir (over the
      leftover data, design §8.6): the shortcuts it creates itself target
      $inst\witness.exe with the AUMID (update mode keeps 0.2.1's, so step 5
      cannot show this) and the data survives. Then uninstall again.

  Local rehearsal: the test writes to the current user's HKCU and shortcuts,
  and the installers kill any witness.exe of this user. It therefore refuses
  to start unless this profile has no Witness install, no Run\Witness value,
  no Witness shortcuts, no com.github.cubityfirst.witness profile and no
  running witness.exe, and it removes everything it created afterwards,
  including the install dir (-InstallDir, or %LOCALAPPDATA%\Witness, which
  must not exist beforehand). Pass -InstallDir to keep the test away from
  %LOCALAPPDATA%\Witness altogether.

.EXAMPLE
  .\installer\tests\upgrade-smoke.ps1 -NewInstaller target\release\bundle\nsis\Witness_0.3.0_x64-setup.exe

.EXAMPLE
  # Local rehearsal in a scratch dir, with the shipped 0.2.1 installer on disk.
  .\installer\tests\upgrade-smoke.ps1 -NewInstaller C:\t\Witness_0.2.2_x64-setup.exe `
      -OldInstaller C:\dl\Witness_0.2.1_x64-setup.exe -InstallDir C:\t\smoke\Witness
#>
[CmdletBinding()]
param(
  # The installer under test (Witness_X.Y.Z_x64-setup.exe).
  [Parameter(Mandatory = $true)] [string] $NewInstaller,
  # Version the new installer must register; default: parsed from its name.
  [string] $ExpectedVersion,
  # The shipped 0.2.1 installer; downloaded from the release when omitted.
  [string] $OldInstaller,
  # Install the old version here (/D=) instead of %LOCALAPPDATA%\Witness.
  [string] $InstallDir,
  # Seconds after the /R installer exits to wait for the relaunch.
  [int] $RelaunchSeconds = 10,
  # Only note (not fail) when the /R relaunch is not observed.
  [switch] $AllowMissingRelaunch
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

# --- The contract (installer/witness.nsi; xtask/src/main.rs `legacy`) -------
$OldUrl = 'https://github.com/CubityFirst/Witness/releases/download/v0.2.1/Witness_0.2.1_x64-setup.exe'
# SHA-256 of the v0.2.1 release asset (also recorded in installer/reference/PROVENANCE.md).
$OldSha256 = 'f586931a520fd6a16a587a3d44dad129dbb73a2900643afa3646573d927a8e6d'
$BundleId = 'com.github.cubityfirst.witness'
$ProductKey = 'Software\github\Witness'
$UninstKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Witness'
$RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run'
$InstallerTimeout = 300

$Programs = [Environment]::GetFolderPath('Programs')
$Desktop = [Environment]::GetFolderPath('DesktopDirectory')
$ProfileDir = Join-Path $env:LOCALAPPDATA $BundleId
$StartLnk = Join-Path $Programs 'Witness.lnk'
$DesktopLnk = Join-Path $Desktop 'Witness.lnk'

$script:Failures = New-Object System.Collections.Generic.List[string]

function Step([string] $Text) { Write-Host "`n== $Text" -ForegroundColor Cyan }
function Pass([string] $Text) { Write-Host "  ok   $Text" -ForegroundColor Green }
function Fail([string] $Text) {
  Write-Host "  FAIL $Text" -ForegroundColor Red
  $script:Failures.Add($Text)
}
function Check([bool] $Condition, [string] $Text) { if ($Condition) { Pass $Text } else { Fail $Text } }

# Registry reads straight through .NET: no environment expansion, value kind
# included, so "byte for byte" really compares what is stored.
function Get-RegValue([string] $SubKey, [string] $Name) {
  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($SubKey)
  if ($null -eq $key) { return $null }
  try {
    if ($key.GetValueNames() -notcontains $Name) { return $null }
    return [pscustomobject]@{
      Kind  = $key.GetValueKind($Name).ToString()
      Value = $key.GetValue($Name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    }
  } finally { $key.Dispose() }
}
function Test-RegKey([string] $SubKey) {
  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($SubKey)
  if ($null -eq $key) { return $false }
  $key.Dispose(); return $true
}

function Get-WitnessProcesses {
  @(Get-CimInstance Win32_Process -Filter "Name = 'witness.exe'" -ErrorAction SilentlyContinue)
}

# Run an installer and wait for that process only (Start-Process -Wait in
# Windows PowerShell also waits for descendants, which would include a
# relaunched app).
function Invoke-Installer([string] $Path, [string[]] $Arguments) {
  Write-Host "  > $(Split-Path $Path -Leaf) $($Arguments -join ' ')"
  $p = Start-Process -FilePath $Path -ArgumentList $Arguments -PassThru
  $null = $p.Handle # keeps ExitCode readable after exit in Windows PowerShell
  if (-not $p.WaitForExit($InstallerTimeout * 1000)) {
    try { $p.Kill() } catch { }
    throw "$Path $Arguments did not finish within $InstallerTimeout s"
  }
  if ($p.ExitCode -ne 0) { throw "$Path $Arguments exited with code $($p.ExitCode)" }
}

# Run an installer that should relaunch $ExePath (/R), watching for that
# process every 100 ms from the moment the installer starts: the relaunch
# happens in .onInstSuccess, before the installer exits, and a short-lived
# app could otherwise come and go unseen. Returns the first sighting
# (Id, CommandLine) or $null.
function Invoke-InstallerWatchingRelaunch([string] $Path, [string[]] $Arguments, [string] $ExePath) {
  Write-Host "  > $(Split-Path $Path -Leaf) $($Arguments -join ' ')"
  $p = Start-Process -FilePath $Path -ArgumentList $Arguments -PassThru
  $null = $p.Handle
  $started = Get-Date
  $graceEnd = $null
  $seen = $null
  while ($true) {
    if ($null -eq $seen) {
      $hit = @(Get-Process -Name witness -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $ExePath })
      if ($hit.Count -gt 0) {
        $cim = Get-CimInstance Win32_Process -Filter "ProcessId = $($hit[0].Id)" -ErrorAction SilentlyContinue
        $cmd = $null
        if ($null -ne $cim) { $cmd = $cim.CommandLine }
        $seen = [pscustomobject]@{ Id = $hit[0].Id; CommandLine = $cmd }
      }
    }
    if ($p.HasExited) {
      if ($null -eq $graceEnd) { $graceEnd = (Get-Date).AddSeconds($RelaunchSeconds) }
      if ($null -ne $seen -or (Get-Date) -gt $graceEnd) { break }
    } elseif (((Get-Date) - $started).TotalSeconds -gt $InstallerTimeout) {
      try { $p.Kill() } catch { }
      throw "$Path $Arguments did not finish within $InstallerTimeout s"
    }
    Start-Sleep -Milliseconds 100
  }
  if ($p.ExitCode -ne 0) { throw "$Path $Arguments exited with code $($p.ExitCode)" }
  return $seen
}

# uninstall.exe re-launches itself from %TEMP% and returns at once, so poll
# for the result instead of waiting on the process.
function Invoke-Uninstaller([string] $Dir) {
  Invoke-Installer (Join-Path $Dir 'uninstall.exe') @('/S')
  $deadline = (Get-Date).AddSeconds($InstallerTimeout)
  while ((Test-RegKey $UninstKey) -or (Test-Path -LiteralPath (Join-Path $Dir 'uninstall.exe'))) {
    if ((Get-Date) -gt $deadline) { throw "the uninstaller did not finish within $InstallerTimeout s" }
    Start-Sleep -Milliseconds 500
  }
}

function Get-Sha256([string] $Path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant() }

function Get-ShortcutTarget([string] $Path) {
  (New-Object -ComObject WScript.Shell).CreateShortcut($Path).TargetPath
}
function Get-ShortcutAumid([string] $Path) {
  $folder = (New-Object -ComObject Shell.Application).Namespace((Split-Path $Path -Parent))
  $item = $folder.ParseName((Split-Path $Path -Leaf))
  if ($null -eq $item) { return $null }
  $item.ExtendedProperty('System.AppUserModel.ID')
}

function Remove-IfPresent([string] $Path) {
  # Retries for a few seconds: files of a just-stopped process can stay
  # locked briefly.
  for ($i = 0; $i -lt 10 -and (Test-Path -LiteralPath $Path); $i++) {
    if ($i -gt 0) { Start-Sleep -Seconds 1 }
    Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction SilentlyContinue
  }
  if (Test-Path -LiteralPath $Path) { Write-Host "  note could not remove $Path" -ForegroundColor Yellow }
}

# --- Preconditions ------------------------------------------------------------
$NewInstaller = (Resolve-Path -LiteralPath $NewInstaller).Path
if (-not $ExpectedVersion) {
  if ((Split-Path $NewInstaller -Leaf) -notmatch '^Witness_(\d+\.\d+\.\d+)_x64-setup\.exe$') {
    throw "Cannot parse the version from $NewInstaller; pass -ExpectedVersion"
  }
  $ExpectedVersion = $Matches[1]
}
if ($InstallDir) { $InstallDir = [IO.Path]::GetFullPath($InstallDir) }

Step "Preconditions (new installer $ExpectedVersion)"
$dirty = @()
if (Test-RegKey $ProductKey) { $dirty += "HKCU\$ProductKey exists" }
if (Test-RegKey $UninstKey) { $dirty += "HKCU\$UninstKey exists" }
if ($null -ne (Get-RegValue $RunKey 'Witness')) { $dirty += "HKCU\$RunKey\Witness exists" }
foreach ($p in @($StartLnk, $DesktopLnk, $ProfileDir)) { if (Test-Path -LiteralPath $p) { $dirty += "$p exists" } }
if (@(Get-WitnessProcesses).Count -gt 0) { $dirty += 'witness.exe is running (the installers would kill it)' }
if ($InstallDir -and (Test-Path -LiteralPath $InstallDir)) { $dirty += "$InstallDir already exists" }
# Without -InstallDir, 0.2.1 installs into %LOCALAPPDATA%\Witness and the
# seeded fake witness.db would land there: never risk a real data dir.
$defaultDir = Join-Path $env:LOCALAPPDATA 'Witness'
if (-not $InstallDir -and (Test-Path -LiteralPath $defaultDir)) {
  $dirty += "$defaultDir exists (it may hold real data; pass -InstallDir for a local run)"
}
if ($dirty.Count -gt 0) {
  throw "This profile is not clean; refusing to touch it:`n  - $($dirty -join "`n  - ")"
}
Pass 'profile has no Witness install, autostart, shortcuts, WebView2 profile or running app'

$inst = $null
try {
  # --- 1. The shipped 0.2.1 installer ----------------------------------------
  Step 'Shipped 0.2.1 installer'
  if (-not $OldInstaller) {
    $OldInstaller = Join-Path ([IO.Path]::GetTempPath()) 'Witness_0.2.1_x64-setup.exe'
    Write-Host "  downloading $OldUrl"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $OldUrl -OutFile $OldInstaller -UseBasicParsing
  }
  $OldInstaller = (Resolve-Path -LiteralPath $OldInstaller).Path
  $oldHash = Get-Sha256 $OldInstaller
  if ($oldHash -ne $OldSha256) { throw "SHA-256 of $OldInstaller is $oldHash, expected $OldSha256" }
  Pass "SHA-256 $oldHash"

  # --- 2. Install it -----------------------------------------------------------
  Step 'Install 0.2.1 with /S'
  $oldArgs = @('/S')
  if ($InstallDir) { $oldArgs += "/D=$InstallDir" } # NSIS: /D= must come last, unquoted
  Invoke-Installer $OldInstaller $oldArgs
  $instValue = Get-RegValue $ProductKey ''
  if ($null -eq $instValue) { throw "0.2.1 did not write HKCU\$ProductKey" }
  $inst = [string] $instValue.Value
  Write-Host "  `$inst = $inst"
  if ($InstallDir) { Check ($inst -eq $InstallDir) "install dir is the requested $InstallDir" }
  Check ((Get-RegValue $UninstKey 'DisplayVersion').Value -eq '0.2.1') 'DisplayVersion 0.2.1'
  Check (Test-Path -LiteralPath (Join-Path $inst 'witness.exe')) '0.2.1 witness.exe installed'

  # --- 3. Seed state -----------------------------------------------------------
  Step 'Seed autostart, user data and the WebView2 profile'
  $runData = "$inst\witness.exe --minimized" # unquoted, exactly as tauri-plugin-autostart wrote it
  $run = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($RunKey)
  try { $run.SetValue('Witness', $runData, [Microsoft.Win32.RegistryValueKind]::String) } finally { $run.Dispose() }

  # The settings point the relaunched app at a separate data dir, so the
  # seeded files below are only ever touched by the installers under test.
  $smokeData = Join-Path $inst 'smoke-data'
  $seeded = [ordered]@{
    'witness.db'            = [byte[]](1..64 | ForEach-Object { 0xA5 })
    'audio\1.opus'          = [Text.Encoding]::ASCII.GetBytes('OggS fake opus archive for the upgrade smoke test')
    'witness-settings.toml' = [Text.Encoding]::UTF8.GetBytes("data_dir = '$smokeData'`r`ncheck_for_updates = false`r`n")
  }
  $hashes = @{}
  foreach ($rel in $seeded.Keys) {
    $path = Join-Path $inst $rel
    New-Item -ItemType Directory -Force -Path (Split-Path $path -Parent) | Out-Null
    [IO.File]::WriteAllBytes($path, $seeded[$rel])
    $hashes[$rel] = Get-Sha256 $path
  }
  New-Item -ItemType Directory -Force -Path (Join-Path $ProfileDir 'EBWebView') | Out-Null
  New-Item -ItemType File -Force -Path (Join-Path $ProfileDir 'EBWebView\x') | Out-Null

  $before = @{
    InstallLocation = Get-RegValue $UninstKey 'InstallLocation'
    MainBinaryName  = Get-RegValue $UninstKey 'MainBinaryName'
    InstDir         = Get-RegValue $ProductKey ''
    Run             = Get-RegValue $RunKey 'Witness'
  }
  Pass "seeded Run\Witness = $runData, $($seeded.Count) data files, EBWebView\x"

  # --- 4. The new installer, as the updater runs it ------------------------------
  Step "Update to $ExpectedVersion with /S /UPDATE"
  Invoke-Installer $NewInstaller @('/S', '/UPDATE')
  Check (-not (Test-Path -LiteralPath (Join-Path $ProfileDir 'EBWebView'))) 'EBWebView profile removed'
  Check (-not (Test-Path -LiteralPath $ProfileDir)) "empty $BundleId profile dir removed"

  Step "Update again with /P /UPDATE /R /ARGS --minimized (the 0.2.x updater's command line)"
  $exePath = Join-Path $inst 'witness.exe'
  $relaunch = Invoke-InstallerWatchingRelaunch $NewInstaller @('/P', '/UPDATE', '/R', '/ARGS', '--minimized') $exePath
  if ($null -ne $relaunch) {
    $cmd = $relaunch.CommandLine
    Check ($null -ne $cmd -and $cmd -like '*--minimized*') "relaunched $exePath with /ARGS (pid $($relaunch.Id): $cmd)"
  } elseif ($AllowMissingRelaunch) {
    Write-Host "  note no witness.exe from $inst seen within $RelaunchSeconds s of the installer exiting (-AllowMissingRelaunch)" -ForegroundColor Yellow
  } else {
    Fail "no witness.exe from $inst seen within $RelaunchSeconds s of the installer exiting: /R /ARGS relaunch missing"
  }
  # Whether it is still running does not matter (the app may exit early on a
  # headless runner); stop whatever is left before the assertions.
  foreach ($p in @(Get-WitnessProcesses | Where-Object { $_.ExecutablePath -eq $exePath })) {
    Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
  }
  Start-Sleep -Seconds 1

  # --- 5. The contract -----------------------------------------------------------
  Step 'Assert the update contract'
  Check ((Get-RegValue $UninstKey 'DisplayVersion').Value -eq $ExpectedVersion) "DisplayVersion = $ExpectedVersion"
  foreach ($name in @('InstallLocation', 'MainBinaryName')) {
    $now = Get-RegValue $UninstKey $name
    Check ($null -ne $now -and $now.Kind -eq $before[$name].Kind -and $now.Value -ceq $before[$name].Value) "$name unchanged ($($before[$name].Value))"
  }
  $now = Get-RegValue $ProductKey ''
  Check ($null -ne $now -and $now.Value -ceq $before.InstDir.Value) "HKCU\$ProductKey (Default) unchanged"
  $now = Get-RegValue $RunKey 'Witness'
  Check ($null -ne $now -and $now.Kind -eq $before.Run.Kind -and $now.Value -ceq $before.Run.Value) 'Run\Witness untouched'
  foreach ($f in @('witness.exe', 'onnxruntime_providers_cuda.dll', 'onnxruntime_providers_shared.dll', 'uninstall.exe')) {
    Check (Test-Path -LiteralPath (Join-Path $inst $f)) "$f installed"
  }
  Check ((Get-ShortcutTarget $StartLnk) -eq $exePath) "Start-menu shortcut targets $exePath"
  Check ((Get-ShortcutAumid $StartLnk) -eq $BundleId) "Start-menu shortcut AUMID = $BundleId"
  if (Test-Path -LiteralPath $DesktopLnk) {
    Check ((Get-ShortcutTarget $DesktopLnk) -eq $exePath) "desktop shortcut targets $exePath"
  }
  foreach ($rel in $seeded.Keys) {
    $path = Join-Path $inst $rel
    Check ((Test-Path -LiteralPath $path) -and (Get-Sha256 $path) -eq $hashes[$rel]) "$rel survives unchanged"
  }

  # --- 6. Uninstall --------------------------------------------------------------
  Step 'Uninstall with /S'
  Invoke-Uninstaller $inst
  Check (-not (Test-RegKey $UninstKey)) 'Uninstall\Witness removed'
  Check ($null -eq (Get-RegValue $RunKey 'Witness')) 'Run\Witness removed'
  foreach ($f in @('witness.exe', 'onnxruntime_providers_cuda.dll', 'onnxruntime_providers_shared.dll')) {
    Check (-not (Test-Path -LiteralPath (Join-Path $inst $f))) "$f removed"
  }
  Check (-not (Test-Path -LiteralPath $StartLnk)) 'Start-menu shortcut removed'
  foreach ($rel in $seeded.Keys) {
    $path = Join-Path $inst $rel
    Check ((Test-Path -LiteralPath $path) -and (Get-Sha256 $path) -eq $hashes[$rel]) "$rel still survives"
  }

  # --- 7. Fresh install over the leftover data -------------------------------------
  # Update mode leaves 0.2.1's shortcuts alone, so only a fresh install shows
  # that witness.nsi's own CreateShortcut + SetLnkAppUserModelId produce the
  # AUMID toasts depend on. /S creates both shortcuts and does not launch
  # the app (no /R).
  Step "Fresh install of $ExpectedVersion with /S over the leftover data"
  Invoke-Installer $NewInstaller @('/S', "/D=$inst")
  Check ((Get-RegValue $UninstKey 'DisplayVersion').Value -eq $ExpectedVersion) "DisplayVersion = $ExpectedVersion"
  Check ((Get-RegValue $ProductKey '').Value -eq $inst) "HKCU\$ProductKey (Default) = $inst"
  foreach ($lnk in @($StartLnk, $DesktopLnk)) {
    if (Test-Path -LiteralPath $lnk) {
      Check ((Get-ShortcutTarget $lnk) -eq $exePath) "$lnk targets $exePath"
      Check ((Get-ShortcutAumid $lnk) -eq $BundleId) "$lnk AUMID = $BundleId"
    } else {
      Fail "$lnk not created"
    }
  }
  foreach ($rel in $seeded.Keys) {
    $path = Join-Path $inst $rel
    Check ((Test-Path -LiteralPath $path) -and (Get-Sha256 $path) -eq $hashes[$rel]) "$rel survives the fresh install"
  }
  Check (@(Get-WitnessProcesses | Where-Object { $_.ExecutablePath -eq $exePath }).Count -eq 0) 'no app launched without /R'

  Step 'Uninstall the fresh install with /S'
  Invoke-Uninstaller $inst
  Check (-not (Test-RegKey $UninstKey)) 'Uninstall\Witness removed'
  Check (-not (Test-Path -LiteralPath $StartLnk) -and -not (Test-Path -LiteralPath $DesktopLnk)) 'shortcuts removed'
} catch {
  Fail "aborted: $($_.Exception.Message)"
} finally {
  # --- Leave the profile as clean as it was ------------------------------------
  Step 'Clean up'
  foreach ($p in Get-WitnessProcesses) {
    if ($inst -and $p.ExecutablePath -like "$inst\*") { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
  }
  if ($inst -and (Test-RegKey $UninstKey)) {
    $u = Join-Path $inst 'uninstall.exe'
    if (Test-Path -LiteralPath $u) {
      try { Start-Process -FilePath $u -ArgumentList '/S' -Wait } catch { }
      Start-Sleep -Seconds 3
    }
  }
  $hkcu = [Microsoft.Win32.Registry]::CurrentUser
  foreach ($k in @($UninstKey, $ProductKey)) { try { $hkcu.DeleteSubKeyTree($k, $false) } catch { } }
  try {
    $github = $hkcu.OpenSubKey('Software\github')
    if ($null -ne $github) {
      $empty = ($github.SubKeyCount -eq 0 -and $github.ValueCount -eq 0); $github.Dispose()
      if ($empty) { $hkcu.DeleteSubKey('Software\github', $false) }
    }
  } catch { }
  $run = $hkcu.OpenSubKey($RunKey, $true)
  if ($null -ne $run) { try { $run.DeleteValue('Witness', $false) } finally { $run.Dispose() } }
  Remove-IfPresent $StartLnk
  Remove-IfPresent $DesktopLnk
  # A relaunched Tauri-era stand-in (local rehearsals) recreates the WebView2
  # profile; Remove-IfPresent retries while its webview children exit.
  Remove-IfPresent $ProfileDir
  # The seeded fake data and settings (data_dir = smoke-data) must not stay
  # behind for a later real install to pick up. The preconditions proved the
  # dir did not exist before, so all of it is ours.
  if ($InstallDir) {
    Remove-IfPresent $InstallDir
  } elseif ($inst -and $inst -eq $defaultDir) {
    Remove-IfPresent $inst
  }
  Pass 'removed the test install dir, registry values, shortcuts and profile'
}

if ($script:Failures.Count -gt 0) {
  Write-Host "`nupgrade smoke test FAILED ($($script:Failures.Count)):" -ForegroundColor Red
  $script:Failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
  exit 1
}
Write-Host "`nupgrade smoke test passed: 0.2.1 -> $ExpectedVersion" -ForegroundColor Green
exit 0
