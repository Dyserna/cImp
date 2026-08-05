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
    let entry = settings
        .find_tab(id)
        .ok_or_else(|| AppError::Pty(format!("tab {id} has no settings entry")))?;

    match entry {
        TabConfig::AiTool(cfg) => {
            build_ai_tool_spec(tab, cfg, settings, launch_cwd, invocation_args)
        }
        // V14 Phase F: Preview tabs are an embedded child webview, not a
        // subprocess — the frontend never calls `pty_start` for one (see
        // `TabKind::Preview`'s doc comment), so this arm should be
        // unreachable in practice; it exists only so the match stays
        // exhaustive and a stray call fails cleanly instead of panicking.
        TabConfig::Preview(_) => Err(AppError::Pty(format!(
            "tab {id} is a Preview tab — it has no PTY to launch"
        ))),
        TabConfig::Shell(cfg) => {
            // The detection module verified the default Shell-1 binary;
            // user-supplied paths from the New Shell Tab dialog are
            // validated up-front in `create_shell_tab` (M2 Phase B).
            // Bare-name paths still go through `resolve_command` so a
            // settings entry pointing at "bash" picks up the PATH
            // resolution every launch.
            let binary = resolve_command(&cfg.command)?;
            let working_dir = cfg.cwd.clone().unwrap_or_else(|| launch_cwd.to_path_buf());
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
    let pre_args = build_pre_args(cfg, settings, tab.as_str());
    let mut extra_args = build_extra_args(cfg, settings, invocation_args);
    let working_dir = cfg.cwd.clone().unwrap_or_else(|| launch_cwd.to_path_buf());
    // V19: OpenCode reads its guidance from a file referenced in the injected
    // config (`instructions`), so write that managed file at launch — kept off
    // the pure `compose_ai_env` path so the config builder stays test-safe.
    if command_is(&cfg.command, "opencode") {
        write_opencode_instructions(cfg, settings);
        // V10: drop the dependency-free injection/memory plugin into the
        // project's `.opencode/plugin/`, baking in the current loopback port +
        // token. Uses `working_dir` (the project root the TUI opens).
        write_opencode_plugin(&working_dir, settings);
    }
    // V20: resolve the out-of-band TTS source. For OpenCode this also injects
    // the `--port`/`--hostname` the fullscreen TUI hosts its event server on
    // (which the adapter taps). Mutates `extra_args`, so it runs on the real
    // launch path only — the pure `build_extra_args` stays test-stable.
    let oob = resolve_oob_source(cfg, &working_dir, &mut extra_args);
    let env = compose_ai_env(cfg, settings, tab.as_str());
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
        // V16 Feature 1: record the OpenCode version for the harness
        // tripwire. Spawn-time only (the event stream carries no version);
        // fire-and-forget on a plain thread so a slow/hung `--version` can
        // never delay the tab launch.
        note_opencode_version(&cfg.command);
        let port = alloc_loopback_port()?;
        extra_args.push("--port".to_string());
        extra_args.push(port.to_string());
        extra_args.push("--hostname".to_string());
        extra_args.push("127.0.0.1".to_string());
        return Some(crate::oob::OobSpec::OpenCodeEvent { port });
    }
    None
}

/// NC-2 (issue #5): every configured Claude AI tab and the working directory it
/// launches in — `(tab_id, working_dir)`. Resolution mirrors
/// [`build_ai_tool_spec`] exactly (per-tab `cwd` override, else the app's launch
/// dir), so the permission-hook route can compare a hook payload's `cwd`
/// against the directory the tab was actually spawned in.
///
/// Note the usual case is that NO tab sets `cwd`, so every Claude tab shares the
/// launch dir — which is why the route's cwd match is only used as a
/// last-resort tie-break and only when it resolves to exactly one tab.
pub(crate) fn claude_tab_dirs(
    settings: &Settings,
    launch_cwd: &Path,
) -> Vec<(String, std::path::PathBuf)> {
    settings
        .tabs
        .iter()
        .filter_map(|t| match t {
            TabConfig::AiTool(c) if command_is(&c.command, "claude") => Some((
                c.id.clone(),
                c.cwd.clone().unwrap_or_else(|| launch_cwd.to_path_buf()),
            )),
            _ => None,
        })
        .collect()
}

/// V16 Feature 1: run `opencode --version` once per tab spawn and record the
/// first output line into the global `harness_versions` tripwire state.
/// Best-effort in every direction: unresolvable binary, spawn failure, or
/// junk output all just skip the note (`note_harness_version` also ignores
/// empty strings and no-ops on an unchanged version).
fn note_opencode_version(command: &str) {
    let Ok(binary) = resolve_command(command) else {
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
        let Ok(out) = cmd.output() else {
            return;
        };
        // `opencode --version` prints a bare version (e.g. "1.4.2"); take the
        // first line defensively in case a future build adds a banner.
        let version = String::from_utf8_lossy(&out.stdout);
        let version = version.lines().next().unwrap_or("").trim().to_string();
        crate::settings::note_harness_version("opencode", &version);
    });
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

/// The four "is this MCP server advertised to this consumer" gates, factored
/// out so the injection sites below and the restart-hint edge detector in
/// `ipc::commands::settings_update` can never drift apart. Servers are
/// injected only at TAB SPAWN (`--mcp-config` / `OPENCODE_CONFIG_CONTENT`),
/// so a running AI tab keeps its old server set until restarted — any edit
/// that flips one of these must surface a restart hint.
pub(crate) fn advertises_offload_to_claude(s: &Settings) -> bool {
    s.offload.enabled || s.graph.enabled || s.offload.any_claude_mcp()
}

pub(crate) fn advertises_audit_to_claude(s: &Settings) -> bool {
    s.code_audit.enabled && s.code_audit.expose_claude
}

pub(crate) fn advertises_offload_to_opencode(s: &Settings) -> bool {
    s.offload.enabled || s.graph.enabled || s.offload.any_opencode_mcp()
}

pub(crate) fn advertises_audit_to_opencode(s: &Settings) -> bool {
    s.code_audit.enabled && s.code_audit.expose_opencode
}

