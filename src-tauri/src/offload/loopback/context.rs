//! The context/workbench hook routes — `/context/retrieve`,
//! `/workbench/tool_checkpoint`, `/context/compaction`,
//! `/context/should_read` and `/context/post_edit`.
//!
//! One family because they share the checkpoint identity funnel
//! ([`checkpoint_identity`]) and the #48 M-7 taint gate
//! ([`super::hook_admit`]); each capability's CORE is here too, because
//! the harness-native transport in `harness::claude::hook` calls the same
//! one (`tests::both_transports_of_a_capability_call_one_core`).
//! V42 R4 (#115) split them out of `loopback.rs`.

use super::*;

/// A `POST /context/retrieve` request body (from the Claude UserPromptSubmit
/// hook or the OpenCode injection plugin).
#[derive(Deserialize)]
pub(crate) struct ContextRetrieveBody {
    /// The calling session's working directory; the project root is resolved
    /// from it (defaults to `.`).
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    /// The user's prompt to rank context against.
    pub(crate) prompt: String,
    /// The agent session id (scopes the working-set boost); optional.
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    /// V13 Phase C: which agent shim is calling — `"claude"` (set by
    /// the Claude `UserPromptSubmit` route) or `"opencode"` (the generated plugin);
    /// absent/`None` for an unrecognized caller. Recorded on the checkpoint
    /// it triggers (see [`WorkbenchService::on_prompt`](crate::workbench::WorkbenchService::on_prompt)),
    /// not otherwise used by context retrieval itself.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// V33: the cImp TAB this prompt belongs to — `--tab <id>` baked into the
    /// `--context-hook` command at spawn (`tabs::config`), or `CIMP_TAB_ID`
    /// from the generated OpenCode plugin. Recorded on the checkpoint this
    /// prompt triggers so the Timeline can tell two same-agent tabs on one
    /// project root apart; nothing about context retrieval reads it.
    ///
    /// `#[serde(default)]` because a hook shim from an older build sends no
    /// such field, and a prompt must never fail for lack of identity — the
    /// checkpoint is simply written without a tab, exactly as before.
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

/// V33: the conversation identity recorded on the prompt-tap checkpoint this
/// route fires — the join key between a Timeline row and a
/// `Screen::Contamination` activity row.
///
/// **The tab id goes through [`tab_identity`]**, the same #45 narrowing every
/// other identity-taking route uses, so only a *configured AI tab id* is ever
/// recorded. A `tab` naming no configured tab is a forged or stale claim, and
/// writing it into a checkpoint would put a fabricated attribution on a record
/// whose whole purpose is to be trusted after an incident. It degrades to no
/// tab — which reads as "cannot attribute this checkpoint", not as "some other
/// tab". `Anonymous` (a hook shim from a build before `--tab` was baked in, or
/// an OpenCode plugin file not yet regenerated) lands in the same place, which
/// is exactly the pre-V33 row.
///
/// `session_id` and `agent` are recorded as sent. They are equally
/// caller-asserted, but neither can widen anything: they are compared for
/// equality against a contamination row and nothing else, and the framing
/// hazard they carry is handled where it lives — at the commit-trailer write
/// boundary (`workbench::shadow`'s `trailer_identity`).
///
/// Split out of the handler so the narrowing is exercised by a test rather than
/// re-implemented in one: a test that owned its own copy of this mapping would
/// stay green if the handler stopped calling it.
pub(super) fn checkpoint_origin(
    settings: &crate::settings::Settings,
    body: &ContextRetrieveBody,
) -> crate::workbench::shadow::Origin {
    checkpoint_identity(
        settings,
        body.agent.as_deref(),
        body.session_id.as_deref(),
        body.tab.as_deref(),
    )
}

/// The narrowing itself, shared by the prompt-tap trigger above and V33 Phase
/// F's pre-tool trigger ([`handle_tool_checkpoint`]) — ONE spelling, so the two
/// checkpoint writers cannot come to disagree about which `tab` claims are
/// believed.
pub(super) fn checkpoint_identity(
    settings: &crate::settings::Settings,
    agent: Option<&str>,
    session_id: Option<&str>,
    tab: Option<&str>,
) -> crate::workbench::shadow::Origin {
    // V33 C5: the id is checked against the tabs of the consumer the body
    // asserts, normalised through the same `hook_agent` funnel the gated hook
    // routes use — a `tab` that names another harness's tab is a forged or
    // stale claim exactly as an invented one is, and lands in the same place.
    let tab = match tab_identity(settings, hook_agent(agent), tab) {
        TabIdentity::Configured(tab) => Some(tab.to_string()),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => None,
    };
    crate::workbench::shadow::Origin::new(
        agent.map(str::to_string),
        session_id.map(str::to_string),
        tab,
    )
}

/// `POST /context/retrieve`: rank files for the prompt and return the injectable
/// digest as `{ ok, text }`. Gated on `context_injection` — returns empty text
/// (never blocks a turn) when injection is off or nothing clears the threshold.
pub(super) async fn handle_context_retrieve(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ContextRetrieveBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let answer = context_retrieve_core(app, &body).await;
    write_json(stream, 200, &answer).await
}

/// **The context-retrieval budget**: how long [`context_retrieve_core`] waits
/// for a fresh digest before answering without it.
///
/// Two ceilings sit above this number, and it must clear BOTH with margin,
/// because past either one the whole reply — greeting, drained auto-check
/// block, and any parked backlog just TAKEN from the store — is discarded
/// *after* those destructive reads already happened:
///
/// - the Claude harness discards the hook's reply outright at 1 s
///   ([`crate::harness::claude::hook::TIMEOUT_SECS`]);
/// - the OpenCode plugin aborts its `/context/retrieve` fetch at **600 ms**
///   (`AbortSignal.timeout(600)` in `templates/plugin.js` — the five deleted
///   shims' own `context_hook::TIMEOUT` number, "a slow/cold index never
///   delays the prompt").
///
/// 500 ms leaves the tighter (OpenCode) ceiling ~100 ms for composing and
/// writing the reply plus the plugin's own fetch overhead. A budget AT 600
/// would lose that race on exactly the timeout path it exists to serve: the
/// reply would leave at ~600 ms + ε, arrive after the client abort, and a
/// backlog already drained out of the park store would be gone for good —
/// on a chronically slow project the OpenCode transport would deliver
/// nothing, ever, while consuming everything.
///
/// The race exists because the measured cost is not the index: on a project
/// with `semantic_search` on and a remote embedding endpoint,
/// `retrieve_context` spends **0.67–2.5 s** in a blocking embed round trip
/// inside this handler. Before the race the handler lost that reply on
/// essentially every prompt while still having consumed the session's
/// once-per-session greeting, marked the dedup ledger injected and drained the
/// parked auto-check block — spending the state and delivering nothing.
///
/// Over budget the result is not discarded, it is parked for the next prompt
/// (`GraphService::park_injection`), the same bargain
/// [`GraphService::post_edit`](crate::graph::GraphService::post_edit) strikes
/// against `POST_EDIT_BUDGET_MS`. A test pins this below BOTH ceilings — the
/// constants live in different files (one of them in a JS template) and
/// nothing else keeps them ordered.
pub(super) const RETRIEVE_BUDGET_MS: u64 = 500;

/// Join the pieces of one injection reply with a blank line, skipping any that
/// are empty (or whitespace-only).
///
/// Extracted from [`context_retrieve_core`] so the ORDER — greeting, parked
/// blocks, fresh digest, drained auto-check — is asserted by a test rather than
/// re-read out of a chain of `if`s. Parts are joined verbatim: a block's own
/// content is never rewritten here.
pub(super) fn merge_injection_blocks(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.trim().is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The reply's `files`: parked first (they were retrieved first), then fresh,
/// de-duplicated preserving that order.
pub(super) fn merge_files_used(parked: Vec<String>, fresh: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(parked.len() + fresh.len());
    for f in parked.into_iter().chain(fresh) {
        if !out.contains(&f) {
            out.push(f);
        }
    }
    out
}

/// The prompt tap's whole effect, shared by `/context/retrieve` (the CHP body a
/// pre-upgrade shim or the OpenCode plugin posts) and
/// [`crate::harness::claude::hook::ROUTE_USER_PROMPT_SUBMIT`] (the raw `UserPromptSubmit` payload
/// the harness posts since V35 Phase J).
///
/// Extracted rather than duplicated: this is the only place the checkpoint
/// trigger fires, the injection gate is read and the digest is composed, so the
/// two transports cannot come to disagree about what a prompt does. Returns the
/// `/context/retrieve` answer verbatim — the Claude-native handler takes `text`
/// out of it and wraps it in the hook-output envelope.
///
/// **The retrieval itself is raced against [`RETRIEVE_BUDGET_MS`]** (2026-08-17
/// fix). `GraphService::retrieve_context` is sync and blocking — SQLite plus,
/// with semantic search on, a remote embed round trip measured at 0.67–2.5 s —
/// so it runs on `spawn_blocking` and this function answers at the budget
/// whether or not it is done. A digest that misses the budget is PARKED for the
/// next prompt (`GraphService::park_injection`) and the reply carries whatever
/// was parked by an earlier prompt, so nothing computed is thrown away.
///
/// What must NOT be parked rides the immediate reply unconditionally: the
/// once-per-session greeting and the drained auto-check block are destructive
/// reads (consumed exactly once), and both are cheap — no embed, no network —
/// so a slow retrieval can never cost the session its project map.
pub(crate) async fn context_retrieve_core(app: &AppHandle, body: &ContextRetrieveBody) -> serde_json::Value {
    // #104: both consumers below create per-project state — the workbench's
    // `<db_subdir>/shadow.git` and the graph store — so the payload's `cwd` is
    // resolved to a real root first. `None` refuses BOTH: no checkpoint and no
    // retrieval for a directory that is no project (`empty` is this route's
    // established "nothing to say" answer).
    let Some(cwd) = external_project_root(app, &live_settings(app), body.tab.as_deref(), body.cwd.as_deref())
    else {
        return serde_json::json!({ "ok": true, "text": "", "files": [], "tokens_est": 0 });
    };

    // V13 Phase C: fire the prompt-tap checkpoint trigger for EVERY prompt
    // that reaches this route, BEFORE the `context_injection` gate below —
    // checkpointing must fire even when injection is off or yields nothing
    // (Decision 4: decoupled from the injection toggle, reusing its
    // transport). Fire-and-forget: `on_prompt`'s own min-gap check is cheap
    // and the real snapshot work runs on a background task inside it, so
    // this never delays the turn waiting on a `git` round trip.
    // FIX 8 (V13 code review): only spawn the task at all when checkpoints
    // are actually on — `on_prompt`/`maybe_snapshot` already no-op when
    // they're off, but that check used to happen AFTER a task was already
    // spawned for every single prompt, which is needless per-prompt work
    // (a task spawn plus a settings read) for a feature the user has
    // disabled.
    if let Some(workbench) = app.try_state::<Arc<crate::workbench::WorkbenchService>>() {
        let workbench = workbench.inner().clone();
        if workbench.checkpoints_enabled() {
            let root = cwd.clone();
            // V33: the identity the Timeline is joined on. The settings read
            // sits INSIDE the `checkpoints_enabled` gate for FIX 8's reason —
            // a user with checkpoints off pays nothing for this.
            let origin = checkpoint_origin(&live_settings(app), body);
            let prompt_head: String = body.prompt.chars().take(80).collect();
            tauri::async_runtime::spawn(async move {
                workbench.on_prompt(&root, origin, &prompt_head).await;
            });
        }
    }

    let empty = serde_json::json!({ "ok": true, "text": "", "files": [], "tokens_est": 0 });
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return empty;
    };
    let graph = graph.inner().clone();
    // The injection toggle is enforced here (the service's retrieve does not) so
    // the preview surface can reuse the same core while injection is off.
    if !graph.context_injection_enabled() {
        return empty;
    }
    let sid = body.session_id.clone().filter(|s| !s.is_empty());

    // A digest an EARLIER prompt's retrieval finished too late to deliver is
    // part of THIS reply. Taken before the race so that a fresh result which
    // also misses the budget parks BEHIND it rather than racing it — the store
    // is oldest-first and this keeps that ordering true.
    let parked = graph.take_parked_injection(sid.as_deref());

    // The slow part, off the async runtime's worker: `retrieve_context` is
    // blocking (SQLite + a blocking HTTP embed), and blocking a runtime thread
    // for seconds would stall every other loopback route, not just this one.
    let mut handle = {
        let graph = graph.clone();
        let root = cwd.clone();
        let prompt = body.prompt.clone();
        let sid = sid.clone();
        tokio::task::spawn_blocking(move || graph.retrieve_context(&root, &prompt, sid.as_deref()))
    };
    // The deadline is taken HERE, before the cheap work below, so the cheap
    // work is overlapped with the retrieval instead of being added on top of
    // the budget.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(RETRIEVE_BUDGET_MS);

    // V11 Phase B: the once-per-session project map. Done here (the real
    // injection path), not in `retrieve_context`, so the preview surface —
    // which also calls `retrieve_context` — never consumes the once-per-session
    // flag. Synchronous and unraced on purpose: it is a once-per-session
    // destructive read with no embed in it, so it must ride this reply.
    let greeting = graph
        .session_greeting(&cwd, sid.as_deref())
        .unwrap_or_default();
    // V12 Phase F: drain any auto-check block a slow post-edit run parked for
    // this session (see `GraphService::post_edit`'s budget/park path) — a
    // turn is never blocked waiting for a check, but its result still reaches
    // the model on the very next opportunity. Destructive too: never parked,
    // never lost to a slow retrieval.
    let pending_check = graph.drain_auto_check(sid.as_deref()).unwrap_or_default();

    let fresh = tokio::select! {
        res = &mut handle => res.ok(),
        _ = tokio::time::sleep_until(deadline) => {
            match sid.clone() {
                Some(s) => {
                    let graph = graph.clone();
                    tauri::async_runtime::spawn(async move {
                        // Bound the parked run for `post_edit`'s reason: a
                        // wedged embedding endpoint must not leave a task (and
                        // a blocking-pool thread) alive for the process's
                        // lifetime. On timeout we stop waiting — `abort` on a
                        // `spawn_blocking` handle cannot interrupt the closure
                        // itself, so this bounds the reaper, not the blocking
                        // call, which is the part that would otherwise leak.
                        const PARKED_MAX_MS: u64 = 60_000;
                        match tokio::time::timeout(
                            Duration::from_millis(PARKED_MAX_MS),
                            &mut handle,
                        )
                        .await
                        {
                            Ok(Ok(r)) => graph.park_injection(Some(&s), &r.context_md, r.files_used),
                            Ok(Err(_join_err)) => {}
                            Err(_elapsed) => handle.abort(),
                        }
                    });
                }
                // No session id to park under — both real transports always
                // send one, so this is the preview-shaped edge case: drop it.
                None => debug!(
                    target: "offload",
                    "context retrieve missed its budget with no session id to park under"
                ),
            }
            None
        }
    };
    let (fresh_text, fresh_files) = match fresh {
        Some(r) => (r.context_md, r.files_used),
        None => (String::new(), Vec::new()),
    };

    let (parked_text, parked_files) = parked.unwrap_or_default();
    let text = merge_injection_blocks(&[&greeting, &parked_text, &fresh_text, &pending_check]);
    let files = merge_files_used(parked_files, fresh_files);
    // Same char→token estimate as the retrieval core (shared divisor so the two
    // can't drift). Estimated from the FULL injected text (greeting + parked +
    // digest + drained auto-check), not just the digest.
    let tokens_est = crate::graph::est_tokens(text.chars().count());
    serde_json::json!({ "ok": true, "text": text, "files": files, "tokens_est": tokens_est })
}

/// A `POST /workbench/tool_checkpoint` request body — V33 Phase F's two
/// out-of-process fire seams: the Claude `PreToolUse` shim
/// (`crate::checkpoint_beacon`) and the OpenCode `tool.execute.before` plugin
/// hook. The worker seam does NOT come through here; it calls
/// `WorkbenchService::on_tool` directly (`offload::tools::dispatch`).
#[derive(Deserialize)]
pub(super) struct ToolCheckpointBody {
    /// The calling session's working directory — the project root the shadow
    /// repo lives under. Defaults to `.` like every other hook route.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// Which harness is calling: `"claude"` / `"opencode"`. Normalised through
    /// [`hook_agent`], and it selects WHICH tool vocabulary the name below is
    /// checked against — the two namespaces are disjoint and must not be
    /// crossed.
    #[serde(default)]
    pub(super) agent: Option<String>,
    /// The cImp TAB, baked into the hook command / the plugin file at spawn.
    /// Narrowed through [`checkpoint_identity`]; an unrecognised id degrades to
    /// "no tab", never to another tab.
    #[serde(default)]
    pub(super) tab: Option<String>,
    /// The harness's own session id, recorded as sent.
    #[serde(default)]
    pub(super) session_id: Option<String>,
    /// The tool about to run, in the CALLER's vocabulary (`Bash`, `edit`).
    /// Required — a checkpoint with no tool name is not a Phase F checkpoint.
    #[serde(default)]
    pub(super) tool: Option<String>,
}

/// **The pre-tool checkpoint budget** (2026-08-13 amendment, locked): how long
/// [`handle_tool_checkpoint`] lets a snapshot run before it abandons it
/// unwritten.
///
/// Deliberately **below** the ~2 s both out-of-process callers wait — the Claude
/// shim's [`checkpoint_beacon::REPLY_TIMEOUT`](crate::checkpoint_beacon) and the
/// OpenCode plugin's `AbortSignal.timeout(2000)`. The ordering is the whole
/// point: the harness starts the tool the instant its hook stops waiting, so if
/// the caller's timer fired first the app would still be staging *into* the tool
/// call while believing it had a valid pre-tool checkpoint. Keeping the app's
/// budget under the caller's makes the app's own answer the one that decides,
/// and leaves ~200 ms for the reply to be written and read.
///
/// Not a per-call latency budget for the *user*: the throttle means most calls
/// never reach a snapshot at all, and this bound is only reached by a
/// `git add -A` over a work tree big enough to take seconds.
/// **V40 Phase C, locked decision 22 — this is no longer a number typed here.**
/// It was `Duration::from_millis(1800)`, hand-computed from two artifacts'
/// timers and asserted against both by a cross-file test, which meant a THIRD
/// harness with a shorter timer would have been silently over-run. Core now
/// derives it as `min(every plugin's declared `hook_reply_timeout`) − margin`
/// ([`crate::harness::ingress::hook_reply_budget`]); the shipped pair still
/// implies 1800 ms and a test pins that, so the behaviour is unchanged and the
/// derivation is what moved.
pub(crate) fn tool_checkpoint_budget() -> Duration {
    crate::harness::ingress::hook_reply_budget()
}

/// V33 Phase F: does `tool` change files on disk, **in `harness`'s own tool
/// vocabulary**?
///
/// Split out of [`handle_tool_checkpoint`] so the namespace selection is
/// exercised by a test rather than re-implemented in one — a test that owned its
/// own copy of this lookup would stay green after the handler stopped calling it.
///
/// **V40 Phase A, locked decision 16.** This was a `match` with `"opencode"` in
/// one arm and Claude's table in the `_` arm, which meant a THIRD harness's
/// `edit` was not rejected but answered out of Claude's vocabulary — `false`,
/// silently, for its whole mutation surface. It is now one registry lookup that
/// fails CLOSED: a token naming no registered harness, and a name the registered
/// plugin does not declare, both answer `true`. A checkpoint nobody needed is a
/// commit into cImp's own shadow repo; a missed one is a destructive call with
/// no way back.
pub(super) fn tool_checkpoint_is_mutating(harness: &str, tool: &str) -> bool {
    crate::harness::native::mutates_fs(crate::harness::HarnessId::from_id(harness), tool)
}

/// The identity check `POST /workbench/tool_checkpoint` makes before anything is
/// staged — the harness token this call is attributed to, or the refusal.
///
/// **V40 review finding M-6 (parity lens).** The route's own doc claims a
/// forged POST "cannot get a destructive call waved through by naming a harness
/// cImp does not know", and the opposite was true: an unregistered token
/// resolves to `UNKNOWN_SOURCE`, `mutates_fs` fails CLOSED for it — which is
/// right for a name inside a known harness's vocabulary and wrong for a source
/// that has no vocabulary — so EVERY tool name from an unidentified caller was
/// "mutating" and minted a snapshot attributed to `unknown:<whatever>`. Bounded
/// by the throttle and the tree-sha dedup, but a checkpoint is the one record
/// that exists to be trusted after an incident, and an unattributable row in it
/// is worse than no row.
///
/// An ABSENT `agent` is a different question with a different answer: it is a
/// shim from a build before the field existed, and it resolves to
/// [`crate::harness::DEFAULT_HARNESS`] exactly as every other hook body does.
///
/// Split out so the decision is testable without a `TcpStream`.
pub(super) fn checkpoint_source_admits(agent: Option<&str>) -> Result<&'static str, String> {
    let harness = hook_agent(agent);
    if crate::harness::HarnessId::from_id(harness).is_some() {
        return Ok(harness);
    }
    Err(format!(
        "`agent` names no registered harness ({}), so this call cannot be attributed to one. A          checkpoint is the record a restore is judged against; an unattributable row in it is          worse than no row.",
        crate::harness::registry::harness_ids().join(", ")
    ))
}

