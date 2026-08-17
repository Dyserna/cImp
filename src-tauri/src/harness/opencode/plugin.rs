//! **OpenCode's generated plugin** — its source, the per-tab flags baked into
//! it, when it is written or swept, and the CHP hello it opens with (V35 Phase
//! K: moved verbatim from `tabs/config.rs`, design § 4's moves table).
//!
//! This is OpenCode's L1 emitter, the twin of
//! [`crate::harness::claude::overlay`] — and the one the design calls "the
//! better design, and the one to standardize on": a single generated artifact
//! that speaks the harness's own extension mechanism on one side and CHP on the
//! other. It used to sit inside a 7000-line file about tab launching, with every
//! JS brace doubled, which is hostile to the one thing this milestone is for —
//! reading a diff when upstream changes.
//!
//! **This file is inside the TCB (design § 5, D7).** The V32 Phase H
//! native-tool refusal is a `throw` in the generated `tool.execute.before`, and
//! only the plugin sits in the harness's own tool path; the V33 Phase F
//! checkpoint trigger and the V32 taint beacon ride the same handler. Editing
//! it is editing a security control. Its honest limits are stated on
//! [`opencode_plugin_source`] and repeated inside the emitted file for whoever
//! reads it on disk.
//!
//! The plugin is **spawn-baked**: written at tab launch, it outlives the binary
//! that wrote it — which is why every body it POSTs carries `chp`
//! ([`crate::harness::chp`]) and why `spawn_inject_sig` carries the flags.
//!
//! Phase M turns the `format!()` template into a real `.js` file; Phase K moved
//! the Rust only, the template string intact.

use std::path::Path;

use crate::settings::injection::NativeWebMode as NativeWebVisibility;
use crate::settings::{Settings, TabConfig};
use super::config::git_exclude_opencode;
use crate::tabs::config::{native_web_for, opencode_native_gate_for};

/// Filename stem of the generated OpenCode plugin, one file **per tab**.
///
/// V32 #48 (H-2): it used to be a single `cimp-inject.js` per *directory*, while
/// `opencode_plugin_wanted` and the baked `beacon`/`native_gate` flags are
/// resolved per TAB and `ai_working_dir` hands every builtin tab the same launch
/// cwd. One file, N tabs, last spawn wins: duplicating the OpenCode tab with `+`
/// and leaving the copy at the app-wide default silently replaced the original's
/// posture, with nothing surfacing it — `injection_status` still reported the
/// original's *resolved* gate as on, and `spawn_inject_sig` compared equal
/// because both consumers embed the same blob.
const OPENCODE_PLUGIN_PREFIX: &str = "cimp-inject-";

/// The pre-#48 single-file artifact. Removed on every OpenCode spawn so an
/// upgrade cleans up after itself — left in place it would keep running (with a
/// dead port and token, so inert, but also with whichever tab's flags were baked
/// last) alongside the per-tab files.
const OPENCODE_PLUGIN_LEGACY: &str = "cimp-inject.js";

/// V10: write (or remove) the OpenCode injection/memory plugin in the project's
/// `.opencode/plugin/cimp-inject-<tab>.js`. The plugin is dependency-free (node
/// builtins + `fetch`, captured into a module-scope binding at load rather than
/// read off `globalThis` per call — V33 C6; so OpenCode does not run a
/// launch-time `bun install`) and bakes in the current loopback port + token — regenerated
/// each launch since the token rotates per app run (idempotent overwrite). It
/// serves two hooks:
///   * `chat.message` → POST the prompt to `/context/retrieve` and append the
///     digest **in place** on the existing text part (schema-safe; verified in
///     the D0 spike), gated by the baked-in inject flag; and
///   * `tool.execute.after` → POST to `/memory/event` (the sole memory ingress
///     for OpenCode, whose OOB SSE stream carries no tool events).
///
/// V32 Phase F adds a third: `tool.execute.before` → POST to `/latch/beacon`
/// when the model reaches for OpenCode's OWN `webfetch`/`websearch`. V32 Phase H
/// extends that same handler from beacon-only to beacon-and-GATE, behind a
/// default-off setting — see [`opencode_plugin_source`] for its honest limits.
///
/// Removed when THIS tab wants nothing ([`opencode_plugin_wanted`]). Also adds
/// `.opencode/` to the project's `.git/info/exclude` so the generated plugin and
/// OpenCode's own `.opencode/.gitignore` don't dirty `git status`.
pub(crate) fn write_opencode_plugin(working_dir: &Path, settings: &Settings, tab: &str) {
    let dir = working_dir.join(".opencode").join("plugin");
    let plugin_path = dir.join(format!("{OPENCODE_PLUGIN_PREFIX}{tab}.js"));

    // Housekeeping first, so it runs whatever this tab's own answer is: drop the
    // pre-#48 single-file artifact and any per-tab file whose tab is gone.
    let _ = std::fs::remove_file(dir.join(OPENCODE_PLUGIN_LEGACY));
    sweep_stale_opencode_plugins(&dir, settings);

    // Nothing to inject, record OR watch → clean up a stale plugin. THIS tab's
    // predicate only; see [`sweep_stale_opencode_plugins`].
    if !opencode_plugin_wanted(settings, tab) {
        let _ = std::fs::remove_file(&plugin_path);
        return;
    }
    // Need the loopback endpoint to reach the app. This runs IN the app at tab
    // spawn, so it must bake THIS instance's endpoint — `read_own_discovery`
    // (pid-keyed), never the shared last-writer-wins file a sibling instance may
    // have overwritten.
    //
    // #48: a missing discovery file used to take the same DELETE branch as
    // "nothing wants it". It is not the same thing — the tab wants the plugin
    // and we cannot write a working one — and on an install where
    // `loopback_needed()` is false (offload, graph and the audit MCP all off,
    // native-web on `sensor`) that branch fired on every spawn, so the sensor
    // was reported live everywhere while no plugin existed on disk. Leave
    // whatever is there alone and say so: a stale file's baked port/token simply
    // fail to connect, and this file's whole posture is "never throws, never
    // denies on doubt", so a dead endpoint costs a beacon, not a session.
    let Some(disc) = crate::offload::loopback::read_own_discovery() else {
        tracing::warn!(
            target: "tabs",
            tab,
            "opencode plugin: no loopback discovery for this instance; \
             leaving any existing plugin in place and skipping the rewrite \
             (its beacon/gate cannot reach the app until the loopback runs)"
        );
        return;
    };

    let inject_enabled = settings.graph.enabled && settings.graph.context_injection;
    // V12 Phase F (6a/6b): same gate as the Claude PostToolUse hook — auto-check
    // needs the graph AND at least one configured check.
    let auto_check_enabled =
        settings.graph.enabled && settings.graph.auto_check && !settings.checks.is_empty();
    let js = opencode_plugin_source(
        disc.port,
        &disc.token,
        tab,
        OpencodePluginFlags {
            inject: inject_enabled,
            auto_check: auto_check_enabled,
            beacon: native_web_for(settings, "opencode", tab) == NativeWebVisibility::Sensor,
            native_gate: opencode_native_gate_for(settings, tab),
            // V33 Phase F: the app-wide checkpoint switch, the same one
            // `WorkbenchService::checkpoints_enabled` reads and the same one
            // that gates the Claude `--checkpoint-beacon` hook. No graph
            // dependency — `/workbench/tool_checkpoint` is Workbench's own
            // route, unlike the `/context/retrieve` prompt tap which carries
            // checkpointing as a passenger on a graph feature.
            checkpoint: settings.workbench.checkpoints,
        },
    );

    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(&plugin_path, js);
    git_exclude_opencode(working_dir);
}

