//! `audit::runner`'s unit tests — the module's own `#[cfg(test)] mod tests`,
//! moved to a sibling file (#132, test-placement wave). Bodies unchanged;
//! only the `include_str!` paths moved with the file, one directory deeper.

use super::*;
use crate::checks::Severity;

/// Every built-in tool, resolved through the embedded manifest against an
/// UNTOUCHED settings container — so each one is exactly what a fresh
/// install gets.
///
/// This is the fixture that replaced `adapters::adapter(AuditToolId::…)`.
/// The difference is the point of Phase E: the facts these tests assert on
/// (argv, exit codes, parser, applicability) used to be a `static` table and
/// are now read from `plugins/builtin/cimp-audit.json` through the same
/// registry a dropped-in plugin goes through.
fn builtin_tools() -> Vec<EffectiveTool> {
    crate::plugins::registry::effective_tools(
        &crate::plugins::builtin::plugin_set(),
        &crate::settings::ToolPluginsSettings::default(),
        None,
    )
}

/// One built-in tool by its wire id, resolved and ready to run.
fn builtin(id: &str) -> RunnableAudit {
    let tools = builtin_tools();
    let tool = tools
        .iter()
        .find(|t| t.tool_id == id)
        .unwrap_or_else(|| panic!("no built-in tool `{id}`"));
    RunnableAudit::from_effective(tool)
        .unwrap_or_else(|e| panic!("`{id}` is not runnable: {e}"))
        .unwrap_or_else(|| panic!("`{id}` is not an umbrella tool"))
}

/// The built-in roster with `enabled` overridden per tool — the fixture the
/// planning tests use in place of the old `Vec<AuditToolConfig>`.
fn builtin_tools_with(enables: &[(&str, bool)]) -> Vec<EffectiveTool> {
    let mut cfg = crate::settings::ToolPluginsSettings::default();
    let plugin = cfg
        .plugins
        .entry(crate::plugins::builtin::AUDIT_PLUGIN_KEY.to_string())
        .or_default();
    for (id, on) in enables {
        plugin.tools.entry((*id).to_string()).or_default().enabled = *on;
    }
    crate::plugins::registry::effective_tools(
        &crate::plugins::builtin::plugin_set(),
        &cfg,
        None,
    )
}

/// [`finalize`] for one built-in tool, reading its exit codes, parser and
/// ingest gate from its manifest.
#[allow(clippy::too_many_arguments)]
fn finalize_builtin(
    id: &str,
    outcome: Outcome,
    sarif: &str,
    sarif_truncated: bool,
    stdout: &str,
    stderr: &str,
    root: &Path,
    timeout: Duration,
) -> (ToolStatus, Vec<AuditFinding>, Option<String>) {
    let tool = builtin(id);
    finalize(
        &Finalize {
            key: tool.key.clone(),
            findings_exit_codes: &tool.findings_exit_codes,
            parser: tool.parser,
            gate: tool.gate,
        },
        outcome,
        sarif,
        sarif_truncated,
        stdout,
        stderr,
        root,
        timeout,
    )
}

/// The security trio's parser — every fixture below is SARIF, and naming it
/// once keeps the `parse_findings` call sites about the fixture.
fn sarif_parser() -> AuditParser {
    builtin("osv-scanner").parser
}

// ── SARIF fixtures ─────────────────────────────────────────────────────
//
// NOTE: osv-scanner / gitleaks / semgrep are not installed in this
// environment (checked at implementation time), so these are faithful
// fixtures constructed from each tool's documented SARIF 2.1.0 output —
// LIVE CAPTURE IS PENDING (the V23 live-verify recipe replaces them with
// real captures once the binaries are dropped in `ebin/`). Each pins the
// fields the findings table consumes: rule id → `Diag.code`, SARIF level →
// `Diag.severity`, and a project-relative path.

/// osv-scanner `scan source --format sarif`: a `Cargo.lock` vuln, rule id =
/// the OSV/GHSA id, level `warning` (osv-scanner's default result level).
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

/// gitleaks `--report-format sarif`: a secret hit, rule id = the gitleaks
/// rule, level `error`, absolute `file://` URI that must relativize to the
/// scan root.
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

/// semgrep `--sarif`: a SAST hit, rule id = the semgrep rule, level `error`.
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

fn root() -> PathBuf {
    PathBuf::from("/proj/root")
}

#[test]
fn osv_sarif_fixture_maps_to_findings() {
    let key = ToolKey::Builtin("osv-scanner".to_string());
    let f = parse_findings(&key, sarif_parser(), OSV_SARIF, &root());
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].tool, key);
    assert_eq!(f[0].diag.code.as_deref(), Some("GHSA-r8w9-5wcg-vfj7"));
    assert_eq!(f[0].diag.severity, Severity::Warning);
    assert_eq!(f[0].diag.file, "Cargo.lock");
    assert_eq!(f[0].diag.line, 1);
}

#[test]
fn gitleaks_sarif_fixture_relativizes_path() {
    let f = parse_findings(
        &ToolKey::Builtin("gitleaks".to_string()),
        sarif_parser(),
        GITLEAKS_SARIF,
        &root(),
    );
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].diag.code.as_deref(), Some("generic-api-key"));
    assert_eq!(f[0].diag.severity, Severity::Error);
    // The absolute file:// URI normalized project-relative against the root.
    assert_eq!(f[0].diag.file, "src/lib/foo.ts");
    assert_eq!(f[0].diag.line, 42);
    assert_eq!(f[0].diag.col, Some(7));
}

#[test]
fn semgrep_sarif_fixture_maps_to_findings() {
    let f = parse_findings(
        &ToolKey::Builtin("semgrep".to_string()),
        sarif_parser(),
        SEMGREP_SARIF,
        &root(),
    );
    assert_eq!(f.len(), 1);
    assert_eq!(
        f[0].diag.code.as_deref(),
        Some("javascript.lang.security.audit.detect-non-literal-fs-filename")
    );
    assert_eq!(f[0].diag.severity, Severity::Error);
    assert_eq!(f[0].diag.file, "src/SettingsApp.svelte");
    assert_eq!(f[0].diag.line, 1291);
}

// ── scan-coverage artifacts (osv-scanner) ───────────────────────────────

/// osv-scanner SARIF carrying `runs[].artifacts`: a relative lockfile, an
/// absolute `file://` manifest (must relativize to the root), and a
/// duplicate (must dedupe).
const OSV_ARTIFACTS_SARIF: &str = r#"{
      "version": "2.1.0",
      "runs": [{
        "tool": { "driver": { "name": "osv-scanner" } },
        "artifacts": [
          { "location": { "uri": "Cargo.lock" } },
          { "location": { "uri": "file:///proj/root/package-lock.json" } },
          { "location": { "uri": "Cargo.lock" } }
        ],
        "results": []
      }]
    }"#;

// (These exercise the shared `parsers::sarif_scanned_artifacts` against the
// audit fixtures — the coverage-line contract belongs to this runner even
// though the extraction now lives with the SARIF parser. The
// sibling-prefix `relativize` guard and `read_capped` truncation tests live
// with their helpers: `checks::parsers` / `procutil`.)

#[test]
fn osv_artifacts_extract_relative_deduped() {
    let a = parsers::sarif_scanned_artifacts(OSV_ARTIFACTS_SARIF, &root());
    assert_eq!(
        a,
        vec!["Cargo.lock".to_string(), "package-lock.json".to_string()]
    );
}

#[test]
fn absent_or_malformed_artifacts_yield_empty() {
    // No `artifacts` key at all (the findings-only fixtures / older tools).
    assert!(parsers::sarif_scanned_artifacts(OSV_SARIF, &root()).is_empty());
    // Malformed SARIF is best-effort empty, never an error.
    assert!(parsers::sarif_scanned_artifacts("not json", &root()).is_empty());
    // An empty-uri artifact is skipped.
    let empty_uri = r#"{"runs":[{"artifacts":[{"location":{"uri":""}}]}]}"#;
    assert!(parsers::sarif_scanned_artifacts(empty_uri, &root()).is_empty());
}

// ── finalize_outcome: the findings-vs-error exit semantics ──────────────

#[test]
fn findings_exit_is_done_with_findings() {
    let (status, findings, error) = finalize_builtin(
        "osv-scanner",
        Outcome::Exited(Some(1)), // findings-present code
        OSV_SARIF,
        false,
        "",
        "",
        &root(),
        Duration::from_secs(600),
    );
    assert_eq!(status, ToolStatus::Done);
    assert_eq!(findings.len(), 1);
    assert!(error.is_none());
}

#[test]
fn clean_exit_is_done_no_findings() {
    let (status, findings, error) = finalize_builtin(
        "gitleaks",
        Outcome::Exited(Some(0)),
        "", // clean run wrote no report
        false,
        "",
        "",
        &root(),
        Duration::from_secs(600),
    );
    assert_eq!(status, ToolStatus::Done);
    assert!(findings.is_empty());
    assert!(error.is_none());
}

/// A findings exit code whose SARIF turned out empty/unparseable (missing
/// temp report, mid-JSON truncation upstream) must be a loud failure, never
/// a clean "0 findings" pass — and the message must say WHICH of the three
/// it was, because they send the reader to three different places.
#[test]
fn findings_exit_with_empty_sarif_is_failed() {
    for sarif in ["", "not json at all"] {
        let (status, findings, error) = finalize_builtin(
            "gitleaks",
            Outcome::Exited(Some(1)), // "leaks found"
            sarif,
            false,
            "",
            "report write failed: permission denied",
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Failed, "sarif = {sarif:?}");
        assert!(findings.is_empty());
        let msg = error.unwrap();
        // The diagnostic tail rides along in every branch — it is the only
        // thing in the message that came from the tool itself.
        assert!(msg.contains("permission denied"), "{msg}");
        if sarif.is_empty() {
            // The tool talked (stderr) but wrote no report.
            assert!(msg.contains("wrote no report at all"), "{msg}");
            assert!(!msg.contains("NO output at all"), "{msg}");
        } else {
            assert!(msg.contains("unreadable — findings were lost"), "{msg}");
        }
    }
}

