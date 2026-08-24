//! The contamination bit: written exactly once per tab-session, surviving
//! flips and rotations, and cleared only on evidence — plus the rows every one
//! of those transitions owes.

use super::*;

/// One row's request payload, parsed.
fn payload(row: &crate::activity::ActivityRecord) -> serde_json::Value {
    serde_json::from_str(&row.request).expect("the row's request payload is JSON")
}

/// The quarantine-only posture: the switch combination that made the
/// proxied path contaminate in complete silence.
const QUARANTINE_ONLY: GatePolicy = GatePolicy {
    latch: false,
    quarantine: true,
};

/// The primary path, which before this wrote **nothing at all**: an
/// admitted proxied EXTERNAL call. One row, carrying when / which tool /
/// which page / which project / which conversation.
///
/// The "exactly once" half is the other half of the finding: the row must
/// name the moment the conversation stopped being clean, so a second
/// EXTERNAL call — which restates a fact this row already carries, and
/// writes its own ordinary MCP activity row — must not write another.
#[test]
fn the_proxied_intake_records_the_contamination_transition_exactly_once() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil.example/page"), Some("evil.example")),
        )
        .is_ok());
    // A second EXTERNAL call, in the same conversation, from a different
    // page: the conversation is already contaminated.
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            CallProvenance::intake(Some("https://other.example/q"), Some("other.example")),
        )
        .is_ok());

    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(
        hits.len(),
        1,
        "a contaminated conversation must produce exactly one transition row, got {:?}",
        hits.iter().map(|r| &r.entry.tool).collect::<Vec<_>>()
    );
    let row = hits[0];
    // WHEN — the standard stamp, not a field the writer invented.
    assert!(row.entry.ts_ms > 0, "the row has no timestamp");
    // WHICH TOOL — the call that caused the transition, not the later one.
    assert_eq!(row.entry.tool, "ddg__fetch_content");
    // WHICH PROJECT — the field F-3 calls load-bearing. An empty root here
    // makes the row invisible to every per-project surface.
    assert_eq!(row.entry.root, TEST_ROOT);
    assert!(!row.entry.root.is_empty());
    // Nothing was refused: the call was admitted, so the feed must not
    // paint this as a failure.
    assert!(row.entry.ok, "a contamination row is not a denial");
    let req = payload(row);
    assert_eq!(req["screen"], "contamination");
    assert_eq!(req["origin"], "internal");
    assert_eq!(
        req["scope"], "claude:claude-1",
        "the LatchScope::label form"
    );
    // WHICH CONVERSATION — what step 3 will join a checkpoint against.
    assert_eq!(req["session"], "sess-a");
    // FROM WHICH PAGE.
    assert_eq!(req["host"], "evil.example");
    assert_eq!(req["url"], "https://evil.example/page");
    assert_eq!(row.entry.target, "evil.example (claude:claude-1)");
    assert!(
        row.response.contains("CONTAMINATED"),
        "the detail must say what happened: {}",
        row.response
    );
    // The latch the call LEAVES the tab in, not the one it found. A row
    // written before `engage` would say `open` about a tab that is
    // EXTERNAL-latched from this very call — the reader would then look for
    // a second event that never happened.
    assert!(
        row.response.contains("latch=external"),
        "the row quotes the pre-engagement latch: {}",
        row.response
    );
}

/// The beacon path records the transition **as well as** its own
/// `latch_beacon` row. The two are different statements — "this
/// conversation stopped being clean" and "a harness-native web tool was
/// detected" — and a build that collapsed them into one would still pass
/// every count-shaped assertion about "a beacon writes a row".
#[test]
fn a_beacon_writes_the_contamination_row_and_its_own_beacon_row() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &out);

    let rows = outbound::test_rows::drain();
    assert_eq!(contamination_rows(&rows).len(), 1, "no contamination row");
    assert_eq!(
        outbound::test_rows::of_screen(&rows, outbound::Screen::LatchBeacon).len(),
        1,
        "the beacon row this work must not have displaced"
    );
    let row = contamination_rows(&rows)[0];
    assert_eq!(row.entry.tool, "WebFetch");
    assert_eq!(row.entry.root, TEST_ROOT);
    let req = payload(row);
    // A beacon is a local process POSTing the loopback, never evidence a
    // human acted — the row has to say so (#45).
    assert_eq!(req["origin"], "http");
    assert_eq!(req["scope"], "claude:claude-1");
    assert_eq!(req["session"], "sess-a");
    // Nothing was fetched *through* cImp, so there is no page to name —
    // absent rather than invented.
    assert_eq!(req["host"], serde_json::Value::Null);

    // And a caller in a loop writes neither row again.
    for _ in 0..5 {
        let again = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &again);
    }
    assert!(
        outbound::test_rows::drain().is_empty(),
        "the transition is over; a loop must not be able to flood the feed"
    );
}

