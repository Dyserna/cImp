//! `offload::loopback`'s unit tests — the module's own `#[cfg(test)] mod tests`,
//! split by subject (#132, test-placement wave). V42 R1 (#114) moved them out of
//! `loopback.rs` verbatim into one 9,117-line file; this splits that file, and
//! nothing else: no test added, removed, reordered within its cluster, or edited.
//!
//! Many of these are **source-scanning** tests: they `include_str!` the
//! production file(s) and assert on the text, so that a gate deleted from a
//! handler fails a test rather than a review. Since V42 R4 (#115) the route
//! surface is a DIRECTORY, so they read a list of files — [`ROUTE_SOURCES`],
//! declared beside the dispatch in `mod.rs` — rather than one `include_str!`.
//! A scan that kept reading the file a handler used to be in would be green
//! about code it no longer covers, which for a security assertion is the same
//! thing as being deleted. Every such path is one directory deeper now.
//!
//! This file holds what more than one cluster needs; a fixture only [`discovery`]
//! or only [`latch`] uses lives in that file, where its subject is.

mod wire;
mod discovery;
mod bodies;
mod latch;
mod identity;
mod contamination;
mod routes;
mod hooks;

use super::*;

// V42 R2/R3 (#114) moved the discovery and taint-latch halves of this module
// to `offload::discovery` and `offload::latch`. What follows is what only the
// TESTS reach for, imported here so each production `use` stays exactly as
// wide as production needs.
use crate::offload::latch::{
    override_row, unlatch_clear_row, ClearBasis, LatchOverride, TabLatch,
};

use crate::offload::toolclass::{Latch, ToolClass, WriteTaint};

use crate::offload::discovery::{
    dispatch_discovery_report, report_skipped_to_app, resolve_external_root, responds,
    select_verified, ChildIdentity, DISCOVERY_REPORT_TIMEOUT, DISCOVERY_SKIPPED_PATH,
};

use crate::harness::claude::hook as claude_hook;

/// V40 Phase C moved the ingress here; the source-scanning tests that read
/// a handler's body follow it.
const HOOK_SRC: &str = include_str!("../../../harness/claude/hook.rs");

/// **Every file a loopback route handler can live in**, paired with its source.
///
/// The route surface ([`super::ROUTE_SOURCES`], one row per family file since
/// V42 R4 (#115)) plus the Claude plugin's ingress, which V40 Phase C moved
/// twelve handlers into. The source-scanning tests below take a LIST rather
/// than a file, so a handler that moves between families keeps its scanners
/// instead of quietly losing them: a scan that kept reading `mod.rs` after the
/// routes moved out would be green about code it no longer covers, and for a
/// security assertion green-about-nothing is the failure mode.
fn route_surface() -> Vec<(&'static str, &'static str)> {
    let mut files = ROUTE_SOURCES.to_vec();
    files.push(("harness/claude/hook.rs", HOOK_SRC));
    files
}

/// Whether `line` opens a top-level item whose signature is `sig`, whatever
/// visibility it wears.
///
/// V40 Phase C made moved items `pub(crate)`; V42 R4 (#115) made every item a
/// family file publishes `pub(super)`. The item is still top-level, which is
/// the property the column-0 `}` terminator depends on — so the scan reads
/// through the modifier rather than growing a case per spelling (which is how
/// a scan comes to silently match nothing).
fn declares(line: &str, sig: &str) -> bool {
    if line.starts_with(sig) {
        // A caller that spells the visibility in `sig` itself (`pub fn
        // proxy_base_for(`) is pinning THAT too, and must not be read
        // through.
        return true;
    }
    past_visibility(line).starts_with(sig)
}

/// `line` with a leading visibility modifier stripped: `pub`, `pub(crate)`,
/// `pub(super)`, `pub(in some::path)`. Returns `line` unchanged when there is
/// none.
///
/// Column 0 is not negotiable and nothing is trimmed: everything these scans
/// assert about is a TOP-LEVEL item, and the column-0 `}` terminator
/// [`fn_body`] relies on is sound only for one.
fn past_visibility(line: &str) -> &str {
    match line.strip_prefix("pub") {
        None => line,
        Some(tail) => tail
            .strip_prefix('(')
            .and_then(|t| t.split_once(") "))
            .map(|(_scope, after)| after)
            .or_else(|| tail.strip_prefix(' '))
            .unwrap_or(tail),
    }
}

