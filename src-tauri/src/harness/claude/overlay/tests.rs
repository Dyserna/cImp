//! `harness::claude::overlay`'s unit tests — what a Claude tab is told at
//! spawn, asserted on the artifact [`build_pre_args`] emits.
//!
//! 39 of these arrived from `tabs::config`'s test module in #132's second pass.
//! Every one of them reached the overlay only through `build_pre_args` and
//! touched no `tabs::config` item, which is what made them this file's tests
//! rather than that one's. Tests that drive the same artifact through
//! `build_launch_spec` / `build_ai_tool_spec` / `compose_ai_env` /
//! `spawn_inject_sig`, and every assertion that spans BOTH harnesses' emitters,
//! deliberately stayed with the composition they exercise.

use super::*;
use crate::harness::fixtures::*;

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

#[test]
fn injects_statusline_overlay_for_claude_when_enabled() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(true));
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));

    let overlay = settings_overlay(&args).expect("statusLine overlay present");
    assert_eq!(overlay["statusLine"]["type"], "command");
    // Idle-refresh timer that keeps the usage push (and the bottom-bar
    // widget) alive between turns; must stay under `harness::claude::usage::STALE_AFTER`.
    assert_eq!(overlay["statusLine"]["refreshInterval"], 30);
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
    settings.set_ext("claude", "statusline", serde_json::json!(false));
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    // With the statusline off and no loopback (H2 gated the NC-2 permission
    // hooks on it), the overlay has nothing to carry and no `--settings`
    // flag is emitted at all.
    assert!(settings_overlay(&args).is_none(), "got: {args:?}");
    // With a loopback running the overlay reappears — carrying the hooks,
    // still no statusLine.
    settings.graph.enabled = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let overlay = settings_overlay(&args).expect("overlay present");
    assert!(overlay.get("statusLine").is_none());
    assert!(overlay["hooks"].get("Notification").is_some());
}

/// V35 Phase J: the `UserPromptSubmit` hook is `type: "http"` and points at
/// this instance's loopback — no `cimp --context-hook` process anywhere.
#[test]
fn context_hook_overlay_injected_when_injection_on() {
    let mut settings = Settings::default();
    settings.graph.enabled = true;
    settings.graph.context_injection = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let overlay = settings_overlay(&args).expect("overlay present");
    let entry = hook_entry(&overlay, "UserPromptSubmit", 0);
    assert_eq!(entry["type"], "http");
    assert_eq!(
        entry["url"],
        "http://127.0.0.1:41999/claude/hook/user_prompt_submit"
    );
    assert!(
        entry.get("command").is_none(),
        "the shim is gone; nothing may spawn a process: {entry}"
    );
}

#[test]
fn no_context_hook_when_injection_off() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(false));
    settings.graph.enabled = true;
    settings.graph.context_injection = false;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    // Graph on but injection off + statusline off + checkpoints off →
    // no UserPromptSubmit hook (the overlay itself still carries the
    // unconditional NC-2 permission hooks).
    let overlay = settings_overlay(&args).expect("overlay present");
    assert!(overlay["hooks"].get("UserPromptSubmit").is_none());
    assert!(overlay.get("statusLine").is_none());
}

/// V16 Feature 0: the read-advisor PreToolUse hook installs when the
/// graph + toggle are on and the E1 contract isn't recorded as failed —
/// and a recorded `e1_status == "fail"` hard-blocks it REGARDLESS of
/// the toggle (a deny whose reason never reaches the model is a bare
/// refusal; worse than no advisor).
///
/// V35 Phase E moved the decision behind `harness::contract::gate` and this
/// test did not change a line — which is the point. It pins the gate's
/// fail-closed table END TO END, through the thing that actually installs
/// the hook, and it is deliberately kept here rather than folded into the
/// unit tests next to the gate: those prove the predicate, this proves the
/// overlay the child process is launched with.
#[test]
fn read_hook_overlay_gated_on_toggle_and_e1_status() {
    let mut settings = Settings::default();
    settings.graph.enabled = true;
    settings.graph.read_advisor = true;
    // The read advisor is the only `PreToolUse` producer under test here;
    // V32 Phase F's sensor beacon is a second one, turned off so
    // "no PreToolUse hook" keeps meaning "no read advisor".
    settings.set_native_web_mode_for_test(NativeWebMode::Off);
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let overlay = settings_overlay(&args).expect("overlay present");
    let entry = hook_entry(&overlay, "PreToolUse", 0);
    assert_eq!(entry["type"], "http");
    assert_eq!(
        entry["url"],
        "http://127.0.0.1:41999/claude/hook/pre_tool_use"
    );
    assert_eq!(overlay["hooks"]["PreToolUse"][0]["matcher"], "Read");

    // E1 recorded as failed ⇒ no PreToolUse hook even with the toggle on.
    settings.harness_versions.e1_status = "fail".to_string();
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let overlay = settings_overlay(&args);
    assert!(
        overlay.is_none_or(|o| o["hooks"].get("PreToolUse").is_none()),
        "e1_status=fail must block the read hook"
    );

    // Unverified (the default) does NOT block — Feature 0's posture is
    // opt-in-until-proven-broken, not blocked-until-proven-working.
    settings.harness_versions.e1_status = "unverified".to_string();
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    assert!(settings_overlay(&args).is_some_and(|o| o["hooks"]["PreToolUse"].is_array()));

    // The statuses are hand-editable strings; anything unrecognized
    // fails CLOSED (a typo'd failure record must not install the hook).
    for status in ["Fail", " fail ", "failed", "faill"] {
        settings.harness_versions.e1_status = status.to_string();
        let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
        let overlay = settings_overlay(&args);
        assert!(
            overlay.is_none_or(|o| o["hooks"].get("PreToolUse").is_none()),
            "unrecognized e1_status {status:?} must fail closed"
        );
    }
    // Recognized non-fail spellings still pass, case-folded.
    settings.harness_versions.e1_status = "Pass".to_string();
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    assert!(settings_overlay(&args).is_some_and(|o| o["hooks"]["PreToolUse"].is_array()));
}

