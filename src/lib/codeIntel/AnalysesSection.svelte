<script lang="ts">
  /// Code Intelligence → Analyses (#130, F4). The three on-demand graph walks:
  /// dead exports, import cycles, and the impact of working-tree changes. Each
  /// runs on a click — walking the graph is comparatively expensive — so the
  /// results here are the user's, not a poll's.
  ///
  /// THE ONE THING THAT CROSSES THE SEAM: the "+N since last pass" badges. Their
  /// state does NOT live here, and deliberately so — the same numbers badge the
  /// section nav in the parent, where they have to be honest from whichever
  /// section the view happens to be on. The parent owns the `graph-analyses`
  /// event, the live counts and the acknowledged baseline; this component reads
  /// the two badge numbers and reports an acknowledgement back through `onAck`
  /// when a run completes, which is the only moment the baseline may move.
  ///
  /// `active` gates the MARKUP only, and the parent mounts this component
  /// unconditionally: a scan's results lived in the parent's script, which a
  /// section switch never touched, so they survived a look at another section.
  /// An `{#if}` around the component would throw away a scan the user paid for.
  ///
  /// Styles: `.card` / `.rows` / `.arow` / `.aname` / `.akind` / `.aloc` /
  /// `.muted` / `.caveat` / `.placeholder` / `.error` / `.badge` / `.actions` /
  /// `.conf` all come from `codeIntel.css`, keyed on the parent's
  /// `.graph-monitor` root. Only the rules no other section can reach —
  /// `.analyses .actions`, `.arow.cycle`, `.arow.dep` and the three
  /// `.conf.<confidence>` fills — travelled here.
  import {
    graphCycles,
    graphDeadExports,
    graphImpact,
    type DeadExportRow,
    type ImpactResult,
  } from '../graph';

  let {
    active,
    deadBadge,
    cyclesBadge,
    onAck,
  }: {
    /// Whether Analyses is the section on screen. Gates the markup only.
    active: boolean;
    /// "+N dead exports since the last pass", or null for no badge. Derived in
    /// the parent, which owns both halves of the comparison.
    deadBadge: number | null;
    /// "+N import cycles since the last pass", or null for no badge.
    cyclesBadge: number | null;
    /// A run of `kind` has just finished and the user is looking at its result,
    /// so the badge baseline may advance. `measured` is what THIS pass counted
    /// — the parent prefers the live event count when it has one and falls back
    /// to `measured` when no `graph-analyses` event has landed yet.
    onAck: (kind: 'dead' | 'cycles', measured: number) => void;
  } = $props();

  // Analyses (Phase B2): on-demand dead-export + import-cycle results. Run only
  // when the user clicks — walking the graph is comparatively expensive.
  let deadExports = $state<DeadExportRow[] | null>(null);
  let cycles = $state<string[][] | null>(null);
  let impact = $state<ImpactResult | null>(null);
  let analysisBusy = $state<'dead' | 'cycles' | 'impact' | null>(null);
  let analysisError = $state<string | null>(null);

  async function runDeadExports(): Promise<void> {
    analysisBusy = 'dead';
    analysisError = null;
    try {
      deadExports = await graphDeadExports();
      onAck('dead', deadExports.length);
    } catch (e) {
      analysisError = String(e);
    } finally {
      analysisBusy = null;
    }
  }

  async function runCycles(): Promise<void> {
    analysisBusy = 'cycles';
    analysisError = null;
    try {
      cycles = await graphCycles();
      onAck('cycles', cycles.length);
    } catch (e) {
      analysisError = String(e);
    } finally {
      analysisBusy = null;
    }
  }

  async function runImpact(): Promise<void> {
    analysisBusy = 'impact';
    analysisError = null;
    try {
      impact = await graphImpact();
    } catch (e) {
      analysisError = String(e);
    } finally {
      analysisBusy = null;
    }
  }
</script>