/// [`fn_body`] over a LIST of files: the one that declares `sig` is found
/// first, and its body scanned.
///
/// **Exactly one file may declare it.** Two copies of a handler is itself the
/// failure these scans exist to catch, and a scan that read whichever came
/// first would be pinning one of them while the other ran.
fn fn_body_in(files: &[(&'static str, &'static str)], sig: &str) -> String {
    let named: Vec<&'static str> = files
        .iter()
        .filter(|(_, src)| src.lines().any(|l| declares(l, sig)))
        .map(|(file, _)| *file)
        .collect();
    assert_eq!(
        named.len(),
        1,
        "`{sig}` is declared in {named:?} — exactly one file in the route surface must"
    );
    let src = files
        .iter()
        .find(|(file, _)| *file == named[0])
        .expect("the file just found")
        .1;
    fn_body(src, sig)
}

/// The files in `files` whose **code** contains `needle`, for the scans whose
/// assertion is about PRESENCE somewhere in the surface rather than about one
/// item's body.
///
/// V42 review, RV-9. This searched the raw source, so a doc comment naming the
/// signature or the header was enough to satisfy it — and both call sites are
/// security assertions ("the exec roots derive from the app, never from a
/// request body"; "the tab-identity headers are actually matched"). A scan a
/// comment can satisfy is a scan that keeps passing after the code it names is
/// deleted.
///
/// [`crate::rustsrc::uncommented`], not `code_of`: one of the two needles IS a
/// string literal (`"x-cimp-tab" =>`, a match arm on a header name), and the
/// strong pass blanks it — which would have replaced "a comment can satisfy
/// this" with "nothing can", the same vacuity wearing the opposite face.
fn files_containing(files: &[(&'static str, &'static str)], needle: &str) -> Vec<&'static str> {
    files
        .iter()
        .filter(|(rel, src)| crate::rustsrc::uncommented(rel, src).contains(needle))
        .map(|(file, _)| *file)
        .collect()
}

/// V32 Phase G: the default posture — both feature switches on. Every
/// pre-Phase-G latch test asserted this implicitly, so it is the value they
/// keep asserting; the switched-off behaviour has its own tests below.
const ON: GatePolicy = GatePolicy {
    latch: true,
    quarantine: true,
};

/// The provenance a NATIVE route states (#48, F-3): cImp's own dispatch,
/// no fetched page in view. What every pre-F-3 `gate` test was implicitly
/// asserting, since none of them was an intake.
const NO_CONTENT: CallProvenance<'static> = CallProvenance::internal();

/// The provenance the `/latch/beacon` route states — always `Http`.
const BEACON_PROV: CallProvenance<'static> = CallProvenance::http();

/// The project root the test scopes claim. A real scope's root is resolved
/// from the tab's settings entry (`tab_root_key`); the tests care only that
/// it is carried through to the row, so one fixed value keeps the
/// assertions readable.
const TEST_ROOT: &str = "P:\\proj";

/// A project root or child cwd, spelled the way the host platform spells
/// one: `P:\<tail>` on Windows — byte-identical to the literals these
/// fixtures used to hard-code — and `/<tail>` everywhere else.
///
/// **Why this cannot be a hard-coded literal.** Discovery routing is
/// component-wise ([`is_ancestor_or_equal`]), and `Path::new(r"P:\proj\b")`
/// is a SINGLE `Component::Normal` on Linux. Every root/hint pair below
/// would therefore stop matching, step 1 of [`select_answering`] would rank
/// nothing, and the tests would either panic on `.expect("match")` or —
/// worse — pass through the sole-entry fallback while asserting nothing
/// about the ranking they were written for. The properties being pinned
/// here (F-11, F-26, F-28, decision 30's probe budget) are not
/// Windows-specific, so their coverage must not be either.
fn proj(tail: &str) -> String {
    if cfg!(windows) {
        format!(r"P:\{}", tail.replace('/', "\\"))
    } else {
        format!("/{tail}")
    }
}

/// [`proj`] as a `PathBuf`, for the cwd/hint side of the same pairs.
fn proj_path(tail: &str) -> PathBuf {
    PathBuf::from(proj(tail))
}