/// `POST /workbench/tool_checkpoint` (V33 Phase F): take a Workbench checkpoint
/// **immediately before** a filesystem-mutating tool call, attributed to that
/// exact call.
///
/// # Why identity and the tool name are both re-checked here
///
/// **Identity first** (V40 review M-6): a body whose `agent` names no
/// registered harness is refused with a 400, because `mutates_fs` fails CLOSED
/// for an unknown vocabulary — which is right for a name inside a known
/// harness's table and wrong for a source that has no table, where it made
/// EVERY tool name mutating. See [`checkpoint_source_admits`].
///
/// Both callers pre-filter — Claude's hook is installed with an
/// `Edit|Write|MultiEdit|Bash` matcher, the plugin consults a baked
/// `CIMP_MUTATING_TOOLS` set — but neither is the authority. This route resolves
/// the name against the SENDING harness's own reviewed table, through
/// [`crate::harness::native::mutates_fs`]. A drifted matcher, a shim from a
/// newer build, or a forged POST from any local process therefore cannot mint a
/// checkpoint for a tool that harness declares non-mutating — and cannot get a
/// destructive call waved through by naming a harness cImp does not know.
///
/// **Crossing the two vocabularies would be silent, not loud**: `edit` and
/// `Edit` are unknown in each other's tables, so one crossed lookup would
/// disable a whole harness's seam while every test that only exercised the other
/// stayed green. The registry lookup is what makes crossing them impossible to
/// express.
///
/// # Containment posture
///
/// Behind the same bearer token as every other route, and **it takes no taint
/// gate** — deliberately. A checkpoint is a `git add -A` + `commit-tree` into
/// cImp's OWN shadow repo; it returns no project data to the caller (the reply
/// is `{ok, checkpointed}`, two booleans) and grants no capability, so there is
/// nothing for a latch to refuse. The abuse case for a forged POST is a spurious
/// snapshot, which the per-`(root, tab)` min-gap throttle and the tree-sha dedup
/// both bound — and which, unlike a refusal, costs the user nothing they wanted.
/// Gating it would instead mean that a tab which had touched external content
/// stopped getting checkpoints exactly when they matter most.
///
/// # The pre-tool budget (2026-08-13 amendment, locked)
///
/// **Both callers of this route stop waiting after ~2 s** — the Claude shim on
/// its reply-read timeout, the OpenCode plugin on its `AbortSignal.timeout(2000)`
/// — and the harness runs the tool the moment they do. A snapshot still staging
/// past that point is racing the very edit it exists to precede, so this route
/// hands [`WorkbenchService::on_tool`](crate::workbench::WorkbenchService::on_tool)
/// a deadline of [`tool_checkpoint_budget`] and the snapshot writes **nothing**
/// once it is spent. The alternative — let the caller give up and let the app
/// commit the row anyway — is the failure this amendment exists to close: a
/// checkpoint that sometimes contains the change it claims to predate silently
/// misleads a restore, which is strictly worse than having none.
///
/// The budget is deliberately *under* the callers' 2 s so the app's own answer
/// is what decides, rather than whichever timer happens to fire first.
///
/// The worker seam does not come through here and gets no deadline: it is
/// in-process and waits as long as the snapshot takes.
///
/// # The reply, and what it is not
///
/// `{ "ok": true, "checkpointed": <bool> }`. `checkpointed` deliberately does
/// not return a checkpoint id, because on a dedup hit the id would name another
/// trigger's (possibly another tab's) checkpoint and no caller may claim it (see
/// [`shadow::SnapshotOutcome`](crate::workbench::shadow::SnapshotOutcome)).
///
/// It means *"the trigger settled — nothing about this call is unaccounted
/// for"*: true for a checkpoint created, for a dedup hit, and for a throttled
/// call (whose tab already has a checkpoint newer than `checkpoint_min_gap_s`).
/// **False now also covers the one new case: the snapshot was abandoned against
/// the budget above**, i.e. exactly when no checkpoint can be said to precede
/// this call. Neither caller gates anything on it — the Claude shim reads it
/// only to make its wait mean something, the OpenCode plugin awaits it for the
/// ordering — so the user-facing report of a miss is the Activity row
/// `workbench` / `checkpoint_missed`, not this boolean.
pub(super) async fn handle_tool_checkpoint(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ToolCheckpointBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let Some(tool) = body
        .tool
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return write_json(
            stream,
            400,
            &serde_json::json!({ "ok": false, "error": "missing `tool`" }),
        )
        .await;
    };
    // V40 review M-6: identity BEFORE anything is staged. See
    // `checkpoint_source_admits`.
    if let Err(msg) = checkpoint_source_admits(body.agent.as_deref()) {
        return write_json(
            stream,
            400,
            &serde_json::json!({ "ok": false, "error": msg }),
        )
        .await;
    }
    let checkpointed = tool_checkpoint_core(
        app,
        &live_settings(app),
        body.agent.as_deref(),
        tool,
        body.cwd.as_deref(),
        body.session_id.as_deref(),
        body.tab.as_deref(),
    )
    .await;
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "checkpointed": checkpointed }),
    )
    .await
}

