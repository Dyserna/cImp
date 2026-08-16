//! V35 Phase A — the machine-readable harness capability registry.
//!
//! One source of truth for everything cImp depends on from a harness it does
//! not control, replacing the prose table in `docs/MAINTENANCE.md` §
//! *Claude Code / OpenCode CLIs* as the **authority**. The prose table stays
//! (it carries the human "how to check" narrative) but is now
//! generated-checked against this list by
//! [`tests::matrix_matches_maintenance_doc`], so the two can no longer drift.
//!
//! # Rank by seam, not by feature
//!
//! Every dependency sits in one of four [`Seam`] tiers, and the tier — not the
//! feature — predicts how it breaks:
//!
//! | Tier | Seam | Breaks how | Blast radius |
//! |---|---|---|---|
//! | **A** | MCP protocol | loudly, versioned, multi-vendor | one tool |
//! | **B** | documented hook / flag / settings key | at the payload boundary, usually announced | one shim file |
//! | **C** | emitted artifact (not an API) | **silently, as zeros and empties** | several modules |
//! | **D** | scraped UI / undocumented behavior | silently, on cosmetic upstream changes | cross-cutting |
//!
//! Tier A has essentially never broken cImp — which is why no seeded row
//! carries [`Seam::A`]: `graph_*`, `run_check`, `offload_task` and `context_*`
//! ride MCP and are not part of this surface. Every painful adaptation to date
//! has been C or D. The standing rule (milestone locked decision 2): when an
//! upstream release makes it possible to move a dependency **down** the ladder
//! (D→C→B→A), that migration outranks new harness features.
//!
//! # Reading a row
//!
//! - [`Capability::id`] is the join key across the Advisor, the canary suite
//!   and the future UI. **Never renamed** once it lands.
//! - [`Capability::depends_on`] is the assertion list a canary will be built
//!   from (Phase B onward). A [`Dep::Behavior`] entry is the marker of an
//!   unverifiable contract: no payload reveals it, so it needs a manual spike,
//!   not a canary. A Tier-B row that carries one has a **D-component** — that
//!   is how `claude.hook.pretooluse_deny` (spike E1),
//!   `claude.hook.precompact` (spike D0) and `opencode.plugin.load_all` (the
//!   OpenCode-veto spike) stay countable rather than living as `TODO`s.
//! - [`Capability::degradation`] says what cImp does when the row is
//!   known-broken. [`Degradation::Silent`] is the dangerous one, which is why
//!   every `Silent` row must carry a canary, a live probe or an explicit
//!   [`Capability::waiver`]
//!   ([`tests::every_silent_degradation_has_a_canary_or_a_probe_or_a_waiver`]).
//! - [`Capability::probe`] is the L2 half of that coverage: the L1 canary asks
//!   "do we still parse the shape we recorded", the probe asks "is the recorded
//!   shape still real". Set only where [`crate::harness::probe`] actually
//!   drives the installed CLI.
//! - [`Capability::controls`] is the **TCB column** (milestone locked decision
//!   10): a row marked with a control id does not merely *carry data* for a
//!   security control, it *is* where the control executes. Documentation, not
//!   a gate — but a reviewer changing such a row is changing the trusted
//!   computing base.
//!
//! # The first runtime consumer arrived in Phase D
//!
//! Phases A–C had none on purpose — the tests below and in
//! [`crate::harness::canary`] were the consumers. V35 Phase D adds the first
//! real one: [`crate::harness::probe`], reached from `cimp --harness-canary`,
//! reads [`CAPABILITIES`] to decide what to drive and what to enumerate as
//! `unknown`. Advisor wiring is still Phase E; nothing here reads Settings or
//! reaches the frontend yet.

// Most of the registry is still declaration-only: the Advisor + gating rewrite
// (E) and the Harness-health panel (G) are the remaining consumers it was
// seeded for. Same pattern (and same reason) as `graph/model.rs`.
#![allow(dead_code)]

use crate::advisor::{
    RULE_DRIFT_HOOK_SILENT, RULE_DRIFT_INJECTION_UNSEEN, RULE_DRIFT_PAYLOAD, RULE_DRIFT_READ_BYPASS,
    RULE_DRIFT_READ_REASON, RULE_DRIFT_SUBAGENT, RULE_DRIFT_USAGE_FIELDS,
};

/// Which harness serves a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    OpenCode,
    /// Harness-neutral — served by whatever adapter is attached. No seeded row
    /// is neutral yet; CHP (milestone decision 9) is what will produce them.
    Any,
}

/// The seam a dependency sits in. See the module docs for what each predicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seam {
    A,
    B,
    C,
    D,
}

