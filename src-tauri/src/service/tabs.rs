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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tracing::{info, warn};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::service::sink::{EventSink, EventSinkExt, WebviewHost};
use crate::settings::{
    default_ai_tab, AiTabId, AiToolTabConfig, Settings, SettingsHandle, ShellNotificationConfig,
    ShellTabConfig as ShellTabSettings, TabConfig,
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

/// The reserved id of the singleton Note tab. It is an ordinary Shell-kind tab
/// on the backend (so it needs no dedicated `TabId` variant and is freely
/// closable), but it runs no PTY: the frontend keys off this id to render the
/// note editor instead of an xterm (see `tabs/types.ts`'s `isNoteTab`).
pub const NOTE_TAB_ID: &str = "note";

/// Wire-format tuple returned by `default_shell_spec`. Args are pre-joined with
/// spaces so the dialog can drop them straight into the args text input; the
/// dialog re-splits via `shlex` on submit (server-side, in
/// [`validate_inputs`]).
#[derive(Debug, Serialize)]
pub struct DefaultShellWire {
    pub command: String,
    pub args: String,
    pub git_bash_found: bool,
    pub notifications_error: String,
    pub notifications_exited: String,
}

/// Wire-format snapshot of a Shell tab's current spawn config, for the
/// Configure dialog's pre-fill. See [`TabService::shell_tab_config`].
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
    /// `(display name, command, args)` for the tool. The command is resolved at
    /// spawn time (drop-in `ebin/` first, then PATH — see `pty::resolve`),
    /// exactly like a user-typed Shell-tab command.
    pub(crate) fn spec(self) -> (&'static str, &'static str, &'static [&'static str]) {
        match self {
            ToolKind::Rustnet => ("rustnet", "rustnet", &[]),
            ToolKind::Broot => ("broot", "broot", &["-g", "-h"]),
        }
    }

    /// The launch command for this tool: an explicit per-tool path from
    /// `settings.external_tools` when the user set one (Settings → Bottom bar),
    /// otherwise the default bare name (resolved ebin → PATH at spawn).
    pub(crate) fn command_for(self, settings: &Settings) -> String {
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

/// Where a committed tab lands — the difference between the four copies of the
/// commit sequence this module absorbed (V42 Phase 0 decision 3).
///
/// A named parameter rather than an accident: the reserved AI builtins are the
/// one kind whose POSITION is a property of the tab rather than of where the
/// user was standing, and the two copies that did it that way looked, at a
/// glance, like the two that did not.
#[derive(Clone, Copy)]
pub(crate) enum Placement {
    /// A user tab: appended to the settings array, and registered beside the
    /// active tab.
    User,
    /// A reserved AI builtin: inserted at its canonical rank among the other
    /// reserved AI tabs, and registered as a builtin. Keeps the AI tabs in
    /// canonical leading order regardless of which subset is enabled.
    ///
    /// Does NOT activate. Ticking a checkbox in Settings → Tabs is not a
    /// request to go there — see the activation branch in
    /// [`TabService::commit_new_tab`].
    AiBuiltin(AiTabId),
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
    /// The project directory this app launched in — where the Note tab's
    /// backing file lives. The only path this module needs, and the reason it
    /// is a field rather than a parameter is that `open_note` is the only
    /// caller: a parameter would make every other method's caller name it.
    launch_cwd: &'a std::path::Path,
}

impl<'a> TabService<'a> {
    pub fn new(
        settings: &'a SettingsHandle,
        registry: &'a TabRegistryHandle,
        signals: &'a mpsc::Sender<StateSignal>,
        read_only: &'a ReadOnlyTabs,
        serializer: &'a TokioMutex<()>,
        launch_cwd: &'a std::path::Path,
    ) -> Self {
        Self {
            settings,
            registry,
            signals,
            read_only,
            serializer,
            launch_cwd,
        }
    }

    /// Commit a freshly-minted tab: persist, register, announce, and — for a
    /// user tab only — activate.
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
    ///
    /// `placement` is the fourth copy's difference, named (decision 3): a
    /// reserved AI builtin goes in at its canonical rank, registers as a
    /// builtin and is NOT activated; everything else appends, registers as a
    /// user tab and takes focus.
    async fn commit_new_tab(
        &self,
        meta: &TabMeta,
        entry: TabConfig,
        placement: Placement,
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
                    match placement {
                        Placement::User => snap.tabs.push(entry),
                        Placement::AiBuiltin(id) => {
                            // Walk the existing tabs and find the last
                            // reserved-AI tab whose canonical order is below
                            // this id's; insert immediately after it.
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
                            snap.tabs.insert(pos.min(snap.tabs.len()), entry);
                        }
                    }
                }
                also(snap);
            });
        }

        let position = {
            let mut registry = self.registry.lock().await;
            match placement {
                Placement::User => registry.insert_user_tab(tab.clone(), meta.name.clone()),
                Placement::AiBuiltin(_) => {
                    registry.insert_ai_builtin_tab(tab.clone(), meta.name.clone())
                }
            }
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

        // Activation is the USER half of this sequence, not part of "commit a
        // tab". A user tab is asked for by an act that means "take me there" —
        // the + menu, a tool button, a worktree — and all four copies this
        // helper absorbed activated. A reserved AI builtin is not: it appears
        // because a checkbox in Settings → Tabs was ticked, `add_ai_builtin_tab`
        // never activated it, and doing so both steals focus from the tab the
        // user is standing in and moves the `active` that `set_enabled_ai`'s
        // step 2 reads to decide whether the active tab is about to be closed.
        if matches!(placement, Placement::User) {
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

        self.commit_new_tab(&meta, entry, Placement::User, |_| {}, "create_shell_tab")
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
            Placement::User,
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

        self.commit_new_tab(
            &meta,
            TabConfig::AiTool(cfg),
            Placement::User,
            |_| {},
            "create_ai_tab",
        )
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
        //
        // ── V42 Phase A2, locked decision 6: **stays ambient, argued** ──────
        //
        // The criterion is the headless one: does injecting it buy a test
        // anything? Here it does not. `scrollback::delete` resolves
        // `<exe-dir>/scrollback/<tab>.bin` and no-ops when the file is absent,
        // and under `cargo test` the exe is the test binary — so a service
        // driven headlessly resolves a path under `target/`, finds nothing and
        // returns `Ok`. There is no state to observe and none to corrupt.
        // Wrapping it in a `ScrollbackStore` trait would add an interface with
        // one method and one implementation, in front of a fire-and-forget
        // cleanup whose only failure mode is already a `warn!`.
        //
        // What WOULD change the answer: a scrollback path that could resolve
        // into a real installation from a test, or a caller that needed to
        // assert the delete happened. Neither holds today.
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
        //
        // ── V42 Phase A2, locked decision 6: **stays ambient, argued** ──────
        //
        // The other half of the same decision, and it passes the criterion for
        // the opposite reason to the scrollback delete: this global is a pure
        // in-process registry whose effect IS test-visible — `delegation`'s own
        // tests call `note_worker_gone` and then read the flight back. An
        // injected handle would be a second way to reach a `OnceLock` that has
        // exactly one instance per process, and a test asserting through it
        // would be asserting about the injection rather than about the rule.
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

    /// Activate a tab AND persist its id as `session.active_tab_id`, so the
    /// user's last-active tab is restored on next launch. The settings write is
    /// debounced, so a fast Ctrl+1/Ctrl+2 burst doesn't hammer the disk.
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

    /// Duplicate `template`'s AI config, validated but not yet committed.
    ///
    /// Split from [`create_ai`](Self::create_ai) for the worktree variant,
    /// whose ordering rule is that the template is validated BEFORE a branch
    /// and a directory are created: this read has no side effects, so doing it
    /// first means a malformed request cannot orphan a worktree on disk.
    pub fn ai_template(&self, template: &TabId) -> Result<AiToolTabConfig, TabLifecycleError> {
        let snap = self.settings.current();
        let entry =
            snap.find_tab(template.as_str())
                .ok_or_else(|| TabLifecycleError::TabNotFound {
                    tab: template.as_str().to_string(),
                })?;
        match entry {
            TabConfig::AiTool(ai) => Ok(ai.clone()),
            TabConfig::Shell(_) | TabConfig::Preview(_) => Err(TabLifecycleError::WrongKind),
        }
    }

    /// Commit a duplicated AI config as a new closable tab named after
    /// `base_name` (uniqued), optionally rooted at `cwd`.
    ///
    /// Does NOT take the lifecycle serializer: the two callers hold it across a
    /// wider span than this — `create_ai` around its own template read, the
    /// worktree command around the `git worktree add` between the read and this
    /// commit — and taking it again here would deadlock.
    pub async fn commit_ai_duplicate(
        &self,
        mut cfg: AiToolTabConfig,
        base_name: &str,
        cwd: Option<PathBuf>,
        what: &str,
    ) -> Result<TabId, TabLifecycleError> {
        let tab = TabId::Ai(format!("ai-{}", Uuid::new_v4()));
        let name = unique_tab_name(&self.settings.current(), base_name);
        cfg.id = tab.as_str().to_string();
        cfg.builtin = false;
        cfg.name = name.clone();
        if cwd.is_some() {
            cfg.cwd = cwd;
        }

        let meta = TabMeta {
            id: tab.clone(),
            kind: TabKind::AiTool,
            name,
        };
        self.commit_new_tab(
            &meta,
            TabConfig::AiTool(cfg),
            Placement::User,
            |_| {},
            what,
        )
        .await?;
        Ok(tab)
    }

    /// Update a Shell tab's spawn config. Does NOT respawn — the new config
    /// takes effect on next restart.
    ///
    /// Three checks before anything is written, each with its own typed error:
    /// the id must be Shell-kind, the registry must know it, and settings must
    /// hold a Shell entry for it. The write is a `mutate` (not `current()` +
    /// `set()`) so a concurrent `save_layout` / `settings_update` cannot
    /// clobber the reconfigured entry, and its closure re-checks under the held
    /// lock so a tab that vanished meanwhile is a no-op rather than a
    /// resurrection.
    ///
    /// The rename half runs only when the name actually changed: it touches the
    /// registry AND sends `TabRenameRequested`, and doing both for an unchanged
    /// name would make every Configure-dialog OK look like a rename to every
    /// subscriber.
    ///
    /// Takes the lifecycle serializer for the whole body, like every other
    /// mutating method here and exactly as the command did before the fold.
    /// Without it, a `close_tab` interleaving between the "settings still holds
    /// a Shell entry for this tab" check and the `mutate` makes the closure's
    /// re-check find nothing and drop the edit on the floor — and the user is
    /// told `Ok`. The `which::which` probe inside `validate_inputs` runs under
    /// the lock deliberately: the alternative is validating outside it and
    /// re-checking inside, which buys a shorter hold on a path the user drives
    /// one dialog at a time, at the cost of a second copy of the checks. The
    /// probe is bounded (one PATH resolution) and lifecycle commands are not
    /// concurrent in practice, so the whole body stays inside.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconfigure_shell(
        &self,
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
        let _serializer = self.serializer.lock().await;
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
            let registry = self.registry.lock().await;
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

        match self.settings.current().find_tab(tab.as_str()) {
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
        self.settings.mutate(move |snap| {
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
                let mut registry = self.registry.lock().await;
                registry.set_name(&tab, &validated.name);
            }
            if let Err(e) = self
                .signals
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

    /// One Shell tab's current spawn config, as the Configure dialog pre-fills
    /// from it. `WrongKind` for AI/Preview tabs, `TabNotFound` for unknown ids.
    ///
    /// The args are joined with spaces here and re-split with `shlex` on
    /// submit, which is why the round trip is worth a test: a command argument
    /// containing a space has to survive being shown in a single text input.
    pub fn shell_tab_config(&self, tab: &TabId) -> Result<ShellTabConfigWire, TabLifecycleError> {
        if !matches!(tab.kind(), TabKind::Shell) {
            return Err(TabLifecycleError::WrongKind);
        }
        let snap = self.settings.current();
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

    /// Launch a built-in tool (rustnet / broot) into a fresh closable Shell tab
    /// (V16).
    ///
    /// Deliberately does **not** PATH-validate the command up front: a missing
    /// tool still spawns the tab, which then shows the standard "command not
    /// found" closed overlay. The tab is an ordinary `builtin: false` Shell tab
    /// with a uuid id, so repeated launches each get a fresh one, auto-numbered
    /// (`rustnet`, `rustnet 2`, …) to keep the tab bar legible.
    ///
    /// One settings snapshot drives both the collision-free display name and
    /// the launch command, so a concurrent write cannot have the name computed
    /// against one document and the command against another.
    pub async fn open_tool(&self, tool: ToolKind) -> Result<TabId, TabLifecycleError> {
        let _serializer = self.serializer.lock().await;
        let (base_name, _default_command, args) = tool.spec();

        let tab = TabId::Shell(format!("shell-{}", Uuid::new_v4()));
        let snap = self.settings.current();
        let command = tool.command_for(&snap);
        let name = {
            let taken = snap.tabs.iter().any(|t| t.name() == base_name);
            if taken {
                unique_tab_name(&snap, base_name)
            } else {
                base_name.to_string()
            }
        };

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
        let meta = TabMeta {
            id: tab.clone(),
            kind: TabKind::Shell,
            name,
        };
        self.commit_new_tab(&meta, entry, Placement::User, |_| {}, "open_tool_tab")
            .await?;
        info!(?tab, ?tool, "tool tab opened");
        Ok(tab)
    }

    /// Open the Note scratchpad tab — a singleton.
    ///
    /// An already-open note tab is re-activated rather than duplicated, and the
    /// backing file is created up front so the bottom-bar button "opens an
    /// existing note or creates one". A failed create is non-fatal: the tab
    /// still opens and `read_note` retries, because a scratchpad that refuses
    /// to open because its file is missing is worse than an empty one.
    ///
    /// The tab is Shell-kind with the reserved [`NOTE_TAB_ID`] id and an EMPTY
    /// command: it runs no PTY, and the frontend keys off the id to render the
    /// note editor instead of an xterm.
    pub async fn open_note(&self) -> Result<TabId, TabLifecycleError> {
        let _serializer = self.serializer.lock().await;
        let tab = TabId::Shell(NOTE_TAB_ID.to_string());

        if let Err(e) = crate::ipc::note::ensure_note_file(self.launch_cwd) {
            warn!(error = %e, "open_note_tab: ensure note file failed; opening tab anyway");
        }

        {
            let mut registry = self.registry.lock().await;
            if registry.has_tab(&tab) {
                if let Err(e) = registry.activate(tab.clone()).await {
                    warn!(error = %e, "open_note_tab: activate existing failed");
                }
                return Ok(tab);
            }
        }

        let name = "Note".to_string();
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
        let meta = TabMeta {
            id: tab.clone(),
            kind: TabKind::Shell,
            name,
        };
        self.commit_new_tab(&meta, entry, Placement::User, |_| {}, "open_note_tab")
            .await?;
        info!(?tab, "note tab opened");
        Ok(tab)
    }

    /// Apply a new `enabled_ai_tabs` value: open and close the AI builtin tabs
    /// so the live set matches the selection.
    ///
    /// **The order is the contract**, and every step of it exists because the
    /// obvious order is wrong somewhere:
    ///
    /// 1. Add the newly-enabled tabs FIRST, so step 2 has somewhere to go.
    /// 2. If the active tab is about to be removed, move active onto a
    ///    surviving tab — otherwise the frontend's active indicator (and the
    ///    TTS active cell) briefly points at a tab that no longer exists.
    /// 3. Only then remove the newly-disabled tabs.
    /// 4. Persist the setting last, beside the tabs array steps 1 and 3 left.
    ///
    /// Bypasses `BuiltinNotClosable` — this checkbox group is the canonical way
    /// to close an AI builtin, while a plain `close_tab` on one still refuses.
    /// An empty list is rejected: the UI's last-checked lock prevents it, and
    /// this is the defense in depth behind that.
    ///
    /// The preflight gate runs before ANY state changes, and only for tabs
    /// being turned on, so the toggle stays atomic — the UI reverts the
    /// checkbox and shows the harness's own reason.
    pub async fn set_enabled_ai(&self, value: Vec<AiTabId>) -> Result<(), TabLifecycleError> {
        if value.is_empty() {
            return Err(TabLifecycleError::EmptyAiTabsList);
        }

        // De-dup while preserving the user's intended order — defense against a
        // malformed IPC payload, since the UI cannot produce duplicates.
        let mut seen: HashSet<AiTabId> = HashSet::with_capacity(value.len());
        let want_ordered: Vec<AiTabId> = value.into_iter().filter(|id| seen.insert(*id)).collect();
        let want: HashSet<AiTabId> = want_ordered.iter().copied().collect();

        // Serialize with all other lifecycle commands so a concurrent
        // close_tab / create_shell_tab cannot interleave with the multi-step
        // add-then-remove sequence below.
        let _serializer = self.serializer.lock().await;

        let (have, prev_value) = {
            let snap = self.settings.current();
            let have: HashSet<AiTabId> = snap
                .tabs
                .iter()
                .filter_map(|t| AiTabId::from_id(t.id()))
                .collect();
            (have, snap.enabled_ai_tabs.clone())
        };
        let prev_set: HashSet<AiTabId> = prev_value.iter().copied().collect();
        if prev_set == want && have == want {
            return Ok(());
        }

        // **V40 Phase E, locked decision 26.** This used to be one hard-coded
        // `resolve_command("opencode")` with the other harness's exemption
        // stated in a comment — an exemption a third harness would have
        // inherited by accident. Each plugin answers `preflight()` for itself;
        // Claude's "not gated, it's the app's own front end" is a declared `Ok`.
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

        let canonical = crate::settings::canonical_ai_tab_order();

        for &id in &canonical {
            if want.contains(&id) && !have.contains(&id) {
                self.add_ai_builtin(id).await?;
            }
        }

        let surviving: Option<TabId> = canonical
            .iter()
            .find(|id| want.contains(id))
            .map(|id| TabId::from_str(id.as_str()));
        if let Some(target) = surviving {
            let active = {
                let registry = self.registry.lock().await;
                registry.active()
            };
            let about_to_close = AiTabId::from_id(active.as_str())
                .map(|id| have.contains(&id) && !want.contains(&id))
                .unwrap_or(false);
            if about_to_close && active != target {
                let mut registry = self.registry.lock().await;
                if let Err(e) = registry.activate(target).await {
                    warn!(error = %e, "set_enabled_ai_tabs: pre-close activate failed");
                }
            }
        }

        for &id in &canonical {
            if !want.contains(&id) && have.contains(&id) {
                self.remove_ai_builtin(TabId::from_str(id.as_str())).await?;
            }
        }

        if self.settings.current().enabled_ai_tabs != want_ordered {
            let v = want_ordered.clone();
            self.settings.mutate(move |snap| {
                snap.enabled_ai_tabs = v;
            });
        }

        info!(?want_ordered, "enabled_ai_tabs updated");
        Ok(())
    }

    /// Commit one reserved AI builtin, at its canonical position. See
    /// [`Placement::AiBuiltin`].
    async fn add_ai_builtin(&self, id: AiTabId) -> Result<(), TabLifecycleError> {
        let config = default_ai_tab(id);
        let tab = TabId::from_str(id.as_str());
        let meta = TabMeta {
            id: tab,
            kind: TabKind::AiTool,
            name: config.name().to_string(),
        };
        self.commit_new_tab(
            &meta,
            config,
            Placement::AiBuiltin(id),
            |_| {},
            "set_enabled_ai_tabs",
        )
        .await
    }

    /// Drop one reserved AI builtin: registry entry, persisted scrollback,
    /// read-only row, delegation flight, settings entry.
    ///
    /// A tab the registry did not have is a successful no-op — the checkbox and
    /// the live set can disagree after a settings file edit, and the user's
    /// click should still land.
    async fn remove_ai_builtin(&self, tab: TabId) -> Result<(), TabLifecycleError> {
        let removed = {
            let mut registry = self.registry.lock().await;
            registry.remove_tab(&tab).await
        };
        if !removed {
            return Ok(());
        }

        // Both process-globals below stay ambient — the argument for each is
        // recorded once, at `close`'s call sites (V42 Phase A2, decision 6).
        if let Err(e) = crate::pty::scrollback::delete(&tab) {
            warn!(?tab, error = %e, "set_enabled_ai_tabs: scrollback delete failed");
        }

        // V39 Phase A: same for its read-only row (mirrors `close`).
        self.read_only.forget(&tab);
        // V39 Phase B: …and the same delegation signal, for the same reason.
        crate::delegation::note_worker_gone(&tab);

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
            warn!(error = %e, "set_enabled_ai_tabs: state-signal channel closed (remove)");
            return Err(TabLifecycleError::internal("state signal channel closed"));
        }
        info!(?tab, "ai builtin tab removed");
        Ok(())
    }
}

/// The window label the terminal views live in.
const MAIN_WINDOW: &str = "main";

/// The event a Terminal component listens for to re-run its own `pty_restart`.
///
/// V42 F6 (#131): DEFINED FROM `service::events`, so the string is spelled
/// exactly once in the crate. The alias stays because this name is what the
/// module's own callers and tests read.
const TAB_RESTART_REQUESTED: &str = crate::service::events::TAB_RESTART_REQUESTED;

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

/// #152: ask the main window to restart an AI tab's harness process.
///
/// The mirror image of [`request_tab_restart`]'s `shell_only` guard, and a
/// separate entry point rather than a third value of that flag: the two
/// affordances are refusals in opposite directions, and a caller that could
/// pass either kind is a caller that has already lost the distinction. What it
/// emits is byte-identical to the settings-window path (`shell_only = false`),
/// so the Terminal component's restart handling is one path, not two.
pub fn request_ai_tab_restart(events: &dyn EventSink, tab: TabId) -> AppResult<()> {
    if !matches!(tab.kind(), TabKind::AiTool) {
        return Err(AppError::Ipc(format!(
            "restart_ai_tab: not an ai tool tab: {tab:?}"
        )));
    }
    request_tab_restart(events, tab, false)
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
    use crate::testutil::ScratchDir;
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

    impl Fixture {
        /// One seed tab, marked builtin in settings so the "builtins are not
        /// closable" rule has something to refuse.
        fn new() -> Self {
            let scratch = ScratchDir::new("tabsvc");
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
                &self._scratch.0,
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

    /// #152: the AI-side entry point refuses a Shell tab and emits exactly what
    /// the settings-window path emits for an AI one.
    ///
    /// The refusal direction is the half worth pinning: a guard written against
    /// the wrong kind still passes every AI-tab test while restarting shells
    /// from a button that does not exist for them.
    #[test]
    fn request_ai_tab_restart_refuses_a_shell_tab() {
        let events = RecordingEventSink::default();

        let shell = TabId::Shell("shell-1".to_string());
        assert!(request_ai_tab_restart(&events, shell).is_err());
        assert!(
            events.events().is_empty(),
            "a refused restart must emit nothing"
        );

        let ai = TabId::Ai("ai-1".to_string());
        request_ai_tab_restart(&events, ai).expect("ai restart");
        let emitted = events.events();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].window.as_deref(), Some("main"));
        assert_eq!(emitted[0].event, "tab-restart-requested");
        assert_eq!(emitted[0].payload, r#""ai-1""#);
    }

    /// **The Note tab is a singleton, and opening it twice re-activates it.**
    ///
    /// Previously "click the Note button twice and count the tabs". The second
    /// call must not add a second entry — the id is reserved, so a duplicate
    /// would give two tabs claiming the same backing file — and the settings
    /// array must still hold exactly one.
    #[tokio::test]
    async fn opening_the_note_tab_twice_re_activates_the_one_that_exists() {
        let f = Fixture::new();
        let first = f.service().open_note().await.expect("first open");
        assert_eq!(first.as_str(), NOTE_TAB_ID);
        let second = f.service().open_note().await.expect("second open");
        assert_eq!(first, second);

        let notes = f
            .settings
            .current()
            .tabs
            .iter()
            .filter(|t| t.id() == NOTE_TAB_ID)
            .count();
        assert_eq!(notes, 1, "the reserved id must not be duplicated");
    }

    /// **Tool tabs are numbered so the tab bar stays legible**, and each launch
    /// is a fresh closable tab rather than a re-activation — the opposite rule
    /// to the Note tab's, and the two live one method apart.
    #[tokio::test]
    async fn each_tool_launch_is_a_new_tab_with_a_collision_free_name() {
        let f = Fixture::new();
        let first = f.service().open_tool(ToolKind::Broot).await.expect("first");
        let second = f
            .service()
            .open_tool(ToolKind::Broot)
            .await
            .expect("second");
        assert_ne!(first, second, "each launch gets its own tab");

        let names: Vec<String> = f
            .settings
            .current()
            .tabs
            .iter()
            .filter(|t| t.id() == first.as_str() || t.id() == second.as_str())
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"broot".to_string()), "{names:?}");
        assert!(
            names.iter().any(|n| n != "broot"),
            "the second launch must not reuse the name: {names:?}"
        );
        // Not PATH-validated up front: a missing tool still gets a tab, which
        // then shows the standard "command not found" overlay.
        assert!(f.service().list().await.len() >= 3);
    }

    /// **An empty AI-tab selection is refused**, which is the defense in depth
    /// behind the Settings UI's last-checked lock — the user must keep at least
    /// one AI tab.
    #[tokio::test]
    async fn an_empty_ai_tab_selection_is_refused() {
        let f = Fixture::new();
        assert!(matches!(
            f.service().set_enabled_ai(Vec::new()).await,
            Err(TabLifecycleError::EmptyAiTabsList)
        ));
    }

    /// **A duplicated id in the payload does not open two tabs.**
    ///
    /// The UI cannot produce duplicates, so this is purely about a malformed
    /// IPC payload — and the de-dup preserves the caller's order, because that
    /// order is what gets persisted as `enabled_ai_tabs`.
    #[tokio::test]
    async fn a_duplicated_id_is_de_duplicated_in_the_callers_order() {
        let Some(id) = crate::settings::canonical_ai_tab_order().first().copied() else {
            return;
        };
        let f = Fixture::new();
        // Two of the same id: one tab, one entry in the persisted list.
        let _ = f.service().set_enabled_ai(vec![id, id]).await;
        assert_eq!(
            f.settings.current().enabled_ai_tabs,
            vec![id],
            "a duplicate must not survive into the persisted selection"
        );
        let entries = f
            .settings
            .current()
            .tabs
            .iter()
            .filter(|t| crate::settings::AiTabId::from_id(t.id()) == Some(id))
            .count();
        assert_eq!(entries, 1, "and it must not open the tab twice");
    }

    /// **Enabling an AI builtin never moves the user.**
    ///
    /// Ticking a harness in Settings → Tabs makes its tab exist; it does not
    /// mean "and take me there". `add_ai_builtin_tab` never activated, and when
    /// its body folded into [`TabService::commit_new_tab`] — whose other three
    /// callers all DO activate — it nearly inherited one. Two things break if
    /// it does: the user is yanked out of whatever tab they were in by a
    /// checkbox in another window, and `set_enabled_ai`'s step 2 then reads an
    /// `active` this very call moved, so its "is the active tab about to be
    /// closed?" question is asked about the wrong tab.
    #[tokio::test]
    async fn enabling_an_ai_builtin_never_changes_the_active_tab() {
        let Some(id) = crate::settings::canonical_ai_tab_order().first().copied() else {
            return;
        };
        let mut fx = Fixture::new();
        let seed = TabId::Shell("shell-seed".to_string());
        assert_eq!(fx.registry.lock().await.active(), seed);

        fx.service()
            .set_enabled_ai(vec![id])
            .await
            .expect("enable one AI builtin");

        let added = TabId::from_str(id.as_str());
        assert!(
            fx.registry.lock().await.has_tab(&added),
            "the builtin's tab must exist"
        );
        assert_eq!(
            fx.registry.lock().await.active(),
            seed,
            "a Settings checkbox must not steal focus"
        );
        assert_eq!(
            fx.signal_names(),
            vec!["TabAdded"],
            "the frontend must see the tab appear and nothing else"
        );
    }

    /// **A Shell tab's config round-trips through the Configure dialog's wire
    /// shape**, args included — they are joined with spaces here and re-split
    /// with `shlex` on submit, so an argument containing a space has to survive
    /// being shown in one text input.
    #[tokio::test]
    async fn a_shell_tabs_config_round_trips_through_the_dialogs_wire_shape() {
        let f = Fixture::new();
        let seed = TabId::Shell("shell-seed".to_string());

        let wire = f
            .service()
            .shell_tab_config(&seed)
            .expect("the seed tab is a Shell tab");
        assert_eq!(wire.name, "Seed");
        assert_eq!(wire.command, "cmd");

        // Wrong kind and unknown id are distinguishable errors, because the
        // dialog renders them differently.
        assert!(matches!(
            f.service().shell_tab_config(&TabId::from_str("claude")),
            Err(TabLifecycleError::WrongKind)
        ));
        assert!(matches!(
            f.service()
                .shell_tab_config(&TabId::Shell("shell-nope".to_string())),
            Err(TabLifecycleError::TabNotFound { .. })
        ));
    }

    /// **`reconfigure_shell` serializes against the other lifecycle commands.**
    ///
    /// The struct's documented invariant — "held for the whole of every
    /// mutating method" — is the reason a `close_tab` cannot land between this
    /// method's "settings still holds a Shell entry" check and its `mutate`,
    /// where the closure's re-check would silently drop the edit and still
    /// return `Ok`. Nothing else observes that hold, so it is pinned directly:
    /// while the serializer is held elsewhere the call must not make progress,
    /// and it must complete once the holder lets go.
    #[tokio::test]
    async fn reconfigure_shell_waits_on_the_lifecycle_serializer() {
        let fx = Fixture::new();
        let seed = TabId::Shell("shell-seed".to_string());
        let guard = fx.serializer.lock().await;

        let svc = fx.service();
        let call = svc.reconfigure_shell(
            seed.clone(),
            "Reconfigured".to_string(),
            a_real_command(),
            String::new(),
            None,
            HashMap::new(),
            "err".to_string(),
            "exit".to_string(),
            None,
            None,
        );
        tokio::pin!(call);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut call)
                .await
                .is_err(),
            "reconfigure_shell ran without the lifecycle serializer"
        );

        drop(guard);
        call.await.expect("reconfigure once the serializer is free");
        assert!(fx.tab_names().contains(&"Reconfigured".to_string()));
    }
}
