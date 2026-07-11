#!/usr/bin/env bash
# Bash twin of scripts/fetch-models.ps1 for Linux/WSL dev environments —
# see that script's header for the full story. Fetches + verifies the
# bundled model blobs (Kokoro TTS + voicepacks + Whisper STT) against
# models/CHECKSUMS.txt, downloading missing/corrupt files from the
# `models-v1` GitHub release.
set -euo pipefail

REPO="${MODELS_REPO:-Dyserna/cImp}"
TAG="${MODELS_RELEASE_TAG:-models-v1}"
DIR="$(cd "$(dirname "$0")/.." && pwd)/models"
CHECKSUMS="$DIR/CHECKSUMS.txt"
[ -f "$CHECKSUMS" ] || { echo "not found: $CHECKSUMS" >&2; exit 1; }

verify() { # verify <expected-sha> <path>
  [ -f "$2" ] && echo "$1  $2" | sha256sum -c --status - 2>/dev/null
}

fail=0
while read -r expected rel; do
  case "$expected" in ''|\#*) continue ;; esac
  path="$DIR/$rel"
  if verify "$expected" "$path"; then
    echo "ok       $rel"
    continue
  fi
  mkdir -p "$(dirname "$path")"
  echo "fetching $rel"
  curl -L --fail --retry 3 --silent --show-error -o "$path.download" \
    "https://github.com/$REPO/releases/download/$TAG/$(basename "$rel")"
  mv -f "$path.download" "$path"
  if verify "$expected" "$path"; then
    echo "fetched  $rel"
  else
    echo "FAILED   $rel (sha256 mismatch)" >&2
    fail=1
  fi
done < <(tr -d '\r' < "$CHECKSUMS")   # tr: CHECKSUMS.txt may be CRLF
exit $fail
