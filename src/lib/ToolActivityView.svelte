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
  // The two reference lists are GENERATED (V42 F6, #131) from
  // `src-tauri/src/service/toolref.rs`, whose tests pin the SET of names to the
  // real tool tables in both directions. They used to be two hand-written arrays
  // right here, mirroring `graph::mcp::tools::tool_specs` and the offload tool
  // defs by eye; the mirror drifted (#113, D1 — three graph tools missing) and
  // nothing noticed. Add a tool on the Rust side and the suite tells you to
  // write its user-facing line.
  import { GRAPH_TOOLS, OFFLOAD_TOOLS } from './generated/tools';

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
      note="offload_task and offload_batch are the tools an AI tab calls to delegate; the rest are the tools the local worker uses to complete the task."
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
