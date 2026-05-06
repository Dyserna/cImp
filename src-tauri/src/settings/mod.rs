//! Settings module: load/save JSON, in-memory store with broadcast updates.
//!
//! The store is the single source of truth for runtime configuration. The
//! broadcast channel propagates updates to subscribers (TTS engine, audio
//! output, processing layer, frontend). Saves are debounced (~500ms) so a
//! slider drag doesn't write the file on every frame.

mod broadcaster;
mod migration;
mod persistence;
mod schema;

pub use broadcaster::SettingsHandle;
pub use schema::*;

use crate::shell::ShellSpec;

/// Bring up the settings store from disk (or defaults). Always succeeds —
/// missing/corrupt files are recovered with defaults; v1 / v1.1 files are
/// migrated to v1.2 and a backup is written. The default shell is needed
/// to fill in the platform-specific Shell-1 entry on fresh installs and
/// when migration consumes the legacy `_shell_1_tmp` interim key.
pub fn init(default_shell: &ShellSpec) -> SettingsHandle {
    let initial = persistence::load(default_shell);
    SettingsHandle::new(initial)
}
