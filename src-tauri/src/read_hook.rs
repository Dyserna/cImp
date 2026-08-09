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

use crate::context_hook::{
    missing_fields, post_loopback, report_contract_drift, resolve_cwd, tab_arg,
};

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
    let tool_input = v
        .get("tool_input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let file_path = tool_input
        .get("file_path")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let command = tool_input
        .get("command")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    let cwd_raw = v.get("cwd").and_then(|s| s.as_str()).unwrap_or("");
    // V16 Feature 3 / V17 Phase B: payload-shape drift report, before every
    // early return. A DIFFERENT tool_name is a matcher-config matter, not
    // payload drift — only its ABSENCE (and `tool_input.file_path`'s when the
    // tool is Read/unknown, `tool_input.command`'s when the tool is Bash) counts.
    report_contract_drift(
        "read_hook",
        &missing_fields(&contract_checks(
            tool_name, session_id, cwd_raw, file_path, command,
        )),
        session_id,
    );

    let cwd = resolve_cwd(cwd_raw);
    // Map the payload to a verdict request, or let the tool proceed untouched
    // (non-target tool, empty path, or a Bash command that isn't a provable
    // whole-file read).
    let Some(reqst) = plan_request(tool_name, &tool_input, &cwd) else {
        return;
    };

    let body = serde_json::json!({
        "cwd": cwd,
        "session_id": session_id,
        "file_path": reqst.file_path,
        "offset": reqst.offset,
        "limit": reqst.limit,
        // #48 (M-7): the identity the route's taint gate resolves a latch scope
        // from — baked into argv at spawn, like `--context-hook`'s. `null`
        // without it, which the route admits (the pre-#48 behaviour).
        "agent": "claude",
        "tab": tab_arg(&std::env::args().skip(1).collect::<Vec<_>>()),
    })
    .to_string();

    // `post_loopback` returns the response's `text` field, which the route only
    // sets for a `remind` verdict — so `Some(text)` means "deny with content",
    // and `None` (pass / error) means "let the read proceed".
    let Some(text) = post_loopback("/context/should_read", &body) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    // The server verdict is tool-agnostic; the Bash path prepends its own
    // "answered without running the command — " note so the deny reads sensibly
    // for a shell read.
    let reason = format!("{}{text}", reqst.deny_prefix);
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    let _ = writeln!(std::io::stdout(), "{out}");
}

/// The verdict-request a hook payload maps to, before the loopback POST.
struct ReadRequest {
    /// The file to check — a `Read`'s `file_path` as given (Claude passes it
    /// absolute), or a Bash whole-file read's path absolutized against cwd.
    file_path: String,
    /// The `Read` offset (`None` for a shell read — it's always a full read).
    offset: Option<u64>,
    /// The `Read` limit (`None` for a shell read).
    limit: Option<u64>,
    /// Prepended to a remind reason: empty for `Read`, the shell note for `Bash`.
    deny_prefix: &'static str,
}

/// The shim-side note prepended to a `Bash` interception's deny reason.
const BASH_DENY_PREFIX: &str = "answered without running the command — ";

/// The `(field, present)` requiredness pairs for a hook payload (V16 Feature 3
/// drift reporting), tool-aware: `file_path` required for a `Read` (and for an
/// unknown tool — defensive), `command` for a `Bash`; the base fields always.
/// Split out so it's unit-testable without a socket.
fn contract_checks(
    tool_name: Option<&str>,
    session_id: &str,
    cwd_raw: &str,
    file_path: &str,
    command: &str,
) -> Vec<(&'static str, bool)> {
    vec![
        ("session_id", !session_id.is_empty()),
        ("cwd", !cwd_raw.is_empty()),
        ("tool_name", tool_name.is_some()),
        (
            "tool_input.file_path",
            tool_name.is_some_and(|t| t != "Read") || !file_path.is_empty(),
        ),
        (
            "tool_input.command",
            !matches!(tool_name, Some("Bash")) || !command.is_empty(),
        ),
    ]
}

