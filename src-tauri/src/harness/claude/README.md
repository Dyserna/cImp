# `harness/claude` — Claude Code

Everything cImp knows about **Claude Code specifically**: what it depends on,
what a Claude Code release could silently change, and how to tell. This file is
the human twin of the machine-readable rows in
[`contract.rs`](../contract.rs) and of this directory's `impl HarnessPlugin`
([`plugin.rs`](plugin.rs)); the code is the authority, this is the narrative.

V40 Phase G moved it here from three core documents — `docs/MAINTENANCE.md`'s
drift table, `docs/ARCHITECTURE.md` § *Token Efficiency (V11)*'s hook routing
table and `docs/CHP.md` §§ 4.5 / 6.2 — because none of it is true of harnesses
in general. Those documents now link here. The neutral halves stayed where they
were: the ingress machinery is `harness/ingress.rs`, the protocol is `CHP.md`,
and the harness-neutral `delegation.worker` contract is still a
`docs/MAINTENANCE.md` row.

**Kept in step by tests, not by discipline.**
`harness::contract::tests::matrix_matches_maintenance_doc` reads this file
(and `../opencode/README.md`, and `docs/MAINTENANCE.md`) and asserts that every
capability the registry declares for **this** harness has exactly one row
below — and that no row here names a capability the registry does not have, or
one that belongs to another harness. A new dependency cannot land in the code
and not here, and a combined "both harnesses" row no longer parses.

---

## Drift watch — capability rows

**Claude Code is a user-installed, auto-updating CLI that cImp does not pin.**
Several features depend on undocumented or loosely-documented behaviour
contracts that an update can silently change. Re-run this checklist
periodically and after any noticeable Claude Code update (`claude --version`).

**Capability id(s)** are the join keys into
[`harness/contract.rs`](../contract.rs).