/// **The pre-tool checkpoint itself** — the core both out-of-process fire seams
/// reach: this route's harness-neutral body (the OpenCode plugin) and
/// [`crate::harness::claude::hook::ROUTE_PRE_TOOL_USE_CHECKPOINT`]'s Claude hook payload.
///
/// Split out on 2026-08-17, when the Claude side stopped being a shim POSTing to
/// the route and became a handler beside it. One core, so the two transports
/// cannot come to disagree about the tool-name re-check, the enabled switch, the
/// identity narrowing or the deadline — the property
/// `both_transports_of_a_capability_call_one_core` asserts and the reason the
/// Claude migration is a relocation rather than a second implementation.
///
/// Returns `checkpointed`: the trigger settled and nothing about this call is
/// unaccounted for — true for a checkpoint created, a dedup hit and a throttled
/// call; false for a non-mutating name, checkpoints off, no service, or a
/// snapshot abandoned against [`tool_checkpoint_budget`]. `settings` is passed in
/// rather than read here so a handler resolves identity and policy under ONE
/// snapshot.
pub(crate) async fn tool_checkpoint_core(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    agent: Option<&str>,
    tool: &str,
    cwd: Option<&str>,
    session_id: Option<&str>,
    tab: Option<&str>,
) -> bool {
    // Normalized for the two decisions that must not cross the harnesses' tool
    // vocabularies, and passed on RAW to `checkpoint_identity`, which records the
    // caller's own spelling in the commit trailer exactly as it always has.
    let harness = hook_agent(agent);
    if !tool_checkpoint_is_mutating(harness, tool) {
        // Not an error: a caller whose matcher is wider than the table is
        // over-reporting, and a fail-open sensor must never learn to treat that
        // as a failure. No log line either — this is reachable once per
        // non-mutating matched call and would be unbounded chatter.
        return false;
    }
    let Some(workbench) = app.try_state::<Arc<crate::workbench::WorkbenchService>>() else {
        return false;
    };
    let workbench = workbench.inner().clone();
    if !workbench.checkpoints_enabled() {
        return false;
    }
    // #104: the checkpointer creates `<root>/<db_subdir>/shadow.git`, so a cwd
    // that resolves to no project takes no checkpoint rather than minting a
    // shadow repo inside one.
    let Some(root) = external_project_root(app, settings, tab, cwd) else {
        return false;
    };
    let origin = checkpoint_identity(settings, agent, session_id, tab);
    // `harness:tool_name` — the locked value format. `bounded_id` caps the
    // caller-supplied half before it reaches a commit trailer; `trailer_identity`
    // rejects the framing hazards at the write boundary, and an over-long value
    // there would be dropped WHOLE, losing the harness prefix too.
    let source = format!("{harness}:{}", bounded_id(tool));
    // AWAITED, unlike the prompt-tap trigger's fire-and-forget spawn: the point
    // of this trigger is that the snapshot precedes the mutation, and both fire
    // seams hold the tool call until this returns (the OpenCode plugin awaits
    // its POST inside `tool.execute.before`; a Claude `PreToolUse` http hook
    // blocks the call until the handler answers). The wait is bounded twice over
    // — by the throttle, which admits at most one snapshot per
    // `checkpoint_min_gap_s` per `(root, tab)` and is what makes the common case
    // free, and by the budget below, which stops a slow one from outliving the
    // caller's patience and minting a row for a tool call that has already run.
    workbench
        .on_tool(
            &root,
            origin,
            &source,
            Some(Instant::now() + tool_checkpoint_budget()),
        )
        .await
}

