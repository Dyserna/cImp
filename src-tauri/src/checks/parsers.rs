//! Built-in [`ParserKind`] implementations — decode one checker's captured
//! stdout/stderr into a flat `Vec<Diag>` (dedup happens in `checks::run`).
//! Each parser is intentionally lenient: a line it can't parse is skipped,
//! never an error — a checker's output format drifting across versions
//! should degrade the diagnostic count, not break `run_check` outright.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use super::{Diag, ParserKind, Severity};

/// Dispatch to the parser named by `kind`. The line-oriented parsers get
/// ANSI-stripped input — a user-configured checker command can leak color
/// (FORCE_COLOR, a tool that doesn't tty-detect), and escape sequences glued
/// to the file path would otherwise break both the regex match and the
/// downstream changed-file comparison. The JSON parsers don't need it at the
/// top level (JSON string content never carries raw ESC from these tools —
/// though `jest` embeds color *inside* its JSON message strings, stripped
/// there). `cwd` is the run root: `jest`/`vitest` report absolute
/// `testFilePath`s, and only the parser that consumes them needs it to
/// relativize (see [`parse_jest_json`]); every other arm ignores it.
pub fn parse(kind: ParserKind, stdout: &str, stderr: &str, cwd: &Path) -> Vec<Diag> {
    match kind {
        ParserKind::CargoJson => parse_cargo_json(stdout),
        ParserKind::EslintJson => parse_eslint_json(stdout),
        ParserKind::Tsc => parse_tsc(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::Pytest => parse_pytest(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::CargoTest => parse_cargo_test(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::JestJson => parse_jest_json(stdout, cwd),
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

// ── cargo test (stable text output) ─────────────────────────────────────

fn cargo_panic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Newer rustc: `… panicked at src/lib.rs:42:9:`; older single-line form:
    // `… panicked at 'message', src/lib.rs:42:9`. The optional `'…', ` swallows
    // the old-form message so the capture is the path in both. The non-greedy
    // path lets the `:line:col` anchor win past a Windows drive letter's own
    // colon (`C:\repo\lib.rs:42:9`).
    RE.get_or_init(|| Regex::new(r"panicked at (?:'.*', )?(.+?):(\d+):(\d+)").expect("valid regex"))
}

fn cargo_panic_tail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The *older multi-line* panic form ends the block with the location on its
    // own tail: `… right: `2`', src/lib.rs:5:5` — the `panicked at` anchor is
    // several lines up, so `cargo_panic_re` can't see it. Match the trailing
    // `', <file>:<line>:<col>` instead.
    RE.get_or_init(|| Regex::new(r"', (.+?):(\d+):(\d+)\s*$").expect("valid regex"))
}

fn rustc_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // rustc's human diagnostic header: `error[E0308]: mismatched types` /
    // `warning: unused variable` — the code bracket is optional (lints and
    // `error: aborting due to …` carry none).
    RE.get_or_init(|| Regex::new(r"^(error|warning)(?:\[([A-Za-z0-9_]+)\])?: (.+)$").expect("valid regex"))
}

fn rustc_arrow_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The location line under a rustc header: `  --> src/main.rs:10:5`.
    RE.get_or_init(|| Regex::new(r"^\s*-->\s+(.+?):(\d+):(\d+)").expect("valid regex"))
}

/// `test tests::foo ... FAILED` ⇒ `Some("tests::foo")`. Only `FAILED` lines
/// produce a diagnostic — `ok`/`ignored` don't.
fn failed_test_name(line: &str) -> Option<&str> {
    Some(line.strip_prefix("test ")?.strip_suffix(" ... FAILED")?.trim())
}

/// `---- tests::foo stdout ----` (or `stderr`) ⇒ `Some("tests::foo")`.
fn stdout_block_name(line: &str) -> Option<&str> {
    let inner = line.strip_prefix("---- ")?.strip_suffix(" ----")?;
    let name = inner.strip_suffix(" stdout").or_else(|| inner.strip_suffix(" stderr")).unwrap_or(inner);
    Some(name.trim())
}

/// Capture one failure's stdout block starting at `start`, stopping at the next
/// block header / the `failures:` summary list / the `test result:` tail / EOF.
/// Returns the block's lines and the index to resume scanning from.
fn capture_block<'a>(lines: &[&'a str], start: usize) -> (Vec<&'a str>, usize) {
    let mut block = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let t = lines[i].trim();
        if (t.starts_with("---- ") && t.ends_with(" ----")) || t == "failures:" || t.starts_with("test result:") {
            break;
        }
        block.push(lines[i]);
        i += 1;
    }
    (block, i)
}

