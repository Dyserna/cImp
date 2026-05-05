use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use tauri::ipc::Channel;
use tauri::AppHandle;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tracing::{debug, info, warn};

use crate::audio::AudioOutput;
use crate::error::{AppError, AppResult};
use crate::pty::PtyManager;
use crate::settings::SettingsHandle;
use crate::state::{StateSignal, TabId};
use crate::tabs::config::build_launch_spec;
use crate::tts::{ActiveTab, TtsRequest};

/// Owns one PtyManager per TabId plus the shared resources tabs read on
/// activation (audio output, active-tab cell, state signal channel). All
/// public methods are async because PtyManager.start/shutdown are.
pub struct TabRegistry {
    managers: HashMap<TabId, PtyManager>,
    active: TabId,
    /// Shared with the TTS worker so it can filter by active tab on every
    /// request. Updated under write-lock from `activate`.
    tts_active: ActiveTab,
    audio: Arc<RwLock<Option<Arc<AudioOutput>>>>,
    state_signals: mpsc::Sender<StateSignal>,
}

pub type TabRegistryHandle = Arc<TokioMutex<TabRegistry>>;

impl TabRegistry {
    pub fn new(
        initial_active: TabId,
        tts_active: ActiveTab,
        audio: Arc<RwLock<Option<Arc<AudioOutput>>>>,
        state_signals: mpsc::Sender<StateSignal>,
    ) -> Self {
        let mut managers = HashMap::new();
        for tab in TabId::ALL {
            managers.insert(tab, PtyManager::new());
        }
        Self {
            managers,
            active: initial_active,
            tts_active,
            audio,
            state_signals,
        }
    }

    pub fn active(&self) -> TabId {
        self.active
    }

    /// Spawn the subprocess for `tab` and bind it to `output_channel`. Each
    /// tab calls this once on first xterm mount.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_tab(
        &self,
        app: AppHandle,
        tab: TabId,
        output_channel: Channel<String>,
        rows: u16,
        cols: u16,
        launch_cwd: &std::path::Path,
        invocation_args: &[String],
        tts_segments: mpsc::Sender<TtsRequest>,
        user_typed_tts: Arc<StdMutex<HashSet<String>>>,
        settings: SettingsHandle,
    ) -> AppResult<()> {
        let manager = self
            .managers
            .get(&tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        let snap = settings.current();
        let spec = build_launch_spec(tab, &snap, launch_cwd, invocation_args)?;
        manager
            .start(
                app,
                spec,
                output_channel,
                rows,
                cols,
                tts_segments,
                user_typed_tts,
                self.state_signals.clone(),
            )
            .await
    }

    /// Tear down + bring up the subprocess for `tab`. The frontend supplies a
    /// fresh output channel because the previous one is dropped with the
    /// previous session.
    #[allow(clippy::too_many_arguments)]
    pub async fn restart_tab(
        &self,
        app: AppHandle,
        tab: TabId,
        output_channel: Channel<String>,
        rows: u16,
        cols: u16,
        launch_cwd: &std::path::Path,
        invocation_args: &[String],
        tts_segments: mpsc::Sender<TtsRequest>,
        user_typed_tts: Arc<StdMutex<HashSet<String>>>,
        settings: SettingsHandle,
    ) -> AppResult<()> {
        let manager = self
            .managers
            .get(&tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.shutdown().await?;
        let snap = settings.current();
        let spec = build_launch_spec(tab, &snap, launch_cwd, invocation_args)?;
        manager
            .start(
                app,
                spec,
                output_channel,
                rows,
                cols,
                tts_segments,
                user_typed_tts,
                self.state_signals.clone(),
            )
            .await
    }

    pub async fn write(&self, tab: TabId, bytes: Vec<u8>) -> AppResult<()> {
        let manager = self
            .managers
            .get(&tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.write_input(bytes).await
    }

    pub async fn resize(&self, tab: TabId, rows: u16, cols: u16) -> AppResult<()> {
        let manager = self
            .managers
            .get(&tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.resize(rows, cols).await
    }

    /// Switch the active tab. Order matters:
    ///   1. If the prev tab was speaking, *synchronously* emit a stop
    ///      signal tagged with the prev tab so the state machine drops it
    ///      out of Speaking. We can't rely on the audio thread to do this:
    ///      `stop_all` is fire-and-forget over an mpsc, the audio thread
    ///      processes it later, and by the time it emits its own stop
    ///      signal the active-tab cell has already flipped — so its signal
    ///      gets tagged with the NEW tab and Claude stays pinned in
    ///      Speaking forever.
    ///   2. Stop in-flight audio (rodio sink clear) so the prev tab's TTS
    ///      doesn't bleed into the next view.
    ///   3. Flip the TTS active-tab cell so any in-flight processing-layer
    ///      sends for the prev tab get filtered out at the worker.
    ///   4. Update our own `active` field.
    ///   5. Broadcast TabActivated to the state manager.
    pub async fn activate(&mut self, tab: TabId) -> AppResult<()> {
        if !self.managers.contains_key(&tab) {
            return Err(AppError::Pty(format!("unknown tab {tab:?}")));
        }
        if tab == self.active {
            return Ok(());
        }

        let prev = self.active;

        // Step 1 + 2: stop audio, with a synchronous stop signal first if
        // playback is in flight. The audio thread's own edge will later
        // emit a redundant stop tagged with the NEW tab — harmless because
        // the new tab is in a non-Speaking state, so the transition is a
        // no-op there.
        if let Ok(slot) = self.audio.read() {
            if let Some(audio) = slot.as_ref() {
                if audio.is_playing() {
                    let _ = self
                        .state_signals
                        .try_send(StateSignal::TtsPlaybackStopped { tab: prev });
                }
                audio.stop_all();
            }
        }

        // Step 3: flip TTS gate.
        if let Ok(mut g) = self.tts_active.write() {
            *g = tab;
        }

        // Step 4: update local pointer.
        self.active = tab;
        info!(?prev, ?tab, "tab activated");

        // Step 5: tell the state manager so it can broadcast ActiveTabChanged.
        let _ = self
            .state_signals
            .try_send(StateSignal::TabActivated { tab });

        Ok(())
    }

    pub async fn shutdown_all(&self) {
        for (tab, manager) in &self.managers {
            if let Err(e) = manager.shutdown().await {
                warn!(?tab, error = %e, "shutdown_all: tab teardown failed");
            }
        }
        debug!("all tabs shut down");
    }
}

/// Per-tab input-length counters for the state manager's auto-leave-Listening
/// tick. Allocated up-front since the TabId set is static in v2.
pub fn make_input_lengths() -> HashMap<TabId, Arc<AtomicI32>> {
    TabId::ALL
        .iter()
        .map(|&t| (t, Arc::new(AtomicI32::new(0))))
        .collect()
}

#[allow(dead_code)]
pub fn launch_cwd_default() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
