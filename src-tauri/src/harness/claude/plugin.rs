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
    /// args (`cimp --resume <id>`, etc.) flow into every Claude tab.
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
        let local_env = s
            .tabs
            .iter()
            .any(|t| {
                matches!(t, TabConfig::AiTool(c)
                    if c.use_local_provider && HarnessId::from_command(&c.command) == Some(me()))
            })
            .then(|| serde_json::json!(super::settings::local_provider(s)));
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

    fn native_tools(&self) -> &'static [NativeTool] {
        super::tools::CLAUDE_NATIVE_TABLE
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

    fn drift_vocabulary(&self) -> &'static [&'static str] {
        super::hook::DRIFT_TOKENS
    }

    fn hook_reply_timeout(&self) -> Option<std::time::Duration> {
        Some(super::hook::REPLY_TIMEOUT)
    }

    fn permission_patterns(&self) -> &'static [crate::processing::permission::PatternSpec] {
        super::prompts::PATTERNS
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
    const SELECTORS: [&str; 7] = [
        "--session-id",
        "--resume",
        "-r",
        "--continue",
        "-c",
        "--fork-session",
        "--from-pr",
    ];
    args.iter().any(|a| {
        let head = a.split_once('=').map_or(a.as_str(), |(k, _)| k);
        SELECTORS.contains(&head)
    })
}

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
