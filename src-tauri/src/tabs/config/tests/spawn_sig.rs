//! `spawn_inject_sig` — the spawn-baked settings signature and the restart hint
//! it feeds. A setting that changes an artifact a running tab already carries
//! must move this map, and one that changes nothing spawn-time must not.

use super::*;

/// **The Phase B shape change raises no spurious restart hint** (locked
/// decision 8).
///
/// `spawn_inject_sig` went from a positional `PerHarness<Value>` to a
/// `BTreeMap<HarnessId, Value>` and grew an automatic `"ext"` half built
/// from every `spawn_baked` field a plugin declares. The hazard in that is
/// invisible: if the refactor changed any VALUE for an unchanged settings
/// file, the first save after an upgrade would tell the user to restart
/// every AI tab for a change nobody made — and a hint that fires for
/// nothing is a hint nobody reads, which is the rule this mechanism's own
/// docs state.
///
/// So: a fixed fixture, and the exact per-harness objects it must produce,
/// written out. This is a GOLDEN, deliberately: a diff here is a real
/// answer changing, and the reviewer's job is to say whether the user owes
/// a restart for it.
#[test]
fn the_spawn_signature_is_pinned_for_a_fixed_settings_fixture() {
    let sig = spawn_inject_sig(&Settings::default());

    assert_eq!(
        sig[&h("claude")],
        serde_json::json!({
            "mcp": [false],
            "guidance": crate::harness::plugin::guidance_gates(&Settings::default()),
            "sandbox": crate::harness::plugin::sandbox_gates(&Settings::default()),
            "hooks": [false, false, false, false, false, false],
            "notify_hooks": false,
            "local_env": null,
            "channels": false,
            "injection": crate::settings::injection::spawn_sig(
                &Settings::default(),
                h("claude"),
            ),
            // The automatic half: this harness's `spawn_baked` declarations
            // that reach a launch, at their declared defaults. `statusline`
            // used to be a hand-written `"statusline"` key on the object
            // above and is covered by the declaration now.
            //
            // The three `local.*` rows are ABSENT, and that is the answer,
            // not an omission (V40 review M-4, parity lens): they are
            // synthesized into `ANTHROPIC_*` only for a tab that opted into
            // the local provider, so with no such tab in this fixture they
            // reach no launch and editing the proxy URL must raise no hint —
            // which is exactly what `local_env: null` above says. They
            // rejoin the object the moment a tab opts in
            // (`a_local_provider_tab_brings_the_local_rows_into_the_signature`).
            "ext": { "statusline": true },
        }),
        "the Claude spawn signature moved for a DEFAULT settings file"
    );

    assert_eq!(
        sig[&h("opencode")]["ext"],
        serde_json::json!({
            "native_gate": true,
            "provider_auto": false,
            "provider": null,
        }),
        "the OpenCode ext half"
    );

    // Every registered harness has a slot, and none of them is `null`: a
    // harness with no signature gets no restart hint at all, which is the
    // failure the map replaced the positional pair to prevent.
    for harness in crate::harness::registry::all() {
        let slot = sig.get(&harness).expect("every harness has a slot");
        assert!(!slot.is_null(), "{harness} has an empty spawn signature");
    }
}

