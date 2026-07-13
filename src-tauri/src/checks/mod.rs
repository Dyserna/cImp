//! V12 Phase A — `run_check`: run one of a project's configured checker
//! commands (`cargo check`, `tsc`, `eslint`, `pytest`, ...) and return
//! **deduplicated, structured diagnostics** instead of a raw dump. Configured
//! per project via the top-level `checks: Vec<CheckDef>` settings field (rides
//! the `.cimp/config.json` overlay — see `settings::schema::Settings::checks`).
//!
//! Security posture, same as the offload `run_command` tool: the command
//! comes from the *user's* config, never the model. The `run_check` MCP tool
//! (`graph::mcp`) lets a model-supplied `name` only *select* among the
//! configured [`CheckDef`]s — it can never supply or alter the command line.
//!
//! Independent of the code-knowledge-graph feature: `run` takes a project
//! root and a [`CheckDef`], nothing graph-shaped. The `graph::mcp` tool
//! surface just happens to be where cloud-agent tools live in this codebase.

pub mod auto;
pub mod gitls;
pub mod parsers;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::{AppError, AppResult};

/// One configured project check (a build/lint/test command + how to parse
/// its output). Lives in `.cimp/config.json`'s top-level `checks` array —
/// see `settings::schema::Settings::checks`.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub struct CheckDef {
    /// Short id the `run_check` tool's `name` argument selects (e.g. `"cargo"`).
    pub name: String,
    /// The full shell command line to run, cwd = the project root (e.g.
    /// `"cargo check --message-format=json"`).
    pub cmd: String,
    /// How to parse its output into diagnostics.
    pub parser: ParserKind,
    /// Hard timeout in seconds. [`run`] floors this at 10s regardless of a
    /// smaller configured value — a check that can't even attempt in 10s
    /// isn't meaningfully bounded.
    pub timeout_secs: u64,
}

impl Default for CheckDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            cmd: String::new(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 120,
        }
    }
}

/// Which built-in parser decodes a check's captured output into [`Diag`]s.
/// Wire format is kebab-case (`"cargo-json"`, `"generic-gcc"`, ...), matching
/// the rest of the settings schema's enum convention.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ParserKind {
    /// `cargo check --message-format=json` (or `cargo clippy` the same way).
    CargoJson,
    /// `tsc --noEmit --pretty false`.
    Tsc,
    /// `eslint --format json`.
    EslintJson,
    /// `pytest` — the short test-summary section (`FAILED file::test - msg`)
    /// plus the tail counts line.
    Pytest,
    /// `cargo test` — stable-toolchain text output (JSON is nightly-only).
    CargoTest,
    /// `jest --json` / `vitest --reporter=json` (same shape).
    JestJson,
    /// `file:line[:col]: error|warning|note: message` — the fallback for
    /// gcc/clang and most other line-oriented CLI checkers.
    #[default]
    GenericGcc,
}

/// Diagnostic severity, shared by every parser.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// One raw diagnostic as parsed from a checker's output, before dedup.
#[derive(Clone, Debug, PartialEq)]
pub struct Diag {
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
    pub file: String,
    pub line: u32,
    pub col: Option<u32>,
}

/// One deduplicated diagnostic group — every [`Diag`] that shares
/// `(severity, code, normalized message)` collapses into one of these. `key`
/// is the internal dedup key (not meant for display); `message` folds the
/// code in when present (`"E0425: cannot find value ‹…› in this scope"`) so
/// it reads as a complete line on its own.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiagGroup {
    pub key: String,
    pub severity: Severity,
    pub message: String,
    pub count: usize,
    /// First occurrences, `(file, line)`, capped at [`MAX_SITES`].
    pub sites: Vec<(String, u32)>,
}

/// One `run_check` invocation's result.
#[derive(Clone, Debug, Serialize)]
pub struct CheckReport {
    pub name: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// The command was killed after its timeout; `groups` reflects only the
    /// output captured before the kill, so this run may be incomplete.
    pub timed_out: bool,
    pub groups: Vec<DiagGroup>,
}

/// Cap on sample locations kept per [`DiagGroup`].
const MAX_SITES: usize = 5;

