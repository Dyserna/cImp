# V33 implementation plan — stages 1–3 (no OS dependency)

**Written 2026-08-13.** Source of truth for the agent briefs of this run. The
milestone spec is `MILESTONE-V33-sandboxing.md`; this file records what the
2026-08-13 investigation found to be *false* in that spec, and the contracts the
orchestrator has fixed so parallel lanes cannot drift.

**User decisions taken 2026-08-13 (locked, do not re-litigate):**
1. Build stages 1–3 (everything with no OS dependency) now; spikes S1/S2 after.
2. **F-5 stays fail-open.** Fix the `cwd` half only; the locked
   "a tool call must never fail for lack of identity" posture is unchanged.
3. **The yara-x/wasmtime exposure is V33 work**, not an accepted residual —
   actually constrain it (stage 3).
4. No prerequisite user actions are planned around; code first.

## Corrections to the spec (verified 2026-08-13, every claim cited)

| Spec says | Reality |
|---|---|
| "the two spawn seams cImp owns" (`:7-12`, decision 1) | **Four.** `pty/manager.rs:199`, `offload/tools/run_command.rs:258`, **`checks/mod.rs:734`** (`run_check`, runs via a *shell*), **`audit/runner.rs:1235`** (`security_audit`/`quality_audit`). All four are agent-initiated; the last two are reachable both from the offload worker and from Claude/OpenCode over MCP. `offload/mcp_host.rs:1230` is a fifth, agent-*serving*. |
| decision 7 "job objects … under every agent spawn" (`:144`) | Job Objects **already exist** — `process_guard.rs` (`CreateJobObjectW:89`, `KILL_ON_JOB_CLOSE:95`). `run_command:263`, `checks:739`, `audit/runner:1247`, `mcp_host:1233` all call `guard_child`. **`guard_child` is typed to `tokio::process::Child`, so the PTY child — every AI tab — is outside the job.** On Linux `process_guard.rs:46` is `let _ = child;` and `procutil.rs:61-73` kills only the direct child. |
| Phase F "the V32 class table gaining a `mutates_fs` attribute" (`:264`) | **Already landed in V32.** `toolclass.rs:157-179`; the accessor `:502-508` sits behind `allow(dead_code)` and its doc `:161-166` names V33 Phase F as its consumer. Four rows are true: `run_command:217`, `Edit:371`, `Write:372`, `Bash:373`. |
| Phase F fire seams "loopback `/graph_run`//`/mcp/call` (all proxied tools)" (`:267`) | **Both would fire zero checkpoints.** No tool either route serves has `mutates_fs: true`. The only routed mutating tool, `run_command`, is reached solely via worker dispatch (`tools/mod.rs:183`). |
| Phase F OpenCode "`tool.execute.before` — the SAME unverified hook as V32 spike E2" (`:269-271`) | **Implemented and shipped** — `tabs/config.rs:1938-2064`, load-bearing for V32's native gate, with tests at `:4844-4859`. Spike E2 is answered positively. |
| Phase F "the fields parser must tolerate a missing `source`" (`:296`) | **Already tolerant by construction** — `CORE_FIELDS = 8` with a `<` guard (`shadow.rs:861-863`) and `fields.get()` reads (`:878-881`), rule stated at `:806-825`. `Trigger::parse` already maps unknown ⇒ `Manual` (`:138-145`), which is the accepted degradation the spec asks for. |
| Phase D "rides the Linux milestone" (`:248`), `:12` links `docs/MILESTONE-linux-support.md` | **The port shipped.** That path is dead — the file is `docs/completedMilestones/MILESTONE-linux-support.md`. Linux builds with full GPU parity on Ubuntu 24.04 and ships a tarball from CI. Phase D is blocked only on a Linux CI test job, the `process_guard`/`procutil` backstop, and the 0/12 hardware runbook. |
| Phase E decision 11 "the llama-servers … currently speak plain unauthenticated HTTP" (`:172-183`) | True, **and larger than stated**: the bearer auth that exists covers only `OffloadBackendKind::Remote`, which this install has configured *empty*. The active backend is `local` (`--host 0.0.0.0 --port 12344`, no `--api-key`), and `Local` has no token field at all. Embedding (`172.21.1.11:12344`) and both MCP endpoints (`:17201`/`:17202`) are also unauthenticated. |

