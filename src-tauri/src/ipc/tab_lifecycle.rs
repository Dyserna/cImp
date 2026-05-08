//! Tab-lifecycle IPC commands for V3: create / close / rename / reconfigure
//! user-managed Shell tabs. Each command returns a `TabLifecycleError`
//! with a serde-tagged shape so the frontend dialog can render inline
//! field errors keyed off the variant.
//!
//! V3-M3: every command persists its mutation to settings via
//! `SettingsHandle::set`, which broadcasts the new state to all listeners
//! and triggers a debounced disk write. Settings is the single source of
//! truth for tab identity, name, and spawn config — there is no per-tab
//! side table any more.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, State};
use tracing::{info, warn};
use uuid::Uuid;

use crate::ipc::AppState;
use crate::settings::{
    default_claude_local_tab, default_claude_tab, ClaudeTabsEnabled, ShellNotificationConfig,
    ShellTabConfig as ShellTabSettings, TabConfig, CLAUDE_LOCAL_TAB_ID, CLAUDE_TAB_ID,
};
use crate::shell::detect;
use crate::state::{StateSignal, TabId, TabKind, TabMeta};

/// Wire-format error for the tab-lifecycle commands. Internally tagged so
/// each variant becomes `{ "kind": "...", ...fields }` on the JSON side.
/// The frontend's dialog matches on `kind` to render inline errors next
/// to the offending field.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TabLifecycleError {
    /// Name field was empty (or whitespace-only). Validation happens on
    /// the backend after `trim()`; the dialog re-trims locally for live
    /// feedback but the canonical check is here.
    EmptyName,
    /// `command` could not be resolved. `tried` is whatever the user
    /// typed (relative path, absolute path, or bare name resolved via
    /// PATH).
    CommandNotFound { tried: String },
    /// `cwd` was provided but the directory does not exist.
    CwdNotFound { path: String },
    /// The target tab id does not exist in the registry.
    TabNotFound { tab: String },
    /// Attempt to close an AI builtin (Claude / Claude-local).
    BuiltinNotClosable,
    /// `reconfigure_shell_tab` was called on a non-Shell tab.
    WrongKind,
    /// Subprocess spawn failed after validation. Surfaced verbatim so the
    /// dialog can show the OS-level reason (PTY open failed, kernel said
    /// no, etc.). Reserved for future use — the frontend's Terminal
    /// component currently surfaces spawn failures via its own error
    /// overlay, so `create_shell_tab` returns Ok and lets that path fire.
    #[allow(dead_code)]
    SpawnFailed { message: String },
    /// Internal error (lock poisoning, channel send failure, etc.). Not
    /// expected in practice — surfaces as a toast on the frontend.
    Internal { message: String },
}

impl TabLifecycleError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal { message: msg.into() }
    }
}

/// Validated input for `create_shell_tab` / `reconfigure_shell_tab`.
/// Frontend dialog sends raw strings; backend resolves command, splits
/// args via `shlex`, and only then constructs this struct.
struct ValidatedShellInput {
    name: String,
    command: PathBuf,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: HashMap<String, String>,
}

fn validate_inputs(
    name: String,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
) -> Result<ValidatedShellInput, TabLifecycleError> {
    let trimmed_name = name.trim().to_string();
    if trimmed_name.is_empty() {
        return Err(TabLifecycleError::EmptyName);
    }

    // Resolve the command. If it contains a path separator we treat it as
    // an absolute or relative path; otherwise we resolve via PATH using
    // `which`. The `which` crate handles symlinks correctly on both
    // platforms.
    let raw = command.clone();
    let resolved = if command.contains(['/', '\\']) {
        let path = PathBuf::from(&command);
        if !path.is_file() {
            return Err(TabLifecycleError::CommandNotFound { tried: raw });
        }
        path
    } else {
        which::which(&command)
            .map_err(|_| TabLifecycleError::CommandNotFound { tried: raw.clone() })?
    };

    let cwd_path = match cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let p = PathBuf::from(s);
            if !p.is_dir() {
                return Err(TabLifecycleError::CwdNotFound { path: s.to_string() });
            }
            Some(p)
        }
        None => None,
    };

    Ok(ValidatedShellInput {
        name: trimmed_name,
        command: resolved,
        args,
        cwd: cwd_path,
        env,
    })
}

