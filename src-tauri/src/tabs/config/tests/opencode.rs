//! The OpenCode launch spine: argv (the `--mini` guard), the authenticated
//! spawn and its tap credential, `OPENCODE_CONFIG_CONTENT`'s pinned permission
//! block and provider rows, and the generated plugin's on-disk gates.

use super::*;

// ── 2026-08-17: the OpenCode server credential, spawn → reader ──────────

/// **The seam, end to end.** The password is generated where the child's env
/// is composed and consumed where the tap is described, and the two halves are
/// only correct together: a credential on the child that the reader does not
/// present makes every tap request 401, and a credential the reader presents
/// that the child never got makes the server refuse nothing.
#[test]
fn an_opencode_tab_is_spawned_authenticated_and_its_tap_is_given_the_credential() {
    use crate::harness::opencode::config::{
        server_basic_auth, SERVER_PASSWORD_ENV, SERVER_USERNAME, SERVER_USERNAME_ENV,
    };
    let settings = Settings::default();
    let env = compose_ai_env(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint()));

    // The pair is on the child, and the password is substantive — an EMPTY
    // one disables auth upstream, which is the one value that would make
    // every assertion below pass while enforcing nothing.
    let password = env
        .get(SERVER_PASSWORD_ENV)
        .expect("an OpenCode tab must be spawned with a server password");
    assert!(!password.is_empty(), "an empty password disables auth");
    assert_eq!(env.get(SERVER_USERNAME_ENV).map(String::as_str), Some(SERVER_USERNAME));

    // …and the tap is handed the matching credential, via the spec rather
    // than via argv or a URL.
    let mut extra: Vec<String> = Vec::new();
    let source = resolve_oob_source(
        &opencode_cfg(),
        Path::new("C:/proj"),
        &mut extra,
        &env,
    );
    let Some(crate::harness::OobSpec::OpenCodeEvent { port, auth }) = source else {
        panic!("an OpenCode tab must resolve an event source");
    };
    assert_eq!(
        auth.as_deref(),
        server_basic_auth(password).as_deref(),
        "the tap must present the credential the child was spawned with"
    );
    // The secret is on neither the command line nor the port flag.
    assert!(
        !extra.iter().any(|a| a.contains(password.as_str())),
        "a secret must never reach argv: {extra:?}"
    );
    assert!(extra.iter().any(|a| a == &port.to_string()));

    // A CLAUDE tab gets neither half: this is an OpenCode server.
    let claude = compose_ai_env(&claude_cfg(), &settings, "claude", Some(&hook_endpoint()));
    assert!(!claude.contains_key(SERVER_PASSWORD_ENV));
    assert!(!claude.contains_key(SERVER_USERNAME_ENV));
}

/// Every spawn gets its own password — the same discipline as the loopback
/// token, and the reason neither owes a `spawn_inject_sig` entry: nothing a
/// user can configure moves it, so there is no setting to raise a restart
/// hint for.
#[test]
fn each_opencode_spawn_gets_a_fresh_server_password() {
    use crate::harness::opencode::config::SERVER_PASSWORD_ENV;
    let settings = Settings::default();
    let first = compose_ai_env(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint()));
    let second = compose_ai_env(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint()));
    assert_ne!(
        first.get(SERVER_PASSWORD_ENV),
        second.get(SERVER_PASSWORD_ENV),
        "the password must be per spawn, not per build"
    );
    // …and it is not what `spawn_inject_sig` reports on: two signatures over
    // the same settings must still compare equal, or every settings edit
    // would nag every OpenCode tab to restart for a value the user cannot
    // see or set.
    assert_eq!(spawn_inject_sig(&settings), spawn_inject_sig(&settings));
}

/// A user who sets their own password per tab keeps it (the per-tab `env`
/// merge is an instruction, not an accident) — and the tap follows, because
/// it reads the credential back out of the COMPOSED environment rather than
/// remembering what cImp generated. Getting this wrong 401s every tap on a
/// tab that looks correctly configured.
#[test]
fn a_per_tab_server_password_override_is_honoured_by_the_tap_too() {
    use crate::harness::opencode::config::{server_basic_auth, SERVER_PASSWORD_ENV};
    let mut cfg = opencode_cfg();
    cfg.env
        .insert(SERVER_PASSWORD_ENV.to_string(), "mine-not-cimps".to_string());
    let env = compose_ai_env(&cfg, &Settings::default(), "opencode", Some(&hook_endpoint()));
    assert_eq!(
        env.get(SERVER_PASSWORD_ENV).map(String::as_str),
        Some("mine-not-cimps"),
        "a per-tab env entry wins over the synthesized value"
    );
    let mut extra: Vec<String> = Vec::new();
    let Some(crate::harness::OobSpec::OpenCodeEvent { auth, .. }) =
        resolve_oob_source(&cfg, Path::new("C:/proj"), &mut extra, &env)
    else {
        panic!("an OpenCode tab must resolve an event source");
    };
    assert_eq!(auth.as_deref(), server_basic_auth("mine-not-cimps").as_deref());
}

