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
//! # The runtime consumers
//!
//! Phases A–C had none on purpose — the tests below and in
//! [`crate::harness::canary`] were the consumers. V35 Phase D added the first
//! real one: [`crate::harness::probe`], reached from `cimp --harness-canary`,
//! reads [`CAPABILITIES`] to decide what to drive and what to enumerate as
//! `unknown`.
//!
//! **Phase E added the other two** (matrix draft § 3, consumers 1 and 2):
//!
//! 1. **Feature gating** — [`gate`] is the ONE query that answers "is this
//!    capability blocked, and why". It replaced
//!    `HarnessVersions::e1_blocked()` and its hand-written frontend mirror
//!    `harnessStatusBlocks`: a new fail-closed gate is now a [`GATED`] entry
//!    plus a `match` arm, not a new Settings field plus a second copy of the
//!    interpretation in TypeScript. [`gates`] serves the whole list over IPC,
//!    which is also the shape Phase G's *Harness health* panel renders.
//! 2. **The Advisor** — [`capabilities_for_rule`] and
//!    [`capability_for_payload_shim`] are the reverse of the
//!    [`Capability::drift_rule`] link. The eight V16 statistical detectors keep
//!    their thresholds and sample floors (milestone locked decision 5); what
//!    consolidated is the notice envelope, which now speaks as
//!    `advisor::RULE_DRIFT_CAPABILITY` about a named capability.

use crate::advisor::{
    RULE_DRIFT_HOOK_SILENT, RULE_DRIFT_INJECTION_UNSEEN, RULE_DRIFT_PAYLOAD, RULE_DRIFT_READ_BYPASS,
    RULE_DRIFT_READ_REASON, RULE_DRIFT_SUBAGENT, RULE_DRIFT_USAGE_FIELDS,
};
use crate::settings::Settings;

/// Which harness serves a capability.
// `Any` is unconstructed on purpose: no seeded row is harness-neutral yet, and
// CHP (milestone decision 9) is what will produce the first one.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    OpenCode,
    /// Harness-neutral — served by whatever adapter is attached. No seeded row
    /// is neutral yet; CHP (milestone decision 9) is what will produce them.
    Any,
}

/// The seam a dependency sits in. See the module docs for what each predicts.
// `A` is unconstructed on purpose: Tier A (MCP) has never broken cImp, so the
// ladder decision 2 ranks by needs its top rung declared even with no row on it.
#[allow(dead_code)]
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
// Six columns (`contract`, `degradation`, `canary`, `probe`, `waiver`,
// `controls`) are asserted by this module's tests and by `harness::canary` but
// have no non-test reader yet; Phase G's *Harness health* panel is the one they
// were seeded for.
#[allow(dead_code)]
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
    /// The `shim` token this capability's payload-drift reports arrive under at
    /// `POST /activity/contract_drift` — the join [`capability_for_payload_shim`]
    /// makes. `None` for a row nothing ever reports about.
    ///
    /// **Explicit since V35 Phase J**, and the change is a consequence of the
    /// phase rather than a preference. The attribution used to be *inferred*
    /// from [`Capability::wired_in`] — `read_hook` ⇒ `src-tauri/src/read_hook.rs`
    /// ⇒ this row — which worked while every reporter was its own binary in its
    /// own file. Phase J deleted those five files and moved four of the
    /// reporters into ONE module (`harness/claude_hook.rs`) plus one handler
    /// file, so the inference has nothing left to discriminate on: four rows now
    /// name the same two paths. Naming the token is the honest replacement, and
    /// the tests below assert it is unique and that every `drift.payload.v1` row
    /// carries one.
    pub drift_token: Option<&'static str>,
}

/// V32 Phase H native-tool containment: the enforcement `throw` in the
/// generated OpenCode plugin's `tool.execute.before`
/// (`tabs/config.rs`, ~line 2205). cImp only *computes* the verdict.
///
/// OpenCode-only, and that is a fact rather than a gap: Claude Code has no
/// equivalent site: its `PreToolUse` shims are report-only by locked decision 14
/// and are structurally incapable of denying.
pub const CONTROL_TOOL_GATE: &str = "tool.gate";
/// V33 Phase F: the pre-mutation checkpoint trigger, taken inside the same
/// `tool.execute.before` handler, after the gate. **The OpenCode instance** —
/// see [`CONTROL_CHECKPOINT_PRE_MUTATION_CLAUDE`] for the other one.
pub const CONTROL_CHECKPOINT_PRE_MUTATION: &str = "checkpoint.pre_mutation";
/// V32: the taint beacon the OpenCode plugin posts for native web tools.
/// **The OpenCode instance** — see [`CONTROL_TAINT_BEACON_CLAUDE`].
pub const CONTROL_TAINT_BEACON: &str = "taint.beacon";
/// V32 Phase F: the taint beacon as it executes on the **Claude** side — the
/// `cimp --taint-beacon` `PreToolUse` shim (`taint_beacon.rs`).
pub const CONTROL_TAINT_BEACON_CLAUDE: &str = "taint.beacon.claude";
/// V33 Phase F: the pre-mutation checkpoint as it executes on the **Claude**
/// side — the `cimp --checkpoint-beacon` `PreToolUse` shim
/// (`checkpoint_beacon.rs`).
pub const CONTROL_CHECKPOINT_PRE_MUTATION_CLAUDE: &str = "checkpoint.pre_mutation.claude";

