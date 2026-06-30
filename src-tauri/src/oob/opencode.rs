//! V20: OpenCode out-of-band TTS via the event stream.
//!
//! cImp launches the OpenCode fullscreen TUI with `--port <N> --hostname
//! 127.0.0.1`, so the TUI hosts its own HTTP server on that port. We subscribe
//! to `GET /event` (SSE) and read assistant text from the structured events —
//! no terminal scraping. The event vocabulary (captured live, spike 0a):
//!
//!   * `message.updated` — `properties.info.{id,role,time}`. `role:"assistant"`
//!     identifies an assistant message; `time.completed` marks it finished.
//!   * `message.part.delta` — `properties.{messageID,partID,field,delta}`.
//!     `field:"text"` is speakable prose; `field:"reasoning"` is skipped.
//!   * `session.idle` — the turn finished; flush anything still buffered.
//!
//! We accumulate text deltas per assistant message and flush (segment → speak)
//! when that message completes, so each assistant message is spoken as soon as
//! it finishes — including messages between tool calls. The same stream drives
//! the avatar Thinking/Idle state (V20 Phase E folds in here since it shares the
//! connection).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, trace, warn};

use super::OobContext;
use crate::state::StateSignal;

const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// Subscribe to the OpenCode event stream on `port` and drive TTS + avatar
/// state until the tab's cancel token fires. Reconnects on stream errors (the
/// TUI may not have bound the port yet at launch, or may restart its server).
pub async fn run(port: u16, ctx: OobContext) {
    let url = format!("http://127.0.0.1:{port}/event");
    // No request timeout: this is a long-lived stream. (reqwest's default
    // builder sets none; we read with explicit cancel-aware selects instead.)
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            warn!(tab = ?ctx.tab, error = %e, "OpenCode OOB: client build failed");
            return;
        }
    };

    loop {
        if ctx.cancel.is_cancelled() {
            return;
        }
        match consume(&client, &url, &ctx).await {
            Ok(()) => return, // cancelled cleanly inside.
            Err(e) => {
                trace!(tab = ?ctx.tab, error = %e, "OpenCode OOB: stream ended; reconnecting");
                tokio::select! {
                    _ = ctx.cancel.cancelled() => return,
                    _ = sleep(RECONNECT_DELAY) => {}
                }
            }
        }
    }
}

/// One connection lifetime: open the SSE stream and process events until it
/// ends or the cancel token fires. Returns `Ok(())` only on cancellation.
async fn consume(client: &reqwest::Client, url: &str, ctx: &OobContext) -> reqwest::Result<()> {
    let resp = tokio::select! {
        _ = ctx.cancel.cancelled() => return Ok(()),
        r = client.get(url).send() => r?,
    };
    let mut resp = resp.error_for_status()?;
    debug!(tab = ?ctx.tab, "OpenCode OOB: event stream connected");

    let mut state = Tracker::default();
    let mut line_buf = String::new();

    loop {
        let chunk = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(()),
            c = resp.chunk() => c?,
        };
        let chunk = match chunk {
            Some(c) => c,
            None => {
                // Stream closed; flush remainder and signal idle.
                state.flush_all(ctx).await;
                return Ok(());
            }
        };
        line_buf.push_str(&String::from_utf8_lossy(&chunk));
        // Process complete lines; keep any trailing partial line buffered.
        while let Some(nl) = line_buf.find('\n') {
            let line: String = line_buf.drain(..=nl).collect();
            let line = line.trim_end();
            if let Some(payload) = line.strip_prefix("data:") {
                if let Ok(ev) = serde_json::from_str::<Value>(payload.trim()) {
                    state.handle(&ev, ctx).await;
                }
            }
        }
    }
}

/// Per-connection accumulation of assistant messages and working state.
///
/// Text is buffered per **part** (`partID`), not per message, because OpenCode
/// streams a message's reasoning and its answer as separate parts — and BOTH
/// arrive as `message.part.delta` events with `field:"text"`. The only thing
/// that distinguishes them is the part's `type` (`reasoning` vs `text`), which
/// comes from `message.part.updated`. So we key text by part, learn each part's
/// type, and at flush time speak only the non-reasoning parts of the message.
#[derive(Default)]
struct Tracker {
    /// partID -> accumulated delta text.
    part_text: HashMap<String, String>,
    /// partID -> latest full-text snapshot (from `message.part.updated`),
    /// used when a part arrives without deltas (e.g. a short message).
    part_snapshot: HashMap<String, String>,
    /// partID -> part type ("text" / "reasoning" / ...). Unknown ⇒ treated as
    /// speakable text (reasoning parts are reliably declared).
    part_type: HashMap<String, String>,
    /// partID -> owning messageID.
    part_msg: HashMap<String, String>,
    /// messageID -> partIDs in first-seen order (preserves answer order).
    msg_parts: HashMap<String, Vec<String>>,
    /// messageIDs known to be assistant messages.
    assistant: HashSet<String>,
    /// messageIDs already spoken (don't double-flush on idle).
    flushed: HashSet<String>,
    /// Whether we've emitted ClaudeOutputStarted without a matching Stopped.
    working: bool,
}

