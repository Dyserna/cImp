# V40 — Harness registry (one descriptor drives every harness surface)

**Status:** DRAFT for approval (2026-08-21) — implementation not started.
GitHub: not filed yet (umbrella + phase issues on approval).
**Sequencing:** after V39 ships and is live-verified. **V41 — Codex CLI** is
the consumer: it is the first harness added *through* this registry, and it
does not start until V40 is merged, released and the live-verify below is
green. V40 itself adds no harness and changes no behaviour for Claude Code or
OpenCode; it is a refactor with one schema migration (35 → 36) and two new
enforcing tests.

## Motivation

V35 Phase K moved "harness knowledge" into `src-tauri/src/harness/` and wrote
down the promise — `harness/README.md` § *Adding a harness*,
`docs/ARCHITECTURE.md` § *Adding a harness plugin*, design doc § 6: *"a new
harness is one directory and no changes above L2; you must not need a new enum
variant outside `harness/`, a new match arm in `tabs/config.rs`, a bespoke
gate constant, a frontend mirror."*

An inventory on 2026-08-21 (sweep for every site a third harness would touch)
shows the promise holds for the **protocol** layer (CHP, the registry rows,
the health read-model) and fails for the **settings, spawn and UI** layers:

- ~60 per-harness data entries and ~90 per-harness code branches, roughly 80 %
  of them outside `harness/`. Three fail loudly if forgotten (`HARNESS_DIRS`
  in `layering.rs:499`, the `MAINTENANCE.md` drift-table parity test,
  `AUDIT_CONSUMERS`' self-test in `loopback.rs`); everything else compiles.
- `layering.rs::no_harness_literals_outside_harness` never polices harness
  *identity*: its needles come from registry `Dep` tokens filtered to ≥ 8
  chars / underscore / camelCase (`layering.rs:198-201`), so `"claude"` and
  `"opencode"` are not needles. `graph/service.rs` carries 128 of them and the
  suite is green. The test protects Claude's payload *field names*, which is
  what Phase K set out to do; "which harness is this" was never in scope.
- Ten different "which harness is this?" functions (`tabs/config.rs:165
  harness_of`, `:418 command_is`, `:443 tab_consumer`,
  `settings/injection.rs:1310 Consumer::of_command`, `offload/mcp_host.rs:1023
  Consumer::from_str`, `graph/mcp.rs:611`, `harness/probe.rs:189/202`,
  `offload/outbound.rs:1430`, `audit/runner.rs:462`,
  `src/lib/status/contextMeter.ts:77`). **Six fall back to Claude** for an
  unrecognised command — a new harness is silently misattributed (activity
  badge, injection scope, expose flag, audit latch slot, graph source), never
  rejected.
- Two unrelated `Harness` enums (`harness/contract.rs:90`,
  `sandbox/tabs.rs:67`), plus `AiTabId`, `TabId`, two `Consumer` enums,
  `AiToolBuiltin`, and three TypeScript unions — eight spellings of the same
  set.
- Every per-harness setting is a hand-written **field pair**, never a map:
  `expose_commands_claude/opencode`, `code_audit.expose_claude/opencode`,
  `McpServerConfig.claude_access/opencode_access`,
  `claude_last_seen/opencode_last_seen` (already asymmetric —
  `claude_last_verified` and `claude_auto_verify` have no OpenCode twin),
  three ~35-line `default_*_tab()` templates. Each is mirrored by hand in
  `src/lib/settings/types.ts` and nothing checks the two sides agree.
- `spawn_inject_sig` is `[Value; 2]` (`tabs/config.rs:591`) read
  **positionally** in `ipc/commands.rs:1000-1007`. A missing slot means a
  spawn-baked setting flips with no restart hint — the failure the mechanism
  exists to prevent.
- Silent no-ops: `note_harness_version` is `_ => {}`
  (`settings/persistence.rs:520`); `expects_chp` is hard-coded
  `agent == "opencode" || agent == "claude"` (`harness/chp.rs:224`), so
  stale-artifact detection is off for any other harness; `probe.rs:410-411`
  is two literal `run_for` calls; an unmapped command gets `Harness::None`
  in `sandbox/tabs.rs` and is **not sandboxed at all**.
- `SettingsApp.svelte:3579-3874` is ~300 lines of hand-authored per-harness
  checkboxes, nav buttons and `{:else if}` bodies; `terminals.ts:239`
  renders an unknown harness as "Shell"; `tabs/types.ts:118 isShellTab`
  classifies it as a shell.
- The one surface already shaped right end-to-end: `harness_health` →
  IPC → `{#each harnessFresh.harness_health as panel}`
  (`SettingsApp.svelte:7552`). Zero frontend branches; spoiled only by
  `health.rs:352/381/396` reading the field pairs through `match` arms.

Adding Codex CLI against this tree would be a ~150-site scavenger hunt where
most misses are silent. A checklist document would rot within a milestone
(the one we have already has). The fix is structural: **one production
descriptor per harness that the settings, spawn, sandbox, loopback and UI
layers consume**, plus tests that make the README's claim true, plus a short
checklist for the residue that genuinely needs design per harness.

## Locked decisions

1. **One `HarnessDescriptor` registry in `harness/`, the single source of
   truth for harness identity.** `harness/registry.rs` declares
   `pub static HARNESSES: &[HarnessDescriptor]`, one entry per
   `harness/<id>/` directory, replacing `layering.rs::HARNESS_DIRS` (which
   becomes a view over it). Fields (all `'static` data, no I/O):
   `id` (`HarnessId`, the string `"claude"` / `"opencode"` / later
   `"codex"`), `label` ("Claude Code"), `binaries` (`["claude"]`,
   `["opencode"]` — what `command_is` matches), `tab_ids` (reserved built-in
   tab ids with canonical order: `claude`, `claude-local` → Claude),
   `consumer` (the MCP consumer token), `expects_chp`, `sandbox_grants` (the
   grant rows now in `sandbox/tabs.rs:288-370`), `env_strip`
   (`HARNESS_ENV_VARS`), `features` (decision 6), `oob` (decision 4) and
   `default_tab(tab_id)`. *Why:* every "which harness" question today is
   answered by a different function with a different vocabulary and six of
   them default to Claude; one table with one lookup ends that.
2. **Unknown is an error, never Claude.** `HarnessId::from_command`,
   `from_tab_id`, `from_consumer` return `Option`/`Result`. The six
   Claude-fallback sites are rewritten to propagate `None` (a shell tab) or
   refuse (a consumer token nobody declared). The `--consumer` default in
   `main.rs:311-323`, `offload/mcp.rs:88`, `audit/mcp.rs:580` stays
   `"claude"` on the command line for backward compatibility — but it is
   resolved through the registry, so a typo fails the proxy start with the
   harness list in the message.
3. **The two `Harness` enums merge.** `sandbox::tabs::Harness` is deleted;
   `contract::Harness` becomes `HarnessId` (the `Any` variant stays for
   harness-neutral registry rows). `AiTabId`, `TabId::{Claude, ClaudeLocal,
   OpenCode}`, both `Consumer` enums and `AiToolBuiltin` keep their wire
   encodings (`as_id`/`from_id` strings are persisted) but their
   per-harness `match` arms become registry lookups; the built-in variants
   are retained only where a persisted form depends on them, otherwise they
   collapse to `Ai(HarnessId, tab_id)`.
4. **Per-harness behaviour lives behind a small trait, implemented in
   `harness/<id>/mod.rs`.** `trait HarnessPlugin { fn resolve_oob(..);
   fn compose_env(..); fn extra_args(..); fn pre_args(..); fn write_artifacts(..);
   fn spawn_sig(&Settings) -> Value; fn note_version(..) }`. The descriptor
   holds `&'static dyn HarnessPlugin`. `tabs/config.rs` keeps the
   harness-neutral composer (`build_ai_tool_spec`, cwd, session pinning
   plumbing, sandbox wiring) and calls the plugin for the ~11 sites that
   branch today (`:118-124`, `:165-178`, `:229-284`, `:345-359`, `:366`,
   `:443`, `:1199`, `:1219`, `:1287-1437`). Claude's `resolve_oob_source`
   branch (mint uuid, `--session-id`) and OpenCode's (port, `--port
   --hostname`, `OPENCODE_CONFIG_CONTENT`) move verbatim into their
   directories — same text, same tests, per the Phase K rule. *Why a trait
   and not pure data:* OOB transport, env composition and artifact writing
   are code, and design doc D1 ("the extension point is the protocol, not a
   Rust trait") is about L2 — this trait sits at L1, below it, and is
   internal to the tree (D7: no third-party loading).
5. **Per-harness settings become a map keyed by `HarnessId`.** Schema
   35 → 36. `Settings.harness: BTreeMap<HarnessId, HarnessSettings>` with
   `{ expose_commands, expose_code_audit, last_seen, last_verified,
   auto_verify }`; `McpServerConfig.access: BTreeMap<HarnessId, McpAccess>`
   replaces `claude_access/opencode_access`;
   `enabled_ai_tabs` stays (tab ids, not harnesses). The migration copies
   each existing pair into its slot; absent keys take the descriptor's
   defaults, so a harness added later needs **no migration** — that is the
   whole point. Single-harness settings that are *features*, not identity
   (`statusline`, `claude_local`, `offload.opencode_provider`,
   `injection.opencode_native_gate_enabled`) are **not** moved; they are
   declared (decision 6).
6. **Feature declaration replaces id branching in the UI.** Each descriptor
   carries `features: &[HarnessFeature]` — `Statusline`, `LocalProvider`,
   `NativeToolGate`, `SessionUsage`, `ContextBar`, `Channels`, `Hooks`,
   `Plugin`, `FileArtifact` (has goldens). The Settings UI shows a
   Claude-only section because `features.contains(Statusline)`, never
   because `id === 'claude'`. A new harness that has none of them gets a
   correct, smaller Settings page with no UI work.
7. **The frontend receives the registry over IPC and never re-declares it.**
   `harness_list` command returns `{ id, label, tab_ids, binaries, features,
   consumer }` per harness. `AI_TABS`, `RESERVED_AI_TAB_IDS`,
   `isShellTab`/`isOpencodeTabId`, both `order` arrays in
   `SettingsApp.svelte:1652/1660`, `subSectionForTabId`, `consumerTabs`,
   `displayNameFor`, the enable checkboxes / nav buttons / `{:else if}`
   bodies (`:3579-3874`), `McpManagementEditor` exposure checkboxes,
   `App.svelte:217-227` restart-hint chain, `TabBar.svelte:123` fallback
   and `contextMeter.ts:77 commandIsClaude` all become iteration or lookup
   over that list. `types.ts` field pairs collapse with decision 5. Same
   shape as `harness_health` today. The list is fetched once at startup with
   `AI_TABS` kept as the synchronous fallback **only** until the IPC answers
   (it is static data; it cannot disagree).
8. **`spawn_inject_sig` becomes `BTreeMap<HarnessId, Value>`.** Each
   plugin's `spawn_sig()` builds its own object (today's `:647-748` and
   `:749-790` bodies, moved verbatim). The restart-hint consumer in
   `ipc/commands.rs:1000-1007` iterates the map; the per-harness slot test
   (decision 10) asserts every registry entry has a slot. *Why:* positional
   `[0]`/`[1]` is the one place where forgetting a harness disables a
   safety mechanism silently.
9. **Every per-harness loop iterates the registry.** `probe.rs:410-411`,
   `capture.rs:483`, `health.rs::PANELS`, `AUDIT_CONSUMERS`
   (`loopback.rs:4826`), `outbound.rs::UNSCOPED` (becomes a map keyed by
   `HarnessId`), `persistence.rs:1843` / `tab_lifecycle.rs:1096` canonical
   orders, `chp::expects_chp`, `note_harness_version`. None keeps a literal
   list.
10. **Two new enforcing tests in `layering.rs`, both checked in both
    directions like the existing allowlists.**
    (a) `no_harness_identity_outside_registry`: the literals `"claude"`,
    `"claude-local"`, `"opencode"` (and every `HARNESSES[i].id` /
    `tab_ids` / `binaries` string) may appear outside `harness/` only in
    files on `IDENTITY_ALLOWLIST` with a reason (expected survivors:
    `settings/schema.rs` tab-id consts until decision 3 lands,
    `main.rs` `--consumer` default, loopback route table, persistence
    migrations — migrations are frozen history and always exempt).
    (b) `every_registry_entry_is_fully_wired`: for each descriptor, the
    directory exists, the registry has rows, a hello is declared, a
    `spawn_sig` slot exists, a sandbox grant table is non-empty, a health
    panel row appears, `MAINTENANCE.md` has the drift rows, and — for
    `features.contains(FileArtifact)` — a goldens directory exists. The
    existing `every_harness_dir_declares_its_capabilities` folds into it.
    *Why:* this is what turns "forgot the sixth place" into a red build,
    the same trick V35 used for payload names.
11. **The frontend gets a parity test too.** A vitest that loads the Rust
    `harness_list` fixture (emitted by `cargo test` into
    `src-tauri/fixtures/harness/registry.json`, committed) and asserts the
    TypeScript `HarnessFeature` union and `HarnessId` type cover it — the
    same pattern as the settings-pointer tests. A descriptor field added in
    Rust without a TS mirror fails `vitest`.
12. **Docs carry only the residue.** `harness/README.md` § *Adding a
    harness* and `ARCHITECTURE.md` § *Adding a harness plugin* are rewritten
    to the truthful list (§ *What a new harness still costs* below); the
    false "no frontend mirror / no match arm" claims are replaced by the
    test names that now enforce them. `HARNESS-PLUGIN-LAYER.md` gains a
    § *Registry* page; `docs/CHP.md` `agent`/`consumer` prose says "a
    registered harness id" instead of "`claude` or `opencode`".
13. **No behaviour change for Claude Code or OpenCode.** Every moved
    function keeps its text and its tests (Phase K rule); the plugin goldens
    under `fixtures/plugin-goldens/opencode/` stay byte-identical; the V39
    `InputProfile` and `delegate_task_<id>` generation consume the registry
    but produce the same tool set. The live-verify is a regression pass, not
    a feature pass.
14. **Migrations are frozen.** Nothing in `settings/persistence.rs`'s
    historical migrations is rewritten to use the registry; they describe
    old on-disk shapes and must keep their literals. They are allowlisted
    wholesale in test 10(a).

## Architecture

```
harness/
  registry.rs        HARNESSES: &[HarnessDescriptor]   ← NEW, the source of truth
  plugin.rs          trait HarnessPlugin                ← NEW (L1 contract, in-tree only)
  contract.rs        capability rows (Harness → HarnessId)
  chp.rs             expects_chp() reads the registry
  health.rs          PANELS = HARNESSES.iter()
  layering.rs        HARNESS_DIRS → view; + two tests (decision 10)
  claude/mod.rs      impl HarnessPlugin for Claude  (oob, env, args, overlay, spawn_sig)
  opencode/mod.rs    impl HarnessPlugin for OpenCode (oob, env, args, plugin.js, spawn_sig)

tabs/config.rs       harness-neutral composer; calls descriptor.plugin.*
sandbox/tabs.rs      grant rows come from descriptor.sandbox_grants
settings/schema.rs   Settings.harness: BTreeMap<HarnessId, HarnessSettings>  (schema 36)
ipc/commands.rs      harness_list; restart hints iterate spawn_sig map
src/lib/harness.ts   HarnessInfo[] from IPC; helpers replace AI_TABS et al.
```

Data flow for a tab spawn after V40: `tab.command` → `HarnessId::from_command`
(registry) → `descriptor.plugin.resolve_oob / compose_env / extra_args /
write_artifacts` → `PtyLaunchSpec { harness: Option<HarnessId> }` →
`sandbox::tabs` picks `descriptor.sandbox_grants`. A `None` harness is a shell
tab; it is never sandboxed as if it were Claude.

## Failure modes (adversarial)

- **Descriptor present, plugin half-implemented.** Test 10(b) covers the
  static wiring; it cannot prove `compose_env` is right. Mitigation: the L2
  probe per harness (V35) stays the runtime check, and V41 is the real test.
- **Map-keyed settings with an id nobody registered** (a downgraded binary
  reading a settings file written by a newer one with `codex`). Unknown
  keys are preserved on load and round-tripped on save, never dropped —
  the same rule V37 applied to unknown MCP categories. They are not shown.
- **Frontend list arrives late.** Until `harness_list` resolves, the
  synchronous fallback is the committed `registry.json` (decision 11), so
  first paint cannot disagree with the backend.
- **Allowlist rot.** Both new allowlists are checked in both directions; an
  entry that stops matching fails the build (the lesson from Phase K's
  `UPWARD_EXEMPT`).
- **Migration 35 → 36 on a file with only one harness configured.** The
  map gets one entry; defaults fill the other at read time from the
  descriptor — the migration never has to know the harness list.
- **`spawn_inject_sig` text drift.** Moving the two bodies verbatim keeps
  the existing equality tests; a dedicated test asserts the map serialises
  to the same JSON the `[Value; 2]` did for a fixed settings fixture, so the
  restart hint does not fire once for every user on upgrade.

## Out of scope

- Adding Codex CLI (V41). V40 ships with exactly the two harnesses it has.
- Third-party / out-of-tree harness loading (D7 stands).
- Reworking the capability registry rows, CHP, canaries, probes or the
  health panel — they are already harness-neutral; they only switch from
  literal loops to registry iteration.
- `docs/HARNESS-NATIVE-TOOLS.md` restructuring — its per-harness sections
  stay; V41 adds a Codex section.
- Making `statusline`, `claude_local`, `opencode_provider` or the OpenCode
  native gate generic. They are real single-harness features and are
  declared as such (decision 6), not generalised.

## Phases

- **A — Registry + identity (backend, no schema change).** `registry.rs`,
  `HarnessId`, `HarnessDescriptor`, `HarnessPlugin` trait with both impls
  moved verbatim from `tabs/config.rs` / `sandbox/tabs.rs`; the ten
  "which harness" functions collapse to registry lookups; the six Claude
  fallbacks become `None`/refusals; `expects_chp`, `note_harness_version`,
  `PANELS`, probe/capture loops, `AUDIT_CONSUMERS`, `UNSCOPED` iterate the
  registry; `sandbox::tabs::Harness` deleted. Test 10(b) lands here; 10(a)
  lands with an allowlist naming every survivor, so it is green and the
  survivors are the worklist for B/C.
- **B — Settings map (schema 36) + spawn-sig map.** Decision 5 and 8, the
  migration, `health.rs`/`verify.rs`/`probe.rs` read the map, the
  `ipc/commands.rs` restart-hint consumer iterates it, the spawn-sig
  JSON-equality regression test. `types.ts` mirror collapses.
- **C — Frontend over IPC.** `harness_list` command, `registry.json`
  fixture + vitest parity test (decision 11), `src/lib/harness.ts`, every
  site in decision 7 rewritten to iterate; feature-gated sections
  (decision 6). Allowlist 10(a) shrinks to the frozen-history set.
- **D — Docs + README truth pass.** Decision 12; `MAINTENANCE.md` gains a
  "registry" row in the drift table pointing at the two tests.
- **E — Live-verify** (regression pass, below), RC, then V41 opens.

Each phase is independently mergeable and leaves both harnesses working;
A is the largest and should be one agent run with the 2026-08-21 inventory
as its brief (it is recorded in the orchestrator's memory as
`harness-descriptor-gap`).

## What a new harness still costs (the truthful README list, post-V40)

1. `harness/<id>/mod.rs` with `impl HarnessPlugin` — OOB transport, env,
   args, artifact writer, `spawn_sig`. **Design work**; nothing can make a
   harness's wire shape data.
2. One `HarnessDescriptor` entry — id, label, binaries, tab ids, consumer,
   features, sandbox grant rows (**design**: what the harness needs on disk),
   default tab template.
3. A CHP hello (`serves`/`cannot`) and registry rows with `wired_in`.
4. L1 canaries + fixtures under `fixtures/harness/<id>/<version>/` and the
   L2 probe `drive` arm — **design**, per wire format.
5. Goldens if the artifact is a file.
6. `MAINTENANCE.md` drift rows, a `HARNESS-NATIVE-TOOLS.md` section (feeds
   `toolclass.rs` — security-relevant), `CHP.md` route table if it pushes.

Everything else — settings, spawn sig, restart hints, sandbox selection,
health panel, probes, audit consumers, Settings UI, tab naming, MCP
exposure checkboxes — is derived, and tests 10(a)/10(b)/11 fail if it is not.

## Live-verify

Regression pass on a fresh RC, both harnesses, fresh tabs (spawn-baked
artifacts are regenerated):

1. Claude tab and OpenCode tab launch, CHP hello arrives for both, Harness
   health panel shows both rows with versions and *last verified* intact
   after the 35 → 36 migration (values carried over, not reset).
2. Flip `expose_commands` for OpenCode only → restart hint names OpenCode
   only; flip a Claude spawn-baked setting → names Claude only (spawn-sig
   map, decision 8).
3. Per-server MCP access checkboxes show two columns driven by the list,
   values preserved from `claude_access/opencode_access`.
4. Sandbox on: Claude tab gets the Claude grant rows, OpenCode the OpenCode
   rows (chip popover lists them); a plain shell tab is not sandboxed and
   not labelled as either harness.
5. A tab whose command is `foo` (unknown binary): renders as a Shell,
   attaches no OOB reader, no `delegate_task_foo` appears, audit consumer
   refusal names the registered ids.
6. `cimp --harness-canary` and `--harness-capture` run both harnesses (probe
   loop iterates the registry); output unchanged vs the previous RC.
7. `cimp --consumer codex` → proxy refuses to start with the registered list
   in the message (decision 2).
8. V39 regression: delegation Claude ↔ OpenCode still works; tool set
   identical to the pre-V40 RC (`tools/list` diff empty).
9. Settings page: Claude-only sections (statusline, context bar, session
   usage, local provider) show under Claude; OpenCode-only (provider,
   native gate) under OpenCode — by feature flag, confirmed by temporarily
   removing `Statusline` from the Claude descriptor in a dev build and
   seeing the section disappear with no other edit.
10. Downgrade test: write a settings file containing `harness.codex = {...}`
    by hand, launch → no error, key preserved on next save, not shown.
11. `cargo test`: 10(a), 10(b) and the spawn-sig JSON-equality test are in
    the run; `vitest`: the registry parity test is in the run. Delete the
    OpenCode `spawn_sig` arm in a scratch build → 10(b) fails naming it.

## V41 preview — Codex CLI (not part of V40)

Recorded here only so V40's residue list is checked against a real target.
To be researched by the V41 milestone, not assumed: Codex CLI's extension
mechanism (config file vs hooks vs its JSON-RPC app-server mode) decides
whether it is a Tier B push harness or a Tier C reader; its MCP
configuration shape decides the proxy wiring; its native tool surface
feeds `toolclass.rs`; its session/transcript location decides the OOB
reader; its config/cache directories decide the sandbox grant rows. Each of
those is an item on the residue list above and nothing else — if V41 finds
itself editing a file outside `harness/codex/` that is not on that list,
V40 missed a seam and that is a V40 defect, not a V41 task.
