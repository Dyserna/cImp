use std::path::PathBuf;

use crate::pty::PtyManager;

pub struct AppState {
    pub pty: PtyManager,
    pub launch: LaunchContext,
}

#[derive(Clone)]
pub struct LaunchContext {
    pub cwd: PathBuf,
    pub extra_args: Vec<String>,
}