fn validated_to_shell_config(
    id: String,
    builtin: bool,
    input: &ValidatedShellInput,
    notifications: ShellNotificationConfig,
) -> ShellTabSettings {
    ShellTabSettings {
        id,
        builtin,
        name: input.name.clone(),
        command: input.command.to_string_lossy().into_owned(),
        args: input.args.clone(),
        cwd: input.cwd.clone(),
        env: input.env.clone(),
        notifications,
        theme_override: None,
        background_override: None,
    }
}

/// Build a `ShellNotificationConfig` from the dialog's two strings. An empty
/// string is intentional disable-this-notification (the manager treats empty
/// as "skip"); the dialog is responsible for pre-filling the defaults so the
/// user has to actively clear a field to disable it.
fn notifications_from_dialog(error: String, exited: String) -> ShellNotificationConfig {
    ShellNotificationConfig { error, exited }
}

/// Create a new user-managed Shell tab. Validates the inputs, registers it
/// in the registry, appends a `TabConfig::Shell` to settings, and emits
/// `TabAdded` so the frontend mirrors the addition into its tabs store.
#[tauri::command]
pub async fn create_shell_tab(
    app: AppHandle,
    state: State<'_, AppState>,
    name: String,
    command: String,
    args_string: String,
    cwd: Option<String>,
    env: HashMap<String, String>,
    notifications_error: String,
    notifications_exited: String,
) -> Result<TabId, TabLifecycleError> {
    let args = shlex::split(&args_string).unwrap_or_default();
    let validated = validate_inputs(name, command, args, cwd, env)?;
    let notifications = notifications_from_dialog(notifications_error, notifications_exited);

    let tab = TabId::Shell(format!("shell-{}", Uuid::new_v4()));
    let tab_meta = TabMeta {
        id: tab.clone(),
        kind: TabKind::Shell,
        name: validated.name.clone(),
    };

    // Persist to settings BEFORE registering with the registry. This way,
    // when the registry's start_tab path runs (post TabAdded → frontend
    // mount → pty_start), `build_launch_spec` can find the entry. The
    // broadcast triggered by `set` is also what the frontend's settings
    // store consumes to reflect the new entry in the Tabs section.
    {
        let mut snap = state.settings.current();
        let entry = TabConfig::Shell(validated_to_shell_config(
            tab.as_str().to_string(),
            false,
            &validated,
            notifications,
        ));
        // Idempotent on duplicate id (matches the registry's behavior).
        if let Some(existing) = snap.tabs.iter_mut().find(|t| t.id() == tab.as_str()) {
            *existing = entry;
        } else {
            snap.tabs.push(entry);
        }
        state.settings.set(snap);
    }

    let position = {
        let mut registry = state.tabs.lock().await;
        registry.insert_user_shell_tab(tab.clone(), validated.name.clone())
    };

    if let Err(e) = state
        .state_signals
        .send(StateSignal::TabAdded {
            meta: tab_meta,
            position,
        })
        .await
    {
        warn!(error = %e, "create_shell_tab: state-signal channel closed");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }

    {
        let mut registry = state.tabs.lock().await;
        if let Err(e) = registry.activate(tab.clone()).await {
            warn!(error = %e, "create_shell_tab: activate failed");
        }
    }

    let _ = app; // reserved for future per-window emits
    info!(?tab, "shell tab created");
    Ok(tab)
}

