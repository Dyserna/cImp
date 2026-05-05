use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Identifier for one of the multi-tab subprocesses cctts owns. Closed enum
/// in v2 (Claude + Aider only); v3 will likely widen this to allow arbitrary
/// user-managed tabs.
#[derive(Clone, Copy, Hash, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabId {
    Claude,
    Aider,
}

impl TabId {
    pub const ALL: [TabId; 2] = [TabId::Claude, TabId::Aider];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    SubprocessExited,
    TtsError,
    AudioError,
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorInfo {
    pub tab: TabId,
    pub kind: ErrorKind,
    pub message: &'static str,
}

impl ErrorInfo {
    fn from_signal(s: StateSignal) -> Option<Self> {
        let (kind, message) = match s {
            StateSignal::SubprocessExited { .. } => (ErrorKind::SubprocessExited, "Subprocess stopped."),
            StateSignal::TtsError { .. } => (ErrorKind::TtsError, "Text-to-speech is unavailable."),
            StateSignal::AudioError { .. } => (ErrorKind::AudioError, "Audio output is unavailable."),
            _ => return None,
        };
        Some(Self {
            tab: s.tab(),
            kind,
            message,
        })
    }
}

/// Auto-leave Listening when the input has been empty AND idle this long.
/// Same rule as v1, applied per-tab.
const EMPTY_INPUT_IDLE: Duration = Duration::from_secs(5);

/// Tick rate for the auto-leave-Listening sweep across all tabs.
const TICK: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum AvatarState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Error,
}

/// Signals consumed by the state machine. Every variant carries the tab it
/// originated from (or, for `TabActivated`, the tab that's becoming active).
/// Transition logic mirrors v1's per-state-machine rules; the manager just
/// runs them per tab and routes events back tagged with the same TabId.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // some variants reserved for later milestones
pub enum StateSignal {
    UserKeystroke { tab: TabId },
    UserSubmit { tab: TabId },
    ClaudeOutputStarted { tab: TabId },
    ClaudeOutputStopped { tab: TabId },
    TtsPlaybackStarted { tab: TabId },
    TtsPlaybackStopped { tab: TabId },
    SubprocessExited { tab: TabId },
    AudioError { tab: TabId },
    TtsError { tab: TabId },
    ErrorAcknowledged { tab: TabId },
    /// Compose-overlay textarea content crossed the empty/non-empty edge.
    /// Always routed to the active tab (compose targets whoever is on
    /// screen).
    ComposeContentChanged { tab: TabId, non_empty: bool },
    /// User activated a tab (click or Ctrl+N). Updates `active` and
    /// broadcasts so the frontend can swap avatar/terminal visuals.
    TabActivated { tab: TabId },
    /// Permission detector saw a known prompt pattern in the rendered tail.
    /// Sets `awaiting_permission` on the tab; does NOT drive the avatar
    /// state machine.
    PermissionPromptDetected { tab: TabId },
    /// Permission detector observed the previously-matched pattern leave the
    /// rendered tail. Clears `awaiting_permission`.
    PermissionPromptResolved { tab: TabId },
}

impl StateSignal {
    pub fn tab(&self) -> TabId {
        match *self {
            Self::UserKeystroke { tab }
            | Self::UserSubmit { tab }
            | Self::ClaudeOutputStarted { tab }
            | Self::ClaudeOutputStopped { tab }
            | Self::TtsPlaybackStarted { tab }
            | Self::TtsPlaybackStopped { tab }
            | Self::SubprocessExited { tab }
            | Self::AudioError { tab }
            | Self::TtsError { tab }
            | Self::ErrorAcknowledged { tab }
            | Self::ComposeContentChanged { tab, .. }
            | Self::TabActivated { tab }
            | Self::PermissionPromptDetected { tab }
            | Self::PermissionPromptResolved { tab } => tab,
        }
    }
}

