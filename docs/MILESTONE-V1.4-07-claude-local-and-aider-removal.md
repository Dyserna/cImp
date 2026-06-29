# Milestone V1.4-07: Claude Code Local-Provider Tab + Aider Removal

> **Superseded by V19.** This earlier plan removed the original aider tab in
> favor of a Claude local-provider tab. Aider was later re-introduced (V14) and
> is now replaced by OpenCode in `docs/MILESTONE-V19-opencode-replaces-aider.md`.
> Retained for the in-place reserved-tab-rewrite + layout-id-rewrite migration
> pattern that V19's `migrate_v18_to_v19` reuses.

> **Release tag:** the user has chosen to ship this as `v1.3.3`. Milestone series numbering (V1.4-XX) is independent of the git tag, mirroring V1.4-01..04 which shipped as v1.3.2. Numbers V1.4-05 and V1.4-06 belong to the cancelled per-tab avatar / TTS plans (deleted in v1.3.2); this milestone takes the next free slot.

## Purpose

Three connected changes shipped under one milestone:

- **A. Per-tab appearance reaches the AI-tab Configure flow.** The schema and Settings → Tabs UI already expose `theme_override` and `background_override` for AI tabs (`AiToolTabConfig` carries the fields; `TabSettingsSection.svelte` renders the rows). The gap is the right-click **Configure tab** entry: today it opens `ConfigureTabDialog.svelte`, which is hardcoded shell-only (calls `getShellTabConfig` / `reconfigureShellTab`). For AI tabs, that entry will instead open Settings → Tabs scoped to the right tab, surfacing the existing per-tab Appearance section. No new dialog work, no risk of regressing the shell flow.
- **B. Drop the Aider tab kind.** The `AiToolKindWire` enum collapses to a single ClaudeCode-only flavor (or is removed entirely — see B.1). Aider-specific code paths are deleted: the `AiderFirstLaunchNotice` component, aider permission-detection patterns, the `AIDER_TAB_ID` constant, `default_aider_tab()`, and the related lines across the 14 backend files and 10 frontend files that name aider today.
- **C. Add a "Claude Code (local LLM)" provider option.** A new global `claude_local: { base_url, auth_token, model_alias }` settings group, plus a per-tab `use_local_provider: bool` flag on AI tabs. When the flag is true, the launch flow synthesizes `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` (and optionally `ANTHROPIC_MODEL`) into the spawned process's env. The local backend speaks the Anthropic Messages API directly — LM Studio (≥0.4.1), Ollama, vLLM, and llama.cpp's `llama-server` (PR #17570, `--jinja` for tool use) all expose a native `/v1/messages` endpoint, so no translation proxy is required. Backends that are OpenAI-only need a separate translator (e.g. `anthropic-proxy-rs`) run by the user; cctts is agnostic to that. The default install ships two AI tabs: "Claude" (subscription, `use_local_provider: false`) and "Claude (local)" (`use_local_provider: true`).

The phases are independently shippable and ordered A → B → C because A is the lowest-risk change (one route swap), B is mostly deletions, and C builds the new provider config on top of B's simplified schema.

## What This Milestone Delivers

**Phase A — AI-tab Configure routes to Settings → Tabs**

1. The right-click context menu's **Configure tab** entry, when invoked on an AI tab, opens the Settings window with `?tab=<tab-id>` (or an equivalent IPC payload) and scrolls/expands `TabSettingsSection.svelte` for that tab. Shell tabs continue to open `ConfigureTabDialog.svelte` as today.
2. New IPC `open_settings_window_to_tab(tab_id)` (or extension of the existing `openSettingsWindow`) — the Tauri main window opens the Settings window and posts a one-shot event the Settings frontend listens for to set its initial scroll/select.
3. `TabSettingsSection.svelte` already renders the Appearance subsection (theme override + background override + nested `BackgroundConfigEditor`) for any tab kind. No content changes — only the route in.
4. **Spot-verify runtime application.** `terminals.ts:280-411` already calls `effectiveTheme(tab, ...)` and `effectiveBackgroundMode(tab, ...)` keyed off the tab id, with no kind-discriminating branch. AI tabs go through the same `createTerminal(tabId)` path as shell tabs, so per-tab overrides should already render correctly. This step is a manual smoke test — set a Solarized Light override on the Claude tab, set a custom background, confirm both render — not a new code path.

**Phase B — Drop Aider**

5. `AiToolKindWire` enum collapses. Two equivalent shapes; pick at impl time:

   - **B.1.a:** Remove `AiToolKindWire` entirely. `AiToolTabConfig` no longer carries `ai_tool_kind`. Implies AI tabs are simply "Claude Code" with optional local-provider config.
   - **B.1.b:** Keep the enum for future extension, drop only the `Aider` variant. Slightly more code, leaves room for a future `Codex` / `Cline` variant without a schema bump.

   Default to **B.1.a** (delete entirely) since the Codex/Cline future is hypothetical and the simpler shape costs nothing to re-add later if a real second variant emerges. State-side `AiToolKind` enum (in `state::manager`) follows the same fate.
6. `AIDER_TAB_ID` constant removed; integrity check no longer reseeds an aider tab. `default_aider_tab()` removed. `default_claude_local_tab()` (Phase C.5) takes its place in `Settings::default()` for fresh installs.
7. `src/lib/AiderFirstLaunchNotice.svelte` deleted. `App.svelte`'s mount of it removed. `first_launch_notice_dismissed` field stays on `AiToolTabConfig` for now (it was always a per-tab flag, just only used by aider in practice) but becomes dead. Remove it in a follow-on cleanup; not worth a schema bump for one bool.

   *Decision deferred to impl time:* if no other call site uses `first_launch_notice_dismissed` after aider is gone (likely), drop it in this milestone's migration to keep the schema honest. Otherwise leave for later.
8. Aider permission-detection patterns in `src-tauri/src/processing/permission.rs` deleted. The pattern doc in `FUTURE-FEATURES.md` (under "External dependencies → Aider permission detection patterns") moves to the historical section with the cancellation rationale.
9. Aider TTS-injection deferred work in `FUTURE-FEATURES.md` (under "External dependencies → Aider TTS markup injection") moves to the historical section. cctts no longer has an aider tab to inject into; the upstream-aider blocker is irrelevant to cctts going forward.
10. README, `docs/DESIGN.md`, `docs/CLAUDE.md` (if it mentions aider), and `docs/features/FEATURE-aider-parity.md` updated. The feature doc moves into a clearly-archival state — either deleted outright or prefixed with a "**Closed: aider removed in v1.3.3**" note. Default to deletion since the feature is no longer aspirational; the rationale lives in CHANGELOG.

**Phase C — Local-provider config + second Claude tab**

11. New `ClaudeLocalSettings` group:

    ```rust
    #[derive(Clone, Serialize, Deserialize, Debug)]
    #[serde(default)]
    pub struct ClaudeLocalSettings {
        /// Anthropic-compatible endpoint URL (e.g. http://localhost:1234
        /// for LM Studio). Becomes ANTHROPIC_BASE_URL in the spawned
        /// process's env.
        pub base_url: String,
        /// Auth token. Local backends typically accept any string;
        /// "sk-dummy" is a common placeholder. Becomes ANTHROPIC_AUTH_TOKEN.
        pub auth_token: String,
        /// Optional model alias. Empty = use Claude Code's default model
        /// selection (the backend resolves `claude-*` names per its own
        /// config).
        pub model_alias: String,
    }

    impl Default for ClaudeLocalSettings {
        fn default() -> Self {
            Self {
                // LM Studio's default port. llama-server uses 8080,
                // Ollama 11434, vLLM 8000 — user retargets in Settings.
                base_url: "http://localhost:1234".to_string(),
                auth_token: "sk-dummy".to_string(),
                model_alias: String::new(),
            }
        }
    }
    ```

    Lives at `Settings::claude_local`, alongside `terminal`, `tts`, `avatar`, etc.
12. `AiToolTabConfig` gains `pub use_local_provider: bool` (default `false`). `Default` impl and `default_claude_tab()` set it to `false`; `default_claude_local_tab()` (new) sets it to `true`.
13. `default_claude_local_tab()`:

    ```rust
    pub const CLAUDE_LOCAL_TAB_ID: &str = "claude-local";

    pub fn default_claude_local_tab() -> TabConfig {
        TabConfig::AiTool(AiToolTabConfig {
            id: CLAUDE_LOCAL_TAB_ID.to_string(),
            builtin: true,
            name: "Claude (local)".to_string(),
            command: "claude".to_string(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            use_local_provider: true,
            tts_injection: TtsInjection {
                enabled: true,
                instructions: crate::tts::RUNTIME_SYSTEM_PROMPT.to_string(),
            },
            notifications: AiNotificationConfig {
                idle: "Claude (local) is idle".to_string(),
                awaiting_permission: "Claude (local) is awaiting permission".to_string(),
                error: "Claude (local) encountered an error".to_string(),
            },
            first_launch_notice_dismissed: true,
            theme_override: None,
            background_override: None,
        })
    }
    ```
14. **Launch-time env composition.** Wherever the AI tab's PTY is spawned (`src-tauri/src/pty/manager.rs` / `tabs/registry.rs`), if `use_local_provider == true`, merge synthesized env on top of the tab's existing `env` HashMap before spawn:

    ```rust
    let mut env = tab.env.clone();
    if tab.use_local_provider {
        let cl = settings.claude_local.clone();
        env.insert("ANTHROPIC_BASE_URL".into(), cl.base_url);
        env.insert("ANTHROPIC_AUTH_TOKEN".into(), cl.auth_token);
        if !cl.model_alias.is_empty() {
            // Claude Code uses --model flag, not env; pass via args
            // instead. But ANTHROPIC_MODEL is sometimes respected by
            // proxies — set both. Verify at impl time.
            env.insert("ANTHROPIC_MODEL".into(), cl.model_alias.clone());
        }
    }
    ```

    Per-tab `env` entries take precedence over synthesized ones (the user can still override per-tab if they need a different backend on a specific tab) — flip the merge order if so.

    **Decision at impl time:** do per-tab `env` entries override synthesized values, or vice versa? Recommend tab-env wins (it's the more specific scope) and document.
15. **Settings UI: AI section.** A new "AI" section in the Settings window (or a subsection of the existing Tabs section) exposes:
    - Base URL (text input)
    - Auth token (password-masked input with a show/hide toggle; cleartext storage in settings.json — documented)
    - Model alias (text input, optional)
    - Help text naming the supported backends (LM Studio, Ollama, vLLM, llama-server) and noting that the backend must be running separately (cctts does not auto-spawn it).
16. **Per-tab UI: Tabs section.** `TabSettingsSection.svelte` for AI tabs gains a "Use local LLM" checkbox bound to `use_local_provider`. When checked, the tab's effective env shows `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` synthesized — render this as read-only helper text below the existing per-tab `env` editor so the user understands what's being injected.
17. **Restart semantics.** Toggling `use_local_provider` or editing `claude_local.*` doesn't restart running tabs. The change takes effect on next tab restart. Add a "Restart required" pill on the affected tab(s) when these fields change while a tab is running, mirroring the existing "Restart required" pattern in Settings (see V5-02's `<Pill>` primitive).

**Phase D — Migration v1.7 → v1.8**

18. New migration step `migrate_v1_7_to_v1_8` in `src-tauri/src/settings/migration.rs`:

    a. **Rewrite the aider tab in place.** The aider tab keeps its id (`"aider"`) so layout-tree references stay valid; rename to `claude-local` only via a follow-on layout fix-up — *or* preserve the id `"aider"` and live with the legacy id in `settings.json` for that one tab. **Recommend preserving the id** to keep the migration trivial and the layout tree untouched. Update `kind` (drop discriminator if B.1.a), `name = "Claude (local)"`, `command = "claude"`, `args = []`, `use_local_provider = true`. Keep the user's existing `env` (if they had a local-LLM proxy URL set there manually, it's preserved as a per-tab override on top of the new global config).
    b. Remove `ai_tool_kind` from every AI tab object (B.1.a path) — schema cleanup.
    c. Add `use_local_provider: false` to every other AI tab.
    d. Add the new top-level `claude_local: { base_url: "http://localhost:1234", auth_token: "sk-dummy", model_alias: "" }` block.
    e. (If chosen in step 7) Remove `first_launch_notice_dismissed` from every AI tab.
