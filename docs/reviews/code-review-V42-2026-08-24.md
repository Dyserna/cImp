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

---

# Tranche 2 — review of `ed1e93e..develop`, 2026-08-24

A second adversarial pass, at **high** effort, over the V42 **tranche-2** merges
— lane A (machine-scope table #116, migration floor + step deletion #120, schema
per-domain split #121, invariant tables #125, offload lifts #126), lane B (the
SettingsApp split a/b/c + residuals #129, core splits #124), lane C (GraphService
/ advisor / mcp de-homing #117–#119, index split + stage-and-swap #122, param
objects #126 R26).

**Reviewer output:** 10 findings, plus five refuted on inspection and recorded
below so they are not re-raised. **Every one of the ten was ruled CLOSE**, and
every one carries its commit here.

| Gate | Baseline (`3542dad`) | After (`a2da0a0`) |
|---|---|---|
| `cargo test --all-targets` | 2834 passed · 0 failed · 6 ignored | **2838 · 0 · 6** |
| `cargo clippy --all-targets` | clean | **clean** |
| `npx vitest run` | 907 tests · 46 files | **909 · 46** |
| `npm run check` | 0 errors · 0 warnings · 394 files | **0 · 0 · 394** |
| `npm run build` | clean | **clean** |

Known flakes, unchanged by this pass: `sandbox::windows`'s mapped-drive case,
and the gitignored census check (#135 — intermittent in a full run, green in
isolation).

One commit in the range is not a finding: `3e2bd94` regenerates
`src/lib/settings/generated/settings.ts` after `0447d77`'s prose re-point, which
changed a Rust doc comment mirrored into the generated bindings without
regenerating them — the CI bindings diff would have gone red on the next run.
The sentence itself was re-wrapped too: it ran past the column and closed a
parenthesis it never opened.

## The ten

### T2-1 — a below-floor file that could not be quarantined was overwritten by the next save

`settings/persistence.rs`. `reseed_below_floor`'s move-aside-FAILED branch
returned seeded defaults and wrote nothing, which is half a promise: those
defaults become the live `Settings`, and the next write of the global file —
`load`'s own post-repair `save_global`, a Settings-window edit, one of the
out-of-band writers — put them straight over the user's ONE remaining copy of
settings this build could not read and could not move aside.

**Fixed** (`526bd91`): the failure latches the path (`PRESERVED_GLOBAL`, one
slot, path-keyed) and `save_to` — the single write path every global writer
funnels through — refuses it loudly for the rest of the session, returning an
error so each caller's own "save failed" log fires too. A successful quarantine
releases the latch before writing the defaults.

Deliberately **not** self-healing: nothing re-attempts the move inside the save
path, because that would move the user's file aside at an arbitrary moment to
make room for a write they never connected to it. The next launch re-reads,
re-reaches the verdict, and either succeeds or says so again — the floor's
existing loud-on-every-launch posture, extended to the writes.

The test drives the real branch: a DIRECTORY at every quarantine name the move
can aim at over a six-second window defeats both its rename and its copy
fallback. It is discriminating in both directions — a working quarantine fails
its byte comparison, a missing latch fails the refusal.

### T2-2 — an unstamped overlay beside a quarantined global entered the cascade at CURRENT

`settings/persistence.rs`. `load_global` reported a below-floor quarantine as
`stated: None`, which `load` could not tell from "the file was silent". An
overlay with no stamp of its own then fell through to the CURRENT default, so
`migrate_overlay` found nothing to do, the value deep-merged unmigrated onto a
DEFAULTS baseline, and serde silently dropped every key the deleted steps would
have MOVED — the invisible per-project loss V40 Phase I closed, arriving through
the floor.

**Fixed** (`123cb6b`): `load_global` answers with a
`GlobalLoad { settings, stated, below_floor }`, and `overlay_entry_version` is
the one place the entry version is decided — own stamp, else the version the
global file stated beside it, else CURRENT, except when the global was
below-floor-quarantined and the overlay states nothing, where there is no honest
answer and it returns `None`. `load` then logs an error naming the overlay file
and passes a version below every floor, so the refusal is `migrate_overlay`'s
own refuse-and-warn path (which still strips the stamp) rather than a second one
written at the call site.

The normal path is untouched by construction, and the test pins all four cases:
stamped-ok (self-describing — the quarantine next door is irrelevant),
unstamped-refused, and the two ordinary ones.

### T2-3 — two chrome children #129 (a) changed without auditing

`settings-chrome.css` + `EnvEditor.svelte` + `HarnessExtForm.svelte`. Hoisting
the form chrome out of `SettingsApp.svelte` unscoped it, so it reaches markup no
scoped rule ever could. #129 audited the sections; these two are what it did not.

**Fixed** (`a2da0a0`):

* `EnvEditor`'s `.remove:hover` (0,3,0) lost background and border-colour to the
  chrome's `button:hover:not(:disabled)` (0,3,1) and kept only the danger
  colour — a remove button that half turns red. Same counter-rule `ArrayEditor`
  already carries, with the arithmetic written down.
* `EnvEditor`'s monospace input rules TIE with `input[type='text']` at (0,2,1),
  which decides on emission order — an order that file cannot see: EnvEditor is
  used by the main window's tab dialogs too, so its CSS is in the shared app
  chunk, which loads BEFORE the settings chunk carrying the chrome sheet. Env
  names and values therefore rendered in the body font inside Settings and in
  monospace everywhere else. Rooted at `.env-editor`, both are the file's own
  decision again.
* `HarnessExtForm`'s bare `<section>` started matching the hoisted card rules and
  drew a card inside the Tabs section's card. **LV-20's half — the labels,
  checkboxes and hints finally looking like every other section's — is INTENDED
  and stays**; only the nesting is the defect, neutralized by class at (0,2,0)
  against the chrome's (0,1,1) rather than by relying on order (`ChecksEditor`'s
  `.check-card` is the same manoeuvre one card up).

