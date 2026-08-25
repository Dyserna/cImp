//! The settings use cases: read the live snapshot, apply the Settings
//! window's save, and the writers that deliberately bypass the live snapshot.
//!
//! ## What the A1 settings run found
//!
//! `settings_update` is the command with the most collaborators in the crate,
//! and the inventory's "AppHandle → EventSink" summary describes exactly one
//! of them. The other four are the reason it was reachable only through a
//! WebView:
//!
//! 1. *Emitter* — one `app.emit("ai-tab-restart-hint", …)` at the very end.
//!    That is [`EventSink`], and it is one line.
//! 2. *The STT worker's control channel* (`state.stt`) — a plain
//!    [`SttHandle`](crate::stt::SttHandle), already UI-neutral, already
//!    constructible in a test by its own `new()`. It only looked coupled
//!    because it was reached through `State<'_, AppState>`.
//! 3. *The tab registry + the state-signal channel*, for the reserved feature
//!    tabs this save materialises or removes. Both are ordinary in-process
//!    handles; the helper that used them took `&AppState` purely because its
//!    only caller had one, which is why it now lives beside the rest of the
//!    tab lifecycle as [`crate::service::tabs::sync_reserved_feature_tab`].
//! 4. *The warm code-graph index*, for the `graph.ignore` resync edge. That one
//!    is another domain's whole capability, so it is the run's only new trait —
//!    [`GraphIndexHost`](crate::service::sink::GraphIndexHost), one method, the
//!    same shape and rationale as `WebviewHost`. (At A1 it was also literally
//!    unconstructible in a test, because `GraphService::new` took an
//!    `AppHandle`. V42 Phase A2 fixed that; the trait stays for the reason that
//!    outlived it — see its own doc comment.)
//!
//! ## What did NOT change
//!
//! [`SettingsService::update`]'s body is the old command body, in the old
//! order: snapshot the pre-update flags (reserved tabs, the STT pair, the
//! effective `graph.ignore`, [`spawn_inject_sig`](crate::tabs::spawn_inject_sig))
//! → one atomic `mutate` running [`apply_incoming_settings`] → the STT edge →
//! the reserved-tab edge under the lifecycle serializer → the ignore edge → the
//! spawn-signature edge and its restart hint. Nothing was reordered, nothing
//! was hoisted into the wrapper, and the machine-scope enforcement and overlay
//! bans stay where they always were, in `settings::persistence`.
//!
//! ## The writers that bypass the handle
//!
//! The prompt-template library, the LLM price table and the harness version
//! records are written straight to the physical global `settings.json`, NOT
//! through [`SettingsHandle`]. That is not an accident to be tidied away — it
//! is what makes them global rather than per-project overlay diffs, and
//! [`apply_incoming_settings`] preserves each of them from the live state for
//! exactly that reason. The free functions at the foot of this module are those
//! writers' use cases; they take no service because they have no handles to
//! borrow.

use std::path::Path;

use tokio::sync::{mpsc, Mutex as TokioMutex};

use crate::error::{AppError, AppResult};
use crate::service::sink::{EventSink, EventSinkExt, GraphIndexHost};
use crate::settings::{
    default_claude_local_tab, default_claude_tab, default_opencode_tab, AiToolTabConfig, Settings,
    SettingsHandle, TabConfig, CLAUDE_LOCAL_TAB_ID, CLAUDE_TAB_ID, OPENCODE_TAB_ID,
};
use crate::state::{StateSignal, TabId};
use crate::stt::SttHandle;
use crate::tabs::TabRegistryHandle;

/// The settings use cases, over borrowed handles — same shape and rationale as
/// [`crate::service::tabs::TabService`].
///
/// Five handles, none of which needs a WebView. The sixth collaborator, the
/// warm graph index, is a per-call `&dyn GraphIndexHost` rather than a field:
/// only [`Self::update`] touches it, and making it a field would force every
/// other caller to have one.
pub struct SettingsService<'a> {
    settings: &'a SettingsHandle,
    registry: &'a TabRegistryHandle,
    signals: &'a mpsc::Sender<StateSignal>,
    /// Serializes the reserved-tab materialize/remove pass against
    /// `create_tab` / `close_tab`, exactly as the command held it.
    serializer: &'a TokioMutex<()>,
    stt: &'a SttHandle,
}