/// A `POST /context/compaction` request body (the Claude `PreCompact` shim).
#[derive(Deserialize)]
pub(crate) struct ContextCompactionBody {
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    /// `"manual"` / `"auto"`; recorded, not currently branched on.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) trigger: Option<String>,
    /// #48 (M-7): which shim is calling. See [`hook_agent`].
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves, baked into argv at spawn.
    /// `#[serde(default)]` because a shim from an older build sends none — see
    /// the residual note above.
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

/// `POST /context/compaction` (V11 Phase D): always runs the session's
/// compaction side effects (clear injection dedup, mark post-compaction) and
/// returns a compact working-set/notes block as `{ ok, text }` to carry through
/// the summary. Never blocks — an empty block is returned as empty text.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_COMPACTION`], which
/// classifies TRUSTED — so this gate admits every call today, and the route is
/// inside the mechanism rather than beside it (demoting that one row is all it
/// takes to close the route, and its comment states what else a demotion must
/// do first). The block's content is why: paths, symbol NAMES and memory-note
/// text, no source text, with quarantined notes already excluded.
pub(super) async fn handle_context_compaction(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ContextCompactionBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let empty = serde_json::json!({ "ok": true, "text": "" });
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_COMPACTION,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &empty).await;
    }
    let block = compaction_block(app, &body);
    write_json(stream, 200, &serde_json::json!({ "ok": true, "text": block })).await
}

