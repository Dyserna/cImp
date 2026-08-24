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
    class="status-button status-badge wb-badge"
    onclick={open}
    title="{count} file{count === 1 ? '' : 's'} changed — open Workbench"
    aria-label="{count} files changed in the working tree — open the Workbench tab"
  >
    ±{count}
  </button>
{/if}

<style>
  /* Shell + focus ring: `.status-button.status-badge` in `src/app.css`. Local
     delta: tabular numerals so the pill does not jitter as the count changes. */
  .status-button {
    font-variant-numeric: tabular-nums;
  }
  .status-button:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
</style>