impl<'a> SettingsService<'a> {
    pub fn new(
        settings: &'a SettingsHandle,
        registry: &'a TabRegistryHandle,
        signals: &'a mpsc::Sender<StateSignal>,
        serializer: &'a TokioMutex<()>,
        stt: &'a SttHandle,
    ) -> Self {
        Self {
            settings,
            registry,
            signals,
            serializer,
            stt,
        }
    }

    /// The live in-memory settings snapshot.
    pub fn get(&self) -> Settings {
        self.settings.current()
    }

    /// V21 F7: merge the curated read-only command preset (`git` + `cargo`
    /// metadata/tree, with the `cargo` policy that pins it to those verbs) into
    /// the live offload settings, and return the updated `Settings` so the
    /// Settings window can refresh its snapshot. Idempotent — re-invoking adds
    /// nothing and never clobbers a user-authored `cargo` policy (see
    /// [`crate::settings::merge_readonly_preset`]). A merge-into-settings
    /// action: the user sees exactly what got added in the allowlist / policy
    /// editors and can prune it. Mutates atomically under the settings lock
    /// (broadcast + debounced save), like every other settings write.
    pub fn enable_readonly_commands(&self) -> Settings {
        self.settings.mutate(|s| {
            crate::settings::merge_readonly_preset(
                &mut s.offload.command_allowlist,
                &mut s.offload.command_policies,
            );
        });
        self.settings.current()
    }

