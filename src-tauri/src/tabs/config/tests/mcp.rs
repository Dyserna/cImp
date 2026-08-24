//! The `--mcp-config` / `mcp` child composition on both harnesses: which servers
//! a tab advertises under which gates, and the per-tab id every child carries.

use super::*;

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

/// V32 C-1b (2026-08-07 review) — this REPLACES
/// `the_code_audit_child_gets_no_tab_id`, which pinned the opposite.
///
/// That test was right about V28's question (the audit child resolves no
/// memory scope) and wrong about V32's: a taint latch is keyed by
/// `(agent, tab)`, and once `security_audit`/`quality_audit` became
/// LOCAL-CAPABILITY, a child with no identity meant `/audit/run` had no
/// latch to consult — a contaminated tab could still run a gitleaks scan and
/// put the findings in its next search query. The identity is the gate's
/// input, so it is pinned per tab and on BOTH consumers' spawn paths.
#[test]
fn the_code_audit_child_carries_its_own_tab_id() {
    let mut settings = Settings::default();
    settings.code_audit.enabled = true;
    settings.harness_row("claude").expose_code_audit = true;
    for tab in ["claude", "claude-local"] {
        let args = build_pre_args(&claude_cfg(), &settings, tab, Some(&hook_endpoint()));
        let i = args.iter().position(|a| a == "--mcp-config").unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
        let argv: Vec<String> = cfg["mcpServers"]["cimp-code-audit"]["args"]
            .as_array()
            .expect("audit args")
            .iter()
            .map(|v| v.as_str().expect("string arg").to_string())
            .collect();
        assert_eq!(argv[0], "--code-audit-mcp", "{argv:?}");
        assert!(
            argv.windows(2).any(|w| w == ["--tab", tab]),
            "tab {tab} argv: {argv:?}"
        );
    }
    // The OpenCode mirror bakes it into the same `mcp` block that already
    // carries `--consumer opencode`.
    let mut oc = Settings::default();
    oc.code_audit.enabled = true;
    oc.harness_row("opencode").expose_code_audit = true;
    let cfg = build_opencode_config(&opencode_cfg(), &oc, "opencode-2");
    let cmd: Vec<String> = cfg["mcp"]["cimp-code-audit"]["command"]
        .as_array()
        .expect("audit command")
        .iter()
        .map(|v| v.as_str().expect("string arg").to_string())
        .collect();
    assert_eq!(
        &cmd[1..],
        [
            "--code-audit-mcp",
            "--consumer",
            "opencode",
            "--tab",
            "opencode-2"
        ],
        "got: {cmd:?}"
    );
}