/// V17 Phase B: the second `PreToolUse` **Bash** matcher (whole-file shell
/// read interception) is present exactly when every gate holds —
/// `read_advisor` AND `read_advisor_shell` AND E1 not failed. The `Read`
/// matcher tracks `read_advisor` + E1 alone (the sub-toggle never affects
/// it), and the sub-toggle being off is a zero overlay delta for the Bash
/// side.
#[test]
fn shell_read_bash_matcher_gated_on_full_matrix() {
    // Whether the overlay carries a PreToolUse entry for `matcher`.
    fn has_matcher(read_advisor: bool, shell: bool, e1: &str, matcher: &str) -> bool {
        let mut settings = Settings::default();
        settings.graph.enabled = true;
        settings.graph.read_advisor = read_advisor;
        settings.graph.read_advisor_shell = shell;
        settings.harness_versions.e1_status = e1.to_string();
        let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
        settings_overlay(&args)
            .and_then(|o| o["hooks"]["PreToolUse"].as_array().cloned())
            .is_some_and(|arr| arr.iter().any(|e| e["matcher"] == matcher))
    }

    for &read_advisor in &[false, true] {
        for &shell in &[false, true] {
            for e1 in ["unverified", "pass", "fail"] {
                let e1_ok = e1 != "fail";
                let read_present = read_advisor && e1_ok;
                let bash_present = read_advisor && shell && e1_ok;
                assert_eq!(
                    has_matcher(read_advisor, shell, e1, "Read"),
                    read_present,
                    "Read matcher: read_advisor={read_advisor} shell={shell} e1={e1}"
                );
                assert_eq!(
                    has_matcher(read_advisor, shell, e1, "Bash"),
                    bash_present,
                    "Bash matcher: read_advisor={read_advisor} shell={shell} e1={e1}"
                );
            }
        }
    }
}

/// V13 Phase C: the UserPromptSubmit hook (the prompt-tap checkpoint
/// trigger's transport) must still install when `workbench.checkpoints`
/// is on, even with context injection off — the milestone's Decision 4.
#[test]
fn context_hook_overlay_installed_when_checkpoints_on_even_if_injection_off() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(false));
    settings.graph.enabled = true;
    settings.graph.context_injection = false;
    settings.workbench.checkpoints = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let overlay = settings_overlay(&args).expect("overlay present");
    assert_eq!(
        hook_entry(&overlay, "UserPromptSubmit", 0)["url"],
        "http://127.0.0.1:41999/claude/hook/user_prompt_submit"
    );
    // PreCompact stays off — it's still gated on context_injection alone.
    assert!(overlay["hooks"].get("PreCompact").is_none());
}

/// V33: the `UserPromptSubmit` hook carries the cImp TAB it serves, so the
/// prompt-tap checkpoint it fires can be attributed to one tab rather than
/// to "some Claude tab on this root".
///
/// The hook PAYLOAD carries no tab identity, so the emitted entry is the
/// only channel — the same conclusion `--taint-beacon` and the per-tab MCP
/// children reached. **V35 Phase J moved it from argv (` --tab <id>`) to the
/// `X-CIMP-Tab` header**, because an http hook has no argv; the fact it
/// encodes is identical.
///
/// **What it would still pass with:** a build that emitted a constant tab
/// id for every tab — hence the loop over two different ids and the
/// assertion that the emitted entries DIFFER, which is the property the
/// whole step exists for.
#[test]
fn the_context_hook_carries_its_own_tab_id() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(false));
    settings.graph.enabled = true;
    settings.graph.context_injection = true;
    let entry = |tab: &str| {
        let args = build_pre_args(&claude_cfg(), &settings, tab, Some(&hook_endpoint()));
        let overlay = settings_overlay(&args).expect("overlay present");
        hook_entry(&overlay, "UserPromptSubmit", 0)
    };
    for tab in ["claude", "claude-local"] {
        let e = entry(tab);
        assert_eq!(e["headers"]["X-CIMP-Tab"], tab, "got: {e}");
        assert_eq!(e["headers"]["X-CIMP-Agent"], "claude", "got: {e}");
    }
    assert_ne!(
        entry("claude"),
        entry("claude-local"),
        "two tabs must not post an identical hook entry"
    );
}

/// Checkpoints alone (graph off) must NOT install the hook — the
/// milestone's widened condition still requires `graph.enabled` (the
/// hook's own gate prefix is unchanged, only the injection/checkpoints
/// half was widened).
#[test]
fn no_context_hook_when_checkpoints_on_but_graph_disabled() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(false));
    settings.graph.enabled = false;
    settings.workbench.checkpoints = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    // Graph off ⇒ no loopback either, so the overlay is empty and omitted
    // entirely (H2). Assert through the option so the test keeps meaning
    // "no UserPromptSubmit hook" in both shapes.
    let hooks = settings_overlay(&args).map(|o| o["hooks"].clone());
    assert!(
        hooks
            .as_ref()
            .is_none_or(|h| h.get("UserPromptSubmit").is_none()),
        "got: {hooks:?}"
    );
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
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let overlay = settings_overlay(&args).expect("overlay present");
    assert_eq!(overlay["hooks"]["PostToolUse"][0]["matcher"], "Edit|Write|MultiEdit");
    assert_eq!(
        hook_entry(&overlay, "PostToolUse", 0)["url"],
        "http://127.0.0.1:41999/claude/hook/post_tool_use"
    );
}

/// #48 (M-7): **every** hook whose loopback route resolves a taint scope
/// carries the cImp TAB it serves.
///
/// `--context-hook` already did (V33). `--precompact-hook`, `--read-hook`
/// and `--postedit-hook` did not, which is why `/context/compaction`,
/// `/context/should_read` and `/context/post_edit` had no identity to gate
/// against — the second half of the finding. A hook payload names no cImp
/// tab (the E2 spike), so the emitted entry is the only channel.
///
/// **V35 Phase J: the channel is `X-CIMP-Tab`, not ` --tab <id>`.** Four of
/// the routes below are now the app's own; the identity they carry, and the
/// gate that consumes it, are unchanged. The token and the CHP version ride
/// the same headers and are asserted here too, because a hook that reaches
/// the loopback without the token is a silent 401 on every call — the exact
/// class of failure this test exists to make loud.
///
/// **What this would still pass with:** a build that baked one constant id
/// into every tab's entries — hence the two ids and the inequality
/// assertion, the same guard `the_context_hook_carries_its_own_tab_id` uses.
/// And a build that wired only SOME of the four routes — hence all four in
/// one loop rather than one assertion per test.
#[test]
fn every_context_hook_carries_the_tab_its_route_gates_on() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(false));
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
    // Keep the sensor beacon out of `PreToolUse` so the entries below are
    // the read advisor's two matchers and nothing else.
    settings.set_native_web_mode_for_test(NativeWebMode::Off);

    // Every hook object the overlay installs, flattened across events and
    // matchers — so a hook that stops being installed at all fails the
    // lookup below rather than silently passing the loop.
    let entries = |tab: &str| -> Vec<serde_json::Value> {
        let args = build_pre_args(&claude_cfg(), &settings, tab, Some(&hook_endpoint()));
        let overlay = settings_overlay(&args).expect("overlay present");
        let hooks = overlay["hooks"].clone();
        let mut out = Vec::new();
        for event in [
            "UserPromptSubmit",
            "PreCompact",
            "PreToolUse",
            "PostToolUse",
        ] {
            for entry in hooks[event].as_array().cloned().unwrap_or_default() {
                for h in entry["hooks"].as_array().cloned().unwrap_or_default() {
                    out.push(h);
                }
            }
        }
        out
    };

    for tab in ["claude", "claude-local"] {
        let all = entries(tab);
        for route in [
            claude_hook::ROUTE_USER_PROMPT_SUBMIT,
            claude_hook::ROUTE_PRE_COMPACT,
            claude_hook::ROUTE_PRE_TOOL_USE,
            claude_hook::ROUTE_POST_TOOL_USE,
        ] {
            let hits: Vec<&serde_json::Value> = all
                .iter()
                .filter(|h| h["url"].as_str().is_some_and(|u| u.ends_with(route)))
                .collect();
            assert!(!hits.is_empty(), "{route} is not installed at all: {all:?}");
            for h in hits {
                assert_eq!(h["headers"]["X-CIMP-Tab"], tab, "{route} must carry its tab");
                assert_eq!(
                    h["headers"]["Authorization"], "Bearer $CIMP_HOOK_TOKEN",
                    "{route} must carry the token or every call is a silent 401"
                );
                assert_eq!(
                    h["allowedEnvVars"],
                    serde_json::json!(["CIMP_HOOK_TOKEN"]),
                    "{route}: an env var not listed here substitutes to the empty string"
                );
                assert_eq!(
                    h["headers"]["X-CIMP-Chp"],
                    crate::harness::chp::CHP_VERSION.to_string()
                );
            }
        }
    }
    assert_ne!(
        entries("claude"),
        entries("claude-local"),
        "two tabs must not post identical hook entries"
    );
}

