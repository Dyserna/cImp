# Milestone V20: Fullscreen-only AI tabs + out-of-band TTS (unified)

> **Schema:** bumps `CURRENT_SCHEMA_VERSION` 19 → 20. The V-number tracks the
> schema version (V14 stamped 14, …, V19 stamped 19), so this is V20 and its
> migration is `migrate_v19_to_v20`. The bump strips any stored `--mini` arg and
> drops the now-dead `speak_all` setting; see Phase F. (Copy-on-select stays —
> see the interaction-layer note below.)

> **DIRECTION CHOSEN: fullscreen-only.** Earlier drafts kept inline as the
> default with fullscreen opt-in (two render paths). That is **superseded**.
> cImp drives **every** AI tab in the app's native **fullscreen (alternate-
> screen) TUI** — no inline mode, no `--mini`, no `CLAUDE_CODE_DISABLE_ALTERNATE_
> SCREEN`. One render behavior, one interaction layer, one TTS abstraction. This
> deletes the scrape pipeline at the cost of staking **all** automatic TTS on two
> out-of-band sources — both now **proven** in Phase 0 (the gate).

> **Scope boundary — shell tabs are unaffected.** Only the AI tabs (Claude /
> OpenCode) change. Shell tabs keep cImp's terminal features unchanged. This is
> why copy-on-select and right-click paste are **kept** (a plain shell does not
> implement them — the terminal does); they only become *additionally* redundant
> inside a fullscreen AI tab, where they coexist with the app's own handling.

## Purpose & decision

OpenCode's full command palette (`/connect`, etc.) only exists in its fullscreen
TUI; its inline `--mini` mode is a feature-reduced interface. To give OpenCode
full usability inside cImp we must run it fullscreen. Rather than maintain two
render paths (inline Claude + fullscreen OpenCode), this milestone unifies on
**fullscreen for both** and re-bases AI-tab TTS on structured out-of-band sources
instead of screen-scraping the terminal.

**Does this unify behavior more than the opt-in design? Yes, materially:**
- One render mode for all AI tabs — no `render_mode` branch, no per-buffer-type
  logic, no two-knob force-inline table.
- The scrape pipeline's TTS role (`processing/screen.rs` cell model,
  `processing/tags.rs` marker stripping, inline permission matching) is
  **deleted**, not maintained behind an abstraction.
- TTS becomes one shape — "subscribe to the tool's structured output" — with a
  thin per-tool adapter. The downstream segmenter/synthesizer is unchanged.
- The `[[TTS]]` marker convention + runtime system-prompt injection can be
  **retired for AI tabs** (out-of-band text needs no markers); see Phase C.

**The bet (now de-risked).** Fullscreen-only means there is no scrape fallback:
if a tool's out-of-band source fails, that tool has no automatic TTS. Phase 0
proved both sources before any deletion — see status below.

## Verified facts (probed 2026-06-30 against installed binaries)

| Concern | Finding |
|---|---|
| OpenCode full palette | Lives only in the fullscreen TUI; `--mini` is reduced. No documented "full TUI inline" flag. |
| Force-inline knobs (to remove) | Claude `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` (`config.rs:407`); OpenCode `--mini` (`config.rs:253`). |
| Frontend renderer | xterm.js, raw PTY bytes written verbatim; renders the alternate screen natively; alt-screen already detected at `terminals.ts:651`. |
| Claude out-of-band source | **Confirmed + working:** live transcript JSONL `~/.claude/projects/<slug>/<id>.jsonl`, appended in real time; assistant `message.content[]` = `thinking`+`text`. **Block-level** (written complete at message finish), **sub-second** lag in practice. |
| OpenCode out-of-band source | **Confirmed + working:** `opencode serve --port N` → SSE at `GET /event`; a prompt streams back as **`message.part.delta`** (token-level) + `text` parts, with `reasoning` events separable. `GET /api/session/{id}/permission` also exists. |
| Copy-on-select | **Kept.** Shell tabs need cImp's `onSelectionChange` copy (`terminals.ts:501`) — a shell doesn't self-copy. In fullscreen AI tabs it coexists with the app's OSC 52 copy (different gestures: Shift-drag → cImp; plain drag → app), so no double-copy. |
| Right-click paste | **Kept.** Pasting into a terminal app is the terminal's job (bracketed paste); a shell can't self-paste. Only the right-click *gesture* may collide with the app's mouse tracking in a fullscreen AI tab — adapt there, don't remove. |
| Permission detection (to re-source) | Inline string match (`processing/permission.rs`) dies with the scrape path. Replace via Claude's **Notification hook** and OpenCode's `/event` + `/permission` (Phase E). |

