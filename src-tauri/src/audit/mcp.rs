//! V26 — the **code-audit MCP surface**: the shared tool descriptors, the pure
//! result formatter, the single app-side entry, and the `cimp --code-audit-mcp`
//! stdio child.
//!
//! The whole point of this module is **decoupling**. The MCP boundary exposes
//! exactly two zero-argument tools — `security_audit` and `quality_audit` — and
//! the only things that ever cross it are a [`Category`] string (out) and a
//! formatted text report (back). Which concrete scanners run, how they're
//! configured, how their output is parsed — all of that stays behind
//! [`AuditState::run_scan_and_wait`]. Adding or removing an audit tool touches
//! only `adapters.rs` / `schema.rs`; it must never touch this file, and the tool
//! *descriptions* here deliberately name **no** underlying binary (a consumer
//! model shouldn't learn — or come to depend on — "gitleaks" vs "semgrep"). Tool
//! ids appear only as opaque data strings inside the result text.
//!
//! Three consumers share this surface (modeled on how `offload/mcp.rs` serves
//! Claude, OpenCode, and the offload worker from one place, V8-01):
//!
//! - **Claude Code / OpenCode** spawn the [`run`] stdio child (`--code-audit-mcp`).
//!   Like the offload child it is a thin JSON-RPC bridge that proxies a
//!   `tools/call` to the running app's loopback (`POST /audit/run`) — the audit
//!   needs the app's live `AuditState`, so there is **no** headless fallback: an
//!   unreachable app becomes a clean tool error.
//! - **The offload worker** (in-process, Stage 4) reaches the state directly via
//!   the [`super::global`] handle and calls [`run_audit`] — no child process.
//!
//! Both the child (via the loopback route, Stage 3) and the worker ultimately
//! call the same [`run_audit`], so a scan triggered by *any* consumer streams
//! live into the Code audit section (Tool Activity tab) exactly as a
//! UI-triggered scan does — and lands as a roll-up row in the Activities feed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex as TokioMutex;

use super::adapters::Category;
use super::runner::{AuditSnapshot, AuditState, ToolStatus};
use crate::checks::Severity;
use crate::mcp_stdio::tool_error;
use crate::offload::loopback::{parse_result_line, proxy_base_for};
use crate::settings::AuditToolId;

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "cimp-code-audit";

/// Cap the findings section at this many entries. A pathological repo can
/// surface thousands of lints; past this the report is truncated with an
/// explicit note stating the true totals (whichever of this and
/// [`MAX_RESULT_BYTES`] hits first).
const MAX_FINDINGS: usize = 300;

/// Byte budget for the whole result text (~64 KB). MCP results ride the model's
/// context window, so a huge audit must not blow it out; the truncation note
/// still states the real totals so nothing is silently hidden.
const MAX_RESULT_BYTES: usize = 64 * 1024;

// ── Shared tool surface ────────────────────────────────────────────────────

/// The two MCP tool descriptors — the single source of truth shared by the
/// stdio child's `tools/list` and the offload worker's `ToolDef`s (Stage 4), so
/// the wire contract can never drift between consumers.
///
/// Both tools are **zero-argument** (empty object schema, `additionalProperties:
/// false`): "run the security/quality audit" needs no parameters — the *what*
/// is entirely the project's own Code Audit configuration. Descriptions speak
/// only in capability classes and never name a binary (see the module doc).
pub fn tool_descriptors() -> Vec<Value> {
    vec![
        audit_tool(
            "security_audit",
            "Run this project's configured SECURITY audit and return the findings. \
             Covers secret detection (leaked credentials, API keys, tokens in the working \
             tree and git history), dependency vulnerability scanning (known CVEs / advisories \
             in the project's lockfiles and manifests), and security-focused static analysis. \
             Takes no arguments — it runs exactly the security scanners the Code Audit view is \
             configured to run for this project, so results match that view 1:1. This launches \
             real scanners against the whole repository and may take several minutes on a large \
             project. The result is a text report: a summary line, one status line per scanner, \
             then findings as `SEVERITY file:line [tool/code] message`.",
        ),
        audit_tool(
            "quality_audit",
            "Run this project's configured code-QUALITY audit and return the findings. \
             Covers linting, dead-code and unused-dependency detection, typo checking, and \
             style / static analysis — whichever quality checks apply to the languages detected \
             in this project. Takes no arguments — it runs exactly the quality checks the Code \
             Audit view auto-selects for this project's languages, so results match that view \
             1:1 (checks that don't apply to this project are reported as skipped). This launches \
             real tools against the whole repository and may take several minutes on a large \
             project. The result is a text report: a summary line, one status line per tool, \
             then findings as `SEVERITY file:line [tool/code] message`.",
        ),
    ]
}

