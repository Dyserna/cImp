# V14 — Workflow & Visibility (token X-ray · tuning advisor · preview tab · prompt library)

**Status:** SPEC (written 2026-07-08). Not yet coded.
**Builds on:** V20 OOB transcript tail (`src-tauri/src/oob/` — the JSONL stream
already carries per-message `usage`), the Offload Server dashboard tab pattern,
the compose overlay, the reserved-tab pattern, V10 Context section's
tokens-injected counter.

## Why

Four quality-of-life features that round out the agentic workflow. They share
one theme — **see what's happening, and feed the loop faster**:

1. **Token/cost X-ray** — V10/V11 add knobs (budgets, dedup, digests) whose
   entire point is token savings, but nothing today shows where a session's
   tokens actually go. You can't tune budgets you can't see.
2. **Budget-tuning advisor** — and once you *can* see, the tuning itself
   shouldn't be homework: the X-ray data drives measured, propose-and-confirm
   suggestions for the V10/V11 knobs, turning the dashboard from a report
   into a controller.
3. **Preview tab + images into compose** — for web-dev vibe coding the missing
   verify loop is *seeing the app*. And even outside web dev, pasting a
   screenshot into the compose overlay is table stakes for a 2026 agent
   frontend.
4. **Prompt library** — the same prompts get retyped daily; templates with
   variables are trivial to build on the compose overlay and pay off
   immediately.

---

## Feature 1 — Token/cost X-ray

### Goal
Per-tab / per-session / per-project token accounting, broken down by *what
consumed them* — turns, tool results, injected context — plus the offload
local-vs-cloud split. Honest measurement (from transcript `usage` fields where
present, labeled estimates elsewhere), no fabricated "savings %".

### Data source (no new interception)
- The OOB tail already parses every transcript JSONL line for TTS and memory.
  Extend the tap to also extract, per assistant message: `usage`
  (`input_tokens`, `output_tokens`, `cache_read_input_tokens`,
  `cache_creation_input_tokens`) and, per `tool_result`, the result size
  (chars → estimated tokens, labeled est.). Claude transcripts carry `usage`
  verbatim; OpenCode's `/event` stream is checked for an equivalent
  (spike — if absent, OpenCode rows show est.-only).
- Aggregate in a new `usage_stat` relation in the per-project `graph.db`
  (session_id, turn, tokens by class, top tool consumers), ring-bounded like
  `mem_event`.
- Join with what cImp already knows: tokens injected per turn (V10 counter),
  dedup-suppressed chars (V11), `offload_task` calls that ran locally (the
  offload metrics already exist in the dashboard).

### UI — new **Usage** section in the Code Intelligence tab
(It's session/project analytics over the same stores — it belongs with
Activity/Memory/Context, not in a new tab.)
- **This session:** stacked per-turn bars — input / cache-read / output /
  est. tool-result share; a "top consumers" table (tool name × est. tokens),
  which is the actionable view ("Read of `foo.rs` cost 18k twice").
- **Project totals:** per-session rows with totals + cache-hit ratio; the
  offload split ("N tasks served locally").
- **Effectiveness panel:** injected vs. suppressed-by-dedup vs. read-advisor
  displacement (V11 Activity events) — measured chars, est. tokens, honest
  labels.
- Status-bar: the existing usage meter is *rate-limit* oriented; add an
  optional per-session token counter to its popover, not a new widget.

### Edge cases
- Estimates are estimates: every derived number carries the `est.` label in
  the UI; only transcript `usage` fields are shown as exact.
- Cost-in-currency is out of scope (subscription plans make $/token
  meaningless for most users); tokens only.

---

## Feature 1b — Budget-tuning advisor (closing the loop)

### Goal
The X-ray collects exactly the data the V10/V11 knobs need for tuning, but
sliders leave the tuning to the user. The advisor proposes measured changes —
**propose-and-confirm, never silent self-modification** (a setting that
changes itself erodes the trust the honest-accounting posture builds).