/// **V37 Phase F, the phase's own contract.** With ZERO MCP grants, offload
/// disabled and the graph disabled — the exact install where the old gate
/// injected nothing — both harnesses' configs carry the `cimp-offload`
/// entry, and a Shell tab still carries nothing at all.
///
/// This is what makes an access flip propagate live: the entry IS the stdio
/// child, the child IS the `/events` subscriber, and a tab without one has
/// no channel for the contract-C5 pulse to arrive on.
#[test]
fn the_proxy_child_rides_ai_tabs_with_zero_grants_and_no_shell_tab() {
    let settings = Settings::default();
    assert!(!settings.offload.enabled);
    assert!(!settings.graph.enabled);
    assert!(settings.offload.mcp_servers.is_empty());
    assert!(!settings.offload.any_harness_mcp());
    assert!(!settings.offload.any_harness_mcp());

    // Claude.
    let args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    let i = args.iter().position(|a| a == "--mcp-config").expect("overlay");
    let claude: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
    assert_eq!(
        claude["mcpServers"]["cimp-offload"]["args"][0],
        "--offload-mcp"
    );
    // OpenCode — same child, `type: local`, its own consumer discriminator.
    let oc = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    assert_eq!(oc["mcp"]["cimp-offload"]["type"], "local");
    assert_eq!(oc["mcp"]["cimp-offload"]["command"][1], "--offload-mcp");
    assert_eq!(oc["mcp"]["cimp-offload"]["command"][3], "opencode");

    // A Shell tab is not an agent seam (V33 decision B1, and the same
    // reasoning here): it gets no pre-args and no generated config, so it
    // can never carry the proxy child.
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
            spec.pre_args.is_empty(),
            "a Shell tab must get no injected MCP config: {:?}",
            spec.pre_args
        );
        assert!(
            !spec.env.contains_key("OPENCODE_CONFIG_CONTENT"),
            "a Shell tab must get no generated OpenCode config"
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
fn every_advertised_mcp_server_gets_a_loopback() {
    // Tripwire for the V26 gap: any settings combo that advertises a
    // NON-EMPTY MCP tool surface MUST also flip
    // `Settings::loopback_needed()` — the injected children proxy every call
    // over the loopback, so advertising without serving strands them with
    // "cImp is not running" while the app is visibly up.
    //
    // **V37 Phase F narrowed the antecedent from "injects a server" to
    // "advertises a tool", and that is not a weakening.** The `cimp-offload`
    // entry is now written into every AI tab, so "injects a server" is a
    // tautology; what the child then LISTS is assembled at call time from
    // the same three inputs `loopback_needed()` is built out of — offload
    // tools gated on `offload.enabled`, `graph::mcp_tools()` on the graph
    // feature, and the proxied surface fetched from `POST /mcp/list`, which
    // returns nothing when there is no loopback to ask. So an all-off tab
    // advertises an EMPTY list and has nothing to strand, while every combo
    // that can produce a tool is still asserted below. The Code Audit child
    // is unchanged: gated, and tool-bearing the moment it exists.
    //
    // H2 (2026-08-05 review) widened it to HOOK SHIMS: every shim in the
    // `--settings` overlay reaches the app the same way (`post_loopback`),
    // so an injected hook without a loopback is the same defect in a
    // quieter form — the shim spawns, the POST is dropped, and nothing logs.
    // Sweep each feature axis alone and combined.
    for (offload, graph, audit) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, true, true),
    ] {
        let mut settings = Settings::default();
        settings.offload.enabled = offload;
        settings.graph.enabled = graph;
        settings.code_audit.enabled = audit;
        let claude_args = build_pre_args(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
        let claude_mcp: serde_json::Value = claude_args
            .iter()
            .position(|a| a == "--mcp-config")
            .map(|i| serde_json::from_str(&claude_args[i + 1]).unwrap())
            .unwrap_or(serde_json::Value::Null);
        let opencode_mcp = build_opencode_config(&opencode_cfg(), &settings, "opencode")
            .get("mcp")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        // Phase F's own invariant, asserted here because this is the sweep
        // that walks every feature axis: the proxy child rides BOTH
        // harnesses' configs for every combo, all-off included.
        assert!(
            !claude_mcp["mcpServers"]["cimp-offload"].is_null()
                && !opencode_mcp["cimp-offload"].is_null(),
            "the proxy child must ride every AI tab: \
                 offload={offload} graph={graph} audit={audit}"
        );
        // A tool-bearing advertisement: the audit child (tools by
        // construction), or an offload child whose live surface can be
        // non-empty.
        let audit_advertised = !claude_mcp["mcpServers"]["cimp-code-audit"].is_null()
            || !opencode_mcp["cimp-code-audit"].is_null();
        let offload_surface = settings.offload.enabled
            || settings.graph.enabled
            || settings.offload.any_harness_mcp()
            || settings.offload.any_harness_mcp();
        if audit_advertised || offload_surface {
            assert!(
                settings.loopback_needed(),
                "advertised an MCP tool without a loopback: \
                     offload={offload} graph={graph} audit={audit}"
            );
        }
        let hooks_installed = settings_overlay(&claude_args)
            .and_then(|o| o.get("hooks").cloned())
            .and_then(|h| h.as_object().map(|m| !m.is_empty()))
            .unwrap_or(false);
        if hooks_installed {
            assert!(
                settings.loopback_needed(),
                "installed a hook shim without a loopback: \
                     offload={offload} graph={graph} audit={audit}"
            );
        }
    }
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

#[test]
fn opencode_config_injects_mcp_when_offload_enabled() {
    let mut settings = Settings::default();
    settings.offload.enabled = true;
    let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    let cmd = &cfg["mcp"]["cimp-offload"]["command"];
    assert_eq!(cfg["mcp"]["cimp-offload"]["type"], "local");
    // The child is launched with the opencode consumer discriminator.
    let args: Vec<&str> = cmd
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(args.contains(&"--offload-mcp"), "got: {args:?}");
    assert!(
        args.windows(2).any(|w| w == ["--consumer", "opencode"]),
        "got: {args:?}"
    );
}

#[test]
fn opencode_mcp_child_carries_its_tab_id() {
    // V28: the OpenCode-side mirror of `claude_mcp_child_carries_its_own_tab_id`
    // — the `OPENCODE_CONFIG_CONTENT` mcp block bakes `--tab <id>` alongside
    // the consumer discriminator, and it reaches the real launch env (not
    // just the pure config builder).
    let mut settings = Settings::default();
    settings.graph.enabled = true;
    let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    let argv: Vec<&str> = cfg["mcp"]["cimp-offload"]["command"]
        .as_array()
        .expect("command array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        argv.windows(2).any(|w| w == ["--tab", "opencode"]),
        "got: {argv:?}"
    );
    // V32 C-1b: the audit child carries one too now — it is the taint
    // gate's input on `/audit/run`, not a memory scope. The full argv shape
    // is pinned by `the_code_audit_child_carries_its_own_tab_id`.
    let mut audit = Settings::default();
    audit.code_audit.enabled = true;
    audit.harness_row("opencode").expose_code_audit = true;
    let cfg = build_opencode_config(&opencode_cfg(), &audit, "opencode");
    let argv: Vec<&str> = cfg["mcp"]["cimp-code-audit"]["command"]
        .as_array()
        .expect("audit command")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        argv.windows(2).any(|w| w == ["--tab", "opencode"]),
        "got: {argv:?}"
    );

    // End-to-end through the env composer the PTY actually launches with.
    let env = compose_ai_env(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint()));
    let raw = env
        .get("OPENCODE_CONFIG_CONTENT")
        .expect("config env present");
    assert!(raw.contains("\"--tab\",\"opencode\""), "got: {raw}");
}