/// One zero-argument tool descriptor. The empty, closed input schema mirrors how
/// `offload/mcp.rs` shapes its schemas (`type: object` + `properties`), minus any
/// params — an audit call carries no arguments.
fn audit_tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    })
}

// ── Pure result formatter ──────────────────────────────────────────────────

/// Render an [`AuditSnapshot`] into the free-text report a consumer model reads
/// back from a `security_audit` / `quality_audit` call. Pure (no I/O, no clock)
/// so it is unit-tested directly.
///
/// Only the tools of `category` are considered — a scan runs one category, but
/// filtering is defensive so the formatter is correct for any snapshot. Layout:
///
/// 1. A **summary line**: the category, the scan root, tool counts by outcome,
///    and finding counts by severity.
/// 2. One **status line per tool** in the category — `done` (N findings +
///    duration), `failed` (error), `not installed`, `misconfigured` (a
///    configured path that doesn't resolve), `skipped (not applicable)`,
///    or `disabled` (idle).
/// 3. The **findings**, errors first (then warnings, then notes; emit order
///    within each band), each as `SEVERITY file:line [tool/code] message`. Tool
///    ids appear here only as opaque data strings — never in a way the model is
///    meant to key off.
///
/// The findings section is capped at [`MAX_FINDINGS`] entries **and**
/// ~[`MAX_RESULT_BYTES`] (whichever hits first); past the cap an explicit
/// truncation note states the true totals so nothing is silently dropped.
pub fn format_result(snapshot: &AuditSnapshot, category: Category) -> String {
    let tools: Vec<&super::runner::ToolState> = snapshot
        .tools
        .iter()
        .filter(|t| t.category == category)
        .collect();

    // ── outcome tallies ──
    let mut done = 0usize;
    let mut failed = 0usize;
    let mut not_installed = 0usize;
    let mut misconfigured = 0usize;
    let mut skipped = 0usize;
    let mut disabled = 0usize;
    for t in &tools {
        match t.status {
            ToolStatus::Done => done += 1,
            ToolStatus::Failed => failed += 1,
            ToolStatus::NotInstalled => not_installed += 1,
            ToolStatus::PathInvalid => misconfigured += 1,
            ToolStatus::SkippedNotApplicable => skipped += 1,
            ToolStatus::Idle => disabled += 1,
            // A completed snapshot shouldn't carry `Running`, but count it as
            // "running" rather than mislabel it.
            ToolStatus::Running => {}
        }
    }

    // ── findings, severity-tallied and bucketed errors-first ──
    // Three severity buckets instead of a full sort: O(n), inherently stable
    // (each tool's findings keep emit order within a band), and no ordering
    // work is spent on the thousands of entries a pathological repo can
    // surface past the render cap.
    let mut buckets: [Vec<(&AuditToolId, &crate::checks::Diag)>; 3] = Default::default();
    for t in &tools {
        for f in &t.findings {
            buckets[severity_rank(f.diag.severity) as usize].push((&f.tool, &f.diag));
        }
    }
    let (errors, warnings, notes) = (buckets[0].len(), buckets[1].len(), buckets[2].len());
    let total_findings = errors + warnings + notes;

    // Each tool's wire id, derived once (a serde round-trip) rather than once
    // per rendered finding.
    let id_strs: HashMap<AuditToolId, String> =
        tools.iter().map(|t| (t.id, tool_id_str(&t.id))).collect();

    let mut out = String::new();

    // 1) Summary line.
    out.push_str(&format!(
        "{} audit of {}: {} tool{} — {done} completed, {failed} failed, \
         {not_installed} not installed, {misconfigured} misconfigured, \
         {skipped} not applicable, {disabled} disabled. \
         Findings: {total_findings} ({errors} error{}, {warnings} warning{}, {notes} note{}).\n",
        category_title(category),
        snapshot.root,
        tools.len(),
        plural(tools.len()),
        plural(errors),
        plural(warnings),
        plural(notes),
    ));

    // 2) Per-tool status lines.
    if tools.is_empty() {
        out.push_str("\n(No tools configured for this category.)\n");
    } else {
        out.push('\n');
        for t in &tools {
            let wire = &id_strs[&t.id];
            out.push_str(&format!("  {wire} — {}\n", status_line(t)));
        }
    }

    // 3) Findings, capped.
    if total_findings == 0 {
        out.push_str("\nNo findings.\n");
        return out;
    }

    out.push_str("\nFindings:\n");
    let mut shown = 0usize;
    'render: for bucket in &buckets {
        for (tool, d) in bucket {
            let wire = id_strs
                .get(*tool)
                .cloned()
                .unwrap_or_else(|| tool_id_str(tool));
            let line = format_finding(&wire, d);
            // Cap on count OR bytes, whichever first — stop *before*
            // overshooting the byte budget so the note itself still fits.
            if shown >= MAX_FINDINGS || out.len() + line.len() > MAX_RESULT_BYTES {
                break 'render;
            }
            out.push_str(&line);
            shown += 1;
        }
    }
    if shown < total_findings {
        out.push_str(&format!(
            "\n… truncated: showing {shown} of {total_findings} findings \
             (cap: {MAX_FINDINGS} findings / {} KB).\n",
            MAX_RESULT_BYTES / 1024
        ));
    }
    out
}

