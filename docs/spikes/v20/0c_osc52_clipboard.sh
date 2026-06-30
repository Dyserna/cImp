#!/usr/bin/env bash
# V20 spike 0c — OSC 52 clipboard handoff.
#
# If we remove cImp's copy-on-select (Phase B.4), the apps must put selections
# on the SYSTEM clipboard themselves. A fullscreen TUI can only do that via the
# OSC 52 escape, which xterm.js must be configured to honor. Two checks:
#
#   CHECK 1 (xterm honors OSC 52 write) — RUN THIS INSIDE A cImp SHELL TAB:
#       bash 0c_osc52_clipboard.sh inject
#     then verify the OS clipboard now contains the marker string. If it does,
#     cImp's xterm honors OSC 52 writes.  (Running it in a non-cImp terminal
#     tests THAT terminal, not cImp — so do it inside cImp.)
#
#   CHECK 2 (the apps EMIT OSC 52 on selection) — manual:
#     Run `claude` / `opencode` FULLSCREEN inside cImp, select text with the
#     mouse, and check the OS clipboard. If it updates, the app copies on select
#     via OSC 52 and cImp's own handler is redundant. If it does NOT, keep
#     cImp's copy-on-select. (Optionally capture the raw PTY bytes and grep for
#     the `]52;` sequence to confirm emission directly.)
#
# PASS (to remove cImp copy-on-select): both checks succeed for both apps.
set -uo pipefail

MARK="${MARK:-cimp-osc52-$$}"
b64() { printf '%s' "$1" | base64 | tr -d '\n'; }

case "${1:-inject}" in
  inject)
    payload="$(b64 "$MARK")"
    # OSC 52 ; c (clipboard) ; <base64> ST(BEL)
    printf '\033]52;c;%s\007' "$payload"
    echo ""
    echo "[0c] emitted OSC 52 set-clipboard with marker: $MARK"
    echo "[0c] now verify the OS clipboard:"
    echo "      powershell -NoProfile -Command Get-Clipboard"
    echo "[0c] PASS if it prints: $MARK"
    ;;
  check)
    got="$(powershell -NoProfile -Command Get-Clipboard 2>/dev/null | tr -d '\r')"
    echo "[0c] clipboard = '$got'"
    ;;
  *)
    echo "usage: $0 [inject|check]"; exit 2;;
esac
