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
//!
//! ## V14 Phase C spike (C3): no usage/token fields on this stream
//! `message.updated`'s `properties.info` object was captured exhaustively
//! live in spike 0a (the vocabulary above is the complete shape observed) and
//! carries only `{id, role, time}` — no `tokens`/`usage`/cost fields on the
//! pinned OpenCode version. So unlike Claude's transcript (which embeds exact
//! `usage` on every assistant message), OpenCode gives this SSE consumer
//! nothing to build an exact token count from, and this file adds no usage
//! tap. Per the milestone's "absent ⇒ tool_result-class events only" fallback,
//! the actual OpenCode usage tap lives at `offload::loopback::handle_memory_event`
//! (`POST /memory/event`) instead — already the sole memory ingress for
//! OpenCode (see that function's doc comment) and the only place that has
//! both a `cwd`/session AND a tool name to record against; this SSE consumer
//! has neither. It estimates chars from the tool's INPUT args (its output
//! isn't visible to that hook either), which is why every OpenCode session
//! reports `est_only: true` in [`crate::graph::GraphIndex::usage_all_sessions`]
//! (derived from `session.agent != "claude"`, not a separately tracked flag).
//! TODO(spike C3): re-check `message.updated`'s `info` shape against a newer
//! pinned OpenCode release — if a future version adds token/usage fields
//! there, wire a `record_usage` call into this file's `Tracker` directly
//! (it would need a `root`/session context this struct doesn't carry today).

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
            Ok(StreamEnd::Cancelled) => return,
            Ok(StreamEnd::Closed) => {
                // A live SSE stream never ends gracefully in normal operation,
                // so a clean close means the server went away (e.g. the TUI
                // restarted its HTTP server) — reconnect, don't stop.
                trace!(tab = ?ctx.tab, "OpenCode OOB: stream closed; reconnecting");
            }
            Err(e) => {
                trace!(tab = ?ctx.tab, error = %e, "OpenCode OOB: stream ended; reconnecting");
            }
        }
        tokio::select! {
            _ = ctx.cancel.cancelled() => return,
            _ = sleep(RECONNECT_DELAY) => {}
        }
    }
}

/// Why one connection's event loop stopped (the non-error cases).
#[derive(Debug, PartialEq, Eq)]
enum StreamEnd {
    /// The tab's cancel token fired — stop the adapter for good.
    Cancelled,
    /// The server closed the stream — reconnect and keep listening.
    Closed,
}

/// One connection lifetime: open the SSE stream and process events until it
/// ends or the cancel token fires.
async fn consume(
    client: &reqwest::Client,
    url: &str,
    ctx: &OobContext,
) -> reqwest::Result<StreamEnd> {
    let resp = tokio::select! {
        _ = ctx.cancel.cancelled() => return Ok(StreamEnd::Cancelled),
        r = client.get(url).send() => r?,
    };
    let mut resp = resp.error_for_status()?;
    debug!(tab = ?ctx.tab, "OpenCode OOB: event stream connected");

    let mut state = Tracker::default();
    // Raw bytes: chunk boundaries can split a multi-byte UTF-8 sequence, so
    // decoding happens per COMPLETE line (in `drain_lines`), never per chunk —
    // a per-chunk lossy decode would corrupt the split character to U+FFFD.
    let mut line_buf: Vec<u8> = Vec::new();

    loop {
        let chunk = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(StreamEnd::Cancelled),
            c = resp.chunk() => c,
        };
        let chunk = match chunk {
            Ok(Some(c)) => c,
            Ok(None) => {
                // Stream closed; flush the remainder and release Thinking —
                // the fresh Tracker after reconnect starts with `working:
                // false` and can't release a stale edge from this connection.
                state.flush_all(ctx).await;
                state.set_working(ctx, false);
                return Ok(StreamEnd::Closed);
            }
            Err(e) => {
                // Deliberately no flush: after a mid-message error the
                // buffered deltas are partial and the reconnected stream may
                // re-serve the message fuller. But Thinking must not stay
                // stuck across the gap (same reasoning as the close branch).
                state.set_working(ctx, false);
                return Err(e);
            }
        };
        line_buf.extend_from_slice(&chunk);
        for line in drain_lines(&mut line_buf) {
            if let Some(payload) = line.strip_prefix("data:") {
                if let Ok(ev) = serde_json::from_str::<Value>(payload.trim()) {
                    state.handle(&ev, ctx).await;
                }
            }
        }
    }
}

