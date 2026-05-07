<script lang="ts">
  // Configure Tab dialog. Reuses the same field component as the New
  // Shell Tab dialog. Pre-fills with the target tab's current name; the
  // dialog asks the backend for the live shell config via a new IPC
  // call (M3 will move this to settings; for M2 we read the registry).
  //
  // Save calls `reconfigure_shell_tab`; the running PTY does NOT
  // restart — per design, the new config takes effect on next restart.
  // A footer note communicates this to the user.
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { closeDialog, dialogState } from './store';
  import {
    getShellTabConfig,
    reconfigureShellTab,
    type TabLifecycleError,
  } from '../ipc';
  import ShellTabFields from './ShellTabFields.svelte';
  import { settings as settingsStore } from '../settings/store';
  import type {
    BackgroundOverrideWire,
    TerminalBackgroundSettings,
    TerminalThemeSettings,
    ThemeColorsWire,
  } from '../settings/types';
  import { BUNDLED_THEME_NAMES, BUNDLED_THEMES, resolveBundledTheme } from '../themes';
  import ThemeSwatch from '../settings/ThemeSwatch.svelte';
  import CustomThemeEditor from '../settings/CustomThemeEditor.svelte';
  import BackgroundConfigEditor from '../settings/BackgroundConfigEditor.svelte';

  let name = $state('');
  let command = $state('');
  let argsString = $state('');
  let cwd = $state('');
  let notificationsError = $state('');
  let notificationsExited = $state('');
  let themeOverride = $state<TerminalThemeSettings | null>(null);
  let backgroundOverride = $state<BackgroundOverrideWire | null>(null);
  let error = $state<TabLifecycleError | null>(null);
  let busy = $state(false);

  // Live global theme name, used for the "Use global default (current: X)"
  // entry label. Reactive via $derived so changes elsewhere reflect.
  let globalThemeName = $derived($settingsStore.terminal.theme.name);

  // V1.4-03: human-readable summary of the global background, shown in
  // the "Use global default" Background-row option label.
  let globalBgSummary = $derived(
    globalBgSummaryOf($settingsStore.terminal.background),
  );

  // Override-row dropdown value:
  //   '__inherit'      → themeOverride = null
  //   bundled name     → themeOverride = { name, custom: null }
  //   'Custom'         → themeOverride = { name: 'Custom', custom: ... }
  let overrideSelection = $derived(
    themeOverride === null ? '__inherit' : themeOverride.name,
  );

  // V1.4-03: three-state Background override.
  //   '__inherit'  → backgroundOverride = null
  //   '__disabled' → backgroundOverride = 'disabled'
  //   '__custom'   → backgroundOverride = TerminalBackgroundSettings object
  type BgOverrideMode = '__inherit' | '__disabled' | '__custom';
  let bgOverrideSelection = $derived<BgOverrideMode>(
    backgroundOverride === null
      ? '__inherit'
      : backgroundOverride === 'disabled'
        ? '__disabled'
        : '__custom',
  );

  function globalBgSummaryOf(bg: TerminalBackgroundSettings): string {
    if (bg.image) {
      const filename = bg.image.split(/[\\/]/).pop() ?? bg.image;
      return `image: ${filename}`;
    }
    if (bg.color) return `solid ${bg.color}`;
    return 'theme background';
  }

  let isOpen = $derived($dialogState.kind === 'configure-tab');
  let targetTab = $derived(
    $dialogState.kind === 'configure-tab' ? $dialogState.tab : null,
  );

  let lastTab: string | null = null;
  $effect(() => {
    const t = targetTab;
    if (isOpen && t && t !== lastTab) {
      lastTab = t;
      void initFields(t);
    } else if (!isOpen && lastTab) {
      lastTab = null;
    }
  });

  async function initFields(tab: string): Promise<void> {
    error = null;
    busy = false;
    // Read theme_override from the live settings store. The IPC's
    // `get_shell_tab_config` doesn't carry it (kept narrow for M2's
    // shell-fields shape); the store is canonical and always in sync
    // because the backend broadcasts on change.
    const liveTab = get(settingsStore).tabs.find((t) => t.id === tab);
    themeOverride = liveTab?.theme_override ?? null;
    backgroundOverride = liveTab?.background_override ?? null;
    try {
      const cfg = await getShellTabConfig(tab);
      name = cfg.name;
      command = cfg.command;
      argsString = cfg.args;
      cwd = cfg.cwd ?? '';
      notificationsError = cfg.notifications_error;
      notificationsExited = cfg.notifications_exited;
    } catch (e) {
      console.error('get_shell_tab_config failed:', e);
      // Fall back to empty fields on read failure; the user can re-enter.
      name = '';
      command = '';
      argsString = '';
      cwd = '';
      notificationsError = '';
      notificationsExited = '';
    }
  }

  /// Translate the dropdown selection into a `theme_override` value.
  /// Seeds the Custom block from the previously-effective palette so
  /// the user opens the editor with sensible starting colors.
  function selectOverride(value: string): void {
    if (value === '__inherit') {
      themeOverride = null;
      return;
    }
    if (value === 'Custom') {
      // Determine seed source: the previously-effective theme is either
      // the prior override (if any) or the global theme.
      const liveGlobal = get(settingsStore).terminal.theme;
      const previousName =
        themeOverride === null ? liveGlobal.name : themeOverride.name;
      const seed =
        previousName === 'Custom'
          ? BUNDLED_THEMES.Default
          : resolveBundledTheme(previousName);
      themeOverride = {
        name: 'Custom',
        custom: { ...seed } as ThemeColorsWire,
      };
      return;
    }
    themeOverride = { name: value, custom: null };
  }

  /// V1.4-03: translate the Background-row dropdown selection into a
  /// `background_override` value. Custom mode seeds from the existing
  /// override (if any) or the current global background.
  function selectBgOverride(value: BgOverrideMode): void {
    if (value === '__inherit') {
      backgroundOverride = null;
      return;
    }
    if (value === '__disabled') {
      backgroundOverride = 'disabled';
      return;
    }
    // '__custom' — already custom: preserve.
    if (
      backgroundOverride !== null &&
      backgroundOverride !== 'disabled' &&
      typeof backgroundOverride === 'object'
    ) {
      return;
    }
    const liveGlobal = get(settingsStore).terminal.background;
    backgroundOverride = { ...liveGlobal };
  }

  function cancel(): void {
    closeDialog();
  }

  async function save(): Promise<void> {
    if (busy || !targetTab) return;
    busy = true;
    error = null;
    try {
      await reconfigureShellTab({
        tab: targetTab,
        name,
        command,
        argsString,
        cwd: cwd.trim() === '' ? null : cwd,
        env: {},
        notificationsError,
        notificationsExited,
        themeOverride,
        backgroundOverride,
      });
      closeDialog();
    } catch (e) {
      const wire = e as { kind?: string } | string | null;
      if (wire && typeof wire === 'object' && 'kind' in wire) {
        error = wire as TabLifecycleError;
      } else {
        error = {
          kind: 'internal',
          message: typeof e === 'string' ? e : JSON.stringify(e),
        };
      }
    } finally {
      busy = false;
    }
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (!isOpen) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      cancel();
    } else if (e.key === 'Enter' && (e.target as HTMLElement)?.tagName !== 'BUTTON') {
      e.preventDefault();
      void save();
    }
  }

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  });
</script>

