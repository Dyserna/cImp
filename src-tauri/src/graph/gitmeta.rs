//! V12 Phase D — git-aware context: churn metadata collection.
//!
//! Commit history is free, dense documentation the ranker otherwise ignores.
//! This module collects **file-level** churn (last touch time, last commit
//! subject, touch count) from `git log`, bounded to a 90-day window (no
//! full-history walk) — never per-line blame, just per-file. [`collect`] does
//! one spawn for a full pass (rebuild); [`collect_for`] does one spawn per
//! file for the small watcher-batch case. Both degrade to an empty result
//! (never an error) when `root` isn't a git repository: the feature is simply
//! absent, same convention as `graph::impact`'s diff mode uses for its own
//! (harder) failure case.
//!
//! Deliberately synchronous — both spawns (here and in `graph::impact`, which
//! shares this module's [`super::gitcmd::run_git`] helper) are called from
//! sync contexts (the rebuild thread, the watcher thread), never the async
//! runtime.

use std::collections::HashMap;
use std::path::Path;

use crate::error::AppResult;

use super::gitcmd::run_git;

/// Record separator between commits in [`collect`]'s `git log` format string
/// — a control byte vanishingly unlikely to appear in a real commit subject
/// (git doesn't strictly forbid `\x01` in a message). If a malformed subject
/// ever does contain one, [`parse_log_output`] simply drops that record —
/// no need for a stricter guarantee than "safe, not silently wrong".
const RECORD_SEP: char = '\x01';

/// One file's git churn, as stored in the `commit_touch` relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChurn {
    pub file: String,
    /// Unix seconds (git's `%ct`, same unit `git log` reports) of the newest
    /// commit that touched this file within the collection window.
    pub last_ts: i64,
    pub last_subject: String,
    /// Number of commits touching this file within the 90-day window. Only
    /// [`collect`] (the full pass) computes this precisely; [`collect_for`]'s
    /// per-file incremental spawn doesn't re-walk the whole window, so it
    /// reports `1` — a deliberate approximation that self-heals at the next
    /// full rebuild (churn is git-derived and fully repopulated each pass).
    pub touches_90d: u32,
}

