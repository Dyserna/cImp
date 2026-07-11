<script lang="ts">
  // The Workbench's "Git graph" section — the classic branching/merging
  // commit graph (gitk/git log --graph style): one row per commit, colored
  // rails for branches, curves where they fork and merge, ref chips for
  // branch/tag tips. Read-only; data is `workbench_git_graph` (every ref,
  // topological order, so children always render above their parents).
  import { onMount } from 'svelte';
  import { workbenchGitGraph, type CommitInfo, type GitGraph } from './workbench';
  import RefChip from './RefChip.svelte';
  import { fmtDate, fmtTime } from './format';

  let graph = $state<GitGraph | null>(null);
  let error = $state<string | null>(null);
  let loading = $state(false);

  async function refresh(): Promise<void> {
    loading = true;
    try {
      graph = await workbenchGitGraph();
      error = null;
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    void refresh();
  });

  // ── Lane layout ──────────────────────────────────────────────────────
  // The standard railway algorithm over a topologically-ordered list:
  // each lane holds the commit hash it expects next. A commit lands on the
  // first lane expecting it (extra expecting lanes merge in), a tip opens
  // the first free lane, the first parent continues the commit's own lane,
  // and every further parent either joins a lane that already expects it
  // (a fork point) or opens a new one.
  interface Edge {
    from: number;
    to: number;
    color: number;
  }
  interface Row {
    commit: CommitInfo;
    lane: number;
    color: number;
    /// Rail segments from the row's top edge to its vertical center.
    top: Edge[];
    /// Rail segments from the center to the row's bottom edge.
    bottom: Edge[];
  }

  function layout(commits: CommitInfo[]): { rows: Row[]; laneCount: number } {
    const lanes: ({ expect: string; color: number } | null)[] = [];
    let nextColor = 0;
    let maxLanes = 0;
    const rows: Row[] = [];
    const firstFree = (): number => {
      const i = lanes.findIndex((l) => l === null);
      return i === -1 ? lanes.length : i;
    };

    for (const c of commits) {
      const incoming: number[] = [];
      lanes.forEach((l, i) => {
        if (l && l.expect === c.hash) incoming.push(i);
      });
      let lane: number;
      let color: number;
      if (incoming.length > 0) {
        lane = incoming[0];
        color = lanes[lane]!.color;
      } else {
        lane = firstFree();
        color = nextColor++;
      }

      const top: Edge[] = [];
      lanes.forEach((l, i) => {
        if (!l) return;
        top.push({ from: i, to: l.expect === c.hash ? lane : i, color: l.color });
      });
      for (const i of incoming) lanes[i] = null;

      const bottom: Edge[] = [];
      lanes.forEach((l, i) => {
        if (l) bottom.push({ from: i, to: i, color: l.color });
      });
      c.parents.forEach((p, k) => {
        const existing = lanes.findIndex((l) => l !== null && l.expect === p);
        if (existing !== -1) {
          bottom.push({ from: lane, to: existing, color: lanes[existing]!.color });
          return;
        }
        let target: number;
        let edgeColor: number;
        if (k === 0 && lanes[lane] == null) {
          target = lane;
          edgeColor = color;
        } else {
          target = firstFree();
          edgeColor = nextColor++;
        }
        while (lanes.length <= target) lanes.push(null);
        lanes[target] = { expect: p, color: edgeColor };
        bottom.push({ from: lane, to: target, color: edgeColor });
      });

      maxLanes = Math.max(maxLanes, lanes.length);
      rows.push({ commit: c, lane, color, top, bottom });
      while (lanes.length > 0 && lanes[lanes.length - 1] === null) lanes.pop();
    }
    return { rows, laneCount: Math.max(maxLanes, 1) };
  }

  const laid = $derived(graph ? layout(graph.commits) : null);

  // ── SVG geometry ─────────────────────────────────────────────────────
  // ONE svg spans the whole rails column (a per-row svg would create
  // hundreds of separate SVG documents for the browser to lay out); rows
  // reserve its width with padding and the rails paint behind them, so row
  // hover/zebra striping still covers the full row width. The column is as
  // wide as the graph really is — the .rows container scrolls horizontally
  // if a many-branch repo needs it.
  const COL_W = 12;
  const ROW_H = 26;
  const DOT_R = 3.5;
  const PALETTE = [
    '#61afef', '#98c379', '#e06c75', '#c678dd', '#d19a66',
    '#56b6c2', '#e5c07b', '#528bff', '#be5046', '#7f848e',
  ];
  const x = (lane: number): number => 6 + lane * COL_W;
  const graphWidth = $derived(laid ? x(laid.laneCount - 1) + 6 : COL_W);

  function edgePath(e: Edge, half: 'top' | 'bottom'): string {
    const [y0, y1] = half === 'top' ? [0, ROW_H / 2] : [ROW_H / 2, ROW_H];
    const x0 = x(e.from);
    const x1 = x(e.to);
    if (x0 === x1) return `M ${x0} ${y0} L ${x1} ${y1}`;
    const ym = (y0 + y1) / 2;
    return `M ${x0} ${y0} C ${x0} ${ym}, ${x1} ${ym}, ${x1} ${y1}`;
  }
