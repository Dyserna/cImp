//! Discovery: which instance a project root resolves to, what a probe is
//! allowed to cost, and what a reported (or forged) discovery row may say
//! about itself. The CHP observation rides here too — it reads the bodies
//! these routes actually send.

use super::*;

#[test]
fn discovery_round_trips() {
    let d = Discovery {
        port: 8123,
        token: "tok".into(),
        pid: 42,
        root: "P:\\proj".into(),
    };
    let s = serde_json::to_string(&d).unwrap();
    let back: Discovery = serde_json::from_str(&s).unwrap();
    assert_eq!(back.port, 8123);
    assert_eq!(back.token, "tok");
    assert_eq!(back.pid, 42);
    // Legacy files (pre-root) still parse: `root` defaults empty.
    let legacy: Discovery = serde_json::from_str(r#"{"port":1,"token":"t","pid":9}"#).unwrap();
    assert_eq!(legacy.root, "");
}

fn disc(pid: u32, port: u16, root: &str) -> Discovery {
    Discovery {
        port,
        token: format!("tok{pid}"),
        pid,
        root: root.to_string(),
    }
}

/// The preference ORDER on its own: every candidate answers and there is no
/// legacy store.
///
/// These four cases predate locked decision 30 and used to call
/// `select_discovery`, which now probes real sockets and reads the real
/// `.cimp-offload.json` next to the test binary. Stubbing liveness all-live
/// is what keeps them a statement about ORDERING — the property they were
/// written for — rather than about the machine they run on.
fn select_all_live(entries: Vec<Discovery>, hint: Option<&Path>) -> Option<Discovery> {
    select_verified(entries, hint, |_| true, || None)
}

#[test]
fn select_discovery_routes_by_root() {
    // Two instances off one install: a child whose cwd is inside project
    // B must reach B's instance, never last-writer-wins.
    let entries = vec![
        disc(1, 1001, &proj("proj/a")),
        disc(2, 1002, &proj("proj/b")),
    ];
    let picked = select_all_live(entries, Some(&proj_path("proj/b/src"))).expect("match");
    assert_eq!(picked.pid, 2);
}

#[test]
fn select_discovery_deepest_matching_root_wins() {
    // Nested checkouts: the closest (deepest) serving instance wins.
    let entries = vec![
        disc(1, 1001, &proj("proj")),
        disc(2, 1002, &proj("proj/nested")),
    ];
    let picked = select_all_live(entries, Some(&proj_path("proj/nested/src"))).expect("match");
    assert_eq!(picked.pid, 2);
    // A hint outside the nested root resolves to the outer instance.
    let entries = vec![
        disc(1, 1001, &proj("proj")),
        disc(2, 1002, &proj("proj/nested")),
    ];
    let picked = select_all_live(entries, Some(&proj_path("proj/other"))).expect("match");
    assert_eq!(picked.pid, 1);
}

#[cfg(windows)]
#[test]
fn select_discovery_is_case_insensitive_on_windows() {
    let entries = vec![disc(1, 1001, "p:\\PROJ\\A")];
    let picked =
        select_all_live(entries, Some(Path::new("P:\\proj\\a\\deep"))).expect("match");
    assert_eq!(picked.pid, 1);
}

#[test]
fn select_discovery_sole_entry_wins_without_a_root_match() {
    // One running instance is unambiguous even when the hint doesn't
    // land inside its root (e.g. an agent launched outside any project).
    let entries = vec![disc(7, 1007, "P:\\elsewhere")];
    let picked = select_all_live(entries, Some(Path::new("Q:\\other"))).expect("sole entry");
    assert_eq!(picked.pid, 7);
}

/// #48 F-26 — the repro two `graph/mcp.rs` comments used to document, pinned
/// as a test because the wrong version of that sentence produced a false PASS
/// in live verification (the tester truncated one file, saw a served call, and
/// recorded "no fallback reachable").
///
/// A truncated per-instance entry is dropped by `read_all_discoveries`'s
/// `filter_map(… .ok())`, so selection sees it not at all — and that is
/// exactly step 3's cue: the legacy `.cimp-offload.json` still resolves, the
/// app is still reached, and nothing goes headless. `ProxyMiss::NoInstance`
/// needs BOTH stores unusable.
#[test]
fn a_corrupt_per_instance_entry_still_resolves_through_the_legacy_file() {
    let hint = proj_path("proj/src");
    let legacy = disc(99, 4444, "");
    // The corrupted `<pid>.json` is simply absent from the entry list.
    let picked = select_verified(vec![], Some(&hint), |_| true, || Some(legacy.clone()))
        .expect("the legacy store still resolves");
    assert_eq!(picked.pid, 99, "step 3 was not consulted");
    assert_eq!(picked.port, 4444);
    // And only when the first two preferences produce nothing: a matching
    // per-instance entry must never be overridden by the legacy file.
    let picked = select_verified(
        vec![disc(1, 1001, &proj("proj"))],
        Some(&hint),
        |_| true,
        || panic!("the legacy store must not be read when a per-instance entry matches"),
    )
    .expect("match");
    assert_eq!(picked.pid, 1);
    // Decision 30 added a second condition to step 3 that F-26's original
    // wording did not have: the legacy entry must ANSWER too. A legacy file
    // naming a dead endpoint is not a resolution — it is the last candidate,
    // and `NoInstance` is the honest answer once it fails.
    assert!(
        select_verified(vec![], Some(&hint), |_| false, || Some(legacy.clone())).is_none(),
        "a dead legacy entry must not resolve (#48 F-11)"
    );
}

/// The single-write trigger F-26 named, **re-pointed by locked decision 30**.
///
/// **This test changed meaning.** It was written as
/// `a_deeper_well_formed_entry_outranks_the_running_instance` and it pinned
/// F-11's DEFECT on purpose — a deeper well-formed entry outranked the real
/// instance *whatever its port*, so ONE `Write` steered a child onto a dead
/// endpoint (and chose `ProxyMiss::Transport` as the reason the system would
/// report). Its author left the defect pinned so that a green suite could not
/// be mistaken for F-11 being closed. Decision 30 closed it, so the assertion
/// is now the post-fix invariant, and the name says which half is which:
///
/// * a deeper entry that **answers** legitimately still wins — the
///   deepest-root preference is deliberately kept (dropping it reintroduces
///   "project A's child talks to project B's app");
/// * a deeper entry that does **not** answer no longer wins, and the real
///   instance below it does.
///
/// What it still does NOT claim: that a planted entry cannot win. An attacker
/// who binds the port they wrote answers the probe. Decision 30's accepted
/// bound is "one write plus a listener", and this test is the pin on the
/// *write-only* half — see [`responds`].
#[test]
fn a_deeper_entry_outranks_the_running_instance_only_while_it_answers() {
    let real = disc(10, 4000, &proj("proj"));
    let planted = disc(11, 1, &proj("proj/sub"));
    let hint = proj_path("proj/sub/deeper");
    let no_legacy = || panic!("a matching per-instance entry answers");

    // Half 1 — a deeper entry that answers is still preferred. Depth, not
    // liveness, is what ranks; liveness only filters.
    let picked = select_verified(
        vec![real.clone(), planted.clone()],
        Some(&hint),
        |_| true,
        no_legacy,
    )
    .expect("a matching entry exists");
    assert_eq!(picked.pid, 11, "the deepest LIVE entry must still win");

    // Half 2 — the finding itself: the deeper entry is dead, so the running
    // instance underneath it serves the call instead of the child going
    // headless. `dead` is keyed on the port, which is the one thing the
    // planted file cannot fake without a listener.
    let picked = select_verified(
        vec![real, planted],
        Some(&hint),
        |d| d.port != 1,
        no_legacy,
    )
    .expect("the shallower live instance resolves");
    assert_eq!(picked.pid, 10, "a dead deeper entry must not win (#48 F-11)");
    assert_eq!(picked.port, 4000);
}

/// #48 F-28 — one `Write` no longer disarms decision 14's native-web sensor.
///
/// `taint_beacon::dispatch` resolves its endpoint with `read_discovery_for`
/// and is **fail-open by design**: no endpoint means no beacon, silently, so a
/// `WebFetch` stops contaminating the tab. That fail-open is correct and is
/// deliberately unchanged — the defect was that the resolution it failed open
/// *from* was steerable by one file write.
///
/// Driven through the beacon's own resolution shape (its cwd is the project
/// directory Claude spawned it in) against a REAL socket for the live
/// instance, so the probe itself — not a stubbed closure — is what rejects the
/// planted entry. F-28 keeps its own live-verification row; this is the unit
/// pin, not a substitute for it.
#[test]
fn a_planted_dead_entry_no_longer_disarms_the_native_web_beacon() {
    let live = fake_instance("tok-live");
    // The planted file: well-formed, a DEEPER root than the running
    // instance's, and a port nothing is listening on.
    let planted = disc(4242, dead_port(), &proj("proj/sub"));
    let real = Discovery {
        port: live,
        token: "tok-live".into(),
        pid: 10,
        root: proj("proj"),
    };
    let cwd = proj_path("proj/sub/pkg");

    let picked = select_verified(vec![real, planted], Some(&cwd), responds, || None)
        .expect("the beacon still finds the running instance");
    assert_eq!(
        picked.port, live,
        "the beacon must reach the live instance, not the planted endpoint"
    );
    assert_eq!(picked.token, "tok-live");
}

/// The probe is an authentication check as well as a liveness check: a socket
/// that answers but does not recognize the entry's token is somebody else's
/// process, and accepting it would let a planted file borrow a real port.
#[test]
fn the_probe_accepts_only_an_endpoint_that_honours_this_entrys_token() {
    let port = fake_instance("tok-right");
    let right = Discovery {
        port,
        token: "tok-right".into(),
        pid: 1,
        root: String::new(),
    };
    let wrong = Discovery {
        token: "tok-wrong".into(),
        ..right.clone()
    };
    assert!(responds(&right), "the real token must answer 200");
    assert!(!responds(&wrong), "a 401 is not an answer");
    assert!(
        !responds(&Discovery {
            port: dead_port(),
            ..right
        }),
        "a dead port is not an answer"
    );
}

/// The latency bound is a property, not a comment: a resolution probes at
/// most [`MAX_DISCOVERY_PROBES`] candidates however many entries exist, so the
/// worst case a hook shim can be made to pay is bounded by the constant and
/// not by how many files an attacker wrote.
#[test]
fn a_resolution_never_probes_more_than_its_budget() {
    let hint = proj_path("proj/a/b/c/d/e/f/g/h");
    // Twenty matching entries, each deeper than the last, none answering.
    let entries: Vec<Discovery> = (0..20)
        .map(|i| disc(i, 1000 + i as u16, &proj("proj")))
        .collect();
    let probes = std::cell::Cell::new(0usize);
    let picked = select_verified(
        entries,
        Some(&hint),
        |_| {
            probes.set(probes.get() + 1);
            false
        },
        || Some(disc(99, 4444, "")),
    );
    assert!(picked.is_none(), "nothing answered, so nothing resolves");
    assert!(
        probes.get() <= MAX_DISCOVERY_PROBES,
        "probed {} candidates, budget is {MAX_DISCOVERY_PROBES}",
        probes.get()
    );
    // And the ceiling is a non-answer, never a free pass: exhausting the
    // budget must not let an UNVERIFIED entry through.
    assert!(probes.get() > 0, "the budget must actually be spent");
}

/// A sole per-instance entry that is dead now falls through to the legacy
/// file. New with decision 30 and deliberate: a hard-killed instance leaves
/// its `<pid>.json` behind (removal is graceful-exit only), so "the sole
/// surviving entry" can be a corpse while `.cimp-offload.json` names a live
/// instance. Previously that child went headless.
#[test]
fn a_dead_sole_entry_falls_through_to_the_legacy_store() {
    let hint = PathBuf::from("Q:\\other");
    let picked = select_verified(
        vec![disc(7, 1007, "P:\\elsewhere")],
        Some(&hint),
        |d| d.pid == 99,
        || Some(disc(99, 4444, "")),
    )
    .expect("the legacy store names a live instance");
    assert_eq!(picked.pid, 99);
}

// ── #48 F-32 / locked decision 37 — the child→app discovery report ───────

/// A ledger local to one test. The production one is process-global and the
/// suite runs concurrently, so a test that used it would be racing its
/// neighbours on the shared `(no tab identity)` bucket — which is exactly
/// the bucket the key-space property is about.
fn test_ledger() -> HashMap<String, outbound::Doubling> {
    HashMap::new()
}

/// The facts a configured tab resolves to, stood in for. The `Cell` records
/// whether the closure ran at all: an identity-less report must never reach
/// the app-side resolvers, because there is no tab to resolve.
fn facts_probe(
    called: &std::cell::Cell<bool>,
) -> impl FnOnce(&str, &'static str) -> TabFacts + '_ {
    move |tab, _agent| {
        called.set(true);
        TabFacts {
            root: format!("P:\\proj\\{tab}"),
            session: Some("sess-f32".to_string()),
        }
    }
}

fn skipped_body(tab: Option<&str>, skipped: u32) -> DiscoverySkippedBody {
    DiscoverySkippedBody {
        tab: tab.map(str::to_string),
        consumer: None,
        skipped,
    }
}

/// **Decision 37's bar, clauses (1), (2) and (5).** A token-holder can cause
/// a row; it cannot make that row name a tab that is not configured, claim
/// `Headless` when the truth is unknown, carry a root or a session it chose,
/// state a count no genuine child could produce, or move anything.
///
/// Asserted **through** `record_discovery_skipped` with `test_rows`
/// observing the row the producer actually wrote — not by calling
/// `tab_identity` beside it and comparing to itself, which is the shape that
/// let three findings survive their fixes here. Deleting the `tab_identity`
/// call from the producer fails this test.
#[test]
fn a_forged_discovery_report_cannot_claim_a_tab_or_choose_what_the_row_says() {
    use crate::activity::Attribution;
    let s = settings_with_tabs(&["f32-real"]);
    let mut ledger = test_ledger();
    let row_for = |ledger: &mut HashMap<String, outbound::Doubling>,
                   body: &DiscoverySkippedBody|
     -> Option<crate::activity::ActivityRecord> {
        outbound::test_rows::reset();
        let called = std::cell::Cell::new(false);
        record_discovery_skipped(&s, body, |k| claim_in(ledger, k), facts_probe(&called));
        let mut rows = outbound::test_rows::drain();
        assert!(rows.len() <= 1, "one report, at most one row");
        let row = rows.pop();
        // The app-side resolvers run for a CONFIGURED tab and for nothing
        // else: there is no tab to resolve a root or a session for.
        if let Some(r) = &row {
            assert_eq!(
                called.get(),
                matches!(&r.entry.tab, Attribution::Tab(_)),
                "the app-side facts were resolved for a non-tab (or not for a tab)"
            );
        }
        row
    };

    // A configured id is the only one that becomes a tab — and its root and
    // session come from the app, not from the wire (there is no wire field
    // for either).
    let real = row_for(&mut ledger, &skipped_body(Some("f32-real"), 1)).expect("a row");
    assert_eq!(
        real.entry.tab,
        Attribution::Tab("f32-real".to_string())
    );
    assert_eq!(real.entry.root, "P:\\proj\\f32-real");
    assert_eq!(real.entry.session.as_deref(), Some("sess-f32"));
    assert_eq!(real.entry.source, "discovery_skipped");
    assert_eq!(real.entry.tool, "discovery");
    assert!(
        real.entry.ok,
        "containment WORKED — a denial-shaped row would say cImp blocked the child"
    );
    assert!(
        real.request.contains("\"origin\": \"http\""),
        "a local process asserted this, and the row must say so: {}",
        real.request
    );

    // An id naming no configured tab is `Unrecognized`, never `Tab` and
    // never `Headless`, and it carries no root and no session.
    let forged = row_for(&mut ledger, &skipped_body(Some("f32-not-a-tab"), 1)).expect("a row");
    assert_eq!(
        forged.entry.tab,
        Attribution::Unrecognized("f32-not-a-tab".to_string())
    );
    assert!(
        forged.entry.root.is_empty() && forged.entry.session.is_none(),
        "a forged id must not be able to file a security row under a project"
    );

    // No id at all is `Unattributed` — "this writer does not know" — and
    // explicitly NOT `Headless`, which is the positive claim "a worker run
    // with no tab behind it". That collapse is F-20's defect and F-29's; a
    // body-supplied tab is indistinguishable from an invented one, so this
    // frame cannot make the positive claim.
    let anon = row_for(&mut ledger, &skipped_body(None, 1)).expect("a row");
    assert_eq!(anon.entry.tab, Attribution::Unattributed);
    assert_ne!(anon.entry.tab, Attribution::Headless);

    // An unbounded invented id cannot choose how many bytes of a capped feed
    // one report occupies — bounded AFTER classification, so truncation can
    // never fold a long id onto a configured one.
    // (Its own ledger: the sentinel bucket is shared — which is the point of
    // the key-space assertion below — so in the main one this report would
    // land between powers of two and be correctly suppressed.)
    let long = "x".repeat(4096);
    let big = row_for(&mut test_ledger(), &skipped_body(Some(&long), 1)).expect("a row");
    let Attribution::Unrecognized(id) = &big.entry.tab else {
        panic!("a 4096-char id is not a tab: {:?}", big.entry.tab);
    };
    assert!(id.chars().count() <= BEACON_TOOL_MAX + 1, "{}", id.len());
    assert!(
        !is_configured_tab(&s, "claude", id),
        "truncation is not a forgery"
    );

    // The count is caller-asserted and the row says so, clamped to the probe
    // budget a genuine resolution cannot exceed.
    let huge = row_for(&mut ledger, &skipped_body(Some("f32-real"), 9999)).expect("a row");
    assert!(huge.response.contains("skipped 6 candidate"), "{}", huge.response);
    assert!(huge.response.contains("clamped"), "{}", huge.response);
    assert!(
        huge.response.contains("CALLER-ASSERTED"),
        "the honesty clause is not optional: {}",
        huge.response
    );

    // A report of zero skips is not a report — and it is what a malformed or
    // empty body degrades to, so neither writes anything.
    assert!(row_for(&mut ledger, &skipped_body(Some("f32-real"), 0)).is_none());
    for raw in [
        &b"{ not json"[..],
        &b""[..],
        &b"null"[..],
        &br#"{"skipped":"lots"}"#[..],
    ] {
        let body: DiscoverySkippedBody = serde_json::from_slice(raw).unwrap_or_default();
        assert_eq!(bounded_skips(body.skipped), None, "{raw:?}");
    }

    // The whole exchange keyed exactly two ledger buckets: the one
    // configured tab, plus ONE sentinel shared by every identity-less report
    // — the anonymous one and the invented-id one landed in the same bucket.
    assert_eq!(
        ledger.len(),
        2,
        "the key space is the user's tab list plus a sentinel: {:?}",
        ledger.keys().collect::<Vec<_>>()
    );
}

/// **Decision 37's bar, clause (6): the response is not the signal.**
///
/// Two halves, and each is load-bearing on its own. The reply is a single
/// constant reached by the handler's only exit — so a prober learns nothing
/// from bad JSON, an unknown tab, an anonymous tab, `skipped: 0`,
/// `skipped: 9999`, a row written or a row suppressed — **while the rows
/// those inputs produce differ**, which is what stops the property from
/// being satisfied trivially by a handler that does nothing at all.
#[test]
fn the_discovery_report_answers_identically_on_every_path() {
    // Half 1: the bytes. One constant, one exit, no branch before it.
    assert_eq!(DISCOVERY_ACK, br#"{"ok":true}"#);
    let body = handler_body("handle_discovery_skipped");
    assert_eq!(
        body.matches("write_").count(),
        1,
        "a second writer is a second response shape: {body}"
    );
    assert!(body.contains("DISCOVERY_ACK"), "{body}");
    for forbidden in ["write_json", "400", "return ", "?;", "if ", "match "] {
        assert!(
            !body.contains(forbidden),
            "`{forbidden}` in this handler is a path the reply could diverge on: {body}"
        );
    }

    // Half 2: the rows DO differ, so the constant reply is hiding something.
    let s = settings_with_tabs(&["f32-t2"]);
    let mut ledger = test_ledger();
    let wrote = |ledger: &mut HashMap<String, outbound::Doubling>, b: &DiscoverySkippedBody| {
        outbound::test_rows::reset();
        let called = std::cell::Cell::new(false);
        record_discovery_skipped(&s, b, |k| claim_in(ledger, k), facts_probe(&called));
        !outbound::test_rows::drain().is_empty()
    };
    // Reports 1 and 2 for this scope write; report 3 is folded into the
    // next power of two. Same reply, different store.
    assert!(wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)));
    assert!(wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)));
    assert!(
        !wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)),
        "the third report is suppressed — and answers identically"
    );
    assert!(!wrote(&mut ledger, &skipped_body(Some("f32-t2"), 0)));
    outbound::test_rows::reset();
}

