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
//! **V42 Phase A1-3 finished the job.** `reconfigure_shell_tab`,
//! `set_enabled_ai_tabs`, `open_tool_tab`, `open_note_tab` and
//! `create_ai_tab_in_worktree` were outside the Phase 0 slice and held their
//! own bodies; those are `service::tabs` methods now, and the three copies of
//! the commit sequence they carried went into `commit_new_tab` with them (Phase
//! 0 decision 3: the reserved-AI-builtin placement is a named parameter, not an
//! accident). `default_shell_spec` is the one command left holding a body, and
//! its doc says why.
//!
//! One ordering rule stayed at this boundary on purpose:
//! `create_ai_tab_in_worktree` holds the lifecycle serializer itself, because it
//! has to span the `git worktree add` between the service's template read and
//! its commit.
//!
//! Every command still persists its mutation to settings via `SettingsHandle`,
//! which broadcasts the new state to all listeners and triggers a debounced
//! disk write. Settings is the single source of truth for tab identity, name
//! and spawn config — there is no per-tab side table.

use std::collections::HashMap;
use std::path::PathBuf;

use tauri::{AppHandle, State};
use tracing::info;

use crate::ipc::AppState;
use crate::service::sink::WebviewHost;
use crate::service::tabs::TabService;
pub use crate::service::tabs::{
    DefaultShellWire, ShellTabConfigWire, TabLifecycleError, ToolKind,
};
use crate::settings::{AiTabId, ShellNotificationConfig};
use crate::shell::detect;
use crate::state::TabId;

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
        &state.launch.cwd,
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
    // The serializer is held HERE rather than inside the service, because it
    // has to span the worktree creation between the two service calls: a
    // concurrent create must not see the settings snapshot the template read
    // took and the one the commit writes as two different documents.
    let _serializer = state.lifecycle_serializer.lock().await;
    let service = tab_service(&state);

    // Validate the template BEFORE creating the worktree — this read has no
    // side effects, so an invalid template cannot orphan a freshly-created
    // branch and directory on disk. (Failures PAST `worktree_create` still do
    // not roll it back; see the doc comment.)
    let cfg = service.ai_template(&template)?;

    let root_path = match root.as_deref().map(str::trim) {
        Some(r) if !r.is_empty() => PathBuf::from(r),
        _ => state.launch.cwd.clone(),
    };
    let wt_path = workbench
        .worktree_create(&root_path, &slug)
        .await
        .map_err(|e| TabLifecycleError::internal(format!("create worktree: {e}")))?;

    let base_name = format!("⑂ {slug}: {}", cfg.name);
    let tab = service
        .commit_ai_duplicate(
            cfg,
            &base_name,
            Some(wt_path),
            "create_ai_tab_in_worktree",
        )
        .await?;

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
    tab_service(&state)
        .reconfigure_shell(
            tab,
            name,
            command,
            args_string,
            cwd,
            env,
            notifications_error,
            notifications_exited,
            theme_override,
            background_override,
        )
        .await
}

/// Query the platform default shell spec. Frontend's New Shell Tab
/// dialog calls this to populate the command + args defaults plus the
/// platform-default notification text (used to pre-fill the new fields
/// added in M4).
///
/// **Left as a direct call** (V42 Phase A): it takes no `State` and no
/// `AppHandle`, so it was already callable from a test — the same negative
/// finding `service::view`'s module doc records for `activity_list`. Wrapping
/// it would move ten lines and buy no reachability.
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

/// Look up the current Shell-tab config from settings. Returns `WrongKind`
/// for AI tabs and `TabNotFound` for unknown ids.
#[tauri::command]
pub async fn get_shell_tab_config(
    state: State<'_, AppState>,
    tab: TabId,
) -> Result<ShellTabConfigWire, TabLifecycleError> {
    tab_service(&state).shell_tab_config(&tab)
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
    tab_service(&state).set_enabled_ai(value).await
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
    let _ = app; // reserved for future per-window emits
    tab_service(&state).open_tool(tool).await
}

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
    let _ = app; // reserved for future per-window emits
    tab_service(&state).open_note().await
}

