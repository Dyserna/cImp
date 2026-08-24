//! Tab lifecycle as a service: create, close, rename, activate.
//!
//! ## What this module is for
//!
//! `ipc/tab_lifecycle.rs` carried seven near-copies of the same eleven-step
//! commit sequence — persist the entry to settings, insert into the registry,
//! send `TabAdded`, roll both back if the signal channel is closed, activate,
//! log. The copies had already drifted: one upserts by id where the others
//! push, one takes its position from settings where the others take the
//! registry's, and the rollback is present in four of them. That sequence is
//! [`TabService::commit_new_tab`] here, and "a new tab is committed like this"
//! became a thing the code says once (V42 #114's scope note names this helper
//! as the body of Phase A's tab-lifecycle work; Phase 0 is where its shape gets
//! proven).
//!
//! ## What it takes
//!
//! Borrowed handles, not `State<'_, AppState>`. Every collaborator here is
//! already UI-neutral — [`SettingsHandle`] is an `Arc` + a broadcast channel,
//! [`TabRegistryHandle`] is an `Arc<Mutex<_>>`, and `StateSignal` is an
//! in-process mpsc that the state manager (not Tauri) drains. The two that are
//! not are behind traits: [`WebviewHost`] for the Preview teardown and
//! [`EventSink`] for the one window-targeted emit.
//!
//! That is the finding the tab slice exists to produce: **tab lifecycle was
//! never coupled to Tauri events at all**. It was coupled to `AppState`, a
//! struct of twenty fields it uses six of, reachable only through a live app.
//! Borrowing the six is what makes the flow testable — see the tests at the
//! foot of this file, which drive create → rename → close with no WebView.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tracing::{info, warn};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::service::sink::{EventSink, EventSinkExt, WebviewHost};
use crate::settings::{
    Settings, SettingsHandle, ShellNotificationConfig, ShellTabConfig as ShellTabSettings,
    TabConfig,
};
use crate::state::{ReadOnlyTabs, StateSignal, TabId, TabKind, TabMeta};
use crate::tabs::TabRegistryHandle;

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
    /// Attempt to close an AI builtin.
    BuiltinNotClosable,
    /// `reconfigure_shell_tab` was called on a non-Shell tab.
    WrongKind,
    /// `set_enabled_ai_tabs` was called with an empty list. The UI's
    /// last-checked-is-locked rule prevents this from the user side; the
    /// IPC enforces it as defense-in-depth.
    EmptyAiTabsList,
    /// An attempt to enable a harness tab whose CLI cannot be resolved (not in
    /// `ebin`, not on PATH). The tab is left disabled and the UI surfaces
    /// `label` + `hint` so the user can install it first.
    ///
    /// **V40 Phase E (locked decision 26).** The probe and the exemption for
    /// the other harness used to be spelled in the IPC module; which harnesses
    /// are gated, and what a refusal advises, are `HarnessPlugin::preflight`'s
    /// answer now; core carries the refusal without knowing whose it is.
    HarnessNotFound {
        harness: String,
        label: String,
        hint: String,
    },
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
/// Frontend dialog sends raw strings; the service resolves the command,
/// splits args via `shlex`, and only then constructs this struct.
pub(crate) struct ValidatedShellInput {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
}