/// The capability behind the read advisor's PreToolUse deny, spelled once.
///
/// A `&'static str` const rather than a literal because **three** consumers
/// join on it and a typo in any of them would silently un-gate a feature: the
/// registry row's own [`Capability::id`], the two `tabs/config.rs` gate call
/// sites via [`gate`], and the Settings toggle in `SettingsApp.svelte` (which
/// mirrors the string as `CAP_PRETOOLUSE_DENY` in `src/lib/settings/types.ts`,
/// pinned by [`tests::the_gated_capability_ids_reach_the_frontend`]).
pub const CAP_PRETOOLUSE_DENY: &str = "claude.hook.pretooluse_deny";

/// Every control id that is declared somewhere in [`CAPABILITIES`]. Each must
/// appear exactly once — see [`tests::tcb_controls_are_declared_exactly_once`].
///
/// **A control id names one PLACE enforcement executes, not one concept.** That
/// is what the exactly-once test is actually asserting, and V35 Phase I is where
/// the distinction became load-bearing: the taint beacon and the pre-mutation
/// checkpoint each run in *two* enforcement sites — inside the generated
/// OpenCode plugin's `tool.execute.before`, and inside a Claude `PreToolUse`
/// shim binary — on two harnesses, in two source files, with two different
/// failure modes (the Claude checkpoint shim waits for the app's reply; the
/// OpenCode one is bounded by its own abort signal). Folding them onto one id
/// would have made the column say "enforcement lives here", singular, about a
/// row that is only half of it.
// Documentation, not a gate (milestone locked decision 10): the enforcement
// test IS its consumer, so it has no runtime reader by design.
#[allow(dead_code)]
pub const CONTROLS: &[&str] = &[
    CONTROL_TOOL_GATE,
    CONTROL_CHECKPOINT_PRE_MUTATION,
    CONTROL_TAINT_BEACON,
    CONTROL_TAINT_BEACON_CLAUDE,
    CONTROL_CHECKPOINT_PRE_MUTATION_CLAUDE,
];

