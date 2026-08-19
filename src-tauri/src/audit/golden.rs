//! V38 Phase E — the **pre-migration byte-match goldens** for the fourteen
//! built-in audit tools (design live-verify 7).
//!
//! Phase E moves those fourteen adapters onto the plugin framework: their argv
//! templates, transports, exit-code semantics, applicability gates and parsers
//! stop being a `static` table selected by a closed enum and become embedded
//! JSON manifests read through the same loader a user plugin goes through. That
//! is a *representation* change, and the only honest way to say so is to prove
//! it changed nothing a user or a model can see.
//!
//! So this module renders one deterministic text document — every tool's
//! rendered argv under three substitution shapes, its declared metadata, the
//! finalized outcome of six canned runs, and the two umbrella reports a model
//! actually reads — and compares it byte for byte against
//! `fixtures/builtin-report-goldens.txt`, which was captured **before** the
//! migration and is not touched by it.
//!
//! # What is deliberately NOT in the golden
//!
//! Nothing that depends on the machine: no resolved paths, no durations
//! (pinned to fixed values), no clock, no temp directory. A golden that
//! wobbled would be deleted within a week, and a deleted golden proves nothing.
//!
//! # Regenerating
//!
//! `CIMP_UPDATE_GOLDENS=1 cargo test -p cimp audit::golden` rewrites the
//! fixture. That is a **deliberate, visible act**: the diff it produces is the
//! behaviour change, and it belongs in a commit whose message argues for it.
//! Regenerating to make a red test green is how a regression ships.

use std::path::{Path, PathBuf};

use super::adapters::Category;
use super::runnable::RunnableAudit;
use super::runner::{AuditSnapshot, Outcome, ToolState, ToolStatus};

/// The captured document. Compared, never consulted at run time.
const GOLDEN: &str = include_str!("fixtures/builtin-report-goldens.txt");

/// The scan root every rendering uses — POSIX-shaped so the document reads the
/// same on both platforms (`Path::to_string_lossy` never rewrites separators).
fn root() -> PathBuf {
    PathBuf::from("/proj/root")
}

/// The temp report path substituted for `{report}` / `Arg::Report`.
fn report() -> PathBuf {
    PathBuf::from("/tmp/cimp-audit/report.sarif")
}

/// One built-in tool, fully materialized into the facts the golden states.
///
/// **This struct is the migration seam.** Everything below it is representation
/// -independent rendering; [`cases`] is the one function Phase E rewrites, from
/// "read the `static Adapter` table" to "read the embedded manifests through the
/// registry". If the document still matches afterwards, the two representations
/// agree.
pub(super) struct ToolCase {
    /// The wire id a report, a chip and a settings file all key off.
    pub wire: String,
    pub category: Category,
    pub transport: &'static str,
    pub findings_exit_codes: Vec<i32>,
    pub extensions: Vec<String>,
    pub markers: Vec<String>,
    /// `(label, rendered argv)` for the three substitution shapes.
    pub argvs: Vec<(&'static str, Vec<String>)>,
    /// `(label, finalized state)` for the six canned runs.
    pub outcomes: Vec<(&'static str, ToolState)>,
}

// ── canned tool output ──────────────────────────────────────────────────────
//
// Faithful to each tool's documented format, kept minimal: one finding apiece,
// with the shapes the decoders actually branch on (a relative SARIF uri, an
// absolute `file://` one, an absolute eslint `filePath`, a `null` typos
// correction, several knip buckets, a cargo-machete header + indent).

const OSV_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "osv-scanner" } },
    "results": [{
      "ruleId": "GHSA-r8w9-5wcg-vfj7",
      "level": "warning",
      "message": { "text": "tokio 1.38.0 is affected by GHSA-r8w9-5wcg-vfj7" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "Cargo.lock" },
          "region": { "startLine": 1 }
        }
      }]
    }]
  }]
}"#;

const GITLEAKS_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "gitleaks" } },
    "results": [{
      "ruleId": "generic-api-key",
      "level": "error",
      "message": { "text": "generic-api-key detected" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "file:///proj/root/src/lib/foo.ts" },
          "region": { "startLine": 42, "startColumn": 7 }
        }
      }]
    }]
  }]
}"#;

