use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{mpsc, Mutex as TokioMutex};

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
    /// V1.4-07 A: pending deep-link target for the Settings window.
    /// `open_settings_window_to_tab` writes the tab id here so the
    /// Settings window can read+clear it on mount (cold-open path)
    /// while a `settings-deep-link` event covers the hot-open path.
    pub pending_settings_deep_link: Arc<Mutex<Option<String>>>,
    /// Serializes tab-lifecycle commands (`create_shell_tab`,
    /// `close_tab`, `reconfigure_shell_tab`, `set_enabled_ai_tabs`)
    /// against each other. Without this each command would interleave
    /// its multiple read-modify-write passes against `state.settings`
    /// and the registry, producing inconsistent state — for example,
    /// `set_enabled_ai_tabs` reading the current have-set and racing a
    /// concurrent `close_tab` of the same AI tab. The lifecycle
    /// commands are user-initiated and rare; the serializer is cheap
    /// and trivially correct.
    pub lifecycle_serializer: Arc<TokioMutex<()>>,
}

#[derive(Clone)]
pub struct LaunchContext {
    pub cwd: PathBuf,
    pub extra_args: Vec<String>,
}
