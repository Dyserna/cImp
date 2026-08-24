//! `GET /events` (the SSE stream), `GET /status` (the V32 Phase B debug view)
//! and the injection-status projection the Settings panel reads.
//!
//! One of the route families V42 R4 (#115) split out of `loopback.rs`.

use super::*;

/// `GET /status`: the proxy's V32 Phase B debug view — one row per tab it has
/// served, with that tab's resolved session and latch state
/// ([`Latch::label`]). Read by hand (and by the live-verification recipes) to
/// answer "why is this tab being refused?" without turning on trace logging.
///
/// Behind the same bearer token as every other route; it exposes no fetched
/// content, only cImp's own identifiers and three fixed labels.
/// V32 Phase G (locked decision 16) adds the `injection` object: the RESOLVED
/// value of every control at every scope, and which of the three levels decided
/// it. With three levels, "why is this tab not latching?" has to be answerable
/// without reading code — and `/status` is where the live-verification recipes
/// already look.
pub(super) async fn handle_status(stream: &mut TcpStream, ctx: &RouteCtx) -> AppResult<()> {
    write_json(
        stream,
        200,
        &serde_json::json!({
            // Step 4: through `latch_snapshot`, so a hand-run `/status` and the
            // UI's badge poll see the same freshness rule rather than two.
            "latches": latch_snapshot(ctx.app()),
            "injection": injection_status(&ctx.settings()),
        }),
    )
    .await
}

/// The `/status` + `latch_status` introspection view of the enable hierarchy:
/// the master switch, whether protection is reduced anywhere, and one row per
/// scope naming every feature's resolved value and deciding level.
///
/// Scopes reported, always all of them: `app` (the app-wide controls), the
/// `offload-worker` pseudo-scope, and every configured AI tab. Reporting a scope
/// even when nothing is overridden there is the point — the question this
/// answers is "what is in force", and an absent row reads as "off" to exactly
/// the user who is trying to find out.
///
/// **#48 F-35 — the `app` row reports [`Scope::UnknownCaller`], not
/// [`Scope::AppWide`], and that is deliberate for now.** Locked decision 36
/// split one `Scope::App` into those two; this row's label says
/// *"Application-wide"* while its numbers are the identity-less caller's — the
/// app-wide baseline PLUS any configured tab's L3 `On` (N-1). That mismatch is
/// pre-existing rather than introduced here: it is exactly what the row has
/// always published, it is what live-verify recipe *"an identity-less call
/// honours a per-tab `On`"* observes through `/status` (the app row reading
/// `decided_by:"scope"` while its own `override_value` is `"inherit"`), and
/// moving it to `AppWide` would change `GET /status` JSON and take that recipe's
/// only observation point away. Repointing it is a behaviour change with its own
/// retest box and is raised as **F-38**, not folded into the split.
///
/// [`Scope::UnknownCaller`]: crate::settings::injection::Scope::UnknownCaller
/// [`Scope::AppWide`]: crate::settings::injection::Scope::AppWide
pub fn injection_status(settings: &crate::settings::Settings) -> serde_json::Value {
    use crate::settings::injection::{self as inj, Scope};
    let mut scopes = vec![
        serde_json::json!({
            "scope": Scope::UnknownCaller.key(),
            "label": "Application-wide",
            "features": inj::report(settings, Scope::UnknownCaller),
        }),
        serde_json::json!({
            "scope": Scope::OffloadWorker.key(),
            "label": "Offload worker",
            "features": inj::report(settings, Scope::OffloadWorker),
        }),
    ];
    for t in &settings.tabs {
        if let crate::settings::TabConfig::AiTool(c) = t {
            let scope = Scope::tab_only(&c.id);
            scopes.push(serde_json::json!({
                "scope": c.id,
                "label": c.name,
                "features": inj::report(settings, scope),
            }));
        }
    }
    serde_json::json!({
        "protection": inj::master_enabled(settings),
        "reduced": inj::protection_reduced(settings),
        "scopes": scopes,
    })
}

