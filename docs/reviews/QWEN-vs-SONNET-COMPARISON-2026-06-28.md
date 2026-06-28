# Qwen 3.6 (local offload) vs Sonnet — code-review head-to-head (2026-06-28)

**Local model under test:** `Qwen3.6-35B-A3B-UD-Q4_K_M` — a 35B-total **Mixture-of-Experts with only ~3B active params** per token, UD-Q4_K_M quant, served by llama-server (1 slot @ 180k ctx) on the RTX 3070.
**Cloud baseline:** Claude Sonnet (14 parallel finder agents).

> **Worth re-running:** the result below is for the **3B-active** MoE. It would be interesting to repeat this with a **dense ~27B (27B-active) model**, which has been reported to outperform this 3B-active MoE on reasoning. The hypothesis to test: the dense model closes Qwen's three weak spots here (deep concurrency, validation-invariant logic, severity calibration) while keeping the strong pattern-bug recall. Same 14-unit harness, same prompts — just swap the served model.

Same 14 review units, same finder mandate. Sonnet ran as 14 parallel agents; Qwen ran serially via the offload server. Frontend's 4 large Sonnet units were consolidated into 2 narrowed Qwen calls (the full units overflowed Qwen's context).

## Per-unit scorecard (Qwen vs Sonnet's findings on the same scope)

| Unit | Qwen result | Matched Sonnet | Qwen unique | Key misses |
|---|---|---|---|---|
| offload process/supervisor | MIXED — low overlap | ~1 (supervisor lock, diff line) | **3** incl. drain-hang [HIGH], zombie pollers, empty-answer-masks-error | trailing-slash cache, kill().await, consecutive-user-turns |
| offload net/MCP | partial; 81k verbosity overflow | 3 (relay timeout, session-id, reader-hang) | 0 | proxy_run status-check, unbounded SSE buf, nested-think |
| offload native tools | STRONG | **got the git -c SECURITY bug** + 3 | 2 (Windows .cmd→cmd.exe, subcommand-shift) | — |
| graph | WEAK on correctness | 3 cleanups (search_docs, ef-overflow, remove_file) | low-value | **both HIGH bugs** (watcher panic, backfill race); rated everything "cleanup" |
| processing | full scope → EMPTY; narrowed → STRONG | 2 (CUF, raw_buffer) + **beat Sonnet** (all 4 cursor sites vs 1) | 1 | scan_offset unclosed-opener; under-rated buffer leak as cleanup |
| pty | STRONGEST | **5/5** signal-drops + resize TOCTOU + poisoned-mutex | **1** (manager.rs:130 error-path exit drop) | rx/tick copy-paste (cleanup) |
| tts/audio/stt | STRONG | 5 (StopAll, 2×ONNX, cpal, total_duration) | 1 (current_frame_len) | **notify_waiters lost-wakeup — missed despite a hint** |
| settings/state | FULL MATCH | **5/5** incl. global-mute, dangling-active, overlay-revert, migration-loop | split out tick-sweep | (migration.rs out of scope) |
| ipc/notifications | STRONG | 4/4 (question-asymmetry, cross-tab key, shlex, canonical-order) | **1** (just_dispatched mid-drain leak) | remove() wipes-N-counts |
| frontend-A (leaks) | STRONG | 4/4 leak family (GraphMonitor, Offload, Split, paused) | 1 (testEmbedder) | App.svelte void-listen |
| frontend-B (TS) | STRONG | 5/5 (optimistic-write, init-race, aider, 2×selectionTts) | 0 | **layout active_tab_id — missed despite a hint** |

## Verdict

**Where Qwen matched or beat Sonnet:** pattern-shaped bugs — dropped `try_send`, locks/blocking on async, swallowed errors, discarded UnlistenFns, async-onMount leaks, trait-contract violations, optimistic writes, off-by-one clamps. On tight scopes it frequently hit Sonnet's findings line-for-line, occasionally found MORE (all 4 cursor sites; manager.rs:130; notifications:488; the offload drain-hang Sonnet missed). It independently found the single most important item in the whole run — the **git `-c` flag-bypass security bug**.

**Where Qwen is weaker:**
1. **Deep concurrency reasoning** — missed the `notify_waiters` lost-wakeup *even when the prompt named `notify_one` vs `notify_waiters`*.
2. **Validation-logic bugs** — missed the layout `active_tab_id`-in-filtered-set check *even when hinted*.
3. **Severity calibration** — rated real correctness/leak bugs as "cleanup" (whole graph unit marked cleanup; missed its 2 HIGH bugs incl. a process-crashing panic).
4. **Complex-async low overlap** — on the hardest file (offload service/supervisor) Qwen and Sonnet found largely *different* sets → they're complementary there, not redundant.

**Operational caveats (Qwen/offload, not the model's analysis quality):**
- Scope-sensitive: 7-file unit → empty return; same unit at 2 files → strong. Keep units ≤2-4 files.
- Needs a tight output cap ("JSON only, <3500 tokens") or it over-produces (one unit = 81k chars, overflowed the tool).
- Emits confidence as floats, not the requested high/med/low.
- Server issues hit during the run are logged separately (offload-investigation-notes): concurrency dispatch failures, silent empty-on-overflow, orphaned-slot-on-cancel.

## Bottom line
Qwen3.6-35B-A3B (3B active) on narrow, pattern-rich scopes lands roughly **80–100% of Sonnet's findings** and adds the odd unique catch — a genuinely useful, ~free **first-pass / cross-check** tier. It is NOT a substitute for Sonnet on the subtle minority: deep concurrency, validation invariants, and correct severity ranking. Best use: run Qwen first to sweep the cheap pattern bugs, then spend Sonnet/Opus budget on the hard reasoning and on severity triage. **Re-test with a dense 27B-active model to see if the weak spots close.**
