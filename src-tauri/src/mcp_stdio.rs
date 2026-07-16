//! Shared scaffolding for cImp's stdio MCP children (`--offload-mcp`,
//! `--code-audit-mcp`): the newline-delimited JSON-RPC read loop and the
//! common `isError` tool-result shape.
//!
//! Extracted (V26 review) so the children can't drift: the loop carries the
//! panic-capture spawn, the shared-stdout write mutex, the shutdown-on-broken-
//! stdout guard, and stdin robustness exactly once — a hardening fix applied
//! here reaches every server.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as TokioMutex;

/// A JSON-RPC handler outcome: `Ok(result)` or `Err((code, message))` for a
/// JSON-RPC error object.
pub type RpcResult = Result<Value, (i64, String)>;

/// An `isError` MCP tool result — the one wire shape every cImp server uses
/// for tool-level failures the model should read and adapt to (as opposed to
/// protocol-level JSON-RPC errors).
pub fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

/// The stdio JSON-RPC request loop shared by every cImp MCP child.
///
/// Each request is spawned so a minutes-long `tools/call` can't wedge the read
/// loop — a concurrent `ping` / `initialize` still gets answered. Responses
/// are matched by `id`, so out-of-order completion is fine; the shared
/// `stdout` mutex serializes writes (a caller may hold its own clone for
/// out-of-band notifications, e.g. the offload child's `/events` relay).
///
/// Robustness, identical for every child:
/// - a handler panic surfaces as a JSON-RPC error (the client still gets a
///   reply) via the nested spawn, never a hung caller;
/// - a failed response write (the host closed the pipe) sets a shutdown flag
///   that stops the loop spawning handlers whose results can't be delivered;
/// - an invalid-UTF-8 frame on stdin is skipped rather than treated as EOF —
///   one stray byte must not kill an otherwise-healthy server mid-session.
///   Only a real I/O error or EOF ends the loop.
pub async fn serve<F, Fut>(
    stdout: Arc<TokioMutex<tokio::io::Stdout>>,
    panic_label: &'static str,
    handler: F,
) where
    F: Fn(String, Value) -> Fut,
    Fut: std::future::Future<Output = RpcResult> + Send + 'static,
{
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    // Set by a spawned handler when its response write fails (the host closed
    // stdout): stop accepting new work whose results could never be delivered.
    let shutdown = Arc::new(AtomicBool::new(false));

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break, // EOF: the host closed stdin
            // The offending line is consumed; skip it and keep serving.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(_) => break, // real I/O error: the pipe is gone
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // ignore malformed frames
        };
        // Notifications (no id) get no response — don't spawn a handler for
        // them (it would run only to discard the result).
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let fut = handler(method, params);
        let stdout = stdout.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            // Run the handler on its own task so a panic inside it surfaces as
            // a JSON-RPC error (the client gets a reply) rather than being
            // swallowed by the dropped JoinHandle — which would hang the
            // caller forever waiting on a response that never comes.
            let response = match tokio::spawn(fut).await {
                Ok(r) => r,
                Err(e) => Err((-32603, format!("{panic_label} handler panicked: {e}"))),
            };
            let frame = match response {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            };
            let mut bytes = frame.to_string();
            bytes.push('\n');
            let mut out = stdout.lock().await;
            if out.write_all(bytes.as_bytes()).await.is_err() || out.flush().await.is_err() {
                // stdout is gone (the host closed the pipe): signal the read
                // loop to stop spawning handlers.
                shutdown.store(true, Ordering::Relaxed);
            }
        });
    }
}
