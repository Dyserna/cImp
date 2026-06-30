//! Per-tab launch configuration — translates a `TabConfig` (binary, flags,
//! TTS injection) from settings into a `PtyLaunchSpec`. Encapsulates
//! Claude's `--append-system-prompt` injection mechanism here so the
//! PtyManager stays generic.
//!
//! V3-M3: settings is the single source of truth — there is no per-tab
//! side table any more. `build_launch_spec` looks the tab up by id; an
//! unknown id is a hard error (the registry shouldn't know about a tab
//! whose entry is missing from settings).
//!
//! V19: AI tabs cover Claude Code (`claude` / `claude-local`) and a single
//! OpenCode tab (`opencode`). Claude's `use_local_provider` flag reads from
//! `claude_local` and synthesizes `ANTHROPIC_*` env vars. OpenCode manages its
//! own providers/credentials, so cimp injects no provider — just the mcp +
//! instructions. Per-tab `env` entries take precedence over synthesized values.
//!
//! System-prompt / capability injection differs by tool: Claude uses CLI
//! flags (`--append-system-prompt` / `--settings` / `--mcp-config`, see
//! `build_pre_args`); OpenCode uses a single `OPENCODE_CONFIG_CONTENT` env
//! var carrying the equivalent `mcp` / `instructions` JSON
//! (see `compose_ai_env` + `build_opencode_config`).

use std::collections::HashMap;
use std::path::Path;

use crate::error::{AppError, AppResult};
use crate::pty::{resolve_command, PtyLaunchSpec};
use crate::settings::{AiToolTabConfig, Settings, TabConfig};
use crate::state::TabId;

pub fn build_launch_spec(
    tab: TabId,
    settings: &Settings,
    launch_cwd: &Path,
    invocation_args: &[String],
) -> AppResult<PtyLaunchSpec> {
    let id = tab.as_str();
    let entry = settings.find_tab(id).ok_or_else(|| {
        AppError::Pty(format!("tab {id} has no settings entry"))
    })?;

    match entry {
        TabConfig::AiTool(cfg) => build_ai_tool_spec(tab, cfg, settings, launch_cwd, invocation_args),
        TabConfig::Shell(cfg) => {
            // The detection module verified the default Shell-1 binary;
            // user-supplied paths from the New Shell Tab dialog are
            // validated up-front in `create_shell_tab` (M2 Phase B).
            // Bare-name paths still go through `resolve_command` so a
            // settings entry pointing at "bash" picks up the PATH
            // resolution every launch.
            let binary = resolve_command(&cfg.command)?;
            let working_dir = cfg
                .cwd
                .clone()
                .unwrap_or_else(|| launch_cwd.to_path_buf());
            Ok(PtyLaunchSpec {
                tab,
                binary,
                pre_args: Vec::new(),
                extra_args: cfg.args.clone(),
                working_dir,
                env: cfg.env.clone(),
                // Shell tabs have no AI assistant output to speak.
                oob: None,
            })
        }
    }
}

fn build_ai_tool_spec(
    tab: TabId,
    cfg: &AiToolTabConfig,
    settings: &Settings,
    launch_cwd: &Path,
    invocation_args: &[String],
) -> AppResult<PtyLaunchSpec> {
    let binary = resolve_command(&cfg.command)?;
    let pre_args = build_pre_args(cfg, settings);
    let mut extra_args = build_extra_args(cfg, settings, invocation_args);
    let working_dir = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| launch_cwd.to_path_buf());
    // V19: OpenCode reads its guidance from a file referenced in the injected
    // config (`instructions`), so write that managed file at launch — kept off
    // the pure `compose_ai_env` path so the config builder stays test-safe.
    if command_is(&cfg.command, "opencode") {
        write_opencode_instructions(cfg, settings);
    }
    // V20: resolve the out-of-band TTS source. For OpenCode this also injects
    // the `--port`/`--hostname` the fullscreen TUI hosts its event server on
    // (which the adapter taps). Mutates `extra_args`, so it runs on the real
    // launch path only — the pure `build_extra_args` stays test-stable.
    let oob = resolve_oob_source(cfg, &working_dir, &mut extra_args);
    let env = compose_ai_env(cfg, settings);
    Ok(PtyLaunchSpec {
        tab,
        binary,
        pre_args,
        extra_args,
        working_dir,
        env,
        oob,
    })
}

