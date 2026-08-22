//! V35 Phase B — L1 canaries for the five Tier-C readers.
//!
//! # What these assert, and why it is not "does it parse"
//!
//! Every reader pinned here is deliberately lenient. `parse_usage_line` ends
//! each token lookup in `unwrap_or(0)`; `statusline/mod.rs` documents that "a
//! parse failure yields `Input::default()`" and walks the push payload as a raw
//! `Value` where every field is an `Option`; `Tracker::handle` `match`es on the
//! SSE event `type` and ignores everything it does not know. That leniency is
//! correct — a shim must never break a user's turn over an unexpected field —
//! but it means **an upstream rename produces zeros and empty strings, not
//! errors**. Nothing throws, nothing logs, the usage widget just reads 0.
//!
//! So a canary asserts **substantiveness**: fed a fixture of the shape we
//! recorded, does the reader still produce a non-zero, non-empty result?
//! (Milestone locked decision 3; global principle 5, *empty is not absent*.)
//! Every fixture is authored so that every asserted field is legitimately
//! non-zero — a fixture with a real zero in it cannot tell "absent" from
//! "zero" and quietly defeats the test. **Fixture selection is part of the
//! contract.**
//!
//! # Two callers, ONE code path (V35 Phase F)
//!
//! Phase B shipped this module as `#[cfg(test)]`, which was enough while
//! `cargo test` was the only consumer. Phase F made the canaries run **in the
//! shipped binary**, in the background, whenever the installed Claude Code
//! version changes — so every positive assertion is an ordinary function
//! `fn(&str) -> Result<(), String>` taking the fixture body, declared by its
//! harness as a [`Canary`] row and dispatched by [`run_embedded`]. The
//! `#[test]`s beside each one hand it the same embedded fixture the runner
//! does, so the two callers really are one code path.
//!
//! That shape is the point: if `cargo test` drove a *copy* of the assertions,
//! coverage would fork — the suite could go green while the auto-verify that
//! advances `claude_last_verified` checked something else. The negative
//! canaries additionally assert that the very same functions return `Err` on a
//! drift fixture, so "the canary fires" is proven about the production path and
//! not merely about a test.
//!
//! # Fixtures
//!
//! `src-tauri/fixtures/harness/<harness>/<version>/<name>`. The five
//! **positive** fixtures are `include_str!`-embedded (a release binary has no
//! repo tree to load them from — the milestone deploy trap allows exactly this:
//! "`include_str!` only for the small synthetic fixtures"). Everything else —
//! the `_synthetic/` drift models and the manifest walker — loads from disk
//! through [`fixture`] at test runtime, where the tree exists.
//!
//! They are synthetic-minimal and hand-authored from the reader code's
//! contract, never copied from a real transcript: real transcripts carry user
//! prompts, file contents, tool output and plausibly credentials (locked
//! decision 4). Each version directory carries a `MANIFEST.toml` recording where
//! the shape came from, and [`tests::every_fixture_version_dir_has_a_manifest`]
//! fails the suite for a directory without one — an anonymous fixture is
//! indistinguishable from a guess.
//!
//! The Phase C drift models live beside the version directories in
//! `<harness>/_synthetic/` and carry a manifest under the *same* rule plus one
//! extra key (`models_version`, which must name a real sibling version
//! directory). `_synthetic` is deliberately **not** exempted from the walker:
//! an exemption is exactly the silent hole through which undated fixtures
//! would accumulate.
//!
//! # Where the canaries live (V40 Phase A, locked decision 17)
//!
//! This module is the harness-neutral **runner**: the dispatcher, the
//! `substantive!` macro both harnesses' assertions are written in, the shared
//! fixture plumbing, the corpus provenance walker and the registry
//! cross-checks. The assertions themselves live with the harness they are true
//! of — `harness/claude/canary.rs`, `harness/opencode/canary.rs` — and reach
//! here through
//! [`HarnessPlugin::canaries`](crate::harness::plugin::HarnessPlugin::canaries).
//! There is no harness `match` here; [`run_embedded`] asks the registry.
//!
//! # One naming rule
//!
//! Every canary is named `canary_<capability id with dots as underscores>`, its
//! negative twin `negative_canary_<same>`, and [`support::row`] re-asserts on
//! every run that the registry row it claims points back at it. **A canary id
//! IS a capability id** — never a third namespace. That is what lets
//! [`tests::canaries_and_the_matrix_agree`] cross-check the suite against the
//! registry mechanically instead of against a hand-maintained list, and what
//! lets [`embedded`] be checked against the registry's `canary` column rather
//! than trusted.
//!
//! # Negative canaries (Phase C)
//!
//! A positive canary that never actually ran passes just as green as one that
//! did. So each covered capability also gets a **drift model**: the same
//! fixture with one load-bearing field renamed, and a test asserting the reader
//! answers with its degraded default — zero, empty, `None`, no speech. Phase B
//! established this by hand-mutating fixtures once; Phase C makes it permanent.
//! Every one of them is a `guard: this fixture models the drift case` assertion
//! (design doc § 3.4): it does not describe desired behavior, it pins today's
//! silent-degradation behavior so the positive canary's assertion is proven to
//! be load-bearing. Each also asserts the *untouched* half of the same fixture
//! still works, so a broken fixture cannot masquerade as a proven mechanism.

