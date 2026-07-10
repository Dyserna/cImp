# Legacy Code Review Plan (2026-07)

Staged review of code that predates or was skipped by the recent review sweeps
(V10–V15, workbench, graph, ipc, post-v0.40.x). Derived from a full-history
analysis (2026-07-10): last-touch dates, review-commit coverage, fix-density,
and risk indicators across all 392 commits.

## How to run a session

Each session is self-contained. Start a **fresh Claude session** and prompt:

> Run Session N of docs/reviews/PLAN-legacy-code-review-2026-07.md

Per session:

1. Read only the files listed for that session (plus direct callers/tests as needed).
2. Review for correctness bugs first, then robustness (panics on bad input,
   races, leaks), then simplification. Use the session's focus notes.
3. Fan out finding-hunts to Sonnet agents; synthesize/verify with the main model
   (per cost policy). A scoped Workflow (find → adversarial verify) is fine for
   the two largest sessions; flag expected cost first.
4. Apply confirmed fixes, add the missing tests called out below, run
   `cargo test` / `npm test` + `run_check {changed_only:true}`.
5. Commit as `fix(review): legacy sweep session N — <area>` on `develop`.
6. Tick the checkbox here and note the finding count.

Sessions are ordered by priority; they are independent, so any order works.

---

## Session 1 — Avatar/animation frontend cluster (~700 lines)

- [x] Done — findings: 10 (2026-07-10)
- `src/lib/spritePlayer.ts` (330) — animation state machine
- `src/lib/avatarState.ts` (232)
- `src/lib/avatarConfig.ts` (127)

**Why:** the app's face; oldest core code (May, v0.8.0 era); only scrutiny ever
was the generic May bug-hunt; **zero test files**.
**Focus:** rAF/timer lifecycle (leaks on tab switch/teardown), stuck-state
transitions in the sprite state machine, manifest-driven group edge cases
(missing/empty groups), listener cleanup. Add unit tests for the state
transitions while there.

## Session 2 — TTS text-processing path (~850 lines)

- [x] Done — findings: 16 (2026-07-10)
- `src-tauri/src/tts/phonemize.rs` (287) — untouched since v0.23 rename
- `src-tauri/src/processing/segmenter.rs` (131)
- `src-tauri/src/processing/patterns_file.rs` (210)
- `src-tauri/src/oob/prose.rs` (217) — **one commit ever**

**Why:** the audible core; predates every review pass.
**Focus:** unicode/grapheme handling in segmentation, OOV/espeak fallback paths
in phonemize, regex/pattern edge cases in patterns_file (12 unwraps — verify
which are non-test), prose.rs markdown-stripping correctness (it feeds
everything the user hears).

## Session 3 — External-input parsers / panic surface (~1,000 lines)

- [x] Done — findings: 16 (2026-07-10)
- `src-tauri/src/checks/parsers.rs` (341) — **single commit, never revisited**
- `src-tauri/src/theming/mod.rs` (483) — parses user-editable theme/palette files
- `src-tauri/build.rs` (161) — copies themes/binaries at build time

**Why:** both runtime files parse *external* input (tool output, user-edited
files on disk). theming has 14 unwrap/expect (some in tests — verify the rest).
**Focus:** malformed input must degrade, never panic (backend panic = broken
app); parser edge cases against real cargo/tsc/eslint output variants;
build.rs failure modes on missing/locked files.

## Session 4 — Layout tree & drag-and-drop (~1,320 lines) — largest; consider splitting

- [ ] 4a done — findings: __ &nbsp; [ ] 4b done — findings: __
- **4a:** `src/lib/layout/tree.ts` (281), `src/lib/layout/store.ts` (637),
  `src/lib/layout/persistence.ts`
- **4b:** `src/lib/dnd/drag.ts` (196), `src/lib/dnd/dropTarget.ts` (137)

**Why:** May-era V4 code; tree.ts has good tests but store.ts and drag.ts have
none; pointer-event code is where ghost-drag/orphaned-pane bugs live.
**Focus:** tree invariants after split/close/move sequences (store ↔ tree
consistency), persistence round-trip of odd layouts, pointer capture/release
on cancelled drags, drop-target hit-testing at pane edges.

## Session 5 — OOB adapters & session memory (~1,000 lines)

