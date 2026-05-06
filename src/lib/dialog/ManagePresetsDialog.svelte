<script lang="ts">
  // Manage Presets dialog: scrollable list of every preset with
  // Restore / Rename (inline) / Delete (confirm) per row. Renders
  // reactively from the live `settings.layout_presets` array — every
  // CRUD action updates the array via the settings broadcast, no
  // local refresh needed.
  import { onMount } from 'svelte';
  import { closeDialog, dialogState } from './store';
  import { settings } from '../settings/store';
  import {
    deleteLayoutPreset,
    renameLayoutPreset,
    restoreLayoutPreset,
  } from '../layout/presets';
  import type { LayoutPreset } from '../settings/types';

  let isOpen = $derived($dialogState.kind === 'manage-presets');

  /// Inline-edit state per preset. Keyed by the *original* name so a
  /// rename doesn't lose the editing row mid-keystroke (the new
  /// settings broadcast lands and re-renders the list with the new
  /// name, but the edit state is keyed on the old name and migrates
  /// to the new one in `commitRename`).
  let editingName = $state<string | null>(null);
  let editValue = $state('');
  let editError = $state<string | null>(null);

  /// Confirm-delete state — show a confirm message inline next to the
  /// Delete button rather than a second modal. Keyed by preset name.
  let confirmingDelete = $state<string | null>(null);

  /// Sorted by created_at descending — same order as the popover, so
  /// the list is consistent across surfaces.
  let presets = $derived(
    [...$settings.layout_presets].sort((a, b) =>
      b.created_at.localeCompare(a.created_at),
    ),
  );

  function close(): void {
    if (editingName) cancelEdit();
    closeDialog();
  }

  function startEdit(p: LayoutPreset): void {
    editingName = p.name;
    editValue = p.name;
    editError = null;
  }

  function cancelEdit(): void {
    editingName = null;
    editValue = '';
    editError = null;
  }

  async function commitRename(): Promise<void> {
    if (editingName === null) return;
    const oldName = editingName;
    const newName = editValue.trim();
    if (newName === '' || newName === oldName) {
      cancelEdit();
      return;
    }
    if ($settings.layout_presets.some((p) => p.name === newName)) {
      editError = `A preset named “${newName}” already exists`;
      return;
    }
    try {
      await renameLayoutPreset(oldName, newName);
      cancelEdit();
    } catch (e) {
      editError = typeof e === 'string' ? e : JSON.stringify(e);
    }
  }

  function handleEditKey(e: KeyboardEvent): void {
    if (e.key === 'Enter') {
      e.preventDefault();
      void commitRename();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      cancelEdit();
    }
  }

  function handleRestore(p: LayoutPreset): void {
    restoreLayoutPreset(p.name);
    close();
  }

  function askDelete(p: LayoutPreset): void {
    confirmingDelete = p.name;
  }

  async function performDelete(): Promise<void> {
    if (confirmingDelete === null) return;
    const name = confirmingDelete;
    confirmingDelete = null;
    try {
      await deleteLayoutPreset(name);
    } catch (e) {
      console.error('delete_layout_preset failed', e);
    }
  }

  function cancelDelete(): void {
    confirmingDelete = null;
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (!isOpen) return;
    if (e.key === 'Escape') {
      // Let the inline edit/confirm handlers consume Escape first by
      // checking for those states.
      if (editingName !== null) {
        e.preventDefault();
        cancelEdit();
        return;
      }
      if (confirmingDelete !== null) {
        e.preventDefault();
        cancelDelete();
        return;
      }
      e.preventDefault();
      close();
    }
  }

  function formatTimestamp(p: LayoutPreset): string {
    return p.created_at.replace('T', ' ').replace('Z', ' UTC').slice(0, 19);
  }

  /// Svelte action: focus + select on mount. Equivalent to the
  /// `autofocus` attribute, but doesn't trigger the a11y warning
  /// (autofocus on initial page load is bad; on a deliberately-opened
  /// inline edit it's the expected interaction).
  function focusOnMount(node: HTMLInputElement): void {
    node.focus();
    node.select();
  }

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  });

  $effect(() => {
    if (!isOpen) {
      editingName = null;
      editValue = '';
      editError = null;
      confirmingDelete = null;
    }
  });