/// Close a Shell tab (including the default `shell-default-1`). The AI
/// builtins (Claude / Claude-local) reject with `BuiltinNotClosable`. The
/// PTY is killed, the registry entry dropped, the settings entry removed,
/// and `TabRemoved` is emitted.
#[tauri::command]
pub async fn close_tab(
    state: State<'_, AppState>,
    tab: TabId,
) -> Result<(), TabLifecycleError> {
    {
        // Snapshot the entry to gate on builtin status. Settings is the
        // canonical builtin marker; the id-based heuristic is a fallback.
        let snap = state.settings.current();
        if let Some(entry) = snap.find_tab(tab.as_str()) {
            if entry.builtin() {
                return Err(TabLifecycleError::BuiltinNotClosable);
            }
        }
    }

    let prev_tab = {
        let registry = state.tabs.lock().await;
        if !registry.has_tab(&tab) {
            return Err(TabLifecycleError::TabNotFound {
                tab: tab.as_str().to_string(),
            });
        }
        registry.previous_tab(&tab)
    };

    // If we're closing the active tab, switch to its left neighbor first
    // so the frontend's active-tab indicator (and the TTS active cell)
    // points at a tab that still exists. Builtins always occupy the
    // leftmost positions, so a previous tab always exists.
    {
        let mut registry = state.tabs.lock().await;
        if registry.active() == tab {
            if let Some(target) = prev_tab {
                if let Err(e) = registry.activate(target).await {
                    warn!(error = %e, "close_tab: activate previous failed");
                }
            }
        }
    }

    let removed = {
        let mut registry = state.tabs.lock().await;
        registry.remove_tab(&tab).await
    };
    if !removed {
        return Err(TabLifecycleError::TabNotFound {
            tab: tab.as_str().to_string(),
        });
    }

    // V1.4-04 D.6: drop any persisted scrollback file for the closed
    // tab. The orphan-prune sweep at next launch would also catch it,
    // but cleaning up immediately keeps the disk-state consistent
    // with the user's mental model.
    if let Err(e) = crate::pty::scrollback::delete(&tab) {
        warn!(?tab, error = %e, "close_tab: scrollback delete failed");
    }

    // Remove the settings entry. Drop the active_tab_id pointer if it
    // referenced this tab — the frontend will set a new one on its next
    // tab-switch event.
    {
        let mut snap = state.settings.current();
        snap.tabs.retain(|t| t.id() != tab.as_str());
        if snap.session.active_tab_id.as_deref() == Some(tab.as_str()) {
            snap.session.active_tab_id = None;
        }
        state.settings.set(snap);
    }

    if let Err(e) = state
        .state_signals
        .send(StateSignal::TabRemoved { tab: tab.clone() })
        .await
    {
        warn!(error = %e, "close_tab: state-signal channel closed");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }

    info!(?tab, "tab closed");
    Ok(())
}

/// Rename any tab. Builtins are renamable too — only the display name
/// changes; the underlying command/args are unaffected.
#[tauri::command]
pub async fn rename_tab(
    state: State<'_, AppState>,
    tab: TabId,
    new_name: String,
) -> Result<(), TabLifecycleError> {
    let trimmed = new_name.trim().to_string();
    if trimmed.is_empty() {
        return Err(TabLifecycleError::EmptyName);
    }
    {
        let mut registry = state.tabs.lock().await;
        if !registry.has_tab(&tab) {
            return Err(TabLifecycleError::TabNotFound {
                tab: tab.as_str().to_string(),
            });
        }
        registry.set_name(&tab, &trimmed);
    }
    {
        let mut snap = state.settings.current();
        if let Some(entry) = snap.find_tab_mut(tab.as_str()) {
            entry.set_name(trimmed.clone());
        }
        state.settings.set(snap);
    }
    if let Err(e) = state
        .state_signals
        .send(StateSignal::TabRenameRequested {
            tab: tab.clone(),
            name: trimmed,
        })
        .await
    {
        warn!(error = %e, "rename_tab: state-signal channel closed");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }
    Ok(())
}

