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
  import { open } from '@tauri-apps/plugin-dialog';
  import {
    initSettings,
    settings,
    applySettings,
  } from './lib/settings/store';
  import { createDraftSync } from './lib/settings/draftSync';
  import {
    aiToolTabDefaults,
    auditDetectTool,
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
  import { listen } from '@tauri-apps/api/event';
  import type {
    AiToolTabConfig,
    AuditDetectResult,
    CapabilityHealth,
    HarnessHealth,
    HarnessStatus,
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
    CONTROL_READ_ADVISOR,
    OUTCOME_NO_FAILURE,
    controlBlocked,
    // V32 / #48 F-27: the ONE list of spawn-baked injection features, read by
    // both restart-hint shapes below.
    spawnBakedInjectionL2,
    spawnBakedTabOverrides,
    toPresetConfig,
    // V40 Phase B: the per-harness settings map. `harnessRow` answers the
    // declared defaults for a key the file has never carried, so no control
    // here has to know whether a harness has ever been saved.
    harnessRow,
    setHarnessExt,
  } from './lib/settings/types';
  import type { HarnessInfo, SettingFieldView } from './lib/harness';
  import {
    findHarnessByTabId,
    harnesses,
    harnessLabels,
    harnessLabelsProse,
    harnessLoadState,
    labelForTabId,
    loadHarnesses,
    reservedAiTabIds,
  } from './lib/harness';
  import HarnessExtForm from './lib/settings/HarnessExtForm.svelte';
  import { contentClear, contentOpenFolder, setEnabledAiTabs } from './lib/ipc';
  import { listSttModels, listInputDevices } from './lib/stt';
  import {
    offloadTest,
    offloadDeriveLocalProvider,
    offloadStatuses,
    offloadBackendStart,
    offloadBackendStop,
    offloadBackendRestart,
    describeBackendStatus,
    offloadServiceStatus,
    offloadReloadMcp,
    offloadEnableReadonlyCommands,
    detectionStatus,
    detectionCheckNow,
    detectionRevert,
    detectionOpenRulesFolder,
    type BackendStatus,
    type ServiceStatus,
    type DetectionStatus,
  } from './lib/offload';
  import { graphIgnorePick, graphRebuild, graphStatus, type GraphStatus } from './lib/graph';
  // V32 Phase G: the resolved enable hierarchy, from the backend's one resolver.
  import { fetchInjectionStatus, type InjectionStatus } from './lib/latch';
  import ArrayEditor from './lib/settings/ArrayEditor.svelte';
  import type {
    OffloadBackend,
    ToolScope,
    BackendTier,
    CommandPolicy,
    ServerCommandTemplate,
    RemoteBackendTemplate,
    PromptTemplate,
    LlmPricingModel,
    InjectionSettings,
  } from './lib/settings/types';
  import {
    composeTemplatesGlobalGet,
    composeTemplatesGlobalSet,
    composeTemplatesProjectGet,
  } from './lib/compose/templates';
  import {
    HARNESS_NATIVE_GATE_KEY,
    localDataExcludedScope,
    toolScopeMode,
  } from './lib/settings/types';
  // V39 Phase C: the facade backends, derived from the tab roles this window
  // already holds — never fetched, so the list cannot disagree with the role
  // radio in the same settings snapshot.
  import { facadeBackends } from './lib/delegation';
  import type { AiTabId } from './lib/tabs/types';
  import { SPRITE_SETS } from './lib/avatarConfig';
  import { version as appVersion } from '../package.json';
  import NumberField from './lib/settings/NumberField.svelte';
  import SelectField from './lib/settings/SelectField.svelte';
  import Toggle from './lib/settings/Toggle.svelte';
  import ShortcutCapture from './lib/settings/ShortcutCapture.svelte';
  import TabSettingsSection from './lib/settings/TabSettingsSection.svelte';
  import ThemeSwatch from './lib/settings/ThemeSwatch.svelte';
  import TuiTitleBar from './lib/TuiTitleBar.svelte';
  import CustomThemeEditor from './lib/settings/CustomThemeEditor.svelte';
  import BackgroundConfigEditor from './lib/settings/BackgroundConfigEditor.svelte';
  import ChecksEditor from './lib/settings/ChecksEditor.svelte';
  import { formatDetect } from './lib/settings/codeAudit';
  // V37 Phase D: the MCP-servers section's body, extracted (contract C8).
  import McpManagementEditor from './lib/settings/McpManagementEditor.svelte';
  import type { McpRegistry } from './lib/settings/mcpEditor';
  import {
    AUDIT_PLUGIN_KEY,
    errorRows,
    permissionsOpen,
    pluginDisplayLabels,
    pluginRows,
    revertToGlobalPath,
    setCategoryEnabled,
    setGlobalPath,
    setPluginEnabled,
    setProjectPath,
    setToolEnabled,
    setToolParameters,
    setToolTimeout,
    setToolVariable,
    shouldAutoFill,
    siblingAutoFillTargets,
    type PluginRow,
    type PluginSet,
    type ToolRow,
  } from './lib/settings/toolPlugins';
  import { auditRefreshCensus } from './lib/codeAudit/ipc';
  import { censusIsEmpty, qualityAutoSelection } from './lib/codeAudit/logic';
  import type { AuditCensus } from './lib/codeAudit/types';
  import { resolveBundledTheme, defaultPalette } from './lib/themes';
  import { themeRegistry, paletteRegistry } from './lib/themes/registry';
  import {
    TUI_THEME_ID,
    TUI_ACCENT_PRESETS,
    normalizeTuiAccent,
    normalizeHexColor,
    DEFAULT_LATCHED_COLOR,
    DEFAULT_CONTAMINATED_COLOR,
  } from './lib/themes/accent';
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
  // V1.4-07's `showLocalToken` moved into `HarnessExtForm` with the input it
  // toggled: the Show/Hide button is now driven by the declaration's `secret`
  // column, so every plugin's credentials get it and none of them needs a flag
  // in this file.
  // Inline error under the AI-tabs checkbox group — e.g. when enabling an
  // an AI tab is rejected because its harness binary isn't installed
  // (ebin/PATH).
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

  // V21: register the given Local backend as `harness`'s local provider.
  // Derives base URL + model from its server command in Rust (which errors,
  // naming the missing --port/model flag, when the command is incomplete), then
  // persists the snapshot so a freshly opened tab of that harness is ready to
  // use. Overrides any existing registration; `providerMsg` reports
  // success/failure inline.
  //
  // V40 Phase F (locked decision 26/27): the harness is passed rather than
  // assumed. The button is mounted by the `local_provider_config` feature, so
  // the harness whose writer runs is the one whose button was clicked — with
  // two such harnesses the backend would otherwise have refused, asking which.
  let providerMsg = $state<{ i: number; text: string; ok: boolean } | null>(null);
  async function registerLocalProvider(i: number, h: HarnessInfo): Promise<void> {
    const backend = snapshot?.offload.backends[i];
    const blockKey = h.affordances.localProviderConfigBlockKey;
    if (!backend || backend.kind.type !== 'local' || !blockKey) return;
    providerMsg = null;
    try {
      const provider = await offloadDeriveLocalProvider(h.id, backend.kind.server_command);
      // V40 Phase B: the derived block is that plugin's own `ext` row
      // (`SettingKind::Json` — written by cImp, never typed), not a field in
      // the offload block named after a harness. V40 review F-6: under the key
      // the plugin DECLARES, not the one this file used to spell.
      patch((s) => setHarnessExt(s, h.id, blockKey, provider));
      providerMsg = {
        i,
        ok: true,
        text: `Registered ${provider.model} at ${provider.base_url}. New tabs of that harness will use it by default.`,
      };
    } catch (e) {
      providerMsg = { i, ok: false, text: `${e}` };
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

  /// Whether the detection updater may run at all — **the backend's own
  /// `updates_enabled`, read verbatim** (#48, M-21).
  ///
  /// It used to be re-derived here from the resolved-scope matrix (`app` scope +
  /// `detection`), which is the same conjunction and was correct — but it was a
  /// SECOND reading of the question the two IPC commands enforce with the first,
  /// assembled from a different poll's payload. Two predicates for one gate can
  /// drift; one cannot. `updates_allowed` in `ipc/commands.rs` and this line now
  /// resolve through the same `updater::updates_enabled`, so a greyed button and
  /// a served command cannot disagree about the state, only about the moment.
  ///
  /// Defaults to `true` while the first poll is in flight — and if a build ever
  /// omits the field: this only disables buttons, and the enforcement is in the
  /// IPC command, which refuses with the sentence below. Greying out three
  /// controls for a second at startup would be the more visible bug.
  const detectionUpdatesEnabled = $derived(detection?.updater.updates_enabled ?? true);

  /// WHY the updater is inert, or `undefined` when it is not (#48, M-21).
  ///
  /// One string, rendered on the button row, on all three buttons and in the
  /// prose above them, because those four used to carry four hand-written copies
  /// of a single claim — *"injection detection is off"* — and that claim is FALSE
  /// in one of the three states that produce it. A worker-only override leaves
  /// this updater off while the offload worker is still screening every fetched
  /// page with the bundle on disk; telling that user their detection is switched
  /// off is a false statement about a running security layer, and it is the one
  /// they would act on.
  ///
  /// **Reporting only.** Nothing here decides anything: `detectionUpdatesEnabled`
  /// alone gates the controls, and the IPC commands refuse independently. The
  /// sentences deliberately mirror `ipc::commands::updates_allowed`'s two
  /// refusals — the same state must not get two different explanations depending
  /// on whether the user read a tooltip or clicked and got an error.
  ///
  /// The three states, in the order they are checked:
  /// 1. `worker_only_detection` — off here, ON in the worker. Backend-published,
  ///    never inferred: absent (an older backend) reads `false`, which serves the
  ///    generic sentence rather than claiming a layer is running.
  /// 2. the L1 master is off, which resolves detection off with it. Claimed only
  ///    when `injection` has actually been read — an unread hierarchy falls
  ///    through to (3), whose parenthetical covers the master either way.
  /// 3. detection is off app-wide and nowhere else is running it. The backend's
  ///    own wording for exactly this branch.
  const detectionUpdatesOffReason = $derived.by((): string | undefined => {
    if (detectionUpdatesEnabled) return undefined;
    if (detection?.updater.worker_only_detection === true) {
      return (
        'Injection detection is switched off app-wide and for every AI tab, so nothing is ' +
        'polled or swapped — not on the daily schedule and not from these buttons. It is ' +
        'still switched ON for the offload worker, which keeps screening with the rule ' +
        'bundle already on disk: the updater follows the app-wide answer, and one worker ' +
        'override does not start it. To keep that bundle current, turn injection detection ' +
        'back on app-wide above.'
      );
    }
    if (injection?.protection === false) {
      return (
        'Injection protection is switched off at the master switch above, which resolves ' +
        'injection detection off with it — so nothing is polled or swapped, not on the ' +
        'daily schedule and not from these buttons. Turn the master switch, and injection ' +
        'detection under it, back on.'
      );
    }
    return (
      'Injection detection is switched off, so nothing is polled or swapped — not on the ' +
      'daily schedule and not from these buttons. Turn it (and the injection-protection ' +
      'master above it) back on.'
    );
  });

  /// The per-feature copy this window still owns: the hint text, and the L2
  /// settings key when it cannot be derived.
  ///
  /// **Everything else now comes from the backend's report** (#48, F-y). This
  /// table used to carry eleven literal rows duplicating each feature's key,
  /// label, `spawnBaked` and scope predicates — a hand-kept mirror of
  /// `Feature::ALL`, `label()`, `spawn_baked()` and `has_tab_scope()` /
  /// `has_worker_scope()`, with no drift guard. #47 made every *Rust* mirror a
  /// compile error, which quietly made this worse: the seven errors a new
  /// variant now produces all point at Rust files, so the prompt that used to
  /// sit beside a hand-edited `const ALL` array is gone and this was the only
  /// enumeration left with no signal at all. A V33 control would have shipped
  /// with a status-bar warning naming it and no checkbox here to change it.
  ///
  /// So the matrix renders from `injection.scopes`, and a feature missing from
  /// this table is missing its HINT, not its control.
  type InjectionFeatureMeta = {
    /// The L2 settings key on `offload.injection`. Omit to derive it by the
    /// `<feature>_enabled` convention every flag follows; `null` for a feature
    /// with no boolean L2 at all, whose row is then read-only.
    ///
    /// Typed as a keyof rather than a bare string so a renamed flag is a compile
    /// error here, not a silently dead checkbox. It was previously `'protection'`
    /// — the GLOBAL MASTER — as filler on the native-web row; doubly guarded and
    /// inert, but `keyof InjectionSettings` permitted it, and one regressed guard
    /// would have made that checkbox toggle L1.
    field?: keyof InjectionSettings | null;
    hint: string;
  };

  const INJECTION_FEATURE_META: Record<string, InjectionFeatureMeta> = {
    taint_latch: {
      hint: 'Bidirectional mutual exclusion between external (web/MCP) tools and local file/source-text tools, per task and per tab session. Off: no latching, no refusals, and the offload worker advertises its whole tool surface all run.',
    },
    spotlighting: {
      hint: 'Wraps every external tool result and every recalled memory in nonced data-not-instructions markers. Off: results arrive as raw text, with no standing instruction around them.',
    },
    detection: {
      hint: 'Parent of the signature and classifier layers below — off here disables both regardless of their own toggles.',
    },
    ssrf_guard: {
      hint: 'Screens every outbound fetch URL against the private/loopback/link-local ranges before the call leaves the machine. Off: an injected page can point a fetch at your LAN.',
    },
    fetch_budgets: {
      hint: 'The on/off above the call/byte caps below. Off: neither cap applies, whatever their numbers say.',
    },
    canary: {
      hint: 'A per-task marker planted in the worker’s system context; seeing it leave in a tool argument aborts the task. Worker-only — a harness’s own system prompt is not ours to mark.',
    },
    memory_quarantine: {
      hint: 'Notes written by a conversation that has read external content are stored held-for-review instead of entering project memory. Off: they are stored normally. Notes ALREADY held stay held — turning this off never releases them.',
    },
    native_web: {
      // No boolean L2: `native_web_visibility`'s tri-mode IS this feature's
      // app-wide switch (the Phase G reconciliation), so there is no field to
      // bind and the checkbox is read-only.
      //
      // F-18's companion defect: a read-only checkbox can only say on/off, and
      // `injectionL2On` ticks it for BOTH live modes — so at `sensor`, the
      // shipped default, the row read plain "on" and a user took that to mean
      // the harness was refusing its own web tools when it never denies one.
      // Locked decision 14 makes `sensor` a posture, not a bug, so the fix is
      // to name which of the three modes is in force (rendered beside the label
      // from `nativeWebModeWord`) rather than to change the default.
      field: null,
      hint: 'Set by the Native web tools mode below, which is this feature’s app-wide switch: its "off" IS this control off, and its "sensor" — the shipped default — is this control ON but REPORT-ONLY, raising the taint badge without ever refusing a call. Only "deny" blocks anything. Use the per-tab overrides here to exempt or force one tab.',
    },
    consumer_hygiene: {
      hint: 'The pinned harness permission block and the data-not-instructions paragraph in the session guidance. Off: the harness inherits its upstream defaults and the session is never told how to read cImp’s markers.',
    },
    tool_steering: {
      hint: 'One fixed paragraph in the session guidance asking the harness to prefer the `run_check` and `run_command` MCP tools over running the same commands in its own shell. It names no check, binary or path — it points at the tools’ own enums, which update live — so editing the tool registry never changes it. The `run_command` half is written only when that tool is exposed to this consumer (Tool Plugins → advertise commands). Off: nothing is injected and the harness reaches for its shell as it would without cImp.',
    },
    [HARNESS_NATIVE_GATE_KEY]: {
      hint: 'App-wide ON since V39, like every other control here — but a newly created tab has all of its own switches OFF, so this denies nothing until you enable it for a tab (its shield badge, or the per-tab override below). It shipped app-wide OFF under locked decision 17, for the reason the sentence after this one gives; V39 kept the judgement and moved it a level down. With it on, a tab of the harness that HAS this gate, once it has read external content, is refused its OWN shell/read/edit/write/patch/glob/grep for the rest of the session (and, having gone local first, its own web tools instead). Whole-surface by design: a partial gate is routed around. Policy, not containment — it runs inside the harness’s own process, so a nested ungated copy of it, its pure mode, a user-typed !shell and the raw terminal all bypass it. A per-tab override is the usual way in; it does nothing on a tab whose harness has no native gate.',
    },
    terminal_escape_hygiene: {
      hint: 'Strips ANSI/OSC control sequences (including OSC 52 clipboard writes) out of external text cImp composes into spoken/toast output. Off: a fetched page’s escape sequences travel with the text.',
    },
  };

  /// One matrix row, composed from the backend report plus the local meta.
  type InjectionFeatureRow = {
    key: string;
    label: string;
    spawnBaked: boolean;
    /// Whether the master switch above reaches this control at all
    /// (`Feature::master_gated`). The one row that says `false` today —
    /// managed-tool steering, a token-efficiency nudge rather than a
    /// containment control — must stay EDITABLE while the master is off, or the
    /// window would show a greyed checkbox for a switch that is in force.
    masterGated: boolean;
    /// `null` ⇒ no boolean L2 to bind; the checkbox is read-only.
    field: keyof InjectionSettings | null;
    hint: string;
  };

  /// The L2 settings key for a feature: the meta table's, or the convention
  /// every flag follows. Checked against the live snapshot rather than assumed,
  /// so a convention-derived name that does not exist yields `null` (a read-only
  /// row) instead of a checkbox bound to `undefined`.
  function injectionL2Field(key: string): keyof InjectionSettings | null {
    const meta = INJECTION_FEATURE_META[key];
    if (meta && meta.field !== undefined) return meta.field;
    const derived = `${key}_enabled`;
    return snapshot && derived in snapshot.offload.injection
      ? (derived as keyof InjectionSettings)
      : null;
  }

  /// The matrix rows, in the backend's `Feature::ALL` order. Every scope reports
  /// every feature, so the first scope that has been reported is enough; the
  /// union is taken anyway so a future partial report loses nothing.
  const injectionRows = $derived.by((): InjectionFeatureRow[] => {
    const seen = new Map<string, InjectionFeatureRow>();
    for (const scope of injection?.scopes ?? []) {
      for (const f of scope.features) {
        if (seen.has(f.feature)) continue;
        seen.set(f.feature, {
          key: f.feature,
          label: f.label,
          spawnBaked: f.spawn_baked,
          masterGated: f.master_gated,
          field: injectionL2Field(f.feature),
          hint: INJECTION_FEATURE_META[f.feature]?.hint ?? '',
        });
      }
    }
    return [...seen.values()];
  });

  /// The app-wide native-web mode, normalized exactly as the backend's single
  /// reader does it (`injection::NativeWebMode::parse`): trimmed, and anything
  /// unrecognized resolves to `sensor` rather than `off` — a typo must not blind
  /// the latch. One normalizer here too, so the matrix checkbox and the mode
  /// word below cannot disagree about which mode is in force.
  const nativeWebMode = $derived.by((): 'off' | 'sensor' | 'deny' => {
    const raw = snapshot?.offload.native_web_visibility.trim() ?? '';
    return raw === 'off' ? 'off' : raw === 'deny' ? 'deny' : 'sensor';
  });

  /// F-18's companion defect, as a value: which of the three modes is in force,
  /// in words that say what it DOES. The matrix row renders this beside the
  /// feature label, because that row's checkbox is a boolean collapse of a
  /// tri-mode switch and a ticked box at `sensor` — the shipped default — was
  /// read as "the harness is blocking its web tools" when `sensor` never denies
  /// a call. Wording deliberately echoes the select's own option text in "Native
  /// web tools" below: two surfaces, one claim.
  ///
  /// A stored value that is not one of the three is NAMED rather than swallowed:
  /// it resolves to `sensor`, and a hand-edited config must not read as a mode
  /// the user chose.
  const NATIVE_WEB_MODE_WORDS: Record<'off' | 'sensor' | 'deny', string> = {
    off: 'off — no hook, no visibility',
    sensor: 'sensor — reports and taints, never denies a call',
    deny: 'deny — the harness refuses its own web tools',
  };
  const nativeWebModeWord = $derived.by(() => {
    if (!snapshot) return '';
    const raw = snapshot.offload.native_web_visibility.trim();
    const word = NATIVE_WEB_MODE_WORDS[nativeWebMode];
    return raw === nativeWebMode ? word : `${word} (stored as “${raw}”)`;
  });

  /// A feature's app-wide L2 value, for the read-only display and for the
  /// "Inherit (on/off)" label on every override cell. One reader, because the
  /// tri-mode exception below used to be spelled out at each of them.
  function injectionL2On(f: InjectionFeatureRow): boolean {
    if (!snapshot) return false;
    if (f.field) return snapshot.offload.injection[f.field] as boolean;
    // The one feature with no boolean L2 (see the meta table). Its app-wide
    // switch is the `native_web_visibility` select below; `off` IS this control
    // off, and BOTH other modes are it on — which is why the row also renders
    // `nativeWebModeWord` (F-18's companion defect: on ≠ blocking).
    return f.key === 'native_web' ? nativeWebMode !== 'off' : false;
  }

  /// One override cell, resolved for display.
  ///
  /// Which scopes a feature HAS comes from the report's `in_scope` (#48, F-y) —
  /// it is `Feature::has_tab_scope` / `has_worker_scope` as the backend answers
  /// them, rather than a TypeScript copy of the same two predicates. The stored
  /// cell value still comes from `snapshot`, which is live: the report reflects
  /// SAVED settings, so binding the select to it would make a just-changed cell
  /// snap back until the debounced save and the next poll landed.
  function injectionScopeRows(f: InjectionFeatureRow): Array<{
    scope: string;
    label: string;
    value: string;
    inherited: boolean;
    resolved: string;
  }> {
    if (!snapshot) return [];
    const out: Array<{
      scope: string;
      label: string;
      value: string;
      inherited: boolean;
      resolved: string;
    }> = [];
    const inherited = injectionL2On(f);
    for (const scope of injection?.scopes ?? []) {
      // The app scope has no override cells — it is the level the cells inherit
      // FROM — so it is reported but never rendered as a row here.
      if (scope.scope === 'app') continue;
      const row = scope.features.find((x) => x.feature === f.key);
      if (!row?.in_scope) continue;
      const stored =
        scope.scope === 'offload-worker'
          ? (snapshot.offload.injection.worker as unknown as Record<string, string>)[f.key]
          : (
              snapshot.tabs.find((t) => t.kind === 'ai_tool' && t.id === scope.scope) as
                | { injection_overrides?: Record<string, string> }
                | undefined
            )?.injection_overrides?.[f.key];
      const why =
        row.decided_by === 'global'
          ? 'master'
          : row.decided_by === 'scope'
            ? 'this scope'
            : 'app-wide';
      out.push({
        scope: scope.scope,
        label: scope.label,
        value: stored ?? 'inherit',
        inherited,
        resolved: `→ ${row.effective ? 'on' : 'off'} (${why})`,
      });
    }
    return out;
  }

  /// Write one L3 cell. Goes through the ordinary `patch` save path like every
  /// other setting — there is deliberately no side-channel command, so the
  /// Settings window has one write path and cannot race its own full-object
  /// save.
  function setInjectionOverride(scope: string, key: string, value: string): void {
    patch((s) => {
      if (scope === 'offload-worker') {
        (s.offload.injection.worker as unknown as Record<string, string>)[key] = value;
        return;
      }
      for (const t of s.tabs) {
        if (t.kind === 'ai_tool' && t.id === scope) {
          (t.injection_overrides as unknown as Record<string, string>)[key] = value;
        }
      }
    });
  }

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
          kind: {
            type: 'local',
            server_command: '',
            autostart: false,
            show_command_on_start: false,
            auth_token: '',
          },
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
            // The legacy single-server fields never had a token; adopting one
            // is an "no auth" backend until the user fills the field in.
            auth_token: '',
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
  // V37 Phase D (contract C8): the editor itself is `McpManagementEditor`. What
  // stays here is the persistence seam, because only this file owns `snapshot`
  // and the awaited `applySettings` the rest of the panel uses.
  //
  // Two callbacks, deliberately distinct:
  //
  // * `setMcpRegistry` — the LOCAL snapshot only, no backend write. Text fields
  //   commit on blur rather than per keystroke: persisting per keystroke raced,
  //   because fire-and-forget `applySettings` calls could complete out of order
  //   and leave the backend holding a half-typed URL, which the 12s health watch
  //   would then flag as down.
  // * `applyMcpRegistry` — ONE awaited `settings_update` followed by ONE
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
    // Same lost-update gate as `patch()`: this is a wholesale push too, and a
    // broadcast landing during the awaits below must not regress the draft.
    const settled = draftSync.beginPush();
    try {
      await applySettings($state.snapshot(snapshot));
    } finally {
      settled();
    }
    await reloadMcpHost();
  }
  // Reconcile the warm host now and fold the fresh status into the health chips.
  async function reloadMcpHost(): Promise<void> {
    const status = await offloadReloadMcp();
    if (status) serviceStatus = status;
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
        b.tool_scope = localDataExcludedScope();
      } else {
        b.kind.cloud_consent = false;
        b.tool_scope = { mode: 'all' };
      }
    });
  }
  // Tool-scope picker: 'all' | 'web' (web/docs only) | custom (allexcept local-data).
  //
  // F-27: both the reader and the writer come from `settings/types` now, so the
  // radio cannot recognize a different set than the one it writes — and neither
  // depends on the exclusion list's LENGTH (a length test made a migrated
  // 7-entry list read as "custom", and clicking "web/docs only" then wrote the
  // stale 6-entry list back, dropping `run_check` from the exclusion).
  function scopeMode(scope: ToolScope): 'all' | 'web' | 'custom' {
    return toolScopeMode(scope);
  }
  function setScopeMode(i: number, mode: 'all' | 'web'): void {
    updateBackend(i, (b) => {
      b.tool_scope = mode === 'all' ? { mode: 'all' } : localDataExcludedScope();
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
  const e1Blocked = $derived(e1Gate !== null);

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

  /// Coarse age of a timestamp, for "last verified 3 h ago". Display-only: the
  /// panel needs the shape of the number, not its precision.
  function ageOf(atMs: number): string {
    const delta = Date.now() - atMs;
    if (!Number.isFinite(delta) || delta < 0) return 'just now';
    const mins = Math.floor(delta / 60000);
    if (mins < 1) return 'just now';
    if (mins < 60) return `${mins} min ago`;
    const hours = Math.floor(mins / 60);
    if (hours < 24) return `${hours} h ago`;
    return `${Math.floor(hours / 24)} d ago`;
  }

  /// The badge class for one row's last outcome. `no_failure` deliberately does
  /// NOT get the pass styling — the stored record keeps failures only, so it is
  /// the weaker statement and must not read as a green tick.
  function outcomeClass(outcome: string): string {
    if (outcome === 'fail') return 'bad';
    if (outcome === 'pass') return 'good';
    return 'quiet';
  }

  function outcomeLabel(outcome: string): string {
    if (outcome === OUTCOME_NO_FAILURE) return 'no failure reported';
    return outcome;
  }

  /// The badge class for a stored `AutoVerify.status`. Only the two statuses
  /// Rust writes get a colour; anything else is a hand edit (or a record from a
  /// newer cImp) and stays neutral rather than being guessed into "fine" —
  /// the same direction `harness::verify::tripwire_superseded` takes.
  function recordClass(status: string): string {
    if (status === 'fail') return 'bad';
    if (status === 'pass') return 'good';
    return 'quiet';
  }

  /// Rows whose seam is the one that breaks silently on a cosmetic upstream
  /// change, counted for the header. Display summary only — the rows carry the
  /// facts.
  function brokenNow(h: HarnessHealth): CapabilityHealth[] {
    return h.capabilities.filter(
      (c) => c.last_verify?.outcome === 'fail' || c.gate?.blocked,
    );
  }

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
  /// The harnesses that MOUNT each feature panel (V40 Phase F, locked decision
  /// 6). The two bottom-bar sections used to exist unconditionally and name one
  /// product in their headings; they are feature slots now, so a build with no
  /// harness that reports usage shows no usage panel at all rather than a panel
  /// about a thing nothing does.
  const sessionUsageHarnesses = $derived(
    $harnesses.filter((h) => h.features.includes('session_usage')),
  );
  const contextBarHarnesses = $derived(
    $harnesses.filter((h) => h.features.includes('context_bar')),
  );
  /// The harnesses whose local-provider block the Offload card renders.
  ///
  /// V40 review F-6: the feature MOUNTS the block and the plugin DECLARES the
  /// two `ext` keys it writes, so a harness that declares the feature without
  /// them renders nothing here rather than having an undeclared key written
  /// under it — one the backend preserves untouched, so it would be stored
  /// forever and read by nobody while the real setting stayed at its default.
  /// `harness::info::tests::a_declared_config_writer_exists` makes that
  /// combination fail a Rust test, so this filter is belt and braces.
  const localProviderConfigHarnesses = $derived(
    $harnesses.filter(
      (h) =>
        h.features.includes('local_provider_config') &&
        h.affordances.localProviderConfigBlockKey &&
        h.affordances.localProviderConfigAutoKey,
    ),
  );
  /// The shipped roster, as copy. Every sentence that used to enumerate the
  /// harnesses by hand ("restart the X/Y tab") interpolates one of
  /// these, so what a user reads is the roster the app actually has (V40 Phase
  /// F, locked decision 7).
  const harnessNames = $derived(harnessLabels($harnesses));
  const harnessNamesProse = $derived(harnessLabelsProse($harnesses));
  /// Each harness's OWN web tools, spelled the way it spells them — one
  /// harness capitalises them and another does not, which is why no single
  /// spelling could serve both (locked decision 27).
  const nativeWebToolsByHarness = $derived(
    $harnesses
      .filter((h) => h.affordances.webTools.length > 0)
      .map((h) => `${h.label}'s ${h.affordances.webTools.join('/')}`)
      .join(' and '),
  );
  /// Where each harness keeps its own state, for the sandbox copy.
  const harnessStateDirs = $derived(
    $harnesses.flatMap((h) => h.affordances.stateDirs).join(', '),
  );
  /// How each harness receives an injected prompt.
  const injectMechanisms = $derived(
    $harnesses
      .filter((h) => h.affordances.injectMechanism)
      .map((h) => `${h.label} via ${h.affordances.injectMechanism}`)
      .join(', '),
  );
  /// The sub-tab the Tabs section opens on: the first reserved tab there is.
  /// `''` while the roster is loading, which is one paint at most.
  let tabsSubSection = $state<TabsSubSection>('');
  $effect(() => {
    if (tabsSubSection === '' && aiTabIds.length > 0) tabsSubSection = aiTabIds[0];
  });
  // Sub-tab nav within the Offload section: the backend pool + limits live
  // under 'pool'; native tools, allowlist, and command policies under 'tools'.
  // (MCP servers moved to their own top-level `mcp` section — they're usable by
  // the harness tabs directly now, not just the offload worker.)
  type OffloadSubSection = 'pool' | 'tools';
  let offloadSubSection = $state<OffloadSubSection>('pool');
  // Sub-tab nav within the Code Intelligence section: index/build knobs under
  // 'graph'; semantic search + the embedding server under 'semantic'; context
  // injection + the read advisor under 'efficiency'; the 3D view under 'viz'.
  type GraphSubSection = 'graph' | 'semantic' | 'efficiency' | 'viz';
  let graphSubSection = $state<GraphSubSection>('graph');
  function subSectionForTabId(tabId: string): TabsSubSection {
    return aiTabIds.includes(tabId) ? tabId : 'shells';
  }

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

    // #48 (F-x): the backend's spawn-injection edge hint. It used to be emitted
    // to the main window only — a toast the user never sees, because they are
    // standing HERE when they flip the switch that raised it. Rendered as the
    // per-tab restart hint the Tabs section already has, so the affordance the
    // hint points at (a Restart button) is the same one it appears beside.
    const restartHintUnlisten = await listen<string[]>('ai-tab-restart-hint', (e) => {
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
  /// The push is registered with `draftSync` for as long as it is in flight, so
  /// a `settings-changed` broadcast that lands mid-burst cannot replace the
  /// draft with a state that is missing an edit this window just made — the
  /// lost-update race. The promise is no longer discarded silently: settling it
  /// is what reopens the gate, in either direction (`applySettings` resolves
  /// even on a rejected push — it rolls the store back itself — but `finally`
  /// keeps that from mattering here).
  function patch(updater: (s: Settings) => void) {
    if (!snapshot) return;
    const next = structuredClone($state.snapshot(snapshot));
    updater(next);
    snapshot = next;
    const settled = draftSync.beginPush();
    void applySettings(next).finally(settled);
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

  // ── Code Audit ─────────────────────────────────────────────────────────
  //
  // What is left here is the FEATURE's settings. The fourteen scanners
  // themselves are configured in the Tool Plugins section, because since V38
  // they are a plugin — one whose manifests cImp ships rather than one you drop
  // in a folder, but a plugin, rendered by the pane that already knows how.

  // Per-tool Detect probe result, keyed by TOOL KEY (`cimp-audit@1/gitleaks`,
  // or a user plugin's `name@version/tool-id`). `'probing'` while the IPC is in
  // flight. The IPC itself writes no settings; when it answers a click on an
  // EMPTY path box, this component stores what it found — see
  // `detectPluginTool`.
  let auditDetect = $state<Record<string, AuditDetectResult | 'probing' | undefined>>({});

  // The latest scan's language census, read from the runner so the
  // auto-selection button can apply the project's languages immediately rather
  // than waiting for the next census refresh. Empty (both lists) before any
  // scan.
  let auditCensus = $state<AuditCensus>({ extensions: [], markers: [] });

  // The "Auto-select for this project" button: back to automatic mode, and —
  // when a census is already known — apply the project-language selection at
  // once. The rule is `codeAudit/logic`'s mirror of the backend's, and the
  // flags land in the tool-plugins container where the built-in scanners'
  // state lives.
  function applyQualityAutoSelect(): void {
    patch((s) => {
      s.code_audit.quality_auto_select = true;
      if (censusIsEmpty(auditCensus)) return;
      for (const { id, enabled } of qualityAutoSelection(auditCensus)) {
        setToolEnabled(s, AUDIT_PLUGIN_KEY, id, enabled);
      }
    });
  }

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

  /// Detect: probe one tool, and — when the box was empty — SELECT what was
  /// found, for this tool and for the siblings of the same plugin that resolve
  /// to the same binary.
  ///
  /// Pressing Detect on an empty box is a question with one useful answer
  /// ("here it is, and it is now yours"): reporting a path the user then has to
  /// retype is the button doing nine tenths of the work and stopping. The write
  /// is deliberately HERE rather than in the IPC — `audit_detect_tool` is
  /// settings-read-only, so a probe can never change what a scan launches on its
  /// own; this is the user's click storing a path exactly as a Browse… would.
  ///
  /// A non-empty box is left alone: the probe was then asking about a path the
  /// user chose, and confirming it is not a reason to rewrite it.
  ///
  /// And a row `shouldAutoFill` refuses is left alone too, however empty its
  /// box is. A built-in that resolves by name is SUPPOSED to have an empty box —
  /// it finds its binary through `ebin` then `PATH` on every run, which is what
  /// its placeholder promises — so storing today's hit would quietly turn that
  /// live lookup into a pin and make the next drop-in update of the binary
  /// invisible. The probe result is displayed either way; only the write is
  /// refused, and per row: a sibling that does need a path still gets one.
  async function detectPluginTool(plugin: PluginRow, tool: ToolRow): Promise<void> {
    const toolKey = tool.toolKey;
    const path = tool.path.effective;
    // Read off the rows as they are NOW, before the await: the probe answers
    // milliseconds later against a settings snapshot that may have moved, and
    // "the rows the user was looking at when they clicked" is the honest
    // population to fill.
    const siblings = path.trim() === '' ? siblingAutoFillTargets(plugin, tool) : [];
    const fillClicked = shouldAutoFill(tool);
    auditDetect = { ...auditDetect, [toolKey]: 'probing' };
    try {
      // Probe the LIVE editing value, not the persisted setting — a just-typed
      // path would otherwise race the fire-and-forget applySettings push.
      const r = await auditDetectTool(toolKey, path);
      auditDetect = { ...auditDetect, [toolKey]: r };
      const targets = [...(fillClicked ? [toolKey] : []), ...siblings.map((s) => s.toolKey)];
      if (path.trim() === '' && r.found && r.path && targets.length > 0) {
        const found = r.path;
        patchPlugin((s) => {
          // One binary, several rows: `cargo build`, `cargo test`, `cargo
          // clippy` and `cargo` are four tools and one executable, and making
          // the user press Detect on each is the button stopping short.
          for (const target of targets) setGlobalPath(s, target, found);
        });
      }
    } catch (e) {
      auditDetect = {
        ...auditDetect,
        [toolKey]: { found: false, path: null, version: null, error: String(e) },
      };
    }
  }

  // ── V38 Tool Plugins ───────────────────────────────────────────────────
  // The section is master-detail over `plugins_snapshot`: the loader's set is
  // read ONCE on mount (it is a read of already-scanned state, never a disk
  // walk) and refreshed only by the explicit Rescan. Rows and every write are
  // in `lib/settings/toolPlugins.ts` — this component decides nothing.
  let pluginSet = $state<PluginSet | null>(null);
  // The key this project's per-tool path overrides are stored under. Asked of
  // the backend rather than derived here: canonicalizing a path touches the
  // disk, and a second spelling rule would silently stop matching the first.
  let pluginProjectKey = $state('');
  let pluginSelected = $state<string | null>(null);
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

  const pluginList = $derived<PluginRow[]>(
    pluginSet && snapshot ? pluginRows(pluginSet, snapshot, pluginProjectKey) : [],
  );
  const pluginErrors = $derived(pluginSet ? errorRows(pluginSet) : []);
  // Keep the selection valid across a Rescan that removed the selected plugin,
  // and land on the first one so the pane is never a blank right-hand side.
  const pluginActive = $derived<PluginRow | null>(
    pluginList.find((p) => p.key === pluginSelected) ?? pluginList[0] ?? null,
  );
  // What the LIST prints per key: the bare name, and the version only for rows
  // that would otherwise read identically (decision 9's collision case). The
  // detail pane always shows the version, so nothing is hidden — it is just not
  // repeated on every line of a list of names.
  const pluginLabels = $derived(pluginDisplayLabels(pluginList));

  function patchPlugin(updater: (s: Settings) => void): void {
    patch(updater);
  }

  /// A number input that means "no override" when blank. Shared by the timeout
  /// field so an unparseable keystroke reverts to inherit rather than to 0.
  function optionalSeconds(raw: string): number | null {
    const v = Number(raw.trim());
    return raw.trim() !== '' && Number.isFinite(v) && v >= 1 ? Math.floor(v) : null;
  }

  async function pickToolBinary(toolKey: string, scope: 'global' | 'project'): Promise<void> {
    const p = await pickFile('Executable', ['exe', 'cmd', 'bat', 'com']);
    if (!p) return;
    patchPlugin((s) =>
      scope === 'global'
        ? setGlobalPath(s, toolKey, p)
        : setProjectPath(s, pluginProjectKey, toolKey, p),
    );
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

<!-- V40 Phase B (locked decision 6) — one harness's DECLARED settings.
     A snippet rather than a copy per section: the sections that host it are
     per TAB, what it renders is per HARNESS, and this window should not be
     where those two get confused. A harness that declares no fields renders
     nothing, with no work here. -->
<!-- V40 review F-2/F-3: what a block that renders per harness shows while the
     roster is not in. `loadHarnesses` retries and reports, so the two states are
     distinguishable and the failed one offers the retry rather than leaving the
     window permanently missing its per-harness controls with no explanation. -->
{#snippet rosterPending()}
  {#if $harnessLoadState === 'failed'}
    <div class="roster-error" role="status">
      <span
        >The harness registry could not be read, so the per-harness controls are
        hidden. Nothing is lost — your settings are untouched.</span
      >
      <button type="button" onclick={() => void loadHarnesses()}>Try again</button>
    </div>
  {:else}
    <small class="hint">Loading the harness registry…</small>
  {/if}
{/snippet}

{#snippet harnessSettingsFor(harnessId: string, filter: (f: SettingFieldView) => boolean = () => true)}
  {#if snapshot}
    {#each $harnesses.filter((h) => h.id === harnessId) as h (h.id)}
      <HarnessExtForm
        harness={h}
        snapshot={snapshot}
        patch={(id, key, value) => patch((s) => setHarnessExt(s, id, key, value))}
        {filter}
      />
    {/each}
  {/if}
{/snippet}


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
        <section>
          <h2>TTS</h2>
          <Toggle
            label="Enable text-to-speech"
            checked={snapshot.tts.enabled}
            onchange={(next) => patch((s) => (s.tts.enabled = next))}
          />
          <small class="hint top">
            Loads the Kokoro voice model. Turn off to unload it and free
            memory — no AI output is spoken while disabled. (To keep the model
            loaded but silence playback, use <em>Mute</em> instead.)
          </small>
          <SelectField
            label="Process on"
            value={snapshot.tts.device}
            disabled={!snapshot.tts.enabled}
            onchange={(next) => patch((s) => (s.tts.device = next as ProcessingDevice))}
          >
            <option value="gpu">GPU (fall back to CPU)</option>
            <option value="cpu">CPU</option>
          </SelectField>
          <small class="hint">
            Where Kokoro runs. <strong>GPU</strong> uses the graphics card and
            automatically falls back to CPU if none is available;
            <strong>CPU</strong> forces CPU. Switching reloads the model on the
            new device — no restart needed.
          </small>
          <SelectField
            label="Voice"
            value={snapshot.tts.voice}
            disabled={!snapshot.tts.enabled}
            onchange={(next) => patch((s) => (s.tts.voice = next))}
          >
            {#each voices as v}
              <option value={v}>{v}</option>
            {/each}
          </SelectField>
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
          <Toggle
            label="Mute"
            checked={snapshot.tts.mute}
            disabled={!snapshot.tts.enabled}
            onchange={(next) => patch((s) => (s.tts.mute = next))}
          />
        </section>

        <section>
          <h2>Behavior</h2>
          <small class="hint">
            TTS is only stopped by Esc (or by switching tabs) — typing never
            interrupts speech.
          </small>
          <Toggle
            label="Auto-speak detected segments"
            checked={snapshot.behavior.auto_speak}
            onchange={(next) => patch((s) => (s.behavior.auto_speak = next))}
          />
          <Toggle
            label="Follow avatar visibility"
            checked={snapshot.behavior.follow_avatar}
            onchange={(next) => patch((s) => (s.behavior.follow_avatar = next))}
          />
          <small class="hint">
            When on, hiding the avatar mutes TTS and showing it unmutes —
            the Mute toggle tracks the avatar. Turn this off to control
            mute independently.
          </small>
          <Toggle
            label="Announce focused tab"
            checked={snapshot.behavior.announce_focused_tab}
            onchange={(next) => patch((s) => (s.behavior.announce_focused_tab = next))}
          />
          <small class="hint">
            Off by default — announcements (idle, awaiting permission, error,
            exit) only fire for background tabs. Turn on to hear them for the
            tab you're currently looking at as well.
          </small>
          <NumberField
            label="Announce idle only after working for … seconds"
            min="0"
            max="3600"
            step="10"
            value={snapshot.behavior.idle_announce_min_working_secs}
            onchange={(next) =>
              patch((s) => (s.behavior.idle_announce_min_working_secs = Math.max(0, Math.round(+next || 0))))}
          />
          <small class="hint">
            An idle announcement is skipped when the tab worked for less than
            this. 0 announces every idle. Permission, question and error
            announcements are never gated.
          </small>
          <Toggle
            label="Speak tagged TTS from background tabs"
            checked={snapshot.behavior.speak_background_tabs}
            onchange={(next) => patch((s) => (s.behavior.speak_background_tabs = next))}
          />
          <small class="hint">
            Off by default — tagged TTS segments (the spoken bits inside
            AI-tab output) only play for the active tab. Turn on to hear
            them from background tabs too. Announcements are unaffected.
          </small>
          <Toggle
            label="Copy on select"
            checked={snapshot.behavior.copy_on_select}
            onchange={(next) => patch((s) => (s.behavior.copy_on_select = next))}
          />
          <small class="hint">
            When on, text selected in any terminal is copied to the system
            clipboard automatically.
          </small>
          <Toggle
            label="Paste on right-click"
            checked={snapshot.behavior.paste_on_right_click}
            onchange={(next) => patch((s) => (s.behavior.paste_on_right_click = next))}
          />
          <small class="hint">
            When on, right-clicking inside any terminal pastes the system
            clipboard into the focused shell and suppresses the browser's
            default context menu.
          </small>
          <Toggle
            label="Speak selection on Ctrl+right-click"
            checked={snapshot.behavior.speak_selection_on_right_click}
            onchange={(next) => patch((s) => (s.behavior.speak_selection_on_right_click = next))}
          />
          <small class="hint">
            When on, Ctrl+right-clicking inside any terminal reads the
            selected text aloud through TTS. Holding Ctrl always suppresses
            paste, so the gesture never pastes the clipboard.
          </small>
          <Toggle
            label="Highlight selection while reading"
            checked={snapshot.tts.selection_highlight.enabled}
            onchange={(next) => patch((s) => (s.tts.selection_highlight.enabled = next))}
          />
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
          <Toggle
            label="Show selection-TTS controls in the status bar"
            checked={snapshot.tts.show_selection_controls}
            onchange={(next) => patch((s) => (s.tts.show_selection_controls = next))}
          />
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
            <NumberField
              label="Stability timeout (ms)"
              min="0"
              max="2000"
              step="10"
              value={snapshot.processing.stability_timeout_ms}
              onchange={(next) =>
                patch((s) => (s.processing.stability_timeout_ms = Math.max(0, +next)))}
            />
            <NumberField
              label="Max hold (ms)"
              min="50"
              max="5000"
              step="50"
              value={snapshot.processing.max_hold_ms}
              onchange={(next) =>
                patch((s) => (s.processing.max_hold_ms = Math.max(50, +next)))}
            />
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
          <Toggle
            label="Enable speech-to-text"
            checked={snapshot.stt.enabled}
            onchange={(next) => patch((s) => (s.stt.enabled = next))}
          />
          <small class="hint">
            Shows a microphone button in the bottom bar and enables the
            push-to-talk shortcut. Requires a model in the <code>models/</code> folder.
          </small>

          <SelectField
            label="Model"
            value={snapshot.stt.model_file}
            onchange={(next) => patch((s) => (s.stt.model_file = next))}
          >
            {#if !sttModels.includes(snapshot.stt.model_file)}
              <option value={snapshot.stt.model_file}>{snapshot.stt.model_file} (missing)</option>
            {/if}
            {#each sttModels as m}
              <option value={m}>{m}</option>
            {/each}
          </SelectField>
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

          <SelectField
            label="Process on"
            value={snapshot.stt.device}
            onchange={(next) => patch((s) => (s.stt.device = next as ProcessingDevice))}
          >
            <option value="gpu">GPU (fall back to CPU)</option>
            <option value="cpu">CPU</option>
          </SelectField>
          <small class="hint">
            Where Whisper runs. <strong>GPU</strong> uses the graphics card and
            automatically falls back to CPU if none is available;
            <strong>CPU</strong> forces CPU. Takes effect on your next recording.
          </small>

          <SelectField
            label="Input device"
            value={snapshot.stt.input_device}
            onchange={(next) => patch((s) => (s.stt.input_device = next))}
          >
            <option value="">System default</option>
            {#if snapshot.stt.input_device && !inputDevices.includes(snapshot.stt.input_device)}
              <option value={snapshot.stt.input_device}>{snapshot.stt.input_device} (not found)</option>
            {/if}
            {#each inputDevices as d}
              <option value={d}>{d}</option>
            {/each}
          </SelectField>

          <SelectField
            label="Language"
            value={snapshot.stt.language}
            onchange={(next) => patch((s) => (s.stt.language = next))}
          >
            {#if !STT_LANGUAGES.some((l) => l.code === snapshot!.stt.language)}
              <option value={snapshot.stt.language}>{snapshot.stt.language}</option>
            {/if}
            {#each STT_LANGUAGES as l}
              <option value={l.code}>{l.label}</option>
            {/each}
          </SelectField>

          <Toggle
            label="Translate to English"
            checked={snapshot.stt.translate_to_english}
            onchange={(next) => patch((s) => (s.stt.translate_to_english = next))}
          />
          <small class="hint">
            Transcribe non-English speech as English instead of verbatim.
          </small>

          <SelectField
            label="Record button mode"
            value={snapshot.stt.button_mode}
            onchange={(next) =>
              patch((s) => (s.stt.button_mode = next as 'toggle' | 'hold'))}
          >
            <option value="toggle">Toggle (click to start / stop)</option>
            <option value="hold">Hold (press and hold to record)</option>
          </SelectField>

          <small class="hint">
            The push-to-talk shortcut (hold to record) lives in
            <strong>Keyboard controls</strong>.
          </small>
        </section>

      {:else if activeSection === 'avatar'}
        <section>
          <h2>Avatar</h2>
          <Toggle
            label="Visible"
            checked={snapshot.avatar.visible}
            onchange={(next) => patch((s) => (s.avatar.visible = next))}
          />
          <SelectField
            label="Type"
            value={snapshot.avatar.kind}
            onchange={(next) =>
              patch((s) => (s.avatar.kind = next as Settings['avatar']['kind']))}
          >
            <option value="media">Picture / Video</option>
            <option value="sprite">Animated sprites</option>
          </SelectField>
          {#if snapshot.avatar.kind === 'sprite'}
            <SelectField
              label="Sprite set"
              value={snapshot.avatar.sprite.set}
              onchange={(next) => patch((s) => (s.avatar.sprite.set = next))}
            >
              <!-- V40 Phase F: the bundled sets are named once, in
                   `avatarConfig.ts` (locked decision 29 rules them brand
                   assets, not harness identity). -->
              {#each SPRITE_SETS as set (set.id)}
                <option value={set.id}>{set.label}</option>
              {/each}
            </SelectField>
            <small class="hint">
              Frame-animated pixel-art mascot. Each state (Idle, Listening,
              Thinking, Speaking, Error) maps to a set of animations from the
              set's <code>manifest.json</code>; the per-state image/video and
              transition options below are ignored in this mode.
            </small>
          {/if}
          <SelectField
            label="Position"
            value={snapshot.avatar.position}
            onchange={(next) =>
              patch((s) => (s.avatar.position = next as Settings['avatar']['position']))}
          >
            <option value="top-right">Top Right</option>
            <option value="top-left">Top Left</option>
            <option value="bottom-right">Bottom Right</option>
            <option value="bottom-left">Bottom Left</option>
          </SelectField>
          <div class="row">
            <NumberField
              label="Width (px)"
              min="50"
              max="1200"
              value={snapshot.avatar.size.width_px}
              onchange={(next) =>
                patch((s) => (s.avatar.size.width_px = Math.max(50, +next)))}
            />
            <NumberField
              label="Height (px)"
              min="50"
              max="1200"
              value={snapshot.avatar.size.height_px}
              onchange={(next) =>
                patch((s) => (s.avatar.size.height_px = Math.max(50, +next)))}
            />
          </div>
          <div class="row">
            <NumberField
              label="Margin X (px)"
              min="0"
              max="200"
              value={snapshot.avatar.margin.x_px}
              onchange={(next) =>
                patch((s) => (s.avatar.margin.x_px = Math.max(0, +next)))}
            />
            <NumberField
              label="Margin Y (px)"
              min="0"
              max="200"
              value={snapshot.avatar.margin.y_px}
              onchange={(next) =>
                patch((s) => (s.avatar.margin.y_px = Math.max(0, +next)))}
            />
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
          <Toggle
            label="Show border"
            checked={snapshot.avatar.show_border}
            onchange={(next) => patch((s) => (s.avatar.show_border = next))}
          />

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
          <NumberField
            label="Duration (ms)"
            min="0"
            max="5000"
            step="50"
            value={snapshot.avatar.transition.duration_ms}
            onchange={(next) =>
              patch((s) => (s.avatar.transition.duration_ms = Math.max(0, +next)))}
          />
          {/if}
        </section>

        <section>
          <h2>Waveform</h2>
          <Toggle
            label="Show waveform"
            checked={snapshot.avatar.waveform.visible}
            onchange={(next) => patch((s) => (s.avatar.waveform.visible = next))}
          />
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
          <SelectField
            label="Theme"
            value={snapshot.ui.theme}
            onchange={(next) => {
              const theme = next;
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
          </SelectField>

          {#if snapshot.ui.theme === TUI_THEME_ID}
            <!-- Accent picker — TUI-only: the built-in theme derives its whole
                 accent family from this one color; disk themes carry their own
                 fixed accents, so the control hides for them. -->
            <div class="accent-row">
              <span>Accent color</span>
              <div class="accent-controls">
                {#each TUI_ACCENT_PRESETS as p}
                  <button
                    type="button"
                    class="icon accent-swatch"
                    class:selected={normalizeTuiAccent(snapshot.ui.tui_accent) === p.color}
                    style:background={p.color}
                    title={p.name}
                    aria-label={`Accent: ${p.name}`}
                    onclick={() => patch((s) => (s.ui.tui_accent = p.color))}
                  ></button>
                {/each}
                <input
                  type="color"
                  aria-label="Custom accent color"
                  value={normalizeTuiAccent(snapshot.ui.tui_accent)}
                  oninput={(e) => {
                    const color = (e.currentTarget as HTMLInputElement).value;
                    patch((s) => (s.ui.tui_accent = color));
                  }}
                />
              </div>
            </div>
            <!-- `top`: this hint follows the swatch row, not a label — the
                 default hint's -8px pull-up would drag it into the swatches. -->
            <small class="hint top">
              Tints buttons, borders, tabs, and the waveform. Presets match
              the four classic TUI accents; the swatch on the right picks
              anything.
            </small>
          {/if}

          <!-- V32 containment colors. Theme-independent (unlike the TUI
               accent above): the taint badge and pane frame render under
               every theme, so their colors are always editable. Two states,
               two colors — matching the badge's own distinction, where
               contamination outlives the latch and wears the stronger one. -->
          <h3>Containment colors</h3>
          <small class="hint top">
            Worn by a tab's ⛨ shield badge and drawn as a frame around that
            tab's content while containment applies — so a latched or
            contaminated tab is visible without reading the tab strip.
          </small>
          <div class="accent-row">
            <span>Latched session</span>
            <div class="accent-controls">
              <input
                type="color"
                aria-label="Latched tab color"
                title="The session used a gated tool (web/external or local), so the opposite tool family is closed for it"
                value={normalizeHexColor(snapshot.ui.latched_color, DEFAULT_LATCHED_COLOR)}
                oninput={(e) => {
                  const color = (e.currentTarget as HTMLInputElement).value;
                  patch((s) => (s.ui.latched_color = color));
                }}
              />
              <button
                type="button"
                class="secondary"
                disabled={normalizeHexColor(snapshot.ui.latched_color, DEFAULT_LATCHED_COLOR) ===
                  DEFAULT_LATCHED_COLOR}
                onclick={() => patch((s) => (s.ui.latched_color = DEFAULT_LATCHED_COLOR))}
                >Reset</button
              >
            </div>
          </div>
          <div class="accent-row">
            <span>Contaminated session</span>
            <div class="accent-controls">
              <input
                type="color"
                aria-label="Contaminated tab color"
                title="External content entered the conversation — the stronger state; it outlives the latch"
                value={normalizeHexColor(
                  snapshot.ui.contaminated_color,
                  DEFAULT_CONTAMINATED_COLOR,
                )}
                oninput={(e) => {
                  const color = (e.currentTarget as HTMLInputElement).value;
                  patch((s) => (s.ui.contaminated_color = color));
                }}
              />
              <button
                type="button"
                class="secondary"
                disabled={normalizeHexColor(
                  snapshot.ui.contaminated_color,
                  DEFAULT_CONTAMINATED_COLOR,
                ) === DEFAULT_CONTAMINATED_COLOR}
                onclick={() =>
                  patch((s) => (s.ui.contaminated_color = DEFAULT_CONTAMINATED_COLOR))}
                >Reset</button
              >
            </div>
          </div>

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
          <Toggle
            label="Preview image / category changes in Configure Tab dialog"
            checked={snapshot.terminal.background.preview_category_flips}
            onchange={(next) => patch((s) => (s.terminal.background.preview_category_flips = next))}
          />
          <small class="hint">
            When off, image-toggle and category-flip changes wait for Save in
            the Configure Tab dialog. Color, opacity, blur, size, position,
            and tint always preview live.
          </small>
          <NumberField
            label="Scrollback kept across renderer switches (lines)"
            min="0"
            value={snapshot.terminal.background.snapshot_lines}
            onchange={(next) =>
              patch((s) => {
                const n = Number(next);
                s.terminal.background.snapshot_lines = Number.isFinite(n)
                  ? Math.max(0, Math.floor(n))
                  : 2000;
              })}
          />
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
          <NumberField
            label="Terminal font size (px)"
            min="8"
            max="48"
            value={snapshot.display.terminal_font_size}
            onchange={(next) =>
              patch((s) => (s.display.terminal_font_size = Math.max(8, +next)))}
          />
        </section>

        <section>
          <h2>Compose</h2>
          <small class="hint top">
            Sizing of the multi-line compose box that opens for prompts.
          </small>
          <div class="row">
            <NumberField
              label="Min height (px)"
              min="40"
              max="400"
              value={snapshot.compose.min_height_px}
              onchange={(next) =>
                patch((s) => (s.compose.min_height_px = Math.max(40, +next)))}
            />
            <NumberField
              label="Max height (px)"
              min="60"
              max="800"
              value={snapshot.compose.max_height_px}
              onchange={(next) =>
                patch((s) => (s.compose.max_height_px = Math.max(60, +next)))}
            />
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
        {#if sessionUsageHarnesses.length > 0}
        <section>
          <h2>{harnessLabels(sessionUsageHarnesses)} session usage</h2>
          <small class="hint top">
            Shows the quota windows {harnessLabelsProse(sessionUsageHarnesses)}
            reports, in the bottom bar next to Layouts. The numbers come from
            that harness's own status line, so they need the context status bar
            (below) left on and one of its tabs to have sent at least one
            message; the widget hides until then, and dims when the last report
            gets old (tab closed, or idle too long).
          </small>
          <Toggle
            label="Show usage in the bottom bar"
            checked={snapshot.usage.enabled}
            onchange={(next) => patch((s) => (s.usage.enabled = next))}
          />
          <small class="hint">
            The toggles below pick which pieces of each window are shown
            (they apply to both the 5h and 7d readouts).
          </small>
          <Toggle
            label="Bar"
            checked={snapshot.usage.show_bar}
            disabled={!snapshot.usage.enabled}
            onchange={(next) => patch((s) => (s.usage.show_bar = next))}
          />
          <Toggle
            label="Percentage"
            checked={snapshot.usage.show_percentage}
            disabled={!snapshot.usage.enabled}
            onchange={(next) => patch((s) => (s.usage.show_percentage = next))}
          />
          <Toggle
            label="Countdown timer"
            checked={snapshot.usage.show_countdown}
            disabled={!snapshot.usage.enabled}
            onchange={(next) => patch((s) => (s.usage.show_countdown = next))}
          />
          <Toggle
            label="Reset clock (local time)"
            checked={snapshot.usage.show_reset_clock}
            disabled={!snapshot.usage.enabled}
            onchange={(next) => patch((s) => (s.usage.show_reset_clock = next))}
          />
          <NumberField
            label="Poll interval (seconds)"
            min="15"
            max="3600"
            step="15"
            value={snapshot.usage.poll_interval_secs}
            disabled={!snapshot.usage.enabled}
            onchange={(next) =>
              patch((s) => (s.usage.poll_interval_secs = Math.max(15, +next)))}
          />
          <small class="hint">
            How often the widget re-reads the status line's latest report (a
            local read — no network). Minimum 15s; the countdown ticks every
            second locally between refreshes.
          </small>
        </section>
        {/if}

        <section>
          <h2>Local machine information</h2>
          <small class="hint top">
            Live CPU / memory / GPU / network panel in the bottom bar, right of
            the session usage meter.
          </small>
          <Toggle
            label="Show local machine information"
            checked={snapshot.system_stats.enabled}
            onchange={(next) => patch((s) => (s.system_stats.enabled = next))}
          />
          <small class="hint">
            The toggles below pick which components are shown.
          </small>
          <Toggle
            label="CPU usage"
            checked={snapshot.system_stats.show_cpu}
            disabled={!snapshot.system_stats.enabled}
            onchange={(next) => patch((s) => (s.system_stats.show_cpu = next))}
          />
          <Toggle
            label="Memory"
            checked={snapshot.system_stats.show_memory}
            disabled={!snapshot.system_stats.enabled}
            onchange={(next) => patch((s) => (s.system_stats.show_memory = next))}
          />
          <Toggle
            label="GPU (usage + VRAM)"
            checked={snapshot.system_stats.show_gpu}
            disabled={!snapshot.system_stats.enabled}
            onchange={(next) => patch((s) => (s.system_stats.show_gpu = next))}
          />
          <Toggle
            label="GPU temperature"
            checked={snapshot.system_stats.show_gpu_temp}
            disabled={!snapshot.system_stats.enabled || !snapshot.system_stats.show_gpu}
            onchange={(next) => patch((s) => (s.system_stats.show_gpu_temp = next))}
          />
          <Toggle
            label="Network"
            checked={snapshot.system_stats.show_network}
            disabled={!snapshot.system_stats.enabled}
            onchange={(next) => patch((s) => (s.system_stats.show_network = next))}
          />
          <NumberField
            label="Poll interval (seconds)"
            min="1"
            max="60"
            value={snapshot.system_stats.poll_interval_secs}
            disabled={!snapshot.system_stats.enabled}
            onchange={(next) =>
              patch((s) => (s.system_stats.poll_interval_secs = Math.max(1, +next)))}
          />
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

        {#each contextBarHarnesses as h (h.id)}
        <section>
          <h2>{h.label} context bar</h2>
          <small class="hint top">
            Adds a context-window usage bar to {h.label}'s own status line inside
            each of its tabs — e.g.
            <code>model ▓▓▓▓▓░░░░░ 50% (100k/200k)</code>, themed to your
            terminal palette. cImp wires this up only for the tabs it launches;
            your own global {h.label} configuration is left untouched. The status
            line also feeds the session-usage meter above — turning it off leaves
            that meter with no data.
          </small>
          <!-- V40 Phase B: the switch is one of the harness's own declared
               settings, so it renders with the rest of them rather than being a
               control this window hard-codes for one harness. V40 Phase F: the
               section itself is mounted by the `context_bar` feature, and every
               name in it is the descriptor's. -->
          <small class="hint">
            The switch lives with the harness that has the status line:
            <strong>Tabs → {labelForTabId($harnesses, h.tab_ids[0])}</strong>.
          </small>
        </section>
        {/each}

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
        {@const shellEntries = tabEntries.filter((e) => e.kind === 'shell')}
        {@const enabledAiTabs = snapshot.enabled_ai_tabs}
        {@const lastChecked = enabledAiTabs.length === 1 ? enabledAiTabs[0] : null}
        <section>
          <h2>Tabs</h2>
          <fieldset class="ai-tabs-radio">
            <legend>AI tabs enabled</legend>
            <small class="hint">
              Pick which AI-tool tabs to keep. Toggling a checkbox opens
              or closes the matching tab (the closed tab's PTY is killed
              and its scrollback dropped). At least one tab must remain
              checked.
            </small>
            <!-- V40 review F-2: not before the roster is in. These are
                 DESTRUCTIVE controls (a tick kills a PTY), and until the
                 registry answers there is no label to put on them. -->
            {#if !rosterReady}
              {@render rosterPending()}
            {:else}
            <div class="radio-row">
              <!-- V40 Phase F (locked decision 7): one checkbox per RESERVED
                   tab id the registry declares, in its canonical order. It was
                   three hand-written boxes, so a third harness's tabs could not
                   be turned on at all without editing this file. -->
              {#each aiTabIds as aiTabId (aiTabId)}
                <label>
                  <input
                    type="checkbox"
                    name="ai-tabs-enabled"
                    value={aiTabId}
                    checked={enabledAiTabs.includes(aiTabId)}
                    disabled={lastChecked === aiTabId}
                    onchange={(e) =>
                      void toggleAiTabEnabled(
                        aiTabId,
                        (e.currentTarget as HTMLInputElement).checked,
                      )}
                  />
                  {labelForTabId($harnesses, aiTabId)}
                </label>
              {/each}
            </div>
            {/if}
            {#if aiTabsError}
              <small class="error">{aiTabsError}</small>
            {/if}
          </fieldset>
          <Toggle
            checked={snapshot.ui.tool_activity_tab}
            onchange={(next) => patch((s) => (s.ui.tool_activity_tab = next))}
          >
            Show the <strong>Tools</strong> tab
          </Toggle>
          <small class="hint">
            One place to watch tool usage: a unified feed of code-intelligence
            graph calls and offload requests, plus the graph/offload tool
            reference lists.
          </small>
          <Toggle
            checked={snapshot.ui.events_tab}
            onchange={(next) => patch((s) => (s.ui.events_tab = next))}
          >
            Show the <strong>Events</strong> tab
          </Toggle>
          <small class="hint">
            The same recorded activity, read as events: every row says which
            tab and which session it came from, and the feed filters by kind,
            source/screen and tab. Independent of the Tools tab — turning one
            off leaves the other alone.
          </small>
          <Toggle
            checked={snapshot.preview_allow_remote}
            onchange={(next) => patch((s) => (s.preview_allow_remote = next))}
          >
            Allow <strong>Preview</strong> tabs to load remote URLs
          </Toggle>
          <small class="hint">
            Off (default) restricts Preview-tab navigation to localhost and
            private-network (RFC&nbsp;1918) hosts — the tab is meant for your
            own dev servers. On lets a Preview tab load any http(s) URL in its
            embedded webview.
          </small>
          <div class="sub-tabs" role="tablist" aria-label="Tabs sub-sections">
            <!-- V40 Phase F: one sub-tab per reserved AI tab id, from the
                 registry (locked decision 7). Three hand-written buttons
                 before, each naming a tab id and a product. -->
            {#if rosterReady}
              {#each aiTabIds as aiTabId (aiTabId)}
                <button
                  type="button"
                  role="tab"
                  class:active={tabsSubSection === aiTabId}
                  aria-selected={tabsSubSection === aiTabId}
                  onclick={() => (tabsSubSection = aiTabId)}
                >
                  {labelForTabId($harnesses, aiTabId)}
                </button>
              {/each}
            {/if}
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

          {#if !rosterReady && tabsSubSection !== 'shells'}
            {@render rosterPending()}
          {:else if rosterReady && aiTabIds.includes(tabsSubSection)}
            <!--
              V40 Phase F (locked decision 7): ONE body for every reserved AI
              tab, instead of a `{:else if}` per tab id. The two facts that used
              to be spelled per branch are registry lookups now:

              * the harness's declared settings render under its FIRST reserved
                tab, because they are the harness's and not the tab's — with
                ONE declared exception (issue #109): the rows a plugin marks
                `provider_tab` describe the custom-provider variant, so they
                render on THAT tab's page instead, next to the tab they
                configure. A reserved tab that is neither gets a pointer rather
                than a second copy of the form;
              * every name comes from the descriptor, so a harness added over
                IPC arrives with its own heading and no markup here.
            -->
            {@const harness = findHarnessByTabId($harnesses, tabsSubSection)}
            {@const live = aiTabAt(tabsSubSection)}
            <!--
              Where a declared field renders: its harness's custom-provider tab
              if it is marked `provider_tab` AND such a tab exists, otherwise
              the harness's first reserved tab. The fallback is deliberate — a
              harness that declares provider rows and no provider tab shows them
              rather than hiding them (no shipped harness does).
            -->
            {@const fieldHome = (f: SettingFieldView) =>
              f.provider_tab && harness?.provider_tab_id
                ? harness.provider_tab_id
                : (harness?.tab_ids[0] ?? '')}
            {@const ownsForm =
              harness?.tab_ids[0] === tabsSubSection ||
              harness?.provider_tab_id === tabsSubSection}
            <div id="tab-section-{tabsSubSection}">
              {#if live}
                <TabSettingsSection
                  tabId={tabsSubSection}
                  displayName={labelForTabId($harnesses, tabsSubSection)}
                  bind:settings={
                    () => live,
                    (v) => patchAiTab(tabsSubSection, v)
                  }
                  defaults={tabDefaults[tabsSubSection] ?? null}
                  restartRequired={restartRequired[tabsSubSection] ?? false}
                  onchange={() => {}}
                  onrestart={() => restartTab(tabsSubSection)}
                />
              {:else}
                <small class="hint top"
                  >{labelForTabId($harnesses, tabsSubSection)} tab is disabled — tick the
                  checkbox above to enable it.</small
                >
              {/if}
              {#if harness && ownsForm}
                {@render harnessSettingsFor(
                  harness.id,
                  (f) => fieldHome(f) === tabsSubSection,
                )}
              {:else if harness}
                <small class="hint top">
                  This tab's custom-provider values (and everything else this
                  harness declares) are {harness.label}'s own settings — see
                  <strong>Tabs → {labelForTabId($harnesses, harness.tab_ids[0])}</strong>.
                </small>
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
            <code>offload_task</code> tool into cImp-launched AI tabs.
            The main session can hand token-heavy subtasks (broad codebase
            searches, large-file/log summarization, web research) to the
            local model and get back only the synthesized result —
            conserving its context window. Everything stays local. Off by
            default; the model is user-supplied (not bundled).
          </small>
          <Toggle
            label="Enable offload"
            checked={snapshot.offload.enabled}
            onchange={(next) => patch((s) => (s.offload.enabled = next))}
          />
          <Toggle
            label="Inject offload guidance into the system prompt"
            checked={snapshot.offload.inject_guidance}
            onchange={(next) => patch((s) => (s.offload.inject_guidance = next))}
          />
          <small class="hint">
            The <code>offload_task</code> tool and its guidance are injected
            when an AI tab starts — restart the {harnessNames} tab
            (Tabs → Restart) after changing either toggle.
          </small>
          <Toggle
            label="Session push (experimental)"
            checked={snapshot.offload.session_push}
            onchange={(next) => patch((s) => (s.offload.session_push = next))}
          />
          <small class="hint">
            Lets cImp push notices — offload results, audit and graph-index
            completions — straight into a live AI tab.
            A tab whose harness can be PUSHED to receives them as
            <code>&lt;channel source="cimp-offload"&gt;</code> messages at the
            next turn boundary, which <em>starts a turn</em> when the tab is
            idle; that half is baked in at launch, so restart the tab after
            toggling — cImp shows the restart hint automatically. It also needs
            the <code>cimp-offload</code> MCP server to be injected, i.e.
            offload or the code graph enabled.
            A tab whose harness takes silently injected context
            (<code>noReply</code>) instead receives the same envelope with
            nothing started: the model picks it up on its next turn, and that
            half is read live — no tab restart needed.
            <strong>Experimental:</strong> the push half rides a harness
            research-preview flag that may change or disappear, the harness
            paints a
            persistent "Channels (experimental)" banner (plus a harmless
            "no MCP server configured with that name" warning) in every tab it
            registers, and a push that can't be delivered is silently dropped.
            Off by default.
          </small>

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
                <label>
                  <span>Auth token (optional)</span>
                  <!-- V33: a Local backend is only "local" in the sense that
                       cImp owns the process — the server still listens on a
                       socket, and `--host 0.0.0.0` puts it on the LAN. Same
                       `type="password"`, cleartext-on-disk treatment as the
                       Remote token below; `?? ''` because a settings file
                       written before V33 has no key here and an `undefined`
                       must render as an empty field, never blank the card. -->
                  <input
                    type="password"
                    value={backend.kind.auth_token ?? ''}
                    oninput={(e) =>
                      updateBackend(i, (b) => {
                        if (b.kind.type === 'local') b.kind.auth_token = (e.currentTarget as HTMLInputElement).value;
                      })}
                    placeholder="Matches --api-key in the command above"
                  />
                  <small class="hint">
                    Sent as a <code>Bearer</code> header to this server. Leave
                    empty for no auth. Set it when the command above passes
                    <code>--api-key</code> — the two must match, and a server
                    bound to <code>--host 0.0.0.0</code> is reachable by
                    anything on your LAN without one.
                  </small>
                </label>
                <Toggle
                  label="Show command on start"
                  checked={backend.kind.show_command_on_start}
                  onchange={(next) =>
                    updateBackend(i, (b) => {
                      if (b.kind.type === 'local')
                        b.kind.show_command_on_start = next;
                    })}
                />
                <small class="hint">
                  The Start button in Tools → Offload server opens the
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
                      <!-- Said out loud because the Remote popup's counterpart
                           DOES save its token, so silence here would read as
                           "the same, plus the token". -->
                      <small class="hint">Saves the server command only — not the auth token.</small>
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
                <Toggle
                  label="Cloud backend (data leaves this machine)"
                  checked={backend.kind.is_cloud}
                  onchange={(next) => setBackendCloud(i, next)}
                />
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
                <NumberField
                  label="Declared context (tokens, when /props is absent)"
                  min="0"
                  placeholder="e.g. 16000"
                  value={backend.declared_context ?? ''}
                  event="input"
                  onchange={(next) =>
                    updateBackend(i, (b) => {
                      const v = next;
                      const n = +v;
                      // Empty / non-numeric → null (use /props), never NaN.
                      b.declared_context =
                        v === '' || Number.isNaN(n) ? null : Math.max(0, n);
                    })}
                />
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
              <SelectField
                label="Tool scope"
                value={scopeMode(backend.tool_scope)}
                disabled={scopeMode(backend.tool_scope) === 'custom'}
                onchange={(next) => setScopeMode(i, next as 'all' | 'web')}
              >
                <option value="all">All tools</option>
                <option value="web">Web/docs only (deny local files, code, commands, git)</option>
                {#if scopeMode(backend.tool_scope) === 'custom'}
                  <option value="custom">Custom (edit in settings.json)</option>
                {/if}
                {#snippet after()}
                  <small class="hint">
                    Cloud backends default to web/docs only so local file contents
                    never leave the machine. Widen a cloud backend only with intent.
                  </small>
                {/snippet}
              </SelectField>

              {#if backend.kind.type === 'local'}
                <hr class="card-divider" />
                <!-- V40 Phase F: one block per harness that declares
                     `local_provider_config` — i.e. one cImp can WRITE a provider
                     block for. It was hard-coded for one harness, so a second
                     one with a config writer would have had no button at all. -->
                {#each localProviderConfigHarnesses as h (h.id)}
                  {@const autoKey = h.affordances.localProviderConfigAutoKey ?? ''}
                  <div class="button-row">
                    <button
                      type="button"
                      class="secondary"
                      onclick={() => registerLocalProvider(i, h)}
                    >Add to {h.label}</button>
                    <label class="checkbox inline">
                      <input
                        type="checkbox"
                        checked={harnessRow(snapshot, h.id).ext?.[autoKey] === true}
                        onchange={(e) =>
                          patch((s) =>
                            setHarnessExt(
                              s,
                              h.id,
                              autoKey,
                              (e.currentTarget as HTMLInputElement).checked,
                            ),
                          )}
                      />
                      <span>Auto-sync while offload enabled</span>
                    </label>
                  </div>
                  {#if h.affordances.localProviderConfigNote}
                    <small class="hint provider-desc">{h.affordances.localProviderConfigNote}</small>
                  {/if}
                {/each}
                {#if providerMsg && providerMsg.i === i}
                  <small class={providerMsg.ok ? 'hint' : 'error'}>{providerMsg.text}</small>
                {/if}
                <div class="button-row offload-lifecycle-row">
                  <button type="button" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendStart(backend.name))}>Start</button>
                  <button type="button" class="secondary" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendStop(backend.name))}>Stop</button>
                  <button type="button" class="secondary" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendRestart(backend.name))}>Reset</button>
                </div>
              {/if}
            </div>
          {/each}

          <!--
            V39 Phase C — the facade backends, READ-ONLY.

            They are not in `offload.backends` and never will be: a Remote-offload
            tab IS the backend (locked decision 8), so there is exactly one place
            to change one, and it is that tab's own popover. Listing them here
            anyway is the point — a backend the router can pick but the backend
            list does not mention is a backend the user cannot account for.
          -->
          {#each facadeBackends(snapshot) as facade (facade.tabId)}
            <div class="backend-card facade">
              <div class="backend-head">
                <span class="backend-name-static" title="The name the requesting harness sees">{facade.name}</span>
                <span class="facade-kind">tab worker</span>
                <span class="facade-kind">{facade.tier}</span>
                {#if facade.declaredContext}
                  <span class="facade-kind">~{Math.max(1, Math.round(facade.declaredContext / 1000))}k ctx</span>
                {/if}
              </div>
              <!--
                V39 review M-9: a name collision DROPS the facade from the pool
                (the router, the run log and the dashboard all key on the name),
                and the drop used to be a `warn!` in the log and nothing else —
                this list showed the row as if it were live. Rendered rather
                than hidden: the row is where the user can see what to rename.
              -->
              {#if facade.droppedReason}
                <small class="error">{facade.droppedReason}</small>
              {/if}
              <small class="hint">
                Configured on the tab “{facade.tabName}” — set its role, backend name,
                tier and context in that tab's ⇄ popover. It is offered to
                <code>offload_task</code> under the name above and never as a tab;
                it is ready while the tab is open and idle.
              </small>
            </div>
          {/each}

          <div class="button-row">
            <button type="button" onclick={addLocalBackend}>+ Local backend</button>
            <button type="button" onclick={addRemoteBackend}>+ Remote backend</button>
          </div>

          <hr class="card-divider lg" />
          <h3>Limits</h3>
          <NumberField
            label="Working-budget high-water (%)"
            min="10"
            max="100"
            value={snapshot.offload.budget_high_water_pct}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.offload.budget_high_water_pct = Math.min(
                    100,
                    Math.max(10, +next || 10),
                  )),
              )}
          >
            <small class="hint">
              Fraction of the per-slot window the loop works against,
              reserving the rest for reasoning + the answer (~80%).
            </small>
          </NumberField>
          <NumberField
            label="Per-tool-result token cap"
            min="256"
            value={snapshot.offload.per_tool_result_token_cap}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.offload.per_tool_result_token_cap = Math.max(
                    256,
                    +next || 256,
                  )),
              )}
          />
          <NumberField
            label="Max steps"
            min="1"
            value={snapshot.offload.max_steps}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.offload.max_steps = Math.max(
                    1,
                    +next || 1,
                  )),
              )}
          />
          <NumberField
            label="Per-task timeout (seconds)"
            min="30"
            value={snapshot.offload.offload_timeout_secs}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.offload.offload_timeout_secs = Math.max(
                    30,
                    +next || 30,
                  )),
              )}
          >
            <small class="hint">Bounds each offload, including the wait for a free slot.</small>
          </NumberField>
          <NumberField
            label="Max queue depth (blank = unlimited)"
            min="0"
            placeholder="unlimited"
            value={snapshot.offload.max_queue_depth ?? ''}
            onchange={(next) => {
              const raw = next.trim();
              const n = Math.floor(+raw);
              patch(
                (s) =>
                  (s.offload.max_queue_depth =
                    raw === '' || !Number.isFinite(n) || n <= 0 ? null : n),
              );
            }}
          >
            <small class="hint">
              When every slot is busy and this many tasks are already waiting,
              new offloads are rejected immediately instead of queuing. Blank
              keeps the unbounded queue (each waits up to the timeout above).
            </small>
          </NumberField>
          <NumberField
            label="Global concurrency (blank = auto)"
            min="1"
            placeholder="auto"
            value={snapshot.offload.global_concurrency ?? ''}
            onchange={(next) => {
              const raw = next.trim();
              const n = Math.floor(+raw);
              patch(
                (s) =>
                  (s.offload.global_concurrency =
                    raw === '' || !Number.isFinite(n) || n <= 0 ? null : n),
              );
            }}
          >
            <small class="hint">
              Cap on offload tasks in flight across the whole app. Blank
              auto-sizes from the summed per-backend slot counts.
            </small>
          </NumberField>
          <!--
            F-18: the injection controls used to sit right here, at the bottom
            of this sub-tab. This is where anyone who remembers that, or who
            read one of the pointers that named a "Tools" section, will look.
            A breadcrumb rather than an alias in the deep-link router: this
            section id is live and still means the offload pool, so aliasing it
            would hijack every legitimate link to this page.
          -->
          <hr class="card-divider lg" />
          <small class="hint">
            <strong>Injection protection moved.</strong> The master switch, the
            per-feature matrix, the external fetch budgets, native web tools and
            injection detection are now a top-level Settings category of their
            own — they govern every AI tab, not just the offload worker.
          </small>
          <div class="button-row">
            <button type="button" class="secondary" onclick={() => (activeSection = 'injection')}>
              Open Injection protection
            </button>
          </div>

          <Toggle
            label="Escalate partial fast-tier answers to the quality backend"
            checked={snapshot.offload.escalate_partial}
            onchange={(next) => patch((s) => (s.offload.escalate_partial = next))}
          />
          <small class="hint">
            When a fast-tier offload comes back only partially verified, re-run it
            once on a distinct, ready quality backend and keep the better answer.
            Inert unless a second, quality-tier backend is configured.
          </small>
          {:else}
          <h3>Native tools</h3>
          <Toggle
            label="read_file — bounded file reads"
            checked={snapshot.offload.tools.read_file}
            onchange={(next) => patch((s) => (s.offload.tools.read_file = next))}
          />
          <Toggle
            label="list_dir — enumerate a directory (what files exist / how many)"
            checked={snapshot.offload.tools.list_dir}
            onchange={(next) => patch((s) => (s.offload.tools.list_dir = next))}
          />
          <Toggle
            label="code_search — literal search across the roots"
            checked={snapshot.offload.tools.code_search}
            onchange={(next) => patch((s) => (s.offload.tools.code_search = next))}
          />
          <Toggle
            label="run_command — allowlisted, read-only commands"
            checked={snapshot.offload.tools.run_command}
            onchange={(next) => patch((s) => (s.offload.tools.run_command = next))}
          />
          <Toggle
            checked={snapshot.offload.tools.run_check}
            onchange={(next) => patch((s) => (s.offload.tools.run_check = next))}
          >
            run_check — run a configured project check (build/typecheck/lint/test).
            Inert until the project's <code>checks</code> are configured.
          </Toggle>

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
                    <!-- `icon` opts out of the TUI themes' `[ … ]` bracket
                         framing — brackets around a lone × wrap it tall. -->
                    <button
                      type="button"
                      class="secondary icon"
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

          <h3>Cross-harness delegation</h3>
          <small class="hint top">
            V39: one tab drives another — cImp types a request into an open
            harness tab exactly as you would, waits for the turn to finish, and
            hands the answer back to the tab that asked. Which tabs may be driven
            is set per tab, from that tab's <code>⇄</code> icon; these are
            the two knobs that are not per tab. Every run is a row in the Events
            tab under <strong>delegation</strong>.
          </small>
          <Toggle
            label="Lock a tab's keyboard while another harness is driving it"
            checked={snapshot.delegation.auto_read_only}
            onchange={(next) => patch((s) => (s.delegation.auto_read_only = next))}
          />
          <small class="hint">
            On by default. While cImp is typing into a tab, a stray keystroke of
            yours lands in the middle of someone else's turn. A courtesy lock over
            your own hands, not a security boundary: a permission or question
            prompt relaxes it for that prompt, and <strong>Take over</strong> — on
            the tab's <code>⇄</code> popover and its context menu — clears it
            outright and ends the delegation. Turning it off leaves the tab
            writable throughout; the banner and the glyph still say it is being
            driven.
          </small>
          <NumberField
            label="Default timeout (seconds)"
            min="1"
            max="86400"
            step="1"
            value={snapshot.delegation.default_timeout_s}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.delegation.default_timeout_s = Math.max(
                    1,
                    Math.round(+next || 600),
                  )),
              )}
          >
            <small class="hint">
              How long cImp waits for a worker's reply when the caller named no
              timeout of its own. On expiry the asking tab is told
              <code>timeout</code> and <strong>no keys are ever sent</strong> to
              cancel the worker — it finishes its turn visibly, in its own tab.
              A standing permission prompt buys one bounded extension, so a run
              waiting on you does not expire while you walk over to it.
            </small>
          </NumberField>
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
            <strong>Tools</strong> tab's <strong>Offload server</strong>
            section.
          </small>
        </section>
      {:else if activeSection === 'injection'}
        <section>
          <!--
            V32 Phase G (locked decision 16): the three-level enable hierarchy.
            Placed AHEAD of the individual V32 blocks below (budgets, native
            web, detection) because it governs all of them: a user who has come
            here to turn something off should meet the master switch before the
            tuning knobs.

            F-18: this whole group used to be three headings at the BOTTOM of
            "Offload task tools" → Pool, below the backend list and the limits,
            while every pointer to it in the app and in the docs sent the user to
            a Settings "Tools" section that has never existed. It governs every
            AI tab and the MCP surface as much as the offload worker, so it is
            its own top-level category now; the group heading became the
            section heading, and nothing else about these controls moved.
            `settingsPointers.test.ts` is the tripwire that keeps the pointers
            and this sidebar's labels from drifting apart again.
          -->
          <h2>Injection protection</h2>
          <small class="hint top">
            Every V32 containment control has three levels of switch: this master,
            a per-feature switch app-wide, and a per-scope override. A control is
            on when the master is on <em>and</em> either the scope says so or the
            feature does. An override can re-enable a feature its app-wide switch
            disabled; nothing re-enables a containment control past the master.
            Since V39 this master and every per-feature switch ship <em>on</em>,
            and a <strong>newly created AI tab has all of its own overrides off</strong>
            — so a tab's own row is where protection is actually engaged, from
            the shield badge on the tab itself or from the per-scope selects
            below. Tabs that existed before V39 keep their behaviour: the upgrade
            wrote <code>inherit</code> into every cell they had not set.
            (<em>Managed-tool steering</em> below is not a containment control —
            it is a token-efficiency nudge — so the master switch does not reach
            it; its own two switches still do.)
          </small>
          <Toggle
            label="Injection protection (master switch)"
            checked={snapshot.offload.injection.protection}
            onchange={(next) => patch((s) => (s.offload.injection.protection = next))}
          />
          {#if !snapshot.offload.injection.protection}
            <small class="hint down">
              ⚠ <strong>Every containment control is off</strong> — for every tab
              and the offload worker. No taint latch, no spotlighting envelope, no
              SSRF screen, no fetch budgets, no canary, no memory quarantine, no
              native-web visibility, no consumer hygiene, no escape stripping.
              Fetched pages reach the model as raw text and a research session can
              read your files and call out to the web in the same turn. This is the
              documented escape hatch for when a control misfires on real work; the
              per-feature switches below are the smaller instrument.
              <br />
              <em>Managed-tool steering</em> is the one row below this switch does
              not touch: it injects no protection, only a paragraph asking the
              harness to prefer cImp's <code>run_check</code> / <code>run_command</code>
              tools over its own shell. This switch reduces your security posture; a
              token-budget preference is not posture, so that row stays live and
              keeps its own switches.
            </small>
          {/if}
          {#if injectionAppRestartRequired}
            <!--
              #48 (F-x): the app-wide half of the restart hint. The per-tab hint
              in the Tabs section diffs a tab's own L3 cells; the master switch
              and the three app-wide L2 inputs move the backend's spawn
              signature too, and until now nothing in this window said so. It
              stays up until a tab is restarted from Settings, because there is
              no way to tell from here that they all have been.
            -->
            <small class="hint down">
              ⚠ Spawn-baked changes are pending. The master switch, the
              spotlighting envelope, the native web tools mode, consumer hygiene,
              managed-tool steering and harness native-tool gating are baked
              into an AI tab when it launches, so every running tab
              keeps the posture it started with — restart them (Settings → Tabs
              → Restart) for these to apply.
            </small>
          {/if}
          {#if injectionRows.length === 0}
            <small class="hint down">
              ⚠ The resolved injection state could not be read from the backend,
              so the per-feature matrix cannot be rendered — it is built from
              that report rather than from a second copy of the feature list.
              The master switch above still applies. Check the console.
            </small>
          {/if}
          {#each injectionRows as f (f.key)}
            <div class="updater-row">
              <label class="checkbox">
                <input
                  type="checkbox"
                  disabled={(f.masterGated && !snapshot.offload.injection.protection) ||
                    f.field === null}
                  checked={injectionL2On(f)}
                  onchange={(e) => {
                    // A feature with no boolean L2 (native-web: its L2 IS the
                    // tri-mode select in "Native web tools" below) shows the
                    // derived value read-only. Guarded as well as `disabled`
                    // because a checkbox that could write here would put the
                    // same decision in two controls — the contradictory state
                    // the Phase G reconciliation exists to prevent.
                    const field = f.field;
                    if (field === null) return;
                    const on = (e.currentTarget as HTMLInputElement).checked;
                    patch((s) => ((s.offload.injection[field] as boolean) = on));
                  }}
                />
                <!-- The mode word is rendered for the ONE feature whose L2 is a
                     tri-mode rather than a boolean, because its checkbox cannot
                     say which of the two live modes is in force and "on" was
                     read as "denying" (F-18's companion defect). -->
                <span
                  >{f.label}{f.spawnBaked ? ' (needs a tab restart)' : ''}{#if f.key === 'native_web'}
                    — <strong>{nativeWebModeWord}</strong>
                  {/if}</span
                >
              </label>
              {#if f.hint}<small class="hint">{f.hint}</small>{/if}
              {#if injectionScopeRows(f).length > 0}
                <div class="row">
                  {#each injectionScopeRows(f) as sc (sc.scope)}
                    <label class="inline-override">
                      <span>{sc.label}</span>
                      <select
                        value={sc.value}
                        onchange={(e) =>
                          setInjectionOverride(
                            sc.scope,
                            f.key,
                            (e.currentTarget as HTMLSelectElement).value,
                          )}
                      >
                        <option value="inherit">Inherit ({sc.inherited ? 'on' : 'off'})</option>
                        <option value="on">On</option>
                        <option value="off">Off</option>
                      </select>
                      <span class="mcp-detail">{sc.resolved}</span>
                    </label>
                  {/each}
                </div>
              {:else}
                <small class="hint">
                  App-wide only — the backend reports no per-tab or per-worker
                  override row for this control, so there is nothing narrower to
                  set. (Terminal escape hygiene is the case that exists today:
                  TTS and toasts are global surfaces.)
                </small>
              {/if}
            </div>
          {/each}

          <NumberField
            label="External fetch budget — calls (0 = unlimited)"
            min="0"
            value={snapshot.offload.external_fetch_max_calls}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.offload.external_fetch_max_calls = Math.max(
                    0,
                    Math.floor(+next) || 0,
                  )),
              )}
          >
            <small class="hint">
              How many external (web / MCP-server) tool calls one offload task —
              or one AI tab session — may make before further ones
              are refused. Generous by design: it stops runaway fetch loops and
              bulk data staging, not research.
            </small>
          </NumberField>
          <NumberField
            label="External fetch budget — bytes (0 = unlimited)"
            min="0"
            value={snapshot.offload.external_fetch_max_bytes}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.offload.external_fetch_max_bytes = Math.max(
                    0,
                    Math.floor(+next) || 0,
                  )),
              )}
          >
            <small class="hint">
              Cumulative bytes of external content one task/session may pull.
              Exhausting either budget refuses further external calls and writes
              one flagged row to Tools → Activities.
            </small>
          </NumberField>

          <h3>Native web tools</h3>
          <small class="hint top">
            cImp's containment latch only sees web access that goes through its
            own proxy. {nativeWebToolsByHarness} bypass it
            entirely, so without one of the modes below a tab can read a hostile
            page while cImp still believes it is clean. Takes effect when an AI
            tab is <strong>restarted</strong>.
          </small>
          <label>
            <span>Native web visibility</span>
            <select
              value={snapshot.offload.native_web_visibility}
              onchange={(e) => {
                const v = (e.currentTarget as HTMLSelectElement).value;
                patch((s) => (s.offload.native_web_visibility = v));
              }}
            >
              <option value="off">Off — no interference, no visibility</option>
              <option value="sensor">Sensor — report only (default)</option>
              <option value="deny">Deny — the harness refuses its own web tools</option>
            </select>
            <small class="hint">
              <strong>Sensor</strong> installs a report-only hook on the two web
              tools (nothing else — no cost on Read/Grep/Bash): using one engages
              that tab's external latch and raises its taint badge, exactly as a
              proxied fetch would. It never blocks a call, and a failure is
              silent.
              <strong>Deny</strong> closes the route by configuration, so all web
              flows through the proxied <code>ddg</code>/MCP tools where the latch
              is fully effective — pair it with local/proxied web servers.
              <strong>Off</strong> is the escape hatch if a hook misbehaves.
              In every mode, shell-level access (<code>curl</code> in Bash) stays
              invisible.
            </small>
            <!-- F-18's companion defect, at the source of it: a `<select>` whose
                 stored value matches none of its options renders BLANK, which
                 reads as "not set" while the backend is enforcing `sensor`
                 regardless. The mode in force is stated rather than left to the
                 widget — and it is the same string the matrix row above shows,
                 so the two cannot disagree. -->
            <small class="hint">
              In force now: <strong>{nativeWebModeWord}</strong>. Only
              <strong>deny</strong> refuses a call.
            </small>
          </label>

          <h3>Injection detection</h3>
          <small class="hint top">
            Screens the text every external tool brings back (fetched pages, docs
            lookups) for prompt-injection content. Both layers are
            <strong>surface-only</strong>: a hit prepends a warning header for the
            reading model and writes a flagged row to Tools → Activities — nothing
            is ever blocked, withheld or modified, so a false positive costs a line
            of noise, not a broken task.
          </small>
          {#if detection}
            <ul class="mcp-health">
              <!-- #48/N-3: the dot binds the BACKEND's predicate. Deriving it
                   here as `files_failed === 0 && files_loaded > 0` omitted
                   `rules`, which the updater's own health check requires, so a
                   .yar file that parsed and defined nothing rendered green
                   beside "1 file(s) loaded, 0 rule(s)" while scan returned
                   empty. One predicate, in one language. -->
              <li class:healthy={detection.rules.healthy} class:down={!detection.rules.healthy}>
                <span class="mcp-dot" aria-hidden="true"></span>
                <span class="mcp-name">Signature rules</span>
                <span class="mcp-detail" title={detection.rules.dir}>
                  {detection.rules.files_loaded} file(s) loaded, {detection.rules.rules} rule(s){detection.rules.files_failed > 0
                    ? ` — ${detection.rules.files_failed} failed: ${detection.rules.failed.join(', ')}`
                    : ''}{!detection.rules.armed
                    ? ' — the signature layer has nothing to match with'
                    : ''}
                </span>
              </li>
              {#if detection.local_rules_broken?.failed.length}
                <!-- #48/U-4's other half: once a broken `local/` rule stopped
                     vetoing the update channel it stopped being loud, and its
                     only trace was a `warn!` line in a log nobody has open.
                     The Advisor card is the nudge; this is the row in the place
                     the user goes to look. Same backend predicate, so the two
                     cannot disagree about whether their rules are live. -->
                <li class="down">
                  <span class="mcp-dot" aria-hidden="true"></span>
                  <span class="mcp-name">Your rule files</span>
                  <span class="mcp-detail" title={detection.local_rules_broken.dir}>
                    {detection.local_rules_broken.failed.length} file(s) in
                    <code>rules.d/local/</code> did not compile and are NOT matching:
                    {detection.local_rules_broken.failed.join(', ')} — the rest of the
                    set ({detection.local_rules_broken.rules} rule(s)) is live. Fix the
                    file and press Reload rules.
                  </span>
                </li>
              {/if}
              {#if detection.local_rules_broken?.renamed.length}
                <!-- #48/M-13: a rule of the user's whose identifier a shipped
                     rule has taken. It IS live and IS matching, so this is
                     deliberately NOT the `down` row above — describing it in
                     the broken file's words would be the same "degraded path
                     reporting the wrong thing" shape this milestone keeps
                     finding. It still needs a row: the identifier a hit
                     reports is no longer the one their file spells. -->
                <li class="healthy">
                  <span class="mcp-dot" aria-hidden="true"></span>
                  <span class="mcp-name">Your renamed rules</span>
                  <span class="mcp-detail" title={detection.local_rules_broken.dir}>
                    {detection.local_rules_broken.renamed.length} rule(s) in
                    <code>rules.d/local/</code> declare an identifier the shipped bundle
                    also uses, so cImp loaded yours under a renamed one and they keep
                    matching:
                    {detection.local_rules_broken.renamed
                      .map((r) => `${r.from} → ${r.to} (${r.file})`)
                      .join(', ')} — a hit reports the NEW identifier. Your files were
                    not modified; rename the rule yourself to take the name back.
                  </span>
                </li>
              {/if}
              <li class:healthy={detection.classifier.present} class:down={!detection.classifier.present}>
                <span class="mcp-dot" aria-hidden="true"></span>
                <span class="mcp-name">Classifier</span>
                <span class="mcp-detail" title={detection.classifier.dir}>
                  {detection.classifier.present
                    ? 'Prompt Guard 2 weights loaded'
                    : (detection.classifier.error ?? 'weights not installed')}
                </span>
              </li>
            </ul>
          {:else}
            <!-- #48/H-10: "unavailable" is a third state, not a quiet "fine".
                 The old parenthetical guessed a cause ("still starting") that a
                 permanently failing `detection_status` makes untrue, and this
                 panel is where a user goes to check. -->
            <small class="hint">
              Detection status unavailable — cImp could not read it. It may still
              be starting; if this persists, the layers below are UNVERIFIED
              rather than known to be off. Check the console.
            </small>
          {/if}
          <div class="row">
            <button type="button" onclick={reloadDetection}>Reload rules</button>
            <button type="button" onclick={() => void detectionOpenRulesFolder()}>
              Open rules folder
            </button>
          </div>
          <small class="hint">
            Rules are plain <code>.yar</code> files next to cimp.exe under
            <code>detection/rules.d/</code>. Drop your own in the
            <code>local/</code> subfolder — the auto-updater below replaces the
            shipped bundle but never touches <code>local/</code>. A file that
            fails to compile is skipped and the rest still load.
          </small>
          <Toggle
            label="Signature screen (YARA rules)"
            checked={snapshot.offload.detection_signature_enabled}
            onchange={(next) => patch((s) => (s.offload.detection_signature_enabled = next))}
          />
          <Toggle
            label="Classifier screen (Prompt Guard 2)"
            checked={snapshot.offload.detection_classifier_enabled}
            onchange={(next) => patch((s) => (s.offload.detection_classifier_enabled = next))}
          />
          {#if detection && !detection.classifier.present}
            <small class="hint">
              Optional and not bundled — cImp does not ship these weights, because
              they are under the Llama Community Licence rather than the permissive
              licences the TTS and speech models use. The layer stays inert until
              you install them, and that is a supported configuration: the YARA
              signature screen carries detection on its own. To enable it, put
              <code>model.onnx</code> and <code>tokenizer.json</code> in
              <code>models/promptguard2-22m/</code> and restart. An ungated ONNX
              export lives at
              <code>huggingface.co/gravitee-io/Llama-Prompt-Guard-2-22M-onnx</code>
              — it offers a 284&nbsp;MB fp32 build and a 72&nbsp;MB int8 one
              (<code>model.quant.onnx</code>, rename it). Digests to verify against,
              and the requirements any other export must meet, are in
              <code>models/CHECKSUMS.txt</code>.
            </small>
          {/if}
          <NumberField
            label="Classifier threshold (0–1)"
            min="0"
            max="1"
            step="0.01"
            value={snapshot.offload.detection_classifier_threshold}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.offload.detection_classifier_threshold = Math.min(
                    1,
                    Math.max(0, +next || 0),
                  )),
              )}
          >
            <small class="hint">
              Probability at or above which the classifier flags a result. Lower
              catches more and warns more often; 0.9 is the conservative default,
              because a header on every page trains the model to ignore it.
            </small>
          </NumberField>

          <h4>Detection updates</h4>
          <small class="hint top">
            Signature rules go stale: they only match phrasings someone has
            already written down. cImp checks a curated manifest
            (its own GitHub release, never third-party repos) on a daily
            interval. A candidate bundle is verified by SHA-256, compiled, and
            run against shipped control documents — it must catch the known
            attacks and must NOT flag the benign ones — before it goes live. If
            a candidate bundle is refused, the old data stays active and you get
            a card; detection never silently degrades to nothing. If the channel
            simply cannot be reached — offline, a proxy, a release that is not
            published yet — that is reported here and nowhere else, until it has
            been unreachable long enough to mean this component has stopped
            getting fresher. Every check follows the <em>app-wide</em> answer of
            the <em>Injection detection</em> switch above (and the master switch
            above that): with detection off app-wide, nothing is polled or
            swapped — not on the daily schedule and not from the buttons below.
            Switching it on for one AI tab counts as app-wide here, because there
            is one rule bundle on disk for the whole app; switching it on for the
            offload worker alone does not start the updater, and the worker goes
            on screening with the bundle it already has.
          </small>
          <!--
            #48 (M-21): and WHICH of those states is in force, in the words
            `ipc::commands::updates_allowed` refuses with. The paragraph above
            states the rule; this states the fact, and it is rendered rather than
            left to the tooltips because a disabled button does not reliably
            raise one (the same reason the button row carries a title of its
            own). Absent — including when the status could not be read at all —
            nothing is claimed.
          -->
          {#if detectionUpdatesOffReason}
            <small class="hint">{detectionUpdatesOffReason}</small>
          {/if}
          {#if detection}
            {#each detection.updater.components as comp (comp.component)}
              <div class="updater-row">
                <div class="row">
                  <strong>Signature rules</strong>
                  <span class="mcp-detail">
                    installed: <code>{comp.installed_version || '(shipped)'}</code>
                    {#if comp.available_version && comp.available_version !== comp.installed_version}
                      · available: <code>{comp.available_version}</code>
                    {/if}
                    {#if comp.last_check_ms > 0}
                      · checked {new Date(comp.last_check_ms).toLocaleString()}
                    {:else}
                      · never checked
                    {/if}
                  </span>
                </div>
                <SelectField
                  label="Update mode"
                  value={snapshot.offload.detection_update_rules_mode}
                  onchange={(next) => {
                    const v = next;
                    patch((s) => {
                      s.offload.detection_update_rules_mode = v;
                    });
                  }}
                >
                  <option value="off">Off — never check</option>
                  <option value="check">Check only — tell me, change nothing</option>
                  <option value="auto">Auto — validate and apply</option>
                </SelectField>
                <!--
                  #48: all three are gated on the resolved detection feature,
                  not only on `detectionBusy`. The IPC commands refuse too — a
                  disabled attribute is a courtesy, not a control — and the
                  tooltip sits on the ROW as well as on each button, because a
                  disabled button does not reliably raise one.

                  #48 (M-21): all four tooltips render the SAME
                  `detectionUpdatesOffReason`. They used to carry four separate
                  literals of one claim, and that claim named a cause nobody had
                  checked. What gates the buttons is unchanged — the reason
                  string decides nothing and is never read by `disabled`.
                -->
                <div class="row" title={detectionUpdatesOffReason}>
                  <button
                    type="button"
                    onclick={() => void checkDetectionUpdate(comp.component, false)}
                    disabled={detectionBusy !== null || !detectionUpdatesEnabled}
                    title={detectionUpdatesOffReason}
                  >
                    {detectionBusy === comp.component ? 'Checking…' : 'Check now'}
                  </button>
                  <button
                    type="button"
                    onclick={() => void checkDetectionUpdate(comp.component, true)}
                    disabled={detectionBusy !== null ||
                      !detectionUpdatesEnabled ||
                      !comp.available_version}
                    title={detectionUpdatesOffReason}
                  >
                    Apply update
                  </button>
                  <button
                    type="button"
                    onclick={() => void revertDetection(comp.component)}
                    disabled={detectionBusy !== null ||
                      !detectionUpdatesEnabled ||
                      !comp.can_revert}
                    title={detectionUpdatesOffReason}
                  >
                    Revert to {comp.previous_version || 'previous'}
                  </button>
                </div>
                {#if comp.last_outcome_kind === 'unavailable'}
                  <!--
                    #46: the channel could not be REACHED. Nothing was refused,
                    so this is deliberately NOT the unhealthy colour — a 404, a
                    proxy or an offline laptop is not a security event, and
                    painting it red is what made the real rejection colour
                    meaningless. The streak is shown because "for how long" is
                    the part that eventually matters.
                  -->
                  <small class="hint">
                    Could not reach the update channel: {comp.last_outcome}
                    {#if comp.unreachable_streak > 1}
                      ({comp.unreachable_streak} checks in a row — the installed
                      data is still live, but this component is not getting
                      fresher.)
                    {/if}
                  </small>
                {:else if comp.last_outcome}
                  <small class="hint" class:down={!comp.last_ok}>
                    Last check: {comp.last_outcome}
                  </small>
                {/if}
                {#if comp.unrestored_files.length}
                  <!--
                    #48/M-11: the only line in this block that means "degraded
                    RIGHT NOW". A failed rollback left the live directory short
                    of files that are still in the retained copy — the state no
                    other readout here can express, because everything that is
                    present compiles clean. Unhealthy colour, unconditionally:
                    unlike a refusal, this is not "the old data is still live".
                  -->
                  <small class="hint down">
                    Incomplete rule set: {comp.unrestored_files.length} file(s) could
                    not be restored ({comp.unrestored_files.join(', ')}) and are
                    missing from the live folder. They are still retained, and cImp
                    retries on every check and every launch — close anything holding
                    them open (antivirus, an editor) and restart.
                  </small>
                {/if}
                {#if comp.available_notes}
                  <small class="hint">Release note: {comp.available_notes}</small>
                {/if}
              </div>
            {/each}
            <small class="hint">
              Manifest: <code>{detection.updater.manifest_url}</code>
            </small>
          {/if}
          <NumberField
            label="Check interval (hours)"
            min="1"
            max="720"
            step="1"
            value={snapshot.offload.detection_update_interval_hours}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.offload.detection_update_interval_hours = Math.max(
                    1,
                    Math.round(+next || 24),
                  )),
              )}
          >
            <small class="hint">
              Also checked once shortly after launch, and skipped if the last
              check was inside this window — a restart does not re-download.
              Floored at 1 hour.
            </small>
          </NumberField>
          <label>
            <span>Manifest URL override</span>
            <input
              type="text"
              placeholder="(the pinned cImp detection manifest)"
              value={snapshot.offload.detection_update_manifest_url}
              onchange={(e) =>
                patch(
                  (s) =>
                    (s.offload.detection_update_manifest_url = (
                      e.currentTarget as HTMLInputElement
                    ).value.trim()),
                )}
            />
            <small class="hint">
              Leave empty for the pinned URL. Downloads must live under the same
              directory as whatever manifest is in force, so an override
              relocates the whole bundle rather than letting a manifest point at
              a host of its choosing. Mainly for testing a staged bundle.
            </small>
          </label>
        </section>
      {:else if activeSection === 'mcp'}
        <section>
          <h2>MCP servers</h2>
          <small class="hint top">
            Model Context Protocol servers cImp connects to and keeps warm. Each
            server's read-class tools (web search, fetch, docs, …) can be exposed
            to <strong>{harnessNamesProse}</strong> and/or to the
            <strong>offload worker</strong> — per server, below.
            Write/destructive tools are filtered out. Exposing a server to a
            harness works whether or not offload is enabled.
          </small>
          <!-- V37 Phase D (contract C8): the body of this section is
               `McpManagementEditor` — registry, categories, per-project
               activation and health chips. The two callbacks are the whole
               contract between it and this file, which owns `snapshot`. -->
          <McpManagementEditor
            servers={snapshot.offload.mcp_servers}
            categories={snapshot.offload.mcp_categories}
            activation={snapshot.offload.mcp_activation}
            health={serviceStatus?.mcp_servers ?? []}
            healthIntervalSecs={snapshot.offload.mcp_health_interval_secs}
            onedit={setMcpRegistry}
            onapply={applyMcpRegistry}
            onhealthinterval={(secs) =>
              patch((s) => (s.offload.mcp_health_interval_secs = secs))}
          />
        </section>
      {:else if activeSection === 'graph'}
        <section>
          <h2>Code Intelligence</h2>
          <small class="hint top">
            Build a per-project graph of your code and docs (symbols, calls,
            imports, doc-comments), stored at
            <code>&lt;project&gt;/.cimp/graph.db</code> and kept live by a file
            watcher. The harness session queries it through
            <code>graph_*</code> tools (re-launch a tab to pick them up) instead
            of grepping. Off by default; everything stays on this machine.
          </small>
          <Toggle
            label="Enable code graph"
            checked={snapshot.graph.enabled}
            onchange={(next) => patch((s) => (s.graph.enabled = next))}
          />

          {#if snapshot.graph.enabled}
            <hr class="card-divider lg" />
            <div class="sub-tabs" role="tablist" aria-label="Code Intelligence sub-sections">
              <button
                type="button"
                role="tab"
                class:active={graphSubSection === 'graph'}
                aria-selected={graphSubSection === 'graph'}
                onclick={() => (graphSubSection = 'graph')}
              >
                Code graph
              </button>
              <button
                type="button"
                role="tab"
                class:active={graphSubSection === 'semantic'}
                aria-selected={graphSubSection === 'semantic'}
                onclick={() => (graphSubSection = 'semantic')}
              >
                Semantic search
              </button>
              <button
                type="button"
                role="tab"
                class:active={graphSubSection === 'efficiency'}
                aria-selected={graphSubSection === 'efficiency'}
                onclick={() => (graphSubSection = 'efficiency')}
              >
                Token efficiency
              </button>
              <button
                type="button"
                role="tab"
                class:active={graphSubSection === 'viz'}
                aria-selected={graphSubSection === 'viz'}
                onclick={() => (graphSubSection = 'viz')}
              >
                Graph view
              </button>
            </div>

            {#if graphSubSection === 'graph'}
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
            <Toggle
              label="Index Markdown docs + doc-comments (powers doc search)"
              checked={snapshot.graph.index_docs}
              onchange={(next) => patch((s) => (s.graph.index_docs = next))}
            />
            <NumberField
              label="Max file size (bytes)"
              min="1024"
              value={snapshot.graph.max_file_bytes}
              onchange={(next) =>
                patch(
                  (s) =>
                    (s.graph.max_file_bytes = Math.max(
                      1024,
                      Number(next) || 1048576,
                    )),
                )}
            />
            <NumberField
              label="Watcher debounce (ms)"
              min="50"
              value={snapshot.graph.watch_debounce_ms}
              onchange={(next) =>
                patch(
                  (s) =>
                    (s.graph.watch_debounce_ms = Math.max(
                      50,
                      Number(next) || 300,
                    )),
                )}
            />

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

            <h3>Tool surface</h3>
            <Toggle
              label="Lean tool surface (hide cold-tail graph tools)"
              checked={snapshot.graph.lean_tools}
              onchange={(next) => patch((s) => (s.graph.lean_tools = next))}
            />
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
              (Tools tab).
              Edge confidence (extracted/inferred/ambiguous) is always on.
            </small>
            <NumberField
              label="Path tracing max hops (1–32)"
              min="1"
              max="32"
              value={snapshot.graph.path_max_hops}
              onchange={(next) =>
                patch(
                  (s) =>
                    (s.graph.path_max_hops = Math.min(
                      32,
                      Math.max(1, Number(next) || 8),
                    )),
                )}
            />
            <NumberField
              label="Max subsystems reported"
              min="1"
              value={snapshot.graph.arch_max_communities}
              onchange={(next) =>
                patch(
                  (s) =>
                    (s.graph.arch_max_communities = Math.max(
                      1,
                      Number(next) || 12,
                    )),
                )}
            />
            <NumberField
              label="Minimum subsystem size"
              min="1"
              value={snapshot.graph.arch_min_community_size}
              onchange={(next) =>
                patch(
                  (s) =>
                    (s.graph.arch_min_community_size = Math.max(
                      1,
                      Number(next) || 3,
                    )),
                )}
            />

            <h3>Offload worker access</h3>
            <Toggle
              checked={snapshot.graph.allow_remote_worker_access}
              onchange={(next) => patch((s) => (s.graph.allow_remote_worker_access = next))}
            >
              Allow a <strong>remote</strong> offload worker to query the graph
            </Toggle>
            <small class="hint">
              ⚠ <strong>Privacy:</strong> the local offload worker can always
              query the graph. A <strong>remote</strong> backend — whether a box
              on your LAN or a public cloud API — would receive your project's
              code structure (symbol names, call relationships, doc snippets).
              Leave this off unless you trust the remote. A harness tab's own
              <code>graph_*</code> tools are unaffected by this
              setting.
            </small>
            {:else if graphSubSection === 'semantic'}
            <h3>Semantic search</h3>
            <Toggle
              label="Enable semantic (embedding) doc search"
              checked={snapshot.graph.semantic_search}
              onchange={(next) => patch((s) => (s.graph.semantic_search = next))}
            />
            <small class="hint">
              Needs an OpenAI-compatible <code>/v1/embeddings</code> endpoint
              (e.g. a <code>llama-server --embedding</code> on a spare GPU box).
              Degrades to full-text search when the endpoint is unreachable; the
              structural graph never depends on it. Toggling this changes the
              tools and guidance an AI tab sees — restart AI tabs
              to pick it up.
            </small>
            <h3>Embedding server</h3>
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
                <span>Auth token (optional)</span>
                <!-- V33: the embedding server is usually a llama-server on a
                     spare box, i.e. on the LAN. `?? ''` guards the pre-V33
                     settings file that has no such key. Not `.trim()`-ed on
                     write like the endpoint above: a token is opaque and
                     trimming it would silently alter a credential. -->
                <input
                  type="password"
                  value={snapshot.graph.embedding_auth_token ?? ''}
                  onchange={(e) =>
                    patch(
                      (s) =>
                        (s.graph.embedding_auth_token = (
                          e.currentTarget as HTMLInputElement
                        ).value),
                    )}
                />
                <small class="hint">
                  Sent as a <code>Bearer</code> header to the endpoint above.
                  Leave empty for no auth. Stored cleartext in
                  <code>settings.json</code>.
                </small>
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
              <NumberField
                label="Embedding dimensions (0 = auto-probe)"
                min="0"
                value={snapshot.graph.embedding_dims}
                onchange={(next) =>
                  patch(
                    (s) =>
                      (s.graph.embedding_dims = Math.max(
                        0,
                        Number(next) || 0,
                      )),
                  )}
              />
              <NumberField
                label="Embedding max tokens (0 = auto-detect)"
                min="0"
                value={snapshot.graph.embedding_max_tokens}
                onchange={(next) =>
                  patch(
                    (s) =>
                      (s.graph.embedding_max_tokens = Math.max(
                        0,
                        Number(next) || 0,
                      )),
                  )}
              />
              <small class="hint">
                0 = auto-detect from the server (a <code>llama-server</code>
                reports its context window on <code>/props</code>). Longer texts
                are truncated to fit before they're sent — without this, one
                oversized chunk makes the endpoint reject the whole batch. Set it
                manually only for a server that exposes no <code>/props</code>.
              </small>
              <small class="hint">
                Changing the model or dimensions starts a background re-embed.
                Use <strong>Rebuild embeddings</strong> in Tools →
                Graph index after a silent model swap behind the same name.
              </small>
            {:else if graphSubSection === 'efficiency'}
            <h3>Context injection</h3>
            <Toggle
              label="Auto-inject relevant file digests into each prompt"
              checked={snapshot.graph.context_injection}
              onchange={(next) => patch((s) => (s.graph.context_injection = next))}
            />
            <small class="hint">
              Prepends a budget-bounded digest of the most relevant files to each
              prompt ({injectMechanisms}). Off by default — it
              changes what the agent sees. Re-launch a tab to pick it up. Tune and
              preview it on the <strong>Context</strong> section of the Code
              Intelligence tab.
            </small>
            {#if snapshot.graph.context_injection}
              <NumberField
                label="Per-file budget (chars)"
                min="100"
                value={snapshot.graph.context_per_file_chars}
                onchange={(next) =>
                  patch(
                    (s) =>
                      (s.graph.context_per_file_chars = Math.max(
                        100,
                        Number(next) || 800,
                      )),
                  )}
              />
              <NumberField
                label="Per-turn budget (chars)"
                min="500"
                value={snapshot.graph.context_turn_budget_chars}
                onchange={(next) =>
                  patch(
                    (s) =>
                      (s.graph.context_turn_budget_chars = Math.max(
                        500,
                        Number(next) || 6000,
                      )),
                  )}
              />
              <NumberField
                label="Min relevance score (skip below)"
                min="0"
                value={snapshot.graph.context_min_score}
                onchange={(next) =>
                  patch((s) => {
                    // 0 is a valid value (no threshold), so keep it — a bare
                    // `|| 3` would treat the falsy 0 as "unset" and revert it.
                    const n = Number(next);
                    s.graph.context_min_score = Number.isFinite(n) ? Math.max(0, n) : 3;
                  })}
              />
              <Toggle
                label="Rank session-hot files first (from Memory)"
                checked={snapshot.graph.context_include_session}
                onchange={(next) => patch((s) => (s.graph.context_include_session = next))}
              />
              <NumberField
                label="Dedup TTL (turns, 0 = re-inject every turn)"
                min="0"
                value={snapshot.graph.context_dedup_ttl_turns}
                onchange={(next) =>
                  patch((s) => {
                    // 0 is a valid value (dedup off), so keep it — a bare
                    // `|| 10` would treat the falsy 0 as "unset" and revert it.
                    const n = Number(next);
                    s.graph.context_dedup_ttl_turns = Number.isFinite(n) ? Math.max(0, n) : 10;
                  })}
              />
              <small class="hint">
                A file injected in full is demoted to a one-line "unchanged"
                reminder on later turns until it changes or this many turns pass.
              </small>
              <Toggle
                label="Prepend the project map to each new session's first turn"
                checked={snapshot.graph.repo_map_on_session_start}
                onchange={(next) => patch((s) => (s.graph.repo_map_on_session_start = next))}
              />
              <Toggle
                label="Feed working set + pinned notes to the harness's compactor"
                checked={snapshot.graph.compaction_context}
                onchange={(next) => patch((s) => (s.graph.compaction_context = next))}
              />
              <small class="hint">
                On compaction (<code>PreCompact</code> hook) the session's working
                set and pinned notes are handed to the summarizer so they survive.
                Costs a few hundred chars once per compaction. Re-launch the tab to
                pick up changes.
              </small>
            {/if}

            <Toggle
              label="Redundant-read advisor"
              checked={snapshot.graph.read_advisor}
              disabled={e1Blocked}
              onchange={(next) => patch((s) => (s.graph.read_advisor = next))}
            />
            {#if e1Gate}
              <!--
                V35 Phase E: the sentence comes from the gate itself
                (`harness::contract::gate`) rather than being written out again
                here. The rule and the explanation of the rule were two things
                to keep in sync; now the code that decides is the code that
                says why, and a new gate arrives with its own wording.
              -->
              <small class="hint">Blocked: {e1Gate.reason}</small>
            {:else}
              <small class="hint">
                Intercepts a <code>Read</code> of a file already read unchanged this
                session and answers with a cheap outline reminder instead of
                re-reading it. Changes the agent's tool behaviour — strictly opt-in.
                It needs a harness that can deny a tool call before it runs, so it
                reaches only tabs whose harness declares that. Re-launch the tab to
                pick it up.
              </small>
            {/if}
            {#if snapshot.graph.read_advisor && !e1Blocked}
              <NumberField
                label="Min file size to advise (lines)"
                min="0"
                value={snapshot.graph.read_advisor_min_lines}
                onchange={(next) =>
                  patch(
                    (s) =>
                      (s.graph.read_advisor_min_lines = Math.max(
                        0,
                        Number(next) || 300,
                      )),
                  )}
              />
              <small class="hint">
                Files with fewer lines than this always pass — a small file is
                cheap to re-read; the reminder isn't worth it.
              </small>
              <SelectField
                label="Reminder mode"
                value={snapshot.graph.read_advisor_mode}
                onchange={(next) => patch((s) => (s.graph.read_advisor_mode = next))}
              >
                <option value="advise">Advise — outline reminder only</option>
                <option value="substitute">Substitute — outline + most relevant symbol body</option>
              </SelectField>
              <NumberField
                label="Trust TTL (retrieve turns, 0 = whole session)"
                min="0"
                value={snapshot.graph.read_advisor_ttl_turns}
                onchange={(next) =>
                  patch((s) => {
                    // 0 is a valid value (TTL off), so keep it — a bare
                    // `|| 0` happens to coincide here, but stay explicit.
                    const n = Number(next);
                    s.graph.read_advisor_ttl_turns = Number.isFinite(n) ? Math.max(0, n) : 0;
                  })}
              />
              <small class="hint">
                After this many retrieval turns since the advisor last saw the
                file read in full, a <code>Read</code> passes again — bounds how
                long the agent's memory is trusted across context loss the
                advisor can't observe (context editing, tool-result truncation).
              </small>
              <Toggle
                label="Diff-substitute changed-file re-reads"
                checked={snapshot.graph.read_advisor_diffs}
                onchange={(next) => patch((s) => (s.graph.read_advisor_diffs = next))}
              />
              <small class="hint">
                When you re-read a file <em>after it changed</em>, answer with a
                line-level unified diff against what you last read instead of the
                whole file — exact, so it's safe on the edit-then-verify loop.
                Falls back to a normal read when no snapshot survives or the diff
                would be more than half the new file.
              </small>
              <Toggle
                label="Intercept whole-file shell reads"
                checked={snapshot.graph.read_advisor_shell}
                onchange={(next) => patch((s) => (s.graph.read_advisor_shell = next))}
              />
              <small class="hint">
                Also advise on a whole-file shell read
                (<code>cat</code>, <code>Get-Content</code>, <code>type</code>,
                <code>gc</code>) of an already-read file, the same as a
                <code>Read</code>. Strict — only a provable whole-file read of one
                file is intercepted; anything with a pipe, redirect, glob, second
                path, or a partial-read verb (<code>sed</code>, <code>head</code>)
                runs untouched. Installs a second hook matcher — re-launch the
                tab to pick it up.
              </small>
              <NumberField
                label="First-read digest tier (KiB, 0 = off)"
                min="0"
                value={snapshot.graph.read_advisor_first_read_kb}
                onchange={(next) =>
                  patch((s) => {
                    const n = Number(next);
                    s.graph.read_advisor_first_read_kb = Number.isFinite(n)
                      ? Math.max(0, Math.trunc(n))
                      : 0;
                  })}
              />
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
            <Toggle
              label="Local-model digests for outline-poor files"
              checked={snapshot.graph.context_llm_digests}
              disabled={!snapshot.graph.context_llm_digests && !localOffloadReady}
              onchange={(next) => patch((s) => (s.graph.context_llm_digests = next))}
            />
            <small class="hint">
              For files with no useful outline (docs, configs, long scripts), the
              <strong>local</strong> offload backend writes a 3-line semantic
              digest, cached in <code>graph.db</code>. Needs a ready local offload
              backend; never leaves this machine.
              {#if !localOffloadReady}
                <strong>No local offload backend is ready</strong> — start one in
                Settings → Offload task tools to enable this.
              {/if}
            </small>

            {:else if graphSubSection === 'viz'}
            <Toggle
              checked={snapshot.graph.graph_viz}
              onchange={(next) => patch((s) => (s.graph.graph_viz = next))}
            >
              Enable the <strong>Graph view</strong> (live 3D force graph)
            </Toggle>
            <small class="hint">
              Draws the code graph and pulses nodes as agents read/edit/query
              the codebase, in the Tools tab's "Graph view" section.
              Off by default — it's a human-facing visual, not on any agent
              path.
            </small>
            {#if snapshot.graph.graph_viz}
              <NumberField
                label="Max rendered nodes"
                min="50"
                value={snapshot.graph.graph_viz_max_nodes}
                onchange={(next) =>
                  patch(
                    (s) =>
                      (s.graph.graph_viz_max_nodes = Math.max(
                        50,
                        Number(next) || 1500,
                      )),
                  )}
              />
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
                <NumberField
                  label={knob.label}
                  min="0.2"
                  max={knob.max}
                  step="0.1"
                  value={(snapshot.graph as unknown as Record<string, number>)[knob.key]}
                  onchange={(next) =>
                    patch(
                      (s) =>
                        ((s.graph as unknown as Record<string, number>)[knob.key] = Math.min(
                          knob.max,
                          Math.max(0.2, Number(next) || 1),
                        )),
                    )}
                />
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
            {/if}
          {/if}
        </section>
      {:else if activeSection === 'checks'}
        <section>
          <h2>Checks</h2>
          <small class="hint">
            Project checker commands the <code>run_check</code> tool exposes to
            {harnessNamesProse}, and the offload worker — a build, typecheck, lint, or test run
            turned into bounded, deduplicated diagnostics instead of a raw dump.
            Configured per project; changes land in this project's
            <code>.cimp/config.json</code> overlay.
          </small>
          <ChecksEditor
            checks={snapshot.checks}
            allowRemoteWorker={snapshot.checks_allow_remote_worker}
            onchange={(next) => patch((s) => (s.checks = next))}
          />

          <!-- F-12's opt-in (`checks_allow_remote_worker`). Deliberately the same
               shape, heading and tone as Code Intelligence → Code graph →
               "Offload worker access" (`graph.allow_remote_worker_access`): per
               project, global across backends, denied by default. It sits in THIS
               section because the setting lives at the settings root beside
               `checks`, not inside `graph` — and because the commands it governs
               are the ones listed right above. Until this landed, the setting
               existed only in Rust and was reachable only by hand-editing
               `.cimp/config.json`.

               OWED TO THE RUST LANE (F-18's fifth site, second half): the
               `run_check` refusal in `offload/backend_gate.rs` sends the user to
               a Code-Intelligence sub-tab named Checks, which has never existed
               — Checks is a top-level section, a SIBLING of Code Intelligence.
               The real path is "Settings → Checks → Offload worker access", i.e.
               this heading. Unaffected by F-18's restructure and still wrong;
               not corrected here because the string is Rust-side and this pass
               may not edit `src-tauri/`. -->
          <h3>Offload worker access</h3>
          <Toggle
            checked={snapshot.checks_allow_remote_worker}
            onchange={(next) => patch((s) => (s.checks_allow_remote_worker = next))}
          >
            Allow a <strong>remote</strong> offload worker to run these checks
          </Toggle>
          <small class="hint">
            ⚠ <strong>Runs commands on this machine:</strong> the local offload
            worker can always run these checks. A <strong>remote</strong> backend —
            a box on your LAN or a public cloud API — cannot, unless you tick this.
            Ticking it lets that remote choose which of the checks above runs here,
            against your working tree, and hands it their output, which quotes your
            source. Denied by default; leave it off unless you trust the remote.
            An AI tab session's own <code>run_check</code> is
            unaffected — this governs the offload worker only. Applies from the
            worker's next call; no tab restart needed.
          </small>
        </section>
      {:else if activeSection === 'code-audit'}
        <section>
          <h2>Code Audit</h2>
          <small class="hint top">
            Aggregated security and quality scanning. cImp runs external
            scanners against the project root and merges their findings into one
            table. Nothing is bundled — each scanner resolves from the
            <code>ebin\</code> drop-in folder first, then your PATH.
            <strong>The scanners themselves are configured in
            <button type="button" class="linkish" onclick={() => (activeSection = 'tool-plugins')}
              >Tool Plugins</button
            ></strong>: they are a plugin cImp ships, so they are enabled, pointed
            at a binary and given their extra arguments in the same place as any
            tool you drop in yourself. What is here is the feature.
          </small>

          <Toggle
            label="Enable Code Audit (Tools → Code audit)"
            checked={snapshot.code_audit.enabled}
            onchange={(next) => patch((s) => (s.code_audit.enabled = next))}
          />

          <h3>Scan settings</h3>
          <NumberField
            label="Per-tool timeout (seconds)"
            min="1"
            value={snapshot.code_audit.timeout_secs}
            event="input"
            onchange={(next) =>
              patch((s) => {
                const v = Number(next);
                if (Number.isFinite(v) && v >= 1)
                  s.code_audit.timeout_secs = Math.floor(v);
              })}
          />

          <h3>Quality tool selection</h3>
          <small class="hint">
            The quality scanners are language-gated: one only runs when the
            project contains files it applies to. In <strong>automatic</strong>
            mode cImp keeps their checkboxes following the project's languages
            (the two that run a real build or need the network stay opt-in);
            editing one of their checkboxes in Tool Plugins switches to manual so
            your choice sticks. Security scanners are never touched.
          </small>
          {#if snapshot.code_audit.quality_auto_select}
            <small class="hint audit-auto-note">
              Selection: <strong>automatic</strong> — follows this project's
              languages.
            </small>
          {:else}
            <div class="audit-auto-row">
              <button type="button" class="secondary" onclick={applyQualityAutoSelect}>
                Auto-select for this project
              </button>
              <small class="hint">
                re-select the scanners matching this project's languages and keep
                them in sync automatically
              </small>
            </div>
          {/if}

          <h3>MCP exposure</h3>
          <small class="hint">
            Advertise the <code>cimp-code-audit</code> MCP server
            (<code>security_audit</code> / <code>quality_audit</code>, native
            worker tools for offload) so AI consumers can trigger audits
            themselves. Each requires Code Audit enabled above. The server set
            is injected when an AI tab starts — after enabling Code Audit or
            flipping an exposure here, restart the {harnessNames} tab
            (Tabs → Restart) for the tools to appear.
          </small>
<!-- V40 Phase B: one box per REGISTERED harness. It was a hand-written
               two-harness pair, so Code Audit would have been unreachable from
               a third harness until someone edited this file. -->
          {#each $harnesses as h (h.id)}
            <Toggle
              checked={harnessRow(snapshot, h.id).expose_code_audit}
              onchange={(next) =>
                patch((s) => {
                  const on = next;
                  s.harness = {
                    ...(s.harness ?? {}),
                    [h.id]: { ...harnessRow(s, h.id), expose_code_audit: on },
                  };
                })}
            >
              Expose to {h.label}
            </Toggle>
          {/each}
          <Toggle
            label="Expose to offload worker"
            checked={snapshot.code_audit.expose_offload}
            onchange={(next) => patch((s) => (s.code_audit.expose_offload = next))}
          />
        </section>
      {:else if activeSection === 'tool-plugins'}
        <section>
          <h2>Tool Plugins</h2>
          <small class="hint top">
            Tool definitions. A plugin is one JSON file describing tools cImp can
            run — no rebuild, and <strong>no binaries</strong>: the plugin says
            how to call a tool, you say where that tool lives. Drop your own into
            the <code>plugins\</code> folder beside cImp. The ones marked
            <strong>built in</strong> ship with cImp (the Code Audit scanners
            live here) and are the only ones that resolve a binary for you, from
            the <code>ebin\</code> folder then your PATH; for every other tool an
            unset path means it does not run. Enables, timeouts and paths are
            machine-wide (they describe this computer); the per-project path
            override and the declared variables are per project.
          </small>

          <h3>Command tools in AI tabs</h3>
          <small class="hint">
            Let AI tabs run this project's
            <strong>command</strong> tools through the
            <code>run_command</code> MCP tool — the enabled ones with a path set,
            and nothing else. It runs the registered binary directly with the
            arguments the model passes (no shell) in the project root. Hidden
            while no command tool is runnable. A harness that caches its tool
            list at connect picks a change here up only after a tab restart
            (Tabs → Restart). A change here also rewrites the managed-tool
            steering paragraph a tab is launched with (Injection protection →
            Managed-tool steering), which is spawn-baked on both harnesses — so
            open tabs are owed a restart either way.
          </small>
<!-- V40 Phase B: one box per registered harness, same reason as the
               Code Audit set above. -->
          {#each $harnesses as h (h.id)}
            <Toggle
              checked={harnessRow(snapshot, h.id).expose_commands}
              onchange={(next) =>
                patch((s) => {
                  const on = next;
                  s.harness = {
                    ...(s.harness ?? {}),
                    [h.id]: { ...harnessRow(s, h.id), expose_commands: on },
                  };
                })}
            >
              Expose to {h.label}
            </Toggle>
          {/each}

          {#snippet toolPluginRow(plugin: PluginRow, tool: ToolRow)}
            <div class="audit-tool">
              <label class="checkbox">
                <input
                  type="checkbox"
                  checked={tool.enabled}
                  onchange={(e) =>
                    {
                      const on = (e.currentTarget as HTMLInputElement).checked;
                      patchPlugin((s) => setToolEnabled(s, plugin.key, tool.id, on));
                      // A manual edit of a built-in QUALITY scanner switches
                      // auto-selection to manual mode, so the choice sticks
                      // across census refreshes instead of being re-derived at
                      // the next scan.
                      noteManualToolEdit(plugin.key, tool.id);
                    }}
                />
                <span class="audit-name">{tool.label}</span>
                <span class="audit-role">{tool.description ?? tool.kind}</span>
                <span class="audit-scope" class:local={tool.path.scope === 'project'}>
                  {tool.provider
                    ? 'MCP'
                    : tool.path.scope === 'unset'
                      ? 'no path'
                      : tool.path.scope}
                </span>
              </label>

              <!-- The phone-app pattern: what this tool ASKS for, in one place,
                   beside the switch that grants it. Read-only — the screening
                   that can refuse a grant happens at spawn time. -->
              <details class="plugin-perms" open={permissionsOpen(tool)}>
                <summary>This tool asks for…</summary>
                <ul>
                  {#each tool.permissions as line (line)}
                    <li>{line}</li>
                  {/each}
                </ul>
              </details>

              {#if !plugin.enabled}
                <small class="hint audit-na">off — the plugin is disabled</small>
              {:else if !tool.provider && tool.path.effective === '' && !tool.resolvesByName}
                <small class="hint audit-na">
                  no path set, so this tool does not run
                </small>
              {/if}

              {#if tool.provider}
                <!-- V38 Phase F, tier 2: no binary, so no path boxes. The pane
                     shows the server this tool calls instead — an empty path
                     input beside it would be an instruction nobody can follow,
                     and a "no path set, so this tool does not run" hint would be
                     simply false. Editing the server is MCP-registry work and
                     lives in the MCP servers section, so this is read-only. -->
                <small class="hint plugin-field">Answered by an MCP server</small>
                <small class="hint">
                  <code>{tool.provider.server}</code> → <code>{tool.provider.tool}</code>
                  — configure and enable it under <strong>MCP servers</strong>.
                  Nothing is installed or spawned for this tool on this machine.
                </small>
              {:else}
                <small class="hint plugin-field">Path on this machine</small>
                <div class="input-with-action">
                  <input
                    type="text"
                    placeholder={tool.resolvesByName
                      ? '(use the ebin folder / PATH)'
                      : '(not set — the tool will not run)'}
                    value={tool.path.global}
                    oninput={(e) =>
                      patchPlugin((s) =>
                        setGlobalPath(
                          s,
                          tool.toolKey,
                          (e.currentTarget as HTMLInputElement).value,
                        ),
                      )}
                  />
                  <button
                    type="button"
                    class="secondary"
                    onclick={() => void detectPluginTool(plugin, tool)}
                  >
                    Detect
                  </button>
                  <button
                    type="button"
                    class="secondary"
                    onclick={() => void pickToolBinary(tool.toolKey, 'global')}
                  >
                    Browse…
                  </button>
                  <button
                    type="button"
                    class="secondary"
                    onclick={() => patchPlugin((s) => setGlobalPath(s, tool.toolKey, ''))}
                  >
                    Clear
                  </button>
                </div>
                {#if formatDetect(auditDetect[tool.toolKey]).kind !== 'idle'}
                  {@const disp = formatDetect(auditDetect[tool.toolKey])}
                  <small
                    class="hint audit-detect"
                    class:ok={disp.kind === 'found'}
                    class:bad={disp.kind === 'not-found'}
                  >
                    {disp.text}
                  </small>
                {/if}

                {#if pluginProjectKey}
                  <small class="hint plugin-field">
                    This project
                    {tool.path.project === null ? '(inherited)' : '(overridden)'}
                  </small>
                  <div class="input-with-action">
                    <input
                      type="text"
                      placeholder="(use the machine-wide path above)"
                      value={tool.path.project ?? ''}
                      oninput={(e) =>
                        patchPlugin((s) =>
                          setProjectPath(
                            s,
                            pluginProjectKey,
                            tool.toolKey,
                            (e.currentTarget as HTMLInputElement).value,
                          ),
                        )}
                    />
                    <button
                      type="button"
                      class="secondary"
                      onclick={() => void pickToolBinary(tool.toolKey, 'project')}
                    >
                      Browse…
                    </button>
                    <button
                      type="button"
                      class="secondary"
                      disabled={tool.path.project === null}
                      onclick={() =>
                        patchPlugin((s) =>
                          revertToGlobalPath(s, pluginProjectKey, tool.toolKey),
                        )}
                    >
                      Use machine-wide
                    </button>
                  </div>
                {/if}
              {/if}

              {#each tool.variables as variable (variable.name)}
                <label class="audit-timeout">
                  <span>{variable.label}</span>
                  <input
                    class="plugin-var"
                    type="text"
                    placeholder={variable.fallback ?? '(no default — set a value)'}
                    value={variable.value}
                    oninput={(e) =>
                      patchPlugin((s) =>
                        setToolVariable(
                          s,
                          plugin.key,
                          tool.id,
                          variable.name,
                          (e.currentTarget as HTMLInputElement).value,
                        ),
                      )}
                  />
                </label>
              {/each}

              <label class="audit-timeout">
                <span>Timeout override (seconds — blank uses the plugin's)</span>
                <input
                  type="number"
                  min="1"
                  placeholder="(plugin default)"
                  value={tool.timeoutSecs ?? ''}
                  oninput={(e) =>
                    patchPlugin((s) =>
                      setToolTimeout(
                        s,
                        plugin.key,
                        tool.id,
                        optionalSeconds((e.currentTarget as HTMLInputElement).value),
                      ),
                    )}
                />
              </label>

              {#if tool.parametersAllowed}
                <small class="hint">
                  Extra arguments (appended after the tool's own):
                </small>
                {#each tool.parameters as parameter, i (i)}
                  <div class="input-with-action">
                    <input
                      type="text"
                      value={parameter}
                      oninput={(e) =>
                        patchPlugin((s) =>
                          setToolParameters(
                            s,
                            plugin.key,
                            tool.id,
                            tool.parameters.map((p, j) =>
                              j === i ? (e.currentTarget as HTMLInputElement).value : p,
                            ),
                          ),
                        )}
                    />
                    <button
                      type="button"
                      class="secondary"
                      onclick={() =>
                        patchPlugin((s) =>
                          setToolParameters(
                            s,
                            plugin.key,
                            tool.id,
                            tool.parameters.filter((_, j) => j !== i),
                          ),
                        )}
                    >
                      Remove
                    </button>
                  </div>
                {/each}
                <div class="button-row">
                  <button
                    type="button"
                    class="secondary"
                    onclick={() =>
                      patchPlugin((s) =>
                        setToolParameters(s, plugin.key, tool.id, [...tool.parameters, '']),
                      )}
                  >
                    Add argument
                  </button>
                </div>
              {/if}
            </div>
          {/snippet}

          <div class="button-row">
            <button
              type="button"
              class="secondary"
              disabled={pluginRescanning}
              onclick={() => void refreshPlugins(true)}
            >
              {pluginRescanning ? 'Rescanning…' : 'Rescan'}
            </button>
            {#if pluginSet?.dir}
              <code class="plugin-dir">{pluginSet.dir}</code>
            {/if}
          </div>
          {#if pluginLoadError}
            <small class="hint audit-detect bad">{pluginLoadError}</small>
          {/if}

          {#if pluginErrors.length > 0}
            <h3>Not loaded</h3>
            <small class="hint">
              These files are in the folder and were refused. Each one is also a
              row in the Events tab, with the same reason.
            </small>
            {#each pluginErrors as e (e.paths.join('|'))}
              <div class="plugin-error">
                <div class="plugin-error-head">
                  <span class="audit-name">{e.label}</span>
                  <span class="audit-scope local">{e.kind}</span>
                </div>
                <small class="hint audit-detect bad">{e.reason}</small>
                {#each e.paths as p (p)}
                  <code class="plugin-dir">{p}</code>
                {/each}
              </div>
            {/each}
          {/if}

          {#if pluginList.length === 0}
            <p class="hint">
              No plugins loaded yet. Put a manifest in
              <code class="plugin-dir">{pluginSet?.dir ?? 'the plugins folder beside cimp.exe'}</code>
              and press Rescan.
            </p>
          {:else}
            <div class="plugin-split">
              <ul class="plugin-list">
                {#each pluginList as p (p.key)}
                  <li>
                    <!-- One line, styled as the settings sidebar's entries are:
                         this IS a category list, and a two-line bordered card
                         per plugin made the pane read as a different app than
                         the one it lives in. What each plugin IS (built in, how
                         many tools, where its manifest is) belongs to the row
                         the user selected, not to all of them at once — so it
                         moved into the detail. The one piece of state that
                         cannot wait for a click is "off", because a list that
                         looks uniform while half of it is inert is a lie. -->
                    <!-- `icon` opts out of the TUI themes' `[ … ]` bracket framing:
                         these are list entries, not actions. -->
                    <button
                      type="button"
                      class="plugin-list-entry icon"
                      class:active={pluginActive?.key === p.key}
                      class:off={!p.enabled}
                      onclick={() => (pluginSelected = p.key)}
                    >
                      {pluginLabels.get(p.key) ?? p.label}{p.enabled ? '' : ' · off'}
                    </button>
                  </li>
                {/each}
              </ul>

              <div class="plugin-detail">
                {#if pluginActive}
                  {@const plugin = pluginActive}
                  <label class="checkbox">
                    <input
                      type="checkbox"
                      checked={plugin.enabled}
                      onchange={(e) =>
                        patchPlugin((s) =>
                          setPluginEnabled(
                            s,
                            plugin.key,
                            (e.currentTarget as HTMLInputElement).checked,
                          ),
                        )}
                    />
                    <span class="audit-name">{plugin.label}</span>
                    <!-- Decision 9's version, shown here rather than in the
                         list: it identifies the plugin the user is looking AT,
                         and two coexisting versions are told apart by this line
                         plus the manifest path below it. -->
                    <span class="plugin-version">{plugin.version}</span>
                  </label>
                  <small class="hint plugin-origin">
                    {plugin.builtin ? 'built in · ' : ''}{plugin.toolCount}
                    {plugin.toolCount === 1 ? 'tool' : 'tools'}
                  </small>
                  {#if plugin.description}
                    <small class="hint">{plugin.description}</small>
                  {/if}
                  <code class="plugin-dir">{plugin.manifestPath}</code>
                  {#if !plugin.enabled}
                    <small class="hint audit-na">
                      Every tool below is off while the plugin is. Their own
                      checkboxes keep what you set, so switching the plugin back
                      on restores this selection.
                    </small>
                  {/if}

                  {#each plugin.categories as category (category.id)}
                    <div class="plugin-category">
                      <label class="checkbox">
                        <input
                          type="checkbox"
                          checked={category.state === 'on'}
                          indeterminate={category.state === 'mixed'}
                          onchange={(e) =>
                            patchPlugin((s) =>
                              setCategoryEnabled(
                                s,
                                plugin.key,
                                category,
                                (e.currentTarget as HTMLInputElement).checked,
                              ),
                            )}
                        />
                        <span class="audit-name">{category.label}</span>
                        <span class="audit-role">
                          {category.tools.filter((t) => t.enabled).length}/{category.tools.length}
                          on
                        </span>
                      </label>

                      {#each category.tools as tool (tool.toolKey)}
                        {@render toolPluginRow(plugin, tool)}
                      {/each}
                    </div>
                  {/each}
                {/if}
              </div>
            </div>
          {/if}

        </section>
      {:else if activeSection === 'pricing'}
        <section>
          <h2>LLM pricing</h2>
          <small class="hint top">
            Provider/model token prices (USD per <strong>million tokens</strong>,
            "MTok") used by the Code Intelligence tab's session-cost popup and
            its Usage view's <em>est. cost</em> mode (auto-matched by the
            <em>Id prefix</em> column). Fresh installs are seeded with current
            vendor API and GitHub Copilot rates — cache-write priced at the
            1-hour-TTL 2× rate a real session actually pays; every
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
                <span title="Transcript model-id prefix this row auto-matches in the Usage view's cost mode. Longest match wins; empty = manual-pick only.">Id prefix</span>
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
                    placeholder="model-id prefix"
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
      {:else if activeSection === 'sandboxing'}
        <section>
          <!--
            V33 Phase A. Locked decisions 16 (one category) and 17 (the master
            switch reaches the OS layer ONLY) — see
            `docs/MILESTONE-V33-sandboxing.md` and the S1 spike report for what
            the boundary actually is.
          -->
          <h2>Sandboxing</h2>
          <small class="hint top">
            An OS-enforced boundary around the child processes an agent starts —
            the commands the offload worker runs through <code>run_command</code>,
            the configured checks <code>run_check</code> runs, the code-audit
            scanners, and (separately switched below) AI tool tabs. Injection
            protection (above) constrains a compromised model at the tool layer;
            this makes the operating system enforce a boundary the model cannot
            negotiate with. They are separate categories because neither delivers
            the other.
          </small>
          <small class="hint top">
            <strong>These settings are machine-global.</strong> They are saved to
            the global settings file and are deliberately ignored if they appear
            in a project's <code>.cimp/config.json</code> — a boundary a project
            file could switch off would be no boundary at all, since anything
            running inside the project root can write that file.
          </small>
          <Toggle
            label="Sandbox agent-started processes (master switch)"
            checked={snapshot.sandbox.enabled}
            onchange={(next) => patch((s) => (s.sandbox.enabled = next))}
          />
          <small class="hint down">
            On Windows each allowlisted command runs inside an
            <strong>AppContainer</strong>: it can read and write the project root
            and read the operating system plus the tool's own program files — and
            nothing else. Your credentials, other projects and cImp's own tokens
            are unreadable to it. Off, the command still gets the minimal
            environment, the process-tree kill and every injection-layer control:
            this switch governs the OS boundary only, never the containment
            underneath it.
          </small>
          {#if !snapshot.sandbox.enabled}
            <small class="hint down">
              Sandboxing is <strong>off by user choice</strong>. Commands run with
              your full file access. This state and “unavailable — a prerequisite
              is missing” are recorded distinctly in the Events tab, so a failed
              prerequisite can never hide behind this setting.
            </small>
          {/if}
          <!--
            V33 Phase B (locked decision B2). A scope widener INSIDE the OS
            layer, not a second master switch — hence disabled until the master
            is on, and hence its own paragraph about what confining the agent
            itself costs.
          -->
          <Toggle
            checked={snapshot.sandbox.tabs}
            disabled={!snapshot.sandbox.enabled}
            onchange={(next) => patch((s) => (s.sandbox.tabs = next))}
          >
            Also sandbox AI tabs ({harnessNames})
          </Toggle>
          <small class="hint down">
            The tab <em>is</em> the agent, so this confines everything it later
            runs. A sandboxed tab reads and writes the project and its own
            harness state ({harnessStateDirs}),
            reads <code>~/.gitconfig</code> for your commit identity, and always
            has network access — an AI CLI without egress is a bricked tab.
            Deliberately <strong>not</strong> granted: <code>~/.ssh</code> and the
            Windows Credential Manager, so a <code>git push</code> from inside a
            sandboxed tab will be refused. Add what you want reachable under
            “Extra readable tool directories” below. Plain Shell tabs are never
            sandboxed — they are your own hands, not an agent seam. Changing this
            affects tabs started afterwards; running tabs need a restart.
          </small>
          <Toggle
            label="Allow network access from sandboxed processes"
            checked={snapshot.sandbox.allow_network}
            disabled={!snapshot.sandbox.enabled}
            onchange={(next) => patch((s) => (s.sandbox.allow_network = next))}
          />
          <small class="hint down">
            Off, a sandboxed command reaches no network at all — the right default
            for build and test probes. On, it reaches the internet <em>and</em>
            your LAN: Windows capabilities cannot separate the two on this
            network, so per-host allowlisting is not yet offered rather than
            offered and untrue. This applies to the commands, checks and audit
            scanners an agent runs; sandboxed <em>tabs</em> always have network
            access regardless of this setting.
          </small>
          <label class="field">
            <span>Extra readable tool directories</span>
            <textarea
              class="sandbox-dirs"
              rows="5"
              disabled={!snapshot.sandbox.enabled}
              value={snapshot.sandbox.extra_grant_dirs.join('\n')}
              onchange={(e) =>
                patch((s) => {
                  s.sandbox.extra_grant_dirs = (e.currentTarget as HTMLTextAreaElement).value
                    .split('\n')
                    .map((l) => l.trim())
                    .filter((l) => l.length > 0);
                })}
            ></textarea>
          </label>
          <small class="hint down">
            One path per line. The command's own program directory is granted
            automatically, which covers most tools; add a directory here when a
            toolchain reaches sideways into another (a compiler calling a linker
            from a different tree). Tools installed under Program Files need no
            entry. If a directory cannot be granted — typically one owned by
            Administrators — the command runs unsandboxed and says so in Events
            rather than failing.
          </small>
          <small class="hint down">
            Tools that need a runtime — Python, Node, a JRE, the .NET SDK, Go,
            cargo — get that runtime's own directories granted automatically,
            and their caches are redirected into
            <code>.cimp/sandbox-cache/</code> inside the project, because the
            project is the only place a sandboxed tool may write. When a runtime
            is needed but cannot be located, the tool still runs and Events says
            which piece was missing rather than leaving you a silent failure.
          </small>
          <small class="hint down">
            Some paths are refused here on purpose: credential directories
            (<code>.ssh</code>, <code>.aws</code>, <code>.gnupg</code>, the
            Windows credential stores), your user-profile root, a drive root and
            the Windows directory. A refused line is reported in Events and the
            remaining grants still apply — so a single bad entry never widens the
            boundary and never breaks the run.
          </small>
        </section>
      {:else if activeSection === 'harness'}
        <section>
          <!--
            V35 Phase G — the matrix draft's third consumer (§ 3.3): the screen
            that answers "what is actually broken right now" without reading
            source. Everything below is RENDERED, not decided: the grouping,
            the tier order, the coverage marks, the gate verdicts and every
            outcome come from `harness::health::health()`.

            Restyled 2026-08-23 (user decision): STATUS FIRST. The registry's
            columns — tier, degradation, coverage, TCB, wired-in — are the
            maintainer's bookkeeping ("which rows need a canary"), not a
            status, and a page full of "Breaks silently" marks read as a to-do
            list the user could do nothing about. The user's view is now one
            verdict line per harness, plus the rows that are ACTUALLY failing
            or gated off, each written as consequence + what they can do. The
            full matrix is still here, behind a maintainer disclosure — the
            same data, nothing removed from the wire.
          -->
          <h2>Harness health</h2>
          <small class="hint top">
            cImp rides user-installed CLIs it does not pin, and they self-update.
            When a CLI update changes something cImp depends on, a feature can
            stop working with no error — this page says when that has happened,
            what stops working, and what you can do about it.
          </small>
          {#if !harnessFresh}
            <small class="hint">Reading the capability registry…</small>
          {:else}
            {#if harnessRunError}
              <small class="error">{harnessRunError}</small>
            {/if}
            {#if harnessBusy && harnessStarting === null}
              <small class="hint">
                A verification run is already in progress — most likely the automatic
                check that follows a CLI version change. This page updates when it
                finishes.
              </small>
            {/if}
            {#each harnessFresh.harness_health as panel (panel.harness)}
              {@const broken = brokenNow(panel)}
              {@const stale = panel.stale_plugins?.length ?? 0}
              {@const behind =
                !!panel.last_seen &&
                !!panel.last_verified &&
                panel.last_seen !== panel.last_verified}
              <div
                class="harness-panel"
                class:harness-panel-bad={broken.length > 0}
                class:harness-panel-warn={broken.length === 0 && (stale > 0 || behind)}
              >
                <div class="harness-head">
                  <span class="harness-title">{panel.label}</span>
                  <!--
                    Every button is disabled while ANY run is in flight — the
                    single flight is process-wide (one set of probe children at
                    a time), so a second harness's click would be dropped
                    rather than queued.
                  -->
                  <button
                    onclick={() => void runHarnessChecks(panel.harness)}
                    disabled={harnessBusy}
                  >
                    {harnessStarting === panel.harness
                      ? 'Running checks…'
                      : 'Run checks now'}
                  </button>
                </div>
                <!--
                  The verdict line. One sentence, computed from the same facts
                  the old facts-list showed; the facts themselves moved into
                  the maintainer disclosure.
                -->
                <p class="harness-verdict">
                  {#if broken.length > 0}
                    <span class="badge bad">{broken.length} broken</span>
                  {:else if panel.last_verified == null}
                    <span class="badge quiet">nothing to verify</span>
                  {:else if !panel.last_seen}
                    <span class="badge quiet">not seen yet</span>
                  {:else if behind}
                    <span class="badge warn">not yet verified</span>
                  {:else}
                    <span class="badge good">all checks passed</span>
                  {/if}
                  <span class="fact-detail">
                    {#if panel.last_seen}
                      <code>{panel.last_seen}</code> installed
                    {/if}
                    {#if panel.auto_verify}
                      · last automatic check {ageOf(panel.auto_verify.at_ms)}
                    {/if}
                    {#if panel.last_run}
                      · last run {ageOf(panel.last_run.at_ms)}
                      ({panel.last_run.pass} pass, {panel.last_run.fail} fail{#if panel.last_run.unknown > 0},
                        {panel.last_run.unknown} could not be checked{/if})
                    {/if}
                  </span>
                </p>
                {#if behind && broken.length === 0}
                  <small class="hint">
                    <code>{panel.last_seen}</code> is installed but cImp last verified
                    its contracts against <code>{panel.last_verified || 'nothing'}</code>.
                    An automatic check runs after an update; <em>Run checks now</em>
                    runs it immediately.
                  </small>
                {/if}
                {#if stale > 0}
                  <!--
                    V35 Phase I. The plugin/overlay a tab runs is baked at
                    LAUNCH, so upgrading cImp with a tab open leaves an old
                    artifact talking to new loopback code. This one the user
                    CAN fix — open a fresh tab — so it stays in the user view.
                  -->
                  <div class="harness-issue warn">
                    <div class="issue-title">
                      {stale === 1 ? 'One tab is' : `${stale} tabs are`} running an
                      out-of-date cImp plugin
                    </div>
                    <small class="hint">
                      {#each panel.stale_plugins as sp (sp.tab)}
                        <div><code>{sp.tab}</code> — {sp.note}</div>
                      {/each}
                    </small>
                    <small class="hint issue-action">
                      <strong>What to do:</strong> close and reopen
                      {stale === 1 ? 'that tab' : 'those tabs'}; a fresh tab
                      gets the current plugin.
                    </small>
                  </div>
                {/if}
                {#each broken as cap (cap.id)}
                  <!--
                    A failing or gated-off row, in user terms: what it is, what
                    that costs, what can be done. The contract sentence is the
                    registry's own, the effect is the degradation sentence, and
                    the remedy is the one the user actually has: reinstall the
                    verified CLI version, wait for a cImp update, or report it.
                  -->
                  <div class="harness-issue bad">
                    <div class="issue-title">
                      <code class="cap-id">{cap.id}</code>
                      {#if cap.gate?.blocked}
                        <span class="badge bad">gated off</span>
                      {:else}
                        <span class="badge bad">failed</span>
                      {/if}
                    </div>
                    <p class="cap-contract issue-effect">{cap.user_effect}</p>
                    {#if cap.gate?.blocked}
                      <small class="error">{cap.gate.reason}</small>
                    {/if}
                    {#if cap.last_verify?.outcome === 'fail'}
                      <small class="hint">
                        {cap.last_verify.detail}
                        — {ageOf(cap.last_verify.at_ms)}, against
                        <code>{cap.last_verify.version || 'no recorded version'}</code>
                      </small>
                    {/if}
                    <small class="hint">
                      <strong>Detail:</strong> {cap.contract}
                      {cap.degradation.label}{#if cap.degradation.user_message}
                        — “{cap.degradation.user_message}”{/if}{#if cap.degradation.fallback_to}
                        — <code>{cap.degradation.fallback_to}</code> takes over{/if}.
                      {#if cap.wired_in.length > 0}
                        Affects
                        {#each cap.wired_in as path, i (path)}<code>{path}</code>{i < cap.wired_in.length - 1 ? ', ' : ''}{/each}.
                      {/if}
                    </small>
                    <small class="hint issue-action">
                      <strong>What to do:</strong>
                      {#if cap.gate?.blocked}
                        the feature is switched off until this is resolved.
                      {/if}
                      If this started after the CLI updated, installing the last
                      version cImp verified{#if panel.last_verified}
                        (<code>{panel.last_verified}</code>){/if}
                      brings it back; otherwise wait for a cImp update, or report it
                      together with the output of <em>Run checks now</em>.
                    </small>
                  </div>
                {/each}
                <!--
                  The maintainer view: the whole registry, as before. Same
                  data, same marks; only the default visibility changed.
                -->
                <details class="cap-more harness-matrix">
                  <summary>
                    All {panel.capabilities.length} dependencies (maintainer view)
                  </summary>
                  <small class="hint">
                    Each row is one thing cImp depends on from this CLI, ranked by
                    the <strong>seam</strong> it sits in — Tier D (scraped UI,
                    undocumented behavior) is most fragile and listed first; Tier A
                    (MCP) has never broken cImp. "Breaks silently" is the
                    classification of a row, not its status: it says a break would
                    produce no error, which is why cImp checks it.
                  </small>
                  <ul class="harness-facts">
                    <li>
                      <span class="fact-key">Version seen</span>
                      <code>{panel.last_seen || 'not observed yet'}</code>
                    </li>
                    {#if panel.last_verified != null}
                      <li>
                        <span class="fact-key">Contracts verified against</span>
                        <code>{panel.last_verified || 'never verified'}</code>
                        {#if behind}
                          <span class="badge warn">behind the installed build</span>
                        {/if}
                      </li>
                    {/if}
                    {#if panel.auto_verify}
                      <li>
                        <span class="fact-key">Last automatic run</span>
                        <span class="badge {recordClass(panel.auto_verify.status)}"
                          >{panel.auto_verify.status}</span
                        >
                        <span class="fact-detail">
                          against <code>{panel.auto_verify.version || 'no version'}</code>,
                          {ageOf(panel.auto_verify.at_ms)}
                        </span>
                      </li>
                    {/if}
                    {#if panel.last_run}
                      <li>
                        <span class="fact-key">Last run this session</span>
                        <span class="fact-detail">
                          {panel.last_run.pass} pass · {panel.last_run.fail} fail ·
                          {panel.last_run.unknown} unknown · {panel.last_run.transition}
                          transition, {ageOf(panel.last_run.at_ms)}
                          {#if panel.last_run.capped}
                            — the live-probe half was skipped for time
                          {/if}
                        </span>
                      </li>
                    {/if}
                    {#if stale > 0}
                      <li>
                        <span class="fact-key">Out-of-step tabs</span>
                        <span class="badge warn">{stale}</span>
                        <span class="fact-detail">
                          {#each panel.stale_plugins as sp (sp.tab)}
                            <div>
                              <code>{sp.tab}</code> — sends CHP {sp.seen_chp}, this build
                              writes CHP {sp.expected}. {sp.note}
                            </div>
                          {/each}
                        </span>
                      </li>
                    {/if}
                  </ul>
                  <ul class="cap-list">
                    {#each panel.capabilities as cap (cap.id)}
                      <li
                        class="cap"
                        class:cap-bad={cap.last_verify?.outcome === 'fail' ||
                          cap.gate?.blocked}
                      >
                        <div class="cap-head">
                          <span class="badge tier tier-{cap.tier}">Tier {cap.tier}</span>
                          <code class="cap-id">{cap.id}</code>
                          {#if cap.controls.length > 0}
                            <!--
                              Matrix decision 10: a TCB row does not merely carry
                              data for a security control, the control EXECUTES
                              inside it.
                            -->
                            <span class="badge tcb" title="Security control executes here"
                              >TCB</span
                            >
                          {/if}
                          {#if cap.last_verify}
                            <span class="badge {outcomeClass(cap.last_verify.outcome)}"
                              >{outcomeLabel(cap.last_verify.outcome)}</span
                            >
                          {:else}
                            <span class="badge quiet">never checked</span>
                          {/if}
                        </div>
                        <p class="cap-contract">{cap.contract}</p>
                        <div class="cap-marks">
                          <span class="mark {cap.degradation.kind === 'silent' ? 'bad' : ''}"
                            >{cap.degradation.label}</span
                          >
                          {#if cap.degradation.user_message}
                            <span class="mark quiet">“{cap.degradation.user_message}”</span>
                          {/if}
                          {#if cap.degradation.fallback_to}
                            <span class="mark quiet"
                              >Falls back to <code>{cap.degradation.fallback_to}</code></span
                            >
                          {/if}
                        </div>
                        <div class="cap-marks">
                          {#if cap.coverage.canary}
                            <span class="badge good">canary L1</span>
                          {/if}
                          {#if cap.coverage.probe}
                            <span class="badge good">live probe L2</span>
                          {/if}
                          {#if cap.coverage.unproven}
                            <span class="badge warn">waiver only — nothing checks this</span>
                          {:else if cap.coverage.waiver}
                            <span class="badge quiet">waiver</span>
                          {/if}
                          {#if !cap.coverage.canary && !cap.coverage.probe && !cap.coverage.waiver}
                            <span class="badge quiet">no automatic check</span>
                          {/if}
                          {#each cap.controls as control (control)}
                            <span class="badge tcb">{control}</span>
                          {/each}
                        </div>
                        {#if cap.gate?.blocked}
                          <small class="error">Gated off: {cap.gate.reason}</small>
                        {/if}
                        {#if cap.last_verify}
                          <small class="hint">
                            {cap.last_verify.detail}
                            <br />
                            {ageOf(cap.last_verify.at_ms)}, against
                            <code>{cap.last_verify.version || 'no recorded version'}</code>
                            {#if cap.last_verify.evidence}
                              · <code>{cap.last_verify.evidence}</code>
                            {/if}
                          </small>
                        {/if}
                        {#if cap.coverage.waiver}
                          <details class="cap-more">
                            <summary>Why nothing automatic covers it</summary>
                            <small class="hint">{cap.coverage.waiver}</small>
                          </details>
                        {/if}
                        <details class="cap-more">
                          <summary>What breaks if this drifts</summary>
                          <small class="hint">
                            {#each cap.wired_in as path (path)}
                              <code>{path}</code>{' '}
                            {/each}
                          </small>
                        </details>
                      </li>
                    {/each}
                  </ul>
                </details>
              </div>
            {/each}
            <small class="hint down">
              <em>Run checks now</em> drives this harness's embedded fixture canaries
              (L1) and then the installed CLI itself (L2); it takes up to 90 seconds
              and only one run happens at a time across the whole app. For a
              harness with a recorded auto-verify path it records the same result
              an automatic post-update run would, and advances the verified
              version when nothing failed. Recording a
              <em>manual</em> contract spike (the D0 / E1 behaviours no payload can
              reveal) is still the Advisor card's <em>Mark verified</em>, not this
              button.
            </small>
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
          <Toggle
            label="Show the Workbench tab"
            checked={snapshot.workbench.enabled}
            onchange={(next) => patch((s) => (s.workbench.enabled = next))}
          />

          <h3>Checkpoints</h3>
          <Toggle
            label="Enable automatic checkpoints"
            checked={snapshot.workbench.checkpoints}
            onchange={(next) => patch((s) => (s.workbench.checkpoints = next))}
          />
          <small class="hint">
            Off by default in V1 — Diff and Worktrees work without it. When
            on, cImp periodically snapshots your working tree into a separate
            shadow git repo (your own <code>.git</code> is never touched).
            Enable this to start capturing checkpoints; restore one from the
            Workbench tab's Timeline section. The per-prompt checkpoint trigger
            rides the harness prompt hook installed at tab launch (needs the code
            graph) — if context injection is off, restart the tab after enabling
            this.
          </small>
          <NumberField
            label="Max checkpoints kept"
            min="1"
            value={snapshot.workbench.checkpoint_max}
            disabled={!snapshot.workbench.checkpoints}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.workbench.checkpoint_max = Math.max(
                    1,
                    Number(next) || 100,
                  )),
              )}
          />
          <NumberField
            label="Max checkpoint age (days)"
            min="1"
            value={snapshot.workbench.checkpoint_max_age_days}
            disabled={!snapshot.workbench.checkpoints}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.workbench.checkpoint_max_age_days = Math.max(
                    1,
                    Number(next) || 7,
                  )),
              )}
          />
          <small class="hint">
            The burst trigger fires an "activity" checkpoint when a shell tab
            or other non-hooked flow touches several files at once — the
            fallback that covers what the per-prompt trigger can't see.
          </small>
          <NumberField
            label="Burst trigger: files changed"
            min="1"
            value={snapshot.workbench.checkpoint_burst_files}
            disabled={!snapshot.workbench.checkpoints}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.workbench.checkpoint_burst_files = Math.max(
                    1,
                    Number(next) || 5,
                  )),
              )}
          />
          <NumberField
            label="Burst trigger: time window (seconds)"
            min="1"
            value={snapshot.workbench.checkpoint_burst_window_s}
            disabled={!snapshot.workbench.checkpoints}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.workbench.checkpoint_burst_window_s = Math.max(
                    1,
                    Number(next) || 60,
                  )),
              )}
          />
          <small class="hint">
            The minimum gap is enforced per AI tab, not per project: with two
            tabs open on one project, each tab's prompt can still take its own
            checkpoint inside the other's cooldown — so the Timeline can show
            which checkpoint was live for a given tab. Two tabs editing one
            working tree do interleave their checkpoints, so restoring one
            tab's checkpoint can roll back the other's work.
          </small>
          <NumberField
            label="Minimum gap between snapshots, per tab (seconds)"
            min="1"
            value={snapshot.workbench.checkpoint_min_gap_s}
            disabled={!snapshot.workbench.checkpoints}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.workbench.checkpoint_min_gap_s = Math.max(
                    1,
                    Number(next) || 120,
                  )),
              )}
          />
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
          <SelectField
            label="Log level"
            value={snapshot.logging.level}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.logging.level = next as Settings['logging']['level']),
              )}
          >
            <option value="trace">Trace</option>
            <option value="debug">Debug</option>
            <option value="info">Info</option>
            <option value="warn">Warn</option>
            <option value="error">Error</option>
          </SelectField>
          <SelectField
            label="Retention"
            value={snapshot.logging.retention}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.logging.retention = next as Settings['logging']['retention']),
              )}
          >
            <option value="daily">Daily (keep 1 day)</option>
            <option value="weekly">Weekly (keep 7 days)</option>
            <option value="monthly">Monthly (keep 30 days)</option>
            <option value="never">Never (keep everything)</option>
            {#snippet after()}
              <small class="hint">
                Cleanup runs at launch and whenever this setting changes.
                Files older than the window are deleted; the active day's log
                is always kept.
              </small>
            {/snippet}
          </SelectField>

          <h3>Content capture</h3>
          <small class="hint top">
            When on, raw PTY output for every AI / shell tab is also
            written to <code>logs/content/&lt;tab-id&gt;.log.&lt;date&gt;</code>,
            rotated daily. Output includes ANSI escape codes — pipe through
            <code>sed</code> or a viewer if you want plain text.
          </small>
          <Toggle
            label="Capture full tab output"
            checked={snapshot.logging.content_capture.enabled}
            onchange={(next) => patch((s) => (s.logging.content_capture.enabled = next))}
          />
          <SelectField
            label="Retention"
            value={snapshot.logging.content_capture.retention}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.logging.content_capture.retention = next as Settings['logging']['content_capture']['retention']),
              )}
          >
            <option value="daily">Daily (keep 1 day)</option>
            <option value="weekly">Weekly (keep 7 days)</option>
            <option value="monthly">Monthly (keep 30 days)</option>
            <option value="never">Never (keep everything)</option>
          </SelectField>
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
          <NumberField
            label="Ring buffer size (bytes per tab)"
            min="4096"
            value={snapshot.terminal.scrollback.ring_bytes}
            onchange={(next) =>
              patch(
                (s) =>
                  (s.terminal.scrollback.ring_bytes = Math.max(
                    4096,
                    Number(next) || 262144,
                  )),
              )}
          />
          <Toggle
            label="Save scrollback to disk on exit"
            checked={snapshot.terminal.scrollback.persist}
            onchange={(next) => patch((s) => (s.terminal.scrollback.persist = next))}
          />
          <small class="hint">
            On graceful exit each tab's ring is written to
            <code>scrollback/&lt;tab-id&gt;.bin</code> in the config
            directory. Terminal output can contain sensitive text — leave
            off if that shouldn't touch disk.
          </small>
          <Toggle
            label="Restore saved scrollback on launch"
            checked={snapshot.terminal.scrollback.restore_on_launch}
            onchange={(next) => patch((s) => (s.terminal.scrollback.restore_on_launch = next))}
          />
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
  /* V32 C3 — one updatable detection component: a header line, its mode
     select, and the three buttons. Boxed like the MCP editor's server rows
     so the two components
     read as two units rather than one long column of controls. */
  .updater-row {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    padding: var(--space-2) 0;
    border-top: 1px solid var(--border-faint);
  }
  .updater-row .mcp-detail {
    font-size: var(--font-size-xs);
  }
  /* V32 Phase G — one per-scope override cell inside a feature row. Laid out
     inline so a feature's scopes read as a short matrix row rather than as a
     column of full-width selects, which at ten features would bury the
     per-feature switches they hang off. */
  .inline-override {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    font-size: var(--font-size-xs);
    color: var(--text-secondary);
  }
  .inline-override select {
    font-size: var(--font-size-xs);
    padding: 1px 4px;
  }
  /* A rejected update: the old data is still live, so this is a warning the
     user should act on, not an error state for the whole section. */
  small.hint.down {
    color: var(--danger, #c9564b);
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
    color: var(--text-quiet, #999);
    margin-top: 0.8rem;
  }
  .pricing-head-row .num,
  .pricing-row input.num {
    text-align: right;
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
    color: var(--text-quiet, #999);
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
  /* V39 Phase C: a facade is read-only here, and it should look it. */
  .backend-card.facade {
    border-style: dashed;
  }
  .backend-name-static {
    font-weight: 600;
  }
  .facade-kind {
    font-size: 0.8em;
    opacity: 0.75;
    border: 1px solid var(--border-subtle);
    border-radius: 4px;
    padding: 0 0.35rem;
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
  .badge {
    font-size: var(--font-size-sm);
    padding: 0.05rem 0.4rem;
    border-radius: 999px;
    border: 1px solid var(--border-subtle);
  }
  .badge.warn {
    color: var(--text-warning, #d08770);
    border-color: var(--border-warning, #d08770);
  }
  /* V35 Phase G — Harness health. Deliberately built out of the idiom already
     here (.badge, the card border/radius of .policy-card, .backend-card's
     sunken surface) rather than a new visual language: this panel is a status
     board inside Settings, not a dashboard of its own. */
  .badge.good {
    color: var(--accent, #6abf69);
    border-color: var(--accent, #6abf69);
  }
  .badge.bad {
    color: var(--text-danger-soft, #d06b6b);
    border-color: var(--text-danger-soft, #d06b6b);
  }
  .badge.quiet {
    color: var(--text-quiet, #999);
  }
  /* The TCB mark. Filled rather than outlined so a security control reads
     differently from a data pipe at a glance (matrix decision 10). */
  .badge.tcb {
    color: var(--text-warning, #d08770);
    border-color: var(--border-warning, #d08770);
    font-weight: 600;
    letter-spacing: 0.03em;
  }
  .badge.tier {
    font-variant-numeric: tabular-nums;
  }
  /* Tier D is the riskiest seam and leads each list; the colour repeats the
     ordering so a scroll past the top still reads. */
  .badge.tier-D {
    color: var(--text-danger-soft, #d06b6b);
    border-color: var(--text-danger-soft, #d06b6b);
  }
  .badge.tier-C {
    color: var(--text-warning, #d08770);
    border-color: var(--border-warning, #d08770);
  }
  /* Status-first restyle (2026-08-23): the panel border repeats the verdict so
     a scroll past the header still reads; the issue cards are the only thing
     a user sees besides the verdict line. */
  .harness-panel-bad {
    border-left: 3px solid var(--text-danger-soft, #d06b6b);
  }
  .harness-panel-warn {
    border-left: 3px solid var(--text-warning, #d08770);
  }
  .harness-verdict {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    flex-wrap: wrap;
    margin: 0.5rem 0 0.25rem;
    font-size: var(--font-size-sm);
  }
  .harness-issue {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.5rem 0.6rem;
    margin: 0.5rem 0;
    background: var(--surface-sunken);
  }
  .harness-issue.bad {
    border-left: 3px solid var(--text-danger-soft, #d06b6b);
  }
  .harness-issue.warn {
    border-left: 3px solid var(--text-warning, #d08770);
  }
  .issue-title {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    flex-wrap: wrap;
    font-weight: 600;
  }
  .issue-effect {
    font-size: var(--font-size-md);
  }
  .issue-action {
    display: block;
    margin-top: 0.3rem;
  }
  .harness-matrix {
    margin-top: 0.5rem;
  }
  .harness-matrix > summary {
    font-weight: 600;
  }
  .harness-panel {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.75rem;
    margin: 0.75rem 0;
  }
  .harness-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    flex-wrap: wrap;
  }
  .harness-title {
    font-weight: 600;
  }
  .harness-facts {
    list-style: none;
    margin: 0.5rem 0 0.75rem;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: var(--font-size-sm);
  }
  .harness-facts li {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    flex-wrap: wrap;
  }
  .fact-key {
    min-width: 12rem;
    color: var(--text-quiet, #999);
  }
  .fact-detail {
    color: var(--text-quiet, #999);
  }
  .cap-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .cap {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.5rem 0.6rem;
    background: var(--surface-sunken);
  }
  /* A row that FAILED or is gated off gets a left rule — the panel's whole
     question is "what is broken right now", so the answer must be findable
     without reading every row. */
  .cap-bad {
    border-left: 3px solid var(--text-danger-soft, #d06b6b);
  }
  .cap-head {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    flex-wrap: wrap;
  }
  .cap-id {
    font-weight: 600;
  }
  .cap-contract {
    margin: 0.35rem 0 0.25rem;
    font-size: var(--font-size-sm);
  }
  .cap-marks {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    flex-wrap: wrap;
    font-size: var(--font-size-sm);
    margin-bottom: 0.2rem;
  }
  .cap-marks .mark {
    color: var(--text-quiet, #999);
  }
  .cap-marks .mark.bad {
    color: var(--text-danger-soft, #d06b6b);
  }
  .cap-more {
    font-size: var(--font-size-sm);
    margin-top: 0.2rem;
  }
  .cap-more summary {
    cursor: pointer;
    color: var(--text-quiet, #999);
  }
  /* Multiline, word-wrapping Server command field so every argument of a long
     llama-server invocation stays visible without horizontal scrolling. */
  /* Extra readable tool directories: one path per line, so it wants the full
     column width and enough rows to show a handful without scrolling. */
  textarea.sandbox-dirs {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: var(--font-mono, monospace);
    font-size: var(--font-size-sm);
    line-height: 1.4;
  }
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
  /* The first h3 in a section sits right under the h2 — skip the divider
     so we don't double-up with the section's top edge. */
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
  /* V23 Phase A: Code Audit per-tool row. */
  .audit-tool {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) 0;
    border-top: 1px solid var(--border-default, rgba(128, 128, 128, 0.25));
  }
  .audit-tool .audit-name {
    font-weight: 600;
  }
  .audit-tool .audit-role {
    margin-left: var(--space-2);
    opacity: 0.7;
    font-size: 0.85em;
  }
  /* Per-tool scope badge: does this tool's config match the global file
     ("global") or carry a project override ("local")? */
  .audit-tool .audit-scope {
    margin-left: auto;
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
    border: 1px solid var(--border-subtle);
    border-radius: 3px;
    padding: 0 0.3rem;
    white-space: nowrap;
  }
  .audit-tool .audit-scope.local {
    color: var(--accent, #d77757);
    border-color: currentcolor;
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
  /* V38: Tool Plugins master-detail. */
  .plugin-split {
    display: flex;
    gap: var(--space-4);
    align-items: stretch;
    margin-top: var(--space-3);
  }
  /* The plugin list is the settings sidebar's idiom applied inside a section:
     a column of single-line entries, separated from what they select by the
     same hairline the window's own .sidebar uses against .content. */
  .plugin-list {
    flex: 0 0 13rem;
    list-style: none;
    margin: 0;
    padding: 0 var(--space-3) 0 0;
    border-right: 1px solid var(--border-faint);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .plugin-list-entry {
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
    /* A long plugin name shortens rather than reflowing: the entries are one
       line each, and a wrapped one would break the rhythm of the column. */
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard);
  }
  /* A disabled plugin keeps its place in the list — it is still the thing you
     click to switch it back on — but reads as inactive, and says so. */
  .plugin-list-entry.off:not(.active) {
    color: var(--text-tertiary);
  }
  /* One step raised from the surface behind them, exactly as .sidebar's entries
     are against .sidebar. That surface is --surface-1 here (a settings section)
     rather than --surface-deep, so the same RELATIONSHIP is one token up. */
  .plugin-list-entry:hover:not(.active) {
    background: var(--surface-2);
    color: var(--text-primary);
  }
  .plugin-list-entry.active {
    background: var(--surface-2);
    color: var(--accent-purple);
    font-weight: 600;
    border-color: var(--border-subtle);
  }
  .plugin-list-entry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .plugin-detail {
    flex: 1;
    min-width: 0;
  }
  .plugin-version {
    margin-left: var(--space-2);
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
  }
  /* "built in · N tools" — the provenance/size line the list used to carry
     under every entry. It belongs to the plugin's identity, so it sits tight
     under the enable checkbox rather than a paragraph away from it. (Top margin
     comes from the `label.checkbox + small.hint` rule further down.) */
  small.hint.plugin-origin {
    margin-bottom: var(--space-2);
  }
  .plugin-category {
    margin-top: var(--space-3);
  }
  .plugin-error {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) 0;
    border-top: 1px solid var(--border-default, rgba(128, 128, 128, 0.25));
  }
  .plugin-error-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  code.plugin-dir {
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
    word-break: break-all;
  }
  .plugin-perms {
    font-size: 0.85em;
    opacity: 0.85;
  }
  .plugin-perms summary {
    cursor: pointer;
  }
  .plugin-perms ul {
    margin: var(--space-2) 0 0;
    padding-left: 1.2rem;
  }
  .audit-timeout input.plugin-var {
    width: 14rem;
  }
  small.hint.plugin-field {
    margin: var(--space-2) 0 0;
    font-size: 0.85em;
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
  /* V40 review F-3: the roster-load failure banner. Same weight as a field
     error — it explains why a block is missing, and carries the retry. */
  .roster-error {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
    color: var(--text-danger-soft);
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
  .accent-row {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }
  .accent-controls {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  /* Preset swatches are icon-class buttons (no TUI bracket framing) painted
     in their accent color; the active one gets a bordered frame. */
  .accent-swatch {
    width: 22px;
    height: 22px;
    padding: 0;
    border: 1px solid var(--border-default);
    cursor: pointer;
  }
  .accent-swatch.selected {
    outline: 1px solid var(--text-bright);
    outline-offset: 1px;
  }
  .accent-controls input[type='color'] {
    width: 44px;
    height: 22px;
    padding: 0;
    border: 1px solid var(--border-default);
    background: transparent;
    cursor: pointer;
  }
  .ai-tabs-radio {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    padding: var(--space-3) var(--space-4);
    margin: 0 0 var(--space-4) 0;
    background: var(--surface-1);
  }
  .ai-tabs-radio legend {
    padding: 0 var(--space-2);
    font-size: var(--font-size-sm);
    font-weight: 500;
    color: var(--text-primary);
  }
  .ai-tabs-radio .hint {
    display: block;
    margin: 0 0 var(--space-3) 0;
    color: var(--text-quiet);
  }
  .ai-tabs-radio .radio-row {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
  }
  .ai-tabs-radio .radio-row label {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: var(--font-size-sm);
    cursor: pointer;
  }
  .tabs-grid {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: var(--space-2);
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