/// V20: pick the out-of-band TTS source for an AI tab, and for OpenCode inject
/// the loopback `--port`/`--hostname` its TUI exposes the event stream on.
///
/// - **Claude** (`claude` / `claude-local`): tail the project's transcript
///   JSONL rooted at `working_dir`.
/// - **OpenCode**: allocate a free loopback port, append `--port <N>
///   --hostname 127.0.0.1` (appended last so it wins over any user `--port`),
///   and tap `http://127.0.0.1:<N>/event`. If no port can be allocated, the
///   tab still launches — just without automatic TTS.
/// - **Anything else**: no source.
fn resolve_oob_source(
    cfg: &AiToolTabConfig,
    working_dir: &Path,
    extra_args: &mut Vec<String>,
) -> Option<crate::oob::OobSpec> {
    if command_is(&cfg.command, "claude") {
        return Some(crate::oob::OobSpec::ClaudeTranscript {
            project_dir: working_dir.to_path_buf(),
        });
    }
    if command_is(&cfg.command, "opencode") {
        let port = alloc_loopback_port()?;
        extra_args.push("--port".to_string());
        extra_args.push(port.to_string());
        extra_args.push("--hostname".to_string());
        extra_args.push("127.0.0.1".to_string());
        return Some(crate::oob::OobSpec::OpenCodeEvent { port });
    }
    None
}

/// Reserve a free loopback TCP port by binding `127.0.0.1:0` and reading the
/// OS-assigned port, then releasing it. There is a small window between release
/// and OpenCode re-binding it, but on loopback at launch this is reliable in
/// practice; a collision just means the event tap fails to connect and the tab
/// has no automatic TTS (it still works otherwise).
fn alloc_loopback_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|addr| addr.port())
}

/// True when `command` resolves to the named binary, comparing on the
/// file stem so `"claude"`, `"claude.exe"`, and `"/usr/bin/claude"` all
/// match `"claude"`. AI launch behavior keys off this (and
/// `use_local_provider`) rather than the `TabId` variant, so a `+`-spawned
/// duplicate — which copies its template's `command` — gets identical
/// treatment to the reserved tab it came from.
fn command_is(command: &str, name: &str) -> bool {
    Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case(name))
        .unwrap_or(false)
}

/// Pre-args injected ahead of the tab's own `args` and the wrapper's
/// invocation args. Claude-only — these injections target Claude Code's CLI
/// flags; OpenCode gets the equivalents via `OPENCODE_CONFIG_CONTENT` (see
/// `build_opencode_config`), so non-Claude tabs get empty pre-args:
///
///   * `--append-system-prompt <instructions>` — TTS markup convention,
///     gated on the per-tab `tts_injection` toggle.
///   * `--settings <json>` — a session-scoped overlay pointing Claude
///     Code's `statusLine` at our `cimp --statusline` renderer, gated on
///     the global `statusline.enabled`. The overlay merges with the
///     user's own Claude settings (only `statusLine` is set), so it
///     scopes the context bar to cImp without touching `~/.claude`.
fn build_pre_args(cfg: &AiToolTabConfig, settings: &Settings) -> Vec<String> {
    if !command_is(&cfg.command, "claude") {
        return Vec::new();
    }
    let mut args = Vec::new();

    // Compose a single `--append-system-prompt` from every addendum we inject
    // (TTS markup convention + offload + graph guidance). Merging into one flag
    // avoids relying on Claude Code concatenating repeated flags. The same
    // composition feeds OpenCode's instructions file (see `build_opencode_config`)
    // so the two agents stay in lockstep.
    let addendum = compose_capability_guidance(cfg, settings);
    if !addendum.is_empty() {
        args.push("--append-system-prompt".to_string());
        args.push(addendum);
    }

    if settings.statusline.enabled {
        if let Some(command) = crate::statusline::launch_command() {
            let overlay = serde_json::json!({
                "statusLine": { "type": "command", "command": command }
            });
            args.push("--settings".to_string());
            args.push(overlay.to_string());
        }
    }

    // V8-01: point Claude at our own `cimp --offload-mcp` MCP server so
    // the `offload_task` tool is available in this session. Session-scoped
    // (a `--mcp-config` overlay), never written to `~/.claude`. Claude
    // spawns the `command` as argv (no shell), so the raw exe path is
    // correct — no shell-quoting needed (unlike the statusLine command).
    // V8-01 + V9-01: point Claude at our `cimp --offload-mcp` server, which
    // carries the `offload_task` tool, the `graph_*` tools, AND any MCP server
    // exposed to Claude Code. Inject it whenever ANY of those is in play — the
    // graph tools and Claude-exposed MCP servers must reach Claude even when
    // offload is disabled (they ride the same MCP child).
    if settings.offload.enabled || settings.graph.enabled || settings.offload.any_claude_mcp() {
        if let Ok(exe) = std::env::current_exe() {
            let mcp = serde_json::json!({
                "mcpServers": {
                    "cimp-offload": {
                        "command": exe.to_string_lossy(),
                        "args": ["--offload-mcp"]
                    }
                }
            });
            args.push("--mcp-config".to_string());
            args.push(mcp.to_string());
        }
    }

    args
}

