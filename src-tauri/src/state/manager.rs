use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Shared, runtime-mutable per-tab input-length counter map. The state
/// manager mutates it on TabAdded/TabRemoved; the IPC `pty_write` handler
/// reads a counter Arc per-write. `RwLock` rather than `DashMap` because
/// mutations are rare (tab create/close) while reads are also rare (one per
/// keystroke per tab); a plain RwLock is simpler than a third dependency.
pub type InputLengths = Arc<RwLock<HashMap<TabId, Arc<AtomicI32>>>>;

/// Identifier for one of the multi-tab subprocesses cimp owns. Four
/// reserved AI variants cover the V14 builtins (subscription / local
/// pairs for Claude Code and Aider); `Shell(id)` carries the
/// user-managed tab IDs introduced in v3 (M1 had a hardcoded "shell-1";
/// M2/M3 generalize). The runtime kind discriminator is [`TabKind`],
/// not this — `TabId` is purely an opaque identity used as HashMap key
/// and IPC payload.
///
/// Wire format: a single string. Reserved IDs serialize as `"claude"` /
/// `"claude-local"` / `"aider"` / `"aider-local"`; `Ai(s)` and `Shell(s)`
/// serialize as the inner string verbatim. Round-tripping a string that
/// starts with `"ai-"` yields an `Ai` variant; any other unrecognized
/// string yields a `Shell` variant.
#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub enum TabId {
    Claude,
    /// V1.4-07: Claude Code talking to a local LLM via the
    /// `claude_local` provider settings. Replaces the pre-V1.4-07 `Aider`
    /// variant; the v1.7 → v1.8 migration rewrites the aider tab to
    /// this id.
    ClaudeLocal,
    /// V19: the single OpenCode AI-tool tab — OpenCode picks its own
    /// provider/model, so (unlike Claude) there is no local variant. Replaces
    /// both the V14 `Aider` and `AiderLocal` variants.
    OpenCode,
    /// A user-spawned *duplicate* of one of the AI builtins (the `+` on
    /// a Claude/OpenCode tab). Carries a `"ai-<uuid>"` id and is a closable,
    /// non-builtin AI-kind tab. Its launch behavior (env synthesis,
    /// `--append-system-prompt`, etc.) is driven entirely by its
    /// `AiToolTabConfig` in settings — which is cloned from the template
    /// tab at spawn time — so this variant carries no template marker.
    Ai(String),
    Shell(String),
    /// V8-03: the read-only, non-closable Offload Server tab. Shell-kind
    /// (see [`Self::kind`]) but a distinct, reserved identity so it never
    /// collides with a user shell and the close guard can refuse it. Renders
    /// the local `llama-server`'s live output; spawns no PTY of its own.
    OffloadServer,
    /// V9-01: the read-only, non-closable Code Graph monitor tab. Like
    /// [`Self::OffloadServer`] it's Shell-kind with a reserved identity and no
    /// PTY, but it is app-rendered (an in-process dashboard of the graph
    /// indexer/embedder), not a mirror of a child process's output.
    GraphMonitor,
}

impl TabId {
    pub fn as_str(&self) -> &str {
        match self {
            TabId::Claude => "claude",
            TabId::ClaudeLocal => "claude-local",
            TabId::OpenCode => "opencode",
            TabId::OffloadServer => "offload-server",
            TabId::GraphMonitor => "graph-monitor",
            TabId::Ai(s) => s.as_str(),
            TabId::Shell(s) => s.as_str(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "claude" => TabId::Claude,
            "claude-local" => TabId::ClaudeLocal,
            "opencode" => TabId::OpenCode,
            "offload-server" => TabId::OffloadServer,
            "graph-monitor" => TabId::GraphMonitor,
            // Spawned AI-tab duplicates carry an `"ai-<uuid>"` id (see
            // `create_ai_tab`). They must round-trip back to `Ai`, not
            // `Shell`, so they keep AI-kind behavior on relaunch. The
            // reserved exact-matches above are checked first, and
            // `"opencode"` doesn't start with `"ai-"`, so
            // there's no collision.
            other if other.starts_with("ai-") => TabId::Ai(other.to_string()),
            other => TabId::Shell(other.to_string()),
        }
    }

    /// Pure mapping from id to runtime kind. Stable across milestones —
    /// every reserved AI variant and every spawned `Ai(_)` duplicate maps
    /// to `AiTool`; any `Shell(_)` id is a Shell tab. Lets call sites that
    /// don't carry `TabKind` explicitly (PTY processor, launch-spec
    /// builder) branch without threading a separate metadata table.
    pub fn kind(&self) -> TabKind {
        match self {
            TabId::Claude
            | TabId::ClaudeLocal
            | TabId::OpenCode
            | TabId::Ai(_) => TabKind::AiTool,
            // The Offload Server tab reuses Shell-kind for processing/state
            // purposes (it never runs a PTY, so this is inert), keeping it off
            // the per-kind match explosion. Its read-only behavior is keyed
            // off the reserved id, not the kind.
            TabId::Shell(_) | TabId::OffloadServer | TabId::GraphMonitor => TabKind::Shell,
        }
    }

    /// True for the reserved non-closable builtins: the four AI builtins
    /// (which `+` spawns duplicates of). Spawned `Ai(_)` duplicates and all
    /// `Shell(_)` tabs are closable, so they return false — including the
    /// on-demand `rustnet` / `broot` tool tabs, which are ordinary uuid-id
    /// Shell tabs. This is the canonical `builtin` flag surfaced to the
    /// frontend (gates the close `×`; the spawn `+` is additionally gated on
    /// AI-tool kind); keep it in sync with `tabs::registry`'s `is_builtin_id`.
    pub fn is_builtin(&self) -> bool {
        match self {
            TabId::Claude
            | TabId::ClaudeLocal
            | TabId::OpenCode
            // Non-closable: the Offload Server tab is removed only by
            // disabling offload, never by the close `×`. The Code Graph
            // monitor tab is likewise removed only by disabling the graph.
            | TabId::OffloadServer
            | TabId::GraphMonitor => true,
            TabId::Shell(_) | TabId::Ai(_) => false,
        }
    }
}

impl Serialize for TabId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TabId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(TabId::from_str(&s))
    }
}

/// Discriminator for which kind of subprocess a tab runs. Gates per-kind
/// behavior in the state machine, the processing layer, and the
/// notification system. V1.4-07 collapsed the AI inner discriminator
/// (Claude was the only remaining variant after Aider was dropped);
/// any future second AI tool would warrant re-introducing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabKind {
    AiTool,
    Shell,
}