/// **Decision 37's bar, clauses (3) and (4): a flood costs `log2(n)` rows in
/// its own lane and none anywhere else.**
///
/// Three halves. The doubling itself, asserted on the `suppressed` counts
/// and not merely on how many rows appear — a plain global cap would also
/// produce "a small number". The key space, which is the assertion
/// `/activity/contract_drift` has no equivalent of and the one that would
/// have caught F-37. And the lane, which comes free with the `Screen`
/// variant and is proved for every screen by
/// `activity::tests::no_screen_can_evict_another_screens_rows`.
#[test]
fn a_flood_of_discovery_reports_costs_log2_rows_and_evicts_nothing() {
    // Half 1 — the ledger. 200 reports on one key write 8 rows, at 1, 2, 4,
    // 8, 16, 32, 64, 128, and each names how many it stands for.
    let mut ledger = test_ledger();
    let mut written: Vec<(u32, u32)> = Vec::new();
    for _ in 0..200 {
        if let outbound::DoublingRow::Write { total, suppressed } =
            claim_in(&mut ledger, "claude:one-scope")
        {
            written.push((total, suppressed));
        }
    }
    assert_eq!(
        written,
        vec![
            (1, 0),
            (2, 0),
            (4, 1),
            (8, 3),
            (16, 7),
            (32, 15),
            (64, 31),
            (128, 63)
        ],
        "the magnitude of a loop must survive in the window, not be inferred \
         from the absence of rows"
    );

    // Half 2 — the key space. Ten thousand DISTINCT invented tab ids get ONE
    // bucket and log2-many rows, because the key is the identity the app
    // resolved and not the string the caller typed.
    outbound::test_rows::reset();
    let s = settings_with_tabs(&["f32-t3"]);
    let mut invented = test_ledger();
    for i in 0..10_000u32 {
        let called = std::cell::Cell::new(false);
        record_discovery_skipped(
            &s,
            &skipped_body(Some(&format!("invented-{i}")), 1),
            |k| claim_in(&mut invented, k),
            facts_probe(&called),
        );
    }
    assert_eq!(
        invented.len(),
        1,
        "ten thousand invented ids must not buy ten thousand counters: {:?}",
        invented.keys().collect::<Vec<_>>()
    );
    let rows = outbound::test_rows::drain();
    assert_eq!(
        rows.len(),
        14,
        "10 000 reports must cost log2 rows, not 10 000"
    );
    assert!(rows.iter().all(|r| r.entry.source == "discovery_skipped"));

    // Half 3 — the lane. Declaring the variant is what buys it; the
    // every-screen-against-every-screen matrix in `activity` covers this one
    // as both flooder and victim with no edit to that test, which is the
    // property being relied on here.
    assert!(
        outbound::Screen::ALL.contains(&outbound::Screen::DiscoverySkipped),
        "a screen missing from ALL shares the catch-all lane instead of \
         getting its own guaranteed window"
    );
}