Also corrected: the schema constant is `CURRENT_SCHEMA_VERSION` (`schema.rs:163`),
not `settings_version`; there is no `/v1/models` call and no non-streaming chat
path anywhere in the tree.

## Locked contracts (orchestrator-owned; briefs must use these names verbatim)

### C1 — the seam ledger
An exhaustive `(file, reason)` table in code, asserted by an `include_str!`
tripwire in the house style of
`offload/agent.rs:3878-3893` (`concat!` so the test does not trip itself;
multi-file variant at `:4625-4627`). Must exclude `#[cfg(test)]` blocks — ~15
files carry test-only `Command::new("git")`. Every entry is classified
`AgentSpawn` or `HostSpawn` with the reason recorded. Host spawns that must
never be sandboxed: `offload/supervisor.rs:1066` (llama-server),
`workbench/git.rs:247` (**the shadow-checkpoint repo — sandboxing it breaks
restore**), `graph/gitcmd.rs:26`, `checks/gitls.rs:62`, `audit/mod.rs:227`,
`tabs/config.rs:300-308`, `procutil.rs:69`, `ipc/commands.rs:1360,3269`.

### C2 — `run_command` minimal environment (spec decision 10)
Build up from nothing; never inherit-and-subtract. One function next to the
spawn. The bar, in order of precedence:
1. No secret cImp holds may reach the child — no API keys, no loopback token,
   no OAuth material.
2. `git log`, a `cargo` probe and an `npm` probe must still work
   (live-verify item 7). This means the table **must** carry the toolchain's
   own state pointers (`HOME`/`USERPROFILE`, `CARGO_HOME`, `RUSTUP_HOME`, npm
   cache) — granting a tool its own state dir is not a hole (spec decision 3).
3. The table is data in code with a doc comment, reviewed like the V32 class
   table.
Applies to `run_command` children **only**. Tab spawns keep `env_remove`.

### C3 — Job Object coverage
`process_guard` gains a pid-taking entry point beside `guard_child`. The PTY
child (`portable_pty::Child`, `pty/manager.rs:199`) is assigned via its
`process_id()`. The assign-after-spawn race window is **documented, not hidden**.
Linux: process-group spawn + `killpg` in `procutil::kill_tree`, and
`PR_SET_PDEATHSIG` via `pre_exec` if it can be done without restructuring the
four spawn sites. Adds the project's first `[target.'cfg(unix)'.dependencies]`.

### C4 — `/context/post_edit` root allowlist
Copy the pattern `/audit/run` already uses (`loopback.rs:4709-4718`). Roots
derive from configured tabs and the served root, **never from the request**.
The F-5 fail-open is untouched (user decision 2). Siblings `/context/should_read`
(`:5286`) and `/context/compaction` (`:5372`) do not execute anything — leave
them, and say so in the row.

### C5 — F-4 `(consumer, tab)`
`is_configured_tab` (`loopback.rs:1556`) becomes consumer-scoped.

> **Correction 2026-08-13 — the second sentence of this contract was WRONG and
> was not implemented.** It read: *"the empty-list availability floor is
> preserved but narrowed: it applies only when the asserted consumer has zero
> configured tabs, not when any consumer does."* That is a **widening**, not a
> narrowing. On the ordinary install — Claude tabs only — *"opencode has zero
> configured tabs"* is permanently true, so every forged id asserting
> `consumer: opencode` would receive a scope, re-opening the unbounded registry
> key space that issue #45 closed. The floor's own rationale is `live_settings`
> falling back to `Settings::default()`, which is a **global** "settings
> unreadable" condition, not a per-consumer one.
> **As built:** only the positive membership test is consumer-scoped; the floor
> stays keyed on the whole tab list. Strict tightening of the admitted set, no
> new key space. The empty-list test now asserts the failure mode of the
> literal reading, so the error cannot be reintroduced.

