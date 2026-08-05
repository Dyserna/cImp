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
    default_ai_tab, AiTabId, Settings, ShellNotificationConfig, ShellTabConfig as ShellTabSettings,
    TabConfig,
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
    /// An attempt to enable an OpenCode tab (cloud or local) while the
    /// `opencode` command can't be resolved (not in `ebin`, not on PATH). The
    /// tab is left disabled and the UI surfaces this so the user can install
    /// OpenCode first — unlike Claude (the app's own front end), a missing
    /// OpenCode would only ever show a dead "command not found" tab. cimp does
    /// not bundle the ~158 MB binary (V19 require-install decision).
    OpencodeNotFound,
    /// Internal error (lock poisoning, channel send failure, etc.). Not
    /// expected in practice — surfaces as a toast on the frontend.
    Internal { message: String },
}

impl TabLifecycleError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal {
            message: msg.into(),
        }
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

        let cwd_path = match cwd_string
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => {
                let p = PathBuf::from(s);
                if !p.is_dir() {
                    return Err(TabLifecycleError::CwdNotFound {
                        path: s.to_string(),
                    });
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
// Tauri command: each parameter is a field of the frontend's `invoke` payload,
// so collapsing them into a struct changes the IPC contract.
#[allow(clippy::too_many_arguments)]
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
    let args = shlex::split(&args_string).unwrap_or_else(|| {
        warn!(args = %args_string, "tab args have unbalanced quotes; treating as no args");
        Vec::new()
    });
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
        let entry = TabConfig::Shell(validated_to_shell_config(
            tab.as_str().to_string(),
            false,
            &validated,
            notifications,
        ));
        // Atomic mutate (not current()/set()) so a concurrent save_layout /
        // settings_update / set_active_tab can't clobber the new tab with a
        // stale whole-struct snapshot. Idempotent on duplicate id.
        let id = tab.as_str().to_string();
        state.settings.mutate(move |snap| {
            if let Some(existing) = snap.tabs.iter_mut().find(|t| t.id() == id) {
                *existing = entry;
            } else {
                snap.tabs.push(entry);
            }
        });
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
        // Roll back the committed settings + registry entries so a phantom tab
        // doesn't persist (and resurrect on next launch) with no frontend view.
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
            warn!(error = %e, "create_shell_tab: activate failed");
        }
    }

    let _ = app; // reserved for future per-window emits
    info!(?tab, "shell tab created");
    Ok(tab)
}

/// Create a new user-managed Preview tab (the "+"-adjacent "New Preview tab"
/// affordance — V14 Phase F). Unlike `create_shell_tab`/`create_ai_tab`
/// there is no command/binary/cwd/env to validate — a Preview tab has no
/// PTY at all, just a `url`/`device_width`/`auto_reload` triple the backend
/// child webview (`crate::preview`) reads. An empty `url` falls back to
/// `Settings::preview_last_url` (the last URL used by any Preview tab in
/// this project), then `preview::DEFAULT_PREVIEW_URL`.
#[tauri::command]
pub async fn create_preview_tab(
    state: State<'_, AppState>,
    url: String,
) -> Result<TabId, TabLifecycleError> {
    let _serializer = state.lifecycle_serializer.lock().await;

    let snap = state.settings.current();
    let resolved_url = {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            snap.preview_last_url
                .clone()
                .unwrap_or_else(|| crate::preview::DEFAULT_PREVIEW_URL.to_string())
        } else {
            trimmed.to_string()
        }
    };
    let name = unique_preview_tab_name(&snap);

    let tab = TabId::Preview(format!("preview-{}", Uuid::new_v4()));
    let tab_meta = TabMeta {
        id: tab.clone(),
        kind: TabKind::Preview,
        name: name.clone(),
    };

    // Persist BEFORE registering — same ordering rationale as
    // `create_shell_tab`/`create_ai_tab` (though Preview tabs never call
    // `pty_start`, keeping the ordering uniform avoids a special case here).
    {
        let entry = crate::settings::PreviewTabConfig {
            id: tab.as_str().to_string(),
            builtin: false,
            name: name.clone(),
            url: resolved_url.clone(),
            device_width: None,
            auto_reload: false,
        };
        state.settings.mutate(move |s| {
            s.tabs.push(TabConfig::Preview(entry.clone()));
            s.preview_last_url = Some(resolved_url.clone());
        });
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
        warn!(error = %e, "create_preview_tab: state-signal channel closed");
        let id = tab.as_str().to_string();
        state
            .settings
            .mutate(move |s| s.tabs.retain(|t| t.id() != id));
        state.tabs.lock().await.remove_tab(&tab).await;
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }

    {
        let mut registry = state.tabs.lock().await;
        if let Err(e) = registry.activate(tab.clone()).await {
            warn!(error = %e, "create_preview_tab: activate failed");
        }
    }

    info!(?tab, "preview tab created");
    Ok(tab)
}