/// Cap on captured bytes per stream (stdout/stderr). Generous — diagnostics
/// are deduped after parsing, so a chatty checker doesn't blow up the report,
/// but bounds memory on a runaway process the way `run_command` does.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Run `def.cmd` (via the shell, cwd = `root`) and return its deduplicated
/// diagnostics. `changed_only` filters the resulting groups to sites in files
/// touched since HEAD (tracked diff ∪ untracked, via [`gitls::changed_files`]);
/// a git failure (not a repo, no commits yet, ...) degrades to **unfiltered**
/// rather than erroring or silently returning nothing, so `run_check` still
/// works outside git.
pub async fn run(root: &Path, def: &CheckDef, changed_only: bool) -> AppResult<CheckReport> {
    let started = Instant::now();
    let timeout_secs = def.timeout_secs.max(10);
    let (exit_code, stdout, stderr, timed_out) = spawn_capture(root, &def.cmd, timeout_secs).await?;
    let duration_ms = started.elapsed().as_millis() as u64;

    let diags = parsers::parse(def.parser, &stdout, &stderr, root);
    // `group` keeps every site (uncapped) so the `changed_only` filter below
    // sees the FULL occurrence list, not just the first `MAX_SITES` in source
    // order — a diagnostic already firing in ≥ `MAX_SITES` other files must
    // still be recognized as touching a just-edited file even when that
    // file's occurrence would otherwise fall past the cap. Sites are capped
    // to `MAX_SITES` only after that filter runs, below.
    let mut groups = group(diags);

    let changed = if changed_only {
        // Git failure degrades to unfiltered — see the doc comment above.
        gitls::changed_files(root).await.ok()
    } else {
        None
    };

    if let Some(changed) = &changed {
        groups.retain(|g| g.sites.iter().any(|(f, _)| changed.contains(&normalize_rel(root, f))));
    }
    for g in &mut groups {
        cap_sites(&mut g.sites, root, changed.as_ref());
    }

    Ok(CheckReport {
        name: def.name.clone(),
        exit_code,
        duration_ms,
        timed_out,
        groups,
    })
}

/// Group raw diagnostics by `(severity, code, normalized message)`, folding
/// the code into the group's display message. `sites` is kept UNCAPPED here —
/// deliberately, so [`run`]'s `changed_only` filter can see every occurrence
/// before [`cap_sites`] trims for display (see [`run`]'s doc comment on the
/// V12 review's 5-site-cap fix). Preserves first-seen order (not sorted) so
/// the model reads the checker's own ordering (usually source order).
fn group(diags: Vec<Diag>) -> Vec<DiagGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, DiagGroup> = HashMap::new();
    for d in diags {
        let normalized = normalize_message(&d.message);
        let key = format!("{}|{}|{}", d.severity.as_str(), d.code.as_deref().unwrap_or(""), normalized);
        if !map.contains_key(&key) {
            order.push(key.clone());
            let message = match &d.code {
                Some(c) => format!("{c}: {normalized}"),
                None => normalized,
            };
            map.insert(
                key.clone(),
                DiagGroup { key: key.clone(), severity: d.severity, message, count: 0, sites: Vec::new() },
            );
        }
        let entry = map.get_mut(&key).expect("just inserted");
        entry.count += 1;
        entry.sites.push((d.file.clone(), d.line));
    }
    order.into_iter().filter_map(|k| map.remove(&k)).collect()
}

/// Cap `sites` to [`MAX_SITES`] for display. When `changed` is `Some` (the
/// `changed_only` path), a site whose file is in the changed set sorts first
/// — via a stable sort, so relative order within each partition (changed vs.
/// not) is preserved — so a new occurrence in the just-edited file is never
/// pushed out by ≥ `MAX_SITES` older occurrences elsewhere. `changed == None`
/// (the unfiltered path, or a degraded git failure) leaves the original
/// source-order truncation unchanged.
fn cap_sites(sites: &mut Vec<(String, u32)>, root: &Path, changed: Option<&HashSet<String>>) {
    if sites.len() <= MAX_SITES {
        return;
    }
    if let Some(changed) = changed {
        sites.sort_by_key(|(f, _)| if changed.contains(&normalize_rel(root, f)) { 0 } else { 1 });
    }
    sites.truncate(MAX_SITES);
}

