//! `tabs::config`'s unit tests — the module's own `#[cfg(test)] mod tests`, in a
//! sibling directory (#132, test-placement wave). `config.rs` was 899
//! production lines under 4,585 test lines; the tests are unchanged by the
//! move, and this file holds only what more than one of them needs.
//!
//! The clusters are the sections the module already had: [`overlay`] (Claude's
//! `--settings` artifact), [`spawn_sig`], [`guidance`], [`mcp`], [`opencode`]
//! and [`env`].

mod overlay;
mod spawn_sig;
mod guidance;
mod mcp;
mod opencode;
mod env;

// The Claude hook-route vocabulary and OpenCode's config writers: the
// production launch path reaches them through each harness's plugin now,
// but the tests below assert on the ARTIFACTS both produce, so they name
// them directly (a test that quotes a payload is a recorded input, which is
// why `layering.rs`'s literal scan skips test text).
use crate::harness::instructions::{text as instruction_text, Slot as ISlot};

// V40 Phase G: the three neutral model-visible strings this file used to own
// are inventory rows now (locked decision 24). The tests below still assert
// on their BYTES, so they read them back through the inventory rather than
// through a `pub(crate)` const nothing would stop production code from
// reaching around the seam for. `None` is the neutral rendering, which for
// a neutral row is every rendering.
fn injection_hygiene_guidance() -> String {
    instruction_text(None, ISlot::InjectionHygiene).to_string()
}

fn tool_steering_checks() -> &'static str {
    instruction_text(None, ISlot::ToolSteeringChecks)
}

fn tool_steering_commands() -> &'static str {
    instruction_text(None, ISlot::ToolSteeringCommands)
}

fn tool_steering_tail() -> &'static str {
    instruction_text(None, ISlot::ToolSteeringTail)
}

use crate::harness::claude::hook as claude_hook;

use crate::harness::opencode::config::build_opencode_config;

/// What `build_ai_tool_spec` does for the out-of-band source, in one line —
/// V40 Phase A moved the bodies behind `HarnessPlugin::resolve_oob`, and the
/// tests below drive the whole path (classify, then ask) rather than one
/// harness's half, which is what they were always about.
fn resolve_oob_source(
    cfg: &AiToolTabConfig,
    working_dir: &Path,
    extra_args: &mut Vec<String>,
    env: &HashMap<String, String>,
) -> Option<crate::harness::OobSpec> {
    crate::harness::HarnessId::from_command(&cfg.command)
        .and_then(|h| h.plugin())
        .and_then(|p| p.resolve_oob(cfg, working_dir, extra_args, env))
}

use super::*;

// V42 (#124 R25): spelled from its own module now that this file's
// `NativeWebVisibility` alias is gone.
use crate::settings::injection::NativeWebMode;

/// A registered harness by id — **tests only**. The spawn-signature map is
/// keyed by `HarnessId` since V40 Phase B, and these tests were written
/// against the positional pair it replaced, so each one now names the
/// harness its assertion was always about.
fn h(id: &str) -> crate::harness::HarnessId {
    crate::harness::HarnessId::from_id(id).expect("registered harness")
}

use crate::settings::{
    ai_tab_inheriting_injection, default_claude_tab, default_opencode_tab,
};

/// The builtin Claude tab with an all-`Inherit` injection row — see
/// [`crate::settings::ai_tab_inheriting_injection`] for why these fixtures
/// do not use the V39 shipping row.
fn claude_tab_inheriting() -> crate::settings::TabConfig {
    ai_tab_inheriting_injection(default_claude_tab())
}

/// The builtin OpenCode tab with an all-`Inherit` injection row.
fn opencode_tab_inheriting() -> crate::settings::TabConfig {
    ai_tab_inheriting_injection(default_opencode_tab())
}

// V35 Phase K: the two generated artifacts moved to `harness/{claude,opencode}/`.
// These tests did NOT move with them, deliberately: most drive the emitted
// JSON through `build_launch_spec`/`build_ai_tool_spec` — the tab-spawn
// composition that stays here — and the ones that call the generators
// directly share ~30 helpers with them (`claude_cfg`, `hook_endpoint`,
// `settings_overlay`, the node plugin harness). Splitting the module would
// have duplicated those helpers, which is a behaviour risk this phase does
// not accept. Every test name and body is unchanged.
use crate::harness::claude::overlay::{
    build_pre_args, CLAUDE_MUTATING_TOOL_MATCHER, CLAUDE_WEB_TOOL_MATCHER,
};