/// The compaction carry-over block, **after** the gate — shared by
/// `/context/compaction` and [`crate::harness::claude::hook::ROUTE_PRE_COMPACT`].
///
/// The gate itself deliberately stays in each handler rather than moving in
/// here: the route-enumeration test (`every_loopback_route_declares_what_it_does_
/// about_the_latch`) checks each handler's own body for its `hook_admit(latches(),
/// …)` call, and a gate that a route merely inherits from a helper is a gate a
/// reviewer cannot see at the route.
pub(crate) fn compaction_block(app: &AppHandle, body: &ContextCompactionBody) -> String {
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return String::new();
    };
    let graph = graph.inner().clone();
    // #104: `compaction_context` opens the project's store — resolve, never
    // trust the payload's cwd. No root ⇒ no carry-over block, the route's own
    // fail-safe.
    let Some(root) = external_project_root(app, &live_settings(app), body.tab.as_deref(), body.cwd.as_deref())
    else {
        return String::new();
    };
    graph
        .compaction_context(&root, body.session_id.as_deref())
        .unwrap_or_default()
}

/// A `POST /context/should_read` request body (the Claude `PreToolUse` Read
/// advisor shim).
#[derive(Deserialize)]
pub(crate) struct ShouldReadBody {
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    pub(crate) file_path: String,
    /// 1-based read offset, when the agent asked for a windowed read.
    #[serde(default)]
    pub(crate) offset: Option<u32>,
    /// V17 Phase B: the `Read` line limit, when the agent asked for a slice.
    /// Forwarded so the verdict can tell a full read from a head-peek (a
    /// deliberate slice always passes — Phase C's first-read branch).
    #[serde(default)]
    pub(crate) limit: Option<u32>,
    /// #48 (M-7): which shim is calling. See [`hook_agent`].
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves, baked into argv at spawn.
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

/// `POST /context/should_read` (V11 Phase E): the read-advisor verdict for a
/// `Read`. Returns `{ ok, verdict: "pass" }` to let the read through, or
/// `{ ok, verdict: "remind", text }` to deny-with-content. Fails open to `pass`
/// on any missing state — the advisor must never block a legitimate read.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_SHOULD_READ`]
/// (LOCAL-CAPABILITY — the verdict hands back the file's outline, its symbol
/// body, or a unified diff of it, which is repo source text). **This does not
/// weaken the sentence above.** The gate's only reachable effect is to turn a
/// `remind` into a `pass`, because `pass` is the fail-safe every arm of this
/// route falls back to: a latched conversation gets its read through untouched
/// and pays only the tokens the advisor would have saved. The advisor can still
/// never block a legitimate read — after this change it can block strictly
/// fewer of them.
pub(super) async fn handle_should_read(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let pass = serde_json::json!({ "ok": true, "verdict": "pass" });
    let body: ShouldReadBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_SHOULD_READ,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &pass).await;
    }
    match should_read_verdict(app, &body) {
        Some(text) => {
            write_json(
                stream,
                200,
                &serde_json::json!({ "ok": true, "verdict": "remind", "text": text }),
            )
            .await
        }
        None => write_json(stream, 200, &pass).await,
    }
}

