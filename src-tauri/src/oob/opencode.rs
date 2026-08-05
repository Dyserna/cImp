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
//! ## Token/usage path (V14 spike C3 → V24 Phase F)
//! This SSE stream carries no token fields: `message.updated`'s
//! `properties.info` on the OOB stream shows only `{id, role, time}` (captured
//! exhaustively live in spike 0a — the vocabulary above is the complete shape),
//! so this consumer, which anyway has neither a `cwd` nor a tool name to record
//! against, adds no usage tap of its own.
//!
//! **V28 correction:** the stream *does* carry the SESSION id — every
//! session-scoped event has `properties.sessionID` (see the spike-0a capture in
//! `docs/spikes/v20/ev.ndjson`); what it lacks is tokens/cwd/tool. That is why
//! [`Tracker::track_live_session`] can bind this tab to its current OpenCode
//! session in the live-session registry, giving OpenCode tabs the same
//! per-tab memory scoping Claude tabs get.
//!
//! The real token path lives elsewhere. **V24 Phase F** (spike-confirmed on
//! OpenCode 1.18.1): the injected plugin's `event` hook forwards each COMPLETED
//! assistant turn's exact token totals — the plugin sees a richer
//! `message.updated.properties.info` (with `tokens`/`cost`/`modelID`) than this
//! SSE does — as a `kind: "usage"` body to
//! [`crate::offload::loopback::handle_memory_event`] (`POST /memory/event`),
//! which records a `UsageEvent::Turn` (rolled up to the parent session with
//! `origin: Agent` when a sub-agent reports). Before that, OpenCode's only
//! usage ingress was that same handler's tool-event arm, which estimates chars
//! from a tool call's INPUT args (its output isn't visible either) — a session
//! with only those and no Phase F turns reads `est_only: true` in
//! [`crate::graph::GraphIndex::usage_all_sessions`] (V24 Phase E derives
//! `est_only` from zero Turn-token totals, not the agent name, so a
//! plugin-reporting session loses the est badge).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, trace, warn};

use super::OobContext;
use crate::offload::service::{valid_meta_key, PushNotice};
use crate::state::StateSignal;

const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// V30 Phase D: the `source` attribute every cImp channel envelope carries —
/// the same string Claude Code renders for the `cimp-offload` MCP server, so a
/// notice reads identically in either agent's transcript.
const PUSH_SOURCE: &str = "cimp-offload";

/// V30 Phase D: per-push HTTP budget. Pushes are best-effort notify-only
/// (milestone invariant 2), so a wedged OpenCode server must never stall this
/// tap's event loop for longer than a blink — and there are no retries.
const PUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// V28 (issue #13): how often this tap refreshes its live-session registry
/// entry. Every session-scoped SSE event carries `properties.sessionID`, and a
/// streaming turn emits token-level deltas by the dozen — re-stamping the
/// registry on each one is pure lock churn. 5 s is far inside the registry's
/// 90 s TTL, and a turn ALWAYS opens with low-frequency events
/// (`session.status` busy / the user `message.updated`) before the assistant can
/// issue an MCP call, so the entry is refreshed well before it could matter.
const LIVE_MARK_INTERVAL_MS: i64 = 5_000;

