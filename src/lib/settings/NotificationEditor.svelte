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
    color: #888;
    font-size: 11px;
  }
  .controls {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .controls input {
    flex: 1;
    background: #2a2a2a;
    border: 1px solid #444;
    color: #ddd;
    padding: 6px 8px;
    border-radius: 4px;
    font-size: 12px;
  }
  .reset {
    background: #2a2a2a;
    border: 1px solid #444;
    color: #aaa;
    padding: 4px 10px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 11px;
  }
  .reset:hover:not(:disabled) {
    background: #333;
    color: #ddd;
  }
  .reset:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
