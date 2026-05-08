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
  // V1.4-07: `tabId` is plumbed in from the parent so future per-id
  // conditionals (e.g., a "this is the local-LLM tab — confirm proxy
  // is up" warning) have an anchor. Currently unused after the aider
  // TTS-injection warning was removed; underscore-prefix marks it so
  // svelte-check doesn't complain.
  let {
    tabId: _tabId,
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
  <div class="group">
    <h4>Subprocess</h4>
    <label class="field">
      <span>
        Command
        {#if restartRequired}
          <Pill variant="orange" size="xs">restart required</Pill>
        {/if}
      </span>
      <input
        type="text"
        value={settings.command}
        oninput={(e) =>
          update('command', (e.currentTarget as HTMLInputElement).value)}
      />
      <small class="hint">
        The binary spawned for this tab. Defaults to <code>claude</code>;
        edit if your <code>claude</code> binary lives somewhere PATH
        doesn't reach (e.g. an absolute path like
        <code>C:\tools\claude.exe</code>). Restart this tab after
        changing.
      </small>
    </label>

    <label class="field">
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
        checked={settings.use_local_provider}
        onchange={(e) =>
          update(
            'use_local_provider',
            (e.currentTarget as HTMLInputElement).checked,
          )}
      />
      <span>
        Use local LLM provider
        {#if restartRequired}
          <Pill variant="orange" size="xs">restart required</Pill>
        {/if}
      </span>
    </label>
    {#if settings.use_local_provider}
      <p class="hint hint-effective-env">
        On launch this tab synthesizes
        <code>ANTHROPIC_BASE_URL={$settingsStore.claude_local.base_url}</code>,
        <code>ANTHROPIC_AUTH_TOKEN=…</code>{$settingsStore.claude_local.model_alias
          ? `, ANTHROPIC_MODEL=${$settingsStore.claude_local.model_alias}`
          : ''}
        from the global <em>Local LLM provider</em> settings. Per-tab env
        entries below override these.
      </p>
    {/if}
  </div>

  <div class="group">
    <h4>TTS injection</h4>
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

    <label class="field">
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
  </div>

  <div class="group">
    <h4>Notifications</h4>
    <label class="field">
      <span>Per-state notification text</span>
      <NotificationEditor
        bind:notifications={
          () => settings.notifications,
          (v) => update('notifications', v)
        }
        defaults={defaults?.notifications ?? settings.notifications}
      />
      <small class="hint">
        Text used for inactive-tab notifications. Notification firing
        itself ships in V2-04; the configuration is wired now.
      </small>
    </label>
  </div>

  <div class="group">
    <h4>Appearance</h4>
    <label class="field palette-row">
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
      <small class="hint">
        Override the global terminal palette for this tab. Applied
        immediately — no restart needed.
      </small>
    </label>
    {#if settings.theme_override && settings.theme_override.name === 'Custom' && settings.theme_override.custom}
      <CustomThemeEditor
        value={settings.theme_override.custom}
        onchange={updateCustomColors}
      />
    {/if}

    <label class="field palette-row">
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
      {#if bgOverrideSelection === '__disabled'}
        <small class="hint">
          No observable effect when the global background is also "Theme
          default."
        </small>
      {/if}
    </label>
    {#if bgOverrideSelection === '__custom' && settings.background_override !== null && settings.background_override !== 'disabled' && typeof settings.background_override === 'object'}
      <BackgroundConfigEditor
        bind:config={
          () => settings.background_override as TerminalBackgroundSettings,
          (v) => updateCustomBg(v)
        }
      />
    {/if}
  </div>

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
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
    padding: var(--space-4) var(--space-4);
    background: var(--surface-sunken);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
  }

  /* Logical group: Subprocess / TTS injection / Notifications / Appearance.
     Each group separates with a top border + uppercase mini-heading so
     the previously-stacked-flat fields read as distinct sections. */
  .group {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding-top: var(--space-4);
    border-top: 1px solid var(--border-faint);
  }
  .tab-section > .group:first-of-type {
    padding-top: 0;
    border-top: none;
  }
  .group h4 {
    margin: 0;
    font-size: var(--font-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-tertiary);
  }

  /* Scoped form-control styling. Svelte's parent-scoped CSS doesn't
     reach into child components, so without these the labels and inputs
     here would inherit only browser defaults — which was the cramped
     look the previous milestone left behind. */
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin: 0;
  }
  .field > span:first-child {
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
    font-variant-numeric: tabular-nums;
    font-feature-settings: 'tnum';
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .field input[type='text'],
  .field select {
    width: 100%;
    background: var(--surface-deep);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: inherit;
    font-size: var(--font-size-md);
    box-sizing: border-box;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .field input[type='text']:focus,
  .field select:focus {
    outline: none;
    border-color: var(--accent);
  }

  .checkbox {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0;
  }
  .checkbox > span {
    color: var(--text-primary);
    font-size: var(--font-size-sm);
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }

  .palette-row {
    display: grid;
    grid-template-columns: 1fr auto;
    grid-column-gap: var(--space-2);
    align-items: center;
  }
  .palette-row > span:first-child {
    grid-column: 1 / -1;
  }
  .palette-row > .hint {
    grid-column: 1 / -1;
  }

  small.hint {
    display: block;
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    line-height: 1.4;
    margin: 2px 0 0 0;
  }
  small.hint.inline {
    margin: 0 0 0 10px;
    color: var(--text-faint);
  }

  /* Effective-env block shown when "Use local LLM provider" is on.
     Different shape than the inline hints — a tinted callout, not a
     mini caption. */
  p.hint-effective-env {
    margin: 0;
    padding: var(--space-2) var(--space-3);
    background: var(--surface-deep);
    border-left: 2px solid var(--accent-soft);
    border-radius: var(--radius-sm);
    color: var(--text-quiet);
    font-size: var(--font-size-xs);
    line-height: 1.5;
  }
  p.hint-effective-env code {
    background: var(--surface-1);
    padding: 1px var(--space-1);
    border-radius: var(--radius-sm);
    font-family: Consolas, Menlo, monospace;
    font-size: 11px;
    color: var(--text-primary);
  }

  .restart-row {
    display: flex;
    align-items: center;
    padding-top: var(--space-3);
    border-top: 1px solid var(--border-faint);
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