/// One finding line: `SEVERITY file:line [tool/code] message`. `wire` is the
/// tool's kebab wire id, resolved by the caller (memoized per tool).
fn format_finding(wire: &str, d: &crate::checks::Diag) -> String {
    let sev = d.severity.as_str().to_ascii_uppercase();
    let tag = match &d.code {
        Some(code) if !code.is_empty() => format!("[{wire}/{code}]"),
        _ => format!("[{wire}]"),
    };
    format!("{sev} {}:{} {tag} {}\n", d.file, d.line, d.message.trim())
}

/// The status half of a per-tool line (the `id — <this>` tail).
fn status_line(t: &super::runner::ToolState) -> String {
    match t.status {
        ToolStatus::Done => format!(
            "done — {} finding{} in {}",
            t.findings.len(),
            plural(t.findings.len()),
            fmt_duration(t.duration_ms)
        ),
        ToolStatus::Failed => format!("failed — {}", t.error.as_deref().unwrap_or("unknown error")),
        ToolStatus::NotInstalled => {
            "not installed (no path configured, not on PATH/ebin)".to_string()
        }
        ToolStatus::PathInvalid => format!(
            "misconfigured — {}",
            t.error
                .as_deref()
                .unwrap_or("configured path not found — fix it in cImp Settings")
        ),
        ToolStatus::SkippedNotApplicable => "skipped (not applicable)".to_string(),
        ToolStatus::Idle => "disabled".to_string(),
        ToolStatus::Running => "running".to_string(),
    }
}

/// The tool's kebab **wire** id (`osv-scanner`, `semgrep-quality`, …) — distinct
/// from its binary `command_name` (two tools share `semgrep` / `dotnet`), so this
/// derives it from the serde representation to keep findings unambiguous.
fn tool_id_str(id: &AuditToolId) -> String {
    serde_json::to_value(id)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| id.command_name().to_string())
}

/// Errors first, then warnings, then notes.
fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Note => 2,
    }
}

/// `""`/`"s"` suffix for the given count.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Human-friendly duration: `"340ms"` under a second, else `"1.2s"`.
fn fmt_duration(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// Title-case category word for the summary line.
fn category_title(category: Category) -> &'static str {
    match category {
        Category::Security => "Security",
        Category::Quality => "Quality",
    }
}

// ── Single app-side entry ──────────────────────────────────────────────────

