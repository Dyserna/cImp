<script lang="ts">
  // The read-only, app-rendered Tool Activity tab (displayed as "Tools" in
  // the tab strip) — one place to see which tools are available and how the
  // tool infrastructure is doing. Six sections: Graph tools and Offload tools
  // (the two reference lists, moved here from the Code Intelligence and
  // Offload Server tabs), Graph index (the graph indexer dashboard + rebuild
  // actions, moved here from the Code Intelligence tab's Overview), Graph
  // view (the live force-graph — formerly the reserved Graph View tab,
  // retired in schema v26), Offload server (the live backend dashboard —
  // formerly the reserved Offload Server tab, retired in schema v25), plus
  // Code audit (the Security | Quality scan panels — formerly the reserved
  // Code Audit tab, retired in schema v27). Same reserved/no-PTY pattern as
  // CodeIntelligenceView; rendered by Pane.svelte for the `tool-activity`
  // tab id.
  //
  // The "Activities" section (the persistent activity FEED) lived here from
  // v0.41.0 until the #51 consolidation removed it: the Events tab shows the
  // same store with tab/session attribution, filters, and the delete/clear
  // actions — one feed, one place. This tab keeps the reference/dashboard
  // sections only.
  import { settings } from './settings/store';
  import ToolsReference from './ToolsReference.svelte';
  import OffloadServerView from './OffloadServerView.svelte';
  import GraphIndexView from './GraphIndexView.svelte';
  import GraphView from './GraphView.svelte';
  import CodeAuditView from './CodeAuditView.svelte';
  import { graphReveal } from './graphReveal';
  import SectionNav from './SectionNav.svelte';
  import { loadViewSection } from './viewSection';

  // Reference list of the graph_* MCP tools the code graph exposes to AI tabs
  // (and the offload worker) while the graph is enabled. Mirrors the
  // descriptions in `src-tauri/src/graph/mcp.rs::tool_specs`; kept here as
  // static docs (moved from CodeIntelligenceView).
  const GRAPH_TOOLS = [
    { name: 'graph_find_symbol', desc: 'Where a symbol (function/struct/trait/…) is defined — file, line, kind. Never source text (V32 H-1); pair with graph_snippet for the body.', example: 'Where is GraphService defined?' },
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
    { name: 'graph_path', desc: 'Shortest path between two code entities through call/import/containment edges — how does X reach Y. Each hop shows its edge kind and confidence; says so plainly when there is no path instead of inventing one.', example: 'How does the auth handler reach the connection pool?' },
    { name: 'graph_architecture', desc: 'A once-per-project map of the system’s shape: god nodes (the highest-degree hubs everything flows through), subsystems (cohesive file communities), and surprising connections (candidate accidental coupling). Topology only; clustering is heuristic, so treat subsystem boundaries as advisory.', example: 'What does this codebase look like architecturally?' },
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
  // is the MCP tool an AI tab calls to delegate; read_file / code_search /
  // run_command are the native tools the local worker uses to complete the
  // task (toggle them in Settings → Offload task tools → Tools). Static docs (moved from
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
    | 'graph-tools'
    | 'graph-index'
    | 'graph-view'
    | 'offload-tools'
    | 'offload-server'
    | 'code-audit';
  const SECTIONS: { id: Section; label: string }[] = [
    { id: 'offload-server', label: 'Offload server' },
    { id: 'offload-tools', label: 'Offload tools' },
    { id: 'graph-index', label: 'Graph index' },
    { id: 'graph-view', label: 'Graph view' },
    { id: 'graph-tools', label: 'Graph tools' },
    { id: 'code-audit', label: 'Code audit' },
  ];
  // The selection survives the component's destroy/recreate cycle (tab
  // switch, hide/un-hide) and app restarts — see viewSection.ts. A persisted
  // 'activities' (the removed section) fails validation and lands here too.
  let section = $state<Section>(
    loadViewSection('tool-activity', SECTIONS.map((s) => s.id), 'offload-server'),
  );

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

</script>

<div class="tool-activity">
  <header>
    <h2>Tools</h2>
  </header>

  <SectionNav
    view="tool-activity"
    sections={SECTIONS}
    bind:section
  />

  {#if !$settings.graph.enabled && !$settings.offload.enabled && !$settings.code_audit.enabled}
    <div class="feature-note">
      The code graph, offload, and Code Audit are all disabled (Settings →
      Code Graph / Offload / Code Audit), so none of these tools are registered
      with any agent and no new activity will be recorded here.
    </div>
  {/if}

  {#if section === 'graph-tools'}
    <ToolsReference
      title="Graph tools"
      tools={GRAPH_TOOLS}
      note="MCP tools exposed to AI tabs (and the offload worker) while the graph is enabled. Ask in natural language — the agent picks the tool."
    />
  {:else if section === 'graph-index'}
    <!-- The graph indexer dashboard (status cards + rebuild/pause actions),
         in normal flow so this container keeps owning the scroll. -->
    <GraphIndexView />
  {:else if section === 'offload-tools'}
    <ToolsReference
      title="Offload tools"
      tools={OFFLOAD_TOOLS}
      note="offload_task is the tool an AI tab calls to delegate; the rest are the tools the local worker uses to complete the task."
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
  .feature-note {
    border: 1px solid var(--border-warning, rgba(227, 179, 65, 0.5));
    border-radius: 8px;
    padding: 8px 12px;
    margin-bottom: 12px;
    background: var(--surface-warning-faint, rgba(227, 179, 65, 0.08));
    color: var(--text-warning, #e3b341);
    font-size: 12px;
  }
</style>
