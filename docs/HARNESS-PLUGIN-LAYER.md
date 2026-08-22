# The harness plugin layer

**Status:** V35 phases A–M complete (2026-08-17), plus the same-day
capability-refresh batch: both harnesses re-audited against current upstream
(Claude Code 2.1.233, OpenCode 1.18.18), the two Claude beacons and OpenCode
server auth moved D → B, `PostToolUseFailure` wired, and the
`claude.hook.posttooluse` drift gap closed. § 3.2 and § 11 reflect it.
**V40 (phases A–G) then put a registry under all of it** — one descriptor and
one `HarnessPlugin` impl per harness, with every per-harness fact moved behind
that interface. § 2.2 is the registry; §§ 8 and 9 are rewritten against it. This
document describes the layer *as it shipped*, and is the long-form twin of
[`src-tauri/src/harness/README.md`](../src-tauri/src/harness/README.md).

cImp rides two user-installed, aggressively self-updating CLIs it does not pin.
Everything it knows about them lives in `src-tauri/src/harness/`, behind one
versioned protocol, with one machine-readable list of every dependency and a
build that fails when a new one enters unrecorded. This document is how that
works and how to extend it.

## 1. What this is, and what to read instead

| You want | Read |
|---|---|
| The wire contract — routes, bodies, headers, compatibility rules | [`docs/CHP.md`](CHP.md). **Authoritative for message shapes; this document never restates them.** |
| The 60-second in-tree entry point | [`src-tauri/src/harness/README.md`](../src-tauri/src/harness/README.md) |
| What one harness's plugin actually declares | that harness's own README — [`claude`](../src-tauri/src/harness/claude/README.md), [`opencode`](../src-tauri/src/harness/opencode/README.md) |
| The roster itself — descriptor fields, `HarnessId`, `PerHarness<T>` | § 2.2 below, and `src-tauri/src/harness/registry.rs` |
| The same how-to beside its siblings | [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) § *Adding a harness plugin* |
| Why it is shaped this way — locked decisions, findings, phase-by-phase record | [`docs/MILESTONE-V35-harness-resilience.md`](MILESTONE-V35-harness-resilience.md) |
| The designs the milestone executed | [`DESIGN-harness-plugin-architecture.md`](DESIGN-harness-plugin-architecture.md) (layers, D1–D7), [`DESIGN-harness-capability-matrix.md`](DESIGN-harness-capability-matrix.md) (tier ladder), [`DESIGN-harness-drift-canaries.md`](DESIGN-harness-drift-canaries.md) (canary layers) |
| The operational drift table and the "how to check" narrative | [`docs/MAINTENANCE.md`](MAINTENANCE.md) § *Claude Code / OpenCode CLIs* |

Two of those are load-bearing rather than advisory. `docs/CHP.md` is
`include_str!`ed by `harness::chp` and checked against the code
(`the_doc_states_this_version`, `the_doc_documents_every_event`);
`docs/MAINTENANCE.md`'s drift table is `include_str!`ed by `harness::contract`
and checked against the registry (`matrix_matches_maintenance_doc`). Editing
either without its code half fails `cargo test`. **This document is not scanned
by any test** — it carries no enforcement, so keep it honest by reading the tree.

Where the milestone's spec and the tree disagree, **the tree wins**; § 3.4 lists
the places that matters.

## 2. The four layers, as modules

```
  L4  Capabilities     graph/ · tts/ · workbench/ · delegation/ · offload/
                       ── speak cImp domain types + contract::gate(id) only
                                      ▲  never imported from below the seam
  L3  Session bus      harness/{contract,health,verify}.rs
                       the capability registry and every degradation decision —
                       harness-agnostic, no harness literals
      The roster       harness/{registry,plugin}.rs  ── WHICH harness, and
                       harness/{info,ingress,native,instructions,probe}.rs
                       the neutral lookups over WHAT each one declared
  ══  L2  CHP ═════════ harness/chp.rs + docs/CHP.md ═══ THE STABLE SEAM ═════
                       versioned HTTP+JSON on loopback, bearer auth,
                       `agent` = a registered harness id, capability negotiation
                       (core's router appends each plugin's own routes last)
  L1  Harness plugin   harness/claude/* · harness/opencode/*
                       + harness/render.rs · harness/reader.rs
                       THE ONLY PER-HARNESS ARTIFACT
  L0  Harness          Claude Code (verified 2.1.233)
                       OpenCode (1.18.13; byte-identical surface at 1.18.18)
                       uncontrolled, self-updating
```

`crate::usage` used to sit at L4. It is not there any more: V40 Phase D moved
its whole data path — the status-line push, the push file, the quota and context
readings — **below** the seam, into `harness/claude/usage.rs` behind
`HarnessPlugin::usage_source()`. There is no module above the seam for a harness
module to reach for, which is why it left `layering::CAPABILITY_MODULES`.

The real tree at HEAD:

| File | Layer | What it is |
|---|---|---|
| `harness/registry.rs` | roster | **The harness registry** — `HarnessId`, `HarnessDescriptor`, `HARNESSES`, `DEFAULT_HARNESS`, `PerHarness<T>`, and the one place that answers *which harness is this?*. See § 2.2. |
| `harness/plugin.rs` | roster | **`HarnessPlugin`** — the L1 interface and the neutral types it speaks (`InputProfile`, `NativeTool`, `SettingField`, `Route`/`HookReply`, `ActivitySource`, `SessionKey`, `HarnessAffordances`, …). Every method's default is the harness-neutral answer. |
| `harness/info.rs` | roster | The registry **as the frontend receives it** — `harness_list`, plus the committed `fixtures/harness/registry.json` that vitest checks the TypeScript unions against. |
| `harness/ingress.rs` | roster | The neutral lookups over the plugins' own loopback ingress: `route`, `identity_of_request`, `drift_tokens`, `wire_default`, `hook_reply_budget`. |
| `harness/native.rs` | roster | The neutral lookup over the plugins' own tool vocabularies: `mutates_fs`, `memory_kind`, `class`, `memory_arg` — each taking the request's **source**, and each failing closed when it is unknown. |
| `harness/instructions.rs` | roster | Every string cImp puts in front of a model, in one place, rendered per harness from that harness's `tool_for_role()` names and label. |
| `harness/contract.rs` | L3 | **The capability registry** — every dependency cImp has on a harness, with tier, deps, consumers, degradation and coverage. The authority; `MAINTENANCE.md`'s drift table is checked against it. Chains each plugin's own `capabilities()` rows after the neutral ones. Also holds the `gate_for` feature-gate query and the Advisor's reverse lookups. |
| `harness/health.rs` | L3 | Read-model for Settings → *Harness health*. Joins registry rows against gate verdicts, the stored auto-verify record and the last in-process run — one panel per harness, plus a neutral `Harness::ANY` panel. |
| `harness/verify.rs` | L3 | Auto-verify, **per harness**: iterates the registry, and on one harness's CLI version change runs that harness's L1 + L2 and advances its own `last_verified`. |
| `harness/chp.rs` | L2 | **CHP** — `CHP_VERSION`, the event vocabulary (`EVENTS`), the `/session/hello` peer registry, arbitration (`served`), the quiet detector (`note_event`), stale-artifact classification (`stale_for`). |
| `harness/probe.rs` | — | The L2 probe **runner**: the report shape, the outcome model, `IMPLEMENTED` (the declared report order) and the neutral declared-unprobed row. The probe *bodies* live in each plugin. |
| `harness/canary.rs` | — | The L1 canary **runner** and corpus rules; the assertions live in each plugin's `canary.rs`. |
| `harness/capture.rs` | — | The capture-on-success corpus, so a break starts with a diff. |
| `harness/render.rs` | L1 | `{{key}}` substitution for text artifacts, and the one place a value gets JSON-quoted (`json_lit`). |
| `harness/reader.rs` | L1 | Which fallback reader a tab attaches (`OobSpec` / `spawn`), the context it runs with (`OobContext`), and the arbitration query at the tap (`OobContext::pushed`). |
| `harness/claude/plugin.rs` | L1 | Claude's `impl HarnessPlugin` — the one file the descriptor points at. |
| `harness/claude/overlay.rs` | L1 | cImp ▸ Claude: the generated `--settings` / `--mcp-config` overlays and the hello they declare. |
| `harness/claude/hook.rs` | L1 | Claude ▸ cImp: its `ROUTES_TABLE` (returned from `routes()`), the hook payload type, the contract checks, the emitted hook entry. |
| `harness/claude/{read,usage,statusline}.rs` | L1 | Claude's **fallback readers** — the transcript JSONL tail, and (since V40 Phase D) the whole status-line/quota/context data path behind `usage_source()`. |
| `harness/claude/{tools,prompts,input,settings,canary,probe}.rs` | L1 | Its native-tool table, TUI prompt grammar, input profile, declared `ext` fields, L1 canaries and L2 probe bodies. |
| `harness/opencode/harness_plugin.rs` | L1 | OpenCode's `impl HarnessPlugin`. |
| `harness/opencode/plugin.rs` | L1 | cImp ▸ OpenCode: the generated plugin's key set, its values, when it is written or swept. |
| `harness/opencode/templates/plugin.js` | L1 | **The emitted artifact itself**, as a real `.js` file with `{{cimp.*}}` slots. |
| `harness/opencode/config.rs` | L1 | cImp ▸ OpenCode: `OPENCODE_CONFIG_CONTENT`, the managed instructions file, the pinned permission block, the per-spawn server-auth env pair. |
| `harness/opencode/tools.rs` | L1 | The reviewed table of OpenCode's **own** tool ids — what the plugin's gate and beacon match on. |
| `harness/opencode/read.rs` | L1 | OpenCode's fallback reader — the `GET /event` SSE tap (Tier C). |
| `harness/_retired/` | — | Data a retired harness left behind. No descriptor, no plugin, no tab, no code path; the leading underscore is what says so, and `every_registry_entry_is_fully_wired` skips it by that convention alone. |
| `harness/layering.rs` | — | The four tests that keep all of the above true (`#[cfg(test)]`). |

