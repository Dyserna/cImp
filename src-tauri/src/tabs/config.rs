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

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde_json::Value;

use crate::error::{AppError, AppResult};
// V35 Phase K: the two generated harness artifacts moved into `harness/`, one
// directory per harness (design § 4). What stays here is what is cImp's — when
// a tab spawns, and what a setting means; what left is how each harness is
// told. #132 finished the move on the test side too: the tests that reached an
// emitted artifact ONLY through its harness's emitter are with the emitter now,
// and what this module's own tests still drive through `build_launch_spec` /
// `compose_ai_env` / `spawn_inject_sig` is the composition they assert on.
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
                // A Shell tab exists to give the user the environment they
                // actually have — cImp does not edit it (see `HARNESS_ENV_VARS`).
                env_remove: Vec::new(),
                // Shell tabs have no AI assistant output to speak.
                oob: None,
                // V33 Phase B decision B1: a Shell tab is NOT an agent seam. It
                // is the user's own hands at their own machine, so it is never
                // sandboxed and mints no sandbox row — the same reason
                // `env_remove` is empty above.
                harness: None,
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
    // V35 Phase J: the loopback THIS instance serves, read once and handed to
    // both the overlay builder (which bakes the port into every `type: "http"`
    // hook's URL) and the env composer (which puts the bearer token where the
    // harness will substitute it). `read_own_discovery` is pid-keyed — never the
    // shared last-writer-wins file a sibling instance may have overwritten —
    // exactly as `write_opencode_plugin` reads it for the same reason.
    let endpoint = crate::offload::discovery::read_own_discovery();
    // V40 Phase A: which harness this tab is, resolved ONCE, here — the
    // function that already knew. Everything harness-specific below is asked of
    // its plugin; `None` is a tab whose command matches no registered harness,
    // which gets the neutral launch path and no harness wiring at all.
    let harness = crate::harness::HarnessId::from_command(&cfg.command);
    let plugin = harness.and_then(|h| h.plugin());
    let pre_args = plugin
        .map(|p| p.pre_args(cfg, settings, tab.as_str(), endpoint.as_ref()))
        .unwrap_or_default();
    let mut extra_args = build_extra_args(cfg, settings, invocation_args);
    let working_dir = ai_working_dir(cfg, launch_cwd);
    // The files this harness needs on disk before its tab launches (OpenCode's
    // managed instructions file and its generated plugin). Kept off the pure
    // `compose_ai_env` path so the config builders stay test-safe.
    if let Some(p) = plugin {
        p.write_artifacts(cfg, settings, tab.as_str(), &working_dir);
        // V16 Feature 1: record the harness version this spawn is about to run,
        // for the drift tripwire. Fire-and-forget by contract — a version note
        // must never delay a tab launch. It used to sit inside the OpenCode arm
        // of `resolve_oob_source`, which made "is this harness versioned at
        // spawn" a property of where the call happened to be written.
        p.note_version(&cfg.command);
    }
    // The child's environment is composed FIRST, because since 2026-08-17 the
    // OpenCode tap's credential is read back out of it: the effective server
    // password is whatever the child is spawned with (including a per-tab
    // override), so the reader must derive its header from the same map rather
    // than from a value remembered at generation.
    let env = compose_ai_env(cfg, settings, tab.as_str(), endpoint.as_ref());
    // V20: resolve the out-of-band TTS source. For OpenCode this also injects
    // the `--port`/`--hostname` the fullscreen TUI hosts its event server on
    // (which the adapter taps). Mutates `extra_args`, so it runs on the real
    // launch path only — the pure `build_extra_args` stays test-stable.
    let oob = plugin.and_then(|p| p.resolve_oob(cfg, &working_dir, &mut extra_args, &env));
    let env_remove = ai_env_removals(cfg);
    Ok(PtyLaunchSpec {
        tab,
        binary,
        pre_args,
        extra_args,
        working_dir,
        env,
        env_remove,
        oob,
        // V33 Phase B (decision B1): which harness this tab is, resolved HERE
        // because this is the function that already knows — `build_pre_args`
        // (Claude) and `build_opencode_config` (OpenCode) branch on the very
        // same `command_is` test a few lines up. Resolving it again in the PTY
        // manager would give "which tabs are agent seams" two answers.
        //
        // An AI-tool tab whose command is NEITHER (a user pointed the entry at
        // something else) gets `None` and is not sandboxed: a grant table nobody
        // wrote is not a boundary, it is a tool that fails to start for reasons
        // the user cannot see.
        harness,
    })
}

