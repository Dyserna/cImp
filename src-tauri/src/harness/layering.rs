//! V35 Phase K — **the layering, as tests** (design § 4.1).
//!
//! A layering that exists only in a design document rots. Phase K moved the
//! harness surface into one directory; the three § 4.1 tests here
//! ([`no_harness_literals_outside_harness`],
//! [`harness_modules_do_not_import_capabilities`],
//! [`every_registry_entry_is_fully_wired`]) are what stop the next
//! feature from putting it back. The fourth test named in § 4.1,
//! `wired_in_paths_exist`, already lives with the registry it checks
//! ([`crate::harness::contract`]) and is what forced this phase to update every
//! `wired_in` path in the same commit.
//!
//! Three more tests guard the *guards*, because a source scanner that reads the
//! wrong slice of a file reports on nothing and says `ok` while doing it:
//! [`the_literal_scan_reads_the_same_code_on_every_platform`],
//! [`every_literal_allowlist_entry_is_still_earning_it`] and
//! [`executable_text_ignores_line_endings_and_cuts_at_every_test_item`]. All
//! three were added after the first two tests shipped a defect each — the story
//! is on [`executable_text`], and it is worth reading before touching this file.
//!
//! The tree-reading tests use the repo's existing source-scanning idiom:
//! `CARGO_MANIFEST_DIR` is `<repo>/src-tauri`, so `src/` is one join away and no
//! path is hard-coded relative to a working directory.
//!
//! # What "harness-owned" means here
//!
//! A harness-owned string is a name **cImp did not choose** — a field in a
//! payload Claude Code emits, a key in a settings file OpenCode reads, a phrase
//! in a TUI cImp scrapes. Every one of them is a dependency on something
//! upstream, which is precisely what [`crate::harness::contract`] enumerates.
//! So the needle list is *derived from the registry* rather than hand-kept:
//! adding a `Dep::JsonPath` automatically widens what the scan will refuse to
//! see outside `harness/`, which is the two-sources-of-truth discipline the rest
//! of this milestone is built on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::contract::{self, Dep, Harness};

/// `<repo>/src-tauri/src`.
fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, as `(repo-relative-ish path with forward
/// slashes, contents)`. Paths are rooted at `src/` so the allowlists below read
/// the way a person would write them.
fn source_files() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                if let Ok(text) = std::fs::read_to_string(&p) {
                    out.push((rel, text));
                }
            }
        }
    }
    let root = src_root();
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out.sort();
    out
}