19. Backup at `config.json.v1.7.bak.<ts>`. Migration cascade tests grow: a v1.3.0 file lands at v1.8 with five backups.
20. Layout integrity check (`tabs/registry.rs`'s startup repair): the reserved-id list changes from `[CLAUDE_TAB_ID, AIDER_TAB_ID, SHELL_DEFAULT_TAB_ID]` to `[CLAUDE_TAB_ID, CLAUDE_LOCAL_TAB_ID, SHELL_DEFAULT_TAB_ID]`. If a user has manually deleted both their aider/claude-local tabs and the migration above didn't run (somehow), the integrity check restores `claude-local` from `default_claude_local_tab()`.

    Edge case: user is on v1.7 with a deleted aider tab. The migration's "rewrite aider in place" step finds nothing to rewrite. The integrity check then restores `claude-local`. Net result: user gets the new tab, no data lost. Document.

**Cross-cutting**

21. README updated: replace the "Claude / aider" framing with "Claude (subscription) / Claude (local LLM)". Add a paragraph naming the supported backends — LM Studio, Ollama, vLLM, llama-server — and noting that all four expose a native Anthropic Messages endpoint, so no translation proxy is needed. Mention `anthropic-proxy-rs` once as the recommended translator for OpenAI-only backends. Note that cctts does not start the backend, and that env-var precedence is per-tab `env` > synthesized > nothing.
22. `docs/DESIGN.md` updated: replace the aider tab description with the local-provider model. Document the `claude_local` settings group and `use_local_provider` per-tab flag.
23. `CHANGELOG.md` v1.3.3 entry: Added (claude-local provider, "Claude (local)" tab, AI section in Settings, AI-tab Configure routes to Settings → Tabs, scoped settings deep-link), Removed (Aider tab kind, AiderFirstLaunchNotice, aider permission patterns, aider parity feature doc), Migrated (v1.7 → v1.8 with the rewrite-in-place semantics).
24. `docs/FUTURE-FEATURES.md` updated: aider TTS injection and aider permission detection move from "External dependencies" to "Done / historical" with the cancellation note. `docs/features/FEATURE-aider-parity.md` deleted.

## Key Deltas vs V1.4-04

- **First removal-heavy milestone in V1.4 series.** V1.4-01..04 were all additive. V1.4-07 deletes `AiToolKindWire::Aider`, `AIDER_TAB_ID`, `default_aider_tab()`, `AiderFirstLaunchNotice.svelte`, and the aider permission patterns. The migration's rewrite-in-place step is the first migration that actively *transforms* an existing tab (rather than stamping a new field default). Test coverage for migration grows accordingly.
- **First env-synthesis-on-spawn.** Until V1.4-07, the spawn flow read `tab.env` and that was it. Phase C.4 introduces a settings-driven env synthesis layer between the tab config and the actual spawn. The merge order (tab-env vs synthesized) is the key design decision; it surfaces in tests and in the docs.
- **Second milestone to use the "open Settings scoped to X" pattern** (V4-04 introduced layout-preset deep links; V1.4-07 reuses the pattern for tab-scoped Settings). Worth checking if a small abstraction emerges; otherwise two-call-sites is fine, don't over-abstract.
- **Schema-version bump touches multiple unrelated fields.** v1.7 → v1.8 stamps `claude_local` (Phase C), drops `ai_tool_kind` from AI tabs (Phase B), rewrites the aider tab (Phase D), and optionally drops `first_launch_notice_dismissed` (Phase B step 7). One transform, four shape changes — keep the transform code well-commented so the migration intent is recoverable from the file alone.

## What This Milestone Does NOT Do

- **Auto-spawn the local backend.** cctts does not bundle, install, or start LM Studio, Ollama, llama-server, vLLM, or any external translator. The user runs the backend themselves (a Shell tab is a natural place to start it). Documented in README. A future "backend sidecar" feature is a candidate for FUTURE-FEATURES.md if real-use friction surfaces.
- **Bundle a local model** or any provider config beyond the env vars. cctts is provider-agnostic past the env-var injection. The user picks their own model in their backend's config (LM Studio, Ollama, llama-server, vLLM).
- **Multiple local-provider configs.** One global `claude_local` group; tabs are local or not. If the user wants a second local provider on a third tab, they set the env vars in the per-tab `env` HashMap directly (which is the existing v1.3 mechanism — already works).
- **Tool-use compatibility certification.** Local models vary in tool-call reliability. cctts displays whatever the model emits; broken tool use is a model issue, not a cctts bug. Documented in README under "Local LLM caveats."
- **Credential masking beyond a UI password input.** `auth_token` is stored cleartext in `settings.json`. Local backends typically accept dummy tokens, so this is acceptable. If a user puts a real Anthropic API key there (rather than a local backend token), it sits cleartext. Documented; OS keychain integration is a separate feature.
- **Right-click "Configure" extension to a kind-aware dialog.** The user explicitly chose the simpler route (open Settings → Tabs scoped to the tab) over extending `ConfigureTabDialog.svelte` to handle both shells and AI tabs. If discoverability turns out to bite, revisit; until then no shell-dialog regression risk.
- **Aider import path.** Users on aider with custom config (special args, env, cwd) get *some* of it preserved by the rewrite-in-place migration: `name` and `args` are reset to claude-local defaults, but per-tab `env` is preserved. Users who heavily customized their aider tab will need to re-set Claude-relevant args in Settings → Tabs after upgrade. Documented in CHANGELOG migration notes.
- **Renaming the "AI tool" framing in code.** `TabConfig::AiTool` and `AiToolTabConfig` keep their names — they're now Claude-only, but renaming to `ClaudeTab` is a wide-reaching mechanical refactor and not worth it inside this milestone. The naming is mildly aspirational ("AI tool" leaves room for future variants); revisit when concrete need surfaces.

## Implementation Steps

A → B → C → D in shipping order.

### Phase A — AI-tab Configure routes to Settings → Tabs

#### A.1 Settings deep-link IPC

`src-tauri/src/ipc/commands.rs` (or wherever `openSettingsWindow` lives today):

```rust
#[tauri::command]
pub async fn open_settings_window_to_tab(
    app: AppHandle,
    tab_id: String,
) -> AppResult<()> {
    open_settings_window(app.clone()).await?;
    // Settings window listens for `settings-deep-link` events and
    // scrolls/expands the matching section on receive.
    app.emit("settings-deep-link", json!({
        "kind": "tab",
        "tab_id": tab_id,
    }))?;
    Ok(())
}
```

Registered in `tauri::Builder::invoke_handler`.

#### A.2 Settings frontend handles the deep-link

`src/SettingsApp.svelte` mounts a one-time listener:

```ts
import { listen } from '@tauri-apps/api/event';

onMount(() => {
  const unsub = listen<{ kind: 'tab'; tab_id: string }>(
    'settings-deep-link',
    (e) => {
      if (e.payload.kind === 'tab') {
        scrollToTabSection(e.payload.tab_id);
      }
    },
  );
  return () => { void unsub.then((u) => u()); };
});

function scrollToTabSection(tabId: string): void {
  // The tabs list renders one TabSettingsSection per tab. Each gets a
  // stable id `tab-section-<tabId>` for the deep-link target.
  const el = document.getElementById(`tab-section-${tabId}`);
  if (el) {
    el.scrollIntoView({ block: 'start', behavior: 'smooth' });
    // Optional: pulse the section briefly to draw attention.
  }
}
```

Add `id={`tab-section-${tab.id}`}` on each `TabSettingsSection` wrapper in `SettingsApp.svelte`.

#### A.3 TabBar dispatches by kind

`src/lib/TabBar.svelte:238` currently always opens `ConfigureTabDialog`:

```ts
onConfigure={t ? () => openConfigureTabDialog(t.id) : undefined}
```

Replace with a kind-aware dispatch:

```ts
onConfigure={t
  ? () => {
      const kind = $tabs.find((x) => x.id === t.id)?.kind;
      if (kind === 'ai_tool') {
        void openSettingsWindowToTab(t.id);
      } else {
        openConfigureTabDialog(t.id);
      }
    }
  : undefined}
```

`openSettingsWindowToTab` is the new TS binding for the IPC in A.1.

#### A.4 Spot-test runtime application

Manual: open Settings → Tabs → Claude → Appearance. Set `theme_override` to Solarized Light. Confirm the Claude tab's terminal repaints to Solarized Light without restart. Same for `background_override` (set to a custom image, confirm the tab's background updates live via the `unsubAppearance` path in `terminals.ts`). If something doesn't apply: the V1.4-01..04 wiring assumed all tab kinds run through `createTerminal(tabId)` — confirm this is true for the Claude tab's xterm instance (it should be — the Claude tab is a PTY-backed terminal exactly like a Shell tab, just with a different command).