/// V30 (review M9): environment markers of a harness session cImp was launched
/// from, stripped from every AI tab's child.
///
/// Launching cImp from inside a harness session is routine during development,
/// and the child then inherits that session's markers. The load-bearing one is
/// Claude's `CLAUDE_CODE_CHILD_SESSION`: a Claude spawned with it set runs with
/// **no transcript, no history, no session records** (spike-documented in
/// `docs/MILESTONE-V30-mcp-channels.md`), which silently blinds the out-of-band
/// tap — no TTS, no usage, no live-session registry entry, no V28 per-tab
/// scoping, and no log anywhere saying why. The others are the generic "you are
/// running inside <harness>" markers a fresh, user-facing tab must not claim to
/// be under.
///
/// Deliberately NOT a settings knob and deliberately not `env_clear`: this is a
/// fixed, minimal list of harness markers, so it needs no `spawn_inject_sig`
/// entry (nothing about it can change between spawns) and it cannot strip
/// anything the user's own environment legitimately carries.
///
/// **V40 Phase A: the union of every descriptor's `env_strip`**, not a literal
/// array. The variables are one harness's names, so they belong to that
/// harness's row; stripping *every* registered harness's markers from *every* AI
/// tab is deliberate and unchanged — an OpenCode tab launched from inside a
/// Claude session inherits the same misleading markers.
fn harness_env_vars() -> Vec<&'static str> {
    crate::harness::HARNESSES
        .iter()
        .flat_map(|d| d.env_strip.iter().copied())
        .collect()
}

/// The strip list for one AI tab: [`HARNESS_ENV_VARS`] minus anything the user
/// set explicitly on the tab — a per-tab `env` entry is an instruction, not an
/// accident, and `PtyManager` applies additions after removals anyway.
fn ai_env_removals(cfg: &AiToolTabConfig) -> Vec<String> {
    harness_env_vars()
        .into_iter()
        .filter(|k| !cfg.env.contains_key(*k))
        .map(|k| k.to_string())
        .collect()
}

/// The directory an AI tab launches in: its per-tab `cwd` override, else the
/// app's launch dir. THE one definition — [`build_ai_tool_spec`] (which hands it
/// to [`resolve_oob_source`], so it also becomes the Claude transcript root
/// behind the H1 same-root ambiguity predicate) and [`harness_tab_dirs`] (the
/// permission-hook cwd fallback) both call it, so the tab-identity seam and the
/// hook-attribution seam can never disagree about where a tab runs.
fn ai_working_dir(cfg: &AiToolTabConfig, launch_cwd: &Path) -> std::path::PathBuf {
    cfg.cwd.clone().unwrap_or_else(|| launch_cwd.to_path_buf())
}

/// The directory a configured AI tab launches in, by tab id — [`ai_working_dir`]
/// for one tab instead of a whole list (#48, finding F-3).
///
/// **Why this exists.** V32 activity rows written from the loopback taint gate
/// carried `root: ""`, so they could not be filtered per project — which is
/// exactly why the contamination row could not be shown on a per-project
/// surface. The root has to come from somewhere the caller cannot choose: the
/// request bodies carry a `cwd`, but that is the child's claim about itself,
/// and the row's whole purpose is to be trustworthy after an incident. A tab id
/// is config-derived and already validated against this same settings snapshot
/// (`latch::is_configured_tab`), so resolving the root *from the tab* keeps
/// the row's project attribution as trustworthy as its tab attribution.
///
/// `None` for an id that names no configured AI tab — including a Shell or
/// Preview tab, which host no harness. Callers decide their own fallback; this
/// function does not invent one, because "the tab does not exist" and "the tab
/// runs in the launch dir" are different facts.
///
/// Every AI tab kind, not just one harness's ([`harness_tab_dirs`]'s narrower set): the
/// taint latch scopes OpenCode tabs identically.
pub(crate) fn ai_tab_dir(
    settings: &Settings,
    tab_id: &str,
    launch_cwd: &Path,
) -> Option<std::path::PathBuf> {
    settings.tabs.iter().find_map(|t| match t {
        TabConfig::AiTool(c) if c.id == tab_id => Some(ai_working_dir(c, launch_cwd)),
        _ => None,
    })
}

/// NC-2 (issue #5): every configured AI tab of ONE harness and the working directory it
/// launches in — `(tab_id, working_dir)`. Resolution is [`ai_working_dir`], the
/// same call [`build_ai_tool_spec`] makes, so the permission-hook route can
/// compare a hook payload's `cwd` against the directory the tab was actually
/// spawned in.
///
/// Note the usual case is that NO tab sets `cwd`, so every tab of a harness shares the
/// launch dir — which is why the route's cwd match is only used as a
/// last-resort tie-break and only when it resolves to exactly one tab.
///
/// This lists CONFIGURED tabs (running or not) by design: it is the cwd
/// tie-break's candidate set, where an extra candidate can only make the route
/// refuse. The H1 ambiguity predicate needs the opposite posture — a
/// configured-but-closed tab must not degrade a running one — so it is fed by
/// the running taps themselves (`GraphService::mark_live_tab_root`), not by this
/// list.
/// **V40 Phase C, locked decision 22 — the harness is an argument now.**
/// This was `claude_tab_dirs`, filtered by a local `claude_harness()` that
/// spelled `"claude"` and carried a note saying Phase C would move it. The gap
/// it backs is a *harness's* gap (a hook payload that carries no cwd), so the
/// harness asking the question supplies its own id: the plugin route calls this
/// with the id it already is, and no core caller names one.
pub(crate) fn harness_tab_dirs(
    settings: &Settings,
    launch_cwd: &Path,
    harness: crate::harness::HarnessId,
) -> Vec<(String, std::path::PathBuf)> {
    settings
        .tabs
        .iter()
        .filter_map(|t| match t {
            TabConfig::AiTool(c)
                if crate::harness::HarnessId::from_command(&c.command) == Some(harness) =>
            {
                Some((c.id.clone(), ai_working_dir(c, launch_cwd)))
            }
            _ => None,
        })
        .collect()
}