/// **A flip names ONLY the harness it can reach** — the other half of the
/// hint's contract, driven through the declared `ext` rows since those are
/// the ones a plugin can now add without touching this file.
#[test]
fn an_ext_flip_moves_only_the_declaring_harnesss_slot() {
    let base = spawn_inject_sig(&Settings::default());

    // The `local.*` rows reach a launch only for a tab that opted in (V40
    // review M-4), so this fixture carries one — otherwise the flip
    // correctly moves NOTHING and the test would be asserting the opposite
    // of `a_local_url_edit_with_no_local_tab_raises_no_hint` below.
    let mut with_local = Settings::default();
    with_local.tabs.push(crate::settings::default_claude_local_tab());
    let base_local = spawn_inject_sig(&with_local);

    for (id, key, flipped, fixture, baseline) in [
        ("claude", "statusline", serde_json::json!(false), Settings::default(), &base),
        (
            "claude",
            "local.base_url",
            serde_json::json!("http://elsewhere:1"),
            with_local.clone(),
            &base_local,
        ),
        ("opencode", "native_gate", serde_json::json!(false), Settings::default(), &base),
        ("opencode", "provider_auto", serde_json::json!(true), Settings::default(), &base),
    ] {
        let base = baseline;
        let mut s = fixture;
        s.set_ext(id, key, flipped);
        let sig = spawn_inject_sig(&s);
        let moved: Vec<&str> = sig
            .iter()
            .filter(|(harness, v)| base.get(*harness) != Some(*v))
            .filter_map(|(harness, _)| harness.id())
            .collect();
        assert_eq!(
            moved,
            vec![id],
            "flipping `{id}.ext.{key}` must move exactly that harness's slot"
        );
    }
}

/// **A spawn-baked value that reaches no launch raises no hint** (V40
/// review finding M-4, parity lens).
///
/// The local-provider rows are synthesized into `ANTHROPIC_*` only for a tab
/// that opted in. Before V40 they had no signature entry of their own — they
/// rode the gated `local_env` element — so editing the proxy URL with no
/// such tab open was correctly silent. Declaring them `spawn_baked` made
/// core fold them in unconditionally, and a hint that fires for a change
/// that changes nothing is a hint nobody reads.
#[test]
fn a_local_url_edit_with_no_local_tab_raises_no_hint() {
    let claude = h("claude");
    let keys = crate::harness::claude::settings::LOCAL_KEYS;

    // No local-provider tab: the rows are absent and an edit moves nothing.
    let base = spawn_inject_sig(&Settings::default());
    for key in keys {
        assert!(
            base[&claude]["ext"].get(*key).is_none(),
            "`{key}` reaches no launch here and must not be in the signature"
        );
        let mut s = Settings::default();
        s.set_ext("claude", key, serde_json::json!("http://elsewhere:1"));
        assert_eq!(
            spawn_inject_sig(&s),
            base,
            "editing `{key}` with no local-provider tab must raise no restart hint"
        );
    }

    // …and the moment a tab opts in, every one of them is back and live.
    let mut opted = Settings::default();
    opted.tabs.push(crate::settings::default_claude_local_tab());
    let with_tab = spawn_inject_sig(&opted);
    assert_ne!(
        with_tab[&claude], base[&claude],
        "opting a tab into the local provider IS a spawn-time change"
    );
    for key in keys {
        assert!(
            with_tab[&claude]["ext"].get(*key).is_some(),
            "`{key}` must rejoin the signature once a tab opts in"
        );
        let mut s = opted.clone();
        s.set_ext("claude", key, serde_json::json!("http://elsewhere:1"));
        assert_ne!(
            spawn_inject_sig(&s)[&claude],
            with_tab[&claude],
            "editing `{key}` with a local-provider tab open MUST raise the hint"
        );
    }
}

/// The mask and the schema name the same rows.
///
/// `LOCAL_KEYS` is what `spawn_baked_reaches_a_launch` filters on; a key
/// that fell out of it would be folded into the signature unconditionally
/// again, silently.
#[test]
fn the_local_keys_are_exactly_the_declared_local_rows() {
    use crate::harness::plugin::HarnessPlugin as _;
    let declared: Vec<&str> = crate::harness::claude::plugin::PLUGIN
        .settings_schema()
        .iter()
        .map(|f| f.key)
        .filter(|k| k.starts_with("local."))
        .collect();
    assert_eq!(
        declared,
        crate::harness::claude::settings::LOCAL_KEYS.to_vec(),
        "`LOCAL_KEYS` and the declared `local.*` rows must be the same list"
    );
    assert!(!declared.is_empty(), "this harness declares no local rows");
}