| Capability id(s) | Feature | Contract it depends on | Where wired | Symptom if the contract drifts |
|---|---|---|---|---|
| `claude.hook.user_prompt_submit` | Context injection (V10) | A `UserPromptSubmit` hook of `type: "http"` (**Claude Code ≥ 2.1.63**) POSTs the payload and parses the 2xx JSON reply as it would a command hook's stdout, so `hookSpecificOutput.additionalContext` reaches the model; `$CIMP_HOOK_TOKEN` is substituted into the `Authorization` header from `allowedEnvVars` | `harness/claude/hook.rs` (payload + output shapes), route `/claude/hook/user_prompt_submit` in `harness/claude/hook.rs::ROUTES_TABLE` (registered through `HarnessPlugin::routes()` since V40 Phase C), overlay in `harness/claude/overlay.rs` | Effectiveness "chars injected" keeps growing but injected files are never followed (Advisor follow-rate collapses); agent re-explores constantly. On a CLI older than 2.1.63 the entry is not understood and the capability is simply absent — V35 Phase J generates no command-hook fallback |
| `claude.hook.precompact` | Compaction survival (V11-D) | A `PreCompact` hook of `type: "http"` whose 2xx reply's `additionalContext` reaches the compaction prompt — spike **D0**; outcome recorded in `harness_versions.d0_status` (still `unverified` until run — see the V16 spike recipes below) | `harness/claude/hook.rs`, route `/claude/hook/pre_compact` in `harness/claude/hook.rs::ROUTES_TABLE` (registered through `HarnessPlugin::routes()` since V40 Phase C), overlay in `harness/claude/overlay.rs` | Hard to observe (server-side dedup-clear stays correct regardless); post-compaction re-exploration despite the feature being on |
| `claude.hook.pretooluse_deny` | Read advisor (V11-E) | A `PreToolUse` hook of `type: "http"` can deny only by answering 2xx with `permissionDecision: "deny"` — a non-2xx, a timeout and a refused connection are all non-blocking — and the accompanying `permissionDecisionReason` is surfaced **to the model**: spike **E1**, outcome in `harness_versions.e1_status` (`"fail"` hard-blocks the advisor: Settings toggle disabled + hook never installed) | `harness/claude/hook.rs` (`plan_request`, `deny`), route `/claude/hook/pre_tool_use` in `harness/claude/hook.rs::ROUTES_TABLE` (registered through `HarnessPlugin::routes()` since V40 Phase C), overlay in `harness/claude/overlay.rs` | `drift.read_reason.v1` fires (~100% remind→immediate full re-read = bare refusals); `drift.read_hook_silent.v1` fires (remind counter flatlines while large unchanged files keep being re-read) |
| `claude.hook.posttooluse` | Post-edit checks (V12) | A `PostToolUse` hook of `type: "http"` fires for `Edit`/`Write`/`MultiEdit` **on success** with the documented payload shape and accepts `additionalContext` back. Success-only is right for this row (there is nothing to check after a failed edit); the new `PostToolUseFailure` event is wired for the *sizing* row instead (see the tool-result sizing row below) | `harness/claude/hook.rs`, route `/claude/hook/post_tool_use` in `harness/claude/hook.rs::ROUTES_TABLE` (registered through `HarnessPlugin::routes()` since V40 Phase C) → `/context/post_edit`'s core, overlay in `harness/claude/overlay.rs` | Auto-check diagnostics stop appearing after edit bursts. **Phase A finding 2 CLOSED 2026-08-17:** the route now files `drift.payload.v1` under the token `post_edit_hook` when `session_id`/`cwd`/`tool_name`/`tool_input.file_path` go missing (deliberately NOT the never-shipped `postedit_hook` spelling, which stays unattributed). Still LAGGING only — a hook that stops firing entirely says nothing, and there is no witness that proves an edit should have happened |
| `claude.hook.notification`, `perm.tui_scrape` | Permission detection (NC-2, issue #5) | PRIMARY: `Notification` + `PermissionDenied` hooks of `type: "http"` (observe-only — they answer `{}` on every path; matcher `""` = all notification types, classified app-side by type with prose fallback). Both events reach ONE route, which dispatches on `hook_event_name`. Payload shape is read BOTH flat (`notification_type`/`message`) and nested (`notification: {type, message}`) — the docs are ambiguous, see the UNVERIFIED note carried into `harness/claude/hook.rs`. FALLBACK: the TUI scanner (V2-03) matches the approval prompt's footer *grammar*, not the old literal "Esc to cancel · Tab to amend" — chord labels are user-remappable and the amend segment is conditional, so that literal was retired. `processing/permission.rs` ships two OR'd patterns: `claude_permission` (all_of `to cancel ·` — the cancel hint followed by another segment, which only happens when cancel comes first) and `claude_permission_bare` (all_of `to cancel` + `1. Yes 2.`, the numbered options as corroborating anchor), both with none_of `to select` / `to navigate` so select-menu chrome is vetoed. Both paths feed the same idempotent flag | `harness/claude/hook.rs` → route `/claude/hook/notification` in `harness/claude/hook.rs::ROUTES_TABLE` (registered through `HarnessPlugin::routes()` since V40 Phase C) → `classify_permission_event` (same core `/permission/event` calls); scanner as fallback | `drift.payload.v1` under the token `notify_hook` (required fields missing) — unchanged from the deleted shim, so a pre-upgrade tab's reports land in the same bucket; permission notifications stop firing entirely = both paths broke — recharacterize the fallback via `RUST_LOG=perm_capture=debug`, capture a real hook payload via a `cat > file` Notification hook |
| `claude.flag.settings_overlay`, `claude.statusline.stdin`, `claude.transcript.usage` | Statusline / usage | `--settings` overlay accepted at spawn; statusline stdin JSON carries the `rate_limits` object (account quota → written to `<exe-dir>/claude-usage-push.json`, feeds the usage widget — no network poller) and the `context_window` block (`used_percentage` / `total_input_tokens` / `context_window_size` + cache split → context bar, NC-3, issue #14); transcript JSONL `usage` fields present. **Neither of these two migrated in V35 Phase L, and the reason is upstream's:** no Claude Code hook input carries token counts (the common payload set is `session_id` / `transcript_path` / `cwd` / `permission_mode` / `hook_event_name`; `PostCompact` exposes no compaction metrics either) and none carries a context window or a `rate_limits` block. The only documented token surface is the OpenTelemetry `claude_code.token.usage` metric — a different integration, not a hook. So both stay **Tier C, permanently-until-upstream-changes**; `chp::EVENTS` keeps `session.usage` and `session.context` reserved with no producer rather than deleting them | `harness/claude/statusline.rs` (the `--statusline` subcommand: extract, push and the rendered bar), `harness/claude/usage.rs` (the push file), the transcript tap in `harness/claude/read.rs` | Context bar / quota widget go blank or freeze (a payload with neither `rate_limits` nor `context_window` writes no push); Usage section stops populating |
| `claude.transcript.tool_result`, `claude.transcript.identity`, `claude.transcript.subagents`, `claude.flag.session_id` | Transcript tap — shape beyond usage (V14/V17.1/V24/V34); **two of these are FALLBACKS since V35 Phase L** | The rest of the Claude transcript JSONL contract the tap reads, beside the `usage` block above. `tool_result` and the sub-agent LIFECYCLE are now pushed (see the two Phase L hook rows above) and the corresponding taps here are suppressed for a tab whose CHP hello declares them — but `identity`, `--session-id` pinning, sub-agent TOKEN accounting and the `launch_seen`/`completion_seen` drift bookkeeping are **not** arbitrated and run on every tab, because no hook payload carries any of them. User lines carry `tool_result` content blocks with `tool_use_id` / `is_error` (tool-result sizing, V14); every line carries `sessionId`, `version` (feeds the harness version tripwire), `isSidechain` and `isMeta`; sub-agent traffic appears either inline (`isSidechain: true`) or as `<session_id>/subagents/agent-*.jsonl`, launched by a `tool_use` named `Task` (1.x) or `Agent` (2.x); and `--session-id <uuid>` still pins one tab to one transcript file (V34) — without it two tabs on one project are indistinguishable. | `harness/claude/read.rs` (drain, `SubagentFile`), `tabs/config.rs` (`resolve_oob_source`, `args_select_session`) | Silent, and each differently: tool-result sizes and per-tab identity go blank rather than wrong; `drift.subagent_transcripts.v1` fires when sub-agent traffic is in neither known location; losing `version` *silences* the version tripwire instead of firing it; a rejected `--session-id` reverts to pre-V34 newest-transcript-wins binding (ambiguous, not broken). |
| `claude.hook.stop`, `claude.transcript.assistant_text` | Assistant prose → TTS (V20; **pushed since V35 Phase L**) | PRIMARY: a `Stop` hook of `type: "http"` fires at every turn's end carrying `last_assistant_message` — the complete final assistant text, i.e. the *same unit at the same cadence* the transcript tail delivers, which is what makes the migration cadence-preserving by construction (`MessageDisplay` is deliberately unused: per-chunk deltas on the streaming hot path would change the segmenter's unit). FALLBACK: harness/claude/read.rs::assistant_texts lifts `message.content[]` blocks with `type == "text"` out of an `assistant` line, keyed by `message.id` so one message is not re-spoken every drain tick, and `thinking`/`tool_use` blocks stay distinguishable so reasoning is never read aloud. ARBITRATION: per capability, per tab — the reader's tap is suppressed exactly when that tab's CHP hello declares `assistant_text`, so the two can never both speak; a mid-session switchover (`SessionStart` fires on resume/clear) is closed by the handoff in `tts/prose.rs`, which strips from the first push whatever the reader already said of the same message | `harness/claude/hook.rs` + `harness/claude/overlay.rs` → route `/claude/hook/stop` in `harness/claude/hook.rs::ROUTES_TABLE` (registered through `HarnessPlugin::routes()` since V40 Phase C) → `tts/prose.rs::speak_prose`; fallback `harness/claude/read.rs` | Tab goes MUTE. If the hook broke, `drift.payload.v1` fires under the token `stop_hook` — either because `last_assistant_message` arrived empty, or because the hook stopped firing at all (the Phase L quiet detector, witnessed by three `prompt` pushes with no `Stop` between them). **cImp does NOT fall back to the reader when that happens** — falling back would restore the audio and hide the breakage; restart the tab to re-declare. If the FALLBACK broke instead, the fallback row's own canary + probe fire (see the transcript-tap row) |
| `claude.hook.tool_result` | Tool-result sizing, pushed (V35 Phase L; **errored half added 2026-08-17**) | **TWO all-tools (`""`) `type: "http"` entries, one per outcome**, because `PostToolUse` fires only when a tool SUCCEEDS: `hooks.PostToolUse` carries `tool_name` + `tool_result` (string, or `{type:"text", text}` blocks) and `hooks.PostToolUseFailure` carries `tool_name` + `error`. Both are separate ROUTES from the auto-check entry: its group and the success group both fire for an `Edit`, so one shared route would run the project's checks twice and count one result twice. Both are sized through the transcript reader's own `tool_result_chars`, so that reader's fixture canary is the leading check for every path. The failure route maps to NO CHP event of its own (the capability is `session.tool_result`; a second event would let a rare failure push reset the quiet counter watching the common success entry) and shares its sibling's drift token | `harness/claude/hook.rs` + `harness/claude/overlay.rs` → routes `/claude/hook/post_tool_use_result` and `/claude/hook/post_tool_use_failure` → `UsageEvent::ToolResult` | Tool-result sizes stop being recorded (the reader stays suppressed, on purpose). `drift.payload.v1` under `tool_result_hook`, for a present-but-unsizeable `tool_result`/`error` or for the hook going quiet (witnessed by `context.post_edit` pushes). **Version-skew residual:** `PostToolUseFailure` is newer than the 2.1.63 floor, so a CLI between the two ignores that entry and failed results go uncounted with nothing firing — the quiet detector does not see it, because the success half keeps pushing |
| `claude.hook.subagent` | Sub-agent lifecycle → avatar (V35 Phase L) | `SubagentStart` + `SubagentStop` hooks of `type: "http"` on ONE route (matcher `""` = all agent types), dispatching on `hook_event_name` and keyed by `agent_id` — an id started and not stopped is an agent running. **Lifecycle only:** no hook payload carries sub-agent token counts and none names a sub-agent transcript path, so the transcript sub-agent row (see the transcript-tap row below) keeps reading `<session_id>/subagents/agent-*.jsonl` for the spend on every tab, and its `launch_seen`/`completion_seen` bookkeeping keeps running too (suppressing it would make `drift_condition` report a false "launcher tool renamed") | `harness/claude/hook.rs` + `harness/claude/overlay.rs` → route `/claude/hook/subagent` → `StateSignal::AgentsActiveChanged` | The avatar stops showing sub-agent activity. **No quiet detector, declared:** a session may legitimately launch no sub-agents, so no other push proves one should have been reported — `drift.payload.v1` under `subagent_hook` covers the malformed-payload half only |
| `claude.hook.taint_beacon`, `claude.hook.checkpoint_beacon` | The two `PreToolUse` beacons (V32 Phase F / V33 Phase F) — **TCB rows**, migrated Tier **D → B** on 2026-08-17 | Two `type: "http"` `PreToolUse` entries carrying `{session_id, cwd, tool_name}`: matcher `WebFetch\|WebSearch` → `/claude/hook/pre_tool_use_taint`, matcher `Edit\|Write\|MultiEdit\|Bash` → `/claude/hook/pre_tool_use_checkpoint`. **They were Tier D because each rested on an UNDOCUMENTED behaviour of `type: "command"` hooks** — that a hook writing nothing and exiting 0 is non-blocking *including on timeout*, and that the tool does not begin until the hook process exits. The http contract states both in writing (verified against 2.1.233, 2026-08-17): a non-2xx, a timeout and a refused connection are non-blocking, blocking is expressible ONLY as 2xx plus a decision field, and a `PreToolUse` hook blocks the tool call until the response — which is what makes `permissionDecision: "deny"` expressible and therefore what makes the checkpoint's ordering a documented guarantee. Both are still report-only and now structurally incapable of denying (V32 locked decision 14): their handlers emit no decision field. The checkpoint entry's `timeout` is 5 s, a ceiling over the app's own 1800 ms snapshot budget — the handler awaits the snapshot before answering. `cimp --taint-beacon` / `cimp --checkpoint-beacon` are deleted; the flags survive in `main.rs` as stdin-draining tombstones so a pre-upgrade tab's overlay cannot launch a second GUI per call. | `harness/claude/hook.rs` + overlay entries in `harness/claude/overlay.rs` → routes in `harness/claude/hook.rs::ROUTES_TABLE` → `latch_beacon_core` (`/latch/beacon`'s own core) and `tool_checkpoint_core` (`/workbench/tool_checkpoint`'s) | `drift.payload.v1` under the tokens `taint_beacon` / `checkpoint_beacon` — unchanged by the migration, so a tab still running the old shim lands in the same bucket, and since V35 Phase I both resolve to these rows instead of the un-attributed channel. **No quiet detector, declared:** a turn may legitimately never `WebFetch` and never edit, so no witness proves either should have fired — and both events also have an OpenCode producer these Claude-named tokens would misattribute. Otherwise **silent**: a beacon that stops firing leaves a tab's EXTERNAL latch unengaged (the proxied half still catches anything routed through cImp), and a checkpoint that stops firing loses per-call rewind points while the prompt-level ones remain. A blown snapshot budget is *not* silent — it writes its own `workbench` / `checkpoint_missed` Activity event. **A tab open across the upgrade reports `old_plugin` and has neither beacon until it is restarted.** |
| `claude.transcript.stop_reason` | Cross-harness delegation — the fallback reader's TURN boundary (V39 review HIGH-1) | An assistant transcript line carries `message.stop_reason`, and its value tells a turn that CONTINUES from one that is OVER: `tool_use` means the model paused to call a tool, anything else non-null (`end_turn`, and the rarer `max_tokens` / `stop_sequence`) means it stopped talking. One API message is written as SEVERAL transcript lines — one per content block — all repeating that message's stop reason, so the turn's final text can follow the line that declared the turn over; the reader therefore files at the end of a drain pass, not on the first `end_turn` line it sees. An UNRECOGNIZED value ends the turn, which is the fail-toward-answering direction | `harness/claude/read.rs::is_turn_end` + `TurnText`, feeding `delegation::note_assistant_text` through `OobContext::note_turn_text` | **Nothing visible in the tab.** Every line still parses, every message is still spoken, and only a driver waiting on a delegation finds out: no completion is ever filed, so the flight runs to the configured delegation timeout (ten minutes by default) and mints a `timeout` row for a turn that ended in seconds. A tab whose `Stop` hook pushes `assistant_text` is unaffected — the push core files the completion and this reader is suppressed (the `Stop`-hook row above is the named fallback). Caught by this row's own canary + live probe (fixture `transcript.stop-reason.jsonl`, drift model `_synthetic/stop-reason-renamed.jsonl`) |
| `claude.input.profile` | How a turn is TYPED into this harness's TUI (V39, the push half of locked decision 16) | A bracketed paste (`ESC [ 200 ~` ... `ESC [ 201 ~`) lands in the composer as **one literal insertion** — embedded newlines become newlines in the buffer, not submits — and a CR written after a short settle submits that buffer as **exactly one turn**. This TUI is known to enable bracketed paste (private mode 2004; `src/lib/terminals.ts` passes it through while swallowing mouse tracking). Everything past that is undocumented: the settle window and paste bound in `harness/claude/input.rs` are **floors chosen from the failure they prevent, not measurements**. **Spike (input-profile), outcome in `Settings.harness[<id>].input_profile_status`**, read by the cross-harness delegation worker gate (the neutral row in `docs/MAINTENANCE.md`) — same fail-closed reader as E1/D0 (`contract::spike_status_blocks`), so anything unrecognized blocks | `harness/claude/input.rs` (the values), `harness/plugin.rs` (`InputProfile` / `PasteMode` and the `input_profile()` trait method — there is no per-harness lookup table any more), consumed by `delegation/engine.rs` | **Silent if it drifts and the spike is not re-run** — a TUI that split a paste into two turns would send the worker a truncated question, which it would answer perfectly. That is why this row fails closed on a recorded `"fail"` rather than degrading. **Each maintenance run (and after any visible Claude Code update): re-run V39 live-verify 1 and 2** (delegate a two-line task and confirm in the worker's own transcript that it arrived as ONE turn, verbatim, with no `[Pasted text]` placeholder), and record the outcome |

