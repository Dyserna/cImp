//! V12 Phase B — diff → changed-symbols mapping, the input side of
//! `graph_impact` (blast-radius analysis).
//!
//! Given a project root, runs `git diff --unified=0 HEAD` (tracked changes)
//! and `git status --porcelain` (untracked files) — console-suppressed,
//! same convention as every other spawned subprocess in this codebase — then
//! maps the touched line ranges onto the indexed symbol spans they
//! intersect. Files that changed but aren't indexed (docs, configs, or
//! untracked non-code) are reported separately rather than silently dropped.
//! [`changed_symbols`] is deliberately synchronous: the diff/status calls are
//! quick, one-shot per `graph_impact` invocation — unlike `checks::gitls`'s
//! async twin, which sits on the `run_check` hot path.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::error::{AppError, AppResult};

use super::gitcmd::run_git;
use super::index::{DependentHit, GraphIndex, SymbolHit};

/// One (new-side) changed line range within a file, 1-based inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineRange {
    start: u32,
    end: u32,
}

impl LineRange {
    fn intersects(&self, start: u32, end: u32) -> bool {
        self.start <= end && start <= self.end
    }
}

/// Symbols touched by the working-tree diff vs `HEAD`, plus changed files the
/// index doesn't know about. Returned by [`changed_symbols`].
#[derive(Clone, Debug, Default)]
pub struct ChangedSet {
    pub changed: Vec<SymbolHit>,
    /// Changed files with no indexed symbols (docs/configs, or an untracked
    /// file in a language the graph doesn't parse). Deduped, sorted.
    pub unindexed: Vec<String>,
}

/// Working-tree diff's blast radius: the changed symbols themselves plus
/// their transitive dependents (name-keyed — approximate by construction,
/// same convention as `graph_references`). Computed by
/// `GraphService::impact` for the Analyses UI's diff-only mode; the
/// `symbols`-scoped `graph_impact` MCP tool path bypasses this and calls
/// [`GraphIndex::dependents_transitive`] directly with agent-supplied roots.
#[derive(Clone, Debug, Default)]
pub struct ImpactReport {
    pub changed: Vec<SymbolHit>,
    pub dependents: Vec<DependentHit>,
    pub unindexed: Vec<String>,
}

/// Compute [`ChangedSet`] for `root`'s working tree vs `HEAD`, using `index`
/// to resolve each changed line range to the symbols it overlaps. `Err`
/// ([`AppError::NotAGitRepo`]) when `root` isn't a git repository (or `git`
/// isn't on PATH) — the caller renders this as "requires git" guidance. A
/// repo with no commits yet (no `HEAD`) degrades to "no tracked changes"
/// rather than erroring, since untracked files are still reportable.
pub fn changed_symbols(root: &Path, index: &GraphIndex) -> AppResult<ChangedSet> {
    // Confirm this is a git repo up front — distinguishes "not a repo" from
    // "clean tree" (an empty diff is not an error) and from "no HEAD yet"
    // (degrades below rather than failing the whole call).
    if run_git(root, &["rev-parse", "--is-inside-work-tree"]).is_err() {
        return Err(AppError::NotAGitRepo(format!(
            "{} is not a git repository — `graph_impact`'s default (diff-vs-HEAD) mode needs git; \
             pass `symbols` explicitly to analyze specific symbols instead",
            root.display()
        )));
    }

    let mut ranges: HashMap<String, Vec<LineRange>> = HashMap::new();
    // F17: harden the diff invocation like the sibling `git status` below.
    // `core.quotePath=false` stops git C-quoting non-ASCII paths (`"b/caf\303\251.rs"`),
    // and pinning `diff.mnemonicPrefix=false`/`diff.noprefix=false` guarantees the
    // `a/`…`b/` prefixes `parse_diff_new_path` strips — a user's `diff.mnemonicPrefix`
    // (→ `w/`) or `diff.noprefix` config would otherwise mangle the path so it never
    // matches the indexed one, silently dropping those changed symbols from impact.
    if let Ok(diff) = run_git(
        root,
        &[
            "-c",
            "core.quotePath=false",
            "-c",
            "diff.mnemonicPrefix=false",
            "-c",
            "diff.noprefix=false",
            "diff",
            "--unified=0",
            "HEAD",
        ],
    ) {
        parse_unified_diff(&diff, &mut ranges);
    }

    // `-z --untracked-files=all`: without it, `git status --porcelain`
    // collapses a wholly-untracked new directory into one `?? dir/` entry
    // (hiding the individual files a diff-blast-radius scan needs to see)
    // and C-quotes non-ASCII/special paths. `-z` NUL-terminates each record
    // with no quoting, so we split on `\0`, not lines, and never strip
    // quotes — same fix as `checks::gitls::changed_files`.
    let mut whole_file: BTreeSet<String> = BTreeSet::new();
    if let Ok(status) = run_git(
        root,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    ) {
        for part in status.split('\0') {
            if let Some(rest) = part.strip_prefix("?? ") {
                whole_file.insert(rest.replace('\\', "/"));
            }
        }
    }

    let mut changed: Vec<SymbolHit> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unindexed: BTreeSet<String> = BTreeSet::new();

    for (file, file_ranges) in &ranges {
        let outline = index.outline(file)?;
        if outline.is_empty() {
            unindexed.insert(file.clone());
            continue;
        }
        for s in outline {
            if file_ranges
                .iter()
                .any(|r| r.intersects(s.start_line, s.end_line))
                && seen_ids.insert(s.id.clone())
            {
                changed.push(s);
            }
        }
    }
    for file in &whole_file {
        let outline = index.outline(file)?;
        if outline.is_empty() {
            unindexed.insert(file.clone());
            continue;
        }
        for s in outline {
            if seen_ids.insert(s.id.clone()) {
                changed.push(s);
            }
        }
    }

    changed.sort_by(|a, b| a.file.cmp(&b.file).then(a.start_line.cmp(&b.start_line)));
    Ok(ChangedSet {
        changed,
        unindexed: unindexed.into_iter().collect(),
    })
}

