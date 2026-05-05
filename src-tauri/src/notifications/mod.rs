//! Audible notifications for cross-tab events.
//!
//! When the user is focused on one tab and another tab transitions into a
//! notable state (Idle, AwaitingPermission, Error), this module queues a
//! short spoken announcement and plays it as soon as the current TTS audio
//! finishes. Per-tab dedup at play-time means a tab firing several
//! notifications in quick succession only speaks the most recent one.
//!
//! Inputs:
//! - `broadcast::Receiver<StateEvent>` from the state manager (avatar/perm
//!   edges).
//! - `Notify` from `AudioOutput` (idle edges) so we drain right when the
//!   sink empties.
//!
//! Output: `TtsRequest::SynthesizeNotification` on the existing TTS mpsc.
//! The worker synthesizes; the audio sink plays sequentially.

mod manager;

pub use manager::spawn_notification_manager;