/// Compose the capability-guidance addendum shared by Claude
/// (`--append-system-prompt`) and OpenCode (the managed instructions file):
/// the offload nudge (gated on `offload.enabled && offload.inject_guidance`)
/// and the code-graph nudge (gated on `graph.enabled`, with the semantic
/// addendum when `graph.semantic_search`). Sections are joined by a blank line.
/// Reusing the exact same sources keeps both agents in lockstep.
///
/// V20: the `[[TTS]]` markup convention is NO LONGER injected — AI tabs are
/// fullscreen and TTS is sourced out-of-band (`crate::oob`), which speaks all
/// assistant prose directly. The per-tab `tts_injection.enabled` flag is now
/// the "speak this tab" gate read by the out-of-band sources, not a prompt
/// injection toggle; `tts_injection.instructions` is vestigial.
fn compose_capability_guidance(_cfg: &AiToolTabConfig, settings: &Settings) -> String {
    let mut addendum = String::new();
    if settings.offload.enabled && settings.offload.inject_guidance {
        if !addendum.is_empty() {
            addendum.push_str("\n\n");
        }
        addendum.push_str(OFFLOAD_GUIDANCE);
    }
    if settings.graph.enabled {
        if !addendum.is_empty() {
            addendum.push_str("\n\n");
        }
        addendum.push_str(GRAPH_GUIDANCE);
        if settings.graph.semantic_search {
            addendum.push_str(GRAPH_SEMANTIC_GUIDANCE);
        }
    }
    addendum
}

/// V8-01: the system-prompt addendum telling Opus *when* to reach for
/// `offload_task`. Without this nudge the model rarely offloads. Gated by
/// `offload.inject_guidance`.
const OFFLOAD_GUIDANCE: &str = "You have an `offload_task` tool (from the cimp-offload MCP server) \
backed by a local model. For token-heavy subtasks — broad codebase searches, summarizing large \
files or logs, or web research — prefer calling `offload_task` with a self-contained instruction \
instead of doing the work yourself: it returns only a synthesized result, conserving your context \
window. Keep work that needs your full reasoning or the conversation's context here. Set the \
`thinking` arg to 'off' for simple lookups/extraction, 'on' for analysis, or leave it 'auto'.";

/// V9-01: the system-prompt addendum telling Opus the code-knowledge-graph
/// tools exist. Gated on `graph.enabled` (the tools are only injected then).
const GRAPH_GUIDANCE: &str = "This project has a code knowledge graph (from the cimp-offload MCP \
server). Prefer the `graph_*` tools over grep for code-structure questions: `graph_find_symbol` \
(where a symbol is defined), `graph_callers`/`graph_callees` (call relationships), \
`graph_references`, `graph_imports`, `graph_outline` (a file's definitions), `graph_transitive` \
(transitive call chains), `graph_search_docs` (documentation/doc-comments), and \
`graph_struct_search` (find code by AST shape via a tree-sitter query — e.g. every `.unwrap()` or \
every function with a given parameter pattern — when text search can't express the structure). They \
return precise, token-bounded results from an index, so they're cheaper and more exact than text \
search for 'where is X defined', 'who calls X', and impact analysis.";

