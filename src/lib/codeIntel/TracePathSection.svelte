<script lang="ts">
  /// Code Intelligence → Trace path (#130, F4). V15 Feature 1: the shortest
  /// path between two entities over the graph's call/import/contains edges.
  ///
  /// Self-contained. Nothing outside this section reads the form fields or the
  /// result, and no poll drives it — a trace happens when the user asks. The
  /// `.preview-in` / `.preview-meta` / `.pin-toggle` / `.conf` rules it shares
  /// with Memory and Context are in `codeIntel.css` and reach this markup
  /// through the DOM; only the `.path-*` family travelled with it. The
  /// `.conf.<confidence>` FILLS are part of that — they were left in
  /// `AnalysesSection`'s scoped block by the split, where nothing here could
  /// match them, so an edge's confidence badge had its shape and none of its
  /// colour (V42 Phase-F review, F-2).
  ///
  /// `active` gates the MARKUP only, and the parent mounts this component
  /// unconditionally: the two endpoints someone typed and the trace they ran
  /// lived in the parent's script, which a section switch never touched. An
  /// `{#if}` around the component would clear the form every time they looked
  /// at another section.
  import { graphPath, type PathNodeRow, type PathResult } from '../graph';

  let {
    active,
  }: {
    /// Whether Trace path is the section on screen. Gates the markup only.
    active: boolean;
  } = $props();

  // V15 Feature 1: path tracing — the "Trace path" section. `from`/`to` accept
  // a symbol name, `file:line`, or bare file path (resolved backend-side).
  // Edge-kind toggles default to all three kinds checked.
  let pathFrom = $state('');
  let pathTo = $state('');
  let pathSymmetric = $state(false);
  let pathKindCall = $state(true);
  let pathKindImport = $state(true);
  let pathKindContains = $state(true);
  let pathResult = $state<PathResult | null>(null);
  let pathBusy = $state(false);
  let pathError = $state<string | null>(null);

  async function runPath(): Promise<void> {
    if (!pathFrom.trim() || !pathTo.trim() || pathBusy) return;
    pathBusy = true;
    pathError = null;
    try {
      const kinds = [
        pathKindCall ? 'call' : null,
        pathKindImport ? 'import' : null,
        pathKindContains ? 'contains' : null,
      ].filter((k): k is string => k !== null);
      pathResult = await graphPath(pathFrom.trim(), pathTo.trim(), {
        kinds,
        symmetric: pathSymmetric,
      });
    } catch (e) {
      pathError = String(e);
      pathResult = null;
    } finally {
      pathBusy = false;
    }
  }

  // A file node's `label` is just its path; a symbol node shows name + loc + kind.
  function pathNodeText(n: PathNodeRow): string {
    return n.kind === 'file' ? n.file : `${n.label} (${n.file}:${n.line}) [${n.kind}]`;
  }
</script>

{#if active}
    <div class="path-sec">
      <p class="caveat">
        Traces the shortest path between two entities over the code graph's
        call/import/contains edges. Heuristic — a missing edge (dynamic
        dispatch, an unindexed language) can hide a real path.
      </p>
      <section class="card">
        <div class="history-head">Trace path</div>
        <div class="preview-in path-in">
          <input
            type="text"
            placeholder="symbol name, file:line, or file path"
            bind:value={pathFrom}
            onkeydown={(e) => e.key === 'Enter' && runPath()}
          />
          <span class="path-sep">→</span>
          <input
            type="text"
            placeholder="symbol name, file:line, or file path"
            bind:value={pathTo}
            onkeydown={(e) => e.key === 'Enter' && runPath()}
          />
          <button onclick={runPath} disabled={pathBusy || !pathFrom.trim() || !pathTo.trim()}>
            {pathBusy ? 'Tracing…' : 'Trace'}
          </button>
        </div>
        <div class="path-opts">
          <label class="pin-toggle">
            <input type="checkbox" bind:checked={pathSymmetric} /> Undirected (related at all?)
          </label>
          <label class="pin-toggle"><input type="checkbox" bind:checked={pathKindCall} /> call</label>
          <label class="pin-toggle"><input type="checkbox" bind:checked={pathKindImport} /> import</label>
          <label class="pin-toggle"><input type="checkbox" bind:checked={pathKindContains} /> contains</label>
        </div>

        {#if pathError}
          <p class="error">{pathError}</p>
        {/if}

        {#if pathResult}
          {#if !pathResult.found}
            <p class="placeholder">No path found within the hop limit (or an endpoint isn't indexed).</p>
          {:else}
            <div class="path-chain">
              {#each pathResult.nodes as n, i (n.id + ':' + i)}
                <div class="path-node" title={n.file}>{pathNodeText(n)}</div>
                {#if n.edge_to_next}
                  <div class="path-edge">
                    ──{n.edge_to_next}{#if n.confidence}<span class="conf {n.confidence}" title="edge confidence: {n.confidence}">{n.confidence}</span>{/if}──▶
                  </div>
                {/if}
              {/each}
            </div>
            <p class="preview-meta">
              {pathResult.hops} hop{pathResult.hops === 1 ? '' : 's'}{#if pathResult.equal_alternatives > 0}
                &nbsp;(+{pathResult.equal_alternatives} other path{pathResult.equal_alternatives === 1 ? '' : 's'} of equal length)
              {/if}
            </p>
          {/if}
        {/if}
      </section>
    </div>
{/if}

<style>
  /* ── V15 Feature 1: Trace path ─────────────────────────────────────────── */
  .path-in {
    align-items: center;
  }
  .path-sep {
    opacity: 0.6;
    flex: 0 0 auto;
  }
  .path-opts {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 16px;
    margin: 2px 0 10px;
    font-size: 12px;
  }
  .path-chain {
    display: flex;
    flex-direction: column;
    font-size: 12px;
    margin: 6px 0;
  }
  .path-node {
    font-family: monospace;
    padding: 2px 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .path-edge {
    padding: 1px 0 1px 1.2em;
    opacity: 0.75;
    font-size: 11px;
    font-family: monospace;
  }
</style>