/// The one app-side entry both the loopback route (Stage 3) and the offload
/// worker's native tools (Stage 4) call: run a full scan of `category` to
/// completion and format its snapshot. Everything category-specific stays behind
/// [`AuditState::run_scan_and_wait`]; this only threads the result through
/// [`format_result`].
///
/// `source` names the agent that issued the call (`"claude"` / `"opencode"`
/// from the stdio child's `--consumer` flag, `"offload"` for the worker) and
/// is recorded on the roll-up activity entry below. Every agent-triggered
/// audit thus lands in the persistent tool-activity store (the Tool Activity
/// tab's Activities feed) as ONE `security_audit`/`quality_audit` row with
/// consumer attribution — alongside the per-scanner rows the runner records
/// (kind `audit`, source `"audit"`) for every scan, UI-triggered ones
/// included. Failures (busy runner, feature disabled) are recorded too, as
/// `ok:false` rows, so a refused agent call is visible.
pub async fn run_audit(
    state: &Arc<AuditState>,
    category: Category,
    source: &str,
) -> Result<String, String> {
    use crate::activity::{self, now_ms, ActivityEntry, ActivityKind, ActivityRecord};

    let started = now_ms();
    let outcome = state.run_scan_and_wait(category).await;
    let (result, findings, root) = match outcome {
        Ok(snapshot) => {
            let text = format_result(&snapshot, category);
            let root = snapshot.root.clone();
            (Ok(text), snapshot.total_findings, root)
        }
        // The scan never ran, so no snapshot came back — pull the root from
        // the runner's current state for the failure row.
        Err(e) => (Err(e), 0, state.snapshot().root),
    };

    let tool = match category {
        Category::Security => "security_audit",
        Category::Quality => "quality_audit",
    };
    activity::record_bg(ActivityRecord {
        entry: ActivityEntry::new(
            ActivityKind::Audit,
            started,
            activity::root_key(std::path::Path::new(&root)),
            source.to_string(),
            tool.to_string(),
            root,
            findings,
            now_ms().saturating_sub(started),
            result.is_ok(),
        ),
        request: format!("{tool} ({source})"),
        response: match &result {
            Ok(text) => text.clone(),
            Err(e) => format!("[error] {e}"),
        },
    });

    result
}

// ── The `cimp --code-audit-mcp` stdio child ────────────────────────────────

/// The consumer this child serves (`--consumer <name>`, default `"claude"`),
/// forwarded to the app on every `/audit/run` so the route re-enforces that
/// consumer's `expose_*` toggle at run time (`loopback::handle_audit_run`).
/// Set once at startup (mirrors `offload/mcp.rs`).
static CONSUMER: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The configured consumer name, lowercased; `"claude"` when unset.
fn consumer() -> &'static str {
    CONSUMER.get().map(String::as_str).unwrap_or("claude")
}

/// Entry point for `cimp --code-audit-mcp [--consumer <name>]`. Builds a
/// current-thread tokio runtime and drives the shared stdio JSON-RPC loop
/// ([`crate::mcp_stdio::serve`] — panic capture, shutdown-on-broken-stdout,
/// UTF-8 tolerance) until stdin closes. Much smaller than `offload/mcp.rs::run`
/// — no backends, no SSE relay, no headless fallback. Never panics: a crash
/// here would garble the host agent's MCP session.
pub fn run(consumer: &str) {
    let _ = CONSUMER.set(consumer.trim().to_ascii_lowercase());
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };
    rt.block_on(async {
        let stdout = Arc::new(TokioMutex::new(tokio::io::stdout()));
        crate::mcp_stdio::serve(stdout, "code-audit", handle_owned).await;
    });
}

/// Owned-argument wrapper around [`handle`] so it can run on its own spawned task
/// (a `'static` future) for panic capture.
async fn handle_owned(method: String, params: Value) -> Result<Value, (i64, String)> {
    handle(&method, params).await
}

/// Dispatch one JSON-RPC method. `Ok(result)` or `Err((code, message))` for a
/// JSON-RPC error object.
async fn handle(method: &str, params: Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_descriptors() })),
        "tools/call" => handle_tools_call(params).await,
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

/// Map a `tools/call` to its [`Category`] and drive the scan via the loopback.
async fn handle_tools_call(params: Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let category = match name {
        "security_audit" => Category::Security,
        "quality_audit" => Category::Quality,
        other => return Err((-32602, format!("unknown tool: {other}"))),
    };
    match run_via_loopback(category).await {
        Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
        // A "not running" / busy / disabled condition is a tool result (not a
        // protocol error) so the model can read it and adapt.
        Err(msg) => Ok(tool_error(&msg)),
    }
}

/// The one HTTP client for loopback calls — built lazily, reused across
/// `tools/call`s (a `reqwest::Client` is an `Arc`'d connection pool; rebuilding
/// its connector/TLS state per call is pure waste). The stored `Err` is
/// returned to every caller if the one-time build ever fails.
static HTTP: std::sync::OnceLock<Result<reqwest::Client, String>> = std::sync::OnceLock::new();

/// Short connect timeout to fast-detect "app not listening". No overall
/// request timeout: once connected we wait out the (heartbeat-paced) scan and
/// rely on the per-line idle window to tell a slow-but-alive scan from a
/// wedged one.
fn http_client() -> Result<reqwest::Client, String> {
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))
    })
    .clone()
}