// ── V32 Phase B — the proxy's per-session taint latch ──────────────────

use crate::offload::toolclass::{
    REFUSAL_EXTERNAL_BLOCKED, REFUSAL_EXTERNAL_USER_LOCAL, REFUSAL_LOCAL_BLOCKED,
};

/// A scope for `tab`, claiming session `session` (`None` = the registry
/// withheld one). `claude` unless the test says otherwise.
fn scope(tab: &str, session: Option<&str>) -> LatchScope {
    LatchScope {
        agent: "claude",
        tab: tab.to_string(),
        session: session.map(str::to_string),
        root: TEST_ROOT.to_string(),
    }
}

/// Drive [`audit_admit`] against `reg` with a fixed served root, an
/// `exposed` verdict and a pre-resolved scoping — the same three
/// dependencies the handler supplies from `AuditState` / `latch_scope` /
/// `GatePolicy::resolve`.
fn admit(
    reg: &LatchRegistry,
    body: &AuditRunBody,
    exposed: bool,
    scoping: LatchScoping,
) -> Result<&'static str, String> {
    audit_admit(
        reg,
        body,
        Path::new("P:\\proj"),
        |_| exposed,
        |_, _| scoping,
        |_| ON,
    )
}

// ── V32 Phase C — the proxy's per-session EXTERNAL fetch budget ─────────

const TEST_LIMITS: outbound::BudgetLimits = outbound::BudgetLimits {
    max_calls: 3,
    max_bytes: 1000,
};

// ── #45 — the registry's bound, and the audit row's provenance ──────────

/// Settings carrying `ids` as AI tabs, plus one reserved Shell tab (which
/// hosts no harness and must therefore never be a valid latch scope).
fn settings_with_tabs(ids: &[&str]) -> crate::settings::Settings {
    settings_with_consumer_tabs(&ids.iter().map(|id| ("claude", *id)).collect::<Vec<_>>())
}

/// V33 C5: settings carrying `(consumer, id)` AI tabs — `"claude"` builds a
/// Claude tab, anything else an OpenCode one — plus the same reserved Shell
/// tab. The consumer of a tab is its COMMAND (`tabs::tab_consumer`), so
/// these are built from the real defaults rather than by stamping a field.
fn settings_with_consumer_tabs(tabs_in: &[(&str, &str)]) -> crate::settings::Settings {
    use crate::settings::{default_ai_tab, default_graph_monitor_tab, TabConfig};
    let mut tabs = vec![default_graph_monitor_tab()];
    for (consumer, id) in tabs_in {
        let mut t = crate::settings::ai_tab_inheriting_injection(default_ai_tab(
            if *consumer == "claude" {
                crate::settings::ai_tab_id("claude")
            } else {
                crate::settings::ai_tab_id("opencode")
            },
        ));
        if let TabConfig::AiTool(c) = &mut t {
            c.id = (*id).to_string();
        }
        tabs.push(t);
    }
    crate::settings::Settings {
        tabs,
        ..Default::default()
    }
}

// ── #48 finding F-3 — the contamination TRANSITION row ─────────────────
//
// Every case below asserts on the rows `record_flag` actually received
// (`outbound::test_rows`), not on the registry's own return values. That is
// deliberate: `BeaconOutcome::contaminated_now` and `LatchStatus.contaminated`
// were already true before this work and F-3 was still open, because
// "the bit flipped" and "something recorded that the bit flipped" are
// different facts and only the second one survives the call.

/// Contamination rows in the order they were written.
fn contamination_rows(
    rows: &[crate::activity::ActivityRecord],
) -> Vec<&crate::activity::ActivityRecord> {
    outbound::test_rows::of_screen(rows, outbound::Screen::Contamination)
}

// ── Step 4 — the two user-driven contamination clears ──────────────────
//
// The governing risk in this area, three findings running: the code is
// right against the proof-of-concept and wrong against the invariant, and
// the test pins the PoC's shape. So the cases below are written against the
// *observable consequence* wherever one exists — a re-contamination row
// rather than a boolean, a `WriteTaint` rather than a bit — and the two
// that guard H-2 assert what must NOT happen on a tab nobody armed.