#[test]
fn no_postedit_hook_when_auto_check_off_or_no_checks_configured() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(false));
    settings.graph.enabled = true;
    settings.graph.auto_check = false;
    settings.checks = vec![crate::checks::CheckDef::default()];
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    // auto_check off → no auto-check entry. `PostToolUse` itself is no
    // longer empty, because V35 Phase L put a SECOND, independently gated
    // entry on the same event (the all-tools tool-result push), so the
    // assertion is about the MATCHER GROUP rather than about the key —
    // which is the sharper claim anyway: what must not exist is a group
    // that runs the project's checks.
    let overlay = settings_overlay(&args).expect("overlay present");
    assert!(!post_tool_use_has_auto_check(&overlay), "{}", overlay["hooks"]);

    let mut settings2 = Settings::default();
    settings2.set_ext("claude", "statusline", serde_json::json!(false));
    settings2.graph.enabled = true;
    settings2.graph.auto_check = true;
    settings2.checks = Vec::new();
    let args2 = build_pre_args(&claude_cfg(), &settings2, "claude", Some(&hook_endpoint()));
    let overlay2 = settings_overlay(&args2).expect("overlay present");
    assert!(!post_tool_use_has_auto_check(&overlay2), "{}", overlay2["hooks"]);
}

/// **The two `PostToolUse` groups are two routes, and that is what stops the
/// auto-check running twice** (V35 Phase L).
///
/// Claude evaluates every matching group, so an `Edit` fires BOTH. Sharing
/// one route would therefore execute the project's configured checks twice
/// per edit and count one tool result twice — the two double-delivery
/// failures this phase is most exposed to. This is the assertion that keeps
/// a later "simplify the matcher" from reintroducing them.
#[test]
fn the_two_post_tool_use_groups_never_share_a_route() {
    let (hooks, _http) = maxed_overlay();
    let groups = hooks["PostToolUse"]
        .as_array()
        .expect("both PostToolUse groups")
        .clone();
    assert_eq!(groups.len(), 2, "{hooks}");
    let by_matcher: Vec<(String, String)> = groups
        .iter()
        .map(|g| {
            (
                g["matcher"].as_str().unwrap_or_default().to_string(),
                g["hooks"][0]["url"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let auto = by_matcher
        .iter()
        .find(|(m, _)| m == "Edit|Write|MultiEdit")
        .expect("the auto-check group keeps its exact matcher");
    let result = by_matcher
        .iter()
        .find(|(m, _)| m.is_empty())
        .expect("the tool-result group takes every tool");
    assert!(auto.1.ends_with(claude_hook::ROUTE_POST_TOOL_USE));
    assert!(result.1.ends_with(claude_hook::ROUTE_POST_TOOL_USE_RESULT));
    assert_ne!(
        auto.1, result.1,
        "one shared route would run the auto-check twice on every Edit"
    );
}

/// The sub-agent pair rides ONE route, like the notification pair — so the
/// lifecycle's two halves cannot be served by handlers that disagree.
#[test]
fn the_subagent_pair_shares_one_route_like_the_notification_pair() {
    let (hooks, _http) = maxed_overlay();
    for event in ["SubagentStart", "SubagentStop"] {
        let group = &hooks[event][0];
        assert_eq!(group["matcher"], "", "{event} must take every agent type");
        assert!(group["hooks"][0]["url"]
            .as_str()
            .expect("url")
            .ends_with(claude_hook::ROUTE_SUBAGENT));
    }
    assert!(hooks["Stop"][0]["hooks"][0]["url"]
        .as_str()
        .expect("url")
        .ends_with(claude_hook::ROUTE_STOP));
    // `MessageDisplay` is the cadence trap (locked decision 2): it fires per
    // streaming chunk with a 10 s default timeout. A future edit that wires
    // it would hand the sentence segmenter token deltas where it is fed
    // complete text today, silently changing what TTS says.
    assert!(
        hooks.get("MessageDisplay").is_none(),
        "MessageDisplay must never be wired — see claude_hook::ROUTE_STOP"
    );
}

/// NC-2 (issue #5) + H2 (2026-08-05 review): the `Notification` +
/// `PermissionDenied` hooks are injected for a Claude tab exactly when the
/// loopback they POST into runs — from the barest settings that flip
/// `loopback_needed()` and nothing else. Both point at the one
/// notification route with the documented match-everything
/// `"matcher": ""` (a narrowing matcher filters on notification TYPE; we
/// classify app-side so a renamed type degrades to "ignored", not silence).
#[test]
fn permission_hooks_injected_for_claude_when_the_loopback_runs() {
    // Barest settings that start the loopback: graph on, everything else
    // (statusline, injection, advisors, auto-check) off.
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(false));
    settings.graph.enabled = true;
    settings.graph.context_injection = false;
    settings.workbench.checkpoints = false;
    settings.graph.read_advisor = false;
    settings.graph.auto_check = false;
    assert!(settings.loopback_needed());
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    // The Claude Code `--settings` contract: ONE flag, one merged overlay —
    // the hooks must ride the same object as everything else, never a
    // second flag (Claude does not concatenate repeated `--settings`).
    assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
    let overlay = settings_overlay(&args).expect("overlay present");
    for event in ["Notification", "PermissionDenied"] {
        let entry = &overlay["hooks"][event][0];
        assert_eq!(
            entry["matcher"], "",
            "{event} must match every type/tool: {entry}"
        );
        assert_eq!(
            entry["hooks"][0]["url"],
            "http://127.0.0.1:41999/claude/hook/notification",
            "both events reach the ONE route that dispatches on hook_event_name"
        );
    }

    // Non-Claude tabs get no pre-args at all (OpenCode is configured via
    // OPENCODE_CONFIG_CONTENT), so nothing leaks there.
    assert!(build_pre_args(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint())).is_empty());
}

/// H2: on a DEFAULT install nothing dials back into the app, so the hooks
/// must NOT be injected — a shim spawn per notification whose POST is
/// dropped is worse than no hook at all (the regex fallback still runs).
#[test]
fn no_permission_hooks_when_the_loopback_does_not_run() {
    let settings = Settings::default(); // offload + graph + audit all off
    assert!(!settings.loopback_needed());
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    // Statusline defaults on, so the overlay exists — it just must carry no
    // hooks at all (and if a future default drops the statusline too, the
    // absent overlay satisfies the same claim).
    if let Some(overlay) = settings_overlay(&args) {
        assert!(
            overlay.get("hooks").is_none(),
            "no loopback ⇒ no hook entries: {overlay}"
        );
    }
}

#[test]
fn statusline_and_context_hook_share_one_overlay() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(true));
    settings.graph.enabled = true;
    settings.graph.context_injection = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    // Exactly one `--settings` flag carrying both keys.
    assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
    let overlay = settings_overlay(&args).expect("overlay present");
    assert!(overlay.get("statusLine").is_some());
    assert!(overlay.get("hooks").is_some());
}

