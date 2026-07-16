# Token Efficiency in cImp — Features & the Effectiveness Card

*Written 2026-07-12. Grounded in `src/lib/CodeIntelligenceView.svelte`,
`src-tauri/src/graph/service.rs`, and `docs/FEATURES.md`. For the original
design rationale see `docs/MILESTONE-V11-token-efficiency.md`.*

cImp attacks cloud-token spend from four directions, layered across
milestones. This doc lists each feature, how it saves tokens, and then
explains how to read the **Effectiveness** card in Code Intelligence →
Overview → Usage.

---

## 1. Task offload (V8) — don't send the work to the cloud at all

The `offload_task` / `offload_batch` MCP tools let the cloud model hand a
self-contained subtask (broad searches, log/file summarization, web research)
to a **local llama-server**. The heavy input never enters the cloud context;
only the synthesized result comes back. The local worker has its own native
tools (`read_file`, `code_search`, `run_command`) plus user-configured MCP
servers, so it can do multi-step work independently.

## 2. Code graph tools (V9 / V12 / V15) — indexed answers instead of file dumps

`graph_find_symbol`, `graph_callers` / `graph_callees`, `graph_references`,
`graph_outline`, `graph_impact`, `graph_tests_for`, `graph_path`,
`graph_architecture` answer code-structure questions from the CozoDB index
with precise, token-bounded rows — replacing grep fan-outs and whole-file
Reads. `run_check` returns deduplicated, grouped diagnostics (≤5 sample sites
per group) instead of a raw compiler/linter dump.

## 3. Context engine (V10) — inject relevance so the agent doesn't explore

Session memory tracks what files each agent session touched; on every prompt,
`/context/retrieve` (via the Claude `UserPromptSubmit` hook / OpenCode
plugin) injects a budget-bounded set of ranked file digests. The bet: a few
hundred injected chars displace multi-thousand-token exploration turns.

## 4. Token Efficiency milestone (V11) — the dedicated push

All shipped:

- **`graph_snippet`** — fetch one definition's body (16 KiB cap,
  `max_body_bytes`) instead of Reading a 2k-line file. Flagged `stale` when
  the on-disk hash has drifted from the index.
- **`graph_repo_map`** — a once-per-session, char-budgeted
  (`repo_map_budget_chars`) skeleton of the most call-central files and their
  top exported signatures, so the agent starts oriented instead of burning
  explore turns. Agent-pullable any time; opt-in first-prompt injection
  (`repo_map_on_session_start`).
- **Injection dedup** — a file already injected in full and unchanged is
  demoted to a one-line "unchanged" reminder on later turns instead of being
  re-sent; re-injects with an `(updated)` tag on change or after
  `context_dedup_ttl_turns` (default 10).
- **Compaction survival** — a Claude `PreCompact` hook
  (`cimp --precompact-hook` → `POST /context/compaction`) feeds the compactor
  the session's ranked working set + pinned notes and clears the dedup state,
  so the agent doesn't re-explore after compaction (the biggest single token
  bleed in long sessions). `compaction_context`, default on.
- **Redundant-read advisor** (opt-in, off by default) — a Claude `PreToolUse`
  hook on `Read` (`cimp --read-hook` → `POST /context/should_read`) denies a
  re-read of an unchanged already-read file, replying with its outline
  (`advise` mode) or outline + relevant symbol body (`substitute` mode)
  instead of the full content — never a bare refusal. One reminder per file
  per session; always passes right after a compaction.
- **Local-model context digests** — files with no useful outline
  (docs/configs/long scripts) get a ≤3-line semantic digest written by the
  **local** offload backend (never routed off-box) and cached, so injection
  stays compact without cloud spend. `context_llm_digests`.
- **Code embeddings + `graph_semantic_code`** — symbol-level semantic code
  search returning signature rows (never bodies) to chain into
  `graph_snippet`; the search → snippet chain is the token-efficient
  replacement for grep → Read.

## 5. Measurement layer (V14)

The **Usage** section in Code Intelligence: per-turn stacked token bars, a
top-consumers table, per-session totals + cache-hit ratio, the Effectiveness
panel below, and a **budget-tuning Advisor** that proposes measured,
propose-and-confirm changes to the V10/V11 knobs (e.g. raise
`context_min_score` when injected files go unused; raise
`read_advisor_min_lines` when reminders are usually followed by a full
re-read anyway).

## 6. Harness contract hardening (V16)

Everything above rides on hook contracts of two self-updating CLIs cImp
doesn't pin. V16 adds detection + honest pricing:

- **Version tripwire + drift canaries** — the Advisor card gains a
  `drift.*` rule class: harness-version-changed (with a Mark-verified
  action), reminder-reasons-not-reaching-the-model, hook-silent,
  injection-unseen, usage-fields-gone, shim payload drift
  (`/activity/contract_drift`), and shell-bypass detection. See
  `docs/MAINTENANCE.md` → harness contracts for the spike recipes and the
  `harness_versions` state (global `settings.json`).
- **Trust TTL** (`read_advisor_ttl_turns`, default 0=off) — bounds how long
  the advisor trusts the agent's memory of a file across context loss it
  can't observe (context editing, tool-result truncation).
- **Price-weighted Usage view** — a **tokens | est. cost** toggle on the
  per-turn bars and Sessions table; segments repriced by $/MTok from the
  global LLM price table, auto-matched by `model_prefix` (longest wins; no
  match ⇒ tokens + a hint). Cache-write is now its own bar segment.
  Anthropic seed rows use the 1h-TTL 2× cache-write rate Claude Code
  sessions actually pay.
- **Compounding savings** — the Effectiveness card's new line (see below).

---

## 7. Redundant-read advisor, escalated (V17)

V17 extends the V11 read advisor (§4) into the corners V11 deliberately passed
on. All three pieces are gated under the same `read_advisor` opt-in and the V16
E1 hard block; the sub-toggles default **on** *within* that opt-in (the advisor
itself is still off by default).

- **Diff-substitute for changed-file re-reads** (`read_advisor_diffs`, default
  on) — V11 passed a re-read unconditionally once the content hash differed,
  but "changed since last read" is the *dominant* re-read trigger: the agent
  just edited the file (or `cargo fmt` / a build script / another tab touched
  it) and re-reads the whole thing to verify. The advisor now retains a
  **snapshot** of the content it last showed the agent (per-entry ≤ 512 KiB,
  whole store LRU-bounded to ~16 MiB, in-memory only) and answers a changed
  re-read with a **line-level unified diff against that snapshot** — exact, not
  lossy: a diff versus what the agent actually read cannot mislead, so it is
  safe on the post-edit verify loop. Falls back to a plain pass whenever no
  snapshot survives (small file / over-cap / evicted) or the rendered diff
  exceeds half the new content (a near-rewrite isn't worth a denial). A content
  change **re-arms** an already-reminded file (the old remind's "unchanged"
  promise is stale), capped at 3 reminders per file per session so the advisor
  can never fight an insistent agent in a loop; the immediate second ask on
  *unchanged* content always passes.
- **Shell-read interception** (`read_advisor_shell`, default on) — V16 *detected*
  `cat` / `Get-Content` whole-file reads of just-reminded files
  (`drift.read_bypass.v1`) but did not intercept them, so a bypass cost the
  remind *plus* the whole file — worse than no advisor. A second `PreToolUse`
  **Bash** matcher on the same `cimp --read-hook` shim now runs the shell
  command through a strict parser (`graph/shellread.rs`): only a provable pure
  whole-file read of one file — verb ∈ {`cat`, `type`, `Get-Content`, `gc`},
  `-Raw` tolerated, no pipe/redirect/glob/second-path/command-chaining — is
  routed to the identical `should_read` verdict; anything composite runs
  untouched. Interception must be provably equivalent to a `Read`, never a
  guess. The deny reason is the same advice text prefixed `answered without
  running the command —`. Partial-read verbs (`sed -n`, `head`, `tail`) are
  deliberately rejected. Feature 1's diff branch applies here too. Claude-only
  for now (OpenCode rides the pending V16 `tool.execute.before` spike).
- **First-read digest tier** (`read_advisor_first_read_kb`, default 0 = off) —
  the advisor only fires on *re*-reads; a first `Read` of a 300 KB
  log/lockfile/generated JSON is a pure burn. When enabled, the first
  whole-file read of a **non-code** file (no parsed outline) at or above the
  KiB threshold is answered with the cached local-model digest (§4) plus a
  first/last ~40-line sample, with the escape hatch `Read({file, offset,
  limit}) always passes`. Requires a digest cached for the current content hash
  (content-hash-keyed, so it survives across sessions); a cache miss enqueues
  one on the local-only path and passes — protection begins on the second
  encounter. No snapshot is kept for these (generated-file diffs are useless
  and would blow the LRU).

**Honest numbers.** Diff and first-read reminds reuse the V11 remind accounting
path unchanged, so every counter gets truthful numbers for free: `displaced` =
the **full** new-content chars (what the agent would have received), and
`advice_chars` = the **remind** text chars (the diff, or the digest + head/tail
sample). The Activity `request` string is marked `(changed — diff substituted)`
or `first-read` so the Effectiveness tooltip can split these out from plain
outline reminds without a new field.

## 8. Test-run parsers for `run_check` (V17)

V12's `run_check` displaced raw compiler/linter dumps with grouped diagnostics;
V17 extends the same machinery to the remaining big raw dump — test runs — by
adding two `ParserKind`s. Failures-only by construction, so the existing
group/dedup/≤5-samples machinery and the auto-check baseline diff work
unchanged; a test that starts failing surfaces exactly like a new compiler
error.

- **`cargo-test`** — parses stable-toolchain *text* output (`--format json` is
  nightly-only): `test <name> ... FAILED` lines, each upgraded when its
  `---- <name> stdout ----` block follows (truncated to the first ~15 lines,
  with `panicked at <file>:<line>:<col>` resolved into the diag location), plus
  the tail counts line (`test result: …`) folded in as a file-less `Note` so a
  clean run renders `ok — N passed` rather than silence. A compile error that
  aborts before any test lines is additionally run through the generic rustc
  matcher and merged, so `run_check(name:"test")` on a broken build still
  surfaces the compile error.
- **`jest-json`** — parses `jest --json` / `vitest --reporter=json` (same
  shape): each `testResults[] × assertionResults[]` with `status == "failed"`
  yields a diag from the first lines of `failureMessages[0]` (ANSI-stripped —
  jest embeds color codes inside JSON strings); `testFilePath` is absolute, so
  it's relativized against the run cwd (the changed-only filter compares
  git-relative paths); `numPassedTests`/`numFailedTests` become the counts
  `Note`. Malformed JSON yields an empty result, never an error.