/// **The two silent cases F-3 is about.** Both contaminate without moving
/// any latch, so a fix keyed on the latch transition — or a test that only
/// exercised the happy path — leaves exactly the bug being fixed.
#[test]
fn contamination_is_recorded_even_when_no_latch_moves() {
    // (a) A tab already latched LOCAL. The beacon cannot flip it (the fetch
    //     already happened), so nothing about the latch changes — while
    //     every `context_note` from here on is quarantined.
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Native,
            "graph_snippet",
            ON,
            NO_CONTENT
        )
        .is_ok());
    assert_eq!(reg.snapshot()[0].latch(), "local");
    let _ = outbound::test_rows::drain();

    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert!(!out.engaged, "a beacon never flips a LOCAL latch");
    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(hits.len(), 1, "the LOCAL-latched case recorded nothing");
    assert_eq!(hits[0].entry.root, TEST_ROOT);
    assert_eq!(payload(hits[0])["scope"], "claude:claude-1");

    // (b) The taint latch feature OFF, the memory quarantine ON. The
    //     contamination bit is still tracked (it is the quarantine's input),
    //     the latch never engages, and this is the posture under which the
    //     proxied path was silent even for a brand-new conversation.
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let t = scope("claude-2", Some("sess-b"));
    assert!(reg
        .gate(
            Some(&t),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            QUARANTINE_ONLY,
            CallProvenance::intake(Some("https://p.example/x"), Some("p.example")),
        )
        .is_ok());
    assert_eq!(
        reg.snapshot()[0].latch(),
        "open",
        "the latch feature is off, so nothing engaged"
    );
    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(hits.len(), 1, "the latch-off case recorded nothing");
    assert_eq!(hits[0].entry.root, TEST_ROOT);
    assert_eq!(payload(hits[0])["host"], "p.example");
    assert_eq!(payload(hits[0])["session"], "sess-b");
    assert!(
        hits[0].response.contains("latch=open"),
        "with the latch feature off the row must not claim a latch: {}",
        hits[0].response
    );
    // The quarantine that follows is the fact the row explains.
    assert_eq!(
        reg.gate(
            Some(&t),
            LatchRoute::Native,
            "context_note",
            QUARANTINE_ONLY,
            NO_CONTENT
        ),
        Ok(WriteTaint::Quarantined)
    );
}

/// The row follows the BIT, so everything that does not set the bit writes
/// nothing: a purely local conversation, a REFUSED external call (which
/// must never contaminate — that is what keeps a hallucinated call to the
/// blocked side from quarantining a clean session), a native route's
/// EXTERNAL-classified name (a typo, not a page), and an inert policy.
#[test]
fn nothing_that_leaves_the_conversation_clean_writes_a_contamination_row() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    for name in ["graph_snippet", "graph_outline", "context_recall"] {
        assert!(reg
            .gate(Some(&s), LatchRoute::Native, name, ON, NO_CONTENT)
            .is_ok());
    }
    // EXTERNAL on a NATIVE route: a misspelled native tool, not content.
    assert!(reg
        .gate(Some(&s), LatchRoute::Native, "ddg__search", ON, NO_CONTENT)
        .is_ok());
    // The tab is LOCAL-latched now, so a proxied external call is refused.
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__search",
            ON,
            CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
        ),
        Err(REFUSAL_EXTERNAL_BLOCKED)
    );
    // Both controls off: a disabled control leaves no trace at all.
    const OFF: GatePolicy = GatePolicy {
        latch: false,
        quarantine: false,
    };
    let inert = scope("claude-3", Some("sess-c"));
    assert!(reg
        .gate(
            Some(&inert),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            OFF,
            CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
        )
        .is_ok());
    assert!(
        !reg.beacon(Some(&inert), "WebFetch", OFF, BEACON_PROV)
            .report
    );

    let rows = outbound::test_rows::drain();
    assert!(
        contamination_rows(&rows).is_empty(),
        "a clean conversation was reported as contaminated: {:?}",
        contamination_rows(&rows)
            .iter()
            .map(|r| &r.entry.tool)
            .collect::<Vec<_>>()
    );
    assert!(!reg.snapshot()[0].view.contaminated);
}

