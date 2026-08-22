//! Loader for `<exe-dir>/patterns.json` — the user-editable list of
//! prompt-detection substrings consumed by [`PermissionDetector`].
//!
//! On first launch the file doesn't exist; we write the bundled defaults
//! (see [`super::permission::default_patterns`]) so the user can hand-edit
//! to add or refine patterns without rebuilding. A missing or corrupt file
//! falls back to defaults in-memory so detection always works.
//!
//! # Replace-if-pristine reconciliation
//!
//! The file is seeded once and then loaded verbatim forever, so an install
//! made before a defaults change keeps the old patterns even when the shipped
//! ones get strictly better (as when the permission footer moved from the
//! literal `Esc to cancel · Tab to amend` marker to the grammar-based match).
//! On load we therefore compare the parsed pattern list against snapshots of
//! every default set previous releases shipped ([`legacy_default_sets`]). An
//! exact match means the user never touched the file, so it is rewritten with
//! the current defaults. Anything else — one edited substring, an added entry,
//! or already-current content — is loaded verbatim and never rewritten. There
//! is deliberately no version field and no merging of user edits: a file we
//! cannot prove is pristine belongs to the user.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::processing::permission::{default_patterns, PatternSpec, PermissionPattern};

const PATTERNS_FILE_NAME: &str = "patterns.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PatternsFile {
    /// Free-form notes the user can leave in the JSON. Not consumed by the
    /// detector — keeps a place for "how to edit this file" docs that
    /// survive round-trips through serde.
    #[serde(rename = "_doc", skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub patterns: Vec<PermissionPattern>,
}

/// Default file body, including the `_doc` header. Written verbatim on
/// first launch and used as the fallback when load fails.
fn default_file() -> PatternsFile {
    PatternsFile {
        doc: Some(
            "Edit this file to add or refine prompt-detection substrings. \
             Each pattern's `all_of` is a list of substrings that must ALL be \
             present in the rendered terminal tail for the pattern to match; \
             the optional `none_of` list vetoes the match when ANY of its \
             substrings is present (that is how the permission patterns stay \
             off Claude's select menus). \
             `kind` is `permission` or `question`. Set `disabled: true` to \
             keep an entry in the file without enabling it. Capture live \
             chrome with RUST_LOG=perm_capture=debug. Patterns are tested in \
             declaration order; first match per kind wins."
                .to_string(),
        ),
        patterns: default_patterns(),
    }
}

/// Every named era of the shipped `patterns.json`, **newest first**.
///
/// Core owns the labels because they are cImp's own release ranges; the rows
/// under each label belong to the harnesses that shipped them (V40 locked
/// decision 21). The v0.22.0 set is the overwhelmingly likely match, so it is
/// tested first.
///
/// Provenance (see also `scripts/patterns.default.json`, which mirrored these
/// from the v0.7.0 era onward):
/// * `v0.4.0` — 7128de7, 2026-05-08. First shipped set.
/// * `v0.6.3` — 6abe5af/8b2dec1, 2026-06-09. Adds the aider prompts.
/// * `v0.7.0` — 0299b95, 2026-06-10. Real `claude_question` + `claude_working`
///   replace the question template.
/// * `v0.22.0` — 8b4728d, 2026-06-29 (reflowed by rustfmt in 5d3a9fc without
///   content change). Aider prompts out, OpenCode templates in. Shipped
///   through v0.49.1, so this is the set nearly every live install holds.
const LEGACY_ERAS: [&str; 4] = [
    "v0.22.0..v0.49.1",
    "v0.7.0..v0.21.x",
    "v0.6.3..v0.6.x",
    "v0.4.0",
];

