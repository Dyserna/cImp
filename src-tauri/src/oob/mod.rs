//! V20: out-of-band TTS sources for fullscreen AI tabs.
//!
//! Before V20, cImp forced its AI tools into an inline renderer so the
//! processing layer could scrape `[[TTS]]` markers out of the linear terminal
//! stream. V20 runs both tools in their native fullscreen (alternate-screen)
//! TUIs, where screen-scraping is impossible — so the speakable text is read
//! from each tool's *structured* side channel instead:
//!
//!   * **Claude Code** appends a transcript JSONL under
//!     `~/.claude/projects/<slug>/<id>.jsonl`; assistant `text` blocks are
//!     written complete at message finish ([`claude`]).
//!   * **OpenCode** exposes an SSE event stream at `GET /event` on the same
//!     port the TUI is launched with (`--port`); assistant text arrives as
//!     token-level `message.part.delta` events ([`opencode`]).
//!
//! Both adapters convert assistant prose to sentence segments (reusing
//! [`crate::processing::segment_sentences`]) and push them onto the shared
//! [`TtsRequest`] channel — the exact same channel the old scrape path fed, so
//! the segmenter, synthesizer, active-tab filter, and Esc-suppression all work
//! unchanged. The source is the only thing that changed.
//!
//! Each source runs as a task tied to the tab's PTY [`CancellationToken`]: it
//! starts when the AI tab spawns and stops when the tab's process exits.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::graph::GraphService;
use crate::settings::{SettingsHandle, TabConfig};
use crate::state::{StateSignal, TabId};
use crate::tts::TtsRequest;

pub mod claude;
pub mod opencode;
pub mod prose;

/// Describes which out-of-band source (if any) a tab's launch should attach.
/// Resolved by `tabs::config` at launch time and carried on the `PtyLaunchSpec`
/// so `PtyManager::start` can spawn the matching adapter once the child is up.
#[derive(Debug, Clone)]
pub enum OobSpec {
    /// Tail Claude Code's transcript JSONL for the project rooted at this dir.
    ClaudeTranscript { project_dir: PathBuf },
    /// Subscribe to an OpenCode TUI's event stream on this loopback port (the
    /// `--port` cImp launched the TUI with).
    OpenCodeEvent { port: u16 },
}

/// Everything an out-of-band adapter needs to feed TTS + avatar state for one
/// tab. Cloned from the per-tab launch context in `PtyManager::start`.
#[derive(Clone)]
pub struct OobContext {
    pub tab: TabId,
    pub tts: tokio::sync::mpsc::Sender<TtsRequest>,
    pub state_signals: tokio::sync::mpsc::Sender<StateSignal>,
    pub settings: SettingsHandle,
    pub cancel: CancellationToken,
    /// V10: the warm graph service, so the Claude transcript tap can record
    /// session/action memory in-process. `None` when the graph feature isn't
    /// wired (tests, or a build without a GraphService in managed state).
    pub mem: Option<Arc<GraphService>>,
    /// V30 Phase D: the session-push bus, so a tap can subscribe its tab to
    /// `push_to_tab`/`push_broadcast` **in-process** and deliver the notice over
    /// its agent's own transport. Only [`opencode`] uses it today — Claude tabs
    /// are served by the stdio child's `/events` SSE relay, which registers on
    /// the same registry from the loopback side. `None` when the offload service
    /// isn't in managed state (tests, headless builds) ⇒ feature absent, zero
    /// behaviour change.
    pub pushes: Option<Arc<crate::offload::service::PushRegistry>>,
}

/// Spawn the adapter described by `spec`, tied to `ctx.cancel`. Non-blocking:
/// returns immediately, the adapter runs until the token is cancelled (tab
/// exit) or the source ends.
pub fn spawn(spec: OobSpec, ctx: OobContext) {
    match spec {
        OobSpec::ClaudeTranscript { project_dir } => {
            debug!(tab = ?ctx.tab, ?project_dir, "spawning Claude transcript OOB source");
            tauri::async_runtime::spawn(claude::run(project_dir, ctx));
        }
        OobSpec::OpenCodeEvent { port } => {
            debug!(tab = ?ctx.tab, port, "spawning OpenCode event OOB source");
            tauri::async_runtime::spawn(opencode::run(port, ctx));
        }
    }
}

