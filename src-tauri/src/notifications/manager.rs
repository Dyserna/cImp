//! Notification queue + dedup + play-when-idle scheduler.
//!
//! Subscribes to the in-process `StateEvent` broadcast and the audio idle
//! `Notify`. State edges enqueue notifications (subject to the global
//! announcements toggle and active-tab filter). Idle edges trigger a drain:
//! we collapse per-tab duplicates, then dispatch each survivor as a
//! `SynthesizeNotification` TTS request in arrival order.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Drain debounce: when a notification is enqueued and audio is currently
/// idle, wait this long before draining. Lets closely-spaced related events
/// (e.g. HarnessOutputStopped → Idle followed shortly by a permission
/// detection on the same tab) land in the queue together so dedup can
/// collapse them to the most informative one.
const DRAIN_DEBOUNCE: Duration = Duration::from_millis(200);

use crate::audio::AudioOutput;
use crate::settings::SettingsHandle;
use crate::state::{AvatarState, StateEvent, TabId, TabKind};
use crate::tts::{ActiveTab, TtsRequest};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotificationEvent {
    Idle,
    AwaitingPermission,
    /// AskUserQuestion-style prompt detected. Independent of the
    /// `AwaitingPermission` event so each can have its own template and
    /// fire even when the other is also in flight.
    AwaitingQuestion,
    Error,
    /// Shell-only: subprocess exited while the user was on a different tab.
    /// Phase 5 of MILESTONE-V3-01.
    Exited,
}

/// Per-kind allowlist of notification triggers. AI tabs get the v1.1 trio;
/// Shell tabs get only `Error` and the new `Exited`. Defense-in-depth: the
/// upstream code already won't generate Idle/AwaitingPermission for Shell
/// tabs (the avatar machine's per-kind gating in Phase 4 makes those edges
/// unreachable), but the explicit gate makes the rule grep-able.
fn allowed_for(kind: &TabKind, event: NotificationEvent) -> bool {
    match (kind, event) {
        (TabKind::AiTool, NotificationEvent::Idle) => true,
        (TabKind::AiTool, NotificationEvent::AwaitingPermission) => true,
        (TabKind::AiTool, NotificationEvent::AwaitingQuestion) => true,
        (TabKind::AiTool, NotificationEvent::Error) => true,
        (TabKind::AiTool, NotificationEvent::Exited) => false,
        (TabKind::Shell, NotificationEvent::Error) => true,
        (TabKind::Shell, NotificationEvent::Exited) => true,
        (TabKind::Shell, NotificationEvent::Idle) => false,
        (TabKind::Shell, NotificationEvent::AwaitingPermission) => false,
        (TabKind::Shell, NotificationEvent::AwaitingQuestion) => false,
        // V14 Phase F: Preview tabs run no subprocess and speak no output —
        // none of these edges are reachable for one, but the allowlist stays
        // explicit (like every other kind here) rather than falling through.
        (TabKind::Preview, _) => false,
    }
}

#[derive(Clone, Debug)]
struct Queued {
    tab: TabId,
    text: String,
    timestamp: Instant,
}

/// Spawn the notification manager on the Tauri/tokio runtime. Returns
/// nothing — failures are logged and the loop ends naturally if any of its
/// inputs close.
pub fn spawn_notification_manager(
    state_events: broadcast::Receiver<StateEvent>,
    audio: Arc<AudioOutput>,
    tts_tx: mpsc::Sender<TtsRequest>,
    settings: SettingsHandle,
    active: ActiveTab,
    initial_active: TabId,
) {
    let manager = NotificationManager::new(
        state_events,
        audio,
        tts_tx,
        settings,
        active,
        initial_active,
    );
    tauri::async_runtime::spawn(manager.run());
}

struct NotificationManager {
    state_events: broadcast::Receiver<StateEvent>,
    audio: Arc<AudioOutput>,
    tts_tx: mpsc::Sender<TtsRequest>,
    settings: SettingsHandle,
    active: ActiveTab,
    queue: Vec<Queued>,
    /// Most-recent observed avatar state per tab. The state manager already
    /// only emits StateChanged on real transitions, but it does re-emit
    /// initial Idle on startup; the cache lets us reject re-fires without
    /// queueing a phantom `idle` notification before any real activity.
    last_avatar: HashMap<TabId, AvatarState>,
    last_awaiting: HashMap<TabId, bool>,
    last_awaiting_question: HashMap<TabId, bool>,
    /// Set when something is enqueued and we haven't yet drained. The
    /// run loop selects on `sleep_until(deadline)` and drains when it
    /// fires. Cleared by both the deadline arm and the audio idle-edge
    /// arm so the next enqueue can re-schedule cleanly.
    drain_deadline: Option<Instant>,
    /// Tabs whose notification audio is currently in flight (from `try_drain`
    /// to the matching `Speaking → Idle` echo). The audio playback for our
    /// own announcement fires `TtsPlaybackStarted/Stopped` on the active
    /// tab, which makes the state machine cycle `Idle → Speaking → Idle`.
    /// Without this guard, that closing `Speaking → Idle` would itself look
    /// like a fresh "tab went idle" event and queue another announcement —
    /// which loops on every "Claude is idle" repeat. Cleared either when
    /// the matching echo arrives (suppressed) or when any non-Speaking,
    /// non-Idle state edge for the tab pre-empts it.
    /// Per-tab count of armed echo-suppressions, not a set: draining N
    /// announcements that all resolve on the same (active) tab must arm N
    /// suppressions, or the 2nd..Nth closing `Speaking → Idle` echoes slip
    /// through as spurious "Claude is idle" notifications.
    just_dispatched: HashMap<TabId, u32>,
    /// Tabs the user has actually interacted with this session — observed by
    /// the tab entering `Listening`, which is only reachable via a user
    /// keystroke or compose input. The spoken `Idle` notification is gated on
    /// membership here: until a tab is armed, any settle into Idle is just
    /// startup chrome (a harness's welcome banner cycles a freshly-spawned tab
    /// `Idle → Thinking → Idle` as it prints) and must stay silent. This is
    /// the user-input analogue of the `last_avatar` "no idle before any real
    /// activity" guard, which only rejects the re-emitted *initial* Idle and
    /// not this real banner-driven transition.
    ///
    /// **V40 Phase E (locked decision 26): the guard is keyed on
    /// `HarnessPlugin::emits_startup_chrome`.** Whether a fresh tab paints a
    /// banner that looks like a finished turn is a fact about the harness, not
    /// about notifications; a harness that declares `false` gets its first Idle
    /// announced. The default is `true`, so an unclassified tab keeps the
    /// pre-V40 behaviour.
    interacted: HashSet<TabId>,
    /// Turn-boundary bookkeeping for the idle announcement — see [`IdleGate`].
    /// Split out of this struct because everything about it is testable and
    /// nothing about the manager is: `NotificationManager` owns an
    /// [`AudioOutput`], which opens a real output device.
    gate: IdleGate,
    /// Tabs whose pending permission / question prompt was just cleared by
    /// input. Consumed by the next avatar edge for that tab, which is the
    /// `Listening` the answering keystroke produces.
    prompt_answered: std::collections::HashSet<TabId>,
}