/// H2: the hooks are Settings-DEPENDENT and baked at spawn, so
/// `spawn_inject_sig` must carry them — otherwise enabling graph/offload
/// mid-session leaves every running Claude tab permanently hook-blind with
/// no restart hint.
#[test]
fn permission_hooks_have_a_spawn_inject_sig_entry() {
    let settings = Settings::default();
    let sig = spawn_inject_sig(&settings);
    let hooks = sig[&h("claude")]["hooks"].as_array().expect("claude hooks sig array");
    // The six GATED hook entries (five, plus V33 Phase F's pre-mutation
    // checkpoint beacon) — all off by default.
    assert_eq!(hooks.len(), 6, "unexpected hook-gate count: {hooks:?}");
    assert!(hooks.iter().all(|g| g == &serde_json::Value::Bool(false)));
    // The NC-2 pair rides its own key and tracks `loopback_needed()`.
    assert_eq!(sig[&h("claude")]["notify_hooks"], serde_json::json!(false));
    let mut with_graph = Settings::default();
    with_graph.graph.enabled = true;
    let sig2 = spawn_inject_sig(&with_graph);
    assert_eq!(sig2[&h("claude")]["notify_hooks"], serde_json::json!(true));
    assert_ne!(sig[&h("claude")], sig2[&h("claude")], "the flip must change the signature");

    // V33 Phase F: `workbench.checkpoints` alone must move the signature.
    // It is the half no other entry carries — the UserPromptSubmit slot
    // reads it only ANDed with `graph.enabled`, so on a graph-off install
    // that slot is pinned `false` and a checkpoint flip would have been
    // invisible without the dedicated entry.
    let mut with_cp = Settings::default();
    with_cp.workbench.checkpoints = true;
    assert!(
        !with_cp.graph.enabled,
        "the point of this case is a graph-OFF install"
    );
    let sig3 = spawn_inject_sig(&with_cp);
    assert_ne!(
        sig[&h("claude")], sig3[&h("claude")],
        "toggling workbench.checkpoints must raise the restart hint — the \
             PreToolUse checkpoint beacon is baked at spawn"
    );
}

/// V39 Phase A, locked decision 15: **the read-only lock and the
/// `delegation` block are NOT spawn-baked.**
///
/// The lock is enforced per write in `pty_write`, and the delegation knobs
/// are read when a delegation runs — none of them changes how a tab is
/// launched. If either moved this signature, flipping "Read-only" from the
/// tab's own popover would raise a "restart the AI tab" hint for a switch
/// that takes effect on the very next keystroke, and a restart hint that
/// fires for nothing is how a restart hint stops being read.
#[test]
fn read_only_and_delegation_do_not_move_the_spawn_signature() {
    use crate::settings::DelegationRole;
    let mut base = Settings::default();
    base.tabs.push(claude_tab_inheriting());
    let sig = spawn_inject_sig(&base);

    let mut locked = Settings::default();
    locked.tabs.push(claude_tab_inheriting());
    for cfg in locked.tabs.iter_mut() {
        if let TabConfig::AiTool(c) = cfg {
            c.read_only = true;
        }
    }
    assert!(
        locked
            .tabs
            .iter()
            .any(|t| matches!(t, TabConfig::AiTool(c) if c.read_only)),
        "the fixture must actually have flipped a tab, or this asserts nothing"
    );
    assert_eq!(
        sig,
        spawn_inject_sig(&locked),
        "locking a tab read-only must not ask the user to restart it"
    );

    let mut delegation = Settings::default();
    delegation.tabs.push(claude_tab_inheriting());
    delegation.delegation.auto_read_only = !base.delegation.auto_read_only;
    delegation.delegation.default_timeout_s = base.delegation.default_timeout_s + 30;
    delegation.delegation.max_depth = base.delegation.max_depth + 1;
    assert_ne!(
        base.delegation, delegation.delegation,
        "the fixture must actually differ"
    );
    assert_eq!(
        sig,
        spawn_inject_sig(&delegation),
        "the delegation settings are not baked into any tab's launch"
    );

    // V39 Phase B extends the same claim to the per-tab ROLE (locked
    // decision 15, and live-verify 9 checks the same thing in the app):
    // the `delegate_task_*` set rides the child proxy's live `tools/list`
    // plus the V37 `list_changed` pulse, and the facade rides
    // `offload_task`'s live description — so a role change takes effect on
    // the next turn and must NOT raise a restart hint. This is the half a
    // reader is most likely to assume is spawn-baked, because it changes
    // what a running child advertises.
    for role in [DelegationRole::Manual, DelegationRole::RemoteOffload] {
        let mut roled = Settings::default();
        roled.tabs.push(claude_tab_inheriting());
        for cfg in roled.tabs.iter_mut() {
            if let TabConfig::AiTool(c) = cfg {
                c.delegation_role = role;
                c.delegation_backend.name = Some("lan-worker-2".to_string());
                c.delegation_backend.declared_context = Some(128_000);
            }
        }
        assert!(
            roled
                .tabs
                .iter()
                .any(|t| matches!(t, TabConfig::AiTool(c) if c.delegation_role == role)),
            "the fixture must actually have set the role, or this asserts nothing"
        );
        assert_eq!(
            sig,
            spawn_inject_sig(&roled),
            "setting the {role:?} delegation role must not ask the user to restart the tab"
        );
    }

    // …and the input-profile spike outcome is not spawn-baked either: it
    // gates the surface at list time, not at launch.
    let mut spiked = Settings::default();
    spiked.tabs.push(claude_tab_inheriting());
    spiked.harness_row("claude").input_profile_status = "fail".to_string();
    assert_eq!(sig, spawn_inject_sig(&spiked));
}

