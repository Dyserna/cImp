//! V40 Phase A — **Claude Code's [`HarnessPlugin`]**: everything
//! `tabs/config.rs` used to branch on `command_is(.., "claude")` for.
//!
//! Every body here is the pre-V40 body, moved verbatim (the Phase K rule: same
//! text, same tests). What changed is *who asks*: core now calls one interface
//! method per launch step and never names this harness.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::harness::plugin::{
    Canary, GrantCtx, HarnessPlugin, InputProfile, NativeTool, ProbeOutput,
};
use crate::harness::registry::HarnessId;
use crate::harness::reader::OobSpec;
use crate::sandbox::{GrantAccess, GrantRow};
use crate::settings::{AiToolTabConfig, Settings, TabConfig};

use super::hook as claude_hook;

/// The one instance. A ZST, so the registry can hold `&'static dyn
/// HarnessPlugin` without a `static` allocation of its own.
pub struct ClaudePlugin;

/// The value the registry's descriptor points at.
pub static PLUGIN: ClaudePlugin = ClaudePlugin;

/// **The five timings core's TUI activity arbitration used to hard-code**
/// (locked decision 18).
///
/// Every number is a measurement of Claude Code's screen, and each was tuned
/// against an observed avatar defect:
///
/// * `burst_min` (1000 ms) — real responses sustain bytes for seconds; a
///   per-keystroke TUI redraw is tens of ms, so anything shorter is churn.
/// * `quiet` (500 ms) — closes a burst once the marker is gone. Claude routinely
///   emits nothing for >0.5 s mid-response, which is why the MARKER and not this
///   timer decides Idle while it is working.
/// * `marker_grace` (1200 ms) — while Claude drives parallel sub-agents its
///   `esc to interrupt` footer blinks in and out roughly once a second. Each gap
///   used to trip the 500 ms release, so the avatar cycled Thinking → Idle →
///   Thinking every second and announced "idle" on each cycle.
/// * `working_stale` (6 s) — the live spinner repaints its elapsed-second
///   counter ~once/sec, so a marker still matched with the stream fully silent
///   this long is a ghost left in the cell grid.
/// * `subagents_stall` (8 s) — longer than `working_stale` on purpose, so the
///   marker path always concludes first (asserted by
///   `harness::plugin::tests::every_stall_backstop_outlasts_its_marker_path`).
///
/// **Pinned by a golden test** rather than merely moved: these are the
/// pre-V40 constants to the millisecond, and a refactor that rounds one of them
/// is a regression nobody sees until an avatar flickers.
const ACTIVITY_TUNING: crate::harness::plugin::ActivityTuning =
    crate::harness::plugin::ActivityTuning {
        burst_min: std::time::Duration::from_millis(1000),
        quiet: std::time::Duration::from_millis(500),
        marker_grace: std::time::Duration::from_millis(1200),
        working_stale: std::time::Duration::from_secs(6),
        subagents_stall: std::time::Duration::from_secs(8),
    };

/// This plugin's own id, for the one place it has to recognise its own tabs
/// (the `claude_local` spawn-signature slot). Inside `harness/claude/` naming
/// this harness is the point; test 10(a) polices core, not here.
pub(in crate::harness) fn id() -> Option<HarnessId> {
    HarnessId::from_id("claude")
}

/// This plugin's own id, for the places inside `harness/claude/` that have to
/// recognise their own tabs. Inside this directory naming this harness is the
/// point; locked decision 10(a) polices core, not here.
pub(in crate::harness) fn me() -> HarnessId {
    id().expect("claude is a registered harness")
}

/// Whether any configured tab of THIS harness opted into the local provider.
///
/// The one condition under which the `local.*` rows reach a launch at all —
/// read by both halves of the spawn signature (the gated `local_env` element
/// and `spawn_baked_reaches_a_launch`), so the two cannot disagree about when
/// editing the proxy URL is worth a restart hint.
fn any_local_provider_tab(s: &Settings) -> bool {
    s.tabs.iter().any(|t| {
        matches!(t, TabConfig::AiTool(c)
            if c.use_local_provider && HarnessId::from_command(&c.command) == Some(me()))
    })
}

impl HarnessPlugin for ClaudePlugin {
    fn input_profile(&self) -> Option<InputProfile> {
        Some(super::input::input_profile())
    }

    fn pre_args(
        &self,
        cfg: &AiToolTabConfig,
        settings: &Settings,
        tab: &str,
        endpoint: Option<&crate::offload::loopback::Discovery>,
    ) -> Vec<String> {
        super::overlay::build_pre_args(cfg, settings, tab, endpoint)
    }

    /// cimp is documented as a drop-in replacement for `claude`, so invocation
    /// args (`cimp --resume <id>`, etc.) flow into every Claude tab — and
    /// `main.rs` composes the sentence in `cimp --help` from whichever harness
    /// answers `true` here (locked decision 26), so the promise and the tab the
    /// args reach cannot disagree.
    fn accepts_passthrough_argv(&self) -> bool {
        true
    }

