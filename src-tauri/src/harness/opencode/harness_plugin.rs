//! V40 Phase A — **OpenCode's [`HarnessPlugin`]**: everything `tabs/config.rs`
//! used to branch on `command_is(.., "opencode")` for.
//!
//! Named `harness_plugin` rather than `plugin` because this directory already
//! has a [`super::plugin`] — the *generated JavaScript* plugin cImp writes into
//! `.opencode/plugin/`. Two different meanings of the word, one directory; the
//! longer name is the one that says which.
//!
//! Every body here is the pre-V40 body, moved verbatim (the Phase K rule).

use std::collections::HashMap;
use std::path::Path;

use crate::harness::plugin::{
    alloc_loopback_port, Canary, GrantCtx, HarnessPlugin, InputProfile, NativeTool,
    ProbeOutput,
};
use crate::harness::reader::OobSpec;
use crate::sandbox::{GrantAccess, GrantRow};
use crate::settings::{AiToolTabConfig, Settings};

/// The one instance. See [`super::super::claude::plugin::ClaudePlugin`] for why
/// it is a ZST.
pub struct OpenCodePlugin;

/// The value the registry's descriptor points at.
pub static PLUGIN: OpenCodePlugin = OpenCodePlugin;

/// **The one spelling of OpenCode's main-session lane**, written verbatim into
/// `usage_stat.origin`. The same string Claude uses, and deliberately so: the
/// column is shared, the rows already on disk carry it, and an id is frozen
/// once written.
pub(in crate::harness) const ORIGIN_SESSION: &str = "session";

/// **The one spelling of OpenCode's sub-session lane** — the roll-up target for
/// a turn whose `/memory/event` body carried a `parent_session_id` (the task
/// tool's child session). Same frozen-by-disk posture as [`ORIGIN_SESSION`].
pub(in crate::harness) const ORIGIN_AGENT: &str = "agent";

/// The billing categories OpenCode's generated plugin reports per turn.
///
/// Four, matching the `in_tok` / `out_tok` / `cache_read` / `cache_make` fields
/// its `/memory/event` POST body carries (see `templates/plugin.js`, which
/// derives them from `tok.input`, `tok.output + tok.reasoning`, `cache.read`
/// and `cache.write`). The ids are cImp's own pricing vocabulary — the same
/// four the price table has rates for — because that is what the stored columns
/// mean.
const TOKEN_KINDS: &[crate::harness::plugin::TokenKindSpec] = &[
    crate::harness::plugin::TokenKindSpec { id: "input", label: "Input" },
    crate::harness::plugin::TokenKindSpec { id: "cache_write", label: "Cache write" },
    crate::harness::plugin::TokenKindSpec { id: "cache_read", label: "Cache read" },
    crate::harness::plugin::TokenKindSpec { id: "output", label: "Output" },
];

/// The two lanes an OpenCode turn can be attributed to. The plugin stamps a
/// child session's POST with `parent_session_id`, and the loopback rolls that
/// spend up to the parent in the `subagent` lane — the same contract Claude's
/// sidechain rows follow, declared here rather than assumed by core.
const ORIGINS: &[crate::harness::plugin::TurnOrigin] = &[
    crate::harness::plugin::TurnOrigin {
        id: ORIGIN_SESSION,
        label: "main session",
        subagent: false,
    },
    crate::harness::plugin::TurnOrigin {
        id: ORIGIN_AGENT,
        label: "sub-agents",
        subagent: true,
    },
];

/// **The shape of a recorded OpenCode turn.**
///
/// Declared even though [`OpenCodePlugin::usage_source`] is `None`: this
/// harness reports no subscription quota and no context window, and it still
/// writes real per-turn token rows. Before V40 Phase G the two facts shared one
/// declaration, so saying the first forced saying the second.
pub static TURN_SHAPE: crate::harness::plugin::TurnUsageShape =
    crate::harness::plugin::TurnUsageShape { token_kinds: TOKEN_KINDS, origins: ORIGINS };

