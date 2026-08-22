# `harness/opencode` — OpenCode

Everything cImp knows about **OpenCode specifically**: what it depends on, what
an OpenCode release could silently change, and how to tell. This file is the
human twin of the machine-readable rows in [`contract.rs`](../contract.rs) and
of this directory's `impl HarnessPlugin`
([`harness_plugin.rs`](harness_plugin.rs)); the code is the authority, this is
the narrative.

V40 Phase G moved it here from `docs/MAINTENANCE.md`'s drift table and the
per-harness paragraphs scattered through `docs/ARCHITECTURE.md` and
`docs/CHP.md`, because none of it is true of harnesses in general. Those
documents now link here.

**Kept in step by tests, not by discipline.**
`harness::contract::tests::matrix_matches_maintenance_doc` reads this file
(and `../claude/README.md`, and `docs/MAINTENANCE.md`) and asserts that every
capability the registry declares for **this** harness has exactly one row
below — and that no row here names a capability the registry does not have, or
one that belongs to another harness.

---

## Drift watch — capability rows

**OpenCode is a user-installed, auto-updating CLI that cImp does not pin.**
Re-run this checklist periodically and after any noticeable OpenCode update
(`opencode --version`).

**Capability id(s)** are the join keys into
[`harness/contract.rs`](../contract.rs).

