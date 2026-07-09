//! Built-in [`ParserKind`] implementations — decode one checker's captured
//! stdout/stderr into a flat `Vec<Diag>` (dedup happens in `checks::run`).
//! Each parser is intentionally lenient: a line it can't parse is skipped,
//! never an error — a checker's output format drifting across versions
//! should degrade the diagnostic count, not break `run_check` outright.

use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use super::{Diag, ParserKind, Severity};

/// Dispatch to the parser named by `kind`.
pub fn parse(kind: ParserKind, stdout: &str, stderr: &str) -> Vec<Diag> {
    match kind {
        ParserKind::CargoJson => parse_cargo_json(stdout),
        ParserKind::Tsc => parse_tsc(stdout, stderr),
        ParserKind::EslintJson => parse_eslint_json(stdout),
        ParserKind::Pytest => parse_pytest(stdout, stderr),
        ParserKind::GenericGcc => parse_generic_gcc(stdout, stderr),
    }
}

// ── cargo --message-format=json ───────────────────────────────────────────

#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    #[serde(default)]
    message: Option<CargoDiagnostic>,
}

#[derive(Deserialize)]
struct CargoDiagnostic {
    level: String,
    #[serde(default)]
    code: Option<CargoCode>,
    message: String,
    #[serde(default)]
    spans: Vec<CargoSpan>,
}

#[derive(Deserialize)]
struct CargoCode {
    code: String,
}

#[derive(Deserialize)]
struct CargoSpan {
    file_name: String,
    line_start: u32,
    column_start: u32,
    #[serde(default)]
    is_primary: bool,
}

/// One JSON object per line (`--message-format=json`); only
/// `reason == "compiler-message"` lines carry a diagnostic — build-script
/// output, artifact notifications, etc. are skipped. The primary span (the
/// one the compiler carets) wins over the first span when there's more than
/// one; a span-less message (e.g. "aborting due to N previous errors") gets
/// an empty location rather than being dropped.
fn parse_cargo_json(stdout: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<CargoMessage>(line) else { continue };
        if msg.reason != "compiler-message" {
            continue;
        }
        let Some(diag) = msg.message else { continue };
        let severity = match diag.level.as_str() {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            _ => Severity::Note,
        };
        let span = diag.spans.iter().find(|s| s.is_primary).or_else(|| diag.spans.first());
        let (file, line_no, col) = match span {
            Some(s) => (s.file_name.clone(), s.line_start, Some(s.column_start)),
            None => (String::new(), 0, None),
        };
        out.push(Diag { severity, code: diag.code.map(|c| c.code), message: diag.message, file, line: line_no, col });
    }
    out
}

// ── tsc ────────────────────────────────────────────────────────────────

fn tsc_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.+)\((\d+),(\d+)\): (error|warning) (TS\d+): (.*)$").expect("valid regex"))
}

fn parse_tsc(stdout: &str, stderr: &str) -> Vec<Diag> {
    let re = tsc_re();
    stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            let caps = re.captures(line)?;
            let severity = if &caps[4] == "error" { Severity::Error } else { Severity::Warning };
            Some(Diag {
                severity,
                code: Some(caps[5].to_string()),
                message: caps[6].to_string(),
                file: caps[1].to_string(),
                line: caps[2].parse().ok()?,
                col: caps[3].parse().ok(),
            })
        })
        .collect()
}

// ── eslint --format json ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct EslintFileResult {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(default)]
    messages: Vec<EslintMessage>,
}

#[derive(Deserialize)]
struct EslintMessage {
    #[serde(rename = "ruleId", default)]
    rule_id: Option<String>,
    /// 2 = error, 1 = warning (eslint's own convention).
    severity: u8,
    message: String,
    #[serde(default)]
    line: u32,
    #[serde(default)]
    column: Option<u32>,
}