/// A tab with no identity keys nothing and reports nothing — the fail-open
/// reading every gate here takes. Stated as a test because the row's whole
/// value is per-tab attribution, and a row scoped to "(no tab identity)"
/// would be a row no per-project surface could use.
#[test]
fn an_identityless_call_records_no_contamination() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    assert!(reg
        .gate(
            None,
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
        )
        .is_ok());
    let _ = reg.beacon(None, "WebFetch", ON, BEACON_PROV);
    let rows = outbound::test_rows::drain();
    assert!(contamination_rows(&rows).is_empty());
}

/// **One transition per TAB, not per conversation** — and the row's
/// `session` therefore names the conversation contamination *started* in.
///
/// This follows H-2 rather than the beacon's own reporting rule, and the
/// difference is deliberate on both sides. `observe` re-arms
/// `beacon_flagged` on a proved session rotation (a new conversation may
/// report a native web tool again) but does **not** clear `contaminated`,
/// because the rotation signal is a file the model's own shell can write.
/// So a `/clear` in a contaminated tab keeps the taint, keeps quarantining
/// its memory writes — and writes no second row, because nothing
/// transitioned.
///
/// Pinned as a test because a consumer that joins these rows to
/// conversation-scoped state has to know it: the anchor is the tab's first
/// contamination, not "the contamination of the session you are looking
/// at". If the contamination bit ever regains a clear path, this is the
/// test that has to be revisited with it.
#[test]
fn contamination_is_recorded_once_per_tab_across_session_rotations() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let first = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&first),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(None, Some("a.example")),
        )
        .is_ok());
    let rotated = scope("claude-1", Some("sess-b"));
    assert!(reg
        .gate(
            Some(&rotated),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(None, Some("b.example")),
        )
        .is_ok());
    // The rotation did happen — the latch reopened and the budget refilled…
    assert_eq!(reg.snapshot()[0].session.as_deref(), Some("sess-b"));
    // …and the tab stayed contaminated across it, so there was no second
    // transition to record.
    assert!(reg.snapshot()[0].view.contaminated);
    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(
        hits.len(),
        1,
        "the sticky bit transitioned once, so exactly one row may exist"
    );
    assert_eq!(
        payload(hits[0])["session"],
        "sess-a",
        "the row names the conversation contamination STARTED in"
    );
    assert_eq!(payload(hits[0])["host"], "a.example");
}

/// The two paths produce ONE shape of row, because they share
/// [`note_contamination`]. Asserted over the payload KEYS rather than by
/// eye: a second writer that drifted (a missing `session`, a different
/// `scope` spelling) would give the Timeline two shapes to understand.
#[test]
fn both_contamination_paths_write_the_same_row_shape() {
    outbound::test_rows::reset();
    let reg = LatchRegistry::default();
    let a = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&a),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://x.example/"), Some("x.example")),
        )
        .is_ok());
    let b = scope("claude-2", Some("sess-b"));
    let out = reg.beacon(Some(&b), "WebFetch", ON, BEACON_PROV);
    report_beacon(Some(&b), outbound::Origin::Http, "WebFetch", &out);

    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(hits.len(), 2);
    let keys = |r: &crate::activity::ActivityRecord| {
        let mut k: Vec<String> = payload(r)
            .as_object()
            .expect("object payload")
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    assert_eq!(keys(hits[0]), keys(hits[1]));
    for row in &hits {
        assert_eq!(row.entry.source, "contamination");
        assert_eq!(row.entry.kind, "injection_flag");
        assert!(!row.entry.root.is_empty(), "an empty root defeats the row");
        assert!(row.entry.ok);
    }
}

