use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Shared, runtime-mutable per-tab input-length counter map. The state
/// manager mutates it on TabAdded/TabRemoved; the IPC `pty_write` handler
/// reads a counter Arc per-write. `RwLock` rather than `DashMap` because
/// mutations are rare (tab create/close) while reads are also rare (one per
/// keystroke per tab); a plain RwLock is simpler than a third dependency.
pub type InputLengths = Arc<RwLock<HashMap<TabId, Arc<AtomicI32>>>>;

/// V39 Phase A (locked decision 4): why a tab's keyboard is locked.
///
/// Two sources, deliberately not collapsed into one flag — they have
/// different lifetimes and different owners:
///
/// * [`ReadOnlySource::User`] — the sticky lock the user sets from the tab's
///   communication popover. Persisted in `AiToolTabConfig::read_only` and
///   restored into [`ReadOnlyTabs`] at app start.
/// * [`ReadOnlySource::Driven`] — the transient lock the delegation engine
///   holds while it drives the tab (Phase B). **Never persisted** — after a
///   restart no tab is `Driven`.
///
/// Named `User`, not `Manual`: `Manual` is a tab *role* in Phase B and the two
/// must not read as the same concept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum ReadOnlySource {
    User,
    Driven { by: TabId },
}

impl ReadOnlySource {
    /// The human reason every refusal carries. A block with a blank reason is
    /// not a block the user can act on, so this is the ONE place the two
    /// strings are spelled — the IPC error, the toast and the popover all
    /// render what this returns.
    ///
    /// `driver_name` is the *display name* of the driving tab (Phase B passes
    /// it; names live in the tab registry, not here). Falls back to the
    /// driver's tab id, never to an empty string.
    pub fn reason(&self, driver_name: Option<&str>) -> String {
        match self {
            ReadOnlySource::User => "read-only (user)".to_string(),
            ReadOnlySource::Driven { by } => {
                let who = driver_name
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| by.as_str());
                format!("driven by {who}")
            }
        }
    }
}

/// One tab's row in [`ReadOnlyTabs`]. Both sources are held side by side so
/// that clearing the engine's lock at the end of a delegation cannot silently
/// clear the user's sticky lock — they are set by different actors, and a
/// single last-writer-wins slot would lose one of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ReadOnlyEntry {
    user: bool,
    driven_by: Option<TabId>,
    /// **A prompt is standing on this tab, so the keyboard is open for it**
    /// (locked decision 5, V39 review M-5).
    ///
    /// A separate flag rather than "clear `driven_by` for the duration", which
    /// is what the engine used to do: a tab whose USER lock was also set then
    /// fell back to `ReadOnlySource::User` and the keyboard stayed refused —
    /// so the one prompt only the user can answer could not be answered, and
    /// the delegation ran to its deadline reporting "worker awaiting
    /// permission". Clearing it also lost the row's `driven_by` for the
    /// duration, which the banner and the take-over path read.
    ///
    /// Lives and dies with the flight: [`ReadOnlyTabs::set_driven`] clears it
    /// whenever the engine's lock is released, so it can never outlive the
    /// prompt that justified it.
    prompt_relaxed: bool,
}

impl ReadOnlyEntry {
    /// `Driven` wins over `User` while both hold: it is the more specific
    /// answer to "why did my keystroke bounce", and it names who to take over
    /// from.
    fn source(&self) -> Option<ReadOnlySource> {
        // Decision 5 outranks both locks while a prompt stands: answering the
        // prompt the worker addressed to the USER is the only way this turn
        // completes, and neither lock is worth more than the turn.
        if self.prompt_relaxed {
            return None;
        }
        match (&self.driven_by, self.user) {
            (Some(by), _) => Some(ReadOnlySource::Driven { by: by.clone() }),
            (None, true) => Some(ReadOnlySource::User),
            (None, false) => None,
        }
    }

    fn is_clear(&self) -> bool {
        // `prompt_relaxed` is deliberately not consulted: it only ever holds
        // inside a flight, and `set_driven(None)` clears it on the way out. A
        // row that was nothing but a relaxation would be a leak, and dropping
        // it here is what makes that impossible.
        !self.user && self.driven_by.is_none()
    }
}

/// Shared, runtime-mutable per-tab read-only state — the map `pty_write` asks
/// before it does anything else (locked decision 4: enforcement is
/// server-side, and it runs ahead of every side effect of a write).
///
/// **Why this is a shared map and not a field on [`TabState`]:** `TabState`
/// lives inside the state-manager actor task and is reachable only by sending
/// a [`StateSignal`], so an IPC command cannot read it at all, let alone
/// synchronously — an enforcement check that has to round-trip through an
/// actor has a race window in front of it. This handle has exactly the shape
/// of [`InputLengths`] directly above (shared, held by `AppState`, mutated on
/// tab-lifecycle edges, read on the write path under one `RwLock`), which is
/// the repo's existing answer to "the write path needs per-tab runtime state".
/// The state manager can take a clone of this handle when Phase B needs the
/// permission-prompt relaxation (decision 5) — one map, one owner per source,
/// no mirror to keep in sync.
#[derive(Clone, Default)]
pub struct ReadOnlyTabs {
    inner: Arc<RwLock<HashMap<TabId, ReadOnlyEntry>>>,
}

impl ReadOnlyTabs {
    /// Seed the user locks from persisted `AiToolTabConfig::read_only` at app
    /// start. `Driven` is never seeded — nothing is in flight at startup.
    pub fn seeded(user_locked: impl IntoIterator<Item = TabId>) -> Self {
        let this = Self::default();
        this.sync_users(user_locked);
        this
    }

    /// The effective lock on `tab`, or `None` when the keyboard is free.
    pub fn read_only(&self, tab: &TabId) -> Option<ReadOnlySource> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        map.get(tab).and_then(ReadOnlyEntry::source)
    }

    /// Set or clear the sticky user lock (the popover's Access radio).
    pub fn set_user(&self, tab: &TabId, on: bool) {
        self.mutate(tab, |e| e.user = on);
    }

    /// Make the user locks match `user_locked` exactly — the tabs whose
    /// persisted `read_only` is currently `true`.
    ///
    /// Called on every settings broadcast, not just at startup: `read_only`
    /// is a persisted field, so the Settings window, a project-overlay switch
    /// and a hand-edited settings file can all move it without going through
    /// [`Self::set_user`]. Without this the runtime map would be a second
    /// source of truth that silently drifts from the file. `Driven` rows are
    /// untouched — settings has no opinion about them.
    pub fn sync_users(&self, user_locked: impl IntoIterator<Item = TabId>) {
        let wanted: std::collections::HashSet<TabId> = user_locked.into_iter().collect();
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        for tab in &wanted {
            map.entry(tab.clone()).or_default().user = true;
        }
        map.retain(|tab, entry| {
            if !wanted.contains(tab) {
                entry.user = false;
            }
            !entry.is_clear()
        });
    }

    /// Set or clear the engine's transient lock. Phase B's engine calls this
    /// *before* it writes, so the "user types during the paste window" race is
    /// closed by ordering rather than by timing.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_driven(&self, tab: &TabId, by: Option<TabId>) {
        self.mutate(tab, |e| {
            // Releasing the engine's lock ends any prompt relaxation with it:
            // the relaxation is a property of a flight, and a flight that has
            // ended cannot be relaxing anything.
            if by.is_none() {
                e.prompt_relaxed = false;
            }
            e.driven_by = by;
        });
    }

    /// **Open (or re-close) the keyboard for a standing prompt** (locked
    /// decision 5). Called by the delegation engine on the prompt's rising and
    /// falling edges; the engine's own lock stays recorded throughout, so the
    /// banner keeps naming the driver and Take over keeps working.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_prompt_relaxed(&self, tab: &TabId, on: bool) {
        self.mutate(tab, |e| e.prompt_relaxed = on);
    }

    /// Drop a closed tab's row. Called from `close_tab` next to the other
    /// per-tab map cleanups.
    pub fn forget(&self, tab: &TabId) {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        map.remove(tab);
    }

    fn mutate(&self, tab: &TabId, f: impl FnOnce(&mut ReadOnlyEntry)) {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(tab.clone()).or_default();
        f(entry);
        if entry.is_clear() {
            map.remove(tab);
        }
    }
}

