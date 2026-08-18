mod manager;
mod resolve;
/// V33 Phase B — the AppContainer ConPTY backend for sandboxed AI tabs.
/// Windows-only by construction: it *is* the Win32 dance, and the plain
/// `portable_pty` path is what every other platform uses.
#[cfg(windows)]
pub mod sandboxed_conpty;
pub mod scrollback;
mod tasks;

pub use manager::{PtyLaunchSpec, PtyManager};
pub use resolve::resolve_command;