/// **A: false-positive resume.** The user judged the flagged content
/// harmless, so the bit goes now — and *nothing else moves*. The latch keeps
/// its position, the session keeps its id, the budget keeps its spend.
///
/// Asserting those three is the point rather than padding: "clear the
/// contamination flag" is a one-line change to a boolean, and the tempting
/// wrong version of it is `*entry = TabLatch::fresh()`, which would pass any
/// test that only looked at `contaminated`.
#[test]
fn a_false_positive_resume_clears_the_bit_and_touches_nothing_else() {
    let (reg, s) = contaminated_registry();
    // Spend the budget down to its limit so a reset would be visible.
    reg.charge(Some(&s), 100_000);
    assert_eq!(
        reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__search"),
        Err(outbound::REFUSAL_BUDGET)
    );

    let out = reg
        .apply_override(&s, LatchOverride::ClearContamination)
        .expect("a contaminated tab can be resumed");
    assert!(!out.view.contaminated, "the bit is gone");
    assert!(!out.view.can_clear, "and there is nothing left to clear");
    assert!(!out.view.awaiting_session_clear);

    let row = &reg.snapshot()[0];
    assert_eq!(
        row.session.as_deref(),
        Some("sess-a"),
        "the SESSION is untouched — a resume is not a restart"
    );
    assert_eq!(
        row.latch(),
        "external",
        "and so is the latch: it has its own two buttons"
    );
    assert_eq!(
        reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__search"),
        Err(outbound::REFUSAL_BUDGET),
        "and the fetch budget keeps its spend — a click that refilled it \
         would make the budget advisory"
    );
    // The consequence of leaving the latch alone, stated so nobody reads
    // this feature as more than it is: an EXTERNAL latch quarantines memory
    // writes on its OWN authority (`Latch::proxy_gate`), so clearing the bit
    // does not reopen persistence while the tab is still latched. Reopening
    // it is `unlatch`, which is a separate decision with a separate button.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined),
        "the LATCH still holds writes; clearing the bit is not an unlatch"
    );
    reg.apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean),
        "…and with both released, writes are clean again"
    );
}

/// **B: restore.** The user rolled files back. That cannot un-read a page,
/// so the bit **stays set** — this action only arms the wait.
///
/// The locked decision is the assertion: a build that "helpfully" cleared on
/// restore is the exact regression this test exists to catch, and it would
/// pass any test that merely checked the command succeeded.
#[test]
fn a_restore_arms_the_wait_and_clears_nothing_now() {
    // LOCAL-latched, so the quarantine assertion below is about the
    // contamination bit rather than about the latch — see
    // `contaminated_local_registry`.
    let (reg, s) = contaminated_local_registry();
    let out = reg
        .apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect("a contaminated tab can be armed");
    assert!(
        out.view.contaminated,
        "restoring FILES cannot remove injected text from a context window"
    );
    assert!(out.view.awaiting_session_clear, "it arms the one-shot");
    // And the quarantine it gates is still in force for this conversation.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined),
        "a note written after the restore is still held for review"
    );

    // Arming twice is answered, not silently repeated: a second click that
    // reported success would imply something new happened.
    let again = reg
        .apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect_err("a second arm is refused");
    assert!(again.contains("already waiting"), "{again}");
    // …and neither refusal nor repetition may clear anything.
    assert!(reg.snapshot()[0].view.contaminated);
}

