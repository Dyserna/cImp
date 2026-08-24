# Investigation — repo-wide refactoring survey (2026-08-23)

cImp is near feature-complete after ~42 incremental milestones. This survey answers:
*now that accretion is slowing, what is structurally worth cleaning up?* Four
parallel deep-read agents (offload · graph/audit/advisor · core backend · frontend)
plus mechanical passes (churn, size, prod/test split). All file:line references
verified against the working tree at rc.10 + the health-page restyle commits.
V42 (#106, headless core) scope is **excluded** throughout and cross-referenced
where an item touches it.

## 0. The corrected size picture

Raw `wc -l` overstates the problem badly: the repo keeps tests inline, and in the
biggest files **40–92 % of the lines are `#[cfg(test)]`**.

| file | total | production | inline tests |
|---|---:|---:|---:|
| offload/loopback.rs | 18 720 | ~9 950 | ~8 780 (47 %) |
| graph/index.rs | 8 984 | 5 584 | 3 399 |
| settings/schema.rs | 7 544 | ~5 922 | ~1 621 |
| settings/migration.rs | 7 215 | ~5 000 | ~2 150 |
| graph/service.rs | 6 963 | 5 470 | 1 492 |
| tabs/config.rs | 5 464 | **899** | **4 565** |
| advisor.rs | 4 159 | 1 962 | 2 196 (53 %) |
| audit/runner.rs | 4 665 | 2 542 | 2 122 |

There is **no `src-tauri/tests/` directory** — every test is an inline unit-test
module. Frontend: 73 268 lines / 212 files, **zero component tests** (all 42 test
files are pure-logic; markup changes are guarded by svelte-check + manual QA only).

Churn leaders (90 d): `settings/schema.rs` 146×, `SettingsApp.svelte` 144×,
`ipc/commands.rs` 138×, `main.rs` 123×, `offload/loopback.rs` 108×.

## 1. Defects found by the survey (fix now — not refactors)

These came out of the sweep and are wrong today. Each is small.

| # | defect | evidence | fix |
|---|---|---|---|
| D1 | **Tool reference list drifted**: Settings-visible `GRAPH_TOOLS` mirrors `graph/mcp.rs::tool_specs` by hand and is missing `context_lines`, `graph_architecture`, `graph_path` (21 listed, 24 real). *Verified.* | `src/lib/ToolActivityView.svelte:33-56` | add the 3 now; long-term render from Rust specs (§ 4, F-6) |
| D2 | **Error text isn't red**: `var(--text-error)` (SettingsApp `small.error`, `.roster-error`) and `var(--fg)` (EventsView) are declared in **no** theme file, no fallback → inherit body colour. *Verified.* | `src/SettingsApp.svelte:9513,9523`, `src/lib/EventsView.svelte:1341` | use `--text-danger-soft` / `--text-primary`; add a vitest guard scanning `var(--x)` against declared tokens |
| D3 | **Vacuous compile-time tripwire**: `assert!(FILE_COMPACT_LINES >= TOTAL_CAPACITY + FILE_COMPACT_SLACK)` where the constant is *defined as* that sum — can never fire, comment claims it guards lane growth. *Verified.* | `src-tauri/src/activity.rs:199,205` | restate against something real or delete |
| D4 | **CHP route column unpinned**: `harness/chp.rs:154-228` restates 22 route literals also spelled in loopback's dispatch; nothing asserts the two agree. A renamed core route silently stops CHP envelope observation with every test green. `core_route_paths()` exists and would pin it. | `harness/chp.rs:154-228,236,250` | one bidirectional test (live ⊆ dispatched, non-live ∉) |
| D5 | **toolclass TABLE not pinned to tool_specs**: a new graph tool absent from `offload/toolclass.rs` TABLE silently classifies as `External`. Test asserts counts, not coverage. | `offload/toolclass.rs:225-384,1140-1160` | add `tool_specs() ⊆ TABLE` test |
| D6 | **Dead write-only subsystem** (V20 residue): the `user_typed_tts`/`user_input_buf` echo-suppression set is written on the keystroke hot path and consumed by nothing (`pty/manager.rs:287` names it `_user_typed_tts`). ~200 LOC + 2 `AppState` fields + `processing/tags.rs`. | `ipc/commands.rs:305-333,845-930`, `ipc/state.rs:39,44`, `pty/manager.rs:287` | delete the subsystem |
| D7 | **Dead code, small**: `settings/TextAreaWithReset.svelte` (103 lines, zero importers); `harness/registry.rs` `get_mut`/`IndexMut`/`as_map` (~25 LOC, `#[allow(dead_code)]` naming a shipped phase); stale blanket allows on `offload/remote.rs:33,71` + `offload/mod.rs:81` hiding whatever is actually dead. | as listed | delete / drop the allows and let the compiler name the rest |
| D8 | **Stale docs**: `settings/persistence.rs:2317` claims overlays are never migrated (false since V40-I) + two orphan tombstone comments in `RESERVED_TAB_SPECS`; `graph/service.rs:12-16` claims the fs-watcher "arrives in Phase D" (it shipped, `service.rs:4102`). | as listed | one-line fixes |

## 2. The one big structural finding

**`offload/loopback.rs` is not primarily a route-split problem.** Its two biggest
coherent regions are non-HTTP subsystems consumed almost entirely from *outside*
the file:

- **taint-latch subsystem** — `loopback.rs:1866-4900`, ~3 035 lines (31 % of prod):
  `LatchScope/TabLatch/LatchRegistry/GatePolicy/Contamination/…` — consumers are
  `toolclass.rs`, `outbound.rs`, `agent.rs:34`, `ipc/commands.rs:2139,2196`,
  `harness/claude/hook.rs:97-106`.
- **discovery / instance resolution** — `loopback.rs:59-948`, ~890 lines:
  client-side "find an app instance" code used by `offload/mcp.rs`, `audit/mcp.rs`,
  `tabs/config.rs`, `sandbox/tabs.rs`, both harness plugins.

Extracting those two + relocating the 8 780-line inline test module removes
**~40 % of the production code and 47 % of the file** before any route handler
moves. The route-family split proper (handlers are near-independent; the only
shared state is `latches()`, `live_settings`, and `Arc<OffloadService>` on 4
routes) then shrinks to ~5 900 lines — and V40 Phase C already proved the
mechanics by extracting the 12 `/claude/hook/*` arms.

**The dominant cost is not coupling — it is source-text introspection.** 14 test
sites parse `loopback.rs`'s own source (`core_route_paths()` at :1398 — which
*panics* on an unparseable arm and is load-bearing from `harness/ingress.rs:207` —
`handler_body`/`fn_body`/`top_level_fn` assuming column-0 terminators), plus
cross-file scans `toolclass.rs:1836` and `harness/chp.rs:1120`. These are
security-property tests (they catch "gate deleted from a handler"); the split
re-points them to a `&[(path, source)]` list, never deletes them. Budget ~40 % of
the effort for that.

