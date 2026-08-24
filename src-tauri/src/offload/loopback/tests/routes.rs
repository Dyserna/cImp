//! The route surface, read as source. The containment table below is the
//! declaration every loopback route is held to, and the scanners that read it
//! are what make a gate deleted from a handler a test failure rather than a
//! review miss.

use super::*;

/// The module named by a top-level `mod NAME;` declaration, whatever
/// visibility it wears.
///
/// V42 review, RV-7. [`the_source_scanners_read_every_route_file`] scraped
/// `mod.rs` with a bare `strip_prefix("mod ")`, so a family file declared
/// `pub(crate) mod x;` was invisible to it — and invisible on BOTH sides of
/// the join it feeds: such a file would be missing from the scrape AND from
/// `ROUTE_SOURCES`, the two shortened lists would agree, and the one test
/// whose job is to notice an unscanned route file would be green about exactly
/// that.
fn mod_name(line: &str) -> Option<&str> {
    past_visibility(line)
        .strip_prefix("mod ")?
        .strip_suffix(';')
}

/// The control for [`files_containing`] (V42 review, RV-9), permanent rather
/// than a plant-and-revert: the inputs are synthetic, so this asserts the
/// property directly instead of asserting that today's production text happens
/// to have it.
#[test]
fn files_containing_reads_code_and_not_prose() {
    let needle = "fn hook_exec_roots(app: &AppHandle";
    let commented = format!("// {needle}, settings: &S) -> Vec<PathBuf>\nfn other() {{}}\n");
    let real = format!("{needle}, settings: &S) -> Vec<PathBuf> {{\n}}\n");

    // The control's own premise: the RAW text does contain the needle, which
    // is precisely what the pre-RV-9 `src.contains(needle)` matched. If this
    // ever stops holding, the assertion below is passing on nothing.
    assert!(
        commented.contains(needle),
        "the commented fixture must still contain the needle as raw text"
    );

    assert!(
        files_containing(&[("prose.rs", Box::leak(commented.into_boxed_str()))], needle)
            .is_empty(),
        "a doc/line comment satisfied a scan whose whole assertion is that the CODE does it"
    );
    assert_eq!(
        files_containing(&[("real.rs", Box::leak(real.into_boxed_str()))], needle),
        vec!["real.rs"],
        "the scan stopped seeing a real declaration"
    );

    // …and the deliberate scope limit: a needle that IS a literal must still
    // be found, because the header scan below looks for a match arm.
    const ARM: &str = "match h { \"x-cimp-tab\" => 1, _ => 0 };\n";
    assert_eq!(
        files_containing(&[("arm.rs", ARM)], "\"x-cimp-tab\" =>"),
        vec!["arm.rs"],
        "blanking string literals would make the header scan match nothing at all"
    );
}

/// The `pub const NAME: &str = "<path>";` a plugin source declares for
/// `path`, by name.
///
/// The join that lets the containment enumeration check a plugin's route
/// table without restating its constants: the table is written in terms of
/// the constants, the enumeration in terms of the paths, and this is what
/// pins the two spellings equal.
fn route_const_named(src: &str, path: &str) -> Option<String> {
    let needle = format!(": &str = \"{path}\";");
    src.lines().find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix("pub const ")?;
        let (name, tail) = rest.split_once(':')?;
        (format!(":{tail}") == needle).then(|| name.trim().to_string())
    })
}