/// A contaminated, EXTERNAL-latched tab in session `sess-a`, with a page
/// already fetched (so its budget carries real spend) and both
/// one-row-per-scope report bits used up.
fn contaminated_registry() -> (LatchRegistry, LatchScope) {
    let reg = LatchRegistry::default();
    let s = scope("claude-1", Some("sess-a"));
    assert!(reg
        .gate(
            Some(&s),
            LatchRoute::Proxied,
            "ddg__fetch_content",
            ON,
            CallProvenance::intake(Some("https://evil.example/p"), Some("evil.example")),
        )
        .is_ok());
    assert!(reg.snapshot()[0].view.contaminated);
    (reg, s)
}

/// A contaminated tab whose latch is **LOCAL** — the #48 beacon case: the
/// tab used a local tool first, then the harness's own `WebFetch` reported
/// in, which contaminates the conversation without moving the latch.
///
/// It exists because of a seam that is easy to test past. Under an EXTERNAL
/// latch a `context_note` is quarantined by the **latch**
/// (`Latch::proxy_gate`), whatever the contamination bit says — so a test
/// that cleared the bit on an EXTERNAL-latched tab and asserted the write
/// was still held would be asserting the latch's behaviour and calling it
/// the bit's. On a LOCAL-latched tab the bit is the only thing deciding, so
/// every assertion about what clearing changes is made here.
fn contaminated_local_registry() -> (LatchRegistry, LatchScope) {
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
    let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
    assert!(!out.engaged, "the latch stays LOCAL");
    assert_eq!(out.view.latch, "local");
    assert!(out.view.contaminated);
    (reg, s)
}

/// The rows the clear wrote, in order.
fn cleared_rows(
    rows: &[crate::activity::ActivityRecord],
) -> Vec<&crate::activity::ActivityRecord> {
    outbound::test_rows::of_screen(rows, outbound::Screen::ContaminationCleared)
}

// ── #48, finding M-7 — the route enumeration, by containment property ──
//
// The finding's third clause was that the three `/context/*` hook routes
// "appear in no route enumeration". They did appear in the pinned path list
// below — what they appeared in was an enumeration of STRINGS, which cannot
// tell a route that gates from a route that walks straight into
// `GraphService`. So the enumeration now records, per route, what it does
// about the taint latch, and the test checks that claim against the
// handler's own source rather than restating it.

