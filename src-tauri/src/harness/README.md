# `harness/` — the entire harness surface

**Everything cImp knows about a CLI it does not control lives in this
directory.** If you are adding a harness, or absorbing a change in one, this is
the only place you should have to edit.

That is a claim with tests behind it, not a convention: `layering.rs` fails the
build when a harness-owned string appears outside this tree, when a module in
here reaches up into a capability, or when a `harness/<id>/` directory has no
rows in the registry and no CHP hello.

## The layers

```
  L4  Capabilities        graph/ · tts/ · usage/ · workbench/ · offload/
                          ── speak ONLY cImp domain types + contract::gate(id)
                                        ▲   never imported from in here
  L3  Session bus         the registry (contract.rs) and every degradation
                          decision — harness-agnostic, no harness literals
  ══  L2  CHP ═══════════ chp.rs + docs/CHP.md ═══ THE STABLE SEAM ══════════
  L1  Harness plugin      claude/ · opencode/ — THE ONLY PER-HARNESS ARTIFACT
  L0  Harness             Claude Code · OpenCode. Uncontrolled, self-updating.
```

## What is where

| File | Layer | What it is |
|---|---|---|
| `contract.rs` | L3 | **The capability registry.** Every dependency cImp has on a harness, ranked by seam (A–D), with what it depends on, what breaks, and how it degrades. The authority; `docs/MAINTENANCE.md`'s drift table is checked against it. |
| `chp.rs` | L2 | **CHP** — protocol version, the event vocabulary, the `/session/hello` handshake, stale-artifact detection. Wire contract: `docs/CHP.md`. |
| `ingress.rs` | L2 | The registry-wide lookups over the harnesses' own loopback routes: who serves a path, where a request's identity lives, the drift-token key space, and the pre-tool reply budget derived from what the plugins declare. |
| `_retired/` | — | Data a retired harness left behind that a live code path still compares against — permission-pattern rows for pristine-file reconciliation, and nothing else. No plugin, no descriptor, no registry row. |
| `canary.rs` | — | L1 canaries: recorded fixtures still produce **substantive** output (not merely parse). |
| `probe.rs` | — | L2 live probes: the recorded shape is still real, driven against the installed CLI. |
| `verify.rs` | — | Joins the two when the installed version changes, and advances `claude_last_verified` on its own. |
| `health.rs` | — | Read-model for the Settings *Harness health* panel. |
| `capture.rs` | — | The payload corpus — what a probe saw, scrubbed and stamped, so a break starts with a diff. |
| `reader.rs` | L1 | Which fallback reader a tab attaches, and the context it runs with. Retired from the hot path by Phase L. |
| `render.rs` | L1 | `{{cimp.*}}` substitution for the generated artifacts, and the one place values get JSON-quoted. Phase M. |
| `layering.rs` | — | The three tests that keep all of the above true. |
| `claude/`, `opencode/` | L1 | One directory per harness — see below. |

## Adding a harness

A new harness is one directory, one registry row and no changes above L2 --
plus the two things that are generated FROM the row (a frontend mirror and
a doc section) and the one thing that cannot be: the README beside your code.
The list below is what a plugin actually costs, with the test that enforces
each claim.

1. **`harness/<id>/mod.rs`** — and add `pub mod <id>;` here.
1a. **A `HarnessDescriptor` row in `registry.rs`** (V40 Phase A) — id, label,
   binaries, reserved tab ids in canonical order, consumer token, `expects_chp`,
   the environment markers to strip, the features core mounts for you, and a
   `&'static dyn HarnessPlugin`. This is what every "which harness is this?"
   question in the tree now resolves against, so a harness without a row is not
   misclassified, it is simply not a harness. `every_registry_entry_is_fully_wired`
   checks the row against everything below.
1b. **`impl HarnessPlugin`** in your directory — the code half. Every method has
   a harness-neutral default, so implement only what is true of your harness;
   what you do not declare, you do not get (`input_profile() == None` means your
   tabs are not delegation workers, and that is a visible refusal rather than a
   task typed into a TUI cImp cannot drive).