impl NotificationManager {
    fn new(
        state_events: broadcast::Receiver<StateEvent>,
        audio: Arc<AudioOutput>,
        tts_tx: mpsc::Sender<TtsRequest>,
        settings: SettingsHandle,
        active: ActiveTab,
        initial_active: TabId,
    ) -> Self {
        // Per-tab caches start empty; `TabCreated` events seed them. The
        // state manager emits one `TabCreated` per launch-seed tab during
        // startup, so by the time any `StateChanged` arrives the relevant
        // entry is in place. Runtime-added Shell tabs (M2) extend the
        // caches the same way.
        let _ = initial_active; // reserved; not currently used outside `active`.
        Self {
            state_events,
            audio,
            tts_tx,
            settings,
            active,
            queue: Vec::new(),
            last_avatar: HashMap::new(),
            last_awaiting: HashMap::new(),
            last_awaiting_question: HashMap::new(),
            drain_deadline: None,
            just_dispatched: HashMap::new(),
            interacted: HashSet::new(),
            gate: IdleGate::default(),
            prompt_answered: std::collections::HashSet::new(),
        }
    }

    async fn run(mut self) {
        let idle_notify = self.audio.idle_notify();
        loop {
            // `sleep_until` needs a concrete Instant; when no drain is
            // scheduled we await `pending` so this arm never fires. Copy
            // the deadline out so the async block can be `move`.
            let deadline = self.drain_deadline;
            tokio::select! {
                biased;
                evt = self.state_events.recv() => {
                    match evt {
                        Ok(e) => self.on_state_event(e),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "notifications: state-event broadcast lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = idle_notify.notified() => {
                    self.try_drain().await;
                    self.drain_deadline = None;
                }
                _ = async move {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.try_drain().await;
                    self.drain_deadline = None;
                }
            }
        }
        debug!("notification manager: state-event channel closed; exiting");
    }

    fn on_state_event(&mut self, event: StateEvent) {
        let queued = match event {
            StateEvent::StateChanged { tab, state } => {
                // Entering `Listening` is the first observable proof the user
                // has interacted with this tab (it's reachable only via a
                // keystroke or compose input). Arm the tab so a later settle
                // into Idle is allowed to notify; until then, Idle edges are
                // startup-banner chrome and stay silent.
                if state == AvatarState::Listening {
                    self.interacted.insert(tab.clone());
                }
                let prev = self
                    .last_avatar
                    .insert(tab.clone(), state)
                    .unwrap_or(AvatarState::Idle);
                if prev == state {
                    return;
                }
                // Turn-span bookkeeping. Runs ahead of every suppression
                // `return` below, so a turn is measured over the real
                // transitions rather than over the subset that survives as far
                // as an enqueue.
                // A keystroke that ANSWERS a pending permission / question
                // prompt is not the user taking the tab back: the turn
                // continues the moment the answer lands. Keep the clock, or a
                // long turn with a prompt near its end is judged by its tail
                // alone and goes silent (rc.9 live-verify, orchestrator review).
                // The prompt-cleared event is dispatched BEFORE this avatar
                // edge, so `last_awaiting` already reads false here; the
                // one-shot `prompt_answered` flag set there is what survives.
                let answering_prompt = self.prompt_answered.remove(&tab);
                if !(state == AvatarState::Listening && answering_prompt) {
                    self.gate.note_avatar_state(&tab, state, Instant::now());
                }
                // Suppress the `Speaking → Idle` echo from our own
                // notification's audio playback ending. The audio thread
                // emits `TtsPlaybackStarted/Stopped` for the active tab on
                // every stretch of audio, which makes the state machine
                // cycle `Idle → Speaking → Idle` whenever an announcement
                // plays. Without this, that closing edge fires another
                // "Claude is idle" notification, which plays, fires another
                // edge, and so on. `just_dispatched` is set in `try_drain`
                // and is consumed exactly once here.
                if prev == AvatarState::Speaking
                    && state == AvatarState::Idle
                    && self.consume_dispatch(&tab)
                {
                    debug!(?tab, "notifications: suppressing Idle (own playback echo)");
                    return;
                }
                // Any state edge other than `Idle ↔ Speaking` for a
                // just-dispatched tab means real activity got there before
                // the echo (user typed, Claude restarted, error fired).
                // Drop the marker so the next dispatch's echo can be
                // suppressed cleanly without our flag stale-suppressing a
                // genuine future Idle.
                if state != AvatarState::Speaking && state != AvatarState::Idle {
                    // Real activity beat the echo for ONE armed suppression.
                    // Decrement a single pending count — `remove` would wipe
                    // every queued suppression for this tab at once, so the
                    // remaining N-1 closing echoes would each fire a spurious
                    // "idle" notification (a cascade).
                    if let Some(count) = self.just_dispatched.get_mut(&tab) {
                        *count -= 1;
                        if *count == 0 {
                            self.just_dispatched.remove(&tab);
                        }
                    }
                }
                if self.suppress_for_focus(&tab) {
                    return;
                }
                let nev = match state {
                    AvatarState::Idle if prev != AvatarState::Idle => {
                        // **Is this edge even this tab's turn boundary?**
                        //
                        // For a harness that pushes an explicit end-of-turn
                        // signal it is not: its Idle edges come from a TUI
                        // activity heuristic that fires once per output BURST,
                        // so one tool-heavy turn settles to Idle once per tool
                        // call. Measured live on 2026-08-23: eight
                        // "<tab> is idle" announcements inside ONE turn. Those
                        // tabs are announced on `StateEvent::TurnEnded` and
                        // nowhere else; every other tab (OpenCode, and any tab
                        // whose binary core cannot classify) keeps this edge.
                        if self.turn_end_push(&tab) {
                            debug!(
                                ?tab,
                                "notifications: Idle edge is not a turn boundary \
                                 (this harness pushes its turn end)"
                            );
                            return;
                        }
                        // Pre-interaction settle (e.g. the startup welcome
                        // banner cycling Idle → Thinking → Idle as it prints):
                        // the user has never driven this tab to Listening, so
                        // this is not a "the harness finished your task" event.
                        // Stay silent until the tab has been interacted with —
                        // for a harness that says it paints such chrome.
                        if self.emits_startup_chrome(&tab) && !self.interacted.contains(&tab) {
                            debug!(
                                ?tab,
                                "notifications: suppressing Idle (no user interaction yet)"
                            );
                            return;
                        }
                        // Suppress Idle when this tab currently has a
                        // pending permission prompt — the avatar state
                        // machine drops to Idle when Claude stops printing
                        // (which is exactly what happens just before a
                        // permission prompt), so without this filter the
                        // user hears "awaiting permission" followed by
                        // "idle" for the same logical event.
                        if self.last_awaiting.get(&tab).copied().unwrap_or(false) {
                            debug!(
                                ?tab,
                                "notifications: suppressing Idle (awaiting permission)"
                            );
                            return;
                        }
                        // Symmetric guard for a pending question: the avatar also
                        // drops to Idle just before a question prompt, so without
                        // this the user hears "idle" instead of "awaiting question".
                        if self
                            .last_awaiting_question
                            .get(&tab)
                            .copied()
                            .unwrap_or(false)
                        {
                            debug!(?tab, "notifications: suppressing Idle (awaiting question)");
                            return;
                        }
                        if !self.turn_worth_announcing(&tab) {
                            return;
                        }
                        NotificationEvent::Idle
                    }
                    AvatarState::Error => NotificationEvent::Error,
                    _ => return,
                };
                self.try_enqueue(tab, nev)
            }
            StateEvent::AwaitingPermissionChanged { tab, awaiting } => {
                let prev = self
                    .last_awaiting
                    .insert(tab.clone(), awaiting)
                    .unwrap_or(false);
                if prev && !awaiting {
                    self.prompt_answered.insert(tab.clone());
                }
                if prev == awaiting || !awaiting {
                    return;
                }
                if self.suppress_for_focus(&tab) {
                    return;
                }
                self.try_enqueue(tab, NotificationEvent::AwaitingPermission)
            }
            StateEvent::AwaitingQuestionChanged { tab, awaiting } => {
                let prev = self
                    .last_awaiting_question
                    .insert(tab.clone(), awaiting)
                    .unwrap_or(false);
                if prev && !awaiting {
                    self.prompt_answered.insert(tab.clone());
                }
                if prev == awaiting || !awaiting {
                    return;
                }
                if self.suppress_for_focus(&tab) {
                    return;
                }
                self.try_enqueue(tab, NotificationEvent::AwaitingQuestion)
            }
            StateEvent::TabClosedStateChanged {
                tab,
                closed,
                exit_code,
                closed_message: _,
            } => {
                // Only the closed=true edge fires a notification; the
                // closed=false edge (restart) is a UI-only transition.
                if !closed {
                    return;
                }
                if self.suppress_for_focus(&tab) {
                    return;
                }
                self.try_enqueue_with_code(tab, NotificationEvent::Exited, exit_code)
            }
            StateEvent::TabCreated { tab, .. } => {
                // Seed per-tab caches so the first real StateChanged /
                // AwaitingPermissionChanged / AwaitingQuestionChanged event
                // compares against an explicit baseline instead of relying
                // on `unwrap_or` fallbacks. Idempotent.
                self.last_avatar
                    .entry(tab.clone())
                    .or_insert(AvatarState::Idle);
                self.last_awaiting.entry(tab.clone()).or_insert(false);
                self.last_awaiting_question.entry(tab).or_insert(false);
                return;
            }
            StateEvent::TabClosed { tab } => {
                self.last_avatar.remove(&tab);
                self.last_awaiting.remove(&tab);
                self.last_awaiting_question.remove(&tab);
                self.just_dispatched.remove(&tab);
                self.interacted.remove(&tab);
                self.gate.forget(&tab);
                self.prompt_answered.remove(&tab);
                // Drop any queued notifications targeting the closed tab —
                // playing them after close would refer to a tab that no
                // longer exists in the UI.
                self.queue.retain(|q| q.tab != tab);
                return;
            }
            StateEvent::TurnEnded { tab } => {
                // The harness itself said the assistant turn is over. For a
                // harness that pushes this it is the ONLY idle-announcement
                // trigger (see the `AvatarState::Idle` arm above); for any
                // other tab a stray push is a no-op, so the two paths can never
                // both fire for one turn.
                if !self.turn_end_push(&tab) {
                    return;
                }
                if self.suppress_for_focus(&tab) {
                    return;
                }
                // The same two "this Idle is really a prompt" guards the Idle
                // edge runs: a `Stop` can land immediately before the prompt
                // that follows it, and the user would hear "idle" for what is
                // actually "awaiting permission".
                if self.last_awaiting.get(&tab).copied().unwrap_or(false) {
                    debug!(?tab, "notifications: suppressing turn end (awaiting permission)");
                    return;
                }
                if self
                    .last_awaiting_question
                    .get(&tab)
                    .copied()
                    .unwrap_or(false)
                {
                    debug!(?tab, "notifications: suppressing turn end (awaiting question)");
                    return;
                }
                // Deliberately NOT behind the `interacted` guard the Idle edge
                // runs. That guard exists because a fresh tab's startup banner
                // cycles the avatar like a finished turn — and a turn-end push
                // cannot come from a banner: it fires only after a genuine
                // assistant turn, which IS the proof of activity the guard
                // stands in for. A resumed session that finishes a turn before
                // the user has typed anything is a real event and gets said.
                if !self.turn_worth_announcing(&tab) {
                    return;
                }
                self.try_enqueue(tab, NotificationEvent::Idle)
            }
            StateEvent::ActiveTabChanged { .. }
            | StateEvent::DoneWhileAwayChanged { .. }
            | StateEvent::TabRenamed { .. }
            | StateEvent::TtsSelectionProgress { .. } => {
                return;
            }
        };

        // Schedule a debounced drain. The delay lets closely-spaced
        // related events (e.g. permission detection arriving microseconds
        // after the Idle that preceded it) land in the queue together so
        // dedup can collapse them. If `drain_deadline` is already set the
        // earlier deadline stands — no point pushing it out on every event.
        if queued && self.drain_deadline.is_none() {
            self.drain_deadline = Some(Instant::now() + DRAIN_DEBOUNCE);
        }
    }

    /// Whether `tab`'s harness paints startup chrome that can look like a
    /// finished turn — the predicate the pre-interaction Idle guard runs on
    /// (V40 Phase E, locked decision 26).
    ///
    /// A tab that runs no registered harness answers `true`: the trait default,
    /// reached the same way, and the fail-safe direction (a suppressed
    /// announcement nobody asked for costs less than a spurious one on every
    /// spawn).
    fn emits_startup_chrome(&self, tab: &TabId) -> bool {
        crate::tabs::tab_harness_by_id(&self.settings.current(), tab.as_str())
            .and_then(|h| h.plugin())
            .is_none_or(|p| p.emits_startup_chrome())
    }

    /// Whether `tab`'s harness pushes an explicit end-of-turn signal
    /// ([`crate::harness::plugin::HarnessPlugin::turn_end_push`]) — i.e.
    /// whether this tab is announced on `StateEvent::TurnEnded` instead of on
    /// the avatar's `Idle` edge.
    ///
    /// A tab running no registered harness answers `false`, which is the
    /// trait default and the safe direction: it keeps the Idle-edge path, so an
    /// unclassified tab announces (possibly a little more than it should)
    /// rather than going silent waiting for a push that has no producer.
    fn turn_end_push(&self, tab: &TabId) -> bool {
        crate::tabs::tab_harness_by_id(&self.settings.current(), tab.as_str())
            .and_then(|h| h.plugin())
            .is_some_and(|p| p.turn_end_push())
    }

    /// Close `tab`'s open turn and answer whether it ran long enough to be
    /// worth an announcement — `behavior.idle_announce_min_working_secs`.
    ///
    /// Read-and-clear either way: the caller only reaches this once it has
    /// decided the edge really is a turn boundary. Re-reads settings on every
    /// call so a changed value applies to the next turn without a restart, like
    /// the focus filter — a tab that is mid-turn when the number changes is
    /// judged by the NEW number, since the measurement is a timestamp and not a
    /// decision taken at turn start.
    fn turn_worth_announcing(&mut self, tab: &TabId) -> bool {
        let min_secs = self
            .settings
            .current()
            .behavior
            .idle_announce_min_working_secs;
        let min = Duration::from_secs(u64::from(min_secs));
        match self.gate.close_turn(tab, Instant::now(), min) {
            IdleVerdict::Announce => true,
            IdleVerdict::TooShort { worked } => {
                let worked_secs = worked.map(|d| d.as_secs_f32()).unwrap_or(0.0);
                debug!(
                    ?tab,
                    "notifications: idle suppressed (worked {worked_secs:.1}s < {min_secs}s)"
                );
                false
            }
        }
    }

    /// True if `tab` should be skipped because it's the currently-focused
    /// tab and the user hasn't opted into hearing announcements for the
    /// focused tab. Re-reads settings each call so toggling the checkbox
    /// applies on the very next state edge without restart.
    fn suppress_for_focus(&self, tab: &TabId) -> bool {
        if self.settings.current().behavior.announce_focused_tab {
            return false;
        }
        // Benign fallback on a poisoned lock rather than `.expect()`: a panic
        // here would permanently kill the notification task and silence all
        // cross-tab announcements. The registry's default tab (V40 Phase E),
        // matching how the rest of the codebase reads this shared lock.
        let active_tab = self
            .active
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| TabId::first_harness_default());
        *tab == active_tab
    }

