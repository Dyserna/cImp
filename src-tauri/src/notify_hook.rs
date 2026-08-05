//! NC-2 (issue #5): the `cimp --notify-hook` Claude Code `Notification` /
//! `PermissionDenied` shim — the PRIMARY "this tab is awaiting a permission
//! decision" signal.
//!
//! Claude Code runs this with the hook payload on stdin and IGNORES everything
//! we produce (both events are observe-only: no decision control, and for
//! `PermissionDenied` even the exit code and stderr are documented as ignored).
//! We simply forward the payload to the app's loopback `/permission/event`,
//! which maps it to a tab and emits the same `PermissionPromptDetected` /
//! `PermissionPromptResolved` state signals the TUI-regex detector emits. The
//! regex detector stays installed as the FALLBACK: both paths feed the same
//! idempotent `awaiting_permission` flag, so a double-fire is a no-op and a
//! missed hook still gets caught by the pattern matcher.
//!
//! One shim serves both events — the payload's `hook_event_name` says which one
//! fired, so a single overlay command covers the pair (and any future event we
//! decide to route here).
//!
//! Contract notes:
//!   * **Verified** (Claude Code hooks guide): `Notification` fires when Claude
//!     Code surfaces a notification, its matcher filters on the notification
//!     TYPE (`permission_prompt`, `idle_prompt`, `auth_success`,
//!     `elicitation_dialog`, `elicitation_complete`, `elicitation_response`,
//!     `agent_needs_input`, `agent_completed`), and `"matcher": ""` "fires on
//!     all notification types". We register with `""` and classify app-side
//!     (see `offload::loopback::classify_permission_event`), which reads an
//!     UNRECOGNIZED type as "no usable type" and falls through to the prose
//!     check — so a renamed type degrades to "classified by its message", and
//!     only a notification that is neither a known type nor permission-flavoured
//!     prose is ignored. Never to silence (M12, 2026-08-05 review: the earlier
//!     shape returned early on any unrecognized type, which for the permission
//!     case IS silence).
//!   * **UNVERIFIED — read this before changing the parsing below.** The
//!     reference page could not be retrieved reliably enough to pin the
//!     `Notification` payload's exact shape: it is either flat
//!     (`{notification_type, message}`) or nested
//!     (`{notification: {type, message}}`) depending on which rendering of the
//!     doc you get. This shim therefore reads BOTH shapes and forwards whatever
//!     it finds; the app-side classifier falls back to prose matching whenever
//!     no RECOGNIZED type arrives (absent, renamed, or unknown). To settle it
//!     empirically, register
//!     `"command": "cat > C:/tmp/notif.json"` as a `Notification` hook and read
//!     the captured stdin.
//!   * `PermissionRequest` is NOT adopted: it fires BEFORE the decision — i.e.
//!     also for calls that allow-rules/auto-mode approve silently — so it is
//!     not a "a prompt is on screen" signal, and it costs a shim spawn per tool
//!     call.
//!
//! Deliberately dependency-light, synchronous, and fail-open like the sibling
//! shims (`--context-hook` / `--read-hook`): Claude waits on hook processes, so
//! a dead or slow cImp instance must never delay a permission decision. Prints
//! NOTHING on stdout in every case (an observe-only hook must never emit
//! `hookSpecificOutput`) and always exits 0.

use std::io::Read;

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
    let event = str_field(&v, "hook_event_name");
    let session_id = str_field(&v, "session_id");
    let cwd_raw = str_field(&v, "cwd");
    let transcript_path = str_field(&v, "transcript_path");
    // The notification's type and prose, read from every spelling the payload
    // is documented/observed with — flat (`notification_type` / `message`) or
    // nested under a `notification` object (`type` / `message`). Empty for
    // `PermissionDenied` (and for a Notification carrying only prose).
    let nested = v
        .get("notification")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let notification_type = first_non_empty(&[
        str_field(&v, "notification_type"),
        str_field(&v, "type"),
        str_field(&nested, "notification_type"),
        str_field(&nested, "type"),
    ]);
    let message = first_non_empty(&[
        str_field(&v, "message"),
        str_field(&v, "title"),
        str_field(&nested, "message"),
        str_field(&nested, "title"),
    ]);
    let tool_name = str_field(&v, "tool_name");

    // Payload-shape drift, reported BEFORE any early return (same discipline as
    // the sibling shims). We depend on: `session_id` + `transcript_path` + `cwd`
    // (the tab-mapping inputs — mapping needs at least one of them),
    // `hook_event_name` (which event fired), and — for a `Notification` — some
    // way to tell a permission prompt from an idle one (`notification_type` or
    // a `message`). A `PermissionDenied` needs neither of those last two.
    report_contract_drift(
        "notify_hook",
        &missing_fields(&contract_checks(
            event,
            session_id,
            cwd_raw,
            transcript_path,
            notification_type,
            message,
        )),
        session_id,
    );

    if event.is_empty() {
        // Without the event name the app can't classify the edge, and guessing
        // would risk flipping `awaiting_permission` on an idle notification.
        return;
    }
    let cwd = resolve_cwd(cwd_raw);

    let body = serde_json::json!({
        "cwd": cwd,
        "session_id": session_id,
        "transcript_path": transcript_path,
        "event": event,
        "notification_type": notification_type,
        "message": message,
        "tool_name": tool_name,
    })
    .to_string();

    // Fire-and-forget: the response (if any) is discarded, and nothing is ever
    // written to stdout.
    let _ = post_loopback("/permission/event", &body);
}