## Phases

### Phase 0 — Out-of-band spikes (THE GATE) — STATUS: PASSED

Runnable suite in **`docs/spikes/v20/`**.

0a. **OpenCode event stream** — `0a_opencode_event.sh`. **PART 1 PASSED:** drove a
   prompt over the API (free model, no creds); reply streamed on `GET /event` as
   `message.part.delta` (token-level) + `text`, `reasoning` separable → OpenCode
   out-of-band TTS is **real-time streaming**. **PART 2 (low-risk, manual):**
   confirm the same with a real **TUI attached** (`opencode attach`). The event
   stream is server-wide, so this is expected to pass as a formality.
0b. **Claude transcript tail** — `0b_claude_transcript_tail.sh`. **PASSED:** parser
   extracts assistant `text`, skips `thinking`; written complete at message
   finish (block-level); observed Claude-tab → tail gap is **sub-second** — well
   within TTS comfort.
0c. **OSC 52 clipboard** — `0c_osc52_clipboard.sh`. **Informational, not a gate.**
   Since copy-on-select stays, this only tells us whether the app *also* copies on
   plain-drag in fullscreen (so we know both gestures put text on the clipboard).
   Verify xterm honors an OSC 52 write (CHECK 1) and whether the apps emit it
   (CHECK 2).
0d. **Select-text TTS under fullscreen** — `0d_select_tts.md` (manual). Keep-vs-drop
   for **Ctrl+right-click speak-selection**: does Shift-drag yield a local
   selection under mouse tracking, and does the gesture still speak? Owner will
   drop it if fiddly.

**Gate verdict: 0a Part 1 + 0b passed → fullscreen-only is GO.** 0a Part 2 is a
formality; 0c/0d are keep-vs-drop, not gates. If anything regresses, the fallback
is the opt-in design (that tool stays inline) rather than a mute tab.

### Phase A — Fullscreen launch (remove the force-inline knobs) — STATUS: DONE

1. ✅ Deleted the `--mini` prepend (`build_extra_args`) and the
   `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN` env (`compose_ai_env`). Every AI tab
   now starts in its native fullscreen TUI. Kept the OpenCode
   `OPENCODE_CONFIG_CONTENT` injection and noise-suppression env (still needed).
   Tests rewritten (`opencode_launches_without_mini`, `no_mini_for_any_ai_tab`,
   `claude_launches_fullscreen_by_default`, `no_ai_tab_forces_inline_renderer`,
   `per_tab_env_can_reenable_inline_renderer`); all 25 `tabs::config` tests green.
2. Strip any stored `--mini` from existing tab `args` (Phase F migration). _(pending)_

### Phase B — Interaction layer (fullscreen AI tabs; shell tabs untouched)

3. **Mouse belongs to the app.** With mouse tracking on, drags/clicks route to the
   TUI. Accept this — it's how every terminal hosts a fullscreen app.