#[test]
/// **V37 Phase F flipped this test.** The `mcp` block used to be omitted
/// entirely when nothing was in play; the proxy child now rides every
/// OpenCode tab, so what is pinned is its EXACT argv — the block exists and
/// carries exactly one entry, the bare child.
fn opencode_config_carries_the_proxy_child_when_all_off() {
    let settings = Settings::default(); // offload + graph off, no servers
    let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    let mcp = cfg.get("mcp").expect("V37 Phase F: the proxy child always rides");
    assert_eq!(mcp["cimp-offload"]["type"], "local");
    let argv: Vec<&str> = mcp["cimp-offload"]["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        &argv[1..],
        ["--offload-mcp", "--consumer", "opencode", "--tab", "opencode"]
    );
    assert!(
        mcp.get("cimp-code-audit").is_none(),
        "the audit child is still gated"
    );
    assert_eq!(
        mcp.as_object().map(|m| m.len()),
        Some(1),
        "one entry, not two"
    );
}

#[test]
fn opencode_config_injects_mcp_when_graph_enabled() {
    let mut settings = Settings::default();
    settings.graph.enabled = true;
    let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    assert!(
        cfg["mcp"]["cimp-offload"].is_object(),
        "graph alone injects the mcp block"
    );
}

#[test]
fn opencode_config_injects_code_audit_when_enabled() {
    // V26: Code Audit enabled (offload + graph off) injects only the
    // `cimp-code-audit` entry, launched as a local child carrying the
    // opencode consumer discriminator.
    let mut settings = Settings::default();
    settings.code_audit.enabled = true;
    let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    assert_eq!(cfg["mcp"]["cimp-code-audit"]["type"], "local");
    let cmd: Vec<&str> = cfg["mcp"]["cimp-code-audit"]["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // Exact shape (V32 C-1b added `--tab <id>`):
    // [exe, "--code-audit-mcp", "--consumer", "opencode", "--tab", <id>].
    assert_eq!(cmd.len(), 6, "got: {cmd:?}");
    assert_eq!(
        &cmd[1..],
        [
            "--code-audit-mcp",
            "--consumer",
            "opencode",
            "--tab",
            "opencode"
        ]
    );
    // V37 Phase F: the offload child has no gate any more, so it is here
    // too — the audit entry shares the block instead of standing alone.
    assert_eq!(
        cfg["mcp"]["cimp-offload"]["type"],
        "local",
        "V37 Phase F: the proxy child rides every AI tab"
    );
}

#[test]
fn opencode_config_no_code_audit_when_expose_opencode_off() {
    // Feature on but the OpenCode consumer opted out ⇒ no audit entry.
    // V37 Phase F: the block itself survives, carrying the proxy child, so
    // this asserts the audit KEY rather than the block's absence.
    let mut settings = Settings::default();
    settings.code_audit.enabled = true;
    settings.harness_row("opencode").expose_code_audit = false;
    let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    assert!(
        cfg["mcp"].get("cimp-code-audit").is_none(),
        "no audit entry when the only enabled feature is opted out of OpenCode"
    );
}

/// V30 Phase A: the channel registration flag is emitted for a Claude tab
/// exactly when `offload.session_push` is on — as an adjacent
/// `<flag> server:cimp-offload` pair, addressing the same `mcpServers` key
/// `build_pre_args` writes into the `--mcp-config` overlay.
#[test]
fn session_push_adds_the_channel_registration_flag_for_claude_only() {
    let cfg = claude_cfg();

    // Default (off): no channel flag anywhere in the pre-args.
    let mut s = Settings::default();
    s.offload.enabled = true;
    let off = build_pre_args(&cfg, &s, "claude", Some(&hook_endpoint()));
    assert!(
        !off.iter().any(|a| a == CHANNEL_REGISTRATION_FLAG),
        "session_push defaults off — no channel flag"
    );

    // …and the CHILD half is absent too: with the gate off the spawned
    // `cimp-offload` argv carries no `--channel-push`.
    let off_mcp = off
        .iter()
        .position(|a| a == "--mcp-config")
        .and_then(|j| off.get(j + 1))
        .expect("offload enabled ⇒ an mcp-config overlay");
    assert!(!off_mcp.contains(CHANNEL_PUSH_FLAG));

    // On: flag + target, in that order, adjacent.
    s.offload.session_push = true;
    let on = build_pre_args(&cfg, &s, "claude", Some(&hook_endpoint()));
    let i = on
        .iter()
        .position(|a| a == CHANNEL_REGISTRATION_FLAG)
        .expect("channel flag is injected when session_push is on");
    assert_eq!(
        on.get(i + 1).map(String::as_str),
        Some("server:cimp-offload")
    );
    // The target names the very server the `--mcp-config` overlay defines;
    // a rename on either side would break registration silently.
    let mcp = on
        .iter()
        .position(|a| a == "--mcp-config")
        .and_then(|j| on.get(j + 1))
        .expect("offload enabled ⇒ an mcp-config overlay");
    assert!(mcp.contains("\"cimp-offload\""));
    // V30 (M5): BOTH halves of the gate come from this one settings read —
    // the client flag above and the child's own `--channel-push` on the
    // `cimp-offload` argv. A child restart must not be able to re-decide.
    let overlay: serde_json::Value = serde_json::from_str(mcp).unwrap();
    let child_args = overlay["mcpServers"]["cimp-offload"]["args"]
        .as_array()
        .expect("cimp-offload carries an args array");
    assert!(
        child_args.iter().any(|a| a == CHANNEL_PUSH_FLAG),
        "session_push on ⇒ the child is told to declare the channel"
    );

    // OpenCode (and any non-Claude command) gets no pre-args at all.
    assert!(build_pre_args(&opencode_cfg(), &s, "opencode", Some(&hook_endpoint())).is_empty());

    // **V37 Phase F flipped the last case.** It used to assert that
    // `session_push` with offload, graph and every Claude-exposed MCP server
    // off emitted no flag, because the `cimp-offload` server would not be
    // defined and registering a channel against an undefined server is
    // noise. The server is now always defined, so the registration is always
    // meaningful and the gate is `session_push` alone.
    let mut bare = Settings::default();
    bare.offload.session_push = true;
    bare.offload.enabled = false;
    bare.graph.enabled = false;
    let none = build_pre_args(&cfg, &bare, "claude", Some(&hook_endpoint()));
    let j = none
        .iter()
        .position(|a| a == CHANNEL_REGISTRATION_FLAG)
        .expect("V37 Phase F: the proxy child exists, so the channel registers");
    assert_eq!(
        none.get(j + 1).map(String::as_str),
        Some(CHANNEL_REGISTRATION_TARGET)
    );
}
