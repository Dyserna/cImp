//! V35 Phase J — **Claude Code's L1**: the harness POSTs its own hook payloads
//! straight at cImp's loopback as `type: "http"` hooks, and the shim binaries
//! that used to stand between them are gone.
//!
//! **2026-08-17 (Claude Code 2.1.233): the last two went with them.** Phase J
//! deleted five and left `cimp --taint-beacon` / `cimp --checkpoint-beacon` as
//! `type: "command"` hooks, because a report-only side effect with no reply to
//! parse gained nothing from http. What that reasoning missed is that the two
//! rows were **Tier D** — their fail-open and their ordering were undocumented
//! behaviours, not contracts — and the http hook contract states both facts in
//! writing. Migrating them is the D→B move locked decision 2 ranks above new
//! features, and it is the closing condition both rows' waivers named. See the
//! route constants below.
//!
//! # What changed
//!
//! Until Phase J, every Claude hook spawned `cimp --<something>-hook`, which
//! read the payload on stdin, re-shaped it into a harness-neutral CHP body,
//! POSTed that to a `/context/*` route, and printed the reply back as the hook's
//! stdout JSON. Five binaries, five process spawns per turn, 1053 lines — all of
//! it a courier between two processes that were already able to talk.
//!
//! Claude Code 2.1.63 added `type: "http"` hooks: the harness itself POSTs the
//! hook-input JSON and parses the 2xx JSON reply *exactly as it parses a command
//! hook's stdout*. So the shim layer is pure overhead, and this module is what
//! replaces it — the payload mechanics that used to live in the five shims, now
//! running inside the app, on the receiving end of the same wire.
//!
//! # The shape of the thing
//!
//! ```text
//!   Claude Code  ──POST hook-input JSON──►  /claude/hook/<event>  ──►  the same
//!     (L0)          + X-CIMP-* headers          (this module)          internal
//!                                                                      logic the
//!                                                                      legacy
//!                                                                      routes call
//! ```
//!
//! * **Identity rides headers, not the body.** A Claude hook payload carries
//!   `session_id` and `cwd` and nothing that names a cImp tab (the E2 spike's
//!   finding, which is why `--tab <id>` used to be baked into every shim's
//!   argv). It is now baked into the emitted hook entry's `headers` instead —
//!   `X-CIMP-Tab`, plus `X-CIMP-Agent` and `X-CIMP-Chp` so a Claude tab is
//!   observed by the same [`crate::harness::chp`] staleness machinery an
//!   OpenCode tab is.
//! * **The bearer token rides `allowedEnvVars`.** `Authorization: Bearer
//!   $CIMP_HOOK_TOKEN`, substituted by the harness from its own environment,
//!   which `tabs::config::compose_ai_env` sets at spawn. It is deliberately not
//!   a literal in the `--settings` overlay: that overlay is an argv value and
//!   argv is the most casually readable thing on the machine.
//! * **The legacy `/context/*` routes stay.** A tab launched before the upgrade
//!   is still running an overlay full of `cimp --context-hook` commands, and
//!   those keep working (the dispatch flags survive as tombstones in `main.rs`).
//!   The two paths converge on one internal core per capability — see
//!   `offload::loopback`'s Phase J section.
//!
//! # Fail-open, restated for HTTP
//!
//! The shims' discipline was "print nothing, exit 0". The HTTP equivalent, from
//! the hooks reference: a timeout, a refused connection and any non-2xx are all
//! **non-blocking** — execution continues. A 2xx with an empty or directive-free
//! JSON body is a no-op. Blocking is expressible *only* as 2xx + a JSON decision
//! field. So every handler here answers `200 {}` when it has nothing to say, and
//! the one route that can deny ([`ROUTE_PRE_TOOL_USE`]) does so only by saying
//! so explicitly.
//!
//! **`terminalSequence` is never emitted.** It is a hook-output field that
//! writes escape sequences into the PTY cImp renders; it is not a CHP capability
//! and no handler here may produce one (design § 5.2, pinned by a test).

use serde::Deserialize;

// ── the routes ──────────────────────────────────────────────────────────────

/// The prefix every harness-native Claude ingress route shares.
///
/// Deliberately **not** under `/context/*`: those routes take the
/// harness-neutral CHP body documented in `docs/CHP.md` § 4.2, and these take
/// Claude Code's own hook-input JSON verbatim. One prefix says which of the two
/// a reader is looking at, and CHP rule 4 ("a route is never repurposed") is
/// what makes that a new route rather than a second body shape on an old one.
pub const ROUTE_PREFIX: &str = "/claude/hook/";

/// `UserPromptSubmit` — the context-injection tap and the prompt-tap checkpoint
/// trigger. Was `cimp --context-hook`.
pub const ROUTE_USER_PROMPT_SUBMIT: &str = "/claude/hook/user_prompt_submit";
/// `PreCompact` — the compaction carry-over block. Was `cimp --precompact-hook`.
pub const ROUTE_PRE_COMPACT: &str = "/claude/hook/pre_compact";
/// `PreToolUse` (matchers `Read` / `Bash`) — the redundant-read advisor, the one
/// route here that can deny. Was `cimp --read-hook`.
pub const ROUTE_PRE_TOOL_USE: &str = "/claude/hook/pre_tool_use";
/// `PostToolUse` (matcher `Edit|Write|MultiEdit`) — the auto-check diff. Was
/// `cimp --postedit-hook`.
pub const ROUTE_POST_TOOL_USE: &str = "/claude/hook/post_tool_use";
/// `Notification` + `PermissionDenied` — permission detection. Was
/// `cimp --notify-hook`, and like it this ONE route serves both events and
/// dispatches on `hook_event_name`.
pub const ROUTE_NOTIFICATION: &str = "/claude/hook/notification";
/// `SessionStart` — Claude's CHP hello (design D3, milestone decision 5 for this
/// phase). New in Phase J: there was no shim to replace, because there was no
/// hello.
pub const ROUTE_SESSION_START: &str = "/claude/hook/session_start";

// ── V35 Phase L: the read path, pushed ──────────────────────────────────────
//
// Three routes that replace no shim, because what they replace is not a shim:
// they are the Tier-C transcript tail's taps, arriving as documented hook
// payloads instead of as fields cImp scrapes out of an emitted artifact. Each
// feeds a CHP event that Phase I reserved (`docs/CHP.md` § 4.3) and Phase L
// realizes.

/// `Stop` — the complete final assistant message of a turn
/// (`last_assistant_message`), feeding `assistant_text` → TTS.
///
/// **`MessageDisplay` is deliberately NOT wired**, and that is locked decision
/// 2 rather than an oversight. It fires per streaming chunk, which would hand
/// the segmenter token deltas where it is fed complete text today; and it runs
/// on the streaming hot path with a 10 s default timeout, i.e. inside the
/// rendering the user is watching. `Stop`'s cadence — one complete message at
/// message finish — is *identically* the cadence
/// `harness::claude::read::assistant_texts` delivers, so the migration
/// preserves TTS behaviour by construction rather than by testing for it after
/// the fact (live-verify recipe 10).
pub const ROUTE_STOP: &str = "/claude/hook/stop";

/// `PostToolUse` on an ALL-TOOLS matcher — the tool result's size, feeding
/// `session.tool_result`.
///
/// **A second route, not a widened [`ROUTE_POST_TOOL_USE`].** Both entries fire
/// for an `Edit`, so sharing one route would run the auto-check twice and count
/// one tool result twice — the two failure modes this phase is most exposed to.
/// CHP compatibility rule 4 says the same thing from the protocol side: new
/// meaning, new route. The consequence is that the auto-check entry keeps its
/// exact `Edit|Write|MultiEdit` matcher and its exact handler, which is what
/// makes "the post-edit path is unchanged" a fact about the diff.
pub const ROUTE_POST_TOOL_USE_RESULT: &str = "/claude/hook/post_tool_use_result";

/// `PostToolUseFailure` on an ALL-TOOLS matcher — the **errored** half of
/// `session.tool_result` (2026-08-17, Claude Code 2.1.233).
///
/// `PostToolUse` fires only when a tool SUCCEEDS. Upstream added
/// `PostToolUseFailure` for the other half (`tool_name`, `tool_input`, `error`,
/// `tool_use_id` plus the common fields), and without it a failed tool result
/// reached cImp **only** through the transcript tail — which is arbitrated OFF
/// on exactly the tabs that serve `session.tool_result`. So a serving tab lost
/// every failed result's size: a real seam gap, not a rounding error, since a
/// failing `Bash` returns as much text as a succeeding one.
///
/// **Its own route, and deliberately no CHP event of its own** — see
/// [`chp_event`]. The capability is `session.tool_result`; the failure half is
/// the same datum with `is_error` set, and the transcript reader it replaces
/// sizes both through one function without looking at the flag.
pub const ROUTE_POST_TOOL_USE_FAILURE: &str = "/claude/hook/post_tool_use_failure";

/// `SubagentStart` **and** `SubagentStop` — sub-agent lifecycle, feeding
/// `session.subagent`.
///
/// One route for two events, dispatching on `hook_event_name`, exactly as
/// [`ROUTE_NOTIFICATION`] serves `Notification` and `PermissionDenied`. The
/// pair is a lifecycle: an id that started and has not stopped is an agent
/// running, which is the only fact the avatar's `AgentsActiveChanged` edge
/// needs.
///
/// **Sub-agent TOKEN usage does not come this way**, and cannot: no hook
/// payload carries token counts. The transcript tail keeps reading
/// `<session_id>/subagents/agent-*.jsonl` for that, permanently-until-upstream-
/// changes — see `claude.transcript.subagents`' registry row.
pub const ROUTE_SUBAGENT: &str = "/claude/hook/subagent";