/// **#142 — a DUPLICATED OpenCode tab is an OpenCode tab.** The `+` copy keeps
/// the template's `command` and gets a fresh `ai-<uuid>` id and `builtin:
/// false`, so every harness decision on the launch path must still be taken
/// from the command. A duplicate that resolved no `OobSpec` would run with no
/// event tap at all: no turn boundaries, no TTS, no usage — and a delegation
/// into it could never observe the turn end (the symptom filed as #142).
///
/// Asserts the whole per-tab shape, because "an OpenCode tab" is not enough:
/// the port must be the duplicate's OWN (never the template's) and the
/// credential must be the one THIS spawn's environment carries.
#[test]
fn a_duplicated_opencode_tab_still_resolves_its_own_event_tap() {
    use crate::harness::opencode::config::{server_basic_auth, SERVER_PASSWORD_ENV};
    let settings = Settings::default();
    // Exactly what `service::tabs::commit_ai_duplicate` writes: same command,
    // new id, not builtin.
    let mut dup = opencode_cfg();
    dup.id = "ai-ac0f0268-eb5d-44d0-8bbc-54bb5b7fb990".to_string();
    dup.builtin = false;
    dup.name = "OpenCode 2".to_string();

    let env = compose_ai_env(&dup, &settings, &dup.id, Some(&hook_endpoint()));
    let password = env
        .get(SERVER_PASSWORD_ENV)
        .expect("a duplicated OpenCode tab is spawned authenticated too");
    let mut extra: Vec<String> = Vec::new();
    let Some(crate::harness::OobSpec::OpenCodeEvent { port, auth }) =
        resolve_oob_source(&dup, Path::new("C:/proj"), &mut extra, &env)
    else {
        panic!("a duplicated OpenCode tab must resolve an event source (#142)");
    };
    assert_eq!(
        auth.as_deref(),
        server_basic_auth(password).as_deref(),
        "the duplicate's tap must present the duplicate's own credential"
    );
    // The port the TUI is told to host on is the port the tap will dial.
    let at = extra
        .iter()
        .position(|a| a == "--port")
        .expect("a duplicated OpenCode tab must be launched with --port");
    assert_eq!(extra.get(at + 1).map(String::as_str), Some(port.to_string().as_str()));
    assert!(extra.iter().any(|a| a == "--hostname"));

    // …and it is its OWN port: two duplicates of one template must not be
    // handed the same server to tap.
    let mut extra2: Vec<String> = Vec::new();
    let env2 = compose_ai_env(&dup, &settings, &dup.id, Some(&hook_endpoint()));
    let Some(crate::harness::OobSpec::OpenCodeEvent { port: port2, .. }) =
        resolve_oob_source(&dup, Path::new("C:/proj"), &mut extra2, &env2)
    else {
        panic!("a duplicated OpenCode tab must resolve an event source (#142)");
    };
    assert_ne!(port, port2, "each spawn reserves its own loopback port");
}

// ---- V19: OpenCode launch spine ----

#[test]
fn opencode_launches_without_mini() {
    // V20: OpenCode runs its full fullscreen TUI — no `--mini` is injected,
    // so the complete command palette (e.g. `/connect`) is available.
    let settings = Settings::default();
    let args = build_extra_args(&opencode_cfg(), &settings, &[]);
    assert!(
        !args.iter().any(|a| a == "--mini"),
        "V20: opencode must NOT get --mini, got: {args:?}"
    );
}

#[test]
fn no_mini_for_any_ai_tab() {
    let settings = Settings::default();
    let claude = build_extra_args(&claude_cfg(), &settings, &[]);
    assert!(
        !claude.iter().any(|a| a == "--mini"),
        "claude must not get --mini"
    );
    let opencode = build_extra_args(&opencode_cfg(), &settings, &[]);
    assert!(
        !opencode.iter().any(|a| a == "--mini"),
        "opencode must not get --mini in V20"
    );
    // A non-opencode, non-claude AI command must not get --mini either.
    let mut other = claude_cfg();
    other.command = "some-other-tool".to_string();
    let other = build_extra_args(&other, &settings, &[]);
    assert!(
        !other.iter().any(|a| a == "--mini"),
        "non-opencode tabs must not get --mini"
    );
}

