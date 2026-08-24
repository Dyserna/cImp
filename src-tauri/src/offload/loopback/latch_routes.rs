//! `POST /latch/beacon` and `POST /latch/state` — the taint latch's own HTTP
//! surface.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`. The
//! REGISTRY is `offload::latch` (V42 R3, #114); what is here is only what
//! the wire may ask of it — a beacon that can never do anything but
//! tighten, and a read. There is deliberately no `POST /latch/override`.

use super::*;

/// A `POST /latch/beacon` body — V32 Phase F (locked decision 14).
///
/// Posted by the OpenCode plugin's `tool.execute.before` handler when the model
/// reaches for a HARNESS-NATIVE web tool — and, until 2026-08-17, by the
/// `cimp --taint-beacon` Claude shim, which a tab open across that upgrade may
/// still be running. Claude's current path is
/// [`crate::harness::claude::hook::ROUTE_PRE_TOOL_USE_TAINT`], whose handler carries Claude's own
/// hook payload and reaches [`latch_beacon_core`] directly rather than through
/// this body. Every field except `tab` is descriptive; `tab` is the only one the
/// latch actually needs.
#[derive(Deserialize)]
pub(super) struct LatchBeaconBody {
    /// The cImp tab id the reporting harness was spawned for. Absent ⇒
    /// fail-open, exactly like [`GraphRunBody::tab`].
    #[serde(default)]
    pub(super) tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer` so one
    /// tab keys the same latch from every route.
    #[serde(default)]
    pub(super) consumer: Option<String>,
    /// The native tool that is about to run (`WebFetch`, `webfetch`, …). Log
    /// and diagnostics only.
    #[serde(default)]
    pub(super) tool: Option<String>,
}

/// A `POST /latch/state` body — V32 Phase H (locked decision 17).
///
/// Same two identity fields as [`LatchBeaconBody`] and nothing else: the query
/// is "what is in force for this tab", and the answer must not depend on
/// anything the *caller* claims about the tool it is about to run.
#[derive(Deserialize)]
pub(super) struct LatchStateBody {
    /// The cImp tab id. Absent ⇒ no scope ⇒ the fail-open answer (`gate:false`).
    #[serde(default)]
    pub(super) tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer`.
    #[serde(default)]
    pub(super) consumer: Option<String>,
}

