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
  import { contentClear, contentOpenFolder, setBrootEnabled, setEnabledAiTabs } from './lib/ipc';
  import { listSttModels, listInputDevices } from './lib/stt';
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
  // V14: same toggle for the Aider local LLM section.
  let showAiderLocalToken = $state<boolean>(false);
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
  // on this so only one group renders at a time. Default lands on 'audio'
  // (TTS sat at the top of the original single-scroll layout).
  type SectionId =
    | 'audio'
    | 'stt'
    | 'avatar'
    | 'theme'
    | 'background'
    | 'display'
    | 'bottom-bar'
    | 'tabs'
    | 'shortcuts'
    | 'local-llm'
    | 'aider-local-llm'
    | 'advanced'
    | 'about';
  let activeSection = $state<SectionId>('audio');
  const SECTIONS: { id: SectionId; label: string }[] = [
    { id: 'audio', label: 'Audio' },
    { id: 'stt', label: 'Speech-to-text' },
    { id: 'avatar', label: 'Avatar' },
    { id: 'theme', label: 'Theme' },
    { id: 'background', label: 'Background' },
    { id: 'display', label: 'Display' },
    { id: 'bottom-bar', label: 'Bottom bar' },
    { id: 'tabs', label: 'Tabs' },
    { id: 'shortcuts', label: 'Shortcuts' },
    { id: 'local-llm', label: 'Local LLM (Claude)' },
    { id: 'aider-local-llm', label: 'Local LLM (Aider)' },
    { id: 'advanced', label: 'Advanced' },
    { id: 'about', label: 'About' },
  ];
  const REPO_URL = 'https://github.com/Dyserna/ccImp';
  // V15: reserved id of the broot utility tab. Mirrors
  // `SHELL_BROOT_TAB_ID` in the Rust schema. Its presence in
  // `snapshot.tabs` is its enabled state (it has no separate setting).
  const BROOT_TAB_ID = 'shell-broot';

  // Sub-tab nav within the Tabs section. Each AI builtin gets its own
  // sub-tab; every Shell tab is grouped under 'shells'. Keeps the
  // previously-collapsible <details> wall navigable.
  type TabsSubSection = AiTabId | 'shells';
  let tabsSubSection = $state<TabsSubSection>('claude');
  function subSectionForTabId(tabId: string): TabsSubSection {
    if (
      tabId === 'claude' ||
      tabId === 'claude-local' ||
      tabId === 'aider' ||
      tabId === 'aider-local'
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
      const order: AiTabId[] = ['claude', 'claude-local', 'aider', 'aider-local'];
      next_ids = order.filter((x) => prev.includes(x) || x === id);
    } else {
      if (prev.length <= 1) return; // last-one lock (also guarded by the disabled attribute)
      next_ids = prev.filter((x) => x !== id);
    }
    if (!enable && tabsSubSection === id) {
      // Jump to the first surviving id in canonical order.
      const order: AiTabId[] = ['claude', 'claude-local', 'aider', 'aider-local'];
      const survivor = order.find((x) => next_ids.includes(x));
      if (survivor) tabsSubSection = survivor;
    }
    const updated = structuredClone($state.snapshot(snapshot));
    updated.enabled_ai_tabs = next_ids;
    snapshot = updated;
    try {
      await setEnabledAiTabs(next_ids);
    } catch (e) {
      console.error('set_enabled_ai_tabs failed:', e);
      const restored = structuredClone($state.snapshot(snapshot));
      restored.enabled_ai_tabs = prev;
      snapshot = restored;
    }
  }

  /// Enable / disable the reserved broot tab. Like `toggleAiTabEnabled`,
  /// routes through a dedicated IPC because the backend has to open or
  /// close the tab (kill PTY, drop scrollback) in response. The checkbox
  /// state derives from `snapshot.tabs` presence; the backend's settings
  /// broadcast refreshes `snapshot` after the toggle, so no optimistic
  /// mutation is needed here.
  async function toggleBrootEnabled(enable: boolean) {
    if (!snapshot) return;
    const present = snapshot.tabs.some((t) => t.id === BROOT_TAB_ID);
    if (present === enable) return;
    try {
      await setBrootEnabled(enable);
    } catch (e) {
      console.error('set_broot_enabled failed:', e);
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

  /// Active-tab metadata, used by the Advanced → "Apply to global"
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
  <TuiTitleBar title="ccImp settings" />
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
          <label>
            <span>Voice</span>
            <select
              value={snapshot.tts.voice}
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
              oninput={(e) =>
                patch((s) => (s.tts.volume = +(e.currentTarget as HTMLInputElement).value))}
            />
          </label>
          <label class="checkbox">
            <input
              type="checkbox"
              checked={snapshot.tts.mute}
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

          <label>
            <span>Push-to-talk</span>
            <ShortcutCapture
              bind:value={
                () => snapshot!.shortcuts.push_to_talk,
                (v) => patch((s) => (s.shortcuts.push_to_talk = v))
              }
            />
          </label>
          <small class="hint">
            Hold the chord to record, release to transcribe. The default is
            bare <code>Ctrl+Shift</code> — a quick tap or a
            <code>Ctrl+Shift+&lt;key&gt;</code> chord won't trigger a recording.
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
                <option value="impSprites">ccImp (pixel art)</option>
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
            Governs the ccImp chrome — tab bar, status bar, dialogs.
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
      {:else if activeSection === 'background'}
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
      {:else if activeSection === 'display'}
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
      {:else if activeSection === 'tabs'}
        {@const claudeLive = aiTabAt('claude')}
        {@const claudeLocalLive = aiTabAt('claude-local')}
        {@const aiderLive = aiTabAt('aider')}
        {@const aiderLocalLive = aiTabAt('aider-local')}
        {@const shellEntries = tabEntries.filter(
          (e) => e.kind === 'shell' && e.id !== BROOT_TAB_ID,
        )}
        {@const brootEnabled = tabEntries.some((e) => e.id === BROOT_TAB_ID)}
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
                  value="aider"
                  checked={enabledAiTabs.includes('aider')}
                  disabled={lastChecked === 'aider'}
                  onchange={(e) =>
                    void toggleAiTabEnabled(
                      'aider',
                      (e.currentTarget as HTMLInputElement).checked,
                    )}
                />
                Aider (cloud)
              </label>
              <label>
                <input
                  type="checkbox"
                  name="ai-tabs-enabled"
                  value="aider-local"
                  checked={enabledAiTabs.includes('aider-local')}
                  disabled={lastChecked === 'aider-local'}
                  onchange={(e) =>
                    void toggleAiTabEnabled(
                      'aider-local',
                      (e.currentTarget as HTMLInputElement).checked,
                    )}
                />
                Aider (local)
              </label>
            </div>
          </fieldset>
          <fieldset class="claude-tabs-radio">
            <legend>Utility tabs</legend>
            <small class="hint">
              The broot tab runs <code>broot -g</code> (the broot file
              browser with git info) in the directory ccImp was started in.
              While enabled it's a builtin tab — it can't be closed from the
              tab bar; untick here to remove it.
            </small>
            <div class="radio-row">
              <label>
                <input
                  type="checkbox"
                  name="util-tabs-enabled"
                  value="broot"
                  checked={brootEnabled}
                  onchange={(e) =>
                    void toggleBrootEnabled(
                      (e.currentTarget as HTMLInputElement).checked,
                    )}
                />
                broot (git)
              </label>
            </div>
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
              class:active={tabsSubSection === 'aider'}
              aria-selected={tabsSubSection === 'aider'}
              onclick={() => (tabsSubSection = 'aider')}
            >
              Aider
            </button>
            <button
              type="button"
              role="tab"
              class:active={tabsSubSection === 'aider-local'}
              aria-selected={tabsSubSection === 'aider-local'}
              onclick={() => (tabsSubSection = 'aider-local')}
            >
              Aider (local)
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
            </div>
          {:else if tabsSubSection === 'aider'}
            <div id="tab-section-aider">
              {#if aiderLive}
                <TabSettingsSection
                  tabId={'aider'}
                  displayName={'Aider'}
                  bind:settings={
                    () => aiderLive,
                    (v) => patchAiTab('aider', v)
                  }
                  defaults={tabDefaults['aider'] ?? null}
                  restartRequired={restartRequired['aider'] ?? false}
                  onchange={() => {}}
                  onrestart={() => restartTab('aider')}
                />
              {:else}
                <small class="hint top">Aider tab is disabled — tick the checkbox above to enable it.</small>
              {/if}
            </div>
          {:else if tabsSubSection === 'aider-local'}
            <div id="tab-section-aider-local">
              {#if aiderLocalLive}
                <TabSettingsSection
                  tabId={'aider-local'}
                  displayName={'Aider (local)'}
                  bind:settings={
                    () => aiderLocalLive,
                    (v) => patchAiTab('aider-local', v)
                  }
                  defaults={tabDefaults['aider-local'] ?? null}
                  restartRequired={restartRequired['aider-local'] ?? false}
                  onchange={() => {}}
                  onrestart={() => restartTab('aider-local')}
                />
              {:else}
                <small class="hint top">Aider (local) tab is disabled — tick the checkbox above to enable it.</small>
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
          <h2>Shortcuts</h2>
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
        </section>
      {:else if activeSection === 'local-llm'}
        <section>
          <h2>Local LLM provider — Claude</h2>
          <small class="hint top">
            Settings for the Claude AI tab when <em>Use local LLM provider</em>
            is enabled. Run a LiteLLM (or compatible) proxy that translates the
            Anthropic Messages API to your local model — ccImp does not start
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
      {:else if activeSection === 'aider-local-llm'}
        <section>
          <h2>Local LLM provider — Aider</h2>
          <small class="hint top">
            Settings for the Aider (local) tab. ccImp synthesizes
            <code>OPENAI_API_BASE</code> / <code>OPENAI_API_KEY</code>
            from the values below and (when <em>Model</em> is set) passes
            <code>--model &lt;model&gt;</code> on the spawn argv. Point
            this at any OpenAI-compatible endpoint (Ollama, LM Studio,
            vLLM, LiteLLM proxy, …); ccImp does not start the endpoint
            itself.
          </small>
          <label>
            <span>Endpoint URL</span>
            <input
              type="text"
              value={snapshot?.aider_local.base_url ?? ''}
              oninput={(e) =>
                patch(
                  (s) =>
                    (s.aider_local.base_url = (
                      e.currentTarget as HTMLInputElement
                    ).value),
                )}
              placeholder="http://localhost:11434/v1"
            />
            <small class="hint">
              Becomes <code>OPENAI_API_BASE</code> on launch. For Ollama,
              the default <code>:11434/v1</code> path serves the
              OpenAI-compatible API.
            </small>
          </label>
          <label>
            <span>Auth token</span>
            <div class="input-with-action">
              <input
                type={showAiderLocalToken ? 'text' : 'password'}
                value={snapshot?.aider_local.auth_token ?? ''}
                oninput={(e) =>
                  patch(
                    (s) =>
                      (s.aider_local.auth_token = (
                        e.currentTarget as HTMLInputElement
                      ).value),
                  )}
                placeholder="ollama"
              />
              <button
                type="button"
                class="secondary"
                onclick={() => (showAiderLocalToken = !showAiderLocalToken)}
              >
                {showAiderLocalToken ? 'Hide' : 'Show'}
              </button>
            </div>
            <small class="hint">
              Becomes <code>OPENAI_API_KEY</code>. Stored cleartext;
              local endpoints typically accept any non-empty value
              (Ollama defaults to the literal string <code>ollama</code>).
            </small>
          </label>
          <label>
            <span>Model</span>
            <input
              type="text"
              value={snapshot?.aider_local.model ?? ''}
              oninput={(e) =>
                patch(
                  (s) =>
                    (s.aider_local.model = (
                      e.currentTarget as HTMLInputElement
                    ).value),
                )}
              placeholder="qwen3:14b"
            />
            <small class="hint">
              When non-empty, passed as <code>--model &lt;model&gt;</code>
              on the aider spawn argv. Aider's own naming conventions
              apply (e.g. <code>openai/qwen3:14b</code> for some endpoints).
            </small>
          </label>
        </section>
      {:else if activeSection === 'advanced'}
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

        <section>
          <h2>Logging</h2>
          <small class="hint top">
            Log files roll daily into <code>logs/</code> next to the ccImp
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
    margin: -8px 0 var(--space-3) 0;
  }
  /* hint placed directly under an h3 (rather than tucked under a label)
     needs normal top margin — it has no preceding label to overlap. */
  small.hint.top {
    margin-top: 0;
    margin-bottom: var(--space-3);
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
