<script lang="ts">
  // Edits an array of strings as one row per item with add/remove buttons.
  // Used for the per-tab `args` array in the Tabs settings; the v1
  // newline-textarea pattern was brittle around whitespace and didn't
  // surface "this row is one arg" clearly.
  let {
    items = $bindable<string[]>([]),
    placeholder = '',
    onchange,
    oncommit,
  }: {
    items: string[];
    placeholder?: string;
    onchange?: () => void;
    /// Fired on discrete edit boundaries only — input blur/Enter and row
    /// removal (NOT per keystroke, and not on adding an empty row). Lets a
    /// consumer persist on commit while `bind:items` tracks keystrokes
    /// locally; consumers that persist via `onchange` are unaffected.
    oncommit?: () => void;
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
    oncommit?.();
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
        onblur={() => oncommit?.()}
        onkeydown={(e) => {
          if (e.key === 'Enter') oncommit?.();
        }}
      />
      <!-- `icon` opts out of the TUI themes' `[ … ]` bracket framing (their
           `section button:not(.icon)::before/::after` rules) — brackets
           don't fit in this 28px box and wrap it three lines tall. -->
      <button
        type="button"
        class="remove icon"
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
  /* Base button look. Deliberately element-level — the lowest scoped
     specificity — so a TUI theme's flat `[ … ]` section-button reset
     (`html[data-theme] section button`) outranks it; a class selector here
     would win instead and draw this box AROUND the theme's brackets. Only
     the `.remove` icon button keeps class-level visuals: it opts out of the
     brackets via `icon`, so its box is correct under every theme. */
  button {
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-quiet-strong);
    padding: var(--space-1) 10px;
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--font-size-xs);
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  button:hover {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  /* Class-level (not element-level) ON PURPOSE: `icon` skips the TUI
     bracket framing, but the TUI section-button reset still strips
     background/border — these higher-specificity rules keep the compact
     box in every theme. */
  .remove {
    width: 28px;
    padding: 0;
    line-height: 24px;
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-quiet-strong);
    border-radius: var(--radius-sm);
    font-size: 16px;
  }
  /* #129 (a): scoped under `.array-editor` so this beats the hoisted
     `settings-chrome.css` rule `button:hover:not(:disabled)` (0,3,1) — a bare
     `.remove:hover` is 0,3,0 and would lose the danger colours to the generic
     hover. Adding the root class makes it 0,4,0. */
  .array-editor .remove:hover {
    background: var(--surface-danger-bg);
    color: var(--text-danger-pale);
    border-color: var(--border-danger-strong);
  }
  .add {
    align-self: flex-start;
    margin-top: var(--space-1);
  }
</style>
