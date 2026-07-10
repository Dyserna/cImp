//! Built-in [`ParserKind`] implementations — decode one checker's captured
//! stdout/stderr into a flat `Vec<Diag>` (dedup happens in `checks::run`).
//! Each parser is intentionally lenient: a line it can't parse is skipped,
//! never an error — a checker's output format drifting across versions
//! should degrade the diagnostic count, not break `run_check` outright.

use std::borrow::Cow;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use super::{Diag, ParserKind, Severity};

/// Dispatch to the parser named by `kind`. The line-oriented parsers get
/// ANSI-stripped input — a user-configured checker command can leak color
/// (FORCE_COLOR, a tool that doesn't tty-detect), and escape sequences glued
/// to the file path would otherwise break both the regex match and the
/// downstream changed-file comparison. The JSON parsers don't need it (JSON
/// string content never carries raw ESC from these tools).
pub fn parse(kind: ParserKind, stdout: &str, stderr: &str) -> Vec<Diag> {
    match kind {
        ParserKind::CargoJson => parse_cargo_json(stdout),
        ParserKind::EslintJson => parse_eslint_json(stdout),
        ParserKind::Tsc => parse_tsc(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::Pytest => parse_pytest(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::GenericGcc => parse_generic_gcc(&strip_ansi(stdout), &strip_ansi(stderr)),
    }
}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // CSI sequences (colors, cursor movement, erase-line) — the escape shapes
    // CLI checkers actually emit.
    RE.get_or_init(|| Regex::new("\x1b\\[[0-9;?]*[A-Za-z]").expect("valid regex"))
}

/// Remove ANSI CSI escape sequences. Borrows unchanged input (the common,
/// color-free case) so the per-run cost is one `contains` scan.
fn strip_ansi(s: &str) -> Cow<'_, str> {
    if !s.contains('\x1b') {
        return Cow::Borrowed(s);
    }
    ansi_re().replace_all(s, "")
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

/// tsc's project-level diagnostics carry no `file(line,col):` prefix at all —
/// `error TS18003: No inputs were found in config file 'tsconfig.json'.` — and
/// dropping them would make a completely broken tsconfig read as *zero*
/// diagnostics. Mirrors the cargo parser's span-less handling: empty location.
fn tsc_global_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(error|warning) (TS\d+): (.*)$").expect("valid regex"))
}

