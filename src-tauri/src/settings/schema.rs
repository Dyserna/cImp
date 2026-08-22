//! Settings schema. Every struct uses `#[serde(default)]` so loading a JSON
//! file written by a future or past version still succeeds: missing fields
//! get defaults, unknown fields are ignored. v1.2 schema; the v1 → v2 and
//! v1.1 → v1.2 migrations live in `migration.rs` and run once on first load.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::shell::ShellSpec;

// ── Enum-ish string settings: a wrong TYPE must not quarantine the file ────
//
// (#48, the G-1 defect class.) A settings field whose real domain is a small
// closed vocabulary but whose storage is `String` carries exactly the defect
// `injection::Override` had before #48: the post-hoc `parse` covers an
// unrecognized **string**, while a value of the wrong JSON **type** never
// reaches it. `#[serde(default)]` fires for an ABSENT key, never for a present
// one that fails to deserialize — so `"native_web_visibility": true` (or
// `null`, or `0`) failed the typed parse of the WHOLE settings file, which
// `settings::persistence` quarantines and replaces with seeded defaults:
// themes, tabs, backends, checks, MCP servers and pricing all reset because one
// mode string was hand-edited wrong.
//
// The rule below is one sentence: **a non-string reads exactly as an
// unrecognized string does.** Not "as the shipped default" — for the two
// updater modes those differ, and the documented fallback (`check`, the middle
// setting) is the one that must neither silently disable the updater nor
// silently grant it activation rights. The fallback is returned spelled
// canonically so the repaired value also round-trips to disk as something the
// Settings window's `<select>` can display: a corrupt cell is repaired, not
// perpetuated.
//
// Per-field wrappers rather than one blanket lenient deserializer, because the
// answer belongs to the field: a shared helper that guessed would be one more
// place the vocabulary is written down.
fn lenient_enum_string<'de, D>(d: D, fallback: &str) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => s,
        _ => fallback.to_string(),
    })
}

/// `offload.native_web_visibility` — non-string ⇒ `sensor`, which is also what
/// `injection::NativeWebMode::parse` answers for an unrecognized string: a typo
/// must neither blind the latch nor silently take a tool away.
fn de_native_web_visibility<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    lenient_enum_string(d, "sensor")
}

/// The two `offload.detection_update_*_mode` fields — non-string ⇒ `check`,
/// which is what `detection::updater::Mode::parse` answers for an unrecognized
/// string (the middle setting, deliberately).
fn de_update_mode<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    lenient_enum_string(d, "check")
}

/// `graph.read_advisor_mode` — non-string ⇒ `advise`, the behaviour every
/// consumer already falls back to for an unrecognized string (only an explicit
/// `substitute` selects the other arm).
fn de_read_advisor_mode<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    lenient_enum_string(d, "advise")
}

/// Reserved IDs the integrity check protects. Hand-edited settings files
/// cannot make these disappear — they are restored with defaults if missing
/// and forced to `builtin: true` regardless of what the file claims. Four
/// reserved AI ids today: subscription/local pairs for both Claude Code
/// and Aider.
pub const CLAUDE_TAB_ID: &str = "claude";
/// V1.4-07: second Claude tab preconfigured to use a local LLM via the
/// `claude_local` provider settings. Replaces the v1.7-and-earlier
/// `aider` reserved id (the v1.7 → v1.8 migration rewrites the aider
/// tab in place to this id).
pub const CLAUDE_LOCAL_TAB_ID: &str = "claude-local";
/// The single OpenCode AI-tool tab. OpenCode picks its own provider/model
/// (global config + credentials, switchable in-session), so unlike Claude
/// there is no cloud/local pair. Reserved in V19, replacing BOTH the V14
/// `aider` and `aider-local` reserved ids (the v18 → v19 migration collapses
/// them into this one id).
pub const OPENCODE_TAB_ID: &str = "opencode";
pub const SHELL_DEFAULT_TAB_ID: &str = "shell-default-1";
/// Legacy id of the V8-03 reserved Offload Server tab. Retired in schema v25:
/// the live backend dashboard moved INSIDE the Tool Activity tab as the
/// "Offload server" section (ToolActivityView.svelte), so there is no separate
/// reserved tab anymore. This constant survives only so the v24 → v25
/// migration can find and drop the old materialized `offload-server` entry
/// from existing settings files.
pub const OFFLOAD_SERVER_TAB_ID: &str = "offload-server";
/// V9-01: reserved id of the read-only, non-closable "Code Graph" monitor
/// tab. Materialized iff `graph.enabled` (reconciled by the integrity
/// check). App-rendered, not PTY-backed.
pub const GRAPH_MONITOR_TAB_ID: &str = "graph-monitor";
/// V13 Phase A: reserved id of the read-only, app-rendered Workbench tab
/// (live diff / checkpoint timeline / worktrees). Materialized iff
/// `workbench.enabled` (reconciled by the integrity check, exactly like the
/// Code Graph monitor tab). App-rendered like the graph monitor — no PTY.
pub const WORKBENCH_TAB_ID: &str = "workbench-1";
/// Legacy id of the V15 Feature 4 reserved Graph View tab. Retired in schema
/// v26: the live force-graph moved INSIDE the Tool Activity tab as the
/// "Graph view" section (ToolActivityView.svelte), so there is no separate
/// reserved tab anymore. This constant survives only so the v25 → v26
/// migration (and the integrity check's retired-tab prune) can find and drop
/// the old materialized `graph-view` entry from existing settings files.
pub const GRAPH_VIEW_TAB_ID: &str = "graph-view";
/// Reserved id of the read-only, app-rendered Tool Activity tab — a unified
/// feed of graph-tool calls + offload requests, plus the graph/offload tool
/// reference lists. Materialized iff `ui.tool_activity_tab` (default true,
/// reconciled by the integrity check, exactly like the Code Graph monitor
/// tab). App-rendered like the monitor — no PTY.
pub const TOOL_ACTIVITY_TAB_ID: &str = "tool-activity";
/// #51: reserved id of the read-only, app-rendered Events tab — the activity
/// feed read as *events*, attributed per tab/session and filterable by kind,
/// source/screen and tab (see `activity::Attribution`). Materialized iff
/// `ui.events_tab` (default true, reconciled by the integrity check, exactly
/// like the Tool Activity tab). App-rendered — no PTY.
///
/// **Strictly additive**: the Tool Activity tab keeps its feed and its
/// sections, and the Workbench Timeline stays its own tab. Nothing is retired,
/// so this id must never appear in `RETIRED_TAB_IDS`.
pub const EVENTS_TAB_ID: &str = "events";
/// Legacy id of the V23 reserved Code Audit tab. Retired in schema v27: the
/// Security | Quality audit panels moved INSIDE the Tool Activity tab as the
/// "Code audit" section (ToolActivityView.svelte), so there is no separate
/// reserved tab anymore. This constant survives only so the v26 → v27
/// migration (and the integrity check's retired-tab prune) can find and drop
/// the old materialized `code-audit` entry from existing settings files.
pub const CODE_AUDIT_TAB_ID: &str = "code-audit";
/// Legacy id of the V25 reserved Code Quality tab. Retired in schema v23: the
/// Quality view moved INSIDE the Code Audit tab as a sub-tab (Security |
/// Quality), so there is no separate reserved tab anymore. This constant
/// survives only so the v22 → v23 migration can find and drop the old
/// materialized `code-quality` entry from existing settings files.
pub const CODE_QUALITY_TAB_ID: &str = "code-quality";
/// Legacy id of the V15 reserved broot tab. Retired in V16: broot is no
/// longer a persistent builtin — it (like rustnet) launches on demand from
/// the bottom-bar tool buttons into ordinary closable Shell tabs (uuid ids).
/// This constant survives only so the v15 → v16 migration can find and drop
/// the old auto-seeded `shell-broot` entry from existing settings files.
pub const SHELL_BROOT_TAB_ID: &str = "shell-broot";

/// The on-disk schema version. Bumped on every migration step. Detection
/// of legacy files prefers this field's integer value over the older
/// presence-of-key archaeology in the migration cascade — a future migration
/// only needs to compare `value.schema_version < N`.
///
/// Files that pre-date V1.10 lack the field entirely; the cascade still
/// uses the `looks_v1_X` predicates for those, falling through to a final
/// step that stamps the field with the current value.
pub const CURRENT_SCHEMA_VERSION: u8 = 36;

fn current_schema_version() -> u8 {
    CURRENT_SCHEMA_VERSION
}

/// Serde default for `Settings::pricing_seeded_generation` — see that field's
/// doc comment for why it must be 0 rather than the container default.
fn pricing_generation_none() -> u32 {
    0
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Settings {
    /// On-disk schema version. Stamped at `CURRENT_SCHEMA_VERSION`
    /// on fresh installs and by the v1.9→v1.10 migration. Defaulted via
    /// `current_schema_version` so legacy files (which lack the key)
    /// deserialize at the latest version after migration runs.
    #[serde(default = "current_schema_version")]
    pub schema_version: u8,
    pub tts: TtsSettings,
    /// V6-01 offline speech-to-text (dictation) config. Additive
    /// `#[serde(default)]` field — old settings files round-trip with
    /// the feature disabled by default.
    pub stt: SttSettings,
    pub avatar: AvatarSettings,
    pub display: DisplaySettings,
    pub behavior: BehaviorSettings,
    /// Bottom-bar Claude Code usage tracker config.
    pub usage: UsageSettings,
    /// Bottom-bar system-monitor panel config.
    pub system_stats: SystemStatsSettings,
    pub compose: ComposeSettings,
    pub shortcuts: ShortcutSettings,
    /// Ordered list of tabs. AI builtins occupy the canonical leading
    /// slots in the order claude → claude-local → aider → aider-local
    /// (only the ids in `enabled_ai_tabs` actually materialize); user
    /// shell tabs follow. The startup integrity check reconciles
    /// `tabs[]` with `enabled_ai_tabs` — hand-edits that delete an
    /// enabled AI builtin are repaired at load time.
    pub tabs: Vec<TabConfig>,
    pub processing: ProcessingSettings,
    /// Last-active tab pointer, restored on launch. None on a fresh install
    /// (falls back to the first tab); set whenever the user switches tabs.
    pub session: SessionState,
    /// Persisted layout tree + focused-pane id (V4-04). `None` on fresh
    /// installs and on the first launch after a v1.2 → v1.3 migration that
    /// detected the file but couldn't synthesize a layout (defensive — the
    /// migration normally always builds a single-pane layout). The frontend
    /// builds a default single-root-pane tree containing every tab when
    /// this is `None`.
    ///
    /// Unlike the other settings structs (which tolerate missing/unknown
    /// fields via `#[serde(default)]`), the recursive `LayoutNodePersisted`
    /// has required fields per node, so a single malformed node (e.g. a
    /// `Split` missing `ratio`) would otherwise fail the *entire* `Settings`
    /// parse and silently discard the whole per-folder overlay. The lenient
    /// deserializer degrades a bad layout to `None` — the frontend rebuilds
    /// a default tree — keeping the rest of the config intact.
    #[serde(default, deserialize_with = "deserialize_lenient_layout")]
    pub layout: Option<LayoutPersisted>,
    /// Named layout presets. Empty by default; populated via the Layouts
    /// menu's "Save current layout as..." entry. Restoring a preset
    /// replaces the live tree wholesale; the preset itself is unchanged.
    /// Lenient like `layout`: a malformed preset is dropped individually
    /// rather than taking the whole settings load down with it.
    #[serde(default, deserialize_with = "deserialize_lenient_presets")]
    pub layout_presets: Vec<LayoutPreset>,
    /// UI chrome theme settings (V5). The `theme` field selects the
    /// design-token block applied to the cimp chrome (tab bar, status
    /// bar, dialogs, settings). Distinct from `terminal.theme`, which
    /// governs the xterm.js terminal palette inside each tab.
    pub ui: UiSettings,
    /// Terminal-pane settings (V1.4-01+): xterm.js theme today, plus
    /// the V1.4-02 background image/color group when that ships.
    /// Distinct from `ui`, which themes the cimp chrome.
    pub terminal: TerminalSettings,
    /// Optional explicit executable paths for the bundled quick-launch
    /// tools (rustnet / broot). A non-empty field overrides the normal
    /// `ebin/` → PATH resolution for that tool, letting the user point at
    /// an exe in any folder; empty means "resolve normally". Additive
    /// `#[serde(default)]` — old settings files load with both empty.
    pub external_tools: ExternalToolsSettings,
    /// V8-01: local task-offload config. cImp runs a user-supplied
    /// `llama-server` and exposes an `offload_task` MCP tool into
    /// cImp-launched Claude tabs so Opus can delegate token-heavy
    /// subtasks to the local model. Off by default. Additive — old
    /// settings files load with the feature disabled.
    pub offload: OffloadSettings,
    /// V9-01: per-project code knowledge graph config. cImp builds an
    /// on-disk graph of code + docs at `<project>/<db_subdir>/graph.db`
    /// and exposes `graph_*` query tools to Claude tabs (MCP) and the
    /// offload worker (native). Off by default. Additive — old settings
    /// files load with the feature disabled.
    pub graph: GraphSettings,
    /// V13 Phase A: the Workbench feature — live diff pane, checkpoints (a
    /// shadow git repo), and a worktree manager, hosted in one reserved tab.
    /// The master `enabled` flag defaults `true` (the tab itself is cheap;
    /// each sub-feature gates itself, and checkpoints stay off by default —
    /// see `WorkbenchSettings`). Additive — old settings files load with the
    /// tab present but checkpoints off.
    pub workbench: WorkbenchSettings,
    /// V33 Phase A: OS-level sandboxing of agent-initiated children (locked
    /// decisions 16-17). Off by default; additive `#[serde(default)]` at the
    /// container level, so old settings files load with the layer off.
    pub sandbox: SandboxSettings,
    /// V39: cross-harness delegation (one tab drives another). Phase A ships
    /// only [`DelegationSettings::auto_read_only`]'s consumer-to-be plus the
    /// read-only lock itself; the timeout and depth bounds are declared here
    /// now so the engine phases add behaviour, not schema. Additive
    /// `#[serde(default)]` at the container level.
    pub delegation: DelegationSettings,
    /// V12 Phase A: project checker commands (`cargo check`, `tsc`, `eslint`,
    /// `pytest`, …) the `run_check` MCP tool can run. Lives at the root, not
    /// inside `GraphSettings` — it's project tooling, independent of the code
    /// graph (`run_check` is advertised whenever this is non-empty, whether or
    /// not `graph.enabled`). Empty by default; rides the `.cimp/config.json`
    /// overlay, which is where users actually set it. A model-supplied
    /// `run_check` tool call only *selects* a `CheckDef` by name — the command
    /// itself is never model-supplied.
    pub checks: Vec<crate::checks::CheckDef>,
    /// V22 Phase D: when `true`, validated auto-detection proposals are applied
    /// automatically the first time a project's code graph finishes indexing
    /// with an empty `checks` list (for fleet users who want zero-touch setup
    /// across many projects). Default **false** — a wrong auto-applied check
    /// burns tokens on every `auto_check` fire, so the propose-then-approve chip
    /// is the default; this is the opt-in. Applied entries carry
    /// `CheckDef::auto = true` so a later re-detection can refresh them without
    /// fighting user edits. Rides the per-project `.cimp/config.json` overlay
    /// like `checks` itself. Additive `#[serde(default)]` (struct-level) ⇒ old
    /// configs load with it off.
    pub checks_auto_configure: bool,
    /// V22 Phase D: set once the user dismisses the "N suggested checks" nudge
    /// (Code Intelligence chip) for THIS project, so it doesn't re-appear on
    /// every index. Per-project via the overlay (a fresh project re-offers the
    /// nudge). Written by the `checks_dismiss_suggestion` IPC. Additive.
    pub checks_suggestion_dismissed: bool,
    /// #48, finding **F-12**: let the **offload worker** run this project's
    /// configured checks (`run_check`) when it is working on a **remote**
    /// backend. The exact shape of `graph.allow_remote_worker_access`, for the
    /// same reason: a remote backend (LAN *or* cloud) receives the check
    /// command's output, which quotes source — and `run_check` executes local
    /// build/test/lint commands, so advertising it off-machine hands a third
    /// party arbitrary local command execution against the user's repo.
    ///
    /// **Off by default, and `false` is the safe value.** `Settings` carries a
    /// container-level `#[serde(default)]`, so a config file that predates this
    /// field — and a Settings-window snapshot that omits it — deserializes to
    /// `false`, i.e. *denied*. That is the direction the F-19 trap has to fail
    /// in: the worst case is an opt-in that has to be re-ticked, never a silent
    /// re-opening of the hole.
    ///
    /// Lives at the root beside [`Self::checks`] (project tooling, independent
    /// of the code graph) and rides the per-project `.cimp/config.json` overlay
    /// like `checks` itself, so the decision is made per repo — which is the
    /// granularity that matters, since the commands are per repo.
    ///
    /// **Not spawn-baked**: nothing in a Claude/OpenCode tab's argv or injected
    /// config depends on it. It is resolved per offload run by
    /// `BackendGate::for_worker`, so it needs no `tabs::config::spawn_inject_sig`
    /// entry and must not acquire one (that would nag every tab to restart for a
    /// setting that takes effect on the next worker call).
    pub checks_allow_remote_worker: bool,
    /// Which AI-tool tabs are enabled. Each id in this list corresponds
    /// to one of the four reserved AI builtins (`claude`, `claude-local`,
    /// `aider`, `aider-local`). Adding an id opens that tab; removing
    /// one closes it (kills its PTY, drops scrollback, removes the
    /// settings entry). The list is required to be non-empty — the
    /// integrity check forces `[claude]` if it deserializes empty, and
    /// the runtime IPC rejects an empty value. Default is `[claude]` so
    /// a fresh install starts with the subscription Claude tab only.
    pub enabled_ai_tabs: Vec<AiTabId>,
    /// File-logger configuration. The tracing subscriber writes daily
    /// rolling files into `<portable-root>/logs/`; the level field drives
    /// the EnvFilter via a reload handle so changes apply live.
    pub logging: LoggingSettings,
    /// V14 Phase A: the global-scope prompt-template library. Populated with
    /// [`starter_prompt_templates`] once (see `templates_seeded`), then
    /// entirely user-owned. Read/written through dedicated `compose_templates_*`
    /// IPC that targets the physical global `settings.json` directly (NOT the
    /// normal per-project overlay diff every other field goes through) — see
    /// `settings::persistence::{read,write}_global_prompt_templates` — so the
    /// library really is global regardless of which project cImp is launched
    /// from. Project-scope additions live separately, in the `.cimp/config.json`
    /// overlay's own `prompt_templates` array (see [`PromptTemplate`]'s doc
    /// comment); this field is NOT merged with those.
    pub prompt_templates: Vec<PromptTemplate>,
    /// One-shot gate for the starter-template seed: `false` until the first
    /// load seeds [`starter_prompt_templates`] into `prompt_templates` and
    /// flips this to `true`. Deliberately independent of
    /// `CURRENT_SCHEMA_VERSION` — a user who deletes all 4 starters must not
    /// have them reappear on a future migration.
    pub templates_seeded: bool,
    /// F-19: which [`PRICING_GENERATION`] this install's `llm_pricing` has
    /// been topped up to. Newer built-in rows are appended once, on the next
    /// load, by `persistence::top_up_llm_pricing_if_needed`.
    ///
    /// A watermark rather than a `templates_seeded`-style bool because the
    /// built-in set keeps growing: a one-shot flag would flip on the first
    /// launch and never carry a later model in.
    ///
    /// **The explicit `default` is load-bearing.** `Settings` carries a
    /// container-level `#[serde(default)]`, which fills a missing field from
    /// `Settings::default()` — i.e. `PRICING_GENERATION`, marking every
    /// pre-existing install as already current and permanently suppressing the
    /// top-up it exists to perform. The field-level attribute overrides that
    /// with 0, so an install that predates this field is correctly seen as
    /// generation 0. Fresh installs get the current generation from `Default`,
    /// since `default_llm_pricing` already includes every row.
    #[serde(default = "pricing_generation_none")]
    pub pricing_seeded_generation: u32,
    /// V14 Phase D2: budget-tuning advisor proposals the user has dismissed.
    /// Each entry suppresses ONE rule at ONE coarse (10%-bucketed) rate —
    /// see `advisor::Proposal::signature`'s doc comment — so a materially
    /// changed rate re-fires the proposal even though the same `rule_id`
    /// still matches. Additive `#[serde(default)]`; empty on a fresh install.
    pub advisor_dismissed: Vec<DismissedRule>,
    /// Advisor proposals the user has APPLIED, with the project's session
    /// count at apply time. Each entry holds its rule quiet for
    /// `advisor::APPLY_COOLDOWN_SESSIONS` further sessions — the advisor's
    /// rates are cumulative, so an immediate re-proposal after Apply would be
    /// judging data collected almost entirely under the OLD value, not
    /// evidence the raise failed. One entry per (rule, project root);
    /// re-applying replaces it (see `ipc::commands::advisor_mark_applied`).
    /// Additive `#[serde(default)]`; empty on a fresh install.
    pub advisor_applied: Vec<AppliedRule>,
    /// V14 Phase F: the last URL entered into any Preview tab, remembered so
    /// the next "New Preview tab" starts from where the user left off rather
    /// than the hardcoded fallback (`preview::DEFAULT_PREVIEW_URL`). A plain
    /// scalar field (unlike `prompt_templates`) — it merges correctly through
    /// the ordinary per-project `.cimp/config.json` overlay diff (a later
    /// scalar write simply overwrites the earlier one; no array-replace pitfall
    /// applies), so a project remembers its own dev-server URL without any
    /// bespoke read/write path. `None` until the first Preview tab is created
    /// or navigated.
    pub preview_last_url: Option<String>,
    /// V14 Phase F: global gate on the Preview tab's navigation policy — when
    /// `false` (the default), `preview::is_allowed_preview_host` only allows
    /// localhost/127.0.0.1/RFC-1918 hosts; navigation to anything else opens
    /// in the system browser instead. Opt-in per the milestone's design: a
    /// Preview tab is a dev-server surface, not a general (and
    /// prompt-injectable) browsing pane beside the agent tabs.
    pub preview_allow_remote: bool,
    /// Provider/model token-price table ($ per million tokens) backing the
    /// Code Intelligence tab's per-session cost popup. App-wide like
    /// `prompt_templates`: read/written through dedicated `llm_pricing_*`
    /// IPC that targets the physical global `settings.json` directly (see
    /// `settings::persistence::{read,write}_global_llm_pricing`), NOT the
    /// per-project overlay diff — an array field would be replaced wholesale
    /// by the overlay merge, so routing it through `settings_update` would
    /// silently scope "global" edits to one project. Seeded with current
    /// Anthropic API + GitHub Copilot prices via the serde/`Default` default
    /// (a file that carries the key — even as `[]` — keeps what it has, so
    /// deleted seeds stay deleted; no `templates_seeded`-style flag needed).
    #[serde(default = "crate::pricing::default_llm_pricing")]
    pub llm_pricing: Vec<LlmPricingModel>,
    /// V16 Feature 1: per-install harness version + contract-verification
    /// state. Global-only like `llm_pricing` (a harness install is per
    /// machine, not per project): read/written through
    /// `settings::persistence::{read,write}_global_harness_versions`, which
    /// target the physical global `settings.json` directly — background
    /// writers (the transcript tap, tab spawn) must never land this in a
    /// project overlay diff.
    #[serde(default)]
    pub harness_versions: HarnessVersions,
    /// V40 Phase B (locked decision 5), schema 36: **per-harness settings**,
    /// keyed by the registry id (`"claude"`, `"opencode"`, later `"codex"`).
    ///
    /// Machine scope, like `harness_versions` and `sandbox`: it carries state
    /// written out of band (the version the transcript tap observed, the
    /// auto-verify record) beside configuration that is about the harness
    /// INSTALL on this machine (where its local proxy is, whether its status
    /// line is on, whether cImp derived a provider block for it). None of that
    /// is a property of the project you happen to have open, so `harness` is in
    /// `OVERLAY_BANNED_KEYS` and a Settings save writes it through to the
    /// physical global file — the `sandbox` pattern exactly.
    ///
    /// A `String` key rather than a `HarnessId`: an id nobody registered must
    /// survive a load/save round trip (a `harness.codex` block written by a
    /// newer build, or by hand), and a typed key would refuse to parse it.
    /// Every ACCESSOR takes a `HarnessId` — see [`Self::harness_settings`] —
    /// so no reader spells one either.
    #[serde(default)]
    pub harness: BTreeMap<String, HarnessSettings>,
    /// V23 Phase A: Code Audit (aggregated security scanning) config. Off by
    /// default; `enabled` gates the reserved Code Audit dashboard tab (mirrors
    /// `ui.tool_activity_tab`) and the bottom-bar entry point. Additive
    /// `#[serde(default)]` — old settings files load with the feature disabled
    /// and the three default tools present. No schema-version bump.
    #[serde(default)]
    pub code_audit: CodeAuditSettings,
    /// V38 Phase B: user state for the drop-in **tool plugins** — which of a
    /// manifest's tools are enabled, what their declared variables are set to,
    /// and where their binaries live on this machine. Schema v33 (additive: the
    /// container materializes empty, nothing moved into it).
    ///
    /// Keyed maps rather than typed fields, on purpose: the set of plugins is
    /// whatever is in `<exe-dir>/plugins/` today, so a typed shape would need a
    /// settings migration every time a user dropped a file in a folder. See
    /// [`ToolPluginsSettings`] for how the three maps divide by SCOPE.
    #[serde(default, deserialize_with = "deserialize_lenient_tool_plugins")]
    pub tool_plugins: ToolPluginsSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            tts: TtsSettings::default(),
            stt: SttSettings::default(),
            avatar: AvatarSettings::default(),
            display: DisplaySettings::default(),
            behavior: BehaviorSettings::default(),
            usage: UsageSettings::default(),
            system_stats: SystemStatsSettings::default(),
            compose: ComposeSettings::default(),
            shortcuts: ShortcutSettings::default(),
            tabs: Vec::new(),
            processing: ProcessingSettings::default(),
            session: SessionState::default(),
            layout: None,
            layout_presets: Vec::new(),
            ui: UiSettings::default(),
            terminal: TerminalSettings::default(),
            external_tools: ExternalToolsSettings::default(),
            offload: OffloadSettings::default(),
            graph: GraphSettings::default(),
            workbench: WorkbenchSettings::default(),
            sandbox: SandboxSettings::default(),
            delegation: DelegationSettings::default(),
            checks: Vec::new(),
            checks_auto_configure: false,
            checks_suggestion_dismissed: false,
            // F-12: denied by default — the remote worker does not get to run
            // this project's commands until the user says so.
            checks_allow_remote_worker: false,
            enabled_ai_tabs: vec![AiTabId::Claude],
            logging: LoggingSettings::default(),
            prompt_templates: Vec::new(),
            templates_seeded: false,
            // A fresh install takes the whole current table from
            // `default_llm_pricing`, so it starts already topped up.
            pricing_seeded_generation: crate::pricing::PRICING_GENERATION,
            advisor_dismissed: Vec::new(),
            advisor_applied: Vec::new(),
            preview_last_url: None,
            preview_allow_remote: false,
            llm_pricing: crate::pricing::default_llm_pricing(),
            harness_versions: HarnessVersions::default(),
            harness: default_harness_settings(),
            code_audit: CodeAuditSettings::default(),
            tool_plugins: ToolPluginsSettings::default(),
        }
    }
}

/// V39: cross-harness delegation — the global knobs that are not per tab.
///
/// All three are declared in Phase A even though only the first has a UI in
/// Phase A, because the alternative is touching the schema again in B and C
/// for fields whose defaults are already decided. New fields with correct
/// defaults need no migration step (the container is `#[serde(default)]`), so
/// declaring them early costs nothing and keeps the on-disk shape stable.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct DelegationSettings {
    /// Lock the worker tab's keyboard for the duration of a delegation
    /// (`ReadOnlySource::Driven`), then release it.
    ///
    /// Default **on**: while cImp is typing into a tab, a stray keystroke of
    /// the user's lands in the middle of someone else's turn. This is a
    /// courtesy lock over the user's own hands, not a security boundary —
    /// permission and question prompts relax it (decision 5) and "Take over"
    /// clears it outright.
    pub auto_read_only: bool,
    /// How long the engine waits for a worker's reply before giving up, when
    /// the caller did not pass a `timeout_s`. On expiry the driver is told
    /// `timeout`; **no keys are ever sent to the worker** to cancel it — it
    /// finishes visibly.
    pub default_timeout_s: u64,
    /// How deep delegations may nest. Default **1** = a tab that is being
    /// driven may not itself drive (the acyclic check refuses and names the
    /// chain). Raising it is the knob that opens nesting later without a
    /// redesign.
    pub max_depth: u8,
}

impl Default for DelegationSettings {
    fn default() -> Self {
        Self {
            auto_read_only: true,
            default_timeout_s: 600,
            max_depth: 1,
        }
    }
}

/// V38 Phase B: the **tool plugin** user-state container (schema v33).
///
/// One stable, keyed-map block so plugin churn never migrates the schema: a
/// plugin is a file a user drops into `<exe-dir>/plugins/`, so the set of keys
/// is data, not shape. Adding a plugin adds a map entry; deleting its file
/// leaves the entry alone (see below).
///
/// # The three maps are three SCOPES, and that is the whole design
///
/// * [`Self::plugins`] — enables, timeouts, declared-variable values and extra
///   parameters, keyed `name@version`. Of these only `variables` and
///   `parameters` may ride a project's `.cimp/config.json`; enables and
///   timeouts are machine-global (amended decision 10). The overlay strip in
///   `settings::persistence` enforces that structurally rather than by
///   documentation.
/// * [`Self::global_paths`] — "where does this tool's binary live on this
///   machine", keyed `name@version/tool-id`. A path is a machine fact, never a
///   project preference, and the V26 field report (scanner paths configured in
///   one repo, audits run in another resolving nothing) is what that rule is
///   made of.
/// * [`Self::project_paths`] — the same fact, overridden **per project**, keyed
///   by the canonical project root and then by tool key. Still machine-global
///   storage: a per-project *override* is not the same thing as a per-project
///   *file*, and putting it in the overlay would put a binary path inside the
///   sandbox boundary a confined child can write to (V33's `sandbox` ban, same
///   reasoning).
///
/// Effective path = `project_paths[root][tool_key]` ?? `global_paths[tool_key]`
/// ?? unset; a tool with no path is inert (nothing to run). The resolution
/// lives in `plugins::registry`, which is the one place that answers it.
///
/// # Entries outlive their plugins, deliberately
///
/// Nothing prunes a key whose manifest is not currently loaded. A plugin file
/// removed for an afternoon — or a plugin folder on a machine that has not
/// synced yet — must not silently discard the user's configuration for it, and
/// "the tool disappeared, so I threw away your settings" is a data-loss bug
/// wearing a tidiness costume. The settings pane renders what the loader found;
/// the state for everything else simply waits.
/// # Exposure of `command`-kind tools (V38 F-3)
///
/// The two `expose_commands_*` flags are the fourth member of the container and
/// the only non-map one. They are **machine scope** like everything here except
/// the two per-tool leaves — the overlay strip is an allow-list, so they are
/// dropped from a project's `.cimp/config.json` without anyone adding a rule,
/// which is the right default for a switch that decides what a model may
/// execute.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct ToolPluginsSettings {
    /// Per-plugin state, keyed `name@version` (`loader::LoadedPlugin::key`).
    pub plugins: BTreeMap<String, PluginState>,
    /// Per-project binary paths: canonical project root → tool key → path.
    /// Machine-global storage of a per-project override; see the type docs.
    pub project_paths: BTreeMap<String, BTreeMap<String, String>>,
    /// Machine-wide binary paths, keyed `name@version/tool-id`
    /// (`loader::LoadedPlugin::tool_key`). The fallback when the current
    /// project names none.
    pub global_paths: BTreeMap<String, String>,
    // V38 F-3's `expose_commands_claude` / `_opencode` pair is
    // `Settings::harness[<id>].expose_commands` since V40 Phase B (locked
    // decision 5) — one field per harness instead of one field pair per
    // question, and no third field to add for a third harness.
}

impl Default for ToolPluginsSettings {
    fn default() -> Self {
        Self {
            plugins: BTreeMap::new(),
            project_paths: BTreeMap::new(),
            global_paths: BTreeMap::new(),
        }
    }
}

// `ToolPluginsSettings::commands_exposed_to(consumer)` is gone with the field
// pair it read: the question "is `run_command` advertised to this harness" is
// `Settings::harness_settings(h).expose_commands`, asked with a `HarnessId`
// rather than a free string whose unrecognized values used to resolve as
// Claude.

/// One plugin's user state.
///
/// `BTreeMap` rather than `HashMap` throughout the container: the settings file
/// is diffed textually against a baseline to produce the project overlay, so a
/// map that serialized in a different order on each save would manufacture a
/// diff out of nothing.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct PluginState {
    /// Master switch for every tool this plugin declares. Disabling it disables
    /// them **as a unit** without touching their own `enabled` flags, so
    /// re-enabling the plugin restores exactly the selection the user had
    /// (decision 9).
    pub enabled: bool,
    /// Per-tool state, keyed by the tool's manifest id (NOT the namespaced tool
    /// key — the plugin key is already the outer map's key).
    pub tools: BTreeMap<String, ToolState>,
}

impl Default for PluginState {
    fn default() -> Self {
        Self {
            // A plugin the user installed is on. The consent that matters is
            // dropping the file in; a second, invisible "and now switch it on"
            // step would only teach people to click past it.
            enabled: true,
            tools: BTreeMap::new(),
        }
    }
}

/// One tool's user state. **No path field**: paths are machine-scope and live
/// in [`ToolPluginsSettings::global_paths`] / `project_paths` — the split that
/// keeps a project overlay from pinning a copy of a machine fact.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct ToolState {
    pub enabled: bool,
    /// Wall-clock override in seconds; `None` = the manifest's value, then the
    /// consuming pipeline's default.
    pub timeout_secs: Option<u64>,
    /// Extra CLI arguments appended after the tool's own argv — the successor
    /// to the pre-v34 `code_audit.tools[].extra_args`, offered only for a tool
    /// whose manifest sets `parameters_allowed`.
    pub parameters: Vec<String>,
    /// Values for the tool's **declared** variables, by declared name. A name
    /// the manifest does not declare is inert (the registry only substitutes
    /// declared ones) but is kept: it is most likely a plugin mid-upgrade.
    pub variables: BTreeMap<String, String>,
}

impl Default for ToolState {
    fn default() -> Self {
        Self {
            // Same reasoning as `PluginState::enabled`, one level down: a tool
            // the plugin author shipped is on unless the user says otherwise.
            // The gate that actually decides whether it RUNS is the path — no
            // path, nothing to spawn — so "enabled by default" is not "runs by
            // default".
            enabled: true,
            timeout_secs: None,
            parameters: Vec::new(),
            variables: BTreeMap::new(),
        }
    }
}

/// Deserialize [`ToolPluginsSettings`] tolerantly, dropping entries rather than
/// failing the whole `Settings` parse.
///
/// **This is not defensive decoration — it is required by how overlays work.**
/// `settings::persistence::diff` writes an explicit JSON `null` for every key
/// the baseline has and the current value does not, so that the reverse merge
/// can reconstruct a *deletion*. Every other field in `Settings` is a struct
/// field or an array, and structs always serialize every key, so that null
/// never arises for them. This container is the first one keyed by DATA (plugin
/// keys, tool ids, variable names), so a user who clears one variable in a
/// project produces `{"variables": {"ruleset": null}}` in that project's
/// overlay — and a strict `BTreeMap<String, String>` would refuse it, taking
/// the *entire* settings file down to "typed parse failed; using global" (the
/// same failure mode `deserialize_lenient_layout` exists to prevent, arriving
/// through a different door).
///
/// So: a null entry means "deleted", which is exactly what the diff meant by
/// it, and is honoured by dropping the key. A malformed entry is dropped with a
/// warning for the same reason the audit-tool list drops unknown ids — one bad
/// plugin's state must not cost the user everything else in the file.
fn deserialize_lenient_tool_plugins<'de, D>(d: D) -> Result<ToolPluginsSettings, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(d)?;
    if raw.is_null() {
        return Ok(ToolPluginsSettings::default());
    }
    let serde_json::Value::Object(map) = raw else {
        tracing::warn!("settings: tool_plugins was not an object; ignoring");
        return Ok(ToolPluginsSettings::default());
    };

    // Read field by field rather than through `from_value`: nulls can appear at
    // EVERY level here (plugin key, tool id, variable name), so one strict
    // `from_value` anywhere in the tree would discard a whole subtree over a
    // single deleted leaf. `tool_plugins_round_trips_through_the_lenient_reader`
    // is what keeps this walk honest when a field is added to either struct.
    let mut out = ToolPluginsSettings::default();
    for (key, state) in object_entries(map.get("plugins"), "tool_plugins.plugins") {
        let Some(pobj) = state.as_object() else {
            tracing::warn!("settings: tool-plugin state for `{key}` was not an object; ignoring");
            continue;
        };
        let mut p = PluginState::default();
        if let Some(b) = pobj.get("enabled").and_then(serde_json::Value::as_bool) {
            p.enabled = b;
        }
        for (tool_id, tv) in object_entries(pobj.get("tools"), "tools") {
            let Some(tobj) = tv.as_object() else {
                tracing::warn!(
                    "settings: tool state for `{key}/{tool_id}` was not an object; ignoring"
                );
                continue;
            };
            let mut t = ToolState::default();
            if let Some(b) = tobj.get("enabled").and_then(serde_json::Value::as_bool) {
                t.enabled = b;
            }
            // Absent, null and non-numeric all mean "no override" — the same
            // state, so they take the same branch rather than three.
            t.timeout_secs = tobj.get("timeout_secs").and_then(serde_json::Value::as_u64);
            if let Some(a) = tobj.get("parameters").and_then(serde_json::Value::as_array) {
                t.parameters = a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect();
            }
            for (name, val) in object_entries(tobj.get("variables"), "variables") {
                if let Some(s) = val.as_str() {
                    t.variables.insert(name, s.to_string());
                }
            }
            p.tools.insert(tool_id, t);
        }
        out.plugins.insert(key, p);
    }
    for (root, entry) in object_entries(map.get("project_paths"), "tool_plugins.project_paths") {
        let mut paths = BTreeMap::new();
        for (tool_key, val) in object_entries(Some(&entry), "project paths") {
            if let Some(s) = val.as_str() {
                paths.insert(tool_key, s.to_string());
            }
        }
        out.project_paths.insert(root, paths);
    }
    for (tool_key, entry) in object_entries(map.get("global_paths"), "tool_plugins.global_paths") {
        if let Some(s) = entry.as_str() {
            out.global_paths.insert(tool_key, s.to_string());
        }
    }
    Ok(out)
}