/// The whole output is one JSON array (not line-delimited, unlike cargo's
/// message stream) — a malformed/truncated document yields no diagnostics
/// rather than a partial parse.
fn parse_eslint_json(stdout: &str) -> Vec<Diag> {
    let Ok(files) = serde_json::from_str::<Vec<EslintFileResult>>(stdout.trim()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in files {
        for m in f.messages {
            let severity = if m.severity >= 2 { Severity::Error } else { Severity::Warning };
            out.push(Diag { severity, code: m.rule_id, message: m.message, file: f.file_path.clone(), line: m.line, col: m.column });
        }
    }
    out
}

// ── pytest ─────────────────────────────────────────────────────────────

/// The short test-summary section's `FAILED <nodeid> - <reason>` lines, plus
/// the tail counts line (e.g. `2 failed, 10 passed in 1.23s`, folded in as a
/// file-less `Note` diagnostic so it still surfaces in the report). No line
/// number is available from this section (pytest's short summary doesn't
/// carry one) — `line` is `0`.
fn parse_pytest(stdout: &str, stderr: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("FAILED ") {
            let (nodeid, message) = match rest.split_once(" - ") {
                Some((n, m)) => (n, m.trim().to_string()),
                None => (rest, "test failed".to_string()),
            };
            let file = nodeid.split("::").next().unwrap_or(nodeid).to_string();
            out.push(Diag { severity: Severity::Error, code: None, message, file, line: 0, col: None });
        } else if let Some(summary) = pytest_summary_line(line) {
            out.push(Diag { severity: Severity::Note, code: None, message: summary.to_string(), file: String::new(), line: 0, col: None });
        }
    }
    out
}

/// The pytest tail line, e.g. `====== 2 failed, 10 passed in 1.23s =======` —
/// padded with `=` on both sides, starts (once trimmed) with a digit, and
/// mentions one of the standard outcome words. Returns the trimmed line.
fn pytest_summary_line(line: &str) -> Option<&str> {
    let trimmed = line.trim_matches(|c: char| c == '=' || c.is_whitespace());
    let starts_with_digit = trimmed.chars().next().is_some_and(|c| c.is_ascii_digit());
    let has_outcome = ["passed", "failed", "error", "skipped"].iter().any(|w| trimmed.contains(w));
    (starts_with_digit && has_outcome).then_some(trimmed)
}

// ── generic file:line:col fallback ────────────────────────────────────────

fn generic_gcc_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.+?):(\d+)(?::(\d+))?:\s*(error|warning|note)?:?\s*(.*)$").expect("valid regex"))
}

