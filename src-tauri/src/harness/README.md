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
| `chp.rs` | L2 | **CHP** — protocol version, the event vocabulary, the `/session/hello` handshake, stale-artifact detection. Wire contract: `docs/CHP.md`. Route handlers live in `offload/loopback.rs`. |
| `canary.rs` | — | L1 canaries: recorded fixtures still produce **substantive** output (not merely parse). |
| `probe.rs` | — | L2 live probes: the recorded shape is still real, driven against the installed CLI. |
| `verify.rs` | — | Joins the two when the installed version changes, and advances `claude_last_verified` on its own. |
| `health.rs` | — | Read-model for the Settings *Harness health* panel. |
| `capture.rs` | — | The payload corpus — what a probe saw, scrubbed and stamped, so a break starts with a diff. |
| `reader.rs` | L1 | Which fallback reader a tab attaches, and the context it runs with. Retired from the hot path by Phase L. |
| `layering.rs` | — | The three tests that keep all of the above true. |
| `claude/`, `opencode/` | L1 | One directory per harness — see below. |

## Adding a harness

A new harness is one directory and no changes above L2.

1. **`harness/<id>/mod.rs`** — and add `pub mod <id>;` here.
2. **The generated artifact** — whatever this harness's own extension mechanism
   is: `claude/overlay.rs` emits a `--settings` JSON overlay,
   `opencode/plugin.rs` emits an ES module, `opencode/config.rs` emits an env
   var. All three are computed in Rust at tab spawn and are **spawn-baked**: the
   artifact outlives the binary that wrote it, so it must carry `chp`
   (`chp::CHP_VERSION`) on every body it posts, and any Settings-derived value
   baked into it needs a `tabs::config::spawn_inject_sig` entry so the user gets
   the restart hint.
3. **A hello** — `serves` / `cannot`, built from the *same* flags that decided
   what was emitted, so the declaration cannot claim something the artifact does
   not do. Use `chp::EV_*` ids. A capability absent from `serves` is
   *unavailable, with a reason*, never *nobody wrote it down*.
4. **Registry rows** in `contract.rs` for anything this harness serves that no
   row covers yet, with `wired_in` pointing at your files. `wired_in_paths_exist`
   will hold you to it.
5. **A fallback reader** (`<id>/read.rs`) only if the harness cannot push. Tier C
   stays possible; it is now contained and declared rather than ambient. If you
   write one, it will need L4 types — add it to `layering.rs`'s `UPWARD_EXEMPT`
   with the reason.

What you must **not** need: a new enum variant outside `harness/`, a new match
arm in `tabs/config.rs`, a bespoke gate constant, a frontend mirror. If you find
yourself writing one, the seam is in the wrong place — say so rather than
adding it.

There is **no third-party plugin loading** (design D7). Adding a harness means
adding a directory here and opening a PR. The reason is in
`docs/DESIGN-harness-plugin-architecture.md` § 5: the plugin is inside the TCB —
cImp only *computes* the V32 Phase H verdict, and the enforcement is a `throw`
inside the plugin's own tool path. A plugin that omits it disables containment
while looking completely functional, and no cImp-side test can catch that.

## Before you edit `opencode/plugin.rs` or `opencode/tools.rs`

Those are **security controls**, not data pipes: the native-tool gate, the taint
beacon and the pre-mutation checkpoint all execute inside the generated plugin.
The registry marks them (`Capability::controls`). Treat a change there as a TCB
change.

## Reading list

- `docs/MILESTONE-V35-harness-resilience.md` — the why, and the locked decisions.
- `docs/DESIGN-harness-plugin-architecture.md` — the four layers, D1–D7, the
  target tree (§ 4) and these tests (§ 4.1).
- `docs/DESIGN-harness-capability-matrix.md` — the tier ladder and the seed rows.
- `docs/DESIGN-harness-drift-canaries.md` — why a canary asserts substantiveness.
- `docs/CHP.md` — the wire contract.
- `docs/ARCHITECTURE.md` § *Adding a harness plugin* — the same how-to, beside
  its siblings.
