# V35 — Harness Resilience (capability matrix + drift canaries)

**Status:** IN PROGRESS — Phase A implemented 2026-08-16 (`82c0da2`); Phase B
underway. GitHub milestone 8, issues #54–#71 (one per phase; #55 = Phase A
closed by that commit; #63/#64 track the two pre-milestone gaps).
**Design source of truth:** this file for the *why*, the scope and the locked
decisions; the three companion drafts for the detailed design —
[DESIGN-harness-capability-matrix.md](DESIGN-harness-capability-matrix.md)
(what we depend on),
[DESIGN-harness-drift-canaries.md](DESIGN-harness-drift-canaries.md)
(proving it still holds), and
[DESIGN-harness-plugin-architecture.md](DESIGN-harness-plugin-architecture.md)
(making the surface small and confined instead of merely enumerable). An
`IMPL-PLAN-V35` splits out at build time if the phases need per-agent
contracts, matching V33's convention.

**Builds on:** V16 harness contract hardening — the eight drift rules
(`advisor.rs:149-156`), the `harness_versions` tripwire + Advisor **Mark
verified** action, the spike register (D0 / E1 / OpenCode-veto), and the prose
drift table in `MAINTENANCE.md` § *Claude Code / OpenCode CLIs — hook & plugin
behavior contracts*. Also the five hook shims (`context_hook.rs`,
`compact_hook.rs`, `read_hook.rs`, `postedit_hook.rs`, `notify_hook.rs`), the
two OOB adapters (`oob/claude.rs`, `oob/opencode.rs`), `statusline/mod.rs`,
`offload/toolclass.rs::OPENCODE_NATIVE_TABLE`, and the existing
two-sources-of-truth test pattern (`checks/mod.rs:1016`,
`graph/memory.rs:782` — `include_str!` the TS types, diff against Rust).

**Companion milestones:** V16 (this is its second half — V16 built *detection*,
V35 builds *declaration + leading verification*). Independent of V33; both
touch `advisor.rs` and the spawn/config seams, so serialize the Rust work.

## Why

cImp rides two user-installed, aggressively self-updating CLIs it does not
pin. The felt problem is a permanent adaptation tax: upstream ships, something
quietly stops working, and cImp reacts. V16 attacked this and got real
distance — but three structural gaps remain, and they are what actually
produce the treadmill:

1. **Every V16 drift rule is a lagging statistical indicator.**
   `drift.usage_fields_gone.v1` needs N Claude sessions with no token fields.
   `drift.read_reason.v1` needs enough remind→reread pairs to compute a rate.
   `drift.injection_unseen.v1` needs a follow-rate to collapse. Each one
   detects drift *by watching real work degrade first*. There is no leading
   check that runs in seconds and says "the shape changed".

2. **The dependency surface is undeclared.** What cImp needs from a harness
   lives in three disconnected places — the prose table in `MAINTENANCE.md`,
   the `harness_versions.*` spike strings, and constants scattered through
   `advisor.rs`, `toolclass.rs`, `tabs/config.rs`. Nothing links a contract to
   the code that depends on it, so "which features are degraded right now" is
   answerable only by reading source, and a new feature can take a fragile
   dependency with no record anywhere.

3. **The one leading signal cries wolf.** `drift.harness_version.v1` fires on
   *every* CLI auto-update whether or not anything broke. The rational
   response is to click **Mark verified** without running the ten-minute
   recipes — which disarms the control guarding all the others.

The organizing insight: **every dependency sits in a seam tier, and the tier —
not the feature — predicts how it breaks.**

| Tier | Seam | Breaks how | Examples |
|---|---|---|---|
| **A** | MCP protocol | loudly, versioned | `graph_*`, `run_check`, `offload_task` |
| **B** | documented hook / flag / settings key | at the payload boundary | `UserPromptSubmit`, `--settings`, `--session-id` |
| **C** | emitted artifact (not an API) | **silently, as zeros** | transcript JSONL, statusline stdin, OpenCode SSE |
| **D** | scraped UI / undocumented behavior | silently, on cosmetic changes | the TUI permission regex, `PreToolUse` timeout semantics |

Tier A has essentially never broken cImp. Every painful adaptation to date has
been C or D. V35 makes the tiers explicit, tests the C rows, and turns the
version tripwire from a reflex into a signal.

## Locked decisions