### C6 — H-7, cheap half only
The OpenCode plugin template binds `fetch` into a private module-scope constant
at load, so a later `globalThis.fetch` swap cannot neuter the beacon or gate.
Correct the additive-posture doc at `tabs/config.rs:2302-2308`.
`OPENCODE_DISABLE_PROJECT_CONFIG` is **not** shipped in this run — its
population question is entangled with F-31's default-off gate; recorded as owed.

### C7 — Phase E field names and traps
- `OffloadBackendKind::Local` gains `auth_token: String` — **field-level
  `#[serde(default)]` is mandatory**: serde's container default does not apply
  to enum variants (`schema.rs:3041-3042`, cf. `show_command_on_start:3054`).
  Omit it and every existing settings file fails to deserialize.
- `GraphSettings` gains `embedding_auth_token: String`. `GraphSettings` derives
  `Debug` today and its doc says "No secrets here" (`schema.rs:2168-2171`) —
  **hand-roll `Debug` and correct that comment.**
- `McpServerConfig` gains `auth_token: String`. **It MUST be added to
  `config_sig` (`mcp_host.rs:1136-1147`)** — that list is explicit, and omitting
  the token means editing it in Settings never reconnects the server.
- Headers attach at exactly `http_request` (`mcp_host.rs:1530`) and
  `http_notify` (`:1596`); the token threads through `Conn::Http` (`:536-538`).
- `server.rs` probes (`:377`, `:494`) send bearer when the token is non-empty.
- `Embedder::new` (`embed.rs:97-109`) is the single injection point for all four
  embedding request sites (`:308-312`, `:197-202`, `:255-259`, `:277-281`).
- UI follows the existing three-instance house pattern: cleartext in
  settings.json, redacting `Debug`, `type="password"` input.
  (`ClaudeLocalSettings.auth_token schema.rs:1226,1230-1243` is the model.)

### C8 — Phase F contracts
- `Trigger::Tool`, wire value `"tool"`.
- New git trailer `Source:` **appended after `Tab:` at the tail**, read at index
  10 via `fields.get(10)`. `CORE_FIELDS` stays `8`; the `<` guard stays `<`.
- `Origin` gains `source` **as a named field — do NOT add a fourth positional
  argument** to `Origin::new` (`shadow.rs:203`); its own doc warns about
  transposable same-typed strings.
- Value format `harness:tool_name` — `claude:Bash`, `offload:run_command`,
  `opencode:edit`.
- Fire seams for this run: **worker dispatch** (`tools/mod.rs:173-201`) +
  a Claude `PreToolUse` shim + the OpenCode `tool.execute.before` report-only
  half. `/graph_run` and `/mcp/call` are **deliberately not wired** — zero
  mutating tools today; record the reason so it is not re-raised as an omission.
- The Claude shim follows `taint_beacon.rs`: report-only, never denies,
  `--tab` baked. It is a **third** `pre_tool_use.push` (`tabs/config.rs:647`),
  and needs a `spawn_inject_sig` entry (`:450`, enumerated `:486-496`).