/// CD-4 (maintenance 2026-08-04) — the Claude Code `--settings` contract.
/// Two guarantees, asserted against the largest overlay we can emit:
///
///   * **No PATH permission rules, no plugins.** Claude Code 2.1.214
///     narrowed single-segment permission globs (`Edit(src/**)` now matches
///     only `<cwd>/src` depth) and deprecated the `Write(path)` /
///     `Glob(path)` / `NotebookEdit(path)` rule forms in favor of
///     `Edit(path)` / `Read(path)`; plugins delivered through `--settings`
///     were broken in 2.1.181–2.1.214. cImp's overlay carries no plugins at
///     all, and — since V32 Phase F — exactly one kind of permission rule:
///     the bare tool names `WebFetch`/`WebSearch` under `permissions.deny`,
///     in `deny` mode only. A bare name carries no path segment, so the
///     glob-narrowing and the deprecated rule forms do not reach it.
///     Pinning the key set keeps the negative durable: any further
///     `permissions` content (paths, `allow`, `ask`) or a `plugins` key has
///     to come past this note. The `deny`-mode shape is asserted separately
///     by `deny_mode_permission_denies_the_native_web_tools`.
///   * **Size.** Settings over 2 MiB hard-fail at startup (2.1.214). The
///     overlay is bounded by construction — fixed-shape JSON whose only
///     variable part is this binary's own path, repeated once per hook
///     command — and no user-supplied JSON is ever merged into it (the
///     `.cimp.custom.config.json` overlay is cImp's *own* settings layer
///     and never reaches Claude). A static ceiling is therefore enough;
///     there is nothing unbounded to re-check at spawn time.
#[test]
fn settings_overlay_matches_claude_settings_contract() {
    let mut settings = Settings::default();
    // Every overlay-producing gate on at once — the biggest overlay
    // `build_pre_args` can build.
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
    let i = args
        .iter()
        .position(|a| a == "--settings")
        .expect("overlay present");
    let raw = &args[i + 1];
    let overlay: serde_json::Value =
        serde_json::from_str(raw).expect("--settings value is valid JSON");

    // Sanity: this really is the maxed-out overlay, not a degenerate one.
    let hooks = overlay["hooks"].as_object().expect("hooks object");
    for k in [
        "UserPromptSubmit",
        "PreCompact",
        "PreToolUse",
        "PostToolUse",
        // NC-2 — unconditional, so present in every overlay.
        "Notification",
        "PermissionDenied",
        // V35 Phase J: Claude's CHP hello.
        "SessionStart",
    ] {
        assert!(hooks.contains_key(k), "expected hook {k} in {overlay}");
    }
    assert_eq!(
        overlay["hooks"]["PreToolUse"].as_array().map(Vec::len),
        Some(4),
        "Read + Bash read-advisor matchers, the V32 Phase F web beacon, and \
             the V33 Phase F pre-mutation checkpoint beacon",
    );
    // …and the three producers really are three DISTINCT matchers, not one
    // entry duplicated: Claude evaluates every matching entry, so an
    // accidental overlap would fire two hooks per call.
    let matchers: Vec<&str> = overlay["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse array")
        .iter()
        .filter_map(|e| e["matcher"].as_str())
        .collect();
    assert_eq!(
        matchers,
        vec![
            "Read",
            "Bash",
            CLAUDE_WEB_TOOL_MATCHER,
            CLAUDE_MUTATING_TOOL_MATCHER
        ],
        "got: {overlay}"
    );

    // The whole overlay is exactly these two keys.
    let mut keys: Vec<&str> = overlay
        .as_object()
        .expect("overlay is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["hooks", "statusLine"],
        "unexpected `--settings` key — see the permission-glob / plugin \
             contract notes on this test before adding one",
    );

    // Ceiling: ~170x headroom over the real maxed-out overlay (measured
    // 1135 bytes before NC-2 added two more hook commands) and 8x below
    // Claude Code's 2 MiB hard-fail.
    const MAX_OVERLAY_BYTES: usize = 256 * 1024;
    assert!(
        raw.len() < MAX_OVERLAY_BYTES,
        "overlay is {} bytes, ceiling is {MAX_OVERLAY_BYTES}",
        raw.len(),
    );
}