fn parse_tsc(stdout: &str, stderr: &str) -> Vec<Diag> {
    let re = tsc_re();
    let global_re = tsc_global_re();
    stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            if let Some(caps) = re.captures(line) {
                let severity = if &caps[4] == "error" { Severity::Error } else { Severity::Warning };
                return Some(Diag {
                    severity,
                    code: Some(caps[5].to_string()),
                    message: caps[6].to_string(),
                    file: caps[1].to_string(),
                    line: caps[2].parse().ok()?,
                    col: caps[3].parse().ok(),
                });
            }
            let caps = global_re.captures(line)?;
            let severity = if &caps[1] == "error" { Severity::Error } else { Severity::Warning };
            Some(Diag {
                severity,
                code: Some(caps[2].to_string()),
                message: caps[3].to_string(),
                file: String::new(),
                line: 0,
                col: None,
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
    /// 2 = error, 1 = warning (eslint's own convention). Defaulted so one
    /// message missing the field degrades to a warning instead of failing the
    /// whole-document parse (which would null out the entire run).
    #[serde(default)]
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
    // A wrapper script can prefix the stream with a UTF-8 BOM, which `trim`
    // does not strip and which would fail the whole-document parse.
    let doc = stdout.trim_start_matches('\u{feff}').trim();
    let Ok(files) = serde_json::from_str::<Vec<EslintFileResult>>(doc) else {
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
            let (nodeid, message) = match split_nodeid_reason(rest) {
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

/// Split a `FAILED` line's remainder into `(nodeid, reason)` at the first
/// ` - ` **outside brackets** — a parametrized node id can itself contain the
/// separator (`test_foo.py::test_bar[a - b] - AssertionError`), and a naive
/// `split_once` would cut inside the parameter, garbling the message.
fn split_nodeid_reason(rest: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (i, b) in rest.bytes().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b' ' if depth == 0 && rest[i..].starts_with(" - ") => {
                return Some((&rest[..i], &rest[i + 3..]));
            }
            _ => {}
        }
    }
    None
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
    RE.get_or_init(|| {
        Regex::new(r"^(.+?):(\d+)(?::(\d+))?:\s*(fatal error|error|warning|note)?:?\s*(.*)$")
            .expect("valid regex")
    })
}

/// `file:line[:col]: [error|warning|note:] message` — gcc/clang's classic
/// shape and the fallback for anything else line-oriented. Lines with no
/// trailing message (a bare `file:line:` with nothing after it) are skipped —
/// almost always a false match on non-diagnostic output (e.g. a timestamp).
/// Two more junk shapes are rejected: an all-digit "file" (a timestamp like
/// `12:34:56 Build succeeded` matches the regex as file="12") and a "file"
/// starting with whitespace (compilers print paths at column 0; indented
/// matches are decoration like cargo's `  --> src/main.rs:2:5`).
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
            let file = &caps[1];
            if file.bytes().all(|b| b.is_ascii_digit()) || file.starts_with(char::is_whitespace) {
                return None;
            }
            let severity = match caps.get(4).map(|m| m.as_str()) {
                Some("error") | Some("fatal error") => Severity::Error,
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

    #[test]
    fn tsc_global_error_without_location_is_captured() {
        // A broken tsconfig produces file-less errors; dropping them would
        // make the run read as zero diagnostics.
        let diags = parse_tsc("error TS18003: No inputs were found in config file 'tsconfig.json'.\n", "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("TS18003"));
        assert_eq!(diags[0].file, "");
        assert_eq!(diags[0].line, 0);
        assert!(diags[0].message.contains("No inputs"));
    }

    #[test]
    fn tsc_windows_path_with_drive_letter_parses() {
        let diags = parse_tsc(r"C:\repo\src\app.ts(12,5): error TS2345: bad arg", "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, r"C:\repo\src\app.ts");
        assert_eq!(diags[0].line, 12);
    }

    #[test]
    fn ansi_colored_output_still_parses() {
        let colored = "\x1b[96msrc/app.ts\x1b[0m(\x1b[93m12\x1b[0m,\x1b[93m5\x1b[0m): \x1b[91merror\x1b[0m TS2345: bad arg\n";
        let diags = parse(ParserKind::Tsc, colored, "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "src/app.ts");
        assert_eq!(diags[0].line, 12);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn strip_ansi_borrows_when_clean() {
        assert!(matches!(strip_ansi("no escapes here"), Cow::Borrowed(_)));
        assert_eq!(strip_ansi("\x1b[1;31mred\x1b[0m"), "red");
    }

    #[test]
    fn eslint_message_without_severity_degrades_to_warning() {
        // One message missing `severity` must not null out the whole run.
        let json = r#"[{"filePath":"/repo/a.js","messages":[
            {"ruleId":"x","message":"no severity","line":1},
            {"ruleId":"y","severity":2,"message":"real error","line":2}
        ]}]"#;
        let diags = parse_eslint_json(json);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert_eq!(diags[1].severity, Severity::Error);
    }

    #[test]
    fn eslint_json_with_bom_parses() {
        let json = format!("\u{feff}{ESLINT_JSON}");
        assert_eq!(parse_eslint_json(&json).len(), 2);
    }

    #[test]
    fn pytest_parametrized_nodeid_with_dash_splits_after_brackets() {
        let diags = parse_pytest("FAILED tests/test_foo.py::test_bar[a - b] - AssertionError: nope\n", "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "tests/test_foo.py");
        assert_eq!(diags[0].message, "AssertionError: nope");
    }

    #[test]
    fn generic_gcc_fatal_error_is_error_severity() {
        let diags = parse_generic_gcc("src/main.c:1:10: fatal error: bar.h: No such file or directory\n", "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].message, "bar.h: No such file or directory");
    }

    #[test]
    fn generic_gcc_rejects_timestamp_and_indented_decoration() {
        // `12:34:56 Build succeeded` matches the regex shape with file="12";
        // cargo's human `  --> src/main.rs:2:5` matches with an indented file.
        let out = "12:34:56 Build succeeded\n  --> src/main.rs:2:5\n";
        assert!(parse_generic_gcc(out, "").is_empty());
    }

    #[test]
    fn generic_gcc_windows_drive_letter_path_parses() {
        let diags = parse_generic_gcc(r"C:\repo\src\main.c:10:5: error: bad", "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, r"C:\repo\src\main.c");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].severity, Severity::Error);
    }
}