// ── 2026-08-17: the two beacons, migrated (Tier D → B) ──────────────────────
//
// `cimp --taint-beacon` and `cimp --checkpoint-beacon` were the two Claude
// hooks Phase J left as `type: "command"` shims, on the reasoning that
// report-only side effects with no reply to parse gained nothing from http.
// That reasoning was incomplete: what the shims *also* had was an
// **undocumented** contract — "a hook that writes nothing and exits 0 never
// perturbs the call, including on timeout" — and the checkpoint one leaned on
// "the tool does not start until the hook process exits". Both were `Dep::
// Behavior` entries, which is why both rows were Tier D.
//
// The http hook contract states the same two facts *documented* (verified
// against the 2.1.233 hooks reference, 2026-08-17): a non-2xx, a timeout and a
// refused connection are non-blocking, blocking is expressible ONLY as 2xx plus
// a decision field, and a `PreToolUse` hook BLOCKS the tool call until the
// response — which is what makes `permissionDecision: "deny"` expressible at
// all, and therefore what makes the checkpoint's ordering a documented
// guarantee rather than an observed one. Multiple `PreToolUse` entries run in
// parallel and all must resolve before the tool starts, so the beacon and the
// advisor still do not serialize against each other.
//
// What the migration buys, in the registry's terms: delivery becomes
// **app-observable** (the route is either reached or it is not, and the tab's
// `chp`/hello observation runs on it), payload drift reports through the same
// in-process channel as every other converted hook, and two process spawns per
// matched tool call disappear.

/// `PreToolUse` (matcher `WebFetch|WebSearch`) — the V32 taint beacon. Was
/// `cimp --taint-beacon`.
///
/// Report-only: the handler answers [`no_op`] on every path, exactly as the
/// shim wrote nothing to stdout. Locked decision 14 ("hooks never deny; sensor
/// mode must never break a tab") is now structural in a second way — a beacon
/// route that cannot emit a decision field cannot deny.
pub const ROUTE_PRE_TOOL_USE_TAINT: &str = "/claude/hook/pre_tool_use_taint";

/// `PreToolUse` (matcher `Edit|Write|MultiEdit|Bash`) — the V33 pre-mutation
/// checkpoint. Was `cimp --checkpoint-beacon`.
///
/// **The one route in this family whose handler must FINISH its work before it
/// replies.** "The checkpoint precedes the tool call" rests on the tool not
/// starting until the hook resolves, so the handler awaits the snapshot (bounded
/// by `loopback::TOOL_CHECKPOINT_BUDGET`, 1800 ms) and only then answers 200.
/// That is the shim's 2 s reply wait expressed the other way round: the app is
/// now on the *inside* of the wait rather than being polled across a socket by a
/// process that had to guess how long to listen.
///
/// It is why this entry's [`timeout_secs`] is 5 rather than 1 — see that
/// function.
pub const ROUTE_PRE_TOOL_USE_CHECKPOINT: &str = "/claude/hook/pre_tool_use_checkpoint";

/// Every route in this family, so the dispatcher's CHP observation and the
/// overlay generator agree about the surface without either restating it.
pub const ROUTES: &[&str] = &[
    ROUTE_USER_PROMPT_SUBMIT,
    ROUTE_PRE_COMPACT,
    ROUTE_PRE_TOOL_USE,
    ROUTE_POST_TOOL_USE,
    ROUTE_NOTIFICATION,
    ROUTE_SESSION_START,
    ROUTE_STOP,
    ROUTE_POST_TOOL_USE_RESULT,
    ROUTE_POST_TOOL_USE_FAILURE,
    ROUTE_SUBAGENT,
    ROUTE_PRE_TOOL_USE_TAINT,
    ROUTE_PRE_TOOL_USE_CHECKPOINT,
];

/// The CHP event one Claude ingress route feeds — the join the quiet detector
/// (`chp::note_event`) and the arbitration rule need in order to speak about
/// capabilities rather than about transports.
///
/// `None` for the routes whose event is not one arbitration can turn off: the
/// hello is the negotiation itself, and the four Phase J capability hooks have
/// no fallback reader to arbitrate against.
///
/// **[`ROUTE_POST_TOOL_USE_FAILURE`] is the deliberate exception to
/// one-route-one-event, and it maps to `None` rather than to
/// `session.tool_result`.** Two ids that can never be declared independently are
/// one id (the same reasoning that keeps `session.usage` off *both* sides of
/// Claude's hello): the failure entry is emitted from the same boolean as the
/// success entry, feeds the same core, the same consumer, the same drift token
/// and the same `served` predicate, so there is no per-tab decision a second
/// event could report. What mapping it here would cost is precise: `note_event`
/// **resets** a served capability's quiet counter, so a rare failure push would
/// silently rearm the detector that watches the common success entry — a live
/// breakage hidden by a tool that happened to fail. Staleness observation is
/// unaffected either way; `note_chp` reads a hook route's envelope from headers
/// before it consults this join.
pub fn chp_event(route: &str) -> Option<&'static str> {
    use crate::harness::chp;
    match route {
        ROUTE_USER_PROMPT_SUBMIT => Some(chp::EV_PROMPT),
        ROUTE_PRE_COMPACT => Some(chp::EV_CONTEXT_COMPACTION),
        ROUTE_PRE_TOOL_USE => Some(chp::EV_CONTEXT_SHOULD_READ),
        ROUTE_POST_TOOL_USE => Some(chp::EV_CONTEXT_POST_EDIT),
        ROUTE_NOTIFICATION => Some(chp::EV_PERMISSION_EVENT),
        ROUTE_STOP => Some(chp::EV_ASSISTANT_TEXT),
        ROUTE_POST_TOOL_USE_RESULT => Some(chp::EV_SESSION_TOOL_RESULT),
        ROUTE_SUBAGENT => Some(chp::EV_SESSION_SUBAGENT),
        ROUTE_PRE_TOOL_USE_TAINT => Some(chp::EV_TAINT_BEACON),
        ROUTE_PRE_TOOL_USE_CHECKPOINT => Some(chp::EV_CHECKPOINT_PRE_MUTATION),
        _ => None,
    }
}

/// Whether `route` is a harness-native Claude ingress route — i.e. one whose
/// identity arrives in headers rather than in the body.
///
/// Prefix **and** membership: the prefix is what makes the family recognisable
/// to a reader, the list is what makes it exact. A route under the prefix that
/// is not in [`ROUTES`] is not served, and must not be treated as one that is.
pub fn is_hook_route(route: &str) -> bool {
    route.starts_with(ROUTE_PREFIX) && ROUTES.contains(&route)
}

// ── the headers ─────────────────────────────────────────────────────────────

/// The cImp tab this hook serves, baked into the emitted hook entry at spawn.
/// **Caller-supplied like every other identity on this listener** — validated
/// against the user's configured AI tabs before anything is recorded.
pub const HEADER_TAB: &str = "X-CIMP-Tab";
/// The harness discriminator. Always the literal `claude` today; emitted rather
/// than assumed so the handler reads identity from one place.
pub const HEADER_AGENT: &str = "X-CIMP-Agent";
/// The CHP version the *overlay that wired this hook* speaks, substituted from
/// [`crate::harness::chp::CHP_VERSION`] at generation time exactly as the
/// generated OpenCode plugin substitutes it. This is what makes a Claude tab
/// launched by an older build legible as stale instead of mysterious.
pub const HEADER_CHP: &str = "X-CIMP-Chp";
/// The hello declaration — `{"serves":[…],"cannot":[{id,why}…]}` — carried on
/// the `SessionStart` entry only.
///
/// A hook's *body* is the harness's, so cImp cannot put anything in it; a header
/// is the only channel a generated hook entry has. The content is the same pair
/// the generated OpenCode plugin puts in its hello body, built by the same kind
/// of Rust from the same per-tab flags.
pub const HEADER_HELLO: &str = "X-CIMP-Hello";

/// The environment variable carrying the loopback bearer token into the Claude
/// child, named in each hook entry's `allowedEnvVars` so the harness will
/// substitute it into the `Authorization` header.
///
/// Unlisted variables substitute to the empty string, so this name is
/// load-bearing in two places at once and is spelled here for both.
pub const TOKEN_ENV: &str = "CIMP_HOOK_TOKEN";

// ── the timeout budget ──────────────────────────────────────────────────────

/// The `timeout` (seconds) every emitted hook entry carries.
///
/// **Pinned at generation, never typed into a template** (design § 5.2). The
/// five shims all budgeted **600 ms** for their loopback round trip — one
/// constant, `context_hook::TIMEOUT`, shared through `post_loopback` — with the
/// documented reason that "a slow/cold index never delays the prompt". Rounded
/// up to the nearest second, that is 1.
///
/// With the shim gone this number is the *whole* budget rather than a ceiling
/// over a shim that gave up first, which is why it must not drift upward by
/// accident: the harness default is 600 s, `UserPromptSubmit`'s is 30 s, and
/// either would turn a wedged handler into a wedged turn. A test on the emitted
/// overlay pins it.
pub const TIMEOUT_SECS: u64 = 1;

/// The `timeout` (seconds) the **pre-mutation checkpoint** entry carries — the
/// one route whose handler must finish its work before it answers.
///
/// Deliberately the value the deleted `--checkpoint-beacon` hook entry carried,
/// and for the same reason: it is a ceiling over a wait that is *supposed* to
/// happen, not a budget for a round trip. The app abandons an unfinished
/// snapshot at `loopback::TOOL_CHECKPOINT_BUDGET` (1800 ms) and answers, so this
/// is a backstop for a wedged listener rather than the mechanism — the same
/// two-timer relationship the shim had, with the outer timer now enforced by the
/// harness instead of by a process that had to guess. A test pins the ordering
/// (`5 s > TOOL_CHECKPOINT_BUDGET`), because the two constants live in different
/// files and nothing else keeps them ordered.
///
/// Everything else stays at [`TIMEOUT_SECS`]: 1 s is right for a hook that must
/// not delay a turn, and would be wrong here — a checkpoint abandoned at 1 s on
/// a large work tree is the feature not working on the trees that need it.
pub const TIMEOUT_CHECKPOINT_SECS: u64 = 5;