/// Frontend-facing events emitted via the Tauri AppHandle. Kept distinct from
/// the input `StateSignal` so the wire format can evolve without disturbing
/// the internal signal vocabulary.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[allow(dead_code)] // some variants are consumed only by the frontend
// Variant names are the IPC wire format (kebab-case after serde); renaming
// to satisfy the lint would break the frontend contract.
#[allow(clippy::enum_variant_names)]
pub enum StateEvent {
    StateChanged { tab: TabId, state: AvatarState },
    ActiveTabChanged { tab: TabId },
    AwaitingPermissionChanged { tab: TabId, awaiting: bool },
    DoneWhileAwayChanged { tab: TabId, done: bool },
}

#[derive(Clone, Debug)]
struct TabState {
    avatar_state: AvatarState,
    has_unsent_input: bool,
    composing: bool,
    last_keystroke_at: Option<Instant>,
    /// Set by permission detection; cleared on detector-resolve, user input,
    /// or tab activation triggering Claude to emit further output.
    awaiting_permission: bool,
    /// UI-derived: tab transitioned to Idle while inactive. Cleared on
    /// activation. Independent of `avatar_state` and `awaiting_permission`.
    done_while_away: bool,
}

impl TabState {
    fn new() -> Self {
        Self {
            avatar_state: AvatarState::Idle,
            has_unsent_input: false,
            composing: false,
            last_keystroke_at: None,
            awaiting_permission: false,
            done_while_away: false,
        }
    }
}

/// Spawn the state-manager task. The channel is created at app startup so
/// AppState can hold a clone of the sender before the AppHandle exists.
///
/// `state_events` receives the same `StateEvent`s emitted to the frontend,
/// so in-process subscribers (e.g. the notification manager) can react to
/// state edges without going through the IPC layer.
pub fn spawn_state_manager(
    app: AppHandle,
    rx: mpsc::Receiver<StateSignal>,
    state_events: broadcast::Sender<StateEvent>,
    input_lengths: HashMap<TabId, Arc<AtomicI32>>,
    initial_active: TabId,
) {
    tauri::async_runtime::spawn(async move {
        run(app, rx, state_events, input_lengths, initial_active).await;
    });
}

