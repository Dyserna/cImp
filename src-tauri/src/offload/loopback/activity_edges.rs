//! The routes and cores that produce an ACTIVITY EDGE rather than a reply
//! the caller reads — `/activity/contract_drift`,
//! `/activity/discovery_skipped`, and the neutral permission /
//! turn-ended edges core writes on a harness plugin's behalf.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`.

use super::*;

/// A `POST /activity/contract_drift` request body (V16 Feature 3): a hook
/// shim reporting a payload that was missing required fields.
#[derive(Deserialize)]
pub(crate) struct ContractDriftBody {
    pub(crate) shim: String,
    #[serde(default)]
    pub(crate) missing: Vec<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
}

/// The one bucket every shim name cImp does not ship shares. Parenthesized like
/// [`outbound::NO_TAB_IDENTITY`], so it cannot be confused with a real name.
pub(super) const DRIFT_SHIM_UNKNOWN: &str = "(unrecognized shim)";

/// The ledger key for a caller-supplied `shim` string: a token some registered
/// harness declares, or the one shared sentinel.
///
/// Returns `&'static str` and not `String` **on purpose** — that is the bound
/// itself rather than a check that implements it. The ledger's key type makes a
/// caller-supplied string unable to become a key at all, so the key space is
/// `drift_tokens().len() + 1` by construction and cannot drift back.
///
/// **Exact match, never a prefix** (after trimming): `"read_hook-forged"` is the
/// sentinel, not `read_hook`. A prefix or truncation rule here would let an
/// invented name claim a real shim's counter — [`bounded_id`]'s ordering rule,
/// one route over.
///
/// **V40 Phase C, locked decision 22.** The list used to be a
/// `const DRIFT_SHIMS: [&str; 10]` here — one harness's shim-token vocabulary,
/// the whole key space of the drift ledger, in core. It comes from
/// [`crate::harness::ingress::drift_tokens`] now: still `&'static str`, still
/// bounded, but by what the plugins declare. Drift fails SAFE in both
/// directions exactly as before — an undeclared shim shares the sentinel bucket
/// (fewer rows, never more), and a declared name no shim sends is a bucket
/// nothing ever claims.
///
/// The names themselves are unchanged, and deliberately: a tab open across an
/// upgrade still runs the old shim binary and still POSTs these exact strings
/// over the wire, so both paths must land in ONE bucket per capability.
pub(super) fn drift_shim_key(raw: &str) -> &'static str {
    let raw = raw.trim();
    crate::harness::ingress::drift_tokens()
        .into_iter()
        .find(|shim| *shim == raw)
        .unwrap_or(DRIFT_SHIM_UNKNOWN)
}

/// Rate-limit state for `handle_contract_drift`. A systematically broken payload
/// fires its shim on every hook invocation, and without a ledger one bad session
/// would flood the Activity store's 400-row graph ring.
///
/// **#48 F-37 / locked decision 42 — this used to be a `HashSet<(String,
/// String)>` keyed on the caller's own `shim` and `session_id`.** Both halves of
/// the key came off the wire, so any token-holder could grow it without limit and
/// evict the whole graph lane, taking genuine security rows out of a capped ring
/// with it. The fix is the bar `/activity/discovery_skipped` already meets
/// ([`DISCOVERY_REPORTS`]), in both of its halves:
///
/// * **The key is not the caller's** — it is [`drift_shim_key`]'s classification
///   of it, a `&'static str` from a compile-time list. Ten thousand invented shim
///   names buy **one** bucket.
/// * **Repeats cost `log2(n)` rows** — [`outbound::Doubling`], the same primitive
///   `claim_ssrf` and the discovery report use, and each row states how many
///   reports it stands for so a fold is never a silent drop.
///
/// **`session_id` is deliberately no longer part of the key.** It is
/// caller-supplied with nothing app-side to classify it against — this body
/// carries no tab, and the missing `session_id` is frequently the very drift
/// being reported — so keeping it would have left the unbounded half in place.
/// The cost is the documented "one row per shim per session" becoming "rows at
/// reports 1, 2, 4, 8 … per shim per app run": strictly more rows for a genuinely
/// broken shim, and the consumer is unaffected, since `drift.payload.v1` reads
/// events since process start and de-duplicates by shim
/// (`ipc::commands::advisor_signals`).
///
/// Process lifetime, unchanged and for the same reason as before.
pub(super) static CONTRACT_DRIFT_SEEN: OnceLock<Mutex<HashMap<&'static str, outbound::Doubling>>> =
    OnceLock::new();

