//! V12 Phase F — proactive automation core: per-session debounce state, the
//! new-vs-baseline diagnostic diff (pure, unit-testable), and single-flight
//! check running per project root. Driven by `GraphService::post_edit`
//! (`graph/service.rs`), which owns the actual per-session/per-root state maps
//! and layers in the graph-derived auto-impact note (V12 F2/6b — that note's
//! GATE and DISPLAY counts come from `GraphIndex` queries the service already
//! has open). This module knows nothing about the graph, only about
//! [`CheckDef`]/[`CheckReport`]/[`DiagGroup`], so its core logic is testable
//! without a `GraphIndex`, an `AppHandle`, or a real checker process — the
//! same posture as `checks::mod`'s `group`/`normalize_message` pure helpers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;

use super::{run, CheckDef, CheckReport, DiagGroup};

/// One check's dedup-key → count snapshot, from the last report a session has
/// effectively "seen" (run and diffed against, whether or not anything in it
/// was actually surfaced).
pub type Baseline = HashMap<String, usize>;

/// Per-session auto-check bookkeeping (V12 F2). Lives in a
/// `Mutex<HashMap<session_id, AutoCheckState>>` on `GraphService` — baselines
/// and the debounce clock are per SESSION (never per project), so two agents
/// editing the same project are each told only what THEY haven't seen (V10
/// session scoping).
#[derive(Default)]
pub struct AutoCheckState {
    /// When this session last triggered an actual check run — the debounce
    /// clock. `None` until the first edit.
    pub last_run: Option<Instant>,
    /// Per-check baseline (check name -> dedup-key -> count).
    pub baseline: HashMap<String, Baseline>,
    /// A block computed after the client-facing budget elapsed (a slow check
    /// run continuing in the background), waiting to be drained by the next
    /// `post_edit` or `/context/retrieve` call. Taken (cleared) on drain.
    pub pending: Option<String>,
}

/// Leading-edge debounce: the FIRST edit after a quiet gap of at least
/// `debounce` since the last triggered run returns `true` (and resets the
/// clock to `now`); further edits inside the window return `false` and are
/// coalesced. This is what turns "three rapid edits" into "one run" — the
/// burst's LAST edit's on-disk state is still what the next run (or the
/// following edit, once the gap reopens) checks, since checks run against the
/// file system, not a specific edit. Pure — the caller supplies `now`, so
/// this is directly testable without real sleeps.
pub fn should_run(state: &mut AutoCheckState, now: Instant, debounce: Duration) -> bool {
    let run = state
        .last_run
        .map(|t| now.saturating_duration_since(t) >= debounce)
        .unwrap_or(true);
    if run {
        state.last_run = Some(now);
    }
    run
}

/// New-or-worsened groups in `groups` relative to `baseline`: a group absent
/// from the baseline (new) or whose count increased (worsened) is kept; an
/// unchanged or improved (lower) count is dropped. Pure — the core of the
/// whole auto-check feature, and the one piece unit-tested without spawning a
/// real checker.
pub fn diff_groups<'a>(baseline: &Baseline, groups: &'a [DiagGroup]) -> Vec<&'a DiagGroup> {
    groups
        .iter()
        .filter(|g| {
            baseline
                .get(&g.key)
                .map(|&prev| g.count > prev)
                .unwrap_or(true)
        })
        .collect()
}

/// The baseline snapshot to remember after a run: every group's current
/// count, keyed by dedup key. A group that disappeared (fixed) is simply
/// absent from the result — if it reappears in a LATER report, [`diff_groups`]
/// correctly re-treats it as new (there's no stale "already seen" entry for
/// it to compare against).
pub fn to_baseline(groups: &[DiagGroup]) -> Baseline {
    groups.iter().map(|g| (g.key.clone(), g.count)).collect()
}

