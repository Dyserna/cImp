# V40 harness-registry — adversarial review record

**Branch:** `feat/v40-harness-registry` (review base `b1bea91`)
**Worktree:** `P:\Documents\AI-private\cc-avatar\cctts-v40`
**Date:** 2026-08-22 · **Fix pass:** 2026-08-23

Three review lenses were commissioned — **seams/security**, **parity**, and
**frontend**. Only the seams report (`code-review-V40-seams-2026-08-22.md`, which
carried its own frontend section) was written to disk; the parity and frontend
reports were never delivered as files, and their findings reached this pass as
the orchestrator's per-finding directions. Everything below is the one record:
what was found, what was done about it, and where. The three lens files are
deleted — this file replaces them.

**Ids.** Seams findings keep their own ids (`H-1`, `M-n`, `F-n`, `L-n`). Findings
that came from the parity lens are prefixed `P-`, from the frontend lens `FE-`.
Where two lenses found one thing, the row says so and the fix is one commit.

## Final state

`cargo test --bin cimp` **2845 / 0 / 6** (baseline 2822) · `npx vitest run`
**856** (baseline 842) · `npm run check` **361 files, 0 errors, 0 warnings**
(baseline 360) · `npx tsc --noEmit` clean · `cargo check --all-targets` **0
warnings**.

## Findings