/// Granting, revoking and DEACTIVATING an MCP server changes nothing a tab
/// bakes in — the companion to the `spawn_inject_sig` assertions, at the
/// artifact level rather than the signature level.
#[test]
fn mcp_toggles_change_no_spawn_time_artifact() {
    let mut base = Settings::default();
    base.graph.enabled = true; // loopback already needed — isolate the MCP axis
    base.offload.mcp_servers = vec![crate::settings::McpServerConfig {
        name: "ddg".to_string(),
        ..Default::default()
    }];
    let claude_before = build_pre_args(&claude_cfg(), &base, "claude", Some(&hook_endpoint()));
    let oc_before = build_opencode_config(&opencode_cfg(), &base, "opencode");
    for mutate in [
        |m: &mut crate::settings::McpServerConfig| {
            m.access = crate::settings::access_for_test(&[("claude", true)]);
        },
        |m: &mut crate::settings::McpServerConfig| {
            m.access = crate::settings::access_for_test(&[("opencode", true)]);
        },
        |m: &mut crate::settings::McpServerConfig| m.offload_access = true,
        |m: &mut crate::settings::McpServerConfig| m.enabled = false,
    ] {
        let mut after = base.clone();
        mutate(&mut after.offload.mcp_servers[0]);
        assert_eq!(
            build_pre_args(&claude_cfg(), &after, "claude", Some(&hook_endpoint())),
            claude_before,
            "an MCP toggle must not change Claude's spawn artifact"
        );
        assert_eq!(
            build_opencode_config(&opencode_cfg(), &after, "opencode"),
            oc_before,
            "an MCP toggle must not change OpenCode's spawn artifact"
        );
    }
}