/// The non-null entries of a JSON object field, in key order. A null value is a
/// deletion (see [`deserialize_lenient_tool_plugins`]); a non-object field is
/// nothing we can read.
fn object_entries(v: Option<&serde_json::Value>, what: &str) -> Vec<(String, serde_json::Value)> {
    match v {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Object(o)) => o
            .iter()
            .filter(|(_, val)| !val.is_null())
            .map(|(k, val)| (k.clone(), val.clone()))
            .collect(),
        Some(_) => {
            tracing::warn!("settings: {what} was not an object; ignoring");
            Vec::new()
        }
    }
}

/// V16 Feature 0 (contract spikes): the recorded outcomes of the two V11
/// `TODO(spike)` contracts — the questions no payload reveals and no fixture
/// can settle, answered by a human running the recipe in the owning harness's
/// `harness/<id>/README.md` -> "Open spikes". Plain strings so a hand edit in
/// `settings.json` is always possible.
///
/// **V40 Phase B emptied the version half of this struct.** The five fields
/// that used to live here — `claude_last_seen`, `claude_last_verified`,
/// `opencode_last_seen`, `claude_auto_verify` and `input_profile_status` —
/// were per-harness state spelled as harness-named scalars, three of them with
/// no OpenCode twin at all. They are [`HarnessSettings`] rows now
/// (`Settings::harness`), reached by [`Settings::harness_settings`]. What is
/// left is genuinely global: two spike outcomes about ONE harness's hook
/// contracts, kept here because their READER is the neutral capability gate and
/// their `Capability` rows already say which harness they are about.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct HarnessVersions {
    /// Outcome of the E1 spike (PreToolUse deny `permissionDecisionReason`
    /// reaches the model): `"unverified" | "pass" | "fail"`. `"fail"` hard
    /// blocks the read advisor — the Settings toggle renders disabled and the
    /// launch path refuses to install the PreToolUse hook regardless of
    /// `graph.read_advisor`.
    ///
    /// V35 Phase E: this field is **input to a gate, not a gate**. Nothing may
    /// interpret it here — the one query that turns it into a verdict is
    /// [`crate::harness::contract::gate`], keyed by the capability id
    /// `claude.hook.pretooluse_deny`. The separate, deliberately STRICTER
    /// `== "pass"` checks — `advisor::Signals::e1_pass` and its reader in
    /// `ipc/commands.rs` — mean *proven* rather than *not known-broken* and are
    /// NOT the same test; see the F2 note on `contract::spike_status_blocks`.
    pub e1_status: String,
    /// Outcome of the D0 spike (PreCompact `additionalContext` reaches the
    /// compaction prompt): `"unverified" | "pass" | "fail"`. Informational —
    /// a fail warns (the feature degrades to a no-op, it can't misbehave).
    pub d0_status: String,
}

impl Default for HarnessVersions {
    fn default() -> Self {
        Self {
            e1_status: SPIKE_UNVERIFIED.to_string(),
            d0_status: SPIKE_UNVERIFIED.to_string(),
        }
    }
}

/// V35 Phase F: one recorded auto-verify run.
///
/// The record exists so three consumers can read the SAME fact instead of
/// re-deriving it: the Advisor (which raises a `drift.capability.v1` notice per
/// failing capability, and suppresses the version tripwire when this record
/// already speaks for that version), the auto-advance itself, and Phase G's
/// *Harness health* panel.
///
/// Invariant, pinned by `harness::verify::tests::a_record_agrees_with_itself`:
/// [`Self::status`] is [`AutoVerify::FAIL`] **iff** [`Self::failures`] is
/// non-empty. A status that disagreed with the list would be exactly the
/// "empty is not absent" defect (a `"fail"` with nothing named is a card with
/// no fix pointer; a `"pass"` with failures is a silenced break).
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AutoVerify {
    /// The `claude_last_seen` value this run was made against. Compared by
    /// equality with the current one — a record for an older version says
    /// nothing about today's install.
    pub version: String,
    /// Wall-clock ms when the run finished (`activity::now_ms`).
    pub at_ms: u64,
    /// [`AutoVerify::PASS`] or [`AutoVerify::FAIL`]. A run that cannot reach a
    /// verdict at all writes **no record**, rather than a third status: the
    /// record and the version advance are one write, so there is no state where
    /// half of it landed, and "no record for this build" is exactly the case
    /// the version tripwire is kept as a fallback for. Anything else here is a
    /// hand edit (or a newer cImp) and is read as *not* a failure — see
    /// `harness::verify::tripwire_superseded`.
    pub status: String,
    /// One entry per capability that FAILED. Empty on a pass — and on an
    /// error, where the run could not reach a verdict at all.
    pub failures: Vec<AutoVerifyFailure>,
}

impl AutoVerify {
    /// Every capability answered; none failed. The version auto-advanced.
    pub const PASS: &'static str = "pass";
    /// At least one capability failed. The version did NOT advance and the
    /// Advisor names each failure.
    pub const FAIL: &'static str = "fail";
}

/// One failing capability inside an [`AutoVerify`] record.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AutoVerifyFailure {
    /// The `harness::contract::Capability::id` — the join key the Advisor
    /// notice, the gate and the registry all speak.
    pub capability: String,
    /// Which layer saw it: `harness::verify::EVIDENCE_CANARY` (L1, the
    /// embedded fixture) or `EVIDENCE_PROBE` (L2, the installed CLI).
    pub evidence: String,
    /// The assertion message, verbatim — the sentence the Advisor card shows.
    pub detail: String,
}

// ── the per-harness settings map (V40 locked decision 5) ────────────────────

/// **One harness's settings.** The value type of [`Settings::harness`].
///
/// V40 Phase B, schema 36. Before it, every one of these was half of a FIELD
/// PAIR — `expose_commands_claude` / `expose_commands_opencode`,
/// `code_audit.expose_claude` / `_opencode`, `claude_last_seen` /
/// `opencode_last_seen`, `claude_access` / `opencode_access` — and adding a
/// third harness meant finding all of them, adding a field to each, adding a
/// migration for each, and adding a `match` arm at every reader. The pairs that
/// were *missing* a half were worse: `claude_last_verified` and
/// `claude_auto_verify` had no OpenCode twin at all, so half the drift
/// machinery simply did not exist for the second harness cImp ships.
///
/// A map ends both. An absent key reads [`Self::defaults_for`] — the core
/// defaults plus every declared `ext` default — so **a harness added later
/// needs no migration**, which is the whole point of the decision.
///
/// # `ext`
///
/// Settings only ONE harness has (locked decision 6) live in [`Self::ext`], an
/// opaque object whose schema, defaults and validation are the plugin's
/// (`HarnessPlugin::settings_schema`). Core stores it, type-checks the declared
/// keys at the parse boundary and folds the spawn-baked ones into the spawn
/// signature — and never names a key.
///
/// # Unknown keys survive
///
/// Both levels round-trip what they do not understand. A `harness.codex` block
/// written by a newer cImp (or by hand) keeps its fields through a load/save on
/// a build that has no such harness — [`Self::unknown`] catches anything
/// outside the core fields, and an ext key nobody declares is left alone. A
/// downgrade that silently deleted the settings of the harness you just
/// upgraded for is the failure this exists to prevent.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HarnessSettings {
    /// V38 F-3: advertise the `run_command` MCP tool — the registry's runnable
    /// `command`-kind entries under one tool with a `tool` enum — to this
    /// harness's tabs. Was `tool_plugins.expose_commands_{claude,opencode}`.
    ///
    /// Default **on**, and the real gate is elsewhere: the tool is hidden
    /// unless at least one `command`-kind entry is enabled AND path-configured,
    /// which on a fresh install is none of them.
    pub expose_commands: bool,
    /// V26: advertise the `cimp-code-audit` MCP server to this harness's tabs.
    /// Was `code_audit.expose_{claude,opencode}`. ANDed with the master
    /// `code_audit.enabled`, and re-checked per run at `/audit/run`, so
    /// unchecking it blocks scans immediately rather than at the next restart.
    pub expose_code_audit: bool,
    /// Latest version of this harness cImp has observed — Claude's from the
    /// transcript `version` field, OpenCode's from `opencode --version` at tab
    /// spawn. Empty until the harness has run once. Written by
    /// `settings::persistence::note_harness_version`, change-guarded.
    pub last_seen: String,
    /// The version this harness's MAINTENANCE.md contract checks were last run
    /// against — by the Advisor card's *Mark verified*, by hand, or by an
    /// all-pass auto-verify run. `last_seen != last_verified` is the version
    /// tripwire's condition.
    pub last_verified: String,
    /// V39 Phase B, per-harness since V40 Phase B (amendment 0-f): outcome of
    /// **this harness's** input-profile spike —
    /// `"unverified" | "pass" | "fail"`.
    ///
    /// The question it records: *does this harness's TUI on this machine accept
    /// a pasted multi-line request as ONE turn?* No payload reveals it and no
    /// fixture can settle it (the same class as [`HarnessVersions::e1_status`]
    /// and `d0_status`), and getting it wrong is silent — a split paste makes
    /// the worker answer a truncated question perfectly.
    ///
    /// It was ONE scalar for all harnesses until V40 Phase B, which is two
    /// defects in one field: a `"fail"` recorded against one TUI removed every
    /// `delegate_task_*` tool and refused delegation for every harness, and a
    /// `"pass"` recorded against Claude silently vouched for a harness nobody
    /// had ever typed into. The 35 -> 36 migration copies the single value into
    /// every existing key — the honest carry-over, since the recorded spike was
    /// in fact run against whichever harnesses the user had.
    ///
    /// **Input to a gate, not a gate.** The one query that interprets it is
    /// [`crate::harness::contract::gate_for`] keyed by `delegation.worker`, and
    /// it resolves the row of the WORKER's harness.
    pub input_profile_status: String,
    /// V35 Phase F: this harness's last **automatic** verification run — the L1
    /// embedded canaries plus the L2 live probes, run in the background when
    /// [`Self::last_seen`] changes and once at startup when it does not match
    /// [`Self::last_verified`].
    ///
    /// `None` until the first run, which is a genuinely different state from
    /// "ran and passed": it is what makes the version tripwire the
    /// *cannot-verify fallback* it became in Phase F (see
    /// `harness::verify::supersedes_tripwire`).
    pub auto_verify: Option<AutoVerify>,
    /// This harness's OWN settings — the plugin's `settings_schema()` fields,
    /// stored opaquely. See the type docs.
    pub ext: BTreeMap<String, serde_json::Value>,
    /// Anything else the file carried. See *Unknown keys survive* above.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl Default for HarnessSettings {
    fn default() -> Self {
        Self {
            expose_commands: true,
            expose_code_audit: true,
            last_seen: String::new(),
            last_verified: String::new(),
            input_profile_status: SPIKE_UNVERIFIED.to_string(),
            auto_verify: None,
            ext: BTreeMap::new(),
            unknown: serde_json::Map::new(),
        }
    }
}

/// Redacts every `ext` value any registered plugin declares `secret`.
///
/// The union across plugins rather than this row's own harness, because a
/// `HarnessSettings` does not carry its id — and over-redacting a log line is
/// free where under-redacting one writes an auth token into the rolling log.
/// This is the defense-in-depth `ClaudeLocalSettings`'s hand-rolled `Debug`
/// carried before its three fields became `ext` rows.
impl std::fmt::Debug for HarnessSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ext: BTreeMap<&str, serde_json::Value> = self
            .ext
            .iter()
            .map(|(k, v)| {
                if !secret_ext_key(k) {
                    return (k.as_str(), v.clone());
                }
                let shown = if v.as_str().is_some_and(str::is_empty) {
                    "<empty>"
                } else {
                    "<redacted>"
                };
                (k.as_str(), serde_json::Value::String(shown.to_string()))
            })
            .collect();
        f.debug_struct("HarnessSettings")
            .field("ext", &ext)
            .field("expose_commands", &self.expose_commands)
            .field("expose_code_audit", &self.expose_code_audit)
            .field("last_seen", &self.last_seen)
            .field("last_verified", &self.last_verified)
            .field("input_profile_status", &self.input_profile_status)
            .field("auto_verify", &self.auto_verify)
            .field("unknown", &self.unknown)
            .finish()
    }
}

/// Whether ANY registered plugin declares `key` a credential.
fn secret_ext_key(key: &str) -> bool {
    crate::harness::registry::HARNESSES.iter().any(|d| {
        d.plugin
            .settings_schema()
            .iter()
            .any(|f| f.key == key && f.secret)
    })
}

/// The recorded-spike value meaning "nobody has run this check".
pub const SPIKE_UNVERIFIED: &str = "unverified";

impl HarnessSettings {
    /// What an ABSENT `harness[<id>]` key reads as: the core defaults above
    /// plus every field `id`'s plugin declares, at its declared default.
    pub fn defaults_for(h: crate::harness::HarnessId) -> Self {
        let mut out = Self::default();
        if let Some(p) = h.plugin() {
            for field in p.settings_schema() {
                out.ext
                    .insert(field.key.to_string(), field.default.to_json());
            }
        }
        out
    }
}

// ── the per-harness map: defaults, accessors, parse-boundary validation ─────

/// Every registered harness's row at its defaults — what a fresh install
/// materializes into `Settings::harness`.
///
/// Materialized rather than left implicit so the block is visible and
/// hand-editable in `settings.json`; the accessors below fall back to the same
/// values, so a deleted key behaves identically to a present one.
pub fn default_harness_settings() -> BTreeMap<String, HarnessSettings> {
    crate::harness::registry::HARNESSES
        .iter()
        .map(|d| (d.id.to_string(), HarnessSettings::defaults_for(d.harness())))
        .collect()
}

/// The fallback row for a [`crate::harness::HarnessId`] with no registry slot
/// (`HarnessId::ANY`). Core defaults, no `ext`.
fn neutral_harness_settings() -> &'static HarnessSettings {
    static NEUTRAL: std::sync::OnceLock<HarnessSettings> = std::sync::OnceLock::new();
    NEUTRAL.get_or_init(HarnessSettings::default)
}

/// The defaults, built once, so [`Settings::harness_settings`] can hand out a
/// reference for an absent key instead of cloning one per call.
fn harness_defaults() -> &'static crate::harness::PerHarness<HarnessSettings> {
    static DEFAULTS: std::sync::OnceLock<crate::harness::PerHarness<HarnessSettings>> =
        std::sync::OnceLock::new();
    DEFAULTS.get_or_init(|| crate::harness::PerHarness::from_fn(HarnessSettings::defaults_for))
}

impl Settings {
    /// **The** per-harness settings read (V40 locked decision 5).
    ///
    /// Takes a [`crate::harness::HarnessId`], so no caller spells a harness
    /// name, and answers the declared defaults for a key the file does not
    /// carry — which is what lets a harness registered later work with no
    /// migration and no backfill.
    pub fn harness_settings(&self, h: crate::harness::HarnessId) -> &HarnessSettings {
        h.id()
            .and_then(|id| self.harness.get(id))
            .or_else(|| harness_defaults().get(h))
            .unwrap_or_else(|| neutral_harness_settings())
    }

    /// The writable row, created at its declared defaults if absent.
    ///
    /// `None` for an id with no registry slot: a write about a harness nobody
    /// registered would invent a key no reader could ever resolve.
    pub fn harness_settings_mut(
        &mut self,
        h: crate::harness::HarnessId,
    ) -> Option<&mut HarnessSettings> {
        let id = h.id()?;
        Some(
            self.harness
                .entry(id.to_string())
                .or_insert_with(|| HarnessSettings::defaults_for(h)),
        )
    }

    /// One declared `ext` value, or its declared default.
    ///
    /// Core calls this only on a plugin's behalf (the spawn signature, the
    /// Settings form); the plugin that declared the key is the one that names
    /// it.
    pub fn harness_ext(&self, h: crate::harness::HarnessId, key: &str) -> serde_json::Value {
        if let Some(v) = self.harness_settings(h).ext.get(key) {
            return v.clone();
        }
        h.plugin()
            .and_then(|p| p.settings_schema().iter().find(|f| f.key == key))
            .map(|f| f.default.to_json())
            .unwrap_or(serde_json::Value::Null)
    }

    /// [`Self::harness_ext`] as a `bool`. A value of the wrong type reads as
    /// the declared default — the parse boundary should already have replaced
    /// it, and answering `false` for a hand-edited `"yes"` would silently turn
    /// a protection off.
    pub fn harness_ext_bool(&self, h: crate::harness::HarnessId, key: &str) -> bool {
        let declared = h
            .plugin()
            .and_then(|p| p.settings_schema().iter().find(|f| f.key == key))
            .map(|f| matches!(f.default, crate::harness::plugin::SettingDefault::Bool(true)))
            .unwrap_or(false);
        self.harness_ext(h, key).as_bool().unwrap_or(declared)
    }

    /// [`Self::harness_ext`] as a `String`; empty for anything not a string.
    pub fn harness_ext_str(&self, h: crate::harness::HarnessId, key: &str) -> String {
        self.harness_ext(h, key)
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Write one `ext` value. Silently ignored for an unregistered id, for the
    /// reason [`Self::harness_settings_mut`] answers `None`.
    pub fn set_harness_ext(
        &mut self,
        h: crate::harness::HarnessId,
        key: &str,
        value: serde_json::Value,
    ) {
        if let Some(row) = self.harness_settings_mut(h) {
            row.ext.insert(key.to_string(), value);
        }
    }

    /// **The parse boundary for the harness map** (global principle 4:
    /// declared is not enforced).
    ///
    /// Three things, in this order, and each one is a decision:
    ///
    /// 1. Every registered harness gets a row, materialized at its defaults.
    ///    Visible in the file, hand-editable, and identical to what the
    ///    accessors would have answered.
    /// 2. Every DECLARED `ext` key is type-checked against its
    ///    `SettingKind`; a value the kind rejects is replaced by the declared
    ///    default and logged. A hand-edited `"statusline": "yes"` would
    ///    otherwise reach the launch path as a string that every reader
    ///    answers `false` for — a protection silently off, with the file
    ///    saying it is on.
    /// 3. An UNDECLARED key is left exactly as it is. Not a leniency gap: a key
    ///    a newer cImp declares must survive a downgrade, and deleting it here
    ///    would make "open the old build once" a data-loss operation. The same
    ///    reasoning keeps an unregistered harness's whole row.
    ///
    /// Returns `true` if anything changed, so the caller can decide whether to
    /// write back.
    pub fn normalize_harness_settings(&mut self) -> bool {
        let mut changed = false;
        for d in crate::harness::registry::HARNESSES {
            let h = d.harness();
            let schema = d.plugin.settings_schema();
            let row = self
                .harness
                .entry(d.id.to_string())
                .or_insert_with(|| {
                    changed = true;
                    HarnessSettings::defaults_for(h)
                });
            for field in schema {
                match row.ext.get(field.key) {
                    None => {
                        row.ext
                            .insert(field.key.to_string(), field.default.to_json());
                        changed = true;
                    }
                    Some(v) if !field.kind.accepts(v) => {
                        tracing::warn!(
                            harness = d.id,
                            key = field.key,
                            "settings: `harness.{}.ext.{}` holds a value its declared kind \
                             rejects; reset to the declared default",
                            d.id,
                            field.key
                        );
                        row.ext
                            .insert(field.key.to_string(), field.default.to_json());
                        changed = true;
                    }
                    Some(_) => {}
                }
            }
        }
        changed
    }
}

/// Test-only conveniences for the per-harness map.
///
/// Every one of these names a harness, which is exactly what a fixture is for:
/// `layering`'s identity scan drops test regions, and what it polices is a
/// PRODUCTION path spelling a harness name.
#[cfg(test)]
impl Settings {
    /// One harness's row, created at its declared defaults if absent.
    pub(crate) fn harness_row(&mut self, id: &str) -> &mut HarnessSettings {
        let h = crate::harness::HarnessId::from_id(id).expect("registered harness");
        self.harness_settings_mut(h).expect("registered harness")
    }

    /// One harness's row, read-only.
    pub(crate) fn harness_row_of(&self, id: &str) -> &HarnessSettings {
        let h = crate::harness::HarnessId::from_id(id).expect("registered harness");
        self.harness_settings(h)
    }

    /// Write one declared `ext` value.
    pub(crate) fn set_ext(&mut self, id: &str, key: &str, v: serde_json::Value) {
        let h = crate::harness::HarnessId::from_id(id).expect("registered harness");
        self.set_harness_ext(h, key, v);
    }
}

/// Build an [`McpServerConfig::access`] map from `(id, granted)` pairs —
/// **tests only**, for the same reason as `harness::per_harness_for_test`.
#[cfg(test)]
pub fn access_for_test(pairs: &[(&str, bool)]) -> BTreeMap<String, McpAccess> {
    pairs
        .iter()
        .map(|(id, on)| ((*id).to_string(), McpAccess { enabled: *on }))
        .collect()
}

/// Per-server MCP access for ONE harness — the value type of
/// [`McpServerConfig::access`].
///
/// A struct rather than a bare `bool` because that is the shape the pair it
/// replaced was already growing toward (V37 added a per-server *activation*
/// question beside the grant, and V38 an audit one), and widening a
/// `BTreeMap<String, bool>` later would be a second schema bump for a field
/// that could have carried it from the start.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct McpAccess {
    /// Expose this server's tools to this harness, proxied through its
    /// per-session `cimp-offload --consumer <id>` child. Off by default: a new
    /// server reaches nothing until the user says so.
    pub enabled: bool,
}

/// One provider/model price entry: USD per million tokens (MTok) for the four
/// billing categories the transcripts report (`UsageTotals`' `in_tok` /
/// `cache_make` / `cache_read` / `out_tok`). Fully user-editable in
/// Settings → LLM pricing; the session-cost popup multiplies these against a
/// session's token totals. `(provider, model)` is the display identity — no
/// uniqueness is enforced, the popup just lists rows in order.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct LlmPricingModel {
    pub provider: String,
    pub model: String,
    /// V16 Feature 8: transcript model-id prefix this row auto-matches
    /// (e.g. `"claude-opus-4-8"` matches both the bare alias and dated
    /// snapshots). Longest matching prefix wins; empty = manual-pick only
    /// (the row still appears in the cost popup's dropdown).
    pub model_prefix: String,
    /// $/MTok for uncached input tokens.
    pub input: f64,
    /// $/MTok for cache-write (cache-creation) tokens.
    pub cache_write: f64,
    /// $/MTok for cache-read tokens.
    pub cache_read: f64,
    /// $/MTok for output tokens.
    pub output: f64,
}

// The seeded price table and its top-up watermark moved to `crate::pricing`
// (V40 locked decision 29): they are **provider** knowledge, not harness
// knowledge and not a persisted shape. `LlmPricingModel` above is the on-disk
// row and stays here; `default_llm_pricing` / `pricing_rows_since` /
// `PRICING_GENERATION` are `crate::pricing`'s.

/// V14 Phase D2: one dismissed advisor proposal. `rule_id` mirrors
/// `advisor::Proposal::rule_id` (a versioned string constant, e.g.
/// `"advisor.raise_context_min_score.v1"`); `signature` is the coarse
/// (10%-bucketed) rate that triggered the dismissed proposal. Equality on
/// BOTH fields is what "suppressed" means — see `advisor::evaluate`.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct DismissedRule {
    pub rule_id: String,
    pub signature: String,
}

/// One APPLIED advisor proposal — the Apply-cooldown's memory. `rule_id`
/// mirrors `advisor::Proposal::rule_id`; `root` is the project root the
/// Apply happened in (the advisor's rates are per-root, so a cooldown in one
/// project must not mute another); `session_count` is that root's distinct
/// session count at apply time. The rule stays quiet until the root has seen
/// `advisor::APPLY_COOLDOWN_SESSIONS` further sessions — see
/// `advisor::evaluate`.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AppliedRule {
    pub rule_id: String,
    pub root: String,
    pub session_count: u64,
}

/// Logging configuration. The file path is fixed at
/// `<portable-root>/logs/cimp.log.<YYYY-MM-DD>`; the `level` field drives
/// the live filter and `retention` drives the startup cleanup pass.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct LoggingSettings {
    pub level: LogLevel,
    pub retention: LogRetention,
    pub content_capture: ContentCaptureSettings,
}

/// Per-tab content (PTY raw output) capture configuration. Disabled by
/// default — when enabled, every byte read from each tab's PTY is also
/// appended to `<portable-root>/logs/content/<tab-id>.log.<YYYY-MM-DD>`,
/// rotated daily by `tracing-appender`. `retention` runs the same
/// max-age cleanup as the tracing logs but against the `content/`
/// subdirectory.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ContentCaptureSettings {
    pub enabled: bool,
    pub retention: LogRetention,
}

impl Default for ContentCaptureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            retention: LogRetention::Weekly,
        }
    }
}

/// Per-process tracing-filter level. Mapped to an `EnvFilter` string by
/// `as_filter_str`; serialized as a lowercase string. The `RUST_LOG`
/// environment variable, when set, overrides this at startup.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_filter_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// How long to keep rolled log files before the startup cleanup pass
/// removes them. `Never` skips cleanup entirely. Computed as a max-age
/// against each file's mtime in `logging::run_cleanup`.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogRetention {
    Daily,
    #[default]
    Weekly,
    Monthly,
    Never,
}

impl LogRetention {
    /// Max-age threshold for cleanup. `None` means "keep everything".
    /// Approximate calendar values (1d / 7d / 30d) — exact day-boundary
    /// alignment isn't necessary for log retention.
    pub fn max_age(self) -> Option<std::time::Duration> {
        const DAY: u64 = 24 * 60 * 60;
        match self {
            Self::Daily => Some(std::time::Duration::from_secs(DAY)),
            Self::Weekly => Some(std::time::Duration::from_secs(7 * DAY)),
            Self::Monthly => Some(std::time::Duration::from_secs(30 * DAY)),
            Self::Never => None,
        }
    }
}

/// One of the three reserved AI-tool tab ids. Wire format is the kebab-
/// case tab-id string (`"claude"`, `"claude-local"`, `"opencode"`); the type
/// exists so `enabled_ai_tabs` can be a strongly-typed `Vec<AiTabId>` instead
/// of an untyped string list.
///
/// V19 ships a single OpenCode tab (not a cloud/local pair like Claude):
/// OpenCode addresses many providers as `provider/model`, switches between
/// them in-session, and reads global config/credentials, so a local variant
/// would be redundant — the one tab covers cloud and local.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum AiTabId {
    Claude,
    ClaudeLocal,
    // Explicit rename: serde's kebab-case would split the camelCase variant
    // name into `open-code`, but the wire format must be the single-word tab id
    // `opencode` (matching OPENCODE_TAB_ID, the migration output, and the
    // frontend literals). A mismatch quarantines settings on load — see the
    // round-trip test.
    #[serde(rename = "opencode")]
    OpenCode,
}

impl AiTabId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => CLAUDE_TAB_ID,
            Self::ClaudeLocal => CLAUDE_LOCAL_TAB_ID,
            Self::OpenCode => OPENCODE_TAB_ID,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            CLAUDE_TAB_ID => Some(Self::Claude),
            CLAUDE_LOCAL_TAB_ID => Some(Self::ClaudeLocal),
            OPENCODE_TAB_ID => Some(Self::OpenCode),
            _ => None,
        }
    }

    /// True for the local-provider variants (`claude-local`). The integrity
    /// check uses this as the canonical `use_local_provider` value for each
    /// reserved id. OpenCode picks its own provider, so it is never "local".
    pub fn uses_local_provider(self) -> bool {
        matches!(self, Self::ClaudeLocal)
    }

    /// Canonical tab-bar position: claude (0) → claude-local → opencode,
    /// with shells trailing afterwards. Used by `add_ai_builtin_tab` and
    /// `integrity_check` so re-adding a previously-disabled AI tab lands in
    /// the same slot every time.
    pub fn canonical_order(self) -> usize {
        // V40 Phase A: the registry's canonical order, not a literal ranking.
        // A reserved tab id no descriptor claims sorts last rather than
        // colliding with slot 0 — an unknown built-in is not "the first one".
        canonical_ai_tab_order()
            .iter()
            .position(|&id| id == self)
            .unwrap_or(usize::MAX)
    }
}

/// The reserved AI tab ids in **canonical tab-bar order**, flattened out of the
/// registry (`claude` → `claude-local` → `opencode` today).
///
/// V40 Phase A: this used to be a literal `[AiTabId::Claude, ..]` array written
/// out in three places (`persistence::restore_enabled_ai_builtins`,
/// `ipc::tab_lifecycle`, and `AiTabId::canonical_order`), which is three places
/// a new harness's tab could be forgotten and only one of them would have said
/// so. The order is the descriptors' declaration order and each descriptor's own
/// `tab_ids` order — so a harness owns where its tabs sit relative to its own,
/// and the registry owns where harnesses sit relative to each other.
pub fn canonical_ai_tab_order() -> Vec<AiTabId> {
    crate::harness::registry::canonical_tab_ids()
        .into_iter()
        .filter_map(AiTabId::from_id)
        .collect()
}

impl Settings {
    /// Lookup a tab entry by id. Returns None for ids that don't exist —
    /// callers (launch flow, lifecycle commands) treat this as "tab gone"
    /// rather than constructing a default.
    pub fn find_tab(&self, id: &str) -> Option<&TabConfig> {
        self.tabs.iter().find(|t| t.id() == id)
    }

    pub fn find_tab_mut(&mut self, id: &str) -> Option<&mut TabConfig> {
        self.tabs.iter_mut().find(|t| t.id() == id)
    }

    /// Whether the loopback endpoint (and the offload runtime that owns it)
    /// must run: true when ANY feature whose out-of-process children — MCP
    /// stdio servers, hook shims — dial back into the app over the loopback
    /// is on. The advertise gates in `tabs::config` (`build_pre_args`,
    /// `build_opencode_config`) must stay a subset of this: advertising a
    /// server whose endpoint never starts strands every tool call with
    /// "cImp is not running" while the app is visibly running (the V26 Code
    /// Audit gap — `offload.mcp_host_needed()` alone misses `graph` and
    /// `code_audit`, which both inject servers on their own).
    ///
    /// **The subset rule covers HOOK SHIMS too, not just MCP servers.** Every
    /// shim in the Claude `--settings` overlay (`--context-hook`,
    /// `--precompact-hook`, `--read-hook`, `--postedit-hook`, and the NC-2
    /// `--notify-hook` permission shim) reaches the app only through
    /// `post_loopback`. The gated ones ride `graph.enabled`, which implies this
    /// predicate; the NC-2 pair has no feature toggle of its own and is
    /// therefore gated on `loopback_needed()` directly (H2, 2026-08-05 review)
    /// — injecting it without the endpoint spawned a shim process per Claude
    /// notification whose POST was dropped, silently killing the PRIMARY
    /// permission signal on a default install. Tripwire:
    /// `tabs::config::tests::every_advertised_mcp_server_gets_a_loopback`.
    pub fn loopback_needed(&self) -> bool {
        self.offload.mcp_host_needed() || self.graph.enabled || self.code_audit.mcp_exposed(self)
    }

    /// **The whole offload pool: configured backends + the V39 facades.**
    ///
    /// [`OffloadSettings::effective_backends`] cannot answer this and should not
    /// try: the facades are synthesized from *tab roles*, which live on
    /// [`Settings::tabs`], one level above the offload block. So the wrapper
    /// lives here, and every consumer that must see a facade calls this one
    /// instead of the inner method:
    ///
    /// | caller | why |
    /// |---|---|
    /// | `offload::service::resolve_pool` | routing — the facade must be a candidate |
    /// | `offload::service::describe` | the live `offload_task` prose |
    /// | `offload::service::compute_global_cap` | a facade is one more concurrent slot |
    /// | the metrics poller / `supervisor::statuses` | the dashboard rows |
    /// | `offload::mcp::offload_task_description` | the child's config-derived prose |
    ///
    /// The ones deliberately left on the raw list are the ones that are about a
    /// *process* or a *URL*, neither of which a facade has:
    /// `supervisor::local_backends` (what to spawn),
    /// [`OffloadSettings::primary_local_command`] (the OpenCode provider), and
    /// `outbound::Policy::from_settings` (SSRF carve-outs — a facade has no
    /// endpoint to carve out). The **backend editor's save path** is a fourth:
    /// it edits `offload.backends` directly and must never see a synthesized
    /// entry, which is what
    /// `a_synthesized_facade_never_reaches_the_persisted_backend_list` pins.
    ///
    /// Facades are appended in **both** branches of `effective_backends` — the
    /// configured-pool branch and the legacy synthesized-local one — because a
    /// user whose only backend is a harness tab has a pool of exactly one, and a
    /// facade that only appeared next to a configured HTTP backend would be a
    /// feature you had to own a llama-server to use.
    pub fn effective_offload_backends(&self) -> Vec<OffloadBackend> {
        let mut out = self.offload.effective_backends();
        for cfg in &self.tabs {
            let TabConfig::AiTool(c) = cfg else { continue };
            if c.delegation_role != DelegationRole::RemoteOffload {
                continue;
            }
            let name = c
                .delegation_backend
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| facade_default_name(&c.id));
            // A configured backend wins the name, and the facade is dropped
            // rather than renamed: the router, the run log and the dashboard all
            // key on the name, so two entries answering to one name is a bug
            // with no good half. Warned ONCE per name per process — this runs on
            // every route, every describe and every 600 ms dashboard tick.
            if out.iter().any(|b| b.name == name) {
                warn_backend_name_collision(&name, &c.id);
                continue;
            }
            out.push(OffloadBackend {
                name,
                enabled: true,
                kind: OffloadBackendKind::HarnessTab { tab: c.id.clone() },
                declared_context: c.delegation_backend.declared_context,
                declared_model: String::new(),
                tier: c.delegation_backend.tier,
                // Locked decision 3: the driver must not be able to tell a
                // facade from an HTTP backend, and a narrowed scope is visible
                // in the tool prose. It is also the honest value — the worker
                // tab runs its own harness with its own tools, and cImp does not
                // filter them.
                tool_scope: ToolScope::All,
            });
        }
        out
    }
}

/// **The backend name a Remote-offload tab takes when the user picks none**
/// (V39 review L-2).
///
/// It used to be the TAB's display name, and that name is rendered into
/// `offload_task`'s description (`offload::mcp::backend_label`) and into every
/// result the driver reads — so a tab called "Claude — API work" told the
/// asking model exactly what its "LAN backend" really was. Locked decision 3 is
/// that a driver must not be able to tell a facade from an HTTP backend; a
/// default that leaks the tab is that decision failing open on the path most
/// users will never touch.
///
/// So: `worker-<4 hex>` over a hash of the tab ID. Stable (the id does not
/// change when the tab is renamed), non-identifying (a hash of an opaque id),
/// and short enough to be a name a person can repeat. FNV-1a over the id's
/// UTF-8 bytes, because the frontend has to produce the SAME string for the
/// popover's placeholder and the Settings list — `src/lib/delegation.ts`'s
/// `defaultFacadeName` is the mirror, and a test on each side pins the pair.
pub fn facade_default_name(tab_id: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in tab_id.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("worker-{:04x}", (h >> 16) as u16)
}

/// One warning per colliding backend name per process.
///
/// Not a rate limiter and not a counter: the collision is a *configuration*
/// fact, so it is worth saying once and worth never saying again until the user
/// changes something. Keyed by name so a second, different collision still
/// speaks.
fn warn_backend_name_collision(name: &str, tab: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut g = seen.lock().unwrap_or_else(|e| e.into_inner());
    if g.insert(name.to_string()) {
        tracing::warn!(
            backend = %name,
            tab = %tab,
            "offload: a configured backend already answers to this name, so the Remote-offload              tab is NOT in the pool — rename one of them (the tab's backend name is in its              delegation popover)"
        );
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct SessionState {
    pub active_tab_id: Option<String>,
}

/// Persisted layout state. Mirrors the frontend's `LayoutState` 1:1 — the
/// `type` discriminator on `LayoutNodePersisted` matches the frontend's
/// `'split' | 'pane'` shape, so serialize/deserialize is identity work
/// across the IPC boundary.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LayoutPersisted {
    pub tree: LayoutNodePersisted,
    pub focused_pane_id: String,
}

/// Recursive layout-tree node. Splits are internal (two children + ratio +
/// direction); panes are leaves (ordered tab id list + per-pane active tab
/// id).
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNodePersisted {
    Split {
        id: String,
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNodePersisted>,
        second: Box<LayoutNodePersisted>,
    },
    Pane {
        id: String,
        tab_ids: Vec<String>,
        active_tab_id: Option<String>,
    },
}

/// Direction of a Split node. Naming matches CSS flexbox: `Horizontal`
/// arranges children side-by-side (vertical splitter between them);
/// `Vertical` stacks them top-to-bottom. See DESIGN.md for the
/// rationale for this convention.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// A named layout preset. The tree is the layout-only payload — focus and
/// the live `focused_pane_id` are intentionally not persisted with the
/// preset, since restoring a preset is "set up panes this way" and focus
/// follows the user's next click.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LayoutPreset {
    pub name: String,
    /// RFC 3339 / ISO 8601 timestamp (UTC, second precision). Used to
    /// order the popover's "Recent presets" list. Renames do not refresh
    /// this — it remains the original creation time.
    pub created_at: String,
    pub tree: LayoutNodePersisted,
}