/// Static metadata describing one tab at registration time. Plumbed into the
/// state manager and notification manager so per-kind behavior is decided
/// without round-tripping through settings on every signal.
#[derive(Clone, Debug)]
pub struct TabMeta {
    pub id: TabId,
    pub kind: TabKind,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    SubprocessExited,
    TtsError,
    AudioError,
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorInfo {
    pub tab: TabId,
    pub kind: ErrorKind,
    pub message: &'static str,
}

impl ErrorInfo {
    fn from_signal(s: &StateSignal) -> Option<Self> {
        let (kind, message) = match s {
            StateSignal::SubprocessExited { .. } => (ErrorKind::SubprocessExited, "Subprocess stopped."),
            StateSignal::TtsError { .. } => (ErrorKind::TtsError, "Text-to-speech is unavailable."),
            StateSignal::AudioError { .. } => (ErrorKind::AudioError, "Audio output is unavailable."),
            _ => return None,
        };
        Some(Self {
            tab: s.tab(),
            kind,
            message,
        })
    }
}

/// Auto-leave Listening when the input has been empty AND idle this long.
/// Same rule as v1, applied per-tab.
const EMPTY_INPUT_IDLE: Duration = Duration::from_secs(5);

/// Tick rate for the auto-leave-Listening sweep across all tabs.
const TICK: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum AvatarState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Error,
}

/// Signals consumed by the state machine. Every variant carries the tab it
/// originated from (or, for `TabActivated`, the tab that's becoming active).
/// Transition logic mirrors v1's per-state-machine rules; the manager just
/// runs them per tab and routes events back tagged with the same TabId.
///
/// Not `Copy` because `TabId::Shell` carries a `String`. The cost — one
/// small string clone per signal touch — is paid only on cross-thread sends
/// and the per-signal route step in the run loop, never in tight loops.
#[derive(Debug, Clone)]
pub enum StateSignal {
    UserKeystroke { tab: TabId },
    UserSubmit { tab: TabId },
    ClaudeOutputStarted { tab: TabId },
    ClaudeOutputStopped { tab: TabId },
    TtsPlaybackStarted { tab: TabId },
    TtsPlaybackStopped { tab: TabId },
    /// A selection-read (Ctrl+right-click) crossed a sentence boundary in
    /// playback. `index` is the chunk now starting to play; `index ==
    /// chunk_count` is the end-of-session sentinel. Pure pass-through — the
    /// state machine does not mutate any `TabState`, it just re-emits this as
    /// `StateEvent::TtsSelectionProgress` so the frontend can advance the
    /// read-along highlight. `session` lets the frontend ignore stale reads.
    TtsSelectionProgress { tab: TabId, session: u64, index: u32 },
    /// Subprocess for `tab` exited with the given exit code (`None` if the
    /// child wait returned an error, or if we synthesize the signal from a
    /// spawn-time failure where there is no process to exit). Phase 4 routes
    /// this per-kind: AI tabs go to Error, Shell tabs go to the closed
    /// sub-state with the code surfaced in the overlay.
    SubprocessExited { tab: TabId, code: Option<i32> },
    AudioError { tab: TabId },
    TtsError { tab: TabId },
    ErrorAcknowledged { tab: TabId },
    /// Compose-overlay textarea content crossed the empty/non-empty edge.
    /// Always routed to the active tab (compose targets whoever is on
    /// screen).
    ComposeContentChanged { tab: TabId, non_empty: bool },
    /// User activated a tab (click or Ctrl+N). Updates `active` and
    /// broadcasts so the frontend can swap avatar/terminal visuals.
    TabActivated { tab: TabId },
    /// Permission detector saw a known prompt pattern in the rendered tail.
    /// Sets `awaiting_permission` on the tab; does NOT drive the avatar
    /// state machine.
    PermissionPromptDetected { tab: TabId },
    /// Permission detector observed the previously-matched pattern leave the
    /// rendered tail. Clears `awaiting_permission`.
    PermissionPromptResolved { tab: TabId },
    /// Detector saw a question-pattern match in the rendered tail (e.g.
    /// Claude Code's AskUserQuestion multi-option prompt). Sets
    /// `awaiting_question` on the tab; mirrors the permission path but
    /// drives a separate notification template.
    QuestionPromptDetected { tab: TabId },
    /// Detector observed the previously-matched question pattern leave the
    /// rendered tail. Clears `awaiting_question`.
    QuestionPromptResolved { tab: TabId },
    /// A Shell tab's subprocess has been (re)spawned after a previous exit.
    /// Clears the `closed` flag and emits `TabClosedStateChanged { closed:
    /// false }`. AI tabs don't use this — they have no closed sub-state.
    ShellRestarted { tab: TabId },
    /// A new tab has been registered with the runtime (M2's
    /// `create_shell_tab`). The state manager allocates a `TabState`
    /// entry, an input-length counter, and emits `StateEvent::TabCreated`
    /// so the frontend mirrors the addition into its tabs store.
    TabAdded { meta: TabMeta, position: usize },
    /// A tab has been removed from the runtime (M2's `close_tab`). The
    /// state manager drops its `TabState`, drops the input-length counter,
    /// and emits `StateEvent::TabClosed`.
    TabRemoved { tab: TabId },
    /// A tab's display name was changed (M2's `rename_tab` /
    /// `reconfigure_shell_tab`). The state manager updates its name and
    /// emits `StateEvent::TabRenamed`.
    TabRenameRequested { tab: TabId, name: String },
    /// A Shell tab's spawn failed at launch in a way that is not a runtime
    /// crash — typically the configured command no longer resolves on PATH
    /// or its file no longer exists. Routes the tab to the closed sub-
    /// state with a custom message that the frontend overlay shows in
    /// place of "Shell exited (code N)". M3 of v3 fires this from the
    /// registry's start path when `build_launch_spec` returns a
    /// `CommandNotFound`.
    ShellLaunchFailed { tab: TabId, message: String },
}

