# Milestone V19: OpenCode Replaces Aider (MCP + Code Graph + Offload Integration)

> **Schema:** bumps `CURRENT_SCHEMA_VERSION` 18 → 19. The V-number tracks
> the schema version (V14 stamped 14, …, V18 stamped 18), so this milestone
> is V19 and its migration step is `migrate_v18_to_v19`.

## Purpose

Replace the two Aider AI-tool tabs (`aider`, `aider-local`) with two
**OpenCode** tabs (`opencode`, `opencode-local`), and wire OpenCode into the
same ccImp capabilities the Claude tabs already enjoy: the **offload tool**,
the **code knowledge graph**, and the **web-research MCP servers** — all of
which already ride the single `ccimp --offload-mcp` child. The decision to go
with OpenCode (over keeping Aider) is recorded in the research that preceded
this milestone: OpenCode is an autonomous, MCP-native, multi-model agent that
is far closer to Claude Code's model than Aider's manual pair-programming flow.

The integration hinges on two facts verified against the installed binary
(`opencode-ai` **v1.17.11**, Windows x64):

- **`opencode --mini`** ("minimal interactive interface") renders **inline**
  into the normal scrollback buffer (it maintains a replay buffer and replays
  on resize) rather than taking the alternate screen. This is the *same
  rendering class* as Claude Code's `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`
  inline renderer that cctts already processes — so the TTS marker stripper
  (`processing/screen.rs`) and permission detector (`processing/permission.rs`)
  have a fighting chance. The **default** `opencode` TUI is a full-screen
  redraw (synchronized-update + absolute cursor addressing) and **must not**
  be used. `--mini` requires a real TTY — cctts spawns every tab in a PTY, so
  that requirement is satisfied.
- **`OPENCODE_CONFIG_CONTENT`** is an env var that takes the *entire* OpenCode
  config as an inline JSON string. Combined with `OPENCODE_DISABLE_PROJECT_CONFIG`
  this lets cctts inject MCP servers, instructions, and the local provider
  **session-scoped via env**, never writing to `~/.config/opencode` or the
  project — the exact analog of how the Claude tabs get `--mcp-config` /
  `--settings` / `--append-system-prompt`, just delivered as one env var.

Unlike Aider (which cctts never spoke through — no `--append-system-prompt`
equivalent, line-based REPL), OpenCode can be given the TTS-markup convention
via the `instructions` config key, so **the OpenCode tabs can speak** like the
Claude tabs. That is a capability upgrade, gated behind a verification step.

## Verified facts (opencode v1.17.11)

| Concern | Mechanism |
|---|---|
| Inline (linear) interactive mode | `opencode --mini` — inline scrollback render; needs a TTY (PTY supplies it). |
| Headless | `opencode run [msg] --format default\|json` (not used by the interactive tab). |
| Session-scoped config injection | `OPENCODE_CONFIG_CONTENT` (inline JSON), `OPENCODE_CONFIG` (path), `OPENCODE_CONFIG_DIR` (dir), `OPENCODE_DISABLE_PROJECT_CONFIG=1` (hermetic). |
| MCP config key | top-level **`mcp`**; local = `{"type":"local","command":[exe,"--offload-mcp"],"environment":{…}}`; remote = `{"type":"remote","url":…}`. |
| System-prompt injection | **`instructions`**: array of file paths/globs whose contents are appended. |
| Local provider | `provider` block (OpenAI-compatible `options.baseURL`/`apiKey`) + model as `provider/model`; exact `$defs.ProviderConfig` keys to confirm at impl. |
| Noise suppression | `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT=1`, `OPENCODE_DISABLE_TERMINAL_TITLE=1`, `OPENCODE_GIT_BASH_PATH` (Windows). |
| Install | npm `opencode-ai`, brew `anomalyco/tap/opencode`, curl installer. Single ~158 MB native binary. |

The config schema lives at `https://opencode.ai/config.json` (the `$schema`
value). Pin the relevant `$defs` (`McpLocalConfig`, `McpRemoteConfig`,
`ProviderConfig`, the `instructions` array) into a test fixture so a future
schema change is caught.