/// A payload string field, or `""` when absent/not-a-string.
fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|f| f.as_str()).unwrap_or("")
}

/// The first non-empty candidate, or `""`. Lets one shim tolerate the payload
/// being flat or nested, and a field rename (`notification_type` ⇄ `type`,
/// `message` ⇄ `title`), without a release — see the module doc's UNVERIFIED
/// note on the `Notification` payload shape.
fn first_non_empty<'a>(candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .copied()
        .find(|s| !s.is_empty())
        .unwrap_or("")
}

/// The `(field, present)` requiredness pairs for a notify-hook payload, split
/// out so the check is unit-testable without a socket. Event-aware: the
/// classification fields only matter for a `Notification`, and mapping needs at
/// least one of `session_id` / `transcript_path` / `cwd` (reported per-field so
/// the drift entry names what actually went missing).
fn contract_checks(
    event: &str,
    session_id: &str,
    cwd_raw: &str,
    transcript_path: &str,
    notification_type: &str,
    message: &str,
) -> Vec<(&'static str, bool)> {
    let is_notification = event == "Notification";
    vec![
        ("hook_event_name", !event.is_empty()),
        ("session_id", !session_id.is_empty()),
        ("cwd", !cwd_raw.is_empty()),
        ("transcript_path", !transcript_path.is_empty()),
        (
            "notification_type|message",
            !is_notification || !notification_type.is_empty() || !message.is_empty(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The notification type is read from the flat shape, the nested
    /// (`notification: {type}`) shape, or the `type` alias — whichever the
    /// installed Claude Code actually sends (module doc: UNVERIFIED shape).
    #[test]
    fn reads_notification_type_from_flat_or_nested_payloads() {
        let flat = json!({ "notification_type": "permission_prompt" });
        let alias = json!({ "type": "idle_prompt" });
        let nested = json!({ "notification": { "type": "permission_prompt", "message": "m" } });
        let pick = |v: &serde_json::Value| -> String {
            let n = v
                .get("notification")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            first_non_empty(&[
                str_field(v, "notification_type"),
                str_field(v, "type"),
                str_field(&n, "notification_type"),
                str_field(&n, "type"),
            ])
            .to_string()
        };
        assert_eq!(pick(&flat), "permission_prompt");
        assert_eq!(pick(&alias), "idle_prompt");
        assert_eq!(pick(&nested), "permission_prompt");
        assert_eq!(pick(&json!({})), "");
        // An empty string never wins over a later populated candidate.
        assert_eq!(first_non_empty(&["", "", "x", "y"]), "x");
        assert_eq!(first_non_empty(&[]), "");
        assert_eq!(str_field(&json!({ "a": 3 }), "a"), "", "non-string ⇒ empty");
    }

    #[test]
    fn contract_checks_require_classification_only_for_notification() {
        // A Notification with neither a type nor a message can't be classified.
        let miss = missing_fields(&contract_checks("Notification", "s", "c", "t", "", ""));
        assert_eq!(miss, vec!["notification_type|message"]);
        // Either one alone is enough.
        assert!(missing_fields(&contract_checks(
            "Notification",
            "s",
            "c",
            "t",
            "permission_prompt",
            ""
        ))
        .is_empty());
        assert!(missing_fields(&contract_checks(
            "Notification",
            "s",
            "c",
            "t",
            "",
            "Claude needs your permission to use Bash"
        ))
        .is_empty());
        // PermissionDenied carries neither and that is not drift.
        assert!(
            missing_fields(&contract_checks("PermissionDenied", "s", "c", "t", "", "")).is_empty()
        );
    }

    #[test]
    fn contract_checks_report_each_missing_mapping_field() {
        let miss = missing_fields(&contract_checks("", "", "", "", "", ""));
        assert!(miss.contains(&"hook_event_name"), "got {miss:?}");
        assert!(miss.contains(&"session_id"), "got {miss:?}");
        assert!(miss.contains(&"cwd"), "got {miss:?}");
        assert!(miss.contains(&"transcript_path"), "got {miss:?}");
    }
}
