//! Tab-lifecycle IPC commands for V3 Milestone 2: create / close / rename
//! / reconfigure user-managed Shell tabs. Each command returns a
//! `TabLifecycleError` with a serde-tagged shape so the frontend dialog
//! can render inline field errors keyed off the variant.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, State};
use tracing::{info, warn};
use uuid::Uuid;

use crate::ipc::AppState;
use crate::shell::{detect, ShellSpec};
use crate::state::{StateSignal, TabId, TabKind, TabMeta};
use crate::tabs::ShellTabConfig;

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
    /// Attempt to close a builtin (Claude / Aider). Builtins are pinned in
    /// V3; closing requires a different milestone.
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

/// Validated input for `create_shell_tab`. Frontend dialog sends raw
/// strings; backend resolves command, splits args via `shlex`, and only
/// then constructs this struct. Lives here rather than in the command
/// signature so callers (tests, future REST surface, etc.) can reuse the
/// validation pipeline.
struct ValidatedShellInput {
    name: String,
    spec: ShellSpec,
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
        spec: ShellSpec {
            command: resolved,
            args,
        },
        cwd: cwd_path,
        env,
    })
}

/// Create a new user-managed Shell tab. Validates the inputs, spawns a
/// fresh PTY, registers it with the registry + state manager, and emits
/// `TabCreated` so the frontend mirrors the addition into its tabs store.
#[tauri::command]
pub async fn create_shell_tab(
    app: AppHandle,
    state: State<'_, AppState>,
    name: String,
    command: String,
    args_string: String,
    cwd: Option<String>,
    env: HashMap<String, String>,
) -> Result<TabId, TabLifecycleError> {
    // Args arrive as a single string (the dialog has a single text input
    // for them, with shell-style quoting). Splitting here so the dialog
    // doesn't have to depend on a JS shell-lex implementation; failures
    // fall through to an empty arg vector, mirroring portable-pty's
    // tolerant parser.
    let args = shlex::split(&args_string).unwrap_or_default();
    let validated = validate_inputs(name, command, args, cwd, env)?;

    let tab = TabId::Shell(format!("shell-{}", Uuid::new_v4()));
    let tab_meta = TabMeta {
        id: tab.clone(),
        kind: TabKind::Shell,
        name: validated.name.clone(),
    };

    // Mutate the registry: install a fresh PtyManager and the shell
    // config, append to tab_order, capture the new position. We do NOT
    // spawn the PTY here — frontend's Terminal.svelte calls `pty_start`
    // on its first mount, same path as the launch-seed tabs use. The
    // `TabCreated` event triggers that mount.
    let position = {
        let mut registry = state.tabs.lock().await;
        registry.insert_user_shell_tab(
            tab.clone(),
            validated.name.clone(),
            ShellTabConfig {
                spec: validated.spec,
                cwd: validated.cwd,
                env: validated.env,
            },
        )
    };

    // Tell the state manager the new tab exists. It allocates a
    // `TabState` entry, an input-length counter, emits `TabCreated`
    // (frontend) + initial `StateChanged { Idle }`. Failure to send is a
    // hard internal error — we already mutated the registry, but
    // returning Err here lets the frontend retry; on a successful retry
    // the registry's idempotent `insert_user_shell_tab` will overwrite.
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

    // Activate the new tab so the frontend switches to it on next render.
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

/// Close a user-managed Shell tab. Builtins (Claude/Aider) reject. The
/// PTY is killed, the registry entry dropped, and `TabClosed` is emitted.
#[tauri::command]
pub async fn close_tab(
    state: State<'_, AppState>,
    tab: TabId,
) -> Result<(), TabLifecycleError> {
    if matches!(tab, TabId::Claude | TabId::Aider) {
        return Err(TabLifecycleError::BuiltinNotClosable);
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

    // Drop the PTY + registry entry. The PtyManager.shutdown() kills the
    // child if it's still running; the waiter task converts the kill into
    // a `SubprocessExited` signal but the state manager will discard it
    // (the tab no longer has a TabState by then) — the explicit
    // `TabRemoved` we send below is the canonical lifecycle event.
    let removed = {
        let mut registry = state.tabs.lock().await;
        registry.remove_user_shell_tab(&tab).await
    };
    if !removed {
        return Err(TabLifecycleError::TabNotFound {
            tab: tab.as_str().to_string(),
        });
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
) -> Result<(), TabLifecycleError> {
    if !matches!(tab.kind(), TabKind::Shell) {
        return Err(TabLifecycleError::WrongKind);
    }
    let args = shlex::split(&args_string).unwrap_or_default();
    let validated = validate_inputs(name, command, args, cwd, env)?;

    let name_changed: bool = {
        let mut registry = state.tabs.lock().await;
        if !registry.has_tab(&tab) {
            return Err(TabLifecycleError::TabNotFound {
                tab: tab.as_str().to_string(),
            });
        }
        let prev = registry.name_of(&tab);
        let changed = prev.as_deref() != Some(validated.name.as_str());
        registry.replace_shell_config(
            &tab,
            ShellTabConfig {
                spec: validated.spec.clone(),
                cwd: validated.cwd.clone(),
                env: validated.env.clone(),
            },
        );
        if changed {
            registry.set_name(&tab, &validated.name);
        }
        changed
    };

    if name_changed {
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
/// dialog calls this to populate the command + args defaults.
#[tauri::command]
pub fn default_shell_spec() -> DefaultShellWire {
    let (spec, _source) = detect::default_shell_resolution();
    DefaultShellWire {
        command: spec.command.to_string_lossy().into_owned(),
        args: spec.args.join(" "),
        git_bash_found: detect::was_default_git_bash_found(),
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
}

/// Wire-format snapshot of a Shell tab's current spawn config. Returned
/// by `get_shell_tab_config` so the Configure dialog can pre-fill from
/// the live registry state (rather than just the platform default).
#[derive(Debug, Serialize)]
pub struct ShellTabConfigWire {
    pub name: String,
    pub command: String,
    pub args: String,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
}

/// Look up the current Shell-tab config. Returns `WrongKind` for AI tabs
/// and `TabNotFound` for unknown ids.
#[tauri::command]
pub async fn get_shell_tab_config(
    state: State<'_, AppState>,
    tab: TabId,
) -> Result<ShellTabConfigWire, TabLifecycleError> {
    if !matches!(tab.kind(), TabKind::Shell) {
        return Err(TabLifecycleError::WrongKind);
    }
    let registry = state.tabs.lock().await;
    let cfg = registry
        .shell_config(&tab)
        .ok_or_else(|| TabLifecycleError::TabNotFound {
            tab: tab.as_str().to_string(),
        })?
        .clone();
    let name = registry
        .name_of(&tab)
        .unwrap_or_else(|| tab.as_str().to_string());
    Ok(ShellTabConfigWire {
        name,
        command: cfg.spec.command.to_string_lossy().into_owned(),
        args: cfg.spec.args.join(" "),
        cwd: cfg.cwd.map(|p| p.to_string_lossy().into_owned()),
        env: cfg.env,
    })
}