/// The registry. Extracted from the code on `develop`, not invented.
pub const CAPABILITIES: &[Capability] = &[
    // ── Claude Code: documented hooks (Tier B) ──────────────────────────────
    Capability {
        id: "claude.hook.user_prompt_submit",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "A `UserPromptSubmit` hook of `type: \"http\"` (Claude Code ≥ 2.1.63) POSTs \
                   `{prompt, session_id, cwd}` as JSON, substitutes `$CIMP_HOOK_TOKEN` into the \
                   configured `Authorization` header from `allowedEnvVars`, and parses the 2xx \
                   JSON reply exactly as it parses a command hook's stdout — so the \
                   `hookSpecificOutput.additionalContext` cImp answers with is prepended to the \
                   user's prompt and reaches the model.",
        depends_on: &[
            Dep::ConfigKey("hooks.UserPromptSubmit"),
            Dep::ConfigKey("type=http"),
            Dep::ConfigKey("headers"),
            Dep::ConfigKey("allowedEnvVars"),
            Dep::ConfigKey("timeout"),
            Dep::JsonPath("prompt"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("hookSpecificOutput.hookEventName"),
            Dep::JsonPath("hookSpecificOutput.additionalContext"),
            Dep::Behavior(
                "a 2xx JSON response body is parsed as hook output, and a timeout / refused \
                 connection / non-2xx is a NON-BLOCKING error — the fail-open contract the \
                 deleted shim used to provide by printing nothing and exiting 0",
            ),
        ],
        wired_in: &[
            "src-tauri/src/harness/claude_hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/tabs/config.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_INJECTION_UNSEEN, RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "V16 lags both halves: `drift.injection_unseen.v1` watches the follow-rate collapse, \
             and the route reports a payload missing `session_id`/`cwd` as `drift.payload.v1` \
             (under the token `context_hook`, unchanged from the shim so a pre-upgrade tab's \
             reports land in the same bucket). Both are lagging; the leading fixture check over \
             the response envelope lands in V35 Phase B.",
        ),
        controls: &[],
        drift_token: Some("context_hook"),
    },
    Capability {
        id: "claude.hook.precompact",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "A `PreCompact` hook of `type: \"http\"` POSTs `{session_id, trigger, cwd}` and \
                   the `hookSpecificOutput.additionalContext` in its 2xx JSON reply reaches the \
                   *compaction prompt* — spike D0, outcome recorded in \
                   `harness_versions.d0_status`.",
        depends_on: &[
            Dep::ConfigKey("hooks.PreCompact"),
            Dep::ConfigKey("type=http"),
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
        wired_in: &[
            "src-tauri/src/harness/claude_hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/tabs/config.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "The payload half is covered by the route's `drift.payload.v1` reports (token \
             `compact_hook`, unchanged from the deleted shim). The behavior half is spike D0 and \
             stays manual by milestone decision 7 — no payload reveals whether a compaction \
             prompt consumed the block. A D0 failure degrades to a no-op (server-side dedup-clear \
             stays correct regardless), so this is an accepted residual rather than a scheduled \
             canary.",
        ),
        controls: &[],
        drift_token: Some("compact_hook"),
    },
    Capability {
        id: CAP_PRETOOLUSE_DENY,
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "A `PreToolUse` hook of `type: \"http\"` can deny a tool call by answering 2xx \
                   with `hookSpecificOutput.permissionDecision: \"deny\"`, and the accompanying \
                   `permissionDecisionReason` is surfaced **to the model** — not only to the \
                   user. Spike E1, recorded in `harness_versions.e1_status`. **Blocking is \
                   expressible only this way**: a non-2xx, a timeout and a refused connection are \
                   all non-blocking, which is what makes the advisor structurally unable to \
                   refuse a read by failing.",
        depends_on: &[
            Dep::ConfigKey("hooks.PreToolUse"),
            Dep::ConfigKey("type=http"),
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
        wired_in: &[
            "src-tauri/src/harness/claude_hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/tabs/config.rs",
        ],
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
        drift_token: Some("read_hook"),
    },
    Capability {
        id: "claude.hook.posttooluse",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "A `PostToolUse` hook of `type: \"http\"` fires for `Edit` / `Write` / \
                   `MultiEdit` with the documented payload (`tool_name`, `tool_input.file_path`, \
                   `session_id`, `cwd`) and accepts `hookSpecificOutput.additionalContext` back \
                   in the 2xx JSON reply.",
        depends_on: &[
            Dep::ConfigKey("hooks.PostToolUse"),
            Dep::ConfigKey("type=http"),
            Dep::JsonPath("tool_name"),
            Dep::JsonPath("tool_input.file_path"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("hookSpecificOutput.hookEventName"),
            Dep::JsonPath("hookSpecificOutput.additionalContext"),
        ],
        wired_in: &[
            "src-tauri/src/harness/claude_hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/tabs/config.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: None,
        probe: None,
        waiver: Some(
            "GAP, recorded rather than assumed and deliberately NOT closed by V35 Phase J: this \
             is the ONE converted hook that reports no payload drift (it was the one shim that \
             never called `report_contract_drift`), so `drift.payload.v1` does NOT lag this row \
             and no other V16 rule does either — a matcher or field rename stops auto-check \
             diagnostics with nothing firing anywhere. Inventing a report during the http \
             migration would have moved a recorded gap into a footnote. Canary lands in V35 \
             Phase B; owner is the V35 milestone.",
        ),
        controls: &[],
        drift_token: None,
    },
    Capability {
        id: "claude.hook.notification",
        harness: Harness::Claude,
        tier: Seam::B,
        contract: "`Notification` and `PermissionDenied` hooks of `type: \"http\"` fire (matcher \
                   `\"\"` = all types) with `{hook_event_name, session_id, cwd, transcript_path}` \
                   plus a type and a message. The type/message pair is read BOTH flat \
                   (`notification_type`/`message`) and nested (`notification.{type,message}`) \
                   because the upstream docs are ambiguous about which shape ships.",
        depends_on: &[
            Dep::ConfigKey("hooks.Notification"),
            Dep::ConfigKey("hooks.PermissionDenied"),
            Dep::ConfigKey("type=http"),
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
            "src-tauri/src/harness/claude_hook.rs",
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
        drift_token: Some("notify_hook"),
    },
    // ── Claude Code: the two beacon shims (Tier D — V35 Phase I) ────────────
    //
    // These two close V35 Phase E's accepted residual. `drift.payload.v1`
    // survived Phase E's consolidation *solely* as the channel for their
    // reports, because they post to `/activity/contract_drift` under their own
    // shim names and neither had a registry row — so their reports could not be
    // attributed to a capability and their drift landed in the un-consolidated
    // channel. With these rows, [`capability_for_payload_shim`] resolves both
    // through `wired_in` like every other shim, and "one notice source" holds
    // for the whole matrix.
    //
    // **They are CLAUDE shims, not OpenCode plugin code.** `cimp --taint-beacon`
    // and `cimp --checkpoint-beacon` are `PreToolUse` hook binaries
    // (`taint_beacon.rs`, `checkpoint_beacon.rs`); the OpenCode plugin reaches
    // the same two loopback routes from inside `tool.execute.before`, and THAT
    // half is `opencode.plugin.load_all`'s. Two harnesses, two enforcement
    // sites, two rows — which is also why the TCB column carries a distinct
    // control id per site (see [`CONTROLS`]).
    Capability {
        id: "claude.hook.taint_beacon",
        harness: Harness::Claude,
        tier: Seam::D,
        contract: "A `PreToolUse` hook with matcher `WebFetch|WebSearch` fires BEFORE the tool \
                   runs, carrying `{session_id, cwd, tool_name}`, and a hook that writes nothing \
                   to stdout/stderr and exits 0 is NON-BLOCKING — including when it times out, \
                   which the hooks reference does not document at all. That undocumented \
                   timeout semantic is why the shim never waits on anything it does not control.",
        depends_on: &[
            Dep::ConfigKey("hooks.PreToolUse"),
            Dep::ConfigKey("WebFetch|WebSearch"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("tool_name"),
            Dep::Behavior(
                "a silent exit-0 hook never blocks or perturbs the tool call — and a TIMED-OUT \
                 hook does not either, which is undocumented (verified against the hooks \
                 reference 2026-08-07) and is the D-component this row is tiered on",
            ),
        ],
        wired_in: &[
            "src-tauri/src/taint_beacon.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/tabs/config.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "No canary and no probe yet, and the gap is structural rather than unowned: proving \
             this row needs a real Claude turn that reaches for `WebFetch` and an assertion that \
             the beacon landed — the scripted-turn probe class V35 Phase D deliberately did not \
             fake. Two things cover it meanwhile: `drift.payload.v1` lags it (the shim reports its \
             own missing fields, and Phase I is what makes that report resolve to THIS row), and \
             a beacon that stops arriving leaves the tab's EXTERNAL latch unengaged, which the \
             proxied half of the same latch still catches for anything routed through cImp. \
             CLOSES WITH: Phase L's push migration, which replaces the shim with an http hook \
             whose delivery is observable app-side, or a scripted-turn probe — whichever lands \
             first.",
        ),
        controls: &[CONTROL_TAINT_BEACON_CLAUDE],
        drift_token: Some("taint_beacon"),
    },
    Capability {
        id: "claude.hook.checkpoint_beacon",
        harness: Harness::Claude,
        tier: Seam::D,
        contract: "A `PreToolUse` hook with matcher `Edit|Write|MultiEdit|Bash` fires BEFORE the \
                   tool runs with the same payload, and Claude Code does not start the tool until \
                   the hook process EXITS — which is what makes \"the checkpoint precedes the \
                   call\" exact rather than best-effort. The configured `timeout` (5 s) stays a \
                   ceiling above the shim's own 2 s reply deadline, not the mechanism.",
        depends_on: &[
            Dep::ConfigKey("hooks.PreToolUse"),
            Dep::ConfigKey("Edit|Write|MultiEdit|Bash"),
            Dep::ConfigKey("timeout"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("tool_name"),
            Dep::Behavior(
                "the tool call does not begin until this hook process exits — undocumented, and \
                 the ordering the whole feature rests on: a checkpoint that can contain the \
                 change it claims to predate silently misleads a restore",
            ),
        ],
        wired_in: &[
            "src-tauri/src/checkpoint_beacon.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/tabs/config.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "Same structural gap as its sibling — a scripted Claude turn that edits a file, plus \
             an assertion about checkpoint ORDERING, which no fixture can express. What covers it \
             meanwhile is strictly better than for the taint beacon: `drift.payload.v1` lags the \
             payload half (resolving to this row from Phase I on), and a blown reply deadline \
             already surfaces as its own Activity event (`workbench` / `checkpoint_missed`) \
             instead of being lost — so the failure mode this row is `Silent` for is the hook not \
             FIRING, not the checkpoint failing. CLOSES WITH: Phase L's push migration or a \
             scripted-turn probe.",
        ),
        controls: &[CONTROL_CHECKPOINT_PRE_MUTATION_CLAUDE],
        drift_token: Some("checkpoint_beacon"),
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
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
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
        // V35 Phase E, closing Phase A finding 5. The seeded `VisibleOff` was
        // **declared intent, never observed behavior**: `resolve_oob_source`
        // (`tabs/config.rs:184-195`) pushes `--session-id <uuid>` onto the
        // child's argv unconditionally for every Claude tab that does not
        // already select its own session, and nothing anywhere probes the flag
        // first, catches a usage error, or renders a message. A vanished flag
        // therefore kills the tab at spawn — loudly, with the CLI's own
        // argument error in the pane — rather than degrading to the pre-V34
        // newest-transcript-wins binding the seeded `user_message` promised.
        //
        // The row is DOWNGRADED to what the code truly does rather than
        // building the speculative UI to match the prose. Building it would
        // mean probing `--session-id` at every spawn (milestone locked decision
        // 8's "not at every tab spawn") to serve a message for a flag whose
        // disappearance the L2 probe already reports as a Fail. The honest
        // fallback exists and is one line — `pinned_session = None`, the branch
        // `args_select_session` already takes — so if a future release does
        // remove the flag this row becomes `VisibleOff` *and* that branch
        // becomes reachable in the same commit.
        degradation: Degradation::FailClosed,
        drift_rule: &[],
        canary: None,
        probe: Some("claude.flag.session_id"),
        waiver: None,
        controls: &[],
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
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
        drift_token: None,
    },
];

// `all()` lived here from Phase A as the accessor the seeded consumers would
// use. Phase E deleted it rather than carrying it under an allow: every real
// consumer — the probe, the gate, the Advisor's two reverse lookups — either
// iterates `CAPABILITIES` directly or asks by id, so it was an alias for a
// public const that nothing called.

/// One capability by id, or `None`. [`gate`] builds on this.
pub fn get(id: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|c| c.id == id)
}

// ── Consumer 1: feature gating (V35 Phase E) ────────────────────────────────

/// The runtime verdict for one capability's feature gate.
///
/// Serialized straight to the frontend (`CapabilityGate` in
/// `src/lib/settings/types.ts`) so the Settings window renders a *computed*
/// answer instead of re-implementing the interpretation — which is exactly what
/// `harnessStatusBlocks` was, and what V35 Phase E deleted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Gate {
    /// The capability id — the join key, echoed so a consumer never has to
    /// pair a verdict with a position in a list.
    pub id: &'static str,
    pub blocked: bool,
    /// Why, in the user's words, ready to render. **Empty iff `!blocked`**
    /// (global principle 5, and pinned by
    /// [`tests::a_blocked_gate_always_says_why`]): a block with a blank reason
    /// is a feature that vanished without explanation.
    pub reason: String,
}

impl Gate {
    fn available(id: &'static str) -> Self {
        Self {
            id,
            blocked: false,
            reason: String::new(),
        }
    }
}

/// The id [`gate`] echoes when handed a string no registry row claims. Not a
/// capability id — parenthesized like `loopback::DRIFT_SHIM_UNKNOWN` so it
/// cannot be mistaken for one in a payload.
pub const UNKNOWN_CAPABILITY: &str = "(unknown capability)";

/// Every capability that carries a runtime gate — i.e. every id for which
/// [`gate`] can answer `blocked: true`.
///
/// Deliberately a short list and not the whole registry: most rows *degrade*
/// when they break (that is the [`Capability::degradation`] column) rather than
/// gating a feature off. [`tests::every_gated_capability_can_actually_block`]
/// keeps it honest in both directions — an entry here that no `match` arm can
/// ever block is a gate that does not exist, and a `match` arm not listed here
/// is a gate no UI can see.
pub const GATED: &[&str] = &[CAP_PRETOOLUSE_DENY];

/// Whether a recorded spike status BLOCKS its capability.
///
/// Moved here verbatim from `HarnessVersions::status_blocks` in V35 Phase E
/// (which is also when its frontend mirror `harnessStatusBlocks` was deleted).
/// The statuses are deliberately hand-editable strings, so this is the ONE
/// comparison allowed to interpret them — never a bare `"fail"` literal at a
/// call site. Normalizes (trim + case-fold) and fails **CLOSED**: anything that
/// is not a recognized non-fail value (`"unverified"`, `"pass"`, or
/// empty/missing) blocks, so a hand-typed `"Fail"`/`"failed"` surfaces as the
/// disabled toggle + uninstalled hook instead of silently sailing past.
///
/// NOTE the deliberate asymmetry with the F2 strict checks
/// (`advisor::Signals::e1_pass`, `ipc/commands.rs`): those require
/// `== "pass"` because "verified OK" must mean *proven*. This one passes
/// `"unverified"` too, because the gate's posture is
/// opt-in-until-proven-broken, not blocked-until-proven-working. The two are
/// NOT interchangeable and must never be merged.
fn spike_status_blocks(status: &str) -> bool {
    let s = status.trim().to_ascii_lowercase();
    !(s.is_empty() || s == "unverified" || s == "pass")
}

/// **The** feature-gate query (matrix draft § 3, consumer 1).
///
/// One place that turns a capability id plus the current [`Settings`] into a
/// renderable verdict, replacing V16's `HarnessVersions::e1_blocked()` and the
/// hand-kept TypeScript copy of its rule.
///
/// An id no row claims answers *available* (with [`UNKNOWN_CAPABILITY`] echoed
/// back). Fail-open is the right direction for an unknown id — the alternative
/// would let a typo'd string switch a working feature off — and it is safe here
/// only because the direction that matters is defended structurally instead:
/// call sites pass the `CAP_*` consts (a typo is a compile error), and
/// [`tests::every_gated_capability_can_actually_block`] proves each declared
/// gate really can block.
pub fn gate(id: &str, settings: &Settings) -> Gate {
    // Resolve through the registry so the returned `id` is the row's own
    // `&'static str` — the join key itself, not a caller-supplied lookalike.
    let Some(cap) = get(id) else {
        return Gate::available(UNKNOWN_CAPABILITY);
    };
    match cap.id {
        // V16 Feature 0: a recorded E1 spike FAILURE (the deny reason never
        // reaches the model, so every read-advisor remind is a bare refusal)
        // hard-blocks the read advisor regardless of `graph.read_advisor`.
        CAP_PRETOOLUSE_DENY => {
            let status = settings.harness_versions.e1_status.trim();
            if spike_status_blocks(status) {
                Gate {
                    id: cap.id,
                    blocked: true,
                    reason: format!(
                        "The E1 contract check is recorded as {status:?}: this Claude Code build \
                         does not surface a PreToolUse deny reason to the model, so every \
                         read-advisor reminder would be a bare refusal — worse than no advisor. \
                         The hook is not installed regardless of the toggle. Re-run the check in \
                         MAINTENANCE.md → harness contracts after the next Claude Code update, \
                         and record the outcome in `harness_versions.e1_status`."
                    ),
                }
            } else {
                Gate::available(cap.id)
            }
        }
        _ => Gate::available(cap.id),
    }
}

/// Every [`GATED`] capability's current verdict, in declaration order — the
/// payload `harness_versions_get` serves and the Settings window reads, and the
/// list Phase G's *Harness health* panel renders. A `Vec` of self-describing
/// records rather than one bespoke boolean per feature, precisely so the next
/// gate costs a [`GATED`] entry instead of a second wire field plus a second
/// frontend mirror.
pub fn gates(settings: &Settings) -> Vec<Gate> {
    GATED.iter().map(|id| gate(id, settings)).collect()
}

// ── Consumer 2: the Advisor (V35 Phase E) ───────────────────────────────────

/// Every registry row whose [`Capability::drift_rule`] column names `rule`.
///
/// The reverse of that link, and the join the Advisor raises its consolidated
/// `advisor::RULE_DRIFT_CAPABILITY` notices through: a V16 detector fires, this
/// says which capabilities it is evidence ABOUT, and one notice is raised per
/// affected capability. One notice per capability (rather than one naming them
/// all) is deliberate — the capability id is the dismissal key, so a shared
/// notice would let dismissing a symptom on one capability silence a sibling.
pub fn capabilities_for_rule(rule: &str) -> Vec<&'static Capability> {
    CAPABILITIES
        .iter()
        .filter(|c| c.drift_rule.contains(&rule))
        .collect()
}

/// The registry row for the reporter that filed a payload drift under `shim` —
/// the first argument of the drift report, which `loopback::contract_drift_row`
/// records as the leading token of the activity target
/// (`"<shim>: <missing fields>"`).
///
/// Resolved through the [`Capability::drift_token`] column.
///
/// **That column is V35 Phase J, and it replaced an inference.** The attribution
/// used to derive from [`Capability::wired_in`] — `read_hook` ⇒
/// `src-tauri/src/read_hook.rs` ⇒ [`CAP_PRETOOLUSE_DENY`] — which was exact
/// while every reporter was its own binary in its own file, and which Phase I
/// leaned on to close Phase E's residual for the two beacons at zero cost.
/// Phase J deleted the five shim files and moved four of the six reporters into
/// one module, so the file-suffix inference has nothing left to discriminate on.
/// Naming the token is the honest replacement: the tokens themselves are
/// unchanged (a pre-upgrade tab still POSTs them from its old shim binary), and
/// the tests below assert uniqueness in both directions.
///
/// `None` therefore means one of two things, and both are real rather than
/// defensive: a **forged** name, which lands in the loopback's
/// `(unrecognized shim)` bucket; or `postedit_hook` — Phase A finding 2, the one
/// converted hook that files no drift report at all, so its row names no drift
/// rule and can never appear here.
pub fn capability_for_payload_shim(shim: &str) -> Option<&'static Capability> {
    if shim.is_empty() {
        return None;
    }
    let mut hits = CAPABILITIES
        .iter()
        .filter(|c| c.drift_token == Some(shim) && c.drift_rule.contains(&RULE_DRIFT_PAYLOAD));
    let first = hits.next()?;
    // Two rows naming one token is a registry defect, not a runtime condition —
    // and an arbitrary pick would attribute a real report to the wrong
    // capability. Fall back to the unattributed channel, which loses the row
    // but never lies about it. `tests::every_payload_shim_resolves_to_one_row`
    // is what keeps this branch unreachable.
    if hits.next().is_some() {
        None
    } else {
        Some(first)
    }
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

    // ── V35 Phase E: the gate query ─────────────────────────────────────

    /// Settings with `e1_status` set, and nothing else touched.
    fn settings_with_e1(status: &str) -> Settings {
        let mut s = Settings::default();
        s.harness_versions.e1_status = status.to_string();
        s
    }

    /// Every [`GATED`] id is a real capability, and every one of them can
    /// actually answer `blocked` for SOME settings.
    ///
    /// The second half is the one that matters. A gate that no input can trip
    /// is a "quality signal with no consumer" wearing a gate's clothes: the UI
    /// would render a row that is permanently green while nothing enforces
    /// anything. The probe is per-id and deliberately crude — flip the one
    /// input each arm reads — so adding a `match` arm without a way to trip it
    /// fails here rather than shipping as decoration.
    #[test]
    fn every_gated_capability_can_actually_block() {
        assert!(!GATED.is_empty(), "the gate list must not be empty");
        for id in GATED {
            let cap = get(id).unwrap_or_else(|| {
                panic!("`GATED` names `{id}`, which is not a capability id — ids are the join key")
            });
            let blocking = match cap.id {
                CAP_PRETOOLUSE_DENY => settings_with_e1("fail"),
                other => panic!(
                    "`{other}` is in `GATED` but this test has no input that trips it. Add one, \
                     or drop the entry — a gate nothing can trip is not a gate."
                ),
            };
            assert!(
                gate(cap.id, &blocking).blocked,
                "`{}` is declared gated but the blocking input does not block it",
                cap.id
            );
        }
    }

    /// The inverse: every `match` arm in [`gate`] that can block is declared in
    /// [`GATED`]. Checked by running the whole registry through the gate with
    /// the same blocking inputs — a row that blocks while absent from `GATED`
    /// is a feature that switches itself off with no list any UI can render.
    #[test]
    fn no_gate_blocks_outside_the_declared_list() {
        let inputs = [settings_with_e1("fail"), settings_with_e1("nonsense")];
        for c in CAPABILITIES {
            for s in &inputs {
                if gate(c.id, s).blocked {
                    assert!(
                        GATED.contains(&c.id),
                        "`{}` blocks but is not listed in `GATED` — add it, or the Settings \
                         window and the Harness health panel cannot see the gate",
                        c.id
                    );
                }
            }
        }
    }

    /// The exact fail-closed semantics V16 pinned on
    /// `HarnessVersions::e1_blocked()`, relocated with the logic in V35 Phase E
    /// (the `tabs/config.rs` overlay tests pin the same table end to end).
    ///
    /// The statuses are hand-editable strings, so the interesting cases are the
    /// ones nobody meant to type: `"Fail"`, `" fail "` and `"failed"` all block,
    /// and so does anything else unrecognized. Only empty / `"unverified"` /
    /// `"pass"` pass, case- and whitespace-insensitively — `"unverified"`
    /// deliberately among them (see [`spike_status_blocks`]'s note on the F2
    /// asymmetry).
    #[test]
    fn the_e1_gate_fails_closed_on_anything_unrecognized() {
        for ok in ["", "  ", "unverified", "UNVERIFIED", " pass ", "Pass"] {
            assert!(
                !gate(CAP_PRETOOLUSE_DENY, &settings_with_e1(ok)).blocked,
                "{ok:?} must NOT block the read advisor"
            );
        }
        for bad in ["fail", "Fail", " fail ", "FAILED", "failed", "faill", "ok", "yes", "0"] {
            let g = gate(CAP_PRETOOLUSE_DENY, &settings_with_e1(bad));
            assert!(g.blocked, "unrecognized status {bad:?} must fail CLOSED");
            assert!(
                g.reason.contains(bad.trim()),
                "the reason must quote the status actually recorded ({bad:?}), got: {}",
                g.reason
            );
        }
        // The default install is `"unverified"` and must not block: Feature 0's
        // posture is opt-in-until-proven-broken, not the reverse.
        assert!(!gate(CAP_PRETOOLUSE_DENY, &Settings::default()).blocked);
    }

    /// A gate that blocks always says why, and one that does not never
    /// pretends there is something to say (global principle 5 — "empty is not
    /// absent" applied to the reason string).
    #[test]
    fn a_blocked_gate_always_says_why() {
        for s in [
            Settings::default(),
            settings_with_e1("fail"),
            settings_with_e1("wat"),
        ] {
            for g in gates(&s) {
                assert_eq!(
                    g.blocked,
                    !g.reason.trim().is_empty(),
                    "`{}`: blocked and reason must agree — got blocked={}, reason={:?}",
                    g.id,
                    g.blocked,
                    g.reason
                );
                assert!(GATED.contains(&g.id), "`gates()` returned an unlisted id");
            }
        }
        assert_eq!(gates(&Settings::default()).len(), GATED.len());
    }

    /// An id no row claims answers *available* under the sentinel, never a
    /// capability id it was not handed. Fail-open by design — see [`gate`].
    #[test]
    fn an_unknown_capability_id_is_not_a_gate() {
        let g = gate("claude.hook.pretooluse_denied", &settings_with_e1("fail"));
        assert!(!g.blocked);
        assert_eq!(g.id, UNKNOWN_CAPABILITY);
        assert!(get(UNKNOWN_CAPABILITY).is_none());
    }

    /// The join key the Settings window uses must be the string Rust gates on.
    /// This is the tripwire that REPLACED the `harnessStatusBlocks` mirror in
    /// V35 Phase E: instead of two copies of a rule, there are two copies of an
    /// id — and this fails the build if they part company.
    ///
    /// Same `include_str!` pattern as `settings::frontend_mirrors`, and the
    /// same self-guard: the const must be FOUND in the TypeScript (a rename or
    /// a move panics instead of vacuously passing).
    #[test]
    fn the_gated_capability_ids_reach_the_frontend() {
        const TS_TYPES: &str = include_str!("../../../src/lib/settings/types.ts");
        // The retired mirror must not creep back — the whole point of Phase E
        // is that the interpretation lives in Rust and the frontend reads a
        // computed verdict.
        // The DECLARATION, not the name: types.ts still mentions the retired
        // mirror in the doc comment that explains where the rule went, and a
        // note about history must not read as a regression.
        assert!(
            !TS_TYPES.contains("function harnessStatusBlocks"),
            "`harnessStatusBlocks` is back in src/lib/settings/types.ts — the E1 rule must not \
             be re-implemented in TypeScript; read `CapabilityGate.blocked` instead"
        );
        for id in GATED {
            assert!(
                TS_TYPES.contains(&format!("'{id}'")),
                "capability id `{id}` is gated in Rust but is not spelled in \
                 src/lib/settings/types.ts — the Settings window joins on this exact string"
            );
        }
    }

    // ── V35 Phase E: the Advisor's reverse lookups ──────────────────────

    /// Every drift rule a row names resolves back to that row, and every
    /// consolidated notice therefore has at least one capability to be about.
    /// A rule referenced by no row would raise no notice at all — the V16
    /// detector would keep computing and nothing would ever say it fired.
    #[test]
    fn every_declared_drift_rule_resolves_back_to_its_rows() {
        let mut seen = BTreeSet::new();
        for c in CAPABILITIES {
            for rule in c.drift_rule {
                seen.insert(*rule);
                let rows: Vec<&str> = capabilities_for_rule(rule).iter().map(|r| r.id).collect();
                assert!(
                    rows.contains(&c.id),
                    "`{}` names `{rule}` but the reverse lookup does not return it",
                    c.id
                );
            }
        }
        assert!(!seen.is_empty(), "no row names a drift rule — the join is vacuous");
        assert!(capabilities_for_rule("drift.no_such_rule.v1").is_empty());
    }

    /// The six reporters that file payload drift each resolve to exactly one
    /// row, through the [`Capability::drift_token`] column.
    ///
    /// **The last two were V35 Phase I**, closing Phase E's accepted residual:
    /// `taint_beacon` and `checkpoint_beacon` report through the same route and
    /// used to resolve to nothing, so their drift could not be attributed to a
    /// capability and kept the un-consolidated `drift.payload.v1` channel.
    ///
    /// **V35 Phase J changed the mechanism and kept every token.** Four of the
    /// six are no longer binaries at all — they are `type: "http"` routes whose
    /// payload checks run in-process — so the old `wired_in`-suffix inference
    /// could no longer tell them apart (they share two files). The tokens are
    /// deliberately unchanged: a tab open across the upgrade is still running
    /// the old shim and still POSTs these strings, so both paths must land on
    /// the same row.
    ///
    /// The negative half is still the interesting one and is asserted by name:
    /// `postedit_hook` is Phase A finding 2 (the one converted hook that never
    /// reports at all, so its row names no rule), and a forged name must never
    /// be pinned on a capability that did not report it.
    #[test]
    fn every_payload_shim_resolves_to_one_row() {
        for (shim, expect) in [
            ("context_hook", "claude.hook.user_prompt_submit"),
            ("compact_hook", "claude.hook.precompact"),
            ("read_hook", CAP_PRETOOLUSE_DENY),
            ("notify_hook", "claude.hook.notification"),
            ("taint_beacon", "claude.hook.taint_beacon"),
            ("checkpoint_beacon", "claude.hook.checkpoint_beacon"),
        ] {
            let cap = capability_for_payload_shim(shim)
                .unwrap_or_else(|| panic!("shim `{shim}` resolves to no capability"));
            assert_eq!(cap.id, expect, "shim `{shim}` resolved to the wrong row");
        }
        for shim in ["postedit_hook", "(unrecognized shim)", "", "hook", "beacon"] {
            assert!(
                capability_for_payload_shim(shim).is_none(),
                "shim `{shim}` must NOT be attributed to a capability"
            );
        }

        // Both directions, so the column cannot rot: a row that names
        // `drift.payload.v1` must carry a token that resolves back to it (or it
        // claims a lagging indicator that can never attribute to it), and a row
        // that carries a token must name the rule (or the token is decoration).
        let mut tokens: Vec<&str> = Vec::new();
        for c in CAPABILITIES {
            match (c.drift_token, c.drift_rule.contains(&RULE_DRIFT_PAYLOAD)) {
                (Some(tok), true) => {
                    tokens.push(tok);
                    assert!(
                        capability_for_payload_shim(tok).is_some_and(|r| r.id == c.id),
                        "`{}`'s drift token `{tok}` does not resolve back to it",
                        c.id
                    );
                }
                (Some(tok), false) => panic!(
                    "`{}` declares drift token `{tok}` but does not name `{RULE_DRIFT_PAYLOAD}` \
                     — the token would never be consulted",
                    c.id
                ),
                (None, true) => panic!(
                    "`{}` names `{RULE_DRIFT_PAYLOAD}` but declares no drift token — the row \
                     would never receive an attributed report",
                    c.id
                ),
                (None, false) => {}
            }
        }
        tokens.sort_unstable();
        let n = tokens.len();
        tokens.dedup();
        assert_eq!(n, tokens.len(), "two rows claim the same drift token");
        assert_eq!(n, 6, "the reporter set changed without this test noticing");
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