### Phase B — Drop Aider

#### B.1 Schema

Pick **B.1.a** (delete `AiToolKindWire`):

```rust
// In src-tauri/src/settings/schema.rs:

// DELETE:
//   pub enum AiToolKindWire { ClaudeCode, Aider }
//   pub const AIDER_TAB_ID: &str = "aider";
//   pub fn default_aider_tab() -> TabConfig { ... }

// REMOVE the `ai_tool_kind: AiToolKindWire,` field from AiToolTabConfig
// and from its Default impl.
```

State-side `state::AiToolKind` enum and the `From` impls between it and `AiToolKindWire` follow. Grep for the enum and remove call sites (the runtime no longer needs to discriminate).

#### B.2 Permission patterns

`src-tauri/src/processing/permission.rs` — remove any aider-specific regex patterns or comments. Keep the Claude Code pattern (per the saved memory: matches "Esc to cancel · Tab to amend"; recharacterize via `RUST_LOG=perm_capture=debug` if it breaks).

#### B.3 Frontend

- Delete `src/lib/AiderFirstLaunchNotice.svelte`.
- `src/App.svelte` — remove the import and mount of `AiderFirstLaunchNotice`.
- `src/lib/tabs/types.ts` and `src/lib/tabs/errorState.ts` — remove the `'aider'` literal from kind unions or any aider-specific branches.
- `src/lib/settings/types.ts` — remove `AiToolKindWire` mirror.
- `src/lib/settings/TabSettingsSection.svelte` — remove the kind dropdown (or hardcode to "Claude Code") and any aider-specific helper text.
- `src/lib/TabErrorOverlay.svelte` — replace any `kind === 'aider'` branches with the generic AI-tab path.
- `src/SettingsApp.svelte`, `src/lib/terminals.ts`, `src/lib/layout/store.ts` — strip remaining literal mentions.

