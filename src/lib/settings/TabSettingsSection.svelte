<script lang="ts">
  import { get } from 'svelte/store';
  import type { TabId } from '../tabs/types';
  import type {
    AiToolTabConfig,
    TerminalBackgroundSettings,
    ThemeColorsWire,
  } from './types';
  import ArrayEditor from './ArrayEditor.svelte';
  import EnvEditor from './EnvEditor.svelte';
  import NotificationEditor from './NotificationEditor.svelte';
  import Pill from '../Pill.svelte';
  import ThemeSwatch from './ThemeSwatch.svelte';
  import CustomThemeEditor from './CustomThemeEditor.svelte';
  import BackgroundConfigEditor from './BackgroundConfigEditor.svelte';
  import { resolveBundledTheme, defaultPalette } from '../themes';
  import { paletteRegistry } from '../themes/registry';
  import { settings as settingsStore } from './store';
  import { harnessRow } from './types';
  import { findHarnessByTabId, harnesses } from '../harness';

  // Renders the per-AI-tab settings form: command (read-only), CLI args,
  // a TTS speak toggle, notification texts, and a Restart Tab button. The
  // parent owns baseline tracking and computes `restartRequired`; this
  // component just shows the indicator and routes the restart click back up.
  // V40 Phase F: what differs per harness — whether there is a local-provider
  // control at all, which env vars it synthesizes, what the Command field
  // defaults to — is the registry's declared affordances (locked decision 27),
  // not an `if (tabId === …)` here.
  // V20: the TTS toggle is a plain per-tab speak gate (the `[[TTS]]` markup
  // convention is retired — prose is spoken from each tool's out-of-band
  // transcript/event stream), so there is no longer an instructions field.
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

  /// This tab's harness, or `null` for a tab whose harness the registry does
  /// not know (and for the frame before `harness_list` answers).
  const harness = $derived(findHarnessByTabId($harnesses, tabId));
  const affordances = $derived(harness?.affordances ?? null);
  /// The env vars a custom-provider tab of this harness synthesizes, in render
  /// order. `null` means this harness has NO custom-provider variant — it
  /// manages its own providers — and the harness's own explanation renders in
  /// place of the env preview.
  const localProvider = $derived(affordances?.localProvider ?? null);

  /// Whether THIS tab is its harness's custom-provider tab (issue #109).
  ///
  /// It used to be a per-tab checkbox, which was a second, editable spelling of
  /// something the registry already decides — the backend's integrity pass
  /// forced the stored flag back to the reserved tab's declared value on every
  /// load, so ticking it on the primary tab was a control that could not stick.
  /// The reserved tab IS the choice now; the stored `use_local_provider` field
  /// stays on the wire, tab-determined.
  const isProviderTab = $derived(harness?.provider_tab_id === tabId);

  /// The preview rows for the local-provider env, filled from the harness's own
  /// `ext` values so the user sees what THIS tab will actually launch with.
  ///
  /// A var with no `extKey` is the credential and prints masked; one marked
  /// `onlyWhenSet` is omitted while its key is empty, because an empty value
  /// means the variable is not set at all and printing `NAME=` would describe a
  /// spawn that does not happen.
  function envPreview(): string[] {
    const id = harness?.id;
    if (!localProvider || !id) return [];
    const ext = harnessRow($settingsStore, id).ext ?? {};
    const out: string[] = [];
    for (const v of localProvider) {
      if (v.extKey === null) {
        out.push(`${v.name}=…`);
        continue;
      }
      const value = ext[v.extKey];
      const text = typeof value === 'string' ? value : '';
      if (v.onlyWhenSet && text === '') continue;
      out.push(`${v.name}=${text}`);
    }
    return out;
  }

  function update<K extends keyof AiToolTabConfig>(key: K, value: AiToolTabConfig[K]) {
    settings = { ...settings, [key]: value };
    onchange();
  }

  function setSpeakEnabled(value: boolean) {
    settings = { ...settings, tts_injection: { enabled: value } };
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
          ? defaultPalette()
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
        The binary spawned for this tab{#if affordances?.defaultCommand}. Defaults
          to <code>{affordances.defaultCommand}</code>; edit if your
          <code>{affordances.defaultCommand}</code> binary lives somewhere PATH
          doesn't reach{#if affordances.commandExample} (e.g. an absolute path
            like <code>{affordances.commandExample}</code>){/if}{/if}. Restart
        this tab after changing.
      </small>
    </label>

    {#if settings.cwd}
      <label class="field">
        <span>Working directory</span>
        <input type="text" value={settings.cwd} readonly />
        <small class="hint">
          Read-only. This tab was spawned into a cImp-managed git worktree
          (V13 Phase D's "New tab in worktree…") — it always runs here, not
          the project root. Manage the worktree itself (diff / merge /
          discard) from the Workbench tab's Worktrees section.
        </small>
      </label>
    {/if}

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

    <label class="field">
      <span>
        Environment variables
        {#if restartRequired}
          <Pill variant="orange" size="xs">restart required</Pill>
        {/if}
      </span>
      <EnvEditor
        env={settings.env}
        onchange={(v) => update('env', v)}
      />
      <small class="hint">
        Extra environment variables for this tab's subprocess. They
        override any values cImp synthesizes (the custom-provider set this
        harness declares, where this tab has one). Values are stored cleartext
        in settings.json. Restart this tab after changing.
      </small>
    </label>

    <!-- Issue #109: the custom provider is the TAB, not a checkbox. This tab
         either is its harness's custom-provider tab — and then it always
         launches against the provider configured below it — or it is not, and
         there is nothing here to say. A harness with no custom-provider variant
         at all keeps its own explanation of why. -->
    {#if localProvider && isProviderTab}
      <p class="hint hint-effective-env">
        This tab always launches against the custom provider configured below:
        it synthesizes
        {#each envPreview() as row, i}{#if i > 0}, {/if}<code>{row}</code>{/each}
        from the harness's own settings. Per-tab env entries above override
        these.
      </p>
    {:else if !localProvider && affordances?.localProviderNote}
      <p class="hint hint-effective-env">{affordances.localProviderNote}</p>
    {/if}
  </div>

  <div class="group">
    <h4>Text-to-speech</h4>
      <label class="checkbox">
        <input
          type="checkbox"
          checked={settings.tts_injection.enabled}
          onchange={(e) =>
            setSpeakEnabled((e.currentTarget as HTMLInputElement).checked)}
        />
        <span>Speak this tab's replies aloud</span>
      </label>
      <small class="hint">
        Reads the assistant's prose through the TTS voice while this tab is
        active. Takes effect immediately — no restart needed.
      </small>
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
        {#each $paletteRegistry as p}
          <option value={p.name}>{p.name}</option>
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

  /* Effective-env block on a harness's custom-provider tab (and the
     "manages its own providers" note in its place elsewhere).
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