/// The pinned `timeout` for one emitted entry, **derived from its route** so the
/// number is decided in one place rather than typed per call site.
///
/// Design § 5.2's "timeouts are pinned at generation" with one documented
/// exception; see [`TIMEOUT_CHECKPOINT_SECS`].
pub fn timeout_secs(route: &str) -> u64 {
    match route {
        ROUTE_PRE_TOOL_USE_CHECKPOINT => TIMEOUT_CHECKPOINT_SECS,
        _ => TIMEOUT_SECS,
    }
}

// ── the hook-output vocabulary ──────────────────────────────────────────────

/// The `hookSpecificOutput.hookEventName` values cImp emits, spelled once.
pub const EVENT_USER_PROMPT_SUBMIT: &str = "UserPromptSubmit";
pub const EVENT_PRE_COMPACT: &str = "PreCompact";
pub const EVENT_PRE_TOOL_USE: &str = "PreToolUse";
pub const EVENT_POST_TOOL_USE: &str = "PostToolUse";
/// One of the two `hook_event_name` values [`ROUTE_NOTIFICATION`] dispatches on
/// — the one whose presence decides whether the classification fields are
/// required. `PermissionDenied` needs no constant here: nothing in this module
/// branches on it, and the classifier that does lives beside the state signal it
/// emits (`offload::loopback::classify_permission_event`).
pub const EVENT_NOTIFICATION: &str = "Notification";
/// V35 Phase L: the `hook_event_name` [`ROUTE_SUBAGENT`] dispatches on. Its
/// twin `SubagentStop` needs no constant for the same reason `PermissionDenied`
/// does not — nothing branches on it, it is simply "not a start".
pub const EVENT_SUBAGENT_START: &str = "SubagentStart";

/// The reply that says nothing: a 2xx JSON body with no directive in it.
///
/// The HTTP spelling of the shims' "print nothing and exit 0". Claude parses a
/// 2xx JSON body exactly as it parses a command hook's stdout, so an object with
/// no `hookSpecificOutput`, no `continue` and no `systemMessage` is a no-op —
/// and for [`ROUTE_PRE_TOOL_USE`] specifically it is what lets the tool proceed.
pub fn no_op() -> serde_json::Value {
    serde_json::json!({})
}

/// `hookSpecificOutput.additionalContext` for `event` — the shape three of the
/// five shims printed, built in one place so they cannot drift apart.
pub fn additional_context(event: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": text,
        }
    })
}

/// A `PreToolUse` deny with `reason` — the read advisor's only non-no-op answer,
/// byte-identical to what `read_hook.rs` printed.
pub fn deny(reason: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": EVENT_PRE_TOOL_USE,
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
}

// ── the drift tokens ────────────────────────────────────────────────────────

/// The `shim` token each converted hook still reports payload drift under.
///
/// **Deliberately unchanged from the shim names.** Three things join on these
/// strings and all three would break if Phase J renamed them: the loopback's
/// `DRIFT_SHIMS` key space, `contract::capability_for_payload_shim`'s
/// attribution, and the Advisor's per-shim de-duplication of
/// `drift.payload.v1`. There is also a live reason: a pre-upgrade tab is still
/// running the old shim binary and still POSTs these exact names to
/// `/activity/contract_drift`, so keeping them is what makes both paths land in
/// **one** bucket per capability instead of two.
pub const DRIFT_CONTEXT_HOOK: &str = "context_hook";
pub const DRIFT_COMPACT_HOOK: &str = "compact_hook";
pub const DRIFT_READ_HOOK: &str = "read_hook";
pub const DRIFT_NOTIFY_HOOK: &str = "notify_hook";

/// The two beacons' tokens, unchanged by the 2026-08-17 http migration for
/// exactly the reason the four above are unchanged: a tab open across the
/// upgrade is still running the old shim binary and still POSTs these strings to
/// `/activity/contract_drift`, and the registry rows that resolve them
/// (`claude.hook.taint_beacon` / `claude.hook.checkpoint_beacon`) keep their ids
/// and their tokens. One bucket per capability, two ways in.
pub const DRIFT_TAINT_BEACON: &str = "taint_beacon";
pub const DRIFT_CHECKPOINT_BEACON: &str = "checkpoint_beacon";

/// **The auto-check route's token, and it is new** (2026-08-17), closing the
/// recorded gap V35 Phase A opened as finding 2 and Phase J deliberately left
/// open: this was the ONE converted hook that reported no payload drift at all,
/// so a matcher or field rename stopped auto-check diagnostics with nothing
/// firing anywhere.
///
/// Named `post_edit_hook` and **not** `postedit_hook`, which is the name the
/// deleted shim binary would have used had it ever reported. The distinction is
/// deliberate: `postedit_hook` never appeared on the wire, so nothing can be
/// carrying it, and keeping the two spellings apart means a report under the old
/// name still resolves to nothing (as it always has) instead of quietly claiming
/// this row.
pub const DRIFT_POST_EDIT_HOOK: &str = "post_edit_hook";

/// V35 Phase L's three. These name no deleted binary — there never was one —
/// so they are named for the capability they carry, in the same `<thing>_hook`
/// shape so one glance at an Activity row says which family it came from.
///
/// They carry a second duty the Phase J tokens do not: a **quiet** report
/// (locked decision 7) rides the same channel, so a served capability that
/// stops pushing is attributed to the same capability row as a malformed
/// payload from it. One bucket per capability, two ways in.
pub const DRIFT_STOP_HOOK: &str = "stop_hook";
pub const DRIFT_TOOL_RESULT_HOOK: &str = "tool_result_hook";
pub const DRIFT_SUBAGENT_HOOK: &str = "subagent_hook";

/// The `missing` entry a quiet report carries.
///
/// Not a payload field name, and deliberately shaped so it cannot be mistaken
/// for one: what is missing is not a field in a message, it is the message.
pub const MISSING_PUSH: &str = "(no push — the hook stopped firing)";

// ── the payload ─────────────────────────────────────────────────────────────

/// One Claude Code hook-input payload, read leniently.
///
/// Every field is `#[serde(default)]` and nothing is `deny_unknown_fields`, for
/// the same two reasons the shims parsed with `serde_json::Value`: a hook that
/// rejects a payload is a hook that blocks a turn, and the whole point of the
/// drift reports below is that a missing field is *measured*, not fatal.
///
/// The union of six events' payloads in one struct rather than six structs: the
/// events share `session_id`/`cwd`/`hook_event_name`, the per-event fields are
/// disjoint, and one type is what lets [`contract_checks`] be a single
/// event-aware function the way `read_hook::contract_checks` already was.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct HookInput {
    /// Which event fired. Present on every documented payload; its ABSENCE is
    /// drift for [`ROUTE_NOTIFICATION`], which dispatches on it.
    #[serde(default)]
    pub hook_event_name: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: String,
    #[serde(default)]
    pub cwd: String,
    /// `UserPromptSubmit`.
    #[serde(default)]
    pub prompt: String,
    /// `PreCompact`: `"manual"` / `"auto"`. Forwarded, not branched on.
    #[serde(default)]
    pub trigger: String,
    /// `PreToolUse` / `PostToolUse`. `Option` because "absent" and "empty" are
    /// different answers to the read advisor's contract check.
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    /// `SessionStart`: `"startup"` / `"resume"` / `"clear"` / `"compact"`.
    #[serde(default)]
    pub source: String,
    /// **Speculative, and empty in every payload shape documented today.** The
    /// hook-input contract has no CLI version field — `session_id`,
    /// `transcript_path`, `cwd`, `permission_mode` and `hook_event_name` are the
    /// common set — so this is read opportunistically and left empty when
    /// absent. cImp learns Claude's version from the transcript's own top-level
    /// `version` instead (`oob::claude::cli_version_of`), and the CHP
    /// `harness_version` staleness arm therefore still has no Claude producer.
    /// See `docs/CHP.md` § 6.2.
    #[serde(default)]
    pub version: String,
    /// `Notification`, flat spelling.
    #[serde(default)]
    pub notification_type: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub title: String,
    /// `Notification`, the `type` alias.
    #[serde(default, rename = "type")]
    pub type_alias: String,
    /// `Notification`, nested spelling (`notification: {type, message}`).
    #[serde(default)]
    pub notification: serde_json::Value,
    // ── V35 Phase L ─────────────────────────────────────────────────────────
    /// `Stop` and `SubagentStop`: the complete final assistant message.
    ///
    /// **The whole basis of the TTS migration.** It is documented as the final
    /// assistant text, which is the same unit
    /// `harness::claude::read::assistant_texts` lifts out of an `assistant`
    /// transcript line — so the segmenter's input cadence is unchanged. What
    /// cImp does NOT know without a live turn is whether the two renderings are
    /// byte-identical (markdown fences, tool-use interleaving); the reader's
    /// per-block extraction and this per-message one can only differ in how
    /// several text blocks of one message are joined. `to_speakable` reduces
    /// both before segmentation, and the L1 canary asserts the reader half
    /// still produces substantive prose, so a divergence degrades to *what is
    /// spoken*, never to *whether anything is spoken*.
    #[serde(default)]
    pub last_assistant_message: String,
    /// `PostToolUse`: the tool's full result content. Read only for its SIZE
    /// ([`tool_result_chars`]) — the same estimated-token proxy the transcript
    /// reader computes, and deliberately not for anything else.
    #[serde(default)]
    pub tool_result: serde_json::Value,
    /// `PostToolUseFailure` (2026-08-17): what the tool failed with. Read for its
    /// SIZE through the same [`tool_result_chars`] the success half uses, and for
    /// nothing else — the transcript reader it replaces sizes a failed
    /// `tool_result` block's content with that one function too, without looking
    /// at `is_error`, so the two paths produce the same number for the same
    /// failure.
    ///
    /// `Value` rather than `String` on purpose: the payload documents a text
    /// error, but a reshape into `{type:"text", text}` blocks (the shape the
    /// success half already tolerates) must size rather than read as absent, and
    /// a shape NEITHER reader knows must report as drift rather than pass as an
    /// empty error. See the `PostToolUseFailure` arm of [`contract_checks`].
    #[serde(default)]
    pub error: serde_json::Value,
    // `tool_use_id` is documented on this payload and is deliberately NOT read.
    // The tool-result core keys nothing on it — the `UsageEvent::ToolResult`
    // row it writes has no id column, exactly as the transcript reader's does
    // not — so declaring the field would put a dependency in the registry that
    // no line of code has. An unread field is a contract cImp cannot notice
    // breaking.
    /// `SubagentStart` / `SubagentStop`: which sub-agent. This is the lifecycle
    /// key — an id that started and has not stopped is an agent running.
    #[serde(default)]
    pub agent_id: String,
    /// `SubagentStart` / `SubagentStop`: the sub-agent's type. Carried for the
    /// log line, never branched on: a new agent type must not change whether
    /// the avatar sees an agent.
    #[serde(default)]
    pub agent_type: String,
}