/// Subscribe to the OpenCode event stream on `port` and drive TTS + avatar
/// state until the tab's cancel token fires. Reconnects on stream errors (the
/// TUI may not have bound the port yet at launch, or may restart its server).
pub async fn run(port: u16, ctx: OobContext) {
    // V28: clear this tab's live-session registry entry on every exit path, so a
    // closed OpenCode tab stops being reported live without waiting out the TTL.
    // Mirrors `claude::LiveSessionGuard`. Only the TAB-keyed entry is dropped —
    // the loopback's separate session-keyed entries (which the Usage "live now"
    // badge reads) are untouched.
    let _live_guard = LiveSessionGuard(&ctx);
    // V30 Phase D: subscribe this tab to the session-push bus for the task's
    // whole lifetime — the guard's `Drop` deregisters on every exit path (tab
    // close, cancel, source end), exactly like `_live_guard` above and like the
    // Claude side's `PushGuard` in `loopback::handle_events`. The RECEIVER is
    // held here, across reconnects, so a notice that lands while the SSE stream
    // is down waits in the bounded queue instead of being lost.
    let (_push_guard, mut push_rx) = match ctx.register_pushes("opencode") {
        Some((g, rx)) => (Some(g), Some(rx)),
        None => (None, None),
    };
    // No request timeout: this is a long-lived stream. (reqwest's default
    // builder sets none; we read with explicit cancel-aware selects instead.)
    // Push POSTs set their own per-request `PUSH_TIMEOUT` on the same client.
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
        match consume(&client, port, &ctx, &mut push_rx).await {
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

/// V28: RAII cleanup of an OpenCode tab's TAB-keyed live-session registry entry
/// (the V24 Claude tap's guard, mirrored). Dropped on every one of [`run`]'s
/// return paths.
struct LiveSessionGuard<'a>(&'a OobContext);

impl Drop for LiveSessionGuard<'_> {
    fn drop(&mut self) {
        self.0.clear_live_session();
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
    port: u16,
    ctx: &OobContext,
    push_rx: &mut Option<mpsc::Receiver<PushNotice>>,
) -> reqwest::Result<StreamEnd> {
    let url = format!("http://127.0.0.1:{port}/event");
    let resp = tokio::select! {
        _ = ctx.cancel.cancelled() => return Ok(StreamEnd::Cancelled),
        r = client.get(&url).send() => r?,
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
            // V30 Phase D: a session push addressed to this tab (or broadcast).
            // Forwarding is an HTTP round trip on the same client, awaited
            // inline — it is bounded by `PUSH_TIMEOUT` and pauses only the SSE
            // read, which reqwest buffers meanwhile. `recv` is cancel-safe, so
            // losing this branch to the chunk branch never drops a notice.
            notice = next_push(push_rx) => {
                forward_push(client, port, ctx, state.current_session(), &notice).await;
                continue;
            }
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

// ── V30 Phase D: session push → OpenCode ─────────────────────────────────────
//
// Claude tabs receive pushes through their per-tab `cimp --offload-mcp` stdio
// child: the app queues a `PushNotice`, the child's `/events` SSE relay turns it
// into a `notifications/claude/channel`, and CLAUDE renders the
// `<channel source="…">` envelope. OpenCode has no inbound MCP path at all (the
// SDK v2 that would have carried one was reverted upstream in 1.18.9), so this
// tap is the delivery mechanism instead: it is already the only thing in the app
// holding a live connection to the tab's OpenCode server AND tracking the tab's
// current MAIN session, which is exactly what a push needs. It builds the SAME
// envelope by hand and POSTs it into the session as a `noReply` message.
//
// Because nothing is negotiated at spawn on this side, the `offload.session_push`
// gate is read LIVE at delivery time (see [`forward_target`]) rather than baked
// into the tab's argv — which is why, unlike the Claude side, toggling the
// setting needs no tab restart and no `spawn_inject_sig` entry.

/// Await the next push, or park forever when this tap has no subscription.
///
/// `mpsc::Receiver::recv` is cancel-safe, so this is safe to lose repeatedly to
/// a competing `select!` branch. A closed queue parks too rather than yielding
/// `None` forever: while the [`PushGuard`](crate::offload::service::PushGuard)
/// lives nothing can close it, and busy-looping a `select!` on a dead branch
/// would be worse than the alternative.
async fn next_push(rx: &mut Option<mpsc::Receiver<PushNotice>>) -> PushNotice {
    match rx.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(notice) => notice,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Why a push wasn't forwarded (the pure half of [`forward_push`]).
#[derive(Debug, PartialEq, Eq)]
enum PushSkip {
    /// `offload.session_push` is off right now.
    Disabled,
    /// This tap hasn't observed a main session id yet (tab just launched, or
    /// the user hasn't started a conversation).
    NoSession,
}

/// Decide whether one push should be forwarded, and to which session.
///
/// `enabled` is read live per push, never cached at spawn: the OpenCode path
/// bakes nothing into the tab's launch (no argv flag, no handshake), so the
/// fanout can be switched on and off mid-session with no tab restart — the
/// mirror image of the Claude side, where `offload.session_push` gates a
/// spawn-time `--dangerously-load-development-channels` flag and therefore
/// carries a `spawn_inject_sig` entry plus a restart hint.
///
/// `session` must be the tab's **main** session (never a sub-agent's) — see
/// [`Tracker::current_session`].
fn forward_target(enabled: bool, session: Option<&str>) -> Result<&str, PushSkip> {
    if !enabled {
        return Err(PushSkip::Disabled);
    }
    session.filter(|s| !s.is_empty()).ok_or(PushSkip::NoSession)
}

/// Escape one channel attribute value for the `<channel …>` tag.
///
/// XML attribute rules: `&` first (or the other escapes get double-escaped),
/// then the delimiters. Control characters (a newline in a `detail` string, say)
/// collapse to a space so the opening tag stays one line, matching how an XML
/// parser would normalize them anyway.
fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Render the model-visible `<channel>` envelope for one notice — the same
/// shape Claude Code paints for a `notifications/claude/channel`
/// (`<channel source="cimp-offload" kind="audit">…</channel>`, verified live in
/// the Phase 0 spike), so a notice reads identically in both agents.
///
/// Meta keys are re-checked against [`valid_meta_key`] even though
/// [`PushNotice::new`] already enforces it: a notice can also arrive by
/// `Deserialize` (the SSE wire type), which bypasses the constructor — validate
/// at the parse boundary anyway. `meta` is a `BTreeMap`, so attribute order is
/// stable across pushes.
fn render_channel_envelope(notice: &PushNotice) -> String {
    let mut out = format!("<channel source=\"{PUSH_SOURCE}\"");
    for (key, value) in &notice.meta {
        if !valid_meta_key(key) {
            continue;
        }
        out.push(' ');
        out.push_str(key);
        out.push_str("=\"");
        out.push_str(&escape_attr(value));
        out.push('"');
    }
    out.push_str(">\n");
    out.push_str(&notice.content);
    out.push_str("\n</channel>");
    out
}

/// The `POST /session/:id/message` body that injects `text` into a session
/// **without** starting a model turn.
fn push_message_body(text: &str) -> Value {
    serde_json::json!({
        "noReply": true,
        "parts": [{ "type": "text", "text": text }],
    })
}

/// Forward one push into the tab's OpenCode session. Best-effort by contract:
/// every failure (gate off, no session yet, HTTP error, timeout, non-2xx) is
/// logged and dropped — never retried, never allowed to break the event loop.
async fn forward_push(
    client: &reqwest::Client,
    port: u16,
    ctx: &OobContext,
    session: Option<&str>,
    notice: &PushNotice,
) {
    let session = match forward_target(ctx.session_push_enabled(), session) {
        Ok(s) => s,
        Err(PushSkip::Disabled) => {
            trace!(tab = ?ctx.tab, "OpenCode push: offload.session_push is off — dropping notice");
            return;
        }
        Err(PushSkip::NoSession) => {
            debug!(
                tab = ?ctx.tab,
                "OpenCode push: no live session on this tab yet — dropping notice (pushes are best-effort)"
            );
            return;
        }
    };
    // Same client, same host, no auth — mirrors the `/event` GET above, which is
    // the only other request cImp makes against a tab's OpenCode server (the TUI
    // binds loopback-only and requires no credentials).
    //
    // RISK (V30 Phase D, unresolved by design): `noReply: true` is
    // source-verified in OpenCode 1.18.13 — it persists the message into the
    // session WITHOUT starting a turn, the documented plugin context-injection
    // mechanism. The version that introduced it is unconfirmed and the OpenCode
    // installed here is 1.18.1. If 1.18.1 predates the field, it is simply
    // ignored (unknown fields usually are) and this POST becomes a normal
    // prompt that STARTS A TURN. There is deliberately no version detection:
    // the milestone's live-verify list carries "confirm a push does not start a
    // turn on the installed OpenCode; if it does, upgrade OpenCode (CD-7)
    // before enabling `offload.session_push`".
    let url = format!("http://127.0.0.1:{port}/session/{session}/message");
    let body = push_message_body(&render_channel_envelope(notice));
    match client
        .post(&url)
        .timeout(PUSH_TIMEOUT)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            debug!(tab = ?ctx.tab, session, "OpenCode push: injected a channel notice")
        }
        Ok(resp) => warn!(
            tab = ?ctx.tab,
            session,
            status = %resp.status(),
            "OpenCode push: server rejected the notice — dropping (no retry)"
        ),
        Err(e) => warn!(
            tab = ?ctx.tab,
            session,
            error = %e,
            "OpenCode push: delivery failed — dropping (no retry)"
        ),
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
    /// V28: session ids observed to be CHILD (sub-agent / task-tool) sessions —
    /// `session.created` with a `properties.info.parentID`. Their events ride
    /// the same stream as the main session's, and binding the tab to one would
    /// scope the tab's memory tools to a sub-agent instead of the conversation
    /// (the milestone's "tab resolves to its current MAIN session" invariant).
    child_sessions: HashSet<String>,
    /// V28: `(session_id, ts_ms)` of the last live-session mark, for the
    /// [`LIVE_MARK_INTERVAL_MS`] throttle. A session CHANGE always marks
    /// immediately (a `/new` rotation must not wait out the interval).
    last_mark: Option<(String, i64)>,
}

impl Tracker {
    async fn handle(&mut self, ev: &Value, ctx: &OobContext) {
        let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let props = ev.get("properties").unwrap_or(&Value::Null);
        // V28: bind this TAB to the session it is currently driving, before the
        // per-event handling below (a `session.idle` still counts as liveness).
        self.track_live_session(kind, props, ctx);
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
                    self.part_text
                        .entry(pid.to_string())
                        .or_default()
                        .push_str(delta);
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

    /// V28 (issue #13) — OpenCode's half of the per-tab session identity.
    ///
    /// Spike verdict (evidence: the V20 spike-0a capture at
    /// `docs/spikes/v20/ev.ndjson`): **every** session-scoped SSE event carries
    /// `properties.sessionID` — `session.created`, `session.status`,
    /// `session.idle`, `message.updated`, `message.part.updated` and
    /// `message.part.delta` all do. (The module doc's older "no cwd/session"
    /// note is about the *token* fields the SSE lacks, not the session id.) So
    /// this per-tab tap can do exactly what `oob/claude.rs:208` does: stamp
    /// `tab id → session id` into the live-session registry, which is what
    /// `/graph_run`'s `tab` lookup resolves against.
    ///
    /// Sub-agent sessions ride the SAME stream, so binding blindly would point
    /// the tab at a task-tool session mid-run. `session.created` announces a
    /// child with `properties.info.parentID` (verified live in the V24 Phase F
    /// spike), so children are recorded and skipped. A child whose `created`
    /// event we missed (tap attached mid-run, or a reconnect reset the tracker)
    /// is marked — a fail-open degradation, never an error.
    fn track_live_session(&mut self, kind: &str, props: &Value, ctx: &OobContext) {
        if let Some(sid) = self.live_session_target(kind, props, crate::activity::now_ms() as i64) {
            ctx.mark_live_session(&sid, "opencode");
        }
    }

    /// The decision half of [`Self::track_live_session`], with an explicit clock
    /// so it is unit-testable: the session id this event should stamp into the
    /// registry, or `None` (no session id, a child session, or throttled).
    /// Mutates the child set and the throttle state, so call it once per event.
    fn live_session_target(&mut self, kind: &str, props: &Value, now: i64) -> Option<String> {
        let sid = props.get("sessionID").and_then(Value::as_str)?;
        if kind == "session.created"
            && props
                .get("info")
                .and_then(|i| i.get("parentID"))
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty())
        {
            self.child_sessions.insert(sid.to_string());
            return None;
        }
        if self.child_sessions.contains(sid) {
            return None;
        }
        // Always mark on a session CHANGE (a `/new` rotation must not wait out
        // the interval), else once per `LIVE_MARK_INTERVAL_MS`.
        if let Some((last_sid, at)) = self.last_mark.as_ref() {
            if last_sid == sid && now.saturating_sub(*at) < LIVE_MARK_INTERVAL_MS {
                return None;
            }
        }
        self.last_mark = Some((sid.to_string(), now));
        Some(sid.to_string())
    }

    /// V30 Phase D: the session a push should be injected into — the last
    /// session this connection bound the tab to.
    ///
    /// This reuses [`Self::last_mark`] rather than tracking a second id, which
    /// is what keeps the invariant honest: `last_mark` is written only by
    /// [`Self::live_session_target`], which **excludes child (sub-agent)
    /// sessions**. So a push landing while a task-tool sub-agent is mid-run
    /// still goes to the tab's MAIN conversation, never into the sub-agent's
    /// session. `None` until the tap has seen its first session-scoped event.
    fn current_session(&self) -> Option<&str> {
        self.last_mark.as_ref().map(|(sid, _)| sid.as_str())
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
            let snapshot = self
                .part_snapshot
                .get(pid)
                .map(String::as_str)
                .unwrap_or("");
            // Prefer whichever view is fuller: deltas can be missing entirely
            // (short message ⇒ snapshot only) or partial (stream joined
            // mid-message ⇒ the accumulated deltas hold only the tail while
            // the `message.part.updated` snapshot carries the full text).
            let text = if snapshot.len() > streamed.len() {
                snapshot
            } else {
                streamed
            };
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

    fn ctx_with(
        tab: &str,
    ) -> (
        OobContext,
        mpsc::Receiver<TtsRequest>,
        mpsc::Receiver<StateSignal>,
    ) {
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
            pushes: None,
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
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;

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
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;
        assert!(
            tts_rx.try_recv().is_err(),
            "reasoning must not be spoken even if declared late"
        );
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
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;
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
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;
        assert!(matches!(
            tts_rx.try_recv(),
            Ok(TtsRequest::Synthesize { .. })
        ));
        // Working state went up then down.
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::ClaudeOutputStarted { .. })
        ));
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::ClaudeOutputStopped { .. })
        ));
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
        assert_eq!(
            drain_lines(&mut buf),
            vec!["data: {\"delta\":\"café ready\"}".to_string()]
        );
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
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;
        match tts_rx.try_recv() {
            Ok(TtsRequest::Synthesize { text, .. }) => assert_eq!(text, "Late but here."),
            other => panic!("expected idle to recover the message, got {other:?}"),
        }
        // And only once.
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;
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
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;
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
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;
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
        t.handle(&ev(r#"{"type":"session.idle","properties":{}}"#), &ctx)
            .await;
        assert!(matches!(
            tts_rx.try_recv(),
            Ok(TtsRequest::Synthesize { .. })
        ));
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
            consume(&client, port, &ctx, &mut None),
        )
        .await
        .expect("consume must return when the stream closes")
        .expect("clean close is not an error");
        server.await.unwrap();
        assert_eq!(
            end,
            StreamEnd::Closed,
            "close must ask for a reconnect, not read as cancel"
        );
        // Buffered text was flushed on close…
        match tts_rx.try_recv() {
            Ok(TtsRequest::Synthesize { text, .. }) => assert_eq!(text, "Mid-turn text."),
            other => panic!("expected the buffered message, got {other:?}"),
        }
        // …and Thinking was released (Started then Stopped).
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::ClaudeOutputStarted { .. })
        ));
        assert!(matches!(
            sig.try_recv(),
            Ok(StateSignal::ClaudeOutputStopped { .. })
        ));
    }
}