// ── #48 F-37 / locked decision 42 — the contract-drift ledger ────────────

fn drift_ledger() -> HashMap<&'static str, outbound::Doubling> {
    HashMap::new()
}

fn drift_body(shim: &str, missing: &[&str], session: Option<&str>) -> ContractDriftBody {
    ContractDriftBody {
        shim: shim.to_string(),
        missing: missing.iter().map(|m| (*m).to_string()).collect(),
        session_id: session.map(str::to_string),
    }
}

/// **F-37's whole point: the KEY SPACE, not the map's size.**
///
/// The old ledger was a `HashSet<(shim, session_id)>` with both halves off
/// the wire, so a token-holder could mint unlimited entries and evict the
/// 400-row graph lane — genuine security rows included. The bar is the one
/// `/activity/discovery_skipped` already meets: key on something the caller
/// does not control.
///
/// Three halves. The doubling, asserted on the `suppressed` counts rather
/// than on how many rows appear (a plain global cap would also produce "a
/// small number"). The key space, asserted as **membership of a compile-time
/// list** and not as `len() < something` — an implementation that merely
/// evicted or cleared a caller-keyed map when it got big would still hold
/// caller strings, and would pass a size assertion. And the total ceiling,
/// which is what "bounded" means here: five shims plus one sentinel, for
/// every possible input, forever.
///
/// **What this would still pass if the implementation were wrong:** it does
/// not check the row's *contents* (that is the next test), and it does not
/// check that the sentinel is shared by the *right* strings — a classifier
/// that sent every name including the real ones to the sentinel would pass
/// halves 1 and 2 and fail half 3.
#[test]
fn a_flood_of_contract_drift_reports_keys_a_fixed_list_and_costs_log2_rows() {
    // Half 1 — the doubling. 200 reports on one shim write 8 rows, at 1, 2,
    // 4 … 128, each naming how many it stands for.
    let mut ledger = drift_ledger();
    let mut written: Vec<(u32, u32)> = Vec::new();
    for _ in 0..200 {
        if let outbound::DoublingRow::Write { total, suppressed } =
            drift_claim_in(&mut ledger, "read_hook")
        {
            written.push((total, suppressed));
        }
    }
    assert_eq!(
        written,
        vec![
            (1, 0),
            (2, 0),
            (4, 1),
            (8, 3),
            (16, 7),
            (32, 15),
            (64, 31),
            (128, 63)
        ],
        "the magnitude of a flood must survive in the window, not be inferred \
         from the absence of rows"
    );

    // Half 2 — the key space. Ten thousand invented shims, each with its own
    // invented session (the old key's second half), get ONE bucket and
    // log2-many rows, because the key is a classification and not a string
    // the caller typed.
    let mut invented = drift_ledger();
    let mut rows = 0;
    for i in 0..10_000u32 {
        let body = drift_body(
            &format!("invented-{i}"),
            &["session_id"],
            Some(&format!("sess-{i}")),
        );
        if contract_drift_row(&body, |k| drift_claim_in(&mut invented, k)).is_some() {
            rows += 1;
        }
    }
    assert_eq!(
        invented.len(),
        1,
        "ten thousand invented names must not buy ten thousand counters: {:?}",
        invented.keys().collect::<Vec<_>>()
    );
    assert_eq!(invented.keys().copied().collect::<Vec<_>>(), [DRIFT_SHIM_UNKNOWN]);
    assert_eq!(rows, 14, "10 000 reports must cost log2 rows, not 10 000");

    // Half 3 — the ceiling. Every real shim keeps its own counter; everything
    // else shares one; and no input of any kind can key anything else,
    // because the key type is `&'static str` from `DRIFT_SHIMS`.
    let mut all = drift_ledger();
    for shim in crate::harness::ingress::drift_tokens() {
        let body = drift_body(shim, &["cwd"], None);
        assert!(contract_drift_row(&body, |k| drift_claim_in(&mut all, k)).is_some());
    }
    for junk in ["", "   ", "read_hook ", "READ_HOOK", "read", "read_hook2", "🙂"] {
        let body = drift_body(junk, &["cwd"], Some("s"));
        let _ = contract_drift_row(&body, |k| drift_claim_in(&mut all, k));
    }
    assert_eq!(
        all.len(),
        crate::harness::ingress::drift_tokens().len() + 1,
        "the key space is the shim list plus one sentinel: {:?}",
        all.keys().collect::<Vec<_>>()
    );
    for key in all.keys() {
        assert!(
            crate::harness::ingress::drift_tokens().contains(key)
                || *key == DRIFT_SHIM_UNKNOWN,
            "a caller-supplied string reached the ledger's key space: {key}"
        );
    }
    // Trimming is the one normalisation, and it is not a prefix rule:
    // an invented name that merely starts with a real one is the sentinel.
    assert_eq!(drift_shim_key("  read_hook  "), "read_hook");
    assert_eq!(drift_shim_key("read_hook-forged"), DRIFT_SHIM_UNKNOWN);
    assert_eq!(
        drift_shim_key(&format!("read_hook{}", "x".repeat(5_000))),
        DRIFT_SHIM_UNKNOWN,
        "classification must see the whole string, never a truncation of it"
    );
}

