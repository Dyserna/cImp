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

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tracing::{info, warn};
use uuid::Uuid;

use crate::ipc::AppState;
use crate::settings::{
    default_ai_tab, AiTabId, Settings, ShellNotificationConfig,
    ShellTabConfig as ShellTabSettings, TabConfig,
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
    /// `set_enabled_ai_tabs` was called with an empty list. The UI's
    /// last-checked-is-locked rule prevents this from the user side; the
    /// IPC enforces it as defense-in-depth.
    EmptyAiTabsList,
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

/// Validate the dialog inputs. Each filesystem probe (`which::which`,
/// `is_file`, `is_dir`) is synchronous and may stat slow paths (network
/// drives, antivirus-scanned directories), so the whole probe runs on a
/// blocking pool thread. The Tauri command wrapper is async; calling
/// this from there off the runtime keeps the tokio worker thread free
/// for other IPC work while a slow PATH walk is in flight.
async fn validate_inputs(
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

    let cwd_string = cwd;
    tokio::task::spawn_blocking(move || {
        // Resolve the command. If it contains a path separator we treat it
        // as an absolute or relative path; otherwise we resolve via PATH
        // using `which`.
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

        let cwd_path = match cwd_string.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
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
    })
    .await
    .map_err(|e| TabLifecycleError::internal(format!("validate_inputs join: {e}")))?
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
/// user has to actively clear a field to disable it. V1.11 promoted each
/// slot to `{ enabled, text }`; the dialog still sends bare strings, so
/// `enabled` is derived from non-emptiness here. Users wanting the
/// "disabled but text preserved" combination edit via Settings → Tabs.
fn notifications_from_dialog(error: String, exited: String) -> ShellNotificationConfig {
    ShellNotificationConfig {
        error: crate::settings::NotificationSlot {
            enabled: !error.is_empty(),
            text: error,
        },
        exited: crate::settings::NotificationSlot {
            enabled: !exited.is_empty(),
            text: exited,
        },
    }
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
    let _serializer = state.lifecycle_serializer.lock().await;
    let args = shlex::split(&args_string).unwrap_or_default();
    let validated = validate_inputs(name, command, args, cwd, env).await?;
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
        registry.insert_user_tab(tab.clone(), validated.name.clone())
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

/// Pick a unique display name for a spawned duplicate by suffixing the
/// template's name with the lowest free integer ≥ 2 (e.g. "Claude" →
/// "Claude 2", "Claude 3"). Falls back to a uuid suffix in the
/// (practically impossible) event the first thousand are all taken.
fn unique_tab_name(settings: &Settings, base: &str) -> String {
    let taken: std::collections::HashSet<&str> =
        settings.tabs.iter().map(|t| t.name()).collect();
    for n in 2..1000 {
        let candidate = format!("{base} {n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    format!("{base} {}", Uuid::new_v4())
}

/// Spawn a duplicate of an existing AI tab — the `+` affordance on a
/// Claude/Aider builtin. The new tab clones the *template's live config*
/// (command, env, tts-injection, use_local_provider, theme/background
/// overrides, …) so it behaves identically to the tab it came from,
/// including local-provider env synthesis. It gets a fresh `"ai-<uuid>"`
/// id, `builtin: false` (so it's closable and shows the `×`), and a
/// unique auto-incremented name. Persisting it to `settings.tabs` means
/// it survives a restart; the integrity check leaves non-reserved AI ids
/// untouched.
#[tauri::command]
pub async fn create_ai_tab(
    app: AppHandle,
    state: State<'_, AppState>,
    template: TabId,
) -> Result<TabId, TabLifecycleError> {
    let _serializer = state.lifecycle_serializer.lock().await;

    // Clone the template's AI config. The `+` only appears on AI tabs, so
    // a missing entry or a Shell template is a malformed request.
    let mut cfg = {
        let snap = state.settings.current();
        let entry = snap.find_tab(template.as_str()).ok_or_else(|| {
            TabLifecycleError::TabNotFound {
                tab: template.as_str().to_string(),
            }
        })?;
        match entry {
            TabConfig::AiTool(ai) => ai.clone(),
            TabConfig::Shell(_) => return Err(TabLifecycleError::WrongKind),
        }
    };

    let tab = TabId::Ai(format!("ai-{}", Uuid::new_v4()));
    let name = unique_tab_name(&state.settings.current(), &cfg.name);
    cfg.id = tab.as_str().to_string();
    cfg.builtin = false;
    cfg.name = name.clone();

    let tab_meta = TabMeta {
        id: tab.clone(),
        kind: TabKind::AiTool,
        name: name.clone(),
    };

    // Persist BEFORE registering so `build_launch_spec` can find the entry
    // once the frontend mounts the new tab and calls `pty_start` (same
    // ordering rationale as `create_shell_tab`). Append, like a shell tab —
    // the visible position is owned by the frontend layout.
    {
        let mut snap = state.settings.current();
        snap.tabs.push(TabConfig::AiTool(cfg));
        state.settings.set(snap);
    }

    let position = {
        let mut registry = state.tabs.lock().await;
        registry.insert_user_tab(tab.clone(), name.clone())
    };

    if let Err(e) = state
        .state_signals
        .send(StateSignal::TabAdded {
            meta: tab_meta,
            position,
        })
        .await
    {
        warn!(error = %e, "create_ai_tab: state-signal channel closed");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }

    {
        let mut registry = state.tabs.lock().await;
        if let Err(e) = registry.activate(tab.clone()).await {
            warn!(error = %e, "create_ai_tab: activate failed");
        }
    }

    let _ = app; // reserved for future per-window emits
    info!(?tab, ?template, "ai tab duplicated");
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
    let _serializer = state.lifecycle_serializer.lock().await;
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
    let _serializer = state.lifecycle_serializer.lock().await;
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
    let _serializer = state.lifecycle_serializer.lock().await;
    if !matches!(tab.kind(), TabKind::Shell) {
        return Err(TabLifecycleError::WrongKind);
    }
    let args = shlex::split(&args_string).unwrap_or_default();
    let validated = validate_inputs(name, command, args, cwd, env).await?;
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
        notifications_error: notif_defaults.error.text,
        notifications_exited: notif_defaults.exited.text,
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
        notifications_error: cfg.notifications.error.text.clone(),
        notifications_exited: cfg.notifications.exited.text.clone(),
    })
}

/// Apply a new `enabled_ai_tabs` value: open and close the AI builtin
/// tabs as needed so the live tabs match the selection. Bypasses
/// `BuiltinNotClosable` (the checkbox group is the canonical way to
/// close an AI tab); regular `close_tab` calls on AI builtins still
/// reject. Order matters — we add the newly-enabled tab(s) first (so
/// the active-tab switch has somewhere to go), then move active off
/// any soon-to-be-removed tab, then drop the now-disabled tab(s) (kill
/// PTY, drop scrollback, remove the settings entry, emit TabRemoved).
///
/// Empty input is rejected — the user must keep at least one AI tab
/// enabled. The Settings UI enforces this with a last-checked lock;
/// this server-side check is defense-in-depth.
#[tauri::command]
pub async fn set_enabled_ai_tabs(
    state: State<'_, AppState>,
    value: Vec<AiTabId>,
) -> Result<(), TabLifecycleError> {
    if value.is_empty() {
        return Err(TabLifecycleError::EmptyAiTabsList);
    }

    // De-dup while preserving the user's intended order — useful only
    // as defense against a malformed IPC payload, since the UI cannot
    // produce duplicates.
    let mut seen: std::collections::HashSet<AiTabId> =
        std::collections::HashSet::with_capacity(value.len());
    let want_ordered: Vec<AiTabId> = value
        .into_iter()
        .filter(|id| seen.insert(*id))
        .collect();
    let want: std::collections::HashSet<AiTabId> = want_ordered.iter().copied().collect();

    // Serialize with all other lifecycle commands so concurrent
    // close_tab / create_shell_tab / etc. can't interleave with the
    // multi-step add-then-remove sequence below.
    let _serializer = state.lifecycle_serializer.lock().await;

    let (have, prev_value) = {
        let snap = state.settings.current();
        let have: std::collections::HashSet<AiTabId> = snap
            .tabs
            .iter()
            .filter_map(|t| AiTabId::from_id(t.id()))
            .collect();
        (have, snap.enabled_ai_tabs.clone())
    };
    let prev_set: std::collections::HashSet<AiTabId> = prev_value.iter().copied().collect();
    if prev_set == want && have == want {
        return Ok(());
    }

    // Canonical add order: claude → claude-local → aider → aider-local
    // so insertions land in the right relative slot.
    let canonical = [
        AiTabId::Claude,
        AiTabId::ClaudeLocal,
        AiTabId::Aider,
        AiTabId::AiderLocal,
    ];

    // 1. Add any newly-enabled tabs.
    for &id in &canonical {
        if want.contains(&id) && !have.contains(&id) {
            add_ai_builtin_tab(&state, id).await?;
        }
    }

    // 2. If the currently-active tab is about to be removed, switch
    //    to a surviving AI tab first so the frontend's active
    //    indicator (and the TTS active cell) doesn't briefly point at
    //    a tab that no longer exists. Pick the first id from `want` in
    //    canonical order.
    let surviving: Option<TabId> = canonical
        .iter()
        .find(|id| want.contains(id))
        .map(|id| TabId::from_str(id.as_str()));
    if let Some(target) = surviving.clone() {
        let active = {
            let registry = state.tabs.lock().await;
            registry.active()
        };
        let active_id_opt = AiTabId::from_id(active.as_str());
        let about_to_close = active_id_opt
            .map(|id| have.contains(&id) && !want.contains(&id))
            .unwrap_or(false);
        if about_to_close && active != target {
            let mut registry = state.tabs.lock().await;
            if let Err(e) = registry.activate(target).await {
                warn!(error = %e, "set_enabled_ai_tabs: pre-close activate failed");
            }
        }
    }

    // 3. Remove any newly-disabled tabs.
    for &id in &canonical {
        if !want.contains(&id) && have.contains(&id) {
            remove_ai_builtin_tab(&state, TabId::from_str(id.as_str())).await?;
        }
    }

    // 4. Persist the new setting value alongside the post-step-1/3 tabs
    //    array.
    {
        let mut snap = state.settings.current();
        if snap.enabled_ai_tabs != want_ordered {
            snap.enabled_ai_tabs = want_ordered.clone();
            state.settings.set(snap);
        }
    }

    info!(?want_ordered, "enabled_ai_tabs updated");
    Ok(())
}

/// A built-in launchable tool, exposed as a bottom-bar quick-launch button
/// (V16). Each tool runs a fixed command; launching one spawns a fresh
/// *closable* Shell tab so the user can open as many as they like and close
/// them individually — these are situational tools, not persistent builtins.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Terminal network monitor. Resolved via PATH as `rustnet`.
    Rustnet,
    /// broot file browser with git info (`broot -g`). Resolved via PATH.
    Broot,
}

impl ToolKind {
    /// `(display name, command, args)` for the tool. The command is resolved
    /// via PATH at spawn time, exactly like a user-typed Shell-tab command.
    fn spec(self) -> (&'static str, &'static str, &'static [&'static str]) {
        match self {
            ToolKind::Rustnet => ("rustnet", "rustnet", &[]),
            ToolKind::Broot => ("broot", "broot", &["-g"]),
        }
    }
}

/// Launch a built-in tool (rustnet / broot) into a fresh closable Shell tab
/// (V16). Mirrors `create_shell_tab`'s persist → register → `TabAdded` →
/// activate flow, but builds the spawn config from the tool's fixed preset
/// and — crucially — does **not** PATH-validate the command up front: a
/// missing tool still spawns the tab, which then shows the standard
/// "command not found" closed overlay (the same UX the old seeded broot tab
/// had). The new tab lands in the focused pane and becomes active.
///
/// The tab is an ordinary `builtin: false` Shell tab with a uuid id, so it
/// shows the close `×`, is closable, and persists across restarts like any
/// user shell. Repeated launches each get a fresh tab; the display name is
/// auto-numbered (`rustnet`, `rustnet 2`, …) to keep the tab bar legible.
#[tauri::command]
pub async fn open_tool_tab(
    app: AppHandle,
    state: State<'_, AppState>,
    tool: ToolKind,
) -> Result<TabId, TabLifecycleError> {
    let _serializer = state.lifecycle_serializer.lock().await;
    let (base_name, command, args) = tool.spec();

    let tab = TabId::Shell(format!("shell-{}", Uuid::new_v4()));

    // Auto-numbered, collision-free display name (`rustnet`, `rustnet 2`, …).
    let name = {
        let snap = state.settings.current();
        let taken = snap.tabs.iter().any(|t| t.name() == base_name);
        if taken {
            unique_tab_name(&snap, base_name)
        } else {
            base_name.to_string()
        }
    };

    // Persist BEFORE registering so `build_launch_spec` finds the entry once
    // the frontend mounts the tab and calls `pty_start`.
    {
        let mut snap = state.settings.current();
        snap.tabs.push(TabConfig::Shell(ShellTabSettings {
            id: tab.as_str().to_string(),
            builtin: false,
            name: name.clone(),
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: None,
            env: HashMap::new(),
            notifications: ShellNotificationConfig::default(),
            theme_override: None,
            background_override: None,
        }));
        state.settings.set(snap);
    }

    let position = {
        let mut registry = state.tabs.lock().await;
        registry.insert_user_tab(tab.clone(), name.clone())
    };

    if let Err(e) = state
        .state_signals
        .send(StateSignal::TabAdded {
            meta: TabMeta {
                id: tab.clone(),
                kind: TabKind::Shell,
                name,
            },
            position,
        })
        .await
    {
        warn!(error = %e, "open_tool_tab: state-signal channel closed");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }

    {
        let mut registry = state.tabs.lock().await;
        if let Err(e) = registry.activate(tab.clone()).await {
            warn!(error = %e, "open_tool_tab: activate failed");
        }
    }

    let _ = app; // reserved for future per-window emits
    info!(?tab, ?tool, "tool tab opened");
    Ok(tab)
}

async fn add_ai_builtin_tab(
    state: &State<'_, AppState>,
    id: AiTabId,
) -> Result<(), TabLifecycleError> {
    let config = default_ai_tab(id);
    let name = config.name().to_string();
    let tab = TabId::from_str(id.as_str());
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
            // Canonical position: walk the existing tabs and find the
            // last reserved-AI tab whose canonical order is below this
            // id's. Insert immediately after it. This keeps AI tabs in
            // the canonical leading order regardless of which subset
            // the user has enabled.
            let target = id.canonical_order();
            let mut pos = 0usize;
            for (idx, tab) in snap.tabs.iter().enumerate() {
                match AiTabId::from_id(tab.id()) {
                    Some(other) if other.canonical_order() < target => {
                        pos = idx + 1;
                    }
                    _ => break,
                }
            }
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
        warn!(error = %e, "set_enabled_ai_tabs: state-signal channel closed (add)");
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
        warn!(?tab, error = %e, "set_enabled_ai_tabs: scrollback delete failed");
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
        warn!(error = %e, "set_enabled_ai_tabs: state-signal channel closed (remove)");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }
    info!(?tab, "ai builtin tab removed");
    Ok(())
}