</script>

{#if isOpen}
  <div class="backdrop" onclick={close} role="presentation"></div>
  <div class="card" role="dialog" aria-label="Manage layout presets">
    <h2>Manage layout presets</h2>
    {#if presets.length === 0}
      <div class="empty">
        No layout presets saved yet. Use “Save current layout as…” from the
        Layouts menu to create one.
      </div>
    {:else}
      <ul class="list">
        {#each presets as preset (preset.name)}
          <li class="row">
            <div class="info">
              {#if editingName === preset.name}
                <input
                  type="text"
                  class="edit-input"
                  bind:value={editValue}
                  onkeydown={handleEditKey}
                  onblur={() => void commitRename()}
                  use:focusOnMount
                />
                {#if editError}
                  <div class="row-error">{editError}</div>
                {/if}
              {:else}
                <span class="name">{preset.name}</span>
                <span class="timestamp">{formatTimestamp(preset)}</span>
              {/if}
            </div>
            <div class="row-actions">
              {#if confirmingDelete === preset.name}
                <span class="confirm-label">Delete?</span>
                <button
                  type="button"
                  class="danger"
                  onclick={() => void performDelete()}
                >
                  Yes
                </button>
                <button
                  type="button"
                  class="cancel"
                  onclick={cancelDelete}
                >
                  No
                </button>
              {:else}
                <button
                  type="button"
                  class="action"
                  onclick={() => handleRestore(preset)}
                  disabled={editingName !== null}
                >
                  Restore
                </button>
                <button
                  type="button"
                  class="action"
                  onclick={() => startEdit(preset)}
                  disabled={editingName !== null}
                >
                  Rename
                </button>
                <button
                  type="button"
                  class="danger"
                  onclick={() => askDelete(preset)}
                  disabled={editingName !== null}
                >
                  Delete
                </button>
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    {/if}
    <div class="actions">
      <button type="button" class="primary" onclick={close}>Close</button>
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
    width: 520px;
    max-width: calc(100vw - 40px);
    max-height: calc(100vh - 80px);
    color: var(--text-primary);
    z-index: 101;
    box-shadow: var(--shadow-lg);
    display: flex;
    flex-direction: column;
  }
  h2 {
    margin: 0 0 var(--space-3);
    font-size: 16px;
    font-weight: 600;
  }
  .empty {
    color: var(--text-tertiary);
    font-style: italic;
    font-size: var(--font-size-md);
    padding: var(--space-5) 0;
    text-align: center;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    overflow-y: auto;
    flex: 1 1 auto;
    min-height: 0;
    max-height: 360px;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-2) var(--space-1);
    border-bottom: 1px solid var(--border-faint);
    gap: var(--space-3);
  }
  .row:last-child {
    border-bottom: none;
  }
  .info {
    flex: 1 1 auto;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .name {
    font-size: var(--font-size-md);
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .timestamp {
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
  }
  .edit-input {
    width: 100%;
    background: var(--surface-sunken);
    border: 1px solid var(--accent);
    border-radius: var(--radius-md);
    padding: var(--space-1) 6px;
    color: var(--text-primary);
    font-size: var(--font-size-md);
    box-sizing: border-box;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .edit-input:focus {
    outline: none;
  }
  .row-error {
    color: var(--danger);
    font-size: var(--font-size-xs);
  }
  .row-actions {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    flex: 0 0 auto;
  }
  .row-actions button {
    padding: var(--space-1) 10px;
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: var(--font-size-sm);
    border: 1px solid var(--border-default);
    background: var(--surface-4);
    color: var(--text-secondary);
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .row-actions button:hover:not([disabled]) {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  .row-actions button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .row-actions .danger {
    color: var(--danger);
    border-color: var(--border-danger);
  }
  .row-actions .danger:hover:not([disabled]) {
    background: var(--border-danger-soft);
  }
  button[disabled] {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .confirm-label {
    font-size: var(--font-size-sm);
    color: var(--text-warning);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    margin-top: var(--space-4);
  }
  .primary {
    padding: 6px var(--space-4);
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: var(--font-size-md);
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--accent-fg);
    font-weight: var(--font-weight-semibold);
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .primary:hover {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }
  .primary:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
