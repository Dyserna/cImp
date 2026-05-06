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
    background: #2a2a2a;
    border: 1px solid #444;
    border-radius: 6px;
    padding: 20px 24px;
    width: 520px;
    max-width: calc(100vw - 40px);
    max-height: calc(100vh - 80px);
    color: #e0e0e0;
    z-index: 101;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.6);
    display: flex;
    flex-direction: column;
  }
  h2 {
    margin: 0 0 12px;
    font-size: 16px;
    font-weight: 600;
  }
  .empty {
    color: #888;
    font-style: italic;
    font-size: 13px;
    padding: 24px 0;
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
    padding: 8px 4px;
    border-bottom: 1px solid #333;
    gap: 12px;
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
    font-size: 13px;
    color: #e0e0e0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .timestamp {
    font-size: 11px;
    color: #888;
  }
  .edit-input {
    width: 100%;
    background: #1a1a1a;
    border: 1px solid #4a90e2;
    border-radius: 3px;
    padding: 4px 6px;
    color: #e0e0e0;
    font-size: 13px;
    box-sizing: border-box;
  }
  .edit-input:focus {
    outline: none;
  }
  .row-error {
    color: #e74c3c;
    font-size: 11px;
  }
  .row-actions {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    flex: 0 0 auto;
  }
  .row-actions button {
    padding: 4px 10px;
    border-radius: 3px;
    cursor: pointer;
    font-size: 12px;
    border: 1px solid #444;
    background: #2a2a2a;
    color: #c0c0c0;
  }
  .row-actions button:hover:not([disabled]) {
    background: #383838;
  }
  .row-actions .danger {
    color: #e74c3c;
    border-color: #5a3030;
  }
  .row-actions .danger:hover:not([disabled]) {
    background: #4a2828;
  }
  button[disabled] {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .confirm-label {
    font-size: 12px;
    color: #f0c060;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    margin-top: 16px;
  }
  .primary {
    padding: 6px 16px;
    border-radius: 3px;
    cursor: pointer;
    font-size: 13px;
    border: 1px solid #4a90e2;
    background: #4a90e2;
    color: white;
  }
  .primary:hover {
    background: #5aa0f2;
  }
</style>