</script>

<div class="git-graph">
  <div class="toolbar">
    {#if graph}
      <span class="head-line">
        {#if graph.head}On branch <strong>{graph.head}</strong>{:else}Detached HEAD{/if}
        · {graph.commits.length} commit{graph.commits.length === 1 ? '' : 's'}
        {#if graph.truncated}<span class="trunc">(showing the most recent — history truncated)</span>{/if}
      </span>
    {/if}
    <button type="button" class="refresh" onclick={() => void refresh()} disabled={loading}>
      {loading ? 'Refreshing…' : 'Refresh'}
    </button>
  </div>

  {#if error}
    <p class="msg err">Couldn't load the commit graph: {error}</p>
  {:else if !graph || !laid}
    <p class="msg">Loading…</p>
  {:else if graph.commits.length === 0}
    <p class="msg">No commits yet.</p>
  {:else}
    <div class="rows">
      <svg
        class="rails"
        width={graphWidth}
        height={laid.rows.length * ROW_H}
        viewBox={`0 0 ${graphWidth} ${laid.rows.length * ROW_H}`}
        aria-hidden="true"
      >
        {#each laid.rows as row, i (row.commit.hash)}
          <g transform={`translate(0, ${i * ROW_H})`}>
            {#each row.top as e, j (j)}
              <path d={edgePath(e, 'top')} stroke={PALETTE[e.color % PALETTE.length]} />
            {/each}
            {#each row.bottom as e, j (j)}
              <path d={edgePath(e, 'bottom')} stroke={PALETTE[e.color % PALETTE.length]} />
            {/each}
            <circle
              cx={x(row.lane)}
              cy={ROW_H / 2}
              r={DOT_R}
              fill={PALETTE[row.color % PALETTE.length]}
            />
          </g>
        {/each}
      </svg>
      {#each laid.rows as row (row.commit.hash)}
        <div class="row" style={`padding-left: ${graphWidth + 8}px; height: ${ROW_H}px`}>
          <span class="hash">{row.commit.short}</span>
          {#each row.commit.refs as r (r)}
            <RefChip {r} />
          {/each}
          <span class="subject" title={row.commit.subject}>{row.commit.subject}</span>
          <span class="meta">
            {row.commit.author} · {fmtDate(row.commit.ts_ms)} {fmtTime(row.commit.ts_ms)}
          </span>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .git-graph {
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
  .toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .head-line {
    opacity: 0.8;
  }
  .trunc {
    opacity: 0.6;
    font-size: var(--font-size-xs);
  }
  .refresh {
    padding: 3px 10px;
    border-radius: var(--radius-sm, 5px);
    border: 1px solid var(--border-subtle, #444);
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: var(--font-size-xs, 11px);
    cursor: pointer;
  }
  .refresh:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .rows {
    position: relative;
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    overflow-x: auto;
  }
  .rails {
    position: absolute;
    top: 0;
    left: 0;
    display: block;
    pointer-events: none;
  }
  .rails path {
    fill: none;
    stroke-width: 1.5;
  }
  .row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding-right: var(--space-2);
    min-width: 0;
    box-sizing: border-box;
  }
  .row:nth-child(even) {
    background: rgba(255, 255, 255, 0.02);
  }
  .row:hover {
    background: var(--surface-2);
  }
  .hash {
    flex: 0 0 auto;
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
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
</style>
