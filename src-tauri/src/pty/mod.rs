mod manager;
mod resolve;
pub mod scrollback;
mod tasks;

pub use manager::{PtyLaunchSpec, PtyManager};
pub use resolve::resolve_command;