    /// Consume one armed echo-suppression for `tab`. Returns true if one was
    /// armed (so this `Speaking → Idle` edge is our own playback echo and
    /// should be suppressed). Counts down so each drained announcement
    /// suppresses exactly one closing echo.
    fn consume_dispatch(&mut self, tab: &TabId) -> bool {
        // Prefer the exact tab (the common same-tab echo).
        if let Some(count) = self.just_dispatched.get_mut(tab) {
            *count -= 1;
            if *count == 0 {
                self.just_dispatched.remove(tab);
            }
            return true;
        }
        // Cross-tab fallback: the echo's `Speaking → Idle` is tagged with
        // whatever tab is active when our announcement audio actually plays,
        // which can differ from the tab we guessed at drain time if the active
        // tab changed in between. Audio is globally serialized (one sink), so a
        // `Speaking → Idle` arriving while a suppression is armed is our echo —
        // consume one pending entry from any armed tab. Without this, the stale
        // count left on the originally-guessed tab would silence that tab's
        // next genuine Idle notification.
        if let Some(key) = self.just_dispatched.keys().next().cloned() {
            match self.just_dispatched.get_mut(&key) {
                Some(count) => {
                    *count -= 1;
                    if *count == 0 {
                        self.just_dispatched.remove(&key);
                    }
                    true
                }
                None => false,
            }
        } else {
            false
        }
    }

