//! V32 Phase F (locked decision 14): the `cimp --taint-beacon` Claude Code
//! `PreToolUse` shim — the sensor that makes Claude's OWN web tools visible to
//! the taint latch.
//!
//! # Why this exists
//!
//! cImp's containment latch (`offload/loopback.rs`) governs the tools the
//! loopback proxy serves. Claude Code's native `WebFetch` / `WebSearch` never
//! route through cImp, so a session can ingest a hostile page while `/status`
//! still reports that tab as `open` and the proxied local-capability tools stay
//! wide open beside it — precisely the fetch-then-read half of the lethal
//! trifecta the milestone exists to close. This shim reports the fact: it POSTs
//! to the loopback's `/latch/beacon`, which engages that tab's EXTERNAL latch
//! exactly as a proxied `ddg__fetch_content` would.
//!
//! # Report-only, and structurally incapable of denying
//!
//! Locked decision 14: *"Hooks never deny; a hook/loopback failure is silently
//! fail-open (sensor mode must never break a tab)."* A `PreToolUse` hook denies
//! only by SAYING so — exit code **2** (stderr fed back to the model) or exit 0
//! with `hookSpecificOutput.permissionDecision: "deny"` on stdout; any other
//! non-zero exit is a non-blocking error and the tool proceeds (verified
//! against the Claude Code hooks reference, 2026-08-07). This shim writes
//! **nothing** to either stream and returns normally in every path, so a dead
//! app, a rotated token or a 401 all end as "the call proceeds, unreported".
//! There is no branch here that can produce a denial, which is the property
//! that makes the mode safe to default on.
//!
//! # The documentation gap this design is built around (2026-08-07)
//!
//! The exit-code semantics above are documented. What is **NOT** documented is
//! what Claude Code does with a hook that TIMES OUT: whether a timeout maps to
//! the blocking (exit-2) case or the non-blocking one, and whether it cancels
//! the one hook command or the whole event. Verified against the current hooks
//! reference — the pages state the `timeout` field's unit and default and the
//! exit-code table, and say nothing about the timeout's effect on the call.
//!
//! Locked decision 14 requires this hook to be incapable of affecting a tool
//! call, so its fail-open property **must not rest on an undocumented
//! behaviour**. Hence: this shim never waits on anything it does not control.
//! It reads the discovery file, dispatches the POST with a sub-100 ms connect
//! and write deadline, and **never reads the response** — there is no
//! app-controlled duration anywhere in its path, so the configured `timeout` is
//! a backstop that should never be reached rather than the mechanism that keeps
//! the tool call safe. (Waiting on the loopback's *reply*, as the sibling shims
//! do, would have made the shim's latency a function of app health, which is
//! exactly the coupling that turns an unknown timeout semantic into a risk.)
//!
//! **Accepted consequence:** a beacon can be LOST — the app briefly down, a
//! connect refused, a write that does not land. That is the right trade for a
//! sensor: a missed engagement understates taint for one call (and the next
//! proxied call, the next beacon, or the manual override still surfaces it),
//! whereas a blocked `WebFetch` is a broken tab. Never trade the second for the
//! first.
//!
//! # Identity
//!
//! The hook payload carries `session_id` and `cwd` but no cImp TAB id, and the
//! latch registry is keyed by `(agent, tab)` — so `--tab <id>` is baked into
//! the hook command at spawn (`tabs/config.rs`), the same way the per-tab
//! `cimp-offload` MCP child gets its `--tab`. Without it there is nothing to
//! engage, and the shim returns without POSTing rather than guessing.
//!
//! # Cost
//!
//! One process spawn plus one one-way loopback POST, and ONLY on `WebFetch` /
//! `WebSearch` — the matcher installed in the `--settings` overlay is
//! `WebFetch|WebSearch`, so `Read`/`Grep`/`Bash` pay nothing.
//!
//! **Corrected 2026-08-08 (#48, review Part 7 item 16.)** This paragraph said
//! the POST is "capped by `context_hook`'s 600 ms socket timeout and the hook
//! entry's own 2 s budget". Both numbers named the wrong things. This shim
//! does not use `context_hook`'s timeout at all — it has its own
//! [`DISPATCH_TIMEOUT`] of **80 ms**, applied to the connect and to the write,
//! and it never reads the reply, so there is no third wait. The hook entry
//! `tabs::config` writes carries `"timeout": 5` (**5 s**, the siblings' value),
//! and that is a defence-in-depth ceiling on a pathological process spawn, not
//! the mechanism that keeps the call safe — see the timeout-semantics note
//! above. Both are fail-open.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::context_hook::{missing_fields, resolve_cwd, tab_arg};

/// The agent vocabulary this shim reports under. Only Claude Code runs
/// `PreToolUse` hooks; OpenCode beacons from its plugin
/// (`tabs::config::opencode_plugin_source`) with `"opencode"`.
const CONSUMER: &str = "claude";

/// The entire network budget of this shim, applied to the connect and to the
/// write separately (the response is never read, so there is no third).
///
/// Deliberately an order of magnitude below `context_hook::TIMEOUT`'s 600 ms:
/// the sibling shims wait because their whole purpose is the reply, while this
/// one has nothing to wait for. On loopback a live app accepts into the backlog
/// immediately and a dead one is refused immediately, so the only case this
/// bound covers is a wedged app — the case where waiting would be worst.
const DISPATCH_TIMEOUT: Duration = Duration::from_millis(80);