use serde_json::Value;

use crate::harness::plugin::Canary;

// ── the runtime dispatcher (V35 Phase F; V40 Phase A moved the bodies) ──────

/// The capability ids with an embedded, runtime-callable canary, in the order
/// [`crate::harness::verify`] runs them.
///
/// **Registry order, not a literal** (V40 Phase A, locked decision 17): each
/// harness declares its own through
/// [`HarnessPlugin::canaries`](crate::harness::plugin::HarnessPlugin::canaries),
/// so a harness added to the registry brings its canaries with it and core
/// keeps no per-harness list to forget to widen.
///
/// Set-compared against the registry's `canary: Some(..)` column in both
/// directions by [`tests::embedded_canaries_are_exactly_the_declared_ones`]: a
/// declared canary missing from here would be a row the auto-verify silently
/// never checks, and an entry here with no row would be a check nobody
/// declared.
pub fn embedded() -> Vec<&'static str> {
    all_canaries().map(|c| c.id).collect()
}

/// Every registered harness's canaries, in registry order.
fn all_canaries() -> impl Iterator<Item = &'static Canary> {
    crate::harness::registry::all()
        .filter_map(|h| h.plugin())
        .flat_map(|p| p.canaries().iter())
}

/// Run one embedded canary by capability id. `None` when the id has no
/// embedded canary — deliberately distinct from `Some(Err(..))`, because
/// "nothing checks this" and "this failed" must never be the same value
/// (a `Fail` blocks the auto-advance; an absent canary must not).
///
/// **Blocking.** `opencode.sse.events` drives an `async` reader, so its `run`
/// parks a private current-thread runtime on it — which means this function
/// must NOT be called from inside an async context (it would panic). The
/// auto-verify worker is a plain OS thread; the async test drives
/// [`crate::harness::opencode::canary::check_opencode_sse_events`] directly instead.
pub fn run_embedded(id: &str) -> Option<Result<(), String>> {
    all_canaries().find(|c| c.id == id).map(|c| (c.run)(c.fixture))
}

/// Drive one future to completion on a private current-thread runtime — the
/// idiom `offload::mcp` and `sandbox` already use for "an async call from a
/// blocking context". A runtime that cannot be built is reported as a canary
/// failure rather than swallowed: it means the check did not run, and the whole
/// point of Phase F is that an unrun check never looks like a passing one.
pub(in crate::harness) fn block_on_current_thread(
    fut: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(fut),
        Err(e) => Err(format!(
            "the canary could not be run at all: building a current-thread runtime failed ({e})"
        )),
    }
}

/// `return Err(format!(..))` unless the condition holds — the runtime canaries'
/// `assert!`. Spelled as a macro so each check reads like the assertion it
/// replaced and keeps its message verbatim.
// Written as `if cond {} else {}` rather than `if !cond {}` on purpose: several
// of the conditions are float comparisons, and negating a `PartialOrd`
// comparison is a clippy denial (`neg_cmp_op_on_partial_ord`) — for a good
// reason, since `!(x > 0.0)` is also true for `NaN`.
macro_rules! substantive {
    ($cond:expr, $($msg:tt)*) => {
        if $cond {
        } else {
            return Err(format!($($msg)*));
        }
    };
}
// The harness canary modules are the callers; the macro stays declared here so
// both harnesses' assertions keep reading (and failing) identically.
pub(in crate::harness) use substantive;