/// Replace every `'…'` / `` `…` `` quoted span in `msg` with a placeholder, so
/// e.g. `` cannot find value `x` in this scope `` and `` cannot find value `y`
/// in this scope `` collapse into the same dedup group — the quoted identifier
/// is exactly the part that varies per occurrence.
fn normalize_message(msg: &str) -> String {
    let chars: Vec<char> = msg.chars().collect();
    let mut out = String::with_capacity(msg.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '`' {
            if let Some(rel_end) = chars[i + 1..].iter().position(|&x| x == c) {
                out.push_str("‹…›");
                i += rel_end + 2; // skip the opening quote, the span, and the closing quote
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Normalize a diagnostic-reported file path to a project-relative,
/// forward-slash form comparable against [`gitls::changed_files`]'s output.
/// Handles both OS path separators (tsc/gcc emit whatever the platform uses)
/// and absolute paths (eslint's `filePath` is always absolute).
fn normalize_rel(root: &Path, file: &str) -> String {
    let forward = file.replace('\\', "/");
    let p = Path::new(&forward);
    if p.is_absolute() {
        if let (Ok(canon_root), Ok(canon_file)) = (root.canonicalize(), p.canonicalize()) {
            if let Ok(stripped) = canon_file.strip_prefix(&canon_root) {
                return stripped.to_string_lossy().replace('\\', "/");
            }
        }
        return forward;
    }
    forward.trim_start_matches("./").to_string()
}

/// Spawn `cmd` via the platform shell (cwd = `root`), console-suppressed on
/// Windows, capturing stdout/stderr separately (reader tasks that outlive the
/// timeout, so a killed process still yields whatever it had already printed —
/// the "parse partial output" half of the timeout contract). Returns
/// `(exit_code, stdout, stderr, timed_out)`; `Err` only for a spawn failure
/// (bad shell, permissions), never for the checked command's own exit code.
async fn spawn_capture(
    root: &Path,
    cmd: &str,
    timeout_secs: u64,
) -> AppResult<(Option<i32>, String, String, bool)> {
    let mut command = shell_command(cmd);
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Kill the child if this future is dropped, so an aborted `run` call
        // never leaks an orphaned checker process.
        .kill_on_drop(true);
    // Don't flash a console window for each spawned checker on Windows —
    // same CREATE_NO_WINDOW convention as every other spawned subprocess
    // (offload's `llama-server`, MCP host, `run_command`).
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);

    let mut child = command
        .spawn()
        .map_err(|e| AppError::Checks(format!("failed to spawn check `{cmd}`: {e}")))?;
    // Backstop: reap this checker subprocess via the kill-on-job-close job if
    // cImp dies hard before `kill_on_drop` can fire.
    crate::process_guard::guard_child(&child);

    // Drain stdout/stderr on their own tasks so the buffers survive a timeout
    // on `child.wait()` below — killing the child for a timeout only closes
    // its pipes (which cleanly EOFs these readers), it doesn't discard what
    // was already captured.
    let out_task = tokio::spawn(read_capped(child.stdout.take(), MAX_OUTPUT_BYTES));
    let err_task = tokio::spawn(read_capped(child.stderr.take(), MAX_OUTPUT_BYTES));

    let (exit_code, timed_out) = match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => return Err(AppError::Checks(format!("check `{cmd}` failed: {e}"))),
        Err(_) => {
            // Timed out: kill (kill_on_drop is a backstop, not a guarantee the
            // process is gone by the time we read the buffers below) and reap.
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true)
        }
    };

    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    Ok((exit_code, stdout, stderr, timed_out))
}

/// Wrap `cmd` (a full command line, e.g. `"cargo check --message-format=json"`)
/// for shell execution: `cmd.exe /C <cmd>` on Windows, `sh -c <cmd>` elsewhere.
/// `cmd` is always a user-configured [`CheckDef::cmd`] (never model-supplied —
/// see the module doc comment), so shell interpretation is the intended
/// behavior, not an injection surface.
///
/// On Windows the `/C` payload is appended via [`raw_arg`] rather than
/// [`arg`] — `cmd.exe` parses its command tail with its OWN quoting rules,
/// not the `CommandLineToArgvW` (C-runtime) rules `.arg()` assumes; running a
/// command string containing its own quotes (e.g. `"C:\Program
/// Files\...\tsc.cmd" --noEmit`) through `.arg()` gets it double-escaped and
/// `cmd.exe` fails to parse it back out. `raw_arg` appends the text verbatim,
/// exactly as if it had been typed after `cmd /C` interactively.
///
/// [`raw_arg`]: tokio::process::Command::raw_arg
/// [`arg`]: tokio::process::Command::arg
fn shell_command(cmd: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd");
        c.raw_arg("/C").raw_arg(cmd);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    }
}

