//! Tab-lifecycle IPC: create / close / rename / reconfigure user-managed
//! tabs.
//!
//! **V42 Phase 0.** The use cases moved to [`crate::service::tabs`]; what is
//! left here is the wire boundary. A command in this file names the Tauri
//! things the service cannot get for itself (`State<'_, AppState>`, the
//! `PreviewRegistry` behind [`WebviewHost`]) and delegates. The serde-tagged
//! error shape the dialogs match on ([`TabLifecycleError`]) is re-exported
//! from here so the frontend contract reads unchanged.
//!
//! Not everything moved: `reconfigure_shell_tab`, `set_enabled_ai_tabs`,
//! `open_tool_tab`, `open_note_tab`, `create_ai_tab_in_worktree` and the
//! reserved-feature-tab sync are outside the Phase 0 slice and still hold
//! their own bodies. They share the validation helpers and the name-uniquing
//! helpers with the service rather than keeping a second copy — those live in
//! `service::tabs` now, one call away.
//!
//! Every command still persists its mutation to settings via `SettingsHandle`,
//! which broadcasts the new state to all listeners and triggers a debounced
//! disk write. Settings is the single source of truth for tab identity, name
//! and spawn config — there is no per-tab side table.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tracing::{info, warn};
use uuid::Uuid;

use crate::ipc::AppState;
use crate::service::sink::WebviewHost;
use crate::service::tabs::{
    notifications_from_dialog, unique_tab_name, validate_inputs, TabService,
};
pub use crate::service::tabs::TabLifecycleError;
use crate::settings::{
    default_ai_tab, AiTabId, Settings, ShellNotificationConfig,
    ShellTabConfig as ShellTabSettings, TabConfig,
};
use crate::shell::detect;
use crate::state::{StateSignal, TabId, TabKind, TabMeta};

/// Build the tab service over this app's handles. One place, so no command
/// can drift in what it hands it — and `pub(crate)` because the two
/// activation commands live in [`crate::ipc::commands`].
pub(crate) fn tab_service(state: &AppState) -> TabService<'_> {
    TabService::new(
        &state.settings,
        &state.tabs,
        &state.state_signals,
        &state.read_only,
        &state.lifecycle_serializer,
    )
}

/// Create a new user-managed Shell tab. See [`TabService::create_shell`].
// Tauri command: each parameter is a field of the frontend's `invoke` payload,
// so collapsing them into a struct changes the IPC contract.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn create_shell_tab(
    state: State<'_, AppState>,
    name: String,
    command: String,
    args_string: String,
    cwd: Option<String>,
    env: HashMap<String, String>,
    notifications_error: String,
    notifications_exited: String,
) -> Result<TabId, TabLifecycleError> {
    tab_service(&state)
        .create_shell(
            name,
            command,
            args_string,
            cwd,
            env,
            notifications_error,
            notifications_exited,
        )
        .await
}

/// Create a new user-managed Preview tab. See [`TabService::create_preview`].
#[tauri::command]
pub async fn create_preview_tab(
    state: State<'_, AppState>,
    url: String,
) -> Result<TabId, TabLifecycleError> {
    tab_service(&state).create_preview(url).await
}

/// Spawn a duplicate of an existing AI tab. See [`TabService::create_ai`].
#[tauri::command]
pub async fn create_ai_tab(
    state: State<'_, AppState>,
    template: TabId,
) -> Result<TabId, TabLifecycleError> {
    tab_service(&state).create_ai(template).await
}

