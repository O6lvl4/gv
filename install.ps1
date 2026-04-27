<#
.SYNOPSIS
    Install gv on Windows.

.DESCRIPTION
    Fetches the latest (or pinned) gv release, verifies its sha256, extracts
    gv.exe + gv-shim.exe, and drops them into a user-writable bin directory
    (default: $env:LOCALAPPDATA\gv\bin).

.PARAMETER Version
    Tag to install (e.g. v0.2.1). Defaults to the latest release.
    May also be set via the GV_VERSION env var.

.PARAMETER InstallDir
    Where to place gv.exe / gv-shim.exe. Defaults to
    $env:LOCALAPPDATA\gv\bin. May also be set via GV_INSTALL_DIR.

.EXAMPLE
    iwr https://raw.githubusercontent.com/O6lvl4/gv/main/install.ps1 | iex

.EXAMPLE
    $env:GV_VERSION = "v0.2.0"
    iwr https://raw.githubusercontent.com/O6lvl4/gv/main/install.ps1 | iex
#>

[CmdletBinding()]
param(
    [string]$Version    = $env:GV_VERSION,
    [string]$InstallDir = $env:GV_INSTALL_DIR
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3
$Repo = "O6lvl4/gv"

function Say($msg) { Write-Host "gv-install: $msg" }

if (-not $InstallDir) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "gv\bin"
}

function Resolve-Tag {
    if ($Version) { return $Version }
    Say "resolving latest release"
    $api = "https://api.github.com/repos/$Repo/releases/latest"
    $resp = Invoke-RestMethod -Uri $api -UseBasicParsing -Headers @{ "User-Agent" = "gv-install.ps1" }
    return $resp.tag_name
}

function Detect-Target {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -eq "AMD64" -or [Environment]::Is64BitOperatingSystem) {
        return "x86_64-pc-windows-msvc"
    }
    throw "unsupported architecture: $arch (gv ships x86_64-pc-windows-msvc only)"
}

function Verify-Sha256([string]$path, [string]$expected) {
    $actual = (Get-FileHash -Path $path -Algorithm SHA256).Hash.ToLower()
    if ($expected.ToLower() -ne $actual) {
        throw "sha256 mismatch: expected $expected, got $actual"
    }
}

$tag    = Resolve-Tag
$target = Detect-Target
$asset  = "gv-$tag-$target.tar.gz"
$url    = "https://github.com/$Repo/releases/download/$tag/$asset"

$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ("gv-install-" + [System.Guid]::NewGuid().ToString()))
try {
    Say "downloading $asset"
    $archivePath = Join-Path $tmp $asset
    Invoke-WebRequest -Uri $url -OutFile $archivePath -UseBasicParsing

    Say "verifying sha256"
    $shaPath = "$archivePath.sha256"
    Invoke-WebRequest -Uri "$url.sha256" -OutFile $shaPath -UseBasicParsing
    $expected = (Get-Content $shaPath -Raw).Trim().Split(" ")[0]
    Verify-Sha256 $archivePath $expected

    Say "extracting"
    & tar.exe -xzf $archivePath -C $tmp
    if ($LASTEXITCODE -ne 0) { throw "tar extraction failed (exit $LASTEXITCODE)" }
    $stage = Join-Path $tmp "gv-$tag-$target"

    if (-not (Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }
    Copy-Item -Force (Join-Path $stage "gv.exe")      (Join-Path $InstallDir "gv.exe")
    Copy-Item -Force (Join-Path $stage "gv-shim.exe") (Join-Path $InstallDir "gv-shim.exe")
    # Windows lacks reliable user-level symlinks; copy to gvx.exe instead.
    # argv[0]-stem dispatch in the gv binary rewrites `gvx …` → `gv x …`.
    Copy-Item -Force (Join-Path $InstallDir "gv.exe") (Join-Path $InstallDir "gvx.exe")

    Say "installed to $InstallDir"
    & (Join-Path $InstallDir "gv.exe") --version

    if ($env:Path -split ";" -notcontains $InstallDir) {
        Write-Host ""
        Say "$InstallDir is not on PATH. Add it persistently with:"
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"`$([Environment]::GetEnvironmentVariable('Path','User'));$InstallDir`", 'User')"
    }

    Say "done. Try: gv install latest"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