    fn compose_env(
        &self,
        cfg: &AiToolTabConfig,
        settings: &Settings,
        _tab: &str,
        endpoint: Option<&crate::offload::loopback::Discovery>,
        env: &mut HashMap<String, String>,
    ) {
        // ── V35 Phase J: the bearer token for Claude's `type: "http"` hooks ──
        //
        // Every emitted hook entry sends `Authorization: Bearer
        // $CIMP_HOOK_TOKEN` and names that variable in `allowedEnvVars`; the
        // harness substitutes it from its OWN environment, which is this map.
        // An unlisted or unset name substitutes to the empty string, so a
        // missing value here is a silent 401 on every hook — which is why it is
        // set unconditionally for a Claude tab whenever this instance has a
        // loopback at all, rather than being ANDed with the per-hook gates.
        //
        // **Env rather than a literal in the overlay**, which is where the
        // OpenCode side puts it (`opencode_plugin_source` bakes it into a
        // file). The overlay is an argv value — `--settings <json>` — and argv
        // is readable by every process running as this user with no effort at
        // all. That is not a trust boundary either way (`docs/CHP.md` § 2: the
        // token means *a local process*, never *cImp's own child*), so this is
        // defence in depth, not containment.
        //
        // Not Settings-derived — the token is per app launch — so it needs no
        // `spawn_inject_sig` entry, same reasoning as `CIMP_TAB_ID`.
        if let Some(disc) = endpoint {
            env.insert(claude_hook::TOKEN_ENV.to_string(), disc.token.clone());
        }

        // V20: Claude Code runs in its native fullscreen (alternate-screen) TUI
        // — cImp no longer sets `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`. The old
        // inline forcing existed so the scrape pipeline could find `[[TTS]]`
        // markers and keep mouse gestures local; both concerns are retired.

        // ── `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` is DELIBERATELY NOT SET ─────
        //
        // Do not re-add it. Maintenance D-2 (2026-08-04) pinned it to `0` for
        // every Claude tab to disable Claude Code's ~2-minute MCP
        // auto-backgrounding, because cImp's loopback proxy and offload/audit
        // result handling were assumed to require a *synchronous* MCP return.
        //
        // V30 Phase 0 test T4 (2026-08-05, Claude Code 2.1.222) live-verified
        // that assumption is wrong: a backgrounded MCP call's **complete result
        // text** arrives in a `<task-notification>` message, losing nothing.
        // Blocking the harness for minutes per call was the more expensive half
        // of that trade, so V30 Phase C removed the kill switch.

        // Claude against a local provider: synthesize `ANTHROPIC_*` env.
        if cfg.use_local_provider {
            let [base_url, auth_token, model_alias] = super::settings::local_provider(settings);
            if !base_url.is_empty() {
                env.insert("ANTHROPIC_BASE_URL".to_string(), base_url);
            }
            if !auth_token.is_empty() {
                env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), auth_token);
            }
            if !model_alias.is_empty() {
                // Claude Code primarily uses --model flag for model selection,
                // but ANTHROPIC_MODEL is honored by some proxies; setting both
                // is harmless.
                env.insert("ANTHROPIC_MODEL".to_string(), model_alias);
            }
        }
    }

    fn resolve_oob(
        &self,
        cfg: &AiToolTabConfig,
        working_dir: &Path,
        extra_args: &mut Vec<String>,
        _env: &HashMap<String, String>,
    ) -> Option<OobSpec> {
        // V34: pin this tab's session id. `--session-id <uuid>` is the only
        // per-process discriminator Claude Code offers, and without one the tap
        // can only tail the newest `*.jsonl` under a project-derived root —
        // which two Claude tabs on one project share, making every tab-keyed
        // identity claim from either unprovable (V28 decision 4a). Generated
        // here, next to the `OobSpec` that carries it, so the flag on the
        // child's argv and the file the tap follows can never disagree.
        //
        // Skipped when the tab's own args already choose a session: `--resume`
        // and friends name a conversation that already exists, so a second
        // selector would either be rejected or silently fight the user's. Such
        // a tab keeps the pre-V34 newest-wins binding (and its ambiguity).
        let pinned_session = if args_select_session(extra_args) {
            tracing::debug!(
                tab = %cfg.id,
                "claude tab selects its own session; leaving it unpinned"
            );
            None
        } else {
            let sid = uuid::Uuid::new_v4().to_string();
            extra_args.push("--session-id".to_string());
            extra_args.push(sid.clone());
            Some(sid)
        };
        Some(OobSpec::ClaudeTranscript {
            project_dir: working_dir.to_path_buf(),
            pinned_session,
        })
    }

    /// The three `local.*` rows reach a launch only for a tab that opted into
    /// the local provider (V40 review M-4, parity lens).
    ///
    /// They are synthesized into `ANTHROPIC_BASE_URL` / `_AUTH_TOKEN` /
    /// `_MODEL` in `compose_env`, and only there. Before V40 they had no
    /// signature entry of their own — they rode the gated `local_env` element
    /// below — so editing the proxy URL with no local-provider tab open raised
    /// no restart hint. Declaring them `spawn_baked` made core fold them in
    /// unconditionally, which turned a correct silence into a hint for a change
    /// that changes nothing.
    fn spawn_baked_reaches_a_launch(&self, s: &Settings, key: &str) -> bool {
        if !super::settings::LOCAL_KEYS.contains(&key) {
            return true;
        }
        any_local_provider_tab(s)
    }

    fn spawn_sig(&self, s: &Settings) -> serde_json::Value {
        let guidance = crate::harness::plugin::guidance_gates(s);
        let sandbox = crate::harness::plugin::sandbox_gates(s);
        // V35 Phase E: the E1 hard block is now the capability matrix's gate,
        // asked by id (`harness::contract::gate`) instead of a bespoke helper on
        // `HarnessVersions`. Same verdict, same fail-closed semantics.
        let read_hook = s.graph.enabled
            && s.graph.read_advisor
            && !crate::harness::plugin::read_advisor_gate_blocked(s);
        let post_edit = s.graph.enabled && s.graph.auto_check && !s.checks.is_empty();
        // `claude_local` env vars are synthesized at spawn, but only for Claude
        // tabs that opted in — irrelevant edits shouldn't nag.
        let local_env =
            any_local_provider_tab(s).then(|| serde_json::json!(super::settings::local_provider(s)));
        serde_json::json!({
            // V37 Phase F: ONE element, not two. The `cimp-offload` entry is now
            // written into every AI tab's harness config unconditionally, so the
            // element that used to carry `advertises_offload_to_claude` was a
            // constant — and worse, a constant assembled from `any_claude_mcp()`,
            // which is exactly the live-propagating input that phase removed from
            // the spawn-baked set. The audit child IS still gated, so its element
            // stays.
            "mcp": [crate::harness::plugin::audit_advertised(s, me())],
            "guidance": guidance,
            "sandbox": sandbox,
            // V40 Phase B: the `ext` half of the signature is added by
            // `spawn_inject_sig` from the `spawn_baked` column of
            // `settings_schema()` — `statusline` and the three `local.*` rows
            // are in there automatically, so this object no longer names them.
            // `local_env` STAYS, because it is not the raw values: it is
            // `Some(values)` only while a tab actually opted into the local
            // provider, so editing the proxy URL with no such tab open raises
            // no hint.
            // The `--settings` hooks overlay gates, in `build_pre_args` order:
            // UserPromptSubmit, PreCompact, PreToolUse Read, PreToolUse Bash,
            // PreToolUse pre-mutation checkpoint (V33 Phase F), PostToolUse
            // auto-check.
            //
            // V35 Phase J's `SessionStart` hello needs **no slot of its own**: it
            // is emitted whenever any other hook is, and its `serves`/`cannot`
            // declaration is computed from exactly these booleans plus
            // `native_web` (carried by `"injection"` below) and
            // `notify_hooks`/`workbench.checkpoints` (already here).
            //
            // **2026-08-17 changed the SHAPE of three emitted entries and needs
            // no new slot either, which is a fact worth checking rather than
            // assuming**:
            //   * the taint beacon became `type: "http"` — its gate is still
            //     `native_web == Sensor && loopback_needed()`, carried by
            //     `"injection"` + `"notify_hooks"`;
            //   * the pre-mutation checkpoint became `type: "http"` — its gate is
            //     still `workbench.checkpoints && loopback_needed()`, both halves
            //     already here;
            //   * the new `PostToolUseFailure` entry rides `tool_result_hook`
            //     (`graph.enabled && loopback_needed()`), and `graph.enabled`
            //     already moves `"guidance"` and three `hooks` slots.
            "hooks": [
                s.graph.enabled && (s.graph.context_injection || s.workbench.checkpoints),
                s.graph.enabled && s.graph.context_injection && s.graph.compaction_context,
                read_hook,
                read_hook && s.graph.read_advisor_shell,
                // V33 Phase F. Spawn-baked like every other hook entry, so
                // without a slot here toggling `workbench.checkpoints`
                // mid-session would leave every running Claude tab permanently
                // checkpoint-blind (or still checkpointing) with no restart
                // hint.
                s.workbench.checkpoints,
                post_edit,
            ],
            // NC-2 + H2 fix: the `Notification` / `PermissionDenied` pair.
            // Injected whenever the loopback they POST into actually runs, so
            // the value is Settings-derived even though there is no
            // permission-detection toggle of its own.
            "notify_hooks": s.loopback_needed(),
            "local_env": local_env,
            // V30 Phase A: the session-push flag pair — Claude's
            // `--dangerously-load-development-channels` and the `cimp-offload`
            // child's own `--channel-push`. Baked at spawn, so without it
            // toggling `session_push` mid-session leaves every running tab
            // silently unregistered (or registered) with no restart hint.
            "channels": s.offload.session_push,
            // V32 Phase F (locked decision 14) + Phase G (locked decision 16):
            // the native-web visibility mode AND the consumer-hygiene switch,
            // both spawn-baked, both resolved PER TAB through the three-level
            // hierarchy. Live features are deliberately absent: they take effect
            // on the next call, and a restart nag for a change that needs no
            // restart is how a hint stops being read.
            "injection": crate::settings::injection::spawn_sig(s, me()),
        })
    }

    fn settings_schema(&self) -> &'static [crate::harness::plugin::SettingField] {
        super::settings::FIELDS
    }

    /// The two names cImp's own guidance has to spell (locked decision 24).
    /// Claude Code capitalises both; the pre-V40 `GRAPH_GUIDANCE` said `Read`
    /// and `Bash` in core, which is exactly these two values inlined.
    fn tool_for_role(&self, role: crate::harness::plugin::ToolRole) -> Option<&'static str> {
        use crate::harness::plugin::ToolRole;
        Some(match role {
            ToolRole::Read => "Read",
            ToolRole::Shell => "Bash",
        })
    }

    /// The model-visible inventory, rendered once in Claude's vocabulary.
    ///
    /// `OnceLock` per implementation, not per trait: a `static` in a default
    /// trait body would be shared by every harness, which is the one shape this
    /// method must not have.
    fn instructions(&self) -> &[crate::harness::instructions::Instruction] {
        static CELL: std::sync::OnceLock<Vec<crate::harness::instructions::Instruction>> =
            std::sync::OnceLock::new();
        CELL.get_or_init(|| crate::harness::instructions::render_for(me()))
    }

    fn native_tools(&self) -> &'static [NativeTool] {
        super::tools::CLAUDE_NATIVE_TABLE
    }

    /// The fix pointers the drift advisor used to spell in core (V40 Phase C,
    /// locked decision 23).
    ///
    /// Every one of these names a **Claude Code** mechanism — a hook event, a
    /// transcript field, a directory layout. They were sentences inside
    /// `advisor.rs`'s rule bodies, which fire for whichever harness produced
    /// the evidence, so a notice about an OpenCode capability would have told
    /// the reader to check `PreToolUse`.
    fn drift_hint(&self, capability: &str) -> Option<&'static str> {
        match capability {
            "claude.hook.pretooluse_deny" => Some(
                "Check the `PreToolUse` hook wiring per MAINTENANCE.md → \"harness contracts\":                  a `type: \"http\"` entry whose matcher still names the read tools, in an                  overlay this tab was launched with.",
            ),
            "claude.hook.user_prompt_submit" => Some(
                "Check the `UserPromptSubmit` contract per MAINTENANCE.md: the hook's                  `hookSpecificOutput.additionalContext` is what carries the injected block into                  the turn, and a harness that stops honouring it drops the block silently.",
            ),
            "claude.transcript.usage" => Some(
                "The shape to re-check is the transcript's `message.usage` object —                  `input_tokens` / `output_tokens` and the two cache counters.",
            ),
            "claude.transcript.subagents" => Some(
                "Verify the transcript layout per MAINTENANCE.md: sub-agent traffic is expected                  either inline in the parent transcript or under                  `<session_id>/subagents/agent-*.jsonl`, and the launcher tool's name is the                  third thing that can have moved.",
            ),
            _ => None,
        }
    }

    fn memory_arg_keys(&self, arg: crate::harness::plugin::MemArg) -> &'static [&'static str] {
        use crate::harness::plugin::MemArg;
        match arg {
            // `notebook_path` is `NotebookEdit`'s; `path` is not a spelling any
            // documented Claude tool input uses, and it is deliberately absent.
            MemArg::Path => &["file_path", "notebook_path"],
            MemArg::Pattern => &["pattern", "path"],
            MemArg::Command => &["command"],
        }
    }

    fn capabilities(&self) -> &'static [crate::harness::contract::Capability] {
        super::input::CAPABILITIES
    }

    fn canaries(&self) -> &'static [Canary] {
        super::canary::CANARIES
    }

    fn probe(&self) -> ProbeOutput {
        let (mut results, help) = super::probe::probe_claude_flags();
        let (transcript, mut observed, version) = super::probe::probe_claude_transcript();
        results.extend(transcript);
        // Both flag rows are answered from ONE `claude --help`, and each gets
        // its own copy of it. The duplication is deliberate: the file name IS
        // the join key, so a reader who was sent here by a failing
        // `claude.flag.settings_overlay` finds a file with that name rather than
        // a shared blob they have to know the provenance of. It is ~20 KiB
        // twice, bounded by the retention sweep.
        if let Some(help) = help {
            observed.push(crate::harness::capture::Observed::new(
                "claude.flag.session_id",
                "txt",
                help.clone(),
            ));
            observed.push(crate::harness::capture::Observed::new(
                "claude.flag.settings_overlay",
                "txt",
                help,
            ));
        }
        ProbeOutput { results, observed, version }
    }

    fn declared_unprobed(&self) -> &'static [(&'static str, &'static str)] {
        DECLARED_UNPROBED
    }

    fn routes(&self) -> &'static [crate::harness::plugin::Route] {
        super::hook::ROUTES_TABLE
    }

    fn identity_of_request(
        &self,
        route: &str,
        req: &crate::offload::loopback::Request,
    ) -> Option<crate::harness::plugin::RequestIdentity> {
        super::hook::identity_of_request(route, req)
    }

    fn chp_event_for_route(&self, route: &str) -> Option<&'static str> {
        super::hook::chp_event(route)
    }

    fn drift_token_for_capability(&self, capability: &str) -> Option<&'static str> {
        super::hook::drift_token_for_event(capability)
    }

    /// TUI markers, with the timings Claude Code's screen was measured at.
    ///
    /// Claude Code reports no turn boundaries out of band — no hook payload
    /// carries "a turn started" — so the only way to know this tab is busy is
    /// to read the terminal cImp is already painting: the `claude_working`
    /// footer (`esc to interrupt`) while it is on screen, a sustained byte
    /// burst when a response never paints it.
    fn activity_source(&self) -> crate::harness::plugin::ActivitySource {
        crate::harness::plugin::ActivitySource::TuiMarkers(ACTIVITY_TUNING)
    }

    /// The status-line push file — see [`super::usage`].
    fn usage_source(&self) -> Option<&'static dyn crate::harness::plugin::UsageSource> {
        Some(&super::usage::USAGE)
    }

    /// The shape of a recorded turn — see [`super::usage::TURN_SHAPE`]. Declared
    /// separately from the quota source above (V40 Phase G): the four billing
    /// categories and the `session`/`agent` lanes describe a stored
    /// `usage_stat` row, not a status-line push.
    fn turn_usage_shape(&self) -> Option<&'static crate::harness::plugin::TurnUsageShape> {
        Some(&super::usage::TURN_SHAPE)
    }

    /// Claude Code has an inbound MCP path (development channels), which is
    /// what the session-push registration and the `--channel-push` subscription
    /// both gate on.
    /// Claude Code's channel capability lives in its own vendor namespace, so
    /// the key is declared here rather than written by core for whichever
    /// consumer happened to have push armed (locked decision 25).
    fn decorate_initialize(&self, result: &mut serde_json::Value) {
        result["capabilities"]["experimental"] = serde_json::json!({ "claude/channel": {} });
    }

    /// The twin of the capability key above — a push sent under any other
    /// method name is dropped client-side, silently.
    fn push_notification_method(&self) -> Option<&'static str> {
        Some("notifications/claude/channel")
    }

    /// **Pinned to the era where the client honours channels** (V30 milestone
    /// invariant 1). Claude Code stopped honouring `notifications/claude/
    /// channel` outside `2025-06-18`, so this is a compatibility pin and not a
    /// preference — moving it forward silently disables session push.
    fn mcp_protocol_version(&self) -> Option<&'static str> {
        Some("2025-06-18")
    }

    fn supports_session_push(&self) -> bool {
        true
    }

    /// Claude's live session is bound by cImp's own transcript tap and keyed by
    /// the TAB it runs in. Nothing that arrives over the wire keys it — which
    /// is what closes C-2 structurally instead of by a collision check.
    fn session_key_space(&self) -> crate::harness::plugin::SessionKey {
        crate::harness::plugin::SessionKey::Tab
    }

    /// The sub-agent transcript contract (`<sid>/subagents/*.jsonl`) is
    /// Claude's, and so is the drift report `read.rs` files when it moves.
    fn drift_report_tools(&self) -> &'static [&'static str] {
        &["subagent_drift"]
    }

    /// Claude Code's session-selector CLI vocabulary (locked decision 26),
    /// which `args_select_session` matches a tab's stored arguments against
    /// before cImp offers a `--session-id` of its own.
    fn session_selector_flags(&self) -> &'static [&'static str] {
        SESSION_SELECTORS
    }

    /// **Declared `Ok`, not absent** (locked decision 26). Claude Code is the
    /// app's own front end: a cImp with no Claude installed still opens the tab,
    /// where the "command not found" is visible and actionable. Saying so here
    /// is what keeps the exemption from being something a third harness inherits
    /// by accident.
    fn preflight(&self) -> Result<(), &'static str> {
        Ok(())
    }

    /// **Claude Code's half of the window's copy** (locked decision 27).
    ///
    /// Every string is the one the frontend used to hold, moved verbatim: the
    /// `TabErrorOverlay` install hint and its `docs.anthropic.com` link, the
    /// `WebFetch`/`WebSearch` spelling the native-web-visibility copy needs (it
    /// is capitalised here and lower-case in OpenCode, which is why one
    /// spelling could not serve both), the `ANTHROPIC_*` trio the local-provider
    /// preview prints, the `claude` default command, and the two status-bar rows
    /// the 5h/7d pair needs.
    fn affordances(&self) -> crate::harness::plugin::HarnessAffordances {
        use crate::harness::plugin::{HarnessAffordances, LocalProviderVar};
        /// `ANTHROPIC_AUTH_TOKEN` has no `ext_key` on purpose: it is the
        /// credential, and the preview prints `…` rather than the value.
        const LOCAL_PROVIDER: &[LocalProviderVar] = &[
            LocalProviderVar {
                name: "ANTHROPIC_BASE_URL",
                ext_key: Some("local.base_url"),
                only_when_set: false,
            },
            LocalProviderVar {
                name: "ANTHROPIC_AUTH_TOKEN",
                ext_key: None,
                only_when_set: false,
            },
            LocalProviderVar {
                name: "ANTHROPIC_MODEL",
                ext_key: Some("local.model_alias"),
                only_when_set: true,
            },
        ];
        HarnessAffordances {
            new_session_command: Some("/clear"),
            tool_list_refresh: Some("picks it up on its next turn"),
            web_tools: &["WebFetch", "WebSearch"],
            state_dirs: &["~/.claude"],
            install_hint: Some(
                "Make sure Claude Code is installed and on your PATH. Installation instructions:",
            ),
            docs_url: Some("https://docs.anthropic.com/en/docs/claude-code/setup"),
            local_provider: Some(LOCAL_PROVIDER),
            statusline_rows: 2,
            inject_mechanism: Some("a UserPromptSubmit hook"),
            default_command: "claude",
            command_example: Some(r"C:\tools\claude.exe"),
            accent: "var(--text-info, #58a6ff)",
            ..HarnessAffordances::default()
        }
    }

    /// Claude Code's welcome banner cycles a fresh tab `Idle → Thinking → Idle`
    /// as it prints — the transition the notification manager's
    /// "not until this tab has been interacted with" guard exists for.
    fn emits_startup_chrome(&self) -> bool {
        true
    }

    /// The `claude --help` probe's row in the spawn ledger (locked decision 26):
    /// the argv is this product's, so the row lives with the code that runs it.
    fn spawn_sites(&self) -> &'static [crate::spawn_ledger::SpawnSite] {
        super::probe::SPAWN_SITES
    }

    /// `cimp --statusline` — see [`super::statusline`].
    fn subcommands(&self) -> &'static [crate::harness::plugin::Subcommand] {
        super::statusline::SUBCOMMANDS
    }

    fn drift_vocabulary(&self) -> &'static [&'static str] {
        super::hook::DRIFT_TOKENS
    }

    fn hook_reply_timeout(&self) -> Option<std::time::Duration> {
        Some(super::hook::REPLY_TIMEOUT)
    }

    fn permission_patterns(&self) -> &'static [crate::processing::permission::PatternSpec] {
        super::prompts::PATTERNS
    }

    fn patterns_doc_note(&self) -> Option<&'static str> {
        Some(super::prompts::DOC_NOTE)
    }

    fn legacy_permission_patterns(
        &self,
        era: &str,
    ) -> &'static [crate::processing::permission::PatternSpec] {
        super::prompts::legacy_patterns(era)
    }

    fn sandbox_grants(&self, ctx: &GrantCtx) -> Vec<GrantRow> {
        // `CLAUDE_CONFIG_DIR` relocates the state directory; honoring it costs
        // one lookup and its absence would silently confine a tab away from the
        // state it actually uses.
        let claude_dir = (ctx.env)("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.home.join(".claude"));
        vec![
            GrantRow {
                path: claude_dir,
                access: GrantAccess::Full,
                is_file: false,
                reason: "Claude Code's own state — projects, history, sessions, shell \
                         snapshots. Written on every turn; the CLI does not start without it",
                required: false,
            },
            GrantRow {
                path: ctx.home.join(".claude.json"),
                access: GrantAccess::Full,
                is_file: true,
                reason: "Claude Code's top-level config, rewritten in place on most \
                         sessions. A FILE grant, so the home directory around it stays dark",
                required: false,
            },
            GrantRow {
                path: ctx.home.join(".claude.json.backup"),
                access: GrantAccess::Full,
                is_file: true,
                reason: "the backup Claude Code rotates beside its config; same width, same \
                         file-only scope",
                required: false,
            },
            GrantRow {
                // `.local/bin/claude.exe` is a launcher — the install dir grant
                // `prepare` derives from the program path covers only `bin`, and
                // the JS payload lives in a sibling tree.
                path: ctx.xdg("XDG_DATA_HOME", &[".local", "share"]).join("claude"),
                access: GrantAccess::ReadExecute,
                is_file: false,
                reason: "the installed CLI payload (versions/<n>/…), which the launcher in \
                         ~/.local/bin executes. READ-ONLY on purpose: a sandboxed agent that \
                         can rewrite its own program image can persist across the boundary, \
                         so in-tab auto-update is refused rather than allowed",
                required: false,
            },
            GrantRow {
                path: ctx.xdg("XDG_STATE_HOME", &[".local", "state"]).join("claude"),
                access: GrantAccess::Full,
                is_file: false,
                reason: "the CLI's lock/state directory (~/.local/state/claude)",
                required: false,
            },
        ]
    }
}

