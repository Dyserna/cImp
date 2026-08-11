//! V10: the `cimp --context-hook` Claude Code `UserPromptSubmit` shim.
//!
//! Claude Code runs this with the hook payload on stdin
//! (`{session_id, prompt, cwd, …}`) and reads our stdout. We POST the prompt to
//! the app's loopback `/context/retrieve`, and — if it returns a non-empty
//! digest — print Claude's documented
//! `{hookSpecificOutput:{hookEventName:"UserPromptSubmit", additionalContext}}`
//! so the digest is prepended to the turn. On ANY error (no app running, a
//! timeout, an empty result) we print nothing and exit 0: the hook must never
//! block or perturb the user's turn.
//!
//! Deliberately dependency-light and synchronous (a blocking socket, no async
//! runtime) so it spawns and returns fast, like `--statusline`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Total budget for the loopback round-trip. Kept small so a slow/cold index
/// never delays the prompt; a miss just injects nothing.
const TIMEOUT: Duration = Duration::from_millis(600);

pub fn run() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let v: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };
    let prompt = v.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
    let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    let cwd_raw = v.get("cwd").and_then(|s| s.as_str()).unwrap_or("");
    // V16 Feature 3: report payload-shape drift BEFORE any early return, so
    // a payload broken enough to make this shim bail still gets counted.
    report_contract_drift(
        "context_hook",
        &missing_fields(&[
            ("session_id", !session_id.is_empty()),
            ("cwd", !cwd_raw.is_empty()),
        ]),
        session_id,
    );
    if prompt.trim().is_empty() {
        return;
    }
    let cwd = resolve_cwd(cwd_raw);

    let body = serde_json::json!({
        "cwd": cwd,
        "prompt": prompt,
        "session_id": session_id,
        // V13 Phase C: identifies this shim to the prompt-tap checkpoint
        // trigger (recorded on the checkpoint it fires) — see
        // `offload/loopback.rs`'s `ContextRetrieveBody::agent`.
        "agent": "claude",
        // V33: the cImp TAB this hook serves, baked into argv at spawn.
        //
        // The hook payload carries `session_id` and `cwd` but NO tab identity
        // (the same fact `--taint-beacon` is built around), and `agent` is the
        // harness NAME — shared by every Claude tab — so without this the
        // checkpoint stream cannot tell two Claude tabs on one project root
        // apart. `null` when the flag is absent (a `--settings` overlay written
        // by an older build, until the tab is restarted): the app records the
        // checkpoint with no tab rather than guessing one.
        "tab": tab_arg(&std::env::args().skip(1).collect::<Vec<_>>()),
    })
    .to_string();

    let Some(text) = post_loopback("/context/retrieve", &body) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }

    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": text,
        }
    });
    let _ = writeln!(std::io::stdout(), "{out}");
}

/// Discover the running app and POST `body` to a loopback `path`, returning the
/// response's `text` field, or `None` on any error/timeout/miss. Shared by the
/// V11 `--precompact-hook` / `--read-hook` shims so the framing (Bearer auth,
/// Content-Length, `Connection: close`, 2xx-only, short timeout) lives once.
///
/// Discovery is root-aware by this shim's own cwd: Claude spawns hook shims
/// in the project directory (the same fact `resolve_cwd` leans on), so with
/// several cImp instances off one install the hook reaches the instance
/// serving ITS project rather than the last one launched.
///
/// Since locked decision 30 (#48 F-11) that resolution is liveness-verified: a
/// discovery entry naming a port nothing answers on is skipped, so one planted
/// file can no longer make this shim deliver nowhere. It stays **fail-open** —
/// `None` here means no context is injected, never that the hook blocks — which is
/// deliberately unchanged: the fix is about *which* instance is chosen, not about
/// whether a shim refuses work.
pub(crate) fn post_loopback(path: &str, body: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok();
    let disc = crate::offload::loopback::read_discovery_for(cwd.as_deref())?;
    let answer = post_context(disc.port, &disc.token, path, body);
    if answer.is_none() {
        // The memoized endpoint answered a probe and has now failed a real post
        // (the app exited between the two, or rotated its token). Drop it so the
        // shim's SECOND post — every shim may report contract drift and then do its
        // own work — re-resolves rather than inheriting a dead endpoint.
        crate::offload::loopback::forget_resolved_discovery();
    }
    answer
}

