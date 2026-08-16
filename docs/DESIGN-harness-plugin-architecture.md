# Harness plugin architecture (draft)

**Status:** draft, not implemented. Third companion to
`docs/DESIGN-harness-capability-matrix.md` (what we depend on) and
`docs/DESIGN-harness-drift-canaries.md` (proving it still holds). Those two
make the dependency surface *enumerable and loud*; this one makes it *small
and confined*. Fold into V35, or split as V36 — the matrix is a prerequisite
either way, since it supplies the capability vocabulary this design types
against.

**The ask being answered:** a layered architecture — cImp capabilities ▸ cImp
plugin layer ▸ harness plugin ▸ harness — such that supporting a **new
harness**, or absorbing a **change in an existing one**, means implementing
one thing in one place.

## 1. The finding: half of this already exists and works

cImp integrates with each harness along two independent paths, and they have
opposite shapes.

**The push path — the harness calls cImp.** Both harnesses already send an
identical, harness-neutral body to the same loopback route:

```
context_hook.rs:49    {"cwd":…,"prompt":…,"session_id":…,"agent":"claude",  "tab":…}
tabs/config.rs:2119   {"cwd":…,"prompt":…,"session_id":…,"agent":"opencode","tab":…}
                                                    ↓  POST /context/retrieve
```

`agent` is already the discriminator. The same shape carries
`/context/post_edit`, `/context/should_read`, `/context/compaction`,
`/permission/event`, `/memory/event`, `/latch/{beacon,state}`. **This is a
harness-neutral protocol that nobody has named, versioned, or declared.**

**The read path — cImp reaches into the harness.** Usage, assistant prose for
TTS, session identity, tool results, and subagent discovery come from
`oob/claude.rs` tailing transcript JSONL (2969 lines), `oob/opencode.rs`
consuming SSE (2193 lines), and `statusline/mod.rs` parsing stdin (949 lines).
No plugin layer exists here at all.

Now overlay the V35 tier table:

| Path | Layered? | Tier | Failure mode |
|---|---|---|---|
| push (`/context/*`, `/permission/*`, `/latch/*`) | yes | A/B | at the payload boundary, usually announced |
| read (`oob/*`, `statusline/*`) | **no** | **C/D** | **silently, as zeros and empties** |

Every painful adaptation cImp has absorbed has been on the unlayered half.
That is not a coincidence to note in passing — it is the entire argument for
this design. **The architecture already works where it exists.** The work is
to extend it over the read path, and to make the seam explicit enough that a
third harness is additive.

## 2. The four layers

```
  L4  Capabilities        context injection · memory · usage · TTS · permission
                          detection · read advisor · post-edit checks · code intel
                          ── speak ONLY cImp domain types + contract::gate(id)

  L3  Session bus         normalizes CHP events into cImp domain events;
                          owns the capability registry (the V35 matrix) and
                          every degradation decision
                          ── harness-agnostic; no harness string literals

  ══  L2  CHP — cImp Harness Protocol  ═══════════ THE STABLE SEAM ═══════════
                          versioned HTTP+JSON on loopback, bearer auth,
                          `agent` discriminator, capability negotiation

  L1  Harness plugin      GENERATED, harness-native. Speaks the harness's own
                          extension mechanism on one side, CHP on the other.
                          ── THE ONLY PER-HARNESS ARTIFACT

  L0  Harness             Claude Code · OpenCode · next one. Uncontrolled,
                          unpinned, self-updating.
```

Today L1 is **fully realized for OpenCode** (`tabs/config.rs::opencode_plugin_source`
generates an ES module that POSTs to loopback with `Bearer CIMP_TOKEN`) and
**scattered for Claude** (five shim binaries — `context_hook.rs`,
`compact_hook.rs`, `read_hook.rs`, `postedit_hook.rs`, `notify_hook.rs`, 1053
lines — plus a `--settings` overlay generator, plus the OOB tap that bypasses
L1 entirely).