## What This Milestone Delivers

Five phases, ordered by risk: A (the integration spine: launch with `--mini`
+ env-config injection), B (drop Aider), C (OpenCode tabs + settings), D
(MCP/graph/offload plumbing to OpenCode), E (migration v18 → v19).

### Phase A — OpenCode launch spine (`--mini` + env config)

1. **`--mini` is mandatory for the interactive tab.** `build_extra_args`
   (`tabs/config.rs`) prepends `--mini` for any tab whose command resolves to
   `opencode`, ahead of the user's own `cfg.args`. Document that the default
   full-screen TUI breaks the linear-stream assumptions; `--mini` is the
   contract.
2. **`OPENCODE_CONFIG_CONTENT` env injection** in `compose_ai_env`. For an
   `opencode`-command tab, synthesize a single JSON document carrying the
   `mcp`, `instructions`, and (when local) `provider` blocks (Phases C/D), and
   set it as `OPENCODE_CONFIG_CONTENT`. Also set the noise-suppression env vars
   above. Per-tab `env` still wins (same `.entry()`-style precedence as today).
3. **Config merge vs hermetic. → STARTING additive (with a hermetic toggle).**
   cctts does *not* set `OPENCODE_DISABLE_PROJECT_CONFIG=1` by default, so a
   user's project `AGENTS.md` / `.opencode.json` still applies with the cctts
   `mcp` server + instructions layered on top via the env content. Verify
   OpenCode's merge precedence (env content vs project file) during A.4; expose
   a setting to force hermetic (`OPENCODE_DISABLE_PROJECT_CONFIG=1`) if
   collisions bite. This is reversible, so it does not gate the milestone.
4. **Marker-stripping + permission-detection verification against `--mini`.**
   This is the one residual unknown the deep dive could not fully exercise from
   a non-TTY pipe. Launch an `opencode --mini` tab and confirm: (a) assistant
   text flows in the normal buffer (no alt-screen takeover), (b) the TTS marker
   stripper finds `[[TTS]]…[[/TTS]]` in the stream, (c) the permission detector
   fires on OpenCode's permission prompt. If (b)/(c) fail, treat as a
   per-renderer fix in the same class as the existing Claude inline support —
   not a redesign. Characterize the permission-prompt string via
   `RUST_LOG=perm_capture=debug` (same approach memory records for Claude's
   "Esc to cancel · Tab to amend").

### Phase B — Drop Aider

5. **`AiTabId` enum** (`settings/schema.rs`): `Aider` → `OpenCode`,
   `AiderLocal` → `OpenCodeLocal`. `canonical_order` unchanged (claude 0,
   claude-local 1, opencode 2, opencode-local 3); `uses_local_provider`,
   `as_str`, `from_id` updated. Tab ids become `"opencode"` /
   `"opencode-local"`.
6. **Reserved-id constants:** `AIDER_TAB_ID`/`AIDER_LOCAL_TAB_ID` →
   `OPENCODE_TAB_ID = "opencode"` / `OPENCODE_LOCAL_TAB_ID = "opencode-local"`.
   Keep the literal strings `"aider"`/`"aider-local"` *only* inside the
   migration detector (Phase E) so old files are still recognized.
7. **Default-tab builders:** `default_aider_tab` → `default_opencode_tab`,
   `default_aider_local_tab` → `default_opencode_local_tab` (command
   `"opencode"`, names "OpenCode" / "OpenCode (local)", `--mini` arrives via
   `build_extra_args` not as a stored arg, `tts_injection.enabled = true`).
   `default_ai_tab` match arms updated.
8. **State side** (`state/manager.rs`): the `AiToolKind` reserved variants and
   doc comments for Aider → OpenCode.
9. **Aider permission patterns** removed from `processing/permission.rs`; add
   the OpenCode `--mini` permission pattern (Phase A.4 characterizes it).
10. **Aider tab gating** (added in v0.20.0, `ipc/tab_lifecycle.rs` /
    lifecycle): the "reject enabling when `aider` is unresolvable" guard
    becomes "reject when `opencode` is unresolvable." See Phase C.7 for the
    bundling decision that affects resolvability.