/// **The string half of the same class: what one drift report may put IN the
/// row.**
///
/// `ActivityStore::record` truncates `request` and `response` and **not**
/// `target` — and `target` is what `ipc::commands::advisor_signals` copies
/// verbatim into a user-facing signal. So the shim name, the session id and
/// the whole `missing` list reached a capped ring at whatever length a caller
/// chose. Bounded here, after classification, exactly like F-39's id.
///
/// **What this would still pass if the implementation were wrong:** it says
/// nothing about *control characters* in those strings — that is Phase D's
/// concern at the surfaces that render, and `bounded_id`'s doc says so. It
/// would also pass a bound applied before classification, which is why the
/// key-space test above asserts the ordering separately.
#[test]
fn a_forged_contract_drift_report_cannot_choose_how_many_bytes_a_row_costs() {
    // The honest case first: byte-identical to the plain `join(", ")` this
    // replaced, so the bound costs a real report nothing.
    let mut ledger = drift_ledger();
    let real = contract_drift_row(
        &drift_body("read_hook", &["session_id", "cwd"], Some("sess-1")),
        |k| drift_claim_in(&mut ledger, k),
    )
    .expect("the first report from a shim always writes");
    assert_eq!(real.entry.target, "read_hook: session_id, cwd");
    assert_eq!(real.entry.source, "harness");
    assert_eq!(real.entry.tool, "contract_drift");
    assert_eq!(real.entry.session.as_deref(), Some("sess-1"));
    assert_eq!(real.entry.tab, crate::activity::Attribution::Unattributed);
    assert!(!real.entry.ok, "a drift report is never `ok`");
    assert!(
        real.request.contains("report 1 from this shim this app run, 0 folded into it"),
        "a folded report must be countable from the row that stands for it: {}",
        real.request
    );

    // The forged case: every caller-supplied string bounded, and the row
    // still filed under the sentinel rather than under `read_hook`.
    let huge_missing: Vec<String> = (0..5_000).map(|i| format!("{}{i}", "f".repeat(4096))).collect();
    let borrowed: Vec<&str> = huge_missing.iter().map(String::as_str).collect();
    let long = "x".repeat(4096);
    let mut forged_ledger = drift_ledger();
    let forged = contract_drift_row(
        &drift_body(&long, &borrowed, Some(&long)),
        |k| drift_claim_in(&mut forged_ledger, k),
    )
    .expect("a first report writes");
    assert_eq!(
        forged_ledger.keys().copied().collect::<Vec<_>>(),
        [DRIFT_SHIM_UNKNOWN]
    );
    // `shim: ` + at most MAX_DRIFT_MISSING bounded names + the overflow note.
    let ceiling = (BEACON_TOOL_MAX + 1) * (MAX_DRIFT_MISSING + 1) + 64;
    assert!(
        forged.entry.target.chars().count() <= ceiling,
        "{} chars reached a row the store does not truncate",
        forged.entry.target.chars().count()
    );
    assert!(
        forged.entry.session.as_deref().unwrap().chars().count() <= BEACON_TOOL_MAX + 1,
        "the session column is a join key, not a payload"
    );
    assert!(
        forged.entry.target.contains("(+4988 more)"),
        "a cut list must say how much was cut: {}",
        forged.entry.target
    );

    // A shim that sends nothing but empty strings still produces a row that
    // reads honestly — "empty" must not be spelled the same way as a name.
    let mut empty_ledger = drift_ledger();
    let empty = contract_drift_row(&drift_body("", &[], None), |k| {
        drift_claim_in(&mut empty_ledger, k)
    })
    .expect("a first report writes");
    assert_eq!(empty.entry.target, ": ");
    assert_eq!(empty.entry.session.as_deref(), Some(""));
}