const SEMGREP_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "semgrep" } },
    "results": [{
      "ruleId": "javascript.lang.security.audit.detect-non-literal-fs-filename",
      "level": "error",
      "message": { "text": "Detected non-literal fs filename" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "src/SettingsApp.svelte" },
          "region": { "startLine": 1291, "startColumn": 3 }
        }
      }]
    }]
  }]
}"#;

const OXLINT_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "oxlint" } },
    "results": [{
      "ruleId": "no-debugger",
      "level": "error",
      "message": { "text": "`debugger` statement is not allowed" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "src/app.ts" },
          "region": { "startLine": 12, "startColumn": 3 }
        }
      }]
    }]
  }]
}"#;

const GOLANGCI_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "golangci-lint" } },
    "results": [{
      "ruleId": "errcheck",
      "level": "warning",
      "message": { "text": "Error return value is not checked" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "cmd/main.go" },
          "region": { "startLine": 88, "startColumn": 2 }
        }
      }]
    }]
  }]
}"#;

const RUFF_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "ruff" } },
    "results": [{
      "ruleId": "F401",
      "level": "warning",
      "message": { "text": "`os` imported but unused" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "file:///proj/root/app/main.py" },
          "region": { "startLine": 1, "startColumn": 8 }
        }
      }]
    }]
  }]
}"#;

const CPPCHECK_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "cppcheck" } },
    "results": [{
      "ruleId": "nullPointer",
      "level": "error",
      "message": { "text": "Null pointer dereference: p" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "src/parse.c" },
          "region": { "startLine": 210, "startColumn": 9 }
        }
      }]
    }]
  }]
}"#;

const PMD_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "PMD" } },
    "results": [{
      "ruleId": "UnusedPrivateField",
      "level": "note",
      "message": { "text": "Avoid unused private fields such as 'cache'" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "src/main/java/App.java" },
          "region": { "startLine": 17 }
        }
      }]
    }]
  }]
}"#;

const DOTNET_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "Microsoft.CodeAnalysis" } },
    "results": [{
      "ruleId": "CA1822",
      "level": "warning",
      "message": { "text": "Member 'Run' does not access instance data" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "Program.cs" },
          "region": { "startLine": 30, "startColumn": 21 }
        }
      }]
    }]
  }]
}"#;

const SEMGREP_QUALITY_SARIF: &str = r#"{
  "version": "2.1.0",
  "runs": [{
    "tool": { "driver": { "name": "semgrep" } },
    "results": [{
      "ruleId": "python.lang.best-practice.sleep.arbitrary-sleep",
      "level": "note",
      "message": { "text": "time.sleep() call; did you mean to wait on a condition?" },
      "locations": [{
        "physicalLocation": {
          "artifactLocation": { "uri": "worker/poll.py" },
          "region": { "startLine": 64, "startColumn": 5 }
        }
      }]
    }]
  }]
}"#;

const TYPOS_JSONL: &str = concat!(
    r#"{"type":"typo","path":"docs/README.md","line_num":3,"byte_offset":11,"typo":"recieve","corrections":["receive"]}"#,
    "\n",
    r#"{"type":"typo","path":"src/lib.rs","line_num":91,"byte_offset":4,"typo":"seperate","corrections":null}"#,
    "\n",
    r#"{"type":"binary_file","path":"assets/logo.png"}"#,
    "\n"
);

const ESLINT_JSON: &str = r#"[
  {
    "filePath": "/proj/root/src/app.ts",
    "messages": [
      { "ruleId": "no-unused-vars", "severity": 2, "message": "'x' is assigned a value but never used", "line": 3, "column": 7 },
      { "ruleId": "eqeqeq", "severity": 1, "message": "Expected '===' and instead saw '=='", "line": 10, "column": 5 }
    ]
  }
]"#;