/// **Every emitted hook is `type: "http"` and carries an explicit, pinned
/// `timeout` — the one its own route declares.**
///
/// Design § 5.2: the five shims budgeted 600 ms for their loopback round
/// trip *"so a slow/cold index never delays the prompt"*, and with the shim
/// gone that budget is the whole of it rather than a ceiling over a process
/// that gave up first. The harness defaults are 600 s (most events), 30 s
/// (`UserPromptSubmit`) and 10 s (`MessageDisplay`) — inheriting any of them
/// turns a wedged handler into a wedged turn, and the old value survived
/// only as a comment. This is the test that makes a hand edit or a template
/// drift fail the build.
///
/// **2026-08-17: the numbers come from `claude_hook::timeout_secs(route)`**,
/// not from one constant, because the pre-mutation checkpoint entry is a
/// ceiling over a wait that is supposed to happen (its handler holds the tool
/// call while the snapshot is taken) rather than a round-trip budget. The
/// assertion is still "the value the generator pinned for THIS route", which
/// is what a hand edit breaks; `hook.rs`'s own test is what stops the
/// exception from spreading.
#[test]
fn every_emitted_hook_is_http_and_pins_its_routes_budget() {
    let (hooks, http) = maxed_overlay();
    assert_eq!(
        http.len(),
        15,
        "the five converted hooks — the read advisor is TWO entries (Read + Bash) \
             on one route and Notification/PermissionDenied are two more on one route \
             — plus SessionStart, plus V35 Phase L's four (Stop, the all-tools \
             PostToolUse result entry, and SubagentStart/SubagentStop on one route), \
             plus 2026-08-17's three (PostToolUseFailure and the two migrated \
             beacons): {hooks}"
    );
    // No COMMAND hook survives anywhere in the overlay: the two beacons were
    // the last of them, and `statusLine` is the only `type: "command"` cImp
    // still emits (a different key entirely, asserted below).
    assert!(
        !hooks.to_string().contains("\"command\""),
        "a Claude hook is never a command any more: {hooks}"
    );
    for h in &http {
        let url = h["url"].as_str().unwrap_or_default();
        let route = url
            .strip_prefix("http://127.0.0.1:41999")
            .unwrap_or_else(|| panic!("an http hook must point at THIS instance's loopback: {h}"));
        assert!(
            claude_hook::is_hook_route(route),
            "an emitted entry points at a route `claude_hook` does not declare: {h}"
        );
        assert_eq!(
            h["timeout"],
            serde_json::json!(claude_hook::timeout_secs(route)),
            "an http hook without the budget its route pins: {h}"
        );
        assert!(
            h["timeout"].is_u64(),
            "the timeout must be an integer number of seconds: {h}"
        );
        assert_eq!(h["allowedEnvVars"], serde_json::json!(["CIMP_HOOK_TOKEN"]));
        assert_eq!(h["headers"]["Authorization"], "Bearer $CIMP_HOOK_TOKEN");
    }
    // …and the exception is exactly one entry, so "pinned at 1 s" stays the
    // rule rather than becoming a range.
    let long: Vec<&serde_json::Value> = http
        .iter()
        .filter(|h| h["timeout"] != serde_json::json!(claude_hook::TIMEOUT_SECS))
        .collect();
    assert_eq!(long.len(), 1, "exactly one entry may deviate: {long:?}");
    assert_eq!(long[0]["timeout"], 5);
    assert!(
        long[0]["url"]
            .as_str()
            .is_some_and(|u| u.ends_with(claude_hook::ROUTE_PRE_TOOL_USE_CHECKPOINT)),
        "and it is the pre-mutation checkpoint: {:?}",
        long[0]
    );
}

/// **`terminalSequence` is never emitted**, by the overlay or by any handler
/// that answers one of these routes.
///
/// It is a hook-output field that writes escape sequences straight into the
/// PTY cImp renders (design § 5.2). It is not a CHP capability and cImp has
/// no use for it; a test is cheaper than a convention nobody remembers.
#[test]
fn no_emitted_hook_or_handler_ever_produces_a_terminal_sequence() {
    let (hooks, _) = maxed_overlay();
    assert!(
        !hooks.to_string().contains("terminalSequence"),
        "the overlay must never mention it: {hooks}"
    );
    // V42 R2 (#114) split the loopback module and R4 (#115) split its route
    // surface again; the scan follows every file both produced, or the
    // needle could just move next door.
    for (file, src) in [
        ("harness/claude/hook.rs", include_str!("../../../harness/claude/hook.rs")),
        ("offload/discovery.rs", include_str!("../../../offload/discovery.rs")),
        ("offload/latch.rs", include_str!("../../../offload/latch.rs")),
    ]
    .into_iter()
    .chain(crate::offload::loopback::ROUTE_SOURCES.iter().copied())
    {
        // The needle is the JSON KEY form, so the prose and the assertions
        // that name the field (including this one) are not false positives —
        // what is forbidden is writing it into an emitted object.
        assert!(
            !src.contains("\"terminalSequence\":"),
            "{file} emits `terminalSequence`, which writes escape sequences \
                 into the terminal cImp renders"
        );
    }
}

/// **Claude's CHP hello**: the `SessionStart` entry carries a declaration
/// computed from the very booleans that decided what to emit, and every
/// Claude-servable event lands on exactly one side of it.
#[test]
fn the_session_start_hello_declares_what_the_overlay_actually_wired() {
    use crate::harness::chp;
    let (hooks, _) = maxed_overlay();
    let raw = hooks["SessionStart"][0]["hooks"][0]["headers"]["X-CIMP-Hello"]
        .as_str()
        .expect("the hello header");
    let hello = claude_hook::Hello::parse(raw).expect("a parseable declaration");
    // Everything on: nothing may be in `cannot`.
    assert!(hello.cannot.is_empty(), "got {:?}", hello.cannot);
    for id in [
        chp::EV_HELLO,
        chp::EV_PROMPT,
        chp::EV_CONTEXT_COMPACTION,
        chp::EV_CONTEXT_SHOULD_READ,
        chp::EV_CONTEXT_POST_EDIT,
        chp::EV_PERMISSION_EVENT,
        chp::EV_CHECKPOINT_PRE_MUTATION,
        chp::EV_CONTRACT_DRIFT,
    ] {
        assert!(hello.serves.contains(&id.to_string()), "missing {id}");
    }
    // **A declared capability has an ENTRY behind it.** The two beacons are
    // the pair this most matters for since 2026-08-17: they were declared
    // from booleans re-spelled beside the hello while the emission sites had
    // their own copies, which is exactly how an artifact and its own hello
    // come to disagree. One binding each now, and this is the check.
    let routes: Vec<String> = hooks
        .as_object()
        .expect("hooks object")
        .values()
        .flat_map(|entries| entries.as_array().cloned().unwrap_or_default())
        .flat_map(|e| e["hooks"].as_array().cloned().unwrap_or_default())
        .filter_map(|h| h["url"].as_str().map(str::to_string))
        .collect();
    let wired = |route: &str| routes.iter().any(|u| u.ends_with(route));
    for (id, route) in [
        (chp::EV_TAINT_BEACON, claude_hook::ROUTE_PRE_TOOL_USE_TAINT),
        (
            chp::EV_CHECKPOINT_PRE_MUTATION,
            claude_hook::ROUTE_PRE_TOOL_USE_CHECKPOINT,
        ),
        (
            chp::EV_SESSION_TOOL_RESULT,
            claude_hook::ROUTE_POST_TOOL_USE_RESULT,
        ),
    ] {
        assert_eq!(
            hello.serves.contains(&id.to_string()),
            wired(route),
            "`{id}` is declared iff its entry (`{route}`) was emitted"
        );
    }
    // …and the failure half rides the SUCCESS half's declaration, because it
    // is the same capability. Emitted together or not at all: one without the
    // other would either lose failed results or count them twice.
    assert_eq!(
        wired(claude_hook::ROUTE_POST_TOOL_USE_FAILURE),
        wired(claude_hook::ROUTE_POST_TOOL_USE_RESULT),
        "the tool-result pair must be emitted together"
    );
    assert!(
        !hello
            .serves
            .iter()
            .chain(hello.cannot.iter().map(|u| &u.id))
            .any(|id| id.contains("tool_error") || id.contains("tool_failure")),
        "the failure half declares no id of its own — see `claude_hook::chp_event`"
    );
    // …and the one thing a maxed overlay still cannot serve, because the
    // native-web mode defaults to `sensor` only when it is set to it.
    let mut off = Settings::default();
    off.set_ext("claude", "statusline", serde_json::json!(false));
    off.graph.enabled = true;
    off.set_native_web_mode_for_test(NativeWebMode::Off);
    let args = build_pre_args(&claude_cfg(), &off, "claude", Some(&hook_endpoint()));
    let overlay = settings_overlay(&args).expect("overlay present");
    let hello = claude_hook::Hello::parse(
        overlay["hooks"]["SessionStart"][0]["hooks"][0]["headers"]["X-CIMP-Hello"]
            .as_str()
            .expect("the hello header"),
    )
    .expect("a parseable declaration");
    for id in [
        chp::EV_PROMPT,
        chp::EV_CONTEXT_COMPACTION,
        chp::EV_CONTEXT_SHOULD_READ,
        chp::EV_CONTEXT_POST_EDIT,
        chp::EV_TAINT_BEACON,
        chp::EV_CHECKPOINT_PRE_MUTATION,
    ] {
        let entry = hello.cannot.iter().find(|u| u.id == id);
        let entry = entry.unwrap_or_else(|| panic!("`{id}` is neither served nor explained"));
        assert!(
            entry.why.len() > 20,
            "`{id}` must say WHY it is unavailable, got {:?}",
            entry.why
        );
    }
    // serves ∪ cannot is a partition — no id may appear on both sides.
    for u in &hello.cannot {
        assert!(!hello.serves.contains(&u.id), "`{}` is on both sides", u.id);
    }
    // The declaration is header-safe: no CR/LF can reach the wire.
    assert!(!raw.contains('\n') && !raw.contains('\r'));
}