Use `Grep` or the `Explore` agent to confirm no aider mentions remain. The file count from the audit was 14 backend + 10 frontend; expect most touches to be one-line removals.

#### B.4 Docs

- Delete `docs/features/FEATURE-aider-parity.md`.
- `docs/FUTURE-FEATURES.md` — move the two aider entries from "External dependencies" to "Done / historical" with a strikethrough header and the v1.3.3 cancellation note.
- `README.md` — remove aider mentions; add the local-Claude paragraph (Phase C).
- `docs/DESIGN.md` — replace the AI-tabs-are-Claude-or-aider paragraph with AI-tabs-are-Claude-with-optional-local-provider.

### Phase C — Local-provider config + second Claude tab

#### C.1 Schema

Per step 11 above. `Settings::claude_local` field, `ClaudeLocalSettings` struct, `Default` impl pointing at `http://localhost:1234` / `sk-dummy` / empty alias.

#### C.2 TS mirror

`src/lib/settings/types.ts`:

```ts
export interface ClaudeLocalSettings {
  base_url: string;
  auth_token: string;
  model_alias: string;
}

export interface Settings {
  // ... existing ...
  claude_local: ClaudeLocalSettings;
}

export interface AiToolTabConfigWire {
  // ... existing ...
  use_local_provider: boolean;
  // ai_tool_kind: removed (B.1.a)
}
```

