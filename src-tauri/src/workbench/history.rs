//! Commit history queries for the Workbench's "Session commits" and
//! "Git graph" sections — read-only `git log`/`git show` wrappers on the
//! §0.2 git harness. Unlike [`super::worktree`] nothing here ever mutates
//! the repo: every entry point is a query, so there are no safety
//! invariants beyond argument validation ([`validate_hash`] keeps a
//! frontend-supplied commit id from smuggling in a flag or a path).
//!
//! Session ↔ commit association is purely temporal: a session "owns" the
//! commits whose committer timestamp falls inside its
//! `started_ms..=last_ms` window (the graph memory's `session` relation).
//! That is deliberately approximate — cheap, needs no per-commit metadata,
//! and matches how sessions actually run (the agent commits while the
//! session is live). Commits are drawn from `--all` so work on worktree
//! branches (`cimp/<slug>`) counts too.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

use super::git::{self, GitCtx};

/// Record/field separators for the `--pretty` format below — control chars
/// that can't appear in git metadata (same trick as `graph::gitmeta`). The
/// free-form body field is LAST so stray newlines in it can't split a
/// record.
const FIELD_SEP: char = '\x02';
const RECORD_SEP: char = '\x01';

/// Backstop on how many commits a single log walk parses — far above any
/// realistic session window, and a sane upper bound for the graph view.
pub const MAX_LOG_COMMITS: usize = 5000;

/// `git log` / `git show` can walk a lot of history on large repos — give
/// them the same generous budget as the worktree module's bulk operations.
const LOG_TIMEOUT: Duration = Duration::from_secs(60);

/// One commit, as both the Session-commits row and the Git-graph node.
/// `parents`/`refs` only matter to the graph view but are cheap to carry
/// everywhere (one shared parse).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CommitInfo {
    pub hash: String,
    pub short: String,
    /// Parent hashes, oldest-first as git reports them (first parent first).
    pub parents: Vec<String>,
    /// Committer timestamp, epoch ms.
    pub ts_ms: i64,
    pub author: String,
    /// Decorations from `%D`, split on `", "` — e.g. `"HEAD -> develop"`,
    /// `"tag: v0.40.5"`, `"origin/develop"`. Empty for undecorated commits.
    pub refs: Vec<String>,
    pub subject: String,
    /// Full body (everything after the subject line), trimmed. Often empty.
    pub body: String,
    /// True when this commit was caught LIVE from the session's transcript
    /// (the OOB tap saw the `git commit` tool call — exact provenance),
    /// rather than merely falling inside the session's time window. Always
    /// false outside the session-commits query (e.g. the git graph).
    pub tracked: bool,
}

/// One session's time window for [`commit_counts`] — mirrors the frontend's
/// Sessions-card rows (`SessionUsageRow.started_ms/last_ms`).
#[derive(Clone, Debug, Deserialize)]
pub struct SessionWindow {
    pub session_id: String,
    pub from_ms: i64,
    pub to_ms: i64,
}

/// Does `commit` (full hash) match any recorded hash? Recorded hashes come
/// from git's own commit summary output — usually the short form — so match
/// by prefix.
fn matches_recorded(full_hash: &str, recorded: &[String]) -> bool {
    recorded.iter().any(|h| !h.is_empty() && full_hash.starts_with(h.as_str()))
}

/// The `workbench_git_graph` payload: the current branch (or `None` when
/// detached/unborn) plus up to `limit` commits in topological order
/// (children strictly before parents — what the frontend's lane layout
/// requires).
#[derive(Clone, Debug, Serialize)]
pub struct GitGraph {
    pub head: Option<String>,
    pub commits: Vec<CommitInfo>,
    /// True when the walk stopped at `limit` with history left over.
    pub truncated: bool,
}

/// A frontend-supplied commit id must look like one before it's passed to
/// `git show` — hex only, so it can never be parsed as a flag, a path, or a
/// revision expression (`HEAD~3`, `:/text`, …).
fn validate_hash(hash: &str) -> AppResult<()> {
    let ok = (4..=40).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err(AppError::Workbench(format!("not a commit hash: {hash:?}")))
    }
}