/// Which HARNESS a configured AI tab belongs to, or `None`.
///
/// The one spelling of "which harness is this tab" above the seam — a thin
/// forward to [`crate::harness::HarnessId::from_command`], kept as a named
/// function because the *tab* is the unit callers hold.
///
/// **`None` is a first-class answer, and V40 Phase A is what made it one**
/// (locked decision 2). This used to be `tab_consumer`, returning
/// `"claude"` for a Claude command and **`"opencode"` for everything else** —
/// so a tab pointed at any other CLI (a wrapper script, a third harness) was
/// classified as OpenCode, became eligible for its Manual delegation slot, and
/// would be typed into with OpenCode's paste profile. Every one of its callers
/// now propagates the `None`.
///
/// **V33 C5 (F-4) is why one spelling matters.** `latch::is_configured_tab`
/// verifies a caller's asserted `(consumer, tab)` pair against this, so a tab
/// classified one way at spawn and another way at verification would be launched
/// with hooks whose `--tab` its own beacons could not key. Classifying both ends
/// through this function is what keeps the pair verifiable.
pub(crate) fn tab_harness(cfg: &AiToolTabConfig) -> Option<crate::harness::HarnessId> {
    crate::harness::HarnessId::from_command(&cfg.command)
}

/// Which harness the CONFIGURED tab with this id runs, or `None`.
///
/// The id-keyed twin of [`tab_harness`], for a caller that holds a `TabId` and
/// no config — the state manager's sub-agent stall backstop, which needs the
/// tab's declared activity tuning (V40 Phase D, locked decision 18). `None`
/// covers all three honest answers: no such tab, not an AI tab, or a command no
/// registered harness claims.
pub(crate) fn tab_harness_by_id(
    settings: &Settings,
    tab_id: &str,
) -> Option<crate::harness::HarnessId> {
    settings.tabs.iter().find_map(|t| match t {
        TabConfig::AiTool(c) if c.id == tab_id => tab_harness(c),
        _ => None,
    })
}

/// [`tab_harness`] as the CHP `agent` token, for the callers that key a map or a
/// wire field by it. `None` means the same thing it does there: not a harness.
pub(crate) fn tab_consumer(cfg: &AiToolTabConfig) -> Option<&'static str> {
    tab_harness(cfg).and_then(|h| h.id())
}

// The two "is the Code Audit MCP server advertised to this consumer" gates,
// factored out so the injection sites below and the restart-hint edge detector
// in `ipc::commands::settings_update` can never drift apart. The audit child is
// injected only at TAB SPAWN (`--mcp-config` / `OPENCODE_CONFIG_CONTENT`), so a
// running AI tab keeps its old server set until restarted — any edit that flips
// one of these must surface a restart hint.
//
// # V37 Phase F — the offload pair is gone, and that is the point
//
// There used to be FOUR gates here. `advertises_offload_to_claude` /
// `advertises_offload_to_opencode` (`offload.enabled ∨ graph.enabled ∨
// any_{claude,opencode}_mcp()`) decided whether the `cimp-offload` proxy child
// was written into a tab's harness config. **It is now unconditional for AI
// tabs**, so the predicates had no callers left and were deleted rather than
// left as a constant-true trap for the next reader.
//
// The reason is the V37 no-restart story: a tab spawned while all three
// disjuncts were false had no stdio child at all, hence no `tools/listChanged`
// relay, hence no way for a later MCP access grant to reach the running
// session — the one case contract C5's propagation could not serve. Injecting
// the child always costs an idle process per AI tab and buys live propagation
// for every toggle. The child's `tools/list` is assembled at call time, so a
// tab with nothing enabled advertises an EMPTY list.
//
// The knock-on effects, all of them, are: the two `spawn_inject_sig` `"mcp"`
// slots lost their offload element (an MCP access flip no longer nags every tab
// to restart), the `"channels"` entry lost its `advertises_offload_to_claude`
// conjunct, and [`injection_hygiene_applies`] lost the advertise gate under its
// feature switch.
//
// `advertises_audit_to_{claude,opencode}` and `read_advisor_gate_blocked` moved
// to `harness::plugin` in V40 Phase B. Both existed here only because the
// per-harness settings they read were core fields, and both were called by the
// plugins — an UPWARD edge from `harness/<id>/` into `tabs::config` for two
// one-line predicates. `audit_advertised(settings, harness)` is one body over
// the settings map instead of two bodies over a field pair, and the read-advisor
// gate is now asked of `harness::contract` directly, beside the row that
// declares it.

