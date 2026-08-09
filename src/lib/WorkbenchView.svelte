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
  import { workbenchStatus, sessionCommitsRequest, takeSessionCommitsRoute, type WorkbenchStatus } from './workbench';
  import { settings } from './settings/store';
  import { WORKBENCH_TAB_ID } from './tabs/types';
  import { onAppViewShown } from './appViewVisibility';
  import { loadViewSection, saveViewSection } from './viewSection';
  import DiffView from './DiffView.svelte';
  import TimelineView from './TimelineView.svelte';
  import WorktreesView from './WorktreesView.svelte';
  import SessionCommitsView from './SessionCommitsView.svelte';
  import GitGraphView from './GitGraphView.svelte';
  import { latchByTab } from './latch';
  import { evidenceOffNotice } from './timeline';

  type Section = 'diff' | 'timeline' | 'worktrees' | 'session-commits' | 'git-graph';
  const SECTIONS: { id: Section; label: string }[] = [
    { id: 'git-graph', label: 'Git graph' },
    { id: 'diff', label: 'Diff' },
    { id: 'session-commits', label: 'Session commits' },
    { id: 'timeline', label: 'Timeline' },
    { id: 'worktrees', label: 'Worktrees' },
  ];
  // The selection survives the component's destroy/recreate cycle (tab
  // switch, hide/un-hide) and app restarts — see viewSection.ts.
  let section = $state<Section>(
    loadViewSection('workbench', SECTIONS.map((s) => s.id), 'diff'),
  );
  $effect(() => saveViewSection('workbench', section));

  // A Sessions-card "commits" click (Code Intelligence tab) reveals this tab
  // AND must land on the Session-commits section — switch on every new
  // request (nonce), while plain section clicks stay untouched. The latch
  // lives in workbench.ts MODULE scope (takeSessionCommitsRoute): this
  // component is destroyed/recreated on tab switches, so component-local
  // state would reset and replay the store's last request on every remount.
  $effect(() => {
    const req = $sessionCommitsRequest;
    if (req && takeSessionCommitsRoute(req.nonce)) section = 'session-commits';
  });

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
    // Keep-alive (appViews.ts): the component is no longer remounted on
    // re-activation, so re-check git availability whenever the tab returns.
    return onAppViewShown(WORKBENCH_TAB_ID, () => void refreshStatus());
  });

  // Diff and Worktrees both need a real git repo; Timeline needs
  // checkpoints on (Phase C) — a non-git project can still use checkpoints
  // (the shadow repo is self-contained), so Timeline's gate is independent
  // of `needsGit`/`gitBannerText` below.
  const needsGit = $derived(
    section === 'diff' || section === 'worktrees' || section === 'session-commits' || section === 'git-graph',
  );
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

  // V33 step 5: with checkpoints off there is no Timeline, and therefore no
  // contamination evidence surface. That is a configuration the user is allowed
  // to choose, so the off-state banner has to say what is missing AND name a
  // control that still works — a silent Timeline reads as "nothing is wrong",
  // which is exactly the claim this feature exists to stop making.
  const contaminatedScopes = $derived(
    Object.values($latchByTab)
      .filter((r) => r?.contaminated)
      .map((r) => `${r!.consumer}:${r!.tab}`),
  );
  const evidenceOff = $derived(evidenceOffNotice(contaminatedScopes));
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
    <p class="banner" class:warn={contaminatedScopes.length > 0}>{evidenceOff}</p>
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
    {#if statusError || (needsGit && gitBannerText)}
      <!-- The banner above already explains what's missing. -->
    {:else}
      <WorktreesView />
    {/if}
  {:else if section === 'session-commits'}
    {#if statusError || (needsGit && gitBannerText)}
      <!-- The banner above already explains what's missing. -->
    {:else}
      <SessionCommitsView />
    {/if}
  {:else if section === 'git-graph'}
    {#if statusError || (needsGit && gitBannerText)}
      <!-- The banner above already explains what's missing. -->
    {:else}
      <GitGraphView />
    {/if}
  {/if}
</div>

<style>
  .workbench {
    /* Sit ABOVE the pane's absolutely-positioned (empty) terminal slot —
       same convention as CodeIntelligenceView, otherwise
       that transparent slot paints on top of this static content and
       swallows every button click. */
    position: absolute;
    inset: 0;
    overflow-y: auto;
    padding: 16px;
    font-size: 13px;
    color: var(--text-primary, #ddd);
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
    border-bottom: 1px solid var(--border-subtle, #333);
    padding-bottom: 8px;
    flex-wrap: wrap;
  }
  .seg {
    padding: 4px 12px;
    border-radius: 6px;
    border: 1px solid transparent;
    background: transparent;
    color: var(--text-primary, #ddd);
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
    color: var(--accent-fg, #fff);
    opacity: 1;
    border-color: var(--accent, #3b6ea5);
  }
  .banner {
    margin: 0 0 14px;
    padding: 8px 10px;
    border-radius: 6px;
    font-size: 12px;
    border: 1px solid var(--border-default, #444);
    background: rgba(255, 255, 255, 0.04);
    opacity: 0.9;
  }
  .banner.err {
    background: var(--surface-danger, rgba(179, 38, 30, 0.18));
    border-color: var(--border-danger, #b3261e);
    color: var(--text-danger-soft, #ffb4ab);
  }
  /* Step 5: a live contamination the Timeline cannot show is not a neutral
     configuration note. */
  .banner.warn {
    border-color: var(--awaiting, #d0a24c);
    color: var(--awaiting, #d0a24c);
    opacity: 1;
  }
</style>
