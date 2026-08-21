# V40 appendix — harness-specific residue outside `harness/` (the V35 leftovers ledger)

**Status:** worklist for `docs/MILESTONE-V40-harness-registry.md` (sweep of 2026-08-21/22; line numbers are as of develop `79c2d9c`). Every row is something true of Claude Code or OpenCode specifically that still lives in cImp core. The milestone's decisions 18–29 say where each area goes; this file is the row-level brief for the implementation agents and is retired (deleted) when the milestone closes — the layering tests are the durable record.

Companion to the first inventory (identity enums, settings pairs, spawn composition, grant tables, `spawn_inject_sig`, hook route table, tool tables, canaries/probes, frontend mirrors), which the milestone's decisions 1–17 already cover and which is **not** repeated here.


**Scope note / correction to the V40 doc's framing.** The doc says `graph/service.rs` "carries 128 of them". Actual count is **196 matching lines, of which only 14 are production code** (`service.rs:953, 960, 962, 1030-1031, 1871, 1879, 3061, 3251-3275`); the rest are doc comments and `#[cfg(test)]`. The same ratio holds across `graph/`: `index.rs` 127 matches → **5** production lines; `mcp.rs` 89 → **5**. The real density is elsewhere — `offload/loopback.rs` (47 production findings), `SettingsApp.svelte` (30), `settings/types.ts` (16), and the `usage/`+`statusline/` chain.

Everything below **excludes** the V40 plan's list (identity enums/fns, settings field pairs, `tabs/config.rs` spawn composition, sandbox grant tables, `spawn_inject_sig`, `/claude/hook/*` route table, `toolclass.rs` native tables, `memory_kind_of`, canaries/probes, frontend tab-id/label/Settings mirrors).

---

## A. `graph/`

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `graph/service.rs:953-964` | `live_claude_tab_sessions` — filters the live-session registry on `e.agent == "claude"` and `tab_binding_is_ambiguous(…, "claude", …)` | claude | identity literal | `HarnessPlugin::session_key_space()` + neutral `live_sessions_for(HarnessId)` | needs a neutral type |
| `graph/service.rs:1871-1879` | `GraphService::live_claude_sessions()` — public API named after one harness; the `/permission/event` resolver's only input | claude | identity literal | neutral `live_sessions_for(harness)` | mechanical |
| `graph/service.rs:1030-1031, 3251-3275` | `claude_sessions` / `claude_tokenless` cached drift counters on a core struct | claude | payload shape | `HarnessPlugin::usage_source()` → per-harness counts | needs a neutral type |
| `graph/service.rs:3056-3061` | `session_agent(sid).unwrap_or_else(\|\| "claude".to_string())` — a memory-event row silently attributed to Claude when the lookup misses | claude | identity literal | `harness::DEFAULT_HARNESS`, or refuse | mechanical |
| `graph/index.rs:4214-4234` | `claude_tokenless_sessions()` — a **CozoDB Datalog query with `agent == "claude"` embedded in the query string** | claude | payload shape | `harness/claude/read.rs` via `usage_source()` | needs a neutral type |
| `graph/index.rs:3687-3708, 3725-3747` | `"<synthetic>"` — Claude Code's pseudo-model sentinel, filtered in two core query methods | claude | payload shape/parsing | `HarnessPlugin::model_sentinels()` | mechanical |
| `graph/memory.rs:749-760` | `classify_tool` — a hardcoded merged table of Claude's `Read/Edit/Write/MultiEdit/NotebookEdit/Grep/Glob/Bash` **and** OpenCode's `read/edit/write/patch/grep/glob/list/bash` (this is *adjacent to* but distinct from `memory_kind_of`) | both | payload shape | `HarnessPlugin::native_tools()` | mechanical |
| `graph/memory.rs:461-463, 502-520` | `UsageOrigin::{Session,Agent}` — the sub-agent lane concept, defined by "`<sid>/subagents/*.jsonl` or inline `isSidechain:true` lines" | claude | payload shape | harness-declared origin list; CHP `turn.usage.origin` | needs a neutral type |
| `graph/mcp.rs:326` | `tools() = tools_for("claude")` — the app-side surface measurement reports the Claude view by default | claude | identity literal | registry default | mechanical |
| `graph/mcp.rs:976` | `execute`'s `source` param documented as the free-string agent tag `"claude"`/`"opencode"`/`"offload"` | both | identity literal | `HarnessId` | mechanical |
| `graph/secrets.rs:441`, `graph/secrets.yar` | `secret_anthropic_api_key` YARA rule | (vendor) | identity literal | see (d) — vendor, not harness | mechanical |
| `graph/service.rs:168, 996`; `ipc/commands.rs:2588`; `graph/shellread.rs:6`; `offload/loopback.rs:1073, 2164, 11454, 15527, 16602`; `state/manager.rs:325`; `pty/tasks.rs:62` | **Stale module paths** in doc comments: `oob::claude::*` and `harness::claude_hook` — modules that moved in V35 Phase K | claude | identity literal | doc fix only | mechanical |

`graph/shellread.rs` is genuinely neutral (a shell whole-file-read parser); only its header comment names Claude's hook.

## B. `usage/` + `statusline/` — Claude's statusline schema mirrored into core and out to the UI

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `usage/mod.rs:1-50` | Module is *defined* as "Claude Code subscription usage tracker"; the whole push protocol | claude | payload shape | `harness/claude/statusline.rs` via `usage_source()` | behaviour-bearing |
| `usage/mod.rs:67-108` | `UsageWindow` / `UsageSnapshot.{five_hour, seven_day}` — Anthropic's two subscription windows as *field names* | claude | payload shape | neutral `windows: Vec<QuotaWindow>` | needs a neutral type |
| `usage/mod.rs:120-181` | `ContextSnapshot` — verbatim mirror of Claude's `context_window` + `current_usage.*` + `agent.name` / `effort` / `thinking` / `fast_mode` | claude | payload shape | neutral `ContextReading` on a CHP event | needs a neutral type |
| `usage/mod.rs:218` | `const PUSH_FILE: &str = "claude-usage-push.json"` | claude | file path | `harness/claude/` | mechanical |
| `usage/mod.rs:315-325` | `PushMeta.session_key` — "`session_id`, else the transcript path, else the session name" | claude | payload shape | `HarnessPlugin::session_key_space()` | mechanical |
| `usage/mod.rs:661-884` | **`mod endpoint_poll`, `#[allow(dead_code)]` but compiled**: `https://api.anthropic.com/api/oauth/usage`, `anthropic-beta: oauth-2025-04-20`, `~/.claude/.credentials.json`, `claudeAiOauth.accessToken` | claude | file path/config reader | `harness/claude/` or delete | mechanical |
| `statusline/mod.rs:57-64` | `launch_command()` → the `statusLine.command` string for Claude's `--settings` overlay | claude | CLI flag | `harness/claude/statusline.rs` | mechanical |
| `statusline/mod.rs:76-140` | `shell_safe_path` + Win32 `GetShortPathNameW` — exists solely because Claude Code runs the statusline through an unknown shell | claude | behaviour | same | behaviour-bearing |
| `statusline/mod.rs:146-160` | `run()` — the `--statusline` stdin/stdout contract | claude | CLI flag | same | mechanical |
| `statusline/mod.rs:180` | Model-name fallback literal `"Claude"` | claude | UI string | plugin-supplied | mechanical |
| `statusline/mod.rs:300-325` | Reads `<exe-dir>/settings.json` out-of-process; palette fallback `"OpenCode Grey"` | opencode | UI string | see (d) | mechanical |
| `main.rs:241-242` | `--statusline` subcommand dispatch | claude | CLI flag | plugin-registered subcommand | mechanical |
| `ipc/commands.rs:567-573` | IPC command **named** `get_claude_usage` | claude | identity literal | `harness_usage` | mechanical |

