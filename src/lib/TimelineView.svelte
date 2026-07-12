<script lang="ts">
  // V13 Phase C — the Timeline section (`WorkbenchView`'s "Timeline" tab):
  // the checkpoint list with Diff-vs-now / Restore row actions. Only
  // rendered once `settings.workbench.checkpoints` is on (WorkbenchView's
  // banner explains the toggle otherwise) — this component assumes the
  // feature is enabled and just deals with fetching/rendering/acting on
  // whatever checkpoints already exist.
  import {
    workbenchCheckpoints,
    workbenchCheckpointDiff,
    workbenchCheckpointNow,
    workbenchCheckpointsVersion,
    FULL_FILE_CONTEXT,
    type Checkpoint,
    type FileDiff,
  } from './workbench';
  import { onMount } from 'svelte';
  import { openRestoreCheckpointDialog } from './dialog/store';
  import { errorMessage } from './errors';
  import CheckpointDiffView from './CheckpointDiffView.svelte';
  import { WORKBENCH_TAB_ID } from './tabs/types';
  import { onAppViewShown } from './appViewVisibility';
  import { loadViewString, saveViewString } from './viewSection';

  let checkpoints = $state<Checkpoint[]>([]);
  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let creatingNow = $state(false);

  // The open "Diff vs now" persists (viewSection.ts) like the sibling
  // sections' expansions — a stale id matches no row and renders nothing;
  // refresh() fetches the diff for a restored id (toggleDiff only fetches
  // on click).
  let openDiffFor = $state<string | null>(loadViewString('timeline', 'open-diff'));
  $effect(() => saveViewString('timeline', 'open-diff', openDiffFor));
  let diffFiles = $state<Map<string, FileDiff[]>>(new Map());
  let diffErrors = $state<Map<string, string>>(new Map());
  let diffLoading = $state<Set<string>>(new Set());

  async function refresh(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      // Newest first — the shadow module returns oldest-first (matches
      // `git log`'s natural iteration order for the backing commits).
      checkpoints = (await workbenchCheckpoints()).slice().reverse();
      if (openDiffFor !== null && checkpoints.some((c) => c.id === openDiffFor)) {
        void loadDiffFor(openDiffFor);
      }
    } catch (e) {
      loadError = errorMessage(e);
    } finally {
      loading = false;
    }
  }

  // Refetch after a restore (or a future "checkpoint now" from elsewhere)
  // bumps the shared version store — see `workbench.ts`'s doc comment. The
  // effect's initial run also covers the first load on mount (no separate
  // `onMount` fetch — that would fire a duplicate `workbench_checkpoints`
  // call every time the view mounts).
  $effect(() => {
    $workbenchCheckpointsVersion;
    void refresh();
  });

  // Keep-alive (appViews.ts): auto-checkpoints that landed while the tab was
  // off-screen don't bump the version store — refetch when the tab returns
  // (the pre-keep-alive remount used to cover this).
  onMount(() => onAppViewShown(WORKBENCH_TAB_ID, () => void refresh()));

  async function checkpointNow(): Promise<void> {
    creatingNow = true;
    try {
      await workbenchCheckpointNow();
      await refresh();
    } catch (e) {
      loadError = errorMessage(e);
    } finally {
      creatingNow = false;
    }
  }

  async function toggleDiff(id: string): Promise<void> {
    if (openDiffFor === id) {
      openDiffFor = null;
      return;
    }
    openDiffFor = id;
    await loadDiffFor(id);
  }

  async function loadDiffFor(id: string): Promise<void> {
    if (diffFiles.has(id) || diffLoading.has(id)) return;
    diffLoading.add(id);
    diffLoading = new Set(diffLoading);
    try {
      const files = await workbenchCheckpointDiff(id);
      diffFiles.set(id, files);
      diffFiles = new Map(diffFiles);
      diffErrors.delete(id);
    } catch (e) {
      diffErrors.set(id, errorMessage(e));
      diffErrors = new Map(diffErrors);
    } finally {
      diffLoading.delete(id);
      diffLoading = new Set(diffLoading);
    }
  }

  function triggerIcon(trigger: Checkpoint['trigger']): string {
    switch (trigger) {
      case 'prompt': return '💬';
      case 'burst': return '⚡';
      case 'manual': return '📌';
      case 'pre-restore': return '⏮';
    }
  }

  function triggerTitle(trigger: Checkpoint['trigger']): string {
    switch (trigger) {
      case 'prompt': return 'Automatic — fired by a prompt';
      case 'burst': return 'Automatic — fired by a burst of file activity';
      case 'manual': return 'Manual — "Checkpoint now"';
      case 'pre-restore': return 'Automatic safety net taken right before a restore';
    }
  }

  function formatTime(iso: string): string {
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
  }
</script>