/// Map a parsed hook payload to a [`ReadRequest`], or `None` when the shim
/// should let the tool proceed untouched. `cwd` is the already-resolved payload
/// cwd (used to absolutize a relative shell path). Same verdict body for both
/// tools — the only difference is the deny prefix — so a `Read` and an
/// equivalent `cat` get byte-identical advice modulo that prefix.
fn plan_request(
    tool_name: Option<&str>,
    tool_input: &serde_json::Value,
    cwd: &str,
) -> Option<ReadRequest> {
    match tool_name {
        Some("Read") => {
            let file_path = tool_input
                .get("file_path")
                .and_then(|p| p.as_str())
                .unwrap_or("");
            if file_path.trim().is_empty() {
                return None;
            }
            Some(ReadRequest {
                file_path: file_path.to_string(),
                // V17 Phase B: also forward `limit` — Feature C distinguishes a
                // full read from a head-peek slice.
                offset: tool_input.get("offset").and_then(|o| o.as_u64()),
                limit: tool_input.get("limit").and_then(|o| o.as_u64()),
                deny_prefix: "",
            })
        }
        Some("Bash") => {
            let command = tool_input
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            // Strict: `Some(path)` only for a provable pure whole-file read of
            // one file. Anything else ⇒ let the command run.
            let path = crate::graph::shellread::whole_file_read(command)?;
            // Resolve a relative shell path against the payload cwd so the server
            // relativizes it the same way it does an absolute `Read` file_path.
            let file_path = if std::path::Path::new(&path).is_absolute() {
                path
            } else {
                std::path::Path::new(cwd)
                    .join(&path)
                    .to_string_lossy()
                    .into_owned()
            };
            Some(ReadRequest {
                file_path,
                offset: None,
                limit: None,
                deny_prefix: BASH_DENY_PREFIX,
            })
        }
        // Future-proof: the same shim may serve more matchers later.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_payload_forwards_offset_and_limit_no_prefix() {
        let input = json!({ "file_path": "C:/proj/big.rs", "offset": 40, "limit": 80 });
        let r = plan_request(Some("Read"), &input, "C:/proj").expect("read request");
        assert_eq!(r.file_path, "C:/proj/big.rs");
        assert_eq!(r.offset, Some(40));
        assert_eq!(r.limit, Some(80));
        assert_eq!(r.deny_prefix, "");
    }

    #[test]
    fn read_empty_path_is_skipped() {
        assert!(plan_request(Some("Read"), &json!({ "file_path": "  " }), "C:/proj").is_none());
        assert!(plan_request(Some("Read"), &json!({}), "C:/proj").is_none());
    }

    #[test]
    fn bash_whole_file_read_resolves_relative_path_against_cwd() {
        let r = plan_request(
            Some("Bash"),
            &json!({ "command": "cat foo.txt" }),
            "C:/proj",
        )
        .expect("bash request");
        // Absolutized against cwd (not left bare-relative), offset/limit cleared,
        // and carries the shell deny prefix.
        assert!(
            std::path::Path::new(&r.file_path).is_absolute(),
            "got {}",
            r.file_path
        );
        assert!(
            r.file_path.replace('\\', "/").ends_with("proj/foo.txt"),
            "got {}",
            r.file_path
        );
        assert_eq!(r.offset, None);
        assert_eq!(r.limit, None);
        assert_eq!(r.deny_prefix, BASH_DENY_PREFIX);
    }

    #[test]
    fn bash_absolute_path_is_left_as_is() {
        let r = plan_request(
            Some("Bash"),
            &json!({ "command": "cat C:\\proj\\a.rs" }),
            "C:/other",
        )
        .expect("bash request");
        assert_eq!(r.file_path, "C:\\proj\\a.rs");
    }

    #[test]
    fn bash_non_whole_file_read_is_skipped() {
        // Pipe, partial-read verb, and a bare non-read command all pass through.
        assert!(plan_request(
            Some("Bash"),
            &json!({ "command": "cat a | grep x" }),
            "C:/p"
        )
        .is_none());
        assert!(plan_request(Some("Bash"), &json!({ "command": "head -50 f" }), "C:/p").is_none());
        assert!(plan_request(Some("Bash"), &json!({ "command": "npm test" }), "C:/p").is_none());
        assert!(plan_request(Some("Bash"), &json!({}), "C:/p").is_none());
    }

    #[test]
    fn non_target_tools_are_skipped() {
        assert!(plan_request(Some("Edit"), &json!({ "file_path": "a" }), "C:/p").is_none());
        assert!(plan_request(None, &json!({ "file_path": "a" }), "C:/p").is_none());
    }

    /// Verdict parity: a `Read` and the equivalent `cat` produce the same body
    /// (same file_path/offset/limit) — the only difference is the deny prefix.
    #[test]
    fn read_and_cat_yield_identical_body_modulo_prefix() {
        let bash = plan_request(
            Some("Bash"),
            &json!({ "command": "cat foo.txt" }),
            "C:/proj",
        )
        .expect("bash");
        let read = plan_request(
            Some("Read"),
            &json!({ "file_path": bash.file_path.clone() }),
            "C:/proj",
        )
        .expect("read");
        assert_eq!(bash.file_path, read.file_path);
        assert_eq!((bash.offset, bash.limit), (None, None));
        assert_eq!((read.offset, read.limit), (None, None));
        assert_eq!(read.deny_prefix, "");
        assert_eq!(bash.deny_prefix, BASH_DENY_PREFIX);
    }

    #[test]
    fn contract_checks_require_command_only_for_bash() {
        use crate::context_hook::missing_fields;
        // Bash with no command ⇒ reports the missing command field.
        let miss = missing_fields(&contract_checks(Some("Bash"), "s", "c", "", ""));
        assert!(miss.contains(&"tool_input.command"), "got {miss:?}");
        assert!(
            !miss.contains(&"tool_input.file_path"),
            "file_path not required for Bash: {miss:?}"
        );
        // Bash WITH a command ⇒ no drift.
        let ok = missing_fields(&contract_checks(Some("Bash"), "s", "c", "", "cat f"));
        assert!(ok.is_empty(), "got {ok:?}");
        // Read requires file_path, not command.
        let rmiss = missing_fields(&contract_checks(Some("Read"), "s", "c", "", ""));
        assert!(rmiss.contains(&"tool_input.file_path"), "got {rmiss:?}");
        assert!(!rmiss.contains(&"tool_input.command"), "got {rmiss:?}");
        // Base fields still checked regardless of tool.
        let base = missing_fields(&contract_checks(None, "", "", "", ""));
        assert!(
            base.contains(&"session_id") && base.contains(&"cwd") && base.contains(&"tool_name")
        );
    }
}
