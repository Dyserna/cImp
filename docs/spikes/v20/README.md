# V20 Phase 0 — out-of-band TTS gate (runnable spikes)

These four spikes decide whether the **fullscreen-only** V20 design is viable.
**0a and 0b must both pass** before any scrape-path deletion. 0c and 0d are
keep-vs-drop decisions, not gates.

| Spike | Question | Pass criterion | Script |
|---|---|---|---|
| 0a | Can we stream OpenCode assistant text out-of-band, concurrently with a TUI? | Assistant text appears on `GET /event` while a prompt runs (and while a TUI is attached). | `0a_opencode_event.sh` |
| 0b | Can we tail Claude's transcript for assistant text, with acceptable latency? | Each assistant `text` block surfaces within ~1s of appearing on screen. | `0b_claude_transcript_tail.sh` |
| 0c | (Informational) Do the apps ALSO copy selections via OSC 52, and does xterm honor it? | Injected OSC 52 updates the OS clipboard; note whether in-app plain-drag does too. | `0c_osc52_clipboard.sh` |
| 0d | Does Shift-drag local selection + speak-on-select work under mouse tracking? | Shift-drag yields a non-empty `getSelection()`; Ctrl+right-click speaks it. | `0d_select_tts.md` (manual) |

## Spike results (2026-06-30)

- **0a Part 1 — PASS.** Drove a prompt over the HTTP API (free model
  `opencode/north-mini-code-free`, no credentials needed) and the assistant
  reply streamed back on `GET /event` as **`message.part.delta`** events
  (token-level) plus `text` parts — and separate `reasoning` events we can skip.
  So OpenCode out-of-band TTS is **real-time streaming**, not block-batched.
  *Remaining:* Part 2, confirm the same with a TUI **attached** (`opencode attach`).
- **0b — PASS (mechanism + latency).** Parser extracts assistant `text` blocks
  and skips `thinking` from the live JSONL. Text is written **complete at message
  finish** (block-level). *Latency confirmed (2026-06-30): sub-second* between
  text appearing in the Claude tab and in the tail (owner observation) — well
  within TTS comfort. Claude clears the gate for fullscreen-only.

## Findings already established (2026-06-30, against installed binaries)

- **OpenCode server is real and documented.** `opencode serve --port N` →
  OpenAPI at `/doc`; SSE at `GET /event` (`Content-Type: text/event-stream`,
  emits `{"id","type","properties"}` events, first event `server.connected`).
  Message + permission + prompt endpoints exist
  (`POST /session/{id}/message`, `GET /api/session/{id}/permission`). So
  OpenCode permission detection can ALSO come from the stream (Phase E.14), not
  just be dropped. **0a Part 1 passed (see above); only the attached-TUI
  confirmation remains.**
- **Claude transcript is real and live-appending.**
  `~/.claude/projects/<slug>/<id>.jsonl`; assistant lines are
  `{"type":"assistant","message":{"content":[{"type":"text","text":...},
  {"type":"thinking",...}]}}`. The text block is written **complete, at message
  finish** (observed with `stop_reason:"tool_use"`), i.e. **block-level after
  completion, not token-streamed.** Latency measured sub-second (0b passed).

Run from this directory in Git Bash. `curl` and `python` are required and
present on the dev machine.
