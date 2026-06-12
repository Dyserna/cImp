use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use tauri::ipc::Channel;
use tauri::AppHandle;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::processing::permission::PermissionPattern;
use crate::pty::PtyManager;
use crate::settings::{AiTabId, SettingsHandle};
use crate::state::{InputLengths, StateSignal, TabId, TabKind};
use crate::tabs::config::build_launch_spec;
use crate::tts::{ActiveTab, TtsRequest};

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
///
/// V3-M3 removed the per-Shell-tab `shell_configs` side table — settings is
/// now the single source of truth for spawn config. The registry only owns
/// runtime state (PtyManagers, tab order, names) plus the routing handles.
pub struct TabRegistry {
    /// User-visible tab order. Mutated by `insert_user_shell_tab` (append)
    /// and `remove_user_shell_tab`. The state-manager-side ordering mirrors
    /// this via the `position` carried on `StateSignal::TabAdded`; the
    /// registry is the source of truth for runtime order.
    tab_order: Vec<TabId>,
    managers: HashMap<TabId, PtyManager>,
    /// Per-tab display name. Renames mutate this; the IPC `list_tabs`
    /// command reads it on frontend mount.
    names: HashMap<TabId, String>,
    active: TabId,
    /// Shared with the TTS worker so it can filter by active tab on every
    /// request. Updated under write-lock from `activate`.
    tts_active: ActiveTab,
    state_signals: mpsc::Sender<StateSignal>,
    /// Shared input-length counter map. Mutated by the state manager on
    /// TabAdded/TabRemoved; the registry only reads it (indirectly, via
    /// the IPC layer).
    input_lengths: InputLengths,
    /// Detection patterns loaded from `<exe-dir>/patterns.json` at
    /// startup. Cloned per-tab into the processor task on PTY start.
    /// Wrapped in `Arc` so the per-tab clone is cheap (the inner Vec
    /// itself is only cloned by the processor when constructing its
    /// detector).
    patterns: Arc<Vec<PermissionPattern>>,
}

pub type TabRegistryHandle = Arc<TokioMutex<TabRegistry>>;

/// True when `id` is a reserved builtin that cannot be closed: the four
/// AI builtins, plus the `shell-broot` utility tab (V15) which is gated by
/// the Settings → Tabs enable toggle rather than the close `×`. The
/// reserved `shell-default-1` id is *not* a builtin: it ships as the
/// default first shell tab on fresh installs but is closable just like
/// any user-created shell. User-created Shell tabs use uuid-based ids
/// that never collide with these.
fn is_builtin_id(id: &str) -> bool {
    AiTabId::from_id(id).is_some() || id == crate::settings::SHELL_BROOT_TAB_ID
}