11. **Frontend literals:** `src/lib/terminals.ts` `displayNameFor`
    (`aider`/`aider-local` → `opencode`/`opencode-local`), `src/lib/tabs/types.ts`,
    `src/lib/settings/types.ts`, `TabSettingsSection.svelte`, `SettingsApp.svelte`
    — every `aider` literal in live code. Grep must come back clean (Test Plan).
12. **Docs:** `docs/DESIGN.md`, `README.md`, `docs/FUTURE-FEATURES.md` (move any
    aider entries to historical), `CHANGELOG.md`. The stale
    `docs/MILESTONE-V1.4-07-claude-local-and-aider-removal.md` (an *earlier,
    superseded* aider-removal plan that was never executed as written) gets a
    one-line "superseded by V19" note rather than deletion, to preserve history.

### Phase C — OpenCode tabs + local-provider settings

13. **Settings group rename:** `aider_local: AiderLocalSettings` →
    `opencode_local: OpencodeLocalSettings`. Fields: `base_url`, `auth_token`,
    `model` (carried over; `model` now means an OpenCode `provider/model`
    string or a bare model name resolved against the synthesized local
    provider). Default `base_url` to a common local OpenAI-compatible port
    (e.g. `http://localhost:1234/v1`); document that OpenCode expects the
    OpenAI-compatible `/v1` suffix where LM Studio/llama-server do.
14. **TS mirror + defaults** (`src/lib/settings/types.ts`,
    `src/lib/settings/store.ts`): `opencode_local`, default tab `command`.
15. **Settings UI AI section** (`SettingsApp.svelte`): rename "Aider local
    provider" → "OpenCode local provider"; base URL / token / model fields;
    help text naming OpenCode + that cctts launches it with `--mini` and does
    not install it. Link `https://opencode.ai/docs`.
16. **Per-tab UI** (`TabSettingsSection.svelte`): the `use_local_provider`
    toggle and effective-config helper text now describe the OpenCode env
    injection (show that `OPENCODE_CONFIG_CONTENT` is synthesized).
17. **Binary availability / bundling decision. → DECIDED: require install.**
    OpenCode is a single ~158 MB native binary. cctts does **not** bundle it:
    `resolve_command("opencode")` accepts `opencode` from `ebin/` (resolved
    before PATH) or PATH, and an unresolvable binary produces a clear gate error
    pointing at `https://opencode.ai/docs` (or "drop `opencode.exe` in `ebin/`"),
    mirroring today's aider gate. Bundling `opencode.exe` in `ebin/` is tracked
    as a followup once demand is clear (158 MB inflates the portable zip
    materially). The Windows binary is `opencode-windows-x64`.

### Phase D — MCP, code graph, and offload reach OpenCode

The single `ccimp --offload-mcp` child already exposes `offload_task`, the
`graph_*` tools, and the user MCP servers flagged for a given consumer. The
Claude tabs reach it via `--mcp-config`. OpenCode reaches the **same child**
via the injected `mcp` block.

18. **`mcp` block in the injected config.** When
    `settings.offload.enabled || settings.graph.enabled || <any server exposed
    to OpenCode>`, add to `OPENCODE_CONFIG_CONTENT`:

    ```jsonc
    "mcp": {
      "ccimp-offload": {
        "type": "local",
        "command": ["<current_exe>", "--offload-mcp", "--consumer", "opencode"]
      }
    }
    ```

    Mirror the existing Claude gate (`build_pre_args`) so the server is injected
    whenever offload, graph, **or** an OpenCode-exposed MCP server is in play.
19. **Per-consumer MCP exposure. → DECIDED: dedicated `opencode_access` flag.**
    `McpServerConfig` gains **`opencode_access: bool`** alongside `claude_access`
    + `offload_access` for symmetric per-server control. The `--offload-mcp`
    child learns a **`--consumer opencode`** discriminator: today it is hard-wired
    to the Claude tool set (`mcp.rs` → `tool_defs_for_claude`), so this means
    parsing `--consumer` in `main.rs`/`offload::mcp::run`, threading it to the
    loopback/host so `tool_defs_filtered(consumer)` selects the OpenCode server
    set (generalize the `claude: bool` arg at `mcp_host.rs:530` to a consumer
    enum, add an opencode arm). Migration defaults `opencode_access` to each
    server's existing `claude_access` value so upgraders keep their web-research
    tools. The Settings MCP editor gains a third per-server checkbox column.
