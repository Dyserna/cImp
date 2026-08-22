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
//! reads [`capabilities`] to decide what to drive and what to enumerate as
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

/// Which harness serves a capability — the registry's [`HarnessId`], re-exported
/// under the name this module has always used for the column.
///
/// V40 Phase A folded the enum into the registry (locked decision 3). Its three
/// variants were three answers to "which harness", and the whole point of the
/// registry is that there is one; [`Harness::ANY`] survives as the marker for a
/// row whose contract is stated about a *tab* rather than about a vendor.
pub use crate::harness::registry::HarnessId as Harness;

/// The declaration-site spellings for the `harness:` column below.
///
/// Inside `harness/` naming a harness is the job; what locked decision 10(a)
/// forbids is core doing it. Checked against the registry by
/// [`registry::every_declared_id_is_registered`].
const CLAUDE: Harness = Harness::declared("claude");
const OPENCODE: Harness = Harness::declared("opencode");
/// Harness-neutral — served by whatever adapter is attached.
///
/// **Constructed since V39 Phase B**, by exactly one row: `delegation.worker`
/// (locked decision 16). Phase A of V35 predicted CHP would produce the first
/// neutral row; what actually produced it is the first capability whose contract
/// is *"whatever harness this tab runs, it must serve `assistant_text` and
/// accept an input profile"* — a requirement stated about a tab, not a vendor.
const ANY: Harness = Harness::ANY;

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
    /// reporters into ONE module (`harness/claude/hook.rs`) plus one handler
    /// file, so the inference has nothing left to discriminate on: four rows now
    /// name the same two paths. Naming the token is the honest replacement, and
    /// the tests below assert it is unique and that every `drift.payload.v1` row
    /// carries one.
    pub drift_token: Option<&'static str>,
}

/// V32 Phase H native-tool containment: the enforcement `throw` in the
/// generated OpenCode plugin's `tool.execute.before`
/// (`harness/opencode/templates/plugin.js` since V35 Phase M — grep
/// `CIMP_REFUSAL_NATIVE_LOCAL`). cImp only *computes* the verdict.
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
/// V32 Phase F: the taint beacon as it executes on the **Claude** side.
///
/// **Enforcement moved on 2026-08-17 and this is where it lives now:** the
/// emitted `type: "http"` `PreToolUse` entry (`harness/claude/overlay.rs`,
/// matcher `WebFetch|WebSearch`) plus the handler it points at
/// (`offload::loopback::handle_claude_taint_beacon` →
/// `latch_beacon_core`). It was `cimp --taint-beacon` (`taint_beacon.rs`,
/// deleted). The control did not change what it does — engage the tab's EXTERNAL
/// latch before a harness-native web tool runs — but *where it executes* is the
/// whole content of this column, so the move is recorded rather than implied.
///
/// Note the asymmetry with OpenCode's [`CONTROL_TAINT_BEACON`]: that one runs
/// INSIDE the harness (a `throw`-capable plugin), which is why nothing cImp can
/// test proves it ran. This one is now delivery cImp can observe — the hook
/// either reaches the route or it does not.
pub const CONTROL_TAINT_BEACON_CLAUDE: &str = "taint.beacon.claude";
/// V33 Phase F: the pre-mutation checkpoint as it executes on the **Claude**
/// side.
///
/// **Enforcement moved on 2026-08-17**, same as its sibling above: the emitted
/// `type: "http"` `PreToolUse` entry (matcher `Edit|Write|MultiEdit|Bash`) plus
/// `offload::loopback::handle_claude_checkpoint` → `tool_checkpoint_core`. It was
/// `cimp --checkpoint-beacon` (`checkpoint_beacon.rs`, deleted).
///
/// The ordering this control rests on — the snapshot completes before the tool
/// runs — is now enforced by the handler awaiting it, under upstream's documented
/// "a `PreToolUse` hook blocks the call until the response". The shim enforced the
/// same thing from outside by reading its reply with a deadline, on an
/// undocumented behaviour.
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

/// V39 Phase B, locked decision 16 — **the worker gate**, spelled once.
///
/// The registry's first [`ANY`] row: "this tab's harness serves
/// `assistant_text` for the session's final message (or has a live fallback
/// reader) and accepts an [`InputProfile`]". A harness that is not gate-clean
/// is **not a valid worker** — it gets no `delegate_task_*` tool, its
/// Remote-offload tabs are not routed to, and preflight refuses naming the
/// gate's reason.
///
/// Mirrored to the frontend as `CAP_DELEGATION_WORKER` in
/// `src/lib/settings/types.ts`, like [`CAP_PRETOOLUSE_DENY`] and pinned by the
/// same test.
///
/// [`InputProfile`]: crate::harness::InputProfile
pub const CAP_DELEGATION_WORKER: &str = "delegation.worker";

/// Every control id that is declared somewhere in the registry. Each must
/// appear exactly once — see [`tests::tcb_controls_are_declared_exactly_once`].
///
/// **A control id names one PLACE enforcement executes, not one concept.** That
/// is what the exactly-once test is actually asserting, and V35 Phase I is where
/// the distinction became load-bearing: the taint beacon and the pre-mutation
/// checkpoint each run in *two* enforcement sites — inside the generated
/// OpenCode plugin's `tool.execute.before`, and on Claude's own `PreToolUse`
/// path — on two harnesses, in two artifacts, with two different failure modes
/// (the Claude checkpoint holds the tool call until the app has taken the
/// snapshot; the OpenCode one is bounded by its own abort signal). Folding them
/// onto one id would have made the column say "enforcement lives here",
/// singular, about a row that is only half of it.
///
/// The Claude half's site MOVED on 2026-08-17 — shim binary → `type: "http"`
/// entry plus its handler — without either id changing, which is the column
/// working as intended: an id names a place, and the place is described at the
/// id.
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

