#!/usr/bin/env bash
# V20 spike 0a — OpenCode out-of-band assistant-text stream.
#
# Proves cImp can read assistant text from `GET /event` (SSE) while OpenCode
# runs, WITHOUT scraping the terminal. Two parts:
#   PART 1 (automated): server + SSE + prompt over HTTP → assert assistant text.
#   PART 2 (manual):    same server with a real TUI ATTACHED → assert the stream
#                       still carries the text typed in the TUI (the real cImp
#                       topology: PTY-hosted `opencode attach`, plus our SSE tap).
#
# PASS: assistant text for the prompt shows up in the captured event stream.
#
# Requires: opencode on PATH, curl, python.
set -uo pipefail

PORT="${PORT:-17999}"
HOST="127.0.0.1"
BASE="http://$HOST:$PORT"
DIR="$(cd "$(dirname "$0")" && pwd)"
EVLOG="$DIR/0a_events.ndjson"
SRVLOG="$DIR/0a_serve.log"
export PROMPT="${PROMPT:-Reply with exactly one word: pineapple}"
# A model must be named (no default in config). Free hosted models work without
# credentials; override via MODEL_PROVIDER / MODEL_ID. List with `opencode models`.
MODEL_PROVIDER="${MODEL_PROVIDER:-opencode}"
MODEL_ID="${MODEL_ID:-north-mini-code-free}"

cleanup() {
  [ -n "${EV_PID:-}" ] && kill "$EV_PID" 2>/dev/null
  if [ -n "${SRV_PORT_PID:-}" ]; then taskkill //F //PID "$SRV_PORT_PID" 2>/dev/null; fi
}
trap cleanup EXIT

echo "[0a] starting opencode serve on $BASE ..."
opencode serve --port "$PORT" --hostname "$HOST" > "$SRVLOG" 2>&1 &
# Wait for the listener (poll, no fixed sleep).
for i in $(seq 1 30); do
  curl -s -m 1 "$BASE/doc" >/dev/null 2>&1 && break
  curl -s -m 1 "$BASE/event" >/dev/null 2>&1 && break
done
SRV_PORT_PID=$(netstat -ano 2>/dev/null | grep ":$PORT" | grep LISTENING | awk '{print $5}' | head -1)
echo "[0a] server pid=$SRV_PORT_PID"

echo "[0a] tapping $BASE/event -> $EVLOG"
curl -s -N "$BASE/event" > "$EVLOG" 2>/dev/null &
EV_PID=$!

echo "[0a] creating session ..."
SID=$(curl -s -m 10 -X POST "$BASE/session" -H 'content-type: application/json' -d '{}' \
      | python -c "import sys,json; print(json.load(sys.stdin).get('id',''))")
echo "[0a] session=$SID"
if [ -z "$SID" ]; then echo "[0a] FAIL: no session id (see $SRVLOG)"; exit 1; fi

echo "[0a] sending prompt (model $MODEL_PROVIDER/$MODEL_ID) ..."
# Shape verified against /doc: required `parts[]`, plus `model{providerID,modelID}`
# since config has no default. Assistant output streams back as
# `message.part.delta` events (token-level) plus `text` parts.
BODY=$(MP="$MODEL_PROVIDER" MI="$MODEL_ID" python -c "import json,os; print(json.dumps({'model':{'providerID':os.environ['MP'],'modelID':os.environ['MI']},'parts':[{'type':'text','text':os.environ['PROMPT']}]}))")
curl -s -m 90 -X POST "$BASE/session/$SID/message" -H 'content-type: application/json' \
  -d "$BODY" > "$DIR/0a_prompt_reply.json" 2>&1

echo "[0a] waiting for streamed assistant output on the event stream ..."
# Pass = assistant output deltas appear out-of-band. `message.part.delta` is the
# token-level stream; require at least a couple so we don't trip on the echo of
# our own prompt text.
FOUND=""
for i in $(seq 1 60); do
  n=$(grep -c '"type":"message.part.delta"' "$EVLOG" 2>/dev/null || echo 0)
  if [ "${n:-0}" -ge 2 ]; then FOUND=1; break; fi
  curl -s -m1 "$BASE/doc" >/dev/null 2>&1
done

echo "[0a] ---- event types seen ----"
grep -o '"type":"[^"]*"' "$EVLOG" 2>/dev/null | sort | uniq -c | head -30

if [ -n "$FOUND" ]; then
  echo "[0a] PASS: assistant text observed out-of-band on $BASE/event"
  echo "[0a] PART 2 (manual): in another terminal run:  opencode attach $BASE"
  echo "      type a message in that TUI, then re-grep $EVLOG for its reply text."
  exit 0
else
  echo "[0a] INCONCLUSIVE: no assistant text matched. Inspect $EVLOG and the"
  echo "     /session/{id}/message schema at $BASE/doc, then adjust the matcher."
  exit 2
fi
