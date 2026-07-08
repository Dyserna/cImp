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
//! TODO(spike E1): confirm that a PreToolUse deny's `permissionDecisionReason`
//! is surfaced **to the model** (not only the user) on the pinned Claude Code
//! version. If it is not, the advisor can't substitute content and the phase is
//! cancelled per the milestone — a bare deny is worse than nothing.
//!
//! Synchronous and dependency-light like the sibling shims; fails open (prints
//! nothing) so it can never wrongly block a legitimate read.

use std::io::{Read, Write};

use crate::context_hook::post_loopback;

pub fn run() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let v: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };
    // Only advise on Read (the matcher should already scope this, but be safe).
    if v.get("tool_name").and_then(|t| t.as_str()) != Some("Read") {
        return;
    }
    let tool_input = v.get("tool_input").cloned().unwrap_or(serde_json::Value::Null);
    let file_path = tool_input.get("file_path").and_then(|p| p.as_str()).unwrap_or("");
    if file_path.trim().is_empty() {
        return;
    }
    let offset = tool_input.get("offset").and_then(|o| o.as_u64());
    let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    let cwd = v
        .get("cwd")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();

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
