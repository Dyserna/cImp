//! Tab registry: owns N per-tab `PtyManager`s and routes activation events
//! through the shared audio output, TTS active-tab cell, and state-manager
//! channel. The registry is the single seam through which the IPC layer
//! interacts with multi-tab subprocess state.

/// `pub(crate)` so the launch composer's harness-neutral helpers are reachable
/// from the plugins and from `settings::injection` (#48). Since V40 Phase A the
/// "which harness is this tab" question is answered by
/// [`crate::harness::HarnessId::from_command`] alone, so no module keeps a copy
/// of it. Items inside the module keep their own visibility; nothing new is
/// public.
pub(crate) mod config;
pub mod registry;

/// NC-2: Claude tabs + their launch directories, for the `/permission/event`
/// route's cwd fallback when a hook payload can't be matched by session id.
pub(crate) use config::claude_tab_dirs;
/// #48 (F-3): one AI tab's launch directory, for the project root V32
/// containment rows are recorded under — see [`config::ai_tab_dir`].
pub(crate) use config::ai_tab_dir;
/// V33 (C5, finding F-4): which consumer a configured AI tab belongs to, for
/// `loopback::is_configured_tab`'s `(consumer, tab)` check — see
/// [`config::tab_consumer`] for why both ends must classify through one call.
pub(crate) use config::tab_consumer;
/// The advertised-MCP-server signature, re-exported for the Settings save
/// path's restart-hint edge detector (`ipc::commands::settings_update`).
pub(crate) use config::spawn_inject_sig;
pub use registry::{TabMetaWire, TabRegistry, TabRegistryHandle};
