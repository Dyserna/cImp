//! Tab environment and launch geometry: `compose_ai_env`'s synthesized entries,
//! the inherited-marker scrub, the per-tab `env` precedence rule, the renderer
//! choice, and which tabs carry a harness at all.

use super::*;

/// NC-2: the cwd-fallback input — every Claude tab with the directory it
/// actually launches in (per-tab `cwd` override, else the app launch dir),
/// resolved exactly as `build_ai_tool_spec` does. OpenCode/Shell tabs are
/// excluded: the hook only fires for Claude.
#[test]
fn harness_tab_dirs_lists_that_harnesss_tabs_with_their_launch_dirs() {
    let mut settings = Settings {
        tabs: vec![
            TabConfig::AiTool(claude_cfg()),
            TabConfig::AiTool(opencode_cfg()),
        ],
        ..Settings::default()
    };
    let launch = Path::new("C:/proj");
    let dirs = harness_tab_dirs(&settings, launch, crate::harness::HarnessId::from_id("claude").unwrap());
    assert_eq!(dirs.len(), 1, "only the Claude tab: {dirs:?}");
    assert!(
        dirs.iter().all(|(_, d)| d == launch),
        "no per-tab cwd ⇒ every tab inherits the launch dir: {dirs:?}"
    );
    assert!(
        !dirs.iter().any(|(id, _)| id == "opencode"),
        "non-Claude tabs must not appear: {dirs:?}"
    );

    // A worktree tab (the one flow that sets `cwd`) reports its own dir.
    let mut wt = claude_cfg();
    wt.id = "ai-worktree".to_string();
    wt.cwd = Some(std::path::PathBuf::from("C:/proj/wt"));
    settings.tabs.push(TabConfig::AiTool(wt));
    let dirs = harness_tab_dirs(&settings, launch, crate::harness::HarnessId::from_id("claude").unwrap());
    assert_eq!(
        dirs.iter()
            .find(|(id, _)| id == "ai-worktree")
            .map(|(_, d)| d.clone()),
        Some(std::path::PathBuf::from("C:/proj/wt"))
    );
}

/// H1 (2026-08-05 review) cross-module invariant: the directory a Claude
/// tab's out-of-band tap derives its transcript root from (and therefore the
/// key the same-root ambiguity predicate groups tabs by) is the SAME
/// directory `harness_tab_dirs` reports to the permission-hook cwd fallback.
/// If these ever diverge, one seam would call two tabs co-tenants while the
/// other treats them as distinct — the failure mode H1 exists to remove.
#[test]
fn claude_oob_root_and_permission_cwd_resolve_to_the_same_dir() {
    let launch = Path::new("C:/proj");
    let mut wt = claude_cfg();
    wt.id = "ai-worktree".to_string();
    wt.cwd = Some(std::path::PathBuf::from("C:/proj/wt"));
    let settings = Settings {
        tabs: vec![
            TabConfig::AiTool(claude_cfg()),
            TabConfig::AiTool(wt.clone()),
        ],
        ..Settings::default()
    };
    let dirs = harness_tab_dirs(&settings, launch, crate::harness::HarnessId::from_id("claude").unwrap());
    for (cfg, id) in [(claude_cfg(), "claude"), (wt, "ai-worktree")] {
        let mut extra: Vec<String> = Vec::new();
        // Exactly what `build_ai_tool_spec` hands the oob resolver. The env
        // map is only read for the OpenCode server credential, so a Claude
        // tab's resolution is unaffected by it being empty here.
        let source = resolve_oob_source(
            &cfg,
            &ai_working_dir(&cfg, launch),
            &mut extra,
            &HashMap::new(),
        );
        let Some(crate::harness::OobSpec::ClaudeTranscript {
            project_dir,
            pinned_session,
        }) = source
        else {
            panic!("a Claude tab must resolve a transcript source");
        };
        // V34: the pin the tap will follow must be the one actually put on
        // the child's argv — the two are produced together precisely so
        // they cannot drift, and this is the assertion that keeps it so.
        let sid = pinned_session.expect("a plain Claude tab must be pinned");
        assert_eq!(
            extra.windows(2).find(|w| w[0] == "--session-id").map(|w| &w[1]),
            Some(&sid),
            "tab {id}: --session-id on argv must match the pinned session"
        );
        let hook_dir = dirs
            .iter()
            .find(|(t, _)| t == id)
            .map(|(_, d)| d.clone())
            .expect("tab listed for the hook fallback");
        assert_eq!(
            project_dir, hook_dir,
            "tab {id}: transcript root dir and permission cwd must agree"
        );
    }
}

