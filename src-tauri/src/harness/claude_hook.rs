//! V35 Phase J — **Claude Code's L1**: the harness POSTs its own hook payloads
//! straight at cImp's loopback as `type: "http"` hooks, and the five shim
//! binaries that used to stand between them are gone.
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

/// Every route in this family, so the dispatcher's CHP observation and the
/// overlay generator agree about the surface without either restating it.
pub const ROUTES: &[&str] = &[
    ROUTE_USER_PROMPT_SUBMIT,
    ROUTE_PRE_COMPACT,
    ROUTE_PRE_TOOL_USE,
    ROUTE_POST_TOOL_USE,
    ROUTE_NOTIFICATION,
    ROUTE_SESSION_START,
];

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
/// exactly as it was in `context_hook.rs`. Still used by the two surviving
/// Claude shim binaries (`taint_beacon`, `checkpoint_beacon`) as well as by
/// [`contract_checks`].
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
/// `postedit_hook` is the one shim that never reported drift at all (Phase A
/// finding 2). Phase J does **not** silently fix that: [`ROUTE_POST_TOOL_USE`]
/// gets no checks here and its registry row keeps saying so, because inventing
/// a report for it would move a recorded gap into a footnote.
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
        // `PostToolUse` (see above) and `SessionStart` (whose only required
        // field is one cImp supplies itself, in a header) report nothing.
        _ => {}
    }
    out
}

/// The drift token a route reports under, or `None` for the two that do not
/// report at all. See [`DRIFT_CONTEXT_HOOK`] for why these are the shim names.
pub fn drift_token(route: &str) -> Option<&'static str> {
    match route {
        ROUTE_USER_PROMPT_SUBMIT => Some(DRIFT_CONTEXT_HOOK),
        ROUTE_PRE_COMPACT => Some(DRIFT_COMPACT_HOOK),
        ROUTE_PRE_TOOL_USE => Some(DRIFT_READ_HOOK),
        ROUTE_NOTIFICATION => Some(DRIFT_NOTIFY_HOOK),
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

// ── what the two surviving shim binaries still need ─────────────────────────

/// The working directory a hook payload names, falling back to the shim's own
/// process cwd when the field is absent/empty.
///
/// **For the two surviving Claude shim binaries only** (`cimp --taint-beacon`,
/// `cimp --checkpoint-beacon`). Claude spawns hook processes in the project
/// directory, so the fallback is usually right *for a process Claude spawned* —
/// which is exactly why the app-side handlers must NOT use it: the app's own cwd
/// is its launch directory, not the tab's project. They resolve an absent `cwd`
/// from the tab instead (`loopback::hook_cwd`).
pub fn resolve_cwd(cwd_raw: &str) -> String {
    if !cwd_raw.is_empty() {
        return cwd_raw.to_string();
    }
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The value following `--tab` in `args`, trimmed and non-empty.
///
/// A cImp tab id is never discoverable from a hook payload, so the two beacon
/// shims get theirs baked into argv at spawn. (The five converted hooks get
/// theirs from [`HEADER_TAB`] instead — same fact, one layer up.)
///
/// Pure, so the contract ("no id ⇒ no tab claimed") is testable without a socket
/// or a Claude process.
pub fn tab_arg(args: &[String]) -> Option<String> {
    let i = args.iter().position(|a| a == "--tab")?;
    let raw = args.get(i + 1)?.trim();
    (!raw.is_empty()).then(|| raw.to_string())
}

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
/// * `timeout` — [`TIMEOUT_SECS`], always explicit. The harness defaults are
///   600 s (most events), 30 s (`UserPromptSubmit`) and 10 s
///   (`MessageDisplay`); inheriting any of them would turn a wedged handler into
///   a wedged turn.
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
        "timeout": TIMEOUT_SECS,
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

    /// `tab_arg` is the two surviving shims' identity parser, unchanged by the
    /// move: it reads the baked id and refuses an empty one.
    #[test]
    fn tab_arg_reads_the_baked_id_and_refuses_an_empty_one() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            tab_arg(&a(&["--taint-beacon", "--tab", "claude-2"])).as_deref(),
            Some("claude-2")
        );
        assert_eq!(
            tab_arg(&a(&["--tab", " claude ", "--taint-beacon"])).as_deref(),
            Some("claude")
        );
        assert!(tab_arg(&a(&["--taint-beacon"])).is_none());
        assert!(tab_arg(&a(&["--taint-beacon", "--tab"])).is_none());
        assert!(tab_arg(&a(&["--tab", "   "])).is_none());
        assert!(tab_arg(&[]).is_none());
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
        // The four converted reporters keep the shim names; the two that never
        // reported still do not.
        assert_eq!(drift_token(ROUTE_USER_PROMPT_SUBMIT), Some("context_hook"));
        assert_eq!(drift_token(ROUTE_PRE_COMPACT), Some("compact_hook"));
        assert_eq!(drift_token(ROUTE_PRE_TOOL_USE), Some("read_hook"));
        assert_eq!(drift_token(ROUTE_NOTIFICATION), Some("notify_hook"));
        assert_eq!(
            drift_token(ROUTE_POST_TOOL_USE),
            None,
            "Phase A finding 2 is recorded, not quietly fixed"
        );
        assert_eq!(drift_token(ROUTE_SESSION_START), None);
        // …and a route with no checks reports nothing even when the payload is
        // empty, which is what makes the `None` above honest rather than lossy.
        assert!(contract_checks(ROUTE_POST_TOOL_USE, &HookInput::default()).is_empty());
        assert!(contract_checks(ROUTE_SESSION_START, &HookInput::default()).is_empty());
    }

    /// The whole point of the timeout column: it is 1 s, derived from the shims'
    /// 600 ms budget, and not the harness's 600 s / 30 s defaults.
    #[test]
    fn the_pinned_timeout_is_the_shims_budget_rounded_up() {
        assert_eq!(TIMEOUT_SECS, 1, "600 ms rounded up to whole seconds");
    }
}
