# IMPL-PLAN V17 — Token Efficiency II (Advisor Escalation & Output Dedup)

Companion to `docs/MILESTONE-V17-token-efficiency-2.md`. File-by-file build
plan. Open decisions **assumed at proposed defaults** (re-arm with cap 3;
`similar` crate for diffs; first-read remind requires a cached digest; the
proposed five lean-hidden tools pending the E0 activity check; `cargo test`
stable text output) — sections marked ⚠ change if a decision flips.

Phases: **A** (diff-substitute) → **B** (shell interception) → **C**
(first-read tier) → **D** (test parsers) → **E** (surface accounting) → **F**
(graduation rules) → **G** (docs/tests/release). A→B→C is one coherent
advisor release; D, E, F are independent and can land in any order or in
parallel.

Grounding anchors (verified against current `develop`, post-V16 1c34566):

- **Verdict path:** `GraphService::should_read` at `graph/service.rs:1556-1696`.
  Order today: gates → post-compaction pass → `reminded` short-circuit
  (`:1578`) → `read_seen` hash compare (`:1601-1626`, TTL re-stamp `:1611`,
  never-seen/changed `_` arm `:1616-1624`) → min-lines pass (`:1628`) →
  remind + accounting (`:1632-1695`).
- **Session state:** `read_seen: StdMutex<HashMap<(String,String),(String,u32)>>`
  (`(session,rel)` → `(hash,turn)`) at `service.rs:213`; `reminded:
  StdMutex<HashMap<String,HashMap<String,RemindMark>>>` at `:203`;
  `RemindMark { turn, ts_ms, chars, advice_chars, bypassed }` at `:316-336`;
  `InjectState.displaced_chars_total` at `:280-310`.
- **Advice rendering:** `graph/context.rs::read_advice:560` (outline +
  `Re-read with Read({file, offset, limit}) if you need the exact text.`;
  substitute body via `substitute_body:596`, capped `max_body_bytes/2`).
- **Shim:** `read_hook.rs` (89 lines) — stdin JSON → fields `tool_name`,
  `tool_input.file_path`, `session_id`, `cwd`, `tool_input.offset`; contract
  drift via `context_hook.rs::report_contract_drift:110`; early-return unless
  `tool_name == "Read"` (`read_hook.rs:57-59`); POSTs
  `{cwd, session_id, file_path, offset}` to `/context/should_read`
  (`post_loopback`, 600 ms timeout); deny = one-line
  `hookSpecificOutput{permissionDecision:"deny", permissionDecisionReason}`
  (`read_hook.rs:81-88`); fails open, always exit 0.
- **Route:** `offload/loopback.rs:349` → `handle_should_read:752`;
  `ShouldReadBody { cwd, session_id, file_path, offset }` at `:736-746`.
- **Overlay:** `tabs/config.rs:291-314` — `PreToolUse` entry `{matcher:
  "Read"}`, gated `graph.enabled && graph.read_advisor &&
  !harness_versions.e1_blocked()`; `e1_blocked` → `status_blocks`
  (`settings/schema.rs:356-376`, fails closed); gating test
  `read_hook_overlay_gated_on_toggle_and_e1_status` at `config.rs:963`.
- **Bypass tap:** `GraphService::check_bypass:1711` (window `BYPASS_TURNS=3`
  / `BYPASS_MS=5min`), tokenizer `path_like_tokens:2862` — **deliberately
  NOT a shell parser** (heuristic: quoted spans + slash-bearing tokens), so
  the spec's "reuse/extract the V16 parser" resolves to *writing the strict
  parser new* and giving the tap a skip-guard, not extraction (see B1).
- **Checks:** `checks/mod.rs` — `CheckDef:33` `{name, cmd, parser,
  timeout_secs}`, `ParserKind:60` (`#[serde(rename_all = "kebab-case")]`,
  variants CargoJson/Tsc/EslintJson/Pytest/GenericGcc), `Diag:100`
  `{severity, code, message, file, line, col}`, `run:152` (spawn_capture 1 MB
  caps → `parsers::parse` → `group:196` keyed
  `(severity, code, normalized_message)` → changed-only filter →
  `cap_sites:227` MAX_SITES=5); `checks/auto.rs::diff_groups:68`
  (baseline = key→count; keeps new-or-worsened). Parser dispatch
  `checks/parsers.rs:21-29`; `parse_pytest:207` is the text-parser sibling
  (folds the tail counts line in as a file-less `Note`); zero groups renders
  `"No diagnostics."` (`graph/mcp.rs:825-828` in `fmt_check_report:817`).
  Parser tests: inline `const` fixtures in `parsers.rs` `mod tests:311-509`.