/// This plugin's own id, for the places inside `harness/opencode/` that have to
/// recognise their own tabs and settings rows. Inside this directory naming
/// this harness is the point; locked decision 10(a) polices core, not here.
pub(in crate::harness) fn me() -> crate::harness::HarnessId {
    crate::harness::HarnessId::from_id("opencode").expect("opencode is a registered harness")
}

impl HarnessPlugin for OpenCodePlugin {
    /// OpenCode's SSE event stream reports its own turn boundaries, so core
    /// runs **no** TUI heuristic for one of its tabs. This declaration replaces
    /// `pty::manager`'s `oob_drives_activity = matches!(spec.oob,
    /// Some(OobSpec::OpenCodeEvent { .. }))` — core testing for this harness's
    /// transport to decide whether to model its terminal.
    fn activity_source(&self) -> crate::harness::plugin::ActivitySource {
        crate::harness::plugin::ActivitySource::OutOfBand
    }

    /// OpenCode has **no usage source**. Its plugin posts per-turn token
    /// totals to `/memory/event` (which the graph records), but nothing
    /// reports a subscription quota or a live context window — so
    /// `harness_usage` answers *no usage source*, and the widget must render
    /// that as absence rather than as a harness sitting at 0%.
    fn usage_source(&self) -> Option<&'static dyn crate::harness::plugin::UsageSource> {
        None
    }

    /// …but it **does** record turns — see [`TURN_SHAPE`].
    ///
    /// V40 Phase G. The two declarations were one before, hanging off
    /// [`crate::harness::plugin::UsageSource`], which made "no quota" mean "no
    /// token accounting" and left this harness's `usage_stat` rows shaped by
    /// another harness's declaration.
    fn turn_usage_shape(&self) -> Option<&'static crate::harness::plugin::TurnUsageShape> {
        Some(&TURN_SHAPE)
    }

    /// Identity is the session id OpenCode reports over the loopback; it lives
    /// in its own key space and therefore can never name a cImp tab.
    fn session_key_space(&self) -> crate::harness::plugin::SessionKey {
        crate::harness::plugin::SessionKey::Session
    }

    fn input_profile(&self) -> Option<InputProfile> {
        Some(super::input::input_profile())
    }

    /// D-8 (maintenance 2026-08-04): cImp does not merely decline to inject
    /// `--mini` — it must actively strip a user-supplied one from an OpenCode
    /// tab's stored `args`. [`Self::resolve_oob`] unconditionally appends
    /// `--port <N> --hostname 127.0.0.1` (that port is the TTS event tap), and
    /// `opencode --mini --port N` HARD-FAILS: the two flags are mutually
    /// exclusive. So the combination is reachable — the v19→v20 migration
    /// stripped `--mini` from stored args once, but nothing stops it coming back
    /// via a hand-edited settings file, a `.cimp.custom.config.json` overlay, or
    /// a settings file carried over from another machine. Dropping it keeps the
    /// tab launchable (the flag is inert under V20 anyway) instead of handing
    /// the user an opaque OpenCode usage error.
    ///
    /// Matches the bare flag and the `--mini=<value>` form (clap accepts both
    /// for a bool flag), so neither spelling can survive into a launch that also
    /// carries `--port`.
    fn arg_is_rejected(&self, arg: &str) -> Option<&'static str> {
        (arg == "--mini" || arg.starts_with("--mini=")).then_some(
            "cImp launches OpenCode fullscreen with `--port` for the TTS event tap, and \
             OpenCode rejects `--mini` combined with `--port`. Remove it from the tab's args.",
        )
    }

    fn compose_env(
        &self,
        cfg: &AiToolTabConfig,
        settings: &Settings,
        tab: &str,
        _endpoint: Option<&crate::offload::loopback::Discovery>,
        env: &mut HashMap<String, String>,
    ) {
        // V19: OpenCode launch env. Now that the renderer is fullscreen (no
        // `--mini`), this still (1) injects the session-scoped config as one
        // `OPENCODE_CONFIG_CONTENT` env var — the env-var analog of Claude's
        // `--mcp-config` / `--settings` / `--append-system-prompt` CLI flags —
        // and (2) quiets terminal features that fight cImp's own
        // selection/title handling. Set before the per-tab `env` merge (which
        // the neutral composer applies last) so a user can override any of
        // these per tab.
        let config = super::config::build_opencode_config(cfg, settings, tab);
        // V35 Phase K: the env var's NAME is OpenCode's, so it is spelled once,
        // in `harness/opencode/config.rs`, beside the document it carries.
        env.insert(super::config::CONFIG_ENV.to_string(), config.to_string());
        // ── 2026-08-17: authenticate the TUI's own HTTP server ───────────────
        //
        // The fullscreen TUI hosts an HTTP server on the `--port` below, and
        // until then cImp depended on that server accepting UNAUTHENTICATED
        // loopback calls — capability `opencode.route.noauth`, whose second edge
        // was that any local process could `POST /session/:id/message` into a
        // live session and start an agent turn. Upstream's documented answer is
        // these two variables, and the whole mechanism lives in
        // `harness/opencode/config.rs`.
        //
        // A FRESH password per spawn, never persisted, never in argv, and read
        // back out of this map by `resolve_oob` so the tap presents the
        // credential the child will actually use. Not Settings-derived, so it
        // owes no `spawn_inject_sig` entry.
        for (name, value) in
            super::config::server_auth_env(&super::config::new_server_password())
        {
            env.insert(name, value);
        }
        // V32 Phase F: the generated plugin's only channel to its own tab
        // identity. OpenCode's `tool.execute.before` input carries a session id
        // but no tab and no cwd (the E2 spike's finding), and the latch registry
        // is keyed by (agent, tab) — so without this the beacon has nothing to
        // engage. Claude's side needs no equivalent: its hook command bakes
        // `--tab <id>` into argv.
        //
        // Unconditional and NOT Settings-derived (the tab id is config-derived
        // and stable), so it needs no `spawn_inject_sig` entry of its own.
        env.insert("CIMP_TAB_ID".to_string(), tab.to_string());
        env.insert(
            "OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT".to_string(),
            "1".to_string(),
        );
        env.insert(
            "OPENCODE_DISABLE_TERMINAL_TITLE".to_string(),
            "1".to_string(),
        );
        // Windows: OpenCode shells out via Git Bash. Pass the path through when
        // the parent environment already names it, so the child finds it.
        if let Ok(bash) = std::env::var("OPENCODE_GIT_BASH_PATH") {
            if !bash.is_empty() {
                env.insert("OPENCODE_GIT_BASH_PATH".to_string(), bash);
            }
        }
    }

    fn write_artifacts(
        &self,
        cfg: &AiToolTabConfig,
        settings: &Settings,
        tab: &str,
        working_dir: &Path,
    ) {
        // V19: OpenCode reads its guidance from a file referenced in the
        // injected config (`instructions`), so write that managed file at
        // launch.
        super::config::write_opencode_instructions(cfg, settings);
        // V10: drop the dependency-free injection/memory plugin into the
        // project's `.opencode/plugin/`, baking in the current loopback port +
        // token. Uses `working_dir` (the project root the TUI opens).
        super::plugin::write_opencode_plugin(working_dir, settings, tab);
    }

    fn resolve_oob(
        &self,
        _cfg: &AiToolTabConfig,
        _working_dir: &Path,
        extra_args: &mut Vec<String>,
        env: &HashMap<String, String>,
    ) -> Option<OobSpec> {
        let port = alloc_loopback_port()?;
        extra_args.push("--port".to_string());
        extra_args.push(port.to_string());
        extra_args.push("--hostname".to_string());
        extra_args.push("127.0.0.1".to_string());
        Some(OobSpec::OpenCodeEvent {
            port,
            // 2026-08-17: the credential for the server this child is about to
            // host, taken from the environment it will be spawned with — so the
            // tap authenticates with the password the server will read, and the
            // secret rides neither argv nor a URL.
            auth: super::config::server_auth_from_env(env),
        })
    }

    /// V16 Feature 1: run `opencode --version` once per tab spawn and record the
    /// first output line into the global `harness_versions` tripwire state.
    ///
    /// Best-effort in every direction: unresolvable binary, spawn failure, or
    /// junk output all just skip the note (`note_harness_version` also ignores
    /// empty strings and no-ops on an unchanged version).
    fn note_version(&self, command: &str) {
        let Ok(binary) = crate::pty::resolve_command(command) else {
            return;
        };
        std::thread::spawn(move || {
            let mut cmd = std::process::Command::new(binary);
            cmd.arg("--version");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                // CREATE_NO_WINDOW, same convention as every spawned subprocess.
                cmd.creation_flags(0x0800_0000);
            }
            // The stdio `output()` would have chosen implicitly, written down:
            // it spawns with stdin null and both output streams piped. Made
            // explicit so this can go through the spawn gate as a *spawn* —
            // wrapping the synchronous `output()` instead would hold the shared
            // guard for the whole run of `opencode --version`, and a long shared
            // hold blocks the sandbox's exclusive window (see `spawn_gate`).
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let Ok(out) = crate::spawn_gate::spawn_std(&mut cmd).and_then(|c| c.wait_with_output())
            else {
                return;
            };
            // `opencode --version` prints a bare version (e.g. "1.4.2"); take
            // the first line defensively in case a future build adds a banner.
            let version = String::from_utf8_lossy(&out.stdout);
            let version = version.lines().next().unwrap_or("").trim().to_string();
            crate::settings::note_harness_version("opencode", &version);
        });
    }

    fn spawn_sig(&self, s: &Settings) -> serde_json::Value {
        let post_edit = s.graph.enabled && s.graph.auto_check && !s.checks.is_empty();
        serde_json::json!({
            // V37 Phase F: ONE element — see the Claude half.
            "mcp": [crate::harness::plugin::audit_advertised(s, me())],
            "guidance": crate::harness::plugin::guidance_gates(s),
            "sandbox": crate::harness::plugin::sandbox_gates(s),
            // `write_opencode_plugin` inputs: plugin presence + its baked
            // CIMP_INJECT_ENABLED / CIMP_AUTO_CHECK_ENABLED flags.
            //
            // V32 Phase F: plugin PRESENCE is no longer `graph.enabled` alone —
            // sensor mode needs the plugin too (`opencode_plugin_wanted`).
            // V32 Phase G: that predicate is now per-tab, and its native-web
            // half is fully covered by the `"injection"` entry below (which
            // carries every tab's resolved mode), so only the app-wide graph
            // half belongs here.
            "plugin": [
                s.graph.enabled,
                s.graph.enabled && s.graph.context_injection,
                post_edit,
                // V33 Phase F: the pre-mutation checkpoint flag, and the fourth
                // disjunct of `opencode_plugin_wanted`. It is app-wide (not part
                // of the injection hierarchy), so it cannot ride the
                // `"injection"` entry below and needs a slot of its own.
                s.workbench.checkpoints,
            ],
            // The RESOLVED `local-llama` provider block
            // (`build_opencode_config`). The two stored rows behind it —
            // `ext["provider"]` and `ext["provider_auto"]` — ride the automatic
            // `ext` half of the signature, but the resolution is not a stored
            // value: with auto-sync on it is re-derived from the primary Local
            // backend's command, so an edit to THAT command has to move the
            // signature too.
            "provider": super::settings::resolve_provider(s)
                .map(|p| serde_json::json!([p.base_url, p.model, p.api_key])),
            // V32 Phase F: `sensor` bakes the beacon handler's flag into the
            // plugin, `deny` writes `permission.webfetch/websearch = "deny"`
            // into `OPENCODE_CONFIG_CONTENT` — both spawn-time, like the Claude
            // half. V32 Phase G: the per-tab fragment, now scoped to the
            // OPENCODE tabs and to the features this consumer reads (#48, F-x).
            "injection": crate::settings::injection::spawn_sig(s, me()),
        })
    }

    fn settings_schema(&self) -> &'static [crate::harness::plugin::SettingField] {
        super::settings::FIELDS
    }

    /// V32 Phase H's native-tool gate: the mechanism is a
    /// `tool.execute.before` handler inside the plugin file cImp generates for
    /// THIS harness, so no other consumer can be subject to it — and before
    /// V40 Phase B, `Consumer::reads` said so with a `self == Consumer::Opencode`
    /// comparison in core.
    fn scoped_features(&self) -> &'static [crate::harness::plugin::ScopedFeature] {
        &[crate::harness::plugin::ScopedFeature {
            feature: crate::settings::injection::Feature::HarnessNativeGate,
            ext_key: super::settings::NATIVE_GATE,
        }]
    }

    /// OpenCode's tool ids are lower-case, and this is the pair cImp's graph
    /// guidance names (locked decision 24). Until Phase E that blob told an
    /// OpenCode session to prefer `Read` and `Bash` — two tools it does not
    /// serve.
    fn tool_for_role(&self, role: crate::harness::plugin::ToolRole) -> Option<&'static str> {
        use crate::harness::plugin::ToolRole;
        Some(match role {
            ToolRole::Read => "read",
            ToolRole::Shell => "bash",
        })
    }

    /// The model-visible inventory, rendered once in OpenCode's vocabulary.
    /// See the sibling implementation in `claude/plugin.rs` for why the
    /// `OnceLock` is per implementation.
    fn instructions(&self) -> &[crate::harness::instructions::Instruction] {
        static CELL: std::sync::OnceLock<Vec<crate::harness::instructions::Instruction>> =
            std::sync::OnceLock::new();
        CELL.get_or_init(|| crate::harness::instructions::render_for(me()))
    }

    fn native_tools(&self) -> &'static [NativeTool] {
        super::tools::OPENCODE_NATIVE_TABLE
    }

    fn memory_arg_keys(&self, arg: crate::harness::plugin::MemArg) -> &'static [&'static str] {
        use crate::harness::plugin::MemArg;
        match arg {
            // camelCase, and `path` as the fallback the generated plugin itself
            // falls back to (`inp.args.filePath || inp.args.path`).
            MemArg::Path => &["filePath", "path"],
            MemArg::Pattern => &["pattern", "path", "query"],
            MemArg::Command => &["command"],
        }
    }

    fn capabilities(&self) -> &'static [crate::harness::contract::Capability] {
        super::input::CAPABILITIES
    }

    fn canaries(&self) -> &'static [Canary] {
        super::canary::CANARIES
    }

    fn probe(&self) -> ProbeOutput {
        let (results, observed, version) = super::probe::probe_opencode();
        ProbeOutput { results, observed, version }
    }

    /// One `opencode serve` child answers every OpenCode probe.
    fn probes_share_one_child(&self) -> bool {
        true
    }

    /// The ids [`Self::probe`] emits, in emission order. `tool_registry` is
    /// first on purpose: it is the security-relevant one, the standing manual
    /// maintenance obligation, and the reason the probe phase exists at all.
    fn probes(&self) -> &'static [&'static str] {
        &["opencode.tool_registry", "opencode.route.noauth"]
    }

    fn declared_unprobed(&self) -> &'static [(&'static str, &'static str)] {
        DECLARED_UNPROBED
    }

    /// OpenCode's generated plugin posts its pre-mutation checkpoint beacon
    /// under `AbortSignal.timeout(2000)` (`templates/plugin.js`) and starts the
    /// tool the instant that timer fires. Declared so core's pre-tool budget is
    /// derived from it rather than hand-computed against it.
    fn hook_reply_timeout(&self) -> Option<std::time::Duration> {
        Some(super::plugin::BEACON_REPLY_TIMEOUT)
    }

    /// `/memory/event` and `/latch/state` are OpenCode's alone: the generated
    /// plugin is the only artifact that has ever posted to either
    /// (`docs/CHP.md` § 4.3), and a `/latch/state` answer attributed to another
    /// harness would gate the wrong tab's native tools.
    fn legacy_wire_default_routes(&self) -> &'static [&'static str] {
        &["/memory/event", "/latch/state"]
    }

    fn permission_patterns(&self) -> &'static [crate::processing::permission::PatternSpec] {
        super::prompts::PATTERNS
    }

    fn legacy_permission_patterns(
        &self,
        era: &str,
    ) -> &'static [crate::processing::permission::PatternSpec] {
        super::prompts::legacy_patterns(era)
    }

    /// **The one gated harness** (locked decision 26). cImp does not bundle
    /// OpenCode's ~158 MB binary (V19 require-install decision), so enabling the
    /// tab without it installed would materialise a dead "command not found"
    /// terminal with nothing saying why. The same resolution the spawn path uses
    /// (`ebin` → `PATH`), so a refusal here means the launch would have failed.
    fn preflight(&self) -> Result<(), &'static str> {
        if crate::pty::resolve_command("opencode").is_ok() {
            Ok(())
        } else {
            Err("Install it from https://opencode.ai/docs (or drop opencode.exe in ebin/)")
        }
    }

    /// `opencode serve` is a Bun binary that forks children (observed: two
    /// grandchildren per server), so a bare `Child::kill` leaves a live HTTP
    /// server bound to the port cImp handed it. Declared explicitly even though
    /// it is the default: this one is an OBSERVATION, and the default is only a
    /// posture.
    fn needs_tree_reap(&self) -> bool {
        true
    }

    /// **OpenCode's half of the window's copy** (locked decision 27).
    ///
    /// The install hint is the one `preflight` already returns, spelled once
    /// here and reused there, so the refusal a user reads when enabling the tab
    /// and the hint the error overlay shows cannot drift apart. The lower-case
    /// `webfetch`/`websearch` are this harness's own tool names — the reason
    /// the web-visibility copy lists tools per harness instead of picking a
    /// spelling.
    fn affordances(&self) -> crate::harness::plugin::HarnessAffordances {
        use crate::harness::plugin::HarnessAffordances;
        HarnessAffordances {
            new_session_command: Some("/clear"),
            tool_list_refresh: Some("refreshes its tool list in the same session"),
            web_tools: &["webfetch", "websearch"],
            state_dirs: &[
                "~/.config/opencode",
                "~/.local/share/opencode",
                "~/.local/state/opencode",
            ],
            install_hint: Some("OpenCode is not installed. Install it from"),
            docs_url: Some("https://opencode.ai/docs"),
            local_provider: None,
            local_provider_note: Some(
                "OpenCode manages its own providers and credentials (global config, switchable \
                 in-session). Configure providers in OpenCode itself; cimp injects only its MCP \
                 tools and the TTS/offload/graph guidance.",
            ),
            local_provider_config_note: Some(
                "Registers this server as OpenCode's local-llama provider (base URL + model read                  from the command above) and selects it as the default model, so a freshly opened                  OpenCode tab is ready to work. Overrides any existing local-llama. Auto-sync                  re-derives it from the primary local backend at launch and on save, but only                  while the offload server is enabled. OpenCode reads the provider from its launch                  config — restart the OpenCode tab to apply a change.",
            ),
            local_provider_config_block_key: Some(super::settings::PROVIDER),
            local_provider_config_auto_key: Some(super::settings::PROVIDER_AUTO),
            inject_mechanism: Some("a generated .opencode/plugin"),
            default_command: "opencode",
            accent: "var(--accent-purple, #d2a8ff)",
            ..HarnessAffordances::default()
        }
    }

    /// The `opencode serve` probe's row in the spawn ledger — the argv is this
    /// product's CLI, so it lives with the code that runs it.
    fn spawn_sites(&self) -> &'static [crate::spawn_ledger::SpawnSite] {
        super::probe::SPAWN_SITES
    }

    /// OpenCode's TUI paints its own banner on a fresh tab, so the notification
    /// manager's "not until this tab has been interacted with" guard applies
    /// here too. Declared rather than inherited from the default, because the
    /// default is a *posture* and this is an observation.
    fn emits_startup_chrome(&self) -> bool {
        true
    }

    /// The `local-llama` provider block, derived from the offload server's own
    /// command line (locked decision 26) — the Settings "Add to OpenCode"
    /// button's backend.
    fn config_writer(&self) -> Option<&'static dyn crate::harness::plugin::ConfigWriter> {
        Some(&super::config::WRITER)
    }

    fn sandbox_grants(&self, ctx: &GrantCtx) -> Vec<GrantRow> {
        vec![
            GrantRow {
                path: ctx.xdg("XDG_CONFIG_HOME", &[".config"]).join("opencode"),
                access: GrantAccess::Full,
                is_file: false,
                reason: "OpenCode's config tree — opencode.json(c), themes, and the \
                         `node_modules` it installs plugin dependencies into at startup, \
                         which is why this is read+WRITE",
                required: false,
            },
            GrantRow {
                path: ctx.xdg("XDG_DATA_HOME", &[".local", "share"]).join("opencode"),
                access: GrantAccess::Full,
                is_file: false,
                reason: "OpenCode's data directory — auth.json, the session SQLite database \
                         (+ its -wal/-shm), logs, snapshots. Written continuously",
                required: false,
            },
            GrantRow {
                path: ctx.xdg("XDG_STATE_HOME", &[".local", "state"]).join("opencode"),
                access: GrantAccess::Full,
                is_file: false,
                reason: "OpenCode's state directory (~/.local/state/opencode)",
                required: false,
            },
        ]
    }
}

