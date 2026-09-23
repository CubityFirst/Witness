[CmdletBinding()]
param(
    # Produce signed updater artifacts (.sig + latest.json) for a published
    # release. Needs TAURI_SIGNING_PRIVATE_KEY or TAURI_SIGNING_PRIVATE_KEY_PATH.
    [switch]$Sign,
    # Release notes embedded in latest.json (shown in Settings → Updates).
    [string]$Notes = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$releaseDirectory = Join-Path $projectRoot "src-tauri\target\release"
$runtimeDlls = @(
    "onnxruntime_providers_cuda.dll",
    "onnxruntime_providers_shared.dll"
)

function Invoke-Npm {
    param([Parameter(Mandatory)][string[]]$Arguments)

    & npm @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "npm $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

Push-Location $projectRoot
try {
    # ort-sys generates its runtime DLLs from a dependency build script. Build
    # the executable first so Tauri never tries to resolve those resources
    # before Cargo has produced them.
    Invoke-Npm -Arguments @("run", "tauri", "--", "build", "--no-bundle", "--ci")

    foreach ($dll in $runtimeDlls) {
        $path = Join-Path $releaseDirectory $dll
        # ort-sys prefers symlinks when Windows Developer Mode is available.
        # FileInfo.Length describes the link itself in Windows PowerShell 5.1,
        # so open the path to validate the target content instead.
        $stream = [System.IO.File]::Open(
            $path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::Read
        )
        try {
            $length = $stream.Length
        }
        finally {
            $stream.Dispose()
        }

        if ($length -le 0) {
            throw "Generated runtime DLL is empty: $path"
        }
        Write-Host "Verified generated runtime DLL: $dll ($length bytes)"
    }

    # Tauri's bundle command consumes an already-built application. Only this
    # phase applies the resource overlay, after the exact DLLs are present.
    $bundleArguments = @(
        "run",
        "tauri",
        "--",
        "bundle",
        "--config",
        "src-tauri/tauri.release.conf.json"
    )
    if ($Sign) {
        if (-not ($env:TAURI_SIGNING_PRIVATE_KEY -or $env:TAURI_SIGNING_PRIVATE_KEY_PATH)) {
            throw "-Sign needs TAURI_SIGNING_PRIVATE_KEY or TAURI_SIGNING_PRIVATE_KEY_PATH"
        }
        # The bundler itself only reads TAURI_SIGNING_PRIVATE_KEY.
        if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
            $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -LiteralPath $env:TAURI_SIGNING_PRIVATE_KEY_PATH -Raw).Trim()
        }
        $bundleArguments += @("--config", "src-tauri/tauri.updater.conf.json")
    }
    Invoke-Npm -Arguments ($bundleArguments + @("--ci"))

    $nsisWorkDirectory = Join-Path $releaseDirectory "nsis"
    $nsisScript = Get-ChildItem -LiteralPath $nsisWorkDirectory -Filter "installer.nsi" -File -Recurse |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if ($null -eq $nsisScript) {
        throw "Tauri did not produce an NSIS installer manifest in $nsisWorkDirectory"
    }

    $nsisManifest = Get-Content -LiteralPath $nsisScript.FullName -Raw
    foreach ($dll in $runtimeDlls) {
        if (-not $nsisManifest.Contains("/oname=$dll")) {
            throw "NSIS installer manifest does not contain runtime DLL: $dll"
        }
        Write-Host "Verified NSIS installer resource: $dll"
    }

    $installerDirectory = Join-Path $releaseDirectory "bundle\nsis"
    $installer = Get-ChildItem -LiteralPath $installerDirectory -Filter "*-setup.exe" -File |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if ($null -eq $installer -or $installer.Length -le 0) {
        throw "Tauri did not produce a non-empty NSIS installer in $installerDirectory"
    }

    Write-Host "Windows installer ready: $($installer.FullName) ($($installer.Length) bytes)"

    if ($Sign) {
        $signaturePath = "$($installer.FullName).sig"
        if (-not (Test-Path -LiteralPath $signaturePath)) {
            throw "Signed build did not produce an updater signature: $signaturePath"
        }
        $version = (Get-Content -LiteralPath (Join-Path $projectRoot "src-tauri\tauri.conf.json") -Raw |
            ConvertFrom-Json).version
        # GitHub serves release assets with spaces replaced by dots.
        $assetName = $installer.Name -replace " ", "."
        $feed = [ordered]@{
            version   = $version
            notes     = $Notes
            pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
            platforms = [ordered]@{
                "windows-x86_64" = [ordered]@{
                    signature = (Get-Content -LiteralPath $signaturePath -Raw).Trim()
                    url       = "https://github.com/CubityFirst/Witness/releases/download/v$version/$assetName"
                }
            }
        }
        $feedPath = Join-Path $installerDirectory "latest.json"
        # UTF-8 without BOM: the updater's JSON parser rejects a BOM.
        [System.IO.File]::WriteAllText(
            $feedPath,
            ($feed | ConvertTo-Json -Depth 4),
            [System.Text.UTF8Encoding]::new($false)
        )
        Write-Host "Update feed ready: $feedPath"
    }
}
finally {
    Pop-Location
}