/// Validate the dialog inputs. Each filesystem probe (`which::which`,
/// `is_file`, `is_dir`) is synchronous and may stat slow paths (network
/// drives, antivirus-scanned directories), so the whole probe runs on a
/// blocking pool thread. The Tauri command wrapper is async; calling
/// this from there off the runtime keeps the tokio worker thread free
/// for other IPC work while a slow PATH walk is in flight.
pub(crate) async fn validate_inputs(
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

pub(crate) fn validated_to_shell_config(
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
pub(crate) fn notifications_from_dialog(
    error: String,
    exited: String,
) -> ShellNotificationConfig {
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
/// template's name with the lowest free integer ≥ 2 (e.g. "Shell" →
/// "Shell 2", "Shell 3"). Falls back to a uuid suffix in the
/// (practically impossible) event the first thousand are all taken.
pub(crate) fn unique_tab_name(settings: &Settings, base: &str) -> String {
    let taken: std::collections::HashSet<&str> = settings.tabs.iter().map(|t| t.name()).collect();
    for n in 2..1000 {
        let candidate = format!("{base} {n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    format!("{base} {}", Uuid::new_v4())
}

/// The tab-lifecycle use cases, over borrowed handles.
///
/// Borrowed rather than owned so constructing one at the top of an IPC command
/// is free — no `Arc` traffic on a path the user can drive at keyboard speed —
/// and so a test can build the handles on the stack and hand out references.
pub struct TabService<'a> {
    settings: &'a SettingsHandle,
    registry: &'a TabRegistryHandle,
    signals: &'a mpsc::Sender<StateSignal>,
    read_only: &'a ReadOnlyTabs,
    /// Serializes the lifecycle commands against each other. Held for the whole
    /// of every mutating method below, exactly as the commands held it.
    serializer: &'a TokioMutex<()>,
}

impl<'a> TabService<'a> {
    pub fn new(
        settings: &'a SettingsHandle,
        registry: &'a TabRegistryHandle,
        signals: &'a mpsc::Sender<StateSignal>,
        read_only: &'a ReadOnlyTabs,
        serializer: &'a TokioMutex<()>,
    ) -> Self {
        Self {
            settings,
            registry,
            signals,
            read_only,
            serializer,
        }
    }

    /// Commit a freshly-minted tab: persist, register, announce, activate.
    ///
    /// The ordering is load-bearing and was previously restated at every call
    /// site. Settings is written BEFORE the registry entry so that when the
    /// registry's `start_tab` path runs (post `TabAdded` → frontend mount →
    /// `pty_start`), `build_launch_spec` can find the entry; the broadcast that
    /// `mutate` triggers is also what the frontend's settings store consumes to
    /// reflect the new entry in the Tabs section. The write is an atomic
    /// `mutate` (not `current()` + `set()`) so a concurrent `save_layout` /
    /// `settings_update` / `set_active_tab` cannot clobber the new tab with a
    /// stale whole-struct snapshot; it is an upsert by id, so a retry cannot
    /// double-add.
    ///
    /// A closed signal channel means no view will ever exist for this tab, so
    /// both commits are rolled back — without that, a phantom tab persists and
    /// resurrects on next launch. `also` is the caller's extra settings edit
    /// (the Preview path remembers the URL); it is deliberately NOT rolled
    /// back, matching what the copies did.
    async fn commit_new_tab(
        &self,
        meta: &TabMeta,
        entry: TabConfig,
        also: impl FnOnce(&mut Settings) + Send,
        what: &str,
    ) -> Result<(), TabLifecycleError> {
        let tab = meta.id.clone();
        {
            let id = tab.as_str().to_string();
            self.settings.mutate(move |snap| {
                if let Some(existing) = snap.tabs.iter_mut().find(|t| t.id() == id) {
                    *existing = entry;
                } else {
                    snap.tabs.push(entry);
                }
                also(snap);
            });
        }

        let position = {
            let mut registry = self.registry.lock().await;
            registry.insert_user_tab(tab.clone(), meta.name.clone())
        };

        if let Err(e) = self
            .signals
            .send(StateSignal::TabAdded {
                meta: meta.clone(),
                position,
            })
            .await
        {
            warn!(error = %e, "{what}: state-signal channel closed");
            let id = tab.as_str().to_string();
            self.settings
                .mutate(move |snap| snap.tabs.retain(|t| t.id() != id));
            self.registry.lock().await.remove_tab(&tab).await;
            return Err(TabLifecycleError::internal("state signal channel closed"));
        }

        {
            let mut registry = self.registry.lock().await;
            if let Err(e) = registry.activate(tab).await {
                warn!(error = %e, "{what}: activate failed");
            }
        }
        Ok(())
    }

    /// Create a new user-managed Shell tab from the dialog's raw strings.
    ///
    /// The parameter list mirrors the dialog's `invoke` payload one-for-one:
    /// collapsing it into a struct here would only push the same seven fields
    /// up into the wrapper, since each one IS a payload field.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_shell(
        &self,
        name: String,
        command: String,
        args_string: String,
        cwd: Option<String>,
        env: HashMap<String, String>,
        notifications_error: String,
        notifications_exited: String,
    ) -> Result<TabId, TabLifecycleError> {
        let _serializer = self.serializer.lock().await;
        let args = shlex::split(&args_string).unwrap_or_else(|| {
            warn!(args = %args_string, "tab args have unbalanced quotes; treating as no args");
            Vec::new()
        });
        let validated = validate_inputs(name, command, args, cwd, env).await?;
        let notifications = notifications_from_dialog(notifications_error, notifications_exited);

        let tab = TabId::Shell(format!("shell-{}", Uuid::new_v4()));
        let meta = TabMeta {
            id: tab.clone(),
            kind: TabKind::Shell,
            name: validated.name.clone(),
        };
        let entry = TabConfig::Shell(validated_to_shell_config(
            tab.as_str().to_string(),
            false,
            &validated,
            notifications,
        ));

        self.commit_new_tab(&meta, entry, |_| {}, "create_shell_tab")
            .await?;
        info!(?tab, "shell tab created");
        Ok(tab)
    }

    /// Create a new user-managed Preview tab (the "+"-adjacent "New Preview
    /// tab" affordance — V14 Phase F). Unlike the Shell and AI paths there is
    /// no command/binary/cwd/env to validate — a Preview tab has no PTY at all,
    /// just a `url`/`device_width`/`auto_reload` triple the backend child
    /// webview (`crate::preview`) reads. An empty `url` falls back to
    /// `Settings::preview_last_url` (the last URL used by any Preview tab in
    /// this project), then `preview::DEFAULT_PREVIEW_URL`.
    pub async fn create_preview(&self, url: String) -> Result<TabId, TabLifecycleError> {
        let _serializer = self.serializer.lock().await;

        let snap = self.settings.current();
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
        let meta = TabMeta {
            id: tab.clone(),
            kind: TabKind::Preview,
            name: name.clone(),
        };
        let entry = TabConfig::Preview(crate::settings::PreviewTabConfig {
            id: tab.as_str().to_string(),
            builtin: false,
            name,
            url: resolved_url.clone(),
            device_width: None,
            auto_reload: false,
        });

        self.commit_new_tab(
            &meta,
            entry,
            move |s| s.preview_last_url = Some(resolved_url),
            "create_preview_tab",
        )
        .await?;
        info!(?tab, "preview tab created");
        Ok(tab)
    }

    /// Spawn a duplicate of an existing AI tab — the `+` affordance on an AI
    /// builtin. The new tab clones the *template's live config* (command, env,
    /// tts-injection, use_local_provider, theme/background overrides, …) so it
    /// behaves identically to the tab it came from, including local-provider
    /// env synthesis. It gets a fresh `"ai-<uuid>"` id, `builtin: false` (so
    /// it's closable and shows the `×`), and a unique auto-incremented name.
    /// Persisting it to `settings.tabs` means it survives a restart; the
    /// integrity check leaves non-reserved AI ids untouched.
    pub async fn create_ai(&self, template: TabId) -> Result<TabId, TabLifecycleError> {
        let _serializer = self.serializer.lock().await;

        // Clone the template's AI config. The `+` only appears on AI tabs, so
        // a missing entry or a Shell template is a malformed request.
        let mut cfg = {
            let snap = self.settings.current();
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
        let name = unique_tab_name(&self.settings.current(), &cfg.name);
        cfg.id = tab.as_str().to_string();
        cfg.builtin = false;
        cfg.name = name.clone();

        let meta = TabMeta {
            id: tab.clone(),
            kind: TabKind::AiTool,
            name,
        };

        self.commit_new_tab(&meta, TabConfig::AiTool(cfg), |_| {}, "create_ai_tab")
            .await?;
        info!(?tab, ?template, "ai tab duplicated");
        Ok(tab)
    }

    /// Close a user tab: drop its webview (Preview only), its persisted
    /// scrollback, its read-only row, its settings entry and its registry
    /// entry, and tell the state manager.
    ///
    /// `webviews` is a parameter rather than a field because this is the ONLY
    /// use case in the module that needs one — passing it to the constructor
    /// would make every caller of [`Self::activate`] name a webview host it has
    /// no business knowing about.
    pub async fn close(
        &self,
        tab: TabId,
        webviews: &dyn WebviewHost,
    ) -> Result<(), TabLifecycleError> {
        let _serializer = self.serializer.lock().await;
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
            let snap = self.settings.current();
            if let Some(entry) = snap.find_tab(tab.as_str()) {
                if entry.builtin() {
                    return Err(TabLifecycleError::BuiltinNotClosable);
                }
            }
        }

        // Existence check, active-switch, and removal all happen under ONE lock
        // acquisition. Splitting them lets a concurrent `tab_activate` /
        // `set_active_tab` (which take the registry but NOT the lifecycle
        // serializer) re-activate this tab in the gap between the switch and the
        // remove, leaving `active` dangling at a removed tab. Holding the lock
        // across the whole sequence closes that window.
        //
        // If we're closing the active tab, switch to its left neighbor first so
        // the frontend's active-tab indicator (and the TTS active cell) points
        // at a tab that still exists.
        let removed = {
            let mut registry = self.registry.lock().await;
            if !registry.has_tab(&tab) {
                return Err(TabLifecycleError::TabNotFound {
                    tab: tab.as_str().to_string(),
                });
            }
            if registry.active() == tab {
                // Prefer the left neighbor; if this is the leftmost tab (a
                // closable non-builtin can legitimately be at index 0 once AI
                // builtins are disabled), fall back to the right neighbor so
                // `active` never dangles at the just-removed tab.
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
        // `destroy_preview` is idempotent, so this is safe even if the
        // frontend's own cleanup also runs (whichever gets there first wins;
        // the other is a no-op).
        if tab.kind() == TabKind::Preview {
            webviews.destroy_preview(tab.as_str());
        }

        // V1.4-04 D.6: drop any persisted scrollback file for the closed
        // tab. The orphan-prune sweep at next launch would also catch it,
        // but cleaning up immediately keeps the disk-state consistent
        // with the user's mental model.
        if let Err(e) = crate::pty::scrollback::delete(&tab) {
            warn!(?tab, error = %e, "close_tab: scrollback delete failed");
        }

        // V39 Phase A: and its read-only row. The settings entry is dropped just
        // below, so the next broadcast would clear a `User` lock anyway; this also
        // drops a `Driven` row (which settings never describes) and keeps the map
        // from holding one entry per closed tab for the rest of the session.
        self.read_only.forget(&tab);
        // V39 Phase B: and tell any delegation in flight on this tab that its
        // worker is gone. It cannot be inferred from the state mirror — closing a
        // tab drops that row, so a closed tab reads exactly like an idle one — and
        // without it the driver would wait out its whole deadline on a tab that no
        // longer exists.
        crate::delegation::note_worker_gone(&tab);

        // Remove the settings entry. Drop the active_tab_id pointer if it
        // referenced this tab — the frontend will set a new one on its next
        // tab-switch event. Atomic mutate so a concurrent save_layout /
        // settings_update can't resurrect the just-closed tab from a stale
        // whole-struct snapshot.
        self.settings.mutate(|snap| {
            snap.tabs.retain(|t| t.id() != tab.as_str());
            if snap.session.active_tab_id.as_deref() == Some(tab.as_str()) {
                snap.session.active_tab_id = None;
            }
        });

        if let Err(e) = self
            .signals
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
    pub async fn rename(&self, tab: TabId, new_name: String) -> Result<(), TabLifecycleError> {
        let _serializer = self.serializer.lock().await;
        let trimmed = new_name.trim().to_string();
        if trimmed.is_empty() {
            return Err(TabLifecycleError::EmptyName);
        }
        {
            let mut registry = self.registry.lock().await;
            if !registry.has_tab(&tab) {
                return Err(TabLifecycleError::TabNotFound {
                    tab: tab.as_str().to_string(),
                });
            }
            registry.set_name(&tab, &trimmed);
        }
        self.settings.mutate(|snap| {
            if let Some(entry) = snap.find_tab_mut(tab.as_str()) {
                entry.set_name(trimmed.clone());
            }
        });
        if let Err(e) = self
            .signals
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

    /// Activate a tab without persisting the choice.
    ///
    /// Deliberately does NOT take the lifecycle serializer: activation is a
    /// keyboard-speed operation (Ctrl+1/Ctrl+2 held down) and the registry lock
    /// is the only thing it needs. That is how the command behaved and is why
    /// [`Self::close`] does its existence check, neighbour switch and removal
    /// under one lock acquisition.
    pub async fn activate(&self, tab: TabId) -> AppResult<()> {
        let mut registry = self.registry.lock().await;
        registry.activate(tab).await
    }

    /// Activate a tab AND persist its id as `session.active_tab_id`, so the
    /// user's last-active tab is restored on next launch. The settings write is
    /// debounced, so a fast Ctrl+1/Ctrl+2 burst doesn't hammer the disk.
    /// Every live tab in registry order — the tab strip's initial fill. Live
    /// changes arrive as `tab-created` / `tab-closed` events; this is what a
    /// view that mounts mid-session asks.
    pub async fn list(&self) -> Vec<crate::tabs::TabMetaWire> {
        self.registry.lock().await.list()
    }

    /// The compose overlay's non-empty edge.
    ///
    /// Compose targets the currently ACTIVE tab — its non-empty edge promotes
    /// that tab Idle→Listening and pins Listening while content remains. The
    /// active tab is resolved here rather than sent by the frontend for the
    /// reason every other command in this module resolves it here: the overlay
    /// has no tab of its own, and a stale id from a webview that has not yet
    /// seen an activation would move the wrong tab's avatar.
    pub async fn compose_content_changed(&self, non_empty: bool) {
        let tab = self.registry.lock().await.active();
        let _ = self
            .signals
            .try_send(StateSignal::ComposeContentChanged { tab, non_empty });
    }

    /// The user dismissed a tab's error badge.
    ///
    /// Best-effort like every other signal send here: a full or closed channel
    /// means the state manager is gone, and there is nothing useful to tell a
    /// user who just clicked an X.
    pub fn acknowledge_error(&self, tab: TabId) {
        let _ = self.signals.try_send(StateSignal::ErrorAcknowledged { tab });
    }

    pub async fn set_active(&self, tab: TabId) -> AppResult<()> {
        let id_string = tab.as_str().to_string();
        {
            let mut registry = self.registry.lock().await;
            registry.activate(tab).await?;
        }
        // Atomic read-modify-write so a concurrent close / settings_update
        // can't clobber this with a stale whole-struct snapshot (lost-update).
        // The outer `current()` check just skips the broadcast/save on a no-op
        // re-activation; the real write re-checks under the held lock.
        if self.settings.current().session.active_tab_id.as_deref() != Some(id_string.as_str()) {
            self.settings.mutate(move |snap| {
                snap.session.active_tab_id = Some(id_string);
            });
        }
        Ok(())
    }
}

/// The window label the terminal views live in.
const MAIN_WINDOW: &str = "main";

/// The event a Terminal component listens for to re-run its own `pty_restart`.
const TAB_RESTART_REQUESTED: &str = "tab-restart-requested";

/// Ask the main window to restart a tab's PTY.
///
/// The Terminal component for the targeted tab owns the channel and sizing —
/// it does the actual `pty_restart` invocation — so this is routed as a
/// frontend event rather than done here, keeping all PTY-touching IPC in one
/// window. `shell_only` is the closed-overlay path's extra guard (Phase 7's
/// Enter-to-restart affordance only exists on Shell tabs).
pub fn request_tab_restart(
    events: &dyn EventSink,
    tab: TabId,
    shell_only: bool,
) -> AppResult<()> {
    if shell_only && !matches!(tab.kind(), TabKind::Shell) {
        return Err(AppError::Ipc(format!(
            "restart_shell_tab: not a shell tab: {tab:?}"
        )));
    }
    events
        .emit_to_window(MAIN_WINDOW, TAB_RESTART_REQUESTED, &tab)
        .map_err(|e| AppError::Ipc(format!("emit restart: {e}")))?;
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
///
/// **The fourth commit variant** (locked decision 3). It is not a
/// [`TabService`] method yet: it commits a tab whose position comes from
/// SETTINGS rather than from the registry, which is the one difference
/// `commit_new_tab` has to absorb as a named parameter rather than inherit by
/// accident. It moved here from `ipc::tab_lifecycle` in the A1 settings run
/// because its only caller — the settings save — stopped having an `AppState`
/// to hand it; folding it into `commit_new_tab` is the tab-lifecycle run's
/// work, and doing it here would have been a behaviour change smuggled into a
/// mechanical wrap.
pub(crate) async fn sync_reserved_feature_tab(
    settings: &SettingsHandle,
    registry: &TabRegistryHandle,
    signals: &mpsc::Sender<StateSignal>,
    tab: TabId,
    enabled: bool,
) {
    if enabled {
        // Name + canonical position come from the (already-reconciled) settings.
        let snap = settings.current();
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
            let mut registry = registry.lock().await;
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
        if let Err(e) = signals.send(StateSignal::TabAdded { meta, position }).await {
            warn!(error = %e, ?tab, "sync_reserved_feature_tab: add signal channel closed");
            // Roll back the registry entry so it doesn't linger without a view.
            registry.lock().await.remove_tab(&tab).await;
        } else {
            info!(?tab, "reserved feature tab materialized (feature enabled)");
        }
    } else {
        let removed = {
            let mut registry = registry.lock().await;
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
            if let Err(e) = signals
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::sink::testing::{NoWebviews, RecordingEventSink};
    use crate::state::TabMeta;
    use crate::tabs::TabRegistry;
    use std::sync::{Arc, RwLock};
    use tokio::sync::Mutex as TokioMutex;

    /// Everything [`TabService`] borrows, owned on the stack.
    ///
    /// This is the whole cost of making the tab lifecycle headless: six
    /// handles, none of which needs a WebView, a window, or a Tauri `App`.
    /// Before the service split, the same coverage needed a running app and a
    /// human clicking — every one of these flows is on the live-verify list.
    struct Fixture {
        settings: SettingsHandle,
        registry: TabRegistryHandle,
        signals: mpsc::Sender<StateSignal>,
        rx: mpsc::Receiver<StateSignal>,
        read_only: ReadOnlyTabs,
        serializer: TokioMutex<()>,
        _scratch: ScratchDir,
    }

    /// A throwaway directory to point [`SettingsHandle`] at, so the debounced
    /// saver writes its `.cimp/config.json` somewhere disposable instead of
    /// into the real temp root (these tests DO mutate settings, unlike the
    /// existing `SettingsHandle` fixtures, which never trigger a save).
    ///
    /// Hand-rolled rather than a `tempfile` dev-dependency: one `Drop` is
    /// cheaper than a new crate in the lock file. Removal is best-effort — the
    /// saver task lives on Tauri's runtime and may land its write after the
    /// test's own runtime is gone.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cimp-tabsvc-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Fixture {
        /// One seed tab, marked builtin in settings so the "builtins are not
        /// closable" rule has something to refuse.
        fn new() -> Self {
            let scratch = ScratchDir::new();
            let mut defaults = Settings::default();
            let seed_id = TabId::Shell("shell-seed".to_string());
            defaults.tabs = vec![TabConfig::Shell(ShellTabSettings {
                id: seed_id.as_str().to_string(),
                builtin: true,
                name: "Seed".to_string(),
                command: "cmd".to_string(),
                args: Vec::new(),
                cwd: None,
                env: HashMap::new(),
                notifications: ShellNotificationConfig::default(),
                theme_override: None,
                background_override: None,
            })];
            let settings = SettingsHandle::new(defaults.clone(), defaults, scratch.0.clone());
            let (signals, rx) = mpsc::channel::<StateSignal>(64);
            let registry = Arc::new(TokioMutex::new(TabRegistry::new(
                vec![TabMeta {
                    id: seed_id.clone(),
                    kind: TabKind::Shell,
                    name: "Seed".to_string(),
                }],
                seed_id,
                Arc::new(RwLock::new(TabId::Shell("shell-seed".to_string()))),
                signals.clone(),
                Arc::new(Vec::new()),
            )));
            Self {
                settings,
                registry,
                signals,
                rx,
                read_only: ReadOnlyTabs::default(),
                serializer: TokioMutex::new(()),
                _scratch: scratch,
            }
        }

        fn service(&self) -> TabService<'_> {
            TabService::new(
                &self.settings,
                &self.registry,
                &self.signals,
                &self.read_only,
                &self.serializer,
            )
        }

        /// Drain the state-signal channel into a list of variant names, which
        /// is what the state manager turns into `tab-created` / `tab-renamed` /
        /// `tab-closed` on the wire.
        fn signal_names(&mut self) -> Vec<&'static str> {
            let mut out = Vec::new();
            while let Ok(sig) = self.rx.try_recv() {
                out.push(match sig {
                    StateSignal::TabAdded { .. } => "TabAdded",
                    StateSignal::TabRemoved { .. } => "TabRemoved",
                    StateSignal::TabRenameRequested { .. } => "TabRenameRequested",
                    StateSignal::TabActivated { .. } => "TabActivated",
                    _ => "other",
                });
            }
            out
        }

        fn tab_names(&self) -> Vec<String> {
            self.settings
                .current()
                .tabs
                .iter()
                .map(|t| t.name().to_string())
                .collect()
        }
    }

    /// A command every test can resolve: this test binary's own path, which is
    /// absolute and certainly a file, so `validate_inputs` accepts it on every
    /// platform without depending on what is installed.
    fn a_real_command() -> String {
        std::env::current_exe()
            .expect("current_exe")
            .to_string_lossy()
            .into_owned()
    }

    /// **Previously "user clicks in the app".** The live-verify recipe is: open
    /// the New Shell Tab dialog, create a tab, rename it from the tab bar's
    /// context menu, then close it with the `×`, and check the tab bar, the
    /// Settings → Tabs list and the persisted config after each step.
    ///
    /// Headless, that is one test — and it asserts the thing a human cannot
    /// see, the ORDER of the state signals the frontend's tab store reconciles
    /// from.
    #[tokio::test]
    async fn create_rename_close_round_trip() {
        let mut fx = Fixture::new();

        let tab = fx
            .service()
            .create_shell(
                "  Scratch  ".to_string(),
                a_real_command(),
                String::new(),
                None,
                HashMap::new(),
                "err".to_string(),
                "exit".to_string(),
            )
            .await
            .expect("create");

        // Name is trimmed, the entry is persisted, and the registry knows it.
        assert!(fx.tab_names().contains(&"Scratch".to_string()));
        assert!(fx.registry.lock().await.has_tab(&tab));
        assert_eq!(fx.registry.lock().await.active(), tab);

        fx.service()
            .rename(tab.clone(), "  Renamed  ".to_string())
            .await
            .expect("rename");
        assert!(fx.tab_names().contains(&"Renamed".to_string()));
        assert_eq!(
            fx.registry.lock().await.name_of(&tab).as_deref(),
            Some("Renamed")
        );

        fx.service().close(tab.clone(), &NoWebviews).await.expect("close");
        assert!(!fx.tab_names().contains(&"Renamed".to_string()));
        assert!(!fx.registry.lock().await.has_tab(&tab));
        // Focus fell back to the seed tab rather than dangling at the closed one.
        assert_eq!(
            fx.registry.lock().await.active(),
            TabId::Shell("shell-seed".to_string())
        );

        assert_eq!(
            fx.signal_names(),
            vec![
                "TabAdded",
                "TabActivated",
                "TabRenameRequested",
                "TabActivated",
                "TabRemoved",
            ],
            "the frontend's tab store reconciles from exactly this sequence"
        );
    }

    /// **Previously "user clicks in the app".** The recipe is: try to close a
    /// builtin (there is no `×`, so this is really "confirm the guard holds if
    /// something calls it anyway"), rename a tab to whitespace, and close a tab
    /// id that does not exist. All three are refusals the dialog renders inline
    /// off the error `kind`, and none of them was reachable from a test.
    #[tokio::test]
    async fn refusals_carry_the_wire_kind_the_dialog_matches_on() {
        let fx = Fixture::new();
        let seed = TabId::Shell("shell-seed".to_string());

        let err = fx.service().close(seed.clone(), &NoWebviews).await.unwrap_err();
        assert!(matches!(err, TabLifecycleError::BuiltinNotClosable));
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"kind":"builtin-not-closable"}"#
        );

        let err = fx
            .service()
            .rename(seed.clone(), "   ".to_string())
            .await
            .unwrap_err();
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"kind":"empty-name"}"#
        );

        let ghost = TabId::Shell("shell-nope".to_string());
        let err = fx.service().close(ghost, &NoWebviews).await.unwrap_err();
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"kind":"tab-not-found","tab":"shell-nope"}"#
        );

        // The refused close left the seed tab alone.
        assert!(fx.tab_names().contains(&"Seed".to_string()));
    }

    /// **Previously "user clicks in the app".** Two "New Preview tab" clicks:
    /// the first is plain "Preview", the second is "Preview 2", and the second
    /// one's empty URL inherits what the first one remembered.
    #[tokio::test]
    async fn preview_tabs_name_and_remember_their_url() {
        let fx = Fixture::new();

        let first = fx
            .service()
            .create_preview("http://localhost:4321/".to_string())
            .await
            .expect("first preview");
        let second = fx
            .service()
            .create_preview(String::new())
            .await
            .expect("second preview");

        assert!(fx.tab_names().contains(&"Preview".to_string()));
        assert!(fx.tab_names().contains(&"Preview 2".to_string()));

        let snap = fx.settings.current();
        assert_eq!(
            snap.preview_last_url.as_deref(),
            Some("http://localhost:4321/")
        );
        for tab in [&first, &second] {
            match snap.find_tab(tab.as_str()).expect("entry") {
                TabConfig::Preview(p) => assert_eq!(p.url, "http://localhost:4321/"),
                other => panic!("expected a preview entry, got {other:?}"),
            }
        }
    }

    /// **Previously "user clicks in the app".** Press Enter on a closed Shell
    /// tab's overlay: the backend emits `tab-restart-requested` at the main
    /// window and the Terminal component re-spawns. The guard that the same
    /// command refuses a non-Shell tab had no test at all.
    #[test]
    fn request_tab_restart_targets_the_main_window() {
        let events = RecordingEventSink::default();
        let shell = TabId::Shell("shell-1".to_string());

        request_tab_restart(&events, shell, true).expect("shell restart");
        let emitted = events.events();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].window.as_deref(), Some("main"));
        assert_eq!(emitted[0].event, "tab-restart-requested");
        assert_eq!(emitted[0].payload, r#""shell-1""#);

        let ai = TabId::Ai("ai-1".to_string());
        assert!(request_tab_restart(&events, ai.clone(), true).is_err());
        // ...and the un-guarded caller (the settings window's restart button)
        // is allowed to target one.
        assert!(request_tab_restart(&events, ai, false).is_ok());
    }
}