/// Render new/worsened groups across one or more checks into one injectable
/// block, capped at `cap_chars` (truncated with a trailing marker on overflow,
/// never mid-multibyte-char). Empty input (or every check's group list empty)
/// → empty string, which the caller treats as "nothing to inject". Mirrors
/// `run_check`'s per-group line shape (`graph::mcp::fmt_check_report`, the
/// Feature 1 compact format: `severity · message · ×count · sites`) so the
/// model reads the same shape whether it pulled the report itself or the
/// harness pushed it.
pub fn format_diff_block(per_check: &[(&str, Vec<&DiagGroup>)], cap_chars: usize) -> String {
    let mut out = String::new();
    for (name, groups) in per_check {
        if groups.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("New since your last edit — {name}:\n"));
        for g in groups {
            let sites: Vec<String> = g.sites.iter().map(|(f, l)| format!("{f}:{l}")).collect();
            out.push_str(&format!(
                "{} · {} · ×{} · {}\n",
                g.severity.as_str(),
                g.message,
                g.count,
                sites.join(", ")
            ));
        }
    }
    let out = out.trim_end();
    if out.chars().count() > cap_chars {
        let truncated: String = out.chars().take(cap_chars).collect();
        format!("{truncated}\n… (truncated)")
    } else {
        out.to_string()
    }
}

/// Pure formatter for the auto-impact blast-radius note (V12 F2/6b). `callers`
/// is the DIRECT inbound edge count that gates whether anything is emitted at
/// all (`GraphIndex::callers_count`, V11-A1) — kept separate from the wider
/// `dependents`/`files`/`tests` DISPLAY counts (`dependents_transitive` /
/// `tests_for`) so the note can honestly show a bigger transitive number than
/// the smaller direct count that triggered it. All counts are computed by the
/// caller (graph queries); this module has no graph dependency.
pub fn impact_note(
    callers: u64,
    dependents: usize,
    files: usize,
    tests: usize,
    min_dependents: u64,
) -> Option<String> {
    if callers < min_dependents || dependents == 0 {
        return None;
    }
    Some(format!(
        "{dependents} dependents across {files} files; {tests} tests cover this — \
         `graph_impact` / `graph_tests_for` for the list."
    ))
}

/// Render a check's spawn/run failure as a visible one-line note (V12 review
/// — a check that fails to spawn/run must never be indistinguishable from a
/// clean run by simply vanishing from the result). Shared wording between
/// [`RootRunner::run`]'s auto-check aggregation and the `run_check` tool's own
/// error formatting (`graph::mcp::run_check_inner`), so the model reads the
/// same shape whichever path surfaced it.
pub fn spawn_failure_line(name: &str, err: &str) -> String {
    format!("⚠ check `{name}` did not run: {err}")
}

/// Single-flight per-root check runner (V12 F2): concurrent callers for the
/// SAME root share one run's result instead of each spawning the configured
/// checks — a Claude tab and an OpenCode tab editing the same project at the
/// same time don't duplicate a build. One `tokio::sync::Mutex` per root plus
/// a small "last completed run" cache: a caller that starts WAITING for the
/// lock before an in-flight run finishes will, once it acquires the lock, see
/// a cached result stamped AFTER its own wait began — so it reuses that
/// result instead of re-running. Diffing that shared result against each
/// caller's own (per-session) baseline is the caller's job (`GraphService`),
/// not this runner's. Shared across every session on `GraphService`.
/// One root's cached run: when it finished, its reports, and the per-check
/// error strings.
type CachedRun = (Instant, Vec<CheckReport>, Vec<String>);

#[derive(Default)]
pub struct RootRunner {
    locks: StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
    last: StdMutex<HashMap<PathBuf, CachedRun>>,
}