| # | Sev | File | One line | Status |
|---|-----|------|----------|--------|
| **H-1** | HIGH | `offload/loopback.rs`, `offload/mcp_host.rs`, `offload/service.rs` | `?consumer=offload` was served Claude's granted MCP servers while its taint-latch key resolved to an activity source that names no configured tab — grant and identity split on one request, with the EXTERNAL budget uncharged | FIXED `7756975` |
| **M-1** | MEDIUM | `settings/persistence.rs`, `ipc/commands.rs` | The declared parse boundary for `harness.*.ext` never ran on the load path, so a hand-edited value reached the launch path as one thing and the Settings window rendered another | FIXED `6970cfe` |
| **M-2** | MEDIUM | `settings/persistence.rs` | `harness` joined `OVERLAY_BANNED_KEYS`, silently narrowing five per-project settings to machine scope and deleting their project values on the first post-upgrade save | FIXED `66757a8` |
| **M-3** | MEDIUM | `settings/schema.rs`, `harness/layering.rs` | `AiTabId` / `TabId` / `default_ai_tab` are closed enums keyed to the two shipped harnesses and 10(b) did not police the join — a third descriptor would compile, pass every test, and be silently dropped from the tab machinery | FIXED `adecd1b` |
| **M-4** | MEDIUM | `offload/loopback.rs` | An **empty** (not absent) agent discriminator resolved to `unknown`, switching off CHP stale-artifact recording and the quiet-capability detector for exactly the pre-upgrade artifacts they exist to catch | FIXED `05b20df` |
| **M-5** | MEDIUM | `settings/migration.rs` | The 35→36 merge replaced an existing `harness.<id>.ext` object wholesale, so a partial `ext` discarded every key the step had just carried over | FIXED `c02a326` |
| **F-1** / FE-M-1 | MEDIUM | `lib/settings/types.ts`, `SettingsApp.svelte` | The frontend spawn-signature mirror answered a hardcoded `true` while the roster loaded, and the Settings window captured its restart baseline inside that window | FIXED `f5cde79` |
| **F-2** / FE-H-1 | MEDIUM | `lib/harness.ts`, `SettingsApp.svelte` | `reservedAiTabIds` bootstraps and `labelForTabId` did not, so the pre-roster window rendered **unlabelled** AI-tab enable checkboxes — destructive controls, a tick kills a PTY — and the comment claiming otherwise was false | FIXED `f5cde79` |
| **F-3** / FE-H-2 | MEDIUM | `lib/harness.ts`, `SettingsApp.svelte` | A failed `harness_list` was silent, unretried and indistinguishable from "still loading"; several controls simply vanished for the window's lifetime | FIXED `f5cde79` |
| **F-4** / P-M-5 | MEDIUM | `lib/CodeIntelligenceView.svelte` | Declared turn lanes were unioned across harnesses, first-id-wins, and rendered for every session | FIXED `f5cde79` |
| **F-5** / FE-M-5 | MEDIUM | `lib/harness.test.ts`, `lib/settings/HarnessExtForm.svelte` | The parity test stopped at the container, so `fields[]` / `scoped_features[]` and `SettingKind` were unchecked — and the form's `{:else}` rendered an unknown kind as a text box that writes the wrong type | FIXED `f5cde79` |
| **F-6** / FE-M-3 | MEDIUM | `SettingsApp.svelte`, `harness/plugin.rs`, `harness/info.rs`, `harness/opencode/harness_plugin.rs` | The Offload section wrote two `ext` keys by hardcoded string rather than by declaration | FIXED `f5cde79` |
| **FE-M-4** | MEDIUM | `lib/settings/types.ts`, `SettingsApp.svelte`, `harness/contract.rs` | `gated_controls?.[CONTROL] ?? ''` fails **open**: a control renamed in Rust silently un-gated the toggle that installs a `PreToolUse` hook | FIXED `f5cde79` |
| **FE-M-7** | MEDIUM | `lib/settings/HarnessExtForm.svelte` | The spawn-baked restart warning hid inside `{#if field.hint}`; an emptied `int` was dropped on the floor; an out-of-range `enum` silently rendered the first option | FIXED `f5cde79` |
| **P-M-1** | MEDIUM | `harness/verify.rs` | A failing first harness starved every harness behind it — the worker answered its already-run memo with `return`, and a failed run leaves the harness pending forever | FIXED `2748c77` |
| **P-M-2** | MEDIUM | `harness/verify.rs`, `settings/persistence.rs` | The first post-upgrade launch spawned a harness's own CLI to probe it even when its tab had never been enabled, because the migration leaves `last_seen` set and `last_verified` empty | FIXED `2748c77` |
| **P-M-3** | MEDIUM | `lib/tabs/state.ts` | The `activeTab` placeholder re-seed could walk back a restored/broadcast tab id (and, as written, was dead code guarding on a value that can never occur) — same site as `L-18` | FIXED `f5cde79` |
| **P-M-4** | MEDIUM | `harness/plugin.rs`, `harness/claude/plugin.rs`, `harness/claude/settings.rs`, `tabs/config.rs` | Declaring the three `local.*` rows `spawn_baked` folded them into the signature unconditionally, so editing the local proxy URL with no local-provider tab raised the restart hint for a change that changes nothing | FIXED `2748c77` |
| **P-M-6** | MEDIUM | `offload/loopback.rs` | A forged `/workbench/tool_checkpoint` from an unregistered agent minted a snapshot for any tool name — `mutates_fs` fails closed for an unknown *vocabulary*, which is the wrong answer to "may this *caller*" | FIXED `2748c77` |
| **P-M-7** | MEDIUM | `offload/toolclass.rs`, `harness/native.rs` | Harness-native names lost their declared class, so `classify("Edit")` fell to EXTERNAL — which the latch **admits** under an EXTERNAL latch and which marks an open tab externally-contaminated | FIXED `2748c77` |
| **L-1** | LOW | `ipc/commands.rs` | Out-of-band field preservation iterated `cur.harness`, so a registered harness with no live row took `last_seen`/`last_verified`/`auto_verify` from a frontend-fabricated snapshot | FIXED `6970cfe` |
| **L-2** | LOW | `offload/loopback.rs` | `core_route_paths()` scraped the whole file and had to skip what it could not parse, so a wrapped dispatch arm would drop out with the shadow test still green | FIXED `2a2860f` |
| **L-3** | LOW | `harness/registry.rs` | Plugin subcommands had neither the collision guard nor the core-shadow guard plugin routes have | FIXED `2a2860f` |
| **L-4** | LOW | `main.rs` | `resolve_consumer` accepted `offload` for `--code-audit-mcp`, whose every scan is then refused at `/audit/run` | FIXED `2a2860f` |
| **L-5** | LOW | `sandbox/tabs.rs` | `HarnessId::ANY` is a type-valid sandbox harness with no grant table | FIXED `2a2860f` |
| **L-6** | LOW | `settings/migration.rs` | The frozen migration's `ext` key literals were untied to the plugins' declarations | FIXED `c02a326` |
| **L-7** | LOW | `settings/persistence.rs` | `sync_harness_into` rewrote the global file on essentially every save after the migration | FIXED `6970cfe` (gone with M-1 + M-2) |
| **L-8** | LOW | `offload/outbound.rs` | The "unattributed" audit lane is shared by `offload`, `audit` and forged callers, so the doc's "its own lane" is true only against other harnesses | FIXED `2a2860f` (doc) |
| **L-9** | LOW | `offload/loopback.rs` | `/latch/state` with a mismatched consumer degrades to `latch:"open", contaminated:false` where develop answered the Claude tab's real view | **DECLINED** — see below |
| **L-10** | LOW | `harness/verify.rs` | Stale doc: "`Harness` is a two-value enum in practice" | FIXED `2a2860f` |
| **L-11** | LOW | `lib/harness.ts` | The two `from_command` implementations disagree on a trailing space and a Windows path read on POSIX | FIXED `f5cde79` (documented + pinned as deliberate) |
| **L-12** | LOW | `lib/DelegationPopover.svelte` | A tab with an unknown command rendered `delegate_task_` and its Manual radio was not disabled | FIXED `f5cde79` |
| **L-13** | LOW | `lib/terminals.ts` | A startup `pty_exit` wrote "**Shell** failed to start." into a reserved AI tab's error card | FIXED `f5cde79` |
| **L-14** | LOW | `lib/composeState.ts` | Attachment format for a spawned `ai-<uuid>` duplicate always fell to the default | FIXED `f5cde79` |
| **L-15** | LOW | `lib/tabs/types.ts` | `isShellTab` answers `true` for every `ai-<uuid>` duplicate, so those tabs lose AI mouse/wheel handling | **DEFERRED** — see below |
| **L-16** | LOW | `lib/harnessIdentity.test.ts` | The identity scan's regex was word-boundary only, so it would have passed on develop's own `getClaudeUsage` | FIXED `f5cde79` |
| **L-17** | LOW | `lib/settings/harness.test.ts` | Not a parity test: the `ext` keys were hand-typed | FIXED `f5cde79` |
| **L-18** | LOW | `lib/tabs/state.ts` | The re-seed subscription was dead code guarding on a value that can never occur | FIXED `f5cde79` (with `P-M-3`) |
| **W-1** | WEAK | `harness/ingress.rs` | `DRIFT_TOKENS`' literals were "pinned" by a scan of the file that declares them, which the `const` itself satisfies | FIXED `2748c77` |
| **W-2** | WEAK | `advisor.rs` | The `"{token}:{version}"` dismissal wire form — a persisted signature — was pinned by nothing | FIXED `2748c77` |
| P-LOW (copy) | LOW | — | The parity lens's LOW list (probe report tail order, tooltip / drift-card copy) | **NOT ADDRESSED** — see below |

