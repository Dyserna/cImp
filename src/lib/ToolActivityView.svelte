<script lang="ts">
  // The read-only, app-rendered Tool Activity tab — one place to see what
  // tools the agents are using and which tools are available. Three sections:
  // Activities (a unified, newest-first feed merging the code-intelligence
  // graph-call history with the offload backends' request history), Graph
  // tools, and Offload tools (the two reference lists, moved here from the
  // Code Intelligence and Offload Server tabs). Same reserved/no-PTY pattern
  // as CodeIntelligenceView; rendered by Pane.svelte for the `tool-activity`
  // tab id.
  import { onMount, onDestroy } from 'svelte';
  import { graphHistory, type GraphCall } from './graph';
  import {
    offloadServerMetrics,
    onOffloadServerMetrics,
    type BackendDashboard,
  } from './offload';
  import { listenManaged } from './listenManaged';
  import { settings } from './settings/store';
  import { fmtTime } from './format';
  import { fmtTok } from './usageMath';
  import ToolsReference from './ToolsReference.svelte';

  // Reference list of the graph_* MCP tools the code graph exposes to Claude
  // (and the offload worker) while the graph is enabled. Mirrors the
  // descriptions in `src-tauri/src/graph/mcp.rs::tool_specs`; kept here as
  // static docs (moved from CodeIntelligenceView).
  const GRAPH_TOOLS = [
    { name: 'graph_find_symbol', desc: 'Where a symbol (function/struct/trait/…) is defined — file, line, signature.', example: 'Where is GraphService defined?' },
    { name: 'graph_callers', desc: 'Which functions call the given symbol (its call sites). Impact analysis.', example: 'What calls graphRebuild?' },
    { name: 'graph_callees', desc: 'Which symbols are called by the given symbol.', example: 'What does handle_call call?' },
    { name: 'graph_references', desc: 'Every reference (use site) of a name — file, line, column.', example: 'Find all references to ToolDef.' },
    { name: 'graph_imports', desc: 'The modules/paths a file imports.', example: 'What does src/offload/mcp.rs import?' },
    { name: 'graph_outline', desc: 'Every definition in a file, in source order (a structural outline).', example: 'Outline BackendDashboardCard.svelte.' },
    { name: 'graph_snippet', desc: "Fetch just one definition's body (by symbol, or file+line) instead of reading the whole file. Pair with graph_outline for big files.", example: 'Show the body of dispatch_recorded.' },
    { name: 'graph_repo_map', desc: 'A budget-bounded map of the most call-central files with their top signatures — orient fast at the start of a task.', example: 'Give me a project map.' },
    { name: 'graph_transitive', desc: 'Transitive call chain for a symbol — everything it reaches (callees) or that reaches it (callers).', example: 'What does runOffloadTest transitively call?' },
    { name: 'graph_search_docs', desc: 'Keyword search over docs and doc-comments; returns matching snippets.', example: "Search the docs for 'warm pool'." },
    { name: 'graph_struct_search', desc: 'Find code by AST shape via a tree-sitter query (not text).', example: 'Find every .unwrap() in the Rust code.' },
    { name: 'graph_semantic_docs', desc: 'Meaning-based (embedding) search over docs — only when Semantic search is enabled.', example: 'Find docs about how offload timeouts are handled.' },
    { name: 'graph_semantic_code', desc: 'Meaning-based (embedding) search over symbol bodies — only when "Embed code bodies" is enabled. Returns file:line/kind/signature/distance, never the body; pair with graph_snippet.', example: 'Find code that retries a failed network request.' },
    { name: 'graph_dead_exports', desc: 'Candidate unused public symbols (no reference, no inbound call). Candidates only — may include false positives.', example: 'List candidate dead exports.' },
    { name: 'graph_cycles', desc: 'Import cycles between files (loops of files that import one another).', example: 'Are there any import cycles?' },
    { name: 'graph_impact', desc: 'Blast radius: what could this change break? Defaults to the working-tree diff vs HEAD; pass symbols to analyze specific names instead. include_tests appends an affected-tests block. Results are approximate (name-keyed).', example: 'What would break if I change GraphIndex::dependents_transitive?' },
    { name: 'graph_tests_for', desc: 'Which tests (candidates) would exercise a symbol or file if it changed — the transitive dependents tagged as tests. Candidates only — dynamic dispatch/fixtures aren\'t captured.', example: 'What tests cover dependents_transitive?' },
    { name: 'graph_recent_changes', desc: "What's been happening lately — files ranked by git churn (touch count, then recency) with their last commit subject. File-level, 90-day window. Unavailable outside a git repo.", example: 'What files have changed most recently?' },
    { name: 'context_recall', desc: "Recall this session's working set — the files it read/edited/queried and the symbols touched.", example: 'What has this session been working on?' },
    { name: 'context_note', desc: 'Remember a non-obvious decision/fact for this project (pin to keep it across sessions).', example: 'Note: we chose FNV hashing for stability.' },
    { name: 'context_notes', desc: "List this session's notes plus every pinned note for the project.", example: 'Show my remembered notes.' },
  ];

  // Reference list of the tools the offload feature provides. `offload_task`
  // is the MCP tool Claude calls to delegate; read_file / code_search /
  // run_command are the native tools the local worker uses to complete the
  // task (toggle them in Settings → Offload → Tools). Static docs (moved from
  // OffloadServerView).
  const OFFLOAD_TOOLS = [
    { name: 'offload_task', desc: 'Delegate a token-heavy subtask to the local model and get back only the synthesized result — conserving the main session’s context.', example: 'Offload: summarize every TODO/FIXME across the repo and group them by theme.' },
    { name: 'read_file', desc: 'Worker reads a file (within the configured allowed roots).', example: 'Read src/offload/openai.rs, lines 1–200.' },
    { name: 'code_search', desc: 'Worker searches the codebase with ripgrep.', example: 'Search the repo for predicted_per_second.' },
    { name: 'run_command', desc: 'Worker runs an allowlisted, read-only command.', example: 'Run git log --oneline -20.' },
  ];

  type Section = 'activities' | 'graph-tools' | 'offload-tools';
  const SECTIONS: { id: Section; label: string }[] = [
    { id: 'activities', label: 'Activities' },
    { id: 'graph-tools', label: 'Graph tools' },
    { id: 'offload-tools', label: 'Offload tools' },
  ];
  let section = $state<Section>('activities');

  // Graph-call history is poll-based (same 2s cadence CodeIntelligenceView
  // used); offload request history rides the pushed dashboard snapshots, with
  // a one-shot fetch to seed before the first poller tick.
  let graphCalls = $state<GraphCall[]>([]);
  let dashboards = $state<BackendDashboard[]>([]);
  let poll: ReturnType<typeof setInterval> | null = null;

  async function refresh(): Promise<void> {
    try {
      graphCalls = await graphHistory();
    } catch {
      /* graph disabled — the feed just shows offload rows */
    }
  }

  // Armed at component init so teardown survives an unmount during an await.
  let pushedMetrics = false;
  listenManaged(() =>
    onOffloadServerMetrics((rows) => {
      pushedMetrics = true;
      dashboards = rows;
    })
  );

  onMount(async () => {
    // The seed fetch runs alongside the first graph poll; a pushed snapshot
    // that lands while it's in flight is fresher than the one-shot response,
    // so the seed must never clobber it.
    const seed = offloadServerMetrics();
    await refresh();
    const seeded = await seed;
    if (!pushedMetrics) dashboards = seeded;
    poll = setInterval(refresh, 2000);
  });

  onDestroy(() => {
    if (poll) clearInterval(poll);
  });

  // One unified feed row. `source` is the agent (claude/opencode/offload) for
  // graph calls and the backend name for offload requests.
  interface ActivityRow {
    key: string;
    ts: number;
    kind: 'graph' | 'offload';
    source: string;
    main: string;
    meta: string;
    ok: boolean;
  }

  const rows = $derived.by(() => {
    const out: ActivityRow[] = [];
    for (const c of graphCalls) {
      out.push({
        key: `g-${c.ts_ms}-${c.tool}-${c.target}-${c.source}`,
        ts: c.ts_ms,
        kind: 'graph',
        source: c.source,
        main: `${c.tool.replace('graph_', '')} · ${c.target}`,
        meta: `${c.ms}ms · ${fmtTok(c.chars)} chars`,
        ok: c.ok,
      });
    }
    for (const d of dashboards) {
      for (const r of d.metrics.history) {
        out.push({
          key: `o-${d.name}-${r.start_ms}-${r.slot}`,
          ts: r.start_ms,
          kind: 'offload',
          source: d.name,
          main: `request · slot ${r.slot}`,
          meta: `${r.duration_s.toFixed(1)}s · ${(r.prompt_tokens + r.tokens).toLocaleString()} tok · ${Math.round(r.avg_tps)} tok/s`,
          ok: true,
        });
      }
    }
    // No combined cap: both sources are already ring-capped upstream (graph
    // history at 200, offload history at 50 per backend), and slicing the
    // merged feed would let a graph-heavy burst silently crowd every offload
    // row out of view.
    out.sort((a, b) => b.ts - a.ts);
    // A repeat of the same call shape within one millisecond is possible
    // (e.g. batched tool dispatch); suffix repeats so the keyed {#each}
    // never sees duplicate keys.
    const seen = new Map<string, number>();
    for (const r of out) {
      const n = seen.get(r.key) ?? 0;
      seen.set(r.key, n + 1);
      if (n > 0) r.key = `${r.key}-${n}`;
    }
    return out;
  });