{#if isOpen}
  <div class="backdrop" onclick={cancel} role="presentation"></div>
  <div class="card" role="dialog" aria-label="Configure tab">
    <h2>Configure tab</h2>
    <ShellTabFields
      bind:name
      bind:command
      bind:argsString
      bind:cwd
      bind:notificationsError
      bind:notificationsExited
      {error}
    />
    {#if error && !['empty-name', 'command-not-found', 'cwd-not-found'].includes(error.kind)}
      <div class="generic-error">
        {#if error.kind === 'wrong-kind'}
          This tab cannot be reconfigured.
        {:else if error.kind === 'tab-not-found'}
          Tab not found.
        {:else if error.kind === 'internal'}
          {error.message}
        {:else}
          {error.kind}
        {/if}
      </div>
    {/if}

    <h3 class="section-h">Appearance</h3>
    <label class="appearance-row">
      <span>Terminal palette</span>
      <select
        value={overrideSelection}
        onchange={(e) =>
          selectOverride((e.currentTarget as HTMLSelectElement).value)}
      >
        <option value="__inherit">Use global default (current: {globalThemeName})</option>
        {#each BUNDLED_THEME_NAMES as paletteName}
          <option value={paletteName}>{paletteName}</option>
        {/each}
        <option value="Custom">Custom…</option>
      </select>
      {#if themeOverride !== null}
        <ThemeSwatch name={themeOverride.name} custom={themeOverride.custom} />
      {/if}
    </label>
    {#if themeOverride && themeOverride.name === 'Custom' && themeOverride.custom}
      <CustomThemeEditor
        value={themeOverride.custom}
        onchange={(next) => {
          themeOverride = themeOverride
            ? { ...themeOverride, custom: next }
            : null;
        }}
      />
    {/if}

    <label class="appearance-row">
      <span>Background</span>
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
      <small class="hint-row">
        No observable effect when the global background is also "Theme
        default."
      </small>
    {/if}
    {#if bgOverrideSelection === '__custom' && backgroundOverride !== null && backgroundOverride !== 'disabled' && typeof backgroundOverride === 'object'}
      <BackgroundConfigEditor
        bind:config={
          () => backgroundOverride as TerminalBackgroundSettings,
          (v) => {
            backgroundOverride = v;
          }
        }
      />
    {/if}

    <small class="footer-note">
      Changes apply on next shell restart. Palette changes apply
      immediately.
    </small>
    <div class="actions">
      <button type="button" class="cancel" onclick={cancel} disabled={busy}>
        Cancel
      </button>
      <button type="button" class="primary" onclick={save} disabled={busy}>
        {busy ? 'Saving…' : 'Save'}
      </button>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 100;
  }
  .card {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    padding: 20px var(--space-5);
    width: 480px;
    max-width: calc(100vw - 40px);
    color: var(--text-primary);
    z-index: 101;
    box-shadow: var(--shadow-lg);
  }
  h2 {
    margin: 0 0 var(--space-4);
    font-size: 16px;
    font-weight: 600;
  }
  .section-h {
    margin: var(--space-4) 0 var(--space-2) 0;
    font-size: var(--font-size-sm);
    font-weight: 600;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .appearance-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-bottom: var(--space-2);
  }
  .appearance-row span {
    flex: 0 0 140px;
    color: var(--text-secondary);
    font-size: var(--font-size-sm);
  }
  .appearance-row select {
    flex: 1;
  }
  .footer-note {
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    display: block;
    margin-top: var(--space-2);
  }
  .hint-row {
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    display: block;
    margin-bottom: var(--space-2);
    margin-left: 140px;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-4);
  }
  .actions button {
    padding: 6px var(--space-4);
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: var(--font-size-md);
    border: 1px solid var(--border-default);
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .actions button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .cancel {
    background: var(--surface-4);
    color: var(--text-secondary);
  }
  .cancel:hover:not([disabled]) {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  .primary {
    background: var(--accent);
    color: var(--accent-fg);
    border-color: var(--accent);
    font-weight: var(--font-weight-semibold);
  }
  .primary:hover:not([disabled]) {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }
  button[disabled] {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .generic-error {
    color: var(--danger);
    font-size: var(--font-size-sm);
    margin-top: -6px;
    margin-bottom: var(--space-2);
  }
</style>