## Declined, deferred, not addressed

* **L-9 — DECLINED.** `/latch/state` documents itself as *"always 200, and always
  fail-open in shape"*, with a **stated residual** (#48) that an id keying no
  registry entry answers `latch: "open"`. A mismatched consumer now lands in
  exactly that documented case rather than in a new one, it is unreachable
  through the shipped plugin (`templates/plugin.js` hard-codes its consumer), and
  the `gate` half still comes from the app-wide resolution. Changing it means
  either refusing on a hot path whose client reads `{gate, latch}` off a 200 (a
  behaviour change needing its own live-verify) or re-widening
  `source_for_consumer`, which would undo the deliberate V40 narrowing. Recorded
  as a Phase H eyes-on item instead.
* **L-15 — DEFERRED.** Identical in effect to develop, so not a V40 regression.
  Fixing it flips mouse-tracking suppression, copy-on-select, right-click paste,
  the V20 wheel encoding, the closed-tab overlay and the restart affordance for
  every user-created `ai-<uuid>` tab — a visible behaviour change with no test
  coverage of those affordances, which belongs to a phase that can live-verify
  it, not to a review pass inside a refactor milestone.
* **P-LOW (copy) — NOT ADDRESSED.** The parity report was never written to disk,
  so its LOW list reached this pass only as a summary ("probe report tail order,
  `--consumer` refusal text, tooltip/drift-card copy"). The `--consumer` refusal
  text was rewritten under `L-4` and now names the registered list on both
  subcommands. The other two could not be acted on without the finding's own
  before/after. Phase H live-verify items 16, 17 and 22 already diff that text
  against the previous RC by hand, which is where they will surface.
* **M-2 residual.** The narrowing is undone, but a project overlay written
  BEFORE the upgrade still carries the pre-36 spellings (`statusline.enabled`,
  `claude_local.*`, `code_audit.expose_<id>`, `offload.opencode_provider*`), and
  a project overlay is never schema-migrated — a documented, pre-existing
  limitation of the overlay format. Those keys are ignored and dropped on the
  next save exactly as any other unknown key is. What changed is that the user's
  re-set now **sticks per project** instead of being erased again.

## Deviations from the fix brief, stated

* **M-2's field list.** The brief named four out-of-band fields (`last_seen`,
  `last_verified`, `auto_verify`, `input_profile_status`) and said everything
  else should be per-project "exactly as on develop". Those two clauses conflict
  for **`expose_commands`**, which on develop was `tool_plugins.expose_commands_<id>`
  and was already stripped from overlays. It stays machine scope: it decides
  whether `run_command` is advertised — a capability grant — and a project config
  file lives inside the sandbox boundary a confined tool can write. The governing
  clause ("exactly as on develop") is what was followed.
