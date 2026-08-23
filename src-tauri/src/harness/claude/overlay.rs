//! **Claude Code's generated launch surface** — the `--settings` overlay, the
//! `--mcp-config` overlay and the CHP hello that declares what they wired
//! (V35 Phase K: moved verbatim from `tabs/config.rs`, design § 4's moves
//! table).
//!
//! This is Claude's L1 emitter, the twin of
//! [`crate::harness::opencode::plugin`]. Everything in it is written in one
//! harness's vocabulary — `hooks.UserPromptSubmit`, `statusLine`,
//! `permissions.deny`, `mcpServers` — which is the reason it no longer lives
//! inside a 7000-line file about tab launching. `tabs::config` still owns
//! *when* a tab spawns and *what settings mean*; this module owns *how Claude
//! Code is told*.
//!
//! Every artifact here is **spawn-baked**: it is argv, computed once from one
//! settings snapshot and handed to a process that outlives further changes. See
//! `tabs::config::spawn_inject_sig` for the restart hint that makes that
//! visible, and `harness::chp` for what detects a stale one.
//!
//! Phase M turns the emitted JSON into template files; Phase K moved the Rust
//! only, `format!()`/`json!()` strings intact.

use crate::harness::claude::hook as claude_hook;
use crate::settings::injection::NativeWebMode as NativeWebVisibility;
use crate::settings::{AiToolTabConfig, Settings};
use crate::tabs::config::{
    compose_capability_guidance, native_web_for, CHANNEL_PUSH_FLAG, CHANNEL_REGISTRATION_FLAG,
    CHANNEL_REGISTRATION_TARGET,
};

/// The Claude `PreToolUse` matcher and the OpenCode tool names for the two
/// harness-native web tools, in one reviewed place.
///
/// The matcher is deliberately NARROW. It cost a process spawn per matched call
/// until the 2026-08-17 http migration and now costs a loopback POST, but the
/// reasoning is unchanged and is not really about the cost: a wide (or empty)
/// matcher would put cImp on the path of every `Read`/`Grep`/`Bash` in the
/// session for a signal only the web tools can produce. What the migration did
/// change is the *shape* of the tax — no `CreateProcess`, no discovery-file read
/// per call — which is also why the E1 latency spike is irrelevant here (see the
/// milestone's Phase E note).
pub(crate) const CLAUDE_WEB_TOOL_MATCHER: &str = "WebFetch|WebSearch";

/// **V33 Phase F**: the Claude `PreToolUse` matcher for the harness-native
/// tools that can change files on disk — the pre-mutation checkpoint's fire
/// set.
///
/// Narrow for the same reason [`CLAUDE_WEB_TOOL_MATCHER`] is, and it is **not**
/// the authority: the checkpoint core re-resolves every name against
/// `toolclass::TABLE`'s `mutates_fs` column, so this string only decides which
/// calls cImp is asked about. [`tests::every_matched_claude_tool_is_classified_as_mutating`]
/// pins the one direction that matters — every name here has a
/// `mutates_fs: true` row, so no call is *held* for a checkpoint the core will
/// decline. That mattered when a mismatch cost a wasted process spawn; since the
/// 2026-08-17 migration it matters more, because this entry's handler blocks the
/// tool call while it runs.
///
/// `MultiEdit` is in the set and got its `TABLE` row in the same change; `Bash`
/// is in it because a shell command is the widest mutation surface Claude has,
/// and leaving it out would be the V32 E2 spike's lesson unlearned (the model
/// routes a blocked write through the shell). `NotebookEdit` is deliberately
/// absent: it has no row, cImp's own `PostToolUse` auto-check matcher has never
/// named it either, and adding it here alone would spawn a process per call for
/// a POST the route declines.
pub(crate) const CLAUDE_MUTATING_TOOL_MATCHER: &str = "Edit|Write|MultiEdit|Bash";

/// The Claude `permissions.deny` rules for `deny` mode. Bare tool names, not
/// glob forms: Claude Code 2.1.214's narrowing of single-segment permission
/// globs applies to *path* patterns (`Edit(src/**)`), and a bare name is the
/// documented "every use of this tool" spelling — nothing to narrow.
const CLAUDE_WEB_DENY_RULES: [&str; 2] = ["WebFetch", "WebSearch"];

/// V35 Phase J: which Claude hooks one tab's `--settings` overlay actually
/// wired, as the booleans the overlay builder decided from.
///
/// Passed to [`claude_hello`] so the tab's CHP declaration is computed from the
/// same values that decided what to emit — the OpenCode side's discipline
/// (`opencode_plugin_source` builds `serves`/`cannot` from `OpencodePluginFlags`)
/// applied to the harness that has no plugin file to bake them into.
struct ClaudeHookFlags {
    prompt: bool,
    compact: bool,
    read_advisor: bool,
    post_edit: bool,
    notify: bool,
    /// The two beacons. `type: "http"` like everything else since 2026-08-17 —
    /// this comment used to explain why two COMMAND hooks still belonged in a
    /// declaration about `type: "http"` entries, and the answer (`serves`
    /// describes the tab's L1, not one transport) is now simply unexercised.
    ///
    /// `checkpoint` additionally decides whether the tab has an entry whose
    /// handler BLOCKS the tool call while it works, which is the one place in
    /// this overlay where a `serves` claim is also a latency claim.
    taint_beacon: bool,
    checkpoint: bool,
    /// V35 Phase L: the three read-path pushes. Each is what turns its
    /// fallback reader's tap OFF for this tab (`chp::served`), so a `false`
    /// here is not merely "no push" — it is "the transcript tail keeps serving
    /// it", which is the sentence the `cannot` reason has to say.
    stop: bool,
    tool_result: bool,
    subagent: bool,
}