/// Parse `git diff --unified=0 HEAD` output into per-file new-side changed
/// line ranges. Tracks the current file via `+++ b/<path>` lines; hunk
/// headers (`@@ -a,b +c,d @@`) contribute `[c, c+d-1]` (`d` defaults to 1
/// when omitted — a 1-line hunk, git's own convention). F18: a pure-deletion
/// hunk (`d == 0`, no surviving new-side line) is NOT dropped — deleting lines
/// from inside a live symbol is a genuine behavior change impact should flag,
/// so it contributes a small range at the deletion point (`c` is the new-file
/// line just before it, `0` if the file head was cut) so the enclosing symbol's
/// span still intersects. A deleted FILE (`+++ /dev/null`) is skipped: nothing
/// left to map to a symbol.
fn parse_unified_diff(diff: &str, out: &mut HashMap<String, Vec<LineRange>>) {
    let mut current: Option<String> = None;
    // Real file headers (`--- a/…` / `+++ b/…`) appear only in the header block
    // that follows a `diff --git` line, before that file's first `@@` hunk.
    // Tracking that block prevents an *in-hunk content* line like `--- foo` or
    // `+++ foo` — which git emits under `--unified=0` for a source line whose
    // text starts with `--`/`++` (e.g. a SQL/Haskell/Lua comment, or a tracked
    // `.diff`/`.patch` file) — from being misread as a file header and
    // clobbering `current` to a bogus path.
    let mut in_header_block = false;
    let mut prev_was_old_header = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header_block = true;
            prev_was_old_header = false;
            // A hunk before the next `+++` (shouldn't happen) must not map to
            // the previous file.
            current = None;
            continue;
        }
        if in_header_block && line.starts_with("--- ") {
            prev_was_old_header = true;
            continue;
        }
        if in_header_block && prev_was_old_header {
            if let Some(rest) = line.strip_prefix("+++ ") {
                current = parse_diff_new_path(rest);
                prev_was_old_header = false;
                in_header_block = false; // header consumed; hunks follow
                continue;
            }
            prev_was_old_header = false;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            in_header_block = false;
            let Some(file) = current.as_ref() else {
                continue;
            };
            if let Some((start, len)) = parse_hunk_new_range(rest) {
                if len > 0 {
                    // Saturating so a malformed/corrupt hunk header with
                    // near-`u32::MAX` values can't overflow-panic — this parser
                    // is documented to tolerate bad input without panicking.
                    out.entry(file.clone()).or_default().push(LineRange {
                        start,
                        end: start.saturating_add(len).saturating_sub(1),
                    });
                } else {
                    // F18: pure deletion. `start` is the new-file line just
                    // before the cut (0 if the head was deleted); mark it and the
                    // line after so a symbol that lost interior lines intersects.
                    let anchor = start.max(1);
                    out.entry(file.clone()).or_default().push(LineRange {
                        start: anchor,
                        end: anchor.saturating_add(1),
                    });
                }
            }
        }
    }
}

/// Extract the project-relative path from a `+++ ` diff header line
/// (`"b/src/foo.rs"` → `"src/foo.rs"`; `"/dev/null"` → `None`, a deletion).
fn parse_diff_new_path(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if rest == "/dev/null" {
        return None;
    }
    Some(rest.strip_prefix("b/").unwrap_or(rest).replace('\\', "/"))
}

