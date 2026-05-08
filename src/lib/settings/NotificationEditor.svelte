<script lang="ts">
  import type { AiNotificationConfig } from './types';

  // Three labeled rows (idle, awaiting permission, error) with a per-row
  // reset. Defaults come from a prop so the same component renders for any
  // tab. Notification firing logic itself ships in V2-04; this is just the
  // configuration surface.
  let {
    notifications = $bindable<AiNotificationConfig>({
      idle: '',
      awaiting_permission: '',
      question: '',
      error: '',
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

  function update(key: RowKey, value: string) {
    notifications = { ...notifications, [key]: value };
    onchange?.();
  }

  function reset(key: RowKey) {
    update(key, defaults[key]);
  }
</script>

<div class="notif-editor">
  {#each rows as row (row.key)}
    <div class="row">
      <span class="label">{row.label}</span>
      <div class="controls">
        <input
          type="text"
          value={notifications[row.key]}
          oninput={(e) =>
            update(row.key, (e.currentTarget as HTMLInputElement).value)}
        />
        <button
          type="button"
          class="reset"
          disabled={notifications[row.key] === defaults[row.key]}
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
  .label {
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
  }
  .controls {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .controls input {
    flex: 1;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .controls input:focus {
    outline: none;
    border-color: var(--accent);
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
