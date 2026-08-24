//! The `/session/*` routes — the CHP hello, the three V35 Phase L read-path
//! pushes (`assistant_text` / `tool_result` / `subagent`) and the V40 Phase D
//! turn-boundary edges (`output_started` / `output_stopped` /
//! `subagents_active`).
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`. They
//! share one identity funnel ([`session_push_identity`]) and one
//! arbitration predicate (`harness::chp::served`), which is why they are
//! one file.

use super::*;

/// A `POST /session/hello` body — V35 Phase I, design D3.
///
/// Every field is optional and every one is caller-supplied. The handler bounds
/// each before it can reach a row or the panel; nothing here is believed beyond
/// "this local process said so", which is the standard every `Origin::Http`
/// producer on this listener is held to.
#[derive(Deserialize)]
pub(super) struct SessionHelloBody {
    /// The protocol version the artifact speaks. Absent ⇒ pre-CHP, never an
    /// error — a hello is exactly the message an old artifact would not send at
    /// all, so tolerating its absence here is belt-and-braces rather than a
    /// live path.
    #[serde(default)]
    pub(super) chp: Option<u32>,
    /// `claude` / `opencode`, normalized through `source_for_consumer` like
    /// every other route's discriminator.
    #[serde(default)]
    pub(super) agent: Option<String>,
    /// The cImp tab this artifact was generated for. Required in practice: a
    /// hello with no tab has nothing to key, and one naming an unconfigured tab
    /// is refused (see the handler).
    #[serde(default)]
    pub(super) tab: Option<String>,
    /// The harness's own version, when it exposes one to its extensions.
    #[serde(default)]
    pub(super) harness_version: Option<String>,
    /// The CHP events this artifact will actually push, with its per-tab flags
    /// applied.
    #[serde(default)]
    pub(super) serves: Vec<String>,
    /// …and the rest, each with a reason.
    #[serde(default)]
    pub(super) cannot: Vec<SessionHelloUnable>,
}

/// One `cannot` entry: a capability this artifact will not serve, and why.
#[derive(Deserialize)]
pub(super) struct SessionHelloUnable {
    #[serde(default)]
    pub(super) id: String,
    #[serde(default)]
    pub(super) why: String,
}

/// How many `serves` / `cannot` entries one hello may declare.
///
/// The live vocabulary is 17 ids (`harness::chp::EVENTS`), so this is slack for
/// a future event rather than a limit anything genuine can reach — the same
/// shape and the same reasoning as [`MAX_DRIFT_MISSING`]. Without it, `serves`
/// is an unbounded list of unbounded strings that reaches an in-memory registry
/// and a Settings panel.
pub(crate) const MAX_HELLO_DECLARATIONS: usize = 32;

/// The doubling ledger for hello rows, keyed on the **resolved** `agent:tab` —
/// which is only ever reached after [`is_configured_tab`] accepted it, so the
/// key space is bounded by the user's own tab list exactly as
/// [`DISCOVERY_REPORTS`]'s is.
///
/// Two gates, not one, and they catch different things: the row is written only
/// when the hello actually CHANGED what cImp knows (a plugin re-loading with the
/// same declaration is silent), and repeats of a genuinely flip-flopping
/// declaration cost `log2(n)` rows. Process lifetime, following its two
/// siblings.
pub(super) static HELLO_SEEN: OnceLock<Mutex<HashMap<String, outbound::Doubling>>> = OnceLock::new();

/// Count one hello against the process ledger. See [`HELLO_SEEN`].
pub(crate) fn claim_hello(key: &str) -> outbound::DoublingRow {
    let ledger = HELLO_SEEN.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    claim_in(&mut ledger, key)
}

/// The caller's declaration list, bounded in both dimensions before it reaches
/// the peer registry — the [`bounded_missing`] discipline applied to `serves`
/// and to `cannot`.
pub(crate) fn bounded_declarations(raw: &[String]) -> Vec<String> {
    raw.iter()
        .take(MAX_HELLO_DECLARATIONS)
        .map(|s| bounded_id(s))
        .collect()
}

/// The Activity row one hello writes, or `None` when nothing changed.
///
/// Split from the handler for the reason [`contract_drift_row`] documents:
/// `activity::record_bg` has no `cfg(test)` diversion, so a row written inside a
/// handler is unobservable to the suite. Returning the record makes what a
/// caller can put in the store assertable without touching the global store.
pub(crate) fn hello_row(
    agent: &'static str,
    tab: &str,
    chp: u32,
    version: &str,
    serves: &[String],
    cannot: usize,
    claim: impl FnOnce(&str) -> outbound::DoublingRow,
) -> Option<crate::activity::ActivityRecord> {
    let outbound::DoublingRow::Write { total, suppressed } = claim(&format!("{agent}:{tab}")) else {
        return None;
    };
    let version = if version.is_empty() {
        "version not declared".to_string()
    } else {
        format!("v{version}")
    };
    let target = format!(
        "{agent}/{tab}: chp {chp} ({version}) — serves {}, cannot {cannot}",
        serves.len()
    );
    Some(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            // The lane `contract_drift` already uses for harness-contract facts,
            // with the same `source: "harness"`. Deliberately NOT a new
            // retention lane: a hello fires once per tab launch, and the two
            // rows a reader wants side by side ("this plugin introduced itself"
            // / "this shim's payload broke") belong in one feed.
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            String::new(), // no root — the hello is about a harness, not a project
            "harness".to_string(),
            "chp_hello".to_string(),
            target,
            serves.len(),
            0,
            // A hello is a normal, healthy event — unlike a drift report, which
            // flags its entry.
            true,
            crate::activity::Attribution::Tab(tab.to_string()),
            None,
            None,
            None,
        ),
        request: format!(
            "serves: {}",
            if serves.is_empty() {
                "(nothing declared)".to_string()
            } else {
                serves.join(", ")
            }
        ),
        response: format!(
            "hello {total} from this tab this app run, {suppressed} folded into it"
        ),
    })
}

/// `POST /session/hello` (V35 Phase I, design D3): a generated harness artifact
/// introducing itself — the protocol version it speaks, the harness version it
/// runs under (when the harness exposes one), and what it will and will not
/// serve.
///
/// # Nothing gates on this, and that is the phase's exit criterion
///
/// `serves` / `cannot` are RECORDED and DISPLAYED, never consulted by a
/// capability. Negotiation becomes load-bearing in Phase L; making it so here
/// would be a behavior change dressed as a declaration — and would hand an
/// artifact the power to switch cImp features off by lying about itself.
///
/// **`serves` is not a trust claim in either direction.** An artifact declaring
/// `tool.gate` has said nothing cImp relies on: the gate's authority is cImp
/// computing the verdict at `/latch/state`, and the artifact's only power is to
/// refuse MORE than it was told to.
///
/// # Auth, and the same honesty clause every route here owes
///
/// Bearer, inherited from the pre-dispatch [`authorized`] check. The launch
/// token is readable by any process running as this user, so "authenticated"
/// means *a local process*, never *cImp's own plugin*. Which is why the tab id
/// is validated ([`is_configured_tab`]) and every string is bounded before it
/// reaches the registry or the Settings panel.
///
/// # Answers
///
/// `200 {ok, chp}` — the ack carries the SERVER's version so a future client can
/// adapt to an older cImp. `400` on a malformed body or an unconfigured tab,
/// following [`handle_latch_beacon`]'s discipline rather than
/// [`handle_discovery_skipped`]'s constant-ack one: this route answers cImp's
/// own generated artifact, which is fail-open and discards the reply, and a
/// rejected tab is a fact worth a log line.
pub(super) async fn handle_session_hello(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let Some(body) = decode::<SessionHelloBody, _>(stream, req, bad_body_result).await? else {
        return Ok(());
    };
    let agent = crate::graph::source_for_consumer(body.agent.as_deref().unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    let tab = body.tab.as_deref().map(str::trim).unwrap_or("");
    let settings = live_settings(app);
    if tab.is_empty() || !is_configured_tab(&settings, agent, tab) {
        warn!(
            target: "offload",
            agent,
            tab = %bounded_id(tab),
            "loopback: /session/hello rejected — not a configured tab id"
        );
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(
                "/session/hello accepts configured AI tabs only — a hello with no tab has \
                 nothing to key"
                    .to_string(),
            ),
        };
        return write_json(stream, 400, &r).await;
    }
    let chp = body.chp.unwrap_or(crate::harness::chp::PRE_CHP);
    let version = bounded_id(body.harness_version.as_deref().unwrap_or("").trim());
    let serves = bounded_declarations(&body.serves);
    let cannot: Vec<crate::harness::chp::Unable> = body
        .cannot
        .iter()
        .take(MAX_HELLO_DECLARATIONS)
        .map(|u| crate::harness::chp::Unable {
            id: bounded_id(&u.id),
            why: bounded_id(&u.why),
        })
        .collect();
    let changed = crate::harness::chp::note_hello(
        agent,
        tab,
        chp,
        &version,
        serves.clone(),
        cannot.clone(),
        crate::activity::now_ms(),
    );
    if changed {
        if let Some(record) = hello_row(agent, tab, chp, &version, &serves, cannot.len(), claim_hello)
        {
            crate::activity::record_bg(record);
        }
    }
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "chp": crate::harness::chp::CHP_VERSION }),
    )
    .await
}

// ── V35 Phase L: the read path, pushed (design D2, issue #69) ────────────────
//
// Three capabilities that reached cImp by TAILING AN EMITTED ARTIFACT — Tier C,
// whose whole failure mode is silent zeros — now arrive as documented hook
// payloads. Six routes, three cores:
//
//   `/session/assistant_text`  ← `/claude/hook/stop`                (Stop)
//   `/session/tool_result`     ← `/claude/hook/post_tool_use_result` (PostToolUse, all tools)
//   `/session/subagent`        ← `/claude/hook/subagent`             (SubagentStart/Stop)
//
// **The arbitration rule lives in the cores, not in the handlers**, and it is
// the same predicate the fallback readers ask
// ([`crate::harness::chp::served`]): a capability is served for a tab when
// THAT tab's hello declared it. Both sides consulting one predicate is what
// makes "exactly one path produces this data" a property rather than a
// convention — the two failures that would otherwise be invisible are TTS
// speaking a message twice and one tool result being counted twice.
//
// What is deliberately NOT here:
//
// * **`session.usage`.** No Claude hook payload carries token counts — the
//   common input set is `session_id` / `transcript_path` / `cwd` /
//   `permission_mode` / `hook_event_name`, and `PostCompact` exposes no
//   compaction metrics either. `claude.transcript.usage` therefore stays Tier C
//   on the transcript tail, permanently-until-upstream-changes. The V35
//   milestone's Phase L row lists "usage" among the migrations; that was
//   written before the payload set was checked, and the registry row now
//   records the limitation instead of the intent.
// * **`session.context`.** Same shape of answer: the statusline stdin payload
//   has no hook equivalent.
// * **Sub-agent token usage.** `SubagentStop` carries
//   `last_assistant_message`, not tokens, and there is no sub-agent transcript
//   path in any payload — so `SubagentState::scan`'s sub-agent-lane
//   accounting keeps reading `<session_id>/subagents/agent-*.jsonl`. What
//   migrates is the LIFECYCLE (which drives the avatar), not the spend.

/// A `POST /session/assistant_text` body — one complete assistant message, as
/// prose.
///
/// **Prose, never markup or control** (design § 5.2). The sender is not trusted
/// to segment: `text` goes through `tts::prose::speak_prose`, which strips
/// terminal escapes, reduces markdown and segments app-side exactly as it does
/// for the fallback readers. A plugin controls *what* cImp says out loud, which
/// is why this capability sits in the freely-declarable data tier and the
/// per-tab `tts_injection.enabled` gate still applies.
#[derive(Deserialize)]
pub(super) struct SessionAssistantTextBody {
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) tab: Option<String>,
    #[serde(default)]
    pub(super) text: String,
}

/// A `POST /session/tool_result` body — one tool result's SIZE.
///
/// `chars` and not the content: the consumer is usage accounting, whose
/// estimated-token proxy has always been a character count
/// (`harness::claude::read::tool_result_chars`). Taking the content here would
/// put an unbounded, model-influenced blob on the wire for a `u32`'s worth of
/// information.
#[derive(Deserialize)]
pub(super) struct SessionToolResultBody {
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) tab: Option<String>,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    #[serde(default)]
    pub(super) session_id: Option<String>,
    #[serde(default)]
    pub(super) tool: Option<String>,
    #[serde(default)]
    pub(super) chars: u32,
}

/// A `POST /session/subagent` body — one sub-agent lifecycle edge.
///
/// `active` rather than an event-name string, because the only thing the
/// consumer needs is whether this id is now running: an id that started and has
/// not stopped holds the avatar in *Thinking*. A harness that grows a third
/// lifecycle state maps it onto this pair rather than teaching L3 a new word.
#[derive(Deserialize)]
pub(super) struct SessionSubagentBody {
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) tab: Option<String>,
    #[serde(default)]
    pub(super) agent_id: String,
    #[serde(default)]
    pub(super) active: bool,
}

/// A `POST /session/output_started` or `/session/output_stopped` body — one
/// turn boundary, reported by the harness itself.
///
/// Identity only: the edge is the message. Which direction it is comes from the
/// ROUTE rather than a body field, for the same reason the two `permission.*`
/// events are two routes — an edge whose direction is a payload value can be
/// dropped by a lenient parser and read as its opposite.
#[derive(Deserialize)]
pub(super) struct HarnessOutputBody {
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) tab: Option<String>,
}

/// A `POST /session/subagents_active` body — the sub-agent COUNT's zero
/// boundary, as the harness sees it.
///
/// Distinct from `session.subagent` (`/session/subagent`), which reports one
/// sub-agent's lifecycle and lets core derive the edge: a harness that already
/// knows "none running / some running" posts this and core keeps no set for it.
#[derive(Deserialize)]
pub(super) struct SubagentsActiveBody {
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) tab: Option<String>,
    #[serde(default)]
    pub(super) active: bool,
}

/// `POST /session/output_{started,stopped}`: a pushed turn boundary.
///
/// **Gated on the tab's own hello** (`chp::served`), exactly as every other
/// pushed core is, and here that gate is load-bearing for a reason worth
/// stating: core may ALSO be inferring this tab's activity from its terminal
/// (`ActivitySource::TuiMarkers`). Two producers for one avatar is the
/// double-speak V35 Phase L's arbitration exists to prevent, so a harness that
/// pushes these edges must declare them in its hello — at which point its
/// plugin declares `ActivitySource::OutOfBand` and the TUI heuristic never runs
/// for its tabs.
pub(super) async fn handle_harness_output(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
    started: bool,
) -> AppResult<()> {
    // `_body`: this route's whole payload is the identity plus the boundary,
    // and the boundary is the dispatch arm's own `started` argument.
    let Some((agent, tab, _body)) = push_admit::<HarnessOutputBody>(stream, app, req, |b| {
        (b.agent.as_deref(), b.tab.as_deref())
    })
    .await?
    else {
        return Ok(());
    };
    harness_output_core(app, agent, &tab, started);
    push_ok(stream).await
}

/// Apply one pushed turn boundary — the `harness.output_*` core.
///
/// Returns whether it acted, the same shape the other pushed cores answer with
/// so the arbitration tests can assert an exact complement.
pub(crate) fn harness_output_core(
    app: &AppHandle,
    agent: &'static str,
    tab: &str,
    started: bool,
) -> bool {
    let event = if started {
        crate::harness::chp::EV_HARNESS_OUTPUT_STARTED
    } else {
        crate::harness::chp::EV_HARNESS_OUTPUT_STOPPED
    };
    if !crate::harness::chp::served(agent, tab, event) {
        return false;
    }
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    let tab = crate::state::TabId::from_str(tab);
    let signal = if started {
        crate::state::StateSignal::HarnessOutputStarted { tab }
    } else {
        crate::state::StateSignal::HarnessOutputStopped { tab }
    };
    let _ = state.state_signals.try_send(signal);
    true
}

/// `POST /session/subagents_active`: the pushed zero-boundary of a tab's
/// sub-agent count.
pub(super) async fn handle_subagents_active(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let Some((agent, tab, body)) = push_admit::<SubagentsActiveBody>(stream, app, req, |b| {
        (b.agent.as_deref(), b.tab.as_deref())
    })
    .await?
    else {
        return Ok(());
    };
    subagents_active_core(app, agent, &tab, body.active);
    push_ok(stream).await
}

/// Apply one pushed sub-agent-count edge — the `subagents.active` core.
///
/// Emits the same `SubagentsActiveChanged` signal [`subagent_core`] derives
/// from individual lifecycles, so the state manager sees one signal shape
/// whichever path produced it.
pub(crate) fn subagents_active_core(
    app: &AppHandle,
    agent: &'static str,
    tab: &str,
    active: bool,
) -> bool {
    if !crate::harness::chp::served(agent, tab, crate::harness::chp::EV_SUBAGENTS_ACTIVE) {
        return false;
    }
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    let _ = state
        .state_signals
        .try_send(crate::state::StateSignal::SubagentsActiveChanged {
            tab: crate::state::TabId::from_str(tab),
            active,
        });
    true
}

/// Speak one pushed assistant message — the `assistant_text` core.
///
/// Returns whether it acted, which is what the arbitration tests assert on:
/// for one `(agent, tab, capability)` the answer here and the fallback reader's
/// `ctx.pushed(..)` are exact complements.
pub(crate) async fn assistant_text_core(app: &AppHandle, agent: &'static str, tab: &str, text: &str) -> bool {
    if !crate::harness::chp::served(agent, tab, crate::harness::chp::EV_ASSISTANT_TEXT) {
        // Not declared by THIS tab's artifact ⇒ its reader is still speaking,
        // and speaking here too is the double-speak this phase must not ship.
        return false;
    }
    let tab_id = crate::state::TabId::from_str(tab);
    // V39 Phase B — **delegation is a SECOND consumer of this core** (locked
    // decision 16's read half). It sits inside the `served` gate, above the
    // empty-text return and above the TTS toggle, and each of those three
    // positions is deliberate:
    //
    // * inside the gate, because arbitration decides which of the push core
    //   and the fallback reader produces this datum, and both call the same
    //   completion feed — a delegation must be told exactly once;
    // * above the empty-text return, because locked decision 13 needs to tell
    //   "the worker said nothing" (an error, now) from "the worker never
    //   answered" (a timeout, minutes later) — and it can only do that if an
    //   empty turn is reported as a completion;
    // * above the TTS path, because a delegation must complete on a tab with
    //   speech switched off.
    //
    // Additive: nothing below this line changed, so the existing TTS behaviour
    // is exactly what it was.
    crate::delegation::note_assistant_text(&tab_id, text);
    if text.trim().is_empty() {
        return false;
    }
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    crate::tts::speak_prose(
        &tab_id,
        &state.tts_segments,
        &state.settings,
        None,
        crate::tts::ProseSource::ChpPush,
        text,
    )
    .await;
    true
}

/// Record one pushed tool-result size — the `session.tool_result` core.
///
/// The same `UsageEvent::ToolResult` row the transcript tail writes, into the
/// same graph service, keyed the same way. Nothing downstream can tell which
/// path produced it, which is the point: the migration is of the SOURCE, not of
/// the data model.
pub(crate) fn tool_result_core(
    app: &AppHandle,
    agent: &'static str,
    tab: &str,
    cwd: Option<&str>,
    session_id: &str,
    tool: Option<String>,
    chars: u32,
) -> bool {
    if !crate::harness::chp::served(agent, tab, crate::harness::chp::EV_SESSION_TOOL_RESULT) {
        return false;
    }
    let (Some(cwd), false) = (cwd.filter(|c| !c.trim().is_empty()), session_id.is_empty()) else {
        // No project root or no session ⇒ nothing to attribute the row to. The
        // reader has both by construction (it IS reading a session's file), so
        // this is the push path's own honest floor.
        return false;
    };
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return false;
    };
    // #104: `record_usage` opens the project's store. A sub-agent's cwd is not
    // a root; with no resolvable one there is nothing to attribute the usage to.
    let Some(root) = external_project_root(app, &live_settings(app), Some(tab), Some(cwd)) else {
        return false;
    };
    graph.record_usage(
        &root,
        session_id,
        agent,
        crate::graph::UsageEvent::ToolResult { tool, chars },
    );
    true
}

/// The most sub-agent ids one tab's pushed lifecycle set will hold.
///
/// The set is keyed by `(agent, tab)` — both validated — but the ids inside it
/// come off the wire, and a `SubagentStart` storm with no matching stops would
/// otherwise grow one tab's set without limit. At the cap a further start is
/// counted as "still active" without being remembered individually, which
/// degrades the edge detection to coarse rather than unbounded.
pub(super) const MAX_PUSHED_SUBAGENTS: usize = 64;

/// Sub-agents currently running per `(agent, tab)`, as declared by pushes.
///
/// In-memory and non-durable for the reason the CHP peer registry is: it
/// describes live tabs, and an app restart ends every one of them. The
/// transcript tail keeps its OWN equivalent set (`update_agents`) for the tabs
/// that do not push, and the two never both drive the avatar for one tab —
/// arbitration decides which.
pub(super) type SubagentSets = HashMap<(String, String), std::collections::HashSet<String>>;
pub(super) static PUSHED_SUBAGENTS: OnceLock<Mutex<SubagentSets>> = OnceLock::new();

/// Apply one pushed sub-agent lifecycle edge — the `session.subagent` core.
///
/// Emits `StateSignal::SubagentsActiveChanged` on the empty↔non-empty EDGE only,
/// exactly as `harness::claude::read::update_agents` does, so the state manager
/// sees the same signal shape whichever path produced it.
pub(crate) fn subagent_core(
    app: &AppHandle,
    agent: &'static str,
    tab: &str,
    agent_id: &str,
    active: bool,
) -> bool {
    if !crate::harness::chp::served(agent, tab, crate::harness::chp::EV_SESSION_SUBAGENT) {
        return false;
    }
    let agent_id = bounded_id(agent_id);
    if agent_id.trim().is_empty() {
        // A lifecycle with no key cannot be closed; recording it would wedge the
        // avatar in Thinking forever. `contract_checks` reports the absence.
        return false;
    }
    let key = (agent.to_string(), tab.to_string());
    let registry = PUSHED_SUBAGENTS.get_or_init(Default::default);
    let mut registry = registry.lock().unwrap_or_else(PoisonError::into_inner);
    let set = registry.entry(key).or_default();
    let was_active = !set.is_empty();
    if active {
        if set.len() < MAX_PUSHED_SUBAGENTS {
            set.insert(agent_id);
        }
    } else {
        set.remove(&agent_id);
    }
    let now_active = !set.is_empty();
    drop(registry);
    if was_active == now_active {
        return true; // recorded, but not an edge — no signal.
    }
    if let Some(state) = app.try_state::<crate::ipc::AppState>() {
        let _ = state
            .state_signals
            .try_send(crate::state::StateSignal::SubagentsActiveChanged {
                tab: crate::state::TabId::from_str(tab),
                active: now_active,
            });
    }
    true
}

/// The `(agent, tab)` a harness-neutral Phase L body claims, **validated**.
///
/// One helper for the three routes, because all three carry the same two
/// identity fields and all three must narrow them the same way (#45's rule):
/// `agent` normalizes through `source_for_consumer` like every other route's
/// discriminator, and `tab` must name a configured AI tab for that agent.
pub(super) fn session_push_identity(
    app: &AppHandle,
    agent: Option<&str>,
    tab: Option<&str>,
) -> Option<(&'static str, String)> {
    let agent = crate::graph::source_for_consumer(agent.unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    let tab = tab.map(str::trim).unwrap_or("");
    if tab.is_empty() {
        return None;
    }
    let settings = live_settings(app);
    if !is_configured_tab(&settings, agent, tab) {
        return None;
    }
    Some((agent, tab.to_string()))
}

/// **The envelope every pushed `/session/*` route has**: decode the body in
/// this family's 400 shape, then resolve ONE identity through
/// [`session_push_identity`].
///
/// `Ok(None)` means the route is **already answered** — either the 400 a body
/// that will not parse gets, or the same 200 an identity-less or unconfigured
/// tab has always got, written here without anything being called. Both are
/// terminal and both are byte-for-byte the reply each handler wrote before
/// V42 R22 (#115) folded five copies of this preamble into one.
///
/// The CORE is deliberately NOT called here. The five differ in arity and one
/// of them is `async`; more to the point, a handler that names its own core in
/// its own body is what
/// `tests::both_transports_of_a_capability_call_one_core` and the containment
/// enumeration read, and a route must not be able to claim a core it reaches
/// only through a shared wrapper.
///
/// `identity` rather than a trait over the five body types: it is one line per
/// call site against five impl blocks, and it keeps the two fields the
/// envelope reads visible AT the route.
async fn push_admit<T: serde::de::DeserializeOwned>(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
    identity: impl FnOnce(&T) -> (Option<&str>, Option<&str>),
) -> AppResult<Option<(&'static str, String, T)>> {
    let Some(body) = decode::<T, _>(stream, req, |_| bad_request("bad request body")).await?
    else {
        return Ok(None);
    };
    let (agent, tab) = identity(&body);
    let Some((agent, tab)) = session_push_identity(app, agent, tab) else {
        push_ok(stream).await?;
        return Ok(None);
    };
    Ok(Some((agent, tab, body)))
}

/// The 200 every pushed `/session/*` route answers with, spelled once.
async fn push_ok(stream: &mut TcpStream) -> AppResult<()> {
    write_json(
        stream,
        200,
        &RunResult {
            ok: true,
            text: None,
            error: None,
        },
    )
    .await
}

/// `POST /session/assistant_text` — one complete assistant message, spoken.
pub(super) async fn handle_session_assistant_text(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let Some((agent, tab, body)) = push_admit::<SessionAssistantTextBody>(stream, app, req, |b| {
        (b.agent.as_deref(), b.tab.as_deref())
    })
    .await?
    else {
        return Ok(());
    };
    assistant_text_core(app, agent, &tab, &body.text).await;
    push_ok(stream).await
}

/// `POST /session/tool_result` — one tool result's size, recorded.
pub(super) async fn handle_session_tool_result(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let Some((agent, tab, body)) = push_admit::<SessionToolResultBody>(stream, app, req, |b| {
        (b.agent.as_deref(), b.tab.as_deref())
    })
    .await?
    else {
        return Ok(());
    };
    tool_result_core(
        app,
        agent,
        &tab,
        body.cwd.as_deref(),
        body.session_id.as_deref().unwrap_or(""),
        body.tool.as_deref().map(bounded_tool_name),
        body.chars,
    );
    push_ok(stream).await
}

/// `POST /session/subagent` — one sub-agent lifecycle edge.
pub(super) async fn handle_session_subagent(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let Some((agent, tab, body)) = push_admit::<SessionSubagentBody>(stream, app, req, |b| {
        (b.agent.as_deref(), b.tab.as_deref())
    })
    .await?
    else {
        return Ok(());
    };
    subagent_core(app, agent, &tab, &body.agent_id, body.active);
    push_ok(stream).await
}