/// V13 Phase D D3: "New <Claude|OpenCode> tab in worktree…" — the worktree
/// counterpart of [`create_ai_tab`]. Creates a fresh cImp worktree (branch
/// `cimp/<slug>` under `<root>/.cimp/worktrees/<slug>`, refused per
/// `workbench::worktree::create`'s preconditions: nested repo, detached
/// `HEAD`, duplicate slug), then duplicates `template`'s AI config exactly
/// like the plain "+" duplicate — except the new tab's `cwd` is set to the
/// worktree's path (D2: threaded through `build_ai_tool_spec` /
/// `pty::manager` exactly the way a Shell tab's `cwd` already is) and its
/// display name is prefixed `⑂ <slug>` so it reads as worktree-scoped at a
/// glance. `root` defaults to the launch directory, like every other
/// Workbench IPC command.
///
/// If the worktree is created but tab registration then fails (the
/// state-signal-channel-closed case `create_ai_tab` also guards), the
/// worktree is intentionally left in place rather than auto-discarded —
/// `discard` is a double-confirmed, branch-deleting action the milestone
/// reserves for an explicit user click; an orphaned (no-live-tab) worktree
/// just shows up in the Worktrees section for manual cleanup, exactly like
/// one whose tab was later closed.
#[tauri::command]
pub async fn create_ai_tab_in_worktree(
    app: AppHandle,
    state: State<'_, AppState>,
    workbench: State<'_, std::sync::Arc<crate::workbench::WorkbenchService>>,
    template: TabId,
    root: Option<String>,
    slug: String,
) -> Result<TabId, TabLifecycleError> {
    let _serializer = state.lifecycle_serializer.lock().await;

    let root_path = match root.as_deref().map(str::trim) {
        Some(r) if !r.is_empty() => PathBuf::from(r),
        _ => state.launch.cwd.clone(),
    };

    // Validate the template BEFORE creating the worktree. Same rule as
    // `create_ai_tab`: the worktree-tab affordance only appears on AI tabs, so
    // a missing entry or a Shell template is a malformed request. This read has
    // no side effects, so doing it first means an invalid template can't orphan
    // a freshly-created branch + worktree dir on disk. (Failures *past* the
    // `worktree_create` below still don't roll back — see the doc comment.)
    let mut cfg = {
        let snap = state.settings.current();
        let entry =
            snap.find_tab(template.as_str())
                .ok_or_else(|| TabLifecycleError::TabNotFound {
                    tab: template.as_str().to_string(),
                })?;
        match entry {
            TabConfig::AiTool(ai) => ai.clone(),
            TabConfig::Shell(_) | TabConfig::Preview(_) => {
                return Err(TabLifecycleError::WrongKind)
            }
        }
    };

    let wt_path = workbench
        .worktree_create(&root_path, &slug)
        .await
        .map_err(|e| TabLifecycleError::internal(format!("create worktree: {e}")))?;

    let tab = TabId::Ai(format!("ai-{}", Uuid::new_v4()));
    let base_name = format!("⑂ {slug}: {}", cfg.name);
    let name = unique_tab_name(&state.settings.current(), &base_name);
    cfg.id = tab.as_str().to_string();
    cfg.builtin = false;
    cfg.name = name.clone();
    cfg.cwd = Some(wt_path);

    let tab_meta = TabMeta {
        id: tab.clone(),
        kind: TabKind::AiTool,
        name: name.clone(),
    };

    // Persist BEFORE registering — same ordering rationale as `create_ai_tab`.
    state.settings.mutate(move |snap| {
        snap.tabs.push(TabConfig::AiTool(cfg));
    });

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
        warn!(error = %e, "create_ai_tab_in_worktree: state-signal channel closed");
        let id = tab.as_str().to_string();
        state
            .settings
            .mutate(move |snap| snap.tabs.retain(|t| t.id() != id));
        state.tabs.lock().await.remove_tab(&tab).await;
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }

    {
        let mut registry = state.tabs.lock().await;
        if let Err(e) = registry.activate(tab.clone()).await {
            warn!(error = %e, "create_ai_tab_in_worktree: activate failed");
        }
    }

    let _ = app; // reserved for future per-window emits
    info!(?tab, ?template, %slug, "ai tab created in worktree");
    Ok(tab)
}

