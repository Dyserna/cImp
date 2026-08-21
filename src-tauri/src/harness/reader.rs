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
use crate::settings::SettingsHandle;
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
    OpenCodeEvent {
        port: u16,
        /// 2026-08-17: the `Authorization: Basic …` value every call this reader
        /// makes against that server must carry, because cImp now spawns the
        /// child with a per-spawn `OPENCODE_SERVER_PASSWORD` (capability
        /// `opencode.route.noauth`, Tier D → B).
        ///
        /// Carried on the SPEC rather than looked up by the reader, for the same
        /// reason `pinned_session` is: the credential the child was spawned with
        /// is a fact about *this launch*, and a reader that re-derived it could
        /// authenticate against a value the running server never read (the
        /// password is snapshotted at module load in the child). It travels with
        /// the port it belongs to.
        ///
        /// `None` = that child's server is unauthenticated, which is what a
        /// launch whose composed environment carries no password means. Never a
        /// URL parameter and never argv:
        /// `harness::opencode::config::server_basic_auth` builds a header, and
        /// upstream's `auth_token` query param is deliberately unused (a
        /// present-but-wrong one wins over a correct header and 401s).
        auth: Option<String>,
    },
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

/// The tabs that currently have a fallback reader attached.
///
/// V39 Phase B, and it exists for exactly one question: **is there a
/// completion signal for this tab at all?** Locked decision 12 refuses a
/// delegation into a worker cImp cannot read back from, and for a harness that
/// declares `cannot` for CHP `assistant_text` — OpenCode does, by design (D6) —
/// the reader IS the signal. Without this, "the tab has a reader" would be
/// inferred from its command, which is a guess about a task that may never have
/// started.
///
/// A `BTreeSet` behind a plain `Mutex`: the write path is two tab-lifecycle
/// edges, the read path is one preflight.
static LIVE_READERS: std::sync::Mutex<Option<std::collections::BTreeSet<String>>> =
    std::sync::Mutex::new(None);

fn live_readers<T>(f: impl FnOnce(&mut std::collections::BTreeSet<String>) -> T) -> T {
    let mut g = LIVE_READERS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Default::default))
}

/// Whether `tab` has a fallback reader attached right now.
///
/// **"Attached", not "healthy".** A reader whose source has gone silent (a 401
/// on the SSE tap, a transcript that never appears) still answers `true` here,
/// and that is the honest bound of what this can know from outside: the
/// difference between a slow turn and a dead tap is a timeout, which is what
/// the engine's deadline is for. What it does rule out is the case worth
/// ruling out — a tab that never had a reader and never will.
pub fn has_live_reader(tab: &crate::state::TabId) -> bool {
    live_readers(|s| s.contains(tab.as_str()))
}

/// Spawn the adapter described by `spec`, tied to `ctx.cancel`. Non-blocking:
/// returns immediately, the adapter runs until the token is cancelled (tab
/// exit) or the source ends.
pub fn spawn(spec: OobSpec, ctx: OobContext) {
    // V39 Phase B: register this tab's reader for the lifetime of the token
    // that owns the adapter below. Deregistration rides the SAME token the
    // adapter does, so the two cannot disagree about whether a reader is
    // attached — a separate "on tab close" call site could be forgotten by the
    // next lifecycle path, and this one cannot.
    {
        let tab = ctx.tab.as_str().to_string();
        live_readers(|s| s.insert(tab.clone()));
        let cancel = ctx.cancel.clone();
        tauri::async_runtime::spawn(async move {
            cancel.cancelled().await;
            live_readers(|s| s.remove(&tab));
        });
    }
    match spec {
        OobSpec::ClaudeTranscript {
            project_dir,
            pinned_session,
        } => {
            debug!(tab = ?ctx.tab, ?project_dir, ?pinned_session, "spawning Claude transcript OOB source");
            tauri::async_runtime::spawn(claude::read::run(project_dir, pinned_session, ctx));
        }
        OobSpec::OpenCodeEvent { port, auth } => {
            // The credential itself is never logged — only whether there is one,
            // which is the fact worth having when a tap starts 401ing.
            debug!(
                tab = ?ctx.tab,
                port,
                authenticated = auth.is_some(),
                "spawning OpenCode event OOB source"
            );
            tauri::async_runtime::spawn(opencode::read::run(port, auth, ctx));
        }
    }
}

impl OobContext {
    /// V35 Phase L — **the arbitration query, asked at the tap**: is `event`
    /// being PUSHED for this tab, so that this reader must not also produce it?
    ///
    /// `agent` is the harness this reader speaks for; it is a literal at every
    /// call site because a reader only ever serves one, and passing it keeps
    /// the arbitration keyed the same way the peer registry is (`(agent, tab)`).
    ///
    /// A `true` here suppresses ONE tap, never the reader: the Claude
    /// transcript tail still carries usage, identity and sub-agent token
    /// accounting, none of which any hook payload exposes. See
    /// [`crate::harness::chp::served`] for the three properties of the rule.
    pub fn pushed(&self, agent: &str, event: &str) -> bool {
        crate::harness::chp::served(agent, self.tab.as_str(), event)
    }

    /// V39 Phase B — **hand one completed assistant message to whatever is
    /// waiting for this tab's turn to end**, beside speaking it.
    ///
    /// The read half of delegation (locked decision 16) is CHP
    /// `assistant_text`, arbitrated: a tab whose hello declares it is served by
    /// the push core in `offload::loopback`, and a tab that does not is served
    /// by its reader — this call. Exactly one of the two fires per message,
    /// because this is invoked from the same arbitrated branch as
    /// [`Self::speak`].
    ///
    /// Deliberately NOT folded into `speak`: `speak` runs through
    /// `tts::speak_prose`, which is gated by the per-tab TTS toggle, and a
    /// delegation must complete on a tab with TTS switched off. Two consumers,
    /// two calls, one arbitration decision above them.
    ///
    /// Routed through this context rather than called directly from the L1
    /// readers so that `crate::delegation` — an L4 capability — is named in one
    /// file under `harness/` (this one, already `UPWARD_EXEMPT`) instead of in
    /// each harness's reader.
    pub fn note_turn_text(&self, text: &str) {
        crate::delegation::note_assistant_text(&self.tab, text);
    }

    /// Speak one block of assistant prose from THIS reader.
    ///
    /// V35 Phase L moved the composition itself — escape hygiene, markdown
    /// reduction, sentence segmentation, the per-sentence live toggle re-read
    /// and the cancel-raced send — into [`crate::tts::prose::speak_prose`], so
    /// that the CHP push path speaks through the same code rather than through
    /// a second copy of it. This wrapper supplies the three things a reader owns
    /// and a loopback handler does not: the tab, the sender it was handed at
    /// spawn, and the tab's cancellation token.
    ///
    /// [`ProseSource::FallbackReader`] is not decoration: it is what records the
    /// handoff that stops a mid-session switchover from re-speaking a message
    /// this reader had already started (see that module's docs).
    ///
    /// [`ProseSource::FallbackReader`]: crate::tts::ProseSource::FallbackReader
    pub async fn speak(&self, text: &str) {
        crate::tts::speak_prose(
            &self.tab,
            &self.tts,
            &self.settings,
            Some(&self.cancel),
            crate::tts::ProseSource::FallbackReader,
            text,
        )
        .await;
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
