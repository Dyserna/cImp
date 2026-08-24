<script lang="ts">
  // Manage Presets dialog: scrollable list of every preset with
  // Restore / Rename (inline) / Delete (confirm) per row. Renders
  // reactively from the live `settings.layout_presets` array — every
  // CRUD action updates the array via the settings broadcast, no
  // local refresh needed.
  import { closeDialog, dialogState } from './store';
  import ModalShell from './ModalShell.svelte';
  import { settings } from '../settings/store';
  import {
    deleteLayoutPreset,
    renameLayoutPreset,
    restoreLayoutPreset,
  } from '../layout/presets';
  import type { LayoutPreset } from '../settings/types';
  import { errorMessage } from '../errors';

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
      editError = errorMessage(e);
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
    // Fire-and-forget: the restore is an IPC round-trip since V42 Phase B, and
    // holding the dialog open until it lands would only show a frozen list. A
    // failure leaves the live layout untouched (see `presets.ts`).
    void restoreLayoutPreset(p.name);
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

  $effect(() => {
    if (!isOpen) {
      editingName = null;
      editValue = '';
      editError = null;
      confirmingDelete = null;
    }
  });
</script>

<ModalShell
  open={isOpen}
  label="Manage layout presets"
  title="Manage layout presets"
  width={520}
  fit="column"
  titleGap="md"
  onCancel={close}
  onEscape={() => {
    // The inline rename and the delete confirm consume Escape first:
    // closing the dialog out from under an open editor would be a
    // surprise. This is now the only Escape handler in the file, so the
    // ordering is a branch rather than two listeners racing.
    if (editingName !== null) {
      cancelEdit();
      return;
    }
    if (confirmingDelete !== null) {
      cancelDelete();
      return;
    }
    close();
  }}
>
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
  {#snippet actions()}
    <button type="button" class="primary" onclick={close}>Close</button>
  {/snippet}
</ModalShell>

<style>
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
  /* #128: this file used to re-declare the whole button (padding, radius,
     border, transition) inside `.primary`, because it was the only dialog with
     no `.actions button` base. `ModalShell` supplies that base now, so
     `.primary` is the same four-line modifier the other six dialogs carry and
     the focus ring comes from the shell. One consequence, deliberately
     accepted: `.actions button` is (0,2,1) and `.primary` is (0,2,0), so the
     shorthand `border` wins over `border-color` and this button's 1px ring is
     `--border-default` rather than `--accent` — which is exactly what the
     other six primaries already looked like. */
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
</style>