/// Current unix time in seconds — the same unit as git's `%ct`, so callers
/// can diff a stored `last_ts` against "now" without a unit conversion.
pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Render `event_ts` relative to `now_ts` as a short human tag (`"3d ago"`,
/// `"2h ago"`, `"5mo ago"`) for context trailers and `graph_recent_changes`
/// rows. Never negative (a clock skew or "now" computed slightly before the
/// stored commit time clamps to `"just now"`).
pub fn relative_age(now_ts: i64, event_ts: i64) -> String {
    let secs = (now_ts - event_ts).max(0);
    let days = secs / 86_400;
    if days == 0 {
        let hours = secs / 3_600;
        if hours == 0 {
            "just now".to_string()
        } else {
            format!("{hours}h ago")
        }
    } else if days < 30 {
        format!("{days}d ago")
    } else if days < 365 {
        format!("{}mo ago", days / 30)
    } else {
        format!("{}y ago", days / 365)
    }
}

/// Truncate a commit subject to at most `max_chars`, char-safe, appending `…`
/// when cut. Used everywhere a subject rides along a context trailer or tool
/// row so one very long commit message can't blow a token budget.
pub fn truncate_subject(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Full 90-day churn pass: one spawn of `git log --since=90.days --name-only
/// --format=%x01%ct%x09%s`, parsed in a single pass and aggregated per file.
/// Not a git repository (or `git` not on PATH) ⇒ `Ok(vec![])` — the feature is
/// simply absent, not an error; everything else keeps working.
pub fn collect(root: &Path) -> AppResult<Vec<FileChurn>> {
    if !is_git_repo(root) {
        return Ok(Vec::new());
    }
    // `%x01`/`%x09` are git pretty-format hex-byte escapes (NOT Rust format
    // specifiers) — git itself emits the raw 0x01/0x09 bytes into stdout,
    // which [`parse_log_output`] then splits on. Passed as literal text so
    // there's no ambiguity about how the byte reaches the child process's
    // argv on any platform.
    let out = match run_git(
        root,
        &[
            "log",
            "--since=90.days",
            "--name-only",
            "--format=%x01%ct%x09%s",
        ],
    ) {
        Ok(text) => text,
        // No commits yet, or some other transient failure — degrade to
        // "nothing collected" rather than propagating a hard error.
        Err(_) => return Ok(Vec::new()),
    };
    Ok(aggregate(parse_log_output(&out)))
}

/// Incremental churn refresh for a small set of `files` (a watcher batch): one
/// `git log -1 --format=%ct%x09%s -- <file>` spawn per file. A file with no
/// history yet (untracked, or outside the 90-day format's scope entirely — `-1`
/// has no `--since` bound, so this only misses files that have literally never
/// been committed) is silently skipped. `touches_90d` is always `1` here (see
/// [`FileChurn::touches_90d`]'s doc) — the next full [`collect`] pass restores
/// the precise count. Not a git repo ⇒ `Ok(vec![])`, same as [`collect`].
pub fn collect_for(root: &Path, files: &[String]) -> AppResult<Vec<FileChurn>> {
    if files.is_empty() || !is_git_repo(root) {
        return Ok(Vec::new());
    }
    // Bound the per-file `git log` spawns. This runs on the watcher/reindex
    // thread, and one process spawn per file (tens of ms each on Windows)
    // stalls it for seconds on a mass rename/branch-switch batch — which then
    // delays draining the bounded event channel and can itself trigger the
    // overflow → full-rebuild path. Beyond the cap, skip the incremental churn
    // refresh; the next full `collect` (or the rebuild a large batch tends to
    // trigger) restores precise churn anyway.
    const MAX_INCREMENTAL_FILES: usize = 128;
    if files.len() > MAX_INCREMENTAL_FILES {
        tracing::debug!(
            files = files.len(),
            cap = MAX_INCREMENTAL_FILES,
            "gitmeta: batch too large for incremental churn refresh — deferring to next full collect"
        );
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for file in files {
        // Normalize to git's forward-slash convention ONCE, then use the same
        // value for both the pathspec we query and the key we store — so
        // `collect_for` can't key a file differently from the full `collect`
        // pass (whose keys come from git's own forward-slash `--name-only`
        // output). The sole caller already passes repo-relative paths; this
        // keeps that a local guarantee rather than an unstated precondition.
        let file = normalize_path(file);
        // `--literal-pathspecs` (a global git flag, so it precedes `log`):
        // everything after `--` is a pathspec with fnmatch glob semantics by
        // default, so a filename with metacharacters — e.g. a Next.js/
        // SvelteKit route file literally named `[id].tsx` — would match the
        // one-char class `[id]` (a sibling `i.tsx`'s history!) instead of
        // itself. The full `collect` pass uses no pathspec and is unaffected.
        let Ok(text) = run_git(
            root,
            &[
                "--literal-pathspecs",
                "log",
                "-1",
                "--format=%ct%x09%s",
                "--",
                &file,
            ],
        ) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue; // never committed — nothing to report
        }
        if let Some((last_ts, last_subject)) = parse_header_line(text) {
            out.push(FileChurn {
                file,
                last_ts,
                last_subject,
                touches_90d: 1,
            });
        }
    }
    Ok(out)
}

/// Parse `git log --name-only --format=%x01%ct%x09%s` output into
/// `(commit_ts, subject, files)` triples, newest-first (git's own order).
/// Each record starts with [`RECORD_SEP`], followed by a `<ct>\t<subject>`
/// header line, a blank separator line, then zero or more touched file paths
/// up to the next record marker. A subject containing literal tab characters
/// is preserved verbatim (only the FIRST tab on the header line splits
/// timestamp from subject).
pub fn parse_log_output(output: &str) -> Vec<(i64, String, Vec<String>)> {
    let mut out = Vec::new();
    for record in output.split(RECORD_SEP) {
        if record.trim().is_empty() {
            continue; // leading empty chunk before the first separator
        }
        let mut lines = record.lines();
        let Some(header) = lines.next() else { continue };
        let Some((ts, subject)) = parse_header_line(header) else {
            continue;
        };
        let files: Vec<String> = lines
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(normalize_path)
            .collect();
        out.push((ts, subject, files));
    }
    out
}

/// Parse one `<ct>\t<subject>` header line (splitting only on the FIRST tab,
/// so a subject that itself contains tabs is preserved intact).
fn parse_header_line(line: &str) -> Option<(i64, String)> {
    let tab = line.find('\t')?;
    let ts: i64 = line[..tab].trim().parse().ok()?;
    Some((ts, line[tab + 1..].to_string()))
}

/// Aggregate parsed `(ts, subject, files)` records — newest-first — into one
/// [`FileChurn`] per file: `touches_90d` counts every commit touching it,
/// `last_ts`/`last_subject` come from the newest. A rename (git's default
/// `--name-only` reports it as a delete of the old path + add of the new one,
/// two separate lines) is tolerated as two independent file touches — no
/// special-casing needed.
fn aggregate(records: Vec<(i64, String, Vec<String>)>) -> Vec<FileChurn> {
    let mut map: HashMap<String, FileChurn> = HashMap::new();
    for (ts, subject, files) in records {
        for file in files {
            let entry = map.entry(file.clone()).or_insert_with(|| FileChurn {
                file,
                last_ts: ts,
                last_subject: subject.clone(),
                touches_90d: 0,
            });
            entry.touches_90d += 1;
            // Records arrive newest-first, so the first hit for a file is
            // already the newest — this guard also makes aggregation order-
            // independent for callers/tests that don't preserve git's order.
            if ts > entry.last_ts {
                entry.last_ts = ts;
                entry.last_subject = subject.clone();
            }
        }
    }
    let mut out: Vec<FileChurn> = map.into_values().collect();
    out.sort_by(|a, b| a.file.cmp(&b.file));
    out
}

/// Forward-slash-normalize a path (git's own on-disk convention; defensive
/// against a Windows-style separator slipping through).
fn normalize_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// `true` iff `root` is inside a git working tree (probed once via `git
/// rev-parse --is-inside-work-tree`, same convention as `graph::impact`).
fn is_git_repo(root: &Path) -> bool {
    run_git(root, &["rev-parse", "--is-inside-work-tree"]).is_ok()
}

#[cfg(test)]
mod tests {
    use crate::testutil::git;
    use super::*;

    // ── pure parser tests (no git needed) ─────────────────────────────────

    #[test]
    fn parse_log_output_handles_multi_file_commit() {
        let out = "\x011700000000\tfeat: add widget\n\nsrc/a.rs\nsrc/b.rs\n";
        let records = parse_log_output(out);
        assert_eq!(records.len(), 1);
        let (ts, subject, files) = &records[0];
        assert_eq!(*ts, 1700000000);
        assert_eq!(subject, "feat: add widget");
        assert_eq!(files, &vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
    }

    #[test]
    fn parse_log_output_splits_multiple_commits_on_record_separator() {
        let out = "\x011700000200\tnewer commit\n\nsrc/a.rs\n\x011700000100\tolder commit\n\nsrc/b.rs\nsrc/c.rs\n";
        let records = parse_log_output(out);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].0, 1700000200);
        assert_eq!(records[0].1, "newer commit");
        assert_eq!(records[0].2, vec!["src/a.rs".to_string()]);
        assert_eq!(records[1].0, 1700000100);
        assert_eq!(
            records[1].2,
            vec!["src/b.rs".to_string(), "src/c.rs".to_string()]
        );
    }

    #[test]
    fn parse_log_output_preserves_tabs_in_subject() {
        // Only the FIRST tab splits ts from subject; a subject with a literal
        // tab (rare, but not impossible — e.g. pasted from a table) survives.
        let out = "\x011700000000\tfix:\tcap retry at 30s\n\nsrc/a.rs\n";
        let records = parse_log_output(out);
        assert_eq!(records[0].1, "fix:\tcap retry at 30s");
    }

    #[test]
    fn parse_log_output_ignores_malformed_header() {
        // No tab at all → header can't split → the whole record is skipped.
        let out = "\x01garbage-no-tab\n\nsrc/a.rs\n";
        assert!(parse_log_output(out).is_empty());
    }

    #[test]
    fn aggregate_counts_touches_and_keeps_newest_subject() {
        let records = vec![
            (
                200,
                "second touch".to_string(),
                vec!["src/a.rs".to_string()],
            ),
            (
                100,
                "first touch".to_string(),
                vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            ),
        ];
        let churn = aggregate(records);
        let a = churn.iter().find(|c| c.file == "src/a.rs").unwrap();
        assert_eq!(a.touches_90d, 2);
        assert_eq!(a.last_ts, 200);
        assert_eq!(a.last_subject, "second touch");
        let b = churn.iter().find(|c| c.file == "src/b.rs").unwrap();
        assert_eq!(b.touches_90d, 1);
        assert_eq!(b.last_ts, 100);
    }

    #[test]
    fn aggregate_is_order_independent_for_newest_pick() {
        // Even if records arrive oldest-first (not git's actual order), the
        // `ts > entry.last_ts` guard still finds the true newest.
        let records = vec![
            (100, "older".to_string(), vec!["src/a.rs".to_string()]),
            (200, "newer".to_string(), vec!["src/a.rs".to_string()]),
        ];
        let churn = aggregate(records);
        assert_eq!(churn[0].last_ts, 200);
        assert_eq!(churn[0].last_subject, "newer");
    }

    #[test]
    fn relative_age_buckets() {
        let now = 1_000_000i64;
        assert_eq!(relative_age(now, now), "just now");
        assert_eq!(relative_age(now, now - 3 * 3_600), "3h ago");
        assert_eq!(relative_age(now, now - 3 * 86_400), "3d ago");
        assert_eq!(relative_age(now, now - 60 * 86_400), "2mo ago");
        assert_eq!(relative_age(now, now - 400 * 86_400), "1y ago");
        // Future timestamp (clock skew) never goes negative.
        assert_eq!(relative_age(now, now + 10_000), "just now");
    }

    #[test]
    fn truncate_subject_caps_and_marks_cut() {
        assert_eq!(truncate_subject("short", 60), "short");
        let long = "x".repeat(80);
        let cut = truncate_subject(&long, 60);
        assert_eq!(cut.chars().count(), 60);
        assert!(cut.ends_with('…'));
    }

    // ── real-git integration (collect / collect_for / non-repo degrade) ───

    #[test]
    fn collect_not_a_repo_returns_empty_not_an_error() {
        let dir = std::env::temp_dir().join(format!("gitmeta-norepo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let churn = collect(&dir).expect("collect never errors on a non-repo");
        assert!(churn.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_end_to_end_counts_touches_and_picks_newest_subject() {
        let dir = std::env::temp_dir().join(format!("gitmeta-collect-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);

        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init: a and b"]);

        std::fs::write(dir.join("a.rs"), "fn a() { 1 }\n").unwrap();
        git(&dir, &["add", "a.rs"]);
        git(&dir, &["commit", "-q", "-m", "fix: a returns 1"]);

        let churn = collect(&dir).expect("collect");
        let a = churn
            .iter()
            .find(|c| c.file == "a.rs")
            .expect("a.rs present");
        assert_eq!(a.touches_90d, 2, "{churn:?}");
        assert_eq!(a.last_subject, "fix: a returns 1");
        let b = churn
            .iter()
            .find(|c| c.file == "b.rs")
            .expect("b.rs present");
        assert_eq!(b.touches_90d, 1);
        assert_eq!(b.last_subject, "init: a and b");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_for_incremental_lookup_matches_full_collect() {
        let dir = std::env::temp_dir().join(format!("gitmeta-incr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init: a"]);

        let churn = collect_for(&dir, &["a.rs".to_string()]).expect("collect_for");
        assert_eq!(churn.len(), 1);
        assert_eq!(churn[0].file, "a.rs");
        assert_eq!(churn[0].last_subject, "init: a");
        assert_eq!(
            churn[0].touches_90d, 1,
            "incremental is a best-effort count of 1"
        );

        // A file with no history at all is silently skipped, not an error.
        let none = collect_for(&dir, &["never-committed.rs".to_string()]).expect("collect_for");
        assert!(none.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: without `--literal-pathspecs`, the pathspec `[id].tsx` is
    /// parsed as the one-char glob class `[id]` + `.tsx`, so `git log -1 --
    /// pages/[id].tsx` returns a sibling `i.tsx`'s history (or nothing) —
    /// wrong churn for every Next.js/SvelteKit-style bracketed route file.
    #[test]
    fn collect_for_treats_bracketed_filenames_literally() {
        let dir = std::env::temp_dir().join(format!("gitmeta-glob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("pages")).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);

        // The glob-decoy sibling first, in its own commit: `[id]` matches `i`.
        std::fs::write(dir.join("pages/i.tsx"), "export default 1\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "feat: i sibling"]);
        std::fs::write(dir.join("pages/[id].tsx"), "export default 2\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "feat: id route"]);

        let churn = collect_for(&dir, &["pages/[id].tsx".to_string()]).expect("collect_for");
        assert_eq!(churn.len(), 1, "{churn:?}");
        assert_eq!(churn[0].file, "pages/[id].tsx");
        assert_eq!(
            churn[0].last_subject, "feat: id route",
            "the bracketed file's OWN commit, not the glob-matched sibling's"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_for_empty_files_or_non_repo_is_empty() {
        let dir = std::env::temp_dir().join(format!("gitmeta-incr-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        assert!(collect_for(&dir, &[]).expect("collect_for").is_empty());
        let _ = std::fs::remove_dir_all(&dir);

        let norepo =
            std::env::temp_dir().join(format!("gitmeta-incr-norepo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&norepo).unwrap();
        assert!(collect_for(&norepo, &["a.rs".to_string()])
            .expect("collect_for")
            .is_empty());
        let _ = std::fs::remove_dir_all(&norepo);
    }
}
