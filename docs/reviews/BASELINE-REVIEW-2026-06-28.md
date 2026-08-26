# Baseline full-codebase review — 2026-06-28 (develop @ 9d881b8)

**Reviewer fleet:** 14 Sonnet finder agents (10 Rust + 4 frontend), high-recall.
**Coverage:** ~37k LOC Rust (22 module areas) + ~23k LOC Svelte/TS.
**Caveats:** Findings are recall-biased and NOT individually re-verified — expect some false positives (each finder self-filtered only). The local-Qwen cross-check sweep was unavailable (llama-server down), so this was a Sonnet-only fleet in practice. Confidence tags below are my triage, not the finders'.

This is the **baseline mark**: future reviews can run on the diff since this commit. ~108 raw candidates → after dedup, ~55 bugs + ~30 cleanups.

---

## 0. SECURITY

| # | file:line | issue |
|---|---|---|
| S1 | offload/tools/run_command.rs:92 | `flag_denied` misses POSIX concatenated short flag `-ccore.hooksPath=/x` → bypasses the git `-c` guard; attacker-controlled hooksPath runs on next hook subcommand. **Highest-priority bug in the run.** |
| S2 | statusline/mod.rs:317 | palette `name` from settings.json joined into path without sanitization → `../` traversal read (local-only; needs settings write). |
| S3 | offload/tools/read_file.rs:60 | TOCTOU between `metadata()` size guard and `read()` → a growing file allocates full post-growth size → OOM. |

## 1. SYSTEMIC THEMES (fix once, fixes many)

