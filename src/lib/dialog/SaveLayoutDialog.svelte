<script lang="ts">
  // Save-current-layout-as dialog. Mounted alongside the other modal
  // dialogs in App.svelte; renders only when the dialog discriminator
  // is 'save-layout'. Defaults the name to "Layout {N}" where N is
  // one more than the existing preset count.
  import { onMount } from 'svelte';
  import { closeDialog, dialogState } from './store';
  import { settings } from '../settings/store';
  import { saveCurrentLayoutAsPreset } from '../layout/presets';

  let isOpen = $derived($dialogState.kind === 'save-layout');
  let name = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);
  let confirmingOverwrite = $state(false);
  let inputEl: HTMLInputElement | undefined = $state();

  let lastOpenSeen = false;
  $effect(() => {
    if (isOpen && !lastOpenSeen) {
      lastOpenSeen = true;
      const count = $settings.layout_presets.length;
      name = `Layout ${count + 1}`;
      busy = false;
      error = null;
      confirmingOverwrite = false;
      // Focus the input on open. Using $effect's microtask timing to
      // wait for the input to mount.
      queueMicrotask(() => {
        inputEl?.focus();
        inputEl?.select();
      });
    } else if (!isOpen && lastOpenSeen) {
      lastOpenSeen = false;
    }
  });

  function cancel(): void {
    if (busy) return;
    closeDialog();
  }

  /// True if a preset already has the name the user is about to save
  /// under. The popover and the confirm path branch on this.
  let nameExists = $derived(
    $settings.layout_presets.some((p) => p.name === name.trim()),
  );

  async function performSave(): Promise<void> {
    busy = true;
    error = null;
    try {
      await saveCurrentLayoutAsPreset(name.trim());
      closeDialog();
    } catch (e) {
      error = typeof e === 'string' ? e : JSON.stringify(e);
    } finally {
      busy = false;
    }
  }

  async function handleSubmit(): Promise<void> {
    if (busy) return;
    if (name.trim() === '') {
      error = 'Name cannot be empty';
      return;
    }
    if (nameExists && !confirmingOverwrite) {
      // Two-step: first submit reveals an inline overwrite confirm.
      confirmingOverwrite = true;
      return;
    }
    await performSave();
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (!isOpen) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      cancel();
    } else if (
      e.key === 'Enter' &&
      (e.target as HTMLElement)?.tagName !== 'BUTTON'
    ) {
      e.preventDefault();
      void handleSubmit();
    }
  }

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  });
</script>

{#if isOpen}
  <div class="backdrop" onclick={cancel} role="presentation"></div>
  <div class="card" role="dialog" aria-label="Save layout">
    <h2>Save current layout</h2>
    <label class="field">
      <span class="label">Name</span>
      <input
        type="text"
        bind:this={inputEl}
        bind:value={name}
        oninput={() => {
          confirmingOverwrite = false;
          error = null;
        }}
        disabled={busy}
      />
    </label>
    {#if confirmingOverwrite && nameExists}
      <div class="confirm">
        A preset named “{name.trim()}” already exists. Save again to
        overwrite, or change the name.
      </div>
    {/if}
    {#if error}
      <div class="error">{error}</div>
    {/if}
    <div class="actions">
      <button type="button" class="cancel" onclick={cancel} disabled={busy}>
        Cancel
      </button>
      <button
        type="button"
        class="primary"
        onclick={handleSubmit}
        disabled={busy}
      >
        {#if busy}
          Saving…
        {:else if confirmingOverwrite && nameExists}
          Overwrite
        {:else}
          Save
        {/if}
      </button>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 100;
  }
  .card {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    padding: 20px var(--space-5);
    width: 360px;
    max-width: calc(100vw - 40px);
    color: var(--text-primary);
    z-index: 101;
    box-shadow: var(--shadow-lg);
  }
  h2 {
    margin: 0 0 var(--space-4);
    font-size: 16px;
    font-weight: 600;
  }
  .field {
    display: block;
    margin-bottom: var(--space-3);
  }
  .label {
    display: block;
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
    margin-bottom: var(--space-1);
  }
  input[type='text'] {
    width: 100%;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-md);
    padding: 6px var(--space-2);
    color: var(--text-primary);
    font-size: var(--font-size-md);
    box-sizing: border-box;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  input[type='text']:focus {
    outline: none;
    border-color: var(--accent);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-4);
  }
  .actions button {
    padding: 6px var(--space-4);
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: var(--font-size-md);
    border: 1px solid var(--border-default);
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .actions button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .cancel {
    background: var(--surface-4);
    color: var(--text-secondary);
  }
  .cancel:hover:not([disabled]) {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  .primary {
    background: var(--accent);
    color: var(--accent-fg);
    border-color: var(--accent);
    font-weight: var(--font-weight-semibold);
  }
  .primary:hover:not([disabled]) {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }
  button[disabled] {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .confirm {
    color: var(--text-warning);
    font-size: var(--font-size-sm);
    margin-bottom: var(--space-2);
  }
  .error {
    color: var(--danger);
    font-size: var(--font-size-sm);
    margin-bottom: var(--space-2);
  }
</style>
