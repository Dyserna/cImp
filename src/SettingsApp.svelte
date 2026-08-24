<script lang="ts">
  // #129 (a): the form/chrome base styles used to live in this file's own style
  // block, which is what stopped the sections being split out into children —
  // Svelte scoping keeps a parent's rules out of every child component. They
  // are now a plain sheet keyed on the `.settings-chrome` class this component
  // puts on `.root`; a section child extracted in #129 (c) imports the same
  // sheet and needs no markup change. This import stays FIRST: the sheet must
  // be emitted ahead of every child component's CSS so that where a rule here
  // ties on specificity with a child's own scoped rule, the child wins.
  import './lib/settings/settings-chrome.css';
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import { initSettings, settings } from './lib/settings/store';
  // `applySettings` is deliberately NOT imported here: every push of this
  // window's draft goes through `pushDraft`, which is the gate and the push
  // in one call (V42 tranche-2 review, T2-6). `draftSync.test.ts` enforces
  // that — assembling the pair by hand is how one push came to be ungated.
  import { createDraftSync, pushDraft } from './lib/settings/draftSync';
  import {
    aiToolTabDefaults,
    consumeSettingsDeepLink,
    harnessRunChecks,
    harnessVersionsGet,
    listVoices,
    llmPricingGet,
    llmPricingSet,
    pluginsProjectKey,
    pluginsRescan,
    pluginsSnapshot,
    requestTabRestart,
  } from './lib/settings/ipc';
  import {
    listenEvent,
    AI_TAB_RESTART_HINT,
    SETTINGS_DEEP_LINK,
  } from './lib/events';
  import type {
    AiToolTabConfig,
    AuditDetectResult,
    HarnessStatus,
    Settings,
  } from './lib/settings/types';
  import {
    defaultSettings,
    findTab,
    CONTROL_READ_ADVISOR,
    controlBlocked,
    // V32 / #48 F-27: the ONE list of spawn-baked injection features, read by
    // both restart-hint shapes below.
    spawnBakedInjectionL2,
    spawnBakedTabOverrides,
  } from './lib/settings/types';
  import {
    harnesses,
    harnessLabels,
    harnessLabelsProse,
    loadHarnesses,
    reservedAiTabIds,
  } from './lib/harness';
  import { setEnabledAiTabs } from './lib/ipc';
  import { listSttModels, listInputDevices } from './lib/stt';
  import {
    offloadStatuses,
    offloadServiceStatus,
    offloadReloadMcp,
    offloadEnableReadonlyCommands,
    detectionStatus,
    detectionCheckNow,
    detectionRevert,
    type BackendStatus,
    type ServiceStatus,
    type DetectionStatus,
  } from './lib/offload';
  // V32 Phase G: the resolved enable hierarchy, from the backend's one resolver.
  import { fetchInjectionStatus, type InjectionStatus } from './lib/latch';
  import { graphIgnorePick, graphRebuild, graphStatus, type GraphStatus } from './lib/graph';
  import type {
    PromptTemplate,
    LlmPricingModel,
  } from './lib/settings/types';
  import {
    composeTemplatesGlobalGet,
    composeTemplatesGlobalSet,
    composeTemplatesProjectGet,
  } from './lib/compose/templates';
  import type { AiTabId } from './lib/tabs/types';
  import AudioSection from './lib/settings/sections/AudioSection.svelte';
  import SttSection from './lib/settings/sections/SttSection.svelte';
  import ThemeSection from './lib/settings/sections/ThemeSection.svelte';
  import TabsSection from './lib/settings/sections/TabsSection.svelte';
  import OffloadSection from './lib/settings/sections/OffloadSection.svelte';
  import InjectionSection from './lib/settings/sections/InjectionSection.svelte';
  import SandboxingSection from './lib/settings/sections/SandboxingSection.svelte';
  import ToolPluginsSection from './lib/settings/sections/ToolPluginsSection.svelte';
  import AboutSection from './lib/settings/sections/AboutSection.svelte';
  import ChecksSection from './lib/settings/sections/ChecksSection.svelte';
  import McpSection from './lib/settings/sections/McpSection.svelte';
  import PricingSection from './lib/settings/sections/PricingSection.svelte';
  import ComposeSection from './lib/settings/sections/ComposeSection.svelte';
  import HarnessSection from './lib/settings/sections/HarnessSection.svelte';
  import GraphSection from './lib/settings/sections/GraphSection.svelte';
  import ShortcutsSection from './lib/settings/sections/ShortcutsSection.svelte';
  import CodeAuditSection from './lib/settings/sections/CodeAuditSection.svelte';
  import BottomBarSection from './lib/settings/sections/BottomBarSection.svelte';
  import AvatarSection from './lib/settings/sections/AvatarSection.svelte';
  import AdvancedSection from './lib/settings/sections/AdvancedSection.svelte';
  import WorkbenchSection from './lib/settings/sections/WorkbenchSection.svelte';
  import TuiTitleBar from './lib/TuiTitleBar.svelte';
  // V37 Phase D: the MCP-servers section's body (contract C8), now inside the
  // section component #129 (c) extracted. The registry type stays here because
  // the two persistence callbacks below are still this window's.
  import type { McpRegistry } from './lib/settings/mcpEditor';
  import { AUDIT_PLUGIN_KEY, type PluginSet } from './lib/settings/toolPlugins';
  import { auditRefreshCensus } from './lib/codeAudit/ipc';
  import { qualityAutoSelection } from './lib/codeAudit/logic';
  import type { AuditCensus } from './lib/codeAudit/types';
  import { themeRegistry } from './lib/themes/registry';

  // Whether the active theme uses the OS-native chrome — drives the custom
  // settings-window title bar. Derived from the registry so it follows the
  // theme's `decorations` metadata (and updates once the registry loads).
  let useCustomTitleBar = $derived(
    !($themeRegistry.find((t) => t.id === $settings.ui.theme)?.decorations ?? false),
  );

  let voices = $state<string[]>([]);
  // V6-01: available STT models (ggml-*.bin under models/) and cpal input
  // device names, populated on mount for the STT section dropdowns.
  let sttModels = $state<string[]>([]);
  let inputDevices = $state<string[]>([]);
  // V1.4-07's `showLocalToken` moved into `HarnessExtForm` with the input it
  // toggled: the Show/Hide button is now driven by the declaration's `secret`
  // column, so every plugin's credentials get it and none of them needs a flag
  // in this file.
  // Inline error under the AI-tabs checkbox group — e.g. when enabling an
  // an AI tab is rejected because its harness binary isn't installed
  // (ebin/PATH).
  let aiTabsError = $state<string | null>(null);
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
  //
  // #129: this push takes the SAME draftSync gate as `patch()` and
  // `applyMcpRegistry()`. It is a wholesale push of the LOCAL DRAFT, so a
  // `settings-changed` broadcast landing while it is in flight could replace
  // that draft — and because the rows are edited IN PLACE through
  // `ArrayEditor`'s bind, what such a replacement discards is the ignore list
  // the user just typed, with the next `patch()` then cloning the regressed
  // draft and pushing it wholesale. That is the lost-update race `draftSync`
  // exists to close, and this was the one push in the window not registered
  // with it — which is why there is no longer a way to write one that is not:
  // `pushDraft` IS the gate plus the push (T2-6).
  function commitGraphIgnore(): void {
    if (!snapshot) return;
    void pushDraft(draftSync, $state.snapshot(snapshot));
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
  /// The one place a compose edit marks the list dirty. `ComposeSection` owns
  /// the four row transforms — pure functions of the current list — and hands
  /// the result back here.
  function setGlobalTemplates(next: PromptTemplate[]): void {
    globalTemplates = next;
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
  /// The one place an edit marks the table dirty. `PricingSection` owns the row
  /// transforms — they are pure functions of the current array — and hands the
  /// result back here, so "changed" is decided once rather than in each of the
  /// four editors.
  function setLlmPricingRows(next: LlmPricingModel[]): void {
    llmPricing = next;
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

  // V21: register the given Local backend as `harness`'s local provider.

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
  // V32 Phase C: how much of the injection-detection surface is actually live.
  // Read-only disk facts (rule files compiled, classifier weights present), so
  // it rides the same poller as the backend statuses rather than the settings
  // snapshot — it is not a setting and never round-trips through `patch`.
  let detection = $state<DetectionStatus | null>(null);
  // Recompile the rules from disk and fold the fresh counts back in.
  async function reloadDetection(): Promise<void> {
    detection = await detectionStatus(true);
  }
  // V32 Phase C3: which component (if any) has a check/apply/revert in flight.
  // A single slot rather than a per-button flag because all three buttons on
  // both rows drive the same updater and a second concurrent run would race the
  // same staging directory — so any one running disables all of them.
  let detectionBusy = $state<string | null>(null);
  // Check now / Apply. The whole run (download, validation, swap) is awaited,
  // so the returned status already reflects the outcome and nothing has to be
  // polled for.
  async function checkDetectionUpdate(component: string, apply: boolean): Promise<void> {
    if (detectionBusy) return;
    detectionBusy = component;
    try {
      const next = await detectionCheckNow(component, apply);
      if (next) detection = next;
    } finally {
      detectionBusy = null;
    }
  }
  async function revertDetection(component: string): Promise<void> {
    if (detectionBusy) return;
    detectionBusy = component;
    try {
      const next = await detectionRevert(component);
      if (next) detection = next;
    } finally {
      detectionBusy = null;
    }
  }
  // ── V32 Phase G — the injection enable hierarchy ─────────────────────────
  //
  // The RESOLVED view, from the backend's single resolver. The Settings matrix
  // renders the raw switches from `snapshot` (they are ordinary settings) but
  // the "what is actually in force, and which level decided it" column comes
  // from here — reimplementing the resolution rule in TypeScript would put the
  // locked decision-16 rule in two places, which is the one thing it cannot
  // survive.
  let injection = $state<InjectionStatus | null>(null);

  let backendStatusTimer: ReturnType<typeof setInterval> | null = null;
  async function refreshBackendStatuses(): Promise<void> {
    try {
      backendStatuses = await offloadStatuses();
    } catch (e) {
      console.warn('offload_statuses failed', e);
    }
    serviceStatus = await offloadServiceStatus();
    // Cheap: cached disk facts, no recompile (`reload = false`). Skipped while
    // an updater run is in flight — a 4-second poll landing mid-swap would
    // overwrite the button row with a half-applied snapshot, and the run's own
    // return value is the authoritative one.
    if (!detectionBusy) detection = await detectionStatus();
    // V32 Phase G: the RESOLVED hierarchy, from the backend's one resolver.
    // Deliberately not recomputed in TypeScript from `snapshot`: a second
    // implementation of the resolution rule is exactly the drift the
    // one-resolver invariant exists to prevent, and the cost of asking is one
    // in-process mutex read. It reflects SAVED settings, so a just-flipped
    // switch shows its new resolved value once the debounced save lands.
    try {
      injection = await fetchInjectionStatus();
    } catch (e) {
      console.warn('injection_status failed', e);
    }
  }
  function startBackendStatusPolling(): void {
    if (backendStatusTimer) return;
    void refreshBackendStatuses();
    backendStatusTimer = setInterval(refreshBackendStatuses, 4000);
  }

  async function enableReadonlyCommands(): Promise<void> {
    const updated = await offloadEnableReadonlyCommands();
    if (updated) snapshot = updated;
  }
  // ── MCP tool servers (MCP servers section) ─────────────────────────────
  // V37 Phase D (contract C8): the editor itself is `McpManagementEditor`. What
  // stays here is the persistence seam, because only this file owns `snapshot`
  // and the awaited push the rest of the panel uses.
  //
  // Two callbacks, deliberately distinct:
  //
  // * `setMcpRegistry` — the LOCAL snapshot only, no backend write. Text fields
  //   commit on blur rather than per keystroke: persisting per keystroke raced,
  //   because fire-and-forget pushes could complete out of order
  //   and leave the backend holding a half-typed URL, which the 12s health watch
  //   would then flag as down.
  // * `applyMcpRegistry` — ONE awaited gated push followed by ONE
  //   `offload_reload_mcp` (contract C5's UI half). Awaited in that order so the
  //   reconcile runs against the new config rather than the stale one, and a
  //   category toggle spanning N servers is still exactly one of each.
  function setMcpRegistry(next: McpRegistry): void {
    if (!snapshot) return;
    const s = structuredClone($state.snapshot(snapshot));
    s.offload.mcp_servers = next.servers;
    s.offload.mcp_categories = next.categories;
    s.offload.mcp_activation = next.activation;
    snapshot = s;
  }
  async function applyMcpRegistry(next: McpRegistry): Promise<void> {
    setMcpRegistry(next);
    if (!snapshot) return;
    // Same lost-update gate as `patch()` — `pushDraft` carries it — because
    // this is a wholesale push too, and a broadcast landing during the awaits
    // below must not regress the draft.
    await pushDraft(draftSync, $state.snapshot(snapshot));
    await reloadMcpHost();
  }
  // Reconcile the warm host now and fold the fresh status into the health chips.
  async function reloadMcpHost(): Promise<void> {
    const status = await offloadReloadMcp();
    if (status) serviceStatus = status;
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
  // Settings-window open, and the payload carries the gate verdicts Rust
  // computed against those fresh versions, so a just-recorded outcome
  // disables the toggle without an app restart.
  //
  // V35 Phase E: this used to read `harness_versions.e1_status` and apply a
  // hand-kept TypeScript copy of the fail-closed rule (`harnessStatusBlocks`).
  // It now reads the verdict for a capability id — the same id
  // `tabs/config.rs` asks the gate about before installing the hook — so the
  // toggle and the hook cannot disagree by one of them being re-implemented
  // here. Before the fetch resolves nothing is blocked, which is exactly the
  // pre-Phase-E behaviour: `snapshot` is null then too, and the old
  // expression read an empty status as "not blocked".
  let harnessFresh = $state<HarnessStatus | null>(null);
  // V40 Phase F: the capability id comes from the payload, keyed by the neutral
  // control name (locked decision 27). An id the backend has not sent yet
  // (first paint) reads as "not blocked", the same as a capability with no gate.
  // V40 review M-4: through `controlBlocked`, which fails CLOSED when the
  // payload carries no row for the control. `gated_controls?.[…] ?? ''` handed
  // `capabilityBlocked` an empty id, which answers "not blocked" — so a control
  // renamed in Rust silently un-gated the toggle it protects.
  const e1Gate = $derived(controlBlocked(harnessFresh, CONTROL_READ_ADVISOR));

  // ── V35 Phase G: Harness health ─────────────────────────────────────────
  //
  // The panel renders `harnessFresh.harness_health` and decides nothing: the
  // grouping, the tier ordering, the coverage marks and every verdict were
  // computed in Rust (`harness::health`). What lives here is display state —
  // which harness's checks the user just started, and the poll that watches for
  // the run to finish.
  //
  // The poll exists because the run is fire-and-forget across a thread boundary
  // (up to 90s of child processes) and its answers land in two places the
  // window is not subscribed to: the physical global settings file and an
  // in-process cache. Re-reading the same command the window already opened
  // with is cheaper than inventing an event, and it is the same "fetch fresh,
  // never trust the startup snapshot" rule `harness_versions` has carried since
  // V16.
  const HARNESS_POLL_MS = 2000;
  let harnessPoll: ReturnType<typeof setInterval> | undefined;
  /// The harness whose checks this window asked for, until the payload shows a
  /// run in flight (or the poll gives up). Distinct from `verify_in_flight`:
  /// that flag is process-wide and can be true because of an automatic
  /// version-change run nobody clicked for.
  let harnessStarting = $state<string | null>(null);
  let harnessRunError = $state<string | null>(null);
  const harnessBusy = $derived(
    harnessStarting !== null || (harnessFresh?.verify_in_flight ?? false),
  );

  async function refreshHarness(): Promise<void> {
    try {
      harnessFresh = await harnessVersionsGet();
    } catch (e) {
      console.warn('harness_versions_get failed', e);
    }
  }

  function stopHarnessPoll(): void {
    if (harnessPoll !== undefined) {
      clearInterval(harnessPoll);
      harnessPoll = undefined;
    }
  }

  // Poll only while something is running, and stop the moment it clears — a
  // permanent timer in a settings window is the "anything periodic in a view
  // must gate on visibility" trap with extra steps. The round cap is the
  // belt-and-braces half: a backend run is bounded at ~90s, so five minutes of
  // polling means the flag is wedged, and a wedged flag must not leave a timer
  // running for the life of the window.
  const HARNESS_POLL_MAX_ROUNDS = 150;
  function startHarnessPoll(): void {
    if (harnessPoll !== undefined) return;
    let rounds = 0;
    harnessPoll = setInterval(async () => {
      rounds += 1;
      await refreshHarness();
      // The optimistic flag is held until the BACKEND says nothing is running:
      // clearing it as soon as the payload confirms the run would flip the
      // button's label back to idle while the checks were still going.
      if (!(harnessFresh?.verify_in_flight ?? false) || rounds >= HARNESS_POLL_MAX_ROUNDS) {
        harnessStarting = null;
        stopHarnessPoll();
      }
    }, HARNESS_POLL_MS);
  }

  async function runHarnessChecks(harness: string): Promise<void> {
    if (harnessBusy) return;
    harnessRunError = null;
    harnessStarting = harness;
    startHarnessPoll();
    try {
      const started = await harnessRunChecks(harness);
      if (!started) {
        // Single-flight said no: a run was already going. Not an error — the
        // poll will show its result — but say so rather than pretending this
        // click did something.
        harnessRunError =
          'A verification run was already in progress; this request was dropped. Its result will appear here when it finishes.';
      }
    } catch (e) {
      harnessRunError = String(e);
      harnessStarting = null;
      stopHarnessPoll();
    }
  }
  // Sidebar nav: which group is visible. Since #129 (c) the template is one
  // chain of section COMPONENTS gated on this, so only one group is mounted at
  // a time. Default lands on 'theme' (Appearance sits at the top of the nav
  // order).
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
    | 'injection'
    | 'sandboxing'
    | 'mcp'
    | 'graph'
    | 'checks'
    | 'code-audit'
    | 'tool-plugins'
    | 'pricing'
    | 'workbench'
    | 'harness'
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
    // F-18: a top-level category of its own, labelled with the phrase the UI
    // strings and the ⛨ status chip already use ("Injection protection is
    // reduced…"), so every pointer at it is a path correction rather than a
    // rename. It used to be three headings at the bottom of Offload task tools
    // → Pool, and it governs every AI tab and the MCP surface, not just the
    // offload worker. Adjacent to `offload` deliberately: that is where anyone
    // who remembers the old layout, or read an older pointer, starts looking.
    { id: 'injection', label: 'Injection protection' },
    // V33 locked decision 16: ONE top-level category holds every sandboxing
    // setting, created before the settings scattered rather than consolidated
    // after (F-18's lesson applied ahead of the mess). Sibling to Injection
    // protection and deliberately NOT merged into it: V32 constrains a
    // compromised model at the tool layer; V33 makes the OS enforce a boundary
    // the model cannot negotiate with. Merging would let a user believe one
    // delivers the other. Membership test for anything added here: does it
    // control the boundary the OS enforces?
    { id: 'sandboxing', label: 'Sandboxing' },
    { id: 'mcp', label: 'MCP servers' },
    { id: 'graph', label: 'Code Intelligence' },
    { id: 'checks', label: 'Checks' },
    { id: 'code-audit', label: 'Code Audit' },
    // V38: adjacent to Code Audit because it is the same job one level up —
    // Code Audit configures the fourteen scanners cImp ships knowing about,
    // this configures the ones a user dropped into `plugins/`. Anyone looking
    // for "where do I point cImp at a tool" looks at one of the two, so they
    // sit together rather than one being filed under Advanced.
    { id: 'tool-plugins', label: 'Tool Plugins' },
    { id: 'pricing', label: 'LLM pricing' },
    { id: 'workbench', label: 'Workbench' },
    // V35 Phase G, following the same rule Sandboxing was created under (V33
    // decision 16) and for the reason F-18 taught: a top-level category of its
    // own, named exactly as the milestone and the docs name it, so every
    // pointer at it is findable. It is deliberately NOT a sub-tab of Code
    // Intelligence or Tabs — the rows it shows govern the transcript readers,
    // the status line, the hooks, the out-of-band tap AND the native-tool gate, so
    // burying it under any one consumer would misdescribe its scope. Adjacent
    // to Advanced because it is a status board, not a set of knobs: it is
    // entirely read-only apart from one button.
    { id: 'harness', label: 'Harness health' },
    { id: 'advanced', label: 'Advanced' },
    { id: 'about', label: 'About' },
  ];

  // Sub-tab nav within the Tabs section. Each AI builtin gets its own
  // sub-tab; every Shell tab is grouped under 'shells'. Keeps the
  // previously-collapsible <details> wall navigable.
  type TabsSubSection = AiTabId | 'shells';
  /// The reserved AI tab ids, in canonical order, from the registry (V40 Phase
  /// F, locked decision 7). Empty until `harness_list` answers — every consumer
  /// below iterates it, so a window that opened a frame early renders no AI tab
  /// sections rather than sections for a roster it guessed.
  const aiTabIds = $derived(reservedAiTabIds($harnesses));
  /// Whether the roster has actually ARRIVED (V40 review findings F-2/F-3).
  ///
  /// `aiTabIds` above is deliberately non-empty before it does —
  /// `reservedAiTabIds` carries the bootstrap fallback locked decision 7
  /// sanctions — so the comment that used to sit here, claiming this window
  /// "renders no AI tab sections rather than sections for a roster it guessed",
  /// was false. What it rendered was three enable checkboxes with NO label
  /// (`labelForTabId` had no fallback) and three blank sub-tab buttons; ticking
  /// one of those unlabelled boxes kills that tab's PTY and drops its
  /// scrollback. Every block that renders per reserved tab id gates on this and
  /// shows a loading — or, since `loadHarnesses` retries and reports, a
  /// FAILED — state instead.
  const rosterReady = $derived($harnesses.length > 0);
  /// The shipped roster, as copy. Every sentence that used to enumerate the
  /// harnesses by hand ("restart the X/Y tab") interpolates one of
  /// these, so what a user reads is the roster the app actually has (V40 Phase
  /// F, locked decision 7).
  const harnessNames = $derived(harnessLabels($harnesses));
  const harnessNamesProse = $derived(harnessLabelsProse($harnesses));
  /// Where each harness keeps its own state, for the sandbox copy.
  const harnessStateDirs = $derived(
    $harnesses.flatMap((h) => h.affordances.stateDirs).join(', '),
  );
  /// The sub-tab the Tabs section opens on: the first reserved tab there is.
  /// `''` while the roster is loading, which is one paint at most.
  let tabsSubSection = $state<TabsSubSection>('');
  $effect(() => {
    if (tabsSubSection === '' && aiTabIds.length > 0) tabsSubSection = aiTabIds[0];
  });
  function subSectionForTabId(tabId: string): TabsSubSection {
    return aiTabIds.includes(tabId) ? tabId : 'shells';
  }

  // ── Section view state the WINDOW owns (V42 tranche-2 review, T2-5) ──────
  //
  // The sidebar renders one section at a time inside an `{#if}`, so switching
  // sections DESTROYS the component and takes its `$state` with it. Before
  // #129 (c) split this file up, every field below was the monolith's own and
  // therefore lived as long as the window; the split moved them into the
  // children by default, which quietly turned "come back to it later" into
  // "start again". These are the fields where that is visible — a typed
  // prompt, a probe result, a half-filled dialog, a chosen sub-tab — so they
  // stay here and reach their section as props, exactly as `tabsSubSection`
  // above already does. Everything else a section keeps is genuinely per-mount
  // (in-flight busy flags, transient inline messages) and stays there.
  /// The Appearance section's inline save-preset dialog, as one value: it is
  /// opened, filled and validated together, so it is carried together.
  type ThemeSavePreset = { open: boolean; name: string; error: string | null };
  type OffloadSubSection = 'pool' | 'tools';
  let offloadSubSection = $state<OffloadSubSection>('pool');
  /// The offload test box: what the user typed, and the last answer.
  let offloadTestInput = $state('');
  let offloadTestResult = $state('');
  type GraphSubSection = 'graph' | 'semantic' | 'efficiency' | 'viz';
  let graphSubSection = $state<GraphSubSection>('graph');
  /// The Appearance section's inline save-preset dialog: open, the name being
  /// typed, and the validation message under it.
  let themeSavePreset = $state<ThemeSavePreset>({ open: false, name: '', error: null });
  /// Which plugin the Tool Plugins detail pane shows.
  let pluginSelected = $state<string | null>(null);
  /// Per-tool Detect results, keyed by tool key. A probe is an IPC round trip
  /// the user asked for; losing its answer to a sidebar click is the most
  /// expensive of these to re-earn.
  let pluginDetect = $state<Record<string, AuditDetectResult | 'probing' | undefined>>({});

  // Keep `snapshot` in sync with the global store. Every input mutates
  // `snapshot` and pushes via `applySettings`; the broadcast comes back and
  // overwrites `snapshot`.
  //
  // That overwrite is NOT unconditional any more. `settings_update` is a
  // wholesale replace and `settings-changed` is asynchronous, so a burst of
  // edits used to lose one: the echo of an earlier push replaced the draft with
  // a state missing a newer edit, and the next `patch()` cloned the regressed
  // draft and pushed it — erasing that edit from the backend too (observed live
  // on a machine-wide tool path). `draftSync` gates the overwrite on "no push of
  // ours in flight" and replays the last suppressed broadcast once the burst
  // ends; see `lib/settings/draftSync.ts` for the full rule and its residual.
  const draftSync = createDraftSync<Settings>((s) => {
    if (disposed) return;
    snapshot = structuredClone(s);
  });
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
  /// #48 (F-x): the `ai-tab-restart-hint` listener, now that the backend emits
  /// to this window too.
  let unlistenRestartHint: (() => void) | undefined;
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
    // V40 Phase F (locked decision 7): the registry — ids, labels, reserved tab
    // ids, features, affordances and each harness's declared settings. Fetched
    // once, since it is `'static` backend data, and read by the per-harness
    // form, the enable checkboxes, the sub-tab nav, the two exposure lists and
    // the MCP access boxes, so none of them re-declares the roster.
    const roster = loadHarnesses();
    startBackendStatusPolling();
    void refreshPlugins();
    pluginsProjectKey()
      .then((k) => (pluginProjectKey = k))
      // A missing key is not fatal: every per-project path control gates on
      // it, so the pane degrades to machine-wide paths only rather than
      // writing overrides under an empty key that nothing would ever read.
      .catch((e) => console.warn('plugins_project_key failed', e));
    snapshot = structuredClone(get(settings));
    for (const t of aiTabIds) captureBaseline(t);
    injectionAppBaseline = injectionAppShape(snapshot);
    // **The restart baselines are re-taken once the roster is in** (V40 review
    // finding F-1). One cell of `injectionAppShape` — the harness-scoped
    // native-tool gate — is resolved through `harness_list`, and
    // `spawnBakedInjectionL2` reads that roster with a non-reactive `get()`
    // inside a rune, so the `$derived` does NOT re-run when it arrives: it
    // re-runs on the user's first edit, against a baseline captured when the
    // answer was unknown, and the section's "AI tabs launch differently —
    // restart them" hint fires with no user change behind it. Re-taken from the
    // SAME snapshot, and only while the user has not touched it, so this can
    // never absorb an edit made in the gap.
    const preRoster = JSON.stringify(snapshot);
    await roster;
    if (disposed) return;
    if (JSON.stringify(snapshot) === preRoster) {
      for (const t of aiTabIds) captureBaseline(t);
      injectionAppBaseline = injectionAppShape(snapshot);
    }
    unsub = settings.subscribe((s) => {
      draftSync.broadcast(s);
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
    // V35 Phase G: this now also fills the Harness health panel. If a run is
    // already in flight when the window opens (an automatic version-change
    // check, say), start watching for it immediately rather than showing a
    // stale board until the user clicks something.
    void refreshHarness().then(() => {
      if (harnessFresh?.verify_in_flight) startHarnessPoll();
    });
    for (const t of aiTabIds) {
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
    const deepLinkUnlisten = await listenEvent(
      SETTINGS_DEEP_LINK,
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

    // #48 (F-x): the backend's spawn-injection edge hint. It used to be emitted
    // to the main window only — a toast the user never sees, because they are
    // standing HERE when they flip the switch that raised it. Rendered as the
    // per-tab restart hint the Tabs section already has, so the affordance the
    // hint points at (a Restart button) is the same one it appears beside.
    const restartHintUnlisten = await listenEvent(AI_TAB_RESTART_HINT, (e) => {
      const tabs = new Set(spawnStaleTabs);
      for (const c of e.payload ?? []) for (const t of consumerTabs(c)) tabs.add(t);
      spawnStaleTabs = [...tabs];
    });
    if (disposed) {
      restartHintUnlisten();
      return;
    }
    unlistenRestartHint = restartHintUnlisten;
  });

  onDestroy(() => {
    disposed = true;
    unsub?.();
    unlistenDeepLink?.();
    unlistenRestartHint?.();
    if (backendStatusTimer) clearInterval(backendStatusTimer);
    // V35 Phase G: the harness-verify watcher. The backend run keeps going —
    // it is a detached thread that writes through the settings file — but this
    // window stops asking about it.
    stopHarnessPoll();
  });

  /// Mutate the live snapshot via `updater`, then push to the backend.
  /// Backend's debounced save coalesces rapid calls (slider drags).
  ///
  /// `pushDraft` registers the push with `draftSync` for as long as it is in
  /// flight, so a `settings-changed` broadcast that lands mid-burst cannot
  /// replace the draft with a state that is missing an edit this window just
  /// made — the lost-update race. It also settles the promise in either
  /// direction rather than discarding it, which is what reopens the gate; see
  /// `lib/settings/draftSync.ts` for why those two halves are one call.
  function patch(updater: (s: Settings) => void) {
    if (!snapshot) return;
    const next = structuredClone($state.snapshot(snapshot));
    updater(next);
    snapshot = next;
    void pushDraft(draftSync, next);
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
      // tab-bar order users see. The order is the registry's declaration order
      // flattened through each descriptor's `tab_ids` (V40 Phase F).
      next_ids = aiTabIds.filter((x) => prev.includes(x) || x === id);
    } else {
      if (prev.length <= 1) return; // last-one lock (also guarded by the disabled attribute)
      next_ids = prev.filter((x) => x !== id);
    }
    if (!enable && tabsSubSection === id) {
      // Jump to the first surviving id in canonical order.
      const survivor = aiTabIds.find((x) => next_ids.includes(x));
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
      // The backend rejects enabling a harness tab whose CLI can't be resolved
      // (not in ebin, not on PATH) — surface that specifically so the user knows
      // to install it; everything else is a generic failure. V40 Phase E: the
      // harness label and the install hint come from the refusal (locked
      // decision 26), so this sentence names no product.
      const err = e as { kind?: string; label?: string; hint?: string } | null;
      aiTabsError =
        err?.kind === 'harness-not-found'
          ? `${err.label ?? 'That harness'} was not found in ebin or on your PATH. ${err.hint ?? ''} — then try again.`.replace(
              ' —  — ',
              ' — ',
            )
          : 'Failed to update AI tabs — see logs for details.';
    }
  }


  // Restart-affecting subset: command + args + cwd + env + the custom-provider
  // flag (it synthesizes the harness's provider env at launch). Since issue
  // #109 that flag is tab-determined and has no control, so it never differs
  // from the baseline in practice — it stays in the shape because the value is
  // still on the wire, and a hand-edited settings file can still move it.
  // Notifications,
  // first_launch_notice_dismissed, and (V20) the tts_injection speak gate
  // apply live and are excluded — the out-of-band TTS source reads the toggle
  // per-utterance, so flipping it takes effect without relaunching the tab.
  function restartShape(t: AiToolTabConfig) {
    return {
      command: t.command,
      args: t.args,
      cwd: t.cwd,
      env: t.env,
      use_local_provider: t.use_local_provider,
      // V32 Phase G: some of this tab's injection overrides are SPAWN-BAKED, so
      // flipping one needs the tab restarted before it means anything. The
      // others resolve per call and are deliberately excluded — a restart hint
      // for a change that takes effect immediately is how a hint stops being
      // read. This mirrors `spawn_inject_sig`'s split on the backend.
      //
      // WHICH ones is not spelled here (#48, F-27 second instance): this used to
      // be a hand-written trio and it went stale the moment `spotlighting`
      // became spawn-baked (M-3), which left this window silent about a flip
      // every running tab was carrying the old answer for. One list, in
      // `SPAWN_BAKED_INJECTION_FEATURES`, read by both this and
      // `injectionAppShape` below.
      injection_spawn_baked: spawnBakedTabOverrides(t.injection_overrides),
    };
  }

  /// The APP-WIDE spawn-baked injection cells: L1 plus the app-wide L2 input of
  /// every spawn-baked feature, the same set the hierarchy feeds into
  /// `spawn_inject_sig`.
  ///
  /// `restartShape` above covers a TAB's L3 cells and nothing else (#48, F-x), so
  /// flipping the master switch — or any spawn-baked feature at L2 — raised no
  /// hint in this window at all, even though every one of them moves the backend
  /// signature and therefore every running tab's posture. Section-level rather
  /// than per-tab because that is what they are: they affect all tabs at once.
  ///
  /// The feature cells come from `spawnBakedInjectionL2`, not from a list written
  /// out here (#48, F-27 second instance): the hand-written version omitted
  /// `spotlighting_enabled` from the day it became spawn-baked (M-3). L1 is
  /// prepended rather than joining that list because it is not a feature — it is
  /// the master above all of them, and it reaches every launch there is.
  function injectionAppShape(s: Settings | null): string {
    if (!s) return '';
    return JSON.stringify([
      s.offload.injection.protection,
      ...spawnBakedInjectionL2(s),
    ]);
  }

  /// Captured when this window opens (and again whenever a tab is restarted
  /// from here — the natural "you have acted on it" moment). There is no way to
  /// know from Settings that every AI tab has been restarted, so the hint
  /// deliberately errs toward staying visible rather than clearing itself.
  let injectionAppBaseline = $state<string>('');
  const injectionAppRestartRequired = $derived(
    injectionAppBaseline !== '' && injectionAppShape(snapshot) !== injectionAppBaseline,
  );

  /// Tabs the BACKEND has told us launch differently now — the payload of
  /// `ai-tab-restart-hint`, expanded from consumer names to this window's tab
  /// ids (#48, F-x: that event used to reach the main window only, which is the
  /// one place the user is NOT standing when they change a setting here).
  ///
  /// This is the wider signal of the two: it covers every spawn-baked input,
  /// not just the injection hierarchy — MCP server exposure, the guidance
  /// addendum, the status-line overlay, whatever local-provider config a
  /// harness writes. Cleared per tab when that tab is restarted from here.
  let spawnStaleTabs = $state<string[]>([]);

  /// The reserved tabs a consumer token's harness owns — the tabs an exposure
  /// flag for that consumer actually reaches.
  ///
  /// V40 Phase F: a registry lookup. It used to be a two-arm branch whose
  /// `else` handed every unrecognised consumer one harness's tabs, so a third
  /// harness's exposure row would have listed somebody else's tabs as its own.
  /// An unregistered token now owns no tabs, which is the honest answer.
  function consumerTabs(consumer: string): AiTabId[] {
    return $harnesses.find((h) => h.consumer === consumer)?.tab_ids ?? [];
  }

  const restartRequired = $derived.by(() => {
    const out: Record<string, boolean> = {};
    if (!snapshot) return out;
    for (const t of aiTabIds) {
      const baseline = tabBaselines[t];
      const live = aiTabFromSnapshot(t);
      const backendStale = spawnStaleTabs.includes(t);
      if (!baseline || !live) {
        out[t] = backendStale;
        continue;
      }
      out[t] =
        backendStale ||
        JSON.stringify(restartShape(live)) !== JSON.stringify(restartShape(baseline));
    }
    return out;
  });

  async function restartTab(tab: AiTabId) {
    await requestTabRestart(tab);
    captureBaseline(tab);
    spawnStaleTabs = spawnStaleTabs.filter((t) => t !== tab);
    // The app-wide injection cells are baked into whatever launches next, so a
    // restart here is also the moment this window's section-level hint has been
    // acted on for at least one tab. Re-baseline only when nothing else is
    // still stale, so the hint outlives a partial restart.
    if (spawnStaleTabs.length === 0) injectionAppBaseline = injectionAppShape(snapshot);
  }

  /// Hard reset: replaces the live Settings struct with the canonical
  /// defaults from `defaultSettings()`. Wipes user-created shell tabs,
  /// saved layouts, shortcut overrides, etc. — fully destructive, so
  /// gated on a native `confirm()` prompt.
  ///
  /// Shaped exactly like `patch()`, and for the same reason (#129). The reset
  /// used to push the defaults and leave `snapshot` alone, letting the echo
  /// bring the draft down — which left a window in which the draft still held
  /// the PRE-reset settings: an edit made in that window cloned the stale
  /// draft and pushed it wholesale, resurrecting everything the user had just
  /// reset. So the draft is assigned here, immediately, and the push goes
  /// through `pushDraft` like every other one.
  ///
  /// `defaultSettings()` already hands back a fresh deep clone per call, so
  /// the assign-and-push pair aliases one private object exactly as `patch()`
  /// does with its `structuredClone` — no second clone is needed, and nothing
  /// else holds a reference through which to mutate it under the push.
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
    const next = defaultSettings();
    snapshot = next;
    void pushDraft(draftSync, next);
  }

  // ── Code Audit ─────────────────────────────────────────────────────────
  //
  // What is left here is the FEATURE's settings. The fourteen scanners
  // themselves are configured in the Tool Plugins section, because since V38
  // they are a plugin — one whose manifests cImp ships rather than one you drop
  // in a folder, but a plugin, rendered by the pane that already knows how.

  // The latest scan's language census, read from the runner so the
  // auto-selection button can apply the project's languages immediately rather
  // than waiting for the next census refresh. Empty (both lists) before any
  // scan.
  let auditCensus = $state<AuditCensus>({ extensions: [], markers: [] });

  // A manual edit of a BUILT-IN QUALITY tool's checkbox switches auto-selection
  // to manual mode, so the choice sticks across census refreshes instead of
  // being re-derived at the next scan. Only for that population: a user
  // plugin's tool is never auto-selected, so toggling one says nothing about
  // the mode.
  function noteManualToolEdit(pluginKey: string, toolId: string): void {
    if (pluginKey !== AUDIT_PLUGIN_KEY) return;
    if (qualityAutoSelection(auditCensus).some((t) => t.id === toolId)) {
      patch((s) => (s.code_audit.quality_auto_select = false));
    }
  }

  // ── V38 Tool Plugins ───────────────────────────────────────────────────
  // The section itself is `sections/ToolPluginsSection.svelte` (#129 (c)). What
  // stays here is the LOAD: the loader's set is read ONCE on mount (a read of
  // already-scanned state, never a disk walk) together with the project key,
  // and refreshed only by the section's explicit Rescan, which calls back into
  // `refreshPlugins`. Keeping the fetch here is the issue's behaviour
  // invariant — a load moved into the child would fire on first VIEW instead.
  let pluginSet = $state<PluginSet | null>(null);
  // The key this project's per-tool path overrides are stored under. Asked of
  // the backend rather than derived here: canonicalizing a path touches the
  // disk, and a second spelling rule would silently stop matching the first.
  let pluginProjectKey = $state('');
  let pluginRescanning = $state(false);
  let pluginLoadError = $state<string | null>(null);

  async function refreshPlugins(rescan = false): Promise<void> {
    pluginLoadError = null;
    if (rescan) pluginRescanning = true;
    try {
      pluginSet = rescan ? await pluginsRescan() : await pluginsSnapshot();
    } catch (e) {
      pluginLoadError = `Could not read the plugins folder: ${e}`;
    } finally {
      pluginRescanning = false;
    }
  }

</script>


{#if useCustomTitleBar}
  <TuiTitleBar title="cImp settings" />
{/if}
{#if !snapshot}
  <div class="loading">Loading settings…</div>
{:else}
  <!-- `settings-chrome` keys `lib/settings/settings-chrome.css`. It sits on
       `.root` rather than on `<html>` so the sheet cannot reach TuiTitleBar or
       the `.loading` splash, both of which render outside this element. -->
  <div class="root settings-chrome">
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
        <AudioSection {snapshot} {patch} {voices} />
      {:else if activeSection === 'stt'}
        <SttSection {snapshot} {patch} models={sttModels} devices={inputDevices} />

      {:else if activeSection === 'avatar'}
        <AvatarSection {snapshot} {patch} />
      {:else if activeSection === 'theme'}
        <ThemeSection
          {snapshot}
          {patch}
          savePreset={themeSavePreset}
          onsavepreset={(next) => (themeSavePreset = next)}
        />
      {:else if activeSection === 'bottom-bar'}
        <BottomBarSection {snapshot} {patch} />
      {:else if activeSection === 'tabs'}
        <TabsSection
          {snapshot}
          {patch}
          {aiTabIds}
          {rosterReady}
          {tabDefaults}
          {restartRequired}
          {aiTabsError}
          subSection={tabsSubSection}
          onsubsection={(id) => (tabsSubSection = id as TabsSubSection)}
          onrestart={(tab) => void restartTab(tab)}
          ontoggleenabled={(id, enable) => void toggleAiTabEnabled(id, enable)}
        />
      {:else if activeSection === 'shortcuts'}
        <ShortcutsSection {snapshot} {patch} />
      {:else if activeSection === 'compose'}
        <ComposeSection
          globals={globalTemplates}
          projects={projectTemplates}
          loading={composeTemplatesLoading}
          dirty={composeTemplatesDirty}
          error={composeTemplatesError}
          onglobals={setGlobalTemplates}
          onsave={() => void saveGlobalTemplates()}
        />
      {:else if activeSection === 'offload'}
        <OffloadSection
          {snapshot}
          {patch}
          {backendStatuses}
          {harnessNames}
          subSection={offloadSubSection}
          onsubsection={(id) => (offloadSubSection = id as OffloadSubSection)}
          testInput={offloadTestInput}
          testResult={offloadTestResult}
          ontestinput={(v) => (offloadTestInput = v)}
          ontestresult={(v) => (offloadTestResult = v)}
          onenablereadonly={() => void enableReadonlyCommands()}
          onnavigate={(s) => (activeSection = s as SectionId)}
        />
      {:else if activeSection === 'injection'}
        <InjectionSection
          {snapshot}
          {patch}
          {detection}
          {injection}
          {detectionBusy}
          appRestartRequired={injectionAppRestartRequired}
          onreloadrules={() => void reloadDetection()}
          oncheckupdate={(c, apply) => void checkDetectionUpdate(c, apply)}
          onrevert={(c) => void revertDetection(c)}
        />
      {:else if activeSection === 'mcp'}
        <McpSection
          {snapshot}
          {patch}
          {harnessNamesProse}
          health={serviceStatus?.mcp_servers ?? []}
          onedit={setMcpRegistry}
          onapply={applyMcpRegistry}
        />
      {:else if activeSection === 'graph'}
        <GraphSection
          {snapshot}
          {patch}
          {localOffloadReady}
          {e1Gate}
          subSection={graphSubSection}
          onsubsection={(id) => (graphSubSection = id as GraphSubSection)}
          statuses={graphStatuses}
          busy={graphBusy}
          onrefresh={() => void refreshGraphStatus()}
          onrebuild={() => void runGraphRebuild()}
          onignorepick={(folder) => void addGraphIgnorePick(folder)}
          oncommitignore={commitGraphIgnore}
        />
      {:else if activeSection === 'checks'}
        <ChecksSection {snapshot} {patch} {harnessNamesProse} />
      {:else if activeSection === 'code-audit'}
        <CodeAuditSection
          {snapshot}
          {patch}
          {auditCensus}
          {harnessNames}
          onnavigate={(s) => (activeSection = s as SectionId)}
        />
      {:else if activeSection === 'tool-plugins'}
        <ToolPluginsSection
          {snapshot}
          {patch}
          {pluginSet}
          {pluginProjectKey}
          rescanning={pluginRescanning}
          loadError={pluginLoadError}
          selected={pluginSelected}
          detect={pluginDetect}
          onselected={(key) => (pluginSelected = key)}
          ondetect={(next) => (pluginDetect = next)}
          onrescan={() => void refreshPlugins(true)}
          onmanualtooledit={noteManualToolEdit}
        />
      {:else if activeSection === 'pricing'}
        <PricingSection
          rows={llmPricing}
          loading={llmPricingLoading}
          dirty={llmPricingDirty}
          error={llmPricingError}
          onrows={setLlmPricingRows}
          onsave={() => void saveLlmPricing()}
        />
      {:else if activeSection === 'sandboxing'}
        <SandboxingSection {snapshot} {patch} {harnessNames} {harnessStateDirs} />
      {:else if activeSection === 'harness'}
        <HarnessSection
          health={harnessFresh}
          busy={harnessBusy}
          starting={harnessStarting}
          runError={harnessRunError}
          onrun={(h) => void runHarnessChecks(h)}
        />
      {:else if activeSection === 'workbench'}
        <WorkbenchSection {snapshot} {patch} />
      {:else if activeSection === 'advanced'}
        <AdvancedSection {snapshot} {patch} onreset={resetSettingsToDefaults} />
      {:else if activeSection === 'about'}
        <AboutSection />
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
  /* No width cap: a horizontally enlarged window widens every section and
     its inputs instead of leaving a fixed 720px column centred in space. */
  .inner {
    width: 100%;
  }
  .loading {
    padding: var(--space-6);
    text-align: center;
    color: var(--text-tertiary);
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