4. **Copy-on-select stays** (`terminals.ts:501`). No removal, no setting change.
   Verify it still fires on a **Shift-drag** local selection under mouse tracking
   (the only way to make a local selection when the app owns the mouse). In
   fullscreen AI tabs the app's own plain-drag copy (OSC 52) coexists; confirm no
   double-copy of the *same* gesture (there isn't — different triggers).
5. **Right-click paste stays.** Shell tabs need it. In a fullscreen AI tab the
   plain right-click may also be consumed by the app's mouse tracking; if that
   double-acts, gate cImp's paste behind a modifier (**Shift+right-click**) *in
   mouse-tracking mode only*, leaving shell-tab behavior exactly as today.
   Keyboard paste (Ctrl+Shift+V) remains the always-works path.
6. **Speak-on-select (Ctrl+right-click → `selectionTts.ts`) is OPTIONAL.** Owner
   will sacrifice it. It needs a local (Shift-drag) selection plus the
   `contextmenu`/marker math holding on the alt buffer. If either is fiddly,
   **remove the gesture** — auto out-of-band TTS is the primary path. Keep it only
   if Shift-selection works for free (Phase 0d decides).
7. **Scrollback** is the app's responsibility in fullscreen; document that cImp's
   inline scrollback no longer applies to AI tabs (shell tabs keep theirs).

### Phase C — TTS via out-of-band sources only (delete the scrape path)

8. **One `TtsSource` shape:** subscribe → stream of assistant text → existing
   `Segmenter` → synthesizer. No cell model, no marker stripping.
9. **Delete** the TTS role of `processing/screen.rs` + `processing/tags.rs` and
   the inline `[[TTS]]` marker convention for AI tabs. Speak prose directly; use
   the transcript/event **structure** (not markers) to skip code/tool blocks.
   *Decision needed:* default to speak-all-prose, or keep opt-in markup for "don't
   speak this." (Recommend: speak-all-prose with a per-tab off switch.) *Finding
   (0b):* the transcript carries any `[[TTS]]` markers **verbatim**, so
   marker-gating stays *available* out-of-band for Claude if we keep the prompt
   injection — the choice is open, not forced. If we go speak-all-prose, the
   reader strips stray markers.
9b. **Remove TTS-all-output (`speak_all`) entirely.** Owner has never used it. It
   rode the scrape path (`ProcessingLayer.speak_all`, `TagScanner::scan_all`) and
   "speak every raw line" doesn't map onto out-of-band sources (assistant messages
   only). Delete the field, the scanner mode, and the setting (Phase F).
10. **Retire runtime prompt injection** of the TTS markup for AI tabs
    (`--append-system-prompt` / OpenCode `instructions` TTS block) if we go
    speak-all-prose. Offload/graph guidance injection is unaffected.

### Phase D — Per-tool adapters (now unblocked — Phase 0 passed)

11. **OpenCode adapter** (`oob_opencode.rs`): subscribe to `GET /event`, filter
    `message.part.delta`/`text` (skip `reasoning`); lifecycle
    (start/health/teardown/reconnect) reusing warm-pool patterns; event→sentence
    bridge honoring the per-tab TTS-enabled gate.
12. **Claude adapter** (`oob_claude_transcript.rs`): resolve the active session
    JSONL for the tab's cwd (newest `*.jsonl` under the project slug), tail it
    (readline-based so `tell()` stays enabled), emit assistant `text` blocks (skip
    `thinking`), handle rotation/compaction/locking, de-dup on message id.

### Phase E — Re-source permission detection / notifications

13. **Claude:** replace inline permission scraping with Claude Code's
    **Notification hook** (fires on permission prompts), wired via the managed
    settings cImp already injects. Verify the hook payload identifies the prompt.
14. **OpenCode:** derive permission state from `GET /api/session/{id}/permission`
    and/or the `/event` stream (Phase D.11); reply via the documented
    `permission/{id}/reply` endpoint or let the user answer the native TUI prompt.

### Phase F — Schema migration v19 → v20 + settings cleanup

15. `migrate_v19_to_v20` (stamp literal 20): strip `--mini` from AI-tab `args`;
    drop the `speak_all` / TTS-all-output setting; preserve everything else
    (**copy-on-select setting is retained**); back up `config.json.v19.bak.<ts>`.
    Cascade tests (v1.3→v20 backup count; v19→v20 strips `--mini`, drops
    `speak_all`, keeps `copy_on_select`, touches nothing else).