2. **The generated artifact** — whatever this harness's own extension mechanism
   is: `claude/overlay.rs` emits a `--settings` JSON overlay,
   `opencode/plugin.rs` emits an ES module, `opencode/config.rs` emits an env
   var.

   **Text artifact ⇒ a real file with `{{cimp.*}}` slots** under
   `<id>/templates/`, `include_str!`ed and rendered by `render.rs` (V35 Phase M).
   `opencode/templates/plugin.js` is the worked example: the Rust keeps the key
   set and the values, the JavaScript keeps the JavaScript. Values are whole
   serde literals (`render::json_lit`) so nothing a contributor adds to a table
   can malform the artifact, and the key set is checked both ways at test time —
   a typo'd `{{key}}` fails `cargo test` instead of shipping. **Structured
   artifact (JSON/TOML) ⇒ build it structurally**, as `claude/overlay.rs` does
   with `serde_json::Value`; converting one of those to a text template would be
   a regression, not a cleanup.

   All are computed in Rust at tab spawn and are **spawn-baked**: the
   artifact outlives the binary that wrote it, so it must carry `chp`
   (`chp::CHP_VERSION`) on every body it posts, and any Settings-derived value
   baked into it needs a `tabs::config::spawn_inject_sig` entry so the user gets
   the restart hint.
3. **A hello** — `serves` / `cannot`, built from the *same* flags that decided
   what was emitted, so the declaration cannot claim something the artifact does
   not do. Use `chp::EV_*` ids. A capability absent from `serves` is
   *unavailable, with a reason*, never *nobody wrote it down*.