/// With no loopback endpoint, NO http hook is emitted — an http hook has a
/// baked URL and there is nothing to point it at.
///
/// Stated as a test rather than left implicit because it is a real behaviour
/// change from the command-hook era: a command hook installed before the
/// loopback existed would find it later through discovery. Every gate that
/// reaches this point implies `loopback_needed()`, so the endpoint is
/// present at any real spawn; the residual is the window before the listener
/// binds.
///
/// **Since 2026-08-17 that covers the two beacons too**, and it is the one
/// behavioural consequence of migrating them worth pinning: as command hooks
/// they resolved the endpoint themselves at run time, so they were installed
/// regardless. Now they are not installed at all without one — which is the
/// same trade every other entry already made, and strictly better than a hook
/// that spawns a process per web call to POST into a closed socket.
#[test]
fn no_endpoint_means_no_http_hooks_at_all() {
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(true));
    settings.graph.enabled = true;
    settings.graph.context_injection = true;
    settings.workbench.checkpoints = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", None);
    let overlay = settings_overlay(&args).expect("statusLine keeps the overlay alive");
    assert!(overlay.get("statusLine").is_some());
    assert!(
        overlay.get("hooks").is_none(),
        "no endpoint ⇒ no hooks of any kind, including the two beacons: {overlay}"
    );
}

#[test]
fn statusline_overlay_is_claude_only() {
    // OpenCode understands neither --append-system-prompt nor --settings
    // (its config arrives via OPENCODE_CONFIG_CONTENT), so its pre-args stay
    // empty even with the global toggle on.
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(true));
    let args = build_pre_args(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint()));
    assert!(
        args.is_empty(),
        "opencode must get no pre-args, got: {args:?}"
    );
}

#[test]
fn guidance_and_statusline_coexist() {
    // V20: TTS markup is no longer injected, but capability guidance
    // (graph/offload) still feeds --append-system-prompt; with the status
    // line also on, both pre-arg pairs are present.
    let mut settings = Settings::default();
    settings.set_ext("claude", "statusline", serde_json::json!(true));
    settings.graph.enabled = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));

    assert!(args.iter().any(|a| a == "--append-system-prompt"));
    assert!(args.iter().any(|a| a == "--settings"));
}

/// Sensor mode injects a `PreToolUse` beacon matched ONLY on the two web
/// tools — the narrowness is the point (no per-call tax on Read/Grep/Bash)
/// — with the tab id in `X-CIMP-Tab`, since a hook payload carries none.
/// `off` and `deny` inject no hook at all.
///
/// **`type: "http"` since 2026-08-17** (the tab id was `--tab` in argv, on a
/// `cimp --taint-beacon` command). What this test pins is unchanged in
/// substance: the matcher is narrow, the identity is baked, and the two other
/// modes inject nothing.
#[test]
fn sensor_mode_injects_a_web_only_beacon_hook() {
    let pre_tool_use = |mode: &str| -> Vec<serde_json::Value> {
        let mut s = Settings::default();
        s.graph.enabled = true; // the loopback the beacon POSTs into
        s.set_native_web_mode_for_test(NativeWebMode::parse(mode));
        let args = build_pre_args(&claude_cfg(), &s, "claude-2", Some(&hook_endpoint()));
        settings_overlay(&args)
            .and_then(|o| o["hooks"]["PreToolUse"].as_array().cloned())
            .unwrap_or_default()
    };

    let sensor = pre_tool_use("sensor");
    let beacon = sensor
        .iter()
        .find(|e| e["matcher"] == CLAUDE_WEB_TOOL_MATCHER)
        .unwrap_or_else(|| panic!("sensor must install the beacon: {sensor:?}"));
    let entry = &beacon["hooks"][0];
    assert_eq!(entry["type"], "http", "got: {entry}");
    assert_eq!(
        entry["url"],
        format!(
            "http://127.0.0.1:41999{}",
            claude_hook::ROUTE_PRE_TOOL_USE_TAINT
        ),
        "got: {entry}"
    );
    // The identity a hook payload cannot carry — the key the whole latch
    // registry is built on — rides the header instead of argv.
    assert_eq!(entry["headers"]["X-CIMP-Tab"], "claude-2", "got: {entry}");
    // Report-only stays structural: no decision field can be emitted from an
    // entry, and the sensor's own budget is the standard 1 s.
    assert_eq!(entry["timeout"], 1, "got: {entry}");

    for mode in ["off", "deny"] {
        assert!(
            !pre_tool_use(mode)
                .iter()
                .any(|e| e["matcher"] == CLAUDE_WEB_TOOL_MATCHER),
            "{mode} must inject no beacon hook"
        );
    }
}

