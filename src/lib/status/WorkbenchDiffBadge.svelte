<script lang="ts">
  // V13 Phase B5: the `±N` status-bar chip. Mounted unconditionally in
  // StatusBar.svelte (like NoteButton) — for the app's practical lifetime,
  // making it the natural owner of the shared `watchWorkbenchDiff()`
  // listener (see workbenchDiff.ts's doc comment). Hidden when the feature
  // is off or there are zero changed files; click focuses/opens the
  // Workbench tab, same pattern as NoteButton's `openNoteTab`.
  import { onDestroy } from 'svelte';
  import { settings } from '../settings/store';
  import { workbenchDiff, watchWorkbenchDiff } from '../workbenchDiff';
  import { revealTab } from '../tabs/visibility';
  import { WORKBENCH_TAB_ID } from '../tabs/types';

  let release: (() => void) | null = null;

  // Only run the listener/poll while the feature is on — flipping it off
  // mid-session tears the watcher down rather than leaving it running for a
  // badge that can never show.
  $effect(() => {
    if ($settings.workbench.enabled) {
      release ??= watchWorkbenchDiff();
    } else {
      release?.();
      release = null;
    }
  });

  onDestroy(() => {
    release?.();
    release = null;
  });

  const count = $derived($workbenchDiff?.files.length ?? 0);
  const visible = $derived($settings.workbench.enabled && count > 0);

  // revealTab (not a bare activate) so a UI-hidden Workbench tab is
  // re-inserted into the layout — hidden tabs live outside the tree, so
  // setFocusedPaneActiveTab alone would silently no-op.
  function open(): void {
    revealTab(WORKBENCH_TAB_ID);
  }
</script>

{#if visible}
  <button
    type="button"
    class="status-button wb-badge"
    onclick={open}
    title="{count} file{count === 1 ? '' : 's'} changed — open Workbench"
    aria-label="{count} files changed in the working tree — open the Workbench tab"
  >
    ±{count}
  </button>
{/if}

<style>
  .status-button {
    appearance: none;
    background: transparent;
    border: 1px solid transparent;
    color: var(--text-secondary);
    cursor: pointer;
    height: 22px;
    padding: 0 8px;
    border-radius: var(--radius-pill);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .status-button:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