{#if active}
    <div class="analyses">
      <div class="actions">
        <button onclick={runDeadExports} disabled={analysisBusy !== null}>
          {analysisBusy === 'dead' ? 'Scanning…' : 'Find dead exports'}{#if deadBadge}<span class="badge" title="New since last pass">+{deadBadge}</span>{/if}
        </button>
        <button onclick={runCycles} disabled={analysisBusy !== null}>
          {analysisBusy === 'cycles' ? 'Scanning…' : 'Find import cycles'}{#if cyclesBadge}<span class="badge" title="New since last pass">+{cyclesBadge}</span>{/if}
        </button>
        <button onclick={runImpact} disabled={analysisBusy !== null}>
          {analysisBusy === 'impact' ? 'Scanning…' : 'Impact of working-tree changes'}
        </button>
      </div>

      {#if analysisError}
        <p class="error">{analysisError}</p>
      {/if}

      {#if deadExports !== null}
        <section class="card">
          <div class="history-head">
            Dead exports <span class="muted">({deadExports.length})</span>
          </div>
          <p class="caveat">
            Candidates only — a symbol reached via dynamic dispatch, an external
            consumer, a macro, or reflection has no static edge and can appear
            here as a false positive; conversely a dead symbol sharing its name
            with a used one is missed. Detection covers languages with visibility
            info: <strong>Rust, JavaScript/TypeScript, Python, Go</strong> (other
            languages report nothing here yet).
          </p>
          {#if deadExports.length === 0}
            <p class="placeholder">No candidate dead exports.</p>
          {:else}
            <div class="rows">
              {#each deadExports as d (d.file + ':' + d.line)}
                <div class="arow">
                  <span class="aname">{d.name}</span>
                  <span class="akind">{d.kind}</span>
                  <span class="aloc" title={d.signature}>{d.file}:{d.line}</span>
                </div>
              {/each}
            </div>
          {/if}
        </section>
      {/if}

      {#if cycles !== null}
        <section class="card">
          <div class="history-head">
            Import cycles <span class="muted">({cycles.length})</span>
          </div>
          <p class="caveat">
            Import resolution covers <strong>JavaScript/TypeScript, Python,
            Rust</strong>; other languages aren't analyzed for cycles yet, so an
            empty result for them means "not checked," not "cycle-free."
          </p>
          {#if cycles.length === 0}
            <p class="placeholder">No import cycles found.</p>
          {:else}
            <div class="rows">
              {#each cycles as c, i (i)}
                <div class="arow cycle">
                  {c.join(' → ')} → {c[0]}
                </div>
              {/each}
            </div>
          {/if}
        </section>
      {/if}

      {#if impact !== null}
        <section class="card">
          <div class="history-head">
            Impact of working-tree changes
            <span class="muted">({impact.changed.length} changed, {impact.dependents.length} dependent{impact.dependents.length === 1 ? '' : 's'})</span>
          </div>
          <p class="caveat">
            Approximate (name-keyed) — call edges aren't id-resolved, so this
            can both miss dynamic-dispatch callers and, more rarely, match a
            same-named symbol elsewhere. Diff vs <code>HEAD</code>; requires a
            git repository.
          </p>
          {#if impact.changed.length === 0}
            <p class="placeholder">No changes detected (working tree matches HEAD).</p>
          {:else}
            <div class="rows">
              {#each impact.changed as s (s.file + ':' + s.line)}
                <div class="arow">
                  <span class="aname">{s.name}</span>
                  <span class="akind">{s.kind}</span>
                  <span class="aloc">{s.file}:{s.line}</span>
                </div>
              {/each}
            </div>
            {#if impact.dependents.length === 0}
              <p class="placeholder">No dependents found (nothing in the index transitively calls the changed symbol(s)).</p>
            {:else}
              <div class="history-head">Dependents</div>
              <p class="caveat">
                Confidence along the discovery chain:
                <span class="conf extracted">extracted</span> (most certain) →
                <span class="conf inferred">inferred</span> →
                <span class="conf ambiguous">ambiguous</span> (least certain).
              </p>
              <div class="rows">
                {#each impact.dependents as d, i (d.file + ':' + d.line + ':' + i)}
                  <div class="arow dep">
                    <span class="aname">{d.approx ? '~' : ''}{d.name}</span>
                    <span class="akind">{d.kind}</span>
                    <span class="aloc">{d.file}:{d.line}</span>
                    <span class="muted">depth {d.depth}</span>
                    <span class="conf {d.confidence}" title="edge confidence: {d.confidence}">{d.confidence}</span>
                  </div>
                {/each}
              </div>
            {/if}
          {/if}
          {#if impact.unindexed.length > 0}
            <p class="caveat">
              Changed but not indexed ({impact.unindexed.length}): {impact.unindexed.join(', ')}
            </p>
          {/if}
        </section>
      {/if}
    </div>
{/if}

<style>
  .analyses .actions {
    margin-bottom: 12px;
  }
  .arow.cycle {
    display: block;
    font-family: monospace;
    font-size: 11.5px;
    white-space: normal;
    word-break: break-all;
  }
  .arow.dep {
    grid-template-columns: 1fr 6rem 2fr auto;
  }
  .conf.extracted {
    background: rgba(255, 255, 255, 0.08);
    color: var(--text-primary, #ddd);
    opacity: 0.75;
  }
  .conf.inferred {
    background: var(--surface-warning, rgba(178, 106, 0, 0.28));
    color: var(--text-warning, #f0c674);
  }
  .conf.ambiguous {
    background: var(--surface-danger, rgba(179, 38, 30, 0.28));
    color: var(--text-danger-soft, #ffb4ab);
  }
  .arow.dep {
    grid-template-columns: 1fr 6rem 2fr auto auto;
  }
</style>