/// **The rc.9 `audit:semgrep` misread.** A sandboxed `semgrep.exe` that was
/// granted its `Scripts` directory but not the Python install root behind it
/// exited **1 with no report, no stdout and no stderr** — it never started
/// its interpreter. Exit 1 is semgrep's findings code, so the runner said
/// "the SARIF report was empty or unreadable — findings were lost", which
/// describes a parser problem the user then went looking for.
///
/// Nothing was lost: nothing was ever produced. A tool that emitted NOTHING
/// on any channel did not run, and its exit code is not evidence of
/// findings — the message has to say that, and name the shape (a runtime or
/// interpreter the sandbox does not grant) that actually causes it.
#[test]
fn a_findings_exit_with_no_output_at_all_is_not_a_lost_report() {
    let (status, findings, error) = finalize_builtin(
        "semgrep",
        Outcome::Exited(Some(1)), // semgrep's findings code
        "",                       // no SARIF
        false,                    // not truncated — there was nothing to truncate
        "",                       // no stdout
        "",                       // no stderr either
        &root(),
        Duration::from_secs(600),
    );
    assert_eq!(status, ToolStatus::Failed);
    assert!(findings.is_empty());
    let msg = error.expect("a silent findings exit must explain itself");
    assert!(msg.contains("NO output at all"), "{msg}");
    assert!(
        msg.contains("not evidence of findings"),
        "the exit code must be disowned, not repeated as fact: {msg}"
    );
    assert!(
        msg.contains("interpreter") && msg.contains("sandbox"),
        "the message must name the shape that produces it: {msg}"
    );
    // …and it must NOT claim a report went missing.
    assert!(!msg.contains("findings were lost"), "{msg}");
}

/// A capped (known-incomplete) stdout SARIF is discarded as a failure even
/// on a "clean" exit — a truncated document must not read as a clean bill.
#[test]
fn truncated_sarif_is_failed_not_clean() {
    for code in [0, 1] {
        let (status, findings, error) = finalize_builtin(
            "osv-scanner",
            Outcome::Exited(Some(code)),
            OSV_SARIF, // even a parseable prefix is untrustworthy
            true,      // stdout blew the capture cap
            "",
            "",
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Failed, "exit {code}");
        assert!(findings.is_empty());
        assert!(error.unwrap().contains("incomplete"), "exit {code}");
    }
}

#[test]
fn tool_error_exit_is_failed_with_message() {
    let (status, findings, error) = finalize_builtin(
        "semgrep",
        Outcome::Exited(Some(2)), // neither 0 nor a findings code
        "",
        false,
        "",
        "network unreachable while downloading rules",
        &root(),
        Duration::from_secs(600),
    );
    assert_eq!(status, ToolStatus::Failed);
    assert!(findings.is_empty());
    let msg = error.unwrap();
    assert!(msg.contains("code 2"), "{msg}");
    assert!(msg.contains("network unreachable"), "{msg}");
}

#[test]
fn timeout_and_cancel_and_spawn_error_map_to_failed() {
    let f = |o: Outcome| {
        finalize_builtin(
            "semgrep",
            o,
            "",
            false,
            "",
            "",
            &root(),
            Duration::from_secs(5),
        )
    };
    let (s, _, e) = f(Outcome::TimedOut);
    assert_eq!(s, ToolStatus::Failed);
    assert!(e.unwrap().contains("timed out after 5s"));

    // V38: a stop the USER asked for is its own status. A timeout stays
    // `Failed` right above — the tool did not finish inside a budget it was
    // given, which is a tool outcome, not a user action.
    let (s, _, e) = f(Outcome::Cancelled);
    assert_eq!(s, ToolStatus::Cancelled);
    assert_eq!(e.as_deref(), Some("scan cancelled"));

    let (s, _, e) = f(Outcome::SpawnError("boom".into()));
    assert_eq!(s, ToolStatus::Failed);
    assert!(e.unwrap().contains("boom"));
}

/// V38 — the `audit` lane row names the outcome, so three facts that were
/// one row (`ok=false · 0 findings`) can be told apart after the fact.
///
/// Live symptom: a user cancelled a scan mid-run and the row for the tool
/// that was in flight was byte-identical to the row a crashed scanner
/// writes. `ok` is unchanged — only success is `true` — and a successful
/// row keeps its pre-V38 wording exactly.
#[test]
fn the_activity_row_word_separates_cancel_timeout_and_failure() {
    assert_eq!(audit_row_outcome(ToolStatus::Done, false), None);
    assert_eq!(
        audit_row_outcome(ToolStatus::Cancelled, false),
        Some("cancelled")
    );
    // The one case the status cannot answer alone: both are `Failed`.
    assert_eq!(
        audit_row_outcome(ToolStatus::Failed, true),
        Some("timed out")
    );
    assert_eq!(audit_row_outcome(ToolStatus::Failed, false), Some("failed"));
    // A provider refused by the user's own server toggle.
    assert_eq!(audit_row_outcome(ToolStatus::Idle, false), Some("disabled"));
    // `timed_out` never rewrites a status that already speaks for itself.
    assert_eq!(audit_row_outcome(ToolStatus::Done, true), None);
    assert_eq!(
        audit_row_outcome(ToolStatus::Cancelled, true),
        Some("cancelled")
    );
}

// ── V25 finalize: clean-exit-with-findings semantics ────────────────────

/// cppcheck ALWAYS exits 0 (no `--error-exitcode`); its findings live only
/// in the report. A clean (exit-0) run with a populated report must be
/// `done`-WITH-findings — the V25 correction to V23's "clean = empty".
#[test]
fn cppcheck_clean_exit_with_report_yields_findings() {
    const CPPCHECK_SARIF: &str = r#"{
          "version": "2.1.0",
          "runs": [{
            "tool": { "driver": { "name": "cppcheck" } },
            "results": [{
              "ruleId": "nullPointer",
              "level": "error",
              "message": { "text": "Null pointer dereference" },
              "locations": [{
                "physicalLocation": {
                  "artifactLocation": { "uri": "src/main.c" },
                  "region": { "startLine": 10 }
                }
              }]
            }]
          }]
        }"#;
    let (status, findings, error) = finalize_builtin(
        "cppcheck",
        Outcome::Exited(Some(0)), // cppcheck's normal "findings present" path
        CPPCHECK_SARIF,
        false,
        "",
        "",
        &root(),
        Duration::from_secs(600),
    );
    assert_eq!(status, ToolStatus::Done);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].diag.code.as_deref(), Some("nullPointer"));
    assert_eq!(findings[0].diag.file, "src/main.c");
    assert!(error.is_none());
}

/// eslint exits 0 when it has warnings-only, yet its JSON still carries them.
/// The [`AuditParser::EslintJson`](super::runnable::AuditParser) decoder runs
/// on the clean-exit output, so those warnings surface as `done`-with-
/// findings rather than a false clean bill.
#[test]
fn eslint_clean_exit_with_warnings_yields_findings() {
    const ESLINT_JSON: &str = r#"[
          { "filePath": "/proj/root/src/app.ts",
            "messages": [
              { "ruleId": "eqeqeq", "severity": 1, "message": "use ===", "line": 4, "column": 3 }
            ]
          }
        ]"#;
    let (status, findings, error) = finalize_builtin(
        "eslint",
        Outcome::Exited(Some(0)), // warnings-only ⇒ exit 0
        ESLINT_JSON,
        false,
        ESLINT_JSON, // eslint's output is on stdout
        "",
        &root(),
        Duration::from_secs(600),
    );
    assert_eq!(status, ToolStatus::Done);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].diag.severity, Severity::Warning);
    assert_eq!(findings[0].diag.code.as_deref(), Some("eqeqeq"));
    assert_eq!(findings[0].diag.file, "src/app.ts");
    assert!(error.is_none());
}

/// A genuinely clean run (empty/absent output) stays `done`-no-findings for
/// every parser — the "report lost" guard is for a *findings exit code* whose
/// output didn't parse, never for a clean exit whose empty output is a real
/// clean bill.
#[test]
fn clean_exit_with_empty_output_is_done_no_findings() {
    for id in ["cppcheck", "eslint", "oxlint"] {
        let (status, findings, error) = finalize_builtin(
            id,
            Outcome::Exited(Some(0)),
            "",
            false,
            "",
            "",
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Done, "{id}");
        assert!(findings.is_empty(), "{id}");
        assert!(error.is_none(), "{id}");
    }
}

// ── begin_scan / run_scan_and_wait guard coverage ──────────────────────
//
// `begin_scan` (shared by `start_scan` and the V26 `run_scan_and_wait` MCP
// surface) has three reject paths — master switch off, "no <category> audit
// tools are enabled", and "a scan is already in progress". Its ONLY pure,
// AppHandle-free logic is the census + `plan_scan` planning, which the
// `plan_scan_*` and `auto_select_quality_*` tests below already pin
// directly. The three rejects themselves live behind `&Arc<AuditState>`,
// which needs a Tauri `AppHandle`: this crate builds `tauri` WITHOUT the
// `test` feature and has no `tauri::test` mock anywhere, so no `AuditState`
// is constructible in a unit test. Rather than bolt on a mock runtime (or
// extract the two one-line guards purely just to satisfy a test), the guard
// behavior is verified live per the V26 MCP verification recipe (busy →
// "scan already in progress" tool error; disabled master switch → refused).
// `run_scan_and_wait` adds no new guard logic of its own — it shares
// `begin_scan` verbatim with the already-exercised `start_scan` and only
// awaits `run` inline instead of spawning it.

// ── V25 plan_scan: category + applicability + disabled filter ────────────