/// V34: does this arg list already choose which conversation Claude Code runs?
///
/// If so, cImp must not add a `--session-id` of its own — the user's selector
/// names an existing session, and ours would either be rejected outright or
/// silently compete with it. The tab then keeps the pre-V34 newest-wins
/// binding, which is correct-if-ambiguous rather than confidently wrong.
///
/// Matches the `=` spellings too (`--resume=<id>`), and the short forms Claude
/// Code documents (`-c`, `-r`). Erring toward over-matching is deliberate: a
/// false positive costs only the pin, while a false negative hands the child two
/// conflicting session selectors.
///
/// V40 Phase A moved it here from `tabs/config.rs`: it is Claude's CLI
/// vocabulary (locked decision 26), and core cannot own another CLI's flag list.
pub(crate) fn args_select_session(args: &[String]) -> bool {
    // Through the trait, not the `const` directly: the list core can ask for and
    // the list this matcher runs on are then the same object by construction.
    let selectors = PLUGIN.session_selector_flags();
    args.iter().any(|a| {
        let head = a.split_once('=').map_or(a.as_str(), |(k, _)| k);
        selectors.contains(&head)
    })
}

/// The flags themselves, as [`HarnessPlugin::session_selector_flags`] declares
/// them (locked decision 26).
///
/// A `const` beside the matcher rather than inside it: the list is data core may
/// ask for — the trait method is what core reads — while the *matching rule*
/// (`=` spellings, short forms) is this harness's and stays above.
const SESSION_SELECTORS: &[&str] = &[
    "--session-id",
    "--resume",
    "-r",
    "--continue",
    "-c",
    "--fork-session",
    "--from-pr",
];