impl StateSignal {
    pub fn tab(&self) -> TabId {
        match self {
            Self::UserKeystroke { tab }
            | Self::UserSubmit { tab }
            | Self::ClaudeOutputStarted { tab }
            | Self::ClaudeOutputStopped { tab }
            | Self::TtsPlaybackStarted { tab }
            | Self::TtsPlaybackStopped { tab }
            | Self::TtsSelectionProgress { tab, .. }
            | Self::SubprocessExited { tab, .. }
            | Self::AudioError { tab }
            | Self::TtsError { tab }
            | Self::ErrorAcknowledged { tab }
            | Self::ComposeContentChanged { tab, .. }
            | Self::TabActivated { tab }
            | Self::PermissionPromptDetected { tab }
            | Self::PermissionPromptResolved { tab }
            | Self::QuestionPromptDetected { tab }
            | Self::QuestionPromptResolved { tab }
            | Self::ShellRestarted { tab }
            | Self::TabRemoved { tab }
            | Self::TabRenameRequested { tab, .. }
            | Self::ShellLaunchFailed { tab, .. } => tab.clone(),
            Self::TabAdded { meta, .. } => meta.id.clone(),
        }
    }
}

/// Frontend-facing events emitted via the Tauri AppHandle. Kept distinct from
/// the input `StateSignal` so the wire format can evolve without disturbing
/// the internal signal vocabulary.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
// Variant names are the IPC wire format (kebab-case after serde); renaming
// to satisfy the lint would break the frontend contract.
#[allow(clippy::enum_variant_names)]
pub enum StateEvent {
    StateChanged { tab: TabId, state: AvatarState },
    ActiveTabChanged { tab: TabId },
    /// Read-along progress for a Ctrl+right-click selection read. `index` is
    /// the sentence chunk now beginning playback; `index == chunk_count`
    /// signals the whole selection finished. Wire tag: `tts-selection-progress`.
    TtsSelectionProgress { tab: TabId, session: u64, index: u32 },
    AwaitingPermissionChanged { tab: TabId, awaiting: bool },
    AwaitingQuestionChanged { tab: TabId, awaiting: bool },
    DoneWhileAwayChanged { tab: TabId, done: bool },
    /// Shell tab's `closed` UI flag flipped. `closed: true` is fired when
    /// the subprocess exits; `closed: false` when the user restarts it.
    /// `exit_code` is `None` for spawn-time failures or for `closed: false`
    /// events. `closed_message` is `Some` only for command-not-found-style
    /// launch failures — the frontend overlay shows it in place of the
    /// standard "Shell exited (code N)" line and routes Enter to the
    /// Configure dialog instead of restart.
    TabClosedStateChanged {
        tab: TabId,
        closed: bool,
        exit_code: Option<i32>,
        closed_message: Option<String>,
    },
    /// A new tab was added to the runtime. Frontend appends to its tabs
    /// store; notification manager seeds its per-tab caches. `position` is
    /// the tab's index in the live tab order. `builtin: false` for every
    /// runtime-added tab (only the launch seed contains builtins, and they
    /// are emitted via this event during startup-replay too).
    TabCreated {
        tab: TabId,
        kind: TabKindWire,
        name: String,
        builtin: bool,
        position: usize,
    },
    /// A tab was removed from the runtime. Frontend drops it from the tabs
    /// store; per-tab cached state (avatar, error, closed-state) is also
    /// dropped on this edge.
    TabClosed { tab: TabId },
    /// A tab's display name was updated. Triggered by both `rename_tab` and
    /// `reconfigure_shell_tab` when the latter's `name` field changed.
    TabRenamed { tab: TabId, name: String },
}

/// Wire-format projection of `TabKind` for the `TabCreated` event. The
/// frontend only needs to know whether a tab is a Shell or an AI tool to
/// gate close-button rendering and similar UI affordances; matching the
/// internal `TabKind` shape one-to-one would have leaked the (now
/// removed) `AiToolKind` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabKindWire {
    AiTool,
    Shell,
}

impl From<&TabKind> for TabKindWire {
    fn from(k: &TabKind) -> Self {
        match k {
            TabKind::AiTool => TabKindWire::AiTool,
            TabKind::Shell => TabKindWire::Shell,
        }
    }
}

#[derive(Clone, Debug)]
struct TabState {
    kind: TabKind,
    name: String,
    avatar_state: AvatarState,
    has_unsent_input: bool,
    composing: bool,
    last_keystroke_at: Option<Instant>,
    /// Set by permission detection; cleared on detector-resolve, user input,
    /// or tab activation triggering Claude to emit further output. Always
    /// false for Shell tabs (the detector is a no-op for them).
    awaiting_permission: bool,
    /// Mirrors `awaiting_permission` but for AskUserQuestion-style prompts
    /// detected by `kind: question` patterns. Independent of the
    /// permission flag — a tab could in principle be in both at once.
    awaiting_question: bool,
    /// UI-derived: tab transitioned to Idle while inactive. Cleared on
    /// activation. Independent of `avatar_state` and `awaiting_permission`.
    done_while_away: bool,
    /// Shell-only: subprocess has exited and is awaiting user-initiated
    /// restart. Stays false on AI tabs (their exit path goes to Error).
    closed: bool,
    closed_exit_code: Option<i32>,
    /// Shell-only: a non-runtime spawn failure (currently command-not-
    /// found at launch). When set, the frontend overlay renders this
    /// message instead of the standard "Shell exited (code N)" line, and
    /// Enter routes to the Configure dialog instead of restart. Cleared
    /// on `ShellRestarted`.
    closed_message: Option<String>,
    /// Set true between `ClaudeOutputStarted` and `ClaudeOutputStopped`.
    /// Lets `Speaking → TtsPlaybackStopped` fall back to Thinking instead
    /// of Idle when Claude is still emitting output (the TTS tag was a
    /// commentary tag, not a final answer). Always false for Shell tabs.
    claude_output_active: bool,
}

impl TabState {
    fn new(kind: TabKind, name: String) -> Self {
        Self {
            kind,
            name,
            avatar_state: AvatarState::Idle,
            has_unsent_input: false,
            composing: false,
            last_keystroke_at: None,
            awaiting_permission: false,
            awaiting_question: false,
            done_while_away: false,
            closed: false,
            closed_exit_code: None,
            closed_message: None,
            claude_output_active: false,
        }
    }
}

