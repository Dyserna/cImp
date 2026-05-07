<script lang="ts">
  import { get } from 'svelte/store';
  import type { TabId } from '../tabs/types';
  import type {
    AiToolTabConfig,
    TerminalBackgroundSettings,
    ThemeColorsWire,
  } from './types';
  import ArrayEditor from './ArrayEditor.svelte';
  import TextAreaWithReset from './TextAreaWithReset.svelte';
  import NotificationEditor from './NotificationEditor.svelte';
  import Pill from '../Pill.svelte';
  import ThemeSwatch from './ThemeSwatch.svelte';
  import CustomThemeEditor from './CustomThemeEditor.svelte';
  import BackgroundConfigEditor from './BackgroundConfigEditor.svelte';
  import { BUNDLED_THEME_NAMES, BUNDLED_THEMES, resolveBundledTheme } from '../themes';
  import { settings as settingsStore } from './store';

  // Renders the per-AI-tab settings form: command (read-only), CLI args,
  // TTS injection toggle + instructions, notification texts, and a Restart
  // Tab button. The parent owns baseline tracking and computes
  // `restartRequired`; this component just shows the indicator and routes
  // the restart click back up.
  let {
    tabId,
    displayName,
    settings = $bindable<AiToolTabConfig>(),
    defaults,
    restartRequired,
    onchange,
    onrestart,
  }: {
    tabId: TabId;
    displayName: string;
    settings: AiToolTabConfig;
    defaults: AiToolTabConfig | null;
    restartRequired: boolean;
    onchange: () => void;
    onrestart: () => void;
  } = $props();

  function update<K extends keyof AiToolTabConfig>(key: K, value: AiToolTabConfig[K]) {
    settings = { ...settings, [key]: value };
    onchange();
  }

  function updateInjection<K extends 'enabled' | 'instructions'>(
    key: K,
    value: K extends 'enabled' ? boolean : string,
  ) {
    settings = {
      ...settings,
      tts_injection: { ...settings.tts_injection, [key]: value },
    };
    onchange();
  }

  // Live global theme name for the "Use global default (current: X)"
  // dropdown entry. Reactive via $derived so it tracks Settings changes.
  let globalThemeName = $derived($settingsStore.terminal.theme.name);

  let overrideSelection = $derived(
    settings.theme_override === null ? '__inherit' : settings.theme_override.name,
  );

  // V1.4-03: per-tab background override. Three states mirror the shell-
  // tab dialog's BgOverrideMode: '__inherit' / '__disabled' / '__custom'.
  type BgOverrideMode = '__inherit' | '__disabled' | '__custom';
  let bgOverrideSelection = $derived<BgOverrideMode>(
    settings.background_override === null
      ? '__inherit'
      : settings.background_override === 'disabled'
        ? '__disabled'
        : '__custom',
  );

  let globalBgSummary = $derived(
    globalBgSummaryOf($settingsStore.terminal.background),
  );

  function globalBgSummaryOf(bg: TerminalBackgroundSettings): string {
    if (bg.image) {
      const filename = bg.image.split(/[\\/]/).pop() ?? bg.image;
      return `image: ${filename}`;
    }
    if (bg.color) return `solid ${bg.color}`;
    return 'theme background';
  }

  function selectBgOverride(value: BgOverrideMode): void {
    if (value === '__inherit') {
      update('background_override', null);
      return;
    }
    if (value === '__disabled') {
      update('background_override', 'disabled');
      return;
    }
    // '__custom' — already custom: preserve.
    if (
      settings.background_override !== null &&
      settings.background_override !== 'disabled' &&
      typeof settings.background_override === 'object'
    ) {
      return;
    }
    // V1.4-04 B/C: strip the global presets list when descending into
    // an override (presets live globally; the embedded list inside an
    // override is harmless wire-format growth we'd rather avoid).
    const liveGlobal = get(settingsStore).terminal.background;
    update('background_override', { ...liveGlobal, presets: [] });
  }

  function updateCustomBg(next: TerminalBackgroundSettings): void {
    update('background_override', next);
  }

  function selectThemeOverride(value: string): void {
    if (value === '__inherit') {
      update('theme_override', null);
      return;
    }
    if (value === 'Custom') {
      const liveGlobal = get(settingsStore).terminal.theme;
      const previousName =
        settings.theme_override === null
          ? liveGlobal.name
          : settings.theme_override.name;
      const seed =
        previousName === 'Custom'
          ? BUNDLED_THEMES.Default
          : resolveBundledTheme(previousName);
      update('theme_override', {
        name: 'Custom',
        custom: { ...seed } as ThemeColorsWire,
      });
      return;
    }
    update('theme_override', { name: value, custom: null });
  }

  function updateCustomColors(next: ThemeColorsWire): void {
    if (!settings.theme_override) return;
    update('theme_override', { ...settings.theme_override, custom: next });
  }
</script>