/// Drop every `#[cfg(test)]` item and every comment line.
///
/// **Tests are deliberately out of scope.** A fixture that quotes a harness
/// payload is a *recorded input*, not a dependency on one — the Phase B canary
/// corpus is made of nothing else, and an assertion that Claude's overlay
/// carries a `statusLine` key has to spell `statusLine` to be an assertion at
/// all. What this scan is about is production code that *reads or writes* an
/// upstream name; that is the thing which must sit in `harness/` so a rename
/// upstream is a diff in one directory.
///
/// Comments go for the same reason: prose naming `rate_limits` is
/// documentation, and documentation that explains the seam is wanted
/// everywhere, not confined.
///
/// # Why this delegates instead of finding a boundary itself
///
/// It used to cut at `text.match_indices("\n#[cfg(test)]\n").last()`, and that
/// was wrong in two independent ways — both of which shipped, and one of which
/// only ever fired off this developer's machine:
///
///  1. **It was line-ending-sensitive.** `\r\n#[cfg(test)]\r\n` does not match,
///     so a CRLF checkout found no boundary at all and fell back to scanning the
///     WHOLE file, tests included. Every `.rs` file in this repo is LF *in the
///     index*, but `core.autocrlf` is on by default on Windows, so the CI
///     runner's checkout is CRLF while a working copy whose files were rewritten
///     in place is a mix. The v0.52.0-rc.1 Tests run is the record: byte-identical
///     content, `no_harness_literals_outside_harness` green on the Linux job and
///     red on the Windows job with 26 hits across four files, every one inside a
///     `mod tests`. A verification test whose coverage depends on how Git checked
///     the file out reports on the checkout, not on the code.
///  2. **`.last()` is not "the trailing test module".** `#[cfg(test)]` marks
///     test-only *items*, of which a file may have many: `graph/mcp.rs` has
///     eleven test modules, so the cut landed at the eleventh and left the first
///     ten (~1800 lines) inside the scan. Worse in the other direction, a
///     `#[cfg(test)] mod tests;` **declaration** is the last such item in its
///     file — so `processing/mod.rs` was cut at line 47 of ~500 and
///     `harness/mod.rs` at line 99, hiding the production code both tests exist
///     to read. Silent under-coverage, which is how a canary goes vacuous.
///
/// Neither is fixable by a smarter single cut: `offload/mcp.rs` has production
/// code (`proxy_graph_outcome`) *between* two test modules, so no one boundary
/// separates test from production text. What is needed is every
/// `#[cfg(test)]` item's span, brace-matched, with strings and comments blanked
/// first so a `"#[cfg(test)]"` inside a literal is not mistaken for one — which
/// is exactly what [`crate::rustsrc`] already did for the spawn ledger, controls
/// and all. So this normalizes line endings, asks for the spans, and removes
/// them.
///
/// What that deliberately still keeps in scope: `#[cfg(test)]`-gated *helpers*
/// are removed along with the modules (they are test-only either way), while a
/// plain `fn` used only by tests but not gated is production text and is
/// scanned. That is the right side to err on — the gate is the declaration.
fn executable_text(rel: &str, text: &str) -> String {
    // FIRST, before any offset is taken: Windows and Linux must scan
    // byte-identical bytes, so the local run is authoritative for CI.
    let norm = text.replace('\r', "");
    let code = crate::rustsrc::code_of(rel, &norm);
    let mut kept = String::with_capacity(norm.len());
    let mut at = 0usize;
    // Sorted by start; a nested `#[cfg(test)]` inside a test module yields a
    // span already covered, hence the `max`.
    for (start, end) in crate::rustsrc::test_regions(&code) {
        let (start, end) = (start.min(norm.len()), end.min(norm.len()));
        if start > at {
            kept.push_str(&norm[at..start]);
        }
        at = at.max(end);
    }
    kept.push_str(&norm[at..]);
    kept.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Path segments too common to be evidence of harness knowledge on their own.
///
/// Every one is a word cImp uses for its own concepts (`session_id` is on every
/// CHP body cImp defines; `prompt`, `message`, `type` and `cwd` are the app's
/// own vocabulary), so requiring them inside `harness/` would flag the whole
/// codebase and teach the reader to ignore this test — the exact fate of the
/// version tripwire this milestone exists to fix.
const GENERIC: &[&str] = &[
    "type", "id", "name", "text", "message", "content", "cwd", "prompt", "model", "version",
    "session_id", "timeout", "headers", "hooks", "http", "usage", "agent", "task", "properties",
    "info", "part", "role", "time", "session", "delta", "field", "trigger", "command", "url",
    // An ordinary English word cImp uses for its own toasts and TTS cues long
    // before Claude Code had a `Notification` hook.
    "notification",
];

/// A registry [`Dep`] string, reduced to the tokens worth scanning for.
///
/// Both the whole dotted path (some readers match it literally — an SSE event
/// type is `message.part.delta`, not three separate lookups) and its individual
/// segments (a JSONL reader indexes `["message"]["usage"]["input_tokens"]`).
fn tokens_of(dep_str: &str) -> Vec<String> {
    let mut out = Vec::new();
    if dep_str.contains('.') && !dep_str.contains(' ') {
        out.push(dep_str.replace("[]", ""));
    }
    for seg in dep_str.split(['.', '|', '=', ' ', '{', '}', ',', '"']) {
        let seg = seg.trim().trim_end_matches("[]");
        if seg.is_empty() || GENERIC.contains(&seg) {
            continue;
        }
        // Distinctive enough to be a name somebody upstream chose: long, or
        // snake_cased, or camelCased.
        let distinctive = seg.len() >= 8
            || seg.contains('_')
            || seg.chars().skip(1).any(|c| c.is_ascii_uppercase());
        if distinctive {
            out.push(seg.to_string());
        }
    }
    out
}

/// The literals no production file outside `harness/` may contain.
///
/// Sourced from the registry ([`Dep::JsonPath`] / [`Dep::ConfigKey`] /
/// [`Dep::Route`]) plus a short explicit list for the things a registry row
/// cannot carry.
///
/// **[`Dep::Flag`] is deliberately excluded, and this is a known gap.** Claude's
/// session-selection flags (`--session-id`, `--resume`, `--continue`,
/// `--fork-session`, `--from-pr`) are still read in
/// `tabs::config::{resolve_oob_source, args_select_session}`, which is where the
/// V34 per-tab session pinning decides what a tab's reader may assume. Moving
/// that is a behaviour-bearing change to tab launch, not a relocation, so Phase
/// K left it and recorded it here rather than widening the allowlist until the
/// test said nothing.
fn harness_literals() -> BTreeSet<String> {
    let mut needles: BTreeSet<String> = BTreeSet::new();
    for c in contract::capabilities() {
        for d in c.depends_on {
            let raw = match d {
                Dep::JsonPath(p) | Dep::ConfigKey(p) => *p,
                Dep::Route(r) => {
                    // `GET /event` → `/event`: the path is the name upstream owns.
                    if let Some(path) = r.split_whitespace().nth(1) {
                        needles.insert(path.to_string());
                    }
                    continue;
                }
                _ => continue,
            };
            needles.extend(tokens_of(raw));
        }
    }
    // Things no `Dep` can express. The TUI footer is Tier D by definition — a
    // phrase cImp scrapes off a screen, with no payload to point a `JsonPath`
    // at (capability `perm.tui_scrape`).
    needles.insert("Esc to cancel · Tab to amend".to_string());
    needles
}

/// Production files outside `harness/` that may still name a harness-owned
/// literal, each with the reason and the phase that retires it.
///
/// This list is the point of the test. Before Phase K the answer was "wherever";
/// now it is these five files, on purpose, and a sixth needs a line here and a
/// reviewer.
///
/// **Checked in both directions**, like [`UPWARD_EXEMPT`] and for the same
/// reason: [`every_literal_allowlist_entry_is_still_earning_it`] requires each
/// path to exist and to still contain a needle, so an entry cannot outlive the
/// literal it was written for and quietly become a blanket exemption for
/// whatever that file grows into next.
const LITERAL_ALLOWLIST: &[(&str, &str)] = &[
    // `offload/loopback.rs` was the first entry here, and V40 Phase C deleted
    // it. Its reason was "L2 by design (§ 4: 'route table; handlers stay in
    // offload/loopback.rs'), and `classify_permission_event` reads Claude's
    // `hook_event_name` values because it is the receiving end of the wire".
    // The design sentence it quoted is exactly what locked decisions 15 and 22
    // overturned: the route table AND the handlers are the plugin's, so the
    // receiving end of the wire is `harness/claude/hook.rs` and the classifier
    // went with it. This file holds no harness payload field at all now, which
    // is strictly better than an exemption — a future one fails the build.
    // TWO MORE entries were deleted here on 2026-08-17, and this time by the
    // follow-up their reasons named. `taint_beacon.rs` and
    // `checkpoint_beacon.rs` were Claude's last two command-hook shims, exempted
    // because they read `tool_name` off a Claude payload from outside
    // `harness/claude/` — they were separate process entry points. Their reasons
    // ended "retire with the beacons themselves, or fold their payload reading
    // into `harness::claude::hook`", and the http migration did both at once: the
    // payload reading is `claude_hook::contract_checks`, the files are gone, and
    // the scan is two files wider with nothing to allow.
    // `processing/patterns_file.rs` was the second entry, for "the Claude TUI
    // permission footer (capability `perm.tui_scrape`, Tier D)" — the literal
    // `Esc to cancel · Tab to amend` in its per-release snapshot table, plus the
    // `aider_*` rows of a harness retired in V19. V40 Phase C moved every row to
    // the harness that shipped it (`harness/<id>/prompts.rs`, and data-only
    // `harness/_retired/aider.rs`); what is left is the era list and the
    // composition, which name no harness at all. The fallback detector is
    // unchanged and still the fallback — only the transcription moved.
    // TWO entries were deleted here once the allowlist gained its own
    // both-directions check (`every_literal_allowlist_entry_is_still_earning_it`),
    // and in both cases the reason had been describing code that does not exist:
    //
    //  * `processing/permission.rs` was exempted for "the same footer" as
    //    `patterns_file.rs`. It does not contain it. `Esc to cancel · Tab to amend`
    //    appears in that file only in its module docs, in `//` comments explaining
    //    the wrapped/padded variants, and in test fixtures — the production
    //    matcher is fed patterns from `patterns_file.rs` and quotes no footer of
    //    its own. One capability row, ONE file.
    //  * `usage/mod.rs` was exempted for reusing the upstream spelling
    //    (`five_hour`, `resets_at`, `context_window_size`) in cImp's own on-disk
    //    format. That description of the code is accurate and still worth knowing
    //    — but serde field names are IDENTIFIERS, and this scan only ever matched
    //    quoted literals, so those fields were never hits. What was hitting was
    //    the JSON in its doc comment and its test fixtures.
    //
    // Both files are now inside the scan and clean, which is strictly better than
    // an exemption: a future Claude-payload read in either one fails the build.
    // `offload/toolclass.rs` was listed here for Claude's capitalized natives
    // (`Edit`/`Write`/`Bash`/`MultiEdit`) in cImp's routed `TABLE`, kept there
    // because V33's `mutates_fs` consumer resolved a tool name through it. The
    // reason ended "folding Claude's into [OpenCode's table] would recreate the
    // two-vocabularies-one-lookup bug both tables exist to prevent" — which was
    // true of that move and is not what V40 Phase A did. Claude's rows went to
    // `harness/claude/tools.rs`, a THIRD table for a third vocabulary, and the one
    // lookup that must not cross them (`harness::native`) resolves the harness
    // from the request instead of picking a table by hand. `TABLE` now holds only
    // names cImp routes, which is what its unknown-⇒-EXTERNAL law is about.
    (
        "graph/index.rs",
        "A COLLISION, not a dependency — and one worth stating rather than silencing. `\"tool_result\"` \
         here is cImp's OWN discriminator in the usage-event table (`kind` column: `\"turn\"` vs \
         `\"tool_result\"`), chosen long before V35 and readable by cImp alone. It became a needle \
         in V35 Phase L, when `claude.hook.tool_result` declared the Claude payload field of the \
         same name, so the scan now sees two unrelated uses of one word. Renaming the column would \
         be a graph migration to fix a test's vocabulary; renaming the Claude field is not cImp's \
         to do. The exemption is the honest third option — and it is narrow: `graph/index.rs` \
         reads no harness payload at all.",
    ),
    // `graph/memory.rs` was listed here from Phase K as a FINDING rather than a
    // clean exemption: `classify_tool` matched BOTH harnesses' edit-tool ids
    // (`Edit`/`Write`/`MultiEdit`/`NotebookEdit`/`edit`/`write`/`patch`) inline to
    // classify a memory event, and the reason said it "should ask
    // `toolclass`/`harness::opencode::tools` instead — but the two vocabularies
    // answer differently for `edit` vs `Edit`, so rerouting it is a behaviour
    // decision, not a relocation."
    //
    // V40 Phase A took that decision (locked decision 16). The classification is a
    // `memory_kind` column on each plugin's own native-tool table, core reads it
    // through `harness::native::memory_kind` with the request's SOURCE, and a
    // source cImp cannot identify records nothing instead of borrowing a
    // vocabulary. The file names no harness id at all now, so it is inside the
    // scan rather than exempt from it.
];

/// **No harness-owned string outside `harness/`** (design § 4.1).
///
/// Before Phase K these literals were confined to `oob/`, `statusline/` and
/// `tabs/config.rs` by habit alone. This converts the habit into an invariant:
/// a new feature that reads a Claude payload field in `graph/` or `workbench/`
/// fails the build instead of quietly creating the next Tier-C dependency
/// nobody wrote down.
/// The scan itself, over whatever `(path, text)` pairs it is handed.
///
/// Extracted from the test so [`the_literal_scan_reads_the_same_code_on_every_platform`]
/// can feed it the same tree with the other line ending — the failure this
/// separation exists to catch was invisible to a test that could only ever see
/// one checkout.
fn literal_offenders(files: &[(String, String)]) -> BTreeMap<String, BTreeSet<String>> {
    let needles = harness_literals();
    assert!(
        needles.len() > 30,
        "the needle list collapsed to {} entries — the registry-derived half stopped producing \
         tokens, which would make this test pass by finding nothing",
        needles.len()
    );
    let mut offenders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (path, text) in files {
        if path.starts_with("harness/") || path.ends_with("tests.rs") {
            continue;
        }
        if LITERAL_ALLOWLIST.iter().any(|(p, _)| p == path) {
            continue;
        }
        let body = executable_text(path, text);
        for n in &needles {
            // The whole literal, not a substring: a log line that mentions
            // `SessionStart` in prose is describing the seam, while `"tool_name"`
            // is reading across it.
            if body.contains(&format!("\"{n}\"")) {
                offenders.entry(path.clone()).or_default().insert(n.clone());
            }
        }
    }
    offenders
}

#[test]
fn no_harness_literals_outside_harness() {
    let offenders = literal_offenders(&source_files());
    assert!(
        offenders.is_empty(),
        "harness-owned literals outside `harness/` — move the code that reads them into \
         `harness/<id>/`, or add an allowlist entry saying why it cannot move:\n{offenders:#?}"
    );
}

/// **The scan's coverage does not depend on the checkout** — the regression test
/// for the bug that made the v0.52.0-rc.1 Tests run red.
///
/// `no_harness_literals_outside_harness` above is only as good as the slice of
/// each file it looks at, and for one release it looked at a *different* slice
/// depending on whether Git had written LF or CRLF: green on Linux, red on
/// Windows, identical bytes. So this asserts the property the scan needs rather
/// than the result it happens to produce — run the whole thing twice over the
/// same tree, once with every line ending flipped, and demand the same answer.
///
/// It is deliberately not an `assert!(is_empty())` duplicate: an empty-vs-empty
/// comparison would still pass if a future change made BOTH runs blind, so it
/// also re-checks that the CRLF pass reads a substantive amount of code. Both
/// halves are needed — the equality catches divergence, the substantiveness
/// catches a shared collapse.
#[test]
fn the_literal_scan_reads_the_same_code_on_every_platform() {
    let lf = source_files();
    let crlf: Vec<(String, String)> = lf
        .iter()
        .map(|(p, t)| (p.clone(), t.replace('\n', "\r\n")))
        .collect();
    assert_eq!(
        literal_offenders(&lf),
        literal_offenders(&crlf),
        "the literal scan sees different code in a CRLF checkout than in an LF one — the \
         `#[cfg(test)]` boundary is line-ending-sensitive again, and the test now reports on how \
         Git checked the tree out rather than on the tree"
    );

    // …and neither pass may be reading nothing. `tabs/config.rs` is the file the
    // CRLF bug flooded with false hits, so it is also the honest witness that the
    // CRLF pass still reaches real production code: `build_ai_tool_spec` is a
    // production fn, sits above that file's test module, and is not a needle.
    // (It replaced `resolve_oob_source`, which V40 Phase A moved behind
    // `HarnessPlugin::resolve_oob` — a witness has to be a function that is
    // still there.)
    let (_, config) = crlf
        .iter()
        .find(|(p, _)| p == "tabs/config.rs")
        .expect("tabs/config.rs is in the tree");
    let body = executable_text("tabs/config.rs", config);
    assert!(
        body.contains("fn build_ai_tool_spec"),
        "the CRLF pass lost `tabs/config.rs`'s production code — an equal-and-empty comparison \
         above would then be two blind runs agreeing"
    );
    assert!(
        !body.contains("\"PostToolUse\""),
        "the CRLF pass is still scanning `tabs/config.rs`'s test module, where ~70 tests assert \
         on Claude's hook names — that is the exact CI failure this test pins"
    );
}

/// **Every [`LITERAL_ALLOWLIST`] entry still hits** — the other direction.
///
/// [`UPWARD_EXEMPT`] has had this check since Phase K and it is what caught a
/// false exemption the moment `executable_text` stopped reading test text; the
/// literal allowlist had no equivalent, so its reasons could have rotted
/// unobserved. An entry that no longer names any harness literal is not harmless:
/// it exempts the WHOLE file from the scan, so the next feature that puts a
/// Claude payload read in `graph/index.rs` inherits a pass.
#[test]
fn every_literal_allowlist_entry_is_still_earning_it() {
    let needles = harness_literals();
    let files: BTreeMap<String, String> = source_files().into_iter().collect();
    let mut stale = Vec::new();
    for (path, _reason) in LITERAL_ALLOWLIST {
        let Some(text) = files.get(*path) else {
            stale.push(format!("{path}: no such file under src/ — the code moved or was deleted"));
            continue;
        };
        let body = executable_text(path, text);
        if !needles.iter().any(|n| body.contains(&format!("\"{n}\""))) {
            stale.push(format!(
                "{path}: names no harness literal any more, so its exemption now covers the whole \
                 file for free — delete the entry"
            ));
        }
    }
    assert!(
        stale.is_empty(),
        "LITERAL_ALLOWLIST entries that stopped earning their exemption:\n{stale:#?}"
    );
}

/// [`executable_text`]'s own unit controls, on input whose answer is written
/// down rather than inferred from the tree.
///
/// The tree-wide test above proves the property end to end; these name the two
/// specific defects, so a future regression says which one came back.
#[test]
fn executable_text_ignores_line_endings_and_cuts_at_every_test_item() {
    // Defect 1: the same source, two line endings, one answer.
    let src = "fn prod() { let a = \"keep\"; }\n#[cfg(test)]\nmod tests {\n    let b = \"drop\";\n}\n";
    let lf = executable_text("f.rs", src);
    let crlf = executable_text("f.rs", &src.replace('\n', "\r\n"));
    assert_eq!(lf, crlf, "line endings must not change what is scanned");
    assert!(lf.contains("\"keep\""));
    assert!(!lf.contains("\"drop\""), "the test module must be dropped");

    // Defect 2a: a `#[cfg(test)] mod tests;` DECLARATION ends at its semicolon —
    // it must not swallow the production code that follows it, which is how
    // `processing/mod.rs` lost ~500 lines from the scan.
    let decl = "#[cfg(test)]\nmod tests;\n\nfn prod() { let a = \"keep\"; }\n";
    let body = executable_text("f.rs", decl);
    assert!(
        body.contains("\"keep\""),
        "a `#[cfg(test)] mod x;` declaration must not truncate the file: {body:?}"
    );

    // Defect 2b: EVERY test item goes, not just the last one, and production
    // code between two of them survives — `offload/mcp.rs`'s real shape.
    let many = "#[cfg(test)]\nmod a { let x = \"drop_a\"; }\nfn mid() { let m = \"keep_mid\"; }\n\
                #[cfg(test)]\nmod b { let y = \"drop_b\"; }\n";
    let body = executable_text("f.rs", many);
    assert!(body.contains("\"keep_mid\""), "code between test modules is production");
    assert!(!body.contains("\"drop_a\""), "the FIRST test module must go too");
    assert!(!body.contains("\"drop_b\""));

    // A `#[cfg(test)]` spelt inside a string literal is not a test item.
    let quoted = "fn prod() { let s = \"#[cfg(test)]\\nmod t {\"; let a = \"keep\"; }\n";
    assert!(
        executable_text("f.rs", quoted).contains("\"keep\""),
        "a quoted `#[cfg(test)]` must not start a region"
    );
}

/// Which `harness/<id>/` directory belongs to which [`Harness`] — **a view over
/// the registry**, not a second list (V40 locked decision 1).
///
/// It used to be a hand-kept array, which meant a new harness had to be declared
/// twice and the test could only ever check the half it was told about.
fn harness_dirs() -> Vec<(&'static str, Harness)> {
    crate::harness::registry::HARNESSES
        .iter()
        .map(|d| (d.id, d.harness()))
        .collect()
}

/// **Every registry entry is fully wired** (V40 locked decision 10(b)).
///
/// One descriptor is a promise about eight places; this is the test that makes
/// forgetting one of them a red build instead of a silent hole. It absorbed
/// V35's `every_harness_dir_declares_its_capabilities`, whose two claims are the
/// first two below.
///
/// Checked **in both directions** for the directory set: a `harness/<id>/`
/// directory the registry does not declare fails just as loudly as a descriptor
/// with no directory.
#[test]
fn every_registry_entry_is_fully_wired() {
    let root = src_root().join("harness");
    let on_disk: BTreeSet<String> = std::fs::read_dir(&root)
        .expect("src/harness exists")
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // `_retired/` is data a retired harness left behind, not a harness
        // (V40 Phase C, locked decision 21). It has no plugin, no descriptor,
        // no tab and no code path — the underscore is the convention that says
        // so, and this is the one place it has to be spelled: a directory here
        // would otherwise have to be declared, and declaring it would make a
        // harness cImp cannot run look supported.
        .filter(|name| !name.starts_with('_'))
        .collect();
    let declared: BTreeSet<String> = harness_dirs().iter().map(|(d, _)| d.to_string()).collect();
    assert_eq!(
        on_disk, declared,
        "a `harness/<id>/` directory exists that the registry does not declare (or vice versa) — \
         add its `HarnessDescriptor` row so its rows, its hello, its grants, its spawn signature \
         and its health panel are all checked"
    );

    // The `MAINTENANCE.md` drift table — read as a document, so "the doc has
    // rows for this harness" is checked against the file rather than a copy.
    let maintenance = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/MAINTENANCE.md"),
    )
    .expect("docs/MAINTENANCE.md is readable");

    for d in crate::harness::registry::HARNESSES {
        let dir = d.id;
        let harness = d.harness();

        // 1. Capability rows: what cImp depends on this harness for.
        let rows: Vec<&str> = contract::capabilities()
            .filter(|c| c.harness == harness)
            .map(|c| c.id)
            .collect();
        assert!(
            !rows.is_empty(),
            "harness/{dir}/ has no rows in the capability registry — nothing records what cImp \
             depends on it for, so nothing can degrade visibly when it changes"
        );

        // 2. The hello: `serves` / `cannot`, built from the per-tab flags that
        //    decided what was actually wired (design D3).
        let has_hello = std::fs::read_dir(root.join(dir))
            .expect("harness dir readable")
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
            .any(|e| {
                std::fs::read_to_string(e.path())
                    .map(|t| t.contains("EV_HELLO"))
                    .unwrap_or(false)
            });
        assert!(
            has_hello,
            "harness/{dir}/ declares no CHP hello (`chp::EV_HELLO`) — without one, Phase I's \
             stale-artifact detection cannot cover its tabs and a capability's absence is \
             indistinguishable from nobody having written it down"
        );

        // 3. Identity: a binary to recognise it by, at least one reserved tab id,
        //    a consumer token, and a label a human reads.
        assert!(!d.binaries.is_empty(), "{dir}: no binary — nothing can classify its tabs");
        assert!(!d.tab_ids.is_empty(), "{dir}: no reserved tab id");
        assert!(!d.consumer.is_empty(), "{dir}: no MCP consumer token");
        assert!(!d.label.is_empty(), "{dir}: no human label");

        // 3b. Exactly ONE `<id>.input.profile` row, contributed by the plugin
        //     (V40 locked decision 17). The row states a Tier-D behaviour no
        //     payload reveals — "a bracketed paste plus a CR yields exactly one
        //     turn" — and the `delegation.worker` gate is built on it. A harness
        //     that declares an `InputProfile` with no row would be typed into on
        //     the strength of a contract nobody wrote down, with nothing to mark
        //     verified and nothing to degrade; two rows would mean two contracts
        //     claiming the same name, and the health panel would show whichever
        //     it iterated first.
        let profile_rows: Vec<&str> = contract::capabilities()
            .filter(|c| c.id == format!("{dir}.input.profile"))
            .map(|c| c.id)
            .collect();
        assert_eq!(
            profile_rows.len(),
            1,
            "{dir}: expected exactly one `{dir}.input.profile` capability row from its plugin's \
             `capabilities()`, got {profile_rows:?}"
        );
        assert!(
            d.plugin.input_profile().is_some(),
            "{dir}: declares an `{dir}.input.profile` contract row but no `input_profile()` — the \
             row would describe a paste encoding nothing uses"
        );

        // 4. A spawn signature. A harness with none gets NO restart hint when a
        //    spawn-baked setting changes — the exact failure the mechanism exists
        //    to prevent, and the one that used to be a missing array element.
        assert!(
            !d.plugin
                .spawn_sig(&crate::settings::Settings::default())
                .is_null(),
            "{dir}: `spawn_sig` answers null — a spawn-baked setting could change with no \
             restart hint, silently"
        );

        // 4b. Every DECLARED setting is well formed, and every harness-SCOPED
        //     injection feature names an `ext` key this harness actually
        //     declares as a `Bool` (V40 locked decision 6).
        //
        //     Both halves are load-bearing. A duplicate key would make one
        //     declaration silently unreachable — the form would render two
        //     controls writing the same slot. And a `scoped_features()` row
        //     pointing at a key nobody declared would resolve that feature's
        //     app-wide L2 to a default nothing stores: the Settings matrix
        //     would show a switch the user can flip and the launch path would
        //     never read it, which is the "declared but not enforced" shape
        //     this milestone keeps removing.
        let mut keys: BTreeSet<&str> = BTreeSet::new();
        for field in d.plugin.settings_schema() {
            assert!(
                keys.insert(field.key),
                "{dir}: `settings_schema()` declares `{}` twice — one of the two would be \
                 unreachable and the form would render both onto the same slot",
                field.key
            );
            assert!(
                !field.label.trim().is_empty(),
                "{dir}: setting `{}` has no label, so the generic form would render a \
                 nameless control",
                field.key
            );
            assert!(
                field.kind.accepts(&field.default.to_json()),
                "{dir}: setting `{}`'s declared default is not a value its declared kind \
                 accepts — the parse boundary would reset it to itself, forever",
                field.key
            );
        }
        for scoped in d.plugin.scoped_features() {
            let row = d
                .plugin
                .settings_schema()
                .iter()
                .find(|f| f.key == scoped.ext_key);
            let row = row.unwrap_or_else(|| {
                panic!(
                    "{dir}: `scoped_features()` names ext key `{}` for {:?}, which \
                     `settings_schema()` does not declare — its app-wide L2 would resolve to a \
                     default nothing stores",
                    scoped.ext_key, scoped.feature
                )
            });
            assert!(
                matches!(row.kind, crate::harness::plugin::SettingKind::Bool),
                "{dir}: `{}` backs the {:?} feature's L2 and must be a Bool row",
                scoped.ext_key,
                scoped.feature
            );
        }

        // 5. A non-empty sandbox grant table. A grant table nobody wrote is not a
        //    boundary; it is a tool that fails to start for reasons the user
        //    cannot see.
        let home = std::path::PathBuf::from("/home/tester");
        let grants = d.plugin.sandbox_grants(&crate::harness::plugin::GrantCtx {
            home: &home,
            env: &|_| None,
        });
        assert!(
            !grants.is_empty(),
            "{dir}: declares no sandbox grant rows — a sandboxed tab of this harness would be \
             confined away from its own state with no row explaining it"
        );

        // 6. A health panel row, so a user can see what is broken.
        assert!(
            super::health::panel_labels().iter().any(|(h, _)| *h == harness),
            "{dir}: no Harness health panel — a capability the user can be blocked by and \
             cannot see is what that panel exists to end"
        );

        // 7. The `MAINTENANCE.md` drift table names its rows.
        for id in &rows {
            assert!(
                maintenance.contains(id),
                "{dir}: capability `{id}` has no row in docs/MAINTENANCE.md — the human twin of \
                 the registry has to be editable in the same commit"
            );
        }

        // 8. A harness configured through a file cImp writes gets plugin
        //    goldens, so a change to that file is a reviewable diff.
        if d.features
            .contains(&crate::harness::registry::HarnessFeature::FileArtifact)
        {
            let goldens = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures")
                .join("harness")
                .join(dir)
                .join("goldens");
            assert!(
                goldens.is_dir(),
                "{dir}: declares FileArtifact but has no fixtures/harness/{dir}/goldens/ — the \
                 artifact it writes would change with no diff to review"
            );
        }
    }

    // The neutral panel is a SECOND source, not a descriptor (amendment 0-e):
    // dropping it would hide the `delegation.worker` gate.
    assert!(
        super::health::panel_labels()
            .iter()
            .any(|(h, _)| *h == Harness::ANY),
        "the Harness health panels lost their neutral row — the `Harness::ANY` capabilities \
         (today `delegation.worker`) carry a GATE, and a gate the user can be blocked by and \
         cannot see is the thing that panel exists to end"
    );
}