/// Spawn the state-manager task. The channel is created at app startup so
/// AppState can hold a clone of the sender before the AppHandle exists.
///
/// `state_events` receives the same `StateEvent`s emitted to the frontend,
/// so in-process subscribers (e.g. the notification manager) can react to
/// state edges without going through the IPC layer.
///
/// `tab_metas` defines every tab the manager tracks (kind + name); the
/// manager keys its per-tab state map by `TabId` from this list.
pub fn spawn_state_manager(
    app: AppHandle,
    rx: mpsc::Receiver<StateSignal>,
    state_events: broadcast::Sender<StateEvent>,
    input_lengths: InputLengths,
    tab_metas: Vec<TabMeta>,
    initial_active: TabId,
    ai_tts_suppressed: crate::tts::AiTtsSuppressed,
) {
    tauri::async_runtime::spawn(async move {
        run(
            app,
            rx,
            state_events,
            input_lengths,
            tab_metas,
            initial_active,
            ai_tts_suppressed,
        )
        .await;
    });
}

async fn run(
    app: AppHandle,
    mut rx: mpsc::Receiver<StateSignal>,
    state_events: broadcast::Sender<StateEvent>,
    input_lengths: InputLengths,
    tab_metas: Vec<TabMeta>,
    initial_active: TabId,
    ai_tts_suppressed: crate::tts::AiTtsSuppressed,
) {
    // Preserve tab_metas order so the startup TabCreated emit positions
    // match the registry's tab order (registry uses the same launch_seed).
    let seed_metas: Vec<TabMeta> = tab_metas;
    let mut tabs: HashMap<TabId, TabState> = seed_metas
        .iter()
        .cloned()
        .map(|m| (m.id, TabState::new(m.kind, m.name)))
        .collect();
    let mut active = initial_active;

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Emit the initial Idle for each tab so the frontend has a baseline before
    // any signal arrives. The avatar component skips its first-render
    // transition so this doesn't play an unwanted animation. We also emit a
    // `TabCreated` for each seed tab so the frontend's tabs store has one
    // event-driven source of truth — no static frontend list needs to mirror
    // the backend's launch seed.
    for (position, meta) in seed_metas.iter().enumerate() {
        emit_tab_created(
            &app,
            &state_events,
            meta.id.clone(),
            (&meta.kind).into(),
            meta.name.clone(),
            meta.id.is_builtin(),
            position,
        );
    }
    for (tab, ts) in &tabs {
        emit_state(&app, &state_events, tab.clone(), ts.avatar_state);
    }
    emit_active_tab(&app, &state_events, active.clone());

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(signal) = maybe else { break };

                // Runtime tab lifecycle (TabAdded / TabRemoved /
                // TabRenameRequested) is handled before the per-tab
                // transition routing because (a) the target TabState may
                // not exist yet (TabAdded) or any longer (TabRemoved), and
                // (b) the frontend needs the events emitted regardless of
                // any avatar-state side effects. The registry computes the
                // `position` field; we just relay it.
                if let StateSignal::TabAdded { meta, position } = &signal {
                    let meta = meta.clone();
                    let position = *position;
                    if !tabs.contains_key(&meta.id) {
                        tabs.insert(
                            meta.id.clone(),
                            TabState::new(meta.kind.clone(), meta.name.clone()),
                        );
                        if let Ok(mut g) = input_lengths.write() {
                            g.entry(meta.id.clone())
                                .or_insert_with(|| Arc::new(AtomicI32::new(0)));
                        }
                        info!(tab = ?meta.id, position, "tab added");
                        emit_state(&app, &state_events, meta.id.clone(), AvatarState::Idle);
                        emit_tab_created(
                            &app,
                            &state_events,
                            meta.id.clone(),
                            (&meta.kind).into(),
                            meta.name,
                            meta.id.is_builtin(),
                            position,
                        );
                    }
                    continue;
                }
                if let StateSignal::TabRemoved { tab } = &signal {
                    let tab = tab.clone();
                    if tabs.remove(&tab).is_some() {
                        if let Ok(mut g) = input_lengths.write() {
                            g.remove(&tab);
                        }
                        // If the active tab was just removed, repoint `active` at
                        // a surviving tab. Leaving it on the dead id breaks the
                        // idle sweep's `*tab != active` checks, marking every
                        // survivor done-while-away (a spurious badge). The
                        // frontend's follow-up TabActivated sets the real one.
                        if active == tab {
                            if let Some(next) = tabs.keys().next().cloned() {
                                active = next;
                            }
                        }
                        info!(?tab, "tab removed");
                        emit_tab_closed_event(&app, &state_events, tab);
                    }
                    continue;
                }
                if let StateSignal::TabRenameRequested { tab, name } = &signal {
                    let tab = tab.clone();
                    let name = name.clone();
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if ts.name != name {
                            ts.name = name.clone();
                            info!(?tab, name = %name, "tab renamed");
                            emit_tab_renamed(&app, &state_events, tab, name);
                        }
                    }
                    continue;
                }

                // TabActivated isn't a per-tab transition — it just moves the
                // active pointer and re-broadcasts. We DON'T re-emit the new
                // tab's state here; the frontend listens for ActiveTabChanged
                // and re-derives from the per-tab cache it already has.
                if let StateSignal::TabActivated { tab } = &signal {
                    let tab = tab.clone();
                    // Never point `active` at a tab we don't know about: a stray
                    // or out-of-order activation would leave `active` dangling
                    // at a non-existent tab, breaking the idle sweep's
                    // `*tab != active` checks and done-while-away routing.
                    // `TabAdded` is always enqueued before its `TabActivated`,
                    // so a legitimate activation always finds the tab present.
                    if !tabs.contains_key(&tab) {
                        debug!(?tab, "ignoring TabActivated for unknown tab");
                        continue;
                    }
                    if active != tab {
                        info!(from = ?active, to = ?tab, "active tab");
                        active = tab.clone();
                        emit_active_tab(&app, &state_events, tab.clone());
                        // Clear DoneWhileAway on the newly-active tab — the
                        // user's now looking at it, so the "you missed
                        // something" hint has served its purpose.
                        if let Some(ts) = tabs.get_mut(&tab) {
                            if ts.done_while_away {
                                ts.done_while_away = false;
                                emit_done_while_away(&app, &state_events, tab, false);
                            }
                        }
                    }
                    continue;
                }

                // New Claude output clears the Esc-driven AI-TTS suppression:
                // the user stopped the *previous* burst's tagged speech, but a
                // fresh burst should speak again. Done as a peek (no `continue`)
                // so the signal still drives the avatar transition below.
                //
                // Only the ACTIVE tab's fresh output clears it: the suppression
                // is global (one voice), but it was armed against the tab the
                // user Esc-silenced while looking at it. Clearing on ANY tab's
                // output would let a background tab's output un-silence that tab.
                if let StateSignal::ClaudeOutputStarted { tab } = &signal {
                    if *tab == active {
                        ai_tts_suppressed.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                }

                // Selection-read progress is a pure pass-through to the
                // frontend — it carries no avatar-state meaning, so we relay
                // it as an event and skip the per-tab transition routing.
                if let StateSignal::TtsSelectionProgress { tab, session, index } = &signal {
                    dispatch(
                        &app,
                        &state_events,
                        StateEvent::TtsSelectionProgress {
                            tab: tab.clone(),
                            session: *session,
                            index: *index,
                        },
                    );
                    continue;
                }

                // Permission-prompt edges are independent of the avatar state
                // machine — they only flip `awaiting_permission`. Resolved
                // and user-input both clear; the input clearing path below
                // handles UserKeystroke / UserSubmit.
                if let StateSignal::PermissionPromptDetected { tab } = &signal {
                    let tab = tab.clone();
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if !ts.awaiting_permission {
                            ts.awaiting_permission = true;
                            info!(?tab, "awaiting permission: set");
                            emit_awaiting_permission(&app, &state_events, tab, true);
                        }
                    }
                    continue;
                }
                if let StateSignal::PermissionPromptResolved { tab } = &signal {
                    let tab = tab.clone();
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if ts.awaiting_permission {
                            ts.awaiting_permission = false;
                            info!(?tab, "awaiting permission: cleared (resolved)");
                            emit_awaiting_permission(&app, &state_events, tab, false);
                        }
                    }
                    continue;
                }
                if let StateSignal::QuestionPromptDetected { tab } = &signal {
                    let tab = tab.clone();
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if !ts.awaiting_question {
                            ts.awaiting_question = true;
                            info!(?tab, "awaiting question: set");
                            emit_awaiting_question(&app, &state_events, tab, true);
                        }
                    }
                    continue;
                }
                if let StateSignal::QuestionPromptResolved { tab } = &signal {
                    let tab = tab.clone();
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if ts.awaiting_question {
                            ts.awaiting_question = false;
                            info!(?tab, "awaiting question: cleared (resolved)");
                            emit_awaiting_question(&app, &state_events, tab, false);
                        }
                    }
                    continue;
                }

                // Shell tabs route SubprocessExited to the closed sub-state
                // instead of Error (per DESIGN.md § "Shell-Tab Closed
                // Sub-State"). AI tabs fall through to the
                // generic transition path below where the existing v1 logic
                // turns the signal into Error. Spawn-time failures with
                // `code = None` still hit this same branch.
                if let StateSignal::SubprocessExited { tab, code } = &signal {
                    let tab = tab.clone();
                    let code = *code;
                    let route_to_closed = tabs
                        .get(&tab)
                        .map(|ts| matches!(ts.kind, TabKind::Shell))
                        .unwrap_or(false);
                    if route_to_closed {
                        if let Some(ts) = tabs.get_mut(&tab) {
                            // A SubprocessExited landing on a tab that already
                            // has a closed_message (from ShellLaunchFailed)
                            // means the same launch failure is bubbling up
                            // twice — preserve the message so the user still
                            // sees "command not found" rather than the
                            // generic "exited" overlay.
                            if !ts.closed {
                                ts.closed = true;
                                ts.closed_exit_code = code;
                                let msg = ts.closed_message.clone();
                                info!(?tab, ?code, "shell tab: closed");
                                emit_tab_closed_state(&app, &state_events, tab, true, code, msg);
                            }
                        }
                        continue;
                    }
                    // AI tab: fall through; the generic routing below feeds
                    // the signal into transition() which produces Error.
                }

                // Shell tab launch-failure: spawn-time error that should NOT
                // be retried by Enter (e.g. command not found). Routes to
                // the closed sub-state and stamps a custom message that the
                // frontend overlay displays in place of the standard text.
                if let StateSignal::ShellLaunchFailed { tab, message } = &signal {
                    let tab = tab.clone();
                    let message = message.clone();
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if matches!(ts.kind, TabKind::Shell) {
                            ts.closed = true;
                            ts.closed_exit_code = None;
                            ts.closed_message = Some(message.clone());
                            info!(?tab, message = %message, "shell tab: launch failed");
                            emit_tab_closed_state(
                                &app,
                                &state_events,
                                tab,
                                true,
                                None,
                                Some(message),
                            );
                        }
                    }
                    continue;
                }

                // Shell tab restart (Phase 6 emits this after a fresh PTY
                // has been bound). Clears the closed flag (and any custom
                // launch-failure message) so the overlay hides; AI tabs
                // ignore.
                if let StateSignal::ShellRestarted { tab } = &signal {
                    let tab = tab.clone();
                    if let Some(ts) = tabs.get_mut(&tab) {
                        if matches!(ts.kind, TabKind::Shell) && ts.closed {
                            ts.closed = false;
                            ts.closed_exit_code = None;
                            ts.closed_message = None;
                            info!(?tab, "shell tab: restarted");
                            emit_tab_closed_state(&app, &state_events, tab, false, None, None);
                        }
                    }
                    continue;
                }

                // Compose signals always target the active tab (the compose
                // overlay submits to whoever is on screen). The signal
                // arrives tagged with `active` from the IPC handler, but we
                // re-resolve here defensively in case anything ever changes.
                let target_tab = match &signal {
                    StateSignal::ComposeContentChanged { .. } => active.clone(),
                    other => other.tab(),
                };

                let Some(ts) = tabs.get_mut(&target_tab) else { continue };

                match &signal {
                    StateSignal::UserKeystroke { .. } => {
                        ts.has_unsent_input = true;
                        ts.last_keystroke_at = Some(Instant::now());
                    }
                    StateSignal::UserSubmit { .. } => {
                        ts.has_unsent_input = false;
                        ts.last_keystroke_at = Some(Instant::now());
                    }
                    StateSignal::ComposeContentChanged { non_empty, .. } => {
                        ts.composing = *non_empty;
                        if *non_empty {
                            ts.last_keystroke_at = Some(Instant::now());
                        }
                    }
                    StateSignal::ClaudeOutputStarted { .. } => {
                        ts.claude_output_active = true;
                    }
                    StateSignal::ClaudeOutputStopped { .. } => {
                        ts.claude_output_active = false;
                    }
                    // Reset the output-active flag on any error edge / its
                    // acknowledgment. A ClaudeOutputStarted with no matching
                    // Stopped (the subprocess crashed or exited mid-output —
                    // the normal exit path) would otherwise leave the flag
                    // stuck true, so a later normal speech cycle resolves to
                    // Thinking instead of Idle (avatar sticks; no idle
                    // announcement). Runs before `transition()` below.
                    StateSignal::SubprocessExited { .. }
                    | StateSignal::AudioError { .. }
                    | StateSignal::TtsError { .. }
                    | StateSignal::ErrorAcknowledged { .. } => {
                        ts.claude_output_active = false;
                    }
                    _ => {}
                }

                // Input-driven clearing of awaiting_permission /
                // awaiting_question. The user typing into the prompt is the
                // signal that the prompt is being answered; clearing an
                // already-false flag is a no-op.
                let is_input = matches!(
                    signal,
                    StateSignal::UserKeystroke { .. } | StateSignal::UserSubmit { .. }
                );
                if is_input && ts.awaiting_permission {
                    ts.awaiting_permission = false;
                    info!(tab = ?target_tab, "awaiting permission: cleared (input)");
                    emit_awaiting_permission(&app, &state_events, target_tab.clone(), false);
                }
                if is_input && ts.awaiting_question {
                    ts.awaiting_question = false;
                    info!(tab = ?target_tab, "awaiting question: cleared (input)");
                    emit_awaiting_question(&app, &state_events, target_tab.clone(), false);
                }

                let prev_state = ts.avatar_state;
                // Shell tabs short-circuit transition — only Idle ↔ Error
                // is reachable for them, and SubprocessExited has already
                // been routed elsewhere. The remaining error edges
                // (AudioError, TtsError, ErrorAcknowledged) come through
                // here and use the same logic as AI tabs.
                let is_shell = matches!(ts.kind, TabKind::Shell);
                let next = if is_shell && !is_error_edge(&signal) {
                    prev_state
                } else {
                    transition(
                        prev_state,
                        &signal,
                        ts.has_unsent_input,
                        ts.composing,
                        ts.claude_output_active,
                    )
                };
                if next != prev_state {
                    info!(tab = ?target_tab, from = ?prev_state, to = ?next, ?signal, "avatar state");
                    ts.avatar_state = next;
                    let inactive = target_tab != active;
                    let bump_done_while_away = next == AvatarState::Idle && inactive && !ts.done_while_away;
                    if bump_done_while_away {
                        ts.done_while_away = true;
                    }
                    emit_state(&app, &state_events, target_tab.clone(), next);
                    if next == AvatarState::Error {
                        if let Some(info) = ErrorInfo::from_signal(&signal) {
                            emit_error(&app, &info);
                        }
                    }
                    if bump_done_while_away {
                        info!(tab = ?target_tab, "done while away: set");
                        emit_done_while_away(&app, &state_events, target_tab, true);
                    }
                }
            }
            _ = tick.tick() => {
                // Per-tab idle-Listening sweep. Each tab's input-length
                // counter is independent. The RwLock read lock is held only
                // long enough to clone the per-tab `Arc<AtomicI32>`s — the
                // map is never mutated under it during the sweep.
                let snapshot: HashMap<TabId, Arc<AtomicI32>> = match input_lengths.read() {
                    Ok(g) => g.clone(),
                    // Recover a poisoned lock rather than skipping the sweep —
                    // `continue` here would permanently break the idle→Idle
                    // avatar transition for the rest of the session if any
                    // writer ever panicked. The map only holds Arcs to atomics,
                    // so a poisoned writer can't leave it logically corrupt
                    // (this mirrors how `sysmon` recovers via `into_inner`).
                    Err(e) => e.into_inner().clone(),
                };
                for (tab, ts) in tabs.iter_mut() {
                    if ts.avatar_state != AvatarState::Listening { continue; }
                    if ts.composing { continue; }
                    let len = snapshot
                        .get(tab)
                        .map(|c| c.load(Ordering::Relaxed))
                        .unwrap_or(0);
                    if len != 0 { continue; }
                    let idle_long_enough = ts
                        .last_keystroke_at
                        .map(|t| t.elapsed() >= EMPTY_INPUT_IDLE)
                        .unwrap_or(true);
                    if !idle_long_enough { continue; }
                    info!(?tab, from = ?ts.avatar_state, to = ?AvatarState::Idle, signal = "EmptyInputTimeout", "avatar state");
                    ts.avatar_state = AvatarState::Idle;
                    ts.has_unsent_input = false;
                    // Forced back to Idle by inactivity — clear any lingering
                    // output-active flag so it can't drive a later speech cycle
                    // to Thinking.
                    ts.claude_output_active = false;
                    emit_state(&app, &state_events, tab.clone(), ts.avatar_state);
                    if *tab != active && !ts.done_while_away {
                        ts.done_while_away = true;
                        info!(?tab, "done while away: set (tick)");
                        emit_done_while_away(&app, &state_events, tab.clone(), true);
                    }
                }
            }
        }
    }

    debug!("state manager: signal channel closed; exiting");
}

