<script lang="ts">
  // The read-only, app-rendered Tool Activity tab (displayed as "Tools" in
  // the tab strip) — one place to see what
  // tools the agents are using and which tools are available. Seven sections:
  // Activities (the unified, newest-first feed from the backend's persistent
  // activity store — graph/context tool calls plus completed offload_task
  // runs, surviving app restarts), Graph tools, and Offload tools (the two
  // reference lists, moved here from the Code Intelligence and Offload Server
  // tabs), Graph index (the graph indexer dashboard + rebuild actions, moved
  // here from the Code Intelligence tab's Overview), Graph view (the live
  // force-graph — formerly the reserved Graph View tab, retired in schema
  // v26), Offload server (the live backend dashboard — formerly the
  // reserved Offload Server tab, retired in schema v25), plus Code audit
  // (the Security | Quality scan panels — formerly the reserved Code Audit
  // tab, retired in schema v27). Rows are clickable
  // (a popup shows the captured request/response), individually deletable,
  // and the whole history can be cleared. Same reserved/no-PTY pattern as
  // CodeIntelligenceView; rendered by Pane.svelte for the `tool-activity`
  // tab id.
  import { onMount, onDestroy } from 'svelte';
  import {
    activityClear,
    activityDelete,
    activityDetail,
    activityList,
    type ActivityEntry,
    type ActivityRecord,
  } from './activity';
  import { settings } from './settings/store';
  import { fmtTime } from './format';
  import { fmtTok } from './usageMath';
  import ToolsReference from './ToolsReference.svelte';
  import OffloadServerView from './OffloadServerView.svelte';
  import GraphIndexView from './GraphIndexView.svelte';
  import GraphView from './GraphView.svelte';
  import CodeAuditView from './CodeAuditView.svelte';
  import { graphReveal } from './graphReveal';
  import { TOOL_ACTIVITY_TAB_ID } from './tabs/types';
  import { isAppViewVisible, onAppViewShown } from './appViewVisibility';
  import { loadViewSection, saveViewSection } from './viewSection';

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
    { name: 'list_dir', desc: 'Worker enumerates a directory — the ground-truth answer to what files exist / how many.', example: 'List the top-level *.md files in docs/.' },
    { name: 'code_search', desc: 'Worker searches the codebase with ripgrep.', example: 'Search the repo for predicted_per_second.' },
    { name: 'run_command', desc: 'Worker runs an allowlisted, read-only command.', example: 'Run git log --oneline -20.' },
    { name: 'run_check', desc: "Worker runs one of the project's configured checks (build/typecheck/lint/test) to verify a claim before stating it. Inert until checks are configured.", example: 'Does the test suite pass? Prove it with run_check.' },
  ];

  type Section =
    | 'activities'
    | 'graph-tools'
    | 'graph-index'
    | 'graph-view'
    | 'offload-tools'
    | 'offload-server'
    | 'code-audit';
  const SECTIONS: { id: Section; label: string }[] = [
    { id: 'activities', label: 'Activities' },
    { id: 'offload-server', label: 'Offload server' },
    { id: 'offload-tools', label: 'Offload tools' },
    { id: 'graph-index', label: 'Graph index' },
    { id: 'graph-view', label: 'Graph view' },
    { id: 'graph-tools', label: 'Graph tools' },
    { id: 'code-audit', label: 'Code audit' },
  ];
  // The selection survives the component's destroy/recreate cycle (tab
  // switch, hide/un-hide) and app restarts — see viewSection.ts.
  let section = $state<Section>(
    loadViewSection('tool-activity', SECTIONS.map((s) => s.id), 'activities'),
  );
  $effect(() => saveViewSection('tool-activity', section));

  // Unlike the other sections, the Graph view keeps an expensive laid-out
  // force simulation, so it is NOT destroyed on a section switch: mounted
  // lazily on the first visit (while `graph.graph_viz` is on), then kept
  // alive hidden via display:none — GraphView's own IntersectionObserver
  // pauses its render loop while hidden, so an inactive section costs
  // nothing. Toggling `graph_viz` off unmounts it for real.
  let graphViewMounted = $state(false);
  $effect(() => {
    if (section === 'graph-view' && $settings.graph.graph_viz) graphViewMounted = true;
  });
  // The Workbench/Code-Audit "show in graph" jump writes the graphReveal
  // store (GraphView consumes and clears it) and reveals THIS tab — flipping
  // to the Graph view section is our part of that handoff.
  $effect(() => {
    if ($graphReveal) section = 'graph-view';
  });

  // The Code audit section gets the same keep-alive-hidden treatment: a
  // running scan streams into its (possibly hidden) AuditPanel and the
  // panels' selections are ephemeral, so destroying the view on a section
  // switch would drop mid-scan state. Mounted lazily on the first visit
  // (while `code_audit.enabled` is on), then kept alive via display:none —
  // the panels are event-driven, so a hidden section costs nothing. Toggling
  // `code_audit.enabled` off unmounts it for real.
  let codeAuditMounted = $state(false);
  $effect(() => {
    if (section === 'code-audit' && $settings.code_audit.enabled) codeAuditMounted = true;
  });

  // The unified feed is poll-based (same 2s cadence CodeIntelligenceView
  // used); both feed kinds land in the backend's single persistent store, so
  // one endpoint covers everything.
  let entries = $state<ActivityEntry[]>([]);
  let poll: ReturnType<typeof setInterval> | null = null;
  // Bumped by every local mutation (delete/clear). A poll response that was
  // already in flight when a mutation landed is stale — applying it would
  // resurrect just-deleted rows for a poll cycle — so refresh() drops it.
  let mutationSeq = 0;

  async function refresh(): Promise<void> {
    const seq = mutationSeq;
    try {
      const list = await activityList();
      if (seq === mutationSeq) entries = list;
    } catch {
      /* backend unavailable mid-teardown — keep whatever we have */
    }
  }

  // ── Detail popup ──────────────────────────────────────────────────────
  let detailOpen = $state(false);
  let detail = $state<ActivityRecord | null>(null);
  let detailMissing = $state(false);
  // Fetch token: a click on row B while row A's (slower) fetch is in flight
  // must not let A's late response overwrite the popup — the Delete button
  // acts on `detail.id`, so a stale overwrite would delete the wrong entry.
  let detailSeq = 0;

  async function openDetail(id: number): Promise<void> {
    const seq = ++detailSeq;
    detailOpen = true;
    detail = null;
    detailMissing = false;
    try {
      const rec = await activityDetail(id);
      if (seq !== detailSeq) return; // superseded by a later click / close
      if (rec) detail = rec;
      else detailMissing = true;
    } catch {
      if (seq === detailSeq) detailMissing = true;
    }
  }

  function closeDetail(): void {
    detailSeq += 1; // invalidate any in-flight fetch
    detailOpen = false;
    detail = null;
    detailMissing = false;
  }

  // ── Delete / clear ────────────────────────────────────────────────────
  // Both update optimistically, then unconditionally refresh AFTER the final
  // seq bump: the bump invalidates any poll that raced the backend mutation,
  // and the trailing refresh (which captures the post-bump seq) repaints
  // authoritatively — restoring the rows if the backend call failed.
  async function removeEntry(id: number): Promise<void> {
    mutationSeq += 1;
    entries = entries.filter((e) => e.id !== id);
    if (detail?.id === id) closeDetail();
    try {
      await activityDelete(id);
    } catch {
      /* the refresh below restores the row */
    }
    mutationSeq += 1;
    void refresh();
  }

  // Two-step confirm (same pattern as SaveLayoutDialog's overwrite): the
  // first click arms the button, a second within 4s clears for real.
  let confirmClear = $state(false);
  let clearTimer: ReturnType<typeof setTimeout> | null = null;

  async function clearHistory(): Promise<void> {
    if (!confirmClear) {
      confirmClear = true;
      clearTimer = setTimeout(() => (confirmClear = false), 4000);
      return;
    }
    if (clearTimer) clearTimeout(clearTimer);
    clearTimer = null;
    confirmClear = false;
    mutationSeq += 1;
    entries = [];
    closeDetail();
    try {
      await activityClear();
    } catch {
      /* the refresh below restores the feed */
    }
    mutationSeq += 1;
    void refresh();
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === 'Escape' && detailOpen) {
      e.preventDefault();
      closeDetail();
    }
  }

  // Keep-alive (appViews.ts): the poll idles while the tab is off-screen
  // and a fresh refresh runs the moment it comes back.
  const unsubShown = onAppViewShown(TOOL_ACTIVITY_TAB_ID, () => void refresh());

  onMount(() => {
    void refresh();
    poll = setInterval(() => {
      if (isAppViewVisible(TOOL_ACTIVITY_TAB_ID)) void refresh();
    }, 2000);
    window.addEventListener('keydown', onKeyDown);
  });

  onDestroy(() => {
    if (poll) clearInterval(poll);
    if (clearTimer) clearTimeout(clearTimer);
    window.removeEventListener('keydown', onKeyDown);
    unsubShown();
  });

  // Agent sources with a dedicated accent class; anything else (offload
  // backend names are user-chosen) falls back to the default row color rather
  // than leaking arbitrary strings into the class attribute.
  const KNOWN_SOURCES = new Set([
    'claude',
    'opencode',
    'offload',
    'read_advisor',
    'auto_check',
    'audit',
  ]);
  function srcClass(source: string): string {
    return KNOWN_SOURCES.has(source) ? ` ${source}` : '';
  }

  function rowMain(e: ActivityEntry): string {
    // mcp tools are namespaced `<server>__<tool>` — render the first `__`
    // as a separator so the server reads as a prefix.
    const tool =
      e.kind === 'graph'
        ? e.tool.replace('graph_', '')
        : e.kind === 'mcp'
          ? e.tool.replace('__', '/')
          : e.tool;
    return e.target ? `${tool} · ${e.target}` : tool;
  }

  function rowMeta(e: ActivityEntry): string {
    const dur = e.ms >= 10_000 ? `${(e.ms / 1000).toFixed(1)}s` : `${e.ms}ms`;
    // For audit entries `chars` carries the finding count, not a payload size.
    return e.kind === 'audit'
      ? `${dur} · ${e.chars} findings`
      : `${dur} · ${fmtTok(e.chars)} chars`;
  }
