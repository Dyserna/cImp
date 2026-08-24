//! The offload block, Code Audit, the bundled external tools, and the
//! injection-protection settings.
//!
//! Split out of `schema.rs` by V42 R10; see the module docs in `mod.rs`.

use super::*;

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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
    /// `crate::settings::injection::native_web_mode` (the V42 #124 home of the parse; the old `tabs::config` alias is deleted)): a typo must neither
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── V23 Phase A: Code Audit settings ──────────────────────────────────

    // ── RETIRED by V42 Phase E ──────────────────────────────────────────
    //
    // `AUDIT_TS_TYPES` and the two field-name scans it fed —
    // `code_audit_field_names_mirrored_in_types_ts` and
    // `tool_plugins_field_names_mirrored_in_types_ts` — both asked "does every
    // wire key of this struct appear in `src/lib/settings/types.ts`?".
    //
    // `types.ts` no longer declares these types. `settings::codegen` emits
    // them into `src/lib/settings/generated/settings.ts` FROM THIS FILE during
    // `cargo test`, and CI regenerates and diffs the result — so a Rust field
    // that reached the wire reached the TypeScript in the same run. Keeping
    // the scans would be asserting that a generator generated.
    //
    // This is the one class of `include_str!` tripwire the refactoring
    // survey's "re-point, never delete" rule does not cover; its § 4 entry
    // carries the amendment. Every scan over a type that is still HAND-written
    // was re-pointed instead: `checks::tests`' `CheckDef`/`ParserKind` mirror,
    // `harness::health`'s panel-field scan, `sandbox::tabs`' `tabs: false`
    // defaults check, and `settings::frontend_mirrors`' value constants.

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
        const CODE_AUDIT_TS: &str = include_str!("../../../../src/lib/codeAudit/types.ts");
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
}