/// What one HTTP route does about the V32 session taint latch.
#[derive(Debug)]
enum Containment {
    /// Gates, on a tool name the REQUEST supplies (`/run`, `/graph_run`,
    /// `/audit/run`, `/mcp/call`). Which class the call lands in is the
    /// caller's tool's business; that the registry is consulted at all is
    /// this route's.
    GatesRequestTool,
    /// Gates, on a FIXED [`toolclass::TABLE`] name — the three hook routes.
    /// `refused_under_external` states the security-relevant consequence:
    /// whether a conversation that has ingested untrusted content is
    /// REFUSED here. It is checked against `toolclass`, not restated, so a
    /// demotion of the row shows up as a failure of this test.
    GatesFixedTool {
        tool: &'static str,
        refused_under_external: bool,
    },
    /// Touches the registry for something that is not a capability gate —
    /// a state read, or the beacon (which can only ever tighten). The
    /// string says which.
    RegistryNoGate(&'static str),
    /// Never consults the registry. The string is the reason, and it is the
    /// claim a reviewer has to disagree with in order to add a route here.
    NoRegistry(&'static str),
}

struct RouteRow {
    path: &'static str,
    method: &'static str,
    /// The handler function the dispatch table routes to. `""` for the two
    /// routes answered inline in the dispatch arm itself.
    handler: &'static str,
    containment: Containment,
}

const fn route(
    path: &'static str,
    method: &'static str,
    handler: &'static str,
    containment: Containment,
) -> RouteRow {
    RouteRow {
        path,
        method,
        handler,
        containment,
    }
}

/// **Every route this listener serves, and what it does about containment.**
///
/// This is the single enumeration: `no_http_route_can_reach_a_contamination_clear`
/// pins the SURFACE from it and
/// `every_loopback_route_declares_what_it_does_about_the_latch` pins the
/// PROPERTY. A new route therefore cannot be added by editing one list.
const ROUTE_CONTAINMENT: &[RouteRow] = &[
    route("/run", "POST", "handle_run", Containment::GatesRequestTool),
    route(
        "/graph_run",
        "POST",
        "handle_graph_run",
        Containment::GatesRequestTool,
    ),
    route(
        "/audit/run",
        "POST",
        "handle_audit_run",
        Containment::GatesRequestTool,
    ),
    // RECORDED RESIDUAL, not a clean case. The auto-injection channel: its
    // V32 containment is the spotlighting / memory-quarantine envelope
    // (locked decisions 10/12), not refusal, and it fires on every prompt
    // rather than on a model's election. But its digest carries exported
    // SIGNATURES — the same content H-1 demoted `graph_repo_map` for — so
    // "ungated" here is a standing question, not a settled one. M-7 named
    // three routes and this is not one of them; it is written down so the
    // next reviewer inherits the question instead of rediscovering it.
    route(
        "/context/retrieve",
        "POST",
        "handle_context_retrieve",
        Containment::NoRegistry(
            "auto-injection; contained by the spotlight/quarantine envelope, not by refusal",
        ),
    ),
    // V33 Phase F. Takes a Workbench checkpoint before a mutating tool
    // call. Ungated on purpose, and the reason is not "nobody got round to
    // it": it returns two booleans, grants no capability and hands back no
    // project data, so a latch would have nothing to refuse — while a
    // refusal would remove checkpoints from a tab that had touched external
    // content, i.e. exactly the tab most likely to need a rewind. It is
    // still token-gated, tab-narrowed, throttled per `(root, tab)` and
    // re-checked against the class table for `mutates_fs`.
    route(
        "/workbench/tool_checkpoint",
        "POST",
        "handle_tool_checkpoint",
        Containment::NoRegistry(
            "snapshots the work tree into cImp's own shadow repo; returns no local data \
             and grants no capability",
        ),
    ),
    route(
        "/context/compaction",
        "POST",
        "handle_context_compaction",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_COMPACTION,
            // TRUSTED: paths, symbol names and memory-note text, no source
            // text. Gated so the class table stays the one place this can
            // change.
            refused_under_external: false,
        },
    ),
    route(
        "/context/should_read",
        "POST",
        "handle_should_read",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_SHOULD_READ,
            refused_under_external: true,
        },
    ),
    route(
        "/context/post_edit",
        "POST",
        "handle_post_edit",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_POST_EDIT,
            refused_under_external: true,
        },
    ),
    route(
        "/memory/event",
        "POST",
        "handle_memory_event",
        // Ingress, not egress: it records the caller's OWN tool/usage events
        // and returns no project data. Nothing to refuse — a latch here
        // would only lose the record of what the tab did.
        Containment::NoRegistry("records the caller's own events; returns no local data"),
    ),
    route(
        "/activity/contract_drift",
        "POST",
        "handle_contract_drift",
        Containment::NoRegistry("a shim reporting its own broken payload; returns nothing"),
    ),
    route(
        // The constant the CHILD sends, so the two ends cannot drift: the
        // dispatch arm is scanned from the source and compared against this
        // list, which is built from `DISCOVERY_SKIPPED_PATH`.
        DISCOVERY_SKIPPED_PATH,
        "POST",
        "handle_discovery_skipped",
        Containment::NoRegistry(
            "a child reporting a discovery entry it skipped; answers a fixed `ok` on every \
             path and touches no registry",
        ),
    ),
    route(
        "/permission/event",
        "POST",
        "handle_permission_event",
        Containment::NoRegistry("a hook reporting a permission prompt; returns nothing"),
    ),
    route(
        "/latch/beacon",
        "POST",
        "handle_latch_beacon",
        Containment::RegistryNoGate(
            "engages the EXTERNAL latch for a harness-native web tool; it can only tighten",
        ),
    ),
    route(
        "/latch/state",
        "POST",
        "handle_latch_state",
        Containment::RegistryNoGate(
            "reads this tab's view for the plugin gate; creates nothing",
        ),
    ),
    // V35 Phase I (CHP). A generated harness artifact declaring its protocol
    // version and what it will serve. Ungated on purpose and the reason is
    // not "nobody got round to it": it hands back no project data and grants
    // no capability, so a latch would have nothing to refuse — while a
    // refusal would deny cImp the one message that says a tab is running a
    // STALE plugin, i.e. exactly the tab whose containment behaviour is in
    // question. It is still token-gated, and its tab id is validated against
    // the user's configured AI tabs before anything is recorded.
    route(
        "/session/hello",
        "POST",
        "handle_session_hello",
        Containment::NoRegistry(
            "a harness artifact declaring its protocol version and capabilities; returns no \
             local data and grants nothing",
        ),
    ),
    // ── V35 Phase J: the Claude-native ingress ───────────────────────────
    //
    // Six routes carrying Claude Code's own hook payloads, replacing five
    // shim binaries. Each declares the SAME containment as the CHP route it
    // shares a core with, and the test below checks that against the
    // handler's own source — so the two transports cannot come to gate
    // differently, which is the only way this phase could have introduced a
    // containment hole.
    route(
        "/claude/hook/user_prompt_submit",
        "POST",
        "handle_claude_user_prompt_submit",
        // Same recorded residual as `/context/retrieve`, whose core this
        // shares: the auto-injection channel is contained by the
        // spotlight/quarantine envelope, not by refusal.
        Containment::NoRegistry(
            "auto-injection; contained by the spotlight/quarantine envelope, not by refusal",
        ),
    ),
    route(
        "/claude/hook/pre_compact",
        "POST",
        "handle_claude_pre_compact",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_COMPACTION,
            refused_under_external: false,
        },
    ),
    route(
        "/claude/hook/pre_tool_use",
        "POST",
        "handle_claude_pre_tool_use",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_SHOULD_READ,
            refused_under_external: true,
        },
    ),
    route(
        "/claude/hook/post_tool_use",
        "POST",
        "handle_claude_post_tool_use",
        Containment::GatesFixedTool {
            tool: HOOK_TOOL_POST_EDIT,
            refused_under_external: true,
        },
    ),
    route(
        "/claude/hook/notification",
        "POST",
        "handle_claude_notification",
        Containment::NoRegistry("a hook reporting a permission prompt; returns nothing"),
    ),
    route(
        "/claude/hook/session_start",
        "POST",
        "handle_claude_session_start",
        // The Claude twin of `/session/hello`, and ungated for its reasons:
        // it hands back no project data and grants no capability, while a
        // refusal would deny cImp the one message that says a tab is running
        // a STALE overlay.
        Containment::NoRegistry(
            "a harness artifact declaring its protocol version and capabilities; returns no \
             local data and grants nothing",
        ),
    ),
    // ── V35 Phase L: the read path, pushed ───────────────────────────────
    //
    // Six routes, one containment answer: none of them consults the latch,
    // and the reason is the same for all six and is worth stating rather
    // than assuming. Each one moves data FROM the harness INTO cImp and
    // hands nothing back — no project content, no tool, no capability. The
    // taint latch exists to stop a conversation that has ingested untrusted
    // content from reaching cImp's own capabilities; a route that only
    // receives has none to reach.
    //
    // The one that came closest to needing a gate is `assistant_text`,
    // because its effect is audible: a compromised artifact can make cImp
    // SAY something. That is a social-engineering surface, not code
    // execution (design § 5.2), and it is contained by three things that are
    // not the latch — the per-tab `tts_injection.enabled` toggle, the
    // app-side escape-sequence strip, and app-side segmentation of prose the
    // sender cannot control.
    route(
        "/session/assistant_text",
        "POST",
        "handle_session_assistant_text",
        Containment::NoRegistry(
            "receives assistant prose for TTS; returns no local data and grants no capability \
             — contained by the per-tab TTS toggle and app-side escape stripping, not by the \
             latch",
        ),
    ),
    // ── V39 Phase B: cross-harness delegation ───────────────────────────
    //
    // **Closed, not declared.** The first cut of this phase left the route
    // ungated and said so here, because the generated `delegate_task_<id>`
    // names cannot be classified by a static table and a gate keyed on them
    // would have been a silent no-op. That framing was the mistake: the
    // name the GATE needs was never the one the model types. The child
    // resolves the harness id and forwards it; the route states its own
    // class-table identity (`DELEGATE_TOOL`), exactly as the three
    // `/context/*` hooks do — which makes this a fixed-tool route, and the
    // per-harness naming a non-problem.
    //
    // It gates as LOCAL-CAPABILITY, so a contaminated conversation is
    // refused here for the same reason V32 C-1c refuses it `offload_task`:
    // both hand a task to a fresh, permissive executor, and this one's is a
    // whole peer harness with its own tools. "The user asked for it" does
    // not launder the request — the task text is model-authored.
    route(
        "/delegate",
        "POST",
        "handle_delegate",
        Containment::GatesFixedTool {
            tool: DELEGATE_TOOL,
            // Computed, not restated: LOCAL-CAPABILITY under an EXTERNAL
            // latch is blocked, so a demotion of the row fails this test.
            refused_under_external: true,
        },
    ),
    route(
        "/session/tool_result",
        "POST",
        "handle_session_tool_result",
        Containment::NoRegistry("receives one tool result's SIZE; returns nothing"),
    ),
    route(
        "/session/output_started",
        "POST",
        "handle_harness_output",
        Containment::NoRegistry(
            "receives a turn boundary; returns nothing and grants no capability — the tab \
             must have declared the event in its hello for it to reach the avatar at all",
        ),
    ),
    route(
        "/session/output_stopped",
        "POST",
        "handle_harness_output",
        Containment::NoRegistry(
            "receives a turn boundary; returns nothing and grants no capability — the tab \
             must have declared the event in its hello for it to reach the avatar at all",
        ),
    ),
    route(
        "/session/subagents_active",
        "POST",
        "handle_subagents_active",
        Containment::NoRegistry("receives a sub-agent COUNT edge; returns nothing"),
    ),
    route(
        "/session/subagent",
        "POST",
        "handle_session_subagent",
        Containment::NoRegistry("receives a sub-agent lifecycle edge; returns nothing"),
    ),
    route(
        "/claude/hook/stop",
        "POST",
        "handle_claude_stop",
        Containment::NoRegistry(
            "the Claude ingress for /session/assistant_text; same core, same answer",
        ),
    ),
    route(
        "/claude/hook/post_tool_use_result",
        "POST",
        "handle_claude_tool_result",
        Containment::NoRegistry(
            "the Claude ingress for /session/tool_result; sizes a result, runs nothing",
        ),
    ),
    route(
        "/claude/hook/subagent",
        "POST",
        "handle_claude_subagent",
        Containment::NoRegistry(
            "the Claude ingress for /session/subagent; same core, same answer",
        ),
    ),
    route(
        "/claude/hook/post_tool_use_failure",
        "POST",
        "handle_claude_tool_failure",
        Containment::NoRegistry(
            "the errored half of /session/tool_result; sizes an error string, runs nothing",
        ),
    ),
    // ── 2026-08-17: the two migrated beacons ─────────────────────────────
    //
    // Each declares the SAME containment as the harness-neutral route it
    // shares a core with, and the test below checks that against the
    // handler's own source — so the two transports cannot come to gate
    // differently, which is the only way a migration like this could
    // introduce a containment hole.
    route(
        "/claude/hook/pre_tool_use_taint",
        "POST",
        "handle_claude_taint_beacon",
        Containment::RegistryNoGate(
            "engages the EXTERNAL latch for a harness-native web tool; it can only tighten",
        ),
    ),
    route(
        "/claude/hook/pre_tool_use_checkpoint",
        "POST",
        "handle_claude_checkpoint",
        // `/workbench/tool_checkpoint`'s answer, for its reasons: it
        // snapshots the work tree into cImp's own shadow repo, returns no
        // local data and grants no capability, so a latch would have nothing
        // to refuse — while a refusal would remove checkpoints from the tab
        // most likely to need a rewind.
        Containment::NoRegistry(
            "snapshots the work tree into cImp's own shadow repo; returns no local data \
             and grants no capability",
        ),
    ),
    route(
        "/mcp/list",
        "POST",
        "handle_mcp_list",
        // Advertisement only. Consumers cache `tools/list` at connect, which
        // is exactly why the proxy enforces by REFUSAL at `/mcp/call`
        // instead of by removing defs here (decision 3).
        Containment::NoRegistry("tool advertisement; enforcement is by refusal at /mcp/call"),
    ),
    route(
        "/mcp/call",
        "POST",
        "handle_mcp_call",
        Containment::GatesRequestTool,
    ),
    route(
        "/describe",
        "GET",
        "",
        Containment::NoRegistry("the proxy's own tool list as text; no project data"),
    ),
    route(
        "/events",
        "GET",
        "handle_events",
        Containment::NoRegistry("the offload service's own event stream"),
    ),
    route(
        "/health",
        "GET",
        "",
        Containment::NoRegistry("a fixed `ok`"),
    ),
    route(
        "/status",
        "GET",
        "handle_status",
        // Reads latch state through `latch_snapshot`, not through the
        // registry handle — a debug view over cImp's own identifiers, with
        // no capability behind it.
        Containment::NoRegistry("a debug view of cImp's own identifiers"),
    ),
];

