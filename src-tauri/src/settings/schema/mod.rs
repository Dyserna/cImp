//! Settings schema. Every struct uses `#[serde(default)]` so loading a JSON
//! file written by a future or past version still succeeds: missing fields
//! get defaults, unknown fields are ignored. v1.2 schema; the v1 → v2 and
//! v1.1 → v1.2 migrations live in `migration.rs` and run once on first load.
//!
//! **V42 R10 — one file per domain.** This was a single 7,698-line
//! `schema.rs`, the most-churned file in the repo: every feature in every
//! area added its fields to it, so ~35 % of its commits also touched
//! `offload/` and ~24 % `graph/`. The domain blocks were already contiguous,
//! so the split is pure code motion — each type keeps its `#[serde(…)]` and
//! `#[cfg_attr(test, ts(…))]` attributes and its `impl Default` beside it,
//! and each domain's tests moved with the types they drive.
//!
//! What stays here is what is not a domain: the [`Settings`] root and its
//! `impl`, [`AiTabId`] and the reserved tab-id constants (persisted wire
//! forms — the one `IDENTITY_ALLOWLIST` row `harness::layering` keeps for
//! this tree points here), the `lenient_*` / `de_*` serde helpers the domains
//! share, and the per-harness settings map.
//!
//! Every domain module is `pub use`d below, so `settings::schema::*` — and
//! through `settings`' own re-export, the whole crate's view of the schema —
//! is exactly the surface it was before the split. No consumer outside
//! this directory names a domain module, and none should: adding one would
//! re-create the import churn the split exists to avoid.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::shell::ShellSpec;

// ── The domain modules (V42 R10) ───────────────────────────────────────────
//
// Declared in the order the single file laid them out.

/// Session/layout persistence and the per-tab configs.
mod tabs;
/// The offload block, Code Audit, and the injection-protection settings.
mod offload;
/// The code-graph / code-intelligence block.
mod graph;
/// Workbench (checkpoints, diff, worktrees) and the sandbox knobs.
mod workbench;
/// Server-command templates, command policies, and the MCP/backend pool.
mod mcp;
/// Notification slots and the seeded builtin tabs.
mod notifications;
/// Avatar, TTS/STT, waveform and display.
mod media;
/// UI chrome: terminal, background, compose, prompt templates, shortcuts.
mod ui;

pub use graph::*;
pub use mcp::*;
pub use media::*;
pub use notifications::*;
pub use offload::*;
pub use tabs::*;
pub use ui::*;
pub use workbench::*;

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
/// `aider` reserved id. (The v1.7 → v1.8 migration that rewrote the aider
/// tab in place to this id was below the migration floor and deleted by V42 R9;
/// a file old enough to still say `aider` is quarantined, not migrated.)
pub const CLAUDE_LOCAL_TAB_ID: &str = "claude-local";
/// The single OpenCode AI-tool tab. OpenCode picks its own provider/model
/// (global config + credentials, switchable in-session), so unlike Claude
/// there is no cloud/local pair. Reserved in V19, replacing BOTH the V14
/// `aider` and `aider-local` reserved ids. (The v18 → v19 migration that
/// collapsed them into this one id was below the migration floor and deleted by
/// V42 R9.)
pub const OPENCODE_TAB_ID: &str = "opencode";
pub const SHELL_DEFAULT_TAB_ID: &str = "shell-default-1";
/// Legacy id of the V8-03 reserved Offload Server tab. Retired in schema v25:
/// the live backend dashboard moved INSIDE the Tool Activity tab as the
/// "Offload server" section (ToolActivityView.svelte), so there is no separate
/// reserved tab anymore. The v24 → v25 migration that dropped the old
/// materialized entry is below the migration floor and deleted (V42 R9); this
/// constant survives for `RETIRED_TAB_IDS`, the integrity check's fail-safe
/// prune, which is what still catches the id in an unmigrated overlay.
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
/// reserved tab anymore. The v25 → v26 migration that dropped the old
/// materialized entry is below the migration floor and deleted (V42 R9); this
/// constant survives for `RETIRED_TAB_IDS`, the integrity check's fail-safe
/// prune, which is what still catches the id in an unmigrated overlay.
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
/// reserved tab anymore. The v26 → v27 migration that dropped the old
/// materialized entry is below the migration floor and deleted (V42 R9); this
/// constant survives for `RETIRED_TAB_IDS`, the integrity check's fail-safe
/// prune, which is what still catches the id in an unmigrated overlay.
pub const CODE_AUDIT_TAB_ID: &str = "code-audit";
/// Legacy id of the V25 reserved Code Quality tab. Retired in schema v23: the
/// Quality view moved INSIDE the Code Audit tab as a sub-tab (Security |
/// Quality), so there is no separate reserved tab anymore. The v22 → v23
/// migration that dropped the old materialized entry is below the migration
/// floor and deleted (V42 R9); this constant survives for `RETIRED_TAB_IDS`,
/// the integrity check's fail-safe prune, which is what still catches the id in
/// an unmigrated overlay.
pub const CODE_QUALITY_TAB_ID: &str = "code-quality";
// V42 R9 (issue #120): `SHELL_BROOT_TAB_ID` lived here for one reason — the
// v15 → v16 migration that dropped the auto-seeded `shell-broot` entry. That
// step is below the migration floor and gone, and the id needs no fail-safe of
// its own: broot is an ordinary closable Shell tab now, so an entry that
// survives in an unmigrated overlay is a tab the user can close, not a reserved
// one the app would try to own (unlike the ids in `RETIRED_TAB_IDS`).

