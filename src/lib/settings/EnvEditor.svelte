<script lang="ts">
  // Edits a `Record<string, string>` env map as one key/value row per
  // entry with add/remove buttons — the map cousin of ArrayEditor. Rows
  // are owned locally so a half-typed key (empty, or briefly colliding
  // with another row) doesn't destroy its neighbor in the emitted map;
  // only rows with a non-empty trimmed key are committed.
  let {
    env,
    onchange,
  }: {
    env: Record<string, string>;
    onchange: (next: Record<string, string>) => void;
  } = $props();

  type Row = { key: string; value: string };

  function toRows(m: Record<string, string>): Row[] {
    return Object.entries(m ?? {}).map(([key, value]) => ({ key, value }));
  }

  function mapOf(rows: Row[]): Record<string, string> {
    const out: Record<string, string> = {};
    for (const r of rows) {
      const k = r.key.trim();
      if (k) out[k] = r.value;
    }
    return out;
  }

  /// Order-independent signature, so a prop echo of our own emit (same
  /// entries, possibly reordered by serde) doesn't clobber in-progress rows.
  function sig(m: Record<string, string>): string {
    return JSON.stringify(
      Object.entries(m ?? {}).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)),
    );
  }

  // Capturing the initial `env` here is deliberate: rows are local state
  // seeded once, and the $effect below is the explicit re-sync path for
  // later prop changes.
  // svelte-ignore state_referenced_locally
  let rows = $state<Row[]>(toRows(env));
  // svelte-ignore state_referenced_locally
  let lastCommitted = sig(env);

  // Reseed only when the prop diverges from what this editor last saw —
  // an external change (dialog re-open, settings reload), not our echo.
  $effect(() => {
    const incoming = sig(env);
    if (incoming !== lastCommitted) {
      rows = toRows(env);
      lastCommitted = incoming;
    }
  });

  function commit() {
    const m = mapOf(rows);
    lastCommitted = sig(m);
    onchange(m);
  }

  function updateKey(index: number, key: string) {
    const next = rows.slice();
    next[index] = { ...next[index], key };
    rows = next;
    commit();
  }

  function updateValue(index: number, value: string) {
    const next = rows.slice();
    next[index] = { ...next[index], value };
    rows = next;
    commit();
  }

  function removeAt(index: number) {
    rows = rows.filter((_, i) => i !== index);
    commit();
  }

  function addRow() {
    rows = [...rows, { key: '', value: '' }];
    // No commit — an empty key contributes nothing to the map yet.
  }
</script>

<div class="env-editor">
  {#each rows as row, i (i)}
    <div class="row">
      <input
        type="text"
        class="key"
        value={row.key}
        placeholder="NAME"
        oninput={(e) => updateKey(i, (e.currentTarget as HTMLInputElement).value)}
      />
      <span class="eq">=</span>
      <input
        type="text"
        class="value"
        value={row.value}
        placeholder="value"
        oninput={(e) => updateValue(i, (e.currentTarget as HTMLInputElement).value)}
      />
      <button
        type="button"
        class="remove"
        aria-label="Remove"
        onclick={() => removeAt(i)}
      >
        ×
      </button>
    </div>
  {/each}
  <button type="button" class="add" onclick={addRow}>+ Add variable</button>
</div>

<style>
  .env-editor {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .row {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .row input {
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: monospace;
    font-size: var(--font-size-sm);
    transition: border-color var(--motion-fast) var(--easing-standard);
    min-width: 0;
  }
  .row input.key {
    flex: 2 1 0;
  }
  .row input.value {
    flex: 3 1 0;
  }
  .row input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .eq {
    color: var(--text-tertiary);
    font-family: monospace;
  }
  .remove {
    width: 28px;
    padding: 0;
    line-height: 24px;
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-quiet-strong);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: 16px;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .remove:hover {
    background: var(--surface-danger-bg);
    color: var(--text-danger-pale);
    border-color: var(--border-danger-strong);
  }
  .remove:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .add {
    align-self: flex-start;
    margin-top: var(--space-1);
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
  .add:hover {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  .add:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
