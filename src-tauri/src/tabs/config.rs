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
                overlay.insert(
                    "statusLine".to_string(),
                    serde_json::json!({ "type": "command", "command": command }),
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
            if settings.graph.enabled && (settings.graph.context_injection || settings.workbench.checkpoints) {
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
            if settings.graph.enabled && settings.graph.context_injection && settings.graph.compaction_context {
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
                    hooks.insert(
                        "PreToolUse".to_string(),
                        serde_json::json!([ { "matcher": "Read", "hooks": [
                            { "type": "command", "command": command, "timeout": 5 }
                        ] } ]),
                    );
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
            if !hooks.is_empty() {
                overlay.insert("hooks".to_string(), serde_json::Value::Object(hooks));
            }
        }
        if !overlay.is_empty() {
            args.push("--settings".to_string());
            args.push(serde_json::Value::Object(overlay).to_string());
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
    pinned.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
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
deduplicated diagnostics instead of a raw build dump; `graph_recent_changes` shows what's been \
churning lately. This project also has \
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
    // Need the loopback endpoint to reach the app; without it, skip (and clean).
    let Some(disc) = crate::offload::loopback::read_discovery() else {
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
fn opencode_plugin_source(port: u16, token: &str, inject_enabled: bool, auto_check_enabled: bool) -> String {
    format!(
        r#"// Generated by cImp (V10 Code Intelligence). Do not edit — regenerated each launch.
const CIMP_LOOPBACK = "http://127.0.0.1:{port}";
const CIMP_TOKEN = "{token}";
const CIMP_INJECT_ENABLED = {inject};
const CIMP_AUTO_CHECK_ENABLED = {auto_check};
const CIMP_EDIT_TOOLS = new Set(["edit", "write", "patch"]);

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
      await fetch(CIMP_LOOPBACK + "/memory/event", {{
        method: "POST",
        headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
        body: JSON.stringify({{
          cwd: input.directory,
          session_id: inp.sessionID,
          agent: "opencode",
          tool: inp.tool,
          args: inp.args,
        }}),
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
/// - `mcp.cimp-offload` → `cimp --offload-mcp --consumer opencode`, injected
///   whenever offload, the graph, or an OpenCode-exposed MCP server is in play
///   (mirrors the Claude `--mcp-config` gate in `build_pre_args`).
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
    fn context_hook_overlay_injected_when_injection_on() {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.context_injection = true;
        let args = build_pre_args(&claude_cfg(), &settings);
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
        let args = build_pre_args(&claude_cfg(), &settings);
        // Graph on but injection off + statusline off + checkpoints off →
        // no --settings overlay.
        assert!(settings_overlay(&args).is_none());
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
        let args = build_pre_args(&claude_cfg(), &settings);
        let overlay = settings_overlay(&args).expect("overlay present");
        let cmd = overlay["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("hook command is a string");
        assert!(cmd.ends_with(" --read-hook"), "got: {cmd}");
        assert_eq!(overlay["hooks"]["PreToolUse"][0]["matcher"], "Read");

        // E1 recorded as failed ⇒ no PreToolUse hook even with the toggle on.
        settings.harness_versions.e1_status = "fail".to_string();
        let args = build_pre_args(&claude_cfg(), &settings);
        let overlay = settings_overlay(&args);
        assert!(
            overlay.map_or(true, |o| o["hooks"].get("PreToolUse").is_none()),
            "e1_status=fail must block the read hook"
        );

        // Unverified (the default) does NOT block — Feature 0's posture is
        // opt-in-until-proven-broken, not blocked-until-proven-working.
        settings.harness_versions.e1_status = "unverified".to_string();
        let args = build_pre_args(&claude_cfg(), &settings);
        assert!(settings_overlay(&args).is_some_and(|o| o["hooks"]["PreToolUse"].is_array()));

        // The statuses are hand-editable strings; anything unrecognized
        // fails CLOSED (a typo'd failure record must not install the hook).
        for status in ["Fail", " fail ", "failed", "faill"] {
            settings.harness_versions.e1_status = status.to_string();
            let args = build_pre_args(&claude_cfg(), &settings);
            let overlay = settings_overlay(&args);
            assert!(
                overlay.map_or(true, |o| o["hooks"].get("PreToolUse").is_none()),
                "unrecognized e1_status {status:?} must fail closed"
            );
        }
        // Recognized non-fail spellings still pass, case-folded.
        settings.harness_versions.e1_status = "Pass".to_string();
        let args = build_pre_args(&claude_cfg(), &settings);
        assert!(settings_overlay(&args).is_some_and(|o| o["hooks"]["PreToolUse"].is_array()));
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
        let args = build_pre_args(&claude_cfg(), &settings);
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
        let args = build_pre_args(&claude_cfg(), &settings);
        assert!(settings_overlay(&args).is_none());
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
        let args = build_pre_args(&claude_cfg(), &settings);
        let overlay = settings_overlay(&args).expect("overlay present");
        let hook = &overlay["hooks"]["PostToolUse"][0];
        assert_eq!(hook["matcher"], "Edit|Write|MultiEdit");
        let cmd = hook["hooks"][0]["command"].as_str().expect("hook command is a string");
        assert!(cmd.ends_with(" --postedit-hook"), "got: {cmd}");
    }

    #[test]
    fn no_postedit_hook_when_auto_check_off_or_no_checks_configured() {
        let mut settings = Settings::default();
        settings.statusline.enabled = false;
        settings.graph.enabled = true;
        settings.graph.auto_check = false;
        settings.checks = vec![crate::checks::CheckDef::default()];
        let args = build_pre_args(&claude_cfg(), &settings);
        // auto_check off → no --settings overlay at all (nothing else is on).
        assert!(settings_overlay(&args).is_none());

        let mut settings2 = Settings::default();
        settings2.statusline.enabled = false;
        settings2.graph.enabled = true;
        settings2.graph.auto_check = true;
        settings2.checks = Vec::new();
        let args2 = build_pre_args(&claude_cfg(), &settings2);
        assert!(settings_overlay(&args2).is_none());
    }

    #[test]
    fn statusline_and_context_hook_share_one_overlay() {
        let mut settings = Settings::default();
        settings.statusline.enabled = true;
        settings.graph.enabled = true;
        settings.graph.context_injection = true;
        let args = build_pre_args(&claude_cfg(), &settings);
        // Exactly one `--settings` flag carrying both keys.
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        let overlay = settings_overlay(&args).expect("overlay present");
        assert!(overlay.get("statusLine").is_some());
        assert!(overlay.get("hooks").is_some());
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
        let off = opencode_plugin_source(1, "x", false, false);
        assert!(off.contains("CIMP_INJECT_ENABLED = false"));
        assert!(off.contains("CIMP_AUTO_CHECK_ENABLED = false"));
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
        assert!(js.contains(r#"agent: "opencode""#), "missing agent field: {js}");
        // The fetch call must appear BEFORE any inject-gated early return —
        // i.e. there is no `if (!CIMP_INJECT_ENABLED) return;` guarding the
        // `chat.message` handler's body ahead of the fetch.
        let chat_message_start = js.find("\"chat.message\"").expect("chat.message handler present");
        let fetch_pos = js[chat_message_start..].find("fetch(CIMP_LOOPBACK").expect("fetch call present");
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
    fn opencode_config_no_provider_when_unregistered() {
        // With no `local-llama` registered, cimp injects no `provider`/`model`
        // block — regardless of the per-tab `use_local_provider` flag (which
        // drives Claude's env synthesis, not OpenCode's config).
        let settings = Settings::default();
        let mut cfg = opencode_cfg();
        cfg.use_local_provider = true;
        let config = build_opencode_config(&cfg, &settings);
        assert!(config.get("provider").is_none(), "no registration ⇒ no provider block");
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
        let config = build_opencode_config(&opencode_cfg(), &settings);
        let prov = &config["provider"]["local-llama"];
        assert_eq!(prov["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(prov["options"]["baseURL"], "http://127.0.0.1:8080/v1");
        assert!(prov["models"]["Qwen3-Q4"].is_object(), "model listed in provider");
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
        let config = build_opencode_config(&opencode_cfg(), &settings);
        assert_eq!(config["provider"]["local-llama"]["options"]["baseURL"], "http://127.0.0.1:9001/v1");
        assert_eq!(config["model"], "local-llama/my-model");
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
            idx.add_project_fact("f-unpinned", "an unpinned fact must not appear", "s1", 300, false)
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
        assert!(!block.contains("must not appear"), "unpinned facts must not be promoted: {block}");

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
                let text = format!("pinned fact number {i} with some padding text to reach length ##########");
                idx.add_project_fact(&format!("f{i}"), &text, "s1", i as i64, true).unwrap();
            }
        }

        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.promote_pinned_facts = true;

        let block = fact_promotion_block(&dir, &settings).expect("block present");
        assert!(block.len() <= 1500 + 200, "block should stay near the cap: {} chars", block.len());

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
            idx.add_project_fact("f1", "an unpinned fact", "s1", 1, false).unwrap();
        }
        // A built graph with only unpinned facts is still `None`.
        assert!(fact_promotion_block(&dir, &settings).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
