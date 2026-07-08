# IMPL-PLAN V14 — Workflow & Visibility

Companion to `docs/MILESTONE-V14-workflow-visibility.md`. File-by-file build
plan. Open decisions **assumed at proposed defaults** (Usage lives as a sixth
Code Intelligence section; Preview is a user-creatable tab type; image attach
= path text appended to the message) — sections marked ⚠ change if a decision
flips.

Phases: **A** (prompt library) → **B** (image paste/drop) → **C** (usage tap +
store) → **D** (usage UI) → **D2** (tuning advisor) → **E0** (preview spike) →
**F** (preview tab) → **G** (docs/tests/release). A, B, C+D+D2 are
independently shippable slices.

Grounding anchors (verified against current `develop`, post-V10):
- OOB tap: `src-tauri/src/oob/` — Claude transcript JSONL drain in
  `oob/claude.rs` (the V10 memory tap `record_tool_events` sits at the drain
  point; `newest_jsonl` picks the session file), OpenCode `/event` SSE in
  `oob/opencode.rs`. **No `usage` parsing exists today** (verified: zero
  matches in `src-tauri/src/oob`).
- Memory/graph store: `graph/service.rs` (write-lock discipline, ring-bounded
  relations precedent), `graph/schema.rs` (`GRAPH_SCHEMA_VERSION`
  reset-migration).
- Frontend: `lib/ComposeOverlay.svelte` (draft persistence, submit to focused
  tab), `lib/StatusBar.svelte` (usage meter + popover precedent),
  `lib/CodeIntelligenceView.svelte` (section router), `lib/tabs/types.ts`
  (`TabKind = 'ai-tool' | 'shell'` at `:74`).
- Clipboard: the Tauri clipboard plugin (WebView2 denies
  `navigator.clipboard.readText` — established workaround, see the AI-tab
  clipboard work).
- Settings: `settings/schema.rs` + the `.cimp/config.json` per-project
  overlay; migration system with timestamped backups.
- Scratch/temp discipline: session-scoped temp dirs, pruned on exit.

Schema note: Phase C adds a `usage_stat` relation to `graph.db` — coordinate
with whichever V11/V12 bump is in flight (one reset, not several).

---

## Phase A — Prompt library

**A1. Schema** (`settings/schema.rs`):
```rust
pub struct PromptTemplate { pub name: String, pub body: String }
// SettingsRoot gains:
pub prompt_templates: Vec<PromptTemplate>,   // global scope
pub templates_seeded: bool,                  // starter-set seeded once
```
Project-scope templates live in the `.cimp/config.json` overlay's own
`prompt_templates` array. **Resolution is by-name at read time** (project
entry shadows a same-named global), implemented in a small resolver IPC
rather than relying on raw JSON-merge semantics of the overlay:
`compose_templates(root?) -> Vec<{name, body, scope}>` in
`ipc/commands.rs`. Migration seeds 4 starter templates
(`review-this-diff`, `write-tests-for`, `explain-selection`,
`commit-message`) only when `templates_seeded == false`, then sets it —
deleting them sticks.

**A2. Variables** (frontend, `lib/compose/templates.ts`, new):
- On insert, substitute what's resolvable immediately:
  `{selection}` ← the focused pane's active terminal selection (the terminals
  store already exposes the xterm instance; `term.getSelection()`),
  `{clipboard}` ← Tauri clipboard plugin `readText`.
- Remaining `{placeholder}` tokens stay literal; the first one is selected
  (textarea `setSelectionRange`) so the user overtypes it; `Tab` jumps to the
  next (keydown handler active only while unresolved placeholders remain —
  scoped so it never fights the tab-key behavior of the overlay otherwise).

**A3. Picker UI** (`lib/ComposeOverlay.svelte` + `lib/TemplatePicker.svelte`,
new):
- Trigger: `/` typed when the textarea is empty → popover listing resolved
  templates, subsequence-fuzzy filtered by continued typing; `↑↓` + `Enter`
  inserts, `Esc` (or any non-matching input flow) dismisses and leaves the
  literal text — the agent's own slash commands are unaffected because the
  picker only ever exists pre-insert and dismisses into plain text.
- A small 📋 button beside the textarea opens the same popover
  (discoverability); a rebindable shortcut `open compose with picker`
  registers in the existing shortcut dispatcher.

**A4. Management UI** (`SettingsApp.svelte`, new "Compose" section): table of
global templates (name/body edit, delete, add); a read-only listing of
project-scope templates for the current cwd with a note that they live in
`.cimp/config.json` (edited there — Settings writes the *global* file only,
matching the existing settings-window scope rule).