## C. `settings/` (beyond the planned field pairs)

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `settings/schema.rs:953-1032` | `default_llm_pricing()` — **8 Anthropic rows with `claude-*` `model_prefix` values** (`claude-fable-5`, `claude-opus-5/4-8/4-7/4-6`, `claude-sonnet-5/4-6`, `claude-haiku-4-5`) + 8 Copilot rows; the doc pins cache-write to "the 1-hour-TTL 2× rate **Claude Code sessions** use" | claude | payload shape | `HarnessPlugin::default_pricing()` — or a separate `pricing/` seam, see (d) | behaviour-bearing |
| `settings/schema.rs:1060-1076` | `pricing_rows_since()` — a versioned top-up watermark shipping one more `claude-opus-5` row | claude | payload shape | same | mechanical |
| `settings/schema.rs:250-253, 1497-1499` | `ClaudeLocalSettings` → `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_MODEL` env synthesis contract | claude | file path/config writer | plugin `ext` (decision 6) + `compose_env` | behaviour-bearing |
| `settings/persistence.rs:2100-2104` | Integrity repair: an empty `enabled_ai_tabs` is **forced back to `[claude]`** — a specific harness is load-bearing for boot | claude | identity literal | registry `default_enabled` | mechanical |
| `settings/persistence.rs:2078` | `const AI_BUILTIN_IDS: [&str; 3]` — fixed arity | both | identity literal | registry view | mechanical |
| `settings/injection.rs:223-255, 1338-1356` | `Feature::OpencodeNativeGate` — a core injection-feature enum variant that only one harness has, plus `Consumer::applies` arms explaining which half of the blob each harness gets | opencode | identity literal | plugin `settings_schema()` + `Feature` scoping | needs a neutral type |
| `settings/mod.rs:404` | TS-codegen literal listing `opencode_native_gate` in the settings-pointer contract | opencode | identity literal | generated from the registry | mechanical |

## D. `tabs/` + `ipc/` + `main.rs` (beyond planned spawn composition)

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `tabs/config.rs:1238-1261` | `args_select_session()` — `SELECTORS: [&str; 7] = ["--session-id","--resume","-r","--continue","-c","--fork-session","--from-pr"]`, **Claude Code's session-selector CLI vocabulary** hardcoded in the neutral session-pinning plumbing | claude | CLI flag | `HarnessPlugin::session_selector_flags()` | mechanical |
| `tabs/config.rs:1009-1013` | `CHANNEL_REGISTRATION_FLAG = "--dangerously-load-development-channels"` + `CHANNEL_REGISTRATION_TARGET` — a `pub(crate)` Claude CLI flag read from `offload/mcp.rs` | claude | CLI flag | `harness/claude/` | mechanical |
| `tabs/config.rs:1043, 1055` | **`GRAPH_GUIDANCE` — a prompt sent to *both* harnesses — names Claude's capitalized tools**: "over a full **Read**" and "running the test command in **Bash**". OpenCode's are `read`/`bash`. | claude leaking into both | prompt text | `HarnessPlugin::native_tools()` substitution into the shared blob | behaviour-bearing |
| `ipc/tab_lifecycle.rs:55-61, 1074-1092` | `TabLifecycleError::OpencodeNotFound` + the pre-enable `resolve_command("opencode")` probe, with "Claude is intentionally not gated — it's the app's own front end" | both | identity literal | `HarnessPlugin::preflight()` | behaviour-bearing |
| `ipc/tab_lifecycle.rs:389-390` | Duplicate auto-naming documented as `"Claude" → "Claude 2"` | claude | UI string | registry label | mechanical |
| `ipc/commands.rs:2572, 2678-2688` | `detection_status` IPC payload carries `claude_sessions`, `claude_tokenless_sessions`, `claude_last_seen/_verified/_auto_verify` as **named fields on the wire** | claude | payload shape | per-harness map (extends decision 5 to the IPC surface) | needs a neutral type |
| `ipc/commands.rs:2809-2816` | `harness_mark_verified` writes `claude_last_verified = claude_last_seen` with no harness argument | claude | identity literal | take a `HarnessId` | mechanical |
| `ipc/commands.rs:2613` | Gate id literal `claude.hook.pretooluse_deny` in a core comparison comment | claude | identity literal | registry capability id | mechanical |
| `main.rs:112-123, 185-189, 375` | `cimp` is a **drop-in `claude` replacement**: unrecognized argv is forwarded verbatim to the Claude tab (`extra_args`); help text says so | claude | CLI flag | `HarnessPlugin::accepts_passthrough_argv()` | behaviour-bearing |
| `main.rs:508-530` | Boot active-tab fallback `unwrap_or(TabId::Claude)` ("post-integrity that's always Claude") | claude | identity literal | registry default | mechanical |
| `Cargo.toml:4` | Crate description: "Claude Code wrapper with TTS, avatar…" | claude | UI string | reword | mechanical |