impl HookInput {
    /// The notification's type, read from every spelling the payload is
    /// documented or observed with — flat (`notification_type`), the `type`
    /// alias, or nested under a `notification` object.
    ///
    /// Moved verbatim from `notify_hook.rs`, whose module doc records WHY:
    /// the reference page could not be retrieved reliably enough to pin which
    /// shape ships, so both are read and the app-side classifier falls back to
    /// prose whenever no RECOGNIZED type arrives.
    pub fn notification_kind(&self) -> &str {
        first_non_empty(&[
            &self.notification_type,
            &self.type_alias,
            nested_str(&self.notification, "notification_type"),
            nested_str(&self.notification, "type"),
        ])
    }

    /// …and its prose, from the same four-way read.
    pub fn notification_message(&self) -> &str {
        first_non_empty(&[
            &self.message,
            &self.title,
            nested_str(&self.notification, "message"),
            nested_str(&self.notification, "title"),
        ])
    }

    /// The CLI version the payload declared, trimmed and non-empty — `""` in
    /// every shape documented today. See [`HookInput::version`].
    pub fn harness_version(&self) -> &str {
        self.version.trim()
    }

    /// V35 Phase L: whether this sub-agent payload is a START (rather than the
    /// `SubagentStop` half). Read off `hook_event_name`, the one field the
    /// shared route dispatches on.
    pub fn is_subagent_start(&self) -> bool {
        self.hook_event_name == EVENT_SUBAGENT_START
    }
}

/// The character length of a `PostToolUse` payload's `tool_result` — the
/// estimated-token proxy `session.tool_result` carries.
///
/// **Delegates to the transcript reader's own sizing**
/// ([`crate::harness::claude::read::tool_result_chars`]) on purpose. The two
/// paths carry the same shape (a plain string, or an array of `{type, text}`
/// blocks), they feed the same `UsageEvent::ToolResult` row, and the whole
/// point of arbitrating between them is that they must produce the *same
/// number* for the same result — which a second implementation could not
/// promise. It also means the Phase B fixture canary for
/// `claude.transcript.tool_result` is the leading check for both.
///
/// A shape neither reader recognises sizes to `0`, which is a silent zero — so
/// [`contract_checks`] reports it as drift rather than letting it pass as a
/// legitimately empty result (see that function's `PostToolUse`-result arm).
pub fn tool_result_chars(tool_result: &serde_json::Value) -> usize {
    crate::harness::claude::read::tool_result_chars(tool_result)
}

/// Whether a `tool_result` payload holds anything at all — the "empty is not
/// absent" half of the check above.
///
/// A tool that genuinely returned nothing is `null`, `""` or `[]`, and reports
/// no drift. A tool_result that is a non-empty object, or a non-empty array
/// with no text block cImp can read, IS drift: something is there and cImp
/// sized it at zero.
pub fn tool_result_is_present(tool_result: &serde_json::Value) -> bool {
    match tool_result {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        serde_json::Value::Object(map) => !map.is_empty(),
        _ => true,
    }
}

/// A string field of a nested JSON object, or `""`.
fn nested_str<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
}

/// The first non-empty candidate, or `""`. Lets one reader tolerate the payload
/// being flat or nested, and a field rename (`notification_type` ⇄ `type`,
/// `message` ⇄ `title`), without a release.
fn first_non_empty<'a>(candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .copied()
        .find(|s| !s.is_empty())
        .unwrap_or("")
}

// ── contract checks (V16 Feature 3, moved off the shims) ────────────────────