/// **The scanners see every file the routes are in.**
///
/// [`ROUTE_SOURCES`] is what every source-scanning test below reads, and it is
/// hand-kept — so the way it goes wrong is a family file added to `mod.rs` and
/// not to the list: the new routes' handlers would then be scanned by nobody,
/// with every test green. Joined here at the two real sources, the `mod`
/// declarations and the list itself, in both directions.
#[test]
fn the_source_scanners_read_every_route_file() {
    // V42 review RV-7: the scrape reads THROUGH the visibility modifier. It
    // used to be `strip_prefix("mod ")`, which sees `mod x;` and nothing else
    // — and the file it could not see would be missing from `ROUTE_SOURCES`
    // too, so the join below would compare two equally-short lists and pass.
    // These are the spellings a family file can legitimately be declared with;
    // each must be seen.
    for spelling in [
        "mod probe;",
        "pub mod probe;",
        "pub(crate) mod probe;",
        "pub(super) mod probe;",
        "pub(in crate::offload) mod probe;",
    ] {
        assert_eq!(
            mod_name(spelling),
            Some("probe"),
            "`{spelling}` is invisible to the scrape — a route file declared that way \
             would be scanned by nobody with this test green"
        );
    }
    // …and it stays a TOP-LEVEL declaration scrape: prose, a nested `mod`, an
    // inline module and a lookalike identifier are all not one.
    for not_a_route_file in [
        "// mod probe;",
        "    mod probe;",
        "mod probe {",
        "use foo::mod_probe;",
    ] {
        assert_eq!(
            mod_name(not_a_route_file),
            None,
            "`{not_a_route_file}` was read as a route-file declaration"
        );
    }

    let dispatch = include_str!("../mod.rs");
    let mut declared: Vec<String> = dispatch
        .lines()
        .filter_map(mod_name)
        .filter(|m| *m != "tests")
        .map(|m| format!("offload/loopback/{m}.rs"))
        .collect();
    // Vacuity guard: an empty scrape would make the comparison below trivially
    // satisfiable by an empty list, which is the failure this test is about.
    assert!(
        declared.len() > 5,
        "the `mod` scrape found {declared:?} — it is not seeing the declarations"
    );
    declared.push("offload/loopback/mod.rs".to_string());
    declared.sort();

    let mut listed: Vec<String> = ROUTE_SOURCES.iter().map(|(f, _)| f.to_string()).collect();
    listed.sort();
    assert_eq!(
        listed, declared,
        "a route file is declared but unscanned (or the reverse) — every source \
         scan below would silently stop covering it"
    );

    // …and no two rows are the same text: a list of twelve copies of one
    // `include_str!` would satisfy the join above and scan one file twelve
    // times.
    for (a, (file, src)) in ROUTE_SOURCES.iter().enumerate() {
        for (other, second) in ROUTE_SOURCES.iter().skip(a + 1) {
            assert_ne!(
                src, second,
                "{file} and {other} scan the same text — one of the rows names the \
                 wrong file"
            );
        }
    }
}

/// Whether `path` is served by a plugin rather than by core's own `match`.
fn is_plugin_route(path: &str) -> bool {
    crate::harness::registry::all()
        .filter_map(|h| h.plugin())
        .any(|p| p.routes().iter().any(|r| r.path == path))
}