async fn run(
    app: AppHandle,
    mut rx: mpsc::Receiver<StateSignal>,
    state_events: broadcast::Sender<StateEvent>,
    input_lengths: HashMap<TabId, Arc<AtomicI32>>,
    initial_active: TabId,
) {
    let mut tabs: HashMap<TabId, TabState> = TabId::ALL
        .iter()
        .map(|&t| (t, TabState::new()))
        .collect();
    let mut active = initial_active;

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Emit the initial Idle for each tab so the frontend has a baseline before
    // any signal arrives. The avatar component skips its first-render
    // transition so this doesn't play an unwanted animation.
    for (&tab, ts) in &tabs {
        emit_state(&app, &state_events, tab, ts.avatar_state);
    }
    emit_active_tab(&app, &state_events, active);

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(signal) = maybe else { break };

                // TabActivated isn't a per-tab transition — it just moves the
                // active pointer and re-broadcasts. We DON'T re-emit the new
                // tab's state here; the frontend listens for ActiveTabChanged
                // and re-derives from the per-tab cache it already has.
                if let StateSignal::TabActivated { tab } = signal {
                    if active != tab {
                        info!(from = ?active, to = ?tab, "active tab");
                        active = tab;
                        emit_active_tab(&app, &state_events, active);
                        // Clear DoneWhileAway on the newly-active tab — the
                        // user's now looking at it, so the "you missed
                        // something" hint has served its purpose.
                        if let Some(ts) = tabs.get_mut(&tab) {
                            if ts.done_while_away {
                                ts.done_while_away = false;
                                emit_done_while_away(&app, &state_events, tab, false);
                            }
                        }
                    }
                    continue;
                }

                // Permission-prompt edges are independent of the avatar state
                // machine — they only flip `awaiting_permission`. Resolved
                // and user-input both clear; the input clearing path below
                // handles UserKeystroke / UserSubmit.
                if let StateSignal::PermissionPromptDetected { tab } = signal {
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if !ts.awaiting_permission {
                            ts.awaiting_permission = true;
                            info!(?tab, "awaiting permission: set");
                            emit_awaiting_permission(&app, &state_events, tab, true);
                        }
                    }
                    continue;
                }
                if let StateSignal::PermissionPromptResolved { tab } = signal {
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if ts.awaiting_permission {
                            ts.awaiting_permission = false;
                            info!(?tab, "awaiting permission: cleared (resolved)");
                            emit_awaiting_permission(&app, &state_events, tab, false);
                        }
                    }
                    continue;
                }

                // Compose signals always target the active tab (the compose
                // overlay submits to whoever is on screen). The signal
                // arrives tagged with `active` from the IPC handler, but we
                // re-resolve here defensively in case anything ever changes.
                let target_tab = match signal {
                    StateSignal::ComposeContentChanged { .. } => active,
                    other => other.tab(),
                };

                let Some(ts) = tabs.get_mut(&target_tab) else { continue };

                match signal {
                    StateSignal::UserKeystroke { .. } => {
                        ts.has_unsent_input = true;
                        ts.last_keystroke_at = Some(Instant::now());
                    }
                    StateSignal::UserSubmit { .. } => {
                        ts.has_unsent_input = false;
                        ts.last_keystroke_at = Some(Instant::now());
                    }
                    StateSignal::ComposeContentChanged { non_empty, .. } => {
                        ts.composing = non_empty;
                        if non_empty {
                            ts.last_keystroke_at = Some(Instant::now());
                        }
                    }
                    _ => {}
                }

                // Input-driven clearing of awaiting_permission. Either the
                // user typed a yes/no into the prompt, or they're typing
                // unrelated text on a tab that wasn't actually prompting —
                // clearing an already-false flag below is a no-op.
                if matches!(
                    signal,
                    StateSignal::UserKeystroke { .. } | StateSignal::UserSubmit { .. }
                ) && ts.awaiting_permission
                {
                    ts.awaiting_permission = false;
                    info!(tab = ?target_tab, "awaiting permission: cleared (input)");
                    emit_awaiting_permission(&app, &state_events, target_tab, false);
                }

                let prev_state = ts.avatar_state;
                let next = transition(prev_state, signal, ts.has_unsent_input, ts.composing);
                if next != prev_state {
                    info!(tab = ?target_tab, from = ?prev_state, to = ?next, ?signal, "avatar state");
                    ts.avatar_state = next;
                    emit_state(&app, &state_events, target_tab, next);
                    if next == AvatarState::Error {
                        if let Some(info) = ErrorInfo::from_signal(signal) {
                            emit_error(&app, &info);
                        }
                    }
                    // DoneWhileAway: a transition INTO Idle on an inactive tab
                    // raises the flag. Same rule applies on the tick-arm
                    // Listening→Idle sweep below.
                    if next == AvatarState::Idle
                        && target_tab != active
                        && !ts.done_while_away
                    {
                        ts.done_while_away = true;
                        info!(tab = ?target_tab, "done while away: set");
                        emit_done_while_away(&app, &state_events, target_tab, true);
                    }
                }
            }
            _ = tick.tick() => {
                // Per-tab idle-Listening sweep. Each tab's input-length
                // counter is independent.
                for (&tab, ts) in tabs.iter_mut() {
                    if ts.avatar_state != AvatarState::Listening { continue; }
                    if ts.composing { continue; }
                    let len = input_lengths
                        .get(&tab)
                        .map(|c| c.load(Ordering::Relaxed))
                        .unwrap_or(0);
                    if len != 0 { continue; }
                    let idle_long_enough = ts
                        .last_keystroke_at
                        .map(|t| t.elapsed() >= EMPTY_INPUT_IDLE)
                        .unwrap_or(true);
                    if !idle_long_enough { continue; }
                    info!(?tab, from = ?ts.avatar_state, to = ?AvatarState::Idle, signal = "EmptyInputTimeout", "avatar state");
                    ts.avatar_state = AvatarState::Idle;
                    ts.has_unsent_input = false;
                    emit_state(&app, &state_events, tab, ts.avatar_state);
                    if tab != active && !ts.done_while_away {
                        ts.done_while_away = true;
                        info!(?tab, "done while away: set (tick)");
                        emit_done_while_away(&app, &state_events, tab, true);
                    }
                }
            }
        }
    }

    debug!("state manager: signal channel closed; exiting");
}

