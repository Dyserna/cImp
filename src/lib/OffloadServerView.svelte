<script lang="ts">
  // V8-03 / V8-04: the read-only Offload Server tab's content — a live
  // dashboard with one card per enabled backend, grouped into Local and
  // Remote sections. Driven by the backend metrics poller via the
  // `offload-server-metrics` event (one row per backend). Each card renders
  // its own slots/throughput/history (and, for Local backends, a collapsible
  // raw server log).
  import { onMount, onDestroy } from 'svelte';
  import BackendDashboardCard from './BackendDashboardCard.svelte';
  import {
    offloadServerMetrics,
    onOffloadServerMetrics,
    type BackendDashboard,
  } from './offload';

  let dashboards = $state<BackendDashboard[]>([]);
  let unlistenMetrics: (() => void) | null = null;

  onMount(async () => {
    dashboards = await offloadServerMetrics();
    unlistenMetrics = await onOffloadServerMetrics((rows) => (dashboards = rows));
  });

  onDestroy(() => {
    unlistenMetrics?.();
  });

  const local = $derived(dashboards.filter((d) => d.kind === 'local'));
  const remote = $derived(dashboards.filter((d) => d.kind === 'lan' || d.kind === 'cloud'));
</script>

<div class="dash">
  {#if dashboards.length === 0}
    <div class="offline">
      <span class="dot off"></span>
      <span>No offload backend configured — add one in Settings → Offload.</span>
    </div>
  {:else}
    {#if local.length > 0}
      <section class="group">
        <h3 class="group-head">Local</h3>
        {#each local as d (d.name)}
          <BackendDashboardCard dash={d} />
        {/each}
      </section>
    {/if}

    {#if remote.length > 0}
      <section class="group">
        <h3 class="group-head">Remote</h3>
        {#each remote as d (d.name)}
          <BackendDashboardCard dash={d} />
        {/each}
      </section>
    {/if}
  {/if}
</div>

<style>
  .dash {
    position: absolute;
    inset: 0;
    overflow: auto;
    padding: 0.8rem 1rem;
    font-family: var(--font-sans, system-ui, sans-serif);
    font-size: var(--font-size-md, 13px);
    color: var(--text-primary, #c9d1d9);
    background: var(--surface-0, #0d1117);
  }
  .offline {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    color: var(--text-secondary, #8b949e);
    padding: 1rem 0;
  }
  .group {
    margin-bottom: 1.1rem;
  }
  .group-head {
    margin: 0 0 0.5rem;
    font-size: 0.78em;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-secondary, #8b949e);
    border-bottom: 1px solid var(--border-subtle, #21262d);
    padding-bottom: 0.3rem;
  }
  .group :global(.card + .card) {
    margin-top: 0.6rem;
  }
  .dot {
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 50%;
    flex: 0 0 auto;
    display: inline-block;
    background: var(--text-secondary, #8b949e);
  }
  .dot.off {
    background: var(--danger, #d08770);
  }
</style>