/// V39 Phase B — **the readable mirror of the three `TabState` flags the
/// delegation engine's preflight and wait loop need**, kept beside
/// [`ReadOnlyTabs`] and for the identical reason.
///
/// `TabState` lives inside the state-manager actor task and is reachable only
/// by *sending* a [`StateSignal`]; nothing can read it. Preflight (locked
/// decision 12) has to answer "is this worker idle, and is a prompt standing?"
/// **before** it types, and the wait loop has to notice a prompt appearing
/// *during* a flight (decision 5: relax the lock, extend the deadline). An
/// actor round-trip in front of either would be a race with a message queue in
/// the middle of it.
///
/// So the state manager writes the three edges it already computes into this
/// shared map as it handles them, and readers above the seam get a
/// synchronous, lock-free-ish answer. **One writer, many readers**: nothing
/// outside [`note_signal`](Self::note_signal) may set a flag, which is what
/// keeps this a mirror rather than a second source of truth.
///
/// [`StateSignal`]: crate::state::StateSignal
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TabActivityFlags {
    /// A permission prompt is standing on this tab
    /// (`PermissionPromptDetected` … `PermissionPromptResolved`).
    pub awaiting_permission: bool,
    /// An AskUserQuestion-style prompt is standing (the same pair, for
    /// questions). Tracked separately because the two can stand at once and
    /// each clears on its own edge.
    pub awaiting_question: bool,
    /// Output is streaming right now (`HarnessOutputStarted` …
    /// `HarnessOutputStopped`) — i.e. a turn is in flight. Preflight refuses to
    /// type into a tab mid-burst: the request would land in the middle of
    /// someone else's turn.
    pub output_running: bool,
    /// The start this row describes (V39 review R-5) — bumped by
    /// [`TabActivity::begin_start`] before every spawn, and carried by that
    /// spawn's exit signal so a late exit from a PREVIOUS start can be ignored
    /// instead of re-latching `exited` on a live process.
    pub start_gen: u64,
    /// The tab's subprocess has exited. Latched: only a restart clears it —
    /// `TabAdded` (a fresh tab), `ShellRestarted` (a shell respawn) or
    /// `pty_restart` (an AI tab respawn, which emits no signal of its own).
    /// V39 review HIGH-3 is what happens when one of those is missing: the row
    /// stays `exited` and every later preflight refuses the tab with "has no
    /// running process", for a process that is running.
    pub exited: bool,
}

impl TabActivityFlags {
    /// Whether a prompt of either kind is standing — the predicate decision 5
    /// is written in terms of.
    pub fn awaiting_prompt(&self) -> bool {
        self.awaiting_permission || self.awaiting_question
    }
}

/// Shared, runtime-mutable per-tab activity flags — see [`TabActivityFlags`].
#[derive(Clone, Default)]
pub struct TabActivity {
    inner: Arc<RwLock<HashMap<TabId, TabActivityFlags>>>,
}

impl TabActivity {
    /// This tab's flags. An unknown tab reads as all-false, which is the
    /// honest answer: nothing has been observed about it.
    ///
    /// Note what that means for `exited` — a tab the mirror has never seen is
    /// NOT reported as exited. Liveness is the tab registry's answer
    /// (`TabRegistry::is_started`); this flag only records an exit that was
    /// observed, and the engine checks both.
    pub fn flags(&self, tab: &TabId) -> TabActivityFlags {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        map.get(tab).copied().unwrap_or_default()
    }

    /// Fold one state signal into the mirror. **The only writer.**
    ///
    /// Called from the state manager's loop with every signal, before any of
    /// its `continue`-guarded branches, so a signal handled by an early return
    /// still updates the mirror. Deliberately total over the signal set with a
    /// catch-all: a new signal that says nothing about these four facts must
    /// not need an edit here.
    pub fn note_signal(&self, signal: &StateSignal) {
        let (tab, apply): (&TabId, fn(&mut TabActivityFlags)) = match signal {
            StateSignal::PermissionPromptDetected { tab } => (tab, |f| f.awaiting_permission = true),
            StateSignal::PermissionPromptResolved { tab } => {
                (tab, |f| f.awaiting_permission = false)
            }
            StateSignal::QuestionPromptDetected { tab } => (tab, |f| f.awaiting_question = true),
            StateSignal::QuestionPromptResolved { tab } => (tab, |f| f.awaiting_question = false),
            StateSignal::HarnessOutputStarted { tab } => (tab, |f| f.output_running = true),
            StateSignal::HarnessOutputStopped { tab } => (tab, |f| f.output_running = false),
            // V39 review R-5: an exit from a start this row has moved past is
            // NOT this process's exit. Handled here rather than by the generic
            // arm below because it is the one signal whose meaning depends on
            // WHICH start it came from.
            StateSignal::SubprocessExited { tab, start_gen, .. } => {
                let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
                let entry = map.entry(tab.clone()).or_default();
                if *start_gen < entry.start_gen {
                    return;
                }
                entry.exited = true;
                // A dead process is not mid-burst and holds no prompt. Left
                // set, `output_running` would make every later preflight
                // refuse a tab the user has since restarted, for a burst that
                // ended when the process did.
                entry.output_running = false;
                entry.awaiting_permission = false;
                entry.awaiting_question = false;
                return;
            }
            // V39 review HIGH-3: a restarted subprocess is a CLEAN one. The
            // mirror's `exited` is latched and was cleared only by `TabAdded`,
            // which a restart into an existing tab never sends — so a worker
            // that had exited once was refused forever, "has no running
            // process", however many times it was restarted. Whole-row reset
            // rather than clearing `exited` alone: every flag here describes
            // the process that just went away.
            StateSignal::ShellRestarted { tab } => (tab, |f| *f = TabActivityFlags::default()),
            // A user keystroke or submit clears a standing prompt in the state
            // manager's own bookkeeping, so it clears here too — otherwise a
            // prompt the user answered by typing would hold the deadline open
            // for the rest of the flight.
            StateSignal::UserSubmit { tab } => (tab, |f| {
                f.awaiting_permission = false;
                f.awaiting_question = false;
            }),
            _ => return,
        };
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        apply(map.entry(tab.clone()).or_default());
    }

    /// Seed (or re-seed) a tab with clean flags — a fresh or restarted
    /// subprocess. Clears a latched `exited`, and keeps the row's generation
    /// (only [`Self::begin_start`] moves that).
    pub fn reset(&self, tab: &TabId) {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let start_gen = map.get(tab).map(|f| f.start_gen).unwrap_or(0);
        map.insert(
            tab.clone(),
            TabActivityFlags {
                start_gen,
                ..TabActivityFlags::default()
            },
        );
    }

    /// **A new subprocess is about to be spawned for `tab`** (V39 review R-5):
    /// clean flags, and a NEW generation, which the caller hands to the spawn
    /// so that start's exit signal carries it.
    ///
    /// The generation is what makes a late exit recognisable. `reset` alone
    /// could not: signals arrive through an mpsc, so an exit emitted while the
    /// old child was being killed can be handled after the reset, re-latching
    /// `exited` on the process that just started — and preflight then refuses a
    /// live worker with "has no running process", permanently.
    pub fn begin_start(&self, tab: &TabId) -> u64 {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let start_gen = map.get(tab).map(|f| f.start_gen).unwrap_or(0) + 1;
        map.insert(
            tab.clone(),
            TabActivityFlags {
                start_gen,
                ..TabActivityFlags::default()
            },
        );
        start_gen
    }