/// The full per-commit record ([`parse_log`]'s input): `%x02`/`%x01` are git
/// pretty-format hex-byte escapes (same trick as `graph::gitmeta`) — git
/// emits the separator bytes itself, so no control characters ever cross the
/// process-argument boundary. Body last (multiline).
const FULL_PRETTY: &str = "--pretty=format:%H%x02%h%x02%P%x02%ct%x02%an%x02%D%x02%s%x02%b%x01";

/// hash + committer-time only — the count path never reads subject/body/refs,
/// so it shouldn't pay to transport or parse them.
const TIMES_PRETTY: &str = "--pretty=format:%H%x02%ct%x01";

/// Run one `git log --all` walk and return raw stdout; `None` for a repo
/// with zero commits (which exits non-zero on some git versions — that's
/// "no commits", not a failure).
async fn run_log(root: &Path, max: usize, topo: bool, pretty: &str) -> AppResult<Option<String>> {
    let ctx = GitCtx::discover(root);
    let max_arg = format!("--max-count={max}");
    let mut args = vec!["log", "--all", "--no-color", &max_arg, pretty];
    if topo {
        args.insert(2, "--topo-order");
    }
    let out = git::run(&ctx, &args, Some(LOG_TIMEOUT)).await?;
    if !out.success() {
        let err = out.stderr.trim();
        if err.contains("does not have any commits") || err.contains("bad default revision") {
            return Ok(None);
        }
        return Err(AppError::Workbench(format!("git log failed: {err}")));
    }
    Ok(Some(out.stdout))
}

/// Shared full log walk: up to `max` commits from every ref, parsed into
/// [`CommitInfo`]s, plus whether history was ACTUALLY cut off. `topo`
/// switches to `--topo-order` (the graph view); otherwise git's default
/// reverse-chronological order is kept (session windows). The walk asks for
/// `max + 1` commits so "exactly max exist" and "more than max exist" are
/// distinguishable — a repo whose entire history fits is never reported as
/// truncated.
async fn log_commits(root: &Path, max: usize, topo: bool) -> AppResult<(Vec<CommitInfo>, bool)> {
    let Some(raw) = run_log(root, max + 1, topo, FULL_PRETTY).await? else {
        return Ok((Vec::new(), false));
    };
    let mut commits = parse_log(&raw);
    let truncated = commits.len() > max;
    commits.truncate(max);
    Ok((commits, truncated))
}

/// Lightweight walk for the count path: `(full_hash, ts_ms)` per commit plus
/// the same over-fetch truncation signal as [`log_commits`]. Public so
/// `WorkbenchService` can cache the walk behind a short TTL.
pub async fn log_commit_times(root: &Path, max: usize) -> AppResult<(Vec<(String, i64)>, bool)> {
    let Some(raw) = run_log(root, max + 1, false, TIMES_PRETTY).await? else {
        return Ok((Vec::new(), false));
    };
    let mut out = Vec::new();
    for record in raw.split(RECORD_SEP) {
        let record = record.trim_start_matches(['\n', '\r']);
        if record.is_empty() {
            continue;
        }
        let mut f = record.splitn(2, FIELD_SEP);
        let (Some(hash), Some(ts)) = (f.next(), f.next()) else { continue };
        out.push((
            hash.to_string(),
            ts.trim().parse::<i64>().unwrap_or(0).saturating_mul(1000),
        ));
    }
    let truncated = out.len() > max;
    out.truncate(max);
    Ok((out, truncated))
}