Also: `handle_delegate` sits *after* the test module (`:18500-18720`) — V39
appended at EOF. Any "prod then tests" tooling assumption is wrong on this file.

## 3. Ranked cross-subsystem candidates

Ranking = payoff ÷ (effort × risk), behaviour-preserving only. "Wave" = suggested
grouping (§ 5).

### Tier 1 — high payoff, proven mechanics

| # | candidate | shape | effort | wave |
|---|---|---|---|---|
| R1 | **loopback step 0: tests out** — `#[cfg(test)] #[path = "loopback/tests.rs"]` (precedent: `detection/updater/tests.rs`, `processing/tests.rs`). 12 `include_str!` sites become `../loopback.rs`. Proves the scanner re-pointing on a run-time-inert change. | S | A |
| R2 | **loopback: extract discovery** → `offload/discovery.rs`. Pure, self-contained tests, ~890 lines. | S–M | A |
| R3 | **loopback: extract taint-latch** → `offload/latch/{scope,registry,rows,override}.rs`. ~3 035 lines + ~2.5k of its tests. Main risk = privacy audit of ~25 types (compile error, not silent). | M | A |
| R4 | **loopback: route-family split** → `offload/loopback/{run,graph,audit,context,session,activity_edges,latch_routes,mcp,delegate,events}.rs` after R1–R3. Split by family — V40-C's "not-core" extraction produced a 3 546-line destination file; don't repeat that. | L | B |
| R5 | **machine-scope field matrix → table** (`settings/persistence.rs:953-1849`): 16 hand-written strip/promote/enforce/sync fns + hand-enumerated call sites in `load()`/`save()`. Failure mode of a missed cell = machine state leaking into a portable overlay (the exact bug class V38/V40-M-2 fixed twice). One `const MACHINE_SCOPED: &[MachineScopedField]` iterated in both. | M | B |
| R6 | **GraphService split** — 21 fields in ≥6 unrelated buckets (`graph/service.rs:275-410`): the read advisor (~950 lines incl. tests) and the live-session registry (~770) are already free-function islands with value-type seams. `graph/service/{readadvisor,live,walk}.rs`. Refutes "graph is cohesive" — the *store* is cohesive, the service is a god object. | M then L | B |
| R7 | **advisor.rs split** — `drift_rules` is one 744-line fn (`advisor.rs:975-1719`) whose second half (329 lines) is the detection/updater advisor reading only `offload::detection` types; its 6 `Signals` fields + 580 test lines partition cleanly. `advisor/{drift,detection}.rs`. Preserve card order; keep `ALL_RULE_IDS` reachable. | M | B |
| R8 | **graph/mcp.rs de-homing** — its own doc says run_check "is independent of the graph" (`mcp.rs:322`): ~1 100 lines of run_check/run_command dispatch + the process-wide `SurfaceStats` move to `graph/mcp/{checks_tools,surface}.rs` (or out of graph/ entirely) behind unchanged re-exports. Kills a false "graph owns checks" layering signal (11 `crate::checks` refs). | M | B |
| R9 | **migration floor raise — as a TWO-part change.** The framing "raise `MIN_OVERLAY_SCHEMA_VERSION`" is wrong: that constant is **overlay-only** (single read, `migration.rs:125`); the global path (`migrate_if_needed`) has **no floor**, and deleting pre-v30 steps naively causes *silent data loss* — an old global file parses via `#[serde(default)]`, loses every moved field, and keeps its old stamp forever. Required: (1) an explicit global floor that quarantines-and-reseeds (reuse the corrupt-file path, `persistence.rs:772-780`) or refuses to launch, handling unstamped pre-v10 files; (2) then raise the overlay floor and delete v1.0→v29: **~2 250 prod + ~2 150 test LOC (−61 % of migration.rs)**, 96 tests deleted, 5 rewritten, 5 stale fixtures outside the file. This is also the Aider cleanup — the only deletable Aider code lives in those steps. | M (needs a design decision: quarantine vs refuse) | B |
| R10 | **settings/schema.rs per-domain split** — the churn concentrator (146× / 90 d; ~35 % of its commits co-touch offload/, ~24 % graph/). Domain blocks are already contiguous; `settings/mod.rs` re-exports `schema::*` so import churn is zero. `schema/{tabs,offload,graph,workbench,mcp,notifications,media,ui}.rs`. | M–L | C |