#[cfg(test)]
mod live_session_tests {
    //! V28 (issue #13): the OpenCode half of per-tab session identity.
    //!
    //! Payload shapes below are copied from the real spike-0a capture
    //! (`docs/spikes/v20/ev.ndjson`) — `properties.sessionID` is present on
    //! every session-scoped event type, which is what makes this wiring
    //! possible at all.
    use super::{Tracker, LIVE_MARK_INTERVAL_MS};
    use serde_json::Value;

    fn props(s: &str) -> Value {
        serde_json::from_str::<Value>(s).unwrap()["properties"].clone()
    }

    #[test]
    fn every_session_scoped_event_type_yields_the_session_id() {
        // One event of each shape the tap sees; each must be able to refresh the
        // tab→session binding on its own (the tracker is reset between them so
        // the throttle doesn't mask a type that fails to expose the id).
        for raw in [
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"busy"}}}"#,
            r#"{"type":"session.idle","properties":{"sessionID":"ses_1"}}"#,
            r#"{"type":"message.updated","properties":{"sessionID":"ses_1","info":{"id":"msg_1","role":"user","sessionID":"ses_1"}}}"#,
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","part":{"type":"text","messageID":"msg_1","id":"prt_1"}}}"#,
            r#"{"type":"message.part.delta","properties":{"sessionID":"ses_1","messageID":"msg_1","partID":"prt_1","field":"text","delta":"x"}}"#,
        ] {
            let ev: Value = serde_json::from_str(raw).unwrap();
            let kind = ev["type"].as_str().unwrap();
            let mut t = Tracker::default();
            assert_eq!(
                t.live_session_target(kind, &ev["properties"], 0),
                Some("ses_1".to_string()),
                "event {kind} must expose the session id"
            );
        }
    }

    #[test]
    fn an_event_without_a_session_id_marks_nothing() {
        let mut t = Tracker::default();
        assert_eq!(
            t.live_session_target("server.connected", &props(r#"{"properties":{}}"#), 0),
            None
        );
    }

    #[test]
    fn a_session_rotation_marks_immediately_but_repeats_are_throttled() {
        let mut t = Tracker::default();
        let p = props(r#"{"properties":{"sessionID":"ses_1"}}"#);
        assert_eq!(
            t.live_session_target("session.idle", &p, 0),
            Some("ses_1".to_string())
        );
        // Same session inside the interval: no repeat work.
        assert_eq!(
            t.live_session_target("session.idle", &p, LIVE_MARK_INTERVAL_MS - 1),
            None
        );
        // Past the interval it refreshes again (the registry TTL must not lapse).
        assert_eq!(
            t.live_session_target("session.idle", &p, LIVE_MARK_INTERVAL_MS),
            Some("ses_1".to_string())
        );
        // A DIFFERENT session (a `/new` rotation) never waits out the interval —
        // otherwise the tab would keep reporting the session it just left.
        let rotated = props(r#"{"properties":{"sessionID":"ses_2"}}"#);
        assert_eq!(
            t.live_session_target("session.idle", &rotated, LIVE_MARK_INTERVAL_MS),
            Some("ses_2".to_string())
        );
    }

    #[test]
    fn a_child_session_never_binds_the_tab() {
        // Sub-agent (task-tool) sessions ride the SAME stream. Binding the tab to
        // one would scope the tab's memory tools to a sub-agent mid-run; the
        // milestone's contract is the tab's current MAIN session.
        let mut t = Tracker::default();
        let created = props(
            r#"{"properties":{"sessionID":"ses_child","info":{"id":"ses_child","parentID":"ses_main"}}}"#,
        );
        assert_eq!(t.live_session_target("session.created", &created, 0), None);
        // ...and none of the child's subsequent events bind it either.
        let child_delta = props(r#"{"properties":{"sessionID":"ses_child"}}"#);
        assert_eq!(
            t.live_session_target("message.part.delta", &child_delta, 1_000),
            None
        );
        // The parent still binds normally.
        let parent = props(r#"{"properties":{"sessionID":"ses_main"}}"#);
        assert_eq!(
            t.live_session_target("session.idle", &parent, 2_000),
            Some("ses_main".to_string())
        );
    }

    #[test]
    fn a_top_level_session_created_binds_the_tab() {
        // The real capture's `session.created` has an `info` with NO `parentID` —
        // that is a main session and must bind (a missing/blank parent is not a
        // reason to skip).
        let mut t = Tracker::default();
        let created = props(
            r#"{"properties":{"sessionID":"ses_top","info":{"id":"ses_top","slug":"quiet-comet"}}}"#,
        );
        assert_eq!(
            t.live_session_target("session.created", &created, 0),
            Some("ses_top".to_string())
        );
        let blank = props(
            r#"{"properties":{"sessionID":"ses_two","info":{"id":"ses_two","parentID":""}}}"#,
        );
        assert_eq!(
            t.live_session_target("session.created", &blank, 1_000),
            Some("ses_two".to_string())
        );
    }
}

#[cfg(test)]
mod push_tests {
    //! V30 Phase D: session push → OpenCode.
    //!
    //! The envelope, the request body and the forward/skip decision are pure, so
    //! they're pinned here without a socket; the one socket test pins the wire
    //! contract a comment alone can't defend (method, path, `noReply`).
    use super::*;
    use crate::settings::{Settings, SettingsHandle};
    use crate::state::TabId;
    use tokio_util::sync::CancellationToken;

    fn notice<'a>(content: &str, meta: impl IntoIterator<Item = (&'a str, &'a str)>) -> PushNotice {
        PushNotice::new(content, meta)
    }

    /// A tap context whose `offload.session_push` is `enabled`.
    fn ctx_with_push(enabled: bool) -> OobContext {
        let (tts_tx, _tts_rx) = mpsc::channel(4);
        let (sig_tx, _sig_rx) = mpsc::channel(4);
        let mut defaults = Settings::default();
        defaults.tabs.push(crate::settings::default_opencode_tab());
        defaults.offload.session_push = enabled;
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());
        OobContext {
            tab: TabId::from_str("opencode"),
            tts: tts_tx,
            state_signals: sig_tx,
            settings,
            cancel: CancellationToken::new(),
            mem: None,
            pushes: None,
        }
    }

    // ── envelope ────────────────────────────────────────────────────────────

    #[test]
    fn envelope_matches_the_claude_side_shape() {
        // The exact rendering Claude Code paints for a
        // `notifications/claude/channel` (Phase 0 spike, T2): source attribute
        // first, meta after, content on its own line.
        let n = notice("Graph index rebuilt.", [("kind", "graph_index")]);
        assert_eq!(
            render_channel_envelope(&n),
            "<channel source=\"cimp-offload\" kind=\"graph_index\">\nGraph index rebuilt.\n</channel>"
        );
    }

    #[test]
    fn envelope_without_meta_still_carries_the_source() {
        let n = notice("bare", [] as [(&str, &str); 0]);
        assert_eq!(
            render_channel_envelope(&n),
            "<channel source=\"cimp-offload\">\nbare\n</channel>"
        );
    }

    #[test]
    fn envelope_attribute_order_is_stable() {
        // `meta` is a BTreeMap so the same notice renders byte-identically
        // whatever order the producer inserted keys in — a push is user-visible
        // transcript text; a wobbling attribute order would be diff noise.
        let a = notice("x", [("zulu", "1"), ("alpha", "2"), ("mike", "3")]);
        let b = notice("x", [("mike", "3"), ("zulu", "1"), ("alpha", "2")]);
        assert_eq!(render_channel_envelope(&a), render_channel_envelope(&b));
        assert_eq!(
            render_channel_envelope(&a),
            "<channel source=\"cimp-offload\" alpha=\"2\" mike=\"3\" zulu=\"1\">\nx\n</channel>"
        );
    }

    #[test]
    fn envelope_escapes_attribute_values() {
        let n = notice("body", [("detail", r#"a & b < c > d "quoted""#)]);
        assert_eq!(
            render_channel_envelope(&n),
            "<channel source=\"cimp-offload\" detail=\"a &amp; b &lt; c &gt; d &quot;quoted&quot;\">\nbody\n</channel>"
        );
    }

    #[test]
    fn envelope_attribute_values_stay_on_one_line() {
        // A multi-line `detail` must not break the opening tag.
        let n = notice("body", [("detail", "line one\nline two\ttabbed")]);
        let rendered = render_channel_envelope(&n);
        let open = rendered.split_once('\n').unwrap().0;
        assert_eq!(
            open,
            "<channel source=\"cimp-offload\" detail=\"line one line two tabbed\">"
        );
    }

    #[test]
    fn envelope_drops_meta_keys_the_client_would_reject() {
        // `PushNotice::new` already filters, but a notice can also arrive by
        // `Deserialize` (the SSE wire type), which bypasses the constructor —
        // so the renderer re-checks at its own boundary.
        let deserialized: PushNotice = serde_json::from_str(
            r#"{"content":"c","meta":{"ok_key":"1","bad-key":"2","9nope":"3"}}"#,
        )
        .unwrap();
        assert_eq!(
            render_channel_envelope(&deserialized),
            "<channel source=\"cimp-offload\" ok_key=\"1\">\nc\n</channel>"
        );
    }

    #[test]
    fn envelope_content_is_not_escaped() {
        // Claude renders the content verbatim inside the tag; the OpenCode
        // envelope must read identically, so content is passed through.
        let n = notice("see <file.rs> & run", [] as [(&str, &str); 0]);
        assert!(render_channel_envelope(&n).contains("see <file.rs> & run"));
    }

    // ── request body ────────────────────────────────────────────────────────

    #[test]
    fn message_body_is_a_no_reply_text_part() {
        // `noReply` is the whole point: it persists the message into the session
        // WITHOUT starting a model turn.
        let body = push_message_body("hello");
        assert_eq!(
            body,
            serde_json::json!({
                "noReply": true,
                "parts": [{ "type": "text", "text": "hello" }],
            })
        );
    }

    // ── forward decision ────────────────────────────────────────────────────

    #[test]
    fn a_disabled_gate_drops_the_push() {
        assert_eq!(
            forward_target(false, Some("ses_1")),
            Err(PushSkip::Disabled)
        );
    }

    #[test]
    fn no_session_yet_drops_the_push() {
        assert_eq!(forward_target(true, None), Err(PushSkip::NoSession));
        assert_eq!(forward_target(true, Some("")), Err(PushSkip::NoSession));
    }

    #[test]
    fn an_enabled_gate_with_a_session_forwards() {
        assert_eq!(forward_target(true, Some("ses_1")), Ok("ses_1"));
    }

    // ── session resolution ──────────────────────────────────────────────────

    #[test]
    fn the_push_target_is_the_main_session_never_a_sub_agent() {
        // Sub-agent sessions ride the same SSE stream. A push landing while a
        // task-tool sub-agent runs must still go to the tab's conversation.
        let mut t = Tracker::default();
        assert_eq!(t.current_session(), None, "nothing seen yet");
        let main: Value =
            serde_json::from_str(r#"{"sessionID":"ses_main","info":{"id":"ses_main"}}"#).unwrap();
        t.live_session_target("session.created", &main, 0);
        assert_eq!(t.current_session(), Some("ses_main"));

        let child: Value = serde_json::from_str(
            r#"{"sessionID":"ses_child","info":{"id":"ses_child","parentID":"ses_main"}}"#,
        )
        .unwrap();
        t.live_session_target("session.created", &child, 1_000);
        let child_event: Value = serde_json::from_str(r#"{"sessionID":"ses_child"}"#).unwrap();
        t.live_session_target("message.part.delta", &child_event, 2_000);
        assert_eq!(
            t.current_session(),
            Some("ses_main"),
            "a sub-agent session must never become the push target"
        );

        // A `/new` rotation does move it.
        let rotated: Value = serde_json::from_str(r#"{"sessionID":"ses_two"}"#).unwrap();
        t.live_session_target("session.idle", &rotated, 3_000);
        assert_eq!(t.current_session(), Some("ses_two"));
    }

    // ── wire contract (one socket test) ─────────────────────────────────────

    #[tokio::test]
    async fn forward_push_posts_the_envelope_to_the_session_message_endpoint() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read head + body (content-length framed).
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
            }
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                .await
                .unwrap();
            let _ = sock.flush().await;
            String::from_utf8_lossy(&buf).to_string()
        });

        let ctx = ctx_with_push(true);
        let client = reqwest::Client::new();
        forward_push(
            &client,
            port,
            &ctx,
            Some("ses_abc"),
            &notice("Audit finished: 3 findings.", [("kind", "audit")]),
        )
        .await;
        let raw = server.await.unwrap();

        let (head, body) = raw.split_once("\r\n\r\n").expect("a complete request");
        assert!(
            head.starts_with("POST /session/ses_abc/message "),
            "endpoint drift: {head}"
        );
        let json: Value = serde_json::from_str(body).expect("a JSON body");
        assert_eq!(
            json["noReply"],
            serde_json::json!(true),
            "must not start a turn"
        );
        assert_eq!(json["parts"][0]["type"], serde_json::json!("text"));
        assert_eq!(
            json["parts"][0]["text"],
            serde_json::json!(
                "<channel source=\"cimp-offload\" kind=\"audit\">\nAudit finished: 3 findings.\n</channel>"
            )
        );
    }

    #[tokio::test]
    async fn a_dead_server_is_logged_and_dropped_not_retried() {
        // Best-effort by contract: an unreachable OpenCode must not hang or
        // panic the tap. Bind then drop, so the port is (almost certainly) free.
        let port = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let ctx = ctx_with_push(true);
        let client = reqwest::Client::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            forward_push(
                &client,
                port,
                &ctx,
                Some("ses_abc"),
                &notice("x", [] as [(&str, &str); 0]),
            ),
        )
        .await
        .expect("a failed push must return promptly");
    }
}
