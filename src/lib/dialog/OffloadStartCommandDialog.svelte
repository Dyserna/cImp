<script lang="ts">
  // "Show command on start" confirm dialog (the Offload server dashboard's Start
  // button, gated by the Local backend's setting). Mounted alongside the
  // other modal dialogs in App.svelte; renders only when the discriminator
  // is 'offload-start-command'. The command is editable but applies to this
  // launch only — the backend treats it as a one-shot override and never
  // persists it. Validation is the backend's own start path (same parse as
  // the configured command); its error renders inline and the dialog stays
  // open. While the start call is in flight the dialog can't be dismissed,
  // so a failure always has a live surface to land on.
  import { closeDialog, dialogState } from './store';
  import ModalShell from './ModalShell.svelte';
  import { offloadBackendStart } from '../offload';
  import { errorMessage } from '../errors';

  let target = $derived(
    $dialogState.kind === 'offload-start-command' ? $dialogState : null,
  );
  let isOpen = $derived(target !== null);
  let command = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);
  let textareaEl: HTMLTextAreaElement | undefined = $state();

  let lastOpenSeen = false;
  $effect(() => {
    if (target && !lastOpenSeen) {
      lastOpenSeen = true;
      command = target.command;
      busy = false;
      error = null;
      queueMicrotask(() => textareaEl?.focus());
    } else if (!target && lastOpenSeen) {
      lastOpenSeen = false;
    }
  });

  function cancel(): void {
    if (busy) return;
    closeDialog();
  }

  async function handleStart(): Promise<void> {
    const name = target?.name;
    if (busy || !name) return;
    busy = true;
    error = null;
    try {
      await offloadBackendStart(name, command);
      closeDialog();
    } catch (e) {
      error = errorMessage(e);
    } finally {
      busy = false;
    }
  }
</script>

<ModalShell
  open={isOpen}
  label="Start server command"
  title={`Start "${target?.name ?? ''}" with this command?`}
  width={640}
  onCancel={cancel}
  onEscape={cancel}
>
  <textarea rows="6" wrap="soft" bind:this={textareaEl} bind:value={command} disabled={busy}
  ></textarea>
  <div class="hint">
    Edits apply to this launch only — the command saved in Settings is unchanged.
  </div>
  {#if error}
    <div class="error">{error}</div>
  {/if}
  {#snippet actions()}
    <button type="button" class="cancel" onclick={cancel} disabled={busy}>Cancel</button>
    <button type="button" class="primary" onclick={handleStart} disabled={busy}>
      {busy ? 'Starting…' : 'Start'}
    </button>
  {/snippet}
</ModalShell>

<style>
  textarea {
    width: 100%;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-md);
    padding: 6px var(--space-2);
    color: var(--text-primary);
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: var(--font-size-sm);
    box-sizing: border-box;
    resize: vertical;
    min-height: 5.5em;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  textarea:focus {
    outline: none;
    border-color: var(--accent);
  }
  .hint {
    color: var(--text-secondary);
    font-size: var(--font-size-sm);
    margin-top: var(--space-1);
  }
  .error {
    color: var(--danger);
    font-size: var(--font-size-sm);
    margin-top: var(--space-2);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
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
</style>