    /// Apply the Settings window's whole-struct save.
    ///
    /// The highest-risk method in the service layer: five edges are computed
    /// across one atomic write, and each of them is a bug that shipped once.
    /// See the module docs for the ordering contract — it is the command's, in
    /// the command's order.
    pub async fn update(
        &self,
        mut settings: Settings,
        graph: &dyn GraphIndexHost,
        events: &dyn EventSink,
    ) -> AppResult<()> {
        // Re-point bundled avatar videos at the (possibly just-changed) theme's
        // on-disk subfolder before broadcasting, so switching theme switches the
        // avatar. User overrides are preserved; see `apply_portable_avatar_paths`.
        crate::settings::apply_portable_avatar_paths(&mut settings);

        // The reserved feature tabs and the settings flag gating each. ONE table
        // drives both the pre-update snapshot and the post-update live
        // materialize/remove below, so a new reserved tab can't be snapshotted
        // but not synced (or vice versa) — the miss used to surface as "the tab
        // only appears after a restart". The integrity pass that normally owns
        // these tabs only runs at load.
        type ReservedTabFlag = (TabId, fn(&Settings) -> bool);
        const RESERVED_TAB_FLAGS: &[ReservedTabFlag] = &[
            (TabId::GraphMonitor, |s| s.graph.enabled),
            (TabId::Workbench, |s| s.workbench.enabled),
            (TabId::ToolActivity, |s| s.ui.tool_activity_tab),
            (TabId::Events, |s| s.ui.events_tab),
        ];

        // Snapshot the pre-update flags (reserved tabs via the table, plus the
        // STT pair handled separately below) and the effective `graph.ignore`
        // list for the resync edge at the bottom.
        let (was_reserved, was_stt, was_stt_device, was_graph_ignore, was_spawn_sig) = {
            let old = self.settings.current();
            let was: Vec<bool> = RESERVED_TAB_FLAGS
                .iter()
                .map(|(_, flag)| flag(&old))
                .collect();
            (
                was,
                old.stt.enabled,
                old.stt.device,
                normalized_ignore(&old.graph.ignore),
                crate::tabs::spawn_inject_sig(&old),
            )
        };

        // The Settings window holds a full snapshot and replaces wholesale, but it
        // never edits `layout` or `session` (those are driven only by the main
        // window's save_layout / set_active_tab commands). Preserve them from the
        // live state so a stale snapshot from the settings webview can't clobber a
        // layout the user just dragged or the active-tab the main window just set.
        // `tabs` IS legitimately edited here (TabBar reorder, ConfigureTabDialog,
        // reset-to-defaults), so it is taken from the incoming struct.
        //
        // V14 code-review fix (HIGH, data loss): `prompt_templates` +
        // `templates_seeded` are ALSO out-of-band fields — they're written only
        // by `compose_templates_global_set` -> `write_global_prompt_templates`,
        // straight to the physical global `settings.json`, bypassing this
        // `SettingsHandle` entirely (see that command's doc comment). The
        // Settings window's generic snapshot can easily be stale for these two
        // fields (e.g. fetched before a Compose-section edit), and without this
        // preservation a completely unrelated save (theme, a toggle, ...) would
        // stomp the live in-memory copy with that stale value, which a later
        // read (or a diff-and-persist elsewhere) could then present as the
        // template library having reverted or lost entries. Preserve both here,
        // exactly like `layout`/`session`, so the dedicated compose IPC stays
        // the only writer of the template library.
        self.settings
            .mutate(move |cur| apply_incoming_settings(cur, settings));

        // On an `stt.enabled` edge, load or unload the Whisper model so the toggle
        // actually frees/reclaims memory (not just hides the record button). When
        // the feature stays enabled but the device (GPU↔CPU) changed, preload
        // reloads the model on the new device — `needs_reload` in the worker
        // detects the device mismatch and rebuilds the context, freeing the old
        // device's memory. (Unlike TTS, the STT worker isn't a settings subscriber;
        // it's driven by these control messages, so the reload must be nudged here.)
        let now = self.settings.current();
        if now.stt.enabled != was_stt {
            if now.stt.enabled {
                self.stt.preload();
            } else {
                self.stt.unload();
            }
        } else if now.stt.enabled && now.stt.device != was_stt_device {
            self.stt.preload();
        }

        // On an actual enable/disable edge, mirror the change into the runtime so
        // the reserved tab appears/disappears live (tab bar + pane placement).
        let now_reserved: Vec<bool> = RESERVED_TAB_FLAGS
            .iter()
            .map(|(_, flag)| flag(&now))
            .collect();
        if now_reserved != was_reserved {
            // Serialize against create/close_tab while we touch the registry.
            let _serializer = self.serializer.lock().await;
            for (i, (tab, _)) in RESERVED_TAB_FLAGS.iter().enumerate() {
                if now_reserved[i] != was_reserved[i] {
                    crate::service::tabs::sync_reserved_feature_tab(
                        self.settings,
                        self.registry,
                        self.signals,
                        tab.clone(),
                        now_reserved[i],
                    )
                    .await;
                }
            }
        }

        // On an effective `graph.ignore` edge, reconcile the live index: drop
        // newly-excluded files, index newly-included ones. Compared normalized
        // (trimmed, empties out) but ORDER-SENSITIVE — gitignore semantics are
        // last-match-wins, so reordering `!` re-includes is a real change. The
        // resync is a no-op walk when the edit doesn't affect any indexed file.
        if now.graph.enabled && normalized_ignore(&now.graph.ignore) != was_graph_ignore {
            graph.spawn_ignore_resync();
        }

        // On a spawn-injection edge — anything baked into an AI tab only at
        // launch: the advertised MCP server set (`--mcp-config` for Claude,
        // `OPENCODE_CONFIG_CONTENT` for OpenCode), the guidance addendum, the
        // `--settings` statusline/hooks overlay, the local-provider env, the
        // OpenCode plugin flags/provider — tell the main window to show a restart
        // hint: a running AI tab keeps its old injection until restarted. The V26
        // field report: Code Audit enabled mid-session advertised nothing, the
        // agent went probing for a CLI and opened GUI instances. Payload = the
        // consumer names whose spawn injection changed.
        let now_spawn_sig = crate::tabs::spawn_inject_sig(&now);
        if now_spawn_sig != was_spawn_sig {
            // Locked decision 8: iterate the map instead of reading slots 0 and 1.
            // A positional pair meant a harness with no slot got NO restart hint
            // when a spawn-baked setting changed — the exact failure the mechanism
            // exists to prevent, and one that compiled. Phase A made it a
            // registry-sized `PerHarness`; Phase B made it the
            // `BTreeMap<HarnessId, Value>` the decision asks for, and folded every
            // plugin's `spawn_baked` `ext` rows into it automatically, so the flag
            // and its hint are one declaration.
            let consumers: Vec<&'static str> = now_spawn_sig
                .iter()
                .filter(|(h, v)| was_spawn_sig.get(*h) != Some(*v))
                .filter_map(|(h, _)| h.id())
                .collect();
            // Best-effort UI hint — never fail the save over it.
            //
            // BOTH windows (#48, F-x). This used to target `main` only, whose
            // listener is a toast — and the user who just flipped the switch is
            // standing in the SETTINGS window, which never heard the event that
            // exists to tell them their change needs a restart. The Settings window
            // renders it as a per-tab restart hint beside its own Restart buttons;
            // the main window keeps its toast for the case where Settings is
            // already closed.
            //
            // ONE broadcast, not one `emit_to` per window (rc.9 live-verify A1):
            // a JS `listen()` registers with `EventTarget::Any`, and Tauri's
            // `match_any_or_filter` lets an `Any` listener receive EVERY emit
            // regardless of the target it was addressed to — so two targeted
            // emits reached the main window's listener twice and it showed the
            // toast twice. `emit` delivers once to every webview.
            let _ = events.emit(crate::service::events::AI_TAB_RESTART_HINT, &consumers);
        }
        Ok(())
    }
}