/// `POST /latch/beacon`: engage a tab's EXTERNAL latch because the HARNESS's
/// own web tool is about to run (locked decision 14, sensor mode).
///
/// Behind the same bearer token as every other route — an unauthenticated
/// caller must not be able to latch a tab out of its local tools, which would
/// be a denial-of-service on the user's session dressed as containment.
///
/// **This is the route #45 left reachable**, and the reasoning is the asymmetry
/// between what it can do and what the removed override route could. A beacon
/// only ever TIGHTENS: Open → External, plus the contamination bit. It cannot
/// flip to Local, cannot unlatch, and cannot clear contamination. Its abuse case
/// is therefore a denial of the user's own local tools, recoverable by a tab
/// restart — not an escape from containment. Against that it has a real caller
/// (the Claude `PreToolUse` shim and the OpenCode plugin) with no IPC path
/// available to it, because it fires from a child process. Two hardenings make
/// the residual honest:
///
/// 1. **The `tab` is validated** against the user's configured AI tabs
///    ([`is_configured_tab`]) — the fix for the registry-growth finding, since
///    an unvalidated body-supplied key is the map's whole key space.
/// 2. **An engagement writes an origin-marked row** ([`outbound::Origin::Http`]),
///    so the feed says a local process asserted this rather than implying the
///    user did. Bounded to one row per tab-session by
///    [`BeaconOutcome::engaged`], because the latch is sticky.
///
/// Answers 200 with the tab's resulting view for every beacon it accepts,
/// including one with no tab identity (nothing engaged, `latch: "open"`). The
/// reporter is a fail-open shim that discards the body; the status code exists
/// for a human reading a trace, not for control flow — which is also why a
/// rejected tab id gets a 400 it will never read.
pub(super) async fn handle_latch_beacon(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let Some(body) = decode::<LatchBeaconBody, _>(stream, req, bad_body_result).await? else {
        return Ok(());
    };
    let agent =
        crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    // ONE settings read for the whole request: the tab-id check, `latch_scope`
    // and the policy must not resolve against three different snapshots.
    let settings = live_settings(app);
    // #45: reject an unknown tab id explicitly rather than letting it fall into
    // `latch_scope`'s fail-open `None`. The two are the same for the registry
    // (nothing is created either way), but they are not the same for a reader:
    // "this beacon named a tab that does not exist" is a fact worth a log line,
    // and answering 200 to it would tell a prober that the id was accepted.
    //
    // No activity row here, deliberately. The id is entirely caller-supplied,
    // so a row per rejection is an unbounded write into a capped feed — it would
    // evict the genuine rows this issue exists to preserve. The signal's
    // consumer is the enforcement itself (the request is refused) plus the
    // ABSENCE of the engagement row a real beacon leaves.
    //
    // #48: the check reads the SAME resolution the scope does, rather than a
    // second `is_configured_tab` call beside it — two spellings of one rule are
    // two things to keep in step.
    let tool = bounded_tool(body.tool.as_deref());
    match latch_beacon_core(latches(), app, &settings, agent, body.tab.as_deref(), &tool) {
        Ok(view) => write_json(stream, 200, &serde_json::json!({ "ok": true, "latch": view })).await,
        Err(tab) => {
            warn!(
                target: "offload",
                agent,
                tab = %tab,
                tool = %tool,
                "loopback: /latch/beacon rejected — not a configured tab id"
            );
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!(
                    "unknown tab id {tab:?} — /latch/beacon accepts configured AI tabs only"
                )),
            };
            write_json(stream, 400, &r).await
        }
    }
}

/// **The taint engagement itself** — the core both fire seams reach: this
/// route's harness-neutral body (the OpenCode plugin) and
/// [`crate::harness::claude::hook::ROUTE_PRE_TOOL_USE_TAINT`]'s Claude hook payload.
///
/// Split out on 2026-08-17, when the Claude side stopped being a shim POSTing to
/// the route and became a handler beside it. One core, so the two transports
/// cannot come to disagree about the #45 narrowing, the policy resolution, the
/// provenance or the row the engagement writes.
///
/// `Err(tab)` is the ONE case the two callers answer differently — the id names
/// no configured AI tab — and it is returned rather than handled here because
/// this route 400s a caller that will never read it while a Claude hook must
/// answer `{}` on every path (a `PreToolUse` non-2xx is a non-blocking error the
/// harness logs, and there is nothing to log about a hook with nothing to say).
/// Either way nothing is engaged and no registry entry is created, which is #45's
/// bound.
/// **The taint beacon, as one call a plugin can make** (V40 Phase C).
///
/// The narrow twin of [`latch_beacon_core`], for the same reason
/// [`hook_gate_admits`] is the narrow twin of [`hook_admit`]: the registry the
/// core takes is a private type, and a harness's ingress route must be able to
/// engage the latch without holding the latch machinery. Same core, same row,
/// same #45 narrowing — `Err(tab)` still means "named no configured tab, nothing
/// engaged".
pub(crate) fn latch_beacon_for(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&str>,
    tool: &str,
) -> Result<LatchView, String> {
    latch_beacon_core(latches(), app, settings, agent, tab, tool)
}

pub(super) fn latch_beacon_core(
    reg: &LatchRegistry,
    app: &AppHandle,
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&str>,
    tool: &str,
) -> Result<LatchView, String> {
    let scoping = latch_scope(app, settings, agent, tab);
    if let LatchScoping::Unknown(tab) = scoping {
        return Err(tab);
    }
    let scope = scoping.scope();
    let policy = GatePolicy::resolve(settings, scope);
    // `CallProvenance::http()`: both seams are a loopback POST from a local
    // process (a Claude hook's POST is the harness's, which is no better), and
    // the contamination row this may write has to say so for the same reason the
    // beacon row does — the launch token is readable by anything running as this
    // user (#45).
    let out = reg.beacon(scope, tool, policy, CallProvenance::http());
    report_beacon(scope, outbound::Origin::Http, tool, &out);
    Ok(out.view)
}

