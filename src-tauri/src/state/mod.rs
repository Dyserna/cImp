//! Avatar state machine.
//!
//! The state machine itself is unaware of visuals — it consumes raw
//! [`StateSignal`]s from the PTY, processing layer, TTS engine, and audio
//! output, computes the new [`AvatarState`], and broadcasts changes to the
//! frontend via the `avatar-state` Tauri event. Per the design doc, this is
//! the single seam that turns "what's happening" into "what to render."

mod manager;

pub use manager::{spawn_state_manager, StateSignal};
