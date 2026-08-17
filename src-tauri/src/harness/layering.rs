//! V35 Phase K — **the layering, as tests** (design § 4.1).
//!
//! A layering that exists only in a design document rots. Phase K moved the
//! harness surface into one directory; these three tests are what stop the next
//! feature from putting it back. The fourth test named in § 4.1,
//! `wired_in_paths_exist`, already lives with the registry it checks
//! ([`crate::harness::contract`]) and is what forced this phase to update every
//! `wired_in` path in the same commit.
//!
//! All three read the source tree, in the repo's existing source-scanning
//! idiom: `CARGO_MANIFEST_DIR` is `<repo>/src-tauri`, so `src/` is one join
//! away and no path is hard-coded relative to a working directory.
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

use super::contract::{Dep, Harness, CAPABILITIES};

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

/// Drop the trailing `#[cfg(test)]` module and every comment line.
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
fn executable_text(text: &str) -> String {
    let cut = text
        .match_indices("\n#[cfg(test)]\n")
        .last()
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    text[..cut]
        .lines()
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
    for c in CAPABILITIES {
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
/// now it is these four files, on purpose, and a fifth needs a line here and a
/// reviewer.
const LITERAL_ALLOWLIST: &[(&str, &str)] = &[
    (
        "offload/loopback.rs",
        "L2 by design (§ 4: 'route table; handlers stay in offload/loopback.rs'). \
         `classify_permission_event` reads Claude's `hook_event_name` values because it is the \
         receiving end of the wire — the CHP seam itself, not a consumer above it.",
    ),
    (
        "taint_beacon.rs",
        "V35 Phase J deleted five of Claude's seven command-hook shims; these two survive because \
         they are report-only side effects with no reply to parse, so `type: \"http\"` bought them \
         nothing. They read `tool_name` off a Claude payload — genuinely Claude's L1, living \
         outside `harness/claude/` because they are separate process entry points. Retire with \
         the beacons themselves, or fold their payload reading into `harness::claude::hook` \
         (which already serves them `missing_fields`/`resolve_cwd`/`tab_arg`).",
    ),
    (
        "checkpoint_beacon.rs",
        "The sibling of `taint_beacon.rs` above — same shape, same reason, same follow-up.",
    ),
    (
        "processing/patterns_file.rs",
        "The Claude TUI permission footer (capability `perm.tui_scrape`, Tier D). The regex \
         matcher is the FALLBACK detector behind the `Notification` hook, and it lives with the \
         PTY screen scraper it runs against. Phase J already retired its primacy; Phase L kept \
         the fallback, on the same principle every other fallback here follows.",
    ),
    (
        "processing/permission.rs",
        "The same footer, in the matcher `patterns_file.rs` feeds — one capability row \
         (`perm.tui_scrape`), two files, same follow-up.",
    ),
    (
        "usage/mod.rs",
        "cImp's OWN on-disk usage format, which deliberately reuses the upstream spelling \
         (`five_hour`, `resets_at`, `context_window_size`) so a reader can line the two up. The \
         thing that PARSES the harness payload moved to `harness/claude/statusline.rs` in this \
         phase; what is left here is the sink and its serde field names, which cImp owns and a \
         Claude rename cannot touch.",
    ),
    (
        "offload/toolclass.rs",
        "`MultiEdit` in cImp's routed tool `TABLE`. Deliberate and documented at the row: those \
         are Claude's capitalized natives, kept in cImp's vocabulary because V33's `mutates_fs` \
         consumer resolves a tool name there. The HARNESS-owned table (OpenCode's own ids) moved \
         to `harness/opencode/tools.rs` in this phase; folding Claude's into it would recreate \
         the two-vocabularies-one-lookup bug both tables exist to prevent.",
    ),
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
    (
        "graph/memory.rs",
        "FINDING, not a clean exemption: `memory_kind_of` matches both harnesses' edit-tool ids \
         (`Edit`/`Write`/`MultiEdit`/`NotebookEdit`/`edit`/`write`/`patch`) inline to classify a \
         memory event. It should ask `toolclass`/`harness::opencode::tools` instead — but the \
         two vocabularies answer differently for `edit` vs `Edit`, so rerouting it is a \
         behaviour decision, not a relocation. Phase K recorded it rather than changing it.",
    ),
];

/// **No harness-owned string outside `harness/`** (design § 4.1).
///
/// Before Phase K these literals were confined to `oob/`, `statusline/` and
/// `tabs/config.rs` by habit alone. This converts the habit into an invariant:
/// a new feature that reads a Claude payload field in `graph/` or `workbench/`
/// fails the build instead of quietly creating the next Tier-C dependency
/// nobody wrote down.
#[test]
fn no_harness_literals_outside_harness() {
    let needles = harness_literals();
    assert!(
        needles.len() > 30,
        "the needle list collapsed to {} entries — the registry-derived half stopped producing \
         tokens, which would make this test pass by finding nothing",
        needles.len()
    );
    let mut offenders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (path, text) in source_files() {
        if path.starts_with("harness/") || path.ends_with("tests.rs") {
            continue;
        }
        if LITERAL_ALLOWLIST.iter().any(|(p, _)| *p == path) {
            continue;
        }
        let body = executable_text(&text);
        for n in &needles {
            // The whole literal, not a substring: a log line that mentions
            // `SessionStart` in prose is describing the seam, while `"tool_name"`
            // is reading across it.
            if body.contains(&format!("\"{n}\"")) {
                offenders.entry(path.clone()).or_default().insert(n.clone());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "harness-owned literals outside `harness/` — move the code that reads them into \
         `harness/<id>/`, or add an allowlist entry saying why it cannot move:\n{offenders:#?}"
    );
}

/// Which `harness/<id>/` directory belongs to which [`Harness`].
///
/// A new sub-directory must be added here, which is what makes
/// [`every_harness_dir_declares_its_capabilities`] able to say anything at all.
const HARNESS_DIRS: &[(&str, Harness)] = &[("claude", Harness::Claude), ("opencode", Harness::OpenCode)];

/// **Every `harness/<id>/` declares itself** (design § 4.1).
///
/// Two claims, because a harness nobody can reason about fails either one: it
/// has rows in the capability registry (what cImp depends on it for), and it
/// opens with a CHP hello (what it says it can serve, per tab, at connect).
#[test]
fn every_harness_dir_declares_its_capabilities() {
    let root = src_root().join("harness");
    let on_disk: BTreeSet<String> = std::fs::read_dir(&root)
        .expect("src/harness exists")
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let declared: BTreeSet<String> = HARNESS_DIRS.iter().map(|(d, _)| d.to_string()).collect();
    assert_eq!(
        on_disk, declared,
        "a `harness/<id>/` directory exists that this test does not know about (or vice versa) — \
         declare it in HARNESS_DIRS so its rows and its hello are checked"
    );

    for (dir, harness) in HARNESS_DIRS {
        let rows: Vec<&str> = CAPABILITIES
            .iter()
            .filter(|c| c.harness == *harness)
            .map(|c| c.id)
            .collect();
        assert!(
            !rows.is_empty(),
            "harness/{dir}/ has no rows in the capability registry — nothing records what cImp \
             depends on it for, so nothing can degrade visibly when it changes"
        );
        // The hello is the other half: `serves` / `cannot`, built from the
        // per-tab flags that decided what was actually wired (design D3).
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
    }
}

/// The L4 capabilities. A harness module reaching for one of these has put
/// capability logic in the wrong layer (design § 2: L4 speaks only cImp domain
/// types; L1 speaks the harness).
const CAPABILITY_MODULES: &[&str] = &["crate::graph", "crate::tts", "crate::usage", "crate::workbench"];

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
    (
        "harness/opencode/read.rs",
        "The SSE tap, and OpenCode's DECLARED fallback since V35 Phase L (design D6). Its plugin \
         API can reach assistant text and tool results — the hello now says so, and says why \
         neither is wired: one would change the segmenter's unit, the other would add a \
         capability rather than migrate one. Until that changes this file is the whole read path \
         for OpenCode.",
    ),
    (
        "harness/claude/statusline.rs",
        "The status-line payload IS the usage widget's only data source, and V35 Phase L did not \
         change that: no hook input carries a context window or a rate-limit block, so \
         `session.context` stays reserved with no producer (`chp::EVENTS`).",
    ),
    (
        "harness/canary.rs",
        "The L1 canaries assert the Tier-C readers still produce SUBSTANTIVE output, so they \
         necessarily speak the capability types those readers return. Since V35 Phase L they are \
         the FALLBACK's proof — which makes them more load-bearing, not less: a fallback nobody \
         checks is what makes a primary's failure fatal. They follow the readers.",
    ),
    (
        "harness/probe.rs",
        "The L2 probe drives the same readers against the installed CLI — same reason as the \
         canaries.",
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
        let body = executable_text(&text);
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