/// Spawn-baked ⇒ `spawn_inject_sig` entry ⇒ restart hint. All three modes
/// act only at tab launch, so flipping one while tabs are running must move
/// BOTH consumers' signatures — a tab that launched in `off` stays blind
/// until it restarts, and the user is owed that hint.
#[test]
fn native_web_visibility_moves_the_spawn_inject_signature() {
    let base = spawn_inject_sig(&Settings::default());
    for mode in ["off", "deny"] {
        let mut s = Settings::default();
        s.set_native_web_mode_for_test(NativeWebMode::parse(mode));
        let sig = spawn_inject_sig(&s);
        assert_ne!(sig[&h("claude")], base[&h("claude")], "claude signature must move for {mode}");
        assert_ne!(sig[&h("opencode")], base[&h("opencode")], "opencode signature must move for {mode}");
        // V32 Phase G: the mode moved out of a top-level `native_web` key
        // and into the `injection` fragment, where it sits as the
        // native-web feature's L2 alongside the master switch, the
        // consumer-hygiene flag and every tab's resolved posture. #48 keyed
        // that `l2` array by feature (it is per-consumer now, so a
        // positional index would silently mean a different control on the
        // Claude side than on the OpenCode side).
        //
        // #48 (M-3): SEARCHED, not indexed. `l2` is built in `Feature::ALL`
        // declaration order over the spawn-baked set, and spotlighting
        // joining that set moved `native_web` off index 0 — a positional
        // read here would have failed for a reason that has nothing to do
        // with native-web visibility.
        for consumer in [h("claude"), h("opencode")] {
            let l2 = sig[&consumer]["injection"]["l2"]
                .as_array()
                .expect("the l2 array")
                .clone();
            assert!(
                l2.contains(&serde_json::json!(["native_web", mode])),
                "consumer {consumer}: {l2:?}"
            );
        }
    }
}

/// V32 Phase G: the OTHER two levels of the same spawn-baked features move
/// the signature too — a per-tab override and the global master, neither of
/// which existed when the test above was written.
#[test]
fn the_injection_hierarchy_moves_the_spawn_inject_signature_at_every_level() {
    let with_tab = || Settings {
        tabs: vec![claude_tab_inheriting()],
        ..Settings::default()
    };
    let base = spawn_inject_sig(&with_tab());
    // L1.
    let mut s = with_tab();
    s.set_master_for_test(false);
    assert_ne!(spawn_inject_sig(&s)[&h("claude")], base[&h("claude")], "the master switch");
    // L2 for consumer hygiene (native-web's L2 is covered above).
    let mut s = with_tab();
    s.set_l2_for_test(crate::settings::injection::Feature::ConsumerHygiene, false);
    assert_ne!(spawn_inject_sig(&s)[&h("opencode")], base[&h("opencode")], "consumer hygiene L2");
    // L3, per tab, for every spawn-baked feature that HAS a tab cell.
    // Derived, not hand-listed (#48, M-3): a hand-list is how spotlighting
    // stayed out of this test for a whole milestone.
    //
    // Two things the hand-list hid, both of which the derivation forces into
    // the open. BOTH consumers have to be configured, because the set is not
    // uniform — Phase H's OpenCode gate reaches only one of them, so a
    // Claude-only fixture would demand a signature move that cannot happen.
    // And the override has to FLIP the resolved value: `spawn_sig` carries
    // resolved booleans, so `Off` over a default-off control (the gate) is
    // not a change at all.
    let with_both = || Settings {
        tabs: vec![claude_tab_inheriting(), opencode_tab_inheriting()],
        ..Settings::default()
    };
    let base_both = spawn_inject_sig(&with_both());
    let spawn_baked_with_tab_scope: Vec<_> = crate::settings::injection::Feature::ALL
        .iter()
        .copied()
        .filter(|f| f.spawn_baked() && f.has_tab_scope())
        .collect();
    assert!(
        spawn_baked_with_tab_scope.len() >= 3,
        "expected at least native-web, consumer-hygiene and spotlighting; \
             got {spawn_baked_with_tab_scope:?}"
    );
    for feature in spawn_baked_with_tab_scope {
        let flip = if feature.default_enabled() {
            crate::settings::injection::Override::Off
        } else {
            crate::settings::injection::Override::On
        };
        let mut s = with_both();
        for i in 0..2 {
            let id = ai_tab_id(&s, i);
            s.set_tab_override_for_test(&id, feature, flip)
                .expect("a spawn-baked, tab-scoped feature carries a tab cell");
        }
        assert_ne!(spawn_inject_sig(&s), base_both, "{feature:?} L3");
    }
    // A LIVE feature must not move it — the restart hint is only honest if
    // it fires for changes that actually need a restart.
    let mut s = with_tab();
    s.set_l2_for_test(crate::settings::injection::Feature::TaintLatch, false);
    s.set_l2_for_test(crate::settings::injection::Feature::Detection, false);
    assert_eq!(spawn_inject_sig(&s), base, "live features must not nag");
}

