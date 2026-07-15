//! Built-in [`ParserKind`] implementations — decode one checker's captured
//! stdout/stderr into a flat `Vec<Diag>` (dedup happens in `checks::run`).
//! Each parser is intentionally lenient: a line it can't parse is skipped,
//! never an error — a checker's output format drifting across versions
//! should degrade the diagnostic count, not break `run_check` outright.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
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
/// relativize (see [`parse_jest_json`]); every other arm ignores it. `pattern`
/// is only read by [`parse_regex_custom`] (the `regex-custom` escape hatch) —
/// it's `CheckDef::pattern`, threaded through from [`super::run`]; every other
/// arm ignores it.
pub fn parse(kind: ParserKind, stdout: &str, stderr: &str, cwd: &Path, pattern: Option<&str>) -> Vec<Diag> {
    match kind {
        ParserKind::CargoJson => parse_cargo_json(stdout),
        ParserKind::EslintJson => parse_eslint_json(stdout),
        ParserKind::Tsc => parse_tsc(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::Pytest => parse_pytest(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::CargoTest => parse_cargo_test(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::JestJson => parse_jest_json(stdout, cwd),
        // JSON / XML formats carry no top-level ANSI (see the note above).
        ParserKind::Sarif => parse_sarif(stdout, cwd),
        ParserKind::GoTestJson => parse_go_test_json(stdout),
        ParserKind::JunitXml => parse_junit_xml(stdout),
        ParserKind::Go => parse_go(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::Dotnet => parse_dotnet(&strip_ansi(stdout), &strip_ansi(stderr)),
        ParserKind::RegexCustom => parse_regex_custom(&strip_ansi(stdout), &strip_ansi(stderr), pattern),
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

/// Scan a JSON-lines stream (`cargo --message-format=json`, `go test -json`, …)
/// and yield each line that deserializes to `T`: trim the line, skip any that
/// doesn't start with `{`, and silently skip lines that don't parse — exactly
/// the lenient contract every parser here follows. Shared so that hardening the
/// scan (CRLF, a BOM-prefixed first line, a max-line-length guard) lands in one
/// place instead of being re-copied into each new JSON-lines parser.
fn json_lines<T: serde::de::DeserializeOwned>(stdout: &str) -> impl Iterator<Item = T> + '_ {
    stdout.lines().filter_map(|line| {
        let line = line.trim();
        if !line.starts_with('{') {
            return None;
        }
        serde_json::from_str::<T>(line).ok()
    })
}

/// One JSON object per line (`--message-format=json`); only
/// `reason == "compiler-message"` lines carry a diagnostic — build-script
/// output, artifact notifications, etc. are skipped. The primary span (the
/// one the compiler carets) wins over the first span when there's more than
/// one; a span-less message (e.g. "aborting due to N previous errors") gets
/// an empty location rather than being dropped.
fn parse_cargo_json(stdout: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    for msg in json_lines::<CargoMessage>(stdout) {
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

/// Blank-trim `block` (drop leading/trailing empty lines) and join at most
/// `max` of the surviving lines: the *head* when `from_end` is false, the
/// *tail* when true. Shared core behind [`truncate_lines`]/[`tail_lines`].
fn slice_lines(block: &[&str], max: usize, from_end: bool) -> String {
    let start = block.iter().position(|l| !l.trim().is_empty()).unwrap_or(block.len());
    let end = block.iter().rposition(|l| !l.trim().is_empty()).map(|i| i + 1).unwrap_or(start);
    let trimmed = &block[start..end];
    if from_end {
        trimmed[trimmed.len().saturating_sub(max)..].join("\n")
    } else {
        trimmed[..trimmed.len().min(max)].join("\n")
    }
}

/// Join the first `max` lines of a block (leading/trailing blank lines
/// trimmed) — the truncated context that becomes a failure's message.
fn truncate_lines(block: &[&str], max: usize) -> String {
    slice_lines(block, max, false)
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
pub(crate) fn relativize(cwd: &Path, abs: &str) -> String {
    let abs_fwd = abs.replace('\\', "/");
    let cwd_fwd = cwd.to_string_lossy().replace('\\', "/");
    let cwd_fwd = cwd_fwd.trim_end_matches('/');
    if !cwd_fwd.is_empty() {
        if let Some(rest) = abs_fwd.strip_prefix(cwd_fwd) {
            // Only a real child path: the byte after the prefix must be a
            // separator, else a sibling like `<cwd>-tests/x` would relativize.
            if rest.starts_with('/') {
                let rest = rest.trim_start_matches('/');
                if !rest.is_empty() {
                    return rest.to_string();
                }
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

// ── SARIF 2.1 (sarif) ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SarifLog {
    #[serde(default)]
    runs: Vec<SarifRun>,
}

#[derive(Deserialize)]
struct SarifRun {
    #[serde(default)]
    results: Vec<SarifResult>,
    /// The files the tool reported *scanning* — findings parsing ignores these;
    /// [`sarif_scanned_artifacts`] reads them for the audit coverage line.
    #[serde(default)]
    artifacts: Vec<SarifArtifact>,
}

#[derive(Deserialize)]
struct SarifArtifact {
    #[serde(default)]
    location: Option<SarifArtifactLocation>,
}

#[derive(Deserialize)]
struct SarifResult {
    #[serde(default, rename = "ruleId")]
    rule_id: Option<String>,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    message: SarifMessage,
    #[serde(default)]
    locations: Vec<SarifLocation>,
}

#[derive(Deserialize, Default)]
struct SarifMessage {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct SarifLocation {
    #[serde(default, rename = "physicalLocation")]
    physical_location: Option<SarifPhysicalLocation>,
}

#[derive(Deserialize)]
struct SarifPhysicalLocation {
    #[serde(default, rename = "artifactLocation")]
    artifact_location: Option<SarifArtifactLocation>,
    #[serde(default)]
    region: Option<SarifRegion>,
}

#[derive(Deserialize)]
struct SarifArtifactLocation {
    #[serde(default)]
    uri: String,
}

#[derive(Deserialize)]
struct SarifRegion {
    #[serde(default, rename = "startLine")]
    start_line: u32,
    #[serde(default, rename = "startColumn")]
    start_column: Option<u32>,
}

/// Turn a SARIF `artifactLocation.uri` into a report path, honouring the RFC
/// 8089 `file:` forms a producer can emit (dependency-free; no disk access), so
/// the resulting absolute path relativizes against the run `cwd` exactly like
/// the jest parser's `testFilePath`s. All output uses forward slashes, the
/// pipeline's canonical separator (`relativize`/`normalize_rel` both fold `\`
/// → `/`), so a Windows UNC root `\\server\share` matches a `//server/share/…`
/// path. Handled forms (`rest` is everything after `file://`):
///   * `file:///C:/repo/a.rs`  (empty authority, drive)   → `C:/repo/a.rs`
///   * `file:///repo/a.rs`     (empty authority, POSIX)    → `/repo/a.rs`
///   * `file:////server/share/a.rs` (empty authority, UNC) → `//server/share/a.rs`
///   * `file://server/share/a.rs`   (host authority, UNC)  → `//server/share/a.rs`
///   * `file://localhost/C:/a.rs`   (localhost ≡ no host)  → `C:/a.rs`
///
/// A uri with no `file://` scheme is already project-relative and passes
/// through unchanged. Byte-index-safe and total — it never panics.
pub(crate) fn sarif_uri_to_path(cwd: &Path, uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    // Split the authority (host) from the path: the authority is everything
    // before the first `/`. `file:///…` and `file:////…` start with `/`, so
    // their authority is empty (the whole `rest` is the path).
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        // `file://host` with no path component.
        None => (rest, ""),
    };
    // RFC 8089: an empty authority and `localhost` are equivalent.
    let host = if authority.eq_ignore_ascii_case("localhost") { "" } else { authority };
    let abs = if host.is_empty() {
        // Empty authority. `/C:/…` drops its leading slash to a drive path;
        // a POSIX `/repo/…` root or an empty-authority UNC `//server/share/…`
        // (path already starts `//`) is kept verbatim.
        if path.starts_with('/') && path.as_bytes().get(2) == Some(&b':') {
            path[1..].to_string()
        } else {
            path.to_string()
        }
    } else {
        // Non-empty host ⇒ UNC share `//host/share/…`.
        format!("//{host}{path}")
    };
    relativize(cwd, &abs)
}

/// Extract the files a SARIF report says it *scanned* (`runs[].artifacts[]
/// .location.uri`), project-relative and deduped (order-preserving). The V23
/// audit runner reads this for its scan-coverage line — findings parsing
/// ignores artifacts entirely. Paths normalize through the exact same
/// [`sarif_uri_to_path`]/[`relativize`] pair as findings paths, so coverage
/// and finding entries for the same file always spell it identically.
/// Best-effort: malformed / artifact-less SARIF yields an empty list, never an
/// error.
pub(crate) fn sarif_scanned_artifacts(sarif: &str, cwd: &Path) -> Vec<String> {
    let doc = sarif.trim_start_matches('\u{feff}').trim();
    let Ok(log) = serde_json::from_str::<SarifLog>(doc) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for run in &log.runs {
        for a in &run.artifacts {
            let uri = a.location.as_ref().map(|l| l.uri.as_str()).unwrap_or("");
            if uri.is_empty() {
                continue;
            }
            // `file://` forms normalize through the RFC 8089 path; a schemeless
            // uri (osv-scanner's usual `Cargo.lock`, or a bare absolute path)
            // just gets separator-folding + root-stripping.
            let rel = if uri.starts_with("file://") {
                sarif_uri_to_path(cwd, uri)
            } else {
                relativize(cwd, uri)
            };
            if !rel.is_empty() && !out.contains(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

/// SARIF 2.1 JSON (`ruff --output-format sarif`, `clang-tidy`, `golangci-lint`,
/// `semgrep`, CodeQL, ...) — the modern-lint long tail in one parser. One
/// `Diag` per `runs[].results[]`: `level` → `Severity` (`error`→Error,
/// `note`/`none`→Note, `warning` **and a missing level** → Warning, per the
/// SARIF default), the first physical location's `artifactLocation.uri` +
/// `region.startLine/startColumn`, and `ruleId` → code. A location-less result
/// gets an empty location (the `tsc_global` posture) rather than being dropped.
/// Every optional field tolerates absence (serde defaults); a malformed or
/// truncated document ⇒ no diagnostics (the whole-document JSON posture).
fn parse_sarif(stdout: &str, cwd: &Path) -> Vec<Diag> {
    let doc = stdout.trim_start_matches('\u{feff}').trim();
    let Ok(log) = serde_json::from_str::<SarifLog>(doc) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for run in &log.runs {
        for r in &run.results {
            let severity = match r.level.as_deref() {
                Some("error") => Severity::Error,
                Some("note") | Some("none") => Severity::Note,
                // `warning`, or a missing level (SARIF's spec default), → Warning.
                _ => Severity::Warning,
            };
            let (file, line, col) = match r.locations.iter().find_map(|l| l.physical_location.as_ref()) {
                Some(p) => {
                    let uri = p.artifact_location.as_ref().map(|a| a.uri.as_str()).unwrap_or("");
                    let (line, col) = match &p.region {
                        Some(reg) => (reg.start_line, reg.start_column),
                        None => (0, None),
                    };
                    (sarif_uri_to_path(cwd, uri), line, col)
                }
                None => (String::new(), 0, None),
            };
            out.push(Diag { severity, code: r.rule_id.clone(), message: r.message.text.clone(), file, line, col });
        }
    }
    out
}

// ── go build / go vet (go) ────────────────────────────────────────────────

fn go_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `file:line[:col]: message`. The non-greedy `.+?` lets the `:line` anchor
    // win past a Windows drive letter's own colon (`C:\pkg\main.go:10:5: …`),
    // the `generic_gcc` trick; `col` is optional (`go build` sometimes omits it).
    RE.get_or_init(|| Regex::new(r"^(.+?):(\d+)(?::(\d+))?:\s+(.+)$").expect("valid regex"))
}

/// `go build` / `go vet` text: `file:line[:col]: message`. Go prints no
/// severity token on these lines, and Go shops gate merges on `vet`, so every
/// matched line is `Error` (spec decision 4). `# package/path` stanza headers
/// (which carry no location) are skipped, as is any non-matching line. The
/// all-digit-"file" (a `12:34:56` timestamp) and indented-decoration guards
/// mirror `generic_gcc`.
fn parse_go(stdout: &str, stderr: &str) -> Vec<Diag> {
    let re = go_re();
    stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            // `# example.com/foo` stanza headers carry no diagnostic location.
            if line.starts_with('#') {
                return None;
            }
            let caps = re.captures(line)?;
            let file = &caps[1];
            if file.bytes().all(|b| b.is_ascii_digit()) || file.starts_with(char::is_whitespace) {
                return None;
            }
            Some(Diag {
                severity: Severity::Error,
                code: None,
                message: caps[4].trim().to_string(),
                file: file.to_string(),
                line: caps[2].parse().ok()?,
                col: caps.get(3).and_then(|m| m.as_str().parse().ok()),
            })
        })
        .collect()
}

// ── go test -json (go-test-json) ──────────────────────────────────────────

#[derive(Deserialize)]
struct GoTestEvent {
    #[serde(default, rename = "Action")]
    action: String,
    #[serde(default, rename = "Package")]
    package: String,
    #[serde(default, rename = "Test")]
    test: Option<String>,
    #[serde(default, rename = "Output")]
    output: Option<String>,
}

/// Join the last `max` non-blank-trimmed lines of `block` — the *tail* of a Go
/// test's captured output, where `go test` prints the `--- FAIL` marker and the
/// assertion detail. Mirrors `truncate_lines`, but keeps the END (a Go
/// failure's signal is at the bottom, not the top).
fn tail_lines(block: &[&str], max: usize) -> String {
    slice_lines(block, max, true)
}

/// One failure `Diag` from a label + the collected `Output` chunks for its
/// `(Package, Test)` key: the label plus the output tail (~15 lines, the
/// cargo-test cap), or just `"<label> failed"` when no output was captured.
fn go_test_diag(label: &str, collected: Option<&Vec<String>>) -> Diag {
    let joined = collected.map(|v| v.concat()).unwrap_or_default();
    let lines: Vec<&str> = joined.lines().collect();
    let tail = tail_lines(&lines, 15);
    let message = if tail.is_empty() { format!("{label} failed") } else { format!("{label}\n{tail}") };
    Diag { severity: Severity::Error, code: None, message, file: String::new(), line: 0, col: None }
}

/// `go test -json` event stream (one JSON object per line: `Action`, `Package`,
/// `Test`, `Output`, `Elapsed`). `Output` events are collected per
/// `(Package, Test)` as the stream is read; each `Action == "fail"` with a
/// `Test` becomes one `Error` whose message is the test name plus its output
/// tail. A package-level `fail` (no `Test` — a build failure) surfaces the
/// package's own collected output, but only when no per-test failure was
/// already recorded for that package (so an ordinary test failure isn't
/// double-reported). A pass/fail counts `Note` folds in like `parse_cargo_test`
/// — but only once at least one event parsed, so garbage ⇒ zero diagnostics.
fn parse_go_test_json(stdout: &str) -> Vec<Diag> {
    let mut output: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut failed_pkgs: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let (mut passed, mut failed) = (0u32, 0u32);
    let mut saw_event = false;

    for ev in json_lines::<GoTestEvent>(stdout) {
        saw_event = true;
        let test_key = ev.test.clone().unwrap_or_default();
        if let Some(text) = &ev.output {
            output.entry((ev.package.clone(), test_key.clone())).or_default().push(text.clone());
        }
        match ev.action.as_str() {
            "pass" if ev.test.is_some() => passed += 1,
            "fail" => match &ev.test {
                Some(name) => {
                    failed += 1;
                    failed_pkgs.insert(ev.package.clone());
                    let collected = output.get(&(ev.package.clone(), name.clone()));
                    out.push(go_test_diag(name, collected));
                }
                // A build failure surfaces the package's output — unless a
                // per-test failure already reported for the same package.
                None if !failed_pkgs.contains(&ev.package) => {
                    let collected = output.get(&(ev.package.clone(), String::new()));
                    out.push(go_test_diag(&format!("package {}", ev.package), collected));
                }
                None => {}
            },
            _ => {}
        }
    }

    if saw_event {
        out.push(Diag {
            severity: Severity::Note,
            code: None,
            message: format!("{passed} passed, {failed} failed"),
            file: String::new(),
            line: 0,
            col: None,
        });
    }
    out
}

// ── dotnet build / MSBuild (dotnet) ───────────────────────────────────────

fn dotnet_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // MSBuild canonical: `file(line,col): error|warning CODE: message`. The
    // non-greedy `.+?` up to `(line,col)` tolerates a Windows drive-letter path;
    // the code token is `\w+` (`CS0103`, `MSB3202`, `FS0001`, ...).
    RE.get_or_init(|| Regex::new(r"^(.+?)\((\d+),(\d+)\): (error|warning) (\w+): (.+)$").expect("valid regex"))
}

fn dotnet_project_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // MSBuild appends the owning project in brackets: ` [C:\src\App.csproj]`.
    RE.get_or_init(|| Regex::new(r"\s*\[[^\]]*\.(?:csproj|fsproj|vbproj|proj|sln)\]\s*$").expect("valid regex"))
}

/// MSBuild canonical diagnostics from `dotnet build --nologo`:
/// `file(line,col): error|warning CODE: message` — one regex family covering
/// C#/F#/VB. MSBuild appends the owning project (` [C:\src\App.csproj]`) to
/// each line; it's stripped from the message. MSBuild also prints the same
/// diagnostic once per target it built through — those identical lines collapse
/// in `checks::run`'s `group`/dedup, so no dedup is done here.
fn parse_dotnet(stdout: &str, stderr: &str) -> Vec<Diag> {
    let re = dotnet_re();
    let suffix = dotnet_project_suffix_re();
    stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            let caps = re.captures(line.trim_end())?;
            let severity = if &caps[4] == "error" { Severity::Error } else { Severity::Warning };
            let message = suffix.replace(&caps[6], "").trim().to_string();
            if message.is_empty() {
                return None;
            }
            Some(Diag {
                severity,
                code: Some(caps[5].to_string()),
                message,
                file: caps[1].to_string(),
                line: caps[2].parse().ok()?,
                col: caps[3].parse().ok(),
            })
        })
        .collect()
}

// ── JUnit XML (junit-xml) ─────────────────────────────────────────────────

/// One `<testcase>`'s accumulated state while its element is open.
struct JunitCase {
    classname: String,
    name: String,
    file: String,
    line: u32,
    failed: bool,
    fail_message: Option<String>,
    fail_text: String,
}

/// The value of `e`'s attribute named `key` (local name, namespace-insensitive),
/// entity-unescaped. `None` when the attribute is absent.
fn junit_attr(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

/// `e`'s numeric attribute `key` (a `<testsuite>` count), defaulting to 0.
fn junit_num(e: &BytesStart, key: &[u8]) -> u64 {
    junit_attr(e, key).and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// Build the failure `Diag` for a failed `<testcase>`: `classname.name` (each
/// omitted when empty) plus the failure detail — the `<failure>`/`<error>`
/// `message` attribute, else the first non-blank line of its text content.
fn junit_diag(c: &JunitCase) -> Diag {
    let mut label = match (c.classname.is_empty(), c.name.is_empty()) {
        (false, false) => format!("{}.{}", c.classname, c.name),
        (true, false) => c.name.clone(),
        (false, true) => c.classname.clone(),
        (true, true) => "test".to_string(),
    };
    let detail = c
        .fail_message
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(str::to_string)
        .or_else(|| c.fail_text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string));
    if let Some(d) = detail {
        label.push_str(": ");
        label.push_str(&d);
    }
    Diag { severity: Severity::Error, code: None, message: label, file: c.file.clone(), line: c.line, col: None }
}

/// Streaming state for [`parse_junit_xml`]. Totals are summed across every
/// `<testsuite>` (a `<testsuites>` wrapper's own totals are a fallback used only
/// when no `<testsuite>` was seen — normally its children carry the real
/// counts).
#[derive(Default)]
struct JunitState {
    out: Vec<Diag>,
    suite_total: u64,
    suite_failures: u64,
    suite_errors: u64,
    wrapper: Option<(u64, u64, u64)>,
    saw_element: bool,
    saw_suite: bool,
    cur: Option<JunitCase>,
    in_failure: bool,
}

impl JunitState {
    fn open(&mut self, e: &BytesStart, empty: bool) {
        match e.local_name().as_ref() {
            b"testsuites" => {
                self.saw_element = true;
                self.wrapper = Some((junit_num(e, b"tests"), junit_num(e, b"failures"), junit_num(e, b"errors")));
            }
            b"testsuite" => {
                self.saw_element = true;
                self.saw_suite = true;
                self.suite_total += junit_num(e, b"tests");
                self.suite_failures += junit_num(e, b"failures");
                self.suite_errors += junit_num(e, b"errors");
            }
            b"testcase" => {
                // A self-closing `<testcase/>` is a passing test — nothing to
                // open (no `<failure>`/`<error>` child can follow it).
                if empty {
                    return;
                }
                self.cur = Some(JunitCase {
                    classname: junit_attr(e, b"classname").unwrap_or_default(),
                    name: junit_attr(e, b"name").unwrap_or_default(),
                    file: junit_attr(e, b"file").unwrap_or_default(),
                    line: junit_attr(e, b"line").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
                    failed: false,
                    fail_message: None,
                    fail_text: String::new(),
                });
            }
            b"failure" | b"error" => {
                if let Some(cur) = &mut self.cur {
                    cur.failed = true;
                    if cur.fail_message.is_none() {
                        cur.fail_message = junit_attr(e, b"message");
                    }
                }
                // A non-empty `<failure>…</failure>` has text to capture until
                // its `End`; a self-closing one carries all it has in `message`.
                if !empty {
                    self.in_failure = true;
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &[u8]) {
        match name {
            b"failure" | b"error" => self.in_failure = false,
            b"testcase" => {
                if let Some(c) = self.cur.take() {
                    if c.failed {
                        self.out.push(junit_diag(&c));
                    }
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        if self.in_failure {
            if let Some(cur) = &mut self.cur {
                cur.fail_text.push_str(t);
            }
        }
    }

    fn finish(mut self) -> Vec<Diag> {
        if self.saw_element {
            let (total, failures, errors) = if self.saw_suite {
                (self.suite_total, self.suite_failures, self.suite_errors)
            } else {
                self.wrapper.unwrap_or((0, 0, 0))
            };
            self.out.push(Diag {
                severity: Severity::Note,
                code: None,
                message: format!("{total} tests, {failures} failures, {errors} errors"),
                file: String::new(),
                line: 0,
                col: None,
            });
        }
        self.out
    }
}

/// A JUnit XML test report (Maven Surefire, Gradle, pytest `--junit-xml`,
/// PHPUnit, ...), normally read via Phase B's `report_file` (these runners
/// write XML to disk, not stdout). One failed `<testcase>` (a `<failure>` or
/// `<error>` child) ⇒ one `Error`; a passing (self-closing) testcase is
/// ignored. A counts `Note` folds in the `<testsuite>` `tests`/`failures`/
/// `errors` totals. Both a `<testsuites>` wrapper and a bare `<testsuite>` are
/// tolerated. Malformed XML ⇒ whatever parsed before the error — and, since no
/// `<testsuite>`/`<testcase>` element is seen in non-JUnit input, that's zero
/// diagnostics (the module's lenient posture).
fn parse_junit_xml(input: &str) -> Vec<Diag> {
    let mut reader = Reader::from_str(input);
    let mut st = JunitState::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => st.open(&e, false),
            Ok(Event::Empty(e)) => st.open(&e, true),
            Ok(Event::End(e)) => st.close(e.local_name().as_ref()),
            Ok(Event::Text(e)) => {
                if let Ok(t) = e.decode() {
                    st.text(&t);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    st.finish()
}

// ── regex-custom (the universal escape hatch) ─────────────────────────────

/// Validate a `regex-custom` pattern at settings-save time: it must compile and
/// declare the mandatory named capture groups `file`, `line`, and `message` — so
/// a bad pattern surfaces as a save/validation error, not a silent
/// zero-diagnostics run. The optional `col`/`severity` groups aren't required.
pub fn validate_pattern(pattern: &str) -> Result<(), String> {
    let re = Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;
    let names: Vec<&str> = re.capture_names().flatten().collect();
    for required in ["file", "line", "message"] {
        if !names.contains(&required) {
            return Err(format!("regex-custom pattern must define a named group `(?<{required}>…)`"));
        }
    }
    Ok(())
}

/// The universal escape hatch: apply a user-supplied regex (from
/// [`super::CheckDef::pattern`]) per line to stdout+stderr. Named groups: `file`
/// and `line` and `message` are required (a line missing any is skipped);
/// `col` is optional; `severity` maps case-insensitively (`warning`→Warning,
/// `note`→Note, anything else — including absence — → Error, per spec). A
/// missing pattern, or one that fails to compile (already rejected at save time
/// by [`validate_pattern`], but guarded here too), yields no diagnostics rather
/// than a panic.
fn parse_regex_custom(stdout: &str, stderr: &str, pattern: Option<&str>) -> Vec<Diag> {
    let Some(pat) = pattern else { return Vec::new() };
    let Ok(re) = Regex::new(pat) else { return Vec::new() };
    stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            let caps = re.captures(line)?;
            let file = caps.name("file")?.as_str();
            let line_no: u32 = caps.name("line")?.as_str().parse().ok()?;
            let message = caps.name("message")?.as_str().trim();
            if file.is_empty() || message.is_empty() {
                return None;
            }
            let severity = match caps.name("severity").map(|m| m.as_str().to_ascii_lowercase()).as_deref() {
                Some("warning") => Severity::Warning,
                Some("note") => Severity::Note,
                _ => Severity::Error,
            };
            Some(Diag {
                severity,
                code: None,
                message: message.to_string(),
                file: file.to_string(),
                line: line_no,
                col: caps.name("col").and_then(|m| m.as_str().parse().ok()),
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

    #[test]
    fn json_lines_yields_only_valid_typed_values() {
        #[derive(Deserialize, PartialEq, Debug)]
        struct Row {
            n: u32,
        }
        // Blank line, plain text, a `{`-leading line that isn't valid JSON, a
        // `{`-leading line that parses but lacks the field, and two good rows —
        // only the well-typed objects survive, in stream order.
        let input = "\nnot json\n  {\"n\":1}  \n{ oops\n{\"other\":true}\n{\"n\":2}\n";
        let rows: Vec<Row> = json_lines(input).collect();
        assert_eq!(rows, vec![Row { n: 1 }, Row { n: 2 }]);
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
        let diags = parse(ParserKind::Tsc, colored, "", Path::new("."), None);
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
    fn slice_lines_head_tail_and_blank_trim() {
        let block = ["", "a", "b", "c", "d", ""];
        // Head vs tail, both blank-trimmed and capped at `max`.
        assert_eq!(slice_lines(&block, 2, false), "a\nb");
        assert_eq!(slice_lines(&block, 2, true), "c\nd");
        assert_eq!(truncate_lines(&block, 2), "a\nb");
        assert_eq!(tail_lines(&block, 2), "c\nd");
        // Block shorter than max returns the whole trimmed block, either way.
        assert_eq!(slice_lines(&block, 99, false), "a\nb\nc\nd");
        assert_eq!(slice_lines(&block, 99, true), "a\nb\nc\nd");
        // All-blank block trims to empty (no panic on the empty slice).
        let blank = ["", "  ", "\t"];
        assert_eq!(slice_lines(&blank, 5, false), "");
        assert_eq!(slice_lines(&blank, 5, true), "");
        // max == 0 yields nothing from a non-empty block.
        assert_eq!(slice_lines(&block, 0, false), "");
        assert_eq!(slice_lines(&block, 0, true), "");
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

    // ── SARIF ───────────────────────────────────────────────────────────

    const SARIF: &str = r#"{
      "version": "2.1.0",
      "runs": [
        {
          "results": [
            {"ruleId":"E501","level":"error","message":{"text":"line too long"},
             "locations":[{"physicalLocation":{"artifactLocation":{"uri":"src/app.py"},"region":{"startLine":10,"startColumn":80}}}]},
            {"ruleId":"W605","message":{"text":"invalid escape sequence"},
             "locations":[{"physicalLocation":{"artifactLocation":{"uri":"file:///repo/src/util.py"},"region":{"startLine":3}}}]},
            {"ruleId":"INFO1","level":"note","message":{"text":"informational only"},"locations":[]}
          ]
        }
      ]
    }"#;

    #[test]
    fn sarif_maps_levels_locations_and_rule_ids() {
        let diags = parse_sarif(SARIF, Path::new("/repo"));
        assert_eq!(diags.len(), 3, "{diags:?}");
        // error level, relative uri, file:line:col + ruleId.
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("E501"));
        assert_eq!(diags[0].file, "src/app.py");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].col, Some(80));
        // Missing level defaults to Warning; `file://` uri relativized to cwd.
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[1].code.as_deref(), Some("W605"));
        assert_eq!(diags[1].file, "src/util.py");
        assert_eq!(diags[1].line, 3);
        assert_eq!(diags[1].col, None);
        // `note` level, and a location-less result ⇒ empty location.
        assert_eq!(diags[2].severity, Severity::Note);
        assert_eq!(diags[2].file, "");
        assert_eq!(diags[2].line, 0);
    }

    #[test]
    fn sarif_malformed_yields_empty() {
        assert!(parse_sarif("not json at all", Path::new("/repo")).is_empty());
        // A valid-but-empty envelope also yields nothing (no runs/results).
        assert!(parse_sarif(r#"{"version":"2.1.0","runs":[]}"#, Path::new("/repo")).is_empty());
    }

    /// A sibling path that merely shares the cwd's name prefix must not be
    /// relativized: cwd `/proj/app` + `/proj/app-tests/x` stays absolute.
    #[test]
    fn relativize_requires_separator_after_prefix() {
        assert_eq!(
            relativize(Path::new("/proj/app"), "/proj/app-tests/Cargo.lock"),
            "/proj/app-tests/Cargo.lock"
        );
        // A real child still relativizes; the cwd itself passes through whole.
        assert_eq!(relativize(Path::new("/proj/app"), "/proj/app/Cargo.lock"), "Cargo.lock");
        assert_eq!(relativize(Path::new("/proj/app"), "/proj/app"), "/proj/app");
    }

    #[test]
    fn sarif_uri_handles_rfc8089_forms() {
        // Existing empty-authority forms still relativize against cwd.
        assert_eq!(
            sarif_uri_to_path(Path::new("/repo"), "file:///repo/src/util.py"),
            "src/util.py",
            "POSIX-root file uri"
        );
        assert_eq!(
            sarif_uri_to_path(Path::new(r"c:\repo"), "file:///c:/repo/a.rs"),
            "a.rs",
            "drive-letter file uri"
        );
        // A relative (schemeless) uri passes through untouched.
        assert_eq!(sarif_uri_to_path(Path::new("/repo"), "src/app.py"), "src/app.py");

        // 4-slash / empty-authority UNC → `//server/share/…`, matched against a
        // UNC cwd (`\\server\share` folds to `//server/share`).
        assert_eq!(
            sarif_uri_to_path(Path::new(r"\\server\share"), "file:////server/share/src/x.rs"),
            "src/x.rs",
            "empty-authority UNC file uri"
        );
        // 2-slash host-authority UNC → same canonical `//server/share/…`.
        assert_eq!(
            sarif_uri_to_path(Path::new(r"\\server\share"), "file://server/share/src/x.rs"),
            "src/x.rs",
            "host-authority UNC file uri"
        );
        // `localhost` authority ≡ empty authority ⇒ drive-letter path.
        assert_eq!(
            sarif_uri_to_path(Path::new(r"C:\repo"), "file://localhost/C:/repo/a.rs"),
            "a.rs",
            "localhost + drive file uri"
        );

        // Non-matching absolute path (UNC share vs POSIX cwd — different
        // volumes) is KEPT best-effort, never dropped.
        assert_eq!(
            sarif_uri_to_path(Path::new("/repo"), "file:////server/share/x.rs"),
            "//server/share/x.rs",
            "non-matching UNC kept as absolute"
        );

        // Total on degenerate inputs — no panic, no bad byte-indexing.
        assert_eq!(sarif_uri_to_path(Path::new("/repo"), "file://"), "");
        assert_eq!(sarif_uri_to_path(Path::new("/repo"), "file://host"), "//host");
    }

    // ── go build / go vet ───────────────────────────────────────────────

    const GO_OUTPUT: &str = "# example.com/foo\n./main.go:10:6: undefined: bar\nmain.go:20: missing return at end of function\nok  \texample.com/foo\t0.02s\n";

    #[test]
    fn go_parses_file_line_col_and_skips_stanza_headers() {
        let diags = parse_go(GO_OUTPUT, "");
        assert_eq!(diags.len(), 2, "stanza header + `ok` line skipped: {diags:?}");
        assert_eq!(diags[0].file, "./main.go");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].col, Some(6));
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].message, "undefined: bar");
        // A col-less `go build` line still parses.
        assert_eq!(diags[1].line, 20);
        assert_eq!(diags[1].col, None);
        assert_eq!(diags[1].severity, Severity::Error, "severity-less go lines stay Error");
    }

    #[test]
    fn go_windows_drive_letter_path_parses() {
        let diags = parse_go(r"C:\pkg\main.go:10:5: undefined: fmt", "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, r"C:\pkg\main.go");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].col, Some(5));
    }

    #[test]
    fn go_garbage_input_yields_empty() {
        assert!(parse_go("Compiling...\nno diagnostics here\n", "").is_empty());
    }

    // ── go test -json ───────────────────────────────────────────────────

    const GO_TEST_JSON: &str = concat!(
        r#"{"Action":"run","Package":"example/pkg","Test":"TestAdd"}"#, "\n",
        r#"{"Action":"output","Package":"example/pkg","Test":"TestAdd","Output":"=== RUN   TestAdd\n"}"#, "\n",
        r#"{"Action":"pass","Package":"example/pkg","Test":"TestAdd","Elapsed":0}"#, "\n",
        r#"{"Action":"run","Package":"example/pkg","Test":"TestSub"}"#, "\n",
        r#"{"Action":"output","Package":"example/pkg","Test":"TestSub","Output":"    sub_test.go:12: got 1 want 2\n"}"#, "\n",
        r#"{"Action":"output","Package":"example/pkg","Test":"TestSub","Output":"--- FAIL: TestSub (0.00s)\n"}"#, "\n",
        r#"{"Action":"fail","Package":"example/pkg","Test":"TestSub","Elapsed":0}"#, "\n",
        r#"{"Action":"output","Package":"example/pkg","Output":"FAIL\texample/pkg\t0.01s\n"}"#, "\n",
        r#"{"Action":"fail","Package":"example/pkg","Elapsed":0.01}"#, "\n",
    );

    #[test]
    fn go_test_json_failed_test_with_output_tail_and_counts() {
        let diags = parse_go_test_json(GO_TEST_JSON);
        // One per-test failure (the package-level fail is suppressed) + counts Note.
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(diags[0].message.starts_with("TestSub"), "name leads: {:?}", diags[0].message);
        assert!(diags[0].message.contains("--- FAIL: TestSub"), "output tail folded in: {:?}", diags[0].message);
        assert_eq!(diags[1].severity, Severity::Note);
        assert_eq!(diags[1].message, "1 passed, 1 failed");
    }

    const GO_TEST_JSON_BUILD_FAIL: &str = concat!(
        r##"{"Action":"output","Package":"example/bad","Output":"# example/bad\n"}"##, "\n",
        r##"{"Action":"output","Package":"example/bad","Output":"./bad.go:5:2: undefined: foo\n"}"##, "\n",
        r##"{"Action":"fail","Package":"example/bad","Elapsed":0}"##, "\n",
    );

    #[test]
    fn go_test_json_package_build_failure_surfaces_output() {
        let diags = parse_go_test_json(GO_TEST_JSON_BUILD_FAIL);
        assert_eq!(diags.len(), 2, "package build failure + counts Note: {diags:?}");
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(diags[0].message.contains("undefined: foo"), "build error surfaced: {:?}", diags[0].message);
        assert_eq!(diags[1].message, "0 passed, 0 failed");
    }

    #[test]
    fn go_test_json_garbage_yields_empty() {
        // No parseable event ⇒ not even a counts Note.
        assert!(parse_go_test_json("not json\n{ broken\n").is_empty());
    }

    // ── dotnet / MSBuild ────────────────────────────────────────────────

    const DOTNET_OUTPUT: &str = "Program.cs(10,13): error CS0103: The name 'x' does not exist in the current context [C:\\proj\\App.csproj]\nProgram.cs(15,9): warning CS0219: The variable 'y' is assigned but its value is never used [C:\\proj\\App.csproj]\n";

    #[test]
    fn dotnet_parses_and_strips_project_suffix() {
        let diags = parse_dotnet(DOTNET_OUTPUT, "");
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("CS0103"));
        assert_eq!(diags[0].file, "Program.cs");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].col, Some(13));
        assert!(diags[0].message.ends_with("current context"), "project suffix stripped: {:?}", diags[0].message);
        assert!(!diags[0].message.contains(".csproj"), "no leftover suffix: {:?}", diags[0].message);
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[1].code.as_deref(), Some("CS0219"));
    }

    #[test]
    fn dotnet_garbage_input_yields_empty() {
        assert!(parse_dotnet("Build succeeded.\n    0 Warning(s)\n", "").is_empty());
    }

    // ── junit-xml ───────────────────────────────────────────────────────

    const JUNIT_BARE: &str = r#"<testsuite name="suite" tests="3" failures="1" errors="1">
      <testcase classname="pkg.Foo" name="test_ok" time="0.01"/>
      <testcase classname="pkg.Foo" name="test_bad" file="tests/foo.py" line="10">
        <failure message="assert 1 == 2">AssertionError: assert 1 == 2</failure>
      </testcase>
      <testcase classname="pkg.Bar" name="test_boom">
        <error message="RuntimeError: boom">Traceback (most recent call last): ...</error>
      </testcase>
    </testsuite>"#;

    #[test]
    fn junit_bare_testsuite_failure_and_error_children() {
        let diags = parse_junit_xml(JUNIT_BARE);
        assert_eq!(diags.len(), 3, "2 failed testcases + counts Note: {diags:?}");
        // <failure> child, with file/line attributes (pytest emits them).
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].file, "tests/foo.py");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].message, "pkg.Foo.test_bad: assert 1 == 2");
        // <error> child, no file/line attrs ⇒ empty location.
        assert_eq!(diags[1].message, "pkg.Bar.test_boom: RuntimeError: boom");
        assert_eq!(diags[1].file, "");
        assert_eq!(diags[1].line, 0);
        // Counts Note from the testsuite attributes.
        assert_eq!(diags[2].severity, Severity::Note);
        assert_eq!(diags[2].message, "3 tests, 1 failures, 1 errors");
    }

    const JUNIT_WRAPPED: &str = r#"<testsuites tests="2" failures="1" errors="0">
      <testsuite name="s1" tests="2" failures="1" errors="0">
        <testcase classname="C" name="a"/>
        <testcase classname="C" name="b"><failure message="nope, wrong value"/></testcase>
      </testsuite>
    </testsuites>"#;

    #[test]
    fn junit_testsuites_wrapper_and_self_closing_failure() {
        let diags = parse_junit_xml(JUNIT_WRAPPED);
        assert_eq!(diags.len(), 2, "one failure (self-closing) + counts Note: {diags:?}");
        assert_eq!(diags[0].severity, Severity::Error);
        // Self-closing `<failure message=.../>` — detail from the message attr.
        assert_eq!(diags[0].message, "C.b: nope, wrong value");
        assert_eq!(diags[1].severity, Severity::Note);
        assert_eq!(diags[1].message, "2 tests, 1 failures, 0 errors");
    }

    #[test]
    fn junit_non_junit_xml_yields_empty() {
        assert!(parse_junit_xml("not xml at all").is_empty());
        assert!(parse_junit_xml("<html><body>hi</body></html>").is_empty());
    }

    // ── regex-custom ────────────────────────────────────────────────────

    #[test]
    fn regex_custom_severity_mapping_and_default_error() {
        let pat = r"^(?<file>[^:\s]+):(?<line>\d+): (?<severity>\w+): (?<message>.+)$";
        let input = "tool.py:10: warning: something suspicious\ntool.py:20: error: it broke\ntool.py:30: bananas: unknown severity word\n";
        let diags = parse_regex_custom(input, "", Some(pat));
        assert_eq!(diags.len(), 3, "{diags:?}");
        assert_eq!(diags[0].severity, Severity::Warning);
        assert_eq!(diags[0].file, "tool.py");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].col, None);
        assert_eq!(diags[1].severity, Severity::Error);
        // An unrecognized severity word defaults to Error.
        assert_eq!(diags[2].severity, Severity::Error);
        assert_eq!(diags[2].message, "unknown severity word");
    }

    #[test]
    fn regex_custom_optional_col_and_no_pattern() {
        let pat = r"^(?<file>\S+):(?<line>\d+):(?<col>\d+): (?<message>.+)$";
        let diags = parse_regex_custom("a.rs:3:7: boom\n", "", Some(pat));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].col, Some(7));
        assert_eq!(diags[0].severity, Severity::Error, "absent severity group ⇒ Error");
        // No pattern ⇒ no diagnostics (and no panic).
        assert!(parse_regex_custom("a.rs:3:7: boom\n", "", None).is_empty());
        // Non-matching input ⇒ empty.
        assert!(parse_regex_custom("nothing here matches\n", "", Some(pat)).is_empty());
    }

    #[test]
    fn validate_pattern_requires_groups_and_valid_regex() {
        assert!(validate_pattern(r"^(?<file>\S+):(?<line>\d+): (?<message>.+)$").is_ok());
        // Missing `line`.
        assert!(validate_pattern(r"^(?<file>\S+): (?<message>.+)$").is_err());
        // Missing `message`.
        assert!(validate_pattern(r"^(?<file>\S+):(?<line>\d+)$").is_err());
        // Uncompilable regex.
        assert!(validate_pattern(r"(?<file>[").is_err());
    }
}