<div class="timeline">
  <div class="toolbar">
    <button type="button" class="checkpoint-now" onclick={checkpointNow} disabled={creatingNow}>
      {creatingNow ? 'Checkpointing…' : 'Checkpoint now'}
    </button>
    <button type="button" class="refresh" onclick={refresh} disabled={loading}>Refresh</button>
  </div>

  {#if loadError}
    <p class="msg err">Couldn't load checkpoints: {loadError}</p>
  {:else if loading && checkpoints.length === 0}
    <p class="msg">Loading…</p>
  {:else if checkpoints.length === 0}
    <p class="msg">
      No checkpoints yet. They're created automatically (per prompt, or after a
      burst of file activity) or on demand with "Checkpoint now" above.
    </p>
  {:else}
    <div class="rows">
      {#each checkpoints as cp (cp.id)}
        <div class="row">
          <div class="row-main">
            <span class="trigger" title={triggerTitle(cp.trigger)}>{triggerIcon(cp.trigger)}</span>
            <span class="time">{formatTime(cp.ts)}</span>
            <span class="label" title={cp.label}>{cp.label}</span>
            <span class="files">{cp.files_changed} file{cp.files_changed === 1 ? '' : 's'}</span>
            <span class="agent">{cp.agent ?? '—'}</span>
            <span class="actions">
              <button type="button" onclick={() => void toggleDiff(cp.id)}>
                {openDiffFor === cp.id ? 'Hide diff' : 'Diff vs now'}
              </button>
              <button type="button" class="restore" onclick={() => openRestoreCheckpointDialog(cp.id)}>
                Restore
              </button>
            </span>
          </div>
          {#if openDiffFor === cp.id}
            <div class="row-diff">
              {#if diffLoading.has(cp.id)}
                <p class="msg">Loading diff…</p>
              {:else if diffErrors.get(cp.id)}
                <p class="msg err">{diffErrors.get(cp.id)}</p>
              {:else}
                <CheckpointDiffView
                  files={diffFiles.get(cp.id) ?? []}
                  fetchFull={() => workbenchCheckpointDiff(cp.id, FULL_FILE_CONTEXT)}
                  stateKey={`timeline:${cp.id}`}
                />
              {/if}
            </div>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .timeline {
    display: flex;
    flex-direction: column;
    gap: var(--space-2, 8px);
    font-size: var(--font-size-sm, 13px);
  }
  .toolbar {
    display: flex;
    gap: 8px;
  }
  .toolbar button {
    appearance: none;
    background: var(--surface-3, #2a2a2a);
    border: 1px solid var(--border-subtle, #444);
    color: var(--text-primary, #ddd);
    border-radius: var(--radius-sm, 4px);
    padding: 4px 10px;
    font-size: var(--font-size-xs, 11px);
    cursor: pointer;
  }
  .toolbar button:hover:not(:disabled) {
    background: var(--surface-4, #333);
  }
  .toolbar button:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .toolbar .checkpoint-now {
    border-color: var(--accent, #3b6ea5);
    color: var(--accent, #3b6ea5);
  }
  .msg {
    opacity: 0.7;
    font-style: italic;
    padding: var(--space-2, 8px) 0;
  }
  .msg.err {
    color: var(--text-danger-soft, #ff8a80);
    font-style: normal;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .row {
    border: 1px solid var(--border-subtle, #444);
    border-radius: var(--radius-md, 6px);
    overflow: hidden;
  }
  .row-main {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 10px;
    background: var(--surface-2, #232323);
    flex-wrap: wrap;
  }
  .trigger {
    flex: 0 0 auto;
  }
  .time {
    flex: 0 0 auto;
    color: var(--text-tertiary, #999);
    font-size: var(--font-size-xs, 11px);
    white-space: nowrap;
  }
  .label {
    flex: 1 1 200px;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .files {
    flex: 0 0 auto;
    color: var(--text-secondary, #bbb);
    font-size: var(--font-size-xs, 11px);
    font-variant-numeric: tabular-nums;
  }
  .agent {
    flex: 0 0 auto;
    color: var(--text-tertiary, #999);
    font-size: var(--font-size-xs, 11px);
    min-width: 5ch;
  }
  .actions {
    flex: 0 0 auto;
    display: inline-flex;
    gap: 4px;
  }
  .actions button {
    appearance: none;
    background: transparent;
    border: 1px solid var(--border-subtle, #444);
    color: var(--text-secondary, #bbb);
    border-radius: var(--radius-sm, 4px);
    padding: 2px 8px;
    font-size: var(--font-size-xs, 11px);
    cursor: pointer;
  }
  .actions button:hover {
    background: var(--surface-3, #2a2a2a);
    color: var(--text-primary, #ddd);
  }
  .actions button.restore {
    border-color: var(--border-danger, #a33);
    color: var(--text-danger-soft, #ff8a80);
  }
  .row-diff {
    padding: 8px 10px;
    background: var(--surface-sunken, #1a1a1a);
    border-top: 1px solid var(--border-faint, #333);
  }
</style>