/// Priority-based transitions, identical to v1's logic. The `tab` carried by
/// each signal is consumed by the caller (it routes the signal to the right
/// per-tab `TabState` before invoking this).
fn transition(
    current: AvatarState,
    signal: StateSignal,
    has_unsent_input: bool,
    composing: bool,
) -> AvatarState {
    use AvatarState::*;
    use StateSignal::*;

    if matches!(
        signal,
        SubprocessExited { .. } | AudioError { .. } | TtsError { .. }
    ) {
        return Error;
    }

    if let ComposeContentChanged { non_empty, .. } = signal {
        if non_empty && current == Idle {
            return Listening;
        }
        return current;
    }

    match (current, signal) {
        (Error, ErrorAcknowledged { .. }) => Idle,
        (Error, _) => Error,

        (Speaking, TtsPlaybackStopped { .. }) => {
            if has_unsent_input || composing {
                Listening
            } else {
                Idle
            }
        }
        (Speaking, _) => Speaking,

        (Thinking, TtsPlaybackStarted { .. }) => Speaking,
        (Thinking, ClaudeOutputStopped { .. }) => Idle,
        (Thinking, _) => Thinking,

        (Listening, UserSubmit { .. }) => Thinking,
        (Listening, TtsPlaybackStarted { .. }) => Speaking,
        (Listening, _) => Listening,

        (Idle, UserKeystroke { .. }) => Listening,
        (Idle, TtsPlaybackStarted { .. }) => Speaking,
        (Idle, _) => Idle,
    }
}

/// Frontend `app.emit` + in-process broadcast share the same event payload.
/// `broadcast::send` returns Err only when there are zero subscribers, which
/// is the normal case at startup, so we drop that result silently.
fn dispatch(app: &AppHandle, bcast: &broadcast::Sender<StateEvent>, event: StateEvent) {
    if let Err(e) = app.emit("avatar-state", &event) {
        warn!(error = %e, "failed to emit avatar-state");
    }
    let _ = bcast.send(event);
}

fn emit_state(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    state: AvatarState,
) {
    dispatch(app, bcast, StateEvent::StateChanged { tab, state });
}

fn emit_active_tab(app: &AppHandle, bcast: &broadcast::Sender<StateEvent>, tab: TabId) {
    dispatch(app, bcast, StateEvent::ActiveTabChanged { tab });
}

fn emit_error(app: &AppHandle, info: &ErrorInfo) {
    if let Err(e) = app.emit("avatar-error", info) {
        warn!(error = %e, "failed to emit avatar-error");
    }
}

fn emit_awaiting_permission(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    awaiting: bool,
) {
    dispatch(app, bcast, StateEvent::AwaitingPermissionChanged { tab, awaiting });
}

fn emit_done_while_away(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    done: bool,
) {
    dispatch(app, bcast, StateEvent::DoneWhileAwayChanged { tab, done });
}

#[cfg(test)]
mod tests {
    use super::*;
    use AvatarState::*;
    use StateSignal::*;

    const T: TabId = TabId::Claude;

