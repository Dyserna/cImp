use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Auto-leave Listening when the input has been empty AND idle this long.
/// Matches the user's stated rule: "if I clear the input and don't type
/// for 5 seconds, switch to Idle." Stays in Listening if there's still
/// pending text in the input box, regardless of how long it's been.
const EMPTY_INPUT_IDLE: Duration = Duration::from_secs(5);

/// How often we poll the auto-leave condition. Cheap (an atomic load),
/// and the latency it introduces (≤500ms after the 5s threshold) is
/// imperceptible.
const TICK: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum AvatarState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Error,
}

/// Signals consumed by the state machine. The model is priority-based:
/// Error > Speaking > Thinking > Listening > Idle. A signal that would
/// drop to a lower-priority state is ignored.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // some variants reserved for later milestones
pub enum StateSignal {
    /// Any non-Enter keystroke. Promotes Idle → Listening; ignored from
    /// higher states.
    UserKeystroke,
    /// Enter key (or input ending in CR). Promotes Listening → Thinking;
    /// ignored from higher states or from Idle (no message to submit).
    UserSubmit,
    /// Real claude response burst ended (the processor's burst detector
    /// fires this only after a sustained ≥1s byte stream goes quiet for
    /// 500ms — so it's specific to actual responses, not TUI churn).
    /// Drops Thinking → Idle.
    ClaudeOutputStopped,
    /// TTS audio started playing. Promotes anything below Error to
    /// Speaking.
    TtsPlaybackStarted,
    /// TTS audio finished. Drops Speaking → Idle.
    TtsPlaybackStopped,
    /// Engine/process failures — interrupt anything below Error.
    SubprocessExited,
    AudioError,
    TtsError,
    /// User dismissed the error (no UI for this in M4; here for symmetry
    /// and for later milestones).
    ErrorAcknowledged,
    /// Reserved for the byte-burst detector. Currently unused by the
    /// state machine — kept so the processor doesn't have to learn a new
    /// signal vocabulary, and so we have an obvious hook if a future
    /// model wants it.
    ClaudeOutputStarted,
}

/// Spawn the state-manager task. The channel is created at app startup so
/// AppState can hold a clone of the sender before the AppHandle exists; this
/// function is called from the Tauri `setup` hook with the deferred receiver.
pub fn spawn_state_manager(
    app: AppHandle,
    rx: mpsc::Receiver<StateSignal>,
    input_length: Arc<AtomicI32>,
) {
    tauri::async_runtime::spawn(async move {
        run(app, rx, input_length).await;
    });
}

async fn run(
    app: AppHandle,
    mut rx: mpsc::Receiver<StateSignal>,
    input_length: Arc<AtomicI32>,
) {
    let mut current = AvatarState::Idle;
    // Tracks whether the user has typed at least one keystroke since the
    // last submit. We can't see Claude's input box from here, so this is
    // an approximation — it'll be true if the box has content, but also
    // stays true if the user backspaced everything without sending.
    // Drives the (Speaking → ?) decision when TTS ends: if the user has
    // unsent input we resume to Listening rather than dropping to Idle.
    let mut has_unsent_input = false;
    // Most recent keystroke timestamp. Drives the auto-leave-Listening
    // tick: we only drop Listening → Idle when the input is empty AND
    // there's been no typing for EMPTY_INPUT_IDLE.
    let mut last_keystroke_at: Option<Instant> = None;

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Emit the initial Idle so the frontend store mirrors backend truth even
    // before any signal arrives. The avatar component knows not to play a
    // transition for the very first state assignment.
    emit(&app, current);

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(signal) = maybe else {
                    break;
                };
                match signal {
                    StateSignal::UserKeystroke => {
                        has_unsent_input = true;
                        last_keystroke_at = Some(Instant::now());
                    }
                    StateSignal::UserSubmit => {
                        has_unsent_input = false;
                        last_keystroke_at = Some(Instant::now());
                    }
                    _ => {}
                }
                let next = transition(current, signal, has_unsent_input);
                if next != current {
                    info!(from = ?current, to = ?next, ?signal, "avatar state");
                    current = next;
                    emit(&app, current);
                }
            }
            _ = tick.tick() => {
                if current == AvatarState::Listening
                    && input_length.load(Ordering::Relaxed) == 0
                    && last_keystroke_at
                        .map(|t| t.elapsed() >= EMPTY_INPUT_IDLE)
                        .unwrap_or(true)
                {
                    info!(
                        from = ?current,
                        to = ?AvatarState::Idle,
                        signal = "EmptyInputTimeout",
                        "avatar state"
                    );
                    current = AvatarState::Idle;
                    has_unsent_input = false;
                    emit(&app, current);
                }
            }
        }
    }

    debug!("state manager: signal channel closed; exiting");
}

