# V17 — Token Efficiency II (Advisor Escalation & Output Dedup)

**Status:** SPEC (written 2026-07-12). Not yet coded.
**Builds on:** V11 token efficiency (`should_read` verdict path in
`graph/service.rs:1556`, `read_hook.rs` / the PreToolUse overlay in
`tabs/config.rs:291-310`, the digest cache + `offload/supervisor.rs::run_internal:592`),
V12 `run_check` (`checks/` module: `CheckDef`, `ParserKind`, group/dedup
machinery), V14 measurement (Advisor rules in `advisor.rs`,
`GraphIndex::advisor_reread_rate:3422`, the Usage/Effectiveness card),
V16 harness hardening (the shell-bypass matcher in `oob/claude.rs:388` +
`graph/service.rs:313`, `harness_versions.e1_status` gating, apply cooldown).

## Why

V11 attacked whole-file `Read`s, re-exploration, and redundant injection; V14
measured it; V16 hardened the contracts it rides on. The field data and the
V16 code review surfaced the next tier of sinks — all in corners the V11
features deliberately pass on:

1. **Changed files always pass.** `should_read` passes unconditionally when
   the content hash differs from the last observed read
   (`graph/service.rs:1616-1624`) — but "changed since last read" is the
   *dominant* re-read trigger: the agent just edited the file (or `cargo fmt`
   / a build script / another tab touched it) and re-reads the whole thing to
   verify. The advisor's most common case is its least protected.
2. **Shell reads route around the advisor.** V16 *detects* `cat`/`Get-Content`
   bypasses (the `drift.read_bypass.v1` canary) but doesn't intercept them —
   today a bypass costs the remind *plus* the whole file.
3. **First reads of huge non-code files burn with no protection.** The advisor
   only fires on *re*-reads; a first `Read` of a 300 KB log/lockfile/generated
   JSON is a pure burn, and the V11 digest cache (`context_llm_digests`) only
   serves injection.
4. **Test output is the last big raw dump.** `run_check` displaced
   compiler/linter dumps with grouped diagnostics, and its parser set already
   includes `Pytest` — but `cargo test` and jest/vitest output still arrives
   through Bash uncompressed.
5. **The tool surface itself is unpriced.** ~30 graph/offload tool schemas
   ride every session's system prompt — cache-written at the 2× rate once per
   session and re-written on every cache expiry — and that cost appears
   nowhere in the Usage view.
6. **The V11 graduation rules were never written.** Milestone decision 2 said
   `read_advisor` (and `substitute` mode) should graduate from field data;
   V14/V16 built the evidence (`advisor_reread_rate`, bypass rate, E1
   status) but no Advisor rule consumes it yet.

Every feature reuses shipped machinery; **no graph.db schema bump** — all new
state is in-memory session state, existing relations (`digest`, `mem_event`),
or the Activity store. Features 1–3 are Claude-first (they ride `PreToolUse`
and inherit the V16 E1-fail hard block); Feature 4 rides the shared MCP
surface (both harnesses + the offload worker); 5–6 are measurement/UI.

**Posture.** Same as V11: behavior-altering pieces are gated under the
existing `read_advisor` opt-in (sub-toggles default on *within* that opt-in,
since the advisor itself is off by default); everything else is
detection/measurement and needs no opt-in.

---

## Feature 1 — Diff-substitute for changed-file re-reads

### Goal
Answer the re-read of a *changed* already-read file with a **unified diff
against what the agent actually read**, not the whole file. Unlike outline
reminders this is exact, not lossy — a diff versus the last-read snapshot
cannot mislead — so it is safe to fire on the post-edit verify loop that
dominates real sessions.

### Design (`graph/service.rs::should_read` + `graph/context.rs`)
- **Snapshot store:** `should_read` already reads the file at verdict time
  (`:1590`) to hash it. Retain the content, not just the hash: extend
  `read_seen`'s value from `(hash, turn)` to `(hash, turn, Option<Arc<str>>)`
  — snapshot kept only for files ≥ `read_advisor_min_lines` and ≤ a per-entry
  cap (512 KiB), whole store LRU-bounded (~16 MiB, constants not settings).
  In-memory only; an evicted or missing snapshot simply means the changed-file
  read passes as today. Snapshots are also captured on the *pass* paths (first
  read, changed read, TTL re-stamp) — every observation point already holds
  the content.