/// The L4 capabilities. A harness module reaching for one of these has put
/// capability logic in the wrong layer (design § 2: L4 speaks only cImp domain
/// types; L1 speaks the harness).
const CAPABILITY_MODULES: &[&str] = &[
    "crate::graph",
    "crate::tts",
    // `crate::usage` was here until V40 Phase D. It is not an L4 capability any
    // more — it was never neutral enough to be one, and its whole data path
    // (the status-line push, the push file, the quota and context readings)
    // lives in `harness/claude/usage.rs` behind `usage_source()`. There is no
    // module above the seam for a harness module to reach for.
    "crate::workbench",
    // V39 Phase B. `crate::delegation` is an L4 capability like the four above,
    // and it needs one thing from L1: the moment a turn ended. That arrives
    // through `OobContext::note_turn_text` — so the readers themselves name
    // nothing above the seam, and the ONE file that does is `harness/reader.rs`,
    // which owns the readers' spawn context and is already exempt. Listing the
    // module here is what keeps that true: without it, the next harness reader
    // could call the engine directly and no test would notice.
    "crate::delegation",
];

/// Harness modules that DO still import upward, with the reason — **a shrinking
/// list, not an escape hatch**.
///
/// Phase K wrote this list expecting Phase L to empty it: "Phase L makes the
/// plugin the source of all three, at which point each of these files stops
/// needing anything above L2 and its line here is deleted." **That expectation
/// was wrong, and the reasons below are the correction.** Phase L moved
/// assistant text, tool-result sizing and sub-agent lifecycle onto pushes, but
/// it could not move usage, session identity, the context window or sub-agent
/// token accounting — no Claude hook payload carries any of them — and it
/// deliberately did not move OpenCode's half (design D6: a declared fallback
/// beats a lossy migration). So every reader here still runs, for the
/// capabilities nothing pushes, on every tab.
///
/// What DID change is the entries' meaning: these are no longer "the only path,
/// pending a migration" but "the arbitrated fallback, plus the capabilities that
/// have no other path". An entry leaves this list when its file stops importing
/// upward at all — which for the readers now means an upstream change, not a
/// cImp phase.
///
/// The test asserts in BOTH directions: a harness module NOT on this list may
/// not import upward, and a module ON it must still be importing upward — so
/// the list cannot rot into padding that outlives the reason for it.
const UPWARD_EXEMPT: &[(&str, &str)] = &[
    (
        "harness/reader.rs",
        "`OobContext` is the fallback readers' spawn context — it OWNS the TTS sender and the \
         graph handle it hands them. V35 Phase L thinned it: the prose→speech composition moved \
         to `crate::tts::prose` so the push path could share it, leaving this file with the \
         sender, the graph recorders and the arbitration query. Retires with the readers.",
    ),
    (
        "harness/claude/read.rs",
        "V35 Phase L arbitrated three of its taps off for a tab that pushes them, and could not \
         arbitrate the rest: `UsageEvent::Turn` (no hook carries token counts), session identity, \
         session→commit provenance and sub-agent token accounting all still record into `graph` \
         from here, and prose still reaches `tts` on every tab that does not serve \
         `assistant_text`. Retires when upstream grows the payloads, not on a cImp schedule.",
    ),
    // `harness/opencode/read.rs` USED to be listed here, and its removal is the
    // first thing this list's both-directions assertion ever caught. The entry
    // was never true: that file's production `use` block names no capability
    // module at all — it reaches TTS and the graph through
    // `super::super::OobContext`, which is precisely the L1 → L2 direction this
    // test wants. The only `crate::tts` in it is a `use` inside `mod tests`, and
    // the old boundary finder (see `executable_text`) was scanning that test
    // module, so the exemption "still imports upward" check passed on test text.
    // The capability-coverage claim it carried — that this file is OpenCode's
    // DECLARED fallback since Phase L (design D6) — was real but belongs where it
    // is enforced: the hello (`chp::EVENTS`) and the file's own module docs. An
    // import exemption is only ever about import direction.
    // `harness/claude/statusline.rs` was listed here because the status-line
    // payload IS the usage widget's only data source and the widget lived in
    // `crate::usage`, an L4 capability. V40 Phase D moved the widget's whole
    // data path into this directory (`harness/claude/usage.rs`), so the file
    // imports nothing above L2 and the exemption has nothing left to allow.
    // The FACT it recorded is unchanged and still true — no hook input carries
    // a context window or a rate-limit block, so `session.context` stays
    // reserved with no producer (`chp::EVENTS`) — it is just no longer an
    // import-direction question.
    (
        "harness/claude/canary.rs",
        "The L1 canaries assert the Tier-C readers still produce SUBSTANTIVE output, so they \
         necessarily speak the capability types those readers return (`UsageEvent`, \
         `UsageOrigin`). Since V35 Phase L they are the FALLBACK's proof — which makes them \
         more load-bearing, not less: a fallback nobody checks is what makes a primary's \
         failure fatal. They follow the readers. V40 Phase A moved the assertions here from \
         `harness/canary.rs` (locked decision 17); the runner keeps the corpus rules and the \
         dispatcher and names nothing above the seam, which is why its own entry is gone.",
    ),
    (
        "harness/opencode/canary.rs",
        "The same, one layer over: OpenCode's canary drives `Tracker::handle` to the end of \
         the chain, and the end of that chain is `crate::tts` — proving the turn is SPOKEN is \
         the whole assertion, so the speech type is the canary's vocabulary too.",
    ),
    (
        "harness/claude/probe.rs",
        "The L2 probe drives the same readers against the installed CLI — same reason as the \
         canaries. V40 Phase A moved the bodies out of `harness/probe.rs` (locked decision 17) \
         and the exemption moved with them: what reaches `crate::graph` is `parse_usage_line`'s \
         return type, which is the READER's vocabulary, not the runner's. `harness/probe.rs` \
         itself keeps the report shape, the outcome model and the CLI, and names nothing above \
         the seam — which is why its own entry is gone rather than reworded.",
    ),
    (
        "harness/claude/hook.rs",
        "ONE call: `graph::shellread::whole_file_read` decides whether a `Bash` command is really \
         a whole-file read, on the `PreToolUse` path. That predicate is a shell parser with no \
         graph state — it is mis-homed, not mis-layered. Move `shellread` below the seam and this \
         line goes; until then it is declared rather than ambient.",
    ),
];