/// The body of [`SettingsService::update`]'s read-modify-write, factored out so
/// it can be exercised directly without a `SettingsHandle` at all
/// (`SettingsHandle::mutate` requires the closure to run under its own lock;
/// this is the pure logic that closure runs).
///
/// `incoming` is the Settings-window's full snapshot; `cur` is the live
/// in-memory state. See the call site in [`SettingsService::update`] for why
/// `layout`/`session`/`prompt_templates`/`templates_seeded`/
/// `pricing_seeded_generation` are preserved from `cur` rather than taken from
/// `incoming`.
fn apply_incoming_settings(cur: &mut Settings, mut incoming: Settings) {
    incoming.layout = cur.layout.clone();
    incoming.session = cur.session.clone();
    incoming.prompt_templates = cur.prompt_templates.clone();
    incoming.templates_seeded = cur.templates_seeded;
    // `llm_pricing` is out-of-band exactly like `prompt_templates`: written
    // only by `llm_pricing_set` -> `write_global_llm_pricing`, straight to
    // the physical global file. Preserve it so a stale Settings-window
    // snapshot can't stomp a price edit made through the dedicated IPC.
    incoming.llm_pricing = cur.llm_pricing.clone();
    // F-19: the watermark travels with `llm_pricing` and must be preserved for
    // a sharper reason than the table itself. A Settings-window snapshot that
    // omits the field deserializes it as 0 (its serde default), so taking it
    // from `incoming` would reset the watermark on every settings save — and
    // the next launch would then re-run the top-up and resurrect a built-in
    // row the user had deliberately deleted.
    incoming.pricing_seeded_generation = cur.pricing_seeded_generation;
    // `harness_versions` is likewise out-of-band (V16): written by the OOB
    // transcript tap / tab spawn / `harness_mark_verified`, straight to the
    // physical global file. A stale Settings-window snapshot must not revert
    // a version observation or a Mark-verified. (The persistence layer
    // additionally bans `llm_pricing`/`harness_versions` from project
    // overlays wholesale — the `OverlayStrip::Banned` rows of `MACHINE_SCOPED`
    // in settings/persistence.rs; this list here covers the in-memory round
    // trip, that one the on-disk
    // diff/merge. Keep both in mind when adding an out-of-band field.)
    incoming.harness_versions = cur.harness_versions.clone();
    // V40 Phase B: the same rule, one level down. `Settings::harness` is NOT
    // preserved wholesale — `expose_commands`, `expose_code_audit`, the
    // recorded spike outcome and every plugin `ext` value are what the Settings
    // window is FOR — but the three OUT-OF-BAND fields on each row are
    // (`sync_harness_into` excludes them from the disk write for the same
    // reason). The window's snapshot is taken when it opens; the transcript tap
    // or the auto-verify worker may have written a newer version since, and a
    // save must not revert a version observation or a Mark-verified.
    //
    // V40 review L-1: iterate the REGISTRY, not `cur.harness`. A registered
    // harness with no live row used to take all three straight from the
    // incoming snapshot — and the frontend fabricates them for an absent key
    // (`harnessRow` answers `last_seen: ''`, `auto_verify: null`), so the health
    // panel would read "never auto-verified" until the next restart.
    // `harness_settings` supplies the declared defaults for an absent row, which
    // is the same answer every other reader gets.
    for h in crate::harness::registry::all() {
        let Some(id) = h.id() else {
            continue;
        };
        let live = cur.harness_settings(h).clone();
        let row = incoming
            .harness
            .entry(id.to_string())
            .or_insert_with(|| live.clone());
        row.last_seen = live.last_seen;
        row.last_verified = live.last_verified;
        row.auto_verify = live.auto_verify;
    }
    // A row for a harness this build does not know can only have come from the
    // file, through `cur`; keep it whole rather than letting a window snapshot
    // that never showed it decide its shape.
    for (id, live) in &cur.harness {
        if crate::harness::HarnessId::from_id(id).is_none() {
            incoming
                .harness
                .entry(id.clone())
                .or_insert_with(|| live.clone());
        }
    }
    *cur = incoming;
    // V40 review M-1: the declared parse boundary, on the IPC write path too.
    // The Settings window can post an out-of-enum `SettingKind::Enum` value or a
    // non-object `Json` block (its generic form has an `{:else}` branch), and
    // without this the wrong-typed value would live in memory — and in the file
    // it is saved to — until some later out-of-band read repaired it.
    cur.normalize_harness_settings();
    // Keep the reserved feature tabs (Code Graph monitor / Workbench / ...)
    // present-iff-enabled in the persisted list.
    crate::settings::reconcile_reserved_tabs(cur);
    // V38 Phase E: there is nothing to reconcile for the audit roster any more.
    // The fourteen built-in tools are an embedded manifest read at invocation
    // time, so a settings file that predates one of them simply has no state for
    // it and the registry supplies the manifest default — which is what made the
    // old top-up (`reconcile_audit_tools`, appending missing built-ins to a
    // persisted array) unnecessary rather than merely moved.
    // V21: when a harness declares a config writer that tracks cImp's own
    // offload command, re-derive its snapshot if the primary Local command
    // changed (no-op otherwise). V40 Phase B moved the two settings behind the
    // OpenCode plugin, so this calls the plugin's own sync rather than a method
    // on `OffloadSettings` named after one harness.
    crate::harness::opencode::settings::sync_provider_on_save(cur);
}