- **Verdict branch:** where the current code passes on hash mismatch
  (`:1616-1624`): if a snapshot exists and the new content is valid UTF-8,
  compute a line-level unified diff (new dependency, see Decisions). Remind
  with: header (`file changed since you read it (turn N) — diff against what
  you read:`), the diff, and the standard escape hatch (`Re-read with
  Read({file, offset, limit}) if you need exact text.`).
- **Only when it saves:** if the diff (rendered) is > 50% of the new file's
  size, pass instead — a near-rewrite isn't worth a denial. Binary/non-UTF-8
  ⇒ pass.
- **After a diff-remind** the agent knows current content: update `read_seen`
  to the new hash + new snapshot, stamp the turn.
- **Remind bookkeeping (see Decisions 1):** a remind — diff or outline — still
  inserts into `reminded`, and a reminded file still passes… but a **content
  change re-arms the file** (the old remind promised "unchanged"; once the
  file changed that promise is stale), capped at `read_advisor_remind_cap`
  reminds per file per session (default 3) so the advisor can never fight an
  insistent agent in a loop. The immediate-second-ask escape hatch is
  unchanged: same file, same content ⇒ always pass.
- **Post-compaction:** existing flag already passes everything — diffs against
  content the agent no longer holds would mislead; correct as-is.
- **Accounting:** reuse the remind path unchanged — `displaced` = full file
  chars, `advice_chars` = diff text chars; Activity + `RemindMark` +
  compounding base all get the honest numbers for free. Diff reminds carry
  `tool: remind` with a `diff` marker in the request string so the
  Effectiveness tooltip can split them out.

### Settings
`read_advisor_diffs: bool` (default **true** — strictly-better substitute,
still master-gated by the `read_advisor` opt-in and the E1 hard block).

### Tests
Changed file + snapshot ⇒ diff remind, `read_seen` re-stamped; diff > 50% ⇒
pass; snapshot evicted ⇒ pass; non-UTF-8 ⇒ pass; re-arm fires on change and
respects the cap; second ask on unchanged content passes; post-compaction
passes; LRU never exceeds the byte budget.

---

## Feature 2 — Shell-read interception (close the detect→advise loop)

### Goal
The V16 transcript tap already *recognizes* shell whole-file reads of
just-reminded files (`oob/claude.rs:388`); intercept them instead of only
scoring them. Today a bypass costs the remind plus the full file — worse than
no advisor. Interception makes the remind stick.

### Design
- **Overlay** (`tabs/config.rs`): the `PreToolUse` entry gains a second
  matcher `Bash` pointing at the **same** `--read-hook` shim (no new binary,
  no new route). Installed under the same gates as the `Read` matcher
  (`read_advisor` on + E1 not failed).
- **Shim** (`read_hook.rs`): dispatch on `tool_name`. For `Bash`, extract
  `tool_input.command`; if it parses as a **pure whole-file read** — a single
  command, no pipes/redirects/globs/command-chaining, one path argument, verb
  ∈ {`cat`, `type`, `Get-Content` (alias `gc`, flag `-Raw` tolerated)} —
  resolve the path against the hook's `cwd` and POST the existing
  `/context/should_read`. Anything composite passes untouched: interception
  must be provably equivalent to a `Read`, never a guess. Reuse/extract the
  V16 bypass matcher's command parser (`graph/service.rs:313` area) into a
  shared helper so tap and shim can't drift apart.
- **Verdict:** identical `should_read` path (offset `None`); deny reason =
  the same advice text prefixed `answered without running the command —`.
  Feature 1's diff branch applies here too.