/// H2 discipline (`every_advertised_mcp_server_gets_a_loopback`): the
/// beacon's only delivery path is the loopback, so it must not be injected
/// when none runs — a process spawn per web call POSTing into a closed
/// socket is worse than no sensor.
#[test]
fn the_beacon_hook_is_not_injected_without_a_loopback() {
    let settings = Settings::default(); // offload + graph + audit all off
    assert!(!settings.loopback_needed());
    assert_eq!(
        crate::settings::injection::native_web_mode(
            &settings,
            crate::settings::injection::Scope::Tab {
                agent: "claude",
                tab: "claude",
            },
        ),
        NativeWebMode::Sensor,
        "the default mode is what makes this case worth pinning"
    );
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    if let Some(overlay) = settings_overlay(&args) {
        assert!(
            overlay.get("hooks").is_none(),
            "no loopback ⇒ no beacon: {overlay}"
        );
    }
}

/// Deny mode adds `permissions.deny` for the two web tools — and only in
/// deny mode. Bare tool names, no path globs (see the
/// `settings_overlay_matches_claude_settings_contract` note), and the rest
/// of the overlay is untouched.
#[test]
fn deny_mode_permission_denies_the_native_web_tools() {
    let overlay_for = |mode: &str| -> Option<serde_json::Value> {
        let mut s = Settings::default();
        s.graph.enabled = true;
        s.set_native_web_mode_for_test(NativeWebMode::parse(mode));
        settings_overlay(&build_pre_args(&claude_cfg(), &s, "claude", Some(&hook_endpoint())))
    };
    let deny = overlay_for("deny").expect("overlay present");
    assert_eq!(
        deny["permissions"],
        serde_json::json!({ "deny": ["WebFetch", "WebSearch"] }),
        "got: {deny}"
    );
    // Nothing else moved: the hooks object is still there and no
    // allow/ask lists were invented.
    assert!(deny["hooks"].is_object());
    assert!(deny["permissions"].get("allow").is_none());
    assert!(deny["permissions"].get("ask").is_none());
    for mode in ["off", "sensor"] {
        assert!(
            overlay_for(mode).is_some_and(|o| o.get("permissions").is_none()),
            "{mode} must carry no permission rules"
        );
    }
}

// ── V33 Phase F: the pre-mutation checkpoint seams ──────────────────────

/// **The Claude `PreToolUse` checkpoint beacon, and its two gates.**
///
/// The interesting half is what it is NOT gated on: `graph.enabled`. The
/// UserPromptSubmit checkpoint trigger rides `/context/retrieve`, a graph
/// route, and so carries `graph.enabled` as a passenger; this one rides
/// Workbench's own route and must not, or a checkpoint setting would depend
/// silently on an unrelated feature.
///
/// **What it would still pass with:** a hook injected unconditionally would
/// satisfy the presence assertion, so the two negative cases (checkpoints
/// off, and no loopback to deliver to) are asserted too — the second being
/// the H2 trap every other shim already has to answer.
#[test]
fn the_checkpoint_beacon_is_gated_on_checkpoints_and_a_live_loopback() {
    let pre_tool_matchers = |s: &Settings| -> Vec<String> {
        let args = build_pre_args(&claude_cfg(), s, "claude", Some(&hook_endpoint()));
        settings_overlay(&args)
            .and_then(|o| o["hooks"]["PreToolUse"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e["matcher"].as_str().map(str::to_string))
            .collect()
    };

    // Checkpoints ON, loopback live (offload alone is enough), graph OFF.
    let mut s = Settings::default();
    s.offload.enabled = true;
    s.workbench.checkpoints = true;
    assert!(s.loopback_needed());
    assert!(!s.graph.enabled, "the point of this case is a graph-OFF install");
    // The read advisor needs the graph and is therefore absent; the V32
    // native-web beacon is on by default under `sensor` and is not — which
    // is exactly the point: the checkpoint entry sits beside it with no
    // graph dependency of its own.
    assert!(
        pre_tool_matchers(&s).contains(&CLAUDE_MUTATING_TOOL_MATCHER.to_string()),
        "the checkpoint beacon must not depend on the code graph: {:?}",
        pre_tool_matchers(&s)
    );
    // …and the tab id is baked into the entry's headers, since the payload
    // names no cImp tab and an unattributable checkpoint is the one thing
    // this feature must not write.
    let args = build_pre_args(&claude_cfg(), &s, "claude-7", Some(&hook_endpoint()));
    let entries = settings_overlay(&args).expect("overlay")["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse array")
        .clone();
    let entry = entries
        .iter()
        .find(|e| e["matcher"] == CLAUDE_MUTATING_TOOL_MATCHER)
        .expect("the checkpoint entry")["hooks"][0]
        .clone();
    assert_eq!(entry["type"], "http", "got: {entry}");
    assert_eq!(
        entry["url"],
        format!(
            "http://127.0.0.1:41999{}",
            claude_hook::ROUTE_PRE_TOOL_USE_CHECKPOINT
        ),
        "got: {entry}"
    );
    assert_eq!(entry["headers"]["X-CIMP-Tab"], "claude-7", "got: {entry}");
    // The ONE entry whose ceiling is not 1 s: its handler holds the tool call
    // while the snapshot is taken, which is what makes "the checkpoint
    // precedes the call" exact (`claude_hook::TIMEOUT_CHECKPOINT_SECS`).
    assert_eq!(entry["timeout"], 5, "got: {entry}");

    // Checkpoints OFF ⇒ no checkpoint entry (the web beacon is unaffected —
    // asserted, so a regression that deleted BOTH would not read as a pass).
    s.workbench.checkpoints = false;
    let off = pre_tool_matchers(&s);
    assert!(!off.contains(&CLAUDE_MUTATING_TOOL_MATCHER.to_string()), "{off:?}");
    assert!(off.contains(&CLAUDE_WEB_TOOL_MATCHER.to_string()), "{off:?}");

    // Checkpoints ON but NO loopback ⇒ still no entry: the shim's only
    // delivery path is the loopback, and a process spawn per edit whose
    // POST lands nowhere is worse than no hook (H2).
    let mut s = Settings::default();
    s.workbench.checkpoints = true;
    assert!(!s.loopback_needed());
    assert!(pre_tool_matchers(&s).is_empty());
}

#[test]
fn injects_offload_mcp_config_for_claude_when_enabled() {
    let mut settings = Settings::default();
    settings.offload.enabled = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));

    let i = args
        .iter()
        .position(|a| a == "--mcp-config")
        .expect("--mcp-config present");
    let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
    assert_eq!(
        cfg["mcpServers"]["cimp-offload"]["args"][0],
        "--offload-mcp"
    );
}

