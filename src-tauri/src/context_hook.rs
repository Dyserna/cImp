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
    if prompt.trim().is_empty() {
        return;
    }
    let session_id = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    let cwd = v
        .get("cwd")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let Some(disc) = crate::offload::loopback::read_discovery() else { return };
    let body = serde_json::json!({
        "cwd": cwd,
        "prompt": prompt,
        "session_id": session_id,
    })
    .to_string();

    let Some(text) = post_context(disc.port, &disc.token, &body) else { return };
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

/// Minimal blocking HTTP/1.1 POST to the loopback; returns the response's `text`
/// field, or `None` on any error/timeout.
fn post_context(port: u16, token: &str, body: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(TIMEOUT)).ok()?;
    let req = format!(
        "POST /context/retrieve HTTP/1.1\r\n\
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
    if !status_line.split(' ').nth(1).is_some_and(|c| c.starts_with('2')) {
        return None;
    }
    let start = resp.find("\r\n\r\n")? + 4;
    let json: serde_json::Value = serde_json::from_str(resp[start..].trim()).ok()?;
    json.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
}
