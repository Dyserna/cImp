//! V11 Phase E: the `cimp --read-hook` Claude Code `PreToolUse` (matcher `Read`)
//! redundant-read advisor shim.
//!
//! Claude Code runs this before a `Read`, with the hook payload on stdin
//! (`{tool_name, tool_input:{file_path, offset?}, session_id, cwd, …}`). We POST
//! to the app's loopback `/context/should_read`. The route answers `pass`
//! (returns no `text`) or `remind` (returns the reminder `text` — the file's
//! outline, plus its body in substitute mode). On `remind` we deny the Read with
//! that text as the reason, so the agent gets usable content instead of
//! re-reading a large unchanged file. On `pass`, any error, or a non-Read tool
//! we print nothing and the Read proceeds.
//!
//! Spike E1 (V16 Feature 0): whether a PreToolUse deny's
//! `permissionDecisionReason` is surfaced **to the model** (not only the
//! user) — recipe in `docs/MAINTENANCE.md` → harness contracts; outcome
//! recorded in `harness_versions.e1_status` (global settings). `"fail"`
//! hard-blocks the advisor (toggle disabled, hook not installed — see
//! `tabs/config.rs`), and the `drift.read_reason.v1` Advisor canary watches
//! for the symptom (~100% remind→immediate re-read) at runtime either way.
//!
//! Synchronous and dependency-light like the sibling shims; fails open (prints
//! nothing) so it can never wrongly block a legitimate read.

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
    let tool_name = v.get("tool_name").and_then(|t| t.as_str());
    let tool_input = v.get("tool_input").cloned().unwrap_or(serde_json::Value::Null);
    let file_path = tool_input.get("file_path").and_then(|p| p.as_str()).unwrap_or("");
    let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    let cwd_raw = v.get("cwd").and_then(|s| s.as_str()).unwrap_or("");
    // V16 Feature 3: payload-shape drift report, before every early return.
    // A DIFFERENT tool_name is a matcher-config matter, not payload drift —
    // only its ABSENCE (and `tool_input.file_path`'s, when the tool is Read
    // or unknown) counts.
    report_contract_drift(
        "read_hook",
        &missing_fields(&[
            ("session_id", !session_id.is_empty()),
            ("cwd", !cwd_raw.is_empty()),
            ("tool_name", tool_name.is_some()),
            ("tool_input.file_path", tool_name.is_some_and(|t| t != "Read") || !file_path.is_empty()),
        ]),
        session_id,
    );
    // Only advise on Read (the matcher should already scope this, but be safe).
    if tool_name != Some("Read") {
        return;
    }
    if file_path.trim().is_empty() {
        return;
    }
    let offset = tool_input.get("offset").and_then(|o| o.as_u64());
    let cwd = resolve_cwd(cwd_raw);

    let body = serde_json::json!({
        "cwd": cwd,
        "session_id": session_id,
        "file_path": file_path,
        "offset": offset,
    })
    .to_string();

    // `post_loopback` returns the response's `text` field, which the route only
    // sets for a `remind` verdict — so `Some(text)` means "deny with content",
    // and `None` (pass / error) means "let the read proceed".
    let Some(text) = post_loopback("/context/should_read", &body) else { return };
    if text.trim().is_empty() {
        return;
    }
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": text,
        }
    });
    let _ = writeln!(std::io::stdout(), "{out}");
}