const KNIP_JSON: &str = r#"{
  "issues": [
    {
      "file": "src/legacy.ts",
      "files": [{ "name": "src/legacy.ts" }],
      "exports": [{ "name": "oldHelper", "line": 4, "col": 14 }],
      "types": [],
      "dependencies": [],
      "devDependencies": [],
      "unlisted": [],
      "unresolved": []
    },
    {
      "file": "package.json",
      "files": [],
      "exports": [],
      "types": [],
      "dependencies": [{ "name": "left-pad", "line": 12 }],
      "devDependencies": [{ "name": "gulp", "line": 20 }],
      "unlisted": [{ "name": "chalk", "line": 0 }],
      "unresolved": []
    }
  ]
}"#;

const MACHETE_TEXT: &str = "cargo-machete found the following unused dependencies in /proj/root:\n\
                            \tonce_cell\n\
                            \tregex\n";

/// The canned output the "findings" case feeds a tool, by wire id.
fn canned_findings_output(wire: &str) -> &'static str {
    match wire {
        "osv-scanner" => OSV_SARIF,
        "gitleaks" => GITLEAKS_SARIF,
        "semgrep" => SEMGREP_SARIF,
        "oxlint" => OXLINT_SARIF,
        "golangci-lint" => GOLANGCI_SARIF,
        "ruff" => RUFF_SARIF,
        "cppcheck" => CPPCHECK_SARIF,
        "typos" => TYPOS_JSONL,
        "eslint" => ESLINT_JSON,
        "pmd" => PMD_SARIF,
        "dotnet-analyzers" => DOTNET_SARIF,
        "knip" => KNIP_JSON,
        "cargo-machete" => MACHETE_TEXT,
        "semgrep-quality" => SEMGREP_QUALITY_SARIF,
        other => panic!("no canned output for built-in tool `{other}`"),
    }
}

/// The six runs every tool is put through. `code` is resolved per tool (its
/// first findings exit code, else 0), so cppcheck — which exits 0 even with
/// findings — is exercised on its real contract rather than on a borrowed one.
pub(super) enum Case {
    /// The tool ran and produced its canned output.
    Findings,
    /// Exit 0, nothing written. gitleaks' clean run is exactly this, and it
    /// must stay a clean bill rather than becoming an ingest error.
    CleanSilent,
    /// A findings exit code with no output at all — the rc.9 "died before it
    /// started" message, and the one case the built-in tier must never present
    /// as clean.
    FindingsNoOutput,
    /// A genuine tool error, with diagnostics on stderr.
    ErrorExit,
    TimedOut,
    SpawnFailed,
}

impl Case {
    fn label(&self) -> &'static str {
        match self {
            Case::Findings => "findings",
            Case::CleanSilent => "clean, no output",
            Case::FindingsNoOutput => "findings exit, no output",
            Case::ErrorExit => "tool error exit 99",
            Case::TimedOut => "timed out",
            Case::SpawnFailed => "spawn failed",
        }
    }

    /// `(outcome, output, stdout, stderr)` for one tool.
    pub(super) fn inputs(&self, wire: &str, findings_exit: Option<i32>) -> (Outcome, String, String, String) {
        match self {
            Case::Findings => {
                let out = canned_findings_output(wire).to_string();
                (
                    Outcome::Exited(Some(findings_exit.unwrap_or(0))),
                    out.clone(),
                    out,
                    String::new(),
                )
            }
            Case::CleanSilent => (
                Outcome::Exited(Some(0)),
                String::new(),
                String::new(),
                String::new(),
            ),
            Case::FindingsNoOutput => (
                Outcome::Exited(Some(findings_exit.unwrap_or(0))),
                String::new(),
                String::new(),
                String::new(),
            ),
            Case::ErrorExit => (
                Outcome::Exited(Some(99)),
                String::new(),
                String::new(),
                "fatal: could not read the ruleset\n".to_string(),
            ),
            Case::TimedOut => (
                Outcome::TimedOut,
                String::new(),
                String::new(),
                String::new(),
            ),
            Case::SpawnFailed => (
                Outcome::SpawnError(
                    "The system cannot find the file specified. (os error 2)".to_string(),
                ),
                String::new(),
                String::new(),
                String::new(),
            ),
        }
    }
}