/// The read advisor's verdict, **after** the gate — `Some(reminder)` for a
/// `remind`, `None` for a `pass`. Shared by `/context/should_read` and
/// [`crate::harness::claude::hook::ROUTE_PRE_TOOL_USE`]; see [`compaction_block`] for why the
/// gate stays at each route rather than moving in here.
pub(crate) fn should_read_verdict(app: &AppHandle, body: &ShouldReadBody) -> Option<String> {
    let graph = app.try_state::<Arc<crate::graph::GraphService>>()?;
    let graph = graph.inner().clone();
    // #104: the advisor OPENS (and therefore creates) the project's graph store,
    // so it is the route that minted the stray state dirs. The root is resolved,
    // never taken from the payload; no root ⇒ pass the read through, which is
    // this route's fail-safe everywhere else too.
    let root = external_project_root(app, &live_settings(app), body.tab.as_deref(), body.cwd.as_deref())?;
    graph.should_read(
        &root,
        body.session_id.as_deref(),
        &body.file_path,
        body.offset,
        body.limit,
    )
}

/// A `POST /context/post_edit` request body (the Claude `PostToolUse` shim, or
/// the OpenCode plugin's `tool.execute.after` hook).
#[derive(Deserialize)]
pub(crate) struct ContextPostEditBody {
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) file_path: String,
    /// Recorded for symmetry with the shim's payload; not currently branched
    /// on (the matcher/plugin already scope this to edit-class tools).
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) tool_name: Option<String>,
    /// #48 (M-7): which shim is calling — `"claude"` (the `--postedit-hook`
    /// shim) or `"opencode"` (the generated plugin). See [`hook_agent`].
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves — `--tab <id>` from argv on the
    /// Claude side, `CIMP_TAB_ID` on the OpenCode side.
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