/// Update a Shell tab's spawn config. Does NOT respawn — the new config
/// takes effect on next restart (manual via the closed-overlay Enter
/// affordance, or automatic on subprocess exit).
#[tauri::command]
pub async fn reconfigure_shell_tab(
    state: State<'_, AppState>,
    tab: TabId,
    name: String,
    command: String,
    args_string: String,
    cwd: Option<String>,
    env: HashMap<String, String>,
    notifications_error: String,
    notifications_exited: String,
    theme_override: Option<crate::settings::TerminalThemeSettings>,
    background_override: Option<crate::settings::BackgroundOverride>,
) -> Result<(), TabLifecycleError> {
    if !matches!(tab.kind(), TabKind::Shell) {
        return Err(TabLifecycleError::WrongKind);
    }
    let args = shlex::split(&args_string).unwrap_or_default();
    let validated = validate_inputs(name, command, args, cwd, env)?;
    let notifications = notifications_from_dialog(notifications_error, notifications_exited);

    let name_changed: bool = {
        let registry = state.tabs.lock().await;
        if !registry.has_tab(&tab) {
            return Err(TabLifecycleError::TabNotFound {
                tab: tab.as_str().to_string(),
            });
        }
        registry
            .name_of(&tab)
            .as_deref()
            .map(|n| n != validated.name.as_str())
            .unwrap_or(true)
    };

    {
        let mut snap = state.settings.current();
        let Some(entry) = snap.find_tab_mut(tab.as_str()) else {
            return Err(TabLifecycleError::TabNotFound {
                tab: tab.as_str().to_string(),
            });
        };
        let TabConfig::Shell(cfg) = entry else {
            return Err(TabLifecycleError::WrongKind);
        };
        cfg.name = validated.name.clone();
        cfg.command = validated.command.to_string_lossy().into_owned();
        cfg.args = validated.args.clone();
        cfg.cwd = validated.cwd.clone();
        cfg.env = validated.env.clone();
        cfg.notifications = notifications;
        cfg.theme_override = theme_override;
        cfg.background_override = background_override;
        // builtin/id stay as they were.
        state.settings.set(snap);
    }

    if name_changed {
        {
            let mut registry = state.tabs.lock().await;
            registry.set_name(&tab, &validated.name);
        }
        if let Err(e) = state
            .state_signals
            .send(StateSignal::TabRenameRequested {
                tab: tab.clone(),
                name: validated.name,
            })
            .await
        {
            warn!(error = %e, "reconfigure_shell_tab: state-signal channel closed");
            return Err(TabLifecycleError::internal("state signal channel closed"));
        }
    }
    Ok(())
}

/// Query the platform default shell spec. Frontend's New Shell Tab
/// dialog calls this to populate the command + args defaults plus the
/// platform-default notification text (used to pre-fill the new fields
/// added in M4).
#[tauri::command]
pub fn default_shell_spec() -> DefaultShellWire {
    let (spec, _source) = detect::default_shell_resolution();
    let notif_defaults = ShellNotificationConfig::default();
    DefaultShellWire {
        command: spec.command.to_string_lossy().into_owned(),
        args: spec.args.join(" "),
        git_bash_found: detect::was_default_git_bash_found(),
        notifications_error: notif_defaults.error,
        notifications_exited: notif_defaults.exited,
    }
}

/// Wire-format tuple returned by `default_shell_spec`. Args are
/// pre-joined with spaces so the dialog can drop them straight into the
/// args text input; the dialog re-splits via `shlex` on submit (server-
/// side, in `validate_inputs`).
#[derive(Debug, Serialize)]
pub struct DefaultShellWire {
    pub command: String,
    pub args: String,
    pub git_bash_found: bool,
    pub notifications_error: String,
    pub notifications_exited: String,
}