/// The on-disk schema version. Bumped on every migration step, and the ONLY
/// thing the cascade's detectors read: every step is `schema_version == N`.
///
/// A file that states no version at all is a pre-v1.10 file, from before this
/// field existed. Since V42 R9 that is not a shape the cascade can enter — it is
/// below [`crate::settings::migration::MIN_GLOBAL_SCHEMA_VERSION`], and such a
/// file is quarantined and reseeded rather than migrated.
pub const CURRENT_SCHEMA_VERSION: u8 = 38;

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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
    // HAND-KEPT SEAM: `CheckDef`/`ParserKind` mirror `crate::checks`, NOT this
    // file, so V42 Phase E left them hand-written in `types.ts` (and left
    // `checks::tests`' mirror tripwire pointing at them). The generated file
    // therefore names the hand-kept type through an inline type-only import.
    #[cfg_attr(test, ts(type = "import('../types').CheckDef[]"))]
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
    // HAND-KEPT SEAM: `AiTabId`'s (de)serialize is hand-written (a bare id
    // string, refused unless a descriptor claims it), so no derive expresses
    // it. The frontend's `AiTabId` is that same bare string; this points at
    // its declaration rather than restating the alias here.
    #[cfg_attr(test, ts(type = "import('../../tabs/types').AiTabId[]"))]
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
    /// or navigated — the toolbar then falls back to `lib/preview/policy.ts`'s
    /// `DEFAULT_PREVIEW_URL`.
    pub preview_last_url: Option<String>,
    /// V14 Phase F: global gate on the Preview tab's navigation policy — when
    /// `false` (the default), `preview::is_allowed_preview_host` only allows
    /// localhost/127.0.0.1/RFC-1918 hosts; navigation to anything else opens
    /// in the system browser instead. Opt-in per the milestone's design: a
    /// Preview tab is a dev-server surface, not a general (and
    /// prompt-injectable) browsing pane beside the agent tabs. The toolbar
    /// pre-flights the same rule in `lib/preview/policy.ts`'s
    /// `isAllowedPreviewHost` before it calls the backend at all.
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
    /// is a property of the project you happen to have open, so a project overlay
    /// never carries the machine-scope half of this block and a Settings save
    /// writes that half through to the physical global file — the `sandbox`
    /// pattern, one level down.
    ///
    /// One level down is also where the MECHANISM differs, and V40 review finding
    /// M-2 is why: banning the whole container (as `sandbox` is banned) silently
    /// narrowed five per-project settings that had moved into it to machine
    /// scope. `harness` therefore gets a structured, per-field strip
    /// (`persistence::strip_overlay_harness`, which NAMES what it drops) rather
    /// than a whole-key ban.
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
            // V40 Phase I: the FIRST REGISTERED harness's first built-in tab,
            // not a literal `[ai_tab_id("claude")]` — the same rule
            // `integrity_check` repairs an empty list to, so a fresh install and
            // a repaired one agree without either naming a product.
            enabled_ai_tabs: canonical_ai_tab_order().into_iter().take(1).collect(),
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
    // `Record<string, unknown>` rather than ts-rs' `JsonValue` shadow type:
    // the window renders `ext` from the plugin's declaration and names no key,
    // and a `JsonValue` alias would drag a second generated file in for
    // nothing. Matches what the mirror said before V42 Phase E.
    #[cfg_attr(test, ts(type = "Record<string, unknown>"))]
    pub ext: BTreeMap<String, serde_json::Value>,
    /// Anything else the file carried. See *Unknown keys survive* above.
    // Absent from the mirror before V42 Phase E and deliberately still absent:
    // the flattened catch-all is a round-trip mechanism, not a frontend-
    // readable field.
    #[cfg_attr(test, ts(skip))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct McpAccess {
    /// Expose this server's tools to this harness, proxied through its
    /// per-session `cimp-offload --consumer <id>` child. Off by default: a new
    /// server reaches nothing until the user says so.
    pub enabled: bool,
}