/// Render one `event: push` SSE frame from a [`PushNotice`].
///
/// The frame grammar is the SSE minimum the child's parser understands:
/// `event: push\ndata: <one-line JSON>\n\n`. `serde_json` escapes every control
/// character, so the payload is *guaranteed* to be a single line however
/// multi-line the pushed content is — the one-line invariant the wire format
/// depends on is enforced by the encoder, not by the caller. Pure, so the shape
/// is unit-testable without a socket — including from the child's side of the
/// wire (`mcp::tests`), which pins encoder and decoder against each other.
pub(in crate::offload) fn push_frame(notice: &PushNotice) -> Vec<u8> {
    let data =
        serde_json::to_string(notice).unwrap_or_else(|_| r#"{"content":"","meta":{}}"#.to_string());
    format!("event: push\ndata: {data}\n\n").into_bytes()
}

/// `GET /events`: an SSE stream carrying two event types to one per-tab
/// `--offload-mcp` child —
///
/// - `event: change` — the pre-V30 capability pulse, sent to EVERY subscriber
///   (semantics unchanged; the child relays it as `tools/list_changed`);
/// - `event: push` — V30 Phase B, sent only to subscribers a push is addressed
///   to, carrying the semantic [`PushNotice`] payload the child wraps into
///   `notifications/claude/channel`.
///
/// Periodic keep-alive comments (every 20 s) keep idle intermediaries — and the
/// child's own 60 s read-idle watchdog — from dropping the connection.
///
/// The subscriber's identity comes from the child's query params
/// (`?tab=&consumer=&channels=`); `channels=1` means the child ACTUALLY declared
/// the capability on its handshake, not that the setting is on. Registration
/// happens after auth (the caller's job) and is undone by
/// [`PushGuard`](super::service::PushGuard)'s `Drop` when this loop exits — for
/// any reason at all.
pub(super) async fn handle_events(
    mut stream: TcpStream,
    service: Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\r\n";
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| AppError::Offload(format!("events head: {e}")))?;
    // Prime the stream so the child's reader unblocks immediately.
    let _ = stream.write_all(b": connected\n\n").await;
    let _ = stream.flush().await;

    // V30 Phase B: register this child in the instance's push registry. The
    // guard is bound for the whole loop — dropping it (on ANY exit below, or on
    // task cancellation) is the sole deregistration path.
    let tab = query_param(&req.path, "tab")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let consumer = query_param(&req.path, "consumer")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(crate::harness::DEFAULT_HARNESS.token())
        .to_string();
    // Anything but an explicit affirmative means "no channels" — a pre-V30
    // child sends no `channels` param at all and must never be pushed to.
    let channels = matches!(query_param(&req.path, "channels"), Some("1") | Some("true"));
    debug!(
        tab = ?tab,
        consumer = %consumer,
        channels,
        "offload loopback: /events subscriber connected"
    );
    let (_push_guard, mut push_rx) = service.register_push_subscriber(tab, consumer, channels);

    let mut rx = service.subscribe_changes();
    loop {
        let tick = tokio::time::sleep(Duration::from_secs(20));
        tokio::select! {
            // V30 Phase B: an addressed push for THIS child.
            notice = push_rx.recv() => {
                match notice {
                    Some(n) => {
                        if stream.write_all(&push_frame(&n)).await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    // Unreachable while `_push_guard` lives (it owns the only
                    // path that removes our sender), but a closed queue can
                    // never yield another notice — stop selecting on it.
                    None => break,
                }
            }
            recv = rx.recv() => {
                match recv {
                    Ok(()) => {
                        if stream.write_all(b"event: change\ndata: {}\n\n").await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    // Lagged: still emit one change so the child re-syncs.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if stream.write_all(b"event: change\ndata: {}\n\n").await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    Err(_) => break, // sender dropped
                }
            }
            _ = tick => {
                if stream.write_all(b": keep-alive\n\n").await.is_err() {
                    break;
                }
                let _ = stream.flush().await;
            }
        }
    }
    Ok(())
}
