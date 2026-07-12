//! V11 Phase D: the `cimp --precompact-hook` Claude Code `PreCompact` shim.
//!
//! Claude Code runs this just before it compacts the transcript, with the hook
//! payload on stdin (`{session_id, trigger, cwd, …}`). We POST to the app's
//! loopback `/context/compaction`, which (a) always clears the session's
//! injection-dedup state and marks it post-compaction — side effects that make
//! the dedup (Phase C) and read-advisor (Phase E) correct across a compaction —
//! and (b) returns a compact block (ranked working set + pinned notes) to carry
//! through the summary. We print that block as the hook's additional context.
//!
//! Spike D0 (V16 Feature 0): whether the emitted
//! `hookSpecificOutput.additionalContext` reaches the *compaction prompt* —
//! recipe in `docs/MAINTENANCE.md` → harness contracts; outcome recorded in
//! `harness_versions.d0_status` (global settings, informational). The
//! server-side side effects above happen regardless of how Claude consumes
//! stdout, so the feature degrades safely if this field is ignored.
//!
//! Dependency-light and synchronous, like `--context-hook`; prints nothing and
//! exits 0 on any error so it never blocks or perturbs a compaction.

use std::io::{Read, Write};

use crate::context_hook::{missing_fields, post_loopback, report_contract_drift, resolve_cwd};

pub fn run() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let v: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };
    let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    // "manual" (a `/compact`) or "auto" (context-window pressure); forwarded to
    // the route but not currently acted on (it's ignored server-side today).
    let trigger = v.get("trigger").and_then(|s| s.as_str()).unwrap_or("");
    let cwd_raw = v.get("cwd").and_then(|s| s.as_str()).unwrap_or("");
    // V16 Feature 3: payload-shape drift report (fail-open unchanged).
    report_contract_drift(
        "compact_hook",
        &missing_fields(&[("session_id", !session_id.is_empty()), ("cwd", !cwd_raw.is_empty())]),
        session_id,
    );
    let cwd = resolve_cwd(cwd_raw);

    let body = serde_json::json!({
        "cwd": cwd,
        "session_id": session_id,
        "trigger": trigger,
    })
    .to_string();

    // The POST runs the server-side side effects even when the returned block is
    // empty (or the feature's block is gated off), so we call it unconditionally.
    let Some(text) = post_loopback("/context/compaction", &body) else { return };
    if text.trim().is_empty() {
        return;
    }
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreCompact",
            "additionalContext": text,
        }
    });
    let _ = writeln!(std::io::stdout(), "{out}");
}
