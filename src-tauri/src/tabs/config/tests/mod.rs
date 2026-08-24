//! `tabs::config`'s unit tests — the module's own `#[cfg(test)] mod tests`, in a
//! sibling directory (#132, test-placement wave). `config.rs` was 899
//! production lines under 4,585 test lines; the tests are unchanged by the
//! move, and this file holds only what more than one of them needs.
//!
//! The clusters are the sections the module already had: [`spawn_sig`],
//! [`guidance`], [`mcp`], [`opencode`] and [`env`].
//!
//! **What is NOT here** (#132, second pass): the 55 tests that asserted on a
//! harness's emitted artifact and reached it only through that harness's
//! emitter now live with the emitter — `harness::claude::overlay::tests` (39),
//! `harness::opencode::config::tests` (15), `harness::opencode::plugin::tests`
//! (1). What stayed is what this module's own production code decides: the
//! spawn signature, the guidance addendum, the tab environment, the launch
//! spine, and every cross-harness invariant that no single harness owns. The
//! fixtures both sides need are [`crate::harness::fixtures`], defined once.

mod env;
mod guidance;
mod mcp;
mod opencode;
mod spawn_sig;

use crate::harness::fixtures::*;

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

// V35 Phase K moved the two generated artifacts to `harness/{claude,opencode}/`
// and these tests did NOT move with them, for a reason it wrote down: the ones
// calling the generators directly shared ~30 helpers with the ones driving them
// through the tab-spawn composition, and splitting the module would have
// duplicated those helpers. #132 removed that objection rather than overruling
// it — the shared helpers are `crate::harness::fixtures`, defined once — so the
// generator-only tests are with their generators now. What is left here still
// reaches the emitted JSON, and it reaches it through what THIS module decides.
use crate::harness::claude::overlay::build_pre_args;

use crate::harness::opencode::plugin::opencode_plugin_wanted;

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