/// Parse a hunk header's new-side range from the text after `"@@ "`, e.g.
/// `"-12,3 +15,4 @@ fn foo() {"` → `Some((15, 4))`, `"-1 +1 @@"` → `Some((1, 1))`
/// (comma-less = a single line). `None` if no `+`-prefixed range is present
/// (a malformed/unexpected header — never happens with real git output, but
/// don't panic on it).
fn parse_hunk_new_range(rest: &str) -> Option<(u32, u32)> {
    let plus = rest.split_whitespace().find(|t| t.starts_with('+'))?;
    let spec = plus.trim_start_matches('+');
    let mut parts = spec.splitn(2, ',');
    let start: u32 = parts.next()?.parse().ok()?;
    let len: u32 = match parts.next() {
        Some(s) => s.parse().ok()?,
        None => 1,
    };
    Some((start, len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{parse_file, GraphIndex, Lang};
    use std::process::Command as StdCommand;

    // ── hunk-header / diff parsing ─────────────────────────────────────

    #[test]
    fn parse_hunk_new_range_handles_shapes() {
        // Multi-line addition.
        assert_eq!(
            parse_hunk_new_range("-12,3 +15,4 @@ fn foo() {"),
            Some((15, 4))
        );
        // Comma-less = a single new-side line.
        assert_eq!(parse_hunk_new_range("-1 +1 @@"), Some((1, 1)));
        // Pure deletion: new-side length 0 (insertion point, no surviving lines).
        assert_eq!(parse_hunk_new_range("-5,3 +4,0 @@"), Some((4, 0)));
        // Malformed: no '+' token.
        assert_eq!(parse_hunk_new_range("garbage @@"), None);
    }

    #[test]
    fn parse_diff_new_path_strips_b_prefix_and_flags_deletion() {
        assert_eq!(
            parse_diff_new_path("b/src/foo.rs"),
            Some("src/foo.rs".to_string())
        );
        assert_eq!(parse_diff_new_path("/dev/null"), None);
    }

    #[test]
    fn parse_unified_diff_multi_file_add_and_delete_only_hunks() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,0 +2,3 @@
+x
+y
+z
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -10,2 +9,0 @@
-old1
-old2
diff --git a/src/c.rs b/src/c.rs
--- a/src/c.rs
+++ /dev/null
@@ -1,5 +0,0 @@
-gone
";
        let mut out: HashMap<String, Vec<LineRange>> = HashMap::new();
        parse_unified_diff(diff, &mut out);

        // a.rs: a 3-line addition starting at new-side line 2.
        assert_eq!(
            out.get("src/a.rs"),
            Some(&vec![LineRange { start: 2, end: 4 }])
        );
        // b.rs: F18 — a delete-only hunk (`+9,0`) anchors a small range at the
        // deletion point so the enclosing symbol is still flagged.
        assert_eq!(
            out.get("src/b.rs"),
            Some(&vec![LineRange { start: 9, end: 10 }])
        );
        // c.rs: a deleted FILE (+++ /dev/null) still contributes nothing —
        // there's no surviving symbol to map to.
        assert!(!out.contains_key("src/c.rs"));
    }

    #[test]
    fn delete_only_hunk_at_file_head_anchors_at_line_one() {
        // F18: `+0,0` (head of file cut) clamps the anchor to line 1 rather than 0.
        let diff = "\
diff --git a/src/d.rs b/src/d.rs
--- a/src/d.rs
+++ b/src/d.rs
@@ -1,3 +0,0 @@
-a
-b
-c
";
        let mut out: HashMap<String, Vec<LineRange>> = HashMap::new();
        parse_unified_diff(diff, &mut out);
        assert_eq!(
            out.get("src/d.rs"),
            Some(&vec![LineRange { start: 1, end: 2 }])
        );
    }

    #[test]
    fn parse_unified_diff_ignores_a_content_line_that_looks_like_a_file_header() {
        // An added line whose own content starts with `++` renders in the
        // diff as a `+++ ...` line (git's own leading `+` plus the content's
        // `++`) — without a preceding `--- ` line, this must NOT be misread
        // as a new file header that clobbers `current`.
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,0 +2,2 @@
+++ looks like a header but isn't
+real content
@@ -10,0 +12,1 @@
+more content
";
        let mut out: HashMap<String, Vec<LineRange>> = HashMap::new();
        parse_unified_diff(diff, &mut out);
        // Both hunks still map to src/a.rs — `current` was never clobbered.
        assert_eq!(
            out.get("src/a.rs"),
            Some(&vec![
                LineRange { start: 2, end: 3 },
                LineRange { start: 12, end: 12 }
            ])
        );
    }

    // ── git plumbing + changed_symbols ─────────────────────────────────

    fn git(dir: &Path, args: &[&str]) {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A throwaway git repo + graph index, with `commit_src` committed as
    /// `src/lib.rs` and the working tree containing whatever the caller
    /// writes next.
    fn setup(tag: &str, commit_src: &str) -> (std::path::PathBuf, GraphIndex) {
        let dir = std::env::temp_dir().join(format!("impact-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        // Gitignore the graph store's own db dir — a real project ignores
        // `.cimp/` (see the repo's own `.gitignore`); without this, the
        // freshly-opened `.ckg/graph.db` shows up as an untracked "changed
        // file" and pollutes `unindexed`.
        std::fs::write(dir.join(".gitignore"), ".ckg/\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), commit_src).unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/lib.rs", commit_src, Lang::Rust))
            .expect("index");
        (dir, idx)
    }

    #[test]
    fn not_a_git_repo_is_a_typed_error() {
        let dir = std::env::temp_dir().join(format!("impact-norepo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let err = changed_symbols(&dir, &idx).expect_err("not a repo");
        assert!(matches!(err, AppError::NotAGitRepo(_)), "{err:?}");
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_symbols_maps_edited_line_to_its_symbol() {
        let src = "pub fn a() -> i32 {\n    1\n}\npub fn b() -> i32 {\n    2\n}\n";
        let (dir, idx) = setup("edit", src);
        // Edit b()'s body only.
        let edited = "pub fn a() -> i32 {\n    1\n}\npub fn b() -> i32 {\n    99\n}\n";
        std::fs::write(dir.join("src/lib.rs"), edited).unwrap();

        let set = changed_symbols(&dir, &idx).expect("changed_symbols");
        assert!(
            set.changed.iter().any(|s| s.name == "b"),
            "{:?}",
            set.changed
        );
        assert!(
            !set.changed.iter().any(|s| s.name == "a"),
            "{:?}",
            set.changed
        );
        assert!(set.unindexed.is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_symbols_reports_untracked_whole_file_and_unindexed_docs() {
        let (dir, idx) = setup("untracked", "pub fn a() {}\n");
        // A new Rust file (untracked, whole-file change) plus a doc file the
        // graph doesn't index by symbol (markdown has no `outline` rows here
        // since it wasn't indexed at all).
        std::fs::write(dir.join("src/new.rs"), "pub fn n() {}\n").unwrap();
        idx.index_file_graph(&parse_file("src/new.rs", "pub fn n() {}\n", Lang::Rust))
            .expect("index new");
        std::fs::write(dir.join("NOTES.md"), "# notes\n").unwrap();

        let set = changed_symbols(&dir, &idx).expect("changed_symbols");
        assert!(
            set.changed.iter().any(|s| s.name == "n"),
            "{:?}",
            set.changed
        );
        assert!(
            set.unindexed.iter().any(|f| f == "NOTES.md"),
            "{:?}",
            set.unindexed
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_symbols_untracked_new_directory_sees_individual_files() {
        // A brand-new, wholly-untracked directory with two Rust files must be
        // seen as two individual changed files, not collapsed into a single
        // `?? newmod/` porcelain entry that `changed_symbols` can't map to
        // anything (the `-z --untracked-files=all` fix).
        let (dir, idx) = setup("newdir", "pub fn a() {}\n");
        std::fs::create_dir_all(dir.join("src/newmod")).unwrap();
        std::fs::write(dir.join("src/newmod/x.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(dir.join("src/newmod/y.rs"), "pub fn y() {}\n").unwrap();
        idx.index_file_graph(&parse_file(
            "src/newmod/x.rs",
            "pub fn x() {}\n",
            Lang::Rust,
        ))
        .expect("index x");
        idx.index_file_graph(&parse_file(
            "src/newmod/y.rs",
            "pub fn y() {}\n",
            Lang::Rust,
        ))
        .expect("index y");

        let set = changed_symbols(&dir, &idx).expect("changed_symbols");
        assert!(
            set.changed.iter().any(|s| s.name == "x"),
            "{:?}",
            set.changed
        );
        assert!(
            set.changed.iter().any(|s| s.name == "y"),
            "{:?}",
            set.changed
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_symbols_clean_tree_is_empty_not_an_error() {
        let (dir, idx) = setup("clean", "pub fn a() {}\n");
        let set = changed_symbols(&dir, &idx).expect("changed_symbols");
        assert!(set.changed.is_empty());
        assert!(set.unindexed.is_empty());
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