/// Drive one audit by POSTing to the app's loopback `/audit/run` and consuming
/// the streamed NDJSON reply.
///
/// **Contract with the Stage-3 loopback route:**
/// - Request body: `{"category": ..., "consumer": "<name>", "cwd": "<dir>"}`
///   (bearer-authenticated). The scan root is the app's own launch project;
///   `cwd` (this child's working dir — the agent's project) is sent for
///   VERIFICATION only: the route rejects a request whose cwd falls outside
///   its root, so a misrouted child (stale/foreign discovery entry) gets a
///   "wrong instance" error instead of a silently-wrong-project scan. The
///   endpoint itself is picked root-aware via `proxy_base_for`.
/// - Response: newline-delimited JSON. Any number of heartbeat lines (to keep a
///   minutes-long scan's connection alive) followed by exactly one final result
///   line — the loopback's `RunResult { ok, text, error }`. The **final line is
///   the only one carrying an `ok` boolean**; every other line is treated as a
///   heartbeat and skipped (so the exact heartbeat shape is not load-bearing).
/// - Idle window: [`IDLE`] between lines. The route heartbeats every ~10 s, so a
///   full idle gap means the scan wedged — surfaced as a tool error, never
///   retried locally (there is no local audit path).
///
/// App not discoverable / not listening ⇒ the "cImp is not running" tool error.
async fn run_via_loopback(category: Category) -> Result<String, String> {
    let cwd = std::env::current_dir().ok();
    let Some((base, token)) = proxy_base_for(cwd.as_deref()) else {
        return Err("cImp is not running — start cImp to run code audits.".into());
    };
    let client = http_client()?;

    // `category` serializes through its own `#[serde(rename_all = "lowercase")]`
    // derive — the exact serde the route's `AuditRunBody` deserializes with, so
    // the two ends agree by construction (no hand-maintained wire words).
    let body = json!({
        "category": category,
        "consumer": consumer(),
        "cwd": cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
    });
    let mut resp = match client
        .post(format!("{base}/audit/run"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        // Could not even reach the app → treat as not running.
        Err(_) => return Err("cImp is not running — start cImp to run code audits.".into()),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        return Err(format!(
            "cImp returned {status}: {}",
            txt.chars().take(300).collect::<String>()
        ));
    }

    // Audits heartbeat every ~10 s; 45 s of silence means it's stuck.
    const IDLE: Duration = Duration::from_secs(45);
    let mut buf: Vec<u8> = Vec::new();
    let mut result: Option<Result<String, String>> = None;
    loop {
        match tokio::time::timeout(IDLE, resp.chunk()).await {
            Err(_) => {
                return Err(
                    "the code audit stopped responding (no heartbeat) — it may be stuck.".into(),
                )
            }
            Ok(Ok(Some(bytes))) => {
                buf.extend_from_slice(&bytes);
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=nl).collect();
                    if let Some(r) = parse_result_line(&raw, "code audit failed") {
                        result = Some(r);
                    }
                }
            }
            Ok(Ok(None)) => break, // EOF (connection closed)
            Ok(Err(e)) => return Err(format!("code audit stream error: {e}")),
        }
    }
    // A trailing unterminated final line (no closing newline).
    if result.is_none() && !buf.is_empty() {
        if let Some(r) = parse_result_line(&buf, "code audit failed") {
            result = Some(r);
        }
    }
    result.unwrap_or_else(|| Err("code audit ended without a result.".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::runner::{AuditFinding, ToolState};
    use crate::checks::{Diag, Severity};

    // ── descriptor shape ───────────────────────────────────────────────────

    #[test]
    fn descriptors_are_exactly_the_two_zero_arg_tools() {
        let d = tool_descriptors();
        assert_eq!(d.len(), 2, "exactly two audit tools");
        let names: Vec<&str> = d.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"security_audit"));
        assert!(names.contains(&"quality_audit"));
        for t in &d {
            let schema = &t["inputSchema"];
            assert_eq!(schema["type"], "object");
            // Zero params: empty `properties`, no `required`, closed object.
            assert_eq!(
                schema["properties"].as_object().unwrap().len(),
                0,
                "audit tools take no arguments"
            );
            assert!(t["inputSchema"].get("required").is_none());
            assert_eq!(schema["additionalProperties"], false);
        }
    }

    /// The decoupling guarantee: no description may name an underlying tool
    /// binary — a consumer must key off the category, never a scanner.
    #[test]
    fn descriptions_never_name_an_underlying_tool() {
        const BLACKLIST: &[&str] = &[
            "gitleaks", "semgrep", "osv", "oxlint", "eslint", "ruff", "cppcheck", "knip", "typos",
            "pmd", "golangci", "machete", "roslyn", "dotnet",
        ];
        for t in tool_descriptors() {
            let desc = t["description"].as_str().unwrap().to_ascii_lowercase();
            for bad in BLACKLIST {
                assert!(
                    !desc.contains(bad),
                    "description names underlying tool `{bad}`: {desc}"
                );
            }
            // And it must set expectations about duration.
            assert!(desc.contains("several minutes"));
        }
    }

    // ── format_result ──────────────────────────────────────────────────────

    fn finding(
        tool: AuditToolId,
        sev: Severity,
        code: &str,
        file: &str,
        line: u32,
        msg: &str,
    ) -> AuditFinding {
        AuditFinding {
            tool,
            diag: Diag {
                severity: sev,
                code: Some(code.to_string()),
                message: msg.to_string(),
                file: file.to_string(),
                line,
                col: None,
            },
        }
    }

    fn tool_state(
        id: AuditToolId,
        category: Category,
        status: ToolStatus,
        findings: Vec<AuditFinding>,
        duration_ms: u64,
        error: Option<&str>,
    ) -> ToolState {
        ToolState {
            id,
            category,
            status,
            findings,
            duration_ms,
            error: error.map(str::to_string),
            resolved: None,
            scanned_artifacts: Vec::new(),
        }
    }

    fn snapshot(tools: Vec<ToolState>) -> AuditSnapshot {
        let total_findings = tools.iter().map(|t| t.findings.len()).sum();
        AuditSnapshot {
            root: "/proj/root".to_string(),
            scanning: false,
            last_scan_at: Some(1),
            tools,
            census: Default::default(),
            total_findings,
            truncated: false,
        }
    }

    #[test]
    fn severity_counts_and_errors_first_ordering() {
        let t = tool_state(
            AuditToolId::Semgrep,
            Category::Security,
            ToolStatus::Done,
            vec![
                finding(
                    AuditToolId::Semgrep,
                    Severity::Warning,
                    "w1",
                    "b.rs",
                    2,
                    "warn one",
                ),
                finding(
                    AuditToolId::Semgrep,
                    Severity::Error,
                    "e1",
                    "a.rs",
                    1,
                    "err one",
                ),
                finding(
                    AuditToolId::Semgrep,
                    Severity::Note,
                    "n1",
                    "c.rs",
                    3,
                    "note one",
                ),
                finding(
                    AuditToolId::Semgrep,
                    Severity::Error,
                    "e2",
                    "a.rs",
                    5,
                    "err two",
                ),
            ],
            1200,
            None,
        );
        let out = format_result(&snapshot(vec![t]), Category::Security);

        // Summary counts.
        assert!(
            out.contains("Findings: 4 (2 errors, 1 warning, 1 note)"),
            "{out}"
        );
        // Errors-first: both ERROR lines precede the WARNING, which precedes NOTE.
        let e1 = out.find("err one").unwrap();
        let e2 = out.find("err two").unwrap();
        let w = out.find("warn one").unwrap();
        let n = out.find("note one").unwrap();
        assert!(e1 < w && e2 < w, "errors must sort before warnings");
        assert!(w < n, "warnings must sort before notes");
        // Finding line shape: `SEVERITY file:line [tool/code] message`.
        assert!(out.contains("ERROR a.rs:1 [semgrep/e1] err one"), "{out}");
        // Duration rendered on the status line.
        assert!(out.contains("done — 4 findings in 1.2s"), "{out}");
    }

    #[test]
    fn not_installed_and_skipped_render_as_status_lines() {
        let tools = vec![
            tool_state(
                AuditToolId::Gitleaks,
                Category::Security,
                ToolStatus::NotInstalled,
                vec![],
                0,
                None,
            ),
            tool_state(
                AuditToolId::Semgrep,
                Category::Security,
                ToolStatus::SkippedNotApplicable,
                vec![],
                0,
                None,
            ),
            tool_state(
                AuditToolId::OsvScanner,
                Category::Security,
                ToolStatus::Idle,
                vec![],
                0,
                None,
            ),
            tool_state(
                AuditToolId::Semgrep,
                Category::Security,
                ToolStatus::Failed,
                vec![],
                0,
                Some("network unreachable"),
            ),
        ];
        let out = format_result(&snapshot(tools), Category::Security);
        assert!(out.contains("gitleaks — not installed"), "{out}");
        assert!(out.contains("semgrep — skipped (not applicable)"), "{out}");
        assert!(out.contains("osv-scanner — disabled"), "{out}");
        assert!(out.contains("failed — network unreachable"), "{out}");
        // Outcome tally in the summary reflects all four.
        assert!(
            out.contains(
                "0 completed, 1 failed, 1 not installed, 0 misconfigured, \
                 1 not applicable, 1 disabled"
            ),
            "{out}"
        );
    }

    #[test]
    fn path_invalid_renders_as_misconfigured() {
        // A configured-but-broken path must read as a user-fixable
        // misconfiguration (with the offending path), not "not installed".
        let tools = vec![tool_state(
            AuditToolId::Gitleaks,
            Category::Security,
            ToolStatus::PathInvalid,
            vec![],
            0,
            Some("configured path not found: C:\\tools\\gitleaks.exe — fix it in Settings"),
        )];
        let out = format_result(&snapshot(tools), Category::Security);
        assert!(
            out.contains(
                "gitleaks — misconfigured — configured path not found: C:\\tools\\gitleaks.exe"
            ),
            "{out}"
        );
        assert!(out.contains("1 misconfigured"), "{out}");
    }

    #[test]
    fn empty_findings_is_a_clean_summary() {
        let t = tool_state(
            AuditToolId::Gitleaks,
            Category::Security,
            ToolStatus::Done,
            vec![],
            340,
            None,
        );
        let out = format_result(&snapshot(vec![t]), Category::Security);
        assert!(out.contains("Security audit of /proj/root"), "{out}");
        assert!(
            out.contains("Findings: 0 (0 errors, 0 warnings, 0 notes)"),
            "{out}"
        );
        assert!(out.contains("No findings."), "{out}");
        assert!(out.contains("done — 0 findings in 340ms"), "{out}");
        // No truncation note on a clean run.
        assert!(!out.contains("truncated"), "{out}");
    }

    #[test]
    fn past_the_cap_a_truncation_note_states_totals() {
        let findings: Vec<AuditFinding> = (0..(MAX_FINDINGS + 100))
            .map(|i| {
                finding(
                    AuditToolId::Semgrep,
                    Severity::Error,
                    "e",
                    "f.rs",
                    i as u32,
                    "boom",
                )
            })
            .collect();
        let total = findings.len();
        let t = tool_state(
            AuditToolId::Semgrep,
            Category::Security,
            ToolStatus::Done,
            findings,
            10,
            None,
        );
        let out = format_result(&snapshot(vec![t]), Category::Security);
        assert!(out.contains("truncated"), "{out}");
        assert!(out.contains(&format!("of {total} findings")), "{out}");
        // The rendered body must not exceed the byte budget (plus the note).
        assert!(
            out.len() <= MAX_RESULT_BYTES + 200,
            "over byte budget: {}",
            out.len()
        );
    }

    // ── loopback line parsing ──────────────────────────────────────────────

    /// Pins the audit child's use of the shared loopback line parser: any
    /// heartbeat shape and blanks are skipped, the single `ok`-bearing line is
    /// the result, and this child's fallback error text applies.
    #[test]
    fn parse_result_line_skips_heartbeats_and_reads_result() {
        let parse = |raw: &[u8]| parse_result_line(raw, "code audit failed");
        // Heartbeats (any shape lacking `ok`) and blanks are skipped.
        assert!(parse(b"{\"heartbeat\":true}\n").is_none());
        assert!(parse(b"{\"hb\":true}\n").is_none());
        assert!(parse(b"   \n").is_none());
        assert!(parse(b"not json").is_none());
        // Final result lines.
        assert_eq!(
            parse(b"{\"ok\":true,\"text\":\"report\"}\n"),
            Some(Ok("report".to_string()))
        );
        assert_eq!(
            parse(b"{\"ok\":false,\"error\":\"busy\"}"),
            Some(Err("busy".to_string()))
        );
        assert_eq!(
            parse(b"{\"ok\":false}"),
            Some(Err("code audit failed".to_string()))
        );
    }
}
