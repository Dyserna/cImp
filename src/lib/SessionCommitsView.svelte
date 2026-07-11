<script lang="ts">
  // The Workbench's "Session commits" section — commits whose committer time
  // falls inside one session's `started_ms..=last_ms` window, picked from the
  // Code Intelligence tab's Sessions card (the `sessionCommitsRequest`
  // store-bus in `workbench.ts`). Each commit expands to its full message
  // body plus a read-only per-file diff (`CheckpointDiffView`, the same
  // renderer the Timeline and Worktrees sections reuse). Commits caught live
  // from the session's transcript carry a "✓ agent" provenance chip.
  import { SvelteMap, SvelteSet } from 'svelte/reactivity';
  import { get } from 'svelte/store';
  import {
    sessionCommitsRequest,
    workbenchSessionCommits,
    workbenchCommitDiff,
    type CommitInfo,
    type FileDiff,
    type SessionCommitsRequest,
  } from './workbench';
  import CheckpointDiffView from './CheckpointDiffView.svelte';
  import RefChip from './RefChip.svelte';
  import { fmtDate, fmtTime } from './format';

  let commits = $state<CommitInfo[] | null>(null);
  let truncated = $state(false);
  let error = $state<string | null>(null);
  // Reactive collections: plain Set/Map in $state aren't proxied by Svelte 5,
  // so in-place mutation would never re-render (same reasoning as
  // CheckpointDiffView's SvelteSet). One map holds each commit's diff-load
  // outcome — success and error can never fall out of sync.
  type DiffState = { files: FileDiff[] } | { error: string };
  const expanded = new SvelteSet<string>();
  const diffs = new SvelteMap<string, DiffState>();

  // Refetch on every new button click (nonce), not on store identity — a
  // remount with the same pending request must also load, hence the local
  // "which nonce did I already load" latch instead of comparing objects.
  let loadedNonce = -1;
  $effect(() => {
    const req = $sessionCommitsRequest;
    if (!req || req.nonce === loadedNonce) return;
    loadedNonce = req.nonce;
    void load(req);
  });

  async function load(req: SessionCommitsRequest): Promise<void> {
    commits = null;
    truncated = false;
    error = null;
    expanded.clear();
    diffs.clear();
    try {
      const res = await workbenchSessionCommits(req.sessionId, req.fromMs, req.toMs);
      // Two quick clicks race their IPC calls: only the CURRENT request may
      // publish its result, or a slower stale response would overwrite the
      // newer session's list while the header shows the newer session.
      if (get(sessionCommitsRequest)?.nonce !== req.nonce) return;
      commits = res.commits;
      truncated = res.truncated;
    } catch (e) {
      if (get(sessionCommitsRequest)?.nonce !== req.nonce) return;
      error = String(e);
    }
  }

  async function toggle(c: CommitInfo): Promise<void> {
    if (expanded.has(c.hash)) {
      expanded.delete(c.hash);
      return;
    }
    expanded.add(c.hash);
    if (diffs.has(c.hash)) return;
    try {
      diffs.set(c.hash, { files: await workbenchCommitDiff(c.hash) });
    } catch (e) {
      diffs.set(c.hash, { error: String(e) });
    }
  }
</script>

<div class="session-commits">
  {#if !$sessionCommitsRequest}
    <p class="msg">
      No session picked. Open <strong>Code Intelligence → Overview → Sessions</strong>
      and click a session's <strong>commits</strong> button.
    </p>
  {:else}
    {@const req = $sessionCommitsRequest}
    <p class="scope">
      <span class="agent">{req.agent}</span> session ·
      {fmtDate(req.fromMs)} · {fmtTime(req.fromMs)} – {fmtTime(req.toMs)}
      {#if commits}
        · {commits.length} commit{commits.length === 1 ? '' : 's'}
      {/if}
    </p>

    {#if truncated}
      <p class="msg">
        The history walk hit its cap before reaching this session's start —
        older commits may be missing.
      </p>
    {/if}

    {#if error}
      <p class="msg err">Couldn't load commits: {error}</p>
    {:else if !commits}
      <p class="msg">Loading…</p>
    {:else if commits.length === 0}
      <p class="msg">No commits were made during this session.</p>
    {:else}
      <div class="commit-list">
        {#each commits as c (c.hash)}
          {@const diff = diffs.get(c.hash)}
          <div class="commit-row">
            <button
              type="button"
              class="commit-header"
              aria-expanded={expanded.has(c.hash)}
              onclick={() => void toggle(c)}
            >
              <span class="chevron" aria-hidden="true">{expanded.has(c.hash) ? '▾' : '▸'}</span>
              <span class="hash">{c.short}</span>
              {#if c.tracked}
                <span
                  class="tracked"
                  title="Caught live from this session's transcript — the agent ran this git commit"
                >✓ agent</span>
              {/if}
              <span class="subject">{c.subject}</span>
              {#each c.refs as r (r)}
                <RefChip {r} />
              {/each}
              <span class="meta">{c.author} · {fmtTime(c.ts_ms)}</span>
            </button>

            {#if expanded.has(c.hash)}
              <div class="commit-body">
                {#if c.body}
                  <pre class="message">{c.body}</pre>
                {/if}
                {#if !diff}
                  <p class="msg">Loading diff…</p>
                {:else if 'error' in diff}
                  <p class="msg err">Couldn't load this commit's diff: {diff.error}</p>
                {:else}
                  <CheckpointDiffView files={diff.files} />
                {/if}
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  {/if}
</div>

<style>
  .session-commits {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    font-size: var(--font-size-sm);
  }
  .msg {
    opacity: 0.7;
    font-style: italic;
    padding: var(--space-2) 0;
  }
  .msg.err {
    color: var(--text-danger-soft, #ffb4ab);
    font-style: normal;
  }
  .scope {
    margin: 0;
    opacity: 0.75;
  }
  .scope .agent {
    font-weight: var(--font-weight-semibold, 600);
  }
  .commit-list {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .commit-row {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    overflow: hidden;
  }
  .commit-header {
    appearance: none;
    width: 100%;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: var(--surface-2);
    border: none;
    color: var(--text-primary);
    padding: 6px var(--space-2);
    text-align: left;
    cursor: pointer;
    font-family: inherit;
    font-size: inherit;
  }
  .commit-header:hover {
    background: var(--surface-3);
  }
  .chevron {
    width: 1em;
    flex: 0 0 auto;
    opacity: 0.6;
  }
  .hash {
    flex: 0 0 auto;
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
  }
  .tracked {
    flex: 0 0 auto;
    font-size: var(--font-size-xs);
    color: var(--text-success, #9f9);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-sm);
    padding: 0 4px;
    white-space: nowrap;
    cursor: help;
  }
  .subject {
    flex: 0 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta {
    flex: 0 0 auto;
    margin-left: auto;
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
    white-space: nowrap;
  }
  .commit-body {
    background: var(--surface-sunken);
    padding: var(--space-2);
  }
  .message {
    margin: 0 0 var(--space-2);
    padding: var(--space-2);
    background: var(--surface-2);
    border-radius: var(--radius-sm);
    border: 1px solid var(--border-faint);
    white-space: pre-wrap;
    word-break: break-word;
    font-family: inherit;
    font-size: var(--font-size-xs);
    color: var(--text-secondary);
  }
</style>