/// A Quality scan never launches a Security tool, shows a disabled tool as
/// `idle`, and reports an enabled-but-inapplicable tool `skipped-not-
/// applicable`; only enabled + applicable tools land in `to_run`.
#[test]
fn plan_scan_quality_filters_category_disabled_and_applicability() {
    // A Rust + JS project: no `.py`, no `.go`.
    let census = census::Census::from_parts(&["ts", "rs"], &["Cargo.toml", "package.json"]);
    let tools = builtin_tools_with(&[("cargo-machete", false)]);
    let (chips, to_run) = plan_scan(&tools, Category::Quality, &census);

    // No security tool leaks into a quality scan.
    assert!(chips.iter().all(|c| c.category == Category::Quality));
    assert!(!chips.iter().any(|c| c.id.is_builtin("osv-scanner")));

    let status = |id: &str| {
        chips
            .iter()
            .find(|c| c.id.is_builtin(id))
            .unwrap_or_else(|| panic!("no chip for `{id}`"))
            .status
    };
    assert_eq!(status("oxlint"), ToolStatus::Running, "applicable (.ts)");
    assert_eq!(
        status("ruff"),
        ToolStatus::SkippedNotApplicable,
        "no .py in this project"
    );
    assert_eq!(status("cargo-machete"), ToolStatus::Idle, "disabled");
    assert_eq!(status("typos"), ToolStatus::Running, "always applicable");
    // A DISABLED built-in is still a chip — the pre-V38 contract the panel
    // greys out and the report counts as `disabled`.
    assert_eq!(
        chips.len(),
        11,
        "every quality built-in gets a chip, enabled or not"
    );

    // `to_run` is exactly the enabled + applicable set, in manifest order.
    let run: Vec<String> = to_run.iter().map(|t| t.key.wire()).collect();
    assert_eq!(run, vec!["oxlint", "typos", "knip"]);
}

/// A Security scan excludes every Quality tool; the always-applicable trio
/// runs even against an empty census.
#[test]
fn plan_scan_security_excludes_quality_tools() {
    let census = census::Census::default();
    let (chips, to_run) = plan_scan(&builtin_tools(), Category::Security, &census);
    assert!(chips.iter().all(|c| c.category == Category::Security));
    assert!(!chips.iter().any(|c| c.id.is_builtin("oxlint")));
    let run: Vec<String> = to_run.iter().map(|t| t.key.wire()).collect();
    assert_eq!(run, vec!["osv-scanner", "gitleaks", "semgrep"]);
}

/// Quality auto-selection: each built-in quality tool's `enabled` becomes
/// its manifest default AND census-applicable; the tools whose manifests say
/// `enabled_by_default: false` stay opt-in even when applicable; security
/// tools and user plugins are never touched; a second pass is a no-op.
///
/// Driven from two extreme starting states rather than one, because the
/// function returns only what CHANGES: from all-off, every "must be
/// selected" claim is a real change to assert, and from all-on every "must
/// be deselected" one is. A single starting state would leave half the rules
/// asserted as `None`, which is also what a broken implementation returns.
#[test]
fn auto_select_quality_follows_census_and_keeps_heavyweights_opt_in() {
    // A Rust + TS project: no `.py` / `.go` / `.java` / C / eslint config.
    let census = census::Census::from_parts(&["ts", "rs"], &["Cargo.toml", "package.json"]);
    const QUALITY: &[&str] = &[
        "oxlint",
        "golangci-lint",
        "ruff",
        "cppcheck",
        "typos",
        "eslint",
        "pmd",
        "knip",
        "cargo-machete",
        "dotnet-analyzers",
        "semgrep-quality",
    ];
    let all = |on: bool| -> Vec<(&'static str, bool)> {
        QUALITY.iter().map(|id| (*id, on)).collect()
    };
    let decide = |state: &[(&'static str, bool)]| {
        let changes = auto_select_quality(&builtin_tools_with(state), &census);
        move |id: &str| changes.iter().find(|(k, _)| k == id).map(|(_, v)| *v)
    };

    // From ALL OFF: exactly the default-on, applicable tools turn on.
    let off = all(false);
    let from_off = decide(&off);
    for id in ["oxlint", "typos", "knip", "cargo-machete"] {
        assert_eq!(from_off(id), Some(true), "{id} applies to this project");
    }
    for id in ["ruff", "golangci-lint", "cppcheck", "pmd", "eslint"] {
        assert_eq!(from_off(id), None, "{id} does not apply and stays off");
    }
    // The two the MANIFEST marks opt-in stay off even though
    // `semgrep-quality` is ungated (always applicable) — this is the rule
    // that stops a first quality audit running a real .NET build or
    // fetching a ruleset over the network.
    for id in ["dotnet-analyzers", "semgrep-quality"] {
        assert_eq!(from_off(id), None, "{id} is opt-in by its manifest");
    }

    // From ALL ON: exactly the inapplicable and the opt-in tools turn off.
    let on = all(true);
    let from_on = decide(&on);
    for id in ["ruff", "golangci-lint", "cppcheck", "pmd", "eslint"] {
        assert_eq!(from_on(id), Some(false), "{id} does not apply here");
    }
    for id in ["dotnet-analyzers", "semgrep-quality"] {
        assert_eq!(from_on(id), Some(false), "{id} is opt-in by its manifest");
    }
    for id in ["oxlint", "typos", "knip", "cargo-machete"] {
        assert_eq!(from_on(id), None, "{id} is already at its automatic value");
    }
    // Security tools are out of scope entirely, from either direction — a
    // security audit must never become census-dependent.
    for id in ["osv-scanner", "gitleaks", "semgrep"] {
        assert_eq!(from_off(id), None, "{id} is a security tool");
        assert_eq!(from_on(id), None, "{id} is a security tool");
    }

    // Idempotent: applying what it asked for leaves nothing to decide.
    let mut applied = on.clone();
    for (id, v) in auto_select_quality(&builtin_tools_with(&on), &census) {
        let id = leaked(&id);
        match applied.iter_mut().find(|(k, _)| *k == id) {
            Some(slot) => slot.1 = v,
            None => applied.push((id, v)),
        }
    }
    assert!(auto_select_quality(&builtin_tools_with(&applied), &census).is_empty());
}

/// `builtin_tools_with` takes `&'static str` keys; these ids come from a
/// manifest read at run time. Leaking a handful of short strings in a test
/// process is the honest trade against threading a lifetime through a
/// fixture helper.
fn leaked(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// V25 Phase C: a per-tool `timeout_secs` override wins over the global; a
/// `None` override falls back to the global; a 0 override clamps to 1s.
#[test]
fn effective_tool_timeout_prefers_override_else_global() {
    let global = Duration::from_secs(600);
    assert_eq!(effective_tool_timeout(None, global), global);
    assert_eq!(
        effective_tool_timeout(Some(1200), global),
        Duration::from_secs(1200)
    );
    // A 0 override is clamped to ≥ 1s (never an instant timeout).
    assert_eq!(
        effective_tool_timeout(Some(0), global),
        Duration::from_secs(1)
    );
}

// ── snapshot wire cap ───────────────────────────────────────────────────

#[test]
fn event_snapshot_caps_findings_and_flags_truncated() {
    let mut ts = ToolState::fresh(ToolKey::Builtin("gitleaks".to_string()), Category::Security);
    ts.status = ToolStatus::Done;
    let one = || AuditFinding {
        tool: ToolKey::Builtin("gitleaks".to_string()),
        diag: Diag {
            severity: Severity::Error,
            code: Some("generic-api-key".into()),
            message: "secret".into(),
            file: "a.ts".into(),
            line: 1,
            col: None,
        },
    };
    ts.findings = (0..EVENT_FINDINGS_PER_TOOL_CAP + 10)
        .map(|_| one())
        .collect();
    let inner = Inner {
        root: root(),
        scanning: false,
        last_scan_at: Some(123),
        tools: vec![ts],
        census: CensusBlock::default(),
        cancel: None,
    };
    // Full snapshot (IPC): everything, never truncated.
    let full = inner.snapshot(None);
    assert_eq!(full.total_findings, EVENT_FINDINGS_PER_TOOL_CAP + 10);
    assert!(!full.truncated);
    assert_eq!(
        full.tools[0].findings.len(),
        EVENT_FINDINGS_PER_TOOL_CAP + 10
    );
    // Event snapshot: capped, truncated flag set, total still true.
    let evt = inner.snapshot(Some(EVENT_FINDINGS_PER_TOOL_CAP));
    assert!(evt.truncated);
    assert_eq!(evt.tools[0].findings.len(), EVENT_FINDINGS_PER_TOOL_CAP);
    assert_eq!(evt.total_findings, EVENT_FINDINGS_PER_TOOL_CAP + 10);
}

/// V25 Phase C: the snapshot serializes the census block (both cap modes),
/// so the split UI can gate chips off a single IPC/event payload.
#[test]
fn snapshot_carries_census_block() {
    let inner = Inner {
        root: root(),
        scanning: false,
        last_scan_at: None,
        tools: vec![ToolState::fresh(ToolKey::Builtin("oxlint".to_string()), Category::Quality)],
        census: CensusBlock {
            extensions: vec!["rs".into(), "ts".into()],
            markers: vec!["Cargo.toml".into()],
        },
        cancel: None,
    };
    for cap in [None, Some(EVENT_FINDINGS_PER_TOOL_CAP)] {
        let snap = inner.snapshot(cap);
        assert_eq!(
            snap.census.extensions,
            vec!["rs".to_string(), "ts".to_string()]
        );
        assert_eq!(snap.census.markers, vec!["Cargo.toml".to_string()]);
        assert_eq!(snap.tools[0].category, Category::Quality);
    }
}

// ── Rust↔TS wire tripwire (runtime types) ──────────────────────────────

/// The runtime wire shapes (`AuditSnapshot`/`ToolState`/`AuditFinding`/
/// `AuditDiag` — including `checks::Diag`, which crosses the wire verbatim
/// inside `AuditFinding`) must stay mirrored in codeAudit/types.ts. The
/// settings-side audit types have their own tripwire in `settings::schema`;
/// without this one, renaming a `Diag`/`ToolState` field keeps cargo green
/// while the Code Audit table silently reads `undefined`.
const AUDIT_RUNTIME_TS: &str = include_str!("../../../../src/lib/codeAudit/types.ts");

#[test]
fn runtime_wire_shapes_mirrored_in_code_audit_types_ts() {
    // A fully-populated snapshot — every Option is Some, so every wire
    // field key appears in the serialized JSON and gets checked.
    let snap = AuditSnapshot {
        root: "/proj/root".into(),
        scanning: true,
        last_scan_at: Some(123),
        tools: vec![ToolState {
            id: ToolKey::Builtin("gitleaks".to_string()),
            category: Category::Security,
            status: ToolStatus::Done,
            findings: vec![AuditFinding {
                tool: ToolKey::Builtin("gitleaks".to_string()),
                diag: Diag {
                    severity: Severity::Error,
                    code: Some("generic-api-key".into()),
                    message: "secret".into(),
                    file: "a.ts".into(),
                    line: 1,
                    col: Some(2),
                },
            }],
            duration_ms: 5,
            error: Some("boom".into()),
            resolved: Some(PathBuf::from("C:/ebin/gitleaks.exe")),
            scanned_artifacts: vec!["Cargo.lock".into()],
        }],
        census: CensusBlock {
            extensions: vec!["rs".into(), "ts".into()],
            markers: vec!["Cargo.toml".into()],
        },
        total_findings: 1,
        truncated: false,
    };
    fn assert_keys(v: &serde_json::Value, ts: &str) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m {
                    assert!(
                        ts.contains(&format!("{k}:")),
                        "wire field `{k}` is missing from src/lib/codeAudit/types.ts — \
                             update the TS mirror together with the Rust type",
                    );
                    assert_keys(val, ts);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| assert_keys(x, ts)),
            _ => {}
        }
    }
    assert_keys(
        &serde_json::to_value(&snap).expect("snapshot serializes"),
        AUDIT_RUNTIME_TS,
    );
}