/// Remove generated plugin files belonging to tab ids that are no longer
/// configured at all (a duplicated tab the user deleted, a renamed reserved id).
///
/// **Keyed on existence, never on another tab's predicate.**
/// [`opencode_plugin_wanted`]'s own docs lock the reason: "a gate that
/// disappears when an unrelated feature is toggled is worse than no gate", and
/// from tab A's side tab B's settings are exactly such an unrelated feature —
/// which is how the single-file artifact went wrong in the first place. A tab
/// that still exists but no longer wants its plugin drops it at its own next
/// spawn, in the branch above.
fn sweep_stale_opencode_plugins(dir: &Path, settings: &Settings) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // no plugin dir yet — nothing to sweep
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name
            .strip_prefix(OPENCODE_PLUGIN_PREFIX)
            .and_then(|rest| rest.strip_suffix(".js"))
        else {
            continue; // not ours (OpenCode's own plugins live here too)
        };
        let configured = settings
            .tabs
            .iter()
            .any(|t| matches!(t, TabConfig::AiTool(c) if c.id == id));
        if !configured {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// V32 Phase F: whether the generated OpenCode plugin should exist at all.
///
/// **This is the E2 spike's fail-open trap, closed.** Until Phase F the plugin
/// was written if and only if `graph.enabled`, and DELETED otherwise — so any
/// security-relevant handler riding it would vanish the moment a user turned
/// the code graph off, with no error and no UI trace. A gate that disappears
/// when an unrelated feature is toggled is worse than no gate, because the
/// `/status` view would still show the tab as `open` while its native web tool
/// ran unobserved. The write condition is therefore the OR of every consumer's
/// need, and each handler carries its own baked flag inside the file.
///
/// Pure (settings in, bool out), so the write condition is one expression a
/// reviewer can read in one place.
///
/// **Corrected 2026-08-08 (#48, review Part 7 item 10).** This used to say
/// "so both `write_opencode_plugin` and [`spawn_inject_sig`] read the same
/// predicate and the restart hint can never disagree with what a fresh tab
/// would write". Only the first of those calls it. `spawn_inject_sig`
/// **reconstructs** the condition: `graph.enabled` rides its `plugin[0]` entry,
/// and the native-web and Phase H halves ride
/// `injection::spawn_sig(s, Consumer::Opencode)` instead. The two do add up
/// today — that is asserted, not assumed — but by argument rather than by
/// construction, so **a fourth disjunct added here needs a matching
/// `spawn_inject_sig` input or the plugin changes with no restart hint.**
///
/// V32 Phase G: per-TAB, because the sensor half is now resolved per tab. The
/// graph half is app-wide and stays so.
pub(crate) fn opencode_plugin_wanted(s: &Settings, tab: &str) -> bool {
    // V10/V12/V24: context injection, the memory/usage tap and auto-check.
    s.graph.enabled
        // V32 Phase F: the native-web beacon (sensor mode only — `deny` needs
        // no plugin, the pinned permission block does that work, and `off`
        // wants nothing installed at all).
        || native_web_for(s, "opencode", tab) == NativeWebVisibility::Sensor
        // V32 Phase H: the native-tool gate. Its own consumer of the same trap:
        // without this line, turning the graph off (and native-web to `off`)
        // would delete the file carrying a gate the user switched ON — the
        // security control vanishing because an unrelated feature moved, which
        // is precisely what this predicate exists to prevent.
        || opencode_native_gate_for(s, tab)
        // V33 Phase F: the pre-mutation checkpoint POST. The FOURTH disjunct the
        // note above warns about — so `spawn_inject_sig`'s opencode `"plugin"`
        // array gained a matching `s.workbench.checkpoints` entry in the same
        // change, and `the_plugin_predicate_and_the_restart_hint_agree` asserts
        // the two still add up. Without this line an OpenCode tab with the graph
        // off would silently lose its Timeline rewind points.
        || s.workbench.checkpoints
}

/// The per-tab, Settings-derived switches baked into one generated plugin file.
///
/// A struct rather than positional `bool`s: the list reached four in V32 Phase
/// H and five in V33 Phase F, and a call site that transposes `beacon` and
/// `native_gate` would turn a report-only sensor into a denial with no compiler
/// complaint — the same reasoning that made `toolclass::CallGuards` a struct.
/// `Default` is all-false — "the plugin is written, every optional handler is
/// inert" — which is what the tests spell `..Default::default()` around.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpencodePluginFlags {
    /// V10/V13: apply the `/context/retrieve` digest to the draft prompt.
    pub(crate) inject: bool,
    /// V12 Phase F: fire `/context/post_edit` after an edit tool.
    pub(crate) auto_check: bool,
    /// V32 Phase F: beacon on the harness's own web tools (sensor mode).
    pub(crate) beacon: bool,
    /// V32 Phase H: also DENY native tools against the tab's taint latch.
    pub(crate) native_gate: bool,
    /// V33 Phase F: POST `/workbench/tool_checkpoint` before a native tool that
    /// can change files on disk, so the Timeline has a rewind point attributed
    /// to that exact call.
    pub(crate) checkpoint: bool,
}

