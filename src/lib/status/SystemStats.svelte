<script lang="ts">
  // Inline system-monitor panel for the bottom status bar, right of the
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
  // `Math.max(1, NaN)` is NaN → setTimeout(…, NaN) coerces to 0 and busy-polls.
  // Coerce a non-finite interval to the floor first.
  const pollMs = $derived(
    Math.max(1, Number.isFinite(stats.poll_interval_secs) ? stats.poll_interval_secs : 1) * 1000,
  );
  const netMax = $derived(netHist.length ? Math.max(...netHist, 1) : 1);

  // Each visible "column" is a stacked pair: CPU/RAM, GPU/VRAM, NET
  // graph/speed. Dividers are drawn only between two visible columns.
  const showCpuRam = $derived(stats.show_cpu || stats.show_memory);
  const showGpuVram = $derived(stats.show_gpu);
  const showNet = $derived(stats.show_network);

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
  <!-- Three stacked "columns", each a 2-row mini-grid that self-aligns its
       own top/bottom lines. Top line = usage %/graphs; bottom line =
       capacity/throughput:
         CPU% / graph     GPU% / graph (+temp)     NET label / graph
         RAM% / bar       VRAM% / bar              speed (under the label)
       Short dividers sit between visible columns; the full-height frame
       around the whole panel is drawn by the status-bar slot. -->
  <div class="sysmon" title="System monitor">
    {#if showCpuRam}
      <!-- [label][value][graph]; label/value share columns so CPU%↔RAM%
           line up, and the bar stretches to end where the graph ends. -->
      <div class="group ab">
        {#if stats.show_cpu}
          <span class="lbl">CPU</span>
          <span class="val">{pct(snapshot.cpu_pct)}%</span>
          {@render spark(cpuHist, 100)}
        {:else}
          {@render empty(3)}
        {/if}
        {#if stats.show_memory}
          <span class="lbl">RAM</span>
          <span class="val">{pct(snapshot.mem_pct)}%</span>
          {@render bar(snapshot.mem_pct)}
        {:else}
          {@render empty(3)}
        {/if}
      </div>
    {/if}

    {#if showCpuRam && showGpuVram}
      <span class="vdiv" aria-hidden="true"></span>
    {/if}

    {#if showGpuVram}
      <!-- [label][value][graph][temp]; GPU%↔VRAM% share the value column,
           and the VRAM bar spans the graph+temp columns so it ends where
           the temperature ends. -->
      <div class="group gpu">
        {#if snapshot.gpu}
          <span class="lbl">GPU</span>
          <span class="val">{pct(snapshot.gpu.util_pct)}%</span>
          {@render spark(gpuHist, 100)}
          {#if stats.show_gpu_temp}
            <span class="temp">{Math.round(snapshot.gpu.temp_c)}°C</span>
          {:else}
            {@render empty(1)}
          {/if}
          <span class="lbl">VRAM</span>
          <span class="val">{pct(snapshot.gpu.mem_pct)}%</span>
          {@render bar(snapshot.gpu.mem_pct)}
        {:else}
          <span class="lbl na">GPU</span>
          <span class="val na">n/a</span>
          {@render empty(2)}
          {@render empty(4)}
        {/if}
      </div>
    {/if}

    {#if (showCpuRam || showGpuVram) && showNet}
      <span class="vdiv" aria-hidden="true"></span>
    {/if}

    {#if showNet}
      <div class="group net">
        <span class="net-top">
          <span class="lbl">NET</span>
          {@render spark(netHist, netMax)}
        </span>
        <span class="net">
          <span class="net-dl">↓{fmtBps(snapshot.net.down_bps)}</span>
          <span class="net-ul">↑{fmtBps(snapshot.net.up_bps)}</span>
        </span>
      </div>
    {/if}
  </div>
{/if}

<style>
  /* A flex row of stacked "columns" (CPU/RAM, GPU/VRAM, NET) separated by
     short dividers. Each column self-aligns its own two lines. */
  .sysmon {
    display: inline-flex;
    flex-direction: row;
    align-items: center;
    gap: var(--space-2);
    font-size: 11px;
    line-height: 1;
    color: var(--text-secondary);
    white-space: nowrap;
    user-select: none;
  }
  /* One column = a 2-row mini-grid: top line (usage) over bottom line
     (capacity). Label and value sit in their own tracks so the two rows'
     percentages line up; the graph/bar share the last track(s). */
  .group {
    display: grid;
    align-items: center;
    justify-items: start;
    column-gap: var(--space-2);
    row-gap: var(--space-1);
  }
  /* CPU/RAM: [label][value][graph] */
  .group.ab {
    grid-template-columns: max-content max-content max-content;
  }
  /* GPU/VRAM: [label][value][graph][temp] */
  .group.gpu {
    grid-template-columns: max-content max-content max-content max-content;
  }
  /* VRAM bar spans the graph + temp tracks so it ends under the temp. */
  .group.gpu .bar {
    grid-column: span 2;
  }
  /* NET column stacks "NET + graph" over the speed, with the speed
     left-aligned so it sits directly under the NET label. */
  .group.net {
    display: inline-flex;
    flex-direction: column;
    align-items: stretch;
    justify-content: center;
    row-gap: var(--space-1);
  }
  .net-top {
    display: flex;
    align-items: center;
    gap: 4px;
  }
  /* The NET graph stretches to fill the column width (set by the speed
     below), so the top line ends where the speed ends. */
  .group.net .spark {
    flex: 1 1 0;
    min-width: 0;
    width: auto;
  }
  /* Short vertical divider between two visible columns. Deliberately
     shorter than the panel height so the columns still read as one
     component (the full-height frame is the status-bar slot border). */
  .vdiv {
    flex: 0 0 auto;
    width: 1px;
    height: 1.8em;
    background: var(--border-subtle);
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
  /* Reserve room for the widest reading ("100%", "100°C") and right-align,
     so a single↔double↔triple digit change doesn't reflow the grid and
     shift the panel. */
  .val {
    display: inline-block;
    min-width: 4ch;
    text-align: right;
  }
  .temp {
    display: inline-block;
    min-width: 5ch;
    text-align: right;
  }
  /* Down and up each get a fixed slot so a change in the download width
     can't shove the upload sideways, and the two together give the column
     a stable width (which also fixes the graph length above). Sized to the
     common high range; left-aligned so the arrows stay put. */
  .net {
    display: inline-flex;
    gap: var(--space-2);
  }
  .net-dl,
  .net-ul {
    display: inline-block;
    min-width: 10.5ch;
    text-align: left;
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
  /* Stretch to fill its grid track(s): in CPU/RAM that's the graph column
     (so the bar ends exactly where the graph above it ends); in GPU it
     spans graph+temp (so it ends where the temperature ends). min-width
     keeps it from collapsing if the graph above is toggled off. */
  .bar {
    position: relative;
    display: block;
    width: auto;
    justify-self: stretch;
    min-width: 36px;
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