// ── V35 Phase I — CHP: the hello row and the observation seam ────────────

/// A hello writes ONE row, says what the artifact declared, and a
/// flip-flopping caller costs `log2(n)` rows rather than one per hello.
///
/// The row shape matters as much as the bound: the target has to name the
/// tab, the protocol version and the sizes of the two declaration lists,
/// because that is what a reader diffing "before the upgrade / after the
/// upgrade" actually compares.
#[test]
fn a_hello_row_names_the_version_and_costs_log2_rows_under_a_flood() {
    let mut ledger: HashMap<String, outbound::Doubling> = HashMap::new();
    let serves = vec!["prompt".to_string(), "tool.gate".to_string()];
    let first = hello_row(
        "opencode",
        "opencode-1",
        crate::harness::chp::CHP_VERSION,
        "1.18.13",
        &serves,
        1,
        |k| claim_in(&mut ledger, k),
    )
    .expect("the first hello writes a row");
    assert_eq!(first.entry.source, "harness");
    assert_eq!(first.entry.tool, "chp_hello");
    assert_eq!(
        first.entry.kind,
        crate::activity::ActivityKind::Graph.as_str(),
        "the hello rides the lane `contract_drift` already uses for harness facts"
    );
    assert!(first.entry.ok, "a hello is a healthy event, not a flag");
    assert_eq!(
        first.entry.tab,
        crate::activity::Attribution::Tab("opencode-1".to_string()),
        "the tab was validated against the configured list before this point"
    );
    assert!(
        first
            .entry
            .target
            .contains(&format!("chp {}", crate::harness::chp::CHP_VERSION)),
        "{}",
        first.entry.target
    );
    assert!(first.entry.target.contains("v1.18.13"), "{}", first.entry.target);
    assert!(first.entry.target.contains("serves 2"), "{}", first.entry.target);
    assert!(first.entry.target.contains("cannot 1"), "{}", first.entry.target);
    assert!(first.request.contains("tool.gate"));
    // An undeclared version says so rather than rendering as an empty `v`.
    let quiet = hello_row("claude", "claude-1", 0, "", &[], 0, |k| {
        claim_in(&mut ledger, k)
    })
    .expect("a second key gets its own counter");
    assert!(
        quiet.entry.target.contains("version not declared"),
        "{}",
        quiet.entry.target
    );
    assert!(quiet.request.contains("(nothing declared)"));

    // The bound: 200 hellos from ONE tab write 8 rows, each stating how many
    // it stands for. The key is `agent:tab` and the tab is only ever reached
    // after `is_configured_tab` accepted it, so the key space is the user's
    // own tab list.
    let mut flood: HashMap<String, outbound::Doubling> = HashMap::new();
    let rows = (0..200)
        .filter(|_| {
            hello_row("opencode", "opencode-1", 1, "", &[], 0, |k| {
                claim_in(&mut flood, k)
            })
            .is_some()
        })
        .count();
    assert_eq!(rows, 8, "a re-hellowing plugin must cost log2 rows, not 200");
    assert_eq!(flood.len(), 1, "one tab, one counter");
}

