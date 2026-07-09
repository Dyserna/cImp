<script lang="ts">
  // V13 Phase A: the app-rendered (no PTY) Workbench tab — hosts the live
  // diff pane (Phase B), the checkpoint timeline (Phase C),
  // and the worktree manager (Phase D) as sections of one reserved tab,
  // mirroring the Code Intelligence tab's segmented-section pattern
  // (`CodeIntelligenceView.svelte`). Rendered by `Pane.svelte` for the
  // reserved `workbench-1` tab id.
  //
  // Phase A ships the shell only: section routing + a top-of-view banner
  // explaining what each section needs (git, or checkpoints once Phase C
  // lands). The sections themselves are filled in by B/C/D.
  import { onMount } from 'svelte';
  import { workbenchStatus, type WorkbenchStatus } from './workbench';
  import { settings } from './settings/store';
  import DiffView from './DiffView.svelte';
  import TimelineView from './TimelineView.svelte';

  type Section = 'diff' | 'timeline' | 'worktrees';
  const SECTIONS: { id: Section; label: string }[] = [
    { id: 'diff', label: 'Diff' },
    { id: 'timeline', label: 'Timeline' },
    { id: 'worktrees', label: 'Worktrees' },
  ];
  let section = $state<Section>('diff');

  let status = $state<WorkbenchStatus | null>(null);
  let statusError = $state<string | null>(null);

  async function refreshStatus(): Promise<void> {
    try {
      status = await workbenchStatus();
      statusError = null;
    } catch (e) {
      statusError = String(e);
    }
  }

  onMount(() => {
    void refreshStatus();
  });

  // Diff and Worktrees both need a real git repo; Timeline needs
  // checkpoints on (Phase C) — a non-git project can still use checkpoints
  // (the shadow repo is self-contained), so Timeline's gate is independent
  // of `needsGit`/`gitBannerText` below.
  const needsGit = $derived(section === 'diff' || section === 'worktrees');
  const gitBannerText = $derived.by(() => {
    if (!status) return null;
    if (!status.git_available) {
      return "git wasn't found on PATH. Install git to use the diff pane and worktrees.";
    }
    if (!status.is_repo) {
      return 'This project is not a git repository yet — run `git init`, or turn on checkpoints (Settings → Workbench).';
    }
    return null;
  });

  const checkpointsOff = $derived(section === 'timeline' && !$settings.workbench.checkpoints);
</script>

<div class="workbench">
  <header>
    <h2>Workbench</h2>
  </header>

  <nav class="sections">
    {#each SECTIONS as s (s.id)}
      <button
        type="button"
        class="seg"
        class:active={section === s.id}
        onclick={() => (section = s.id)}
      >{s.label}</button>
    {/each}
  </nav>

  {#if statusError}
    <p class="banner err">Couldn't check git status: {statusError}</p>
  {:else if needsGit && gitBannerText}
    <p class="banner">{gitBannerText}</p>
  {:else if checkpointsOff}
    <p class="banner">
      Checkpoints are off. Turn on "Enable automatic checkpoints" in Settings
      → Workbench to start building a timeline — checkpoints use a separate
      shadow git repo (<code>.cimp/shadow.git</code>); your own
      <code>.git</code> is never touched.
    </p>
  {/if}

  {#if section === 'diff'}
    {#if statusError || (needsGit && gitBannerText)}
      <!-- The banner above already explains what's missing (git or the
           launch-dir root) — nothing more to render until that's resolved. -->
    {:else}
      <DiffView />
    {/if}
  {:else if section === 'timeline'}
    {#if !checkpointsOff}
      <TimelineView />
    {/if}
  {:else if section === 'worktrees'}
    <p class="placeholder">
      Worktree manager — coming in Phase D. Once shipped, this section lets
      you spin up an isolated worktree + branch for a parallel agent task,
      then merge or discard it from here.
    </p>
  {/if}
</div>

<style>
  .workbench {
    /* Sit ABOVE the pane's absolutely-positioned (empty) terminal slot —
       same convention as CodeIntelligenceView/OffloadServerView, otherwise
       that transparent slot paints on top of this static content and
       swallows every button click. */
    position: absolute;
    inset: 0;
    overflow-y: auto;
    padding: 16px;
    font-size: 13px;
    color: var(--text, #ddd);
    box-sizing: border-box;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 14px;
  }
  header h2 {
    margin: 0;
    font-size: 15px;
  }
  nav.sections {
    display: flex;
    gap: 4px;
    margin-bottom: 14px;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 8px;
    flex-wrap: wrap;
  }
  .seg {
    padding: 4px 12px;
    border-radius: 6px;
    border: 1px solid transparent;
    background: transparent;
    color: var(--text, #ddd);
    font-size: 12px;
    cursor: pointer;
    opacity: 0.7;
  }
  .seg:hover {
    background: rgba(255, 255, 255, 0.06);
    opacity: 1;
  }
  .seg.active {
    background: var(--accent, #3b6ea5);
    color: #fff;
    opacity: 1;
    border-color: var(--accent, #3b6ea5);
  }
  .banner {
    margin: 0 0 14px;
    padding: 8px 10px;
    border-radius: 6px;
    font-size: 12px;
    border: 1px solid var(--border, #444);
    background: rgba(255, 255, 255, 0.04);
    opacity: 0.9;
  }
  .banner.err {
    background: rgba(179, 38, 30, 0.18);
    border-color: #b3261e;
    color: #ffb4ab;
  }
  .placeholder {
    opacity: 0.65;
    font-style: italic;
    line-height: 1.5;
    max-width: 60ch;
  }
</style>