/// Read `reader` to EOF, retaining at most `cap` bytes but continuing to
/// drain (and discard) the rest so the child isn't blocked on a full pipe.
/// `None` (a stream that wasn't piped) yields an empty string. Lossy UTF-8 —
/// checker output is text, and a stray invalid byte shouldn't drop the run.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, cap: usize) -> String {
    let mut bytes = Vec::new();
    if let Some(mut reader) = reader {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bytes.len() < cap {
                        let take = n.min(cap - bytes.len());
                        bytes.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(severity: Severity, code: Option<&str>, message: &str, file: &str, line: u32) -> Diag {
        Diag { severity, code: code.map(str::to_string), message: message.to_string(), file: file.to_string(), line, col: None }
    }

    #[test]
    fn normalize_message_replaces_quoted_spans() {
        assert_eq!(
            normalize_message("cannot find value `x` in this scope"),
            "cannot find value ‹…› in this scope"
        );
        assert_eq!(normalize_message("expected 'i32', found 'String'"), "expected ‹…›, found ‹…›");
        // No quotes: unchanged.
        assert_eq!(normalize_message("aborting due to 2 previous errors"), "aborting due to 2 previous errors");
        // Unterminated quote: left as a literal character, not consumed.
        assert_eq!(normalize_message("odd ' quote"), "odd ' quote");
    }

    #[test]
    fn group_dedups_but_leaves_sites_uncapped() {
        let mut diags = Vec::new();
        for i in 0..8 {
            diags.push(diag(Severity::Error, Some("E0425"), &format!("cannot find value `v{i}` in this scope"), &format!("src/f{i}.rs"), 1));
        }
        // A different code must NOT collapse into the same group.
        diags.push(diag(Severity::Warning, Some("unused"), "unused variable `y`", "src/g.rs", 3));

        let groups = group(diags);
        assert_eq!(groups.len(), 2, "the 8 E0425s collapse to one group, the warning is separate: {groups:?}");
        let e0425 = groups.iter().find(|g| g.message.starts_with("E0425")).expect("E0425 group");
        assert_eq!(e0425.count, 8);
        // `group` no longer caps — that's `cap_sites`'s job now, run after
        // the `changed_only` filter (see `run`'s doc comment).
        assert_eq!(e0425.sites.len(), 8, "group() itself leaves sites uncapped: {e0425:?}");
        assert_eq!(e0425.message, "E0425: cannot find value ‹…› in this scope");
    }

    #[test]
    fn cap_sites_plain_truncates_in_source_order() {
        let dir = std::env::temp_dir().join(format!("checks-capsites-{}", uuid::Uuid::new_v4()));
        let mut sites: Vec<(String, u32)> =
            (0..8).map(|i| (format!("src/f{i}.rs"), 1)).collect();
        cap_sites(&mut sites, &dir, None);
        assert_eq!(sites.len(), MAX_SITES);
        assert_eq!(sites, vec![
            ("src/f0.rs".to_string(), 1),
            ("src/f1.rs".to_string(), 1),
            ("src/f2.rs".to_string(), 1),
            ("src/f3.rs".to_string(), 1),
            ("src/f4.rs".to_string(), 1),
        ]);
    }

    #[test]
    fn cap_sites_prefers_a_changed_file_site_past_the_cap() {
        // 6 sites, none of the first 5 (source order) is the changed file —
        // it's the 6th. A naive source-order truncation would drop it; the
        // changed-file site must still show up among the capped 5.
        let dir = std::env::temp_dir().join(format!("checks-capsites-changed-{}", uuid::Uuid::new_v4()));
        let mut sites: Vec<(String, u32)> = (0..6).map(|i| (format!("src/f{i}.rs"), 1)).collect();
        let mut changed = HashSet::new();
        changed.insert("src/f5.rs".to_string());

        cap_sites(&mut sites, &dir, Some(&changed));

        assert_eq!(sites.len(), MAX_SITES);
        assert!(sites.iter().any(|(f, _)| f == "src/f5.rs"), "{sites:?}");
    }

    #[test]
    fn group_preserves_first_seen_order() {
        let diags = vec![
            diag(Severity::Warning, None, "second thing", "b.rs", 1),
            diag(Severity::Error, None, "first thing", "a.rs", 1),
            diag(Severity::Warning, None, "second thing", "b.rs", 2),
        ];
        let groups = group(diags);
        assert_eq!(groups[0].message, "second thing");
        assert_eq!(groups[1].message, "first thing");
        assert_eq!(groups[0].count, 2);
    }

    #[test]
    fn cargo_test_identical_assertions_group_via_normalized_message() {
        // Two failing tests whose panic blocks differ only in backtick-quoted
        // values collapse into ONE group — the dedup key normalizes quoted
        // spans, so `left: `1`` / `left: `3`` don't split the group. Pins that
        // the cargo-test parser's block-as-message plays nicely with `group`.
        const OUT: &str = "test tests::a ... FAILED\ntest tests::b ... FAILED\n\nfailures:\n\n---- tests::a stdout ----\nthread 'tests::a' panicked at 'assertion failed: `(left == right)`\n  left: `1`,\n right: `2`', src/helper.rs:5:5\n\n---- tests::b stdout ----\nthread 'tests::b' panicked at 'assertion failed: `(left == right)`\n  left: `3`,\n right: `4`', src/helper.rs:5:5\n\ntest result: FAILED. 0 passed; 2 failed;\n";
        let diags = parsers::parse(ParserKind::CargoTest, OUT, "", std::path::Path::new("."));
        let groups = group(diags);
        let errs: Vec<_> = groups.iter().filter(|g| g.severity == Severity::Error).collect();
        assert_eq!(errs.len(), 1, "identical assertions should group: {groups:?}");
        assert_eq!(errs[0].count, 2);
    }

    #[test]
    fn normalize_rel_handles_separators_and_absolute_paths() {
        let dir = std::env::temp_dir().join(format!("checks-normrel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("lib.rs"), "").unwrap();

        assert_eq!(normalize_rel(&dir, "src\\lib.rs"), "src/lib.rs");
        assert_eq!(normalize_rel(&dir, "./src/lib.rs"), "src/lib.rs");
        let abs = dir.join("src").join("lib.rs");
        assert_eq!(normalize_rel(&dir, &abs.to_string_lossy()), "src/lib.rs");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Resolve `program` to an absolute path (quoted for shell re-embedding).
    /// `cmd.exe`'s OWN internal PATH search (used for an unqualified command
    /// name inside a `/C` line) has a much smaller effective buffer than the
    /// `PATH` env var itself — on a dev box with many native-dependency build
    /// scripts (this repo's tree-sitter/whisper/espeak crates each prepend
    /// their `OUT_DIR` under `cargo test`), `PATH` easily exceeds that limit
    /// and `cmd /C <bare-name> ...` starts failing with a false "not
    /// recognized" even though the program is genuinely on `PATH`. Passing an
    /// already-resolved absolute path sidesteps `cmd.exe`'s search entirely,
    /// so these tests stay robust regardless of how long `PATH` is.
    fn resolve_quoted(program: &str) -> String {
        let path = which::which(program).unwrap_or_else(|_| panic!("{program} not found on PATH"));
        format!("\"{}\"", path.display())
    }

    /// A quick real spawn, no timeout — verifies the shell wrapper + console
    /// suppression + capture plumbing actually runs a process end to end.
    /// `cargo` is always present in this repo's build/test environment.
    #[tokio::test]
    async fn run_executes_a_real_command() {
        let cmd = format!("{} --version", resolve_quoted("cargo"));
        let def = CheckDef { name: "sanity".into(), cmd, parser: ParserKind::GenericGcc, timeout_secs: 30 };
        let report = run(&std::env::temp_dir(), &def, false).await.expect("run");
        assert_eq!(report.exit_code, Some(0));
        assert!(!report.timed_out);
    }

    /// A real timeout: the spawned command sleeps far longer than the (floored)
    /// 10s budget. Slow (~10s) by construction — `timeout_secs` is floored, not
    /// the test's choice — but it's the only way to exercise the real
    /// kill-and-report-partial path end to end.
    #[tokio::test]
    async fn run_kills_on_timeout_and_reports_partial() {
        #[cfg(windows)]
        let cmd = format!("{} -n 40 127.0.0.1 >NUL", resolve_quoted("ping"));
        #[cfg(not(windows))]
        let cmd = format!("{} 40", resolve_quoted("sleep"));
        let def = CheckDef { name: "slow".into(), cmd, parser: ParserKind::GenericGcc, timeout_secs: 1 };
        let started = Instant::now();
        let report = run(&std::env::temp_dir(), &def, false).await.expect("run");
        assert!(report.timed_out);
        assert_eq!(report.exit_code, None);
        // Floored at 10s, generous upper bound for slow CI.
        assert!(started.elapsed() >= Duration::from_secs(9), "elapsed: {:?}", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(60), "elapsed: {:?}", started.elapsed());
    }
}
