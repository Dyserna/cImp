<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import { open } from '@tauri-apps/plugin-dialog';
  import {
    initSettings,
    settings,
    applySettings,
  } from './lib/settings/store';
  import {
    aiToolTabDefaults,
    auditDetectTool,
    consumeSettingsDeepLink,
    harnessVersionsGet,
    listVoices,
    llmPricingGet,
    llmPricingSet,
    requestTabRestart,
  } from './lib/settings/ipc';
  import { listen } from '@tauri-apps/api/event';
  import type {
    AiToolTabConfig,
    AuditDetectResult,
    AuditToolConfig,
    AuditToolId,
    HarnessVersions,
    ProcessingDevice,
    Settings,
    ShellTabConfig,
    TabConfig,
  } from './lib/settings/types';
  import {
    asThemedTabConfig,
    defaultSettings,
    findTab,
    findTabIndex,
    harnessStatusBlocks,
    toPresetConfig,
  } from './lib/settings/types';
  import { contentClear, contentOpenFolder, setEnabledAiTabs } from './lib/ipc';
  import { listSttModels, listInputDevices } from './lib/stt';
  import {
    offloadTest,
    offloadDeriveOpencodeProvider,
    offloadStatuses,
    offloadBackendStart,
    offloadBackendStop,
    offloadBackendRestart,
    describeBackendStatus,
    offloadServiceStatus,
    offloadReloadMcp,
    offloadEnableReadonlyCommands,
    describeMcpServerHealth,
    type BackendStatus,
    type ServiceStatus,
  } from './lib/offload';
  import { graphIgnorePick, graphRebuild, graphStatus, type GraphStatus } from './lib/graph';
  import ArrayEditor from './lib/settings/ArrayEditor.svelte';
  import type {
    OffloadBackend,
    ToolScope,
    BackendTier,
    CommandPolicy,
    McpServerConfig,
    ServerCommandTemplate,
    RemoteBackendTemplate,
    PromptTemplate,
    LlmPricingModel,
  } from './lib/settings/types';
  import {
    composeTemplatesGlobalGet,
    composeTemplatesGlobalSet,
    composeTemplatesProjectGet,
  } from './lib/compose/templates';
  import { LOCAL_DATA_TOOLS } from './lib/settings/types';
  import type { AiTabId } from './lib/tabs/types';
  import { AI_TABS } from './lib/tabs/types';
  import { version as appVersion } from '../package.json';
  import ShortcutCapture from './lib/settings/ShortcutCapture.svelte';
  import TabSettingsSection from './lib/settings/TabSettingsSection.svelte';
  import ThemeSwatch from './lib/settings/ThemeSwatch.svelte';
  import TuiTitleBar from './lib/TuiTitleBar.svelte';
  import CustomThemeEditor from './lib/settings/CustomThemeEditor.svelte';
  import BackgroundConfigEditor from './lib/settings/BackgroundConfigEditor.svelte';
  import ChecksEditor from './lib/settings/ChecksEditor.svelte';
  import {
    auditToolGroups,
    formatDetect,
    toolNotApplicable,
    type AuditToolRow,
  } from './lib/settings/codeAudit';
  import { auditRefreshCensus } from './lib/codeAudit/ipc';
  import {
    AUDIT_TOOL_CATEGORY,
    autoSelectQuality,
    censusIsEmpty,
  } from './lib/codeAudit/logic';
  import type { AuditCensus } from './lib/codeAudit/types';
  import { resolveBundledTheme, defaultPalette } from './lib/themes';
  import { themeRegistry, paletteRegistry } from './lib/themes/registry';
  import type { ThemeColorsWire } from './lib/settings/types';

  // Whether the active theme uses the OS-native chrome — drives the custom
  // settings-window title bar. Derived from the registry so it follows the
  // theme's `decorations` metadata (and updates once the registry loads).
  let useCustomTitleBar = $derived(
    !($themeRegistry.find((t) => t.id === $settings.ui.theme)?.decorations ?? false),
  );

  /// Terminal palette paired with a UI chrome theme: each theme's metadata
  /// carries its default palette. Selecting a UI theme re-points the terminal
  /// palette to its pairing (a manual palette pick afterward sticks until the
  /// next theme switch). An unknown theme leaves the palette untouched.
  function pairedPalette(themeId: string): string | undefined {
    return $themeRegistry.find((t) => t.id === themeId)?.palette;
  }

  let voices = $state<string[]>([]);
  // V6-01: available STT models (ggml-*.bin under models/) and cpal input
  // device names, populated on mount for the STT section dropdowns.
  let sttModels = $state<string[]>([]);
  let inputDevices = $state<string[]>([]);
  // Common Whisper language hints offered in the dropdown ("auto" detects).
  const STT_LANGUAGES: { code: string; label: string }[] = [
    { code: 'auto', label: 'Auto-detect' },
    { code: 'en', label: 'English' },
    { code: 'es', label: 'Spanish' },
    { code: 'fr', label: 'French' },
    { code: 'de', label: 'German' },
    { code: 'it', label: 'Italian' },
    { code: 'pt', label: 'Portuguese' },
    { code: 'nl', label: 'Dutch' },
    { code: 'ru', label: 'Russian' },
    { code: 'zh', label: 'Chinese' },
    { code: 'ja', label: 'Japanese' },
    { code: 'ko', label: 'Korean' },
    { code: 'ar', label: 'Arabic' },
    { code: 'he', label: 'Hebrew' },
    { code: 'hi', label: 'Hindi' },
  ];
  // V1.4-07: Local LLM provider section. The auth-token input toggles
  // between password and text via this flag (no keychain integration in
  // this milestone — the token sits cleartext in settings.json).
  let showLocalToken = $state<boolean>(false);
  // Inline error under the AI-tabs checkbox group — e.g. when enabling an
  // OpenCode tab is rejected because `opencode` isn't installed (ebin/PATH).
  let aiTabsError = $state<string | null>(null);
  // V8-01 offload: test-box input/result and a busy guard for the
  // Start/Stop/Reset/Test buttons.
  let offloadTestInput = $state<string>('');
  let offloadTestResult = $state<string>('');
  let offloadBusy = $state<boolean>(false);
  async function runOffloadAction(action: () => Promise<void>): Promise<void> {
    offloadBusy = true;
    try {
      await action();
    } catch (e) {
      offloadTestResult = `Error: ${e}`;
    } finally {
      offloadBusy = false;
    }
  }
  // V9-01 Code graph: rebuild trigger + status readout for the Settings panel.
  let graphBusy = $state<boolean>(false);
  let graphStatuses = $state<GraphStatus[]>([]);
  async function refreshGraphStatus(): Promise<void> {
    try {
      graphStatuses = await graphStatus();
    } catch {
      graphStatuses = [];
    }
  }
  // Ignore-list editing: keystrokes only mutate the local snapshot (via
  // ArrayEditor's bind), and the backend save happens on commit boundaries
  // (blur/Enter/row-remove) — the same edit-vs-commit split the MCP servers
  // editor uses, so per-keystroke saves can't fire an index resync per char.
  function commitGraphIgnore(): void {
    if (!snapshot) return;
    void applySettings($state.snapshot(snapshot));
  }
  // "Add file…" / "Add folder…": native picker → project-relative glob,
  // appended and committed in one step. Cancel (null) changes nothing.
  async function addGraphIgnorePick(folder: boolean): Promise<void> {
    try {
      const glob = await graphIgnorePick(folder);
      if (!glob) return;
      patch((s) => {
        if (!s.graph.ignore.includes(glob)) s.graph.ignore.push(glob);
      });
    } catch (e) {
      console.error('graph_ignore_pick failed', e);
    }
  }
  async function runGraphRebuild(): Promise<void> {
    graphBusy = true;
    try {
      await graphRebuild();
      // Give the worker thread a moment to flip to "building", then poll a few
      // times so the user sees counts land without a manual refresh.
      for (let i = 0; i < 6; i++) {
        await new Promise((r) => setTimeout(r, 500));
        await refreshGraphStatus();
        if (graphStatuses.every((s) => !s.building)) break;
      }
    } catch (e) {
      console.error('graph rebuild failed', e);
    } finally {
      graphBusy = false;
    }
  }

  // V14 Phase A: prompt-library "Compose" section. Global templates are
  // edited here and saved through the dedicated global-only IPC (NOT
  // `patch`/`applySettings` — see `compose_templates_global_set`'s doc
  // comment); project templates are a read-only listing (they live in this
  // project's `.cimp/config.json`, edited by hand).
  let globalTemplates = $state<PromptTemplate[]>([]);
  let projectTemplates = $state<PromptTemplate[]>([]);
  let composeTemplatesLoading = $state(true);
  let composeTemplatesDirty = $state(false);
  let composeTemplatesError = $state<string | null>(null);

  async function loadComposeTemplates(): Promise<void> {
    composeTemplatesLoading = true;
    composeTemplatesError = null;
    try {
      const [g, p] = await Promise.all([
        composeTemplatesGlobalGet(),
        composeTemplatesProjectGet(),
      ]);
      globalTemplates = g;
      projectTemplates = p;
      composeTemplatesDirty = false;
    } catch (e) {
      composeTemplatesError = `${e}`;
    } finally {
      composeTemplatesLoading = false;
    }
  }

  function addGlobalTemplate(): void {
    globalTemplates = [...globalTemplates, { name: `template-${globalTemplates.length + 1}`, body: '' }];
    composeTemplatesDirty = true;
  }
  function renameGlobalTemplate(i: number, name: string): void {
    globalTemplates = globalTemplates.map((t, idx) => (idx === i ? { ...t, name } : t));
    composeTemplatesDirty = true;
  }
  function editGlobalTemplateBody(i: number, body: string): void {
    globalTemplates = globalTemplates.map((t, idx) => (idx === i ? { ...t, body } : t));
    composeTemplatesDirty = true;
  }
  function deleteGlobalTemplate(i: number): void {
    globalTemplates = globalTemplates.filter((_, idx) => idx !== i);
    composeTemplatesDirty = true;
  }
  async function saveGlobalTemplates(): Promise<void> {
    composeTemplatesError = null;
    try {
      await composeTemplatesGlobalSet(globalTemplates);
      composeTemplatesDirty = false;
    } catch (e) {
      composeTemplatesError = `${e}`;
    }
  }

  // LLM pricing section: the provider/model $/MTok table behind the Code
  // Intelligence tab's session-cost popup. Global-only like the compose
  // templates — edited here, saved through the dedicated `llm_pricing_set`
  // IPC (straight to the physical global settings.json, NOT `patch`/
  // `applySettings` — an array field would otherwise land in the project
  // overlay; see `llm_pricing_set`'s doc comment).
  let llmPricing = $state<LlmPricingModel[]>([]);
  let llmPricingLoading = $state(true);
  let llmPricingDirty = $state(false);
  let llmPricingError = $state<string | null>(null);

  async function loadLlmPricing(): Promise<void> {
    llmPricingLoading = true;
    llmPricingError = null;
    try {
      llmPricing = await llmPricingGet();
      llmPricingDirty = false;
    } catch (e) {
      llmPricingError = `${e}`;
    } finally {
      llmPricingLoading = false;
    }
  }
  function addLlmPricingRow(): void {
    llmPricing = [
      ...llmPricing,
      { provider: 'Custom', model: `model-${llmPricing.length + 1}`, model_prefix: '', input: 0, cache_write: 0, cache_read: 0, output: 0 },
    ];
    llmPricingDirty = true;
  }
  function editLlmPricingText(i: number, field: 'provider' | 'model' | 'model_prefix', value: string): void {
    llmPricing = llmPricing.map((r, idx) => (idx === i ? { ...r, [field]: value } : r));
    llmPricingDirty = true;
  }
  function editLlmPricingRate(
    i: number,
    field: 'input' | 'cache_write' | 'cache_read' | 'output',
    value: string,
  ): void {
    // Clamp garbage/negatives to 0 so a saved row can never poison the cost
    // popup's math with NaN or a negative price.
    const n = Math.max(0, Number(value) || 0);
    llmPricing = llmPricing.map((r, idx) => (idx === i ? { ...r, [field]: n } : r));
    llmPricingDirty = true;
  }
  function deleteLlmPricingRow(i: number): void {
    llmPricing = llmPricing.filter((_, idx) => idx !== i);
    llmPricingDirty = true;
  }
  async function saveLlmPricing(): Promise<void> {
    llmPricingError = null;
    try {
      await llmPricingSet($state.snapshot(llmPricing));
      llmPricingDirty = false;
    } catch (e) {
      llmPricingError = `${e}`;
    }
  }

  async function runOffloadTest(): Promise<void> {
    offloadBusy = true;
    offloadTestResult = 'Running…';
    try {
      offloadTestResult = await offloadTest(offloadTestInput);
    } catch (e) {
      offloadTestResult = `Error: ${e}`;
    } finally {
      offloadBusy = false;
    }
  }

  // V21: register the given Local backend as the OpenCode `local-llama`
  // provider. Derives base URL + model from its server command in Rust (which
  // errors, naming the missing --port/model flag, when the command is
  // incomplete), then persists the snapshot and selects it as the default
  // model so the OpenCode tab is ready to use. Overrides any existing
  // registration. `opencodeProviderMsg` reports success/failure inline.
  let opencodeProviderMsg = $state<{ i: number; text: string; ok: boolean } | null>(null);
  async function registerOpencodeProvider(i: number): Promise<void> {
    const backend = snapshot?.offload.backends[i];
    if (!backend || backend.kind.type !== 'local') return;
    opencodeProviderMsg = null;
    try {
      const provider = await offloadDeriveOpencodeProvider(backend.kind.server_command);
      patch((s) => {
        s.offload.opencode_provider = provider;
      });
      opencodeProviderMsg = {
        i,
        ok: true,
        text: `Registered local-llama → ${provider.model} at ${provider.base_url}. OpenCode tabs will use it by default.`,
      };
    } catch (e) {
      opencodeProviderMsg = { i, ok: false, text: `${e}` };
    }
  }

  // V8-02 backend pool: live per-backend status rows + a refresh loop while
  // the Offload section is open.
  let backendStatuses = $state<BackendStatus[]>([]);
  // V16 Feature 6: the Code Intelligence section's `context_llm_digests`
  // toggle is health-aware — enabled only when a LOCAL backend is ready
  // (the digest path is local-only by design). Polled by the same
  // `startBackendStatusPolling` loop the Offload section uses. Turning the
  // feature OFF is always allowed; only turning it ON is gated.
  const localOffloadReady = $derived(
    backendStatuses.some((b) => b.kind === 'local' && b.state === 'ready'),
  );
  // V8-03 warm pool: honest global in-flight + per-MCP-server health.
  let serviceStatus = $state<ServiceStatus | null>(null);
  let backendStatusTimer: ReturnType<typeof setInterval> | null = null;
  async function refreshBackendStatuses(): Promise<void> {
    try {
      backendStatuses = await offloadStatuses();
    } catch (e) {
      console.warn('offload_statuses failed', e);
    }
    serviceStatus = await offloadServiceStatus();
  }
  function startBackendStatusPolling(): void {
    if (backendStatusTimer) return;
    void refreshBackendStatuses();
    backendStatusTimer = setInterval(refreshBackendStatuses, 4000);
  }

  function statusFor(name: string): BackendStatus | undefined {
    return backendStatuses.find((s) => s.name === name);
  }

  // Backend-pool mutations (all go through `patch` so they persist + mark dirty).
  function uniqueBackendName(base: string): string {
    const names = new Set((snapshot?.offload.backends ?? []).map((b) => b.name));
    if (!names.has(base)) return base;
    let i = 2;
    while (names.has(`${base}-${i}`)) i++;
    return `${base}-${i}`;
  }
  function addLocalBackend(): void {
    patch((s) => {
      s.offload.backends = [
        ...s.offload.backends,
        {
          name: uniqueBackendName('local'),
          enabled: true,
          kind: { type: 'local', server_command: '', autostart: false, show_command_on_start: false },
          declared_context: null,
          declared_model: '',
          tier: 'quality',
          tool_scope: { mode: 'all' },
        },
      ];
    });
  }
  function addRemoteBackend(): void {
    patch((s) => {
      s.offload.backends = [
        ...s.offload.backends,
        {
          name: uniqueBackendName('remote'),
          enabled: true,
          kind: { type: 'remote', base_url: '', auth_token: '', is_cloud: false, cloud_consent: false },
          declared_context: null,
          declared_model: '',
          tier: 'fast',
          tool_scope: { mode: 'all' },
        },
      ];
    });
  }
  // Adopt the legacy single `server_command` into the pool as one Local backend.
  function adoptLegacyServer(): void {
    patch((s) => {
      s.offload.backends = [
        {
          name: 'local',
          enabled: true,
          kind: {
            type: 'local',
            server_command: s.offload.server_command,
            autostart: s.offload.autostart,
            show_command_on_start: false,
          },
          declared_context: null,
          declared_model: '',
          tier: 'quality',
          tool_scope: { mode: 'all' },
        },
      ];
    });
  }
  function removeBackend(i: number): void {
    patch((s) => {
      s.offload.backends = s.offload.backends.filter((_, idx) => idx !== i);
    });
  }
  function updateBackend(i: number, fn: (b: OffloadBackend) => void): void {
    patch((s) => {
      fn(s.offload.backends[i]);
    });
  }
  // ── Backend templates (global libraries) ───────────────────────────────
  // Save/Load/Delete controls under a backend field manage a global template
  // library shared across backends and restarts: Local backends use
  // `offload.server_command_templates` (name + command); Remote backends use
  // `offload.remote_backend_templates` (name + base URL + auth token). Only one
  // popup is open at a time; `templatePopup` records which backend (by index)
  // opened it and which mode it's in — the backend's own kind decides which
  // library the popup acts on.
  let templatePopup = $state<{ i: number; mode: 'save' | 'load' | 'delete' } | null>(null);
  let newTemplateName = $state('');
  let templateError = $state<string | null>(null);

  function openTemplatePopup(i: number, mode: 'save' | 'load' | 'delete'): void {
    // A second click on the same button closes the popup (toggle).
    if (templatePopup && templatePopup.i === i && templatePopup.mode === mode) {
      closeTemplatePopup();
      return;
    }
    templatePopup = { i, mode };
    newTemplateName = '';
    templateError = null;
  }
  function closeTemplatePopup(): void {
    templatePopup = null;
    newTemplateName = '';
    templateError = null;
  }
  // Validate the pending template name against an existing library; returns the
  // trimmed name or null (and sets `templateError`) when invalid.
  function validateTemplateName(existing: string[]): string | null {
    const name = newTemplateName.trim();
    if (!name) {
      templateError = 'Name required.';
      return null;
    }
    if (existing.includes(name)) {
      templateError = `A template named "${name}" already exists.`;
      return null;
    }
    return name;
  }
  // Local backend (server command) ───────────────────────────────
  function commitSaveLocalTemplate(i: number): void {
    if (!snapshot) return;
    const name = validateTemplateName(
      snapshot.offload.server_command_templates.map((t) => t.name),
    );
    if (!name) return;
    const backend = snapshot.offload.backends[i];
    const command = backend?.kind.type === 'local' ? backend.kind.server_command : '';
    patch((s) => {
      s.offload.server_command_templates = [
        ...s.offload.server_command_templates,
        { name, command },
      ];
    });
    closeTemplatePopup();
  }
  function loadLocalTemplate(i: number, tpl: ServerCommandTemplate): void {
    updateBackend(i, (b) => {
      if (b.kind.type === 'local') b.kind.server_command = tpl.command;
    });
    closeTemplatePopup();
  }
  function deleteLocalTemplate(name: string): void {
    patch((s) => {
      s.offload.server_command_templates =
        s.offload.server_command_templates.filter((t) => t.name !== name);
    });
  }
  // Remote backend (base URL + auth token) ───────────────────────
  function commitSaveRemoteTemplate(i: number): void {
    if (!snapshot) return;
    const name = validateTemplateName(
      snapshot.offload.remote_backend_templates.map((t) => t.name),
    );
    if (!name) return;
    const backend = snapshot.offload.backends[i];
    const base_url = backend?.kind.type === 'remote' ? backend.kind.base_url : '';
    const auth_token = backend?.kind.type === 'remote' ? backend.kind.auth_token : '';
    patch((s) => {
      s.offload.remote_backend_templates = [
        ...s.offload.remote_backend_templates,
        { name, base_url, auth_token },
      ];
    });
    closeTemplatePopup();
  }
  function loadRemoteTemplate(i: number, tpl: RemoteBackendTemplate): void {
    updateBackend(i, (b) => {
      if (b.kind.type === 'remote') {
        b.kind.base_url = tpl.base_url;
        b.kind.auth_token = tpl.auth_token;
      }
    });
    closeTemplatePopup();
  }
  function deleteRemoteTemplate(name: string): void {
    patch((s) => {
      s.offload.remote_backend_templates =
        s.offload.remote_backend_templates.filter((t) => t.name !== name);
    });
  }
  // ── Command security policies (Tools tab) ──────────────────────────────
  // All mutations route through `patch` so they persist + mark dirty, mirroring
  // the backend-pool helpers above.
  function addCommandPolicy(): void {
    patch((s) => {
      s.offload.command_policies = [
        ...s.offload.command_policies,
        { program: '', denied_flags: [], denied_subcommands: [], allowed_subcommands: [], env: [] },
      ];
    });
  }
  function removeCommandPolicy(i: number): void {
    patch((s) => {
      s.offload.command_policies = s.offload.command_policies.filter((_, idx) => idx !== i);
    });
  }
  function updatePolicy(i: number, fn: (p: CommandPolicy) => void): void {
    patch((s) => {
      fn(s.offload.command_policies[i]);
    });
  }
  // V21 F7: one-click "safe read-only commands" preset. The backend merges
  // `git` + `cargo` (metadata/tree, with its pinning policy) into the live
  // allowlist/policies atomically and returns the updated settings, which we
  // fold into the local snapshot. Idempotent + non-destructive — a merge, not a
  // mode: the user sees exactly what got added in the allowlist / policy
  // editors below and can prune any of it.
  async function enableReadonlyCommands(): Promise<void> {
    const updated = await offloadEnableReadonlyCommands();
    if (updated) snapshot = updated;
  }
  // ── MCP tool servers (MCP servers section) ─────────────────────────────
  // Add/remove/toggle with live host reload — no cImp restart. Edits persist
  // through the same awaited `applySettings` the rest of the panel uses; once
  // the backend has the new value we call `offload_reload_mcp`, which
  // reconciles the warm MCP host and returns fresh health for the status list.
  function uniqueMcpName(base: string): string {
    const names = new Set((snapshot?.offload.mcp_servers ?? []).map((m) => m.name));
    if (!names.has(base)) return base;
    let i = 2;
    while (names.has(`${base}-${i}`)) i++;
    return `${base}-${i}`;
  }
  // Persist `next` to the backend and wait for it (so the live reload below
  // reconciles against the new config, not the stale one). Mirrors `patch` but
  // awaitable.
  async function applyMcp(updater: (s: Settings) => void): Promise<void> {
    if (!snapshot) return;
    const next = structuredClone($state.snapshot(snapshot));
    updater(next);
    snapshot = next;
    await applySettings(next);
  }
  // Reconcile the warm host now and fold the fresh status into the read-only
  // list above the editor.
  async function reloadMcpHost(): Promise<void> {
    const status = await offloadReloadMcp();
    if (status) serviceStatus = status;
  }
  // A text-field edit (name/url): update the local snapshot ONLY — no backend
  // write per keystroke. The full snapshot is persisted on blur/Enter
  // (`commitMcpEdits`), which also reloads the host. Persisting per keystroke
  // (the old `patch` path) raced: fire-and-forget `applySettings` calls could
  // complete out of order and leave the backend holding a half-typed URL, which
  // the 12s health watch would then flag as down.
  function setMcpServer(i: number, fn: (m: McpServerConfig) => void): void {
    if (!snapshot) return;
    const next = structuredClone($state.snapshot(snapshot));
    fn(next.offload.mcp_servers[i]);
    snapshot = next;
  }
  // Blur handler for the name/url inputs: ensure the latest snapshot is
  // persisted, then reload the host.
  async function commitMcpEdits(): Promise<void> {
    if (!snapshot) return;
    await applySettings($state.snapshot(snapshot));
    await reloadMcpHost();
  }
  function addMcpServer(): void {
    // New rows default to an HTTP endpoint (empty url → shows "down" until
    // filled); no reload yet since there's nothing to connect to.
    patch((s) => {
      s.offload.mcp_servers = [
        ...s.offload.mcp_servers,
        {
          name: uniqueMcpName('server'),
          command: '',
          args: [],
          env: {},
          url: '',
          claude_access: false,
          offload_access: true,
          opencode_access: false,
        },
      ];
    });
  }
  async function removeMcpServer(i: number): Promise<void> {
    await applyMcp((s) => {
      s.offload.mcp_servers = s.offload.mcp_servers.filter((_, idx) => idx !== i);
    });
    await reloadMcpHost();
  }
  async function setMcpAccess(
    i: number,
    field: 'claude_access' | 'offload_access' | 'opencode_access',
    value: boolean,
  ): Promise<void> {
    await applyMcp((s) => {
      s.offload.mcp_servers[i][field] = value;
    });
    await reloadMcpHost();
  }
  // Comma-separated <-> string[] for the flag/subcommand inputs (mirrors the
  // allowlist input). Empty entries are dropped.
  function csvToList(value: string): string[] {
    return value
      .split(',')
      .map((c) => c.trim())
      .filter((c) => c.length > 0);
  }
  // Whether an allowlisted program currently has a hardening policy — drives
  // the transparency line next to the allowlist.
  function policyForProgram(program: string): CommandPolicy | undefined {
    const stem = program.split(/[\\/]/).pop()?.replace(/\.[^.]+$/, '').toLowerCase() ?? program;
    return snapshot?.offload.command_policies.find(
      (p) => p.program.toLowerCase() === stem,
    );
  }
  // Toggling a backend's cloud flag flips its default tool scope to the safe
  // web/docs-only set (deny the local-data tools) so a cloud backend never
  // ships local file/exec tools unless the user explicitly widens it.
  function setBackendCloud(i: number, isCloud: boolean): void {
    updateBackend(i, (b) => {
      if (b.kind.type !== 'remote') return;
      b.kind.is_cloud = isCloud;
      if (isCloud) {
        b.tool_scope = { mode: 'allexcept', tools: [...LOCAL_DATA_TOOLS] };
      } else {
        b.kind.cloud_consent = false;
        b.tool_scope = { mode: 'all' };
      }
    });
  }
  // Tool-scope picker: 'all' | 'web' (web/docs only) | custom (allexcept local-data).
  function scopeMode(scope: ToolScope): 'all' | 'web' | 'custom' {
    if (scope.mode === 'all') return 'all';
    if (
      scope.mode === 'allexcept' &&
      LOCAL_DATA_TOOLS.every((t) => scope.tools.includes(t)) &&
      scope.tools.length === LOCAL_DATA_TOOLS.length
    ) {
      return 'web';
    }
    return 'custom';
  }
  function setScopeMode(i: number, mode: 'all' | 'web'): void {
    updateBackend(i, (b) => {
      b.tool_scope = mode === 'all' ? { mode: 'all' } : { mode: 'allexcept', tools: [...LOCAL_DATA_TOOLS] };
    });
  }
  // Per-tab "applied" baselines — used to compute the Restart Required
  // indicator when subprocess-affecting fields drift from the spawn-time
  // settings. Notification text and first-launch dismissal are NOT in
  // the diff because they apply live without restart. Keyed by tab id
  // so additional AI tabs in future versions plug in without a refactor.
  let tabBaselines = $state<Record<string, AiToolTabConfig | null>>({});
  // Per-tab default settings, fetched from the backend so "Reset to default"
  // buttons match the Rust-side defaults exactly.
  let tabDefaults = $state<Record<string, AiToolTabConfig | null>>({});
  let snapshot = $state<Settings | null>(null);

  // V16: `harness_versions` is out-of-band — written straight to the
  // physical global file by the transcript tap / hand edits — so the
  // settings snapshot only reflects app startup. Fetched fresh once per
  // Settings-window open; the E1 hard block below prefers it so a
  // just-recorded outcome disables the toggle without an app restart.
  let harnessFresh = $state<HarnessVersions | null>(null);
  const e1Blocked = $derived(
    harnessStatusBlocks((harnessFresh ?? snapshot?.harness_versions)?.e1_status ?? ''),
  );

  // Sidebar nav: which group is visible. The template gates each <section>
  // on this so only one group renders at a time. Default lands on 'theme'
  // (Appearance sits at the top of the nav order).
  type SectionId =
    | 'audio'
    | 'stt'
    | 'avatar'
    | 'theme'
    | 'bottom-bar'
    | 'tabs'
    | 'shortcuts'
    | 'compose'
    | 'offload'
    | 'mcp'
    | 'graph'
    | 'checks'
    | 'code-audit'
    | 'pricing'
    | 'workbench'
    | 'advanced'
    | 'about';
  let activeSection = $state<SectionId>('theme');
  const SECTIONS: { id: SectionId; label: string }[] = [
    { id: 'theme', label: 'Appearance' },
    { id: 'avatar', label: 'Avatar' },
    { id: 'shortcuts', label: 'Keyboard controls' },
    { id: 'compose', label: 'Compose' },
    { id: 'bottom-bar', label: 'Bottom bar' },
    { id: 'audio', label: 'Text-to-speech' },
    { id: 'stt', label: 'Speech-to-text' },
    { id: 'tabs', label: 'Tabs' },
    { id: 'offload', label: 'Offload task tools' },
    { id: 'mcp', label: 'MCP servers' },
    { id: 'graph', label: 'Code Intelligence' },
    { id: 'checks', label: 'Checks' },
    { id: 'code-audit', label: 'Code Audit' },
    { id: 'pricing', label: 'LLM pricing' },
    { id: 'workbench', label: 'Workbench' },
    { id: 'advanced', label: 'Advanced' },
    { id: 'about', label: 'About' },
  ];
  const REPO_URL = 'https://github.com/Dyserna/cImp';

  // Shortcut rows rendered as loops — the numbered tab slots and the pane
  // actions are 16 near-identical <label> rows otherwise. Every key is a
  // `string | null` field of the shortcuts slice.
  type ShortcutKey = keyof Settings['shortcuts'];
  const TAB_SHORTCUT_ROWS: readonly (readonly [ShortcutKey, string])[] = [
    ['switch_to_tab_3', 'Switch to tab 3'],
    ['switch_to_tab_4', 'Switch to tab 4'],
    ['switch_to_tab_5', 'Switch to tab 5'],
    ['switch_to_tab_6', 'Switch to tab 6'],
    ['switch_to_tab_7', 'Switch to tab 7'],
    ['switch_to_tab_8', 'Switch to tab 8'],
    ['switch_to_tab_9', 'Switch to tab 9'],
    ['new_shell_tab', 'New shell tab'],
    ['close_tab', 'Close current tab'],
  ];
  const PANE_SHORTCUT_ROWS: readonly (readonly [ShortcutKey, string])[] = [
    ['focus_pane_left', 'Focus pane left'],
    ['focus_pane_right', 'Focus pane right'],
    ['focus_pane_up', 'Focus pane up'],
    ['focus_pane_down', 'Focus pane down'],
    ['split_pane_horizontal', 'Split pane (side by side)'],
    ['split_pane_vertical', 'Split pane (stacked)'],
    ['close_pane', 'Close focused pane'],
  ];

  // Sub-tab nav within the Tabs section. Each AI builtin gets its own
  // sub-tab; every Shell tab is grouped under 'shells'. Keeps the
  // previously-collapsible <details> wall navigable.
  type TabsSubSection = AiTabId | 'shells';
  let tabsSubSection = $state<TabsSubSection>('claude');
  // Sub-tab nav within the Offload section: the backend pool + limits live
  // under 'pool'; native tools, allowlist, and command policies under 'tools'.
  // (MCP servers moved to their own top-level `mcp` section — they're usable by
  // Claude Code directly now, not just the offload worker.)
  type OffloadSubSection = 'pool' | 'tools';
  let offloadSubSection = $state<OffloadSubSection>('pool');
  function subSectionForTabId(tabId: string): TabsSubSection {
    if (
      tabId === 'claude' ||
      tabId === 'claude-local' ||
      tabId === 'opencode'
    ) {
      return tabId;
    }
    return 'shells';
  }

  // Keep `snapshot` in sync with the global store. Every input mutates
  // `snapshot` and pushes via `applySettings`; the broadcast comes back and
  // overwrites `snapshot` (which is fine — same value, no churn).
  let unsub: (() => void) | undefined;

  function aiTabFromSnapshot(id: string): AiToolTabConfig | null {
    if (!snapshot) return null;
    const entry = findTab(snapshot, id);
    return entry && entry.kind === 'ai_tool' ? entry : null;
  }

  function captureBaseline(tab: AiTabId) {
    const entry = aiTabFromSnapshot(tab);
    if (!entry) return;
    tabBaselines = {
      ...tabBaselines,
      [tab]: structuredClone($state.snapshot(entry)),
    };
  }

  // V1.4-07 A: deep-link to a specific tab's section. Cold-open path
  // reads the pending target from the backend on mount; hot-open path
  // listens for `settings-deep-link` events fired while the window is
  // already open. Both call `scrollToTabSection` with the same id.
  let unlistenDeepLink: (() => void) | undefined;
  // Async-IIFE / async-onMount disposal guard. The deep-link listener
  // is registered after an `await` and may not resolve before the
  // window closes. Without this flag the late-resolving listener gets
  // stored in `unlistenDeepLink` after onDestroy already ran, leaking
  // the listener for the rest of the parent process's life.
  let disposed = false;

  // Every valid sidebar section id, for validating a `section:` deep-link
  // target before assigning it (a hand-crafted event shouldn't be able to set
  // `activeSection` to garbage).
  const SECTION_IDS = new Set<string>(SECTIONS.map((s) => s.id));

  // Route a cold-open deep-link target: a `section:<id>` jumps the sidebar to
  // that section (V22 Phase E — the Code Intelligence checks nudge chip uses
  // this); anything else is a tab id for the Tabs section scroll.
  function applyDeepLinkTarget(target: string): void {
    const sectionPrefix = 'section:';
    if (target.startsWith(sectionPrefix)) {
      const id = target.slice(sectionPrefix.length);
      if (SECTION_IDS.has(id)) activeSection = id as SectionId;
      return;
    }
    scrollToTabSection(target);
  }

  function scrollToTabSection(tabId: string): void {
    // Sidebar nav + sub-tabs both hide content, so flip both before
    // looking up the inner element — otherwise it wouldn't be in the
    // DOM yet on a cold open.
    activeSection = 'tabs';
    tabsSubSection = subSectionForTabId(tabId);
    queueMicrotask(() => {
      const el = document.getElementById(`tab-section-${tabId}`);
      if (!el) return;
      // Force any wrapping <details> open so the section is visible.
      if (el instanceof HTMLDetailsElement) el.open = true;
      el.scrollIntoView({ block: 'start', behavior: 'smooth' });
    });
  }

  onMount(async () => {
    await initSettings();
    if (disposed) return;
    startBackendStatusPolling();
    snapshot = structuredClone(get(settings));
    for (const t of AI_TABS) captureBaseline(t);
    unsub = settings.subscribe((s) => {
      snapshot = structuredClone(s);
    });
    listVoices()
      .then((v) => {
        voices = v.length > 0 ? v : [snapshot?.tts.voice ?? 'af_heart'];
      })
      .catch((e) => console.warn('list_voices failed', e));
    listSttModels()
      .then((m) => (sttModels = m))
      .catch((e) => console.warn('stt_list_models failed', e));
    listInputDevices()
      .then((d) => (inputDevices = d))
      .catch((e) => console.warn('stt_list_input_devices failed', e));
    void loadComposeTemplates();
    void loadLlmPricing();
    // Best-effort: the audit census drives the Code Audit section's per-tool
    // "not applicable" hints and quality auto-selection. The refresh variant
    // has the backend take (or reuse, ≤60s cache) a real census — and apply
    // auto-selection — so both work before the first scan; while the feature
    // is disabled it's a plain snapshot read (empty census, no hints).
    auditRefreshCensus()
      .then((s) => (auditCensus = s.census))
      .catch(() => {});
    harnessVersionsGet()
      .then((hv) => (harnessFresh = hv))
      .catch((e) => console.warn('harness_versions_get failed', e));
    for (const t of AI_TABS) {
      aiToolTabDefaults(t)
        .then((d) => {
          tabDefaults = { ...tabDefaults, [t]: d };
        })
        .catch((e) => console.warn(`ai_tool_tab_defaults(${t}) failed`, e));
    }

    // V1.4-07 A: cold-open deep-link. The IPC stored a target id in
    // backend state when the user clicked "Configure tab" on an AI
    // tab; we read+clear it and scroll if non-null.
    consumeSettingsDeepLink()
      .then((target) => {
        if (target) applyDeepLinkTarget(target);
      })
      .catch((e) => console.warn('consume_settings_deep_link failed', e));

    // V1.4-07 A: hot-open deep-link. Fired while this window is already
    // open and the user clicks Configure on a different tab. If we got
    // disposed between the await and now, tear the listener down
    // immediately rather than storing it where onDestroy can no longer
    // reach it.
    const deepLinkUnlisten = await listen<{ kind: string; tab_id?: string; section?: string }>(
      'settings-deep-link',
      (e) => {
        if (e.payload.kind === 'tab' && e.payload.tab_id) {
          scrollToTabSection(e.payload.tab_id);
        } else if (e.payload.kind === 'section' && e.payload.section) {
          if (SECTION_IDS.has(e.payload.section)) activeSection = e.payload.section as SectionId;
        }
      },
    );
    if (disposed) {
      deepLinkUnlisten();
      return;
    }
    unlistenDeepLink = deepLinkUnlisten;
  });

  onDestroy(() => {
    disposed = true;
    unsub?.();
    unlistenDeepLink?.();
    if (backendStatusTimer) clearInterval(backendStatusTimer);
  });

  /// Mutate the live snapshot via `updater`, then push to the backend.
  /// Backend's debounced save coalesces rapid calls (slider drags).
  function patch(updater: (s: Settings) => void) {
    if (!snapshot) return;
    const next = structuredClone($state.snapshot(snapshot));
    updater(next);
    snapshot = next;
    void applySettings(next);
  }

  /// Toggle one AI tab's enabled state. Routes through the dedicated
  /// IPC instead of a plain `applySettings` because the backend has to
  /// open / close the AI builtin tabs (kill PTY, drop scrollback) in
  /// response — `settings_update` alone wouldn't trigger that
  /// lifecycle. Optimistically updates the local snapshot so the
  /// checkbox reflects the new value before the broadcast comes back;
  /// the broadcast overwrites with the same value harmlessly.
  ///
  /// The "at least one AI tab must be enabled" rule is enforced two
  /// ways: the checkbox renders `disabled` when it would be the last
  /// remaining tick, and the IPC additionally rejects an empty array
  /// as defense-in-depth.
  ///
  /// If the user disables the tab they're currently viewing, jump to
  /// the first surviving tab in canonical order so the sub-tab body
  /// doesn't render an empty-state hint until the broadcast settles.
  async function toggleAiTabEnabled(id: AiTabId, enable: boolean) {
    if (!snapshot) return;
    const prev = snapshot.enabled_ai_tabs;
    const wasEnabled = prev.includes(id);
    if (wasEnabled === enable) return;
    let next_ids: AiTabId[];
    if (enable) {
      // Insert in canonical order so the persisted list mirrors the
      // tab-bar order users see.
      const order: AiTabId[] = ['claude', 'claude-local', 'opencode'];
      next_ids = order.filter((x) => prev.includes(x) || x === id);
    } else {
      if (prev.length <= 1) return; // last-one lock (also guarded by the disabled attribute)
      next_ids = prev.filter((x) => x !== id);
    }
    if (!enable && tabsSubSection === id) {
      // Jump to the first surviving id in canonical order.
      const order: AiTabId[] = ['claude', 'claude-local', 'opencode'];
      const survivor = order.find((x) => next_ids.includes(x));
      if (survivor) tabsSubSection = survivor;
    }
    const updated = structuredClone($state.snapshot(snapshot));
    updated.enabled_ai_tabs = next_ids;
    snapshot = updated;
    aiTabsError = null;
    try {
      await setEnabledAiTabs(next_ids);
    } catch (e) {
      console.error('set_enabled_ai_tabs failed:', e);
      const restored = structuredClone($state.snapshot(snapshot));
      restored.enabled_ai_tabs = prev;
      snapshot = restored;
      // The backend rejects enabling an OpenCode tab when `opencode` can't be
      // resolved (not in ebin, not on PATH) — surface that specifically so the
      // user knows to install it; everything else is a generic failure.
      const kind = (e as { kind?: string } | null)?.kind;
      aiTabsError =
        kind === 'opencode-not-found'
          ? 'OpenCode was not found in ebin or on your PATH. Install it from https://opencode.ai/docs (or drop opencode.exe in ebin/), then try again.'
          : 'Failed to update AI tabs — see logs for details.';
    }
  }

  // V1.4-04 B.5: inline UI for save/manage presets. Implemented as
  // toggleable inline panels rather than modal dialogs to match the
  // SettingsApp's existing flow (no <dialog> elements elsewhere).
  let savingPreset = $state(false);
  let newPresetName = $state('');
  let savePresetError = $state<string | null>(null);
  let managingPresets = $state(false);

  function startSavePreset() {
    savingPreset = true;
    newPresetName = '';
    savePresetError = null;
  }

  function cancelSavePreset() {
    savingPreset = false;
    newPresetName = '';
    savePresetError = null;
  }

  function commitSavePreset() {
    if (!snapshot) return;
    const name = newPresetName.trim();
    if (!name) {
      savePresetError = 'Name required.';
      return;
    }
    if (snapshot.terminal.background.presets.some((p) => p.name === name)) {
      savePresetError = `A preset named "${name}" already exists.`;
      return;
    }
    patch((s) => {
      const cfg = toPresetConfig(s.terminal.background);
      s.terminal.background.presets = [
        ...s.terminal.background.presets,
        { name, config: cfg },
      ];
    });
    savingPreset = false;
    newPresetName = '';
    savePresetError = null;
  }

  function deletePreset(name: string) {
    patch((s) => {
      s.terminal.background.presets = s.terminal.background.presets.filter(
        (p) => p.name !== name,
      );
    });
  }

  function renamePreset(oldName: string, nextName: string) {
    if (!snapshot) return;
    const trimmed = nextName.trim();
    if (!trimmed || trimmed === oldName) return;
    if (snapshot.terminal.background.presets.some((p) => p.name === trimmed)) {
      // Silent reject — duplicate. The input value reverts on next
      // store flush via the {#each} key change.
      return;
    }
    patch((s) => {
      const idx = s.terminal.background.presets.findIndex(
        (p) => p.name === oldName,
      );
      if (idx < 0) return;
      s.terminal.background.presets[idx].name = trimmed;
    });
  }

  /// Replace the AI-tab entry at `id` in the snapshot. Used by the
  /// TabSettingsSection's bound setter; the array shape forces the
  /// find-by-id lookup at write time.
  function patchAiTab(id: string, value: AiToolTabConfig) {
    // The value came in via a $bindable() prop spread from the child
    // (TabSettingsSection). The spread copies own keys but leaves nested
    // children as $state proxy references. Snapshotting here flattens
    // those to plain JS so structuredClone in the store subscriber and
    // Tauri's IPC serializer don't choke. See the DataCloneError that
    // surfaced when wiring the per-tab Terminal palette dropdown.
    const plain = $state.snapshot(value) as AiToolTabConfig;
    patch((s) => {
      const idx = findTabIndex(s, id);
      if (idx < 0) return;
      s.tabs[idx] = plain;
    });
  }

  // Restart-affecting subset: command + args + cwd + env. Notifications,
  // first_launch_notice_dismissed, and (V20) the tts_injection speak gate
  // apply live and are excluded — the out-of-band TTS source reads the toggle
  // per-utterance, so flipping it takes effect without relaunching the tab.
  function restartShape(t: AiToolTabConfig) {
    return {
      command: t.command,
      args: t.args,
      cwd: t.cwd,
      env: t.env,
    };
  }

  const restartRequired = $derived.by(() => {
    const out: Record<string, boolean> = {};
    if (!snapshot) return out;
    for (const t of AI_TABS) {
      const baseline = tabBaselines[t];
      const live = aiTabFromSnapshot(t);
      if (!baseline || !live) continue;
      out[t] = JSON.stringify(restartShape(live)) !== JSON.stringify(restartShape(baseline));
    }
    return out;
  });

  async function restartTab(tab: AiTabId) {
    await requestTabRestart(tab);
    captureBaseline(tab);
  }

  /// Tabs visible in the Tabs section, in their stored order. Filtered
  /// view of `snapshot.tabs` so the template can render AI tabs and Shell
  /// tabs differently. Empty array when settings haven't loaded yet.
  const tabEntries = $derived<TabConfig[]>(snapshot?.tabs ?? []);

  /// Resolved theme default for the waveform color, used as the picker's
  /// displayed value when `avatar.waveform.color` is empty (the "follow
  /// theme" sentinel). Re-evaluates whenever `ui.theme` changes — the
  /// `<html data-theme>` attribute has already been updated by
  /// `settings_main.ts` so `getComputedStyle` reflects the new theme.
  const themeWaveformColor = $derived.by(() => {
    void snapshot?.ui.theme;
    if (typeof window === 'undefined') return '#bb55ff';
    const v = getComputedStyle(document.documentElement)
      .getPropertyValue('--waveform-color')
      .trim();
    return v || '#bb55ff';
  });

  /// Active-tab metadata, used by the Appearance → "Apply to global"
  /// button. The session's `active_tab_id` is the canonical
  /// "currently focused tab" reference at the settings layer; if
  /// nothing has set it yet (fresh install before any tab focus),
  /// fall back to the first tab so the action remains useful.
  const activeTabId = $derived(
    snapshot?.session.active_tab_id ?? snapshot?.tabs[0]?.id ?? null,
  );
  const activeTab = $derived(
    activeTabId && snapshot
      ? asThemedTabConfig(snapshot.tabs.find((t) => t.id === activeTabId)) ?? null
      : null,
  );
  const activeTabHasOverrides = $derived(
    activeTab !== null &&
      (activeTab.theme_override !== null ||
        (activeTab.background_override !== null &&
          activeTab.background_override !== 'disabled')),
  );

  /// Promote the active tab's terminal palette + background overrides
  /// to the global terminal settings, then clear overrides on every
  /// tab so all tabs inherit the new global. The 'disabled' literal
  /// in `background_override` is an opt-out, not a config, so it does
  /// not get promoted.
  /// Hard reset: replaces the live Settings struct with the canonical
  /// defaults from `defaultSettings()`. Wipes user-created shell tabs,
  /// saved layouts, shortcut overrides, etc. — fully destructive, so
  /// gated on a native `confirm()` prompt.
  function resetSettingsToDefaults() {
    const ok = confirm(
      'Reset every setting to its default? This wipes:\n' +
        '  • all user-created shell tabs\n' +
        '  • saved layouts and presets\n' +
        '  • shortcut overrides\n' +
        '  • theme, background, and per-tab overrides\n' +
        '\nThis cannot be undone.',
    );
    if (!ok) return;
    void applySettings(defaultSettings());
  }

  function applyActiveTabOverridesToGlobal() {
    patch((s) => {
      const id = s.session.active_tab_id ?? s.tabs[0]?.id;
      if (!id) return;
      const src = asThemedTabConfig(s.tabs.find((t) => t.id === id));
      if (!src) return;
      if (src.theme_override) {
        s.terminal.theme = src.theme_override;
      }
      if (
        src.background_override !== null &&
        src.background_override !== 'disabled'
      ) {
        s.terminal.background = {
          ...s.terminal.background,
          ...src.background_override,
        };
      }
      // Preview tabs carry neither field — nothing to clear on them.
      for (const t of s.tabs) {
        if (t.kind === 'preview') continue;
        t.theme_override = null;
        t.background_override = null;
      }
    });
  }

  function aiTabAt(id: string): AiToolTabConfig | null {
    return aiTabFromSnapshot(id);
  }

  function shellSummary(t: ShellTabConfig): string {
    const args = t.args.length > 0 ? ' ' + t.args.join(' ') : '';
    return `${t.command}${args}`;
  }

  /// Replace the Shell-tab entry's notification config in the snapshot.
  /// Inline-editable in the Settings window (M4) — notifications apply
  /// live, no restart needed, so the existing settings broadcast flow is
  /// all we need. Spawn-affecting fields (command/args/cwd) are read-only
  /// here; the user changes them via the tab bar's right-click → Configure.
  function patchShellNotifications(
    id: string,
    next: ShellTabConfig['notifications'],
  ) {
    patch((s) => {
      const idx = findTabIndex(s, id);
      if (idx < 0) return;
      const entry = s.tabs[idx];
      if (entry.kind !== 'shell') return;
      s.tabs[idx] = { ...entry, notifications: next };
    });
  }

  async function pickFile(
    name: string,
    extensions: string[],
  ): Promise<string | null> {
    try {
      const r = await open({ multiple: false, filters: [{ name, extensions }] });
      if (typeof r === 'string') return r;
      return null;
    } catch (e) {
      console.error('dialog open failed', e);
      return null;
    }
  }

  // Browse for an external-tool executable and store its path (Settings →
  // Bottom bar). Cancelling the dialog leaves the current value untouched.
  // `.cmd`/`.bat` are included because many tools ship as launcher shims (npm
  // bins, PMD's pmd.bat) rather than real .exes — the spawn path runs them
  // through cmd.exe, so they work anywhere an .exe does.
  async function pickToolExe(tool: keyof Settings['external_tools']) {
    const p = await pickFile('Executable', ['exe', 'cmd', 'bat', 'com']);
    if (p) patch((s) => (s.external_tools[tool] = p));
  }

  // ── V23 Phase A: Code Audit tools ──────────────────────────────────────
  // Per-tool Detect probe result, keyed by tool id. `'probing'` while the IPC
  // is in flight. Display-only — the probe never writes back into the tool's
  // `path` field, so the stored config stays "resolve normally" unless the user
  // browses to an exe.
  let auditDetect = $state<Record<string, AuditDetectResult | 'probing' | undefined>>({});

  // V25 Phase D: the latest scan's language census, read once from the runner so
  // the tool rows can flag those the current project gates off ("not applicable
  // to the current project"). Empty (both lists) before any scan — no hint then.
  let auditCensus = $state<AuditCensus>({ extensions: [], markers: [] });

  // V25 Phase D: the tool rows split into the Security / Quality groups the
  // section renders under separate headers.
  let auditGroups = $derived(
    snapshot ? auditToolGroups(snapshot) : { security: [], quality: [] },
  );

  // Mutate one audit tool's config (by id) in place through the normal patch
  // path. No-op if the id isn't present (e.g. dropped by a future migration).
  function patchAuditTool(id: AuditToolId, updater: (t: AuditToolConfig) => void): void {
    patch((s) => {
      const t = s.code_audit.tools.find((x) => x.id === id);
      if (t) updater(t);
    });
  }

  // The enabled checkbox goes through here (not the generic patchAuditTool):
  // a manual QUALITY edit flips auto-selection to manual mode, so the choice
  // sticks across census refreshes instead of being re-derived at next scan.
  function toggleAuditToolEnabled(id: AuditToolId, enabled: boolean): void {
    patch((s) => {
      const t = s.code_audit.tools.find((x) => x.id === id);
      if (t) t.enabled = enabled;
      if (AUDIT_TOOL_CATEGORY[id] === 'quality') s.code_audit.quality_auto_select = false;
    });
  }

  // The "Auto-select for this project" button: back to automatic mode, and —
  // when a census is already known — apply the project-language selection
  // immediately rather than waiting for the next census refresh/scan.
  function applyQualityAutoSelect(): void {
    patch((s) => {
      s.code_audit.quality_auto_select = true;
      if (!censusIsEmpty(auditCensus)) {
        s.code_audit.tools = autoSelectQuality(s.code_audit.tools, auditCensus);
      }
    });
  }

  async function detectAuditTool(id: AuditToolId): Promise<void> {
    auditDetect = { ...auditDetect, [id]: 'probing' };
    try {
      // Probe the LIVE editing value, not the persisted setting — a just-typed
      // path would otherwise race the fire-and-forget applySettings push.
      const path = snapshot?.code_audit.tools.find((t) => t.id === id)?.path ?? '';
      const r = await auditDetectTool(id, path);
      auditDetect = { ...auditDetect, [id]: r };
    } catch (e) {
      auditDetect = {
        ...auditDetect,
        [id]: { found: false, path: null, version: null, error: String(e) },
      };
    }
  }

  // Browse for an audit tool executable and store it as that tool's `path`
  // override. Includes `.cmd`/`.bat` — the node tools (eslint, knip) only
  // exist as npm launcher shims, never as standalone .exes.
  async function pickAuditToolExe(id: AuditToolId): Promise<void> {
    const p = await pickFile('Executable', ['exe', 'cmd', 'bat', 'com']);
    if (p) patchAuditTool(id, (t) => (t.path = p));
  }

  // Persist after an in-place `bind:` edit of an audit tool's extra_args
  // (mirrors `commitGraphIgnore`). Fired on ArrayEditor commit boundaries.
  function commitAudit(): void {
    if (!snapshot) return;
    void applySettings($state.snapshot(snapshot));
  }

  function imagePicker(state: keyof Settings['avatar']['images']) {
    return async () => {
      const p = await pickFile('Image / Video', [
        'png',
        'jpg',
        'jpeg',
        'gif',
        'webp',
        'mp4',
        'webm',
        'mov',
      ]);
      if (p === null) return;
      patch((s) => {
        s.avatar.images[state] = p;
      });
    };
  }

  async function pickTransition() {
    const p = await pickFile('Image / Video', [
      'png',
      'jpg',
      'jpeg',
      'gif',
      'webp',
      'mp4',
      'webm',
      'mov',
    ]);
    if (p === null) return;
    patch((s) => {
      s.avatar.transition.path = p;
    });
  }

  function basename(p: string | null): string {
    if (!p) return '— not set —';
    return p.split(/[/\\]/).pop() ?? p;
  }