/// V1.4-04 D: replicate the filename-sanitization done by
/// `pty::scrollback::scrollback_file_for`. Has to be in sync — the
/// orphan prune compares filenames against this set.
fn sanitize_tab_id_for_filename(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl TabRegistry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        seed: Vec<crate::state::TabMeta>,
        initial_active: TabId,
        tts_active: ActiveTab,
        state_signals: mpsc::Sender<StateSignal>,
        input_lengths: InputLengths,
        patterns: Arc<Vec<PermissionPattern>>,
    ) -> Self {
        let mut managers = HashMap::new();
        let mut names = HashMap::new();
        let mut tab_order = Vec::with_capacity(seed.len());
        for meta in &seed {
            tab_order.push(meta.id.clone());
            names.insert(meta.id.clone(), meta.name.clone());
            managers.insert(meta.id.clone(), PtyManager::new());
        }
        Self {
            tab_order,
            managers,
            names,
            active: initial_active,
            tts_active,
            state_signals,
            input_lengths,
            patterns,
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
                builtin: is_builtin_id(id.as_str()),
            })
            .collect()
    }

    /// Snapshot of the live tab order. Used by IPC commands that need to
    /// echo the order back to the frontend (e.g. `list_tabs`).
    pub fn tab_order(&self) -> Vec<TabId> {
        self.tab_order.clone()
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

    /// Append a new user-managed tab (a Shell tab, or a `+`-spawned AI
    /// duplicate) to the registry. Returns the resulting position in
    /// `tab_order`. Idempotent on duplicate ids: the existing entry's name
    /// is overwritten and the original position is returned. Unlike
    /// `insert_ai_builtin_tab`, this appends at the end rather than at a
    /// canonical slot — the frontend layout decides the visible position.
    ///
    /// V3-M3: the spawn config now lives in settings — callers persist the
    /// `TabConfig` entry separately via `SettingsHandle::set`.
    pub fn insert_user_tab(&mut self, tab: TabId, name: String) -> usize {
        if let Some(idx) = self.tab_order.iter().position(|t| t == &tab) {
            self.names.insert(tab, name);
            return idx;
        }
        let position = self.tab_order.len();
        self.tab_order.push(tab.clone());
        self.managers.insert(tab.clone(), PtyManager::new());
        self.names.insert(tab, name);
        position
    }

    /// Remove a tab. Callers (IPC) are responsible for any policy gate —
    /// `close_tab` rejects AI builtins with `BuiltinNotClosable`, but the
    /// `set_enabled_ai_tabs` path bypasses that check because the
    /// checkbox group is the canonical way to close AI tabs. Returns
    /// `true` if the tab was found and removed.
    pub async fn remove_tab(&mut self, tab: &TabId) -> bool {
        let Some(idx) = self.tab_order.iter().position(|t| t == tab) else {
            return false;
        };
        self.tab_order.remove(idx);
        if let Some(manager) = self.managers.remove(tab) {
            if let Err(e) = manager.shutdown().await {
                warn!(?tab, error = %e, "remove_tab: shutdown failed");
            }
        }
        self.names.remove(tab);
        true
    }

    /// Re-insert an AI builtin tab at its canonical position
    /// (claude → 0, claude-local → after claude, aider → after
    /// claude-local, aider-local → after aider). Used by
    /// `set_enabled_ai_tabs` when the user re-enables a previously-
    /// removed tab. Idempotent; returns the resulting position.
    /// Distinct from `insert_user_shell_tab` because AI builtins land
    /// at fixed positions rather than at the end of the list.
    pub fn insert_ai_builtin_tab(&mut self, tab: TabId, name: String) -> usize {
        if let Some(idx) = self.tab_order.iter().position(|t| t == &tab) {
            self.names.insert(tab, name);
            return idx;
        }
        let position = match AiTabId::from_id(tab.as_str()) {
            Some(target) => {
                let target_order = target.canonical_order();
                let mut pos = 0usize;
                for (idx, existing) in self.tab_order.iter().enumerate() {
                    match AiTabId::from_id(existing.as_str()) {
                        Some(other) if other.canonical_order() < target_order => {
                            pos = idx + 1;
                        }
                        _ => break,
                    }
                }
                pos
            }
            // Non-reserved id (shouldn't happen in this code path) —
            // fall through to "append".
            None => self.tab_order.len(),
        };
        let position = position.min(self.tab_order.len());
        self.tab_order.insert(position, tab.clone());
        self.managers.insert(tab.clone(), PtyManager::new());
        self.names.insert(tab, name);
        position
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
        // build_launch_spec resolves the command via PATH; "binary not on
        // PATH" is the most common failure surface. Shell tabs route this
        // to a dedicated `ShellLaunchFailed` signal that pins the tab to
        // the closed sub-state with a "command not found" message — Enter
        // on that overlay opens the Configure dialog rather than retrying
        // the spawn (which would just fail again). AI tabs and other
        // failure types fall back to the generic SubprocessExited path.
        let spec = match build_launch_spec(tab.clone(), &snap, launch_cwd, invocation_args) {
            Ok(s) => s,
            Err(e) => {
                emit_launch_failure(&self.state_signals, &tab, &e);
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
                self.patterns.clone(),
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
        let spec = match build_launch_spec(tab.clone(), &snap, launch_cwd, invocation_args) {
            Ok(s) => s,
            Err(e) => {
                emit_launch_failure(&self.state_signals, &tab, &e);
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
                self.patterns.clone(),
            )
            .await;
        if result.is_ok() && matches!(tab.kind(), TabKind::Shell) {
            let _ = self
                .state_signals
                .try_send(StateSignal::ShellRestarted { tab });
        }
        result
    }

    /// V1.4-03: rebind the PTY's output channel to a fresh
    /// `Channel<String>` without restarting the shell. Used when the
    /// JS-side xterm is destroyed and recreated for a renderer-category
    /// flip (image ↔ fast). Errors propagate from `PtyManager::rebind_channel`
    /// — `NotStarted` if the PTY never spawned, `Pty("processor task gone")`
    /// if the child has exited and the processor task already cleaned up.
    /// In either case the caller (frontend `attemptSpawn(entry, 'rebind')`)
    /// falls back to a fresh `pty_start`.
    pub async fn rebind_channel(
        &self,
        tab: TabId,
        new_channel: Channel<String>,
    ) -> AppResult<()> {
        let manager = self
            .managers
            .get(&tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.rebind_channel(new_channel).await
    }

    pub async fn write(&self, tab: TabId, bytes: Vec<u8>) -> AppResult<()> {
        let manager = self
            .managers
            .get(&tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.write_input(bytes).await
    }

    /// V1.4-04 D: snapshot a tab's scrollback ring as raw bytes. Used
    /// by graceful-exit persistence (one snapshot per tab) and the
    /// `pty_get_scrollback` Tauri command (diagnostic).
    pub async fn scrollback_snapshot(&self, tab: TabId) -> AppResult<Vec<u8>> {
        let manager = self
            .managers
            .get(&tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.scrollback_snapshot().await
    }

    /// V1.4-04 D: seed a tab's scrollback ring with bytes restored
    /// from the previous session. Called by `pty_start` after the new
    /// PTY has spawned successfully.
    pub async fn seed_scrollback(&self, tab: &TabId, bytes: &[u8]) -> AppResult<()> {
        let manager = self
            .managers
            .get(tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.seed_scrollback(bytes).await
    }

    /// V1.4-04 D: drop a tab's scrollback ring. Called on
    /// user-initiated `pty_restart` because the user explicitly asked
    /// for a clean shell.
    pub async fn clear_scrollback(&self, tab: &TabId) -> AppResult<()> {
        let manager = self
            .managers
            .get(tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.clear_scrollback().await
    }

    /// V1.4-04 D: snapshot the set of known tab IDs (sanitized to the
    /// scrollback-file form). Used by the orphan-prune sweep at app
    /// startup so files for tabs deleted between sessions get cleaned
    /// up.
    pub fn known_scrollback_ids(&self) -> HashSet<String> {
        self.tab_order
            .iter()
            .map(|t| sanitize_tab_id_for_filename(t.as_str()))
            .collect()
    }

    /// V1.4-04 D.4: snapshot of the live tab order. Used by the
    /// graceful-exit handler in `main.rs` to walk every tab and
    /// persist its ring buffer. Returning an owned `Vec` avoids
    /// holding the registry's lock across the loop.
    pub fn tab_order_snapshot(&self) -> Vec<TabId> {
        self.tab_order.clone()
    }

    pub async fn resize(&self, tab: TabId, rows: u16, cols: u16) -> AppResult<()> {
        let manager = self
            .managers
            .get(&tab)
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;
        manager.resize(rows, cols).await
    }

    /// Switch the active tab.
    ///
    /// Switching tabs does NOT stop in-flight TTS — by design, speech is only
    /// interrupted by Esc (`tts_stop`). Audio already enqueued plays to
    /// completion regardless of which tab is in front; the audio thread
    /// remembers which tab started the current stretch of speech and tags its
    /// playing→idle edge with that tab (see `playback.rs`), so the speaking
    /// tab's avatar leaves Speaking when the audio actually finishes — not
    /// when the user happens to switch away.
    ///
    /// We still flip the TTS active-tab cell here so that *future*
    /// processing-layer sends for the previous tab are filtered out at the
    /// worker (the "active tab speaks" rule applies to newly-produced
    /// segments, not to audio that is already playing).
    pub async fn activate(&mut self, tab: TabId) -> AppResult<()> {
        if !self.managers.contains_key(&tab) {
            return Err(AppError::Pty(format!("unknown tab {tab:?}")));
        }
        if tab == self.active {
            return Ok(());
        }

        let prev = self.active.clone();

        // Flip the TTS active-tab cell. A new-tab processing-layer send in
        // flight is correctly accepted; an old-tab send in flight is correctly
        // dropped (it doesn't match the new active). Audio already in the sink
        // is unaffected and keeps playing.
        if let Ok(mut g) = self.tts_active.write() {
            *g = tab.clone();
        }

        self.active = tab.clone();
        info!(?prev, ?tab, "tab activated");

        // Tell the state manager so it can broadcast ActiveTabChanged.
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

/// Translate a launch-time `AppError` into the right state-machine signal.
/// Shell tabs with a `CommandNotFound` get a dedicated `ShellLaunchFailed`
/// signal so the closed overlay can show a "command not found" message and
/// route Enter to Configure instead of restart. Everything else (AI tabs,
/// other Shell failure modes) goes through the generic SubprocessExited
/// path that turns into Error / a regular closed overlay.
fn emit_launch_failure(state_signals: &mpsc::Sender<StateSignal>, tab: &TabId, err: &AppError) {
    if matches!(tab.kind(), TabKind::Shell) {
        if let AppError::CommandNotFound(name) = err {
            let message = format!(
                "Shell command not found: {name}. Reconfigure or close this tab."
            );
            let _ = state_signals.try_send(StateSignal::ShellLaunchFailed {
                tab: tab.clone(),
                message,
            });
            return;
        }
    }
    let _ = state_signals.try_send(StateSignal::SubprocessExited {
        tab: tab.clone(),
        code: None,
    });
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