/// D-8 — the `--mini` × `--port` guard. `resolve_oob_source` always
/// appends `--port <N> --hostname 127.0.0.1` to an OpenCode launch, and
/// OpenCode hard-fails when `--mini` is combined with `--port`. A stored
/// `--mini` (hand-edited settings, a carried-over config file) must
/// therefore never reach the command line — while every other user arg
/// survives untouched.
#[test]
fn opencode_strips_user_supplied_mini_but_keeps_other_args() {
    let settings = Settings::default();
    let mut cfg = opencode_cfg();
    cfg.args = vec![
        "--mini".to_string(),
        "--model".to_string(),
        "x".to_string(),
        "--mini=true".to_string(),
        String::new(),
        "--continue".to_string(),
    ];
    let args = build_extra_args(&cfg, &settings, &[]);
    assert!(
        !args.iter().any(|a| a.starts_with("--mini")),
        "stored --mini must be stripped (it hard-fails with the injected --port), got: {args:?}"
    );
    assert_eq!(
        args,
        vec![
            "--model".to_string(),
            "x".to_string(),
            "--continue".to_string()
        ],
        "only --mini is dropped; every other user arg is preserved in order",
    );
}

/// The guard is OpenCode-specific: another AI tool's `--mini` (whatever it
/// may mean there) is none of cImp's business — no `--port` is injected for
/// it, so there is no conflict to resolve.
#[test]
fn mini_guard_is_opencode_only() {
    let settings = Settings::default();
    let mut cfg = claude_cfg();
    cfg.command = "some-other-tool".to_string();
    cfg.args = vec!["--mini".to_string()];
    assert_eq!(
        build_extra_args(&cfg, &settings, &[]),
        vec!["--mini".to_string()],
        "non-opencode tabs keep their own args verbatim",
    );
}

#[test]
fn opencode_config_content_is_valid_json() {
    let settings = Settings::default();
    let env = compose_ai_env(&opencode_cfg(), &settings, "opencode", Some(&hook_endpoint()));
    let raw = env
        .get("OPENCODE_CONFIG_CONTENT")
        .expect("opencode tab sets OPENCODE_CONFIG_CONTENT");
    let cfg: serde_json::Value =
        serde_json::from_str(raw).expect("OPENCODE_CONFIG_CONTENT is valid JSON");
    assert_eq!(cfg["$schema"], "https://opencode.ai/config.json");
}

/// D-8 — `subagent_depth` is pinned unconditionally. OpenCode 1.18.2 made
/// the default 1 (subagents may not launch subagents); cImp states 2 so an
/// upgrade can't silently change nesting behavior. Constant by design: it
/// derives from no setting, so it needs no `spawn_inject_sig` entry and
/// must be present in the barest possible config.
#[test]
fn opencode_config_pins_subagent_depth() {
    for settings in [Settings::default(), {
        // Every injection gate on — the key survives a maximal config too.
        let mut s = Settings::default();
        s.offload.enabled = true;
        s.graph.enabled = true;
        s.code_audit.enabled = true;
        s.harness_row("opencode").expose_code_audit = true;
        s
    }] {
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        assert_eq!(
            cfg["subagent_depth"],
            serde_json::json!(2),
            "subagent_depth must be pinned to 2 in every OpenCode config",
        );
    }
}

// ── V32 Phase F — native-web visibility modes (locked decision 14) ──────

/// The locked default and the post-hoc validation of a hand-editable
/// string. `sensor` is the default because we cannot assume what MCP setup
/// a user runs and a silent side channel is worse than a beacon; an
/// unrecognized value must land on that same default rather than blinding
/// the latch (`off`) or taking a tool away (`deny`).
#[test]
fn native_web_visibility_defaults_to_sensor_and_validates_post_hoc() {
    // The stored default, read through the resolver rather than off the
    // field (#48: the tri-mode IS `Feature::NativeWeb`'s L2, so it now sits
    // behind the same `pub(in crate::settings)` boundary as the rest).
    assert_eq!(
        crate::settings::injection::native_web_mode(
            &Settings::default(),
            crate::settings::injection::Scope::Tab {
                agent: "opencode",
                tab: "opencode",
            },
        ),
        NativeWebMode::Sensor
    );
    assert_eq!(NativeWebMode::parse("off"), NativeWebMode::Off);
    assert_eq!(
        NativeWebMode::parse(" sensor "),
        NativeWebMode::Sensor
    );
    assert_eq!(NativeWebMode::parse("deny"), NativeWebMode::Deny);
    for junk in ["", "OFF", "Deny", "denied", "sensr", "true"] {
        assert_eq!(
            NativeWebMode::parse(junk),
            NativeWebMode::Sensor,
            "{junk:?} must fall back to the default, not to off/deny"
        );
    }
}