/// Join the first `max` lines of a block (leading/trailing blank lines
/// trimmed) — the truncated context that becomes a failure's message.
fn truncate_lines(block: &[&str], max: usize) -> String {
    let start = block.iter().position(|l| !l.trim().is_empty()).unwrap_or(block.len());
    let end = block.iter().rposition(|l| !l.trim().is_empty()).map(|i| i + 1).unwrap_or(start);
    let trimmed = &block[start..end];
    let take = trimmed.len().min(max);
    trimmed[..take].join("\n")
}

/// Resolve a panic's `file:line:col` from a failure block: the modern anchored
/// form first (the `panicked at …:line:col` line), then the older multi-line
/// tail form (`', file:line:col`).
fn panic_location(block: &[&str]) -> Option<(String, u32, u32)> {
    let loc = |c: &regex::Captures| Some((c[1].to_string(), c[2].parse().ok()?, c[3].parse().ok()?));
    for line in block {
        if let Some(c) = cargo_panic_re().captures(line) {
            return loc(&c);
        }
    }
    for line in block {
        if let Some(c) = cargo_panic_tail_re().captures(line) {
            return loc(&c);
        }
    }
    None
}

/// rustc's two-line human diagnostic: an `error[E0308]: msg` / `warning: msg`
/// header followed within a couple of lines by `  --> file:line:col`. The
/// generic matcher can't see these (the header carries no `file:line`, and the
/// `-->` line is indented decoration it deliberately rejects), so a
/// compile-error `run_check` would otherwise report nothing.
fn parse_rustc_human(lines: &[&str]) -> Vec<Diag> {
    let header = rustc_header_re();
    let arrow = rustc_arrow_re();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(caps) = header.captures(line.trim_end()) else { continue };
        let severity = if &caps[1] == "error" { Severity::Error } else { Severity::Warning };
        let (mut file, mut line_no, mut col) = (String::new(), 0u32, None);
        for look in lines.iter().skip(i + 1).take(3) {
            if let Some(a) = arrow.captures(look) {
                file = a[1].to_string();
                line_no = a[2].parse().unwrap_or(0);
                col = a[3].parse().ok();
                break;
            }
        }
        out.push(Diag {
            severity,
            code: caps.get(2).map(|m| m.as_str().to_string()),
            message: caps[3].to_string(),
            file,
            line: line_no,
            col,
        });
    }
    out
}