/// V9-01: appended after [`GRAPH_GUIDANCE`] only when semantic search is on
/// (the `graph_semantic_docs` tool is advertised to Opus only then).
const GRAPH_SEMANTIC_GUIDANCE: &str = " Also available: `graph_semantic_docs`, a meaning-based \
(embedding) search over the project's docs and doc-comments — use it when you want relevant \
material that may not share keywords with your query.";

fn build_extra_args(
    cfg: &AiToolTabConfig,
    _settings: &Settings,
    invocation_args: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // V20: OpenCode launches in its native fullscreen (alternate-screen) TUI —
    // no `--mini`. The earlier inline forcing (`--mini`) was dropped because the
    // reduced palette hid commands like `/connect`; cImp now drives every AI tab
    // fullscreen and sources TTS out-of-band (OpenCode's `GET /event` stream),
    // not by scraping the linear terminal stream. See MILESTONE-V20.
    out.extend(cfg.args.iter().filter(|s| !s.is_empty()).cloned());

    // cimp is documented as a drop-in replacement for `claude`, so
    // invocation args (`cimp --resume <id>`, etc.) flow into every
    // Claude tab. OpenCode's model/provider selection arrives via the
    // injected config, not flags, so we only forward invocation args to
    // Claude tabs.
    if command_is(&cfg.command, "claude") {
        for arg in invocation_args {
            if !arg.is_empty() {
                out.push(arg.clone());
            }
        }
    }
    out
}

/// Deterministic path of the managed OpenCode instructions file for `cfg`.
/// One file per tab id (the TTS toggle is per-tab) under a managed dir next to
/// the exe (the portable root, like the offload discovery file), falling back
/// to the OS temp dir. Pure — computing the path never touches the filesystem,
/// so `build_opencode_config` stays test-safe; the actual write happens on the
/// real launch path (`build_ai_tool_spec`).
fn opencode_instructions_path(cfg: &AiToolTabConfig) -> std::path::PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir)
        .join("opencode-instructions");
    // Tab ids are kebab-case reserved ids or `ai-<uuid>` duplicates — safe as a
    // filename stem.
    dir.join(format!("{}.md", cfg.id))
}

/// Write the managed OpenCode instructions file for `cfg` (idempotent
/// overwrite at each launch so it tracks live settings). Best-effort: a write
/// failure just means OpenCode launches without the guidance addendum, exactly
/// like a Claude tab with TTS injection disabled. Removes a stale file when no
/// guidance applies so a since-disabled toggle doesn't leave dead instructions.
fn write_opencode_instructions(cfg: &AiToolTabConfig, settings: &Settings) {
    let path = opencode_instructions_path(cfg);
    let text = compose_capability_guidance(cfg, settings);
    if text.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let _ = std::fs::write(&path, text);
}

/// V19: synthesize OpenCode's session-scoped config — the JSON document that
/// `OPENCODE_CONFIG_CONTENT` carries (the env-var analog of Claude's
/// `--mcp-config` / `--settings` / `--append-system-prompt`):
///
/// - `$schema` marker.
/// - `mcp.cimp-offload` → `cimp --offload-mcp --consumer opencode`, injected
///   whenever offload, the graph, or an OpenCode-exposed MCP server is in play
///   (mirrors the Claude `--mcp-config` gate in `build_pre_args`).
/// - `instructions` → the managed guidance file (TTS + offload + graph), when
///   any guidance applies. The file content is written on the launch path; here
///   we only reference its (deterministic) path.
///
/// No `provider` block: OpenCode manages its own providers/credentials (global
/// config, switchable in-session), so cimp never injects one.
///
/// Additive by default — cimp does not set `OPENCODE_DISABLE_PROJECT_CONFIG`,
/// so a user's project config still merges underneath. Pure: no filesystem I/O.
fn build_opencode_config(cfg: &AiToolTabConfig, settings: &Settings) -> serde_json::Value {
    let mut config = serde_json::Map::new();
    config.insert(
        "$schema".to_string(),
        serde_json::Value::String("https://opencode.ai/config.json".to_string()),
    );

    // The single `cimp --offload-mcp` child carries `offload_task`, the
    // `graph_*` tools, and any OpenCode-exposed MCP server. Inject it whenever
    // ANY of those is in play (same shape as the Claude gate).
    if settings.offload.enabled || settings.graph.enabled || settings.offload.any_opencode_mcp() {
        if let Ok(exe) = std::env::current_exe() {
            config.insert(
                "mcp".to_string(),
                serde_json::json!({
                    "cimp-offload": {
                        "type": "local",
                        "command": [exe.to_string_lossy(), "--offload-mcp", "--consumer", "opencode"]
                    }
                }),
            );
        }
    }

    // Reference the managed instructions file when any guidance applies. The
    // file itself is written at launch (see `build_ai_tool_spec`).
    // NOTE: `instructions` is emitted as an array-of-paths (the documented
    // shape); confirm against the live schema at F1 alongside the provider
    // block — if OpenCode silently ignores it, the TTS/offload/graph guidance
    // never reaches the session (no launch error surfaces).
    if !compose_capability_guidance(cfg, settings).is_empty() {
        let path = opencode_instructions_path(cfg);
        config.insert(
            "instructions".to_string(),
            serde_json::json!([path.to_string_lossy()]),
        );
    }

    // V19: no `provider` block. The single OpenCode tab uses OpenCode's own
    // provider config + credentials (global `~/.config/opencode` + `auth.json`,
    // switchable in-session), so cimp injects only the mcp + instructions and
    // leaves provider/model selection entirely to OpenCode.
    serde_json::Value::Object(config)
}

