<script lang="ts">
  import type { AiNotificationConfig, NotificationSlot } from './types';

  // Four labeled rows (idle, awaiting permission, question, error). Each
  // row carries an enabled checkbox plus a text input with a per-row
  // reset. V1.11 promoted each slot to `{ enabled, text }` so disabling
  // a notification preserves the user's typed text — re-enabling
  // restores it without retyping.
  let {
    notifications = $bindable<AiNotificationConfig>({
      idle: { enabled: false, text: '' },
      awaiting_permission: { enabled: false, text: '' },
      question: { enabled: false, text: '' },
      error: { enabled: false, text: '' },
    }),
    defaults,
    onchange,
  }: {
    notifications: AiNotificationConfig;
    defaults: AiNotificationConfig;
    onchange?: () => void;
  } = $props();

  type RowKey = keyof AiNotificationConfig;
  const rows: { key: RowKey; label: string }[] = [
    { key: 'idle', label: 'When tab becomes idle while you’re on another tab' },
    {
      key: 'awaiting_permission',
      label: 'When tab requests permission while you’re on another tab',
    },
    {
      key: 'question',
      label: 'When tab asks a question while you’re on another tab',
    },
    { key: 'error', label: 'When tab encounters an error while you’re on another tab' },
  ];

  function updateSlot(key: RowKey, slot: NotificationSlot) {
    notifications = { ...notifications, [key]: slot };
    onchange?.();
  }

  function toggleEnabled(key: RowKey, enabled: boolean) {
    updateSlot(key, { ...notifications[key], enabled });
  }

  function updateText(key: RowKey, text: string) {
    updateSlot(key, { ...notifications[key], text });
  }

  function reset(key: RowKey) {
    updateSlot(key, { ...defaults[key] });
  }

  function isAtDefault(key: RowKey): boolean {
    const cur = notifications[key];
    const def = defaults[key];
    return cur.enabled === def.enabled && cur.text === def.text;
  }
</script>

<div class="notif-editor">
  {#each rows as row (row.key)}
    <div class="row">
      <label class="row-toggle">
        <input
          type="checkbox"
          checked={notifications[row.key].enabled}
          onchange={(e) =>
            toggleEnabled(
              row.key,
              (e.currentTarget as HTMLInputElement).checked,
            )}
        />
        <span class="label">{row.label}</span>
      </label>
      <div class="controls">
        <input
          type="text"
          value={notifications[row.key].text}
          disabled={!notifications[row.key].enabled}
          oninput={(e) =>
            updateText(row.key, (e.currentTarget as HTMLInputElement).value)}
        />
        <button
          type="button"
          class="reset"
          disabled={isAtDefault(row.key)}
          onclick={() => reset(row.key)}
        >
          Reset
        </button>
      </div>
    </div>
  {/each}
</div>

<style>
  .notif-editor {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .row {
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .row-toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    cursor: pointer;
  }
  .row-toggle input[type='checkbox'] {
    margin: 0;
  }
  .label {
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
  }
  .controls {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .controls input[type='text'] {
    flex: 1;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .controls input[type='text']:focus {
    outline: none;
    border-color: var(--accent);
  }
  .controls input[type='text']:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .reset {
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-quiet-strong);
    padding: var(--space-1) 10px;
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--font-size-xs);
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard);
  }
  .reset:hover:not(:disabled) {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  .reset:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .reset:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
