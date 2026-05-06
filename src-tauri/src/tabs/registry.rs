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
use crate::shell::ShellSpec;
use crate::state::{InputLengths, StateSignal, TabId, TabKind};
use crate::tabs::config::build_launch_spec;
use crate::tts::{ActiveTab, TtsRequest};

/// Per-Shell-tab launch configuration. M2 keeps this in-memory only — M3
/// of v3 reshapes the settings schema to persist it. Each user-created
/// Shell tab gets its own entry; the M1 launch-seed Shell-1 is seeded with
/// the auto-detected `default_shell`.
#[derive(Clone, Debug)]
pub struct ShellTabConfig {
    pub spec: ShellSpec,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
}

/// Wire-format tab metadata for the `list_tabs` IPC. Mirrors the fields
/// the frontend's `tabs` store reads on mount; runtime mutations come via
/// `tab-created`/`tab-closed`/`tab-renamed` events.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TabMetaWire {
    pub id: crate::state::TabId,
    pub kind: crate::state::TabKindWire,
    pub name: String,
    pub builtin: bool,
}

/// Owns one PtyManager per TabId plus the shared resources tabs read on
/// activation (audio output, active-tab cell, state signal channel). All
/// public methods are async because PtyManager.start/shutdown are.
pub struct TabRegistry {
    /// User-visible tab order. Mutated by `add_tab` (append) and
    /// `remove_tab`. The state-manager-side ordering mirrors this via the
    /// `position` carried on `StateSignal::TabAdded`; the registry is the
    /// source of truth.
    tab_order: Vec<TabId>,
    managers: HashMap<TabId, PtyManager>,
    /// Per-tab display name. The state manager's `TabState` carries the
    /// same name for transition-time bookkeeping; the registry mirror is
    /// what the IPC `list_tabs` command reads on frontend mount (the state
    /// manager doesn't currently expose a synchronous query). Renames
    /// update both via the state-signal flow.
    names: HashMap<TabId, String>,
    active: TabId,
    /// Per-Shell-tab spawn config. Looked up by `build_launch_spec` for
    /// any `TabId::Shell(_)`. Builtins (Claude/Aider) do not appear here.
    shell_configs: HashMap<TabId, ShellTabConfig>,
    /// Shared with the TTS worker so it can filter by active tab on every
    /// request. Updated under write-lock from `activate`.
    tts_active: ActiveTab,
    audio: Arc<RwLock<Option<Arc<AudioOutput>>>>,
    state_signals: mpsc::Sender<StateSignal>,
    /// Resolved at app startup by `shell::detect::default_shell()` and
    /// shared with every Shell-tab launch path. Cloned cheaply (Arc) into
    /// the per-tab `PtyLaunchSpec` so each spawn has the binary + args
    /// without re-running detection. M2 uses this as the default seed for
    /// new user Shell tabs (the dialog can override before submission).
    default_shell: Arc<ShellSpec>,
    /// Shared input-length counter map. Mutated by the state manager on
    /// TabAdded/TabRemoved; the registry only reads it (indirectly, via
    /// the IPC layer).
    input_lengths: InputLengths,
}

pub type TabRegistryHandle = Arc<TokioMutex<TabRegistry>>;