### Tier 2 — solid, smaller

| # | candidate | shape | effort |
|---|---|---|---|
| R11 | **Unify the 5 test source-tree walkers** into `crate::rustsrc` (its docs already call for it). They disagree on CR-stripping, dot-dir skipping, and the >100-files vacuity guard — the CRLF variant already shipped a green-on-Linux/red-on-Windows bug. One caller at a time (each gained check can make a tripwire fire — that's a fix, but stage it). | S |
| R12 | **CozoDB stage-and-swap dedup** — `migrate_usage_stat_origin` vs `migrate_mem_note_shape` are documented as "mechanically identical" crash-safety engines; one `stage_and_swap()` helper. Keep the frozen test fixtures. | S |
| R13 | **graph/index.rs submodule split** — memory/usage half is already a separate 1 760-line `impl` block; `index/{memory,usage,vectors,viz,arch}.rs` beside the existing `notes.rs` precedent. MUST move the `LITERAL_ALLOWLIST` `"graph/index.rs"` row (`harness/layering.rs:303`) in the same commit; don't relocate `mem_note` queries out of `notes.rs`. Also folds the near-identical doc/code vector-store function pairs. | M |
| R14 | **tab-commit helper** — `ipc/tab_lifecycle.rs` writes persist→register→signal→rollback→activate 7× (~400 LOC) and has **zero tests**. ⚠ This IS V42 Phase A's service-layer body for tab lifecycle — do it inside V42, not separately. | M (V42) |
| R15 | **state/manager.rs**: 530-line `run()` select-loop → `struct Loop` with per-signal methods; collapse the 9 one-line `emit_*` forwards. Keep `note_signal` ahead of every `continue`. | M |
| R16 | **main.rs bootstrap**: 1 137-line `main()`, 22 `_for_X` clone aliases → `struct Wiring` + `fn wire_*` per service (ordering is load-bearing and documented — keep block order). Not V42 scope (bootstrap, not commands). | M |
| R17 | **sandbox/mod.rs split** (runtime-profile inference ~975 / activity recording ~900 / the actual model) + one `once_per_session(slot, key)` helper for the 9 drifted copies of the static-dedup preamble (per-site statics stay — a shared static would merge dedup namespaces). | M |
| R18 | **Feature table** (`settings/injection.rs`): 12 variants × ~14 hand-kept sites (18 greps for one feature) → one `const ROWS` table for the seven *derivable* predicates. `spawn_baked` must stay const-evaluable (const-asserted at `tabs/config.rs:415`). Do NOT table the override structs' exhaustive matches — the compile error on a new variant is the mechanism (their docs say so). | M |
| R19 | **outbound.rs**: lift the pure URL-extraction block (~580 lines, 18 pure fns + ~600 test lines) to `outbound/urlscan.rs`. | S |
| R20 | **detection/updater split** along its 9 marked sections; only offload module with tests already external. | M |
| R21 | **OffloadTask hoist**: `service.rs run/run_on` thread 9/14 positional params; the bundle type already exists (`agent.rs:527`) but is built at the bottom of the chain. Keep the pinned call spelling `service.run(`. | S–M |
| R22 | **loopback micro-dedups** (with/after R4): decode-body-or-400 ×14 (preserve per-route wire bytes — 400 bodies are NOT uniform), `hook_admit`≡`delegate_admit` (keep pinned call spellings), the 3 identical session-push handlers. | S |
| R23 | **activity.rs kind table**: enum/as_str/cap-const/if-chain quadruple → one `KINDS` table + `const fn cap`; explicit "no cap" cell for `injection_flag`; fixes D3 alongside. | S |
| R24 | **notifications matrix**: `allowed_for` + `notification_text` encode the same (TabKind × Event) table twice → one `slot_for()`; keep `allowed_for` as a documented thin wrapper (defence-in-depth doc stays). | S |
| R25 | **now_ms ×4** → one fn. `PtyStart` param struct (10→9 args with D6; keep the H1-R3 no-await window intact). Post-V40 glue: plugins call `settings::injection` directly instead of two 4-line `tabs::config` forwards; delete the duplicate `NativeWebVisibility` alias (fixes the last core↔plugin upward edge V40 chartered). | S |
| R26 | **Parameter objects for the graph tool chain** (`ToolCall<'a>` through run_graph_tool→dispatch_recorded→run_tool) and audit's spawn chain — 20 `too_many_arguments` allows across graph/audit. | M |
| R27 | **⚠ security-adjacent, flagged not scheduled**: `audit::spawn_and_capture` (283 lines) vs `checks::mod.rs` sandboxed spawn (~145) are line-for-line parallel around the Landlock/AppContainer boundary → one `procutil::run_confined()`. High payoff (one boundary, not two) but touches the TCB; do deliberately with both test suites green. Same class: the 6-8 caller-identity resolvers in loopback — share the *mechanical* half only, never unify the policy (fail-open/fail-closed differences are documented and load-bearing). | M–L |

### Frontend tier

| # | candidate | shape | effort |
|---|---|---|---|
| F1 | **Fix D1 + D2 now**; add the token-exists vitest guard. | S |
| F2 | **CSS dedup, four narrow families** (~1 000 of 11 128 style lines): `.status-button` ×11 files/324 lines (2 already drifted) → theme-level rules; **ModalShell** dialog component (7 dialogs, ~330 shared shell lines + 7 Escape handlers; scoped-CSS boundary means the shell must own the action buttons); shared diff-hunk renderer (DiffView↔CheckpointDiffView, 155 identical CSS + 20 identical markup lines); SectionNav ×4 (~140 lines — **after** V42-C, its persistence half changes there). | S–M each |
| F3 | **SettingsApp split, in the proven order**: (a) hoist the ~390-line bare-element form CSS to a shared sheet (pixel-diff check), (b) field primitives — Toggle/SelectField/NumberField over the 172 `currentTarget as` coercions (**−1 500 to −2 500 lines**, keeps `patch()`'s clone-mutate-push contract, no `bind:`), (c) then extract sections lowest-risk-first (about → checks/mcp → pricing → compose → … → the offload/injection/sandboxing triangle as ONE group). Only 7 of 264 declarations cross sections; the real blockers are scoped CSS and the eager 123-line `onMount` (lazy-loading a section is a behaviour change — keep loads parent-owned). `restartRequired` baseline machinery stays parent-owned. | M+M+M, triangle L |
| F4 | **CodeIntelligenceView split first** — better first target than SettingsApp: 4 568 lines, only 5 cross-section identifiers, `overview` owns 82/216 decls + 89/212 CSS rules exclusively. Hoist the 67 shared CSS rules first. ⚠ overview is usageMath-driven → V42-D moves that logic; sequence after or accept a rebase. | M |
| F5 | **GraphView engine → plain TS** (`graph/{sim,camera,render}.ts`, ~640 lines currently untestable in-component; keep the non-reactive-state boundary). ⚠ Confirm V42-D's "graph" doesn't mean the sim — if it does, skip. | M–L |
| F6 | **Codegen alongside V42-E**: tool reference lists (D1) and the 17 hand-matched Tauri event-name literals → generated constants/types. | S each |
| F7 | **Popover clamp/dismiss helper** ×3 verbatim copies (one documents itself as a deliberate copy). | S |

### Test-placement wave (mechanical, do AFTER the module splits land)

`loopback` (R1, first), `tabs/config` (899 prod / 4 565 test — also ~50 harness-overlay
assertions whose production code moved to `harness/` in V35-K/V40; relocating those
is a second, deliberate pass), `graph/index`, `advisor`, `audit/runner`,
`graph/mcp` (9 test mods — but `harness/layering.rs:108-117` calibrates its
`executable_text` cutter against this file's shape; verify those tests after).
Each is S and buys 40-55 % file-size reduction with zero runtime change.

## 4. Do-NOT-touch list (looks bad, is right)

Consolidated from all four reports — reject future "cleanup" PRs against these:

- **migration ladder shape** — append-only frozen history; only deletion-via-floor, never generalisation.
- **theme.css per-theme token duplication** — policy, stated in-file at :20-36.
- **`TabInjectionOverrides`/`WorkerInjectionOverrides` exhaustive matches** — the compile error is the mechanism.
- **`harness/layering.rs` tombstone prose + allowlists** — the reviewable record; self-enforcing tests already prune stale entries.
- **`offload/toolclass.rs`** — best-factored file in offload; its 52 % test share is the source-scanners that pin every routing site.
- **loopback's hand-rolled HTTP + `ct_eq`** (~120 lines) — deliberately not a framework; move whole, never modernise.
- **the `include_str!` source-scanner tests** — fragile by design, each catches a silent security regression; re-point, never delete. **Amended 2026-08-24 (V42 Phase E, #134):** the one exception is a scan whose *target* stopped being hand-written. Phase E generates the `settings/schema.rs` tree into `src/lib/settings/generated/settings.ts` (ts-rs, written during `cargo test`, committed, CI-diffed), so a scan asking "does every wire key of this struct appear in the TS mirror?" now asserts that a generator generated. **Retired:** `settings::schema::tests::{code_audit,tool_plugins}_field_names_mirrored_in_types_ts` and their shared `AUDIT_TS_TYPES` const, plus the field-name half of `sandbox::tabs::tests::sandbox_settings_fields_are_mirrored_in_types_ts` — three scans, all over types `generated/settings.ts` now emits. **Re-pointed, NOT retired** (the rule still binds wherever the target is still hand-written): that sandbox test's other half, the `tabs: false` defaults check, now reads `generated/defaults.json` — a committed artifact the frontend imports, so a hand-edit or a stale regeneration is exactly what it still catches; and `harness::health`'s `the_health_field_names_reach_the_frontend`, whose eleven panel field names mirror `harness/health.rs`, not `schema.rs`, and stay hand-written in `types.ts` — it now scans both files rather than losing a live tripwire to a codegen that does not cover it. **Untouched and still green:** `settings::frontend_mirrors` (value constants + Settings-window prose pointers), `checks::tests`' `CheckDef`/`ParserKind` mirror, `harness::contract`, `graph::memory`, `audit::runner`. The test to apply before deleting one of these is not "is it fragile" but "is its target still hand-written".
- **`detection/mod.rs` compose ordering; `delegation::registry()` global; `AiTabId`; `StatusChip`; `draftSync.ts`; `dialog/store.ts`; EventsView's bespoke resizable table; `processing/prose.rs` vs `tts/prose.rs`; per-struct `impl Default` in schema.rs; the 22 `busy=true/finally` sites; `harness/_retired/aider.rs`** — each documented cohesive/deliberate (details in the four agent reports).
- **`truncate_chars` ×3** — three different user-visible semantics; not a dedup.

## 5. Suggested packaging

- **Wave 0 (defects, this week, no milestone needed):** D1–D8 + R25's `now_ms` + D5/D4 tests. All S; several are one-liners. ~600 LOC deleted, two user-visible bugs fixed.
- **Wave A — "loopback shrink" (V43 candidate, before or alongside V42-A):** R1→R2→R3. Removes ~47 % of the repo's biggest file with proven mechanics; makes V42's service layer wrap sane modules. R4 optional tail.
- **Wave B — "table the invariants" (V43/V44):** R5, R7, R8, R9 (with its design decision), R6-first-half, R11, R12, R23, R24. Theme: every hand-kept parallel structure becomes one table + one test.
- **Wave C — "big-file splits" (after V42 lands, low urgency):** R10, R13, R15, R16, R17, R20, remaining R6, test-placement wave.
- **Frontend wave — interleave with V42:** F1 now; F2 anytime; F3(a)(b) before V42's SettingsApp-adjacent phases; F4 after V42-D; F6 rides V42-E.
- **R27 (sandbox spawn unification)** — its own small milestone with both test suites + a live sandbox battery, per its TCB status.

Sequencing constraints worth pinning in briefs: every `graph/` file split moves its
`harness/layering.rs` allowlist row in the same commit; loopback splits re-point the
source-scanners as part of the definition of done; frontend splits hoist shared
scoped CSS *before* extracting children; nothing in Wave A–C changes wire bytes or
user-visible strings.

## 6. What the survey did NOT find

- No repo-wide dead-code problem: retired features (tabs, Aider) were cleaned properly; dead code is scarce and enumerated in D6/D7.
- No Svelte 4 residue: zero `$:` statements; store-vs-runes usage is the correct split.
- No cross-view duplication in the activity/timeline family — already factored into `activity.ts`/`StatusChip`/`viewSection.ts`.
- No IPC-layer mess: 172 `invoke()` sites, zero in `.svelte` files, already grouped by domain — V42 starts clean.