/// "Preview" if untaken, else the lowest-free-integer-suffixed "Preview N"
/// (N ≥ 2) — unlike [`unique_tab_name`] (which always suffixes, because its
/// caller's `base` is an existing template's name), a Preview tab has no
/// pre-existing instance to collide with, so the FIRST one is plain
/// "Preview".
fn unique_preview_tab_name(settings: &Settings) -> String {
    let taken: std::collections::HashSet<&str> = settings.tabs.iter().map(|t| t.name()).collect();
    if !taken.contains("Preview") {
        return "Preview".to_string();
    }
    for n in 2..1000 {
        let candidate = format!("Preview {n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    format!("Preview {}", Uuid::new_v4())
}

/// Pick a unique display name for a spawned duplicate by suffixing the
/// template's name with the lowest free integer ≥ 2 (e.g. "Claude" →
/// "Claude 2", "Claude 3"). Falls back to a uuid suffix in the
/// (practically impossible) event the first thousand are all taken.
fn unique_tab_name(settings: &Settings, base: &str) -> String {
    let taken: std::collections::HashSet<&str> = settings.tabs.iter().map(|t| t.name()).collect();
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
        warn!(error = %e, "create_ai_tab: state-signal channel closed");
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
            warn!(error = %e, "create_ai_tab: activate failed");
        }
    }

    let _ = app; // reserved for future per-window emits
    info!(?tab, ?template, "ai tab duplicated");
    Ok(tab)
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

/// Close a Shell tab (including the default `shell-default-1`). The AI
/// builtins (Claude / Claude-local) reject with `BuiltinNotClosable`. The
/// PTY is killed, the registry entry dropped, the settings entry removed,
/// and `TabRemoved` is emitted.
#[tauri::command]
pub async fn close_tab(
    state: State<'_, AppState>,
    preview_registry: State<'_, crate::preview::PreviewRegistry>,
    tab: TabId,
) -> Result<(), TabLifecycleError> {
    let _serializer = state.lifecycle_serializer.lock().await;
    // The reserved dashboard tabs are never closable — each is removed only
    // by disabling its feature toggle. Guard on the shared predicate (not
    // the settings `builtin` flag) so a hand-edit that clears the flag still
    // can't close them, and a new reserved dashboard can't miss this guard.
    if tab.is_reserved_dashboard() {
        return Err(TabLifecycleError::BuiltinNotClosable);
    }
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

    // Existence check, active-switch, and removal all happen under ONE lock
    // acquisition. Splitting them lets a concurrent `tab_activate` /
    // `set_active_tab` (which take `state.tabs` but NOT the lifecycle
    // serializer) re-activate this tab in the gap between the switch and the
    // remove, leaving `active` dangling at a removed tab. Holding the lock
    // across the whole sequence closes that window.
    //
    // If we're closing the active tab, switch to its left neighbor first so the
    // frontend's active-tab indicator (and the TTS active cell) points at a tab
    // that still exists. Builtins always occupy the leftmost positions, so a
    // previous tab always exists.
    let removed = {
        let mut registry = state.tabs.lock().await;
        if !registry.has_tab(&tab) {
            return Err(TabLifecycleError::TabNotFound {
                tab: tab.as_str().to_string(),
            });
        }
        if registry.active() == tab {
            // Prefer the left neighbor; if this is the leftmost tab (a closable
            // non-builtin can legitimately be at index 0 once AI builtins are
            // disabled), fall back to the right neighbor so `active` never
            // dangles at the just-removed tab.
            let target = registry
                .previous_tab(&tab)
                .or_else(|| registry.next_tab(&tab));
            if let Some(target) = target {
                if let Err(e) = registry.activate(target).await {
                    warn!(error = %e, "close_tab: activate neighbor failed");
                }
            }
        }
        registry.remove_tab(&tab).await
    };
    if !removed {
        return Err(TabLifecycleError::TabNotFound {
            tab: tab.as_str().to_string(),
        });
    }

    // V14 code-review fix (webview leak): proactively destroy a closed
    // Preview tab's child webview from the backend, rather than relying
    // solely on `PreviewToolbar.svelte`'s `onDestroy` — a renderer crash,
    // an HMR reload, or a thrown exception could skip that path entirely
    // and leak the webview for the rest of the process's life.
    // `destroy_if_open` is idempotent, so this is safe even if the
    // frontend's own cleanup also runs (whichever gets there first wins;
    // the other is a no-op).
    if tab.kind() == TabKind::Preview {
        crate::preview::destroy_if_open(&preview_registry, tab.as_str());
    }

    // V1.4-04 D.6: drop any persisted scrollback file for the closed
    // tab. The orphan-prune sweep at next launch would also catch it,
    // but cleaning up immediately keeps the disk-state consistent
    // with the user's mental model.
    if let Err(e) = crate::pty::scrollback::delete(&tab) {
        warn!(?tab, error = %e, "close_tab: scrollback delete failed");
    }

    // Drop the closed tab's typed-input echo buffer. Other per-tab maps
    // (input_lengths, settings, scrollback) are cleaned up on close; without
    // this one a long session that opens/closes many tabs slowly leaks one
    // map entry per closed tab.
    if let Ok(mut buf) = state.user_input_buf.lock() {
        buf.remove(&tab);
    }

    // Remove the settings entry. Drop the active_tab_id pointer if it
    // referenced this tab — the frontend will set a new one on its next
    // tab-switch event. Atomic mutate so a concurrent save_layout /
    // settings_update can't resurrect the just-closed tab from a stale
    // whole-struct snapshot.
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
        warn!(error = %e, "close_tab: state-signal channel closed");
        return Err(TabLifecycleError::internal("state signal channel closed"));
    }

    info!(?tab, "tab closed");
    Ok(())
}