/// Parse the `%x01`/`%x02`-separated output of [`log_commits`]'s format.
fn parse_log(raw: &str) -> Vec<CommitInfo> {
    let mut out = Vec::new();
    for record in raw.split(RECORD_SEP) {
        // `format:` joins records with a newline AFTER our %x01 terminator.
        let record = record.trim_start_matches(['\n', '\r']);
        if record.is_empty() {
            continue;
        }
        let mut f = record.splitn(8, FIELD_SEP);
        let (Some(hash), Some(short), Some(parents), Some(ts), Some(author), Some(refs), Some(subject)) = (
            f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next(),
        ) else {
            continue; // malformed record — skip, never panic on git output
        };
        let body = f.next().unwrap_or("");
        out.push(CommitInfo {
            hash: hash.to_string(),
            short: short.to_string(),
            parents: parents.split_whitespace().map(str::to_string).collect(),
            ts_ms: ts.trim().parse::<i64>().unwrap_or(0).saturating_mul(1000),
            author: author.to_string(),
            refs: refs
                .split(", ")
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            subject: subject.to_string(),
            body: body.trim().to_string(),
            tracked: false,
        });
    }
    out
}

/// The `session_commits` payload: the commit list plus whether the log walk
/// hit its cap before reaching the requested window's start (older commits
/// may then be missing — the frontend shows a note instead of silently
/// under-reporting).
#[derive(Clone, Debug, Serialize)]
pub struct SessionCommits {
    pub commits: Vec<CommitInfo>,
    pub truncated: bool,
}

/// The Session-commits list, newest first: the UNION of commits caught live
/// from the session's transcript (`recorded` — hash prefixes from the graph
/// memory's `session_commit` relation, flagged `tracked`) and commits whose
/// committer time falls inside `from_ms..=to_ms` (the fallback that also
/// covers manual commits, pre-provenance sessions, and OpenCode).
pub async fn session_commits(
    root: &Path,
    from_ms: i64,
    to_ms: i64,
    recorded: &[String],
) -> AppResult<SessionCommits> {
    let (commits, hit_cap) = log_commits(root, MAX_LOG_COMMITS, false).await?;
    // The walk is newest-first; if it was cut off AND the window starts
    // before the oldest commit scanned, older window matches may be missing.
    let oldest_scanned = commits.iter().map(|c| c.ts_ms).min().unwrap_or(i64::MIN);
    let truncated = hit_cap && from_ms < oldest_scanned;
    let commits = commits
        .into_iter()
        .filter_map(|mut c| {
            let tracked = matches_recorded(&c.hash, recorded);
            let in_window = c.ts_ms >= from_ms && c.ts_ms <= to_ms;
            if !tracked && !in_window {
                return None;
            }
            c.tracked = tracked;
            Some(c)
        })
        .collect();
    Ok(SessionCommits { commits, truncated })
}

/// Per-session commit counts for the Sessions card's button state — pure
/// counting over a [`log_commit_times`] walk (hash + time only; the caller
/// caches the walk, see `WorkbenchService::session_commit_counts`). Same
/// union semantics as [`session_commits`]: a commit counts if it was
/// recorded for the session OR falls inside its window. Counts are
/// best-effort beyond the walk cap (the badge is advisory; the
/// Session-commits view itself surfaces truncation).
pub fn commit_counts_from(
    commits: &[(String, i64)],
    windows: &[SessionWindow],
    recorded: &HashMap<String, Vec<String>>,
) -> HashMap<String, u32> {
    let mut out: HashMap<String, u32> =
        windows.iter().map(|w| (w.session_id.clone(), 0)).collect();
    for (hash, ts_ms) in commits {
        for w in windows {
            let hashes = recorded.get(&w.session_id).map(|v| v.as_slice()).unwrap_or(&[]);
            if (*ts_ms >= w.from_ms && *ts_ms <= w.to_ms) || matches_recorded(hash, hashes) {
                if let Some(n) = out.get_mut(&w.session_id) {
                    *n += 1;
                }
            }
        }
    }
    out
}