#[test]
fn status_and_severity_wire_strings_mirrored_in_code_audit_types_ts() {
    // Exhaustive matches are the Rust-side half of the tripwire: a new
    // variant that isn't added to these lists is a compile error.
    let statuses = [
        ToolStatus::Idle,
        ToolStatus::Running,
        ToolStatus::Done,
        ToolStatus::Failed,
        ToolStatus::Cancelled,
        ToolStatus::NotInstalled,
        ToolStatus::PathInvalid,
        ToolStatus::SkippedNotApplicable,
    ];
    fn _statuses_exhaustive(s: ToolStatus) {
        match s {
            ToolStatus::Idle
            | ToolStatus::Running
            | ToolStatus::Done
            | ToolStatus::Failed
            | ToolStatus::Cancelled
            | ToolStatus::NotInstalled
            | ToolStatus::PathInvalid
            | ToolStatus::SkippedNotApplicable => {}
        }
    }
    for s in statuses {
        let wire = serde_json::to_value(s)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            AUDIT_RUNTIME_TS.contains(&format!("'{wire}'")),
            "ToolStatus wire `{wire}` is missing from the TS `AuditToolStatus` union",
        );
    }

    // V25 Phase C: the two Category wire strings must be in the TS
    // `AuditCategory` union (a scan is dispatched by, and every ToolState
    // tagged with, this value).
    let categories = [Category::Security, Category::Quality];
    fn _categories_exhaustive(c: Category) {
        match c {
            Category::Security | Category::Quality => {}
        }
    }
    for c in categories {
        let wire = serde_json::to_value(c)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            AUDIT_RUNTIME_TS.contains(&format!("'{wire}'")),
            "Category wire `{wire}` is missing from the TS `AuditCategory` union",
        );
    }

    let severities = [Severity::Error, Severity::Warning, Severity::Note];
    fn _severities_exhaustive(s: Severity) {
        match s {
            Severity::Error | Severity::Warning | Severity::Note => {}
        }
    }
    for sev in severities {
        let wire = serde_json::to_value(sev)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            AUDIT_RUNTIME_TS.contains(&format!("'{wire}'")),
            "Severity wire `{wire}` is missing from the TS `AuditSeverity` union",
        );
    }
}


// ── V38 Phase C: the plugin fan-out ────────────────────────────────────

/// A plugin set built through the REAL loader, so every fixture below is a
/// manifest that actually validates. Four tools: a security scanner, a
/// quality one gated on Java, a `command` kind (Phase D's population) and
/// one whose id collides with a built-in's name.
fn plugin_fixture() -> (crate::plugins::PluginSet, PathBuf) {
    let dir = std::env::temp_dir().join(format!("cimp-fanout-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(
        dir.join("acme.json"),
        r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "categories": [
                { "id": "sec", "label": "Security", "tools": ["scan", "gitleaks"] },
                { "id": "q", "label": "Quality", "tools": ["lint", "fmt"] }
              ],
              "tools": [
                { "id": "scan", "label": "Acme Scan", "kind": "security", "argv": ["{root}"] },
                { "id": "gitleaks", "label": "gitleaks", "kind": "security", "argv": ["{root}"] },
                { "id": "lint", "label": "Acme Lint", "kind": "audit", "argv": ["{root}"],
                  "applicability": { "extensions": ["java"], "markers": [] } },
                { "id": "fmt", "label": "Acme Format", "kind": "command" }
              ]
            }"#,
    )
    .expect("write manifest");
    let set = crate::plugins::loader::scan_dir(&dir, crate::plugins::manifest::Provenance::User);
    assert!(set.errors.is_empty(), "{:?}", set.errors);
    (set, dir)
}

/// The fixture plugin's tools, resolved with a path so they are runnable —
/// **without** the built-in roster, for the tests that are about the plugin
/// population alone.
fn effective(set: &crate::plugins::PluginSet) -> Vec<crate::plugins::registry::EffectiveTool> {
    let mut cfg = crate::settings::ToolPluginsSettings::default();
    for id in ["scan", "gitleaks", "lint", "fmt"] {
        cfg.global_paths.insert(
            format!("acme@1.0.0/{id}"),
            "C:\\bin\\acme.exe".to_string(),
        );
    }
    crate::plugins::registry::effective_tools(set, &cfg, None)
}

/// The whole population the runner sees: cImp's own embedded definitions
/// FIRST, then the fixture plugin's — the order `loader::scan_all`
/// establishes and that the security floor rests on.
fn effective_with_builtins(
    set: &crate::plugins::PluginSet,
) -> Vec<crate::plugins::registry::EffectiveTool> {
    let mut all = builtin_tools();
    all.extend(effective(set));
    all
}