/// Priority-based transitions: Error > Speaking > Thinking > Listening
/// > Idle. A signal that would drop to a lower-priority state is
/// ignored, except for the explicit "downgrade" signals (TtsPlaybackStopped
/// from Speaking → Idle/Listening, ClaudeOutputStopped from Thinking → Idle,
/// and ErrorAcknowledged from Error → Idle).
fn transition(
    current: AvatarState,
    signal: StateSignal,
    has_unsent_input: bool,
) -> AvatarState {
    use AvatarState::*;
    use StateSignal::*;

    // Errors always interrupt — they're the top of the priority stack.
    if matches!(signal, SubprocessExited | AudioError | TtsError) {
        return Error;
    }

    match (current, signal) {
        // Error sticks until explicitly acknowledged.
        (Error, ErrorAcknowledged) => Idle,
        (Error, _) => Error,

        // Speaking only ends when TTS playback ends. If the user has been
        // typing during/before the TTS clip and hasn't submitted, resume
        // to Listening rather than blinking through Idle. Otherwise Idle.
        (Speaking, TtsPlaybackStopped) => {
            if has_unsent_input {
                Listening
            } else {
                Idle
            }
        }
        (Speaking, _) => Speaking,

        // Thinking can be promoted to Speaking by TTS, or downgraded to
        // Idle by the burst-detector signaling a real response is done.
        // Typing/Enter while Thinking are ignored (per priority rule).
        (Thinking, TtsPlaybackStarted) => Speaking,
        (Thinking, ClaudeOutputStopped) => Idle,
        (Thinking, _) => Thinking,

        // Listening is left only by Enter (→ Thinking) or TTS (→ Speaking).
        // No 2-second auto-timeout: stays until the user submits or
        // something higher-priority happens.
        (Listening, UserSubmit) => Thinking,
        (Listening, TtsPlaybackStarted) => Speaking,
        (Listening, _) => Listening,

        // Idle: typing promotes to Listening; TTS promotes to Speaking.
        // A bare Enter on an empty input box does nothing.
        (Idle, UserKeystroke) => Listening,
        (Idle, TtsPlaybackStarted) => Speaking,
        (Idle, _) => Idle,
    }
}

fn emit(app: &AppHandle, state: AvatarState) {
    if let Err(e) = app.emit("avatar-state", state) {
        warn!(error = %e, "failed to emit avatar-state");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AvatarState::*;
    use StateSignal::*;

    fn t(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, signal, false)
    }

    fn t_with_input(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, signal, true)
    }

    #[test]
    fn idle_keystroke_listens() {
        assert_eq!(t(Idle, UserKeystroke), Listening);
    }

    #[test]
    fn idle_bare_enter_stays_idle() {
        assert_eq!(t(Idle, UserSubmit), Idle);
    }

    #[test]
    fn listening_enter_thinks() {
        assert_eq!(t(Listening, UserSubmit), Thinking);
    }

    #[test]
    fn listening_more_typing_stays() {
        assert_eq!(t(Listening, UserKeystroke), Listening);
    }

    #[test]
    fn listening_tts_speaks() {
        assert_eq!(t(Listening, TtsPlaybackStarted), Speaking);
    }

    #[test]
    fn thinking_tts_speaks() {
        assert_eq!(t(Thinking, TtsPlaybackStarted), Speaking);
    }

    #[test]
    fn thinking_claude_done_returns_idle() {
        assert_eq!(t(Thinking, ClaudeOutputStopped), Idle);
    }

    #[test]
    fn thinking_typing_or_enter_ignored() {
        assert_eq!(t(Thinking, UserKeystroke), Thinking);
        assert_eq!(t(Thinking, UserSubmit), Thinking);
    }

    #[test]
    fn speaking_tts_stop_returns_idle_when_no_pending_input() {
        assert_eq!(t(Speaking, TtsPlaybackStopped), Idle);
    }

    #[test]
    fn speaking_tts_stop_resumes_listening_when_user_typed() {
        assert_eq!(t_with_input(Speaking, TtsPlaybackStopped), Listening);
    }

    #[test]
    fn speaking_typing_or_enter_ignored() {
        assert_eq!(t(Speaking, UserKeystroke), Speaking);
        assert_eq!(t(Speaking, UserSubmit), Speaking);
    }

    #[test]
    fn errors_interrupt_any_state() {
        for s in [Idle, Listening, Thinking, Speaking] {
            assert_eq!(t(s, SubprocessExited), Error);
            assert_eq!(t(s, AudioError), Error);
            assert_eq!(t(s, TtsError), Error);
        }
    }

    #[test]
    fn error_sticks_until_acknowledged() {
        assert_eq!(t(Error, UserKeystroke), Error);
        assert_eq!(t(Error, UserSubmit), Error);
        assert_eq!(t(Error, TtsPlaybackStarted), Error);
        assert_eq!(t(Error, ErrorAcknowledged), Idle);
    }
}