    fn try_enqueue(&mut self, tab: TabId, event: NotificationEvent) -> bool {
        self.try_enqueue_with_code(tab, event, None)
    }

    fn try_enqueue_with_code(
        &mut self,
        tab: TabId,
        event: NotificationEvent,
        exit_code: Option<i32>,
    ) -> bool {
        let settings = self.settings.current();
        if !settings.behavior.announcements_enabled {
            return false;
        }
        if !allowed_for(&tab.kind(), event) {
            debug!(?tab, ?event, "notifications: dropped (disallowed for kind)");
            return false;
        }
        let text = notification_text(&settings, &tab, event, exit_code);
        if text.is_empty() {
            // Per design: empty text disables the (tab, event) announcement.
            return false;
        }
        debug!(?tab, ?event, "notifications: queued");
        self.queue.push(Queued {
            tab,
            text,
            timestamp: Instant::now(),
        });
        true
    }

    async fn try_drain(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.audio.is_playing() {
            // Notify fired earlier but new playback started before we ran;
            // wait for the next idle edge.
            return;
        }
        let drained = std::mem::take(&mut self.queue);
        let surviving = dedup_per_tab(&drained);
        info!(
            queued = drained.len(),
            playing = surviving.len(),
            "notifications: draining"
        );
        for n in surviving {
            let tab = n.tab.clone();
            let req = TtsRequest::SynthesizeNotification {
                tab: n.tab,
                text: n.text,
            };
            if let Err(e) = self.tts_tx.send(req).await {
                warn!(error = %e, "notifications: tts channel closed");
                return;
            }
            // Arm the echo-suppression guard on the tab whose avatar will
            // actually produce the playback echo. The audio thread tags its
            // `TtsPlaybackStarted/Stopped` events with the *currently active*
            // tab, not the notification's originating tab, so the
            // `Idle → Speaking → Idle` cycle plays out on the active tab. In the
            // common same-tab case that's the originating tab; in the cross-tab
            // case (announcement for an inactive tab while
            // `announce_focused_tab=true`) it's the active tab. Arm exactly that
            // one — arming the originating tab too would, in the cross-tab case,
            // leave a count that no echo ever consumes, which then silences the
            // next genuine Idle notification on that inactive tab.
            let echo_tab = match self.active.read() {
                Ok(active) => active.clone(),
                Err(_) => tab.clone(),
            };
            *self.just_dispatched.entry(echo_tab).or_insert(0) += 1;
        }
    }
}

