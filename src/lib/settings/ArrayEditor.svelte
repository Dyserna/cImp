<script lang="ts">
  // Edits an array of strings as one row per item with add/remove buttons.
  // Used for the per-tab `args` array in the Tabs settings; the v1
  // newline-textarea pattern was brittle around whitespace and didn't
  // surface "this row is one arg" clearly.
  let {
    items = $bindable<string[]>([]),
    placeholder = '',
    onchange,
  }: {
    items: string[];
    placeholder?: string;
    onchange?: () => void;
  } = $props();

  function updateAt(index: number, value: string) {
    const next = items.slice();
    next[index] = value;
    items = next;
    onchange?.();
  }

  function removeAt(index: number) {
    items = items.filter((_, i) => i !== index);
    onchange?.();
  }

  function addRow() {
    items = [...items, ''];
    onchange?.();
  }
</script>

<div class="array-editor">
  {#each items as item, i (i)}
    <div class="row">
      <input
        type="text"
        value={item}
        {placeholder}
        oninput={(e) => updateAt(i, (e.currentTarget as HTMLInputElement).value)}
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
  <button type="button" class="add" onclick={addRow}>+ Add</button>
</div>

<style>
  .array-editor {
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
    flex: 1;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: monospace;
    font-size: var(--font-size-sm);
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .row input:focus {
    outline: none;
    border-color: var(--accent);
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