/// `cargo test`'s stable text output. Each `test <name> ... FAILED` line is one
/// `Error` diag (`"<name> failed"`), upgraded in place when its
/// `---- <name> stdout ----` panic block follows: the block (first ~15 lines)
/// becomes the message and the `panicked at file:line:col` location fills
/// `file`/`line`/`col`. The tail `test result: …` line folds in as a file-less
/// `Note` (the pytest tail-line trick) so a clean run still renders its counts
/// instead of `"No diagnostics."`. When the build never got to running tests (a
/// compile error aborts first — no `test …` line appears at all) the input is
/// additionally run through the generic matcher plus a local rustc two-line
/// pass, so the compile error surfaces instead of nothing.
fn parse_cargo_test(stdout: &str, stderr: &str) -> Vec<Diag> {
    let combined: Vec<&str> = stdout.lines().chain(stderr.lines()).collect();
    let mut out: Vec<Diag> = Vec::new();
    let mut idx_of: HashMap<String, usize> = HashMap::new();
    let mut saw_test_line = false;

    let mut i = 0;
    while i < combined.len() {
        let trimmed = combined[i].trim();

        if let Some(name) = failed_test_name(trimmed) {
            saw_test_line = true;
            idx_of.entry(name.to_string()).or_insert_with(|| {
                out.push(Diag {
                    severity: Severity::Error,
                    code: None,
                    message: format!("{name} failed"),
                    file: String::new(),
                    line: 0,
                    col: None,
                });
                out.len() - 1
            });
            i += 1;
            continue;
        }
        if trimmed.starts_with("test ") && trimmed.contains(" ... ") {
            saw_test_line = true;
        }
        if let Some(rest) = trimmed.strip_prefix("test result:") {
            saw_test_line = true;
            out.push(Diag {
                severity: Severity::Note,
                code: None,
                message: format!("test result:{rest}"),
                file: String::new(),
                line: 0,
                col: None,
            });
            i += 1;
            continue;
        }
        if let Some(name) = stdout_block_name(trimmed) {
            let (block, next) = capture_block(&combined, i + 1);
            if let Some(&di) = idx_of.get(name) {
                let msg = truncate_lines(&block, 15);
                if !msg.is_empty() {
                    out[di].message = msg;
                }
                if let Some((f, l, c)) = panic_location(&block) {
                    out[di].file = f;
                    out[di].line = l;
                    out[di].col = Some(c);
                }
            }
            i = next;
            continue;
        }
        i += 1;
    }

    // A compile error aborts before any test line — surface it via the generic
    // matcher plus the local rustc two-line pass (`parse_rustc_human`).
    if !saw_test_line {
        out.extend(parse_generic_gcc(stdout, stderr));
        out.extend(parse_rustc_human(&combined));
    }
    out
}

// ── jest / vitest --json ────────────────────────────────────────────────

#[derive(Deserialize)]
struct JestReport {
    #[serde(default, rename = "numPassedTests")]
    num_passed_tests: Option<u64>,
    #[serde(default, rename = "numFailedTests")]
    num_failed_tests: Option<u64>,
    #[serde(default, rename = "testResults")]
    test_results: Vec<JestFileResult>,
}

#[derive(Deserialize)]
struct JestFileResult {
    #[serde(default, rename = "testFilePath")]
    test_file_path: String,
    #[serde(default, rename = "assertionResults")]
    assertion_results: Vec<JestAssertion>,
}

#[derive(Deserialize)]
struct JestAssertion {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "failureMessages")]
    failure_messages: Vec<String>,
}