/// Write the [`Screen::LatchBeacon`](outbound::Screen) row for one beacon, if
/// this beacon is the one that reports.
///
/// Split out of `handle_latch_beacon` (#48, F-3) so the *pair* of rows a beacon
/// produces is assertable: `LatchRegistry::beacon` writes the contamination row
/// itself, this writes the beacon row, and the two say different things — "this
/// conversation stopped being clean" and "a harness-native web tool was
/// detected". A test that only saw one of them could not tell a regression that
/// dropped the other from a design that never had it.
///
/// Keyed on [`BeaconOutcome::report`], not on `engaged`: a beacon that
/// contaminates a `Local`-latched tab moves no latch and used to leave no trace
/// at all, while quarantining every `context_note` the tab made afterwards.
pub(super) fn report_beacon(
    scope: Option<&LatchScope>,
    origin: outbound::Origin,
    tool: &str,
    out: &BeaconOutcome,
) {
    if !out.report {
        return;
    }
    let Some(scope) = scope else { return };
    let row = beacon_row(origin, tool, out);
    outbound::record_flag(outbound::Flag {
        screen: row.screen,
        origin: row.origin,
        consumer: scope.agent,
        scope: &scope.label(),
        // The scope is in hand — see `LatchScope::attribution`.
        attribution: scope.attribution(),
        session: scope.session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root: scope.root.clone(),
        detail: &row.detail,
    });
}

/// The upper bound on a caller-supplied tool name before it reaches an activity
/// row, a log line or the TTS surface (#48). Long enough for every real
/// harness tool name (`WebFetch`, `websearch`) with room to spare.
pub(super) const BEACON_TOOL_MAX: usize = 64;

/// `/latch/beacon`'s `tool`, bounded (#48).
///
/// The field is an arbitrary unbounded string from a request body and it lands
/// in the row's `tool` column and its `detail`. Svelte escapes on render and
/// the row is bounded to one per tab-session, so this is not an injection or a
/// flood — what it is is a caller choosing how many bytes of the feed, the
/// `tracing` output and the TTS surface one beacon occupies. Truncated by
/// **chars**, not bytes, so a multi-byte name cannot be cut mid-codepoint.
///
/// Control-sequence hygiene is a separate concern with its own owner (Phase D,
/// at the surfaces that render); this only bounds length.
pub(crate) fn bounded_tool(raw: Option<&str>) -> String {
    let raw = raw.map(str::trim).filter(|t| !t.is_empty());
    let Some(raw) = raw else {
        return "(native web tool)".to_string();
    };
    bounded_id(raw)
}

/// One caller-supplied identifier, bounded before it reaches an activity row —
/// the truncation half of [`bounded_tool`], shared rather than re-spelled.
///
/// Its second caller is [`record_discovery_skipped`]'s `Unrecognized` arm (#48
/// F-32): a tab id that names no configured tab is an arbitrary unbounded string
/// from a request body, and putting it in a row verbatim would let a caller
/// choose how many bytes of a capped feed one report occupies. **Only ever
/// applied AFTER classification** — truncating first could fold a long invented
/// id onto a configured one, which would turn a bound into a forgery primitive.
///
/// Its third and fourth callers are #48 F-39 and F-37 (locked decision 42), the
/// same string half of the same class: [`LatchScoping::attribution`]'s
/// `Unrecognized` arm — reached by `/graph_run` and `/mcp/call`, and likewise
/// only after [`latch_scope`] classified the full id — and
/// [`contract_drift_row`], where the shim name and the session id a hook shim
/// reports are both arbitrary strings that reach a row.
///
/// Truncated by **chars**, not bytes, so a multi-byte id cannot be cut
/// mid-codepoint. Control-sequence hygiene is a separate concern with its own
/// owner (Phase D, at the surfaces that render); this only bounds length.
pub(crate) fn bounded_id(raw: &str) -> String {
    let mut out: String = raw.chars().take(BEACON_TOOL_MAX).collect();
    if raw.chars().nth(BEACON_TOOL_MAX).is_some() {
        out.push('…');
    }
    out
}