### Design
- **Deterministic heuristics, not a black box:** rules computed in Rust over
  `usage_stat` + the V10/V11 effectiveness stores. V1 rules:
  - injected files that were never subsequently read/edited in-session at a
    high rate ⇒ propose raising `context_min_score`;
  - read-advisor reminders followed by an immediate full re-read at a high
    rate ⇒ propose raising `read_advisor_min_lines` (the reminders fire on
    files the agent genuinely needs whole);
  - injected-but-unread rate high *while* budgets are maxed ⇒ propose
    lowering `context_turn_budget_chars`.
  Only rules with a clear causal story ship; no speculative correlations.
- Each proposal carries: the setting, current → proposed value, the measured
  rationale, and an **Apply** button that writes through the normal settings
  path (visible in Settings, undoable, migration-safe).
- Optional local-model narrative (V11's `run_internal`) phrasing a summary;
  the *numbers* always come from the heuristics.

### UI
An **Advisor** card at the top of the Usage section: current proposals with
Apply/Dismiss (or "no changes suggested — data looks healthy"). Dismissed
proposals don't re-fire until the underlying rate changes materially.

### Edge cases
- **Cold start:** rules need a minimum sample (≥ 5 sessions / ≥ 200
  injections) before proposing; below that the card says it's collecting.
- **Mixed harnesses:** OpenCode sessions may be est-only (no exact `usage` —
  Feature 1's C3 spike). Token-based rules aggregate over exact-usage
  sessions only, rather than blending estimates into measured rates;
  behavior-based rules (injected-then-never-touched, remind-then-reread) use
  all sessions since their signals don't depend on token counts.
- Rules are versioned in code and listed in the card's tooltip — inspectable,
  not magic.

---

## Feature 2 — Localhost preview tab + images into compose

### Goal
Close the visual verify loop: an embedded browser tab pointed at the dev
server, one click from "what the agent just built" to "screenshot in the
agent's context". Plus the standalone half everyone needs anyway: image paste
into the compose overlay.

### Feature 2a — image paste/drop into compose (ships first, independently)
- Paste (`Ctrl+V` with image clipboard content) or drag-drop an image file
  onto the compose overlay → saved to a session-scoped temp dir
  (`%TEMP%/cimp-attach/<session>/n.png`) → a chip appears above the textarea;
  on submit, the file path(s) are appended to the message text (Claude Code
  and OpenCode both accept local image paths in prompts).
- Uses the existing Tauri clipboard plugin (the WebView2
  `navigator.clipboard` denial is a known gotcha — same workaround as the
  AI-tab clipboard work).
- Cleanup: temp dir pruned on app exit + age-capped.

### Feature 2b — preview tab
- New user-creatable tab type **Preview** (`+` menu): a Tauri WRY child
  webview navigated to a user-entered URL (default `http://localhost:<port>`,
  remembered per project in the `.cimp/config.json` overlay). Toolbar: URL
  bar, back/reload, device-width presets (mobile/tablet/desktop), and
  **Snapshot → compose**.
- **Snapshot → compose:** capture the webview (Tauri window/webview capture
  API — D0 spike below) to PNG in the attach temp dir, open the compose
  overlay with the image chip pre-attached, targeted at the focused AI tab.
  One click from pixels to prompt.
- Auto-reload option: reload on graph-watcher quiet periods (the "agent
  finished an edit burst" signal, debounced ~1 s) so the preview tracks the
  agent's work without manual refreshes.
- **Not** a general browser: no tab history UI, no profiles, external links
  open in the system browser. Navigation is restricted to
  localhost/127.0.0.1/LAN-private hosts by default (`preview_allow_remote`
  opt-in for staging URLs) — this is a *preview* surface, and the restriction
  also keeps the embedded webview from becoming a general (and
  prompt-injectable) browsing surface next to agent tabs.

### D0 spike (gates 2b)
Verify in Tauri 2.x on WebView2: (1) a child-webview per tab coexists with
the xterm panes and the portal/drag system (reuse the AI-tab child-webview
learnings if any apply), (2) programmatic capture of *that* webview's
viewport is available (WebView2 `CapturePreviewAsync` exists; confirm wry
exposes it or add a small windows-rs call), (3) focus/keyboard isolation.
Linux parity (webkit2gtk capture) checked but not blocking — Windows-first
like the rest of the app.

**D0/E0 spike RESOLVED (2026-07-09) — gate cleared, EMBEDDED path shipped.**
No live app was available to drive this from (source-inspection + compile
verification only, same posture as the OpenCode capture-harness spike's
*type-level* half); the findings below are what compiling against this
project's exact pinned versions (Tauri 2.11.0 / wry 0.55.0, per `Cargo.lock`)
confirmed, plus what still needs a live pass:

- **Child webview:** `tauri::Window::add_child(WebviewBuilder, position, size)`
  exists and is stable-shaped in 2.11.0, gated behind the `unstable` cargo
  feature (a Tauri naming quirk, not a claim about API risk — it's the
  documented multi-webview API, with a runnable doctest in the tauri crate
  itself). `Webview::{set_bounds, hide, show, navigate, close}` cover
  open/reposition/hide-show/navigate/destroy with no further FFI. Enabled via
  `tauri = { features = ["protocol-asset", "unstable"] }`.
- **Capture:** wry does NOT expose `CapturePreviewAsync` directly, confirming
  the milestone's expectation — but `Webview::with_webview` →
  `PlatformWebview::controller()` hands back an `ICoreWebView2Controller`
  from the `webview2-com` crate, and critically **that crate is already a
  transitive dependency of wry 0.55 pinned to the exact same version
  (0.38.2)** — so depending on it directly (`webview2-com = "0.38"`) gives a
  type-IDENTICAL `ICoreWebView2Controller`, not just a COM-GUID-compatible
  one; `.CoreWebView2()` → `ICoreWebView2::CapturePreview(format, IStream,
  handler)` is the one COM call. Rather than the usual in-memory
  `IStream`+`HGLOBAL` byte-extraction dance, capture points at a
  FILE-BACKED stream (`SHCreateStreamOnFileW`) so the PNG lands on disk with
  no manual byte copying at all — see `src-tauri/src/preview/capture.rs`.
  This compiled cleanly (full workspace build, including `webview2-com`/
  `windows` 0.61/`tao`/`wry` from scratch) on the first attempt once the
  exact pinned versions were used.
- **Result:** both halves compile cleanly against this project's real
  dependency graph, so Phase F ships the EMBEDDED path end-to-end, not the
  system-browser re-scope. `docs/MAINTENANCE.md` should note the new
  Windows-only deps (`webview2-com`, `windows`) alongside the E0 findings —
  see the Phase G TODO.
- **NOT verified (needs a live pass — same posture as every other spike in
  this codebase that hit its context-budget ceiling before a live run):**
  z-order/coexistence with the xterm panes during an actual tab drag; focus/
  keyboard isolation in practice (typing in the Preview webview vs. cImp's
  global shortcuts — no hold-Alt-bypass-equivalent was added because
  nothing in the source suggested WebView2 child webviews fight the host
  window's accelerator table the way the AI-tab PTY mouse capture did, but
  this is an assumption, not a measurement); whether the captured PNG is
  actually pixel-correct (right viewport bounds, true CSS-pixel — not
  HiDPI-inflated — scale, correct timing relative to paint); and the
  `on_navigation`/`on_new_window` policy handlers' real runtime behavior
  (only their Rust-level logic and wiring were exercised, via
  `preview::is_allowed_preview_host`'s unit tests — not an actual denied
  navigation in a live webview). See the `TODO(spike E0)` comments in
  `src-tauri/src/preview/mod.rs` and `src-tauri/src/preview/capture.rs` for
  the exact call sites.
- Linux (webkit2gtk) capture: still not attempted, per the milestone's own
  non-blocking allowance — `preview::capture` compiles a clear "not
  implemented on this platform" stub for non-Windows targets.
- **KNOWN LIMITATION (code-review pass, 2026-07-09):** the navigation
  policy (`is_allowed_preview_host`, applied at `preview_open`/
  `preview_navigate`, `on_navigation`, and `on_new_window`) only polices the
  MAIN FRAME — wry exposes no subframe-navigation hook, so a policy-allowed
  page embedding `<iframe src="https://some-remote-host">` can load remote
  content inside the Preview tab without ever being checked. Acceptable for
  a localhost dev-preview surface (the threat model is "don't let the tab
  casually reach hosts you didn't ask for," not "sandbox untrusted
  third-party content"); recorded here for a future hardening pass. See the
  `// KNOWN LIMITATION` comment in `src-tauri/src/preview/mod.rs`'s module
  doc comment.

### Edge cases
- Dev server down: standard "connection refused" page with a retry — never an
  error dialog loop.
- HiDPI capture scaling: snapshot at CSS-pixel scale so screenshots aren't
  4× the needed size (token cost of images is real).

---

## Feature 3 — Prompt library

### Goal
Saved, parameterized prompt templates, insertable from the compose overlay.
Trivial cost, daily-use win.

### Design
- **Storage:** global list in `settings.json` + per-project additions in the
  `.cimp/config.json` overlay (project templates shadow global ones by name —
  same precedence rule as every other overlaid setting).
  ```
  prompt_templates: [ { name, body, scope: "global"|"project" } ]
  ```
- **Variables:** `{selection}` (current terminal selection of the focused
  pane), `{file}` (prompted on insert), `{clipboard}`, plus free-form
  `{placeholder}` names — unresolved placeholders become tab-stops the user
  fills in the textarea (first placeholder selected on insert).
- **Invocation:** in the compose overlay, `/` at the start of an empty
  textarea opens a fuzzy-filter popover (↑↓ + Enter), mirroring the CLI
  slash-command idiom; also a small 📋 button for discoverability. A
  rebindable shortcut opens compose *with* the picker open.
- **Management:** Settings → Compose section: list, add/edit/delete, scope
  toggle, import/export as JSON (team sharing via a committed
  `.cimp/config.json` works out of the box because of the overlay).
- Ship 4–5 starter templates (review-this-diff, write-tests-for, explain,
  commit-message) — deletable, clearly marked as examples.

### Edge cases
- Name collisions between global and project scope: project wins, Settings
  shows the shadowed global entry greyed with a note.
- `/` conflicts with typing a literal slash-command for the AI tab: the picker
  only triggers on *empty* textarea + `/`, and `Esc`/continuing-to-type
  dismisses it into literal text — the agent's own slash commands still work
  unimpeded.

---

## Phasing

| Phase | Scope | Notes |
|---|---|---|
| **A. Prompt library** | Schema + overlay precedence + picker UI + starter set | Smallest; ships alone |
| **B. Image paste/drop** | Clipboard/drop handling + attach chips + temp-dir lifecycle | Independent of preview |
| **C. Usage tap + store** | Extend OOB tap for `usage` + `usage_stat` relation + OpenCode `/event` spike | Backend half of X-ray |
| **D. Usage UI** | Usage section in Code Intelligence + status-bar popover counter | Depends on C |
| **D2. Tuning advisor** | Heuristic rules + proposals IPC + Advisor card + apply-through-settings | Depends on C/D; richest with the V11 counters |
| **E0. Preview spike** | Child webview + capture + focus isolation on WebView2 | Gates F |
| **F. Preview tab** | Tab type (schema bump) + toolbar + snapshot→compose + auto-reload | Depends on B (attach path) + E0 |
| **G. Docs/tests** | README/FEATURES/MAINTENANCE, settings UI, unit+integration | Per repo convention |

Suggested order **A → B → C → D → D2 → E0 → F → G** — but A, B, and C+D+D2
are three independently shippable slices; the preview tab is the only gated
one.

## Decisions — OPEN

1. **Usage section placement** — proposed: inside Code Intelligence (6th
   section) rather than a new tab. Confirm the tab isn't getting crowded
   (Index/Activity/Memory/Context/Analyses/Usage).
2. **Preview tab type** — user-creatable tab type (schema bump) vs. reserved
   single tab. Proposed: user-creatable (people run several dev servers), which
   costs a migration like any tab-type addition.
3. **Image attach format** — append path text (simple, matches Claude CLI
   semantics today) vs. structured attachment per agent. Proposed: path text
   for V1, verified against both agents in a quick harness.

## Cost note

All four features are mechanical UI + plumbing work (Sonnet/Haiku fan-out);
the E0 webview-capture spike is the one part worth Opus attention — per the
standing agent-cost guidance.