20. **`instructions` injection (TTS + offload + graph guidance).** OpenCode's
    `instructions` key takes file paths, so write a managed instructions file
    (e.g. `<config-dir>/opencode/instructions.md`) composed from the same
    sources the Claude pre-args use — `crate::tts::RUNTIME_SYSTEM_PROMPT` (TTS
    markup, gated on `tts_injection.enabled`), `OFFLOAD_GUIDANCE` (gated on
    `offload.inject_guidance`), `GRAPH_GUIDANCE` (+ `GRAPH_SEMANTIC_GUIDANCE`
    when `graph.semantic_search`) — and reference it via
    `"instructions": ["<path>"]` in the injected config. Reuse the existing
    guidance constants verbatim so Claude and OpenCode stay in lockstep. Write
    the file at spawn (idempotent overwrite) so it tracks live settings.
21. **Local provider block.** When `use_local_provider`, add a `provider` block
    pointing the OpenAI-compatible provider at `opencode_local.base_url` /
    `auth_token`, and set the default model to `opencode_local.model`
    (as `provider/model`). Confirm exact `ProviderConfig` keys against
    `$defs.ProviderConfig`; cloud OpenCode (`use_local_provider:false`) injects
    no provider block and uses OpenCode's own credentials.

### Phase E — Migration v18 → v19

22. **`migrate_v18_to_v19`** (`settings/migration.rs`), following the
    established pattern (stamp a *literal* 19, not `CURRENT_SCHEMA_VERSION`):

    a. **Rename the settings group:** `aider_local` → `opencode_local`
       (carry `base_url`/`auth_token`; `model` preserved). If absent, stamp the
       new default.
    b. **Rewrite reserved tabs in place** — `aider` → `opencode`,
       `aider-local` → `opencode-local`: change `id`, `command`
       (`"aider"`→`"opencode"`), `name`; **preserve** `use_local_provider` and
       per-tab `env`; reset `args` to `[]`; set `tts_injection.enabled = true`
       (OpenCode can speak — an upgrade over the silent aider tab). Drop any
       stored `--model` arg (now synthesized).
    c. **Rewrite layout-tree, `session.active_tab_id`, and layout-preset
       references** `aider`→`opencode` and `aider-local`→`opencode-local`
       (reuse the recursive `rewrite_layout_tab_id` helper pattern from the
       V1.4-07 design).
    d. **`enabled_ai_tabs`:** map `aider`→`opencode`, `aider-local`→`opencode-local`.
    e. **`mcp_servers[*].opencode_access`:** default to each server's
       `claude_access` (Phase D.19).
    f. Stamp `schema_version: 19`; back up at `config.json.v18.bak.<ts>`.
23. **Reserved-id list** in `tabs/registry.rs` `is_builtin_id` /
    `integrity_check`: `AIDER_*` → `OPENCODE_*`. A user who deleted their aider
    tabs and never migrated gets `opencode`/`opencode-local` restored from the
    new default builders.
24. **Cascade tests** grow: a v18 file with the standard
    claude/claude-local/aider/aider-local set lands at v19 with the aider tabs
    rewritten as opencode, layout references rewritten, and the offload/graph
    settings untouched. Plus the long v1.3→v19 cascade backup-count test.

## Key Deltas vs Prior Milestones

- **First AI tab that both speaks *and* is a non-Claude agent.** Aider was
  silent (no system-prompt injection path); OpenCode gets the TTS markup
  convention via `instructions`, so the avatar/TTS pipeline applies to a
  second agent for the first time. The marker-stripping verification (A.4) is
  the load-bearing check.