Verified against the emitted bundle: Svelte renders these as (0,3,1), (0,4,0)
and (0,2,0).

### T2-4 — the toolclass scan lost its coverage of the two graph entry points

`offload/toolclass.rs`. V42 R8 (#119) routed `run_check` / `run_command` out of
`graph::mcp::handle_call` and `GraphService::run_graph_tool` into the shared
`dispatch_rootless`, and each entry point's `DISPATCH_SITES` row went with them.
Correct — a row over a body with no name literal yields nothing and
`served_names`' emptiness check would fail on it — but it left the two funnels
every warm and every headless graph call arrives through with nothing watching
them. A name comparison re-added there is a dispatch surface with no row, whose
tool classifies EXTERNAL and is waved past the latch on a native route (finding
M-2, and M-8 for why `run_check` sitting in one of those bodies mattered).

**Fixed** (`dd0762f`) with the opposite assertion, which is the stronger one
here: `NAME_BLIND_ENTRY_POINTS` names both bodies and the test asserts they
compare no tool name (`name == "x"`, `match name`, `matches!(name`), still call
`dispatch_rootless`, and that the table holds both.

**Probe:** planting `if name == "probe"` in `handle_call` fails the test naming
`["probe"]` and the gate it defeats. Reverted.

### T2-5 — #129 (c) regression: section state no longer survived a sidebar switch

The sidebar renders one section at a time inside an `{#if}`, so switching
DESTROYS the component and takes its `$state` with it. Before the split all of
this was the monolith's own state and lived as long as the window; the split
moved it into the children by default and turned "come back to it later" into
"start again" — silently, because nothing that survives a REMOUNT was involved.

**Fixed** (`b8eb050`): hoisted back to `SettingsApp`, reaching their section as
props + callbacks (the pattern `tabsSubSection` already used) — the offload test
box's prompt and result plus its sub-tab, the Appearance save-preset dialog
(open / name / error, as one value: the three are always written together), the
Tool Plugins selection and its per-tool Detect results, and the Code
Intelligence sub-tab. Nothing else moved: in-flight busy flags, transient inline
messages and per-mount view modes (`managingPresets`) are genuinely per-mount and
each now says so where it is declared. The Detect table is the one that cost the
most to lose — a probe is an IPC round trip the user asked for.

### T2-6 — the push-registers-with-draftSync scan was enforced over a fraction of its scope

`draftSync.test.ts`. The scan read exactly ONE file, `SettingsApp.svelte`, while
`applySettings` is importable by all twenty-one section children. The hole it was
built to catch (#129's ungated `commitGraphIgnore`) could therefore be reopened
in any of the other twenty-one with the guard still passing.

**Fixed** (`26e86c7`) by making the pair inseparable rather than by scanning more
files for it. `pushDraft(sync, next)` registers the push, sends it, and settles
the window in either direction; the window's four pushers route through it and
`SettingsApp` no longer imports `applySettings` at all. The structural scan is
re-spelled honestly — every pusher goes through `pushDraft`, the exemption map is
for RAW pushes and still empty — and gains the check that closes the scope gap:
**no source under `src/lib/settings/`, nor the window, may IMPORT
`applySettings`**, allowlist `store.ts` (defines it) and `draftSync.ts` (wraps
it), matched on import statements so the identifier stays usable in prose. Both
new checks carry vacuity guards.

**Negative control:** an `import { applySettings }` in `OffloadSection` fails the
ban naming the file and the specifier. Reverted.

### T2-7 — stage-and-swap recovery promotes by name, with no cross-site distinctness guarantee

`graph/index.rs`. The recovery branch promotes by NAME: a stage relation present
on open means "a prior migration was interrupted after the stage was durably
populated — adopt it", and nothing on disk says which migration built it. Two
sites sharing a stage name would let whichever runs first rename the other
relation's staged rows over its own live relation — the direction that loses
rows, performed by the branch that exists to prevent losing them. The engine sees
one call at a time, so the property is across call sites and had no owner.

**Fixed** (`10749b5`): a scan of the two constructing files resolves each site's
`live`/`stage` (a literal or a `Self::CONST` declared in the same file, panicking
on anything it cannot read) and asserts the stages are pairwise distinct, differ
from their own live relation, and collide with no other site's live relation.
Vacuity-guarded at >= 2 sites.

**Negative control:** pointing `USAGE_STAT_STAGE` at `"mem_note_v32"` fails the
test naming both files and the shared relation. Reverted.

### T2-8 — the cascade threaded a `ShellSpec` no remaining step uses

`settings/migration.rs`. The v1.0 / v1.1 steps rebuilt a shell entry from the
default shell; every step after them declared the parameter as `_shell` and
ignored it. V42 R9 (#120) retired those two steps with the migration floor and
left the parameter behind: `persistence::load` threaded it into
`migrate_if_needed` and `migrate_overlay`, which handed it to eight wrappers
whose entire body was to drop it.

**Fixed** (`072ce44`): steps are `fn(&mut Value)`, the eight wrappers are gone
and `MIGRATION_STEPS` points at the transforms directly, both public entry points
lose the parameter as do their two call sites, and `fake_default_shell` plus its
thirteen `let shell = …` lines go with them — as does the comment claiming the
deleted steps still need the shell. **Step bodies are untouched**: the ladder's
shape is unchanged and the cascade and floor tests pass unchanged.

### T2-9 — `OVERLAY_BANNED_KEYS` was a hand-kept parallel of the `Banned` rows

`settings/persistence.rs`. The drift class #116 exists to kill, kept in step by a
test asserting the two agreed. The disagreement it guarded against was not
cosmetic: the marker does no work of its own, so a `Banned` row whose key the
const never gained is a family stripped by nothing on all three legs.

**Fixed** (`1d68120`): `strip_overlay_banned` walks the table itself. The const
is gone, and with it the agreement half of the test; what that half was really
asserting is asserted directly instead — a banned row's keys must be TOP-LEVEL
names, because the pass is a top-level `remove` — plus a vacuity guard. The
all-three-legs-or-none invariant is kept unchanged, and is what makes deriving
from `overlay_strip` safe for the readonly and diff legs too.

Prose re-pointed in the four other files that named the const
(`ipc/commands.rs`, `sandbox/mod.rs`, `settings/schema/mod.rs`,
`docs/DESIGN.md`). Two of them said `harness` was in the list; it has not been
since V40 review M-2 took it back out, so those two now say what actually holds
it back (the structured per-field strip) and why the whole-key ban would be
wrong. `schema/mod.rs`'s doc is mirrored into the CI-diffed generated
`settings.ts`, regenerated in the same commit.

### T2-10 — the orphan guard duplicated the token guard's walker, and re-read the repo

`tests/settingsCssOrphans.test.ts` carried `walk`, `read` and `rel` byte for byte
from `tests/cssTokens.test.ts`. Two copies of a walk is one copy that can grow an
exclusion the other never hears about — a guard that quietly stops looking at
part of the tree, which is the failure mode both of these exist to prevent.

**Fixed** (`4b249a0`): they live in `tests/repoFiles.ts` (not a `*.test.ts`, so
vitest does not collect it), with the reason they are outside `src/` written down
once. Two costs in the orphan scan went with the duplication: `unscopedCss()` —
every theme sheet plus every `.svelte` file in the repo, for the `:global(…)`
pass — ran twice, once in the scan and once in the vacuity guard, so the scan
hands the set back; and `hasRule` compiled a `RegExp` per class per source,
replaced by one `classTokens` extraction and a Set lookup with the same
permissive rule and the same charset `classesUsed` uses.

**Negative controls, one per guard:** an unstyled class planted in a section
fails the orphan guard naming it; a `var(--undeclared)` planted in the chrome
sheet fails the token guard naming it. Both reverted.

## Refuted — recorded so they are not re-raised

1. **Overlay `stated=None` as originally framed.** The first form of the finding
   misplaced the mechanism; the real one survives as T2-2 above, which is a
   different claim about a different branch.
2. **`load_readonly` bypasses the migration floor — as a regression.** It does
   not migrate at all, by contract (no side effects for the MCP child), and that
   predates the floor. Not something tranche 2 changed.
3. **`TabsSection`'s `?? []`.** Not a swallowed error path.
4. **The `kind_cap` arm.** Reads correctly on inspection.
5. **`slot_for` divergence.** The matrix and its callers agree.

## New live-verify items

Two of the fixes are visible to the user and are eyes-on rather than
test-covered:

* **T2-5** — open a section, leave state in it, switch sections and come back:
  the offload prompt + result and its sub-tab, a half-typed preset name, the
  selected plugin and its Detect results, and the Code Intelligence sub-tab are
  all still there. Closing and reopening the Settings window still starts clean.
* **T2-3** — Settings → Checks (or a tab's env editor): env rows render
  monospace and the `×` hover is fully red, not half. Tabs → the harness page: the
  declared-settings block reads as a group inside the Tabs card, not as a card
  inside a card, and keeps LV-20's label / checkbox / hint chrome.
