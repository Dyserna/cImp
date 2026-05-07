mod manager;
pub mod scrollback;
mod tasks;

pub use manager::{resolve_command, PtyLaunchSpec, PtyManager};