## E. `offload/` (47 findings in `loopback.rs` alone)

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `loopback.rs:42` | Core imports `crate::harness::claude::hook as claude_hook` — **28 call sites** | claude | identity literal | dissolves with `routes()` | mechanical |
| `loopback.rs:1406, 1451, 4653, 5884-5886, 6643, 7476, 8158, 9245, 9511, 9953-9955, 10017` | **11 × `unwrap_or("claude")`** consumer/agent defaults; each documented as a *wire-compat promise to an older shim build* | claude | identity literal | one named `harness::DEFAULT_HARNESS` | mechanical |
| `loopback.rs:9063, 9805` | 2 × `unwrap_or("opencode")` — the opposite default on two routes; the asymmetry is load-bearing | opencode | identity literal | plugin-owned route (9063); explicit policy (9805) | behaviour-bearing |
| `loopback.rs:4826-4854` | `AUDIT_CONSUMERS: [&str; 2]` + refusal text hardcoding "(claude, opencode)" | both | identity literal | `PerHarness<T>` | needs a neutral type |
| `loopback.rs:5573-5593` | `TOOL_CHECKPOINT_BUDGET = 1800ms` — hand-computed from **Claude's shim `REPLY_TIMEOUT` and OpenCode's `AbortSignal.timeout(2000)`**, asserted against both by a cross-file test | both | behaviour constant | `HarnessPlugin::hook_reply_timeout()`, core takes `min − margin` | behaviour-bearing |
| `loopback.rs:5596-5608` | `tool_checkpoint_is_mutating` — the per-harness *dispatch* ("the `_` arm is Claude's") | both | payload shape | `native_tools().mutates_fs()` | mechanical |
| `loopback.rs:6118-6165` | `DRIFT_SHIMS: [&str; 10]` — Claude's shim token vocabulary; the whole key space of the drift ledger | claude | identity literal | `HarnessPlugin::drift_vocabulary()` | needs a neutral type |
| `loopback.rs:6372-6441` | `note_chp` / `report_quiet_capabilities` special-case Claude because "its hook body cannot carry a CHP envelope" — identity read from `X-CIMP-*` headers instead | claude | payload shape | `HarnessPlugin::identity_of_request()` | needs a neutral type |
| `loopback.rs:6733-6830` | `claude_hook_tab`, `claude_hook_cwd` ("Claude runs hook processes in the project directory"), `parse_hook_input`, `report_hook_drift` | claude | payload shape | `harness/claude/hook.rs` | mechanical / behaviour-bearing |
| `loopback.rs:6832-6932` | 5 × `*_from_hook` converters, each stamping `agent: Some("claude")` | claude | payload shape | same | mechanical |
| `loopback.rs:6934-7853` | ~900 lines: 12 `handle_claude_*` bodies reading Claude-only payload fields (`last_assistant_message`, `agent_id`, `tool_result`, `error`, `input.source`) and emitting Claude hook-output envelopes (`no_op`/`deny`/`additional_context`) — *the doc plans the route table, not these bodies* | claude | payload shape | `harness/claude/hook.rs` behind `routes()` | mechanical, needs a neutral `HookReply` |
| `loopback.rs:8302-8347` | `struct PermissionEventBody` — verbatim Claude `Notification` payload; `PERMISSION_MESSAGE_MARKERS = ["your permission","permission to use"]`; `PERMISSION_NOTIFICATION_TYPE = "permission_prompt"` | claude | payload shape | `harness/claude/hook.rs`; core keeps a neutral `PermissionEdge` | needs a neutral type |
| `loopback.rs:8349-8362` | `IGNORED_NOTIFICATION_TYPES: [&str; 7]` — transcribed from the Claude Code hooks guide | claude | payload shape | same | mechanical |
| `loopback.rs:8394-8495` | `classify_permission_event`, `resolve_permission_tab` (session-id → transcript-stem → cwd chain), `transcript_session_id` ("the filename stem IS the session id") | claude | payload shape | **CHP event** `PermissionPromptDetected/Resolved` | behaviour-bearing |
| `loopback.rs:8877-8963, 9073-9200` | `MemoryEventBody` — the OpenCode plugin's wire payload verbatim (`msg_id`, `parent_session_id`, `in_tok/out_tok/cache_read/cache_make`, `kind:"usage"`) + its usage mapping and sub-agent roll-up | opencode | payload shape | `usage_source()` | needs a neutral type |
| `loopback.rs:9014-9035` | `mark_live_session_from_event` — exists **only** because `live_sessions` has two key spaces (Claude keys by tab id, OpenCode by session id) | both | payload shape | `SessionKey` enum from the plugin | behaviour-bearing |
| `loopback.rs:9124-9145` | Tool-arg aliasing `file_path`/`filePath`/`notebook_path`/`path`, `pattern`/`query` — Claude snake_case + OpenCode camelCase merged in one core `match` | both | payload shape | `native_tools().arg_names()` | needs a neutral type |
| `offload/mcp.rs:2032` | **`CHANNEL_INSTRUCTIONS` — prompt text injected into the model's system prompt** via MCP `instructions` | claude | prompt text | `HarnessPlugin::instructions()` | needs a neutral type |
| `offload/mcp.rs:2040-2043, 1975` | `capabilities.experimental["claude/channel"]`; notification method `"notifications/claude/channel"` | claude | payload shape | `decorate_initialize()` / `push_notification_method()` | mechanical |
| `offload/mcp.rs:70, 2038` | `PROTOCOL_VERSION = "2025-06-18"` pinned to "the era where the client honours channels" | claude | wire constant | `mcp_protocol_version()` | behaviour-bearing |
| `offload/mcp.rs:2065-2066` | `session_push_enabled() = consumer()=="claude" && …` | claude | identity literal | `supports_session_push()` | mechanical |
| `mcp_host.rs:198-252, 1062-1090, 1153-1159, 1767-1768, 1848-1856` | `ServerSurface{claude_access,opencode_access}`, `SurfaceDigest{claude,opencode}`, `Consumer::granted(claude,offload,opencode)` (3 positional bools), `tool_defs_for_claude()` / `tool_defs_for_opencode()` | both | identity literal | `PerHarness<T>` + `tool_defs_for(HarnessId)` | needs a neutral type |
| `offload/service.rs:1063-1077, 1158-1165` | `Offload`/`Audit` consumers **fold onto Claude's** grant flag ("the conservative, `claude_access`-guarded default") | both | identity literal | explicit `Consumer::conservative_grant()` in core — see (c) | behaviour-bearing |
| `offload/server.rs:276-365` | `derive_opencode_provider()` + `model_id_from_path()` — writes OpenCode's `local-llama` provider block | opencode | config writer | `harness/opencode/config.rs` via `config_writer()` | mechanical |
| `offload/outbound.rs:1425-1437` | `UnscopedAudit::slot()` — `const fn` mapping agent → one of **2 array slots** (`b"opencode"⇒1, _⇒0`) | both | identity literal | `PerHarness<ScopeLedger>` | needs a neutral type |
| `offload/toolclass.rs:312, 326, 366` | `hook_post_edit` / `hook_should_read` / `hook_compaction` rows — exist only because Claude has a hook mechanism | both | identity literal | rename neutrally; rows stay in core (see (c)) | needs a neutral type |
| `audit/runner.rs:460-466` | `consumer_exposed()` — `_ ⇒ expose_claude` silent fallback | both | identity literal | `PerHarness<bool>` | mechanical |
| `audit/mcp.rs:579-581` | `--consumer` default `"claude"` in the audit MCP child | claude | identity literal | `DEFAULT_HARNESS` | mechanical |

## F. `state/` + `pty/` — the TUI activity heuristic is a model of Claude's terminal

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `state/manager.rs:314-330` | `StateSignal::ClaudeOutputStarted` / `ClaudeOutputStopped` and `AgentsActiveChanged` — **the core L2 signal vocabulary is named after one harness**; the latter documented as "the count of in-flight `Task` sub-agents … emitted by the transcript tail" | claude | identity literal | **CHP events** `HarnessOutputStarted/Stopped`, `SubagentsActiveChanged` | mechanical rename, ~14-site blast radius |
| `state/manager.rs:601-1262` | `claude_output_active` per-tab field + every transition arm reading it | claude | identity literal | rename to `harness_output_active` | mechanical |
| `state/manager.rs:272-284` | `AGENTS_STALL_TIMEOUT` sized against "Claude Code's footer repaints ~once/second" | claude | behaviour constant | `HarnessPlugin::activity_tuning()` | behaviour-bearing |
| `state/manager.rs:34-105` | `TabId::{Claude,ClaudeLocal,OpenCode}` — a **third** harness-identity enum, distinct from `Harness` and `Consumer`, living in `state/` | both | identity literal | `TabId::Builtin(HarnessId)` | needs a neutral type |
| `pty/tasks.rs:43, 48, 60-88` | `CLAUDE_BURST_MIN` (1000ms), `CLAUDE_QUIET` (500ms), `CLAUDE_MARKER_GRACE` (1200ms), `RESIZE_BURST_GRACE`, `CLAUDE_WORKING_STALE` (6s) — five timing constants **tuned to Claude's spinner/footer repaint rate** | claude | behaviour constant | `HarnessPlugin::activity_tuning()` | behaviour-bearing |
| `pty/tasks.rs:386-441, 466-487, 539-556` | The marker-vs-byte-burst arbitration loop, `should_release_idle()`, and the `"claude working detected/resolved"` log lines | claude | behaviour-bearing logic | `ActivitySource::TuiMarkers(tuning)`; core keeps the CHP edge | behaviour-bearing |
| `pty/manager.rs:518-519` | `oob_drives_activity = matches!(spec.oob, Some(OobSpec::OpenCodeEvent{..}))` — core decides whether to run the Claude heuristic by testing for **one specific harness's** OOB variant | opencode | identity literal | `activity_source()` — the branch disappears | mechanical |
| `procutil.rs:126-140` | `kill_tree_blocking` exists because "`opencode serve` is a Bun binary that forks children (observed: two grandchildren)" | opencode | behaviour | primitive stays; requirement → `needs_tree_reap()` | mechanical |