</script>

{#if useCustomTitleBar}
  <TuiTitleBar title="cImp settings" />
{/if}
{#if !snapshot}
  <div class="loading">Loading settings…</div>
{:else}
  <div class="root">
    <nav class="sidebar" aria-label="Settings sections">
      <div class="sidebar-title">Settings</div>
      {#each SECTIONS as s}
        <button
          type="button"
          class:active={activeSection === s.id}
          onclick={() => (activeSection = s.id)}
        >
          {s.label}
        </button>
      {/each}
    </nav>

    <div class="content">
      <div class="inner">

      {#if activeSection === 'audio'}
        <section>
          <h2>TTS</h2>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.tts.enabled}
              onchange={(e) =>
                patch((s) => (s.tts.enabled = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Enable text-to-speech</span>
          </label>
          <small class="hint top">
            Loads the Kokoro voice model. Turn off to unload it and free
            memory — no AI output is spoken while disabled. (To keep the model
            loaded but silence playback, use <em>Mute</em> instead.)
          </small>
          <label>
            <span>Process on</span>
            <select
              value={snapshot.tts.device}
              disabled={!snapshot.tts.enabled}
              onchange={(e) => patch((s) => (s.tts.device = (e.currentTarget as HTMLSelectElement).value as ProcessingDevice))}
            >
              <option value="gpu">GPU (fall back to CPU)</option>
              <option value="cpu">CPU</option>
            </select>
          </label>
          <small class="hint">
            Where Kokoro runs. <strong>GPU</strong> uses the graphics card and
            automatically falls back to CPU if none is available;
            <strong>CPU</strong> forces CPU. Switching reloads the model on the
            new device — no restart needed.
          </small>
          <label>
            <span>Voice</span>
            <select
              value={snapshot.tts.voice}
              disabled={!snapshot.tts.enabled}
              onchange={(e) => patch((s) => (s.tts.voice = (e.currentTarget as HTMLSelectElement).value))}
            >
              {#each voices as v}
                <option value={v}>{v}</option>
              {/each}
            </select>
          </label>
          <label>
            <span>Speed: {snapshot.tts.speed.toFixed(2)}×</span>
            <input
              type="range"
              min="0.5"
              max="2"
              step="0.05"
              value={snapshot.tts.speed}
              disabled={!snapshot.tts.enabled}
              oninput={(e) =>
                patch((s) => (s.tts.speed = +(e.currentTarget as HTMLInputElement).value))}
            />
          </label>
          <label>
            <span>Volume: {Math.round(snapshot.tts.volume * 100)}%</span>
            <input
              type="range"
              min="0"
              max="1"
              step="0.01"
              value={snapshot.tts.volume}
              disabled={!snapshot.tts.enabled}
              oninput={(e) =>
                patch((s) => (s.tts.volume = +(e.currentTarget as HTMLInputElement).value))}
            />
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.tts.mute}
              disabled={!snapshot.tts.enabled}
              onchange={(e) =>
                patch((s) => (s.tts.mute = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Mute</span>
          </label>
        </section>

        <section>
          <h2>Behavior</h2>
          <small class="hint">
            TTS is only stopped by Esc (or by switching tabs) — typing never
            interrupts speech.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.behavior.auto_speak}
              onchange={(e) =>
                patch((s) => (s.behavior.auto_speak = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Auto-speak detected segments</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.behavior.follow_avatar}
              onchange={(e) =>
                patch((s) => (s.behavior.follow_avatar = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Follow avatar visibility</span>
          </label>
          <small class="hint">
            When on, hiding the avatar mutes TTS and showing it unmutes —
            the Mute toggle tracks the avatar. Turn this off to control
            mute independently.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.behavior.announce_focused_tab}
              onchange={(e) =>
                patch((s) => (s.behavior.announce_focused_tab = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Announce focused tab</span>
          </label>
          <small class="hint">
            Off by default — announcements (idle, awaiting permission, error,
            exit) only fire for background tabs. Turn on to hear them for the
            tab you're currently looking at as well.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.behavior.speak_background_tabs}
              onchange={(e) =>
                patch((s) => (s.behavior.speak_background_tabs = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Speak tagged TTS from background tabs</span>
          </label>
          <small class="hint">
            Off by default — tagged TTS segments (the spoken bits inside
            AI-tab output) only play for the active tab. Turn on to hear
            them from background tabs too. Announcements are unaffected.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.behavior.copy_on_select}
              onchange={(e) =>
                patch((s) => (s.behavior.copy_on_select = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Copy on select</span>
          </label>
          <small class="hint">
            When on, text selected in any terminal is copied to the system
            clipboard automatically.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.behavior.paste_on_right_click}
              onchange={(e) =>
                patch((s) => (s.behavior.paste_on_right_click = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Paste on right-click</span>
          </label>
          <small class="hint">
            When on, right-clicking inside any terminal pastes the system
            clipboard into the focused shell and suppresses the browser's
            default context menu.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.behavior.speak_selection_on_right_click}
              onchange={(e) =>
                patch((s) => (s.behavior.speak_selection_on_right_click = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Speak selection on Ctrl+right-click</span>
          </label>
          <small class="hint">
            When on, Ctrl+right-clicking inside any terminal reads the
            selected text aloud through TTS. Holding Ctrl always suppresses
            paste, so the gesture never pastes the clipboard.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.tts.selection_highlight.enabled}
              onchange={(e) =>
                patch((s) => (s.tts.selection_highlight.enabled = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Highlight selection while reading</span>
          </label>
          <small class="hint">
            While the selection is read aloud, it is recolored and the
            highlight recedes sentence-by-sentence as each is spoken. The
            sentence being read uses a distinct accent color; finished text
            returns to its original colors. Press Esc to cancel and restore.
          </small>
          <small class="hint">
            Uncheck "Custom" on any channel to leave it as the terminal's own
            palette color (e.g. tint only the background, keeping the original
            text color).
          </small>
          <div class="color-grid" class:disabled={!snapshot.tts.selection_highlight.enabled}>
            {#each [
              { key: 'unread_fg', custom: 'unread_fg_custom', label: 'Unread text' },
              { key: 'unread_bg', custom: 'unread_bg_custom', label: 'Unread background' },
              { key: 'reading_fg', custom: 'reading_fg_custom', label: 'Reading text' },
              { key: 'reading_bg', custom: 'reading_bg_custom', label: 'Reading background' },
            ] as ch (ch.key)}
              <div class="color-cell">
                <span class="color-cell-label">{ch.label}</span>
                <label class="checkbox compact">
                  <input
                    type="checkbox"
                    checked={(snapshot.tts.selection_highlight as unknown as Record<string, boolean>)[ch.custom]}
                    disabled={!snapshot.tts.selection_highlight.enabled}
                    onchange={(e) =>
                      patch((s) => ((s.tts.selection_highlight as unknown as Record<string, boolean>)[ch.custom] = (e.currentTarget as HTMLInputElement).checked))}
                  />
                  <span>Custom</span>
                </label>
                <input
                  type="color"
                  value={(snapshot.tts.selection_highlight as unknown as Record<string, string>)[ch.key]}
                  disabled={!snapshot.tts.selection_highlight.enabled ||
                    !(snapshot.tts.selection_highlight as unknown as Record<string, boolean>)[ch.custom]}
                  onchange={(e) =>
                    patch((s) => ((s.tts.selection_highlight as unknown as Record<string, string>)[ch.key] = (e.currentTarget as HTMLInputElement).value))}
                />
              </div>
            {/each}
          </div>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.tts.show_selection_controls}
              onchange={(e) =>
                patch((s) => (s.tts.show_selection_controls = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Show selection-TTS controls in the status bar</span>
          </label>
          <small class="hint">
            Adds play / pause / restart / stop buttons to the bottom bar for
            reading the current terminal selection aloud (play has the same
            effect as Ctrl+right-click).
          </small>
          <label class="checkbox disabled">
            <input type="checkbox" checked={snapshot.behavior.fallback_silent} disabled />
            <span>Fallback silent on TTS error (always on in v1)</span>
          </label>
        </section>

        <section>
          <h2>Processing</h2>
          <small class="hint top">
            Stream-stability tuning for the segmenter. Increase if speech
            chops mid-sentence; decrease if reactions feel sluggish.
          </small>
          <div class="row">
            <label>
              <span>Stability timeout (ms)</span>
              <input
                type="number"
                min="0"
                max="2000"
                step="10"
                value={snapshot.processing.stability_timeout_ms}
                onchange={(e) =>
                  patch((s) => (s.processing.stability_timeout_ms = Math.max(0, +(e.currentTarget as HTMLInputElement).value)))}
              />
            </label>
            <label>
              <span>Max hold (ms)</span>
              <input
                type="number"
                min="50"
                max="5000"
                step="50"
                value={snapshot.processing.max_hold_ms}
                onchange={(e) =>
                  patch((s) => (s.processing.max_hold_ms = Math.max(50, +(e.currentTarget as HTMLInputElement).value)))}
              />
            </label>
          </div>
        </section>
      {:else if activeSection === 'stt'}
        <section>
          <h2>Speech-to-text</h2>
          <small class="hint">
            Dictate by voice instead of typing. A fully offline Whisper model
            transcribes your speech into the compose overlay for review before
            you send it. Nothing leaves your machine.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.stt.enabled}
              onchange={(e) =>
                patch((s) => (s.stt.enabled = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Enable speech-to-text</span>
          </label>
          <small class="hint">
            Shows a microphone button in the bottom bar and enables the
            push-to-talk shortcut. Requires a model in the <code>models/</code> folder.
          </small>

          <label>
            <span>Model</span>
            <select
              value={snapshot.stt.model_file}
              onchange={(e) => patch((s) => (s.stt.model_file = (e.currentTarget as HTMLSelectElement).value))}
            >
              {#if !sttModels.includes(snapshot.stt.model_file)}
                <option value={snapshot.stt.model_file}>{snapshot.stt.model_file} (missing)</option>
              {/if}
              {#each sttModels as m}
                <option value={m}>{m}</option>
              {/each}
            </select>
          </label>
          {#if !sttModels.includes(snapshot.stt.model_file)}
            <small class="hint warn">
              Model <code>{snapshot.stt.model_file}</code> isn't in the
              <code>models/</code> folder. Download a ggml Whisper model (e.g.
              <code>ggml-small.bin</code>) from
              huggingface.co/ggerganov/whisper.cpp and drop it there.
            </small>
          {:else}
            <small class="hint">
              Drop additional <code>ggml-*.bin</code> files into the
              <code>models/</code> folder to add models. Changing the model
              reloads the engine on your next recording.
            </small>
          {/if}

          <label>
            <span>Process on</span>
            <select
              value={snapshot.stt.device}
              onchange={(e) => patch((s) => (s.stt.device = (e.currentTarget as HTMLSelectElement).value as ProcessingDevice))}
            >
              <option value="gpu">GPU (fall back to CPU)</option>
              <option value="cpu">CPU</option>
            </select>
          </label>
          <small class="hint">
            Where Whisper runs. <strong>GPU</strong> uses the graphics card and
            automatically falls back to CPU if none is available;
            <strong>CPU</strong> forces CPU. Takes effect on your next recording.
          </small>

          <label>
            <span>Input device</span>
            <select
              value={snapshot.stt.input_device}
              onchange={(e) => patch((s) => (s.stt.input_device = (e.currentTarget as HTMLSelectElement).value))}
            >
              <option value="">System default</option>
              {#if snapshot.stt.input_device && !inputDevices.includes(snapshot.stt.input_device)}
                <option value={snapshot.stt.input_device}>{snapshot.stt.input_device} (not found)</option>
              {/if}
              {#each inputDevices as d}
                <option value={d}>{d}</option>
              {/each}
            </select>
          </label>

          <label>
            <span>Language</span>
            <select
              value={snapshot.stt.language}
              onchange={(e) => patch((s) => (s.stt.language = (e.currentTarget as HTMLSelectElement).value))}
            >
              {#if !STT_LANGUAGES.some((l) => l.code === snapshot!.stt.language)}
                <option value={snapshot.stt.language}>{snapshot.stt.language}</option>
              {/if}
              {#each STT_LANGUAGES as l}
                <option value={l.code}>{l.label}</option>
              {/each}
            </select>
          </label>

          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.stt.translate_to_english}
              onchange={(e) =>
                patch((s) => (s.stt.translate_to_english = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Translate to English</span>
          </label>
          <small class="hint">
            Transcribe non-English speech as English instead of verbatim.
          </small>

          <label>
            <span>Record button mode</span>
            <select
              value={snapshot.stt.button_mode}
              onchange={(e) =>
                patch((s) => (s.stt.button_mode = (e.currentTarget as HTMLSelectElement).value as 'toggle' | 'hold'))}
            >
              <option value="toggle">Toggle (click to start / stop)</option>
              <option value="hold">Hold (press and hold to record)</option>
            </select>
          </label>

          <small class="hint">
            The push-to-talk shortcut (hold to record) lives in
            <strong>Keyboard controls</strong>.
          </small>
        </section>

      {:else if activeSection === 'avatar'}
        <section>
          <h2>Avatar</h2>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.avatar.visible}
              onchange={(e) =>
                patch((s) => (s.avatar.visible = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Visible</span>
          </label>
          <label>
            <span>Type</span>
            <select
              value={snapshot.avatar.kind}
              onchange={(e) =>
                patch((s) => (s.avatar.kind = (e.currentTarget as HTMLSelectElement).value as Settings['avatar']['kind']))}
            >
              <option value="media">Picture / Video</option>
              <option value="sprite">Animated sprites</option>
            </select>
          </label>
          {#if snapshot.avatar.kind === 'sprite'}
            <label>
              <span>Sprite set</span>
              <select
                value={snapshot.avatar.sprite.set}
                onchange={(e) =>
                  patch((s) => (s.avatar.sprite.set = (e.currentTarget as HTMLSelectElement).value))}
              >
                <option value="impSprites">cImp (pixel art)</option>
                <option value="claudeSprites">Claude (pixel art)</option>
              </select>
            </label>
            <small class="hint">
              Frame-animated pixel-art mascot. Each state (Idle, Listening,
              Thinking, Speaking, Error) maps to a set of animations from the
              set's <code>manifest.json</code>; the per-state image/video and
              transition options below are ignored in this mode.
            </small>
          {/if}
          <label>
            <span>Position</span>
            <select
              value={snapshot.avatar.position}
              onchange={(e) =>
                patch((s) => (s.avatar.position = (e.currentTarget as HTMLSelectElement).value as Settings['avatar']['position']))}
            >
              <option value="top-right">Top Right</option>
              <option value="top-left">Top Left</option>
              <option value="bottom-right">Bottom Right</option>
              <option value="bottom-left">Bottom Left</option>
            </select>
          </label>
          <div class="row">
            <label>
              <span>Width (px)</span>
              <input
                type="number"
                min="50"
                max="1200"
                value={snapshot.avatar.size.width_px}
                onchange={(e) =>
                  patch((s) => (s.avatar.size.width_px = Math.max(50, +(e.currentTarget as HTMLInputElement).value)))}
              />
            </label>
            <label>
              <span>Height (px)</span>
              <input
                type="number"
                min="50"
                max="1200"
                value={snapshot.avatar.size.height_px}
                onchange={(e) =>
                  patch((s) => (s.avatar.size.height_px = Math.max(50, +(e.currentTarget as HTMLInputElement).value)))}
              />
            </label>
          </div>
          <div class="row">
            <label>
              <span>Margin X (px)</span>
              <input
                type="number"
                min="0"
                max="200"
                value={snapshot.avatar.margin.x_px}
                onchange={(e) =>
                  patch((s) => (s.avatar.margin.x_px = Math.max(0, +(e.currentTarget as HTMLInputElement).value)))}
              />
            </label>
            <label>
              <span>Margin Y (px)</span>
              <input
                type="number"
                min="0"
                max="200"
                value={snapshot.avatar.margin.y_px}
                onchange={(e) =>
                  patch((s) => (s.avatar.margin.y_px = Math.max(0, +(e.currentTarget as HTMLInputElement).value)))}
              />
            </label>
          </div>
          <label>
            <span>Opacity: {Math.round(snapshot.avatar.opacity * 100)}%</span>
            <input
              type="range"
              min="0.3"
              max="1"
              step="0.01"
              value={snapshot.avatar.opacity}
              oninput={(e) =>
                patch((s) => (s.avatar.opacity = +(e.currentTarget as HTMLInputElement).value))}
            />
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.avatar.show_border}
              onchange={(e) =>
                patch((s) => (s.avatar.show_border = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Show border</span>
          </label>

          {#if snapshot.avatar.kind !== 'sprite'}
          <h3>Per-state images</h3>
          {#each ['idle', 'listening', 'thinking', 'speaking', 'error'] as const as state}
            <div class="file-row">
              <span class="state-label">{state}</span>
              <span class="filename" title={snapshot.avatar.images[state] ?? ''}>
                {basename(snapshot.avatar.images[state])}
              </span>
              <button onclick={imagePicker(state)}>Pick…</button>
              <button
                class="ghost"
                onclick={() => patch((s) => (s.avatar.images[state] = null))}
                disabled={snapshot.avatar.images[state] === null}
              >
                Reset
              </button>
            </div>
          {/each}

          <h3>Transition</h3>
          <div class="file-row">
            <span class="state-label">Path</span>
            <span class="filename" title={snapshot.avatar.transition.path ?? ''}>
              {basename(snapshot.avatar.transition.path)}
            </span>
            <button onclick={pickTransition}>Pick…</button>
            <button
              class="ghost"
              onclick={() => patch((s) => (s.avatar.transition.path = null))}
              disabled={snapshot.avatar.transition.path === null}
            >
              Clear
            </button>
          </div>
          <small class="hint">An empty path disables transitions (states snap directly).</small>
          <label>
            <span>Duration (ms)</span>
            <input
              type="number"
              min="0"
              max="5000"
              step="50"
              value={snapshot.avatar.transition.duration_ms}
              onchange={(e) =>
                patch((s) => (s.avatar.transition.duration_ms = Math.max(0, +(e.currentTarget as HTMLInputElement).value)))}
            />
          </label>
          {/if}
        </section>

        <section>
          <h2>Waveform</h2>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.avatar.waveform.visible}
              onchange={(e) =>
                patch((s) => (s.avatar.waveform.visible = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Show waveform</span>
          </label>
          <div class="file-row">
            <span class="state-label">Color</span>
            <input
              type="color"
              value={snapshot.avatar.waveform.color || themeWaveformColor}
              oninput={(e) =>
                patch((s) => (s.avatar.waveform.color = (e.currentTarget as HTMLInputElement).value))}
            />
            <button
              class="ghost"
              onclick={() => patch((s) => (s.avatar.waveform.color = ''))}
              disabled={snapshot.avatar.waveform.color === ''}
              title="Follow active UI theme"
            >
              Reset
            </button>
          </div>
          <label>
            <span>Line width: {snapshot.avatar.waveform.line_width.toFixed(1)}</span>
            <input
              type="range"
              min="0.5"
              max="8"
              step="0.5"
              value={snapshot.avatar.waveform.line_width}
              oninput={(e) =>
                patch((s) => (s.avatar.waveform.line_width = +(e.currentTarget as HTMLInputElement).value))}
            />
          </label>
          <label>
            <span>Glow: {Math.round(snapshot.avatar.waveform.glow_intensity * 100)}%</span>
            <input
              type="range"
              min="0"
              max="1"
              step="0.05"
              value={snapshot.avatar.waveform.glow_intensity}
              oninput={(e) =>
                patch((s) => (s.avatar.waveform.glow_intensity = +(e.currentTarget as HTMLInputElement).value))}
            />
          </label>
          <label>
            <span>Opacity: {Math.round(snapshot.avatar.waveform.opacity * 100)}%</span>
            <input
              type="range"
              min="0"
              max="1"
              step="0.01"
              value={snapshot.avatar.waveform.opacity}
              oninput={(e) =>
                patch((s) => (s.avatar.waveform.opacity = +(e.currentTarget as HTMLInputElement).value))}
            />
          </label>
        </section>
      {:else if activeSection === 'theme'}
        <section>
          <h2>Theme</h2>

          <h3>UI theme</h3>
          <small class="hint top">
            Governs the cImp chrome — tab bar, status bar, dialogs.
            Distinct from the terminal palette below.
          </small>
          <label>
            <span>Theme</span>
            <select
              value={snapshot.ui.theme}
              onchange={(e) => {
                const theme = (e.currentTarget as HTMLSelectElement).value;
                patch((s) => {
                  s.ui.theme = theme;
                  // Pair the terminal palette to the chosen theme. Skipped for
                  // a user "Custom" palette so a hand-tuned palette isn't lost
                  // on a theme switch.
                  const paired = pairedPalette(theme);
                  if (paired && s.terminal.theme.name !== 'Custom') {
                    s.terminal.theme.name = paired;
                    s.terminal.theme.custom = null;
                  }
                });
              }}
            >
              {#each $themeRegistry as t}
                <option value={t.id}>{t.name}</option>
              {/each}
            </select>
          </label>

          <h3>Terminal palette</h3>
          <small class="hint top">
            Colors used inside terminal tabs. Each tab can override this in
            its Configure dialog.
          </small>
          <label class="palette-row">
            <span>Palette</span>
            <select
              value={snapshot.terminal.theme.name}
              onchange={(e) => {
                const name = (e.currentTarget as HTMLSelectElement).value;
                patch((s) => {
                  // Read the previous name from `s` itself — `patch`'s
                  // working copy holds the pre-update value at entry,
                  // which is what we want for seeding.
                  const previousName = s.terminal.theme.name;
                  s.terminal.theme.name = name;
                  if (name === 'Custom') {
                    if (!s.terminal.theme.custom) {
                      const seed =
                        previousName === 'Custom'
                          ? defaultPalette()
                          : resolveBundledTheme(previousName);
                      s.terminal.theme.custom = { ...seed } as ThemeColorsWire;
                    }
                  } else {
                    s.terminal.theme.custom = null;
                  }
                });
              }}
            >
              {#each $paletteRegistry as p}
                <option value={p.name}>{p.name}</option>
              {/each}
              <option value="Custom">Custom…</option>
            </select>
            <ThemeSwatch
              name={snapshot.terminal.theme.name}
              custom={snapshot.terminal.theme.custom}
            />
          </label>

          {#if snapshot.terminal.theme.name === 'Custom' && snapshot.terminal.theme.custom}
            <CustomThemeEditor
              value={snapshot.terminal.theme.custom}
              onchange={(next) =>
                patch((s) => {
                  s.terminal.theme.custom = next;
                })}
            />
          {/if}
        </section>
        <section>
          <h2>Terminal background</h2>
          <small class="hint top">
            Image, color, and gradient options applied behind every
            terminal tab. Per-tab overrides live in each tab's Configure
            dialog.
          </small>

          <BackgroundConfigEditor
            bind:config={
              () => snapshot!.terminal.background,
              (v) =>
                patch((s) => {
                  s.terminal.background = v;
                })
            }
          />

          <h3>Presets</h3>
          <div class="preset-actions">
            <button type="button" onclick={startSavePreset}>Save as preset…</button>
            <button
              type="button"
              onclick={() => (managingPresets = !managingPresets)}
            >
              {managingPresets ? 'Done managing' : 'Manage presets…'}
            </button>
          </div>

          {#if savingPreset}
            <div class="preset-save">
              <input
                type="text"
                placeholder="Preset name"
                bind:value={newPresetName}
                onkeydown={(e) => {
                  if (e.key === 'Enter') commitSavePreset();
                  if (e.key === 'Escape') cancelSavePreset();
                }}
              />
              <button type="button" onclick={commitSavePreset}>Save</button>
              <button type="button" onclick={cancelSavePreset}>Cancel</button>
              {#if savePresetError}
                <small class="error">{savePresetError}</small>
              {/if}
            </div>
            <small class="hint">
              Presets reference image paths by absolute location — moving an
              image file breaks any preset that uses it.
            </small>
          {/if}

          {#if managingPresets}
            {#if snapshot.terminal.background.presets.length === 0}
              <small class="hint">No presets saved yet.</small>
            {:else}
              <ul class="preset-list">
                {#each snapshot.terminal.background.presets as p (p.name)}
                  <li>
                    <input
                      type="text"
                      value={p.name}
                      onchange={(e) =>
                        renamePreset(
                          p.name,
                          (e.currentTarget as HTMLInputElement).value,
                        )}
                    />
                    <button type="button" onclick={() => deletePreset(p.name)}>
                      Delete
                    </button>
                  </li>
                {/each}
              </ul>
            {/if}
          {/if}

          <h3>Preview</h3>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.terminal.background.preview_category_flips}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.terminal.background.preview_category_flips = (
                      e.currentTarget as HTMLInputElement
                    ).checked),
                )}
            />
            <span>Preview image / category changes in Configure Tab dialog</span>
          </label>
          <small class="hint">
            When off, image-toggle and category-flip changes wait for Save in
            the Configure Tab dialog. Color, opacity, blur, size, position,
            and tint always preview live.
          </small>
          <label>
            <span>Scrollback kept across renderer switches (lines)</span>
            <input
              type="number"
              min="0"
              value={snapshot.terminal.background.snapshot_lines}
              onchange={(e) =>
                patch((s) => {
                  const n = Number((e.currentTarget as HTMLInputElement).value);
                  s.terminal.background.snapshot_lines = Number.isFinite(n)
                    ? Math.max(0, Math.floor(n))
                    : 2000;
                })}
            />
          </label>
          <small class="hint">
            Rows re-painted when a background change switches the terminal
            renderer (WebGL ↔ DOM). Higher keeps more history through the
            flip at the cost of a bigger in-memory snapshot.
          </small>
        </section>
        <section>
          <h2>Display</h2>
          <label>
            <span>Terminal font family</span>
            <input
              type="text"
              value={snapshot.display.terminal_font_family}
              onchange={(e) =>
                patch((s) => (s.display.terminal_font_family = (e.currentTarget as HTMLInputElement).value))}
            />
          </label>
          <label>
            <span>Terminal font size (px)</span>
            <input
              type="number"
              min="8"
              max="48"
              value={snapshot.display.terminal_font_size}
              onchange={(e) =>
                patch((s) => (s.display.terminal_font_size = Math.max(8, +(e.currentTarget as HTMLInputElement).value)))}
            />
          </label>
        </section>

        <section>
          <h2>Compose</h2>
          <small class="hint top">
            Sizing of the multi-line compose box that opens for prompts.
          </small>
          <div class="row">
            <label>
              <span>Min height (px)</span>
              <input
                type="number"
                min="40"
                max="400"
                value={snapshot.compose.min_height_px}
                onchange={(e) =>
                  patch((s) => (s.compose.min_height_px = Math.max(40, +(e.currentTarget as HTMLInputElement).value)))}
              />
            </label>
            <label>
              <span>Max height (px)</span>
              <input
                type="number"
                min="60"
                max="800"
                value={snapshot.compose.max_height_px}
                onchange={(e) =>
                  patch((s) => (s.compose.max_height_px = Math.max(60, +(e.currentTarget as HTMLInputElement).value)))}
              />
            </label>
          </div>
        </section>

        <section>
          <h2>Per-tab overrides</h2>
          <small class="hint top">
            Promote the active tab's terminal palette and background
            overrides to the global defaults, then clear the overrides
            on every tab so they inherit the new global. Useful after
            dialing in one tab and wanting the rest to match.
          </small>
          <button
            type="button"
            class="promote-overrides"
            onclick={applyActiveTabOverridesToGlobal}
            disabled={!activeTabHasOverrides}
          >
            {#if activeTab && activeTabHasOverrides}
              Apply "{activeTab.name}" overrides to global
            {:else if activeTab}
              No overrides on "{activeTab.name}" to promote
            {:else}
              No active tab
            {/if}
          </button>
        </section>
      {:else if activeSection === 'bottom-bar'}
        <section>
          <h2>Claude session usage</h2>
          <small class="hint top">
            Shows your Claude Code session (5h) and weekly (7d) quota in the
            bottom bar, next to Layouts. Data comes from Claude's usage
            endpoint; the widget hides when you're not logged into Claude.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.usage.enabled}
              onchange={(e) =>
                patch((s) => (s.usage.enabled = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Show usage in the bottom bar</span>
          </label>
          <small class="hint">
            The toggles below pick which pieces of each window are shown
            (they apply to both the 5h and 7d readouts).
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.usage.show_bar}
              disabled={!snapshot.usage.enabled}
              onchange={(e) =>
                patch((s) => (s.usage.show_bar = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Bar</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.usage.show_percentage}
              disabled={!snapshot.usage.enabled}
              onchange={(e) =>
                patch((s) => (s.usage.show_percentage = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Percentage</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.usage.show_countdown}
              disabled={!snapshot.usage.enabled}
              onchange={(e) =>
                patch((s) => (s.usage.show_countdown = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Countdown timer</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.usage.show_reset_clock}
              disabled={!snapshot.usage.enabled}
              onchange={(e) =>
                patch((s) => (s.usage.show_reset_clock = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Reset clock (local time)</span>
          </label>
          <label>
            <span>Poll interval (seconds)</span>
            <input
              type="number"
              min="15"
              max="3600"
              step="15"
              disabled={!snapshot.usage.enabled}
              value={snapshot.usage.poll_interval_secs}
              onchange={(e) =>
                patch((s) => (s.usage.poll_interval_secs = Math.max(15, +(e.currentTarget as HTMLInputElement).value)))}
            />
          </label>
          <small class="hint">
            How often the usage figures refresh. Minimum 15s; the countdown
            ticks every second locally between refreshes.
          </small>
        </section>

        <section>
          <h2>Local machine information</h2>
          <small class="hint top">
            Live CPU / memory / GPU / network panel in the bottom bar, right of
            the Claude session usage meter.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.system_stats.enabled}
              onchange={(e) =>
                patch((s) => (s.system_stats.enabled = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Show local machine information</span>
          </label>
          <small class="hint">
            The toggles below pick which components are shown.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.system_stats.show_cpu}
              disabled={!snapshot.system_stats.enabled}
              onchange={(e) =>
                patch((s) => (s.system_stats.show_cpu = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>CPU usage</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.system_stats.show_memory}
              disabled={!snapshot.system_stats.enabled}
              onchange={(e) =>
                patch((s) => (s.system_stats.show_memory = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Memory</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.system_stats.show_gpu}
              disabled={!snapshot.system_stats.enabled}
              onchange={(e) =>
                patch((s) => (s.system_stats.show_gpu = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>GPU (usage + VRAM)</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.system_stats.show_gpu_temp}
              disabled={!snapshot.system_stats.enabled || !snapshot.system_stats.show_gpu}
              onchange={(e) =>
                patch((s) => (s.system_stats.show_gpu_temp = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>GPU temperature</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.system_stats.show_network}
              disabled={!snapshot.system_stats.enabled}
              onchange={(e) =>
                patch((s) => (s.system_stats.show_network = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Network</span>
          </label>
          <label>
            <span>Poll interval (seconds)</span>
            <input
              type="number"
              min="1"
              max="60"
              disabled={!snapshot.system_stats.enabled}
              value={snapshot.system_stats.poll_interval_secs}
              onchange={(e) =>
                patch((s) => (s.system_stats.poll_interval_secs = Math.max(1, +(e.currentTarget as HTMLInputElement).value)))}
            />
          </label>
          <small class="hint">
            How often CPU / GPU / network are sampled. The graphs update at this
            rate.
          </small>
        </section>

        <section>
          <h2>Status bar arrangement</h2>
          <small class="hint top">
            Drag the session and local-machine panels in the bottom bar to
            reorder them, or drag one sideways to leave a gap (e.g. push the
            local-machine panel to the right). Reordering clears any gaps.
          </small>
          <button
            type="button"
            class="reset-arrangement"
            onclick={() =>
              patch(
                (s) =>
                  (s.ui.status_bar = {
                    items: [
                      { component: 'usage', gap: 0 },
                      { component: 'system_stats', gap: 0 },
                    ],
                  }),
              )}
          >
            Reset to default arrangement
          </button>
          <small class="hint">
            Restores the default order (session, then local machine) and removes
            any spacers you added.
          </small>
        </section>

        <section>
          <h2>Claude context bar</h2>
          <small class="hint top">
            Adds a context-window usage bar to Claude Code's own status line
            inside each Claude tab — e.g. <code>Opus ▓▓▓▓▓░░░░░ 50% (100k/200k)</code>,
            themed to your terminal palette. cImp wires this up only for the
            Claude tabs it launches; your global Claude Code configuration is
            left untouched.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.statusline.enabled}
              onchange={(e) =>
                patch((s) => (s.statusline.enabled = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Show the context bar in Claude's status line</span>
          </label>
          <small class="hint">
            Takes effect on the next Claude tab launch (restart the tab to apply).
          </small>
        </section>

        <section>
          <h2>External tools</h2>
          <small class="hint top">
            The quick-launch buttons (and shell tabs) run these tools by name,
            resolved from the <code>ebin\</code> drop-in folder first, then your
            PATH. Tools are not bundled — install them yourself, or drop the
            exe into <code>ebin\</code>. To use a specific build, point cImp at
            the exe here; leave blank to resolve normally. Takes effect the
            next time you launch the tool.
          </small>
          <label>
            <span>rustnet</span>
            <div class="input-with-action">
              <input
                type="text"
                placeholder="(use ebin / PATH)"
                value={snapshot.external_tools.rustnet}
                oninput={(e) =>
                  patch(
                    (s) =>
                      (s.external_tools.rustnet = (
                        e.currentTarget as HTMLInputElement
                      ).value),
                  )}
              />
              <button
                type="button"
                class="secondary"
                onclick={() => void pickToolExe('rustnet')}
              >
                Browse…
              </button>
              <button
                type="button"
                class="secondary"
                onclick={() => patch((s) => (s.external_tools.rustnet = ''))}
              >
                Clear
              </button>
            </div>
          </label>
          <label>
            <span>broot</span>
            <div class="input-with-action">
              <input
                type="text"
                placeholder="(use ebin / PATH)"
                value={snapshot.external_tools.broot}
                oninput={(e) =>
                  patch(
                    (s) =>
                      (s.external_tools.broot = (
                        e.currentTarget as HTMLInputElement
                      ).value),
                  )}
              />
              <button
                type="button"
                class="secondary"
                onclick={() => void pickToolExe('broot')}
              >
                Browse…
              </button>
              <button
                type="button"
                class="secondary"
                onclick={() => patch((s) => (s.external_tools.broot = ''))}
              >
                Clear
              </button>
            </div>
          </label>
        </section>
      {:else if activeSection === 'tabs'}
        {@const claudeLive = aiTabAt('claude')}
        {@const claudeLocalLive = aiTabAt('claude-local')}
        {@const opencodeLive = aiTabAt('opencode')}
        {@const shellEntries = tabEntries.filter((e) => e.kind === 'shell')}
        {@const enabledAiTabs = snapshot.enabled_ai_tabs}
        {@const lastChecked = enabledAiTabs.length === 1 ? enabledAiTabs[0] : null}
        <section>
          <h2>Tabs</h2>
          <fieldset class="claude-tabs-radio">
            <legend>AI tabs enabled</legend>
            <small class="hint">
              Pick which AI-tool tabs to keep. Toggling a checkbox opens
              or closes the matching tab (the closed tab's PTY is killed
              and its scrollback dropped). At least one tab must remain
              checked.
            </small>
            <div class="radio-row">
              <label>
                <input
                  type="checkbox"
                  name="ai-tabs-enabled"
                  value="claude"
                  checked={enabledAiTabs.includes('claude')}
                  disabled={lastChecked === 'claude'}
                  onchange={(e) =>
                    void toggleAiTabEnabled(
                      'claude',
                      (e.currentTarget as HTMLInputElement).checked,
                    )}
                />
                Claude (cloud)
              </label>
              <label>
                <input
                  type="checkbox"
                  name="ai-tabs-enabled"
                  value="claude-local"
                  checked={enabledAiTabs.includes('claude-local')}
                  disabled={lastChecked === 'claude-local'}
                  onchange={(e) =>
                    void toggleAiTabEnabled(
                      'claude-local',
                      (e.currentTarget as HTMLInputElement).checked,
                    )}
                />
                Claude (local)
              </label>
              <label>
                <input
                  type="checkbox"
                  name="ai-tabs-enabled"
                  value="opencode"
                  checked={enabledAiTabs.includes('opencode')}
                  disabled={lastChecked === 'opencode'}
                  onchange={(e) =>
                    void toggleAiTabEnabled(
                      'opencode',
                      (e.currentTarget as HTMLInputElement).checked,
                    )}
                />
                OpenCode
              </label>
            </div>
            {#if aiTabsError}
              <small class="error">{aiTabsError}</small>
            {/if}
          </fieldset>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.ui.tool_activity_tab}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.ui.tool_activity_tab = (e.currentTarget as HTMLInputElement).checked),
                )}
            />
            <span>Show the <strong>Tool Activity</strong> tab</span>
          </label>
          <small class="hint">
            One place to watch tool usage: a unified feed of code-intelligence
            graph calls and offload requests, plus the graph/offload tool
            reference lists.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.preview_allow_remote}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.preview_allow_remote = (e.currentTarget as HTMLInputElement).checked),
                )}
            />
            <span>Allow <strong>Preview</strong> tabs to load remote URLs</span>
          </label>
          <small class="hint">
            Off (default) restricts Preview-tab navigation to localhost and
            private-network (RFC&nbsp;1918) hosts — the tab is meant for your
            own dev servers. On lets a Preview tab load any http(s) URL in its
            embedded webview.
          </small>
          <div class="sub-tabs" role="tablist" aria-label="Tabs sub-sections">
            <button
              type="button"
              role="tab"
              class:active={tabsSubSection === 'claude'}
              aria-selected={tabsSubSection === 'claude'}
              onclick={() => (tabsSubSection = 'claude')}
            >
              Claude
            </button>
            <button
              type="button"
              role="tab"
              class:active={tabsSubSection === 'claude-local'}
              aria-selected={tabsSubSection === 'claude-local'}
              onclick={() => (tabsSubSection = 'claude-local')}
            >
              Claude (local)
            </button>
            <button
              type="button"
              role="tab"
              class:active={tabsSubSection === 'opencode'}
              aria-selected={tabsSubSection === 'opencode'}
              onclick={() => (tabsSubSection = 'opencode')}
            >
              OpenCode
            </button>
            <button
              type="button"
              role="tab"
              class:active={tabsSubSection === 'shells'}
              aria-selected={tabsSubSection === 'shells'}
              onclick={() => (tabsSubSection = 'shells')}
            >
              Shells
              {#if shellEntries.length > 0}
                <span class="sub-tab-count">{shellEntries.length}</span>
              {/if}
            </button>
          </div>

          {#if tabsSubSection === 'claude'}
            <div id="tab-section-claude">
              {#if claudeLive}
                <TabSettingsSection
                  tabId={'claude'}
                  displayName={'Claude'}
                  bind:settings={
                    () => claudeLive,
                    (v) => patchAiTab('claude', v)
                  }
                  defaults={tabDefaults['claude'] ?? null}
                  restartRequired={restartRequired['claude'] ?? false}
                  onchange={() => {}}
                  onrestart={() => restartTab('claude')}
                />
              {:else}
                <small class="hint top">Claude tab is disabled — tick the checkbox above to enable it.</small>
              {/if}
            </div>
          {:else if tabsSubSection === 'claude-local'}
            <div id="tab-section-claude-local">
              {#if claudeLocalLive}
                <TabSettingsSection
                  tabId={'claude-local'}
                  displayName={'Claude (local)'}
                  bind:settings={
                    () => claudeLocalLive,
                    (v) => patchAiTab('claude-local', v)
                  }
                  defaults={tabDefaults['claude-local'] ?? null}
                  restartRequired={restartRequired['claude-local'] ?? false}
                  onchange={() => {}}
                  onrestart={() => restartTab('claude-local')}
                />
              {:else}
                <small class="hint top">Claude (local) tab is disabled — tick the checkbox above to enable it.</small>
              {/if}
              <section>
                <h2>Local LLM provider</h2>
                <small class="hint top">
                  Settings for this tab when <em>Use local LLM provider</em>
                  is enabled. Run a LiteLLM (or compatible) proxy that translates the
                  Anthropic Messages API to your local model — cImp does not start
                  the proxy. See the
                  <a
                    href="https://docs.litellm.ai/docs/proxy/quick_start"
                    target="_blank"
                    rel="noopener noreferrer">LiteLLM docs</a
                  >.
                </small>
                <label>
                  <span>Proxy URL</span>
                  <input
                    type="text"
                    value={snapshot?.claude_local.base_url ?? ''}
                    oninput={(e) =>
                      patch(
                        (s) =>
                          (s.claude_local.base_url = (
                            e.currentTarget as HTMLInputElement
                          ).value),
                      )}
                    placeholder="http://localhost:4000"
                  />
                  <small class="hint">
                    Becomes <code>ANTHROPIC_BASE_URL</code> on launch.
                  </small>
                </label>
                <label>
                  <span>Auth token</span>
                  <div class="input-with-action">
                    <input
                      type={showLocalToken ? 'text' : 'password'}
                      value={snapshot?.claude_local.auth_token ?? ''}
                      oninput={(e) =>
                        patch(
                          (s) =>
                            (s.claude_local.auth_token = (
                              e.currentTarget as HTMLInputElement
                            ).value),
                        )}
                      placeholder="sk-dummy"
                    />
                    <button
                      type="button"
                      class="secondary"
                      onclick={() => (showLocalToken = !showLocalToken)}
                    >
                      {showLocalToken ? 'Hide' : 'Show'}
                    </button>
                  </div>
                  <small class="hint">
                    Becomes <code>ANTHROPIC_AUTH_TOKEN</code>. Stored cleartext;
                    local proxies usually accept dummy tokens.
                  </small>
                </label>
                <label>
                  <span>Model alias (optional)</span>
                  <input
                    type="text"
                    value={snapshot?.claude_local.model_alias ?? ''}
                    oninput={(e) =>
                      patch(
                        (s) =>
                          (s.claude_local.model_alias = (
                            e.currentTarget as HTMLInputElement
                          ).value),
                      )}
                    placeholder=""
                  />
                  <small class="hint">
                    When non-empty, becomes <code>ANTHROPIC_MODEL</code>. Most
                    users leave this blank and configure model mapping inside
                    their LiteLLM proxy config.
                  </small>
                </label>
              </section>
            </div>
          {:else if tabsSubSection === 'opencode'}
            <div id="tab-section-opencode">
              {#if opencodeLive}
                <TabSettingsSection
                  tabId={'opencode'}
                  displayName={'OpenCode'}
                  bind:settings={
                    () => opencodeLive,
                    (v) => patchAiTab('opencode', v)
                  }
                  defaults={tabDefaults['opencode'] ?? null}
                  restartRequired={restartRequired['opencode'] ?? false}
                  onchange={() => {}}
                  onrestart={() => restartTab('opencode')}
                />
              {:else}
                <small class="hint top">OpenCode tab is disabled — tick the checkbox above to enable it.</small>
              {/if}
            </div>
          {:else}
            <small class="hint top">
              Shell tabs in their stored order. Each row shows notification
              text — edit command / args / cwd via right-click → Configure
              on the tab bar.
            </small>
            {#if shellEntries.length === 0}
              <small class="hint top">No shell tabs configured.</small>
            {:else}
              <div class="tabs-grid">
                {#each shellEntries as entry (entry.id)}
                  {#if entry.kind === 'shell'}
                    <details id="tab-section-{entry.id}">
                      <summary>
                        {entry.name}
                        <span class="kind-badge shell">Shell</span>
                        {#if entry.builtin}
                          <span class="builtin-tag">builtin</span>
                        {/if}
                      </summary>
                      <div class="shell-edit">
                        <label>
                          <span>Command</span>
                          <input type="text" value={shellSummary(entry)} disabled readonly />
                          <small class="hint">
                            To change the command, args, or working directory,
                            right-click the tab in the tab bar and choose
                            Configure…
                          </small>
                        </label>
                        <div class="shell-notif-row">
                          <label class="row-toggle">
                            <input
                              type="checkbox"
                              checked={entry.notifications.error.enabled}
                              onchange={(e) =>
                                patchShellNotifications(entry.id, {
                                  ...entry.notifications,
                                  error: {
                                    ...entry.notifications.error,
                                    enabled: (e.currentTarget as HTMLInputElement)
                                      .checked,
                                  },
                                })}
                            />
                            <span>Error notification</span>
                          </label>
                          <input
                            type="text"
                            value={entry.notifications.error.text}
                            disabled={!entry.notifications.error.enabled}
                            oninput={(e) =>
                              patchShellNotifications(entry.id, {
                                ...entry.notifications,
                                error: {
                                  ...entry.notifications.error,
                                  text: (e.currentTarget as HTMLInputElement).value,
                                },
                              })}
                          />
                          <small class="hint">
                            Spoken when this tab errors while you're on a different
                            tab.
                          </small>
                        </div>
                        <div class="shell-notif-row">
                          <label class="row-toggle">
                            <input
                              type="checkbox"
                              checked={entry.notifications.exited.enabled}
                              onchange={(e) =>
                                patchShellNotifications(entry.id, {
                                  ...entry.notifications,
                                  exited: {
                                    ...entry.notifications.exited,
                                    enabled: (e.currentTarget as HTMLInputElement)
                                      .checked,
                                  },
                                })}
                            />
                            <span>Exited notification</span>
                          </label>
                          <input
                            type="text"
                            value={entry.notifications.exited.text}
                            disabled={!entry.notifications.exited.enabled}
                            oninput={(e) =>
                              patchShellNotifications(entry.id, {
                                ...entry.notifications,
                                exited: {
                                  ...entry.notifications.exited,
                                  text: (e.currentTarget as HTMLInputElement).value,
                                },
                              })}
                          />
                          <small class="hint">
                            Spoken when this shell exits while you're on a different
                            tab. Use <code>{'{code}'}</code> to insert the exit code.
                          </small>
                        </div>
                      </div>
                    </details>
                  {/if}
                {/each}
              </div>
            {/if}
          {/if}
        </section>
      {:else if activeSection === 'shortcuts'}
        <section>
          <h2>Keyboard controls</h2>
          <label>
            <span>Open compose</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.open_compose,
                (v) => patch((s) => (s.shortcuts.open_compose = v))
              }
            />
          </label>
          <label>
            <span>Open compose with template picker</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.open_compose_picker,
                (v) => patch((s) => (s.shortcuts.open_compose_picker = v))
              }
            />
          </label>
          <label>
            <span>Submit compose</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.submit_compose,
                (v) => patch((s) => (s.shortcuts.submit_compose = v))
              }
            />
          </label>
          <label>
            <span>Cancel compose</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.cancel_compose,
                (v) => patch((s) => (s.shortcuts.cancel_compose = v))
              }
            />
          </label>
          <label>
            <span>Open settings</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.open_settings,
                (v) => patch((s) => (s.shortcuts.open_settings = v))
              }
            />
          </label>
          <h3>Tabs</h3>
          <label>
            <span>Switch to tab 1</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.switch_to_tab_1,
                (v) => patch((s) => (s.shortcuts.switch_to_tab_1 = v))
              }
            />
          </label>
          <label>
            <span>Switch to tab 2</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.switch_to_tab_2,
                (v) => patch((s) => (s.shortcuts.switch_to_tab_2 = v))
              }
            />
          </label>
          {#each TAB_SHORTCUT_ROWS as [key, label] (key)}
            <label>
              <span>{label}</span>
              <ShortcutCapture
                bind:value={
                  () => snapshot!.shortcuts[key],
                  (v) => patch((s) => (s.shortcuts[key] = v))
                }
              />
            </label>
          {/each}

          <h3>Panes</h3>
          {#each PANE_SHORTCUT_ROWS as [key, label] (key)}
            <label>
              <span>{label}</span>
              <ShortcutCapture
                bind:value={
                  () => snapshot!.shortcuts[key],
                  (v) => patch((s) => (s.shortcuts[key] = v))
                }
              />
            </label>
          {/each}

          <h3>Voice</h3>
          <label>
            <span>Push-to-talk (speech-to-text)</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.push_to_talk,
                (v) => patch((s) => (s.shortcuts.push_to_talk = v))
              }
            />
          </label>
          <small class="hint">
            Hold the chord to record, release to transcribe. Works only when
            speech-to-text is enabled. The default is bare
            <code>Ctrl+Shift</code> — a quick tap or a
            <code>Ctrl+Shift+&lt;key&gt;</code> chord won't trigger a recording.
          </small>
          <label>
            <span>Speak selection (text-to-speech)</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.speak_selection,
                (v) => patch((s) => (s.shortcuts.speak_selection = v))
              }
            />
          </label>
          <small class="hint">
            Reads the active terminal's current selection aloud — the keyboard
            equivalent of Ctrl+right-click. Shows a "No text selected" notice
            when nothing is selected.
          </small>
        </section>
      {:else if activeSection === 'compose'}
        <section>
          <h2>Compose</h2>
          <small class="hint top">
            Saved prompt templates, insertable from the compose overlay's
            <code>/</code> picker (type <code>/</code> on an empty line, or
            click the 📋 button beside the textarea). Variables:
            <code>{'{selection}'}</code> (the focused pane's terminal
            selection) and <code>{'{clipboard}'}</code> (the system
            clipboard) are filled in immediately; any other
            <code>{'{name}'}</code> becomes a tab-stop you Tab between and
            overtype after inserting.
          </small>

          <h3>Global templates</h3>
          <small class="hint">
            Available from every project. Saved directly to the global
            settings file, not this project's overlay.
          </small>
          {#if composeTemplatesLoading}
            <small class="hint">Loading…</small>
          {:else}
            {#if globalTemplates.length === 0}
              <small class="hint">No templates yet — add one below.</small>
            {:else}
              <ul class="template-list compose-template-list">
                {#each globalTemplates as t, i (i)}
                  <li class="compose-template-row">
                    <input
                      type="text"
                      class="compose-template-name"
                      placeholder="name"
                      value={t.name}
                      oninput={(e) =>
                        renameGlobalTemplate(i, (e.currentTarget as HTMLInputElement).value)}
                    />
                    <textarea
                      class="compose-template-body"
                      placeholder={'Template body — use {selection}, {clipboard}, or {any-name} for tab-stops'}
                      rows="2"
                      value={t.body}
                      oninput={(e) =>
                        editGlobalTemplateBody(i, (e.currentTarget as HTMLTextAreaElement).value)}
                    ></textarea>
                    <button type="button" class="danger" onclick={() => deleteGlobalTemplate(i)}
                      >Delete</button
                    >
                  </li>
                {/each}
              </ul>
            {/if}
            <div class="button-row">
              <button type="button" onclick={addGlobalTemplate}>Add template</button>
              <button
                type="button"
                disabled={!composeTemplatesDirty}
                onclick={() => void saveGlobalTemplates()}
                >Save</button
              >
              {#if composeTemplatesDirty}
                <small class="hint">Unsaved changes</small>
              {/if}
            </div>
            {#if composeTemplatesError}
              <small class="error">{composeTemplatesError}</small>
            {/if}
          {/if}

          <h3>Project templates</h3>
          <small class="hint">
            Read-only here — project-scope templates live in this project's
            <code>.cimp/config.json</code> (a top-level
            <code>prompt_templates</code> array), edited by hand or committed
            for team sharing. A project template shadows a global one of the
            same name.
          </small>
          {#if projectTemplates.length === 0}
            <small class="hint">None for this project.</small>
          {:else}
            <ul class="template-list">
              {#each projectTemplates as t (t.name)}
                <li>
                  <span class="template-name" title={t.body}>{t.name}</span>
                </li>
              {/each}
            </ul>
          {/if}
        </section>
      {:else if activeSection === 'offload'}
        <section>
          <h2>Local task offload</h2>
          <small class="hint top">
            Run a local <code>llama-server</code> and expose an
            <code>offload_task</code> tool into cImp-launched Claude tabs.
            The main session can hand token-heavy subtasks (broad codebase
            searches, large-file/log summarization, web research) to the
            local model and get back only the synthesized result —
            conserving its context window. Everything stays local. Off by
            default; the model is user-supplied (not bundled).
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.enabled}
              onchange={(e) =>
                patch((s) => (s.offload.enabled = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Enable offload</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.inject_guidance}
              onchange={(e) =>
                patch((s) => (s.offload.inject_guidance = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Inject offload guidance into the system prompt</span>
          </label>

          <hr class="card-divider lg" />
          <div class="sub-tabs" role="tablist" aria-label="Offload sub-sections">
            <button
              type="button"
              role="tab"
              class:active={offloadSubSection === 'pool'}
              aria-selected={offloadSubSection === 'pool'}
              onclick={() => (offloadSubSection = 'pool')}
            >
              Pool
            </button>
            <button
              type="button"
              role="tab"
              class:active={offloadSubSection === 'tools'}
              aria-selected={offloadSubSection === 'tools'}
              onclick={() => (offloadSubSection = 'tools')}
            >
              Tools
            </button>
          </div>

          {#if offloadSubSection === 'pool'}
          <h3>Backend pool</h3>
          <small class="hint top">
            V8-02: route each offload to the right backend. Add a LAN box or a
            cloud API alongside the local server; the router picks one per task
            by tool need, required context, tier, and availability. The single
            <code>Server command</code> above is used as one local backend when
            the pool below is empty.
          </small>

          {#if snapshot.offload.backends.length === 0}
            <div class="button-row">
              <button type="button" onclick={adoptLegacyServer} disabled={!snapshot.offload.server_command.trim()}>
                Adopt the server command above as a Local backend
              </button>
            </div>
          {/if}

          {#each snapshot.offload.backends as backend, i (i)}
            <div class="backend-card">
              <div class="backend-head">
                <input
                  class="backend-name"
                  type="text"
                  value={backend.name}
                  oninput={(e) => updateBackend(i, (b) => (b.name = (e.currentTarget as HTMLInputElement).value))}
                  placeholder="name"
                />
                <select
                  value={backend.tier}
                  onchange={(e) => updateBackend(i, (b) => (b.tier = (e.currentTarget as HTMLSelectElement).value as BackendTier))}
                >
                  <option value="quality">quality</option>
                  <option value="fast">fast</option>
                </select>
                <label class="checkbox inline">
                  <input
                    type="checkbox"
                    checked={backend.enabled}
                    onchange={(e) => updateBackend(i, (b) => (b.enabled = (e.currentTarget as HTMLInputElement).checked))}
                  />
                  <span>enabled</span>
                </label>
                {#if backend.kind.type === 'local'}
                  <label class="checkbox inline">
                    <input
                      type="checkbox"
                      checked={backend.kind.autostart}
                      onchange={(e) =>
                        updateBackend(i, (b) => {
                          if (b.kind.type === 'local') b.kind.autostart = (e.currentTarget as HTMLInputElement).checked;
                        })}
                    />
                    <span>Start on launch</span>
                  </label>
                {/if}
                <button type="button" class="secondary danger" onclick={() => removeBackend(i)}>Remove</button>
              </div>

              {#if statusFor(backend.name)}
                {@const st = statusFor(backend.name)!}
                <div class="offload-status">
                  <span class="offload-status-label">{st.kind}:</span>
                  <span class:status-error={st.state === 'error'}>{describeBackendStatus(st)} · {st.tool_scope}</span>
                  {#if st.cloud_blocked}<span class="badge warn">consent required</span>{/if}
                </div>
              {/if}

              {#if backend.kind.type === 'local'}
                <hr class="card-divider lg" />
                <label>
                  <span>Server command</span>
                  <textarea
                    class="server-command"
                    rows="6"
                    wrap="soft"
                    value={backend.kind.server_command}
                    oninput={(e) =>
                      updateBackend(i, (b) => {
                        if (b.kind.type === 'local') b.kind.server_command = (e.currentTarget as HTMLTextAreaElement).value;
                      })}
                    placeholder="llama-server --model … --port 8080 --jinja -ngl 99 --ctx-size 150000"
                  ></textarea>
                </label>
                <label class="checkbox">
                  <input
                    type="checkbox"
                    checked={backend.kind.show_command_on_start}
                    onchange={(e) =>
                      updateBackend(i, (b) => {
                        if (b.kind.type === 'local')
                          b.kind.show_command_on_start = (e.currentTarget as HTMLInputElement).checked;
                      })}
                  />
                  <span>Show command on start</span>
                </label>
                <small class="hint">
                  The Start button in Tool Activity → Offload server opens the
                  command in an editable popup first — edits apply to that
                  launch only and are not saved here.
                </small>
                <div class="button-row template-actions">
                  <button
                    type="button"
                    class="secondary"
                    class:active={templatePopup?.i === i && templatePopup?.mode === 'save'}
                    onclick={() => openTemplatePopup(i, 'save')}
                  >Save</button>
                  <button
                    type="button"
                    class="secondary"
                    class:active={templatePopup?.i === i && templatePopup?.mode === 'load'}
                    onclick={() => openTemplatePopup(i, 'load')}
                  >Load</button>
                  <button
                    type="button"
                    class="secondary"
                    class:active={templatePopup?.i === i && templatePopup?.mode === 'delete'}
                    onclick={() => openTemplatePopup(i, 'delete')}
                  >Delete</button>
                </div>

                {#if templatePopup?.i === i}
                  {@const templates = snapshot.offload.server_command_templates}
                  <div class="template-popup" role="group">
                    {#if templatePopup.mode === 'save'}
                      <div class="template-save">
                        <input
                          type="text"
                          placeholder="Template name"
                          bind:value={newTemplateName}
                          onkeydown={(e) => {
                            if (e.key === 'Enter') commitSaveLocalTemplate(i);
                            if (e.key === 'Escape') closeTemplatePopup();
                          }}
                        />
                        <button type="button" onclick={() => commitSaveLocalTemplate(i)}>Save</button>
                        <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
                      </div>
                      {#if templateError}
                        <small class="error">{templateError}</small>
                      {/if}
                    {:else if templatePopup.mode === 'load'}
                      {#if templates.length === 0}
                        <small class="hint">No saved commands yet.</small>
                      {:else}
                        <ul class="template-list">
                          {#each templates as t (t.name)}
                            <li>
                              <span class="template-name" title={t.command}>{t.name}</span>
                              <button type="button" onclick={() => loadLocalTemplate(i, t)}>Load</button>
                            </li>
                          {/each}
                        </ul>
                      {/if}
                      <div class="button-row">
                        <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
                      </div>
                    {:else if templatePopup.mode === 'delete'}
                      {#if templates.length === 0}
                        <small class="hint">No saved commands yet.</small>
                      {:else}
                        <ul class="template-list">
                          {#each templates as t (t.name)}
                            <li>
                              <span class="template-name" title={t.command}>{t.name}</span>
                              <button type="button" class="danger" onclick={() => deleteLocalTemplate(t.name)}>Delete</button>
                            </li>
                          {/each}
                        </ul>
                      {/if}
                      <div class="button-row">
                        <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
                      </div>
                    {/if}
                  </div>
                {/if}
              {:else if backend.kind.type === 'remote'}
                <hr class="card-divider lg" />
                <label>
                  <span>Base URL</span>
                  <input
                    type="text"
                    value={backend.kind.base_url}
                    oninput={(e) =>
                      updateBackend(i, (b) => {
                        if (b.kind.type === 'remote') b.kind.base_url = (e.currentTarget as HTMLInputElement).value;
                      })}
                    placeholder="http://192.168.1.50:8080  or  https://api.example.com/v1"
                  />
                </label>
                <label>
                  <span>Auth token (optional)</span>
                  <input
                    type="password"
                    value={backend.kind.auth_token}
                    oninput={(e) =>
                      updateBackend(i, (b) => {
                        if (b.kind.type === 'remote') b.kind.auth_token = (e.currentTarget as HTMLInputElement).value;
                      })}
                    placeholder="Bearer token for cloud APIs"
                  />
                </label>
                <div class="button-row template-actions">
                  <button
                    type="button"
                    class="secondary"
                    class:active={templatePopup?.i === i && templatePopup?.mode === 'save'}
                    onclick={() => openTemplatePopup(i, 'save')}
                  >Save</button>
                  <button
                    type="button"
                    class="secondary"
                    class:active={templatePopup?.i === i && templatePopup?.mode === 'load'}
                    onclick={() => openTemplatePopup(i, 'load')}
                  >Load</button>
                  <button
                    type="button"
                    class="secondary"
                    class:active={templatePopup?.i === i && templatePopup?.mode === 'delete'}
                    onclick={() => openTemplatePopup(i, 'delete')}
                  >Delete</button>
                </div>

                {#if templatePopup?.i === i}
                  {@const templates = snapshot.offload.remote_backend_templates}
                  <div class="template-popup" role="group">
                    {#if templatePopup.mode === 'save'}
                      <div class="template-save">
                        <input
                          type="text"
                          placeholder="Template name"
                          bind:value={newTemplateName}
                          onkeydown={(e) => {
                            if (e.key === 'Enter') commitSaveRemoteTemplate(i);
                            if (e.key === 'Escape') closeTemplatePopup();
                          }}
                        />
                        <button type="button" onclick={() => commitSaveRemoteTemplate(i)}>Save</button>
                        <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
                      </div>
                      {#if templateError}
                        <small class="error">{templateError}</small>
                      {/if}
                      <small class="hint">Saves the base URL and auth token above.</small>
                    {:else if templatePopup.mode === 'load'}
                      {#if templates.length === 0}
                        <small class="hint">No saved endpoints yet.</small>
                      {:else}
                        <ul class="template-list">
                          {#each templates as t (t.name)}
                            <li>
                              <span class="template-name" title={t.base_url}>{t.name}</span>
                              <span class="template-sub">{t.base_url}</span>
                              <button type="button" onclick={() => loadRemoteTemplate(i, t)}>Load</button>
                            </li>
                          {/each}
                        </ul>
                      {/if}
                      <div class="button-row">
                        <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
                      </div>
                    {:else if templatePopup.mode === 'delete'}
                      {#if templates.length === 0}
                        <small class="hint">No saved endpoints yet.</small>
                      {:else}
                        <ul class="template-list">
                          {#each templates as t (t.name)}
                            <li>
                              <span class="template-name" title={t.base_url}>{t.name}</span>
                              <span class="template-sub">{t.base_url}</span>
                              <button type="button" class="danger" onclick={() => deleteRemoteTemplate(t.name)}>Delete</button>
                            </li>
                          {/each}
                        </ul>
                      {/if}
                      <div class="button-row">
                        <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
                      </div>
                    {/if}
                  </div>
                {/if}
                <hr class="card-divider lg" />
                <label class="checkbox">
                  <input
                    type="checkbox"
                    checked={backend.kind.is_cloud}
                    onchange={(e) => setBackendCloud(i, (e.currentTarget as HTMLInputElement).checked)}
                  />
                  <span>Cloud backend (data leaves this machine)</span>
                </label>
                {#if backend.kind.is_cloud}
                  <label class="checkbox cloud-consent">
                    <input
                      type="checkbox"
                      checked={backend.kind.cloud_consent}
                      onchange={(e) =>
                        updateBackend(i, (b) => {
                          if (b.kind.type === 'remote') b.kind.cloud_consent = (e.currentTarget as HTMLInputElement).checked;
                        })}
                    />
                    <span>
                      I understand: offloading to this backend sends the task text
                      (and any tool results scoped in) to a third party. Unusable
                      until checked.
                    </span>
                  </label>
                {/if}
                <label>
                  <span>Declared context (tokens, when /props is absent)</span>
                  <input
                    type="number"
                    min="0"
                    value={backend.declared_context ?? ''}
                    oninput={(e) =>
                      updateBackend(i, (b) => {
                        const v = (e.currentTarget as HTMLInputElement).value;
                        const n = +v;
                        // Empty / non-numeric → null (use /props), never NaN.
                        b.declared_context =
                          v === '' || Number.isNaN(n) ? null : Math.max(0, n);
                      })}
                    placeholder="e.g. 16000"
                  />
                </label>
                <label>
                  <span>Declared model name (when /props is absent)</span>
                  <input
                    type="text"
                    placeholder="e.g. qwen3-32b"
                    value={backend.declared_model}
                    oninput={(e) =>
                      updateBackend(i, (b) => {
                        b.declared_model = (e.currentTarget as HTMLInputElement).value.trim();
                      })}
                  />
                  <small class="hint">
                    Cosmetic label shown for this backend when the endpoint
                    doesn't report its model.
                  </small>
                </label>
              {/if}

              <hr class="card-divider lg" />
              <label>
                <span>Tool scope</span>
                <select
                  value={scopeMode(backend.tool_scope)}
                  onchange={(e) => setScopeMode(i, (e.currentTarget as HTMLSelectElement).value as 'all' | 'web')}
                  disabled={scopeMode(backend.tool_scope) === 'custom'}
                >
                  <option value="all">All tools</option>
                  <option value="web">Web/docs only (deny local files, code, commands, git)</option>
                  {#if scopeMode(backend.tool_scope) === 'custom'}
                    <option value="custom">Custom (edit in settings.json)</option>
                  {/if}
                </select>
                <small class="hint">
                  Cloud backends default to web/docs only so local file contents
                  never leave the machine. Widen a cloud backend only with intent.
                </small>
              </label>

              {#if backend.kind.type === 'local'}
                <hr class="card-divider" />
                <div class="button-row">
                  <button
                    type="button"
                    class="secondary"
                    onclick={() => registerOpencodeProvider(i)}
                  >Add to OpenCode</button>
                  <label class="checkbox inline">
                    <input
                      type="checkbox"
                      checked={snapshot.offload.opencode_provider_auto}
                      onchange={(e) =>
                        patch((s) => {
                          s.offload.opencode_provider_auto = (e.currentTarget as HTMLInputElement).checked;
                        })}
                    />
                    <span>Auto-sync while offload enabled</span>
                  </label>
                </div>
                <small class="hint opencode-desc">
                  Registers this server as OpenCode's <code>local-llama</code>
                  provider (base URL + model read from the command above) and
                  selects it as the default model, so a freshly opened OpenCode
                  tab is ready to work. Overrides any existing
                  <code>local-llama</code>. Auto-sync re-derives it from the
                  primary local backend at launch and on save, but only while the
                  offload server is enabled.
                </small>
                {#if opencodeProviderMsg && opencodeProviderMsg.i === i}
                  <small class={opencodeProviderMsg.ok ? 'hint' : 'error'}>{opencodeProviderMsg.text}</small>
                {/if}
                <div class="button-row offload-lifecycle-row">
                  <button type="button" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendStart(backend.name))}>Start</button>
                  <button type="button" class="secondary" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendStop(backend.name))}>Stop</button>
                  <button type="button" class="secondary" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendRestart(backend.name))}>Reset</button>
                </div>
              {/if}
            </div>
          {/each}

          <div class="button-row">
            <button type="button" onclick={addLocalBackend}>+ Local backend</button>
            <button type="button" onclick={addRemoteBackend}>+ Remote backend</button>
          </div>

          <hr class="card-divider lg" />
          <h3>Limits</h3>
          <label>
            <span>Working-budget high-water (%)</span>
            <input
              type="number"
              min="10"
              max="100"
              value={snapshot.offload.budget_high_water_pct}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.offload.budget_high_water_pct = Math.min(
                      100,
                      Math.max(10, +(e.currentTarget as HTMLInputElement).value || 10),
                    )),
                )}
            />
            <small class="hint">
              Fraction of the per-slot window the loop works against,
              reserving the rest for reasoning + the answer (~80%).
            </small>
          </label>
          <label>
            <span>Per-tool-result token cap</span>
            <input
              type="number"
              min="256"
              value={snapshot.offload.per_tool_result_token_cap}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.offload.per_tool_result_token_cap = Math.max(
                      256,
                      +(e.currentTarget as HTMLInputElement).value || 256,
                    )),
                )}
            />
          </label>
          <label>
            <span>Max steps</span>
            <input
              type="number"
              min="1"
              value={snapshot.offload.max_steps}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.offload.max_steps = Math.max(
                      1,
                      +(e.currentTarget as HTMLInputElement).value || 1,
                    )),
                )}
            />
          </label>
          <label>
            <span>Per-task timeout (seconds)</span>
            <input
              type="number"
              min="30"
              value={snapshot.offload.offload_timeout_secs}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.offload.offload_timeout_secs = Math.max(
                      30,
                      +(e.currentTarget as HTMLInputElement).value || 30,
                    )),
                )}
            />
            <small class="hint">Bounds each offload, including the wait for a free slot.</small>
          </label>
          <label>
            <span>Max queue depth (blank = unlimited)</span>
            <input
              type="number"
              min="0"
              placeholder="unlimited"
              value={snapshot.offload.max_queue_depth ?? ''}
              onchange={(e) => {
                const raw = (e.currentTarget as HTMLInputElement).value.trim();
                const n = Math.floor(+raw);
                patch(
                  (s) =>
                    (s.offload.max_queue_depth =
                      raw === '' || !Number.isFinite(n) || n <= 0 ? null : n),
                );
              }}
            />
            <small class="hint">
              When every slot is busy and this many tasks are already waiting,
              new offloads are rejected immediately instead of queuing. Blank
              keeps the unbounded queue (each waits up to the timeout above).
            </small>
          </label>
          <label>
            <span>Global concurrency (blank = auto)</span>
            <input
              type="number"
              min="1"
              placeholder="auto"
              value={snapshot.offload.global_concurrency ?? ''}
              onchange={(e) => {
                const raw = (e.currentTarget as HTMLInputElement).value.trim();
                const n = Math.floor(+raw);
                patch(
                  (s) =>
                    (s.offload.global_concurrency =
                      raw === '' || !Number.isFinite(n) || n <= 0 ? null : n),
                );
              }}
            />
            <small class="hint">
              Cap on offload tasks in flight across the whole app. Blank
              auto-sizes from the summed per-backend slot counts.
            </small>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.escalate_partial}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.offload.escalate_partial = (e.currentTarget as HTMLInputElement).checked),
                )}
            />
            <span>Escalate partial fast-tier answers to the quality backend</span>
          </label>
          <small class="hint">
            When a fast-tier offload comes back only partially verified, re-run it
            once on a distinct, ready quality backend and keep the better answer.
            Inert unless a second, quality-tier backend is configured.
          </small>
          {:else}
          <h3>Native tools</h3>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.tools.read_file}
              onchange={(e) =>
                patch((s) => (s.offload.tools.read_file = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>read_file — bounded file reads</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.tools.list_dir}
              onchange={(e) =>
                patch((s) => (s.offload.tools.list_dir = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>list_dir — enumerate a directory (what files exist / how many)</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.tools.code_search}
              onchange={(e) =>
                patch((s) => (s.offload.tools.code_search = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>code_search — literal search across the roots</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.tools.run_command}
              onchange={(e) =>
                patch((s) => (s.offload.tools.run_command = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>run_command — allowlisted, read-only commands</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.tools.run_check}
              onchange={(e) =>
                patch((s) => (s.offload.tools.run_check = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span
              >run_check — run a configured project check (build/typecheck/lint/test).
              Inert until the project's <code>checks</code> are configured.</span
            >
          </label>

          <label>
            <span>Allowed roots (one per line)</span>
            <textarea
              rows="3"
              value={snapshot.offload.allowed_roots.join('\n')}
              oninput={(e) =>
                patch(
                  (s) =>
                    (s.offload.allowed_roots = (e.currentTarget as HTMLTextAreaElement).value
                      .split('\n')
                      .map((r) => r.trim())
                      .filter((r) => r.length > 0)),
                )}
              placeholder="Leave empty to confine to the launch project root"
            ></textarea>
            <small class="hint">
              <code>code_search</code>/<code>read_file</code>/<code>run_command</code>
              are confined to these. Empty = the launch project root.
            </small>
          </label>
          <label>
            <span>Command allowlist (comma-separated)</span>
            <input
              type="text"
              value={snapshot.offload.command_allowlist.join(', ')}
              oninput={(e) =>
                patch(
                  (s) =>
                    (s.offload.command_allowlist = (e.currentTarget as HTMLInputElement).value
                      .split(',')
                      .map((c) => c.trim())
                      .filter((c) => c.length > 0)),
                )}
              placeholder="git, cargo"
            />
            <small class="hint">
              <code>run_command</code> runs nothing unless its program is
              listed here (deny by default).
            </small>
          </label>

          <div class="button-row">
            <button type="button" class="secondary" onclick={enableReadonlyCommands}>
              Enable safe read-only commands
            </button>
          </div>
          <small class="hint">
            Adds <code>git</code> and <code>cargo</code> to the allowlist and
            installs a <code>cargo</code> policy that permits only
            <code>metadata</code> / <code>tree</code> (never
            <code>run</code>/<code>build</code>). A one-time merge — it never
            overwrites your own entries, and you can prune anything it adds below.
          </small>

          {#if snapshot.offload.command_allowlist.length > 0}
            <ul class="policy-status">
              {#each snapshot.offload.command_allowlist as prog (prog)}
                {@const pol = policyForProgram(prog)}
                <li>
                  <code>{prog}</code>
                  {#if pol}
                    <span class="hardened">✓ hardened by policy</span>
                  {:else}
                    <span class="unguarded">— no extra guards (allowlist + bare-name only)</span>
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}

          <h3>Command security policies</h3>
          <small class="hint top">
            Per-program hardening layered on top of the allowlist:
            <code>run_command</code> refuses the listed flags/subcommands and
            forces the listed environment variables at spawn. <code>program</code>
            matches an allowlisted command by name (file-stem, case-insensitive).
            The default <code>git</code> policy blocks the config-injection and
            root-escape vectors and neutralizes the pager/ssh hooks. You can edit
            or remove any policy — weakening one can reopen an
            arbitrary-code-execution path, so change with care.
          </small>
          {#each snapshot.offload.command_policies as policy, i (i)}
            <fieldset class="policy-card">
              <div class="policy-head">
                <label class="policy-program">
                  <span>Program</span>
                  <input
                    type="text"
                    value={policy.program}
                    oninput={(e) =>
                      updatePolicy(i, (p) => (p.program = (e.currentTarget as HTMLInputElement).value.trim()))}
                    placeholder="git"
                  />
                </label>
                <button type="button" class="secondary danger" onclick={() => removeCommandPolicy(i)}>
                  Remove
                </button>
              </div>
              <label>
                <span>Denied flags (comma-separated)</span>
                <input
                  type="text"
                  value={policy.denied_flags.join(', ')}
                  oninput={(e) =>
                    updatePolicy(i, (p) => (p.denied_flags = csvToList((e.currentTarget as HTMLInputElement).value)))}
                  placeholder="-c, --git-dir, --work-tree"
                />
              </label>
              <label>
                <span>Denied subcommands (comma-separated)</span>
                <input
                  type="text"
                  value={policy.denied_subcommands.join(', ')}
                  oninput={(e) =>
                    updatePolicy(i, (p) => (p.denied_subcommands = csvToList((e.currentTarget as HTMLInputElement).value)))}
                  placeholder="config"
                />
              </label>
              <label>
                <span>Allowed subcommands (comma-separated)</span>
                <input
                  type="text"
                  value={policy.allowed_subcommands.join(', ')}
                  oninput={(e) =>
                    updatePolicy(i, (p) => (p.allowed_subcommands = csvToList((e.currentTarget as HTMLInputElement).value)))}
                  placeholder="metadata, tree"
                />
                <small class="hint">
                  When set, ONLY these subcommands may run — every other, and a
                  bare invocation, is refused. Leave empty to allow all except
                  the denied ones.
                </small>
              </label>
              <div class="policy-env">
                <span class="policy-env-label">Spawn environment (forced)</span>
                {#each policy.env as ev, j (j)}
                  <div class="env-row">
                    <input
                      type="text"
                      value={ev.key}
                      oninput={(e) =>
                        updatePolicy(i, (p) => (p.env[j].key = (e.currentTarget as HTMLInputElement).value))}
                      placeholder="GIT_PAGER"
                    />
                    <input
                      type="text"
                      value={ev.value}
                      oninput={(e) =>
                        updatePolicy(i, (p) => (p.env[j].value = (e.currentTarget as HTMLInputElement).value))}
                      placeholder="cat"
                    />
                    <button
                      type="button"
                      class="secondary"
                      aria-label="Remove environment variable"
                      onclick={() => updatePolicy(i, (p) => (p.env = p.env.filter((_, idx) => idx !== j)))}
                    >
                      ×
                    </button>
                  </div>
                {/each}
                <div class="button-row">
                  <button
                    type="button"
                    class="secondary"
                    onclick={() => updatePolicy(i, (p) => (p.env = [...p.env, { key: '', value: '' }]))}
                  >
                    Add env var
                  </button>
                </div>
              </div>
            </fieldset>
          {/each}
          <div class="button-row">
            <button type="button" onclick={addCommandPolicy}>Add command policy</button>
          </div>
          {/if}

          <hr class="card-divider lg" />
          <label>
            <span>Test offload</span>
            <input
              type="text"
              bind:value={offloadTestInput}
              placeholder="Leave empty for a canned reachability check, or type a task…"
            />
            <div class="button-row">
              <button type="button" disabled={offloadBusy} onclick={runOffloadTest}>
                Run test
              </button>
            </div>
            {#if offloadTestResult}
              <pre class="offload-test-result">{offloadTestResult}</pre>
            {/if}
          </label>
          <small class="hint">
            Watch the local model load + server logs live in the
            <strong>Tool Activity</strong> tab's <strong>Offload server</strong>
            section.
          </small>
        </section>
      {:else if activeSection === 'mcp'}
        <section>
          <h2>MCP servers</h2>
          <small class="hint top">
            Model Context Protocol servers cImp connects to and keeps warm. Each
            server's read-class tools (web search, fetch, docs, …) can be exposed
            to <strong>Claude Code</strong> directly and/or to the
            <strong>offload worker</strong> — toggle per server below.
            Write/destructive tools are filtered out. Exposing a server to Claude
            Code works whether or not offload is enabled.
          </small>

          <h3>Server status</h3>
          <small class="hint top">
            Live health of the warm MCP host's connections. Updates as you add,
            remove, or enable/disable servers below — no restart needed.
          </small>
          {#if serviceStatus && serviceStatus.mcp_servers.length > 0}
            <ul class="mcp-health">
              {#each serviceStatus.mcp_servers as srv (srv.name)}
                <li class:healthy={srv.healthy} class:down={!srv.healthy}>
                  <span class="mcp-dot" aria-hidden="true"></span>
                  <span class="mcp-name">{srv.name}</span>
                  <span class="mcp-detail">{describeMcpServerHealth(srv)}</span>
                </li>
              {/each}
            </ul>
          {:else if snapshot.offload.mcp_servers.length > 0}
            <small class="hint">
              {snapshot.offload.mcp_servers.length} server(s) configured —
              health appears once the warm MCP host is running (it starts when
              offload is enabled or any server is exposed to Claude Code).
            </small>
          {:else}
            <small class="hint">No MCP servers configured yet.</small>
          {/if}

          <h3>Tool servers</h3>
          <small class="hint top">
            Add an HTTP MCP endpoint by name + URL; changes apply live. cImp's
            warm MCP host aggregates the read-class tools from these servers and
            keeps the connections warm. Advanced stdio servers (command/args/env)
            remain editable in <code>settings.json</code> under
            <code>offload.mcp_servers</code>.
          </small>
          <!-- Keyed by index deliberately: name/url are editable and the
               snapshot is replaced (cloned) on every edit, so a name/url/object
               key would change mid-edit and drop input focus. Inputs are
               controlled (`value={…}`), so values always track the data after a
               removal/reorder, and removal is button-triggered (no focused text
               field to bleed) — the index-key caveat is harmless here. -->
          {#each snapshot.offload.mcp_servers as srv, i (i)}
            <div class="mcp-row">
              <label class="mcp-field">
                <span>Name</span>
                <input
                  type="text"
                  placeholder="duckduckgo"
                  value={srv.name}
                  oninput={(e) =>
                    setMcpServer(i, (m) => (m.name = (e.currentTarget as HTMLInputElement).value.trim()))}
                  onchange={commitMcpEdits}
                />
              </label>
              <label class="mcp-field grow">
                <span>URL</span>
                <input
                  type="text"
                  placeholder="http://host:port/mcp"
                  value={srv.url}
                  oninput={(e) =>
                    setMcpServer(i, (m) => (m.url = (e.currentTarget as HTMLInputElement).value.trim()))}
                  onchange={commitMcpEdits}
                />
              </label>
              <label class="mcp-enable" title="Expose this server's tools to Claude Code">
                <input
                  type="checkbox"
                  checked={srv.claude_access}
                  onchange={(e) =>
                    setMcpAccess(i, 'claude_access', (e.currentTarget as HTMLInputElement).checked)}
                />
                <span>Claude Code</span>
              </label>
              <label class="mcp-enable" title="Expose this server's tools to the offload worker">
                <input
                  type="checkbox"
                  checked={srv.offload_access}
                  onchange={(e) =>
                    setMcpAccess(i, 'offload_access', (e.currentTarget as HTMLInputElement).checked)}
                />
                <span>Offload</span>
              </label>
              <label class="mcp-enable" title="Expose this server's tools to OpenCode">
                <input
                  type="checkbox"
                  checked={srv.opencode_access}
                  onchange={(e) =>
                    setMcpAccess(i, 'opencode_access', (e.currentTarget as HTMLInputElement).checked)}
                />
                <span>OpenCode</span>
              </label>
              <button type="button" class="secondary danger" onclick={() => removeMcpServer(i)}>
                Remove
              </button>
            </div>
          {/each}
          <div class="button-row">
            <button type="button" onclick={addMcpServer}>Add MCP server</button>
          </div>
        </section>
      {:else if activeSection === 'graph'}
        <section>
          <h2>Code Intelligence</h2>
          <small class="hint top">
            Build a per-project graph of your code and docs (symbols, calls,
            imports, doc-comments), stored at
            <code>&lt;project&gt;/.cimp/graph.db</code> and kept live by a file
            watcher. The cloud Claude session queries it through
            <code>graph_*</code> tools (re-launch a tab to pick them up) instead
            of grepping. Off by default; everything stays on this machine.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.graph.enabled}
              onchange={(e) =>
                patch((s) => (s.graph.enabled = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Enable code graph</span>
          </label>

          {#if snapshot.graph.enabled}
            <div class="button-row">
              <button type="button" disabled={graphBusy} onclick={runGraphRebuild}>
                {graphBusy ? 'Rebuilding…' : 'Rebuild index'}
              </button>
              <button type="button" class="secondary" disabled={graphBusy} onclick={refreshGraphStatus}>
                Refresh status
              </button>
            </div>
            {#if graphStatuses.length === 0}
              <small class="hint">No index built yet — click <strong>Rebuild index</strong>.</small>
            {:else}
              {#each graphStatuses as gs (gs.root)}
                <small class="hint">
                  <strong>{gs.state}</strong> · {gs.files} files · {gs.symbols} symbols ·
                  {gs.edges} edges
                  {#if gs.last_error}<br />Error: {gs.last_error}{/if}
                </small>
              {/each}
            {/if}

            <h3>Indexing</h3>
            <label>
              <span>Languages (comma-separated)</span>
              <input
                type="text"
                value={snapshot.graph.languages.join(', ')}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.languages = (e.currentTarget as HTMLInputElement).value
                        .split(',')
                        .map((x) => x.trim().toLowerCase())
                        .filter((x) => x.length > 0)),
                  )}
              />
            </label>
            <small class="hint">
              Full symbol + call graph: <code>rust</code>, <code>typescript</code>,
              <code>javascript</code>, <code>python</code>, <code>go</code>,
              <code>java</code>, <code>c</code>, <code>cpp</code>, <code>csharp</code>,
              <code>php</code>, <code>bash</code>, <code>scala</code>, <code>ocaml</code>,
              <code>ruby</code>, <code>haskell</code>, <code>kotlin</code>,
              <code>swift</code>, <code>sql</code>, <code>erlang</code>, <code>r</code>,
              <code>perl</code>, <code>ada</code>. Docs: <code>markdown</code>.
              Struct-search only (add to enable): <code>html</code>, <code>css</code>,
              <code>json</code>, <code>yaml</code>, <code>xml</code>, <code>asm</code>.
            </small>
            <label class="checkbox">
              <input
                type="checkbox"
                checked={snapshot.graph.index_docs}
                onchange={(e) =>
                  patch((s) => (s.graph.index_docs = (e.currentTarget as HTMLInputElement).checked))}
              />
              <span>Index Markdown docs + doc-comments (powers doc search)</span>
            </label>
            <label>
              <span>Max file size (bytes)</span>
              <input
                type="number"
                min="1024"
                value={snapshot.graph.max_file_bytes}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.max_file_bytes = Math.max(
                        1024,
                        Number((e.currentTarget as HTMLInputElement).value) || 1048576,
                      )),
                  )}
              />
            </label>
            <label>
              <span>Watcher debounce (ms)</span>
              <input
                type="number"
                min="50"
                value={snapshot.graph.watch_debounce_ms}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.watch_debounce_ms = Math.max(
                        50,
                        Number((e.currentTarget as HTMLInputElement).value) || 300,
                      )),
                  )}
              />
            </label>

            <h3>Ignored files & folders</h3>
            <small class="hint">
              Gitignore-style globs, relative to the project root (e.g.
              <code>/docs/generated/</code>, <code>*.snap</code>,
              <code>!keep-this.md</code>). Applied on top of your
              <code>.gitignore</code>. Changes take effect immediately: newly
              ignored files are dropped from the index, un-ignored ones are
              indexed.
            </small>
            <ArrayEditor
              bind:items={snapshot.graph.ignore}
              placeholder="e.g. /vendor/ or *.gen.ts"
              oncommit={commitGraphIgnore}
            />
            <div class="button-row">
              <button type="button" class="secondary" onclick={() => addGraphIgnorePick(false)}>
                Add file…
              </button>
              <button type="button" class="secondary" onclick={() => addGraphIgnorePick(true)}>
                Add folder…
              </button>
            </div>

            <h3>Semantic search</h3>
            <label class="checkbox">
              <input
                type="checkbox"
                checked={snapshot.graph.semantic_search}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.semantic_search = (e.currentTarget as HTMLInputElement).checked),
                  )}
              />
              <span>Enable semantic (embedding) doc search</span>
            </label>
            <small class="hint">
              Needs an OpenAI-compatible <code>/v1/embeddings</code> endpoint
              (e.g. a <code>llama-server --embedding</code> on a spare GPU box).
              Degrades to full-text search when the endpoint is unreachable; the
              structural graph never depends on it.
            </small>
            {#if snapshot.graph.semantic_search}
              <label>
                <span>Embedding endpoint</span>
                <input
                  type="text"
                  placeholder="http://host:8081"
                  value={snapshot.graph.embedding_endpoint}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.embedding_endpoint = (
                          e.currentTarget as HTMLInputElement
                        ).value.trim()),
                    )}
                />
              </label>
              <label>
                <span>Embedding model</span>
                <input
                  type="text"
                  placeholder="nomic-embed-text"
                  value={snapshot.graph.embedding_model}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.embedding_model = (
                          e.currentTarget as HTMLInputElement
                        ).value.trim()),
                    )}
                />
              </label>
              <label>
                <span>Embedding dimensions (0 = auto-probe)</span>
                <input
                  type="number"
                  min="0"
                  value={snapshot.graph.embedding_dims}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.embedding_dims = Math.max(
                          0,
                          Number((e.currentTarget as HTMLInputElement).value) || 0,
                        )),
                    )}
                />
              </label>
              <small class="hint">
                Changing the model or dimensions starts a background re-embed.
                Use <strong>Rebuild embeddings</strong> in Tool Activity →
                Graph index after a silent model swap behind the same name.
              </small>
            {/if}

            <h3>Context injection</h3>
            <label class="checkbox">
              <input
                type="checkbox"
                checked={snapshot.graph.context_injection}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.context_injection = (e.currentTarget as HTMLInputElement).checked),
                  )}
              />
              <span>Auto-inject relevant file digests into each prompt</span>
            </label>
            <small class="hint">
              Prepends a budget-bounded digest of the most relevant files to each
              prompt (Claude via a <code>UserPromptSubmit</code> hook, OpenCode via
              a generated <code>.opencode/plugin</code>). Off by default — it
              changes what the agent sees. Re-launch a tab to pick it up. Tune and
              preview it on the <strong>Context</strong> section of the Code
              Intelligence tab.
            </small>
            {#if snapshot.graph.context_injection}
              <label>
                <span>Per-file budget (chars)</span>
                <input
                  type="number"
                  min="100"
                  value={snapshot.graph.context_per_file_chars}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.context_per_file_chars = Math.max(
                          100,
                          Number((e.currentTarget as HTMLInputElement).value) || 800,
                        )),
                    )}
                />
              </label>
              <label>
                <span>Per-turn budget (chars)</span>
                <input
                  type="number"
                  min="500"
                  value={snapshot.graph.context_turn_budget_chars}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.context_turn_budget_chars = Math.max(
                          500,
                          Number((e.currentTarget as HTMLInputElement).value) || 6000,
                        )),
                    )}
                />
              </label>
              <label>
                <span>Min relevance score (skip below)</span>
                <input
                  type="number"
                  min="0"
                  value={snapshot.graph.context_min_score}
                  onchange={(e) =>
                    patch((s) => {
                      // 0 is a valid value (no threshold), so keep it — a bare
                      // `|| 3` would treat the falsy 0 as "unset" and revert it.
                      const n = Number((e.currentTarget as HTMLInputElement).value);
                      s.graph.context_min_score = Number.isFinite(n) ? Math.max(0, n) : 3;
                    })}
                />
              </label>
              <label class="checkbox">
                <input
                  type="checkbox"
                  checked={snapshot.graph.context_include_session}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.context_include_session = (
                          e.currentTarget as HTMLInputElement
                        ).checked),
                    )}
                />
                <span>Rank session-hot files first (from Memory)</span>
              </label>
              <label>
                <span>Dedup TTL (turns, 0 = re-inject every turn)</span>
                <input
                  type="number"
                  min="0"
                  value={snapshot.graph.context_dedup_ttl_turns}
                  onchange={(e) =>
                    patch((s) => {
                      // 0 is a valid value (dedup off), so keep it — a bare
                      // `|| 10` would treat the falsy 0 as "unset" and revert it.
                      const n = Number((e.currentTarget as HTMLInputElement).value);
                      s.graph.context_dedup_ttl_turns = Number.isFinite(n) ? Math.max(0, n) : 10;
                    })}
                />
              </label>
              <small class="hint">
                A file injected in full is demoted to a one-line "unchanged"
                reminder on later turns until it changes or this many turns pass.
              </small>
              <label class="checkbox">
                <input
                  type="checkbox"
                  checked={snapshot.graph.repo_map_on_session_start}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.repo_map_on_session_start = (
                          e.currentTarget as HTMLInputElement
                        ).checked),
                    )}
                />
                <span>Prepend the project map to each new session's first turn</span>
              </label>
              <label class="checkbox">
                <input
                  type="checkbox"
                  checked={snapshot.graph.compaction_context}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.compaction_context = (
                          e.currentTarget as HTMLInputElement
                        ).checked),
                    )}
                />
                <span>Feed working set + pinned notes to Claude's compactor</span>
              </label>
              <small class="hint">
                On compaction (<code>PreCompact</code> hook) the session's working
                set and pinned notes are handed to the summarizer so they survive.
                Costs a few hundred chars once per compaction. Re-launch the tab to
                pick up changes.
              </small>
            {/if}

            <h3>Token efficiency</h3>
            <label class="checkbox">
              <input
                type="checkbox"
                checked={snapshot.graph.read_advisor}
                disabled={e1Blocked}
                onchange={(e) =>
                  patch(
                    (s) => (s.graph.read_advisor = (e.currentTarget as HTMLInputElement).checked),
                  )}
              />
              <span>Redundant-read advisor (Claude tabs)</span>
            </label>
            {#if e1Blocked}
              <small class="hint">
                Blocked: the E1 contract check recorded that Claude Code does
                <strong>not</strong> surface a deny reason to the model on this
                version — every reminder would be a bare refusal, worse than no
                advisor. The hook is not installed regardless of this toggle.
                Re-run the check in <code>MAINTENANCE.md</code> → harness
                contracts after the next Claude Code update.
              </small>
            {:else}
              <small class="hint">
                Intercepts a <code>Read</code> of a file already read unchanged this
                session and answers with a cheap outline reminder instead of
                re-reading it. Changes the agent's tool behaviour — strictly opt-in.
                Claude tabs only for now. Re-launch the tab to pick it up.
              </small>
            {/if}
            {#if snapshot.graph.read_advisor && !e1Blocked}
              <label>
                <span>Min file size to advise (lines)</span>
                <input
                  type="number"
                  min="0"
                  value={snapshot.graph.read_advisor_min_lines}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.read_advisor_min_lines = Math.max(
                          0,
                          Number((e.currentTarget as HTMLInputElement).value) || 300,
                        )),
                    )}
                />
              </label>
              <small class="hint">
                Files with fewer lines than this always pass — a small file is
                cheap to re-read; the reminder isn't worth it.
              </small>
              <label>
                <span>Reminder mode</span>
                <select
                  value={snapshot.graph.read_advisor_mode}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.read_advisor_mode = (
                          e.currentTarget as HTMLSelectElement
                        ).value),
                    )}
                >
                  <option value="advise">Advise — outline reminder only</option>
                  <option value="substitute">Substitute — outline + most relevant symbol body</option>
                </select>
              </label>
              <label>
                <span>Trust TTL (retrieve turns, 0 = whole session)</span>
                <input
                  type="number"
                  min="0"
                  value={snapshot.graph.read_advisor_ttl_turns}
                  onchange={(e) =>
                    patch((s) => {
                      // 0 is a valid value (TTL off), so keep it — a bare
                      // `|| 0` happens to coincide here, but stay explicit.
                      const n = Number((e.currentTarget as HTMLInputElement).value);
                      s.graph.read_advisor_ttl_turns = Number.isFinite(n) ? Math.max(0, n) : 0;
                    })}
                />
              </label>
              <small class="hint">
                After this many retrieval turns since the advisor last saw the
                file read in full, a <code>Read</code> passes again — bounds how
                long the agent's memory is trusted across context loss the
                advisor can't observe (context editing, tool-result truncation).
              </small>
              <label class="checkbox">
                <input
                  type="checkbox"
                  checked={snapshot.graph.read_advisor_diffs}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.read_advisor_diffs = (
                          e.currentTarget as HTMLInputElement
                        ).checked),
                    )}
                />
                <span>Diff-substitute changed-file re-reads</span>
              </label>
              <small class="hint">
                When you re-read a file <em>after it changed</em>, answer with a
                line-level unified diff against what you last read instead of the
                whole file — exact, so it's safe on the edit-then-verify loop.
                Falls back to a normal read when no snapshot survives or the diff
                would be more than half the new file.
              </small>
              <label class="checkbox">
                <input
                  type="checkbox"
                  checked={snapshot.graph.read_advisor_shell}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.read_advisor_shell = (
                          e.currentTarget as HTMLInputElement
                        ).checked),
                    )}
                />
                <span>Intercept whole-file shell reads</span>
              </label>
              <small class="hint">
                Also advise on a whole-file shell read
                (<code>cat</code>, <code>Get-Content</code>, <code>type</code>,
                <code>gc</code>) of an already-read file, the same as a
                <code>Read</code>. Strict — only a provable whole-file read of one
                file is intercepted; anything with a pipe, redirect, glob, second
                path, or a partial-read verb (<code>sed</code>, <code>head</code>)
                runs untouched.
              </small>
              <label>
                <span>First-read digest tier (KiB, 0 = off)</span>
                <input
                  type="number"
                  min="0"
                  value={snapshot.graph.read_advisor_first_read_kb}
                  onchange={(e) =>
                    patch((s) => {
                      const n = Number((e.currentTarget as HTMLInputElement).value);
                      s.graph.read_advisor_first_read_kb = Number.isFinite(n)
                        ? Math.max(0, Math.trunc(n))
                        : 0;
                    })}
                />
              </label>
              <small class="hint">
                Answer the <em>first</em> read of a large non-code file (log,
                lockfile, generated JSON, data dump) at or above this size with the
                cached local-model digest plus a head/tail sample instead of the
                full content. Source files (anything with a parsed outline) never
                qualify, and a sliced <code>Read</code> always passes. Needs a
                cached digest — the first encounter enqueues one and passes, so
                protection begins on the next. Off by default; try <code>256</code>.
              </small>
            {/if}
            <label class="checkbox">
              <input
                type="checkbox"
                checked={snapshot.graph.context_llm_digests}
                disabled={!snapshot.graph.context_llm_digests && !localOffloadReady}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.context_llm_digests = (
                        e.currentTarget as HTMLInputElement
                      ).checked),
                  )}
              />
              <span>Local-model digests for outline-poor files</span>
            </label>
            <small class="hint">
              For files with no useful outline (docs, configs, long scripts), the
              <strong>local</strong> offload backend writes a 3-line semantic
              digest, cached in <code>graph.db</code>. Needs a ready local offload
              backend; never leaves this machine.
              {#if !localOffloadReady}
                <strong>No local offload backend is ready</strong> — start one in
                Settings → Offload to enable this.
              {/if}
            </small>

            <h3>Tool surface</h3>
            <label class="checkbox">
              <input
                type="checkbox"
                checked={snapshot.graph.lean_tools}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.lean_tools = (
                        e.currentTarget as HTMLInputElement
                      ).checked),
                  )}
              />
              <span>Lean tool surface (hide cold-tail graph tools)</span>
            </label>
            <small class="hint">
              Drop <code>graph_cycles</code>, <code>graph_dead_exports</code>,
              <code>graph_struct_search</code>, <code>graph_path</code>, and
              <code>graph_architecture</code> from the tool list advertised to the
              cloud session and the offload worker — trimming the descriptors
              cache-written once per session. Advertisement-only: each hidden tool
              still answers if an agent calls it by name. The Code Intelligence tab
              shows the current surface size.
            </small>

            <h3>Architecture &amp; path tracing</h3>
            <small class="hint">
              Tune V15's code-intelligence features: <code>graph_path</code>
              (shortest-path tracing), <code>graph_architecture</code> (god
              nodes, subsystems, surprising edges), and the live Graph view
              (Tool Activity tab).
              Edge confidence (extracted/inferred/ambiguous) is always on.
            </small>
            <label>
              <span>Path tracing max hops (1–32)</span>
              <input
                type="number"
                min="1"
                max="32"
                value={snapshot.graph.path_max_hops}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.path_max_hops = Math.min(
                        32,
                        Math.max(1, Number((e.currentTarget as HTMLInputElement).value) || 8),
                      )),
                  )}
              />
            </label>
            <label>
              <span>Max subsystems reported</span>
              <input
                type="number"
                min="1"
                value={snapshot.graph.arch_max_communities}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.arch_max_communities = Math.max(
                        1,
                        Number((e.currentTarget as HTMLInputElement).value) || 12,
                      )),
                  )}
              />
            </label>
            <label>
              <span>Minimum subsystem size</span>
              <input
                type="number"
                min="1"
                value={snapshot.graph.arch_min_community_size}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.arch_min_community_size = Math.max(
                        1,
                        Number((e.currentTarget as HTMLInputElement).value) || 3,
                      )),
                  )}
              />
            </label>
            <label class="checkbox">
              <input
                type="checkbox"
                checked={snapshot.graph.graph_viz}
                onchange={(e) =>
                  patch((s) => (s.graph.graph_viz = (e.currentTarget as HTMLInputElement).checked))}
              />
              <span>Enable the <strong>Graph view</strong> (live 3D force graph)</span>
            </label>
            <small class="hint">
              Draws the code graph and pulses nodes as agents read/edit/query
              the codebase, in the Tool Activity tab's "Graph view" section.
              Off by default — it's a human-facing visual, not on any agent
              path.
            </small>
            {#if snapshot.graph.graph_viz}
              <label>
                <span>Max rendered nodes</span>
                <input
                  type="number"
                  min="50"
                  value={snapshot.graph.graph_viz_max_nodes}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.graph_viz_max_nodes = Math.max(
                          50,
                          Number((e.currentTarget as HTMLInputElement).value) || 1500,
                        )),
                    )}
                />
              </label>
              <h3>Graph view tuning</h3>
              <small class="hint">
                Multipliers on the built-in layout/appearance (1.0 = default;
                0.2–5, folder spacing up to 50). One size doesn't fit every
                repo — a dense monorepo usually wants smaller nodes and wider
                spacing than a small project. Changes apply live to an open
                Graph view.
              </small>
              {#each [
                { key: 'graph_viz_node_scale', label: 'File node size', max: 5 },
                { key: 'graph_viz_dir_scale', label: 'Folder cluster size', max: 5 },
                { key: 'graph_viz_edge_width', label: 'Edge line width', max: 5 },
                { key: 'graph_viz_node_spacing', label: 'Spacing between files', max: 5 },
                { key: 'graph_viz_cluster_spacing', label: 'Spacing between folders', max: 50 },
                { key: 'graph_viz_cluster_strength', label: 'Folder grouping tightness', max: 5 },
              ] as knob (knob.key)}
                <label>
                  <span>{knob.label}</span>
                  <input
                    type="number"
                    min="0.2"
                    max={knob.max}
                    step="0.1"
                    value={(snapshot.graph as unknown as Record<string, number>)[knob.key]}
                    onchange={(e) =>
                      patch(
                        (s) =>
                          ((s.graph as unknown as Record<string, number>)[knob.key] = Math.min(
                            knob.max,
                            Math.max(0.2, Number((e.currentTarget as HTMLInputElement).value) || 1),
                          )),
                      )}
                  />
                </label>
              {/each}
              <div class="row">
                <label>
                  <span>Call edge color</span>
                  <input
                    type="color"
                    value={snapshot.graph.graph_viz_color_call}
                    onchange={(e) =>
                      patch(
                        (s) => (s.graph.graph_viz_color_call = (e.currentTarget as HTMLInputElement).value),
                      )}
                  />
                </label>
                <label>
                  <span>Import edge color</span>
                  <input
                    type="color"
                    value={snapshot.graph.graph_viz_color_import}
                    onchange={(e) =>
                      patch(
                        (s) => (s.graph.graph_viz_color_import = (e.currentTarget as HTMLInputElement).value),
                      )}
                  />
                </label>
              </div>
            {/if}

            <h3>Offload worker access</h3>
            <label class="checkbox">
              <input
                type="checkbox"
                checked={snapshot.graph.allow_remote_worker_access}
                onchange={(e) =>
                  patch(
                    (s) =>
                      (s.graph.allow_remote_worker_access = (
                        e.currentTarget as HTMLInputElement
                      ).checked),
                  )}
              />
              <span>Allow a <strong>remote</strong> offload worker to query the graph</span>
            </label>
            <small class="hint">
              ⚠ <strong>Privacy:</strong> the local offload worker can always
              query the graph. A <strong>remote</strong> backend — whether a box
              on your LAN or a public cloud API — would receive your project's
              code structure (symbol names, call relationships, doc snippets).
              Leave this off unless you trust the remote. The cloud Claude
              session's <code>graph_*</code> tools are unaffected by this
              setting.
            </small>
          {/if}
        </section>
      {:else if activeSection === 'checks'}
        <section>
          <h2>Checks</h2>
          <small class="hint">
            Project checker commands the <code>run_check</code> tool exposes to
            Claude and the offload worker — a build, typecheck, lint, or test run
            turned into bounded, deduplicated diagnostics instead of a raw dump.
            Configured per project; changes land in this project's
            <code>.cimp/config.json</code> overlay.
          </small>
          <ChecksEditor
            checks={snapshot.checks}
            onchange={(next) => patch((s) => (s.checks = next))}
          />
        </section>
      {:else if activeSection === 'code-audit'}
        <section>
          <h2>Code Audit</h2>
          <small class="hint top">
            Aggregated security scanning. cImp runs external scanners against the
            project root and merges their findings into one table. Nothing is
            bundled — each tool resolves from the <code>ebin\</code> drop-in
            folder first, then your PATH; point cImp at a specific build with
            Browse, or check availability with Detect. Enable the feature to show
            the Code audit section in the Tool Activity tab.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.code_audit.enabled}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.code_audit.enabled = (
                      e.currentTarget as HTMLInputElement
                    ).checked),
                )}
            />
            <span>Enable Code Audit (Tool Activity → Code audit)</span>
          </label>

          {#snippet auditToolRow(row: AuditToolRow)}
            {@const det = auditDetect[row.meta.id]}
            {@const disp = formatDetect(det)}
            <div class="audit-tool">
              <label class="checkbox">
                <input
                  type="checkbox"
                  checked={row.tool.enabled}
                  onchange={(e) =>
                    toggleAuditToolEnabled(
                      row.meta.id,
                      (e.currentTarget as HTMLInputElement).checked,
                    )}
                />
                <span class="audit-name">{row.meta.name}</span>
                <span class="audit-role">{row.meta.role}</span>
              </label>
              {#if toolNotApplicable(row.meta.id, auditCensus)}
                <small class="hint audit-na">
                  not applicable to the current project
                </small>
              {/if}
              <div class="input-with-action">
                <input
                  type="text"
                  placeholder="(use ebin / PATH)"
                  value={row.tool.path}
                  oninput={(e) =>
                    patchAuditTool(
                      row.meta.id,
                      (t) =>
                        (t.path = (e.currentTarget as HTMLInputElement).value),
                    )}
                />
                <button
                  type="button"
                  class="secondary"
                  onclick={() => void detectAuditTool(row.meta.id)}
                >
                  Detect
                </button>
                <button
                  type="button"
                  class="secondary"
                  onclick={() => void pickAuditToolExe(row.meta.id)}
                >
                  Browse…
                </button>
                <button
                  type="button"
                  class="secondary"
                  onclick={() => patchAuditTool(row.meta.id, (t) => (t.path = ''))}
                >
                  Clear
                </button>
              </div>
              {#if disp.kind !== 'idle'}
                <small
                  class="hint audit-detect"
                  class:ok={disp.kind === 'found'}
                  class:bad={disp.kind === 'not-found'}
                >
                  {disp.text}
                </small>
              {/if}
              <label class="audit-timeout">
                <span>Timeout override (seconds — blank uses the global)</span>
                <input
                  type="number"
                  min="1"
                  placeholder="(global)"
                  value={row.tool.timeout_secs ?? ''}
                  oninput={(e) =>
                    patchAuditTool(row.meta.id, (t) => {
                      const raw = (e.currentTarget as HTMLInputElement).value.trim();
                      const v = Number(raw);
                      t.timeout_secs =
                        raw !== '' && Number.isFinite(v) && v >= 1 ? Math.floor(v) : null;
                    })}
                />
              </label>
              <small class="hint">
                Extra arguments (appended after the tool's fixed argv):
              </small>
              <ArrayEditor
                bind:items={snapshot!.code_audit.tools[row.index].extra_args}
                placeholder="e.g. --config auto"
                oncommit={commitAudit}
              />
            </div>
          {/snippet}

          <h3>Security tools</h3>
          <small class="hint">
            Shown in the Code audit section's <strong>Security</strong> sub-tab
            (Tool Activity tab).
          </small>
          {#each auditGroups.security as row (row.meta.id)}
            {@render auditToolRow(row)}
          {/each}

          <h3>Quality tools</h3>
          <small class="hint">
            Shown in the Code audit section's <strong>Quality</strong> sub-tab
            (Tool Activity tab).
            Language-gated — a tool only appears there (and only runs) when the
            project contains files it applies to. All tools are listed here
            regardless of the current project.
          </small>
          {#if snapshot.code_audit.quality_auto_select}
            <small class="hint audit-auto-note">
              Selection: <strong>automatic</strong> — follows the project's
              languages (heavyweight opt-ins stay off); editing a checkbox
              switches to manual.
            </small>
          {:else}
            <div class="audit-auto-row">
              <button type="button" class="secondary" onclick={applyQualityAutoSelect}>
                Auto-select for this project
              </button>
              <small class="hint">
                re-select the tools matching the project's languages and keep
                them in sync automatically
              </small>
            </div>
          {/if}
          {#each auditGroups.quality as row (row.meta.id)}
            {@render auditToolRow(row)}
          {/each}

          <h3>Scan settings</h3>
          <label>
            <span>Per-tool timeout (seconds)</span>
            <input
              type="number"
              min="1"
              value={snapshot.code_audit.timeout_secs}
              oninput={(e) =>
                patch((s) => {
                  const v = Number((e.currentTarget as HTMLInputElement).value);
                  if (Number.isFinite(v) && v >= 1)
                    s.code_audit.timeout_secs = Math.floor(v);
                })}
            />
          </label>

          <h3>MCP exposure</h3>
          <small class="hint">
            Advertise the <code>cimp-code-audit</code> MCP server
            (<code>security_audit</code> / <code>quality_audit</code>, native
            worker tools for offload) so AI consumers can trigger audits
            themselves. Each requires Code Audit enabled above. OpenCode caches
            its tool list at connect — flip a toggle and restart the tab.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.code_audit.expose_claude}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.code_audit.expose_claude = (
                      e.currentTarget as HTMLInputElement
                    ).checked),
                )}
            />
            <span>Expose to Claude Code</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.code_audit.expose_opencode}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.code_audit.expose_opencode = (
                      e.currentTarget as HTMLInputElement
                    ).checked),
                )}
            />
            <span>Expose to OpenCode</span>
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.code_audit.expose_offload}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.code_audit.expose_offload = (
                      e.currentTarget as HTMLInputElement
                    ).checked),
                )}
            />
            <span>Expose to offload worker</span>
          </label>
        </section>
      {:else if activeSection === 'pricing'}
        <section>
          <h2>LLM pricing</h2>
          <small class="hint top">
            Provider/model token prices (USD per <strong>million tokens</strong>,
            "MTok") used by the Code Intelligence tab's session-cost popup and
            its Usage view's <em>est. cost</em> mode (auto-matched by the
            <em>Id prefix</em> column). Fresh installs are seeded with current
            Anthropic API and GitHub Copilot rates — Anthropic cache-write at
            the 1-hour-TTL 2× rate Claude Code sessions actually pay; every
            value is editable, and prices drift, so corrections are yours to
            make (no auto-update). Saved to the global settings file, not this
            project's overlay.
          </small>
          {#if llmPricingLoading}
            <small class="hint">Loading…</small>
          {:else}
            {#if llmPricing.length === 0}
              <small class="hint">No entries — add one below.</small>
            {:else}
              <div class="pricing-head-row">
                <span>Provider</span>
                <span>Model</span>
                <span title="Transcript model-id prefix this row auto-matches in the Usage view's cost mode (e.g. claude-opus-4-8). Longest match wins; empty = manual-pick only.">Id prefix</span>
                <span class="num">Input</span>
                <span class="num">Cache write</span>
                <span class="num">Cache read</span>
                <span class="num">Output</span>
                <span></span>
              </div>
              <!-- Keyed by index deliberately, same as the MCP editor: rows are
                   editable and replaced (cloned) on every edit, so a value-based
                   key would change mid-edit and drop input focus. -->
              {#each llmPricing as row, i (i)}
                <div class="pricing-row">
                  <input
                    type="text"
                    placeholder="Provider"
                    value={row.provider}
                    oninput={(e) =>
                      editLlmPricingText(i, 'provider', (e.currentTarget as HTMLInputElement).value)}
                  />
                  <input
                    type="text"
                    placeholder="Model"
                    value={row.model}
                    oninput={(e) =>
                      editLlmPricingText(i, 'model', (e.currentTarget as HTMLInputElement).value)}
                  />
                  <input
                    type="text"
                    placeholder="e.g. claude-opus-4-8"
                    title="Transcript model-id prefix for cost-mode auto-match (longest wins; empty = manual-pick only)"
                    value={row.model_prefix}
                    oninput={(e) =>
                      editLlmPricingText(i, 'model_prefix', (e.currentTarget as HTMLInputElement).value)}
                  />
                  <input
                    type="number"
                    class="num"
                    min="0"
                    step="0.01"
                    title="$ per MTok, input tokens"
                    value={row.input}
                    onchange={(e) =>
                      editLlmPricingRate(i, 'input', (e.currentTarget as HTMLInputElement).value)}
                  />
                  <input
                    type="number"
                    class="num"
                    min="0"
                    step="0.01"
                    title="$ per MTok, cache-write tokens"
                    value={row.cache_write}
                    onchange={(e) =>
                      editLlmPricingRate(i, 'cache_write', (e.currentTarget as HTMLInputElement).value)}
                  />
                  <input
                    type="number"
                    class="num"
                    min="0"
                    step="0.01"
                    title="$ per MTok, cache-read tokens"
                    value={row.cache_read}
                    onchange={(e) =>
                      editLlmPricingRate(i, 'cache_read', (e.currentTarget as HTMLInputElement).value)}
                  />
                  <input
                    type="number"
                    class="num"
                    min="0"
                    step="0.01"
                    title="$ per MTok, output tokens"
                    value={row.output}
                    onchange={(e) =>
                      editLlmPricingRate(i, 'output', (e.currentTarget as HTMLInputElement).value)}
                  />
                  <button type="button" class="secondary danger" onclick={() => deleteLlmPricingRow(i)}>
                    Delete
                  </button>
                </div>
              {/each}
            {/if}
            <div class="button-row">
              <button type="button" onclick={addLlmPricingRow}>Add model</button>
              <button type="button" disabled={!llmPricingDirty} onclick={() => void saveLlmPricing()}>
                Save
              </button>
              {#if llmPricingDirty}
                <small class="hint">Unsaved changes</small>
              {/if}
            </div>
            {#if llmPricingError}
              <small class="error">{llmPricingError}</small>
            {/if}
          {/if}
        </section>
      {:else if activeSection === 'workbench'}
        <section>
          <h2>Workbench</h2>
          <small class="hint top">
            Vibe-coding guardrails: a live diff pane, automatic checkpoints
            (a separate shadow git repo — your own <code>.git</code> is never
            touched), and a worktree manager for running parallel agents
            safely. The tab is cheap to keep around; checkpoints are a
            heavier, opt-in feature below.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.workbench.enabled}
              onchange={(e) =>
                patch(
                  (s) => (s.workbench.enabled = (e.currentTarget as HTMLInputElement).checked),
                )}
            />
            <span>Show the Workbench tab</span>
          </label>

          <h3>Checkpoints</h3>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.workbench.checkpoints}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.workbench.checkpoints = (e.currentTarget as HTMLInputElement).checked),
                )}
            />
            <span>Enable automatic checkpoints</span>
          </label>
          <small class="hint">
            Off by default in V1 — Diff and Worktrees work without it. When
            on, cImp periodically snapshots your working tree into a separate
            shadow git repo (your own <code>.git</code> is never touched).
            Enable this to start capturing checkpoints; restore one from the
            Workbench tab's Timeline section.
          </small>
          <label>
            <span>Max checkpoints kept</span>
            <input
              type="number"
              min="1"
              disabled={!snapshot.workbench.checkpoints}
              value={snapshot.workbench.checkpoint_max}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.workbench.checkpoint_max = Math.max(
                      1,
                      Number((e.currentTarget as HTMLInputElement).value) || 100,
                    )),
                )}
            />
          </label>
          <label>
            <span>Max checkpoint age (days)</span>
            <input
              type="number"
              min="1"
              disabled={!snapshot.workbench.checkpoints}
              value={snapshot.workbench.checkpoint_max_age_days}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.workbench.checkpoint_max_age_days = Math.max(
                      1,
                      Number((e.currentTarget as HTMLInputElement).value) || 7,
                    )),
                )}
            />
          </label>
          <small class="hint">
            The burst trigger fires an "activity" checkpoint when a shell tab
            or other non-hooked flow touches several files at once — the
            fallback that covers what the per-prompt trigger can't see.
          </small>
          <label>
            <span>Burst trigger: files changed</span>
            <input
              type="number"
              min="1"
              disabled={!snapshot.workbench.checkpoints}
              value={snapshot.workbench.checkpoint_burst_files}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.workbench.checkpoint_burst_files = Math.max(
                      1,
                      Number((e.currentTarget as HTMLInputElement).value) || 5,
                    )),
                )}
            />
          </label>
          <label>
            <span>Burst trigger: time window (seconds)</span>
            <input
              type="number"
              min="1"
              disabled={!snapshot.workbench.checkpoints}
              value={snapshot.workbench.checkpoint_burst_window_s}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.workbench.checkpoint_burst_window_s = Math.max(
                      1,
                      Number((e.currentTarget as HTMLInputElement).value) || 60,
                    )),
                )}
            />
          </label>
          <label>
            <span>Minimum gap between snapshots (seconds)</span>
            <input
              type="number"
              min="1"
              disabled={!snapshot.workbench.checkpoints}
              value={snapshot.workbench.checkpoint_min_gap_s}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.workbench.checkpoint_min_gap_s = Math.max(
                      1,
                      Number((e.currentTarget as HTMLInputElement).value) || 120,
                    )),
                )}
            />
          </label>
        </section>
      {:else if activeSection === 'advanced'}
        <section>
          <h2>Logging</h2>
          <small class="hint top">
            Log files roll daily into <code>logs/</code> next to the cImp
            executable. Changing the level applies live; the
            <code>RUST_LOG</code> env var, when set at launch, overrides
            this until you change it here.
          </small>
          <label>
            <span>Log level</span>
            <select
              value={snapshot.logging.level}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.logging.level = (
                      e.currentTarget as HTMLSelectElement
                    ).value as Settings['logging']['level']),
                )}
            >
              <option value="trace">Trace</option>
              <option value="debug">Debug</option>
              <option value="info">Info</option>
              <option value="warn">Warn</option>
              <option value="error">Error</option>
            </select>
          </label>
          <label>
            <span>Retention</span>
            <select
              value={snapshot.logging.retention}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.logging.retention = (
                      e.currentTarget as HTMLSelectElement
                    ).value as Settings['logging']['retention']),
                )}
            >
              <option value="daily">Daily (keep 1 day)</option>
              <option value="weekly">Weekly (keep 7 days)</option>
              <option value="monthly">Monthly (keep 30 days)</option>
              <option value="never">Never (keep everything)</option>
            </select>
            <small class="hint">
              Cleanup runs at launch and whenever this setting changes.
              Files older than the window are deleted; the active day's log
              is always kept.
            </small>
          </label>

          <h3>Content capture</h3>
          <small class="hint top">
            When on, raw PTY output for every Claude / shell tab is also
            written to <code>logs/content/&lt;tab-id&gt;.log.&lt;date&gt;</code>,
            rotated daily. Output includes ANSI escape codes — pipe through
            <code>sed</code> or a viewer if you want plain text.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.logging.content_capture.enabled}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.logging.content_capture.enabled = (
                      e.currentTarget as HTMLInputElement
                    ).checked),
                )}
            />
            <span>Capture full tab output</span>
          </label>
          <label>
            <span>Retention</span>
            <select
              value={snapshot.logging.content_capture.retention}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.logging.content_capture.retention = (
                      e.currentTarget as HTMLSelectElement
                    ).value as Settings['logging']['content_capture']['retention']),
                )}
            >
              <option value="daily">Daily (keep 1 day)</option>
              <option value="weekly">Weekly (keep 7 days)</option>
              <option value="monthly">Monthly (keep 30 days)</option>
              <option value="never">Never (keep everything)</option>
            </select>
          </label>
          <div class="content-actions">
            <button
              type="button"
              onclick={() =>
                contentOpenFolder().catch((e) =>
                  console.error('content_open_folder failed:', e),
                )}
            >
              Open folder
            </button>
            <button
              type="button"
              onclick={async () => {
                if (
                  !confirm(
                    'Delete every file inside the content folder? This cannot be undone.',
                  )
                )
                  return;
                try {
                  await contentClear();
                } catch (e) {
                  console.error('content_clear failed:', e);
                }
              }}
            >
              Delete all files
            </button>
          </div>
        </section>

        <section>
          <h2>Terminal scrollback</h2>
          <small class="hint top">
            Each tab's PTY output is kept in an in-memory ring buffer so
            re-opened panes and restarts can replay history.
          </small>
          <label>
            <span>Ring buffer size (bytes per tab)</span>
            <input
              type="number"
              min="4096"
              value={snapshot.terminal.scrollback.ring_bytes}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.terminal.scrollback.ring_bytes = Math.max(
                      4096,
                      Number((e.currentTarget as HTMLInputElement).value) || 262144,
                    )),
                )}
            />
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.terminal.scrollback.persist}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.terminal.scrollback.persist = (
                      e.currentTarget as HTMLInputElement
                    ).checked),
                )}
            />
            <span>Save scrollback to disk on exit</span>
          </label>
          <small class="hint">
            On graceful exit each tab's ring is written to
            <code>scrollback/&lt;tab-id&gt;.bin</code> in the config
            directory. Terminal output can contain sensitive text — leave
            off if that shouldn't touch disk.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.terminal.scrollback.restore_on_launch}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.terminal.scrollback.restore_on_launch = (
                      e.currentTarget as HTMLInputElement
                    ).checked),
                )}
            />
            <span>Restore saved scrollback on launch</span>
          </label>
          <small class="hint">
            Replays the persisted bytes into each tab before live output
            resumes on the next launch.
          </small>
        </section>

        <section>
          <h2>Reset</h2>
          <small class="hint top">
            Replace every setting with its factory default. Wipes
            user-created shell tabs, saved layouts, shortcut overrides,
            and all theme / background overrides. Cannot be undone.
          </small>
          <button
            type="button"
            class="danger"
            onclick={resetSettingsToDefaults}
          >
            Reset all settings to defaults
          </button>
        </section>
      {:else if activeSection === 'about'}
        <section class="about-section">
          <h2>About</h2>
          <dl class="about-list">
            <dt>Author</dt>
            <dd>Amir Amashe</dd>

            <dt>Version</dt>
            <dd><code>{appVersion}</code></dd>

            <dt>Repository</dt>
            <dd>
              <a href={REPO_URL} target="_blank" rel="noopener noreferrer">
                {REPO_URL}
              </a>
            </dd>
          </dl>
        </section>
      {/if}
      </div>
    </div>
  </div>
{/if}

