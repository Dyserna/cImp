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
    background: #2a2a2a;
    border: 1px solid #444;
    border-radius: 6px;
    padding: 20px 24px;
    width: 360px;
    max-width: calc(100vw - 40px);
    color: #e0e0e0;
    z-index: 101;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.6);
  }
  h2 {
    margin: 0 0 16px;
    font-size: 16px;
    font-weight: 600;
  }
  .field {
    display: block;
    margin-bottom: 12px;
  }
  .label {
    display: block;
    font-size: 12px;
    color: #c0c0c0;
    margin-bottom: 4px;
  }
  input[type='text'] {
    width: 100%;
    background: #1a1a1a;
    border: 1px solid #444;
    border-radius: 3px;
    padding: 6px 8px;
    color: #e0e0e0;
    font-size: 13px;
    box-sizing: border-box;
  }
  input[type='text']:focus {
    outline: none;
    border-color: #4a90e2;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .actions button {
    padding: 6px 16px;
    border-radius: 3px;
    cursor: pointer;
    font-size: 13px;
    border: 1px solid #444;
  }
  .cancel {
    background: #2a2a2a;
    color: #c0c0c0;
  }
  .cancel:hover:not([disabled]) {
    background: #383838;
  }
  .primary {
    background: #4a90e2;
    color: white;
    border-color: #4a90e2;
  }
  .primary:hover:not([disabled]) {
    background: #5aa0f2;
  }
  button[disabled] {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .confirm {
    color: #f0c060;
    font-size: 12px;
    margin-bottom: 8px;
  }
  .error {
    color: #e74c3c;
    font-size: 12px;
    margin-bottom: 8px;
  }
</style>