/// The non-empty lines of a `.jsonl` fixture, parsed. A malformed fixture is a
/// defect in the fixture rather than a drift signal, so it says so.
pub(in crate::harness) fn parse_lines(raw: &str) -> Result<Vec<Value>, String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l)
                .map_err(|e| format!("fixture is not valid JSON ({e}): {}", l.trim()))
        })
        .collect()
}

// ── fixture plumbing shared by the harness canary modules ──────────────────

/// Test-only helpers the harness canary modules' negative twins reach for:
/// loading a `_synthetic/` drift model off disk, and the registry-row join
/// every canary asserts before it runs.
///
/// `pub(in crate::harness)` rather than duplicated per harness, because `row`
/// IS the join-key check — two copies of it could disagree about what a canary
/// id means, which is the one thing that would make the whole suite decorative.
#[cfg(test)]
pub(in crate::harness) mod support {
    use std::path::{Path, PathBuf};

    use serde_json::Value;

    use crate::harness::contract::{self, Capability};

    /// Root of the committed fixture corpus.
    pub(in crate::harness) fn fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("harness")
    }

    /// Read one fixture by its `<harness>/<version>/<name>` relative path.
    /// Panics with the resolved path when it is missing, because a canary that
    /// silently skips is worse than no canary at all.
    pub(in crate::harness) fn fixture(relpath: &str) -> String {
        let path = fixtures_root().join(relpath);
        std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "harness fixture `{relpath}` could not be read at {}: {e}",
                path.display()
            )
        })
    }

    /// Parse one fixture line as JSON. A malformed fixture is a defect in the
    /// fixture, not a drift signal, so it panics distinctly.
    pub(in crate::harness) fn json(raw: &str) -> Value {
        serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("fixture is not valid JSON ({e}): {}", raw.trim()))
    }

    /// The non-empty lines of a `.jsonl` fixture, parsed.
    pub(in crate::harness) fn json_lines(raw: &str) -> Vec<Value> {
        raw.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(json)
            .collect()
    }

    /// The registry row a canary proves, with the join key checked in both
    /// directions: the id must exist in [`contract::capabilities`], and that
    /// row's `canary` must name this same id. A canary drifting away from its
    /// row is the one failure mode that would make the whole suite decorative.
    pub(in crate::harness) fn row(id: &'static str) -> &'static Capability {
        let cap = contract::get(id).unwrap_or_else(|| {
            panic!("canary `{id}` names no capability — canary ids ARE capability ids")
        });
        assert_eq!(
            cap.canary,
            Some(id),
            "capability `{id}` does not claim its canary: set `canary: Some(\"{id}\")` on the \
             registry row (and drop the waiver it replaces)"
        );
        cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use super::support::{fixture, fixtures_root};
    use crate::harness::contract;

    // ── the runtime half (V35 Phase F) ──────────────────────────────────────

    /// The declared canaries and the registry's `canary` column are the same set.
    ///
    /// This is the join that makes auto-verify's coverage checkable: the
    /// registry says which rows claim a canary, and this list says which ones
    /// the *shipped binary* can actually run. A declared canary missing here
    /// would be a row auto-verify silently never checks (and would still count
    /// as coverage in `every_silent_degradation_has_a_canary_or_a_probe_or_a_waiver`);
    /// an entry here with no row would be a check nobody declared.
    #[test]
    fn embedded_canaries_are_exactly_the_declared_ones() {
        let ids = embedded();
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "two harnesses declare the same canary id: {ids:?}"
        );
        let declared: BTreeSet<&str> =
            contract::capabilities().filter_map(|c| c.canary).collect();
        assert_eq!(
            unique, declared,
            "the runtime canary list and the registry's `canary` column have diverged — \
             auto-verify would run a different set than the matrix claims is covered"
        );
    }

    /// The production entry point answers for every embedded id, and every
    /// answer is `Ok` today.
    ///
    /// Deliberately a plain `#[test]`: [`run_embedded`] parks its own runtime
    /// for the async reader, which is exactly how the auto-verify worker (a
    /// plain OS thread) calls it. Running it from a `#[tokio::test]` would
    /// panic — and that panic is the reason this test exists in this form.
    #[test]
    fn run_embedded_answers_for_every_embedded_id() {
        for id in embedded() {
            match run_embedded(id) {
                Some(Ok(())) => {}
                Some(Err(e)) => panic!("embedded canary `{id}` failed: {e}"),
                None => panic!(
                    "`{id}` is declared in EMBEDDED but `run_embedded` has no arm for it — \
                     auto-verify would report it as uncovered rather than as checked"
                ),
            }
        }
        assert!(
            run_embedded("claude.hook.precompact").is_none(),
            "an id with no embedded canary must answer None, NEVER Err — `Err` blocks the \
             auto-advance, and 'nothing checks this' is not a failure"
        );
    }

    // ── the suite ↔ the matrix ──────────────────────────────────────────────

    /// Every canary module's source, read at compile time. The cross-check below
    /// needs the *call sites*, and a hand-kept list of them is precisely the
    /// drift the check exists to prevent — so the list is derived from the text
    /// instead. Test-only, so nothing extra lands in a release binary; the
    /// `_synthetic` fixtures stay file-loaded for that reason, this does not.
    ///
    /// V40 Phase A: the call sites now live in three files rather than one,
    /// because the assertions moved to the harnesses that own them (locked
    /// decision 17). The list is checked against the registry by
    /// [`every_harness_canary_module_is_scanned`], so a harness whose canary
    /// module is missing here fails the build instead of going unscanned.
    const CANARY_SOURCES: &[(&str, &str)] = &[
        ("harness/canary.rs", include_str!("canary.rs")),
        ("harness/claude/canary.rs", include_str!("claude/canary.rs")),
        (
            "harness/opencode/canary.rs",
            include_str!("opencode/canary.rs"),
        ),
    ];

    /// Every capability id passed to [`row`] anywhere in this file.
    ///
    /// The needle is assembled from pieces on purpose: written as one literal it
    /// would appear in this function's own source and the scan would match itself,
    /// which is how an extractor ends up "finding" ids nobody wrote.
    fn canaried_ids(src: &'static str) -> BTreeSet<&'static str> {
        let needle = concat!("row", "(\"");
        let mut out = BTreeSet::new();
        let mut rest = src;
        while let Some(at) = rest.find(needle) {
            let after = &rest[at + needle.len()..];
            let Some(end) = after.find('"') else { break };
            let (id, tail) = after.split_at(end);
            // A call site, not prose: ids are `[a-z0-9_.]` and the string literal
            // closes immediately before the `)`.
            if !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
                && tail.starts_with("\")")
            {
                out.insert(id);
            }
            rest = tail;
        }
        out
    }

    /// Every registered harness that HAS a canary module is scanned by
    /// [`canaried_ids`].
    ///
    /// The both-directions half of [`CANARY_SOURCES`], and the reason that list
    /// can be a literal at all: a `harness/<id>/canary.rs` on disk that nobody
    /// added here would take its `row(..)` call sites out of the matrix
    /// cross-check silently, which is the exact failure that check exists to
    /// catch. Walked from the registry, so a third harness is covered the day
    /// its directory lands.
    #[test]
    fn every_harness_canary_module_is_scanned() {
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("harness");
        let listed: BTreeSet<&str> = CANARY_SOURCES.iter().map(|(p, _)| *p).collect();
        assert!(
            listed.contains("harness/canary.rs"),
            "the runner's own source dropped out of the scan"
        );
        for h in crate::harness::registry::all() {
            let Some(id) = h.id() else { continue };
            let path = src_root.join(id).join("canary.rs");
            let rel = format!("harness/{id}/canary.rs");
            assert_eq!(
                path.is_file(),
                listed.contains(rel.as_str()),
                "`{rel}` exists on disk = {}, but is listed in CANARY_SOURCES = {} — the two must \
                 agree, or this harness's canary call sites sit outside the matrix cross-check",
                path.is_file(),
                listed.contains(rel.as_str())
            );
        }
        // …and nothing in the list names a file that is gone.
        for (p, _) in CANARY_SOURCES {
            let abs = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(p);
            assert!(
                abs.is_file(),
                "CANARY_SOURCES names `{p}`, which is not in the tree"
            );
        }
    }

    /// The suite and the registry name the same capabilities, in both directions
    /// (design doc § 6).
    ///
    /// A canary id **is** a capability id, so this is a set comparison rather than
    /// a mapping: the ids the registry declares canaried must be exactly the ids
    /// this file drives through [`row`]. A declared canary with no test is a row
    /// that traded a waiver for nothing; a test whose id no row declares is the
    /// suite drifting into checking things nobody wrote down. Positive and negative
    /// twins both call [`row`], and the comparison is over ids, so the duplication
    /// is free.
    ///
    /// Deliberately **not** checked here: that every declared canary also has a
    /// negative twin. The five Tier-C readers have one, but Phase D's live-probe
    /// canaries cover `Behavior` deps where a "renamed field" fixture is
    /// meaningless — recorded rather than assumed, so the omission is a decision
    /// and not an oversight.
    #[test]
    fn canaries_and_the_matrix_agree() {
        let tested: BTreeSet<&str> = CANARY_SOURCES
            .iter()
            .flat_map(|(_, src)| canaried_ids(src))
            .collect();
        // A silently-empty extraction would make everything below vacuously true.
        assert!(
            tested.len() >= 4,
            "the canary-call-site scan found only {tested:?} — it has stopped matching this file's \
             own call sites, and every assertion below is now vacuous"
        );

        let mut declared: BTreeSet<&str> = BTreeSet::new();
        for c in contract::capabilities() {
            if let Some(canary) = c.canary {
                // The join key, asserted for EVERY row rather than only for the
                // ones with a test: `row` cannot catch a row whose canary names
                // some other capability, because nothing would call it.
                assert_eq!(
                    canary, c.id,
                    "capability `{}` declares canary `{canary}` — a canary id IS the capability id, \
                     never a third namespace",
                    c.id
                );
                declared.insert(canary);
            }
        }

        let untested: Vec<&str> = declared.difference(&tested).copied().collect();
        assert!(
            untested.is_empty(),
            "declared canary has no test: {untested:?} carry `canary: Some(..)` in \
             `harness::contract`'s registry but no canary module drives them. Write \
             the canary, or put the waiver back."
        );

        let undeclared: Vec<&str> = tested.difference(&declared).copied().collect();
        assert!(
            undeclared.is_empty(),
            "canary exists outside the matrix: {undeclared:?} are driven by harness/canary.rs but no \
             registry row declares them. Add the row (or set `canary: Some(..)` on it) — the suite \
             must not test dependencies the matrix has not recorded."
        );
    }
    // ── the corpus itself ───────────────────────────────────────────────────

    /// The four keys every `MANIFEST.toml` must carry.
    const MANIFEST_KEYS: [&str; 4] = ["captured_from", "date", "method", "redaction"];

    /// The one directory under a harness that is not a CLI version: the Phase C
    /// drift models. It is checked by the SAME walker rather than skipped by it —
    /// an exemption is how a corpus grows an undated corner — and additionally
    /// must declare [`MODELS_VERSION_KEY`].
    const SYNTHETIC_DIR: &str = "_synthetic";

    /// The fifth key a `_synthetic/` manifest carries: the sibling version
    /// directory whose fixtures it mutates. Checked to be a real directory, so a
    /// drift model cannot outlive the fixture it was derived from — the two must
    /// stay byte-identical apart from the renamed field, and that claim is
    /// unverifiable once the twin is gone.
    const MODELS_VERSION_KEY: &str = "models_version";

    /// `manifest[key]`, trimmed of quotes and whitespace. `""` when the key is
    /// absent — present-but-blank and absent are treated alike by the callers,
    /// which is the point (global principle 5).
    fn manifest_value(manifest: &str, key: &str) -> String {
        manifest
            .lines()
            .map(str::trim_start)
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(|rest| rest.trim().trim_matches('"').trim().to_string())
            .unwrap_or_default()
    }

    /// Every `<harness>/<version>/` directory carries a `MANIFEST.toml` with all
    /// four provenance keys, and at least one fixture beside it. `_synthetic/`
    /// (the drift models) is held to the same rule plus `models_version`.
    ///
    /// Locked decision 4: an anonymous fixture is indistinguishable from a guess.
    /// Without this the corpus silently accumulates files nobody can date, and the
    /// first question during a real breakage — "is this shape still what upstream
    /// sends, or did we invent it in 2026?" — has no answer.
    #[test]
    fn every_fixture_version_dir_has_a_manifest() {
        let root = fixtures_root();
        let harnesses = read_dirs(&root);
        assert!(
            !harnesses.is_empty(),
            "no harness fixtures at all under {}",
            root.display()
        );

        let mut checked = 0usize;
        for harness in harnesses {
            let versions = read_dirs(&harness);
            assert!(
                !versions.is_empty(),
                "{}: a harness directory with no version directory — fixtures are versioned by the \
                 CLI build they were modelled on",
                harness.display()
            );
            for version in versions {
                let manifest_path = version.join("MANIFEST.toml");
                let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
                    panic!(
                        "{}: no readable MANIFEST.toml ({e}) — record captured_from / date / method / \
                         redaction, or delete the fixtures",
                        version.display()
                    )
                });
                for key in MANIFEST_KEYS {
                    // Present-but-blank is absent with extra steps.
                    assert!(
                        manifest_value(&manifest, key).len() > 3,
                        "{}: MANIFEST.toml key `{key}` is missing or blank",
                        manifest_path.display()
                    );
                }
                // The drift models are not a CLI version, so they answer one extra
                // question instead: which version's fixtures did you mutate?
                if version.file_name().is_some_and(|n| n == SYNTHETIC_DIR) {
                    let models = manifest_value(&manifest, MODELS_VERSION_KEY);
                    assert!(
                        !models.is_empty()
                            && version
                                .parent()
                                .is_some_and(|p| p.join(&models).is_dir()),
                        "{}: `{MODELS_VERSION_KEY}` must name a sibling version directory that still \
                         exists (got {models:?}) — a drift model whose twin is gone can no longer be \
                         shown to differ from it in exactly one field",
                        manifest_path.display()
                    );
                }
                let fixtures: Vec<PathBuf> = std::fs::read_dir(&version)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n != "MANIFEST.toml"))
                    .collect();
                assert!(
                    !fixtures.is_empty(),
                    "{}: a manifest with no fixtures beside it",
                    version.display()
                );
                checked += 1;
            }
        }
        assert!(checked >= 2, "expected fixtures for both harnesses");
    }


    /// The embedded copies really are the committed files.
    ///
    /// `include_str!` resolves at compile time and the walker checks the file on
    /// disk, so without this the two could describe different corpora after a
    /// path edit that still compiles (a fixture copied to a new version dir,
    /// say). Cheap, and it keeps "the fixtures are provenance-checked" true of
    /// the bytes the shipped canary actually runs.
    ///
    /// V40 Phase A: driven from the registry rather than from a hand-written
    /// pair list, because each [`Canary`] now carries its own fixture AND that
    /// fixture's path. The old literal list had grown a hole — it never named
    /// `transcript.assistant-text.jsonl` — which is what a hand-kept mirror of
    /// a table does.
    #[test]
    fn the_embedded_fixtures_are_the_committed_files() {
        let mut seen = 0usize;
        for c in all_canaries() {
            assert!(
                !c.fixture_path.trim().is_empty(),
                "canary `{}` declares no fixture path, so nothing can check its embedded bytes",
                c.id
            );
            assert_eq!(
                c.fixture,
                fixture(c.fixture_path),
                "the embedded copy of `{}` is not the file the manifest walker checks",
                c.fixture_path
            );
            seen += 1;
        }
        assert!(
            seen >= 5,
            "only {seen} embedded fixtures were checked — the registry stopped producing canaries \
             and this test is now vacuous"
        );
    }

    /// Immediate sub-directories of `dir`, sorted, ignoring files.
    fn read_dirs(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: unreadable ({e})", dir.display()))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        out.sort();
        out
    }
}