/// Drain every complete (`\n`-terminated) line out of `buf`, trailing-trimmed;
/// a trailing partial line stays buffered. Lines are decoded only once
/// complete, so a UTF-8 sequence split across two chunk reads survives intact.
fn drain_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=nl).collect();
        lines.push(String::from_utf8_lossy(&line).trim_end().to_string());
    }
    lines
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
    /// speakable text (reasoning parts are reliably declared); a declared
    /// non-"text" type is skipped at flush.
    part_type: HashMap<String, String>,
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
                        // Present-but-null must read as "not completed": a
                        // nullable-until-set `completed` field would otherwise
                        // flush (and latch) a still-streaming message, dropping
                        // everything that arrives after.
                        let completed = info
                            .get("time")
                            .and_then(|t| t.get("completed"))
                            .is_some_and(|v| !v.is_null());
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
                // Turn over: whatever part state is still buffered belongs to
                // messages that will never flush (user echoes, tool parts).
                // Without this a long-lived session accumulates every part it
                // ever streamed. `assistant`/`flushed` are kept — they hold
                // only message ids (small) and guard against double-speaking.
                self.part_text.clear();
                self.part_snapshot.clear();
                self.part_type.clear();
                self.msg_parts.clear();
            }
            _ => {}
        }
    }

    /// Record a part under its message, preserving first-seen order.
    fn register_part(&mut self, mid: &str, pid: &str) {
        let parts = self.msg_parts.entry(mid.to_string()).or_default();
        if !parts.iter().any(|p| p == pid) {
            parts.push(pid.to_string());
        }
    }

    /// Speak a single assistant message once: concatenate its non-reasoning
    /// parts in order and hand them to TTS. Consumes the message's buffered
    /// part state so it doesn't accumulate across a long session.
    async fn flush(&mut self, mid: &str, ctx: &OobContext) {
        if self.flushed.contains(mid) || !self.assistant.contains(mid) {
            return;
        }
        // No parts registered yet: a completed `message.updated` can be
        // observed before any of the message's parts (joining the stream
        // mid-turn, or reordered delivery). Do NOT latch `flushed` here —
        // the parts may still arrive, and `session.idle`'s flush_all gets a
        // second chance to speak them.
        let Some(parts) = self.msg_parts.remove(mid) else {
            return;
        };
        self.flushed.insert(mid.to_string());
        let mut out = String::new();
        for pid in &parts {
            // Skip any part DECLARED as something other than text (reasoning
            // today; guards future tool/patch/etc. part types too). Unknown
            // (never declared) defaults to speakable text — reasoning parts
            // are reliably declared.
            if self.part_type.get(pid).is_some_and(|t| t != "text") {
                continue;
            }
            let streamed = self.part_text.get(pid).map(String::as_str).unwrap_or("");
            let snapshot = self.part_snapshot.get(pid).map(String::as_str).unwrap_or("");
            // Prefer whichever view is fuller: deltas can be missing entirely
            // (short message ⇒ snapshot only) or partial (stream joined
            // mid-message ⇒ the accumulated deltas hold only the tail while
            // the `message.part.updated` snapshot carries the full text).
            let text = if snapshot.len() > streamed.len() { snapshot } else { streamed };
            if !text.trim().is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        for pid in &parts {
            self.part_text.remove(pid);
            self.part_snapshot.remove(pid);
            self.part_type.remove(pid);
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
            mem: None,
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

    // ── Legacy sweep session 5 regressions ────────────────────────────────

    #[test]
    fn drain_lines_survives_utf8_split_across_chunks() {
        // Regression: the old code lossily decoded each chunk independently,
        // so a multi-byte char straddling a chunk boundary became U+FFFD.
        let line = "data: {\"delta\":\"café ready\"}\n";
        let bytes = line.as_bytes();
        let split = line.find('é').unwrap() + 1; // inside the 2-byte é
        let mut buf = Vec::new();
        buf.extend_from_slice(&bytes[..split]);
        assert!(drain_lines(&mut buf).is_empty(), "no complete line yet");
        buf.extend_from_slice(&bytes[split..]);
        assert_eq!(drain_lines(&mut buf), vec!["data: {\"delta\":\"café ready\"}".to_string()]);
        assert!(buf.is_empty(), "fully drained");
        // A trailing partial line stays buffered.
        buf.extend_from_slice(b"data: {\"a\":1}\ndata: {\"b\"");
        assert_eq!(drain_lines(&mut buf), vec!["data: {\"a\":1}".to_string()]);
        assert_eq!(buf, b"data: {\"b\"");
    }

    #[tokio::test]
    async fn completed_null_is_not_completed() {
        // Regression: `time.completed: null` used to read as "completed",
        // flushing (and latching) a still-streaming message so the rest of
        // its text was permanently dropped.
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{"created":1,"completed":null}}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"p1","field":"text","delta":"First half"}}"#),
            &ctx,
        )
        .await;
        assert!(tts_rx.try_recv().is_err(), "null completed must not flush");
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"p1","field":"text","delta":" and second half."}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{"created":1,"completed":2}}}}"#),
            &ctx,
        )
        .await;
        match tts_rx.try_recv() {
            Ok(TtsRequest::Synthesize { text, .. }) => {
                assert_eq!(text, "First half and second half.")
            }
            other => panic!("expected the full message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn completed_before_parts_is_spoken_once_at_idle() {
        // Regression: a completed `message.updated` observed before any of the
        // message's parts (mid-turn stream join / reordering) used to latch
        // `flushed` with nothing buffered, so the text arriving right after
        // was never spoken.
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{"created":1,"completed":2}}}}"#),
            &ctx,
        )
        .await;
        assert!(tts_rx.try_recv().is_err());
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"p1","field":"text","delta":"Late but here."}}"#),
            &ctx,
        )
        .await;
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;
        match tts_rx.try_recv() {
            Ok(TtsRequest::Synthesize { text, .. }) => assert_eq!(text, "Late but here."),
            other => panic!("expected idle to recover the message, got {other:?}"),
        }
        // And only once.
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;
        assert!(tts_rx.try_recv().is_err(), "must not double-speak");
    }

    #[tokio::test]
    async fn fuller_snapshot_wins_over_partial_deltas() {
        // Regression: non-empty accumulated deltas used to shadow a fuller
        // `message.part.updated` snapshot, speaking only the tail of a
        // message whose early deltas were missed (mid-message reconnect).
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{}}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"p1","field":"text","delta":"world."}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.updated","properties":{"part":{"id":"p1","type":"text","messageID":"m1","text":"Hello world."}}}"#),
            &ctx,
        )
        .await;
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;
        match tts_rx.try_recv() {
            Ok(TtsRequest::Synthesize { text, .. }) => assert_eq!(text, "Hello world."),
            other => panic!("expected the snapshot text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn declared_non_text_part_is_not_spoken() {
        // Forward-compat: a future part type (tool/patch/...) that streams
        // text deltas must not be read aloud — only undeclared parts default
        // to speakable.
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{}}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.updated","properties":{"part":{"id":"p1","type":"tool","messageID":"m1","text":"Running tests"}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"p1","field":"text","delta":"Running tests"}}"#),
            &ctx,
        )
        .await;
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;
        assert!(tts_rx.try_recv().is_err(), "tool part must not be spoken");
    }

    #[tokio::test]
    async fn part_buffers_are_pruned_after_flush_and_idle() {
        // Regression: the Tracker used to keep every part's text/snapshot/type
        // (and every user-message part) for the life of the connection.
        let (ctx, mut tts_rx, _sig) = ctx_with("opencode");
        let mut t = Tracker::default();
        t.handle(
            &ev(r#"{"type":"message.updated","properties":{"info":{"id":"m1","role":"assistant","time":{}}}}"#),
            &ctx,
        )
        .await;
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"m1","partID":"p1","field":"text","delta":"Spoken."}}"#),
            &ctx,
        )
        .await;
        // A user-message echo part that never flushes.
        t.handle(
            &ev(r#"{"type":"message.part.delta","properties":{"messageID":"u1","partID":"pu","field":"text","delta":"my prompt"}}"#),
            &ctx,
        )
        .await;
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx).await;
        assert!(matches!(tts_rx.try_recv(), Ok(TtsRequest::Synthesize { .. })));
        assert!(t.part_text.is_empty(), "part text pruned");
        assert!(t.part_snapshot.is_empty(), "snapshots pruned");
        assert!(t.part_type.is_empty(), "types pruned");
        assert!(t.msg_parts.is_empty(), "part lists pruned");
        assert!(t.flushed.contains("m1"), "double-speak latch kept");
    }

    #[tokio::test]
    async fn stream_close_is_reported_for_reconnect_and_releases_thinking() {
        // Regression twofer against a real (minimal) SSE server:
        //  * a clean server close used to return the same `Ok` as cancellation,
        //    permanently stopping the reconnect loop after one TUI restart;
        //  * it also left the avatar stuck in Thinking (no Stopped signal).
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req).await.unwrap();
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            sock.write_all(
                b"data: {\"type\":\"message.updated\",\"properties\":{\"info\":{\"id\":\"m1\",\"role\":\"assistant\",\"time\":{}}}}\n",
            )
            .await
            .unwrap();
            sock.write_all(
                b"data: {\"type\":\"message.part.delta\",\"properties\":{\"messageID\":\"m1\",\"partID\":\"p1\",\"field\":\"text\",\"delta\":\"Mid-turn text.\"}}\n",
            )
            .await
            .unwrap();
            // Close WITHOUT session.idle: a mid-turn server restart.
            sock.shutdown().await.unwrap();
        });
        let (ctx, mut tts_rx, mut sig) = ctx_with("opencode");
        let client = reqwest::Client::new();
        let end = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            consume(&client, &format!("http://127.0.0.1:{port}/event"), &ctx),
        )
        .await
        .expect("consume must return when the stream closes")
        .expect("clean close is not an error");
        server.await.unwrap();
        assert_eq!(end, StreamEnd::Closed, "close must ask for a reconnect, not read as cancel");
        // Buffered text was flushed on close…
        match tts_rx.try_recv() {
            Ok(TtsRequest::Synthesize { text, .. }) => assert_eq!(text, "Mid-turn text."),
            other => panic!("expected the buffered message, got {other:?}"),
        }
        // …and Thinking was released (Started then Stopped).
        assert!(matches!(sig.try_recv(), Ok(StateSignal::ClaudeOutputStarted { .. })));
        assert!(matches!(sig.try_recv(), Ok(StateSignal::ClaudeOutputStopped { .. })));
    }
}
