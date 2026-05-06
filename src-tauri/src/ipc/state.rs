use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::mpsc;

use crate::audio::AudioOutput;
use crate::settings::SettingsHandle;
use crate::state::{InputLengths, StateSignal};
use crate::tabs::TabRegistryHandle;
use crate::tts::TtsRequest;

pub struct AppState {
    /// All tabs and the active-tab pointer live here. Methods on the
    /// registry are async; the lock is dropped before each await point.
    pub tabs: TabRegistryHandle,
    pub launch: LaunchContext,
    /// Channel into the TTS worker. Each per-tab processing layer clones the
    /// sender; the worker filters by active tab on the receive side.
    pub tts_segments: mpsc::Sender<TtsRequest>,
    /// Tag contents the user typed/pasted. Shared across tabs because the
    /// processing layer's filter is content-based (not timing- or tab-
    /// scoped). Keeping a single set is correct: a `[[TTS]]` the user typed
    /// in either tab shouldn't be re-spoken if it echoes back.
    pub user_typed_tts: Arc<Mutex<HashSet<String>>>,
    pub state_signals: mpsc::Sender<StateSignal>,
    /// Per-tab unsent-input length counters. Each tab's counter is read by
    /// the state manager on every tick to decide whether to drop that tab's
    /// Listening → Idle. The map grows/shrinks at runtime — IPC handlers
    /// must clone the counter Arc out before relying on it across awaits.
    pub input_lengths: InputLengths,
    pub settings: SettingsHandle,
    pub audio: Arc<RwLock<Option<Arc<AudioOutput>>>>,
}

#[derive(Clone)]
pub struct LaunchContext {
    pub cwd: PathBuf,
    pub extra_args: Vec<String>,
}