</script>

<div class="tool-activity">
  <header>
    <h2>Tools</h2>
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

  {#if !$settings.graph.enabled && !$settings.offload.enabled && !$settings.code_audit.enabled}
    <div class="feature-note">
      The code graph, offload, and Code Audit are all disabled (Settings →
      Code Graph / Offload / Code Audit), so none of these tools are registered
      with any agent and no new activity will be recorded here.
    </div>
  {/if}

  {#if section === 'activities'}
    <section class="card history">
      <div class="history-head">
        <span>Recent tool activity <span class="muted">(newest first)</span></span>
        {#if entries.length > 0}
          <button
            type="button"
            class="clear-btn"
            class:arm={confirmClear}
            onclick={clearHistory}
          >{confirmClear ? 'Confirm clear' : 'Clear history'}</button>
        {/if}
      </div>
      <p class="caveat">
        Code-intelligence graph calls, offload runs, Code Audit scans, and
        proxied MCP tool calls, merged into one chronological feed that
        survives restarts. Click a row to see the actual request and response;
        × deletes a single entry.
      </p>
      {#if entries.length === 0}
        <div class="history-empty">
          No tool activity yet — query the graph from a Claude tab or run an
          offload_task and it shows up here.
        </div>
      {:else}
        <div class="history-rows">
          {#each entries as r (r.id)}
            <div
              class="hrow"
              class:err={!r.ok}
              role="button"
              tabindex="0"
              onclick={() => void openDetail(r.id)}
              onkeydown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  void openDetail(r.id);
                }
              }}
            >
              <span class="htime">{fmtTime(r.ts_ms)}</span>
              <span class="hkind {r.kind}">{r.kind}</span>
              <span class="hsrc{srcClass(r.source)}" title={r.source}>{r.source}</span>
              <span class="hmain" title={rowMain(r)}>{rowMain(r)}</span>
              <span class="hmeta">{rowMeta(r)}</span>
              <button
                type="button"
                class="hdel"
                title="Delete entry"
                aria-label="Delete entry"
                onclick={(e) => {
                  e.stopPropagation();
                  void removeEntry(r.id);
                }}
              >×</button>
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
  {:else if section === 'graph-index'}
    <!-- The graph indexer dashboard (status cards + rebuild/pause actions),
         in normal flow so this container keeps owning the scroll. -->
    <GraphIndexView />
  {:else if section === 'offload-tools'}
    <ToolsReference
      title="Offload tools"
      tools={OFFLOAD_TOOLS}
      note="offload_task is the tool Claude calls to delegate; the rest are the tools the local worker uses to complete the task."
    />
  {:else if section === 'offload-server'}
    <!-- The live backend dashboard (event-driven, remount-cheap), in normal
         flow so this container keeps owning the scroll. -->
    <OffloadServerView />
  {:else if section === 'graph-view' && !$settings.graph.graph_viz}
    <div class="feature-note">
      The Graph view (live force graph of the code graph) is disabled. Turn it
      on in Settings → Code Intelligence to draw it here.
    </div>
  {:else if section === 'code-audit' && !$settings.code_audit.enabled}
    <div class="feature-note">
      Code Audit is disabled. Turn it on in Settings → Code Audit to run
      security and quality scans here.
    </div>
  {/if}

  <!-- Kept mounted (hidden, not destroyed) across section switches so the
       laid-out simulation survives — see the graphViewMounted note above. -->
  {#if graphViewMounted && $settings.graph.graph_viz}
    <div class="graph-host" class:hidden={section !== 'graph-view'}>
      <GraphView />
    </div>
  {/if}

  <!-- Kept mounted (hidden, not destroyed) across section switches so a
       running scan keeps streaming — see the codeAuditMounted note above. -->
  {#if codeAuditMounted && $settings.code_audit.enabled}
    <div class="audit-host" class:hidden={section !== 'code-audit'}>
      <CodeAuditView />
    </div>
  {/if}
</div>

{#if detailOpen}
  <div class="backdrop" onclick={closeDetail} role="presentation"></div>
  <div class="detail-card" role="dialog" aria-label="Tool activity detail">
    {#if detail}
      <header class="detail-head">
        <div class="detail-title">
          <span class="hkind {detail.kind}">{detail.kind}</span>
          <span class="detail-tool">{detail.tool}</span>
          {#if !detail.ok}<span class="detail-failed">failed</span>{/if}
        </div>
        <button type="button" class="detail-close" onclick={closeDetail} aria-label="Close">×</button>
      </header>
      <div class="detail-meta">
        {fmtTime(detail.ts_ms)} · {detail.source} · {rowMeta(detail)}{#if detail.target}&nbsp;· <span title={detail.target}>{detail.target}</span>{/if}
      </div>
      <div class="detail-body">
        <div class="payload">
          <div class="payload-head">Request</div>
          <pre>{detail.request || '(not captured)'}</pre>
        </div>
        <div class="payload">
          <div class="payload-head">Response</div>
          <pre>{detail.response || '(not captured)'}</pre>
        </div>
      </div>
      <footer class="detail-actions">
        <button
          type="button"
          class="detail-delete"
          onclick={() => {
            if (detail) void removeEntry(detail.id);
          }}
        >Delete entry</button>
        <button type="button" class="detail-dismiss" onclick={closeDetail}>Close</button>
      </footer>
    {:else if detailMissing}
      <header class="detail-head">
        <div class="detail-title">Entry not found</div>
        <button type="button" class="detail-close" onclick={closeDetail} aria-label="Close">×</button>
      </header>
      <div class="detail-meta">This entry was deleted or has aged out of the history.</div>
    {:else}
      <div class="detail-meta">Loading…</div>
    {/if}
  </div>
{/if}

<style>
  .tool-activity {
    /* Sit ABOVE the pane's absolutely-positioned (empty) terminal slot, the
       same convention as CodeIntelligenceView — otherwise that transparent
       slot paints on top and swallows every click. */
    position: absolute;
    inset: 0;
    overflow-y: auto;
    padding: 16px;
    font-size: 13px;
    color: var(--text-primary, #ddd);
    box-sizing: border-box;
    /* Flex column (children stack exactly as in normal flow) so the Graph
       view host below can flex-grow to the remaining pane height — a canvas
       has no natural height to size the section by. */
    display: flex;
    flex-direction: column;
  }
  /* The Graph view's positioning context: GraphView's root is
     absolute-inset (its original tab convention), so the host provides the
     size — the rest of the pane, but never less than a usable canvas (the
     container scrolls beyond that). */
  .graph-host {
    position: relative;
    flex: 1;
    min-height: 420px;
  }
  .graph-host.hidden {
    display: none;
  }
  /* The Code audit section's positioning context: CodeAuditView's root is
     absolute-inset (its original tab convention), so the host provides the
     size — the rest of the pane, but never less than a usable table (the
     container scrolls beyond that). */
  .audit-host {
    position: relative;
    flex: 1;
    min-height: 420px;
  }
  .audit-host.hidden {
    display: none;
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
  .card {
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 8px;
    padding: 12px;
    margin-bottom: 12px;
    background: var(--surface-card, #1e1e1e);
  }
  .caveat {
    font-size: 11px;
    opacity: 0.65;
    margin: 2px 0 8px;
  }
  .history {
    --hrow-h: 1.55rem;
    /* Fill the rest of the pane (the container is a flex column); the row
       list below scrolls internally. The floor keeps a usable feed on short
       panes — the container scrolls beyond that. */
    flex: 1;
    min-height: calc(8 * var(--hrow-h) + 6rem);
    display: flex;
    flex-direction: column;
  }
  .history-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-weight: 600;
    margin-bottom: 6px;
  }
  .clear-btn {
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 6px;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 11px;
    font-weight: 400;
    padding: 2px 10px;
    cursor: pointer;
    opacity: 0.75;
  }
  .clear-btn:hover {
    opacity: 1;
    background: rgba(255, 255, 255, 0.06);
  }
  /* Armed state: the second click clears for real. */
  .clear-btn.arm {
    color: var(--text-danger-soft, #ffb4ab);
    border-color: var(--text-danger-soft, #ffb4ab);
    opacity: 1;
  }
  .history-empty {
    opacity: 0.6;
    font-style: italic;
  }
  .history-rows {
    display: flex;
    flex-direction: column;
    /* The feed scrolls internally within the flex-sized card, so new rows
       never grow the card or jump the page layout. */
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .feature-note {
    border: 1px solid var(--border-warning, rgba(227, 179, 65, 0.5));
    border-radius: 8px;
    padding: 8px 12px;
    margin-bottom: 12px;
    background: var(--surface-warning-faint, rgba(227, 179, 65, 0.08));
    color: var(--text-warning, #e3b341);
    font-size: 12px;
  }
  .hrow {
    display: grid;
    grid-template-columns: 5.5rem 4.5rem 6rem 1fr 12rem 1.4rem;
    align-items: center;
    gap: 8px;
    height: var(--hrow-h);
    box-sizing: border-box;
    padding: 0 4px;
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
    font-size: 0.86em;
    white-space: nowrap;
    cursor: pointer;
  }
  .hrow:hover,
  .hrow:focus-visible {
    background: rgba(255, 255, 255, 0.05);
    outline: none;
  }
  .hrow.err {
    color: var(--text-danger-soft, #ffb4ab);
  }
  /* Per-row delete: kept invisible until the row is hovered/focused so the
     feed stays visually calm. */
  .hdel {
    border: none;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 1em;
    line-height: 1;
    padding: 0 2px;
    cursor: pointer;
    opacity: 0;
  }
  .hrow:hover .hdel,
  .hrow:focus-visible .hdel,
  .hdel:focus-visible {
    opacity: 0.6;
  }
  .hdel:hover {
    opacity: 1 !important;
    color: var(--text-danger-soft, #ffb4ab);
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
  /* Feed-kind accents: graph = info, offload = success, audit = orange,
     mcp = purple. Semantic theme tokens (not the neutral --text-* ramp, which
     follows the terminal palette) so each theme keeps them legible — dark ink
     variants on light themes. Orange has no token of its own, so it's mixed
     from the warning/danger semantics to stay distinct from read_advisor's
     yellow. */
  .hkind.graph {
    color: var(--text-info, #58a6ff);
  }
  .hkind.offload {
    color: var(--text-success, #3fb950);
  }
  .hkind.audit {
    color: color-mix(in srgb, var(--warning, #f0a020) 60%, var(--danger, #f06080));
  }
  .hkind.mcp {
    color: var(--accent-purple, #d2a8ff);
  }
  /* V32: injection-containment denials. Danger red, and the only kind with a
     tinted chip — a blocked SSRF target or a canary hit is the one row in this
     feed that means "something tried something", so it must not read as
     ordinary traffic when scrolling past. */
  .hkind.injection_flag {
    color: var(--danger, #f06080);
    background: color-mix(in srgb, var(--danger, #f06080) 14%, transparent);
    border-radius: 3px;
    padding: 0 3px;
  }
  /* Agent-source accents for graph rows (claude/opencode/offload, plus the
     backend-internal read_advisor/auto_check services), matching the palette
     the Code Intelligence activity feed used. */
  .hsrc.claude {
    color: var(--text-info, #58a6ff);
  }
  .hsrc.opencode {
    color: var(--accent-purple, #d2a8ff);
  }
  .hsrc.offload {
    color: var(--text-success, #3fb950);
  }
  .hsrc.read_advisor {
    color: var(--text-warning, #e3b341);
  }
  .hsrc.auto_check {
    color: color-mix(in srgb, var(--warning, #f0a020) 60%, var(--danger, #f06080));
  }
  .hsrc.audit {
    color: color-mix(in srgb, var(--warning, #f0a020) 60%, var(--danger, #f06080));
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

  /* ── Detail popup (request/response) — dialog conventions per
     SaveLayoutDialog: fixed backdrop + centered card. ─────────────────── */
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 100;
  }
  .detail-card {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    display: flex;
    flex-direction: column;
    width: min(820px, calc(100vw - 40px));
    max-height: min(80vh, 900px);
    background: var(--surface-3, #1e1e1e);
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: var(--radius-lg, 10px);
    padding: 14px 16px;
    color: var(--text-primary, #ddd);
    z-index: 101;
    box-shadow: var(--shadow-lg, 0 8px 32px rgba(0, 0, 0, 0.5));
    box-sizing: border-box;
    font-size: 13px;
  }
  .detail-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin: 0 0 4px;
  }
  .detail-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
    min-width: 0;
  }
  .detail-tool {
    font-family: var(--font-mono, monospace);
  }
  .detail-failed {
    color: var(--text-danger-soft, #ffb4ab);
    text-transform: uppercase;
    font-size: 0.8em;
  }
  .detail-close {
    border: none;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    opacity: 0.7;
    padding: 2px 6px;
  }
  .detail-close:hover {
    opacity: 1;
  }
  .detail-meta {
    font-size: 11px;
    opacity: 0.7;
    margin-bottom: 10px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .detail-body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .payload-head {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    opacity: 0.7;
    margin-bottom: 4px;
  }
  .payload pre {
    margin: 0;
    padding: 8px 10px;
    background: var(--surface-sunken, rgba(0, 0, 0, 0.3));
    border: 1px solid var(--border-faint, #2a2a2a);
    border-radius: 6px;
    font-size: 12px;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 40vh;
    overflow-y: auto;
  }
  .detail-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 12px;
  }
  .detail-actions button {
    padding: 4px 12px;
    border-radius: 6px;
    border: 1px solid var(--border-subtle, #3a3a3a);
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 12px;
    cursor: pointer;
  }
  .detail-actions button:hover {
    background: rgba(255, 255, 255, 0.06);
  }
  .detail-delete {
    color: var(--text-danger-soft, #ffb4ab);
    border-color: var(--border-danger-soft, rgba(255, 180, 171, 0.5));
  }
  .detail-delete:hover {
    border-color: var(--text-danger-soft, #ffb4ab);
  }
</style>