fn notification_text(
    settings: &crate::settings::Settings,
    tab: &TabId,
    event: NotificationEvent,
    exit_code: Option<i32>,
) -> String {
    use crate::settings::TabConfig;

    let Some(entry) = settings.find_tab(tab.as_str()) else {
        // Tab without a settings entry is a transient state (e.g. closed
        // mid-flight). Returning an empty string suppresses the
        // announcement, which is the right behavior — there's nothing
        // sensible to say about a tab that no longer exists.
        return String::new();
    };

    use crate::settings::NotificationSlot;
    // V1.11 promoted each per-event slot to a `{ enabled, text }`
    // pair. Disabled or empty-text slots both fall through to the
    // empty-string suppression path in the caller, so the firing
    // contract is unchanged.
    let slot: &NotificationSlot = match (entry, event) {
        (TabConfig::AiTool(c), NotificationEvent::Idle) => &c.notifications.idle,
        (TabConfig::AiTool(c), NotificationEvent::AwaitingPermission) => {
            &c.notifications.awaiting_permission
        }
        (TabConfig::AiTool(c), NotificationEvent::AwaitingQuestion) => &c.notifications.question,
        (TabConfig::AiTool(c), NotificationEvent::Error) => &c.notifications.error,
        // AI tabs don't fire Exited; defensive empty.
        (TabConfig::AiTool(_), NotificationEvent::Exited) => return String::new(),
        (TabConfig::Shell(c), NotificationEvent::Error) => &c.notifications.error,
        (TabConfig::Shell(c), NotificationEvent::Exited) => &c.notifications.exited,
        // Shell tabs don't fire Idle / AwaitingPermission / AwaitingQuestion;
        // defensive empty.
        (TabConfig::Shell(_), NotificationEvent::Idle)
        | (TabConfig::Shell(_), NotificationEvent::AwaitingPermission)
        | (TabConfig::Shell(_), NotificationEvent::AwaitingQuestion) => return String::new(),
        // V14 Phase F: Preview tabs never reach `allowed_for` with a `true`
        // result (see its `(TabKind::Preview, _) => false` arm), so this
        // function is never called for one in practice — defensive empty.
        (TabConfig::Preview(_), _) => return String::new(),
    };

    if !slot.enabled {
        return String::new();
    }
    // `{code}` first, then `{tab}`: a display name that happens to contain the
    // literal "{code}" must stay literal instead of being re-scanned.
    interpolate_tab(&interpolate_code(&slot.text, exit_code), entry.name())
}