/// The required-field names a payload is missing — `(name, present-and-non-empty)`
/// pairs in, missing names out.
///
/// Kept split from the reporter so the check is unit-testable without a socket,
/// exactly as it was in `context_hook.rs`. Its second caller — the two beacon
/// shims, which built their check lists by hand — went away with them on
/// 2026-08-17; [`contract_checks`] now owns every list.
pub fn missing_fields(checks: &[(&'static str, bool)]) -> Vec<&'static str> {
    checks
        .iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| *name)
        .collect()
}

/// The `(field, present)` requiredness pairs for one hook payload, by ROUTE.
///
/// This is the four shims' `contract_checks` merged into one event-aware
/// function, field for field:
///
/// * `context_hook` required `session_id` + `cwd`;
/// * `compact_hook` the same two;
/// * `read_hook` those plus `tool_name`, plus `tool_input.file_path` for a
///   `Read` (and for an unknown tool — defensive), plus `tool_input.command`
///   for a `Bash`. A DIFFERENT `tool_name` is a matcher-config matter, not
///   payload drift, so only its absence counts;
/// * `notify_hook` required `hook_event_name`, `session_id`, `cwd`,
///   `transcript_path`, and — for a `Notification` only — some way to tell a
///   permission prompt from an idle one.
///
/// The auto-check route ([`ROUTE_POST_TOOL_USE`]) reported nothing at all until
/// 2026-08-17 — Phase A finding 2, recorded rather than assumed and deliberately
/// not closed by the http migration. It is closed **here**, under
/// [`DRIFT_POST_EDIT_HOOK`], and the two fields it asserts are exactly the two
/// the route cannot work without: `tool_name` (the handler re-checks it against
/// the edit tools and drops anything else) and `tool_input.file_path` (the file
/// the checks run against — absent, the diff is empty and the auto-check answers
/// nothing, silently). `session_id`/`cwd` come with `base` like everywhere else.
pub fn contract_checks(route: &str, input: &HookInput) -> Vec<(&'static str, bool)> {
    let base = |v: &mut Vec<(&'static str, bool)>| {
        v.push(("session_id", !input.session_id.is_empty()));
        v.push(("cwd", !input.cwd.is_empty()));
    };
    let mut out: Vec<(&'static str, bool)> = Vec::new();
    match route {
        ROUTE_USER_PROMPT_SUBMIT | ROUTE_PRE_COMPACT => base(&mut out),
        ROUTE_PRE_TOOL_USE => {
            base(&mut out);
            let tool = input.tool_name.as_deref();
            let file_path = nested_str(&input.tool_input, "file_path");
            let command = nested_str(&input.tool_input, "command");
            out.push(("tool_name", tool.is_some()));
            out.push((
                "tool_input.file_path",
                tool.is_some_and(|t| t != "Read") || !file_path.is_empty(),
            ));
            out.push((
                "tool_input.command",
                !matches!(tool, Some("Bash")) || !command.is_empty(),
            ));
        }
        ROUTE_NOTIFICATION => {
            let is_notification = input.hook_event_name == EVENT_NOTIFICATION;
            out.push(("hook_event_name", !input.hook_event_name.is_empty()));
            base(&mut out);
            out.push(("transcript_path", !input.transcript_path.is_empty()));
            out.push((
                "notification_type|message",
                !is_notification
                    || !input.notification_kind().is_empty()
                    || !input.notification_message().is_empty(),
            ));
        }
        // ── V35 Phase L ─────────────────────────────────────────────────────
        //
        // These three are the first hooks whose checks were written WITH the
        // capability rather than retrofitted onto a shim, so each one asserts
        // the field the capability would go silently to zero without — which is
        // the whole reason the row moved off Tier C.
        ROUTE_STOP => {
            base(&mut out);
            // The migration's single point of failure: no `last_assistant_
            // message` is a mute tab, and (before this line) it would have been
            // a mute tab with nothing anywhere saying why.
            out.push((
                "last_assistant_message",
                !input.last_assistant_message.trim().is_empty(),
            ));
        }
        ROUTE_POST_TOOL_USE_RESULT => {
            base(&mut out);
            out.push(("tool_name", input.tool_name.is_some()));
            // "Empty is not absent": a result that is genuinely empty is fine;
            // a result that is PRESENT and sizes to zero means neither shape
            // matched, i.e. the payload changed under us.
            out.push((
                "tool_result",
                !tool_result_is_present(&input.tool_result)
                    || tool_result_chars(&input.tool_result) > 0,
            ));
        }
        ROUTE_SUBAGENT => {
            out.push(("hook_event_name", !input.hook_event_name.is_empty()));
            base(&mut out);
            // Without an id there is no lifecycle — a start that cannot be
            // matched to its stop would wedge the avatar in Thinking, so the
            // handler drops such a payload and this is what says so out loud.
            out.push(("agent_id", !input.agent_id.trim().is_empty()));
        }
        // ── 2026-08-17 ──────────────────────────────────────────────────────
        //
        // Phase A finding 2, closed. See this function's doc for why these two
        // fields and not others.
        ROUTE_POST_TOOL_USE => {
            base(&mut out);
            out.push(("tool_name", input.tool_name.is_some()));
            out.push((
                "tool_input.file_path",
                !nested_str(&input.tool_input, "file_path").is_empty(),
            ));
        }
        // The failure half of the tool-result push. `error` gets the same
        // "empty is not absent" treatment `tool_result` gets on the success
        // route: a tool that failed with no message at all is odd but not
        // drift, while a PRESENT error that sizes to zero means neither reader
        // shape matched — i.e. the payload changed under us.
        ROUTE_POST_TOOL_USE_FAILURE => {
            base(&mut out);
            out.push(("tool_name", input.tool_name.is_some()));
            out.push((
                "error",
                !tool_result_is_present(&input.error) || tool_result_chars(&input.error) > 0,
            ));
        }
        // The two migrated beacons, field for field what the deleted shims
        // checked and in the order they reported them: `tool_name` is what the
        // rows name, and `cwd` is the one whose absence would break the
        // checkpoint silently (the snapshot would be taken against the wrong
        // root) rather than loudly.
        ROUTE_PRE_TOOL_USE_TAINT | ROUTE_PRE_TOOL_USE_CHECKPOINT => {
            out.push((
                "tool_name",
                input.tool_name.as_deref().is_some_and(|t| !t.is_empty()),
            ));
            base(&mut out);
        }
        // `SessionStart`'s only required field is one cImp supplies itself, in a
        // header, so it reports nothing.
        _ => {}
    }
    out
}

/// The drift token a route reports under, or `None` for the one that does not
/// report at all. See [`DRIFT_CONTEXT_HOOK`] for why these are the shim names.
///
/// **`SessionStart` is now the only `None`.** The auto-check route joined the
/// reporters on 2026-08-17 ([`DRIFT_POST_EDIT_HOOK`]), and the failure half of
/// the tool-result push shares its sibling's token deliberately: one capability,
/// one bucket, whether the payload that broke was a success or an error.
pub fn drift_token(route: &str) -> Option<&'static str> {
    match route {
        ROUTE_USER_PROMPT_SUBMIT => Some(DRIFT_CONTEXT_HOOK),
        ROUTE_PRE_COMPACT => Some(DRIFT_COMPACT_HOOK),
        ROUTE_PRE_TOOL_USE => Some(DRIFT_READ_HOOK),
        ROUTE_POST_TOOL_USE => Some(DRIFT_POST_EDIT_HOOK),
        ROUTE_NOTIFICATION => Some(DRIFT_NOTIFY_HOOK),
        ROUTE_STOP => Some(DRIFT_STOP_HOOK),
        ROUTE_POST_TOOL_USE_RESULT | ROUTE_POST_TOOL_USE_FAILURE => Some(DRIFT_TOOL_RESULT_HOOK),
        ROUTE_SUBAGENT => Some(DRIFT_SUBAGENT_HOOK),
        ROUTE_PRE_TOOL_USE_TAINT => Some(DRIFT_TAINT_BEACON),
        ROUTE_PRE_TOOL_USE_CHECKPOINT => Some(DRIFT_CHECKPOINT_BEACON),
        _ => None,
    }
}

/// The drift token a CHP event's **quiet** report (locked decision 7) arrives
/// under — the same bucket that event's payload drift uses, so one capability
/// has one channel however it broke.
///
/// `None` for an event whose silence is not reportable.
///
/// **`taint.beacon` and `checkpoint.pre_mutation` are deliberately absent even
/// though 2026-08-17 gave them push producers**, and there are two independent
/// reasons, either of which is sufficient:
///
///  * **No sound witness exists.** `chp::witness_of` returns `None` for both, so
///    `note_event` can never return them and an entry here would be unreachable.
///    A turn may legitimately never reach for `WebFetch` and never edit a file,
///    so any threshold would manufacture false reports — the same declared gap
///    `session.subagent` carries, for the same reason.
///  * **Both events have an OpenCode producer too.** These tokens name CLAUDE
///    registry rows, so if a future witness were wired, an OpenCode plugin's
///    silence would report under a Claude row. A per-agent token would be needed
///    first.
pub fn drift_token_for_event(event: &str) -> Option<&'static str> {
    use crate::harness::chp;
    match event {
        chp::EV_ASSISTANT_TEXT => Some(DRIFT_STOP_HOOK),
        chp::EV_SESSION_TOOL_RESULT => Some(DRIFT_TOOL_RESULT_HOOK),
        chp::EV_SESSION_SUBAGENT => Some(DRIFT_SUBAGENT_HOOK),
        _ => None,
    }
}

// ── the read advisor's request planner (moved from `read_hook.rs`) ──────────

/// The verdict-request a `PreToolUse` payload maps to.
pub struct ReadRequest {
    /// The file to check — a `Read`'s `file_path` as given (Claude passes it
    /// absolute), or a Bash whole-file read's path absolutized against cwd.
    pub file_path: String,
    /// The `Read` offset (`None` for a shell read — it's always a full read).
    pub offset: Option<u32>,
    /// The `Read` limit (`None` for a shell read).
    pub limit: Option<u32>,
    /// Prepended to a remind reason: empty for `Read`, the shell note for `Bash`.
    pub deny_prefix: &'static str,
}

/// The note prepended to a `Bash` interception's deny reason.
pub const BASH_DENY_PREFIX: &str = "answered without running the command — ";

/// Map a parsed hook payload to a [`ReadRequest`], or `None` when the tool
/// should proceed untouched (non-target tool, empty path, or a Bash command that
/// isn't a provable whole-file read). `cwd` is the already-resolved payload cwd,
/// used to absolutize a relative shell path.
///
/// Same verdict body for both tools — the only difference is the deny prefix —
/// so a `Read` and an equivalent `cat` get byte-identical advice modulo that
/// prefix.
pub fn plan_request(
    tool_name: Option<&str>,
    tool_input: &serde_json::Value,
    cwd: &str,
) -> Option<ReadRequest> {
    match tool_name {
        Some("Read") => {
            let file_path = nested_str(tool_input, "file_path");
            if file_path.trim().is_empty() {
                return None;
            }
            Some(ReadRequest {
                file_path: file_path.to_string(),
                offset: tool_input.get("offset").and_then(as_u32),
                limit: tool_input.get("limit").and_then(as_u32),
                deny_prefix: "",
            })
        }
        Some("Bash") => {
            let command = nested_str(tool_input, "command");
            // Strict: `Some(path)` only for a provable pure whole-file read of
            // one file. Anything else ⇒ let the command run.
            let path = crate::graph::shellread::whole_file_read(command)?;
            // Resolve a relative shell path against the payload cwd so the
            // server relativizes it the same way it does an absolute `Read`
            // file_path.
            let file_path = if std::path::Path::new(&path).is_absolute() {
                path
            } else {
                std::path::Path::new(cwd)
                    .join(&path)
                    .to_string_lossy()
                    .into_owned()
            };
            Some(ReadRequest {
                file_path,
                offset: None,
                limit: None,
                deny_prefix: BASH_DENY_PREFIX,
            })
        }
        // Future-proof: the same route may serve more matchers later.
        _ => None,
    }
}

/// A JSON number as a `u32`. `/context/should_read`'s body types `offset` and
/// `limit` as `u32`; the shim read them as `u64` and let serde narrow them at
/// the wire. Narrowing here instead keeps the one conversion in one place, and
/// an out-of-range value degrades to "no window", never to an error.
fn as_u32(v: &serde_json::Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}

// `resolve_cwd` and `tab_arg` lived here for the two surviving shim binaries and
// were deleted with them on 2026-08-17. Both were about a SEPARATE PROCESS
// resolving what an in-process handler already knows: `tab_arg` parsed the
// `--tab <id>` baked into a hook command's argv, which is now `HEADER_TAB`, and
// `resolve_cwd` fell back to the shim's own cwd — a fallback the app-side
// handlers must never take, because the app's cwd is its launch directory rather
// than the tab's project (`loopback::claude_hook_cwd` resolves the tab's
// configured directory instead).

// ── the hello declaration (design D3) ───────────────────────────────────────

/// One `cannot` entry: a CHP event this tab's overlay did not wire, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Unable {
    pub id: String,
    pub why: String,
}

/// The `serves` / `cannot` pair a generated overlay declares for one tab —
/// [`HEADER_HELLO`]'s payload, and the same two halves the generated OpenCode
/// plugin puts in its hello body.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hello {
    #[serde(default)]
    pub serves: Vec<String>,
    #[serde(default)]
    pub cannot: Vec<Unable>,
}

impl Hello {
    /// Declare one capability: served, or not-served with a reason. Never
    /// neither — `serves ∪ cannot` covers the whole Claude-servable vocabulary,
    /// which is what makes an absence readable as *unavailable* rather than as
    /// *nobody wrote it down*.
    pub fn declare(&mut self, on: bool, id: &str, why: &str) {
        if on {
            self.serves.push(id.to_string());
        } else {
            self.cannot.push(Unable {
                id: id.to_string(),
                why: why.to_string(),
            });
        }
    }

