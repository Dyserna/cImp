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
pub mod detect;
pub mod gitls;
pub mod parsers;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

/// One configured project check (a build/lint/test command + how to parse
/// its output). Lives in `.cimp/config.json`'s top-level `checks` array —
/// see `settings::schema::Settings::checks`.
///
/// `Debug` is hand-rolled to redact `env` VALUES (they may carry secrets),
/// matching `McpServerConfig`'s precedent.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CheckDef {
    /// Short id the `run_check` tool's `name` argument selects (e.g. `"cargo"`).
    pub name: String,
    /// The full shell command line to run (cwd = the project root, or `cwd`
    /// below when set — e.g. `"cargo check --message-format=json"`).
    pub cmd: String,
    /// How to parse its output into diagnostics.
    pub parser: ParserKind,
    /// Hard timeout in seconds. [`run`] floors this at 10s regardless of a
    /// smaller configured value — a check that can't even attempt in 10s
    /// isn't meaningfully bounded.
    pub timeout_secs: u64,
    /// V22 Phase B: run `cmd` in this directory instead of the project root — a
    /// path RELATIVE to the root, confined strictly beneath it (absolute or
    /// escaping `..` paths are rejected, at [`CheckDef::validate`] and again at
    /// spawn time in [`run`]). Replaces `--manifest-path`-style workarounds for
    /// nested manifests (this repo's `src-tauri/`) and monorepos generically.
    /// Diagnostic `file` paths are re-rooted back to the project root
    /// ([`reroot_diags`]) so the report stays root-relative regardless.
    pub cwd: Option<String>,
    /// V22 Phase B: environment variables forced on the spawned child — the
    /// same mechanism `CommandPolicy` uses for `run_command`. An ordered list
    /// (not a map) keeps the settings diff deterministic. Values may carry
    /// secrets, so [`CheckDef`]'s `Debug` redacts them to their keys.
    pub env: Vec<(String, String)>,
    /// V22 Phase B2: when set, the parser reads THIS file's content after the
    /// run instead of stdout — for tools (junit-xml, sarif) that write a report
    /// to disk rather than to a pipe. Same root-confinement as `cwd`; a
    /// missing/unreadable file becomes an explicit error diagnostic, never a
    /// silent green run. The read is capped at [`MAX_OUTPUT_BYTES`].
    pub report_file: Option<String>,
    /// V22 Phase C: the regex for the `regex-custom` parser (ignored by every
    /// other parser). Named groups `file`/`line`/`message` are mandatory and
    /// `col`/`severity` optional — validated at settings-save time via
    /// [`parsers::validate_pattern`] (reached through [`CheckDef::validate`]) so
    /// a bad pattern is a save error, not a silent zero-diagnostics run.
    pub pattern: Option<String>,
    /// V22 Phase D: `true` when this entry was created by language
    /// auto-detection ([`detect`]) rather than hand-authored. Re-detection
    /// ([`detect::merge_auto`]) may refresh entries with `auto == true` but must
    /// NEVER touch a `false` one — a user-created OR user-edited check owns its
    /// own name. The Phase E editor clears this flag whenever the user edits an
    /// auto entry, so a later re-detection stops fighting the manual change.
    /// `serde(default)` (via the struct-level default) ⇒ pre-V22-D configs load
    /// with `auto == false`, i.e. everything already on disk is treated as
    /// user-owned and protected.
    pub auto: bool,
}

impl Default for CheckDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            cmd: String::new(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 120,
            cwd: None,
            env: Vec::new(),
            report_file: None,
            pattern: None,
            auto: false,
        }
    }
}

impl std::fmt::Debug for CheckDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact `env` values — show only which keys are forced (the
        // `McpServerConfig` precedent), so a stray `?def` log can't leak a
        // token/key carried in a value.
        let env_keys: Vec<&String> = self.env.iter().map(|(k, _)| k).collect();
        f.debug_struct("CheckDef")
            .field("name", &self.name)
            .field("cmd", &self.cmd)
            .field("parser", &self.parser)
            .field("timeout_secs", &self.timeout_secs)
            .field("cwd", &self.cwd)
            .field("env_keys", &env_keys)
            .field("report_file", &self.report_file)
            .field("pattern", &self.pattern)
            .field("auto", &self.auto)
            .finish()
    }
}