/// Count one drift report against the process ledger. See
/// [`CONTRACT_DRIFT_SEEN`].
pub(crate) fn claim_contract_drift(shim: &'static str) -> outbound::DoublingRow {
    let ledger = CONTRACT_DRIFT_SEEN.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    drift_claim_in(&mut ledger, shim)
}

/// [`claim_contract_drift`] against a caller-owned ledger, so the key-space bound
/// and the doubling are assertable without process-global state (the suite runs
/// cases concurrently in one process). The twin of [`claim_in`].
pub(super) fn drift_claim_in(
    ledger: &mut HashMap<&'static str, outbound::Doubling>,
    shim: &'static str,
) -> outbound::DoublingRow {
    ledger.entry(shim).or_default().claim()
}

/// How many field names one drift row may list. Every real payload check in this
/// crate has at most five (`read_hook::contract_checks`), so this is slack for a
/// future check rather than a limit anything genuine can reach.
pub(super) const MAX_DRIFT_MISSING: usize = 12;

/// The caller's `missing` list, bounded in both dimensions before it reaches a
/// row (#48 F-37).
///
/// The list is an arbitrary count of arbitrary strings and it lands in the row's
/// `target` — which the store does **not** truncate (only `request` and
/// `response` are capped) and which `advisor_signals` copies verbatim into a
/// user-facing signal. Every genuine report is byte-identical to the plain
/// `join(", ")` this replaced; only abuse is cut, and the row says it was.
pub(super) fn bounded_missing(raw: &[String]) -> String {
    let mut out: Vec<String> = raw
        .iter()
        .take(MAX_DRIFT_MISSING)
        .map(|f| bounded_id(f))
        .collect();
    if let Some(extra) = raw.len().checked_sub(MAX_DRIFT_MISSING).filter(|n| *n > 0) {
        out.push(format!("… (+{extra} more)"));
    }
    out.join(", ")
}

/// The activity row one drift report writes, or `None` when the ledger folds it
/// into an earlier one.
///
/// Split from the handler and given its ledger as a closure for the reason
/// [`record_discovery_skipped`] documents, plus one this route owns:
/// `activity::record_bg` has **no `cfg(test)` diversion**, so a row written
/// inside the handler is unobservable to the suite — which is why the pre-F-37
/// behaviour had no row-level test at all. Returning the record makes what a
/// caller can put in the store assertable without touching the global store.
///
/// Nothing here is left at the caller's length: [`bounded_id`] on the shim name
/// and on the session id, [`bounded_missing`] on the field list. The bounds are
/// applied **after** [`drift_shim_key`] has classified, so a truncated name
/// cannot claim a real shim's counter.
pub(crate) fn contract_drift_row(
    body: &ContractDriftBody,
    claim: impl FnOnce(&'static str) -> outbound::DoublingRow,
) -> Option<crate::activity::ActivityRecord> {
    let outbound::DoublingRow::Write { total, suppressed } = claim(drift_shim_key(&body.shim))
    else {
        return None;
    };
    let shim = bounded_id(&body.shim);
    let session = bounded_id(body.session_id.as_deref().unwrap_or_default());
    let missing = bounded_missing(&body.missing);
    Some(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            String::new(), // no root — the report is about the harness, not a project
            "harness".to_string(),
            "contract_drift".to_string(),
            format!("{shim}: {missing}"),
            missing.chars().count(),
            0,
            false, // a drift report is never "ok" — it flags the entry in the feed
            // The report is about the harness shim, not a tab's call — but
            // the session it drifted in is known and is the join key.
            //
            // #48 F-20 left this ALONE, and `Unattributed` is honest here:
            // `ContractDriftBody` carries no `tab` field at all, so this
            // writer genuinely does not know. The shim *does* (`--tab {tab}`
            // is baked into its hook command line, `tabs/config.rs`), so the
            // fix is a wire change — `#[serde(default)] tab: Option<String>`
            // on the body plus the shim sending it — and both skew directions
            // degrade safely. That is a shim/app contract change and belongs
            // with F-6's drift-canary work, not here.
            crate::activity::Attribution::Unattributed,
            Some(session.clone()),
            None,
            None,
        ),
        request: format!(
            "shim {shim} payload missing required fields (session {session}) — report {total} \
             from this shim this app run, {suppressed} folded into it"
        ),
        response: missing,
    })
}