## The simplification dividend (what gets deleted)

- `processing/screen.rs` + `processing/tags.rs` TTS roles + the marker stripper —
  the largest single chunk.
- Inline permission matching in `processing/permission.rs` (replaced by hooks/
  events).
- Both force-inline knobs and any `render_mode`/two-path logic.
- TTS-all-output (`speak_all`) — field, scanner mode, and setting.
- Speak-on-select (Ctrl+right-click) — only if the Shift bypass isn't free.
- The `[[TTS]]` marker convention + its runtime prompt injection for AI tabs.

**Kept (not deleted):** copy-on-select and right-click paste — shell tabs depend
on them.

Net: fewer moving parts than today, at the price of a hard dependency on the two
(now proven) out-of-band sources.

## What This Milestone Does NOT Do

- **Keep any inline path for AI tabs.** Fullscreen-only is the point. Shell tabs
  are untouched (they were never inline-forced).
- **Remove copy-on-select or right-click paste.** Both stay for shell tabs; AI
  tabs simply also get the app's native handling.
- **Proceed past Phase 0 on faith.** Gate passed; the irreversible deletions
  (Phase C) still wait on 0a Part 2 being closed.
- **Token-stream Claude TTS.** The transcript is block-level (sub-second, deemed
  acceptable). Token-level Claude TTS is a followup if Anthropic ever exposes it.

## Files Most Likely Touched

- `src-tauri/src/tabs/config.rs` — remove `--mini` / alt-screen knobs.
- `src-tauri/src/processing/` — delete the scrape/marker TTS role + inline
  permission match; introduce the `TtsSource` subscription shape.
- new `src-tauri/src/.../oob_opencode.rs`, `oob_claude_transcript.rs`.
- `src/lib/terminals.ts` — **keep** copy-on-select + right-click paste; verify
  Shift-selection under mouse tracking; adapt right-click paste only if it
  double-acts in fullscreen AI tabs.
- `src/lib/selectionTts.ts` — validate the optional speak-on-select on the alt
  buffer (or remove it per 0d).
- `src-tauri/src/processing/permission.rs` → hook/event-sourced.
- `src-tauri/src/settings/{schema.rs,migration.rs}` — `CURRENT_SCHEMA_VERSION=20`,
  `migrate_v19_to_v20`, drop `speak_all` (keep `copy_on_select`).
- Settings UI: drop the TTS-all-output toggle; note fullscreen behavior. Keep the
  copy-on-select toggle.

## Risks and Open Questions

- **Concentration risk (mitigated).** All auto-TTS rides two out-of-band sources
  with no scrape fallback — both now proven in Phase 0. Residual: 0a Part 2 (TUI
  attached) still to confirm before the Phase C deletions.
- **Claude block-level latency (resolved).** Sub-second in practice; acceptable.
  Revisit only if real-world sessions feel worse than the spike.
- **OpenCode TUI-attached stream (0a Part 2).** Expected to pass (server-wide
  stream), but confirm before deleting the scrape path.
- **Right-click double-action in fullscreen AI tabs.** If both cImp and the app
  act on a plain right-click, gate cImp's paste behind Shift in mouse-tracking
  mode (B.5). Shell tabs unaffected.
- **Shift-selection sufficiency.** Copy-on-select and the optional speak-on-select
  need a local selection, which under mouse tracking requires the Shift bypass.
  Verify it works across mouse modes (1006 SGR); if not, copy-on-select still
  works for shell tabs and the speak gesture is expendable.
- **Permission via hooks.** Claude's Notification hook must reliably identify
  permission prompts; OpenCode exposes a `/permission` endpoint, so it's better
  placed than Claude here.

## Followups Tracked Elsewhere

- **Token-streamed Claude TTS** if Anthropic exposes an incremental assistant
  stream beyond the transcript file.
- **Upstream OpenCode "full TUI inline" mode** — would make fullscreen optional,
  but the out-of-band design is preferable regardless.
- **Generalized agent-protocol TTS layer** if a third tool ever needs an adapter.