    /// Drop a closed tab's row, beside the other per-tab map cleanups.
    pub fn forget(&self, tab: &TabId) {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        map.remove(tab);
    }
}

/// Identifier for one of the multi-tab subprocesses cimp owns. `Harness(id)`
/// covers every reserved AI built-in a registered harness declares;
/// `Shell(id)` carries the user-managed tab IDs introduced in v3 (M1 had a
/// hardcoded "shell-1"; M2/M3 generalize). The runtime kind discriminator is
/// [`TabKind`], not this — `TabId` is purely an opaque identity used as HashMap
/// key and IPC payload.
///
/// Wire format: a single string. Every variant serializes as the id string it
/// carries, verbatim. Round-tripping a reserved built-in tab id yields a
/// `Harness` variant, a string starting with `"ai-"` yields an `Ai` variant, and
/// any other unrecognized string yields a `Shell` variant.
#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub enum TabId {
    /// One of the **reserved built-in tabs a registered harness declares** —
    /// `claude`, `claude-local`, `opencode` today.
    ///
    /// V40 Phase I (issue #107 item 1): was three variants, one per shipped
    /// tab. Review finding M-3 was that a third descriptor's tab had no arm
    /// here, so it fell through to [`Self::Shell`] while [`Self::kind`] —
    /// which already asked the registry — answered `AiTool` for the same id: a
    /// `Shell` variant claiming AI kind, with nothing to say so.
    ///
    /// The payload is the registry's own `&'static str` (see
    /// [`crate::harness::registry::BuiltinTab`]), so this variant cannot name
    /// an id no harness declares: [`Self::from_str`] resolves it through the
    /// registry and an unclaimed string takes one of the arms below. The wire
    /// form is unchanged — `"claude"` is still `"claude"` (locked decision 29).
    Harness(&'static str),
    /// A user-spawned *duplicate* of one of the AI builtins (the `+` on
    /// a Claude/OpenCode tab). Carries a `"ai-<uuid>"` id and is a closable,
    /// non-builtin AI-kind tab. Its launch behavior (env synthesis,
    /// `--append-system-prompt`, etc.) is driven entirely by its
    /// `AiToolTabConfig` in settings — which is cloned from the template
    /// tab at spawn time — so this variant carries no template marker.
    Ai(String),
    Shell(String),
    /// V9-01: the read-only, non-closable Code Graph monitor tab. Shell-kind
    /// (see [`Self::kind`]) but a distinct, reserved identity so it never
    /// collides with a user shell and the close guard can refuse it.
    /// App-rendered (an in-process dashboard of the graph indexer/embedder);
    /// spawns no PTY of its own.
    GraphMonitor,
    /// V13 Phase A: the read-only, non-closable Workbench tab (live diff /
    /// checkpoint timeline / worktrees). Same shape as [`Self::GraphMonitor`]
    /// — Shell-kind, reserved identity, no PTY, app-rendered.
    Workbench,
    /// The read-only, non-closable Tool Activity tab (unified graph-call +
    /// offload-request feed plus the tool reference lists). Same shape as
    /// [`Self::GraphMonitor`] — Shell-kind, reserved identity, no PTY,
    /// app-rendered.
    ToolActivity,
    /// #51: the read-only, non-closable Events tab (the activity feed
    /// attributed per tab/session, with kind/source/tab filters). Same shape
    /// as [`Self::GraphMonitor`] — Shell-kind, reserved identity, no PTY,
    /// app-rendered. Additive to [`Self::ToolActivity`], which keeps its own
    /// feed.
    Events,
    /// V14 Phase F: a user-created Preview tab (embedded localhost browser).
    /// Unlike the reserved app-rendered tabs above, this is a genuinely new
    /// [`TabKind`] (not a Shell-kind reserved id) because it's repeatable —
    /// a user may open several. Carries a `"preview-<uuid>"` id; its
    /// `PreviewTabConfig` (url/device_width/auto_reload) lives in settings.
    /// Like the reserved dashboards, it spawns no PTY — the frontend never
    /// calls `pty_start` for a Preview-kind tab (see `tabs::config::build_launch_spec`,
    /// which errors if it ever were).
    Preview(String),
}

impl TabId {
    /// **The fallback identity for a reader that must answer with SOME tab** —
    /// the first registered harness's first built-in tab (V40 Phase E, locked
    /// decision 26).
    ///
    /// Three hot paths need one: the boot active-tab resolution when settings
    /// name no surviving tab, and the two poisoned-lock reads of the shared
    /// active-tab cell (`audio::playback`, `notifications::manager`), where a
    /// `panic!` would permanently kill the audio or notification task. All three
    /// used to say `TabId::Claude` — "when in doubt, Claude", six times over,
    /// which is what locked decision 2 removed everywhere else.
    ///
    /// It is still a guess; what changed is that it is the REGISTRY's guess and
    /// moves with the registry, so a build that ships a different first harness
    /// does not silently fall back to one it does not ship. A build that ships
    /// NO harness has no AI tab to name, and answers the default shell id — the
    /// only tab such a build has.
    pub fn first_harness_default() -> TabId {
        crate::harness::registry::HARNESSES
            .first()
            .and_then(|d| d.tabs.first())
            .map(|t| TabId::Harness(t.id))
            .unwrap_or_else(|| TabId::Shell(crate::settings::SHELL_DEFAULT_TAB_ID.to_string()))
    }