**How to check (~10 min):** open a Claude tab with `context_injection` (and,
where enabled, `read_advisor`) on, run a couple of prompts against a large
already-read file, and watch (a) the Code Intelligence → Usage Effectiveness
counters move, (b) Activity logging `remind` events *without* an immediate
identical full `Read` right after, (c) the status-bar context/usage line
populating. Any drift: re-run the spike recipes below before trusting the
feature again.

## Version pins

| What | Pinned at | Verified through | Why it matters |
|---|---|---|---|
| `type: "http"` hooks | **2.1.63** | **2.1.233** (2026-08-17) | The floor for the whole hook ingress. Phase J is a hard switch — the overlay generates no command-hook fallback — so an older CLI gets entries it does not understand and every hook capability is simply *absent*. |
| Transcript / statusline shapes | **2.1.232** | 2.1.232 | The captured fixtures under [`fixtures/harness/claude/2.1.232/`](../../../fixtures/harness/claude/2.1.232) are what the canaries assert against. |
| TUI permission grammar | **2.1.221** | 2.1.221 | The scrapes under [`fixtures/harness/claude/2.1.221/tui/`](../../../fixtures/harness/claude/2.1.221/tui) are real screens captured with `RUST_LOG=perm_capture=debug`; `perm.tui_scrape` is the fallback detector's contract. |
| `PostToolUseFailure` | newer than 2.1.63 | 2.1.233 | A CLI without it ignores the entry, so *failed* tool results go uncounted while successes keep flowing. Nothing reports it — an absent hook event cannot. |

