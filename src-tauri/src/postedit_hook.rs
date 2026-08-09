//! V12 Phase F: the `cimp --postedit-hook` Claude Code `PostToolUse`
//! (matcher `Edit|Write|MultiEdit`) shim.
//!
//! Claude Code runs this right after an edit tool completes, with the hook
//! payload on stdin (`{tool_name, tool_input:{file_path, ...}, session_id,
//! cwd, ...}`). We POST to the app's loopback `/context/post_edit`, which
//! debounces the session's edits, runs the project's configured checks
//! single-flight per root, diffs the result against the session's own
//! baseline, and returns only NEW/worsened diagnostics (plus an optional
//! auto-impact blast-radius note) as `{ ok, text }`. We print that as the
//! hook's additional context. The server-side effects (baseline update,
//! parked-report bookkeeping) run whenever the route is reachable, even when
//! nothing is returned this call — the debounce/park mechanics are what make
//! that safe to call on every single edit.
//!
//! TODO(spike F0): the exact stdout field that reaches the model as
//! `PostToolUse` additional context is UNVERIFIED against the pinned Claude
//! Code version — same posture as V11's D0 (`PreCompact`) and E1
//! (`PreToolUse` deny reason) spikes, neither of which has been run yet
//! either. We emit the documented `hookSpecificOutput.additionalContext`
//! shape (mirroring `UserPromptSubmit`/`PreCompact`); if this field is
//! ignored on the pinned version, the feature degrades to parked-block-only
//! delivery (drained via `/context/retrieve`, already built — see
//! `graph::GraphService::drain_auto_check`) rather than failing outright, per
//! the milestone's F0 contingency.
//!
//! Dependency-light and synchronous, like the sibling shims; prints nothing
//! and exits 0 on any error so it never blocks or perturbs an edit.

use std::io::{Read, Write};

use crate::context_hook::{post_loopback, tab_arg};

pub fn run() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let v: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };
    let tool_name = v.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");
    // The matcher already scopes this to edit tools, but be safe.
    if !matches!(tool_name, "Edit" | "Write" | "MultiEdit") {
        return;
    }
    let tool_input = v
        .get("tool_input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let file_path = tool_input
        .get("file_path")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    let cwd = v
        .get("cwd")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    let body = serde_json::json!({
        "cwd": cwd,
        "session_id": session_id,
        "file_path": file_path,
        "tool_name": tool_name,
        // #48 (M-7): the identity the route's taint gate resolves a latch scope
        // from. This route EXECUTES the project's configured checks, so it is
        // the one the finding is really about — baked into argv at spawn, like
        // `--context-hook`'s. `null` without it, which the route admits (the
        // pre-#48 behaviour).
        "agent": "claude",
        "tab": tab_arg(&std::env::args().skip(1).collect::<Vec<_>>()),
    })
    .to_string();

    let Some(text) = post_loopback("/context/post_edit", &body) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": text,
        }
    });
    let _ = writeln!(std::io::stdout(), "{out}");
}