- [ ] Done — findings: __
- `src-tauri/src/oob/opencode.rs` (466) — grew across V20/V10/V14 with no review between
- `src-tauri/src/oob/mod.rs` (170)
- `src-tauri/src/graph/memory.rs` (360) — added *after* the 28-finding graph review

**Why:** feature accretion without review; memory.rs is the one graph file the
graph sweeps never covered.
**Focus:** SSE/event-parse robustness (malformed or reordered OpenCode events),
part-type dispatch (the "speaking reasoning" bug class), usage-tap arithmetic,
memory.rs distillation/pruning correctness and DB-write failure handling.

## Session 6 — Small stragglers batch (~900 lines total, many small files)

- [ ] Done — findings: __
- `src-tauri/src/settings/broadcaster.rs` (172) — lock-heavy cross-window fan-out
- `src-tauri/src/shell/detect.rs` (254) — heuristics, stale since June 12
- `src-tauri/src/pty/resolve.rs` (129)
- `src-tauri/src/logging.rs` (152)
- `src/lib/compose/templates.ts` (160), `src/lib/diffWords.ts` (136),
  `src/lib/usageMath.ts` (63), `src/lib/shortcuts/parser.ts` (85)

**Why:** individually small, collectively the remaining never-reviewed surface.
**Focus:** lock ordering/hold-across-emit in broadcaster, shell-detection
false positives, path-resolution edge cases (spaces, UNC), template escaping,
usage math rounding.

---

## Explicitly de-prioritized

- `offload/server.rs` — live-verified E2E in June, regression tests, dedicated
  2026-06-25 pass; unwraps are mutex idioms/tests.
- `graph/service.rs`, `ipc/commands.rs`, `offload/service.rs` — high historical
  fix-density but all covered by recent sweeps.

## Progress log

| Date | Session | Findings | Commit |
|------|---------|----------|--------|
| 2026-07-10 | 1 — avatar/animation frontend | 10 fixed (3 sprite-player races/freezes, empty-manifest + tile-contract degrades, loadedSet retry latch, fallback rotation key, stale crossfade timer, listener ownership, ghost per-tab entries) + tests for the state-transition logic | (this commit) |
| 2026-07-10 | 3 — external-input parsers / panic surface | 16 fixed. parsers: `fatal error:` lines downgraded to Note, timestamp/indented `word:N:N` lines produced junk diags, file-less tsc errors (TS18003 broken-tsconfig class) dropped → run read as clean, ANSI color corrupted file fields (now stripped for line parsers), one eslint message missing `severity` nulled the whole run, BOM broke eslint doc parse, pytest parametrized `[a - b]` node ids split mid-param. theming (no panic surface found — all unwraps test-only): duplicate palette `name` across files silently nondeterministic (now warned), 2 silent skip paths now warn. build.rs: delete-then-copy could leave `themes/` emptied on a partway copy failure (now non-destructive sync + prune, warns instead of failing the build), locked espeak-ng-data no longer fails the build when a usable copy exists, `?` in `find_espeak_data` aborted the whole search on one bad dir entry, espeak data had no stale cleanup, symlink-to-dir broke `copy_dir_all`. Refuted: rerun-if-changed on nonexistent system paths (cargo treats missing as always-dirty → perpetual rebuilds), unlabeled-severity default change (Note is documented design), pytest summary heuristic (lenient by design). + 11 regression tests | (this commit) |
| 2026-07-10 | 2 — TTS text-processing path | 16 fixed. segmenter: "No." merged next sentence, NFD combining accents fragmented words, CRLF killed paragraph breaks, punctuation-only segments synthesized, no split before closing quote/bracket. prose: fence close ignored marker length (code leaked into speech), tables spoken with pipes, ~~strikethrough~~ tildes spoken, nested blockquote `>` leaked, image `!` leaked, `#hashtag` eaten as heading, false doc claims. phonemize: one symbol-run token ("###", "->") silently dropped the whole sentence's audio (retry without symbol tokens), 510-token truncation cut mid-word. Refuted: NBSP split miss (sanitizer runs first), unclosed-fence swallow (CommonMark semantics, now documented). Deferred: HTML-tag stripping (rare, risky), patterns.json all-or-nothing parse + non-atomic seed write (documented design). + regression tests for all fixes | (this commit) |