/// True when the signal is one of the cross-cutting error edges that apply
/// to every tab regardless of kind. `SubprocessExited` is intentionally NOT
/// in this set — Shell tabs route it to the closed sub-state, AI tabs hit
/// it via `transition()` directly.
fn is_error_edge(signal: &StateSignal) -> bool {
    matches!(
        signal,
        StateSignal::AudioError { .. }
            | StateSignal::TtsError { .. }
            | StateSignal::ErrorAcknowledged { .. }
    )
}

/// Priority-based transitions, identical to v1's logic. The `tab` carried by
/// each signal is consumed by the caller (it routes the signal to the right
/// per-tab `TabState` before invoking this).
fn transition(
    current: AvatarState,
    signal: &StateSignal,
    has_unsent_input: bool,
    composing: bool,
    claude_output_active: bool,
) -> AvatarState {
    use AvatarState::*;
    use StateSignal::*;

    if matches!(
        signal,
        SubprocessExited { .. } | AudioError { .. } | TtsError { .. }
    ) {
        return Error;
    }

    if let ComposeContentChanged { non_empty, .. } = signal {
        if *non_empty && current == Idle {
            return Listening;
        }
        return current;
    }

    match (current, signal) {
        (Error, ErrorAcknowledged { .. }) => Idle,
        (Error, _) => Error,

        (Speaking, TtsPlaybackStopped { .. }) => {
            if has_unsent_input || composing {
                Listening
            } else if claude_output_active {
                // TTS tag was an interstitial comment ("about to do X");
                // Claude is still producing output, so go back to
                // Thinking instead of falsely announcing Idle.
                Thinking
            } else {
                Idle
            }
        }
        (Speaking, _) => Speaking,

        (Thinking, TtsPlaybackStarted { .. }) => Speaking,
        (Thinking, ClaudeOutputStopped { .. }) => Idle,
        (Thinking, _) => Thinking,

        (Listening, UserSubmit { .. }) => Thinking,
        (Listening, TtsPlaybackStarted { .. }) => Speaking,
        (Listening, _) => Listening,

        (Idle, UserKeystroke { .. }) => Listening,
        (Idle, TtsPlaybackStarted { .. }) => Speaking,
        // Claude began producing output without a fresh submit (resumed
        // session, slash command, hook-driven turn). The marker-driven
        // ClaudeOutputStarted is reliable enough to surface Thinking.
        (Idle, ClaudeOutputStarted { .. }) => Thinking,
        (Idle, _) => Idle,
    }
}