4. **Capability rows** for anything this harness serves that no row covers yet,
   with `wired_in` pointing at your files. A row whose contract is a sentence
   about *your product* ("this TUI accepts a bracketed paste as one literal
   insertion") belongs in your directory and is returned by
   `HarnessPlugin::capabilities()`; only rows stated about a *tab* or about
   cImp's own seam stay in `contract.rs`. `contract::capabilities()` is the
   joined view every consumer reads, and `wired_in_paths_exist` will hold you to
   the paths.
5. **`native_tools()`** — the tools your harness serves ITSELF (`harness/<id>/tools.rs`),
   one `NativeTool` row per name: its class, whether it can rewrite the tree, and
   what memory event it is recorded as. cImp never routes these, so nothing can
   block them, but three things need them, and **omitting a name is not
   neutral**: `harness::native::mutates_fs` fails CLOSED, so an undeclared tool
   is treated as mutating and takes a checkpoint before every call.
6. **`canaries()`** — a committed fixture plus an assertion per capability whose
   reader could rot silently (`harness/<id>/canary.rs`). They run every
   `cargo test` AND in the shipped binary when your CLI's version changes. Each
   needs a negative twin: the same fixture with one load-bearing field renamed,
   proving the positive one is load-bearing. `_synthetic/` fixtures carry a
   `MANIFEST.toml` like every other.
7. **`probe()` / `declared_unprobed()`** — what can be driven against the
   *installed* CLI (`harness/<id>/probe.rs`), and what cannot, each with the
   reason. The runner in `probe.rs` iterates the registry; it spawns nothing
   itself, so a spawn you add there needs a `spawn_ledger::LEDGER` row.
8. **`settings_schema()`** — the settings only YOUR harness has
   (`harness/<id>/settings.rs`), one `SettingField` per key: kind, label, hint,
   default, and whether it is `spawn_baked` or a `secret`. Core stores them
   opaquely in `Settings::harness[<id>].ext`, type-checks the declared keys at
   the parse boundary, renders the section from this table with one generic
   form, and never names a key. Three consequences, all of them the point:
   an absent key reads its declared default (so **no migration**), a
   `spawn_baked` row is folded into your spawn signature automatically (so the
   flag and its restart hint cannot disagree), and a `secret` row is redacted in
   `Debug` without a hand-rolled impl. Declare nothing and you get an empty
   section and no work anywhere. If a feature of yours is one of the
   *injection* hierarchy's — the mechanism lives inside the artifact you emit —
   name it in `scoped_features()` with the `ext` key holding its app-wide L2,
   and core will stop offering it to every other harness.
9. **`routes()`** — only if your harness posts something that is not a CHP body
   (`harness/<id>/hook.rs`). Each entry is `(method, path, handler)`; core's
   router appends them after every CHP-neutral arm, so you cannot shadow
   `/session/hello` or `/mcp/*`, and it writes back your `HookReply` — a status
   and a body it does not read — so your reply envelope stays yours. Declare
   `identity_of_request()` if your payload has no room for a CHP envelope and
   the `(agent, tab, chp)` triple rides headers instead, `drift_vocabulary()`
   for the ledger tokens your routes report payload drift under,
   `chp_event_for_route()` / `drift_token_for_capability()` so the quiet
   detector can speak about capabilities rather than transports, and
   `hook_reply_timeout()` if a caller of yours waits on the reply — core takes
   `min(all declared) − margin` as the budget it may spend before answering
   (`harness::ingress::hook_reply_budget`).
10. **`permission_patterns()`** — your TUI's prompt grammar
   (`harness/<id>/prompts.rs`): the substrings that mean "a permission prompt is
   on screen", "a question menu is on screen", "the harness is working". The
   detector engine is core and neutral; what it matches on is a transcription of
   your terminal chrome, so it lives with you, along with the capture recipe and
   the reasoning for each marker. `legacy_permission_patterns(era)` is the same
   rows as an earlier release shipped them, for pristine-file reconciliation —
   append-only, and the one thing a retired harness leaves behind
   (`harness/_retired/`).
11. **A fallback reader** (`<id>/read.rs`) only if the harness cannot push. Tier C
   stays possible; it is now contained and declared rather than ambient. If you
   write one — or a canary or probe that speaks its types — it will need L4
   types; add the file to `layering.rs`'s `UPWARD_EXEMPT` with the reason.

12. **`input_profile()`** — only if a tab of yours may be driven as a
   **delegation worker** (`harness/<id>/input.rs`). Four values: how a task is
   pasted (`PasteMode`), the bytes that submit it, how long to settle before
   submitting, and the largest paste to attempt. The type is neutral
   (`plugin.rs`); the values are yours, and none of them is a measurement — each
   is a floor chosen from the failure it prevents. Declaring it is not free: it
   also needs an `<id>.input.profile` capability row, a `declared_unprobed()`
   entry saying why no probe can settle it (nothing but a real turn in a real
   TUI can), and the recorded spike outcome in
   `Settings::harness[<id>].input_profile_status` that the neutral worker gate
   reads. Declare nothing and your tabs are simply not workers — a visible
   refusal at preflight, never a task typed into a TUI cImp cannot read back.
13. **`instructions()` and `tool_for_role()`** — the text a model reads.
   `harness/instructions.rs` owns the *prose* (its subject is cImp: cImp's graph
   tools, cImp's channel, cImp's delegation contract), and you own the
   **vocabulary it is rendered in**: `tool_for_role(Read)` and
   `tool_for_role(Shell)` name your own tools, and the descriptor names your
   label. Declare neither and every slot renders with descriptions ("a full file
   read", "the shell") instead of another product's tool ids — which is the
   honest answer, and strictly better than what core did before V40.
   `every_harness_declares_every_slot` refuses a plugin that overrode
   `instructions()` and dropped a row.
14. **`preflight()`, `needs_tree_reap()`, `emits_startup_chrome()`,
   `session_selector_flags()`, `accepts_passthrough_argv()`** — the CLI facts
   core used to branch on. Whether your binary must be resolvable before a tab
   of yours can be enabled (and the install hint the refusal carries), whether
   your process forks children a plain kill would orphan, whether a fresh tab
   prints a banner the notification layer must not ring for, which of your flags
   select a session (so cImp does not double-pin one), and whether unrecognised
   `cimp` argv is forwarded to you at all. Each is a declaration with a default;
   none of them is a branch anywhere else.
15. **`spawn_sites()` and `config_writer()`** — anything that spawns or writes.
   `spawn_ledger`'s tripwire scans every `.rs` under `src/`, so a spawn inside
   your directory must be described by a row; the row stays in core and the
   *strings* come from `spawn_sites()`. `config_writer()` is for a file or
   env-var block cImp writes on your behalf into the user's own configuration —
   OpenCode's local-provider block is the worked example.
16. **`affordances` on the descriptor** — what the *frontend* needs to say about
   you without knowing what you are: your new-session command (the "run
   `/clear`" strings), how your tool list refreshes, your web tools, your state
   directories, an install hint and a docs URL, your attachment format, your
   local-provider env preview, how many rows your status line occupies, and the
   attribution template a delegated turn is banner-stamped with. Every one of
   these was prose in a Svelte file before V40.
17. **`harness/<id>/README.md`** — your drift rows, version pins, ingress
   routing, open spikes and their recipes. This is the human twin of your
   capability rows and `matrix_matches_maintenance_doc` pairs the two: every row
   the registry declares for you appears in exactly one row of YOUR README, and
   a row there naming another harness's capability fails. A section in
   `docs/HARNESS-NATIVE-TOOLS.md` reproducing your `native_tools()` is checked
   the same way. Neutral rows — a contract stated about a *tab* rather than
   about a product — stay in `docs/MAINTENANCE.md`.

What you must **not** need: a new enum variant outside `harness/`, a new match
arm in `tabs/config.rs`, a bespoke gate constant. What you **do** need and the
pre-V40 version of this list wrongly denied: a **frontend mirror** — but a
generated one. `harness_list` publishes your descriptor, features and
affordances over IPC, `src/lib/harness.ts` is the only file that mirrors it, and
the committed `fixtures/harness/registry.json` (emitted by `cargo test`) is what
a vitest parity suite checks the TypeScript unions against. A descriptor field
added in Rust with no TS mirror fails `npx vitest`.

Four tests hold the rest of the claim up rather than leaving it to good
intentions:

| Claim | Test |
|---|---|
| Your row reaches every place it must — directory, capability rows, hello, `spawn_sig` slot, sandbox grants, health panel, plugin README, goldens | `harness::layering::tests::every_registry_entry_is_fully_wired` |
| Nothing outside `harness/` spells a harness id, or branches on a `HarnessId` | `harness::layering::tests::no_harness_identity_outside_registry` (allowlisted files carry a reason; the survivors are persisted wire forms and one word collision) |
| The same rule on the frontend | `src/lib/harnessIdentity.test.ts` |
| Prose cannot drift from the registry | `harness::contract::tests::matrix_matches_maintenance_doc`, `harness::native::tests::the_native_tools_doc_matches_the_declared_tables` |

If you find yourself writing a branch, the seam is in the wrong place — say so
rather than adding it.

There is **no third-party plugin loading** (design D7). Adding a harness means
adding a directory here and opening a PR. The reason is in
`docs/DESIGN-harness-plugin-architecture.md` § 5: the plugin is inside the TCB —
cImp only *computes* the V32 Phase H verdict, and the enforcement is a `throw`
inside the plugin's own tool path. A plugin that omits it disables containment
while looking completely functional, and no cImp-side test can catch that.

## Before you edit `opencode/templates/plugin.js`, `opencode/plugin.rs` or `opencode/tools.rs`

Those are **security controls**, not data pipes: the native-tool gate, the taint
beacon and the pre-mutation checkpoint all execute inside the generated plugin.
The registry marks them (`Capability::controls`). Treat a change there as a TCB
change.

Since Phase M the enforcement is a `throw` in a `.js` file you can open and
read, and every edit to it shows up twice — once in the template, once in the
byte-identical goldens under `src-tauri/fixtures/harness/opencode/goldens/`.
Re-blessing a golden without reading its diff defeats the entire arrangement.

## Reading list

- `docs/HARNESS-PLUGIN-LAYER.md` — **the long-form twin of this file**: the four
  layers as modules, the tier model, CHP in one page, the registry's row anatomy
  and enforcing tests, both shipped plugins in detail, and the two developer
  guides (adding a harness, changing an existing plugin).
- `docs/MILESTONE-V35-harness-resilience.md` — the why, and the locked decisions.
- `docs/DESIGN-harness-plugin-architecture.md` — the four layers, D1–D7, the
  target tree (§ 4) and these tests (§ 4.1).
- `docs/DESIGN-harness-capability-matrix.md` — the tier ladder and the seed rows.
- `docs/DESIGN-harness-drift-canaries.md` — why a canary asserts substantiveness.
- `docs/CHP.md` — the wire contract.
- `docs/ARCHITECTURE.md` § *Adding a harness plugin* — the same how-to, beside
  its siblings.
