//! **OpenCode's L1 canary** — the fixture-backed substantiveness check
//! `harness/canary.rs`'s neutral runner drives through
//! [`HarnessPlugin::canaries`](crate::harness::plugin::HarnessPlugin::canaries).
//!
//! V40 Phase A, locked decision 17. See [`crate::harness::canary`]'s module docs
//! for the *why*; moved verbatim, same fixture and same negative twin.

use serde_json::Value;

use crate::harness::canary::{block_on_current_thread, parse_lines, substantive};
use crate::harness::plugin::Canary;

// ── the embedded corpus (V35 Phase F) ───────────────────────────────────────

const FIXTURE_OPENCODE_SSE: &str =
    include_str!("../../../fixtures/harness/opencode/1.18.13/sse.assistant-turn.jsonl");

/// This fixture's `<harness>/<version>/<name>` path under
/// `src-tauri/fixtures/harness/` — see the Claude module for why it is declared
/// beside the embedded bytes.
const PATH_SSE: &str = "opencode/1.18.13/sse.assistant-turn.jsonl";

/// What [`crate::harness::opencode::PLUGIN`] declares to the runner.
///
/// **The one canary whose reader is `async`**, so its `run` parks a private
/// current-thread runtime through [`block_on_current_thread`] — which is why
/// [`crate::harness::canary::run_embedded`] must never be called from inside an
/// async context. The `#[tokio::test]` twin awaits
/// [`opencode_sse_events`] directly instead.
pub const CANARIES: &[Canary] = &[Canary {
    id: "opencode.sse.events",
    fixture: FIXTURE_OPENCODE_SSE,
    fixture_path: PATH_SSE,
    run: |raw| block_on_current_thread(check_opencode_sse_events(raw)),
}];

// ── opencode.sse.events ─────────────────────────────────────────────────────

/// `harness/opencode/read.rs::Tracker::handle` still turns one turn's SSE envelopes into
/// spoken assistant text and still binds the tab to the session.
///
/// Driven as an ordered stream rather than as isolated events, because
/// `Tracker` is a state machine: `message.updated` declares the message
/// assistant, `message.part.updated` types the part, `message.part.delta`
/// accumulates the text, and only the completed `message.updated` flushes.
/// Anything less than the whole sequence cannot show that text still comes out
/// the other end.
pub async fn check_opencode_sse_events(raw: &str) -> Result<(), String> {
    let events = parse_lines(raw)?;
    substantive!(
        events.len() >= 4,
        "fixture guard: the turn needs message.updated + part.updated + part.delta + a completed \
         message.updated to prove anything"
    );

    // What the fixture says should come out: the concatenated deltas, and the
    // session id every session-scoped event carries.
    let expected_text: String = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("message.part.delta"))
        .filter_map(|e| {
            e.get("properties")
                .and_then(|p| p.get("delta"))
                .and_then(Value::as_str)
        })
        .collect();
    substantive!(
        !expected_text.trim().is_empty(),
        "fixture guard: the fixture must carry non-empty delta text"
    );
    let Some(expected_session) = events[0]
        .get("properties")
        .and_then(|p| p.get("sessionID"))
        .and_then(Value::as_str)
    else {
        return Err(
            "fixture guard: every session-scoped event carries properties.sessionID".to_string(),
        );
    };

    let (ctx, mut tts_rx, _signals) = opencode_ctx();
    let mut tracker = crate::harness::opencode::read::Tracker::default();
    for ev in &events {
        tracker.handle(ev, &ctx).await;
    }

    match tts_rx.try_recv() {
        Ok(crate::tts::TtsRequest::Synthesize { text, .. }) => {
            // Non-empty is the substantiveness check; equality additionally
            // proves the delta path (not just the `message.part.updated`
            // snapshot) is still wired. A missing `properties.info.id` or
            // `properties.part.messageID` shows up here too: the flush is keyed
            // by message id, so losing it produces silence, not an error.
            substantive!(!text.trim().is_empty(), "spoken text is empty");
            substantive!(
                text == expected_text,
                "opencode.sse.events: the assistant text no longer survives the stream — check \
                 properties.part.messageID / properties.partID / properties.delta"
            );
        }
        other => {
            return Err(format!(
                "opencode.sse.events: a completed assistant message produced no speech ({other:?}) \
                 — something in the chain moved: `message.updated` / `properties.info.role` / \
                 `properties.info.time.completed` (no flush), or `properties.part.messageID` / \
                 `properties.messageID` / `properties.partID` (nothing registered under the \
                 message)"
            ))
        }
    }

    substantive!(
        tracker.current_session().as_deref() == Some(expected_session),
        "opencode.sse.events: properties.sessionID no longer binds the tab to its session (V28 \
         per-tab identity, and the V30 push target)"
    );
    Ok(())
}