/// Per-consumer spawn-injection signature — `[claude, opencode]`. Captures
/// every Settings-derived input that reaches an AI tab only at spawn (the
/// `--mcp-config` server set, the `compose_capability_guidance` gates, the
/// `--settings` statusline/hooks overlay, the `claude_local` env for
/// local-provider tabs, the OpenCode plugin's baked flags and the injected
/// `local-llama` provider). Compared across a Settings save to decide whether
/// a "restart the AI tab" hint is due. Coarse by design: any difference means
/// a fresh tab would be launched differently from the one still running.
pub(crate) fn spawn_inject_sig(s: &Settings) -> [serde_json::Value; 2] {
    // Guidance addendum gates, shared verbatim by both consumers (Claude's
    // `--append-system-prompt` and OpenCode's managed instructions file).
    let guidance = serde_json::json!([
        s.offload.enabled && s.offload.inject_guidance,
        s.graph.enabled,
        s.graph.enabled && s.graph.semantic_search,
        s.graph.enabled && s.graph.promote_pinned_facts,
    ]);
    let read_hook = s.graph.enabled && s.graph.read_advisor && !s.harness_versions.e1_blocked();
    let post_edit = s.graph.enabled && s.graph.auto_check && !s.checks.is_empty();
    // `claude_local` env vars are synthesized at spawn, but only for Claude
    // tabs that opted in — irrelevant edits shouldn't nag.
    let local_env = s
        .tabs
        .iter()
        .any(|t| {
            matches!(t, TabConfig::AiTool(c)
                if c.use_local_provider && command_is(&c.command, "claude"))
        })
        .then(|| {
            serde_json::json!([
                s.claude_local.base_url,
                s.claude_local.auth_token,
                s.claude_local.model_alias,
            ])
        });
    let claude = serde_json::json!({
        "mcp": [advertises_offload_to_claude(s), advertises_audit_to_claude(s)],
        "guidance": guidance.clone(),
        "statusline": s.statusline.enabled,
        // The `--settings` hooks overlay gates, in `build_pre_args` order:
        // UserPromptSubmit, PreCompact, PreToolUse Read, PreToolUse Bash,
        // PostToolUse auto-check.
        //
        // NC-2: the `Notification` / `PermissionDenied` hooks are NOT listed —
        // they are injected unconditionally for every Claude tab and derive
        // from no Settings field, so no save can make a running tab differ from
        // a fresh one and there is nothing a restart hint could be about. Any
        // future *gated* hook must add an entry here.
        "hooks": [
            s.graph.enabled && (s.graph.context_injection || s.workbench.checkpoints),
            s.graph.enabled && s.graph.context_injection && s.graph.compaction_context,
            read_hook,
            read_hook && s.graph.read_advisor_shell,
            post_edit,
        ],
        "local_env": local_env,
    });
    let opencode = serde_json::json!({
        "mcp": [advertises_offload_to_opencode(s), advertises_audit_to_opencode(s)],
        "guidance": guidance,
        // `write_opencode_plugin` inputs: plugin presence + its baked
        // CIMP_INJECT_ENABLED / CIMP_AUTO_CHECK_ENABLED flags.
        "plugin": [
            s.graph.enabled,
            s.graph.enabled && s.graph.context_injection,
            post_edit,
        ],
        // The injected `local-llama` provider block (`build_opencode_config`).
        "provider": s
            .offload
            .resolve_opencode_provider()
            .map(|p| serde_json::json!([p.base_url, p.model, p.api_key])),
    });
    [claude, opencode]
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
///
/// V28: `tab` is the launching tab's id, baked into the `cimp-offload` MCP
/// child's argv (`--tab <id>`) so the app can resolve which of this agent's
/// sessions a `context_*` call belongs to.
fn build_pre_args(cfg: &AiToolTabConfig, settings: &Settings, tab: &str) -> Vec<String> {
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

    // A single `--settings` overlay carrying every session-scoped Claude Code
    // setting cImp injects: the `statusLine` renderer (gated on
    // `statusline.enabled`) and the V10 `UserPromptSubmit` context-injection
    // hook (gated on `graph.enabled && graph.context_injection`). Merged into
    // one object so we never rely on Claude concatenating repeated `--settings`,
    // and it layers over the user's own settings without touching `~/.claude`.
    {
        let mut overlay = serde_json::Map::new();
        if settings.statusline.enabled {
            if let Some(command) = crate::statusline::launch_command() {
                // `refreshInterval` (seconds) re-runs the command on a timer in
                // addition to event-driven updates, so the `rate_limits` usage
                // push (see `crate::usage`) keeps flowing — and the bottom-bar
                // widget stays fresh — while the tab sits idle. 30s beats the
                // widget's 90s stale threshold with margin; the render itself
                // is a local subprocess, so the cost is negligible.
                overlay.insert(
                    "statusLine".to_string(),
                    serde_json::json!({
                        "type": "command",
                        "command": command,
                        "refreshInterval": 30,
                    }),
                );
            }
        }
        // Accumulate Claude hook entries (UserPromptSubmit context injection,
        // V11 PreCompact compaction survival, V11 PreToolUse read advisor) into
        // one `hooks` object — each entry is installed only when its gate is on.
        {
            let mut hooks = serde_json::Map::new();
            // V13 Phase C: widened from `context_injection` alone so the
            // prompt-tap checkpoint trigger (`workbench::on_prompt`, called
            // from the `/context/retrieve` handler BEFORE its own injection
            // gate) still runs when the user wants checkpoints but has
            // injection off — the milestone's Decision 4. The retrieve
            // handler's own *injection* gate is unaffected by this; it stays
            // on `context_injection` alone.
            if settings.graph.enabled
                && (settings.graph.context_injection || settings.workbench.checkpoints)
            {
                if let Some(command) = crate::statusline::context_hook_command() {
                    hooks.insert(
                        "UserPromptSubmit".to_string(),
                        serde_json::json!([ { "hooks": [
                            { "type": "command", "command": command, "timeout": 5 }
                        ] } ]),
                    );
                }
            }
            // V11 Phase D: PreCompact — carry the working set through a
            // compaction. Kept on its own narrower condition (still requires
            // injection, unlike the widened UserPromptSubmit hook above) —
            // compaction survival is meaningless without injection to feed.
            if settings.graph.enabled
                && settings.graph.context_injection
                && settings.graph.compaction_context
            {
                if let Some(command) = crate::statusline::hook_command("--precompact-hook") {
                    hooks.insert(
                        "PreCompact".to_string(),
                        serde_json::json!([ { "hooks": [
                            { "type": "command", "command": command, "timeout": 5 }
                        ] } ]),
                    );
                }
            }
            // V11 Phase E: PreToolUse read advisor (opt-in; independent of the
            // injection toggle, but still needs the graph). Matches only `Read`.
            // V16 Feature 0: a recorded E1 spike FAILURE (the deny reason
            // never reaches the model — every remind would be a bare
            // refusal) hard-blocks the read advisor regardless of the
            // toggle; the Settings UI renders the block disabled with the
            // same condition. `e1_blocked` fails closed on unrecognized
            // hand-typed values; the registry refreshes `harness_versions`
            // from the physical global file at spawn, so a hand-recorded
            // outcome takes effect on the next tab launch, not the next app
            // restart.
            if settings.graph.enabled
                && settings.graph.read_advisor
                && !settings.harness_versions.e1_blocked()
            {
                if let Some(command) = crate::statusline::hook_command("--read-hook") {
                    let mut entries = vec![serde_json::json!({
                        "matcher": "Read",
                        "hooks": [ { "type": "command", "command": command.clone(), "timeout": 5 } ]
                    })];
                    // V17 Phase B: a second matcher intercepts a whole-file shell
                    // read (`cat FILE`) of an already-read file via the SAME
                    // `--read-hook` shim (which dispatches on `tool_name`). Gated
                    // on the sub-toggle, so it's a zero overlay delta when off.
                    if settings.graph.read_advisor_shell {
                        entries.push(serde_json::json!({
                            "matcher": "Bash",
                            "hooks": [ { "type": "command", "command": command.clone(), "timeout": 5 } ]
                        }));
                    }
                    hooks.insert("PreToolUse".to_string(), serde_json::Value::Array(entries));
                }
            }
            // V12 Phase F (6a/6b): PostToolUse auto-check after an edit — opt-in
            // (behavior hook), needs the graph AND at least one configured check
            // (nothing to run otherwise). Matches the edit-class tools.
            if settings.graph.enabled && settings.graph.auto_check && !settings.checks.is_empty() {
                if let Some(command) = crate::statusline::hook_command("--postedit-hook") {
                    hooks.insert(
                        "PostToolUse".to_string(),
                        serde_json::json!([ { "matcher": "Edit|Write|MultiEdit", "hooks": [
                            { "type": "command", "command": command, "timeout": 5 }
                        ] } ]),
                    );
                }
            }
            // NC-2 (issue #5): `Notification` + `PermissionDenied` — the
            // PRIMARY "this tab is awaiting a permission decision" detector,
            // demoting the TUI-regex matcher (`processing::permission`) to
            // fallback. Both point at the SAME `--notify-hook` shim, which
            // dispatches on the payload's `hook_event_name`.
            //
            // UNCONDITIONAL for every Claude tab: there is no toggle, so there
            // is nothing a Settings save could flip — which is exactly why this
            // needs no `spawn_inject_sig` entry / restart hint (same reasoning
            // as the unconditional `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` env
            // injection below and the pinned OpenCode `subagent_depth`). The
            // shim is fail-open and only spawns when Claude actually surfaces a
            // notification or the auto-classifier denies a call — both rare.
            //
            // `"matcher": ""` on BOTH entries — the docs' explicit "fires on
            // all notification types" form (and, for `PermissionDenied`, all
            // tool names). Deliberately NOT a narrowing `permission_prompt`
            // matcher: the matcher filters on the notification TYPE, and a
            // renamed/removed type would silently stop the hook firing. We take
            // every notification and classify app-side in
            // `offload::loopback::classify_permission_event` instead, so an
            // unrecognized type degrades to "ignored", never to silence. An
            // *absent* matcher key is documented only for events that don't
            // support matchers at all, so the empty string is the safe spelling
            // here. Idle/`idle_prompt` notifications are deliberately NOT wired
            // to the `awaiting_question` pipe — see that classifier's doc.
            if let Some(command) = crate::statusline::hook_command("--notify-hook") {
                for event in ["Notification", "PermissionDenied"] {
                    hooks.insert(
                        event.to_string(),
                        serde_json::json!([ { "matcher": "", "hooks": [
                            { "type": "command", "command": command.clone(), "timeout": 5 }
                        ] } ]),
                    );
                }
            }
            if !hooks.is_empty() {
                overlay.insert("hooks".to_string(), serde_json::Value::Object(hooks));
            }
        }
        if !overlay.is_empty() {
            args.push("--settings".to_string());
            args.push(serde_json::Value::Object(overlay).to_string());
        }
    }

    // Point Claude at our own stdio MCP servers via a session-scoped
    // `--mcp-config` overlay, never written to `~/.claude`. Claude spawns each
    // `command` as argv (no shell), so the raw exe path is correct — no
    // shell-quoting needed (unlike the statusLine command). Up to two servers
    // ride the same overlay, each under its own gate:
    //
    //   - V8-01 + V9-01 `cimp-offload` (`--offload-mcp`) carries the
    //     `offload_task` tool, the `graph_*` tools, AND any MCP server exposed
    //     to Claude Code. Injected whenever ANY of those is in play — the graph
    //     tools and Claude-exposed MCP servers must reach Claude even when
    //     offload is disabled (they ride the same MCP child).
    //   - V26 `cimp-code-audit` (`--code-audit-mcp`) carries `security_audit` /
    //     `quality_audit`. Injected when Code Audit is enabled AND opted in for
    //     the Claude consumer (`code_audit.expose_claude`).
    //
    // The `--mcp-config` flag is emitted only if at least one server made the
    // cut, so behavior is unchanged (no flag) when every gate is off.
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.to_string_lossy().to_string();
        let mut servers = serde_json::Map::new();
        if advertises_offload_to_claude(settings) {
            servers.insert(
                "cimp-offload".to_string(),
                serde_json::json!({
                    "command": exe,
                    // V28 (issue #13): `--tab <id>` binds this per-tab child to
                    // the tab that spawned it. The child forwards it on
                    // `/graph_run`, where the app resolves it against the
                    // live-session registry — that is what stops two Claude tabs
                    // on one project from sharing (and stealing) a memory scope.
                    // Unconditional and NOT Settings-derived, so it needs no
                    // `spawn_inject_sig` entry / restart hint (same reasoning as
                    // the `Notification` hook above).
                    "args": ["--offload-mcp", "--tab", tab]
                }),
            );
        }
        if advertises_audit_to_claude(settings) {
            servers.insert(
                "cimp-code-audit".to_string(),
                serde_json::json!({
                    "command": exe,
                    "args": ["--code-audit-mcp"]
                }),
            );
        }
        if !servers.is_empty() {
            let mcp = serde_json::json!({ "mcpServers": servers });
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
/// injection toggle (the former free-text `instructions` field is gone).
///
/// V12 Phase E: when `graph.promote_pinned_facts` is on, a marked
/// `## cImp project facts` block of PINNED facts is appended last (see
/// [`fact_promotion_block`]) — launch-time only, best-effort.
fn compose_capability_guidance(cfg: &AiToolTabConfig, settings: &Settings) -> String {
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
    if settings.graph.enabled && settings.graph.promote_pinned_facts {
        // `cfg.cwd` is the tab's configured project root; when unset the real
        // launch falls back to the launch directory (`std::env::current_dir()`
        // at the time `main` computed `launch_cwd` — see `main.rs`), which is
        // the same value this fallback reproduces without threading a root
        // parameter through every caller (most of which have no reason to
        // otherwise take one, and are exercised by many existing tests).
        let root = cfg
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if let Some(block) = fact_promotion_block(&root, settings) {
            if !addendum.is_empty() {
                addendum.push_str("\n\n");
            }
            addendum.push_str(&block);
        }
    }
    addendum
}

/// V12 Phase E: the `## cImp project facts` launch-time addendum — PINNED
/// facts only, newest-pinned first, capped ~1500 chars. `None` when the graph
/// hasn't been built at `root` yet, has no pinned facts, or can't be opened —
/// best-effort, same posture as this module's other launch-time I/O (e.g.
/// [`write_opencode_instructions`]).
fn fact_promotion_block(root: &Path, settings: &Settings) -> Option<String> {
    const CAP_CHARS: usize = 1500;
    let sub = settings.graph.effective_db_subdir();
    let idx = crate::graph::GraphIndex::open_existing(root, &sub).ok()?;
    let mut pinned: Vec<_> = idx
        .list_project_facts(false, 200)
        .ok()?
        .into_iter()
        .filter(|f| f.pinned)
        .collect();
    if pinned.is_empty() {
        return None;
    }
    // `list_project_facts` already returns pinned-first/newest, but the
    // pinned-only filter above could in principle be fed a differently-sorted
    // source later — sort explicitly here so "newest-pinned first" holds
    // regardless.
    pinned.sort_by_key(|f| std::cmp::Reverse(f.ts_ms));
    let mut out = String::from("## cImp project facts\n");
    for f in &pinned {
        let line = format!("- {}\n", f.text);
        if out.len() + line.len() > CAP_CHARS {
            break;
        }
        out.push_str(&line);
    }
    Some(out)
}

/// V8-01: the system-prompt addendum telling Opus *when* to reach for
/// `offload_task`. Without this nudge the model rarely offloads. Gated by
/// `offload.inject_guidance`.
const OFFLOAD_GUIDANCE: &str =
    "You have an `offload_task` tool (from the cimp-offload MCP server) \
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
`graph_references`, `graph_imports`, `graph_outline` (a file's definitions), `graph_snippet` \
(fetch just one definition's body instead of reading the whole file — for files over ~300 lines \
prefer `graph_outline` → `graph_snippet` over a full Read), `graph_transitive` \
(transitive call chains), `graph_search_docs` (documentation/doc-comments), and \
`graph_struct_search` (find code by AST shape via a tree-sitter query — e.g. every `.unwrap()` or \
every function with a given parameter pattern — when text search can't express the structure). They \
return precise, token-bounded results from an index, so they're cheaper and more exact than text \
search for 'where is X defined', 'who calls X', and impact analysis. `graph_dead_exports` lists \
candidate unused public symbols and `graph_cycles` lists import cycles. For the edit→check→fix \
loop: before changing shared code run `graph_impact` (what your working-tree diff could break) and \
`graph_tests_for` (which tests cover a symbol); after edits run `run_check {changed_only:true}` for \
deduplicated diagnostics instead of a raw build dump — including test runs: prefer a configured \
test check over running the test command in Bash; it returns failures only; `graph_recent_changes` \
shows what's been churning lately. This project also has \
session memory: call `context_recall` at the start of a follow-up task to reload what this session \
has been working on, and `context_note` to record a non-obvious decision (pin=true to keep it \
across sessions) so it survives into later sessions.";

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
    //
    // D-8 (maintenance 2026-08-04): cImp does not merely decline to inject
    // `--mini` — it must actively strip a user-supplied one from an OpenCode
    // tab's stored `args`. `resolve_oob_source` unconditionally appends
    // `--port <N> --hostname 127.0.0.1` to every OpenCode launch (that port is
    // the TTS event tap), and `opencode --mini --port N` HARD-FAILS: the two
    // flags are mutually exclusive. So the combination is reachable — the
    // v19→v20 migration stripped `--mini` from stored args once, but nothing
    // stops it coming back via a hand-edited settings file, a `.cimp.custom.
    // config.json` overlay, or a settings file carried over from another
    // machine. Dropping it keeps the tab launchable (the flag is inert under
    // V20 anyway) instead of handing the user an opaque OpenCode usage error;
    // the launch log records the correction.
    let mini_guard = command_is(&cfg.command, "opencode");
    for arg in cfg.args.iter().filter(|s| !s.is_empty()) {
        if mini_guard && is_mini_flag(arg) {
            tracing::warn!(
                tab = %cfg.id,
                arg = %arg,
                "opencode tab: dropping `--mini` from args — cImp launches OpenCode \
                 fullscreen with `--port` for the TTS event tap, and OpenCode rejects \
                 `--mini` combined with `--port`. Remove it from the tab's args.",
            );
            continue;
        }
        out.push(arg.clone());
    }

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

/// D-8: does `arg` set OpenCode's `--mini` flag? Matches the bare flag and the
/// `--mini=<value>` form (clap accepts both for a bool flag), so neither
/// spelling can survive into a launch that also carries `--port`.
fn is_mini_flag(arg: &str) -> bool {
    arg == "--mini" || arg.starts_with("--mini=")
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

/// V10: write (or remove) the OpenCode injection/memory plugin in the project's
/// `.opencode/plugin/cimp-inject.js`. The plugin is dependency-free (node
/// builtins + global `fetch`, so OpenCode does not run a launch-time
/// `bun install`) and bakes in the current loopback port + token — regenerated
/// each launch since the token rotates per app run (idempotent overwrite). It
/// serves two hooks:
///   * `chat.message` → POST the prompt to `/context/retrieve` and append the
///     digest **in place** on the existing text part (schema-safe; verified in
///     the D0 spike), gated by the baked-in inject flag; and
///   * `tool.execute.after` → POST to `/memory/event` (the sole memory ingress
///     for OpenCode, whose OOB SSE stream carries no tool events).
///
/// Removed when the graph is off (nothing to inject or record). Also adds
/// `.opencode/` to the project's `.git/info/exclude` so the generated plugin and
/// OpenCode's own `.opencode/.gitignore` don't dirty `git status`.
fn write_opencode_plugin(working_dir: &Path, settings: &Settings) {
    let plugin_path = working_dir
        .join(".opencode")
        .join("plugin")
        .join("cimp-inject.js");

    // No graph → nothing to inject or record; clean up a stale plugin.
    if !settings.graph.enabled {
        let _ = std::fs::remove_file(&plugin_path);
        return;
    }
    // Need the loopback endpoint to reach the app; without it, skip (and
    // clean). This runs IN the app at tab spawn, so it must bake THIS
    // instance's endpoint — `read_own_discovery` (pid-keyed), never the
    // shared last-writer-wins file a sibling instance may have overwritten.
    let Some(disc) = crate::offload::loopback::read_own_discovery() else {
        let _ = std::fs::remove_file(&plugin_path);
        return;
    };

    let inject_enabled = settings.graph.context_injection;
    // V12 Phase F (6a/6b): same gate as the Claude PostToolUse hook — auto-check
    // needs the graph AND at least one configured check.
    let auto_check_enabled = settings.graph.auto_check && !settings.checks.is_empty();
    let js = opencode_plugin_source(disc.port, &disc.token, inject_enabled, auto_check_enabled);

    if let Some(dir) = plugin_path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let _ = std::fs::write(&plugin_path, js);
    git_exclude_opencode(working_dir);
}

/// The dependency-free OpenCode plugin source, with the loopback port + token
/// and the inject/auto-check flags baked in.
fn opencode_plugin_source(
    port: u16,
    token: &str,
    inject_enabled: bool,
    auto_check_enabled: bool,
) -> String {
    format!(
        r#"// Generated by cImp (V10 Code Intelligence). Do not edit — regenerated each launch.
const CIMP_LOOPBACK = "http://127.0.0.1:{port}";
const CIMP_TOKEN = "{token}";
const CIMP_INJECT_ENABLED = {inject};
const CIMP_AUTO_CHECK_ENABLED = {auto_check};
const CIMP_EDIT_TOOLS = new Set(["edit", "write", "patch"]);

// V24 Phase F: child session id -> parent session id, learned from
// `session.created` events. Sub-agent (task-tool) sessions are always created
// while this plugin is running, so a session-lifetime Map is sufficient; usage
// POSTs from a session in this map carry `parent_session_id` so the backend can
// roll the spend up to the parent (mirrors the Claude sub-agent contract).
const CIMP_PARENTS = new Map();

export default async (input) => ({{
  // V13 Phase C: this POST always fires (not gated on CIMP_INJECT_ENABLED)
  // so the app-side prompt-tap checkpoint trigger sees every prompt even
  // when injection is off — only APPLYING the returned text to the draft is
  // gated. Mirrors the Claude `--context-hook` shim, which always POSTs too.
  "chat.message": async (inp, out) => {{
    const p = out.parts.find((x) => x.type === "text");
    if (!p || !p.text) return;
    try {{
      const r = await fetch(CIMP_LOOPBACK + "/context/retrieve", {{
        method: "POST",
        headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
        body: JSON.stringify({{ cwd: input.directory, prompt: p.text, session_id: inp.sessionID, agent: "opencode" }}),
        signal: AbortSignal.timeout(600),
      }});
      const j = await r.json();
      if (CIMP_INJECT_ENABLED && j && j.ok && j.text) p.text += "\n\n" + j.text;
    }} catch (_e) {{}}
  }},
  "tool.execute.after": async (inp) => {{
    try {{
      const body = {{
        cwd: input.directory,
        session_id: inp.sessionID,
        agent: "opencode",
        tool: inp.tool,
        args: inp.args,
      }};
      // V24 Phase F: a task-tool CHILD (sub-agent) session stamps its parent so
      // the backend mirrors the Claude sidechain contract — the child's tool
      // events are dropped and only the parent is marked live (the child's real
      // token spend rolls up to the parent via the `event` usage hook below).
      const parent = CIMP_PARENTS.get(inp.sessionID);
      if (parent) body.parent_session_id = parent;
      await fetch(CIMP_LOOPBACK + "/memory/event", {{
        method: "POST",
        headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(600),
      }});
    }} catch (_e) {{}}
    // V12 Phase F (6a/6b): best-effort, fire-and-forget — OpenCode's hook
    // return value isn't verified to carry context back to the model (the F0
    // spike scope), so this doesn't await/use the response; the server-side
    // debounce/diff/park still runs, and a parked block reaches the model via
    // the next `chat.message` retrieve above.
    if (CIMP_AUTO_CHECK_ENABLED && CIMP_EDIT_TOOLS.has(inp.tool)) {{
      const filePath = (inp.args && (inp.args.filePath || inp.args.path)) || "";
      fetch(CIMP_LOOPBACK + "/context/post_edit", {{
        method: "POST",
        headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
        body: JSON.stringify({{
          cwd: input.directory,
          session_id: inp.sessionID,
          file_path: filePath,
          tool_name: inp.tool,
        }}),
        signal: AbortSignal.timeout(600),
      }}).catch((_e) => {{}});
    }}
  }},
  // V24 Phase F: OpenCode's real per-turn token tap (spike-confirmed against
  // 1.18.1). `chat.message` fires on the USER prompt and carries no tokens, so
  // forwarding happens here on the assistant `message.updated` event once the
  // turn has completed. The message is emitted first with zero tokens, then
  // re-emitted (duplicated) with final tokens — gate on `time.completed` and
  // let the backend upsert-by-msg_id absorb the duplicate. Never gated on the
  // inject/auto-check flags (usage is always recorded); best-effort, non-blocking.
  event: async ({{ event }}) => {{
    try {{
      if (!event) return;
      const info = event.properties && event.properties.info;
      if (event.type === "session.created") {{
        // A sub-agent (task-tool) child session announces its parent here.
        if (info && info.id && info.parentID) CIMP_PARENTS.set(info.id, info.parentID);
        return;
      }}
      if (event.type !== "message.updated") return;
      if (!info || info.role !== "assistant" || !info.time || !info.time.completed) return;
      const tok = info.tokens || {{}};
      const cache = tok.cache || {{}};
      const body = {{
        cwd: input.directory,
        agent: "opencode",
        kind: "usage",
        session_id: info.sessionID,
        msg_id: info.id,
        // Bare modelID (no providerID) — consistent with how Claude sessions
        // store bare model ids and with matchPricing's model_prefix semantics.
        model: info.modelID,
        in_tok: (tok.input || 0),
        // reasoning folds into output (priced as output everywhere it matters).
        out_tok: ((tok.output || 0) + (tok.reasoning || 0)),
        cache_read: (cache.read || 0),
        cache_make: (cache.write || 0),
      }};
      const parent = CIMP_PARENTS.get(info.sessionID);
      if (parent) body.parent_session_id = parent;
      await fetch(CIMP_LOOPBACK + "/memory/event", {{
        method: "POST",
        headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(600),
      }});
    }} catch (_e) {{}}
  }},
}});
"#,
        port = port,
        token = token,
        inject = if inject_enabled { "true" } else { "false" },
        auto_check = if auto_check_enabled { "true" } else { "false" },
    )
}

/// Best-effort: add `.opencode/` to `<project>/.git/info/exclude` so the
/// generated plugin (and OpenCode's own `.opencode/.gitignore`) don't show up in
/// `git status`. No-op when there's no `.git` dir or the line is already present.
fn git_exclude_opencode(working_dir: &Path) {
    let info_dir = working_dir.join(".git").join("info");
    if !info_dir.is_dir() {
        return; // not a git repo (or a worktree/submodule shape we won't touch)
    }
    let exclude = info_dir.join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".opencode/") {
        return;
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(".opencode/\n");
    let _ = std::fs::write(&exclude, next);
}

/// V19: synthesize OpenCode's session-scoped config — the JSON document that
/// `OPENCODE_CONFIG_CONTENT` carries (the env-var analog of Claude's
/// `--mcp-config` / `--settings` / `--append-system-prompt`):
///
/// - `$schema` marker.
/// - `subagent_depth: 2` (D-8) — pins nested-subagent behavior across the
///   OpenCode 1.18.2 default change (see the injection site).
/// - `mcp.cimp-offload` → `cimp --offload-mcp --consumer opencode`, injected
///   whenever offload, the graph, or an OpenCode-exposed MCP server is in play
///   (mirrors the Claude `--mcp-config` gate in `build_pre_args`).
/// - V26 `mcp.cimp-code-audit` → `cimp --code-audit-mcp --consumer opencode`,
///   injected when Code Audit is enabled AND opted in for the OpenCode consumer
///   (`code_audit.expose_opencode`). OpenCode caches `tools/list` at connect, so
///   flipping the flag needs a tab restart to take effect (known caveat).
/// - `instructions` → the managed guidance file (TTS + offload + graph), when
///   any guidance applies. The file content is written on the launch path; here
///   we only reference its (deterministic) path.
///
/// - `provider.local-llama` + a default `model` → injected only when the user
///   has registered the local `llama-server` as an OpenCode provider (Offload
///   settings "Add to OpenCode", or auto-sync). Otherwise omitted, leaving
///   provider/model selection entirely to OpenCode's own global config.
///
/// Additive by default — cimp does not set `OPENCODE_DISABLE_PROJECT_CONFIG`,
/// so a user's project config still merges underneath. Pure: no filesystem I/O.
///
/// V28: `tab` is the launching tab's id, appended to the `cimp-offload` child's
/// argv as `--tab <id>` (the OpenCode-side mirror of the Claude `--mcp-config`
/// injection) so `context_*` calls resolve to THIS tab's session.
fn build_opencode_config(
    cfg: &AiToolTabConfig,
    settings: &Settings,
    tab: &str,
) -> serde_json::Value {
    let mut config = serde_json::Map::new();
    config.insert(
        "$schema".to_string(),
        serde_json::Value::String("https://opencode.ai/config.json".to_string()),
    );

    // D-8 (maintenance 2026-08-04): pin `subagent_depth`. OpenCode 1.18.2
    // introduced this key with a default of **1**, which lets a primary agent
    // launch subagents but blocks those subagents from launching their own —
    // a silent behavior change for any workflow that nested before. cImp's
    // installed OpenCode predates it, so upgrading would quietly break nesting
    // unless the injected config states an intent. `2` restores one level of
    // nesting (the pre-1.18.2 shape) without going unbounded.
    //
    // Deliberately a CONSTANT, not a setting: `spawn_inject_sig` only needs an
    // entry for Settings-derived spawn injections, and a constant can never
    // differ between a running tab and a fresh one.
    //
    // Key verified 2026-08-04 against both https://opencode.ai/docs/config/
    // ("You can control how deeply subagents can invoke other subagents using
    // the `subagent_depth` option… The default is 1") and the live schema at
    // https://opencode.ai/config.json (top-level integer, minimum 0,
    // "Maximum subagent nesting depth. Defaults to 1, which prevents subagents
    // from launching subagents."). Additive, so a user's own project config
    // still merges underneath and can override it.
    config.insert("subagent_depth".to_string(), serde_json::Value::from(2u64));

    // Build the `mcp` object from up to two stdio children, each under its own
    // gate (mirrors the two-server `--mcp-config` map in `build_pre_args`):
    //   - `cimp-offload` carries `offload_task`, the `graph_*` tools, and any
    //     OpenCode-exposed MCP server — injected whenever ANY of those is in
    //     play.
    //   - V26 `cimp-code-audit` carries `security_audit` / `quality_audit` —
    //     injected when Code Audit is enabled AND `expose_opencode` is on.
    // The `mcp` key is emitted only if at least one server made the cut, so an
    // all-gates-off config omits it exactly as before.
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.to_string_lossy().to_string();
        let mut mcp = serde_json::Map::new();
        if advertises_offload_to_opencode(settings) {
            mcp.insert(
                "cimp-offload".to_string(),
                serde_json::json!({
                    "type": "local",
                    // V28: see the Claude-side `--tab` note in `build_pre_args`.
                    "command": [exe, "--offload-mcp", "--consumer", "opencode", "--tab", tab]
                }),
            );
        }
        if advertises_audit_to_opencode(settings) {
            mcp.insert(
                "cimp-code-audit".to_string(),
                serde_json::json!({
                    "type": "local",
                    "command": [exe, "--code-audit-mcp", "--consumer", "opencode"]
                }),
            );
        }
        if !mcp.is_empty() {
            config.insert("mcp".to_string(), serde_json::Value::Object(mcp));
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

    // V21: inject the `local-llama` custom provider + select it as the default
    // `model` when one has been registered (Offload settings "Add to OpenCode",
    // or kept in sync by auto-sync). The OpenCode tab still uses OpenCode's own
    // global providers for everything else; this only adds the local
    // `llama-server`'s OpenAI-compatible endpoint and points `model` at it so a
    // freshly opened tab is ready to work. `None` ⇒ no `provider`/`model` keys,
    // exactly as before (default install / never registered).
    if let Some(provider) = settings.offload.resolve_opencode_provider() {
        if !provider.base_url.is_empty() && !provider.model.is_empty() {
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String(provider.base_url),
            );
            if !provider.api_key.is_empty() {
                options.insert(
                    "apiKey".to_string(),
                    serde_json::Value::String(provider.api_key),
                );
            }
            let mut models = serde_json::Map::new();
            models.insert(
                provider.model.clone(),
                serde_json::json!({ "name": provider.model.clone() }),
            );
            config.insert(
                "provider".to_string(),
                serde_json::json!({
                    "local-llama": {
                        "npm": "@ai-sdk/openai-compatible",
                        "name": "Local Llama (cImp offload)",
                        "options": serde_json::Value::Object(options),
                        "models": serde_json::Value::Object(models),
                    }
                }),
            );
            config.insert(
                "model".to_string(),
                serde_json::Value::String(format!("local-llama/{}", provider.model)),
            );
        }
    }
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
///
/// V28: `tab` is passed straight through to [`build_opencode_config`], which
/// bakes it into the `cimp-offload` child's argv.
fn compose_ai_env(
    cfg: &AiToolTabConfig,
    settings: &Settings,
    tab: &str,
) -> HashMap<String, String> {
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
        let config = build_opencode_config(cfg, settings, tab);
        env.insert("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string());
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

    // Maintenance D-2 (2026-08-04): Claude Code ≥ 2.1.212 auto-backgrounds any
    // MCP call that runs past ~2 minutes — the tool returns a task id straight
    // away and the real result arrives later as a notification. cImp's loopback
    // proxy and offload/audit result handling assume a synchronous MCP return,
    // and several `cimp-offload` tools (offload_task, offload_batch,
    // security_audit, quality_audit, graph indexing) routinely exceed that, so
    // auto-backgrounding would silently truncate them. `0` disables the
    // behaviour outright. Unconditional for every Claude tab (not a user
    // setting, so no `spawn_inject_sig` entry / restart hint is needed) and
    // deliberately outside the `use_local_provider` gate above. Revisit when
    // out-of-band MCP completion lands (issue #15 / NC-4).
    if command_is(&cfg.command, "claude") {
        env.insert(
            "CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS".to_string(),
            "0".to_string(),
        );
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
        let args = build_pre_args(&claude_cfg(), &settings, "claude");

        let overlay = settings_overlay(&args).expect("statusLine overlay present");
        assert_eq!(overlay["statusLine"]["type"], "command");
        // Idle-refresh timer that keeps the usage push (and the bottom-bar
        // widget) alive between turns; must stay under usage::STALE_AFTER.
        assert_eq!(overlay["statusLine"]["refreshInterval"], 30);
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
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        // NC-2: an overlay is always emitted for a Claude tab now (the
        // unconditional permission hooks), so the assertion is about the
        // statusLine key, not about the overlay's existence.
        let overlay = settings_overlay(&args).expect("overlay present");
        assert!(overlay.get("statusLine").is_none());
    }

    #[test]
    fn context_hook_overlay_injected_when_injection_on() {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.context_injection = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let overlay = settings_overlay(&args).expect("overlay present");
        let cmd = overlay["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .expect("hook command is a string");
        assert!(cmd.ends_with(" --context-hook"), "got: {cmd}");
        assert!(!cmd.contains('\\'), "path must be forward-slashed: {cmd}");
    }

    #[test]
    fn no_context_hook_when_injection_off() {
        let mut settings = Settings::default();
        settings.statusline.enabled = false;
        settings.graph.enabled = true;
        settings.graph.context_injection = false;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        // Graph on but injection off + statusline off + checkpoints off →
        // no UserPromptSubmit hook (the overlay itself still carries the
        // unconditional NC-2 permission hooks).
        let overlay = settings_overlay(&args).expect("overlay present");
        assert!(overlay["hooks"].get("UserPromptSubmit").is_none());
        assert!(overlay.get("statusLine").is_none());
    }

    /// V16 Feature 0: the read-advisor PreToolUse hook installs when the
    /// graph + toggle are on and the E1 contract isn't recorded as failed —
    /// and a recorded `e1_status == "fail"` hard-blocks it REGARDLESS of
    /// the toggle (a deny whose reason never reaches the model is a bare
    /// refusal; worse than no advisor).
    #[test]
    fn read_hook_overlay_gated_on_toggle_and_e1_status() {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.read_advisor = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let overlay = settings_overlay(&args).expect("overlay present");
        let cmd = overlay["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("hook command is a string");
        assert!(cmd.ends_with(" --read-hook"), "got: {cmd}");
        assert_eq!(overlay["hooks"]["PreToolUse"][0]["matcher"], "Read");

        // E1 recorded as failed ⇒ no PreToolUse hook even with the toggle on.
        settings.harness_versions.e1_status = "fail".to_string();
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let overlay = settings_overlay(&args);
        assert!(
            overlay.is_none_or(|o| o["hooks"].get("PreToolUse").is_none()),
            "e1_status=fail must block the read hook"
        );

        // Unverified (the default) does NOT block — Feature 0's posture is
        // opt-in-until-proven-broken, not blocked-until-proven-working.
        settings.harness_versions.e1_status = "unverified".to_string();
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        assert!(settings_overlay(&args).is_some_and(|o| o["hooks"]["PreToolUse"].is_array()));

        // The statuses are hand-editable strings; anything unrecognized
        // fails CLOSED (a typo'd failure record must not install the hook).
        for status in ["Fail", " fail ", "failed", "faill"] {
            settings.harness_versions.e1_status = status.to_string();
            let args = build_pre_args(&claude_cfg(), &settings, "claude");
            let overlay = settings_overlay(&args);
            assert!(
                overlay.is_none_or(|o| o["hooks"].get("PreToolUse").is_none()),
                "unrecognized e1_status {status:?} must fail closed"
            );
        }
        // Recognized non-fail spellings still pass, case-folded.
        settings.harness_versions.e1_status = "Pass".to_string();
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        assert!(settings_overlay(&args).is_some_and(|o| o["hooks"]["PreToolUse"].is_array()));
    }

    /// V17 Phase B: the second `PreToolUse` **Bash** matcher (whole-file shell
    /// read interception) is present exactly when every gate holds —
    /// `read_advisor` AND `read_advisor_shell` AND E1 not failed. The `Read`
    /// matcher tracks `read_advisor` + E1 alone (the sub-toggle never affects
    /// it), and the sub-toggle being off is a zero overlay delta for the Bash
    /// side.
    #[test]
    fn shell_read_bash_matcher_gated_on_full_matrix() {
        // Whether the overlay carries a PreToolUse entry for `matcher`.
        fn has_matcher(read_advisor: bool, shell: bool, e1: &str, matcher: &str) -> bool {
            let mut settings = Settings::default();
            settings.graph.enabled = true;
            settings.graph.read_advisor = read_advisor;
            settings.graph.read_advisor_shell = shell;
            settings.harness_versions.e1_status = e1.to_string();
            let args = build_pre_args(&claude_cfg(), &settings, "claude");
            settings_overlay(&args)
                .and_then(|o| o["hooks"]["PreToolUse"].as_array().cloned())
                .is_some_and(|arr| arr.iter().any(|e| e["matcher"] == matcher))
        }

        for &read_advisor in &[false, true] {
            for &shell in &[false, true] {
                for e1 in ["unverified", "pass", "fail"] {
                    let e1_ok = e1 != "fail";
                    let read_present = read_advisor && e1_ok;
                    let bash_present = read_advisor && shell && e1_ok;
                    assert_eq!(
                        has_matcher(read_advisor, shell, e1, "Read"),
                        read_present,
                        "Read matcher: read_advisor={read_advisor} shell={shell} e1={e1}"
                    );
                    assert_eq!(
                        has_matcher(read_advisor, shell, e1, "Bash"),
                        bash_present,
                        "Bash matcher: read_advisor={read_advisor} shell={shell} e1={e1}"
                    );
                }
            }
        }
    }

    /// V13 Phase C: the UserPromptSubmit hook (the prompt-tap checkpoint
    /// trigger's transport) must still install when `workbench.checkpoints`
    /// is on, even with context injection off — the milestone's Decision 4.
    #[test]
    fn context_hook_overlay_installed_when_checkpoints_on_even_if_injection_off() {
        let mut settings = Settings::default();
        settings.statusline.enabled = false;
        settings.graph.enabled = true;
        settings.graph.context_injection = false;
        settings.workbench.checkpoints = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let overlay = settings_overlay(&args).expect("overlay present");
        let cmd = overlay["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .expect("hook command is a string");
        assert!(cmd.ends_with(" --context-hook"), "got: {cmd}");
        // PreCompact stays off — it's still gated on context_injection alone.
        assert!(overlay["hooks"].get("PreCompact").is_none());
    }

    /// Checkpoints alone (graph off) must NOT install the hook — the
    /// milestone's widened condition still requires `graph.enabled` (the
    /// hook's own gate prefix is unchanged, only the injection/checkpoints
    /// half was widened).
    #[test]
    fn no_context_hook_when_checkpoints_on_but_graph_disabled() {
        let mut settings = Settings::default();
        settings.statusline.enabled = false;
        settings.graph.enabled = false;
        settings.workbench.checkpoints = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let overlay = settings_overlay(&args).expect("overlay present");
        assert!(overlay["hooks"].get("UserPromptSubmit").is_none());
    }

    #[test]
    fn postedit_hook_installed_when_auto_check_on_with_checks_configured() {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.auto_check = true;
        settings.checks = vec![crate::checks::CheckDef {
            name: "cargo".to_string(),
            cmd: "cargo check".to_string(),
            ..Default::default()
        }];
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let overlay = settings_overlay(&args).expect("overlay present");
        let hook = &overlay["hooks"]["PostToolUse"][0];
        assert_eq!(hook["matcher"], "Edit|Write|MultiEdit");
        let cmd = hook["hooks"][0]["command"]
            .as_str()
            .expect("hook command is a string");
        assert!(cmd.ends_with(" --postedit-hook"), "got: {cmd}");
    }

    #[test]
    fn no_postedit_hook_when_auto_check_off_or_no_checks_configured() {
        let mut settings = Settings::default();
        settings.statusline.enabled = false;
        settings.graph.enabled = true;
        settings.graph.auto_check = false;
        settings.checks = vec![crate::checks::CheckDef::default()];
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        // auto_check off → no PostToolUse hook (nothing else is on either, so
        // the overlay carries only the unconditional NC-2 permission hooks).
        let overlay = settings_overlay(&args).expect("overlay present");
        assert!(overlay["hooks"].get("PostToolUse").is_none());

        let mut settings2 = Settings::default();
        settings2.statusline.enabled = false;
        settings2.graph.enabled = true;
        settings2.graph.auto_check = true;
        settings2.checks = Vec::new();
        let args2 = build_pre_args(&claude_cfg(), &settings2, "claude");
        let overlay2 = settings_overlay(&args2).expect("overlay present");
        assert!(overlay2["hooks"].get("PostToolUse").is_none());
    }

    /// NC-2 (issue #5): the `Notification` + `PermissionDenied` hooks are
    /// injected for EVERY Claude tab, from the barest possible settings, both
    /// pointing at the one `--notify-hook` shim, both with the documented
    /// match-everything `"matcher": ""` (a narrowing matcher filters on
    /// notification TYPE; we classify app-side so a renamed type degrades to
    /// "ignored", not to silence).
    #[test]
    fn permission_hooks_injected_unconditionally_for_claude() {
        // Barest settings: every gated injection off.
        let settings = Settings::default();
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        // The Claude Code `--settings` contract: ONE flag, one merged overlay —
        // the new hooks must ride the same object as everything else, never a
        // second flag (Claude does not concatenate repeated `--settings`).
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        let overlay = settings_overlay(&args).expect("overlay present");
        for event in ["Notification", "PermissionDenied"] {
            let entry = &overlay["hooks"][event][0];
            assert_eq!(
                entry["matcher"], "",
                "{event} must match every type/tool: {entry}"
            );
            let cmd = entry["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("{event} hook command is a string"));
            assert!(cmd.ends_with(" --notify-hook"), "got: {cmd}");
            assert!(!cmd.contains('\\'), "path must be forward-slashed: {cmd}");
        }

        // Non-Claude tabs get no pre-args at all (OpenCode is configured via
        // OPENCODE_CONFIG_CONTENT), so nothing leaks there.
        assert!(build_pre_args(&opencode_cfg(), &settings, "opencode").is_empty());
    }

    /// NC-2: because the permission hooks are unconditional and derive from no
    /// Settings field, they are deliberately absent from `spawn_inject_sig` —
    /// nothing a user can save may change them, so no restart hint is owed.
    /// (Same convention as the pinned OpenCode `subagent_depth`.)
    #[test]
    fn permission_hooks_need_no_spawn_inject_sig_entry() {
        let settings = Settings::default();
        let sig = spawn_inject_sig(&settings);
        let hooks = sig[0]["hooks"].as_array().expect("claude hooks sig array");
        // The five GATED hook entries, unchanged by NC-2 — all off by default.
        assert_eq!(hooks.len(), 5, "unexpected hook-gate count: {hooks:?}");
        assert!(hooks.iter().all(|g| g == &serde_json::Value::Bool(false)));
        // …while the overlay built from those same all-off settings still
        // carries the two ungated hooks. The gap is the point: a signature
        // entry exists to detect a Settings-driven difference between a running
        // tab and a fresh one, and there is no setting here to differ.
        let overlay = settings_overlay(&build_pre_args(&claude_cfg(), &settings, "claude"))
            .expect("overlay present");
        let installed = overlay["hooks"].as_object().expect("hooks object");
        assert_eq!(
            installed.len(),
            2,
            "all gates off ⇒ only the ungated NC-2 hooks: {installed:?}"
        );
    }

    /// NC-2: the cwd-fallback input — every Claude tab with the directory it
    /// actually launches in (per-tab `cwd` override, else the app launch dir),
    /// resolved exactly as `build_ai_tool_spec` does. OpenCode/Shell tabs are
    /// excluded: the hook only fires for Claude.
    #[test]
    fn claude_tab_dirs_lists_claude_tabs_with_their_launch_dirs() {
        let mut settings = Settings {
            tabs: vec![
                TabConfig::AiTool(claude_cfg()),
                TabConfig::AiTool(opencode_cfg()),
            ],
            ..Settings::default()
        };
        let launch = Path::new("C:/proj");
        let dirs = claude_tab_dirs(&settings, launch);
        assert_eq!(dirs.len(), 1, "only the Claude tab: {dirs:?}");
        assert!(
            dirs.iter().all(|(_, d)| d == launch),
            "no per-tab cwd ⇒ every tab inherits the launch dir: {dirs:?}"
        );
        assert!(
            !dirs.iter().any(|(id, _)| id == "opencode"),
            "non-Claude tabs must not appear: {dirs:?}"
        );

        // A worktree tab (the one flow that sets `cwd`) reports its own dir.
        let mut wt = claude_cfg();
        wt.id = "ai-worktree".to_string();
        wt.cwd = Some(std::path::PathBuf::from("C:/proj/wt"));
        settings.tabs.push(TabConfig::AiTool(wt));
        let dirs = claude_tab_dirs(&settings, launch);
        assert_eq!(
            dirs.iter()
                .find(|(id, _)| id == "ai-worktree")
                .map(|(_, d)| d.clone()),
            Some(std::path::PathBuf::from("C:/proj/wt"))
        );
    }

    #[test]
    fn statusline_and_context_hook_share_one_overlay() {
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        settings.graph.enabled = true;
        settings.graph.context_injection = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        // Exactly one `--settings` flag carrying both keys.
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        let overlay = settings_overlay(&args).expect("overlay present");
        assert!(overlay.get("statusLine").is_some());
        assert!(overlay.get("hooks").is_some());
    }

    /// CD-4 (maintenance 2026-08-04) — the Claude Code `--settings` contract.
    /// Two guarantees, asserted against the largest overlay we can emit:
    ///
    ///   * **No permission rules, no plugins.** Claude Code 2.1.214 narrowed
    ///     single-segment permission globs (`Edit(src/**)` now matches only
    ///     `<cwd>/src` depth) and deprecated the `Write(path)` / `Glob(path)` /
    ///     `NotebookEdit(path)` rule forms in favor of `Edit(path)` /
    ///     `Read(path)`; plugins delivered through `--settings` were broken in
    ///     2.1.181–2.1.214. cImp's overlay carries neither, so none of that
    ///     applies — pinning the key set makes the negative durable: a future
    ///     `permissions`/`plugins` key has to come past this note.
    ///   * **Size.** Settings over 2 MiB hard-fail at startup (2.1.214). The
    ///     overlay is bounded by construction — fixed-shape JSON whose only
    ///     variable part is this binary's own path, repeated once per hook
    ///     command — and no user-supplied JSON is ever merged into it (the
    ///     `.cimp.custom.config.json` overlay is cImp's *own* settings layer
    ///     and never reaches Claude). A static ceiling is therefore enough;
    ///     there is nothing unbounded to re-check at spawn time.
    #[test]
    fn settings_overlay_matches_claude_settings_contract() {
        let mut settings = Settings::default();
        // Every overlay-producing gate on at once — the biggest overlay
        // `build_pre_args` can build.
        settings.statusline.enabled = true;
        settings.workbench.checkpoints = true;
        settings.graph.enabled = true;
        settings.graph.context_injection = true;
        settings.graph.compaction_context = true;
        settings.graph.read_advisor = true;
        settings.graph.read_advisor_shell = true;
        settings.graph.auto_check = true;
        settings.checks = vec![crate::checks::CheckDef {
            name: "cargo".to_string(),
            cmd: "cargo check".to_string(),
            ..Default::default()
        }];

        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let i = args
            .iter()
            .position(|a| a == "--settings")
            .expect("overlay present");
        let raw = &args[i + 1];
        let overlay: serde_json::Value =
            serde_json::from_str(raw).expect("--settings value is valid JSON");

        // Sanity: this really is the maxed-out overlay, not a degenerate one.
        let hooks = overlay["hooks"].as_object().expect("hooks object");
        for k in [
            "UserPromptSubmit",
            "PreCompact",
            "PreToolUse",
            "PostToolUse",
            // NC-2 — unconditional, so present in every overlay.
            "Notification",
            "PermissionDenied",
        ] {
            assert!(hooks.contains_key(k), "expected hook {k} in {overlay}");
        }
        assert_eq!(
            overlay["hooks"]["PreToolUse"].as_array().map(Vec::len),
            Some(2),
            "Read + Bash read-advisor matchers",
        );

        // The whole overlay is exactly these two keys.
        let mut keys: Vec<&str> = overlay
            .as_object()
            .expect("overlay is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["hooks", "statusLine"],
            "unexpected `--settings` key — see the permission-glob / plugin \
             contract notes on this test before adding one",
        );

        // Ceiling: ~170x headroom over the real maxed-out overlay (measured
        // 1135 bytes before NC-2 added two more hook commands) and 8x below
        // Claude Code's 2 MiB hard-fail.
        const MAX_OVERLAY_BYTES: usize = 256 * 1024;
        assert!(
            raw.len() < MAX_OVERLAY_BYTES,
            "overlay is {} bytes, ceiling is {MAX_OVERLAY_BYTES}",
            raw.len(),
        );
    }

    #[test]
    fn opencode_plugin_source_bakes_endpoint_and_flag() {
        let js = opencode_plugin_source(54321, "deadbeef00", true, true);
        assert!(js.contains("127.0.0.1:54321"));
        assert!(js.contains("deadbeef00"));
        assert!(js.contains("CIMP_INJECT_ENABLED = true"));
        assert!(js.contains("CIMP_AUTO_CHECK_ENABLED = true"));
        assert!(js.contains("/context/retrieve"));
        assert!(js.contains("/memory/event"));
        assert!(js.contains("/context/post_edit"));
        assert!(js.contains("chat.message"));
        assert!(js.contains("tool.execute.after"));
        // V24 Phase F: the usage-forwarding `event` hook + its POST body shape.
        assert!(js.contains("event: async"));
        assert!(js.contains(r#"kind: "usage""#));
        let off = opencode_plugin_source(1, "x", false, false);
        assert!(off.contains("CIMP_INJECT_ENABLED = false"));
        assert!(off.contains("CIMP_AUTO_CHECK_ENABLED = false"));
    }

    /// V24 Phase F: the `event` hook forwards a completed assistant turn's real
    /// token totals as a `kind: "usage"` body, maps the token fields per the
    /// spike (reasoning folds into output, bare `modelID`), learns parent
    /// sessions from `session.created`, and is NOT gated on the inject/auto-check
    /// flags (usage is always recorded).
    #[test]
    fn opencode_plugin_source_forwards_usage_on_completed_turn() {
        let js = opencode_plugin_source(54321, "tok", true, true);
        // Gates on an assistant turn that has completed.
        assert!(
            js.contains(r#"info.role !== "assistant""#),
            "role filter: {js}"
        );
        assert!(
            js.contains("info.time.completed"),
            "completed-turn gate: {js}"
        );
        assert!(js.contains(r#"event.type !== "message.updated""#));
        // Body field mapping (spike-confirmed).
        assert!(js.contains("session_id: info.sessionID"));
        assert!(js.contains("msg_id: info.id"));
        assert!(js.contains("model: info.modelID"), "bare modelID: {js}");
        assert!(js.contains("in_tok: (tok.input || 0)"));
        // reasoning folds into output.
        assert!(js.contains("out_tok: ((tok.output || 0) + (tok.reasoning || 0))"));
        assert!(js.contains("cache_read: (cache.read || 0)"));
        assert!(js.contains("cache_make: (cache.write || 0)"));
        // parentID map: populated from session.created, stamped on child POSTs.
        assert!(js.contains("const CIMP_PARENTS = new Map()"));
        assert!(js.contains(r#"event.type === "session.created""#));
        assert!(js.contains("CIMP_PARENTS.set(info.id, info.parentID)"));
        assert!(js.contains("CIMP_PARENTS.get(info.sessionID)"));
        assert!(js.contains("body.parent_session_id = parent"));

        // The usage `event` hook must NOT be gated on the inject/auto-check
        // flags — usage is recorded regardless. The hook body (from `event:`
        // to the end) references neither flag.
        let off = opencode_plugin_source(1, "x", false, false);
        let event_start = off.find("event: async").expect("event hook present");
        let hook = &off[event_start..];
        assert!(
            !hook.contains("CIMP_INJECT_ENABLED") && !hook.contains("CIMP_AUTO_CHECK_ENABLED"),
            "usage event hook must not depend on the gating flags: {hook}"
        );
    }

    /// V24 code-review: the `tool.execute.after` POST also stamps
    /// `parent_session_id` for a task-tool child session (not only the usage
    /// `event` hook), so the backend can drop the child's tool events (Claude
    /// sidechain parity) and roll activity up to the parent.
    #[test]
    fn opencode_tool_event_stamps_parent_session_for_children() {
        let js = opencode_plugin_source(1, "x", true, true);
        let start = js
            .find(r#""tool.execute.after""#)
            .expect("tool hook present");
        // Scope to the tool hook body — up to the next top-level hook (`event:`).
        let end = js[start..]
            .find("event: async")
            .map(|e| start + e)
            .expect("event hook after");
        let hook = &js[start..end];
        assert!(
            hook.contains("CIMP_PARENTS.get(inp.sessionID)"),
            "child lookup in tool hook: {hook}"
        );
        assert!(
            hook.contains("body.parent_session_id = parent"),
            "parent stamp in tool hook: {hook}"
        );
    }

    /// V13 Phase C: the `/context/retrieve` POST inside `chat.message` must
    /// NOT be gated behind an early `if (!CIMP_INJECT_ENABLED) return`
    /// (unlike the applying-the-text step) — the prompt-tap checkpoint
    /// trigger needs every prompt to reach the app even when injection is
    /// off. Also carries `agent: "opencode"` so the checkpoint it fires is
    /// attributable.
    #[test]
    fn opencode_chat_message_posts_retrieve_even_when_injection_disabled() {
        let js = opencode_plugin_source(1, "x", false, false);
        assert!(
            js.contains(r#"agent: "opencode""#),
            "missing agent field: {js}"
        );
        // The fetch call must appear BEFORE any inject-gated early return —
        // i.e. there is no `if (!CIMP_INJECT_ENABLED) return;` guarding the
        // `chat.message` handler's body ahead of the fetch.
        let chat_message_start = js
            .find("\"chat.message\"")
            .expect("chat.message handler present");
        let fetch_pos = js[chat_message_start..]
            .find("fetch(CIMP_LOOPBACK")
            .expect("fetch call present");
        let between = &js[chat_message_start..chat_message_start + fetch_pos];
        assert!(
            !between.contains("if (!CIMP_INJECT_ENABLED) return"),
            "the retrieve POST must not be gated on CIMP_INJECT_ENABLED: {between}"
        );
        // The gate DOES still apply to actually using the response text.
        assert!(js.contains("CIMP_INJECT_ENABLED && j && j.ok && j.text"));
    }

    #[test]
    fn statusline_overlay_is_claude_only() {
        // OpenCode understands neither --append-system-prompt nor --settings
        // (its config arrives via OPENCODE_CONFIG_CONTENT), so its pre-args stay
        // empty even with the global toggle on.
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        let args = build_pre_args(&opencode_cfg(), &settings, "opencode");
        assert!(
            args.is_empty(),
            "opencode must get no pre-args, got: {args:?}"
        );
    }

    #[test]
    fn guidance_and_statusline_coexist() {
        // V20: TTS markup is no longer injected, but capability guidance
        // (graph/offload) still feeds --append-system-prompt; with the status
        // line also on, both pre-arg pairs are present.
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        settings.graph.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");

        assert!(args.iter().any(|a| a == "--append-system-prompt"));
        assert!(args.iter().any(|a| a == "--settings"));
    }

    #[test]
    fn injects_offload_mcp_config_for_claude_when_enabled() {
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");

        let i = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        assert_eq!(
            cfg["mcpServers"]["cimp-offload"]["args"][0],
            "--offload-mcp"
        );
    }

    // ── V28 (issue #13): per-tab MCP identity ─────────────────────────────

    /// The `cimp-offload` child's argv, for whichever Claude tab id is given.
    fn claude_offload_argv(settings: &Settings, tab: &str) -> Vec<String> {
        let args = build_pre_args(&claude_cfg(), settings, tab);
        let i = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        cfg["mcpServers"]["cimp-offload"]["args"]
            .as_array()
            .expect("args array")
            .iter()
            .map(|v| v.as_str().expect("string arg").to_string())
            .collect()
    }

    #[test]
    fn claude_mcp_child_carries_its_own_tab_id() {
        // V28: the per-tab MCP child is told WHICH tab it serves, so the app can
        // resolve that tab's current session instead of "the most recent Claude
        // session" — the whole point of the milestone. Two Claude tabs on one
        // project must bake DIFFERENT ids.
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        for tab in ["claude", "claude-local"] {
            let argv = claude_offload_argv(&settings, tab);
            assert!(
                argv.windows(2).any(|w| w == ["--tab", tab]),
                "tab {tab} argv: {argv:?}"
            );
        }
        assert_ne!(
            claude_offload_argv(&settings, "claude"),
            claude_offload_argv(&settings, "claude-local"),
            "two Claude tabs must not spawn identical MCP children"
        );
    }

    #[test]
    fn tab_id_rides_every_claude_mcp_gate() {
        // `--tab` is unconditional on the `cimp-offload` entry: whichever gate
        // caused the entry to be injected (offload / graph), the identity must
        // ride along. A gate that shipped it only sometimes would silently fall
        // back to the shared-scope bug.
        let with_offload = {
            let mut s = Settings::default();
            s.offload.enabled = true;
            s
        };
        let with_graph = {
            let mut s = Settings::default();
            s.graph.enabled = true;
            s
        };
        let with_both = {
            let mut s = Settings::default();
            s.offload.enabled = true;
            s.graph.enabled = true;
            s
        };
        for settings in [with_offload, with_graph, with_both] {
            let argv = claude_offload_argv(&settings, "claude");
            assert_eq!(argv[0], "--offload-mcp", "{argv:?}");
            assert!(
                argv.windows(2).any(|w| w == ["--tab", "claude"]),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn the_code_audit_child_gets_no_tab_id() {
        // Scope check: only the `cimp-offload` child proxies `/graph_run`, so
        // only it needs (and gets) the tab identity. The audit child's argv is
        // unchanged by V28.
        let mut settings = Settings::default();
        settings.code_audit.enabled = true;
        settings.code_audit.expose_claude = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let i = args.iter().position(|a| a == "--mcp-config").unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        let argv = cfg["mcpServers"]["cimp-code-audit"]["args"]
            .as_array()
            .expect("audit args");
        assert!(
            !argv.iter().any(|v| v == "--tab"),
            "audit child needs no tab identity: {argv:?}"
        );
    }

    #[test]
    fn offload_and_graph_guidance_merge_into_one_flag() {
        // V20: with both offload and graph guidance on, they merge into a
        // single --append-system-prompt (TTS markup no longer participates).
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        settings.offload.inject_guidance = true;
        settings.graph.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");

        let count = args
            .iter()
            .filter(|a| *a == "--append-system-prompt")
            .count();
        assert_eq!(count, 1, "addenda must merge into one flag");
        let i = args
            .iter()
            .position(|a| a == "--append-system-prompt")
            .unwrap();
        assert!(args[i + 1].contains("offload_task"));
        assert!(args[i + 1].contains("graph_find_symbol"));
    }

    #[test]
    fn no_offload_injection_when_disabled() {
        let settings = Settings::default(); // offload + graph off by default
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        assert!(!args.iter().any(|a| a == "--mcp-config"));
    }

    #[test]
    fn graph_enabled_alone_injects_mcp_config() {
        // V9-01: the graph tools ride the same `--offload-mcp` child, so the
        // MCP config must be injected when graph is on even if offload is off.
        let mut settings = Settings::default();
        settings.offload.enabled = false;
        settings.graph.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");

        let i = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present when graph is enabled");
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        assert_eq!(
            cfg["mcpServers"]["cimp-offload"]["args"][0],
            "--offload-mcp"
        );
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
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        assert!(
            args.iter().any(|a| a == "--mcp-config"),
            "--mcp-config present when a server is exposed to Claude Code"
        );
    }

    #[test]
    fn code_audit_enabled_alone_injects_code_audit_server() {
        // V26: Code Audit rides its own `--code-audit-mcp` child, so the server
        // must appear in `--mcp-config` when the feature is on even with offload
        // + graph both off. With the default `expose_claude` true, no other
        // server is present — the audit server stands alone in the map.
        let mut settings = Settings::default();
        settings.offload.enabled = false;
        settings.graph.enabled = false;
        settings.code_audit.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");

        let i = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present when Code Audit is enabled");
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        assert_eq!(
            cfg["mcpServers"]["cimp-code-audit"]["args"][0],
            "--code-audit-mcp"
        );
        // Offload rides a different gate that is off here — it must NOT appear.
        assert!(
            cfg["mcpServers"]["cimp-offload"].is_null(),
            "cimp-offload must be absent when its gate is off"
        );
    }

    #[test]
    fn code_audit_server_absent_when_feature_disabled() {
        // The master switch off ⇒ no audit server even though `expose_claude`
        // defaults true; with offload + graph also off, `--mcp-config` is
        // omitted entirely (behavior unchanged from before V26).
        let mut settings = Settings::default();
        settings.code_audit.enabled = false;
        assert!(settings.code_audit.expose_claude, "default is opted-in");
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        assert!(!args.iter().any(|a| a == "--mcp-config"));
    }

    #[test]
    fn code_audit_server_absent_when_expose_claude_off() {
        // Feature on but the Claude consumer opted out ⇒ the audit server is not
        // advertised to Claude. With offload + graph off there is nothing else
        // to inject, so `--mcp-config` is omitted.
        let mut settings = Settings::default();
        settings.code_audit.enabled = true;
        settings.code_audit.expose_claude = false;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        assert!(
            !args.iter().any(|a| a == "--mcp-config"),
            "no server should be injected when the only enabled feature is opted out"
        );
    }

    #[test]
    fn code_audit_and_offload_share_one_mcp_config() {
        // Both gates on ⇒ both servers ride a single `--mcp-config` overlay.
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        settings.code_audit.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");
        let count = args.iter().filter(|a| *a == "--mcp-config").count();
        assert_eq!(count, 1, "exactly one --mcp-config carries both servers");
        let i = args.iter().position(|a| a == "--mcp-config").unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        assert_eq!(
            cfg["mcpServers"]["cimp-offload"]["args"][0],
            "--offload-mcp"
        );
        assert_eq!(
            cfg["mcpServers"]["cimp-code-audit"]["args"][0],
            "--code-audit-mcp"
        );
    }

    #[test]
    fn every_advertised_mcp_server_gets_a_loopback() {
        // Tripwire for the V26 gap: any settings combo that injects an MCP
        // server (Claude `--mcp-config` or the OpenCode `mcp` block) MUST also
        // flip `Settings::loopback_needed()` — the injected children proxy
        // every call over the loopback, so advertising without serving strands
        // them all with "cImp is not running" while the app is visibly up.
        // Sweep each feature axis alone and combined.
        for (offload, graph, audit) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            let mut settings = Settings::default();
            settings.offload.enabled = offload;
            settings.graph.enabled = graph;
            settings.code_audit.enabled = audit;
            let claude_advertises = build_pre_args(&claude_cfg(), &settings, "claude")
                .iter()
                .any(|a| a == "--mcp-config");
            let opencode_advertises = build_opencode_config(&opencode_cfg(), &settings, "opencode")
                .get("mcp")
                .is_some();
            if claude_advertises || opencode_advertises {
                assert!(
                    settings.loopback_needed(),
                    "advertised an MCP server without a loopback: \
                     offload={offload} graph={graph} audit={audit}"
                );
            }
        }
    }

    #[test]
    fn graph_enabled_injects_graph_guidance() {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        let args = build_pre_args(&claude_cfg(), &settings, "claude");

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
        let args = build_pre_args(&opencode_cfg(), &settings, "opencode");
        assert!(
            args.is_empty(),
            "opencode must get no pre-args, got: {args:?}"
        );
    }

    #[test]
    fn claude_launches_fullscreen_by_default() {
        // V20: cImp no longer forces Claude's inline renderer. Without an
        // explicit per-tab override, no `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`
        // is synthesized, so Claude runs in its native fullscreen TUI.
        let settings = Settings::default();
        let env = compose_ai_env(&claude_cfg(), &settings, "claude");
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
            compose_ai_env(&claude_cfg(), &settings, "claude"),
            compose_ai_env(&opencode_cfg(), &settings, "opencode"),
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
        assert!(
            !args.iter().any(|a| a == "--mini"),
            "V20: opencode must NOT get --mini, got: {args:?}"
        );
    }

    #[test]
    fn no_mini_for_any_ai_tab() {
        let settings = Settings::default();
        let claude = build_extra_args(&claude_cfg(), &settings, &[]);
        assert!(
            !claude.iter().any(|a| a == "--mini"),
            "claude must not get --mini"
        );
        let opencode = build_extra_args(&opencode_cfg(), &settings, &[]);
        assert!(
            !opencode.iter().any(|a| a == "--mini"),
            "opencode must not get --mini in V20"
        );
        // A non-opencode, non-claude AI command must not get --mini either.
        let mut other = claude_cfg();
        other.command = "some-other-tool".to_string();
        let other = build_extra_args(&other, &settings, &[]);
        assert!(
            !other.iter().any(|a| a == "--mini"),
            "non-opencode tabs must not get --mini"
        );
    }

    /// D-8 — the `--mini` × `--port` guard. `resolve_oob_source` always
    /// appends `--port <N> --hostname 127.0.0.1` to an OpenCode launch, and
    /// OpenCode hard-fails when `--mini` is combined with `--port`. A stored
    /// `--mini` (hand-edited settings, a carried-over config file) must
    /// therefore never reach the command line — while every other user arg
    /// survives untouched.
    #[test]
    fn opencode_strips_user_supplied_mini_but_keeps_other_args() {
        let settings = Settings::default();
        let mut cfg = opencode_cfg();
        cfg.args = vec![
            "--mini".to_string(),
            "--model".to_string(),
            "x".to_string(),
            "--mini=true".to_string(),
            String::new(),
            "--continue".to_string(),
        ];
        let args = build_extra_args(&cfg, &settings, &[]);
        assert!(
            !args.iter().any(|a| a.starts_with("--mini")),
            "stored --mini must be stripped (it hard-fails with the injected --port), got: {args:?}"
        );
        assert_eq!(
            args,
            vec![
                "--model".to_string(),
                "x".to_string(),
                "--continue".to_string()
            ],
            "only --mini is dropped; every other user arg is preserved in order",
        );
    }

    /// The guard is OpenCode-specific: another AI tool's `--mini` (whatever it
    /// may mean there) is none of cImp's business — no `--port` is injected for
    /// it, so there is no conflict to resolve.
    #[test]
    fn mini_guard_is_opencode_only() {
        let settings = Settings::default();
        let mut cfg = claude_cfg();
        cfg.command = "some-other-tool".to_string();
        cfg.args = vec!["--mini".to_string()];
        assert_eq!(
            build_extra_args(&cfg, &settings, &[]),
            vec!["--mini".to_string()],
            "non-opencode tabs keep their own args verbatim",
        );
    }

    #[test]
    fn is_mini_flag_matches_both_spellings() {
        assert!(is_mini_flag("--mini"));
        assert!(is_mini_flag("--mini=true"));
        assert!(is_mini_flag("--mini=false"));
        // Near misses stay put.
        assert!(!is_mini_flag("--minimal"));
        assert!(!is_mini_flag("--mini-mode"));
        assert!(!is_mini_flag("-m"));
        assert!(!is_mini_flag("mini"));
    }

    #[test]
    fn opencode_config_content_is_valid_json() {
        let settings = Settings::default();
        let env = compose_ai_env(&opencode_cfg(), &settings, "opencode");
        let raw = env
            .get("OPENCODE_CONFIG_CONTENT")
            .expect("opencode tab sets OPENCODE_CONFIG_CONTENT");
        let cfg: serde_json::Value =
            serde_json::from_str(raw).expect("OPENCODE_CONFIG_CONTENT is valid JSON");
        assert_eq!(cfg["$schema"], "https://opencode.ai/config.json");
    }

    /// D-8 — `subagent_depth` is pinned unconditionally. OpenCode 1.18.2 made
    /// the default 1 (subagents may not launch subagents); cImp states 2 so an
    /// upgrade can't silently change nesting behavior. Constant by design: it
    /// derives from no setting, so it needs no `spawn_inject_sig` entry and
    /// must be present in the barest possible config.
    #[test]
    fn opencode_config_pins_subagent_depth() {
        for settings in [Settings::default(), {
            // Every injection gate on — the key survives a maximal config too.
            let mut s = Settings::default();
            s.offload.enabled = true;
            s.graph.enabled = true;
            s.code_audit.enabled = true;
            s.code_audit.expose_opencode = true;
            s
        }] {
            let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
            assert_eq!(
                cfg["subagent_depth"],
                serde_json::json!(2),
                "subagent_depth must be pinned to 2 in every OpenCode config",
            );
        }
    }

    #[test]
    fn opencode_config_injects_mcp_when_offload_enabled() {
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
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
        assert!(
            args.windows(2).any(|w| w == ["--consumer", "opencode"]),
            "got: {args:?}"
        );
    }

    #[test]
    fn opencode_mcp_child_carries_its_tab_id() {
        // V28: the OpenCode-side mirror of `claude_mcp_child_carries_its_own_tab_id`
        // — the `OPENCODE_CONFIG_CONTENT` mcp block bakes `--tab <id>` alongside
        // the consumer discriminator, and it reaches the real launch env (not
        // just the pure config builder).
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        let argv: Vec<&str> = cfg["mcp"]["cimp-offload"]["command"]
            .as_array()
            .expect("command array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            argv.windows(2).any(|w| w == ["--tab", "opencode"]),
            "got: {argv:?}"
        );
        // The audit child stays identity-free on this side too.
        let mut audit = Settings::default();
        audit.code_audit.enabled = true;
        audit.code_audit.expose_opencode = true;
        let cfg = build_opencode_config(&opencode_cfg(), &audit, "opencode");
        let argv = cfg["mcp"]["cimp-code-audit"]["command"]
            .as_array()
            .expect("audit command");
        assert!(!argv.iter().any(|v| v == "--tab"), "got: {argv:?}");

        // End-to-end through the env composer the PTY actually launches with.
        let env = compose_ai_env(&opencode_cfg(), &settings, "opencode");
        let raw = env
            .get("OPENCODE_CONFIG_CONTENT")
            .expect("config env present");
        assert!(raw.contains("\"--tab\",\"opencode\""), "got: {raw}");
    }

    #[test]
    fn opencode_config_no_mcp_when_all_off() {
        let settings = Settings::default(); // offload + graph off, no servers
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        assert!(
            cfg.get("mcp").is_none(),
            "no mcp block when nothing is in play"
        );
    }

    #[test]
    fn opencode_config_injects_mcp_when_graph_enabled() {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        assert!(
            cfg["mcp"]["cimp-offload"].is_object(),
            "graph alone injects the mcp block"
        );
    }

    #[test]
    fn opencode_config_injects_code_audit_when_enabled() {
        // V26: Code Audit enabled (offload + graph off) injects only the
        // `cimp-code-audit` entry, launched as a local child carrying the
        // opencode consumer discriminator.
        let mut settings = Settings::default();
        settings.code_audit.enabled = true;
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        assert_eq!(cfg["mcp"]["cimp-code-audit"]["type"], "local");
        let cmd: Vec<&str> = cfg["mcp"]["cimp-code-audit"]["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // Exact shape: [exe, "--code-audit-mcp", "--consumer", "opencode"].
        assert_eq!(cmd.len(), 4, "got: {cmd:?}");
        assert_eq!(&cmd[1..], ["--code-audit-mcp", "--consumer", "opencode"]);
        // Offload gate is off ⇒ its entry must be absent.
        assert!(
            cfg["mcp"]["cimp-offload"].is_null(),
            "cimp-offload absent when its gate is off"
        );
    }

    #[test]
    fn opencode_config_no_code_audit_when_expose_opencode_off() {
        // Feature on but the OpenCode consumer opted out ⇒ no audit entry, and
        // with offload + graph off the whole `mcp` block is omitted.
        let mut settings = Settings::default();
        settings.code_audit.enabled = true;
        settings.code_audit.expose_opencode = false;
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        assert!(
            cfg.get("mcp").is_none(),
            "no mcp block when the only enabled feature is opted out of OpenCode"
        );
    }

    #[test]
    fn opencode_config_references_instructions_when_guidance_applies() {
        // V20: TTS markup is no longer injected, so the instructions file is
        // referenced only when capability guidance (graph/offload) applies.
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        let path = cfg["instructions"][0].as_str().expect("instructions path");
        assert!(path.ends_with(".md"), "got: {path}");
        assert!(path.contains("opencode"), "got: {path}");
    }

    #[test]
    fn opencode_config_no_instructions_when_no_guidance() {
        // V20: default settings (offload + graph off) ⇒ no guidance ⇒ no
        // instructions key, regardless of the (now-vestigial) tts_injection.
        let settings = Settings::default();
        let config = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        assert!(
            config.get("instructions").is_none(),
            "no guidance ⇒ no instructions key"
        );
    }

    #[test]
    fn opencode_config_no_provider_when_unregistered() {
        // With no `local-llama` registered, cimp injects no `provider`/`model`
        // block — regardless of the per-tab `use_local_provider` flag (which
        // drives Claude's env synthesis, not OpenCode's config).
        let settings = Settings::default();
        let mut cfg = opencode_cfg();
        cfg.use_local_provider = true;
        let config = build_opencode_config(&cfg, &settings, "opencode");
        assert!(
            config.get("provider").is_none(),
            "no registration ⇒ no provider block"
        );
        assert!(config.get("model").is_none());
    }

    #[test]
    fn opencode_config_injects_registered_local_provider() {
        // A registered snapshot ⇒ a `provider.local-llama` block pointing at the
        // local endpoint + `model` selecting it, so the tab is ready on open.
        let mut settings = Settings::default();
        settings.offload.opencode_provider = Some(crate::settings::OpencodeLocalProvider {
            base_url: "http://127.0.0.1:8080/v1".to_string(),
            model: "Qwen3-Q4".to_string(),
            api_key: String::new(),
            source_command: "llama-server -m Qwen3-Q4.gguf --port 8080".to_string(),
        });
        let config = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        let prov = &config["provider"]["local-llama"];
        assert_eq!(prov["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(prov["options"]["baseURL"], "http://127.0.0.1:8080/v1");
        assert!(
            prov["models"]["Qwen3-Q4"].is_object(),
            "model listed in provider"
        );
        assert_eq!(config["model"], "local-llama/Qwen3-Q4");
        assert!(
            prov["options"].get("apiKey").is_none(),
            "no apiKey key when the command carried none",
        );
    }

    #[test]
    fn opencode_config_auto_derives_provider_from_backend() {
        // Auto-sync on + offload enabled ⇒ derive the provider live from the
        // primary Local backend's command, even with no stored snapshot.
        let mut settings = Settings::default();
        settings.offload.enabled = true;
        settings.offload.opencode_provider_auto = true;
        settings.offload.backends = vec![crate::settings::OffloadBackend {
            name: "local".to_string(),
            enabled: true,
            kind: crate::settings::OffloadBackendKind::Local {
                server_command: "llama-server -a my-model --port 9001 --jinja".to_string(),
                autostart: false,
                show_command_on_start: false,
            },
            ..Default::default()
        }];
        let config = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        assert_eq!(
            config["provider"]["local-llama"]["options"]["baseURL"],
            "http://127.0.0.1:9001/v1"
        );
        assert_eq!(config["model"], "local-llama/my-model");
    }

    #[test]
    fn opencode_sets_noise_suppression_env() {
        let settings = Settings::default();
        let env = compose_ai_env(&opencode_cfg(), &settings, "opencode");
        assert_eq!(
            env.get("OPENCODE_DISABLE_TERMINAL_TITLE")
                .map(String::as_str),
            Some("1"),
        );
        assert!(
            !env.contains_key("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
            "opencode must not get the Claude fullscreen flag"
        );
    }

    #[test]
    fn per_tab_env_overrides_opencode_config_content() {
        let settings = Settings::default();
        let mut cfg = opencode_cfg();
        cfg.env
            .insert("OPENCODE_CONFIG_CONTENT".to_string(), "custom".to_string());
        let env = compose_ai_env(&cfg, &settings, "claude");
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
        let env = compose_ai_env(&cfg, &settings, "claude");
        assert_eq!(
            env.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN")
                .map(String::as_str),
            Some("1"),
            "an explicit per-tab value must pass through the env merge",
        );
    }

    // ── Maintenance D-2: MCP auto-backgrounding kill switch ───────────────

    #[test]
    fn claude_disables_mcp_auto_backgrounding() {
        // Unconditional for Claude tabs — including with the local-provider
        // opt-in off, i.e. outside the `ANTHROPIC_*` gate.
        let settings = Settings::default();
        let env = compose_ai_env(&claude_cfg(), &settings, "claude");
        assert_eq!(
            env.get("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS")
                .map(String::as_str),
            Some("0"),
            "long cimp-offload MCP calls must not be auto-backgrounded",
        );
        assert!(
            !env.contains_key("ANTHROPIC_BASE_URL"),
            "sanity: local-provider env is off in this case",
        );
    }

    #[test]
    fn per_tab_env_overrides_mcp_auto_background_ms() {
        let settings = Settings::default();
        let mut cfg = claude_cfg();
        cfg.env.insert(
            "CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS".to_string(),
            "120000".to_string(),
        );
        let env = compose_ai_env(&cfg, &settings, "claude");
        assert_eq!(
            env.get("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS")
                .map(String::as_str),
            Some("120000"),
            "an explicit per-tab value must win over the synthesized constant",
        );
    }

    #[test]
    fn non_claude_tabs_get_no_mcp_auto_background_env() {
        let settings = Settings::default();
        let mut other = claude_cfg();
        other.command = "some-other-tool".to_string();
        for cfg in [opencode_cfg(), other] {
            let env = compose_ai_env(&cfg, &settings, "claude");
            assert!(
                !env.contains_key("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS"),
                "only Claude tabs get the Claude-specific kill switch (command: {})",
                cfg.command,
            );
        }
        // Shell tabs never reach `compose_ai_env` at all — `build_launch_spec`
        // passes their `env` through verbatim.
    }

    // ── V12 Phase E: fact promotion block ─────────────────────────────────

    #[test]
    fn fact_promotion_block_is_pinned_only_newest_first() {
        let dir = std::env::temp_dir().join(format!("cimp-facts-{}", uuid::Uuid::new_v4()));
        {
            let idx = crate::graph::GraphIndex::open(&dir, ".cimp").expect("open");
            idx.add_project_fact("f-old-pinned", "oldest pinned fact", "s1", 100, true)
                .unwrap();
            idx.add_project_fact("f-new-pinned", "newest pinned fact", "s1", 200, true)
                .unwrap();
            idx.add_project_fact(
                "f-unpinned",
                "an unpinned fact must not appear",
                "s1",
                300,
                false,
            )
            .unwrap();
            // Dropped here, before reopening read-only below.
        }

        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.promote_pinned_facts = true;

        let block = fact_promotion_block(&dir, &settings).expect("block present");
        assert!(block.starts_with("## cImp project facts\n"), "{block}");
        assert!(block.contains("newest pinned fact"), "{block}");
        assert!(block.contains("oldest pinned fact"), "{block}");
        assert!(
            !block.contains("must not appear"),
            "unpinned facts must not be promoted: {block}"
        );

        let pos_new = block.find("newest pinned fact").unwrap();
        let pos_old = block.find("oldest pinned fact").unwrap();
        assert!(pos_new < pos_old, "newest-pinned must come first: {block}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fact_promotion_block_caps_length() {
        let dir = std::env::temp_dir().join(format!("cimp-facts-cap-{}", uuid::Uuid::new_v4()));
        {
            let idx = crate::graph::GraphIndex::open(&dir, ".cimp").expect("open");
            // Enough ~100-char pinned facts to blow well past the 1500-char cap.
            for i in 0..40 {
                let text = format!(
                    "pinned fact number {i} with some padding text to reach length ##########"
                );
                idx.add_project_fact(&format!("f{i}"), &text, "s1", i as i64, true)
                    .unwrap();
            }
        }

        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.promote_pinned_facts = true;

        let block = fact_promotion_block(&dir, &settings).expect("block present");
        assert!(
            block.len() <= 1500 + 200,
            "block should stay near the cap: {} chars",
            block.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fact_promotion_block_none_without_pinned_facts_or_graph() {
        let dir = std::env::temp_dir().join(format!("cimp-facts-none-{}", uuid::Uuid::new_v4()));
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.promote_pinned_facts = true;

        // No graph ever built at this root — best-effort `None`, no panic.
        assert!(fact_promotion_block(&dir, &settings).is_none());

        {
            let idx = crate::graph::GraphIndex::open(&dir, ".cimp").expect("open");
            idx.add_project_fact("f1", "an unpinned fact", "s1", 1, false)
                .unwrap();
        }
        // A built graph with only unpinned facts is still `None`.
        assert!(fact_promotion_block(&dir, &settings).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The restart-hint edge (`update_settings`) compares this signature
    /// across a save — it must move on every spawn-baked setting, stay put on
    /// live-applied tuning, and stay per-consumer where injection is.
    #[test]
    fn spawn_inject_sig_tracks_spawn_time_settings() {
        let base = spawn_inject_sig(&Settings::default());

        // Claude-only: the `--settings` statusline overlay. Flipped relative
        // to the default (it ships enabled), not hardcoded.
        let mut s = Settings::default();
        s.statusline.enabled = !s.statusline.enabled;
        let sig = spawn_inject_sig(&s);
        assert_ne!(sig[0], base[0], "statusline flip must move the Claude sig");
        assert_eq!(sig[1], base[1], "statusline is Claude-only");

        // Both consumers: guidance + MCP + plugin follow the graph toggle.
        let mut s = Settings::default();
        s.graph.enabled = true;
        let with_graph = spawn_inject_sig(&s);
        assert_ne!(with_graph[0], base[0]);
        assert_ne!(with_graph[1], base[1]);

        // Both consumers: context injection = Claude hook gate + the
        // OpenCode plugin's baked CIMP_INJECT_ENABLED flag.
        s.graph.context_injection = true;
        let sig = spawn_inject_sig(&s);
        assert_ne!(sig[0], with_graph[0]);
        assert_ne!(sig[1], with_graph[1]);

        // Claude-only: the checkpoint prompt-hook gate (injection off).
        let mut s = Settings::default();
        s.graph.enabled = true;
        s.workbench.checkpoints = true;
        let sig = spawn_inject_sig(&s);
        assert_ne!(sig[0], with_graph[0], "checkpoints widen the hook gate");
        assert_eq!(sig[1], with_graph[1], "the OpenCode plugin always POSTs");

        // `claude_local` edits count only once a Claude tab opted into the
        // local provider.
        let mut s = Settings::default();
        s.claude_local.base_url = "http://localhost:4000".to_string();
        assert_eq!(
            spawn_inject_sig(&s)[0],
            base[0],
            "no tab uses the local provider yet"
        );
        let mut tab = claude_cfg();
        tab.use_local_provider = true;
        s.tabs.push(TabConfig::AiTool(tab));
        assert_ne!(spawn_inject_sig(&s)[0], base[0]);

        // Live-applied tuning must NOT nag: the read-advisor thresholds are
        // read per-invocation by the loopback handler, not baked at spawn.
        let mut s = Settings::default();
        s.graph.enabled = true;
        s.graph.read_advisor_min_lines += 25;
        s.graph.context_per_file_chars += 100;
        assert_eq!(spawn_inject_sig(&s), with_graph);
    }
}