`harness/mod.rs` re-exports exactly `reader::{spawn, OobContext, OobSpec}` plus
the registry's own vocabulary (`HarnessId`, `DEFAULT_HARNESS`, `PerHarness`) —
and a `HarnessId` is a value core may *hold and pass back*, never one it may
spell or branch on.

### 2.1 The layering is tests, not convention

Four tests in `harness/layering.rs`:

- **`no_harness_literals_outside_harness`** — a string a harness owns may not
  appear in production code outside `harness/`. The needle list is *derived from
  the capability registry's* `depends_on` (`JsonPath` / `ConfigKey` / `Route`,
  plus the TUI footer literal), so declaring a new dependency automatically
  widens what the scan refuses to see elsewhere. `#[cfg(test)]` blocks and
  comment lines are excluded on purpose: a fixture quoting a payload is a
  *recorded input*, and prose explaining the seam is wanted everywhere.
  `Dep::Flag` needles are deliberately excluded. Exceptions are
  `LITERAL_ALLOWLIST` — **one file today** (`graph/index.rs`, where
  `"tool_result"` is cImp's own column name colliding with a harness payload
  field, not a dependency on one) — re-checked by
  `every_literal_allowlist_entry_is_still_earning_it`.
- **`no_harness_identity_outside_registry`** (V40 Phase A, locked decision
  10(a)) — the scan the one above it never was. Its needles are every
  descriptor id, reserved tab id, binary stem and consumer token, derived from
  `registry::HARNESSES`; none of them may appear in production code outside
  `harness/`. Core may *hold* a `HarnessId` and pass it back to the registry; it
  may not spell one and it may not branch on one. Exceptions are
  `IDENTITY_ALLOWLIST` — **two files today** (`settings/schema.rs`,
  `state/manager.rs`), both persisted wire forms — re-checked in both directions
  by `every_identity_allowlist_entry_is_still_earning_it`. `settings/migration.rs`
  and any `tests.rs` are exempt **by class**: a frozen migration describes an old
  on-disk shape and must keep its literals or it stops describing the file it
  exists to read.
- **`harness_modules_do_not_import_capabilities`** — the direction is L1 → L2
  only. A file under `harness/` may not name `crate::graph`, `crate::tts`,
  `crate::workbench` or `crate::delegation`. `UPWARD_EXEMPT` holds the **six**
  that still do, each with a reason, and the test asserts in *both* directions —
  an exemption that stops being needed fails the build, so the list cannot rot
  into padding.
- **`every_registry_entry_is_fully_wired`** (V40 Phase A, locked decision 10(b);
  it absorbed V35's `every_harness_dir_declares_its_capabilities`) — the one
  test that turns a descriptor row into a promise about a dozen places. See
  § 2.2.

Two more guard the *guards*, because a source scanner that reads the wrong slice
of a file reports on nothing and says `ok` while doing it:
`the_literal_scan_reads_the_same_code_on_every_platform` (the Windows runner
checks out CRLF; a local LF tree is not the same input) and
`executable_text_ignores_line_endings_and_cuts_at_every_test_item`. Both were
added after the first two tests shipped a defect each.

And two live elsewhere: `wired_in_paths_exist`, with the capability registry it
checks, and the frontend's own identity scan in `src/lib/harnessIdentity.test.ts`.

### 2.2 The registry — `harness/registry.rs`

Before V40 the question *which harness is this?* had ten answers in ten
vocabularies, **six of which fell back to Claude for an unrecognised command**.
So a third harness was not rejected, it was *misattributed*: wrong activity
badge, wrong injection scope, wrong expose flag, wrong audit latch slot, wrong
graph source — and nothing said so. The registry is the one answer.

**`HarnessId` is an opaque newtype.** It carries the registry's own id string
and nothing else; it can only be produced by a lookup in that module, and it has
**no `Claude` / `OpenCode` constants** for core to compare against. That is what
makes "core may hold a `HarnessId` and pass it to the registry; it may not
branch on its value" checkable rather than aspirational. Adding a harness is a
descriptor row plus a `harness/<id>/` directory — deliberately **not** a new
enum variant, because a variant is a thing every `match` in the tree can be made
to care about. The lookups:

| Lookup | Answers | `None` means |
|---|---|---|
| `from_id(&str)` | the CHP `agent` discriminator | not a registered id — refused, never defaulted |
| `from_command(&str)` | compares the **file stem**, case-insensitively, so `claude`, `claude.exe` and `/usr/bin/claude` all resolve | *a first-class answer*: this tab is a shell tab, not a harness tab |
| `from_tab_id(&str)` | a reserved built-in tab id | a user-created `ai-<uuid>` tab — classified by its command instead |
| `from_consumer(&str)` | an MCP consumer token. **Padding is not trimmed**, deliberately: narrowing `" opencode "` at a caller-asserted wire value would be a widening dressed as a convenience | a token nobody declared — the proxy start fails naming the registered list |

`HarnessId::ANY` is the one non-harness value: the neutral marker for capability
rows whose contract is stated about a *tab* rather than a vendor
(`delegation.worker` is the first). It has no descriptor, no directory, no
binary and no plugin; it prints as `"any"`, and `from_id("any")` answers `None`
so the neutral token can never drive a CLI nobody named.

**`HarnessDescriptor` — one harness, as data.** All `'static`, no I/O. The rule
for what belongs here: *would this still make sense if both shipped harnesses
were deleted?* If not, it is a descriptor field or a `HarnessPlugin` method —
never a branch in core.

| Field | What it is |
|---|---|
| `id` | the CHP `agent` discriminator **and** the `harness/<id>/` directory name |
| `label` | what a human calls it |
| `binaries` | the binary file stems that identify this harness |
| `tab_ids` | reserved built-in tab ids, **in canonical order** — flattened across the registry, this replaces the `[claude, claude-local, opencode]` array that used to be written out in three places |
| `consumer` | the MCP consumer token its per-session child is launched with |
| `expects_chp` | whether cImp generates a CHP-speaking artifact for it — i.e. whether staleness detection covers its tabs. It used to be a hard-coded disjunction, which meant the mechanism was silently OFF for any harness not named in it |
| `env_strip` | environment markers of a session of *this* harness that cImp was launched from, stripped from every AI tab's child |
| `features` | what core mounts beyond the neutral path (`SessionUsage`, `ContextBar`, `FileArtifact`, `UsagePush`, `LocalProviderConfig`) — **declared, not inferred from `id == "claude"`**: a feature only one harness has today is still a feature, and the next harness that grows one gets it by saying so |
| `plugin` | the code half: `&'static dyn HarnessPlugin` |

**`HARNESSES`** is that table, and `registry::all()` / `harness_ids()` /
`canonical_tab_ids()` are the views over it that replaced the hand-kept arrays.

**`DEFAULT_HARNESS` is the one named Claude default left in the tree**, and it
does *not* mean "when in doubt, Claude". It means *a body with no identity on
this wire came from a build old enough that Claude was the only thing that could
have sent it* — a compatibility statement with an expiry date, which is why it
is one constant with that rationale attached rather than the thirteen
`unwrap_or("claude")` literals it replaced. New code does not get to use it: a
route that can carry an identity must require one, and `HarnessId::from_*`
answering `None` is the answer.

**`PerHarness<T>` is sized by the registry.** Every fixed-arity-2 structure in
the tree — the audit consumer list, the unscoped ledger's slots, the per-server
access pair, the spawn-signature pair — was a place where adding a harness meant
finding and widening an array literal, and where *forgetting to do so compiled*.
`PerHarness` is `[T; COUNT]`, so the same mistake is a type error; `get`/`get_mut`
are keyed by `HarnessId` (never by ordinal, which cannot be off by one), and
`as_map()` is the wire form, a `BTreeMap` keyed by id — because a positional
array on the wire is the exact defect this replaced.

**What holds it together, in tests:**

- **`layering::every_registry_entry_is_fully_wired`** — the directory set in
  **both** directions (a `harness/<id>/` directory the registry does not declare
  fails as loudly as a descriptor with no directory; `_`-prefixed directories are
  skipped by convention), then per descriptor: capability rows exist, a
  `chp::EV_HELLO` is declared somewhere in the directory, identity is complete,
  exactly **one** `<id>.input.profile` capability row exists **and** the plugin
  really answers an `input_profile()`, `spawn_sig` is not null, every declared
  setting is unique / labelled / accepted by its own kind and every
  `scoped_features()` row names a `Bool` field the schema declares, the sandbox
  grant table is non-empty, a *Harness health* panel row exists (plus the neutral
  `Harness::ANY` one), `MAINTENANCE.md` names every capability id, and a harness
  declaring `FileArtifact` has committed plugin goldens.
- **`layering::no_harness_identity_outside_registry`** — § 2.1. The registry is
  the only place a harness's name may be written.
- **The frontend parity pair.**
  `info::tests::the_committed_registry_fixture_matches_the_registry` renders
  `harness_list()` to
  `src-tauri/fixtures/harness/registry.json`, **writes it** when it differs and
  then fails — so drift is a red `cargo test` with the fix already in the working
  tree. vitest reads that file and asserts the TypeScript unions in
  `src/lib/harness.ts` cover it (*registry parity (locked decision 11)* in
  `src/lib/harness.test.ts`): the `HarnessFeature` union covers every declared
  feature **and** declares none Rust does not have, `HarnessInfo` and
  `HarnessAffordances` carry exactly the payload's keys, and `tabHarness` /
  `findHarnessByCommand` classify every declared binary the way Rust's
  `from_command` does. A descriptor field, a feature or a harness added in Rust
  without its TypeScript mirror is a red `npm test` rather than a runtime
  `undefined`.
- Plus the registry's own suite: `every_declared_tool_role_names_a_native_tool`,
  `at_most_one_harness_takes_the_passthrough_argv`, `ids_are_unique_and_non_empty`,
  `an_unregistered_command_is_not_a_harness`,
  `a_command_resolves_by_file_stem_case_insensitively`,
  `every_reserved_tab_id_resolves_to_exactly_one_harness`,
  `the_default_harness_is_registered`,
  `per_harness_covers_the_registry_and_nothing_else`, `any_is_not_a_harness`.

## 3. The seam-tier model

The registry ranks every dependency by the **seam** it sits in, and the tier —
not the feature — predicts how it breaks.

| Tier | Seam | Breaks how | Blast radius |
|---|---|---|---|
| **A** | MCP protocol | loudly, versioned, multi-vendor | one tool |
| **B** | documented hook / flag / settings key | at the payload boundary, usually announced | one file |
| **C** | emitted artifact (not an API) | **silently, as zeros and empties** | several modules |
| **D** | scraped UI / undocumented behavior | silently, on cosmetic upstream changes | cross-cutting |

Tier A has essentially never broken cImp, which is why **no row carries
`Seam::A`**: `graph_*`, `run_check`, `offload_task` and `context_*` ride MCP and
are not part of this surface. `Seam::A` and `Harness::Any` are declared but
unconstructed, under `#[allow(dead_code)]`, because a ladder needs its top rung
named even with nothing on it.

**The standing rule (locked decision 2):** when an upstream release makes it
possible to move a dependency *down* the ladder (D→C→B→A), that migration
outranks new harness features.

### 3.1 Push half vs read half

Locked decision 9 states the insight the whole plugin architecture rests on:

> The push path (`/context/*`, `/permission/event`, `/latch/*`) is Tier A/B and
> has never hurt; the read path (transcript tails, SSE, statusline stdin) is
> Tier C/D and is where every painful adaptation has landed. **The difference
> between them is whether the plugin layer exists.**

The push half already had one: a generated artifact, in the harness's own
extension mechanism, POSTing a body cImp defined. The read half had none — cImp
scraped fields out of artifacts the harness emitted for its own reasons. V35
Phase L moved the read path onto pushes *wherever a payload carries the data*,
and recorded, per capability, where it could not.

### 3.2 Where every capability sits today

The 24 rows of `CAPABILITIES` at HEAD. **Coverage** is the row's own
`canary`/`probe`/`waiver` columns; **TCB** marks a row where a security control
*executes*.

| Capability id | Harness | Tier | Degradation | Coverage | Notes |
|---|---|---|---|---|---|
| `claude.hook.user_prompt_submit` | Claude | B | Silent | waiver | drift token `context_hook` |
| `claude.hook.precompact` | Claude | B | Silent | waiver | behavior half is spike D0 |
| `claude.hook.pretooluse_deny` | Claude | B | FailClosed | — | the one **gated** row (`GATED`); spike E1 |
| `claude.hook.posttooluse` | Claude | B | Silent | waiver | gap closed 2026-08-17: `drift.payload.v1` lags it (token `post_edit_hook`) |
| `claude.hook.notification` | Claude | B | Fallback → `perm.tui_scrape` | — | flat *and* nested payload shapes both parsed |
| `claude.hook.taint_beacon` | Claude | B | Silent | waiver | **TCB** `taint.beacon.claude` — `type:"http"` since 2026-08-17 (shim deleted); D→B on the documented deny contract |
| `claude.hook.checkpoint_beacon` | Claude | B | Silent | waiver | **TCB** `checkpoint.pre_mutation.claude` — ditto; handler completes the checkpoint before replying |
| `perm.tui_scrape` | Claude | **D** | Silent | waiver | scraped TUI footer; user-editable patterns |
| `claude.hook.stop` | Claude | B | Fallback → `claude.transcript.assistant_text` | waiver | Phase L; quiet-detected, witness `prompt` |
| `claude.hook.tool_result` | Claude | B | Fallback → `claude.transcript.tool_result` | waiver | Phase L; witness `context.post_edit`; + `PostToolUseFailure` (2026-08-17) so failures stay counted on serving tabs |
| `claude.hook.subagent` | Claude | B | Fallback → `claude.transcript.subagents` | waiver | Phase L; **no witness**, and says why |
| `claude.transcript.assistant_text` | Claude | C | Silent | canary + probe | the arbitrated fallback behind `Stop` |
| `claude.transcript.usage` | Claude | C | Silent | canary + probe | **cannot move** — no hook carries token counts |
| `claude.transcript.tool_result` | Claude | C | Silent | canary + probe | fallback; its canary is the leading check for *both* paths |
| `claude.transcript.identity` | Claude | C | Silent | probe + waiver | **cannot move**; inverse of the version tripwire |
| `claude.transcript.subagents` | Claude | C | Silent | waiver | lifecycle migrated; **token accounting cannot** |
| `claude.statusline.stdin` | Claude | C | Silent | canary | **cannot move** — no hook carries a context window |
| `claude.flag.settings_overlay` | Claude | B | Silent | probe + waiver | the delivery mechanism for every Claude hook row |
| `claude.flag.session_id` | Claude | B | FailClosed | probe | downgraded in Phase E to what the code truly does |
| `opencode.sse.events` | OpenCode | C | Silent | canary | OpenCode's **declared** fallback, and its whole read path; honors both `session.idle` (deprecated upstream) and `session.status` idle (2026-08-17) |
| `opencode.route.push` | OpenCode | B | Silent | waiver | `noReply` losing its meaning is the dangerous half |
| `opencode.route.noauth` | OpenCode | B | VisibleOff | probe | since 2026-08-17: per-spawn Basic auth (documented env pair) enforced and used by the tap/push; id kept, meaning inverted; probe proves both directions |
| `opencode.tool_registry` | OpenCode | C | Silent | probe | allowlist-only ⇒ a new upstream tool ships **ungated** |
| `opencode.plugin.load_all` | OpenCode | **D** | Silent | waiver | **TCB** `tool.gate` + `checkpoint.pre_mutation` + `taint.beacon` |

Counts: 14 × Tier B, 8 × Tier C, 2 × Tier D, 0 × Tier A. 19 Claude rows, 5
OpenCode rows. (2026-08-17 moved three rows D → B; the two that remain Tier D
are `perm.tui_scrape` — a scrape by construction, and only a fallback — and
`opencode.plugin.load_all`, which is structural: nothing outside a harness can
verify a control inside it ran.)

### 3.3 The rows that cannot climb the ladder

Three Claude dependencies are Tier C **permanently-until-upstream-changes**, and
that is upstream's constraint rather than a missing phase. (Re-verified
2026-08-17 against the 2.1.233 docs: still no hook input carries token counts,
a context window or rate limits; OTel export remains the only alternative
surface, and it is an exporter, not a hook. § 11 keeps the wishlist.)

