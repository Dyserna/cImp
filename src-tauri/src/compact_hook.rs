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
//! TODO(spike D0): the exact stdout field that reaches the *compaction prompt*
//! is unverified against the pinned Claude Code version. We emit the documented
//! `hookSpecificOutput.additionalContext` shape (mirroring the UserPromptSubmit
//! hook); the server-side side effects above happen regardless of how Claude
//! consumes stdout, so the feature degrades safely if this field is ignored.
//!
//! Dependency-light and synchronous, like `--context-hook`; prints nothing and
//! exits 0 on any error so it never blocks or perturbs a compaction.

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
    let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    // "manual" (a `/compact`) or "auto" (context-window pressure); passed through
    // for the record, not currently branched on.
    let trigger = v.get("trigger").and_then(|s| s.as_str()).unwrap_or("");
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