    /// The header value: compact JSON on one line. Header values may not contain
    /// CR/LF and `serde_json`'s compact form emits none.
    pub fn header_value(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Parse a [`HEADER_HELLO`] value. `None` on anything malformed — a hello
    /// that cannot be read is recorded as an empty declaration, never as an
    /// error, because a hook that 400s is a hook that perturbs a session start.
    pub fn parse(raw: &str) -> Option<Hello> {
        serde_json::from_str(raw).ok()
    }
}

// ── the emitted hook entry ──────────────────────────────────────────────────

/// One `type: "http"` hook entry, as it appears in the `--settings` overlay.
///
/// **Written here, once**, rather than five times in the overlay builder, for
/// the reason design § 5.2 gives: the `timeout` is load-bearing and "currently
/// survives only as a comment", and a hand-edited copy is how it drifts. The
/// overlay tests pin what this returns.
///
/// The shape, field by field:
///
/// * `url` — loopback, with the port baked at generation. Not a secret, and no
///   substitution is documented for the URL anyway.
/// * `headers.Authorization` — `Bearer $CIMP_HOOK_TOKEN`. The harness
///   substitutes it from its own environment **only because the variable is
///   named in `allowedEnvVars`**; an unlisted name substitutes to the empty
///   string, which would make every hook 401 silently. The token is deliberately
///   not a literal here: this object ends up as an argv value
///   (`--settings <json>`), and argv is the most casually readable thing on the
///   machine. `compose_ai_env` puts the value in the child's environment.
/// * `headers.X-CIMP-*` — the identity a hook body cannot carry (§ the module
///   doc). `X-CIMP-Chp` is substituted from [`crate::harness::chp::CHP_VERSION`]
///   and never typed as a literal, exactly as the generated OpenCode plugin
///   substitutes it.
/// * `timeout` — [`timeout_secs`] for this route, always explicit. The harness
///   defaults are 600 s (most events), 30 s (`UserPromptSubmit`) and 10 s
///   (`MessageDisplay`); inheriting any of them would turn a wedged handler into
///   a wedged turn. One route answers 5 rather than 1, with its reason at
///   [`TIMEOUT_CHECKPOINT_SECS`].
pub fn http_hook_entry(
    port: u16,
    tab: &str,
    route: &str,
    hello: Option<&Hello>,
) -> serde_json::Value {
    let mut headers = serde_json::Map::new();
    headers.insert(
        "Authorization".to_string(),
        serde_json::Value::String(format!("Bearer ${TOKEN_ENV}")),
    );
    headers.insert(
        HEADER_TAB.to_string(),
        serde_json::Value::String(tab.to_string()),
    );
    headers.insert(
        HEADER_AGENT.to_string(),
        serde_json::Value::String("claude".to_string()),
    );
    headers.insert(
        HEADER_CHP.to_string(),
        serde_json::Value::String(crate::harness::chp::CHP_VERSION.to_string()),
    );
    if let Some(hello) = hello {
        headers.insert(
            HEADER_HELLO.to_string(),
            serde_json::Value::String(hello.header_value()),
        );
    }
    serde_json::json!({
        "type": "http",
        "url": format!("http://127.0.0.1:{port}{route}"),
        "headers": serde_json::Value::Object(headers),
        "allowedEnvVars": [TOKEN_ENV],
        "timeout": timeout_secs(route),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── platform-shaped path fixtures ──────────────────────────────────────
    //
    // The behaviour under test is platform-NEUTRAL: an absolute shell path is
    // passed through untouched, a relative one is joined against the payload
    // cwd. `Path::is_absolute()` — the branch `plan_request` actually takes —
    // is not: `C:\proj\a.rs` is a *relative* path on Linux, so a hard-coded
    // Windows literal silently flips these tests onto the other branch and
    // fails rather than testing anything.

    #[cfg(windows)]
    const CWD: &str = "C:/proj";
    #[cfg(not(windows))]
    const CWD: &str = "/proj";

    #[cfg(windows)]
    const ABS_FILE: &str = "C:\\proj\\a.rs";
    #[cfg(not(windows))]
    const ABS_FILE: &str = "/proj/a.rs";

    #[cfg(windows)]
    const OTHER_CWD: &str = "C:/other";
    #[cfg(not(windows))]
    const OTHER_CWD: &str = "/other";

    #[test]
    fn read_payload_forwards_offset_and_limit_no_prefix() {
        let input = json!({ "file_path": "C:/proj/big.rs", "offset": 40, "limit": 80 });
        let r = plan_request(Some("Read"), &input, "C:/proj").expect("read request");
        assert_eq!(r.file_path, "C:/proj/big.rs");
        assert_eq!(r.offset, Some(40));
        assert_eq!(r.limit, Some(80));
        assert_eq!(r.deny_prefix, "");
    }

    #[test]
    fn read_empty_path_is_skipped() {
        assert!(plan_request(Some("Read"), &json!({ "file_path": "  " }), "C:/proj").is_none());
        assert!(plan_request(Some("Read"), &json!({}), "C:/proj").is_none());
    }

    #[test]
    fn bash_whole_file_read_resolves_relative_path_against_cwd() {
        let r = plan_request(Some("Bash"), &json!({ "command": "cat foo.txt" }), CWD)
            .expect("bash request");
        assert!(
            std::path::Path::new(&r.file_path).is_absolute(),
            "got {}",
            r.file_path
        );
        assert!(
            r.file_path.replace('\\', "/").ends_with("proj/foo.txt"),
            "got {}",
            r.file_path
        );
        assert_eq!(r.offset, None);
        assert_eq!(r.limit, None);
        assert_eq!(r.deny_prefix, BASH_DENY_PREFIX);
    }

    #[test]
    fn bash_absolute_path_is_left_as_is() {
        let r = plan_request(
            Some("Bash"),
            &json!({ "command": format!("cat {ABS_FILE}") }),
            OTHER_CWD,
        )
        .expect("bash request");
        assert_eq!(r.file_path, ABS_FILE);
    }

    #[test]
    fn bash_non_whole_file_read_is_skipped() {
        assert!(plan_request(
            Some("Bash"),
            &json!({ "command": "cat a | grep x" }),
            "C:/p"
        )
        .is_none());
        assert!(plan_request(Some("Bash"), &json!({ "command": "head -50 f" }), "C:/p").is_none());
        assert!(plan_request(Some("Bash"), &json!({ "command": "npm test" }), "C:/p").is_none());
        assert!(plan_request(Some("Bash"), &json!({}), "C:/p").is_none());
    }

    #[test]
    fn non_target_tools_are_skipped() {
        assert!(plan_request(Some("Edit"), &json!({ "file_path": "a" }), "C:/p").is_none());
        assert!(plan_request(None, &json!({ "file_path": "a" }), "C:/p").is_none());
    }

    /// Verdict parity: a `Read` and the equivalent `cat` produce the same body
    /// (same file_path/offset/limit) — the only difference is the deny prefix.
    #[test]
    fn read_and_cat_yield_identical_body_modulo_prefix() {
        let bash =
            plan_request(Some("Bash"), &json!({ "command": "cat foo.txt" }), CWD).expect("bash");
        let read = plan_request(
            Some("Read"),
            &json!({ "file_path": bash.file_path.clone() }),
            CWD,
        )
        .expect("read");
        assert_eq!(bash.file_path, read.file_path);
        assert_eq!((bash.offset, bash.limit), (None, None));
        assert_eq!((read.offset, read.limit), (None, None));
        assert_eq!(read.deny_prefix, "");
        assert_eq!(bash.deny_prefix, BASH_DENY_PREFIX);
    }

    /// The read advisor's payload checks, moved off `read_hook.rs` without a
    /// change of meaning: `command` is required only for a `Bash`, `file_path`
    /// only for a `Read` (or an unknown tool), and the base fields always.
    #[test]
    fn contract_checks_require_command_only_for_bash() {
        let input = |tool: Option<&str>, file: &str, command: &str| HookInput {
            session_id: "s".into(),
            cwd: "c".into(),
            tool_name: tool.map(str::to_string),
            tool_input: json!({ "file_path": file, "command": command }),
            ..Default::default()
        };
        let miss = missing_fields(&contract_checks(
            ROUTE_PRE_TOOL_USE,
            &input(Some("Bash"), "", ""),
        ));
        assert!(miss.contains(&"tool_input.command"), "got {miss:?}");
        assert!(
            !miss.contains(&"tool_input.file_path"),
            "file_path not required for Bash: {miss:?}"
        );
        let ok = missing_fields(&contract_checks(
            ROUTE_PRE_TOOL_USE,
            &input(Some("Bash"), "", "cat f"),
        ));
        assert!(ok.is_empty(), "got {ok:?}");
        let rmiss = missing_fields(&contract_checks(
            ROUTE_PRE_TOOL_USE,
            &input(Some("Read"), "", ""),
        ));
        assert!(rmiss.contains(&"tool_input.file_path"), "got {rmiss:?}");
        assert!(!rmiss.contains(&"tool_input.command"), "got {rmiss:?}");
        let base = missing_fields(&contract_checks(ROUTE_PRE_TOOL_USE, &HookInput::default()));
        assert!(
            base.contains(&"session_id") && base.contains(&"cwd") && base.contains(&"tool_name")
        );
    }

    /// The notification classification fields, moved off `notify_hook.rs`:
    /// required only for a `Notification`, and read from the flat, aliased and
    /// nested spellings alike (the payload shape upstream never pinned).
    #[test]
    fn contract_checks_require_classification_only_for_notification() {
        let notif = |kind: &str, msg: &str| HookInput {
            hook_event_name: EVENT_NOTIFICATION.into(),
            session_id: "s".into(),
            cwd: "c".into(),
            transcript_path: "t".into(),
            notification_type: kind.into(),
            message: msg.into(),
            ..Default::default()
        };
        assert_eq!(
            missing_fields(&contract_checks(ROUTE_NOTIFICATION, &notif("", ""))),
            vec!["notification_type|message"]
        );
        assert!(missing_fields(&contract_checks(
            ROUTE_NOTIFICATION,
            &notif("permission_prompt", "")
        ))
        .is_empty());
        assert!(missing_fields(&contract_checks(
            ROUTE_NOTIFICATION,
            &notif("", "Claude needs your permission to use Bash")
        ))
        .is_empty());
        // PermissionDenied carries neither and that is not drift.
        let denied = HookInput {
            hook_event_name: "PermissionDenied".into(),
            session_id: "s".into(),
            cwd: "c".into(),
            transcript_path: "t".into(),
            ..Default::default()
        };
        assert!(missing_fields(&contract_checks(ROUTE_NOTIFICATION, &denied)).is_empty());
        // …and every mapping field is reported by name when the payload is bare.
        let miss = missing_fields(&contract_checks(ROUTE_NOTIFICATION, &HookInput::default()));
        for f in ["hook_event_name", "session_id", "cwd", "transcript_path"] {
            assert!(miss.contains(&f), "{f} missing from {miss:?}");
        }
    }

    /// The type/message pair is read from the flat, aliased and nested shapes —
    /// whichever the installed Claude Code actually sends.
    #[test]
    fn reads_notification_fields_from_flat_alias_or_nested_payloads() {
        let flat: HookInput =
            serde_json::from_value(json!({ "notification_type": "permission_prompt" })).unwrap();
        let alias: HookInput = serde_json::from_value(json!({ "type": "idle_prompt" })).unwrap();
        let nested: HookInput = serde_json::from_value(
            json!({ "notification": { "type": "permission_prompt", "message": "m" } }),
        )
        .unwrap();
        assert_eq!(flat.notification_kind(), "permission_prompt");
        assert_eq!(alias.notification_kind(), "idle_prompt");
        assert_eq!(nested.notification_kind(), "permission_prompt");
        assert_eq!(nested.notification_message(), "m");
        assert_eq!(HookInput::default().notification_kind(), "");
        assert_eq!(first_non_empty(&["", "", "x", "y"]), "x");
        assert_eq!(first_non_empty(&[]), "");
        // A non-string field is not a value.
        let odd: HookInput = serde_json::from_value(json!({ "notification": { "type": 3 } })).unwrap();
        assert_eq!(odd.notification_kind(), "");
    }

    /// The hook-output builders emit exactly the shapes the deleted shims
    /// printed — and never `terminalSequence`, which is not a CHP capability
    /// (design § 5.2). A handler that grows one writes escape sequences into the
    /// PTY cImp renders.
    #[test]
    fn the_output_shapes_match_the_shims_and_never_carry_a_terminal_sequence() {
        let ctx = additional_context(EVENT_USER_PROMPT_SUBMIT, "digest");
        assert_eq!(ctx["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(ctx["hookSpecificOutput"]["additionalContext"], "digest");
        let d = deny("because");
        assert_eq!(d["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(d["hookSpecificOutput"]["permissionDecisionReason"], "because");
        assert_eq!(d["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        // The no-op carries no directive at all — the HTTP spelling of "print
        // nothing, exit 0".
        assert_eq!(no_op(), json!({}));
        for v in [
            ctx,
            d,
            no_op(),
            additional_context(EVENT_PRE_COMPACT, "block"),
            additional_context(EVENT_POST_TOOL_USE, "diag"),
        ] {
            let s = v.to_string();
            assert!(!s.contains("terminalSequence"), "got {s}");
            assert!(!s.contains("continue"), "no handler here halts a turn: {s}");
        }
    }

    /// The hello declaration round-trips through one header value, and every
    /// capability lands on exactly one side of it.
    #[test]
    fn the_hello_declaration_round_trips_and_is_exhaustive() {
        let mut h = Hello::default();
        h.serves.push("hello".to_string());
        h.declare(true, "prompt", "");
        h.declare(false, "context.should_read", "the read advisor is off for this tab");
        let raw = h.header_value();
        assert!(!raw.contains('\n') && !raw.contains('\r'), "header-safe: {raw}");
        let back = Hello::parse(&raw).expect("round trip");
        assert_eq!(back.serves, vec!["hello", "prompt"]);
        assert_eq!(back.cannot.len(), 1);
        assert_eq!(back.cannot[0].id, "context.should_read");
        assert!(!back.cannot[0].why.is_empty(), "a `cannot` always says why");
        assert!(Hello::parse("not json").is_none());
    }

    /// The route table, the drift tokens and the check surface agree — a new
    /// route cannot land half-wired.
    #[test]
    fn every_route_is_declared_once_and_its_drift_token_is_a_shim_name() {
        let mut sorted = ROUTES.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        sorted.dedup();
        assert_eq!(n, sorted.len(), "a route is declared twice");
        for r in ROUTES {
            assert!(is_hook_route(r));
            assert!(
                r.starts_with(ROUTE_PREFIX),
                "{r} is not under the declared prefix"
            );
        }
        assert!(!is_hook_route("/context/retrieve"));
        assert!(!is_hook_route(ROUTE_PREFIX));
        // The four converted reporters keep the shim names…
        assert_eq!(drift_token(ROUTE_USER_PROMPT_SUBMIT), Some("context_hook"));
        assert_eq!(drift_token(ROUTE_PRE_COMPACT), Some("compact_hook"));
        assert_eq!(drift_token(ROUTE_PRE_TOOL_USE), Some("read_hook"));
        assert_eq!(drift_token(ROUTE_NOTIFICATION), Some("notify_hook"));
        // …and so do the two migrated beacons, because a tab open across the
        // upgrade still POSTs these strings from its old shim binary.
        assert_eq!(drift_token(ROUTE_PRE_TOOL_USE_TAINT), Some("taint_beacon"));
        assert_eq!(
            drift_token(ROUTE_PRE_TOOL_USE_CHECKPOINT),
            Some("checkpoint_beacon")
        );
        // **Phase A finding 2, CLOSED 2026-08-17.** This assertion used to read
        // `None` with the note "recorded, not quietly fixed" — the gap was that
        // nothing anywhere lagged the auto-check route, so a matcher or field
        // rename killed its diagnostics in silence. It reports now, under a token
        // that is deliberately NOT the never-shipped shim name.
        assert_eq!(drift_token(ROUTE_POST_TOOL_USE), Some("post_edit_hook"));
        assert_ne!(
            drift_token(ROUTE_POST_TOOL_USE),
            Some("postedit_hook"),
            "the never-shipped shim spelling must stay unclaimed"
        );
        assert!(!contract_checks(ROUTE_POST_TOOL_USE, &HookInput::default()).is_empty());
        // The failure half shares its sibling's bucket: one capability, one
        // token, whichever half of it broke.
        assert_eq!(
            drift_token(ROUTE_POST_TOOL_USE_FAILURE),
            drift_token(ROUTE_POST_TOOL_USE_RESULT)
        );
        // `SessionStart` is now the only route that reports nothing, and it
        // really has nothing to check: its one required field is a header cImp
        // supplies itself.
        assert_eq!(drift_token(ROUTE_SESSION_START), None);
        assert!(contract_checks(ROUTE_SESSION_START, &HookInput::default()).is_empty());
        assert_eq!(
            ROUTES.iter().filter(|r| drift_token(r).is_none()).count(),
            1,
            "every route but the hello reports payload drift"
        );
    }

    /// The whole point of the timeout column: it is 1 s, derived from the shims'
    /// 600 ms budget, and not the harness's 600 s / 30 s defaults — with ONE
    /// documented exception, the checkpoint route, whose handler is supposed to
    /// take time because the tool waits for it.
    #[test]
    fn the_pinned_timeout_is_the_shims_budget_rounded_up() {
        assert_eq!(TIMEOUT_SECS, 1, "600 ms rounded up to whole seconds");
        assert_eq!(
            TIMEOUT_CHECKPOINT_SECS, 5,
            "the deleted shim's own hook-entry ceiling, kept: it is a backstop over a wait \
             the app bounds itself, not a round-trip budget"
        );
        // Derived from the route, so the exception is one `match` arm rather
        // than a hand-typed number at a call site.
        for r in ROUTES {
            let want = if *r == ROUTE_PRE_TOOL_USE_CHECKPOINT {
                TIMEOUT_CHECKPOINT_SECS
            } else {
                TIMEOUT_SECS
            };
            assert_eq!(timeout_secs(r), want, "{r}");
        }
        assert_eq!(
            ROUTES
                .iter()
                .filter(|r| timeout_secs(r) != TIMEOUT_SECS)
                .count(),
            1,
            "exactly one route may deviate from the 1 s budget, and it is the one \
             whose ordering guarantee is the wait"
        );
    }

    // ── 2026-08-17: the migrated beacons and the failure half ───────────────

    /// The two beacon routes assert exactly the three fields their deleted shims
    /// asserted, in the same order — the payload half of the migration is a
    /// relocation, and this is what makes that a fact rather than a claim.
    #[test]
    fn the_migrated_beacons_check_the_fields_their_shims_checked() {
        for route in [ROUTE_PRE_TOOL_USE_TAINT, ROUTE_PRE_TOOL_USE_CHECKPOINT] {
            assert_eq!(
                missing_fields(&contract_checks(route, &HookInput::default())),
                vec!["tool_name", "session_id", "cwd"],
                "{route}"
            );
            let full = HookInput {
                session_id: "s-1".into(),
                cwd: "/proj".into(),
                tool_name: Some("WebFetch".into()),
                ..Default::default()
            };
            assert!(
                missing_fields(&contract_checks(route, &full)).is_empty(),
                "the happy path never reports: {route}"
            );
            // An EMPTY tool name is absent, not present — the shims read a
            // string field that defaults to `""`, and a checkpoint attributed to
            // `claude:` is a row that names no call.
            let anon = HookInput {
                tool_name: Some(String::new()),
                ..full.clone()
            };
            assert!(
                missing_fields(&contract_checks(route, &anon)).contains(&"tool_name"),
                "{route}"
            );
        }
    }

    /// The auto-check route's new report (Phase A finding 2's closure) names the
    /// two fields whose absence makes the diagnostics silently empty, and stays
    /// quiet on a complete payload.
    #[test]
    fn the_auto_check_route_now_reports_its_missing_fields() {
        let bare = missing_fields(&contract_checks(ROUTE_POST_TOOL_USE, &HookInput::default()));
        for f in ["session_id", "cwd", "tool_name", "tool_input.file_path"] {
            assert!(bare.contains(&f), "{f} missing from {bare:?}");
        }
        let full = HookInput {
            session_id: "s".into(),
            cwd: "c".into(),
            tool_name: Some("Edit".into()),
            tool_input: json!({ "file_path": "src/main.rs" }),
            ..Default::default()
        };
        assert!(missing_fields(&contract_checks(ROUTE_POST_TOOL_USE, &full)).is_empty());
        // A renamed path field is the exact failure the gap left invisible.
        let renamed = HookInput {
            tool_input: json!({ "path": "src/main.rs" }),
            ..full
        };
        let miss = missing_fields(&contract_checks(ROUTE_POST_TOOL_USE, &renamed));
        assert_eq!(miss, vec!["tool_input.file_path"], "got {miss:?}");
    }

    /// `PostToolUseFailure` sizes its `error` through the SAME function the
    /// success half sizes `tool_result` with, and separates an empty error from
    /// an unreadable one exactly as that route does.
    #[test]
    fn the_failure_route_sizes_the_error_like_a_tool_result() {
        let sized = |v: serde_json::Value| {
            let input = HookInput {
                session_id: "s".into(),
                cwd: "c".into(),
                tool_name: Some("Bash".into()),
                error: v,
                ..Default::default()
            };
            (
                tool_result_chars(&input.error),
                missing_fields(&contract_checks(ROUTE_POST_TOOL_USE_FAILURE, &input)),
            )
        };
        let (chars, miss) = sized(json!("exit status 1"));
        assert_eq!(chars, 13);
        assert!(miss.is_empty());
        let (chars, miss) = sized(json!([{ "type": "text", "text": "boom" }]));
        assert_eq!(chars, 4, "the block shape sizes too");
        assert!(miss.is_empty());
        for empty in [json!(null), json!(""), json!([])] {
            let (chars, miss) = sized(empty.clone());
            assert_eq!(chars, 0);
            assert!(miss.is_empty(), "an empty error is not drift: {empty}");
        }
        for reshaped in [json!({ "message": "boom" }), json!([{ "type": "text", "body": "x" }])] {
            let (chars, miss) = sized(reshaped.clone());
            assert_eq!(chars, 0);
            assert!(
                miss.contains(&"error"),
                "a present-but-unsizeable error must be reported: {reshaped}"
            );
        }
        // …and the tool name, without which the row cannot be attributed.
        let anon = HookInput {
            session_id: "s".into(),
            cwd: "c".into(),
            error: json!("boom"),
            ..Default::default()
        };
        assert!(
            missing_fields(&contract_checks(ROUTE_POST_TOOL_USE_FAILURE, &anon))
                .contains(&"tool_name")
        );
    }

    // ── V35 Phase L ─────────────────────────────────────────────────────────

    /// The three new routes carry the fields their capabilities would go
    /// silently to zero without, and say so when they are absent.
    #[test]
    fn the_phase_l_routes_report_the_field_each_capability_lives_on() {
        // Stop: no text is a mute tab.
        let bare = HookInput {
            session_id: "s".into(),
            cwd: "c".into(),
            ..Default::default()
        };
        assert_eq!(
            missing_fields(&contract_checks(ROUTE_STOP, &bare)),
            vec!["last_assistant_message"]
        );
        let spoke = HookInput {
            last_assistant_message: "Something was said.".into(),
            ..bare.clone()
        };
        assert!(missing_fields(&contract_checks(ROUTE_STOP, &spoke)).is_empty());
        // Whitespace is not text — "empty is not absent" in the other
        // direction: a payload that is present but blank is still a mute tab.
        let blank = HookInput {
            last_assistant_message: "   \n ".into(),
            ..bare.clone()
        };
        assert!(missing_fields(&contract_checks(ROUTE_STOP, &blank))
            .contains(&"last_assistant_message"));

        // Sub-agent: no id means a lifecycle that can never be closed.
        let start = HookInput {
            hook_event_name: EVENT_SUBAGENT_START.into(),
            agent_id: "agent_1".into(),
            ..bare.clone()
        };
        assert!(missing_fields(&contract_checks(ROUTE_SUBAGENT, &start)).is_empty());
        assert!(start.is_subagent_start());
        let stop = HookInput {
            hook_event_name: "SubagentStop".into(),
            ..start.clone()
        };
        assert!(!stop.is_subagent_start());
        let anon = HookInput {
            agent_id: "  ".into(),
            ..start.clone()
        };
        let miss = missing_fields(&contract_checks(ROUTE_SUBAGENT, &anon));
        assert!(miss.contains(&"agent_id"), "got {miss:?}");
    }

    /// Tool-result sizing: the two documented shapes size, an EMPTY result is
    /// not drift, and a present-but-unreadable one IS.
    ///
    /// That last distinction is the whole check. A tool that returned nothing
    /// and a payload cImp can no longer read both size to zero, and treating
    /// them the same is how a reshape becomes a silent zero.
    #[test]
    fn tool_result_sizing_separates_an_empty_result_from_an_unreadable_one() {
        let sized = |v: serde_json::Value| {
            let input = HookInput {
                session_id: "s".into(),
                cwd: "c".into(),
                tool_name: Some("Read".into()),
                tool_result: v,
                ..Default::default()
            };
            (
                tool_result_chars(&input.tool_result),
                missing_fields(&contract_checks(ROUTE_POST_TOOL_USE_RESULT, &input)),
            )
        };
        // A plain string, and an array of text blocks — the two shapes the
        // transcript reader already knows, reused rather than re-implemented.
        let (chars, miss) = sized(json!("twelve chars"));
        assert_eq!(chars, 12);
        assert!(miss.is_empty());
        let (chars, miss) = sized(json!([
            { "type": "text", "text": "ab" },
            { "type": "image", "source": "…" },
            { "type": "text", "text": "cde" },
        ]));
        assert_eq!(chars, 5, "non-text blocks do not count");
        assert!(miss.is_empty());
        // Genuinely empty: nothing to size, nothing to report.
        for empty in [json!(null), json!(""), json!([])] {
            let (chars, miss) = sized(empty.clone());
            assert_eq!(chars, 0);
            assert!(miss.is_empty(), "an empty result is not drift: {empty}");
        }
        // Present and unreadable: something is there and cImp sized it at zero.
        for reshaped in [json!({ "output": "hello" }), json!([{ "type": "text", "body": "x" }])] {
            let (chars, miss) = sized(reshaped.clone());
            assert_eq!(chars, 0);
            assert!(
                miss.contains(&"tool_result"),
                "a present-but-unsizeable result must be reported: {reshaped}"
            );
        }
        // …and the tool name, without which a row cannot be attributed.
        let anon = HookInput {
            session_id: "s".into(),
            cwd: "c".into(),
            tool_result: json!("x"),
            ..Default::default()
        };
        assert!(missing_fields(&contract_checks(ROUTE_POST_TOOL_USE_RESULT, &anon))
            .contains(&"tool_name"));
    }

    /// Every route maps to exactly one CHP event or to none, and every event
    /// whose SILENCE is reportable maps back to a drift token.
    ///
    /// The two directions are what let a quiet report name a capability rather
    /// than a transport — and what stops a new route from being observed for
    /// staleness while being invisible to arbitration.
    #[test]
    fn the_route_to_event_join_is_total_and_reversible() {
        use crate::harness::chp;
        let mut events: Vec<&str> = ROUTES.iter().filter_map(|r| chp_event(r)).collect();
        // TWO routes map to no event, and each `None` is a decision with its own
        // reason recorded at `chp_event`: `SessionStart` IS the negotiation, and
        // `PostToolUseFailure` carries the error half of an event its sibling
        // route owns — mapping it would let a rare failure push reset the quiet
        // counter watching the common success entry. The injectivity below is
        // what makes that the only way to express "same capability, second
        // entry", so the two facts are one design rather than two.
        assert_eq!(
            events.len(),
            ROUTES.len() - 2,
            "exactly two routes map to no CHP event: the hello, and the failure half"
        );
        assert_eq!(chp_event(ROUTE_SESSION_START), None);
        assert_eq!(chp_event(ROUTE_POST_TOOL_USE_FAILURE), None);
        // …and it is NOT invisible to the capability it belongs to: it reports
        // drift under the same token and its handler consults the same `served`
        // predicate as the success half (asserted in `chp`'s arbitration test).
        assert_eq!(
            drift_token(ROUTE_POST_TOOL_USE_FAILURE),
            drift_token_for_event(chp::EV_SESSION_TOOL_RESULT)
        );
        events.sort_unstable();
        let n = events.len();
        events.dedup();
        assert_eq!(n, events.len(), "two routes claim one CHP event");
        // Every mapped event is really in the vocabulary.
        for e in &events {
            assert!(
                chp::EVENTS.iter().any(|x| x.id == *e),
                "`{e}` is not a CHP event"
            );
        }
        // The reverse join, for the three that can go quiet.
        assert_eq!(
            drift_token_for_event(chp::EV_ASSISTANT_TEXT),
            drift_token(ROUTE_STOP),
            "a capability's payload drift and its silence must land in ONE bucket"
        );
        assert_eq!(
            drift_token_for_event(chp::EV_SESSION_TOOL_RESULT),
            drift_token(ROUTE_POST_TOOL_USE_RESULT)
        );
        assert_eq!(
            drift_token_for_event(chp::EV_SESSION_SUBAGENT),
            drift_token(ROUTE_SUBAGENT)
        );
        assert_eq!(drift_token_for_event(chp::EV_PROMPT), None);
        // The two that never migrated have no token because they have no
        // producer to go quiet — the upstream limitation, restated as code.
        assert_eq!(drift_token_for_event(chp::EV_SESSION_USAGE), None);
        assert_eq!(drift_token_for_event(chp::EV_SESSION_CONTEXT), None);
        // The two beacons DO have producers as of 2026-08-17 and still have no
        // quiet token, which is a declared gap rather than an omission: no
        // witness proves either should have fired, and both events also have an
        // OpenCode producer these Claude-named tokens would misattribute. See
        // `drift_token_for_event`.
        for event in [chp::EV_TAINT_BEACON, chp::EV_CHECKPOINT_PRE_MUTATION] {
            assert_eq!(drift_token_for_event(event), None, "{event}");
            assert_eq!(
                chp::witness_of(event),
                None,
                "`{event}` gained a witness — wire its quiet token, and make it \
                 per-agent first (both harnesses push it)"
            );
        }
    }
}