impl CheckDef {
    /// Lexical validation of the path-shaped fields (`cwd`, `report_file`):
    /// each must be RELATIVE and free of `..` components, so a check can only
    /// touch the project subtree. Cheap and root-free — the entry point a
    /// future settings-save layer can call (none validates `checks` today).
    /// [`run`] additionally applies the full canonical confinement
    /// ([`confine_under_root`]) at spawn time, which also catches symlink
    /// escapes the lexical check can't see.
    pub fn validate(&self) -> AppResult<()> {
        if let Some(rel) = self.cwd.as_deref().filter(|s| !s.is_empty()) {
            lexically_confined("cwd", rel)?;
        }
        if let Some(rel) = self.report_file.as_deref().filter(|s| !s.is_empty()) {
            lexically_confined("report_file", rel)?;
        }
        // The `regex-custom` parser is inert without a valid pattern — require
        // one (with its mandatory named groups) here so a misconfigured check
        // fails at save/run time rather than silently parsing zero diagnostics.
        if self.parser == ParserKind::RegexCustom {
            match self.pattern.as_deref().filter(|s| !s.is_empty()) {
                Some(pat) => parsers::validate_pattern(pat).map_err(AppError::Checks)?,
                None => {
                    return Err(AppError::Checks(
                        "check `regex-custom` parser requires a `pattern`".to_string(),
                    ));
                }
            }
        }
        Ok(())
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
    /// SARIF 2.1 JSON (`ruff --output-format sarif`, `clang-tidy`,
    /// `golangci-lint`, `semgrep`, CodeQL, ...) — the modern-lint long tail in
    /// one parser (V22 Phase C).
    Sarif,
    /// `go build` / `go vet` text (`file:line[:col]: message`).
    Go,
    /// `go test -json` event stream (one JSON object per line).
    GoTestJson,
    /// MSBuild canonical diagnostics from `dotnet build` —
    /// `file(line,col): error|warning CODE: message` (C#/F#/VB).
    Dotnet,
    /// JUnit XML test report (Surefire/Gradle/pytest/PHPUnit/...), normally
    /// read via `report_file`.
    JunitXml,
    /// The universal escape hatch: a user-supplied regex with named groups
    /// (`file`, `line`, optional `col`/`severity`, `message`) applied per line.
    /// Uses [`CheckDef::pattern`].
    RegexCustom,
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
///
/// `Serialize` (V23) lets the audit runner ship raw `Diag`s to the Code Audit
/// tab verbatim (wrapped in `audit::AuditFinding`) without a second DTO — the
/// checks pipeline itself only ever serializes the deduplicated [`DiagGroup`].
#[derive(Clone, Debug, PartialEq, Serialize)]
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
    /// V22 Phase E: raw captured stdout/stderr byte counts (before parsing).
    /// The Phase E "Test" button needs the "did the command produce output at
    /// all?" signal to tell a genuinely clean run apart from a wrong-parser
    /// config that saw plenty of output and matched zero diagnostics — the
    /// [`DiagGroup`] list alone can't distinguish them. Ignored by the
    /// `run_check` MCP renderer (`graph::mcp::fmt_check_report`).
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

/// V22 Phase E: the `checks_test` dry-run result the ChecksEditor renders
/// inline. Built from a [`CheckReport`] (`changed_only = false`) plus the
/// captured output sizes, or carries a `validate`/spawn `error` message when the
/// run never produced a report. `diag_count` is the number of deduplicated
/// diagnostic groups; `diagnostics` is the first few of them (capped) for the
/// preview. This type is NOT covered by the Rust↔TS field tripwire (that only
/// pins `CheckDef`/`ParserKind`) — its mirror is `ChecksTestResult` in
/// `types.ts`, kept in sync by hand.
#[derive(Clone, Debug, Serialize)]
pub struct ChecksTestResult {
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// Number of deduplicated diagnostic groups the parser produced.
    pub diag_count: usize,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    /// The first [`TEST_DIAG_CAP`] diagnostic groups, for the inline preview.
    pub diagnostics: Vec<TestDiag>,
    /// A validation (bad `regex-custom`, escaping `cwd`/`report_file`) or spawn
    /// error — present iff the run never yielded a report. `null` on the wire.
    pub error: Option<String>,
}

/// V22 Phase E: one diagnostic group summarized for the Test-button preview.
#[derive(Clone, Debug, Serialize)]
pub struct TestDiag {
    pub severity: String,
    pub message: String,
    /// `"file:line"` sample locations (already capped to [`MAX_SITES`] by
    /// [`run`]); a location-less group has an empty list.
    pub sites: Vec<String>,
}

/// Cap on diagnostic groups echoed in a [`ChecksTestResult`] preview.
const TEST_DIAG_CAP: usize = 5;

/// V22 Phase E: dry-run `def` through [`run`] (`changed_only = false`) and shape
/// the outcome for the Settings "Test" button — exit status, parsed diagnostic
/// count, the first few diagnostics, and the raw output sizes that let the UI
/// flag a wrong-parser config (output produced, zero diagnostics). A
/// validation/spawn failure is captured into `error` rather than propagated, so
/// the editor can render it inline like any other test outcome.
pub async fn test_check(
    root: &Path,
    def: &CheckDef,
    sandbox: &crate::sandbox::SandboxCfg,
) -> ChecksTestResult {
    match run(root, def, false, sandbox).await {
        Ok(report) => {
            let diagnostics = report
                .groups
                .iter()
                .take(TEST_DIAG_CAP)
                .map(|g| TestDiag {
                    severity: g.severity.as_str().to_string(),
                    message: g.message.clone(),
                    sites: g
                        .sites
                        .iter()
                        .map(|(f, l)| {
                            if *l > 0 {
                                format!("{f}:{l}")
                            } else {
                                f.clone()
                            }
                        })
                        .collect(),
                })
                .collect();
            ChecksTestResult {
                exit_code: report.exit_code,
                duration_ms: report.duration_ms,
                timed_out: report.timed_out,
                diag_count: report.groups.len(),
                stdout_bytes: report.stdout_bytes,
                stderr_bytes: report.stderr_bytes,
                diagnostics,
                error: None,
            }
        }
        Err(e) => ChecksTestResult {
            exit_code: None,
            duration_ms: 0,
            timed_out: false,
            diag_count: 0,
            stdout_bytes: 0,
            stderr_bytes: 0,
            diagnostics: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

/// Cap on sample locations kept per [`DiagGroup`].
const MAX_SITES: usize = 5;

/// Cap on captured bytes per stream (stdout/stderr). Generous — diagnostics
/// are deduped after parsing, so a chatty checker doesn't blow up the report,
/// but bounds memory on a runaway process the way `run_command` does.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Run `def.cmd` (via the shell, cwd = `root`, or the confined `def.cwd` under
/// it when set) and return its deduplicated diagnostics. `changed_only` filters
/// the resulting groups to sites in files touched since HEAD (tracked diff ∪
/// untracked, via [`gitls::changed_files`]); a git failure (not a repo, no
/// commits yet, ...) degrades to **unfiltered** rather than erroring or
/// silently returning nothing, so `run_check` still works outside git. When
/// `def.report_file` is set the parser reads that file's content instead of
/// stdout; either way diagnostic file paths come back project-root-relative.
///
/// `sandbox` is the V33 OS-sandbox config, threaded in from the caller's live
/// settings rather than read from a global — the same plumbing discipline
/// `run_command` follows, so the headless MCP child and unit tests get exactly
/// the config their caller passed. Every real caller derives it with
/// [`crate::sandbox::SandboxCfg::from_settings`]; tests pass
/// `SandboxCfg::disabled()`.
pub async fn run(
    root: &Path,
    def: &CheckDef,
    changed_only: bool,
    sandbox: &crate::sandbox::SandboxCfg,
) -> AppResult<CheckReport> {
    def.validate()?;
    // **A relative root is never this project.** Every path below resolves
    // against it — the effective cwd the checker runs in, the confinement
    // boundary, the sandbox's grants and drive mapping — and a relative one
    // resolves against the *cImp process's* working directory, i.e. cImp's own
    // install directory. Running a build or a linter there and reporting the
    // result as this project's is the same failure shape `Prepared::cwd_under`
    // is written to prevent: a green run of the wrong thing, which is worse
    // than a failure. It arrives here whenever the caller could not resolve a
    // project root at all — live rc.9: `POST /graph_run` with no `cwd` in the
    // body defaults to `"."`, and `run_graph_tool`'s `run_check` arm falls back
    // to that cwd as the root. Refuse it, and say which half is missing.
    if !root.is_absolute() {
        return Err(AppError::Checks(format!(
            "check `{}` was not run: `{}` is not an absolute project root — the calling session \
             supplied no working directory, so cImp cannot tell which project (or which \
             directory) this check belongs to",
            def.name,
            root.display()
        )));
    }
    let started = Instant::now();
    let timeout_secs = def.timeout_secs.max(10);

    // Effective cwd: the confined `def.cwd` under the project root (nested
    // manifests / monorepos), else the root itself. Kept in `root`-joined
    // (non-canonical) form so it still textually matches the tool's own
    // reported paths that `parsers` relativizes (jest/eslint absolutes).
    let effective_cwd = match def.cwd.as_deref().filter(|s| !s.is_empty()) {
        Some(rel) => confine_under_root(root, root, "cwd", rel)?,
        None => root.to_path_buf(),
    };

    let (exit_code, stdout, stderr, timed_out) = spawn_capture(
        root,
        &effective_cwd,
        &def.name,
        &def.cmd,
        &def.env,
        timeout_secs,
        sandbox,
    )
    .await?;
    let duration_ms = started.elapsed().as_millis() as u64;
    // Raw captured sizes, before parsing — the "did the command produce output?"
    // signal the Phase E Test button uses (see [`CheckReport::stdout_bytes`]).
    let (stdout_bytes, stderr_bytes) = (stdout.len(), stderr.len());

    // Parser input: a configured `report_file`'s content (for tools that write
    // XML/JSON to disk, not to a pipe) else stdout+stderr. A missing/unreadable
    // report file is an explicit error diagnostic — never a silent green run.
    let mut report_error: Option<Diag> = None;
    let mut diags = match def.report_file.as_deref().filter(|s| !s.is_empty()) {
        Some(rel) => match read_report_file(root, &effective_cwd, rel).await {
            Ok(content) => parsers::parse(
                def.parser,
                &content,
                "",
                &effective_cwd,
                def.pattern.as_deref(),
            ),
            Err(msg) => {
                report_error = Some(Diag {
                    severity: Severity::Error,
                    code: None,
                    message: msg,
                    file: rel.to_string(),
                    line: 0,
                    col: None,
                });
                Vec::new()
            }
        },
        None => parsers::parse(
            def.parser,
            &stdout,
            &stderr,
            &effective_cwd,
            def.pattern.as_deref(),
        ),
    };
    // Diagnostics parsed against the effective cwd are relative to it; re-root
    // them under the project root so the report — and the `changed_only` git
    // comparison below — stay project-root-relative even for a nested `cwd`.
    if let Some(rel) = def.cwd.as_deref().filter(|s| !s.is_empty()) {
        reroot_diags(&mut diags, rel);
    }
    // The `report_file` error diag (if any) carries the raw configured path
    // (cwd-relative, as the user typed it) — append it after re-rooting so its
    // path isn't double-prefixed with `cwd`.
    if let Some(err) = report_error {
        diags.push(err);
    }
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
        groups.retain(|g| {
            g.sites
                .iter()
                .any(|(f, _)| changed.contains(&normalize_rel(root, f)))
        });
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
        stdout_bytes,
        stderr_bytes,
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
        let key = format!(
            "{}|{}|{}",
            d.severity.as_str(),
            d.code.as_deref().unwrap_or(""),
            normalized
        );
        if !map.contains_key(&key) {
            order.push(key.clone());
            let message = match &d.code {
                Some(c) => format!("{c}: {normalized}"),
                None => normalized,
            };
            map.insert(
                key.clone(),
                DiagGroup {
                    key: key.clone(),
                    severity: d.severity,
                    message,
                    count: 0,
                    sites: Vec::new(),
                },
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
        sites.sort_by_key(|(f, _)| {
            if changed.contains(&normalize_rel(root, f)) {
                0
            } else {
                1
            }
        });
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

/// The lexical half of confinement: reject an absolute path or any `..`
/// component up front, so escaping fails even when the target (or its
/// ancestors) don't exist on disk yet (a `report_file` the run will write).
fn lexically_confined(field: &str, rel: &str) -> AppResult<()> {
    let raw = Path::new(rel);
    if raw.is_absolute() {
        return Err(AppError::Checks(format!(
            "check `{field}` must be relative to the project root, not an absolute path (`{rel}`)"
        )));
    }
    if raw
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AppError::Checks(format!(
            "check `{field}` `{rel}` escapes the project root (`..` is not allowed)"
        )));
    }
    Ok(())
}

/// Resolve `rel` (a `cwd`/`report_file`) against `base`, confined strictly
/// beneath `root` — the `ToolCtx::confine` approach (canonicalize + `starts_with`
/// the canonical root), extended so the target need not exist yet: a check's
/// `report_file` is written BY the run, so when the full path doesn't resolve
/// we canonicalize its nearest existing ancestor and confine that instead. The
/// lexical `..`/absolute guard ([`lexically_confined`]) runs first. `base` is
/// the directory `rel` is interpreted relative to (the project `root` for a
/// `cwd`; the check's effective cwd for a `report_file`), while `root` stays
/// the confinement boundary — so a nested-`cwd` `report_file` still can't
/// escape the project even though it joins onto a subdir. Returns the
/// (non-canonical) `base`-joined path, so it still textually matches the tool's
/// own reported paths that `parsers` relativizes.
fn confine_under_root(root: &Path, base: &Path, field: &str, rel: &str) -> AppResult<PathBuf> {
    lexically_confined(field, rel)?;
    let joined = base.join(rel);
    // Canonicalize the deepest existing part of `joined` and confirm it stays
    // under the canonical `root` — the shared symlink-aware boundary check
    // ([`crate::fsutil::confine_creatable`]), which also confines a not-yet-
    // created `report_file` via its nearest existing ancestor. The returned
    // (non-canonical) `joined` still textually matches the tool's own reported
    // paths that `parsers` relativizes.
    match crate::fsutil::confine_creatable(root, &joined) {
        Ok(_) => Ok(joined),
        Err(crate::fsutil::ConfineError::Boundary(e)) => Err(AppError::Checks(format!(
            "cannot resolve project root: {e}"
        ))),
        Err(crate::fsutil::ConfineError::Escaped) => Err(AppError::Checks(format!(
            "check `{field}` `{rel}` resolves outside the project root"
        ))),
        // `confine_creatable` never reports a missing target as an error.
        Err(crate::fsutil::ConfineError::NotFound) => Ok(joined),
    }
}

/// Read a confined `report_file` (capped at [`MAX_OUTPUT_BYTES`]) for use as
/// the parser input. `rel` is resolved relative to the check's working
/// directory — `cwd` (the `root`-joined effective cwd) when the check sets one,
/// else `root` itself — matching the mental model that a tool documents its
/// output paths relative to where it runs (`mvn` writes `target/surefire-reports`
/// under its module dir, not the repo root). Resolution order, deterministic:
///   1. cwd-relative (`cwd.join(rel)`) — the primary, preferred location;
///   2. back-compat fallback for configs written before this fix (root-relative
///      *with* a `cwd` set): only when a `cwd` is set AND the cwd-relative path
///      does not exist, try `root.join(rel)` and use it if THAT exists.
///
/// Whichever exists wins; cwd-relative wins when both do; when neither exists
/// the error names the cwd-relative (preferred) path. Confinement under the
/// canonical `root` is enforced for every candidate ([`confine_under_root`]).
/// `Err` carries a ready-to-surface message; [`run`] turns it into an explicit
/// error [`Diag`] rather than a silent empty run.
async fn read_report_file(root: &Path, cwd: &Path, rel: &str) -> Result<String, String> {
    // Primary: resolve against the effective cwd (== `root` when no cwd).
    let primary = confine_under_root(root, cwd, "report_file", rel).map_err(|e| e.to_string())?;
    let path = if cwd != root && !tokio::fs::try_exists(&primary).await.unwrap_or(false) {
        // Back-compat: an older root-relative config with a `cwd` set. Use the
        // root-relative location only when it actually exists; otherwise keep
        // `primary` so the "could not be read" error points at the preferred
        // (cwd-relative) path the new semantics expect.
        let fallback =
            confine_under_root(root, root, "report_file", rel).map_err(|e| e.to_string())?;
        if tokio::fs::try_exists(&fallback).await.unwrap_or(false) {
            fallback
        } else {
            primary
        }
    } else {
        primary
    };
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("report_file `{rel}` could not be read: {e}"))?;
    let end = bytes.len().min(MAX_OUTPUT_BYTES);
    Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// Prefix relative diagnostic file paths with `cwd_rel` so a check that ran in
/// a nested `cwd` still reports project-root-relative paths — its tools (and
/// `parsers::relativize` for jest/eslint's absolutes) report paths relative to
/// the *run* directory. Absolute paths (a diag `parse` couldn't relativize,
/// i.e. one outside the effective cwd) are left as-is.
fn reroot_diags(diags: &mut [Diag], cwd_rel: &str) {
    let prefix = cwd_rel.replace('\\', "/");
    let prefix = prefix
        .trim_matches('/')
        .trim_start_matches("./")
        .trim_end_matches('/');
    if prefix.is_empty() {
        return;
    }
    for d in diags.iter_mut() {
        if Path::new(&d.file).is_absolute() {
            continue;
        }
        let f = d.file.replace('\\', "/");
        let f = f.trim_start_matches("./");
        d.file = format!("{prefix}/{f}");
    }
}

/// Spawn `cmd` via the platform shell (cwd = `cwd`), console-suppressed on
/// Windows, capturing stdout/stderr separately (reader tasks that outlive the
/// timeout, so a killed process still yields whatever it had already printed —
/// the "parse partial output" half of the timeout contract). `env` is forced
/// onto the child (V22 Phase B — same shape `CommandPolicy` uses for
/// `run_command`). Returns `(exit_code, stdout, stderr, timed_out)`; `Err`
/// only for a spawn failure (bad shell, permissions), never for the checked
/// command's own exit code.
///
/// # The sandbox fork (V33)
///
/// `root` is the PROJECT root (the sandbox's writable area and the drive it
/// maps); `cwd` is where this check actually runs, which may be a directory
/// beneath it (`CheckDef::cwd`). When the OS sandbox is on and available the
/// shell runs inside the AppContainer instead, with the C2 minimal environment
/// as its base — see [`spawn_capture_sandboxed`]. The plain path below is
/// unchanged in every respect, including its inherit-and-force environment: a
/// sandbox-off user's checks behave exactly as they did.
///
/// `name` is [`CheckDef::name`] — the identity every sandbox-lane row this seam
/// writes is scanned by. NOT the shell: `cmd.exe` is what every check spawns,
/// so a program-derived row would render them all identically and collapse them
/// into one confirmation. See [`row_subject`].
#[allow(clippy::too_many_arguments)]
async fn spawn_capture(
    root: &Path,
    cwd: &Path,
    name: &str,
    cmd: &str,
    env: &[(String, String)],
    timeout_secs: u64,
    sandbox: &crate::sandbox::SandboxCfg,
) -> AppResult<(Option<i32>, String, String, bool)> {
    let timeout = Duration::from_secs(timeout_secs);
    let subject = row_subject(name, cmd);
    // The program cImp actually spawns is the SHELL. Resolve it to an absolute
    // path for the sandbox's benefit (`prepare` grants the program's install
    // dir, and `CreateProcessW` gets no PATH search) — the plain path keeps
    // using the bare name, which is what it has always done.
    let shell = shell_program();
    // Grant inference (V33 locked decision L3): the shell needs no grant
    // (System32 is ALL APPLICATION PACKAGES-readable), but the tool the check
    // invokes does. Resolve the command's FIRST token the same way
    // `run_command` resolves its program.
    //
    // Computed ONLY when the sandbox is on. `check_program_hint` walks PATH,
    // and a sandbox-off user runs checks on every `post_edit` — making them pay
    // a `which` for a value `plan` is about to discard would be a real
    // regression for a feature they turned off.
    let (inferred, base_env) = if sandbox.enabled {
        (
            check_program_hint(cmd, &|name| crate::pty::resolve_command(name).ok())
                .into_iter()
                .collect::<Vec<PathBuf>>(),
            crate::sandbox::child_env::minimal_env(&|key| std::env::var_os(key)),
        )
    } else {
        (Vec::new(), Vec::new())
    };

    let plan = match tokio::time::timeout(
        crate::sandbox::PREPARE_BACKSTOP,
        crate::sandbox::plan(
            sandbox,
            crate::sandbox::SEAM_RUN_CHECK,
            &shell,
            &crate::sandbox::GrantHints {
                programs: inferred,
                // A check writes into the project root (already granted) or into
                // its redirected TEMP on the mapped drive; nothing outside.
                full_dirs: Vec::new(),
                // No reviewed grant rows: this seam widens the boundary only by
                // inferring the check command's own tool directory, above. The
                // row table is V33 Phase B's per-harness state paths.
                rows: Vec::new(),
            },
            root,
            &base_env,
        ),
    )
    .await
    {
        Ok(plan) => plan,
        Err(_) => {
            // Wedged BEFORE the spawn (2026-08-18's second incident shape). The
            // check was never attempted, and it must NOT fall back to a plain
            // spawn — silently dropping the boundary is worse than refusing.
            crate::sandbox::record_event(
                crate::sandbox::SEAM_RUN_CHECK,
                root,
                "wedged",
                crate::sandbox::state_target("wedged", &subject),
                format!(
                    "sandbox preparation for check `{cmd}` did not settle within {}s \
                     (profile / ACL grants / drive mapping). The check was NOT run — refusing \
                     rather than silently dropping the sandbox boundary.",
                    crate::sandbox::PREPARE_BACKSTOP.as_secs(),
                ),
                false,
            );
            return Err(AppError::Checks(format!(
                "sandbox preparation did not settle within {}s — treating as wedged \
                 (see the sandbox lane); check `{cmd}` was not run",
                crate::sandbox::PREPARE_BACKSTOP.as_secs()
            )));
        }
    };
    #[cfg(windows)]
    if let crate::sandbox::Plan::Sandboxed(prepared) = &plan {
        return spawn_capture_sandboxed(
            prepared, root, cwd, &subject, cmd, env, &base_env, timeout, sandbox,
        )
        .await;
    }
    if let crate::sandbox::Plan::Plain(reason) = &plan {
        // Decision 5: degradation is loud, never silent. Deduplicated by
        // (seam, reason) per session inside `record_skip`.
        crate::sandbox::record_skip(crate::sandbox::SEAM_RUN_CHECK, reason, &subject, root);
    }

    let mut command = shell_command(cmd);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Kill the child if this future is dropped, so an aborted `run` call
        // never leaks an orphaned checker process.
        .kill_on_drop(true);
    // Force the configured env onto the child (values redacted in `Debug`).
    for (k, v) in env {
        command.env(k, v);
    }
    // Don't flash a console window for each spawned checker on Windows —
    // same CREATE_NO_WINDOW convention as every other spawned subprocess
    // (offload's `llama-server`, MCP host, `run_command`).
    #[cfg(windows)]
    command.creation_flags(crate::procutil::CREATE_NO_WINDOW);
    // V33 C3: Unix-only — give the shell its own process group so the timeout
    // path below can `killpg` the whole thing. A check command is a shell
    // string, so the process cImp holds is `sh`, and everything the check
    // actually runs is its child; without the group, killing `sh` leaves the
    // real work running and holding the pipe write ends.
    crate::procutil::own_process_group(&mut command);

    // V33 Phase D — on Linux this IS the sandboxed path: Landlock is applied to
    // the shell command built above rather than through a second spawn
    // mechanism. Locked decision L4 still holds, and it is `apply` that
    // enforces it — the confined shell gets the C2 minimal base, then
    // `CheckDef::env`, then the sandbox's TMPDIR/HOME redirections last,
    // replacing the inherit-and-force environment the plain path keeps. An
    // error REFUSES the check rather than running it unconfined (decision D3).
    #[cfg(target_os = "linux")]
    if let crate::sandbox::Plan::Sandboxed(prepared) = &plan {
        prepared
            .apply(
                &mut command,
                &base_env,
                env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            )
            .map_err(AppError::Checks)?;
    }

    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let mut child = crate::spawn_gate::spawn_tokio(&mut command)
        .map_err(|e| AppError::Checks(format!("failed to spawn check `{cmd}`: {e}")))?;
    // Backstop: reap this checker subprocess via the kill-on-job-close job if
    // cImp dies hard before `kill_on_drop` can fire.
    crate::process_guard::guard_child(&child);
    // The confirmation row, once per CHECK per session — the subject is the
    // configured name, never `sh`, for the reason `row_subject` documents.
    #[cfg(target_os = "linux")]
    if matches!(&plan, crate::sandbox::Plan::Sandboxed(_)) {
        crate::sandbox::record_sandboxed(crate::sandbox::SEAM_RUN_CHECK, root, &subject, sandbox);
    }

    // Drain stdout/stderr on their own tasks so the buffers survive a timeout
    // on `child.wait()` below — killing the child for a timeout only closes
    // its pipes (which cleanly EOFs these readers), it doesn't discard what
    // was already captured.
    let out_task = tokio::spawn(crate::procutil::read_capped(
        child.stdout.take(),
        MAX_OUTPUT_BYTES,
    ));
    let err_task = tokio::spawn(crate::procutil::read_capped(
        child.stderr.take(),
        MAX_OUTPUT_BYTES,
    ));

    let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => (status.code(), false),
            Ok(Err(e)) => return Err(AppError::Checks(format!("check `{cmd}` failed: {e}"))),
            Err(_) => {
                // Timed out: kill the whole tree (kill_on_drop is a backstop, not a
                // guarantee the process is gone by the time we read the buffers
                // below — and a checker's own children must not survive it) and reap.
                crate::procutil::kill_tree(&mut child).await;
                (None, true)
            }
        };

    // Bounded: a checker grandchild still holding a pipe write end must not
    // hang the check run forever (truncation is irrelevant here — parsers
    // treat output as best-effort text).
    let (stdout, _) = crate::procutil::drain_capture(out_task).await;
    let (stderr, _) = crate::procutil::drain_capture(err_task).await;

    // V33 Phase D — the Linux denial row, minted where the raw exit code and
    // stderr still exist (a nonzero exit is data to this function's callers).
    // Not for a timeout: a hang matches no access-denial signature, and
    // guessing would put noise in the one lane that is supposed to mean
    // something.
    #[cfg(target_os = "linux")]
    {
        let confined_and_finished =
            !timed_out && matches!(&plan, crate::sandbox::Plan::Sandboxed(_));
        let class = confined_and_finished
            .then(|| crate::sandbox::denial_signature(exit_code, &stderr, sandbox.allow_network))
            .flatten();
        if let Some(class) = class {
            crate::sandbox::record_denial(
                crate::sandbox::SEAM_RUN_CHECK,
                root,
                &subject,
                &[cmd.to_string()],
                exit_code,
                &stderr,
                class,
                sandbox,
            );
        }
    }
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

/// The shell [`shell_command`] spawns, as an ABSOLUTE path.
///
/// The plain path spawns it by bare name and lets the OS resolve it; the
/// sandboxed path cannot — `CreateProcessW` is handed a full command line with
/// no PATH search, and `prepare` grants "the program's install directory",
/// which needs a directory to name. On Windows that is `%ComSpec%` (falling
/// back to `%SystemRoot%\System32\cmd.exe`), which lives under `System32` and
/// is therefore already readable by `ALL APPLICATION PACKAGES` — so this path
/// gets NO ACE stamped on it (`grant_dir`'s `is_app_package_readable` guard),
/// which is exactly what we want for a system directory.
fn shell_program() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(spec) = std::env::var_os("ComSpec") {
            let p = PathBuf::from(spec);
            if p.is_absolute() {
                return p;
            }
        }
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        system_root.join("System32").join("cmd.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/bin/sh")
    }
}

/// **What this seam's sandbox-lane rows are scanned by: the CHECK.**
///
/// The other two seams identify a row by the program they spawn, because that
/// program *is* the work. This one always spawns `cmd.exe`, so doing the same
/// would render every check's row identically — `sandboxed — cmd.exe`, over and
/// over — and, worse, collapse them: the confirmation row dedups per subject,
/// so the first sandboxed check would speak for every other one. The check's
/// configured [`CheckDef::name`] is both the distinguishing fact and the name
/// the user would look for. The full command line stays in the row's detail.
///
/// Falls back to the command's first token, then to a fixed label, so a check
/// saved with a blank name (the struct default) still produces a scannable row
/// instead of a dangling `"sandboxed — "`.
fn row_subject(name: &str, cmd: &str) -> String {
    let name = name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    first_shell_token(cmd).unwrap_or_else(|| "unnamed check".to_string())
}

/// **Grant inference for a shell-mediated check** (V33 locked decision L3).
///
/// The sandbox grants the *spawned program's* install directory, but this seam
/// spawns the shell — so the tool that does the actual work (`cargo` in
/// `cargo test --bin cimp`) would get no grant and die with an access denial
/// the moment the container tried to read its image. This resolves the command
/// line's FIRST token through the same resolver `run_command` uses for its
/// program, so that tool's install directory is granted read+execute too.
///
/// **What is deliberately NOT inferred**, so a later reader does not "improve"
/// it into a shell parser:
///
/// * later tokens of a compound command line (`cargo build && npm test`) — they
///   rely on an already-readable install dir (Program Files / Windows) or on a
///   `sandbox.extra_grant_dirs` row the user added deliberately;
/// * anything that is not a plain first token: a shell builtin (`echo`), an
///   `ENV=value cmd` prefix, a redirection or a subshell.
///
/// Returning `None` is a valid, non-failing answer: the shell itself still runs
/// (its own directory needs no grant), and a tool that then cannot start
/// surfaces as a loud DENIAL row rather than a silent unsandboxed retry.
///
/// `resolve` is a parameter so the rule is testable without touching PATH.
fn check_program_hint(
    cmd: &str,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let token = first_shell_token(cmd)?;
    resolve(&token)
}

/// The first token of a shell command line, when it plausibly names a program.
///
/// Honors a quoted first token (`"C:\Program Files\...\tsc.cmd" --noEmit`, the
/// exact shape [`shell_command`]'s `raw_arg` doc calls out). Returns `None` for
/// anything that is not a bare leading program name: an empty command, an
/// `ENV=value` prefix, or a token carrying shell metacharacters that mean the
/// line starts with something other than a program.
fn first_shell_token(cmd: &str) -> Option<String> {
    let trimmed = cmd.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let token: String = if let Some(rest) = trimmed.strip_prefix('"') {
        // A quoted program path; everything up to the closing quote.
        match rest.split_once('"') {
            Some((inside, _)) => inside.to_string(),
            // Unterminated quote — not something to guess about.
            None => return None,
        }
    } else {
        trimmed
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or_default()
            .to_string()
    };
    if token.is_empty() {
        return None;
    }
    // `FOO=bar cmd` (sh) sets a variable; the program is the NEXT token, and
    // guessing which is not this function's job.
    if token.contains('=') {
        return None;
    }
    // Metacharacters mean the line does not begin with a plain program name
    // (a subshell, a redirect, a pipeline written without a leading space…).
    if token.contains(['&', '|', '<', '>', '(', ')', ';', '^', '%', '"', '\'']) {
        return None;
    }
    Some(token)
}

/// V33 — run one configured check's shell INSIDE the AppContainer.
///
/// Mirrors the plain path's contract (same output caps, same timeout, same
/// `(exit_code, stdout, stderr, timed_out)` shape) and differs only in the OS
/// boundary and — per locked decision L4 — in the environment: a sandboxed
/// child gets the C2 minimal base rather than cImp's whole environment, then
/// `CheckDef::env` on top, then the sandbox's TEMP/HOME redirections last.
///
/// The cwd is `cwd` re-expressed on the mapped drive
/// ([`crate::sandbox::windows::Prepared::cwd_under`]), so a nested
/// `CheckDef::cwd` still runs where it is configured to run.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn spawn_capture_sandboxed(
    prepared: &crate::sandbox::windows::Prepared,
    root: &Path,
    cwd: &Path,
    subject: &str,
    cmd: &str,
    env: &[(String, String)],
    base_env: &[(&str, std::ffi::OsString)],
    timeout: Duration,
    sandbox: &crate::sandbox::SandboxCfg,
) -> AppResult<(Option<i32>, String, String, bool)> {
    let shell = shell_program();
    let mut child_env = crate::sandbox::child_env::ChildEnv::from_base(base_env);
    // The check's own forced variables (V22 Phase B), then the sandbox's
    // redirections LAST — those point at the mapped drive and must win.
    child_env.overlay(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    child_env.overlay(
        prepared
            .env_overrides
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone())),
    );
    let child_env = child_env.into_pairs();

    // `/C <cmd>` goes in as a RAW tail, not as a quoted argument — `cmd.exe`
    // parses its tail with its own rules, exactly as `shell_command`'s
    // `raw_arg` doc explains. Quoting it would double-escape a check command
    // that contains its own quotes, and the two paths must agree.
    let raw_tail = format!("/C {cmd}");
    let settled = tokio::time::timeout(
        crate::sandbox::backstop_for(timeout),
        crate::sandbox::windows::spawn_and_capture(
            prepared,
            crate::sandbox::windows::SpawnRequest {
                program: &shell,
                args: &[],
                raw_tail: Some(&raw_tail),
                env: &child_env,
                cwd: &prepared.cwd_under(cwd),
                cap: MAX_OUTPUT_BYTES,
                timeout,
                // This seam has no cancel channel (a check run is bounded by
                // its own timeout and by the caller dropping the future, which
                // the plain path handles with `kill_on_drop`).
                cancel: None,
            },
        ),
    )
    .await;
    let run = match settled {
        Err(_) => {
            crate::sandbox::record_event(
                crate::sandbox::SEAM_RUN_CHECK,
                root,
                "wedged",
                crate::sandbox::state_target("wedged", subject),
                format!(
                    "check `{cmd}` did not settle within {}s (check timeout {}s + {}s settle \
                     slack). The sandboxed spawn helper never returned; the child may have run, \
                     may still be running, or may never have started — cImp cannot tell, so this \
                     row asserts only the wedge.",
                    crate::sandbox::backstop_for(timeout).as_secs(),
                    timeout.as_secs(),
                    crate::sandbox::SANDBOX_SETTLE_SLACK.as_secs(),
                ),
                false,
            );
            return Err(AppError::Checks(format!(
                "sandboxed check spawn did not settle within {}s — the check may have run; \
                 treating as wedged (see the sandbox lane)",
                crate::sandbox::backstop_for(timeout).as_secs()
            )));
        }
        Ok(Ok(run)) => run,
        Ok(Err(e)) => {
            // Decision 4: a `CreateProcessW` that refuses to start the child is
            // itself a denial shape, so its error string goes through the same
            // classifier — with no exit code, because nothing ran. An error the
            // classifier does NOT recognize still mints a row (`refused`): this
            // seam's failure is otherwise visible only in the check result, and
            // that is exactly how rc.9's `CreateProcessW failed (267)` left the
            // sandbox lane silent about a spawn that never happened.
            crate::sandbox::record_spawn_failure(
                crate::sandbox::SEAM_RUN_CHECK,
                root,
                subject,
                &[cmd.to_string()],
                &e,
                sandbox,
            );
            return Err(AppError::Checks(format!(
                "failed to spawn sandboxed check `{cmd}`: {e}"
            )));
        }
    };
    crate::sandbox::record_sandboxed(crate::sandbox::SEAM_RUN_CHECK, root, subject, sandbox);

    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    if !run.timed_out {
        // A nonzero exit is returned to the caller as data (parsers read it),
        // so this is the last point at which the raw code and stderr exist.
        if let Some(class) =
            crate::sandbox::denial_signature(run.exit_code, &stderr, sandbox.allow_network)
        {
            crate::sandbox::record_denial(
                crate::sandbox::SEAM_RUN_CHECK,
                root,
                subject,
                &[cmd.to_string()],
                run.exit_code,
                &stderr,
                class,
                sandbox,
            );
        }
    }
    if run.drains_leaked {
        // The capture is INCOMPLETE; a parser told nothing would read the gap
        // as "the checker printed no diagnostics", i.e. as a clean run.
        tracing::warn!(
            check = %cmd,
            "sandbox: a pipe drain never finished (leaked write end) — the check's captured \
             output is incomplete"
        );
        stderr.push_str(
            "\n[sandbox: one output stream could not be drained — a copy of its pipe leaked to \
             another process, so part of this output is missing]",
        );
    }
    // The plain path reports a timeout with NO exit code; match it exactly so
    // the two paths produce the same `CheckReport`.
    let exit_code = if run.timed_out { None } else { run.exit_code };
    Ok((exit_code, stdout, stderr, run.timed_out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(severity: Severity, code: Option<&str>, message: &str, file: &str, line: u32) -> Diag {
        Diag {
            severity,
            code: code.map(str::to_string),
            message: message.to_string(),
            file: file.to_string(),
            line,
            col: None,
        }
    }

    /// **The rc.9 live defect, at the seam that must not run.** A check whose
    /// project root is relative would resolve every path — the effective cwd,
    /// the confinement boundary, the sandbox's grants and drive mapping —
    /// against cImp's OWN working directory, i.e. its install directory. The
    /// sandboxed spawn died there with an unattributable `CreateProcessW failed
    /// (267)`; the PLAIN spawn would have quietly run the build command in the
    /// wrong directory and reported the result as this project's.
    ///
    /// So `run` refuses before it spawns anything, on both paths — asserted
    /// with the sandbox OFF, because the plain path is the one where the wrong
    /// answer would have looked like a real one.
    #[tokio::test]
    async fn a_relative_project_root_is_refused_before_anything_spawns() {
        let def = CheckDef {
            name: "cargo-check".into(),
            cmd: "cargo check".into(),
            parser: ParserKind::CargoJson,
            timeout_secs: 30,
            // The nested-cwd shape the live check used; the root is what is
            // broken here, not the cwd.
            cwd: Some("src-tauri".into()),
            ..Default::default()
        };
        for root in [".", "", "some/relative/dir"] {
            let err = run(
                Path::new(root),
                &def,
                false,
                &crate::sandbox::SandboxCfg::disabled(),
            )
            .await
            .expect_err("a relative project root must not run a check");
            let msg = err.to_string();
            assert!(
                msg.contains("absolute project root") && msg.contains("cargo-check"),
                "the error must name the check and the missing half: {msg}"
            );
        }
    }

    // ── V33 — the sandboxed `run_check` seam ──────────────────────────────

    /// **Grant inference, decision L3.** The sandbox grants the *spawned*
    /// program's directory, and this seam spawns the shell — so a check's own
    /// tool gets its grant from the command line's first token or not at all.
    /// A resolvable token yields the program (whose PARENT `prepare` grants);
    /// a builtin or a line that does not start with a plain program name yields
    /// nothing, which is a valid answer: the shell still runs.
    ///
    /// The resolver is injected, so this pins the RULE rather than whatever
    /// happens to be on the machine's PATH.
    #[test]
    fn the_first_token_of_a_check_command_is_what_gets_a_grant() {
        // Forward slashes throughout: `Path` treats `/` as a separator on BOTH
        // platforms, while a backslash is an ordinary character on Linux — a
        // `C:\...` fixture would make `parent()` answer `""` on the Linux CI
        // runner and this test would pass locally and fail there.
        let installed = |name: &str| -> Option<PathBuf> {
            match name {
                "cargo" => Some(PathBuf::from("/home/me/.cargo/bin/cargo")),
                "npm" => Some(PathBuf::from("/opt/nodejs/npm")),
                // A quoted first token is passed through verbatim, spaces and
                // all — the shape `shell_command`'s `raw_arg` doc calls out.
                "/opt/my tools/tsc.cmd" => Some(PathBuf::from("/opt/my tools/tsc.cmd")),
                // Everything else is a shell builtin or simply absent.
                _ => None,
            }
        };
        let hint = |cmd: &str| check_program_hint(cmd, &installed);

        // The ordinary case: the tool that does the work gets resolved, and its
        // parent is the directory `prepare` will grant read+execute.
        let cargo = hint("cargo test --bin cimp").expect("cargo must be inferred");
        assert_eq!(cargo, PathBuf::from("/home/me/.cargo/bin/cargo"));
        assert_eq!(cargo.parent().unwrap(), Path::new("/home/me/.cargo/bin"));
        assert!(hint("npm run lint").is_some());
        // Leading whitespace is not a different command.
        assert!(hint("   cargo clippy").is_some());
        // A quoted first token — the `raw_arg` doc's own example shape (a
        // program path containing a space, which is why it is quoted at all).
        assert!(hint("\"/opt/my tools/tsc.cmd\" --noEmit").is_some());

        // A shell builtin resolves to nothing: no grant, and the shell still
        // runs the check.
        assert!(hint("echo hello").is_none(), "a builtin needs no grant");
        // Only the FIRST token is inferred — later ones rely on an
        // already-readable dir or on `extra_grant_dirs`.
        assert_eq!(
            hint("cargo build && npm test"),
            Some(PathBuf::from("/home/me/.cargo/bin/cargo")),
            "the first token is inferred and the rest deliberately are not"
        );
        // Not-a-program shapes yield nothing rather than a guess.
        assert!(hint("").is_none());
        assert!(hint("   ").is_none());
        assert!(hint("RUST_LOG=debug cargo test").is_none(), "env prefix");
        assert!(hint("(cargo test)").is_none(), "subshell");
        assert!(hint("| cargo test").is_none(), "pipeline fragment");
        assert!(hint("\"unterminated cargo").is_none(), "unbalanced quote");
    }

    /// **A check's sandbox rows are scanned by the check, not by `cmd.exe`.**
    ///
    /// Every check spawns the same shell, so a program-derived subject would
    /// render every row identically and — because the confirmation row dedups
    /// per subject — let the first sandboxed check speak for all of them.
    #[test]
    fn a_checks_sandbox_rows_are_identified_by_its_configured_name() {
        assert_eq!(row_subject("cargo", "cargo test --bin cimp"), "cargo");
        assert_eq!(row_subject("  tsc  ", "tsc --noEmit"), "tsc");
        // Two checks that run the same shell are still two subjects.
        assert_ne!(
            row_subject("cargo", "cargo test"),
            row_subject("clippy", "cargo clippy")
        );
        // …and the row a user scans says so, with no shell in sight.
        let target = crate::sandbox::state_target("sandboxed", &row_subject("cargo", "cargo test"));
        assert_eq!(target, "sandboxed — cargo");
        assert!(!target.contains("cmd"));

        // A blank name (the `CheckDef` default) still yields something
        // scannable rather than a dangling `"sandboxed — "`.
        assert_eq!(row_subject("", "cargo test --bin cimp"), "cargo");
        assert_eq!(row_subject("   ", "echo hi"), "echo");
        // …even when the command line names no plain program at all.
        assert_eq!(row_subject("", "(cargo test)"), "unnamed check");
        assert!(!row_subject("", "").is_empty());
    }

    /// The shell this seam spawns must be an ABSOLUTE path — `CreateProcessW`
    /// does no PATH search, and `prepare` grants "the program's directory",
    /// which a bare `cmd` does not have.
    #[test]
    fn the_shell_program_is_an_absolute_path_with_a_parent_directory() {
        let shell = shell_program();
        assert!(shell.is_absolute(), "{}", shell.display());
        assert!(
            shell.parent().is_some_and(|p| !p.as_os_str().is_empty()),
            "the sandbox grants the program's PARENT; {} has none",
            shell.display()
        );
        #[cfg(windows)]
        assert_eq!(
            shell
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase()),
            Some("cmd.exe".to_string()),
            "the sandboxed path must spawn the same shell the plain path does"
        );
    }

    /// The backstop relation for THIS seam, in the shape
    /// `run_command::sandbox_backstop_exceeds_the_child_timeout` established:
    /// the caller-side deadline must outlast the child's own, or an ordinary
    /// slow check would be reported as a *wedge* — the one row in that lane
    /// that is supposed to mean "something is broken in cImp".
    ///
    /// A check's timeout is per-check and floored at 10 s (see [`run`]), so the
    /// relation is asserted across the range rather than on one constant.
    #[test]
    fn the_check_sandbox_backstop_always_exceeds_the_check_timeout() {
        for secs in [10u64, 30, 120, 600, 3600] {
            let child = Duration::from_secs(secs);
            let backstop = crate::sandbox::backstop_for(child);
            assert!(
                backstop > child,
                "backstop {backstop:?} must exceed the check timeout {child:?}"
            );
            assert_eq!(backstop, child + crate::sandbox::SANDBOX_SETTLE_SLACK);
        }
        // Preparation is bounded independently of the child, because it happens
        // before the child exists.
        assert!(crate::sandbox::PREPARE_BACKSTOP >= Duration::from_secs(30));
    }

    #[test]
    fn normalize_message_replaces_quoted_spans() {
        assert_eq!(
            normalize_message("cannot find value `x` in this scope"),
            "cannot find value ‹…› in this scope"
        );
        assert_eq!(
            normalize_message("expected 'i32', found 'String'"),
            "expected ‹…›, found ‹…›"
        );
        // No quotes: unchanged.
        assert_eq!(
            normalize_message("aborting due to 2 previous errors"),
            "aborting due to 2 previous errors"
        );
        // Unterminated quote: left as a literal character, not consumed.
        assert_eq!(normalize_message("odd ' quote"), "odd ' quote");
    }

    #[test]
    fn group_dedups_but_leaves_sites_uncapped() {
        let mut diags = Vec::new();
        for i in 0..8 {
            diags.push(diag(
                Severity::Error,
                Some("E0425"),
                &format!("cannot find value `v{i}` in this scope"),
                &format!("src/f{i}.rs"),
                1,
            ));
        }
        // A different code must NOT collapse into the same group.
        diags.push(diag(
            Severity::Warning,
            Some("unused"),
            "unused variable `y`",
            "src/g.rs",
            3,
        ));

        let groups = group(diags);
        assert_eq!(
            groups.len(),
            2,
            "the 8 E0425s collapse to one group, the warning is separate: {groups:?}"
        );
        let e0425 = groups
            .iter()
            .find(|g| g.message.starts_with("E0425"))
            .expect("E0425 group");
        assert_eq!(e0425.count, 8);
        // `group` no longer caps — that's `cap_sites`'s job now, run after
        // the `changed_only` filter (see `run`'s doc comment).
        assert_eq!(
            e0425.sites.len(),
            8,
            "group() itself leaves sites uncapped: {e0425:?}"
        );
        assert_eq!(e0425.message, "E0425: cannot find value ‹…› in this scope");
    }

    #[test]
    fn cap_sites_plain_truncates_in_source_order() {
        let dir = std::env::temp_dir().join(format!("checks-capsites-{}", uuid::Uuid::new_v4()));
        let mut sites: Vec<(String, u32)> = (0..8).map(|i| (format!("src/f{i}.rs"), 1)).collect();
        cap_sites(&mut sites, &dir, None);
        assert_eq!(sites.len(), MAX_SITES);
        assert_eq!(
            sites,
            vec![
                ("src/f0.rs".to_string(), 1),
                ("src/f1.rs".to_string(), 1),
                ("src/f2.rs".to_string(), 1),
                ("src/f3.rs".to_string(), 1),
                ("src/f4.rs".to_string(), 1),
            ]
        );
    }

    #[test]
    fn cap_sites_prefers_a_changed_file_site_past_the_cap() {
        // 6 sites, none of the first 5 (source order) is the changed file —
        // it's the 6th. A naive source-order truncation would drop it; the
        // changed-file site must still show up among the capped 5.
        let dir =
            std::env::temp_dir().join(format!("checks-capsites-changed-{}", uuid::Uuid::new_v4()));
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
        let diags = parsers::parse(
            ParserKind::CargoTest,
            OUT,
            "",
            std::path::Path::new("."),
            None,
        );
        let groups = group(diags);
        let errs: Vec<_> = groups
            .iter()
            .filter(|g| g.severity == Severity::Error)
            .collect();
        assert_eq!(
            errs.len(),
            1,
            "identical assertions should group: {groups:?}"
        );
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
        let def = CheckDef {
            name: "sanity".into(),
            cmd,
            parser: ParserKind::GenericGcc,
            timeout_secs: 30,
            ..Default::default()
        };
        let report = run(
            &std::env::temp_dir(),
            &def,
            false,
            &crate::sandbox::SandboxCfg::disabled(),
        ).await.expect("run");
        assert_eq!(report.exit_code, Some(0));
        assert!(!report.timed_out);
    }

    /// The TS mirror of this module's wire types, embedded at compile time so a
    /// Rust-side change that isn't reflected in `types.ts` fails `cargo test`
    /// rather than shipping as silent Rust↔TS drift (V16 tripwire pattern; the
    /// drift the V22 spec's Phase A found — V17's `cargo-test`/`jest-json`
    /// missing from the union — is exactly what this catches). Path is relative
    /// to this file (`src-tauri/src/checks/`), up to the repo root.
    const TS_TYPES: &str = include_str!("../../../src/lib/settings/types.ts");

    /// Every [`ParserKind`] variant. The exhaustive `match` below is the
    /// Rust-side half of the tripwire: adding a variant without extending this
    /// list is a compile error, so it can't reach `cargo test` unnoticed. Wire
    /// names are then *derived* from serde (not a second hand-kept list), so
    /// this stays honest about the actual serialized form.
    fn all_parser_kinds() -> Vec<ParserKind> {
        let all = vec![
            ParserKind::CargoJson,
            ParserKind::Tsc,
            ParserKind::EslintJson,
            ParserKind::Pytest,
            ParserKind::CargoTest,
            ParserKind::JestJson,
            ParserKind::Sarif,
            ParserKind::Go,
            ParserKind::GoTestJson,
            ParserKind::Dotnet,
            ParserKind::JunitXml,
            ParserKind::RegexCustom,
            ParserKind::GenericGcc,
        ];
        fn _assert_exhaustive(k: ParserKind) {
            match k {
                ParserKind::CargoJson
                | ParserKind::Tsc
                | ParserKind::EslintJson
                | ParserKind::Pytest
                | ParserKind::CargoTest
                | ParserKind::JestJson
                | ParserKind::Sarif
                | ParserKind::Go
                | ParserKind::GoTestJson
                | ParserKind::Dotnet
                | ParserKind::JunitXml
                | ParserKind::RegexCustom
                | ParserKind::GenericGcc => {}
            }
        }
        all
    }

    /// The kebab-case wire name serde emits for `kind` (e.g. `"cargo-test"`).
    fn wire_name(kind: ParserKind) -> String {
        serde_json::to_value(kind)
            .expect("ParserKind serializes")
            .as_str()
            .expect("ParserKind serializes to a string")
            .to_string()
    }

    #[test]
    fn parser_kind_wire_names_mirrored_in_types_ts() {
        for kind in all_parser_kinds() {
            let wire = wire_name(kind);
            assert!(
                TS_TYPES.contains(&format!("'{wire}'")),
                "ParserKind `{kind:?}` (wire `{wire}`) is missing from the TS `ParserKind` \
                 union in src/lib/settings/types.ts — add it to keep the mirror in sync",
            );
        }
    }

    #[test]
    fn check_def_field_names_mirrored_in_types_ts() {
        // Serialize a fully-populated CheckDef and assert each JSON key appears
        // in types.ts — so any field added to CheckDef (Phase B's `cwd`/`env`/
        // `report_file`/`pattern`, ...) must also land in the TS interface.
        let def = CheckDef {
            name: "cargo".into(),
            cmd: "cargo check".into(),
            parser: ParserKind::CargoJson,
            timeout_secs: 120,
            // Populate every optional field so its serialized key is exercised
            // by the mirror assertion below (Phase B's `cwd`/`env`/`report_file`).
            cwd: Some("src-tauri".into()),
            env: vec![("RUSTFLAGS".into(), "-Dwarnings".into())],
            report_file: Some("target/report.xml".into()),
            pattern: Some(r"(?<file>\S+):(?<line>\d+): (?<message>.+)".into()),
            // Populate the Phase D marker too so its serialized key is exercised.
            auto: true,
        };
        let value = serde_json::to_value(&def).expect("CheckDef serializes");
        let obj = value.as_object().expect("CheckDef serializes to an object");
        for key in obj.keys() {
            assert!(
                TS_TYPES.contains(&format!("{key}:")),
                "CheckDef field `{key}` is missing from the TS `CheckDef` interface in \
                 src/lib/settings/types.ts — add it to keep the mirror in sync",
            );
        }
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
        let def = CheckDef {
            name: "slow".into(),
            cmd,
            parser: ParserKind::GenericGcc,
            timeout_secs: 1,
            ..Default::default()
        };
        let started = Instant::now();
        let report = run(
            &std::env::temp_dir(),
            &def,
            false,
            &crate::sandbox::SandboxCfg::disabled(),
        ).await.expect("run");
        assert!(report.timed_out);
        assert_eq!(report.exit_code, None);
        // Floored at 10s, generous upper bound for slow CI.
        assert!(
            started.elapsed() >= Duration::from_secs(9),
            "elapsed: {:?}",
            started.elapsed()
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "elapsed: {:?}",
            started.elapsed()
        );
    }

    // ── V22 Phase B — cwd / env / report_file ────────────────────────────

    #[test]
    fn validate_rejects_absolute_and_escaping_cwd_and_report_file() {
        let abs = if cfg!(windows) {
            "C:\\Windows\\Temp"
        } else {
            "/etc"
        };

        // cwd: escaping (`..`) and absolute are both rejected; a plain
        // relative subpath is fine.
        let mut def = CheckDef {
            cwd: Some("../outside".into()),
            ..Default::default()
        };
        assert!(def.validate().is_err(), "escaping cwd must be rejected");
        def.cwd = Some("sub/../../escape".into());
        assert!(def.validate().is_err(), "a `..` mid-path must be rejected");
        def.cwd = Some(abs.into());
        assert!(def.validate().is_err(), "absolute cwd must be rejected");
        def.cwd = Some("src-tauri".into());
        assert!(def.validate().is_ok(), "a plain relative cwd is allowed");

        // report_file: same confinement.
        let mut def = CheckDef {
            report_file: Some("../secrets.xml".into()),
            ..Default::default()
        };
        assert!(
            def.validate().is_err(),
            "escaping report_file must be rejected"
        );
        def.report_file = Some(abs.into());
        assert!(
            def.validate().is_err(),
            "absolute report_file must be rejected"
        );
        def.report_file = Some("target/surefire/TEST.xml".into());
        assert!(
            def.validate().is_ok(),
            "a plain relative report_file is allowed"
        );
    }

    #[tokio::test]
    async fn run_rejects_escaping_cwd_at_run_time() {
        // Confinement holds at run time too, not just in `validate` — `run`
        // calls `validate` before it ever spawns.
        let def = CheckDef {
            name: "x".into(),
            cmd: "echo hi".into(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 30,
            cwd: Some("../escape".into()),
            ..Default::default()
        };
        let result = run(
            &std::env::temp_dir(),
            &def,
            false,
            &crate::sandbox::SandboxCfg::disabled(),
        ).await;
        assert!(
            result.is_err(),
            "run must reject an escaping cwd: {result:?}"
        );
    }

    /// `env` entries are forced onto the spawned child — echo the sentinel back
    /// through the shell and confirm it lands in the captured stdout.
    #[tokio::test]
    async fn spawn_capture_forces_env_onto_child() {
        #[cfg(windows)]
        let cmd = "echo cimp_env=%CIMP_CHECK_ENV%".to_string();
        #[cfg(not(windows))]
        let cmd = "echo cimp_env=$CIMP_CHECK_ENV".to_string();
        let env = vec![("CIMP_CHECK_ENV".to_string(), "sentinel42".to_string())];
        let tmp = std::env::temp_dir();
        let (code, stdout, _stderr, timed_out) = spawn_capture(
            &tmp,
            &tmp,
            "env-forcing",
            &cmd,
            &env,
            30,
            // Deliberately UNsandboxed: this asserts the plain path's
            // inherit-and-force env contract, and routing it through the
            // AppContainer would ACL-stamp real directories as a side effect of
            // running the suite (the `run_command` precedent).
            &crate::sandbox::SandboxCfg::disabled(),
        )
        .await
        .expect("spawn");
        assert_eq!(code, Some(0));
        assert!(!timed_out);
        assert!(
            stdout.contains("cimp_env=sentinel42"),
            "env not forced onto child; stdout: {stdout:?}"
        );
    }

    /// A check run in a nested `cwd` must still report project-root-relative
    /// file paths — the diagnostic's `foo.rs` (relative to the nested run dir)
    /// comes back as `nested/foo.rs`.
    #[tokio::test]
    async fn nested_cwd_diagnostics_are_project_root_relative() {
        let root = std::env::temp_dir().join(format!("checks-nested-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let def = CheckDef {
            name: "nested".into(),
            cmd: "echo foo.rs:1:1: error: boom".into(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 30,
            cwd: Some("nested".into()),
            ..Default::default()
        };
        let report = run(&root, &def, false, &crate::sandbox::SandboxCfg::disabled()).await.expect("run");
        let sites: Vec<_> = report
            .groups
            .iter()
            .flat_map(|g| g.sites.iter().cloned())
            .collect();
        assert!(
            sites.iter().any(|(f, _)| f == "nested/foo.rs"),
            "diagnostic site should be re-rooted under the nested cwd: {sites:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_json_without_new_fields_roundtrips_to_defaults() {
        // A config written before V22 (no cwd/env/report_file) deserializes
        // with those fields defaulted, and is stable across a re-serialize /
        // re-deserialize roundtrip.
        let old =
            r#"{"name":"cargo","cmd":"cargo check","parser":"cargo-json","timeout_secs":120}"#;
        let def: CheckDef = serde_json::from_str(old).expect("old JSON deserializes");
        assert_eq!(def.cwd, None);
        assert!(def.env.is_empty());
        assert_eq!(def.report_file, None);
        assert_eq!(def.pattern, None);
        // V22 Phase D: the `auto` marker defaults to false, so every check
        // already on disk is treated as user-owned and protected from
        // re-detection.
        assert!(!def.auto);
        let reserialized = serde_json::to_value(&def).expect("CheckDef serializes");
        let again: CheckDef = serde_json::from_value(reserialized).expect("re-deserializes");
        assert_eq!(
            def, again,
            "CheckDef must survive a serialize/deserialize roundtrip unchanged"
        );
    }

    /// `report_file` set ⇒ the parser reads the FILE's content after the run,
    /// not the command's stdout.
    #[tokio::test]
    async fn report_file_content_is_used_as_parser_input() {
        let root = std::env::temp_dir().join(format!("checks-report-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("report.txt"), "bar.rs:2:3: warning: watch out\n").unwrap();
        let def = CheckDef {
            name: "rep".into(),
            // The command's own stdout must be ignored in favor of the file.
            cmd: "echo ignored.rs:9:9: error: from_stdout".into(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 30,
            report_file: Some("report.txt".into()),
            ..Default::default()
        };
        let report = run(&root, &def, false, &crate::sandbox::SandboxCfg::disabled()).await.expect("run");
        let msgs: Vec<_> = report.groups.iter().map(|g| g.message.clone()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("watch out")),
            "parser should read report_file: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| m.contains("from_stdout")),
            "stdout must not be parsed when report_file is set: {msgs:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── V22 Phase C — regex-custom validation + dotnet dedup ──────────────

    #[test]
    fn validate_regex_custom_requires_a_valid_pattern() {
        // regex-custom with no pattern is inert — rejected at validate time.
        let mut def = CheckDef {
            parser: ParserKind::RegexCustom,
            ..Default::default()
        };
        assert!(
            def.validate().is_err(),
            "regex-custom without a pattern must be rejected"
        );
        // A pattern missing the mandatory `line`/`message` groups is rejected.
        def.pattern = Some(r"(?<file>\S+)".into());
        assert!(
            def.validate().is_err(),
            "missing mandatory named groups must be rejected"
        );
        // An uncompilable regex is rejected.
        def.pattern = Some(r"(?<file>[".into());
        assert!(def.validate().is_err(), "a bad regex must be rejected");
        // All three mandatory groups present + compiles ⇒ ok.
        def.pattern = Some(r"^(?<file>\S+):(?<line>\d+): (?<message>.+)$".into());
        assert!(
            def.validate().is_ok(),
            "a valid pattern with all groups is accepted"
        );
        // The pattern is ignored (not required) for any other parser.
        let other = CheckDef {
            parser: ParserKind::GenericGcc,
            ..Default::default()
        };
        assert!(
            other.validate().is_ok(),
            "a non-regex-custom parser needs no pattern"
        );
    }

    #[test]
    fn regex_custom_pattern_field_roundtrips() {
        let def = CheckDef {
            name: "markdownlint".into(),
            cmd: "markdownlint .".into(),
            parser: ParserKind::RegexCustom,
            timeout_secs: 60,
            pattern: Some(r"^(?<file>\S+):(?<line>\d+) (?<message>.+)$".into()),
            ..Default::default()
        };
        let value = serde_json::to_value(&def).expect("CheckDef serializes");
        let back: CheckDef = serde_json::from_value(value).expect("re-deserializes");
        assert_eq!(
            def, back,
            "pattern must survive a serialize/deserialize roundtrip"
        );
        assert_eq!(
            back.pattern.as_deref(),
            Some(r"^(?<file>\S+):(?<line>\d+) (?<message>.+)$")
        );
    }

    #[test]
    fn dotnet_doubled_lines_dedup_into_one_group() {
        // MSBuild prints the same diagnostic once per target it built through —
        // identical lines must collapse in `group` (count reflects both).
        const OUT: &str = "Program.cs(10,13): error CS0103: boom [C:\\proj\\App.csproj]\nProgram.cs(10,13): error CS0103: boom [C:\\proj\\App.csproj]\n";
        let diags = parsers::parse(ParserKind::Dotnet, OUT, "", std::path::Path::new("."), None);
        assert_eq!(
            diags.len(),
            2,
            "parser itself emits one diag per line: {diags:?}"
        );
        let groups = group(diags);
        assert_eq!(
            groups.len(),
            1,
            "identical MSBuild lines dedup into one group: {groups:?}"
        );
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].severity, Severity::Error);
    }

    /// A configured-but-missing `report_file` is an explicit error diagnostic —
    /// never an empty, falsely-green run.
    #[tokio::test]
    async fn missing_report_file_yields_explicit_error_diag() {
        let root =
            std::env::temp_dir().join(format!("checks-report-missing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let def = CheckDef {
            name: "rep".into(),
            cmd: "echo hi".into(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 30,
            report_file: Some("does-not-exist.xml".into()),
            ..Default::default()
        };
        let report = run(&root, &def, false, &crate::sandbox::SandboxCfg::disabled()).await.expect("run");
        assert!(
            report
                .groups
                .iter()
                .any(|g| g.severity == Severity::Error && g.message.contains("report_file")),
            "a missing report_file must surface as an explicit error diagnostic: {:?}",
            report.groups
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `report_file` resolves against the check's *effective cwd* (cwd-relative),
    /// not the project root — so `detect.rs`'s unprefixed nested-module presets
    /// (`cwd: "backend"`, `report_file: "target/surefire-reports"`) read from the
    /// module dir the tool actually wrote to. The cwd-relative copy wins even
    /// when a decoy sits at the root-relative location.
    #[tokio::test]
    async fn report_file_resolves_relative_to_cwd() {
        let root = std::env::temp_dir().join(format!("checks-rf-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("backend").join("target")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        // The correct (cwd-relative) file, plus a root-relative decoy.
        std::fs::write(
            root.join("backend").join("target").join("r.xml"),
            "CWD_WINS",
        )
        .unwrap();
        std::fs::write(root.join("target").join("r.xml"), "ROOT_DECOY").unwrap();

        let cwd = root.join("backend");
        let got = read_report_file(&root, &cwd, "target/r.xml")
            .await
            .expect("read");
        assert_eq!(
            got, "CWD_WINS",
            "cwd-relative report_file must win over the root-relative decoy"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Back-compat: a pre-fix config wrote `report_file` root-relative *with* a
    /// `cwd` set. When the cwd-relative path doesn't exist we fall back to the
    /// root-relative one if THAT exists — so old configs keep working.
    #[tokio::test]
    async fn report_file_falls_back_to_root_relative_for_old_configs() {
        let root =
            std::env::temp_dir().join(format!("checks-rf-fallback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("backend")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        // Only the root-relative location exists (no backend/target/r.xml).
        std::fs::write(root.join("target").join("r.xml"), "ROOT_ONLY").unwrap();

        let cwd = root.join("backend");
        let got = read_report_file(&root, &cwd, "target/r.xml")
            .await
            .expect("read");
        assert_eq!(
            got, "ROOT_ONLY",
            "must fall back to root-relative when only that exists"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Confinement still holds: a `..`-escaping `report_file` is rejected before
    /// any read, whichever base it would resolve against.
    #[tokio::test]
    async fn report_file_escape_is_rejected() {
        let root = std::env::temp_dir().join(format!("checks-rf-escape-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("backend")).unwrap();
        let cwd = root.join("backend");
        let err = read_report_file(&root, &cwd, "../../secrets.xml")
            .await
            .unwrap_err();
        assert!(
            err.contains("escapes the project root") || err.contains("not allowed"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── V22 Phase E — Test-button dry run (test_check) ────────────────────

    /// `test_check` shapes a real run into the structured result the Settings
    /// editor renders: a command that prints one diagnostic comes back with a
    /// non-zero diag count, the first diagnostics echoed, and non-zero captured
    /// output size (the wrong-parser signal). Mirrors `run_executes_a_real_command`.
    #[tokio::test]
    async fn test_check_returns_structured_result() {
        let def = CheckDef {
            name: "t".into(),
            cmd: "echo foo.rs:3:4: error: boom".into(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 30,
            ..Default::default()
        };
        let result = test_check(
            &std::env::temp_dir(),
            &def,
            &crate::sandbox::SandboxCfg::disabled(),
        ).await;
        assert!(
            result.error.is_none(),
            "a valid check must not carry an error: {result:?}"
        );
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert_eq!(result.diag_count, 1, "one diagnostic parsed: {result:?}");
        assert!(
            result.stdout_bytes > 0,
            "the echo produced captured stdout: {result:?}"
        );
        let first = result.diagnostics.first().expect("one preview diagnostic");
        assert_eq!(first.severity, "error");
        assert!(first.message.contains("boom"), "{first:?}");
        assert_eq!(first.sites, vec!["foo.rs:3".to_string()]);
    }

    /// The zero-diagnostics-with-output case the UI flags as a wrong parser: a
    /// command that prints lines the chosen parser can't decode comes back with
    /// `diag_count == 0` but a non-zero `stdout_bytes`, so the classifier has the
    /// signal it needs.
    #[tokio::test]
    async fn test_check_reports_output_bytes_when_parser_matches_nothing() {
        let def = CheckDef {
            name: "t".into(),
            // Parseable by cargo-json only as JSON lines — plain text yields zero.
            cmd: "echo this is not json output at all".into(),
            parser: ParserKind::CargoJson,
            timeout_secs: 30,
            ..Default::default()
        };
        let result = test_check(
            &std::env::temp_dir(),
            &def,
            &crate::sandbox::SandboxCfg::disabled(),
        ).await;
        assert!(result.error.is_none(), "{result:?}");
        assert_eq!(
            result.diag_count, 0,
            "cargo-json parses plain text to zero diags: {result:?}"
        );
        assert!(
            result.stdout_bytes > 0,
            "output was produced (wrong-parser signal): {result:?}"
        );
    }

    /// A validation failure (escaping `cwd`) is captured into `error`, not
    /// propagated — the editor renders it inline like any other outcome.
    #[tokio::test]
    async fn test_check_captures_validation_error() {
        let def = CheckDef {
            name: "t".into(),
            cmd: "echo hi".into(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 30,
            cwd: Some("../escape".into()),
            ..Default::default()
        };
        let result = test_check(
            &std::env::temp_dir(),
            &def,
            &crate::sandbox::SandboxCfg::disabled(),
        ).await;
        assert!(
            result.error.is_some(),
            "an escaping cwd must surface as an error: {result:?}"
        );
        assert_eq!(result.diag_count, 0);
    }
}