pub fn run() {
    // The tab id is baked into argv at spawn. No id ⇒ nothing to engage; return
    // before touching stdin or the network.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(tab) = tab_arg(&args) else {
        return;
    };

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let v: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };
    let tool_name = str_field(&v, "tool_name");
    let session_id = str_field(&v, "session_id");
    let cwd_raw = str_field(&v, "cwd");

    // Payload-shape drift, reported BEFORE any early return (the discipline
    // every sibling shim follows). `tool_name` is what the row names; `cwd` is
    // what routes the POST to the right instance when several cImps share one
    // install. Sent through this module's OWN dispatcher rather than
    // `context_hook::report_contract_drift`, which waits for a reply — the
    // module doc's rule is that nothing in this shim's path may block on app
    // health. Rare by construction: the happy path sends nothing here.
    let missing = missing_fields(&[
        ("tool_name", !tool_name.is_empty()),
        ("session_id", !session_id.is_empty()),
        ("cwd", !cwd_raw.is_empty()),
    ]);
    if !missing.is_empty() {
        dispatch(
            "/activity/contract_drift",
            &serde_json::json!({
                "shim": "taint_beacon",
                "missing": missing,
                "session_id": session_id,
            })
            .to_string(),
        );
    }

    let body = serde_json::json!({
        "tab": tab,
        "consumer": CONSUMER,
        // Reported verbatim so the activity row and the log name the tool the
        // harness actually ran. An empty value is still reported: the beacon
        // fired, and the app labels it rather than dropping the engagement.
        "tool": tool_name,
        "cwd": resolve_cwd(cwd_raw),
        "session_id": session_id,
    })
    .to_string();

    // Fire-and-forget. Nothing is written to stdout or stderr, and nothing is
    // awaited — see the module doc on why that is the whole safety argument.
    dispatch("/latch/beacon", &body);
}

/// Send one loopback POST and return, **without reading the response**.
///
/// The deliberate difference from `context_hook::post_loopback`: that helper
/// waits for a reply because its callers need one (injected context, a read
/// verdict). This shim needs nothing back, so waiting would only make its
/// duration a function of app health — and with the harness's timeout semantics
/// undocumented (module doc), a duration we do not control is the one thing
/// that could let this hook affect a tool call.
///
/// Every failure — no running instance, a refused connect, a partial write — is
/// swallowed. A lost beacon understates taint for one call; a blocked
/// `WebFetch` breaks the tab.
///
/// Discovery is root-aware by this process's own cwd, exactly like
/// `post_loopback`: Claude spawns hook shims in the project directory, so with
/// several cImp instances off one install the beacon reaches the instance
/// serving ITS project rather than the last one launched.
fn dispatch(path: &str, body: &str) {
    let cwd = std::env::current_dir().ok();
    let Some(disc) = crate::offload::loopback::read_discovery_for(cwd.as_deref()) else {
        return;
    };
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), disc.port);
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, DISPATCH_TIMEOUT) else {
        return;
    };
    if stream.set_write_timeout(Some(DISPATCH_TIMEOUT)).is_err() {
        return;
    }
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        disc.token,
        body.len()
    );
    let _ = stream.write_all(req.as_bytes());
    // No read: the peer's response goes unread and the socket closes on drop.
    // `Connection: close` already told it not to expect reuse.
}

/// A payload string field, or `""` when absent/not-a-string.
fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|f| f.as_str()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `tab_arg` moved to `context_hook` in V33 when `--context-hook` grew the
    /// same `--tab` flag; this test stays here because this shim's contract
    /// ("no tab ⇒ no beacon at all") is the stricter of the two consumers.
    #[test]
    fn tab_arg_reads_the_baked_id_and_refuses_an_empty_one() {
        let a = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert_eq!(
            tab_arg(&a(&["--taint-beacon", "--tab", "claude-2"])).as_deref(),
            Some("claude-2")
        );
        // Order-independent, and whitespace-trimmed.
        assert_eq!(
            tab_arg(&a(&["--tab", " claude ", "--taint-beacon"])).as_deref(),
            Some("claude")
        );
        // Nothing to engage: absent flag, missing value, or an empty one. Each
        // must yield `None` so `run` returns before POSTing — a beacon with no
        // tab identity could only guess which conversation to latch.
        assert!(tab_arg(&a(&["--taint-beacon"])).is_none());
        assert!(tab_arg(&a(&["--taint-beacon", "--tab"])).is_none());
        assert!(tab_arg(&a(&["--tab", "   "])).is_none());
        assert!(tab_arg(&[]).is_none());
    }

    /// The drift checks name the three fields the beacon depends on, and a
    /// complete payload reports nothing (the happy path never POSTs a drift
    /// row).
    #[test]
    fn contract_checks_cover_the_fields_the_beacon_reads() {
        let full = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s-1",
            "cwd": "P:/proj",
            "tool_name": "WebFetch",
        });
        let checks = |v: &serde_json::Value| {
            missing_fields(&[
                ("tool_name", !str_field(v, "tool_name").is_empty()),
                ("session_id", !str_field(v, "session_id").is_empty()),
                ("cwd", !str_field(v, "cwd").is_empty()),
            ])
        };
        assert!(checks(&full).is_empty());
        let bare = json!({});
        assert_eq!(checks(&bare), vec!["tool_name", "session_id", "cwd"]);
        // A non-string field reads as absent rather than panicking.
        assert_eq!(str_field(&json!({ "tool_name": 7 }), "tool_name"), "");
    }
}