**1. Do not adopt an alternative harness (2026-08-16).**
`deepseek-ai/deepseek-harness` was evaluated in full: MIT, TypeScript/Cordis,
"everything is a plugin", developer preview. Its seams map almost 1:1 onto
cImp features (`ctx.tools` pre/post-execute waterfall, `ctx.approval`,
`ctx.shell`, `ctx.subprocess`, `ctx.fs`, `ctx.web`, `ctx.sessions`), and its
`packages/mcp/mcp-client` (stdio + streamable-http, `mcp__<server>__<tool>`
naming) would consume `cimp --offload-mcp` / `--code-audit-mcp` with **zero
code**. Rejected regardless: users want Claude Code, so adopting it is `+1`
integration and not `−2`; it is preview-stage and will churn faster than
Claude Code does; and it requires a Node runtime, which fights the
single-binary constraint. **Reopen if** cImp ever needs a seam Claude Code
structurally cannot provide (a real approval seam, a subprocess backend) — the
Phase G adapter trait is what keeps that option cheap.

**2. Rank by seam tier; D→C→B→A migration is standing work.** When an
upstream release makes it possible to move a dependency down a tier, that
migration outranks new harness features. The matrix makes the candidates
visible instead of leaving them to feel.

**3. Canaries assert substantiveness, not parse success.** Every reader is
deliberately lenient — `unwrap_or(0)` in `parse_usage_line`, "a parse failure
yields `Input::default()`" in `statusline/mod.rs`, `#[serde(default)]`
throughout. Leniency is correct for shims that must never break a user's turn,
but it means an upstream rename produces **zeros and empty strings, not
errors**. A canary that checks "does it parse" passes forever. Global
principle 5 (*empty is not absent*), applied to harness readers. This is the
load-bearing decision of the milestone.

**4. Committed fixtures are synthetic-minimal and redacted.** Real transcripts
carry user prompts, file contents, tool output and plausibly credentials.
Live captures go to a gitignored capture dir, scrubbed through
`processing/sanitize.rs`; promotion to a committed fixture under
`src-tauri/fixtures/harness/` is a manual, reviewed step. A fixture without a
`MANIFEST.toml` (captured-from version, date, method, redaction status) fails
the suite — an anonymous fixture is indistinguishable from a guess.

**5. V16's statistical rules stay.** They are not replaced by canaries; they
become *evidence attached to a capability* rather than standalone rules.
Lagging behavioral evidence and leading shape checks catch different things —
a field can still be present while the feature it feeds stops working.

**6. Do not unify the two OOB adapters' internals.** `oob/claude.rs` (JSONL
tailing) and `oob/opencode.rs` (SSE) read genuinely different wire formats. A
shared abstraction over both would be premature unification that makes *both*
harder to patch when one drifts. The `HarnessAdapter` trait names the seam
they already respect; it is not a rewrite.

**7. Passing canaries auto-advance `claude_last_verified`.** No click. The
**Mark verified** action survives only for Tier-D `Behavior` deps that no
probe can settle (D0, E1, OpenCode-veto — roughly three rows), so the button
means something again.

**8. "Unavailable" is not "broken".** A probe that cannot run (CLI absent, no
session to tail) reports *unknown*. A probe that finds a **better** upstream
(e.g. OpenCode grows auth) reports a capability *transition*. Neither is a red
test. Modelling these as failures would recreate the alarm fatigue this
milestone exists to remove.

**9. The harness seam is a protocol, not a Rust trait (2026-08-16).** Both
harnesses already POST an identical harness-neutral body
(`{cwd, prompt, session_id, agent, tab}`) to `/context/retrieve` —
`context_hook.rs:49` and `tabs/config.rs:2119`. That accidental protocol gets
named, versioned (`chp`) and declared as **CHP**. Everything above it types
against CHP, not against harness-shaped Rust, so a new harness adds no `match`
arms. The `HarnessAdapter` trait from the matrix draft survives as an L3
internal only. **The tier tells you why:** the push path (`/context/*`,
`/permission/event`, `/latch/*`) is Tier A/B and has never hurt; the read path
(`oob/*`, `statusline/*`) is Tier C/D and is where every painful adaptation
has landed. The difference between them is whether the plugin layer exists.