`harness_versions` in the global `settings.json` records what was last **seen**
and last **verified** *per harness* (V40 Phase B moved the pairs into
`Settings::harness`), and the drift advisor's `version_signature` is per
harness too — so this harness's version notice re-fires independently of any
other's, and a notice dismissed before the V40 upgrade re-fires once after it.

## Hook routing

Claude hooks are `type: "http"` (V35 Phase J). They used to be five shim
binaries (`cimp --context-hook` and friends) whose whole job was to carry a
payload from stdin to the loopback and a reply back to stdout. Claude Code
2.1.63's http hooks let the harness POST that payload itself and parse the 2xx
JSON reply exactly as it parses a command hook's stdout, so the shims were
deleted and their payload mechanics moved into [`hook.rs`](hook.rs).

**Since V40 Phase C this table is registered by the plugin, not by core.** It
is `harness::claude::hook::ROUTES_TABLE`, returned from
`HarnessPlugin::routes()`; the loopback's router matches its own CHP-neutral
arms first and appends every registered plugin's routes after them, so a plugin
can neither shadow a core route nor add one core does not enumerate.

| Hook event | Route | Feeds |
|---|---|---|
| `UserPromptSubmit` | `POST /claude/hook/user_prompt_submit` | `/context/retrieve`'s core (V10) |
| `PreCompact` | `POST /claude/hook/pre_compact` | `/context/compaction`'s core (V11 Phase D) |
| `PreToolUse` (matchers `Read`, `Bash`) | `POST /claude/hook/pre_tool_use` | `/context/should_read`'s core (V11 Phase E) |
| `PostToolUse` (matcher `Edit\|Write\|MultiEdit`) | `POST /claude/hook/post_tool_use` | `/context/post_edit`'s core (V12 Phase F) |
| `Notification` + `PermissionDenied` (both `matcher: ""`) | `POST /claude/hook/notification` | `/permission/event`'s core (NC-2) |
| `SessionStart` | `POST /claude/hook/session_start` | CHP hello (V35 Phase J) |
| `Stop` | `POST /claude/hook/stop` | `assistant_text` → TTS (V20; pushed since V35 Phase L) |
| `PostToolUse` (matcher `""`) | `POST /claude/hook/post_tool_use_result` | `/session/tool_result`'s core (V35 Phase L) |
| `PostToolUseFailure` (`matcher: ""`) | `POST /claude/hook/post_tool_use_failure` | `/session/tool_result`'s core, errored half (2026-08-17) |
| `SubagentStart` + `SubagentStop` (one route, `matcher: ""`) | `POST /claude/hook/subagent` | sub-agent lifecycle → avatar (V35 Phase L) |
| `PreToolUse` (matcher `WebFetch\|WebSearch`) | `POST /claude/hook/pre_tool_use_taint` | `/latch/beacon`'s core (V32 Phase F; http since 2026-08-17) |
| `PreToolUse` (matcher `Edit\|Write\|MultiEdit\|Bash`) | `POST /claude/hook/pre_tool_use_checkpoint` | `/workbench/tool_checkpoint`'s core (V33 Phase F; http since 2026-08-17) |

