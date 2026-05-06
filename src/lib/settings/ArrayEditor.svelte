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
    background: #2a2a2a;
    border: 1px solid #444;
    color: #ddd;
    padding: 6px 8px;
    border-radius: 4px;
    font-family: monospace;
    font-size: 12px;
  }
  .remove {
    width: 28px;
    padding: 0;
    line-height: 24px;
    background: #2a2a2a;
    border: 1px solid #444;
    color: #aaa;
    border-radius: 4px;
    cursor: pointer;
    font-size: 16px;
  }
  .remove:hover {
    background: #3a2020;
    color: #f0aaaa;
    border-color: #6a3030;
  }
  .add {
    align-self: flex-start;
    margin-top: 4px;
    background: #2a2a2a;
    border: 1px solid #444;
    color: #aaa;
    padding: 4px 10px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 11px;
  }
  .add:hover {
    background: #333;
    color: #ddd;
  }
</style>
