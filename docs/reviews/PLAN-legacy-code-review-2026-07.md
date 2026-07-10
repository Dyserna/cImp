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

- [x] 4a done — findings: 11 (2026-07-10) &nbsp; [x] 4b done — findings: 2 (2026-07-10)
- **4a:** `src/lib/layout/tree.ts` (281), `src/lib/layout/store.ts` (637),
  `src/lib/layout/persistence.ts`
- **4b:** `src/lib/dnd/drag.ts` (196), `src/lib/dnd/dropTarget.ts` (137)

**Why:** May-era V4 code; tree.ts has good tests but store.ts and drag.ts have
none; pointer-event code is where ghost-drag/orphaned-pane bugs live.
**Focus:** tree invariants after split/close/move sequences (store ↔ tree
consistency), persistence round-trip of odd layouts, pointer capture/release
on cancelled drags, drop-target hit-testing at pane edges.

## Session 5 — OOB adapters & session memory (~1,000 lines)

- [x] Done — findings: 13 (2026-07-10)
- `src-tauri/src/oob/opencode.rs` (466) — grew across V20/V10/V14 with no review between
- `src-tauri/src/oob/mod.rs` (170)
- `src-tauri/src/graph/memory.rs` (360) — added *after* the 28-finding graph review

**Why:** feature accretion without review; memory.rs is the one graph file the
graph sweeps never covered.
**Focus:** SSE/event-parse robustness (malformed or reordered OpenCode events),
part-type dispatch (the "speaking reasoning" bug class), usage-tap arithmetic,
memory.rs distillation/pruning correctness and DB-write failure handling.

## Session 6 — Small stragglers batch (~900 lines total, many small files)