/// **The critical case: the arm is what decides, not the rotation.**
///
/// Same registry, same decode-proven rotation, two tabs — one armed by a
/// user, one not. The armed tab clears; the unarmed one does not. If step 4
/// silently reverted H-2, the second half fails.
///
/// The rotation is driven **through** `harness::claude::read::LiveSessionGate` rather
/// than beside it, so a build that weakened the decode proof (H-2's own
/// guard) fails here too rather than quietly clearing on a forged file.
#[test]
fn only_an_armed_tab_clears_on_a_proved_rotation() {
    use crate::harness::claude::read::LiveSessionGate;

    for armed in [true, false] {
        outbound::test_rows::reset();
        let (reg, s) = contaminated_registry();
        if armed {
            reg.apply_override(&s, LatchOverride::AwaitSessionClear)
                .expect("arm");
        }
        let _ = outbound::test_rows::drain();

        // The tap proves the new transcript really is this tab's session:
        // a decoded record naming it, which is the ONLY thing that lets a
        // new id reach the live-session registry (H-2).
        let mut live = LiveSessionGate::default();
        live.rotated();
        assert!(!live.observed(false), "no evidence yet, no rotation");
        assert!(live.observed(true), "a decoded record IS the proof");

        // …and only now does a rotated scope reach the registry.
        let rotated = scope("claude-1", Some("sess-b"));
        let view = reg.view_for(&rotated);

        assert_eq!(
            view.contaminated, !armed,
            "armed={armed}: the ARM decides, not the rotation"
        );
        assert!(
            !view.awaiting_session_clear,
            "armed={armed}: a one-shot fires once"
        );
        let rows = outbound::test_rows::drain();
        assert_eq!(
            cleared_rows(&rows).len(),
            usize::from(armed),
            "armed={armed}: a clear writes exactly one row, a non-clear none"
        );

        // The consequence, not the boolean: whether the next memory write is
        // held for review.
        assert_eq!(
            reg.gate(
                Some(&rotated),
                LatchRoute::Native,
                "context_note",
                ON,
                NO_CONTENT
            ),
            if armed {
                Ok(WriteTaint::Clean)
            } else {
                Ok(WriteTaint::Quarantined)
            },
            "armed={armed}"
        );
    }
}

/// **A forged rotation on an unarmed tab still clears nothing** — H-2's own
/// case, re-run against step 4's code rather than against the code H-2 left.
///
/// Two forgeries, because they fail at two different bars:
///
/// 1. `type nul` / `echo {}` — the transcript yields no record naming the
///    session, so `LiveSessionGate` never confirms and no new id ever
///    reaches the registry at all.
/// 2. `echo '{"sessionId":"…"}'` — the decode bar is cleared (decision 3
///    puts the model's Bash outside every cImp latch, so it always can be),
///    the rotation DOES reach `observe`… and the unarmed tab is still
///    contaminated afterwards.
///
/// The deliberate counter-case is in the test above: on an **armed** tab a
/// forged rotation does clear, and that is the design. The arm is the
/// authority — an attacker cannot click restore — so a forgery only helps in
/// the case where the user has already decided the bit should go, and its
/// worst effect is lifting it slightly earlier than their own `/clear`.
#[test]
fn a_forged_rotation_cannot_clear_an_unarmed_tab() {
    use crate::harness::claude::read::LiveSessionGate;
    let (reg, _s) = contaminated_registry();

    // Forgery 1: bytes, but no record naming this session.
    let mut live = LiveSessionGate::default();
    live.rotated();
    for _ in 0..10 {
        assert!(
            !live.observed(false),
            "newline-terminated bytes are not evidence of a harness"
        );
    }
    // So the registry is never told about `sess-forged`, and the tab keeps
    // the session it was contaminated in.
    assert_eq!(reg.snapshot()[0].session.as_deref(), Some("sess-a"));

    // Forgery 2: the attacker writes a record naming the session, clearing
    // the decode bar. The rotation reaches `observe`.
    let forged = scope("claude-1", Some("sess-forged"));
    let view = reg.view_for(&forged);
    assert_eq!(
        view.latch, "open",
        "the permissive state does reset — the fix must not freeze latches"
    );
    assert!(
        view.contaminated,
        "…and the contamination bit does not: no rotation clears an unarmed tab"
    );
    assert_eq!(
        reg.gate(
            Some(&forged),
            LatchRoute::Native,
            "context_note",
            ON,
            NO_CONTENT
        ),
        Ok(WriteTaint::Quarantined),
        "the persistence channel stays closed"
    );
    // Nor can a rotation ARM one — the only writer of the arm is a click.
    assert!(!reg.snapshot()[0].view.awaiting_session_clear);
}