/// **V33 C4** — every directory this instance will run the project's configured
/// CHECK COMMANDS in on a hook's behalf: the **served root** (this app's launch
/// directory) plus each **configured AI tab's** working directory, and nothing
/// else. Derived entirely from the app and the settings snapshot; the request
/// body contributes nothing to this list.
///
/// The tab dirs are here because they are not always under the launch root: V13
/// Phase D's "New tab in worktree…" sets `AiToolTabConfig::cwd` to a freshly
/// created git worktree, and a hook firing in that tab legitimately names it.
/// Resolution is [`crate::tabs::ai_tab_dir`], the same call
/// [`build_ai_tool_spec`](crate::tabs::config) makes when it actually spawns the
/// tab, so this list is the set of directories cImp itself launches agents in.
///
/// Every consumer's tabs, not the caller's: these are the operator's own
/// directories either way, and scoping the list by the caller's asserted
/// `agent` would let the assertion move a *capability* boundary — the thing
/// C5 exists to stop it doing to the identity one.
///
/// An empty vec is possible only when managed state is absent AND
/// `current_dir()` fails (a deleted cwd). It denies everything, which is the
/// correct answer: a root that cannot be resolved must read as absent, never as
/// "allow whatever was asked for".
pub(super) fn hook_exec_roots(app: &AppHandle, settings: &crate::settings::Settings) -> Vec<PathBuf> {
    let launch = app
        .try_state::<crate::ipc::AppState>()
        .map(|s| s.launch.cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    let Some(launch) = launch else {
        return Vec::new();
    };
    let mut roots = vec![launch.clone()];
    for tab in ai_tab_ids(settings) {
        if let Some(dir) = crate::tabs::ai_tab_dir(settings, tab, &launch) {
            if !roots.contains(&dir) {
                roots.push(dir);
            }
        }
    }
    roots
}

/// **V33 C4** — the working directory `POST /context/post_edit` may execute in,
/// or `None` to refuse.
///
/// This is [`audit_admit`]'s step 3 in a second place, deliberately built from
/// the same two helpers ([`canon`] + [`is_ancestor_or_equal`]) rather than from
/// new path logic, so the two routes' notions of "inside a root I serve" cannot
/// drift. What differs is only the answer to a miss: `/audit/run` returns a
/// readable tool error, and this route — a hook that must never perturb an edit
/// — returns its own fail-safe (empty text) with an operator-visible `warn!`.
///
/// Three cases:
///
/// 1. **No `cwd` on the wire** ⇒ the served root. The pre-V33 default was
///    `PathBuf::from(".")`, i.e. the app process's cwd; the served root is that
///    same directory by a route that cannot be moved by a `chdir` and that is
///    stated rather than implied.
/// 2. **A `cwd` at or under one of the roots** ⇒ admitted, and passed through
///    **as written**. The path string keys the single-flight `RootRunner`
///    bucket and the auto-check baseline downstream, so canonicalizing it here
///    would silently re-bucket every existing caller.
/// 3. **Anything else** ⇒ `None`. Including a path containing `..`, which is
///    refused inside [`is_ancestor_or_equal`] rather than here: a component walk
///    cannot resolve a `..`, and [`canon`] only resolves one for a path that
///    EXISTS, so an unresolved `..` reaching a zip-compare reads as a
///    descendant. That refusal is shared with [`audit_admit`] step 3
///    deliberately — see the helper's own note for the measurement behind it,
///    including why the Windows spelling in this comment's first draft
///    (`P:\served\..\..\evil`) was rejected for the wrong reason and
///    `\\?\P:\served\..\..\evil` was not rejected at all. Costs nothing here:
///    every real caller sends the absolute cwd its harness reported.
pub(super) fn admitted_hook_root(roots: &[PathBuf], requested: Option<&str>) -> Option<PathBuf> {
    let Some(req) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return roots.first().cloned();
    };
    let hint = canon(Path::new(req));
    roots
        .iter()
        .any(|r| is_ancestor_or_equal(&canon(r), &hint))
        .then(|| PathBuf::from(req))
}