#[test]
fn claude_launches_fullscreen_by_default() {
    // V20: cImp no longer forces Claude's inline renderer. Without an
    // explicit per-tab override, no `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`
    // is synthesized, so Claude runs in its native fullscreen TUI.
    let settings = Settings::default();
    let env = compose_ai_env(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    assert!(
        !env.contains_key("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
        "V20: cImp must not force Claude's inline renderer",
    );
}

#[test]
fn no_ai_tab_forces_inline_renderer() {
    // V20: neither AI tool gets the alt-screen opt-out; both go fullscreen.
    let settings = Settings::default();
    for env in [
        compose_ai_env(&claude_cfg(), &settings, "claude", Some(&hook_endpoint())),
        compose_ai_env(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint())),
    ] {
        assert!(
            !env.contains_key("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
            "no AI tab should set the Claude fullscreen opt-out in V20",
        );
    }
}

#[test]
fn opencode_sets_noise_suppression_env() {
    let settings = Settings::default();
    let env = compose_ai_env(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint()));
    assert_eq!(
        env.get("OPENCODE_DISABLE_TERMINAL_TITLE")
            .map(String::as_str),
        Some("1"),
    );
    assert!(
        !env.contains_key("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
        "opencode must not get the Claude fullscreen flag"
    );
}

#[test]
fn per_tab_env_overrides_opencode_config_content() {
    let settings = Settings::default();
    let mut cfg = opencode_cfg();
    cfg.env
        .insert("OPENCODE_CONFIG_CONTENT".to_string(), "custom".to_string());
    let env = compose_ai_env(&cfg, &settings, "claude", Some(&hook_endpoint()));
    assert_eq!(
        env.get("OPENCODE_CONFIG_CONTENT").map(String::as_str),
        Some("custom"),
        "an explicit per-tab value must win over the synthesized config",
    );
}

#[test]
fn per_tab_env_can_reenable_inline_renderer() {
    // V20: cImp no longer synthesizes the alt-screen opt-out, but a user who
    // wants the old inline renderer can still set it per tab; the per-tab env
    // merge carries it through untouched.
    let settings = Settings::default();
    let mut cfg = claude_cfg();
    cfg.env.insert(
        "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".to_string(),
        "1".to_string(),
    );
    let env = compose_ai_env(&cfg, &settings, "claude", Some(&hook_endpoint()));
    assert_eq!(
        env.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN")
            .map(String::as_str),
        Some("1"),
        "an explicit per-tab value must pass through the env merge",
    );
}

// ── V30 Phase C: MCP auto-backgrounding is left to Claude Code ─────────

#[test]
fn no_tab_gets_a_synthesized_mcp_auto_background_env() {
    // The inverse of the old Maintenance D-2 assertion: V30 Phase 0 T4
    // proved a backgrounded MCP call still delivers its full result text,
    // so cImp must NOT pin `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` any more.
    // If this fails, the kill switch was re-added — read the comment in
    // `compose_ai_env` before changing this test.
    let settings = Settings::default();
    let mut other = claude_cfg();
    other.command = "some-other-tool".to_string();
    for cfg in [claude_cfg(), opencode_cfg(), other] {
        let env = compose_ai_env(&cfg, &settings, "claude", Some(&hook_endpoint()));
        assert!(
            !env.contains_key("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS"),
            "cImp must not synthesize the auto-background kill switch (command: {})",
            cfg.command,
        );
    }
    // Shell tabs never reach `compose_ai_env` at all — `build_launch_spec`
    // passes their `env` through verbatim.
}

#[test]
fn per_tab_env_can_still_set_mcp_auto_background_ms() {
    // The user-facing escape hatch: cImp synthesizes nothing, but an
    // explicit per-tab value still reaches the child.
    let settings = Settings::default();
    let mut cfg = claude_cfg();
    cfg.env.insert(
        "CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS".to_string(),
        "0".to_string(),
    );
    let env = compose_ai_env(&cfg, &settings, "claude", Some(&hook_endpoint()));
    assert_eq!(
        env.get("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS")
            .map(String::as_str),
        Some("0"),
        "an explicit per-tab value must pass through the env merge",
    );
}

// ── V30 (review M9): harness env scrub ────────────────────────────────

#[test]
fn ai_tabs_scrub_the_inherited_claude_harness_markers() {
    // Pins the LIST. `CLAUDE_CODE_CHILD_SESSION` is the load-bearing one —
    // inheriting it gives the spawned Claude no transcript at all, which
    // blinds the oob tap with no log anywhere. Adding to this list is fine;
    // dropping `CLAUDE_CODE_CHILD_SESSION` re-opens the silent failure.
    for cfg in [claude_cfg(), opencode_cfg()] {
        let removals = ai_env_removals(&cfg);
        assert_eq!(
            removals,
            vec![
                "CLAUDE_CODE_CHILD_SESSION".to_string(),
                "CLAUDECODE".to_string(),
                "CLAUDE_CODE_ENTRYPOINT".to_string(),
            ],
            "every AI tab strips the same harness markers (command: {})",
            cfg.command,
        );
    }
}

#[test]
fn a_per_tab_env_entry_is_never_scrubbed() {
    // The strip list is cImp's default, not a veto on the user's own
    // configuration.
    let mut cfg = claude_cfg();
    cfg.env
        .insert("CLAUDECODE".to_string(), "1".to_string());
    let removals = ai_env_removals(&cfg);
    assert!(!removals.contains(&"CLAUDECODE".to_string()));
    assert!(removals.contains(&"CLAUDE_CODE_CHILD_SESSION".to_string()));
}

#[test]
fn shell_tabs_keep_their_environment_untouched() {
    // A Shell tab's whole point is the environment the user actually has.
    let mut settings = Settings::default();
    settings.tabs.push(TabConfig::Shell(crate::settings::ShellTabConfig {
        id: "shell-1".to_string(),
        name: "Shell".to_string(),
        command: "cmd".to_string(),
        ..Default::default()
    }));
    let spec = build_launch_spec(
        TabId::from_str("shell-1"),
        &settings,
        &std::env::temp_dir(),
        &[],
    );
    if let Ok(spec) = spec {
        assert!(
            spec.env_remove.is_empty(),
            "shell tabs must not have their environment edited"
        );
    }
}

/// **Which tabs are agent seams** (V33 Phase B decision B1).
///
/// # Why the paths here are BUILT, not spelled
///
/// The first version of this test asserted on a literal Windows path and
/// passed on Windows while failing on the Linux CI runner, because the
/// lookup resolves through `Path::file_stem` and a backslash is not a
/// separator on Linux — so that whole string is one file name. The defect
/// was entirely in the fixture (see
/// [`every_default_ai_tab_carries_a_harness_on_every_platform`] for the
/// production-surface guard), and the standing rule it broke is: a path in
/// a test fixture is built with `Path::join` so the separators are the
/// running platform's, or it is not a path at all.
#[test]
fn only_ai_tool_tabs_carry_a_harness_and_shell_tabs_never_do() {
    use crate::harness::HarnessId;
    let claude = HarnessId::from_id("claude");
    let opencode = HarnessId::from_id("opencode");
    // The bare configured names — what settings actually hold, and identical
    // on every platform. `claude-local` is a TAB id whose COMMAND is
    // `claude`, so it is the same harness with the same state directories.
    assert_eq!(HarnessId::from_command("claude"), claude);
    assert_eq!(HarnessId::from_command("opencode"), opencode);
    // Case-insensitive, and an extension is stripped. No separator in these,
    // so they mean the same thing on both platforms.
    assert_eq!(HarnessId::from_command("CLAUDE.EXE"), claude);
    assert_eq!(HarnessId::from_command("OpenCode"), opencode);

    // A fully-qualified path, spelled the way THIS platform spells one.
    let resolved = std::path::Path::new("home")
        .join("x")
        .join(".local")
        .join("bin")
        .join(if cfg!(windows) { "claude.exe" } else { "claude" });
    assert_eq!(
        HarnessId::from_command(&resolved.to_string_lossy()),
        claude,
        "a resolved harness path must be recognised on {}",
        std::env::consts::OS
    );

    // Anything else is NOT sandboxed: a grant table nobody wrote is not a
    // boundary, it is a tool that fails to start invisibly.
    assert_eq!(HarnessId::from_command("bash"), None);
    assert_eq!(HarnessId::from_command("aider"), None);
    assert_eq!(HarnessId::from_command(""), None);

    // …and the split the SANDBOX makes is the split the INJECTION layer
    // makes, because since V40 Phase A there is only one — both ends ask
    // `HarnessId::from_command`, so a tab's grant table and its injected
    // config can never disagree about what it is.
    for command in ["claude", "opencode", "bash"] {
        assert_eq!(
            HarnessId::from_command(command).and_then(|h| h.id()),
            crate::harness::HarnessId::from_command(command).and_then(|h| h.id()),
            "{command}: the sandbox and the injection layer disagree"
        );
    }

    // A Shell tab, through the real builder: no harness, therefore never
    // sandboxed and never a row. It is the user's own hands.
    let mut s = Settings::default();
    s.tabs.push(TabConfig::Shell(crate::settings::ShellTabConfig {
        id: "shell-1".into(),
        name: "Shell".into(),
        command: if cfg!(windows) { "cmd" } else { "sh" }.into(),
        ..Default::default()
    }));
    let dir = std::env::temp_dir();
    if let Ok(spec) = build_launch_spec(TabId::Shell("shell-1".into()), &s, &dir, &[]) {
        assert!(
            spec.harness.is_none(),
            "a Shell tab must never be an agent seam"
        );
    }
}

/// **The production-surface guard the fixture above is not.**
///
/// If `harness_of` ever returned `None` for a real AI tab on some platform,
/// `PtyManager::start` would skip `sandbox::tabs::plan_tab` entirely — so
/// that platform would get neither a boundary nor the loud skip row that is
/// supposed to say why, which is exactly the silent degradation V33 decision
/// 5 forbids. A hand-spelled fixture cannot catch that; the shipped defaults
/// can.
///
/// Runs on every platform and reads the REAL default tab list, so a future
/// default whose command is platform-conditional (or a fourth harness added
/// without a grant table) fails here rather than in the field.
///
/// V40 Phase I: the list is `canonical_ai_tab_order()` rather than three
/// hand-spelled ids guarded by an exhaustive `match` whose only job was to
/// stop compiling when a fourth was added. The registry view covers a fourth
/// reserved tab the day it is DECLARED, which is a stronger guarantee than a
/// compile error someone has to be routed through.
#[test]
fn every_default_ai_tab_carries_a_harness_on_every_platform() {
    use crate::settings::{canonical_ai_tab_order, default_ai_tab};
    // `Settings::default().tabs` is EMPTY — the reserved builtins are
    // materialized by the integrity check from this factory, so the factory
    // is what "the shipped defaults" means here.
    let mut seen = 0usize;
    for id in canonical_ai_tab_order() {
        let TabConfig::AiTool(cfg) = default_ai_tab(id) else {
            panic!("{} is not an AI-tool tab", id.as_str());
        };
        seen += 1;
        assert!(
            crate::harness::HarnessId::from_command(&cfg.command).is_some(),
            "the default AI tab `{}` (command `{}`) carries no harness on {} — it would \
                 spawn with NO sandbox and NO skip row explaining why",
            cfg.id,
            cfg.command,
            std::env::consts::OS
        );
        // A default tab's command must be a bare program name. The moment a
        // default ships a path-shaped command, this seam's answer becomes
        // platform-specific — which is precisely how the fixture above once
        // went green on Windows and red on Linux.
        assert!(
            !cfg.command.contains('/') && !cfg.command.contains('\\'),
            "the default AI tab `{}` ships a path-shaped command (`{}`); harness detection \
                 is `Path::file_stem`-based and a hardcoded separator does not travel",
            cfg.id,
            cfg.command
        );
    }
    assert_eq!(seen, 3, "this test read nothing useful");
}