/// Exactly what cImp reads or calls. Machine-checkable where possible — these
/// strings are what a canary asserts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dep {
    /// A JSON path in an emitted artifact, e.g. `message.usage.input_tokens`.
    JsonPath(&'static str),
    /// The on-disk *layout* of an emitted artifact tree, e.g.
    /// `<session_id>/subagents/agent-*.jsonl`. Split out of [`Dep::JsonPath`]
    /// in V35 Phase B: a layout is fixture-checkable in exactly the way
    /// [`Dep::Behavior`] is not, so it stays on this side of the line — but it
    /// is a *path in a filesystem*, not a path into a JSON document, and a
    /// canary asserts it by walking a directory rather than by indexing a
    /// `Value`.
    FilePath(&'static str),
    /// A CLI flag that must still exist, e.g. `--session-id`.
    Flag(&'static str),
    /// A settings/overlay/plugin key we write, e.g. `hooks`, `statusLine`.
    ConfigKey(&'static str),
    /// An HTTP route we call, e.g. `GET /experimental/tool/ids`.
    Route(&'static str),
    /// A behavior no payload reveals — must be a spike, not a canary.
    Behavior(&'static str),
}

/// What cImp does when a capability is known-broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degradation {
    /// Feature silently produces nothing. THE DANGEROUS ONE — every entry here
    /// needs either a canary or an explicit [`Capability::waiver`].
    Silent,
    /// Feature turns itself off and says so in the UI.
    VisibleOff { user_message: &'static str },
    /// Gate fails closed: the dependent feature refuses to install/run.
    FailClosed,
    /// A fallback path covers it (named, so the fallback itself can be tested).
    Fallback { to: &'static str },
}

/// One thing cImp depends on from a harness it does not control.
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    /// Stable id, used by the Advisor, the canary suite and the UI. Never
    /// renamed — it is the join key across all three, and across the
    /// `MAINTENANCE.md` drift table.
    pub id: &'static str,
    pub harness: Harness,
    pub tier: Seam,
    /// Human sentence: what upstream must keep doing.
    pub contract: &'static str,
    /// Exactly what we read/call — the canary's assertion list.
    pub depends_on: &'static [Dep],
    /// Repo-relative paths of the modules that break if this drifts. Adding a
    /// consumer without adding it here is what [`tests::wired_in_paths_exist`]
    /// keeps honest during refactors.
    pub wired_in: &'static [&'static str],
    pub degradation: Degradation,
    /// The V16 statistical rules that lag this row, if any. Always the
    /// `crate::advisor::RULE_DRIFT_*` constants — never a duplicated literal,
    /// since those constants are the ids the Advisor actually emits.
    pub drift_rule: &'static [&'static str],
    /// The leading canary that proves it, if any. **A canary id IS the
    /// capability id** — never a third namespace, so the registry, the suite in
    /// [`crate::harness::canary`] and the Advisor all join on one key. Phase B
    /// filled in the four Tier-C readers; the rest land in Phases C–D.
    pub canary: Option<&'static str>,
    /// The **L2 live probe** that drives this row against the installed CLI, if
    /// any (V35 Phase D). Same join-key rule as [`Capability::canary`]: a probe
    /// id IS the capability id, never a third namespace.
    ///
    /// Set only for rows [`crate::harness::probe`] actually *drives*. A row the
    /// probe merely enumerates as a permanent `Unknown` (the scripted-turn
    /// class, and the Tier-D behaviors no probe can settle) stays `None` and
    /// lives in `probe::DECLARED_UNPROBED` instead — counting a
    /// permanent-`Unknown` emitter as coverage is exactly the "quality signal
    /// with no consumer" this registry exists to prevent.
    pub probe: Option<&'static str>,
    /// An accepted-residual note: why this row has no canary *yet*, and what
    /// covers it meanwhile. Every [`Degradation::Silent`] row needs one until
    /// its canary exists — that is what lets the enforcement test run from day
    /// one and be tightened as canaries arrive, instead of arriving with them.
    pub waiver: Option<&'static str>,
    /// TCB column (milestone locked decision 10). Security controls that
    /// *execute inside* this capability rather than merely depending on it.
    /// Documentation, not a gate.
    pub controls: &'static [&'static str],
}

/// V32 Phase H native-tool containment: the enforcement `throw` in the
/// generated OpenCode plugin's `tool.execute.before`
/// (`tabs/config.rs`, ~line 2205). cImp only *computes* the verdict.
pub const CONTROL_TOOL_GATE: &str = "tool.gate";
/// V33 Phase F: the pre-mutation checkpoint trigger, taken inside the same
/// `tool.execute.before` handler, after the gate.
pub const CONTROL_CHECKPOINT_PRE_MUTATION: &str = "checkpoint.pre_mutation";
/// V32: the taint beacon the plugin posts for native web tools.
pub const CONTROL_TAINT_BEACON: &str = "taint.beacon";

/// Every control id that is declared somewhere in [`CAPABILITIES`]. Each must
/// appear exactly once — see [`tests::tcb_controls_are_declared_exactly_once`].
pub const CONTROLS: &[&str] = &[
    CONTROL_TOOL_GATE,
    CONTROL_CHECKPOINT_PRE_MUTATION,
    CONTROL_TAINT_BEACON,
];

/// The registry. Extracted from the code on `develop`, not invented.
pub const CAPABILITIES: &[Capability] = &[
    // ── Claude Code: documented hooks (Tier B) ──────────────────────────────
    Capability {
        id: "claude.hook.user_prompt_submit",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "A `UserPromptSubmit` hook receives `{prompt, session_id, cwd}` on stdin, and \
                   the `hookSpecificOutput.additionalContext` it writes to stdout is prepended to \
                   the user's prompt and reaches the model.",
        depends_on: &[
            Dep::ConfigKey("hooks.UserPromptSubmit"),
            Dep::JsonPath("prompt"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("hookSpecificOutput.hookEventName"),
            Dep::JsonPath("hookSpecificOutput.additionalContext"),
        ],
        wired_in: &["src-tauri/src/context_hook.rs", "src-tauri/src/tabs/config.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_INJECTION_UNSEEN, RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "V16 lags both halves: `drift.injection_unseen.v1` watches the follow-rate collapse, \
             and the `context_hook` shim reports a payload missing `session_id`/`cwd` as \
             `drift.payload.v1`. Both are lagging; the leading fixture check over the stdout \
             envelope lands in V35 Phase B.",
        ),
        controls: &[],
    },
    Capability {
        id: "claude.hook.precompact",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "A `PreCompact` hook receives `{session_id, trigger, cwd}` and its \
                   `hookSpecificOutput.additionalContext` reaches the *compaction prompt* — \
                   spike D0, outcome recorded in `harness_versions.d0_status`.",
        depends_on: &[
            Dep::ConfigKey("hooks.PreCompact"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("trigger"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("hookSpecificOutput.hookEventName"),
            Dep::JsonPath("hookSpecificOutput.additionalContext"),
            Dep::Behavior(
                "the additionalContext reaches the compaction prompt, not just the transcript \
                 (spike D0 — still `unverified`)",
            ),
        ],
        wired_in: &["src-tauri/src/compact_hook.rs", "src-tauri/src/tabs/config.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "The payload half is covered by the `compact_hook` shim's `drift.payload.v1` reports. \
             The behavior half is spike D0 and stays manual by milestone decision 7 — no payload \
             reveals whether a compaction prompt consumed the block. A D0 failure degrades to a \
             no-op (server-side dedup-clear stays correct regardless), so this is an accepted \
             residual rather than a scheduled canary.",
        ),
        controls: &[],
    },
    Capability {
        id: "claude.hook.pretooluse_deny",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "A `PreToolUse` hook can deny a tool call with \
                   `hookSpecificOutput.permissionDecision: \"deny\"`, and the accompanying \
                   `permissionDecisionReason` is surfaced **to the model** — not only to the \
                   user. Spike E1, recorded in `harness_versions.e1_status`.",
        depends_on: &[
            Dep::ConfigKey("hooks.PreToolUse"),
            Dep::JsonPath("tool_name"),
            Dep::JsonPath("tool_input.file_path"),
            Dep::JsonPath("tool_input.command"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("hookSpecificOutput.permissionDecision"),
            Dep::JsonPath("hookSpecificOutput.permissionDecisionReason"),
            Dep::Behavior(
                "the deny reason reaches the model and is acted on, rather than surfacing as a \
                 bare refusal (spike E1) — the D-component of this Tier-B row",
            ),
        ],
        wired_in: &["src-tauri/src/read_hook.rs", "src-tauri/src/tabs/config.rs"],
        degradation: Degradation::FailClosed,
        drift_rule: &[
            RULE_DRIFT_READ_REASON,
            RULE_DRIFT_HOOK_SILENT,
            RULE_DRIFT_READ_BYPASS,
            RULE_DRIFT_PAYLOAD,
        ],
        canary: None,
        probe: None,
        waiver: None,
        controls: &[],
    },
    Capability {
        id: "claude.hook.posttooluse",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "A `PostToolUse` hook fires for `Edit` / `Write` / `MultiEdit` with the \
                   documented payload (`tool_name`, `tool_input.file_path`, `session_id`, `cwd`) \
                   and accepts `hookSpecificOutput.additionalContext` back.",
        depends_on: &[
            Dep::ConfigKey("hooks.PostToolUse"),
            Dep::JsonPath("tool_name"),
            Dep::JsonPath("tool_input.file_path"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("hookSpecificOutput.hookEventName"),
            Dep::JsonPath("hookSpecificOutput.additionalContext"),
        ],
        wired_in: &["src-tauri/src/postedit_hook.rs", "src-tauri/src/tabs/config.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: None,
        probe: None,
        waiver: Some(
            "GAP, recorded rather than assumed: `postedit_hook.rs` is the ONE shim that never \
             calls `report_contract_drift`, so `drift.payload.v1` does NOT lag this row and no \
             other V16 rule does either — a matcher or field rename stops auto-check diagnostics \
             with nothing firing anywhere. Canary lands in V35 Phase B; owner is the V35 \
             milestone.",
        ),
        controls: &[],
    },
    Capability {
        id: "claude.hook.notification",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "`Notification` and `PermissionDenied` hooks fire (matcher `\"\"` = all types) \
                   with `{hook_event_name, session_id, cwd, transcript_path}` plus a type and a \
                   message. The type/message pair is read BOTH flat \
                   (`notification_type`/`message`) and nested (`notification.{type,message}`) \
                   because the upstream docs are ambiguous about which shape ships.",
        depends_on: &[
            Dep::ConfigKey("hooks.Notification"),
            Dep::ConfigKey("hooks.PermissionDenied"),
            Dep::JsonPath("hook_event_name"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("transcript_path"),
            Dep::JsonPath("notification_type"),
            Dep::JsonPath("message"),
            Dep::JsonPath("notification.type"),
            Dep::JsonPath("notification.message"),
            Dep::Behavior(
                "which of the flat and nested payload shapes the installed build actually sends \
                 — unverified since NC-2, which is why both are parsed; the D-component of this \
                 Tier-B row",
            ),
        ],
        wired_in: &[
            "src-tauri/src/notify_hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/tabs/config.rs",
        ],
        degradation: Degradation::Fallback {
            to: "perm.tui_scrape",
        },
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: None,
        controls: &[],
    },
    // ── Claude Code: scraped UI (Tier D) ────────────────────────────────────
    Capability {
        id: "perm.tui_scrape",
        harness: Harness::Claude,
        tier: Seam::D,
        contract: "Claude Code's approval prompt keeps the footer grammar `<chord> to cancel \
                   [· <chord> to amend] [· <chord> to explain]` with the cancel hint FIRST, and \
                   the bare-footer variant keeps the adjacent numbered options `1. Yes 2. …`; \
                   its select-menu chrome keeps saying `to select` / `to navigate` so the veto \
                   terms still subtract it. NOTE: the shipped patterns do **not** match the \
                   literal `Esc to cancel · Tab to amend` the prose docs still name — chord \
                   labels are user-remappable and the amend segment is conditional.",
        depends_on: &[
            Dep::Behavior(
                "the rendered tail contains `to cancel ·` — the cancel hint followed by another \
                 footer segment, which only happens when cancel comes first",
            ),
            Dep::Behavior(
                "the single-segment footer variant still renders `1. Yes 2.` on adjacent lines \
                 as the corroborating anchor",
            ),
            Dep::Behavior(
                "menu chrome keeps saying `to select` / `to navigate`, so the `none_of` veto \
                 still separates the question UI from an approval prompt",
            ),
        ],
        wired_in: &["src-tauri/src/processing/permission.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: None,
        probe: None,
        waiver: Some(
            "Tier D by construction — a scrape of rendered chrome that no payload canary can \
             prove. Mitigated instead of canaried: it is only the FALLBACK for \
             `claude.hook.notification`, the patterns are user-editable in \
             `<exe-dir>/patterns.json`, and `RUST_LOG=perm_capture=debug` re-characterizes it in \
             minutes. Accepted residual; the real fix is the D→C→B migration of decision 2.",
        ),
        controls: &[],
    },
    // ── Claude Code: emitted transcript artifact (Tier C) ───────────────────
    Capability {
        id: "claude.transcript.usage",
        harness: Harness::Claude,
        tier: Seam::C,
        contract: "Assistant transcript lines (`type == \"assistant\"`) carry `message.id`, \
                   `message.model` and a `message.usage` block whose four token counters keep \
                   their names.",
        depends_on: &[
            Dep::JsonPath("type"),
            Dep::JsonPath("message.id"),
            Dep::JsonPath("message.model"),
            Dep::JsonPath("message.usage.input_tokens"),
            Dep::JsonPath("message.usage.output_tokens"),
            Dep::JsonPath("message.usage.cache_read_input_tokens"),
            Dep::JsonPath("message.usage.cache_creation_input_tokens"),
        ],
        wired_in: &["src-tauri/src/oob/claude.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_USAGE_FIELDS],
        canary: Some("claude.transcript.usage"),
        probe: Some("claude.transcript.usage"),
        waiver: None,
        controls: &[],
    },
    Capability {
        id: "claude.transcript.tool_result",
        harness: Harness::Claude,
        tier: Seam::C,
        contract: "User-role transcript lines carry the preceding turn's tool results as \
                   `message.content[]` blocks with `type == \"tool_result\"`, `tool_use_id` and \
                   `is_error`, whose `content` is either a plain string or an array of \
                   `{type:\"text\", text}` blocks. Two readers, not one: \
                   `extract_tool_results` (usage accounting) never looks at `is_error` — that \
                   flag is read by `tool_result_is_error`, which keeps a FAILED tool result from \
                   being mined for commit hashes by the session→commit provenance tap.",
        depends_on: &[
            Dep::JsonPath("type"),
            Dep::JsonPath("message.content[].type"),
            Dep::JsonPath("message.content[].tool_use_id"),
            Dep::JsonPath("message.content[].is_error"),
            Dep::JsonPath("message.content[].content"),
            Dep::JsonPath("message.content[].content[].text"),
        ],
        wired_in: &["src-tauri/src/oob/claude.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: Some("claude.transcript.tool_result"),
        probe: Some("claude.transcript.tool_result"),
        waiver: None,
        controls: &[],
    },
    Capability {
        id: "claude.transcript.identity",
        harness: Harness::Claude,
        tier: Seam::C,
        contract: "Every transcript line carries a top-level `sessionId`; lines also carry \
                   `version` (the CLI build that wrote them, feeding the drift tripwire), \
                   `isSidechain` (sub-agent turns, inline contract) and `isMeta` (synthetic \
                   lines to skip).",
        depends_on: &[
            Dep::JsonPath("sessionId"),
            Dep::JsonPath("version"),
            Dep::JsonPath("isSidechain"),
            Dep::JsonPath("isMeta"),
        ],
        wired_in: &["src-tauri/src/oob/claude.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: None,
        probe: Some("claude.transcript.identity"),
        waiver: Some(
            "No L1 fixture canary (Phase B covered the four readers with a *function* to drive; \
             these four fields have no single reader). The V35 Phase D LIVE probe covers it \
             instead, reading `sessionId` through `oob::claude::record_names_session` and \
             `version` through `cli_version_of` on a real transcript tail. Deliberately NOT \
             linked to `drift.harness_version.v1`: that rule is *fed* by `version`, so losing the \
             field silences the tripwire instead of firing it — the inverse of a lagging \
             indicator, and the reason this row needs a leading check more than most.",
        ),
        controls: &[],
    },
    Capability {
        id: "claude.transcript.subagents",
        harness: Harness::Claude,
        tier: Seam::C,
        contract: "Sub-agent traffic is visible in one of the two known places: inline in the \
                   parent transcript as `isSidechain: true` lines, or as \
                   `<projects-root>/<session_id>/subagents/agent-*.jsonl`; and the launch is a \
                   `tool_use` block named `Task` (1.x) or `Agent` (2.x).",
        depends_on: &[
            Dep::JsonPath("isSidechain"),
            Dep::FilePath("<session_id>/subagents/agent-*.jsonl"),
            Dep::JsonPath("message.content[].type == \"tool_use\""),
            Dep::JsonPath("message.content[].name in {Task, Agent}"),
        ],
        wired_in: &["src-tauri/src/oob/claude.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_SUBAGENT],
        canary: None,
        probe: None,
        waiver: Some(
            "Canary lands in V35 Phase B (fixture L1 — a directory fixture, not a single file). \
             `drift.subagent_transcripts.v1` already lags it and has fired once for real (the \
             Task→Agent rename), which is the evidence this layout moves.",
        ),
        controls: &[],
    },
    // ── Claude Code: statusline stdin (Tier C) ──────────────────────────────
    //
    // `depends_on` was reconciled against the readers in V35 Phase C (Phase B
    // found the canary asserting fields the row never declared). Deliberately
    // still NOT declared, having been checked rather than assumed:
    //   * `extract_push_meta`'s `session_id` / `transcript_path` /
    //     `session_name` / `cost.total_api_duration_ms` / `cost.total_cost_usd`
    //     — attribution enrichment, not data. Losing every one of them leaves
    //     the push writing the same numbers; only the M14 multi-tab ownership
    //     of the shared context slot degrades to last-writer-wins, which
    //     `usage::merge_push` handles by design (`unwrap_or_default()` on the
    //     key) rather than by breaking.
    //   * `extract_context`'s `session_name` / `agent.name` / `effort` /
    //     `thinking` / `fast_mode` — display chips. They cannot make a snapshot
    //     substantive (`ContextSnapshot::is_substantive` ignores them) and
    //     `merge_push` drops a metadata-only snapshot, so their absence costs
    //     labels, never a reading.
    // Both are one honest step from load-bearing: if a later milestone makes
    // per-tab attribution or an agent-scoped reading depend on them, they move
    // into `depends_on` and the canary grows an assertion.
    Capability {
        id: "claude.statusline.stdin",
        harness: Harness::Claude,
        tier: Seam::C,
        contract: "The `statusLine` command's stdin JSON carries `model.display_name` (with \
                   `model.id` as the rendered fallback), a `context_window` block \
                   (`used_percentage`, `remaining_percentage`, `total_input_tokens`, \
                   `context_window_size`, plus a `current_usage` sub-block holding the four \
                   per-turn counters — tolerated hoisted to the block level, and the cache pair \
                   accepted under either the `*_input_tokens` or the shorter `*_tokens` \
                   spelling) and a `rate_limits` object whose `five_hour` / `seven_day` windows \
                   carry `used_percentage` and `resets_at`.",
        depends_on: &[
            Dep::JsonPath("model.display_name"),
            Dep::JsonPath("model.id"),
            Dep::JsonPath("context_window.used_percentage"),
            Dep::JsonPath("context_window.remaining_percentage"),
            Dep::JsonPath("context_window.total_input_tokens"),
            Dep::JsonPath("context_window.context_window_size"),
            Dep::JsonPath("context_window.current_usage.input_tokens"),
            Dep::JsonPath("context_window.current_usage.output_tokens"),
            Dep::JsonPath("context_window.current_usage.cache_read_input_tokens"),
            Dep::JsonPath("context_window.current_usage.cache_read_tokens"),
            Dep::JsonPath("context_window.current_usage.cache_creation_input_tokens"),
            Dep::JsonPath("context_window.current_usage.cache_creation_tokens"),
            Dep::JsonPath("rate_limits.five_hour.used_percentage"),
            Dep::JsonPath("rate_limits.five_hour.resets_at"),
            Dep::JsonPath("rate_limits.seven_day.used_percentage"),
            Dep::JsonPath("rate_limits.seven_day.resets_at"),
        ],
        wired_in: &["src-tauri/src/statusline/mod.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: Some("claude.statusline.stdin"),
        probe: None,
        waiver: None,
        controls: &[],
    },
    // ── Claude Code: spawn flags (Tier B) ───────────────────────────────────
    Capability {
        id: "claude.flag.settings_overlay",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "`claude --settings <json>` accepts a session-scoped overlay and honors the \
                   `hooks`, `statusLine` and `permissions` keys inside it, without cImp ever \
                   writing to `~/.claude`.",
        depends_on: &[
            Dep::Flag("--settings"),
            Dep::ConfigKey("hooks"),
            Dep::ConfigKey("statusLine"),
            Dep::ConfigKey("permissions"),
        ],
        wired_in: &[
            "src-tauri/src/tabs/config.rs",
            "src-tauri/src/settings/injection.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: None,
        probe: Some("claude.flag.settings_overlay"),
        waiver: Some(
            "Known live gap, recorded as a V32 accepted residual and re-raised by the V35 \
             milestone: there is NO test that the installed CLI still honors these keys, only \
             unit tests that cImp emits them. It is the whole delivery mechanism for the other \
             four Claude hook rows AND for native-web `deny`, so its silence is the widest of \
             any row here. Tier B and cheap to canary — scheduled for V35 Phase B/C.",
        ),
        controls: &[],
    },
    Capability {
        id: "claude.flag.session_id",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "`claude --session-id <uuid>` still exists and pins the process to one \
                   transcript file (V34), and the selectors cImp stands down for — `--resume`, \
                   `-r`, `--continue`, `-c`, `--fork-session`, `--from-pr` — keep their \
                   spellings, so cImp never hands the child two competing session selectors.",
        depends_on: &[
            Dep::Flag("--session-id"),
            Dep::Flag("--resume"),
            Dep::Flag("--continue"),
            Dep::Flag("--fork-session"),
            Dep::Flag("--from-pr"),
        ],
        wired_in: &["src-tauri/src/tabs/config.rs", "src-tauri/src/oob/claude.rs"],
        degradation: Degradation::VisibleOff {
            user_message: "Per-tab session identity is off: this Claude Code build did not accept \
                           `--session-id`. Tabs fall back to newest-transcript-wins binding, so \
                           two tabs on one project cannot be told apart.",
        },
        drift_rule: &[],
        canary: None,
        probe: Some("claude.flag.session_id"),
        waiver: None,
        controls: &[],
    },
    // ── OpenCode: SSE artifact (Tier C) ─────────────────────────────────────
    Capability {
        id: "opencode.sse.events",
        harness: Harness::OpenCode,
        tier: Seam::C,
        contract: "`GET /event` streams SSE envelopes `{type, properties}` carrying \
                   `message.updated`, `message.part.updated`, `message.part.delta`, \
                   `session.created` and `session.idle`, and every session-scoped event carries \
                   `properties.sessionID`.",
        depends_on: &[
            Dep::Route("GET /event"),
            Dep::JsonPath("message.updated"),
            Dep::JsonPath("properties.info.id"),
            Dep::JsonPath("properties.info.role"),
            Dep::JsonPath("properties.info.time.completed"),
            Dep::JsonPath("message.part.updated"),
            Dep::JsonPath("properties.part.id"),
            Dep::JsonPath("properties.part.type"),
            Dep::JsonPath("properties.part.messageID"),
            Dep::JsonPath("properties.part.text"),
            Dep::JsonPath("message.part.delta"),
            Dep::JsonPath("properties.messageID"),
            Dep::JsonPath("properties.partID"),
            Dep::JsonPath("properties.field"),
            Dep::JsonPath("properties.delta"),
            Dep::JsonPath("session.created"),
            Dep::JsonPath("session.idle"),
            Dep::JsonPath("properties.sessionID"),
        ],
        wired_in: &["src-tauri/src/oob/opencode.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: Some("opencode.sse.events"),
        probe: None,
        waiver: None,
        controls: &[],
    },
    // ── OpenCode: HTTP routes (Tiers B and D) ───────────────────────────────
    Capability {
        id: "opencode.route.push",
        harness: Harness::OpenCode,
        tier: Seam::B,
        contract: "`POST /session/:id/message` accepts a hand-built message envelope, and \
                   `noReply: true` (≥ 1.18.13) injects the text into the session **without** \
                   starting an agent turn.",
        depends_on: &[
            Dep::Route("POST /session/:id/message"),
            Dep::ConfigKey("noReply"),
        ],
        wired_in: &["src-tauri/src/oob/opencode.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: None,
        probe: None,
        waiver: Some(
            "Deferred past V35 Phase D, with the reason now measured rather than assumed: a push \
             is fire-and-forget, so a 4xx is logged and dropped, and the dangerous half is \
             `noReply` losing its meaning rather than the route disappearing — that would turn \
             every V30 fanout into a real agent turn. Proving it needs a real session to push \
             INTO plus an assertion that no turn started, i.e. the scripted-turn probe class \
             Phase D deliberately did not fake. The probe enumerates this row as a permanent \
             `unknown` (`probe::DECLARED_UNPROBED`) so it is counted rather than omitted.",
        ),
        controls: &[],
    },
    Capability {
        id: "opencode.route.noauth",
        harness: Harness::OpenCode,
        tier: Seam::D,
        contract: "OpenCode's local HTTP server still accepts unauthenticated localhost calls on \
                   `GET /event`, `GET /session/:id` and `POST /session/:id/message`. \
                   Double-edged, and deliberately recorded as a capability rather than a bug: if \
                   a release adds auth the tap and push break, and until then the unauthenticated \
                   server is a localhost exposure.",
        depends_on: &[
            Dep::Route("GET /event"),
            Dep::Route("GET /session/:id"),
            Dep::Route("POST /session/:id/message"),
            Dep::Behavior("the server serves these routes with no Authorization header sent"),
        ],
        wired_in: &["src-tauri/src/oob/opencode.rs"],
        degradation: Degradation::VisibleOff {
            user_message: "OpenCode's local server now requires authentication — the live session \
                           tap and the V30 push fanout are off until a token is wired.",
        },
        drift_rule: &[],
        canary: None,
        probe: Some("opencode.route.noauth"),
        waiver: None,
        controls: &[],
    },
    // ── OpenCode: tool registry + plugin (Tiers C and D) ────────────────────
    Capability {
        id: "opencode.tool_registry",
        harness: Harness::OpenCode,
        tier: Seam::C,
        contract: "Every tool id `GET /experimental/tool/ids` returns on the running binary is \
                   present in `offload::toolclass::OPENCODE_NATIVE_TABLE`. The table is \
                   allowlist-only by deliberate design, so an id absent from it is UNGATED.",
        depends_on: &[Dep::Route("GET /experimental/tool/ids")],
        wired_in: &[
            "src-tauri/src/offload/toolclass.rs",
            "src-tauri/src/tabs/config.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: None,
        probe: Some("opencode.tool_registry"),
        // WAIVER EXPIRED in V35 Phase D, as its text promised. `cimp
        // --harness-canary` now spawns `opencode serve` on a free loopback
        // port, calls this route, and FAILS on any live id present in neither
        // `OPENCODE_NATIVE_TABLE` nor `OPENCODE_NATIVE_REVIEWED_UNGATED` — so
        // "a human remembering to run a diff" is no longer the detection
        // mechanism, and Phase A finding 4 (the route cImp declares but never
        // calls) is closed: it is called, by the probe.
        //
        // Still NOT covered, and deliberately not waived here because it is a
        // different row: whether a gated id is gated *correctly*. The probe
        // proves every served id has been classified, not that its class is
        // right — that is `opencode.plugin.load_all`'s TCB waiver.
        waiver: None,
        controls: &[],
    },
    Capability {
        id: "opencode.plugin.load_all",
        harness: Harness::OpenCode,
        tier: Seam::D,
        contract: "Every file in `.opencode/plugin/` is loaded (config delivered via \
                   `OPENCODE_CONFIG_CONTENT`), the `chat.message`, `tool.execute.before` and \
                   `tool.execute.after` handlers fire, and `tool.execute.before` vetoes a call \
                   by `throw`ing — with the thrown message reaching the model.",
        depends_on: &[
            Dep::ConfigKey("OPENCODE_CONFIG_CONTENT"),
            Dep::ConfigKey("chat.message"),
            Dep::ConfigKey("tool.execute.before"),
            Dep::ConfigKey("tool.execute.after"),
            Dep::Behavior(
                "every file in `.opencode/plugin/` is loaded, including ones cImp did not write",
            ),
            Dep::Behavior(
                "`tool.execute.before` denies the call by throwing, and the thrown message \
                 reaches the model (the OpenCode-veto spike)",
            ),
        ],
        wired_in: &[
            "src-tauri/src/tabs/config.rs",
            "src-tauri/src/offload/loopback.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: None,
        probe: None,
        waiver: Some(
            "Tier D **and inside the TCB** (milestone decision 10): nothing outside a harness can \
             verify that a control inside it ran, so no canary cImp can write proves this row. A \
             plugin that loads but skips the `throw` disables native-tool containment while \
             looking fully functional. Verification stays the manual OpenCode-veto spike; V35 \
             Phase I adds a `chp` version handshake so a STALE plugin at least becomes a \
             mismatch instead of a mystery.",
        ),
        controls: &[
            CONTROL_TOOL_GATE,
            CONTROL_CHECKPOINT_PRE_MUTATION,
            CONTROL_TAINT_BEACON,
        ],
    },
];

/// The registry, as the Advisor / canary suite / UI will read it.
pub fn all() -> &'static [Capability] {
    CAPABILITIES
}

/// One capability by id, or `None`. The Phase E gate (`contract::gate(id)`)
/// builds on this.
pub fn get(id: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|c| c.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// The prose drift table, embedded at compile time so it cannot drift from
    /// the registry silently — same two-sources-of-truth pattern as
    /// `checks/mod.rs`'s `TS_TYPES` and `graph/memory.rs`. Path is relative to
    /// this file (`src-tauri/src/harness/`), up to the repo root.
    const MAINTENANCE_MD: &str = include_str!("../../../docs/MAINTENANCE.md");

    /// The heading that opens the drift-table section. The section runs to the
    /// next `## ` (h2) heading.
    const SECTION_HEADING: &str = "### Claude Code / OpenCode CLIs";

    /// Prefixes a capability id can start with. Used to tell registry ids apart
    /// from the many other backticked tokens in the same table cells.
    const ID_PREFIXES: [&str; 3] = ["claude.", "opencode.", "perm."];

    /// Every backtick-delimited token in `line`. Scans for pairs, so an unmatched
    /// trailing backtick yields nothing rather than swallowing the rest.
    fn backticked(line: &str) -> Vec<&str> {
        let bytes = line.as_bytes();
        let mut out = Vec::new();
        let mut i = 0usize;
        while let Some(open) = bytes[i..].iter().position(|b| *b == b'`') {
            let start = i + open + 1;
            let Some(close) = bytes[start..].iter().position(|b| *b == b'`') else {
                break;
            };
            out.push(&line[start..start + close]);
            i = start + close + 1;
        }
        out
    }

    /// A backticked token is a capability id iff it starts with one of the
    /// known prefixes and is made only of `[a-z0-9_.]`. That rejects the other
    /// backticked prose in the same cells (`opencode --version`,
    /// `.opencode/plugin`, `oob/opencode.rs:692`, `drift.payload.v1`, …).
    fn looks_like_id(tok: &str) -> bool {
        ID_PREFIXES.iter().any(|p| tok.starts_with(p))
            && tok
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
    }

    /// The `| … |` rows of the drift table, sliced out of `MAINTENANCE.md`.
    fn drift_table_rows() -> Vec<&'static str> {
        let start = MAINTENANCE_MD
            .find(SECTION_HEADING)
            .unwrap_or_else(|| panic!("MAINTENANCE.md no longer has a `{SECTION_HEADING}` section"));
        let body = &MAINTENANCE_MD[start + SECTION_HEADING.len()..];
        let end = body
            .lines()
            .scan(0usize, |off, l| {
                let here = *off;
                *off += l.len() + 1;
                Some((here, l))
            })
            .find(|(_, l)| l.starts_with("## "))
            .map(|(off, _)| off)
            .unwrap_or(body.len());
        body[..end]
            .lines()
            .filter(|l| l.starts_with('|'))
            .collect()
    }

    /// Both sources of truth for the harness dependency surface must name the
    /// same capabilities. The registry is the authority; the `MAINTENANCE.md`
    /// drift table carries the human narrative and now leads each row with the
    /// ids it describes. Prose can no longer drift from code in either
    /// direction: a new registry row with no doc row fails here, and a doc row
    /// naming a retired id fails here too.
    #[test]
    fn matrix_matches_maintenance_doc() {
        let registry: BTreeSet<&str> = CAPABILITIES.iter().map(|c| c.id).collect();
        assert_eq!(
            registry.len(),
            CAPABILITIES.len(),
            "duplicate capability id in the registry — ids are the join key and must be unique"
        );

        let mut doc: BTreeSet<&str> = BTreeSet::new();
        let mut doc_count = 0usize;
        for row in drift_table_rows() {
            for tok in backticked(row) {
                if looks_like_id(tok) {
                    doc_count += 1;
                    doc.insert(tok);
                }
            }
        }
        assert_eq!(
            doc.len(),
            doc_count,
            "a capability id appears in more than one MAINTENANCE.md drift-table row; each id \
             must live in exactly one row"
        );

        let missing: Vec<&str> = registry.difference(&doc).copied().collect();
        let extra: Vec<&str> = doc.difference(&registry).copied().collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "harness capability registry and the MAINTENANCE.md drift table disagree.\n  \
             in the registry but no drift-table row claims them: {missing:?}\n  \
             named by a drift-table row but not in the registry: {extra:?}\n\
             Fix by editing BOTH sides in the same commit (docs/MAINTENANCE.md \
             § 'Claude Code / OpenCode CLIs', leading 'Capability id(s)' column)."
        );
    }

    /// A `Silent` capability with no canary, no live probe and no explicit
    /// waiver is exactly the fragile-dependency-entering-unrecorded case this
    /// registry exists to stop. In Phase A every canary was `None`, so this
    /// asserted the waivers; Phase B–C swapped four waivers for canaries and
    /// Phase D widened it to accept an **L2 probe** as coverage in its own
    /// right — which is what let `opencode.tool_registry` drop its waiver
    /// rather than merely reword it. The test tightens on its own as coverage
    /// lands.
    ///
    /// L1 and L2 are deliberately alternatives here, not a conjunction: they
    /// answer different questions (see [`Capability::probe`]), and several rows
    /// are structurally reachable by only one of them — a CLI flag has no
    /// fixture to canary, and the four transcript fields of
    /// `claude.transcript.identity` have no single reader to drive one through.
    #[test]
    fn every_silent_degradation_has_a_canary_or_a_probe_or_a_waiver() {
        let naked: Vec<&str> = CAPABILITIES
            .iter()
            .filter(|c| c.degradation == Degradation::Silent)
            .filter(|c| c.canary.is_none() && c.probe.is_none() && c.waiver.is_none())
            .map(|c| c.id)
            .collect();
        assert!(
            naked.is_empty(),
            "these capabilities degrade SILENTLY with no canary, no live probe and no \
             accepted-residual waiver: {naked:?}. Add a canary or a probe (preferred) or state, \
             in `waiver`, what covers the row meanwhile and who owns closing it."
        );

        // Non-empty prose, not merely `Some("")` — global principle 5: a blank
        // waiver would pass the check above while recording nothing.
        for c in CAPABILITIES {
            if let Some(w) = c.waiver {
                assert!(
                    w.trim().len() > 20,
                    "{}: waiver must actually say what covers the row, not just exist",
                    c.id
                );
            }
        }
    }

    /// Every declared consumer path resolves to a real file. Cheap, and it is
    /// what catches the matrix rotting during a refactor — Phase K of this
    /// milestone relocates `oob/*` wholesale, and this test is what makes that
    /// move update the registry.
    #[test]
    fn wired_in_paths_exist() {
        // `CARGO_MANIFEST_DIR` is `<repo>/src-tauri`; `wired_in` entries are
        // repo-relative, so climb one level first.
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut missing: Vec<String> = Vec::new();
        for c in CAPABILITIES {
            assert!(
                !c.wired_in.is_empty(),
                "{}: a capability with no consumer is a dependency nothing needs — delete the \
                 row or name what breaks",
                c.id
            );
            for p in c.wired_in {
                assert!(
                    !p.contains("::"),
                    "{}: `wired_in` holds file paths only, no `::symbol` suffixes ({p}) — put \
                     the symbol in the `contract` sentence instead",
                    c.id
                );
                if !repo_root.join(p).is_file() {
                    missing.push(format!("{} -> {p}", c.id));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these `wired_in` paths do not resolve to a file under the repo root: {missing:#?}"
        );
    }

    /// V35 Phase D: the registry's `probe` column and the live probe's own
    /// declaration of what it drives must name the same capabilities, in both
    /// directions — the L2 twin of
    /// [`crate::harness::canary`]'s `canaries_and_the_matrix_agree`.
    ///
    /// The interesting half is the SECOND set. `probe::DECLARED_UNPROBED` holds
    /// the rows the probe enumerates as a permanent `unknown` (the
    /// scripted-turn class, and the Tier-D behaviors no probe can settle).
    /// Those are honest reporting, not coverage, so a row appearing in both
    /// lists — or carrying `probe: Some(..)` while only ever emitting
    /// `unknown` — would let a row look covered while nothing ever drives it.
    /// That is the "quality signal with no consumer" failure this milestone is
    /// built to avoid, so it is a build failure here.
    #[test]
    fn probes_and_the_matrix_agree() {
        let declared: BTreeSet<&str> = CAPABILITIES
            .iter()
            .filter_map(|c| {
                c.probe.inspect(|p| {
                    assert_eq!(
                        *p, c.id,
                        "capability `{}` declares probe `{p}` — a probe id IS the capability id, \
                         never a third namespace",
                        c.id
                    );
                })
            })
            .collect();
        let implemented: BTreeSet<&str> = crate::harness::probe::implemented_probes()
            .iter()
            .copied()
            .collect();

        let unimplemented: Vec<&str> = declared.difference(&implemented).copied().collect();
        assert!(
            unimplemented.is_empty(),
            "declared probe has no implementation: {unimplemented:?} carry `probe: Some(..)` but \
             `harness::probe::implemented_probes()` does not list them. Implement the probe, or \
             put the waiver back."
        );
        let undeclared: Vec<&str> = implemented.difference(&declared).copied().collect();
        assert!(
            undeclared.is_empty(),
            "probe exists outside the matrix: {undeclared:?} are run by harness/probe.rs but no \
             registry row declares them. Add the row (or set `probe: Some(..)` on it)."
        );

        // The permanent-`unknown` list: real rows, and NOT counted as coverage.
        for id in crate::harness::probe::declared_unprobed() {
            let cap = get(id).unwrap_or_else(|| {
                panic!("`probe::DECLARED_UNPROBED` names `{id}`, which is not a capability id")
            });
            assert!(
                cap.probe.is_none(),
                "`{id}` is both declared probed and enumerated as a permanent `unknown` — a row \
                 the probe never drives must not carry `probe: Some(..)`"
            );
        }

        // Every row is accounted for one way or the other: driven, or named as
        // a residual with a reason. A row in neither list is one the probe
        // silently omits from its report, which is how a dependency stops being
        // counted without anyone deciding to stop counting it.
        let unprobed: BTreeSet<&str> = crate::harness::probe::declared_unprobed()
            .iter()
            .copied()
            .collect();
        let orphans: Vec<&str> = CAPABILITIES
            .iter()
            .map(|c| c.id)
            .filter(|id| !declared.contains(id) && !unprobed.contains(id))
            .collect();
        assert!(
            orphans.is_empty(),
            "the live probe neither drives nor enumerates: {orphans:?}. Add a probe, or add the \
             row to `probe::DECLARED_UNPROBED` with the reason it cannot be driven."
        );
    }

    /// The TCB column (milestone locked decision 10) is documentation, not a
    /// gate — but a control that is declared twice, or not at all, means the
    /// documentation has stopped describing where enforcement lives.
    #[test]
    fn tcb_controls_are_declared_exactly_once() {
        for control in CONTROLS {
            let owners: Vec<&str> = CAPABILITIES
                .iter()
                .filter(|c| c.controls.contains(control))
                .map(|c| c.id)
                .collect();
            assert_eq!(
                owners.len(),
                1,
                "control `{control}` must be declared by exactly one capability, found: {owners:?}"
            );
        }
        for c in CAPABILITIES {
            for declared in c.controls {
                assert!(
                    CONTROLS.contains(declared),
                    "{}: `{declared}` is not a known control id — add it to `CONTROLS` with a \
                     doc comment saying where it executes",
                    c.id
                );
            }
        }
    }
}
