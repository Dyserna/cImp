# Harness capability matrix + adapter contract (draft)

**Status:** draft, not implemented. Companion to
`docs/DESIGN-harness-drift-canaries.md` — the matrix declares what we depend
on; the canaries prove it still holds. Fold both into a V35 milestone if
adopted.

**Problem being solved:** cImp rides two user-installed, aggressively
self-updating CLIs it does not pin. V16 built real drift *detection*, but the
knowledge of **what we depend on** lives in three disconnected places — the
prose table in `MAINTENANCE.md` § *Claude Code / OpenCode CLIs*, the
`harness_versions.*` spike strings, and constants scattered across
`advisor.rs`, `toolclass.rs`, `tabs/config.rs`. Nothing links a contract row
to the code that depends on it, so:

- there is no way to ask "which features are degraded right now, and why";
- a new feature can take a fragile dependency with no record anywhere;
- `drift.harness_version.v1` fires on *every* CLI auto-update whether or not
  anything actually broke, which trains reflexive "Mark verified" clicks —
  alarm fatigue on the one control that guards all the others;
- the MAINTENANCE table can drift from the code silently (several rows
  already say the failure symptom is literally "Silent").

## 1. The core idea: rank dependencies by seam, not by feature

Every harness dependency cImp holds sits in one of four tiers. The tier — not
the feature — predicts how it breaks and how much it costs to fix.

| Tier | Seam | Breaks how | Blast radius | Examples |
|---|---|---|---|---|
| **A** | MCP protocol | Loudly, versioned, multi-vendor | One tool | `graph_*`, `run_check`, `offload_task`, `context_*` |
| **B** | Documented hook / flag / settings key | At the payload boundary, usually announced | One shim file | `UserPromptSubmit`, `PostToolUse`, `--settings`, `--session-id` |
| **C** | Emitted artifact (not an API) | **Silently**, as zeros and empties | Several modules | transcript JSONL, statusline stdin, OpenCode SSE |
| **D** | Scraped UI / undocumented behavior | Silently, on cosmetic upstream changes | Cross-cutting | TUI regex `"Esc to cancel · Tab to amend"`, `PreToolUse` timeout semantics |

Tier A has essentially never broken cImp. That is not luck — it is the tier.
Every painful adaptation to date has been C or D.

**The standing rule this matrix exists to enforce:** when an upstream release
makes it possible to move a dependency *down* the ladder (D→C→B→A), that
migration is the highest-value harness work available, and it should be
visible as a scheduled item rather than discovered by feel.

## 2. The registry

One machine-readable source of truth, `src-tauri/src/harness/contract.rs`,
replacing the prose table as the authority. The prose table stays, but is
*generated-checked* against this (see §5).

```rust
/// One thing cImp depends on from a harness it does not control.
pub struct Capability {
    /// Stable id, used by the Advisor, the canary suite, and the UI.
    /// Never renamed — it is the join key across all three.
    pub id: &'static str,              // "claude.transcript.usage"
    pub harness: Harness,              // Claude | OpenCode | Any
    pub tier: Seam,                    // A | B | C | D
    /// Human sentence: what upstream must keep doing.
    pub contract: &'static str,
    /// Exactly what we read/call. Machine-checkable where possible —
    /// these strings are what the canary asserts on (see companion doc).
    pub depends_on: &'static [Dep],
    /// Modules that break if this drifts. Keeps the matrix honest: adding a
    /// consumer without adding it here is caught by the seam test (§5).
    pub wired_in: &'static [&'static str],
    /// What cImp does when this is known-broken.
    pub degradation: Degradation,
    /// The V16 statistical rule that lags this, if any.
    pub drift_rule: Option<&'static str>,   // advisor::RULE_DRIFT_USAGE_FIELDS
    /// The leading canary that proves it, if any.
    pub canary: Option<&'static str>,       // canary id, companion doc §3
}

pub enum Dep {
    /// A JSON path in an emitted artifact, e.g. "message.usage.input_tokens".
    JsonPath(&'static str),
    /// A CLI flag that must still exist, e.g. "--session-id".
    Flag(&'static str),
    /// A settings/overlay key we write, e.g. "hooks", "statusLine".
    ConfigKey(&'static str),
    /// An HTTP route we call, e.g. "GET /experimental/tool/ids".
    Route(&'static str),
    /// A behavior no payload reveals — must be a spike, not a canary.
    Behavior(&'static str),
}

pub enum Degradation {
    /// Feature silently produces nothing. THE DANGEROUS ONE — every entry
    /// here needs either a canary or an explicit accepted-residual note.
    Silent,
    /// Feature turns itself off and says so in the UI.
    VisibleOff { user_message: &'static str },
    /// Gate fails closed: the dependent feature refuses to install/run.
    FailClosed,
    /// A fallback path covers it (name it, so we can test the fallback).
    Fallback { to: &'static str },
}
```

