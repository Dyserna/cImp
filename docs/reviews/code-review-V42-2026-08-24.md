# Code review — V42 (`develop..f30b0cf`), 2026-08-24

Adversarial review at **high** effort over the V42 refactoring milestone's
diff: Phase C (per-project UI state), Phase D (loopback route split, latch
extraction), Phase E (generated settings bindings), plus the CI gate that
landed with them. The five CSS-consolidation commits after `f30b0cf`
(`59bad07`…`8372e3e`) were **out of scope** and are untouched.

**Reviewer output:** 44 raw findings → 31 unique → **27 verified** (23
CONFIRMED, 4 PLAUSIBLE). Four were **refuted** on inspection and are recorded
at the end so they are not re-raised.

**Disposition:** 25 CLOSED in this pass, 2 DEFERRED with an owner. Every CLOSE
carries its commit below. All four gates green at `8c7b786`:

| Gate | Baseline (`8372e3e`) | After |
|---|---|---|
| `cargo test --bin cimp` | 2896 passed · 0 failed · 6 ignored | **2907 · 0 · 6** |
| `npx vitest run` | 887 tests · 45 files | **896 · 45** |
| `npm run check` | 0 errors · 369 files | **0 · 369** |
| `npm run build` | clean | **clean** |

`src/lib/settings/generated/` is unchanged by the pass (the CI bindings diff
is clean), and `Cargo.lock` did not move.

---

## Top 10 — all CLOSED

### RV-1 — the one-time `localStorage` import MOVED machine-wide state into the first project

`uiState.ts:298`. `runOneTimeImport` called `localStorage.removeItem` on each
durable key after the backend confirmed the write. `localStorage` is
per-**machine**; the import marker that stops the import is per-**project**.
So the first checkout launched after upgrading took the machine's only copy of
the durable view state into its own `ui_state.json`, and every other checkout
on that machine then imported nothing — silently, once, unrecoverably.

**Fixed:** the import is a copy-and-leave. The originals stay as seeds so every
project imports the same values losslessly; they are inert, since a project
whose marker is set never reads `localStorage` again, and the ephemeral prefs
that live there by design meant the origin was never going to be emptied
anyway. Module docs rewritten; the test that asserted removal now asserts
retention, plus a two-checkout test and a structural "the import calls
`removeItem` zero times" assertion.

**Commit:** `728d955`

### RV-2 — the 250 ms debounce re-introduced the closing race

`uiState.ts:247`. A toggle followed inside 250 ms by the window closing was
lost unless the `pagehide` flush won, and `pagehide` is not guaranteed on every
teardown path. The synchronous `localStorage.setItem` this replaced had no such
window. `installLayoutPersistence` was cited as precedent in two comments and
is not one: its layout tree is **backend-held** and flushed on close from the
Rust side, so a dropped frontend timer there costs nothing. This data had no
second copy.

**Fixed:** `setUiValue` schedules a `queueMicrotask` flush — same-tick
coalescing only, so one handler is still one IPC call while a per-task splitter
drag sends per task. `pagehide` stays as a belt. The inaccurate comments are
corrected here and in `ipc/ui_state.rs`. Tests use fake timers so a
re-introduced `setTimeout` cannot be papered over by real time passing.

**Commit:** `7f11f70`

### RV-3 — `mount(App)` was gated on an unbounded file read

`main.ts:133`. A stalled network share, a locked file, or a backend wedged
before managed state came up produced a window that `showMainWindowOnce`'s 3 s
net **revealed** and that then never mounted anything into it.

**Fixed:** `hydrateUiState` carries its own ~2 s budget and returns on defaults
with a `console.warn`. The timeout is implemented *inside* the module rather
than as a `Promise.race` at the call site, deliberately: raced from `main.ts`,
the abandoned hydrate would keep running and mutate the cache and write-liveness
under an app that had already painted. The timeout **latches**, and everything
past an `await` checks it. A window that timed out is write-inert, which is
what keeps `main.ts`'s hidden-tab ordering guarantee intact — an empty
hidden-tab set can un-hide tabs for that session, but the popover's write
cannot persist the emptiness back over the user's choice.