/// Spawn-baked: the gate's flag is compiled into the plugin, so a flip at
/// EITHER level must move the OpenCode spawn signature and raise the restart
/// hint. A gate the user believes is on, in a tab that launched without it,
/// is the failure this pins.
#[test]
fn a_native_gate_flip_raises_the_restart_hint_at_both_levels() {
    let base = Settings {
        tabs: vec![opencode_tab_inheriting()],
        ..Settings::default()
    };
    let before = spawn_inject_sig(&base);

    // Both flips move AWAY from the shipping value, whatever that value is:
    // the property is "a flip at either level moves the signature", and a
    // write of the value already stored proves nothing.
    let mut l2 = base.clone();
    l2.set_l2_for_test(
        crate::settings::injection::Feature::HarnessNativeGate,
        false,
    );
    assert_ne!(spawn_inject_sig(&l2)[&h("opencode")], before[&h("opencode")], "L2 flip");

    let mut l3 = base.clone();
    let id = ai_tab_id(&l3, 0);
    l3.set_tab_override_for_test(
        &id,
        crate::settings::injection::Feature::HarnessNativeGate,
        crate::settings::injection::Override::Off,
    )
    .expect("the OpenCode tab carries a native-gate cell");
    assert_ne!(spawn_inject_sig(&l3)[&h("opencode")], before[&h("opencode")], "L3 flip");
}

