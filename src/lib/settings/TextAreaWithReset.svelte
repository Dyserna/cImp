<script lang="ts">
  // Multi-line text area paired with a "Reset to default" button. The default
  // value comes from a prop (rather than baked in) so the same component can
  // back per-tab fields whose defaults differ.
  let {
    value = $bindable<string>(''),
    defaultValue,
    disabled = false,
    rows = 6,
    placeholder = '',
    onchange,
  }: {
    value: string;
    defaultValue: string;
    disabled?: boolean;
    rows?: number;
    placeholder?: string;
    onchange?: () => void;
  } = $props();

  const isDefault = $derived(value === defaultValue);

  function reset() {
    value = defaultValue;
    onchange?.();
  }
</script>

<div class="textarea-with-reset">
  <textarea
    {rows}
    {placeholder}
    {disabled}
    {value}
    oninput={(e) => {
      value = (e.currentTarget as HTMLTextAreaElement).value;
      onchange?.();
    }}
  ></textarea>
  <button
    type="button"
    class="reset"
    onclick={reset}
    disabled={disabled || isDefault}
    title="Replace with the built-in default"
  >
    Reset to default
  </button>
</div>

<style>
  .textarea-with-reset {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  textarea {
    width: 100%;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: monospace;
    font-size: var(--font-size-sm);
    resize: vertical;
    box-sizing: border-box;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  textarea:focus {
    outline: none;
    border-color: var(--accent);
  }
  textarea:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .reset {
    align-self: flex-end;
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-quiet-strong);
    padding: 3px 10px;
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