**Commit:** `6dc17fe`

### RV-4 — `hydrated = true` was set before the import ran

`uiState.ts:173`. A window whose import then failed was write-**live** over a
cache the import had not finished filling: the next `<details>` toggle would
persist that half-story, and the un-imported values would never be retried,
because a later launch finds the same half-story.

**Fixed:** `runOneTimeImport` returns whether the project is imported, and that
answer is what arms writes. A failed import is read-only for the session;
nothing is deleted, the marker stays absent, the next launch runs the whole
import. Both write-live paths (marker already present, import just committed)
have their own tests.

**Commit:** `728d955`

### RV-5 — two instances on one project root clobber each other

`ipc/ui_state.rs:80`. `merge_ui_state`'s read-modify-write was serialised by a
process-local `Mutex` only, and one project root can legitimately carry two
cImp instances (a second launch in the same directory, a `--statusline` helper,
a dev build beside an installed one). A reads, B reads, A writes, B writes, and
A's keys are gone.

**Fixed:** an exclusive OS advisory lock via `std::fs::File::lock` (flock /
LockFileEx), held from the version probe through the rename. It locks a
**separate** `.cimp/ui_state.lock` file, not the json: the commit is
`write_atomic`, which renames a fresh file over the target, so a lock on the
old file would guard an inode that is about to stop being the state — and on
Windows an open handle on the destination can block the rename outright. Lock
failure degrades to the process-local mutex rather than refusing the write.
Tested by taking the lock on one handle and requiring `try_lock` on a second to
fail (both platforms' primitives conflict per-handle, so one process stands in
for two), then to succeed once the first is dropped.

**No new dependency.** `File::lock` is std since 1.89, so `rust-version` moves
1.88 → 1.89 instead of `fs2`/`fs4` entering the tree for one call. Same
primitive, no supply-chain surface, no license to review. The declared MSRV is
not verified by CI either way (documented gap, `tests.yml` header).

**Commit:** `4f935de`

### RV-6 — no CI job ran `svelte-check`

`tests.yml:148`. The frontend's type layer was verified only when a human
happened to run it. V42 Phase E made that sharp: `settings/types.ts` is
generated from `schema.rs` now, and the bindings gate catches a stale commit —
but a schema change that regenerates **cleanly** and breaks a call site (a
renamed field, a narrowed union, a variant nothing handles) is invisible to
`git diff` and invisible to vitest, which does not type-check.

**Fixed:** `npm run check` runs on the Windows job, mirroring `vitest` and for
the reason the Linux job already states for having no vitest step — both are
Node-only and cannot differ by OS, so a second copy is a second thing to
maintain re-proving one result. `npm run build` already ran on **both** jobs
(tauri-build needs the bundle) and is now documented as the gate it is:
`vite build` runs lightningcss and rejects CSS `svelte-check` accepts, which
caught a real break during this milestone. `docs/MAINTENANCE.md`'s CI-coverage
table is updated to match.

**Commit:** `22c4f3d`

### RV-7 — the route-file join could not see `pub(crate) mod`

`loopback/tests.rs:7695`. `the_source_scanners_read_every_route_file` is the
one test that catches a family file added to `mod.rs` and not to
`ROUTE_SOURCES` — whose handlers would then be scanned by nobody with every
test green. It scraped declarations with `strip_prefix("mod ")`, so a file
declared `pub(crate) mod x;` was invisible. And invisible on **both sides**:
such a file is exactly the one likely to be missing from `ROUTE_SOURCES` too,
so the two shortened lists agree and the join passes on the failure it exists
to detect.

**Fixed:** a `mod_name` helper reads through the visibility modifier, via the
same `past_visibility` extraction `declares` uses, still anchored at column 0
(these scans are about top-level items; the column-0 `}` terminator depends on
it).