/// **Clearing re-arms the transition report — proved by the consequence.**
///
/// `latch_flagged` / `beacon_flagged` are one-row-per-scope claim bits, and
/// the `contamination` row is self-limiting through `note_contamination`'s
/// `mem::replace`. Leave any of them set across a clear and a tab that gets
/// re-contaminated writes **no new row**: the feed says the tab is clean, the
/// registry says it is not, and the only trace is the quarantine rows of
/// later writes. That is the same class of bug #48 fixed for the
/// `Local`-latched beacon.
///
/// Asserted as "a re-contamination writes a new row", not as
/// `assert!(!entry.beacon_flagged)`: the boolean is the mechanism, the row is
/// the invariant, and a mechanism swapped for another one must not fail this
/// test while a lost row must.
///
/// **And both claim bits are actually SPENT first.** The obvious version of
/// this test starts from a proxied fetch, which sets neither bit — so the
/// clear's resets are no-ops and deleting them leaves the test green. (That
/// was the first draft, and reverting the resets did not turn it red. It is
/// exactly the failure mode this whole area keeps producing: a test that
/// pins the happy path's shape rather than the invariant.) So the tab here
/// is LOCAL-latched and it spends both: a beacon that contaminates without
/// moving the latch, and a refused proxied call.
#[test]
fn a_re_contamination_after_a_clear_writes_a_new_row() {
    outbound::test_rows::reset();
    let (reg, s) = contaminated_local_registry();
    let rows = outbound::test_rows::drain();
    assert_eq!(
        contamination_rows(&rows).len(),
        1,
        "the first contamination is recorded"
    );
    // Spend `beacon_flagged`: this beacon reported, so the next one in the
    // same tab-session must not.
    for _ in 0..3 {
        assert!(!reg.beacon(Some(&s), "WebSearch", ON, BEACON_PROV).report);
    }
    // Spend `latch_flagged`: the first refusal writes a row, later ones do
    // not — that bound is what makes leaving the bit set invisible.
    for i in 0..3 {
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED)
        );
        let rows = outbound::test_rows::drain();
        let refusals = outbound::test_rows::of_screen(&rows, outbound::Screen::LatchRefusal);
        assert_eq!(refusals.len(), usize::from(i == 0), "refusal {i}");
    }

    reg.apply_override(&s, LatchOverride::ClearContamination)
        .expect("resume");
    // (No `contamination_cleared` row is expected from the registry here:
    // the resume's row is composed by `override_row` and written by
    // `apply_latch_override`, the IPC entry point, exactly as the two latch
    // moves' rows always have been. The same is true of the unlatch's
    // release row (decision 15's 2026-08-10 amendment), which `unlatch_clear_row`
    // composes for that same entry point — so the `Unlatch` below likewise
    // adds nothing to this feed. Both are asserted in
    // `every_clear_records_its_basis_and_the_state_it_replaced`.)
    assert!(cleared_rows(&outbound::test_rows::drain()).is_empty());

    // 1. The harness reads a page again. The conversation was clean a moment
    //    ago, so this is a NEW transition and must be reported as one — both
    //    as a contamination row and as the beacon's own row.
    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert!(out.contaminated_now, "the tab is contaminated again");
    assert!(
        out.report,
        "a beacon after a clear is a new fact — a stale `beacon_flagged` makes \
         the whole event silent, which is the #48 bug one clear later"
    );
    report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &out);
    let rows = outbound::test_rows::drain();
    assert_eq!(
        contamination_rows(&rows).len(),
        1,
        "the re-contamination writes its own transition row"
    );
    assert_eq!(
        outbound::test_rows::of_screen(&rows, outbound::Screen::LatchBeacon).len(),
        1,
        "…and the beacon row beside it"
    );

    // 2. The next refusal in the re-contaminated tab is likewise a fact the
    //    feed has not carried since the clear.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
        Err(REFUSAL_EXTERNAL_BLOCKED)
    );
    let rows = outbound::test_rows::drain();
    assert_eq!(
        outbound::test_rows::of_screen(&rows, outbound::Screen::LatchRefusal).len(),
        1,
        "a refusal after a clear must be reportable again"
    );

    // 3. And the proxied intake path, which flips the bit through a
    //    different door, reports its own re-contamination too.
    reg.apply_override(&s, LatchOverride::ClearContamination)
        .expect("resume again");
    let _ = outbound::test_rows::drain();
    assert_eq!(
        reg.gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil2.example/p"), Some("evil2.example")),
        ),
        Err(REFUSAL_EXTERNAL_BLOCKED),
        "the LOCAL latch still refuses it — the clear is not an unlatch"
    );
    reg.apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    let _ = outbound::test_rows::drain();
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil2.example/p"), Some("evil2.example")),
        )
        .is_ok());
    let rows = outbound::test_rows::drain();
    let hits = contamination_rows(&rows);
    assert_eq!(hits.len(), 1, "the proxied path reports it too");
    assert_eq!(payload(hits[0])["host"], "evil2.example");
}