**10. No third-party plugin loading (2026-08-16).** cImp gets the plugin
*architecture* — clear layers, one directory per harness — but does **not**
load harness plugins it did not ship. No drop-in directory, no package
manifest, no signing. A new harness is a PR adding `harness/<id>/`, released
as part of cImp. **Why:** the harness plugin is inside the TCB. cImp only
*computes* the V32 Phase H verdict; the enforcement is a `throw` inside the
plugin's `tool.execute.before` (`tabs/config.rs:2205-2207`), and that same
generated file owns the V33 Phase F checkpoint trigger and the V32 taint
beacon. A plugin omitting the `throw` silently disables native-tool
containment while looking fully functional, and nothing outside a harness can
verify that a control inside it ran. Kept from the rejected model: the matrix
gains a **TCB column** marking `tool.gate` / `checkpoint.pre_mutation` /
`taint.beacon` as controls rather than data — documentation, not a gate.

## Phases

| Phase | Work | Exit criteria |
|---|---|---|
| **A** | `src-tauri/src/harness/contract.rs` — the `Capability` registry, seeded with the ~18 real rows in the matrix draft §2.1, plus the **TCB column** from decision 10 | Registry compiles; the three consistency tests pass (below); every control-implementing capability is marked as such |
| **B** | L1 fixture canaries for the four Tier-C readers: transcript usage, transcript tool_result, statusline stdin, OpenCode SSE | `cargo test` fails if any of those readers stops producing substantive output from a real fixture |
| **C** | Negative canaries (renamed-field fixtures) + the matrix↔canary cross-check | Every `Silent` capability has a canary or a recorded waiver; no canary exists outside the matrix |
| **D** | `cimp --harness-canary [--json]` live probe. **Start with the `opencode.tool_registry` diff** | Probe exits non-zero on a real drift; unclassified OpenCode tool ids fail |
| **E** | Advisor + feature gating read from the matrix; retire `HarnessVersions::e1_blocked()` and the `harnessStatusBlocks` frontend mirror | One notice source (`drift.capability.v1`); no bespoke gate constants left |
| **F** | Auto-run L1+L2 on version change; auto-advance `claude_last_verified` on all-pass | A routine CLI auto-update produces **no** Advisor card |
| **G** | Settings → **Harness health** panel: capability, tier, last canary result, last verified version | "What is broken right now" is answerable without reading source |
| **H** | Capture-on-success corpus, stamped per CLI version | A breakage's first diagnostic is a diff, not an investigation |

Phase A is worth landing alone — seeding the table already surfaced two live
gaps (below). Phases A+B+D are the value core; E–H are consolidation.

### Phases I–M — the plugin architecture (decisions 9 + 10)

A+B+D make the surface *loud*; I–M make it *small*. Independent of A–H except
that M's capability ids come from the Phase A registry. Full design in
[DESIGN-harness-plugin-architecture.md](DESIGN-harness-plugin-architecture.md).

| Phase | Work | Exit criteria |
|---|---|---|
| **I** | Declare **CHP**: name the existing loopback routes, add `chp` + a `/session/hello` capability negotiation. Zero behavior change. | The protocol has a written schema and a version on every message |
| **J** | Claude hook shims → `type: "http"` hooks pointing straight at loopback (`headers` + `allowedEnvVars` carry the bearer token, as the OpenCode plugin already does) | The five `*_hook.rs` shims are deleted; Claude's L1 has the same shape as OpenCode's |
| **K** | Move `harness/` into place: `OobSpec` → registry lookup, `oob/{claude,opencode}.rs` → `harness/<id>/read.rs`, plus the layering tests | `no_harness_literals_outside_harness` passes; a contributor can be pointed at one directory |
| **L** | Push the read path, one capability at a time: permission → usage → assistant text → subagents | Each migrated capability moves C→B and deletes a silent-zeros failure mode |
| **M** | Plugin templates out of `format!()` into real `.js`/`.json` files (`include_str!` + a checked substitution key set) | Every `{{key}}` is in the known set or the build fails; an upstream change is a readable diff |

Phase J is the highest value-to-risk ratio. Phase K is a pure refactor with no
behavior change — cheap to land, cheap to review, and it is what makes a third
harness additive. **Phase K is a large file relocation**: the tree is not
rustfmt-clean and is sometimes shared with a second agent, so scope every git
operation to explicit paths.

**Not scheduled:** publishing plugin templates on the `detection-v1` channel.
That channel ships *rules cImp consumes*; a template is *code executing inside
the harness with a loopback token*, which turns it into a code-delivery
channel and raises the bar to signature verification — on top of the updater
work already deferred to #53. Revisit only if release latency starts to hurt.