### A. `state_signals` try_send drops desync the avatar/permission state machine
Shared mpsc, capacity 64, all tabs + all signal kinds. Dropped sends leave the state machine stuck:
- pty/tasks.rs:350 ClaudeOutputStarted drop → avatar stays Idle during a real response.
- pty/tasks.rs:372 ClaudeOutputStopped drop → stuck Thinking between turns.
- pty/tasks.rs:409 PermissionPromptResolved drop → permission overlay stuck up.
- pty/tasks.rs:539 SubprocessExited drop (blocking-pool, can't await) → backend/frontend desync.
- notifications/manager.rs:245 — related: `just_dispatched.remove()` wipes ALL N echo counts → notification cascade.
**Direction:** bounded-but-larger or per-tab channels, or treat these control signals as must-deliver (send().await off the blocking path / a coalescing latest-wins cell). Same family as the window-resize false-notification already fixed at b91b392.

### B. Frontend Tauri-listener / subscription leaks
Two shapes: (1) async `onMount` registers a `listen()` after an `await`, but `onDestroy` ran first with the unlisten still null; (2) `void listen(...)` discarding the UnlistenFn with no teardown. Affected: GraphMonitorView:69, OffloadServerView:19, BackendDashboardCard:53, App.svelte:182, avatarState.ts:222, stt.ts:35, audioStream.ts:46, selectionTts.ts:290, Split.svelte:121, StatusBarArrangement.svelte:85 (pointercancel).
**Direction:** a shared `listenManaged(event, cb)` helper that defers registration safely and auto-unregisters on destroy; capture-and-clear pattern for all window listeners incl. `pointercancel`.

### C. Blocking work on the async runtime
CPU/IO-bound work on tokio workers starves IPC/audio/amplitude:
- content.rs:76 blocking disk write on the PTY hot path.
- tts/worker.rs:76,188 blocking ONNX `synthesize()` not in spawn_blocking.
- graph/service.rs:294 StdMutex held across blocking SQLite open.
- offload/supervisor.rs:360,452 TokioMutex guard held across `set_state().await` / `kill().await`.
- settings/broadcaster.rs:118 Mutex held across full `Settings` deep-clone.
**Direction:** spawn_blocking for CPU/IO; drop guards before await/clone.

## 2. CORRECTNESS BUGS — HIGH

| file:line | issue |
|---|---|
| state/manager.rs:638 | global `ai_tts_suppressed` cleared by ClaudeOutputStarted from ANY tab → Esc-silenced tab resumes when another tab outputs. |
| audio/playback.rs:457 | `notify_waiters()` drops idle edge if no Notified future is being polled → notification queue can stick permanently. Use `notify_one()`. |
| stt/capture.rs:159 | cpal stream error only `warn!`s, never sets SttState::Error → Stop yields silent empty transcript; user thinks they recorded. |
| settings/persistence.rs:358 | failed overlay `remove_file` returns Ok(()) → stale overlay re-merged next launch, silently undoes user's revert. |
| settings/store.ts:41 | `applySettings` optimistic `set()` before await; backend failure → no rollback, UI diverges, changes lost on restart. |
| terminals.ts:207 | `displayNameFor` returns 'Shell' for aider/aider-local → error overlay mislabels Aider crash as "Shell exited". |
| graph/watcher.rs:86 | `thread::spawn().expect()` panics on thread exhaustion, bypasses graceful match → can crash process. |
| graph/service.rs:547 | backfill single-flight `again` flag orphaned in a lock gap → missed embedding pass until next file change. |
| offload/service.rs:729 | remote cache sig built from raw base_url vs trimmed `remote_sig()` → trailing slash = perpetual cache-miss, in_flight resets to 0, router never spills. |
| offload/mcp.rs:507 + loopback.rs:491 | relay/SSE reads & writes have no timeout → half-open TCP after hard-kill hangs/leaks tasks; list_changed stops reaching Claude. |
| offload/mcp.rs:423 | `proxy_run` doesn't check HTTP status before json parse → 4xx/5xx silently falls back to self-contained path (proxy_graph:467 does check). |
| processing/tags.rs:200 | `raw_buffer` grows unbounded when no `[[TTS]]` markers (scan_offset frozen → watermark 0 → compact never fires). MEMORY LEAK + O(session²) rescan (:199). |
| processing/screen.rs:231 | `move_cursor` clamps cursor_col to MAX_COLS not MAX_COLS-1 → persistent one-column write error after a huge CSI col param. |

## 3. CORRECTNESS BUGS — MEDIUM

- offload/mcp_host.rs:982 — read_sse_result buffer unbounded between newlines → OOM (only bound is 45s timeout).
- offload/openai.rs:143 — strip_think mishandles nested `<think>` → stray `</think>` leaks into answer.
- offload/agent.rs:489 — compact() inserts 2nd role:user → consecutive user turns, strict servers 400/422.
- offload/service.rs:244 — global-cap shrink reports phantom inflated in-flight count.
- process_guard.rs:96 — Windows job HANDLE leaked if SetInformationJobObject fails.
- content.rs:119 — delete_all() drops writers mutex before unlink loop → "Clear content" leaves today's active file.
- notifications/manager.rs:268 — no Idle-suppression guard for pending question (asymmetric w/ permission) → hears "idle" not "awaiting question".
- pty/manager.rs:276 — resize() dedup not atomic with update → concurrent resize-back wrongly suppressed, PTY left at wrong size.
- state/manager.rs:576 — TabRemoved for active tab leaves `active` at dead id → tick marks survivors done-while-away (spurious badge).
- settings/persistence.rs:256 — backup write failure → seeded_defaults() whole session while disk intact; AV lock → repeated blank settings.
- settings/migration.rs:1022 / :1250 — v1.7→1.8 skips TTS-injection enable; v1.11→1.12 leaks avatar.margin into overlay.
- tts/worker.rs:89 — transcription error sets Error but never emit_transcript → overlay stuck loading.
- audio/playback.rs:596 — TappedSource::total_duration() returns remaining not total (Source contract violation; latent).
- audio/playback.rs:337 — StopAll doesn't clear pending_start → spurious Started→Stopped edge after Esc.
- processing/tags.rs:198 — scan_offset not advanced past closed pair when unclosed opener follows → wrong opener recovered.
- processing/tags.rs:520 — strip_ansi CUF multi-param `ESC[5;1C` → 1 space not 5, words fused for phonemizer.
- processing/tags.rs:244 — speak_all emits whitespace-normalized text (newlines→spaces) → lost prosody.
- layout/persistence.ts:133 — validateAndRepairLayout skips active_tab_id-in-filtered check → pane with stale active tab, blank display.
- settings/store.ts:23 — initSettings race: settings-changed between get and listen missed → stale store, window divergence.
- audioStream.ts:46 — void listen() amplitude swallows errors → waveform stuck flat.
- GraphMonitorView.svelte:24 — `paused` always inits false (no watch_paused in GraphStatus) → wrong button label/first click.
- RecordButton.svelte:35 — hold-mode reads live button_mode; mode flip during hold → stuck recording.
- UsageMeter.svelte:38 / SystemStats.svelte:28 — pollMs NaN → setTimeout(,NaN)=0 busy-poll on usage endpoint.
- StatusBarArrangement.svelte:85/97 — missing pointercancel (drag lockout); grabDX not recalced after reorder (jump).
- selectionTts.ts:202 — playSelectionTts floating void → rapid double-click races two TTS sessions.
- TabContextMenu.svelte:183 — menu at raw cursor coords, no viewport clamp → off-screen/unreachable near edges.

## 4. CLEANUP / EFFICIENCY (highlights)

- graph/index.rs:427 search_docs full table scan across FFI per call; :846 remove_file_in_tx 2N round-trips full scans.
- theming/mod.rs:321 themes_list/palettes_list re-scan+parse every IPC call (use OnceLock).
- offload/mcp.rs:493 events_relay rebuilds reqwest::Client every 2s reconnect (~1800/hr).
- offload/tools: run_command.rs:277 partial output on pipe-err returned capped=false; code_search.rs:131 case-sensitive suffix misses .SQL on Windows; mod.rs:86 swallowed root canonicalize; run_command always uses roots[0].
- pty/tasks.rs:269 saw_terminal_bytes block copy-pasted rx vs tick arm; tabs/registry.rs:256 start_tab/restart_tab ~40 dup lines.
- SelectionTtsControls.svelte:20 Svelte-4 `$:` legacy in a runes project (only legacy component).
- tab_lifecycle.rs:197/615 shlex::split().unwrap_or_default() silently → empty args on malformed quote.
- amplitude.rs:9 stale module comment; router.rs:291 vacuous debug_assert; main.rs:295 silent poisoned-lock skip of state manager.

## 5. NOTES on the changes that prompted this review (develop since v0.20.0)
- Status-bar regroup (76def46), AvatarToggleButton delete (83a6b24), broot `-h` (792b198), no-models packaging (9d881b8): no correctness bug found.
- Resize false-notification fix (b91b392): no new bug; one cleanup/low at pty/manager.rs:301 — poisoned-mutex if-let-Ok ignored but `Resized` still sent (add warn!). The rx/tick duplication at pty/tasks.rs:269 predates it.

---
*Generated by a 14-agent Sonnet review fleet. Method: per-area finder agents (8 angles, recall-biased) → dedup → manual triage. Not individually verifier-gated; treat HIGH/MEDIUM as leads to confirm before fixing.*
