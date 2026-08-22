# V40 — Harness registry and the V35 leftovers (everything harness-specific moves behind the plugin)

**Status:** **IMPLEMENTED THROUGH PHASE G** (2026-08-22, branch `feat/v40-harness-registry`). Phases 0 and A-G are done; **Phase H (#100) = RC + the live-verify below**, which is the milestone's only remaining gate. Amendments 0-a...0-g are folded into decisions 2, 4, 5, 9, 17, 24 and 27; every ruling taken during implementation is in *What changed vs the design*. The row-level appendix (`docs/V40-harness-residue-ledger.md`) was DELETED in Phase G: every row is moved, or allowlisted with a reason in `harness/layering.rs` / `src/lib/harnessIdentity.test.ts`, and the layering tests are the durable record.
GitHub: umbrella #101, milestone 14; phases 0 #102 · A #93 · B #94 · C #95 · D #96 ·
E #97 · F #98 · G #99 · H #100.
**Sequencing:** after V39 ships and is live-verified. **V41 — Codex CLI** is
the consumer: it is the first harness added *through* this registry, and it
does not start until V40 is merged, released and the live-verify below is
green. V40 adds no harness; it is a refactor with one schema migration
(35 → 36), one CHP minor bump (new neutral events, decision 30), three new
enforcing tests (10a, 10b and their frontend twin), and the short list of
deliberate behaviour changes in *What changed vs the design* below — read that
before treating "no behaviour change" as the whole story. It was driven by a
row-level worklist, `docs/V40-harness-residue-ledger.md`, which Phase G
DELETED: every row is now either moved behind the plugin interface or
allowlisted with a reason the layering tests check in both directions, and
those tests are the durable record the appendix was a stand-in for.

**Scope (user ruling 2026-08-22):** not just "what a third harness touches"
but **everything harness-specific V35 left in core** — the activity
heuristic, the usage/statusline chain, permission-prompt grammar, hook
bodies, the drift advisor, model-visible text, MCP client specifics,
frontend payload mirrors, fixtures and docs. Two sweeps found it: the
identity/settings/spawn inventory (decisions 1–17) and the residue ledger
(decisions 18–30; ≈185 Rust production sites, ≈157 frontend sites, 30
fixture/test sites, 26 doc sections).

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
layers consume through one `HarnessPlugin` interface**, so that everything
harness-specific lives in `harness/<id>/` and nothing else in cImp knows a
harness by name — plus tests that make the README's claim true.

## Locked decisions

0. **The principle: cImp core contains no harness-specific code or data.
   Everything a harness needs reaches core through exactly two interfaces —
   `HarnessPlugin` (L1, in-tree, at spawn/config time) and CHP (L2, on the
   wire, at run time).** "Harness-specific" means anything that is true of
   Claude Code or OpenCode and not of harnesses in general: a binary name, a
   route path, a hook payload field, a native tool name, a settings field
   only one harness has, a UI section only one harness shows, a grant row, a
   probe. V35 Phase K moved the *readers* behind this line; V40 moves the
   rest. The test for whether something belongs in core is: *would it still
   make sense if both shipped harnesses were deleted?* If not, it belongs in
   `harness/<id>/` and core consumes it through the interface. Decisions
   1–17 are this principle applied surface by surface; decision 10 is what
   enforces it.

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
2. **Unknown is an error, never *a shipped harness*.** `HarnessId::from_command`,
   `from_tab_id`, `from_consumer` return `Option`/`Result`. The six
   Claude-fallback sites are rewritten to propagate `None` (a shell tab) or
   refuse (a consumer token nobody declared). *(0-c)* The same applies to the
   **OpenCode**-fallback classifier `tabs/config.rs:443 tab_consumer`
   (`else { "opencode" }`), which V39 gave seven new callers
   (`delegation/engine.rs:265`, `graph/mcp.rs:495`, `ipc/commands.rs:606/623/645`,
   `offload/loopback.rs:19153`, `offload/mcp.rs:3212`) — today a `codex` tab
   would be classified as OpenCode, become eligible for its Manual slot and
   be typed into with OpenCode's paste profile. All seven propagate `None`:
   an unrecognised command is not a delegation target and is never typed
   into with another harness's rules. The frontend twin
   `src/lib/delegation.ts:455 tabHarness` goes the same way (decision 27). The `--consumer` default in
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
   internal to the tree (D7: no third-party loading). *(0-a)* V39's
   `InputProfile` is **not** a second trait: it is a `Copy` data struct
   (`harness/input.rs:64-112`) plus a two-arm lookup
   `input_profile(id) -> Option<InputProfile>` (`:131-137`). The trait gains
   `fn input_profile(&self) -> Option<InputProfile> { None }` — **`Option`
   with a `None` default**, because "a harness without `input.rs` is not a
   valid worker" must keep failing closed (`engine.rs:316-330`). The
   `InputProfile`/`PasteMode`/`paste_bytes`/`fits` types move to
   `harness/plugin.rs` as neutral types; both `harness/<id>/input.rs` bodies
   stay untouched and `harness/input.rs::input_profile` is deleted.
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
   declared (decision 6). *(0-f)* `harness_versions.input_profile_status`
   (`schema.rs:900`) — V39's manual-spike outcome for the paste contract —
   is today **one scalar for all harnesses**: a `"fail"` removes every
   `delegate_task_*` and refuses every delegation for every harness, and a
   Claude pass would silently vouch for Codex. It joins the per-harness core
   block (`Settings.harness[<id>].input_profile_status`), the 35 → 36
   migration copies the single value into every existing key, and the
   `CAP_DELEGATION_WORKER` gate (`contract.rs:1801-1815`) resolves it **for
   the worker's harness only** — `gate_for(id, harness)` or the preflight
   reading the per-harness row directly. Behaviour-bearing; Phase B.
6. **Harness-only settings are owned by the plugin, not by core.**
   `statusline`, `claude_local`, `claude_auto_verify`,
   `offload.opencode_provider(_auto)` and
   `injection.opencode_native_gate_enabled` are today core `Settings` fields
   that only one harness reads. They move to a per-harness namespace
   `Settings.harness[<id>].ext`, a JSON object whose **schema, defaults,
   validation and migration are declared by the plugin**
   (`HarnessPlugin::settings_schema() -> &[SettingField]`, with
   `SettingField { key, kind (bool|int|string|enum|path), label, hint,
   default, spawn_baked }`). Core stores the object opaquely, validates it
   against the plugin's declared fields at the parse boundary (declared ≠
   enforced), and routes reads back through the plugin — core never names
   a key. The Settings UI renders each harness's section from the declared
   fields with one generic form component, so a harness with no `ext`
   fields gets an empty section and no UI work. Features that need richer
   UI than a form (Claude's session-usage and context-bar panels) are the
   one exception: they stay as components but are mounted by a
   `features: &[HarnessFeature]` declaration on the descriptor
   (`SessionUsage`, `ContextBar`, `FileArtifact`), never by `id ===
   'claude'`; their data arrives through CHP events, not through a
   Claude-shaped IPC. The 35 → 36 migration moves each existing field into
   its plugin's `ext` with the same value; a plugin's `spawn_sig` covers its
   `spawn_baked` fields automatically, which closes the "spawn-baked setting
   without a signature entry" class for good.

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
9. **Every per-harness loop iterates the registry.** `probe.rs:745/1178`
   (the two literal `resolve_command` calls; `drive` at `:517-566`),
   `capture.rs:483`, `health.rs::PANELS` — *(0-e)* which since V39 also
   carries a **non-harness** `(Harness::Any, "Cross-harness")` row for the
   `delegation.worker` gate; plain iteration would silently drop it and hide
   a gate the user can be blocked by, so `PANELS` = the descriptors **plus
   one neutral panel for `Harness::Any` rows** (an explicit second source,
   never a pseudo-descriptor with empty binaries) and 10(b) asserts both —
   `offload/mcp.rs:3199 delegate_targets` (already iterates
   `harness_ids()` ✔ but joins through `tab_consumer`, decision 2),
   `loopback.rs:19149 manual_tab_for`, `AUDIT_CONSUMERS`
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
    migrations — migrations are frozen history and always exempt). The same test
    forbids `HarnessId::Claude` / `HarnessId::OpenCode` *comparisons* and
    `match` arms outside `harness/` — core may hold a `HarnessId` and pass
    it to the registry; it may not branch on its value. The built-in
    variants exist for persisted wire forms only.
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
    under `fixtures/harness/opencode/goldens/` stay byte-identical; the V39
    `InputProfile` and `delegate_task_<id>` generation consume the registry
    but produce the same tool set. The live-verify is a regression pass, not
    a feature pass.