### Phase A consistency tests

Following the existing `include_str!` two-sources-of-truth pattern:

- `matrix_matches_maintenance_doc` — parse the `MAINTENANCE.md` drift table,
  assert row-for-row parity with the registry. Prose can no longer drift from
  code.
- `every_silent_degradation_has_a_canary_or_a_waiver` — a `Silent` row with
  neither fails the build. This is the rule that stops new fragile
  dependencies entering unrecorded.
- `wired_in_paths_exist` — every declared consumer path resolves to a real
  file.

## Implementation record

**Phase A (`82c0da2`, 2026-08-16).** Registry seeded (18 rows), four tests
(the three below + `tcb_controls_are_declared_exactly_once`), MAINTENANCE.md
drift table gained a leading **"Capability id(s)"** column + one new row
(*Transcript tap — shape beyond usage*) for the four ids that had no prose row.
Build-time decisions, all binding on later phases:

- **Parity test works via id-set equality**, not prose parsing: backticked
  ids extracted from the drift table's `|` lines, set-compared both ways
  against the registry (plus uniqueness on both sides). A doc row may carry
  several ids.
- **`drift_rule` is a slice**, referencing the `advisor::RULE_DRIFT_*` consts
  (never duplicated literals) — several rows lag via multiple rules.
- **`waiver: Option<&str>`** exists from day one so
  `every_silent_degradation_has_a_canary_or_a_waiver` enforces structurally
  before any canary lands; phases B–D swap waivers for canary ids.
- **Tier is a single `Seam`**; a row's D-component is expressed as a
  `Dep::Behavior` entry (the §2.1 "B+D" rows are `Seam::B` + a Behavior dep;
  `precompact` also carries one — decision 7 names D0 among the spike rows).
- **TCB column** = `controls: &[&str]`; the three control ids live on
  `opencode.plugin.load_all` and a test pins each to exactly one row.

Findings out of Phase A (close-or-defer decided 2026-08-16):

1. `perm.tui_scrape`'s literal `"Esc to cancel · Tab to amend"` is **stale in
   the design docs and MAINTENANCE prose** — `processing/permission.rs:18-40`
   ships `to cancel ·` / `to cancel` + `1. Yes 2.` with `none_of` guards.
   Registry follows the code; MAINTENANCE cell corrected in Phase B.
2. `claude.hook.posttooluse` has **zero lagging coverage** —
   `postedit_hook.rs` never calls `report_contract_drift` (the "three shims"
   in the V16 notes is literal). Explicit waiver; closes with the Phase D
   probe class.
3. `claude.transcript.identity` is the **inverse of the version tripwire** —
   losing `version` silences `drift.harness_version.v1` rather than firing
   it. Documented in the row's waiver; no drift_rule link.
4. `GET /experimental/tool/ids` is **never called by cImp today** (doc-comment
   only) — the `Dep::Route` is the Phase D probe's target, said so in the
   waiver.
5. `claude.flag.session_id`'s `VisibleOff` is **declared intent, not observed
   behavior** (a vanished flag kills the tab loudly instead) — Phase E must
   implement it or downgrade the row.

## Two gaps to fix ahead of the milestone

Both surfaced while seeding the matrix; neither depends on V35 landing.