The asymmetry is historical, not principled. The OpenCode side is the better
design, and it is the one to standardize on.

## 3. Locked decision candidates

D1–D6 below; **D7 (no third-party plugin loading) is in §5**, where its
rationale sits next to the layout it constrains. D7 is already decided — the
rest are proposals.

### D1. The extension point is the protocol, not a Rust trait

The V35 matrix draft §4 proposes a `HarnessAdapter` trait. Keep it — but as an
**L3 internal type**, never as the extension point.

If the seam were the trait, everything above L1 would be typed against
*harness-shaped* Rust, and each new harness would mean new `match` arms
wherever that type is consumed — which is the situation today (`OobSpec` at
`oob/mod.rs:44`, its `spawn` arm at `oob/mod.rs:93`, plus `tabs/config.rs`).
Making the seam a **wire protocol** means L3 and above are typed against CHP,
which does not change when a harness is added; the trait is then just how L3
holds its own two implementations, and it can be refactored freely without
touching a capability.

The second consequence is that the per-harness artifact is **text** (§5.1),
which is what makes an upstream rename a readable diff in a template rather
than an edit inside a `format!()` string. Under D7 that text still ships in
the binary — see §7 step 6 for the one case where publishing it separately is
worth revisiting, and why that is a different decision than it looks.

### D2. Push-only. Every read becomes a push, or degrades visibly

The read path is where Tier C lives. Each OOB reader gets a CHP event the
plugin pushes instead:

| Today (Tier C, silent zeros) | CHP event |
|---|---|
| `oob/claude.rs::parse_usage_line` tailing JSONL | `POST /session/usage` |
| `oob/claude.rs` tool_result extraction | `POST /session/tool_result` |
| `oob/claude.rs::SubagentFile` discovery | `POST /session/subagent` |
| `oob/*` assistant prose → TTS | `POST /session/assistant_text` |
| `statusline/mod.rs` context window / quota | `POST /session/context` |
| `opencode.sse.events` | the same four routes above |

Claude Code can serve all of these today without cImp reading a single
artifact: `Stop` (`last_assistant_message`), `MessageDisplay` (`message_delta`,
streaming), `PostToolUse` (`tool_result`), `SubagentStart`/`SubagentStop`
(`agent_id`, `agent_type`), `PostCompact` (`tokens_removed`), plus the OTel
`claude_code.token.usage` metric with `session.id`/`model` attributes.