/// The declaration lists are bounded before they reach the peer registry or
/// the Settings panel — the [`bounded_missing`] discipline, one route over.
#[test]
fn a_hellos_declarations_cannot_choose_how_much_of_the_panel_they_occupy() {
    let huge: Vec<String> = (0..500).map(|i| format!("{}-{i}", "x".repeat(400))).collect();
    let bounded = bounded_declarations(&huge);
    assert_eq!(bounded.len(), MAX_HELLO_DECLARATIONS);
    assert!(
        bounded.iter().all(|s| s.chars().count() <= BEACON_TOOL_MAX + 1),
        "each entry is truncated like every other caller-supplied id"
    );
    assert!(bounded_declarations(&[]).is_empty());
}

/// The observation seam, in the two directions that matter for "zero
/// behavior change": a pre-CHP body is READ, not rejected, and a route that
/// is not CHP is not observed at all.
///
/// The handler-side half (tab validation, the settings read) needs an
/// `AppHandle` and is covered by the route-surface tests plus
/// `harness::chp`'s own suite; what is asserted here is that the loopback's
/// pre-dispatch hook agrees with the protocol module about *what counts as a
/// CHP message*, which is the seam a new route would silently fall out of.
#[test]
fn the_chp_observation_reads_every_body_the_routes_actually_send() {
    // Exactly the body a pre-Phase-J `--context-hook` shim still posts — no `chp`.
    let (env, tab) = crate::harness::chp::envelope(
        "/context/retrieve",
        br#"{"cwd":"P:\\p","prompt":"hi","session_id":"s","agent":"claude","tab":"claude-1"}"#,
    )
    .expect("the pre-CHP Claude body is still observable");
    assert_eq!(env.chp, None);
    assert_eq!(crate::graph::source_for_consumer(env.agent_token()), "claude");
    assert_eq!(tab, "claude-1");

    // …and the body the generated plugin now sends. The literal is built
    // from `CHP_VERSION` rather than typed, because the point of this arm
    // is that the observer reads whatever the generator baked in — a
    // hard-coded number would turn every protocol bump into a red test
    // about nothing.
    let body = format!(
        "{{\"chp\":{},\"tab\":\"opencode-1\",\"consumer\":\"opencode\",\"tool\":\"webfetch\"}}",
        crate::harness::chp::CHP_VERSION
    );
    let (env, _) = crate::harness::chp::envelope("/latch/beacon", body.as_bytes())
        .expect("the plugin's beacon body");
    assert_eq!(env.chp, Some(crate::harness::chp::CHP_VERSION));
    assert_eq!(
        crate::graph::source_for_consumer(env.agent_token()),
        "opencode"
    );

    // Every route the containment table declares as CHP-carrying is one the
    // protocol module agrees to observe, and none of the non-CHP ones is.
    for row in ROUTE_CONTAINMENT {
        let observed = crate::harness::chp::is_push_route(row.path);
        let expected = matches!(
            row.path,
            "/context/retrieve"
                | "/context/compaction"
                | "/context/should_read"
                | "/context/post_edit"
                | "/memory/event"
                | "/permission/event"
                | "/latch/beacon"
                | "/latch/state"
                | "/workbench/tool_checkpoint"
                | "/activity/contract_drift"
                // V35 Phase L: the three realized read-path events. Their
                // Claude ingress twins are NOT here — a `/claude/hook/*`
                // route carries Claude's own body and its envelope rides
                // headers, which `note_chp` reads on the other branch.
                | "/session/assistant_text"
                | "/session/tool_result"
                | "/session/subagent"
                | "/session/output_started"
                | "/session/output_stopped"
                | "/session/subagents_active"
        );
        assert_eq!(
            observed, expected,
            "`{}` disagrees with `harness::chp::EVENTS` about whether it carries CHP — a new \
             route either belongs in the vocabulary (with a row in docs/CHP.md) or in this \
             list's negative half, deliberately",
            row.path
        );
    }
}

