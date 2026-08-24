//! **Spawn-artifact test fixtures**, shared by the two sides of one split.
//!
//! `tabs::config`'s tests and the harness emitters' tests assert on the same
//! artifacts from opposite ends — cImp's composition decides *what* a tab is
//! told, `harness/{claude,opencode}/` decides *how* — so both need the same
//! handful of inputs: the two builtin AI-tool tab configs, the loopback
//! endpoint the overlay bakes into every `type: "http"` hook, and the reader
//! that pulls the `--settings` overlay back off an argv.
//!
//! #132 moved the `harness/`-owned tests out of `tabs::config`'s module and
//! this file is what made that safe to do. V35 Phase K had declined the same
//! move for one reason — "the ones that call the generators directly share ~30
//! helpers with them, and splitting the module would have duplicated those
//! helpers" — so the helpers are defined ONCE, here, and neither side owns a
//! copy that can drift from the other.

use crate::settings::{
    ai_tab_inheriting_injection, default_claude_tab, default_opencode_tab, AiToolTabConfig,
    TabConfig,
};

/// V35 Phase J: the loopback endpoint the overlay bakes into every
/// `type: "http"` hook's URL, and whose token `compose_ai_env` puts in the
/// child's environment.
///
/// A fixture rather than a real `read_own_discovery()` for the reason every
/// other input to `build_pre_args` is one: the emitted overlay has to be
/// assertable byte for byte, and a test that read the live discovery file
/// would pass or fail depending on whether a cImp happened to be running.
pub(crate) fn hook_endpoint() -> crate::offload::discovery::Discovery {
    crate::offload::discovery::Discovery {
        port: 41999,
        token: "test-loopback-token".to_string(),
        pid: 0,
        root: String::new(),
    }
}

pub(crate) fn claude_cfg() -> AiToolTabConfig {
    match default_claude_tab() {
        TabConfig::AiTool(c) => c,
        _ => unreachable!("default_claude_tab is an AI tool tab"),
    }
}

/// An AI-tool tab whose command resolves to `opencode`.
pub(crate) fn opencode_cfg() -> AiToolTabConfig {
    match default_opencode_tab() {
        TabConfig::AiTool(c) => c,
        _ => unreachable!("default_opencode_tab is an AI tool tab"),
    }
}

/// The value following the first `--settings` flag in `args`, parsed
/// as JSON. `None` if no `--settings` flag is present.
pub(crate) fn settings_overlay(args: &[String]) -> Option<serde_json::Value> {
    let i = args.iter().position(|a| a == "--settings")?;
    let raw = args.get(i + 1)?;
    Some(serde_json::from_str(raw).expect("--settings value is valid JSON"))
}

/// The builtin Claude tab with an all-`Inherit` injection row — see
/// [`crate::settings::ai_tab_inheriting_injection`] for why these fixtures
/// do not use the V39 shipping row.
pub(crate) fn claude_tab_inheriting() -> crate::settings::TabConfig {
    ai_tab_inheriting_injection(default_claude_tab())
}

/// The builtin OpenCode tab with an all-`Inherit` injection row.
pub(crate) fn opencode_tab_inheriting() -> crate::settings::TabConfig {
    ai_tab_inheriting_injection(default_opencode_tab())
}