/// Tolerant deserializer for `Settings::layout`. Parses to a generic
/// `Value` first, then attempts the typed conversion; any failure (a
/// malformed/partial node, a `Split` missing `ratio`, a hand-edit that
/// broke the tree) degrades to `None` with a warning instead of failing
/// the whole `Settings` parse. The frontend rebuilds a default single-pane
/// tree when the layout is `None`, so the user loses only the broken layout
/// — not their entire per-folder overlay.
fn deserialize_lenient_layout<'de, D>(d: D) -> Result<Option<LayoutPersisted>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(d)?;
    match raw {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(val) => match serde_json::from_value::<LayoutPersisted>(val) {
            Ok(layout) => Ok(Some(layout)),
            Err(e) => {
                tracing::warn!(error = %e, "settings: malformed layout dropped to None");
                Ok(None)
            }
        },
    }
}

/// Tolerant deserializer for `Settings::layout_presets`. Drops individual
/// malformed presets (keeping the valid ones) and tolerates the field not
/// being an array at all, rather than aborting the entire settings load.
fn deserialize_lenient_presets<'de, D>(d: D) -> Result<Vec<LayoutPreset>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(d)?;
    let serde_json::Value::Array(items) = raw else {
        if !raw.is_null() {
            tracing::warn!("settings: layout_presets was not an array; ignoring");
        }
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match serde_json::from_value::<LayoutPreset>(item) {
            Ok(p) => out.push(p),
            Err(e) => tracing::warn!(error = %e, "settings: malformed layout preset dropped"),
        }
    }
    Ok(out)
}

/// Discriminated tab config. The `kind` field is the JSON discriminator
/// (`"ai_tool"`, `"shell"`, or — V14 Phase F — `"preview"`), produced by
/// serde's internally-tagged representation. Each variant carries the fields
/// specific to its kind — AI tabs have `tts_injection` and three notification
/// slots; Shell tabs have two notification slots and no TTS hook; Preview
/// tabs have neither (no PTY at all — `url`/`device_width`/`auto_reload`
/// drive an embedded child webview instead, see `crate::preview`).
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabConfig {
    AiTool(AiToolTabConfig),
    Shell(ShellTabConfig),
    Preview(PreviewTabConfig),
}

impl TabConfig {
    pub fn id(&self) -> &str {
        match self {
            TabConfig::AiTool(c) => &c.id,
            TabConfig::Shell(c) => &c.id,
            TabConfig::Preview(c) => &c.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            TabConfig::AiTool(c) => &c.name,
            TabConfig::Shell(c) => &c.name,
            TabConfig::Preview(c) => &c.name,
        }
    }

    pub fn set_name(&mut self, name: String) {
        match self {
            TabConfig::AiTool(c) => c.name = name,
            TabConfig::Shell(c) => c.name = name,
            TabConfig::Preview(c) => c.name = name,
        }
    }

    pub fn builtin(&self) -> bool {
        match self {
            TabConfig::AiTool(c) => c.builtin,
            TabConfig::Shell(c) => c.builtin,
            TabConfig::Preview(c) => c.builtin,
        }
    }

    pub fn set_builtin(&mut self, value: bool) {
        match self {
            TabConfig::AiTool(c) => c.builtin = value,
            TabConfig::Shell(c) => c.builtin = value,
            TabConfig::Preview(c) => c.builtin = value,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AiToolTabConfig {
    pub id: String,
    pub builtin: bool,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// `None` (the default for every builtin and every plain "+"-duplicated
    /// tab) ⇒ spawn with the app's launch directory, same as always. V13
    /// Phase D's "New tab in worktree…" flow
    /// (`ipc::tab_lifecycle::create_ai_tab_in_worktree`) is the one place
    /// that sets this — to the freshly created worktree's path — so the tab
    /// runs isolated from the main working tree. This field already existed
    /// (mirroring `ShellTabConfig::cwd`, wired into `build_ai_tool_spec`
    /// since V3) but was never set by any flow until Phase D; there is no
    /// user-facing "set a custom cwd" affordance for AI tabs, so a non-`None`
    /// value always means "this tab lives in a cImp-managed worktree" — shown
    /// read-only where the tab's Configure surface displays it.
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub tts_injection: TtsInjection,
    pub notifications: AiNotificationConfig,
    /// Carried over from earlier schemas where aider had a one-time
    /// first-launch banner. Aider is gone (V1.4-07) and Claude tabs
    /// pre-dismiss this; left in place to keep the wire format stable
    /// for users still loading older settings files.
    pub first_launch_notice_dismissed: bool,
    /// V1.4-01 per-tab terminal palette override. `None` means inherit
    /// the global `terminal.theme`; `Some(_)` replaces it with the
    /// override's bundled name (or Custom block) for this tab only.
    /// The override travels with the tab through drag-and-drop because
    /// it lives on the tab itself, not on a pane.
    pub theme_override: Option<TerminalThemeSettings>,
    /// V1.4-02 per-tab background override (three-state). `None` means
    /// inherit the global `terminal.background`; `Some(Disabled)` means
    /// opt out (theme bg only); `Some(Custom(cfg))` replaces the global
    /// background wholesale for this tab.
    pub background_override: Option<BackgroundOverride>,
    /// V1.4-07: when `true`, the launch-time env composition synthesizes
    /// `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` (and `ANTHROPIC_MODEL`
    /// if `claude_local.model_alias` is non-empty) from the global
    /// `claude_local` settings group. Per-tab `env` entries override
    /// synthesized values.
    pub use_local_provider: bool,
    /// V32 Phase G (locked decision 16): this tab's **L3** row — a tri-state
    /// `Inherit | On | Off` per injection-protection feature, defaulting to
    /// `Inherit` everywhere so an untouched tab behaves exactly as before.
    ///
    /// Only the features that HAVE a tab scope have a cell (see
    /// [`TabInjectionOverrides`](crate::settings::injection::TabInjectionOverrides)):
    /// the worker-only canary and the app-wide terminal-escape hygiene are
    /// structurally absent rather than present-and-ignored.
    ///
    /// Two of the cells (native-web visibility, consumer hygiene) are
    /// spawn-baked and therefore ride `spawn_inject_sig`, so flipping them
    /// raises the "restart the AI tab" hint; the rest take effect on the next
    /// call.
    ///
    /// `pub(in crate::settings)` (#44): an L3 cell answers a *different*
    /// question from `effective(feature, scope, settings)` — it ignores the
    /// global master and the app-wide flag — so reading one outside the resolver
    /// is the same defect as reading a raw L1/L2 switch, and is now the same
    /// compile error. Test code outside `crate::settings` writes cells through
    /// `Settings::set_tab_override_for_test`.
    pub(in crate::settings) injection_overrides: crate::settings::injection::TabInjectionOverrides,
    /// V39 Phase A (locked decision 4): the user's sticky **read-only** lock
    /// on this tab — the keyboard is refused, the tab keeps running.
    ///
    /// Only the `User` source is persisted. The engine's transient `Driven`
    /// lock lives in `state::ReadOnlyTabs` and is deliberately absent here:
    /// after a crash mid-delegation nothing is in flight, so a persisted
    /// `Driven` would be a lock with no owner to lift it.
    ///
    /// Additive `#[serde(default)]` (container level) ⇒ every existing
    /// settings file loads with the tab writable, which is the pre-V39
    /// behaviour. Read-only governs the *user's* keyboard only: a locked tab
    /// is still a valid delegation worker.
    ///
    /// **Not spawn-baked** — it is enforced per write in `pty_write`, so
    /// flipping it never asks for a tab restart (`spawn_inject_sig` has no
    /// slot for it, and a test pins that).
    pub read_only: bool,
    /// V39 Phase B (locked decision 8): what this tab is **for** in the
    /// delegation surface — the single source of truth for both driver modes.
    ///
    /// Persisted and restored at startup (in-flight state never is), and
    /// exclusive by construction: the roles are one enum, not two flags, so a
    /// tab cannot be both a `delegate_task_*` target and a facade backend.
    ///
    /// **Not spawn-baked** (decision 15): the `delegate_task_*` set rides the
    /// child proxy's live `tools/list` plus the V37 `list_changed` pulse, and
    /// the facade rides `offload_task`'s live description — so changing a role
    /// takes effect on the next turn without restarting either tab, and
    /// `spawn_inject_sig` has no slot for it (a test pins that).
    pub delegation_role: DelegationRole,
    /// V39 (locked decision 8): the per-backend knobs a
    /// [`DelegationRole::RemoteOffload`] tab is synthesized into
    /// `effective_backends()` with.
    ///
    /// Declared in Phase B and **consumed in Phase C**: the fields' defaults
    /// are already decided, and a container that arrives with the role it
    /// belongs to is one schema shape rather than two. Meaningless while the
    /// role is anything else — deliberately not enforced, because a user who
    /// sets a backend name, switches the role away and switches it back should
    /// find the name where they left it.
    pub delegation_backend: DelegationBackend,
}

/// V39 Phase B, locked decision 8 — **one exclusive role per tab**.
///
/// `None` is the default and the answer for every tab that has never been
/// touched: a tab becomes reachable by another harness only by an explicit user
/// action, on that tab.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRole {
    /// Not a delegation target. The default.
    #[default]
    None,
    /// The target of `delegate_task_<harness>` for this tab's harness.
    ///
    /// **At most one Manual tab per harness** — setting Manual on a second tab
    /// of the same harness MOVES the role (the previous holder drops to
    /// `None`), which is enforced in `ipc::commands::tab_set_delegation_role`
    /// rather than by this type: an enum cannot express a cross-tab
    /// uniqueness rule, and a settings file hand-edited into two Manual tabs
    /// must load rather than fail.
    Manual,
    /// A facade offload backend (Phase C): the requesting harness sees a
    /// backend name, never a tab. **Any number** per harness.
    RemoteOffload,
}

/// V39, locked decision 8 — the per-tab knobs a `RemoteOffload` tab carries
/// into the offload backend list. Phase C reads them.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct DelegationBackend {
    /// The name the requesting harness sees (`lan-worker-2`), NEVER the tab
    /// name — decision 3's facade half is only a facade if the tab does not
    /// leak through it. `None` ⇒ [`facade_default_name`], an opaque per-tab
    /// name; it used to fall back to the tab's DISPLAY name, which put the tab
    /// into the asking model's prose (V39 review L-2).
    pub name: Option<String>,
    /// Router bias, exactly as a configured HTTP backend carries it.
    pub tier: BackendTier,
    /// The worker's usable context window, in tokens, if the user knows it.
    /// `None` ⇒ Phase C uses a generous default: a facade whose context is
    /// under-declared is routed away from work it could have done, and one
    /// that is over-declared fails visibly on the worker's own side.
    pub declared_context: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct ShellTabConfig {
    pub id: String,
    pub builtin: bool,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub notifications: ShellNotificationConfig,
    /// V1.4-01 per-tab terminal palette override. `None` means inherit
    /// the global `terminal.theme`; `Some(_)` replaces it for this tab.
    /// See `AiToolTabConfig::theme_override` for the full rationale.
    pub theme_override: Option<TerminalThemeSettings>,
    /// V1.4-02 per-tab background override (three-state). See
    /// `AiToolTabConfig::background_override`.
    pub background_override: Option<BackgroundOverride>,
}

/// V14 Phase F: a user-created Preview tab — an embedded, localhost-scoped
/// child webview, not a subprocess. No `command`/`args`/`cwd`/`env`/PTY
/// fields at all (unlike `AiToolTabConfig`/`ShellTabConfig`) since there is
/// nothing to spawn; `crate::preview` manages the child webview keyed by
/// tab id, reading `url`/`device_width`/`auto_reload` from here.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct PreviewTabConfig {
    pub id: String,
    /// Always `false` in practice — Preview has no reserved/builtin instance
    /// (every one is user-created via the `+` menu), but the field exists so
    /// `TabConfig`'s shared accessors (`builtin()`/`set_builtin()`) stay
    /// uniform across variants.
    pub builtin: bool,
    pub name: String,
    pub url: String,
    /// `None` ⇒ the toolbar's "Desktop" preset (fill the available rect, no
    /// letterboxing). `Some(w)` ⇒ letterbox to a fixed CSS-pixel width (the
    /// mobile/tablet presets) — see `preview::policy` for the shared
    /// device-preset table for the rect math.
    pub device_width: Option<u32>,
    /// Reload after a ~1s quiet period following a `fs-batch` event (V13),
    /// while the tab is visible. Off by default — a dev server's own HMR
    /// usually already handles this, so auto-reload is an opt-in belt for
    /// setups without it.
    pub auto_reload: bool,
}

// `ClaudeLocalSettings` moved to the Claude plugin's declared settings in V40
// Phase B (locked decision 6): `base_url` / `auth_token` / `model_alias` are
// three `ext` rows on `Settings::harness["claude"]`, declared by
// `HarnessPlugin::settings_schema` and read only by the code that synthesizes
// the `ANTHROPIC_*` env. The hand-rolled `Debug` that redacted the token is
// now `HarnessSettings`'s, driven by the `secret` column on the declaration.

/// Optional explicit executable paths for the bundled quick-launch tools
/// (the bottom-bar rustnet / broot buttons). Each field, when non-empty, is
/// used as the launch command verbatim — overriding the normal `ebin/` → PATH
/// resolution (see `pty::resolve`) — so a user who doesn't want the bundled
/// build and doesn't have the tool on PATH can select an exe from any folder.
/// Empty (the default) means "resolve normally". Additive `#[serde(default)]`
/// block — old settings files round-trip with both fields empty.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct ExternalToolsSettings {
    /// Override for the `rustnet` tool; empty = resolve via ebin → PATH.
    pub rustnet: String,
    /// Override for the `broot` tool; empty = resolve via ebin → PATH.
    pub broot: String,
}

/// V23 Phase A: Code Audit (aggregated security scanning) config. cImp runs
/// external security scanners against the project root and aggregates their
/// SARIF output into one findings table (Phase B/C). Off by default
/// (`enabled = false`): the feature gates a reserved dashboard tab (mirrors
/// `ui.tool_activity_tab`) and the bottom-bar entry point, and nothing is
/// bundled — the tools resolve ebin → PATH — so it is strictly opt-in.
///
/// Additive `#[serde(default)]` block — old settings files round-trip with the
/// feature disabled and the three default tools present. The original V23 block
/// shipped without a schema-version bump (V8/V16 precedent); the V26 `expose_*`
/// additions ride the v23→v24 pure version stamp.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct CodeAuditSettings {
    /// Master switch. Off = no "Code audit" section in the Tool Activity tab
    /// (its reserved tab was retired in schema v27), no scanning, no
    /// bottom-bar entry.
    pub enabled: bool,
    /// Per-tool wall-clock timeout in seconds. A tool that exceeds it is killed
    /// and reported `failed` (Phase B); the other tools are unaffected.
    pub timeout_secs: u64,
    /// Keep the built-in QUALITY tools' `enabled` flags following the project's
    /// language census automatically: whenever a census is (re)taken — scan
    /// start, tab open, Settings open — each is selected iff its manifest says
    /// it is on by default AND it applies to the project (see
    /// `audit::runner::auto_select_quality`; the heavyweights
    /// `dotnet-analyzers`/`semgrep-quality` declare `enabled_by_default: false`
    /// and so stay opt-in). On by default; any manual quality-checkbox edit
    /// flips this to `false` (manual mode) so user choices stick, and the
    /// "Auto-select for this project" button turns it back on. Security tools
    /// are never touched, and neither is a user plugin's tool — auto-selection
    /// is a statement about the roster cImp ships and knows the shape of.
    ///
    /// Since schema v34 the flags it writes live in
    /// [`ToolPluginsSettings::plugins`], under the built-in audit plugin's key.
    /// This switch stays here because it is a property of the Code Audit
    /// FEATURE rather than of any one tool.
    pub quality_auto_select: bool,
    // The `expose_claude` / `expose_opencode` pair is
    // `Settings::harness[<id>].expose_code_audit` since V40 Phase B (locked
    // decision 5). `expose_offload` stays here: the offload worker is cImp's
    // own in-process consumer, not a harness, and folding it into the harness
    // map would have invented a harness to hold it.
    /// V26: advertise the code-audit native tools to the offload worker (the
    /// local model), gated in `offload::service::run_on` alongside the graph
    /// tools — enabled AND this flag AND a local backend. The scan always runs
    /// in-process via the process-global `AuditState`, but the report (repo
    /// paths + scanner messages) is local data, so remote backends are excluded
    /// exactly like the graph tools; `HostRouter::call` re-gates dispatch and
    /// `begin_scan` re-enforces the master switch on every scan.
    pub expose_offload: bool,
}

impl Default for CodeAuditSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: 600,
            quality_auto_select: true,
            // Exposure defaults on for all three consumers: the master
            // `enabled` switch (default false) is the real gate, so a fresh
            // install advertises nothing until Code Audit is turned on, at
            // which point every consumer sees the tools unless the user opts a
            // specific one out.
            expose_offload: true,
        }
    }
}

impl CodeAuditSettings {
    /// Whether the `cimp-code-audit` MCP server is advertised to at least one
    /// **harness** — i.e. an out-of-process child will need the loopback.
    ///
    /// Takes the whole `Settings` since V40 Phase B: the per-harness flags are
    /// `Settings::harness[<id>].expose_code_audit`, and this iterates the
    /// registry rather than OR-ing two named fields, so a third harness is
    /// counted the day it registers. `expose_offload` is deliberately absent:
    /// the offload worker runs in-process and is already covered by
    /// `offload.enabled`.
    pub fn mcp_exposed(&self, settings: &Settings) -> bool {
        self.enabled
            && crate::harness::registry::all()
                .any(|h| settings.harness_settings(h).expose_code_audit)
    }
}

/// V8-01: local task-offload configuration. cImp runs a user-supplied
/// `llama-server` (the single source of truth is `server_command`) and
/// exposes an `offload_task` MCP tool into cImp-launched Claude tabs so
/// the main Opus session can delegate token-heavy subtasks (deep search,
/// large-file/log summarization, web research) to the local model and
/// receive only the synthesized result. Off by default (`enabled` and
/// `autostart` both false): the feature spawns a multi-GB server and is
/// useless without a configured command, so it is opt-in.
///
/// Additive `#[serde(default)]` block — old settings files round-trip
/// with the feature disabled. No schema-version bump.
///
/// The `Debug` impl is hand-rolled to redact any secrets carried in the
/// configured MCP servers' `env` maps, mirroring `ClaudeLocalSettings`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OffloadSettings {
    /// Master switch. Off = no server, no `--mcp-config` injection into
    /// Claude tabs, `offload_task` not exposed.
    pub enabled: bool,
    /// When `enabled`, spawn `llama-server` at app launch and keep it
    /// warm. When off, the server starts lazily on the first
    /// `offload_task` (or via a manual Start) so a ~20 GB load never
    /// blocks launch.
    pub autostart: bool,
    /// Inject the system-prompt addendum that tells Opus *when* to
    /// offload. Default true — without the nudge Opus won't reach for
    /// the tool. Routed through the same `--append-system-prompt`
    /// mechanism as the TTS markup convention.
    pub inject_guidance: bool,
    /// The single source-of-truth `llama-server` command, e.g.
    /// `llama-server --model …\Qwen3.6-35B-A3B-Q4.gguf --port 8080
    /// --jinja -ngl 99 --ctx-size 150000 --flash-attn`. cImp
    /// `shlex`-parses it to spawn, parses host/port + `-np` to know
    /// where to connect and how many slots exist, and validates
    /// `--jinja` is present (tool-calling needs it). cImp never
    /// silently mutates the command. Empty on a fresh install.
    pub server_command: String,
    /// Native baseline tool on/off toggles (`read_file`, `code_search`,
    /// `run_command`).
    pub tools: OffloadToolToggles,
    /// Roots the native `code_search`/`read_file` tools and the
    /// `filesystem` MCP server are confined to. Empty default → the loop
    /// resolves it to the launch project root at call time.
    pub allowed_roots: Vec<PathBuf>,
    /// Commands the native `run_command` tool may execute. Deny by
    /// default (empty list = nothing runnable). Matched against the
    /// command's program name.
    pub command_allowlist: Vec<String>,
    /// Per-program command security policies layered on top of the
    /// allowlist by `run_command`: which argument flags and subcommands to
    /// refuse, and which environment variables to force at spawn (to
    /// neutralize config-driven hooks). Seeded with a default `git` policy
    /// (refuse config-injection / root-escape flags + the `config`
    /// subcommand, neutralize pager/ssh via env). Fully visible and editable
    /// in Settings → Offload task tools → Tools. A program with no matching
    /// policy gets only the allowlist + bare-name/PATH guard.
    pub command_policies: Vec<CommandPolicy>,
    /// User-installed MCP tool servers aggregated by cImp's MCP host
    /// and exposed to the local model as OpenAI tools. Mirrors Claude's
    /// own `mcpServers` config shape so users can paste familiar config.
    pub mcp_servers: Vec<McpServerConfig>,
    /// V37 (contract C2): user-created groupings over [`Self::mcp_servers`],
    /// referenced by server name. Global-only — no project overlay ever writes
    /// this, which is what makes a `Vec` safe here (see [`McpCategory`]). Empty
    /// on every pre-v32 file and on a fresh install: no categories means every
    /// server rides its own toggle, i.e. the migration changes no surface.
    pub mcp_categories: Vec<McpCategory>,
    /// V37 (contract C2): per-project activation overrides for servers and
    /// categories. The ONLY MCP-registry field a project overlay may carry, and
    /// map-shaped so `deep_merge` composes it per key — see [`McpActivation`].
    /// Empty = inherit every global toggle.
    pub mcp_activation: McpActivation,
    /// V37 (contract C6): how often the MCP health checker probes each LIVE
    /// server, in seconds. `0` turns the checker off entirely.
    ///
    /// A setting rather than a constant because the right cadence is a property
    /// of the servers, not of cImp: a local `npx` server is free to probe, a
    /// metered remote HTTP endpoint is not. Clamped at use
    /// (`OffloadService::spawn_mcp_health_watch`) rather than at parse, so a
    /// hand-edited `settings.json` cannot turn the checker into a hot loop, and
    /// the per-probe timeout is derived from this value so it stays well under
    /// the cadence by construction.
    ///
    /// Additive under the struct-level `#[serde(default)]`, so every pre-V37
    /// file loads with the 60s default.
    pub mcp_health_interval_secs: u32,
    /// V8-02: the offload backend pool. One entry per backend (Local
    /// `llama-server`, Remote-LAN, or Remote-cloud), each with its own
    /// capabilities, tier, and tool scope. The router picks one per
    /// `offload_task`. Empty on a fresh install / pre-V8-02 file — the
    /// v1.16→v1.17 migration (and [`Self::effective_backends`] at
    /// runtime) synthesizes one Local entry from the legacy
    /// `server_command`/`autostart` fields so single-local setups keep
    /// working unchanged.
    pub backends: Vec<OffloadBackend>,
    /// Saved, reusable `llama-server` launch commands the user can paste into a
    /// Local backend's `Server command` field via the Pool editor's
    /// Save/Load/Delete controls. Purely a convenience library — nothing reads
    /// these at runtime; they only populate the command field on demand. Empty
    /// on a fresh install. Additive `#[serde(default)]`, so old files load with
    /// an empty library.
    pub server_command_templates: Vec<ServerCommandTemplate>,
    /// Saved, reusable Remote-backend endpoints (base URL + auth token) the user
    /// can paste into a Remote backend's fields via the same Save/Load/Delete
    /// controls. Convenience library only; empty on a fresh install. Additive
    /// `#[serde(default)]`.
    pub remote_backend_templates: Vec<RemoteBackendTemplate>,
    /// Fraction (0–100) of the per-slot window the loop works against,
    /// reserving the rest for reasoning + the final answer (~80%).
    pub budget_high_water_pct: u8,
    /// Hard cap on every tool result (native *and* MCP) in tokens; the
    /// loop appends a truncation marker past this so the model knows it
    /// was cut and paginates instead of assuming full coverage (~8k).
    pub per_tool_result_token_cap: u32,
    /// Maximum agent-loop iterations before a forced final-synthesis
    /// turn.
    pub max_steps: u32,
    /// Per-task wall-clock bound (seconds), *including* queue-wait for a
    /// concurrency slot. On expiry the loop is cancelled and a clear
    /// timeout result is returned to Claude rather than hanging the tool
    /// call.
    pub offload_timeout_secs: u64,
    /// V8-03: global cap on offloads in flight across the whole app (all
    /// Claude tabs, all backends). `None` (default) lets the
    /// [`OffloadService`] size it from config — the summed per-backend slot
    /// counts, clamped to a sane ceiling. A queue forms past this so a busy
    /// pool stays coherent and the router's spill/fail-over runs on honest
    /// `in_flight`.
    ///
    /// [`OffloadService`]: crate::offload::OffloadService
    pub global_concurrency: Option<u32>,
    /// Max tasks allowed to *wait* for a slot when the pool is saturated.
    /// `None` (default) = unbounded blocking queue (a new task waits up to
    /// `offload_timeout_secs` for a slot). `Some(n)` fast-rejects a task with
    /// a clear "queue full" error once `n` are already waiting and every slot
    /// is busy — backpressure that fails fast instead of stacking long waits.
    pub max_queue_depth: Option<u32>,
    /// V21 F5: when `true` (default), a fast-tier offload whose answer comes back
    /// only partially verified is re-run once on a distinct, ready quality
    /// backend (the better answer wins). Additive `#[serde(default)]`; inert
    /// unless a second, quality-tier backend exists, so zero-config setups are
    /// unaffected.
    pub escalate_partial: bool,
    // The `opencode_provider` / `opencode_provider_auto` pair moved to the
    // OpenCode plugin's declared settings in V40 Phase B (locked decision 6):
    // `Settings::harness["opencode"].ext["provider"]` (the derived block, an
    // opaque `SettingKind::Json` cImp writes and the user never types) and
    // `ext["provider_auto"]`. `harness::opencode::config` resolves them; core
    // stores them and names neither.
    /// V30 Phase A: register the `cimp-offload` MCP child as a Claude Code
    /// **channel** so it can push out-of-band notices straight into a live
    /// session (`notifications/claude/channel` → a `<channel source="…">`
    /// message at the next turn boundary — see
    /// `docs/MILESTONE-V30-mcp-channels.md`).
    ///
    /// Two spawn-time effects, both Claude-only (OpenCode has no MCP inbound
    /// path):
    ///   * the tab is launched with
    ///     `--dangerously-load-development-channels server:cimp-offload`
    ///     ([`crate::tabs`]'s `CHANNEL_REGISTRATION_FLAG`);
    ///   * the child declares `capabilities.experimental["claude/channel"]` +
    ///     an `instructions` block at `initialize`.
    ///
    /// Default **off**, and marked experimental in the UI: the registration
    /// flag is a Claude Code *research preview* (it may change or vanish), it
    /// paints a persistent banner in every tab, and channel delivery is
    /// fire-and-forget (a misconfigured/policy-blocked push is silently
    /// dropped — hence invariant 2 in the milestone: every push keeps a pull
    /// twin). Spawn-baked, so it carries a `spawn_inject_sig` entry and a
    /// restart hint. Additive `#[serde(default)]` — pre-v29 files load `false`.
    pub session_push: bool,
    /// V32 (locked decision 11): cap on how many EXTERNAL (proxied MCP-server)
    /// tool calls one contaminated scope may make — a worker task, or a
    /// (agent, tab) session at the loopback proxy. Past the cap every further
    /// external call is refused with a fixed string and one Tool Activity row
    /// is written for the scope.
    ///
    /// Generous by design: this exists to stop runaway fetch loops and bulk
    /// exfiltration staging, not to ration research. `0` disables the count
    /// half entirely.
    ///
    /// #48/M-1: "a worker task" means the whole `offload_task` — **including** its
    /// connection fail-over, its `thinking:on → auto` retry and its tier
    /// escalation, which are attempts at one task and share one budget
    /// (`agent::TaskScope`). Until that fix each attempt built its own, so this
    /// number was really up to four times itself. The documented cap is now the
    /// enforced cap; the cost is that a task which spent most of its allowance on
    /// a fast-tier attempt escalates with what is left.
    ///
    /// Additive `#[serde(default)]` — old settings files round-trip with the
    /// default cap. No schema-version bump (the V8/V16/V23 precedent for a
    /// plain additive field).
    /// `pub(in crate::settings)` (#48): an input to the enable hierarchy, read
    /// only through [`injection`](crate::settings::injection) — see
    /// [`InjectionSettings`] for why that boundary is visibility, not a scan.
    pub(in crate::settings) external_fetch_max_calls: u32,
    /// V32 (locked decision 11): cap on the cumulative bytes of EXTERNAL tool
    /// results one contaminated scope may pull. The companion to
    /// [`external_fetch_max_calls`] — a handful of huge pages is the same
    /// exfil-staging shape as many small ones. Charged after each call
    /// completes (a response's size is unknowable before asking for it), so the
    /// cap bites on the call *after* the one that crossed it.
    ///
    /// `0` disables the byte half entirely. Additive `#[serde(default)]`; no
    /// schema-version bump.
    ///
    /// [`external_fetch_max_calls`]: Self::external_fetch_max_calls
    /// `pub(in crate::settings)` (#48) — see
    /// [`external_fetch_max_calls`](Self::external_fetch_max_calls).
    pub(in crate::settings) external_fetch_max_bytes: u64,
    /// V32 (locked decision 7): run the YARA **signature** screen over every
    /// EXTERNAL tool result. Rules are data files under
    /// `<exe-dir>/detection/rules.d/` (plus the user's own `local/` overlay).
    ///
    /// On by default. It is surface-only (locked decision 5) — a match adds a
    /// warning header and a Tool Activity row and changes nothing else — so
    /// there is no correctness risk in leaving it on, and the layer is a cheap
    /// automaton scan over a capped prefix.
    ///
    /// Additive `#[serde(default)]`; no schema-version bump.
    /// `pub(in crate::settings)` (#48): the layer selection lives *inside* the
    /// `Feature::Detection` surface — `injection::detection_config` checks the
    /// parent first and wins — so reading it raw answers a different question.
    pub(in crate::settings) detection_signature_enabled: bool,
    /// V32 (locked decision 7): run the Llama Prompt Guard 2 **classifier**
    /// screen over every EXTERNAL tool result.
    ///
    /// On by default *and inert without weights*: the model files are not
    /// shipped yet, so with them absent the screen skips and Settings says so.
    /// Defaulting to on means installing the weights is the only step needed to
    /// activate it — a default-off flag would leave the layer silently unused
    /// on every machine that fetched them.
    ///
    /// Additive `#[serde(default)]`; no schema-version bump.
    /// `pub(in crate::settings)` (#48) — see
    /// [`detection_signature_enabled`](Self::detection_signature_enabled).
    pub(in crate::settings) detection_classifier_enabled: bool,
    /// V32: the probability at or above which the classifier's verdict counts
    /// as a flag. Prompt Guard 2's positive class is "malicious"; 0.9 is the
    /// conservative default, chosen because a false positive costs a warning
    /// header on legitimate research and header fatigue is the failure mode
    /// that would make the whole surface worthless.
    ///
    /// Additive `#[serde(default)]`; no schema-version bump.
    /// `pub(in crate::settings)` (#48) — see
    /// [`detection_signature_enabled`](Self::detection_signature_enabled).
    pub(in crate::settings) detection_classifier_threshold: f32,
    /// V32 Phase C3 (locked decision 13): what the auto-updater may do with the
    /// **signature rule bundle** — `"off"` / `"check"` / `"auto"`.
    ///
    /// Default `auto`, as the decision locks. Rules are small text files behind
    /// a validate-before-activate gauntlet with a one-click revert, and a stale
    /// signature set is the failure mode the updater exists to prevent — so the
    /// rules half is the one that maintains itself.
    ///
    /// An unrecognized string is read as `check` (see
    /// `detection::updater::Mode::parse`): a typo must neither silently disable
    /// the updater nor silently grant it activation rights.
    ///
    /// A value of the wrong JSON *type* reads the same way (#48) — see
    /// [`de_update_mode`].
    ///
    /// Additive `#[serde(default)]`; no schema-version bump.
    #[serde(deserialize_with = "de_update_mode")]
    pub detection_update_rules_mode: String,
    // `detection_update_classifier_mode` lived here until 2026-08-08. The
    // updater's `classifier` component was removed (user decision) — the
    // Prompt Guard 2 weights ship with the release via the models-v1 pipeline
    // per locked decision 7, so there is no update channel for them to gate.
    // An installed settings file may still carry the key; `Settings` does not
    // set `deny_unknown_fields`, so it is ignored on read and gone on the next
    // write. No migration, no schema-version bump.
    /// V32 Phase C3: hours between update checks. Default 24; floored at
    /// `detection::updater::MIN_INTERVAL_HOURS` so a mistyped `0` cannot become
    /// a request loop against a release asset.
    ///
    /// Additive `#[serde(default)]`; no schema-version bump.
    pub detection_update_interval_hours: u32,
    /// V32 Phase C3: override for the pinned manifest URL. Empty (the default)
    /// means `detection::updater::manifest::DEFAULT_MANIFEST_URL`.
    ///
    /// Exists for the milestone's live-verification recipe — pointing the
    /// updater at a locally staged bundle is the only way to exercise the
    /// download/validate/swap path before the real release assets are
    /// published. It does NOT weaken the asset-origin invariant: artifact URLs
    /// must still live under whatever manifest URL is in force, so an override
    /// relocates the whole bundle, never just part of it.
    ///
    /// Additive `#[serde(default)]`; no schema-version bump.
    pub detection_update_manifest_url: String,
    /// V32 Phase F (locked decision 14): how cImp treats the harnesses' **own**
    /// web tools (Claude `WebFetch`/`WebSearch`, OpenCode `webfetch`/
    /// `websearch`) — `"off"` / `"sensor"` / `"deny"`.
    ///
    /// The taint latch only sees web access that flows through cImp's proxy;
    /// a native web tool is invisible to it, which means a session can ingest
    /// untrusted content with the latch still reading `open`. The three modes:
    ///
    /// - `off` — pre-V32 behaviour: nothing injected, nothing seen. The
    ///   documented escape hatch when a hook misbehaves.
    /// - `sensor` (**default**) — report-only beacons. A Claude `PreToolUse`
    ///   hook matched on the web tools ONLY, and a `tool.execute.before`
    ///   handler in the OpenCode plugin, POST to the loopback and engage that
    ///   tab's EXTERNAL latch. Neither ever denies; a failure is silent.
    /// - `deny` — close the native route by config (Claude `permissions.deny`,
    ///   OpenCode `permission.webfetch/websearch = "deny"`) so all web flows
    ///   through the proxied tools, where the latch is fully effective.
    ///
    /// Default `sensor` because we cannot assume what MCP setup a user runs and
    /// a silently-open side channel is worse than a beacon. An unrecognized
    /// string also reads as `sensor` (see
    /// `crate::tabs::config::NativeWebVisibility::parse`): a typo must neither
    /// blind the latch nor silently take a tool away. A value of the wrong JSON
    /// *type* — `true`, `null`, `0` — reads the same way (#48) instead of
    /// failing the typed parse of the whole file; see [`de_native_web_visibility`].
    ///
    /// **Spawn-baked**: all three modes act only when a tab launches, so this
    /// field carries a `spawn_inject_sig` entry and flipping it raises the
    /// "restart the AI tab" hint.
    ///
    /// `pub(in crate::settings)` (#48): by the Phase G reconciliation this
    /// tri-mode **is** `Feature::NativeWeb`'s L2, so it belongs behind the same
    /// compiler-enforced boundary as the eleven booleans in
    /// [`InjectionSettings`] — leaving one L2 input of eleven readable from an
    /// enforcement site is what made "the no-raw-reads invariant is structural"
    /// overstate its coverage. Read it through
    /// [`injection::native_web_mode`](crate::settings::injection::native_web_mode);
    /// test code outside the boundary writes it through
    /// `Settings::set_native_web_mode_for_test`.
    ///
    /// Additive `#[serde(default)]`; no schema-version bump.
    #[serde(deserialize_with = "de_native_web_visibility")]
    pub(in crate::settings) native_web_visibility: String,
    /// V32 Phase G (locked decision 16): the three-level enable hierarchy's
    /// **L1 + L2** switches, plus the L3 row for the `offload-worker`
    /// pseudo-scope (which has no tab config to hang one off).
    ///
    /// Every field here is read through [`crate::settings::injection`] and
    /// nowhere else. That cross-module invariant is **structural** (#44): the
    /// fields of [`InjectionSettings`] are `pub(in crate::settings)`, so a read
    /// from an enforcement site is a privacy error rather than something a
    /// source scan has to notice. The FIELD stays `pub` — reaching the block is
    /// legal, naming a switch inside it is not. See
    /// [`injection`](crate::settings::injection) for the resolution rule and for
    /// why native-web visibility has no flag in this block.
    ///
    /// Additive `#[serde(default)]`; no schema-version bump.
    pub injection: InjectionSettings,
}

