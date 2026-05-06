<script lang="ts">
  import type { TabId } from '../tabs/types';
  import type { AiToolTabConfig } from './types';
  import ArrayEditor from './ArrayEditor.svelte';
  import TextAreaWithReset from './TextAreaWithReset.svelte';
  import NotificationEditor from './NotificationEditor.svelte';
  import Pill from '../Pill.svelte';

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