- [x] Done — findings: 15 (2026-07-10)
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
| 2026-07-10 | 4 — layout tree & drag-and-drop | 13 fixed (4a: 11, 4b: 2). Placement queue: NewShellTabDialog cancel leaked its queued placement (pushed at `+`-click, never consumed → hijacked the next tab-created anywhere; push moved to submit-time with paneId carried through the dialog store), `onSpawnAiTab`/`onNewPreviewTabAction` never cancelled on IPC failure, and `cancelLastPlacement`'s LIFO pop against the FIFO queue could cancel the wrong in-flight request (now identity-based `cancelPlacement`). Store: `resetLayoutToSinglePane` adopted document-order active tab instead of the focused pane's; `closeFocusedPane`/`moveAllTabsToPane` defensive break dropped the in-flight tab AND collapsed the not-yet-moved rest (now atomic abort); dead `splitFocusedPane` removed. tree: `moveTab` to a nonexistent pane silently vanished the tab. persistence: duplicate tab ids never deduped (dup in one pane breaks the keyed `{#each}`; cross-pane dups fight over the terminal host — now first-occurrence-wins dedupe + orphans placed once under duplicate pane ids), split ratio never sanitized on load (bad value round-tripped forever, one frame of negative flex), collapse-loop double tree-walk, comment overclaimed a nonexistent backend integrity check. dnd: `beginDrag` re-entrancy leaked listeners/capture and stuck `cursor:grabbing` app-wide (now force-cancels the stale machine); pointer handlers moved from sourceEl to window — the "falls back to bubbling" comment was false, a refused capture stranded a frozen ghost + capture-phase Esc interceptor forever. Refuted: reorder insertIndex off-by-one (adjustment exactly compensates), computeZone edge/boundary claims (strict inequalities + tab-bar-first ordering), findUnderCursor edge ties (4px splitter dead zone), cancelDrag-vs-preset-restore ordering, first-emission swallow race (install is synchronous after hydration), collapse-loop dangling focus, paneRegistry staleness across preset restore. Nits left: off-screen reorder-line preview (cosmetic), blanket 50ms click swallower (documented tradeoff). + 34 new tests incl. first suites for store.ts and drag.ts | (this commit) |
| 2026-07-10 | 2 — TTS text-processing path | 16 fixed. segmenter: "No." merged next sentence, NFD combining accents fragmented words, CRLF killed paragraph breaks, punctuation-only segments synthesized, no split before closing quote/bracket. prose: fence close ignored marker length (code leaked into speech), tables spoken with pipes, ~~strikethrough~~ tildes spoken, nested blockquote `>` leaked, image `!` leaked, `#hashtag` eaten as heading, false doc claims. phonemize: one symbol-run token ("###", "->") silently dropped the whole sentence's audio (retry without symbol tokens), 510-token truncation cut mid-word. Refuted: NBSP split miss (sanitizer runs first), unclosed-fence swallow (CommonMark semantics, now documented). Deferred: HTML-tag stripping (rare, risky), patterns.json all-or-nothing parse + non-atomic seed write (documented design). + regression tests for all fixes | (this commit) |
| 2026-07-10 | 6 — small stragglers batch | 15 fixed. broadcaster: `set`/`mutate` broadcast AFTER releasing the store lock, so two racing writers could deliver the older state last — every subscriber (incl. the `settings-changed` fan-out to all windows) stayed stale until the next unrelated change (send now under the lock; `broadcast::Sender::send` is non-blocking); the saver's poisoned-lock branch `continue`d instead of recovering like every other lock site — std mutex poisoning is permanent, so ONE panic-while-locked silently disabled settings persistence for the rest of the process (now `into_inner` like `current()`); a saver already mid-write with an older snapshot could complete its atomic rename after a shutdown `flush()` and clobber the newer data (new `save_lock` held across snapshot-read + write in both writers, so the second writer always persists newer-or-equal state). logging: the saved level unconditionally clobbered a `RUST_LOG` override milliseconds after startup, contradicting both the module doc and the main.rs comment (init records `ENV_OVERRIDE`; main skips the startup `set_level` when active; a LIVE settings change still wins); invalid `RUST_LOG` silently fell back with zero diagnostic (stderr note added). detect: `which("bash.exe")` resolved the WSL launcher shim `System32\bash.exe` (present whenever the WSL feature is enabled, System32 always on PATH) and labeled it GitBashPath — install-Git banner suppressed, dialog pre-filled with a WSL boot instead of Git Bash (now `which_all` + case-insensitive windir filter); registry probe read HKLM only — per-user "just for me" Git installs write HKCU (now both hives, validated per hive so a stale HKLM falls through); `was_default_git_bash_found` re-ran the whole probe chain (file+registry+PATH walk) per dialog open for one bool (replaced with `is_git_bash_source` on the already-computed source); dead `cfg(not(any(unix,windows)))` arm used the `cfg(windows)`-gated `warn!` import — compile error on exactly the targets it exists for (qualified). resolve: dotted-but-not-extensioned names (`aws2.1`, `python3.11`) never got `.exe`/`.cmd`/`.bat` trials in ebin (`Path::extension()` = `Some("1")` short-circuited; now an executable-extension whitelist); Unix ebin hits skipped the exec-bit check — a `+x`-less bundled file resolved then died EACCES at spawn instead of falling back to PATH. shortcuts: the literal `+` key was unrepresentable — capture emitted `Ctrl++`, parse split-and-filtered it into a modifier-less `key:"ctrl"` predicate that can never match (silently dead user shortcut; now emits `Ctrl+Plus`, parses legacy trailing-`+` shapes); Space likewise — `Ctrl+ ` trimmed to a dead predicate, bare Space to `null` (now `Space`). templates: a `{selection}` substitution splicing in code containing `${name}` (JS template literals, shell) created bogus tab-stops — Tab selected a span of the user's own pasted code (overtyping deleted it) and Tab-interception never turned off (placeholder pattern now excludes `$`-prefixed braces via lookbehind, shared by scan+substitution). usageMath: `fmtTok` tested `< 1000` before rounding, so 999.5–1000 printed a bare `"1000"` with no suffix (latent — callers pass integers). Refuted: `$&`/`$1` corruption in template substitution (function replacement is verbatim — pinned by test), shared-regex reentrancy + `nextPlaceholderRange` fromIndex edges (all sites reset lastIndex, no interleaving path), named-key round-trips (identity fallback covers arrows/F-keys), diffWords LCS/backtrace + tokenize char-drop (complementary classes tile the string) + `\ No newline` marker reaching `pairHunkLines` (backend diverts it to `no_newline_at`, foreign markers break the hunk loop), usageMath NaN/negative inputs (u64s clamped at ingestion) + `cacheHitRatio` backend drift (byte-for-byte identical), saver-vs-handle `global` divergence (frozen twins), debounce losing updates (payload-less signals, saver re-reads), torn overlay writes (write_atomic), `run_cleanup` deleting the active log file (lazy rotation + age bound). Nits left: Escape unbindable via capture UI (intentional — hardwired stop_tts), WOW6432Node (32-bit Git discontinued), relative-command-vs-tab-cwd resolution contract, fmtTok "10.0k"/no-B-suffix cosmetics. + 12 regression tests incl. a concurrent-mutate broadcast-order test and first-ever parser.test.ts | (this commit) |
| 2026-07-10 | 5 — OOB adapters & session memory | 13 fixed. opencode: a clean SSE close returned the same `Ok` as cancellation, so ONE TUI server restart permanently killed the adapter (no speech/avatar state for the rest of the tab's life — now `StreamEnd::{Cancelled,Closed}`, Closed reconnects); per-chunk lossy UTF-8 decode corrupted any multibyte char split across a chunk boundary into U+FFFD in spoken text (now a byte line-buffer, complete lines decoded only); stream close/error mid-turn never released Thinking (the fresh post-reconnect Tracker can't emit the missing Stopped — now released on both exit paths); `time.completed: null` read as completed → premature flush latched `flushed` and permanently dropped the rest of a still-streaming message; a completed `message.updated` seen before any parts (mid-turn join/reorder) latched `flushed` empty so late-arriving text was never spoken (idle now retries); partial accumulated deltas shadowed a fuller `part.updated` snapshot (truncated speech after a mid-message join — fuller view now wins); part dispatch was a one-string denylist (`reasoning`) so any future declared part type (tool/patch/…) streaming text would be spoken (now any declared non-`text` type is skipped; undeclared still speaks); Tracker maps grew unboundedly for the connection's life incl. user-echo parts, plus a write-only `part_msg` map (parts consumed at flush, buffers cleared at idle, dead map deleted). mod.rs: `speak()`'s bounded TTS send didn't race the cancel token (a closing tab parked behind a backed-up channel) and the tts toggle was read only at burst start despite the "read live" doc (now select-on-cancel + per-sentence gate). memory.rs: `parse_distilled_facts` persisted model wrapper ("Here are the facts:", bullets, code fences) verbatim as user-visible project facts AND rejected a good 3-fact answer wholesale over one preamble line (now normalizes — drop fences/trailing-colon preambles, strip bullet/number markers — then validates); `WorkingSetEntry` doc claimed `recency × frequency × kind_weight` but the ranker scores `frequency × kind_weight` with recency as tie-break only (doc fixed). claude.rs (shared classify contract): Bash `tool_use` missing `command` recorded a content-free mem_event eating a `MAX_EVENTS_PER_SESSION` ring slot — loopback's OpenCode ingress guarded this, Claude's tap didn't (extracted `mem_target` with the same guard; taps now classify identically). Refuted: dropped `ClaudeOutputStopped` on a saturated 512-slot state channel (dedicated consumer, documented best-effort posture, SubprocessExited backstop), multi-line SSE `data:` joining per spec (OpenCode serializes JSON single-line), missing reconnect backoff (500 ms loopback retry against our own TUI, cancel-bounded), unbounded `line_buf` cap (loopback-only, self-launched peer), usage-ring under-prune on Turn upsert (upsert overwrites in place, never grows the ring). Nit left: `top_symbols` always empty in production (no caller passes symbols — forward surface, no wrong data). + 13 regression tests incl. a real-socket SSE close/reconnect test | (this commit) |