The **legacy `/context/*` routes stay**: a tab open across the upgrade is still
running an overlay full of command hooks, and the retired dispatch flags survive
in `main.rs` as tombstones that drain stdin and exit 0 so an old overlay is inert
rather than launching a second cImp GUI. Both transports meet at one shared core
per capability.

**No Claude hook is a command any more.** The last two shim binaries —
`cimp --taint-beacon` and `cimp --checkpoint-beacon` — became `type: "http"`
entries on 2026-08-17, which moved their registry rows from Tier D to Tier B:
both had been built around *undocumented* behaviours of a command hook (a silent
exit-0 hook never perturbs the call, including on timeout; the tool does not
begin until the hook process exits), and the http contract states both in
writing. In particular a `PreToolUse` http hook blocks the tool call until the
response, so the checkpoint handler simply takes the snapshot before it
answers — the ordering is enforced rather than inferred.

**All of them fail open.** For an http hook that is the harness's own contract:
a timeout, a refused connection and any non-2xx are non-blocking, and a 2xx JSON
body with no directive is a no-op — so every handler answers `200 {}` when it
has nothing to say. Every emitted entry carries an explicit `timeout: 1`, the
deleted shims' 600 ms budget rounded up — with one documented exception, the
pre-mutation checkpoint's 5 s, a ceiling over the app's own 1800 ms snapshot
budget (`tool_checkpoint_budget()`, derived from the shipped plugins' declared
hook reply timeouts rather than hand-written) — pinned by a test rather than
inherited from the harness's 600 s / 30 s defaults.