/// V32 Phase G (locked decision 16) — the app-wide half of the injection
/// enable hierarchy.
///
/// **L1** is [`protection`](Self::protection): off disables every V32 control
/// everywhere, all tabs AND the offload worker, and nothing overrides it upward.
/// **L2** is one `<feature>_enabled` flag per control, app-wide. **L3** lives
/// per scope — [`worker`](Self::worker) here for the `offload-worker`
/// pseudo-scope, and `AiToolTabConfig::injection_overrides` for tabs.
///
/// There is deliberately **no `native_web_enabled`**: locked decision 14's
/// tri-mode `native_web_visibility` already carries an `off` value, and that
/// `off` IS the feature's L2. A second boolean beside it would make a
/// contradictory state representable. See
/// [`injection`](crate::settings::injection) for the full reconciliation.
///
/// **Every default is `true`** since the V39 posture decision — master on, and
/// every sub-protection on. V32 Phase H's harness-scoped native-tool gate was
/// the one exception until then (its L2 is a plugin `ext` row since V40 Phase
/// B); its opt-in nature now lives one level down, on a new tab's all-`Off` L3
/// row
/// ([`injection::TabInjectionOverrides::all_off`](crate::settings::injection::TabInjectionOverrides)).
/// An untouched settings file therefore resolves every app-wide level on, which
/// is the ceiling the per-tab rows sit under.
///
/// # Why every field is `pub(in crate::settings)` (#44)
///
/// The no-raw-reads invariant — "no enforcement site reads a raw switch" — has
/// the shape *only module X may name field Y*, and that is what Rust visibility
/// expresses. Until #44 it was watched by a source-scanning tripwire with a
/// per-field allowlist; a scan is a strictly weaker restatement (aliasing the
/// binding, a `//` anywhere on the line, or an accessor added inside an allowed
/// file all defeated it), so the watcher was replaced by the boundary itself.
///
/// The consequence for a future control: the only way to read one of these from
/// an enforcement site is to widen a field here — one line, in the file the
/// reviewer is already looking at. Serde is unaffected (the generated impls live
/// in this module), and the Settings window never touches Rust fields — it
/// round-trips whole [`Settings`] objects through `apply_settings`.
///
/// Test code outside `crate::settings` goes through the enum-keyed
/// `Settings::set_master_for_test` / `set_l2_for_test` helpers in the resolver,
/// which cannot name a flag that no longer exists.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct InjectionSettings {
    /// **L1 — the global master** (spec decision 16's `injection_protection`).
    /// Default `true`. Off = pre-V32 behaviour at every layer at once.
    pub(in crate::settings) protection: bool,
    /// L2: the bidirectional taint latch (worker + proxy).
    pub(in crate::settings) taint_latch_enabled: bool,
    /// L2: the spotlighting envelope on EXTERNAL results and recalled memory.
    pub(in crate::settings) spotlighting_enabled: bool,
    /// L2: the detection surface. Parent of the existing
    /// `detection_signature_enabled` / `detection_classifier_enabled`
    /// sub-toggles — parent off ⇒ both layers off regardless of them.
    pub(in crate::settings) detection_enabled: bool,
    /// L2: the outbound URL range screen at `McpHost::call_recorded`.
    pub(in crate::settings) ssrf_guard_enabled: bool,
    /// L2: per-scope EXTERNAL call/byte caps. The `external_fetch_max_*`
    /// numerics stay the tuning knobs; this is the on/off above them.
    pub(in crate::settings) fetch_budgets_enabled: bool,
    /// L2: the worker's in-band canary (worker-scoped feature).
    pub(in crate::settings) canary_enabled: bool,
    /// L2: quarantine of `context_note` writes from a contaminated session.
    pub(in crate::settings) memory_quarantine_enabled: bool,
    /// L2: the pinned OpenCode permission block + the injection-hygiene
    /// guidance addendum. Spawn-baked.
    pub(in crate::settings) consumer_hygiene_enabled: bool,
    /// L2: the managed-tool steering paragraph — a fixed, generic nudge to
    /// prefer cImp's `run_check` / `run_command` MCP tools over the harness's
    /// own shell. Written into the same guidance channel as the hygiene
    /// paragraph, so it is spawn-baked too.
    pub(in crate::settings) tool_steering_enabled: bool,
    // V32 Phase H's `opencode_native_gate_enabled` is the OpenCode plugin's
    // `ext["native_gate"]` since V40 Phase B (locked decision 6). It is the L2
    // of a feature whose MECHANISM lives inside one harness's generated plugin,
    // so core held an app-wide flag for a control that could only ever reach
    // one harness — see `injection::Feature::scoped_harnesses`.
    /// L2: stripping terminal control sequences out of external text cImp
    /// composes into non-HTML sinks. App-wide — no per-scope row, because TTS
    /// and toasts are global surfaces (the global-only avatar/TTS decision).
    pub(in crate::settings) terminal_escape_hygiene_enabled: bool,
    /// **L3 for the `offload-worker` pseudo-scope.** The worker is a
    /// task-scoped service with no tab, so its override row lives here beside
    /// the app-wide flags rather than on a tab config.
    ///
    /// `pub(in crate::settings)` for the same reason as the switches above: an
    /// L3 cell read on its own ignores L1 and L2, which is the exact failure the
    /// hierarchy exists to prevent (#44 — the override cells were guarded by
    /// nothing at all before).
    pub(in crate::settings) worker: crate::settings::injection::WorkerInjectionOverrides,
}

impl Default for InjectionSettings {
    fn default() -> Self {
        Self {
            // Every control on, every override neutral: an untouched config
            // behaves exactly as the app did before the hierarchy existed.
            protection: true,
            taint_latch_enabled: true,
            spotlighting_enabled: true,
            detection_enabled: true,
            ssrf_guard_enabled: true,
            fetch_budgets_enabled: true,
            canary_enabled: true,
            memory_quarantine_enabled: true,
            consumer_hygiene_enabled: true,
            tool_steering_enabled: true,
            terminal_escape_hygiene_enabled: true,
            worker: Default::default(),
        }
    }
}

impl std::fmt::Debug for OffloadSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OffloadSettings")
            .field("enabled", &self.enabled)
            .field("autostart", &self.autostart)
            .field("inject_guidance", &self.inject_guidance)
            .field("server_command", &self.server_command)
            .field("tools", &self.tools)
            .field("allowed_roots", &self.allowed_roots)
            .field("command_allowlist", &self.command_allowlist)
            .field("command_policies", &self.command_policies)
            // McpServerConfig has its own redacted Debug.
            .field("mcp_servers", &self.mcp_servers)
            // V37: no secrets — names and booleans.
            .field("mcp_categories", &self.mcp_categories)
            .field("mcp_activation", &self.mcp_activation)
            .field("mcp_health_interval_secs", &self.mcp_health_interval_secs)
            // OffloadBackend redacts the Remote `auth_token`.
            .field("backends", &self.backends)
            // No secrets: plain saved command lines.
            .field("server_command_templates", &self.server_command_templates)
            // RemoteBackendTemplate redacts its `auth_token`.
            .field("remote_backend_templates", &self.remote_backend_templates)
            .field("budget_high_water_pct", &self.budget_high_water_pct)
            .field("per_tool_result_token_cap", &self.per_tool_result_token_cap)
            .field("max_steps", &self.max_steps)
            .field("offload_timeout_secs", &self.offload_timeout_secs)
            .field("global_concurrency", &self.global_concurrency)
            .field("max_queue_depth", &self.max_queue_depth)
            .field("escalate_partial", &self.escalate_partial)
            // No secrets beyond the (already-cleartext) `--api-key` the user
            // themselves put in the server command; `LocalProviderBlock`
            // derives Debug.
            .field("session_push", &self.session_push)
            .field("external_fetch_max_calls", &self.external_fetch_max_calls)
            .field("external_fetch_max_bytes", &self.external_fetch_max_bytes)
            .field(
                "detection_signature_enabled",
                &self.detection_signature_enabled,
            )
            .field(
                "detection_classifier_enabled",
                &self.detection_classifier_enabled,
            )
            .field(
                "detection_classifier_threshold",
                &self.detection_classifier_threshold,
            )
            .field(
                "detection_update_rules_mode",
                &self.detection_update_rules_mode,
            )
            .field(
                "detection_update_interval_hours",
                &self.detection_update_interval_hours,
            )
            .field(
                "detection_update_manifest_url",
                &self.detection_update_manifest_url,
            )
            .field("native_web_visibility", &self.native_web_visibility)
            .field("injection", &self.injection)
            .finish()
    }
}

/// A derived OpenCode custom-provider entry — always registered under the id
/// `local-llama` — pointing at the local `llama-server`'s OpenAI-compatible
/// endpoint. Built from a Local backend's `server_command` (see
/// [`crate::offload::server::derive_opencode_provider`]) and injected into the
/// session config OpenCode receives via `OPENCODE_CONFIG_CONTENT`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct LocalProviderBlock {
    /// OpenAI-compatible base URL, ending in `/v1`
    /// (e.g. `http://127.0.0.1:8080/v1`). Host + port come from `--host`
    /// (default `127.0.0.1`) and `--port` in the command.
    pub base_url: String,
    /// Model id OpenCode sends in the completion request and selects as the
    /// default (`local-llama/<model>`). `--alias`/`-a` if present, else the
    /// `--model`/`-m` file basename (directory + `.gguf` stripped).
    pub model: String,
    /// API key from `--api-key` in the command, if any. `llama-server` ignores
    /// auth unless it was launched with a key, so this is usually empty.
    pub api_key: String,
    /// The `server_command` this snapshot was derived from. Lets the auto-sync
    /// re-derive only when the command actually changed.
    pub source_command: String,
}

impl Default for OffloadSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            autostart: false,
            inject_guidance: true,
            server_command: String::new(),
            tools: OffloadToolToggles::default(),
            allowed_roots: Vec::new(),
            command_allowlist: Vec::new(),
            command_policies: default_command_policies(),
            mcp_servers: Vec::new(),
            // V37 C2: no categories, no overrides — every server rides its own
            // `enabled` (true), so a fresh install and a migrated pre-v32 file
            // advertise exactly the same surface.
            mcp_categories: Vec::new(),
            mcp_activation: McpActivation::default(),
            // V37 C6: one probe a minute. Cheap for a stdio server (a
            // non-blocking `try_wait`, no I/O at all) and one small POST for an
            // HTTP one, while still noticing a dead endpoint inside two
            // minutes — the flap guard needs two consecutive failures.
            mcp_health_interval_secs: 60,
            backends: Vec::new(),
            server_command_templates: Vec::new(),
            remote_backend_templates: Vec::new(),
            budget_high_water_pct: 80,
            per_tool_result_token_cap: 8000,
            max_steps: 16,
            offload_timeout_secs: 300,
            global_concurrency: None,
            max_queue_depth: None,
            escalate_partial: true,
            session_push: false,
            // 40 fetches / 4 MiB per scope. Sized against real research
            // behaviour: a thorough multi-source task runs well under a dozen
            // fetches, and 4 MiB is many pages' worth of extracted text — while
            // both are small enough that a loop or a staged bulk read hits the
            // wall early instead of running out the task deadline.
            external_fetch_max_calls: 40,
            external_fetch_max_bytes: 4 * 1024 * 1024,
            // Both detection layers on. Surface-only, so "on" costs a header
            // on a false positive, never a broken call — and the classifier is
            // additionally inert until its weights are installed.
            detection_signature_enabled: true,
            detection_classifier_enabled: true,
            detection_classifier_threshold: 0.9,
            // The locked C3 default: the rule bundle maintains itself. It is
            // the only updatable component — the classifier weights ship with
            // the release (locked decision 7, models-v1 pipeline).
            detection_update_rules_mode: "auto".into(),
            detection_update_interval_hours: 24,
            // Empty = the pinned manifest URL.
            detection_update_manifest_url: String::new(),
            // Locked decision 14: report-only visibility by default. Not
            // `deny` — taking a tool away from a working tab is a behaviour
            // change; not `off` — an unseen web route is exactly the hole
            // this exists to close.
            native_web_visibility: "sensor".into(),
            // V32 Phase G: every control on, every override neutral — see
            // `InjectionSettings::default`.
            injection: InjectionSettings::default(),
        }
    }
}

/// V9-01: per-project code knowledge graph configuration. The structural
/// graph (symbols/refs/calls/imports/full-text docs) needs no embedding
/// model; the `semantic_*` fields drive the optional Phase-G semantic search
/// over a remote `/v1/embeddings` endpoint. Additive `#[serde(default)]` — old
/// settings files round-trip with the feature disabled.
///
/// **V33 Phase E: this block now holds a secret** — `embedding_auth_token` —
/// so `Debug` is hand-rolled and redacts it, exactly like [`OffloadSettings`]
/// and [`ClaudeLocalSettings`]. (It read "No secrets here, so `Debug` is
/// derived" until the token landed; a derived `Debug` would print the bearer
/// token into the rolling log the first time anyone logs a settings snapshot.)
/// `graph_settings_debug_covers_every_field_and_redacts_the_token` keeps the
/// hand-rolled impl from silently omitting a future field.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphSettings {
    /// Master switch. Off = no indexing, no `graph_*` tools, no monitor tab.
    pub enabled: bool,
    /// Languages to index. Each maps to a tree-sitter grammar (+ `tags.scm`,
    /// + optional stack-graphs `tsg`). Unsupported files are skipped.
    pub languages: Vec<String>,
    /// Extra ignore globs, additive to the project's `.gitignore`. Generated
    /// / vendored / minified dirs are excluded by default to keep the graph
    /// clean and the DB small.
    pub ignore: Vec<String>,
    /// Index markdown files + doc-comments as `doc_chunk` nodes linked to the
    /// code they describe (powers `graph_search_docs`).
    pub index_docs: bool,
    /// Skip files larger than this many bytes (minified bundles, blobs).
    pub max_file_bytes: u64,
    /// Debounce window for the fs watcher's re-index pass (milliseconds).
    pub watch_debounce_ms: u64,
    /// Hard cap on rows returned by any single `graph_*` query (results feed
    /// an LLM context, so they're bounded like V8's tool results).
    pub max_rows_per_query: u32,
    /// Hard cap on the snippet bytes attached to each result row.
    pub max_snippet_bytes: u32,
    /// Hard cap on the body bytes returned by `graph_snippet` (V11 Phase A).
    /// Larger than `max_snippet_bytes` because whole definition bodies are
    /// bigger than the one-line snippets attached to result rows.
    pub max_body_bytes: u32,
    /// Per-project subdirectory holding `graph.db`. Recommended git-ignored.
    pub db_subdir: String,
    /// Let the **offload worker** query the graph when it's running on a
    /// **remote** backend. The local worker always gets graph access; a remote
    /// backend (LAN *or* cloud) sends your project's code structure off this
    /// machine, so it's opt-in and off by default. The user decides per their
    /// trust in the remote (a private LAN box vs. a public cloud API). The
    /// cloud Opus session (via MCP) and a local worker are unaffected.
    pub allow_remote_worker_access: bool,

    // --- Semantic search (Phase G) ---
    /// Enable embedding-based semantic search. Default off — it needs a
    /// reachable embedding endpoint; the structural graph works without it.
    pub semantic_search: bool,
    /// OpenAI-compatible `/v1/embeddings` endpoint (e.g. a `llama-server
    /// --embedding` on a spare GPU box).
    pub embedding_endpoint: String,
    /// V33 Phase E: bearer token sent on every request to
    /// [`embedding_endpoint`](Self::embedding_endpoint) — the embeddings POST
    /// and the `/props`, `/tokenize`, `/detokenize` helpers alike. Empty (the
    /// default, and every pre-V33 settings file) = no `Authorization` header,
    /// i.e. exactly the pre-V33 behaviour.
    ///
    /// Why this one matters most: the embedding endpoint is the only LAN
    /// service whose corruption is **silent**. A poisoned `/health` fails
    /// loudly; poisoned vectors just make semantic search quietly wrong, for
    /// as long as the epoch lives. Redacted in `Debug` (see the type doc).
    pub embedding_auth_token: String,
    /// Embedding model id requested from the endpoint. Baked into the vector
    /// "epoch"; changing it forces a re-embed.
    pub embedding_model: String,
    /// Embedding vector dimension. `0` = auto-probe on the first embed. The
    /// HNSW index never mixes dimensions.
    pub embedding_dims: u32,
    /// Also embed full symbol bodies (not just docs + signatures) for
    /// semantic *code* search. Off by default — multiplies vector count.
    /// Requires `semantic_search` on (it shares the embedder + backfill pass);
    /// with `semantic_search` on, this enables the `graph_semantic_code` tool and
    /// its code-embedding pass.
    pub embed_code_bodies: bool,
    /// Number of chunks per `/v1/embeddings` request (amortizes round-trips).
    pub embedding_batch: usize,
    /// Hard per-input token budget for the embedding endpoint. `0` = auto-detect
    /// from the server's `/props` (`default_generation_settings.n_ctx`, minus a
    /// small margin), cached per endpoint for the process. Any text over the
    /// budget is truncated (via the server's own tokenizer when available)
    /// before it's sent, because a single oversized chunk makes the endpoint
    /// reject the WHOLE batch. Set it manually for a non-llama server that
    /// exposes no `/props`; with no override and no detection, texts are sent
    /// unchanged.
    pub embedding_max_tokens: u32,
    /// Project-wide cap on how many `code_chunk` rows a full rebuild keeps
    /// (a simple count cap for V1 — see `build_tree`). Bounds DB size and
    /// embedding cost on very large repos.
    pub semantic_code_max_chunks: u32,

    // --- Context injection (V10 Phase D) ---
    /// Automatically prepend a budget-bounded digest of the most relevant files
    /// to each user prompt (Claude via a UserPromptSubmit hook, OpenCode via a
    /// plugin). Off by default — it changes what the agent sees.
    pub context_injection: bool,
    /// Max characters of digest emitted per file (outline + best snippet).
    pub context_per_file_chars: u32,
    /// Total character budget for one turn's injected context across all files.
    pub context_turn_budget_chars: u32,
    /// Fold the current session's working set (Phase C memory) into the ranking
    /// so session-hot files rank first.
    pub context_include_session: bool,
    /// Minimum top-file relevance score below which nothing is injected (so
    /// meta/"hi" prompts inject nothing).
    pub context_min_score: u32,

    // --- V11 Phase B: repo map (session-start orientation) ---
    /// Character budget for the once-per-session project map (`graph_repo_map`
    /// tool, and the session-start injection when enabled).
    pub repo_map_budget_chars: u32,
    /// Prepend the project map to the first injected turn of each new session.
    /// Rides the `context_injection` master toggle AND this flag. Off by default.
    pub repo_map_on_session_start: bool,

    // --- V11 Phase C: injection dedup ---
    /// How many turns a dedup suppression lasts: a file injected in full is
    /// demoted to a one-line "unchanged" reminder on later turns until it changes
    /// or this many turns pass. `0` disables dedup (every turn re-injects).
    pub context_dedup_ttl_turns: u32,

    // --- V11 Phase D: compaction survival (Claude PreCompact) ---
    /// Feed the compactor the session's working set + pinned notes so they
    /// survive the summary (and clear dedup / mark post-compaction). Costs a few
    /// hundred chars once per compaction; still master-gated by `context_injection`.
    pub compaction_context: bool,

    // --- V11 Phase E: redundant-read advisor (opt-in; logic in Phase E) ---
    /// Intercept a `Read` of a file already read unchanged this session and
    /// answer with a cheap reminder (outline digest) instead of re-reading it.
    /// Strictly opt-in — it changes the agent's tool behaviour. Default off.
    pub read_advisor: bool,
    /// Files with fewer than this many lines always pass the advisor (a small
    /// file is cheap to re-read; the reminder isn't worth it).
    pub read_advisor_min_lines: u32,
    /// `"advise"` (remind with the outline) or `"substitute"` (also include the
    /// most relevant symbol body). Default `"advise"`. Compared post-hoc by its
    /// consumers, so an unrecognized string — and, since #48, a value of the
    /// wrong JSON type — behaves as `advise` rather than quarantining the whole
    /// settings file; see [`de_read_advisor_mode`].
    #[serde(deserialize_with = "de_read_advisor_mode")]
    pub read_advisor_mode: String,
    /// V16 Feature 5: trust TTL — after this many retrieval turns since the
    /// advisor last observed a full read of a file, a `Read` passes again
    /// (bounds how long the advisor trusts the agent's memory across context
    /// loss it can't observe: context editing, tool-result truncation).
    /// 0 = off (the pre-V16 behavior: trust for the whole session).
    pub read_advisor_ttl_turns: u32,
    /// V17 Phase A: when a file the agent already read is re-read *after it
    /// changed*, answer with a line-level unified diff against the last-read
    /// snapshot instead of passing the whole file. Exact (a diff versus the
    /// snapshot can't mislead), so it's safe on the post-edit verify loop that
    /// dominates real sessions. Default **on** — a strictly-better substitute,
    /// still master-gated by `read_advisor` and the E1 hard block. Falls back to
    /// a plain pass whenever no snapshot survives (small file / over-cap /
    /// LRU-evicted) or the rendered diff exceeds half the new content.
    pub read_advisor_diffs: bool,
    /// V17 Phase B: also intercept a whole-file shell read (`cat FILE`,
    /// `Get-Content FILE`, `type FILE`, `gc FILE`) of an already-read file via a
    /// second `PreToolUse` **Bash** matcher — the shell equivalent of the `Read`
    /// advisor. Strict: only a provable pure whole-file read of one file is
    /// intercepted (anything with a pipe/redirect/glob/second-path/partial-read
    /// verb runs untouched). Default **on**; master-gated by `read_advisor` and
    /// the E1 hard block. Off ⇒ a zero overlay delta (the Bash matcher isn't
    /// installed) and the bypass canary scores shell reads as before.
    pub read_advisor_shell: bool,
    /// V17 Phase C: first-read tier — the size (in KiB) at or above which a
    /// *first* whole-file `Read` of a **non-code** file (log, lockfile, generated
    /// JSON, data dump — no parsed symbols) is answered with the cached
    /// local-model digest + a head/tail sample instead of the full content. A
    /// separate opt-in *within* the advisor: `0` = off (the default). Only fires
    /// when a digest is already cached for the current content hash — a miss
    /// enqueues one and passes, so protection begins on the next (cross-session)
    /// encounter. A deliberate slice (`offset`/`limit`) always passes. Proposed
    /// starting value when enabled: 256.
    pub read_advisor_first_read_kb: u32,

    // --- V17 Phase E: lean tool surface ---
    /// Hide the cold-tail `graph_*` tools (`graph_cycles`, `graph_dead_exports`,
    /// `graph_struct_search`, `graph_path`, `graph_architecture`) from the tool
    /// surface advertised to the cloud session and the offload worker, trimming
    /// the tools block that's cache-written once per session. Advertisement-only:
    /// the hidden tools still ANSWER if an agent calls them by name — they're
    /// just not offered. Default off.
    pub lean_tools: bool,

    // --- V11 Phase F: local-model context digests ---
    /// For files with no useful outline (docs/configs/long scripts), have the
    /// **local** offload backend write a 3-line semantic digest, cached in
    /// `graph.db`. Off by default; needs a ready local offload backend. Never
    /// leaves the machine (local-only path).
    pub context_llm_digests: bool,

    // --- V12 Phase E: memory distillation (durable project facts) ---
    /// Distill an idle session's working set + notes into at most 3 durable
    /// `project_fact` rows via the **local-only** offload path before/instead
    /// of letting that knowledge evaporate with the session. Off by default —
    /// needs a ready local offload backend and the prompt is model-dependent
    /// (milestone Decision 3: revisit after real-session validation).
    pub memory_distillation: bool,
    /// Append **pinned** project facts (only pinned — the human-curated tier)
    /// to the launch-time guidance payload (Claude `--append-system-prompt`,
    /// OpenCode's instructions file), so durable knowledge arrives with zero
    /// tool calls. Off by default. Launch-time only: a fact pinned mid-session
    /// applies on the tab's next launch.
    pub promote_pinned_facts: bool,

    // --- V12 Phase F: proactive automation ---
    /// Auto-run the project's configured checks after an edit (`PostToolUse`
    /// hook → `/context/post_edit`) and inject only NEW/worsened diagnostics
    /// as additional context — the agent learns it broke something in the
    /// same turn instead of three turns later. Strictly opt-in — it's a
    /// behavior hook, same posture as `read_advisor`. Off by default; needs
    /// `checks` non-empty to do anything.
    pub auto_check: bool,
    /// Debounce window (seconds): edits inside this window since the last
    /// triggered run are coalesced (no new run); the run then covers
    /// everything the burst touched, since checks run against the file system
    /// state, not a specific edit.
    pub auto_check_debounce_s: u32,
    /// Minimum DIRECT inbound call count (`graph_callers`'s count) an edited
    /// file's symbol must have before the same hook appends a two-line
    /// blast-radius note (6b) — the moments an agent most needs impact
    /// analysis are exactly the moments it doesn't think to ask for it.
    pub auto_impact_min_dependents: u32,
    /// Re-run `dead_exports`/`import_cycles` after every completed index pass
    /// (bounded, read-only on the warm index — cheap) and badge the Analyses
    /// section when the counts changed. On by default — unlike the other
    /// Phase F toggles this doesn't change agent behavior, only a UI badge.
    pub analyses_auto: bool,
    /// V15 Feature 1: hop bound for `graph_path` shortest-path tracing — how far
    /// the BFS explores before giving up. Clamped 1–32 at the tool boundary.
    pub path_max_hops: u32,
    /// V15 Feature 2: max subsystems (file communities) `graph_architecture`
    /// reports, biggest first.
    pub arch_max_communities: u32,
    /// V15 Feature 2: ignore communities smaller than this in the architecture
    /// report (singletons/pairs are noise, not subsystems).
    pub arch_min_community_size: u32,
    /// V15 Feature 4 (STRETCH): master toggle for the **Graph view** live
    /// force-graph (the Tool Activity tab's "Graph view" section — formerly
    /// its own reserved tab, retired in schema v26). Off by default — it's
    /// the human-facing visual, not on any agent path.
    pub graph_viz: bool,
    /// V15 Feature 4: cap on the rendered subgraph node count so large repos
    /// stay smooth (the view is bounded orientation, never the whole graph).
    pub graph_viz_max_nodes: u32,
    /// Graph View tuning (all multipliers on the built-in behavior, `1.0` =
    /// unchanged): file-node radius. One size doesn't fit every repo — a
    /// dense monorepo wants smaller nodes/wider spacing than a 50-file tool.
    pub graph_viz_node_scale: f32,
    /// Directory-cluster size multiplier (the leash radius files orbit their
    /// folder anchor at — bigger = looser, larger folder discs).
    pub graph_viz_dir_scale: f32,
    /// Edge line-width multiplier (ambient, emphasized, highlighted and the
    /// aggregate folder↔folder edges all scale together).
    pub graph_viz_edge_width: f32,
    /// Spacing multiplier between FILE nodes (connected-pair rest length and
    /// the matching node↔node repulsion).
    pub graph_viz_node_spacing: f32,
    /// Spacing multiplier between DIRECTORY clusters (anchor↔anchor rest
    /// length and the matching cluster repulsion).
    pub graph_viz_cluster_spacing: f32,
    /// Directory-clustering tightness multiplier (the strength of the spring
    /// leashing each file to its folder anchor — higher = files hug their
    /// folder harder, lower = topology wins over directory grouping).
    pub graph_viz_cluster_strength: f32,
    /// Edge colors (`#rrggbb`): call edges and import edges. The remaining
    /// hues (highlight pulses, subsystem palette) stay built-in.
    pub graph_viz_color_call: String,
    pub graph_viz_color_import: String,
    /// Segment colors for the Code Intelligence tab's "This session"
    /// stacked-bar chart (`#rrggbb`). Edited in-place by clicking the chart's
    /// legend swatches; defaults match the original hard-coded palette.
    pub usage_color_in: String,
    pub usage_color_cache: String,
    pub usage_color_out: String,
    pub usage_color_tool: String,
    /// V16 Feature 8: the cache-write segment's color — new alongside the
    /// four above now that `cache_make` is plotted as its own segment.
    pub usage_color_write: String,
    /// V24 Phase C follow-up: the S/A lane colors — main-session and
    /// sub-agent segments under the chart (the agent color also tints the
    /// sub-agent bars' outline). Edited via the same legend swatches.
    pub usage_color_session: String,
    pub usage_color_agent: String,
}

impl std::fmt::Debug for GraphSettings {
    /// Hand-rolled since V33 Phase E purely to redact
    /// [`embedding_auth_token`](GraphSettings::embedding_auth_token); every
    /// other field prints exactly as the derive would.
    ///
    /// The hazard a hand-rolled `Debug` on a struct this wide introduces is
    /// *silent omission* — a field added later, never listed here, simply
    /// vanishes from every debug line. That is what
    /// `graph_settings_debug_covers_every_field_and_redacts_the_token` pins:
    /// it walks the serialized key set and requires each name to appear below.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphSettings")
            .field("enabled", &self.enabled)
            .field("languages", &self.languages)
            .field("ignore", &self.ignore)
            .field("index_docs", &self.index_docs)
            .field("max_file_bytes", &self.max_file_bytes)
            .field("watch_debounce_ms", &self.watch_debounce_ms)
            .field("max_rows_per_query", &self.max_rows_per_query)
            .field("max_snippet_bytes", &self.max_snippet_bytes)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("db_subdir", &self.db_subdir)
            .field("allow_remote_worker_access", &self.allow_remote_worker_access)
            .field("semantic_search", &self.semantic_search)
            .field("embedding_endpoint", &self.embedding_endpoint)
            // The one reason this impl exists.
            .field(
                "embedding_auth_token",
                &if self.embedding_auth_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("embedding_model", &self.embedding_model)
            .field("embedding_dims", &self.embedding_dims)
            .field("embed_code_bodies", &self.embed_code_bodies)
            .field("embedding_batch", &self.embedding_batch)
            .field("embedding_max_tokens", &self.embedding_max_tokens)
            .field("semantic_code_max_chunks", &self.semantic_code_max_chunks)
            .field("context_injection", &self.context_injection)
            .field("context_per_file_chars", &self.context_per_file_chars)
            .field("context_turn_budget_chars", &self.context_turn_budget_chars)
            .field("context_include_session", &self.context_include_session)
            .field("context_min_score", &self.context_min_score)
            .field("repo_map_budget_chars", &self.repo_map_budget_chars)
            .field("repo_map_on_session_start", &self.repo_map_on_session_start)
            .field("context_dedup_ttl_turns", &self.context_dedup_ttl_turns)
            .field("compaction_context", &self.compaction_context)
            .field("read_advisor", &self.read_advisor)
            .field("read_advisor_min_lines", &self.read_advisor_min_lines)
            .field("read_advisor_mode", &self.read_advisor_mode)
            .field("read_advisor_ttl_turns", &self.read_advisor_ttl_turns)
            .field("read_advisor_diffs", &self.read_advisor_diffs)
            .field("read_advisor_shell", &self.read_advisor_shell)
            .field("read_advisor_first_read_kb", &self.read_advisor_first_read_kb)
            .field("lean_tools", &self.lean_tools)
            .field("context_llm_digests", &self.context_llm_digests)
            .field("memory_distillation", &self.memory_distillation)
            .field("promote_pinned_facts", &self.promote_pinned_facts)
            .field("auto_check", &self.auto_check)
            .field("auto_check_debounce_s", &self.auto_check_debounce_s)
            .field(
                "auto_impact_min_dependents",
                &self.auto_impact_min_dependents,
            )
            .field("analyses_auto", &self.analyses_auto)
            .field("path_max_hops", &self.path_max_hops)
            .field("arch_max_communities", &self.arch_max_communities)
            .field("arch_min_community_size", &self.arch_min_community_size)
            .field("graph_viz", &self.graph_viz)
            .field("graph_viz_max_nodes", &self.graph_viz_max_nodes)
            .field("graph_viz_node_scale", &self.graph_viz_node_scale)
            .field("graph_viz_dir_scale", &self.graph_viz_dir_scale)
            .field("graph_viz_edge_width", &self.graph_viz_edge_width)
            .field("graph_viz_node_spacing", &self.graph_viz_node_spacing)
            .field("graph_viz_cluster_spacing", &self.graph_viz_cluster_spacing)
            .field(
                "graph_viz_cluster_strength",
                &self.graph_viz_cluster_strength,
            )
            .field("graph_viz_color_call", &self.graph_viz_color_call)
            .field("graph_viz_color_import", &self.graph_viz_color_import)
            .field("usage_color_in", &self.usage_color_in)
            .field("usage_color_cache", &self.usage_color_cache)
            .field("usage_color_out", &self.usage_color_out)
            .field("usage_color_tool", &self.usage_color_tool)
            .field("usage_color_write", &self.usage_color_write)
            .field("usage_color_session", &self.usage_color_session)
            .field("usage_color_agent", &self.usage_color_agent)
            .finish()
    }
}

impl GraphSettings {
    /// The per-project db subdirectory, falling back to `.cimp` when unset.
    /// Single source of truth so the service and the MCP child can't open
    /// different paths.
    pub fn effective_db_subdir(&self) -> String {
        let s = self.db_subdir.trim();
        if s.is_empty() {
            ".cimp".to_string()
        } else {
            s.to_string()
        }
    }
}

impl Default for GraphSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            // Tier-1 code languages are on by default; markup/data languages
            // (html/css/json) stay opt-in to keep a fresh index lean (V9-02).
            languages: [
                "rust",
                "typescript",
                "javascript",
                "python",
                "markdown",
                "go",
                "java",
                "c",
                "cpp",
                "csharp",
                "php",
                "bash",
                "scala",
                "ocaml",
                "ruby",
                "haskell",
                "kotlin",
                "swift",
                "sql",
                "erlang",
                "r",
                "perl",
                "ada",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            ignore: Vec::new(),
            index_docs: true,
            max_file_bytes: 1_048_576, // 1 MiB
            watch_debounce_ms: 300,
            max_rows_per_query: 100,
            max_snippet_bytes: 2_000,
            max_body_bytes: 16_384,
            db_subdir: ".cimp".to_string(),
            allow_remote_worker_access: false,
            semantic_search: false,
            embedding_endpoint: String::new(),
            embedding_auth_token: String::new(),
            embedding_model: String::new(),
            embedding_dims: 0,
            embed_code_bodies: false,
            embedding_batch: 32,
            embedding_max_tokens: 0,
            semantic_code_max_chunks: 20_000,
            context_injection: false,
            context_per_file_chars: 800,
            context_turn_budget_chars: 6_000,
            context_include_session: true,
            context_min_score: 3,
            repo_map_budget_chars: 4_000,
            repo_map_on_session_start: false,
            context_dedup_ttl_turns: 10,
            compaction_context: true,
            read_advisor: false,
            read_advisor_min_lines: 300,
            read_advisor_mode: "advise".to_string(),
            read_advisor_ttl_turns: 0,
            read_advisor_diffs: true,
            read_advisor_shell: true,
            read_advisor_first_read_kb: 0,
            lean_tools: false,
            context_llm_digests: false,
            memory_distillation: false,
            promote_pinned_facts: false,
            auto_check: false,
            auto_check_debounce_s: 5,
            auto_impact_min_dependents: 10,
            analyses_auto: true,
            path_max_hops: 8,
            arch_max_communities: 12,
            arch_min_community_size: 3,
            graph_viz: false,
            graph_viz_max_nodes: 1500,
            graph_viz_node_scale: 1.0,
            graph_viz_dir_scale: 1.0,
            graph_viz_edge_width: 1.0,
            graph_viz_node_spacing: 1.0,
            graph_viz_cluster_spacing: 1.0,
            graph_viz_cluster_strength: 1.0,
            graph_viz_color_call: "#4fb3ff".to_string(),
            graph_viz_color_import: "#ff8a3d".to_string(),
            usage_color_in: "#58a6ff".to_string(),
            usage_color_cache: "#d2a8ff".to_string(),
            usage_color_out: "#3fb950".to_string(),
            usage_color_tool: "#f0c674".to_string(),
            usage_color_write: "#e3738d".to_string(),
            usage_color_session: "#30363d".to_string(),
            usage_color_agent: "#3b6ea5".to_string(),
        }
    }
}

/// V13 §0.4: the Workbench feature's settings. `enabled` is the master
/// switch for the tab itself (default **on** — the tab is cheap and each
/// section gates its own behavior); `checkpoints` is the shadow-repo
/// snapshot feature (default **off** in V1 — proposed on-by-default once the
/// shadow-repo cost is validated on a large real repo, per the milestone's
/// open decision 2). The five `checkpoint_*` fields tune retention (`_max`,
/// `_max_age_days`) and the debounced burst trigger (`_burst_files`,
/// `_burst_window_s`, `_min_gap_s`) that Phase C's fallback-to-activity
/// snapshot trigger reads.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub struct WorkbenchSettings {
    /// Master switch: the reserved Workbench tab exists. Off = no tab, no
    /// fs-batch event/broadcast, no checkpoint scheduling.
    pub enabled: bool,
    /// Automatic checkpoint snapshots (Phase C's shadow git repo). Off by
    /// default in V1 — the tab's Diff/Worktrees sections work without it;
    /// Timeline needs this on.
    pub checkpoints: bool,
    /// Ring-buffer cap: the shadow repo keeps at most this many checkpoints
    /// (oldest pruned first by `shadow::gc`, subject to `checkpoint_max_age_days`).
    pub checkpoint_max: u32,
    /// Age cap in days: checkpoints older than this are pruned regardless of
    /// how far under `checkpoint_max` the ring is.
    pub checkpoint_max_age_days: u32,
    /// Burst trigger: at least this many distinct changed paths within
    /// `checkpoint_burst_window_s` (and at least `checkpoint_min_gap_s` since
    /// the last snapshot) fires an "activity" checkpoint — the fallback that
    /// covers shell-tab edits and any flow that doesn't go through the
    /// prompt-tap trigger.
    pub checkpoint_burst_files: u32,
    /// Time window (seconds) the burst-file count above is measured over.
    pub checkpoint_burst_window_s: u32,
    /// Minimum seconds between two automatic snapshots FROM THE SAME SOURCE,
    /// so a rapid-fire prompt sequence or a noisy save loop can't spam the
    /// shadow repo with near-duplicate commits.
    ///
    /// "Source" is the AI tab the prompt came from — the burst trigger, which
    /// belongs to no tab, is its own source. The gap is therefore enforced per
    /// `(project, tab)` rather than per project (V33): with two AI tabs on one
    /// project, each tab's prompt can take its own checkpoint inside the
    /// other's cooldown, which is what lets the Timeline say which checkpoint
    /// was live for a GIVEN tab. The cost, accepted deliberately: snapshot
    /// volume scales with the number of active tabs on a project.
    pub checkpoint_min_gap_s: u32,
}

