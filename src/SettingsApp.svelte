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
    consumeSettingsDeepLink,
    listVoices,
    requestTabRestart,
  } from './lib/settings/ipc';
  import { listen } from '@tauri-apps/api/event';
  import type {
    AiToolTabConfig,
    Settings,
    ShellTabConfig,
    TabConfig,
  } from './lib/settings/types';
  import { defaultSettings, findTab, findTabIndex, toPresetConfig } from './lib/settings/types';
  import { contentClear, contentOpenFolder, setEnabledAiTabs } from './lib/ipc';
  import { listSttModels, listInputDevices } from './lib/stt';
  import {
    offloadTest,
    offloadStatuses,
    offloadBackendStart,
    offloadBackendStop,
    offloadBackendRestart,
    describeBackendStatus,
    offloadServiceStatus,
    offloadReloadMcp,
    describeMcpServerHealth,
    type BackendStatus,
    type ServiceStatus,
  } from './lib/offload';
  import { graphRebuild, graphStatus, type GraphStatus } from './lib/graph';
  import type {
    OffloadBackend,
    ToolScope,
    BackendTier,
    CommandPolicy,
    McpServerConfig,
  } from './lib/settings/types';
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

  // V8-02 backend pool: live per-backend status rows + a refresh loop while
  // the Offload section is open.
  let backendStatuses = $state<BackendStatus[]>([]);
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
          kind: { type: 'local', server_command: '', autostart: false },
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
          kind: { type: 'local', server_command: s.offload.server_command, autostart: s.offload.autostart },
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
  // ── Command security policies (Tools tab) ──────────────────────────────
  // All mutations route through `patch` so they persist + mark dirty, mirroring
  // the backend-pool helpers above.
  function addCommandPolicy(): void {
    patch((s) => {
      s.offload.command_policies = [
        ...s.offload.command_policies,
        { program: '', denied_flags: [], denied_subcommands: [], env: [] },
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
  // buttons match the Rust-side defaults exactly (in particular the embedded
  // RUNTIME_SYSTEM_PROMPT for Claude's TTS instructions).
  let tabDefaults = $state<Record<string, AiToolTabConfig | null>>({});
  let snapshot = $state<Settings | null>(null);

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
    | 'offload'
    | 'mcp'
    | 'graph'
    | 'advanced'
    | 'about';
  let activeSection = $state<SectionId>('theme');
  const SECTIONS: { id: SectionId; label: string }[] = [
    { id: 'theme', label: 'Appearance' },
    { id: 'avatar', label: 'Avatar' },
    { id: 'shortcuts', label: 'Keyboard controls' },
    { id: 'bottom-bar', label: 'Bottom bar' },
    { id: 'audio', label: 'Text-to-speech' },
    { id: 'stt', label: 'Speech-to-text' },
    { id: 'tabs', label: 'Tabs' },
    { id: 'offload', label: 'Offload task tools' },
    { id: 'mcp', label: 'MCP servers' },
    { id: 'graph', label: 'Code graph' },
    { id: 'advanced', label: 'Advanced' },
    { id: 'about', label: 'About' },
  ];
  const REPO_URL = 'https://github.com/Dyserna/cImp';

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
        if (target) scrollToTabSection(target);
      })
      .catch((e) => console.warn('consume_settings_deep_link failed', e));

    // V1.4-07 A: hot-open deep-link. Fired while this window is already
    // open and the user clicks Configure on a different tab. If we got
    // disposed between the await and now, tear the listener down
    // immediately rather than storing it where onDestroy can no longer
    // reach it.
    const deepLinkUnlisten = await listen<{ kind: string; tab_id: string }>(
      'settings-deep-link',
      (e) => {
        if (e.payload.kind === 'tab') scrollToTabSection(e.payload.tab_id);
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

  // Restart-affecting subset: command + args + cwd + env + TTS injection.
  // Notifications and first_launch_notice_dismissed apply live and are
  // excluded.
  function restartShape(t: AiToolTabConfig) {
    return {
      command: t.command,
      args: t.args,
      cwd: t.cwd,
      env: t.env,
      tts_injection: t.tts_injection,
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
      ? snapshot.tabs.find((t) => t.id === activeTabId) ?? null
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
      const src = s.tabs.find((t) => t.id === id);
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
      for (const t of s.tabs) {
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

  // Browse for an external-tool exe and store its path (Settings → Bottom
  // bar). Cancelling the dialog leaves the current value untouched.
  async function pickToolExe(tool: keyof Settings['external_tools']) {
    const p = await pickFile('Executable', ['exe']);
    if (p) patch((s) => (s.external_tools[tool] = p));
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
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.display.show_tts_markup}
              onchange={(e) =>
                patch((s) => (s.display.show_tts_markup = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Show TTS markup in terminal (debug)</span>
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
            resolved from the bundled <code>ebin\</code> folder first, then your
            PATH. To use a specific build instead — e.g. one in a folder that
            isn't on PATH — point cImp at the exe here. Leave blank to resolve
            normally. Takes effect the next time you launch the tool.
          </small>
          <label>
            <span>rustnet</span>
            <div class="input-with-action">
              <input
                type="text"
                placeholder="(use bundled ebin / PATH)"
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
                placeholder="(use bundled ebin / PATH)"
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
          <label>
            <span>Switch to Claude tab</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.switch_to_tab_1,
                (v) => patch((s) => (s.shortcuts.switch_to_tab_1 = v))
              }
            />
          </label>
          <label>
            <span>Switch to Claude (local) tab</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.switch_to_tab_2,
                (v) => patch((s) => (s.shortcuts.switch_to_tab_2 = v))
              }
            />
          </label>
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
          <small class="hint">
            When on, cImp injects the <code>offload_task</code> tool into
            Claude tabs (re-launch a tab to pick it up). Configure your
            local/remote models in the <strong>Backend pool</strong> below.
          </small>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.offload.inject_guidance}
              onchange={(e) =>
                patch((s) => (s.offload.inject_guidance = (e.currentTarget as HTMLInputElement).checked))}
            />
            <span>Inject offload guidance into the system prompt</span>
          </label>

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
            Watch the local model load + server logs live in the read-only
            <strong>Offload Server</strong> tab (appears when offload is
            enabled).
          </small>

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
                <label>
                  <span>Server command</span>
                  <input
                    type="text"
                    value={backend.kind.server_command}
                    oninput={(e) =>
                      updateBackend(i, (b) => {
                        if (b.kind.type === 'local') b.kind.server_command = (e.currentTarget as HTMLInputElement).value;
                      })}
                    placeholder="llama-server --model … --port 8080 --jinja -ngl 99 --ctx-size 150000"
                  />
                </label>
                <label class="checkbox">
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
                <div class="button-row">
                  <button type="button" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendStart(backend.name))}>Start</button>
                  <button type="button" class="secondary" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendStop(backend.name))}>Stop</button>
                  <button type="button" class="secondary" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendRestart(backend.name))}>Reset</button>
                </div>
              {:else if backend.kind.type === 'remote'}
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
              {/if}

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
            </div>
          {/each}

          <div class="button-row">
            <button type="button" onclick={addLocalBackend}>+ Local backend</button>
            <button type="button" onclick={addRemoteBackend}>+ Remote backend</button>
          </div>

          {#if serviceStatus}
            <div class="offload-status warm-pool">
              <span class="offload-status-label">Warm pool:</span>
              <span>
                {serviceStatus.global_in_flight} / {serviceStatus.global_cap} offloads in flight
                (global, across all Claude tabs){#if serviceStatus.queue_depth > 0}, {serviceStatus.queue_depth}
                  queued{/if}
              </span>
            </div>
          {/if}

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
          <h2>Code knowledge graph</h2>
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
              Supported: <code>rust</code>, <code>typescript</code>,
              <code>javascript</code>, <code>python</code>, <code>markdown</code>.
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
                Use <strong>Rebuild embeddings</strong> on the Code Graph tab
                after a silent model swap behind the same name.
              </small>
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
  /* V8-03 warm-pool readout + per-MCP-server health */
  .warm-pool {
    color: var(--text-secondary);
  }
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