/// Wire-format snapshot of a Shell tab's current spawn config. Returned
/// by `get_shell_tab_config` so the Configure dialog can pre-fill from
/// the live settings state.
#[derive(Debug, Serialize)]
pub struct ShellTabConfigWire {
    pub name: String,
    pub command: String,
    pub args: String,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    pub notifications_error: String,
    pub notifications_exited: String,
}

/// Look up the current Shell-tab config from settings. Returns `WrongKind`
/// for AI tabs and `TabNotFound` for unknown ids.
#[tauri::command]
pub async fn get_shell_tab_config(
    state: State<'_, AppState>,
    tab: TabId,
) -> Result<ShellTabConfigWire, TabLifecycleError> {
    if !matches!(tab.kind(), TabKind::Shell) {
        return Err(TabLifecycleError::WrongKind);
    }
    let snap = state.settings.current();
    let entry = snap
        .find_tab(tab.as_str())
        .ok_or_else(|| TabLifecycleError::TabNotFound {
            tab: tab.as_str().to_string(),
        })?;
    let TabConfig::Shell(cfg) = entry else {
        return Err(TabLifecycleError::WrongKind);
    };
    Ok(ShellTabConfigWire {
        name: cfg.name.clone(),
        command: cfg.command.clone(),
        args: cfg.args.join(" "),
        cwd: cfg.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        env: cfg.env.clone(),
        notifications_error: cfg.notifications.error.clone(),
        notifications_exited: cfg.notifications.exited.clone(),
    })
}

/// Apply a new `claude_tabs_enabled` value: open and close the AI builtin
/// tabs as needed so the live tabs match the selection. Bypasses
/// `BuiltinNotClosable` (the radio is the canonical way to close an AI
/// tab); regular `close_tab` calls on AI builtins still reject. Order
/// matters — we add the surviving Claude tab first (so the active-tab
/// switch has somewhere to go), then move active off any soon-to-be-
/// removed tab, then drop the now-disabled tabs (kill PTY, drop
/// scrollback, remove the settings entry, emit TabRemoved).
#[tauri::command]
pub async fn set_claude_tabs_enabled(
    state: State<'_, AppState>,
    value: ClaudeTabsEnabled,
) -> Result<(), TabLifecycleError> {
    let want_cloud = value.includes_cloud();
    let want_local = value.includes_local();

    let (had_cloud, had_local, prev_value) = {
        let snap = state.settings.current();
        (
            snap.tabs.iter().any(|t| t.id() == CLAUDE_TAB_ID),
            snap.tabs.iter().any(|t| t.id() == CLAUDE_LOCAL_TAB_ID),
            snap.claude_tabs_enabled,
        )
    };
    if prev_value == value && want_cloud == had_cloud && want_local == had_local {
        return Ok(());
    }

    // 1. Add any newly-enabled tabs first. This guarantees the active-tab
    //    switch in step 2 always has a surviving Claude tab to land on.
    if want_cloud && !had_cloud {
        add_ai_builtin_tab(&state, TabId::Claude, "Claude".to_string(), default_claude_tab()).await?;
    }
    if want_local && !had_local {
        add_ai_builtin_tab(
            &state,
            TabId::ClaudeLocal,
            "Claude (local)".to_string(),
            default_claude_local_tab(),
        )
        .await?;
    }

    // 2. If the currently-active tab is about to be removed, switch
    //    to the surviving Claude tab first so the frontend's active
    //    indicator (and the TTS active cell) doesn't briefly point at
    //    a tab that no longer exists.
    let surviving_claude: Option<TabId> = match (want_cloud, want_local) {
        (true, _) => Some(TabId::Claude),
        (_, true) => Some(TabId::ClaudeLocal),
        // Defensively: this shouldn't be reachable because the radio
        // can't yield (false, false). If it ever does, leave active as
        // the user-set tab and let the caller deal with the empty state.
        (false, false) => None,
    };
    if let Some(target) = surviving_claude.clone() {
        let active = {
            let registry = state.tabs.lock().await;
            registry.active()
        };
        let about_to_close = (active == TabId::Claude && !want_cloud)
            || (active == TabId::ClaudeLocal && !want_local);
        if about_to_close {
            let mut registry = state.tabs.lock().await;
            if let Err(e) = registry.activate(target).await {
                warn!(error = %e, "set_claude_tabs_enabled: pre-close activate failed");
            }
        }
    }

    // 3. Remove any newly-disabled tabs.
    if !want_cloud && had_cloud {
        remove_ai_builtin_tab(&state, TabId::Claude).await?;
    }
    if !want_local && had_local {
        remove_ai_builtin_tab(&state, TabId::ClaudeLocal).await?;
    }

    // 4. Persist the new setting value alongside the post-step-1/3 tabs
    //    array. We re-read the current snapshot here (rather than
    //    cloning earlier) because `add_ai_builtin_tab` /
    //    `remove_ai_builtin_tab` already pushed their own settings.set
    //    calls — we just need to flip the discriminator on top.
    {
        let mut snap = state.settings.current();
        if snap.claude_tabs_enabled != value {
            snap.claude_tabs_enabled = value;
            state.settings.set(snap);
        }
    }

    info!(?value, "claude_tabs_enabled updated");
    Ok(())
}