/// Every path the dispatch table routes, sorted and deduped — scanned from
/// the source because the `match` is not reachable from a test.
///
/// V42 R4 (#115): every file of the route surface is scanned, not just the
/// one the `match` is in. Core's arms all live in `loopback/mod.rs` today,
/// and a route arm is not something a family file may grow — but a scan that
/// looked in one place could not tell the difference between "there are none"
/// and "they moved", and this list is what the containment enumeration is
/// checked against in both directions.
fn dispatched_routes(files: &[(&'static str, &'static str)]) -> Vec<&'static str> {
    let mut routes: Vec<&'static str> = Vec::new();
    for marker in ["(\"POST\", \"", "(\"GET\", \""] {
        for (_file, src) in files {
            for part in src.split(marker).skip(1) {
                routes.push(part.split('"').next().expect("a closing quote"));
            }
        }
    }
    // V40 Phase C, locked decision 15: core's `match` is no longer the whole
    // surface — every registered plugin's `routes()` is appended after it.
    // A route this listener serves is a route this listener serves,
    // whichever file declares it, so the containment enumeration has to see
    // all of them or a plugin could add an ungated door by declaring it.
    for h in crate::harness::registry::all() {
        let Some(p) = h.plugin() else { continue };
        routes.extend(p.routes().iter().map(|r| r.path));
    }
    routes.sort_unstable();
    routes.dedup();
    routes
}