**Every hook carries its cImp tab** (#48, finding M-7) — `X-CIMP-Tab` in the
emitted headers. A Claude hook payload names `session_id` and `cwd` and nothing
that identifies a cImp tab, and the `/context/*` routes need a tab to resolve
the V32 taint latch scope against. Three of those routes (`compaction`,
`should_read`, `post_edit`) gate on it: under an EXTERNAL latch `post_edit` will
not run the project's configured checks and `should_read` will not return source
text, each answering with its own fail-safe rather than an error. A caller that
sends no tab resolves no scope and is admitted — the same locked fail-open every
tool-serving loopback route takes.

**Permission detection is hook-primary, regex-fallback (NC-2).** The
`Notification` / `PermissionDenied` pair has no toggle and no schema entry of
its own; it is injected whenever `Settings::loopback_needed()` holds — i.e.
whenever the loopback it POSTs into actually runs (offload / graph / Code
Audit MCP). That gate is load-bearing (H2, 2026-08-05 review): without it a
default install spawned a hook process per Claude notification whose POST had
nowhere to land, so the *primary* signal was dead and silent. Since V35 Phase J
the gate is structural as well as deliberate: an http hook bakes its URL at
spawn, so with no loopback there is nothing to emit. The consequence is
unchanged — **a feature-less install runs regex-only permission detection** —
and the injection carries a `spawn_inject_sig` entry (`notify_hooks`) so
enabling one of those features raises the restart hint.

The route forwards the payload to `/permission/event`'s core, which classifies
it (`notification_type == "permission_prompt"` ⇒ detected; the other documented
types, `idle_prompt` included, are ignored *without* consulting the prose; an
absent **or unrecognized** type falls through to permission-flavoured prose
matching, so payload-shape drift degrades to "read the message" rather than to
silence — M12; `PermissionDenied` ⇒ resolved), maps it to a tab (`session_id` →
`transcript_path` → unique `cwd`, else DROP — never a guess), and emits the same
neutral `PermissionPromptDetected` / `PermissionPromptResolved` CHP events the
TUI-regex detector (`processing::permission`, fed this harness's pattern rows
from [`prompts.rs`](prompts.rs)) emits. Both producers feed the one idempotent
`awaiting_permission` flag, so the hook simply usually wins the race; the regex
path is untouched and still covers a dropped or missed event. A hook-driven
*resolve* additionally force-clears that tab's regex latch, because the detector
is edge-triggered: an auto-denial landing while a real approval prompt is still
on screen would otherwise clear the badge with nothing able to re-raise it (M11).

## CHP — event → route table

**Additive extension, `chp` unchanged when these landed.** Twelve routes that
take Claude Code's own hook-input JSON verbatim rather than a CHP body. They
are not a second body shape on an existing route (compatibility rule 4 forbids
that) and they are not in `CHP.md` § 5's vocabulary: they are a *transport* for
events that already have ids. The handler answers a neutral `HookReply` — a
status and a body — which core writes without reading, so the *Answers* column
below is this harness's envelope and stays inside this directory.

`POST /permission/event` is in `ROUTES_TABLE` too: it carries neither `agent`
nor `tab` because the only thing that has ever posted to it is this harness's
pre-Phase-J `--notify-hook` shim.

| Claude event | Route | CHP event it feeds | Answers |
|---|---|---|---|
| `UserPromptSubmit` | `POST /claude/hook/user_prompt_submit` | `prompt` | `hookSpecificOutput.additionalContext`, or `{}` |
| `PreCompact` | `POST /claude/hook/pre_compact` | `context.compaction` | `hookSpecificOutput.additionalContext`, or `{}` |
| `PreToolUse` (`Read`, `Bash`) | `POST /claude/hook/pre_tool_use` | `context.should_read` | `permissionDecision: "deny"` + reason, or `{}` |
| `PostToolUse` (`Edit\|Write\|MultiEdit`) | `POST /claude/hook/post_tool_use` | `context.post_edit` | `hookSpecificOutput.additionalContext`, or `{}` |
| `Notification`, `PermissionDenied` | `POST /claude/hook/notification` | `permission.event` | always `{}` (observe-only) |
| `SessionStart` | `POST /claude/hook/session_start` | `hello` | always `{}` |
| `Stop` | `POST /claude/hook/stop` | `assistant_text` | always `{}` (observe-only) |
| `PostToolUse` (matcher `""`) | `POST /claude/hook/post_tool_use_result` | `session.tool_result` | always `{}` (observe-only) |
| `PostToolUseFailure` (matcher `""`) | `POST /claude/hook/post_tool_use_failure` | **none of its own** — the errored half of `session.tool_result` (see below) | always `{}` (observe-only) |
| `SubagentStart`, `SubagentStop` | `POST /claude/hook/subagent` | `session.subagent` | always `{}` (observe-only) |
| `PreToolUse` (`WebFetch\|WebSearch`) | `POST /claude/hook/pre_tool_use_taint` | `taint.beacon` | always `{}` (report-only) |
| `PreToolUse` (`Edit\|Write\|MultiEdit\|Bash`) | `POST /claude/hook/pre_tool_use_checkpoint` | `checkpoint.pre_mutation` | always `{}` (report-only) — **answered only after the snapshot is taken** |

**The failure half maps to no CHP event on purpose.** Two ids that can never be
declared independently are one id: the failure entry is emitted from the same
per-tab boolean as the success entry, feeds the same core, the same consumer,
the same drift token and the same `served` predicate, so there is no per-tab
decision a second event could report. What a second mapping would cost is
concrete — the quiet detector **resets** a served capability's counter on each
push of it, so a rare failure push would silently rearm the detector watching
the common success entry.

## CHP — hook-body identity

**Identity rides headers, because a hook's body is the harness's.** cImp gets no
field in a Claude hook payload, so `CHP.md` § 3's envelope is carried alongside
it. Core does not special-case this harness to find out: the plugin declares it
through `HarnessPlugin::identity_of_request()`, and a harness whose body *can*
carry an envelope simply does not implement it.

| Header | Meaning |
|---|---|
| `Authorization: Bearer $CIMP_HOOK_TOKEN` | The launch token, substituted by the harness from its own environment. The variable **must** be named in the entry's `allowedEnvVars` or it substitutes to the empty string; cImp sets it on the Claude child at spawn rather than baking a literal into the `--settings` argv value. |
| `X-CIMP-Tab` | The baked tab id. Caller-asserted and validated against the user's configured Claude tabs before anything is recorded. |
| `X-CIMP-Agent` | This harness's registered id. |
| `X-CIMP-Chp` | `CHP.md` § 3's `chp`, substituted from `CHP_VERSION` at generation. |
| `X-CIMP-Hello` | `SessionStart` only: `{"serves":[…],"cannot":[{id,why}…]}`, computed from the booleans that decided what this tab's overlay actually wired. |

**Why this harness's hello omits `harness_version`.** The hook-input contract
has no CLI version field — the common set is `session_id`, `transcript_path`,
`cwd`, `permission_mode`, `hook_event_name` — so the `SessionStart` handler
reads a top-level `version` opportunistically (absent in every shape documented
today) and leaves `harness_version` empty when it finds none. cImp learns the
version from the transcript's own top-level `version` instead
([`read.rs`](read.rs)'s `cli_version_of`), recorded per harness in
`Settings::harness`.

## Usage tap — residual limitations

*Architecture: see `docs/ARCHITECTURE.md` § Workflow & Visibility (V14).*

- **Sub-agent transcripts have moved once already and could move again.** Two
  layouts are handled (1.x inline `isSidechain:true` lines; 2.x
  `…/<session_id>/subagents/agent-<id>.jsonl` with the launcher tool renamed
  `Task` → `Agent`). `SubagentState::drift_tick` raises
  `drift.subagent_transcripts.v1` for "transcripts moved" or "launcher tool
  renamed", but a **simultaneous rename and relocation is invisible** from that
  vantage. If sub-agent-heavy sessions ever look suspiciously cheap with no
  canary firing, diff a live session's transcript directory against the two
  known layouts first.
- **`<synthetic>` is a fabricated model id**, stamped on messages this harness
  generates locally (errors, interrupts). Nobody was billed for one, so it is
  declared as a model sentinel by this plugin's usage source and excluded from
  "which model ran this session" — core holds no such literal.

## Memory scoping

*Architecture: see `docs/ARCHITECTURE.md` § Code Intelligence — Context Engine
(V10), "Memory-tool session scoping".*

The `context_recall` / `context_note` / `context_notes` MCP tools resolve a
session scoped to the calling harness **and** to the calling **tab** (V28,
issue #13).

**No harness passes a session id into an MCP server's tool-call context** —
Claude Code gives hooks a `session_id` but gives its MCP children no arg, no env
var and no `tools/call` field. V28 sidesteps that: the `--offload-mcp` child is
per **tab** and cImp composes its argv, so `--tab <tab-id>` is baked at spawn and
the *app* resolves tab → session at call time from the live-session registry.
Stop watching for "session id inside MCP tool calls"; it is no longer
load-bearing.

**What to re-check instead:**

- **`--session-id` is a request, not a guarantee (V34, 2026-08-09).** Per-tab
  identity for two Claude tabs on ONE project rests on cImp pinning each tab's
  session at spawn (`claude --session-id <uuid>`) and the transcript being named
  `<session-id>.jsonl`. **A tab does not always run under its pin** — observed
  in the field on tabs carrying no `--resume`/`--continue` at all — so the pin
  is verified against the transcript's existence before anything is published
  from it, and a tab that never gets its pinned file simply runs as it did
  pre-V34. Degraded, never broken, and never a false identity claim.
  Unpinned IS a supported state; what would be a real regression is a
  pinned-but-unwritten tab going QUIET (no TTS, no usage, no memory) — that
  means the verification turned back into a wait. Check `--session-id` is still
  in `claude --help` on each harness upgrade (it is one of this plugin's
  declared `session_selector_flags()`), and run V28 live-verify recipe b3.
- **The tab → session registry must stay fed.** This harness stamps it from the
  transcript drain tick ([`read.rs`](read.rs)); its declared
  `session_key_space()` is the **tab** id, which is why it can be keyed that way
  at all. If the event shape changes, resolution silently degrades to the
  pre-V28 recency behavior — **no error, no log**. The tell is per-tab isolation
  quietly stopping; verify with the two-tab recipe (a `context_note` in tab A
  must not appear in tab B's `context_recall`).
- **Fail-open is deliberate and total:** missing `--tab`, unknown key, TTL-stale
  entry, blank value → the harness-scoped current session. Never turn any of
  these into a tool error; a memory read is not worth breaking a turn over.

## Open spikes & unverified contracts

| Spike | What it verifies | Status | Where recorded |
|---|---|---|---|
| **D0** (`PreCompact`) | That a `PreCompact` hook's `hookSpecificOutput.additionalContext` actually reaches the **compaction prompt**. | **unverified** — degrades to a no-op (server-side dedup-clear + post-compaction flag are correct regardless) | `harness_versions.d0_status`; recipe below |
| **E1** (`PreToolUse` deny) | That a `PreToolUse` deny's `permissionDecisionReason` is surfaced **to the model**, not just the user — the whole premise of the read advisor. | **unverified** — gate fails closed; a recorded `"fail"` disables the Settings toggle and blocks the hook install | `harness_versions.e1_status`; recipe below |
| **F0** (`PostToolUse`) | Which JSON field of a `PostToolUse` hook's reply reaches the model as additional context. | **unverified** — degrades safely; a parked block still drains via the next `/context/retrieve`, and `auto_check` defaults off | `TODO(spike F0)` in [`hook.rs`](hook.rs); narrative in `docs/ARCHITECTURE.md` § V12 |
| **`Notification` payload shape** | Whether the `Notification` hook payload is flat (`{notification_type, message}`) or nested (`{notification: {type, message}}`) — the reference docs render both ways. The handler reads BOTH spellings; the classifier falls back to prose matching, then the TUI-regex scanner backstops the whole path. | **unverified** — degrades gracefully (never to silence), but the parser carries double-read complexity until settled | Capture recipe in [`hook.rs`](hook.rs)'s module doc; runs naturally alongside the issue #5 live-verify |
| **Input profile** | That a bracketed paste plus a settle plus CR yields exactly ONE turn in this TUI. | **unverified, manual only** — no fixture and no probe can settle a behaviour visible only as a real turn in a real TUI; declared in `declared_unprobed()` | `Settings.harness[<id>].input_profile_status`, read by the cross-harness delegation worker gate (the neutral row in `docs/MAINTENANCE.md`) |
| **`PreToolUse` timeout semantics** | Whether a TIMED-OUT `PreToolUse` hook blocks the tool call. The hooks reference gives the exit-code table and the `timeout` field but never says. | **undocumented upstream** — the taint beacon is built not to depend on it (80 ms dispatch, never reads the reply); a harness change that made a timeout blocking would turn the `sensor` beacon into a silent deny | `docs/MILESTONE-V32-injection-hardening.md`; `harness/claude/hook.rs`'s beacon route |
| **`--settings` overlay key set** | That the overlay's key set (`hooks` + `statusLine`, plus `permissions` in native-web `deny` mode) is still accepted. | **unguarded** — an upstream key rename or a stricter schema breaks the overlay SILENTLY, and the deny-mode key set has no test (V32 accepted residual) | `harness/claude/overlay.rs`; V32 accepted residuals |
| **Version tripwire** | That the currently-installed build still honours every contract above. | **re-armed on every version change** — `drift.harness_version.v1` fires until re-verified, and the signature is **per harness** | The Harness health panel's **Mark verified** action, which takes the harness from the row clicked |

**Spike recipes** (record outcomes in `harness_versions.{e1_status,d0_status}`
in the global `settings.json`):

- **E1 (read advisor deny reason reaches the model).** With the app running and
  `graph.enabled` + `graph.read_advisor` on, open a Claude tab in a project with
  a large indexed file. Have the agent `Read` the file twice in one session
  (second read unchanged). On the second read the hook denies with the outline
  reminder. **Pass:** the model's next message references the outline content
  (it *acts on* the reminder — answers from it, or targets a specific symbol
  next). **Fail:** the model reports a bare permission refusal and immediately
  retries/hits the same wall (check the transcript JSONL for what the model
  actually received). Record `"e1_status": "pass"` or `"fail"`; `"fail"`
  disables the Settings toggle and blocks the hook install until changed back
  after a harness update. A hand edit takes effect on the next tab
  launch/restart and in a freshly opened Settings window — no app restart
  needed. Anything other than `"unverified"`/`"pass"` (any casing) is treated as
  a failure — the gate fails closed on typos.
- **D0 (PreCompact additionalContext reaches the compaction prompt).** With
  `compaction_context` on, run a session up to a `/compact` (manual is fine).
  **Pass:** the post-compaction summary retains working-set files / pinned notes
  fed by `/context/compaction` (compare against the block the route returned —
  visible via `RUST_LOG=debug`). **Fail:** summary shows no trace of it. Record
  `"d0_status"` accordingly (informational — a fail degrades to a no-op).
- **Input profile.** Delegate a two-line task into a Claude worker tab and
  confirm in the worker's own transcript that it arrived as ONE turn, verbatim,
  with no `[Pasted text]` placeholder (V39 live-verify 1 and 2).

## Native tools

This harness's native tool vocabulary — the capitalized `Read` / `Edit` /
`Write` / `MultiEdit` / `NotebookEdit` / `Grep` / `Glob` / `Bash` /
`WebFetch` / `WebSearch` family — is [`tools.rs`](tools.rs), returned from
`HarnessPlugin::native_tools()`. Core never holds a second copy: tool
classification, memory-event kinds, the `mutates_fs` decision behind the
pre-mutation checkpoint and the web-tool list all resolve through
`harness::native` with the *request's* harness, and a tool from a source cImp
cannot identify is treated as mutating rather than borrowed from here.

`docs/HARNESS-NATIVE-TOOLS.md` is the user-facing twin of that table and is
compared against it by a test, so the two cannot drift.

## Input profile

[`input.rs`](input.rs) holds this harness's `InputProfile` — paste encoding,
submit bytes, settle window and paste bound — returned from
`HarnessPlugin::input_profile()`. The type is neutral
([`../plugin.rs`](../plugin.rs)); only the values are this harness's, and they
are **floors chosen from the failure they prevent, not measurements** (see the
input-profile spike above).