<div class="tab-section">
  <h3>{displayName}</h3>

  <label>
    <span>Command</span>
    <input type="text" value={settings.command} disabled readonly />
    <small class="hint">
      The binary spawned for this tab. Not editable in v2.
    </small>
  </label>

  <label>
    <span>
      Persistent CLI args
      {#if restartRequired}
        <Pill variant="orange" size="xs">restart required</Pill>
      {/if}
    </span>
    <ArrayEditor
      bind:items={
        () => settings.args,
        (v) => update('args', v)
      }
      placeholder="--flag or --key=value"
    />
    <small class="hint">
      Appended to every spawn of this tab. One arg per row.
    </small>
  </label>

  <label class="checkbox">
    <input
      type="checkbox"
      checked={settings.tts_injection.enabled}
      onchange={(e) =>
        updateInjection(
          'enabled',
          (e.currentTarget as HTMLInputElement).checked,
        )}
    />
    <span>
      TTS markup injection enabled
      {#if restartRequired}
        <Pill variant="orange" size="xs">restart required</Pill>
      {/if}
    </span>
  </label>

  {#if tabId === 'aider' && settings.tts_injection.enabled}
    <p class="warn">
      Aider does not currently support system-prompt injection via CLI, so this
      toggle has no effect today. The setting is preserved for forward
      compatibility — see <code>docs/FUTURE-FEATURES.md</code>.
    </p>
  {/if}

  <label>
    <span>TTS markup instructions</span>
    <TextAreaWithReset
      bind:value={
        () => settings.tts_injection.instructions,
        (v) => updateInjection('instructions', v)
      }
      defaultValue={defaults?.tts_injection.instructions ?? ''}
      disabled={!settings.tts_injection.enabled}
      rows={6}
      placeholder="Instructions injected via --append-system-prompt"
    />
    <small class="hint">
      Injected on subprocess start when the toggle above is on.
    </small>
  </label>

  <label>
    <span>Notifications</span>
    <NotificationEditor
      bind:notifications={
        () => settings.notifications,
        (v) => update('notifications', v)
      }
      defaults={defaults?.notifications ?? settings.notifications}
    />
    <small class="hint">
      Text used for inactive-tab notifications. Notification firing itself
      ships in V2-04; the configuration is wired now.
    </small>
  </label>

  <label class="palette-row">
    <span>Terminal palette</span>
    <select
      value={overrideSelection}
      onchange={(e) =>
        selectThemeOverride((e.currentTarget as HTMLSelectElement).value)}
    >
      <option value="__inherit">Use global default (current: {globalThemeName})</option>
      {#each BUNDLED_THEME_NAMES as paletteName}
        <option value={paletteName}>{paletteName}</option>
      {/each}
      <option value="Custom">Custom…</option>
    </select>
    {#if settings.theme_override !== null}
      <ThemeSwatch
        name={settings.theme_override.name}
        custom={settings.theme_override.custom}
      />
    {/if}
  </label>
  {#if settings.theme_override && settings.theme_override.name === 'Custom' && settings.theme_override.custom}
    <CustomThemeEditor
      value={settings.theme_override.custom}
      onchange={updateCustomColors}
    />
  {/if}
  <small class="hint">
    Override the global terminal palette for this tab. Applied
    immediately — no restart needed.
  </small>

  <label class="palette-row">
    <span>Terminal background</span>
    <select
      value={bgOverrideSelection}
      onchange={(e) =>
        selectBgOverride(
          (e.currentTarget as HTMLSelectElement).value as BgOverrideMode,
        )}
    >
      <option value="__inherit"
        >Use global default (current: {globalBgSummary})</option
      >
      <option value="__disabled"
        >Disabled — use theme background only</option
      >
      <option value="__custom">Custom for this tab</option>
    </select>
  </label>
  {#if bgOverrideSelection === '__disabled'}
    <small class="hint">
      No observable effect when the global background is also "Theme
      default."
    </small>
  {/if}
  {#if bgOverrideSelection === '__custom' && settings.background_override !== null && settings.background_override !== 'disabled' && typeof settings.background_override === 'object'}
    <BackgroundConfigEditor
      bind:config={
        () => settings.background_override as TerminalBackgroundSettings,
        (v) => updateCustomBg(v)
      }
    />
  {/if}

  <div class="restart-row">
    <button
      type="button"
      class="restart-btn"
      disabled={!restartRequired}
      onclick={onrestart}
      title="Tear down this tab's subprocess and start a fresh one with current settings. The session in this tab is reset."
    >
      Restart {displayName}
    </button>
    <small class="hint inline">
      Clicking Restart resets this tab's session.
    </small>
  </div>
</div>

<style>
  .tab-section {
    padding: var(--space-3) 14px;
    background: var(--surface-sunken);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
  }
  h3 {
    margin: 0 0 var(--space-3) 0;
    font-size: var(--font-size-md);
    font-weight: 600;
    color: var(--text-primary);
  }
  .warn {
    margin: -6px 0 var(--space-3) 0;
    padding: var(--space-2) 10px;
    background: var(--surface-warning-faint);
    border: 1px solid var(--border-warning);
    border-radius: var(--radius-md);
    color: var(--text-warning-bright);
    font-size: var(--font-size-xs);
    line-height: 1.4;
  }
  .warn code {
    background: var(--surface-sunken);
    padding: 1px var(--space-1);
    border-radius: var(--radius-sm);
    font-size: var(--font-size-xs);
  }
  small.hint.inline {
    margin: 0 0 0 10px;
    color: var(--text-faint);
  }
  .restart-row {
    display: flex;
    align-items: center;
    margin-top: var(--space-2);
  }
  .restart-btn {
    background: var(--border-info);
    border: 1px solid var(--border-info);
    color: var(--text-bright);
    padding: 6px 14px;
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: var(--font-size-sm);
    font-weight: var(--font-weight-medium);
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .restart-btn:hover:not(:disabled) {
    background: var(--text-info-quiet);
    border-color: var(--text-info-quiet);
  }
  .restart-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .restart-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