// ── V32 Phase F — native-web visibility (locked decision 14) ───────────────

// `native_web_for` and `consumer_hygiene_for` lived here until V42 (#124
// R25), as four-line forwards to `settings::injection::{native_web_mode,
// effective}` — plus a `NativeWebVisibility` alias for a type V32 Phase G had
// already renamed `NativeWebMode` and moved to `settings::injection`. Their
// callers were the two harness plugins — which V40 chartered to stop reaching
// into core `tabs::` for what is a settings question — plus, for hygiene, ONE
// caller in this file (`injection_hygiene_applies`). All of them resolve the
// same three-level hierarchy through `injection` directly now; the in-file
// one reads it inline, because a forward with a single caller in its own
// module is a rename, not an abstraction.
// The COMPOSITION those forwards documented is unchanged and documented where
// it happens (`injection::native_web_mode`, `Feature::ConsumerHygiene`);
// nothing about the modes, the scopes or the spawn-baked discipline moved.
//
// `tool_steering_for` below stays, and the difference is not caller count: it
// is not a forward. It const-asserts `Feature::ToolSteering.baked_at_spawn()`,
// so a feature that stopped being spawn-baked fails the BUILD here rather than
// silently costing a mid-session tab its restart hint.

/// Whether the managed-tool steering paragraph applies to one tab.
///
/// Resolved through [`injection::effective`](crate::settings::injection::effective)
/// at this tab's scope, like every other spawn-baked injection switch — so it
/// cannot drift into a raw settings read.
///
/// `baked_at_spawn` is const-asserted because this value is written into a
/// system-prompt addendum at launch: if the feature ever stopped reporting
/// `spawn_baked()`, `spawn_inject_sig` would stop moving for it and a tab
/// toggled mid-session would keep (or keep lacking) the paragraph with no
/// restart hint. That is a BUILD error here rather than a review finding.
pub(crate) fn tool_steering_for(s: &Settings, agent: &str, tab: &str) -> bool {
    const STEERING_AT_SPAWN: crate::settings::injection::Feature =
        crate::settings::injection::Feature::ToolSteering.baked_at_spawn();
    crate::settings::injection::effective(
        STEERING_AT_SPAWN,
        crate::settings::injection::Scope::Tab { agent, tab },
        s,
    )
}

// `opencode_native_gate_for` moved to `harness::opencode::plugin` in V40 Phase
// B. It resolved `Feature::HarnessNativeGate` under `Scope::Tab { agent:
// "opencode" }` — core spelling one harness's name to answer a question about a
// file only that harness's plugin writes — and both its callers were already
// inside `harness/opencode/`.

/// Per-harness spawn-injection signature.
///
/// Captures every Settings-derived input that reaches an AI tab **only at
/// spawn** (the `--mcp-config` server set, the `compose_capability_guidance`
/// gates, the `--settings` statusline/hooks overlay, the local-provider env,
/// the OpenCode plugin's baked flags and the injected `local-llama` provider).
/// Compared across a Settings save to decide whether a "restart the AI tab"
/// hint is due. Coarse by design: any difference means a fresh tab would be
/// launched differently from the one still running.
///
/// **V40 Phase A replaced the `[Value; 2]`** (locked decisions 8 and 25). It was
/// read POSITIONALLY by the restart-hint consumer, so a harness with no slot
/// meant a spawn-baked setting could flip with no restart hint and no diff —
/// the failure the mechanism exists to prevent.
///
/// **V40 Phase B made it the `BTreeMap<HarnessId, Value>` decision 8 asks for**,
/// and added the half a plugin can no longer forget: every field a plugin
/// declares `spawn_baked` in `settings_schema()` is folded in here
/// automatically, under the `"ext"` key. The flag and its restart-hint entry
/// are now ONE declaration — which closes the class V32 F-27 and V38 M-3 both
/// landed in, where a spawn-baked control shipped with no signature entry and
/// flipping it silently left every running tab on the old value.
pub(crate) fn spawn_inject_sig(s: &Settings) -> BTreeMap<crate::harness::HarnessId, Value> {
    crate::harness::registry::all()
        .map(|h| (h, harness_spawn_sig(s, h)))
        .collect()
}