/// Every case, in document order.
pub(super) const CASES: &[Case] = &[
    Case::Findings,
    Case::CleanSilent,
    Case::FindingsNoOutput,
    Case::ErrorExit,
    Case::TimedOut,
    Case::SpawnFailed,
];

/// The wall clock the timeout message quotes. Fixed so the document does not
/// depend on `CodeAuditSettings::default().timeout_secs` staying 600.
pub(super) const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// The duration every rendered `done` line reports, so the document carries no
/// clock. Chosen to exercise `fmt_duration`'s sub-minute branch.
const FIXED_MS: u64 = 1234;

// ── rendering ───────────────────────────────────────────────────────────────

fn render_argv(argv: &[String]) -> String {
    if argv.is_empty() {
        "(no fixed arguments)".to_string()
    } else {
        argv.join(" \u{2502} ")
    }
}

fn render_list(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}

fn render_state(state: &ToolState) -> String {
    let mut s = String::new();
    s.push_str(&format!("      status:   {}\n", status_word(state.status)));
    match &state.error {
        Some(e) => s.push_str(&format!("      error:    {e}\n")),
        None => s.push_str("      error:    (none)\n"),
    }
    if state.findings.is_empty() {
        s.push_str("      findings: (none)\n");
    } else {
        s.push_str(&format!("      findings: {}\n", state.findings.len()));
        for f in &state.findings {
            let d = &f.diag;
            s.push_str(&format!(
                "        {} {}:{}{} [{}] {}\n",
                d.severity.as_str().to_ascii_uppercase(),
                d.file,
                d.line,
                d.col.map(|c| format!(":{c}")).unwrap_or_default(),
                d.code.as_deref().unwrap_or(""),
                d.message.trim()
            ));
        }
    }
    s
}

fn status_word(s: ToolStatus) -> &'static str {
    match s {
        ToolStatus::Idle => "idle",
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Failed => "failed",
        ToolStatus::NotInstalled => "not-installed",
        ToolStatus::PathInvalid => "path-invalid",
        ToolStatus::SkippedNotApplicable => "skipped-not-applicable",
    }
}

/// A snapshot for [`super::mcp::format_result`] — no clock, fixed root.
fn snapshot(tools: Vec<ToolState>) -> AuditSnapshot {
    let total_findings = tools.iter().map(|t| t.findings.len()).sum();
    AuditSnapshot {
        root: root().to_string_lossy().into_owned(),
        scanning: false,
        last_scan_at: Some(1),
        tools,
        census: Default::default(),
        total_findings,
        truncated: false,
    }
}