**This does not violate V35 decision 6** ("do not unify the two OOB adapters'
internals"). That decision forbids a shared *abstraction over* JSONL-tailing
and SSE — correct, they are genuinely different wire formats. This design does
something else: it **retires both readers from the hot path**, and keeps each
one as a per-harness *fallback inside its own L1 plugin*, where drift is
confined by construction. Nothing shared is built over them.

### D3. Capability negotiation at connect, not version sniffing

The plugin opens with a hello:

```json
POST /session/hello
{ "chp": 1, "agent": "claude", "harness_version": "2.4.1",
  "serves": ["session.usage", "session.assistant_text", "permission.request",
             "context.inject", "tool.gate"],
  "cannot": [{"id":"context.compaction","why":"PreCompact unavailable <2.2"}] }
```

L3 gates features from `serves`, not from parsing a version string. A
capability absent from `serves` is **unavailable, not broken** — V35 decision
8, now enforced by the protocol rather than by convention. This retires
`HarnessVersions::e1_blocked()` and the `harnessStatusBlocks` frontend mirror
(V35 Phase E) without needing a bespoke gate per feature.

### D4. Core + optional vocabulary. Never a union

Two harnesses' event sets cannot be unioned without the intersection rotting
into the shape of whichever one was implemented first — and a third harness
would then arrive as `Option<T>` fields threaded through L3.

Define a small **core** every harness must serve to be usable at all
(`hello`, `prompt`, `assistant_text`, `session_end`), and an **optional** set
each capability declares a dependency on. The V35 `Capability` registry is
exactly this declaration; here it stops being documentation and becomes the
type L4 gates against.

### D5. Version the protocol, not just the harness

Every CHP message carries `chp`. This is not ceremony — it closes a class of
bug this project has hit at least four times.

The generated plugin and the `--settings` overlay are **spawn-baked**: an
upgraded binary with an old tab still running has an old plugin on disk
speaking to new loopback code. V32 recorded this as a deploy trap four times
(F-13, M-16, F-23, F-32's child half) with the mitigation "needs a FRESH TAB
or it reads as a failure". With `chp` on the wire, L3 *detects* the stale
plugin, and can adapt or say so — instead of the mismatch surfacing as a
mysterious functional failure during a live-verify.

### D6. The plugin is generated, and generation is a declared capability

Both plugins are already generated from Rust (`opencode_plugin_source`, the
`--settings` overlay writer). Keep that — generation is what lets a plugin
carry per-tab identity (`CIMP_TAB_ID`), the launch token, and the reviewed
tables (`OPENCODE_NATIVE_TABLE`) without a config file the user can desync.

A harness's L1 module therefore owns exactly three things:

1. **`emit(profile) -> Vec<GeneratedFile>`** — the plugin/overlay artifacts.
2. **`hello()`** — which CHP capabilities this harness can serve, and why not
   for the rest.
3. **A fallback reader** (optional) — for capabilities the harness cannot
   push, e.g. an old CLI. Tier C stays possible; it is now *contained and
   declared* rather than ambient.

## 4. Where the layers live in the tree

A layering that exists only in a design doc rots. It has to be a directory
someone can be pointed at.

Today there is no such directory. Harness knowledge is spread across nine
locations, none named for it: `oob/{claude,opencode,mod}.rs`,
`statusline/mod.rs`, five top-level `*_hook.rs`, `tabs/config.rs`,
`offload/toolclass.rs`, `advisor.rs`.

Target:

```
src-tauri/src/
  harness/                    ← L1 + L2. The entire harness surface.
    README.md                 ← entry point: "I want to add a harness"
    mod.rs                    registry, load order, per-tab resolution
    contract.rs               the V35 capability registry (the matrix)
    chp/
      mod.rs                  protocol version, hello / negotiation
      events.rs               core + optional event vocabulary (D4)
      routes.rs               route table; handlers stay in offload/loopback.rs
    plugin/
      manifest.rs             package format: parse + validate
      render.rs               template substitution, the documented key set
      install.rs              write artifacts to declared targets, sweep stale
    claude/
      mod.rs                  hello(): what this harness serves, and why not
      templates/              settings overlay + hook config
      read.rs                 LEGACY fallback reader (was oob/claude.rs)
      statusline.rs           Claude-shaped payload parsing
    opencode/
      mod.rs
      templates/              plugin.js
      read.rs                 LEGACY fallback reader (was oob/opencode.rs)
      tools.rs                OPENCODE_NATIVE_TABLE
```

Everything above L2 keeps its current home (`graph/`, `tts/`, `usage/`,
`workbench/`, `offload/`) and *loses* its harness knowledge.

The moves, concretely:

| From | To |
|---|---|
| `oob/mod.rs` (`OobSpec` enum + `spawn`) | `harness/mod.rs` (registry lookup) |
| `oob/claude.rs`, `oob/opencode.rs` | `harness/<id>/read.rs` |
| `oob/prose.rs` | stays shared — it is sentence segmentation, harness-neutral |
| five `*_hook.rs` shims | `harness/claude/shims/`, **deleted** at migration step 2 |
| `statusline/mod.rs` | CLI entry stays put (`main.rs` dispatches it); the Claude-shaped parsing moves to `harness/claude/statusline.rs` |
| `offload/toolclass.rs::OPENCODE_NATIVE_TABLE` | `harness/opencode/tools.rs` |
| `tabs/config.rs::opencode_plugin_source` + overlay writer | `harness/<id>/templates/` — pulls a large block out of a 7122-line file |

`docs/ARCHITECTURE.md` gets an **"### Adding a harness plugin"** section
beside the existing "### Adding a `run_check` parser" (line 641), which is
already exactly this genre of how-to. `harness/README.md` is its in-tree twin.

### 4.1 Enforcing the layering

Four tests, all cheap, all in the repo's existing two-sources-of-truth idiom:

- **`no_harness_literals_outside_harness`** — no harness-owned string
  (`"input_tokens"`, `"hookSpecificOutput"`, `"Esc to cancel · Tab to amend"`,
  `"message.part.delta"`) appears outside `harness/`. *Already nearly true* —
  today they are confined to `oob/`, `statusline/` and the five shims, all of
  which move in. This converts an undocumented habit into an invariant.
- **`harness_modules_do_not_import_capabilities`** — dependency direction is
  L1 → L2 only. A harness module that reaches up into `graph::` or `tts::` has
  put capability logic in the wrong layer.
- **`every_harness_dir_declares_its_capabilities`** — a `harness/<id>/` with
  no `hello()` and no matrix rows is a harness nobody can reason about.
- **`wired_in_paths_exist`** — V35 Phase A, unchanged.

## 5. Plugin artifacts are template files, in-tree

### D7. No third-party plugin loading (decided)

**cImp does not load harness plugins it did not ship.** There is no
`<app-data>/harness-plugins/` drop-in directory, no package manifest format,
no signing infrastructure. Adding a harness means adding `harness/<id>/` to
this repository and opening a PR.

This was considered and rejected on a specific finding, recorded here so it is
not re-litigated later:

**The harness plugin is inside the TCB.** cImp's app side only *computes* the
V32 Phase H verdict — `/latch/state` returns
`{gate, latch, contaminated, local_by_user_flip}`. The **enforcement** is a
`throw` inside the plugin's `tool.execute.before`
(`tabs/config.rs:2205-2207`), because only the plugin sits in the harness's
own tool path. That one file also owns the V33 Phase F checkpoint trigger and
the V32 taint beacon.

A plugin that omits the `throw` — through malice, a misunderstanding, or an
upstream API change its author did not track — **silently disables native-tool
containment for that harness while appearing completely functional.** No
cImp-side test can catch it: the control does not run in cImp's process, and
nothing outside a harness can verify that a control inside it ran. Any
mediation would therefore have to be a hard rule (unsigned plugin ⇒ every
containment-dependent feature fail-closed off), which is most of the cost of
first-party review with none of its benefit.

Two notes worth keeping from that analysis:

- **The bearer token was never the crown jewel.**
  `Discovery { port, token, pid, root }` is written to a file in the exe
  directory with permissions tightened to `0600` **only under
  `#[cfg(unix)]`** (`offload/loopback.rs:897`) — on the primary Windows target
  that call does not run. Any local process running as the user can already
  read it. **This is independent of plugins and worth checking on its own:**
  `portable_root()` is the exe directory, and at least one deployment keeps
  the live exe on a synced network path. Verify the ACLs; consider
  `%LOCALAPPDATA%` for the discovery file regardless of where the exe sits.
- **Mark the TCB capabilities in the matrix anyway.** `tool.gate`,
  `checkpoint.pre_mutation` and `taint.beacon` are *controls*; the rest move
  data. That annotation costs a column and tells the next developer editing
  `harness/opencode/templates/` that they are touching a security control, not
  a data pipe. This is the part of the rejected trust model worth keeping —
  as documentation, not as a gate.

### 5.1 Templates as files, values from reviewed Rust

Dropping third-party loading does **not** mean keeping plugin generation as it
is. Today the OpenCode plugin lives inside a `format!()` string in a
7122-line Rust file, with every JS brace doubled (`{{`/`}}`). That is hostile
to exactly the thing this milestone is for: reading a diff when upstream
changes.

Split it — the artifact becomes a file, the values stay in Rust:

```
harness/opencode/templates/plugin.js      ← real .js: highlighting, lint, readable diff
harness/claude/templates/settings.json    ← the --settings overlay
```

`include_str!` them and substitute a fixed, documented key set
(`{{cimp.loopback_url}}`, `{{cimp.token}}`, `{{cimp.tab_id}}`,
`{{cimp.chp_version}}`, `{{cimp.tools.local}}`, `{{cimp.refusal.local}}`).
The substitution *values* still come from reviewed Rust — `OPENCODE_NATIVE_TABLE`
rendered through serde, the refusal constants JSON-quoted — which is what
today's generator is careful about and must not regress: a tool name added
later must never be able to malform the emitted JS.

`plugin/render.rs` owns substitution, and a test asserts every `{{key}}` in
every template is in the known set, so a typo fails the build instead of
emitting an empty string into a live plugin.

This is a pure developer-experience change with no trust surface — and it is
the one piece of the package-format idea that survives D7 on its own merits.

### 5.2 Effect on rendering, TTS and STT

Three questions the layering raises, answered from the current wiring:

- **STT — unaffected, structurally.** Dictation runs audio → whisper →
  `app.emit("stt-transcription")` to the frontend (`stt/worker.rs:152`), which
  inserts the text. It never touches the harness, so no plugin sits in that
  path under any variant of this design.
- **TTS — affected by design, and this is deliberate.** Today assistant prose
  reaches `TtsRequest` from `oob/claude.rs:2052` and the OpenCode SSE tap.
  Migration step 4 makes the plugin the source of spoken text, which means a
  plugin controls *what cImp says out loud*. That is an annoyance and a
  social-engineering surface, not code execution — so `session.assistant_text`
  belongs in the freely-declarable data tier. Two constraints keep it there:
  segmentation stays app-side in `oob/prose.rs` (the plugin sends prose, never
  markup or control), and the existing per-tab `tts_injection.enabled` gate
  still applies. Worth noting the timing characteristics differ per harness —
  Claude yields complete text at message finish, OpenCode yields token deltas
  — so the push path must preserve what the segmenter assumes rather than
  flattening both into one cadence.
- **Rendering — not touched directly, but reachable through the harness.**
  cImp's xterm renderer draws the harness's PTY output; a plugin never writes
  to it. Two indirect paths exist and both are real:
  1. **Latency.** A hook that blocks stalls the turn the user is watching.
     Claude's `MessageDisplay` runs *during streaming* with a 10s default
     timeout; `UserPromptSubmit` defaults to 30s. cImp's own shims deliberately
     budget 600ms (`context_hook.rs:21`, "kept small so a slow/cold index never
     delays the prompt").
  2. **Escape sequences.** Hook output may carry `terminalSequence`, which the
     harness writes into the PTY that cImp renders.

  Neither is a trust problem under D7, but both are easy to regress by hand.
  Pin them at render time: the `timeout` values are written by
  `plugin/render.rs`, not typed into the template, and `terminalSequence` is
  not a CHP capability at all. A test on the emitted config is cheaper than a
  convention nobody remembers — the 600ms budget is load-bearing and currently
  survives only as a comment.

## 6. What a new harness costs

A new harness is a PR adding one directory:

| Work | Where |
|---|---|
| plugin artifact | `harness/<id>/templates/*` |
| what it serves, and why not the rest | `harness/<id>/mod.rs::hello()` |
| capability rows (if it serves something new) | the V35 registry |
| fallback reader | `harness/<id>/read.rs` — only if it cannot push |
| **changes to L2 / L3 / L4** | **none** |

Specifically **not** required, all of which are required today: a new
`OobSpec` enum variant (`oob/mod.rs:44`), a new arm in `oob::spawn`
(`oob/mod.rs:93`), new match arms in `tabs/config.rs`, new bespoke gate
constants, new frontend mirrors.

The same table answers the second half of the ask. A **change** in an existing
harness lands in the same one place — a template edit and possibly a
`manifest.toml` line — because L2 is what everything downstream depends on,
not the harness. An upstream field rename is a template change; an upstream
capability *removal* is a `serves` line moving to `cannot`, which L3 turns
into a visibly-degraded feature with no code change anywhere above it.

## 7. Migration order

Each step is independently valuable and independently shippable. Nothing here
is a big-bang rewrite; L2 already carries production traffic.

| # | Step | Why first / value |
|---|---|---|
| 1 | **Declare CHP.** Name the existing routes, add `chp` + `hello`, write the schema down. Zero behavior change. | Turns an accidental protocol into a contract. Prerequisite for everything else. |
| 2 | **Claude shims → `type: "http"` hooks.** Point Claude's hooks straight at loopback (`headers` + `allowedEnvVars` carry the bearer token, exactly as the OpenCode plugin does). | Deletes 1053 lines and five spawn paths. Makes Claude's L1 the same shape as OpenCode's. |
| 3 | **Move `harness/` into place** (§4) — `OobSpec` → registry lookup, the two adapters become `harness/{claude,opencode}/`, plus the four §4.1 layering tests. | Pure refactor, no behavior change. The step that makes everything after it additive rather than invasive, and the first point at which a contributor can be pointed at a directory. |
| 4 | **Push the read path.** One capability at a time: permission → usage → assistant text → subagents. | The tier reduction. Each step deletes a silent-zeros failure mode and can ship alone. |
| 5 | **Templates out of `format!()`** (§5.1) — real `.js` / `.json` files, `include_str!` + a checked substitution key set. | Makes an upstream change a readable diff. Pure DX, no trust surface. |
| 6 | *(optional, not scheduled)* **Publish templates on the detection channel.** | Drift fixes in a day, not a release — but see below. |

Step 2 is the highest ratio of value to risk and should go first after #1.
Step 3 is what the "where do plugins sit" question actually asks for, and it
is a refactor with no behavior change — cheap to land and cheap to review.
Steps 1–5 are the whole design under D7; none of them opens a new trust
surface.

**On step 6.** It is tempting — the `detection-v1` channel already exists and
already publishes, validates, watermarks and reverts. But it is a bigger
decision than it looks: that channel today ships **rules cImp consumes**,
whereas a plugin template is **code that executes inside the harness with a
loopback token**. Publishing one turns the update channel into a code-delivery
channel, which raises the bar from "manifest + watermark" to signature
verification — on top of the updater work already deferred to #53 (and the
channel has no SSRF screening today). D7 removes the *third-party* trust
problem; it does not by itself make this safe. Leave it unscheduled until
someone actually feels the release latency.

**Move hygiene:** step 3 is a large file relocation in a tree that is not
rustfmt-clean and is sometimes shared with a second agent. Scope every git
operation to explicit paths — never `git add -A`, never a repo-wide format.

## 8. What this does not solve

- **Behavior contracts are still unverifiable.** Whether a `PreToolUse` deny
  reason reaches the model (E1) is not a payload fact and no protocol layer
  reveals it. Still spikes.
- **A harness that cannot push cannot be layered.** If a future harness has no
  extension mechanism, L1 degenerates into a fallback reader and that harness
  simply lives at Tier C. The architecture makes that *visible and contained*;
  it cannot make it go away.
- **CHP is still a compatibility surface, even under D7.** Plugins are written
  to disk at tab launch and outlive the binary that wrote them, so `chp`
  versions must be supported for as long as a stale artifact can exist. That
  is a real, permanent cost — accepted because it replaces an *undetectable*
  staleness failure (D5) with a versioned one.
- **Adding a harness still needs a release.** That is the deliberate trade in
  D7: harnesses ship with cImp. The cost is bounded by §6 — one directory, no
  changes above L2 — but it is a release, not a drop-in.
- **This is not a reason to add harnesses.** V35 decision 1 (do not adopt an
  alternative harness) stands unchanged. The point is that the *option* costs
  one module instead of a scattering of match arms — which is precisely the
  condition under which decision 1 was made, and which this design preserves
  rather than reverses.