## G. `processing/` — Claude's TUI grammar, in core production code

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `processing/permission.rs:119-206` | The shipped default pattern set: `claude_permission` (`all_of ["to cancel ·"]`, `none_of ["to select","to navigate"]`, verified vs Claude Code 2.1.221), `claude_permission_bare` (`["to cancel","1. Yes 2."]`), `claude_question` (`["Enter to select","Type something"]`), `claude_working` (`["esc to interrupt"]`) | claude | payload shape/parsing | `HarnessPlugin::permission_patterns()` | needs a neutral type |
| `processing/permission.rs:207-238` | `opencode_permission` / `opencode_working` — **disabled placeholder rows** with literal text `"<replace with a substring unique to opencode --mini's permission prompt>"` | opencode | payload shape | `harness/opencode/prompts.rs` | needs a neutral type |
| `processing/permission.rs:12-53, 66-76, 96-97, 280-286` | Module doc encoding Claude's footer grammar, `~/.claude/keybindings.json` remapping, `PatternKind::Question` defined as "AskUserQuestion-style", `PatternKind::Working` mapped to `ClaudeOutputStarted/Stopped` | claude | payload shape | plugin doc + CHP event | mechanical / behaviour-bearing |
| `processing/patterns_file.rs:110` | `const CLAUDE_FOOTER: &str = "Esc to cancel · Tab to amend"` — **production code, not a test** | claude | payload shape | `harness/claude/prompts.rs` | mechanical |
| `processing/patterns_file.rs:113-211` | The **per-release legacy snapshot table** (`v040`/`v063`/`v070`/`v022`) used for pristine-file reconciliation — composed of `claude_*`, `opencode_*` **and `aider_*` rows** (`"Apply edits?"`, `"(Y)es"`, `"Run shell command?"`) for a *retired third harness* | claude/opencode/aider | fixture | `HarnessPlugin::legacy_permission_patterns()` per era; aider needs a retired-harness slot | needs a neutral type |
| `processing/patterns_file.rs:45-62` | The `_doc` header text shipped inside `patterns.json` names Claude's select menus | claude | UI string | plugin-composed | mechanical |
| `processing/screen.rs:139-149` | `recent_rendered` counts chars not bytes *because* "Claude's prompt chrome is full of multibyte glyphs (─ · ↑ ↓)" | claude | payload shape | tuning via `permission_patterns()` | mechanical |
| `scripts/patterns.default.json:1-60` | The shipped seed file — same pattern set on disk | both | fixture | ships with the plugins | mechanical |

## H. `advisor.rs` — every V16 drift rule is Claude-payload-shaped

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `advisor.rs:413-456` | `Signals::{claude_last_seen, claude_last_verified, claude_auto_verify, claude_sessions, claude_tokenless_sessions, subagent_drift}` — **six harness-named fields on the core signal struct** | claude | payload shape | neutral `HarnessDriftSignals` keyed by id | needs a neutral type |
| `advisor.rs:601-606` | `match cap.harness { Claude ⇒ "Claude Code", OpenCode ⇒ "OpenCode" }` — display-name table in core | both | UI string | `Harness::label()` | mechanical |
| `advisor.rs:727-736` | `version_signature()` — every version-keyed rule's re-fire key is **Claude's** version | claude | payload shape | neutral `version_signature(harness, seen)` | needs a neutral type |
| `advisor.rs:741-782` | Auto-verify failure notice — reads `claude_auto_verify`, prose "Claude Code updated to {}" | claude | prompt text | loop over plugins | behaviour-bearing |
| `advisor.rs:799-846` | `drift.version.v1` — condition is `claude_last_seen != claude_last_verified`; **OpenCode has no equivalent path at all** | claude | payload shape | per-harness evaluation | behaviour-bearing |
| `advisor.rs:878-945` | `drift.read_hook_silent.v1` / `drift.injection_unseen.v1` — bodies name Claude's `PreToolUse` / `UserPromptSubmit` mechanisms as the fix pointer | claude | prompt text | `Capability::drift_hint()` | behaviour-bearing |
| `advisor.rs:948-990` | `drift.usage_fields_gone.v1` — fires on `claude_sessions`, names "the transcript's `message.usage` shape" | claude | payload shape | `usage_source()` + `drift_hint()` | behaviour-bearing |
| `advisor.rs:1079-1113` | `drift.subagent_transcripts.v1` — "the **Claude transcript tap** reported sub-agent contract drift" | claude | payload shape | `transcript_reader()`; CHP drift event | behaviour-bearing |
| `advisor.rs:365-367, 1414-1448` | `BYPASS_HIGH` / `drift.read_bypass.v1` — assume a `Bash`-shaped shell tool and enumerate `cat`/`Get-Content`/`sed`/`head`/`tail` | both | identity literal | `native_tools()` / `shell_tool()` | behaviour-bearing |

## I. Remaining core Rust

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `activity.rs:390-450` | `Attribution::Headless` defined by the literal command `claude -p` | claude | CLI flag | plugin note | mechanical |
| `activity.rs:511-518` | `ActivityEntry::source` — free-string agent tag documented as `"claude"`/`"opencode"` | both | identity literal | `HarnessId` (blocked by persisted JSONL, see (c)) | needs a neutral type |
| `notifications/manager.rs:119-137, 226-291` | Echo-suppression + startup-chrome guard exist **because Claude's welcome banner cycles a fresh tab `Idle→Thinking→Idle`** | claude | payload shape | `HarnessPlugin::emits_startup_chrome()` | behaviour-bearing |
| `notifications/manager.rs:399-412`, `audio/playback.rs:216-220` | `TabId::Claude` as the **fail-safe fallback on a poisoned lock**, in two hot paths | claude | identity literal | neutral "first configured AI tab" | mechanical |
| `spawn_ledger.rs:148-161, 268-291, 313-323` | Ledger rows carrying literal argv `opencode serve --port <free> --hostname 127.0.0.1`, `claude --help`, `opencode --version` | both | CLI flag | `spawn_sites()` supplies strings; rows stay (see (c)) | needs a neutral type |
| `theming/tui_theme.css:435-445` | `.status-bar { height: 44px }` — "two lines tall to fit the stacked **Claude Code** usage meter" | claude | UI string/CSS | `HarnessPlugin::statusline_rows()` → CSS var | needs a neutral type |
| `theming/mod.rs:213-244` | `OpenCode Grey` is the `include_str!`-embedded last-resort palette and the default new installs land on | opencode | UI string | see (d) | mechanical |
| `error.rs:42-46`, `checks/auto.rs:88-199`, `content.rs:63-220`, `graph/context.rs:9` | Doc comments only (`"Surfaced to Claude as…"`, `writers["claude"]` as the worked example) | both | UI string | reword | mechanical |

## J. Frontend `src/` (≈157 sites; densest areas listed)