/// **The positive case, on the wire, and its negative control.** A skipped
/// planted entry reaches the app; a clean resolution says nothing.
///
/// Asserted on the BYTES a listening instance receives. `SKIPPED_CANDIDATES`
/// is deliberately not asserted anywhere here: that counter is F-11's, it
/// already works, and *the entire finding is that it has no user consumer* —
/// a test that asserts the counter re-pins the defect's shape.
#[test]
fn a_skipped_entry_is_reported_on_the_wire_and_a_clean_resolution_says_nothing() {
    let (port, seen) = recording_instance("tok-f32");
    let d = Discovery {
        port,
        token: "tok-f32".into(),
        pid: 4242,
        root: "P:\\proj".into(),
    };
    let who = ChildIdentity {
        consumer: "claude",
        tab: Some("claude-1"),
    };

    dispatch_discovery_report(&d, who, 2);
    let req = wait_for_request(&seen, 1).expect("the live instance receives the report");
    assert!(
        req.starts_with("POST /activity/discovery_skipped HTTP/1.1\r\n"),
        "{req}"
    );
    assert!(req.contains("Authorization: Bearer tok-f32\r\n"), "{req}");
    assert!(req.contains("\"skipped\":2"), "{req}");
    assert!(req.contains("\"tab\":\"claude-1\""), "{req}");
    assert!(req.contains("\"consumer\":\"claude\""), "{req}");
    // Nothing else rides along: no cwd, no path, no free text, and no
    // pid/port/root of the entries that were skipped.
    for absent in ["cwd", "root", "pid", "port", "session"] {
        assert!(!req.contains(absent), "`{absent}` on the wire: {req}");
    }

    // The count is clamped by the CHILD too, so the wire is honest rather
    // than relying on the far end to fix it.
    dispatch_discovery_report(&d, who, 9999);
    let req = wait_for_request(&seen, 2).expect("a second report");
    assert!(req.contains("\"skipped\":6"), "{req}");

    // A child with no `--tab` sends no `tab` key at all — absent, not null.
    dispatch_discovery_report(
        &d,
        ChildIdentity {
            consumer: "opencode",
            tab: None,
        },
        1,
    );
    let req = wait_for_request(&seen, 3).expect("a third report");
    assert!(!req.contains("\"tab\""), "{req}");
    assert!(req.contains("\"consumer\":\"opencode\""), "{req}");

    // **The negative control.** A resolution that skipped nothing posts
    // nothing — without this half, an implementation that reported
    // unconditionally would pass everything above.
    report_skipped_to_app(&d, who, 0);
    assert!(
        wait_for_request(&seen, 4).is_none(),
        "a clean resolution must be silent: {:?}",
        seen.lock().unwrap_or_else(PoisonError::into_inner).len()
    );

    // **Fail-open.** The endpoint is dead at report time: the dispatcher
    // returns normally, quickly, and reports nothing back to its caller —
    // its return type is `()`, so it cannot fail the child's real work.
    let dead = Discovery {
        port: dead_port(),
        ..d
    };
    let t0 = std::time::Instant::now();
    dispatch_discovery_report(&dead, who, 1);
    assert!(
        t0.elapsed() < DISCOVERY_REPORT_TIMEOUT * 4,
        "a dead endpoint must cost the stated bound at most: {:?}",
        t0.elapsed()
    );
}