/// `file:line[:col]: [error|warning|note:] message` — gcc/clang's classic
/// shape and the fallback for anything else line-oriented. Lines with no
/// trailing message (a bare `file:line:` with nothing after it) are skipped —
/// almost always a false match on non-diagnostic output (e.g. a timestamp).
fn parse_generic_gcc(stdout: &str, stderr: &str) -> Vec<Diag> {
    let re = generic_gcc_re();
    stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            let caps = re.captures(line)?;
            let message = caps.get(5).map(|m| m.as_str().trim()).unwrap_or("");
            if message.is_empty() {
                return None;
            }
            let severity = match caps.get(4).map(|m| m.as_str()) {
                Some("error") => Severity::Error,
                Some("warning") => Severity::Warning,
                _ => Severity::Note,
            };
            Some(Diag {
                severity,
                code: None,
                message: message.to_string(),
                file: caps[1].to_string(),
                line: caps[2].parse().ok()?,
                col: caps.get(3).and_then(|m| m.as_str().parse().ok()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO_JSON: &str = r#"{"reason":"compiler-artifact","package_id":"foo 0.1.0"}
{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0425","explanation":null},"message":"cannot find value `x` in this scope","spans":[{"file_name":"src/lib.rs","line_start":2,"line_end":2,"column_start":5,"column_end":6,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"unused variable: `y`","spans":[{"file_name":"src/lib.rs","line_start":5,"line_end":5,"column_start":9,"column_end":10,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"error","code":null,"message":"aborting due to previous error","spans":[]}}
not json at all
"#;

    #[test]
    fn cargo_json_extracts_level_code_span() {
        let diags = parse_cargo_json(CARGO_JSON);
        assert_eq!(diags.len(), 3);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("E0425"));
        assert_eq!(diags[0].file, "src/lib.rs");
        assert_eq!(diags[0].line, 2);
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[1].code, None);
        // Span-less message still comes through, with an empty location.
        assert_eq!(diags[2].file, "");
        assert_eq!(diags[2].line, 0);
    }

    const TSC_OUTPUT: &str = "src/app.ts(12,5): error TS2345: Argument of type 'string' is not assignable to parameter of type 'number'.\nsrc/util.ts(3,1): warning TS6133: 'foo' is declared but never used.\nsomething unrelated on stdout\n";

    #[test]
    fn tsc_parses_file_line_col_level_code_message() {
        let diags = parse_tsc(TSC_OUTPUT, "");
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].file, "src/app.ts");
        assert_eq!(diags[0].line, 12);
        assert_eq!(diags[0].col, Some(5));
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("TS2345"));
        assert!(diags[0].message.starts_with("Argument of type"));
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[1].code.as_deref(), Some("TS6133"));
    }

    const ESLINT_JSON: &str = r#"[
      {"filePath":"/repo/src/a.js","messages":[
        {"ruleId":"no-unused-vars","severity":2,"message":"'x' is defined but never used.","line":3,"column":7},
        {"ruleId":"eqeqeq","severity":1,"message":"Expected '===' and instead saw '=='.","line":10,"column":4}
      ]},
      {"filePath":"/repo/src/b.js","messages":[]}
    ]"#;

    #[test]
    fn eslint_json_maps_severity_and_rule() {
        let diags = parse_eslint_json(ESLINT_JSON);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("no-unused-vars"));
        assert_eq!(diags[0].file, "/repo/src/a.js");
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[1].code.as_deref(), Some("eqeqeq"));
    }

    #[test]
    fn eslint_json_malformed_yields_empty() {
        assert!(parse_eslint_json("not json").is_empty());
    }

    const PYTEST_OUTPUT: &str = "============ short test summary info ============\nFAILED tests/test_foo.py::test_bar - AssertionError: expected 1 got 2\nFAILED tests/test_baz.py::test_qux - ValueError: bad input\n======= 2 failed, 10 passed in 1.23s =======\n";

    #[test]
    fn pytest_extracts_failures_and_tail_summary() {
        let diags = parse_pytest(PYTEST_OUTPUT, "");
        assert_eq!(diags.len(), 3);
        assert_eq!(diags[0].file, "tests/test_foo.py");
        assert!(diags[0].message.contains("AssertionError"));
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[1].file, "tests/test_baz.py");
        assert_eq!(diags[2].severity, Severity::Note);
        assert!(diags[2].message.contains("2 failed"));
    }

    #[test]
    fn pytest_failed_line_without_reason_still_parses() {
        let diags = parse_pytest("FAILED tests/test_x.py::test_y\n", "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "tests/test_x.py");
    }

    const GCC_OUTPUT: &str = "src/main.c:10:5: error: expected ';' before '}' token\nsrc/main.c:20: warning: unused variable 'z'\nCompiling foo...\nsrc/util.c:4:2: note: in expansion of macro\n";

    #[test]
    fn generic_gcc_parses_file_line_col_level_message() {
        let diags = parse_generic_gcc(GCC_OUTPUT, "");
        assert_eq!(diags.len(), 3);
        assert_eq!(diags[0].file, "src/main.c");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].col, Some(5));
        assert_eq!(diags[0].severity, Severity::Error);
        // No column group in this line.
        assert_eq!(diags[1].line, 20);
        assert_eq!(diags[1].col, None);
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[2].severity, Severity::Note);
        // A non-matching line ("Compiling foo...") produced no diagnostic.
    }

    #[test]
    fn generic_gcc_skips_lines_with_no_message() {
        assert!(parse_generic_gcc("src/main.c:10:5:\n", "").is_empty());
    }
}