/// V33 Phase A — OS-level sandboxing of agent-initiated child processes.
///
/// **Locked decision 16: one top-level category holds every sandboxing
/// setting.** Not scattered into Tabs / Local task offload / Per-tab overrides.
/// Sibling to `Injection protection`, deliberately **not merged into it**: V32
/// constrains a compromised model at the tool layer, V33 makes the OS enforce a
/// boundary the model cannot negotiate with, and merging them would let a user
/// believe one delivers the other. Membership test for anything added here:
/// *does this control the boundary the OS enforces?* — not *did V33 add it?*
///
/// **Locked decision 17: [`enabled`](Self::enabled) reaches the OS layer
/// ONLY.** Off ⇒ no per-spawn AppContainer wrapper on Windows, no Landlock
/// ruleset on Linux (V33 Phase D), and — when it lands — no Max Paranoia. The
/// same three fields govern both engines: there is no Linux-only setting, and
/// `extra_grant_dirs` means the same thing on both (a reviewed read+execute
/// widening). Unconditional regardless of this switch, and
/// therefore absent from this struct: job-object kill-on-close (lifecycle
/// correctness — switching it off reintroduces orphans, a bug not a freedom),
/// `run_command`'s minimal environment (it withholds credentials, not
/// capability), and the V32/V33 injection-layer fixes.
///
/// The two negative states — `off (user choice)` and `unavailable (prerequisite
/// missing)` — are **distinct and never collapsed**; see
/// [`sandbox::SkipReason`](crate::sandbox::SkipReason). This struct only
/// carries the first; the second is discovered at spawn time.
///
/// ⚠ **A compromised model must not be able to flip this switch.** That rests
/// on it being a settings write with no tool-exposed path: `run_command` cannot
/// reach `settings.json` (it is outside every `allowed_root` and the sandbox
/// itself denies the write), and no MCP tool writes settings. Verified rather
/// than inherited — the V32 run found a comment standing in for a check six
/// times.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Default)]
#[serde(default)]
pub struct SandboxSettings {
    /// Master switch for the OS sandbox layer (decision 17).
    ///
    /// Default **false**: Phase A ships the engine, and the grant ladder can
    /// still surprise a machine whose toolchains live in Administrators-owned
    /// directories. Opt-in first, default-on proposed once the live-verify
    /// items in `docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md` have soaked
    /// — the same posture `workbench.checkpoints` shipped with.
    pub enabled: bool,
    /// V33 Phase B — also sandbox the **AI-tool tabs** (Claude, claude-local,
    /// OpenCode), not just the tool seams.
    ///
    /// Effective only when [`enabled`](Self::enabled) is also true: this is a
    /// scope widener inside the OS layer, never a second master switch. The two
    /// off states stay distinguishable in the Events lane (the skip row's detail
    /// names which switch was off), because "I turned sandboxing off" and "I
    /// left tabs out of it" are different user intents.
    ///
    /// Default **false**, and a bigger step than [`enabled`](Self::enabled) is:
    /// a tab IS the agent, so confining it confines everything the agent
    /// afterwards runs — including a `git push` whose credential helper now
    /// cannot read the user's store. Opt in deliberately.
    ///
    /// **Plain Shell tabs are never sandboxed by this.** A shell tab is the
    /// user's own hands, not an agent seam; confining it would be cImp deciding
    /// what its user may do on their own machine.
    pub tabs: bool,
    /// Give sandboxed children the `internetClient` capability.
    ///
    /// Default **false** — a read-only probe needs no egress. Spike S1
    /// measured that on a Public-profile NIC this single capability opens the
    /// LAN as well as the internet (capabilities are class-granular), so the
    /// honest choice today is all-or-nothing; per-host scoping per locked
    /// decision 4 is WFP work (spike S4).
    ///
    /// **This knob governs `run_command` / `run_check` / the audit scanners
    /// only — NOT tabs.** A sandboxed AI tab always gets `internetClient`
    /// (locked decision B3): an AI CLI that cannot reach its own model endpoint
    /// is a bricked tab, not a hardened one. See
    /// [`crate::sandbox::tabs::tab_sandbox_cfg`].
    pub allow_network: bool,
    /// Extra directories granted read+execute inside the sandbox — the
    /// user-curated rows of decision 3's grant table.
    ///
    /// The spawn path already grants the resolved program's own install
    /// directory, which covers the common case; this is for a toolchain that
    /// reaches sideways (a compiler in one tree calling a linker in another).
    /// Empty by default.
    pub extra_grant_dirs: Vec<String>,
}

// `Default` is derived: every field's "off / empty" is exactly the milestone
// default (sandbox off until the grant ladder soaks, no network capability, no
// extra grants), so a hand-written impl would only be a place for the two to
// drift apart.

impl Default for WorkbenchSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            checkpoints: false,
            checkpoint_max: 100,
            checkpoint_max_age_days: 7,
            checkpoint_burst_files: 5,
            checkpoint_burst_window_s: 60,
            checkpoint_min_gap_s: 120,
        }
    }
}

/// A named, reusable `llama-server` launch command the user can save from a
/// Local backend's `Server command` field in the Offload → Pool editor and
/// paste back into that field later. Stored globally in
/// [`OffloadSettings::server_command_templates`] so a library of commands
/// survives across backends and app restarts.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ServerCommandTemplate {
    /// User-facing label, unique within the list (the UI enforces uniqueness).
    pub name: String,
    /// The saved `llama-server` command line, verbatim.
    pub command: String,
}

/// A named, reusable Remote-backend endpoint (base URL + auth token) the user
/// can save from a Remote backend's fields and paste back in later. Stored
/// globally in [`OffloadSettings::remote_backend_templates`]. The `auth_token`
/// may be a real bearer secret, so the hand-rolled `Debug` below redacts it.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct RemoteBackendTemplate {
    /// User-facing label, unique within the list (the UI enforces uniqueness).
    pub name: String,
    /// Saved backend base URL (e.g. `http://192.168.1.50:8080`).
    pub base_url: String,
    /// Saved bearer/auth token. Stored cleartext on disk (like the backend's
    /// own `auth_token`); redacted in `Debug`.
    pub auth_token: String,
}

impl std::fmt::Debug for RemoteBackendTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteBackendTemplate")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

/// One environment variable forced at spawn by a [`CommandPolicy`] (e.g.
/// `GIT_PAGER=cat`). An ordered list of these — rather than a map — keeps the
/// settings diff deterministic and maps cleanly to the UI's key/value rows.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CommandEnvVar {
    pub key: String,
    pub value: String,
}

/// A per-program command security policy applied by the native `run_command`
/// tool on top of the allowlist. Generalizes what used to be a hardcoded `git`
/// guard so any allowlisted program can be hardened, and so the rules are
/// visible/editable in Settings rather than buried in code.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CommandPolicy {
    /// Program this policy hardens, matched against the allowlisted command by
    /// file-stem, case-insensitively (so `git` covers `git` and `git.exe`).
    pub program: String,
    /// Argument tokens to refuse. An argument matches when it equals the entry
    /// OR starts with `<entry>=` — covering `-c`, `--config-env`, `--git-dir`,
    /// … in both `--flag value` and `--flag=value` forms.
    pub denied_flags: Vec<String>,
    /// Subcommands (the first non-flag argument) to refuse, e.g. `config`.
    pub denied_subcommands: Vec<String>,
    /// V21 F7: when non-empty, an *allowlist* over the first non-flag argument —
    /// only these subcommands may run; every other (and a bare invocation) is
    /// refused. This is the strict counterpart to `denied_subcommands`: a
    /// denylist can't safely enumerate an open-ended set (a program's future or
    /// aliased subcommands would slip through), so a program that must be pinned
    /// to a few read-only verbs (e.g. `cargo` → `metadata`/`tree`, never
    /// `run`/`build`) uses this. Combined with denying the program's
    /// value-taking global flags (so the first non-flag token can't be shifted
    /// off the real subcommand), it can never reach a code-executing subcommand.
    /// Empty (the default) disables the allowlist and leaves only the denylist.
    pub allowed_subcommands: Vec<String>,
    /// Environment variables forced at spawn to neutralize config-driven hooks
    /// (e.g. `GIT_PAGER=cat`, empty `GIT_SSH_COMMAND`).
    pub env: Vec<CommandEnvVar>,
}

/// The seeded default policies. The `git` policy reproduces exactly the
/// hardening that `run_command` previously applied in code, so behavior is
/// unchanged on a fresh install — it's just visible and editable now. Because
/// [`OffloadSettings`] is `#[serde(default)]`, existing config files missing
/// the `command_policies` key inherit this automatically (no migration).
pub fn default_command_policies() -> Vec<CommandPolicy> {
    fn env(key: &str, value: &str) -> CommandEnvVar {
        CommandEnvVar {
            key: key.to_string(),
            value: value.to_string(),
        }
    }
    fn s(v: &str) -> String {
        v.to_string()
    }
    vec![CommandPolicy {
        program: s("git"),
        denied_flags: vec![
            s("-c"),
            s("-C"),
            s("--config-env"),
            s("--exec-path"),
            s("--git-dir"),
            s("--work-tree"),
            s("--upload-pack"),
            s("--receive-pack"),
            // The remaining value-consuming git globals. They aren't dangerous
            // themselves, but `dangerous_args` finds the subcommand as the first
            // non-flag token — so a non-denied value-taking global would shift
            // that token onto its own value and let `git --namespace x config …`
            // slip the `config` check. Denying every value-taking global keeps
            // that assumption sound (a read probe never needs these).
            s("--namespace"),
            s("--super-prefix"),
            s("--attr-source"),
        ],
        denied_subcommands: vec![s("config")],
        allowed_subcommands: vec![],
        env: vec![
            env("GIT_PAGER", "cat"),
            env("PAGER", "cat"),
            env("GIT_SSH_COMMAND", ""),
            env("GIT_TERMINAL_PROMPT", "0"),
            env("GIT_CONFIG_NOSYSTEM", "1"),
            env("GIT_CONFIG_GLOBAL", "/dev/null"),
        ],
    }]
}

/// V21 F7: the program names the curated "safe read-only commands" preset adds
/// to `command_allowlist` (deduped by the UI's merge action). `git` is already
/// hardened by [`default_command_policies`] (read probes allowed, exec/escape
/// vectors blocked); `cargo` is paired with [`readonly_cargo_policy`], which
/// pins it to `metadata`/`tree` — both resolve/read the dependency graph and
/// neither runs build scripts or project code. Deliberately excludes anything
/// that writes, fetches the network by default, or executes project code
/// (`npm`, `make`, bare `cargo`).
pub fn readonly_command_preset() -> Vec<String> {
    vec!["git".to_string(), "cargo".to_string()]
}

/// V21 F7: the `cargo` policy the read-only preset installs. Allowlists only
/// `metadata` and `tree`, and denies cargo's value-taking / code-executing
/// global flags (`--config` can inject a runner/wrapper; `-C` escapes the
/// working dir; `-Z` enables unstable behavior) — denying the value-taking
/// globals also keeps the `allowed_subcommands` check sound, since none of them
/// can shift the first-non-flag token off the real subcommand. `--explain` /
/// `--color` are denied only to preserve that soundness (both take a value);
/// their glued short-flag forms (`-Cdir`, `-Zflag`) are caught by the same
/// flag-denial machinery `git` uses.
pub fn readonly_cargo_policy() -> CommandPolicy {
    fn s(v: &str) -> String {
        v.to_string()
    }
    CommandPolicy {
        program: s("cargo"),
        denied_flags: vec![
            s("--config"),
            s("-C"),
            s("-Z"),
            s("--explain"),
            s("--color"),
        ],
        denied_subcommands: vec![],
        allowed_subcommands: vec![s("metadata"), s("tree")],
        env: vec![],
    }
}

/// V21 F7: merge the curated read-only preset into an existing `allowlist` +
/// `policies` in place. Idempotent (a merge-into-settings action, not a mode):
/// preset programs already present are not duplicated, and the `cargo` policy is
/// installed only if the user has no policy for `cargo` yet — so re-clicking the
/// button, or clicking it after hand-editing, never grows or resets what's
/// there. The user can freely prune any of it afterward. `git`'s policy is
/// seeded by [`default_command_policies`], so this only ever needs to add the
/// `cargo` one.
pub fn merge_readonly_preset(allowlist: &mut Vec<String>, policies: &mut Vec<CommandPolicy>) {
    for prog in readonly_command_preset() {
        if !allowlist.iter().any(|a| a.eq_ignore_ascii_case(&prog)) {
            allowlist.push(prog);
        }
    }
    if !policies
        .iter()
        .any(|p| p.program.eq_ignore_ascii_case("cargo"))
    {
        policies.push(readonly_cargo_policy());
    }
}

/// On/off toggles for the native baseline offload tools (built into
/// cImp, zero external deps). All default on so offload works with no
/// MCP servers installed.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct OffloadToolToggles {
    /// Bounded line/byte reads within an `allowed_root`.
    pub read_file: bool,
    /// Directory enumeration within an `allowed_root` — the ground-truth
    /// answer to "what files exist / how many" (V21).
    pub list_dir: bool,
    /// `grep`/`glob` across an `allowed_root`; matching paths + snippets.
    pub code_search: bool,
    /// Allowlisted, read-only command execution. Default true, but inert
    /// until `command_allowlist` is populated (deny-by-default).
    pub run_command: bool,
    /// V21: run one of the project's *configured* check commands (build /
    /// typecheck / lint / test) and get back deduplicated diagnostics — lets
    /// the worker prove build/test/lint claims instead of asserting them.
    /// Default true, but inert until the top-level `checks` array is
    /// non-empty (gated identically to the `run_check` MCP tool).
    pub run_check: bool,
}

impl Default for OffloadToolToggles {
    fn default() -> Self {
        Self {
            read_file: true,
            list_dir: true,
            code_search: true,
            run_command: true,
            run_check: true,
        }
    }
}

/// V37 (contract C2): where an MCP server's definition came from.
///
/// `External` is every server the user pasted into Settings — an endpoint cImp
/// neither ships nor supervises. `Internal` is reserved for servers cImp itself
/// manages (#41's managed bundle); nothing in V37 *hosts* one, the flag only
/// badges the row and scopes the V37 Phase E description screen, which applies
/// to external surfaces only.
///
/// Defaults to `External` — the same direction the V32 `toolclass.rs` table
/// takes for an unknown name, and the safe one: a server whose origin nobody
/// declared is treated as untrusted, not as cImp's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpOrigin {
    /// A server cImp itself manages (reserved; see #41).
    Internal,
    /// A user-configured third-party endpoint. The default.
    #[default]
    External,
}

/// V37 (contract C1/C2): a user-created grouping of MCP servers, referenced by
/// server `name` (there is no parallel id space — the name IS the id).
///
/// The category list is **global-only**: a `Vec` is safe here precisely because
/// no project overlay ever writes it, so `deep_merge`'s replace-arrays-wholesale
/// rule (`persistence.rs`) can never half-merge two category lists. The
/// per-project surface is [`McpActivation`], which is maps for exactly that
/// reason.
///
/// Membership is many-to-many: a server may appear in several categories, and a
/// server in no category rides its own toggle alone. See the effective-enable
/// predicate `offload::mcp_host::effective_enable` — the single owner of that
/// rule for both advertisement and dispatch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct McpCategory {
    /// Unique, user-chosen. This IS the category's id: renaming a category
    /// creates a new identity, and any activation entry keyed by the old name
    /// becomes inert (it names a category that no longer exists).
    pub name: String,
    /// Member server names ([`McpServerConfig::name`]). A name with no matching
    /// server is simply ignored — membership is resolved against the live list.
    pub servers: Vec<String>,
    /// Global on/off for the whole category. A project overlay may override it
    /// through [`McpActivation::categories`].
    pub enabled: bool,
}

impl Default for McpCategory {
    fn default() -> Self {
        Self {
            name: String::new(),
            // A freshly-created category is ON: creating a group must never
            // silently take tools away from the surface (the V37 C2 migration
            // invariant in the small).
            enabled: true,
            servers: Vec::new(),
        }
    }
}

/// V37 (contract C2): the per-project activation overlay — the ONLY part of the
/// MCP registry a project settings overlay may carry.
///
/// Both halves are **maps keyed by name**, never arrays. `persistence::deep_merge`
/// merges JSON objects key-by-key but replaces arrays *wholesale*, so an array
/// here would mean a project that overrides one server silently discards every
/// other project-level entry the global file carried. As maps, a project's
/// `{"servers": {"ddg": false}}` overlays exactly that one key.
///
/// An entry is an OVERRIDE, not a copy: `Some(v)` wins over the global
/// `enabled`, absence inherits it. That is what lets the UI show
/// inherited-vs-overridden and offer a revert (delete the key).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct McpActivation {
    /// Per-category overrides, keyed by [`McpCategory::name`].
    pub categories: BTreeMap<String, bool>,
    /// Per-server overrides, keyed by [`McpServerConfig::name`].
    pub servers: BTreeMap<String, bool>,
}

/// One user-installed MCP tool server, aggregated by cImp's MCP host.
/// Mirrors Claude Code's own `mcpServers` entry shape: either a stdio
/// server (`command` + `args` + `env`) or an HTTP server (`url`). Only
/// read-class tools from each server are exposed this milestone.
///
/// The hand-rolled `Debug` redacts `env` values, which may carry API
/// keys, and (V33 Phase E) the HTTP transport's `auth_token`, so a stray
/// `?settings` log line cannot leak either.
// V37 F5: `PartialEq`/`Eq` because the registry is written THROUGH to the
// physical global settings file on save (`persistence::sync_mcp_registry_into`),
// and that write-through only rewrites the file when the array actually moved.
// Derived rather than hand-rolled: every field is connection- or
// identity-relevant, so "equal" must mean all of them, and a field added later
// must join the comparison without anyone remembering to.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct McpServerConfig {
    /// Display + namespacing prefix (e.g. `ddg`, `git`). Tools are
    /// exposed to the model as `<name>__<tool>`.
    pub name: String,
    /// Stdio transport: the server executable. Empty when `url` is set.
    pub command: String,
    /// Args for the stdio `command`.
    pub args: Vec<String>,
    /// Extra env for the stdio `command` (may carry secrets — redacted
    /// in `Debug`).
    pub env: HashMap<String, String>,
    /// HTTP transport: the server base URL. Empty when `command` is set.
    pub url: String,
    /// V33 Phase E: bearer token sent on every HTTP request to
    /// [`url`](Self::url) — `initialize`, `notifications/initialized`,
    /// `tools/list` and every `tools/call`. Empty (the default, and every
    /// pre-V33 settings file) = no `Authorization` header, i.e. exactly the
    /// pre-V33 behaviour, which is also the safe direction. Redacted in
    /// `Debug`; ignored by the stdio transport, where a secret belongs in
    /// [`env`](Self::env) instead.
    ///
    /// It is part of [`config_sig`](crate::offload::mcp_host::host_config_sig)
    /// (as a fingerprint, not cleartext) — without that, editing the token in
    /// Settings would never reconnect the server and the change would silently
    /// do nothing.
    pub auth_token: String,
    /// **Per-harness exposure**, keyed by registry id — V40 Phase B (locked
    /// decision 5), schema 36. Replaces the `claude_access` / `opencode_access`
    /// pair the 35 -> 36 migration copies into it.
    ///
    /// An absent key is *not exposed* ([`McpAccess::default`]), which is both
    /// the pre-existing default for a new server and the only safe answer to a
    /// grant question about a harness nobody has decided about. A key for an
    /// unregistered harness round-trips untouched, so a downgrade does not
    /// silently revoke a grant the user will get back on the next upgrade.
    ///
    /// Read through `offload::mcp_host::Consumer::wants`, never field by field:
    /// that is where the offload/audit consumers' conservative fold onto the
    /// same flags lives.
    pub access: BTreeMap<String, McpAccess>,
    /// Expose this server's tools to the **offload worker** (the local model,
    /// via the warm `McpHost`). This is the legacy `enabled` behavior.
    ///
    /// Not in [`Self::access`] on purpose: the worker is cImp's own in-process
    /// consumer, not a harness, and a map keyed by `HarnessId` has no honest
    /// slot for it (locked decision 25 — `Consumer::conservative_grant` stays
    /// core because it is a security default, not a harness fact).
    pub offload_access: bool,
    /// V37 (contract C2): where this server came from — see [`McpOrigin`].
    /// Metadata only in V37: it badges the Settings row and scopes the Phase E
    /// tool-description screen to external surfaces. Defaults to `external`.
    pub origin: McpOrigin,
    /// V37 (contract C2/C3): the server's own global on/off switch, orthogonal
    /// to the three per-consumer `*_access` flags.
    ///
    /// `*_access` answers *who may see this server*; `enabled` answers *does it
    /// exist at all right now*. A disabled server is not connected, not
    /// advertised to any consumer, and `call_for_consumer` refuses a stale call
    /// naming the disabled state (contract C4). A project overlay may override
    /// it through [`McpActivation::servers`]; membership of a disabled category
    /// can turn it off even when this is `true`.
    ///
    /// Defaults to `true` — the C2 migration invariant: every pre-v32 config's
    /// effective tool surface is unchanged after the upgrade.
    ///
    /// Part of `offload::mcp_host::config_sig`, so a toggle moves
    /// `host_config_sig` and `warm_host` actually reconciles.
    pub enabled: bool,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env_keys: Vec<&String> = self.env.keys().collect();
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            // Redact values; show only which keys are present.
            .field("env_keys", &env_keys)
            .field("url", &self.url)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("access", &self.access)
            .field("offload_access", &self.offload_access)
            .field("origin", &self.origin)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: String::new(),
            auth_token: String::new(),
            access: BTreeMap::new(),
            offload_access: true,
            // V37 C2: unknown provenance ⇒ external (untrusted), and a server
            // exists unless the user says otherwise.
            origin: McpOrigin::External,
            enabled: true,
        }
    }
}

/// V8-02: native + MCP-server tool names treated as **local-data** tools —
/// they read the user's files / run commands / query the local repo, so
/// their output must never leave the machine. The router refuses to send a
/// task needing any of these to a cloud backend, and a cloud backend's
/// default [`ToolScope`] denies them. (MCP servers are matched by their
/// configured `name`; native tools by their fixed name.)
///
/// #48, finding **F-12**: `run_check` joined this set. It executes the project's
/// **configured** build/test/lint commands and returns their output — which
/// quotes source — so it is closer to `run_command` than to anything on the
/// cloud-allowed side. Membership here only fixes a **new** backend (it is what
/// [`ToolScope::default_for`] excludes, and what the v29 → v30 migration
/// backfills into an existing "web/docs only" exclusion list). An already
/// configured backend whose scope does *not* name it — `ToolScope::All`, or a
/// hand-picked `AllExcept` — is fixed by the **call-time** half instead:
/// `BackendGate::admit`'s `run_check` rule plus the
/// [`Settings::checks_allow_remote_worker`] opt-in. Neither half is sufficient
/// alone; see `offload::backend_gate`.
///
/// Mirrored by hand in `src/lib/settings/types.ts` (`LOCAL_DATA_TOOLS`), which
/// is what the Settings window writes when a backend's cloud flag is toggled.
///
/// #48 finding **F-27**: that mirror went stale the day `run_check` was added
/// here, so the settings-side half of F-12 had no effect for a release. It is now
/// held by an `include_str!` tripwire —
/// `settings::frontend_mirrors::local_data_tools_mirror_is_current` — which
/// compares the two as SETS in both directions, so editing this list without
/// editing that file fails `cargo test`.
pub const LOCAL_DATA_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "code_search",
    "run_command",
    "run_check",
    "filesystem",
    "git",
];

/// V8-02: web/docs tool names a cloud backend is allowed by default — they
/// reach out to the public internet, not the user's machine. Kept as the
/// documented counterpart to [`LOCAL_DATA_TOOLS`] (the cloud-allow taxonomy)
/// even though no code path currently reads it.
#[allow(dead_code)]
pub const WEB_DOCS_TOOLS: &[&str] = &["duckduckgo", "fetch", "context7"];

/// V8-02: which capability tier a backend serves. The router prefers a
/// `Fast` backend for trivial single-pass work and a `Quality` backend for
/// real reasoning; Claude's `tier` hint on `offload_task` biases the choice.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendTier {
    /// Small/fast backend (e.g. an 8 GB LAN box): trivial, single-pass,
    /// small-context offloads only.
    Fast,
    /// Large/capable backend (the main model): real reasoning, big context,
    /// multi-tool loops. The default when unspecified.
    #[default]
    Quality,
}

/// V8-02: a backend's allow-list over the global tool pool (native tools +
/// configured MCP-server names). Only allowed tools are placed in the
/// `tools` array sent to that backend's model — the privacy boundary for
/// cloud backends.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum ToolScope {
    /// Every tool in the pool (Local + trusted LAN default).
    #[default]
    All,
    /// Only the named tools.
    Only { tools: Vec<String> },
    /// Every tool except the named ones (cloud default = `AllExcept` the
    /// local-data set).
    AllExcept { tools: Vec<String> },
}

impl ToolScope {
    /// Whether `tool` (a native tool name or an MCP-server name) is allowed
    /// under this scope. Matching is exact; MCP tools namespaced as
    /// `<server>__<tool>` are tested by their server prefix via
    /// [`Self::allows_namespaced`].
    pub fn allows(&self, tool: &str) -> bool {
        match self {
            ToolScope::All => true,
            ToolScope::Only { tools } => tools.iter().any(|t| t == tool),
            ToolScope::AllExcept { tools } => !tools.iter().any(|t| t == tool),
        }
    }

    /// Whether a (possibly namespaced) tool id is allowed. An MCP tool is
    /// exposed to the model as `<server>__<tool>`; scopes are written in
    /// terms of native tool names and MCP *server* names, so we test the
    /// server prefix for namespaced ids and the whole id otherwise.
    pub fn allows_namespaced(&self, tool_id: &str) -> bool {
        let key = tool_id.split("__").next().unwrap_or(tool_id);
        self.allows(key)
    }

    /// The default scope for a backend of the given kind: Local and trusted
    /// LAN get everything; a cloud backend gets web/docs only (the
    /// local-data set denied) until the user explicitly opts in. The
    /// Settings UI applies this when a backend's cloud flag is toggled; the
    /// router/tests exercise it as the canonical default.
    #[allow(dead_code)]
    pub fn default_for(kind_is_cloud: bool) -> Self {
        if kind_is_cloud {
            ToolScope::AllExcept {
                tools: LOCAL_DATA_TOOLS.iter().map(|s| s.to_string()).collect(),
            }
        } else {
            ToolScope::All
        }
    }
}

/// V8-02: kind-specific configuration for one offload backend.
///
/// `Local` mirrors V8-01's single-server config (the command cImp owns +
/// spawns as a read-only tab). `Remote` is a `base_url` cImp only
/// health-checks and connects to — no process, no tab. The hand-rolled
/// `Debug` on [`OffloadBackend`] redacts **both** variants' `auth_token`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OffloadBackendKind {
    /// cImp owns the process: the V8-01 `server_command` + `autostart` +
    /// the read-only Offload server dashboard (Tool Activity tab) with
    /// Start/Stop/Reset.
    Local {
        /// The single source-of-truth `llama-server` command (shlex-parsed
        /// to spawn; host/port/`-np`/`--jinja` parsed from it).
        server_command: String,
        /// Spawn at app launch and keep warm (else lazy on first offload).
        autostart: bool,
        /// When true, the Offload server dashboard's Start button shows the
        /// command in an editable confirm popup first; an edited command is
        /// used for that launch only and never persisted.
        #[serde(default)]
        show_command_on_start: bool,
        /// V33 Phase E: bearer token sent on every request cImp makes to this
        /// server — `/health`, `/props` and the agent loop's chat completions.
        /// Empty (the default, and every pre-V33 settings file) = no
        /// `Authorization` header at all, i.e. exactly the pre-V33 behaviour.
        /// Redacted in `Debug`; stored cleartext in `settings.json` like every
        /// other house secret (`ClaudeLocalSettings::auth_token` is the model).
        ///
        /// This is the CLIENT half only. A `llama-server` ignores auth unless it
        /// was launched with `--api-key`, so for a backend cImp itself spawns
        /// the same value must also appear as `--api-key <token>` in
        /// `server_command` — that is what makes the server demand it, and it is
        /// also where [`crate::offload::server::derive_opencode_provider`]
        /// already reads the key from for OpenCode's `local-llama` provider.
        ///
        /// **The effective token may come from the command rather than from
        /// this field** (V33 stage 3). Because the two halves above are the same
        /// secret, requiring it in both places was a trap: get one wrong and
        /// offload and OpenCode disagree about a server they both talk to. So an
        /// EMPTY value here falls back to the `--api-key` already parsed out of
        /// `server_command`. A non-empty value still wins — the field is how you
        /// override a stale or absent flag, which is the case that matters when
        /// cImp does not launch the server itself.
        ///
        /// **Do not read this field directly to decide what to send.**
        /// [`OffloadBackendKind::effective_auth_token`] is the resolver, and
        /// `crate::offload::server::resolve_local_auth` is what it and
        /// `LlamaServer::with_config` share.
        ///
        /// **`#[serde(default)]` is load-bearing and must stay.** Serde's
        /// container-level default does NOT apply to enum variants (this enum is
        /// `#[serde(tag = "type")]` with no container default), so without it
        /// every existing settings file — none of which carries the key — would
        /// fail to deserialize the whole `backends` array. Same reason
        /// `show_command_on_start` above carries its own.
        #[serde(default)]
        auth_token: String,
    },
    /// cImp holds a `base_url` (+ optional auth) and health-checks it; it
    /// cannot start/stop the process. A LAN box or a cloud API.
    Remote {
        /// HTTP origin of the OpenAI-compatible endpoint, e.g.
        /// `http://192.168.1.50:8080` or `https://api.example.com/v1`.
        base_url: String,
        /// Optional bearer token (cloud APIs). Redacted in `Debug`.
        auth_token: String,
        /// Marks a backend whose data leaves the user's machine/network.
        /// Gates the consent toggle, the distinct UI badge, and the
        /// default web/docs-only tool scope.
        is_cloud: bool,
        /// Explicit acknowledgement that offloading here sends task text
        /// (and any local-data tool results, if scoped in) to a third
        /// party. A cloud backend is unusable until this is `true`.
        cloud_consent: bool,
    },
    /// **V39 Phase C — a facade over an open AI tab** (locked decision 3).
    ///
    /// cImp owns neither a process nor a URL here: the "backend" is another
    /// harness tab that the delegation engine drives exactly as a user would.
    /// The requesting harness cannot tell it from an HTTP server — it sees the
    /// user-chosen backend name, `LAN` as the kind
    /// ([`crate::offload::mcp`]'s `backend_label`), and nothing else.
    ///
    /// **Never written by the user, never persisted.** Entries of this kind
    /// exist only in [`Settings::effective_offload_backends`], synthesized from
    /// every AI tab whose `delegation_role` is
    /// [`RemoteOffload`](DelegationRole::RemoteOffload). The backend editor
    /// lists them read-only ("configured on the tab"), and
    /// `a_synthesized_facade_never_reaches_the_persisted_backend_list` pins
    /// that a save round-trip cannot write one.
    HarnessTab {
        /// The worker tab's id. Internal: it keys the engine's registry and the
        /// readiness probe, and it is the ONE field that must never reach a
        /// driver-facing string — see `backend_label` and
        /// `the_live_description_names_the_backend_not_the_tab`.
        tab: String,
    },
}

impl OffloadBackendKind {
    /// The bearer token cImp actually sends to this backend, or `""` for none.
    ///
    /// **The one resolver.** Three call sites read a Local backend's credential
    /// and they must not disagree — the supervised pool
    /// (`offload::server::LlamaServer`, which reaches the same answer through
    /// `resolve_local_auth` because it also needs to know *where* the token came
    /// from), the Offload server dashboard's `/slots`+`/metrics` poller, and the
    /// headless `cimp --offload-mcp` child that runs its own agent loop when the
    /// app is unreachable. A backend authenticated in two of the three is a
    /// credential bug that only shows up on whichever path the user is not
    /// looking at.
    ///
    /// For `Local`, an empty configured token inherits the `--api-key` in
    /// `server_command` — see the field doc above and
    /// [`crate::offload::server::resolve_local_auth`]. For `Remote` there is no
    /// command to inherit from, so it is the configured token verbatim.
    pub fn effective_auth_token(&self) -> String {
        match self {
            OffloadBackendKind::Local {
                auth_token,
                server_command,
                ..
            } => crate::offload::server::resolve_local_auth(auth_token, server_command).token,
            OffloadBackendKind::Remote { auth_token, .. } => auth_token.clone(),
            // A facade is driven through a PTY, not over HTTP: there is no
            // request to authenticate and no credential to resolve. Empty is
            // the honest answer, and every caller already treats it as "send no
            // `Authorization` header at all".
            OffloadBackendKind::HarnessTab { .. } => String::new(),
        }
    }
}

/// V8-02: one backend in the offload pool.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OffloadBackend {
    /// Display + routing-log name (e.g. `main`, `lan-3070`, `cloud`).
    pub name: String,
    /// Per-backend enable toggle. Disabled backends are invisible to the
    /// router and the capability union.
    pub enabled: bool,
    /// Local (owned process) vs. Remote (health-checked URL).
    pub kind: OffloadBackendKind,
    /// Context window to assume when `/props` is unavailable (many cloud
    /// APIs don't expose it). Ignored for endpoints that report `n_ctx`.
    pub declared_context: Option<u32>,
    /// Model label to show when `/props` is unavailable (cosmetic).
    pub declared_model: String,
    /// Which tier this backend serves (router bias).
    pub tier: BackendTier,
    /// This backend's allow-list over the global tool pool.
    pub tool_scope: ToolScope,
}

impl std::fmt::Debug for OffloadBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact both variants' auth_token so a stray `?settings` log can't
        // leak one. (`server_command` is printed verbatim — it is a command
        // line, and a user who puts `--api-key` in it has already put the
        // secret somewhere argv-visible; see the Local `auth_token` doc.)
        let kind_dbg: String = match &self.kind {
            OffloadBackendKind::Local {
                server_command,
                autostart,
                show_command_on_start,
                auth_token,
            } => format!(
                "Local {{ server_command: {server_command:?}, autostart: {autostart}, show_command_on_start: {show_command_on_start}, auth_token: {} }}",
                if auth_token.is_empty() { "<none>" } else { "<redacted>" }
            ),
            OffloadBackendKind::Remote {
                base_url,
                auth_token,
                is_cloud,
                cloud_consent,
            } => format!(
                "Remote {{ base_url: {base_url:?}, auth_token: {}, is_cloud: {is_cloud}, cloud_consent: {cloud_consent} }}",
                if auth_token.is_empty() { "<none>" } else { "<redacted>" }
            ),
            // No secret of any kind on this variant — the whole value is a tab
            // id, which is already all over the logs.
            OffloadBackendKind::HarnessTab { tab } => format!("HarnessTab {{ tab: {tab:?} }}"),
        };
        f.debug_struct("OffloadBackend")
            .field("name", &self.name)
            .field("enabled", &self.enabled)
            .field("kind", &format_args!("{kind_dbg}"))
            .field("declared_context", &self.declared_context)
            .field("declared_model", &self.declared_model)
            .field("tier", &self.tier)
            .field("tool_scope", &self.tool_scope)
            .finish()
    }
}

impl Default for OffloadBackendKind {
    fn default() -> Self {
        OffloadBackendKind::Local {
            server_command: String::new(),
            autostart: false,
            show_command_on_start: false,
            auth_token: String::new(),
        }
    }
}

impl Default for OffloadBackend {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            kind: OffloadBackendKind::default(),
            declared_context: None,
            declared_model: String::new(),
            tier: BackendTier::Quality,
            tool_scope: ToolScope::All,
        }
    }
}

impl OffloadBackend {
    /// A cloud backend that hasn't been consented to is not usable: the
    /// router skips it and the UI flags it. Non-cloud backends are always
    /// "consented".
    pub fn cloud_blocked(&self) -> bool {
        matches!(
            self.kind,
            OffloadBackendKind::Remote {
                is_cloud: true,
                cloud_consent: false,
                ..
            }
        )
    }
}

impl OffloadSettings {
    /// The effective backend pool. Returns the configured [`backends`]
    /// when non-empty; otherwise synthesizes a single Local backend from
    /// the legacy `server_command`/`autostart` fields so a V8-01 config
    /// (or one whose migration hasn't run) keeps working as one
    /// quality-tier, all-tools local backend.
    ///
    /// [`backends`]: Self::backends
    pub fn effective_backends(&self) -> Vec<OffloadBackend> {
        if !self.backends.is_empty() {
            return self.backends.clone();
        }
        vec![OffloadBackend {
            name: "local".to_string(),
            enabled: true,
            kind: OffloadBackendKind::Local {
                server_command: self.server_command.clone(),
                autostart: self.autostart,
                show_command_on_start: false,
                // The legacy V8-01 fields never had a token; a user who wants
                // one configures a real backend entry.
                auth_token: String::new(),
            },
            declared_context: None,
            declared_model: String::new(),
            tier: BackendTier::Quality,
            tool_scope: ToolScope::All,
        }]
    }

    /// The `server_command` of the primary Local backend the OpenCode
    /// `local-llama` provider tracks: the first enabled Local backend with a
    /// non-blank command (from [`effective_backends`], so a legacy single-local
    /// config resolves too). `None` when there's no usable Local command.
    ///
    /// [`effective_backends`]: Self::effective_backends
    pub fn primary_local_command(&self) -> Option<String> {
        self.effective_backends()
            .into_iter()
            .find_map(|b| match b.kind {
                OffloadBackendKind::Local { server_command, .. }
                    if b.enabled && !server_command.trim().is_empty() =>
                {
                    Some(server_command)
                }
                _ => None,
            })
    }

