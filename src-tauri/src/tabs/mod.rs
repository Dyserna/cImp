//! Tab registry: owns N per-tab `PtyManager`s and routes activation events
//! through the shared audio output, TTS active-tab cell, and state-manager
//! channel. The registry is the single seam through which the IPC layer
//! interacts with multi-tab subprocess state.

mod config;
pub mod registry;

pub use registry::{ShellTabConfig, TabMetaWire, TabRegistry, TabRegistryHandle};