    fn t(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, signal, false, false)
    }

    fn t_with_input(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, signal, true, false)
    }

    fn t_composing(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, signal, false, true)
    }

    #[test]
    fn idle_keystroke_listens() {
        assert_eq!(t(Idle, UserKeystroke { tab: T }), Listening);
    }

    #[test]
    fn idle_bare_enter_stays_idle() {
        assert_eq!(t(Idle, UserSubmit { tab: T }), Idle);
    }

    #[test]
    fn listening_enter_thinks() {
        assert_eq!(t(Listening, UserSubmit { tab: T }), Thinking);
    }

    #[test]
    fn listening_more_typing_stays() {
        assert_eq!(t(Listening, UserKeystroke { tab: T }), Listening);
    }

    #[test]
    fn listening_tts_speaks() {
        assert_eq!(t(Listening, TtsPlaybackStarted { tab: T }), Speaking);
    }

    #[test]
    fn thinking_tts_speaks() {
        assert_eq!(t(Thinking, TtsPlaybackStarted { tab: T }), Speaking);
    }

    #[test]
    fn thinking_claude_done_returns_idle() {
        assert_eq!(t(Thinking, ClaudeOutputStopped { tab: T }), Idle);
    }

    #[test]
    fn thinking_typing_or_enter_ignored() {
        assert_eq!(t(Thinking, UserKeystroke { tab: T }), Thinking);
        assert_eq!(t(Thinking, UserSubmit { tab: T }), Thinking);
    }

    #[test]
    fn speaking_tts_stop_returns_idle_when_no_pending_input() {
        assert_eq!(t(Speaking, TtsPlaybackStopped { tab: T }), Idle);
    }

    #[test]
    fn speaking_tts_stop_resumes_listening_when_user_typed() {
        assert_eq!(
            t_with_input(Speaking, TtsPlaybackStopped { tab: T }),
            Listening
        );
    }

    #[test]
    fn speaking_typing_or_enter_ignored() {
        assert_eq!(t(Speaking, UserKeystroke { tab: T }), Speaking);
        assert_eq!(t(Speaking, UserSubmit { tab: T }), Speaking);
    }

    #[test]
    fn errors_interrupt_any_state() {
        for s in [Idle, Listening, Thinking, Speaking] {
            assert_eq!(t(s, SubprocessExited { tab: T }), Error);
            assert_eq!(t(s, AudioError { tab: T }), Error);
            assert_eq!(t(s, TtsError { tab: T }), Error);
        }
    }

    #[test]
    fn idle_compose_non_empty_listens() {
        assert_eq!(
            t(Idle, ComposeContentChanged { tab: T, non_empty: true }),
            Listening,
        );
    }

    #[test]
    fn idle_compose_empty_stays_idle() {
        assert_eq!(
            t(Idle, ComposeContentChanged { tab: T, non_empty: false }),
            Idle
        );
    }

    #[test]
    fn compose_does_not_preempt_higher_states() {
        assert_eq!(
            t(Thinking, ComposeContentChanged { tab: T, non_empty: true }),
            Thinking,
        );
        assert_eq!(
            t(Speaking, ComposeContentChanged { tab: T, non_empty: true }),
            Speaking,
        );
    }

    #[test]
    fn speaking_tts_stop_resumes_listening_when_composing() {
        assert_eq!(
            t_composing(Speaking, TtsPlaybackStopped { tab: T }),
            Listening
        );
    }

    #[test]
    fn error_sticks_until_acknowledged() {
        assert_eq!(t(Error, UserKeystroke { tab: T }), Error);
        assert_eq!(t(Error, UserSubmit { tab: T }), Error);
        assert_eq!(t(Error, TtsPlaybackStarted { tab: T }), Error);
        assert_eq!(t(Error, ErrorAcknowledged { tab: T }), Idle);
    }

    #[test]
    fn permission_signals_dont_drive_avatar() {
        // Defensive: PermissionPromptDetected/Resolved short-circuit before
        // the run loop calls `transition()`, but if they ever reached it
        // they should be no-ops in every state.
        for s in [Idle, Listening, Thinking, Speaking, Error] {
            assert_eq!(t(s, PermissionPromptDetected { tab: T }), s);
            assert_eq!(t(s, PermissionPromptResolved { tab: T }), s);
        }
    }
}
