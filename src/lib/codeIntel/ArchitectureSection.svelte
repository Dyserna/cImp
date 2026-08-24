<script lang="ts">
  /// Code Intelligence → Architecture (#130, F4). God nodes, subsystems and
  /// surprising cross-subsystem edges — V15 Feature 2, heuristic and advisory.
  ///
  /// Self-contained: nothing outside this section ever read `arch`, `archBusy`,
  /// `archError` or `runArchitecture`, and no poll drives it — the results
  /// exist only after the user clicks Recompute.
  ///
  /// `active` gates the MARKUP, and the parent mounts this component
  /// unconditionally, because the results have always outlived a section
  /// switch: they lived in the parent's script, which `{#if section === …}`
  /// never touched. An `{#if}` around the component would throw away a
  /// recompute the moment someone looked at another section.
  ///
  /// Styles: `codeIntel.css` is keyed on the parent's `.graph-monitor` class
  /// and this renders inside it, so `.card` / `.rows` / `.arow` / `.aname` /
  /// `.aloc` / `.muted` / `.caveat` / `.placeholder` / `.error` / `.actions`
  /// reach here through the DOM. Only the rules no other section can reach —
  /// `.arow.god`, `.arow.surprising` and the `.subsys*` family — travelled.
  import { graphArchitecture, type ArchResult } from '../graph';

  let {
    active,
  }: {
    /// Whether Architecture is the section on screen. Gates the markup only.
    active: boolean;
  } = $props();

  // V15 Feature 2: the "Architecture" section — god nodes, subsystems,
  // surprising (cross-subsystem) edges. Heuristic, advisory only.
  let arch = $state<ArchResult | null>(null);
  let archBusy = $state(false);
  let archError = $state<string | null>(null);

  async function runArchitecture(): Promise<void> {
    archBusy = true;
    archError = null;
    try {
      arch = await graphArchitecture();
    } catch (e) {
      archError = String(e);
    } finally {
      archBusy = false;
    }
  }
</script>

{#if active}
    <div class="arch-sec">
      <p class="caveat">
        Heuristic system-shape overview — hub degree + label-propagation
        clustering. Advisory, not authoritative; verify before acting on it.
      </p>
      <div class="actions">
        <button onclick={runArchitecture} disabled={archBusy}>
          {archBusy ? 'Analyzing…' : 'Recompute'}
        </button>
      </div>

      {#if archError}
        <p class="error">{archError}</p>
      {/if}

      {#if arch}
        <section class="card">
          <div class="history-head">God nodes <span class="muted">({arch.god_nodes.length})</span></div>
          <p class="caveat">Hubs the system flows through.</p>
          {#if arch.god_nodes.length === 0}
            <p class="placeholder">No standout hubs found.</p>
          {:else}
            <div class="rows">
              {#each arch.god_nodes as g (g.id)}
                <div class="arow god">
                  <span class="aname">{g.label}</span>
                  <span class="akind">{g.kind}</span>
                  <span class="aloc">{g.file}</span>
                  <span class="muted">degree {g.degree}</span>
                </div>
              {/each}
            </div>
          {/if}
        </section>

        <section class="card">
          <div class="history-head">Subsystems <span class="muted">({arch.subsystems.length})</span></div>
          {#if arch.subsystems.length === 0}
            <p class="placeholder">Single cohesive module — no distinct subsystems detected.</p>
          {:else}
            <div class="subsys-list">
              {#each arch.subsystems as s (s.name)}
                <details class="subsys">
                  <summary>{s.name} — {s.size} file{s.size === 1 ? '' : 's'} · hub {s.hub}</summary>
                  <div class="subsys-files">
                    {#each s.files as f (f)}
                      <div class="aloc">{f}</div>
                    {/each}
                  </div>
                </details>
              {/each}
            </div>
          {/if}
        </section>

        <section class="card">
          <div class="history-head">
            Surprising connections <span class="muted">({arch.surprising.length})</span>
          </div>
          <p class="caveat">
            Candidate accidental coupling — heuristic, verify before acting.
          </p>
          {#if arch.surprising.length === 0}
            <p class="placeholder">No cross-subsystem surprises found.</p>
          {:else}
            <div class="rows">
              {#each arch.surprising as s, i (s.from + ':' + s.to + ':' + i)}
                <div class="arow surprising">
                  <span class="aname">{s.from_subsystem} ✗ {s.to_subsystem}</span>
                  <span class="aloc">{s.from} ──{s.kind}──▶ {s.to}</span>
                </div>
              {/each}
            </div>
          {/if}
        </section>
      {/if}
    </div>
{/if}

<style>
  .arow.god {
    grid-template-columns: 1fr 6rem 2fr auto;
  }
  .arow.surprising {
    grid-template-columns: 1fr 2fr;
    white-space: normal;
  }

  /* ── V15 Feature 2: Architecture ───────────────────────────────────────── */
  .subsys-list {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .subsys {
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
    padding: 4px 2px;
    font-size: 12px;
  }
  .subsys summary {
    cursor: pointer;
    font-weight: 600;
  }
  .subsys-files {
    margin: 6px 0 4px 1.2em;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
</style>