    /// Whether at least one MCP server is exposed to **any harness**.
    ///
    /// V40 Phase B collapsed `any_claude_mcp` + `any_opencode_mcp` into this
    /// one predicate: both bodies asked the same question of a different half
    /// of the same field pair, and every caller wanted the OR of them.
    ///
    /// # V37 D-1, amended by Phase F: NOTHING here is spawn-baked any more
    ///
    /// **The new record.** The `cimp-offload` proxy child is written into EVERY
    /// AI tab's harness config unconditionally (`harness::claude::overlay` /
    /// `harness::opencode::config`), so neither `*_access` nor `enabled` decides
    /// whether a tab has a proxy — every tab has one, and both flags are
    /// re-read live: `*_access` picks the per-consumer surface at
    /// `POST /mcp/list` time, `enabled` (with `mcp_categories` /
    /// `mcp_activation`, via `offload::mcp_host::effective_enable`) is
    /// re-evaluated on every reconcile and every dispatch. Flipping either one
    /// reaches a running tab through the contract-C5 pulse. This predicate and
    /// its two neighbours survive for HOST-LIFECYCLE and advertisement
    /// decisions only; they no longer gate any injection.
    ///
    /// **The original warning, kept because its reasoning is what got us here.**
    /// D-1 said: do not add an `enabled` term, because with every server
    /// disabled at spawn time the tab would come up with no proxy and
    /// re-enabling later could not reach it. That hazard was real, and Phase F
    /// found the same hole one level up — `*_access` had it too, for a tab
    /// spawned with zero grants. The fix was not a better predicate but removing
    /// the spawn-time decision entirely. So the warning is now MOOT FOR
    /// INJECTION (there is no injection gate left to poison) and still live for
    /// [`Self::mcp_host_needed`]: a host that shut itself down because
    /// everything was toggled off would have nothing left to turn back on.
    ///
    /// Spawn broadly — now maximally broadly — and enforce at dispatch.
    pub fn any_harness_mcp(&self) -> bool {
        self.mcp_servers
            .iter()
            .any(|m| m.access.values().any(|a| a.enabled))
    }

    /// Whether the warm MCP HOST (the pool of user-configured MCP servers)
    /// needs to run: offload is enabled (the worker needs the host) OR any
    /// MCP server is exposed to Claude Code or OpenCode directly (each
    /// reaches it over the loopback, independent of offload). Drives the
    /// warm-host lifecycle. NOTE: runtime/loopback startup gates on the
    /// broader [`Settings::loopback_needed`] — graph and Code Audit children
    /// need the loopback without needing the host.
    ///
    /// V37 D-1: composed from the two `*_access` predicates, so it inherits
    /// their deliberate blindness to `enabled` (see [`Self::any_harness_mcp`]).
    /// The warm host must run whenever a server COULD become reachable: it is
    /// the host that owns `effective_enable`, holds the disabled set behind
    /// contract C4's refusal, and is the thing a re-enable has to reconcile
    /// into. A host that shut itself down because everything was toggled off
    /// would have nothing left to turn back on. **This is the one place D-1's
    /// warning is still live** — Phase F retired its injection half, not this.
    ///
    /// V37 Phase F: this predicate must stay NARROW even though the proxy child
    /// is now unconditional. The child is not a reason to hold a pool of
    /// connections open: with no server exposed to anyone it asks
    /// `POST /mcp/list` and gets an empty array from a torn-down host — which
    /// is the correct answer, not a degraded one (`tool_defs_for_*` return
    /// nothing when the pool is empty, and the child's own graph/offload tools
    /// are unaffected). The first grant flips this predicate, `warm_host`
    /// reconciles, and the pulse reaches the tab that was already listening.
    pub fn mcp_host_needed(&self) -> bool {
        self.enabled || self.any_harness_mcp()
    }
}

/// One notification slot: a per-event `{ enabled, text }` pair. The
/// firing path requires both `enabled == true` AND a non-empty `text`
/// to dispatch — the empty-text suppression matches the pre-v1.11
/// convention so users who hand-edit a slot to `""` still see it
/// disabled.
///
/// Custom `Deserialize` accepts either a bare string (the v1.10-and-
/// earlier shape — empty string maps to `enabled: false`, non-empty to
/// `enabled: true`) or the v1.11 object shape, so a legacy file loads
/// without losing the user's text. On next save the file is rewritten
/// in the new shape.
#[derive(Clone, Serialize, Debug, Default)]
pub struct NotificationSlot {
    pub enabled: bool,
    pub text: String,
}

impl NotificationSlot {
    /// A configured-and-enabled slot. Constructor for the builtin tab
    /// defaults so the call sites stay terse.
    pub fn enabled(text: impl Into<String>) -> Self {
        Self {
            enabled: true,
            text: text.into(),
        }
    }
}

impl<'de> Deserialize<'de> for NotificationSlot {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            // v1.10-and-earlier shape: bare string. Empty string was the
            // documented "leave blank to disable" convention, so map it
            // to `enabled: false`. Non-empty maps to `enabled: true` so
            // the upgrade path preserves prior firing behavior.
            serde_json::Value::String(s) => Ok(Self {
                enabled: !s.is_empty(),
                text: s,
            }),
            serde_json::Value::Object(_) => {
                #[derive(Deserialize)]
                struct Inner {
                    #[serde(default = "default_true")]
                    enabled: bool,
                    #[serde(default)]
                    text: String,
                }
                fn default_true() -> bool {
                    true
                }
                let inner: Inner = serde_json::from_value(v).map_err(D::Error::custom)?;
                Ok(Self {
                    enabled: inner.enabled,
                    text: inner.text,
                })
            }
            serde_json::Value::Null => Ok(Self::default()),
            _ => Err(D::Error::custom(
                "notification slot: expected string or { enabled, text } object",
            )),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AiNotificationConfig {
    pub idle: NotificationSlot,
    pub awaiting_permission: NotificationSlot,
    /// Spoken when a `kind: question` pattern fires (AskUserQuestion-style
    /// multi-option prompts). Older settings files that pre-date this
    /// field deserialize to a default-disabled slot via
    /// `#[serde(default)]`; the integrity check at load doesn't backfill
    /// it, so users on the two AI builtins get the configured-defaults
    /// experience only on fresh installs. (See `default_claude_tab` and
    /// `default_claude_local_tab`.)
    pub question: NotificationSlot,
    pub error: NotificationSlot,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ShellNotificationConfig {
    pub error: NotificationSlot,
    /// `{code}` placeholder is interpolated with the actual exit code in M4.
    pub exited: NotificationSlot,
}

impl Default for ShellNotificationConfig {
    fn default() -> Self {
        Self {
            error: NotificationSlot::enabled("Shell encountered an error"),
            exited: NotificationSlot::enabled("Shell exited (code {code})"),
        }
    }
}

// --- Builtin defaults -------------------------------------------------------
//
// Used by:
//   1. The migration step to fill in missing entries (e.g. a claude-local
//      tab absent from an upgraded settings file).
//   2. The integrity check at load time to restore deleted builtins.
//   3. `Settings::default()` to seed a fresh-install file before the first
//      save.

pub fn default_claude_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: CLAUDE_TAB_ID.to_string(),
        builtin: true,
        name: "Claude".to_string(),
        command: "claude".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        tts_injection: TtsInjection { enabled: true },
        notifications: AiNotificationConfig {
            idle: NotificationSlot::enabled("Claude is idle"),
            awaiting_permission: NotificationSlot::enabled("Claude is awaiting permission"),
            question: NotificationSlot::enabled("Claude has a question"),
            error: NotificationSlot::enabled("Claude encountered an error"),
        },
        // Pre-dismissed so the overlay code can use a single per-tab
        // predicate. Aider used to fire a first-launch banner (V1.1)
        // but Claude tabs never did.
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: false,
        // V39 Phase A: a fresh tab accepts the keyboard. The read-only lock
        // is a deliberate user action, never a default.
        read_only: false,
        // V39 Phase B: a fresh tab is nobody's delegation target. Both roles
        // are opt-in, per tab, from that tab's own popover.
        delegation_role: DelegationRole::None,
        delegation_backend: DelegationBackend::default(),
        // V39: a newly created AI tab starts with every tab-scoped injection
        // control explicitly OFF. L1 and every L2 ship on; the per-tab row is
        // the switch the user reaches for, from this tab's shield badge. NOT
        // `Default::default()` — that is all-`Inherit`, which is what an
        // ABSENT cell in an existing settings file must keep meaning (schema
        // step 34 → 35).
        injection_overrides: crate::settings::injection::TabInjectionOverrides::all_off(),
    })
}

/// V1.4-07: second Claude tab, preconfigured to talk to a local LLM
/// via the global `claude_local` provider settings. Replaces the
/// pre-V1.4-07 Aider builtin tab.
pub fn default_claude_local_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: CLAUDE_LOCAL_TAB_ID.to_string(),
        builtin: true,
        name: "Claude (local)".to_string(),
        command: "claude".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        tts_injection: TtsInjection { enabled: true },
        notifications: AiNotificationConfig {
            idle: NotificationSlot::enabled("Claude (local) is idle"),
            awaiting_permission: NotificationSlot::enabled("Claude (local) is awaiting permission"),
            question: NotificationSlot::enabled("Claude (local) has a question"),
            error: NotificationSlot::enabled("Claude (local) encountered an error"),
        },
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: true,
        // V39 Phase A: a fresh tab accepts the keyboard. The read-only lock
        // is a deliberate user action, never a default.
        read_only: false,
        // V39 Phase B: a fresh tab is nobody's delegation target. Both roles
        // are opt-in, per tab, from that tab's own popover.
        delegation_role: DelegationRole::None,
        delegation_backend: DelegationBackend::default(),
        // V39: see `default_claude_tab`.
        injection_overrides: crate::settings::injection::TabInjectionOverrides::all_off(),
    })
}

/// V19: OpenCode AI-tool tab using whatever provider OpenCode's own config
/// selects (cloud / API keys / project config) when `use_local_provider` is
/// off. TTS prompt injection is enabled by default: OpenCode accepts an
/// instructions file (injected via `OPENCODE_CONFIG_CONTENT`), so it honors
/// the TTS-markup convention and the tab can speak.
pub fn default_opencode_tab() -> TabConfig {
    TabConfig::AiTool(AiToolTabConfig {
        id: OPENCODE_TAB_ID.to_string(),
        builtin: true,
        name: "OpenCode".to_string(),
        command: "opencode".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        // V19: unlike Aider, OpenCode accepts an instructions file (injected
        // via OPENCODE_CONFIG_CONTENT), so the TTS-markup convention applies
        // and the tab can speak. Seeded with the same runtime prompt as Claude.
        tts_injection: TtsInjection { enabled: true },
        notifications: AiNotificationConfig {
            idle: NotificationSlot::enabled("OpenCode is idle"),
            awaiting_permission: NotificationSlot::enabled("OpenCode is awaiting permission"),
            question: NotificationSlot::enabled("OpenCode has a question"),
            error: NotificationSlot::enabled("OpenCode encountered an error"),
        },
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: false,
        // V39 Phase A: a fresh tab accepts the keyboard. The read-only lock
        // is a deliberate user action, never a default.
        read_only: false,
        // V39 Phase B: a fresh tab is nobody's delegation target. Both roles
        // are opt-in, per tab, from that tab's own popover.
        delegation_role: DelegationRole::None,
        delegation_backend: DelegationBackend::default(),
        // V39: a newly created AI tab starts with every tab-scoped injection
        // control explicitly OFF. L1 and every L2 ship on; the per-tab row is
        // the switch the user reaches for, from this tab's shield badge. NOT
        // `Default::default()` — that is all-`Inherit`, which is what an
        // ABSENT cell in an existing settings file must keep meaning (schema
        // step 34 → 35).
        injection_overrides: crate::settings::injection::TabInjectionOverrides::all_off(),
    })
}

/// **TEST-ONLY**: one of the builtin AI tabs with its L3 injection row reset to
/// all-`Inherit`.
///
/// V39 ships a newly created tab with every tab-scoped injection cell `Off`
/// (`injection::TabInjectionOverrides::all_off`), which is the right posture for
/// a real tab and the wrong fixture for a test about the RESOLUTION RULE: a row
/// that already states every cell answers "off, decided at L3" before the rule
/// under test is reached. All-`Inherit` is also a real shape — it is exactly
/// what schema step 34 → 35 writes into every tab that predates V39, i.e. what
/// every upgraded install carries.
///
/// Lives here because `AiToolTabConfig::injection_overrides` is
/// `pub(in crate::settings)`: a test in `tabs::config` or `offload::loopback`
/// cannot reach the field, and the boundary that makes that true is worth more
/// than the convenience of a local fixture.
#[cfg(test)]
pub(crate) fn ai_tab_inheriting_injection(tab: TabConfig) -> TabConfig {
    let mut tab = tab;
    if let TabConfig::AiTool(c) = &mut tab {
        c.injection_overrides = crate::settings::injection::TabInjectionOverrides::default();
    }
    tab
}

/// Look up the default `TabConfig` for a reserved AI tab id. Used by
/// the integrity check and the lifecycle IPC when materializing a tab
/// the user just enabled.
/// **Test fixture** — one AI tab holding the V39 Remote-offload role, i.e. one
/// facade backend.
///
/// Lives beside the tab constructors rather than in a `mod tests`, because
/// three modules' tests need it (the pool, the cap, the child's prose) and a
/// fixture copied three times is three fixtures that can disagree about what a
/// facade tab looks like. Built from the default Claude tab so it carries no
/// harness literal of its own.
#[cfg(test)]
pub(crate) fn facade_tab(id: &str, backend_name: &str) -> TabConfig {
    let mut tab = default_claude_tab();
    if let TabConfig::AiTool(c) = &mut tab {
        c.id = id.to_string();
        c.builtin = false;
        c.name = format!("tab {id}");
        c.delegation_role = DelegationRole::RemoteOffload;
        c.delegation_backend = DelegationBackend {
            name: (!backend_name.is_empty()).then(|| backend_name.to_string()),
            tier: BackendTier::Quality,
            declared_context: None,
        };
    }
    tab
}

pub fn default_ai_tab(id: AiTabId) -> TabConfig {
    match id {
        AiTabId::Claude => default_claude_tab(),
        AiTabId::ClaudeLocal => default_claude_local_tab(),
        AiTabId::OpenCode => default_opencode_tab(),
    }
}

/// V9-01: the reserved, non-closable Code Graph monitor tab. A Shell-kind
/// entry with no command (never PTY-backed — its content is an app-rendered
/// dashboard of the graph indexer/embedder). Materialized/removed by the
/// integrity check per `graph.enabled`.
pub fn default_graph_monitor_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: GRAPH_MONITOR_TAB_ID.to_string(),
        builtin: true,
        name: "Code Intelligence".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// V13 Phase A: the reserved, non-closable Workbench tab. Same shape as the
/// Code Graph monitor tab — Shell-kind with no command (app-rendered, no
/// PTY). Materialized/removed by the integrity check per `workbench.enabled`.
pub fn default_workbench_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: WORKBENCH_TAB_ID.to_string(),
        builtin: true,
        name: "Workbench".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// The reserved, non-closable Tools tab (formerly "Tool Activity" — the
/// rename reaches existing installs via `sync_name`). Same shape as the Code
/// Graph monitor tab — Shell-kind with no command (app-rendered, no PTY).
/// Materialized/removed by the integrity check per `ui.tool_activity_tab`.
pub fn default_tool_activity_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: TOOL_ACTIVITY_TAB_ID.to_string(),
        builtin: true,
        name: "Tools".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// #51: the reserved, non-closable Events tab. Same shape as the Tool Activity
/// tab — Shell-kind with no command (app-rendered, no PTY).
/// Materialized/removed by the integrity check per `ui.events_tab`.
pub fn default_events_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: EVENTS_TAB_ID.to_string(),
        builtin: true,
        name: "Events".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// Default Shell-1 entry. Takes the resolved platform default shell so the
/// `command` and `args` fields land on the right binary for the host. The
/// reserved id is just the seed value for the first shell tab on a fresh
/// install — it's a regular closable shell, not a builtin.
pub fn default_shell_1_tab(default_shell: &ShellSpec) -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: SHELL_DEFAULT_TAB_ID.to_string(),
        builtin: false,
        name: "Shell 1".to_string(),
        command: default_shell.command.to_string_lossy().into_owned(),
        args: default_shell.args.clone(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

// --- Other settings sub-structs (unchanged from v2) -------------------------

/// Where a TTS/STT model runs. `Gpu` prefers the compiled GPU backend and
/// **auto-falls-back to CPU** if no usable GPU is present (so it's a safe
/// default everywhere); `Cpu` forces CPU. Runtime-switchable per feature —
/// changing it reloads only that model, no app restart. On a CPU-only build
/// (no GPU Cargo feature) both values run on CPU. This setting is
/// authoritative: it supersedes the legacy `CIMP_GPU` env var, which is no
/// longer consulted for device selection.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingDevice {
    #[default]
    Gpu,
    Cpu,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TtsSettings {
    /// Master enable for the whole TTS feature. When false, the Kokoro ONNX
    /// model is **unloaded** (freeing CPU/GPU memory) and no synthesis runs;
    /// flipping it back on reloads the model. Distinct from `mute`, which
    /// keeps the model loaded and only silences playback. Back-compat via the
    /// struct-level `#[serde(default)]` — files predating the field load as
    /// `true`.
    pub enabled: bool,
    /// GPU vs CPU for Kokoro synthesis. Changing it live reloads the model on
    /// the newly-selected device (no restart). Additive/back-compat via the
    /// struct-level `#[serde(default)]` — older files load as `Gpu`.
    pub device: ProcessingDevice,
    pub voice: String,
    pub speed: f32,
    pub volume: f32,
    pub mute: bool,
    /// Read-along highlight shown while a Ctrl+right-click selection is being
    /// spoken. Optional/back-compat via `#[serde(default)]`.
    pub selection_highlight: SelectionHighlightSettings,
    /// Show the play / pause / restart / stop transport for selection TTS in
    /// the bottom status bar.
    pub show_selection_controls: bool,
}

impl Default for TtsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            device: ProcessingDevice::Gpu,
            voice: "af_heart".to_string(),
            speed: 1.0,
            volume: 1.0,
            mute: false,
            selection_highlight: SelectionHighlightSettings::default(),
            show_selection_controls: true,
        }
    }
}

/// V6-01 offline speech-to-text (dictation). Captures microphone audio and
/// transcribes it with a bundled Whisper model (whisper.cpp), dropping the
/// transcript into the compose overlay. Enabled by default; the feature still
/// needs a `ggml-*.bin` model present under `models/` to actually transcribe.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct SttSettings {
    /// Master enable for the whole STT feature (record button + PTT).
    pub enabled: bool,
    /// GPU vs CPU for Whisper transcription. Changing it live reloads the
    /// model on the newly-selected device (on the next recording / preload).
    /// Additive/back-compat via the struct-level `#[serde(default)]` — older
    /// files load as `Gpu`.
    pub device: ProcessingDevice,
    /// GGML model filename under `models/` (e.g. "ggml-small.bin").
    pub model_file: String,
    /// Whisper language hint. "auto" = detect; "en", "he", … force a language.
    pub language: String,
    /// cpal input device name; empty = system default input device.
    pub input_device: String,
    /// Bottom-bar record button behavior (click-to-toggle vs press-and-hold).
    pub button_mode: SttButtonMode,
    /// Translate non-English speech to English instead of transcribing
    /// verbatim (Whisper's translate task).
    pub translate_to_english: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SttButtonMode {
    Toggle,
    Hold,
}

impl Default for SttSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            device: ProcessingDevice::Gpu,
            model_file: "ggml-small.bin".to_string(),
            language: "auto".to_string(),
            input_device: String::new(),
            button_mode: SttButtonMode::Toggle,
            translate_to_english: false,
        }
    }
}

/// Colors for the Ctrl+right-click read-along highlight. The whole selection
/// is painted with the `unread_*` colors when a read starts; the sentence
/// currently being spoken uses the `reading_*` accent; each sentence reverts
/// to its original terminal colors as it finishes. xterm decorations only
/// accept `#RRGGBB` hex, so these must be 6-digit hex strings.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct SelectionHighlightSettings {
    /// Master toggle. When false, the gesture still speaks the selection
    /// (chunked, so large/multi-line selections work) but paints no highlight.
    pub enabled: bool,
    /// Foreground/background for not-yet-read sentences. Each `*_custom`
    /// flag chooses between the custom color below (true) and leaving that
    /// channel as the terminal's own palette color (false) — so the user can
    /// tint just the background, just the text, both, or neither.
    pub unread_fg: String,
    pub unread_fg_custom: bool,
    pub unread_bg: String,
    pub unread_bg_custom: bool,
    /// Foreground/background for the sentence currently being spoken.
    pub reading_fg: String,
    pub reading_fg_custom: bool,
    pub reading_bg: String,
    pub reading_bg_custom: bool,
}

impl Default for SelectionHighlightSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            unread_fg: "#000000".to_string(),
            unread_fg_custom: true,
            unread_bg: "#ff5555".to_string(),
            unread_bg_custom: true,
            reading_fg: "#000000".to_string(),
            reading_fg_custom: true,
            reading_bg: "#f1fa8c".to_string(),
            reading_bg_custom: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarSettings {
    pub visible: bool,
    /// Which renderer drives the avatar. `Media` (default) shows the
    /// per-state image/video files in `images`; `Sprite` ignores `images`
    /// and instead plays a manifest-driven frame animation from the sprite
    /// set named in `sprite`. The two are mutually exclusive render paths in
    /// the frontend — see `AvatarOverlay.svelte`.
    pub kind: AvatarKind,
    pub size: AvatarSize,
    pub position: AvatarPosition,
    pub margin: AvatarMargin,
    pub opacity: f32,
    /// Draw the 1px frame (in the waveform color) around the avatar box.
    /// Defaults off — the sprite mascot reads better borderless. Applies to
    /// both render kinds; toggled by the "Show border" checkbox in Settings.
    pub show_border: bool,
    pub images: AvatarImages,
    /// Sprite-renderer configuration. Only consulted when `kind == Sprite`.
    pub sprite: SpriteSettings,
    pub transition: TransitionSettings,
    pub waveform: WaveformSettings,
}

impl Default for AvatarSettings {
    fn default() -> Self {
        // Size/margin/opacity are set as literals here (rather than via the
        // field structs' own `Default`) so the v1.11→v1.12 margin migration —
        // which relies on `AvatarMargin::default()` staying 16/16 for legacy
        // files — is unaffected while fresh installs get the tuned sprite look.
        Self {
            visible: true,
            kind: AvatarKind::default(),
            size: AvatarSize {
                width_px: 140,
                height_px: 140,
            },
            position: AvatarPosition::TopRight,
            margin: AvatarMargin { x_px: 21, y_px: 0 },
            opacity: 0.5,
            show_border: false,
            images: AvatarImages::default(),
            sprite: SpriteSettings::default(),
            transition: TransitionSettings::default(),
            waveform: WaveformSettings::default(),
        }
    }
}

/// Avatar render mode. `Media` is the original image/video-per-state
/// renderer; `Sprite` is the pixel-art frame-animation renderer driven by a
/// `manifest.json` under `<root>/sprites/<set>/`.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AvatarKind {
    Media,
    /// Default render mode: the animated pixel-art mascot. The default set is
    /// `impSprites` (the imp), independent of the active UI theme.
    #[default]
    Sprite,
}

/// Sprite-renderer settings. `set` names a folder under the bundled
/// `<root>/sprites/` tree (served to the WebView at `/sprites/<set>/`) that
/// contains a `manifest.json` plus its frame subfolders. Kept as a plain
/// name (not a path) so new sets can be dropped in alongside `impSprites`
/// (default) and `claudeSprites` without a schema change; the frontend maps
/// the name to a URL.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct SpriteSettings {
    pub set: String,
}

impl Default for SpriteSettings {
    fn default() -> Self {
        Self {
            set: "impSprites".to_string(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarSize {
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for AvatarSize {
    fn default() -> Self {
        Self {
            width_px: 240,
            height_px: 240,
        }
    }
}

/// Per-axis offset from the screen edge defined by `AvatarPosition`. The
/// X component pushes the avatar inward from the left/right edge; the Y
/// component pushes it inward from the top/bottom. Replaces the pre-v1.12
/// scalar `margin_px` field, which applied a single value to both axes.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarMargin {
    pub x_px: u32,
    pub y_px: u32,
}

impl Default for AvatarMargin {
    fn default() -> Self {
        Self { x_px: 16, y_px: 16 }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AvatarPosition {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AvatarImages {
    pub idle: Option<PathBuf>,
    pub listening: Option<PathBuf>,
    pub thinking: Option<PathBuf>,
    pub speaking: Option<PathBuf>,
    pub error: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TransitionSettings {
    pub path: Option<PathBuf>,
    pub duration_ms: u32,
}

impl Default for TransitionSettings {
    fn default() -> Self {
        // Bundled URL — frontend distinguishes vite-served paths (start with
        // `/`) from absolute disk paths picked via the file dialog. Empty
        // path on either side disables transitions.
        Self {
            path: Some(PathBuf::from("/avatar/Transition.mp4")),
            duration_ms: 400,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct WaveformSettings {
    /// Hex color the waveform stroke and the avatar's outline use. Empty
    /// string means "follow the active UI theme" — the frontend resolves
    /// it from the `--waveform-color` CSS variable in `theme.css`. A
    /// non-empty value is a user override that wins regardless of theme.
    /// Render the audio waveform over the avatar. When false the waveform
    /// canvas is hidden entirely (the avatar itself is unaffected). Toggled
    /// by the "Show waveform" checkbox in Settings → Avatar → Waveform.
    /// Defaults off.
    pub visible: bool,
    pub color: String,
    pub line_width: f32,
    pub glow_intensity: f32,
    pub opacity: f32,
}

impl Default for WaveformSettings {
    fn default() -> Self {
        Self {
            visible: false,
            color: String::new(),
            line_width: 2.0,
            glow_intensity: 0.6,
            opacity: 0.85,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct DisplaySettings {
    pub terminal_font_family: String,
    pub terminal_font_size: u32,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            terminal_font_family: "Consolas, Menlo, \"DejaVu Sans Mono\", monospace".to_string(),
            terminal_font_size: 14,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct BehaviorSettings {
    pub auto_speak: bool,
    pub fallback_silent: bool,
    pub announcements_enabled: bool,
    /// When true, `tts.mute` follows `avatar.visible` — hiding the avatar
    /// auto-mutes, showing it auto-unmutes. The frontend handles the sync
    /// (App.svelte settings subscriber); the backend just persists the flag.
    pub follow_avatar: bool,
    /// When true, announcements (idle / awaiting-permission / error / exit)
    /// fire even for the tab the user is currently looking at. Default off
    /// preserves the historical "background-only" behavior — most users
    /// don't want to hear "awaiting permission" for the tab they're
    /// staring at.
    pub announce_focused_tab: bool,
    /// When true, an AI tab's spoken prose plays even when that tab is not
    /// the active one. Default off keeps the v2 behavior — only the
    /// foreground tab's TTS plays. Independent of announcement TTS,
    /// which is gated by `announce_focused_tab` and never dropped for
    /// background tabs.
    pub speak_background_tabs: bool,
    /// When true, text selected in any terminal is copied to the system
    /// clipboard automatically. Older settings files without this field
    /// deserialize to the default via serde-default — no migration bump.
    pub copy_on_select: bool,
    /// When true, a right-click inside any terminal pastes the system
    /// clipboard into the focused PTY (and suppresses the browser's
    /// default context menu). Backward-compatible via serde-default.
    pub paste_on_right_click: bool,
    /// When true, Ctrl+right-click inside any terminal reads the current
    /// selection aloud through TTS instead of pasting. The Ctrl modifier
    /// always suppresses the paste branch when this is on, so the gesture
    /// can never accidentally paste. Backward-compatible via serde-default.
    pub speak_selection_on_right_click: bool,
}

impl Default for BehaviorSettings {
    fn default() -> Self {
        Self {
            auto_speak: true,
            fallback_silent: true,
            announcements_enabled: true,
            follow_avatar: false,
            announce_focused_tab: false,
            speak_background_tabs: false,
            copy_on_select: true,
            paste_on_right_click: true,
            speak_selection_on_right_click: true,
        }
    }
}

/// Bottom-bar Claude Code usage tracker (session 5h + weekly 7d). The
/// per-element flags toggle individual pieces of each window's inline
/// readout; they apply to both windows. `enabled` gates the whole widget.
/// Backward-compatible via serde-default — no migration bump.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct UsageSettings {
    /// Overall on/off for the inline usage widget.
    pub enabled: bool,
    /// Show the proportional fill bar.
    pub show_bar: bool,
    /// Show the rounded utilization percentage.
    pub show_percentage: bool,
    /// Show the live countdown to reset.
    pub show_countdown: bool,
    /// Show the local reset clock time.
    pub show_reset_clock: bool,
    // `show_context` lived here until 2026-08-17. It gated NC-3's live
    // context/cache group, which was removed from the widget (the meter now
    // ends at the reset clock), leaving a toggle with no consumer. An
    // installed settings file may still carry the key; `Settings` does not set
    // `deny_unknown_fields`, so it is ignored on read and gone on the next
    // write. No migration, no schema-version bump — same treatment as
    // `detection_update_classifier_mode` above.
    //
    // Only the widget went: the statusline extractor, the `context_window` slot
    // in the push file, `harness_usage`'s wire shape and the terminal
    // status line all still carry and render the context reading.
    /// How often the frontend re-reads the status-line usage push (a local
    /// file — see `harness::claude::usage`), in seconds. The UI clamps this to a sane
    /// minimum as busy-poll hygiene.
    pub poll_interval_secs: u32,
}

impl Default for UsageSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            show_bar: true,
            show_percentage: true,
            show_countdown: true,
            show_reset_clock: true,
            poll_interval_secs: 60,
        }
    }
}

/// Bottom-bar system-monitor panel (CPU / memory / GPU / network), shown to
/// the right of the Claude usage meter. Backward-compatible via serde-default.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct SystemStatsSettings {
    /// Overall on/off for the system-monitor panel.
    pub enabled: bool,
    /// Poll cadence in seconds (the sparklines tick locally between polls).
    pub poll_interval_secs: u32,
    /// Per-component visibility. `show_gpu_temp` is a sub-toggle of `show_gpu`.
    pub show_cpu: bool,
    pub show_memory: bool,
    pub show_gpu: bool,
    pub show_gpu_temp: bool,
    pub show_network: bool,
}

impl Default for SystemStatsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_secs: 1,
            show_cpu: true,
            show_memory: true,
            show_gpu: true,
            show_gpu_temp: true,
            show_network: true,
        }
    }
}

// `StatuslineSettings` moved to the Claude plugin's declared settings in V40
// Phase B (locked decision 6). It was one `bool` that only Claude's
// `--settings` overlay ever read, so it is the `statusline` `ext` row on
// `Settings::harness["claude"]` now — no core field, no core reader, and a
// harness without a status line declares nothing.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct UiSettings {
    /// Active UI chrome theme. `"tui"` is the built-in ratatui-style theme
    /// (custom title bar, square borders, Gruvbox surfaces) — hardcoded in
    /// the binary and always available; its accent color comes from
    /// `tui_accent` below. Any other value refers to an on-disk theme under
    /// `<exe-dir>/themes/` (`"nippon-dark"` / `"nippon-light"` ship today).
    /// New installs land on `"tui"` (paired with the OpenCode Grey terminal
    /// palette). The avatar still defaults to the animated `impSprites`
    /// mascot independently (see [`AvatarKind`] / [`SpriteSettings`]).
    ///
    /// History: the pre-V1.13 `"tui"` value was rewritten to `"tui-orange"`
    /// by the v12 → v13 migration when the four accent-variant tui themes
    /// shipped; the v27 → v28 migration collapses those four back into the
    /// single `"tui"` id, seeding `tui_accent` with each variant's old
    /// accent so users keep their look.
    pub theme: String,
    /// Accent color of the built-in `"tui"` theme, as a `#rrggbb` hex
    /// string. Injected by the frontend as the `--tui-accent` CSS variable;
    /// the theme derives its whole accent family (hover/bright/soft/muted)
    /// from it. Ignored by on-disk themes — the picker only shows for
    /// `"tui"`. Invalid values fall back to the default blue frontend-side.
    pub tui_accent: String,
    /// Arrangement of the bottom status bar's movable left cluster. See
    /// [`StatusBarLayout`]. Added after the `theme` field; old files
    /// lacking the key deserialize to the default `[usage, system_stats]`
    /// via the struct-level `#[serde(default)]`.
    pub status_bar: StatusBarLayout,
    /// Show the reserved Tool Activity tab (unified graph-call + offload
    /// request feed, plus the tool reference lists). Default true; old files
    /// lacking the key deserialize to true via the struct-level
    /// `#[serde(default)]`. Reconciled like the other reserved feature tabs.
    pub tool_activity_tab: bool,
    /// #51: show the reserved Events tab (the per-tab-attributed, filterable
    /// activity feed). Default true; an existing settings file lacking the key
    /// deserializes to true via the struct-level `#[serde(default)]` and the
    /// integrity check materializes the tab on the next load — which is why
    /// this needs no schema-version migration. Additive: `tool_activity_tab`
    /// is untouched and both tabs coexist.
    pub events_tab: bool,
    /// V32: the color (`#rrggbb`) the containment surfaces wear while a tab
    /// is LATCHED but not contaminated — the tab strip's taint badge and the
    /// frame the pane draws around that tab's content. Rendered and validated
    /// frontend-side only (invalid values fall back to this default there,
    /// like `tui_accent`); the default mirrors the TUI theme's `--warning`,
    /// which is what the badge wore before the color became configurable.
    /// Additive with the struct-level `#[serde(default)]` — no migration.
    pub latched_color: String,
    /// V32: the same surfaces while the tab's conversation is CONTAMINATED —
    /// the stronger state (it outlives the latch). Default mirrors the TUI
    /// theme's `--danger`. Same validation/additive story as `latched_color`.
    pub contaminated_color: String,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: crate::theming::TUI_THEME_ID.to_string(),
            tui_accent: DEFAULT_TUI_ACCENT.to_string(),
            status_bar: StatusBarLayout::default(),
            tool_activity_tab: true,
            events_tab: true,
            latched_color: "#fabd2f".to_string(),
            contaminated_color: "#fb4934".to_string(),
        }
    }
}

/// Default accent for the built-in `tui` theme — the blue the pre-v28
/// `tui-blue` default theme used, so new installs keep the familiar look.
/// Mirrors `DEFAULT_TUI_ACCENT` in `src/lib/themes/accent.ts`.
pub const DEFAULT_TUI_ACCENT: &str = "#7aa2f7";

/// A display panel in the status bar's movable left cluster: `usage` =
/// Claude session meter, `system_stats` = CPU/GPU/network panel.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusBarComponent {
    Usage,
    SystemStats,
}

/// One slot in the movable cluster: a component plus the leading gap (in
/// px) before it. The gap is grown/shrunk by dragging the panel left or
/// right — it "stays where you drop it" — and is reset to 0 for every
/// slot whenever the component order changes.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct StatusBarSlot {
    pub component: StatusBarComponent,
    #[serde(default)]
    pub gap: u32,
}

/// Persisted left-to-right arrangement of the status bar's movable
/// cluster. The frontend normalizes on read so `usage` and
/// `system_stats` each appear exactly once regardless of what's on disk.
/// Reordered and spaced by dragging the panels in the bar; reset from
/// Settings → Bottom bar.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct StatusBarLayout {
    pub items: Vec<StatusBarSlot>,
}

impl Default for StatusBarLayout {
    fn default() -> Self {
        Self {
            items: vec![
                StatusBarSlot {
                    component: StatusBarComponent::Usage,
                    gap: 0,
                },
                StatusBarSlot {
                    component: StatusBarComponent::SystemStats,
                    gap: 0,
                },
            ],
        }
    }
}

/// Terminal-pane settings (V1.4-01+). Holds the xterm.js palette config
/// (V1.4-01) and the background image / solid-color sub-group (V1.4-02).
/// Distinct from `ui`, which themes the cimp chrome.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TerminalSettings {
    pub theme: TerminalThemeSettings,
    /// V1.4-02 background configuration. Image, color, and the
    /// opacity/blur/size/position controls that apply only when an
    /// image is set. See `MILESTONE-V1.4-02-terminal-background.md`
    /// for the four-cell rendering matrix and the three-state
    /// override semantics.
    pub background: TerminalBackgroundSettings,
    /// V1.4-04 D: cross-restart scrollback buffer. PTY output is
    /// captured into a per-tab ring buffer (`ring_bytes` cap, 256 KB
    /// default), persisted to disk on graceful exit, and replayed on
    /// next launch.
    pub scrollback: ScrollbackSettings,
}

/// Terminal palette setting. `name` is either a bundled theme name
/// (e.g., "Default", "Dracula") or "Custom" — in which case `custom`
/// carries the user's chosen 22-color override map.
///
/// The `custom` block is kept untyped on the Rust side because its
/// values are pure data the frontend forwards to xterm.js. Missing or
/// malformed keys are tolerated by the resolver, which merges the
/// custom map over the bundled "Default" theme.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TerminalThemeSettings {
    pub name: String,
    pub custom: Option<HashMap<String, String>>,
}

impl Default for TerminalThemeSettings {
    fn default() -> Self {
        Self {
            // Paired with the default built-in `tui` UI theme (whose metadata
            // points at the OpenCode Grey palette). The frontend's theme picker
            // re-pairs this when the user switches UI theme.
            name: "OpenCode Grey".to_string(),
            custom: None,
        }
    }
}