/// V8-03/V9-01: materialize or tear down a reserved, app-rendered feature tab
/// (Code Graph monitor / Workbench / ...) **live** when its feature flag is
/// toggled in Settings, so the tab appears/disappears without an app restart.
///
/// The persisted `settings.tabs` list is kept consistent separately by
/// [`crate::settings::reconcile_reserved_tabs`]; this drives the runtime
/// registry plus the `tab-created`/`tab-closed` events the frontend uses to add
/// the tab to the bar and place it in (or remove it from) a pane. Idempotent in
/// both directions, so a redundant settings broadcast is a no-op. The caller
/// should hold `lifecycle_serializer` so this can't race `create`/`close_tab`.
pub(crate) async fn sync_reserved_feature_tab(state: &AppState, tab: TabId, enabled: bool) {
    if enabled {
        // Name + canonical position come from the (already-reconciled) settings.
        let snap = state.settings.current();
        let Some(entry) = snap.find_tab(tab.as_str()) else {
            return; // reconcile didn't add it (feature still off) — nothing to do
        };
        let name = entry.name().to_string();
        let position = snap
            .tabs
            .iter()
            .position(|t| t.id() == tab.as_str())
            .unwrap_or(snap.tabs.len());
        {
            let mut registry = state.tabs.lock().await;
            if registry.has_tab(&tab) {
                return; // already live
            }
            registry.insert_user_tab(tab.clone(), name.clone());
        }
        let meta = TabMeta {
            id: tab.clone(),
            kind: tab.kind(),
            name,
        };
        if let Err(e) = state
            .state_signals
            .send(StateSignal::TabAdded { meta, position })
            .await
        {
            warn!(error = %e, ?tab, "sync_reserved_feature_tab: add signal channel closed");
            // Roll back the registry entry so it doesn't linger without a view.
            state.tabs.lock().await.remove_tab(&tab).await;
        } else {
            info!(?tab, "reserved feature tab materialized (feature enabled)");
        }
    } else {
        let removed = {
            let mut registry = state.tabs.lock().await;
            if !registry.has_tab(&tab) {
                return; // already gone
            }
            // If the tab being removed is active, hand focus to a neighbor first
            // so `active` never dangles at the removed tab (mirrors close_tab).
            if registry.active() == tab {
                let target = registry
                    .previous_tab(&tab)
                    .or_else(|| registry.next_tab(&tab));
                if let Some(target) = target {
                    if let Err(e) = registry.activate(target).await {
                        warn!(error = %e, "sync_reserved_feature_tab: activate neighbor failed");
                    }
                }
            }
            registry.remove_tab(&tab).await
        };
        if removed {
            if let Err(e) = state
                .state_signals
                .send(StateSignal::TabRemoved { tab: tab.clone() })
                .await
            {
                warn!(error = %e, ?tab, "sync_reserved_feature_tab: remove signal channel closed");
            } else {
                info!(?tab, "reserved feature tab removed (feature disabled)");
            }
        }
    }
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
    state.settings.mutate(|snap| {
        if let Some(entry) = snap.find_tab_mut(tab.as_str()) {
            entry.set_name(trimmed.clone());
        }
    });
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

    // Gate: don't enable an OpenCode tab (cloud or local) unless the `opencode`
    // command actually resolves (ebin → PATH, the same resolution the spawn
    // path uses). Without this an enabled-but-unresolvable OpenCode tab would
    // just materialize as a dead "command not found" tab. We only probe when a
    // NEW opencode tab is being turned on, and reject before any state changes
    // so the toggle is atomic (the UI then reverts the checkbox and shows the
    // reason). Claude is intentionally not gated — it's the app's own front end.
    let enabling_opencode = [AiTabId::OpenCode]
        .iter()
        .any(|id| want.contains(id) && !have.contains(id));
    if enabling_opencode {
        let resolvable =
            tokio::task::spawn_blocking(|| crate::pty::resolve_command("opencode").is_ok())
                .await
                .map_err(|e| TabLifecycleError::internal(format!("opencode probe join: {e}")))?;
        if !resolvable {
            return Err(TabLifecycleError::OpencodeNotFound);
        }
    }

    // Canonical add order: claude → claude-local → opencode so insertions
    // land in the right relative slot.
    let canonical = [AiTabId::Claude, AiTabId::ClaudeLocal, AiTabId::OpenCode];

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
    /// `settings.external_tools` when the user set one (Settings → Bottom
    /// bar), otherwise the default bare name (resolved ebin → PATH at spawn).
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

    // Drop the tab's typed-input echo buffer, mirroring `close_tab` — without
    // this, disabling an AI tab via the Settings checkbox leaves its
    // `user_input_buf` entry behind.
    if let Ok(mut buf) = state.user_input_buf.lock() {
        buf.remove(&tab);
    }

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