| Capability id(s) | Feature | Contract it depends on | Where wired | Symptom if the contract drifts |
|---|---|---|---|---|
| `opencode.plugin.load_all` | OpenCode injection + memory (V10) | `chat.message` plugin hook + `tool.execute.after`; `OPENCODE_CONFIG_CONTENT` env. **Plugin API re-verified byte-identical at 1.18.18 on 2026-08-17** — discovery, ESM loading, `OPENCODE_PURE` and every hook signature cImp uses. One thing found while looking: the published Hooks type declares `permission.ask` and nothing upstream ever fires it, so no control may be built on it (a handler there would read like a permission gate and never run once). The generated plugin talks only to cImp's own loopback, never to OpenCode's HTTP server, so it needs no server credential | generated `.opencode/plugin` | OpenCode sessions stop appearing in Memory; no injection for OpenCode tabs |
| `opencode.tool_registry` | OpenCode native tool registry (V32 Phase H) — **allowlist drift watch** | `harness::opencode::tools::OPENCODE_NATIVE_TABLE` classifies OpenCode's OWN tool ids for the Phase H taint gate, and is deliberately **allowlist-only**: a name absent from it is UNGATED. (Unknown⇒EXTERNAL, the locked rule for cImp's routed vocabulary, is wrong for a harness registry — it would gate `todowrite` as external content.) The consequence is a maintenance obligation: a NEW OpenCode file/shell/web tool ships ungated and nothing fails loudly. **Each maintenance run (and after any visible OpenCode update): re-run `opencode serve` + `GET /experimental/tool/ids` and diff against the table; classify any new id (or record why it carries no capability).** `apply_patch` is the standing example of why — it replaces `edit`/`write` on OpenAI-provider models, so a list naming only `edit`/`write` leaves that whole mutation surface open. **Re-verified 2026-08-17** against the installed 1.18.13 and diffed against 1.18.18: the same 14 live ids, no drift. Two follow-ups from that pass, both about ids the probe structurally *cannot* see because they are registered only behind experiment env flags: (a) `execute` (`OPENCODE_EXPERIMENTAL_CODE_MODE`, runs arbitrary code ⇒ gated exactly like `bash`, mutating) and `lsp` (`OPENCODE_EXPERIMENTAL_LSP_TOOL`, reads project data ⇒ gated, non-mutating) are now in the table, and `plan_exit` (`OPENCODE_EXPERIMENTAL_PLAN_MODE`) is a recorded reviewed-ungated decision — a probe run against a default serve can say nothing about any of them, so `harness/opencode/tools.rs` carries its own test that all three stay classified; (b) **2.0 watch item:** upstream pins the id `bash` with a comment saying it will be RENAMED at opencode 2.0, so expect the probe to report `bash` as declared-but-not-served (a note) *and* the new name as UNCLASSIFIED (a failure) in the same run — classify the new id and keep `bash`. | `harness/opencode/tools.rs::OPENCODE_NATIVE_TABLE`; generated plugin rendered by `harness/opencode/plugin.rs::opencode_plugin_source` from the template `harness/opencode/templates/plugin.js` (V35 Phase M — the emitted sets are goldened under `src-tauri/fixtures/harness/opencode/goldens/`, so a classification change shows up as a reviewable `.js` diff); inventory in `docs/HARNESS-NATIVE-TOOLS.md` §3 | Silent: the gate simply never fires for the new tool. Only the diff above detects it — which is why this row exists. |
| `opencode.sse.events`, `opencode.route.push`, `opencode.route.noauth` | OpenCode OOB tap + push (V24/V30) — **the auth watch CLOSED on 2026-08-17 (Tier D → B)** | **The server is authenticated now, and that is the whole change.** cImp generates a fresh 32-hex password per OpenCode tab spawn, sets the documented `OPENCODE_SERVER_PASSWORD` + `OPENCODE_SERVER_USERNAME` pair on the child, and presents `Authorization: Basic base64("opencode:<password>")` on the SSE tap, the session probe and the push POST. Live-spiked 2026-08-17 on the installed 1.18.13 (diffed against 1.18.18, byte-identical upstream): unauth ⇒ 401 on every route including `GET /event`, Basic ⇒ 200/SSE. So the old double edge is gone in both directions — a release adding auth no longer breaks the tap, and the localhost exposure (`POST /session/:id/message` *without* `noReply` starts a real agent turn, reachable by any local process and plausibly by a browser via DNS rebinding) is closed for every tab cImp launches. **Three upstream footguns the implementation is shaped around:** the password is snapshotted at module load in the child, so it must be set AT SPAWN; an EMPTY password silently disables auth entirely; and a present-but-wrong `auth_token` query parameter WINS over a correct header, so cImp sends the header alone. First-party clients (the TUI, `opencode run`, the plugin's SDK client) self-authenticate from the same env, so the password does not break the tab. The SSE contract itself is re-verified unchanged at 1.18.18 with one movement: `session.idle` is **deprecated upstream and still emitted**, beside its replacement `session.status` (`properties.status.type` = `busy`/`idle`) — the reader honours both, and a second arrival for one turn-over is a no-op. **Each maintenance run:** run `cimp --harness-canary` (the probe spawns `opencode serve` WITH a password and asserts unauth ⇒ 401 *and* authenticated ⇒ accepted, both directions), and check release notes for changes to the Basic-auth scheme or to the turn-over events. | `harness/opencode/config.rs` (`new_server_password`, `server_auth_env`, `server_basic_auth`), `harness/reader.rs` (the spec field that carries the credential), `harness/opencode/read.rs` (`auth_headers`, `consume`, `forward_push`, `verify_main_session`, `Tracker::close_turn`), `tabs/config.rs` (`compose_ai_env`, `resolve_oob_source`) | Tap/push requests start failing 401/403 ⇒ the scheme moved: rewire `server_basic_auth`. Usage "live now" + V30 OpenCode fanout go dark until then (visible-off, and the probe FAILS rather than transitioning). The other direction is a security failure and is scored as one: if unauthenticated calls are served despite the password, the probe fails and names the route. If BOTH turn-over events stop being understood, an OpenCode tab goes mute mid-turn and the avatar stays in Thinking |
| `opencode.input.profile` | How a turn is TYPED into this harness's TUI (V39, the push half of locked decision 16) | Same contract as `claude.input.profile`: a bracketed paste (`ESC [ 200 ~` ... `ESC [ 201 ~`) is **one literal insertion** and a CR after a short settle submits **exactly one turn**. This TUI also enables bracketed paste (private mode 2004). The settle window and paste bound in `harness/opencode/input.rs` are **floors chosen from the failure they prevent, not measurements** — this harness settles faster than Claude's, which is a declared value, not a shared constant. **Spike (input-profile), outcome in `Settings.harness[<id>].input_profile_status`**, read by the `delegation.worker` gate — fail-closed on anything unrecognized | `harness/opencode/input.rs` (the values), `harness/plugin.rs` (`InputProfile` / `PasteMode` and the `input_profile()` trait method), consumed by `delegation/engine.rs` | **Silent if it drifts and the spike is not re-run** — a split paste sends the worker a truncated question it answers perfectly. Fails closed on a recorded `"fail"`. **Each maintenance run (and after any visible OpenCode update): re-run V39 live-verify 1 and 2** and record the outcome |

**How to check (~5 min):** open an OpenCode tab and confirm a session shows up
under Memory, that the Events lane carries its SSE rows, and that the generated
`.opencode/plugin/cimp-inject-<tab>.js` matches the goldens
([`fixtures/harness/opencode/goldens/`](../../../fixtures/harness/opencode/goldens)).
Any drift: re-run the spike recipes below before trusting the feature again.

## Version pins

| What | Pinned at | Verified through | Why it matters |
|---|---|---|---|
| Plugin API (`chat.message`, `tool.execute.*`, `event`) | **1.18.13** | **1.18.18** (2026-08-17, re-verified byte-identical) | The generated plugin is the whole injection + memory + gate path. |
| `noReply` injection behaviour | **≥ 1.18.13** | 1.18.18 | A *minimum-version* fact — the session-push fanout (V30) is only safe from that release. |
| SSE `/event` shapes | **1.18.13** | 1.18.13 | The captured fixture under [`fixtures/harness/opencode/1.18.13/`](../../../fixtures/harness/opencode/1.18.13) is what the canary asserts against. |

`harness_versions` in the global `settings.json` records what was last **seen**
and last **verified** *per harness* (V40 Phase B moved the pairs into
`Settings::harness`), and the drift advisor's `version_signature` is per
harness — so this harness now gets every version-keyed drift rule, which before
V40 only Claude had a path for.

## Hook routing

**This harness has no hook mechanism.** Its equivalent is a generated plugin
module, written to `.opencode/plugin/cimp-inject-<tab>.js` at tab launch by
`HarnessPlugin::write_artifacts()` and rendered from the templates in
[`templates/`](templates). The plugin POSTs into the same loopback routes the
Claude hooks do, which is why the ingress seam is neutral: core registers
whatever routes a plugin declares through `HarnessPlugin::routes()` and writes
back the `HookReply` it returns.

Its own inbound routes — `/memory/event` (per-turn tokens and tool events) and
the push routes — are this plugin's declarations, not core's.

**Never launch OpenCode with `--pure`**: it suppresses plugin loading, and every
capability below rides the plugin.

**`OPENCODE_CONFIG_CONTENT`** carries the MCP wiring (`mcp.cimp-offload`), the
sub-agent depth pin and the permission block; this harness understands neither
`--append-system-prompt` nor `--settings`, so its pre-args stay empty and its
model-visible text arrives through the config instead. That asymmetry is
declared (`HarnessPlugin::pre_args()` / `extra_args()`), not branched on.

## CHP — event → route table

This harness speaks CHP directly: its generated plugin sends a CHP envelope in
the **body**, so it implements no `identity_of_request()` override and needs no
`X-CIMP-*` header scheme. The events it serves and the ones it declares it
`cannot` serve are in its `SessionStart`-equivalent hello, computed from the
booleans that decided what this tab's plugin actually wired.

The neutral protocol — envelope, versioning, the event vocabulary — is
`docs/CHP.md`. `harness::chp::EVENTS` is the authority for the event list.

## CHP — hello and identity

**Why this harness's hello omits `harness_version`.** OpenCode does not expose
its own version to a plugin at module-evaluation scope, and cImp will not bake
in a number and let the plugin report it back as though the harness had said
it — that would be cImp attesting to itself. cImp learns the version from
`GET /global/health` during a probe run instead (V35 Phase H).

**Session identity lives in its own key space.** This plugin declares
`session_key_space() == Session`: live sessions are keyed by the session id
OpenCode reports over the loopback, never by a cImp tab id. Since V40 Phase D
the live-session registry holds the declared key space per harness, so a
`/memory/event` session id is accepted into the session space **even when it
collides with a tab id** — the two are different spaces and the collision is
unrepresentable rather than guarded against.

**Sub-agent sessions** are excluded from tab binding via `session.created`'s
`info.parentID`. If OpenCode stops announcing children that way, a tab can bind
to a sub-agent session mid-run (scope narrows; still isolated per tab, still
never an error).

## Usage tap — residual limitations

*Architecture: see `docs/ARCHITECTURE.md` § Workflow & Visibility (V14).*

- **OpenCode usage is estimate-only on the SSE path.** The `/event` SSE
  `message.updated.properties.info` carries only `{id, role, time}` on the
  pinned version — no token fields — so sessions read through the SSE tap alone
  are recorded `est_only` from tool-call *input* args. **Revisit if a future
  OpenCode release adds real token fields to `message.updated`**;
  [`read.rs`](read.rs)'s doc comment names the exact field path to re-check.
  (V35 Phase L read `@opencode-ai/plugin`'s own `Hooks` types and found the
  plugin's `event` hook already receives `info.tokens` on `message.updated` —
  which is where OpenCode usage actually comes from, via `/memory/event`. What
  is still missing is a tool-RESULT consumer: `tool.execute.after`'s *second*
  parameter carries `{title, output, metadata}` and the generated handler takes
  only the first, so the result text is one parameter away.)
- **This harness declares no quota or context source.** `usage_source()` is
  `None`, so `harness_usage("opencode")` answers *no usage source* rather than
  a harness sitting at 0%. That is the intended reading: absence, not zero.
- **`opencode serve` is a Bun binary that forks children** (observed: two
  grandchildren), which is why this plugin declares `needs_tree_reap()`. The
  tree-kill primitive itself is neutral; only the requirement is this harness's.

## Memory scoping

*Architecture: see `docs/ARCHITECTURE.md` § Code Intelligence — Context Engine
(V10), "Memory-tool session scoping".*

The `context_recall` / `context_note` / `context_notes` MCP tools resolve a
session scoped to the calling harness **and** to the calling **tab** (V28,
issue #13). This harness stamps the tab → session registry from the `/event`
SSE tap ([`read.rs`](read.rs)'s `Tracker::track_live_session`, keyed off
`properties.sessionID`). If the event shape changes, resolution silently
degrades to the pre-V28 recency behavior — **no error, no log**. The tell is
per-tab isolation quietly stopping; verify with the two-tab recipe (a
`context_note` in tab A must not appear in tab B's `context_recall`).

**Fail-open is deliberate and total:** missing `--tab`, unknown key, TTL-stale
entry, blank value → the harness-scoped current session. Never turn any of these
into a tool error.

## Open spikes & unverified contracts

| Spike | What it verifies | Status | Where recorded |
|---|---|---|---|
| **OpenCode veto** (V16 Feature 7 gate) | Whether a `tool.execute.before` handler in the generated plugin can veto a read **and** get the thrown message to the model. | **open** — gates whether an OpenCode read advisor is implementable at all | Recipe below; pass ⇒ implement per the V16 spec, fail ⇒ record Claude-only as permanent-until-upstream-changes here |
| **C3** (SSE usage fields) | Whether `/event` SSE `message.updated` carries token/usage fields. | **resolved — absent** on the pinned build; SSE-path usage stays `est_only` | [`read.rs`](read.rs)'s module doc; re-check on OpenCode releases |
| **Plugin loading is directory-wide** | OpenCode loads **every** file in `.opencode/plugin/` into every session in that directory. The per-tab `cimp-inject-<tab>.js` scheme is only safe while that stays true *and* `CIMP_TAB_ID` stays process-wide. | **observed, undocumented** | V32 milestone doc; the goldens are the reviewable diff |
| **Input profile** | That a bracketed paste plus a settle plus CR yields exactly ONE turn in this TUI. | **unverified, manual only** — no fixture and no probe can settle a behaviour visible only as a real turn in a real TUI; declared in `declared_unprobed()` | `Settings.harness[<id>].input_profile_status`, read by the `delegation.worker` gate |
| **Version tripwire** | That the currently-installed build still honours every contract above. | **re-armed on every version change** — `drift.harness_version.v1` fires until re-verified, and since V40 the signature is **per harness**, so this harness has the rule at all | The Harness health panel's **Mark verified** action, which takes the harness from the row clicked |

**Spike recipes:**

- **OpenCode veto (V16 Feature 7 gate, still open).** In a scratch project, add
  a `tool.execute.before` handler to the generated
  `.opencode/plugin/cimp-inject-<tab>.js` that throws for a known file's read
  and observe whether (a) the read is vetoed and (b) the thrown message reaches
  the model. Pass ⇒ implement the OpenCode read advisor per the V16 spec;
  fail ⇒ record Claude-only as permanent-until-upstream-changes here.
- **Input profile.** Delegate a two-line task into an OpenCode worker tab and
  confirm in the worker's own transcript that it arrived as ONE turn, verbatim
  (V39 live-verify 1 and 2).

## Native tools

This harness's native tool vocabulary — the lowercase `read` / `edit` /
`write` / `patch` / `grep` / `glob` / `list` / `bash` / `webfetch` /
`websearch` family — is [`tools.rs`](tools.rs), returned from
`HarnessPlugin::native_tools()`. It is a **security allowlist**, not just a
naming table: the V32 Phase H gate classifies this harness's own tool ids
against it, and an id cImp does not recognise is treated as mutating rather
than resolved against another harness's vocabulary.

The model-visible guidance a session receives names *these* names — since V40
Phase E `GRAPH_GUIDANCE` is templated through
`HarnessPlugin::tool_for_role()`, so an OpenCode session is told `read` and
`bash` where a Claude session is told `Read` and `Bash`.

`docs/HARNESS-NATIVE-TOOLS.md` is the user-facing twin of that table and is
compared against it by a test, so the two cannot drift.

## Input profile

[`input.rs`](input.rs) holds this harness's `InputProfile` — paste encoding,
submit bytes, settle window and paste bound — returned from
`HarnessPlugin::input_profile()`. The type is neutral
([`../plugin.rs`](../plugin.rs)); only the values are this harness's, and they
are **floors chosen from the failure they prevent, not measurements** (see the
input-profile spike above).