/// **Decision 10 is not touched by any of this.** Clearing the tab bit stops
/// FUTURE writes being held; notes already quarantined stay quarantined, and
/// promote-or-discard remains the Memory view's own review — a separate
/// consent surface with a separate click.
///
/// Two halves, because the interesting failure is a well-meaning one:
/// someone wiring "and release this tab's held notes" into the clear.
#[test]
fn clearing_the_bit_does_not_promote_anything_already_quarantined() {
    // LOCAL-latched: the bit is what decides here, not the latch.
    let (reg, s) = contaminated_local_registry();
    // A note written while contaminated is held for review.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Quarantined)
    );
    reg.apply_override(&s, LatchOverride::ClearContamination)
        .expect("resume");
    // Only the NEXT write changes.
    assert_eq!(
        reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
        Ok(WriteTaint::Clean),
        "future writes are stored clean again — that is the whole effect"
    );

    // And the structural half: nothing on the clear path can reach a stored
    // note. The note store's release/delete API is named here so that wiring
    // it into this module fails the build's own test rather than a review.
    // `concat!` throughout: a needle written whole would match its own text
    // in the file it scans.
    // V42 R2 (#114): this module was one file when the scan was written, so it
    // read one. Every file the split produced is read, or the needle could
    // simply move next door. V42 R4 (#115) split the routes themselves, so
    // the route surface arrives as [`ROUTE_SOURCES`] rather than as a row.
    for (file, src) in ROUTE_SOURCES.iter().copied().chain([
        ("offload/discovery.rs", include_str!("../../discovery.rs")),
        ("offload/latch.rs", include_str!("../../latch.rs")),
    ]) {
        for promotion in [
            concat!("mem_", "promote_note"),
            concat!("mem_", "delete_note"),
            concat!("mem_", "quarantined_notes"),
        ] {
            assert!(
                !src.contains(promotion),
                "`{promotion}` appeared in {file} — promoting a quarantined note is \
                 the Memory view's own review (locked decision 10), not a side effect of \
                 clearing a tab's contamination flag"
            );
        }
    }
}