- **First env-var config injection.** Claude uses CLI flags
  (`--mcp-config`/`--settings`/`--append-system-prompt`); OpenCode uses one
  `OPENCODE_CONFIG_CONTENT` env var carrying the equivalent JSON. New surface
  in `compose_ai_env`; the merge-vs-hermetic decision (A.3) is the key design
  call.
- **First third MCP consumer.** `claude_access`/`offload_access` becomes a
  trio with `opencode_access`, and the `--offload-mcp` child learns a
  `--consumer` discriminator (`mcp_host.rs`).
- **Schema 18 → 19** with an in-place reserved-tab *rewrite* (id + command +
  name), the same transform class as the V14 aider-introduction and the
  V1.4-07 plan.

## What This Milestone Does NOT Do

- **Use the default full-screen OpenCode TUI.** `--mini` only. The default TUI
  is incompatible with the linear-stream model and is never launched.
- **Install or bundle OpenCode** (unless Phase C.17 picks bundling). The user
  installs `opencode`; cctts gates a clear error when it's unresolvable.
- **Headless `opencode run` integration.** The tab is interactive (`--mini`).
  A non-interactive `opencode run` backend (e.g. as an offload-style tool) is
  out of scope; track as a followup if useful.
- **A dedicated OpenCode provider-config UI** beyond base URL / token / model.
  Multi-provider OpenCode config (Gemini, Copilot, etc.) is left to the user's
  own `~/.config/opencode` / project config, which still applies in additive
  mode (A.3).
- **Migrate Aider customizations beyond env.** Per-tab `env` is preserved;
  bespoke `--model`/args are reset to OpenCode defaults. Documented in the
  CHANGELOG migration notes.
- **Rename `TabConfig::AiTool` / `AiToolTabConfig`.** Still the generic AI-tab
  carrier; no wide mechanical rename.

## Files Most Likely Touched

**Phase A/C/D (launch + config)**
- `src-tauri/src/tabs/config.rs` — `--mini` in `build_extra_args`; OpenCode
  branch in `compose_ai_env` (`OPENCODE_CONFIG_CONTENT` + noise-suppression
  env); shared guidance constants reused for the instructions file; tests.
- `src-tauri/src/settings/schema.rs` — `AiTabId` variants, `OPENCODE_*` id
  consts, `OpencodeLocalSettings`, `opencode_local`, `default_opencode_tab` /
  `default_opencode_local_tab`, `default_ai_tab`, `McpServerConfig.opencode_access`.
- `src-tauri/src/offload/mcp_host.rs` (+ `mcp.rs`, `loopback.rs`) —
  `--consumer opencode` selection; `opencode_access` plumbing.
- new helper to write the managed `instructions.md` (alongside statusline/
  config-dir helpers).

**Phase B (drop aider)**
- `src-tauri/src/state/manager.rs`, `src-tauri/src/processing/permission.rs`,
  `src-tauri/src/ipc/tab_lifecycle.rs`, `src-tauri/src/tabs/registry.rs`.
- `src/lib/terminals.ts`, `src/lib/tabs/types.ts`, `src/lib/settings/types.ts`,
  `src/lib/settings/TabSettingsSection.svelte`, `src/SettingsApp.svelte`.
- `README.md`, `docs/DESIGN.md`, `docs/FUTURE-FEATURES.md`, `CHANGELOG.md`.

**Phase E (migration)**
- `src-tauri/src/settings/migration.rs` — `migrate_v18_to_v19`, `looks_v18`,
  layout-id rewrite, tests.
- `src-tauri/src/settings/schema.rs` — `CURRENT_SCHEMA_VERSION = 19`.

## Test Plan

### Phase A
- **Manual (the gating check):** launch an OpenCode tab; confirm `--mini`
  renders inline (no alt-screen flash, scrollback intact on tab switch), the
  marker stripper finds `[[TTS]]` and the avatar speaks, and the permission
  detector fires on an edit-permission prompt.
- **Unit (Rust):** `compose_ai_env` for an opencode tab sets
  `OPENCODE_CONFIG_CONTENT` parseable as JSON with the expected `mcp` /
  `instructions` keys; per-tab `env` overrides a synthesized key;
  `build_extra_args` contains `--mini` for opencode and not for claude/shell.