/// `POST /context/post_edit` (V12 Phase F): debounce this session's edits, run
/// the project's configured checks single-flight per root, diff against the
/// session's own baseline, and return only NEW/worsened diagnostics (plus an
/// optional auto-impact note) as `{ ok, text }`. Fails open to empty text on
/// any missing state — the hook must never block or perturb an edit.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_POST_EDIT`]. This is
/// the route the finding is really about — it **executes the project's
/// configured check commands**, which is the definition of LOCAL-CAPABILITY
/// under decision 1, and it did so with no `latches()` call at all. A refusal
/// answers with the route's own fail-safe (empty text), so a contaminated
/// conversation loses its auto-check diagnostics and nothing else; the edit
/// itself is never perturbed.
///
/// **V33 C4 closes the directory half.** The `cwd` those commands run in used
/// to come straight out of the request body (defaulting to `"."`) with no
/// ancestor check and no allowlist, so anything holding the loopback token could
/// have the operator's own vetted check commands executed in a directory it
/// named — a cloned repo's `Makefile`, say, reached through a `cargo`/`npm`
/// script the operator configured for their own project. It is now resolved through
/// [`admitted_hook_root`] against [`hook_exec_roots`], which derives from the
/// served root and the configured tabs and **never from the request**. A refusal
/// takes the route's own fail-safe (empty text) and logs; it cannot perturb the
/// edit.
///
/// **The identity half is deliberately untouched** (locked V33 decision 2). A
/// body with no usable `tab` still resolves to no scope and is ADMITTED, exactly
/// as on `/graph_run` and `/mcp/call` — see the residual note above `hook_admit`.
/// The two halves are independent: C4's allowlist is app-derived, so omitting
/// `tab` does not walk around it, which is why the directory half could be
/// closed without settling the identity one.
///
/// **Why the sibling hook routes get no such check** (so the asymmetry is not
/// later read as an oversight): `/context/should_read` and
/// `/context/compaction` take the same caller-supplied `cwd` and share the same
/// identity fail-open, but neither EXECUTES anything with it — it selects which
/// project's index to read, and what a read can hand back is what their
/// [`toolclass::TABLE`] rows and their [`hook_admit`] gate already decide. There
/// is no command to run in a directory a caller names, so there is nothing for
/// a root allowlist to contain. If either ever grows a spawn, it inherits this
/// route's treatment.
pub(super) async fn handle_post_edit(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
    let empty = serde_json::json!({ "ok": true, "text": "" });
    let body: ContextPostEditBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_POST_EDIT,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &empty).await;
    }
    let text = post_edit_diagnostics(app, &settings, &body).await;
    write_json(stream, 200, &serde_json::json!({ "ok": true, "text": text })).await
}

/// The auto-check diff for one edit, **after** the gate — including V33 C4's
/// root admission, which is part of the work rather than part of the latch gate.
/// Shared by `/context/post_edit` and [`crate::harness::claude::hook::ROUTE_POST_TOOL_USE`]; see
/// [`compaction_block`] for why the latch gate stays at each route.
pub(crate) async fn post_edit_diagnostics(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    body: &ContextPostEditBody,
) -> String {
    // V33 C4: decide WHERE before deciding whether there is anything to run —
    // the roots are app-derived, so this cannot be moved by the body.
    let exec_roots = hook_exec_roots(app, settings);
    let Some(cwd) = admitted_hook_root(&hook_exec_roots(app, settings), body.cwd.as_deref()) else {
        // Bounded: the rejected string is caller-chosen and unbounded on the
        // wire, and this is the one place it reaches an operator-facing line.
        warn!(
            target: "offload",
            requested = %bounded_id(body.cwd.as_deref().unwrap_or_default()),
            "loopback: /context/post_edit named a working directory outside this instance's \
             served root and its configured tabs' directories — the project's configured check \
             commands were NOT run there (the edit itself is unaffected)"
        );
        return String::new();
    };
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return String::new();
    };
    // #104: admitted is not the same as *is a project root* — a sub-agent's cwd
    // passes C4's allowlist perfectly well and `post_edit` opens the project's
    // store from whatever it is handed. Resolve it, then **re-apply C4 to the
    // answer**: the walk goes UP, and a resolved root above every served root
    // would run the operator's check commands one directory further out than
    // C4 admits. It never widens; on a miss the route takes its own fail-safe.
    let root = external_project_root(app, settings, body.tab.as_deref(), Some(&cwd.to_string_lossy()));
    let Some(root) = root.filter(|r| {
        let r = canon(r);
        exec_roots.iter().any(|allowed| is_ancestor_or_equal(&canon(allowed), &r))
    }) else {
        warn!(
            target: "offload",
            requested = %bounded_id(&cwd.to_string_lossy()),
            "loopback: /context/post_edit could not resolve a project root at or under \
             this instance's served roots from the working directory it named — the \
             project's configured check commands were NOT run (the edit itself is \
             unaffected)"
        );
        return String::new();
    };
    let graph = graph.inner().clone();
    graph
        .post_edit(&root, body.session_id.as_deref(), &body.file_path)
        .await
        .unwrap_or_default()
}