* **P-M-7's blast radius.** Restoring the declared class from `native_tools()`
  gives OpenCode's lowercase ids `LocalCapability` too, where develop had them
  unclassified (its table carried only Claude's four). That is a **tightening**
  in both directions that matter (`Latch::blocks` refuses them under an EXTERNAL
  latch; `Latch::engage` moves an open tab to `Local` rather than `External`),
  and it is what decision 16 asks for — the harness's own declaration, asked of
  the registry. One test assertion in `offload::agent` that pinned the *class*
  rather than the *behaviour* was updated; the behaviour it exists for (a name no
  dispatcher serves neither latches nor is refused, on the worker's route) is
  unchanged and rests on `dispatchable`, not on the class.

## One finding the fix pass believes was mis-stated

**Seams L-18 and parity M-3 describe the same site with opposite symptoms.**
L-18 says the `activeTab` re-seed is dead code (its `cur === ''` guard can never
be true, because `defaultTabId` has a bootstrap fallback); M-3 says the re-seed
yanks a restored tab. L-18 is correct as the code stood — the guard cannot fire.
M-3's hazard is real but latent: it becomes live the moment the bootstrap
fallback goes away, and nothing in the file said so. Both are closed by one
change (`f5cde79`): the store's writes go through a wrapper that records when
anything authoritative has spoken, and the re-seed is inert from that moment
whatever the ordering — so the correction is meaningful *and* cannot walk back a
real value.

## Notes for Phase H (#100) live-verify

Additions the fixes make necessary or newly checkable. These are folded into the
milestone doc's live-verify list as items 31–37.

1. **H-1.** Drive `/mcp/list`, `/mcp/call`, `/run` and `/graph_run` under
   `?consumer=offload` and `?consumer=codex` while a tab is latched `external`.
   Expect: `offload` served under Claude's grants **and refused by Claude's
   latch**; `codex` refused (400) on all four. Live-verify 29 covers the tab, the
   tool name, the proxy start and the settings file, but none of these routes.
2. **M-2.** A project overlay carrying `harness.<id>.ext.statusline = false` and
   `expose_code_audit = false` before launch: both must take effect for that
   project, survive a Settings save, and the Events feed must show a
   machine-scope row naming any `last_seen` / `expose_commands` the overlay also
   carried. The old item 20 tests the user/machine split, not the project one.
3. **F-1 / F-2 / F-3.** Open Settings with `harness_list` deliberately failing
   (or slow). Expect a loading line, then a banner with *Try again* — never an
   unlabelled AI-tab checkbox, and never a restart hint on the first edit.
4. **P-M-2.** On a profile whose OpenCode tab is disabled, confirm the first
   post-upgrade launch spawns **no** `opencode serve` probe, and that *Run checks
   now* on that row still does.
5. **P-M-4.** With no local-provider tab open, edit the local base URL and
   confirm **no** restart hint; tick a tab's *Use local provider*, edit it again,
   and confirm the hint fires naming that harness only.
6. **P-M-7.** In a Claude tab latched `external`, confirm a native `Edit` is
   refused by the latch (it was admitted on this branch before the fix), and that
   an ordinary `Edit` in an untainted tab leaves the tab's latch at `local`, not
   `external`.
7. **L-9 (eyes-on, not a fix).** `/latch/state` with a garbage `consumer` answers
   the fail-open shape. Confirm the shipped plugin never sends one.