### Phase B
- **grep** `aider`/`Aider`/`AIDER` across `src/` + `src-tauri/src/` + non-historical
  docs → only matches in `completedMilestones/`, `CHANGELOG` history, the
  V1.4-07 "superseded" note, and the V19 migration detector.
- **Build:** `cargo build` + `npm run build` clean.

### Phase C/D
- **Unit (Rust):** injected config includes the `ccimp-offload` `mcp` entry
  when offload/graph/an opencode-exposed server is on, and omits it when all
  are off; the instructions file contains TTS + offload + graph guidance under
  the right gates; `opencode_access` selects the right server set under
  `--consumer opencode`.
- **Manual (end-to-end):** in an OpenCode `--mini` tab, confirm `offload_task`,
  a `graph_*` query, and a web-research MCP tool all work — proving OpenCode
  reaches the same ccImp child the Claude tabs use. Local-provider variant:
  point `opencode_local` at a running local endpoint, confirm the model
  responds.

### Phase E
- **Unit (Rust):** v18 → v19 rewrites both aider tabs (id/command/name,
  preserves env + `use_local_provider`), rewrites layout/preset/active-tab
  references, renames `aider_local`→`opencode_local`, defaults
  `opencode_access` from `claude_access`, stamps 19, backs up. v19 not
  re-detected. Full v1.3→v19 cascade backup count.
- **Manual:** hand-authored v18 file with aider tabs placed in a specific pane
  → after launch the pane shows `opencode`/`opencode-local`, layout intact,
  per-tab env preserved.

## Risks and Open Questions

- **`--mini` marker-stripping / permission-detection fidelity (primary).** The
  deep dive confirmed inline rendering but could not exercise the exact escape
  sequences without a PTY. Inline mode still uses cursor addressing for its
  status/spinner line; the stripper and detector must tolerate it. Mitigation:
  A.4 is the first implementation step, and the existing Claude inline support
  is the reference. If OpenCode interleaves tool-call UI in a way the stripper
  mis-segments, scope a small per-renderer adjustment — same class as prior
  Claude fixes — not a redesign.
- **OpenCode honoring the TTS-markup instructions.** Whether OpenCode reliably
  wraps prose in `[[TTS]]…[[/TTS]]` per the injected instructions is
  model-dependent (it's a different harness than Claude Code). If markup is
  inconsistent, the OpenCode tab degrades to non-speaking (like aider today) —
  acceptable fallback; document it. Consider OpenCode's `tts_all_output` path
  as an alternative if marker injection is unreliable.
- **Config merge precedence (A.3).** If `OPENCODE_CONFIG_CONTENT` does *not*
  merge cleanly with a user's project `.opencode.json` (e.g. one wholesale
  replaces the other), the additive plan breaks. Verify early; fall back to
  hermetic (`OPENCODE_DISABLE_PROJECT_CONFIG=1`) + surface the user's important
  keys ourselves if needed.
- **`provider` block shape for a local endpoint.** The exact `ProviderConfig`
  keys (npm package id vs `options.baseURL`) must be confirmed against the live
  schema; the local-provider path is gated on getting this right. Cloud
  OpenCode is unaffected (no provider block).
- **Windows + Git Bash.** OpenCode reads `OPENCODE_GIT_BASH_PATH`; confirm
  whether the `--mini` tab needs it set on this Windows dev machine, and pass
  it through when present.
- **158 MB binary.** If Phase C.17 bundling is chosen, the portable zip grows
  substantially; default is "require install" to avoid that.

## Followups Tracked Elsewhere

- **Bundle `opencode.exe` in `ebin/`** if "did I install opencode" friction is
  real (FUTURE-FEATURES).
- **Headless `opencode run` as an offload-style tool** — a second way to use
  OpenCode non-interactively.
- **Per-tab OpenCode agent/model selection** (build vs plan agent, model
  switching) surfaced in cctts settings rather than OpenCode's own UI.
- **OAuth MCP servers** — OpenCode supports `mcp auth`; if a user adds an
  OAuth-gated remote MCP server, the auth flow lives in OpenCode, not cctts.