/// **The audit row: basis, prior state, and who acted** — for both clears,
/// because they are the same state change reached two ways and a reviewer
/// must be able to tell them apart.
#[test]
fn every_clear_records_its_basis_and_the_state_it_replaced() {
    // Half 1: the immediate resume. Origin `ipc` — a human, right now.
    outbound::test_rows::reset();
    let (reg, s) = contaminated_registry();
    let out = reg
        .apply_override(&s, LatchOverride::ClearContamination)
        .expect("resume");
    let row = override_row(
        outbound::Origin::Ipc,
        LatchOverride::ClearContamination,
        &out,
    );
    assert_eq!(
        row.screen,
        outbound::Screen::ContaminationCleared,
        "a clear is filed beside the row that SET the bit, not among latch moves"
    );
    assert_eq!(row.tool, "clear_contamination");
    assert_eq!(row.origin, outbound::Origin::Ipc);
    let d = &row.detail;
    assert!(d.contains("basis: clear_contamination"), "{d}");
    assert!(d.contains("origin: ipc"), "{d}");
    assert!(d.contains("contaminated=true"), "the PRIOR state: {d}");
    assert!(d.contains("latch=external"), "the PRIOR latch: {d}");
    assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
    assert!(d.contains("STAY quarantined"), "decision 10 stated: {d}");

    // Half 2: the armed rotation. The row is written by the registry itself
    // (nothing else observes the rotation), so it is asserted through the
    // feed rather than through a builder.
    outbound::test_rows::reset();
    let (reg, s) = contaminated_registry();
    reg.apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect("arm");
    let armrows = outbound::test_rows::drain();
    let arm = outbound::test_rows::of_screen(&armrows, outbound::Screen::LatchOverride);
    assert_eq!(
        arm.len(),
        0,
        "the arm row is written by the IPC entry point"
    );
    assert!(
        cleared_rows(&armrows).is_empty(),
        "arming clears nothing, so it writes no clear row"
    );

    let rotated = scope("claude-1", Some("sess-b"));
    assert!(!reg.view_for(&rotated).contaminated);
    let rows = outbound::test_rows::drain();
    let hits = cleared_rows(&rows);
    assert_eq!(hits.len(), 1, "the armed clear writes exactly one row");
    let hit = hits[0];
    assert_eq!(hit.entry.tool, "session_clear_observed");
    assert_eq!(hit.entry.root, TEST_ROOT, "an empty root defeats the row");
    assert!(hit.entry.ok, "nothing was denied");
    let req = payload(hit);
    assert_eq!(req["screen"], "contamination_cleared");
    assert_eq!(
        req["origin"], "internal",
        "the trigger is cImp's own observation; `ipc` means a human acted NOW"
    );
    assert_eq!(req["scope"], "claude:claude-1");
    assert_eq!(
        req["session"], "sess-a",
        "filed under the CONTAMINATED conversation, so it joins the row that opened it"
    );
    let d = &hit.response;
    assert!(d.contains("basis: session_clear_observed"), "{d}");
    assert!(d.contains("ONE-SHOT"), "{d}");
    assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
    assert!(d.contains("(sess-b)"), "and the one that replaced it: {d}");
    assert!(d.contains("latch=external"), "the PRIOR latch: {d}");

    // Half 3: the full unlatch (decision 15's 2026-08-10 amendment). Same
    // shape as half 1 — asserted through the builder, because this row is
    // likewise composed for `apply_latch_override` to file — and it must be
    // a DIFFERENT basis from the resume: "that content was harmless" and "I
    // am taking the whole risk knowingly" are different claims, and a
    // reviewer who cannot tell them apart cannot reconstruct the decision.
    outbound::test_rows::reset();
    let (reg, s) = contaminated_registry();
    let out = reg
        .apply_override(&s, LatchOverride::Unlatch)
        .expect("unlatch");
    let row = unlatch_clear_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &s, &out)
        .expect("the release owes its own row");
    assert_eq!(row.origin, outbound::Origin::Ipc, "a human acted NOW");
    assert_eq!(row.basis.tool(), "unlatch");
    assert_ne!(row.basis, ClearBasis::Resume);
    assert_eq!(row.scope, "claude:claude-1");
    assert_eq!(row.session.as_deref(), Some("sess-a"));
    let d = &row.detail;
    assert!(d.contains("basis: unlatch"), "{d}");
    assert!(d.contains("origin: ipc"), "{d}");
    assert!(d.contains("contaminated=true"), "the PRIOR state: {d}");
    assert!(d.contains("latch=external"), "the PRIOR latch: {d}");
    assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
    assert!(d.contains("STAY quarantined"), "decision 10 stated: {d}");
    assert!(
        d.contains("moved the latch to `open`"),
        "the one sentence the three bases cannot share: {d}"
    );
}

/// **The arm's own row.** It is not a clear, so it is filed as a latch
/// override — and it has to say, in words, that the flag is still set, or a
/// reader who sees "restore" and no later `contamination_cleared` row cannot
/// tell "still waiting" from "lost".
#[test]
fn the_restore_arm_writes_a_row_that_says_the_flag_is_still_set() {
    let (reg, s) = contaminated_registry();
    let out = reg
        .apply_override(&s, LatchOverride::AwaitSessionClear)
        .expect("arm");
    let row = override_row(
        outbound::Origin::Ipc,
        LatchOverride::AwaitSessionClear,
        &out,
    );
    assert_eq!(row.screen, outbound::Screen::LatchOverride);
    assert_eq!(row.tool, "await_session_clear");
    let d = &row.detail;
    assert!(d.contains("NOT cleared"), "{d}");
    assert!(d.contains("contaminated=true"), "{d}");
    assert!(d.contains("`/clear`"), "the user is told what to do: {d}");
}