`defaultSettings()` in `src/lib/settings/store.ts` adds `claude_local: { base_url: 'http://localhost:1234', auth_token: 'sk-dummy', model_alias: '' }` and `use_local_provider: false` on the default Claude tab.

#### C.3 Spawn-time env synthesis

Backend, wherever the AI tab's PTY is spawned. Locate via `grep -rn "tab.env\|tab\.env" src-tauri/src/pty src-tauri/src/tabs`.

```rust
// At the spawn site, after building the base env from tab.env:
let mut env: HashMap<String, String> = tab.env.clone();
if let TabConfig::AiTool(ai) = tab {
    if ai.use_local_provider {
        let cl = settings.claude_local.clone();
        // Synthesized env: tab.env overrides if a key collides (per-tab
        // is the more specific scope). Use entry().or_insert() to honor
        // that precedence.
        env.entry("ANTHROPIC_BASE_URL".into()).or_insert(cl.base_url);
        env.entry("ANTHROPIC_AUTH_TOKEN".into()).or_insert(cl.auth_token);
        if !cl.model_alias.is_empty() {
            env.entry("ANTHROPIC_MODEL".into()).or_insert(cl.model_alias);
        }
    }
}
```

Document the precedence in a comment and in DESIGN.md.

#### C.4 Settings UI — AI section

`src/SettingsApp.svelte` (or the section component if extracted): a new "AI" section with the three fields. Auth-token field uses a password input with a visibility toggle (eye icon), persisting cleartext on save (no keychain integration in this milestone).

```svelte
<section class="ai-section">
  <h3>Local LLM provider</h3>
  <p class="hint">
    Point this at any Anthropic-compatible endpoint:
    <a href="https://lmstudio.ai/docs/developer/anthropic-compat" target="_blank" rel="noopener">LM Studio</a> (≥0.4.1, port 1234),
    <a href="https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md" target="_blank" rel="noopener">llama-server</a> (port 8080, run with <code>--jinja</code> for tool use),
    Ollama, or vLLM. cctts does not start the backend — launch it separately.
    For OpenAI-only backends, run a translator like
    <a href="https://github.com/m0n0x41d/anthropic-proxy-rs" target="_blank" rel="noopener">anthropic-proxy-rs</a>
    in front of it.
  </p>
  <label>
    Endpoint URL
    <input type="text" bind:value={baseUrl} placeholder="http://localhost:1234" />
  </label>
  <label>
    Auth token
    <input type={showToken ? 'text' : 'password'} bind:value={authToken} placeholder="sk-dummy" />
    <button type="button" on:click={() => showToken = !showToken}>{showToken ? 'Hide' : 'Show'}</button>
  </label>
  <label>
    Model alias (optional)
    <input type="text" bind:value={modelAlias} placeholder="claude-haiku-4-5-20251001" />
  </label>
</section>
```

`bind:` writes flow through `applySettings` on blur or section save, matching existing Settings patterns.

#### C.5 Per-tab UI — `use_local_provider` toggle

`TabSettingsSection.svelte` for AI tabs:

```svelte
{#if isAiTool}
  <label class="checkbox-row">
    <input type="checkbox" bind:checked={tab.use_local_provider} />
    Use local LLM (synthesizes ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN
    from the global Local LLM settings)
  </label>
  {#if tab.use_local_provider}
    <p class="hint">
      Effective: <code>ANTHROPIC_BASE_URL={$settings.claude_local.base_url}</code>,
      <code>ANTHROPIC_AUTH_TOKEN=…</code>{$settings.claude_local.model_alias
        ? `, ANTHROPIC_MODEL=${$settings.claude_local.model_alias}`
        : ''}.
      Per-tab env entries below override these.
    </p>
  {/if}
{/if}
```