**A5. Tests:** resolver shadowing (project beats global by name); seed-once
(`templates_seeded`); frontend unit tests for fuzzy filter + placeholder
tab-stop ordering (vitest, existing setup).

---

## Phase B — Image paste/drop into compose

**B1. Attach store** (`src-tauri/src/attach.rs`, new):
```rust
pub fn attach_dir(session: &str) -> PathBuf;        // %TEMP%/cimp-attach/<session>/
pub fn save_png(session: &str, bytes: &[u8]) -> AppResult<PathBuf>;  // n.png, monotonic
pub fn prune(max_age_days: u32);                    // startup + exit
```
`session` = the app-launch id (one dir per app run). Prune on startup (age >
3 days) and best-effort on exit.

**B2. Paste** (`lib/ComposeOverlay.svelte`): `on:paste` — if clipboard has an
image, read it via the Tauri clipboard plugin's image API (**not**
`navigator.clipboard` — WebView2 denies it) → IPC
`compose_attach_image(bytes)` → path back → chip appended to an
`attachments: string[]` state. Text pastes are untouched.

**B3. Drop:** listen to the Tauri native drag-drop event (`tauri://drag-drop`
payload carries absolute paths) while the overlay is open; filter
`.png/.jpg/.jpeg/.webp/.gif`; files are referenced **in place** (no copy —
they already exist on disk); non-image drops ignored (the terminal beneath
keeps its own behavior).

**B4. Submit** ⚠: on submit, append to the message text one line per
attachment: `\n[image] <absolute path>` followed by a single trailing
`Read the attached image file(s).` — plain path text (Claude Code and
OpenCode both accept local image paths in prompts; verified against both in a
5-minute harness before ship, per milestone Decision 3). Chips clear on
submit; draft persistence stores attachment paths alongside the draft text.

**B5. Tests:** attach-dir lifecycle (save/prune); paste handler unit test
with a mocked plugin; submit formatting.

---

## Phase C — Usage tap + store (token X-ray backend)

**C1. Claude tap** (`oob/claude.rs`): beside the existing V10
`record_tool_events` call at the drain point, add `record_usage(&obj, &ctx)`:
- Assistant messages: extract `message.id`, `message.model`, and
  `message.usage.{input_tokens, output_tokens, cache_read_input_tokens,
  cache_creation_input_tokens}` (tolerate absent fields — older transcript
  lines). Dedup by `message.id` (streamed transcripts can carry the same
  message id across updates — keep the **last** seen usage per id: upsert).
- `tool_result` content blocks (user-role lines): record
  `(tool_use_id, chars)`; join to the tool *name* via the `tool_use` block
  recorded earlier in the turn (small in-memory `tool_use_id → name` ring per
  session in `OobContext` — same lifetime as the existing agent tracking).
- Feed `GraphService::record_usage(root, session_id, UsageEvent)` — no-op
  when the graph store is disabled (usage rides `graph.db`; without the graph
  the X-ray is unavailable and the UI says so — same posture as memory).

**C2. Store** (`graph/schema.rs`, coordinated bump; `graph/service.rs`):
```
:create usage_stat {session_id: String, seq: Int =>
    kind: String,          # "turn" | "tool_result"
    model: String?, msg_id: String?,
    in_tok: Int, out_tok: Int, cache_read: Int, cache_make: Int,
    tool: String?, chars: Int, ts_ms: Int}
```
Ring-bounded per session (~2000 rows) and evicted with the session (reuse the
V10 session-eviction cascade). Aggregation queries (Datalog, in
`graph/index.rs`): per-session totals, per-tool char sums, per-turn series.

**C3. OpenCode spike + tap** (`oob/opencode.rs`): inspect the `/event` SSE
payloads on the pinned OpenCode version for token/usage fields
(`message.updated` events carry model metadata in some versions — verify).
Found ⇒ map into the same `record_usage`; absent ⇒ record `tool_result`-class
events only from the plugin's `/memory/event` traffic (chars of args are
already visible there) and mark the session `est-only`. Spike result recorded
in the milestone doc either way.

**C4. Effectiveness joins:** `RetrieveResult.chars` / `deduped_chars`
(V11-C) and read-advisor Activity events (V11-E) are already recorded by
their features; the Usage IPC (D1) reads them from their own stores — no
double-writing into `usage_stat`.

**C5. Tests:** JSONL fixture lines (assistant + usage, tool_result, absent
usage) → expected events; message-id upsert semantics; ring/eviction; the
tool_use_id→name join across a turn.

---

## Phase D — Usage UI