impl OobContext {
    /// Whether this tab should speak its assistant output. Reuses the per-tab
    /// `tts_injection.enabled` toggle (V20 repurposes it from "inject the
    /// `[[TTS]]` markup convention" to "speak this tab's assistant prose"; the
    /// markup convention itself is retired with the scrape path). Read live so
    /// a settings toggle takes effect without a relaunch.
    pub fn tts_enabled(&self) -> bool {
        matches!(
            self.settings.current().find_tab(self.tab.as_str()),
            Some(TabConfig::AiTool(c)) if c.tts_injection.enabled
        )
    }

    /// Segment `text` into sentences and push each onto the TTS channel as a
    /// suppressible `Synthesize` request (so Esc/`tts_stop` cuts the rest of
    /// the burst, exactly like the old scrape path). Markdown is reduced to
    /// speakable prose first; empty/code-only input speaks nothing.
    pub async fn speak(&self, text: &str) {
        if !self.tts_enabled() {
            return;
        }
        let prose = prose::to_speakable(text);
        if prose.trim().is_empty() {
            return;
        }
        for sentence in crate::processing::segment_sentences(&prose) {
            // Re-check the toggle per sentence so switching TTS off mid-burst
            // cuts the rest of a long message (the doc above promises a live
            // read), and race the bounded send against the cancel token so a
            // closing tab isn't held hostage by a backed-up TTS channel.
            if self.cancel.is_cancelled() || !self.tts_enabled() {
                return;
            }
            let send = self.tts.send(TtsRequest::Synthesize {
                tab: self.tab.clone(),
                text: sentence,
                suppressible: true,
            });
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                // Bounded channel; if the worker is backed up, awaiting applies
                // natural backpressure rather than dropping speech.
                res = send => {
                    if res.is_err() {
                        return; // worker gone — stop feeding.
                    }
                }
            }
        }
    }

    /// Emit a state signal, ignoring a full/closed channel (state is
    /// edge-triggered and best-effort, matching the PTY processor's `try_send`).
    pub fn signal(&self, sig: StateSignal) {
        let _ = self.state_signals.try_send(sig);
    }

    /// V10: record one session/action memory event via the graph service. A
    /// no-op when memory isn't wired (`mem` is `None`) or the graph is disabled
    /// (the service gates internally). Best-effort — never blocks or errors the
    /// tap.
    #[allow(clippy::too_many_arguments)]
    pub fn record_mem(
        &self,
        root: &std::path::Path,
        session_id: &str,
        agent: &str,
        kind: &str,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        detail: Option<&str>,
    ) {
        if let Some(mem) = self.mem.as_ref() {
            mem.record_mem_event(root, session_id, agent, kind, path, symbol, line, detail);
        }
    }

    /// Session→commit provenance: record one git commit caught from the
    /// transcript (see `claude::record_commit_events`) — a no-op when memory
    /// isn't wired or the graph is disabled (mirrors [`Self::record_mem`]).
    pub fn record_commit(&self, root: &std::path::Path, session_id: &str, hash: &str) {
        if let Some(mem) = self.mem.as_ref() {
            mem.record_session_commit(root, session_id, hash);
        }
    }

    /// V16 Feature 4: test a Bash command against this session's recent
    /// read-advisor reminders (shell reads of a just-reminded file are the
    /// advisor's blind spot) — a no-op when memory isn't wired or the
    /// advisor is off (the service gates internally). Best-effort like the
    /// other tap recorders.
    pub fn check_bypass(&self, root: &std::path::Path, session_id: &str, command: &str) {
        if let Some(mem) = self.mem.as_ref() {
            mem.check_bypass(root, session_id, command);
        }
    }

    /// V16 review fix: a genuine user prompt is a turn boundary for the read
    /// advisor's trust-TTL and compounding clocks when context injection is
    /// off (the service gates internally — see
    /// `GraphService::note_user_turn`). A no-op when memory isn't wired.
    pub fn note_user_turn(&self, session_id: &str) {
        if let Some(mem) = self.mem.as_ref() {
            mem.note_user_turn(session_id);
        }
    }

    /// V24 Phase B: mark this tab's session live in the graph's live-session
    /// registry, keyed by the stable tab id (so a session rotation on the same
    /// tab doesn't leak a stale key). Called on every Claude drain tick. A
    /// no-op when memory isn't wired (mirrors [`Self::record_mem`]).
    pub fn mark_live_session(&self, session_id: &str, agent: &str) {
        if let Some(mem) = self.mem.as_ref() {
            mem.mark_live_session(self.tab.as_str(), agent, session_id);
        }
    }

    /// V24 Phase B: drop this tab's live-session registry entry — the Claude
    /// tap calls this when its transcript tail exits (tab cancel / source end),
    /// so a closed tab stops being reported active before its TTL lapses. A
    /// no-op when memory isn't wired.
    pub fn clear_live_session(&self) {
        if let Some(mem) = self.mem.as_ref() {
            mem.clear_live_session(self.tab.as_str());
        }
    }

    /// V30 Phase D: subscribe this tab to the session-push bus as `consumer`,
    /// returning the RAII deregistration guard and the notice queue — or `None`
    /// when the bus isn't wired (mirrors [`Self::record_mem`]'s `mem: None`
    /// degradation).
    ///
    /// `channels: true` is reported unconditionally, because for an in-process
    /// subscriber the field's Claude-side meaning ("this child declared the
    /// `claude/channel` capability at handshake time") has no analogue: nothing
    /// was negotiated, so there is nothing that could have gone stale. The
    /// `offload.session_push` gate is therefore checked at DELIVERY time by the
    /// tap instead — see `opencode::forward_target`.
    pub fn register_pushes(
        &self,
        consumer: &str,
    ) -> Option<(
        crate::offload::service::PushGuard,
        tokio::sync::mpsc::Receiver<crate::offload::service::PushNotice>,
    )> {
        self.pushes.as_ref().map(|reg| {
            reg.register(
                Some(self.tab.as_str().to_string()),
                consumer.to_string(),
                true,
            )
        })
    }

    /// V30 Phase D: whether session push is enabled **right now**. Read live
    /// (never cached at spawn) so the OpenCode fanout is togglable without a tab
    /// restart — see the asymmetry note on `opencode::forward_target`.
    pub fn session_push_enabled(&self) -> bool {
        self.settings.current().offload.session_push
    }

    /// V14 Phase C: record one usage/cost event via the graph service — a
    /// no-op when memory isn't wired or the graph is disabled (mirrors
    /// [`Self::record_mem`]). Never blocks or errors the tap.
    pub fn record_usage(
        &self,
        root: &std::path::Path,
        session_id: &str,
        agent: &str,
        event: crate::graph::UsageEvent,
    ) {
        if let Some(mem) = self.mem.as_ref() {
            mem.record_usage(root, session_id, agent, event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use std::time::Duration;

    fn ctx(tts: tokio::sync::mpsc::Sender<TtsRequest>) -> OobContext {
        let (sig_tx, _sig_rx) = tokio::sync::mpsc::channel(4);
        let mut defaults = Settings::default();
        defaults.tabs.push(crate::settings::default_opencode_tab());
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());
        OobContext {
            tab: TabId::from_str("opencode"),
            tts,
            state_signals: sig_tx,
            settings,
            cancel: CancellationToken::new(),
            mem: None,
            pushes: None,
        }
    }

    #[tokio::test]
    async fn speak_aborts_promptly_on_cancel_while_channel_is_full() {
        // Regression: `speak` used to await the bounded TTS send without
        // racing the cancel token — a closing tab could sit parked behind a
        // backed-up channel until the worker drained a slot.
        let (tts_tx, _tts_rx) = tokio::sync::mpsc::channel(1);
        let ctx = ctx(tts_tx.clone());
        // Fill the single slot so the next send parks (the receiver is held,
        // not read, so nothing ever drains).
        tts_tx
            .try_send(TtsRequest::Synthesize {
                tab: TabId::from_str("opencode"),
                text: "plug".into(),
                suppressible: true,
            })
            .unwrap();
        let cancel = ctx.cancel.clone();
        let speaker = tokio::spawn(async move { ctx.speak("One. Two. Three.").await });
        // Give speak() time to park inside the send.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), speaker)
            .await
            .expect("speak must return promptly once cancelled")
            .unwrap();
    }
}