/// **M-7's third clause.** Every route the listener serves declares what it
/// does about the taint latch, and the declaration is checked against the
/// handler rather than believed.
///
/// The four checks, and what each one catches:
///
/// 1. Every dispatched path is declared, and every declared path is
///    dispatched — so a new route cannot slip in unclassified.
/// 2. A route that claims to gate must actually reach `latches()`. **This
///    is the check that would have failed before this commit** for the
///    three `/context/*` hooks, and it is what stops the classic failure of
///    a gate tested through its helper while the call site is deleted.
/// 3. A route that claims NOT to touch the registry must not — so a gate
///    added without a review of what it means to that route also fails.
/// 4. A fixed-tool route names a real class-table row, uses that constant
///    in its own body, and the declared "refused under EXTERNAL" answer is
///    computed from [`toolclass`], not restated. Demoting
///    `hook_post_edit` to TRUSTED therefore fails here.
#[test]
fn every_loopback_route_declares_what_it_does_about_the_latch() {
    // V42 R4 (#115): the dispatch `match` is core's and stays in
    // `loopback/mod.rs`, but the handlers it names are spread across the
    // family files — so the two halves of this test read different things:
    // the ARM from the dispatch, the BODY from whichever family declares it.
    let dispatch = include_str!("../mod.rs");

    // 1. Surface ↔ declaration, both directions.
    let mut declared: Vec<&str> = ROUTE_CONTAINMENT.iter().map(|r| r.path).collect();
    declared.sort_unstable();
    assert_eq!(
        dispatched_routes(ROUTE_SOURCES),
        declared,
        "a route is dispatched but undeclared (or the reverse)"
    );

    for row in ROUTE_CONTAINMENT {
        // The declared handler really is the one the route is served by.
        // For core's own arms that is the dispatch `match`; for a plugin
        // route it is the `route!` entry in the plugin's table, resolved
        // through the path constant so the two spellings cannot part.
        if is_plugin_route(row.path) {
            let konst = route_const_named(HOOK_SRC, row.path).unwrap_or_else(|| {
                panic!("{} is served by a plugin but named by no constant", row.path)
            });
            assert!(
                HOOK_SRC.contains(&format!(
                    "route!(\"{}\", {konst}, {})",
                    row.method, row.handler
                )),
                "{} does not register `{}` in the plugin's route table",
                row.path,
                row.handler
            );
        } else {
            let arm = format!("(\"{}\", \"{}\") =>", row.method, row.path);
            let arm_at = dispatch
                .find(&arm)
                .unwrap_or_else(|| panic!("no dispatch arm for {}", row.path));
            if !row.handler.is_empty() {
                assert!(
                    dispatch[arm_at..].starts_with(&format!("{arm} {}(", row.handler)),
                    "{} does not dispatch to `{}`",
                    row.path,
                    row.handler
                );
            }
        }
        // The two inline arms have no handler to scan; nothing behind them
        // can gate, which is why they are the only rows allowed to omit one.
        if row.handler.is_empty() {
            assert!(
                matches!(row.containment, Containment::NoRegistry(_)),
                "{} is answered inline, so it cannot be gating anything",
                row.path
            );
            continue;
        }
        let body = handler_body(row.handler);
        // V40 Phase C: a plugin route reaches the registry through the
        // narrow facades (`hook_gate_admits`, `latch_beacon_for`), because
        // `LatchRegistry` is private to this module and a harness may not
        // hold it. Same funnels, one indirection further out.
        let reaches_registry = body.contains("latches()")
            || body.contains("hook_gate_admits(")
            || body.contains("latch_beacon_for(");
        let gates = body.contains("latches().gate(")
            // V40 Phase C: a plugin route cannot hold `LatchRegistry` (it is
            // private to this module), so its gate call is the narrow
            // facade. Same funnel, same decision, same ledger.
            || body.contains("if !hook_gate_admits(")
            || body.contains("hook_admit(\n        latches(),")
            || body.contains("audit_admit(\n        latches(),")
            // V39 Phase B: `/delegate`'s own admit funnel, same shape and same
            // reason as the two above — the decision is a function so it can be
            // unit-tested without a `TcpStream`, which means the handler body
            // names the funnel rather than `latches().gate(`.
            || body.contains("delegate_admit(\n        latches(),");

        match row.containment {
            Containment::GatesRequestTool => assert!(
                gates,
                "{} claims to gate but its handler never reaches the latch registry",
                row.path
            ),
            Containment::GatesFixedTool {
                tool,
                refused_under_external,
            } => {
                assert!(
                    gates,
                    "{} claims to gate but its handler never reaches the latch registry",
                    row.path
                );
                assert!(
                    body.contains(tool_const(tool)),
                    "{} must gate on `{tool}`'s constant in its own body",
                    row.path
                );
                // The security-relevant property, computed rather than
                // restated: is a contaminated conversation refused here?
                assert_eq!(
                    Latch::External.blocks(toolclass::classify(tool)),
                    refused_under_external,
                    "`{tool}`'s class no longer matches what {} declares",
                    row.path
                );
            }
            Containment::RegistryNoGate(why) => {
                assert!(
                    reaches_registry,
                    "{} claims to reach the registry ({why}) and does not",
                    row.path
                );
                assert!(
                    !gates,
                    "{} now gates capability — declare it, don't leave it as a state read",
                    row.path
                );
            }
            Containment::NoRegistry(why) => assert!(
                !reaches_registry,
                "{} is declared ungated ({why}) but now reaches the latch registry",
                row.path
            ),
        }
    }
}

/// The identifier a hook tool-name constant is written as at the call site.
/// The handler bodies use the CONSTANT, not the string, so the check above
/// has to look for the same thing a reader would.
fn tool_const(tool: &str) -> &'static str {
    match tool {
        "delegate_task" => "DELEGATE_TOOL",
        "hook_post_edit" => "HOOK_TOOL_POST_EDIT",
        "hook_should_read" => "HOOK_TOOL_SHOULD_READ",
        "hook_compaction" => "HOOK_TOOL_COMPACTION",
        other => panic!("no constant known for `{other}`"),
    }
}
