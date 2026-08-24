//! The handles the headless core runs on, in one value.
//!
//! ## Why this exists (V42 Phase A2)
//!
//! A1 gave every `#[tauri::command]` a service that takes *borrowed handles*,
//! constructed by a one-line `fn x_service(&AppState)` at the wire boundary.
//! That works because a command has an `AppState` in hand: Tauri injected it.
//!
//! The code A2 is about has no such caller. The delegation engine, the offload
//! supervisor and service, and the loopback's route handlers all run on their
//! own tasks, and every one of them reached for
//! `AppHandle::state::<AppState>()` — a service locator — to get back the same
//! bag of handles a command is simply handed. The lookup could fail, so each
//! site invented a fallback for "the tab layer is not running"; none of those
//! fallbacks was reachable in the shipped app, and all of them stood between
//! the code and a test.
//!
//! [`CoreHost`] is that bag, passed instead of looked up. It is `AppState`
//! minus the seven fields only the *wire* uses (the STT handle, the audio
//! slot, the system-stats sampler, the Settings-window deep-link cell, the two
//! TTS suppression flags and the lifecycle serializer) — which is to say, it is
//! the part of the app's state that has nothing to do with there being a
//! window. Every field is an ordinary in-process handle, so a test builds one
//! on the stack.
//!
//! ## It is not a container
//!
//! Nothing looks anything up here. `CoreHost` has no `get::<T>()`, and adding
//! one would rebuild the locator this phase removed. What a consumer needs, it
//! names — and if a consumer needs something that is not in this list, the
//! answer is a parameter on that consumer, not a field here.
//!
//! The two *services* the core reaches (the code graph, the workbench) are
//! deliberately absent for the same reason: they are wired later than the
//! things that use them, they are optional, and folding them in would make
//! this struct the thing it is replacing. They travel as their own
//! constructor arguments — see [`crate::graph::GraphService::new`].

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::service::pty::PtyService;
use crate::service::sink::EventSink;
use crate::settings::SettingsHandle;
use crate::state::{InputLengths, ReadOnlyTabs, StateSignal, TabActivity};
use crate::tabs::TabRegistryHandle;
use crate::tts::TtsRequest;

/// Everything the core needs from the running app that is not a window.
///
/// Cheap to clone (every field is an `Arc`, a channel sender or a `PathBuf`),
/// so a long-lived service holds its own rather than borrowing.
#[derive(Clone)]
pub struct CoreHost {
    /// Where the core's named events go. See [`EventSink`].
    pub events: Arc<dyn EventSink>,
    /// The live settings store.
    pub settings: SettingsHandle,
    /// All tabs and the active-tab pointer.
    pub tabs: TabRegistryHandle,
    /// V39 Phase A: which tabs refuse the user's keyboard, and why.
    pub read_only: ReadOnlyTabs,
    /// The readable mirror of the per-tab prompt / output-burst / exit flags.
    pub tab_activity: TabActivity,
    /// Per-tab unsent-input length counters.
    pub input_lengths: InputLengths,
    /// Into the TTS worker.
    pub tts_segments: mpsc::Sender<TtsRequest>,
    /// Into the state manager.
    pub state_signals: mpsc::Sender<StateSignal>,
    /// The directory the app was launched in.
    pub launch_cwd: PathBuf,
    /// The extra arguments every spawned harness inherits (`AppState`'s
    /// `launch.extra_args`). `Arc` because a `CoreHost` is cloned per consumer
    /// and this is read-only after launch.
    pub invocation_args: Arc<Vec<String>>,
}

impl CoreHost {
    /// The input pipeline, over these handles.
    ///
    /// The delegation engine's door into `pty_write`'s pipeline: it holds the
    /// `Driven` read-only lock itself, so it enters at
    /// [`PtyService::write_through`] rather than at [`PtyService::write`],
    /// which would refuse its own write. Until A2 that door was a one-line
    /// adapter in `ipc::commands` taking `&AppState`, because the engine had
    /// no handles of its own to build a service from.
    pub fn pty(&self) -> PtyService<'_> {
        PtyService::new(
            &self.tabs,
            &self.settings,
            &self.tab_activity,
            &self.tts_segments,
            self.launch_cwd.as_path(),
            self.invocation_args.as_slice(),
            &self.read_only,
            &self.input_lengths,
            &self.state_signals,
        )
    }
}

/// A [`CoreHost`] over throwaway handles, for tests that drive the core
/// without a WebView. Test-only for [`crate::service::sink::testing`]'s reason.
#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::service::sink::testing::RecordingEventSink;
    use crate::state::{StateSignal, TabId};

    /// The host plus the ends a test has to keep alive.
    ///
    /// The two receivers are returned rather than dropped on purpose: an
    /// `mpsc::Sender` whose receiver is gone fails every `send`, so a fixture
    /// that dropped them would quietly turn "the core signalled the state
    /// manager" into "the core tried and could not" — and a test asserting on
    /// the signal would be asserting on a closed channel.
    // Not every fixture reads back what it was given — the offload status
    // surface, for one, emits nothing — but a fixture that dropped these would
    // be handing out closed channels. Held, therefore, and not warned about.
    #[allow(dead_code)]
    pub struct TestCore {
        pub host: CoreHost,
        /// The same sink `host.events` points at, as its concrete type, so a
        /// test can read back what was emitted.
        pub events: Arc<RecordingEventSink>,
        pub signals: mpsc::Receiver<StateSignal>,
        pub tts: mpsc::Receiver<TtsRequest>,
    }

    /// Build one over `settings` and an empty tab registry.
    pub fn core_host(settings: SettingsHandle) -> TestCore {
        let (signal_tx, signals) = mpsc::channel::<StateSignal>(64);
        let (tts_tx, tts) = mpsc::channel::<TtsRequest>(64);
        let seed = TabId::from_str("test-tab");
        let tabs: TabRegistryHandle = Arc::new(tokio::sync::Mutex::new(
            crate::tabs::TabRegistry::new(
                Vec::new(),
                seed.clone(),
                Arc::new(std::sync::RwLock::new(seed)),
                signal_tx.clone(),
                Arc::new(Vec::new()),
            ),
        ));
        let events = Arc::new(RecordingEventSink::default());
        TestCore {
            host: CoreHost {
                events: events.clone(),
                settings,
                tabs,
                read_only: ReadOnlyTabs::default(),
                tab_activity: TabActivity::default(),
                input_lengths: InputLengths::default(),
                tts_segments: tts_tx,
                state_signals: signal_tx,
                launch_cwd: std::env::temp_dir(),
                invocation_args: Arc::new(Vec::new()),
            },
            events,
            signals,
            tts,
        }
    }
}
