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
//! **V35 Phase M**: the emitted file now lives at `templates/plugin.js` — a real
//! `.js` file with `{{cimp.*}}` slots, `include_str!`ed and rendered by
//! [`crate::harness::render`]. What is left here is the half that is genuinely a
//! program: [`OPENCODE_PLUGIN_KEYS`] (which slots exist) and
//! [`opencode_plugin_values`] (what fills them, from reviewed Rust, through
//! serde). The move is proven to be a pure refactor of generation by the
//! byte-identical goldens under `fixtures/plugin-goldens/opencode/`, captured
//! from the pre-Phase-M `format!()` generator; see `templates/README.md`.

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

/// The OpenCode plugin **template**: a real `.js` file beside this module,
/// pulled in at compile time.
///
/// V35 Phase M (design § 5.1). It used to be a `format!()` string in Rust with
/// every JS brace doubled (`{{`/`}}`) — hostile to the one thing this milestone
/// is for, *reading a diff when upstream changes*. As a `.js` file it
/// highlights, greps and diffs like the JavaScript it is, and the only Rust
/// left is the part that is genuinely a program: which slots exist
/// ([`OPENCODE_PLUGIN_KEYS`]) and what fills them
/// ([`opencode_plugin_values`]).
///
/// `include_str!`, not a path read at runtime: a renamed or deleted template is
/// a **compile** error, and a shipped binary can never be one file short of a
/// working security control.
const OPENCODE_PLUGIN_TEMPLATE: &str = include_str!("templates/plugin.js");

/// The FIXED substitution key set of [`OPENCODE_PLUGIN_TEMPLATE`], in emission
/// order.
///
/// Three tests hold this and the template together, and they are the milestone
/// exit criterion ("every `{{key}}` is in the known set or the build fails"):
/// every `{{key}}` in the template is listed here, every key listed here is
/// used by the template, and [`opencode_plugin_values`] supplies exactly this
/// set. A typo in either direction fails `cargo test` instead of emitting a
/// plugin whose gate constant is missing.
///
/// **Every value is a whole JS literal**, quotes included, produced by
/// [`crate::harness::render::json_lit`] — see there for why nothing here is
/// ever hand-quoted.
pub(crate) const OPENCODE_PLUGIN_KEYS: [&str; 18] = [
    // Where to reach this cImp instance, and with what bearer token. Baked per
    // spawn because the token rotates per app run.
    "cimp.loopback_url",
    "cimp.token",
    // V35 Phase I: the protocol version this FILE speaks, from
    // `harness::chp::CHP_VERSION` — one definition, substituted, never typed as
    // a literal in the template.
    "cimp.chp_version",
    // #48 (H-2): which cImp tab this file belongs to. One file per tab.
    "cimp.tab_id",
    // The five per-tab handler switches (`OpencodePluginFlags`), each baked as a
    // `const` so the file on disk says what it does.
    "cimp.flag.inject",
    "cimp.flag.auto_check",
    "cimp.flag.beacon",
    "cimp.flag.native_gate",
    "cimp.flag.checkpoint",
    // The three name sets, all three rendered from `OPENCODE_NATIVE_TABLE`.
    "cimp.tools.local",
    "cimp.tools.web",
    "cimp.tools.mutating",
    // The four fixed refusal sentences (`offload::toolclass`).
    "cimp.refusal.local",
    "cimp.refusal.web",
    "cimp.refusal.web_tainted",
    "cimp.refusal.web_user_local",
    // V35 Phase I: the two halves of the `/session/hello` declaration, with this
    // tab's flags already applied.
    "cimp.hello.serves",
    "cimp.hello.cannot",
];

/// The dependency-free OpenCode plugin source, with the loopback port + token
/// and the per-tab flags baked in.
///
/// V35 Phase M: this is now [`OPENCODE_PLUGIN_TEMPLATE`] rendered with
/// [`opencode_plugin_values`]. It is a **pure refactor of generation** — the
/// goldens under `fixtures/plugin-goldens/opencode/` were captured from the
/// pre-Phase-M `format!()` generator and are asserted byte for byte here
/// (`the_template_renders_the_pre_phase_m_goldens_byte_for_byte`), so nothing
/// about the emitted file changed with the move.
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
    crate::harness::render::render(
        "opencode/templates/plugin.js",
        OPENCODE_PLUGIN_TEMPLATE,
        &OPENCODE_PLUGIN_KEYS,
        &opencode_plugin_values(port, token, tab, flags),
    )
}