/// `graph.ignore` as the backend effectively applies it: trimmed, empty lines
/// dropped (the Settings editor's just-added blank row is not a change).
fn normalized_ignore(globs: &[String]) -> Vec<String> {
    globs
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Per-AI-tab default config. Used by the Settings window's "Reset to
/// default" buttons so the frontend doesn't have to mirror Rust-side
/// tab defaults.
///
/// Only AI tabs have a meaningful "default" in v1.2 — Shell tab defaults
/// depend on the host platform's auto-detected shell, and "reset" on a
/// user-created Shell tab is not a meaningful UX (use the New Shell Tab
/// dialog to spawn a fresh one). Shell ids return an error.
pub fn ai_tool_tab_defaults(tab: &TabId) -> AppResult<AiToolTabConfig> {
    let config = match tab.as_str() {
        CLAUDE_TAB_ID => default_claude_tab(),
        CLAUDE_LOCAL_TAB_ID => default_claude_local_tab(),
        OPENCODE_TAB_ID => default_opencode_tab(),
        other => {
            return Err(AppError::Pty(format!(
                "ai_tool_tab_defaults: tab {other} has no AI defaults"
            )))
        }
    };
    match config {
        TabConfig::AiTool(c) => Ok(c),
        TabConfig::Shell(_) | TabConfig::Preview(_) => Err(AppError::Pty(
            "ai_tool_tab_defaults: reserved id resolved to a non-AI-tool config".into(),
        )),
    }
}

/// V14 Phase A: the compose overlay's `/` picker data source. Resolves the
/// global prompt-template library (from the physical global `settings.json`)
/// against `root`'s project-scope additions (its `.cimp/config.json`
/// overlay's own `prompt_templates` array) by name — a project entry
/// shadows a same-named global one. Deliberately reads both scopes directly
/// off disk rather than through the merged `Settings` the rest of the app
/// uses; see `PromptTemplate`'s doc comment for why the normal deep-merge
/// would silently replace the global list instead of shadowing it.
pub fn resolved_prompt_templates(root: &Path) -> Vec<crate::settings::ResolvedTemplate> {
    let global = crate::settings::read_global_prompt_templates();
    let project = crate::settings::read_project_prompt_templates(root);
    crate::settings::resolve_prompt_templates(global, project)
}

/// V14 Phase A: the Settings window's Compose section reads the raw global
/// list (unshadowed — a template currently shadowed by a project override
/// still needs to be editable here) directly from the physical global file.
pub fn global_prompt_templates() -> Vec<crate::settings::PromptTemplate> {
    crate::settings::read_global_prompt_templates()
}

/// V14 Phase A: the Settings window's Compose section save. Writes straight
/// to the physical global `settings.json` — NOT through
/// [`SettingsService::update`]'s normal per-project overlay diff — so the
/// library really is global regardless of which project this cImp session was
/// launched from. See `settings::persistence::write_global_prompt_templates`'s
/// doc comment.
pub fn set_global_prompt_templates(
    templates: Vec<crate::settings::PromptTemplate>,
) -> AppResult<()> {
    crate::settings::write_global_prompt_templates(templates)
}

/// V14 Phase A: read-only project-scope listing for the Settings window's
/// Compose section (edited by hand in `.cimp/config.json`, not from
/// Settings — matching the milestone's scope rule).
pub fn project_prompt_templates(root: &Path) -> Vec<crate::settings::PromptTemplate> {
    crate::settings::read_project_prompt_templates(root)
}

/// LLM price table for the session-cost popup and its Settings editor. Reads
/// the raw global list directly from the physical global `settings.json`
/// (missing file/key → the seeded Anthropic/Copilot defaults) — same
/// global-only posture as [`global_prompt_templates`].
pub fn llm_pricing() -> Vec<crate::settings::LlmPricingModel> {
    crate::settings::read_global_llm_pricing()
}

/// Save the LLM price table straight to the physical global `settings.json` —
/// NOT through [`SettingsService::update`]'s per-project overlay diff — so
/// provider price edits apply to every project. Mirror of
/// [`set_global_prompt_templates`]; see
/// `settings::persistence::write_global_llm_pricing`'s doc comment.
///
/// Because this bypasses the `SettingsHandle` (and therefore the
/// `settings-changed` broadcast), it emits its own `llm-pricing-changed`
/// event after a successful write so already-open cost surfaces (Code
/// Intelligence Cost/Dashboard cards, Sessions rows) refetch instead of
/// showing a stale table until restart.
pub fn set_llm_pricing(
    pricing: Vec<crate::settings::LlmPricingModel>,
    events: &dyn EventSink,
) -> AppResult<()> {
    crate::settings::write_global_llm_pricing(pricing)?;
    let _ = events.emit(crate::service::events::LLM_PRICING_CHANGED, &());
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::testutil::ScratchDir;
    use super::*;
    use crate::service::sink::testing::{NoGraphIndex, RecordingEventSink};
    use crate::settings::PromptTemplate;
    use crate::state::{TabKind, TabMeta};
    use crate::tabs::TabRegistry;
    use std::sync::{Arc, RwLock};

    /// Everything [`SettingsService`] borrows, owned on the stack — the same
    /// six-handle fixture the tab-lifecycle slice proved, plus the STT handle
    /// (whose `new()` returns the runtime half unstarted, so nothing is
    /// spawned) and minus the two the settings commands never touch.
    struct Fixture {
        settings: SettingsHandle,
        registry: TabRegistryHandle,
        signals: mpsc::Sender<StateSignal>,
        rx: mpsc::Receiver<StateSignal>,
        serializer: TokioMutex<()>,
        stt: SttHandle,
        _stt_runtime: crate::stt::SttRuntime,
        _scratch: ScratchDir,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = ScratchDir::new("setsvc");
            let defaults = Settings::default();
            let settings =
                SettingsHandle::new(defaults.clone(), defaults, scratch.0.clone());
            let (signals, rx) = mpsc::channel::<StateSignal>(64);
            let seed_id = TabId::Shell("shell-seed".to_string());
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
            let (stt, stt_runtime) = SttHandle::new();
            Self {
                settings,
                registry,
                signals,
                rx,
                serializer: TokioMutex::new(()),
                stt,
                _stt_runtime: stt_runtime,
                _scratch: scratch,
            }
        }

        fn service(&self) -> SettingsService<'_> {
            SettingsService::new(
                &self.settings,
                &self.registry,
                &self.signals,
                &self.serializer,
                &self.stt,
            )
        }

        /// Drain the state-signal channel into a list of variant names, which
        /// is what the state manager turns into `tab-created` / `tab-closed` on
        /// the wire.
        fn drain_signals(&mut self) -> Vec<&'static str> {
            let mut out = Vec::new();
            while let Ok(sig) = self.rx.try_recv() {
                out.push(match sig {
                    StateSignal::TabAdded { .. } => "TabAdded",
                    StateSignal::TabRemoved { .. } => "TabRemoved",
                    StateSignal::TabActivated { .. } => "TabActivated",
                    _ => "other",
                });
            }
            out
        }
    }

    /// **Previously "user clicks in the app".** The live-verify recipe is: open
    /// the Settings window, change a setting, save, and check that the main
    /// window sees it — the `settings-changed` broadcast is what every open
    /// surface reconciles from, and a save that lands in memory without
    /// broadcasting looks exactly like a save that worked until the next
    /// reload.
    #[tokio::test]
    async fn settings_update_round_trips_and_broadcasts() {
        let fixture = Fixture::new();
        let mut watch = fixture.settings.subscribe();
        let events = RecordingEventSink::default();

        let mut incoming = fixture.service().get();
        incoming.ui.theme = "future-light".to_string();
        fixture
            .service()
            .update(incoming, &NoGraphIndex::default(), &events)
            .await
            .expect("save");

        // The live snapshot the next `settings_get` would answer with.
        assert_eq!(fixture.service().get().ui.theme, "future-light");
        // …and the broadcast every open window reconciles from.
        let broadcast = watch.try_recv().expect("a settings-changed broadcast");
        assert_eq!(broadcast.ui.theme, "future-light");
    }

    /// **Previously "user clicks in the app".** Flipping a reserved feature
    /// tab's flag has to materialise the tab NOW — the recipe is "enable
    /// Workbench in Settings, look at the tab bar", and the failure it guards
    /// is the one that shipped: the tab only appeared after a restart, because
    /// the integrity pass that owns these tabs runs at load.
    #[tokio::test]
    async fn a_reserved_feature_tab_follows_its_flag_live_in_both_directions() {
        async fn save(fixture: &Fixture, events: &RecordingEventSink, flag: bool) {
            let mut incoming = fixture.service().get();
            incoming.workbench.enabled = flag;
            fixture
                .service()
                .update(incoming, &NoGraphIndex::default(), events)
                .await
                .expect("save");
        }

        let mut fixture = Fixture::new();
        let events = RecordingEventSink::default();

        // Start from off (the default ships it on), so the enable below is a
        // real edge rather than a no-op the assertion would pass vacuously.
        save(&fixture, &events, false).await;
        assert!(!fixture.registry.lock().await.has_tab(&TabId::Workbench));
        fixture.drain_signals();

        save(&fixture, &events, true).await;
        assert!(
            fixture.registry.lock().await.has_tab(&TabId::Workbench),
            "the tab must be live without a restart"
        );
        assert!(
            fixture.drain_signals().contains(&"TabAdded"),
            "the state manager must be told, or no view is ever mounted"
        );

        save(&fixture, &events, false).await;
        assert!(
            !fixture.registry.lock().await.has_tab(&TabId::Workbench),
            "disabling the feature must remove the tab"
        );
        assert!(fixture.drain_signals().contains(&"TabRemoved"));
    }

    /// **Previously "user clicks in the app".** A spawn-baked setting changed
    /// mid-session leaves every running AI tab on its old injection, and the
    /// only thing that tells the user is this hint. The V26 field report is the
    /// regression: Code Audit enabled mid-session advertised nothing and the
    /// agent went probing for a CLI.
    #[tokio::test]
    async fn a_spawn_baked_edit_emits_the_restart_hint() {
        let fixture = Fixture::new();
        let events = RecordingEventSink::default();

        // A no-op save emits nothing…
        let unchanged = fixture.service().get();
        fixture
            .service()
            .update(unchanged, &NoGraphIndex::default(), &events)
            .await
            .expect("save");
        assert!(
            !events
                .events()
                .iter()
                .any(|e| e.event == "ai-tab-restart-hint"),
            "an unchanged save must not nag about restarting"
        );

        // …and a spawn-baked flip does. The flip is asserted to move
        // `spawn_inject_sig` before the save runs, so this test cannot quietly
        // pass by editing something that was never spawn-baked — it is the
        // signature, not a list here, that decides what counts.
        let mut incoming = fixture.service().get();
        let harness = crate::harness::DEFAULT_HARNESS;
        let was = incoming
            .harness_ext(harness, "statusline")
            .as_bool()
            .expect("the `--settings` statusline overlay is a bool row");
        incoming.set_ext(
            harness.token(),
            "statusline",
            serde_json::json!(!was),
        );
        let before = crate::tabs::spawn_inject_sig(&fixture.service().get());
        let after = crate::tabs::spawn_inject_sig(&incoming);
        assert_ne!(before, after, "the flip must move the spawn signature");

        fixture
            .service()
            .update(incoming, &NoGraphIndex::default(), &events)
            .await
            .expect("save");
        assert!(
            events
                .events()
                .iter()
                .any(|e| e.event == "ai-tab-restart-hint"),
            "a spawn-baked edit must announce that running tabs need a restart"
        );
    }

    /// **Previously "user clicks in the app".** The `graph.ignore` resync edge:
    /// the editor's just-added blank row is not a change, and a real edit is.
    #[tokio::test]
    async fn the_ignore_resync_edge_fires_on_real_edits_only() {
        let fixture = Fixture::new();
        let events = RecordingEventSink::default();
        let graph = NoGraphIndex::default();

        let mut on = fixture.service().get();
        on.graph.enabled = true;
        on.graph.ignore = vec!["/gen/".to_string()];
        fixture
            .service()
            .update(on, &graph, &events)
            .await
            .expect("save");
        let after_first = graph.resyncs();

        // A blank row added in the editor normalizes away — no resync.
        let mut blank = fixture.service().get();
        blank.graph.ignore = vec!["/gen/".to_string(), "  ".to_string()];
        fixture
            .service()
            .update(blank, &graph, &events)
            .await
            .expect("save");
        assert_eq!(graph.resyncs(), after_first, "a blank row is not a change");

        // A real glob does.
        let mut real = fixture.service().get();
        real.graph.ignore = vec!["/gen/".to_string(), "*.snap".to_string()];
        fixture
            .service()
            .update(real, &graph, &events)
            .await
            .expect("save");
        assert_eq!(graph.resyncs(), after_first + 1, "a new glob is a change");
    }

    /// The resync edge fires on real changes only: trimming and blank rows
    /// (the editor's just-added empty line) don't count, order does.
    #[test]
    fn normalized_ignore_drops_blanks_keeps_order() {
        let norm =
            |v: &[&str]| normalized_ignore(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            norm(&["/gen/", "", "  ", " *.snap "]),
            vec!["/gen/", "*.snap"]
        );
        assert_ne!(norm(&["/gen/", "!keep"]), norm(&["!keep", "/gen/"]));
    }

    // V14 code-review FIX 1 (HIGH, data loss): `prompt_templates` /
    // `templates_seeded` are written out-of-band by
    // `compose_templates_global_set` (straight to the physical global
    // `settings.json`, bypassing `SettingsHandle`), so the live in-memory
    // copy can legitimately hold templates the Settings window's generic
    // snapshot doesn't know about. Simulate that: `cur` already has
    // templates (as if the dedicated compose path had just run), the
    // incoming snapshot is stale/empty, and applying it must NOT clobber
    // the live templates or the seeded flag.
    #[test]
    fn settings_update_preserves_out_of_band_prompt_templates() {
        let mut cur = Settings {
            prompt_templates: vec![
                PromptTemplate {
                    name: "review-this-diff".to_string(),
                    body: "R".to_string(),
                },
                PromptTemplate {
                    name: "my-new-template".to_string(),
                    body: "N".to_string(),
                },
            ],
            templates_seeded: true,
            ..Settings::default()
        };

        // The incoming snapshot represents an unrelated Settings-window save
        // (e.g. a theme flip) whose local copy of the template library is
        // stale/empty because it was fetched before the compose-section edit.
        let mut incoming = Settings::default();
        incoming.ui.theme = "future-light".to_string();
        assert!(incoming.prompt_templates.is_empty());
        assert!(!incoming.templates_seeded);

        apply_incoming_settings(&mut cur, incoming);

        // The unrelated field DID apply...
        assert_eq!(cur.ui.theme, "future-light");
        // ...but the out-of-band template library and its seeded flag must
        // be exactly as they were live, not reverted/deleted by the stale
        // incoming snapshot.
        assert_eq!(cur.prompt_templates.len(), 2);
        assert_eq!(cur.prompt_templates[0].name, "review-this-diff");
        assert_eq!(cur.prompt_templates[1].name, "my-new-template");
        assert!(cur.templates_seeded);
    }

    // Same stale-snapshot scenario for the LLM price table, which is written
    // only by `llm_pricing_set` -> `write_global_llm_pricing`: an unrelated
    // Settings-window save must not revert live price edits.
    #[test]
    fn settings_update_preserves_out_of_band_llm_pricing() {
        let mut cur = Settings {
            llm_pricing: vec![crate::settings::LlmPricingModel {
                provider: "Custom".to_string(),
                model: "my-model".to_string(),
                model_prefix: String::new(),
                input: 1.0,
                cache_write: 2.0,
                cache_read: 0.5,
                output: 4.0,
            }],
            ..Settings::default()
        };

        let mut incoming = Settings::default();
        incoming.ui.theme = "future-light".to_string();
        assert_ne!(incoming.llm_pricing, cur.llm_pricing); // stale (seeded defaults)

        apply_incoming_settings(&mut cur, incoming);

        assert_eq!(cur.ui.theme, "future-light");
        assert_eq!(cur.llm_pricing.len(), 1);
        assert_eq!(cur.llm_pricing[0].provider, "Custom");
        assert_eq!(cur.llm_pricing[0].model, "my-model");
    }
}