/// V1.4-07 / V14: compose the spawn environment for an AI tab. Per-tab
/// `env` entries are the user's most-specific scope and take
/// precedence over synthesized values. The merge order is:
/// synthesized → tab.env (per-tab keys never get overwritten).
///
/// Synthesis is gated on `use_local_provider` and the resolved binary,
/// not the `TabId` variant — so a `+`-spawned duplicate is treated
/// exactly like the reserved tab it was cloned from:
///
/// - Claude binary + `use_local_provider`: synthesize
///   `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL`
///   from `claude_local`.
/// - OpenCode binary: always set `OPENCODE_CONFIG_CONTENT` (the synthesized
///   session config) plus the noise-suppression env vars. When
///   `use_local_provider`, the local endpoint is carried inside that config
///   as a `provider` block (see `build_opencode_config`), not as env.
/// - Anything else (cloud Claude, `use_local_provider` off): no synthesized
///   provider env — the user's existing configuration is in charge.
fn compose_ai_env(cfg: &AiToolTabConfig, settings: &Settings) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();

    // V20: Claude Code runs in its native fullscreen (alternate-screen) TUI —
    // cImp no longer sets `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`. The old
    // inline forcing existed so the scrape pipeline could find `[[TTS]]` markers
    // and keep mouse gestures local; both concerns are retired. TTS for AI tabs
    // is now sourced out-of-band (Claude's transcript JSONL), and the terminal
    // hosts the fullscreen app like any other terminal would. See MILESTONE-V20.

    // V19: OpenCode launch env. Now that the renderer is fullscreen (no
    // `--mini`), this still (1) injects the session-scoped config as one
    // `OPENCODE_CONFIG_CONTENT` env var — the env-var analog of Claude's
    // `--mcp-config` / `--settings` / `--append-system-prompt` CLI flags — and
    // (2) quiets terminal features that fight cImp's own selection/title
    // handling. Set before the per-tab `env` merge below so a user can override
    // any of these per tab.
    if command_is(&cfg.command, "opencode") {
        let config = build_opencode_config(cfg, settings);
        env.insert("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string());
        env.insert(
            "OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT".to_string(),
            "1".to_string(),
        );
        env.insert("OPENCODE_DISABLE_TERMINAL_TITLE".to_string(), "1".to_string());
        // Windows: OpenCode shells out via Git Bash. Pass the path through when
        // the parent environment already names it, so the child finds it.
        if let Ok(bash) = std::env::var("OPENCODE_GIT_BASH_PATH") {
            if !bash.is_empty() {
                env.insert("OPENCODE_GIT_BASH_PATH".to_string(), bash);
            }
        }
    }

    // Claude against a local provider: synthesize `ANTHROPIC_*` env. OpenCode's
    // local provider arrives inside `OPENCODE_CONFIG_CONTENT` (a `provider`
    // block, see `build_opencode_config`), not as env vars, so it is handled
    // there rather than here.
    if cfg.use_local_provider && command_is(&cfg.command, "claude") {
        let cl = &settings.claude_local;
        if !cl.base_url.is_empty() {
            env.insert("ANTHROPIC_BASE_URL".to_string(), cl.base_url.clone());
        }
        if !cl.auth_token.is_empty() {
            env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), cl.auth_token.clone());
        }
        if !cl.model_alias.is_empty() {
            // Claude Code primarily uses --model flag for model selection, but
            // ANTHROPIC_MODEL is honored by some proxies; setting both is harmless.
            env.insert("ANTHROPIC_MODEL".to_string(), cl.model_alias.clone());
        }
    }
    // Per-tab env wins over synthesized values.
    for (k, v) in &cfg.env {
        env.insert(k.clone(), v.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{default_claude_tab, default_opencode_tab};

    fn claude_cfg() -> AiToolTabConfig {
        match default_claude_tab() {
            TabConfig::AiTool(c) => c,
            _ => unreachable!("default_claude_tab is an AI tool tab"),
        }
    }

    /// An AI-tool tab whose command resolves to `opencode`.
    fn opencode_cfg() -> AiToolTabConfig {
        match default_opencode_tab() {
            TabConfig::AiTool(c) => c,
            _ => unreachable!("default_opencode_tab is an AI tool tab"),
        }
    }

    /// The value following the first `--settings` flag in `args`, parsed
    /// as JSON. `None` if no `--settings` flag is present.
    fn settings_overlay(args: &[String]) -> Option<serde_json::Value> {
        let i = args.iter().position(|a| a == "--settings")?;
        let raw = args.get(i + 1)?;
        Some(serde_json::from_str(raw).expect("--settings value is valid JSON"))
    }

    #[test]
    fn injects_statusline_overlay_for_claude_when_enabled() {
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings);

        let overlay = settings_overlay(&args).expect("statusLine overlay present");
        assert_eq!(overlay["statusLine"]["type"], "command");
        let cmd = overlay["statusLine"]["command"]
            .as_str()
            .expect("command is a string");
        // Points back at this binary's hidden subcommand, forward-slashed.
        assert!(cmd.ends_with(" --statusline"), "got: {cmd}");
        assert!(!cmd.contains('\\'), "path must be forward-slashed: {cmd}");
    }

    #[test]
    fn no_statusline_overlay_when_disabled() {
        let mut settings = Settings::default();
        settings.statusline.enabled = false;
        let args = build_pre_args(&claude_cfg(), &settings);
        assert!(settings_overlay(&args).is_none());
    }

    #[test]
    fn statusline_overlay_is_claude_only() {
        // OpenCode understands neither --append-system-prompt nor --settings
        // (its config arrives via OPENCODE_CONFIG_CONTENT), so its pre-args stay
        // empty even with the global toggle on.
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        let args = build_pre_args(&opencode_cfg(), &settings);
        assert!(args.is_empty(), "opencode must get no pre-args, got: {args:?}");
    }

    #[test]
    fn guidance_and_statusline_coexist() {
        // V20: TTS markup is no longer injected, but capability guidance
        // (graph/offload) still feeds --append-system-prompt; with the status
        // line also on, both pre-arg pairs are present.
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        settings.graph.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings);

        assert!(args.iter().any(|a| a == "--append-system-prompt"));
        assert!(args.iter().any(|a| a == "--settings"));
    }

    #[test]
    fn injects_offload_mcp_config_for_claude_when_enabled() {
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings);

        let i = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        assert_eq!(cfg["mcpServers"]["cimp-offload"]["args"][0], "--offload-mcp");
    }

    #[test]
    fn offload_and_graph_guidance_merge_into_one_flag() {
        // V20: with both offload and graph guidance on, they merge into a
        // single --append-system-prompt (TTS markup no longer participates).
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        settings.offload.inject_guidance = true;
        settings.graph.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings);

        let count = args.iter().filter(|a| *a == "--append-system-prompt").count();
        assert_eq!(count, 1, "addenda must merge into one flag");
        let i = args.iter().position(|a| a == "--append-system-prompt").unwrap();
        assert!(args[i + 1].contains("offload_task"));
        assert!(args[i + 1].contains("graph_find_symbol"));
    }

    #[test]
    fn no_offload_injection_when_disabled() {
        let settings = Settings::default(); // offload + graph off by default
        let args = build_pre_args(&claude_cfg(), &settings);
        assert!(!args.iter().any(|a| a == "--mcp-config"));
    }

    #[test]
    fn graph_enabled_alone_injects_mcp_config() {
        // V9-01: the graph tools ride the same `--offload-mcp` child, so the
        // MCP config must be injected when graph is on even if offload is off.
        let mut settings = Settings::default();
        settings.offload.enabled = false;
        settings.graph.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings);

        let i = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present when graph is enabled");
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        assert_eq!(cfg["mcpServers"]["cimp-offload"]["args"][0], "--offload-mcp");
    }

    #[test]
    fn claude_exposed_mcp_server_alone_injects_mcp_config() {
        // A server exposed to Claude Code rides the same `--offload-mcp` child,
        // so the MCP config must be injected even with offload + graph both off.
        let mut settings = Settings::default();
        settings.offload.enabled = false;
        settings.graph.enabled = false;
        settings.offload.mcp_servers = vec![crate::settings::McpServerConfig {
            name: "duckduckgo".to_string(),
            url: "http://host:1/mcp".to_string(),
            claude_access: true,
            offload_access: false,
            ..Default::default()
        }];
        let args = build_pre_args(&claude_cfg(), &settings);
        assert!(
            args.iter().any(|a| a == "--mcp-config"),
            "--mcp-config present when a server is exposed to Claude Code"
        );
    }

    #[test]
    fn graph_enabled_injects_graph_guidance() {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings);

        let i = args
            .iter()
            .position(|a| a == "--append-system-prompt")
            .expect("graph guidance produces an --append-system-prompt");
        assert!(args[i + 1].contains("graph_find_symbol"));
    }

    #[test]
    fn offload_injection_is_claude_only() {
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        let args = build_pre_args(&opencode_cfg(), &settings);
        assert!(args.is_empty(), "opencode must get no pre-args, got: {args:?}");
    }

    #[test]
    fn claude_launches_fullscreen_by_default() {
        // V20: cImp no longer forces Claude's inline renderer. Without an
        // explicit per-tab override, no `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`
        // is synthesized, so Claude runs in its native fullscreen TUI.
        let settings = Settings::default();
        let env = compose_ai_env(&claude_cfg(), &settings);
        assert!(
            !env.contains_key("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
            "V20: cImp must not force Claude's inline renderer",
        );
    }

    #[test]
    fn no_ai_tab_forces_inline_renderer() {
        // V20: neither AI tool gets the alt-screen opt-out; both go fullscreen.
        let settings = Settings::default();
        for env in [
            compose_ai_env(&claude_cfg(), &settings),
            compose_ai_env(&opencode_cfg(), &settings),
        ] {
            assert!(
                !env.contains_key("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
                "no AI tab should set the Claude fullscreen opt-out in V20",
            );
        }
    }

    // ---- V19: OpenCode launch spine ----

    #[test]
    fn opencode_launches_without_mini() {
        // V20: OpenCode runs its full fullscreen TUI — no `--mini` is injected,
        // so the complete command palette (e.g. `/connect`) is available.
        let settings = Settings::default();
        let args = build_extra_args(&opencode_cfg(), &settings, &[]);
        assert!(!args.iter().any(|a| a == "--mini"),
            "V20: opencode must NOT get --mini, got: {args:?}");
    }

    #[test]
    fn no_mini_for_any_ai_tab() {
        let settings = Settings::default();
        let claude = build_extra_args(&claude_cfg(), &settings, &[]);
        assert!(!claude.iter().any(|a| a == "--mini"), "claude must not get --mini");
        let opencode = build_extra_args(&opencode_cfg(), &settings, &[]);
        assert!(!opencode.iter().any(|a| a == "--mini"), "opencode must not get --mini in V20");
        // A non-opencode, non-claude AI command must not get --mini either.
        let mut other = claude_cfg();
        other.command = "some-other-tool".to_string();
        let other = build_extra_args(&other, &settings, &[]);
        assert!(!other.iter().any(|a| a == "--mini"), "non-opencode tabs must not get --mini");
    }

    #[test]
    fn opencode_config_content_is_valid_json() {
        let settings = Settings::default();
        let env = compose_ai_env(&opencode_cfg(), &settings);
        let raw = env
            .get("OPENCODE_CONFIG_CONTENT")
            .expect("opencode tab sets OPENCODE_CONFIG_CONTENT");
        let cfg: serde_json::Value =
            serde_json::from_str(raw).expect("OPENCODE_CONFIG_CONTENT is valid JSON");
        assert_eq!(cfg["$schema"], "https://opencode.ai/config.json");
    }

    #[test]
    fn opencode_config_injects_mcp_when_offload_enabled() {
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        let cfg = build_opencode_config(&opencode_cfg(), &settings);
        let cmd = &cfg["mcp"]["cimp-offload"]["command"];
        assert_eq!(cfg["mcp"]["cimp-offload"]["type"], "local");
        // The child is launched with the opencode consumer discriminator.
        let args: Vec<&str> = cmd
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(args.contains(&"--offload-mcp"), "got: {args:?}");
        assert!(args.windows(2).any(|w| w == ["--consumer", "opencode"]), "got: {args:?}");
    }

    #[test]
    fn opencode_config_no_mcp_when_all_off() {
        let settings = Settings::default(); // offload + graph off, no servers
        let cfg = build_opencode_config(&opencode_cfg(), &settings);
        assert!(cfg.get("mcp").is_none(), "no mcp block when nothing is in play");
    }

    #[test]
    fn opencode_config_injects_mcp_when_graph_enabled() {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        let cfg = build_opencode_config(&opencode_cfg(), &settings);
        assert!(cfg["mcp"]["cimp-offload"].is_object(), "graph alone injects the mcp block");
    }

    #[test]
    fn opencode_config_references_instructions_when_guidance_applies() {
        // V20: TTS markup is no longer injected, so the instructions file is
        // referenced only when capability guidance (graph/offload) applies.
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        let cfg = build_opencode_config(&opencode_cfg(), &settings);
        let path = cfg["instructions"][0].as_str().expect("instructions path");
        assert!(path.ends_with(".md"), "got: {path}");
        assert!(path.contains("opencode"), "got: {path}");
    }

    #[test]
    fn opencode_config_no_instructions_when_no_guidance() {
        // V20: default settings (offload + graph off) ⇒ no guidance ⇒ no
        // instructions key, regardless of the (now-vestigial) tts_injection.
        let settings = Settings::default();
        let config = build_opencode_config(&opencode_cfg(), &settings);
        assert!(config.get("instructions").is_none(), "no guidance ⇒ no instructions key");
    }

    #[test]
    fn opencode_config_never_injects_provider() {
        // The single OpenCode tab manages its own providers; cimp never
        // injects a `provider`/`model` block, even with use_local_provider set.
        let settings = Settings::default();
        let mut cfg = opencode_cfg();
        cfg.use_local_provider = true;
        let config = build_opencode_config(&cfg, &settings);
        assert!(config.get("provider").is_none(), "opencode gets no provider block");
        assert!(config.get("model").is_none());
    }

    #[test]
    fn opencode_sets_noise_suppression_env() {
        let settings = Settings::default();
        let env = compose_ai_env(&opencode_cfg(), &settings);
        assert_eq!(
            env.get("OPENCODE_DISABLE_TERMINAL_TITLE").map(String::as_str),
            Some("1"),
        );
        assert!(!env.contains_key("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
            "opencode must not get the Claude fullscreen flag");
    }

    #[test]
    fn per_tab_env_overrides_opencode_config_content() {
        let settings = Settings::default();
        let mut cfg = opencode_cfg();
        cfg.env
            .insert("OPENCODE_CONFIG_CONTENT".to_string(), "custom".to_string());
        let env = compose_ai_env(&cfg, &settings);
        assert_eq!(
            env.get("OPENCODE_CONFIG_CONTENT").map(String::as_str),
            Some("custom"),
            "an explicit per-tab value must win over the synthesized config",
        );
    }

    #[test]
    fn per_tab_env_can_reenable_inline_renderer() {
        // V20: cImp no longer synthesizes the alt-screen opt-out, but a user who
        // wants the old inline renderer can still set it per tab; the per-tab env
        // merge carries it through untouched.
        let settings = Settings::default();
        let mut cfg = claude_cfg();
        cfg.env.insert(
            "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".to_string(),
            "1".to_string(),
        );
        let env = compose_ai_env(&cfg, &settings);
        assert_eq!(
            env.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN").map(String::as_str),
            Some("1"),
            "an explicit per-tab value must pass through the env merge",
        );
    }
}
