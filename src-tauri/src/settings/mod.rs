//! Settings module: load/save JSON, in-memory store with broadcast updates.
//!
//! The store is the single source of truth for runtime configuration. The
//! broadcast channel propagates updates to subscribers (TTS engine, audio
//! output, processing layer, frontend). Saves are debounced (~500ms) so a
//! slider drag doesn't write the file on every frame.

mod broadcaster;
mod persistence;
mod schema;

pub use broadcaster::SettingsHandle;
pub use schema::*;

/// Bring up the settings store from disk (or defaults). Always succeeds —
/// missing/corrupt files are recovered with defaults.
pub fn init() -> SettingsHandle {
    let initial = persistence::load();
    SettingsHandle::new(initial)
}