/// The whole document.
pub(super) fn render(cases: &[ToolCase]) -> String {
    let mut out = String::new();
    out.push_str(
        "# cImp built-in audit tools — pre-migration golden (V38 design live-verify 7)\n\
         #\n\
         # Generated by `audit::golden`. Every line below is behaviour a user or a model\n\
         # can observe: the argv a scan spawns, the metadata that gates it, the verdict a\n\
         # canned run produces, and the umbrella report a model reads. Phase E changes how\n\
         # these fourteen tools are DEFINED; this document is what says it changed nothing\n\
         # they DO.\n\
         #\n\
         # root   = /proj/root\n\
         # report = /tmp/cimp-audit/report.sarif\n\
         # every `done` line reports a fixed 1234 ms so the document carries no clock.\n\n",
    );

    out.push_str("== per-tool definitions and runs ==\n");
    for c in cases {
        out.push_str(&format!("\n--- {} ---\n", c.wire));
        out.push_str(&format!(
            "  category:            {}\n",
            match c.category {
                Category::Security => "security",
                Category::Quality => "quality",
            }
        ));
        out.push_str(&format!("  transport:           {}\n", c.transport));
        out.push_str(&format!(
            "  findings exit codes: {}\n",
            if c.findings_exit_codes.is_empty() {
                "(none — exit 0 is the only success)".to_string()
            } else {
                c.findings_exit_codes
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        out.push_str(&format!(
            "  applicability:       extensions [{}] markers [{}]\n",
            render_list(&c.extensions),
            render_list(&c.markers)
        ));
        for (label, argv) in &c.argvs {
            out.push_str(&format!("  argv {label}\n    {}\n", render_argv(argv)));
        }
        out.push_str("  runs:\n");
        for (label, state) in &c.outcomes {
            out.push_str(&format!("    case `{label}`:\n"));
            out.push_str(&render_state(state));
        }
    }

    out.push_str("\n== umbrella reports (what a model reads) ==\n");
    for category in [Category::Security, Category::Quality] {
        let tools: Vec<ToolState> = cases
            .iter()
            .filter(|c| c.category == category)
            .map(|c| {
                let mut st = c
                    .outcomes
                    .iter()
                    .find(|(l, _)| *l == Case::Findings.label())
                    .expect("every tool has a findings case")
                    .1
                    .clone();
                st.duration_ms = FIXED_MS;
                st
            })
            .collect();
        out.push_str(&format!(
            "\n--- {} audit, every tool reporting findings ---\n",
            match category {
                Category::Security => "security",
                Category::Quality => "quality",
            }
        ));
        out.push_str(&super::mcp::format_result(&snapshot(tools), category));
    }

    // One mixed report, so every `status_line` arm the built-in tier can reach
    // is pinned rather than only the happy one.
    out.push_str("\n--- security audit, mixed statuses ---\n");
    let mixed: Vec<ToolState> = cases
        .iter()
        .filter(|c| c.category == Category::Security)
        .enumerate()
        .map(|(i, c)| {
            // The key comes from a rendered state rather than being rebuilt
            // here: this module must not know how a built-in key is SPELLED,
            // only that the tool it belongs to produced it.
            let mut st = ToolState {
                id: c.outcomes[0].1.id.clone(),
                category: Category::Security,
                status: ToolStatus::Idle,
                findings: Vec::new(),
                duration_ms: 0,
                error: None,
                resolved: None,
                scanned_artifacts: Vec::new(),
            };
            match i {
                0 => {
                    st.status = ToolStatus::NotInstalled;
                    st.error = Some(
                        "not found on PATH or ebin — install it or set its path in Settings"
                            .to_string(),
                    );
                }
                1 => {
                    st.status = ToolStatus::PathInvalid;
                    st.error =
                        Some("configured path not found: D:\\gone\\gitleaks.exe — fix it in Settings".to_string());
                }
                _ => st.status = ToolStatus::SkippedNotApplicable,
            }
            st
        })
        .collect();
    out.push_str(&super::mcp::format_result(
        &snapshot(mixed),
        Category::Security,
    ));

    out
}

/// Compare `rendered` against the committed document, newline-agnostically.
///
/// The tree is CRLF on Windows and CI checks out CRLF too, so a `\r` must never
/// be part of the comparison — a golden that fails on line endings teaches
/// people to regenerate it, which is how a golden stops meaning anything.
pub(super) fn assert_matches(rendered: &str) {
    let norm = |s: &str| s.replace("\r\n", "\n");
    let want = norm(GOLDEN);
    let got = norm(rendered);
    if want == got {
        return;
    }
    if std::env::var("CIMP_UPDATE_GOLDENS").is_ok() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/audit/fixtures/builtin-report-goldens.txt");
        std::fs::write(&path, got.as_bytes()).expect("write golden");
        panic!(
            "goldens REGENERATED at {} — this is a behaviour change; read the diff and argue \
             for it in the commit message, or fix the code instead",
            path.display()
        );
    }
    // Name the first differing line: a whole-document diff in a panic message
    // is unreadable, and "not equal" is useless.
    let (wl, gl): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let at = (0..wl.len().max(gl.len()))
        .find(|i| wl.get(*i) != gl.get(*i))
        .unwrap_or(0);
    panic!(
        "the built-in audit tools no longer behave as captured before the V38 plugin \
         migration.\n  first difference at line {}:\n    golden: {:?}\n    now:    {:?}\n  \
         ({} golden lines, {} rendered lines)\n  If the change is intended, regenerate with \
         CIMP_UPDATE_GOLDENS=1 and say why in the commit.",
        at + 1,
        wl.get(at),
        gl.get(at),
        wl.len(),
        gl.len()
    );
}

// ── the migration seam: how the fourteen tools are read ─────────────────────

/// Materialize every built-in tool.
///
/// **Rewritten by Phase E, and that is the whole point of this module.** It used
/// to read the `static Adapter` table through the closed `AuditToolId` enum; it
/// now reads the embedded manifests through the same registry a dropped-in
/// plugin goes through — an untouched settings container, so every value is the
/// manifest's own default. Everything above this function is
/// representation-independent, so the document still matching afterwards is the
/// proof that the two representations agree.
fn cases() -> Vec<ToolCase> {
    let set = crate::plugins::builtin::plugin_set();
    let cfg = crate::settings::ToolPluginsSettings::default();
    let root = root();
    let report = report();

    crate::plugins::registry::effective_tools(&set, &cfg, None)
        .iter()
        .filter_map(|t| {
            let tool = RunnableAudit::from_effective(t)
                .unwrap_or_else(|e| panic!("built-in tool `{}` is not runnable: {e}", t.tool_id))?;
            let wire = tool.key.wire();
            let findings_exit = tool.findings_exit_codes.first().copied();

            // The third argv shape needs a ruleset override and appended
            // parameters. Inserting `ruleset` unconditionally mirrors the
            // pre-migration behaviour exactly: the old `full_argv` took a
            // ruleset string that only tools carrying an `Arg::Ruleset` token
            // did anything with, and a `{var:ruleset}` token is the same gate.
            let mut tuned = tool.clone();
            tuned
                .variables
                .insert("ruleset".to_string(), "RULESET/OVERRIDE".to_string());
            tuned.parameters = vec!["--exclude".to_string(), "vendor".to_string()];

            let argvs = vec![
                (
                    "(git repo, report path, no overrides)",
                    tool.full_argv(&root, Some(&report), true),
                ),
                (
                    "(not a git repo, no report path)",
                    tool.full_argv(&root, None, false),
                ),
                (
                    "(git repo, report path, ruleset + extra args)",
                    tuned.full_argv(&root, Some(&report), true),
                ),
            ];

            let outcomes = CASES
                .iter()
                .map(|case| {
                    let (outcome, output, stdout, stderr) = case.inputs(&wire, findings_exit);
                    let (status, findings, error) = super::runner::finalize(
                        &super::runner::Finalize {
                            key: tool.key.clone(),
                            findings_exit_codes: &tool.findings_exit_codes,
                            parser: tool.parser,
                            gate: tool.gate,
                        },
                        outcome,
                        &output,
                        false,
                        &stdout,
                        &stderr,
                        &root,
                        TIMEOUT,
                    );
                    (
                        case.label(),
                        ToolState {
                            id: tool.key.clone(),
                            category: tool.category,
                            status,
                            findings,
                            duration_ms: FIXED_MS,
                            error,
                            resolved: None,
                            scanned_artifacts: Vec::new(),
                        },
                    )
                })
                .collect();

            Some(ToolCase {
                wire,
                category: tool.category,
                transport: match tool.transport {
                    super::adapters::Transport::Stdout => "stdout",
                    super::adapters::Transport::ReportFile => "report_file",
                },
                findings_exit_codes: tool.findings_exit_codes.clone(),
                extensions: tool.applicability.extensions.clone(),
                markers: tool.applicability.markers.clone(),
                argvs,
                outcomes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The byte-match regression.** See the module docs for what it protects
    /// and why regenerating it is a commit, not a fix.
    #[test]
    fn the_builtin_audit_tools_behave_exactly_as_captured() {
        assert_matches(&render(&cases()));
    }

    /// The document is worth nothing if it is empty or lost a tool. Counted
    /// against the roster the milestone names (fourteen adapters), not against
    /// `cases().len()`, which would agree with itself.
    #[test]
    fn the_golden_covers_all_fourteen_builtin_tools() {
        let cases = cases();
        assert_eq!(cases.len(), 14, "the built-in roster is fourteen tools");
        for c in &cases {
            assert!(
                GOLDEN.contains(&format!("--- {} ---", c.wire)),
                "the golden document has no section for `{}`",
                c.wire
            );
            assert_eq!(c.outcomes.len(), CASES.len());
        }
    }
}
