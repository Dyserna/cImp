#!/usr/bin/env bash
# V20 spike 0b — Claude out-of-band assistant text via transcript tail.
#
# Tails the ACTIVE Claude transcript JSONL for a project and emits each NEW
# assistant `text` block (skipping `thinking`/`tool_use`), de-duped by message
# id, with a wall-clock timestamp. Run it, then drive a real `claude` session in
# that project and eyeball the gap between text appearing on Claude's screen and
# here. That gap is the TTS latency the fullscreen-only design accepts.
#
# PASS: assistant text blocks surface here within ~1s of appearing on screen,
#       and no `thinking` text leaks.
#
# Usage:  ./0b_claude_transcript_tail.sh [PROJECT_SLUG]
#   PROJECT_SLUG defaults to this repo's slug. The active file = newest *.jsonl
#   in ~/.claude/projects/<slug>/ (re-resolved as files rotate).
#
# Requires: python. (Pure-Python tail so it handles Windows file locking.)
set -uo pipefail

SLUG="${1:-P--Documents-AI-private-cc-avatar-cctts}"
ROOT="$HOME/.claude/projects/$SLUG"
[ -d "$ROOT" ] || { echo "[0b] no project dir: $ROOT"; exit 1; }
echo "[0b] watching newest *.jsonl under: $ROOT"
echo "[0b] (Ctrl+C to stop)"

ROOT="$ROOT" python - <<'PY'
import os, time, json, glob, sys

root = os.environ["ROOT"]
seen_msg = set()          # de-dup by message id
offsets = {}              # path -> byte offset already read

def newest():
    files = glob.glob(os.path.join(root, "*.jsonl"))
    return max(files, key=os.path.getmtime) if files else None

def emit_assistant(obj):
    if obj.get("type") != "assistant":
        return
    msg = obj.get("message", {})
    mid = msg.get("id")
    for part in msg.get("content", []) or []:
        if part.get("type") == "text":
            text = (part.get("text") or "").strip()
            if not text:
                continue
            key = (mid, text[:40])
            if key in seen_msg:
                continue
            seen_msg.add(key)
            ts = time.strftime("%H:%M:%S")
            preview = text.replace("\n", " ")
            print(f"[{ts}] ASSISTANT  {preview[:200]}", flush=True)
        # thinking / tool_use intentionally skipped

cur = None
while True:
    nf = newest()
    if nf and nf != cur:
        cur = nf
        offsets.setdefault(cur, 0)
        print(f"[0b] active file -> {os.path.basename(cur)}", flush=True)
    if cur:
        try:
            with open(cur, "r", encoding="utf-8", errors="ignore") as fh:
                fh.seek(offsets[cur])
                # readline() (not `for line in fh`) so tell() stays enabled —
                # iterating a file object buffers ahead and disables tell().
                while True:
                    line = fh.readline()
                    if not line:
                        break              # caught up to EOF
                    if not line.endswith("\n"):
                        break              # partial line; leave offset, retry later
                    offsets[cur] = fh.tell()
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        emit_assistant(json.loads(line))
                    except Exception:
                        pass
        except FileNotFoundError:
            cur = None
    time.sleep(0.2)
PY