/// Frontend `app.emit` + in-process broadcast share the same event payload.
/// `broadcast::send` returns Err only when there are zero subscribers, which
/// is the normal case at startup, so we drop that result silently.
fn dispatch(app: &AppHandle, bcast: &broadcast::Sender<StateEvent>, event: StateEvent) {
    if let Err(e) = app.emit("avatar-state", &event) {
        warn!(error = %e, "failed to emit avatar-state");
    }
    let _ = bcast.send(event);
}

fn emit_state(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    state: AvatarState,
) {
    dispatch(app, bcast, StateEvent::StateChanged { tab, state });
}

fn emit_active_tab(app: &AppHandle, bcast: &broadcast::Sender<StateEvent>, tab: TabId) {
    dispatch(app, bcast, StateEvent::ActiveTabChanged { tab });
}

fn emit_error(app: &AppHandle, info: &ErrorInfo) {
    if let Err(e) = app.emit("avatar-error", info) {
        warn!(error = %e, "failed to emit avatar-error");
    }
}

fn emit_awaiting_permission(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    awaiting: bool,
) {
    dispatch(app, bcast, StateEvent::AwaitingPermissionChanged { tab, awaiting });
}

fn emit_awaiting_question(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    awaiting: bool,
) {
    dispatch(app, bcast, StateEvent::AwaitingQuestionChanged { tab, awaiting });
}