/// **L1 does not reach into L4** (design § 4.1).
///
/// The direction is the whole point: everything above L2 must be typed against
/// CHP rather than against harness-shaped Rust, which is what makes a third
/// harness additive (§ 6: "changes to L2 / L3 / L4 — none"). A harness module
/// that calls `graph::` is a place where adding a harness would mean editing a
/// capability.
#[test]
fn harness_modules_do_not_import_capabilities() {
    let exempt: BTreeMap<&str, &str> = UPWARD_EXEMPT.iter().copied().collect();
    let mut violations: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut still_needed: BTreeSet<String> = BTreeSet::new();

    for (path, text) in source_files() {
        // This file NAMES the capability modules — it is the rule, not a
        // subject of it.
        if !path.starts_with("harness/") || path == "harness/layering.rs" {
            continue;
        }
        let body = executable_text(&path, &text);
        let found: BTreeSet<String> = CAPABILITY_MODULES
            .iter()
            .filter(|m| body.contains(**m))
            .map(|m| m.to_string())
            .collect();
        if found.is_empty() {
            continue;
        }
        if exempt.contains_key(path.as_str()) {
            still_needed.insert(path);
        } else {
            violations.insert(path, found);
        }
    }

    assert!(
        violations.is_empty(),
        "harness modules importing a capability — the dependency direction is L1 → L2 only. Move \
         the capability call above the seam, or (if it is a Phase L fallback reader) add it to \
         UPWARD_EXEMPT with the reason:\n{violations:#?}"
    );

    let declared: BTreeSet<String> = exempt.keys().map(|k| k.to_string()).collect();
    let stale: Vec<&String> = declared.difference(&still_needed).collect();
    assert!(
        stale.is_empty(),
        "these UPWARD_EXEMPT entries no longer import anything upward — delete them. An exemption \
         that outlives its reason is how a shrinking list becomes padding: {stale:#?}"
    );
}