/// One harness's slot: what its plugin composes, plus its declared spawn-baked
/// `ext` values.
///
/// The two halves are kept distinct in the object (`"ext"` is its own key)
/// rather than merged, so a plugin that declares an ext key sharing a name with
/// one of its hand-built entries cannot silently shadow it.
fn harness_spawn_sig(s: &Settings, h: crate::harness::HarnessId) -> Value {
    let Some(plugin) = h.plugin() else {
        return Value::Null;
    };
    let mut sig = plugin.spawn_sig(s);
    // V40 review M-4 (parity lens): a `spawn_baked` value that cannot reach any
    // tab's launch under these settings is left OUT, so editing it raises no
    // hint. Only the declaring plugin can answer that — see
    // `HarnessPlugin::spawn_baked_reaches_a_launch`, whose default is `true`.
    let baked: BTreeMap<&str, Value> = plugin
        .settings_schema()
        .iter()
        .filter(|f| f.spawn_baked && plugin.spawn_baked_reaches_a_launch(s, f.key))
        .map(|f| (f.key, s.harness_ext(h, f.key)))
        .collect();
    // An object is what every plugin returns today; a plugin that returned
    // something else would still get its ext half, wrapped, rather than losing
    // it to a silently skipped insert.
    match sig.as_object_mut() {
        Some(obj) => {
            obj.insert("ext".to_string(), serde_json::json!(baked));
        }
        None => sig = serde_json::json!({ "own": sig, "ext": baked }),
    }
    sig
}