    pub fn as_str(&self) -> &str {
        match self {
            TabId::Harness(s) => s,
            TabId::GraphMonitor => "graph-monitor",
            TabId::Workbench => "workbench-1",
            TabId::ToolActivity => "tool-activity",
            TabId::Events => "events",
            TabId::Ai(s) => s.as_str(),
            TabId::Shell(s) => s.as_str(),
            TabId::Preview(s) => s.as_str(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        // V40 Phase I: the reserved AI ids are the REGISTRY's, resolved before
        // core's own reserved strings. A harness tab id is checked first so the
        // `&'static str` this variant needs comes from the descriptor rather
        // than from an allocation — and so this arm and `Self::kind` (which has
        // asked the registry since Phase A) can never disagree about which ids
        // are AI tabs.
        if let Some(t) = crate::harness::registry::builtin_tab(s) {
            return TabId::Harness(t.id);
        }
        match s {
            "graph-monitor" => TabId::GraphMonitor,
            "workbench-1" => TabId::Workbench,
            "tool-activity" => TabId::ToolActivity,
            "events" => TabId::Events,
            // "code-quality" (the retired V25 reserved tab — its Quality view
            // now lives inside the Code Audit surface as a sub-tab),
            // "offload-server" (the retired V8-03 reserved tab — its dashboard
            // now lives inside Tool Activity as the "Offload server" section),
            // "graph-view" (the retired V15 reserved tab — its force-graph now
            // lives inside Tool Activity as the "Graph view" section) and
            // "code-audit" (the retired V23 reserved tab — its Security |
            // Quality panels now live inside Tool Activity as the "Code audit"
            // section) intentionally
            // fall through to `Shell` below; the settings migrations prune any
            // persisted entry before the id ever reaches the runtime.
            // Spawned AI-tab duplicates carry an `"ai-<uuid>"` id (see
            // `create_ai_tab`). They must round-trip back to `Ai`, not
            // `Shell`, so they keep AI-kind behavior on relaunch. The
            // reserved exact-matches above are checked first, and
            // `"opencode"` doesn't start with `"ai-"`, so
            // there's no collision.
            other if other.starts_with("ai-") => TabId::Ai(other.to_string()),
            // V14 Phase F: Preview tabs carry a `"preview-<uuid>"` id (see
            // `create_preview_tab`) and must round-trip back to `Preview`,
            // not `Shell`, so they keep Preview-kind behavior (no PTY,
            // toolbar rendering) on relaunch.
            other if other.starts_with("preview-") => TabId::Preview(other.to_string()),
            other => TabId::Shell(other.to_string()),
        }
    }

    /// Pure mapping from id to runtime kind. Stable across milestones —
    /// every reserved AI variant and every spawned `Ai(_)` duplicate maps
    /// to `AiTool`; any `Shell(_)` id is a Shell tab. Lets call sites that
    /// don't carry `TabKind` explicitly (PTY processor, launch-spec
    /// builder) branch without threading a separate metadata table.
    pub fn kind(&self) -> TabKind {
        // V40 Phase A: "is this a reserved AI tab id" is the registry's
        // question, not a variant list — a harness added later brings its tab
        // ids with it, and this stops being a place to remember. `Ai(_)` is the
        // spawned duplicate, which is an AI tab by construction.
        if matches!(self, TabId::Ai(_))
            || crate::harness::HarnessId::from_tab_id(self.as_str()).is_some()
        {
            return TabKind::AiTool;
        }
        match self {
            // Unreachable in practice — the registry lookup above answers for
            // every `Harness(_)` — but a `Harness` variant is an AI tab by
            // construction, so the arm states that rather than falling into
            // `Shell` if the two ever diverged.
            TabId::Harness(_) | TabId::Ai(_) => TabKind::AiTool,
            // The reserved dashboards reuse Shell-kind for processing/state
            // purposes (they never run a PTY, so this is inert), keeping them
            // off the per-kind match explosion. Their read-only behavior is
            // keyed off the reserved id, not the kind.
            TabId::Shell(_)
            | TabId::GraphMonitor
            | TabId::Workbench
            | TabId::ToolActivity
            | TabId::Events => TabKind::Shell,
            // V14 Phase F: unlike the reserved dashboards above, Preview is a
            // real kind of its own — it's repeatable (a user may open several),
            // so the frontend needs a wire-visible discriminator rather than
            // id-sniffing a single reserved string.
            TabId::Preview(_) => TabKind::Preview,
        }
    }

    /// THE single enumeration of the reserved app-rendered dashboard tabs:
    /// Shell-kind with a reserved identity and NO PTY (Code Graph monitor,
    /// Workbench, Tool Activity, Events). Every guard
    /// that needs "is this one of the reserved dashboards?" — the pty-write
    /// swallow, the close refusal, the builtin flag — derives from this
    /// predicate, so a new reserved dashboard is added HERE (plus `as_str`/
    /// `from_str`/`kind`, which the compiler forces via the new variant) and
    /// the guards pick it up automatically. Mirrors the frontend's
    /// `isAppRenderedTab` (minus Note/Preview, which aren't reserved).
    pub fn is_reserved_dashboard(&self) -> bool {
        matches!(
            self,
            TabId::GraphMonitor | TabId::Workbench | TabId::ToolActivity | TabId::Events
        )
    }

    /// True for the reserved non-closable builtins: the AI builtins (which
    /// `+` spawns duplicates of) plus the reserved dashboards (removed only
    /// by disabling their feature, never by the close `×`). Spawned `Ai(_)`
    /// duplicates, all `Shell(_)` tabs (including the on-demand `rustnet` /
    /// `broot` tool tabs), and user-created Preview tabs are closable. This
    /// is the canonical `builtin` flag surfaced to the frontend (gates the
    /// close `×`; the spawn `+` is additionally gated on AI-tool kind).
    /// `tabs::registry`'s `is_builtin_id` delegates here.
    pub fn is_builtin(&self) -> bool {
        crate::harness::HarnessId::from_tab_id(self.as_str()).is_some()
            || self.is_reserved_dashboard()
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
///
/// V14 Phase F: `Preview` runs no subprocess at all (an embedded child
/// webview, not a PTY) — the state machine / notification system treat it
/// like `Shell` wherever they only care "is there a real process to
/// signal about" would be wrong to assume, so call sites that fan out on
/// `TabKind` were audited to add an explicit (usually inert) `Preview` arm
/// rather than folding it into `Shell`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabKind {
    AiTool,
    Shell,
    Preview,
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
            StateSignal::SubprocessExited { .. } => {
                (ErrorKind::SubprocessExited, "Subprocess stopped.")
            }
            StateSignal::TtsError { .. } => (ErrorKind::TtsError, "Text-to-speech is unavailable."),
            StateSignal::AudioError { .. } => {
                (ErrorKind::AudioError, "Audio output is unavailable.")
            }
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

/// Backstop for a wedged `subagents_active`, **sized by the harness whose
/// sub-agents they are** (V40 Phase D, locked decision 18).
///
/// The reader is the authoritative signal — a sub-agent id clears when its
/// result lands — but that result can be missing (the user interrupted), be
/// unparseable, or have its `SubagentsActiveChanged` edge dropped under channel
/// backpressure, leaving the avatar stuck in Thinking forever. This forces it
/// back to Idle when a tab has sat in Thinking with sub-agents nominally active
/// and the parent producing NO output for the harness's declared
/// `subagents_stall`. Safe against a genuinely in-flight turn: a busy footer
/// repaints while a turn is live, so `harness_output_active` stays true
/// throughout real work and resets this timer every tick — only a truly stopped
/// parent leaves it continuously false.
///
/// **The fallback is the longest declared value, not a constant.** A tab whose
/// harness cannot be resolved (settings unreadable, a tab id nothing claims)
/// must wait at least as long as any real harness would, because the failure
/// mode of waiting too long is a late Idle, and the failure mode of releasing
/// too early is clipping live work.
fn subagents_stall_timeout(tab: &TabId, app: &AppHandle) -> Duration {
    tab_activity_tuning(tab, app)
        .map(|t| t.subagents_stall)
        .unwrap_or_else(longest_declared_stall)
}

/// The tuning declared by the harness running in `tab`, or `None` when the tab
/// names no registered harness.
fn tab_activity_tuning(
    tab: &TabId,
    app: &AppHandle,
) -> Option<crate::harness::plugin::ActivityTuning> {
    let settings = app.try_state::<crate::ipc::AppState>()?.settings.current();
    let harness = crate::tabs::tab_harness_by_id(&settings, tab.as_str())?;
    match harness.plugin()?.activity_source() {
        crate::harness::plugin::ActivitySource::TuiMarkers(t) => Some(t),
        // An out-of-band harness declares no TUI timings; the backstop still
        // has to have a value, so it takes the conservative one below.
        crate::harness::plugin::ActivitySource::OutOfBand => None,
    }
}

/// The longest `subagents_stall` any registered harness declares, or 8 s when
/// none declares one — the pre-V40 constant, kept as the floor so a build with
/// no TUI-marker harness behaves exactly as this code always did.
fn longest_declared_stall() -> Duration {
    crate::harness::registry::all()
        .filter_map(|h| h.plugin())
        .filter_map(|p| match p.activity_source() {
            crate::harness::plugin::ActivitySource::TuiMarkers(t) => Some(t.subagents_stall),
            crate::harness::plugin::ActivitySource::OutOfBand => None,
        })
        .max()
        .unwrap_or(Duration::from_secs(8))
}

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
    UserKeystroke {
        tab: TabId,
    },
    UserSubmit {
        tab: TabId,
    },
    HarnessOutputStarted {
        tab: TabId,
    },
    HarnessOutputStopped {
        tab: TabId,
    },
    /// Out-of-band (transcript): the count of in-flight `Task` sub-agents for
    /// `tab` crossed the zero boundary. `active: true` when Claude launches
    /// one or more agents and none had been running; `active: false` when the
    /// last outstanding agent's result lands. Holds the avatar in Thinking
    /// while agents run so a marker blink between agent batches can't settle it
    /// to Idle. Emitted by `harness::claude::read`, which tails the transcript JSONL.
    SubagentsActiveChanged {
        tab: TabId,
        active: bool,
    },
    TtsPlaybackStarted {
        tab: TabId,
    },
    TtsPlaybackStopped {
        tab: TabId,
    },
    /// A selection-read (Ctrl+right-click) crossed a sentence boundary in
    /// playback. `index` is the chunk now starting to play; `index ==
    /// chunk_count` is the end-of-session sentinel. Pure pass-through — the
    /// state machine does not mutate any `TabState`, it just re-emits this as
    /// `StateEvent::TtsSelectionProgress` so the frontend can advance the
    /// read-along highlight. `session` lets the frontend ignore stale reads.
    TtsSelectionProgress {
        tab: TabId,
        session: u64,
        index: u32,
    },
    /// Subprocess for `tab` exited with the given exit code (`None` if the
    /// child wait returned an error, or if we synthesize the signal from a
    /// spawn-time failure where there is no process to exit). Phase 4 routes
    /// this per-kind: AI tabs go to Error, Shell tabs go to the closed
    /// sub-state with the code surfaced in the overlay.
    SubprocessExited {
        tab: TabId,
        code: Option<i32>,
        /// **Which start of this tab exited** (V39 review R-5).
        ///
        /// Signals reach the state manager through an mpsc, so an exit emitted
        /// while a tab was being restarted can be HANDLED after the restart has
        /// already re-seeded the activity mirror — re-latching `exited` on a
        /// process that is running, and refusing the tab as a delegation worker
        /// forever. The generation is taken from
        /// [`TabActivity::begin_start`](crate::state::TabActivity::begin_start)
        /// before the spawn and carried by the waiter that observes THAT
        /// child's exit, so a late one is recognisable rather than merely
        /// early.
        start_gen: u64,
    },
    AudioError {
        tab: TabId,
    },
    TtsError {
        tab: TabId,
    },
    ErrorAcknowledged {
        tab: TabId,
    },
    /// Compose-overlay textarea content crossed the empty/non-empty edge.
    /// Always routed to the active tab (compose targets whoever is on
    /// screen).
    ComposeContentChanged {
        tab: TabId,
        non_empty: bool,
    },
    /// User activated a tab (click or Ctrl+N). Updates `active` and
    /// broadcasts so the frontend can swap avatar/terminal visuals.
    TabActivated {
        tab: TabId,
    },
    /// Permission detector saw a known prompt pattern in the rendered tail.
    /// Sets `awaiting_permission` on the tab; does NOT drive the avatar
    /// state machine.
    PermissionPromptDetected {
        tab: TabId,
    },
    /// Permission detector observed the previously-matched pattern leave the
    /// rendered tail. Clears `awaiting_permission`.
    PermissionPromptResolved {
        tab: TabId,
    },
    /// Detector saw a question-pattern match in the rendered tail (e.g.
    /// Claude Code's AskUserQuestion multi-option prompt). Sets
    /// `awaiting_question` on the tab; mirrors the permission path but
    /// drives a separate notification template.
    QuestionPromptDetected {
        tab: TabId,
    },
    /// Detector observed the previously-matched question pattern leave the
    /// rendered tail. Clears `awaiting_question`.
    QuestionPromptResolved {
        tab: TabId,
    },
    /// A Shell tab's subprocess has been (re)spawned after a previous exit.
    /// Clears the `closed` flag and emits `TabClosedStateChanged { closed:
    /// false }`. AI tabs don't use this — they have no closed sub-state.
    ShellRestarted {
        tab: TabId,
    },
    /// A new tab has been registered with the runtime (M2's
    /// `create_shell_tab`). The state manager allocates a `TabState`
    /// entry, an input-length counter, and emits `StateEvent::TabCreated`
    /// so the frontend mirrors the addition into its tabs store.
    TabAdded {
        meta: TabMeta,
        position: usize,
    },
    /// A tab has been removed from the runtime (M2's `close_tab`). The
    /// state manager drops its `TabState`, drops the input-length counter,
    /// and emits `StateEvent::TabClosed`.
    TabRemoved {
        tab: TabId,
    },
    /// A tab's display name was changed (M2's `rename_tab` /
    /// `reconfigure_shell_tab`). The state manager updates its name and
    /// emits `StateEvent::TabRenamed`.
    TabRenameRequested {
        tab: TabId,
        name: String,
    },
    /// A Shell tab's spawn failed at launch in a way that is not a runtime
    /// crash — typically the configured command no longer resolves on PATH
    /// or its file no longer exists. Routes the tab to the closed sub-
    /// state with a custom message that the frontend overlay shows in
    /// place of "Shell exited (code N)". M3 of v3 fires this from the
    /// registry's start path when `build_launch_spec` returns a
    /// `CommandNotFound`.
    ShellLaunchFailed {
        tab: TabId,
        message: String,
    },
}

impl StateSignal {
    pub fn tab(&self) -> TabId {
        match self {
            Self::UserKeystroke { tab }
            | Self::UserSubmit { tab }
            | Self::HarnessOutputStarted { tab }
            | Self::HarnessOutputStopped { tab }
            | Self::SubagentsActiveChanged { tab, .. }
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
    StateChanged {
        tab: TabId,
        state: AvatarState,
    },
    ActiveTabChanged {
        tab: TabId,
    },
    /// Read-along progress for a Ctrl+right-click selection read. `index` is
    /// the sentence chunk now beginning playback; `index == chunk_count`
    /// signals the whole selection finished. Wire tag: `tts-selection-progress`.
    TtsSelectionProgress {
        tab: TabId,
        session: u64,
        index: u32,
    },
    AwaitingPermissionChanged {
        tab: TabId,
        awaiting: bool,
    },
    AwaitingQuestionChanged {
        tab: TabId,
        awaiting: bool,
    },
    DoneWhileAwayChanged {
        tab: TabId,
        done: bool,
    },
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
    TabClosed {
        tab: TabId,
    },
    /// A tab's display name was updated. Triggered by both `rename_tab` and
    /// `reconfigure_shell_tab` when the latter's `name` field changed.
    TabRenamed {
        tab: TabId,
        name: String,
    },
}

/// Wire-format projection of `TabKind` for the `TabCreated` event. The
/// frontend only needs to know whether a tab is a Shell, an AI tool, or (V14
/// Phase F) a Preview to gate close-button rendering and pane-body routing;
/// matching the internal `TabKind` shape one-to-one would have leaked the
/// (now removed) `AiToolKind` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabKindWire {
    AiTool,
    Shell,
    Preview,
}

impl From<&TabKind> for TabKindWire {
    fn from(k: &TabKind) -> Self {
        match k {
            TabKind::AiTool => TabKindWire::AiTool,
            TabKind::Shell => TabKindWire::Shell,
            TabKind::Preview => TabKindWire::Preview,
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
    /// Set true between `HarnessOutputStarted` and `HarnessOutputStopped`.
    /// Lets `Speaking → TtsPlaybackStopped` fall back to Thinking instead
    /// of Idle when Claude is still emitting output (the TTS tag was a
    /// commentary tag, not a final answer). Always false for Shell tabs.
    harness_output_active: bool,
    /// True while one or more `Task` sub-agents are in flight, per the
    /// transcript (`SubagentsActiveChanged`). Like `harness_output_active` it
    /// blocks the settle to Idle — while agents run the parent's `esc to
    /// interrupt` footer blinks, and neither the marker nor a byte pause means
    /// the turn is done. Always false for Shell tabs.
    subagents_active: bool,
    /// When the sub-agent stall backstop first observed this tab wedged
    /// (Thinking + `subagents_active` + parent output quiet). `None` whenever that
    /// condition doesn't hold; the tick sweep sets it on entry and forces Idle
    /// once it has held long enough. Guards against a `Task` whose result never
    /// arrives (Esc-interrupt, dropped edge) pinning the avatar in Thinking.
    subagents_stall_since: Option<Instant>,
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
            harness_output_active: false,
            subagents_active: false,
            subagents_stall_since: None,
        }
    }
}

/// Everything the state manager is wired to at startup, other than the app
/// handle and its own signal channel.
///
/// One struct rather than six positional arguments: four of these are cloneable
/// handles whose types say nothing about their role, so a transposed pair at
/// the single call site in `main` would compile and mis-wire the manager
/// silently.
pub struct StateManagerWiring {
    /// The same `StateEvent`s emitted to the frontend, for in-process
    /// subscribers (e.g. the notification manager) that must react to state
    /// edges without going through the IPC layer.
    pub state_events: broadcast::Sender<StateEvent>,
    /// The readable mirror of each tab's input length.
    pub input_lengths: InputLengths,
    /// V39 Phase B: the readable mirror of the prompt/burst/exit flags. Handed
    /// in rather than owned here for the same reason `input_lengths` is — the
    /// IPC layer and the delegation engine hold the other end.
    pub activity: TabActivity,
    /// Every tab the manager tracks (kind + name); the manager keys its per-tab
    /// state map by `TabId` from this list, in this order.
    pub tab_metas: Vec<TabMeta>,
    /// The tab whose avatar is displayed at startup.
    pub initial_active: TabId,
    /// The per-tab AI-TTS suppression mirror.
    pub ai_tts_suppressed: crate::tts::AiTtsSuppressed,
}

/// Spawn the state-manager task. The channel is created at app startup so
/// AppState can hold a clone of the sender before the AppHandle exists.
pub fn spawn_state_manager(
    app: AppHandle,
    rx: mpsc::Receiver<StateSignal>,
    wiring: StateManagerWiring,
) {
    tauri::async_runtime::spawn(async move {
        run(app, rx, wiring).await;
    });
}

async fn run(app: AppHandle, mut rx: mpsc::Receiver<StateSignal>, wiring: StateManagerWiring) {
    let StateManagerWiring {
        state_events,
        input_lengths,
        activity,
        tab_metas,
        initial_active,
        ai_tts_suppressed,
    } = wiring;
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

                // V39 Phase B: mirror the prompt / output-burst / exit edges
                // FIRST, ahead of every `continue` below, so a signal handled
                // by an early return still reaches the readers above the seam
                // (`crate::delegation`'s preflight and wait loop). Read-only
                // fold, no side effects, cannot reject a signal — the loop
                // behaves identically whether this line runs or not.
                activity.note_signal(&signal);

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
                        // V39 Phase B: a fresh subprocess starts with clean
                        // flags — this is also what clears a latched `exited`
                        // when a tab is restarted into the same id.
                        activity.reset(&meta.id);
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
                        activity.forget(&tab);
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
                if let StateSignal::HarnessOutputStarted { tab } = &signal {
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
                if let StateSignal::SubprocessExited { tab, code, .. } = &signal {
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
                    StateSignal::HarnessOutputStarted { .. } => {
                        ts.harness_output_active = true;
                    }
                    StateSignal::HarnessOutputStopped { .. } => {
                        ts.harness_output_active = false;
                    }
                    StateSignal::SubagentsActiveChanged { active, .. } => {
                        ts.subagents_active = *active;
                    }
                    // Reset the output-active flag on any error edge / its
                    // acknowledgment. A HarnessOutputStarted with no matching
                    // Stopped (the subprocess crashed or exited mid-output —
                    // the normal exit path) would otherwise leave the flag
                    // stuck true, so a later normal speech cycle resolves to
                    // Thinking instead of Idle (avatar sticks; no idle
                    // announcement). Runs before `transition()` below. Clear
                    // `subagents_active` too — a crash mid-agent-run would
                    // otherwise leave the avatar wedged in Thinking forever.
                    StateSignal::SubprocessExited { .. }
                    | StateSignal::AudioError { .. }
                    | StateSignal::TtsError { .. }
                    | StateSignal::ErrorAcknowledged { .. } => {
                        ts.harness_output_active = false;
                        ts.subagents_active = false;
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
                        ts.harness_output_active,
                        ts.subagents_active,
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
                    // Agents-stall backstop. Recover a tab wedged in Thinking by
                    // an `subagents_active` that never cleared (Task result missing
                    // after an Esc-interrupt, unparseable, or its edge dropped).
                    // Only arms while the parent is producing NO output — a live
                    // turn keeps `harness_output_active` true via the ~1 Hz footer
                    // repaint, so this can't clip real work. See
                    // `subagents_stall_timeout`.
                    if ts.avatar_state == AvatarState::Thinking
                        && ts.subagents_active
                        && !ts.harness_output_active
                    {
                        let since = *ts.subagents_stall_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= subagents_stall_timeout(tab, &app) {
                            info!(?tab, from = ?ts.avatar_state, to = ?AvatarState::Idle, signal = "AgentsStallTimeout", "avatar state");
                            ts.subagents_active = false;
                            ts.subagents_stall_since = None;
                            ts.avatar_state = AvatarState::Idle;
                            emit_state(&app, &state_events, tab.clone(), ts.avatar_state);
                            if *tab != active && !ts.done_while_away {
                                ts.done_while_away = true;
                                info!(?tab, "done while away: set (agents stall)");
                                emit_done_while_away(&app, &state_events, tab.clone(), true);
                            }
                            continue;
                        }
                    } else {
                        ts.subagents_stall_since = None;
                    }

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
                    ts.harness_output_active = false;
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
    harness_output_active: bool,
    subagents_active: bool,
) -> AvatarState {
    use AvatarState::*;
    use StateSignal::*;

    // Claude is still working — the marker/byte stream OR an in-flight
    // sub-agent. Either one blocks the settle to Idle.
    let still_working = harness_output_active || subagents_active;

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
            } else if still_working {
                // TTS tag was an interstitial comment ("about to do X");
                // Claude is still producing output (or a sub-agent is in
                // flight), so go back to Thinking instead of falsely
                // announcing Idle.
                Thinking
            } else {
                Idle
            }
        }
        (Speaking, _) => Speaking,

        (Thinking, TtsPlaybackStarted { .. }) => Speaking,
        // Output stopped, but hold Thinking while sub-agents are still running
        // — their results haven't landed, so the turn isn't done.
        (Thinking, HarnessOutputStopped { .. }) => {
            if subagents_active {
                Thinking
            } else {
                Idle
            }
        }
        // The last agent finished (or a crash cleared the flag). Settle to Idle
        // only if Claude isn't also mid-output; otherwise stay Thinking and let
        // the eventual HarnessOutputStopped release it.
        (Thinking, SubagentsActiveChanged { .. }) => {
            if still_working {
                Thinking
            } else {
                Idle
            }
        }
        (Thinking, _) => Thinking,

        (Listening, UserSubmit { .. }) => Thinking,
        (Listening, TtsPlaybackStarted { .. }) => Speaking,
        (Listening, _) => Listening,

        (Idle, UserKeystroke { .. }) => Listening,
        (Idle, TtsPlaybackStarted { .. }) => Speaking,
        // Claude began producing output without a fresh submit (resumed
        // session, slash command, hook-driven turn). The marker-driven
        // HarnessOutputStarted is reliable enough to surface Thinking.
        (Idle, HarnessOutputStarted { .. }) => Thinking,
        // Agents launched while somehow Idle (out-of-band signal beat the
        // marker) — surface Thinking so the run doesn't look finished.
        (Idle, SubagentsActiveChanged { active, .. }) if *active => Thinking,
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
    dispatch(
        app,
        bcast,
        StateEvent::AwaitingPermissionChanged { tab, awaiting },
    );
}

fn emit_awaiting_question(
    app: &AppHandle,
    bcast: &broadcast::Sender<StateEvent>,
    tab: TabId,
    awaiting: bool,
) {
    dispatch(
        app,
        bcast,
        StateEvent::AwaitingQuestionChanged { tab, awaiting },
    );
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

fn emit_tab_closed_event(app: &AppHandle, bcast: &broadcast::Sender<StateEvent>, tab: TabId) {
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
        TabId::from_str("claude")
    }

    fn t(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, false, false, false, false)
    }

    fn t_with_input(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, true, false, false, false)
    }

    fn t_composing(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, false, true, false, false)
    }

    fn t_with_output(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, false, false, true, false)
    }

    fn t_with_agents(current: AvatarState, signal: StateSignal) -> AvatarState {
        transition(current, &signal, false, false, false, true)
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
        assert_eq!(t(Thinking, HarnessOutputStopped { tab: tab() }), Idle);
    }

    #[test]
    fn idle_claude_output_starts_thinking() {
        // Marker-driven HarnessOutputStarted surfaces Thinking even without a
        // fresh UserSubmit (resumed session, slash command, hook turn).
        assert_eq!(t(Idle, HarnessOutputStarted { tab: tab() }), Thinking);
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
            assert_eq!(
                t(
                    s,
                    SubprocessExited {
                        tab: tab(),
                        code: None,
                        start_gen: 0
                    }
                ),
                Error
            );
            assert_eq!(t(s, AudioError { tab: tab() }), Error);
            assert_eq!(t(s, TtsError { tab: tab() }), Error);
        }
    }