#### C.6 Default tabs on fresh install

`Settings::default()` returns `tabs: vec![default_claude_tab(), default_claude_local_tab(), default_shell_tab()]`. Adjusting from the current `vec![default_claude_tab(), default_aider_tab(), default_shell_tab()]` is a one-line change.

#### C.7 Restart-required indicator

Already exists per V5-02 (the `<Pill>` primitive's "restart required" use case). Hook into the per-tab settings change handler: when `use_local_provider` or `claude_local.*` changes for a running tab, set its restart-required flag. Frontend reads `tab.is_running` (or equivalent) plus the change-since-spawn comparator and renders the pill.

### Phase D — Migration v1.7 → v1.8

#### D.1 Migration step

`src-tauri/src/settings/migration.rs`:

```rust
fn looks_v1_7(value: &Value) -> bool {
    // Has terminal.scrollback (v1.7) but lacks claude_local (v1.8).
    let Some(root) = value.as_object() else { return false };
    let has_scrollback = root.get("terminal")
        .and_then(|t| t.get("scrollback"))
        .is_some();
    let has_claude_local = root.get("claude_local").is_some();
    has_scrollback && !has_claude_local
}

fn migrate_v1_7_to_v1_8(value: &mut Value) {
    let Some(root) = value.as_object_mut() else { return };

    // Stamp the new global claude_local group.
    root.insert(
        "claude_local".to_string(),
        json!({
            "base_url": "http://localhost:1234",
            "auth_token": "sk-dummy",
            "model_alias": ""
        }),
    );

    // Walk tabs: drop ai_tool_kind, rewrite the aider tab, add
    // use_local_provider on every AI tab.
    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            let Some(obj) = tab.as_object_mut() else { continue };
            let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind != "ai_tool" { continue }

            let was_aider = obj.get("ai_tool_kind")
                .and_then(Value::as_str)
                .map(|s| s == "aider")
                .unwrap_or(false);

            // Drop the discriminator (B.1.a).
            obj.remove("ai_tool_kind");

            // Default new field.
            obj.entry("use_local_provider".to_string())
                .or_insert(json!(false));

            if was_aider {
                // Rewrite in place — preserve id and per-tab env.
                obj.insert("name".into(), json!("Claude (local)"));
                obj.insert("command".into(), json!("claude"));
                obj.insert("args".into(), json!([]));
                obj.insert("use_local_provider".into(), json!(true));
                // tts_injection: keep whatever the user had; aider's
                // default may have it disabled, so explicitly enable
                // it for the rewritten claude tab.
                if let Some(tts) = obj.get_mut("tts_injection")
                    .and_then(Value::as_object_mut)
                {
                    tts.insert("enabled".into(), json!(true));
                }
                // notifications: leave as-is; aider strings are
                // user-visible but not load-bearing.
            }
        }
    }
}
```

#### D.2 Backup + cascade

Backup at `config.json.v1.7.bak.<ts>`. Cascade tests grow:

- `v1_7_to_v1_8_adds_claude_local_and_use_local_provider`
- `v1_7_to_v1_8_rewrites_aider_in_place_preserving_id_and_env`
- `v1_7_to_v1_8_drops_ai_tool_kind`
- `v1_8_file_is_not_re_detected`
- `v1_3_cascades_through_v1_4_v1_5_v1_6_v1_7_and_v1_8` — five backups.

#### D.3 Reserved-id list

`tabs/registry.rs` — replace `AIDER_TAB_ID` with `CLAUDE_LOCAL_TAB_ID` in the integrity-check reserved list. The integrity check restores `default_claude_local_tab()` if `claude-local` is missing on load.

The aider tab's id is preserved as `"aider"` after rewrite (per D.1), so the integrity check sees three tabs: `claude`, `aider` (rewritten as Claude-local), `shell-default-1`. Wait — that's a mismatch. The integrity check looks for `claude-local`, not `aider`. So:

**Decision:** the rewrite-in-place migration changes the *id* too: `aider` → `claude-local`. Layout tree references to `aider` need to be rewritten in the same migration step:

```rust
// At the top of migrate_v1_7_to_v1_8, rewrite the aider tab id in
// the layout tree:
fn rewrite_layout_tab_id(node: &mut Value, from: &str, to: &str) {
    if let Some(obj) = node.as_object_mut() {
        if let Some(arr) = obj.get_mut("tab_ids").and_then(Value::as_array_mut) {
            for entry in arr.iter_mut() {
                if entry.as_str() == Some(from) {
                    *entry = json!(to);
                }
            }
        }
        if obj.get("active_tab_id").and_then(Value::as_str) == Some(from) {
            obj.insert("active_tab_id".into(), json!(to));
        }
        for child_key in &["first", "second"] {
            if let Some(child) = obj.get_mut(*child_key) {
                rewrite_layout_tab_id(child, from, to);
            }
        }
    }
}

if let Some(layout) = root.get_mut("layout") {
    rewrite_layout_tab_id(layout, "aider", "claude-local");
}
// And in the tab itself:
// (in the loop body, when was_aider is true)
obj.insert("id".into(), json!(CLAUDE_LOCAL_TAB_ID));
```

Now the migration produces a tab list with id `claude-local`, layout tree references to `claude-local`, and the integrity check is satisfied. Adds ~20 lines to the migration but keeps semantics clean.

`session.active_tab_id` if equal to `"aider"` → rewrite to `"claude-local"`.
Layout presets (`layout_presets[].tree`) — apply the same rewrite recursively.

## Test Plan

### Phase A
- **Manual** — Right-click the Claude tab → Configure tab → Settings window opens, scrolls to the Claude tab section.
- **Manual** — Right-click a Shell tab → Configure tab → existing dialog opens (regression check).
- **Manual** — In Settings → Tabs → Claude → Appearance, set theme override to Solarized Light. Tab repaints live. Background override to a custom image — tab updates within one debounce frame.

### Phase B
- **grep** for `aider`, `Aider`, `AIDER` across `src/`, `src-tauri/src/`, and `docs/` after the cleanup pass — only acceptable matches: `completedMilestones/` (history), `CHANGELOG.md` (history), and the new v1.3.3 entry mentioning aider's removal. No matches in live code or non-historical docs.
- **Build** — `cargo build` and `npm run build` both succeed (catches stragglers the grep missed).

### Phase C
- **Unit (Rust)** — `claude_local` round-trips through serde with all field types. Default values match the spec. Spawn-env-synthesis test: feed an AI tab with `use_local_provider: true`, assert spawn env contains `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`. Per-tab `env` overrides synthesized — set `ANTHROPIC_BASE_URL` in `tab.env` and assert it wins.
- **Manual** — Run a backend locally (LM Studio is easiest: load any model and enable the server; the `/v1/messages` endpoint comes up at `http://localhost:1234`). Set `claude_local.base_url` to its URL, `auth_token` to whatever the backend expects (LM Studio accepts any non-empty string). Toggle the Claude (local) tab's `use_local_provider`. Restart the tab. Send a message — confirm the local model responds (visually distinct: different name/style than Anthropic Claude). Toggle off, restart, confirm subscription Claude responds.
- **Manual — concurrent** — Both tabs running simultaneously: send a message to Claude (subscription tab), confirm Anthropic response. Send a message to Claude (local), confirm local-model response. No cross-contamination.
- **Manual — restart-required** — Edit `claude_local.base_url` while the local tab is running. The "Restart required" pill appears on the local tab. Restart confirms the new URL is used; the pill clears.

### Phase D
- **Unit (Rust)** — see D.2.
- **Manual — fresh v1.7 file with default aider** — Hand-author a v1.7 settings.json with the standard claude/aider/shell-default tabs. Launch v1.3.3. Confirm: `config.json.v1.7.bak.<ts>` exists; the new file has `claude_local` populated with defaults; the aider tab is rewritten as `claude-local` (id, name, command, use_local_provider); the layout tree references are rewritten; the Claude tab is unchanged; the shell-default tab is unchanged.
- **Manual — v1.7 with customized aider env** — Hand-author with `tabs[1].env = { "FOO": "bar", "ANTHROPIC_BASE_URL": "http://localhost:8080" }`. Launch. Confirm: rewritten tab keeps the env (both keys preserved); per-tab `ANTHROPIC_BASE_URL` overrides the synthesized one (the user's manual setting wins).
- **Manual — v1.7 with deleted aider** — Author with only claude + shell tabs, no aider. Launch. Confirm: integrity check restores `claude-local` from `default_claude_local_tab()`; user keeps their custom layout untouched (the new tab joins the default pane).
- **Manual — cascade from v1.3** — Author a fresh v1.3 file. Launch. Confirm five backups: v1.3, v1.4, v1.5, v1.6, v1.7. Final file is v1.8 with all fields populated.
- **Manual — layout preset migration** — Save a v1.7 layout preset that places the aider tab in a specific pane. Migrate to v1.8. Confirm the preset's tree references `claude-local` instead of `aider`.

## Files Most Likely Touched

**Phase A**
- `src-tauri/src/ipc/commands.rs` — `open_settings_window_to_tab`
- `src/lib/ipc.ts` — TS binding
- `src/SettingsApp.svelte` — deep-link listener, scroll target ids
- `src/lib/TabBar.svelte` — kind-aware Configure dispatch

**Phase B**
- `src-tauri/src/settings/schema.rs` — drop `AiToolKindWire`, `AIDER_TAB_ID`, `default_aider_tab()`, `ai_tool_kind` field
- `src-tauri/src/state/manager.rs` — drop `AiToolKind` enum + From impls
- `src-tauri/src/processing/permission.rs` — drop aider patterns
- `src-tauri/src/{tabs/registry,tabs/config,pty/manager,pty/tasks,pty/scrollback,notifications/manager,settings/persistence,settings/migration,main,ipc/tab_lifecycle}.rs` — remove aider literals (audit list, expect ≤2 lines per file)
- `src/lib/AiderFirstLaunchNotice.svelte` — delete
- `src/App.svelte` — drop import + mount
- `src/lib/{terminals,layout/store,tabs/types,tabs/errorState,TabErrorOverlay,settings/types,settings/TabSettingsSection}.{ts,svelte}` — remove aider literals
- `src/SettingsApp.svelte` — remove kind dropdown / aider-specific UI
- `docs/features/FEATURE-aider-parity.md` — delete
- `docs/FUTURE-FEATURES.md` — move aider entries to historical
- `README.md`, `docs/DESIGN.md`, `docs/CLAUDE.md` (if relevant) — narrative updates

**Phase C**
- `src-tauri/src/settings/schema.rs` — `ClaudeLocalSettings`, `claude_local` field, `use_local_provider` on AI tabs, `default_claude_local_tab()`, `CLAUDE_LOCAL_TAB_ID`
- `src-tauri/src/pty/manager.rs` (or wherever AI tabs spawn) — env synthesis
- `src/lib/settings/types.ts`, `src/lib/settings/store.ts` — TS mirror, defaults
- `src/SettingsApp.svelte` — new AI section
- `src/lib/settings/TabSettingsSection.svelte` — `use_local_provider` toggle, effective-env helper text
- `README.md`, `docs/DESIGN.md` — local-provider paragraph

**Phase D**
- `src-tauri/src/settings/migration.rs` — `migrate_v1_7_to_v1_8`, `looks_v1_7`, layout-id-rewrite helper, tests
- `src-tauri/src/tabs/registry.rs` — reserved-id list update

**Cross-cutting**
- `CHANGELOG.md` — v1.3.3 entry

## Risks and Open Questions

### Phase A
- **Settings deep-link timing race.** If the IPC fires before the Settings window has mounted its listener, the event is lost. Mitigation: the IPC waits for the window to be ready (Tauri's `window.once('tauri://created')` or similar) before emitting. Verify at impl time; if races surface, add a short retry loop on the Settings side that reads the deep-link state from a one-shot setting key.

### Phase B
- **Hidden aider call sites.** The 14+10 file count is from a `Grep`; some matches may be in comments (low risk) or in dead code that compiles fine but gets exercised under unusual paths (e.g., a notification template that mentions aider by name). The build catches missing-symbol issues; runtime behavior with renamed-but-not-deleted strings shouldn't break, just look weird. Mitigation: a manual UI walkthrough after the cleanup ensures no "Aider…" strings remain in the live UI.
- **Migration cascade reaching v1.8 from very old files.** A user on v1.0 (unlikely but possible — five major migration generations old) would pass through v1 → v1.1 → v1.2 → v1.3 → v1.4 → v1.5 → v1.6 → v1.7 → v1.8 in a single launch with eight backups. Each individual migration is well-tested; the chain isn't. Add one cascade test from v1.0 to v1.8 to lock down.

### Phase C
- **`auth_token` stored cleartext.** Local-backend tokens are typically dummies, so this is fine. If a user mistakenly puts a real Anthropic API key (intending to bypass subscription auth that way), it's at-rest in `settings.json`. Documented; OS keychain is a separate feature. Helper text in the Settings field can warn: "Use a real key only if you understand the security implications — local backend tokens are dummy strings."
- **`ANTHROPIC_MODEL` env support is uncertain.** Claude Code may use the `--model` flag exclusively. Setting `ANTHROPIC_MODEL` may be a no-op. Mitigation: confirm at impl time by running `claude` with the env set and observing whether the model picks up. If it doesn't, drop the model_alias field's env injection and either (a) inject `--model alias` into `args` instead, or (b) leave model selection entirely to the backend's config. Default to (b) — simplest, and matches how the supported backends (LM Studio, Ollama, vLLM, llama-server) expose loaded models.
- **Per-tab env precedence ambiguity.** `entry().or_insert()` makes per-tab env win. If a user sets `ANTHROPIC_BASE_URL` per-tab and then expects the global to update, they'll be surprised. Documented as "per-tab env always wins."
- **No backend-up indicator.** If the user toggles `use_local_provider` but their backend isn't running, Claude Code launches and fails on first message with a connection error. cctts shows the error in the avatar/status bar via the existing error path, but doesn't proactively check the backend. Out of scope; if real-use friction surfaces, add a "ping the backend on tab spawn" warning.
- **Anthropic subscription cookie / session leak.** When `use_local_provider` is on, env vars override the subscription auth, but the subscription credentials may still be cached on disk in `~/.claude/`. They're not transmitted to the local backend (the env vars override at request build time), so this is fine in practice. Documented in CHANGELOG security notes.

### Phase D
- **The aider id rewrite is irreversible without the backup.** A user who lost their `config.json.v1.7.bak.<ts>` and wanted aider back could rebuild manually but their layout-tree positioning of "aider" is now `claude-local`. Backup path documented; this is the standard cctts migration model, no novel risk.
- **TTS injection on the rewritten tab.** Aider's default `tts_injection.enabled` is `false` (TTS markup not supported per `FEATURE-aider-parity.md`). Migration explicitly sets it to `true` because the rewritten tab IS Claude. If a user had manually disabled TTS on the aider tab they should still see TTS work after the rewrite — that's intentional and documented in the CHANGELOG migration notes ("aider tab is rewritten as Claude with TTS injection enabled by default").
- **Layout preset rewrite is best-effort.** If a preset references an old aider id and the user had an unusually-named alternate aider tab id (hand-edited settings), the rewrite misses it. Edge case; documented as "tabs with id `aider` are rewritten."

## Followups Tracked Elsewhere

- **OS keychain integration for `auth_token`.** Add to `FUTURE-FEATURES.md` if real-use friction surfaces (e.g., users storing real Anthropic API keys in settings.json instead of pointing at a local backend).
- **Auto-spawn the local backend as a sidecar.** Candidate for `FUTURE-FEATURES.md` if "did I start the backend" friction is real. Could ship as an opt-in `claude_local.autospawn: { command, args }` group, generic over the backend (LM Studio CLI, `llama-server`, `ollama serve`, etc.).
- **Multiple local-provider configs.** Defer; one is enough for the headline use case. If a user has two local proxies (e.g., a fast one for simple tasks and a slow one for code), they configure per-tab `env` directly.
- **Provider-detection UI hint.** Color-code the AI tab differently when `use_local_provider` is on (e.g., a small "🏠" indicator in the tab title or a different accent on the tab pill). Polish; defer.