/// Claude Code's rows that **no probe can settle**, each with the reason
/// (locked decision 17).
///
/// A SEPARATE list from what [`ClaudePlugin::probe`] answers, on purpose: "this
/// needs a scripted model turn" is a claim worth writing down, and a probe that
/// silently stopped emitting a row must not be mistaken for one of these.
const DECLARED_UNPROBED: &[(&str, &str)] = &[
    (
        "claude.hook.user_prompt_submit",
        "needs a scripted turn (L2 residual): proving the stdout envelope reaches the model \
         requires installing a temporary hook via --settings and running one real prompt",
    ),
    (
        "claude.hook.precompact",
        "needs a scripted turn AND spike D0: whether the additionalContext reaches the compaction \
         prompt is a Behavior dep no payload reveals",
    ),
    (
        "claude.hook.pretooluse_deny",
        "needs a scripted turn AND spike E1: whether the deny reason reaches the model is a \
         Behavior dep no payload reveals",
    ),
    (
        "claude.hook.posttooluse",
        "needs a scripted turn (L2 residual): the payload only exists while a real Edit/Write is \
         being made",
    ),
    (
        "claude.hook.notification",
        "needs a scripted turn (L2 residual), and the open question — which of the flat and \
         nested payload shapes this build sends — only answers itself when a real permission \
         prompt fires",
    ),
    // V35 Phase L's three pushed rows. Same answer as their Phase J siblings
    // above and for the same reason: a hook payload exists only while a real
    // turn produces one, so nothing here can be driven without scripting a
    // model. What is NOT deferred with them is their silence — the Phase L
    // quiet detector reports a served capability that stops pushing, in
    // production, on the live wire, which is the half a scripted probe would
    // have been worst at anyway.
    (
        "claude.hook.stop",
        "needs a scripted turn (L2 residual): `last_assistant_message` exists only when a real \
         turn finishes. The open question is a Behavior dep besides — whether its rendering of a \
         multi-block message matches the transcript reader's join",
    ),
    (
        "claude.hook.tool_result",
        "needs a scripted turn (L2 residual): the payload exists only while a real tool call \
         returns, and the property worth proving is that the all-tools matcher fires for tools \
         the sibling entry does not name",
    ),
    (
        "claude.hook.subagent",
        "needs a scripted turn (L2 residual) AND a session that happens to launch a sub-agent — \
         the same 'an absence proves nothing' problem `claude.transcript.subagents` has, one \
         layer up",
    ),
    (
        "claude.transcript.subagents",
        "needs a scripted turn (L2 residual): a transcript tail can only show the subagents/ \
         layout if the tailed session happened to launch a sub-agent, so an absence proves \
         nothing and a presence is luck",
    ),
    (
        "claude.statusline.stdin",
        "needs a scripted turn (L2 residual): the payload exists only when the CLI invokes the \
         statusLine command, so probing it means running a turn with an overlay installed",
    ),
    (
        "claude.hook.taint_beacon",
        "needs a scripted turn (L2 residual): the hook only fires when a real turn reaches for \
         WebFetch/WebSearch, and the property worth proving is that the beacon LANDED before the \
         tool ran — an ordering, not a payload shape. Unchanged by the 2026-08-17 http migration, \
         which moved the row to Tier B: what it bought is app-observable DELIVERY, which is a \
         production signal rather than something this probe can drive",
    ),
    (
        "claude.hook.checkpoint_beacon",
        "needs a scripted turn (L2 residual), and the load-bearing half is an ORDERING no fixture \
         can express: that the tool call does not begin until the hook's response arrives. Since \
         2026-08-17 that ordering is upstream's DOCUMENTED deny contract rather than an observed \
         behaviour, so what a probe would add is confirmation, not coverage",
    ),
    (
        "claude.input.profile",
        "no probe can settle it: whether a bracketed paste plus a submit yields exactly ONE turn          is a `Dep::Behavior` visible only as a real turn in a real TUI. Manual input-profile          spike, outcome in `harness_versions.input_profile_status` — the same class as D0/E1, and          `Mark verified` survives for exactly these",
    ),
    (
        "perm.tui_scrape",
        "no probe can settle it: a scrape of rendered TUI chrome. Re-characterized in minutes \
         with RUST_LOG=perm_capture=debug; the real fix is the D→C→B migration of decision 2",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::plugin::ActivitySource;
    use std::time::Duration;

    /// **The golden for [`ACTIVITY_TUNING`]** (V40 Phase D, locked decision 18).
    ///
    /// These five values were `CLAUDE_BURST_MIN`, `CLAUDE_QUIET`,
    /// `CLAUDE_MARKER_GRACE`, `CLAUDE_WORKING_STALE` (`pty/tasks.rs`) and
    /// `AGENTS_STALL_TIMEOUT` (`state/manager.rs`) before the move. Each was
    /// tuned against an observed avatar defect and none of them is a round
    /// number by accident, so the move is asserted to the millisecond rather
    /// than trusted: a regression here is invisible until an avatar flickers or
    /// announces "idle" mid-turn.
    #[test]
    fn the_activity_tuning_is_the_pre_v40_constants() {
        assert_eq!(ACTIVITY_TUNING.burst_min, Duration::from_millis(1000));
        assert_eq!(ACTIVITY_TUNING.quiet, Duration::from_millis(500));
        assert_eq!(ACTIVITY_TUNING.marker_grace, Duration::from_millis(1200));
        assert_eq!(ACTIVITY_TUNING.working_stale, Duration::from_secs(6));
        assert_eq!(ACTIVITY_TUNING.subagents_stall, Duration::from_secs(8));
    }

    /// The plugin hands core exactly that table — the declaration and the
    /// golden cannot drift apart, because the test that pins the values reads
    /// the same const the trait method returns.
    #[test]
    fn the_declared_source_carries_the_golden_tuning() {
        match PLUGIN.activity_source() {
            ActivitySource::TuiMarkers(t) => assert_eq!(t, ACTIVITY_TUNING),
            ActivitySource::OutOfBand => panic!(
                "Claude Code reports no turn boundaries out of band; declaring OutOfBand                  would leave its avatar permanently Idle"
            ),
        }
    }

    /// The stall backstop must outlast the marker path, or the avatar is
    /// released while the footer is still on screen — the ordering the pre-V40
    /// comment stated in prose beside two constants in different files.
    #[test]
    fn the_stall_backstop_outlasts_the_marker_path() {
        assert!(ACTIVITY_TUNING.subagents_stall > ACTIVITY_TUNING.working_stale);
        assert!(ACTIVITY_TUNING.working_stale > ACTIVITY_TUNING.quiet);
        assert!(ACTIVITY_TUNING.marker_grace > ACTIVITY_TUNING.quiet);
    }


    #[test]
    fn args_select_session_spots_every_documented_selector() {
        for sel in [
            "--session-id",
            "--resume",
            "-r",
            "--continue",
            "-c",
            "--fork-session",
            "--from-pr",
        ] {
            assert!(
                args_select_session(&[sel.to_string()]),
                "{sel} must suppress the pin"
            );
        }
        // `=` spellings count too, long and short.
        assert!(args_select_session(&["--resume=abc123".to_string()]));
        assert!(args_select_session(&["-r=abc123".to_string()]));
        // ...and the selector is found wherever it sits in the list.
        assert!(args_select_session(&[
            "--model".to_string(),
            "opus".to_string(),
            "--continue".to_string(),
        ]));
    }

    #[test]
    fn args_select_session_does_not_over_match_ordinary_flags() {
        // A false positive only costs the pin, but a flag that merely starts
        // with a selector's letters must not silently disable per-tab identity.
        assert!(!args_select_session(&[]));
        assert!(!args_select_session(&[
            "--model".to_string(),
            "opus".to_string()
        ]));
        assert!(!args_select_session(&["--resumable".to_string()]));
        assert!(!args_select_session(&["--continue-on-error".to_string()]));
    }
}