async fn add_ai_builtin_tab(
    state: &State<'_, AppState>,
    tab: TabId,
    name: String,
    config: TabConfig,
) -> Result<(), TabLifecycleError> {
    let tab_meta = TabMeta {
        id: tab.clone(),
        kind: TabKind::AiTool,
        name: name.clone(),
    };

    {
        let mut snap = state.settings.current();
        if let Some(existing) = snap.tabs.iter_mut().find(|t| t.id() == tab.as_str()) {
            *existing = config;
        } else {
            // Canonical position: claude at front, claude-local right after
            // claude. shell-default-1 / user shell tabs slide right.
            let pos = match tab.as_str() {
                CLAUDE_TAB_ID => 0,
                CLAUDE_LOCAL_TAB_ID => snap
                    .tabs
                    .iter()
                    .position(|t| t.id() == CLAUDE_TAB_ID)
                    .map(|p| p + 1)
                    .unwrap_or(0),
                _ => snap.tabs.len(),
            };
            let pos = pos.min(snap.tabs.len());
            snap.tabs.insert(pos, config);
        }
        state.settings.set(snap);
    }

    let position = {
        let mut registry = state.tabs.lock().await;
        registry.insert_ai_builtin_tab(tab.clone(), name)
    };

    if let Err(e) = state
        .state_signals
        .send(StateSignal::TabAdded {
            meta: tab_meta,
            position,
        })
        .await
    {
        warn!(error = %e, "set_claude_tabs_enabled: state-signal channel closed (add)");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }
    Ok(())
}

async fn remove_ai_builtin_tab(
    state: &State<'_, AppState>,
    tab: TabId,
) -> Result<(), TabLifecycleError> {
    let removed = {
        let mut registry = state.tabs.lock().await;
        registry.remove_tab(&tab).await
    };
    if !removed {
        return Ok(());
    }

    if let Err(e) = crate::pty::scrollback::delete(&tab) {
        warn!(?tab, error = %e, "set_claude_tabs_enabled: scrollback delete failed");
    }

    {
        let mut snap = state.settings.current();
        snap.tabs.retain(|t| t.id() != tab.as_str());
        if snap.session.active_tab_id.as_deref() == Some(tab.as_str()) {
            snap.session.active_tab_id = None;
        }
        state.settings.set(snap);
    }

    if let Err(e) = state
        .state_signals
        .send(StateSignal::TabRemoved { tab: tab.clone() })
        .await
    {
        warn!(error = %e, "set_claude_tabs_enabled: state-signal channel closed (remove)");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }
    info!(?tab, "ai builtin tab removed");
    Ok(())
}