// ── V40 locked decision 10(a): harness IDENTITY, not harness payloads ───────

/// Production files outside `harness/` that may still name a harness, each with
/// the reason and the phase that retires it.
///
/// This is the worklist for V40 phases B–G, and it is **meant to be long right
/// now**: Phase A moved the registry, the plugin interface and the ten "which
/// harness" functions, and every remaining row here is a surface a later phase
/// owns. What it must never be is *dishonest* — an entry that does not
/// correspond to a real hit, or a hit that nobody wrote a reason for.
///
/// **Checked in both directions** by
/// [`every_identity_allowlist_entry_is_still_earning_it`], exactly like
/// [`LITERAL_ALLOWLIST`] and [`UPWARD_EXEMPT`]: a path must exist AND still
/// contain an identity hit, so an entry cannot outlive the code it was written
/// for and become a blanket exemption for whatever that file grows into. It
/// already caught one: `graph/mcp.rs` had a row here until Phase A's fix to
/// `source_for_consumer` left the file with no identity in it at all.
const IDENTITY_ALLOWLIST: &[(&str, &str)] = &[
    // ── settings: persisted wire forms and frozen migrations ───────────────
    (
        "settings/schema.rs",
        "PERSISTED WIRE FORMS, and after Phase B that is ALL that is left. `CLAUDE_TAB_ID` / \
         `CLAUDE_LOCAL_TAB_ID` / `OPENCODE_TAB_ID` and `AiTabId`'s serde renames are what is on \
         disk in every user's settings file, and `RETIRED_TAB_IDS` is a migration input; locked \
         decisions 3 and 29 keep their encodings. `default_{claude,claude_local,opencode}_tab`'s \
         `command:` strings are the same class — a reserved tab's seeded command IS its wire \
         form — and become the descriptor's `default_tab(tab_id)` in a later phase. Phase A took \
         the ORDER (`canonical_ai_tab_order` is a registry view) and the ranking; Phase B took \
         every per-harness FIELD (`claude_local`, `statusline`, `expose_commands_*`, \
         `code_audit.expose_*`, `claude_access`/`opencode_access`, `harness_versions.*`) into \
         `Settings::harness` and the plugins' `ext`.",
    ),
    // ── state: a persisted tab id is a persisted tab id ────────────────────
    (
        "state/manager.rs",
        "PERSISTED WIRE FORMS (locked decision 29). `TabId::{as_str, from_str}` and its \
         `Serialize`/`Deserialize` carry `\"claude\"` / `\"claude-local\"` / `\"opencode\"` \
         because those strings are in every settings file and every frontend payload; typing \
         them as `HarnessId` would mis-read a pre-split row. Phase A removed the per-harness \
         BRANCHES (`kind`, `is_builtin` are registry lookups now); the variants stay for the \
         encodings. Phase D took the OTHER residue this row used to name: the signal \
         vocabulary is `StateSignal::HarnessOutput*` / `SubagentsActiveChanged` now, \
         served over CHP as well as emitted in-process, and the sub-agent stall \
         backstop asks the tab's harness for its timing instead of holding one \
         product's constant (locked decisions 18 and 30). What is left here really is \
         only the persisted strings.",
    ),
    // ── the MCP consumer vocabulary ────────────────────────────────────────
    // ── the loopback wire ──────────────────────────────────────────────────
    //
    // `offload/loopback.rs` left this list in V40 Phase C, and it is the row the
    // list was written to retire. Its reason ended "this file leaves BOTH
    // allowlists in Phase C", and both halves happened in one change: the
    // `/claude/hook/*` route table and the ~900 lines of `handle_claude_*`
    // bodies are `harness::claude::hook::ROUTES_TABLE` behind
    // `HarnessPlugin::routes()` (locked decisions 15 and 22), the `X-CIMP-*`
    // identity special-case is `identity_of_request()`, the drift vocabulary is
    // `drift_vocabulary()`, and `classify_permission_event` went with the
    // payload it classifies. Core's router appends plugin routes after its own
    // arms and writes back a `HookReply` it does not read.
    // ── the per-harness surfaces later phases own ──────────────────────────
    // `graph/service.rs` left this list in V40 Phase D. Its reason was session
    // identity (locked decision 20): `live_claude_sessions` filtered `e.agent ==
    // "claude"`, and the registry was one map holding two key spaces because
    // Claude keys live sessions by TAB id and OpenCode by SESSION id. Both are
    // gone — `live_sessions_for(HarnessId)` takes the harness, `LiveKey` carries
    // the declared `session_key_space()`, and the C-2 collision guard that stood
    // between them is deleted because the collision is unrepresentable.
    // `offload/mcp.rs` left this list in V40 Phase D. Its reason was
    // `consumer() == "claude"` gating the session-push registration
    // (`--dangerously-load-development-channels` and the child's
    // `--channel-push`); locked decision 25 makes it a declared
    // `supports_session_push()` on the plugin, because "has an inbound MCP path"
    // is a fact about a harness and not about the one that happens to have one.
    // `tabs/config.rs` left this list in V40 Phase C. Its ONE residual was
    // `claude_harness()`, the permission-hook cwd fallback's harness — a Claude
    // mechanism, because only Claude's hook payload has the cwd gap it exists
    // for. `claude_tab_dirs` is `harness_tab_dirs(.., HarnessId)` now and the
    // plugin route passes the id it already is, so the question is asked by the
    // harness rather than answered for it (locked decision 22).
    // `ipc/tab_lifecycle.rs` left this list in V40 Phase E, and its reason named
    // exactly what happened: the `resolve_command("opencode")` preflight is
    // `HarnessPlugin::preflight()` (locked decision 26), asked of every harness
    // whose tab is being turned on, and Claude's "intentionally not gated — it's
    // the app's own front end" is a declared `Ok` instead of a comment beside an
    // `if`. The refusal it raises carries the harness token, label and install
    // hint the plugin supplied, so `TabLifecycleError` names no product either.
];

