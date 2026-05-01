use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::pty::PtyManager;

pub struct AppState {
    pub pty: PtyManager,
    pub launch: LaunchContext,
    /// Channel into the TTS worker. The processing layer sends extracted
    /// segments here; the worker synthesizes and pushes PCM into the audio
    /// output. Always present — sends become no-ops (with a debug log) if
    /// the worker isn't running because TTS init failed.
    pub tts_segments: mpsc::Sender<String>,
    /// Tag contents the user typed or pasted into the input box. The
    /// processing layer's scanner skips these when emitting TTS so the
    /// user's own echoed markers don't fire audio. Content-based dedup —
    /// independent of timing, so window focus / terminal queries can't
    /// disrupt it.
    pub user_typed_tts: Arc<Mutex<HashSet<String>>>,
}

#[derive(Clone)]
pub struct LaunchContext {
    pub cwd: PathBuf,
    pub extra_args: Vec<String>,
}