No new settings — `checks` is already the per-project surface. Add to
`.cimp/config.json`:

```jsonc
"checks": [
  { "name": "test", "cmd": "cargo test",             "parser": "cargo-test", "timeout_secs": 300 },
  { "name": "jest", "cmd": "npx jest --json",         "parser": "jest-json",  "timeout_secs": 300 }
  // vitest: "cmd": "npx vitest run --reporter=json", "parser": "jest-json"
]
```

`GRAPH_GUIDANCE` (and the OpenCode instructions it mirrors) now nudges the agent
to prefer a configured test check over running the test command in Bash — "it
returns failures only."

## 9. Tool-surface accounting + lean surface (V17)

The `graph_*` / `run_check` tool descriptors are advertised to the cloud
session and the offload worker and cache-written **once per session**. V17
measures that surface and lets you trim its cold tail:

- **Tool-surface line** — the Effectiveness card shows the advertised surface
  size (`tools()` serialized length + count), labelled `est.` for the
  chars→tokens estimate.
- **`graph.lean_tools`** (default off) — hides the cold-tail tools from the
  **advertised** surface only; each still ANSWERS if an agent calls it by
  name (dispatch is name-driven). `LEAN_HIDDEN` = `graph_cycles`,
  `graph_dead_exports`, `graph_struct_search`, `graph_path`,
  `graph_architecture` (frozen from the E0 Activity-store check; the
  workhorses — `graph_find_symbol`, `graph_callers`, `graph_callees`,
  `graph_outline`, `graph_snippet`, `graph_references`, `run_check`,
  `graph_search_docs`, `graph_semantic_docs` — are never hidden).
- **`surface.lean.v1` advisor rule** — after ≥10 sessions with no calls to any
  hidden tool **in the last 30 days**, the Advisor proposes turning `lean_tools`
  on (Apply; dismissable; a single call to a hidden tool within that trailing
  window silences it). The window is deliberate: the activity ring is
  count-capped, not time-capped, so an all-time scan would let one cold-tail
  call weeks ago suppress the suggestion indefinitely.
- **Editorial pass** — the wordiest tool descriptions were tightened (meaning
  preserved): worker surface ~11,891 → ~11,343 chars (≈137 est. tokens saved
  per session); the MCP surface mirrors the same descriptions when the graph
  is enabled.

## 10. Graduation rules — `adopt.*` (V17)

V11 decision 2 deferred `read_advisor`'s default to field data; V14/V16 built
the evidence (`advisor_reread_rate`, bypass rate, E1 status). V17 turns it into
two per-project, propose-and-confirm Advisor rules — no silent default flips.
Both render on the existing Advisor card and use standard per-rule dismiss
memory + the V16 apply cooldown.