/// `POST /activity/contract_drift` (V16 Feature 3): record a shim's
/// payload-drift report as an Activity event (`source: "harness"`,
/// `tool: "contract_drift"`), rate-limited per shim by [`CONTRACT_DRIFT_SEEN`].
/// Always answers `{ok: true}` — the shim is fail-open and fire-and-forget.
///
/// The 400 on a malformed body is this route's own long-standing contract and is
/// **not** the discipline `handle_discovery_skipped` follows (one constant reply
/// on every path, locked decision 37). The difference is deliberate: that route
/// exists so a *child* can report containment and must give a prober no oracle;
/// this one answers a shim of ours that is already misbehaving, and locked
/// decision 42 moved the bound, not the protocol.
pub(super) async fn handle_contract_drift(stream: &mut TcpStream, req: &Request) -> AppResult<()> {
    let ok = serde_json::json!({ "ok": true });
    let body: ContractDriftBody = match serde_json::from_slice(&req.body) {
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
    if let Some(record) = contract_drift_row(&body, claim_contract_drift) {
        crate::activity::record_bg(record);
    }
    write_json(stream, 200, &ok).await
}

// ── #48 F-32 / locked decision 37: a child reports what containment did ──────

/// A `POST /activity/discovery_skipped` request body — a cImp stdio MCP child
/// saying it skipped one or more candidate discovery entries and reached this
/// instance anyway (#48 F-32).
///
/// Modelled field-for-field on [`LatchBeaconBody`]'s two identity fields,
/// including the `#[serde(default)] Option<String>` spelling and the
/// `source_for_consumer(…unwrap_or(DEFAULT_HARNESS))` normalisation, so one tab is
/// named the same way from every route. **`consumer` is a BODY field**: the
/// query-string form exists only on `/mcp/call`, whose body is not ours — it is
/// MCP JSON-RPC, owned by another protocol — so cImp's transport metadata cannot
/// go in it. Every route whose body cImp defines end to end carries `consumer`
/// here.
///
/// **Three deliberate omissions, each closing an attack:**
///
/// * **No `cwd` and no path of any kind.** `/audit/run` needs the child's cwd
///   for its wrong-instance check; this does not. A body-supplied path would let
///   a caller file a *security* row under a project it is not about. The root is
///   derived app-side from the tab ([`tab_root_key`]) or left honestly empty.
/// * **No free-text field.** `/latch/beacon` needs [`bounded_tool`] purely
///   because it accepts a caller-chosen `tool` string. With no such field there
///   is no truncation and no control-sequence question at all: this row's `tool`
///   column is the fixed literal `"discovery"`.
/// * **No pid, port or root of the skipped entries.** They would be
///   attacker-chosen strings presented to an incident reader as forensic fact.
///   What the app can say about the directory, it observes itself
///   ([`discovery_census`]).
#[derive(Deserialize, Default)]
pub(super) struct DiscoverySkippedBody {
    /// The cImp tab id the reporting child was spawned for. Absent ⇒
    /// [`crate::activity::Attribution::Unattributed`], never `Headless`.
    #[serde(default)]
    pub(super) tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer`.
    #[serde(default)]
    pub(super) consumer: Option<String>,
    /// How many candidates the child says it skipped. **Caller-asserted**, and
    /// the row says so — see [`bounded_skips`] for the bound and
    /// [`discovery_row`] for the honesty clause that states it.
    #[serde(default)]
    pub(super) skipped: u32,
}

/// The one and only response body this route ever produces.
///
/// A constant rather than a serialized value so *"the response is not the
/// signal"* is a fact about the code and not a claim about it — see
/// [`handle_discovery_skipped`].
pub(super) const DISCOVERY_ACK: &[u8] = br#"{"ok":true}"#;

/// The row's `tool` column: a fixed literal, because nothing in the body may
/// choose it.
pub(super) const DISCOVERY_TOOL: &str = "discovery";

/// `POST /activity/discovery_skipped` (#48 F-32, locked decision 37): record
/// that a cImp MCP child skipped candidate discovery entries which did not
/// answer a token-authenticated `GET /health`, and reached this instance anyway.
///
/// **Why a new route rather than an extension of `/activity/contract_drift`.**
/// That route already is a token-authenticated child→app activity-row writer —
/// the finding's claim that none existed was wrong — and reusing it is still
/// wrong, for four reasons: it writes `ActivityKind::Graph`, whose single
/// 400-row lane is shared with every real graph tool call (a security row there
/// is evictable by ordinary work, and a flood there evicts ordinary work); its
/// body carries no `tab`, so its row is honestly `Unattributed` and naming a tab
/// would be the wire change anyway; `activity::record_bg` has **no `cfg(test)`
/// diversion**, which is exactly why F-20's owed test was never written, while
/// `outbound::record_flag` does; and its dedup ledger was keyed on two
/// caller-supplied strings with no bound (F-37, filed separately — since closed
/// by locked decision 42, which gave that ledger this one's discipline and its
/// row a `contract_drift_row` seam for the same reason. The first three reasons
/// are unaffected and this route still stands on its own).
///
/// # Auth
///
/// Bearer, inherited from the pre-dispatch [`authorized`] check — no route-level
/// auth code. And the honesty clause every `Origin::Http` producer owes: **the
/// launch token is readable by any process running as this user** (from
/// `.cimp-offload.json`, from `.cimp-discovery/<pid>.json`, and from the
/// generated OpenCode plugin inside the project tree). "Authenticated" here
/// means *a local process*, never *cImp's own child*.
///
/// # The response is not the signal
///
/// **`200` with the byte-identical body [`DISCOVERY_ACK`] on every single
/// path** — malformed JSON, an empty body, an unknown tab, an anonymous tab,
/// `skipped: 0`, `skipped: 9999`, a row written, a row suppressed by the gate.
/// This function has exactly one exit and nothing before it can return early.
///
/// That **diverges deliberately** from both siblings, and the divergence is the
/// point: `handle_latch_beacon` answers 400 to an unknown tab (a tab-id
/// enumeration oracle in the other direction, moot there because a token-holder
/// can read `settings.json` anyway, but not moot as a precedent) and
/// `handle_contract_drift` answers 400 to a parse error. Locked decision 37
/// requires this route to answer identically on every path. Follow the decision,
/// not the siblings — pinned by
/// `tests::the_discovery_report_answers_identically_on_every_path`.
///
/// The real signal has three consumers, none of them this reply: the activity
/// row (the user consumer F-32 exists to add), a `warn!` on `target: "offload"`
/// (the operator consumer), and the child's own unchanged `eprintln!`.
pub(super) async fn handle_discovery_skipped(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    // Everything that can vary happens in here and returns `()`. No `?`, no
    // early return, no branch on the outcome.
    note_discovery_skipped(app, &req.body);
    write_simple(
        stream,
        200,
        "application/json; charset=utf-8",
        DISCOVERY_ACK,
    )
    .await
}

/// The app-side facts about a **configured** tab that a discovery row needs and
/// a request body must never be allowed to supply.
pub(super) struct TabFacts {
    /// [`tab_root_key`] — resolved from settings, never from the wire.
    pub(super) root: String,
    /// The V28 live-session registry's answer for this tab, never the wire's.
    pub(super) session: Option<String>,
}

/// [`handle_discovery_skipped`] minus the socket: parse the body and record the
/// row, swallowing everything.
///
/// Split so the route's single exit is structural, and so the half that needs an
/// `AppHandle` (which this crate cannot mock) is one thin frame that injects the
/// two app-derived facts into [`record_discovery_skipped`] as a closure — the
/// same seam `mark_live_session_from_event` uses.
pub(super) fn note_discovery_skipped(app: &AppHandle, raw: &[u8]) {
    // A parse failure is NOT an error path here: it degrades to a default body,
    // whose `skipped: 0` writes no row. Answering 400 would have been a second
    // response shape, i.e. the oracle this route exists without.
    let body: DiscoverySkippedBody = serde_json::from_slice(raw).unwrap_or_default();
    // ONE settings read for the whole request, the discipline `/mcp/call`
    // documents: the tab-identity check and the root resolution must not run
    // against two snapshots.
    let settings = live_settings(app);
    record_discovery_skipped(
        &settings,
        &body,
        claim_discovery_report,
        |tab, agent| TabFacts {
            root: tab_root_key(app, &settings, tab),
            session: app
                .try_state::<Arc<crate::graph::GraphService>>()
                .and_then(|g| g.live_session_for_tab(tab, agent)),
        },
    );
}

/// The `skipped` count a row may state, decided at the parse boundary.
///
/// * `None` ⇒ **no row at all.** A report of zero skips is not a report — a
///   genuine child returns before posting — and it is also what a malformed or
///   empty body degrades to. So "a caller can make the store write a row" costs
///   it at least a well-formed claim.
/// * `Some((n, clamped))` ⇒ write a row for `n`, and say so if it was clamped.
///
/// The ceiling is [`MAX_DISCOVERY_PROBES`], and it is not a guess: a single
/// resolution cannot skip more than its probe budget — [`Probe::answers`]
/// enforces that — so any larger value is *definitionally* not something a
/// genuine child produced. It is clamped rather than rejected because rejecting
/// would need a second response shape, which is the oracle
/// [`handle_discovery_skipped`] exists without.
pub(super) fn bounded_skips(raw: u32) -> Option<(u32, bool)> {
    if raw == 0 {
        return None;
    }
    let cap = MAX_DISCOVERY_PROBES as u32;
    Some((raw.min(cap), raw > cap))
}

/// The per-key doubling ledger for discovery reports (#48 F-32).
///
/// **The key space is bounded by something the caller does not control**, which
/// is the property [`CONTRACT_DRIFT_SEEN`] lacked until decision 42 gave it one
/// too (F-37 — its key is a `&'static str` from a fixed list, a stricter bound
/// than this one because that route has no tab list to key on): entries are keyed on
/// the *resolved scope label*, so a configured tab gets its own counter and
/// `Anonymous` + `Unknown(_)` share **one** sentinel bucket per consumer. A
/// caller inventing ten thousand tab ids therefore gets one counter and
/// `log2`-many rows, not ten thousand of each. Map size is bounded by
/// `2 × (configured AI tabs + 1)`.
///
/// Process lifetime, following `CONTRACT_DRIFT_SEEN`'s precedent, and that is a
/// decision rather than an omission: the doubling makes process lifetime cheap,
/// and — unlike a latch or a budget — this is not a per-conversation
/// entitlement, so there is nothing a session rotation should restore.
pub(super) static DISCOVERY_REPORTS: OnceLock<Mutex<HashMap<String, outbound::Doubling>>> = OnceLock::new();

/// Count one report against the process ledger. See [`DISCOVERY_REPORTS`].
pub(super) fn claim_discovery_report(key: &str) -> outbound::DoublingRow {
    let ledger = DISCOVERY_REPORTS.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    claim_in(&mut ledger, key)
}

/// What the APP itself currently sees in `.cimp-discovery/`.
///
/// The half of the row a request cannot forge: the app runs on the same machine
/// as the child, so instead of believing a claim about the directory it lists
/// it. Called **only on the write path** (after the doubling gate), so it can
/// never become a filesystem-scan amplifier under a flood.
pub(super) struct DirCensus {
    /// Parseable per-instance entries present right now.
    pub(super) entries: usize,
    /// …of which do not belong to this process.
    pub(super) foreign: usize,
}

pub(super) fn discovery_census() -> DirCensus {
    let own = std::process::id();
    let all = read_all_discoveries();
    DirCensus {
        entries: all.len(),
        foreign: all.iter().filter(|d| d.pid != own).count(),
    }
}

/// Everything one discovery row states, gathered so [`discovery_row`] can stay
/// pure.
pub(super) struct DiscoveryReport {
    /// The clamped, caller-asserted skip count.
    pub(super) skipped: u32,
    /// Whether the caller's number exceeded the probe budget.
    pub(super) clamped: bool,
    /// Reports this scope has filed (the doubling ledger's `total`).
    pub(super) total: u32,
    /// How many reports this row stands for beyond itself.
    pub(super) suppressed: u32,
    /// What the app observed for itself.
    pub(super) observed: DirCensus,
}

/// Record one discovery report, given a settings snapshot and a way to resolve
/// a configured tab's app-side facts.
///
/// This is where locked decision 37's bar is enforced, clause by clause. A
/// token-holder can cause a row, and cannot:
///
/// * **name a non-configured tab** — the id is re-classified through
///   [`tab_identity`] against the user's own tab list. `Configured` ⇒
///   `Attribution::Tab`; `Unknown` ⇒ `Unrecognized` (bounded, [`bounded_id`],
///   because the id is caller-chosen and unbounded on the wire); `Anonymous` ⇒
///   **`Unattributed`, never `Headless`**. `Headless` is a *positive* claim —
///   "a worker run with no tab behind it" — and a body-supplied tab is
///   indistinguishable from an invented one, so claiming it would be F-20's
///   defect and F-29's, one producer further on. `Attribution::from_child_argv`
///   is forbidden here by its own doc for exactly that reason.
/// * **say anything a genuine row could not** — every remaining field is
///   app-derived: `root` from [`tab_root_key`], `session` from the V28 live
///   registry, `consumer` normalized to one of two words, `tool` a fixed
///   literal, `origin` fixed to `Http`, `skipped` clamped by [`bounded_skips`],
///   and the directory census observed rather than asserted.
/// * **cost another lane a row** — `Screen::DiscoverySkipped` is its own H-9
///   retention lane, so a flood here evicts only discovery rows
///   (`activity::tests::no_screen_can_evict_another_screens_rows` covers it for
///   every screen, this one included, without an edit).
/// * **exceed `log2(n)` rows in its own lane** — [`DISCOVERY_REPORTS`].
/// * **touch any latch** — nothing in this path reaches `latches()`; it holds no
///   registry handle and creates no entry, which
///   `every_loopback_route_declares_what_it_does_about_the_latch` checks against
///   the handler's source rather than believing.
///
/// `claim` and `facts` are injected for the same reason and it is not only
/// testability: the ledger is a process-global map and the facts need an
/// `AppHandle` this crate cannot mock, so a test that had to go through either
/// would be racing its neighbours or unable to run at all. Production wires
/// [`claim_discovery_report`] here — pinned by
/// `tests::the_discovery_report_never_reaches_the_hook_shims_path`, which reads
/// the wiring out of the source rather than trusting it.
pub(super) fn record_discovery_skipped(
    settings: &crate::settings::Settings,
    body: &DiscoverySkippedBody,
    claim: impl FnOnce(&str) -> outbound::DoublingRow,
    facts: impl FnOnce(&str, &'static str) -> TabFacts,
) {
    let Some((skipped, clamped)) = bounded_skips(body.skipped) else {
        return;
    };
    let agent = crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or(crate::harness::DEFAULT_HARNESS.token()));
    let identity = tab_identity(settings, agent, body.tab.as_deref());
    // The scope label doubles as the flood key, which is deliberate: both want
    // "the identity this call actually resolved to", and the identity-less cases
    // must collapse onto one bucket rather than onto whatever the caller typed.
    let scope = match identity {
        TabIdentity::Configured(tab) => format!("{agent}:{tab}"),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => {
            format!("{agent}:{}", outbound::NO_TAB_IDENTITY)
        }
    };
    let outbound::DoublingRow::Write { total, suppressed } = claim(&scope) else {
        return;
    };

    let (attribution, root, session) = match identity {
        TabIdentity::Configured(tab) => {
            let f = facts(tab, agent);
            (
                crate::activity::Attribution::Tab(tab.to_string()),
                f.root,
                f.session,
            )
        }
        TabIdentity::Unknown(tab) => (
            crate::activity::Attribution::Unrecognized(bounded_id(tab)),
            String::new(),
            None,
        ),
        TabIdentity::Anonymous => (
            crate::activity::Attribution::Unattributed,
            String::new(),
            None,
        ),
    };

    let row = discovery_row(
        outbound::Origin::Http,
        &DiscoveryReport {
            skipped,
            clamped,
            total,
            suppressed,
            observed: discovery_census(),
        },
    );
    // The operator consumer. The user consumer is the row below; the child's own
    // stderr line is the third and is unchanged.
    warn!(
        target: "offload",
        agent,
        scope = %scope,
        skipped,
        total,
        suppressed,
        "loopback: /activity/discovery_skipped — a child skipped candidate discovery entries \
         and reached this instance anyway"
    );
    outbound::record_flag(outbound::Flag {
        screen: row.screen,
        origin: row.origin,
        consumer: agent,
        scope: &scope,
        attribution,
        session: session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root,
        detail: &row.detail,
    });
}

/// A discovery report's `injection_flag` row, composed by a **pure** function so
/// what an incident reader is told is assertable without an `AppHandle` — the
/// same seam [`beacon_row`] and [`override_row`] exist for.
///
/// The prose carries six facts, and none of them is optional:
///
/// 1. what happened (a child skipped N candidate entries that did not answer a
///    token-authenticated `GET /health`);
/// 2. that **containment worked, and this row is the proof** — the child reached
///    *this* instance anyway, which is how the report arrived at all;
/// 3. the benign cause (an unclean shutdown leaves `.cimp-discovery/<pid>.json`
///    behind; removal is graceful-exit only);
/// 4. the hostile cause (this is also exactly what a **planted** entry looks
///    like — #48 F-11/F-28, locked decision 30);
/// 5. what to do (list `.cimp-discovery/` next to the cImp executable), together
///    with what the app observed there itself;
/// 6. the **honesty clause**: an authenticated POST from a local process is not
///    evidence of a user action, the count is caller-asserted, and nothing here
///    moved a latch, contaminated a conversation or refused a call.
pub(super) fn discovery_row(origin: outbound::Origin, rep: &DiscoveryReport) -> FlagRow {
    let n = rep.skipped;
    let clamped = if rep.clamped {
        format!(
            " (the caller's number exceeded the probe budget of {MAX_DISCOVERY_PROBES} and was \
             clamped)"
        )
    } else {
        String::new()
    };
    let stands_for = if rep.suppressed > 0 {
        format!(
            " This is report {} for this scope and stands for {} further report(s) folded into \
             it (rows are written at 1, 2, 4, 8 … so a loop cannot evict this lane's history).",
            rep.total, rep.suppressed
        )
    } else {
        String::new()
    };
    FlagRow {
        screen: outbound::Screen::DiscoverySkipped,
        origin,
        tool: DISCOVERY_TOOL.to_string(),
        detail: format!(
            "DISCOVERY ENTRY SKIPPED (origin: {}): a cImp MCP child resolved its loopback \
             endpoint and skipped {n} candidate discovery entr(ies) that did not answer a \
             token-authenticated `GET /health`{clamped} — and then reached THIS instance anyway, \
             which is how this report arrived. Containment worked; this row is the proof, not an \
             alarm about a failure. After an unclean cImp shutdown a leftover \
             `.cimp-discovery/<pid>.json` produces exactly this and is harmless (removal is \
             graceful-exit only). It is ALSO what a PLANTED entry looks like: a well-formed file \
             naming a deeper project root and a port nothing serves is how untrusted content \
             steers a child onto a dead endpoint (#48 F-11/F-28, locked decision 30). If you did \
             not expect it, list `.cimp-discovery/` next to the cImp executable — this app sees \
             {} entr(ies) there right now, {} of them not its own process.{stands_for} This row \
             records an authenticated POST from a local process: the launch token is readable by \
             anything running as this user, so it is NOT evidence of a user action, and the count \
             is CALLER-ASSERTED (clamped to the probe budget of {MAX_DISCOVERY_PROBES}; the \
             directory figures above are the app's own observation and are not). Nothing here \
             moved a latch, contaminated a conversation or refused a call.",
            origin.as_str(),
            rep.observed.entries,
            rep.observed.foreign,
        ),
    }
}

// ── NC-2 (issue #5): the neutral half of hook-driven permission detection ────
//
// **V40 Phase C, locked decision 21.** What used to live here was the whole
// chain: Claude's `Notification` payload struct, its marker strings, its
// `IGNORED_NOTIFICATION_TYPES` list transcribed from the hooks guide, the
// classifier that reads `hook_event_name`, and the session-id → transcript-stem
// → cwd resolution that knows what a Claude transcript path looks like. All of
// that is `harness/claude/hook.rs` now.
//
// What stays is the part that is true of prompt detection in general: the tabs
// an edge could belong to, and the signal an edge becomes. The TUI-regex
// detector produces the same [`PermissionEdge`] from a screen scrape, and both
// producers are idempotent at the state manager — which is why a hook and a
// regex match for the same prompt collapse to one edge rather than being two
// features that must agree.

/// One tab a permission edge could belong to: its id, the harness session id it
/// is currently running (from the graph's live-session registry — `None` for a
/// configured-but-not-running tab), and the directory it launches in.
#[derive(Debug, Clone)]
pub(crate) struct PermissionTabCandidate {
    pub(crate) tab: String,
    pub(crate) session_id: Option<String>,
    pub(crate) cwd: PathBuf,
}

/// What one permission payload did: the tab whose state signal was sent, or why
/// nothing was sent.
///
/// The route answers 200 on every arm — the producers are observe-only and must
/// never be given a reason to retry — so this exists to keep the *diagnosis* out
/// of the transport, not to give a caller anything to branch on.
pub(crate) enum PermissionOutcome {
    Mapped(String),
    Unmapped(&'static str),
}

/// Every tab a permission edge could be attributed to, with the session each is
/// currently running.
///
/// Snapshots what is needed from managed state and drops the guards — nothing
/// borrowed from `AppHandle` is held across a response write. An empty answer
/// (no `AppState`, no configured tabs) is not an error: it makes the resolution
/// find nothing, which is the same "drop it rather than guess" outcome an
/// ambiguous match produces.
pub(crate) fn permission_tab_candidates(
    app: &AppHandle,
    harness: crate::harness::HarnessId,
) -> Vec<PermissionTabCandidate> {
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return Vec::new();
    };
    let sessions: Vec<(String, String)> = app
        .try_state::<Arc<crate::graph::GraphService>>()
        .map(|g| g.live_sessions_for(harness))
        .unwrap_or_default();
    crate::tabs::harness_tab_dirs(&state.settings.current(), &state.launch.cwd, harness)
        .into_iter()
        .map(|(tab, dir)| PermissionTabCandidate {
            session_id: sessions
                .iter()
                .find(|(k, _)| *k == tab)
                .map(|(_, s)| s.clone()),
            tab,
            cwd: dir,
        })
        .collect()
}

/// Emit one neutral permission edge for `tab`, returning whether it was sent.
///
/// The SAME `StateSignal`s the TUI-regex detector emits, so the whole downstream
/// pipeline (`awaiting_permission` → TTS enqueue, per-tab badge, avatar) is
/// untouched by which producer found the prompt.
///
/// Edge-triggered and best-effort, exactly like the PTY processor's `try_send`:
/// a full channel means the state manager is saturated, and the regex detector's
/// next scan re-raises the edge anyway.
pub(crate) async fn send_permission_edge(
    app: &AppHandle,
    tab: &str,
    edge: crate::harness::plugin::PermissionEdge,
) -> bool {
    use crate::harness::plugin::PermissionEdge;
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    let signals = state.state_signals.clone();
    let registry = state.tabs.clone();
    let tab_id = crate::state::TabId::from_str(tab);
    let signal = match edge {
        PermissionEdge::Detected => crate::state::StateSignal::PermissionPromptDetected {
            tab: tab_id.clone(),
        },
        PermissionEdge::Resolved => crate::state::StateSignal::PermissionPromptResolved {
            tab: tab_id.clone(),
        },
    };
    let _ = signals.try_send(signal);
    // M11 (2026-08-05 review): a hook-driven Resolved clears the flag eagerly —
    // a denial from the harness's own auto-classifier can land while a genuine
    // approval prompt is still on screen. The regex fallback cannot recover on
    // its own: `PermissionDetector::check` is edge-triggered on a latched
    // per-kind pattern name, so while that same pattern keeps matching it emits
    // NOTHING. Drop the latch (and re-scan) in the tab's PTY processor so a
    // prompt that is genuinely still up is re-raised immediately. Sent AFTER
    // the Resolved signal so the two land on the state manager in that order.
    if matches!(edge, PermissionEdge::Resolved) {
        registry.lock().await.clear_permission_latch(&tab_id).await;
    }
    true
}

/// **The harness said this tab's assistant turn is over.** Relays it to the
/// state manager as [`crate::state::StateSignal::HarnessTurnEnded`], which
/// re-emits it as `StateEvent::TurnEnded` without touching the avatar.
///
/// Shaped like [`send_permission_edge`] — same lookup, same `try_send`, same
/// "no app state yet ⇒ drop it" answer. A dropped signal costs one missed idle
/// announcement, never correctness: nothing downstream latches on it.
///
/// Only a harness whose plugin declares
/// [`crate::harness::plugin::HarnessPlugin::turn_end_push`] has a producer for
/// this; see that method for why the Idle edge is not the same thing.
pub(crate) async fn send_turn_ended(app: &AppHandle, tab: &str) -> bool {
    let Some(state) = app.try_state::<crate::ipc::AppState>() else {
        return false;
    };
    let signal = crate::state::StateSignal::HarnessTurnEnded {
        tab: crate::state::TabId::from_str(tab),
    };
    let _ = state.state_signals.try_send(signal);
    true
}