/// A beacon's `injection_flag` row (#45), composed from the origin the caller
/// states rather than one baked in here (#48).
///
/// Pure, so what an incident reader is told is assertable without an
/// `AppHandle` — the same seam [`override_row`] exists for. The text states the
/// origin limit in words rather than leaving it to the `origin` key, because
/// the person reading this after the fact needs to know that "the expected shim
/// sent it" is an assumption, not a finding.
///
/// The first sentence follows the outcome rather than asserting the engagement
/// case (#48): a beacon that contaminates a `Local`-latched tab, or one that
/// arrives with the latch feature off, moves no latch and refuses nothing —
/// saying it did would be the row lying about the one fact it exists to record.
pub(super) fn beacon_row(origin: outbound::Origin, tool: &str, out: &BeaconOutcome) -> FlagRow {
    let what = if out.engaged {
        "so this tab is now EXTERNAL-latched and its proxied local-capability tools will refuse"
    } else {
        "and this conversation is now CONTAMINATED — the taint latch did not move (it is not \
         Open, or the latch control is off), so nothing is refused, but every memory write from \
         here on is quarantined and every external result keeps its envelope"
    };
    FlagRow {
        screen: outbound::Screen::LatchBeacon,
        origin,
        tool: tool.to_string(),
        detail: format!(
            "NATIVE-WEB BEACON ({tool}, origin: {}): the harness's own web tool is about to run, \
             {what} (latch={}, contaminated={}). This row records an authenticated POST to \
             /latch/beacon from a local process — a cImp-generated artifact is the expected sender, \
             but the launch token is readable by anything running as this user, so this is NOT \
             evidence of a user action. This route only ever TIGHTENS: it cannot unlatch and it \
             cannot clear the contamination flag. Clearing that is a user action in cImp's own UI \
             (step 4), and no HTTP route reaches it.",
            origin.as_str(),
            out.view.latch,
            out.view.contaminated,
        ),
    }
}

/// V32 Phase H (locked decision 17): whether the OpenCode native-tool gate is
/// **in force** for one tab — the single resolved boolean the plugin is told, so
/// no part of the three-level hierarchy has to be reimplemented in JS.
///
/// It is the AND of two features, and the second one is the point:
///
/// - [`Feature::HarnessNativeGate`] — the Phase H switch itself (default off).
/// - [`Feature::TaintLatch`] — because this gate enforces *the latch's*
///   boundary on tools cImp does not route. With the latch feature off the
///   registry stops engaging (see [`GatePolicy`]), so the latch label the plugin
///   would read is not a boundary anyone is maintaining; denying against it
///   would be enforcement without a policy behind it.
///
/// Resolving it here rather than in the plugin is also what keeps the taint
/// latch a LIVE feature: the gate's own flag is spawn-baked into the plugin
/// file, but this AND is recomputed on every query, so switching the latch off
/// stops the denials without a tab restart.
/// Takes the resolved injection scope rather than a [`LatchScope`] (#48), so
/// the app-wide answer — the one a call with no *usable* tab identity gets — is
/// expressible. See [`LatchScoping::injection`].
pub(super) fn native_gate_verdict(
    settings: &crate::settings::Settings,
    s: crate::settings::injection::Scope<'_>,
) -> bool {
    use crate::settings::injection::{effective, Feature};
    effective(Feature::HarnessNativeGate, s, settings)
        && effective(Feature::TaintLatch, s, settings)
}