- **Tool surface:** single source `graph/mcp.rs::tool_specs():33` (21 base
  specs, `GraphToolSpec {name, description, parameters}` at `:25`);
  advertisement `tools():297-326` (settings-gated only — no consumer arg);
  dispatch is name-driven and independent of advertisement
  (`handle_call:354` → `dispatch_recorded:396` → `run_tool:512`, unknown ⇒
  error at `:675`). Offload worker: `offload/tools/graph_tools.rs::defs():21`
  wraps the same `tool_specs()` into OpenAI-shape `ToolDef`s (no `run_check`);
  superset-by-name test at `graph_tools.rs:53-78`.
- **Aggregates:** `GraphIndex::advisor_reread_rate` at `index.rs:3412-3454`
  (over `mem_event`; returns `Option<(rate, samples)>`); `mem_event` relation
  at `index.rs:2833` (`{session_id, seq => kind, path, symbol?, line?, ts_ms,
  detail?}`; kinds `read`/`edit`/`query`/`remind`);
  `GraphService::bypass_rate:1878` (Activity-ring based). `digest` relation
  at `index.rs:2841` (`{file, content_hash => text, ts_ms}`), accessors
  `get_digest:2208`/`put_digest:2221`; enqueue `enqueue_digest:1405`
  (MAX_INFLIGHT=32) → `compute_and_cache_digest:1432` →
  `OffloadSupervisor::run_internal`; today's only lookup consumer:
  `graph/context.rs:378-388` (injection fallback for outline-less files).
- **Advisor:** `advisor.rs` — `Signals:147` / `Proposal:218`
  (`{setting, current, proposed, rationale, rule_id, signature, warn_only,
  action}`) / `evaluate:265` (drift rules unconditional; tuning rules behind
  `MIN_SESSIONS=5`; apply-cooldown strip via `in_apply_cooldown:291`,
  `APPLY_COOLDOWN_SESSIONS=3`, state `Settings::advisor_applied`
  `schema.rs:481`); dismiss = `(rule_id, signature)` with `bucket10:247`;
  `BYPASS_HIGH=0.4`, `REREAD_HIGH=0.5`, `MIN_REMINDS=20`.
- **Usage plumbing:** IPC `graph_usage` at `ipc/commands.rs:1692-1712`
  (fills `offload_local_tasks` outside `GraphService` — the precedent for
  cross-cutting fields); `UsageSnapshot`/`Effectiveness` at
  `graph/memory.rs:295-329`; Effectiveness card
  `lib/CodeIntelligenceView.svelte:887-924` (counters `:891-922`); Advisor
  card is generic (`:1033-1085`), Apply switch `applyProposal:441-467`,
  manual `ADVISOR_RULES_TOOLTIP:546-555`. Advisor signal wiring:
  `commands.rs:1738-1749`.
- **Settings:** `GraphSettings` at `settings/schema.rs:1200-1412`
  (struct-level `#[serde(default)]`; defaults in `impl Default:1428+`) —
  current advisor fields `read_advisor=false:1304`,
  `read_advisor_min_lines=300:1307`, `read_advisor_mode="advise":1310`,
  `read_advisor_ttl_turns=0:1316`. **No schema-version bump needed** for
  additive defaulted fields (confirmed convention; `CURRENT_SCHEMA_VERSION=21`
  is for reshapes only). UI: `SettingsApp.svelte:4334-4410` (advisor block,
  `e1Blocked`-gated); TS mirror `lib/settings/types.ts:824-829` + defaults
  `:1409-1412`. `lean_tools` confirmed absent repo-wide. `similar` confirmed
  absent from `Cargo.lock` (genuinely new dependency).

Schema note: **no `graph.db` bump anywhere** — Phases A–C are in-memory
session state; F reads existing `mem_event`; C reads existing `digest`. No
new hooks, routes, or CLI subcommands (B reuses `--read-hook` +
`/context/should_read`).

---

## Phase A — Diff-substitute for changed-file re-reads

**A1. Snapshot store** (`graph/service.rs`): replace `read_seen`'s value
tuple with a named struct:

```rust
struct ReadSeen {
    hash: String,
    turn: u32,
    snapshot: Option<Arc<str>>,   // None: small file, over-cap, or evicted
}
```

Constants (not settings): `SNAP_ENTRY_MAX: usize = 512 * 1024`,
`SNAP_TOTAL_MAX: usize = 16 * 1024 * 1024`. Snapshot kept only when the
content has ≥ `read_advisor_min_lines` lines and ≤ `SNAP_ENTRY_MAX` bytes.
LRU: keep a running byte total + touch order (a `VecDeque<(String,String)>`
of keys or a monotonic counter on `ReadSeen`); on overflow drop
oldest-touched snapshots (set `snapshot: None`, keep hash/turn — eviction
must never forget the *observation*, only the content). Capture points —
every arm that stamps `read_seen` stores the snapshot from the `content` it
already holds: the never-seen/changed `_` arm (`:1616-1624`), the TTL
re-stamp (`:1611-1613`), and the new diff-remind branch (A3). The existing
1024-entry clear at `:1619` is subsumed by the byte-budget LRU.