14. **Migrations are frozen.** Nothing in `settings/persistence.rs`'s
    historical migrations is rewritten to use the registry; they describe
    old on-disk shapes and must keep their literals. They are allowlisted
    wholesale in test 10(a).
15. **Loopback routes are registered by the plugin.** The twelve
    `("POST", "/claude/hook/*")` arms in `offload/loopback.rs:1270-1289` and
    their `handle_claude_*` bodies move to `harness/claude/hook.rs` (the
    payload mechanics already live there since Phase J; only the dispatch
    is still in core). `HarnessPlugin::routes() -> &[Route]` returns
    `(method, path, handler)`; loopback's router appends every registered
    plugin's routes after the CHP-neutral ones (`/session/hello`, `/mcp/*`,
    the audit and push routes). Core keeps no harness path literal and
    `loopback.rs` leaves `LITERAL_ALLOWLIST`. `classify_permission_event`
    and the `unwrap_or("claude")` consumer defaults go the same way
    (resolved from the hello's `agent`, never defaulted).
16. **Native tool vocabulary is declared by the plugin.** `toolclass.rs`'s
    `TABLE` keeps only cImp's *own* routed tools; the harness-native rows
    (`Edit`/`Write`/`MultiEdit`/`NotebookEdit`/`Bash`/`WebFetch`… and
    OpenCode's `edit`/`write`/`patch`/`bash`/`webfetch`…) move to
    `harness/<id>/tools.rs` as `HarnessPlugin::native_tools() ->
    &[NativeTool { name, class: ToolClass, mutates_fs, memory_kind }]`.
    `classify`, `mutates_fs`, `tool_checkpoint_is_mutating`
    (`loopback.rs:5604`) and `graph/memory.rs::memory_kind_of` (the
    "FINDING, not a clean exemption" allowlist entry) look the name up via
    the plugin that sent the event — so an unknown tool from an unknown
    source classifies as *unknown/mutating* (fail closed), never as "not in
    Claude's table, therefore safe". `docs/HARNESS-NATIVE-TOOLS.md` becomes
    the human twin of those tables and a test checks them against each
    other, as `MAINTENANCE.md` is checked against the registry today.
17. **Canaries and probes are supplied by the plugin.**
    `HarnessPlugin::canaries() -> &[Canary]` (fixture + assertion fn) and
    `HarnessPlugin::probe(&ProbeCtx) -> ProbeReport` replace the **six**
    named functions in `canary.rs` (`:214, 284, 349, 411, 460` + V39's
    `claude_transcript_stop_reason` at `:396-450`), the `drive` match in
    `probe.rs:517-566`, the two literal `resolve_command` calls
    (`:745`, `:1178`) and V39's Claude-payload probe code
    (`stop_reason_is_substantive` / `stop_reason_outcome`, `:1692-1770`).
    *(0-b)* V39's `*.input.profile` rows have **no probe** — they are in
    `probe.rs:358-370 DECLARED_UNPROBED` ("a REAL turn typed into a REAL
    TUI") — so the trait also gains `declared_unprobed() ->
    &[(CapabilityId, &str)]`, same shape as `canaries()`, and the per-row
    reason prose moves with the plugin; the neutral `delegation.worker`
    entry stays in the runner.
    `canary.rs` and `probe.rs` keep the harness-neutral runner, the report
    shape and the `cimp --harness-canary/--harness-capture` CLI; they
    iterate the registry. The `UPWARD_EXEMPT` entries for `canary.rs` and
    `probe.rs` are deleted once nothing harness-shaped remains in them.

### The V35 leftovers (decisions 18–30)

Each decision names the neutral core abstraction it introduces — the ledger's
section (b) — because that is the real cost: the literal moves are mechanical,
the abstractions are where design happens. Row-level targets are in the
appendix; the decision is the contract.

18. **"Is the harness busy?" is a plugin-declared `ActivitySource`.**
    `HarnessPlugin::activity_source() -> ActivitySource::{OutOfBand,
    TuiMarkers(ActivityTuning)}`. Claude declares `TuiMarkers` with *its*
    tuning (`CLAUDE_BURST_MIN`, `CLAUDE_QUIET`, `CLAUDE_MARKER_GRACE`,
    `CLAUDE_WORKING_STALE`, `AGENTS_STALL_TIMEOUT` — five constants sized to
    Claude's spinner/footer repaint rate, `pty/tasks.rs:43-88`,
    `state/manager.rs:272-284`); OpenCode declares `OutOfBand`. The
    marker-vs-byte-burst arbitration loop (`pty/tasks.rs:386-556`) stays in
    core as neutral machinery parameterised by the tuning;
    `pty/manager.rs:518 oob_drives_activity = matches!(OobSpec::OpenCodeEvent)`
    disappears. `StateSignal::ClaudeOutputStarted/Stopped` and
    `claude_output_active` (~14 sites in `state/manager.rs`) are renamed
    `HarnessOutput*` and become CHP events (decision 30);
    `AgentsActiveChanged` becomes `SubagentsActiveChanged`, emitted by the
    plugin's reader, not "by the transcript tail".
19. **Usage, quota and context are neutral readings; the statusline is
    Claude's.** New core types `QuotaWindow { id, label, used, resets_at }`
    (a list, not `five_hour`/`seven_day` fields), `ContextReading`,
    `TokenKinds` (the billing categories a harness declares —
    input/cache_write/cache_read/output is Claude's set, absent categories
    are *absent*, not zero), and `TurnOrigin` (the `session|agent` lane split
    is Claude's sidechain model; plugins declare their origins). `usage/` and
    `statusline/` move to `harness/claude/statusline.rs` behind
    `HarnessPlugin::usage_source()`; `--statusline` becomes a
    plugin-registered subcommand (`HarnessPlugin::subcommands()`); the IPC
    command `get_claude_usage` becomes `harness_usage(HarnessId)`;
    `usage/mod.rs:661-884 endpoint_poll` (dead, compiled, carries an
    Anthropic OAuth URL and `~/.claude/.credentials.json`) is deleted. The
    frontend `UsageMeter`, `usageMath.ts`, `contextMeter.ts`, `ipc.ts`
    snapshot types and the CodeIntelligence donut/lane labels render from
    the declared windows/categories/origins; `commandIsClaude` and
    `claudePushTabActive` become a registry capability `usage_push`. The
    44 px `.status-bar` height in `tui_theme.css:435` becomes a CSS variable
    from `statusline_rows()`.
20. **Session identity has a declared key space.** `HarnessPlugin::
    session_key_space() -> SessionKey::{Tab, Session}` (Claude keys live
    sessions by tab id, OpenCode by session id — today one map with two key
    spaces, which is why `live_claude_tab_sessions`,
    `mark_live_session_from_event` (`loopback.rs:9014`) and the C-2
    collision guard exist). `GraphService::live_claude_sessions()` becomes
    `live_sessions_for(HarnessId)`; the Datalog query with `agent ==
    "claude"` in its string (`graph/index.rs:4214`) and the
    `"<synthetic>"` model sentinel filters (`:3687, :3725`) move behind
    `usage_source()` / `model_sentinels()`; `session_agent(..).unwrap_or
    ("claude")` (`graph/service.rs:3056`) refuses instead.
21. **Permission-prompt grammar is plugin data; the detector is core.**
    `PermissionDetector` (`processing/permission.rs:263-438` — substring
    matching, veto scoping, the per-kind edge machine) stays. The pattern
    *rows* (`claude_permission`, `claude_permission_bare`, `claude_question`,
    `claude_working`, the disabled `opencode_*` placeholders,
    `CLAUDE_FOOTER`, the `_doc` header, `screen.rs`'s multibyte-glyph
    rationale) move to `harness/<id>/prompts.rs` behind
    `permission_patterns()`. The per-release snapshot table in
    `patterns_file.rs:113-211` is reconstructed from
    `legacy_permission_patterns(era)` per plugin **plus a data-only
    `harness/_retired/aider.rs`** (no plugin, no descriptor — the rows exist
    only so pristine-file reconciliation of files written before V19 keeps
    comparing equal). Claude's `Notification` payload (`PermissionEventBody`,
    the marker strings, `IGNORED_NOTIFICATION_TYPES`,
    `classify_permission_event`, `resolve_permission_tab`,
    `transcript_session_id`) moves into `harness/claude/hook.rs`; core
    receives a neutral `PermissionEdge` via CHP
    `PermissionPromptDetected/Resolved` (decision 30).
22. **Hook ingress is the plugin's, end to end.** Decision 15 moved the route
    *table*; the ~900 lines of `handle_claude_*` bodies
    (`loopback.rs:6934-7853`), the `*_from_hook` converters, `claude_hook_tab`
    / `claude_hook_cwd` / `parse_hook_input` / `report_hook_drift`, the
    `X-CIMP-*` header identity special-case (`note_chp`,
    `report_quiet_capabilities`, `loopback.rs:6372-6441`) and the
    `DRIFT_SHIMS` token vocabulary move with them. Core gains: a neutral
    `HookReply` (Claude answers hook-output JSON — `no_op`/`deny`/
    `additional_context` — OpenCode answers `{ok:true}`; core must not know
    either), `identity_of_request()`, `drift_vocabulary()`, and
    `hook_reply_timeout()` from which core derives `TOOL_CHECKPOINT_BUDGET`
    as `min(all plugins) − margin` instead of a hand-computed 1800 ms
    coupled to two artifacts' timers. The 11 `unwrap_or("claude")` and 2
    `unwrap_or("opencode")` defaults collapse to **one** named
    `harness::DEFAULT_HARNESS` with the wire-compat rationale preserved in
    its doc comment (they are promises to older shim builds, not "any
    harness"); the opposite default on `/latch/state` (`:9805`) becomes an
    explicit, commented policy line.
23. **Drift signals are keyed by harness; every rule runs per harness.**
    `advisor::Signals`' six `claude_*` scalars become `HarnessDriftSignals:
    BTreeMap<HarnessId, DriftSignals>`; the same for the `detection_status`
    IPC payload and `HarnessVersions` in `types.ts`. `version_signature()`
    takes a harness; `drift.version.v1` evaluates for every registered
    harness (today OpenCode has no version-drift path at all);
    `harness_mark_verified` takes a `HarnessId`. The fix-pointer prose that
    names Claude mechanisms (`PreToolUse`, `UserPromptSubmit`,
    `message.usage`, `subagents/*.jsonl`, `BYPASS_HIGH`'s `cat`/`sed`
    enumeration) comes from `Capability::drift_hint()` supplied by the
    plugin; `CodeIntelligenceView`'s `ADVISOR_RULES_TOOLTIP` renders the
    backend-published descriptions instead of repeating them.
24. **Text that reaches the model is a declared seam.**
    `HarnessPlugin::instructions() -> &[Instruction { slot, text }]`. Every
    model-visible string in core is marked: `CHANNEL_INSTRUCTIONS`
    (`offload/mcp.rs:2032`), the attachment instruction
    (`compose/attachments.ts:55`), and `GRAPH_GUIDANCE` — which is sent to
    *both* harnesses yet names Claude's capitalised `Read`/`Bash`
    (`tabs/config.rs:1043`); it is templated with `native_tools()` names per
    harness. Tool-arg aliasing (`file_path`/`filePath`/`notebook_path`,
    `loopback.rs:9124`) comes from `native_tools().arg_names()`. *(0-g)*
    V39 added three more model-visible strings to the inventory:
    `offload/mcp.rs:3152 delegate_tool_contract` + `:3236 delegate_task_tool`
    description (templated with a harness label → descriptor `label`), and
    `offload/agent.rs:653 SCHEMA_FINAL_INSTRUCTION` / `:684
    facade_format_note` (neutral text, marked as model-visible).
25. **MCP client specifics belong to the client's plugin.** The channel
    registration flag, `capabilities.experimental["claude/channel"]`, the
    `notifications/claude/channel` method, the `PROTOCOL_VERSION` pin ("the
    era where the client honours channels") and `session_push_enabled()`
    become `decorate_initialize()`, `push_notification_method()`,
    `mcp_protocol_version()`, `supports_session_push()`. A core
    `PerHarness<T>` (registry-ordinal-keyed map) replaces every fixed-arity-2
    structure: `AUDIT_CONSUMERS`, `UnscopedAudit`'s 2-slot array,
    `SurfaceDigest{claude,opencode}`, `ServerSurface` access pair,
    `Consumer::granted(claude, offload, opencode)`, `code_audit.expose_*`,
    `tool_defs_for_claude/opencode` → `tool_defs_for(HarnessId)`, and the
    frontend `newServer()`/`container()` seeds. The rule that `Offload` and
    `Audit` consumers fold onto Claude's access flag stays in core as an
    explicit `Consumer::conservative_grant()` — it is a security default,
    not a harness fact (see *What stays*).
26. **CLI vocabulary and config writers are the plugin's.**
    `session_selector_flags()` (`--session-id`, `--resume`, `-r`,
    `--continue`, `-c`, `--fork-session`, `--from-pr` —
    `tabs/config.rs:1238`), `accepts_passthrough_argv()` (cImp as a drop-in
    `claude` replacement, `main.rs:112-189`), `preflight()` (the
    `OpencodeNotFound` probe in `tab_lifecycle.rs:1074`; Claude's "not
    gated" becomes a declared `Ok`), `needs_tree_reap()` (`procutil.rs:126`,
    Bun grandchildren), `config_writer()` (`derive_opencode_provider`,
    `offload/server.rs:276`; `ClaudeLocalSettings` → `ANTHROPIC_*` env
    synthesis). The boot/poisoned-lock fallbacks `TabId::Claude`
    (`main.rs:508`, `notifications/manager.rs:399`, `audio/playback.rs:216`)
    and the integrity repair that forces `enabled_ai_tabs = [claude]`
    (`persistence.rs:2100`) become "first registered harness's default tab".
27. **The frontend gets affordances, not prose.** `harness_list` (decision
    7) also carries `HarnessAffordances`: `newSessionCommand` (the three
    "run **/clear** in that tab" strings), `toolListRefresh` (the four
    "OpenCode refreshes in-session, Claude on its next turn" sentences),
    `webTools[]`, `stateDirs[]`, `installHint` + `docsUrl`
    (`TabErrorOverlay`, the `opencode-not-found` copy), `attachmentFormat`,
    `localProviderEnvPreview`, `statuslineRows`, and *(0-d)*
    `attributionTemplate` (`[delegated by {label} · tab "{tab}" · via cImp]`)
    so V39's banner / local-echo / glyph-title string has one source;
    `src/lib/delegation.ts:343 HARNESS_LABELS`, `:349 harnessLabel`, `:365
    attributionLine`, `:455 tabHarness` and `DelegationPopover.svelte:122,
    282, 306` read `harness_list`, and the decision-11 parity test asserts
    `tabHarness` agrees with every descriptor's `binaries`. The "Claude session usage"
    and "Claude context bar" panels (`SettingsApp.svelte:3282-3502`) mount
    through the decision-6 feature slots. `TabId` in `state/manager.rs`
    becomes `Builtin(HarnessId, tab)` with its wire strings unchanged;
    `activeTab = writable('claude')` and `GraphView`'s `isCloud` read the
    registry (default tab; `tier`); `.esrc.claude/.esrc.opencode` CSS
    becomes an accent token per harness; `CAP_PRETOOLUSE_DENY` moves behind
    the registry *together with* the test that pins it
    (`the_gated_capability_ids_reach_the_frontend`).
28. **Fixtures and docs follow the code.** `fixtures/harness/opencode/goldens/`
    → `fixtures/harness/opencode/goldens/`; the Claude TUI scrapes in
    `processing/permission.rs:753-1020` → `fixtures/harness/claude/<ver>/tui/`;
    `docs/spikes/v20/*.ndjson|json|log` (raw payloads, unpinned, unread by any
    canary) → fixtures or deleted; the inline statusline JSON in
    `statusline/mod.rs` tests → the fixture that already exists. Docs:
    `ARCHITECTURE.md`'s Claude hook routing table (§ V11), `CHP.md` § 4.5
    (the 12-row Claude event→route table, "Minimum Claude Code 2.1.63",
    `X-CIMP-Agent: always claude`) and § 6.2, and `MAINTENANCE.md`'s drift
    table (14 rows + version pins) move into `harness/claude/README.md` and
    `harness/opencode/README.md`, which the existing parity test then reads
    instead of `MAINTENANCE.md`. `CHP.md`'s `agent` becomes "a registered
    harness id", never a closed enum. `DESIGN.md` § *What we are building*,
    `README.md`, `Cargo.toml`'s description and `FEATURES.md`'s
    "via a Claude `PreCompact` hook"-style bullets are reworded to describe
    capabilities, with mechanism detail linked to the plugin READMEs.
    Stale `oob::claude::*` / `harness::claude_hook` paths in doc comments
    (11 sites) are fixed in passing.
29. **What stays in core, and why (the exemption list — each entry is a
    `LITERAL_ALLOWLIST`/`IDENTITY_ALLOWLIST` row with this reason).**
    - `spawn_ledger::LEDGER` rows: the tripwire scans every `.rs` under
      `src/` and must keep matching the tree; the argv *strings* come from
      `HarnessPlugin::spawn_sites()`, the rows stay.
    - `offload/toolclass.rs` class rows: the single reviewed authority for
      capability classification is a security decision and does not take
      rows from harness code; the `hook_*` row *names* are neutralised.
    - `Consumer::conservative_grant()`: a security default (decision 25).
    - `TabId` wire strings and `ActivityEntry::source: String`: persisted
      formats (settings JSON; the activity JSONL read back from disk).
      Typing `source` as `HarnessId` would mis-read pre-split rows.
    - `PermissionDetector` machinery, the `pty/tasks.rs` arbitration loop,
      `terminals.ts` mouse-mode handling (must run in the webview against a
      live xterm): neutral engines; only their data/tuning moves.
    - `settings/migration.rs` fixtures (decision 14).
    - **Not harness-specific, ruled so here:** the Anthropic price table
      (`schema.rs:953`) is *provider* knowledge — an OpenCode session reports
      `anthropic/claude-opus-4-8` too — and moves to its own `pricing/`
      seam, not behind a harness plugin; `secret_anthropic_api_key` is a
      vendor secret pattern; the `'OpenCode Grey'` palette and
      `sprites/claudeSprites/` are a persisted-by-name palette and a brand
      asset that depend on no harness (they are *named* after one, which is
      allowed for assets; renaming would be a migration for no gain);
      `segmenter.rs`'s character-class strip runs on any out-of-band prose
      (V20 cut that coupling deliberately); `CANARY_SOURCES`; the
      `sandbox/child_env.rs` node/npm rows.
30. **CHP gains the neutral events the above need, as a minor version
    bump.** `HarnessOutputStarted/Stopped`, `SubagentsActiveChanged`,
    `PermissionPromptDetected/Resolved`, `turn.usage` carrying
    `QuotaWindow`/`TokenKinds`/`TurnOrigin`, and a `drift` event for
    reader-reported contract drift. Vocabulary additions only; every
    existing event and route keeps its shape, so a stale artifact from the
    previous CHP minor still speaks (D5). `docs/CHP.md` gets the rows;
    `chp::EVENTS` is the authority.

## Architecture

```
harness/
  registry.rs        HARNESSES: &[HarnessDescriptor]   ← NEW, the source of truth
  plugin.rs          trait HarnessPlugin                ← NEW (L1 contract, in-tree only)
  contract.rs        capability rows (Harness → HarnessId)
  chp.rs             expects_chp() reads the registry
  health.rs          PANELS = HARNESSES.iter()
  layering.rs        HARNESS_DIRS → view; + two tests (decision 10)
  claude/mod.rs      impl HarnessPlugin for Claude  (oob, env, args, overlay, spawn_sig,
                     settings_schema, routes → hook.rs, native_tools, canaries, probe,
                     activity_source, usage_source → statusline.rs, permission_patterns
                     → prompts.rs, instructions, session_key_space, subcommands, …)
  claude/README.md   the hook route table, drift rows, version pins (from ARCHITECTURE/CHP/MAINTENANCE)
  opencode/mod.rs    impl HarnessPlugin for OpenCode (same shape; plugin.js, tools.rs, config.rs)
  opencode/README.md
  _retired/aider.rs  data only: legacy permission-pattern rows for pristine-file reconciliation

pricing/             provider price table (moved out of settings/schema.rs — not a harness seam)
state/, pty/         neutral activity machinery parameterised by ActivitySource
processing/          PermissionDetector engine; no pattern rows
advisor.rs           rules over HarnessDriftSignals, hints from the plugin

offload/loopback.rs  neutral routes only; plugin routes appended from the registry
offload/toolclass.rs cImp's own tools only; native names resolved via the plugin
graph/memory.rs      memory_kind via plugin.native_tools

tabs/config.rs       harness-neutral composer; calls descriptor.plugin.*
sandbox/tabs.rs      grant rows come from descriptor.sandbox_grants
settings/schema.rs   Settings.harness: BTreeMap<HarnessId, HarnessSettings { ..core, ext }>  (36)
src/lib/settings/    one generic HarnessExtForm.svelte renders plugin-declared fields
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
  native gate *generic*. They stay single-harness features — they just move
  into their plugin's `ext` settings (decision 6) instead of living in core.
- Out-of-process plugins. `HarnessPlugin` is a Rust trait compiled into
  the binary (D7); the interface is the decoupling, not the process
  boundary.

## Phases

Each phase is independently mergeable and leaves both harnesses working.
Phases run **sequentially in one tree**, Phase 0 first (the repo rule: no parallel agents
on a shared tree); A first, then B–H in the order below, each one agent run
briefed with the relevant decisions and ledger sections. This is roughly
three times the original registry-only scope; the phase split keeps each
run reviewable.

- **0 (#102) — Sweep the V39 delegation implementation; re-baseline the
  ledger.** Runs after V39 (#90) merges and live-verifies, before A. The two
  sweeps predate V39, which adds harness-facing code of its own
  (`harness/<id>/input.rs` `InputProfile`, `delegate_task_<id>` generation,
  `OffloadBackendKind::HarnessTab`, `CAP_DELEGATION_WORKER` + frontend
  mirror, `<id>.input.profile` rows + probes, the attribution banner text).
  Deliverables: ledger section M with file:line rows on the post-V39 tree, a
  re-baseline stamp per ledger section, amendment comments on #101 where V39
  contradicts a decision (expected: `InputProfile` folds into
  `HarnessPlugin` rather than a second trait — decision 4; its probe becomes
  `probe()` — decision 17; `delegate_task_*` iterates the registry —
  decision 9; banner labels come from `harness_list` — decision 27), the
  A/C/E/F/G briefs updated with the V39 rows they own, and post-V39
  baselines. Read-only; no code moves.
  **DONE 2026-08-22** on `5e2d87d`: ledger section M (73 rows; engine,
  `HarnessTab`, read-only path, `ActivityKind::Delegation` all neutral ✔ —
  the R-2/R-3/R-7 turn buffers live in `harness/<id>/read.rs`), re-baseline
  table for A–L (`processing/`, `advisor.rs`, `pty/tasks.rs`, docs
  line-exact; `ipc/commands.rs` +529, `state/manager.rs` +~370,
  `loopback.rs` +34/+~100, `settings/schema.rs` +78…+551), amendments
  0-a…0-g folded into decisions 2/4/5/9/17/24/27, phase briefs #93/#95/#97/
  #98/#99 updated. Baselines: vitest 792/792 (38 files), tsc clean;
  cargo-test pending (link-locked test binary at run time).
- **A (#93) — Registry + identity (backend, no schema change).** `registry.rs`,
  `HarnessId`, `HarnessDescriptor`, `HarnessPlugin` trait with both impls
  moved verbatim from `tabs/config.rs` / `sandbox/tabs.rs`; the ten "which
  harness" functions collapse to registry lookups; the six Claude fallbacks
  become `None`/refusals; `DEFAULT_HARNESS` (decision 22); `PerHarness<T>`
  (decision 25) replacing every fixed-arity-2 structure;
  `sandbox::tabs::Harness` and `state::TabId`'s per-harness arms deleted;
  loopback route table, native tool tables, canaries and probes move behind
  the trait (decisions 15–17). Test 10(b) lands here; 10(a) lands with an
  allowlist naming every survivor, so it is green and the survivors are the
  worklist for every later phase (the ledger, by section).
- **B (#94) — Settings map (schema 36) + plugin-owned `ext` + spawn-sig map.**
  Decisions 5, 6 and 8, the migration (core pairs → map, Claude/OpenCode-only
  fields → their plugin's `ext`), `health.rs`/`verify.rs`/`probe.rs` read the
  map, the restart-hint consumer iterates it, the spawn-sig JSON-equality
  regression test. `Feature::OpencodeNativeGate` becomes a plugin-scoped
  feature. `pricing/` seam split out (decision 29). `types.ts` mirror
  collapses.
- **C (#95) — Hook ingress + permission grammar + drift advisor** (decisions 21,
  22, 23, 30). The `handle_claude_*` bodies, converters and
  `PermissionEventBody` move into `harness/claude/hook.rs`; neutral
  `HookReply`, `PermissionEdge`, `HarnessDriftSignals`; pattern rows →
  `prompts.rs` + `_retired/aider.rs`; CHP minor bump with the new events;
  `TOOL_CHECKPOINT_BUDGET` derived. `loopback.rs` leaves `LITERAL_ALLOWLIST`.
- **D (#96) — Activity + usage + session identity** (decisions 18, 19, 20).
  `ActivitySource`, the tuning move, `HarnessOutput*` rename; `usage/` +
  `statusline/` → `harness/claude/statusline.rs`; `QuotaWindow`,
  `ContextReading`, `TokenKinds`, `TurnOrigin`; `SessionKey`;
  `harness_usage(HarnessId)`; `endpoint_poll` deleted; `--statusline` via
  `subcommands()`.
- **E (#97) — Model-visible text, MCP client specifics, CLI vocab, config
  writers** (decisions 24, 25 remainder, 26). `instructions()`,
  `GRAPH_GUIDANCE` templating, `decorate_initialize()` et al.,
  `session_selector_flags()`, `accepts_passthrough_argv()`, `preflight()`,
  `needs_tree_reap()`, `config_writer()`, the boot/poisoned-lock fallbacks.
  **DONE 2026-08-22.** `harness/instructions.rs` is the inventory (9 slots,
  4 harness-templated + 5 neutral, complete-by-construction test both ways);
  `GRAPH_GUIDANCE` templated through `HarnessPlugin::tool_for_role` with
  byte-identical goldens for Claude and a two-word diff for OpenCode
  (`fixtures/harness/<id>/goldens/system-prompt-addendum.txt`); the attachment
  line left `compose/attachments.ts` for the `harness_instructions` IPC;
  `decorate_initialize` / `push_notification_method` / `mcp_protocol_version`
  and `Consumer::conservative_grant` landed; `preflight()` retired
  `TabLifecycleError::OpencodeNotFound` (now `HarnessNotFound{harness,label,
  hint}`) and `ipc/tab_lifecycle.rs` left `IDENTITY_ALLOWLIST`;
  `derive_opencode_provider` moved to `harness/opencode/config.rs` behind
  `ConfigWriter`; `spawn_ledger::LEDGER` became `CORE_LEDGER` + `ledger()`
  joining `spawn_sites()`. Decision 24's `native_tools().arg_names()` is
  Phase C's `memory_arg_keys()` — deliberately not a third vocabulary.
  Baselines: cargo 2807/0/6, vitest 801 (39 files), svelte-check 356/0/0,
  `cargo check --all-targets` 0 warnings.
- **F (#98) — Frontend over IPC** (decisions 7, 11, 27 and the frontend halves of
  19/23). `harness_list` + `HarnessAffordances`, `registry.json` fixture +
  vitest parity test, `src/lib/harness.ts`, every site in decision 7 and 27
  rewritten; the generic `HarnessExtForm`; `UsageMeter`/`usageMath`/
  `contextMeter` over declared windows/categories/origins; feature-mounted
  panels. Allowlist 10(a) shrinks to the decision-29 set.
  **DONE 2026-08-22.** `harness_list` publishes the roster, the affordances and
  two derived tables; `src/lib/harness.ts` is the frontend's only mirror (one
  declared identity, `BOOTSTRAP_RESERVED_TAB_IDS`, asserted equal to
  `fixtures/harness/registry.json` in both directions); `src/lib/harnessIdentity.test.ts`
  is 10(a)'s frontend twin, and its allowlist is down to five rows — the
  registry mirror, a persisted settings key, a sprite set, a palette name and
  the theme metadata that pairs them. The dashboard's lane LABELS became the
  harness's declared origins; the lane SHAPE did not, and that remainder is
  Phase G's.
- **G (#99) — Fixtures + docs truth pass** (decisions 12, 28). Fixture moves,
  `harness/<id>/README.md` with the parity test repointed, `CHP.md` /
  `ARCHITECTURE.md` / `MAINTENANCE.md` / `DESIGN.md` / `README.md` /
  `FEATURES.md` rewrites, stale doc-comment paths, `MAINTENANCE.md` gains a
  "registry" drift row pointing at the two tests. The appendix ledger is
  deleted in this phase (its rows are now either moved or allowlisted with
  a reason).
  **DONE 2026-08-22.** `fixtures/harness/<id>/goldens/` →
  `fixtures/harness/<id>/goldens/`, so one directory per harness holds its
  scrapes, its synthetic renames and the goldens of the artifact cImp writes
  for it. The drift table left `MAINTENANCE.md` for
  `harness/claude/README.md` and `harness/opencode/README.md`;
  `matrix_matches_maintenance_doc` reads all three documents and pairs each
  capability with the document its OWNER owns, which is what split V39's
  combined `*.input.profile` row and is what makes a future combined row fail.
  `HARNESS-NATIVE-TOOLS.md` gained a machine-checked § of `native_tools()`.
  The three neutral model-visible strings left `tabs/config.rs` for the
  decision-24 inventory (`OFFLOAD_GUIDANCE`, the injection-hygiene contract and
  the three tool-steering sentences — the `run_command` half is its own slot
  because it is separately withheld). Decision 19's remainder landed: the
  `graph/` usage payload speaks declared `TurnOrigin`s and `TokenKinds` instead
  of a two-lane struct and four vendor-named fields. The ledger is deleted.
- **H (#100) — Live-verify** (regression pass, below), RC, then V41 opens.

## What a new harness is (the truthful README list, post-V40)

All of it is `harness/<id>/` — the contents of one plugin. Nothing here is
cImp-side work, and none of it can be made data, because it *is* the harness:

1. `harness/<id>/mod.rs` with `impl HarnessPlugin` — OOB transport, env,
   args, artifact writer, `spawn_sig`. **Design work**; nothing can make a
   harness's wire shape data.
2. One `HarnessDescriptor` entry — id, label, binaries, tab ids, consumer,
   features, sandbox grant rows, default tab template.
3. `settings_schema()` — the harness's own settings fields (decision 6);
   `routes()` if it pushes (decision 15); `native_tools()` (decision 16);
   `input_profile()` if it can be a delegation worker (decision 4, V39) —
   plus the `<id>.input.profile` row, its `declared_unprobed()` reason and
   the manual paste spike recorded per harness (decision 5).
4. A CHP hello (`serves`/`cannot`) and registry rows with `wired_in`.
5. `canaries()` + fixtures under `fixtures/harness/<id>/<version>/` and
   `probe()` (decision 17); goldens if the artifact is a file.
6. `MAINTENANCE.md` drift rows, a `HARNESS-NATIVE-TOOLS.md` section (checked
   against `native_tools()`), `CHP.md` route table if it pushes.

Everything outside that directory — settings storage, spawn sig, restart
hints, sandbox selection, loopback dispatch, tool classification, memory
kinds, health panel, probe runner, audit consumers, Settings UI, tab
naming, MCP exposure — is harness-neutral, consumes the plugin through the
interface, and tests 10(a)/10(b)/11 fail if a harness literal reappears.

## What changed vs the design

Every ruling taken inside a phase's discretion, or against the letter of a
locked decision, in one place, followed by the residuals V40 does NOT close — so the design above reads as what was approved
and this section reads as what was built.

1. **`SurfaceDigest` never existed.** Decision 25's `PerHarness<T>` list named
   `SurfaceDigest{claude,opencode}` (`mcp_host.rs:237`) as one of the
   fixed-arity-2 structures to replace. There is no such type in the tree; the
   ledger row was written from a sweep that mis-read the surface. Everything
   else in that list was real and was replaced.
2. **Routes and bodies moved together in Phase C.** Decision 15 said the plugin
   registers the *route table*; the design's § 4 sentence that the handlers
   "stay in `offload/loopback.rs`" was overturned by decisions 15 and 22 read
   together, so the ~900 lines of `handle_claude_*` bodies,
   `PermissionEventBody` and `classify_permission_event` went into
   `harness/claude/hook.rs` with the routes. `loopback.rs` left BOTH allowlists
   as a result, which is strictly better than the exemption the design assumed
   it would keep.
3. **Decision 24's `native_tools().arg_names()` is Phase C's
   `memory_arg_keys()`.** The tool-argument aliasing chain
   (`file_path`/`filePath`/`notebook_path`/`path`) was already a per-harness
   declaration by the time Phase E read the decision; adding a second accessor
   for the same table would have been a third vocabulary for one question.
4. **`LITERAL_ALLOWLIST` keeps one row, permanently.** `graph/index.rs` is
   exempted for `"tool_result"` — cImp's OWN `kind`-column discriminator in the
   usage table, chosen years before a Claude payload field took the same
   spelling. It is a word collision, not a dependency, and neither side is
   cImp's to rename. Any text that says the literal allowlist empties is wrong:
   it shrinks to exactly this row.
5. **`QuotaWindow` has six fields, not two.** Decision 19 described "a rolling
   usage quota"; the type the UI actually needs carries `id`, `label`, `short`,
   `description`, `used` and `resets_at`, because the widget renders a name, a
   duration, a tooltip and a reset time beside the percentage.
6. **`opencode_native_gate` is a frozen wire key.** Decision 6 folds the
   feature into the owning plugin's `settings_schema()`, and it did — but the
   settings-file key keeps its spelling, because it is a `TabInjectionOverrides`
   field on disk in every user's settings and a `/status` row name. Which
   harness owns the feature is `harness_list`'s `scoped_features`; nothing reads
   the key as an identity. Same exemption Rust gives `settings/schema.rs`.
7. **`harness_settings_schema` was subsumed by `harness_list`.** Decision 7
   anticipated a second IPC command for the declared settings fields. One
   roster call that already carries the descriptor, the affordances and the
   feature set carries the schema too; a second command would have been a second
   thing to keep in step with the first.
8. **`offload_derive_opencode_provider` → `offload_derive_local_provider`.** The
   "Add to OpenCode" path is a `ConfigWriter` on the plugin now (decision 26),
   so the IPC command that drives it names the capability rather than the
   harness that happens to have one.
9. **Amendment 0-f landed in Phase B, not Phase C.** `input_profile_status` was
   one global scalar for all harnesses; it is a `Settings.harness[<id>]` row,
   which put it in the settings-map phase rather than the drift phase. The
   `delegation.worker` gate reads the *neutral* verdict across every harness
   that declares an input profile, and skips those that do not.
10. **A neutral health panel is a second source, not a descriptor.** Decision 9
    said `PANELS = HARNESSES.iter()`. It is that plus one non-harness panel,
    because dropping it would hide the `delegation.worker` gate — the registry's
    first `Harness::ANY` row, which belongs to no directory.
11. **`probe::IMPLEMENTED` stayed in core** as the declared report order, even
    though decision 17 moved the probe BODIES behind the plugin. The order a
    report is read in is cImp's presentation, not a harness's claim.
12. **`NotebookEdit` is recorded `mutates_fs: false`.** Noted because it looks
    like a defect and is not one to fix here: the four `true` rows are exactly
    the names in the `PreToolUse` matcher V33 Phase F installed, and a test pins
    that direction. Widening the set is a V33 decision with a live-verify, not a
    V40 rename.
13. **Behaviour changes that are intended, and are the only ones.** V40 is a
    refactor, but these are visible and were each ruled deliberately:
    `GRAPH_GUIDANCE` now names each harness's own tools (an OpenCode session is
    told `read`/`bash`, not `Read`/`Bash`); the `harness` settings block is
    **machine** scope; auto-verify and every version-keyed drift rule run **per
    harness**, so `version_signature` is per harness and a Claude version notice
    dismissed before the upgrade re-fires once after it; an unregistered
    consumer, tool or harness is refused or fails closed (`mutates_fs` is `true`
    for an unknown name); an AI tab whose command matches no registered harness
    gets `ActivitySource::OutOfBand` and no TUI activity inference; an OpenCode
    `/memory/event` session id is accepted into the session key space even when
    it collides with a tab id, because the two spaces are now distinct; CHP is
    at 2, so a tab open across the upgrade reports `old_plugin` until restarted;
    a harness with no `spawn_sig` slot used to get no restart hint at all and
    now gets one.

14. **Decision 19 needed a seam it did not name: `HarnessPlugin::turn_usage_shape()`.**
    Phase D put `token_kinds()` and `origins()` on `UsageSource`, and OpenCode's
    `usage_source()` is `None` — so the harness that writes per-turn token rows
    through `/memory/event` declared nothing about the rows it writes, and the
    read boundary had nothing to ask. The declarations move onto the plugin,
    independent of the quota source: `harness_usage("opencode")` still answers
    *no usage source* (live-verify 14 stands), and now also answers what its
    recorded turns look like. The `harness_usage` payload changed shape with it
    (`token_kinds` / `origins` sit beside `source`, not inside it).
15. **`TurnOrigin` carries `subagent: bool`.** "Which lane is the fan-out lane"
    was a comparison against the string `"agent"` in three places, including
    one in the frontend. A harness with no fan-out declares one lane with the
    flag false. The `/memory/event` roll-up resolves the lane from the
    declaration, and a harness that declares no shape records **nothing**
    rather than being attributed a lane it never claimed — a dropped row is
    recoverable, a mis-attributed one is not.
16. **`cacheHitRatio` returns absence, and one cell changed.** A session with no
    denominator now renders `—` where it rendered `0%`. The backend's
    `SessionUsageRow.cache_hit_ratio` stays an `f64` (a float cannot carry
    absence and it has no other consumer), so this is a frontend reading of the
    same number, for token-less sessions only.

**Recorded residuals — real, bounded, and NOT closed by this milestone:**

* **Lane colour is still a fixed pair.** `usage_color_session` /
  `usage_color_agent` are settings fields, so a third declared lane gets the
  second lane's swatch and no donut CSS rule of its own. The donut, its legend,
  the lane strip and the share line are otherwise fully lane-general. Closing
  it is a settings change (a colour per declared lane), and it belongs to
  whoever adds the third lane.
* **The Cost card's four columns and the Sessions row's four stats are keyed
  off the price table**, which has exactly four rates. A category a harness
  does not declare renders `0` there rather than shifting the table out from
  under the `$/MTok` row beside it. Same for the stacked bar's five segments.
* **`agentBarClass` fails quiet for one paint.** The declared lanes arrive over
  IPC, so sub-agent bars are un-outlined until `harness_usage` answers rather
  than outlined by guessing `"agent"`. The lane strip is unaffected — it keys
  CSS off the stored lane id, as it always did.
* **`MemoryEventBody` still lives in `offload/loopback.rs`.** It is one
  harness's wire payload verbatim, and the ledger's destination for it was
  `usage_source()`. It names no harness id, so both allowlists stay clean and
  the layering tests are satisfied — but the row shape is that plugin's, and
  moving it behind `routes()` is the honest finish. Not a V40 defect; a
  recorded next step.

## Live-verify — Phase H (#100), the milestone's remaining gate

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
9. Settings page: Claude's `ext` fields (statusline, local provider,
   auto-verify) render under Claude from its declared schema with migrated
   values; OpenCode's (provider, native gate) under OpenCode. Confirm by
   temporarily deleting one `SettingField` from a plugin's
   `settings_schema()` in a dev build: the control disappears, the stored
   value survives a save (opaque round-trip), no other edit needed.
9a. Claude hook routes still arrive (permission prompt → notification,
    taint beacon, checkpoint) with the dispatch now registered from
    `harness/claude/hook.rs`; `toolclass::classify("Edit")` and `("edit")`
    return the pre-V40 classes (golden test), and an invented tool name
    from a hello-less source classifies as mutating/unknown.
10. Downgrade test: write a settings file containing `harness.codex = {...}`
    by hand, launch → no error, key preserved on next save, not shown.
11. `cargo test`: 10(a), 10(b) and the spawn-sig JSON-equality test are in
    the run; `vitest`: the registry parity test is in the run. Delete the
    OpenCode `spawn_sig` arm in a scratch build → 10(b) fails naming it.
12. Activity (decision 18): on a Claude tab, the avatar/tab state cycles
    idle → working → idle on a short turn and a fresh tab's welcome banner
    does not ring the notification (echo suppression intact); on an OpenCode
    tab the same states come from the OOB event stream with no TUI
    heuristic running (log line absent). Timing constants unchanged
    (golden test on the tuning struct).
13. Permission prompt (decision 21): trigger a permission prompt in a Claude
    tab → detected by both the TUI pattern path and the `Notification` hook
    path, notification fires once, avatar "awaiting permission", resolved on
    answer; `patterns.json` on disk is byte-identical to the pre-V40 seed
    (pristine-file reconciliation still recognises v040/v063/v070/v022
    files including the aider rows).
14. Usage (decision 19): the usage meter shows the same two windows with the
    same labels and numbers as the pre-V40 RC for the same session; the
    statusline renders inside the Claude TUI; `harness_usage("opencode")`
    returns "no usage source" (not zeros); the session-usage donut's
    main-session / sub-agents split matches the previous RC on the same
    transcript.
15. Hook ingress (decision 22): every Claude hook route still answers (a
    `PreToolUse` deny reaches the model, `additional_context` injection
    arrives, checkpoint beacon lands) with the same reply bodies (golden
    test on `HookReply` serialisation); `TOOL_CHECKPOINT_BUDGET` derived
    value equals 1800 ms for the two shipped plugins (asserted).
16. Drift (decision 23): the Harness health panel shows a version-drift row
    for OpenCode as well as Claude; *Mark verified* takes the harness from
    the row clicked; a drift card's fix hint names the mechanism supplied by
    the plugin (compare text to the previous RC — identical for Claude).
17. Model-visible text (decision 24): diff the MCP `instructions` string and
    the `--append-system-prompt` blob a Claude tab receives against the
    previous RC — identical; an OpenCode tab's `GRAPH_GUIDANCE` now says
    `read`/`bash` (the one intended change).
18. Affordances (decision 27): the three "/clear" strings, the four
    refresh-semantics sentences, the install hints and the Claude-only
    settings panels all still render, now from `harness_list`; no string in
    `src/` names a harness except in `src/lib/harness.ts` tests (grep).
19. CHP (decision 30): a tab spawned by the previous RC's artifact (stale
    CHP minor) still talks to the new binary; the health panel marks it
    stale by version, not broken.

20. **Settings scope (decision 5).** The `harness` block is **machine** scope,
    not user scope: after the 35 → 36 migration, confirm the per-harness rows
    are in the machine settings file and that a user-scope file carrying an old
    `claude_local` / `statusline` / `expose_commands_*` pair no longer shadows
    them. A second machine with a fresh profile gets defaults, not the first
    machine's values.
21. **Restart hint for a harness with no spawn-baked settings.** Before Phase B
    a harness with no `spawn_sig` slot got **no** hint when a spawn-baked
    setting changed. Flip a spawn-baked setting that applies to such a tab and
    confirm the hint now names it.
22. **Instruction inventory (decision 24, extended in Phase G).** Diff the
    `--append-system-prompt` blob a Claude tab receives against the previous RC:
    byte-identical, including the injection-hygiene paragraph, the tool-steering
    sentences and the offload nudge, which moved into
    `harness::instructions` in Phase G. Then turn `expose_commands` OFF for that
    harness and confirm the `run_command` sentence is **absent entirely** rather
    than softened — it is a separately inventoried slot because it is separately
    withheld. `harness_instructions` over IPC lists every slot for a tab.
23. **Usage payload shape (decision 19 remainder).** On the same transcript as
    the previous RC, the session-usage donut, its lanes and the per-model cost
    breakdown show the **same numbers** for a Claude session — the payload now
    carries declared `TurnOrigin` ids and a `TokenKinds` map instead of
    `OriginSplit { session_tok, agent_tok }` and four vendor-named fields, and
    the fixture test pins the arithmetic, but only a live session proves the
    render. Then open the same view on an **OpenCode** session: its lanes are
    labelled from its own declared origins, and a category it does not report is
    **absent**, never a zero bar.
24. **`harness_usage` absence, again.** `harness_usage("opencode")` still
    answers *no usage source* (not zeros) even though the harness now declares
    token kinds and origins for its recorded turns. The quota meter renders
    nothing for it; the donut still renders.
25. **Fixture relocation.** `cimp --harness-canary` and the plugin-golden tests
    read `fixtures/harness/<id>/goldens/`; regenerate an OpenCode tab's plugin
    and confirm the on-disk artifact still matches the golden byte for byte
    (`CIMP_BLESS_PLUGIN_GOLDENS=1` must produce **no** diff).
26. **The docs a reader lands on.** From `docs/MAINTENANCE.md` § *Registered
    harness CLIs*, follow the link to each harness's `README.md` and confirm the
    spike recipes and the *Mark verified* flow read correctly against the
    shipped build. `cargo test matrix_matches_maintenance_doc` is the machine
    half; this is the "can a maintainer actually run it" half.
27. **`HARNESS-NATIVE-TOOLS.md` twin.** The machine-checked section matches
    `native_tools()` for both harnesses (a test asserts it); spot-check that the
    prose recipes around it still describe the shipped `native_web_visibility`
    behaviour in `sensor` and `deny`.
28. **The version notice re-fires exactly once.** `version_signature` is per
    harness now. On a profile where a Claude version notice had been dismissed
    before this upgrade, confirm it re-fires **once** after it and stays
    dismissed thereafter — and that dismissing Claude's does not dismiss
    OpenCode's.
29. **Unknown everything, one pass.** In one session: a tab whose command is
    `foo` (no OOB reader, no TUI activity inference, no `delegate_task_foo`); a
    tool name from a hello-less source (classified mutating); `cimp --consumer
    codex` (refused, naming the registered ids); a settings file carrying
    `harness.codex = {...}` by hand (preserved, not shown, no error). Each is a
    fail-closed path and none of them may be answered with Claude's behaviour.
30. **Nothing regressed that Phase G only touched on paper.** Re-run the V39
    delegation regression (live-verify 8) after the Phase G test rewrites, and
    diff `tools/list` against the previous RC one more time — Phase G rewrote
    the V39 test literals over the registry, and a test that stopped asserting
    what it used to assert is invisible to `cargo test`.

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