/// `POST /latch/state`: the resolved containment state of one tab (V32 Phase H).
///
/// **Why a new route rather than an extension of an existing one.**
/// `/latch/beacon` *mutates* — it engages the EXTERNAL latch — so reusing it for
/// a read would latch a tab every time the model touched a local file.
/// `/status` is the whole-app debug view (every tab, every feature, at every
/// scope): far more than a hook on the hot path should parse, and it answers
/// nothing about *this* tab without the plugin knowing which row is its own.
/// This route answers exactly the two facts the gate needs, for one tab.
///
/// Behind the same bearer check as every other route (it precedes dispatch in
/// [`handle_conn`]), because the reply describes a tab's containment posture.
///
/// **Always 200, and always fail-open in shape**: a tab the proxy has never
/// served, or a body with no identity at all, answer `latch: "open"` — the
/// value that denies nothing. The plugin's own error paths land on the same
/// verdict, so "the app is down" and "the app says no gate" are the same
/// behaviour rather than two.
///
/// **The `gate` half is NOT hard-coded off for an unusable tab id (#48).** It
/// resolves the feature hierarchy at whatever scope the body earns — the tab's
/// own when the id is configured, app-wide otherwise. An id that names no
/// configured tab is very often a real tab that was removed or re-id'd while
/// its per-*directory* OpenCode plugin file kept the old id (the unfixed H-2),
/// and "the gate is switched off" and "the gate cannot find your tab" must not
/// be the same answer.
///
/// Known residual, stated rather than papered over: because an unknown id keys
/// no registry entry (#45's bound, deliberately kept), its `latch` is always
/// `open`, and the plugin denies only on `external`/`local`. So the practical
/// effect for a stale plugin file is still "nothing is refused" — what changes
/// is that the verdict now reflects a decision someone took instead of a
/// collapsed `Option`. Closing it properly needs H-2: a per-tab plugin file.
pub(super) async fn handle_latch_state(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let Some(body) = decode::<LatchStateBody, _>(stream, req, bad_body_result).await? else {
        return Ok(());
    };
    let agent = wire_agent(LATCH_STATE_ROUTE, body.consumer.as_deref());
    let settings = live_settings(app);
    let scoping = latch_scope(app, &settings, agent, body.tab.as_deref());
    // #48: the verdict comes from the resolved injection scope, which is
    // app-wide for both identity-less cases — NOT a hard `false`. #45 folded
    // "an id that names no configured tab" into `latch_scope`'s `None`, and
    // this arm read that `None` as "off", so a stale plugin file (see
    // `LatchScoping::Unknown`) turned the Phase H gate off with only a `warn!`
    // on the sibling beacon route to say so. The registry is untouched either
    // way: `view_for` needs a real scope and never creates one.
    let view = scoping
        .scope()
        .map_or_else(LatchView::default, |scope| latches().view_for(scope));
    write_json(stream, 200, &latch_state_reply(&settings, &scoping, view)).await
}

/// `POST /latch/state`'s reply body, given a resolved scoping and this tab's
/// view.
///
/// Split out of [`handle_latch_state`] (#48) because the regression this issue
/// fixes lived in a `match` arm *here* — `None => (false, …)` — and this crate
/// has no `tauri::test` `AppHandle` mock, so the handler itself is unreachable
/// from a test. Everything that decides the reply is in this function now; the
/// handler's remaining work is the registry lookup, which needs the process
/// global. Re-adding "an unusable tab id means the gate is off" therefore means
/// writing it where `an_unknown_tab_id_resolves_the_app_wide_gate_verdict_not_a_hard_off`
/// can see it.
pub(super) fn latch_state_reply(
    settings: &crate::settings::Settings,
    scoping: &LatchScoping,
    view: LatchView,
) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        // The RESOLVED verdict, not the stored switch: the plugin holds no
        // part of the hierarchy. Deliberately NOT branched on whether the
        // scoping named a usable tab — see `LatchScoping::injection`.
        "gate": native_gate_verdict(settings, scoping.injection()),
        // Flattened rather than nested so the hook reads one string. The
        // full view rides along for a human reading a trace.
        "latch": view.latch,
        "contaminated": view.contaminated,
        // #48 (F-23): WHY the latch is where it is, for the one position that has
        // two possible causes. The plugin refuses the same calls either way — this
        // decides only which fixed refusal it serves, so a plugin (or a loopback)
        // that does not know the field loses nothing but the better message.
        "local_by_user_flip": view.local_by_user_flip,
    })
}