/// V1.4-02 terminal background configuration. `image` and `color` are
/// independent (sibling fields, not a discriminated union) — both can be
/// `Some` simultaneously, in which case `color` becomes the dimming-overlay
/// tint atop `image`. `opacity`, `blur`, `size`, and `position` apply only
/// when `image` is `Some`; when only `color` is set, the resolver
/// rewrites the theme background and the renderer stays on the canvas
/// fast path with no transparency cost.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TerminalBackgroundSettings {
    /// Absolute path to the background image. `None` means "no image".
    /// Invalid paths surface a settings error and resolve to the
    /// no-image rendering path; the `color` field, if set, still applies.
    pub image: Option<PathBuf>,
    /// Hex color (e.g., `"#1a2b3c"`). When `image` is `None`, this
    /// replaces the resolved theme's background field. When `image` is
    /// `Some`, this drives the dimming-overlay tint (default black if
    /// `None`).
    pub color: Option<String>,
    /// Image-mode dimming-overlay alpha. 0.0-1.0. Ignored when `image`
    /// is `None`.
    pub opacity: f32,
    /// Image-mode CSS `backdrop-filter: blur(...)` radius in pixels.
    /// Ignored when `image` is `None`.
    pub blur: u32,
    /// Image-mode CSS `background-size` strategy. Ignored when `image`
    /// is `None`.
    pub size: BackgroundSize,
    /// Image-mode CSS `background-position` value. Ignored when `image`
    /// is `None`.
    pub position: String,
    /// V1.4-04 A.1 snapshot cap. Number of scrollback rows to capture
    /// when `serializeAddon.serialize({ scrollback })` runs on a
    /// renderer-category flip. Bounds JS-heap allocation under
    /// long-scrollback (50k+ lines) edge cases. Existing v1.5 files
    /// without this field deserialize to the default via serde-default
    /// — no migration version bump.
    pub snapshot_lines: u32,
    /// V1.4-04 B: named presets the user has saved. The recursion is
    /// blocked by the sister-struct `BackgroundPresetConfig` (presets
    /// can't contain presets). Migration v1.5 → v1.6 stamps `[]`; older
    /// files deserialize to `[]` via serde-default. When a per-tab
    /// `BackgroundOverride::Custom(...)` round-trips, this field rides
    /// along as `[]` — that's harmless wire-format growth and avoided
    /// the wire-format break that switching `Custom` to wrap
    /// `BackgroundPresetConfig` would have caused.
    pub presets: Vec<BackgroundPreset>,
    /// V1.4-04 C.4: when `false`, per-tab Configure dialog edits that
    /// would flip renderer category (image ↔ no-image) are deferred to
    /// Save. In-place changes (color / opacity / blur / size /
    /// position / tint) preview live regardless. Default `true`. Older
    /// files deserialize to `true` via serde-default; the v1.6 → v1.7
    /// migration (Phase D) stamps it explicitly.
    pub preview_category_flips: bool,
}

impl Default for TerminalBackgroundSettings {
    fn default() -> Self {
        Self {
            image: None,
            color: None,
            opacity: 0.4,
            blur: 0,
            size: BackgroundSize::Cover,
            position: "center".to_string(),
            snapshot_lines: 2000,
            presets: Vec::new(),
            preview_category_flips: true,
        }
    }
}

/// V1.4-04 B: a saved preset. `name` is the user-facing label;
/// `config` carries the same fields as `TerminalBackgroundSettings`
/// minus the `presets` field itself (the sister-struct
/// `BackgroundPresetConfig` makes the "presets don't contain presets"
/// invariant structural rather than runtime-enforced).
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct BackgroundPreset {
    pub name: String,
    pub config: BackgroundPresetConfig,
}

/// V1.4-04 B: the payload of a `BackgroundPreset` — same fields as
/// `TerminalBackgroundSettings` except for the recursive `presets`
/// field. `From`/`Into` impls bridge the two so the editor UI can hand
/// either shape into `composeTheme` etc. The `BackgroundOverride::Custom`
/// variant deliberately stays wrapped around `TerminalBackgroundSettings`
/// rather than this struct — see the doc note on
/// `TerminalBackgroundSettings.presets`.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct BackgroundPresetConfig {
    pub image: Option<PathBuf>,
    pub color: Option<String>,
    pub opacity: f32,
    pub blur: u32,
    pub size: BackgroundSize,
    pub position: String,
    pub snapshot_lines: u32,
}

impl Default for BackgroundPresetConfig {
    fn default() -> Self {
        Self {
            image: None,
            color: None,
            opacity: 0.4,
            blur: 0,
            size: BackgroundSize::Cover,
            position: "center".to_string(),
            snapshot_lines: 2000,
        }
    }
}

impl From<&TerminalBackgroundSettings> for BackgroundPresetConfig {
    fn from(s: &TerminalBackgroundSettings) -> Self {
        Self {
            image: s.image.clone(),
            color: s.color.clone(),
            opacity: s.opacity,
            blur: s.blur,
            size: s.size,
            position: s.position.clone(),
            snapshot_lines: s.snapshot_lines,
        }
    }
}

impl From<BackgroundPresetConfig> for TerminalBackgroundSettings {
    fn from(p: BackgroundPresetConfig) -> Self {
        Self {
            image: p.image,
            color: p.color,
            opacity: p.opacity,
            blur: p.blur,
            size: p.size,
            position: p.position,
            snapshot_lines: p.snapshot_lines,
            presets: Vec::new(),
            // V1.4-04 C.4: a preset doesn't carry the dialog
            // preview-opt-out flag (it's a global UI behavior, not a
            // background-config attribute). Lifting a preset back into
            // a `TerminalBackgroundSettings` defaults to the same
            // value as `Default`.
            preview_category_flips: true,
        }
    }
}

/// CSS `background-size` strategy. `Tile` is mapped to
/// `background-repeat: repeat` + `background-size: auto` on the
/// frontend; `Cover` and `Contain` map directly to their CSS values.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundSize {
    #[default]
    Cover,
    Contain,
    Tile,
}

/// V1.4-02 three-state per-tab override. Encodes:
///   - `None` (the field is `null` on disk, or the override variant is
///     `None`): inherit the global `terminal.background`.
///   - `Some(BackgroundOverride::Disabled)` (`"disabled"` on disk):
///     opt out of any background — render with theme bg only,
///     ignoring both global image and global color.
///   - `Some(BackgroundOverride::Custom(cfg))` (full object on disk):
///     this tab's background config replaces the global one wholesale.
///
/// Custom (de)serialize because `serde(untagged)` can't express the
/// literal-string `"disabled"` cleanly alongside an object variant.
#[derive(Clone, Debug)]
pub enum BackgroundOverride {
    Disabled,
    Custom(TerminalBackgroundSettings),
}

impl Serialize for BackgroundOverride {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Disabled => s.serialize_str("disabled"),
            Self::Custom(c) => c.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for BackgroundOverride {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(ref s) if s == "disabled" => Ok(Self::Disabled),
            serde_json::Value::Object(_) => serde_json::from_value(v)
                .map(Self::Custom)
                .map_err(D::Error::custom),
            _ => Err(D::Error::custom(
                "background_override: expected \"disabled\" string or background config object",
            )),
        }
    }
}

/// V1.4-04 D: cross-restart scrollback configuration. The ring
/// buffer is per-tab and capped at `ring_bytes`. `persist` toggles
/// disk persistence on graceful exit (`tauri::RunEvent::ExitRequested`);
/// `restore_on_launch` toggles the read-back at next `pty_start`.
/// Both default `true`. Defaults match the milestone doc — 256 KB per
/// tab is roughly 600 lines of dense ANSI, enough for "what was I
/// doing yesterday" continuity without ballooning disk usage.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ScrollbackSettings {
    pub ring_bytes: usize,
    pub persist: bool,
    pub restore_on_launch: bool,
}

impl Default for ScrollbackSettings {
    fn default() -> Self {
        Self {
            ring_bytes: 262_144,
            persist: true,
            restore_on_launch: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ComposeSettings {
    pub min_height_px: u32,
    pub max_height_px: u32,
}

impl Default for ComposeSettings {
    fn default() -> Self {
        Self {
            min_height_px: 80,
            max_height_px: 300,
        }
    }
}

/// V14 Phase A: one saved prompt-library entry. `body` may contain the two
/// immediately-resolvable variables `{selection}` / `{clipboard}` (substituted
/// by the frontend on insert — see `lib/compose/templates.ts`) plus any
/// number of free-form `{name}` placeholders, which stay literal as tab-stops
/// the user fills in. Lives at `Settings::prompt_templates` (global scope);
/// project-scope entries of the same shape live in the `.cimp/config.json`
/// overlay's own `prompt_templates` array, read directly by the
/// `compose_templates` resolver IPC rather than through the normal
/// deep-merged `Settings` (see `settings::persistence::read_project_prompt_templates`
/// — the merge would otherwise wholesale-replace the global list instead of
/// shadowing it by name).
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PromptTemplate {
    pub name: String,
    pub body: String,
}

/// The 4 starter templates seeded into the global list exactly once (guarded
/// by `Settings::templates_seeded`, not `CURRENT_SCHEMA_VERSION` — a user who
/// deletes all 4 must not have them reappear on a later migration). Clearly
/// example-flavored bodies; deletable like anything else in the list.
pub fn starter_prompt_templates() -> Vec<PromptTemplate> {
    vec![
        PromptTemplate {
            name: "review-this-diff".to_string(),
            body: "Review the following diff for correctness, style, and missed edge cases:\n\n{selection}".to_string(),
        },
        PromptTemplate {
            name: "write-tests-for".to_string(),
            body: "Write tests covering {selection}. Include the obvious edge cases.".to_string(),
        },
        PromptTemplate {
            name: "explain-selection".to_string(),
            body: "Explain what this does and why:\n\n{selection}".to_string(),
        },
        PromptTemplate {
            name: "commit-message".to_string(),
            body: "Write a concise, conventional commit message for:\n\n{selection}".to_string(),
        },
    ]
}

/// A template resolved by name across the two scopes — what the compose
/// overlay's `/` picker actually renders. `scope` is `"global"` or
/// `"project"`, surfaced in the UI so a shadowed global entry can be shown
/// greyed with a note.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct ResolvedTemplate {
    pub name: String,
    pub body: String,
    pub scope: String,
}

/// Merge global + project template lists by name: a project entry shadows a
/// same-named global one (matching every other overlay's precedence rule),
/// and any project-only entry is appended after the (filtered) global list.
/// Pure and file-I/O-free so the shadowing rule is unit-testable without
/// touching disk; `ipc::commands::compose_templates` is the thin I/O wrapper
/// that feeds it the two raw lists (see the `PromptTemplate` doc comment for
/// why those are read directly rather than through the merged `Settings`).
pub fn resolve_prompt_templates(
    global: Vec<PromptTemplate>,
    project: Vec<PromptTemplate>,
) -> Vec<ResolvedTemplate> {
    let project_names: std::collections::HashSet<&str> =
        project.iter().map(|t| t.name.as_str()).collect();
    let mut out: Vec<ResolvedTemplate> = global
        .into_iter()
        .filter(|t| !project_names.contains(t.name.as_str()))
        .map(|t| ResolvedTemplate {
            name: t.name,
            body: t.body,
            scope: "global".to_string(),
        })
        .collect();
    out.extend(project.into_iter().map(|t| ResolvedTemplate {
        name: t.name,
        body: t.body,
        scope: "project".to_string(),
    }));
    out
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ShortcutSettings {
    pub open_compose: Option<String>,
    /// V14 Phase A: open compose (like `open_compose`) AND immediately open
    /// the prompt-template picker popover — the discoverable keyboard path
    /// alongside the 📋 button beside the compose textarea. Default
    /// `Alt+/` (mirrors `open_compose`'s `Alt+Enter` and the picker's own
    /// `/` trigger); NOT a `Ctrl+Shift+…` chord — see the V6-01 note above
    /// on `push_to_talk`'s bare-`Ctrl+Shift` collision.
    pub open_compose_picker: Option<String>,
    pub submit_compose: Option<String>,
    pub cancel_compose: Option<String>,
    pub open_settings: Option<String>,
    pub switch_to_tab_1: Option<String>,
    pub switch_to_tab_2: Option<String>,
    pub switch_to_tab_3: Option<String>,
    pub switch_to_tab_4: Option<String>,
    pub switch_to_tab_5: Option<String>,
    pub switch_to_tab_6: Option<String>,
    pub switch_to_tab_7: Option<String>,
    pub switch_to_tab_8: Option<String>,
    pub switch_to_tab_9: Option<String>,
    /// Open the New Shell Tab dialog. Identical to clicking the `+`
    /// button on the tab bar.
    pub new_shell_tab: Option<String>,
    /// Close the active tab. No-op (with a transient toast) on builtins.
    pub close_tab: Option<String>,
    /// V4-03 pane shortcuts. Optional so v1.2 settings files round-trip
    /// without migration; the frontend defaults supply working bindings
    /// when these are missing.
    pub focus_pane_left: Option<String>,
    pub focus_pane_right: Option<String>,
    pub focus_pane_up: Option<String>,
    pub focus_pane_down: Option<String>,
    pub split_pane_horizontal: Option<String>,
    pub split_pane_vertical: Option<String>,
    pub close_pane: Option<String>,
    /// V6-01 push-to-talk (hold) dictation trigger. Default bare
    /// `Ctrl+Shift` (modifiers-only) — held to record, released to
    /// transcribe. The dispatcher's arm/debounce + abort-on-other-key
    /// state machine keeps this from firing on ordinary `Ctrl+Shift+<key>`
    /// chords. Optional so older settings files round-trip; the default
    /// supplies the binding when the key is absent.
    pub push_to_talk: Option<String>,
    /// Read the active terminal's current selection aloud through TTS —
    /// the keyboard equivalent of the Ctrl+right-click gesture. Toasts
    /// "No text selected" when nothing is selected. Optional so older
    /// settings files round-trip; the default supplies the binding.
    pub speak_selection: Option<String>,
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            // V6-01: open_compose, split_pane_vertical, and close_pane were
            // moved off `Ctrl+Shift+…` so the bare-`Ctrl+Shift` push-to-talk
            // chord doesn't visibly arm/abort when these fire. New installs
            // get these defaults; existing settings files keep their bindings.
            open_compose: Some("Alt+Enter".to_string()),
            open_compose_picker: Some("Alt+/".to_string()),
            // V6-01: Enter submits (one-handed send for dictation); Alt+Enter
            // (and Shift+Enter) insert a newline — see ComposeOverlay.svelte.
            submit_compose: Some("Enter".to_string()),
            cancel_compose: Some("Escape".to_string()),
            open_settings: Some("Ctrl+,".to_string()),
            switch_to_tab_1: Some("Ctrl+1".to_string()),
            switch_to_tab_2: Some("Ctrl+2".to_string()),
            switch_to_tab_3: Some("Ctrl+3".to_string()),
            switch_to_tab_4: Some("Ctrl+4".to_string()),
            switch_to_tab_5: Some("Ctrl+5".to_string()),
            switch_to_tab_6: Some("Ctrl+6".to_string()),
            switch_to_tab_7: Some("Ctrl+7".to_string()),
            switch_to_tab_8: Some("Ctrl+8".to_string()),
            switch_to_tab_9: Some("Ctrl+9".to_string()),
            new_shell_tab: Some("Ctrl+T".to_string()),
            close_tab: Some("Ctrl+W".to_string()),
            focus_pane_left: Some("Ctrl+Alt+Left".to_string()),
            focus_pane_right: Some("Ctrl+Alt+Right".to_string()),
            focus_pane_up: Some("Ctrl+Alt+Up".to_string()),
            focus_pane_down: Some("Ctrl+Alt+Down".to_string()),
            split_pane_horizontal: Some("Ctrl+\\".to_string()),
            split_pane_vertical: Some("Alt+\\".to_string()),
            close_pane: Some("Ctrl+Alt+W".to_string()),
            push_to_talk: Some("Ctrl+Shift".to_string()),
            speak_selection: Some("Ctrl+Alt+S".to_string()),
        }
    }
}

/// Per-tab gate for whether cImp speaks this AI tab's assistant prose. V20
/// retired the `[[TTS]]` markup convention (out-of-band adapters now read the
/// tool's structured transcript/event stream and speak all prose), so this is
/// a plain on/off toggle — the former free-text `instructions` field is gone.
/// Kept as a struct (not a bare bool) so the serialized shape stays an object
/// and every historical settings file / migration that carries
/// `tts_injection` as `{ "enabled": … }` round-trips without a schema bump; an
/// old file's leftover `instructions` key is simply ignored on load.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TtsInjection {
    pub enabled: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ProcessingSettings {
    pub stability_timeout_ms: u32,
    pub max_hold_ms: u32,
}

impl Default for ProcessingSettings {
    fn default() -> Self {
        Self {
            stability_timeout_ms: 200,
            max_hold_ms: 500,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    // ── V39 Phase C — the synthesized facade backends ───────────────────────

    /// A pool with one configured Local backend, so the *configured* branch of
    /// `effective_backends` is the one under test.
    fn with_configured_local(name: &str) -> Settings {
        let mut s = Settings::default();
        s.offload.backends = vec![OffloadBackend {
            name: name.to_string(),
            kind: OffloadBackendKind::Local {
                server_command: "llama-server --jinja -np 2".to_string(),
                autostart: false,
                show_command_on_start: false,
                auth_token: String::new(),
            },
            ..Default::default()
        }];
        s
    }

    fn pool_names(pool: &[OffloadBackend]) -> Vec<String> {
        pool.iter().map(|b| b.name.clone()).collect()
    }

    /// **A Remote-offload tab is a backend, in both branches** (locked decision
    /// 8: "synthesized from the tab role — there is no separate add-backend
    /// step").
    ///
    /// Both branches, because the legacy one is the whole pool of a user who
    /// runs no llama-server: a facade that only appeared beside a configured
    /// HTTP backend would be a feature you had to own a GPU to use.
    #[test]
    fn a_remote_offload_tab_is_synthesized_into_both_branches_of_the_pool() {
        // Configured branch.
        let mut s = with_configured_local("main");
        s.tabs.push(facade_tab("t1", "lan-worker-2"));
        assert_eq!(
            pool_names(&s.effective_offload_backends()),
            vec!["main", "lan-worker-2"]
        );

        // Legacy branch: no `backends` at all, so `effective_backends`
        // synthesizes its single `local` entry — and the facade still joins it.
        let mut legacy = Settings::default();
        assert!(
            legacy.offload.backends.is_empty(),
            "the fixture must be legacy"
        );
        legacy.tabs.push(facade_tab("t1", "lan-worker-2"));
        assert_eq!(
            pool_names(&legacy.effective_offload_backends()),
            vec!["local", "lan-worker-2"]
        );
    }

    /// The synthesized entry carries the tab's knobs — and `ToolScope::All`,
    /// which is both the honest value (cImp does not filter another harness's
    /// tools) and the one that keeps the facade indistinguishable from a
    /// trusted LAN backend in the tool prose.
    #[test]
    fn the_synthesized_facade_carries_the_tabs_knobs() {
        let mut s = with_configured_local("main");
        let mut tab = facade_tab("t1", "lan-worker-2");
        if let TabConfig::AiTool(c) = &mut tab {
            c.delegation_backend.tier = BackendTier::Fast;
            c.delegation_backend.declared_context = Some(128_000);
        }
        s.tabs.push(tab);
        let pool = s.effective_offload_backends();
        let b = pool.iter().find(|b| b.name == "lan-worker-2").expect("facade");
        assert!(matches!(&b.kind, OffloadBackendKind::HarnessTab { tab } if tab == "t1"));
        assert_eq!(b.tier, BackendTier::Fast);
        assert_eq!(b.declared_context, Some(128_000));
        assert!(
            b.enabled,
            "a facade is enabled by its role, not by a second switch"
        );
        assert!(matches!(b.tool_scope, ToolScope::All));
        assert!(
            !b.cloud_blocked(),
            "nothing about a facade needs cloud consent"
        );
        assert!(
            b.kind.effective_auth_token().is_empty(),
            "a PTY carries no credential"
        );
    }

    /// **The backend name defaults to a name that says nothing** (V39 review
    /// L-2). A blank name is the same as none: a cleared text field writes
    /// `""`, not `null`.
    ///
    /// It used to default to the TAB's display name, which
    /// `offload::mcp::backend_label` renders into `offload_task`'s description
    /// — so a tab called "Claude — API work" told the asking model what its
    /// "LAN backend" really was, which is locked decision 3 failing open on the
    /// path most users will never touch.
    #[test]
    fn a_facade_without_a_chosen_name_answers_to_a_name_that_names_no_tab() {
        let mut s = with_configured_local("main");
        s.tabs.push(facade_tab("t1", ""));
        let mut blank = facade_tab("t2", "");
        if let TabConfig::AiTool(c) = &mut blank {
            c.delegation_backend.name = Some("   ".to_string());
        }
        s.tabs.push(blank);
        assert_eq!(
            pool_names(&s.effective_offload_backends()),
            vec!["main", "worker-0844", "worker-0744"]
        );
        // The property, not just the value: nothing in a default name comes
        // from the tab's own words or from a harness id.
        for name in pool_names(&s.effective_offload_backends()) {
            for leak in ["tab t1", "tab t2", "claude", "opencode", "Claude"] {
                assert!(
                    !name.contains(leak),
                    "the default backend name leaked `{leak}`: {name}"
                );
            }
        }
    }

    /// **The default name is stable, opaque, and the frontend can reproduce
    /// it** (V39 review L-2).
    ///
    /// The literal values are pinned because `src/lib/delegation.ts`'s
    /// `defaultFacadeName` has to answer identically — the popover's
    /// placeholder and the Settings list render it before any backend call —
    /// and the vitest suite asserts the same three strings.
    #[test]
    fn the_default_facade_name_is_a_stable_hash_of_the_tab_id() {
        assert_eq!(facade_default_name("t1"), "worker-0844");
        assert_eq!(facade_default_name("t2"), "worker-0744");
        assert_eq!(facade_default_name("ai-9f3c"), "worker-f0cb");
        // Stable across calls, distinct across ids, and shaped like a name.
        assert_eq!(facade_default_name("t1"), facade_default_name("t1"));
        assert_ne!(facade_default_name("t1"), facade_default_name("t2"));
        assert!(facade_default_name("t1").starts_with("worker-"));
        assert_eq!(facade_default_name("t1").len(), "worker-".len() + 4);
    }

    /// **A name collision: the configured backend wins, the facade is dropped.**
    ///
    /// Not renamed and not appended: the router, the run log and the dashboard
    /// all key on the name, so two entries answering to one name has no good
    /// half. The user is warned once (see `warn_backend_name_collision`).
    #[test]
    fn a_configured_backend_wins_a_name_collision_with_a_facade() {
        let mut s = with_configured_local("main");
        s.tabs.push(facade_tab("t1", "main"));
        let pool = s.effective_offload_backends();
        assert_eq!(pool_names(&pool), vec!["main"]);
        assert!(
            matches!(pool[0].kind, OffloadBackendKind::Local { .. }),
            "the surviving `main` must be the configured one"
        );
    }

    /// **Nothing synthesized is ever persisted** (the kind's "never written by
    /// the user").
    ///
    /// The backend editor edits `offload.backends`, so the load-bearing claim is
    /// that the pool view and the persisted list are different values — asking
    /// for the pool must not mutate what a save then writes. Serializing the
    /// whole file and looking for the tag is the round-trip half: a
    /// `harness_tab` in `settings.json` would be a backend nobody can delete
    /// from the editor and that resurrects itself on every load.
    #[test]
    fn a_synthesized_facade_never_reaches_the_persisted_backend_list() {
        let mut s = with_configured_local("main");
        s.tabs.push(facade_tab("t1", "lan-worker-2"));
        assert_eq!(s.effective_offload_backends().len(), 2, "in the POOL");
        assert_eq!(
            pool_names(&s.offload.backends),
            vec!["main"],
            "…and not in the persisted list"
        );
        let json = serde_json::to_string(&s).expect("settings serialize");
        assert!(
            !json.contains("harness_tab"),
            "a save round-trip must not write a synthesized backend"
        );
    }

    /// A tab with any other role contributes no backend — including `Manual`,
    /// which is the *other* delegation role and must not double as a facade
    /// (locked decision 8: the roles are one enum, not two flags).
    #[test]
    fn only_the_remote_offload_role_synthesizes_a_backend() {
        for role in [DelegationRole::None, DelegationRole::Manual] {
            let mut s = with_configured_local("main");
            let mut tab = facade_tab("t1", "lan-worker-2");
            if let TabConfig::AiTool(c) = &mut tab {
                c.delegation_role = role;
            }
            s.tabs.push(tab);
            assert_eq!(
                pool_names(&s.effective_offload_backends()),
                vec!["main"],
                "a {role:?} tab is not an offload backend"
            );
        }
    }


    /// **The V32 F-19 trap, on the two spike fields `HarnessVersions` still
    /// holds.** A global `settings.json` written before V40 Phase B carries
    /// five more keys in this block; they are `Settings::harness` rows now (the
    /// 35 -> 36 migration moves them) and this struct must load the old shape
    /// without them without failing — a `HarnessVersions` that refused to parse
    /// would quarantine the whole settings file over two fields the migration
    /// has already emptied.
    ///
    /// F-19's lesson was that the container-level `#[serde(default)]` silently
    /// fills a missing field, so an additive field whose *correct* pre-upgrade
    /// value is NOT its `Default` ships broken and looks fine. Here the two
    /// survivors are exactly the ones whose value must NOT change, and both are
    /// pinned.
    #[test]
    fn harness_versions_loads_a_pre_phase_b_file_and_keeps_the_two_spike_outcomes() {
        let old_shape = json!({
            "claude_last_seen": "2.1.232",
            "claude_last_verified": "2.1.14",
            "opencode_last_seen": "1.18.13",
            "input_profile_status": "pass",
            "claude_auto_verify": null,
            "e1_status": "pass",
            "d0_status": "unverified"
        });
        let hv: HarnessVersions =
            serde_json::from_value(old_shape.clone()).expect("the pre-Phase-B shape still loads");
        assert_eq!(hv.e1_status, "pass");
        assert_eq!(hv.d0_status, "unverified");

        // …and through the whole `Settings` round trip, which is the shape
        // `settings::persistence` actually reads.
        let mut file = json!({ "schema_version": CURRENT_SCHEMA_VERSION });
        file["harness_versions"] = old_shape;
        let s: Settings = serde_json::from_value(file).expect("whole-settings round trip");
        // The E1 spike outcome is the field a lost record would silently
        // un-gate a feature through.
        assert_eq!(s.harness_versions.e1_status, "pass");
        assert_eq!(s.harness_versions, HarnessVersions {
            e1_status: "pass".to_string(),
            d0_status: "unverified".to_string(),
        });
    }

    /// **The per-harness map is what an absent key means** (V40 locked
    /// decision 5).
    ///
    /// The whole promise of the map is that a harness registered LATER needs no
    /// migration: its row is absent from every file ever written, and every
    /// read has to answer the declared defaults anyway. This drives that
    /// directly — a `Settings` with an EMPTY `harness` block, which is the
    /// state a hand-edit or a future harness produces.
    #[test]
    fn an_absent_harness_row_reads_its_declared_defaults() {
        let mut file = json!({ "schema_version": CURRENT_SCHEMA_VERSION });
        file["harness"] = json!({});
        let s: Settings = serde_json::from_value(file).expect("round trip");
        assert!(s.harness.is_empty(), "the fixture is the absent case");

        for h in crate::harness::registry::all() {
            let row = s.harness_settings(h);
            assert!(row.expose_commands, "{h}: expose_commands defaults on");
            assert!(row.expose_code_audit, "{h}: expose_code_audit defaults on");
            assert_eq!(row.last_seen, "", "{h}: nothing observed yet");
            assert_eq!(
                row.input_profile_status, SPIKE_UNVERIFIED,
                "{h}: an unrun spike is `unverified`, never `fail`"
            );
            // Every DECLARED ext field answers its declared default, whether or
            // not the file carries it — the property that makes a plugin's new
            // setting cost one table row and no migration.
            for field in h.plugin().expect("registered").settings_schema() {
                assert_eq!(
                    s.harness_ext(h, field.key),
                    field.default.to_json(),
                    "{h}.ext.{} must read its declared default when absent",
                    field.key
                );
            }
        }
    }

    /// **A row for a harness this build does not know survives a load/save.**
    ///
    /// Downgrade safety, and it is not hypothetical: V41 adds Codex, and a user
    /// who opens an older cImp once must not come back to a wiped `codex`
    /// block. The map is keyed by `String` for exactly this, and
    /// `HarnessSettings::unknown` catches the fields inside the row that this
    /// build has no name for.
    #[test]
    fn an_unregistered_harness_row_round_trips_untouched() {
        let mut file = json!({ "schema_version": CURRENT_SCHEMA_VERSION });
        file["harness"] = json!({
            "codex": {
                "expose_commands": false,
                "last_seen": "0.9.1",
                "input_profile_status": "pass",
                "ext": { "sandbox_mode": "workspace-write" },
                "a_field_this_build_never_heard_of": 7
            }
        });
        let mut s: Settings = serde_json::from_value(file).expect("round trip");
        // The parse boundary must not delete it either.
        s.normalize_harness_settings();

        let back = serde_json::to_value(&s).expect("re-serialize");
        let row = &back["harness"]["codex"];
        assert_eq!(row["expose_commands"], json!(false));
        assert_eq!(row["last_seen"], json!("0.9.1"));
        assert_eq!(row["input_profile_status"], json!("pass"));
        assert_eq!(row["ext"]["sandbox_mode"], json!("workspace-write"));
        assert_eq!(
            row["a_field_this_build_never_heard_of"],
            json!(7),
            "a field outside the core set must ride through, or a downgrade is \
             a data-loss operation"
        );
    }

    /// **The parse boundary enforces the declared kinds** (global principle 4).
    ///
    /// A declared schema is a claim about a file the user can hand-edit. A
    /// `"statusline": "yes"` would otherwise reach the launch path as a string
    /// that every boolean reader answers `false` for — a control the file says
    /// is ON, silently off. An UNDECLARED key is left alone on purpose: a key a
    /// newer cImp declares must survive a downgrade.
    #[test]
    fn the_parse_boundary_resets_a_declared_ext_key_of_the_wrong_kind() {
        let claude = crate::harness::HarnessId::from_id("claude").expect("registered");
        let statusline = crate::harness::claude::settings::STATUSLINE;

        let mut s = Settings::default();
        s.set_ext("claude", statusline, json!("yes"));
        s.set_ext("claude", "a.key.nobody.declares", json!({ "kept": true }));
        assert!(s.normalize_harness_settings(), "the bad value is a change");

        assert_eq!(
            s.harness_ext(claude, statusline),
            json!(true),
            "a value its declared kind rejects is reset to the declared default"
        );
        assert_eq!(
            s.harness_ext(claude, "a.key.nobody.declares"),
            json!({ "kept": true }),
            "an UNDECLARED key is not core's to delete"
        );
    }

    /// A `secret` ext value never reaches a log line through `Debug`.
    ///
    /// The defense-in-depth `ClaudeLocalSettings`'s hand-rolled `Debug` carried
    /// before its three fields became `ext` rows — now driven by the `secret`
    /// column, so it covers every plugin at once instead of one struct.
    #[test]
    fn a_secret_ext_value_is_redacted_in_debug() {
        let mut s = Settings::default();
        s.set_ext(
            "claude",
            crate::harness::claude::settings::LOCAL_AUTH_TOKEN,
            json!("sk-super-secret"),
        );
        let shown = format!("{:?}", s.harness_row_of("claude"));
        assert!(
            !shown.contains("sk-super-secret"),
            "an auth token must not reach the rolling log: {shown}"
        );
        assert!(shown.contains("<redacted>"), "…and it must say so: {shown}");
    }

    /// **#48 — the G-1 defect class, on the enum-ish STRING settings.**
    ///
    /// `00b906b` fixed `injection::Override`, whose guard test passed only
    /// strings and so stayed green while every non-string shape quarantined the
    /// file. The same shape was unfixed on four plain-`String` fields with
    /// closed vocabularies and post-hoc parses. This drives whole `Settings`
    /// round trips — the shape `settings::persistence` actually deserializes,
    /// where a failure means `quarantine_corrupt_file` and seeded defaults for
    /// themes, tabs, backends, checks, MCP servers and pricing.
    ///
    /// The asserted answer is the field's fallback for an UNRECOGNIZED STRING,
    /// spelled canonically — the rule these deserializers implement — not
    /// necessarily its shipped default (`detection_update_rules_mode` ships
    /// `auto` and falls back to `check`).
    #[test]
    fn a_non_string_enum_setting_reads_as_its_documented_fallback() {
        // (JSON pointer into `offload`/`graph`, canonical fallback).
        let cases: &[(&str, &str, &str)] = &[
            ("offload", "native_web_visibility", "sensor"),
            ("offload", "detection_update_rules_mode", "check"),
            ("graph", "read_advisor_mode", "advise"),
        ];
        let bogus = [
            json!(true),
            json!(false),
            json!(null),
            json!(1),
            json!(0),
            json!(-1),
            json!(0.5),
            json!([]),
            json!(["sensor"]),
            json!({}),
            json!({ "value": "sensor" }),
        ];
        for (group, field, fallback) in cases {
            for junk in &bogus {
                let s = Settings::default();
                let mut v = serde_json::to_value(&s).expect("settings serialize");
                v[group][field] = junk.clone();
                let back: Settings = serde_json::from_value(v).unwrap_or_else(|e| {
                    panic!("{group}.{field} = {junk} quarantines the settings file: {e}")
                });
                let got = serde_json::to_value(&back).expect("re-serialize");
                assert_eq!(
                    got[group][field].as_str(),
                    Some(*fallback),
                    "{group}.{field} = {junk}"
                );
                // …and the rest of the file survived, which is the finding.
                assert_eq!(back.tabs.len(), s.tabs.len(), "{group}.{field} = {junk}");
                assert_eq!(
                    back.schema_version, s.schema_version,
                    "{group}.{field} = {junk}"
                );
            }
            // A recognized string is still taken verbatim — the leniency must
            // not swallow real values.
            let s = Settings::default();
            let mut v = serde_json::to_value(&s).expect("settings serialize");
            v[group][field] = json!("substitute-or-deny-placeholder");
            let back: Settings = serde_json::from_value(v).expect("a string always parses");
            let got = serde_json::to_value(&back).expect("re-serialize");
            assert_eq!(
                got[group][field].as_str(),
                Some("substitute-or-deny-placeholder"),
                "{group}.{field}"
            );
        }
    }


    // `local_backend` moved to `harness::opencode::settings` with the two
    // provider tests that were its only callers (V40 Phase B).

    // ── V23 Phase A: Code Audit settings ──────────────────────────────────

    /// The TS mirror embedded at compile time so a Rust-side wire change that
    /// isn't reflected in `types.ts` fails `cargo test` rather than shipping as
    /// silent Rust↔TS drift (V16/V22 tripwire pattern). Path is relative to this
    /// file (`src-tauri/src/settings/`), up to the repo root.
    const AUDIT_TS_TYPES: &str = include_str!("../../../src/lib/settings/types.ts");

    /// The fourteen audit-tool wire ids the FRONTEND still keys off, and the
    /// mirror that keeps them true.
    ///
    /// V38 Phase E deleted `AuditToolId`: the fourteen built-in tools are
    /// embedded plugin manifests now, so their ids are strings read from JSON
    /// rather than an enum. The mirror did not go away with it, because the ids
    /// still cross the wire — the Code Audit panel's category, order and
    /// applicability maps are all keyed by them, and a rename in the manifest
    /// with no matching rename in `src/lib/codeAudit/types.ts` would show up as
    /// a chip that never lights.
    ///
    /// So the authority moved from the enum to the manifest, and the tripwire
    /// moved with it: the list below is READ from the embedded manifest rather
    /// than restated, so it cannot drift from what actually ships.
    fn builtin_audit_tool_ids() -> Vec<String> {
        let set = crate::plugins::builtin::plugin_set();
        set.plugins
            .iter()
            .flat_map(|p| p.manifest.tools.iter())
            .map(|t| t.id.clone())
            .collect()
    }

    #[test]
    fn builtin_audit_tool_ids_are_mirrored_in_the_frontend_union() {
        const CODE_AUDIT_TS: &str = include_str!("../../../src/lib/codeAudit/types.ts");
        let ids = builtin_audit_tool_ids();
        assert_eq!(ids.len(), 14, "the built-in audit roster is fourteen tools");
        for id in ids {
            assert!(
                CODE_AUDIT_TS.contains(&format!("'{id}'")),
                "built-in audit tool `{id}` is missing from the TS `AuditToolId` union in \
                 src/lib/codeAudit/types.ts — the panel keys its category, order and \
                 applicability maps off that union, so an id it does not know renders as a \
                 chip that never lights"
            );
        }
    }

    #[test]
    fn code_audit_field_names_mirrored_in_types_ts() {
        // Serialize a fully-populated CodeAuditSettings and assert each JSON key
        // appears in types.ts, so any field added on the Rust side must also
        // land in the TS interface. The per-tool array is gone (schema v34):
        // what a tool is configured with lives in `tool_plugins` now, and its
        // own mirror is `tool_plugins_field_names_mirrored_in_types_ts`.
        let s = CodeAuditSettings {
            enabled: true,
            timeout_secs: 600,
            quality_auto_select: true,
            expose_offload: true,
        };
        let top = serde_json::to_value(&s).expect("CodeAuditSettings serializes");
        for key in top.as_object().expect("object").keys() {
            assert!(
                AUDIT_TS_TYPES.contains(&format!("{key}:")),
                "CodeAuditSettings field `{key}` is missing from the TS `CodeAuditSettings` \
                 interface in src/lib/settings/types.ts",
            );
        }
        // …and the field that LEFT must be gone from the mirror too, or the
        // frontend would keep reading an array the backend no longer writes.
        assert!(
            !AUDIT_TS_TYPES.contains("tools: AuditToolConfig[]"),
            "src/lib/settings/types.ts still declares `code_audit.tools`, which schema v34 \
             moved into `tool_plugins`"
        );
    }

    /// **V39 Phase A: the new fields need no migration step.** A settings file
    /// written before V39 has neither `delegation` nor a per-tab `read_only`;
    /// both containers are `#[serde(default)]`, so it loads at the current
    /// schema version with the tab writable and delegation's own defaults —
    /// which is why schema 35 is not bumped for this phase.
    #[test]
    fn delegation_and_read_only_default_when_absent() {
        let s: Settings = serde_json::from_value(json!({})).expect("empty settings deserialize");
        assert!(
            s.delegation.auto_read_only,
            "the courtesy lock while a tab is driven ships ON"
        );
        assert_eq!(s.delegation.default_timeout_s, 600);
        assert_eq!(s.delegation.max_depth, 1, "no nesting by default");

        let pre_v39 = json!({
            "tabs": [{
                "kind": "ai_tool",
                "id": "claude",
                "name": "Claude",
                "command": "claude",
            }],
        });
        let s: Settings = serde_json::from_value(pre_v39).expect("pre-V39 settings deserialize");
        match s.find_tab("claude") {
            Some(TabConfig::AiTool(c)) => assert!(
                !c.read_only,
                "an existing tab must load writable — the lock is a user action"
            ),
            other => panic!("expected the AI tab, got {other:?}"),
        }
    }

    /// **The user's read-only lock survives a restart** — it is persisted, and
    /// it round-trips through the file rather than living only in memory.
    #[test]
    fn a_user_read_only_lock_round_trips_through_settings() {
        let mut s = Settings::default();
        // `Settings::default()` carries no tabs — the integrity check seeds the
        // builtins at load time — so the fixture supplies the one it locks.
        s.tabs.push(default_claude_tab());
        let id = match s.tabs.iter_mut().find_map(|t| match t {
            TabConfig::AiTool(c) => Some(c),
            _ => None,
        }) {
            Some(c) => {
                c.read_only = true;
                c.id.clone()
            }
            None => panic!("Settings::default() seeds at least one AI tab"),
        };
        let json = serde_json::to_value(&s).expect("serialize");
        assert_eq!(
            json["tabs"]
                .as_array()
                .and_then(|a| a.iter().find(|t| t["id"] == json!(id)))
                .map(|t| t["read_only"].clone()),
            Some(json!(true)),
            "the lock must reach the file"
        );
        let back: Settings = serde_json::from_value(json).expect("deserialize");
        match back.find_tab(&id) {
            Some(TabConfig::AiTool(c)) => assert!(c.read_only),
            other => panic!("expected the AI tab, got {other:?}"),
        }
    }

    #[test]
    fn code_audit_defaults_present_when_block_absent() {
        // An old settings file with no `code_audit` key round-trips to the
        // feature-disabled defaults.
        let s: Settings = serde_json::from_value(json!({})).expect("empty settings deserialize");
        assert!(!s.code_audit.enabled);
        assert_eq!(s.code_audit.timeout_secs, 600);
        // Quality auto-selection ships on: a pre-existing file without the key
        // (and every fresh install) follows the project's language census until
        // the user edits a quality checkbox.
        assert!(s.code_audit.quality_auto_select);
        // V26: every MCP-exposure flag defaults on. The master `enabled`
        // switch (false above) is what actually keeps a fresh install silent;
        // these gate per-consumer opt-out once the feature is turned on. V40
        // Phase B moved the per-HARNESS half into `harness[<id>]`, so this now
        // covers a third harness the day one registers instead of naming two.
        for h in crate::harness::registry::all() {
            assert!(s.harness_settings(h).expose_code_audit, "{h}");
        }
        assert!(s.code_audit.expose_offload);
        // The roster no longer needs seeding into settings at all: it IS the
        // embedded manifest, and an untouched container means "every tool at
        // its declared default". That is what makes a fresh install and an
        // upgraded one agree without a seeding step to keep in step.
        assert!(s.tool_plugins.plugins.is_empty());
    }

    /// A v23-era `code_audit` block still deserializes, and the per-tool array
    /// it carries is simply not a field any more — the v33 → v34 migration is
    /// what moves it, and a file that skipped the migration must not fail to
    /// load over a key it no longer has a home for.
    #[test]
    fn a_legacy_code_audit_block_still_loads_and_ignores_its_tool_array() {
        let ca: CodeAuditSettings = serde_json::from_value(json!({
            "enabled": true,
            "timeout_secs": 120,
            "quality_auto_select": false,
            "tools": [
                { "id": "gitleaks", "enabled": true, "path": "", "extra_args": [] },
                { "id": "semgrep", "enabled": false, "path": "sg.exe", "extra_args": [] }
            ]
        }))
        .expect("a v23-era code_audit block still deserializes");
        assert!(ca.enabled);
        assert_eq!(ca.timeout_secs, 120);
        assert!(!ca.quality_auto_select);
        // …and the additive V26 flag still fills in.
        assert!(ca.expose_offload);
    }

    #[test]
    fn code_audit_v23_json_without_expose_flags_loads_true() {
        // V26: a pre-V26 (schema v23) `code_audit` block that predates the
        // three MCP-exposure flags deserializes with all three defaulting on —
        // the container-level `#[serde(default)]` fills the absent fields from
        // `CodeAuditSettings::default()`. This is why the v23 → v24 migration
        // is a pure version stamp (no data transform): the additive bools
        // round-trip for free.
        let ca: CodeAuditSettings = serde_json::from_value(json!({
            "enabled": true,
            "timeout_secs": 600,
            "quality_auto_select": true,
            "tools": []
        }))
        .expect("v23 code_audit block deserializes");
        assert!(ca.expose_offload);
        // The per-harness half is `harness[<id>].expose_code_audit` since V40
        // Phase B; a settings file with no `harness` block at all reads the
        // same default, which `an_absent_harness_row_reads_its_declared_defaults`
        // is the direct check on.
        let s = Settings::default();
        for h in crate::harness::registry::all() {
            assert!(s.harness_settings(h).expose_code_audit, "{h}");
        }
    }

    #[test]
    fn offload_v28_json_without_session_push_loads_false() {
        // V30 Phase A: a pre-v29 `offload` block that predates `session_push`
        // deserializes with the flag OFF — the container-level
        // `#[serde(default)]` fills the absent field from
        // `OffloadSettings::default()`. This is why the v28 → v29 migration is
        // a pure version stamp (no data transform): the additive bool
        // round-trips for free, and an upgrading user never silently gets a
        // research-preview channel registration they didn't ask for.
        let o: OffloadSettings = serde_json::from_value(json!({
            "enabled": true,
            "autostart": true,
            "inject_guidance": true,
            "server_command": "llama-server --jinja",
            "escalate_partial": true
        }))
        .expect("v28 offload block deserializes");
        assert!(!o.session_push);
        // Default-constructed settings agree (the toggle ships off).
        assert!(!OffloadSettings::default().session_push);
    }

    #[test]
    fn readonly_preset_merge_is_idempotent() {
        // V21 F7: merging into empty settings adds git + cargo to the allowlist
        // and installs the cargo policy (git already has its default policy).
        let mut allowlist: Vec<String> = vec![];
        let mut policies = default_command_policies(); // seeds git
        merge_readonly_preset(&mut allowlist, &mut policies);
        assert!(allowlist.iter().any(|a| a == "git"));
        assert!(allowlist.iter().any(|a| a == "cargo"));
        let cargo_policies = policies.iter().filter(|p| p.program == "cargo").count();
        assert_eq!(cargo_policies, 1, "cargo policy installed exactly once");
        let allowlist_after_one = allowlist.clone();
        let policies_after_one = policies.clone();

        // Re-merging is a no-op: no duplicate allowlist entries, no second cargo
        // policy — a merge action, not a mode.
        merge_readonly_preset(&mut allowlist, &mut policies);
        assert_eq!(
            allowlist, allowlist_after_one,
            "allowlist unchanged on re-merge"
        );
        assert_eq!(
            policies, policies_after_one,
            "policies unchanged on re-merge"
        );

        // A hand-added `git` (any case) is not duplicated either.
        let mut hand = vec!["GIT".to_string()];
        let mut pol = default_command_policies();
        merge_readonly_preset(&mut hand, &mut pol);
        assert_eq!(
            hand.iter()
                .filter(|a| a.eq_ignore_ascii_case("git"))
                .count(),
            1
        );
    }

    #[test]
    fn readonly_preset_respects_a_user_cargo_policy() {
        // A user who already authored their own `cargo` policy keeps it — the
        // preset must not clobber or duplicate it.
        let mut allowlist: Vec<String> = vec![];
        let mut policies = vec![CommandPolicy {
            program: "cargo".to_string(),
            denied_flags: vec!["--frozen".to_string()],
            denied_subcommands: vec![],
            allowed_subcommands: vec![],
            env: vec![],
        }];
        merge_readonly_preset(&mut allowlist, &mut policies);
        assert_eq!(policies.iter().filter(|p| p.program == "cargo").count(), 1);
        // The kept policy is the user's, not the preset's.
        assert_eq!(policies[0].denied_flags, vec!["--frozen".to_string()]);
    }

    #[test]
    fn command_policy_missing_allowed_subcommands_deserializes() {
        // Backward compat: a config written before `allowed_subcommands` existed
        // must load with an empty allowlist (serde default), not fail.
        let pol: CommandPolicy = serde_json::from_value(json!({
            "program": "git",
            "denied_flags": ["-c"],
            "denied_subcommands": ["config"],
            "env": [],
        }))
        .expect("legacy policy without allowed_subcommands deserializes");
        assert!(pol.allowed_subcommands.is_empty());
    }

    #[test]
    fn local_backend_kind_defaults_show_command_on_start() {
        // A pre-existing config written before the field existed must
        // deserialize with the popup off (opt-in behavior).
        let kind: OffloadBackendKind = serde_json::from_value(json!({
            "type": "local",
            "server_command": "llama-server --port 8080 --jinja",
            "autostart": true,
        }))
        .expect("legacy local kind deserializes");
        match kind {
            OffloadBackendKind::Local {
                show_command_on_start,
                ..
            } => assert!(
                !show_command_on_start,
                "missing field must default to false"
            ),
            _ => panic!("expected Local kind"),
        }
    }

    #[test]
    fn local_backend_kind_defaults_auth_token() {
        // V33 Phase E, the trap this test exists for: `OffloadBackendKind` is
        // `#[serde(tag = "type")]` with NO container-level `#[serde(default)]`,
        // and serde's container default would not apply to enum variants even
        // if it had one. Without the FIELD-level `#[serde(default)]` on
        // `auth_token`, this deserialize fails — and since `backends` is a plain
        // `Vec<OffloadBackend>`, that failure takes the WHOLE settings file with
        // it, for every install in existence.
        let kind: OffloadBackendKind = serde_json::from_value(json!({
            "type": "local",
            "server_command": "llama-server --port 12344 --jinja",
            "autostart": true,
            "show_command_on_start": false,
        }))
        .expect("a pre-V33 local kind (no auth_token) must still deserialize");
        match kind {
            OffloadBackendKind::Local { auth_token, .. } => assert!(
                auth_token.is_empty(),
                "missing field must default to no token = no Authorization header"
            ),
            _ => panic!("expected Local kind"),
        }
        // …and the whole enclosing backend list, which is how it is really read.
        let backends: Vec<OffloadBackend> = serde_json::from_value(json!([{
            "name": "local",
            "enabled": true,
            "kind": { "type": "local", "server_command": "llama-server", "autostart": false },
        }]))
        .expect("a pre-V33 backends array must still deserialize");
        assert_eq!(backends.len(), 1);
    }

    /// V33 stage 3 — the resolver the dashboard poller and the headless
    /// `--offload-mcp` child both read, in all three directions.
    ///
    /// It matters that this is tested at the *settings* seam and not only in
    /// `offload::server`: those two call sites used to read the raw field, and a
    /// backend authenticated on the supervised path but not on the dashboard or
    /// the headless one is a credential bug that only shows up where nobody is
    /// looking.
    #[test]
    fn effective_auth_token_falls_back_to_the_commands_api_key() {
        let local = |token: &str, cmd: &str| OffloadBackendKind::Local {
            server_command: cmd.to_string(),
            autostart: false,
            show_command_on_start: false,
            auth_token: token.to_string(),
        };
        let keyed = "llama-server --port 12344 --api-key sk-from-cmd";
        assert_eq!(
            local("sk-configured", keyed).effective_auth_token(),
            "sk-configured",
            "an explicitly configured token wins"
        );
        assert_eq!(
            local("", keyed).effective_auth_token(),
            "sk-from-cmd",
            "an empty token inherits the command's --api-key"
        );
        assert_eq!(
            local("", "llama-server --port 12344").effective_auth_token(),
            "",
            "neither ⇒ no token, which is no Authorization header"
        );
        // Remote has no command to inherit from — the configured value, verbatim.
        let remote = OffloadBackendKind::Remote {
            base_url: "https://api.example.com".into(),
            auth_token: "sk-remote".into(),
            is_cloud: true,
            cloud_consent: true,
        };
        assert_eq!(remote.effective_auth_token(), "sk-remote");
    }

    #[test]
    fn offload_backend_debug_redacts_both_kinds_auth_token() {
        let local = OffloadBackend {
            kind: OffloadBackendKind::Local {
                server_command: "llama-server".into(),
                autostart: false,
                show_command_on_start: false,
                auth_token: "sk-local-secret".into(),
            },
            ..Default::default()
        };
        let dbg = format!("{local:?}");
        assert!(!dbg.contains("sk-local-secret"), "{dbg}");
        assert!(dbg.contains("auth_token: <redacted>"), "{dbg}");
        // An absent token says so rather than reading as a hidden one.
        let none = OffloadBackend::default();
        assert!(format!("{none:?}").contains("auth_token: <none>"));
    }

    #[test]
    fn graph_settings_debug_covers_every_field_and_redacts_the_token() {
        // V33 Phase E. Two properties in one test because they share a cause:
        // `GraphSettings` had a DERIVED `Debug` and a doc saying "No secrets
        // here" until it gained `embedding_auth_token`.
        let g = GraphSettings {
            embedding_auth_token: "sk-embed-secret".into(),
            ..GraphSettings::default()
        };
        let dbg = format!("{g:?}");
        assert!(
            !dbg.contains("sk-embed-secret"),
            "the embedding bearer token reached a Debug line: {dbg}"
        );
        assert!(dbg.contains("embedding_auth_token: \"<redacted>\""), "{dbg}");
        assert!(format!("{:?}", GraphSettings::default())
            .contains("embedding_auth_token: \"<empty>\""));

        // The cost of hand-rolling `Debug` on a struct this wide is that a
        // field added later is silently dropped from every debug line. Walk the
        // serialized key set — the same names, no serde renames in this block —
        // and require each to appear.
        let json = serde_json::to_value(&g).expect("GraphSettings serializes");
        for key in json.as_object().expect("a JSON object").keys() {
            assert!(
                dbg.contains(&format!("{key}:")),
                "the hand-rolled GraphSettings Debug omits `{key}` — add a \
                 `.field(\"{key}\", &self.{key})` line"
            );
        }
    }

    #[test]
    fn mcp_server_config_defaults_and_redacts_auth_token() {
        // Additive over the container-level `#[serde(default)]`: a pre-V33
        // entry loads with no token, which is no `Authorization` header, which
        // is today's behaviour.
        let cfg: McpServerConfig = serde_json::from_value(json!({
            "name": "ddg",
            "url": "http://172.21.1.11:17201/mcp",
            "offload_access": true,
        }))
        .expect("a pre-V33 mcp server entry deserializes");
        assert!(cfg.auth_token.is_empty());

        let with_token = McpServerConfig {
            auth_token: "sk-mcp-secret".into(),
            ..cfg
        };
        let dbg = format!("{with_token:?}");
        assert!(!dbg.contains("sk-mcp-secret"), "{dbg}");
        assert!(dbg.contains("auth_token: \"<redacted>\""), "{dbg}");
    }

    /// V37 contract C2. Both new server fields must default to the values that
    /// reproduce pre-v32 behaviour — a server EXISTS and its provenance is
    /// UNTRUSTED — because the v31 → v32 migration deliberately writes no data
    /// and leans entirely on these.
    #[test]
    fn mcp_server_config_defaults_origin_and_enabled() {
        let cfg: McpServerConfig = serde_json::from_value(json!({
            "name": "ddg",
            "url": "http://172.21.1.11:17201/mcp",
            "offload_access": true,
        }))
        .expect("a pre-V37 mcp server entry deserializes");
        assert!(cfg.enabled, "a pre-v32 server must stay on the surface");
        assert_eq!(cfg.origin, McpOrigin::External);
        // Neither is a secret; the redacted Debug shows both.
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("enabled: true"), "{dbg}");
        assert!(dbg.contains("origin: External"), "{dbg}");

        // Explicit values round-trip, and `origin` uses the lowercase wire form
        // the TS mirror (`src/lib/settings/types.ts`) writes.
        let internal: McpServerConfig = serde_json::from_value(json!({
            "name": "bundled",
            "command": "cimp-mcp",
            "origin": "internal",
            "enabled": false,
        }))
        .expect("an explicit v32 entry deserializes");
        assert_eq!(internal.origin, McpOrigin::Internal);
        assert!(!internal.enabled);
        let text = serde_json::to_string(&internal).unwrap();
        assert!(text.contains("\"origin\":\"internal\""), "{text}");
        let back: McpServerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.origin, McpOrigin::Internal);
        assert!(!back.enabled);
    }