/// Every distinct pattern list that shipped as `default_patterns()` in a past
/// release, each labelled with the era it covers. A patterns.json equal to one
/// of these was written by the seeder and never edited, so it can be replaced
/// with the current defaults. The list is append-only: whenever
/// [`default_file`]'s patterns change, the outgoing set is added here.
///
/// **V40 locked decision 21.** This used to be ~100 lines of literal rows —
/// `claude_*`, `opencode_*` **and `aider_*`, a harness cImp retired in V19** —
/// composed by hand in core. The rows now come from the plugin that shipped
/// them (`HarnessPlugin::legacy_permission_patterns`) plus the data-only
/// [`crate::harness::_retired`] slot, and what is left here is the era list and
/// the composition order.
///
/// **The order is the order every shipped set had**: registered harnesses in
/// registry order, then retired ones. It is unambiguous in practice because no
/// era ever carried both OpenCode's rows and aider's — v0.22.0 is exactly the
/// release that swapped one for the other — and it is asserted by
/// [`tests::current_defaults_are_not_a_legacy_set`], which requires the four
/// sets to stay distinct.
///
/// **The append rule is enforced** (M13, 2026-08-05 review) by
/// `current_defaults_match_the_frozen_snapshot`, which holds a literal copy of
/// the current set: changing what the plugins declare turns that test red and
/// its message spells out the two-step fix. Forgetting the append used to be
/// silent, and its cost is delayed: installs that took the interim release end
/// up holding a set matching no snapshot, so they are treated as hand-edited
/// forever and never receive any future default fix (the 199221b bug).
/// Only sets that actually SHIPPED belong here — a default set changed twice
/// within one unreleased development range never reached a user's disk, so it
/// is not a snapshot anyone can hold.
fn legacy_default_sets() -> Vec<(&'static str, Vec<PermissionPattern>)> {
    LEGACY_ERAS
        .into_iter()
        .map(|era| {
            let mut rows: Vec<PermissionPattern> = crate::harness::registry::all()
                .filter_map(|h| h.plugin())
                .flat_map(|p| p.legacy_permission_patterns(era))
                .map(PatternSpec::to_pattern)
                .collect();
            rows.extend(
                crate::harness::_retired::legacy_permission_patterns(era)
                    .into_iter()
                    .map(PatternSpec::to_pattern),
            );
            (era, rows)
        })
        .collect()
}

/// Era label of the shipped default set that `patterns` reproduces verbatim,
/// or `None` when the list was hand-edited (or already equals the current
/// defaults, which are not in the legacy list).
///
/// Only the pattern list participates: the file's top-level `_doc` string
/// changed across releases independently of the patterns and is not evidence
/// of a user edit either way.
fn pristine_legacy_era(patterns: &[PermissionPattern]) -> Option<&'static str> {
    legacy_default_sets()
        .into_iter()
        .find(|(_, set)| set.as_slice() == patterns)
        .map(|(era, _)| era)
}

/// Resolve the file path next to the running executable (same directory
/// as `settings.json`). Returns `None` if `current_exe()` doesn't yield a
/// usable parent, in which case we just use defaults in memory and don't
/// try to write anything.
pub fn patterns_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join(PATTERNS_FILE_NAME))
}

/// Load patterns from disk, writing default content if the file is absent.
/// Always returns a usable `Vec<PermissionPattern>` — a corrupt file is
/// logged and defaults are used. The `disabled` flag is honored as
/// declared.
pub fn load_or_seed() -> Vec<PermissionPattern> {
    let Some(path) = patterns_path() else {
        tracing::warn!("patterns: cannot resolve exe dir; using built-in defaults in memory");
        return default_patterns();
    };
    load_or_seed_at(&path)
}

