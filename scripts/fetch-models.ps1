#!/usr/bin/env pwsh
# Fetch + verify the model blobs the app bundles (Kokoro TTS model +
# voicepacks + Whisper STT model).
#
# Driven by models/CHECKSUMS.txt: every entry is hashed; files that are
# missing or fail SHA-256 are (re-)downloaded from the `models-v1` GitHub
# release and re-verified. Files that already verify are never touched, so
# re-running is a cheap no-op and a corrupt file self-heals.
#
# These blobs used to live in the repo via Git LFS, but the release
# workflow's LFS pulls (~820 MB x 2 jobs x every tag) burned the LFS
# bandwidth quota. They are now hosted as assets on the `models-v1`
# release — asset downloads are unmetered. Upstream provenance (HuggingFace
# URLs) is documented in models/CHECKSUMS.txt.
#
# Usage:
#   pwsh scripts/fetch-models.ps1              # fetch whatever is missing/corrupt
#   pwsh scripts/fetch-models.ps1 -VerifyOnly  # report + fail instead of downloading
#
# Used by .github/workflows/release.yml (both jobs, wrapped in actions/cache)
# and locally after a fresh clone. Linux/WSL twin: scripts/fetch-models.sh.
# Maintainers: to publish new/updated blobs see scripts/publish-models-release.ps1.

[CmdletBinding()]
param(
  [string]$Repo = 'Dyserna/cImp',
  [string]$ReleaseTag = 'models-v1',
  [switch]$VerifyOnly
)

$ErrorActionPreference = 'Stop'

$modelsDir = Join-Path (Split-Path -Parent $PSScriptRoot) 'models'
$checksumFile = Join-Path $modelsDir 'CHECKSUMS.txt'
if (-not (Test-Path $checksumFile)) { throw "not found: $checksumFile" }

function Get-Sha256([string]$Path) {
  (Get-FileHash -Algorithm SHA256 -Path $Path).Hash.ToLower()
}

function Get-Asset([string]$AssetName, [string]$Dest) {
  $url = "https://github.com/$Repo/releases/download/$ReleaseTag/$AssetName"
  $tmp = "$Dest.download"
  # curl ships with Windows 10+ and every CI runner; Invoke-WebRequest is the
  # fallback for exotic environments.
  $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
  if (-not $curl) {
    $curl = Get-Command curl -CommandType Application -ErrorAction SilentlyContinue
  }
  if ($curl) {
    & $curl.Source -L --fail --retry 3 --silent --show-error -o $tmp $url
    if ($LASTEXITCODE -ne 0) { throw "curl exited $LASTEXITCODE for $url" }
  } else {
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
  }
  Move-Item -Force $tmp $Dest
}

$entries = Get-Content $checksumFile | Where-Object { $_ -and $_ -notmatch '^\s*#' }
$failed = $false
foreach ($line in $entries) {
  $parts = $line.Trim() -split '\s+', 2
  if ($parts.Count -lt 2) { continue }
  $expected = $parts[0].ToLower()
  $rel = $parts[1].Trim()
  $path = Join-Path $modelsDir $rel

  if ((Test-Path $path) -and ((Get-Sha256 $path) -eq $expected)) {
    Write-Host "ok       $rel"
    continue
  }

  if ($VerifyOnly) {
    Write-Host "BAD      $rel (missing or checksum mismatch)"
    $failed = $true
    continue
  }

  New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null
  Write-Host "fetching $rel"
  Get-Asset (Split-Path -Leaf $rel) $path
  $actual = Get-Sha256 $path
  if ($actual -eq $expected) {
    Write-Host "fetched  $rel"
  } else {
    Write-Host "FAILED   $rel (sha256 $actual, expected $expected)"
    $failed = $true
  }
}

if ($failed) { exit 1 }
# Explicit success code: publish-models-release.ps1 gates on $LASTEXITCODE,
# which stays stale/null (→ treated as failure) if this script just falls
# off the end in a fresh shell.
exit 0