/// A tap context wired to the built-in OpenCode tab, so the per-tab TTS gate is
/// satisfied and `ctx.speak` actually delivers. Mirrors `oob::opencode`'s own
/// `ctx_with`; kept local rather than hoisted so this module can be read (and
/// moved, in Phase K) on its own.
fn opencode_ctx() -> (
    crate::harness::OobContext,
    tokio::sync::mpsc::Receiver<crate::tts::TtsRequest>,
    tokio::sync::mpsc::Receiver<crate::state::StateSignal>,
) {
    let (tts_tx, tts_rx) = tokio::sync::mpsc::channel(64);
    let (sig_tx, sig_rx) = tokio::sync::mpsc::channel(64);
    // `Settings::default()` ships no tabs (the real app seeds them from
    // persistence), and an unknown tab speaks nothing.
    let mut defaults = crate::settings::Settings::default();
    defaults.tabs.push(crate::settings::default_opencode_tab());
    let settings = crate::settings::SettingsHandle::new(
        defaults.clone(),
        defaults,
        std::env::temp_dir(),
    );
    let ctx = crate::harness::OobContext {
        tab: crate::state::TabId::from_str(crate::settings::OPENCODE_TAB_ID),
        tts: tts_tx,
        state_signals: sig_tx,
        settings,
        cancel: tokio_util::sync::CancellationToken::new(),
        mem: None,
        pushes: None,
    };
    (ctx, tts_rx, sig_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::canary::support::{fixture, json_lines, row};

    #[tokio::test]
    async fn canary_opencode_sse_events() {
        row("opencode.sse.events");
        // Awaited directly rather than through `run_embedded`, which parks its
        // own runtime and must not be called from an async context.
        check_opencode_sse_events(FIXTURE_OPENCODE_SSE)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
    }
    /// Negative twin: `properties.partID` renamed to `partId` on the
    /// `message.part.delta` event.
    ///
    /// `Tracker::handle` destructures `partID`/`messageID`/`delta` as one tuple, so
    /// the delta is dropped whole; the part's only other text source is the empty
    /// `message.part.updated` snapshot, and `flush` speaks nothing when the joined
    /// text is blank. Result: the turn completes, the tab stays bound to its
    /// session, `session.idle` arrives — and the assistant's answer is never
    /// spoken. No error, no log, no unknown-event branch: the reader `match`es on
    /// the event `type`, which did not change.
    #[tokio::test]
    async fn negative_canary_opencode_sse_events() {
        row("opencode.sse.events");

        let raw = fixture("opencode/_synthetic/sse-renamed-part-id.jsonl");
        let events = json_lines(&raw);
        assert!(events.len() >= 4, "fixture guard: the whole turn must be present");

        let (ctx, mut tts_rx, _signals) = opencode_ctx();
        let mut tracker = crate::harness::opencode::read::Tracker::default();
        for ev in &events {
            tracker.handle(ev, &ctx).await;
        }

        assert!(
            tts_rx.try_recv().is_err(),
            "guard: this fixture models the drift case — a renamed `partID` must produce SILENCE. \
             Speech here means the reader grew an alias (or started falling back to the part \
             snapshot) and the positive canary can no longer detect this rename."
        );
        // Everything else about the stream still worked, which is exactly why this
        // degradation is invisible in production: the tab looks live and bound.
        assert_eq!(
            tracker.current_session().as_deref(),
            Some("ses_canary_main_0001"),
            "guard: only `partID` was renamed — the session binding must survive"
        );

        assert!(
            check_opencode_sse_events(&raw).await.is_err(),
            "the runtime canary must FAIL on the drift model (V35 Phase F)"
        );
    }
}