- **Economics of the canary:** `drift.read_bypass.v1` stays as the guard —
  with interception live its rate should *fall*; a persistently high rate now
  means the agent found a new escape route (e.g. `sed -n`) and the canary
  message points there. Bypass *scoring* (net-of-bypass Effectiveness math)
  is unchanged: an intercepted shell read records as a remind, not a bypass.
- **OpenCode:** rides the pending V16 Phase G `tool.execute.before` veto
  spike; until that lands, Claude-only (same accepted asymmetry as V11 E).

### Settings
`read_advisor_shell: bool` (default **true**, nested under `read_advisor`;
the Bash matcher is only installed when both are on — zero overlay overhead
otherwise).

### Tests
Parser: accepted forms (`cat p`, `type p`, `Get-Content p`, `gc -Raw p`) and
rejected forms (pipes, `>`, two paths, globs, `&&`, `sed -n`); relative path
resolves against hook `cwd`; verdict parity with an equivalent `Read`;
intercepted read records remind (not bypass); overlay gating matrix (toggle ×
E1 status), extending the existing test at `tabs/config.rs:963`.

---

## Feature 3 — First-read substitution for huge non-code files

### Goal
Protect the *first* read of a large non-code file (log, lockfile, generated
JSON, data dump): answer with the cached local-model digest + a head/tail
sample instead of the full content. Gives `context_llm_digests` a second
consumer beyond injection.

### Design (`graph/service.rs::should_read`)
- New branch where the never-read case currently records-and-passes
  (`:1616-1624` `_` arm), evaluated only when all hold:
  - `read_advisor_first_read_kb > 0` and file size ≥ that threshold;
  - the file is **not code**: not indexed with symbols by the graph (data/
    docs/logs — outline-based advice is useless for these, which is exactly
    why V11 skipped them);
  - a digest is **cached** for the current content hash (the V11 `digest`
    relation, keyed `(file, content_hash)`).
- Remind text: byte/line counts, the ≤3-line digest, first ~40 + last ~40
  lines, and the escape hatch (`Read({file, offset, limit})` — offset/limit
  reads always pass, unchanged).
- **Cache miss ⇒ pass and enqueue** the digest (existing bounded queue), same
  never-block posture as V11 F: protection kicks in from the second encounter
  (digests are content-hash-keyed, so they survive across sessions).