/// [`load_or_seed`] against an explicit path. Split out so tests can drive the
/// seed / reconcile / verbatim-load branches without touching the exe dir.
fn load_or_seed_at(path: &Path) -> Vec<PermissionPattern> {
    if !path.exists() {
        let body = default_file();
        if let Err(e) = write_file(path, &body) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "patterns: write defaults failed; using in-memory defaults"
            );
        } else {
            tracing::info!(path = %path.display(), "patterns: wrote default file");
        }
        return body.patterns;
    }

    match read_file(path) {
        Ok(file) if file.patterns.is_empty() => {
            // "Empty is not absent" (2026-08-05 review, LOW): a parsed file
            // whose pattern LIST is empty is a non-substantive artifact, not a
            // configuration. It silently disabled the whole fallback detector
            // AND the `claude_working` marker that drives the avatar's Thinking
            // state — with one info-level line as the only trace. Treat it as
            // absent: load the defaults and say so loudly.
            //
            // The file is deliberately NOT rewritten. It is the user's (they
            // may be mid-edit, or mid-`{}` scaffold), and the loader's standing
            // rule is that anything not provably pristine belongs to them. A
            // user who genuinely wants detection off keeps every entry and sets
            // `"disabled": true` — that stays honored, because the list is then
            // non-empty and is loaded verbatim.
            tracing::warn!(
                path = %path.display(),
                "patterns: file parsed but its pattern list is EMPTY — prompt detection and the \
                 avatar's working-state marker would both be dead. Using the built-in defaults \
                 for this run; the file is left untouched (add entries, or set \
                 \"disabled\": true on them, to make the choice explicit)"
            );
            default_patterns()
        }
        Ok(file) => {
            // Replace-if-pristine: a file that still reproduces an older
            // release's defaults exactly was never customized, so the user is
            // strictly better off on the current ones. A failed rewrite is not
            // fatal — the in-memory defaults still apply for this run.
            if let Some(era) = pristine_legacy_era(&file.patterns) {
                let body = default_file();
                let count = body.patterns.len();
                match write_file(path, &body) {
                    Ok(()) => tracing::info!(
                        path = %path.display(),
                        legacy_era = era,
                        count,
                        "patterns: file still held an older release's defaults and was never \
                         edited; replaced with the current defaults"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        legacy_era = era,
                        "patterns: rewrite of a pristine legacy file failed; using the current \
                         defaults in memory"
                    ),
                }
                return body.patterns;
            }
            tracing::info!(
                path = %path.display(),
                count = file.patterns.len(),
                "patterns: loaded"
            );
            file.patterns
        }
        Err(e) => {
            // Unreadable, 0-byte, or malformed. Defaults apply in memory and
            // the file stays exactly as it is: re-seeding would clobber a
            // hand-edit that happens to be mid-save or momentarily truncated,
            // and the loader never rewrites a file it cannot prove pristine.
            // WARN (not info) because from here on detection runs on something
            // other than what the file says.
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "patterns: file could not be read/parsed (0-byte, truncated or malformed JSON) — \
                 using the built-in defaults for this run and leaving the file untouched"
            );
            default_patterns()
        }
    }
}

fn read_file(path: &Path) -> Result<PatternsFile, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    // Editors on Windows may save the hand-edited file as "UTF-8 with BOM";
    // serde_json rejects the BOM, which would silently revert to defaults.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).map_err(|e| format!("parse: {e}"))
}