/// One provider/model price entry: USD per million tokens (MTok) for the four
/// billing categories the transcripts report. The four ids are this table's own
/// field names and the `usage_stat` columns' declared spellings (`input`,
/// `cache_write`, `cache_read`, `output`). Fully user-editable in
/// Settings → LLM pricing; the session-cost popup multiplies these against a
/// session's token totals. `(provider, model)` is the display identity — no
/// uniqueness is enforced, the popup just lists rows in order.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct LlmPricingModel {
    pub provider: String,
    pub model: String,
    /// V16 Feature 8: transcript model-id prefix this row auto-matches
    /// (e.g. `"claude-opus-4-8"` matches both the bare alias and dated
    /// snapshots). Longest matching prefix wins; empty = manual-pick only
    /// (the row still appears in the cost popup's dropdown). The frontend
    /// resolves the match in `usageMath.ts`'s `matchPricing`.
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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

/// One of the reserved AI-tool tab ids. Wire format is the kebab-
/// case tab-id string (`"claude"`, `"claude-local"`, `"opencode"`); the type
/// exists so `enabled_ai_tabs` can be a strongly-typed `Vec<AiTabId>` instead
/// of an untyped string list.
///
/// **V40 Phase I (issue #107 item 1): a newtype over the registry's own tab id,
/// not a closed enum.** It used to be three variants keyed to the two shipped
/// harnesses, and review finding M-3 was that nothing joined them to the
/// registry: a third descriptor compiled, and then its tab was dropped from
/// [`canonical_ai_tab_order`], could not be held by `enabled_ai_tabs`, had no
/// `default_ai_tab` arm, and fell through `TabId::from_str` to `Shell` while
/// `TabId::kind()` called it `AiTool`. Phase A added a test that FAILED in that
/// case; this closes the case instead, so all a new harness's tab needs is its
/// [`crate::harness::registry::BuiltinTab`] row.
///
/// The **wire format is unchanged** (locked decisions 3 and 29): it serializes
/// as the bare tab-id string and refuses one no descriptor claims — exactly what
/// the derived enum impl did, `opencode`'s explicit `#[serde(rename)]` included,
/// because the spelling now comes from the descriptor rather than from a variant
/// name serde would have split into `open-code`.
///
/// The inner `&'static str` is the registry's, so an `AiTabId` cannot be
/// fabricated: [`Self::from_id`] is the only constructor, and it is a lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AiTabId(&'static str);

impl AiTabId {
    /// Every reserved AI tab, in canonical order, is [`canonical_ai_tab_order`].
    ///
    /// There used to be a hand-kept `const ALL` beside it, pinned against the
    /// registry by `the_ai_tab_enum_and_the_registry_are_the_same_list`. It IS
    /// the registry now, so the second list — and the test that watched the two
    /// for drift — are gone with the drift they watched.
    pub fn as_str(self) -> &'static str {
        self.0
    }

    /// The reserved tab this id names, or `None` for a string no descriptor
    /// claims (a shell id, an `ai-<uuid>` duplicate, a retired id).
    pub fn from_id(id: &str) -> Option<Self> {
        crate::harness::registry::builtin_tab(id).map(|t| AiTabId(t.id))
    }

    /// This tab's declaration. Infallible by construction — the only way to
    /// hold an `AiTabId` is [`Self::from_id`], which looked it up.
    fn spec(self) -> &'static crate::harness::registry::BuiltinTab {
        crate::harness::registry::builtin_tab(self.0)
            .expect("an AiTabId is a registry lookup that succeeded")
    }

    /// True for the local-provider variants (`claude-local` today). The
    /// integrity check uses this as the canonical `use_local_provider` value for
    /// each reserved id.
    ///
    /// V40 Phase I: declared by the harness on its
    /// [`crate::harness::registry::BuiltinTab`] row, not a `matches!` here. A
    /// harness that ships a local-provider variant says so; core does not
    /// recognise one by the shape of its id.
    pub fn uses_local_provider(self) -> bool {
        self.spec().local_provider
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

/// **Tests only** — the [`AiTabId`] for a reserved tab id.
///
/// Naming a harness in a fixture is a recorded input, not a dependency on one
/// (the identity scan drops test regions for exactly this reason). What this
/// buys over an inline `unwrap` is that the panic names the id, instead of a
/// bare `None` twenty lines into a settings fixture.
#[cfg(test)]
pub(crate) fn ai_tab_id(id: &str) -> AiTabId {
    AiTabId::from_id(id).unwrap_or_else(|| panic!("`{id}` is not a registered reserved tab id"))
}

impl Serialize for AiTabId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for AiTabId {
    /// **Refuses an id no descriptor claims**, exactly as the enum's derived
    /// impl refused an unknown variant. That refusal is load-bearing: a
    /// `Vec<AiTabId>` that silently accepted junk would let a hand-edited
    /// settings file name a tab nothing can materialise, and the integrity check
    /// would then hold an id it cannot seed.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        AiTabId::from_id(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown AI tab id `{s}`; expected one of {:?}",
                crate::harness::registry::canonical_tab_ids()
            ))
        })
    }
}