/// Compose the capability-guidance addendum shared by Claude
/// (`--append-system-prompt`) and OpenCode (the managed instructions file):
/// the offload nudge (gated on `offload.enabled && offload.inject_guidance`)
/// and the code-graph nudge (gated on `graph.enabled`, with the semantic
/// addendum when `graph.semantic_search`). Sections are joined by a blank line.
/// Reusing the exact same sources keeps both agents in lockstep.
///
/// V20: the `[[TTS]]` markup convention is NO LONGER injected — AI tabs are
/// fullscreen and TTS is sourced out-of-band (`crate::harness::reader`), which speaks all
/// assistant prose directly. The per-tab `tts_injection.enabled` flag is now
/// the "speak this tab" gate read by the out-of-band sources, not a prompt
/// injection toggle (the former free-text `instructions` field is gone).
///
/// V12 Phase E: when `graph.promote_pinned_facts` is on, a marked
/// `## cImp project facts` block of PINNED facts is appended last (see
/// [`fact_promotion_block`]) — launch-time only, best-effort.
pub(crate) fn compose_capability_guidance(cfg: &AiToolTabConfig, settings: &Settings) -> String {
    let mut addendum = String::new();
    // V32 Phase D: the data-not-instructions contract goes FIRST — it governs
    // how every tool result below it must be read, so it should be in context
    // before the tools are described.
    //
    // Gated on the consumer-hygiene feature switch and nothing else — see
    // [`injection_hygiene_applies`] for why V37 Phase F removed the
    // "is anything advertised" half.
    if injection_hygiene_applies(cfg, settings) {
        addendum.push_str(crate::harness::instructions::text(
            tab_harness(cfg),
            crate::harness::instructions::Slot::InjectionHygiene,
        ));
    }
    // Managed-tool steering, beside the hygiene paragraph and through the same
    // channel. Gated per tab, and its `run_command` half gated additionally on
    // this consumer's exposure flag — the flag decides whether that tool is
    // advertised at all, and a paragraph recommending a tool the session cannot
    // see is worse than no paragraph.
    // V40 Phase A: a tab that runs no registered harness gets no per-agent
    // guidance. `tool_steering_for` / `commands_exposed_to` / `fact_promotion_
    // block` are all keyed by the CHP agent token, and there is no honest token
    // for a command nobody registered — resolving one would mean asking a
    // question about a harness this tab is not.
    // `None` = a tab that runs no registered harness. The neutral nudges below
    // still compose (they name no agent); only the per-agent gates are skipped,
    // because there is no honest CHP token to resolve them under.
    let agent = tab_consumer(cfg);
    if agent.is_some_and(|a| tool_steering_for(settings, a, &cfg.id)) {
        if !addendum.is_empty() {
            addendum.push_str("\n\n");
        }
        // V40 Phase B: the `run_command` half of the paragraph is gated by
        // THIS tab's harness row. `agent` is `Some` here (the `if` above) and
        // it came from `tab_consumer`, so the registry lookup cannot miss.
        let harness = agent.and_then(crate::harness::HarnessId::from_id);
        addendum.push_str(&tool_steering_guidance(
            tab_harness(cfg),
            harness.is_some_and(|h| settings.harness_settings(h).expose_commands),
        ));
    }
    if settings.offload.enabled && settings.offload.inject_guidance {
        if !addendum.is_empty() {
            addendum.push_str("\n\n");
        }
        addendum.push_str(crate::harness::instructions::text(
            tab_harness(cfg),
            crate::harness::instructions::Slot::Offload,
        ));
    }
    if settings.graph.enabled {
        if !addendum.is_empty() {
            addendum.push_str("\n\n");
        }
        // V40 Phase E, locked decision 24: the graph nudge is model-visible text
        // and it NAMES a harness tool ("over a full Read", "the test command in
        // Bash"). It comes from the instruction inventory now, rendered in this
        // tab's own vocabulary — a tab that runs no registered harness gets the
        // neutral rendering rather than Claude's tool ids, which is what it used
        // to be handed.
        let harness = tab_harness(cfg);
        addendum.push_str(crate::harness::instructions::text(
            harness,
            crate::harness::instructions::Slot::GraphGuidance,
        ));
        if settings.graph.semantic_search {
            addendum.push_str(crate::harness::instructions::text(
                harness,
                crate::harness::instructions::Slot::GraphSemantic,
            ));
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
        if let Some(block) =
            agent.and_then(|a| fact_promotion_block(&root, settings, a, &cfg.id))
        {
            if !addendum.is_empty() {
                addendum.push_str("\n\n");
            }
            addendum.push_str(&block);
        }
    }
    addendum
}

/// V32 Phase D: does the untrusted-content contract apply to a tab launched
/// from `cfg`? The paragraph teaches the session the vocabulary of
/// spotlight-enveloped EXTERNAL results, detector warning headers and
/// taint-latch refusals — everything that arrives through the `cimp-offload`
/// proxy.
///
/// V32 Phase G: consumer hygiene is one of the eleven switchable controls (ten
/// until Phase H added `opencode_native_gate`; count corrected 2026-08-08, #48),
/// and the paragraph is spawn-baked, so its L2 and L3 both ride
/// `spawn_inject_sig` through `injection::spawn_sig`. That switch is now the
/// WHOLE predicate.
///
/// # V37 Phase F removed the advertise gate underneath the switch
///
/// It used to also require `advertises_offload_to_{claude,opencode}` — "with no
/// cImp tool surface there is no marker vocabulary to teach". Phase F makes that
/// reasoning unsound in the direction that matters: the proxy child is now in
/// EVERY AI tab and its surface changes LIVE, so a tab launched with zero grants
/// can be handed a fetched page's bytes ten minutes later. This paragraph is
/// spawn-baked; a live-changing input can no longer gate it without leaving a
/// window in which EXTERNAL content reaches a session that was never taught how
/// to read it. Teaching it always is the fail-safe direction, and it is what the
/// switch's OTHER half already does — `build_opencode_config` writes the pinned
/// `permission` block on `Feature::ConsumerHygiene` alone, with no advertise
/// gate.
///
/// The cost, stated: a user with every cImp feature off now gets one paragraph
/// of `--append-system-prompt` (or one managed instructions file) per AI tab.
/// Turning the consumer-hygiene control off is the escape hatch, exactly as it
/// is for the permission pins.
///
/// Still consumer-specific, because the feature resolves per tab and per
/// agent. **V40 Phase A**: a command that names no registered harness is
/// no longer "treated as OpenCode" — it has no consumer, so it gets no
/// paragraph, which is the same answer the rest of its launch path gives it.
fn injection_hygiene_applies(cfg: &AiToolTabConfig, settings: &Settings) -> bool {
    // V40 Phase A: a tab that runs no registered harness has no consumer, so
    // there is no hygiene setting resolved for it and no paragraph to inject.
    tab_consumer(cfg).is_some_and(|agent| {
        crate::settings::injection::effective(
            crate::settings::injection::Feature::ConsumerHygiene,
            crate::settings::injection::Scope::Tab {
                agent,
                tab: &cfg.id,
            },
            settings,
        )
    })
}

/// V12 Phase E: the `## cImp project facts` launch-time addendum — PINNED
/// facts only, newest-pinned first, capped ~1500 chars. `None` when the graph
/// hasn't been built at `root` yet, has no pinned facts, or can't be opened —
/// best-effort, same posture as this module's other launch-time I/O (e.g.
/// [`write_opencode_instructions`]).
///
/// V32 Phase C2 (locked decision 10): this is THE auto-injection path — the one
/// that carries a past session's words into a fresh, clean one — so it is also
/// where memory quarantine matters most. Two things guard it:
///
/// - **Quarantined notes can never reach it.** The block is built from
///   `project_fact` rows, and the only automatic producer of those is the
///   distiller, which reads notes through `GraphIndex::mem_notes` — where
///   tainted rows are filtered out at the storage layer. So a quarantined note
///   cannot become a fact and then be injected.
/// - **What does reach it is spotlight-enveloped**, including facts that are
///   not tainted at all. Pre-V32 memory is unauditable by construction, and this
///   block lands in a system-prompt addendum, the highest-trust position in the
///   session — exactly where an old injected line would do the most damage. The
///   Phase D guidance addendum (composed just above, in
///   [`compose_capability_guidance`]) already names "recalled memory" as a
///   marker-wrapped source, so the session knows how to read it.
///
/// **This decision is BAKED (#48, M-3).** It is taken once, at launch, and
/// written into a system-prompt addendum, so flipping spotlighting cannot
/// change a running tab — which is why `Feature::Spotlighting` is
/// `spawn_baked` and why `spawn_inject_sig` raises the restart hint for it.
fn fact_promotion_block(
    root: &Path,
    settings: &Settings,
    agent: &str,
    tab: &str,
) -> Option<String> {
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
    // Wrap once, at delivery, with a fresh nonce — the same discipline the
    // proxy's EXTERNAL results follow. The cap above is applied to the FACTS,
    // before wrapping, so the envelope can never be the thing that gets
    // truncated away.
    //
    // V32 Phase G: unless spotlighting resolves off for this tab. The block
    // itself is NOT gated by the switch — the facts are the user's own memory
    // and withholding them would be a feature switch quietly disabling an
    // unrelated feature; only the envelope around them is.
    //
    // #48 (M-3): this resolution happens at LAUNCH and is baked into the system
    // prompt, so the control owes the user a restart hint. `baked_at_spawn` is
    // const-asserted, which makes "the feature is not in `Feature::spawn_baked`"
    // a BUILD ERROR here rather than a tab that keeps injecting unenveloped
    // pre-V32 memory until someone notices.
    const SPOTLIGHT_AT_SPAWN: crate::settings::injection::Feature =
        crate::settings::injection::Feature::Spotlighting.baked_at_spawn();
    if !crate::settings::injection::effective(
        SPOTLIGHT_AT_SPAWN,
        crate::settings::injection::Scope::Tab { agent, tab },
        settings,
    ) {
        return Some(out);
    }
    Some(crate::offload::spotlight::recall_envelope(&out))
}

/// V30 Phase A: the Claude Code flag that registers our stdio MCP child as a
/// **session channel**, letting it push `notifications/claude/channel` frames
/// straight into the live session (they surface as `<channel source="…">`
/// messages at the next turn boundary). The argument is
/// `server:<mcpServers key>` — `cimp-offload`, the key `build_pre_args` writes
/// into the `--mcp-config` overlay below; the two MUST stay in lockstep.
///
/// ⚠ **Research-preview contract — this flag may be renamed or removed.**
/// Verified against Claude Code 2.1.222 (V30 Phase 0 spike, 2026-08-05): the
/// flag is hidden from `--help`, registration is silent (no consent dialog),
/// and it paints a persistent "Channels (experimental)" banner plus a cosmetic
/// "no MCP server configured with that name" warning (the dev-flag validation
/// runs before `--mcp-config` files load; function is unaffected). The proper
/// `--channels` flag is allowlisted to `plugin:@marketplace` entries only, so
/// this is the only registration path for a bare `mcpServers` server. See
/// `docs/MILESTONE-V30-mcp-channels.md` for the full contract + drift notes.
pub(crate) const CHANNEL_REGISTRATION_FLAG: &str = "--dangerously-load-development-channels";

/// The channel target passed to [`CHANNEL_REGISTRATION_FLAG`]: our offload MCP
/// child, addressed by its `mcpServers` key.
pub(crate) const CHANNEL_REGISTRATION_TARGET: &str = "server:cimp-offload";

/// V30 (M5): the SERVER half of the same gate — our own flag on the
/// `cimp-offload` child's argv, telling it to declare
/// `capabilities.experimental["claude/channel"]` at `initialize`.
///
/// Emitted from the same `settings.offload.session_push` read as
/// [`CHANNEL_REGISTRATION_FLAG`], one line apart, so the two halves cannot
/// disagree — see `offload/mcp.rs::session_push_enabled` for why the child must
/// not decide this from settings on its own.
pub(crate) const CHANNEL_PUSH_FLAG: &str = "--channel-push";

/// The **managed-tool steering** addendum: prefer cImp's `run_check` /
/// `run_command` MCP tools over running the equivalent commands in the
/// harness's own built-in shell.
///
/// # Fixed and GENERIC, by design
///
/// It names no check, no binary, no path — only the two MCP tools, and it points
/// at their own `name` / `tool` enums for the contents. Three reasons, and the
/// first is the one that makes this a rule rather than a style preference:
///
/// - **The enums are live; a prompt is not.** Since V38 the native tool surface
///   re-advertises itself to a RUNNING tab (`graph::mcp`'s pulse) whenever a
///   check is enabled, a binary is detected or a path is repointed. An enumerated
///   paragraph is written once, at spawn, and would start lying the first time
///   the user touched the registry — while the tool's own schema stayed correct.
/// - **It would move the spawn signature on every registry edit.** Anything the
///   paragraph names is a spawn-baked input, so it must ride
///   [`spawn_inject_sig`] (that is the whole point of
///   [`Feature::spawn_baked`](crate::settings::injection::Feature::spawn_baked)).
///   Naming the registry would therefore nag every open tab to restart for a
///   change that already reached it live — the exact way a restart hint stops
///   being read.
/// - **Token cost.** This rides every session beside the hygiene, offload and
///   graph nudges, and a list that grows with the user's plugin directory would
///   push the useful ones out of attention.
///
/// `commands_exposed` is the tab consumer's `tool_plugins.expose_commands_*`
/// flag as resolved at spawn. Off ⇒ the `run_command` sentence is absent
/// **entirely** rather than softened: with the flag off the tool is not
/// advertised, and steering a session toward a tool it cannot call is worse than
/// staying quiet.
///
/// **V40 Phase G: the three sentences live in the instruction inventory**
/// (`harness::instructions`, locked decision 24). They are neutral - the same
/// bytes for every harness - but the inventory's question is *what does the
/// model see*, and an answer that omitted the neutral half was not an answer.
/// This function is the GATE, which is the part that is not text: the
/// `run_command` sentence is a separately inventoried slot precisely because it
/// is separately withheld.
fn tool_steering_guidance(harness: Option<crate::harness::HarnessId>, commands_exposed: bool) -> String {
    use crate::harness::instructions::{text, Slot};
    let mut out = String::from(text(harness, Slot::ToolSteeringChecks));
    if commands_exposed {
        out.push_str(text(harness, Slot::ToolSteeringCommands));
    }
    out.push_str(text(harness, Slot::ToolSteeringTail));
    out
}

fn build_extra_args(
    cfg: &AiToolTabConfig,
    _settings: &Settings,
    invocation_args: &[String],
) -> Vec<String> {
    let plugin = crate::harness::HarnessId::from_command(&cfg.command).and_then(|h| h.plugin());
    let mut out: Vec<String> = Vec::new();

    // A harness may REFUSE one of the tab's stored arguments (OpenCode's
    // `--mini`, which its own launch flags make fatal). The refusal is a
    // correction with a log line, not a launch failure — see
    // `HarnessPlugin::arg_is_rejected`.
    for arg in cfg.args.iter().filter(|s| !s.is_empty()) {
        if let Some(why) = plugin.and_then(|p| p.arg_is_rejected(arg)) {
            tracing::warn!(tab = %cfg.id, arg = %arg, why, "dropping an argument this harness refuses");
            continue;
        }
        out.push(arg.clone());
    }

    // cImp is documented as a drop-in replacement for one harness's binary, so
    // invocation args (`cimp --resume <id>`, etc.) flow into that harness's
    // tabs. A harness that selects its model and session through config rather
    // than flags declares `false` and gets none of them — forwarding another
    // CLI's flags into it is how a tab fails to launch.
    if plugin.is_some_and(|p| p.accepts_passthrough_argv()) {
        for arg in invocation_args {
            if !arg.is_empty() {
                out.push(arg.clone());
            }
        }
    }
    out
}

/// V1.4-07 / V14: compose the spawn environment for an AI tab. Per-tab
/// `env` entries are the user's most-specific scope and take
/// precedence over synthesized values. The merge order is:
/// synthesized → tab.env (per-tab keys never get overwritten).
///
/// Synthesis is the PLUGIN's, resolved from the tab's command and not from the
/// `TabId` variant — so a `+`-spawned duplicate is treated exactly like the
/// reserved tab it was cloned from, and a tab whose command matches no
/// registered harness gets no synthesized provider env at all (the user's own
/// configuration is in charge).
///
/// **What each harness synthesizes is documented with that harness**
/// (`HarnessPlugin::compose_env`, and the config-file half behind
/// `HarnessPlugin::config_writer` — locked decision 26). This list used to be
/// here, naming one harness's `ANTHROPIC_*` variables and another's config
/// document, which made a core function the place an upstream env rename would
/// have to be noticed.
///
/// V28: `tab` is passed straight through to [`build_opencode_config`], which
/// bakes it into the `cimp-offload` child's argv.
fn compose_ai_env(
    cfg: &AiToolTabConfig,
    settings: &Settings,
    tab: &str,
    endpoint: Option<&crate::offload::discovery::Discovery>,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    // Everything harness-specific — the hook bearer token, the injected config
    // document, the server credential, the local-provider variables — is the
    // plugin's (V40 locked decision 4). A tab whose command matches no
    // registered harness gets none of it, which is the point: synthesizing one
    // harness's variables into another's child is how a launch fails for reasons
    // the user cannot see.
    if let Some(p) = crate::harness::HarnessId::from_command(&cfg.command).and_then(|h| h.plugin())
    {
        p.compose_env(cfg, settings, tab, endpoint, &mut env);
    }
    // Per-tab env wins over synthesized values — the user's most specific scope,
    // applied last so no plugin can overwrite it.
    for (k, v) in &cfg.env {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// The module's unit tests, in a sibling directory (#132): `config.rs` was 899
/// production lines under 4,585 test lines, split into one file per section the
/// module already had. Same crate, same module, same privacy.
#[cfg(test)]
#[path = "config/tests/mod.rs"]
mod tests;
