//! `harness::opencode::config`'s unit tests — the `OPENCODE_CONFIG_CONTENT`
//! object a tab is spawned with, plus the server-auth pair beside it.
//!
//! Fifteen of these arrived from `tabs::config`'s test module in #132's second
//! pass: each reached the emitted config only through [`build_opencode_config`]
//! and touched no `tabs::config` item. The ones that drive the same object
//! through `compose_ai_env` or `spawn_inject_sig` stayed there, with the
//! composition they are about.

use super::*;
use crate::harness::fixtures::*;
use std::collections::HashMap;

// The four below were always here — pure functions over a string. Everything
// after them arrived in #132: what still lives in `tabs::config`'s test module
// is what drives this object through the tab-spawn composition
// (`compose_ai_env`, `spawn_inject_sig`) rather than through
// `build_opencode_config`, plus the assertions that span BOTH harnesses'
// emitters, which no single harness owns.

/// The generated password can never be the value that DISABLES auth.
#[test]
fn a_generated_server_password_is_never_empty_and_never_repeats() {
    let a = new_server_password();
    let b = new_server_password();
    assert!(!a.is_empty(), "an empty password disables auth upstream");
    assert_eq!(a.len(), 32, "32 hex chars of UUIDv4 entropy: {a}");
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    assert_ne!(a, b, "the password must be per spawn, not per build");
}

/// The header is `Basic base64("opencode:<password>")` — and an empty
/// password yields NO header rather than a header for the empty string,
/// because upstream reads an empty password as "auth off".
#[test]
fn the_basic_header_encodes_the_username_pair_and_refuses_an_empty_password() {
    use base64::prelude::*;
    let header = server_basic_auth("s3cret").expect("a non-empty password has a header");
    let encoded = header
        .strip_prefix("Basic ")
        .expect("the scheme is Basic, not Bearer");
    assert_eq!(
        String::from_utf8(BASE64_STANDARD.decode(encoded).expect("base64")).expect("utf8"),
        format!("{SERVER_USERNAME}:s3cret"),
    );
    assert_eq!(server_basic_auth(""), None);
}

/// The reader's credential comes from the child's COMPOSED environment, so a
/// per-tab override (which wins at spawn) cannot leave the tap
/// authenticating with a password the server never saw.
#[test]
fn the_readers_credential_follows_the_childs_effective_environment() {
    let mut env: HashMap<String, String> = HashMap::new();
    assert_eq!(server_auth_from_env(&env), None, "no variable ⇒ no header");
    env.insert(SERVER_PASSWORD_ENV.to_string(), String::new());
    assert_eq!(
        server_auth_from_env(&env),
        None,
        "an empty password disables auth upstream, so it must not produce a header"
    );
    env.insert(SERVER_PASSWORD_ENV.to_string(), "theirs".to_string());
    assert_eq!(
        server_auth_from_env(&env),
        server_basic_auth("theirs"),
        "the tap must authenticate with the password the CHILD will use"
    );
}