use crate::harness::opencode::config::{
    opencode_pinned_read, OPENCODE_PINNED_BASH, OPENCODE_PINNED_EDIT,
    OPENCODE_PINNED_READ_ANY, OPENCODE_PINNED_READ_ENV, OPENCODE_PINNED_READ_ENV_EXAMPLE,
    OPENCODE_PINNED_WEBFETCH, OPENCODE_PINNED_WEBSEARCH,
};

use crate::harness::opencode::plugin::opencode_plugin_wanted;

/// V35 Phase J: the loopback endpoint the overlay bakes into every
/// `type: "http"` hook's URL, and whose token `compose_ai_env` puts in the
/// child's environment.
///
/// A fixture rather than a real `read_own_discovery()` for the reason every
/// other input to `build_pre_args` is one: the emitted overlay has to be
/// assertable byte for byte, and a test that read the live discovery file
/// would pass or fail depending on whether a cImp happened to be running.
fn hook_endpoint() -> crate::offload::discovery::Discovery {
    crate::offload::discovery::Discovery {
        port: 41999,
        token: "test-loopback-token".to_string(),
        pid: 0,
        root: String::new(),
    }
}

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

/// The id of the AI tab at `idx`. The V32 L3 override cells are
/// `pub(in crate::settings)` (#44), so a test writes one by tab id through
/// `Settings::set_tab_override_for_test` rather than by reaching into the
/// config — and this is how the fixtures name the tab they just built.
fn ai_tab_id(s: &Settings, idx: usize) -> String {
    match &s.tabs[idx] {
        TabConfig::AiTool(c) => c.id.clone(),
        _ => unreachable!("tab {idx} is an AI tool tab"),
    }
}

/// The value following the first `--settings` flag in `args`, parsed
/// as JSON. `None` if no `--settings` flag is present.
fn settings_overlay(args: &[String]) -> Option<serde_json::Value> {
    let i = args.iter().position(|a| a == "--settings")?;
    let raw = args.get(i + 1)?;
    Some(serde_json::from_str(raw).expect("--settings value is valid JSON"))
}

/// The one hook object inside `hooks[<event>][idx]`, so an assertion names
/// the entry it is about rather than a chain of indices.
fn hook_entry(overlay: &serde_json::Value, event: &str, idx: usize) -> serde_json::Value {
    overlay["hooks"][event][idx]["hooks"][0].clone()
}

/// Whether the overlay wired the AUTO-CHECK `PostToolUse` group — the one
/// on `Edit|Write|MultiEdit` pointing at `/claude/hook/post_tool_use`.
///
/// Distinguishes it from V35 Phase L's sibling group on the same event
/// (matcher `""`, route `/claude/hook/post_tool_use_result`), which is a
/// different capability with a different gate.
fn post_tool_use_has_auto_check(overlay: &serde_json::Value) -> bool {
    overlay["hooks"]["PostToolUse"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|g| {
            g["matcher"] == "Edit|Write|MultiEdit"
                && g["hooks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|h| {
                        h["url"]
                            .as_str()
                            .is_some_and(|u| u.ends_with(claude_hook::ROUTE_POST_TOOL_USE))
                    })
        })
}

// ── V35 Phase J: the emitted `type: "http"` overlay ─────────────────────

/// The maxed-out overlay, with every gate on — the shape a live spawn
/// produces. Returns `(hooks, every http hook object in it)`.
fn maxed_overlay() -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(true));
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
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let overlay = settings_overlay(&args).expect("overlay present");
    let hooks = overlay["hooks"].clone();
    let mut http = Vec::new();
    for (_event, entries) in hooks.as_object().expect("hooks object") {
        for entry in entries.as_array().cloned().unwrap_or_default() {
            for h in entry["hooks"].as_array().cloned().unwrap_or_default() {
                if h["type"] == "http" {
                    http.push(h);
                }
            }
        }
    }
    (hooks, http)
}

// ── V28 (issue #13): per-tab MCP identity ─────────────────────────────

/// The `cimp-offload` child's argv, for whichever Claude tab id is given.
fn claude_offload_argv(settings: &Settings, tab: &str) -> Vec<String> {
    let args = build_pre_args(&claude_cfg(), settings, tab, Some(&hook_endpoint()));
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