/// The `serves` / `cannot` declaration for one Claude tab — the payload of
/// `X-CIMP-Hello` on the `SessionStart` entry, and the twin of the generated
/// OpenCode plugin's hello body.
///
/// Every Claude-servable CHP event lands on exactly one side, so an absence
/// reads as *unavailable, with a reason* rather than as *nobody wrote it down*
/// (global principle 5). Deliberately **not** derived server-side from live
/// settings at hello time: `SessionStart` also fires on `resume` / `clear` /
/// `compact`, potentially long after the spawn, and a declaration recomputed
/// then would describe settings the running overlay never saw — which is the
/// exact class of drift `chp` exists to make legible.
///
/// `harness_version` is absent for the reason `docs/CHP.md` § 6.2 gives for
/// OpenCode: no hook-input field carries the CLI version, and baking in the
/// number cImp last saw would be cImp attesting to itself.
fn claude_hello(settings: &Settings, flags: ClaudeHookFlags) -> claude_hook::Hello {
    use crate::harness::chp;
    let mut hello = claude_hook::Hello::default();
    // Unconditional: this message is the hello, and every hook emitted below
    // reports payload drift through the same in-process path.
    hello.serves.push(chp::EV_HELLO.to_string());
    hello.serves.push(chp::EV_CONTRACT_DRIFT.to_string());
    hello.declare(
        flags.prompt,
        chp::EV_PROMPT,
        "context injection and Workbench checkpoints are both off for this tab (the prompt tap \
         needs the graph plus one of them)",
    );
    hello.declare(
        flags.compact,
        chp::EV_CONTEXT_COMPACTION,
        "compaction carry-over is off (needs the graph, `graph.context_injection` and \
         `graph.compaction_context`)",
    );
    hello.declare(
        flags.read_advisor,
        chp::EV_CONTEXT_SHOULD_READ,
        if crate::harness::plugin::read_advisor_gate_blocked(settings) {
            "the read advisor is BLOCKED by the capability matrix — `claude.hook.pretooluse_deny` \
             is recorded as failed, so a deny's reason would never reach the model"
        } else {
            "the read advisor is off for this tab (needs the graph and `graph.read_advisor`)"
        },
    );
    hello.declare(
        flags.post_edit,
        chp::EV_CONTEXT_POST_EDIT,
        "auto-check is off for this tab (needs the graph, `graph.auto_check`, and at least one \
         configured check)",
    );
    hello.declare(
        flags.notify,
        chp::EV_PERMISSION_EVENT,
        "no cImp loopback runs on this install (offload, graph and the Code Audit MCP are all \
         off), so permission detection is regex-only",
    );
    hello.declare(
        flags.taint_beacon,
        chp::EV_TAINT_BEACON,
        "native web visibility is not `sensor` for this tab — in `deny` the overlay's permission \
         block refuses those tools outright, so there is nothing to observe",
    );
    hello.declare(
        flags.checkpoint,
        chp::EV_CHECKPOINT_PRE_MUTATION,
        "Workbench checkpoints are off",
    );
    // V35 Phase L. Every `cannot` here names the FALLBACK that keeps serving the
    // capability, because that is what an operator needs to know: an absence
    // from `serves` means "still Tier C on the transcript tail", not "gone".
    hello.declare(
        flags.stop,
        chp::EV_ASSISTANT_TEXT,
        "no cImp loopback runs on this install, so the `Stop` hook has nowhere to post — assistant \
         prose still reaches TTS through the transcript tail (`claude.transcript.assistant_text`, \
         Tier C)",
    );
    hello.declare(
        flags.tool_result,
        chp::EV_SESSION_TOOL_RESULT,
        "the graph is off (or there is no loopback), so nothing consumes tool-result sizes — the \
         transcript tail keeps serving them when it is on (`claude.transcript.tool_result`)",
    );
    hello.declare(
        flags.subagent,
        chp::EV_SESSION_SUBAGENT,
        "no cImp loopback runs on this install, so sub-agent lifecycle is still inferred from the \
         transcript's `isSidechain` lines and `Task`/`Agent` tool_use blocks \
         (`claude.transcript.subagents`, Tier C)",
    );
    // NOT declared on either side, and that is the honest answer rather than an
    // omission: `session.usage` and `session.context` have NO Claude producer to
    // declare. No hook payload carries token counts (`PostCompact` exposes no
    // compaction metrics either) and none carries the statusline's context
    // window, so `claude.transcript.usage` and `claude.statusline.stdin` remain
    // Tier C permanently-until-upstream-changes. A `cannot` entry would imply a
    // per-tab decision cImp made; there is none to make.
    hello
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
///   * `--dangerously-load-development-channels server:cimp-offload` —
///     V30 Phase A session-push registration, gated on the default-off
///     `offload.session_push` (see [`CHANNEL_REGISTRATION_FLAG`]).
///
/// V28: `tab` is the launching tab's id, baked into the `cimp-offload` MCP
/// child's argv (`--tab <id>`) so the app can resolve which of this agent's
/// sessions a `context_*` call belongs to.
///
/// **V35 Phase J: `endpoint` is this instance's loopback.** Five of the hook
/// entries are now `type: "http"` and carry a baked URL, so the generator needs
/// the port at spawn time — the same `read_own_discovery()` value
/// [`write_opencode_plugin`] bakes into the generated plugin, and for the same
/// reason (pid-keyed, never the shared last-writer-wins file a sibling instance
/// may have overwritten). `None` ⇒ those five entries are not emitted at all,
/// stated as a consequence rather than hidden: a command hook could be installed
/// before the loopback existed and would find it later through discovery, an
/// http hook cannot. Every one of the five is gated on a setting that implies
/// `loopback_needed()`, so the loopback is running by the time any tab spawns;
/// the residual is a tab launched in the window before the listener bound.
pub(crate) fn build_pre_args(
    cfg: &AiToolTabConfig,
    settings: &Settings,
    tab: &str,
    endpoint: Option<&crate::offload::loopback::Discovery>,
) -> Vec<String> {
    // Belt and braces: `ClaudePlugin::pre_args` is the only production caller
    // and it is reached only for a Claude tab, but the tests drive this
    // directly with both harnesses' configs and the guard is what makes those
    // assertions mean something.
    if crate::harness::HarnessId::from_command(&cfg.command) != super::plugin::id() {
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
        // V32 Phase F: read once — the mode gates both the beacon hook and the
        // permission denial below, and the two must never disagree.
        // V32 Phase G: resolved for THIS tab, so a per-tab override reaches
        // both halves together for the same reason.
        let native_web = native_web_for(settings, "claude", tab);
        if super::settings::statusline_enabled(settings) {
            if let Some(command) = super::statusline::launch_command() {
                // `refreshInterval` (seconds) re-runs the command on a timer in
                // addition to event-driven updates, so the `rate_limits` usage
                // push (see [`super::usage`]) keeps flowing — and the bottom-bar
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
            // ── V35 Phase J: the five `type: "http"` entries ────────────────
            //
            // Their gates are unchanged, to the boolean — that is the phase's
            // whole risk posture — so they are hoisted here and used both to
            // decide what to emit and to declare it in the hello below. Each is
            // ANDed with `http_port`: an http hook carries a baked URL, so
            // without this instance's loopback endpoint there is nothing to
            // point it at. Every one of these gates implies `loopback_needed()`,
            // so in practice the endpoint is always there — see `build_pre_args`'
            // own note on the launch-window residual.
            let http_port: Option<u16> = endpoint.map(|d| d.port);
            // V13 Phase C: widened from `context_injection` alone so the
            // prompt-tap checkpoint trigger (`workbench::on_prompt`, called from
            // the `/context/retrieve` handler BEFORE its own injection gate)
            // still runs when the user wants checkpoints but has injection off —
            // the milestone's Decision 4. The retrieve handler's own *injection*
            // gate is unaffected; it stays on `context_injection` alone.
            let prompt_hook =
                settings.graph.enabled && (settings.graph.context_injection || settings.workbench.checkpoints);
            // V11 Phase D: carry the working set through a compaction. Kept on
            // its own narrower condition (still requires injection, unlike the
            // widened UserPromptSubmit hook) — compaction survival is meaningless
            // without injection to feed.
            let compact_hook = settings.graph.enabled
                && settings.graph.context_injection
                && settings.graph.compaction_context;
            // V11 Phase E: the read advisor (opt-in; independent of the injection
            // toggle, but still needs the graph). V16 Feature 0: a recorded E1
            // spike FAILURE (the deny reason never reaches the model — every
            // remind would be a bare refusal) hard-blocks it regardless of the
            // toggle. V35 Phase E made that block the capability matrix's gate,
            // asked by id: `contract::gate(CAP_PRETOOLUSE_DENY, ..)` owns the
            // fail-closed reading of unrecognized hand-typed values, and the SAME
            // query answers the Settings window over IPC — so the toggle and this
            // hook can no longer disagree by one of them being re-implemented in
            // TypeScript. The registry refreshes `harness_versions` from the
            // physical global file at spawn, so a hand-recorded outcome takes
            // effect on the next tab launch, not the next app restart.
            let read_hook = settings.graph.enabled
                && settings.graph.read_advisor
                && !crate::harness::plugin::read_advisor_gate_blocked(settings);
            // V17 Phase B: a second matcher intercepts a whole-file shell read
            // (`cat FILE`) of an already-read file via the SAME route (which
            // dispatches on `tool_name`).
            let read_hook_shell = read_hook && settings.graph.read_advisor_shell;
            // V12 Phase F (6a/6b): auto-check after an edit — opt-in (behavior
            // hook), needs the graph AND at least one configured check (nothing
            // to run otherwise).
            let post_edit_hook =
                settings.graph.enabled && settings.graph.auto_check && !settings.checks.is_empty();
            // NC-2 (issue #5): the `Notification` / `PermissionDenied` pair —
            // the PRIMARY "this tab is awaiting a permission decision" detector,
            // demoting the TUI-regex matcher (`processing::permission`) to
            // fallback. See the long note at its emission site for the H2 reason
            // it is gated on `loopback_needed()` and the accepted tradeoff.
            let notify_hook = settings.loopback_needed();
            // ── V35 Phase L: the read path, pushed ──────────────────────────
            //
            // All three are gated on `loopback_needed()` alone (plus the graph
            // for the one whose consumer is the graph), and NOT on the feature
            // switch each capability's fallback reader consults. That is
            // deliberate and it is what keeps a live toggle live:
            //
            //   * `tts_injection.enabled` is per-tab and read LIVE by
            //     `tts::prose::speak_prose` on every burst. Baking it into the
            //     hook gate would make "turn TTS on for this tab" require a tab
            //     restart — a regression against the reader path, which re-reads
            //     it per sentence. The cost of not gating is one loopback POST
            //     per turn on a tab with TTS off, which the handler drops at the
            //     same live check the reader uses.
            //   * `graph.enabled` DOES gate the tool-result push, because
            //     without the graph there is no `UsageEvent` sink at all — the
            //     reader's own tap is `ctx.mem.is_none()`-gated for the same
            //     reason, so the two agree.
            //
            // Neither needs a new `spawn_inject_sig` slot: `loopback_needed()`
            // already rides the `"notify_hooks"` key and `graph.enabled` already
            // moves the `"guidance"` array, so every input that can change what
            // these declare already raises the restart hint.
            let stop_hook = settings.loopback_needed();
            let tool_result_hook = settings.graph.enabled && settings.loopback_needed();
            let subagent_hook = settings.loopback_needed();
            // ── 2026-08-17: the two migrated beacons ────────────────────────
            //
            // Hoisted here with the rest, and their gates are unchanged to the
            // boolean — that is this migration's whole risk posture, exactly as
            // it was Phase J's. They were previously spelled inline at the two
            // emission sites AND again in the `ClaudeHookFlags` block below,
            // which is precisely how an emitted artifact and its own hello come
            // to disagree; one binding each, read twice.
            let taint_beacon_hook =
                native_web == NativeWebVisibility::Sensor && settings.loopback_needed();
            let checkpoint_hook = settings.workbench.checkpoints && settings.loopback_needed();
            // V32 Phase F: `PreToolUse` now has TWO independent producers (the
            // V11 read advisor and the Phase F web beacon), so its entries
            // accumulate here and are inserted once. Claude Code evaluates every
            // matching entry, so a beacon on `WebFetch|WebSearch` and an advisor
            // on `Read|Bash` never interfere.
            let mut pre_tool_use: Vec<serde_json::Value> = Vec::new();
            if let Some(port) = http_port.filter(|_| prompt_hook) {
                // V35 Phase J: `type: "http"`. The tab id rides `X-CIMP-Tab`
                // for the reason `--tab` used to be baked into argv — a
                // `UserPromptSubmit` payload carries `session_id` and `cwd` but
                // nothing that names a cImp tab, and the checkpoint this hook
                // triggers needs the tab id to tell two Claude tabs on one
                // project root apart in the Timeline.
                hooks.insert(
                    "UserPromptSubmit".to_string(),
                    serde_json::json!([ { "hooks": [
                        claude_hook::http_hook_entry(port, tab, claude_hook::ROUTE_USER_PROMPT_SUBMIT, None)
                    ] } ]),
                );
            }
            if let Some(port) = http_port.filter(|_| compact_hook) {
                hooks.insert(
                    "PreCompact".to_string(),
                    serde_json::json!([ { "hooks": [
                        claude_hook::http_hook_entry(port, tab, claude_hook::ROUTE_PRE_COMPACT, None)
                    ] } ]),
                );
            }
            if let Some(port) = http_port.filter(|_| read_hook) {
                // BOTH matchers reach the same route, which dispatches on
                // `tool_name` — so the entry is built once and cloned rather
                // than spelled twice.
                let entry =
                    claude_hook::http_hook_entry(port, tab, claude_hook::ROUTE_PRE_TOOL_USE, None);
                pre_tool_use.push(serde_json::json!({
                    "matcher": "Read",
                    "hooks": [ entry.clone() ]
                }));
                // Gated on the sub-toggle, so it's a zero overlay delta when off.
                if read_hook_shell {
                    pre_tool_use.push(serde_json::json!({
                        "matcher": "Bash",
                        "hooks": [ entry ]
                    }));
                }
            }
            // V32 Phase F (locked decision 14), `sensor` mode: a report-only
            // `PreToolUse` beacon on the harness's OWN web tools. Claude's
            // `WebFetch`/`WebSearch` never route through cImp, so the proxy
            // latch cannot see them — without this the session can ingest a
            // hostile page while `/status` still reads `open`. It POSTs to
            // `/claude/hook/pre_tool_use_taint`, whose handler reaches the same
            // `/latch/beacon` core and engages the tab's EXTERNAL latch exactly
            // as a proxied fetch would.
            //
            // **`type: "http"` since 2026-08-17** (was `cimp --taint-beacon`,
            // deleted). Report-only is now structural rather than argued: a
            // `PreToolUse` hook denies ONLY by answering 2xx with a
            // `permissionDecision`, and the handler answers `{}` on every path.
            // A dead app, a rotated token, a non-2xx and a timeout are all
            // documented as non-blocking — which is locked decision 14's "sensor
            // mode must never break a tab" resting on a written contract instead
            // of on the undocumented timeout semantic the shim was built around.
            //
            // GATED ON `loopback_needed()` for the H2 reason the NC-2 hooks are
            // (see the long note below), and now structurally as well: an http
            // entry has no endpoint to bake without a running loopback, so
            // `http_port` is `None` and nothing is emitted. Consequence, stated
            // honestly: on an install with offload, graph and Code-Audit-MCP all
            // off there is no proxy latch to engage either, so the beacon has
            // nothing to report to — inert, not silently broken.
            //
            // The tab id rides `X-CIMP-Tab` (it was `--tab` in argv) because a
            // hook payload carries no tab identity — the E2 spike's finding —
            // and the tab id is the key the whole latch registry is built on.
            if let Some(port) = http_port.filter(|_| taint_beacon_hook) {
                pre_tool_use.push(serde_json::json!({
                    "matcher": CLAUDE_WEB_TOOL_MATCHER,
                    "hooks": [
                        claude_hook::http_hook_entry(
                            port,
                            tab,
                            claude_hook::ROUTE_PRE_TOOL_USE_TAINT,
                            None,
                        )
                    ]
                }));
            }
            // V33 Phase F: the THIRD `PreToolUse` producer — a report-only
            // checkpoint beacon on the harness's own MUTATING tools. Claude's
            // `Edit`/`Write`/`MultiEdit`/`Bash` never route through cImp, so
            // the only thing that can fire a checkpoint *before* one of them is
            // a hook; without it the Timeline's finest granularity is the
            // prompt, which by the time it matters contains a dozen edits.
            //
            // Gated on `workbench.checkpoints` — the same single switch
            // `WorkbenchService::checkpoints_enabled` reads, so the hook exists
            // exactly when the app would act on it — AND on `loopback_needed()`
            // for the H2 reason every other entry is.
            //
            // Deliberately NOT also gated on `graph.enabled`, unlike the
            // UserPromptSubmit checkpoint trigger above. That one rides the
            // `/context/retrieve` route, which is a graph feature and carries
            // checkpointing as a passenger (V13 Decision 4); this route is
            // Workbench's own and has no graph dependency, so tying it to the
            // graph would make a checkpoint setting silently depend on an
            // unrelated one.
            //
            // **`type: "http"` since 2026-08-17** (was `cimp --checkpoint-beacon`,
            // deleted), and this is the entry the migration is really about. The
            // 2026-08-13 amendment made the shim WAIT for the app's reply,
            // because Claude runs the tool the instant the hook finishes and a
            // snapshot taken concurrently with the edit is a snapshot of the
            // damage. That wait is now the handler's own: a `PreToolUse` http
            // hook blocks the tool call until the response, which is the
            // documented mechanism that makes `permissionDecision` expressible —
            // so the ordering the feature's whole claim rests on is upstream's
            // written contract rather than an observed behaviour. The app still
            // bounds itself (`harness::ingress::hook_reply_budget()`, 1800 ms today) and
            // abandons an unfinished snapshot rather than writing a row that
            // might contain the change it claims to predate; the entry's
            // `timeout` is 5 s, a backstop over that (see
            // `claude_hook::TIMEOUT_CHECKPOINT_SECS`).
            if let Some(port) = http_port.filter(|_| checkpoint_hook) {
                pre_tool_use.push(serde_json::json!({
                    "matcher": CLAUDE_MUTATING_TOOL_MATCHER,
                    "hooks": [
                        claude_hook::http_hook_entry(
                            port,
                            tab,
                            claude_hook::ROUTE_PRE_TOOL_USE_CHECKPOINT,
                            None,
                        )
                    ]
                }));
            }
            // V35 Phase L: `PostToolUse` now has TWO producers, and they are
            // deliberately two ENTRIES pointing at two ROUTES rather than one
            // widened matcher. Claude evaluates every matching group, so an
            // `Edit` fires both — which is exactly why they must not share a
            // route: one shared route would run the auto-check twice and count
            // one tool result twice, the two double-delivery failures this phase
            // is most exposed to. CHP rule 4 ("a route is never repurposed")
            // says the same thing from the protocol side. The auto-check entry
            // below is therefore byte-identical to what Phase J emitted.
            let mut post_tool_use: Vec<serde_json::Value> = Vec::new();
            if let Some(port) = http_port.filter(|_| post_edit_hook) {
                // This is the hook whose route EXECUTES the project's configured
                // checks, so it is the one whose taint gate most needs a scope to
                // resolve — `X-CIMP-Tab` is what gives it one.
                post_tool_use.push(serde_json::json!({
                    "matcher": "Edit|Write|MultiEdit",
                    "hooks": [
                        claude_hook::http_hook_entry(port, tab, claude_hook::ROUTE_POST_TOOL_USE, None)
                    ]
                }));
            }
            if let Some(port) = http_port.filter(|_| tool_result_hook) {
                // `"matcher": ""` — every tool, because tool-result SIZING is
                // about every tool the model ran, not about the edit tools. The
                // transcript tail it replaces is equally indiscriminate
                // (`extract_tool_results` reads every `tool_result` block), so
                // narrowing here would lose rows the fallback was collecting.
                post_tool_use.push(serde_json::json!({
                    "matcher": "",
                    "hooks": [
                        claude_hook::http_hook_entry(
                            port,
                            tab,
                            claude_hook::ROUTE_POST_TOOL_USE_RESULT,
                            None,
                        )
                    ]
                }));
            }
            if !post_tool_use.is_empty() {
                hooks.insert(
                    "PostToolUse".to_string(),
                    serde_json::Value::Array(post_tool_use),
                );
            }
            // 2026-08-17: `PostToolUseFailure` — a SEPARATE hook EVENT upstream
            // (new since 2.1.63), not a third `PostToolUse` matcher group.
            // `PostToolUse` fires only when a tool succeeds, so before this
            // entry a failed tool result reached cImp only through the
            // transcript tail — which is arbitrated OFF on exactly the tabs that
            // serve `session.tool_result`. A serving tab therefore lost every
            // failed result's size, which for a failing `Bash` is as much text
            // as a succeeding one.
            //
            // Gated on the SAME boolean as the success entry, and that coupling
            // is load-bearing rather than tidy: the reader's tap is suppressed
            // per CAPABILITY (`session.tool_result`), so an overlay that wired
            // one entry without the other would either lose failures (as today)
            // or double-count them. `"matcher": ""` for the same reason the
            // success entry uses it — sizing is about every tool the model ran.
            if let Some(port) = http_port.filter(|_| tool_result_hook) {
                hooks.insert(
                    "PostToolUseFailure".to_string(),
                    serde_json::json!([ { "matcher": "", "hooks": [
                        claude_hook::http_hook_entry(
                            port,
                            tab,
                            claude_hook::ROUTE_POST_TOOL_USE_FAILURE,
                            None,
                        )
                    ] } ]),
                );
            }
            // V35 Phase L: `Stop` carries `last_assistant_message` — the
            // complete final assistant text of the turn, which is the SAME unit
            // and the SAME cadence the transcript tail delivers to TTS today.
            // That equivalence is the migration (locked decision 2): the
            // segmenter's input does not change, so recipe 10 is a confirmation
            // rather than a hope. `MessageDisplay` is deliberately not wired —
            // it would deliver per-chunk deltas on the streaming hot path.
            if let Some(port) = http_port.filter(|_| stop_hook) {
                hooks.insert(
                    "Stop".to_string(),
                    serde_json::json!([ { "hooks": [
                        claude_hook::http_hook_entry(port, tab, claude_hook::ROUTE_STOP, None)
                    ] } ]),
                );
            }
            // V35 Phase L: `SubagentStart` + `SubagentStop` on ONE route, which
            // dispatches on `hook_event_name` exactly as the notification pair
            // does. `"matcher": ""` for the documented "all agent types" form —
            // narrowing on `agent_type` would make a new sub-agent type
            // silently invisible to the avatar, which is the failure this row
            // moved off Tier C to escape.
            if let Some(port) = http_port.filter(|_| subagent_hook) {
                let entry =
                    claude_hook::http_hook_entry(port, tab, claude_hook::ROUTE_SUBAGENT, None);
                for event in ["SubagentStart", "SubagentStop"] {
                    hooks.insert(
                        event.to_string(),
                        serde_json::json!([ { "matcher": "", "hooks": [ entry.clone() ] } ]),
                    );
                }
            }
            // NC-2 (issue #5): `Notification` + `PermissionDenied` — the
            // PRIMARY "this tab is awaiting a permission decision" detector,
            // demoting the TUI-regex matcher (`processing::permission`) to
            // fallback. Both point at the SAME route, which dispatches on the
            // payload's `hook_event_name` exactly as the one `--notify-hook`
            // binary did.
            //
            // H2 fix (2026-08-05 review): GATED on `loopback_needed()`. The
            // shim's ONLY delivery path was the loopback, and the loopback server
            // starts only under that predicate (`main.rs`). Injecting the hooks
            // without it spawned a `cimp --notify-hook` process per Claude
            // notification whose POST had nowhere to land — the primary signal
            // dead, silently. V35 Phase J made the gate structural as well as
            // deliberate: an http hook has no endpoint to bake without a running
            // loopback, so `http_port` is `None` and nothing is emitted. The
            // schema's invariant — every spawn-time advertisement must be a
            // subset of `loopback_needed` — is unchanged; the tripwire is
            // `every_advertised_mcp_server_gets_a_loopback`.
            //
            // ACCEPTED TRADEOFF: on a DEFAULT install (offload + graph +
            // code_audit all off) permission detection is regex-only. That is
            // the status quo ante for such installs — the hook never worked
            // there — and it is strictly better than pointing a hook at a closed
            // socket. Hook-primary detection requires one of offload / graph /
            // Code-Audit-MCP to be on. Do NOT "fix" this by making the loopback
            // always run: keeping it off for feature-less installs was a
            // deliberate v0.48.0 decision.
            //
            // Because the injection is Settings-DEPENDENT and baked at spawn, it
            // carries a `spawn_inject_sig` entry (`"notify_hooks"`) so toggling
            // one of those features raises the restart hint — a running tab
            // launched without the hooks would otherwise stay hook-blind with no
            // indication.
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
            if let Some(port) = http_port.filter(|_| notify_hook) {
                let entry =
                    claude_hook::http_hook_entry(port, tab, claude_hook::ROUTE_NOTIFICATION, None);
                for event in ["Notification", "PermissionDenied"] {
                    hooks.insert(
                        event.to_string(),
                        serde_json::json!([ { "matcher": "", "hooks": [ entry.clone() ] } ]),
                    );
                }
            }
            // ── V35 Phase J: the CHP hello (design D3, milestone decision 5) ──
            //
            // A `SessionStart` hook whose only job is to introduce this tab's
            // overlay — the version it speaks and what it was actually wired to
            // serve. It is the Claude analogue of the generated OpenCode plugin's
            // module-scope hello, and it is what makes Phase I's stale-artifact
            // detection cover Claude tabs (`chp::expects_chp`).
            //
            // Gated on there being any hook at all: an overlay that wired nothing
            // has nothing to introduce, and on a feature-less install the
            // loopback is not running to hear it. `serves`/`cannot` are computed
            // from the SAME booleans that decided what was emitted above, three
            // lines up, so the declaration cannot claim something the overlay did
            // not wire.
            if let Some(port) = http_port.filter(|_| !hooks.is_empty() || !pre_tool_use.is_empty()) {
                let hello = claude_hello(
                    settings,
                    ClaudeHookFlags {
                        prompt: prompt_hook,
                        compact: compact_hook,
                        read_advisor: read_hook,
                        post_edit: post_edit_hook,
                        notify: notify_hook,
                        taint_beacon: taint_beacon_hook,
                        checkpoint: checkpoint_hook,
                        stop: stop_hook,
                        tool_result: tool_result_hook,
                        subagent: subagent_hook,
                    },
                );
                hooks.insert(
                    "SessionStart".to_string(),
                    serde_json::json!([ { "hooks": [
                        claude_hook::http_hook_entry(
                            port,
                            tab,
                            claude_hook::ROUTE_SESSION_START,
                            Some(&hello),
                        )
                    ] } ]),
                );
            }
            if !pre_tool_use.is_empty() {
                hooks.insert(
                    "PreToolUse".to_string(),
                    serde_json::Value::Array(pre_tool_use),
                );
            }
            if !hooks.is_empty() {
                overlay.insert("hooks".to_string(), serde_json::Value::Object(hooks));
            }
        }
        // V32 Phase F (locked decision 14), `deny` mode: close the native web
        // route by CONFIG rather than by hook. A `permissions.deny` rule is
        // enforced by Claude Code itself before the tool runs — no shim, no
        // latency, nothing for a compromised model to talk its way past — and
        // it rides the same session-scoped `--settings` overlay as everything
        // else, so `~/.claude` is still never touched.
        //
        // Bare tool names (see [`CLAUDE_WEB_DENY_RULES`]). No `allow`/`ask`
        // keys: the overlay states one intent and leaves the user's own
        // permission configuration otherwise intact.
        //
        // Ungated by `loopback_needed()`, unlike the sensor hook — a denial
        // needs no app to talk to, and its whole point is to hold on the
        // installs where the proxy is not carrying the web traffic.
        if native_web == NativeWebVisibility::Deny {
            overlay.insert(
                "permissions".to_string(),
                serde_json::json!({ "deny": CLAUDE_WEB_DENY_RULES }),
            );
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
    // ride the same overlay:
    //
    //   - V8-01 + V9-01 `cimp-offload` (`--offload-mcp`) carries the
    //     `offload_task` tool, the `graph_*` tools, AND any MCP server exposed
    //     to Claude Code. **V37 Phase F: UNCONDITIONAL** — see the block
    //     comment on the insert below.
    //   - V26 `cimp-code-audit` (`--code-audit-mcp`) carries `security_audit` /
    //     `quality_audit`. Injected when Code Audit is enabled AND opted in for
    //     the Claude consumer (`code_audit.expose_claude`).
    //
    // The `--mcp-config` flag is emitted only if at least one server made the
    // cut — which, since Phase F, is every Claude tab whose exe path resolves.
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.to_string_lossy().to_string();
        let mut servers = serde_json::Map::new();
        {
            // ── V37 Phase F: the proxy child is UNCONDITIONAL in AI tabs ────
            //
            // It used to be gated on `advertises_offload_to_claude` (offload
            // enabled ∨ graph enabled ∨ any Claude-exposed MCP server). That
            // gate made a whole class of settings changes un-propagatable: a
            // tab spawned with none of them had no stdio child, so there was no
            // channel for the V37 contract-C5 pulse to travel on and granting a
            // server later needed a fresh tab — which is the restart-tab
            // semantics V37 exists to remove.
            //
            // The child is cheap when it has nothing to serve: its `tools/list`
            // is assembled live (offload tools gated on `offload.enabled`,
            // `graph::mcp_tools()` on the graph feature, and the proxied surface
            // fetched per call from `POST /mcp/list`), so with everything off it
            // advertises an EMPTY tool list — legal MCP, and self-consistent
            // with the loopback being down in exactly that case (nothing to
            // strand; see `every_advertised_mcp_server_gets_a_loopback`).
            //
            // Self-healing in both directions: `read_discovery_for` memoizes
            // successes only and `events_relay` re-resolves on a 2s retry loop,
            // so a child spawned before the loopback existed finds it when a
            // grant brings it up, and the relay's post-connect
            // `tools/list_changed` refreshes a client that missed the pulse.
            //
            // V28 (issue #13): `--tab <id>` binds this per-tab child to the tab
            // that spawned it. The child forwards it on `/graph_run`, where the
            // app resolves it against the live-session registry — that is what
            // stops two Claude tabs on one project from sharing (and stealing)
            // a memory scope. Unconditional and NOT Settings-derived, so it
            // needs no `spawn_inject_sig` entry / restart hint (same reasoning
            // as the `Notification` hook above).
            let mut child_args = vec![
                "--offload-mcp".to_string(),
                "--tab".to_string(),
                tab.to_string(),
            ];
            // V30 (M5): the CHILD half of the session-push gate, baked into the
            // child's own argv from THIS settings snapshot — the same one that
            // decides the client half (`CHANNEL_REGISTRATION_FLAG`) a few lines
            // below. One read, one decision, two flags: a child that
            // crash-restarts mid-session re-declares exactly what the running
            // Claude process registered, instead of re-reading a settings file
            // that may have been toggled since (which would leave the app
            // pushing into a session that never registered the channel).
            if settings.offload.session_push {
                child_args.push(CHANNEL_PUSH_FLAG.to_string());
            }
            servers.insert(
                "cimp-offload".to_string(),
                serde_json::json!({ "command": exe, "args": child_args }),
            );
        }
        if crate::harness::plugin::audit_advertised(settings, super::plugin::me()) {
            // V32 C-1b: `--tab <id>` here for the same reason as on the offload
            // child, and NOT for the same purpose. The audit child resolves no
            // memory scope — it takes no arguments and always scans the app's
            // own launch root — but a taint latch is keyed by `(agent, tab)`,
            // and `security_audit`/`quality_audit` became LOCAL-CAPABILITY on
            // 2026-08-07. Without an identity, `/audit/run` has no latch to
            // consult and a contaminated tab keeps a gitleaks report one tool
            // call away. Unconditional and not Settings-derived, so it needs no
            // `spawn_inject_sig` entry (same reasoning as the offload child's).
            servers.insert(
                "cimp-code-audit".to_string(),
                serde_json::json!({
                    "command": exe,
                    "args": ["--code-audit-mcp", "--tab", tab]
                }),
            );
        }
        if !servers.is_empty() {
            let mcp = serde_json::json!({ "mcpServers": servers });
            args.push("--mcp-config".to_string());
            args.push(mcp.to_string());
        }
    }

    // V30 Phase A: register the `cimp-offload` child as a session channel so it
    // can push out-of-band notices into this tab. Gated on the default-off
    // `offload.session_push` (research preview — see
    // [`CHANNEL_REGISTRATION_FLAG`]) and nothing else.
    //
    // V37 Phase F removed the second conjunct (`advertises_offload_to_claude`),
    // which existed so a channel was never registered against a server that the
    // overlay above had not defined. The overlay above now ALWAYS defines
    // `cimp-offload`, so the conjunct is constant-true and its only remaining
    // effect would be to keep a dead input in `spawn_inject_sig`'s `"channels"`
    // entry — nagging every tab to restart for an MCP-grant flip that now
    // propagates live. The `session_push` half is still carried there, so
    // flipping the research-preview toggle still raises the restart hint.
    // Claude-only, like every other pre-arg here.
    if settings.offload.session_push {
        args.push(CHANNEL_REGISTRATION_FLAG.to_string());
        args.push(CHANNEL_REGISTRATION_TARGET.to_string());
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every tool the pre-mutation matcher names must be one the checkpoint
    /// core will accept.**
    ///
    /// Moved here from `checkpoint_beacon.rs` when that shim was deleted
    /// (2026-08-17); the matcher it guards lives in this file, so this is where
    /// it belongs. The matcher and `tools::CLAUDE_NATIVE_TABLE` are still edited
    /// separately, and a matcher naming a tool with no `mutates_fs: true` row now
    /// costs more than it used to: the entry's handler blocks the tool call, so a
    /// mismatch means a call held for a checkpoint the core immediately declines
    /// — a silently dead seam with a latency bill.
    ///
    /// The reverse direction is deliberately NOT asserted: `run_command` is
    /// mutating and is not a Claude tool at all, so the table is legitimately
    /// wider than the matcher.
    #[test]
    fn every_matched_claude_tool_is_classified_as_mutating() {
        for tool in CLAUDE_MUTATING_TOOL_MATCHER.split('|') {
            assert!(
                crate::harness::claude::tools::claude_native_mutates_fs(tool),
                "`{tool}` is in the PreToolUse matcher but has no `mutates_fs: true` row — \
                 every matched call would be held for a checkpoint the core refuses"
            );
        }
    }
}
