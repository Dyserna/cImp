use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::mpsc;

use crate::audio::AudioOutput;
use crate::pty::PtyManager;
use crate::settings::SettingsHandle;
use crate::state::StateSignal;

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
    /// Avatar state-machine input. Cloned into anything that produces
    /// signals (pty_write for keystrokes/submit, the processor for the
    /// claude-burst-end signal, the audio thread for playback, the waiter
    /// for subprocess exit).
    pub state_signals: mpsc::Sender<StateSignal>,
    /// Approximate length of the unsent input the user has typed since the
    /// last submit. Updated by `pty_write` as keystrokes flow through;
    /// the state manager reads it to decide when Listening should auto-
    /// leave (input empty + no recent typing). Approximate because we
    /// can't see Claude's input box — we infer from byte deltas (printables
    /// add one, backspace removes one, Ctrl+U/Ctrl+K reset to zero,
    /// Ctrl+W is treated as removing four). Saturating, never negative.
    pub input_length: Arc<AtomicI32>,
    /// Live-updating settings store. Cheap clone; subscribers call
    /// `subscribe()` to receive a `broadcast::Receiver<Settings>`.
    pub settings: SettingsHandle,
    /// Audio output handle. Populated from the Tauri setup hook once the
    /// audio device is up; remains `None` if init failed (TTS silent mode).
    /// `pty_write` reads this to honor the `interrupt_on_input` setting —
    /// typing during playback stops the sink when enabled.
    pub audio: Arc<RwLock<Option<Arc<AudioOutput>>>>,
}

#[derive(Clone)]
pub struct LaunchContext {
    pub cwd: PathBuf,
    pub extra_args: Vec<String>,
}