**D1. IPC** (`ipc/commands.rs`): `graph_usage(root?) -> UsageSnapshot`:
```rust
pub struct UsageSnapshot {
  pub current: Option<SessionUsage>,          // per-turn series + totals + top tools
  pub sessions: Vec<SessionUsageRow>,         // totals, cache-hit ratio, est_only flag
  pub effectiveness: Effectiveness,           // injected_chars, deduped_chars, advisor_displaced_chars
  pub offload_local_tasks: u64,               // from offload metrics
}
```
`SessionUsage.turns: Vec<TurnUsage { in_tok, out_tok, cache_read, tool_chars }>`;
`top_tools: Vec<(String, u64)>` (est tokens = chars/4, flagged `est`).

**D2. Section** (`lib/CodeIntelligenceView.svelte`): extend the section union
with `'usage'` (6th entry; ⚠ milestone Decision 1). Render:
- **This session:** per-turn stacked bars — pure CSS/flex divs (no chart
  dependency; heights normalized to the max turn), legend, and the
  top-consumers table (`tool · est tokens · calls`).
- **Sessions:** rows with totals + cache-hit % (`cache_read / (cache_read +
  input)`), `est-only` badge for OpenCode-degraded sessions.
- **Effectiveness:** three measured counters (injected / suppressed-by-dedup /
  advisor-displaced), every derived number labeled `est.`; the offload
  local-task count with a link-style pointer to the Offload Server tab.
- Reuse the existing 2 s poll + `graph-status` listener pattern; usage adds
  its fetch to `refresh()`.

**D3. Status bar** (`StatusBar.svelte`): the existing usage-meter popover
gains a "session tokens" line (current session in/out totals) fed by a
lightweight event emitted from `record_usage` (debounced 2 s). No new
widget.

**D4. Tests:** aggregation queries (totals, per-tool ranking, cache ratio);
snapshot assembly with graph-off and est-only paths.

---

## Phase D2 — Budget-tuning advisor

**D2.1 Signals** (`graph/index.rs` + `graph/service.rs`): one join the X-ray
doesn't ship by default — "was an injected file subsequently touched this
session": `injected` (V11-C) ⋈ `mem_event` (V10) per session. Add small
aggregates: `injection_follow_rate(root)`, `advisor_reread_rate(root)` (from
the V11-E Activity events), plus sample counts. Each degrades to `None` when
its source feature never ran — a rule without its signal simply doesn't fire.

**D2.2 Rules** (`src-tauri/src/advisor.rs`, new):
```rust
pub struct Signals { /* the D2.1 aggregates + current GraphSettings */ }
pub struct Proposal { pub setting: String, pub current: String,
                      pub proposed: String, pub rationale: String,
                      pub rule_id: &'static str }
pub fn evaluate(sig: &Signals) -> Vec<Proposal>;   // static rule list, each with min_samples
```
V1 rules per the milestone: min-score raise, advisor min-lines raise,
turn-budget lower. Dismissals persisted in settings
(`advisor_dismissed: Vec<DismissedRule { rule_id, signature }>`) keyed by a
coarse signature of the triggering rate (e.g. the rate bucketed to 10%) so a
materially changed rate re-fires the proposal.

**D2.3 IPC + apply path** (`ipc/commands.rs`):
`graph_usage_advice(root?) -> Vec<Proposal>` (runs `evaluate` over fresh
signals), `advisor_dismiss { rule_id, signature }`. **No bespoke apply IPC:**
the Apply button writes through the existing settings-update path the
Settings window uses — visible immediately, undoable, migration-safe.

**D2.4 UI** (Usage section, top card): proposals with Apply/Dismiss;
collecting-state below `min_samples`; a tooltip listing rule ids +
thresholds. Optional narrative line via `run_internal` when a local backend
is ready (skipped silently otherwise; numbers never come from the model).

**D2.5 Tests:** each rule's threshold + min-sample gate; missing-signal rules
don't fire; dismissal signature semantics (same bucket suppressed, changed
bucket re-fires); proposal → settings round-trip.

---

## Phase E0 — Preview spike (gates F)

Empirical, on Windows/WebView2 first (Linux webkit2gtk noted, non-blocking):
1. **Child webview in the main window:** Tauri 2.x multi-webview
   (`WebviewBuilder` attached to the existing window) positioned over a pane
   rect; verify coexistence with the xterm panes, the portal/drag system, and
   z-order during tab drag.
2. **Programmatic capture:** WebView2's `CapturePreviewAsync` — verify wry
   exposes it; if not, a small `windows-rs` call against the
   `ICoreWebView2` handle (C-FFI is acceptable when it earns its keep; this
   is a single COM call, not a dependency saga).