/// **The chip residual Phase C left open, closed.** The PRE-SCAN roster
/// carries the built-ins AND this project's runnable plugin tools, so a
/// plugin tool the user enabled and pointed at a binary is visible before
/// anything has run — not only once a scan starts.
#[test]
fn the_pre_scan_roster_shows_plugin_tools_beside_the_builtins() {
    let (set, dir) = plugin_fixture();
    let tools = effective_with_builtins(&set);

    let roster = plan_roster(&tools, Category::Security, &census::Census::default(), false);
    let ids: Vec<String> = roster.iter().map(|t| t.id.wire()).collect();
    // The built-in security trio, then the plugin tools of that category.
    assert!(ids.contains(&"gitleaks".to_string()));
    assert!(ids.contains(&"acme@1.0.0/scan".to_string()));
    assert!(ids.contains(&"acme@1.0.0/gitleaks".to_string()));
    assert!(
        roster.iter().all(|t| t.status == ToolStatus::Idle),
        "a pre-scan roster promises nothing about outcomes"
    );
    assert!(
        roster.iter().all(|t| t.category == Category::Security),
        "one panel, one category"
    );
    // A `command`-kind tool is never an umbrella chip (Phase D's other half).
    assert!(!ids.iter().any(|id| id.ends_with("/fmt")));

    // A plugin tool's applicability lives in a manifest the frontend cannot
    // read, so the BACKEND decides it — but only against a census that was
    // actually taken. Unknown census ⇒ shown; known-and-not-matching ⇒
    // already marked, so `partitionChips` hides it with no new rule.
    let quality = |census: &census::Census, known| {
        plan_roster(&tools, Category::Quality, census, known)
            .into_iter()
            .find(|t| t.id.wire() == "acme@1.0.0/lint")
            .expect("the java-gated tool is in the quality roster")
            .status
    };
    assert_eq!(
        quality(&census::Census::default(), false),
        ToolStatus::Idle,
        "an unwalked project is unknown, not empty"
    );
    assert_eq!(
        quality(&census::Census::from_parts(&["rs"], &[]), true),
        ToolStatus::SkippedNotApplicable
    );
    assert_eq!(
        quality(&census::Census::from_parts(&["java"], &[]), true),
        ToolStatus::Idle
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The built-in half of the roster is the pre-V38 contract, unchanged: every
/// configured entry of the category, DISABLED ones included (the panel greys
/// them), and untouched by whatever the plugin population does.
#[test]
fn the_roster_still_lists_disabled_builtins() {
    let all_off: Vec<(&str, bool)> = ["osv-scanner", "gitleaks", "semgrep"]
        .iter()
        .map(|id| (*id, false))
        .collect();
    let roster = plan_roster(
        &builtin_tools_with(&all_off),
        Category::Security,
        &census::Census::default(),
        true,
    );
    let ids: Vec<String> = roster.iter().map(|t| t.id.wire()).collect();
    for id in ["osv-scanner", "gitleaks", "semgrep"] {
        assert!(ids.contains(&id.to_string()), "{id} must still be listed");
    }
}

/// The fan-out rule: this category's kind, gated by the SAME census test a
/// built-in gets, with `check`/`command` kinds left to Phase D.
#[test]
fn plan_scan_filters_a_plugin_by_kind_category_and_applicability() {
    let (set, dir) = plugin_fixture();
    let tools = effective(&set);

    let (chips, to_run) = plan_scan(&tools, Category::Security, &census::Census::default());
    let ids: Vec<String> = to_run.iter().map(|t| t.key.wire()).collect();
    assert_eq!(ids, vec!["acme@1.0.0/scan", "acme@1.0.0/gitleaks"]);
    assert!(
        chips.iter().all(|c| c.category == Category::Security),
        "a security fan-out must not carry another category's chips"
    );

    // Quality, empty census: the Java-gated tool is planned as a chip and
    // NOT launched — the built-in `skipped-not-applicable` state, reached by
    // the built-in rule.
    let (chips, to_run) = plan_scan(&tools, Category::Quality, &census::Census::default());
    assert!(to_run.is_empty(), "the java gate held");
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0].status, ToolStatus::SkippedNotApplicable);
    assert_eq!(chips[0].id.wire(), "acme@1.0.0/lint");

    // …and it runs once the project actually contains Java.
    let java = census::Census::from_parts(&["java"], &[]);
    let (_, to_run) = plan_scan(&tools, Category::Quality, &java);
    assert_eq!(to_run.len(), 1);

    // The `command`-kind tool never appears in either umbrella.
    for category in [Category::Security, Category::Quality] {
        let (chips, _) = plan_scan(&tools, category, &java);
        assert!(
            !chips.iter().any(|c| c.id.wire().ends_with("/fmt")),
            "a command-kind tool is Phase D's population, not an umbrella's"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The security floor (invariant 2), generalized.** The built-in trio is
/// part of a Security fan-out whatever the plugin population does, and the
/// two rosters are computed independently — the built-in one FIRST, from
/// settings alone. Keyed off `Provenance`, never off a name (R3).
#[test]
fn plugins_add_to_the_security_fanout_and_can_never_displace_a_builtin() {
    let (set, dir) = plugin_fixture();
    let census = census::Census::default();
    let all = effective_with_builtins(&set);

    let (chips, runs) = plan_scan(&all, Category::Security, &census);

    // The trio is in the fan-out, running, and it is FIRST — cImp's own
    // definitions are laid down before anything scanned from a folder.
    let run_ids: Vec<String> = runs.iter().map(|t| t.key.wire()).collect();
    assert_eq!(
        &run_ids[..3],
        &["osv-scanner", "gitleaks", "semgrep"],
        "the built-in security trio leads every security fan-out"
    );
    // A plugin can only ADD: every entry after the trio is user provenance,
    // and no plugin tool can present itself as a built-in. Keyed off
    // `Provenance` and the key namespaces, never off a name (R3).
    for t in &runs[3..] {
        assert!(
            matches!(t.key, ToolKey::Plugin(_)),
            "a scanned plugin must never land in the built-in namespace"
        );
        assert_eq!(t.provenance, crate::plugins::manifest::Provenance::User);
    }
    assert_eq!(runs.len(), 5, "three built-ins + the fixture's two");

    // Each built-in appears exactly once in the chip roster.
    for id in ["osv-scanner", "gitleaks", "semgrep"] {
        assert_eq!(
            chips.iter().filter(|c| c.id.is_builtin(id)).count(),
            1,
            "{id} appears exactly once"
        );
    }

    // …and disabling every built-in still cannot remove them from the
    // ROSTER (they become `idle` chips), which is what keeps a user who
    // switched one off able to see that they did.
    let mut cfg = crate::settings::ToolPluginsSettings::default();
    let plugin = cfg
        .plugins
        .entry(crate::plugins::builtin::AUDIT_PLUGIN_KEY.to_string())
        .or_default();
    for id in ["osv-scanner", "gitleaks", "semgrep"] {
        plugin.tools.entry(id.to_string()).or_default().enabled = false;
    }
    let off = crate::plugins::registry::effective_tools(
        &crate::plugins::builtin::plugin_set(),
        &cfg,
        None,
    );
    let (chips, runs) = plan_scan(&off, Category::Security, &census);
    assert!(runs.is_empty());
    assert_eq!(chips.len(), 3);
    assert!(chips.iter().all(|c| c.status == ToolStatus::Idle));
    let _ = std::fs::remove_dir_all(&dir);
}

/// **No shadowing at the fan-out.** A plugin tool whose id and label spell a
/// built-in's name gets its own key and runs BESIDE it; nothing about the
/// built-in changes, and the two are distinguishable in every consumer
/// because attribution is the key.
#[test]
fn a_plugin_named_like_a_builtin_runs_beside_it_not_instead_of_it() {
    let (set, dir) = plugin_fixture();
    let tools = effective(&set);
    let census = census::Census::default();

    let (_, plugin_runs) = plan_scan(&tools, Category::Security, &census);
    let shadow = plugin_runs
        .iter()
        .find(|t| t.label == "gitleaks")
        .expect("the shadowing fixture");
    assert_eq!(shadow.key.wire(), "acme@1.0.0/gitleaks");
    assert!(!shadow.key.is_builtin("gitleaks"), "not the built-in");

    // Both populations produce findings, and each finding carries its own
    // key — a report reader can always tell which tool said what.
    let mine = parse_findings(&shadow.key, AuditParser::Sarif, GITLEAKS_SARIF, &root());
    let theirs = parse_findings(
        &ToolKey::Builtin("gitleaks".to_string()),
        sarif_parser(),
        GITLEAKS_SARIF,
        &root(),
    );
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].tool.wire(), "acme@1.0.0/gitleaks");
    assert_eq!(theirs[0].tool.wire(), "gitleaks");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A manifest that cannot produce a runnable tool becomes a FAILED chip
/// carrying the reason — never a silent omission from the roster.
#[test]
fn a_plugin_tool_that_cannot_run_is_a_failed_chip_not_a_silent_drop() {
    let dir = std::env::temp_dir().join(format!("cimp-badparser-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    // `parser` is validated against the findings namespace for USER plugins,
    // so this manifest is built as a builtin-provenance one — the only way
    // to reach the refusal, and exactly the shape Phase E will introduce.
    std::fs::write(
        dir.join("bad.json"),
        r#"{
              "manifest_version": 1,
              "name": "cimp-bad",
              "version": "1.0.0",
              "categories": [{ "id": "sec", "label": "Security", "tools": ["scan"] }],
              "tools": [{ "id": "scan", "label": "Bad", "kind": "security",
                          "argv": ["{root}"], "parser": "cargo-json" }]
            }"#,
    )
    .expect("write manifest");
    let set =
        crate::plugins::loader::scan_dir(&dir, crate::plugins::manifest::Provenance::Builtin);
    assert!(set.errors.is_empty(), "{:?}", set.errors);
    let mut cfg = crate::settings::ToolPluginsSettings::default();
    cfg.global_paths.insert(
        "cimp-bad@1.0.0/scan".to_string(),
        "C:\\bin\\bad.exe".to_string(),
    );
    let tools = crate::plugins::registry::runnable_tools(&set, &cfg, None);

    let (chips, to_run) =
        plan_scan(&tools, Category::Security, &census::Census::default());
    assert!(to_run.is_empty());
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0].status, ToolStatus::Failed);
    let err = chips[0].error.as_deref().unwrap_or_default();
    assert!(err.contains("cargo-json"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── V38 Phase C: ingest semantics ──────────────────────────────────────

/// A plugin fixture's finalize context — SARIF, exit 1 means findings, and
/// the envelope gate ON (which is what every user plugin gets).
fn plugin_spec(key: &ToolKey) -> Finalize<'_> {
    Finalize {
        key: key.clone(),
        findings_exit_codes: &[1],
        parser: AuditParser::Sarif,
        gate: crate::audit::runnable::IngestGate::Sarif,
    }
}

/// **Empty is not absent.** The whole substantiveness matrix on the shared
/// finalize path: `runs: []` on a clean exit is the ONLY empty-looking
/// output that reads as a clean scan.
#[test]
fn a_plugin_tools_blank_output_is_an_error_not_a_clean_scan() {
    let key = ToolKey::Plugin("acme@1.0.0/scan".to_string());
    let spec = plugin_spec(&key);
    let fin = |sarif: &str, code: i32| {
        finalize(
            &spec,
            Outcome::Exited(Some(code)),
            sarif,
            false,
            "",
            "",
            &root(),
            Duration::from_secs(60),
        )
    };

    // A SARIF log with no results: ran, found nothing, clean.
    let (status, findings, error) = fin(r#"{"version":"2.1.0","runs":[]}"#, 0);
    assert_eq!(status, ToolStatus::Done);
    assert!(findings.is_empty() && error.is_none());

    // Nothing at all, on a CLEAN exit: a tool that said nothing is not a
    // tool that found nothing.
    let (status, _, error) = fin("", 0);
    assert_eq!(status, ToolStatus::Failed);
    assert!(error.unwrap().contains("no output at all"));

    // Parseable but not SARIF: zero findings out of a document cImp never
    // understood must not read as a clean bill.
    let (status, _, error) = fin("{}", 0);
    assert_eq!(status, ToolStatus::Failed);
    assert!(error.unwrap().contains("not a SARIF log"));

    // Not JSON at all (a usage message on stdout).
    let (status, _, error) = fin("usage: acme [options]", 0);
    assert_eq!(status, ToolStatus::Failed);
    assert!(error.unwrap().contains("not JSON"));

    // A real log with a result: findings, on either exit class.
    for code in [0, 1] {
        let (status, findings, error) = fin(GITLEAKS_SARIF, code);
        assert_eq!(status, ToolStatus::Done, "exit {code}");
        assert_eq!(findings.len(), 1);
        assert!(error.is_none());
    }

    // A findings exit code with an empty log keeps the built-in rule too —
    // the envelope fires first and says the more precise thing.
    let (status, _, error) = fin("", 1);
    assert_eq!(status, ToolStatus::Failed);
    assert!(error.unwrap().contains("no output at all"));
}

/// **Attribution is the registry entry that ran.** A hostile SARIF naming a
/// built-in scanner as its own driver still files its findings under the
/// plugin key — the tool name inside output is a claim by the thing being
/// audited.
#[test]
fn findings_are_attributed_to_the_tool_that_ran_not_to_the_name_in_the_output() {
    let key = ToolKey::Plugin("acme@1.0.0/scan".to_string());
    let (status, findings, _) = finalize(
        &plugin_spec(&key),
        Outcome::Exited(Some(0)),
        // The driver claims to be gitleaks.
        GITLEAKS_SARIF,
        false,
        "",
        "",
        &root(),
        Duration::from_secs(60),
    );
    assert_eq!(status, ToolStatus::Done);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].tool, key);
    assert!(
        !findings[0].tool.is_builtin("gitleaks"),
        "the embedded driver name must never become attribution"
    );
}

/// The built-in population's semantics are UNCHANGED by the envelope gate
/// (R4): gitleaks writes no report at all on a clean run, and that is still
/// a clean, zero-finding pass.
#[test]
fn the_envelope_gate_does_not_apply_to_the_builtin_tier() {
    let (status, findings, error) = finalize_builtin(
        "gitleaks",
        Outcome::Exited(Some(0)),
        "",
        false,
        "",
        "",
        &root(),
        Duration::from_secs(60),
    );
    assert_eq!(status, ToolStatus::Done);
    assert!(findings.is_empty() && error.is_none());
}

// ── V38 Phase C: the declared sandbox posture ──────────────────────────

/// `SpawnPosture::default()` is the BUILT-IN tier's posture, and its
/// `sandbox_req` must be `optional`.
///
/// A derived `Default` would inherit `SandboxReq`'s own default — `required`,
/// which is the right answer for a manifest and a catastrophic one here: it
/// would refuse to run all fourteen built-in scanners on any machine with
/// the sandbox switched off. This caught exactly that during Phase C.
#[test]
fn the_builtin_spawn_posture_declares_nothing_and_therefore_degrades() {
    let p = SpawnPosture::default();
    assert_eq!(p.sandbox_req, SandboxReq::Optional);
    assert_eq!(p.runtime, crate::sandbox::RuntimeSelect::Infer);
    assert!(p.rows.is_empty() && p.full_dirs.is_empty());
}

/// **`required` refuses even when the sandbox is globally OFF.** The
/// manifest says this tool must never run unprotected, and a global
/// preference does not overrule it — the tool is missing from the scan,
/// with a reason, instead of running unconfined.
#[tokio::test]
async fn a_required_sandbox_refuses_to_run_when_sandboxing_is_off() {
    let (prog, argv) = sleeper();
    let cancel = CancellationToken::new();
    let started = Instant::now();
    let cap = spawn_and_capture(
        &prog,
        &argv,
        &[],
        "test-required",
        &SpawnPosture {
            sandbox_req: SandboxReq::Required,
            ..SpawnPosture::default()
        },
        &RunCtx {
            root: std::env::temp_dir(),
            timeout: Duration::from_secs(30),
            cancel: cancel.clone(),
            sandbox: crate::sandbox::SandboxCfg::disabled(),
        },
    )
    .await;
    // Nothing was spawned at all — the 30s sleeper never ran.
    assert!(started.elapsed() < Duration::from_secs(5));
    match cap.outcome {
        Outcome::SpawnError(e) => {
            assert!(e.contains("sandbox: required"), "{e}");
            assert!(e.contains("switched off"), "{e}");
        }
        _ => panic!("a `required` tool must not run unsandboxed"),
    }
}

/// `unsupported` is the opposite decision and must still RUN — outside the
/// boundary, on purpose, with a row. Proven by the child actually starting
/// (it is killed by the timeout it was given).
#[tokio::test]
async fn an_unsupported_sandbox_declaration_still_runs_the_tool() {
    let (prog, argv) = sleeper();
    let cancel = CancellationToken::new();
    let cap = spawn_and_capture(
        &prog,
        &argv,
        &[],
        "test-unsupported",
        &SpawnPosture {
            sandbox_req: SandboxReq::Unsupported,
            ..SpawnPosture::default()
        },
        &RunCtx {
            root: std::env::temp_dir(),
            timeout: Duration::from_millis(300),
            cancel: cancel.clone(),
            sandbox: crate::sandbox::SandboxCfg::disabled(),
        },
    )
    .await;
    assert!(
        matches!(cap.outcome, Outcome::TimedOut),
        "an `unsupported` tool runs; it is not refused"
    );
}

// A portable long-running child for timeout/cancel tests.
fn sleeper() -> (PathBuf, Vec<String>) {
    #[cfg(windows)]
    {
        // `ping -n 30 127.0.0.1` blocks ~30s without extra tooling.
        let p = which::which("ping").expect("ping on PATH");
        (p, vec!["-n".into(), "30".into(), "127.0.0.1".into()])
    }
    #[cfg(not(windows))]
    {
        let p = which::which("sleep").expect("sleep on PATH");
        (p, vec!["30".into()])
    }
}

#[tokio::test]
async fn timeout_kills_child_and_reports_timed_out() {
    let (prog, argv) = sleeper();
    let cancel = CancellationToken::new();
    let started = Instant::now();
    let cap = spawn_and_capture(
        &prog,
        &argv,
        &[],
        "test-sleeper",
        &SpawnPosture::default(),
        &RunCtx {
            root: std::env::temp_dir(),
            timeout: Duration::from_millis(300),
            cancel: cancel.clone(),
            // Deliberately UNsandboxed: this asserts the timeout/kill
            // contract, and routing it through the AppContainer would
            // ACL-stamp the developer's real toolchain dirs as a side effect
            // of running the suite (the `run_command` precedent).
            sandbox: crate::sandbox::SandboxCfg::disabled(),
        },
    )
    .await;
    // Returns promptly (child killed), not after the ~30s sleep.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "child was not killed on timeout"
    );
    assert!(
        matches!(cap.outcome, Outcome::TimedOut),
        "expected TimedOut"
    );
}

#[tokio::test]
async fn cancel_kills_child() {
    let (prog, argv) = sleeper();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    // Cancel shortly after the child starts.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let started = Instant::now();
    let cap = spawn_and_capture(
        &prog,
        &argv,
        &[],
        "test-sleeper",
        &SpawnPosture::default(),
        &RunCtx {
            root: std::env::temp_dir(),
            timeout: Duration::from_secs(60),
            cancel: cancel.clone(),
            sandbox: crate::sandbox::SandboxCfg::disabled(),
        },
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "child was not killed on cancel"
    );
    assert!(
        matches!(cap.outcome, Outcome::Cancelled),
        "expected Cancelled"
    );
}

// ── V33 — the sandboxed audit seam ────────────────────────────────────

/// The backstop relation for this seam. An audit tool's budget is the
/// user's `code_audit.timeout_secs` (or a per-tool override), so the
/// relation is asserted across the range rather than on one constant: if
/// the caller-side deadline ever failed to outlast the child's, an
/// ordinary slow semgrep run would be reported as a *wedge*, which is the
/// one row in that lane that is supposed to mean cImp itself is broken.
#[test]
fn the_audit_sandbox_backstop_always_exceeds_the_tool_timeout() {
    for secs in [1u64, 60, 300, 1800, 7200] {
        let child = Duration::from_secs(secs);
        let backstop = crate::sandbox::backstop_for(child);
        assert!(
            backstop > child,
            "backstop {backstop:?} must exceed the tool timeout {child:?}"
        );
        assert_eq!(backstop, child + crate::sandbox::SANDBOX_SETTLE_SLACK);
    }
}

/// **The report directory is granted exactly when a tool writes one.**
///
/// A `Transport::ReportFile` scanner (gitleaks, cppcheck, dotnet-analyzers)
/// is handed an absolute SARIF path in its argv and writes there; without a
/// write grant on that directory the sandbox turns three working tools into
/// denial rows. A `Transport::Stdout` scanner writes nothing outside the
/// already-granted project root and must get NO extra grant — every entry
/// here widens the boundary.
///
/// Pure-logic only: no ACL is stamped, and the grant is applied solely on
/// the `Sandboxed` arm (`sandbox::plan` discards the hints when the switch
/// is off, which `sandbox::tests::disabled_cfg_yields_off_user` pins).
#[test]
fn only_a_report_file_tool_gets_its_report_directory_granted() {
    let report = audit_report_dir().join("gitleaks-1234.sarif");

    // The write grant is the report path's OWN parent — derived from the
    // same value that goes into argv, so the granted directory and the
    // argument cannot drift apart.
    let granted = sandbox_full_dirs(Transport::ReportFile, Some(&report));
    assert_eq!(granted, vec![audit_report_dir()]);
    assert_eq!(
        granted[0],
        report.parent().unwrap(),
        "the granted dir must be the parent of the path handed to the scanner"
    );
    // It is cImp's own scratch, NOT the user's project tree.
    assert!(granted[0].starts_with(std::env::temp_dir()));
    assert!(granted[0].is_dir(), "the grant target must exist beforehand");

    // A stdout-transport tool gets nothing.
    assert!(sandbox_full_dirs(Transport::Stdout, Some(&report)).is_empty());
    assert!(sandbox_full_dirs(Transport::Stdout, None).is_empty());
    // …and neither does a report-file tool with no path (defensive: the
    // runner always pairs the two, and an empty grant is the safe answer).
    assert!(sandbox_full_dirs(Transport::ReportFile, None).is_empty());

    // Cross-check against the REAL roster — every one of the fourteen, not a
    // hand-listed sample that would rot the moment a tool changed transport
    // or a fifteenth arrived.
    let mut report_file_tools = 0usize;
    for tool in builtin_tools() {
        let tool = RunnableAudit::from_effective(&tool)
            .expect("runnable")
            .expect("an umbrella tool");
        let subject = tool.spawn_subject();
        let path = matches!(tool.transport, Transport::ReportFile)
            .then(|| temp_report_path(&subject));
        let dirs = sandbox_full_dirs(tool.transport, path.as_deref());
        assert_eq!(
            dirs.is_empty(),
            tool.transport == Transport::Stdout,
            "{subject} asks for the wrong grant for its transport"
        );
        // …and the granted directory really is where the ARGUMENT points.
        // The sandboxed path passes argv through `SpawnRequest::args` with
        // no `raw_tail`, so each element is CRT-quoted and the scanner's own
        // runtime parses the identical string back: the absolute report path
        // reaches the tool unmangled, spaces and all.
        if let Some(path) = &path {
            report_file_tools += 1;
            let argv = tool.full_argv(Path::new("/proj"), Some(path), true);
            let rendered = path.to_string_lossy().into_owned();
            assert!(
                argv.iter().any(|a| a.contains(&rendered)),
                "{subject}'s argv does not carry the report path it will be graded on: \
                     {argv:?}"
            );
            assert_eq!(dirs, vec![path.parent().unwrap().to_path_buf()]);
        }
    }
    assert_eq!(
        report_file_tools, 3,
        "gitleaks, cppcheck and dotnet-analyzers write to a report file; a change here is a \
             change to what the sandbox has to grant"
    );
}

/// An audit tool's `sandbox`-lane rows name the SCANNER, not just "an
/// audit" — the lane is scanned by its source column, and `audit:semgrep`
/// hitting the boundary is a different fact from `audit:gitleaks` doing so.
/// Each label is also distinct from the other two seams, which is what
/// keeps `run_command`'s and `run_check`'s rows apart from these.
///
/// The label is a built-in's COMMAND name, so the Security `semgrep` and the
/// Quality `semgrep-quality` share one — deliberately: it is the same binary
/// under the same grants, and the boundary cannot tell them apart either.
/// That is also why `spawn_subject` exists rather than the runner using the
/// tool key: a user who greps `audit:semgrep` after this migration must
/// still find their rows.
#[test]
fn audit_rows_name_the_scanner_and_not_just_the_seam() {
    let mut seen = std::collections::BTreeSet::new();
    for id in [
        "osv-scanner",
        "gitleaks",
        "semgrep",
        "eslint",
        "dotnet-analyzers",
    ] {
        let tool = builtin(id);
        let command = tool.spawn_subject();
        let seam = crate::sandbox::audit_seam(&command);
        assert!(
            seam.starts_with("audit:") && seam.contains(&command),
            "an audit seam label must name its scanner: {seam}"
        );
        assert_ne!(seam, crate::sandbox::SEAM_RUN_COMMAND);
        assert_ne!(seam, crate::sandbox::SEAM_RUN_CHECK);
        seen.insert(seam);
    }
    assert_eq!(seen.len(), 5, "distinct binaries must get distinct labels");
    // …and the documented exception: one binary, one label.
    assert_eq!(
        crate::sandbox::audit_seam(&builtin("semgrep").spawn_subject()),
        crate::sandbox::audit_seam(&builtin("semgrep-quality").spawn_subject()),
    );
    // The label really is the COMMAND, not the tool id — `dotnet-analyzers`
    // runs `dotnet`, and the lane says so.
    assert_eq!(builtin("dotnet-analyzers").spawn_subject(), "dotnet");
}

#[tokio::test]
async fn spawn_error_for_missing_binary() {
    let cancel = CancellationToken::new();
    let cap = spawn_and_capture(
        Path::new("cimp-definitely-not-a-real-binary-xyz"),
        &[],
        &[],
        "test-missing",
        &SpawnPosture::default(),
        &RunCtx {
            root: std::env::temp_dir(),
            timeout: Duration::from_secs(5),
            cancel: cancel.clone(),
            sandbox: crate::sandbox::SandboxCfg::disabled(),
        },
    )
    .await;
    assert!(matches!(cap.outcome, Outcome::SpawnError(_)));
}

// ── V30 Phase C: completion-push gate + payload ────────────────────────

/// A finished snapshot with the given per-tool statuses.
fn done_snapshot(statuses: &[(&str, ToolStatus)], total_findings: usize) -> AuditSnapshot {
    AuditSnapshot {
        root: "/proj/root".to_string(),
        scanning: false,
        last_scan_at: Some(1_700_000_000_000),
        tools: statuses
            .iter()
            .map(|(id, status)| ToolState {
                status: *status,
                ..ToolState::fresh(ToolKey::Builtin((*id).to_string()), Category::Security)
            })
            .collect(),
        census: CensusBlock::default(),
        total_findings,
        truncated: false,
    }
}

/// The gate itself: only a GUI-initiated scan announces itself. An
/// agent-initiated run returns the same report through its own open
/// `tools/call`, so pushing would duplicate it into that session — and a
/// push into an idle tab costs a model turn.
#[test]
fn only_gui_initiated_scans_push() {
    assert!(
        initiator_pushes(Initiator::Gui),
        "the Scan button has no other completion path"
    );
    assert!(
        !initiator_pushes(Initiator::Agent),
        "an MCP/offload-initiated run already returns its report"
    );
}

/// The full gate. `run` is reached on EVERY exit path, so the producer —
/// not the caller — is where cancellation and triviality are filtered out.
#[test]
fn scan_push_worthy_filters_cancelled_agent_and_trivial_scans() {
    let long = AUDIT_PUSH_MIN_SCAN_MS;
    assert!(
        scan_push_worthy(true, Initiator::Gui, false, long),
        "a real GUI scan announces itself"
    );

    // Review M6: "off means off" app-side. The child-side declaration is
    // latched until the tab restarts, so this is the half that can react to
    // the toggle at once — and it dominates everything else.
    assert!(
        !scan_push_worthy(false, Initiator::Gui, false, long),
        "offload.session_push off ⇒ no producer fires, restart or not"
    );

    // Review M3: cancelling must not broadcast "cImp finished a … audit …
    // Call security_audit for the full report (it re-runs the same scan)" —
    // that invites every armed agent to re-run what the user just aborted.
    // A cancel between tools leaves no `Cancelled` chip, so the snapshot alone
    // cannot distinguish the two: the cancel token is the only signal.
    assert!(
        !scan_push_worthy(true, Initiator::Gui, true, long),
        "a cancelled scan must never push"
    );

    // Review LOW: the duration floor the graph twin already had.
    assert!(
        !scan_push_worthy(true, Initiator::Gui, false, 200),
        "a 200ms scan is not worth a model turn in every armed session"
    );
    assert!(
        !scan_push_worthy(true, Initiator::Gui, false, AUDIT_PUSH_MIN_SCAN_MS - 1),
        "just under the floor stays silent"
    );

    // The echo guard still dominates every other input.
    for cancelled in [false, true] {
        for ms in [0, long, long * 10] {
            assert!(
                !scan_push_worthy(true, Initiator::Agent, cancelled, ms),
                "an agent-initiated run never pushes (cancelled={cancelled}, {ms}ms)"
            );
        }
    }

    assert_eq!(
        AUDIT_PUSH_MIN_SCAN_MS, 30_000,
        "same floor as the graph twin's GRAPH_PUSH_MIN_BUILD_MS by design — \
             the two producers cost the same model turn"
    );
}

/// The pushed line is short, factual, and names its pull twin (milestone
/// invariant 2) rather than inlining the report.
#[test]
fn audit_push_notice_states_counts_and_its_pull_twin() {
    let snap = done_snapshot(
        &[
            ("gitleaks", ToolStatus::Done),
            ("osv-scanner", ToolStatus::Done),
        ],
        7,
    );
    let notice = audit_push_notice(&snap, Category::Security);
    let line = notice.content();
    assert_eq!(
        notice.meta.get("kind").map(String::as_str),
        Some("audit"),
        "the notice keeps its channel attribute"
    );
    assert!(line.contains("security"), "names the category: {line}");
    assert!(line.contains("/proj/root"), "names the scope: {line}");
    assert!(line.contains("7 findings"), "carries the count: {line}");
    assert!(line.contains("2 tool(s)"), "counts completed tools: {line}");
    assert!(
        line.contains("security_audit"),
        "names the pull twin, never inlines the report: {line}"
    );
    assert!(
        !line.contains("failed"),
        "no failure clause when nothing failed: {line}"
    );
    assert!(line.len() < 400, "stays a one-liner: {line}");
}

/// A failed tool is surfaced, so "0 findings" from a broken scan can't read
/// as a clean bill of health, and a Quality scan points at `quality_audit`.
#[test]
fn audit_push_notice_reports_failures_and_the_quality_twin() {
    let snap = done_snapshot(
        &[
            ("ruff", ToolStatus::Done),
            ("eslint", ToolStatus::Failed),
            ("pmd", ToolStatus::NotInstalled),
        ],
        0,
    );
    let notice = audit_push_notice(&snap, Category::Quality);
    let line = notice.content();
    assert!(line.contains("quality"), "names the category: {line}");
    assert!(line.contains("0 findings"), "carries the count: {line}");
    assert!(
        line.contains("1 tool(s)"),
        "only `done` tools count: {line}"
    );
    assert!(
        line.contains("1 tool(s) failed"),
        "surfaces failure: {line}"
    );
    assert!(
        line.contains("quality_audit"),
        "names the quality pull twin: {line}"
    );
}

// ── V38 Phase F: tier 2 (provider-backed audit tools) ────────────────────

/// One provider-backed security tool, resolved through the REAL loader and
/// registry so every assertion runs against a validated manifest.
fn provider_tool(enabled: bool) -> EffectiveTool {
    let dir = std::env::temp_dir().join(format!("cimp-provider-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(
        dir.join("acme.json"),
        r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "categories": [{ "id": "sec", "label": "Security", "tools": ["cloud"] }],
              "tools": [{
                "id": "cloud", "label": "Acme Cloud", "kind": "security",
                "provider": { "server": "acme-mcp", "tool": "scan_repository" }
              }]
            }"#,
    )
    .expect("write manifest");
    let set = crate::plugins::loader::scan_dir(
        &dir,
        crate::plugins::manifest::Provenance::User,
    );
    assert!(set.errors.is_empty(), "{:?}", set.errors);
    let mut cfg = crate::settings::ToolPluginsSettings::default();
    cfg.plugins.insert(
        "acme@1.0.0".to_string(),
        crate::settings::PluginState {
            enabled: true,
            tools: std::collections::BTreeMap::from([(
                "cloud".to_string(),
                crate::settings::ToolState {
                    enabled,
                    ..crate::settings::ToolState::default()
                },
            )]),
        },
    );
    let tools = crate::plugins::registry::effective_tools(&set, &cfg, None);
    let _ = std::fs::remove_dir_all(&dir);
    tools.into_iter().next().expect("one tool")
}

/// The registry rule tier 2 turns on: **no path is needed**. A provider tool
/// is runnable on its enable alone, because there is no binary for the user
/// to point at — and it must therefore reach the fan-out, where a missing or
/// disabled server becomes a failed chip rather than silence.
#[test]
fn a_provider_tool_is_runnable_without_a_path() {
    let t = provider_tool(true);
    assert!(t.is_provider());
    assert!(t.path.is_none(), "nothing to point at");
    assert!(!t.resolves_by_name(), "and nothing to resolve by name either");
    assert!(t.runnable(), "enabled is the whole of runnable for tier 2");

    // …and the enable still governs, exactly as for tier 1.
    assert!(!provider_tool(false).runnable());
}

/// It joins `plan_scan`'s population as one more member of its umbrella,
/// carrying the provider reference the runner branches on. The gate a
/// spawned tool passes (`no binary path is configured`) must not fire.
#[test]
fn a_provider_tool_joins_the_fanout_as_a_running_chip() {
    let t = provider_tool(true);
    let runnable = RunnableAudit::from_effective(&t)
        .expect("a provider tool prepares")
        .expect("and belongs to an umbrella");
    let p = runnable.provider.as_ref().expect("the reference travels");
    assert_eq!(p.server, "acme-mcp");
    assert_eq!(p.tool, "scan_repository");
    assert_eq!(runnable.category, Category::Security);
    assert!(runnable.program.is_empty());
    // Forced at load, so the runner never has to ask which gate applies.
    assert_eq!(runnable.parser, AuditParser::Sarif);
    assert_eq!(runnable.gate, crate::audit::runnable::IngestGate::Sarif);

    let (chips, to_run) = plan_scan(
        std::slice::from_ref(&t),
        Category::Security,
        &census::Census::from_parts(&[], &[]),
    );
    assert_eq!(to_run.len(), 1, "it runs");
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0].status, ToolStatus::Running);
    // The quality umbrella is somebody else's fan-out.
    assert!(plan_scan(
        std::slice::from_ref(&t),
        Category::Quality,
        &census::Census::from_parts(&[], &[])
    )
    .1
    .is_empty());
}