**Negative control** — planted, confirmed red, reverted: `mod events;` →
`pub(crate) mod events;` with its `ROUTE_SOURCES` row deleted. Under the old
scrape both lists lose `events.rs` and the test is GREEN; under the new one the
scrape keeps it and the assertion fails naming the missing row. Five permanent
assertions pin the spellings that must be seen, four the shapes that must not
(prose, a nested `mod`, an inline module, a lookalike identifier).

**Commit:** `8d0f1cb`

### RV-8 — the hand-written serde seams were verified by nothing

`types.ts:62`. Phase E deleted the hand-written mirror and the `include_str!`
field-name scans that watched it — correctly; asserting that a generator
generated is ceremony. But four types do not reach TypeScript through the
generator's understanding of them. Their (de)serialize is hand-written, ts-rs
cannot read a wire word off a hand-written impl, and the answer is a
`#[cfg_attr(test, ts(...))]` override that **restates** what the impl does: a
one-line hand-written mirror inside an attribute, of exactly the kind the phase
deleted. After the retirement nothing checked any of them.

**Fixed:** four tests in `settings::codegen`, each running the real `Serialize`
and requiring its bytes to be spelled in the generated TS:

* `Override` — the union must be **exactly** the set of wire words the enum
  emits, and each must round-trip back through the hand-written parse.
* `BackgroundOverride` — `"disabled"` OR a config object, mirrored by a
  `ts(type = …)` spelled at **two** sites (`schema.rs:2044`, `:2188`). Both are
  checked, which is the drift that was invisible; the `Custom` variant's keys
  must be declared by the interface the seam names.
* `NotificationSlot` — the TS must declare the object it *writes* and must not
  widen to a union offering the migration-only bare-string shape (which is
  still accepted on input, asserted here).
* `AiTabId` — the seam must name a real declaration, `tabs/types.ts` must keep
  it a bare string, every registry id must round-trip, and an unclaimed id must
  still be refused.

**Negative control** — planted, confirmed red, reverted: `rename_all` flipped
to `"UPPERCASE"` and one `background_override` seam retyped to `"off"`. Both
tests fail naming the drift (`["INHERIT","OFF","ON"]` vs
`["inherit","off","on"]`). It takes the **second** build, because
`include_str!` reads the committed file — which is the right source (that is
what Vite bundles and what CI checks out), and the note on `GENERATED_TS`
records the one-build latency and why it is not a gap.

**Commit:** `66f5669`

### RV-9 — a presence scan a comment could satisfy

`loopback/tests.rs:106`. `files_containing` searched raw source, so a doc
comment naming the signature satisfied it — and both call sites are security
assertions ("the exec roots derive from the app, never from a request body";
"the tab-identity headers are actually matched"). A scan a comment can satisfy
keeps passing after the code it names is deleted.