/// The identity needles: every registry id, tab id, binary and consumer token.
///
/// Derived from the registry rather than hand-kept, so a harness added later
/// widens the scan by that fact alone — the same two-sources-of-truth
/// discipline [`harness_literals`] follows.
fn identity_needles() -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for d in crate::harness::registry::HARNESSES {
        out.insert(d.id.to_string());
        out.insert(d.consumer.to_string());
        out.extend(d.tab_ids.iter().map(|s| s.to_string()));
        out.extend(d.binaries.iter().map(|s| s.to_string()));
    }
    out
}

/// Files whose harness identity is EXEMPT by class rather than by row.
///
/// `settings/migration.rs` and the historical migrations in
/// `settings/persistence.rs` describe **old on-disk shapes** (locked decision
/// 14): they are frozen history and must keep their literals, or they stop
/// describing the files they exist to read. Test files are out of scope for the
/// same reason [`executable_text`] drops test modules — a fixture that names a
/// harness is a recorded input, not a dependency on one.
fn identity_exempt_by_class(path: &str) -> bool {
    path == "settings/migration.rs" || path.ends_with("tests.rs")
}

/// Every production file outside `harness/` that still names a harness, with the
/// needles it names.
fn identity_offenders(files: &[(String, String)]) -> BTreeMap<String, BTreeSet<String>> {
    let needles = identity_needles();
    assert!(
        needles.len() >= 3,
        "the identity needle list collapsed to {} entries — the registry-derived scan stopped \
         producing tokens, which would make this test pass by finding nothing",
        needles.len()
    );
    let mut offenders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (path, text) in files {
        if path.starts_with("harness/") || identity_exempt_by_class(path) {
            continue;
        }
        if IDENTITY_ALLOWLIST.iter().any(|(p, _)| p == path) {
            continue;
        }
        let body = executable_text(path, text);
        for n in &needles {
            if body.contains(&format!("\"{n}\"")) {
                offenders.entry(path.clone()).or_default().insert(n.clone());
            }
        }
    }
    offenders
}