fn provider_spec(key: &ToolKey) -> Finalize<'static> {
    Finalize {
        key: key.clone(),
        findings_exit_codes: &[],
        parser: AuditParser::Sarif,
        gate: crate::audit::runnable::IngestGate::Sarif,
    }
}

/// The whole tier-2 ingest contract in one run: a SARIF answer becomes
/// findings attributed to the REGISTRY key, and every other outcome is a
/// failure carrying a reason. Nothing here may read as a clean scan.
#[test]
fn a_provider_answer_goes_through_the_same_gate_and_attribution() {
    let key = ToolKey::Plugin("acme@1.0.0/cloud".to_string());
    let spec = provider_spec(&key);
    let provider = ProviderRef {
        server: "acme-mcp".to_string(),
        tool: "scan_repository".to_string(),
    };
    let root = Path::new("C:\\proj");
    let t = Duration::from_secs(60);

    // A SARIF log that says something. The findings are the registry
    // entry's, NOT `runs[].tool.driver.name`'s — a provider that claimed to
    // be `gitleaks` would otherwise file its findings under a built-in.
    let sarif = r#"{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"gitleaks"}},
          "results":[{"ruleId":"ACME1","level":"error",
            "message":{"text":"secret"},
            "locations":[{"physicalLocation":{
              "artifactLocation":{"uri":"src/a.rs"},
              "region":{"startLine":3}}}]}]}]}"#;
    let (status, findings, error) = finalize_provider(
        &spec,
        &ProviderOutcome::Answered(sarif.to_string()),
        &provider,
        root,
        t,
    );
    assert_eq!(status, ToolStatus::Done);
    assert_eq!(error, None);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].tool, key, "attribution is the key that ran");

    // A SARIF log that ran and found nothing is a CLEAN pass — the
    // distinction the gate exists for.
    let (status, findings, _) = finalize_provider(
        &spec,
        &ProviderOutcome::Answered(r#"{"version":"2.1.0","runs":[]}"#.to_string()),
        &provider,
        root,
        t,
    );
    assert_eq!(status, ToolStatus::Done);
    assert!(findings.is_empty());

    // Empty, and JSON-that-is-not-SARIF: a tool that said nothing at all,
    // and one whose output cImp never understood. Neither is a clean scan.
    for answer in ["", "   ", "{}", "not json", r#"{"ok":true}"#] {
        let (status, findings, error) = finalize_provider(
            &spec,
            &ProviderOutcome::Answered(answer.to_string()),
            &provider,
            root,
            t,
        );
        assert_eq!(status, ToolStatus::Failed, "answer {answer:?} must not pass");
        assert!(findings.is_empty());
        assert!(error.is_some());
    }

    // Cancel, timeout and a refusal from the host: three different facts,
    // three different messages, none of them a pass. A provider cancel
    // takes the same route a spawned tool's does — V38 gave it its own
    // status, so it is not reported as a tool failure.
    let (status, _, error) =
        finalize_provider(&spec, &ProviderOutcome::Cancelled, &provider, root, t);
    assert_eq!(status, ToolStatus::Cancelled);
    assert!(error.unwrap_or_default().contains("cancelled"));

    let (status, _, error) =
        finalize_provider(&spec, &ProviderOutcome::TimedOut, &provider, root, t);
    assert_eq!(status, ToolStatus::Failed);
    assert!(error.unwrap_or_default().contains("timed out"));

    let refusal = crate::offload::mcp_host::REFUSAL_DISABLED;
    let (status, _, error) = finalize_provider(
        &spec,
        &ProviderOutcome::Failed(format!("server `acme-mcp` {refusal}server toggle)")),
        &provider,
        root,
        t,
    );
    assert_eq!(status, ToolStatus::Failed);
    let error = error.unwrap_or_default();
    assert!(
        error.contains("acme-mcp") && error.contains(refusal),
        "a disabled server's refusal reaches the chip verbatim: {error}"
    );
}