/// Replace `{tab}` with the tab's **current** display name.
///
/// This is what makes a seeded notification survive a rename or a duplicate.
/// The four AI slots ship as "{tab} is idle" / "{tab} is awaiting permission" /
/// "{tab} has a question" / "{tab} encountered an error" rather than with the
/// name baked in at seed time, so the tab-duplicate path — which clones the
/// source tab's whole config, notification texts included (see
/// `ipc::tab_lifecycle::create_ai_tab`) — is correct by construction: "Claude 2"
/// says "Claude 2 is idle", where a baked name said "Claude is idle".
///
/// Existing settings files carrying the baked form are rewritten by schema step
/// 37 → 38. A user-edited text is left exactly as typed and simply has no
/// placeholder to resolve.
fn interpolate_tab(template: &str, name: &str) -> String {
    if !template.contains("{tab}") {
        return template.to_string();
    }
    template.replace("{tab}", name)
}

/// What closing a turn says about announcing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleVerdict {
    /// Say it.
    Announce,
    /// The turn ran for less than the configured minimum — or there was no
    /// turn at all (`worked: None`), which is what a settle with nothing in
    /// front of it looks like: startup chrome, not a finished task.
    TooShort { worked: Option<Duration> },
}

/// Per-tab turn-boundary bookkeeping for the idle announcement.
///
/// One open turn per tab, opened by the avatar entering `Thinking` and closed
/// by whichever edge that tab's harness makes its turn boundary. Split out of
/// [`NotificationManager`] so it can be driven by tests with synthetic
/// instants: the manager owns an [`AudioOutput`], which opens a real device.
///
/// The invariant worth stating: **an open turn survives every intermediate
/// edge.** A tool-heavy Claude turn cycles `Thinking → Idle → Thinking → …`
/// once per tool call and a mid-turn TTS tag cycles `Thinking → Speaking →
/// Thinking`; neither restarts the clock, or a 3-minute turn would measure as
/// the 60 ms of its last burst and slip through a gate it should have passed on
/// its real length.
#[derive(Default)]
struct IdleGate {
    turn_started_at: HashMap<TabId, Instant>,
}

impl IdleGate {
    /// Fold one avatar transition in.
    ///
    /// * `Thinking` OPENS a turn — `or_insert`, so only the first burst of a
    ///   turn sets the clock.
    /// * `Listening` CLOSES one without a verdict: it is reachable only from a
    ///   keystroke or compose input, so the user has taken the tab back and
    ///   whatever comes next is a new turn.
    /// * `Idle`, `Speaking` and `Error` say nothing. `Idle` in particular is
    ///   NOT a close: for a pushed-boundary harness it is a mid-turn burst
    ///   edge, and for every other harness the caller closes it explicitly via
    ///   [`Self::close_turn`] once it has cleared its own suppressions.
    fn note_avatar_state(&mut self, tab: &TabId, state: AvatarState, now: Instant) {
        match state {
            AvatarState::Thinking => {
                self.turn_started_at.entry(tab.clone()).or_insert(now);
            }
            AvatarState::Listening => {
                self.turn_started_at.remove(tab);
            }
            AvatarState::Idle | AvatarState::Speaking | AvatarState::Error => {}
        }
    }

    /// Close `tab`'s turn and judge it against `min`. Read-and-clear.
    fn close_turn(&mut self, tab: &TabId, now: Instant, min: Duration) -> IdleVerdict {
        let worked = self
            .turn_started_at
            .remove(tab)
            .map(|start| now.saturating_duration_since(start));
        if idle_announce_allowed(worked, min) {
            IdleVerdict::Announce
        } else {
            IdleVerdict::TooShort { worked }
        }
    }

    /// Drop `tab` entirely (it closed).
    fn forget(&mut self, tab: &TabId) {
        self.turn_started_at.remove(tab);
    }
}

/// Whether a finished turn passes the fast-turn gate.
///
/// `min == 0` disables the gate outright — every turn end announces, the
/// behavior before `behavior.idle_announce_min_working_secs` existed, and the
/// one case where a `None` span still passes (there is nothing to be too short
/// about). Otherwise the turn must have run for at least `min`, and `None`
/// never passes.
fn idle_announce_allowed(worked: Option<Duration>, min: Duration) -> bool {
    if min.is_zero() {
        return true;
    }
    matches!(worked, Some(d) if d >= min)
}

/// Replace `{code}` with the exit code (or `?` when none was reported).
/// `String::replace` handles zero, one, or multiple occurrences. The `?`
/// fallback only matters defensively for templates that use `{code}` outside
/// the Exited slot — Shell exits always carry an exit code in practice.
fn interpolate_code(template: &str, exit_code: Option<i32>) -> String {
    if !template.contains("{code}") {
        return template.to_string();
    }
    let replacement = match exit_code {
        Some(c) => c.to_string(),
        None => "?".to_string(),
    };
    template.replace("{code}", &replacement)
}