**Fixed:** it goes through `crate::rustsrc` now — but **not** `code_of`. One of
the two needles *is* a string literal (a match arm on a header name) and the
strong pass blanks it, which would swap "a comment can satisfy this" for
"nothing can": the same vacuity, opposite face. `rustsrc` gains `uncommented`:
the **same lexer** with literal blanking switched off, which is what keeps a
`//` inside a string from being read as a comment. Its self-check runs the
audited strong pass over the same input, since the "no `"` survives" invariant
is what proves the lexer stayed in sync.

**Negative control** — permanent and synthetic (the pattern sibling tests use):
five cases in `rustsrc::tests`, three in
`files_containing_reads_code_and_not_prose`, including an assertion that the
commented fixture *does* contain the needle as raw text — so the control cannot
pass on nothing.

**Commit:** `8d0f1cb`

### RV-10 — a downgraded build destroyed a future `ui_state.json`

`ipc/ui_state.rs:116`. A `version` this build does not recognise reads as
empty, which is right for a read and destructive for a write: the frontend saw
no import marker, patched one in, and the whole newer file was replaced by
`{version: 1, {marker}}`.

**Fixed:** `merge_ui_state` reads the **raw** version off disk under the lock
and returns an error when it is higher than `UI_STATE_VERSION`. Corrupt is not
future: unparseable, non-object, version-less and non-numeric-version files all
still take the repair path, and an older version is still replaceable (that is
the migration path the stamp exists for). Frontend-side the refusal is
swallowed — the write-failure path logs **once** per window and nothing is
re-queued, so an old build stays perfectly usable on such a project and simply
stops persisting.

**Commits:** `4f935de` (Rust refusal + tests), `7f11f70` (frontend log-once)

---

## Dropped-at-cap — verified real, all CLOSED in the same pass

| # | Finding | Fix | Commit |
|---|---|---|---|
| D-1 | The `cssTokens` guard read `SRC_FILES` only, so the theme sheets that DECLARE the tokens were never asked whether the tokens *they* consume resolve — the defect-D2 shape one layer below where #113 was looking. | One list in both roles. Vacuity guard gains a clause asserting theme sheets actually reached the scan (`walk` swallows a missing directory). Negative control: `var(--rv-probe-undeclared)` in `tui_theme.css` fails naming file, line and token. | `8c7b786` |
| D-2 | `harness/health.rs`'s field tripwire was re-pointed at `concat!(types.ts, generated/settings.ts)` with a plain `.contains` — ~1,900 lines of generated prose behind every needle. `auto_verify` is satisfied by two doc comments about `claude_auto_verify` that would survive a rename of the field. | Hand-written names checked against `types.ts` alone; `auto_verify` against the sliced `HarnessSettings` declaration and by declaration form (`name: `). Two permanent controls. | `f0ecac7` |
| D-3 | The CI bindings gate ran `git diff --exit-code`, which sees TRACKED files only — a generator emitting a NEW file leaves it untracked and the gate stays green. | Both jobs also run `git ls-files --others` over the directory, without `--exclude-standard` so a `.gitignore` cannot hide it, with actionable `::error::` on either half. | `22c4f3d` |
| D-4 | `write_if_changed(&ts_path, &generated)` in `settings::codegen` was dead: `generated` had just been read from that same file, so the comparison always matched. | The read-back becomes the check it stood in for — a CR byte in ts-rs's output would fail CI's byte-exact diff on every Windows run. | `66f5669` |
| D-5 | `ui_state`'s file I/O — including `write_atomic`'s real `sync_all()` and two unbounded waits — ran on a tokio worker. | Both commands on `spawn_blocking`. | `4f935de` |
| D-6 | A third private copy of the `.cimp` path constant (`settings::persistence`, `ipc::note`, `ipc::ui_state`), plus a fourth literal in `fsutil`. | One `fsutil::CIMP_DIR_NAME`; the name is a filesystem-layout fact, so it belongs in the path module all of them already depend on rather than in the settings module none of them wanted to depend on. | `8c387e4` |
| D-7 | `top_level_fn` was a byte-identical copy of `fn_body`, described as its "non-`async` twin" (it never was — the signature is a parameter). | Deleted; two copies of a scanner primitive is one copy that gets hardened. | `8d0f1cb` |
| D-8 | `bad_body_result` re-spelled `bad_request`'s three fields one function away from it. | It delegates; the parse detail in the message is the only difference. | `8d0f1cb` |
| D-9 | `offload::latch` imported `bounded_id` and `live_settings` from `offload::loopback` — a back-edge from the module V42 R3 extracted to the module it was extracted from. | Neither is about routing; both move up to `offload`, and `loopback` re-exports them for its family files' `use super::*` and for `harness::claude::hook`. | `8d0f1cb` |

### One claim not reproduced

The dropped-at-cap list also named **"codegen O(N²)"** alongside the dead
`write_if_changed`. No quadratic algorithm was found in `settings::codegen` (or
in `settings::frontend_mirrors`, the other candidate): `write_if_changed` is
one `replace` plus one compare, and the field/member joins run over lists of
single digits. What *is* there is redundant whole-tree work — `export_all`
followed by a full read-back, and `export_to_string` rendered twice in the
determinism test. The dead read-back is closed above (D-4); the determinism
test's second render is the point of that test and stays. Recorded rather than
"fixed" with an invented change.

---

## Deferred, with an owner

| # | Finding | Ruling |
|---|---|---|
| DEF-1 | `ToolActivityView`'s tool-list tripwire — the D1 stopgap is not its real fix. | **Deferred to #131 (codegen).** Codegen is the actual fix; a note is on that issue. Not touched here. |
| DEF-2 | `DURABLE_VIEW_PREFS` is a hand-kept allowlist nothing checks, and the permanent import marker orphans any key promoted later. | **Accepted and documented** in `uiState.ts`'s module docs (`a916d69`): adding a durable pref means editing the set, and a missing name routes to `localStorage` quietly (the value persists, in the wrong scope); a pref promoted after a project's marker is set is never imported, because the marker means "this project has been through the import", not "these keys have been". Both accepted — the set has been stable, promotions are a deliberate reviewed act, and the alternative is machinery guarding a once-a-year event whose failure mode is one toggle reverting to its default. The honest fix if it bites is a one-off migration for that key, never a change to the marker's meaning. The build-time name-join that would close the first properly is the same shape as the `cssTokens` guard and rides with #131. |

---

## Refuted — do not re-raise

Four findings from the raw set did not survive inspection. Recorded so the next
review does not spend the same effort:

1. **`activity.rs` tripwire is vacuous.** It is not; the needle it scans for is
   present in code, not prose, and the scan is scoped to the item.
2. **`harnessRow()` defaults drift from the backend.** They do not — the row it
   supplies is the same answer `harness_settings` resolves for an absent
   harness, and that is the documented contract.
3. **The `localStorage` wrapper is a layering violation.** `viewSection.ts`
   routing a pref to `localStorage` or to `uiState` is the durable/ephemeral
   split working as designed, not a layer being crossed.
4. **The hydrate call should be hoisted above `loadHarnesses()`.** It should
   not; the two are independent, and the harness roster is deliberately started
   as early as the window can ask rather than sequenced behind a file read.

---

## Final ordering statement (RV-1 + RV-3 + RV-4)

One sentence per step, because the three findings interlock and the next person
should not have to re-derive it:

1. `hydrateUiState()` starts a ~2 s budget and calls `ui_state_get`.
2. **Read fails** → cache empty, window unhydrated (**write-inert**), no
   import. `mount(App)` proceeds on defaults.
3. **Budget expires before the read lands** → the timeout latches, the late
   answer is ignored entirely (cache stays empty, writes stay inert), one
   `console.warn`. `mount(App)` proceeds on defaults.
4. **Read lands in budget** → cache filled. **Reads are live from here**;
   only writes are still waiting.
5. **Marker present** (`'1'`) → no import owed → `hydrated = true`. Done.
6. **Marker absent** → the one-time import runs: it **copies** the durable
   `localStorage` keys and leaves them (RV-1), and sends one patch.
7. **Import commits** → the values are adopted into the cache and
   `hydrated = true`.
8. **Import fails** → `hydrated` stays `false`. Reads still answer from the
   file's values; writes are inert; the marker is unwritten and the next launch
   retries the whole import.
9. **Budget expires while the import is in flight** → the cache keeps the real
   file values read at step 4 (reading them is never the risk), but `hydrated`
   stays `false`: arming writes for a window whose import may or may not have
   committed is the half-story RV-4 is about. The next launch settles it.

The invariant across all nine: **a window writes only when its cache is a
complete picture of the file.** Everything else degrades to read-only, which is
the module's stated posture — *loses persistence, never breaks the UI*.

---

## New dependencies

**None.** RV-5's cross-process lock uses `std::fs::File::lock` / `try_lock`
(stable since Rust 1.89) rather than the `fs4` crate the finding suggested:
identical primitive (`flock` on Unix, `LockFileEx` on Windows), no new crate in
`Cargo.lock`, no license or build impact. The cost is `rust-version` 1.88 →
1.89, recorded in `src-tauri/Cargo.toml` and in `docs/MAINTENANCE.md`'s
not-covered-by-CI list. `Cargo.lock` is byte-identical to `8372e3e`.