    #[test]
    fn idle_compose_non_empty_listens() {
        assert_eq!(
            t(
                Idle,
                ComposeContentChanged {
                    tab: tab(),
                    non_empty: true
                }
            ),
            Listening,
        );
    }

    #[test]
    fn idle_compose_empty_stays_idle() {
        assert_eq!(
            t(
                Idle,
                ComposeContentChanged {
                    tab: tab(),
                    non_empty: false
                }
            ),
            Idle
        );
    }

    #[test]
    fn compose_does_not_preempt_higher_states() {
        assert_eq!(
            t(
                Thinking,
                ComposeContentChanged {
                    tab: tab(),
                    non_empty: true
                }
            ),
            Thinking,
        );
        assert_eq!(
            t(
                Speaking,
                ComposeContentChanged {
                    tab: tab(),
                    non_empty: true
                }
            ),
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
                true,  // harness_output_active
                false, // subagents_active
            ),
            Listening
        );
    }

    #[test]
    fn thinking_output_stopped_holds_while_agents_run() {
        // The parent's `esc to interrupt` footer blinked out (HarnessOutputStopped)
        // while sub-agents are still in flight — hold Thinking, don't settle to
        // Idle (this is the flicker + repeated-"idle" bug).
        assert_eq!(
            t_with_agents(Thinking, HarnessOutputStopped { tab: tab() }),
            Thinking
        );
        // With no agents running the same signal settles to Idle as before.
        assert_eq!(t(Thinking, HarnessOutputStopped { tab: tab() }), Idle);
    }

    #[test]
    fn agents_finishing_settles_to_idle_when_output_quiet() {
        // Last agent's result landed (subagents_active already flipped false in the
        // caller) and Claude isn't mid-output — settle to Idle.
        assert_eq!(
            t(
                Thinking,
                SubagentsActiveChanged {
                    tab: tab(),
                    active: false
                }
            ),
            Idle
        );
    }

    #[test]
    fn agents_finishing_holds_thinking_while_output_active() {
        // Agents done but Claude is streaming its final answer — stay Thinking
        // and let the eventual HarnessOutputStopped release to Idle.
        assert_eq!(
            t_with_output(
                Thinking,
                SubagentsActiveChanged {
                    tab: tab(),
                    active: false
                }
            ),
            Thinking
        );
    }

    #[test]
    fn agents_launching_from_idle_surfaces_thinking() {
        // Out-of-band agent signal beat the marker while Idle — show Thinking.
        assert_eq!(
            t(
                Idle,
                SubagentsActiveChanged {
                    tab: tab(),
                    active: true
                }
            ),
            Thinking
        );
    }

    #[test]
    fn speaking_tts_stop_holds_thinking_while_agents_run() {
        // A cross-tab notification's playback ends mid-agent-run: fall back to
        // Thinking, not Idle, because agents are still outstanding.
        assert_eq!(
            t_with_agents(Speaking, TtsPlaybackStopped { tab: tab() }),
            Thinking
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
            TabId::from_str("claude"),
            TabId::from_str("claude-local"),
            TabId::from_str("opencode"),
            TabId::Ai("ai-1234".to_string()),
            TabId::Shell("shell-1".to_string()),
            TabId::Shell("user-bash".to_string()),
            TabId::ToolActivity,
            TabId::Events,
        ] {
            let s = serde_json::to_string(&id).unwrap();
            let back: TabId = serde_json::from_str(&s).unwrap();
            assert_eq!(id, back);
        }
    }

    /// #51: `events` is a reserved dashboard, not a user shell — so the close
    /// `×` refuses it, no PTY is spawned for it, and it is `builtin`. Pinned
    /// because the id is an ordinary-looking word: without the `from_str` arm
    /// it would silently be a closable Shell tab that tries to launch a shell.
    #[test]
    fn events_id_is_a_reserved_dashboard() {
        let id = TabId::from_str("events");
        assert_eq!(id, TabId::Events);
        assert!(id.is_reserved_dashboard());
        assert!(id.is_builtin());
        assert_eq!(id.kind(), TabKind::Shell);
        // Additive: Tool Activity is still its own reserved dashboard.
        assert!(TabId::from_str("tool-activity").is_reserved_dashboard());
    }

    #[test]
    fn retired_code_audit_id_routes_to_shell() {
        // The V23 Code Audit reserved tab was folded into Tool Activity as
        // the "Code audit" section (schema v27); its wire id no longer maps
        // to a reserved variant. A stray persisted id parses as a plain Shell
        // (closable, not a dashboard) — and the v26 → v27 migration prunes it
        // before it's ever seeded.
        let id = TabId::from_str("code-audit");
        assert_eq!(id, TabId::Shell("code-audit".to_string()));
        assert!(!id.is_reserved_dashboard());
        assert!(!id.is_builtin());
    }

    #[test]
    fn retired_code_quality_id_routes_to_shell() {
        // The V25 Code Quality reserved tab was folded into Code Audit as a
        // sub-tab; its wire id no longer maps to a reserved variant. A stray
        // persisted id parses as a plain Shell (closable, not a dashboard) —
        // and the settings integrity pass prunes it before it's ever seeded.
        let id = TabId::from_str("code-quality");
        assert_eq!(id, TabId::Shell("code-quality".to_string()));
        assert!(!id.is_reserved_dashboard());
        assert!(!id.is_builtin());
    }

    #[test]
    fn retired_offload_server_id_routes_to_shell() {
        // The V8-03 Offload Server reserved tab was folded into Tool Activity
        // as the "Offload server" section (schema v25); its wire id no longer
        // maps to a reserved variant. A stray persisted id parses as a plain
        // Shell (closable, not a dashboard) — and the v24 → v25 migration
        // prunes it before it's ever seeded.
        let id = TabId::from_str("offload-server");
        assert_eq!(id, TabId::Shell("offload-server".to_string()));
        assert!(!id.is_reserved_dashboard());
        assert!(!id.is_builtin());
    }

    #[test]
    fn retired_graph_view_id_routes_to_shell() {
        // The V15 Graph View reserved tab was folded into Tool Activity as
        // the "Graph view" section (schema v26); its wire id no longer maps
        // to a reserved variant. A stray persisted id parses as a plain Shell
        // (closable, not a dashboard) — and the v25 → v26 migration prunes it
        // before it's ever seeded.
        let id = TabId::from_str("graph-view");
        assert_eq!(id, TabId::Shell("graph-view".to_string()));
        assert!(!id.is_reserved_dashboard());
        assert!(!id.is_builtin());
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
        assert_eq!(
            TabId::from_str("opencode"),
            TabId::Harness("opencode"),
            "a reserved built-in id resolves to `Harness`, not `Ai` or `Shell`"
        );
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
        assert!(TabId::from_str("claude").is_builtin());
        assert!(TabId::from_str("opencode").is_builtin());
        assert!(!TabId::Shell("shell-broot".into()).is_builtin());
        assert!(!TabId::Shell("shell-1".into()).is_builtin());
    }

    #[test]
    fn tab_id_wire_format_preserved() {
        assert_eq!(serde_json::to_string(&TabId::from_str("claude")).unwrap(), "\"claude\"");
        assert_eq!(
            serde_json::to_string(&TabId::from_str("claude-local")).unwrap(),
            "\"claude-local\""
        );
        assert_eq!(
            serde_json::to_string(&TabId::from_str("opencode")).unwrap(),
            "\"opencode\""
        );
        assert_eq!(
            serde_json::to_string(&TabId::Shell("shell-1".to_string())).unwrap(),
            "\"shell-1\""
        );
    }

    #[test]
    fn tab_id_kind_mapping() {
        assert_eq!(TabId::from_str("claude").kind(), TabKind::AiTool);
        assert_eq!(TabId::from_str("claude-local").kind(), TabKind::AiTool);
        assert_eq!(TabId::from_str("opencode").kind(), TabKind::AiTool);
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
            code: None,
            start_gen: 0
        }));
        assert!(!is_error_edge(&UserKeystroke { tab: tab() }));
        assert!(!is_error_edge(&UserSubmit { tab: tab() }));
        assert!(!is_error_edge(&TtsPlaybackStarted { tab: tab() }));
    }

    // ---- V39 Phase A: per-tab read-only state -------------------------------

    fn other() -> TabId {
        TabId::from_str("opencode")
    }

    #[test]
    fn read_only_is_absent_until_something_sets_it() {
        let ro = ReadOnlyTabs::default();
        assert_eq!(ro.read_only(&tab()), None);
    }

    #[test]
    fn the_user_lock_is_seeded_from_the_persisted_set() {
        let ro = ReadOnlyTabs::seeded([tab()]);
        assert_eq!(ro.read_only(&tab()), Some(ReadOnlySource::User));
        assert_eq!(ro.read_only(&other()), None);
    }

    #[test]
    fn the_user_lock_toggles_both_ways() {
        let ro = ReadOnlyTabs::default();
        ro.set_user(&tab(), true);
        assert_eq!(ro.read_only(&tab()), Some(ReadOnlySource::User));
        ro.set_user(&tab(), false);
        assert_eq!(ro.read_only(&tab()), None);
    }

    /// The two sources are independent: ending a delegation must not lift a
    /// lock the user set by hand (and vice versa).
    #[test]
    fn clearing_the_driven_lock_leaves_the_user_lock_standing() {
        let ro = ReadOnlyTabs::default();
        ro.set_user(&tab(), true);
        ro.set_driven(&tab(), Some(other()));
        assert_eq!(
            ro.read_only(&tab()),
            Some(ReadOnlySource::Driven { by: other() }),
            "driven wins while both hold"
        );
        ro.set_driven(&tab(), None);
        assert_eq!(
            ro.read_only(&tab()),
            Some(ReadOnlySource::User),
            "the user's own lock survived the delegation"
        );
    }

    #[test]
    fn clearing_the_user_lock_leaves_a_running_delegation_locked() {
        let ro = ReadOnlyTabs::default();
        ro.set_driven(&tab(), Some(other()));
        ro.set_user(&tab(), false);
        assert_eq!(
            ro.read_only(&tab()),
            Some(ReadOnlySource::Driven { by: other() })
        );
    }

    /// `read_only` is a persisted field, so it can also move through the
    /// Settings window / a project overlay / a hand edit. The runtime map
    /// follows the file rather than becoming a second source of truth.
    #[test]
    fn sync_users_follows_settings_in_both_directions() {
        let ro = ReadOnlyTabs::seeded([tab()]);
        ro.sync_users([other()]);
        assert_eq!(ro.read_only(&tab()), None, "unticked in settings, unlocked");
        assert_eq!(ro.read_only(&other()), Some(ReadOnlySource::User));
    }

    #[test]
    fn sync_users_does_not_disturb_a_driven_tab() {
        let ro = ReadOnlyTabs::default();
        ro.set_driven(&tab(), Some(other()));
        ro.sync_users(std::iter::empty());
        assert_eq!(
            ro.read_only(&tab()),
            Some(ReadOnlySource::Driven { by: other() }),
            "settings has no opinion about the engine's transient lock"
        );
    }

    /// **A standing prompt opens the keyboard whichever lock is on the tab**
    /// (locked decision 5, V39 review M-5).
    ///
    /// The relaxation used to be "clear `driven_by`", so a tab that also
    /// carried the user's own sticky lock fell straight back to
    /// `ReadOnlySource::User` and stayed refused — the one prompt only the user
    /// can answer could not be answered, and the flight ran to its deadline
    /// reporting "worker awaiting permission". Both sources are asserted here
    /// because the defect was visible in only one of them.
    #[test]
    fn a_standing_prompt_opens_the_keyboard_for_both_lock_sources() {
        for user_lock in [false, true] {
            let ro = ReadOnlyTabs::default();
            ro.set_user(&tab(), user_lock);
            ro.set_driven(&tab(), Some(other()));
            assert_eq!(
                ro.read_only(&tab()),
                Some(ReadOnlySource::Driven { by: other() })
            );

            ro.set_prompt_relaxed(&tab(), true);
            assert_eq!(
                ro.read_only(&tab()),
                None,
                "user_lock={user_lock}: the prompt must be answerable"
            );

            ro.set_prompt_relaxed(&tab(), false);
            assert_eq!(
                ro.read_only(&tab()),
                Some(ReadOnlySource::Driven { by: other() }),
                "the lock re-engages on the falling edge"
            );
        }
    }

    /// The relaxation cannot outlive the flight that justified it — and the
    /// engine's lock is still recorded throughout it, so the banner keeps
    /// naming the driver and Take over keeps working.
    #[test]
    fn a_relaxation_dies_with_the_flight_and_never_hides_the_driver() {
        let ro = ReadOnlyTabs::default();
        ro.set_user(&tab(), true);
        ro.set_driven(&tab(), Some(other()));
        ro.set_prompt_relaxed(&tab(), true);
        // Released mid-prompt (a take-over, a timeout): the user's own lock is
        // back, not silently lifted by a relaxation nobody cleared.
        ro.set_driven(&tab(), None);
        assert_eq!(ro.read_only(&tab()), Some(ReadOnlySource::User));

        // …and a tab whose only state was a relaxation leaves no row behind.
        let ro = ReadOnlyTabs::default();
        ro.set_driven(&tab(), Some(other()));
        ro.set_prompt_relaxed(&tab(), true);
        ro.set_driven(&tab(), None);
        assert_eq!(ro.read_only(&tab()), None);
    }

    #[test]
    fn forgetting_a_closed_tab_drops_its_row() {
        let ro = ReadOnlyTabs::seeded([tab()]);
        ro.forget(&tab());
        assert_eq!(ro.read_only(&tab()), None);
    }

    /// Every refusal names a reason the user can act on — never an empty
    /// string, and never a bare tab id when a display name is known.
    #[test]
    fn every_read_only_source_has_a_reason() {
        assert_eq!(ReadOnlySource::User.reason(None), "read-only (user)");
        let driven = ReadOnlySource::Driven { by: other() };
        assert_eq!(driven.reason(Some("api-work")), "driven by api-work");
        assert_eq!(
            driven.reason(None),
            format!("driven by {}", other().as_str()),
            "no name available falls back to the id, not to nothing"
        );
        assert_eq!(
            driven.reason(Some("   ")),
            format!("driven by {}", other().as_str()),
            "a blank name is not a name"
        );
        for reason in [
            ReadOnlySource::User.reason(None),
            driven.reason(Some("api-work")),
            driven.reason(None),
        ] {
            assert!(!reason.trim().is_empty());
        }
    }
}