3. **Focus/keyboard isolation:** typing in the preview must not leak into
   shortcuts; `Esc`/global shortcuts must still reach cImp (mirror the
   hold-`Alt` bypass learnings from the AI-tab work).
4. **Resize/DPI:** capture at CSS-pixel scale (token cost of oversized
   screenshots is the point of the feature).
Record findings in the milestone doc (V10 D0 style). Any hard failure ⇒
Phase F re-scopes to "open in system browser + attach screenshots by hand"
(Phase B already ships the attach half) — recorded, not silently dropped.

---

## Phase F — Preview tab

**F1. Tab type** ⚠ (`lib/tabs/types.ts` + settings schema): `TabKind` gains
`'preview'` (`:74`); tab config gains `url: String`,
`device_width: Option<u32>`, `auto_reload: bool`. Settings migration for the
new kind; per-project default URL remembered in the `.cimp/config.json`
overlay (`preview_last_url`).

**F2. Backend** (`src-tauri/src/preview/mod.rs`, new): manages one child
webview per open preview tab:
- `preview_open(tab_id, url, rect)`, `preview_navigate`, `preview_reload`,
  `preview_set_rect` (called from the frontend on pane layout changes — the
  same rect the pane content div occupies), `preview_capture(tab_id) ->
  PathBuf` (PNG into the Phase-B attach dir), `preview_close`.
- **Navigation policy:** an `on_navigation` handler allows only
  `localhost` / `127.0.0.1` / RFC-1918 hosts unless `preview_allow_remote`
  (new root setting, default false); external links open via the system
  opener. This is a preview surface, not a browser — and it must not become a
  prompt-injectable browsing pane beside agent tabs.
- Tab lifecycle: hide (not destroy) on tab-switch away; destroy on tab close;
  connection-refused renders the webview's own error page + a cImp retry
  toolbar button (no dialogs).

**F3. Frontend:** `lib/PreviewToolbar.svelte` rendered by `Pane.svelte` for
preview-kind tabs (URL field, back/reload, device-width presets that letterbox
the child rect, **Snapshot → compose**); the pane body is an empty measured
div whose rect drives `preview_set_rect`. Snapshot button: `preview_capture`
→ open compose with the attachment chip (Phase B path).

**F4. Auto-reload:** subscribe the V13 `fs-batch` broadcast if present (else
skip — feature degrades to manual reload): reload after a quiet period of
~1 s following a batch, only when `auto_reload` and the tab is visible.

**F5. Tests:** navigation-policy unit tests (host classification); rect
math; capture path lands in the attach dir. Webview behavior itself is
covered by the E0 spike + manual passes (same posture as the TUI work).

---

## Phase G — Docs, settings polish, tests, release

- README / `docs/FEATURES.md`: prompt library (+ `/` picker), image attach,
  Usage section (honest-estimates note), Preview tab (+ the
  localhost-only default). `docs/MAINTENANCE.md`: `usage_stat` schema note,
  the WebView2 capture dependency (E0 findings), attach-dir lifecycle.
- `docs/PACKAGING.md` untouched unless E0 surfaces a new dylib (not
  expected).
- Full `cargo test` + `npm run check` + vitest; CHANGELOG; version bump;
  release per the standard workflow.

---

## Appendix — consolidated change surface

**New tab kind:** `'preview'` (+ settings migration). No new reserved tabs.

**New settings:** `prompt_templates`, `templates_seeded`,
`preview_allow_remote`, `advisor_dismissed` (root); per-tab preview fields
(`url`, `device_width`, `auto_reload`); overlay keys `prompt_templates`
(project scope), `preview_last_url`.

**Schema (coordinated bump):** `usage_stat` relation.

**New IPC:** `compose_templates`, `compose_attach_image`, `graph_usage`,
`graph_usage_advice`, `advisor_dismiss`,
`preview_open/navigate/reload/set_rect/capture/close`,
`workbench`-independent (no V13 dependency except the optional `fs-batch`
subscription).

**New Rust files:** `attach.rs`, `preview/mod.rs`, `advisor.rs`.

**New frontend files:** `lib/TemplatePicker.svelte`, `lib/PreviewToolbar.svelte`,
`lib/compose/templates.ts`; touches: `ComposeOverlay.svelte`,
`StatusBar.svelte`, `CodeIntelligenceView.svelte` (6th section),
`SettingsApp.svelte` (Compose section), `tabs/types.ts`.

**Backend touches:** `oob/claude.rs` (+`oob/opencode.rs` spike),
`graph/{schema,service,index}.rs` (usage), `ipc/commands.rs`.

**Spikes:** C3 (OpenCode usage fields), E0 (child webview + capture — gates
the preview tab; its failure mode re-scopes, doesn't cancel, the milestone).