/// The **harness-neutral** half of the registry: every row whose contract is
/// stated about cImp's own seam or about a tab rather than about one vendor.
/// Extracted from the code on `develop`, not invented.
///
/// Each harness's own rows live with it, behind
/// [`HarnessPlugin::capabilities`](crate::harness::plugin::HarnessPlugin::capabilities)
/// (V40 Phase A, locked decision 17). Read the whole registry through
/// [`capabilities`] — a consumer that iterated this const alone would silently
/// stop seeing a harness's rows.
const CORE_CAPABILITIES: &[Capability] = &[
    // ── Claude Code: documented hooks (Tier B) ──────────────────────────────
    Capability {
        id: "claude.hook.user_prompt_submit",
        harness: CLAUDE,
        tier: Seam::B,
        contract: "A `UserPromptSubmit` hook of `type: \"http\"` (Claude Code ≥ 2.1.63, contract \
                   verified unchanged through 2.1.233 on 2026-08-17) POSTs \
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
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/harness/claude/overlay.rs",
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
        harness: CLAUDE,
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
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/harness/claude/overlay.rs",
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
        harness: CLAUDE,
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
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/harness/claude/overlay.rs",
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
        harness: CLAUDE,
        tier: Seam::B,
        contract: "A `PostToolUse` hook of `type: \"http\"` fires for `Edit` / `Write` / \
                   `MultiEdit` **on success** with the documented payload (`tool_name`, \
                   `tool_input.file_path`, `session_id`, `cwd`) and accepts \
                   `hookSpecificOutput.additionalContext` back in the 2xx JSON reply. \
                   Success-only is correct for THIS row — there is nothing to check after a failed \
                   edit — and is why `PostToolUseFailure` is wired for the sizing row instead \
                   (`claude.hook.tool_result`).",
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
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/harness/claude/overlay.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "The recorded GAP (Phase A finding 2 — the ONE converted hook that lagged NOTHING) is \
             CLOSED as of 2026-08-17: the route now reports a payload missing `session_id`, `cwd`, \
             `tool_name` or `tool_input.file_path` as `drift.payload.v1`, under the new token \
             `post_edit_hook`. It is deliberately NOT the never-shipped `postedit_hook` spelling — \
             that shim never reported, so nothing can be carrying it and it stays unattributed. \
             What is STILL uncovered, and why this row keeps a waiver: the report is LAGGING (it \
             fires when a broken payload arrives, so a hook that stops firing entirely says \
             nothing), there is no quiet detector because no witness proves an edit should have \
             happened, and there is no fixture canary over the response envelope. The row is \
             `Silent`, so a matcher removed upstream still costs auto-check diagnostics with only \
             the ABSENCE of `context.post_edit` pushes to notice it by. Owner: the V35 milestone.",
        ),
        controls: &[],
        drift_token: Some("post_edit_hook"),
    },
    Capability {
        id: "claude.hook.notification",
        harness: CLAUDE,
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
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/harness/claude/overlay.rs",
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
    // ── Claude Code: the two beacons (Tier D → B, 2026-08-17) ───────────────
    //
    // These two closed V35 Phase E's accepted residual when Phase I gave them
    // rows: `drift.payload.v1` had survived Phase E's consolidation *solely* as
    // the channel for their reports, because they posted to
    // `/activity/contract_drift` under their own shim names and neither had a
    // registry row. With the rows, [`capability_for_payload_shim`] resolves both
    // and "one notice source" holds for the whole matrix.
    //
    // **2026-08-17 moved them D → B**, which is the migration both waivers named
    // as their closing condition. `cimp --taint-beacon` and
    // `cimp --checkpoint-beacon` are deleted; both are `type: "http"`
    // `PreToolUse` entries now, and what changed is not the transport but the
    // TIER: each row's D-component was a `Dep::Behavior` on something upstream
    // does not document (a silent exit-0 hook never perturbs the call *including
    // on timeout*; the tool does not start until the hook process exits), and the
    // http hook contract states both facts in writing. Verified against the
    // 2.1.233 hooks reference on 2026-08-17: a non-2xx, a timeout and a refused
    // connection are non-blocking, blocking is expressible ONLY as 2xx plus a
    // decision field, and a `PreToolUse` http hook BLOCKS the tool call until the
    // response — which is what makes `permissionDecision: "deny"` expressible and
    // therefore what makes the checkpoint's ordering a documented guarantee.
    //
    // **They are still CLAUDE rows, not OpenCode plugin code.** The OpenCode
    // plugin reaches the same two loopback cores from inside
    // `tool.execute.before`, and THAT half is `opencode.plugin.load_all`'s. Two
    // harnesses, two enforcement sites, two rows — which is also why the TCB
    // column carries a distinct control id per site (see [`CONTROLS`]).
    Capability {
        id: "claude.hook.taint_beacon",
        harness: CLAUDE,
        tier: Seam::B,
        contract: "A `PreToolUse` hook of `type: \"http\"` with matcher `WebFetch|WebSearch` fires \
                   BEFORE the tool runs, POSTing `{session_id, cwd, tool_name}`, and a 2xx reply \
                   carrying no `hookSpecificOutput.permissionDecision` lets the call proceed. \
                   Report-only is STRUCTURAL rather than argued: blocking is expressible only as \
                   2xx plus a decision field, so a handler that never emits one cannot deny, and \
                   a timeout / refused connection / non-2xx is a documented NON-BLOCKING error.",
        depends_on: &[
            Dep::ConfigKey("hooks.PreToolUse"),
            Dep::ConfigKey("type=http"),
            Dep::ConfigKey("WebFetch|WebSearch"),
            Dep::ConfigKey("headers"),
            Dep::ConfigKey("allowedEnvVars"),
            Dep::ConfigKey("timeout"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("tool_name"),
            // The `Dep::Behavior` on the undocumented timeout semantic is GONE,
            // and its deletion is the whole point of the migration: the fail-open
            // contract is now the documented one this row's `contract` sentence
            // quotes, shared with every other `type: "http"` row.
        ],
        wired_in: &[
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/harness/claude/overlay.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "Delivery is now app-observable and payload drift reports through the route (token \
             `taint_beacon`, unchanged so a pre-upgrade tab's own shim lands in the same bucket) — \
             but there is still NO quiet-detection witness, and that is declared rather than \
             missed: a turn may legitimately never reach for `WebFetch`, so no other push proves \
             this one should have fired and any threshold would manufacture false reports \
             (`chp::witness_of` returns `None`, exactly as for `claude.hook.subagent`). A second \
             reason not to wire one: `taint.beacon` also has an OpenCode producer, so a token \
             named for this Claude row would misattribute an OpenCode plugin's silence. What \
             covers the row meanwhile: the payload reports above, and a beacon that stops arriving \
             leaves the tab's EXTERNAL latch unengaged — which the PROXIED half of the same latch \
             still catches for anything routed through cImp. A scripted-turn probe (a real turn \
             that reaches for `WebFetch`, asserting the beacon LANDED) is the only thing that \
             would close it, and it stays the Phase D residual it was.",
        ),
        controls: &[CONTROL_TAINT_BEACON_CLAUDE],
        drift_token: Some("taint_beacon"),
    },
    Capability {
        id: "claude.hook.checkpoint_beacon",
        harness: CLAUDE,
        tier: Seam::B,
        contract: "A `PreToolUse` hook of `type: \"http\"` with matcher \
                   `Edit|Write|MultiEdit|Bash` fires BEFORE the tool runs with the same payload, \
                   and **the tool call does not start until the hook's response arrives** — the \
                   documented mechanism that makes `permissionDecision: \"deny\"` expressible at \
                   all, and therefore what makes \"the checkpoint precedes the call\" exact rather \
                   than best-effort. Multiple `PreToolUse` entries run in parallel and all must \
                   resolve first, so the read advisor on its own matchers does not serialize \
                   against this one. The pinned `timeout` (5 s) is a ceiling above the app's own \
                   1800 ms snapshot budget, not the mechanism.",
        depends_on: &[
            Dep::ConfigKey("hooks.PreToolUse"),
            Dep::ConfigKey("type=http"),
            Dep::ConfigKey("Edit|Write|MultiEdit|Bash"),
            Dep::ConfigKey("timeout"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::JsonPath("tool_name"),
            // The ordering `Dep::Behavior` is GONE — it is the `contract`
            // sentence above now, grounded in the documented deny contract. What
            // is NOT claimed as documented, and needs no spike because a payload
            // cannot reveal it either way: whether the harness's parallel
            // evaluation of several `PreToolUse` entries has a *combined* deadline
            // beyond each entry's own `timeout`. It would only ever make this
            // hook give up sooner, which is the fail-open direction.
        ],
        wired_in: &[
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/harness/claude/overlay.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "Same shape as its sibling, and better covered. The ORDERING half stopped needing a \
             spike on 2026-08-17: it is upstream's documented deny contract, and the handler \
             awaits the snapshot before answering, so the guarantee is enforced app-side rather \
             than inferred. What remains uncovered is the same missing witness — no push proves \
             an edit should have happened, and `checkpoint.pre_mutation` has an OpenCode producer \
             this Claude-named token would misattribute — plus the fixture gap: no canary can \
             express an ordering. Covering it meanwhile: `drift.payload.v1` lags the payload half \
             (token `checkpoint_beacon`, unchanged), and a blown snapshot budget is NOT silent — \
             it writes its own Activity event (`workbench` / `checkpoint_missed`). So the failure \
             mode this row is `Silent` for is the hook not FIRING at all.",
        ),
        controls: &[CONTROL_CHECKPOINT_PRE_MUTATION_CLAUDE],
        drift_token: Some("checkpoint_beacon"),
    },
    // ── Claude Code: scraped UI (Tier D) ────────────────────────────────────
    Capability {
        id: "perm.tui_scrape",
        harness: CLAUDE,
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
    // ── Claude Code: the read path, PUSHED (Tier B — V35 Phase L) ───────────
    //
    // Three rows that did not exist before Phase L, because before Phase L the
    // data they carry had no seam of its own: it was read out of an emitted
    // artifact, and the artifact's row was the only row. Each one is paired
    // with the Tier-C row below it by `Degradation::Fallback { to }`, which is
    // the shape `claude.hook.notification` → `perm.tui_scrape` already used —
    // and which is what the milestone's "C→B" exit criterion means in the
    // registry: the CAPABILITY is served at Tier B, and the Tier-C reader it
    // used to depend on is now a named, tested, arbitrated fallback rather than
    // the only path.
    //
    // The arbitration itself is `harness::chp::served(agent, tab, event)`, asked
    // by BOTH sides — the push core refuses to act when the tab did not declare
    // the capability, the reader's tap refuses when it did — so for one tab
    // exactly one of the two produces each datum.
    Capability {
        id: "claude.hook.stop",
        harness: CLAUDE,
        tier: Seam::B,
        contract: "A `Stop` hook of `type: \"http\"` fires at the end of every assistant turn \
                   with `last_assistant_message` carrying the complete final assistant text. \
                   `MessageDisplay` is deliberately NOT used: it delivers per-chunk deltas on the \
                   streaming hot path, which would change the unit the sentence segmenter is fed \
                   (milestone locked decision 2, live-verify recipe 10).",
        depends_on: &[
            Dep::ConfigKey("hooks.Stop"),
            Dep::ConfigKey("type=http"),
            Dep::JsonPath("last_assistant_message"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            Dep::Behavior(
                "that `last_assistant_message` renders a multi-block assistant message the same \
                 way the transcript reader joins its `content[].text` blocks — both are reduced \
                 by `to_speakable` before segmentation, so a divergence changes WHAT is spoken, \
                 never WHETHER anything is. No payload reveals it; it needs a live turn",
            ),
        ],
        wired_in: &[
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/harness/claude/overlay.rs",
            "src-tauri/src/offload/loopback.rs",
            "src-tauri/src/tts/prose.rs",
        ],
        degradation: Degradation::Fallback {
            to: "claude.transcript.assistant_text",
        },
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "No fixture canary: the payload has one field cImp reads and `contract_checks` \
             already reports its absence on every fire, which is a LEADING check on the live wire \
             rather than against a recording. The silence case — the hook stops firing entirely — \
             is covered by the Phase L quiet detector (`chp::note_event`, witness `prompt`), \
             which reports under this row's own drift token instead of letting the reader quietly \
             take over. Degradation is `Fallback`, not `Silent`, so the enforcement test does not \
             require one.",
        ),
        controls: &[],
        drift_token: Some("stop_hook"),
    },
    Capability {
        id: "claude.hook.tool_result",
        harness: CLAUDE,
        tier: Seam::B,
        contract: "TWO all-tools (`\"\"`) `type: \"http\"` entries, one per outcome, because \
                   `PostToolUse` fires only when a tool SUCCEEDS. `hooks.PostToolUse` carries \
                   `tool_name` + `tool_result` (a string or `{type:\"text\", text}` blocks); \
                   `hooks.PostToolUseFailure` carries `tool_name` + `error` and fires when the \
                   tool fails. Both are SEPARATE routes from the auto-check entry on purpose: its \
                   group and the success group both fire for an `Edit`, so one shared route would \
                   run the project's checks twice and count one result twice.",
        depends_on: &[
            Dep::ConfigKey("hooks.PostToolUse"),
            // 2026-08-17: the errored half. A NEW upstream hook event (it did not
            // exist at 2.1.63), so an older CLI ignores the entry and failed
            // results go uncounted — see the waiver.
            Dep::ConfigKey("hooks.PostToolUseFailure"),
            Dep::ConfigKey("type=http"),
            Dep::JsonPath("tool_name"),
            Dep::JsonPath("tool_result"),
            Dep::JsonPath("error"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
            // `tool_use_id` is documented on both payloads and deliberately NOT
            // declared, because no line of code reads it: the `UsageEvent::
            // ToolResult` row these feed has no id column, exactly as the
            // transcript reader's does not. An unread field is a contract cImp
            // could not notice breaking.
            Dep::Behavior(
                "that an all-tools matcher (`\"\"`) really does fire for EVERY tool and not only \
                 for the ones the sibling entry names — the notification hooks rely on the same \
                 spelling, so a regression would surface on both at once",
            ),
        ],
        wired_in: &[
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/harness/claude/overlay.rs",
            "src-tauri/src/offload/loopback.rs",
        ],
        degradation: Degradation::Fallback {
            to: "claude.transcript.tool_result",
        },
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "The SHAPE half needs no canary of its own: the push sizes `tool_result` — and, since \
             2026-08-17, `error` — through the transcript reader's own `tool_result_chars`, so \
             `claude.transcript.tool_result`'s fixture canary is the leading check for both paths \
             at once, which is why the push reuses that function rather than restating it. \
             `contract_checks` covers the present-but-unreadable case (a payload that sizes to \
             zero while carrying something), and the quiet detector covers the stopped-firing case \
             (witness `context.post_edit`). ONE residual, and it is upstream's version skew rather \
             than a gap in coverage: `PostToolUseFailure` is newer than the 2.1.63 floor the other \
             entries need, so a CLI between the two ignores that entry and failed results go \
             uncounted with nothing firing — an absent hook event cannot report. It fails in the \
             direction the fallback covers (a tab that never serves this capability keeps its \
             reader), and the quiet detector does NOT see it, because the success half keeps \
             pushing.",
        ),
        controls: &[],
        drift_token: Some("tool_result_hook"),
    },
    Capability {
        id: "claude.hook.subagent",
        harness: CLAUDE,
        tier: Seam::B,
        contract: "`SubagentStart` and `SubagentStop` hooks of `type: \"http\"` fire (matcher \
                   `\"\"` = all agent types) carrying `agent_id` and `agent_type`; the pair is a \
                   lifecycle, so an id that started and has not stopped is an agent running. \
                   **No hook carries sub-agent TOKEN usage and no payload names a sub-agent \
                   transcript path**, so this row migrates the lifecycle only — the spend stays \
                   on `claude.transcript.subagents`.",
        depends_on: &[
            Dep::ConfigKey("hooks.SubagentStart"),
            Dep::ConfigKey("hooks.SubagentStop"),
            Dep::ConfigKey("type=http"),
            Dep::JsonPath("hook_event_name"),
            Dep::JsonPath("agent_id"),
            Dep::JsonPath("session_id"),
            Dep::JsonPath("cwd"),
        ],
        wired_in: &[
            "src-tauri/src/harness/claude/hook.rs",
            "src-tauri/src/harness/claude/overlay.rs",
            "src-tauri/src/offload/loopback.rs",
        ],
        degradation: Degradation::Fallback {
            to: "claude.transcript.subagents",
        },
        // `drift.subagent_transcripts.v1` is deliberately NOT listed, even
        // though it is about sub-agents: that rule is fed by
        // `SubagentState::drift_tick`, which reads the TRANSCRIPT layout, so it
        // lags the Tier-C row below and not this one. Claiming it here would
        // point one notice at two capabilities and make the Advisor unable to
        // say which broke.
        drift_rule: &[RULE_DRIFT_PAYLOAD],
        canary: None,
        probe: None,
        waiver: Some(
            "No quiet detector, and that is DECLARED rather than missed: a session may \
             legitimately launch no sub-agents forever, so no other push proves one should have \
             been reported (`chp::witness_of` returns `None` here). What covers it instead is the \
             fallback's own canary path — `SubagentState::drift_condition` keeps running on the \
             transcript for a serving tab, because its `launch_seen` bookkeeping is what would \
             otherwise start reporting a false 'launcher tool renamed'.",
        ),
        controls: &[],
        drift_token: Some("subagent_hook"),
    },
    // ── Claude Code: emitted transcript artifact (Tier C) ───────────────────
    //
    // **The Tier-C risk on these three is now UPSTREAM-CONFIRMED, not inferred**
    // (checked 2026-08-17 against the 2.1.233 docs). The reference pages now
    // state explicitly that the transcript JSONL format is *internal and
    // unstable*, and 2.1.210 shipped a transcript-size compression change — i.e.
    // upstream has both reserved the right to reshape this artifact and used it.
    // Nothing about the rows changes: Tier C already means "an emitted artifact,
    // not an API, that breaks silently as zeros and empties", and the mitigation
    // is already the right one — the L1 substantiveness canaries, the L2 probes
    // that drive them against a real transcript, and the capture-on-success
    // corpus so the first diagnostic is a diff. What changes is the confidence
    // with which the waivers below can be read: "this may move" is now "upstream
    // says this may move".
    Capability {
        id: "claude.transcript.assistant_text",
        harness: CLAUDE,
        tier: Seam::C,
        contract: "Assistant transcript lines (`type == \"assistant\"`) carry `message.content[]` \
                   blocks with `type == \"text\"` and a `text` string, written COMPLETE at \
                   message finish, and `thinking` / `tool_use` blocks stay distinguishable by \
                   `type` so they are never spoken aloud. `message.id` prefixes the dedup key, \
                   which is what stops one message being re-spoken on every 200 ms drain tick.",
        depends_on: &[
            Dep::JsonPath("type"),
            Dep::JsonPath("message.id"),
            Dep::JsonPath("message.content[].type"),
            Dep::JsonPath("message.content[].text"),
        ],
        wired_in: &[
            "src-tauri/src/harness/claude/read.rs",
            "src-tauri/src/tts/prose.rs",
        ],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: Some("claude.transcript.assistant_text"),
        probe: Some("claude.transcript.assistant_text"),
        waiver: None,
        controls: &[],
        drift_token: None,
    },
    // V39: the boundary the row above is READ AT. Deliberately its own row
    // rather than another `Dep::JsonPath` on `claude.transcript.assistant_text`,
    // because the two break differently and are noticed by different people:
    // losing `message.content[].text` makes a tab go mute, which a user hears
    // within one turn; losing `message.stop_reason` changes nothing anybody can
    // see except a driver waiting on a delegation, and it changes it into a
    // ten-minute wait rather than an error. One row per way of breaking.
    Capability {
        id: "claude.transcript.stop_reason",
        harness: CLAUDE,
        tier: Seam::C,
        contract: "An assistant transcript line carries `message.stop_reason`, and its value \
                   distinguishes a turn that CONTINUES from one that is OVER: `\"tool_use\"` means \
                   the model paused to call a tool, anything else non-null (`\"end_turn\"`, and \
                   the rarer `\"max_tokens\"` / `\"stop_sequence\"`) means it stopped talking. \
                   Several transcript lines may carry one message's blocks and they all repeat \
                   that message's stop reason, so the turn's final TEXT can follow the line that \
                   declared the turn over.",
        depends_on: &[
            Dep::JsonPath("type"),
            Dep::JsonPath("message.stop_reason"),
        ],
        wired_in: &["src-tauri/src/harness/claude/read.rs"],
        // A tab whose `Stop` hook pushes `assistant_text` never reads this at
        // all: the push core files the completion per turn, arbitrated by
        // `chp::served`. The fallback is therefore a real, named, tested path —
        // and the residual, for a tab with no push, is a `timeout` row with a
        // reason rather than a hang or a wrong answer (an unrecognized
        // stop reason ENDS the turn, so a new value files one message early
        // instead of never).
        degradation: Degradation::Fallback {
            to: "claude.hook.stop",
        },
        drift_rule: &[],
        canary: Some("claude.transcript.stop_reason"),
        probe: Some("claude.transcript.stop_reason"),
        waiver: None,
        controls: &[],
        drift_token: None,
    },
    // **This row does NOT migrate, and the reason is upstream's, not cImp's.**
    //
    // V35 Phase L moved the read path onto pushed hook payloads wherever a hook
    // carries the data. This one has no such hook: **no Claude Code hook input
    // carries token counts.** The common payload set is `session_id`,
    // `transcript_path`, `cwd`, `permission_mode` and `hook_event_name`; `Stop`
    // adds `last_assistant_message`; `PostToolUse` adds `tool_name` /
    // `tool_input` / `tool_result` / `tool_use_id`; `PostToolUseFailure` adds
    // `error` beside them; `SubagentStart`/`Stop` add `agent_id` / `agent_type` /
    // `agent_instructions`; and `PostCompact` exposes no compaction metrics. The
    // only documented token-usage surface is the OpenTelemetry
    // `claude_code.token.usage` metric, which is a different integration (an
    // exporter, not a hook) and is under-documented enough that the design doc's
    // mention of it could not be verified.
    //
    // **RE-VERIFIED against the 2.1.233 docs on 2026-08-17** (the previous check
    // was against 2.1.63-era docs): the hook-input contract has grown events
    // (`PostToolUseFailure`) and fields (`tool_use_id`), and still carries no
    // token counts, no context window and no rate-limit block anywhere. The
    // OpenTelemetry exporter remains the only usage surface.
    //
    // So this stays Tier C on the transcript tail, **permanently-until-upstream-
    // changes**, and the same is true of `claude.statusline.stdin` below (whose
    // stdin shape is likewise unchanged at 2.1.233 — additive fields only).
    // Decision 2's D→C→B→A ladder still applies — it is simply not climbable
    // here yet. The milestone's Phase L row lists "usage" among the migrations;
    // that text predates this check, and this comment is the correction.
    Capability {
        id: "claude.transcript.usage",
        harness: CLAUDE,
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
        wired_in: &["src-tauri/src/harness/claude/read.rs"],
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
        harness: CLAUDE,
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
        wired_in: &["src-tauri/src/harness/claude/read.rs"],
        // V35 Phase L: the FALLBACK behind `claude.hook.tool_result`, not the
        // primary. `Silent` is still the right degradation and the canary still
        // the right proof — a fallback that rots unnoticed is worse than one
        // that never existed, because the primary's own failure is what makes
        // it load-bearing. Arbitration: the reader's tap is suppressed for a tab
        // whose hello declares `session.tool_result`, so one result is never
        // counted twice; a tab that declares nothing (pre-upgrade, no loopback,
        // OpenCode) is served here exactly as before.
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
        harness: CLAUDE,
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
        wired_in: &["src-tauri/src/harness/claude/read.rs"],
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
        harness: CLAUDE,
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
        wired_in: &["src-tauri/src/harness/claude/read.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[RULE_DRIFT_SUBAGENT],
        canary: None,
        probe: None,
        waiver: Some(
            "Canary lands in V35 Phase B (fixture L1 — a directory fixture, not a single file). \
             `drift.subagent_transcripts.v1` already lags it and has fired once for real (the \
             Task→Agent rename), which is the evidence this layout moves. V35 Phase L made this \
             row the FALLBACK behind `claude.hook.subagent` — for the LIFECYCLE only. Sub-agent \
             TOKEN accounting (`SubagentState::scan`'s `UsageOrigin::Agent` rows) and the \
             `launch_seen`/`completion_seen` bookkeeping this waiver's drift rule reads are NOT \
             arbitrated and keep running on every tab: no hook payload carries sub-agent tokens \
             or names a sub-agent transcript path, and suppressing the bookkeeping would make \
             `drift_condition` report a false 'launcher tool renamed' on every serving session.",
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
        harness: CLAUDE,
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
        wired_in: &["src-tauri/src/harness/claude/statusline.rs", "src-tauri/src/statusline/mod.rs"],
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
        harness: CLAUDE,
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
            "src-tauri/src/harness/claude/overlay.rs",
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
        harness: CLAUDE,
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
        wired_in: &["src-tauri/src/tabs/config.rs", "src-tauri/src/harness/claude/read.rs"],
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
    //
    // **Re-verified live on 2026-08-17** against the installed OpenCode 1.18.13
    // and diffed against 1.18.18: every SSE shape below is unchanged, and the one
    // real movement is the turn-over signal — `session.idle` is now marked
    // deprecated in the upstream schema while still being emitted beside its
    // replacement `session.status`. That is the Tier-C failure mode arriving with
    // notice for once, so the reader took the notice (see the two new deps).
    Capability {
        id: "opencode.sse.events",
        harness: OPENCODE,
        tier: Seam::C,
        contract: "`GET /event` streams SSE envelopes `{type, properties}` carrying \
                   `message.updated`, `message.part.updated`, `message.part.delta`, \
                   `session.created` and BOTH turn-over signals — `session.idle` (deprecated in \
                   the upstream schema, still emitted) and its replacement `session.status`, whose \
                   `properties.status.type` is `\"busy\"` or `\"idle\"`. Both may arrive for one \
                   turn-over; the reader honours either and the second is a no-op. Every \
                   session-scoped event carries `properties.sessionID`.",
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
            // 2026-08-17 (re-verified against 1.18.18): `session.idle` is marked
            // DEPRECATED upstream and still emitted, so the reader now honours
            // BOTH. Declared as two deps because they are two reads — the event
            // type, and the field that says which status it is. Honouring only
            // the deprecated one would go silent the day upstream drops it, and
            // only the new one would go silent on every installed build today.
            Dep::JsonPath("session.status"),
            Dep::JsonPath("properties.status.type"),
            Dep::JsonPath("properties.sessionID"),
        ],
        wired_in: &["src-tauri/src/harness/opencode/read.rs"],
        degradation: Degradation::Silent,
        drift_rule: &[],
        canary: Some("opencode.sse.events"),
        probe: None,
        waiver: None,
        controls: &[],
        drift_token: None,
    },
    // ── OpenCode: HTTP routes (both Tier B since 2026-08-17) ────────────────
    //
    // `noReply` re-verified present and unchanged at 1.18.18 on 2026-08-17 (the
    // route and the field are byte-identical to 1.18.13 upstream). The waiver
    // below is unaffected: what it defers is proving that `noReply` still means
    // "do not start a turn", which needs a real session to push into — reading
    // the field in the source is not that proof.
    Capability {
        id: "opencode.route.push",
        harness: OPENCODE,
        tier: Seam::B,
        contract: "`POST /session/:id/message` accepts a hand-built message envelope, and \
                   `noReply: true` (≥ 1.18.13) injects the text into the session **without** \
                   starting an agent turn.",
        depends_on: &[
            Dep::Route("POST /session/:id/message"),
            Dep::ConfigKey("noReply"),
        ],
        wired_in: &["src-tauri/src/harness/opencode/read.rs"],
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
    // **The id is unchanged and the contract is inverted** (2026-08-17). This row
    // used to be the Tier-D watch its name still says: cImp sent no credential,
    // the probe confirmed OpenCode's local server answered anybody, and the row
    // recorded a DOUBLE-EDGED dependency — auth arriving would break the tap, and
    // until it did, every OpenCode tab cImp launched hosted an unauthenticated
    // HTTP server on loopback where `POST /session/:id/message` without `noReply`
    // starts a real agent turn.
    //
    // **That posture is CLOSED as of 2026-08-17**, which is locked decision 2's
    // D→C→B migration outranking new features. Live-spiked the same day against
    // the installed OpenCode 1.18.13 (and diffed against 1.18.18, byte-identical
    // in every integration-relevant file): setting a non-empty
    // `OPENCODE_SERVER_PASSWORD` on the `opencode` process enforces HTTP Basic
    // auth on every route — `GET /event` included (unauth ⇒ 401, Basic ⇒
    // 200/SSE). cImp now generates a fresh password per tab spawn, sets both
    // documented variables on the child, and authenticates its own tap and push
    // with `Authorization: Basic base64("opencode:<password>")`.
    //
    // Three details that are the row rather than trivia. The password is
    // snapshotted at module load in the child, so it MUST be set at spawn; an
    // EMPTY password silently disables auth entirely (global principle 5 — the
    // generator can only return a non-empty value); and the credential goes in
    // the header alone, because upstream's `auth_token` query param WINS over a
    // correct header and a present-but-wrong one 401s. First-party clients are
    // unaffected — the TUI, `opencode run` and the plugin's SDK client all
    // self-authenticate from the same env, which is why this does not break the
    // tab cImp launched with `--port`/`--hostname`.
    //
    // The id is NEVER renamed (§ 5.1), so "noauth" now reads as the name of the
    // thing that was fixed. The `Behavior` dep is gone with the tier: what the
    // row depends on is two documented env vars and three routes, and the probe
    // drives both directions of it.
    Capability {
        id: "opencode.route.noauth",
        harness: OPENCODE,
        tier: Seam::B,
        contract: "Setting the documented `OPENCODE_SERVER_PASSWORD` (non-empty) and \
                   `OPENCODE_SERVER_USERNAME` on the `opencode` child enforces HTTP Basic auth on \
                   its local server for every route cImp uses — `GET /event`, `GET /session/:id` \
                   and `POST /session/:id/message` — and a request carrying \
                   `Authorization: Basic base64(\"opencode:<password>\")` is accepted. cImp \
                   generates that password per tab spawn, so the tap and the V30 push \
                   authenticate with per-spawn credentials and the local server is no longer an \
                   unauthenticated loopback surface.",
        depends_on: &[
            Dep::ConfigKey("OPENCODE_SERVER_PASSWORD"),
            Dep::ConfigKey("OPENCODE_SERVER_USERNAME"),
            Dep::Route("GET /event"),
            Dep::Route("GET /session/:id"),
            Dep::Route("POST /session/:id/message"),
        ],
        wired_in: &[
            // Where the credential is generated and where the header is built.
            "src-tauri/src/harness/opencode/config.rs",
            // The tap and push that present it.
            "src-tauri/src/harness/opencode/read.rs",
            // The spec field that carries it from the spawn to the reader.
            "src-tauri/src/harness/reader.rs",
        ],
        degradation: Degradation::VisibleOff {
            user_message: "OpenCode's local server no longer accepts cImp's credentials — the \
                           live session tap and the V30 push fanout are off for OpenCode tabs \
                           until the authentication scheme is rewired.",
        },
        drift_rule: &[],
        canary: None,
        probe: Some("opencode.route.noauth"),
        waiver: None,
        controls: &[],
        drift_token: None,
    },
    // ── OpenCode: tool registry + plugin (Tiers C and D) ────────────────────
    //
    // **The live id set was re-verified on 2026-08-17** against the installed
    // 1.18.13 and diffed against 1.18.18: `GET /experimental/tool/ids` answers the
    // same 14 ids, so nothing this row watches has drifted. What DID change is
    // cImp's side — the three ids that exist only behind experiment env flags
    // (`execute`, `lsp`, `plan_exit`) are now classified rather than unexamined.
    // They are invisible to this row's probe by construction (a default serve
    // never lists them), so `harness::opencode::tools` carries a test of its own
    // for them: the subtraction `live − (gated ∪ reviewed) = ∅` can say nothing
    // about an id that is never live.
    Capability {
        id: "opencode.tool_registry",
        harness: OPENCODE,
        tier: Seam::C,
        contract: "Every tool id `GET /experimental/tool/ids` returns on the running binary is \
                   present in `offload::toolclass::OPENCODE_NATIVE_TABLE`. The table is \
                   allowlist-only by deliberate design, so an id absent from it is UNGATED.",
        depends_on: &[Dep::Route("GET /experimental/tool/ids")],
        wired_in: &[
            "src-tauri/src/harness/opencode/tools.rs",
            "src-tauri/src/harness/opencode/plugin.rs",
            // V35 Phase M: the emitted artifact itself. The table above decides
            // which names are gated; THIS file is where they land, and it is
            // the diff a reviewer reads when the classification changes.
            "src-tauri/src/harness/opencode/templates/plugin.js",
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
    // **Plugin API re-verified byte-identical at 1.18.18 on 2026-08-17** —
    // discovery, ESM loading, `OPENCODE_PURE`, and every `Hooks` signature this
    // row names. One thing worth writing down while looking at it: the published
    // Hooks type declares `permission.ask`, and NOTHING upstream fires it. It is
    // declared-but-dead, so no control may be built on it — a handler wired there
    // would read like a permission gate and never run once (the note lives in
    // `harness::opencode::tools`, beside the gate that IS real).
    Capability {
        id: "opencode.plugin.load_all",
        harness: OPENCODE,
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
            "src-tauri/src/harness/opencode/plugin.rs",
            // V35 Phase M: the `throw` this row's waiver is about is a line of
            // JavaScript, and since Phase M it is a line in a JavaScript FILE —
            // which is the point of the move. Open this one, not the emitter.
            "src-tauri/src/harness/opencode/templates/plugin.js",
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
    // ── V39 Phase B: cross-harness delegation (Tier D) ──────────────────────
    //
    // Three rows, one seam. `delegation.worker` is the harness-NEUTRAL
    // requirement the engine gates on; the two `*.input.profile` rows are the
    // per-harness half it is made of. They are separate rows rather than one
    // because they break differently and are fixed in different files: the
    // neutral one loses its READ half (a completion signal), the per-harness
    // ones lose their PUSH half (a paste that no longer yields one turn).
    //
    // A fourth row is part of the same requirement without being part of this
    // group: where the completion signal is a FALLBACK READER, that reader has
    // to derive the turn boundary itself, and the boundary is per harness — for
    // Claude it is `claude.transcript.stop_reason` (V39 review), for OpenCode
    // the `session.idle` / `session.status` pair inside `opencode.sse.events`.
    // Both live with their harness, which is why the neutral row states the
    // requirement and names no vendor.
    Capability {
        id: CAP_DELEGATION_WORKER,
        harness: ANY,
        tier: Seam::D,
        contract: "A tab may be driven as a delegation worker: its harness serves CHP                    `assistant_text` ONCE PER TURN, carrying that turn's final assistant                    message (or has a live fallback reader that derives the same boundary), and it declares an input profile whose paste +                    submit encoding yields exactly one turn. Harness-neutral by construction —                    the requirement is stated about a tab, not about a vendor.",
        depends_on: &[Dep::Behavior(
            "a completion signal exists for this tab (pushed `assistant_text`, or the harness's              declared fallback reader) AND the harness declares an input profile — a worker cImp              can type into but cannot read back from would silently swallow the task",
        ), Dep::Behavior(
            "…and that signal fires ONCE PER TURN, carrying the turn's final assistant message.              A push is turn-shaped by construction (the harness's own turn-over hook); a              fallback reader must DERIVE the boundary from the artifact it reads, which is a              per-harness contract with its own row and its own canary (see the reader rows for              each harness). A per-MESSAGE signal hands the driver a mid-turn preamble and              releases the worker while it is still working; no signal at all leaves the driver              waiting out its whole deadline",
        )],
        wired_in: &[
            "src-tauri/src/delegation/mod.rs",
            "src-tauri/src/delegation/engine.rs",
            "src-tauri/src/harness/plugin.rs",
        ],
        degradation: Degradation::FailClosed,
        drift_rule: &[],
        canary: None,
        probe: None,
        waiver: Some(
            "No fixture and no automatable probe: proving a worker completes needs a REAL turn on              a real TUI, which is the same class as the E1/D0 spikes. Covered meanwhile by (a)              the fail-closed gate itself — preflight refuses a tab with no completion signal and              no input profile rather than typing into it, (b) the recorded spike outcome in              `harness_versions.input_profile_status`, which this row's gate reads, and (c) V39              live-verify recipes 1, 2 and 10. Owner: whoever closes V39 Phase D.",
        ),
        controls: &[],
        drift_token: None,
    },
];

/// **The registry**: the neutral rows, then every registered harness's own, in
/// registry order.
///
/// The one accessor. `CORE_CAPABILITIES` is deliberately private, so "iterate
/// the capability registry" cannot accidentally mean "iterate the half core
/// happens to hold" — which is the shape locked decision 17 removes.
pub fn capabilities() -> impl Iterator<Item = &'static Capability> {
    CORE_CAPABILITIES.iter().chain(
        crate::harness::registry::all()
            .filter_map(|h| h.plugin())
            .flat_map(|p| p.capabilities().iter()),
    )
}

// `all()` lived here from Phase A as the accessor the seeded consumers would
// use. Phase E deleted it rather than carrying it under an allow: every real
// consumer — the probe, the gate, the Advisor's two reverse lookups — either
// iterates the registry directly or asks by id, so it was an alias for a
// public const that nothing called.

/// One capability by id, or `None`. [`gate`] builds on this.
pub fn get(id: &str) -> Option<&'static Capability> {
    capabilities().find(|c| c.id == id)
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
pub const GATED: &[&str] = &[CAP_PRETOOLUSE_DENY, CAP_DELEGATION_WORKER];

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
/// The input-profile spike outcome this gate should read.
///
/// For a named harness: its own row. For [`Harness::ANY`]: the answer to "can
/// delegation happen at all", which is the status of the best-placed candidate
/// — a `"pass"` if any harness that declares an input profile has a
/// non-blocking row, and otherwise the first blocking row, so the reason the
/// user is shown names a real recorded outcome rather than a synthesized one.
///
/// A harness that declares NO input profile is skipped: it is not a worker
/// whatever its row says, so letting it decide the neutral verdict would be a
/// gate answered by a harness the mechanism never touches.
fn worker_spike_status(settings: &Settings, harness: Harness) -> String {
    if harness != Harness::ANY {
        return settings
            .harness_settings(harness)
            .input_profile_status
            .clone();
    }
    let mut first_blocking: Option<String> = None;
    for h in crate::harness::registry::all() {
        if h.plugin().and_then(|p| p.input_profile()).is_none() {
            continue;
        }
        let status = settings.harness_settings(h).input_profile_status.clone();
        if !spike_status_blocks(status.trim()) {
            return status;
        }
        first_blocking.get_or_insert(status);
    }
    // No candidate at all ⇒ the neutral answer is the default, which does not
    // block: "nobody has run the spike" is not "the spike failed", and the
    // no-profile case is refused one layer up with a better message.
    first_blocking.unwrap_or_else(|| crate::settings::SPIKE_UNVERIFIED.to_string())
}

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
    gate_for(id, settings, Harness::ANY)
}

/// [`gate`], asked **about one harness**.
///
/// V40 Phase B, amendment 0-f. `delegation.worker` reads a recorded spike
/// outcome that used to be ONE scalar for every harness
/// (`harness_versions.input_profile_status`), which was two defects wearing one
/// field: a `"fail"` recorded against one TUI removed every `delegate_task_*`
/// tool and refused delegation for every harness, and a `"pass"` recorded
/// against Claude silently vouched for a harness nobody had ever typed into.
/// The status is `Settings::harness[<id>].input_profile_status` now, and the
/// callers that know whose worker they are asking about pass it.
///
/// [`Harness::ANY`] is the *neutral* question — "can delegation happen at
/// all?" — and it is answered by whether ANY harness that could be a worker (it
/// declares an input profile) has a non-blocking row. That is what the Settings
/// gate list and the health panel want: a single blocked row there would
/// otherwise claim delegation is off while a perfectly good worker sits
/// available.
pub fn gate_for(id: &str, settings: &Settings, harness: Harness) -> Gate {
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
        // V39 Phase B, locked decision 16: the delegation worker gate. Its
        // input is the recorded outcome of the **input-profile spike** — the
        // `Dep::Behavior` shared by both `*.input.profile` rows, which no
        // payload reveals and no fixture can settle. Same posture as E1
        // (`spike_status_blocks`: opt-in until proven broken, and anything
        // unrecognized blocks), and deliberately the same *reader*: two spike
        // fields interpreted by two different rules is how one of them ends up
        // meaning something nobody wrote down.
        //
        // **PER HARNESS since V40 Phase B** (amendment 0-f). It was one gate
        // for every harness, and the argument for that — "the recorded status
        // is a human's judgement about typing a turn, not a per-vendor
        // measurement" — was wrong in both directions: a human runs the spike
        // against ONE TUI, so a `"fail"` recorded against one product disabled
        // delegation for every other one, and a `"pass"` recorded against
        // Claude vouched for a harness nobody had ever typed into. The row is
        // the worker's own now; a caller with no worker in hand asks the
        // neutral question (see [`gate_for`]).
        //
        // The other half of per-harness availability is unchanged and still one
        // layer up: a harness with no `input.rs` has no profile, so
        // `harness::input_profile` answers `None` and that harness is not a
        // worker regardless of this verdict.
        CAP_DELEGATION_WORKER => {
            let status = worker_spike_status(settings, harness);
            let status = status.trim();
            if spike_status_blocks(status) {
                Gate {
                    id: cap.id,
                    blocked: true,
                    reason: format!(
                        "The input-profile contract check is recorded as {status:?}: a harness \
                         TUI on this machine does not accept a pasted multi-line request as one \
                         turn, so a delegated task would be typed in truncated or split across \
                         turns — and the worker would answer the wrong question without anything \
                         failing. Delegation is off: no `delegate_task_*` tool is advertised and \
                         no tab can be driven. Re-run the check in MAINTENANCE.md -> harness \
                         contracts and record the outcome in that harness's \
                         `harness[<id>].input_profile_status`."
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
    capabilities()
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
/// Phase J deleted five shim files and moved four of the six reporters into
/// one module, so the file-suffix inference had nothing left to discriminate on.
/// 2026-08-17 finished the job: the last two shim files are gone too, so EVERY
/// reporter now lives in `harness::claude::hook` and the column is the only
/// attribution there is.
/// Naming the token is the honest replacement: the tokens themselves are
/// unchanged (a pre-upgrade tab still POSTs them from its old shim binary), and
/// the tests below assert uniqueness in both directions.
///
/// `None` therefore means a **forged** name, which lands in the loopback's
/// `(unrecognized shim)` bucket. It used to mean one more thing —
/// `postedit_hook`, Phase A finding 2's hook that filed no report at all — and
/// since 2026-08-17 that row DOES report, under the deliberately different token
/// `post_edit_hook`. The old spelling stays unattributed for the reason a forged
/// name is: nothing ever sent it.
pub fn capability_for_payload_shim(shim: &str) -> Option<&'static Capability> {
    if shim.is_empty() {
        return None;
    }
    let mut hits = capabilities()
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
    const ID_PREFIXES: [&str; 4] = ["claude.", "opencode.", "perm.", "delegation."];

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
    /// `.opencode/plugin`, `harness/opencode/read.rs:692`, `drift.payload.v1`, …).
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
        let registry: BTreeSet<&str> = capabilities().map(|c| c.id).collect();
        assert_eq!(
            registry.len(),
            capabilities().count(),
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
        let naked: Vec<&str> = capabilities()
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
        for c in capabilities() {
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
        for c in capabilities() {
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
        let declared: BTreeSet<&str> = capabilities()
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
        let orphans: Vec<&str> = capabilities()
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

    /// V39 Phase B: settings with the input-profile spike outcome set on
    /// EVERY harness, and nothing else touched.
    ///
    /// Every row, because the tests below drive the NEUTRAL question ("can
    /// delegation happen at all?") and V40 Phase B made that the OR over the
    /// candidate harnesses — a fixture that set one row would be asserting
    /// about a harness the neutral gate is allowed to look past. The per-harness
    /// question has its own test.
    fn settings_with_input_profile(status: &str) -> Settings {
        let mut s = Settings::default();
        for h in crate::harness::registry::all() {
            s.harness_settings_mut(h)
                .expect("registered")
                .input_profile_status = status.to_string();
        }
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
                CAP_DELEGATION_WORKER => settings_with_input_profile("fail"),
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
        let inputs = [
            settings_with_e1("fail"),
            settings_with_e1("nonsense"),
            settings_with_input_profile("fail"),
            settings_with_input_profile("nonsense"),
        ];
        for c in capabilities() {
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

    /// **V39 Phase B: the delegation gate fails closed on the same table.**
    ///
    /// Deliberately the same statuses and the same expectations as the E1 test
    /// above, because both read `spike_status_blocks`: two spike fields
    /// interpreted by two different rules is how one of them ends up meaning
    /// something nobody wrote down. The default install must NOT block —
    /// delegation ships available, and a recorded `"fail"` is what turns it
    /// off.
    #[test]
    fn the_delegation_worker_gate_fails_closed_on_anything_unrecognized() {
        for ok in ["", "  ", "unverified", "UNVERIFIED", " pass ", "Pass"] {
            assert!(
                !gate(CAP_DELEGATION_WORKER, &settings_with_input_profile(ok)).blocked,
                "{ok:?} must NOT block delegation"
            );
        }
        for bad in ["fail", "Fail", " fail ", "FAILED", "failed", "wat", "0"] {
            let g = gate(CAP_DELEGATION_WORKER, &settings_with_input_profile(bad));
            assert!(g.blocked, "unrecognized status {bad:?} must fail CLOSED");
            assert!(
                g.reason.contains(bad.trim()),
                "the reason must quote the status actually recorded ({bad:?}), got: {}",
                g.reason
            );
        }
        assert!(!gate(CAP_DELEGATION_WORKER, &Settings::default()).blocked);
        // …and the two spikes are independent: a failed E1 must not switch
        // delegation off, nor the reverse. They gate different features.
        assert!(!gate(CAP_DELEGATION_WORKER, &settings_with_e1("fail")).blocked);
        assert!(!gate(CAP_PRETOOLUSE_DENY, &settings_with_input_profile("fail")).blocked);
    }

    /// **V40 Phase B, amendment 0-f: the gate resolves the WORKER's row.**
    ///
    /// One scalar for every harness was two defects in one field. A `"fail"`
    /// recorded against one TUI refused delegation to every other one; a
    /// `"pass"` recorded against Claude silently vouched for a harness nobody
    /// had ever typed into. Both directions, plus the neutral question's
    /// contract: `Harness::ANY` asks "can delegation happen AT ALL", so one
    /// blocked harness must not make it claim delegation is off while a good
    /// worker sits available.
    #[test]
    fn the_delegation_gate_resolves_the_workers_own_row() {
        let claude = Harness::from_id("claude").expect("registered");
        let opencode = Harness::from_id("opencode").expect("registered");

        let mut s = Settings::default();
        s.harness_settings_mut(claude)
            .expect("registered")
            .input_profile_status = "fail".to_string();

        assert!(
            gate_for(CAP_DELEGATION_WORKER, &s, claude).blocked,
            "the harness whose spike failed is blocked"
        );
        assert!(
            !gate_for(CAP_DELEGATION_WORKER, &s, opencode).blocked,
            "…and no other harness is, which is the whole amendment"
        );
        assert!(
            !gate(CAP_DELEGATION_WORKER, &s).blocked,
            "the neutral question is `can delegation happen at all`, and it can"
        );

        // Every candidate blocked ⇒ the neutral question blocks too, quoting a
        // real recorded status rather than a synthesized one.
        s.harness_settings_mut(opencode)
            .expect("registered")
            .input_profile_status = "fail".to_string();
        let g = gate(CAP_DELEGATION_WORKER, &s);
        assert!(g.blocked);
        assert!(g.reason.contains("fail"), "got: {}", g.reason);
    }

    /// **The registry's first `ANY` row exists and is the one meant.**
    ///
    /// `Any` carried an `#[allow(dead_code)]` from V35 Phase A with a comment
    /// predicting CHP would construct it. This pins what actually did — and
    /// pins that a neutral row names no vendor, which is the property L4
    /// depends on when it looks a harness up by id.
    #[test]
    fn the_neutral_row_is_the_delegation_worker_and_names_no_vendor() {
        let neutral: Vec<&str> = capabilities()
            .filter(|c| c.harness == ANY)
            .map(|c| c.id)
            .collect();
        assert_eq!(neutral, vec![CAP_DELEGATION_WORKER]);
        assert_eq!(ANY.id(), None, "a neutral row has no agent id");
        for id in crate::harness::registry::harness_ids() {
            assert_eq!(
                Harness::from_id(id).and_then(Harness::id),
                Some(id),
                "`{id}` must round-trip through the registry's harness vocabulary"
            );
            assert!(!Harness::from_id(id).unwrap().label().is_empty());
        }
        assert_eq!(
            crate::harness::registry::harness_ids(),
            vec!["claude", "opencode"]
        );
        assert_eq!(Harness::from_id("aider"), None);
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
            settings_with_input_profile("fail"),
            settings_with_input_profile("wat"),
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
        for c in capabilities() {
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

    /// Every reporter that files payload drift resolves to exactly one row,
    /// through the [`Capability::drift_token`] column.
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
    /// **2026-08-17 closed Phase A finding 2 and kept its negative half.** The
    /// auto-check route reports now, under `post_edit_hook` — but
    /// `postedit_hook`, the name the deleted shim binary would have used had it
    /// ever reported, must STILL resolve to nothing. Nothing on the wire can be
    /// carrying it (that shim never reported), so treating it as this row's token
    /// would only make a forged name attributable. The negative half is the
    /// interesting one and is asserted by name.
    #[test]
    fn every_payload_shim_resolves_to_one_row() {
        for (shim, expect) in [
            ("context_hook", "claude.hook.user_prompt_submit"),
            ("compact_hook", "claude.hook.precompact"),
            ("read_hook", CAP_PRETOOLUSE_DENY),
            ("notify_hook", "claude.hook.notification"),
            ("post_edit_hook", "claude.hook.posttooluse"),
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
        for c in capabilities() {
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
        // Six through Phase J (the five converted hooks' shim names plus the
        // two surviving beacons, minus the one that never reported); nine since
        // Phase L, which added a reporter per migrated read capability; ten since
        // 2026-08-17, which closed Phase A finding 2 — the one that never
        // reported now does.
        assert_eq!(n, 10, "the reporter set changed without this test noticing");
    }

    /// The TCB column (milestone locked decision 10) is documentation, not a
    /// gate — but a control that is declared twice, or not at all, means the
    /// documentation has stopped describing where enforcement lives.
    #[test]
    fn tcb_controls_are_declared_exactly_once() {
        for control in CONTROLS {
            let owners: Vec<&str> = capabilities()
                .filter(|c| c.controls.contains(control))
                .map(|c| c.id)
                .collect();
            assert_eq!(
                owners.len(),
                1,
                "control `{control}` must be declared by exactly one capability, found: {owners:?}"
            );
        }
        for c in capabilities() {
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