#[test]
fn claude_mcp_child_carries_its_own_tab_id() {
    // V28: the per-tab MCP child is told WHICH tab it serves, so the app can
    // resolve that tab's current session instead of "the most recent Claude
    // session" — the whole point of the milestone. Two Claude tabs on one
    // project must bake DIFFERENT ids.
    let mut settings = Settings::default();
    settings.graph.enabled = true;
    for tab in ["claude", "claude-local"] {
        let argv = claude_offload_argv(&settings, tab);
        assert!(
            argv.windows(2).any(|w| w == ["--tab", tab]),
            "tab {tab} argv: {argv:?}"
        );
    }
    assert_ne!(
        claude_offload_argv(&settings, "claude"),
        claude_offload_argv(&settings, "claude-local"),
        "two Claude tabs must not spawn identical MCP children"
    );
}

#[test]
fn tab_id_rides_every_claude_mcp_gate() {
    // `--tab` is unconditional on the `cimp-offload` entry: whichever gate
    // caused the entry to be injected (offload / graph), the identity must
    // ride along. A gate that shipped it only sometimes would silently fall
    // back to the shared-scope bug.
    let with_offload = {
        let mut s = Settings::default();
        s.offload.enabled = true;
        s
    };
    let with_graph = {
        let mut s = Settings::default();
        s.graph.enabled = true;
        s
    };
    let with_both = {
        let mut s = Settings::default();
        s.offload.enabled = true;
        s.graph.enabled = true;
        s
    };
    for settings in [with_offload, with_graph, with_both] {
        let argv = claude_offload_argv(&settings, "claude");
        assert_eq!(argv[0], "--offload-mcp", "{argv:?}");
        assert!(
            argv.windows(2).any(|w| w == ["--tab", "claude"]),
            "{argv:?}"
        );
    }
}

/// **V37 Phase F flipped this test.** It used to assert that a Claude tab
/// with offload and graph off got no `--mcp-config` at all. The
/// `cimp-offload` child is now injected into every AI tab — that is the
/// whole phase — so the assertion that survives is about the CHILD'S
/// SURFACE, not the overlay's presence: the entry is there, carrying only
/// `--offload-mcp --tab <id>`, and what it advertises is decided live.
#[test]
fn offload_child_is_injected_even_with_every_feature_disabled() {
    let settings = Settings::default(); // offload + graph off by default
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let i = args
        .iter()
        .position(|a| a == "--mcp-config")
        .expect("V37 Phase F: the proxy child rides every AI tab");
    let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
    assert_eq!(
        cfg["mcpServers"]["cimp-offload"]["args"],
        serde_json::json!(["--offload-mcp", "--tab", "claude"]),
        "nothing enabled ⇒ the bare child argv"
    );
    assert!(
        cfg["mcpServers"]["cimp-code-audit"].is_null(),
        "the audit child is still gated — Phase F changed one server, not two"
    );
}

#[test]
fn graph_enabled_alone_injects_mcp_config() {
    // V9-01: the graph tools ride the same `--offload-mcp` child, so the
    // MCP config must be injected when graph is on even if offload is off.
    let mut settings = Settings::default();
    settings.offload.enabled = false;
    settings.graph.enabled = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));

    let i = args
        .iter()
        .position(|a| a == "--mcp-config")
        .expect("--mcp-config present when graph is enabled");
    let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
    assert_eq!(
        cfg["mcpServers"]["cimp-offload"]["args"][0],
        "--offload-mcp"
    );
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
        access: crate::settings::access_for_test(&[("claude", true)]),
        offload_access: false,
        ..Default::default()
    }];
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    assert!(
        args.iter().any(|a| a == "--mcp-config"),
        "--mcp-config present when a server is exposed to Claude Code"
    );
}

#[test]
fn code_audit_enabled_alone_injects_code_audit_server() {
    // V26: Code Audit rides its own `--code-audit-mcp` child, so the server
    // must appear in `--mcp-config` when the feature is on even with offload
    // + graph both off. With the default `expose_claude` true, no other
    // server is present — the audit server stands alone in the map.
    let mut settings = Settings::default();
    settings.offload.enabled = false;
    settings.graph.enabled = false;
    settings.code_audit.enabled = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));

    let i = args
        .iter()
        .position(|a| a == "--mcp-config")
        .expect("--mcp-config present when Code Audit is enabled");
    let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
    assert_eq!(
        cfg["mcpServers"]["cimp-code-audit"]["args"][0],
        "--code-audit-mcp"
    );
    // V37 Phase F: the offload child no longer rides a gate at all, so it
    // is present here too. The audit server does NOT stand alone any more;
    // what this test still pins is that Code Audit's own gate puts ITS entry
    // in the same overlay.
    assert_eq!(
        cfg["mcpServers"]["cimp-offload"]["args"][0],
        "--offload-mcp",
        "V37 Phase F: the proxy child rides every AI tab"
    );
}

#[test]
fn code_audit_server_absent_when_feature_disabled() {
    // The master switch off ⇒ no audit server even though `expose_claude`
    // defaults true. V37 Phase F: the overlay itself is still emitted (the
    // unconditional proxy child lives in it), so the assertion is about the
    // audit KEY, not about `--mcp-config`.
    let mut settings = Settings::default();
    settings.code_audit.enabled = false;
    assert!(settings.harness_row_of("claude").expose_code_audit, "default is opted-in");
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let i = args.iter().position(|a| a == "--mcp-config").unwrap();
    let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
    assert!(cfg["mcpServers"]["cimp-code-audit"].is_null());
}

#[test]
fn code_audit_server_absent_when_expose_claude_off() {
    // Feature on but the Claude consumer opted out ⇒ the audit server is not
    // advertised to Claude. V37 Phase F: the overlay still carries the
    // unconditional proxy child, so this asserts the audit key's absence.
    let mut settings = Settings::default();
    settings.code_audit.enabled = true;
    settings.harness_row("claude").expose_code_audit = false;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let i = args.iter().position(|a| a == "--mcp-config").unwrap();
    let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
    assert!(
        cfg["mcpServers"]["cimp-code-audit"].is_null(),
        "the audit server must not be injected when its consumer opted out"
    );
}

#[test]
fn code_audit_and_offload_share_one_mcp_config() {
    // Both gates on ⇒ both servers ride a single `--mcp-config` overlay.
    let mut settings = Settings::default();
    settings.offload.enabled = true;
    settings.code_audit.enabled = true;
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let count = args.iter().filter(|a| *a == "--mcp-config").count();
    assert_eq!(count, 1, "exactly one --mcp-config carries both servers");
    let i = args.iter().position(|a| a == "--mcp-config").unwrap();
    let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
    assert_eq!(
        cfg["mcpServers"]["cimp-offload"]["args"][0],
        "--offload-mcp"
    );
    assert_eq!(
        cfg["mcpServers"]["cimp-code-audit"]["args"][0],
        "--code-audit-mcp"
    );
}

#[test]
fn offload_injection_is_claude_only() {
    let mut settings = Settings::default();
    settings.offload.enabled = true;
    let args = build_pre_args(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint()));
    assert!(
        args.is_empty(),
        "opencode must get no pre-args, got: {args:?}"
    );
}