/// One commit vs. its first parent (the whole commit for a root commit),
/// parsed into the same [`super::diff::FileDiff`] shape every other diff
/// surface renders. Read-only — no revert applies to a historical commit.
pub async fn commit_diff(root: &Path, hash: &str, context: u32) -> AppResult<Vec<super::diff::FileDiff>> {
    validate_hash(hash)?;
    let ctx = GitCtx::discover(root);
    let unified = format!("--unified={}", context.min(super::diff::MAX_CONTEXT));
    // `-m --first-parent` pins a merge commit's diff to its first parent
    // (the classic "what did the merge bring in" view) instead of the
    // combined `@@@` format `parse_unified` doesn't speak.
    let out = git::run(
        &ctx,
        &[
            "show", "--no-color", "--format=", &unified, "--first-parent", "-m",
            "--no-renames", hash,
        ],
        Some(LOG_TIMEOUT),
    )
    .await?;
    if !out.success() {
        return Err(AppError::Workbench(format!(
            "git show {hash} failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(super::diff::parse_unified(&out.stdout))
}

/// The Git-graph section's payload: every ref's history in topological
/// order, capped at `limit`, plus the current branch name. `truncated` is
/// exact (over-fetch by one in [`log_commits`]) — a repo whose whole history
/// fits in `limit` is never flagged.
pub async fn git_graph(root: &Path, limit: usize) -> AppResult<GitGraph> {
    let limit = limit.clamp(1, MAX_LOG_COMMITS);
    let (commits, truncated) = log_commits(root, limit, true).await?;
    let ctx = GitCtx::discover(root);
    let head = match git::run(&ctx, &["rev-parse", "--abbrev-ref", "HEAD"], None).await {
        Ok(o) if o.success() => {
            let name = o.stdout.trim().to_string();
            // Detached HEAD reports the literal "HEAD" — not a branch name.
            (!name.is_empty() && name != "HEAD").then_some(name)
        }
        _ => None,
    };
    Ok(GitGraph { head, commits, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_git() -> bool {
        crate::pty::resolve_command("git").is_ok()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git spawns");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn setup_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cimp-history-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
        dir
    }

    fn commit_file(dir: &Path, name: &str, content: &str, msg: &str) {
        std::fs::write(dir.join(name), content).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", msg]);
    }

    #[test]
    fn parse_log_splits_fields_records_and_refs() {
        let raw = format!(
            "aaaa{f}a1{f}bbbb cccc{f}1700000000{f}Amir{f}HEAD -> develop, tag: v1{f}subject line{f}body line 1\nbody line 2{r}\nbbbb{f}b1{f}{f}1700000100{f}Amir{f}{f}second{f}{r}",
            f = FIELD_SEP,
            r = RECORD_SEP,
        );
        let commits = parse_log(&raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].hash, "aaaa");
        assert_eq!(commits[0].parents, vec!["bbbb", "cccc"]);
        assert_eq!(commits[0].ts_ms, 1_700_000_000_000);
        assert_eq!(commits[0].refs, vec!["HEAD -> develop", "tag: v1"]);
        assert_eq!(commits[0].body, "body line 1\nbody line 2");
        assert_eq!(commits[1].parents, Vec::<String>::new());
        assert_eq!(commits[1].refs, Vec::<String>::new());
        assert_eq!(commits[1].body, "");
    }

    #[test]
    fn validate_hash_accepts_hex_rejects_expressions() {
        assert!(validate_hash("4285982").is_ok());
        assert!(validate_hash(&"a".repeat(40)).is_ok());
        assert!(validate_hash("HEAD").is_err());
        assert!(validate_hash("HEAD~3").is_err());
        assert!(validate_hash("--exec=x").is_err());
        assert!(validate_hash("abc").is_err()); // too short
        assert!(validate_hash(&"a".repeat(41)).is_err());
    }

    #[tokio::test]
    async fn session_commits_filters_by_window_and_counts_match() {
        if !has_git() {
            return;
        }
        let dir = setup_repo("window");
        commit_file(&dir, "a.txt", "1", "first");
        commit_file(&dir, "a.txt", "2", "second");
        let all = session_commits(&dir, 0, i64::MAX, &[]).await.unwrap();
        assert_eq!(all.commits.len(), 2);
        assert_eq!(all.commits[0].subject, "second"); // newest first
        assert!(all.commits.iter().all(|c| !c.tracked));
        // The whole (2-commit) history was scanned — never flagged truncated.
        assert!(!all.truncated);

        let none = session_commits(&dir, 0, 1, &[]).await.unwrap();
        assert!(none.commits.is_empty());

        // A recorded hash prefix pulls a commit in even OUTSIDE the window,
        // flagged tracked; window-only commits stay untracked.
        let first_short: String = all.commits[1].hash.chars().take(7).collect();
        let by_hash = session_commits(&dir, 0, 1, &[first_short.clone()]).await.unwrap();
        assert_eq!(by_hash.commits.len(), 1);
        assert_eq!(by_hash.commits[0].subject, "first");
        assert!(by_hash.commits[0].tracked);

        let (times, hit_cap) = log_commit_times(&dir, MAX_LOG_COMMITS).await.unwrap();
        assert_eq!(times.len(), 2);
        assert!(!hit_cap);
        let counts = commit_counts_from(
            &times,
            &[
                SessionWindow { session_id: "s1".into(), from_ms: 0, to_ms: i64::MAX },
                SessionWindow { session_id: "s2".into(), from_ms: 0, to_ms: 1 },
                SessionWindow { session_id: "s3".into(), from_ms: 0, to_ms: 1 },
            ],
            &HashMap::from([("s3".to_string(), vec![first_short])]),
        );
        assert_eq!(counts["s1"], 2);
        assert_eq!(counts["s2"], 0);
        assert_eq!(counts["s3"], 1); // recorded hash counts despite the empty window
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commit_diff_parses_files_and_rejects_bad_hash() {
        if !has_git() {
            return;
        }
        let dir = setup_repo("diff");
        commit_file(&dir, "a.txt", "one\n", "first");
        commit_file(&dir, "a.txt", "two\n", "second");
        let commits = session_commits(&dir, 0, i64::MAX, &[]).await.unwrap().commits;
        let files = commit_diff(&dir, &commits[0].hash, 3).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].hunks.len(), 1);
        // Root commit renders as an all-added diff, not an error.
        let root_files = commit_diff(&dir, &commits[1].hash, 3).await.unwrap();
        assert_eq!(root_files.len(), 1);
        assert!(commit_diff(&dir, "HEAD", 3).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn git_graph_topo_orders_children_before_parents_with_refs() {
        if !has_git() {
            return;
        }
        let dir = setup_repo("graph");
        commit_file(&dir, "a.txt", "1", "base");
        git(&dir, &["checkout", "-b", "feature"]);
        commit_file(&dir, "b.txt", "1", "feat work");
        git(&dir, &["checkout", "main"]);
        commit_file(&dir, "a.txt", "2", "main work");
        git(&dir, &["merge", "--no-ff", "-m", "merge feature", "feature"]);

        let g = git_graph(&dir, 100).await.unwrap();
        assert_eq!(g.head.as_deref(), Some("main"));
        assert!(!g.truncated);
        assert_eq!(g.commits.len(), 4);
        // Topo order: every commit appears before all of its parents.
        let pos: HashMap<&str, usize> =
            g.commits.iter().enumerate().map(|(i, c)| (c.hash.as_str(), i)).collect();
        for c in &g.commits {
            for p in &c.parents {
                assert!(pos[c.hash.as_str()] < pos[p.as_str()], "{} before {}", c.short, p);
            }
        }
        // The merge commit has two parents and carries the HEAD decoration.
        let merge = &g.commits[0];
        assert_eq!(merge.parents.len(), 2);
        assert!(merge.refs.iter().any(|r| r.contains("HEAD")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_and_non_repo_roots_are_empty_not_errors() {
        if !has_git() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("cimp-history-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
        // Repo with zero commits: empty list, head is the unborn branch or None.
        let res = session_commits(&dir, 0, i64::MAX, &[]).await.unwrap();
        assert!(res.commits.is_empty());
        assert!(!res.truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