fn emit_done_while_away(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    done: bool,
) {
    dispatch(app, bcast, StateEvent::DoneWhileAwayChanged { tab, done });
}

fn emit_tab_closed_state(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    closed: bool,
    exit_code: Option<i32>,
    closed_message: Option<String>,
) {
    dispatch(
        app,
        bcast,
        StateEvent::TabClosedStateChanged {
            tab,
            closed,
            exit_code,
            closed_message,
        },
    );
}

fn emit_tab_created(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    kind: TabKindWire,
    name: String,
    builtin: bool,
    position: usize,
) {
    dispatch(
        app,
        bcast,
        StateEvent::TabCreated {
            tab,
            kind,
            name,
            builtin,
            position,
        },
    );
}

fn emit_tab_closed_event(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
) {
    dispatch(app, bcast, StateEvent::TabClosed { tab });
}

fn emit_tab_renamed(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    name: String,
) {
    dispatch(app, bcast, StateEvent::TabRenamed { tab, name });
}

#[cfg(test)]
mod tests {
    use super::*;
    use AvatarState::*;
    use StateSignal::*;

    fn tab() -> TabId {
        TabId::Claude
    }

    fn t(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, false, false, false)
    }

    fn t_with_input(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, true, false, false)
    }

    fn t_composing(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, false, true, false)
    }

    fn t_with_output(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, false, false, true)
    }

    #[test]
    fn idle_keystroke_listens() {
        assert_eq!(t(Idle, UserKeystroke { tab: tab() }), Listening);
    }

    #[test]
    fn idle_bare_enter_stays_idle() {
        assert_eq!(t(Idle, UserSubmit { tab: tab() }), Idle);
    }

    #[test]
    fn listening_enter_thinks() {
        assert_eq!(t(Listening, UserSubmit { tab: tab() }), Thinking);
    }

    #[test]
    fn listening_more_typing_stays() {
        assert_eq!(t(Listening, UserKeystroke { tab: tab() }), Listening);
    }

    #[test]
    fn listening_tts_speaks() {
        assert_eq!(t(Listening, TtsPlaybackStarted { tab: tab() }), Speaking);
    }

    #[test]
    fn thinking_tts_speaks() {
        assert_eq!(t(Thinking, TtsPlaybackStarted { tab: tab() }), Speaking);
    }

    #[test]
    fn thinking_claude_done_returns_idle() {
        assert_eq!(t(Thinking, ClaudeOutputStopped { tab: tab() }), Idle);
    }

    #[test]
    fn idle_claude_output_starts_thinking() {
        // Marker-driven ClaudeOutputStarted surfaces Thinking even without a
        // fresh UserSubmit (resumed session, slash command, hook turn).
        assert_eq!(t(Idle, ClaudeOutputStarted { tab: tab() }), Thinking);
    }

    #[test]
    fn thinking_typing_or_enter_ignored() {
        assert_eq!(t(Thinking, UserKeystroke { tab: tab() }), Thinking);
        assert_eq!(t(Thinking, UserSubmit { tab: tab() }), Thinking);
    }

    #[test]
    fn speaking_tts_stop_returns_idle_when_no_pending_input() {
        assert_eq!(t(Speaking, TtsPlaybackStopped { tab: tab() }), Idle);
    }

    #[test]
    fn speaking_tts_stop_resumes_listening_when_user_typed() {
        assert_eq!(
            t_with_input(Speaking, TtsPlaybackStopped { tab: tab() }),
            Listening
        );
    }

    #[test]
    fn speaking_typing_or_enter_ignored() {
        assert_eq!(t(Speaking, UserKeystroke { tab: tab() }), Speaking);
        assert_eq!(t(Speaking, UserSubmit { tab: tab() }), Speaking);
    }

    #[test]
    fn errors_interrupt_any_state() {
        for s in [Idle, Listening, Thinking, Speaking] {
            assert_eq!(t(s, SubprocessExited { tab: tab(), code: None }), Error);
            assert_eq!(t(s, AudioError { tab: tab() }), Error);
            assert_eq!(t(s, TtsError { tab: tab() }), Error);
        }
    }

    #[test]
    fn idle_compose_non_empty_listens() {
        assert_eq!(
            t(Idle, ComposeContentChanged { tab: tab(), non_empty: true }),
            Listening,
        );
    }

    #[test]
    fn idle_compose_empty_stays_idle() {
        assert_eq!(
            t(Idle, ComposeContentChanged { tab: tab(), non_empty: false }),
            Idle
        );
    }

    #[test]
    fn compose_does_not_preempt_higher_states() {
        assert_eq!(
            t(Thinking, ComposeContentChanged { tab: tab(), non_empty: true }),
            Thinking,
        );
        assert_eq!(
            t(Speaking, ComposeContentChanged { tab: tab(), non_empty: true }),
            Speaking,
        );
    }

    #[test]
    fn speaking_tts_stop_resumes_listening_when_composing() {
        assert_eq!(
            t_composing(Speaking, TtsPlaybackStopped { tab: tab() }),
            Listening
        );
    }

    #[test]
    fn speaking_tts_stop_returns_thinking_when_claude_still_outputting() {
        // Interstitial TTS tag ("I'll start by reading the file"): Claude
        // is still producing output behind the speech, so the avatar
        // should go back to Thinking, not Idle.
        assert_eq!(
            t_with_output(Speaking, TtsPlaybackStopped { tab: tab() }),
            Thinking
        );
    }

    #[test]
    fn speaking_tts_stop_user_input_beats_claude_output() {
        // If the user typed during speech, treat it like a normal
        // interruption — Listening wins over Thinking.
        assert_eq!(
            transition(
                Speaking,
                &TtsPlaybackStopped { tab: tab() },
                true,  // has_unsent_input
                false, // composing
                true,  // claude_output_active
            ),
            Listening
        );
    }

    #[test]
    fn error_sticks_until_acknowledged() {
        assert_eq!(t(Error, UserKeystroke { tab: tab() }), Error);
        assert_eq!(t(Error, UserSubmit { tab: tab() }), Error);
        assert_eq!(t(Error, TtsPlaybackStarted { tab: tab() }), Error);
        assert_eq!(t(Error, ErrorAcknowledged { tab: tab() }), Idle);
    }

    #[test]
    fn permission_signals_dont_drive_avatar() {
        // Defensive: PermissionPromptDetected/Resolved short-circuit before
        // the run loop calls `transition()`, but if they ever reached it
        // they should be no-ops in every state. Same contract for the
        // question prompt edges.
        for s in [Idle, Listening, Thinking, Speaking, Error] {
            assert_eq!(t(s, PermissionPromptDetected { tab: tab() }), s);
            assert_eq!(t(s, PermissionPromptResolved { tab: tab() }), s);
            assert_eq!(t(s, QuestionPromptDetected { tab: tab() }), s);
            assert_eq!(t(s, QuestionPromptResolved { tab: tab() }), s);
        }
    }

    #[test]
    fn tab_id_serde_round_trips() {
        for id in [
            TabId::Claude,
            TabId::ClaudeLocal,
            TabId::OpenCode,
            TabId::Ai("ai-1234".to_string()),
            TabId::Shell("shell-1".to_string()),
            TabId::Shell("user-bash".to_string()),
        ] {
            let s = serde_json::to_string(&id).unwrap();
            let back: TabId = serde_json::from_str(&s).unwrap();
            assert_eq!(id, back);
        }
    }

    #[test]
    fn spawned_ai_id_routes_to_ai_not_shell() {
        // Spawned duplicates carry an "ai-<uuid>" id and must come back as
        // `Ai` (AI-kind, non-builtin) on relaunch — not `Shell`. The reserved
        // "opencode" id must stay its own variant despite sharing the "ai"
        // prefix-without-dash (it doesn't, but the routing guard still must not
        // capture it).
        assert_eq!(
            TabId::from_str("ai-abc123"),
            TabId::Ai("ai-abc123".to_string())
        );
        assert_eq!(TabId::from_str("opencode"), TabId::OpenCode);
        assert_eq!(
            TabId::from_str("shell-xyz"),
            TabId::Shell("shell-xyz".to_string())
        );
    }

    #[test]
    fn spawned_ai_tab_is_ai_kind_but_not_builtin() {
        let dup = TabId::Ai("ai-abc123".to_string());
        assert_eq!(dup.kind(), TabKind::AiTool);
        assert!(!dup.is_builtin());
        // Only the reserved AI tabs are builtins. All Shell tabs — including
        // the retired `shell-broot` id and on-demand tool tabs — are closable.
        assert!(TabId::Claude.is_builtin());
        assert!(TabId::OpenCode.is_builtin());
        assert!(!TabId::Shell("shell-broot".into()).is_builtin());
        assert!(!TabId::Shell("shell-1".into()).is_builtin());
    }

    #[test]
    fn tab_id_wire_format_preserved() {
        assert_eq!(serde_json::to_string(&TabId::Claude).unwrap(), "\"claude\"");
        assert_eq!(
            serde_json::to_string(&TabId::ClaudeLocal).unwrap(),
            "\"claude-local\""
        );
        assert_eq!(
            serde_json::to_string(&TabId::OpenCode).unwrap(),
            "\"opencode\""
        );
        assert_eq!(
            serde_json::to_string(&TabId::Shell("shell-1".to_string())).unwrap(),
            "\"shell-1\""
        );
    }

    #[test]
    fn tab_id_kind_mapping() {
        assert_eq!(TabId::Claude.kind(), TabKind::AiTool);
        assert_eq!(TabId::ClaudeLocal.kind(), TabKind::AiTool);
        assert_eq!(TabId::OpenCode.kind(), TabKind::AiTool);
        assert_eq!(TabId::Ai("ai-1".into()).kind(), TabKind::AiTool);
        assert_eq!(TabId::Shell("anything".into()).kind(), TabKind::Shell);
    }

    #[test]
    fn is_error_edge_covers_the_universal_signals() {
        assert!(is_error_edge(&AudioError { tab: tab() }));
        assert!(is_error_edge(&TtsError { tab: tab() }));
        assert!(is_error_edge(&ErrorAcknowledged { tab: tab() }));
        // SubprocessExited is intentionally NOT in the set — Shell tabs
        // route it to the closed sub-state in the run loop, AI tabs hit
        // it via transition() directly.
        assert!(!is_error_edge(&SubprocessExited {
            tab: tab(),
            code: None
        }));
        assert!(!is_error_edge(&UserKeystroke { tab: tab() }));
        assert!(!is_error_edge(&UserSubmit { tab: tab() }));
        assert!(!is_error_edge(&TtsPlaybackStarted { tab: tab() }));
    }
}