- Remind-once, the Feature 1 re-arm rule, and post-compaction pass all apply
  unchanged. The `read_seen` snapshot is **not** kept for these (they'd blow
  the LRU and diffs of generated files aren't useful) — a later changed
  re-read just passes.

### Settings
`read_advisor_first_read_kb: u32` (default **0 = off** — a separate opt-in
tier within the advisor; proposed starting value when enabled: 256).

### Tests
Under threshold / code file / no digest ⇒ pass (and enqueue on the digest
miss); cached digest ⇒ remind with digest + head/tail; offset read passes;
remind-once respected; disabled by default.

---

## Feature 4 — Test-run parsers for `run_check`

### Goal
Extend `run_check`'s grouped-diagnostics pattern (V12) to the remaining big
raw dump: test runs. `ParserKind::Pytest` already exists; add the ecosystems
this project and its users actually run, so `run_check(name: "test")`
displaces raw `cargo test` / `npm test` Bash output.

### Design (`checks/parsers.rs` + `checks/mod.rs`)
- New `ParserKind` variants (kebab-case wire names, same as the enum
  convention at `checks/mod.rs:63`):
  - **`CargoTest`** — parse stable-toolchain *text* output: `test <name> ...
    FAILED` lines, the `---- <name> stdout ----` blocks (truncated to the
    first ~15 lines each; panic `at <file>:<line>` resolved into the `Diag`
    location when present), and the tail counts line (`N passed; M failed`).
    JSON output is nightly-only — text is deliberate (see Decisions 5).
  - **`JestJson`** — `jest --json` / `vitest --reporter=json` (same shape):
    per-failure `Diag` from `testResults[].assertionResults[]` with the
    failure message's first lines; file from `testFilePath`.
- Failures-only by construction: passing tests emit nothing; the existing
  group/dedup/≤5-samples machinery (`checks::run`, `fmt_check_report`) and
  the auto-check baseline diff (`checks/auto.rs::diff_groups`) work unchanged
  — a test that starts failing surfaces exactly like a new compiler error.
- A successful run with zero failures returns the counts line only
  (`ok — 412 passed`), not silence — the agent needs the confirmation.
- Guidance: `GRAPH_GUIDANCE` + OpenCode instructions addendum — *"prefer
  `run_check` with your project's test check over running the test command in
  Bash; it returns failures only."*
- No new settings: `checks` is already the per-project config surface
  (`.cimp/config.json`); users add
  `{ name: "test", cmd: "cargo test", parser: "cargo-test" }`.

### Tests
Fixture transcripts per parser: failures extracted with location + truncated
output; pass-run yields counts-only; mixed output (compile error before
tests) still parses; group dedup on repeated identical assertion failures;
auto-check baseline flags a newly failing test.

---

## Feature 5 — Tool-surface accounting (and a lean surface)

### Goal
Price the MCP tool surface itself. Every advertised tool's schema rides the
system prompt of **every** session — cache-written at the 2× rate once per
session and re-written on every cache expiry. Measure it honestly first (the
V14 rule: measured counters, never fabricated savings), then offer a lever.

### Design
- **Measurement** (`graph/mcp.rs`): a helper that serializes exactly what
  `tools()` (`:297`) currently advertises — per consumer, since gating
  differs (Claude/OpenCode MCP descriptors vs the offload worker's `ToolDef`s
  at `offload/tools/graph_tools.rs:22`) — and reports
  `{ tools: N, chars: X }`. Exposed over IPC; rendered on the
  Usage/Effectiveness card as one line: *"tool surface: N tools, ~X chars
  (≈X/4 tok est., cache-written once per session)"*. Labeled `est.` — the
  harness's exact schema rendering differs from our serialization.
- **Lean surface** (`GraphSettings.lean_tools: bool`, default false): when
  on, `tools()` omits a **static, documented** niche set — proposed:
  `graph_cycles`, `graph_dead_exports`, `graph_struct_search`, `graph_path`,
  `graph_architecture` (confirm against real Activity per-tool counts before
  coding — see Decisions 4). Static rather than per-project-dynamic: a
  surface that churns per project would invalidate the agent's learned tool
  habits and complicate drift debugging. The list lives in one `const` next
  to `tool_specs()` and is named in the Settings hint and
  `docs/TOKEN-EFFICIENCY.md`.
- **Advisor rule** `surface.lean.v1` (warn-and-propose, standard dismiss +
  apply cooldown): fires when the Activity store shows zero calls to every
  hideable tool across ≥ N recent sessions (proposed N=10) while `lean_tools`
  is off; proposal action sets `lean_tools: true`. The measured line above
  gives the user the chars saved, before and after.
- **Editorial pass** (one-time, ships with the feature): tighten the wordiest
  `tool_specs()` descriptions; the before/after delta is visible in the same
  counter, which keeps the exercise honest.

### Tests
Serialization helper counts match the advertised JSON; `lean_tools` hides
exactly the const set and nothing else; dispatch still *accepts* a hidden
tool by name (hiding is advertisement-only — an agent with stale habits gets
an answer, not an error); advisor rule fires on the zero-usage signal and
respects dismiss/cooldown.

---

## Feature 6 — Graduation rules (the V11 promise, made executable)

### Goal
V11 decision 2 deferred `read_advisor`'s default to field data; V14/V16 built
the evidence. Turn it into per-project, propose-and-confirm Advisor rules —
no silent default flips.

### Design (`advisor.rs`, standard `Proposal` plumbing)
- **`adopt.read_advisor.v1`** — fires when: `read_advisor` is **off**, E1 is
  verified OK (`harness_versions.e1_status` — same gate the overlay honors),
  and session memory shows the waste the advisor exists to stop: ≥ N
  redundant large re-reads per recent session on average (proposed N=3 over
  the last 10 sessions). Detection without the hook: `mem_event` read rows
  (the V10 transcript-tap feed) — same file, same session, read ≥ 2× with no
  intervening edit event, file ≥ `read_advisor_min_lines` (new
  `GraphIndex::redundant_read_candidates(...)` aggregate, a sibling of
  `advisor_reread_rate:3422`). Labeled `est.` — without content hashes this
  is an approximation (external tools may have changed the file between
  reads); the message says so. Proposal action: enable `read_advisor`.
- **`adopt.read_advisor_substitute.v1`** — fires when: mode is `advise`,
  `advisor_reread_rate` is **low** (reminders are sufficient — the agent
  rarely follows one with a full re-read; proposed rate ≤ 0.2 with ≥ 20
  samples), and bypass rate is below the V16 `BYPASS_HIGH` threshold.
  Proposal action: set `read_advisor_mode: "substitute"`. This is the exact
  graduation rule the V11 spec described in prose; it complements (and can
  never fire together with) the existing high-reread-rate rule that raises
  `read_advisor_min_lines`.
- Both use standard per-rule dismiss memory and the V16 apply cooldown; both
  render on the existing Advisor card. No auto-apply — house posture.

### Tests
Threshold matrices for both rules (below-threshold silence, E1-fail
suppression, dismiss + cooldown honored); mutual exclusion with the existing
reread-rate rule; `redundant_read_candidates` counts pairs correctly (edit
between reads breaks the pair).

---

## Phasing

| Phase | Scope | Notes |
|---|---|---|
| **A. Diff-substitute** | Snapshot store + diff branch + re-arm/cap + accounting | Adds the diff dependency; core of the milestone |
| **B. Shell interception** | Shared command parser + `Bash` matcher + shim dispatch | Depends on A only for the diff branch; parser extraction de-risks the V16 tap too |
| **C. First-read tier** | Non-code check + digest-backed remind + setting | Independent of A/B mechanics; same verdict fn |
| **D. Test parsers** | `CargoTest` + `JestJson` + guidance | Fully independent; can ship any time |
| **E. Surface accounting** | Measurement + Usage line + `lean_tools` + `surface.lean.v1` | Fully independent |
| **F. Graduation rules** | `adopt.*` rules + `redundant_read_candidates` | Independent of A–C (judges V11 behavior) |
| **G. Docs/tests/release** | README/FEATURES/TOKEN-EFFICIENCY/MAINTENANCE, Settings UI, CHANGELOG | Per repo convention |

Suggested order **A → B → C** (one coherent advisor release), then **D / E /
F in any order** (independent), then **G**. No graph.db schema bump anywhere;
no new hooks, routes, or CLI subcommands (Feature 2 reuses `--read-hook` and
`/context/should_read`).

## Decisions — OPEN

1. **Remind re-arm semantics (Feature 1)** — proposed: a content change
   re-arms a reminded file, capped at `read_advisor_remind_cap = 3` per file
   per session; immediate second ask on unchanged content always passes.
   Alternative (stricter V11 continuity): diff reminds share the single
   remind-once slot — simpler, but neuters the feature for edit-verify loops.
2. **Diff dependency** — proposed: `similar` (pure Rust, no C-FFI, MIT/Apache,
   mature) for unified line diffs; alternative is a hand-rolled LCS (~150
   lines) if the dependency feels heavy for one call site.
3. **First-read remind without a digest (Feature 3)** — proposed: require a
   cached digest (head/tail alone may genuinely not be enough for a log the
   agent has never seen). Alternative: allow head/tail-only reminds at a
   higher size threshold.
4. **Lean-tools hidden set (Feature 5)** — confirm the proposed five against
   real per-tool Activity counts on this machine before freezing the const.
5. **`cargo test` parsing** — proposed: stable text output (nightly-only JSON
   rejected); revisit if `--format json` stabilizes. `cargo nextest` support
   deferred until someone configures it.

## Cost note

Phases B–E are mechanical (Sonnet/Haiku fan-out fine). Reserve Opus for
Phase A's verdict/re-arm semantics (the one place a wrong denial could
mislead an agent), the Feature 2 parser's accept/reject boundary, and review
— per the standing agent-cost guidance.