/// OpenCode's rows that **no probe can settle**, each with the reason (locked
/// decision 17). See the Claude twin for why this is a separate list.
const DECLARED_UNPROBED: &[(&str, &str)] = &[
    (
        "opencode.sse.events",
        "needs a scripted turn (L2 residual): GET /event on an idle server streams nothing, so \
         the event kinds only arrive if a real agent turn is driven",
    ),
    (
        "opencode.route.push",
        "needs a scripted turn (L2 residual): the dangerous half is `noReply` losing its meaning, \
         which is only observable as an agent turn that should not have started",
    ),
    (
        "opencode.plugin.load_all",
        "no probe can settle it, and it is inside the TCB: nothing outside a harness can verify \
         that a control inside it ran. A plugin that loads but skips the `throw` looks fully \
         functional. Manual OpenCode-veto spike; Phase I's `chp` handshake at least makes a STALE \
         plugin a mismatch instead of a mystery",
    ),
    (
        "opencode.input.profile",
        "no probe can settle it — same behaviour, same spike, same recorded outcome as          `claude.input.profile`",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether this plugin refuses `arg`.
    fn rejects(arg: &str) -> bool {
        PLUGIN.arg_is_rejected(arg).is_some()
    }

    #[test]
    fn is_mini_flag_matches_both_spellings() {
        assert!(rejects("--mini"));
        assert!(rejects("--mini=true"));
        assert!(rejects("--mini=false"));
        // Near misses stay put.
        assert!(!rejects("--minimal"));
        assert!(!rejects("--mini-mode"));
        assert!(!rejects("-m"));
        assert!(!rejects("mini"));
    }
}