impl TabRegistry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        seed: Vec<crate::state::TabMeta>,
        initial_active: TabId,
        tts_active: ActiveTab,
        audio: Arc<RwLock<Option<Arc<AudioOutput>>>>,
        state_signals: mpsc::Sender<StateSignal>,
        default_shell: Arc<ShellSpec>,
        input_lengths: InputLengths,
    ) -> Self {
        let mut managers = HashMap::new();
        let mut shell_configs = HashMap::new();
        let mut names = HashMap::new();
        let mut tab_order = Vec::with_capacity(seed.len());
        for meta in &seed {
            tab_order.push(meta.id.clone());
            names.insert(meta.id.clone(), meta.name.clone());
            managers.insert(meta.id.clone(), PtyManager::new());
            // Seed every Shell tab in the launch list with the auto-
            // detected default config. The M1 launch seed has exactly one
            // Shell tab (`shell-1`); future seeds (M3 settings load) will
            // carry per-tab configs and override this.
            if matches!(meta.kind, TabKind::Shell) {
                shell_configs.insert(
                    meta.id.clone(),
                    ShellTabConfig {
                        spec: (*default_shell).clone(),
                        cwd: None,
                        env: HashMap::new(),
                    },
                );
            }
        }
        Self {
            tab_order,
            managers,
            names,
            active: initial_active,
            shell_configs,
            tts_active,
            audio,
            state_signals,
            default_shell,
            input_lengths,
        }
    }

    /// Snapshot of the current tab order. Used by the IPC `list_tabs`
    /// command so the frontend can build its tabs store deterministically
    /// at mount time, independent of any in-flight events.
    pub fn list(&self) -> Vec<TabMetaWire> {
        self.tab_order
            .iter()
            .map(|id| TabMetaWire {
                id: id.clone(),
                kind: (&id.kind()).into(),
                name: self
                    .names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.as_str().to_string()),
                builtin: matches!(id, TabId::Claude | TabId::Aider),
            })
            .collect()
    }

    /// Snapshot of the live tab order. Used by IPC commands that need to
    /// echo the order back to the frontend (e.g. `list_tabs`).
    pub fn tab_order(&self) -> Vec<TabId> {
        self.tab_order.clone()
    }

    /// Per-tab Shell config lookup. Returns `None` for AI tabs and for
    /// unknown ids. Call sites that need the config to spawn a process
    /// must hold the TabRegistry lock for the lookup duration.
    pub fn shell_config(&self, tab: &TabId) -> Option<&ShellTabConfig> {
        self.shell_configs.get(tab)
    }

    pub fn default_shell(&self) -> Arc<ShellSpec> {
        self.default_shell.clone()
    }

    pub fn active(&self) -> TabId {
        self.active.clone()
    }

    pub fn has_tab(&self, tab: &TabId) -> bool {
        self.managers.contains_key(tab)
    }

    pub fn name_of(&self, tab: &TabId) -> Option<String> {
        self.names.get(tab).cloned()
    }

    /// Tab immediately preceding `tab` in the live order, or `None` if
    /// `tab` is the first entry. Used by `close_tab` to choose the next
    /// active tab when the closed one was active.
    pub fn previous_tab(&self, tab: &TabId) -> Option<TabId> {
        let idx = self.tab_order.iter().position(|t| t == tab)?;
        if idx == 0 {
            None
        } else {
            self.tab_order.get(idx - 1).cloned()
        }
    }

    /// Update the display name. Idempotent; does nothing if the tab is
    /// unknown or the name is the same.
    pub fn set_name(&mut self, tab: &TabId, name: &str) {
        if !self.names.contains_key(tab) {
            return;
        }
        self.names.insert(tab.clone(), name.to_string());
    }

    /// Replace a Shell tab's spawn config. Only valid for Shell-kind ids;
    /// callers (the IPC handler) gate this with a kind check.
    pub fn replace_shell_config(&mut self, tab: &TabId, config: ShellTabConfig) {
        if !matches!(tab.kind(), TabKind::Shell) {
            return;
        }
        self.shell_configs.insert(tab.clone(), config);
    }

    /// Append a new user-managed Shell tab to the registry. Returns the
    /// resulting position in `tab_order`. Idempotent on duplicate ids: the
    /// existing entry's name + config are overwritten and the original
    /// position is returned.
    pub fn insert_user_shell_tab(
        &mut self,
        tab: TabId,
        name: String,
        config: ShellTabConfig,
    ) -> usize {
        if let Some(idx) = self.tab_order.iter().position(|t| t == &tab) {
            self.names.insert(tab.clone(), name);
            self.shell_configs.insert(tab, config);
            return idx;
        }
        let position = self.tab_order.len();
        self.tab_order.push(tab.clone());
        self.managers.insert(tab.clone(), PtyManager::new());
        self.names.insert(tab.clone(), name);
        self.shell_configs.insert(tab, config);
        position
    }

    /// Remove a tab. Builtins are silently ignored — callers (IPC) gate
    /// with `BuiltinNotClosable` first. Returns `true` if the tab was
    /// found and removed.
    pub async fn remove_user_shell_tab(&mut self, tab: &TabId) -> bool {
        if matches!(tab, TabId::Claude | TabId::Aider) {
            return false;
        }
        let Some(idx) = self.tab_order.iter().position(|t| t == tab) else {
            return false;
        };
        self.tab_order.remove(idx);
        if let Some(manager) = self.managers.remove(tab) {
            if let Err(e) = manager.shutdown().await {
                warn!(?tab, error = %e, "remove_user_shell_tab: shutdown failed");
            }
        }
        self.shell_configs.remove(tab);
        self.names.remove(tab);
        true
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
        // resolve_command (inside build_launch_spec) is the most common
        // failure surface — "aider not on PATH". Emit SubprocessExited so
        // the state machine pins the tab to Error and the avatar reflects
        // it; the frontend also gets the raw error back from the Tauri
        // call to render its in-tab overlay.
        let spec = match build_launch_spec(
            tab.clone(),
            &snap,
            self.shell_configs.get(&tab),
            launch_cwd,
            invocation_args,
        ) {
            Ok(s) => s,
            Err(e) => {
                let _ = self
                    .state_signals
                    .try_send(StateSignal::SubprocessExited { tab, code: None });
                return Err(e);
            }
        };
        let result = manager
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
            .await;
        // On a successful Shell spawn, broadcast `ShellRestarted` so the
        // state manager clears the closed flag (no-op when the tab was
        // never closed — covers both first-mount and restart paths).
        if result.is_ok() && matches!(tab.kind(), TabKind::Shell) {
            let _ = self
                .state_signals
                .try_send(StateSignal::ShellRestarted { tab });
        }
        result
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
        let spec = match build_launch_spec(
            tab.clone(),
            &snap,
            self.shell_configs.get(&tab),
            launch_cwd,
            invocation_args,
        ) {
            Ok(s) => s,
            Err(e) => {
                let _ = self
                    .state_signals
                    .try_send(StateSignal::SubprocessExited { tab, code: None });
                return Err(e);
            }
        };
        let result = manager
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
            .await;
        if result.is_ok() && matches!(tab.kind(), TabKind::Shell) {
            let _ = self
                .state_signals
                .try_send(StateSignal::ShellRestarted { tab });
        }
        result
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

        let prev = self.active.clone();

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
                        .try_send(StateSignal::TtsPlaybackStopped { tab: prev.clone() });
                }
                audio.stop_all();
            }
        }

        // Step 3: flip TTS gate.
        if let Ok(mut g) = self.tts_active.write() {
            *g = tab.clone();
        }

        // Step 4: update local pointer.
        self.active = tab.clone();
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
/// tick. Seeded with the launch tab list; the state manager grows/shrinks
/// the map at runtime on TabAdded/TabRemoved signals (see `state::manager`).
pub fn make_input_lengths(seed: &[TabId]) -> InputLengths {
    let map: HashMap<TabId, Arc<AtomicI32>> = seed
        .iter()
        .cloned()
        .map(|t| (t, Arc::new(AtomicI32::new(0))))
        .collect();
    Arc::new(RwLock::new(map))
}