**A2. Dependency** ⚠ (`src-tauri/Cargo.toml`): add `similar` (default
features minus what's unneeded — `text` feature only). One call site:
`TextDiff::from_lines(old, new)` rendered as a unified diff with ~3 context
lines and a `-U`-style header naming the file. If review balks at the dep, a
hand-rolled LCS behind the same helper signature is the fallback (Decision
2) — isolate it as `fn unified_diff(old: &str, new: &str, rel: &str) ->
String` in `graph/context.rs` so the choice is swappable.

**A3. Verdict rework** (`should_read`, `service.rs:1556`): the `reminded`
short-circuit at `:1578` moves **below** the fs read + hash compare (the
re-arm rule needs the current hash; the fs read was already unconditional on
this path). New flow after gates/post-compaction/relativize:

1. Read + hash (as today, `:1589-1591`).
2. Look up `read_seen`:
   - **Never seen** ⇒ Phase C branch (C1) or record-and-pass with snapshot.
   - **Unchanged** ⇒ TTL check as today; then `reminded` check: already
     reminded ⇒ pass (the immediate-second-ask hatch — same file, same
     content, always passes); not reminded + ≥ min-lines ⇒ outline remind
     (existing path).
   - **Changed** ⇒ new branch: if `read_advisor_diffs` is on, a snapshot
     exists, and the rendered unified diff is ≤ 50% of the new content's
     length ⇒ **diff remind**; else record-and-pass (re-stamping
     `read_seen` with the new hash + new snapshot). `read_to_string`
     already guarantees UTF-8 — binary/non-UTF-8 files never reach the
     branch (they fail the read at `:1590` and pass today; unchanged).
3. **Re-arm + cap:** `RemindMark` gains `count: u32`. A changed file that
   was already reminded may be reminded again (the old remind's "unchanged"
   promise is stale) **only while `count < READ_REMIND_CAP`** — `const
   READ_REMIND_CAP: u32 = 3` ⚠ (spec names it like a setting; keep it a
   const per the constants-not-settings posture, promote if field data
   demands). At cap ⇒ pass. An unchanged reminded file never re-reminds
   regardless of count.
4. **After a diff remind** the agent holds current content: update
   `read_seen` to `(new_hash, cur_turn, new_snapshot)`, bump
   `RemindMark.count`, re-stamp `RemindMark.turn`/`ts_ms` (the bypass
   window keys off these — a diff remind opens a fresh window).

Diff remind text (new `graph/context.rs::diff_advice`): header
`` `{rel}` changed since you read it (turn N) — diff against what you read: ``,
the unified diff, then the standard escape hatch line (same wording as
`read_advice:583` so drift canaries keyed on the reason text stay valid —
`drift.read_reason.v1` semantics are unchanged: a diff remind followed by a
full re-read still counts as a reread via the same `mem_event` join).

**A4. Accounting** — reuse the remind block (`:1639-1694`) unchanged:
`displaced` = full new-content chars, `advice_chars` = diff text chars;
`mem_event{kind:"remind"}`, `InjectState.displaced_chars_total`, and the
Activity record all get honest numbers for free. The Activity `request`
string gains a `diff` marker (e.g. ``"agent re-read of `{rel}` (changed —
diff substituted)"``) so the Effectiveness tooltip can split diff reminds
from outline reminds without a new field.

**A5. Settings + UI:** `GraphSettings.read_advisor_diffs: bool`, default
**true** in `impl Default` (`schema.rs:1428+`; struct-level
`serde(default)` handles old files — no migration). Mirror in
`types.ts:824-829` + defaults `:1409-1412`. `SettingsApp.svelte` advisor
block (`:4334-4410`): one checkbox inside the existing
`{#if snapshot.graph.read_advisor && !e1Blocked}` sub-block.

**A6. Tests** (`service.rs` tests + `context.rs` tests): changed file with
snapshot ⇒ diff remind + `read_seen` re-stamped to new hash; rendered diff
> 50% of new content ⇒ pass; snapshot evicted (`snapshot: None`) ⇒ pass;
re-arm fires on change, capped at 3, unchanged re-ask always passes;
post-compaction passes; LRU never exceeds `SNAP_TOTAL_MAX` and eviction
keeps hash/turn; `read_advisor_diffs: false` ⇒ changed reads pass as today;
diff text carries the escape-hatch line.

---

## Phase B — Shell-read interception

**B1. Strict command parser** (new: `graph/shellread.rs`, shared by shim
and tap — both live in the same crate, `read_hook.rs` is just another entry
point of the binary):

```rust
/// Some(path) iff `command` is provably a pure whole-file read of one file.
pub fn whole_file_read(command: &str) -> Option<String>
```

Accept: exactly one command (reject on any unquoted `|`, `>`, `<`, `;`,
`&&`, `||`, `` ` ``, `$(`), verb ∈ {`cat`, `type`, `Get-Content`, `gc`}
(case-insensitive for the PowerShell pair), flags tolerated: `-Raw` only;
exactly one path argument, no glob metacharacters (`*?[`), quotes stripped.
Everything else ⇒ `None` — interception must be provably equivalent to a
`Read`, never a guess. This is a **new** parser, not an extraction: the V16
tap's `path_like_tokens:2862` is a deliberate heuristic tokenizer and stays
as-is for broad bypass matching. `sed -n`, `head`, `tail` are deliberately
rejected (partial reads — the canary economics in B5 cover them).

**B2. Overlay** (`tabs/config.rs:291-314`): the `PreToolUse` array gains a
second entry `{matcher: "Bash", hooks: [same --read-hook command, timeout
5]}`, appended only when `settings.graph.read_advisor_shell` is also true
(full gate: `graph.enabled && read_advisor && read_advisor_shell &&
!e1_blocked()`). Zero overlay delta when the sub-toggle is off. No
`OVERLAY_BANNED_KEYS` interaction (that list guards `harness_versions` /
`llm_pricing` only).

**B3. Shim dispatch** (`read_hook.rs`): replace the `!= "Read"`
early-return (`:57-59`) with a match on `tool_name`:

- `"Read"` — unchanged, plus: also extract `tool_input.limit` and forward
  it (see B4 — Feature C needs to distinguish full reads from slices).
- `"Bash"` — extract `tool_input.command` (contract-drift report when
  missing, extending the tool-aware requiredness already in
  `read_hook.rs:42-55`); run `whole_file_read`; `None` ⇒ print nothing,
  exit 0 (composite commands pass untouched). `Some(path)` ⇒ resolve
  relative paths against the hook payload's `cwd`
  (`context_hook.rs::resolve_cwd:86`), POST the same
  `/context/should_read` body (`offset: None`, `limit: None`); on
  `remind`, emit the deny JSON with the reason prefixed
  `answered without running the command — ` (prefix applied shim-side; the
  server verdict stays tool-agnostic).
- anything else — early return (future-proof: the same shim may serve more
  matchers later).

Feature 1's diff branch applies automatically — the verdict path is shared.

**B4. Route/body** (`offload/loopback.rs:736-746`): `ShouldReadBody` gains
`limit: Option<u32>`; `handle_should_read:752` threads it into
`should_read` (new parameter). Verdict rule: **offset or limit present ⇒
the slice-read escape hatch** — today only `offset` is visible, so a
`Read({file, limit: 40})` head-peek looks like a full read; with `limit`
forwarded, Phase C's first-read branch (and the existing advice hatch) can
honor it precisely. Existing behavior for offset-only callers is
unchanged.

**B5. Canary interplay** (`GraphService::check_bypass:1711`): add a
skip-guard — when `read_advisor_shell` is on and
`whole_file_read(command)` matches, return before scoring: the command was
either intercepted (denied ⇒ the remind was already recorded by
`should_read`) or verdict-passed (⇒ not a bypass). Without the guard every
intercepted-and-denied `cat` would *also* count as a bypass (the tap sees
the denied `tool_use` in the transcript) and poison
`drift.read_bypass.v1`. The canary itself is untouched: with interception
live its rate should fall; a persistently high rate now points at a new
escape route (`sed -n`, `head`) — extend the `RULE_DRIFT_READ_BYPASS`
rationale string in `advisor.rs:512-541` to say so.

**B6. Settings + UI:** `GraphSettings.read_advisor_shell: bool`, default
**true** (same additive pattern as A5); checkbox in the same
Settings sub-block; `types.ts` mirror.

**B7. Tests:** `shellread.rs` unit matrix — accepted: `cat p`, `type p`,
`Get-Content p`, `gc -Raw p`, quoted paths with spaces; rejected: pipes,
`>`, two paths, globs, `&&`, `$(...)`, `sed -n 5,10p f`, `head -50 f`,
`cat a b`. Shim: relative path resolves against payload `cwd`; Bash deny
reason carries the prefix; missing `command` reports contract drift.
Service: intercepted read records a remind Activity event, not a bypass
(guard test on `check_bypass`); verdict parity — same file through `Read`
and through `cat` yields byte-identical advice modulo prefix. Overlay:
extend `read_hook_overlay_gated_on_toggle_and_e1_status`
(`config.rs:963`) to the 2×2×2 matrix (`read_advisor` ×
`read_advisor_shell` × `e1_status`) asserting the Bash matcher's presence
exactly when all gates hold.

---

## Phase C — First-read substitution for huge non-code files

**C1. Verdict branch** (`should_read`, in the never-seen arm from A3 —
evaluated *before* record-and-pass). Fires only when **all** hold:

- `read_advisor_first_read_kb > 0` and `content.len() >=` that threshold
  (bytes, from the already-read content);
- `offset.is_none() && limit.is_none()` (a deliberate slice always passes —
  this is what B4's `limit` forwarding buys);
- the file is **not code**: `idx.outline(&rel)` is empty — the same
  "no parsed symbols" test the injection fallback uses
  (`context.rs:378-388`), so data/logs/lockfiles qualify and source never
  does;
- `idx.get_digest(&rel, &cur_hash)` hits (the V11 relation, content-hash
  keyed — survives across sessions) ⚠ (Decision 3: digest required;
  head/tail-only reminds are out).

Cache miss on an otherwise-qualifying file ⇒ `self.enqueue_digest(root,
&rel, &cur_hash)` (`service.rs:1405` — existing bounds: MAX_INFLIGHT=32,
20 s deadline, ≤400-char validation) and **pass** — never-block posture;
protection starts on the second encounter.

**C2. Remind text** (new `graph/context.rs::first_read_advice`): byte +
line counts, the digest (≤3 lines by construction — `put_digest`'s 400-char
validation), first ~40 + last ~40 lines fenced, and the escape hatch
(`Read({file, offset, limit}) always passes`). Record via the same remind
block — `displaced` = full content chars, `advice_chars` = remind chars;
Activity `request` string marked `first-read` (same tooltip-splitting trick
as A4).

**C3. Interaction rules:** the reminded file enters `reminded` (remind-once
+ A3's re-arm-on-change apply), but `read_seen` stores **no snapshot**
(`snapshot: None` unconditionally for this branch — generated-file diffs
are useless and would blow the LRU); a later changed re-read just passes.
Post-compaction pass applies unchanged (checked before this branch).

**C4. Settings + UI:** `GraphSettings.read_advisor_first_read_kb: u32`,
default **0 = off** (a separate opt-in tier inside the advisor). Settings
UI: number input in the advisor sub-block with hint text proposing 256;
`types.ts` mirror.

**C5. Tests:** under-threshold / code file (outline non-empty) / digest
miss ⇒ pass, and the miss enqueues (assert via the inflight set or a mock
supervisor); cached digest ⇒ remind containing digest + head/tail + counts;
`offset` or `limit` present ⇒ pass; remind-once respected; snapshot not
retained; default-off (kb=0) short-circuits.

---

## Phase D — Test-run parsers for `run_check`

**D1. Variants** (`checks/mod.rs:60-79`): add to `ParserKind`:

```rust
/// `cargo test` — stable-toolchain text output (JSON is nightly-only).
CargoTest,
/// `jest --json` / `vitest --reporter=json` (same shape).
JestJson,
```

Kebab-case wire names `"cargo-test"` / `"jest-json"` come free from the
existing `rename_all`. Dispatch arms in `parsers.rs:21-29`: `CargoTest` →
`parse_cargo_test(&strip_ansi(stdout), &strip_ansi(stderr))` (line-oriented,
colored); `JestJson` → `parse_jest_json(stdout)` (JSON, no stripping —
matches the eslint-json precedent).

**D2. `parse_cargo_test`** (`checks/parsers.rs`, sibling of
`parse_pytest:207`):

- `test <name> ... FAILED` lines ⇒ one `Diag { severity: Error, message:
  "<name> failed", file: "", line: 0 }` — then **upgraded** when its
  `---- <name> stdout ----` block follows: capture the block (truncated to
  the first ~15 lines into `message`), and resolve `panicked at
  <file>:<line>:<col>` (or the older `, <file>:<line>:<col>` form) into
  `Diag.file`/`line` when present.
- Tail counts line (`test result: ok|FAILED. N passed; M failed; …`) ⇒ a
  file-less `Note` diag, exactly the pytest tail-line trick — this is what
  makes a clean run render as `ok — 412 passed`-style output instead of
  `"No diagnostics."` silence-with-no-counts ⚠ (Decision 5: stable text;
  `cargo nextest` deferred).
- **Mixed output:** a compile error aborts before any `test` lines —
  additionally run the input through the existing `parse_generic_gcc`
  line-matcher (rustc's human format is `error[E0308]: … --> file:line:col`;
  add the two-line `-->` form to the generic matcher or match it locally)
  and merge, so `run_check(name:"test")` on a broken build still surfaces
  the compile error instead of nothing.

**D3. `parse_jest_json`** (sibling of `parse_eslint_json`): parse stdout as
JSON (malformed ⇒ empty vec, never an error — module posture,
`parsers.rs:1-5`); for each `testResults[]` × `assertionResults[]` with
`status == "failed"`: `Diag { severity: Error, message: first ~5 lines of
failureMessages[0] (ANSI-stripped — jest embeds color codes inside JSON
strings), file: testFilePath }`. `testFilePath` is absolute — relativize
against the run cwd when it's a prefix (the `changed_only` filter compares
against `gitls::changed_files` relative paths; an absolute path would never
match). Top-level `numPassedTests`/`numFailedTests` ⇒ the counts `Note`.

**D4. Guidance** (`tabs/config.rs:479`, inside `GRAPH_GUIDANCE:466`): extend
the existing `run_check` clause with *"…including test runs — prefer a
configured test check over running the test command in Bash; it returns
failures only."* OpenCode inherits automatically
(`write_opencode_instructions:543` persists the same string). No new
settings — users add `{ "name": "test", "cmd": "cargo test", "parser":
"cargo-test", "timeout_secs": 300 }` to `.cimp/config.json`'s `checks`
(document the jest variant `"cmd": "npx jest --json", "parser":
"jest-json"` alongside).

**D5. Tests** (`parsers.rs mod tests:311+`, inline `const` fixtures per
convention): cargo-test happy path (2 failures with stdout blocks + panic
locations + tail line ⇒ 2 Errors with file:line + 1 Note); pass-run ⇒
counts Note only; compile-error-before-tests fixture ⇒ the rustc error
surfaces; stdout-block truncation at ~15 lines; jest fixture (nested
describe, ANSI in failureMessages) ⇒ Diags with relativized files + counts
Note; malformed JSON ⇒ empty. `checks::mod::tests`: repeated identical
assertion failures group (dedup key already normalizes quoted spans).
`checks/auto.rs` tests: a newly failing test vs a baseline ⇒ kept by
`diff_groups` (it's just a new key — no code change expected, test pins
it).

---

## Phase E — Tool-surface accounting + lean surface

**E0. Pre-coding check** ⚠ (Decision 4, one-off on this machine): aggregate
per-tool call counts from the Activity store (`tool-activity.jsonl` next to
the exe / `crate::activity::snapshot()` — group by `entry.tool`) and
confirm the proposed hidden five (`graph_cycles`, `graph_dead_exports`,
`graph_struct_search`, `graph_path`, `graph_architecture`) really are the
cold tail. Freeze the const from data, not the proposal.

**E1. Measurement helper** (`graph/mcp.rs`):

```rust
pub struct SurfaceStats { pub mcp_tools: usize, pub mcp_chars: usize,
                          pub offload_tools: usize, pub offload_chars: usize }
pub fn surface_stats() -> SurfaceStats
```

`mcp_*` = `serde_json::to_string(&tools()).len()` + count (what Claude and
OpenCode both receive — `tools()` takes no consumer arg, verified; one
number covers both); `offload_*` = the same over
`offload::tools::graph_tools::defs()` (different shape: OpenAI wrapper, no
`run_check`). Both computed post-`lean_tools`-filter so the before/after
delta is visible by toggling. Surface into `UsageSnapshot`
(`graph/memory.rs:295-329`) as a new `surface: SurfaceStats` field, filled
in the `graph_usage` IPC handler (`commands.rs:1692-1712`) beside
`offload_local_tasks` — the established spot for cross-cutting fields.

**E2. Usage line** (`CodeIntelligenceView.svelte`, Effectiveness card
`:887-924`): one row after the `offload_local_tasks` block (`:918-921`),
same `num`/`lbl` pattern: *"tool surface: N tools, ~X chars (≈X/4 tok
est., cache-written once per session)"*. Labeled `est.` — the harness's
exact schema rendering differs from our serialization.

**E3. Lean surface:** `GraphSettings.lean_tools: bool`, default false.
`const LEAN_HIDDEN: &[&str] = &[…five…]` lives next to `tool_specs()`
(`mcp.rs:33`). Filter applied in **`tools()`'s final map (`mcp.rs:321`) and
`graph_tools::defs()`'s final map (`graph_tools.rs:36`)** — never inside
`tool_specs()` itself, so `dispatch_recorded`/`run_tool`/`offload_query`
still answer hidden names (hiding is advertisement-only; an agent with
stale habits gets an answer, not an error). Note: the superset test at
`graph_tools.rs:53-78` compares `defs()` against `tool_specs()` — it must
pin `lean_tools=false` (or compare pre-filter) to stay meaningful. Settings
UI: toggle in the graph section whose hint names the five hidden tools;
also name them in `docs/TOKEN-EFFICIENCY.md`.

**E4. Advisor rule** (`advisor.rs`): `pub const RULE_SURFACE_LEAN: &str =
"surface.lean.v1"` — standard propose-with-Apply (not warn-only). Signals
additions: `hideable_tool_calls: u64` (calls to any `LEAN_HIDDEN` tool in
the Activity ring, computed in the `graph_usage_advice` wiring at
`commands.rs:1738-1749` by filtering `activity::snapshot()` on
`entry.tool`) and reuse of the existing `session_count`. Fires when
`hideable_tool_calls == 0 && session_count >= 10 && !graph.lean_tools`;
`setting: "graph.lean_tools"`, `current: "false"`, `proposed: "true"`,
signature `"zero-usage"` (a call to any hidden tool naturally silences it —
no rate bucketing needed). Rationale cites the measured chars from E1.
Standard dismiss + apply cooldown come free from `evaluate`'s existing
strip. Frontend: one `case 'graph.lean_tools':` in `applyProposal`
(`CodeIntelligenceView.svelte:441-467`) + a tooltip line
(`ADVISOR_RULES_TOOLTIP:546`).

**E5. Editorial pass** (one commit, ships with the feature): tighten the
wordiest `tool_specs()` descriptions (`mcp.rs:52-289`); record the
before/after `surface_stats()` numbers in the commit message — the counter
keeps the exercise honest.

**E6. Tests:** `surface_stats` counts equal the advertised JSON's
`len()`/count for both consumers; `lean_tools=true` hides exactly
`LEAN_HIDDEN` from `tools()` and `defs()` and nothing else;
`run_tool`/`offload_query` still dispatch a hidden name; rule matrix
(zero calls + ≥10 sessions fires; one call silences; dismiss + cooldown
honored; already-lean ⇒ silent).

---

## Phase F — Graduation rules (`adopt.*`)

**F1. Aggregate** (`graph/index.rs`, sibling of `advisor_reread_rate:3412`):

```rust
/// est. — (redundant same-file re-read pairs, distinct sessions scanned)
pub fn redundant_read_candidates(&self, min_lines: u32, last_sessions: usize)
    -> AppResult<Option<(u64, u64)>>
```

Over `mem_event`: group `kind == "read"` rows by `(session_id, path)`,
order by `ts_ms`; each consecutive same-file read pair with **no
intervening `kind == "edit"` row for that path in that session** counts as
one redundant pair. Size filter: `mem_event` carries no line counts —
resolve `path` against the current index/disk line count and keep files ≥
`min_lines`, labeled `est.` (the file may have changed since; the rule's
message says so). Restrict to the most recent `last_sessions` distinct
session ids (by max `ts_ms` per session).

**F2. Signals** (`advisor.rs:147` + wiring `commands.rs:1738-1749`): add
`redundant_reads_per_session: Option<f64>` + `redundant_read_sessions:
u64` (from F1, pairs ÷ sessions over the last 10), and `e1_pass: bool` —
**strictly** `harness_versions.e1_status` trimmed/lowercased `== "pass"`,
not merely `!e1_blocked()`: "verified OK" per the spec means proven, and
`unverified` must not auto-graduate a hook we've never seen work.

**F3. `adopt.read_advisor.v1`** (new `adopt_rules(sig)` called from
`evaluate:265`, behind the same `MIN_SESSIONS` floor as tuning rules):
fires when `!graph.read_advisor && e1_pass &&
redundant_reads_per_session >= 3.0` with `redundant_read_sessions >= 10`.
`setting: "graph.read_advisor"`, `proposed: "true"`, signature =
`bucket10(rate normalized)` or the pairs-per-session rounded — bucketed so
a materially changed rate re-fires past a dismissal. Rationale carries the
`est.` label and the "external tools may have changed the file between
reads" caveat verbatim. Also suppressed while any `drift.*` read rule is
firing (`advisor_disable_proposed` already computed in `evaluate:265` —
don't propose enabling what drift says is broken).

**F4. `adopt.read_advisor_substitute.v1`**: fires when `graph.read_advisor
&& read_advisor_mode == "advise" && advisor_reread_rate <= 0.2` with
`advisor_reread_samples >= 20` and `bypass_rate < BYPASS_HIGH` (0.4).
`setting: "graph.read_advisor_mode"`, `proposed: "substitute"`. Mutual
exclusion with `RULE_ADVISOR_LINES` is structural (that rule needs
`reread_rate >= REREAD_HIGH` = 0.5; the ranges can't overlap) — pin it
with a test, not a runtime guard.

**F5. Frontend:** `applyProposal` (`CodeIntelligenceView.svelte:441-467`)
gains `case 'graph.read_advisor':` (may already exist for the V16
bypass-disable proposal — verify; reuse if so) and
`case 'graph.read_advisor_mode':` (string-valued — first non-bool case;
the switch writes the proposed string as-is). Two `ADVISOR_RULES_TOOLTIP`
lines. Both rules render on the existing generic Advisor card — no new
components. No auto-apply — house posture; standard dismiss + V16 apply
cooldown come free.

**F6. Tests:** `redundant_read_candidates` — pair counting (read-read =
1, read-edit-read = 0, three reads = 2), session windowing, min-lines
filter; rule matrices (below each threshold ⇒ silence; `e1_status`
`"unverified"`/`"fail"` ⇒ silence for adopt.v1; drift-firing ⇒ silence;
dismiss signature re-fires on a changed bucket; apply cooldown);
mutual-exclusion pin (construct a `Signals` and assert no input fires both
`adopt.read_advisor_substitute.v1` and `RULE_ADVISOR_LINES`).

---

## Phase G — Docs, settings polish, tests, release

- `README.md` / `docs/FEATURES.md`: diff-substitute, shell interception,
  first-read tier (all under the read-advisor umbrella), test parsers (+
  the two `.cimp/config.json` recipes), tool-surface line + `lean_tools`,
  the two `adopt.*` rules.
- `docs/TOKEN-EFFICIENCY.md`: new sections per feature; the `LEAN_HIDDEN`
  list; the honest-numbers note for the diff/first-read Activity markers.
- `docs/MAINTENANCE.md`: the B5 canary-interplay note (`drift.read_bypass`
  now measures *residual* escape routes), the `similar` dependency, the
  snapshot-LRU constants, and the F2 `e1_pass`-vs-`e1_blocked` distinction.
- Settings UI final pass: the three new advisor sub-controls +
  `lean_tools`, `types.ts` mirrors, defaults tables.
- Full `cargo test` + frontend checks; live smoke per `MAINTENANCE.md`
  recipes (a real Claude tab: edit → re-read ⇒ diff remind; `cat` a
  reminded file ⇒ interception; `run_check` on a failing `cargo test`).
- CHANGELOG; version bump; release per the standard workflow
  (develop → main → tag).

---

## Appendix — consolidated change surface

**New settings** (all additive, `#[serde(default)]`, no schema bump):
`graph.read_advisor_diffs: bool = true`, `graph.read_advisor_shell: bool =
true`, `graph.read_advisor_first_read_kb: u32 = 0`, `graph.lean_tools:
bool = false`.

**New consts:** `READ_REMIND_CAP = 3`, `SNAP_ENTRY_MAX = 512 KiB`,
`SNAP_TOTAL_MAX = 16 MiB` (service.rs); `LEAN_HIDDEN` (mcp.rs);
`RULE_SURFACE_LEAN`, `RULE_ADOPT_ADVISOR`, `RULE_ADOPT_SUBSTITUTE` +
thresholds (advisor.rs).

**New dependency:** `similar` (⚠ Decision 2).

**New Rust files:** `graph/shellread.rs` (strict whole-file-read parser).

**Backend touches:** `graph/service.rs` (`ReadSeen` struct + LRU, verdict
rework, C branch, `check_bypass` skip-guard), `graph/context.rs`
(`unified_diff`, `diff_advice`, `first_read_advice`), `read_hook.rs` (Bash
dispatch + `limit`), `offload/loopback.rs` (`ShouldReadBody.limit`),
`tabs/config.rs` (Bash matcher + `GRAPH_GUIDANCE` addendum),
`checks/mod.rs` + `checks/parsers.rs` (two variants + parsers),
`graph/mcp.rs` (`surface_stats`, `LEAN_HIDDEN` filter, description
editorial), `offload/tools/graph_tools.rs` (lean filter),
`graph/index.rs` (`redundant_read_candidates`), `graph/memory.rs`
(`UsageSnapshot.surface`), `advisor.rs` (3 rules + `Signals` fields),
`ipc/commands.rs` (surface fill-in + signal wiring),
`settings/schema.rs` (4 fields + defaults).

**Frontend touches:** `SettingsApp.svelte` (advisor sub-controls +
`lean_tools`), `lib/settings/types.ts` (mirrors),
`lib/CodeIntelligenceView.svelte` (surface line, `applyProposal` cases,
tooltip lines).

**No new:** hooks, routes, CLI subcommands, graph.db relations, reserved
tabs. OpenCode interception (Feature 2) explicitly deferred to the V16
Phase G `tool.execute.before` spike — Claude-only until then, same
accepted asymmetry as V11 E.

**Cost note** (per the standing agent-cost guidance): B–E are mechanical
(Sonnet/Haiku fan-out fine); reserve Opus-class attention for Phase A's
verdict/re-arm semantics, the B1 accept/reject boundary, and review.