/// Strip `cwd` from an absolute path when it's a textual prefix (after
/// slash-normalization — no disk `canonicalize`, so this works on the parser's
/// captured output alone). A path outside `cwd` is left as-is; `checks::run`'s
/// `normalize_rel` still canonicalizes it for the changed-file comparison,
/// exactly as it does for eslint's absolute `filePath`s.
fn relativize(cwd: &Path, abs: &str) -> String {
    let abs_fwd = abs.replace('\\', "/");
    let cwd_fwd = cwd.to_string_lossy().replace('\\', "/");
    let cwd_fwd = cwd_fwd.trim_end_matches('/');
    if !cwd_fwd.is_empty() {
        if let Some(rest) = abs_fwd.strip_prefix(cwd_fwd) {
            let rest = rest.trim_start_matches('/');
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    abs_fwd
}

/// Join the first `max` lines of `s`.
fn first_lines(s: &str, max: usize) -> String {
    s.lines().take(max).collect::<Vec<_>>().join("\n")
}

/// The whole output is one JSON document (`jest --json` / `vitest
/// --reporter=json`). Malformed/truncated ⇒ no diagnostics (module posture),
/// never an error. Each failed `assertionResults[]` entry becomes one `Error`
/// from the first ~5 lines of `failureMessages[0]` (ANSI-stripped — jest embeds
/// color codes *inside* the JSON string); `testFilePath` is absolute, so it's
/// relativized against the run `cwd`. Top-level `numPassed/FailedTests` fold in
/// as the counts `Note`.
fn parse_jest_json(stdout: &str, cwd: &Path) -> Vec<Diag> {
    let doc = stdout.trim_start_matches('\u{feff}').trim();
    let Ok(report) = serde_json::from_str::<JestReport>(doc) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for file in &report.test_results {
        let rel = relativize(cwd, &file.test_file_path);
        for a in &file.assertion_results {
            if a.status != "failed" {
                continue;
            }
            let raw = a.failure_messages.first().map(String::as_str).unwrap_or("");
            out.push(Diag {
                severity: Severity::Error,
                code: None,
                message: first_lines(strip_ansi(raw).as_ref(), 5),
                file: rel.clone(),
                line: 0,
                col: None,
            });
        }
    }
    if report.num_passed_tests.is_some() || report.num_failed_tests.is_some() {
        let (p, f) = (report.num_passed_tests.unwrap_or(0), report.num_failed_tests.unwrap_or(0));
        out.push(Diag {
            severity: Severity::Note,
            code: None,
            message: format!("{p} passed, {f} failed"),
            file: String::new(),
            line: 0,
            col: None,
        });
    }
    out
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
        let diags = parse(ParserKind::Tsc, colored, "", Path::new("."));
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

    // ── cargo test ──────────────────────────────────────────────────────

    const CARGO_TEST_FAIL: &str = "running 2 tests\ntest tests::adds ... FAILED\ntest tests::subs ... FAILED\n\nfailures:\n\n---- tests::adds stdout ----\nthread 'tests::adds' panicked at src/math.rs:12:9:\nassertion `left == right` failed\n  left: 4\n right: 5\n\n---- tests::subs stdout ----\nthread 'tests::subs' panicked at src/math.rs:20:5:\ncalled `Option::unwrap()` on a `None` value\n\nfailures:\n    tests::adds\n    tests::subs\n\ntest result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n";

    #[test]
    fn cargo_test_failures_with_panic_locations_and_tail() {
        let diags = parse_cargo_test(CARGO_TEST_FAIL, "");
        assert_eq!(diags.len(), 3, "2 failures + tail Note: {diags:?}");
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].file, "src/math.rs");
        assert_eq!(diags[0].line, 12);
        assert_eq!(diags[0].col, Some(9));
        assert!(diags[0].message.contains("panicked at"), "block folded in: {}", diags[0].message);
        assert_eq!(diags[1].severity, Severity::Error);
        assert_eq!(diags[1].file, "src/math.rs");
        assert_eq!(diags[1].line, 20);
        assert_eq!(diags[2].severity, Severity::Note);
        assert!(diags[2].message.contains("2 failed"));
    }

    const CARGO_TEST_PASS: &str = "running 3 tests\ntest tests::a ... ok\ntest tests::b ... ok\ntest tests::c ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";

    #[test]
    fn cargo_test_clean_run_yields_counts_note_only() {
        let diags = parse_cargo_test(CARGO_TEST_PASS, "");
        assert_eq!(diags.len(), 1, "clean run ⇒ just the counts Note: {diags:?}");
        assert_eq!(diags[0].severity, Severity::Note);
        assert!(diags[0].message.contains("3 passed"));
        assert!(diags[0].message.contains("test result: ok"));
    }

    const CARGO_COMPILE_ERROR: &str = "   Compiling foo v0.1.0 (/repo)\nerror[E0308]: mismatched types\n  --> src/main.rs:10:5\n   |\n10 |     let x: u32 = \"hello\";\n   |                  ^^^^^^^ expected `u32`, found `&str`\n\nerror: aborting due to 1 previous error\n";

    #[test]
    fn cargo_test_compile_error_before_tests_surfaces_rustc_error() {
        let diags = parse_cargo_test(CARGO_COMPILE_ERROR, "");
        let e0308 = diags.iter().find(|d| d.code.as_deref() == Some("E0308")).expect("E0308 surfaced");
        assert_eq!(e0308.severity, Severity::Error);
        assert_eq!(e0308.file, "src/main.rs");
        assert_eq!(e0308.line, 10);
        assert_eq!(e0308.col, Some(5));
        assert!(e0308.message.contains("mismatched types"));
    }

    #[test]
    fn cargo_test_stdout_block_truncated_to_15_lines() {
        let mut s = String::from(
            "test tests::big ... FAILED\n\nfailures:\n\n---- tests::big stdout ----\nthread 'tests::big' panicked at src/x.rs:1:1:\n",
        );
        for i in 0..40 {
            s.push_str(&format!("line {i}\n"));
        }
        s.push_str("\ntest result: FAILED. 0 passed; 1 failed;\n");
        let diags = parse_cargo_test(&s, "");
        let fail = &diags[0];
        assert_eq!(fail.severity, Severity::Error);
        assert!(fail.message.lines().count() <= 15, "block truncated: {}", fail.message.lines().count());
        assert_eq!(fail.file, "src/x.rs");
    }

    #[test]
    fn cargo_test_old_multiline_panic_form_resolves_location() {
        // Pre-1.73 form: `panicked at 'msg…', file:line:col` with the location
        // on the block's tail line, not the `panicked at` line.
        let out = "test tests::t ... FAILED\n\nfailures:\n\n---- tests::t stdout ----\nthread 'tests::t' panicked at 'assertion failed: `(left == right)`\n  left: `1`,\n right: `2`', src/helper.rs:5:5\n\ntest result: FAILED. 0 passed; 1 failed;\n";
        let diags = parse_cargo_test(out, "");
        assert_eq!(diags[0].file, "src/helper.rs");
        assert_eq!(diags[0].line, 5);
        assert_eq!(diags[0].col, Some(5));
    }

    // ── jest / vitest ───────────────────────────────────────────────────

    #[test]
    fn jest_json_failures_relativized_with_counts_note() {
        // Build the report with a real ESC byte in the failure message, then
        // serialize it (serde escapes the ESC as a JSON unicode escape, i.e.
        // valid JSON) so the parser has to strip the embedded color back out.
        let msg = "\u{1b}[31mError: expected 2 to equal 3\u{1b}[0m\n    at Object.<anonymous> (/repo/src/math.test.js:10:20)\n    at line3\n    at line4\n    at line5\n    at line6";
        let doc = serde_json::json!({
            "numPassedTests": 3,
            "numFailedTests": 1,
            "testResults": [{
                "testFilePath": "/repo/src/math.test.js",
                "assertionResults": [
                    {"status": "passed", "title": "adds"},
                    {"status": "failed", "title": "subtracts", "failureMessages": [msg]}
                ]
            }]
        })
        .to_string();
        let diags = parse_jest_json(&doc, Path::new("/repo"));
        assert_eq!(diags.len(), 2, "1 failure + counts Note: {diags:?}");
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].file, "src/math.test.js");
        assert!(!diags[0].message.contains('\u{1b}'), "ANSI stripped: {:?}", diags[0].message);
        assert!(diags[0].message.contains("expected 2 to equal 3"));
        assert!(diags[0].message.lines().count() <= 5, "capped to ~5 lines");
        assert_eq!(diags[1].severity, Severity::Note);
        assert!(diags[1].message.contains("3 passed"));
        assert!(diags[1].message.contains("1 failed"));
    }

    #[test]
    fn jest_json_malformed_yields_empty() {
        assert!(parse_jest_json("not json", Path::new("/repo")).is_empty());
    }
}