/// The two variables are set as a pair, with the username pinned to the
/// value the header is built from.
#[test]
fn the_spawn_env_pairs_the_password_with_the_username_it_is_encoded_under() {
    let pairs = server_auth_env("pw");
    assert_eq!(
        pairs,
        [
            (SERVER_PASSWORD_ENV.to_string(), "pw".to_string()),
            (SERVER_USERNAME_ENV.to_string(), SERVER_USERNAME.to_string()),
        ]
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

/// V32 Phase D (locked decision 8) — the permission block is pinned
/// unconditionally, with the values OpenCode 1.18.13 effectively defaults
/// to. Like `subagent_depth` it derives from no setting, so it must be
/// present in the barest possible config as well as a maximal one.
#[test]
fn opencode_config_pins_the_permission_block() {
    for settings in [Settings::default(), {
        let mut s = Settings::default();
        s.offload.enabled = true;
        s.graph.enabled = true;
        s.code_audit.enabled = true;
        s.harness_row("opencode").expose_code_audit = true;
        s
    }] {
        let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
        let perm = &cfg["agent"]["build"]["permission"];
        assert_eq!(
            perm,
            &serde_json::json!({
                "bash": "allow",
                "edit": "allow",
                // #48 (M-16): the `read` carve-out, restated verbatim. Its
                // ORDER is asserted separately, on the serialized text —
                // a `Value` comparison cannot see it.
                "read": {
                    "*": "allow",
                    "*.env": "ask",
                    "*.env.*": "ask",
                    "*.env.example": "allow",
                },
                "webfetch": "allow",
                "websearch": "allow",
            }),
            "the pinned permission block must be present verbatim; got {cfg:#}",
        );
    }
}

/// The pin lives under `agent.build`, NOT at the top level, and that
/// placement is load-bearing rather than stylistic: OpenCode merges a
/// top-level `permission` block last into EVERY native agent's ruleset, so
/// a top-level pin would override `plan`'s `edit: deny` and the
/// `"*": "deny"` of `explore`/`compaction`/`title`/`summary` — handing
/// restricted agents back bash/edit/webfetch. Pinning must freeze today's
/// behaviour, never loosen it.
#[test]
fn opencode_permission_pin_does_not_leak_to_the_restricted_native_agents() {
    let cfg = build_opencode_config(&opencode_cfg(), &Settings::default(), "opencode");
    assert!(
        cfg.get("permission").is_none(),
        "a TOP-LEVEL permission block de-restricts plan/explore/compaction/title/summary; \
             pin per-agent instead. Got: {cfg:#}",
    );
    let agents = cfg["agent"].as_object().expect("agent is an object");
    assert_eq!(
        agents.keys().collect::<Vec<_>>(),
        vec!["build"],
        "only the default primary agent is pinned",
    );
}

/// The pinned values must stay a restatement of upstream's effective
/// defaults (the milestone locks that they are PINNED, not that behaviour
/// changes). Choosing something stricter — `webfetch: "ask"` is the
/// documented candidate — is a deliberate decision with the user, and this
/// test is where that decision gets recorded: change the consts AND this
/// assertion together, never one alone.
#[test]
fn pinned_permission_values_restate_opencode_1_18_13_defaults() {
    for (name, value) in [
        ("bash", OPENCODE_PINNED_BASH),
        ("edit", OPENCODE_PINNED_EDIT),
        ("webfetch", OPENCODE_PINNED_WEBFETCH),
        ("websearch", OPENCODE_PINNED_WEBSEARCH),
        // #48 (M-16): the two `read` values that resolve through the base
        // `"*": "allow"` rule, same as the four above.
        ("read *", OPENCODE_PINNED_READ_ANY),
        ("read *.env.example", OPENCODE_PINNED_READ_ENV_EXAMPLE),
    ] {
        assert_eq!(
            value, "allow",
            "{name}: OpenCode 1.18.13 resolves this through its `\"*\": \"allow\"` base rule. \
                 Changing it here changes how the user's OpenCode tab behaves — update the \
                 rationale comment in `build_opencode_config` in the same edit.",
        );
    }
    // …and the carve-out itself, which is the ONE pinned value that is not
    // "allow" — it is upstream's `*.env` → ask, restated.
    assert_eq!(
        OPENCODE_PINNED_READ_ENV, "ask",
        "OpenCode 1.18.13 asks before reading `*.env` / `*.env.*`; pinning this to \
             anything else DELETES a secret-file protection rather than freezing one",
    );
}

/// #48 (M-16) — the pinned `read` rule, and the ORDER that makes it a rule
/// rather than a decoration.
///
/// OpenCode evaluates permission rules **last-match-wins**, so `"*"` must be
/// emitted FIRST (or it re-allows everything after the carve-out) and
/// `"*.env.example"` LAST (`"*.env.*"` also matches it). `serde_json`
/// preserves insertion order in this build via the transitive
/// `preserve_order` feature — a fact no `Cargo.toml` in this repo declares —
/// so this asserts the SERIALIZED text, which is what OpenCode actually
/// parses, rather than a `Value` comparison that cannot see order at all.
///
/// The finding: Phase D left `read` unpinned, and a cloned repo shipping
/// `{"permission":{"read":"allow"}}` resolved `read * → allow` and read
/// `.env` with no prompt (verified live).
#[test]
fn the_pinned_read_rule_keeps_the_env_carve_out_in_wildcard_first_order() {
    let cfg = build_opencode_config(&opencode_cfg(), &Settings::default(), "opencode");
    let read = &cfg["agent"]["build"]["permission"]["read"];
    assert_eq!(
        serde_json::to_string(read).expect("serializes"),
        r#"{"*":"allow","*.env":"ask","*.env.*":"ask","*.env.example":"allow"}"#,
        "the pinned `read` rule must emit wildcard-first, `*.env.example` last — \
             last-match-wins makes the ORDER the protection. If this fails with the right \
             pairs in the wrong order, `serde_json`'s `preserve_order` feature is no longer \
             enabled in this build and `opencode_pinned_read` needs a different representation.",
    );
    // The escape hatch is unchanged: hygiene off ⇒ no pin at all.
    let mut off = Settings::default();
    off.set_l2_for_test(crate::settings::injection::Feature::ConsumerHygiene, false);
    assert!(
        build_opencode_config(&opencode_cfg(), &off, "opencode")["agent"].is_null(),
        "consumer hygiene off must restore the pre-V32 posture, `read` included",
    );
}

/// The OpenCode half of `deny`: the Phase D pinned block flips the two WEB
/// values and nothing else. `bash`/`edit` keep their pins in every mode —
/// shell egress is V33's honest limit, and taking `edit` away would gut the
/// tab.
#[test]
fn deny_mode_flips_only_the_web_keys_of_the_pinned_opencode_block() {
    let perm_for = |mode: &str| -> serde_json::Value {
        let mut s = Settings::default();
        s.set_native_web_mode_for_test(NativeWebMode::parse(mode));
        build_opencode_config(&opencode_cfg(), &s, "opencode")["agent"]["build"]["permission"]
            .clone()
    };
    assert_eq!(
        perm_for("deny"),
        serde_json::json!({
            "bash": OPENCODE_PINNED_BASH,
            "edit": OPENCODE_PINNED_EDIT,
            // #48 (M-16): identical in all four modes — that is what makes
            // "only the web keys flip" a real claim rather than a slogan.
            "read": opencode_pinned_read(),
            "webfetch": "deny",
            "websearch": "deny",
        })
    );
    for mode in ["off", "sensor", "nonsense"] {
        assert_eq!(
            perm_for(mode),
            serde_json::json!({
                "bash": OPENCODE_PINNED_BASH,
                "edit": OPENCODE_PINNED_EDIT,
                "read": opencode_pinned_read(),
                "webfetch": OPENCODE_PINNED_WEBFETCH,
                "websearch": OPENCODE_PINNED_WEBSEARCH,
            }),
            "{mode} must leave the Phase D pins alone"
        );
    }
}

#[test]
fn opencode_config_references_instructions_when_guidance_applies() {
    // V20: TTS markup is no longer injected, so the instructions file is
    // referenced only when capability guidance (graph/offload) applies.
    let mut settings = Settings::default();
    settings.graph.enabled = true;
    let cfg = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    let path = cfg["instructions"][0].as_str().expect("instructions path");
    assert!(path.ends_with(".md"), "got: {path}");
    assert!(path.contains("opencode"), "got: {path}");
}

#[test]
fn opencode_config_no_instructions_when_no_guidance() {
    // V20: no guidance ⇒ no instructions key, regardless of the
    // (now-vestigial) tts_injection.
    //
    // V37 Phase F: default settings are no longer a no-guidance case — the
    // injection-hygiene contract paragraph rides every tab now (see
    // `injection_hygiene_applies`), so the empty case is "hygiene off, every
    // feature off", which is what this asserts. The managed-tool steering
    // paragraph is the second always-on addendum and has to be switched off
    // here for the same reason.
    let mut settings = Settings::default();
    settings.set_l2_for_test(
        crate::settings::injection::Feature::ConsumerHygiene,
        false,
    );
    settings.set_l2_for_test(crate::settings::injection::Feature::ToolSteering, false);
    let config = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    assert!(
        config.get("instructions").is_none(),
        "no guidance ⇒ no instructions key"
    );
}

#[test]
fn opencode_config_no_provider_when_unregistered() {
    // With no `local-llama` registered, cimp injects no `provider`/`model`
    // block — regardless of the per-tab `use_local_provider` flag (which
    // drives Claude's env synthesis, not OpenCode's config).
    let settings = Settings::default();
    let mut cfg = opencode_cfg();
    cfg.use_local_provider = true;
    let config = build_opencode_config(&cfg, &settings, "opencode");
    assert!(
        config.get("provider").is_none(),
        "no registration ⇒ no provider block"
    );
    assert!(config.get("model").is_none());
}

#[test]
fn opencode_config_injects_registered_local_provider() {
    // A registered snapshot ⇒ a `provider.local-llama` block pointing at the
    // local endpoint + `model` selecting it, so the tab is ready on open.
    let mut settings = Settings::default();
    settings.set_ext(
        "opencode",
        "provider",
        serde_json::to_value(crate::settings::LocalProviderBlock {
        base_url: "http://127.0.0.1:8080/v1".to_string(),
        model: "Qwen3-Q4".to_string(),
        api_key: String::new(),
        source_command: "llama-server -m Qwen3-Q4.gguf --port 8080".to_string(),
        })
        .expect("provider serializes"),
    );
    let config = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    let prov = &config["provider"]["local-llama"];
    assert_eq!(prov["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(prov["options"]["baseURL"], "http://127.0.0.1:8080/v1");
    assert!(
        prov["models"]["Qwen3-Q4"].is_object(),
        "model listed in provider"
    );
    assert_eq!(config["model"], "local-llama/Qwen3-Q4");
    assert!(
        prov["options"].get("apiKey").is_none(),
        "no apiKey key when the command carried none",
    );
}

#[test]
fn opencode_config_auto_derives_provider_from_backend() {
    // Auto-sync on + offload enabled ⇒ derive the provider live from the
    // primary Local backend's command, even with no stored snapshot.
    let mut settings = Settings::default();
    settings.offload.enabled = true;
    settings.set_ext("opencode", "provider_auto", serde_json::json!(true));
    settings.offload.backends = vec![crate::settings::OffloadBackend {
        name: "local".to_string(),
        enabled: true,
        kind: crate::settings::OffloadBackendKind::Local {
            server_command: "llama-server -a my-model --port 9001 --jinja".to_string(),
            autostart: false,
            show_command_on_start: false,
            auth_token: String::new(),
        },
        ..Default::default()
    }];
    let config = build_opencode_config(&opencode_cfg(), &settings, "opencode");
    assert_eq!(
        config["provider"]["local-llama"]["options"]["baseURL"],
        "http://127.0.0.1:9001/v1"
    );
    assert_eq!(config["model"], "local-llama/my-model");
}