impl RootRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run every configured check for `root` with `changed_only = true`
    /// (single-flight per root; see the struct doc). A check that fails to
    /// spawn/run is dropped from the `Vec<CheckReport>` (one bad `CheckDef`
    /// doesn't blank out the rest) but is NOT silently discarded — its
    /// name/error lands in the returned `Vec<String>` (already formatted as
    /// `"⚠ check \`<name>\` did not run: <err>"`), so a misconfigured check
    /// reads as visibly broken rather than indistinguishable from "ran clean"
    /// (V12 review — a spawn failure must never look green).
    ///
    /// `sandbox` is the V33 OS-sandbox config for this run, derived from the
    /// caller's live settings. The auto-check path is agent-triggered (a
    /// `post_edit` hook after a model wrote a file), so it is as much a
    /// [`crate::spawn_ledger::SpawnClass::AgentSpawn`] as an explicit
    /// `run_check` call and gets the same boundary — decision 17's "the switch
    /// governs the seam, not the caller".
    pub async fn run(
        &self,
        root: &Path,
        defs: &[CheckDef],
        sandbox: &crate::sandbox::SandboxCfg,
    ) -> (Vec<CheckReport>, Vec<String>) {
        let lock = {
            let mut locks = self.locks.lock().unwrap();
            locks
                .entry(root.to_path_buf())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let waited_since = Instant::now();
        let _guard = lock.lock().await;
        if let Some((ts, reports, errors)) = self.last.lock().unwrap().get(root) {
            if *ts >= waited_since {
                return (reports.clone(), errors.clone());
            }
        }
        let mut reports = Vec::with_capacity(defs.len());
        let mut errors = Vec::new();
        for def in defs {
            match run(root, def, true, sandbox).await {
                Ok(r) => reports.push(r),
                Err(e) => errors.push(spawn_failure_line(&def.name, &e.to_string())),
            }
        }
        self.last.lock().unwrap().insert(
            root.to_path_buf(),
            (Instant::now(), reports.clone(), errors.clone()),
        );
        (reports, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::Severity;

    fn group(key: &str, count: usize) -> DiagGroup {
        DiagGroup {
            key: key.to_string(),
            severity: Severity::Error,
            message: key.to_string(),
            count,
            sites: vec![("src/a.rs".to_string(), 1)],
        }
    }

    // ── debounce ─────────────────────────────────────────────────────────

    #[test]
    fn should_run_three_rapid_edits_trigger_one_run() {
        let mut state = AutoCheckState::default();
        let debounce = Duration::from_secs(5);
        let t0 = Instant::now();
        assert!(
            should_run(&mut state, t0, debounce),
            "first edit always runs"
        );
        // Two more edits arrive well inside the debounce window: coalesced.
        assert!(!should_run(
            &mut state,
            t0 + Duration::from_millis(100),
            debounce
        ));
        assert!(!should_run(
            &mut state,
            t0 + Duration::from_millis(200),
            debounce
        ));
    }

    #[test]
    fn should_run_after_window_closes_runs_again() {
        let mut state = AutoCheckState::default();
        let debounce = Duration::from_secs(5);
        let t0 = Instant::now();
        assert!(should_run(&mut state, t0, debounce));
        assert!(!should_run(
            &mut state,
            t0 + Duration::from_secs(1),
            debounce
        ));
        // A quiet gap of >= debounce since the LAST RUN (t0) reopens the window.
        assert!(should_run(
            &mut state,
            t0 + Duration::from_secs(6),
            debounce
        ));
    }

    // ── diff / baseline (the pure core) ─────────────────────────────────

    #[test]
    fn diff_groups_new_group_is_surfaced() {
        let baseline = Baseline::new();
        let groups = vec![group("e1", 1)];
        let diff = diff_groups(&baseline, &groups);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].key, "e1");
    }

    #[test]
    fn diff_groups_identical_second_run_is_empty() {
        let groups = vec![group("e1", 3), group("e2", 1)];
        let baseline = to_baseline(&groups);
        let diff = diff_groups(&baseline, &groups);
        assert!(
            diff.is_empty(),
            "unchanged counts must not re-surface: {diff:?}"
        );
    }

    #[test]
    fn diff_groups_worsened_count_is_surfaced_unchanged_is_not() {
        let baseline = to_baseline(&[group("e1", 2), group("e2", 5)]);
        let fresh = vec![group("e1", 4), group("e2", 5)];
        let diff = diff_groups(&baseline, &fresh);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].key, "e1");
    }

    #[test]
    fn diff_groups_fixed_then_reintroduced_counts_as_new() {
        // A group present in an earlier baseline but absent from a LATER one
        // (it was fixed in between) must read as NEW again if it comes back.
        let baseline = Baseline::new(); // simulates "already fixed, forgotten"
        let fresh = vec![group("e1", 1)];
        assert_eq!(diff_groups(&baseline, &fresh).len(), 1);
    }

    #[test]
    fn diff_groups_improved_count_is_not_surfaced() {
        let baseline = to_baseline(&[group("e1", 5)]);
        let fresh = vec![group("e1", 2)];
        assert!(diff_groups(&baseline, &fresh).is_empty());
    }

    #[test]
    fn diff_groups_newly_failing_test_vs_baseline_is_kept() {
        // A cargo-test/jest failure is just another DiagGroup keyed by its
        // normalized message — a test that starts failing (absent from the
        // baseline of prior failures) rides the same new-vs-baseline diff as
        // any compile diagnostic. No parser code change; this pins that.
        let baseline = to_baseline(&[group("error||tests::still_broken failed", 1)]);
        let fresh = vec![
            group("error||tests::still_broken failed", 1),
            group("error||tests::newly_broken failed", 1),
        ];
        let diff = diff_groups(&baseline, &fresh);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].key, "error||tests::newly_broken failed");
    }

    // ── parked-block drain ──────────────────────────────────────────────

    #[test]
    fn pending_block_drains_exactly_once() {
        let mut state = AutoCheckState {
            pending: Some("new diagnostics".to_string()),
            ..AutoCheckState::default()
        };
        assert_eq!(state.pending.take().as_deref(), Some("new diagnostics"));
        assert_eq!(state.pending, None, "a second drain must see nothing");
    }

    // ── formatting ───────────────────────────────────────────────────────

    #[test]
    fn format_diff_block_empty_input_is_empty_string() {
        assert_eq!(format_diff_block(&[], 1500), "");
        assert_eq!(format_diff_block(&[("cargo", vec![])], 1500), "");
    }

    #[test]
    fn format_diff_block_renders_and_caps() {
        let g = group("e1", 2);
        let block = format_diff_block(&[("cargo", vec![&g])], 1500);
        assert!(block.contains("cargo"));
        assert!(block.contains("e1"));

        let long_msg = "x".repeat(3000);
        let big = DiagGroup {
            key: "big".into(),
            severity: Severity::Error,
            message: long_msg,
            count: 1,
            sites: vec![],
        };
        let capped = format_diff_block(&[("cargo", vec![&big])], 100);
        assert!(capped.chars().count() <= 100 + "\n… (truncated)".chars().count());
        assert!(capped.ends_with("(truncated)"));
    }

    // ── impact note ──────────────────────────────────────────────────────

    #[test]
    fn impact_note_below_threshold_is_none() {
        assert!(impact_note(5, 20, 4, 2, 10).is_none());
    }

    #[test]
    fn impact_note_at_threshold_renders() {
        let note = impact_note(10, 14, 6, 3, 10).expect("note");
        assert!(note.contains("14 dependents across 6 files"));
        assert!(note.contains("3 tests"));
    }

    #[test]
    fn impact_note_zero_transitive_dependents_is_none() {
        // A defensive edge case: the direct-caller gate passed but the
        // transitive query somehow came back empty — never emit an empty note.
        assert!(impact_note(10, 0, 0, 0, 10).is_none());
    }

    // ── single-flight runner ─────────────────────────────────────────────

    /// Exercises the runner's locking/caching plumbing (no deadlock, both
    /// concurrent callers complete) with an empty check set — it does not
    /// (and can't, without a real slow subprocess) prove the cache-reuse path
    /// fires under real contention; that's covered by the struct doc's design
    /// rationale and `checks::run`'s own subprocess-timing tests.
    #[tokio::test]
    async fn root_runner_concurrent_callers_both_complete() {
        let runner = Arc::new(RootRunner::new());
        let root = std::env::temp_dir().join(format!("auto-runner-{}", uuid::Uuid::new_v4()));
        let defs: Vec<CheckDef> = Vec::new();

        let r1 = runner.clone();
        let root1 = root.clone();
        let defs1 = defs.clone();
        let h1 = tokio::spawn(async move {
            r1.run(&root1, &defs1, &crate::sandbox::SandboxCfg::disabled())
                .await
        });
        let r2 = runner.clone();
        let root2 = root.clone();
        let defs2 = defs.clone();
        let h2 = tokio::spawn(async move {
            r2.run(&root2, &defs2, &crate::sandbox::SandboxCfg::disabled())
                .await
        });

        let (res1, res2) = tokio::join!(h1, h2);
        let (reports1, errors1) = res1.unwrap();
        let (reports2, errors2) = res2.unwrap();
        assert!(reports1.is_empty());
        assert!(errors1.is_empty());
        assert!(reports2.is_empty());
        assert!(errors2.is_empty());
    }

    // ── spawn-failure visibility (V12 review) ───────────────────────────

    #[test]
    fn spawn_failure_line_is_visible_and_names_the_check() {
        let line = spawn_failure_line("broken", "failed to spawn check `bogus`: not found");
        assert!(line.starts_with("⚠ check"), "{line}");
        assert!(line.contains("broken"), "{line}");
        assert!(line.contains("not found"), "{line}");
    }
}