/// The dependency-free OpenCode plugin source, with the loopback port + token
/// and the per-tab flags baked in.
///
/// # V32 Phase H — honest limits of the native-tool gate (locked decision 17)
///
/// The gate below is a **policy control, not containment**, and the generated
/// file says so to whoever reads it on disk:
///
/// - it runs inside the agent's own process, so anything that agent can execute
///   runs outside it — `OPENCODE_PURE=1`, or simply spawning a second, ungated
///   `opencode`, walks around it;
/// - a user-typed `!shell` command and the raw PTY route never reach a plugin
///   hook at all;
/// - and `bash` remains egress-capable by nature, so the LOCAL side of the latch
///   is a boundary against the *model's tool calls*, not against the machine.
///
/// OS-level containment is V33's job. What this buys is the thing the E2 spike
/// measured: a compromised model that reaches for `read` after fetching a
/// hostile page is refused, in the same session, with a message it can read.
pub(crate) fn opencode_plugin_source(
    port: u16,
    token: &str,
    tab: &str,
    flags: OpencodePluginFlags,
) -> String {
    // V32 Phase H: the two name sets come from the ONE reviewed table
    // (`toolclass::OPENCODE_NATIVE_TABLE`), rendered through serde so the JS
    // literal cannot be malformed by a name someone adds later. The web set is
    // rendered from the same table for the same reason, and a test pins it
    // against the beacon's `CIMP_WEB_TOOLS`.
    let json_list = |names: Vec<&'static str>| {
        serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string())
    };
    let native_local_tools = json_list(crate::harness::opencode::tools::opencode_native_names(
        crate::offload::toolclass::ToolClass::LocalCapability,
    ));
    let native_web_tools = json_list(crate::harness::opencode::tools::opencode_native_names(
        crate::offload::toolclass::ToolClass::External,
    ));
    // V33 Phase F: the checkpoint set, from the SAME table's `mutates_fs`
    // column. It cuts across the class axis rather than along it (`bash` is
    // local-capability AND mutating; `read` is local-capability and not), which
    // is why it has its own accessor instead of reusing `json_list` over a
    // class.
    let native_mutating_tools =
        json_list(crate::harness::opencode::tools::opencode_native_mutating_names());
    // The fixed refusals, JSON-quoted into JS string literals — never hand-quoted:
    // they contain apostrophes and em dashes, and an escaping bug here would be a
    // syntax error in a file the harness loads at startup.
    let refusal_local = serde_json::to_string(crate::offload::toolclass::REFUSAL_NATIVE_LOCAL_BLOCKED)
        .unwrap_or_else(|_| "\"REFUSED (security boundary)\"".to_string());
    let refusal_web = serde_json::to_string(crate::offload::toolclass::REFUSAL_NATIVE_WEB_BLOCKED)
        .unwrap_or_else(|_| "\"REFUSED (security boundary)\"".to_string());
    let refusal_web_tainted =
        serde_json::to_string(crate::offload::toolclass::REFUSAL_NATIVE_WEB_TAINTED)
            .unwrap_or_else(|_| "\"REFUSED (security boundary)\"".to_string());
    let refusal_web_user_local =
        serde_json::to_string(crate::offload::toolclass::REFUSAL_NATIVE_WEB_USER_LOCAL)
            .unwrap_or_else(|_| "\"REFUSED (security boundary)\"".to_string());
    // V35 Phase I (CHP): what this plugin will and will not push, with THIS
    // tab's flags already applied — the `serves` / `cannot` halves of the
    // `/session/hello` body (design D3). Built in Rust from the same flags the
    // handlers are baked with, so the declaration cannot claim something the
    // generated file does not do, and rendered through serde like every other
    // list here so a future event id can never malform the emitted JS.
    let (serves, cannot) = {
        use crate::harness::chp as chp;
        // Unconditional: the prompt tap always POSTs (only APPLYING the digest
        // is gated), the memory tap is the sole OpenCode memory ingress, and the
        // hello is this message itself.
        let mut serves: Vec<&'static str> = vec![chp::EV_HELLO, chp::EV_PROMPT, chp::EV_MEMORY_EVENT];
        let mut cannot: Vec<serde_json::Value> = Vec::new();
        let mut declare = |on: bool, id: &'static str, why: &'static str| {
            if on {
                serves.push(id);
            } else {
                cannot.push(serde_json::json!({ "id": id, "why": why }));
            }
        };
        declare(
            flags.auto_check,
            chp::EV_CONTEXT_POST_EDIT,
            "auto-check is off for this tab (needs the graph, `graph.auto_check`, and at least \
             one configured check)",
        );
        declare(
            flags.beacon,
            chp::EV_TAINT_BEACON,
            "native web visibility is not `sensor` for this tab — in `deny` the pinned permission \
             block refuses those tools outright, so there is nothing to observe",
        );
        declare(
            flags.native_gate,
            chp::EV_TOOL_GATE,
            "the native-tool gate is off for this tab",
        );
        declare(
            flags.checkpoint,
            chp::EV_CHECKPOINT_PRE_MUTATION,
            "Workbench checkpoints are off",
        );
        (
            serde_json::to_string(&serves).unwrap_or_else(|_| "[]".to_string()),
            serde_json::to_string(&cannot).unwrap_or_else(|_| "[]".to_string()),
        )
    };
    format!(
        r#"// Generated by cImp (V10 Code Intelligence). Do not edit — regenerated each launch.

// ── V33 (C6, H-7's cheap half): the ONE `fetch` this file will ever use ─────
//
// Bound HERE, once, while this module is being evaluated — not resolved out of
// `globalThis` inside the handlers on every call, which is what it used to do.
//
// WHY. cImp runs OpenCode ADDITIVELY: it does not set
// `OPENCODE_DISABLE_PROJECT_CONFIG`, so a cloned repo's own `opencode.json`
// merges underneath cImp's pins, and its `plugin` key is NOT one of the pinned
// ones. A plugin is arbitrary in-process code, and the cheapest thing such a
// module can do is assign `globalThis.fetch`. Against a late-resolving call
// site that one line disarmed BOTH halves of `tool.execute.before` at once —
// the Phase H gate never reaches /latch/state (its own fail-open contract then
// refuses nothing) and the Phase F beacon never reaches /latch/beacon (nothing
// is ever latched) — while cImp's /status and the Settings badge went on
// reporting both ON. A control that reports ON while doing nothing is worse
// than one that is off.
//
// WHAT THIS IS AND IS NOT. It is a NARROWING, not containment, and its bound is
// LOAD ORDER: a hostile module that is EVALUATED BEFORE this one still gets its
// swap in first and is captured here instead. Nothing in an in-process JS hook
// is a security boundary — see the honest-limits block below, which applies to
// this line too. What it closes is the zero-effort variant, where a swap
// performed at ANY point after startup (in a handler, on a timer, on the first
// tool call) silently disarmed a control that was already running.
//
// `.bind(globalThis)`: a bare `const f = globalThis.fetch` throws "Illegal
// invocation" in runtimes whose `fetch` requires its receiver. The `typeof`
// guard covers the other direction — in a runtime with no `fetch` at all this
// file must be INERT, not throw at load: a module that throws while loading
// takes the harness's whole plugin load down with it, and every call site below
// already treats a rejected fetch as "unreported", never as "refused".
const CIMP_FETCH =
  typeof globalThis.fetch === "function"
    ? globalThis.fetch.bind(globalThis)
    : () => Promise.reject(new Error("cimp: this runtime has no fetch"));

const CIMP_LOOPBACK = "http://127.0.0.1:{port}";
const CIMP_TOKEN = "{token}";
// ── V35 Phase I: CHP, the cImp Harness Protocol ─────────────────────────────
//
// The version of the loopback wire this FILE speaks, substituted from
// `harness::chp::CHP_VERSION` at generation time — never typed here as a
// literal, so the constant has exactly one definition and `docs/CHP.md` is
// test-pinned to it.
//
// WHY IT IS ON THE WIRE. This file is written to disk at TAB LAUNCH and outlives
// the binary that wrote it: upgrade cImp with a tab still open and an old plugin
// is talking to new loopback code. V32 recorded that as a deploy trap four
// separate times, always with the same mitigation — "needs a FRESH TAB or it
// reads as a failure". With this field the app RECOGNISES the mismatch and says
// so in Settings → Harness health, instead of the user meeting it as a
// capability that quietly misbehaves.
//
// It is ADDITIVE and TOLERATED-ABSENT in both directions: an app that predates
// CHP ignores the field, and an app that postdates it treats a body without one
// as pre-CHP. Nothing is ever refused over a version.
const CIMP_CHP = {chp};
const CIMP_INJECT_ENABLED = {inject};
const CIMP_AUTO_CHECK_ENABLED = {auto_check};
const CIMP_EDIT_TOOLS = new Set(["edit", "write", "patch"]);
// V32 Phase F (locked decision 14): report-only visibility of OpenCode's OWN
// web tools, which never route through cImp and are therefore invisible to the
// proxy's taint latch. `false` in `off`/`deny` mode — in `deny` the pinned
// `agent.build.permission` block refuses them outright, so there is nothing to
// observe.
const CIMP_BEACON_ENABLED = {beacon};
const CIMP_WEB_TOOLS = new Set({native_web_tools});
// ── V33 Phase F: the pre-mutation CHECKPOINT ────────────────────────────────
//
// A Workbench checkpoint taken immediately before a native tool that can
// change files on disk, attributed to that exact call. The Timeline can then
// blame the call and rewind to just before it, instead of to the start of a
// turn that contains a dozen edits.
//
// The set is `toolclass::OPENCODE_NATIVE_TABLE`'s `mutates_fs` column,
// rendered — the same reviewed table the gate's sets come from, so the JS and
// Rust cannot drift about which tools write. It is NOT the local-capability
// set: `read`/`glob`/`grep` are local capability and change nothing, and
// checkpointing before each of them would be a `git add -A` per file read.
//
// The app re-checks the name against that same table, so this set only decides
// what costs a round trip. Report-only: the POST is awaited (the ordering IS
// the feature, and unlike the Claude hook this one can wait safely) but every
// failure is swallowed — a lost checkpoint costs one call its rewind point, a
// thrown error would refuse the user's own edit.
const CIMP_CHECKPOINT_ENABLED = {checkpoint};
const CIMP_MUTATING_TOOLS = new Set({native_mutating_tools});
// The cImp TAB this FILE was generated for, baked at spawn. The tab id is the
// key the whole latch registry uses, and the hook input carries no tab or cwd
// identity (the E2 spike's finding), so without it a beacon has nothing to
// engage.
const CIMP_TAB_ID = {tab_id};
// …and whether this OpenCode process is that tab's, from the env
// (`compose_ai_env` sets `CIMP_TAB_ID` on every OpenCode spawn).
//
// #48 (H-2): there is now ONE FILE PER TAB in `.opencode/plugin/`, because the
// flags baked above are resolved per tab while the directory is shared by every
// builtin tab. OpenCode loads EVERY file in that directory into EVERY session
// it starts there — so a file that is not this process's tab must be completely
// inert, or tab B's flags would run under tab A's identity and every handler
// would fire once per installed file. Every handler below returns immediately
// unless this matches.
//
// A hand-run `opencode` in the same project (no `CIMP_TAB_ID` in its env) now
// matches nothing and gets no injection, no memory tap and no beacon. That is
// deliberate: those POSTs carried a session cImp has no tab for, and the
// alternative — letting an unbound process run every installed file — is
// duplicate prompt injection and duplicate usage rows.
const CIMP_TAB_MATCH =
  (typeof process !== "undefined" && process.env && process.env.CIMP_TAB_ID) === CIMP_TAB_ID;

// ── V32 Phase H (locked decision 17): the native-tool GATE ──────────────────
//
// Beaconing tells cImp a native web tool ran. Gating additionally REFUSES the
// harness's own tools on the far side of this tab's taint latch: under an
// EXTERNAL latch every local-capability native, under a LOCAL latch the web
// ones. Default off; this flag is baked at tab spawn.
//
// WHOLE-SURFACE OR NOTHING. The E2 spike watched the model route a blocked
// `write` through `bash`, so the local set below is the complete
// local-capability surface of the registry, `apply_patch` included (it REPLACES
// edit/write on OpenAI-provider models). `task` is deliberately absent: a
// sub-agent's own tool calls fire this same hook with this same CIMP_TAB_ID, so
// its `bash`/`read` are gated at the same latch — gating the spawn itself would
// refuse an orchestration primitive whose dangerous leaves are already closed.
//
// HONEST LIMIT — this is POLICY, NOT CONTAINMENT. It runs inside the agent's
// own process: `OPENCODE_PURE=1` and spawning a second, ungated `opencode` walk
// around it, a user-typed `!shell` and the raw PTY never reach a plugin hook at
// all, and `bash` stays egress-capable by nature. OS-level containment is V33.
//
// THE STATE TABLE (#48, F-13). Local-capability natives are refused iff the
// latch is EXTERNAL (or a beacon of this tab's is still in flight). The web
// natives are refused unless this tab is either demonstrably CLEAN (`open` and
// uncontaminated) or in research mode (`external`, where local capability is
// withheld at the same instant):
//
//   latch=open   contaminated=false  → local: admit  web: admit
//   latch=open   contaminated=true   → local: admit  web: REFUSE
//   latch=local  (either)            → local: admit  web: refuse
//   latch=external                   → local: REFUSE web: admit
//
// #48 (F-23): the `latch=local` row refuses the same call whichever way the tab
// got there, but it does not say the same thing. Reached by a local-capability
// tool it blames that tool; reached by the user's decision-15 flip it says so
// instead, because blaming a tool call that never ran is a confident, wrong
// causal story about a security event. The selector is `local_by_user_flip` from
// `/latch/state` — a fact the app recorded when it applied the override.
//
// `latch=open, contaminated=true` is the row F-13 named, and it is not exotic:
// a session rotation reopens the latch and deliberately keeps the bit (H-2), and
// `bash` is pinned `allow`, so a contaminated tab could spawn a second
// `opencode` that inherits CIMP_TAB_ID, publish a fresh session id under this
// tab's identity, and get its web tools back. The bit is what survives that.
const CIMP_NATIVE_GATE_ENABLED = {native_gate};
const CIMP_NATIVE_LOCAL_TOOLS = new Set({native_local_tools});
const CIMP_REFUSAL_NATIVE_LOCAL = {refusal_local};
const CIMP_REFUSAL_NATIVE_WEB = {refusal_web};
// #48 (F-13): the THIRD refusal — the harness's own web tools from a tab that is
// CONTAMINATED but not latched EXTERNAL. It needs its own text because the two
// above name a cause ("already used a local-capability tool") that did not
// happen in this state, and a refusal stating a cause it did not check is F-23.
const CIMP_REFUSAL_NATIVE_WEB_TAINTED = {refusal_web_tainted};
// #48 (F-23) itself: the FOURTH refusal — the `local` latch a USER put there with
// the decision-15 workflow flip. Same refusal, different sentence: the one above
// this pair blames a local-capability tool call, and after a flip no such call
// happened. Which of the two is served is decided by `local_by_user_flip`, a fact
// the app RECORDED when it applied the override — not by anything this process,
// this hook or the model can say about itself. Both are fixed strings baked at
// spawn, so a tab launched before this build still serves the old sentence.
const CIMP_REFUSAL_NATIVE_WEB_USER_LOCAL = {refusal_web_user_local};
// This hook is serialized into EVERY tool call, so the common path must be
// in-memory. 2s is short enough that a latch engaged by a proxied `ddg` fetch
// (which this process never sees) is honoured almost immediately, and long
// enough that a burst of file reads costs one round trip, not twenty.
const CIMP_GATE_TTL_MS = 2000;
// The fail-open verdict, and the initial value: gate off, nothing latched,
// nothing known about contamination (#48, F-13 — `false` is the fail-open value
// for that field too), and no user flip on record (#48, F-23 — `false` there
// selects the pre-F-23 refusal, which refuses exactly what it always did).
let CIMP_GATE_STATE = {{ at: 0, gate: false, latch: "open", contaminated: false, local_by_user_flip: false }};
// Monotonic invalidation counter (#48, H-1). Validation and invalidation used
// to speak different languages: the query RE-ASSIGNED `CIMP_GATE_STATE` to a
// fresh object stamped with a `now` captured BEFORE its fetch, while the beacon
// invalidated by MUTATING `.at = 0` on whatever object was current. A query
// still in flight when a beacon fired therefore overwrote the invalidation with
// its pre-beacon verdict and re-validated it for a full TTL — `read` and
// `webfetch` dispatched concurrently, and every local tool for the next 2 s ran
// against an `open` latch the beacon had already moved to EXTERNAL. Both halves
// now move this counter instead, so a stale reply is recognizable as stale.
let CIMP_GATE_EPOCH = 0;
// Native WEB tool calls of THIS tab that have been admitted but whose beacon
// POST has not landed yet (#48, M-15).
//
// The epoch above closes one half of the race — a query already IN FLIGHT when
// the beacon fires. It cannot close the other half: a query issued DURING the
// beacon POST starts at the already-bumped epoch, and the app has not engaged
// the latch yet (the POST that engages it is the one still in flight), so the
// reply is a truthful, genuinely pre-contamination `open`. `settle` saw nothing
// wrong with it and cached it for a full CIMP_GATE_TTL_MS — the exact window
// the latch exists to close, with a `read` admitted after a `webfetch` was.
//
// This counter is what makes that window visible locally. It is read in two
// places and TIGHTENS in both: `settle` refuses to cache across it, and the
// gate treats it as an EXTERNAL latch for local-capability tools. It never
// loosens anything, which is why an unauthenticated in-process counter is
// allowed to drive it at all — the authority to REFUSE nothing (`gate: false`)
// still comes from the app, and this can only ever add a refusal on top of a
// `gate: true` the app issued.
//
// Bounded by the beacon POST's own `AbortSignal.timeout(2000)` and decremented
// in a `finally`, so it cannot leak. A beacon whose promise never settles at
// all would pin it — but that same promise is awaited by the hook, so the web
// tool call itself would already be hung; the counter outliving it is not a new
// failure mode.
let CIMP_WEB_PENDING = 0;

// Resolve this tab's gate verdict from the app, cached for CIMP_GATE_TTL_MS.
//
// NEVER THROWS and NEVER DENIES ON DOUBT: an unreachable loopback, a non-200, a
// malformed body, a rotated token, an unknown latch label — every one of them
// returns {{ gate: false }}, which refuses nothing. That is the locked V32
// posture for this control and the reason its toggle can ship without adding a
// second failure mode: "the app is down" and "the app says there is no gate"
// have to be the same behaviour, or a crash becomes a lockout.
//
// The failed verdict is cached like a successful one, so a dead app costs one
// attempt per TTL rather than one per tool call.
async function cimpGateState() {{
  const now = Date.now();
  if (now - CIMP_GATE_STATE.at < CIMP_GATE_TTL_MS) return CIMP_GATE_STATE;
  // The epoch this query started at. Anything that invalidates the cache bumps
  // it, so a reply that resolves after an invalidation is recognizably about a
  // latch that has already moved.
  const epoch = CIMP_GATE_EPOCH;
  // …and whether a beacon was ALREADY in flight when it started (#48, M-15).
  // Together the two cover every beacon that overlaps this query at all: one
  // that starts while the query is in flight moves the epoch, and one that
  // started earlier but overlaps was, by definition of overlapping, still
  // pending right here. So `settle` below can decide "did any contamination
  // event touch my window" from two integers read at the same instant.
  const pendingAtStart = CIMP_WEB_PENDING;
  const open = {{ at: now, gate: false, latch: "open", contaminated: false, local_by_user_flip: false }};
  // Commit a verdict to the CACHE only if no contamination event touched this
  // query's window; answer with it either way.
  //
  // The asymmetry is deliberate (#48, M-15). App-side the latch is STICKY — it
  // only ever tightens, open → external/local, and never re-opens — so any
  // verdict that comes back is a LOWER BOUND on the real restriction. Applying
  // one late can therefore under-refuse but never over-refuse, which is exactly
  // the fail-open posture this control is required to have, and it is what this
  // caller got before #48 anyway. What must never happen is CACHING it: that is
  // what turned a single pre-contamination `open` into a full CIMP_GATE_TTL_MS
  // of admitted local tools. Leaving the cache empty costs one round trip on
  // the next tool call and re-reads a latch that has by then moved.
  //
  // Returning `v` rather than `open` also keeps the in-flight window
  // enforceable: `open` carries `gate: false`, so a query that answered with it
  // would skip the deny site's `st.gate === true` guard entirely and admit the
  // very call the window exists to refuse.
  const settle = (v) => {{
    if (CIMP_GATE_EPOCH === epoch && pendingAtStart === 0) CIMP_GATE_STATE = v;
    return v;
  }};
  try {{
    const r = await CIMP_FETCH(CIMP_LOOPBACK + "/latch/state", {{
      method: "POST",
      headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
      body: JSON.stringify({{ chp: CIMP_CHP, tab: CIMP_TAB_ID, consumer: "opencode" }}),
      signal: AbortSignal.timeout(1500),
    }});
    if (!r || !r.ok) return settle(open);
    const j = await r.json();
    // The backend resolves the whole three-level hierarchy (and ANDs in the
    // taint-latch feature) — this side holds no part of it and asserts nothing
    // beyond the two fields' types.
    if (!j || j.gate !== true || typeof j.latch !== "string") return settle(open);
    // #48 (F-13): `contaminated` is read `=== true` and is DELIBERATELY NOT part
    // of the guard above. Requiring it would turn a reply that merely lacks the
    // field — an older loopback, a schema change — into `settle(open)`, i.e. a
    // TOTAL gate bypass including the latch half. Read defensively instead: a
    // missing or non-boolean value is `false`, which loses only the one refusal
    // this field adds and keeps everything else enforced.
    // #48 (F-23): `local_by_user_flip` rides in on the same defensive read as
    // `contaminated`, and is likewise NOT in the guard above — it selects a
    // message, so requiring it would trade the whole gate for a sentence.
    return settle({{ at: now, gate: true, latch: j.latch, contaminated: j.contaminated === true, local_by_user_flip: j.local_by_user_flip === true }});
  }} catch (_e) {{
    return settle(open);
  }}
}}

// ── V35 Phase I: the CHP hello (design D3) ──────────────────────────────────
//
// Fired ONCE, here, while this module is being evaluated — which for a generated
// plugin is per TAB LAUNCH, exactly the spawn-baked moment worth stamping. It
// tells the app which protocol version this file speaks, and which CHP events it
// will actually push with this tab's flags applied. A capability absent from
// `serves` is UNAVAILABLE, not broken.
//
// NOTHING GATES ON IT. The app records and displays the declaration; no cImp
// feature consults it (that is Phase L). In particular `tool.gate` appearing
// here is not a trust claim — the gate's authority is the app computing the
// verdict at /latch/state, and this file's only power is to refuse MORE than it
// was told to.
//
// IT MUST NOT THROW, and the shape below is written for that and nothing else.
// A module that throws while loading takes the harness's whole plugin load down
// with it — the same hazard the CIMP_FETCH binding above is guarded against —
// so the dispatch sits inside a try/catch, the result is only `.catch`ed after
// being checked for a `.catch`, and the reply is never read. A dead app, a
// rotated token or a runtime with no fetch all end as "unannounced".
try {{
  if (CIMP_TAB_MATCH) {{
    const hello = CIMP_FETCH(CIMP_LOOPBACK + "/session/hello", {{
      method: "POST",
      headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
      body: JSON.stringify({{
        chp: CIMP_CHP,
        agent: "opencode",
        tab: CIMP_TAB_ID,
        // `harness_version` is deliberately ABSENT: OpenCode exposes no version
        // to a plugin at module scope, and baking in the number cImp last saw
        // would be cImp attesting to itself rather than the harness declaring
        // anything. `docs/CHP.md` § 6.2.
        serves: {serves},
        cannot: {cannot},
      }}),
      signal: AbortSignal.timeout(2000),
    }});
    if (hello && typeof hello.catch === "function") hello.catch(() => {{}});
  }}
}} catch (_e) {{}}

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
    if (!CIMP_TAB_MATCH) return;
    const p = out.parts.find((x) => x.type === "text");
    if (!p || !p.text) return;
    try {{
      const r = await CIMP_FETCH(CIMP_LOOPBACK + "/context/retrieve", {{
        method: "POST",
        headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
        // V33: `tab` rides along so the prompt-tap checkpoint this POST fires
        // can be attributed to THIS tab — `agent` is the harness name, shared
        // by every OpenCode tab, and `CIMP_TAB_ID` is already baked into this
        // file for the beacon/gate. Same identity, one source.
        body: JSON.stringify({{ chp: CIMP_CHP, cwd: input.directory, prompt: p.text, session_id: inp.sessionID, agent: "opencode", tab: CIMP_TAB_ID }}),
        signal: AbortSignal.timeout(600),
      }});
      const j = await r.json();
      if (CIMP_INJECT_ENABLED && j && j.ok && j.text) p.text += "\n\n" + j.text;
    }} catch (_e) {{}}
  }},
  // Two halves, in this order: the V32 Phase H GATE (may deny) and then the
  // V32 Phase F native-web BEACON (report-only, never throws).
  //
  // The beacon fires BEFORE the tool runs so the latch is engaged by the time
  // the fetched bytes exist, and it NEVER throws or rejects —
  // `tool.execute.before` denies by throwing (the E2 spike verdict), so any
  // escaping error in that half would turn a report-only sensor into a silent
  // deny of the user's own web tool. Everything in it is inside one try/catch,
  // the fetch is awaited only to keep the ordering honest, and its own
  // rejection is caught by the same block. A dead app, a rotated token or a 2s
  // stall all end as "unreported", never as "refused".
  "tool.execute.before": async (inp) => {{
    // ── V32 Phase H: the GATE half, and it runs FIRST. ──────────────────────
    // Deliberately OUTSIDE the try/catch below, because throwing is how this
    // hook denies (the E2 spike verdict) — the only escaping error in this file,
    // and only ever on a definite deny verdict. Args are NEVER rewritten: arg
    // mutation is the buggy upstream path (#31680/#39674/#37963).
    //
    // Ordering matters: gate before beacon, so a refused web call does not first
    // engage the latch and contaminate the conversation. That is the same
    // property the proxy's `gate` has — a refused call never moves the latch.
    if (CIMP_NATIVE_GATE_ENABLED && CIMP_TAB_MATCH && inp) {{
      const local = CIMP_NATIVE_LOCAL_TOOLS.has(inp.tool);
      const web = CIMP_WEB_TOOLS.has(inp.tool);
      // An unlisted name (task, skill, todowrite, question) is UNGATED and costs
      // no round trip — the check is a Set lookup before anything is awaited.
      if (local || web) {{
        const st = await cimpGateState();
        if (st.gate === true) {{
          // #48 (M-15): a native web tool of this tab that has ALREADY been
          // admitted, and whose beacon POST is still in flight, contaminates
          // this conversation exactly as much as one the app has heard about —
          // the app's reply merely predates it. Read here, at the deny site, so
          // NO cache path can bypass it: a cached verdict, a fresh verdict and a
          // fail-open verdict all pass through this one expression.
          //
          // Tighten-only, and deliberately only in the LOCAL direction. It ADDS
          // "external" to the local test and touches nothing else, so it can
          // never turn a `local` latch's web refusal (the line below) into an
          // admission — the one way a local signal could have loosened the gate.
          const external = st.latch === "external" || CIMP_WEB_PENDING > 0;
          // #48 (F-13): `contaminated` has been on this wire since Phase H and
          // nothing read it. Read it HERE, at the same deny site every cache
          // path funnels through, and ONLY in the WEB direction.
          //
          // Once external content has entered this conversation, the harness's
          // own web tools are its unproxied way OUT — no beacon in `off`/`deny`
          // mode, no latch, no budget, no SSRF screen. The other direction is
          // deliberately untouched: contamination's cost to LOCAL capability is
          // persistence, which the app-side write quarantine already enforces,
          // and refusing local tools here would make "switch to local" restore
          // the proxied half of a surface and not the native half.
          //
          // Why `=== "open"` and not `!== "external"`: `cimpGateState`'s
          // contract is that an UNKNOWN latch label denies nothing, and that
          // promise is delivered by the negative form of these tests. Why
          // `=== true`: a missing field must read `false`, i.e. fail open.
          //
          // A clear (`clear_contamination` app-side) reaches this within one
          // CIMP_GATE_TTL_MS and needs no invalidation: the epoch exists so a
          // TIGHTENING cannot be overwritten by a stale verdict, and a loosening
          // that lands late costs at most one TTL of over-refusal — exactly the
          // latency `FlipLocal` already has for the local direction. The app
          // process has no inbound channel into this plugin; the TTL is the
          // mechanism, not a shortcut around one.
          const tainted = st.contaminated === true && st.latch === "open";
          // #48 (F-23): WHICH refusal, never WHETHER. `local_by_user_flip` is a
          // fact the app recorded when it applied the user's workflow flip, read
          // here for the one purpose of not blaming a tool call that never
          // happened. Read `=== true` like `contaminated` and for the same
          // reason: a reply that lacks the field (an older loopback) must lose
          // only the better sentence, never a refusal — so the fallback is the
          // pre-F-23 constant, which is what this line always served.
          const userLocal = st.local_by_user_flip === true;
          if (local && external) throw new Error(CIMP_REFUSAL_NATIVE_LOCAL);
          if (web && st.latch === "local") throw new Error(userLocal ? CIMP_REFUSAL_NATIVE_WEB_USER_LOCAL : CIMP_REFUSAL_NATIVE_WEB);
          if (web && tainted) throw new Error(CIMP_REFUSAL_NATIVE_WEB_TAINTED);
        }}
      }}
    }}
    // ── V33 Phase F: the CHECKPOINT half — report-only, never throws. ───────
    //
    // AFTER the gate, deliberately: a refused call never happened, and
    // checkpointing before it would leave a Timeline row blaming a tool that
    // did not run — the same ordering rule the beacon half follows below, and
    // the same one the proxy's own gate has (a refused call never moves the
    // latch).
    //
    // Its OWN try/catch, not the beacon's: these are two independent reports,
    // and folding them together would mean a slow app that times out the
    // checkpoint POST also skipped the web beacon (or vice versa). Everything
    // inside is swallowed — `tool.execute.before` denies by THROWING, so an
    // escaping error here would turn a checkpoint into a silent refusal of the
    // user's own edit.
    //
    // The POST is AWAITED. That is the point of the feature — a checkpoint
    // taken after the write is a checkpoint of the damage — and it is safe
    // here in a way it is not in the Claude shim, whose fail-open contract
    // forbids waiting on the app: this hook's timeout semantics are ours, so
    // the 2s abort signal below IS the bound, and it degrades to "unreported",
    // never to "refused". Cost is bounded app-side too: the checkpoint
    // throttle is per `(project root, tab)`, so a burst of edits inside one
    // `checkpoint_min_gap_s` window costs one snapshot, not one per call.
    try {{
      if (CIMP_CHECKPOINT_ENABLED && CIMP_TAB_MATCH && inp && CIMP_MUTATING_TOOLS.has(inp.tool)) {{
        await CIMP_FETCH(CIMP_LOOPBACK + "/workbench/tool_checkpoint", {{
          method: "POST",
          headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
          body: JSON.stringify({{
            chp: CIMP_CHP,
            tab: CIMP_TAB_ID,
            agent: "opencode",
            tool: inp.tool,
            cwd: input.directory,
            session_id: inp.sessionID,
          }}),
          signal: AbortSignal.timeout(2000),
        }});
      }}
    }} catch (_e) {{}}
    // ── V32 Phase F: the BEACON half — report-only, and it still never throws.
    try {{
      if (!CIMP_TAB_MATCH || !inp || !CIMP_WEB_TOOLS.has(inp.tool)) return;
      // A native web tool is about to run, so whatever the gate cached a moment
      // ago describes a latch that is about to move. Invalidate here, ABOVE the
      // beacon's own enable guard and BEFORE the POST:
      //
      //   * above the guard, because the gate can be ON while the beacon is off
      //     — `off`/`deny` native-web mode with the gate switched on, where
      //     nothing invalidated the cache at all (#48, H-1's second half);
      //   * before the POST, so a fetch that throws still leaves the stale
      //     verdict dropped rather than live for the rest of its TTL.
      //
      // Bumping the epoch is what makes this stick: a query already in flight
      // can no longer commit its pre-beacon verdict on top of this.
      //
      // #48 (F-14) — WHAT THIS DOES AND DOES NOT COVER. It used to be justified
      // as covering "the most hardened combination available", which claimed
      // more than it delivers, and the claim was the defect. Two honest bounds:
      //   * with the beacon OFF the invalidation still fires, but only for this
      //     tab's OWN native web tools, and in that mode nothing reports the
      //     call — so the re-query answers with whatever the app already knew,
      //     usually `open`. The value is that the placement is ABOVE the enable
      //     guard, i.e. structural, not that it learns anything new;
      //   * a PROXIED `ddg` fetch does move the latch app-side and triggers NO
      //     invalidation here at all — this process never sees it. That path is
      //     covered by CIMP_GATE_TTL_MS alone (≤ 2 s), by design: the app has no
      //     inbound channel into this plugin.
      CIMP_GATE_EPOCH++;
      CIMP_GATE_STATE = {{ at: 0, gate: false, latch: "open", contaminated: false, local_by_user_flip: false }};
      // #48 (M-15): open the in-flight window HERE, on the statement after the
      // epoch bump, and close it in the `finally` below. The adjacency is the
      // whole guarantee — there is no `await` between the two, so this engine
      // cannot run another hook in between, and therefore no gate query can
      // start in the sliver where the epoch has moved but the window is not yet
      // open. A test pins that no `await` appears between them. It is raised
      // above the enable guard for the same reason the epoch bump is, and the
      // `try` starts before the guard so the `return` for a disabled beacon
      // closes the window on its way out.
      CIMP_WEB_PENDING++;
      try {{
        if (!CIMP_BEACON_ENABLED) return;
        await CIMP_FETCH(CIMP_LOOPBACK + "/latch/beacon", {{
          method: "POST",
          headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
          body: JSON.stringify({{
            chp: CIMP_CHP,
            tab: CIMP_TAB_ID,
            consumer: "opencode",
            tool: inp.tool,
            cwd: input.directory,
            session_id: inp.sessionID,
          }}),
          signal: AbortSignal.timeout(2000),
        }});
      }} finally {{
        CIMP_WEB_PENDING--;
      }}
    }} catch (_e) {{}}
  }},
  "tool.execute.after": async (inp) => {{
    if (!CIMP_TAB_MATCH) return;
    try {{
      const body = {{
        chp: CIMP_CHP,
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
      await CIMP_FETCH(CIMP_LOOPBACK + "/memory/event", {{
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
      CIMP_FETCH(CIMP_LOOPBACK + "/context/post_edit", {{
        method: "POST",
        headers: {{ authorization: "Bearer " + CIMP_TOKEN, "content-type": "application/json" }},
        body: JSON.stringify({{
          chp: CIMP_CHP,
          cwd: input.directory,
          session_id: inp.sessionID,
          file_path: filePath,
          tool_name: inp.tool,
          // #48 (M-7): the identity `/context/post_edit`'s taint gate resolves
          // a latch scope from. This route executes the project's configured
          // checks; without a tab it resolves no scope and is ungated.
          agent: "opencode",
          tab: CIMP_TAB_ID,
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
    if (!CIMP_TAB_MATCH) return;
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
        chp: CIMP_CHP,
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
      await CIMP_FETCH(CIMP_LOOPBACK + "/memory/event", {{
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
        // ONE definition of the protocol version (`harness::chp::CHP_VERSION`),
        // substituted here rather than written as a literal in the template —
        // the same rule the refusal strings and the tool tables follow. A `u32`,
        // so there is nothing to escape.
        chp = crate::harness::chp::CHP_VERSION,
        serves = serves,
        cannot = cannot,
        // JSON-quoted, never hand-quoted: tab ids are `[a-z0-9-]` today
        // (reserved ids and `ai-<uuid>` duplicates), but a literal built by
        // string concatenation is one rename away from being a syntax error in
        // a file the harness loads at startup.
        tab_id = serde_json::to_string(tab).unwrap_or_else(|_| "\"\"".to_string()),
        inject = if flags.inject { "true" } else { "false" },
        auto_check = if flags.auto_check { "true" } else { "false" },
        beacon = if flags.beacon { "true" } else { "false" },
        native_gate = if flags.native_gate { "true" } else { "false" },
        checkpoint = if flags.checkpoint { "true" } else { "false" },
        native_local_tools = native_local_tools,
        native_web_tools = native_web_tools,
        native_mutating_tools = native_mutating_tools,
        refusal_local = refusal_local,
        refusal_web = refusal_web,
        refusal_web_tainted = refusal_web_tainted,
        refusal_web_user_local = refusal_web_user_local,
    )
}

