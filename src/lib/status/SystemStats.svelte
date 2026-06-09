<script lang="ts">
  // Inline system-monitor panel for the bottom status bar, right of the Claude
  // usage meter. Two lines on the (already two-line) bar:
  //   Top:    CPU % + sparkline + temperature(°C) + memory % + bar
  //   Bottom: GPU % + sparkline + VRAM % + bar + network sparkline + ↓/↑ speed
  //
  // Polls the backend `get_system_stats` command (default every 1s) and keeps
  // local ring buffers to draw the sparklines. Same keep-last-good + backoff
  // resilience as UsageMeter so a transient failure doesn't blank the panel.
  import { settings } from '../settings/store';
  import { getSystemStats, type SystemStatsSnapshot } from '../ipc';

  // Sparkline dimensions (viewBox units; CSS sizes the box).
  const SPARK_W = 40;
  const SPARK_H = 14;
  // How many samples each sparkline retains.
  const CAP = 40;
  // Largest backoff between polls when the command keeps failing.
  const MAX_BACKOFF_MS = 60_000;

  let snapshot = $state<SystemStatsSnapshot | null>(null);
  let cpuHist = $state<number[]>([]);
  let gpuHist = $state<number[]>([]);
  let netHist = $state<number[]>([]);

  const stats = $derived($settings.system_stats);
  const enabled = $derived(stats.enabled);
  const pollMs = $derived(Math.max(1, stats.poll_interval_secs) * 1000);
  const netMax = $derived(netHist.length ? Math.max(...netHist, 1) : 1);

  function pushHist(arr: number[], v: number): void {
    arr.push(v);
    if (arr.length > CAP) arr.shift();
  }

  async function fetchOnce(): Promise<SystemStatsSnapshot | null> {
    try {
      return await getSystemStats();
    } catch (e) {
      console.warn('system stats fetch failed:', e);
      return null;
    }
  }

  // Poll loop: keep-last-good on failure + exponential backoff so a transient
  // error doesn't blank the panel or hammer the backend.
  $effect(() => {
    if (!enabled) {
      snapshot = null;
      cpuHist = [];
      gpuHist = [];
      netHist = [];
      return;
    }
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let failures = 0;
    const tick = async () => {
      const r = await fetchOnce();
      if (cancelled) return;
      if (r) {
        snapshot = r;
        pushHist(cpuHist, r.cpu_pct);
        pushHist(gpuHist, r.gpu ? r.gpu.util_pct : 0);
        pushHist(netHist, r.net.down_bps + r.net.up_bps);
        failures = 0;
      } else {
        failures += 1;
      }
      const delay = r
        ? pollMs
        : Math.min(pollMs * 2 ** Math.min(failures, 5), MAX_BACKOFF_MS);
      timer = setTimeout(tick, delay);
    };
    void tick();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  });

  function pct(v: number): number {
    return Math.round(v);
  }

  // Bytes/sec → compact human string.
  function fmtBps(b: number): string {
    if (b < 1024) return `${Math.round(b)} B/s`;
    if (b < 1024 * 1024) return `${(b / 1024).toFixed(b < 10 * 1024 ? 1 : 0)} KB/s`;
    if (b < 1024 * 1024 * 1024) return `${(b / 1024 / 1024).toFixed(1)} MB/s`;
    return `${(b / 1024 / 1024 / 1024).toFixed(2)} GB/s`;
  }

  // Build the polyline `points` for a sparkline normalized to `max`.
  function sparkPoints(values: number[], max: number): string {
    const n = values.length;
    const m = Math.max(max, 1);
    const stepX = n > 1 ? SPARK_W / (n - 1) : 0;
    return values
      .map((v, i) => {
        const x = i * stepX;
        const y = SPARK_H - Math.min(1, Math.max(0, v / m)) * SPARK_H;
        return `${x.toFixed(1)},${y.toFixed(1)}`;
      })
      .join(' ');
  }
</script>