> **Amendment 2026-08-13 (user decision) — the shim WAITS.** As first written
> this contract said "never reads the reply", copied from `taint_beacon`'s
> discipline. For a beacon that is right; for a *pre-tool checkpoint* it
> silently breaks the feature's central claim. Because the shim did not wait,
> the app ran the snapshot **concurrently** with Claude's tool execution, so on
> a large tree a `git add -A` can outlast a small `Edit` and capture the very
> change the checkpoint exists to precede. The worker and OpenCode seams do not
> have this problem — both await the snapshot before the tool runs.
> **As decided:** the Claude `PreToolUse` hook **blocks until the snapshot
> completes, with a ~2 s deadline**. Inside the budget the "immediately before"
> guarantee is exact on all three seams. Past it, **no checkpoint is claimed and
> the miss is surfaced** — never a silently misattributed row, which is the
> dedupe hazard's sibling failure mode.
> *Accepted cost:* every `Edit`/`Write`/`MultiEdit`/`Bash` in a Claude tab now
> waits for a `git` stage-and-write-tree. Usually cheap thanks to the
> identical-tree dedupe; occasionally noticeable on a large repo.
> *Note for the implementer:* this is a deliberate divergence from
> `taint_beacon`, the file the shim was copied from — **document it on the shim
> itself**, or the next reader will "restore consistency" and reintroduce the
> race.
- `OPENCODE_NATIVE_TABLE` (`toolclass.rs:404-430`) gains a `mutates_fs` third
  element; `edit`/`write`/`patch`/`apply_patch`/`bash` are true.
- `MultiEdit` gets an `unrouted` `TABLE` row with `mutates_fs: true` — the
  existing `PostToolUse` matcher already names it (`tabs/config.rs:800`) and it
  has no row, so it currently answers `false`.
- **Dedupe hazard (locked):** `shadow.rs:730-736` returns the *existing*
  checkpoint on an unchanged tree without relabeling. A pre-tool checkpoint that
  gets a foreign id back must **not** claim it — no `tool` row is reported for
  it. Never relabel another trigger's checkpoint.
- **Throttle (locked):** the tool trigger keeps the per-`(root, tab)` bucket
  (`CheckpointKey`, `workbench/mod.rs:137`). Per-tab attribution is the point of
  the feature; the 2026-08-09 amendment's question is answered "keep the tab
  bucket".
- `ToolCtx` (`tools/mod.rs:31-42`) must gain what the checkpoint needs — it
  carries no root, no tab, no workbench handle today. This is the only seam in
  Phase F requiring real new plumbing.
- Frontend: `CheckpointTrigger` union (`src/lib/workbench.ts:125`) gains
  `'tool'`; `triggerIcon`/`triggerTitle` (`timeline.ts:319-347`) gain a case —
  both already have `default:` arms, so an un-updated frontend degrades rather
  than breaks.

### C9 — yara-x local rules
`rules.d/local/*.yar` are user-authored and run under a wasm sandbox the V32
review states "is not the boundary", against 16 open wasmtime advisories. The
bar: either that sentence stops being true, or the capability becomes opt-in
with a user-visible signal. Investigate and propose before implementing.

## Standing constraints for every brief in this run

- **RUST is single-occupancy.** One Rust agent at a time — two agents editing
  different `.rs` files still poison each other's `cargo` output. `src/`,
  `docs/`, `.github/` and read-only design agents run freely alongside.
- `isolation: "worktree"` is **unusable** here (`src-tauri/target` is ~255 GB).
- **Never run `cargo fmt`** (it formats the whole crate regardless of path).
- **Never run `git`** — not `checkout`, not `stash`, not `commit`. Save a
  `git diff` patch to the scratchpad on finish instead.
- **`cargo test --bin cimp --no-run` and `cargo check --tests --bin cimp` are
  NOT gates** — they report "Finished" in ~1 s without compiling the test
  target. Only `cargo test --bin cimp` (~220 s) is real.
- `vitest` does **not** type-check. TS changes need `svelte-check`.
- Baselines to hold: cargo **2034 passed / 0 failed / 5 ignored** (the run's
  starting baseline was **2025/0/5** — *not* the 2020/0/5 this plan first
  briefed, which was stale; R1's seam-ledger and minimal-env tests took it to
  2034), clippy clean, vitest **636**, svelte-check **333 / 0 / 0**.
- Two `audit::runner` kill-timing tests fail spuriously under contention at
  `--test-threads=32` (F-17). **Always re-run them alone before calling a
  regression.**
- **Challenge the brief.** Thirteen claims were falsified in the V32 run, three
  of them the orchestrator's. Report contradictions as facts, do not silently
  work around them.