/// The reserved AI tab ids in **canonical tab-bar order**, flattened out of the
/// registry (`claude` → `claude-local` → `opencode` today).
///
/// V40 Phase A: this used to be a literal `[ai_tab_id("claude"), ..]` array written
/// out in three places (`persistence::restore_enabled_ai_builtins`,
/// `ipc::tab_lifecycle`, and `AiTabId::canonical_order`), which is three places
/// a new harness's tab could be forgotten and only one of them would have said
/// so. The order is the descriptors' declaration order and each descriptor's own
/// `tab_ids` order — so a harness owns where its tabs sit relative to its own,
/// and the registry owns where harnesses sit relative to each other.
///
/// V40 review M-3 noted that the old `filter_map` here **dropped** a registered
/// tab id that had no `AiTabId` variant — the one way a third harness could
/// reach a shipped build with no canonical position and no way for the user to
/// enable it. Phase I removed the possibility rather than the symptom: an
/// [`AiTabId`] IS a registered tab id, so this is a total map and there is
/// nothing left to drop or to assert about.
pub fn canonical_ai_tab_order() -> Vec<AiTabId> {
    crate::harness::registry::canonical_tab_ids()
        .into_iter()
        .map(AiTabId)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn ai_tab_id_serde_wire_format_matches_tab_ids() {
        // The serde wire string for each AiTabId MUST equal its tab id (and the
        // frontend literal + the migration output). A mismatch quarantines
        // settings on load. Round-trip both directions, over the WHOLE registry
        // rather than three hand-spelled rows: a fourth reserved tab is covered
        // the day it is declared.
        let ids = crate::harness::registry::canonical_tab_ids();
        assert!(
            ids.len() >= 3,
            "the registry's tab list collapsed to {ids:?} — this test would pass by iterating              nothing"
        );
        for id in &ids {
            let tab = AiTabId::from_id(id).expect("a canonical tab id resolves");
            assert_eq!(serde_json::to_value(tab).unwrap(), json!(id), "serialize {id}");
            assert_eq!(
                serde_json::from_value::<AiTabId>(json!(id)).unwrap(),
                tab,
                "deserialize {id}"
            );
            assert_eq!(tab.as_str(), *id, "as_str {id}");
        }
        // The three spellings this build ships, pinned by hand as well: the loop
        // above would still pass if every id in the registry were renamed at
        // once, and these three strings are in every user's settings file
        // (locked decisions 3 and 29).
        assert_eq!(ids[..3], ["claude", "claude-local", "opencode"]);
    }

    /// **An id no descriptor claims is refused, not silently accepted** (V40
    /// Phase I).
    ///
    /// The old `AiTabId` was a serde enum, so an unknown string failed the
    /// whole `Settings` parse and the file was quarantined. The newtype's
    /// hand-written `Deserialize` has to keep doing that: a `Vec<AiTabId>` that
    /// accepted junk would let a hand-edited `enabled_ai_tabs` name a tab
    /// `restore_enabled_ai_builtins` cannot seed, which is a boot with a tab
    /// missing and nothing said.
    #[test]
    fn an_unregistered_ai_tab_id_is_refused_on_the_wire() {
        assert!(serde_json::from_value::<AiTabId>(json!("aider")).is_err());
        assert!(serde_json::from_value::<AiTabId>(json!("")).is_err());
        assert!(serde_json::from_value::<AiTabId>(json!("ai-1234")).is_err());
        assert!(AiTabId::from_id("shell-default-1").is_none());
        // …and the whole-settings consequence, which is the one users feel.
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        v["enabled_ai_tabs"] = json!(["claude", "aider"]);
        assert!(
            serde_json::from_value::<Settings>(v).is_err(),
            "an unknown reserved tab id must fail the parse, exactly as the enum did"
        );
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
}