/// Close a user tab. Builtins reject with `BuiltinNotClosable`. See
/// [`TabService::close`] — the `PreviewRegistry` this extracts is the
/// [`WebviewHost`] the close path needs for a Preview tab's child webview.
#[tauri::command]
pub async fn close_tab(
    state: State<'_, AppState>,
    preview_registry: State<'_, crate::preview::PreviewRegistry>,
    tab: TabId,
) -> Result<(), TabLifecycleError> {
    let webviews: &dyn WebviewHost = preview_registry.inner();
    tab_service(&state).close(tab, webviews).await
}

/// Rename any tab. See [`TabService::rename`].
#[tauri::command]
pub async fn rename_tab(
    state: State<'_, AppState>,
    tab: TabId,
    new_name: String,
) -> Result<(), TabLifecycleError> {
    tab_service(&state).rename(tab, new_name).await
}

/// Update a Shell tab's spawn config. Does NOT respawn — the new config
/// takes effect on next restart (manual via the closed-overlay Enter
/// affordance, or automatic on subprocess exit).
// Tauri command: parameters are the `invoke` payload fields (see
// `create_shell_tab`).
#[allow(clippy::too_many_arguments)]
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
    let args = shlex::split(&args_string).unwrap_or_else(|| {
        warn!(args = %args_string, "tab args have unbalanced quotes; treating as no args");
        Vec::new()
    });
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

    // Validate existence + kind on a read snapshot (so we can return a typed
    // error), then apply atomically via mutate so a concurrent save_layout /
    // settings_update can't clobber the reconfigured entry. The closure
    // re-checks under the held lock and no-ops if the tab vanished meanwhile.
    match state.settings.current().find_tab(tab.as_str()) {
        None => {
            return Err(TabLifecycleError::TabNotFound {
                tab: tab.as_str().to_string(),
            })
        }
        Some(TabConfig::Shell(_)) => {}
        Some(TabConfig::AiTool(_)) | Some(TabConfig::Preview(_)) => {
            return Err(TabLifecycleError::WrongKind)
        }
    }
    let new_command = validated.command.to_string_lossy().into_owned();
    let new_name = validated.name.clone();
    let new_args = validated.args.clone();
    let new_cwd = validated.cwd.clone();
    let new_env = validated.env.clone();
    let tab_id = tab.as_str().to_string();
    state.settings.mutate(move |snap| {
        if let Some(TabConfig::Shell(cfg)) = snap.find_tab_mut(&tab_id) {
            cfg.name = new_name;
            cfg.command = new_command;
            cfg.args = new_args;
            cfg.cwd = new_cwd;
            cfg.env = new_env;
            cfg.notifications = notifications;
            cfg.theme_override = theme_override;
            cfg.background_override = background_override;
            // builtin/id stay as they were.
        }
    });

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
    let (spec, source) = detect::default_shell_resolution();
    let notif_defaults = ShellNotificationConfig::default();
    DefaultShellWire {
        command: spec.command.to_string_lossy().into_owned(),
        args: spec.args.join(" "),
        // Derived from the resolution above — `was_default_git_bash_found`
        // used to re-run the entire probe chain (file checks, registry
        // read, PATH walk) for this one bool.
        git_bash_found: detect::is_git_bash_source(&source),
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
    let want_ordered: Vec<AiTabId> = value.into_iter().filter(|id| seen.insert(*id)).collect();
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

    // Gate: don't enable a harness's tab unless that harness says it can be.
    // Probed only for tabs being turned ON in this call, and refused before any
    // state changes, so the toggle stays atomic (the UI reverts the checkbox and
    // shows the reason).
    //
    // **V40 Phase E, locked decision 26.** This used to be one hard-coded
    // `resolve_command("opencode")` with the other harness's exemption stated in
    // a comment — an exemption a third harness would have inherited by accident.
    // Each plugin answers `preflight()` for itself; Claude's "not gated, it's the
    // app's own front end" is a declared `Ok`.
    for &id in want.iter() {
        if have.contains(&id) {
            continue;
        }
        let Some(harness) = crate::harness::HarnessId::from_tab_id(id.as_str()) else {
            continue;
        };
        let Some(plugin) = harness.plugin() else {
            continue;
        };
        // `preflight` resolves a binary, so it runs off the async runtime.
        let verdict = tokio::task::spawn_blocking(move || plugin.preflight())
            .await
            .map_err(|e| {
                TabLifecycleError::internal(format!("{} preflight join: {e}", harness.token()))
            })?;
        if let Err(hint) = verdict {
            return Err(TabLifecycleError::HarnessNotFound {
                harness: harness.token().to_string(),
                label: harness.label().to_string(),
                hint: hint.to_string(),
            });
        }
    }

    // Canonical add order: claude → claude-local → opencode so insertions
    // land in the right relative slot.
    let canonical = crate::settings::canonical_ai_tab_order();

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
    if state.settings.current().enabled_ai_tabs != want_ordered {
        let v = want_ordered.clone();
        state.settings.mutate(move |snap| {
            snap.enabled_ai_tabs = v;
        });
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
    /// Terminal network monitor. Resolved as `rustnet` (ebin → PATH).
    Rustnet,
    /// broot file browser with git info, showing hidden files
    /// (`broot -g -h`). Resolved (ebin → PATH).
    Broot,
}

impl ToolKind {
    /// `(display name, command, args)` for the tool. The command is resolved
    /// at spawn time (drop-in `ebin/` first, then PATH — see `pty::resolve`),
    /// exactly like a user-typed Shell-tab command.
    fn spec(self) -> (&'static str, &'static str, &'static [&'static str]) {
        match self {
            ToolKind::Rustnet => ("rustnet", "rustnet", &[]),
            ToolKind::Broot => ("broot", "broot", &["-g", "-h"]),
        }
    }

    /// The launch command for this tool: an explicit per-tool path from
    /// `settings.external_tools` when the user set one
    /// (Settings → Bottom bar), otherwise the default bare name (resolved
    /// ebin → PATH at spawn).
    fn command_for(self, settings: &Settings) -> String {
        let override_path = match self {
            ToolKind::Rustnet => settings.external_tools.rustnet.trim(),
            ToolKind::Broot => settings.external_tools.broot.trim(),
        };
        if override_path.is_empty() {
            self.spec().1.to_string()
        } else {
            override_path.to_string()
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
    let (base_name, _default_command, args) = tool.spec();

    let tab = TabId::Shell(format!("shell-{}", Uuid::new_v4()));

    // One settings snapshot: drives the collision-free display name AND the
    // launch command (honoring an explicit external-tool path override).
    let snap = state.settings.current();
    let command = tool.command_for(&snap);

    // Auto-numbered, collision-free display name (`rustnet`, `rustnet 2`, …).
    let name = {
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
        let entry = TabConfig::Shell(ShellTabSettings {
            id: tab.as_str().to_string(),
            builtin: false,
            name: name.clone(),
            command,
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: None,
            env: HashMap::new(),
            notifications: ShellNotificationConfig::default(),
            theme_override: None,
            background_override: None,
        });
        state.settings.mutate(move |snap| {
            snap.tabs.push(entry);
        });
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
        // Roll back the committed settings + registry entries (see create_shell_tab).
        let id = tab.as_str().to_string();
        state
            .settings
            .mutate(move |snap| snap.tabs.retain(|t| t.id() != id));
        state.tabs.lock().await.remove_tab(&tab).await;
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

/// The reserved id of the singleton Note tab. It's an ordinary Shell-kind tab
/// on the backend (so it needs no dedicated `TabId` variant and is freely
/// closable), but it runs no PTY: the frontend keys off this id to render the
/// `NoteView` editor instead of an xterm (see `tabs/types.ts` `isNoteTab`).
pub const NOTE_TAB_ID: &str = "note";

/// Open the Note scratchpad tab (bottom-bar button). Singleton: if the tab is
/// already open it's simply re-activated; otherwise a fresh closable Shell tab
/// with the reserved [`NOTE_TAB_ID`] id is created, persisted, and activated —
/// the same persist → register → `TabAdded` → activate flow as `open_tool_tab`,
/// minus a spawn command (the tab has no PTY). The backing note file
/// (`.cimp/cimp.note.txt`) is created up front so the button "opens an existing
/// note or creates one"; `read_note` reads it once the frontend mounts.
#[tauri::command]
pub async fn open_note_tab(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TabId, TabLifecycleError> {
    let _serializer = state.lifecycle_serializer.lock().await;
    let tab = TabId::Shell(NOTE_TAB_ID.to_string());

    // Create `.cimp/cimp.note.txt` (and `.cimp/`) if missing. Non-fatal: the
    // tab still opens on failure and `read_note` retries the create.
    if let Err(e) = crate::ipc::note::ensure_note_file(&state.launch.cwd) {
        warn!(error = %e, "open_note_tab: ensure note file failed; opening tab anyway");
    }

    // Singleton: re-activate an already-open note tab instead of duplicating.
    {
        let mut registry = state.tabs.lock().await;
        if registry.has_tab(&tab) {
            if let Err(e) = registry.activate(tab.clone()).await {
                warn!(error = %e, "open_note_tab: activate existing failed");
            }
            let _ = app;
            return Ok(tab);
        }
    }

    let name = "Note".to_string();

    // Persist BEFORE registering, guarding against a stale duplicate entry.
    {
        let entry = TabConfig::Shell(ShellTabSettings {
            id: NOTE_TAB_ID.to_string(),
            builtin: false,
            name: name.clone(),
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            notifications: ShellNotificationConfig::default(),
            theme_override: None,
            background_override: None,
        });
        state.settings.mutate(move |snap| {
            if !snap.tabs.iter().any(|t| t.id() == NOTE_TAB_ID) {
                snap.tabs.push(entry);
            }
        });
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
        warn!(error = %e, "open_note_tab: state-signal channel closed");
        // Roll back the committed settings + registry entries (see open_tool_tab).
        state
            .settings
            .mutate(|snap| snap.tabs.retain(|t| t.id() != NOTE_TAB_ID));
        state.tabs.lock().await.remove_tab(&tab).await;
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }

    {
        let mut registry = state.tabs.lock().await;
        if let Err(e) = registry.activate(tab.clone()).await {
            warn!(error = %e, "open_note_tab: activate failed");
        }
    }

    let _ = app; // reserved for future per-window emits
    info!(?tab, "note tab opened");
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
        let tab_id = tab.as_str().to_string();
        state.settings.mutate(move |snap| {
            if let Some(existing) = snap.tabs.iter_mut().find(|t| t.id() == tab_id) {
                *existing = config;
            } else {
                // Canonical position: walk the existing tabs and find the
                // last reserved-AI tab whose canonical order is below this
                // id's. Insert immediately after it. This keeps AI tabs in
                // the canonical leading order regardless of which subset
                // the user has enabled.
                let target = id.canonical_order();
                let mut pos = 0usize;
                for (idx, t) in snap.tabs.iter().enumerate() {
                    match AiTabId::from_id(t.id()) {
                        Some(other) if other.canonical_order() < target => {
                            pos = idx + 1;
                        }
                        _ => break,
                    }
                }
                let pos = pos.min(snap.tabs.len());
                snap.tabs.insert(pos, config);
            }
        });
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
        // Roll back the committed settings + registry entries.
        let id_str = tab.as_str().to_string();
        state
            .settings
            .mutate(move |snap| snap.tabs.retain(|t| t.id() != id_str));
        state.tabs.lock().await.remove_tab(&tab).await;
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

    // V39 Phase A: same for its read-only row (mirrors `close_tab`).
    state.read_only.forget(&tab);
    // V39 Phase B: …and the same delegation signal, for the same reason.
    crate::delegation::note_worker_gone(&tab);

    state.settings.mutate(|snap| {
        snap.tabs.retain(|t| t.id() != tab.as_str());
        if snap.session.active_tab_id.as_deref() == Some(tab.as_str()) {
            snap.session.active_tab_id = None;
        }
    });

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
