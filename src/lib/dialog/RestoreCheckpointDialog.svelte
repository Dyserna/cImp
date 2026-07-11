<script lang="ts">
  // V13 Phase C — the restore confirmation dialog. Mounted alongside the
  // other modal dialogs in App.svelte; renders only when the dialog
  // discriminator is 'restore-checkpoint'. On open, fetches the dry-run
  // diff (checkpoint vs. the CURRENT working tree) so the user sees exactly
  // what a restore would touch BEFORE confirming — files changed/recreated,
  // and files that exist now but not in the checkpoint ("created since").
  //
  // Safety-critical UI contract (mirrors the backend's invariant D): the
  // "delete files created since" checkbox starts UNCHECKED. The dangerous
  // case is silently losing untracked new work, so keeping it is the
  // default and deleting it requires an explicit, visible opt-in.
  import { closeDialog, dialogState } from './store';
  import {
    workbenchCheckpointDiff,
    workbenchRestore,
    bumpWorkbenchCheckpointsVersion,
    type FileDiff,
  } from '../workbench';
  import { errorMessage } from '../errors';

  let isOpen = $derived($dialogState.kind === 'restore-checkpoint');
  let checkpointId = $derived($dialogState.kind === 'restore-checkpoint' ? $dialogState.id : '');
  let root = $derived($dialogState.kind === 'restore-checkpoint' ? $dialogState.root : undefined);

  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let files = $state<FileDiff[]>([]);
  let deleteNew = $state(false);
  let busy = $state(false);
  let restoreError = $state<string | null>(null);
  let done = $state<{ changed: number; deleted: number } | null>(null);

  let lastOpenId = '';
  $effect(() => {
    if (isOpen && checkpointId !== lastOpenId) {
      lastOpenId = checkpointId;
      deleteNew = false;
      restoreError = null;
      done = null;
      void loadDryRun();
    } else if (!isOpen) {
      lastOpenId = '';
    }
  });

  async function loadDryRun(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      files = await workbenchCheckpointDiff(checkpointId, undefined, root);
    } catch (e) {
      loadError = errorMessage(e);
    } finally {
      loading = false;
    }
  }

  // "Created since" = files this restore would delete IF `deleteNew` is
  // checked — an Added status in the dry-run diff (present now, absent in
  // the checkpoint). Everything else (Modified/Deleted/Renamed/Untracked-
  // in-the-checkpoint-sense-inverted) is content the restore will overwrite
  // or recreate regardless of the checkbox.
  let createdSince = $derived(files.filter((f) => f.status.kind === 'Added').map((f) => f.path));
  let willChange = $derived(files.filter((f) => f.status.kind !== 'Added').map((f) => f.path));

  function cancel(): void {
    if (busy) return;
    closeDialog();
  }

  async function confirmRestore(): Promise<void> {
    if (busy) return;
    busy = true;
    restoreError = null;
    try {
      const report = await workbenchRestore(checkpointId, deleteNew, root);
      done = { changed: report.changed.length, deleted: report.deleted.length };
      bumpWorkbenchCheckpointsVersion();
    } catch (e) {
      restoreError = errorMessage(e);
    } finally {
      busy = false;
    }
  }
</script>

{#if isOpen}
  <div class="backdrop" onclick={cancel} role="presentation"></div>
  <div class="card" role="dialog" aria-label="Restore checkpoint">
    <h2>Restore checkpoint {checkpointId}</h2>

    {#if done}
      <p class="done">
        Restored. {done.changed} file{done.changed === 1 ? '' : 's'} changed{done.deleted > 0
          ? `, ${done.deleted} deleted`
          : ''}. A safety checkpoint of the state right before this restore was
        taken automatically — you can undo this from the Timeline.
      </p>
      <div class="actions">
        <button type="button" class="primary" onclick={closeDialog}>Close</button>
      </div>
    {:else}
      <p class="note">
        A checkpoint of the CURRENT state is taken automatically before restoring,
        so this is always undoable. This never touches your own git repository —
        only the working files themselves.
      </p>

      {#if loading}
        <p class="msg">Checking what this restore would touch…</p>
      {:else if loadError}
        <p class="msg err">Couldn't preview this restore: {loadError}</p>
      {:else}
        {#if willChange.length > 0}
          <div class="file-group">
            <h3>Will be changed or recreated ({willChange.length})</h3>
            <ul>
              {#each willChange as path (path)}<li>{path}</li>{/each}
            </ul>
          </div>
        {/if}
        {#if createdSince.length > 0}
          <div class="file-group">
            <h3>Created since this checkpoint ({createdSince.length})</h3>
            <ul>
              {#each createdSince as path (path)}<li>{path}</li>{/each}
            </ul>
          </div>
        {/if}
        {#if willChange.length === 0 && createdSince.length === 0}
          <p class="msg">No difference from the current working tree.</p>
        {/if}

        <label class="checkbox">
          <input type="checkbox" bind:checked={deleteNew} disabled={createdSince.length === 0} />
          <span
            >Delete files created since this checkpoint ({createdSince.length})
            — unchecked keeps them</span
          >
        </label>
      {/if}

      {#if restoreError}
        <p class="msg err">{restoreError}</p>
      {/if}

      <div class="actions">
        <button type="button" class="cancel" onclick={cancel} disabled={busy}>Cancel</button>
        <button type="button" class="primary" onclick={confirmRestore} disabled={busy || loading}>
          {busy ? 'Restoring…' : 'Restore'}
        </button>
      </div>
    {/if}
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
    width: 480px;
    max-width: calc(100vw - 40px);
    max-height: calc(100vh - 80px);
    overflow-y: auto;
    color: var(--text-primary);
    z-index: 101;
    box-shadow: var(--shadow-lg);
  }
  h2 {
    margin: 0 0 var(--space-3);
    font-size: 16px;
    font-weight: 600;
  }
  h3 {
    margin: 0 0 4px;
    font-size: var(--font-size-sm);
    font-weight: 600;
    color: var(--text-secondary);
  }
  .note {
    margin: 0 0 var(--space-3);
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
    line-height: 1.4;
  }
  .msg {
    opacity: 0.75;
    font-style: italic;
    margin: 0 0 var(--space-3);
  }
  .msg.err {
    color: var(--text-danger-soft);
    font-style: normal;
  }
  .done {
    color: var(--text-success);
    line-height: 1.4;
    margin: 0 0 var(--space-3);
  }
  .file-group {
    margin-bottom: var(--space-3);
  }
  .file-group ul {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 140px;
    overflow-y: auto;
    border: 1px solid var(--border-faint);
    border-radius: var(--radius-sm);
  }
  .file-group li {
    padding: 2px 8px;
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
    border-bottom: 1px solid var(--border-faint);
  }
  .file-group li:last-child {
    border-bottom: none;
  }
  .checkbox {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    font-size: var(--font-size-sm);
    color: var(--text-primary);
    margin: var(--space-2) 0;
    cursor: pointer;
  }
  .checkbox input {
    margin-top: 2px;
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
</style>