1. **`opencode.tool_registry` is silent and security-relevant.**
   `OPENCODE_NATIVE_TABLE` is allowlist-only by deliberate design (unknown ⇒
   EXTERNAL is wrong for a harness's own registry), so a **new upstream
   OpenCode tool ships ungated and nothing fails loudly**. Detection today is
   a human remembering to diff `GET /experimental/tool/ids`. `apply_patch` is
   the standing example of why it matters. → Phase D's first probe.
2. **The `--settings` overlay key set has no test at all** (recorded as a V32
   accepted residual). cImp emits `hooks`, `statusLine`, and `permissions` in
   native-web `deny` mode; an upstream key rename or a stricter schema breaks
   the overlay silently. Tier B, cheap to canary.

## Live-verify recipes

1. **Canary catches a real rename.** Hand-edit a committed fixture to rename
   `input_tokens`; `cargo test` must fail naming `claude.transcript.usage`.
   Revert.
2. **Auto-verify on a no-op update.** With canaries green, bump
   `claude_last_seen` by hand to a new version string; confirm
   `claude_last_verified` advances on its own and **no** Advisor card appears.
3. **Precise notice on a real break.** Point the probe at a doctored
   statusline payload with `context_window` removed; confirm the Advisor card
   names `claude.statusline.stdin` and cites `statusline/mod.rs`.
4. **Unclassified OpenCode tool.** Add a fake id to the live
   `/experimental/tool/ids` response (or remove one from the table); confirm
   `cimp --harness-canary` exits non-zero and names the id.
5. **Unavailable ≠ broken.** Run the probe with no Claude Code installed;
   confirm every Claude capability reports *unknown*, the exit code is 0, and
   nothing is marked failed.
6. **Redaction.** Run `cimp --harness-capture` against a session containing a
   fake secret; confirm the capture lands only in the gitignored dir and the
   secret is scrubbed.

Phases I–M:

7. **HTTP hooks carry identity (J).** Launch a Claude tab, submit a prompt,
   confirm context injection still lands — and that the loopback access log
   shows the POST arriving from the harness with the bearer token, with no
   `cimp --context-hook` process ever spawned.
8. **Stale plugin is detected, not mysterious (I + D5).** Launch a tab, then
   hand-edit the generated plugin's `chp` to an older value; confirm cImp
   reports a version mismatch rather than the capability silently misbehaving.
   This is the trap V32 hit four times as "needs a FRESH TAB".
9. **Gate still enforces after the move (K + L).** Re-run the V32 Phase H
   native-tool refusal recipes against the relocated `harness/opencode/`;
   the refusal strings and the `throw` must be byte-identical in behavior.
   **Phase K is a refactor — a behavior change here is a defect, not a
   finding.**
10. **TTS survives the push path (L).** With assistant text arriving over CHP
    rather than the JSONL tail, confirm sentence segmentation is unchanged for
    Claude (complete text at message finish) *and* OpenCode (token deltas) —
    the two cadences must not be flattened into one.

## Deploy traps

- **Settings schema bump** if Phase G stores per-capability canary results.
  Additive fields on the `Settings` *container* still need care — the
  container-level `#[serde(default)]` trap that bit V32's F-19 applies.
- **`harness_versions` is written out-of-band** (`ipc/commands.rs:848-856`
  bans it from project overlay diffs; the tap and tab spawn write the physical
  global directly). Phase F's auto-advance must go through
  `mutate_global_harness_versions`, never through a Settings save.
- **Retiring `harnessStatusBlocks`** (Phase E) touches
  `src/lib/settings/types.ts` — the Rust and TS sides are diffed by an
  existing test, so both move together or the suite fails.
- **Fixtures are `include_str!`-adjacent**: adding
  `src-tauri/fixtures/**` changes what a release build embeds if loaded that
  way. Prefer runtime path loading for the corpus, `include_str!` only for the
  small synthetic fixtures.

## Accepted residuals

- **Behavior contracts stay manual.** Whether a `PreToolUse` deny reason
  reaches the *model*, whether a hook timeout blocks — no payload reveals
  these. They remain spikes. V35's contribution is narrow but real: it makes
  the unverifiable ones *countable*, so an unrun spike is a visible row rather
  than a `TODO` in a module doc.
- **L2 needs a scripted turn** for the hook and SSE probes — an API call and a
  few seconds. Acceptable at version-change and maintenance cadence; not at
  every tab spawn.
- **Fixtures rot.** A committed fixture from an old version keeps L1 green
  while reality moves. That is precisely why L2 exists and why it refreshes
  fixtures on success rather than only asserting against them.
- **This does not reduce the number of things cImp depends on.** It makes them
  enumerable, tested, and loud. Reducing the count is decision 2's ongoing
  D→C→B→A migration work, not a phase here.

## Out of scope

- Any alternative-harness adapter (locked decision 1). Phases I–K make the
  option cost one directory; nothing in V35 exercises it.
- Third-party plugin loading (locked decision 10) — no drop-in directory, no
  package format, no signing.
- Publishing plugin templates on the detection channel (see Phases I–M).
- Unifying the OOB adapters' internals (locked decision 6). Phase L *retires*
  them from the hot path and keeps each as a per-harness fallback; it does not
  build a shared abstraction over JSONL-tailing and SSE.
- New harness *features*. V35 is entirely about the durability of what exists.