/// What fills each slot of [`OPENCODE_PLUGIN_KEYS`], in the same order.
///
/// This is the half of plugin generation that is reviewed Rust rather than
/// JavaScript, and design § 5.1 is explicit that it stays that way: the values
/// come from `OPENCODE_NATIVE_TABLE` rendered through serde and the refusal
/// constants JSON-quoted, *"which is what today's generator is careful about
/// and must not regress: a tool name added later must never be able to malform
/// the emitted JS"*.
fn opencode_plugin_values(
    port: u16,
    token: &str,
    tab: &str,
    flags: OpencodePluginFlags,
) -> Vec<(&'static str, String)> {
    use crate::harness::render::json_lit;

    // V32 Phase H: the two name sets come from the ONE reviewed table
    // (`toolclass::OPENCODE_NATIVE_TABLE`), rendered through serde so the JS
    // literal cannot be malformed by a name someone adds later. The web set is
    // rendered from the same table for the same reason, and a test pins it
    // against the beacon's `CIMP_WEB_TOOLS`.
    let json_list = |names: Vec<&'static str>| json_lit(&names, "[]");
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
    const REFUSAL_FALLBACK: &str = "\"REFUSED (security boundary)\"";
    let refusal_local = json_lit(
        crate::offload::toolclass::REFUSAL_NATIVE_LOCAL_BLOCKED,
        REFUSAL_FALLBACK,
    );
    let refusal_web = json_lit(
        crate::offload::toolclass::REFUSAL_NATIVE_WEB_BLOCKED,
        REFUSAL_FALLBACK,
    );
    let refusal_web_tainted = json_lit(
        crate::offload::toolclass::REFUSAL_NATIVE_WEB_TAINTED,
        REFUSAL_FALLBACK,
    );
    let refusal_web_user_local = json_lit(
        crate::offload::toolclass::REFUSAL_NATIVE_WEB_USER_LOCAL,
        REFUSAL_FALLBACK,
    );
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
        // ── V35 Phase L: the read path, and why OpenCode does NOT push it ────
        //
        // Milestone locked decision 5 for this phase asked the honest question
        // first — what does the plugin API actually serve? — and the answer,
        // read off `@opencode-ai/plugin`'s own `Hooks` interface, is that BOTH
        // capabilities are reachable. They are declared `cannot` anyway, per
        // design D6 ("a fallback contained and declared beats a lossy
        // migration"), and the reasons are recorded here because they are the
        // conditions under which this decision should be revisited:
        //
        //  * **assistant text.** Two routes exist and neither is a clean win.
        //    `experimental.text.complete` hands over one COMPLETED TEXT PART
        //    (`{sessionID, messageID, partID}` + `{text}`) — but the SSE reader
        //    speaks one MESSAGE, joining its text parts, and pushing per part
        //    would hand the segmenter a different unit at part boundaries. That
        //    is precisely the cadence flattening locked decision 2 forbids
        //    (live-verify recipe 10). The other route, widening this plugin's
        //    existing `event` handler past `message.updated`, reads
        //    `properties.part.text` / `properties.delta` — the SAME Tier-C JSON
        //    shapes `harness/opencode/read.rs` already reads, over a different
        //    transport. It would move no tier and buy no resilience, at the cost
        //    of re-implementing the reader's part accumulator in generated JS.
        //    REVISIT when `experimental.text.complete` graduates out of
        //    `experimental.` **and** a message-completion signal is available
        //    beside it.
        //  * **tool results.** `tool.execute.after`'s SECOND parameter carries
        //    `{title, output, metadata}`; this plugin's handler takes only the
        //    first, so the result text is one parameter away. But cImp has no
        //    OpenCode tool-result consumer to feed: OpenCode usage is
        //    `est_only` from tool-call INPUT args by design, and wiring the
        //    output would be a new capability rather than a migration of an
        //    existing one — out of scope for a milestone about the durability
        //    of what exists. REVISIT with OpenCode usage accounting, not here.
        //
        // Both therefore stay on `harness/opencode/read.rs`, which is now that
        // harness's DECLARED fallback rather than its ambient one — the whole
        // difference D6 is about.
        declare(
            false,
            chp::EV_ASSISTANT_TEXT,
            "OpenCode's plugin API delivers assistant text per completed PART \
             (`experimental.text.complete`) or as the same SSE payload shapes the reader already \
             consumes (`event`), so pushing it would either change the segmenter's unit or move no \
             tier — the `/event` SSE reader stays this harness's declared fallback \
             (`opencode.sse.events`)",
        );
        declare(
            false,
            chp::EV_SESSION_TOOL_RESULT,
            "reachable (`tool.execute.after`'s output parameter carries the result text) but \
             unconsumed: OpenCode usage is estimate-only from tool-call input args, so wiring it \
             would add a capability rather than migrate one",
        );
        (json_lit(&serves, "[]"), json_lit(&cannot, "[]"))
    };

    let flag = |on: bool| if on { "true" } else { "false" }.to_string();
    vec![
        // The whole `"http://127.0.0.1:<port>"` literal, quoted by serde rather
        // than by the template. Phase M's one behavioural tightening over the
        // old `format!()`, which hand-quoted this slot and the token below.
        (
            "cimp.loopback_url",
            json_lit(&format!("http://127.0.0.1:{port}"), "\"\""),
        ),
        ("cimp.token", json_lit(token, "\"\"")),
        // ONE definition of the protocol version (`harness::chp::CHP_VERSION`),
        // substituted here rather than written as a literal in the template —
        // the same rule the refusal strings and the tool tables follow. A `u32`,
        // so there is nothing to escape.
        (
            "cimp.chp_version",
            crate::harness::chp::CHP_VERSION.to_string(),
        ),
        // JSON-quoted, never hand-quoted: tab ids are `[a-z0-9-]` today
        // (reserved ids and `ai-<uuid>` duplicates), but a literal built by
        // string concatenation is one rename away from being a syntax error in
        // a file the harness loads at startup.
        ("cimp.tab_id", json_lit(tab, "\"\"")),
        ("cimp.flag.inject", flag(flags.inject)),
        ("cimp.flag.auto_check", flag(flags.auto_check)),
        ("cimp.flag.beacon", flag(flags.beacon)),
        ("cimp.flag.native_gate", flag(flags.native_gate)),
        ("cimp.flag.checkpoint", flag(flags.checkpoint)),
        ("cimp.tools.local", native_local_tools),
        ("cimp.tools.web", native_web_tools),
        ("cimp.tools.mutating", native_mutating_tools),
        ("cimp.refusal.local", refusal_local),
        ("cimp.refusal.web", refusal_web),
        ("cimp.refusal.web_tainted", refusal_web_tainted),
        ("cimp.refusal.web_user_local", refusal_web_user_local),
        ("cimp.hello.serves", serves),
        ("cimp.hello.cannot", cannot),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::render::template_keys;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    /// The flag combinations the goldens were captured for, with the dummy
    /// endpoint/token/tab each one uses.
    ///
    /// Three, not two: `ALL_ON` and `ALL_OFF` bracket the flags, and the mid
    /// config is the one that catches a renderer which happens to work only when
    /// every slot agrees — it splits the flag groups the plugin reads together
    /// (inject/auto_check, beacon/native_gate, checkpoint) and uses a duplicated
    /// `ai-<uuid>` tab id and a token with a hyphen in it.
    fn golden_cases() -> Vec<(&'static str, u16, &'static str, &'static str, OpencodePluginFlags)> {
        vec![
            (
                "plugin.all-on.js",
                54321,
                "deadbeef00",
                "opencode",
                OpencodePluginFlags {
                    inject: true,
                    auto_check: true,
                    beacon: true,
                    native_gate: true,
                    checkpoint: true,
                },
            ),
            (
                "plugin.all-off.js",
                54321,
                "deadbeef00",
                "opencode",
                OpencodePluginFlags::default(),
            ),
            (
                "plugin.mid.js",
                41999,
                "test-loopback-token",
                "ai-abc123",
                OpencodePluginFlags {
                    inject: true,
                    auto_check: false,
                    beacon: true,
                    native_gate: false,
                    checkpoint: true,
                },
            ),
        ]
    }

    fn goldens_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("plugin-goldens")
            .join("opencode")
    }

    /// **The Phase M guarantee.** These three files were emitted by the
    /// pre-Phase-M `format!()` generator, before the template existed, and are
    /// asserted here byte for byte — which is what makes moving the artifact out
    /// of Rust a *pure refactor of generation* rather than an unreviewed rewrite
    /// of a file inside the TCB.
    ///
    /// From here on they are the standing **readable diff**: change the template
    /// (an upstream rename, a new handler) and the goldens change with it, in
    /// JavaScript, in the same commit — which is the review artifact the
    /// milestone exit criterion is about. Regenerate deliberately with
    /// `CIMP_BLESS_PLUGIN_GOLDENS=1 cargo test --bin cimp byte_for_byte` and read
    /// the diff; never to make a red test green.
    #[test]
    fn the_template_renders_the_pre_phase_m_goldens_byte_for_byte() {
        let bless = std::env::var("CIMP_BLESS_PLUGIN_GOLDENS").is_ok();
        let dir = goldens_dir();
        for (name, port, token, tab, flags) in golden_cases() {
            let rendered = opencode_plugin_source(port, token, tab, flags);
            let path = dir.join(name);
            if bless {
                std::fs::create_dir_all(&dir).expect("goldens dir");
                std::fs::write(&path, &rendered).expect("write golden");
                continue;
            }
            let golden = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!(
                    "{}: golden missing ({e}) — the emitted OpenCode plugin has no reference to \
                     diff against",
                    path.display()
                )
            });
            assert_eq!(
                rendered,
                golden,
                "{}: the rendered plugin no longer matches its golden. If the change is INTENDED, \
                 re-bless with CIMP_BLESS_PLUGIN_GOLDENS=1 and review the .js diff — this file is \
                 inside the TCB (design § 5, D7).",
                path.display()
            );
        }
    }

    /// The milestone exit criterion, first half: a `{{typo}}` in the template
    /// fails here rather than emitting a plugin whose gate constant is missing.
    #[test]
    fn every_placeholder_in_the_template_is_a_known_key() {
        let known: BTreeSet<&str> = OPENCODE_PLUGIN_KEYS.iter().copied().collect();
        let found = template_keys(OPENCODE_PLUGIN_TEMPLATE);
        let unknown: Vec<_> = found.difference(&known).copied().collect();
        assert!(
            unknown.is_empty(),
            "templates/plugin.js uses placeholders that nothing supplies: {unknown:?} — add them \
             to OPENCODE_PLUGIN_KEYS and opencode_plugin_values, or fix the typo"
        );
    }

    /// …and the second half: a key nobody uses is a value computed and thrown
    /// away, which is how a slot silently stops being baked.
    #[test]
    fn every_known_key_is_used_by_the_template() {
        let known: BTreeSet<&str> = OPENCODE_PLUGIN_KEYS.iter().copied().collect();
        let found = template_keys(OPENCODE_PLUGIN_TEMPLATE);
        let unused: Vec<_> = known.difference(&found).copied().collect();
        assert!(
            unused.is_empty(),
            "OPENCODE_PLUGIN_KEYS lists keys the template never uses: {unused:?} — the value is \
             computed and discarded, so whatever it was meant to bake is not baked"
        );
    }

    /// The third leg: the generator supplies exactly the documented set, in that
    /// order. Without this the two tests above could both pass while
    /// `opencode_plugin_values` omitted a key — which `render` would leave as a
    /// literal `{{…}}` in a live plugin.
    #[test]
    fn the_generator_supplies_exactly_the_documented_key_set_in_order() {
        let supplied: Vec<&str> =
            opencode_plugin_values(1, "t", "opencode", OpencodePluginFlags::default())
                .iter()
                .map(|(k, _)| *k)
                .collect();
        assert_eq!(supplied, OPENCODE_PLUGIN_KEYS.to_vec());
    }

    /// No residual `{{`/`}}` anywhere in a rendered plugin, for any of the
    /// golden configurations. The emitted JavaScript has no double-brace
    /// construct of its own, so any survivor is an unsubstituted placeholder —
    /// and `render` deliberately leaves those visible rather than blanking them.
    #[test]
    fn a_rendered_plugin_carries_no_unsubstituted_placeholder() {
        for (name, port, token, tab, flags) in golden_cases() {
            let js = opencode_plugin_source(port, token, tab, flags);
            assert!(
                !js.contains("{{") && !js.contains("}}"),
                "{name}: rendered plugin still contains a double brace — an unsubstituted \
                 placeholder reached a file the harness loads at startup"
            );
        }
    }

    /// The escaping discipline design § 5.1 names as the thing that must not
    /// regress, asserted on the two slots whose quoting Phase M *changed*: the
    /// loopback URL and the token used to be hand-quoted inside the `format!()`
    /// string (`"http://127.0.0.1:{port}"`, `"{token}"`) and are now whole
    /// serde-produced literals like every other value.
    #[test]
    fn hostile_substitution_values_cannot_malform_the_emitted_js() {
        let js = opencode_plugin_source(
            1,
            "a\"; process.exit(1); //",
            "opencode",
            OpencodePluginFlags::default(),
        );
        assert!(
            js.contains(r#"const CIMP_TOKEN = "a\"; process.exit(1); //";"#),
            "a token containing a quote must be escaped into the literal, not close it"
        );
        // …and the tab id, which was already serde-quoted before Phase M.
        let js = opencode_plugin_source(1, "t", "a\"b", OpencodePluginFlags::default());
        assert!(js.contains(r#"const CIMP_TAB_ID = "a\"b";"#));
    }

    /// The template is a `.js` file on disk, not a string in this module — a
    /// grep-able assertion that Phase M is not quietly undone by inlining it
    /// back, and that the slots are still in expression position (where a serde
    /// literal, quotes included, is what fills them).
    #[test]
    fn the_template_is_a_real_file_with_its_slots_in_expression_position() {
        assert!(OPENCODE_PLUGIN_TEMPLATE.starts_with("// Generated by cImp"));
        assert!(
            OPENCODE_PLUGIN_TEMPLATE.contains("const CIMP_LOOPBACK = {{cimp.loopback_url}};"),
            "the template no longer carries its slots in expression position"
        );
    }
}