/// **The hard constraint, as behaviour rather than a source scan for a call
/// that is not there.** `read_discovery_for` resolves an endpoint and sends
/// NOTHING of its own; only `proxy_base_for`, one frame up, may file the
/// skipped-candidate report.
///
/// **The callers that made this load-bearing are gone** — the two beacon
/// shims, deleted 2026-08-17, whose entire safety argument was that they
/// wrote nothing and awaited nothing on a tool call's path. The property is
/// kept rather than retired for two reasons: the split is what `proxy_base_for`
/// documents about itself, and the silent resolver is exactly what a future
/// fire-and-forget caller would reach for. A `grep` for a call that is not
/// there would stay green while a refactor moved the POST down into the
/// shared resolver; the socket assertion would not.
#[test]
fn the_discovery_report_never_reaches_the_hook_shims_path() {
    // The silent resolver's shape, against a real socket: one probe, and
    // nothing else, ever leaves this path.
    let (port, seen) = recording_instance("tok-shim");
    let live = Discovery {
        port,
        token: "tok-shim".into(),
        pid: 10,
        root: proj("proj"),
    };
    let planted = disc(4242, dead_port(), &proj("proj/sub"));
    let cwd = proj_path("proj/sub/pkg");
    let picked = select_verified(vec![live, planted], Some(&cwd), responds, || None)
        .expect("the shim still finds the running instance");
    assert_eq!(picked.port, port);
    std::thread::sleep(Duration::from_millis(250));
    let reqs = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(reqs.len(), 1, "the resolver sent more than a probe: {reqs:?}");
    assert!(reqs[0].starts_with("GET /health "), "{}", reqs[0]);

    // …and structurally: the ONE report site sits in `proxy_base_for`, after
    // the `?` that proves an endpoint resolved. V42 R2 (#114) moved the
    // resolver to `offload/discovery.rs`; the scan follows the code.
    let src = include_str!("../../discovery.rs");
    let resolver = fn_body(src, "pub fn proxy_base_for(");
    let after_q = resolver.find("let d = d?;").expect("the `?` is the guard");
    let report = resolver
        .find("report_skipped_to_app(")
        .expect("the report site");
    assert!(
        report > after_q,
        "reporting before the `?` would fire with no endpoint to report to"
    );
    assert!(
        !fn_body(src, "pub fn read_discovery_for(").contains("report_skipped_to_app"),
        "a report inside `read_discovery_for` is a write and a wait inside every \
         fire-and-forget caller of it — the shape the deleted beacon shims could \
         not survive, and the reason this resolver stays silent"
    );
    // The production ledger really is what the route claims: the process-wide
    // doubling map, not a per-call one that would bound nothing. This half is
    // about the APP side of the seam, so it reads the file the handler is in.
    // V42 R4 (#115) split the route surface again; the scan follows the code
    // by asking the whole surface which file declares the item.
    assert!(
        fn_body_in(ROUTE_SOURCES, "fn note_discovery_skipped(")
            .contains("claim_discovery_report"),
        "the handler must claim against the process ledger"
    );
}

/// A [`fake_instance`] that also **records every request it received**, so a
/// test can assert on what left a code path rather than on what that path
/// decided. Answers exactly as `fake_instance` does.
#[allow(clippy::type_complexity)]
fn recording_instance(token: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut buf = [0u8; 2048];
            let n = conn.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let ok = req.contains(&format!("Authorization: Bearer {token}\r\n"));
            sink.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(req);
            let body: &[u8] = if ok { b"ok" } else { b"unauthorized" };
            let head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                if ok { 200 } else { 401 },
                if ok { "OK" } else { "Unauthorized" },
                body.len()
            );
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(body);
        }
    });
    (port, seen)
}

/// Wait up to 250 ms for the `n`th request to arrive, since the dispatcher
/// never waits for the peer. `None` ⇒ it never came, which is the assertion
/// the negative control needs.
fn wait_for_request(seen: &Arc<Mutex<Vec<String>>>, n: usize) -> Option<String> {
    for _ in 0..50 {
        if let Some(req) = seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(n - 1)
        {
            return Some(req.clone());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

/// A port on the loopback interface that nothing is listening on: bound,
/// read, and released before the test uses the number. A `connect` to it is
/// refused immediately, which is also why a dead planted entry costs the
/// probe budget almost nothing in practice.
fn dead_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = l.local_addr().expect("addr").port();
    drop(l);
    port
}

/// A minimal stand-in for a running cImp instance: answers `GET /health`
/// with 200 when the bearer token matches and 401 when it does not, exactly
/// like `handle_conn` + `write_simple` do. Returns the port it bound.
///
/// A real socket rather than a stubbed closure because the property under
/// test is what [`responds`] puts on the wire — a stub would pass even if the
/// probe sent no `Authorization` header at all.
///
/// The accept loop runs on a **detached** thread and lives until the test
/// binary exits. Deliberate: a joinable guard would have to interrupt a thread
/// parked in `accept`, and a test that can hang waiting on its own diagnostic
/// helper is worse than one leaked thread per test.
fn fake_instance(token: &'static str) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut buf = [0u8; 512];
            let n = conn.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let ok = req.contains(&format!("Authorization: Bearer {token}\r\n"));
            let body: &[u8] = if ok { b"ok" } else { b"unauthorized" };
            let head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                if ok { 200 } else { 401 },
                if ok { "OK" } else { "Unauthorized" },
                body.len()
            );
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(body);
        }
    });
    port
}