/// The restart-hint edge (`update_settings`) compares this signature
/// across a save — it must move on every spawn-baked setting, stay put on
/// live-applied tuning, and stay per-consumer where injection is.
#[test]
fn spawn_inject_sig_tracks_spawn_time_settings() {
    let base = spawn_inject_sig(&Settings::default());

    // Claude-only: the `--settings` statusline overlay. Flipped relative
    // to the default (it ships enabled), not hardcoded.
    let mut s = Settings::default();
    let was = s.harness_ext(h("claude"), "statusline").as_bool().expect("a bool row");
    s.set_ext("claude", "statusline", serde_json::json!(!was));
    let sig = spawn_inject_sig(&s);
    assert_ne!(sig[&h("claude")], base[&h("claude")], "statusline flip must move the Claude sig");
    assert_eq!(sig[&h("opencode")], base[&h("opencode")], "statusline is Claude-only");

    // Both consumers: guidance + MCP + plugin follow the graph toggle.
    let mut s = Settings::default();
    s.graph.enabled = true;
    let with_graph = spawn_inject_sig(&s);
    assert_ne!(with_graph[&h("claude")], base[&h("claude")]);
    assert_ne!(with_graph[&h("opencode")], base[&h("opencode")]);

    // Both consumers: context injection = Claude hook gate + the
    // OpenCode plugin's baked CIMP_INJECT_ENABLED flag.
    s.graph.context_injection = true;
    let sig = spawn_inject_sig(&s);
    assert_ne!(sig[&h("claude")], with_graph[&h("claude")]);
    assert_ne!(sig[&h("opencode")], with_graph[&h("opencode")]);

    // The checkpoint gates. Claude: the prompt-hook gate widens (injection
    // off) AND V33 Phase F's pre-mutation `PreToolUse` beacon appears.
    // OpenCode: V33 Phase F gave the plugin its own baked
    // `CIMP_CHECKPOINT_ENABLED` flag, so this consumer moves too — it used
    // to be pinned equal here, on the strength of "the OpenCode plugin
    // always POSTs", which was true only while the prompt tap was the sole
    // checkpoint producer.
    let mut s = Settings::default();
    s.graph.enabled = true;
    s.workbench.checkpoints = true;
    let sig = spawn_inject_sig(&s);
    assert_ne!(sig[&h("claude")], with_graph[&h("claude")], "checkpoints widen the hook gate");
    assert_ne!(
        sig[&h("opencode")], with_graph[&h("opencode")],
        "the plugin's pre-mutation checkpoint flag is baked at spawn, so a \
             checkpoint flip owes an OpenCode tab a restart hint too"
    );

    // `claude_local` edits count only once a Claude tab opted into the
    // local provider.
    let mut s = Settings::default();
    s.set_ext("claude", "local.base_url", serde_json::json!("http://localhost:4000".to_string()));
    assert_eq!(
        spawn_inject_sig(&s)[&h("claude")],
        base[&h("claude")],
        "no tab uses the local provider yet"
    );
    let mut tab = claude_cfg();
    tab.use_local_provider = true;
    s.tabs.push(TabConfig::AiTool(tab));
    assert_ne!(spawn_inject_sig(&s)[&h("claude")], base[&h("claude")]);

    // Claude-only: V30 Phase A session-push registration. This is THE
    // guard demanded by the rule in `spawn_inject_sig` — the flags are baked
    // into argv at spawn, so flipping them must nag running tabs to restart.
    let mut s = Settings::default();
    s.offload.enabled = true;
    let offload_on = spawn_inject_sig(&s);
    s.offload.session_push = true;
    let sig = spawn_inject_sig(&s);
    assert_ne!(
        sig[&h("claude")], offload_on[&h("claude")],
        "session_push must move the Claude sig"
    );
    assert_eq!(
        sig[&h("opencode")], offload_on[&h("opencode")],
        "channels are Claude-only — OpenCode has no MCP inbound path"
    );

    // …and since V37 Phase F it ALWAYS can change argv: the `cimp-offload`
    // server is injected into every AI tab, so there is no longer a
    // "nothing to register against" case to suppress the hint for. This
    // block used to assert the opposite (`bare` ⇒ no sig movement); it is
    // flipped, not deleted, so the reversal is on the record.
    let mut bare = Settings::default();
    bare.offload.enabled = false;
    bare.graph.enabled = false;
    let bare_base = spawn_inject_sig(&bare);
    bare.offload.session_push = true;
    assert_ne!(
        spawn_inject_sig(&bare)[&h("claude")],
        bare_base[&h("claude")],
        "the proxy child always exists ⇒ session_push always changes argv"
    );

    // ── V37 Phase F: MCP toggles are no longer spawn-baked ──────────
    //
    // The point of the phase. An access grant used to move BOTH consumers'
    // signatures through `advertises_offload_to_*`, nagging every open tab
    // to restart for a change that now takes effect where the user is
    // standing. Both cases are swept, because they fail differently:
    //
    //   * `graph on` — the loopback is already needed, so a grant must move
    //     NOTHING. This is the assertion a partial revert of Phase F trips.
    //   * `bare install` — the FIRST grant flips `loopback_needed()`
    //     false→true, and that genuinely changes what a fresh Claude tab
    //     writes (the Notification / PermissionDenied / Stop / SubagentStop
    //     shims appear). So the hint still fires once, honestly, and this
    //     test pins the residual precisely: `notify_hooks` is the ONLY key
    //     allowed to move, and only on that edge. Everything else — the
    //     `"mcp"` slots, `"guidance"`, `"channels"`, the OpenCode half —
    //     must be identical.
    //
    // The MCP tools themselves reach the running tab either way; the hint on
    // that one edge is about the hook shims, not about them.
    for (label, mut base, may_move_notify) in [
        ("bare install", Settings::default(), true),
        (
            "loopback already needed",
            {
                let mut s = Settings::default();
                s.graph.enabled = true;
                s
            },
            false,
        ),
    ] {
        base.offload.mcp_servers = vec![crate::settings::McpServerConfig {
            name: "ddg".to_string(),
            ..Default::default()
        }];
        let before = spawn_inject_sig(&base);
        let mut flips: Vec<Settings> = Vec::new();
        for grant in [
            |m: &mut crate::settings::McpServerConfig| {
                m.access = crate::settings::access_for_test(&[("claude", true)]);
            },
            |m: &mut crate::settings::McpServerConfig| {
                m.access = crate::settings::access_for_test(&[("opencode", true)]);
            },
            |m: &mut crate::settings::McpServerConfig| m.offload_access = true,
        ] {
            let mut after = base.clone();
            grant(&mut after.offload.mcp_servers[0]);
            flips.push(after);
        }
        // DEACTIVATING one is contract C3's live half and touches no
        // spawn-time input at all — not even `loopback_needed()`.
        let mut disabled = base.clone();
        disabled.offload.mcp_servers[0].enabled = false;
        let deactivation = flips.len();
        flips.push(disabled);

        for (n, after) in flips.iter().enumerate() {
            let sig = spawn_inject_sig(after);
            if n == deactivation || !may_move_notify {
                assert_eq!(
                    sig, before,
                    "{label}: an MCP toggle must not raise a restart hint — it \
                         propagates live through the proxy child (flip {n})"
                );
                continue;
            }
            // Every key but `notify_hooks` must be untouched — compared by
            // overwriting that one key, so a NEW moving key fails here.
            let mut normalized = sig.clone();
            normalized
                .get_mut(&h("claude"))
                .expect("claude has a slot")["notify_hooks"] =
                before[&h("claude")]["notify_hooks"].clone();
            assert_eq!(
                normalized, before,
                "{label}: only `notify_hooks` may move on the first grant \
                     (flip {n}) — got {sig:?}"
            );
        }
    }

    // V33 Phase B: the tab sandbox. The most literally spawn-baked setting
    // there is — an OS boundary exists around a process from
    // `CreateProcessW` onward and cannot be added to a running one — so
    // both consumers owe a restart hint. Asserted with the EFFECTIVE
    // semantics, which is where this could silently go wrong.
    let mut s = Settings::default();
    s.sandbox.tabs = true;
    assert_eq!(
        spawn_inject_sig(&s),
        base,
        "`sandbox.tabs` with the master switch off changes no spawn, so it must not nag"
    );
    s.sandbox.enabled = true;
    let sandboxed = spawn_inject_sig(&s);
    assert_ne!(
        sandboxed[&h("claude")], base[&h("claude")],
        "turning tab sandboxing on must move the Claude sig — a running tab cannot be \
             confined retroactively"
    );
    assert_ne!(
        sandboxed[&h("opencode")], base[&h("opencode")],
        "…and the OpenCode sig: the switch is not per-consumer"
    );
    // The grant table is applied during preparation, so editing it cannot
    // widen a boundary that already exists ⇒ a running tab is owed a hint.
    let mut widened = s.clone();
    widened.sandbox.extra_grant_dirs = vec!["D:/tools".into()];
    assert_ne!(spawn_inject_sig(&widened), sandboxed);
    // …but `allow_network` does NOT govern tabs (decision B3: a sandboxed
    // tab always has egress), so flipping it must not nag a tab that it
    // cannot reach.
    let mut net = s.clone();
    net.sandbox.allow_network = true;
    assert_eq!(
        spawn_inject_sig(&net),
        sandboxed,
        "`allow_network` is the run_command knob; nagging tabs for it is a hint nobody reads"
    );

    // Live-applied tuning must NOT nag: the read-advisor thresholds are
    // read per-invocation by the loopback handler, not baked at spawn.
    let mut s = Settings::default();
    s.graph.enabled = true;
    s.graph.read_advisor_min_lines += 25;
    s.graph.context_per_file_chars += 100;
    assert_eq!(spawn_inject_sig(&s), with_graph);
}