| file:line(s) | what | harness | kind | destination | difficulty |
|---|---|---|---|---|---|
| `src/lib/ipc.ts:95-172` | **`ContextSnapshot` / `UsageWindow` / `UsageSnapshot` mirror Claude's statusline JSON field-for-field** (incl. `agent_name`, `effort`, `thinking`, `fast_mode`, `five_hour`, `seven_day`); `getClaudeUsage()` | claude | payload shape | CHP event → neutral `ContextReading` + `QuotaWindow[]` | needs a neutral type |
| `src/lib/status/contextMeter.ts:60-110` | `RESERVED_AI_TAB_IDS`, `commandIsClaude` (a hand-written mirror of Rust's `command_is`), `claudePushTabActive` — the whole "who can push usage?" rule | claude | identity literal | IPC registry capability `usage_push` | behaviour-bearing |
| `src/lib/status/UsageMeter.svelte:56-69, 213-244` | Widget gated on `claudePushTabActive`; **two hardcoded windows** labelled "current session (5h)" / "weekly session (7d)"; tooltip "Claude Code usage" | claude | UI string + payload | registry-supplied window labels | needs a neutral type |
| `src/lib/usageMath.ts:8-68, 128-173, 399-436` | `TurnTokens` (Anthropic's four billing categories as *the* cost model), `cacheHitRatio`, `LaneSegment`/`agentBarClass` (the `'session'\|'agent'` sub-agent split), `originShareLine` | claude | payload shape | registry-declared billing categories + origins | behaviour-bearing |
| `src/lib/graph.ts:129, 613-682` | `GraphCall.source` closed union with harness ids; `TurnUsage`/`UsageTotals`/`OriginSplit`/`ModelUsage` | both | payload shape | CHP `turn.usage` | needs a neutral type |
| `src/lib/graph.ts:826-830` | `harnessMarkVerified()` takes **no harness argument** — Claude-only by construction | claude | identity literal | take a `HarnessId` | mechanical |
| `src/lib/settings/types.ts:199-208, 1534-1545, 2002-2010` | `StatuslineSettings`, `ClaudeLocalSettings` (`ANTHROPIC_*` env contract), `OpencodeLocalProvider` (`local-llama` block) | both | config writer | plugin `ext` | behaviour-bearing |
| `src/lib/settings/types.ts:927-943, 1164-1169, 1988-1996` | `LlmPricingModel` (Anthropic categories); `CAP_PRETOOLUSE_DENY = 'claude.hook.pretooluse_deny'`; `native_web_visibility` doc naming `WebFetch`/`WebSearch` vs `webfetch`/`websearch` | both | identity literal | registry-published ids + `webTools[]` | needs a neutral type |
| `src/lib/settings/types.ts:2161-2242, 2458` | `DEFAULT_SETTINGS.tabs` — `command:'claude'`, notification strings "Claude is idle" / "…awaiting permission" / "…has a question" / "…encountered an error", ×2 for `claude-local`; `enabled_ai_tabs: ['claude']`; palette `'OpenCode Grey'` | both | identity + UI string | registry-supplied default tab + text templates | mechanical |
| `src/SettingsApp.svelte:3282-3502` | **Two entire settings panels** — "Claude session usage" and "Claude context bar" (with the literal example `Opus ▓▓▓▓▓░░░░░ 50% (100k/200k)`) — that exist only because Claude ships a statusline | claude | UI string | registry-contributed section slot | behaviour-bearing |
| `src/SettingsApp.svelte:5337-5368, 6203-6208, 6316-6322, 6336-6356, 7435-7443` | User copy stating harness mechanisms as fact: `WebFetch`/`webfetch` tool names, "Claude via a `UserPromptSubmit` hook, OpenCode via a generated `.opencode/plugin`", "`PreCompact` hook", "Redundant-read advisor (**Claude tabs**)", "harness state (`~/.claude`, OpenCode's config and data)" | both | UI string + mechanism | registry affordances (`webTools`, `stateDirs`, `injectMechanism`) | behaviour-bearing |
| `src/SettingsApp.svelte:1673-1682` | `'opencode-not-found'` error copy with `https://opencode.ai/docs` and "drop `opencode.exe` in ebin/" | opencode | UI string | `harness.installHint` | mechanical |
| `src/lib/TabErrorOverlay.svelte:30-35` | `installHint` returns non-null **only** for `tabId === 'claude' \|\| 'claude-local'`, with `docs.anthropic.com/en/docs/claude-code/setup` hardcoded | claude | identity + UI string | `harness.docsUrl` | mechanical |
| `src/lib/EventsView.svelte:521-531, 1294-1299` | `KNOWN_SOURCES` set + **harness-named CSS classes** `.esrc.claude` / `.esrc.opencode` | both | CSS | registry accent token | mechanical |
| `src/lib/GraphView.svelte:1343` | `isCloud = call.source === 'claude' \|\| call.source === 'opencode'` — decides the pulse colour bucket | both | identity literal | registry `tier` | needs a neutral type |
| `src/lib/CodeIntelligenceView.svelte:1294-1307` | `ADVISOR_RULES_TOOLTIP` — user-visible copy naming `drift.*` rules **and Claude's hook/transcript mechanisms** | claude | UI string | backend publishes rule descriptions | behaviour-bearing |
| `src/lib/CodeIntelligenceView.svelte:1389-1392, 1724-2176` | `DASH_ORIGIN_LABEL = { session:'main session', agent:'sub-agents' }` + donut/lane labels | claude | UI string | registry origins | needs a neutral type |
| `src/lib/settings/TabSettingsSection.svelte:46-51, 168-175, 222-263` | `isOpencode = tabId === 'opencode'` hides the local-provider control; "Defaults to `claude`… `C:\tools\claude.exe`"; live `ANTHROPIC_*` env preview | both | identity + config writer | registry `supportsLocalProvider` / `defaultCommand` / env preview | behaviour-bearing |
| `src/lib/settings/McpManagementEditor.svelte:292-296, 491-494` | "**OpenCode** refreshes its tool list in the same session, **Claude Code** on its next turn" — per-harness refresh semantics as user copy, stated twice | both | UI string + behaviour | `harness.toolListRefresh` | needs a neutral type |
| `src/lib/settings/mcpEditor.ts:204-218`, `toolPlugins.ts:586-597` | `newServer()` / `container()` seed one boolean **per shipped harness** | both | identity literal | `Record<HarnessId, boolean>` | mechanical |
| `src/lib/TimelineView.svelte:288-292, 366-369`, `TaintMenu.svelte:327-331` | "run **/clear** in that tab" — a harness slash-command in three user-facing strings | both | prompt/CLI text | `harness.newSessionCommand` | needs a neutral type |
| `src/lib/compose/attachments.ts:49-59` | `appendAttachments` emits `[image] <path>` + the literal instruction **"Read the attached image file(s)."** — prompt text, "verified against both" harnesses | both | prompt text | `harness.attachmentFormat` | behaviour-bearing |
| `src/lib/terminals.ts:278-427` | Mouse-mode swallowing (DECSET 1000/1002/1003/1006) + wheel-cell synthesis, tuned from Claude's and OpenCode's TUIs | both | behaviour | stays in the webview — see (c) | behaviour-bearing |
| `src/lib/offload.ts:136-144` | `offloadDeriveOpencodeProvider` → the "Add to OpenCode" button's whole path | opencode | config writer | `harness/opencode/` | behaviour-bearing |
| `src/lib/avatarConfig.ts:105-110`, `sprites/claudeSprites/`, `SettingsApp.svelte:2690` | `KNOWN_SPRITE_SETS` allowlists a **Claude-branded mascot sprite set** | claude | image asset | see (c) | mechanical |
| `src/lib/themes/index.ts:11-132`, `registry.ts:41-48` | `'OpenCode Grey'` compiled-in fallback palette + default | opencode | UI string | see (c) | mechanical |
| `src/lib/tabs/state.ts:12` | `activeTab = writable('claude')` — initial active tab hardcoded | claude | identity literal | registry default | mechanical |
| `src/lib/latch.ts:38-80`, `workbench.ts:145-169`, `timeline.ts:357-388`, `activity.ts:19-243` | Doc/format contracts: `harness:tool_name` (`claude:Bash`, `opencode:edit`), `contaminated` explained via "the newest `*.jsonl`", `local_by_user_flip` (OpenCode-only field), `claude -p` | both | payload/UI string | registry-owned format + CHP | mechanical |

## K. Fixtures / tests outside `src-tauri/fixtures/harness/`

| path | what | verdict |
|---|---|---|
| `src-tauri/fixtures/plugin-goldens/opencode/{plugin.all-on,all-off,mid}.js` + `MANIFEST.toml` | Byte-golden renders of the generated OpenCode plugin — **the only fixture directory outside `fixtures/harness/` that is pure harness payload** | move to `fixtures/harness/opencode/` |
| `processing/permission.rs:753-1020` | Real Claude Code TUI screen scrapes captured 2026-06-09 via `RUST_LOG=perm_capture=debug` (`Esc to cancel · Tab to amend · ctrl+e to explain`, `1. Yes` / `2. Yes, and don't ask again` / `3. No`, Sonnet/Opus model picker) | move to `fixtures/harness/claude/<version>/tui.*.txt` |
| `offload/loopback.rs:10118+` | ~600 literals: full Claude hook-input JSON, `Notification`/`PermissionDenied` payloads, `C:/Users/x/.claude/projects/slug/sess-a.jsonl`, OpenCode `/memory/event` usage bodies | move with the routes |
| `offload/loopback.rs:11460-11493` | `include_str!("../harness/claude/hook.rs")` — a core test reads a plugin's **source text** to prove drift-token reachability | becomes `HarnessPlugin::drift_tokens()` |
| `tabs/config.rs:2652-4930` (~30 sites) | Core tests assert the **OpenCode plugin JS internals** (`CIMP_NATIVE_GATE_ENABLED`, `CIMP_BEACON_ENABLED`, `CIMP_PARENTS`, `const CIMP_CHP = …`, `CIMP_WEB_TOOLS = new Set(["webfetch","websearch"])`) | move with `write_artifacts()` |
| `statusline/mod.rs:427-464` | Claude statusline stdin JSON inlined 5× | move (a fixture already exists and is unused) |
| `settings/schema.rs:5441-5476, 5569-5581` | Pinned versions `2.1.232` / `2.1.14` / `1.18.13`, capability id `claude.statusline.stdin`, the `claude-opus-5` migration tripwire | move |
| `sandbox/tabs.rs:591+`, `workbench/{shadow,mod}.rs` (~130), `offload/mcp.rs:2714+`, `mcp_host.rs:2807+`, `state/manager.rs:1390+`, `pty/{tasks,manager}.rs` | Grant-table goldens (`~/.claude`, `CLAUDE_CONFIG_DIR`), `Origin` trailers (`tool: claude:Edit`), `clientInfo:{"name":"claude-code","version":"2.1.222"}`, `TabId` serde round-trips | move / rewrite neutral |
| `settings/migration.rs:3159-5227` | Legacy `settings.json` samples (`["claude","aider","aider-local"]`, `.aider.model.metadata.json`) | **must NOT move** — frozen history |
| `docs/spikes/v20/{ev,0a_events}.ndjson`, `msg_reply.json`, `0a_serve.log`, `0a_opencode_event.sh`, `0b_claude_transcript_tail.sh` | Raw captured OpenCode SSE / Claude transcript payloads sitting in `docs/`, **no version pin, no canary reads them** | move to `fixtures/harness/` or delete |
| `scripts/portable-readme{,-no-models,-linux}.txt:25-27,110,122,194,201` | End-user install prose: "install Claude Code", "`claude --version` should print a version" | rewrite neutral |
| `src/lib/*.test.ts` (contextMeter 32, usageMath 35, timeline 26, activity 21, latch 20, types 3, templates 3, avatarConfig 1) | `commandIsClaude` path cases, `claude-*` pricing prefixes, `claude:Bash` source strings, `opencode_native_gate` | mostly rename; contextMeter + usageMath need genuinely new fixtures |

## L. Docs describing harness behaviour as core

| doc · section (lines) | what it asserts as core | verdict |
|---|---|---|
| `docs/ARCHITECTURE.md` § Token Efficiency V11 (350-522, hot 378-473) | **The full Claude hook routing table** — `UserPromptSubmit`, `PreCompact`, `PreToolUse`, `PostToolUse`, `Notification`, `SessionStart`, `PostToolUseFailure` + matcher strings — presented as cImp architecture | move to `harness/claude/README.md` (highest-value single move) |
| `docs/ARCHITECTURE.md` § Context Engine V10 (219-349) | `~/.claude/projects/<slug>`, `--session-id`, `--mcp-config`, `OPENCODE_CONFIG_CONTENT.mcp.cimp-offload`, `.opencode/plugin/cimp-inject-<tab>.js`, "Never launch OpenCode with `--pure`" | rewrite neutral + move mechanics |
| `docs/ARCHITECTURE.md` § Agentic Inner Loop V12 (576-660); § Warm pool V8-03 (118-187) | "a fourth Claude hook shim"; "`claude -p` / cron depend on it"; "same shape as Claude's `mcpServers`" | rewrite neutral |
| `docs/DESIGN.md` § What we are building (15-38) | **The product definition itself**: "wraps Claude Code (in two configurations)", "a wrapper around the actual `claude` binary", "NOT a chat app using the Anthropic API" | rewrite neutral — worst offender |
| `docs/DESIGN.md` § Permission prompt detection (246-253) | "the 'Esc to cancel · Tab to amend' footer" — and **stale**: `permission.rs:18-20` says that literal was retired for a grammar matcher | move to plugin README |
| `docs/DESIGN.md` § Tab Kinds/Lifecycle (154-199), § Settings Schema (642-832), § Offload (596-641), § Glossary (1019-1056) | Reserved ids as lifecycle law; canonical settings example is a Claude pair; ASCII diagram reads `Claude tab (Opus) ──offload_task──▶`; glossary defines "Builtin tab" via `claude`/`claude-local` | rewrite neutral |
| `docs/FEATURES.md` § AI Tabs (17-28) + ~17 V10–V25 bullets | Capabilities stated as **hook mechanisms**: "a Claude `PreCompact` hook", "Claude via a `UserPromptSubmit` hook, OpenCode via a generated `.opencode/plugin`", "Claude-only (OpenCode rides the pending V16 spike)" | rewrite neutral, uniformly |
| `docs/MAINTENANCE.md` § drift table (542-638) | 14 capability rows + version pins 2.1.63/2.1.233/1.18.13/1.18.18 — **the largest harness-knowledge block outside `src/harness/`** | split into two plugin READMEs |
| `docs/MAINTENANCE.md` § Steps 5 (24-104), § memory scoping (839-889), § usage taps (955-981), § Open spikes (1098-1122), § live-verify recipes (1154-1265), § component inventory (116-166, 338-358) | "Harness watch (Claude Code + OpenCode)" as a fixed two-item checklist; "OpenCode usage is estimate-only" as a *cImp* limitation; every open-spike row is a harness contract | move / rewrite neutral |
| `docs/CHP.md` § 4.5 (221-308) | **A 12-row Claude event→route table + `Minimum Claude Code: 2.1.63` + `X-CIMP-Agent: Always claude` inside the protocol doc** — the clearest layering violation | move |
| `docs/CHP.md` §§ 3.1-3.2 (95-121), 4.2/4.4 (168-220), 4.6 (309-408), 6.2 (547-567) | `agent` defined as a **closed enum** "`claude` or `opencode`"; producer column hardcodes per-harness handlers; §6.2 is a whole section on one harness's quirk | rewrite neutral / move §6.2 |
| `docs/HARNESS-NATIVE-TOOLS.md` § 7 (417-589) | Frames `native_web_visibility` — a **core setting** — entirely through Claude's `permissions.deny` and OpenCode's plugin gate | rewrite the semantics; move the recipes |
| `README.md` (82 lines) | Product README defines cImp as a Claude Code wrapper; `ANTHROPIC_*` setup instructions | rewrite neutral |
| Other heavy `docs/` files | `IMPL-PLAN-V10` 29, `FUTURE-FEATURES` 18, `IMPL-PLAN-V33` 17, `IMPL-PLAN-V14` 12, `IMPL-PLAN-V11` 12, `spikes/v20/README` 11, `spikes/v20/0a_*.sh` 9, `BUG-HUNT-2026-06-25` 9, `features/FEATURE-rebrand-ccimp` 8, `spikes/v20/0b_*.sh` 7, `TOKEN-EFFICIENCY` 7, `IMPL-PLAN-V17` 7, `features/FEATURE-secret-storage` 6 (`~/.claude/.credentials.json`), `IMPL-PLAN-V37` 6, `IMPL-PLAN-V13`/`V12` 5 each, `TOOL-PLUGINS` 4 | mostly historical plans — leave; `FUTURE-FEATURES` and `TOKEN-EFFICIENCY` worth a neutral pass |

---

## (a) Counts

| Area | Production findings |
|---|---|
| `offload/` (loopback 47, mcp 7, mcp_host 6, service 2, server 2, outbound 1, toolclass 1) | **66** |
| Frontend `src/` (SettingsApp 30, settings/types 16, settings/* 12, usageMath 9, graph.ts 7, ipc.ts 6, UsageMeter 6, contextMeter 5, latch 5, workbench 5, timeline cluster 10, CodeIntelligenceView 8, rest 38) | **≈157** |
| `processing/` (permission 12, patterns_file 7, screen 2, segmenter 1) | **22** |
| `advisor.rs` | **14** |
| `graph/` (service 5, index 2, memory 3, mcp 2, secrets 1) | **13** |
| `pty/` (tasks 8, manager 1) + `procutil.rs` | **10** |
| `usage/` + `statusline/` + the 2 IPC/CLI entry points | **13** |
| `settings/` (schema 4, persistence 2, injection 2, mod 1) | **9** |
| `tabs/` + `ipc/` + `main.rs` + `Cargo.toml` (non-planned) | **11** |
| `state/manager.rs` | **5** |
| `activity.rs` 4, `notifications/` 4, `spawn_ledger.rs` 4, `audio` 1, `theming` 2, `audit/` 2, `checks/` 3, `content.rs`/`error.rs` 2 | **22** |
| **Rust production total** | **≈185** |
| Fixtures/tests outside `fixtures/harness/` | **30** (1 misplaced fixture dir, 18 Rust `#[cfg(test)]`, 8 frontend `.test.ts`, 5 loose data/prose files) |
| Docs | **26 sections** across 6 named docs + **17** other `docs/` files by concentration |

Clean (swept, nothing found): `workbench/*`, `sandbox/{child_env,windows,linux,mod}.rs`, `pty/{scrollback,resolve,sandboxed_conpty}.rs`, `offload/{router,agent,supervisor,metrics,remote,openai,backend_gate,spotlight,detection/*,tools/*}.rs`, `audit/{mod,census,golden,runnable,adapters}.rs`, `stt/*`, `tts/*` (V20 deliberately neutralised it), `graph/shellread.rs`, `fsutil.rs`, `logging.rs`, `attach.rs`, `spawn_gate.rs`, `mcp_stdio.rs`, `rustsrc.rs`, `preview/`, `plugins/`, `sysmon/`, `shell/`, `src/lib/{preview,dnd,stt,selectionTts,diffWords,format,errors,viewSection}`, `terminals.css` (zero hits). **No `src-tauri/tests/` exists.** **No V39 `delegation/` / `InputProfile` code in this tree** (it lives in the `../cctts-v39` worktree).

## (b) Where moving it requires a NEW neutral core abstraction

1. **`PerHarness<T>`** — a registry-ordinal-keyed map replacing every fixed-arity-2 structure: `AUDIT_CONSUMERS` (`loopback.rs:4826`), `UnscopedAudit::slot` + its 2-slot array (`outbound.rs:1425`), `SurfaceDigest{claude,opencode}` (`mcp_host.rs:237`), `ServerSurface`/`McpServerConfig` `*_access` (`mcp_host.rs:198,1153`), `Consumer::granted(claude,offload,opencode)` (`mcp_host.rs:1062`), `code_audit.expose_*` (`runner.rs:460`), and the frontend twins (`mcpEditor.ts:204`, `toolPlugins.ts:586`). Without it, a third harness needs a source edit at each.
2. **`ActivitySource { OutOfBand, TuiMarkers(TuiActivityTuning) }`** returned by `HarnessPlugin::activity_source()` — abstracts "how does cImp know this harness is busy". Subsumes the entire `pty/tasks.rs` constant block + arbitration loop (`:43-88, 386-556`), `pty/manager.rs:518-519`'s `OpenCodeEvent` test, and `state/manager.rs:272-284`. The single largest behaviour-bearing cluster.
3. **Neutral L2 signals `HarnessOutputStarted/Stopped` + `SubagentsActiveChanged`** (CHP events) replacing `StateSignal::ClaudeOutput*` and `claude_output_active` — abstracts "the harness is emitting a turn". ~14 sites in `state/manager.rs`, 2 in `pty/tasks.rs`, 1 in `processing/permission.rs:70-76`.
4. **`QuotaWindow[]` + `ContextReading`** — abstracts "a rolling usage quota" and "how full is the model's context". Today `five_hour`/`seven_day` and Claude's `context_window`+`current_usage`+`agent.name`/`effort`/`thinking`/`fast_mode` are the *field names* across `usage/mod.rs:67-181` → `ipc.ts:95-141` → `UsageMeter.svelte:213-233`. A harness with one window, three, or none cannot be expressed.
5. **`TokenKinds` / declared billing categories** — abstracts "the dimensions a harness bills in". `input/cache_write/cache_read/output` is assumed from `graph/memory.rs:502-520` through `usageMath.ts:54-68`, `settings/schema.rs:953`, and `SettingsApp.svelte:7254`. A harness without prompt caching leaves two of four structurally zero and `cacheHitRatio` meaningless rather than absent.
6. **`TurnOrigin`** — abstracts "which lane of the conversation a turn belongs to". Currently the closed `'session' | 'agent'` union (`graph/memory.rs:461`, `graph.ts:626`, `usageMath.ts:131-173`, `CodeIntelligenceView.svelte:1389`), which is Claude's sidechain shape.
7. **`SessionKey { Tab(String), Session(String) }`** from `HarnessPlugin::session_key_space()` — `graph::live_sessions` is *one map with two key spaces* (Claude keys by tab id, OpenCode by session id). This is why `live_claude_tab_sessions` exists, why `mark_live_session_from_event` (`loopback.rs:9014-9035`) exists, and why the C-2 collision guard exists.
8. **`HookIngress` / `routes()` returning `(neutral CHP body) → HookReply`** — needs a neutral `HookReply` type, because Claude's reply is hook-output JSON (`no_op`/`deny`/`additional_context`) while OpenCode's is `{ok:true}`, and core currently knows both.
9. **`PermissionPatternSet` + `EraSnapshot`** — `PermissionDetector` is already neutral machinery; what's missing is a way for pattern **data** *and its per-release snapshot history* to arrive from plugins. `patterns_file.rs`'s pristine-file reconciliation compares against era vectors (`v040`/`v063`/`v070`/`v022`) that are concatenations of per-harness sets — **including a retired third harness (aider)**, so the type must accommodate retired plugins or the historical file bodies stop comparing equal.
10. **`HarnessDriftSignals`** keyed by harness id — replaces the six `claude_*` scalars on `advisor::Signals` (`:413-456`) and the same fields on the `detection_status` IPC payload (`ipc/commands.rs:2678-2688`) and `HarnessVersions` (`types.ts:945-972`). OpenCode then gets every drift rule for free.
11. **`Capability::drift_hint()`** — the drift cards embed Claude's *mechanism names* (`PreToolUse`, `UserPromptSubmit`, `message.usage`, `subagents/*.jsonl`) as the "what to check" half. Also needed by `CodeIntelligenceView.svelte:1294-1307`, which repeats them as user-visible copy.
12. **`HarnessPlugin::instructions()`** — a *declared* seam for text that reaches the model. `CHANNEL_INSTRUCTIONS` (`mcp.rs:2032`) and the attachment instruction (`compose/attachments.ts:55`) are model-visible strings that nothing in core marks as such.
13. **`ToolArgVocabulary`** — `native_tools().arg_names(MemArg)`, for `loopback.rs:9124-9145`'s merged `file_path`/`filePath`/`notebook_path`/`path` chain.
14. **`HarnessPlugin::hook_reply_timeout()`** with core computing `min(all) − margin` — replaces `TOOL_CHECKPOINT_BUDGET` (`loopback.rs:5573`), today a hand-computed constant coupled to two harness artifacts' timers.
15. **`HarnessAffordances`** for the frontend — the bag of behaviours the UI currently states as prose: `newSessionCommand` (`/clear`, 3 sites), `toolListRefresh` (4 sites), `webTools[]`, `stateDirs[]` (`~/.claude`), `installHint`/`docsUrl` (2 sites), `attachmentFormat`, `localProviderEnvPreview`, `statuslineRows` (which `tui_theme.css:435`'s magic 44px encodes).
16. **A registry-contributed settings-section slot** — "Claude session usage" and "Claude context bar" (`SettingsApp.svelte:3282-3502`) are whole panels; without a slot they stay hardcoded. (Decision 6 anticipates this as `features: &[HarnessFeature]`; these are the two concrete consumers.)
17. **`HarnessPlugin::spawn_sites()`** — supplies the literal argv/reason strings for `spawn_ledger.rs` rows.

## (c) Harness-specific but genuinely cannot move

- **`spawn_ledger::LEDGER` rows** (`:148-161, 268-291, 313-323`). The ledger's tripwire test does an **exhaustive scan of every `.rs` under `src/`** and asserts the table matches the tree. A row living inside `harness/claude/` would describe a spawn the scanner still finds in `harness/probe.rs`. The strings can be delegated; the rows' existence cannot.
- **`offload/service.rs:1063-1077, 1158-1165`** — folding `Offload`/`Audit` consumers onto Claude's access flag is a **security** default ("rather than leaking the offload set"), not a harness fact. It must become an explicit `Consumer::conservative_grant()` in core, never a plugin call.
- **`offload/toolclass.rs`'s class table** — the single reviewed authority for capability classification. Letting plugins contribute rows moves a security decision into harness code. The `hook_*` row *names* can be neutralised; the rows stay.
- **`state/manager.rs:34-105`'s `TabId` wire strings** and **`activity.rs:519`'s `source: String`** — both are persisted formats (settings JSON; the activity JSONL read back from disk). Renaming is a migration, not a code move; typing `source` as `HarnessId` would mis-read pre-split rows.
- **`processing::PermissionDetector`** (`:263-438`) — substring matching, `normalize_ws`, veto scoping, the per-kind edge machine. Only the data moves.
- **`src/lib/terminals.ts:278-427`** — must run *in the webview* against a live `xterm.js` instance (`attachCustomWheelEventHandler`). A backend plugin can publish "this harness wants local mouse"; the implementation stays.
- **`settings/migration.rs`'s legacy fixtures** — must reproduce the exact strings that shipped. Already exempt under decision 14; worth restating that moving them would be actively wrong.
- **`sprites/claudeSprites/` and the `'OpenCode Grey'` palette** — a brand asset and a *persisted-by-name* palette. Moving the palette behind the OpenCode plugin would make the default terminal theme vanish when OpenCode is uninstalled; renaming needs a settings migration. They should stop being *named* after harnesses, not move.
- **`settings/types.ts:1169`'s `CAP_PRETOOLUSE_DENY`** — pinned to Rust by `harness::contract::tests::the_gated_capability_ids_reach_the_frontend`. It can go behind the registry only if that test moves too, or the pin silently stops pinning.
- **13 `unwrap_or("claude")` / 2 `unwrap_or("opencode")` sites** — these are *wire-compat promises to specific older shim builds* (see the comments at `loopback.rs:5884, 4838`), not "pick any harness". They collapse to one named `DEFAULT_HARNESS`, but the value stays Claude for reasons that survive deleting the OpenCode plugin — and `/latch/state:9805`'s opposite default cannot fold in without a behaviour change.

## (d) Unsure whether these count as harness-specific

1. **`settings/schema.rs:953-1076` — the Anthropic price table.** Pricing is *provider* knowledge, not *harness* knowledge, and `graph/index.rs:7470` proves it: an OpenCode session reports `"anthropic/claude-opus-4-8"`. It may deserve its own `pricing/` seam rather than `HarnessPlugin::default_pricing()`.
2. **`graph/secrets.rs:441` `secret_anthropic_api_key`** and the `.yar` rule — a vendor secret pattern. Survives deleting both harnesses.
3. **`'OpenCode Grey'`** (`theming/mod.rs:213-244`, `statusline/mod.rs:321`, `themes/index.ts:70`, `types.ts:2242`) — a colour palette *named after* a harness and shipped as the default. Nothing about it depends on OpenCode existing.
4. **`sprites/claudeSprites/`** — the mascot exists independently of whether cImp drives Claude Code.
5. **`processing/segmenter.rs:99-108`** — the TTS character-class strip is *motivated* by Claude's `\x1b[1C` cursor-skips and spinner cells but now runs on out-of-band prose from any source; making it per-harness would re-couple TTS to the terminal path V20 deliberately cut.
6. **`offload/toolclass.rs:312/326/366`** (`hook_post_edit`, `hook_should_read`, `hook_compaction`) — the *routes* are cImp's own and the OpenCode plugin calls them too; only the word "hook" is Claude-derived.
7. **`src/lib/terminals.ts:278-427`** — passes the litmus test in letter (any fullscreen TUI benefits), but the tuning came from observing exactly these two.
8. **`src/lib/activity.ts:243` `CANARY_SOURCES = {'read_advisor','harness'}`** — both are cImp-internal channels, so the line is neutral; but `read_advisor` only exists because Claude has a `Read` tool and a `PreToolUse` hook.
9. **`GraphView.svelte:1343` `isCloud`** — could be read as "cloud vs local model" (a deployment fact) rather than "which harness"; counted because the discriminant is literally the two ids.
10. **"Awaiting permission" as a *concept*** (`avatarState.ts`, the default notification strings at `types.ts:2174-2177`) — the frontend holds only a boolean, but the UX category is Claude's permission-prompt model.
11. **`docs/CHP.md`** — not on the exclusion list, but arguably a harness-seam doc. I treated §§1-2, 5, 8 as correctly core and flagged §§3.1/3.2, 4.2, 4.4, 4.5, 4.6, 6.2 as intrusions. If you consider CHP.md wholly a harness doc, drop those rows.
12. **`sandbox/child_env.rs`'s node/npm rows** — OpenCode is a Node app, but these serve `run_command`/`run_check` for any Node project. Judged neutral, **not** reported.

---

**Verification caveat:** rows in areas A–D and the counts above are ones I read directly. Areas E–L came from four parallel sweeps; I spot-verified a sample from each (`loopback.rs:4826/5573/5880/8337`, `mcp.rs:2032`, `outbound.rs:1425`, `permission.rs:119-145`, `patterns_file.rs:110`, `contextMeter.ts:60-110`, `ipc.ts:105-141`, `TabErrorOverlay.svelte:30-35`, `EventsView.svelte:521/1294`, `fixtures/plugin-goldens/opencode/`, `docs/spikes/v20/`) and every check matched. The remaining rows carry their reporter's line numbers unverified by me.