/// The working directory a hook payload names, falling back to the shim's
/// own process cwd when the field is absent/empty (Claude spawns hook shims
/// in the project directory, so the fallback is usually right). Shared by
/// all three shims (`--context-hook` / `--precompact-hook` / `--read-hook`),
/// like `missing_fields`/`report_contract_drift`.
pub(crate) fn resolve_cwd(cwd_raw: &str) -> String {
    if !cwd_raw.is_empty() {
        return cwd_raw.to_string();
    }
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The value following `--tab` in `args`, trimmed and non-empty.
///
/// A cImp tab id is never discoverable from a hook payload — Claude Code sends
/// `session_id` and `cwd` and nothing that names a cImp tab (the E2 spike's
/// finding) — so every shim that needs one gets it baked into argv at spawn by
/// `tabs::config`, exactly like the per-tab `cimp-offload` / `cimp-code-audit`
/// MCP children. Lives here with the other shared shim helpers
/// (`resolve_cwd`/`missing_fields`/`post_loopback`) because `--taint-beacon`
/// and `--context-hook` both parse it and two copies of an identity parser is
/// how the two shims' notions of "which tab am I" drift apart.
///
/// Pure, so the contract ("no id ⇒ no tab claimed") is testable without a
/// socket or a Claude process.
pub(crate) fn tab_arg(args: &[String]) -> Option<String> {
    let i = args.iter().position(|a| a == "--tab")?;
    let raw = args.get(i + 1)?.trim();
    (!raw.is_empty()).then(|| raw.to_string())
}

/// V16 Feature 3: the required-field names a hook payload is missing —
/// `(name, present-and-non-empty)` pairs in, missing names out. Split from
/// the reporter so the check is unit-testable without a socket.
pub(crate) fn missing_fields(checks: &[(&'static str, bool)]) -> Vec<&'static str> {
    checks
        .iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| *name)
        .collect()
}

/// V16 Feature 3: report a hook payload that is missing required fields to
/// the app (`POST /activity/contract_drift`) so payload-shape drift is
/// caught at the earliest observable point. Fire-and-forget: the shim keeps
/// failing open exactly as before — this report is the only difference on
/// the broken path, and the happy path never POSTs it. Rate-limiting (one
/// event per shim per session) is app-side, so a systematically broken
/// payload can't flood the Activity store.
pub(crate) fn report_contract_drift(shim: &str, missing: &[&'static str], session_id: &str) {
    if missing.is_empty() {
        return;
    }
    let body = serde_json::json!({
        "shim": shim,
        "missing": missing,
        "session_id": session_id,
    })
    .to_string();
    let _ = post_loopback("/activity/contract_drift", &body);
}

/// Minimal blocking HTTP/1.1 POST to a loopback route; returns the response's
/// `text` field, or `None` on any error/timeout.
fn post_context(port: u16, token: &str, path: &str, body: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(TIMEOUT)).ok()?;
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut resp = Vec::new();
    // Best-effort read; the read timeout caps it even if the peer lingers.
    let _ = stream.read_to_end(&mut resp);
    let resp = String::from_utf8_lossy(&resp);
    // Only parse a 2xx body — a 401 (bad token) or 4xx/5xx carries a non-JSON or
    // error body we must not treat as injectable context.
    let status_line = resp.lines().next().unwrap_or("");
    if !status_line
        .split(' ')
        .nth(1)
        .is_some_and(|c| c.starts_with('2'))
    {
        return None;
    }
    let start = resp.find("\r\n\r\n")? + 4;
    let json: serde_json::Value = serde_json::from_str(resp[start..].trim()).ok()?;
    json.get("text")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    /// #48 (M-7): **every hook shim that POSTs to a gated `/context/*` route
    /// puts its baked tab id in the body.**
    ///
    /// Baking `--tab` into argv (`tabs::config`) and parsing it ([`tab_arg`])
    /// are both already pinned; this is the link between them — the one that,
    /// if dropped, leaves the flag on the command line, the parser in place,
    /// and the route resolving no scope at all. The three routes then admit
    /// everything and every test above still passes.
    ///
    /// A source scan, because a shim's `run()` reads stdin and opens a socket
    /// and this crate cannot drive either. It is scoped to the `json!` body of
    /// each shim so a `tab_arg` call anywhere else in the file cannot satisfy
    /// it.
    #[test]
    fn every_hook_shim_puts_its_tab_in_the_body_it_posts() {
        for (shim, src) in [
            ("--context-hook", include_str!("context_hook.rs")),
            ("--precompact-hook", include_str!("compact_hook.rs")),
            ("--read-hook", include_str!("read_hook.rs")),
            ("--postedit-hook", include_str!("postedit_hook.rs")),
        ] {
            let start = src
                .find("let body = serde_json::json!({")
                .unwrap_or_else(|| panic!("{shim}: no POST body"));
            let end = src[start..]
                .find("})\n    .to_string();")
                .or_else(|| src[start..].find("})\r\n    .to_string();"))
                .map(|e| start + e)
                .unwrap_or_else(|| panic!("{shim}: the body is not terminated"));
            let body = &src[start..end];
            assert!(
                body.contains("\"tab\": tab_arg("),
                "{shim} must send the tab it was spawned for: {body}"
            );
            assert!(
                body.contains("\"agent\": \"claude\""),
                "{shim} must say which harness it is, or the scope is keyed \
                 under the wrong agent: {body}"
            );
        }
    }
}
