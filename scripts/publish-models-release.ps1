#!/usr/bin/env pwsh
# Maintainer tool: publish the local models/ blobs as assets on the models
# GitHub release (the source scripts/fetch-models.ps1 and the release
# workflow download from).
#
# Verifies every file against models/CHECKSUMS.txt first, then creates the
# release if it doesn't exist and uploads every file CHECKSUMS.txt names
# (--clobber, so re-running replaces assets). Voicepacks upload under their
# basename (af_*.bin); fetch-models re-derives the voices/ subdir from
# CHECKSUMS.txt.
#
# To ship a new or updated model:
#   1. Drop the file into models/ and add/refresh its line in CHECKSUMS.txt.
#   2. For a changed EXISTING file, bump the tag (models-v2, ...) here AND in
#      fetch-models.ps1 / fetch-models.sh — old app tags keep fetching the
#      exact bytes they were released against. Purely ADDITIVE files can
#      reuse the current tag.
#   3. Run: pwsh scripts/publish-models-release.ps1
#
# Requires an authenticated `gh` CLI.

[CmdletBinding()]
param(
  [string]$Repo = 'Dyserna/cImp',
  [string]$ReleaseTag = 'models-v1',
  [string]$Target = 'main'
)

$ErrorActionPreference = 'Stop'

& (Join-Path $PSScriptRoot 'fetch-models.ps1') -VerifyOnly
if ($LASTEXITCODE -ne 0) {
  throw 'models/ does not match models/CHECKSUMS.txt — fix that before publishing'
}

$modelsDir = Join-Path (Split-Path -Parent $PSScriptRoot) 'models'
$files = Get-Content (Join-Path $modelsDir 'CHECKSUMS.txt') |
  Where-Object { $_ -and $_ -notmatch '^\s*#' } |
  ForEach-Object {
    $parts = $_.Trim() -split '\s+', 2
    if ($parts.Count -ge 2) { Join-Path $modelsDir $parts[1].Trim() }
  }

gh release view $ReleaseTag --repo $Repo *> $null
if ($LASTEXITCODE -ne 0) {
  Write-Host "creating release $ReleaseTag"
  $notes = @"
Permanent hosting for the model blobs the app bundles, moved out of Git LFS
(CI pulls were burning the LFS bandwidth quota; release-asset downloads are
unmetered).

Contents (SHA-256s in ``models/CHECKSUMS.txt`` in the repo):
- ``kokoro-v1.0.onnx`` — Kokoro TTS model (Apache-2.0), from
  https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX (onnx/model.onnx, renamed)
- ``af_*.bin`` — Kokoro voicepacks (land in ``models/voices/``), same source
- ``ggml-small.bin`` — Whisper STT model (MIT), from
  https://huggingface.co/ggerganov/whisper.cpp

Consumed by the release workflow and ``scripts/fetch-models.ps1`` / ``.sh``.
Not an app release — see the ``vX.Y.Z`` releases for those.
"@
  gh release create $ReleaseTag --repo $Repo --target $Target --latest=false `
    --title 'Model assets v1 (Kokoro TTS + Whisper STT)' --notes $notes
  if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }
}

Write-Host "uploading $($files.Count) assets to $ReleaseTag"
gh release upload $ReleaseTag --repo $Repo --clobber @files
if ($LASTEXITCODE -ne 0) { throw "gh release upload failed" }
Write-Host 'done'