/// Per-tab dedup at play-time: keep only the most recent notification per
/// tab, in the order each tab first appeared in the queue. Preserves the
/// "first-arriving tab plays first" semantic while collapsing repeats from
/// the same tab to whichever announcement is freshest.
fn dedup_per_tab(queue: &[Queued]) -> Vec<Queued> {
    let mut latest: HashMap<TabId, &Queued> = HashMap::new();
    for q in queue {
        latest
            .entry(q.tab.clone())
            .and_modify(|cur| {
                if q.timestamp > cur.timestamp {
                    *cur = q;
                }
            })
            .or_insert(q);
    }
    let mut out = Vec::with_capacity(latest.len());
    let mut emitted: HashSet<TabId> = HashSet::new();
    for q in queue {
        if emitted.insert(q.tab.clone()) {
            if let Some(winner) = latest.get(&q.tab) {
                out.push((*winner).clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(tab: TabId, _event: NotificationEvent, text: &str, t_ms: u64) -> Queued {
        Queued {
            tab,
            text: text.to_string(),
            // Tests only compare via `>` on Instant, so we anchor everything
            // to a single base and offset by ms.
            timestamp: Instant::now() + std::time::Duration::from_millis(t_ms),
        }
    }

    #[test]
    fn dedup_keeps_only_latest_per_single_tab() {
        let queue = vec![
            q(TabId::from_str("claude"), NotificationEvent::Idle, "first", 0),
            q(
                TabId::from_str("claude"),
                NotificationEvent::AwaitingPermission,
                "second",
                10,
            ),
            q(TabId::from_str("claude"), NotificationEvent::Error, "third", 20),
        ];
        let out = dedup_per_tab(&queue);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "third");
    }

    #[test]
    fn dedup_preserves_first_appearing_tab_order() {
        let queue = vec![
            q(TabId::from_str("claude-local"), NotificationEvent::Idle, "local-old", 0),
            q(TabId::from_str("claude"), NotificationEvent::Idle, "claude-old", 5),
            q(
                TabId::from_str("claude-local"),
                NotificationEvent::Error,
                "local-new",
                10,
            ),
            q(TabId::from_str("claude"), NotificationEvent::Error, "claude-new", 15),
        ];
        let out = dedup_per_tab(&queue);
        assert_eq!(out.len(), 2);
        // ClaudeLocal first because it appeared first in the queue.
        assert_eq!(out[0].tab, TabId::from_str("claude-local"));
        assert_eq!(out[0].text, "local-new");
        assert_eq!(out[1].tab, TabId::from_str("claude"));
        assert_eq!(out[1].text, "claude-new");
    }

    #[test]
    fn dedup_passes_single_through_unchanged() {
        let queue = vec![q(TabId::from_str("claude"), NotificationEvent::Idle, "only", 0)];
        let out = dedup_per_tab(&queue);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "only");
    }

    #[test]
    fn dedup_handles_empty_queue() {
        let out = dedup_per_tab(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn allowlist_ai_tabs_get_v1_trio_plus_question() {
        let kind = TabKind::AiTool;
        assert!(allowed_for(&kind, NotificationEvent::Idle));
        assert!(allowed_for(&kind, NotificationEvent::AwaitingPermission));
        assert!(allowed_for(&kind, NotificationEvent::AwaitingQuestion));
        assert!(allowed_for(&kind, NotificationEvent::Error));
        assert!(!allowed_for(&kind, NotificationEvent::Exited));
    }

    #[test]
    fn allowlist_shell_tabs_get_error_and_exited() {
        let kind = TabKind::Shell;
        assert!(allowed_for(&kind, NotificationEvent::Error));
        assert!(allowed_for(&kind, NotificationEvent::Exited));
        assert!(!allowed_for(&kind, NotificationEvent::Idle));
        assert!(!allowed_for(&kind, NotificationEvent::AwaitingPermission));
        assert!(!allowed_for(&kind, NotificationEvent::AwaitingQuestion));
    }

    #[test]
    fn interpolate_replaces_single_code_token() {
        assert_eq!(
            interpolate_code("Shell exited (code {code})", Some(0)),
            "Shell exited (code 0)"
        );
        assert_eq!(
            interpolate_code("Shell exited (code {code})", Some(137)),
            "Shell exited (code 137)"
        );
    }

    #[test]
    fn interpolate_handles_negative_code() {
        // SIGSEGV-style synthesized negatives can flow through on Unix when
        // a child is killed by signal — verify we render them faithfully.
        assert_eq!(interpolate_code("exit {code}", Some(-1)), "exit -1");
    }

    #[test]
    fn interpolate_replaces_all_occurrences() {
        assert_eq!(interpolate_code("{code} {code}", Some(2)), "2 2");
    }

    #[test]
    fn interpolate_falls_back_to_question_mark_when_code_missing() {
        assert_eq!(interpolate_code("exit {code}", None), "exit ?");
    }

    // ── {tab} interpolation ────────────────────────────────────────────────

    #[test]
    fn interpolate_tab_resolves_the_seeded_placeholder() {
        assert_eq!(interpolate_tab("{tab} is idle", "Claude 2"), "Claude 2 is idle");
        assert_eq!(
            interpolate_tab("{tab} is awaiting permission", "OpenCode"),
            "OpenCode is awaiting permission"
        );
    }

    #[test]
    fn interpolate_tab_replaces_all_occurrences_and_passes_others_through() {
        assert_eq!(interpolate_tab("{tab}: {tab}", "A"), "A: A");
        // A user-edited text with no placeholder is left exactly as typed —
        // including one that hard-codes a name, which is a choice, not a bug.
        assert_eq!(interpolate_tab("Claude is idle", "Claude 2"), "Claude is idle");
        assert_eq!(interpolate_tab("", "Claude"), "");
    }

    #[test]
    fn interpolate_tab_does_not_rescan_a_name_for_the_code_token() {
        // The caller resolves `{code}` first, so a display name containing the
        // literal "{code}" survives verbatim rather than being substituted.
        let once = interpolate_code("{tab} exited {code}", Some(3));
        assert_eq!(interpolate_tab(&once, "weird {code} name"), "weird {code} name exited 3");
    }

    // ── the fast-turn gate ─────────────────────────────────────────────────

    #[test]
    fn a_short_turn_is_suppressed_and_a_long_one_is_not() {
        let min = Duration::from_secs(120);
        assert!(!idle_announce_allowed(Some(Duration::from_millis(500)), min));
        assert!(!idle_announce_allowed(Some(Duration::from_secs(119)), min));
        assert!(idle_announce_allowed(Some(min), min), "exactly the minimum counts");
        assert!(idle_announce_allowed(Some(Duration::from_secs(130)), min));
    }

    #[test]
    fn a_settle_with_no_turn_in_front_of_it_is_suppressed_unless_the_gate_is_off() {
        assert!(!idle_announce_allowed(None, Duration::from_secs(120)));
        // …but 0 means "announce every idle", which includes this one.
        assert!(idle_announce_allowed(None, Duration::ZERO));
    }

    #[test]
    fn a_zero_minimum_announces_every_turn() {
        for worked in [None, Some(Duration::ZERO), Some(Duration::from_millis(60))] {
            assert!(idle_announce_allowed(worked, Duration::ZERO), "{worked:?}");
        }
    }

    // ── turn spans ─────────────────────────────────────────────────────────

    /// The failure this whole change exists for: on a Claude tab the activity
    /// heuristic cycles `Thinking → Idle → Thinking` once per tool-call output
    /// burst, so ONE turn produces a series of short spans. The turn clock must
    /// be the first burst's, and the pushed boundary must judge the WHOLE turn.
    #[test]
    fn intermediate_idle_bursts_do_not_restart_the_turn_clock() {
        let tab = TabId::from_str("claude");
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        let min = Duration::from_secs(120);

        gate.note_avatar_state(&tab, AvatarState::Listening, t0);
        gate.note_avatar_state(&tab, AvatarState::Thinking, t0);
        // Three tool-call bursts inside the one turn. A pushed-boundary tab
        // never asks the gate about these edges (the manager returns before
        // `close_turn`), and they must leave the open turn alone.
        for i in 1..=3 {
            let at = t0 + Duration::from_secs(i * 20);
            gate.note_avatar_state(&tab, AvatarState::Idle, at);
            gate.note_avatar_state(&tab, AvatarState::Thinking, at + Duration::from_millis(60));
        }
        // …and the mid-turn TTS interlude is not a restart either.
        gate.note_avatar_state(&tab, AvatarState::Speaking, t0 + Duration::from_secs(90));
        gate.note_avatar_state(&tab, AvatarState::Thinking, t0 + Duration::from_secs(95));

        // The push lands at 130 s: judged on the whole turn, so it announces —
        // exactly once, because closing is read-and-clear.
        let at = t0 + Duration::from_secs(130);
        assert_eq!(gate.close_turn(&tab, at, min), IdleVerdict::Announce);
        assert_eq!(
            gate.close_turn(&tab, at, min),
            IdleVerdict::TooShort { worked: None },
            "a second close of the same turn has nothing left to announce"
        );
    }

    /// The same turn shape, ended at 5 s: no announcement at all — not on the
    /// intermediate bursts (the manager never asks) and not at the boundary.
    #[test]
    fn a_short_pushed_turn_announces_nothing() {
        let tab = TabId::from_str("claude");
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        gate.note_avatar_state(&tab, AvatarState::Thinking, t0);
        gate.note_avatar_state(&tab, AvatarState::Idle, t0 + Duration::from_millis(500));
        gate.note_avatar_state(&tab, AvatarState::Thinking, t0 + Duration::from_millis(700));
        assert_eq!(
            gate.close_turn(&tab, t0 + Duration::from_secs(5), Duration::from_secs(120)),
            IdleVerdict::TooShort {
                worked: Some(Duration::from_secs(5))
            }
        );
    }

    /// A harness with no push (OpenCode, and any unclassified tab) closes its
    /// turn on the avatar's own Idle edge — one per turn there, because that
    /// edge comes from the SSE `session.idle` rather than from an output-burst
    /// heuristic.
    #[test]
    fn a_tab_without_a_push_closes_its_turn_on_the_idle_edge() {
        let tab = TabId::from_str("opencode");
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        let min = Duration::from_secs(120);
        gate.note_avatar_state(&tab, AvatarState::Thinking, t0);
        let at = t0 + Duration::from_secs(200);
        gate.note_avatar_state(&tab, AvatarState::Idle, at);
        assert_eq!(gate.close_turn(&tab, at, min), IdleVerdict::Announce);
    }

    #[test]
    fn a_zero_minimum_announces_a_pushed_turn_however_short() {
        let tab = TabId::from_str("claude");
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        gate.note_avatar_state(&tab, AvatarState::Thinking, t0);
        assert_eq!(
            gate.close_turn(&tab, t0 + Duration::from_millis(60), Duration::ZERO),
            IdleVerdict::Announce
        );
        // And with no turn open at all.
        assert_eq!(gate.close_turn(&tab, t0, Duration::ZERO), IdleVerdict::Announce);
    }

    #[test]
    fn user_input_ends_the_turn_and_the_next_one_starts_fresh() {
        let tab = TabId::from_str("claude");
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        let min = Duration::from_secs(120);
        gate.note_avatar_state(&tab, AvatarState::Thinking, t0);
        // The user types: whatever was running is no longer the turn we would
        // announce, so the clock is dropped rather than carried into the next.
        gate.note_avatar_state(&tab, AvatarState::Listening, t0 + Duration::from_secs(300));
        gate.note_avatar_state(&tab, AvatarState::Thinking, t0 + Duration::from_secs(301));
        assert_eq!(
            gate.close_turn(&tab, t0 + Duration::from_secs(310), min),
            IdleVerdict::TooShort {
                worked: Some(Duration::from_secs(9))
            },
            "the new turn is 9 s old, not 310"
        );
    }

    #[test]
    fn spans_are_per_tab_and_a_closed_tab_is_forgotten() {
        let a = TabId::from_str("claude");
        let b = TabId::from_str("opencode");
        let t0 = Instant::now();
        let mut gate = IdleGate::default();
        let min = Duration::from_secs(120);
        gate.note_avatar_state(&a, AvatarState::Thinking, t0);
        gate.note_avatar_state(&b, AvatarState::Thinking, t0 + Duration::from_secs(200));
        let at = t0 + Duration::from_secs(210);
        assert_eq!(gate.close_turn(&a, at, min), IdleVerdict::Announce);
        assert_eq!(
            gate.close_turn(&b, at, min),
            IdleVerdict::TooShort {
                worked: Some(Duration::from_secs(10))
            }
        );
        gate.note_avatar_state(&a, AvatarState::Thinking, t0);
        gate.forget(&a);
        assert_eq!(
            gate.close_turn(&a, at, min),
            IdleVerdict::TooShort { worked: None }
        );
    }

    #[test]
    fn interpolate_passes_through_when_no_token() {
        assert_eq!(
            interpolate_code("Shell encountered an error", Some(1)),
            "Shell encountered an error"
        );
        assert_eq!(
            interpolate_code("Shell encountered an error", None),
            "Shell encountered an error"
        );
    }
}