/// V38 — the two host errors that are NOT tool failures, from the
/// classification the host stamps to the words the report prints.
///
/// Both were live defects on the same call path: a provider slower than the
/// host's 45s came back as `http request failed: error sending request for
/// url (…)` and was reported as a broken tool, and a server the user had
/// switched off was counted under "failed" beside it. Neither could be told
/// apart from a real fault by the string, which is why both are asserted
/// through `is_timeout` / `is_disabled_by_toggle` and not through wording.
#[test]
fn a_host_timeout_reads_as_timed_out_and_a_toggle_refusal_as_disabled() {
    let key = ToolKey::Plugin("acme@1.0.0/cloud".to_string());
    let spec = provider_spec(&key);
    let provider = ProviderRef {
        server: "acme-mcp".to_string(),
        tool: "scan_repository".to_string(),
    };
    let root = Path::new("C:\\proj");
    let t = Duration::from_secs(600);

    // 1) The inner deadline expiring is the same verdict as the outer timer
    //    expiring — the report must never call it a connection failure.
    let outcome = provider_outcome(Err(HostError::timed_out(t)));
    assert!(matches!(outcome, ProviderOutcome::TimedOut));
    let (status, findings, error) = finalize_provider(&spec, &outcome, &provider, root, t);
    assert_eq!(status, ToolStatus::Failed);
    assert!(findings.is_empty());
    let error = error.unwrap_or_default();
    assert!(error.contains("timed out"), "{error}");
    assert!(
        !error.contains("http request failed") && !error.contains("not reachable"),
        "a slow server is not an unreachable one: {error}"
    );

    // 2) The user's own toggle: disabled, with the sentence that names it.
    let refusal = format!(
        "server `acme-mcp` {}{}",
        crate::offload::mcp_host::REFUSAL_DISABLED,
        crate::offload::mcp_host::REFUSAL_DISABLED_BY_SERVER
    );
    let outcome = provider_outcome(Err(HostError::disabled_by_toggle(refusal.clone())));
    assert!(matches!(outcome, ProviderOutcome::RefusedDisabled(_)));
    let (status, findings, error) = finalize_provider(&spec, &outcome, &provider, root, t);
    assert_eq!(
        status,
        ToolStatus::Idle,
        "`Idle` is the state `format_result` counts and renders as `disabled`"
    );
    assert!(findings.is_empty(), "a server that was never called found nothing");
    assert_eq!(error.as_deref(), Some(refusal.as_str()));

    // 3) An unclassified host error is still a failure — the default must
    //    not excuse anything a new error site forgets to classify.
    let outcome = provider_outcome(Err(HostError::from("the server exploded")));
    assert!(matches!(outcome, ProviderOutcome::Failed(_)));
    let (status, _, error) = finalize_provider(&spec, &outcome, &provider, root, t);
    assert_eq!(status, ToolStatus::Failed);
    assert!(error.unwrap_or_default().contains("did not deliver findings"));

    // 4) A successful answer is untouched by the new branch.
    assert!(matches!(
        provider_outcome(Ok("x".to_string())),
        ProviderOutcome::Answered(_)
    ));
}
