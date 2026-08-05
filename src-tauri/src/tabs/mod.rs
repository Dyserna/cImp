//! Tab registry: owns N per-tab `PtyManager`s and routes activation events
//! through the shared audio output, TTS active-tab cell, and state-manager
//! channel. The registry is the single seam through which the IPC layer
//! interacts with multi-tab subprocess state.

mod config;
pub mod registry;

/// NC-2: Claude tabs + their launch directories, for the `/permission/event`
/// route's cwd fallback when a hook payload can't be matched by session id.
pub(crate) use config::claude_tab_dirs;
/// The advertised-MCP-server signature, re-exported for the Settings save
/// path's restart-hint edge detector (`ipc::commands::settings_update`).
pub(crate) use config::spawn_inject_sig;
pub use registry::{TabMetaWire, TabRegistry, TabRegistryHandle};