- **`adopt.read_advisor.v1`** — proposes *enabling* `read_advisor` when it is
  off, E1 is verified (`harness_versions.e1_status == "pass"` — proven, not
  merely "not failed": an `unverified` hook must never auto-graduate), and
  session memory shows the waste the advisor exists to stop: ≥ 3 redundant
  large same-file re-reads per recent session across ≥ 10 sessions. Detection
  without the hook uses `GraphIndex::redundant_read_candidates` over `mem_event`
  read rows — same file, same session, read ≥ 2× with no intervening edit,
  file ≥ `read_advisor_min_lines`. Labelled `est.` (without content hashes
  this is an approximation — an external tool may have changed the file between
  reads; the message says so). Suppressed while any `drift.*` read rule is
  firing (don't propose enabling what drift says is broken).
- **`adopt.read_advisor_substitute.v1`** — proposes switching
  `read_advisor_mode` to `substitute` when the advisor is on in `advise` mode,
  the remind→full-reread rate is low (≤ 0.2 over ≥ 20 samples — reminders are
  sufficient, the agent rarely follows one with a full re-read) and bypass is
  below the V16 `BYPASS_HIGH` threshold. Mutually exclusive with the existing
  high-reread-rate `read_advisor_min_lines` rule by construction (that rule
  needs reread-rate ≥ 0.5; the ranges can't overlap).

---

## The Effectiveness card

Location: **Code Intelligence → Overview → Usage**. Design rule, stated in a
comment in the component itself: **"measured counters, never fabricated
savings."** Every number is a real character count taken at the point the
event happened; token figures are simply `chars ÷ 4` and are explicitly
badged `est.`. All counters are in-memory, so they read as **"since app
restart,"** not a permanent ledger.

| Value | What it measures | Scope | Savings or spend? |
|---|---|---|---|
| **chars injected** | Cumulative chars of full digests actually injected into prompts by the context engine, summed from each retrieve's measured output | This project's sessions, since restart | **Spend** — what injection costs |
| **chars suppressed by dedup** | Chars of digests that *would* have been re-injected but were demoted/dropped because the file was unchanged since its last injection | This project's sessions, since restart | **Direct measured saving** |
| **chars displaced by read-advisor** | Sizes of the reminder texts the advisor answered Reads with (Activity events `source: read_advisor`, `tool: remind`), **net of bypasses** (V16: a remind the agent answered with a shell `cat` displaced nothing — the bypassed reminders' *text* chars are subtracted, the same unit as the displaced sum; the whole-file chars the shell re-read appear separately in the tooltip, est.) | Process-wide (Activity store has no per-project key), since process start | **Saving, estimated** — hence the `est.` label |
| **chars of cache-reads avoided (compounding)** | V16: displaced chars re-counted once per subsequent turn — the API re-sends the whole conversation every turn, so content kept out at turn N is saved again as a cache read on every turn after N. The turn clock is the injection retrieve when context injection is on, or genuine user prompts seen by the transcript tap when it's off (so the readout accrues for read-advisor-only sessions too). Measured turn-by-turn as the session runs, no projection; with a matched price row it also shows an `est. $` at the cache-read rate | This project's sessions, since restart | **Saving, estimated (compounding)** |
| **tasks served locally** | Count of offload jobs the local llama-server handled instead of the cloud model (filled in from the OffloadService) | Offload host | **Proxy count** — whole subtasks diverted; volume on the Offload server dashboard (Tool Activity tab) |
| **tool surface** | Serialized chars + count of the graph tool descriptors advertised to the cloud session (post-`lean_tools` filter), cache-written once per session | Live settings | **Spend** — the fixed per-session cost of offering the tools; `lean_tools` trims the cold tail |

### How to interpret it

- **Dedup + advisor-displaced are the genuine savings numbers** — real
  content that was measurably not sent to the cloud.
- **Injected is the spend those savings ride on.** It only pays off if
  injected files displace exploration; the Advisor's injection-follow-rate
  rule checks exactly that (were injected files later read/edited in the
  session?) and proposes raising `context_min_score` when they go unused.
- **Tasks served locally** is a count, not a token figure — each one
  represents an entire subtask's tokens kept off the cloud bill; the actual
  local token volume lives on the Offload server dashboard (Tool Activity tab).

A healthy readout is dedup/advisor chars growing relative to injected chars
over a session. When they don't (injections unused, reminders always followed
by a full re-read anyway), the Advisor card above the panel exists precisely
to propose tightening the knobs.