{#snippet spark(values: number[], max: number)}
  <svg
    class="spark"
    viewBox="0 0 {SPARK_W} {SPARK_H}"
    preserveAspectRatio="none"
    aria-hidden="true"
  >
    {#if values.length > 1}
      <polyline points={sparkPoints(values, max)} vector-effect="non-scaling-stroke" />
    {/if}
  </svg>
{/snippet}

{#snippet bar(value: number)}
  <span class="bar"><span class="fill" style="width: {Math.min(100, Math.max(0, value))}%"></span></span>
{/snippet}

<!-- Placeholder cells that hold grid columns for hidden components so the two
     rows stay aligned (CPU↔GPU, MEM↔VRAM, etc.) regardless of which toggles
     are on. -->
{#snippet empty(n: number)}
  {#each Array.from({ length: n }) as _unused}
    <span aria-hidden="true"></span>
  {/each}
{/snippet}

{#if enabled && snapshot}
  <!-- Two lines as one 8-column grid so both rows align into columns:
       usage(label+%) · graph · temp · mem(label+%) · bar · net-label · net-graph · net-speed.
       Each component is gated by its settings toggle; hidden ones emit empty
       cells (above) to preserve the column alignment. The top (CPU/MEM) line
       leaves the trailing 3 network columns empty — network is bottom-line only. -->
  <div class="sysmon" title="System monitor">
    <span class="line">
      {#if stats.show_cpu}
        <span class="metric"><span class="lbl">CPU</span> <span class="val">{pct(snapshot.cpu_pct)}%</span></span>
        {@render spark(cpuHist, 100)}
      {:else}
        {@render empty(2)}
      {/if}
      <!-- CPU has no temperature; this column is always blank on the top line
           (it aligns under the GPU-temp column below). -->
      {@render empty(1)}
      {#if stats.show_memory}
        <span class="metric"><span class="lbl">MEM</span> <span class="val">{pct(snapshot.mem_pct)}%</span></span>
        {@render bar(snapshot.mem_pct)}
      {:else}
        {@render empty(2)}
      {/if}
      {@render empty(3)}
    </span>
    <span class="line">
      {#if stats.show_gpu}
        {#if snapshot.gpu}
          <span class="metric"><span class="lbl">GPU</span> <span class="val">{pct(snapshot.gpu.util_pct)}%</span></span>
          {@render spark(gpuHist, 100)}
          {#if stats.show_gpu_temp}
            <span class="temp">{Math.round(snapshot.gpu.temp_c)}°C</span>
          {:else}
            {@render empty(1)}
          {/if}
          <span class="metric"><span class="lbl">VRAM</span> <span class="val">{pct(snapshot.gpu.mem_pct)}%</span></span>
          {@render bar(snapshot.gpu.mem_pct)}
        {:else}
          <span class="metric na">GPU n/a</span>
          {@render empty(4)}
        {/if}
      {:else}
        {@render empty(5)}
      {/if}
      {#if stats.show_network}
        <span class="metric"><span class="lbl">NET</span></span>
        {@render spark(netHist, netMax)}
        <span class="net">↓{fmtBps(snapshot.net.down_bps)} ↑{fmtBps(snapshot.net.up_bps)}</span>
      {:else}
        {@render empty(3)}
      {/if}
    </span>
  </div>
{/if}

<style>
  /* Left border doubles as the divider from the usage meter; only present
     when the panel renders. */
  /* Both lines share one grid so columns line up across rows (GPU under CPU,
     VRAM under MEM, graphs/temps/bars aligned). 8 fixed columns. */
  .sysmon {
    display: grid;
    grid-template-columns: repeat(8, max-content);
    align-items: center;
    justify-items: start;
    column-gap: var(--space-2);
    row-gap: var(--space-1);
    margin-left: var(--space-3);
    padding-left: var(--space-3);
    border-left: 1px solid var(--border-subtle);
    font-size: 11px;
    line-height: 1;
    color: var(--text-secondary);
    white-space: nowrap;
    user-select: none;
  }
  /* Dissolve the per-line wrapper so each line's cells become grid items
     participating in the shared column tracks. */
  .line {
    display: contents;
  }
  .metric {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .lbl {
    font-weight: 600;
    color: var(--accent);
  }
  .val,
  .temp,
  .net {
    color: var(--text-primary);
    font-variant-numeric: tabular-nums;
  }
  .na {
    opacity: 0.7;
  }
  .spark {
    width: 40px;
    height: 14px;
    display: block;
  }
  .spark polyline {
    fill: none;
    stroke: var(--accent);
    stroke-width: 1;
  }
  .bar {
    position: relative;
    display: inline-block;
    width: 36px;
    height: 6px;
    background: var(--surface-3);
    border-radius: var(--radius-pill);
    overflow: hidden;
  }
  .fill {
    position: absolute;
    left: 0;
    top: 0;
    bottom: 0;
    border-radius: var(--radius-pill);
    background: var(--accent);
    transition: width var(--motion-fast, 200ms) linear;
  }
</style>