/// The source text of one top-level `async fn`, signature to closing brace,
/// with line endings normalised to `\n` (this file is checked out CRLF on
/// Windows, and a needle written with `\n` would silently match nothing —
/// which for a security assertion means silently passing).
///
/// Starts at the SIGNATURE, so a handler's doc comment is deliberately not
/// part of it: a route must not be able to claim a gate in prose.
fn handler_body(name: &str) -> String {
    // V40 Phase C: the twelve `handle_claude_*` bodies and the legacy
    // `--notify-hook` route live in `harness/claude/hook.rs` now, and V42 R4
    // (#115) spread the rest across `loopback/*.rs`. These scans are about
    // the PROPERTY (one core per capability, the gate in the route's own
    // body), which neither move changed — so the scanner takes the whole
    // surface and follows the code, rather than the tests being deleted with
    // it or left reading the file it used to be in.
    fn_body_in(&route_surface(), &format!("async fn {name}("))
}

/// [`handler_body`] for any top-level item, given its exact opening text —
/// so a non-`async` helper (or a shared core, V35 Phase J) can be scanned by
/// the same rules.
///
/// V42 review (dropped-at-cap): there used to be a second, byte-identical
/// copy of this called `top_level_fn`, described as "the non-`async` twin".
/// It never was one — the signature is a parameter, so `async` is just part
/// of the text — and two copies of a scanner primitive is one copy that can be
/// hardened while the other quietly is not. There is one.
fn fn_body(src: &str, sig: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in src.lines() {
        if !inside {
            // V40 Phase C: a moved item is often `pub(crate)` now, because
            // its one remaining caller is the plugin that used to sit
            // beside it; V42 R4 (#115) made a family file's items
            // `pub(super)`. The item is still top-level, which is the
            // property the column-0 `}` terminator depends on — see
            // [`declares`].
            if !declares(line, sig) {
                continue;
            }
            inside = true;
        }
        out.push_str(line);
        out.push('\n');
        // The closing brace of a top-level item is the only `}` in column 0.
        if line == "}" {
            break;
        }
    }
    assert!(!out.is_empty(), "no top-level `{sig}`");
    assert!(
        out.ends_with("}\n"),
        "`{sig}` was not terminated — the scan would read past it"
    );
    out
}
