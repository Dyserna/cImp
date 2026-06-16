use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{mpsc, Mutex as TokioMutex};

use crate::audio::AudioOutput;
use crate::settings::SettingsHandle;
use crate::state::{InputLengths, StateSignal, TabId};
use crate::tabs::TabRegistryHandle;
use crate::tts::{AiTtsSuppressed, SpeakSession, TtsRequest};

pub struct AppState {
    /// V6-01 speech-to-text handle: posts start/stop/cancel to the capture
    /// thread. Recording state and transcripts flow back to the frontend via
    /// the `stt-state` / `stt-transcription` events, not through this handle.
    pub stt: crate::stt::SttHandle,
    /// All tabs and the active-tab pointer live here. Methods on the
    /// registry are async; the lock is dropped before each await point.
    pub tabs: TabRegistryHandle,
    pub launch: LaunchContext,
    /// Channel into the TTS worker. Each per-tab processing layer clones the
    /// sender; the worker filters by active tab on the receive side.
    pub tts_segments: mpsc::Sender<TtsRequest>,
    /// Shared "current selection-read session" id (0 = none).
    /// `tts_speak_selection` bumps it; `tts_stop` zeroes it. The TTS worker
    /// reads it between chunks so an Esc-driven stop or a superseding read
    /// abandons the remaining chunks of an in-flight read. See
    /// [`crate::tts::SpeakSession`].
    pub speak_session: SpeakSession,
    /// Set by `tts_stop` (Esc) to drop the rest of the current AI-output
    /// burst's tagged TTS; cleared by the state manager on the next
    /// `ClaudeOutputStarted`. See [`crate::tts::AiTtsSuppressed`].
    pub ai_tts_suppressed: AiTtsSuppressed,
    /// Tag contents the user typed/pasted. Shared across tabs because the
    /// processing layer's filter is content-based (not timing- or tab-
    /// scoped). Keeping a single set is correct: a `[[TTS]]` the user typed
    /// in either tab shouldn't be re-spoken if it echoes back.
    pub user_typed_tts: Arc<Mutex<HashSet<String>>>,
    /// Per-tab accumulator of the user's in-progress typed line. On Enter its
    /// sentences are folded into `user_typed_tts` so "speak all output" mode
    /// doesn't read the question back when the TUI echoes it. Keyed per tab
    /// because line editing (backspace / kill) is per input box.
    pub user_input_buf: Arc<Mutex<HashMap<TabId, String>>>,
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
    /// System-monitor sampler (CPU / memory / GPU / network) backing the
    /// bottom-bar stats panel's `get_system_stats` command. Holds the sysinfo
    /// + NVML handles; interior-locked so the shared `&AppState` can sample it.
    pub sysmon: crate::sysmon::SystemStatsState,
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