fn write_file(path: &Path, body: &PatternsFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let text = serde_json::to_string_pretty(body).map_err(|e| format!("serialize: {e}"))?;
    fs::write(path, text).map_err(|e| format!("write: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processing::permission::PatternKind;

    #[test]
    fn default_file_round_trips_through_json() {
        let body = default_file();
        let text = serde_json::to_string_pretty(&body).unwrap();
        let parsed: PatternsFile = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.patterns.len(), body.patterns.len());
        assert_eq!(parsed.doc, body.doc);
        // First pattern is the live permission match.
        assert_eq!(parsed.patterns[0].name, "claude_permission");
        assert_eq!(parsed.patterns[0].kind, PatternKind::Permission);
        assert!(!parsed.patterns[0].disabled);
        // Second pattern: the OR'd permission alternative covering the
        // cancel-only footer shape. Its `none_of` vetoes must survive the
        // round-trip — they are what keeps it off Claude's select menus.
        assert_eq!(parsed.patterns[1].name, "claude_permission_bare");
        assert_eq!(parsed.patterns[1].kind, PatternKind::Permission);
        assert!(!parsed.patterns[1].disabled);
        assert_eq!(parsed.patterns[1].none_of, body.patterns[1].none_of);
        assert!(!parsed.patterns[1].none_of.is_empty());
        // Third is the active AskUserQuestion pattern.
        assert_eq!(parsed.patterns[2].kind, PatternKind::Question);
        assert_eq!(parsed.patterns[2].name, "claude_question");
        assert!(!parsed.patterns[2].disabled);
    }

    #[test]
    fn missing_file_seeds_with_defaults() {
        let dir = std::env::temp_dir().join(format!("cimp_patterns_seed_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PATTERNS_FILE_NAME);
        assert!(!path.exists());
        // Force the seed path explicitly (unit test) — load_or_seed uses
        // current_exe() so we drive write_file directly.
        let body = default_file();
        write_file(&path, &body).unwrap();
        assert!(path.exists());
        let parsed = read_file(&path).unwrap();
        assert_eq!(parsed.patterns.len(), body.patterns.len());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_returns_err() {
        let dir =
            std::env::temp_dir().join(format!("cimp_patterns_corrupt_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PATTERNS_FILE_NAME);
        fs::write(&path, b"{ not valid json").unwrap();
        assert!(read_file(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_default_file_matches_code_defaults() {
        // `scripts/patterns.default.json` is copied verbatim into the full
        // release zip as `bin/patterns.json` so the file is discoverable on
        // a fresh install (the loader otherwise seeds it on first run). This
        // test fails if that committed copy drifts from `default_file()` —
        // regenerate it from the body below when the defaults change.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("patterns.default.json");
        let text = fs::read_to_string(&path).expect("read scripts/patterns.default.json");
        let shipped: PatternsFile =
            serde_json::from_str(&text).expect("parse scripts/patterns.default.json");
        // Compare semantically (re-serialize both) so on-disk whitespace
        // isn't load-bearing — only the content has to match.
        let shipped_norm = serde_json::to_string_pretty(&shipped).unwrap();
        let code_norm = serde_json::to_string_pretty(&default_file()).unwrap();
        assert_eq!(
            shipped_norm, code_norm,
            "scripts/patterns.default.json is out of sync with default_file(); \
             regenerate it from the current defaults"
        );
    }

    #[test]
    fn bom_prefixed_file_still_parses() {
        // Windows editors may save the hand-edited file as UTF-8 with BOM;
        // that must not silently revert the user's patterns to defaults.
        let dir = std::env::temp_dir().join(format!("cimp_patterns_bom_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PATTERNS_FILE_NAME);
        let body = serde_json::to_string(&default_file()).unwrap();
        fs::write(&path, format!("\u{feff}{body}")).unwrap();
        let parsed = read_file(&path).expect("BOM file should parse");
        assert_eq!(parsed.patterns.len(), default_file().patterns.len());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Shorthand for a hand-written snapshot row, kept as a TEST helper after
    /// V40 Phase C moved the production snapshots behind the plugins: what the
    /// reconciliation tests need is an arbitrary row, which is exactly what a
    /// plugin declaration is not.
    fn legacy_pattern(
        name: &str,
        kind: PatternKind,
        all_of: &[&str],
        disabled: bool,
    ) -> PermissionPattern {
        PermissionPattern {
            name: name.to_string(),
            kind,
            all_of: all_of.iter().map(|s| (*s).to_string()).collect(),
            none_of: Vec::new(),
            disabled,
        }
    }

    /// Scratch dir + patterns.json path for the reconciliation tests.
    fn temp_patterns_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("cimp_patterns_{tag}_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PATTERNS_FILE_NAME);
        (dir, path)
    }

    /// A patterns.json body as an older release would have written it: the
    /// era's pattern list under a `_doc` that has since been rewritten.
    fn legacy_body(patterns: Vec<PermissionPattern>) -> PatternsFile {
        PatternsFile {
            doc: Some(
                "Edit this file to add or refine prompt-detection substrings. \
                 Each pattern's `all_of` is a list of substrings that must ALL \
                 be present in the rendered terminal tail for the pattern to \
                 match. `kind` is `permission` or `question`."
                    .to_string(),
            ),
            patterns,
        }
    }

    /// M13 (2026-08-05 review): the enforcement half of the append-only rule in
    /// [`legacy_default_sets`]. `current_defaults_are_not_a_legacy_set` below
    /// only catches the reverse mistake (adding the CURRENT set to the legacy
    /// list); nothing caught the one that actually costs users — changing the
    /// shipped defaults and forgetting to append the outgoing set, which leaves
    /// every install that took the interim release stranded on patterns no
    /// snapshot recognizes.
    ///
    /// The literal below is the frozen copy of the current defaults. When it
    /// fails, read the message: it names both edits.
    #[test]
    fn current_defaults_match_the_frozen_snapshot() {
        // Serialized form of `default_patterns()`, frozen. `none_of` is omitted
        // where empty exactly as the serializer omits it, and absent keys parse
        // back to their defaults — so this round-trips to the same structs.
        const FROZEN: &str = r#"[
          { "name": "claude_permission", "kind": "permission",
            "all_of": ["to cancel ·"],
            "none_of": ["to select", "to navigate"], "disabled": false },
          { "name": "claude_permission_bare", "kind": "permission",
            "all_of": ["to cancel", "1. Yes 2."],
            "none_of": ["to select", "to navigate"], "disabled": false },
          { "name": "claude_question", "kind": "question",
            "all_of": ["Enter to select", "Type something"], "disabled": false },
          { "name": "claude_working", "kind": "working",
            "all_of": ["esc to interrupt"], "disabled": false },
          { "name": "opencode_permission", "kind": "permission",
            "all_of": ["<replace with a substring unique to opencode --mini's permission prompt>"],
            "disabled": true },
          { "name": "opencode_working", "kind": "working",
            "all_of": ["<replace with a substring unique to opencode --mini's working footer>"],
            "disabled": true }
        ]"#;
        let frozen: Vec<PermissionPattern> =
            serde_json::from_str(FROZEN).expect("frozen snapshot parses");
        assert_eq!(
            frozen,
            default_patterns(),
            "\nThe shipped default patterns changed. Two edits are required, in order:\n\
             1. If the OUTGOING set (the FROZEN literal in this test) was ever RELEASED, append \
             it to legacy_default_sets() with its era label — otherwise every install that took \
             that release is treated as hand-edited forever and never gets another default fix. \
             A set that only existed on an unreleased branch reached no user and must NOT be \
             appended (it would just add a snapshot nobody holds).\n\
             2. Update the FROZEN literal here to the new defaults, and regenerate \
             scripts/patterns.default.json (see shipped_default_file_matches_code_defaults).\n"
        );
    }

    #[test]
    fn current_defaults_are_not_a_legacy_set() {
        // If the current defaults ever appeared in the legacy list, every load
        // would rewrite the file for no reason. This is the tripwire for the
        // "append the OUTGOING set" rule in legacy_default_sets().
        assert!(pristine_legacy_era(&default_patterns()).is_none());
        // And the snapshots must stay distinct from each other, or the era
        // label in the log would be arbitrary.
        let sets = legacy_default_sets();
        for (i, (era_a, a)) in sets.iter().enumerate() {
            for (era_b, b) in sets.iter().skip(i + 1) {
                assert_ne!(a, b, "legacy sets {era_a} and {era_b} are identical");
            }
        }
    }

    #[test]
    fn pristine_legacy_file_is_replaced_with_current_defaults() {
        for (era, set) in legacy_default_sets() {
            let (dir, path) = temp_patterns_path("legacy");
            write_file(&path, &legacy_body(set)).unwrap();

            let loaded = load_or_seed_at(&path);
            assert_eq!(loaded, default_patterns(), "{era}: returned patterns");

            // The rewrite must land on disk too, not just in memory.
            let on_disk = read_file(&path).unwrap();
            assert_eq!(on_disk.patterns, default_patterns(), "{era}: disk patterns");
            assert_eq!(on_disk.doc, default_file().doc, "{era}: disk _doc");

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn hand_edited_legacy_file_loads_verbatim() {
        // One tweaked substring is enough to make the file the user's.
        let (dir, path) = temp_patterns_path("edited");
        let mut set = legacy_default_sets().remove(0).1;
        set[0].all_of = vec!["Esc to cancel · Tab to amend · Ctrl+E to explain".to_string()];
        write_file(&path, &legacy_body(set.clone())).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = load_or_seed_at(&path);
        assert_eq!(loaded, set);
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "file must not be rewritten"
        );
        let _ = fs::remove_dir_all(&dir);

        // So is an extra entry appended to an otherwise-pristine set.
        let (dir, path) = temp_patterns_path("extra");
        let mut set = legacy_default_sets().remove(0).1;
        set.push(legacy_pattern(
            "my_own_prompt",
            PatternKind::Permission,
            &["Proceed? [y/N]"],
            false,
        ));
        write_file(&path, &legacy_body(set.clone())).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = load_or_seed_at(&path);
        assert_eq!(loaded, set);
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "file must not be rewritten"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_default_file_is_not_rewritten() {
        let (dir, path) = temp_patterns_path("current");
        // Written compact (not the pretty form write_file emits) so a rewrite
        // would visibly change the bytes.
        fs::write(&path, serde_json::to_string(&default_file()).unwrap()).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = load_or_seed_at(&path);
        assert_eq!(loaded, default_patterns());
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "file must not be rewritten"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_file_without_none_of_parses_and_reconciles() {
        // Files written before `none_of` existed have no such key at all; it
        // must default to empty, which is what the legacy snapshots assume.
        let (dir, path) = temp_patterns_path("nonone");
        let text = r#"{
          "_doc": "old header",
          "patterns": [
            { "name": "claude_permission", "kind": "permission",
              "all_of": ["Esc to cancel · Tab to amend"], "disabled": false },
            { "name": "claude_permission_alt_example", "kind": "permission",
              "all_of": ["<replace with a substring unique to this prompt shape>"],
              "disabled": true },
            { "name": "claude_question", "kind": "question",
              "all_of": ["Enter to select", "Type something"], "disabled": false },
            { "name": "claude_working", "kind": "working",
              "all_of": ["esc to interrupt"], "disabled": false },
            { "name": "opencode_permission", "kind": "permission",
              "all_of": ["<replace with a substring unique to opencode --mini's permission prompt>"],
              "disabled": true },
            { "name": "opencode_working", "kind": "working",
              "all_of": ["<replace with a substring unique to opencode --mini's working footer>"],
              "disabled": true }
          ]
        }"#;
        fs::write(&path, text).unwrap();
        let parsed = read_file(&path).unwrap();
        assert!(parsed.patterns.iter().all(|p| p.none_of.is_empty()));
        // …and that raw v0.22.0-era file is recognized as pristine.
        assert_eq!(
            pristine_legacy_era(&parsed.patterns),
            Some("v0.22.0..v0.49.1")
        );
        assert_eq!(load_or_seed_at(&path), default_patterns());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_seeded_by_load_or_seed_at() {
        let (dir, path) = temp_patterns_path("seedat");
        assert!(!path.exists());
        let loaded = load_or_seed_at(&path);
        assert_eq!(loaded, default_patterns());
        assert!(path.exists());
        assert_eq!(read_file(&path).unwrap().patterns, default_patterns());
        let _ = fs::remove_dir_all(&dir);
    }

    /// "Empty is not absent" (2026-08-05 review, LOW). Parsing an empty list is
    /// still correct at the serde layer — it is the LOADER that must refuse to
    /// run on it: zero patterns kills the fallback permission detector and the
    /// `claude_working` avatar marker with it, and used to do so with an
    /// info-level "loaded" line as the only evidence.
    #[test]
    fn empty_pattern_list_is_non_substantive_and_loads_defaults() {
        // Serde layer: `{}` and an explicit empty array both parse to no
        // patterns (this is what the loader has to catch).
        let parsed: PatternsFile = serde_json::from_str("{}").unwrap();
        assert!(parsed.patterns.is_empty());
        assert!(parsed.doc.is_none());

        for body in ["{}", r#"{"patterns": []}"#, r#"{"_doc":"x","patterns":[]}"#] {
            let (dir, path) = temp_patterns_path("empty");
            fs::write(&path, body).unwrap();
            let before = fs::read(&path).unwrap();
            // Defaults in memory …
            assert_eq!(load_or_seed_at(&path), default_patterns(), "body {body}");
            // … and the user's file is left exactly as they wrote it.
            assert_eq!(
                fs::read(&path).unwrap(),
                before,
                "body {body}: file must not be rewritten"
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// The same discipline for a 0-byte / truncated / malformed file: defaults
    /// in memory, WARN, and the file left alone (it may be a hand-edit caught
    /// mid-save — re-seeding would destroy it).
    #[test]
    fn unparseable_file_loads_defaults_without_touching_the_file() {
        for body in ["", "   ", "{ not valid json", "\u{feff}"] {
            let (dir, path) = temp_patterns_path("unparseable");
            fs::write(&path, body).unwrap();
            let before = fs::read(&path).unwrap();
            assert_eq!(load_or_seed_at(&path), default_patterns(), "body {body:?}");
            assert_eq!(
                fs::read(&path).unwrap(),
                before,
                "body {body:?}: file must not be rewritten"
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// A file that keeps its entries but disables them stays the user's
    /// explicit choice — non-empty, so it loads verbatim (detection off).
    #[test]
    fn all_entries_disabled_is_honored_not_replaced() {
        let (dir, path) = temp_patterns_path("disabled");
        let mut set = default_patterns();
        for p in &mut set {
            p.disabled = true;
        }
        write_file(
            &path,
            &PatternsFile {
                doc: None,
                patterns: set.clone(),
            },
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(load_or_seed_at(&path), set);
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }
}
