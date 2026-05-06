<script lang="ts">
  import type { TabId } from '../tabs/types';
  import type { AiToolTabConfig } from './types';
  import ArrayEditor from './ArrayEditor.svelte';
  import TextAreaWithReset from './TextAreaWithReset.svelte';
  import NotificationEditor from './NotificationEditor.svelte';

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
        <span class="restart-tag">restart required</span>
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
        <span class="restart-tag">restart required</span>
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
    padding: 12px 14px;
    background: #1a1a1a;
    border: 1px solid #2a2a2a;
    border-radius: 6px;
  }
  h3 {
    margin: 0 0 12px 0;
    font-size: 13px;
    font-weight: 600;
    color: #ddd;
  }
  .restart-tag {
    display: inline-block;
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: #d8b8ff;
    background: #3a2a55;
    border: 1px solid #6f42a8;
    padding: 1px 6px;
    border-radius: 8px;
    margin-left: 6px;
    vertical-align: middle;
  }
  .warn {
    margin: -6px 0 12px 0;
    padding: 8px 10px;
    background: #2a230f;
    border: 1px solid #6a571a;
    border-radius: 4px;
    color: #e0d090;
    font-size: 11px;
    line-height: 1.4;
  }
  .warn code {
    background: #1a1a1a;
    padding: 1px 4px;
    border-radius: 3px;
    font-size: 11px;
  }
  small.hint.inline {
    margin: 0 0 0 10px;
    color: #777;
  }
  .restart-row {
    display: flex;
    align-items: center;
    margin-top: 8px;
  }
  .restart-btn {
    background: #6f42a8;
    border: 1px solid #6f42a8;
    color: #fff;
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
  }
  .restart-btn:hover:not(:disabled) {
    background: #835ac5;
  }
  .restart-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