/// **The E2 spike's fail-open trap, closed.** Until Phase F the plugin was
/// written iff `graph.enabled` and DELETED otherwise, so a security handler
/// riding it vanished when an unrelated feature was toggled off. The write
/// condition is now the OR of every consumer's need.
#[test]
fn the_opencode_plugin_is_written_for_the_beacon_with_the_graph_off() {
    let with = |graph: bool, mode: &str| -> bool {
        let mut s = Settings::default();
        s.graph.enabled = graph;
        // The Phase H gate is the plugin's FOURTH reason to exist and ships
        // on since V39, so it has to be silenced for the two-disjunct
        // property below to be about the two disjuncts it names.
        s.set_l2_for_test(
            crate::settings::injection::Feature::HarnessNativeGate,
            false,
        );
        s.set_native_web_mode_for_test(NativeWebMode::parse(mode));
        opencode_plugin_wanted(&s, "opencode")
    };
    // The case the trap was: graph off, sensor on ⇒ still written.
    assert!(with(false, "sensor"), "graph off must not delete the sensor");
    assert!(with(true, "sensor"));
    assert!(with(true, "off"), "the graph alone still wants it");
    assert!(with(true, "deny"));
    // Nothing wants it ⇒ removed, as before. `deny` needs no plugin: the
    // pinned permission block does that work.
    assert!(!with(false, "off"));
    assert!(!with(false, "deny"));
    // And a mode flip still raises the restart hint. V32 Phase G moved WHERE
    // it does: `plugin[0]` is now the app-wide graph half alone, because the
    // predicate went per-tab, and the sensor half is carried — per tab, with
    // its resolved mode — by the `injection` fragment. The property the
    // trap-closing test cares about is unchanged: flipping the mode makes a
    // fresh OpenCode tab launch differently, and the signature says so.
    let mut off = Settings {
        tabs: vec![opencode_tab_inheriting()],
        ..Settings::default()
    };
    off.graph.enabled = false;
    off.set_native_web_mode_for_test(NativeWebMode::Off);
    let mut sensor = off.clone();
    sensor.set_native_web_mode_for_test(NativeWebMode::Sensor);
    assert_ne!(
        spawn_inject_sig(&off)[&h("opencode")],
        spawn_inject_sig(&sensor)[&h("opencode")],
        "a mode flip with the graph off must still raise the restart hint"
    );
}

/// The plugin's only channel to its own identity. OpenCode's
/// `tool.execute.before` input carries a session id but no tab and no cwd
/// (the E2 spike's finding), and the latch registry is keyed by
/// (agent, tab) — so without this env var a beacon has nothing to engage.
/// Unconditional: it is not settings-derived, so it needs no restart hint.
#[test]
fn opencode_env_carries_the_tab_id_for_the_plugin() {
    for mode in ["off", "sensor", "deny"] {
        let mut s = Settings::default();
        s.set_native_web_mode_for_test(NativeWebMode::parse(mode));
        let env = compose_ai_env(&opencode_cfg(), &s, "opencode-3", Some(&hook_endpoint()));
        assert_eq!(
            env.get("CIMP_TAB_ID").map(String::as_str),
            Some("opencode-3"),
            "{mode}"
        );
    }
    // Claude tabs need no equivalent — their hook command bakes `--tab`
    // into argv — so nothing is synthesized there.
    let env = compose_ai_env(&claude_cfg(), &Settings::default(), "claude", Some(&hook_endpoint()));
    assert!(!env.contains_key("CIMP_TAB_ID"), "got: {env:?}");
}

/// **The E2 fail-open trap again, checkpoint edition.** `workbench.
/// checkpoints` is the FOURTH disjunct of `opencode_plugin_wanted`, and the
/// predicate's own doc warns that a new disjunct without a matching
/// `spawn_inject_sig` input changes what a fresh tab writes with no restart
/// hint. Both halves are asserted here, because they live in different
/// functions and only their sum is correct.
#[test]
fn checkpoints_alone_keep_the_opencode_plugin_on_disk_and_move_the_signature() {
    let mut s = Settings {
        tabs: vec![opencode_tab_inheriting()],
        ..Settings::default()
    };
    let id = match &s.tabs[0] {
        TabConfig::AiTool(c) => c.id.clone(),
        _ => unreachable!(),
    };
    s.graph.enabled = false;
    s.set_native_web_mode_for_test(NativeWebMode::Off);
    // The Phase H gate is a disjunct of its own and ships on since V39.
    s.set_l2_for_test(
        crate::settings::injection::Feature::HarnessNativeGate,
        false,
    );
    assert!(!opencode_plugin_wanted(&s, &id), "the baseline");
    let before = spawn_inject_sig(&s);

    s.workbench.checkpoints = true;
    assert!(
        opencode_plugin_wanted(&s, &id),
        "checkpoints alone must keep the file on disk — otherwise an \
             OpenCode tab with the graph off silently loses its rewind points"
    );
    assert_ne!(
        spawn_inject_sig(&s)[&h("opencode")],
        before[&h("opencode")],
        "…and the flip is spawn-baked, so it owes the tab a restart hint"
    );
}