- **`claude.transcript.usage`** and **`claude.statusline.stdin`** — *no Claude
  Code hook input carries token counts, a context window or a `rate_limits`
  block.* The common payload set is `session_id`, `transcript_path`, `cwd`,
  `permission_mode`, `hook_event_name`; `PostCompact` exposes no compaction
  metrics. The only documented token-usage surface is the OpenTelemetry
  `claude_code.token.usage` metric, which is an exporter integration, not a
  hook. So `session.usage` and `session.context` stay **reserved** in
  `chp::EVENTS` (`live: false`, `404` on POST) rather than being deleted: an
  absence with a stated reason is a fact, a deletion is a blank.
- **`claude.transcript.identity`** — `sessionId` / `version` / `isSidechain` /
  `isMeta`. Nothing pushes them; the transcript tail is the only source.
- **`claude.transcript.subagents`** — the *lifecycle* migrated to
  `claude.hook.subagent`, but sub-agent **token accounting** did not, and neither
  did the `launch_seen` / `completion_seen` bookkeeping that
  `drift.subagent_transcripts.v1` reads. Both keep running on every tab,
  unarbitrated: suppressing the bookkeeping would make `drift_condition` report a
  false "launcher tool renamed" on every serving session.

**OpenCode migrated nothing, and declares both `cannot`s.** This is a Phase L
outcome under design D6 ("a fallback contained and declared beats a lossy
migration"), not an unfinished migration — the plugin API *can* reach both:

- *assistant text* — `experimental.text.complete` delivers one completed text
  **part**, while the SSE reader speaks one **message** with its parts joined.
  Pushing per part would change the unit the sentence segmenter is fed (locked
  decision 2). The alternative, widening the plugin's existing `event` handler,
  reads `properties.part.text` / `properties.delta` — the *same Tier-C shapes
  the reader already reads*, over a different transport, for no tier gain.
  **Revisit** when `experimental.text.complete` graduates and a
  message-completion signal exists beside it.
- *tool results* — `tool.execute.after`'s second parameter carries
  `{title, output, metadata}` and the plugin's handler takes only the first, so
  the text is one parameter away. But OpenCode usage is estimate-only from
  tool-call *input* args by design, so there is no consumer: wiring it would
  *add* a capability rather than migrate one. **Revisit** with OpenCode usage
  accounting.

Consequence: the three neutral `/session/*` routes are implemented with **no
external producer today** for OpenCode. That is the seam working as intended —
when `experimental.text.complete` graduates, or a third harness arrives, the
change is a plugin change and nothing above L2 moves.

### 3.4 Where the tree contradicts the spec

Read the tree, not the milestone table, on these four:

1. **Usage cannot be pushed.** The Phase L row of the phase table lists "usage"
   among the migrations. It cannot be; see § 3.3. The correction is a comment on
   `claude.transcript.usage` in `contract.rs`.
2. **Phase K expected Phase L to empty `UPWARD_EXEMPT`.** It did not shrink at
   all. All seven entries were relabelled in place from "the only path, pending a
   migration" to "the arbitrated fallback, plus the capabilities that have no
   other path".
3. **The Claude overlay is not a template.** Design § 5.1's
   `claude/templates/settings.json` line does not apply. The implemented rule is
   *text artifact ⇒ template with checked slots; structured artifact ⇒ build it
   structurally*, and the overlay stays `serde_json::Value`.
4. **The beacons were Claude shims** — the correction decision 10's phrasing
   needed — and since 2026-08-17 they are Claude `type:"http"` `PreToolUse`
   hooks whose handlers live in `offload/loopback.rs` (the shim binaries are
   deleted, their argv flags tombstoned). Still two harnesses, two enforcement
   sites, two rows, two control ids.

## 4. CHP in one page

`docs/CHP.md` is the contract. This is the shape of it.

**CHP is the wire both harnesses were already speaking**, given a name and a
version. Phase I documented that reality, stamped `chp` on it and added one
route (`/session/hello`); it changed no behavior. It is **not** a public
extension point (§ 10).

**Version discipline.** `harness::chp::CHP_VERSION` is `1`. It is spelled once,
substituted into the OpenCode plugin at generation and into Claude's hook
entries as the `X-CIMP-Chp` header, echoed by the hello's ack, and asserted
equal to `docs/CHP.md`'s header by `the_doc_states_this_version`. An absent
`chp` means `PRE_CHP` (0) — *old*, never *broken*. Compatibility is permanent:
every value that ever shipped must keep being understood, because a spawn-baked
artifact outlives the binary that wrote it.

**Stale-artifact detection is the consumer.** Generated artifacts are written at
tab launch; an upgraded binary with an old tab running has an old artifact
talking to new loopback code. V32 hit that four times, each time mitigated by
the same sentence — *"needs a FRESH TAB or it reads as a failure."* Every routed
POST and every hello is observed per `(agent, tab)`, and `stale_for` classifies
three states, rendered in Settings → *Harness health* under **Out-of-step tabs**:

| Kind | Meaning |
|---|---|
| `old_plugin` | This tab's artifact speaks a lower `chp` (or none) than this build writes. **Restart the tab.** |
| `new_plugin` | Higher than this build understands — an old binary beside a newer build's artifact. This binary is the stale side. |
| `harness_version` | A hello's declared harness version differs from the CLI version cImp observes. **Implemented and tested, with no producer on either harness** — neither exposes its version where a hello can honestly read it. |

Nothing is refused in any of the three. `expects_chp` answers `true` for both
harnesses since Phase J, which is why every Claude tab open across that upgrade
reported `old_plugin` — the intended reading, not a false positive.

**hello / `serves` / `cannot`.** Sent once at artifact load, which for a
generated artifact is per tab launch. `serves` lists the CHP event ids this
artifact will actually push *with this tab's flags applied*; `cannot` is
`[{id, why}]` for the rest. A capability absent from `serves` is **unavailable,
with a reason**, never *nobody wrote it down*. `harness_version` is absent on
both harnesses on purpose: neither exposes it to its extension, and baking in
the number cImp last saw would be cImp attesting to itself.

**Arbitration: per capability, per tab, push wins when served.** One predicate,
`chp::served(agent, tab, event)`, asked by both sides — the push core in
`offload/loopback.rs` refuses when the tab did not declare the capability, and
the reader's tap (`OobContext::pushed`) refuses when it did — so exactly one
path produces each datum. Three properties, each closing a specific failure:
per capability (a tab pushing prose still has its transcript tailed for usage);
per tab (two Claude tabs can run two different spawn-baked overlays); and it
requires a **hello**, not a setting — the declaration arrives over the push
path, so a hello is itself proof the path works, and a pre-upgrade tab behaves
exactly as it did before. A source scan
(`each_migrated_capability_is_arbitrated_on_both_sides`) asserts both sides
consult the predicate, because a reader guarded by its own boolean would restore
double-delivery with no test noticing.

**The witness rule for quiet capabilities.** A served capability that goes quiet
is **reported, not silently un-served** (locked decision 7): falling back would
restore the data and hide the breakage. So the reader stays suppressed and the
silence raises a drift report under that capability's own token. "Demonstrably
active" is defined by a **witness** — another push whose arrival proves this one
should also have fired: `prompt` witnesses `assistant_text`,
`context.post_edit` witnesses `session.tool_result`. `QUIET_WITNESS_PUSHES = 3`
(one turn can legitimately end without its `Stop`), latched once per
`(tab, capability)`, counters cleared by a new hello.
**`session.subagent` has no witness and asserts why**: a session may
legitimately launch no sub-agents forever, so any threshold there would
manufacture false reports.

## 5. The capability registry

`harness/contract.rs`. One source of truth for everything cImp depends on from a
harness it does not control.

### 5.1 Row anatomy

| Column | What it is |
|---|---|
| `id` | **The universal join key** — the Advisor, the canary suite, the probe, the health panel and the `MAINTENANCE.md` drift table all join on it. **Never renamed** once it lands. |
| `harness` | `Claude` / `OpenCode` / `Any` (unconstructed). |
| `tier` | `Seam::{A,B,C,D}` — one tier per row. A row's D-component is expressed as a `Dep::Behavior` entry, not as a second tier. |
| `contract` | The human sentence: what upstream must keep doing. |
| `depends_on` | Exactly what cImp reads or calls: `JsonPath`, `FilePath`, `Flag`, `ConfigKey`, `Route`, `Behavior`. A `Behavior` entry is the marker of an **unverifiable** contract — it needs a spike, not a canary. This list is also what widens the layering scan. |
| `wired_in` | Repo-relative paths of the modules that break if this drifts. |
| `degradation` | `Silent` (the dangerous one), `VisibleOff { user_message }`, `FailClosed`, `Fallback { to }`. |
| `drift_rule` | The V16 statistical rules that **lag** this row, always via the `advisor::RULE_DRIFT_*` consts — never duplicated literals. |
| `canary` | The L1 fixture canary. **A canary id IS the capability id** — never a third namespace. |
| `probe` | The L2 live probe, same join rule. Set **only** where `harness::probe` actually drives the row; a permanent-`Unknown` emitter is declared by its own plugin (`HarnessPlugin::declared_unprobed()`, joined with the neutral rows by `probe::declared_unprobed()`) instead, because counting one as coverage is the "quality signal with no consumer" this registry exists to prevent. |
| `waiver` | An accepted residual: why there is no canary *yet*, what covers it meanwhile, and who owns closing it. |
| `controls` | **The TCB column.** A control id means the security control *executes inside* this capability, not that the row merely carries data for one. Documentation, not a gate — but a reviewer changing such a row is changing the trusted computing base. |
| `drift_token` | The `shim` token this row's drift reports arrive under at `POST /activity/contract_drift`. **Explicit since Phase J**: attribution used to be inferred from `wired_in` file names, which stopped discriminating when four reporters collapsed into one module. |

### 5.2 The rule of the house

**A new dependency never enters unrecorded, and the build enforces it.**

`every_silent_degradation_has_a_canary_or_a_probe_or_a_waiver` — a `Silent` row
with no canary, no probe and no non-empty waiver fails `cargo test`. L1 and L2
are deliberately *alternatives* here, not a conjunction: they answer different
questions, and several rows are structurally reachable by only one (a CLI flag
has no fixture; four transcript fields have no single reader to drive). The test
tightens on its own as coverage lands.

The other consistency tests, all in `contract.rs`:

| Test | What it stops |
|---|---|
| `matrix_matches_maintenance_doc` | Registry ↔ `MAINTENANCE.md` drift table id-set equality, both directions, uniqueness on both sides. Prose cannot drift from code. |
| `wired_in_paths_exist` | A declared consumer path that no longer resolves — i.e. a refactor that moved the code and left the row pointing at nothing. |
| `probes_and_the_matrix_agree` | Every row is in exactly one of `probe::IMPLEMENTED` / `probe::declared_unprobed()`; a row cannot fall out of both and stop being counted. |
| `tcb_controls_are_declared_exactly_once` | Each control id names exactly one *place* enforcement executes. |
| `every_gated_capability_can_actually_block` / `no_gate_blocks_outside_the_declared_list` | `GATED` and the `gate` match arms agree — an entry no arm can block is a gate that does not exist; an arm not listed is a gate no UI can see. |
| `a_blocked_gate_always_says_why` | A block with a blank reason (global principle 5). |
| `the_e1_gate_fails_closed_on_anything_unrecognized` | A hand-typed `"Fail"` sailing past `spike_status_blocks`. |
| `an_unknown_capability_id_is_not_a_gate` | Fail-open direction for an unknown id, safe only because call sites pass `CAP_*` consts. |
| `the_gated_capability_ids_reach_the_frontend` | The `GATED` ids are mirrored in `src/lib/settings/types.ts`. |
| `every_declared_drift_rule_resolves_back_to_its_rows` | A `drift_rule` link with no row on the other end. |
| `every_payload_shim_resolves_to_one_row` | `drift_token` uniqueness and totality over `drift.payload.v1` rows. |
| `canaries_and_the_matrix_agree` (in `canary.rs`) | A canary outside the registry, or a `canary: Some(..)` row with no canary. Extracts `row("<id>")` call sites from `include_str!("canary.rs")`, with a floor against vacuous extraction. |
| `embedded_canaries_are_exactly_the_declared_ones` (in `canary.rs`) | A declared canary the auto-verify silently never runs, or an embedded check nobody declared. |

## 6. The verification machinery

Four independent layers, none of which subsumes another.

**L1 — substantiveness canaries (`harness/canary.rs`).** Every reader pinned
here is deliberately lenient: `parse_usage_line` ends each lookup in
`unwrap_or(0)`, the statusline parser documents that "a parse failure yields
`Input::default()`", the SSE tracker ignores event types it does not know. That
leniency is correct — a reader must never break a user's turn — but it means an
upstream rename produces **zeros and empty strings, not errors**. So a canary
asserts *substantiveness*: fed the recorded shape, does the reader still produce
a non-zero, non-empty result? (Locked decision 3, the load-bearing decision of
the milestone; global principle 5, *empty is not absent*.) Fixture selection is
part of the contract — a fixture containing a real zero cannot tell "absent"
from "zero". Five capabilities are covered
(`EMBEDDED`: the three transcript readers, the statusline, the OpenCode SSE
tracker), `include_str!`-embedded so they run from a release binary, dispatched
by `run_embedded` — the `#[test]`s are thin wrappers over the *production*
functions, so `cargo test` and the shipped auto-verify can never check different
things. Each has a **negative twin** (`negative_canary_*`) driving the same
function over a drift fixture under `fixtures/harness/<harness>/_synthetic/`,
byte-identical to its positive except one renamed field, asserting the reader
answers with its degraded default. A positive canary that never actually ran
passes just as green as one that did.

**L2 — the live probe (`harness/probe.rs`, `cimp --harness-canary [--json]`).**
L1 asks "do we still parse the shape we recorded"; L2 asks "is the recorded
shape still real", by driving the *installed* CLIs. It needs no app instance, no
loopback and no settings file. Its `Outcome` has four values and **only one is a
failure**: `Pass`, `Fail` (the only non-zero exit), `Unknown` (could not be
driven — CLI absent, no session to tail, no tool call in the window; reported
with a reason, never counted as broken) and `Transition` (upstream changed *for
the better*; OpenCode growing auth WAS the worked
example until the 2026-08-17 batch completed that transition — the variant is
now declared-unconstructed, kept for the next one). Modelling the last
two as failures would recreate exactly the alarm fatigue the milestone exists to
remove — `drift.harness_version.v1` fired on every CLI auto-update until the
rational response became clicking *Mark verified* without running anything.
Nine rows are driven (`probe::IMPLEMENTED`, which is also the **declared report
order** — so a report cannot silently become "whatever the probe functions
happened to answer"); nineteen are enumerated as `unknown` with the reason, one
neutral and eighteen contributed by the two plugins' own `declared_unprobed()`
— printed, not omitted, because a dependency that stops being listed is one that
stopped being counted. Note the split: the runner drives the loop, holds the
report shape and decides the ORDER; the probe **bodies** are the plugins'
(`HarnessPlugin::probe()`), and a plugin whose probes share one expensive child
says so (`probes_share_one_child()`) rather than the runner hard-coding an
order. The five `claude.transcript.*` probes tail a **real** session JSONL, so
every detail string carries counts and field names only.

**Auto-verify on version change, per harness (`harness/verify.rs`).** Everything
here is keyed by `HarnessId`: `pending_harness()` iterates the registry
comparing each harness's own `last_seen` against its own `last_verified` (two
string comparisons per harness against the mtime-cached settings, V40 Phase B),
`on_version_changed(harness)` wakes one background thread for **that** harness,
and `last_run(harness)` keeps at most one in-memory record per harness. L1 runs
in-process in milliseconds, then L2 drives that harness's installed CLI.
**Advance iff nothing FAILED** — `Unknown` and `Transition` never block (locked
decision 8), with the honest consequence recorded rather than hidden: on a
machine where the CLI cannot be probed, a version advances on L1 evidence alone,
which is strictly more than the reflexive click it replaces. Zero failures ⇒
that harness's `Settings.harness[<id>].last_verified` advances by itself and no
Advisor card appears at all; otherwise its version stays put and each failing
capability is recorded so the Advisor can name it, its evidence
(`harness.canary.l1` / `harness.probe.l2`) and its `wired_in` modules. The
Advisor's own `version_signature` is per harness too, so one harness's update
cannot dismiss another's card. The whole run is capped at `OVERALL_CAP` (90 s),
enforced *between* layers so no probe is killed mid-flight leaving a child
behind. `e1_status` / `d0_status` are deliberately untouched — **Mark verified**
survives for exactly the Tier-D `Behavior` spikes no probe can settle, which is
what makes the button mean something again.

**The health panel (`harness/health.rs`).** One computed answer to "what is
broken right now", built entirely in Rust so Settings renders it rather than
re-deriving it: tier, contract sentence, degradation, coverage marks, the TCB
column, the Phase E gate verdict, the stored auto-verify record, the last
in-process run, and the `stale_plugins` list per harness header. The one honest
gap is stated in the view rather than papered over: a stored record names only
*failures*, so for an unnamed row the disk says exactly "this run did not report
a failure for it" — which is strictly weaker than "it passed", and gets its own
token (`OUTCOME_NO_FAILURE`) rather than being promoted.

**Capture-on-success (`harness/capture.rs`, `cimp --harness-capture [--json]`).**
*A breakage's first diagnostic should be a diff between the last known-good
capture and today's, not an investigation.* A probe run with **zero `Fail`**
files the payloads it read — scrubbed through `processing::sanitize::scrub_payload`
(fail-closed: if the credential screen cannot run, nothing is written), stamped
with the CLI version they were seen on — under
`<app-data>/harness-captures/<harness>/<cli-version>/`. Known-good is the whole
semantic: a failing run overwriting it would destroy the artifact the diff needs,
so `--harness-capture` writes a run that *did* drift into a sibling
`<version>-failing/` lane instead. Bounded on purpose: `KEEP_VERSIONS = 8`,
`KEEP_FAILING = 3` (swept independently, so a burst of failing captures cannot
evict the evidence), `LINES_PER_CAPABILITY = 3`. It is the first
OS-app-data path in an otherwise portable tree, deliberately: this is the one
store whose contents derive from a real session, and it must not ride a synced
exe directory. Stated residual — scrubbing removes credentials, not context,
which is why promotion to a committed fixture stays a manual reviewed step.

## 7. Anatomy of the two shipped plugins

### 7.1 OpenCode — one generated ES module

The design calls this "the better design, and the one to standardize on": a
single generated artifact speaking the harness's own extension mechanism on one
side and CHP on the other.

`write_opencode_plugin` renders `templates/plugin.js` into
`<project>/.opencode/plugin/cimp-inject-<tab>.js` at every OpenCode tab spawn —
**one file per tab** (V32 #48: a single per-directory file meant last-spawn-wins
silently replaced a sibling tab's security posture), with a sweep of files whose
tab id is no longer configured and of the pre-#48 `cimp-inject.js`. It is
written iff `opencode_plugin_wanted`, which is the **OR of every consumer's
need** (`graph.enabled`, sensor-mode native web, the native gate,
`workbench.checkpoints`) — because a gate that disappears when an unrelated
feature is toggled is worse than no gate. Five per-tab switches ride in as
`OpencodePluginFlags` (a struct, not positional `bool`s: transposing `beacon`
and `native_gate` would turn a report-only sensor into a denial with no compiler
complaint).

**Generation is a template plus a checked key set.** `plugin.rs` holds
`OPENCODE_PLUGIN_KEYS`, 18 slots in emission order:

```
cimp.loopback_url   cimp.token           cimp.chp_version    cimp.tab_id
cimp.flag.inject    cimp.flag.auto_check cimp.flag.beacon
cimp.flag.native_gate                    cimp.flag.checkpoint
cimp.tools.local    cimp.tools.web       cimp.tools.mutating
cimp.refusal.local  cimp.refusal.web     cimp.refusal.web_tainted
cimp.refusal.web_user_local
cimp.hello.serves   cimp.hello.cannot
```

`opencode_plugin_values` fills them from reviewed Rust — the tool sets from
`opencode::tools`, the refusals from `offload::toolclass`, `chp::CHP_VERSION`,
and the hello halves built from the same flags the handlers are baked with.

**The escaping discipline.** Every value is a whole JS/JSON literal — quotes,
escaping and all — produced by `render::json_lit`, and every slot sits in
**expression position**, never inside a string literal
(`const CIMP_TOKEN = {{cimp.token}};`). That is the rule, not a style: a tool
name someone adds to the table next year, a refusal sentence full of apostrophes
and em dashes, a tab id that grows a quote — none can close a string or malform
the emitted file, because none is ever hand-quoted into one. Several slots
render JSON *arrays* and could not sit inside quotes anyway. The cost is that
the template is not strictly parseable JavaScript; it still highlights, greps
and diffs as JavaScript, which is what the move was for. Substitution is **one
left-to-right pass**, so a value containing `{{` cannot inject a second
placeholder, and an **unknown key is echoed verbatim and logged at `error!`,
never blanked** — a `{{typo}}` left in place is a parse error the harness
reports on load, while an empty string there is a gate constant that reads
`undefined`.

**Three key-set tests plus goldens.** Every `{{key}}` in the template is
declared (`every_placeholder_in_the_template_is_a_known_key`); every declared key
is used (`every_known_key_is_used_by_the_template`); the generator supplies
exactly that set, in order
(`the_generator_supplies_exactly_the_documented_key_set_in_order`). A rendered
plugin carries no residual `{{`. And three **byte-identical goldens** live under
`src-tauri/fixtures/harness/opencode/goldens/` — `plugin.all-on.js`,
`plugin.all-off.js`, `plugin.mid.js` — asserted by
`the_template_renders_the_pre_phase_m_goldens_byte_for_byte`. They were captured
from the pre-Phase-M `format!()` generator *before the template existed*, which
is what makes moving a TCB file out of Rust a provable pure refactor; from here
on they are the standing **readable diff**. Re-bless deliberately:

```
CIMP_BLESS_PLUGIN_GOLDENS=1 cargo test --bin cimp byte_for_byte
```

…then read `git diff` on those files. Goldens sit beside `fixtures/harness/`
rather than inside it on purpose: that tree is walked by
`every_fixture_version_dir_has_a_manifest`, whose contract is
`<harness>/<CLI version>/` recorded *upstream* payloads. These are cImp's own
output, versioned by nothing but this repo. `.gitattributes` pins `eol=lf`.

**The hello fires at module load** — once, while the module is evaluated, which
for a generated plugin is per tab launch. It is written for one property above
all others: **it must not throw.** A module that throws while loading takes the
harness's whole plugin load down with it, so the dispatch sits in a `try/catch`,
the promise is only `.catch`ed after being checked for a `.catch`, the reply is
never read, and the request carries a 2 s `AbortSignal.timeout`. A dead app, a
rotated token or a runtime with no `fetch` all end as "unannounced".
`harness_version` is deliberately absent.

**The TCB controls live inside it.** Three security controls execute in this
file's `tool.execute.before`: the V32 Phase H native-tool gate (the `throw` — the
*only* escaping error in a generated artifact, and deliberately outside the
handler's `try/catch`), the V33 Phase F pre-mutation checkpoint trigger, and the
V32 taint beacon. cImp only *computes* the verdict at `/latch/state`; the plugin
is the only thing sitting in OpenCode's own tool path. That is what makes the
template a security surface rather than a data pipe — and why the file states its
own honest limits to whoever reads it on disk: it runs inside the agent's
process, so `OPENCODE_PURE=1` or a second ungated `opencode` walks around it; a
user-typed `!shell` never reaches a plugin hook; `bash` is egress-capable by
nature. It is a policy control, not containment. OS-level containment is V33's.

### 7.2 Claude Code — no persistent plugin

Claude's L1 is not a file that keeps running. It is a spawn-baked `--settings`
overlay plus the harness's own HTTP hooks pointing at cImp.

**The overlay.** `harness/claude/overlay.rs` composes it as
`serde_json::Value` — structured, not templated, and deliberately not converted
in Phase M (structured construction is strictly safer than a text template for
JSON). It carries `hooks`, `statusLine` and, in native-web `deny` mode,
`permissions`, without cImp ever writing to `~/.claude`.

**`type:"http"` hooks (Claude Code ≥ 2.1.63).** The harness POSTs its own
hook-input JSON at `/claude/hook/<event>` and parses the 2xx JSON reply exactly
as it parses a command hook's stdout. Phase J deleted the five shim binaries
(1053 lines) that used to courier that payload, and the 2026-08-17 batch
deleted the last two (the beacons); `harness/claude/hook.rs` is the receiving
end. Twelve routes are declared in `ROUTES`, and `chp_event` maps ten of them
onto CHP events — `SessionStart` maps to none (it *is* the negotiation), and
`PostToolUseFailure` maps to none deliberately: it is the error half of
`session.tool_result`, and a second event would let a rare failure push re-arm
the quiet detector watching the common success entry. The join stays asserted
total, with both absences encoded in the test.

**Identity rides headers, because a hook's body is the harness's.** Every
emitted entry carries `X-CIMP-Tab` (caller-asserted, validated against the
user's configured Claude tabs before anything is recorded), `X-CIMP-Agent`,
`X-CIMP-Chp` (from `CHP_VERSION` at generation) and
`Authorization: Bearer $CIMP_HOOK_TOKEN`. **The token rides the child env**: the
variable must be named in the entry's `allowedEnvVars` or it substitutes to the
empty string, and `tabs::config::compose_ai_env` sets it on the Claude child at
spawn rather than baking a literal into the `--settings` argv value — argv is
the most casually readable thing on the machine. **Timeouts are pinned at
generation**: `TIMEOUT_SECS = 1`, the deleted shims' shared 600 ms budget rounded
up, held by a test — the harness defaults are 600 s (30 s for
`UserPromptSubmit`), either of which would turn a wedged handler into a wedged
turn.

**`SessionStart` is the hello.** The route synthesizes the § 4.1 record with
`agent: "claude"`, `tab` and `chp` from headers, and `serves`/`cannot` from the
`X-CIMP-Hello` header the generator baked in. Baked, not recomputed at hello
time, and that is the point: `SessionStart` also fires on `resume` / `clear`,
potentially long after the spawn, so a declaration recomputed then would describe
settings the running overlay never saw — exactly the drift `chp` exists to make
legible. Claude's `serves` is unconditionally `hello` + `contract.drift`, plus
whichever of `prompt`, `context.compaction`, `context.should_read`,
`context.post_edit`, `permission.event`, `taint.beacon`,
`checkpoint.pre_mutation`, `assistant_text`, `session.tool_result` and
`session.subagent` this tab's flags wired. Every `cannot` reason **names the
fallback that keeps serving the capability**, because "still Tier C on the
transcript tail" is a different fact from "gone". `session.usage` and
`session.context` appear on **neither** side: there is no per-tab decision to
report, because there is no producer at all.

**Fail-open, in HTTP terms.** The shims' contract was "print nothing, exit 0".
The equivalent is the harness's own: a timeout, a refused connection and any
non-2xx are non-blocking; a 2xx with no directive is a no-op. Blocking is
expressible *only* as 2xx plus a decision field, which is what makes the read
advisor structurally unable to refuse a read by failing.
**`terminalSequence` is never emitted** — it writes escape sequences into the
PTY cImp renders, it is not a CHP capability, and two tests assert no overlay and
no handler produces one.

**The beacons, since 2026-08-17.** `cimp --taint-beacon` and
`cimp --checkpoint-beacon` are gone — the two `PreToolUse` entries are
`type:"http"` like everything else, and their handlers reach the same cores
`/latch/beacon` and `/workbench/tool_checkpoint` serve (a source-scan test
holds both transports of each capability to one core). Report-only is now
structural: the handlers answer 2xx with no decision field on every path. The
checkpoint handler finishes its work before it replies — a `PreToolUse` http
hook blocks the tool call until the response, the documented mechanism that
makes `deny` expressible — so the ordering the feature rests on is upstream's
written contract rather than an observed behavior, and both rows moved D → B
on it (a test pins the entry's 5 s ceiling above the app's own 1800 ms budget).
The deleted argv flags stay tombstoned — an unrecognized flag would fall
through to the GUI launch path — so a pre-upgrade tab's overlay fails open and
reports `old_plugin` instead of misbehaving.

**The fallback reader still owns real work.** `harness/claude/read.rs` is
arbitrated *off* per tab for `assistant_text`, `session.tool_result` and
`session.subagent` lifecycle — and keeps serving, on every tab: `UsageEvent::Turn`
token accounting, session identity, session→commit provenance, and sub-agent
token accounting. `harness/claude/statusline.rs` remains the usage widget's only
data source. Those are not leftovers awaiting a phase; they are capabilities
with no other path (§ 3.3).

**The mid-session switchover.** Because `SessionStart` fires on `resume` and
`clear`, a hello can land after the reader has already spoken part of a turn
that is about to arrive as one complete `Stop` payload. Speaking the push whole
would *replay*; dropping it would *lose* the remainder. `tts::prose` closes the
gap at the message boundary: the reader records the speakable prose it last
emitted for a tab, and the first push after the switchover strips it as a prefix.
One `String` per tab, consumed on read. Both producers go through
`tts::prose::speak_prose`, so there is one composition — strip escapes, reduce
markdown, segment, re-check the live toggle per sentence — and not two copies of
it. **Segmentation stays app-side**: a plugin sends prose, never markup, control
sequences or sentence boundaries.

## 8. Developer guide A — adding a new harness

A new harness is **one directory and no changes above L2**. The tests are the
checklist: work through the steps, and at each missed one the build tells you
what is missing.

### Step 1 — the directory, the descriptor and the impl

`harness/<id>/mod.rs` with an `impl HarnessPlugin`, plus `pub mod <id>;` in
`harness/mod.rs`, plus **one `HarnessDescriptor` row** in `harness/registry.rs`
(§ 2.2). `layering.rs`'s old hand-kept `HARNESS_DIRS` array is gone — it is a
view over the registry now, so a new harness is declared once, and the test can
check the half it was *not* told about.

The full per-method checklist, with the test that fails at each missed step, is
[`docs/ARCHITECTURE.md` § *Adding a harness plugin*](ARCHITECTURE.md); the steps
below are the narrative for the ones that need one.

> **Fails until you do:** `every_registry_entry_is_fully_wired` — in both
> directions: "a `harness/<id>/` directory exists that the registry does not
> declare (or vice versa)".

### Step 2 — emit the artifact

Whatever this harness's own extension mechanism is. The three that exist are the
template: `claude/overlay.rs` (a `--settings` JSON overlay + `--mcp-config`),
`opencode/plugin.rs` (a dependency-free ES module), `opencode/config.rs` (one
env var).

**The rule, and it is not negotiable by preference:**

- **Text artifact ⇒ a real file with `{{cimp.*}}` slots** under
  `<id>/templates/`, `include_str!`ed and rendered by `harness/render.rs`. The
  Rust keeps the key set and the values; the JavaScript (or shell, or whatever)
  keeps itself. Declare the key set as a `const` array in emission order, supply
  it from one function, and add the three key-set tests plus goldens.
- **Structured artifact (JSON/TOML) ⇒ build it structurally**, with
  `serde_json::Value`, as `claude/overlay.rs` does. Converting one of those to a
  text template would be a regression, not a cleanup.

Everything at this layer is **spawn-baked**: computed in Rust at tab spawn, and
it outlives the binary that wrote it. So every body it posts carries
`chp::CHP_VERSION`, and any Settings-derived value baked into it needs a
`tabs::config::spawn_inject_sig` entry so the user gets the restart hint.

> **Fails until you do:** `every_placeholder_in_the_template_is_a_known_key`,
> `every_known_key_is_used_by_the_template`,
> `the_generator_supplies_exactly_the_documented_key_set_in_order`, and the
> golden byte-identity test.

### Step 3 — declare a hello

`serves` / `cannot`, built from the **same booleans** that decided what was
emitted, so the declaration cannot claim something the artifact does not do.
Event ids come from `chp::EV_*`. Every `cannot` carries a `why`, and a good `why`
names what serves the capability instead. A capability absent from `serves` must
read as *unavailable, with a reason* — never as *nobody wrote it down*.

> **Fails until you do:** `every_registry_entry_is_fully_wired` looks for
> `chp::EV_HELLO` in a `.rs` file in your directory.

### Step 4 — capability rows

Add rows to `contract.rs` for anything this harness serves that no row covers,
with `wired_in` naming your files — and add the ids to `MAINTENANCE.md`'s drift
table in the **same commit** (a doc row may carry several ids; each id must
appear in exactly one row).

> **Fails until you do:** `every_registry_entry_is_fully_wired` (no rows for
> your harness), `matrix_matches_maintenance_doc` (registry ↔ doc disagreement,
> both directions), `wired_in_paths_exist` (a path that does not resolve),
> `probes_and_the_matrix_agree` (a row in neither probe list).
>
> Rows whose contract is a sentence about **this product** — "this TUI accepts a
> bracketed paste as one literal insertion" — go in your plugin's
> `capabilities()` rather than in `contract.rs`'s neutral table;
> `contract::capabilities()` chains them after the neutral ones, so every
> consumer (the gate, the probe, the health panel, the drift advisor, the
> literal scan) still sees one registry and none of them holds a per-harness
> list.
>
> And once your row exists, `no_harness_literals_outside_harness` gets *stricter*
> — your `depends_on` strings become needles. If it fires, the code reading them
> is in the wrong directory.

### Step 5 — coverage

Either a canary, a probe, or a recorded waiver. Any `Silent` row needs one of
the three; a canary id and a probe id **are** the capability id. If you write a
canary, write its negative twin: a positive canary that never ran passes just as
green as one that did. Canaries and probe bodies are the **plugin's**
(`canaries()`, `probe()`); `harness/{canary,probe}.rs` keep the runners, the
corpus rules and the report shape.

If you can only write a waiver, say what covers the row meanwhile and who owns
closing it — and declare the row unprobed **from your own plugin**
(`HarnessPlugin::declared_unprobed()`, an `(id, reason)` pair) rather than
leaving it uncounted. That list is deliberately separate from what `probe()`
answers: *"no probe can settle this"* is a claim that needs writing down, and a
probe that silently stopped emitting a row must not be mistaken for one.
`probe::DECLARED_UNPROBED_NEUTRAL` keeps only the rows no plugin owns — the ones
stated about a *tab* — and `probe::declared_unprobed()` is the joined view.
`probe::IMPLEMENTED` likewise stays in core: it is the **declared report
order**, so a report cannot silently become "whatever the probe functions
happened to answer".

> **Fails until you do:**
> `every_silent_degradation_has_a_canary_or_a_probe_or_a_waiver`,
> `canaries_and_the_matrix_agree`,
> `embedded_canaries_are_exactly_the_declared_ones`,
> `every_fixture_version_dir_has_a_manifest` (an anonymous fixture is
> indistinguishable from a guess).

### Step 6 — a fallback reader, only if the harness cannot push

`<id>/read.rs`. Tier C stays possible; since V35 it is *contained and declared*
rather than ambient. Two disciplines come with it:

- Guard every migrated tap on `OobContext::pushed(agent, event)` so exactly one
  of the reader and the push core produces each datum.
- It will need L4 types, so add it to `layering.rs`'s `UPWARD_EXEMPT` **with the
  reason and the condition that retires it**. The exemption list asserts in both
  directions: an entry that stops importing upward fails the build. Do not write
  "retires in phase N" unless it does — Phase K did, and Phase L had to correct
  all seven entries in place.

> **Fails until you do:** `harness_modules_do_not_import_capabilities`, and
> `each_migrated_capability_is_arbitrated_on_both_sides` if you migrate a
> capability that has a reader.

### What a new harness costs above L2

Read this table as *what is enforced*, not as reassurance. Two of the rows it
used to carry were false, and V40 replaced each with the test that now holds it.

| You do **not** touch | Because — and what fails if you try |
|---|---|
| A new enum variant outside `harness/` | `HarnessId` is an opaque newtype with **no per-product constants**, and `OobSpec` plus the spawn seam live in `harness/reader.rs`. `no_harness_identity_outside_registry` fails the build if core spells a harness name at all. |
| A new `match` arm in `tabs/config.rs` | `tabs::config` owns *when* a tab spawns; `harness/<id>/` owns *how the harness is told*. That file left `IDENTITY_ALLOWLIST` in V40 Phase C, so it is now **inside** the identity scan: a new per-harness arm there is a red build, not a discouraged habit. |
| A bespoke gate constant | `contract::gate_for(id, settings, harness)` is the one query. `every_gated_capability_can_actually_block` and `no_gate_blocks_outside_the_declared_list` hold it in both directions. |
| A new Settings field for interpretation | Your own fields are `settings_schema()` rows under `Settings.harness[<id>].ext`, rendered by a generic form. The `harness` block is **machine scope** — it describes this installation's harnesses, not this project's preferences — and `every_registry_entry_is_fully_wired` rejects a duplicate key, a nameless control or a default the declared kind would not accept. |
| `chp.rs`, `contract.rs`'s types, `graph/`, `tts/`, `workbench/`, `delegation/` | they type against CHP and against `HarnessId`, not against harness-shaped Rust. `harness_modules_do_not_import_capabilities` holds the direction. |

**One thing you *do* touch, and the old version of this table denied it: the
frontend mirror.** There is one, and it is pinned rather than hand-kept.
`harness_list` serves the roster over IPC (identity + features + affordances +
declared `ext` fields + scoped features);
`info::tests::the_committed_registry_fixture_matches_the_registry` writes
`src-tauri/fixtures/harness/registry.json` from the registry and fails when the
committed copy differs; vitest reads that file and asserts the TypeScript unions
in `src/lib/harness.ts` cover it. So a new harness costs **no per-harness
frontend code** — no `{:else if}` body, no CSS class, no label map — but it does
cost a re-blessed fixture, and if you add a *field* or a *feature* it costs the
TypeScript union too. Skipping that is a red `npm test`, not a runtime
`undefined`.

If a step forces anything else outside `harness/<id>/`, **the seam is in the
wrong place — say so rather than adding it.**

### The hard rules

- **No third-party plugin loading (design D7, locked decision 10).** There is no
  drop-in directory, no package manifest, no signing. A new harness is a PR
  adding `harness/<id>/`, released as part of cImp. **Why:** the plugin is inside
  the TCB. cImp only *computes* the V32 Phase H verdict; enforcement is a `throw`
  inside the plugin's own tool path, and an artifact that omits it **silently
  disables native-tool containment while appearing completely functional**. No
  cImp-side test can catch that — the control does not run in cImp's process, and
  nothing outside a harness can verify that a control inside it ran.
- **`terminalSequence` is never emitted**, by an overlay or by a handler. It
  writes escape sequences into the PTY cImp renders.
- **Timeouts are pinned at generation**, not left to the harness's defaults.
- **Tokens never appear in argv or in overlay text.** Use the harness's own env
  substitution (`allowedEnvVars` + `$VAR`), or bake into a file the harness
  loads — never into a command line.
- **Every interpolated value goes through the escaping helper**
  (`render::json_lit`). Never hand-quote a substitution value.
- **A plugin must not throw at load.** Whatever a hello or a beacon does, an
  exception during module evaluation takes the harness's whole extension load
  down with it.
- **Fail open.** Every CHP client treats a refused connect, a 401, a timeout, a
  non-2xx or a malformed reply as *unreported*, never as *refused*. The single
  exception is a security control's deliberate refusal.

## 9. Developer guide B — changing an existing plugin

The common case, and the one with the sharpest failure modes.

### Editing the OpenCode template

1. Edit `harness/opencode/templates/plugin.js`.
2. `cargo test --bin cimp` (the plugin tests, the key-set tests and the goldens
   are all in `harness::opencode::plugin`).
3. **Read the golden diff.** The byte-identity test will fail; that is the
   design. The three goldens are the review artifact.
4. Re-bless deliberately:
   `CIMP_BLESS_PLUGIN_GOLDENS=1 cargo test --bin cimp byte_for_byte`, then
   `git diff` the goldens and read them.

**Re-blessing a golden without reading its diff defeats the entire
arrangement.** An unexplained golden diff in a review is a change to a security
control.

### Adding a key to the substitution set, end to end

1. Add the slot to the template, in **expression position**.
2. Add the key to `OPENCODE_PLUGIN_KEYS`, in emission order, with a comment
   saying what supplies it.
3. Add the value to `opencode_plugin_values`, in the same position, through
   `json_lit`.
4. Run the tests: all three key-set tests plus the residual-`{{` check plus the
   goldens must be considered, and the goldens re-blessed.

Skip step 2 and the template test names your typo. Skip step 1 and the
unused-key test tells you a value is computed and thrown away (global principle
3). Skip step 3 and the order test catches it — which matters, because at runtime
an unsupplied key is echoed as a literal `{{…}}` into a file the harness loads.

### When `CHP_VERSION` bumps

**Bump for a semantic change to something already on the wire, or for a
vocabulary addition L3 can gate against. Never for an additive route alone.**
New meaning ⇒ new route (compatibility rule 4); a new route with a new body is
additive and leaves `chp` alone — which is why V35 phases J and L both shipped
at `chp = 1`. **V40 Phase C took it to 2** for the seven neutral events it added
to `chp::EVENTS` (locked decision 30): every existing event, route and body kept
its shape, but the *vocabulary* is what a peer's hello is read against, so an
artifact that had never heard of those ids is genuinely a different peer.
Bumping means, in one commit: the constant in `harness/chp.rs`, the header of
`docs/CHP.md`, a row for each new event in § 5 (its id **and** its route, both
in backticks), and a row in § 6.1's table if the bump changes what a mismatch
*means*. `the_doc_states_this_version` and `the_doc_documents_every_event`
enforce all of that except the last.

What a bump does to live tabs: every tab whose artifact was written by the
previous build starts reporting **`old_plugin`** in *Harness health* until it is
restarted. Nothing is refused. That report is the feature — it is the V32 "needs
a FRESH TAB" trap named at the moment it applies — so do not soften it, and do
not add a compatibility shim that makes an old artifact look current. And note
the reverse: an *older* binary beside a newer build's artifact reports
`new_plugin`, which is the same trap from the stale-binary side.

### The spawn-baked reality

Every artifact at this layer is written at tab launch. So:

- **A change to an emitted artifact needs a fresh tab to be exercised.** An
  upgraded binary with an old tab open is a normal state, not an error.
- **Out-of-step reporting is the detector, not a bug.** If a live-verify makes a
  tab report `old_plugin`, that is the mechanism working.
- **Any Settings-derived value newly baked into an artifact needs a
  `spawn_inject_sig` entry**, or the user changes a setting and gets no restart
  hint. `opencode_plugin_wanted` documents the trap concretely: it and
  `spawn_inject_sig` add up *by argument, not by construction*, so a fifth
  disjunct there needs a matching signature input.

### Adding a capability an existing harness newly serves

1. **The artifact** — wire the handler / hook entry, gated on the same flag you
   will declare from.
2. **The hello** — add it to `serves` when that flag is on, and to `cannot` with
   a `why` when it is off. Compute both from the flag, never from live settings
   at hello time.
3. **The registry row** — with `wired_in`, a `drift_token` if it can report
   drift, and coverage. If it *replaces* a Tier-C reader, the new row is
   `Fallback { to: "<the Tier-C id>" }` and the old row stays `Silent` with its
   canary: a fallback that rots unnoticed is worse than one that never existed,
   because the primary's failure is what makes it load-bearing.
4. **Arbitration** — guard the reader's tap with `OobContext::pushed` and the
   push core with `chp::served`. One predicate, both sides.
5. **Quiet detection** — if another push's arrival *proves* this one should have
   fired, add a `witness_of` entry and a `drift_token_for_event` mapping so the
   silence lands in the same bucket as a malformed payload. If nothing proves it,
   **say so in the row** rather than inventing a threshold: a false-report
   machine is worse than a declared gap.
6. **`MAINTENANCE.md`** — the id, same commit.

### The recipe-9 discipline

> **Phase K is a refactor — a behavior change here is a defect, not a finding.**

That sentence generalizes. When a change is declared a relocation, a template
extraction or a rename, the emitted artifact's behavior must be identical, and
the way you prove it is a byte-level artifact (the goldens) or a re-run of the
behavior recipes — not a reading of the diff. If you find yourself explaining
why a behavior change during a refactor is acceptable, the change is a defect and
the refactor claim is wrong.

## 10. The security model

**The plugin is inside the TCB.** Three CHP events are security controls that
*execute inside the harness*, not data pipes: `tool.gate` (the V32 Phase H
native-tool refusal — the `throw` in `tool.execute.before`),
`checkpoint.pre_mutation` (V33 Phase F) and `taint.beacon` (V32). cImp only
*computes* the verdict; enforcement is in the generated artifact, because only it
sits in the harness's own tool path.

**The registry marks the places, not the concepts.** `CONTROLS` holds five ids,
each asserted to appear on exactly one row: `tool.gate` and
`checkpoint.pre_mutation` and `taint.beacon` on `opencode.plugin.load_all`, plus
`taint.beacon.claude` and `checkpoint.pre_mutation.claude` on the two Claude
beacon rows. The taint beacon and the checkpoint each run in *two* enforcement
sites, on two harnesses, in two source files, with two different failure modes —
folding them onto one id would have made the column say "enforcement lives
here", singular, about a row that is only half of it. `tool.gate` stays
OpenCode-only, and that is a fact rather than a gap: Claude's beacon hooks are
report-only by V32 locked decision 14, and structurally so since 2026-08-17 —
their handlers emit no decision field on any path, so there is nothing for a
deny to ride on.

**`serves` is not a trust claim.** An artifact declaring
`serves: ["tool.gate"]` has said nothing cImp relies on. The gate's authority
comes from cImp computing the verdict at `/latch/state` — which deliberately
carries no payload beyond identity, so the answer cannot depend on what the
caller claims about the tool it is about to run — and the artifact's only power
is to refuse *more* than it was told to.

**First-party only.** Because no cImp-side test can verify that a control inside
a harness ran, cImp loads no plugin it did not ship (design D7). This is also why
publishing plugin templates on the `detection-v1` channel is out of scope: that
channel ships *rules cImp consumes*, whereas a template is *code executing inside
the harness with a loopback token*, which turns it into a code-delivery channel
and raises the bar to signature verification.

**The loopback token is not a trust boundary.** It is readable by any process
running as the user — from the discovery file, and from the generated plugin
inside the project tree. "Authenticated" means *a local process*, never *cImp's
own child*, and every handler is written to that standard. What the token
posture buys is narrower and real: it is not in argv (Claude's rides
`allowedEnvVars` from the child env; OpenCode's is baked into a file inside the
project), it rotates per app run, and the artifact is regenerated at every spawn
because of it. Caller-asserted identity — `X-CIMP-Tab`, the body's `tab` — is
validated against the user's configured AI tabs before anything is recorded, so
the peer registry's key space is `configured AI tabs × 2 agents` and not
"whatever a request body said".

**Redaction rules.** Committed fixtures are **synthetic-minimal and
hand-authored** from the reader code's contract, never copied from a real
transcript (locked decision 4) — a real transcript carries user prompts, file
contents, tool output and plausibly credentials. Every fixture version directory
carries a `MANIFEST.toml` recording where the shape came from, and the suite
fails for a directory without one. Live captures are a different lane: every byte
goes through `scrub_payload` (escape strip + credential redaction driven by the
`graph/secrets.yar` ruleset) and **fails closed** — if the screen cannot run,
nothing is written; a hit it cannot localize to a JSON string value replaces the
whole line; a line over `MAX_SCRUBBABLE_LINE_BYTES` (64 KiB) is omitted
wholesale, never truncated. Captures live outside the repo and outside any
synced directory, bounded to three lines per capability and eight versions, and
promotion to a committed fixture stays manual and reviewed. The L2 probes read
real transcripts and print **counts and field names only** — never a payload
value, never a transcript path, never a session id; the one exception is the CLI
build string, which is harness metadata.

## 11. The upstream wishlist

Everything cImp is waiting on a harness for, in one place — each item tied to
the row it would move and re-checked on every capability audit. This section
used to live scattered across § 3.3, the reserved `chp::EVENTS` entries and
individual waiver texts; those stay authoritative, this is the checklist.
**Last checked 2026-08-17** (Claude Code 2.1.233 docs + changelog; OpenCode
1.18.18 source).

**Waiting on Claude Code:**

| Missing upstream | Would unlock | Row / reserved event |
|---|---|---|
| Token usage in any push surface (a usage block in a hook payload, on `Stop`, or on `SessionEnd`) | `claude.transcript.usage` C → B; `session.usage` goes live | reserved `session.usage` (`live: false`) |
| Context-window / rate-limit data in a hook | `claude.statusline.stdin` C → B; `session.context` goes live | reserved `session.context` |
| Sub-agent token usage on `SubagentStop`, or a payload naming the sub-agent transcript path | completes the Phase L sub-agent migration (lifecycle already pushed) | `claude.transcript.subagents` |
| Pushed session identity (`version`, `isSidechain`, `isMeta`) | retires `claude.transcript.identity`; gives the `harness_version` stale-kind its Claude producer | `claude.transcript.identity` |
| Compaction metrics on `PostCompact` | compaction observability without the transcript | — |
| A stable, documented transcript format | moot if the rows above land; upstream now **explicitly documents the JSONL as unstable**, and 2.1.210 shipped a transcript-compression change | every `claude.transcript.*` |

**Waiting on OpenCode:**

| Missing upstream | Would unlock | Row |
|---|---|---|
| A whole-message completion hook (`experimental.text.complete` graduating, plus a message-level signal beside it) | assistant text C → B without changing the unit the sentence segmenter is fed (§ 3.3) | `opencode.sse.events` |
| `permission.ask` actually firing (declared in the plugin types; **zero trigger sites** at 1.18.18) | dynamic permission interception instead of the gate's `throw` | `opencode.plugin.load_all` |
| The harness version exposed to plugins | the OpenCode `harness_version` producer | — |
| A stable tool-enumeration API (`/experimental/tool/ids` is explicitly experimental) | de-risks the registry probe | `opencode.tool_registry` |

**Flipped into cImp's court by the 2026-08-17 audit** — upstream already ships
these; the missing half is now a cImp consumer:

- **OpenCode real token accounting.** `message.updated` carries actual provider
  `info.tokens` / `cost` (per-step via `step-finish` parts; session totals are a
  DB projection, pollable but not pushed). cImp's OpenCode usage is still
  estimate-only from tool-call input args. Caveats recorded from the source
  audit: message-level `tokens` is the *last step's* usage, not a sum — per-step
  accumulation is the honest reading.
- **Claude `PermissionRequest` hook event** (new since 2.1.63) — could shrink
  what the `perm.tui_scrape` fallback has to cover.

**Watch items** (breaks, not features): OpenCode 2.0 **renames the `bash` tool
id** (stated in upstream source, `tool/shell/id.ts`); `session.idle` is
deprecated in the upstream schema (the reader honors its replacement,
`session.status`, since 2026-08-17); `opencode run` declares but ignores
`--port`; and the `auth_token` query param **beats** a correct `Authorization`
header when both are sent — cImp sends header-only.
