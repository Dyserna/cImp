//! V20: out-of-band TTS sources for fullscreen AI tabs — **the L1 fallback
//! readers' spawn seam** (V35 Phase K: moved here verbatim from `oob/mod.rs`).
//!
//! Design § 6: adding a harness must not mean a new `OobSpec` variant *and* a
//! new arm in a `spawn` living somewhere else in the tree. Both now sit inside
//! `harness/`, next to the readers they name, so the whole per-harness surface
//! is one directory. Phase L retires the readers from the hot path; until then
//! this is where a tab's fallback reader is chosen and started.
//!
//! Before V20, cImp forced its AI tools into an inline renderer so the
//! processing layer could scrape `[[TTS]]` markers out of the linear terminal
//! stream. V20 runs both tools in their native fullscreen (alternate-screen)
//! TUIs, where screen-scraping is impossible — so the speakable text is read
//! from each tool's *structured* side channel instead:
//!
//!   * **Claude Code** appends a transcript JSONL under
//!     `~/.claude/projects/<slug>/<id>.jsonl`; assistant `text` blocks are
//!     written complete at message finish ([`crate::harness::claude::read`]).
//!   * **OpenCode** exposes an SSE event stream at `GET /event` on the same
//!     port the TUI is launched with (`--port`); assistant text arrives as
//!     token-level delta events ([`crate::harness::opencode::read`]).
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

use super::{claude, opencode};

/// Describes which out-of-band source (if any) a tab's launch should attach.
/// Resolved by `tabs::config` at launch time and carried on the `PtyLaunchSpec`
/// so `PtyManager::start` can spawn the matching adapter once the child is up.
#[derive(Debug, Clone)]
pub enum OobSpec {
    /// Tail Claude Code's transcript JSONL for the project rooted at this dir.
    ClaudeTranscript {
        project_dir: PathBuf,
        /// V34: the session id cImp pinned for this tab via `--session-id` at
        /// spawn, when it was able to (see `tabs::config::resolve_oob_source`).
        ///
        /// This is what makes a tab's session binding *provable*. Without it the
        /// tap can only tail the newest `*.jsonl` under a project-derived root,
        /// which two Claude tabs on one project share — the V28 decision-4a
        /// ambiguity that degrades memory scoping and permission attribution.
        ///
        /// `None` when the tab's own args already select a session
        /// (`--resume`/`--continue`/an explicit `--session-id`/...), in which
        /// case the tap falls back to exactly the pre-V34 newest-wins behaviour.
        pinned_session: Option<String>,
    },
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
        OobSpec::ClaudeTranscript {
            project_dir,
            pinned_session,
        } => {
            debug!(tab = ?ctx.tab, ?project_dir, ?pinned_session, "spawning Claude transcript OOB source");
            tauri::async_runtime::spawn(claude::read::run(project_dir, pinned_session, ctx));
        }
        OobSpec::OpenCodeEvent { port } => {
            debug!(tab = ?ctx.tab, port, "spawning OpenCode event OOB source");
            tauri::async_runtime::spawn(opencode::read::run(port, ctx));
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
    ///
    /// V32 Phase D — **escape hygiene at the one external-text boundary the
    /// TTS path has.** `text` is assistant prose lifted from a transcript /
    /// event stream, and an assistant that just read a fetched page routinely
    /// quotes it verbatim — so a page carrying `ESC ] 52 ; c ; …` (a clipboard
    /// write) or cursor-motion sequences reaches this composition site intact.
    /// Stripping here rather than at each tap keeps it one decision in one
    /// place, and it happens BEFORE markdown reduction so a control sequence
    /// cannot alter how `to_speakable` sees fences or list markers.
    ///
    /// V32 Phase G (locked decision 16): the strip is one of the eleven
    /// switchable controls (ten until Phase H added `opencode_native_gate`;
    /// count corrected 2026-08-08, #48), resolved at [`Scope::AppWide`] — TTS and
    /// toasts are global surfaces
    /// (the global-only avatar/TTS decision), so this feature has an L1 and an
    /// L2 and deliberately no per-scope row. Resolved per burst rather than
    /// cached: the settings handle is already read here for `tts_enabled`, and a
    /// user who turns hygiene off wants the next thing spoken to reflect it.
    ///
    /// **The app-wide baseline, not the identity-less caller's answer** (#48,
    /// F-35): the two were one variant until locked decision 36 split them, and
    /// they are provably equal here — a feature with no per-tab row can never
    /// carry the N-1 elevation — so this is a naming change, not a behaviour
    /// change. `AppWide` is the honest one: this is a statement about the
    /// application, and there is no caller to be unsure about.
    ///
    /// [`Scope::AppWide`]: crate::settings::injection::Scope::AppWide
    pub async fn speak(&self, text: &str) {
        if !self.tts_enabled() {
            return;
        }
        let text = if crate::settings::injection::effective(
            crate::settings::injection::Feature::TerminalEscapeHygiene,
            crate::settings::injection::Scope::AppWide,
            &self.settings.current(),
        ) {
            crate::processing::strip_terminal_escapes(text)
        } else {
            std::borrow::Cow::Borrowed(text)
        };
        let prose = crate::processing::to_speakable(&text);
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

    /// H1 fix (2026-08-05 review): declare that this tab is RUNNING `agent` and
    /// derives its session identity from the transcript source `root`, so the
    /// registry can detect when two running tabs share one root and its
    /// tab-keyed answers stop being provable
    /// (`GraphService::mark_live_tab_root`). Called on every poll tick of a
    /// root-binding tap; cleared together with the live-session entry by
    /// [`Self::clear_live_session`]. A no-op when memory isn't wired.
    ///
    /// Only root-binding taps call this. The OpenCode tap must NOT: it reads the
    /// session id off its own per-tab SSE stream, so two OpenCode tabs on one
    /// project are genuinely distinguishable and must keep their scoping.
    /// V34: `pinned` = this tab's tap is following a cImp-chosen session id
    /// (`--session-id`) rather than the newest transcript under `root`, which is
    /// what lets the registry stop treating same-root co-tenants as ambiguous.
    pub fn mark_live_tab_root(&self, agent: &str, root: &std::path::Path, pinned: bool) {
        if let Some(mem) = self.mem.as_ref() {
            mem.mark_live_tab_root(self.tab.as_str(), agent, root, pinned);
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
    /// `channels: true` is reported because a subscription only EXISTS while
    /// `offload.session_push` is on: the OpenCode tap registers and deregisters
    /// as the setting flips (`opencode::sync_subscription`, driven off the
    /// settings broadcast), so no tab restart is needed and
    /// `PushRegistry::deliver`'s count stays honest — it never counts a tab that
    /// is about to drop the notice at the gate. (For an in-process subscriber
    /// the field's Claude-side meaning — "this child declared the
    /// `claude/channel` capability at handshake time" — has no analogue: nothing
    /// is negotiated, so nothing can go stale.) The gate is re-read once more at
    /// DELIVERY time (`opencode::forward_target`), which closes the sub-millisecond
    /// window between a producer's live read and the tap's.
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