<style>
  :global(html, body) {
    background: var(--surface-sunken);
    color: var(--text-primary);
    font-family: system-ui, -apple-system, sans-serif;
    font-size: var(--font-size-md);
  }
  /* V8-01 offload controls */
  .button-row {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.4rem;
    flex-wrap: wrap;
  }
  /* Offload local-backend card: 1 blank line between the OpenCode buttons
     (Add to OpenCode / Auto-sync) and their description. Overrides the
     default `small.hint` negative top margin. */
  small.hint.opencode-desc {
    margin-top: 1.5rem;
  }
  /* …and 2 blank lines between that description and the Start/Stop/Reset
     lifecycle row, which now sits at the bottom of the card. Overrides the
     default `.button-row` top margin. */
  .button-row.offload-lifecycle-row {
    margin-top: 3rem;
  }
  .offload-status {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    margin: 0.6rem 0 0.2rem;
  }
  .offload-status-label {
    font-weight: 600;
    color: var(--text-secondary);
  }
  /* Per-MCP-server health readout */
  .status-error {
    color: var(--danger, #d08770);
    font-weight: 600;
  }
  .mcp-health {
    list-style: none;
    margin: 0.4rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .mcp-health li {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    font-size: var(--font-size-sm);
  }
  .mcp-dot {
    flex: 0 0 auto;
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 50%;
    background: var(--text-secondary);
    align-self: center;
  }
  .mcp-health li.healthy .mcp-dot {
    background: var(--success, #3fb950);
  }
  .mcp-health li.down .mcp-dot {
    background: var(--danger, #d08770);
  }
  .mcp-name {
    font-weight: 600;
    min-width: 6rem;
  }
  .mcp-detail {
    color: var(--text-secondary);
  }
  /* Editable MCP server rows (name + url + enable + remove). */
  .mcp-row {
    display: flex;
    align-items: flex-end;
    gap: 0.5rem;
    margin-top: 0.4rem;
    flex-wrap: wrap;
  }
  /* LLM pricing editor: shared column template so the header row and every
     data row line up as a table. Provider/model get the flexible tracks; the
     four $/MTok fields are fixed-width numerics. */
  .pricing-head-row,
  .pricing-row {
    display: grid;
    /* V16 Feature 8 added the Id-prefix column between Model and Input. */
    grid-template-columns: minmax(6rem, 0.7fr) minmax(8rem, 1fr) minmax(7rem, 0.9fr) 5.5rem 5.5rem 5.5rem 5.5rem auto;
    gap: 0.4rem;
    align-items: center;
    margin-top: 0.4rem;
  }
  .pricing-head-row {
    font-size: var(--font-size-sm);
    color: var(--text-subtle, #999);
    margin-top: 0.8rem;
  }
  .pricing-head-row .num,
  .pricing-row input.num {
    text-align: right;
  }
  .mcp-field {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .mcp-field.grow {
    flex: 1 1 16rem;
  }
  .mcp-field input {
    width: 100%;
  }
  .mcp-enable {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    white-space: nowrap;
    padding-bottom: 0.35rem;
  }
  .mcp-enable input {
    width: auto;
  }
  .offload-test-result {
    margin-top: 0.5rem;
    max-height: 16rem;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
    background: var(--surface-sunken);
    border: 1px solid var(--border-subtle);
    border-radius: 4px;
    padding: 0.5rem;
    font-size: var(--font-size-sm);
  }
  /* Command security policies (Tools sub-tab) */
  .policy-status {
    list-style: none;
    margin: 0.25rem 0 0.75rem;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    font-size: var(--font-size-sm);
  }
  .policy-status li {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
  }
  .policy-status .hardened {
    color: var(--accent, #6abf69);
  }
  .policy-status .unguarded {
    color: var(--text-subtle, #999);
  }
  .policy-card {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.75rem;
    margin-bottom: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .policy-head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 0.5rem;
  }
  .policy-program {
    flex: 1;
  }
  .policy-env-label {
    display: block;
    font-size: var(--font-size-sm);
    margin-bottom: 0.25rem;
  }
  .env-row {
    display: flex;
    gap: 0.4rem;
    margin-bottom: 0.4rem;
  }
  .env-row input {
    flex: 1;
  }
  /* V8-02 backend pool editor */
  .backend-card {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.6rem 0.75rem;
    margin: 0.6rem 0;
    background: var(--surface-sunken);
  }
  .backend-head {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    flex-wrap: wrap;
    margin-bottom: 0.3rem;
  }
  /* Blank vertical spacer used to space out the offload backend editor
     (above Server command, Tool scope, and the Start/Stop/Reset row). No
     rule line — just breathing room. */
  .card-divider {
    border: none;
    margin: 0;
    height: 0.9rem;
  }
  /* Wider gap above the Server command and Tool scope groups. */
  .card-divider.lg {
    height: 1.8rem;
  }
  .backend-name {
    flex: 1 1 8rem;
    min-width: 6rem;
    font-weight: 600;
  }
  .checkbox.inline {
    margin: 0;
  }
  .cloud-consent {
    border-left: 3px solid var(--accent, #d08770);
    padding-left: 0.5rem;
  }
  button.danger {
    color: #d06b6b;
  }
  .badge {
    font-size: var(--font-size-sm);
    padding: 0.05rem 0.4rem;
    border-radius: 999px;
    border: 1px solid var(--border-subtle);
  }
  .badge.warn {
    color: #d08770;
    border-color: #d08770;
  }
  /* Multiline, word-wrapping Server command field so every argument of a long
     llama-server invocation stays visible without horizontal scrolling. */
  textarea.server-command {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    min-height: 7.8rem;
    font-family: var(--font-mono, monospace);
    font-size: var(--font-size-sm);
    line-height: 1.4;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .template-actions {
    margin-top: 0.35rem;
  }
  .template-actions button.active {
    border-color: var(--accent, #d08770);
    color: var(--accent, #d08770);
  }
  .template-popup {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md, 6px);
    padding: 0.5rem 0.6rem;
    margin: 0.4rem 0 0.2rem;
    background: var(--surface-1, var(--surface-sunken));
  }
  .template-save {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    flex-wrap: wrap;
  }
  .template-save input[type='text'] {
    flex: 1 1 10rem;
    min-width: 8rem;
  }
  .template-list {
    list-style: none;
    padding: 0;
    margin: 0 0 var(--space-2);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .template-list li {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .template-list .template-name {
    flex: 0 1 auto;
    max-width: 40%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* Dimmed secondary line (e.g. a remote endpoint's base URL) that fills the
     remaining row width and truncates before the trailing action button. */
  .template-list .template-sub {
    flex: 1 1 auto;
    min-width: 0;
    font-size: var(--font-size-xs);
    opacity: 0.6;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* V14 Phase A: Compose section's global-template editor rows — a name
     field, a growable body textarea, and a delete button. Unlike the
     read-only `.template-list` rows above, each entry here is directly
     editable. */
  .compose-template-list {
    gap: var(--space-3);
  }
  .compose-template-row {
    align-items: flex-start !important;
  }
  .compose-template-name {
    flex: 0 0 10rem;
  }
  .compose-template-body {
    flex: 1 1 auto;
    min-width: 0;
    font-family: inherit;
    font-size: var(--font-size-sm, 13px);
    resize: vertical;
    padding: 6px 8px;
  }
  /* Two-column layout: fixed sidebar on the left, scrollable content on
     the right. The settings page lives inside #app, which app.css pins to
     the viewport — sizing .root to 100vh fills that frame. */
  .root {
    display: flex;
    height: 100vh;
    overflow: hidden;
  }
  .sidebar {
    width: 184px;
    flex-shrink: 0;
    overflow-y: auto;
    background: var(--surface-deep);
    border-right: 1px solid var(--border-faint);
    padding: 16px 10px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    box-sizing: border-box;
  }
  .sidebar-title {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-tertiary);
    padding: 4px 12px var(--space-2);
  }
  .sidebar button {
    display: block;
    width: 100%;
    text-align: left;
    padding: 7px 12px;
    background: transparent;
    border: 1px solid transparent;
    color: var(--text-quiet);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard);
  }
  .sidebar button:hover:not(.active) {
    background: var(--surface-1);
    color: var(--text-primary);
  }
  .sidebar button.active {
    background: var(--surface-1);
    color: var(--accent-purple);
    font-weight: 600;
    border-color: var(--border-subtle);
  }
  .sidebar button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .content {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    overflow-x: hidden;
    padding: 16px 20px 32px;
    box-sizing: border-box;
  }
  .inner {
    max-width: 720px;
    margin: 0 auto;
  }
  .loading {
    padding: var(--space-6);
    text-align: center;
    color: var(--text-tertiary);
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
    margin: 0 0 var(--space-3) 0;
    color: var(--accent-purple);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  /* Sub-headings inside a section get a bit more weight + breathing room
     than the original — used to separate distinct logical groups inside
     a single section (e.g. UI theme vs Terminal palette in Theme). */
  h3 {
    font-size: var(--font-size-md);
    font-weight: 600;
    margin: var(--space-5) 0 var(--space-2) 0;
    padding-top: var(--space-3);
    border-top: 1px solid var(--border-faint);
    color: var(--text-primary);
  }
  /* The first h3 in a section sits right under the h2 — skip the divider
     so we don't double-up with the section's top edge. */
  section > h3:first-of-type {
    margin-top: var(--space-3);
    padding-top: 0;
    border-top: none;
  }
  section {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    padding: var(--space-4);
    margin-bottom: var(--space-4);
    background: var(--surface-1);
  }
  label {
    display: block;
    margin-bottom: var(--space-3);
  }
  label > span:first-child {
    display: block;
    margin-bottom: var(--space-1);
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
    /* Tabular numerics so slider value labels (e.g. "Speed: 1.20×")
       don't jitter the label width as the value changes. */
    font-variant-numeric: tabular-nums;
    font-feature-settings: "tnum";
  }
  label.checkbox {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  label.checkbox > span {
    margin: 0;
  }
  label.checkbox.disabled {
    opacity: 0.6;
  }
  input[type='text'],
  input[type='number'],
  input[type='password'],
  select {
    width: 100%;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: inherit;
    font-size: var(--font-size-md);
    box-sizing: border-box;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  input[type='text']:focus,
  input[type='number']:focus,
  input[type='password']:focus,
  select:focus {
    outline: none;
    border-color: var(--accent);
  }
  input[type='range'] {
    width: 100%;
    accent-color: var(--accent);
  }
  input[type='color'] {
    height: 32px;
    padding: 0;
    border: 1px solid var(--border-default);
    background: var(--surface-2);
    border-radius: var(--radius-md);
  }
  /* Selection-highlight color pickers: two columns, each a label + a
     "Custom" toggle + the swatch. */
  .color-grid {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: var(--space-2) var(--space-3);
    margin: var(--space-2) 0;
  }
  .color-cell {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: 0.85em;
  }
  .color-cell-label {
    font-weight: 500;
  }
  .checkbox.compact {
    font-size: 0.9em;
    gap: var(--space-1);
  }
  .color-grid.disabled {
    opacity: 0.5;
  }
  .row {
    display: flex;
    gap: var(--space-3);
  }
  .row > label {
    flex: 1;
  }
  /* Pair an input with an inline action button (Show/Hide, Browse, etc.).
     Without this, the button wraps below a width:100% input and
     `small.hint`'s negative top margin pulls the hint upward into the
     wrapped button. */
  .input-with-action {
    display: flex;
    gap: var(--space-2);
    align-items: stretch;
  }
  .input-with-action > input {
    flex: 1;
    min-width: 0;
  }
  /* V23 Phase A: Code Audit per-tool row. */
  .audit-tool {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) 0;
    border-top: 1px solid var(--border, rgba(128, 128, 128, 0.25));
  }
  .audit-tool .audit-name {
    font-weight: 600;
  }
  .audit-tool .audit-role {
    margin-left: var(--space-2);
    opacity: 0.7;
    font-size: 0.85em;
  }
  small.hint.audit-detect {
    margin: 0;
    font-family: var(--font-mono, monospace);
    word-break: break-all;
  }
  small.hint.audit-detect.ok {
    color: var(--success, #4caf50);
  }
  small.hint.audit-detect.bad {
    color: var(--danger, #e06c75);
  }
  small.hint.audit-na {
    margin: 0;
    font-style: italic;
    color: var(--warning, #e3b341);
    opacity: 0.85;
  }
  /* Quality auto-selection: the mode note (automatic) / re-apply row (manual). */
  small.hint.audit-auto-note {
    display: block;
    margin: var(--space-2) 0 0;
  }
  .audit-auto-row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin-top: var(--space-2);
  }
  .audit-auto-row small.hint {
    margin: 0;
  }
  .audit-timeout {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 0.85em;
    opacity: 0.85;
  }
  .audit-timeout input {
    width: 7rem;
  }
  .input-with-action > button {
    flex-shrink: 0;
  }
  .file-row {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    margin-bottom: 6px;
  }
  .state-label {
    width: 80px;
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
    text-transform: capitalize;
  }
  .filename {
    flex: 1;
    color: var(--text-primary);
    font-family: monospace;
    font-size: var(--font-size-sm);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  button {
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-3);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  button:hover:not(:disabled) {
    background: var(--surface-input);
    border-color: var(--border-strong);
  }
  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  button.ghost {
    background: transparent;
  }
  button.danger {
    color: var(--text-danger-bright);
    border-color: var(--border-danger);
  }
  button.danger:hover:not(:disabled) {
    background: var(--surface-danger-soft);
    border-color: var(--border-danger-strong);
  }
  small.hint {
    display: block;
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    /* Generous leading so tall inline glyphs — e.g. the ▓░ shade blocks in
       the Claude context-bar example below — don't bleed up into the line
       above when their fallback-font ink overflows the line box. */
    line-height: 1.6;
    margin: -8px 0 var(--space-3) 0;
  }
  /* Inline code inside a hint (the context-bar example, env-var names, …):
     pin a monospace with tight metrics and clamp its line box so the shade
     glyphs stay contained within the paragraph's leading. */
  small.hint code {
    font-family: Consolas, Menlo, monospace;
    font-size: 0.95em;
    line-height: 1;
  }
  /* hint placed directly under an h3 (rather than tucked under a label)
     needs normal top margin — it has no preceding label to overlap. */
  small.hint.top {
    margin-top: 0;
    margin-bottom: var(--space-3);
  }
  /* A hint that follows a bare button or a checkbox row has no tall block
     label above it for the negative top margin to tuck under — that margin
     would otherwise pull the hint up over the button/checkbox (e.g. the
     Bottom bar → Status bar arrangement and Claude context bar sections).
     Reset to a normal positive gap. */
  button + small.hint,
  label.checkbox + small.hint {
    margin-top: var(--space-1);
  }
  /* V6-01: missing-model / device-not-found warning hint. */
  small.hint.warn {
    color: var(--accent, #d77757);
  }
  /* When a hint is nested *inside* a label after the input (Local LLM
     section, shell command field, etc.) the global -8px would pull it
     up over the input box. The negative margin only makes sense for
     sibling hints below a label, where it tightens the gap to the
     label above. */
  label > small.hint {
    margin-top: var(--space-1);
  }
  .preset-actions {
    display: flex;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }
  .content-actions {
    display: flex;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }
  .preset-save {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    margin-bottom: var(--space-2);
  }
  .preset-save input[type='text'] {
    flex: 1;
  }
  small.error {
    color: var(--text-error);
    font-size: var(--font-size-xs);
  }
  .preset-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .preset-list li {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .preset-list li input[type='text'] {
    flex: 1;
  }
  .palette-row {
    display: grid;
    grid-template-columns: 1fr auto;
    grid-column-gap: var(--space-2);
    align-items: end;
  }
  .palette-row > span:first-child {
    grid-column: 1 / -1;
  }
  .claude-tabs-radio {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    padding: var(--space-3) var(--space-4);
    margin: 0 0 var(--space-4) 0;
    background: var(--surface-1);
  }
  .claude-tabs-radio legend {
    padding: 0 var(--space-2);
    font-size: var(--font-size-sm);
    font-weight: 500;
    color: var(--text-primary);
  }
  .claude-tabs-radio .hint {
    display: block;
    margin: 0 0 var(--space-3) 0;
    color: var(--text-quiet);
  }
  .claude-tabs-radio .radio-row {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
  }
  .claude-tabs-radio .radio-row label {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: var(--font-size-sm);
    cursor: pointer;
  }
  .sub-tabs {
    display: flex;
    gap: 2px;
    margin: 0 0 var(--space-4) 0;
    padding: 0;
    border-bottom: 1px solid var(--border-subtle);
  }
  .sub-tabs button {
    appearance: none;
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--text-quiet);
    cursor: pointer;
    padding: 8px 14px;
    font-size: var(--font-size-sm);
    font-weight: 500;
    border-radius: 0;
    margin-bottom: -1px;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    transition:
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .sub-tabs button:hover:not(.active) {
    color: var(--text-primary);
    background: transparent;
  }
  .sub-tabs button.active {
    color: var(--accent-purple);
    border-bottom-color: var(--accent-purple);
    font-weight: 600;
    background: transparent;
  }
  .sub-tabs button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .sub-tab-count {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 18px;
    height: 18px;
    padding: 0 5px;
    border-radius: var(--radius-pill);
    background: var(--surface-2);
    color: var(--text-tertiary);
    font-size: 10px;
    font-weight: 600;
    line-height: 1;
  }
  .sub-tabs button.active .sub-tab-count {
    background: var(--accent-muted);
    color: var(--accent-purple);
  }
  .tabs-grid {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: var(--space-2);
  }
  details {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    background: var(--surface-deep);
  }
  details[open] {
    background: var(--surface-sunken);
  }
  summary {
    cursor: pointer;
    padding: var(--space-2) var(--space-3);
    color: var(--text-primary);
    font-weight: 600;
    font-size: var(--font-size-sm);
    user-select: none;
    border-radius: var(--radius-md);
    transition: background var(--motion-fast) var(--easing-standard);
  }
  summary:hover {
    background: var(--surface-1);
  }
  details[open] > summary {
    border-bottom: 1px solid var(--border-subtle);
    border-radius: var(--radius-md) var(--radius-md) 0 0;
  }
  .kind-badge {
    display: inline-block;
    font-size: 9px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    padding: 1px 6px;
    border-radius: var(--radius-pill);
    margin-left: 6px;
    vertical-align: middle;
    font-weight: 600;
  }
  .kind-badge.shell {
    background: var(--surface-success);
    border: 1px solid var(--text-success-bright);
    color: var(--text-success);
  }
  .builtin-tag {
    display: inline-block;
    font-size: 9px;
    font-weight: var(--font-weight-medium);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-tertiary);
    border: 1px solid var(--border-default);
    padding: 1px 6px;
    border-radius: var(--radius-pill);
    margin-left: 6px;
    vertical-align: middle;
  }
  .shell-edit {
    padding: var(--space-3) 14px;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .shell-edit label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--font-size-sm);
    color: var(--text-quiet);
  }
  .shell-edit input[type="text"] {
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: Consolas, Menlo, monospace;
    font-size: var(--font-size-md);
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .shell-edit input[type="text"]:focus {
    outline: none;
    border-color: var(--accent);
  }
  .shell-edit input[disabled] {
    color: var(--text-tertiary);
    background: var(--surface-deep);
  }
  .shell-edit code {
    background: var(--surface-1);
    padding: 1px var(--space-1);
    border-radius: var(--radius-sm);
    font-size: var(--font-size-xs);
  }
  /* V1.11 per-slot notification row: enabled checkbox above a text
     input. The disabled-text style mirrors `.shell-edit input[disabled]`
     so a toggled-off slot reads as visually quiet without the
     readonly-Command "this is informational" feel. */
  .shell-edit .shell-notif-row {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .shell-edit .row-toggle {
    flex-direction: row;
    align-items: center;
    gap: 6px;
    cursor: pointer;
  }
  .shell-edit .row-toggle input[type="checkbox"] {
    margin: 0;
  }
  .shell-edit .shell-notif-row > input[type="text"]:disabled {
    opacity: 0.5;
  }

  /* About page: a small definition list keyed by Author / Version /
     Repository. Two-column grid (label | value) so the values line up
     even with mixed key lengths. */
  .about-list {
    display: grid;
    grid-template-columns: max-content 1fr;
    column-gap: var(--space-4);
    row-gap: var(--space-3);
    margin: 0;
    padding: 0;
  }
  .about-list dt {
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-weight: 600;
    padding-top: 2px;
  }
  .about-list dd {
    margin: 0;
    color: var(--text-primary);
    font-size: var(--font-size-md);
    word-break: break-all;
  }
  .about-list dd code {
    background: var(--surface-deep);
    border: 1px solid var(--border-subtle);
    padding: 1px var(--space-2);
    border-radius: var(--radius-sm);
    font-family: Consolas, Menlo, monospace;
    font-size: var(--font-size-sm);
  }
  .about-list dd a {
    color: var(--accent-purple);
    text-decoration: none;
    transition: color var(--motion-fast) var(--easing-standard);
  }
  .about-list dd a:hover {
    color: var(--accent-bright);
    text-decoration: underline;
  }

  /* Narrow window: collapse sidebar to a horizontal strip on top so the
     content area still gets full width. The Settings window can resize
     down to ~480px on common installs. */
  @media (max-width: 640px) {
    .root {
      flex-direction: column;
    }
    .sidebar {
      width: 100%;
      flex-direction: row;
      flex-wrap: wrap;
      gap: 4px;
      border-right: none;
      border-bottom: 1px solid var(--border-faint);
      padding: 10px 12px;
    }
    .sidebar-title {
      width: 100%;
      padding: 0 0 var(--space-1);
    }
    .sidebar button {
      width: auto;
      padding: 5px 10px;
    }
  }
</style>