### 2.1 Seed rows

Extracted from the current code, not invented. This is the real dependency
surface as of `develop`:

| id | Tier | Depends on | Wired in | Degradation |
|---|---|---|---|---|
| `claude.hook.user_prompt_submit` | B | `hookSpecificOutput.additionalContext` | `context_hook.rs` | Silent |
| `claude.hook.precompact` | B | same, at compaction | `compact_hook.rs` | Silent (spike D0) |
| `claude.hook.pretooluse_deny` | B+D | `permissionDecisionReason` reaches **the model** | `read_hook.rs` | FailClosed (spike E1) |
| `claude.hook.posttooluse` | B | `tool_name`, `tool_input.file_path` | `postedit_hook.rs` | Silent |
| `claude.hook.notification` | B+D | flat *and* nested payload shape (docs ambiguous) | `notify_hook.rs` | Fallback → `perm.tui_scrape` |
| `perm.tui_scrape` | **D** | literal `"Esc to cancel · Tab to amend"` | `processing/permission.rs` | Silent |
| `claude.transcript.usage` | **C** | `message.usage.{input,output,cache_read_input,cache_creation_input}_tokens` | `oob/claude.rs::parse_usage_line` | Silent (zeros) |
| `claude.transcript.tool_result` | **C** | `type=="user"`, `tool_result`, `tool_use_id`, `is_error` | `oob/claude.rs` | Silent |
| `claude.transcript.identity` | **C** | `sessionId`, `version`, `isSidechain`, `isMeta` | `oob/claude.rs` | Silent |
| `claude.transcript.subagents` | **C** | `subagents/*.jsonl` layout (survived the Task→Agent rename once) | `oob/claude.rs::SubagentFile` | Silent |
| `claude.statusline.stdin` | **C** | `model.*`, `context_window.*`, `rate_limits` | `statusline/mod.rs` | Silent (blank widget) |
| `claude.flag.settings_overlay` | B | `--settings` accepts `hooks`, `statusLine`, `permissions` | `tabs/config.rs`, `settings/injection.rs` | Silent (**no test today**) |
| `claude.flag.session_id` | B | `--session-id` still exists | `tabs/config.rs` | VisibleOff |
| `opencode.sse.events` | **C** | `message.updated`, `message.part.{updated,delta}`, `session.{created,idle}`, `properties.{sessionID,messageID,partID}` | `oob/opencode.rs` | Silent |
| `opencode.route.push` | B | `POST /session/:id/message` + `noReply` (≥ 1.18.13) | `oob/opencode.rs::forward_push` | Silent |
| `opencode.route.noauth` | **D** | server still accepts unauthenticated localhost calls | `oob/opencode.rs` | VisibleOff (401 ⇒ auth landed) |
| `opencode.tool_registry` | **C** | `GET /experimental/tool/ids` ⊆ `OPENCODE_NATIVE_TABLE` | `offload/toolclass.rs` | **Silent — security-relevant** |
| `opencode.plugin.load_all` | **D** | every file in `.opencode/plugin/` loads; `tool.execute.before` vetoes via `throw` | `tabs/config.rs::opencode_plugin_source` | Silent |

Two things jump out of the seeded table and are worth acting on independently
of the rest of this design:

- **`opencode.tool_registry` is Silent *and* security-relevant.** It is an
  allowlist gate; a new upstream tool ships ungated and nothing fails. It is
  the strongest single argument for automating the canary — it already has a
  documented manual diff procedure that only runs when someone remembers.
- **`claude.flag.settings_overlay` has no test at all** (recorded as a V32
  accepted residual). It is Tier B — cheap to canary.

## 3. What the matrix is *for* — every entry needs a consumer

Per global principle 3, a registry nothing reads is a smell with extra steps.
Four consumers, all of which exist today in ad-hoc form:

1. **Feature gating.** Replaces bespoke checks like
   `HarnessVersions::e1_blocked()` with one `contract::gate(id)`. New
   fail-closed gates cost a table row, not a new settings field + a new
   frontend mirror (`harnessStatusBlocks` in `src/lib/settings/types.ts`).
2. **The Advisor.** One notice source (`drift.capability.v1`) carrying the
   capability id, replacing seven bespoke rule constants with per-rule
   thresholds. The existing statistical rules stay — they become *evidence
   attached to a capability* rather than standalone rules.
3. **A "Harness health" panel** in Settings: every capability, its tier, last
   canary result, last verified version. This is the screen that answers
   "what is actually broken right now", which today requires reading source.
4. **The canary suite** (companion doc) — `depends_on` is the assertion list.

## 4. The adapter contract

The matrix is per-harness by construction, which makes the harness-neutral
core fall out for free. Define what cImp needs from *any* harness:

```rust
pub trait HarnessAdapter {
    fn id(&self) -> Harness;
    /// Version as the harness reports it, for the tripwire.
    fn version(&self) -> Option<String>;
    /// Which capabilities this adapter can serve, and by what seam.
    fn capabilities(&self) -> &'static [Capability];

    // The five things cImp actually needs. Each returns Unsupported rather
    // than panicking, so a harness that cannot serve one degrades visibly.
    fn session_events(&self) -> Result<EventStream, Unsupported>;
    fn tool_gate(&self) -> Result<&dyn ToolGate, Unsupported>;
    fn approval(&self) -> Result<&dyn ApprovalSource, Unsupported>;
    fn usage(&self) -> Result<UsageStream, Unsupported>;
    fn inject(&self, ctx: Injection) -> Result<(), Unsupported>;
}
```

**This is deliberately not a rewrite.** `oob/claude.rs` and `oob/opencode.rs`
already *are* these adapters; the trait names the boundary they already
respect so that (a) a third harness is an additive file rather than a new set
of `match` arms scattered through the tree, and (b) "OpenCode cannot serve
`tool_gate` in mode X" becomes a value the UI can render instead of a
comment.

**Explicit non-goal:** do not unify the two adapters' internals. They read
genuinely different wire formats; a shared abstraction over JSONL-tailing and
SSE would be the kind of premature unification that makes *both* harder to
patch when one drifts. The contract is the seam, not the implementation.

## 5. Keeping the matrix honest

Three tests, following the existing repo pattern of asserting two sources of
truth agree (`checks/mod.rs:1016` and `graph/memory.rs:782` already
`include_str!` the TS types and diff them against Rust):

- **`matrix_matches_maintenance_doc`** — `include_str!("../../docs/MAINTENANCE.md")`,
  parse the drift table, assert row-for-row parity with the registry.
  Prose can no longer drift from code.
- **`every_silent_degradation_has_a_canary_or_a_waiver`** — a `Silent` row
  with neither a canary nor an explicit `accepted_residual` note fails the
  build. This is the rule that stops new fragile dependencies entering
  unrecorded.
- **`wired_in_paths_exist`** — every `wired_in` path resolves to a real file.
  Cheap, catches the matrix rotting during refactors.

## 6. Phasing

| Phase | Work | Value |
|---|---|---|
| **A** | `contract.rs` + seed rows + the three §5 tests | The list exists and cannot rot |
| **B** | Advisor + gating read from it; retire `e1_blocked()` and the frontend mirror | One source of truth for degradation |
| **C** | Settings → Harness health panel | "What is broken now" is answerable |
| **D** | `HarnessAdapter` trait wrapped around the existing two adapters | Third harness becomes additive |

Phase A is worth doing even if nothing else follows: the seeded table above
already surfaced two live gaps (the untested `--settings` overlay key set, and
the security-relevant silent tool-registry allowlist).

## 7. What this does *not* fix

The matrix is a leading indicator of **what we depend on**, not of **whether
it still works**. Tier D `Behavior` deps (does a `PreToolUse` timeout block?
does a deny reason reach the model?) cannot be canaried from a payload — they
need the spike recipes, which stay manual. The matrix's contribution there is
narrow but real: it makes the unverifiable ones *countable*, so an unrun
spike is a visible row rather than a `TODO` in a module doc.
