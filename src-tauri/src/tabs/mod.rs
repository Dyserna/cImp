//! Tab registry: owns N per-tab `PtyManager`s and routes activation events
//! through the shared audio output, TTS active-tab cell, and state-manager
//! channel. The registry is the single seam through which the IPC layer
//! interacts with multi-tab subprocess state.

/// `pub(crate)` only so `settings::injection::Consumer::for_command` can reuse
/// [`config::command_is`] (#48). The launch path's own split — `build_pre_args`
/// is Claude-only, `build_opencode_config` takes everything else — is what the
/// per-consumer spawn signature has to partition tabs by, and a second copy of
/// "is this a claude tab" in the settings module is exactly the kind of mirror
/// #47 spent a milestone removing. Items inside the module keep their own
/// visibility; nothing new is public.
pub(crate) mod config;
pub mod registry;

/// NC-2: Claude tabs + their launch directories, for the `/permission/event`
/// route's cwd fallback when a hook payload can't be matched by session id.
pub(crate) use config::claude_tab_dirs;
/// The advertised-MCP-server signature, re-exported for the Settings save
/// path's restart-hint edge detector (`ipc::commands::settings_update`).
pub(crate) use config::spawn_inject_sig;
pub use registry::{TabMetaWire, TabRegistry, TabRegistryHandle};