    /// V37 contract C2: the registry starts empty, and the activation halves
    /// are OBJECTS on the wire. The shape is load-bearing, not cosmetic —
    /// `persistence::deep_merge` merges objects per key but replaces arrays
    /// wholesale, so an array here would make one project's override discard
    /// every other entry (see `overlay_activation_merges_per_key`).
    #[test]
    fn offload_settings_default_registry_is_empty_and_activation_is_map_shaped() {
        let o = OffloadSettings::default();
        assert!(o.mcp_categories.is_empty());
        assert!(o.mcp_activation.categories.is_empty());
        assert!(o.mcp_activation.servers.is_empty());

        // A pre-v32 offload block loads with the empty registry.
        let parsed: OffloadSettings = serde_json::from_value(json!({ "enabled": true })).unwrap();
        assert!(parsed.mcp_categories.is_empty());
        assert!(parsed.mcp_activation.servers.is_empty());

        // Round-trip with content, and check the wire shape.
        let mut full = OffloadSettings {
            mcp_servers: vec![McpServerConfig {
                name: "ddg".into(),
                url: "http://x/mcp".into(),
                origin: McpOrigin::External,
                enabled: true,
                ..Default::default()
            }],
            mcp_categories: vec![McpCategory {
                name: "research".into(),
                servers: vec!["ddg".into()],
                enabled: true,
            }],
            ..Default::default()
        };
        full.mcp_activation.servers.insert("ddg".into(), false);
        full.mcp_activation
            .categories
            .insert("research".into(), true);

        let v = serde_json::to_value(&full).unwrap();
        assert!(v["mcp_activation"]["servers"].is_object(), "{v}");
        assert!(v["mcp_activation"]["categories"].is_object(), "{v}");
        assert!(v["mcp_categories"].is_array(), "{v}");

        let back: OffloadSettings = serde_json::from_value(v).unwrap();
        assert_eq!(back.mcp_categories.len(), 1);
        assert_eq!(back.mcp_categories[0].servers, vec!["ddg".to_string()]);
        assert_eq!(back.mcp_activation.servers.get("ddg"), Some(&false));
        assert_eq!(back.mcp_activation.categories.get("research"), Some(&true));
        // A fresh category is ON: creating a group never silently hides tools.
        assert!(McpCategory::default().enabled);
    }

    // `sync_provider_noop_when_auto_off_or_server_disabled` and
    // `sync_provider_derives_when_enabled_and_rederives_on_change` moved to
    // `harness::opencode::settings` with the two fields they drive (V40 Phase B,
    // locked decision 6): same cases, same commands, asserted through the
    // harness map instead of two `OffloadSettings` fields named after a harness.

    #[test]
    fn ai_tab_id_serde_wire_format_matches_tab_ids() {
        // The serde wire string for each AiTabId MUST equal its tab-id constant
        // (and the frontend literal + the migration output). A mismatch
        // quarantines settings on load. Round-trip both directions.
        for (variant, id) in [
            (AiTabId::Claude, "claude"),
            (AiTabId::ClaudeLocal, "claude-local"),
            (AiTabId::OpenCode, "opencode"),
        ] {
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                json!(id),
                "serialize {id}"
            );
            assert_eq!(
                serde_json::from_value::<AiTabId>(json!(id)).unwrap(),
                variant,
                "deserialize {id}"
            );
            assert_eq!(variant.as_str(), id, "as_str {id}");
        }
    }

    #[test]
    fn malformed_layout_drops_to_none_without_losing_rest_of_settings() {
        // A Split node missing `ratio` is invalid, but it must not take the
        // whole Settings parse down with it. The layout degrades to None and
        // a sibling field (session.active_tab_id) still loads.
        let v = json!({
            "session": { "active_tab_id": "claude" },
            "layout": {
                "tree": {
                    "type": "split",
                    "id": "s1",
                    "direction": "horizontal",
                    "first": { "type": "pane", "id": "p1", "tab_ids": ["claude"], "active_tab_id": "claude" },
                    "second": { "type": "pane", "id": "p2", "tab_ids": [], "active_tab_id": null }
                },
                "focused_pane_id": "p1"
            }
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert!(
            parsed.layout.is_none(),
            "malformed layout should drop to None"
        );
        assert_eq!(parsed.session.active_tab_id.as_deref(), Some("claude"));
    }

    #[test]
    fn valid_layout_still_parses() {
        let v = json!({
            "layout": {
                "tree": { "type": "pane", "id": "p1", "tab_ids": ["claude"], "active_tab_id": "claude" },
                "focused_pane_id": "p1"
            }
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert!(parsed.layout.is_some(), "valid layout should parse");
    }

    #[test]
    fn malformed_preset_is_dropped_individually() {
        // First preset is valid; second is missing `tree`. Keep the good one.
        let v = json!({
            "layout_presets": [
                {
                    "name": "good",
                    "created_at": "2026-01-01T00:00:00Z",
                    "tree": { "type": "pane", "id": "p1", "tab_ids": [], "active_tab_id": null }
                },
                { "name": "bad", "created_at": "2026-01-01T00:00:00Z" }
            ]
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.layout_presets.len(), 1);
        assert_eq!(parsed.layout_presets[0].name, "good");
    }

    #[test]
    fn background_override_null_round_trips_as_none() {
        // Field-level: serializing/deserializing the Option<BackgroundOverride>
        // with `None` produces a JSON `null`.
        let none: Option<BackgroundOverride> = None;
        let v = serde_json::to_value(&none).unwrap();
        assert_eq!(v, Value::Null);
        let parsed: Option<BackgroundOverride> = serde_json::from_value(Value::Null).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn background_override_disabled_string_round_trips() {
        let disabled = Some(BackgroundOverride::Disabled);
        let v = serde_json::to_value(&disabled).unwrap();
        assert_eq!(v, json!("disabled"));
        let parsed: Option<BackgroundOverride> = serde_json::from_value(json!("disabled")).unwrap();
        assert!(matches!(parsed, Some(BackgroundOverride::Disabled)));
    }

    #[test]
    fn stt_settings_default_round_trips() {
        let s = SttSettings::default();
        let v = serde_json::to_value(&s).unwrap();
        let back: SttSettings = serde_json::from_value(v).unwrap();
        assert!(back.enabled);
        assert_eq!(back.device, ProcessingDevice::Gpu);
        assert_eq!(back.model_file, "ggml-small.bin");
        assert_eq!(back.language, "auto");
        assert!(back.input_device.is_empty());
        assert_eq!(back.button_mode, SttButtonMode::Toggle);
        assert!(!back.translate_to_english);
    }

    #[test]
    fn processing_device_serializes_snake_case() {
        // The wire form must be lowercase to match the frontend union type
        // `'gpu' | 'cpu'`.
        assert_eq!(
            serde_json::to_value(ProcessingDevice::Gpu).unwrap(),
            json!("gpu")
        );
        assert_eq!(
            serde_json::to_value(ProcessingDevice::Cpu).unwrap(),
            json!("cpu")
        );
    }

    #[test]
    fn tts_stt_without_device_field_default_to_gpu() {
        // Pre-existing settings files predate the `device` field; the additive
        // struct-level `#[serde(default)]` must load them as GPU (preserving the
        // historical "prefer GPU, fall back to CPU" behavior) — no migration.
        let tts: TtsSettings =
            serde_json::from_value(json!({ "enabled": true, "voice": "af_heart" })).unwrap();
        assert_eq!(tts.device, ProcessingDevice::Gpu);
        let stt: SttSettings =
            serde_json::from_value(json!({ "enabled": true, "model_file": "ggml-small.bin" }))
                .unwrap();
        assert_eq!(stt.device, ProcessingDevice::Gpu);
    }

    #[test]
    fn settings_without_stt_block_loads_defaults() {
        // An old settings file lacking the `stt` field deserializes with the
        // additive `#[serde(default)]` SttSettings — no migration needed.
        let json = json!({ "schema_version": CURRENT_SCHEMA_VERSION });
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.stt.enabled);
        assert_eq!(s.stt.model_file, "ggml-small.bin");
        // The push_to_talk shortcut default is present too.
        assert_eq!(s.shortcuts.push_to_talk.as_deref(), Some("Ctrl+Shift"));
    }

    #[test]
    fn background_override_custom_object_round_trips() {
        let cfg = TerminalBackgroundSettings {
            image: Some(PathBuf::from("/tmp/bg.png")),
            color: Some("#1a2b3c".to_string()),
            opacity: 0.7,
            blur: 12,
            size: BackgroundSize::Contain,
            position: "top left".to_string(),
            snapshot_lines: 2000,
            presets: Vec::new(),
            preview_category_flips: true,
        };
        let custom = Some(BackgroundOverride::Custom(cfg.clone()));
        let v = serde_json::to_value(&custom).unwrap();
        assert!(
            v.is_object(),
            "custom override should serialize as an object"
        );
        let parsed: Option<BackgroundOverride> = serde_json::from_value(v).unwrap();
        match parsed {
            Some(BackgroundOverride::Custom(out)) => {
                assert_eq!(out.image, cfg.image);
                assert_eq!(out.color, cfg.color);
                assert!((out.opacity - cfg.opacity).abs() < f32::EPSILON);
                assert_eq!(out.blur, cfg.blur);
                assert_eq!(out.size, cfg.size);
                assert_eq!(out.position, cfg.position);
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn background_override_rejects_garbage() {
        // Numbers, arrays, and arbitrary strings all fail with the
        // explicit error message — the only valid string is "disabled".
        for v in [json!(42), json!([1, 2, 3]), json!("random")] {
            let result: Result<BackgroundOverride, _> = serde_json::from_value(v);
            assert!(result.is_err(), "expected error for invalid override input");
        }
    }

    #[test]
    fn background_settings_default_matches_milestone_doc() {
        // Sanity check on the values the migration writes — opacity 0.4,
        // blur 0, size cover, position center, no image, no color.
        let d = TerminalBackgroundSettings::default();
        assert!(d.image.is_none());
        assert!(d.color.is_none());
        assert!((d.opacity - 0.4).abs() < f32::EPSILON);
        assert_eq!(d.blur, 0);
        assert_eq!(d.size, BackgroundSize::Cover);
        assert_eq!(d.position, "center");
        assert_eq!(d.snapshot_lines, 2000);
        assert!(d.presets.is_empty());
    }

    #[test]
    fn background_preset_config_round_trips() {
        let p = BackgroundPresetConfig {
            image: Some(PathBuf::from("/tmp/frost.jpg")),
            color: Some("#0011aa".to_string()),
            opacity: 0.55,
            blur: 8,
            size: BackgroundSize::Tile,
            position: "top right".to_string(),
            snapshot_lines: 5000,
        };
        let v = serde_json::to_value(&p).unwrap();
        let parsed: BackgroundPresetConfig = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.image, p.image);
        assert_eq!(parsed.color, p.color);
        assert!((parsed.opacity - p.opacity).abs() < f32::EPSILON);
        assert_eq!(parsed.blur, p.blur);
        assert_eq!(parsed.size, p.size);
        assert_eq!(parsed.position, p.position);
        assert_eq!(parsed.snapshot_lines, p.snapshot_lines);
    }

    #[test]
    fn background_preset_config_from_settings_strips_presets() {
        // From<&TerminalBackgroundSettings> for BackgroundPresetConfig
        // copies the shared fields; presets has no analogue on the sister
        // struct, so it is dropped.
        let mut s = TerminalBackgroundSettings {
            color: Some("#101010".to_string()),
            ..TerminalBackgroundSettings::default()
        };
        s.presets.push(BackgroundPreset {
            name: "noise".to_string(),
            config: BackgroundPresetConfig::default(),
        });
        let p: BackgroundPresetConfig = (&s).into();
        assert_eq!(p.color, s.color);
        // The round-trip back through Into<TerminalBackgroundSettings>
        // produces a fresh `presets: []` regardless of what `s` had.
        let s2: TerminalBackgroundSettings = p.into();
        assert!(s2.presets.is_empty());
    }

    #[test]
    fn background_preset_round_trips() {
        let preset = BackgroundPreset {
            name: "Frosted glass".to_string(),
            config: BackgroundPresetConfig {
                image: Some(PathBuf::from("C:\\images\\frost.jpg")),
                color: None,
                opacity: 0.4,
                blur: 12,
                size: BackgroundSize::Cover,
                position: "center".to_string(),
                snapshot_lines: 2000,
            },
        };
        let v = serde_json::to_value(&preset).unwrap();
        let parsed: BackgroundPreset = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.name, preset.name);
        assert_eq!(parsed.config.image, preset.config.image);
        assert_eq!(parsed.config.blur, preset.config.blur);
    }

    // --- V14 Phase A: prompt library -----------------------------------

    #[test]
    fn starter_prompt_templates_has_the_four_named_entries() {
        let templates = starter_prompt_templates();
        let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "review-this-diff",
                "write-tests-for",
                "explain-selection",
                "commit-message",
            ]
        );
    }

    #[test]
    fn resolve_prompt_templates_project_shadows_global_by_name() {
        let global = vec![
            PromptTemplate {
                name: "a".to_string(),
                body: "global-a".to_string(),
            },
            PromptTemplate {
                name: "b".to_string(),
                body: "global-b".to_string(),
            },
        ];
        let project = vec![
            // Shadows global "a" — project body wins, global "a" is dropped.
            PromptTemplate {
                name: "a".to_string(),
                body: "project-a".to_string(),
            },
            // Project-only entry, appended after the (filtered) global list.
            PromptTemplate {
                name: "c".to_string(),
                body: "project-c".to_string(),
            },
        ];
        let resolved = resolve_prompt_templates(global, project);
        assert_eq!(
            resolved.len(),
            3,
            "shadowed global \"a\" must not appear twice"
        );

        let a = resolved.iter().find(|t| t.name == "a").unwrap();
        assert_eq!(a.body, "project-a");
        assert_eq!(a.scope, "project");

        let b = resolved.iter().find(|t| t.name == "b").unwrap();
        assert_eq!(b.body, "global-b");
        assert_eq!(b.scope, "global");

        let c = resolved.iter().find(|t| t.name == "c").unwrap();
        assert_eq!(c.body, "project-c");
        assert_eq!(c.scope, "project");
    }

    #[test]
    fn resolve_prompt_templates_empty_project_passes_global_through() {
        let global = vec![PromptTemplate {
            name: "a".to_string(),
            body: "x".to_string(),
        }];
        let resolved = resolve_prompt_templates(global, Vec::new());
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].scope, "global");
    }

    #[test]
    fn settings_default_starts_unseeded_with_no_templates() {
        let s = Settings::default();
        assert!(!s.templates_seeded);
        assert!(s.prompt_templates.is_empty());
    }

    // ── V38 Phase B — the `tool_plugins` container ────────────────────────

    /// A fully-populated container survives serialize → deserialize unchanged.
    ///
    /// This is what keeps [`deserialize_lenient_tool_plugins`] honest: it reads
    /// the tree field by field (it has to — nulls appear at every level), so a
    /// field added to `PluginState`/`ToolState` and not to the walk would load
    /// as its default and silently discard the user's setting. That failure has
    /// no other symptom, which is exactly why it needs a test.
    #[test]
    fn tool_plugins_round_trips_through_the_lenient_reader() {
        let mut tools = BTreeMap::new();
        tools.insert(
            "scan".to_string(),
            ToolState {
                enabled: false,
                timeout_secs: Some(900),
                parameters: vec!["--exclude".into(), "vendor".into()],
                variables: BTreeMap::from([("ruleset".to_string(), "p/ci".to_string())]),
            },
        );
        let cfg = ToolPluginsSettings {
            plugins: BTreeMap::from([(
                "acme@1.0.0".to_string(),
                PluginState {
                    enabled: false,
                    tools,
                },
            )]),
            project_paths: BTreeMap::from([(
                "C:\\repo".to_string(),
                BTreeMap::from([("acme@1.0.0/scan".to_string(), "C:\\bin\\acme.exe".to_string())]),
            )]),
            global_paths: BTreeMap::from([(
                "acme@1.0.0/scan".to_string(),
                "D:\\tools\\acme.exe".to_string(),
            )]),
            ..ToolPluginsSettings::default()
        };
        let s = Settings {
            tool_plugins: cfg.clone(),
            ..Settings::default()
        };
        let round: Settings =
            serde_json::from_value(serde_json::to_value(&s).expect("serialize")).expect("parse");
        assert_eq!(round.tool_plugins, cfg);
    }

    /// The overlay diff writes an explicit `null` for a key the baseline has and
    /// the current value does not — that is how it expresses a DELETION. Every
    /// other `Settings` field is a struct or an array, where the case cannot
    /// arise; this container is keyed by data, so it must read a null as the
    /// deletion it is instead of failing the whole file's parse.
    #[test]
    fn a_null_entry_is_a_deletion_not_a_parse_failure() {
        let v = json!({
            "schema_version": CURRENT_SCHEMA_VERSION,
            "tool_plugins": {
                "plugins": {
                    "gone@1.0.0": null,
                    "acme@1.0.0": {
                        "enabled": true,
                        "tools": {
                            "dropped": null,
                            "scan": { "enabled": true, "variables": { "cleared": null, "kept": "v" } }
                        }
                    }
                },
                "global_paths": { "acme@1.0.0/scan": "C:\\bin\\acme.exe", "gone@1.0.0/x": null }
            }
        });
        let s: Settings = serde_json::from_value(v).expect("a null entry must not fail the parse");
        let tp = &s.tool_plugins;
        assert_eq!(tp.plugins.keys().collect::<Vec<_>>(), vec!["acme@1.0.0"]);
        let acme = &tp.plugins["acme@1.0.0"];
        assert_eq!(acme.tools.keys().collect::<Vec<_>>(), vec!["scan"]);
        assert_eq!(
            acme.tools["scan"].variables,
            BTreeMap::from([("kept".to_string(), "v".to_string())])
        );
        assert_eq!(tp.global_paths.keys().collect::<Vec<_>>(), vec!["acme@1.0.0/scan"]);
    }

    /// One malformed plugin's state must not cost the user everything else in
    /// the file — the `deserialize_lenient_audit_tools` rule, one container over.
    #[test]
    fn a_malformed_entry_is_dropped_rather_than_taking_the_file_down() {
        let v = json!({
            "schema_version": CURRENT_SCHEMA_VERSION,
            "tool_plugins": { "plugins": { "bad@1.0.0": "not an object", "ok@1.0.0": {} } }
        });
        let s: Settings = serde_json::from_value(v).expect("parse");
        assert_eq!(s.tool_plugins.plugins.keys().collect::<Vec<_>>(), vec!["ok@1.0.0"]);
        // …and the surviving entry gets the defaults, which are ON.
        assert!(s.tool_plugins.plugins["ok@1.0.0"].enabled);
    }

    /// Both `enabled` flags default to true, at both levels — an installed
    /// plugin is on, and the gate that decides whether a tool RUNS is its path.
    #[test]
    fn tool_plugin_state_defaults_to_enabled() {
        let v = json!({ "plugins": { "a@1": { "tools": { "t": {} } } } });
        let cfg: ToolPluginsSettings = serde_json::from_value(v).expect("parse");
        assert!(cfg.plugins["a@1"].enabled);
        assert!(cfg.plugins["a@1"].tools["t"].enabled);
        assert_eq!(cfg.plugins["a@1"].tools["t"].timeout_secs, None);
    }

    /// The Rust↔TS mirror, the `check_def_field_names_mirrored_in_types_ts`
    /// convention: every wire key of the container must exist in the TS
    /// interfaces, so a field added on one side cannot quietly skip the other.
    #[test]
    fn tool_plugins_field_names_mirrored_in_types_ts() {
        const TS_TYPES: &str = include_str!("../../../src/lib/settings/types.ts");
        let mut tools = BTreeMap::new();
        tools.insert("t".to_string(), ToolState::default());
        let cfg = ToolPluginsSettings {
            plugins: BTreeMap::from([("a@1".to_string(), PluginState { enabled: true, tools })]),
            project_paths: BTreeMap::from([("root".to_string(), BTreeMap::new())]),
            global_paths: BTreeMap::new(),
            ..ToolPluginsSettings::default()
        };
        let value = serde_json::to_value(&cfg).expect("serializes");
        let mut keys: Vec<String> = value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect();
        for k in value["plugins"]["a@1"].as_object().expect("plugin state").keys() {
            keys.push(k.clone());
        }
        for k in value["plugins"]["a@1"]["tools"]["t"]
            .as_object()
            .expect("tool state")
            .keys()
        {
            keys.push(k.clone());
        }
        for key in keys {
            assert!(
                TS_TYPES.contains(&format!("{key}:")),
                "`tool_plugins` wire field `{key}` is missing from src/lib/settings/types.ts \
                 (ToolPluginsSettings / PluginState / ToolState) — add it to keep the mirror \
                 in sync",
            );
        }
        // The container itself must be reachable from the Settings interface.
        assert!(
            TS_TYPES.contains("tool_plugins: ToolPluginsSettings;"),
            "src/lib/settings/types.ts must carry `tool_plugins` on `Settings`"
        );
    }
}