</script>

<div class="tool-activity">
  <header>
    <h2>Tool Activity</h2>
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

  {#if !$settings.graph.enabled && !$settings.offload.enabled}
    <div class="feature-note">
      The code graph and offload are both disabled (Settings → Code Graph /
      Offload), so none of these tools are registered with any agent and no
      activity will be recorded here.
    </div>
  {/if}

  {#if section === 'activities'}
    <section class="card history">
      <div class="history-head">
        Recent tool activity <span class="muted">(newest first)</span>
      </div>
      <p class="caveat">
        Code-intelligence graph calls and offload requests, merged into one
        chronological feed. Graph calls appear when an agent queries the code
        graph; offload requests when a backend serves a delegated task.
      </p>
      {#if rows.length === 0}
        <div class="history-empty">
          No tool activity yet — query the graph from a Claude tab or run an
          offload_task and it shows up here.
        </div>
      {:else}
        <div class="history-rows">
          {#each rows as r (r.key)}
            <div class="hrow" class:err={!r.ok}>
              <span class="htime">{fmtTime(r.ts)}</span>
              <span class="hkind {r.kind}">{r.kind}</span>
              <span class="hsrc {r.source}">{r.source}</span>
              <span class="hmain" title={r.main}>{r.main}</span>
              <span class="hmeta">{r.meta}</span>
            </div>
          {/each}
        </div>
      {/if}
    </section>
  {:else if section === 'graph-tools'}
    <ToolsReference
      title="Graph tools"
      tools={GRAPH_TOOLS}
      note="MCP tools exposed to Claude (and the offload worker) while the graph is enabled. Ask in natural language — Claude picks the tool."
    />
  {:else if section === 'offload-tools'}
    <ToolsReference
      title="Offload tools"
      tools={OFFLOAD_TOOLS}
      note="offload_task is the tool Claude calls to delegate; the rest are the tools the local worker uses to complete the task."
    />
  {/if}
</div>

<style>
  .tool-activity {
    /* Sit ABOVE the pane's absolutely-positioned (empty) terminal slot, the
       same convention as CodeIntelligenceView/OffloadServerView — otherwise
       that transparent slot paints on top and swallows every click. */
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
  /* Segmented section nav under the header (matches CodeIntelligenceView). */
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
  .card {
    border: 1px solid var(--border, #3a3a3a);
    border-radius: 8px;
    padding: 12px;
    margin-bottom: 12px;
    background: var(--panel, #1e1e1e);
  }
  .caveat {
    font-size: 11px;
    opacity: 0.65;
    margin: 2px 0 8px;
  }
  .history {
    --hrow-h: 1.55rem;
  }
  .history-head {
    font-weight: 600;
    margin-bottom: 6px;
  }
  .history-empty {
    opacity: 0.6;
    font-style: italic;
  }
  .history-rows {
    display: flex;
    flex-direction: column;
    /* Bounded like the predecessors' 5-row .history-body (scaled up for a
       dedicated tab): the feed scrolls internally, so new rows never grow
       the card or jump the page layout. */
    max-height: calc(24 * var(--hrow-h));
    overflow-y: auto;
  }
  .feature-note {
    border: 1px solid rgba(227, 179, 65, 0.5);
    border-radius: 8px;
    padding: 8px 12px;
    margin-bottom: 12px;
    background: rgba(227, 179, 65, 0.08);
    color: #e3b341;
    font-size: 12px;
  }
  .hrow {
    display: grid;
    grid-template-columns: 5.5rem 4.5rem 6rem 1fr 12rem;
    align-items: center;
    gap: 8px;
    height: var(--hrow-h);
    box-sizing: border-box;
    padding: 0 4px;
    border-bottom: 1px solid var(--border, #2a2a2a);
    font-size: 0.86em;
    white-space: nowrap;
  }
  .hrow.err {
    color: #ffb4ab;
  }
  .hkind,
  .hsrc {
    text-transform: uppercase;
    font-size: 0.82em;
    font-weight: 600;
    opacity: 0.85;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  /* Feed-kind accents: graph = blue, offload = green. Hardcoded hex (not
     --text-*, which follow the terminal palette) so they stay legible in any
     theme — same rationale as ToolsReference's tool names. */
  .hkind.graph {
    color: #58a6ff;
  }
  .hkind.offload {
    color: #3fb950;
  }
  /* Agent-source accents for graph rows (claude/opencode/offload, plus the
     backend-internal read_advisor/auto_check services), matching the palette
     the Code Intelligence activity feed used. */
  .hsrc.claude {
    color: #58a6ff;
  }
  .hsrc.opencode {
    color: #d2a8ff;
  }
  .hsrc.offload {
    color: #3fb950;
  }
  .hsrc.read_advisor {
    color: #e3b341;
  }
  .hsrc.auto_check {
    color: #f0883e;
  }
  .hmain {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .hmeta {
    text-align: right;
    opacity: 0.7;
  }
  .muted {
    opacity: 0.6;
    font-weight: 400;
  }
</style>
