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
    background: #2a2a2a;
    border: 1px solid #444;
    color: #ddd;
    padding: 6px 8px;
    border-radius: 4px;
    font-family: monospace;
    font-size: 12px;
    resize: vertical;
    box-sizing: border-box;
  }
  textarea:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .reset {
    align-self: flex-end;
    background: #2a2a2a;
    border: 1px solid #444;
    color: #aaa;
    padding: 3px 10px;
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