/// **No harness IDENTITY outside the registry** (V40 locked decision 10(a)).
///
/// [`no_harness_literals_outside_harness`] never policed this: its needles come
/// from registry `Dep` tokens filtered to >= 8 chars / underscore / camelCase,
/// so `"claude"` and `"opencode"` were never needles at all — `graph/service.rs`
/// carried 128 of them and the suite was green. That test protects Claude's
/// *payload field names*, which is what V35 Phase K set out to do; "which
/// harness is this" was never in scope.
///
/// This one is that scope. Core may HOLD a `HarnessId` and pass it to the
/// registry; it may not spell one, and it may not branch on one. The built-in
/// ids exist for persisted wire forms, and those are what the allowlist is for.
#[test]
fn no_harness_identity_outside_registry() {
    let offenders = identity_offenders(&source_files());
    assert!(
        offenders.is_empty(),
        "harness identity outside `harness/` — resolve it through \
         `harness::HarnessId::from_{{command,tab_id,consumer,id}}`, or add an IDENTITY_ALLOWLIST \
         entry naming the phase that retires it:\n{offenders:#?}"
    );
}

/// The identity allowlist, checked the other way round.
///
/// Every entry must name a file that exists AND still contains an identity
/// needle. An exemption that outlives its reason is how a shrinking worklist
/// becomes padding — the failure mode
/// [`every_literal_allowlist_entry_is_still_earning_it`] was written for.
#[test]
fn every_identity_allowlist_entry_is_still_earning_it() {
    let files: BTreeMap<String, String> = source_files().into_iter().collect();
    let needles = identity_needles();
    let mut stale: Vec<&str> = Vec::new();
    let mut duplicated: Vec<&str> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (path, reason) in IDENTITY_ALLOWLIST {
        assert!(
            !reason.trim().is_empty(),
            "{path}: an allowlist entry with no reason is an exemption nobody can review"
        );
        if !seen.insert(path) {
            duplicated.push(path);
        }
        let Some(text) = files.get(*path) else {
            stale.push(path);
            continue;
        };
        let body = executable_text(path, text);
        if !needles.iter().any(|n| body.contains(&format!("\"{n}\""))) {
            stale.push(path);
        }
    }
    assert!(
        duplicated.is_empty(),
        "IDENTITY_ALLOWLIST names the same file twice: {duplicated:#?}"
    );
    assert!(
        stale.is_empty(),
        "these IDENTITY_ALLOWLIST entries name a file that is gone or no longer contains any \
         harness identity — delete them; the list is the B-G worklist, and a done row must \
         leave it: {stale:#?}"
    );
}