impl Tracker {
    async fn handle(&mut self, ev: &Value, ctx: &OobContext) {
        let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let props = ev.get("properties").unwrap_or(&Value::Null);
        match kind {
            "message.updated" => {
                let info = props.get("info").unwrap_or(&Value::Null);
                if info.get("role").and_then(Value::as_str) == Some("assistant") {
                    if let Some(id) = info.get("id").and_then(Value::as_str) {
                        self.assistant.insert(id.to_string());
                        self.set_working(ctx, true);
                        let completed = info
                            .get("time")
                            .and_then(|t| t.get("completed"))
                            .is_some();
                        if completed {
                            self.flush(id, ctx).await;
                        }
                    }
                }
            }
            "message.part.updated" => {
                // Learn the part's type (text vs reasoning) and capture its
                // full-text snapshot. `part.id` is the partID.
                let part = props.get("part").unwrap_or(&Value::Null);
                if let Some(pid) = part.get("id").and_then(Value::as_str) {
                    if let Some(ty) = part.get("type").and_then(Value::as_str) {
                        self.part_type.insert(pid.to_string(), ty.to_string());
                    }
                    if let Some(mid) = part.get("messageID").and_then(Value::as_str) {
                        self.register_part(mid, pid);
                    }
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        self.part_snapshot.insert(pid.to_string(), text.to_string());
                    }
                }
            }
            "message.part.delta" => {
                // Every delta is `field:"text"` (even for reasoning parts), so
                // we DON'T filter on field — we buffer per part and decide by
                // part type at flush.
                if let (Some(pid), Some(mid), Some(delta)) = (
                    props.get("partID").and_then(Value::as_str),
                    props.get("messageID").and_then(Value::as_str),
                    props.get("delta").and_then(Value::as_str),
                ) {
                    self.register_part(mid, pid);
                    self.part_text.entry(pid.to_string()).or_default().push_str(delta);
                }
            }
            "session.idle" => {
                self.flush_all(ctx).await;
                self.set_working(ctx, false);
            }
            _ => {}
        }
    }

    /// Record a part under its message, preserving first-seen order, and its
    /// owning messageID.
    fn register_part(&mut self, mid: &str, pid: &str) {
        self.part_msg.entry(pid.to_string()).or_insert_with(|| mid.to_string());
        let parts = self.msg_parts.entry(mid.to_string()).or_default();
        if !parts.iter().any(|p| p == pid) {
            parts.push(pid.to_string());
        }
    }

    /// Speak a single assistant message once: concatenate its non-reasoning
    /// parts in order and hand them to TTS.
    async fn flush(&mut self, mid: &str, ctx: &OobContext) {
        if self.flushed.contains(mid) || !self.assistant.contains(mid) {
            return;
        }
        self.flushed.insert(mid.to_string());
        let Some(parts) = self.msg_parts.get(mid) else {
            return;
        };
        let mut out = String::new();
        for pid in parts {
            // Skip reasoning; unknown type defaults to speakable text.
            if self.part_type.get(pid).map(String::as_str) == Some("reasoning") {
                continue;
            }
            let text = self
                .part_text
                .get(pid)
                .filter(|t| !t.trim().is_empty())
                .or_else(|| self.part_snapshot.get(pid))
                .map(String::as_str)
                .unwrap_or("");
            if !text.trim().is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        if !out.trim().is_empty() {
            trace!(tab = ?ctx.tab, "OpenCode OOB: speaking assistant message (reasoning excluded)");
            ctx.speak(&out).await;
        }
    }

    /// Flush every not-yet-spoken assistant message (turn ended).
    async fn flush_all(&mut self, ctx: &OobContext) {
        let ids: Vec<String> = self
            .assistant
            .iter()
            .filter(|id| !self.flushed.contains(*id))
            .cloned()
            .collect();
        for id in ids {
            self.flush(&id, ctx).await;
        }
    }

    /// Edge-triggered avatar Thinking/Idle, mirroring the old scrape path's
    /// `claude_working` marker.
    fn set_working(&mut self, ctx: &OobContext, working: bool) {
        if working == self.working {
            return;
        }
        self.working = working;
        let tab = ctx.tab.clone();
        ctx.signal(if working {
            StateSignal::ClaudeOutputStarted { tab }
        } else {
            StateSignal::ClaudeOutputStopped { tab }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Settings, SettingsHandle};
    use crate::state::TabId;
    use crate::tts::TtsRequest;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn ctx_with(tab: &str) -> (OobContext, mpsc::Receiver<TtsRequest>, mpsc::Receiver<StateSignal>) {
        let (tts_tx, tts_rx) = mpsc::channel(64);
        let (sig_tx, sig_rx) = mpsc::channel(64);
        // Seed the opencode tab so the per-tab TTS gate (tts_injection.enabled,
        // true by default for opencode) is satisfied — Settings::default() ships
        // no tabs; the real app seeds them via persistence.
        let mut defaults = Settings::default();
        defaults.tabs.push(crate::settings::default_opencode_tab());
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());
        let ctx = OobContext {
            tab: TabId::from_str(tab),
            tts: tts_tx,
            state_signals: sig_tx,
            settings,
            cancel: CancellationToken::new(),
        };
        (ctx, tts_rx, sig_rx)
    }

    fn ev(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[tokio::test]
    async fn assistant_message_completes_and_speaks() {
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{"created":1}}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.updated","properties":{"part":{"id":"p1","type":"text","messageID":"m1","text":""}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"p1","field":"text","delta":"Hello world."}}"#),
            &ctx,
        )
        .await;
        // No flush yet.
        assert!(tts_rx.try_recv().is_err());
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{"created":1,"completed":2}}}}"#),
            &ctx,
        )
        .await;
        match tts_rx.try_recv() {
            Ok(TtsRequest::Synthesize { text, .. }) => assert_eq!(text, "Hello world."),
            other => panic!("expected synthesize, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reasoning_part_is_skipped_answer_is_spoken() {
        // Regression: reasoning and answer BOTH stream as field:"text" deltas;
        // only the part type (from message.part.updated) tells them apart. The
        // reasoning part must not be spoken; the text part must.
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{}}}}"#),
            &ctx,
        )
        .await;
        // Reasoning part (declared reasoning) — deltas use field:"text".
        t.handle(
            &ev(r#"{"type":"message.part.updated","properties":{"part":{"id":"pr","type":"reasoning","messageID":"m1","text":""}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"pr","field":"text","delta":"Let me think about this."}}"#),
            &ctx,
        )
        .await;
        // Answer part.
        t.handle(
            &ev(r#"{"type":"message.part.updated","properties":{"part":{"id":"pt","type":"text","messageID":"m1","text":""}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"pt","field":"text","delta":"The answer is 42."}}"#),
            &ctx,
        )
        .await;
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;

        match tts_rx.try_recv() {
            Ok(TtsRequest::Synthesize { text, .. }) => {
                assert!(text.contains("The answer is 42."), "got: {text}");
                assert!(!text.contains("Let me think"), "reasoning leaked: {text}");
            }
            other => panic!("expected the answer to be spoken, got {other:?}"),
        }
        // Only the answer — nothing else queued.
        assert!(tts_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reasoning_part_ordering_independent() {
        // The reasoning delta can arrive BEFORE its part-type is declared; the
        // decision is made at flush, so it's still skipped.
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{}}}}"#),
            &ctx,
        )
        .await;
        // Delta first, type declaration after.
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"pr","field":"text","delta":"secret thoughts"}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.updated","properties":{"part":{"id":"pr","type":"reasoning","messageID":"m1","text":""}}}"#),
            &ctx,
        )
        .await;
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;
        assert!(tts_rx.try_recv().is_err(), "reasoning must not be spoken even if declared late");
    }

    #[tokio::test]
    async fn user_message_text_is_not_spoken() {
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        // A user message + its echoed text part must never reach TTS.
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"u1","role":"user","time":{}}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"u1","partID":"pu","field":"text","delta":"my prompt"}}"#),
            &ctx,
        )
        .await;
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;
        assert!(tts_rx.try_recv().is_err(), "user text must not be spoken");
    }

    #[tokio::test]
    async fn session_idle_flushes_uncompleted_message() {
        let (ctx, mut tts_rx, mut sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m9","role":"assistant","time":{}}}}"#),
            &ctx,
        )
        .await;
        // No part-type declaration: unknown ⇒ treated as speakable text.
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m9","partID":"p9","field":"text","delta":"Done now."}}"#),
            &ctx,
        )
        .await;
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;
        assert!(matches!(tts_rx.try_recv(), Ok(TtsRequest::Synthesize { .. })));
        // Working state went up then down.
        assert!(matches!(sig.try_recv(), Ok(StateSignal::ClaudeOutputStarted { .. })));
        assert!(matches!(sig.try_recv(), Ok(StateSignal::ClaudeOutputStopped { .. })));
    }
}
