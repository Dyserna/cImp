<script lang="ts">
  // Inline Claude Code usage tracker for the bottom status bar (right of
  // Layouts). Shows the session (5h) and weekly (7d) quota windows, each as a
  // proportional bar, a rounded percentage, a live countdown to reset, and the
  // local reset clock time. Every element is individually toggleable via
  // `settings.usage`; the whole widget hides when disabled or when usage can't
  // be fetched (not logged into Claude / endpoint unreachable).
  //
  // Data comes from the backend `get_claude_usage` command, which GETs an
  // undocumented Anthropic oauth/usage endpoint. We poll on
  // `usage.poll_interval_secs`; the countdown ticks locally between polls.
  import { settings } from '../settings/store';
  import {
    getClaudeUsage,
    type UsageResult,
    type UsageSnapshot,
    type UsageWindow,
  } from '../ipc';

  // Floor on the poll cadence so a hand-edited tiny interval can't hammer the
  // undocumented endpoint.
  const MIN_POLL_SECS = 15;
  // Cooldown after a 429 when the server didn't send a usable Retry-After.
  const RATE_LIMIT_COOLDOWN_MS = 2 * 60_000;

  let snapshot = $state<UsageSnapshot | null>(null);
  // True when the last fetch was a 429. Keeps the widget visible (with stale /
  // placeholder data) during a rate-limit instead of hiding it.
  let rateLimited = $state(false);
  let now = $state(Date.now());

  const usage = $derived($settings.usage);
  // Derive the individual primitives the effects depend on, rather than the
  // whole `usage` object. Svelte only re-runs an effect when a value it reads
  // actually changes, so this keeps the poll/tick effects from re-arming (and
  // re-fetching) on unrelated settings edits — and collapses the
  // default→loaded settings swap at startup into a single fetch.
  const enabled = $derived(usage.enabled);
  const pollMs = $derived(Math.max(MIN_POLL_SECS, usage.poll_interval_secs) * 1000);
  const showCountdown = $derived(usage.show_countdown);
  const showResetClock = $derived(usage.show_reset_clock);

  // Largest backoff between polls when the endpoint is unavailable (not a 429).
  const MAX_BACKOFF_MS = 5 * 60_000;

  // Fetch once; never throws. Returns null only on a transport error (treated
  // as "unavailable").
  async function fetchOnce(): Promise<UsageResult | null> {
    try {
      return await getClaudeUsage();
    } catch (e) {
      console.warn('usage fetch failed:', e);
      return null;
    }
  }

  // Poll loop. Three outcomes:
  //   - success → show the snapshot, clear rate-limit, poll again at pollMs.
  //   - 429 → keep the widget visible (last-good or placeholder), and wait the
  //     server's Retry-After (or a fixed cooldown) before retrying. This is not
  //     an error backoff — we honor the server's cadence, not exponential.
  //   - unavailable (no token / network) → keep last-good if any, else the
  //     widget hides; retry with exponential backoff.
  $effect(() => {
    if (!enabled) {
      snapshot = null;
      rateLimited = false;
      return;
    }
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let failures = 0;
    const tick = async () => {
      const result = await fetchOnce();
      if (cancelled) return;
      let delay: number;
      if (result && result.snapshot) {
        snapshot = result.snapshot;
        rateLimited = false;
        failures = 0;
        delay = pollMs;
      } else if (result && result.rate_limited) {
        rateLimited = true; // keep last-good snapshot on screen
        failures = 0;
        const ra = result.retry_after_secs ?? 0;
        delay = ra > 0 ? Math.max(pollMs, ra * 1000) : RATE_LIMIT_COOLDOWN_MS;
      } else {
        rateLimited = false;
        failures += 1;
        delay = Math.min(pollMs * 2 ** Math.min(failures, 5), MAX_BACKOFF_MS);
      }
      timer = setTimeout(tick, delay);
    };
    void tick();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  });

  // Local clock tick that drives the countdown (and keeps the reset clock's
  // today/weekday decision fresh). Per-second while the countdown shows;
  // coarser when only the reset clock needs it; off entirely otherwise.
  $effect(() => {
    if (!enabled) return;
    if (!showCountdown && !showResetClock) return;
    const period = showCountdown ? 1000 : 30000;
    const id = setInterval(() => (now = Date.now()), period);
    return () => clearInterval(id);
  });

  function pct(u: number): number {
    return Math.round(u);
  }

  function fmtCountdown(resetsAt: string | null, nowMs: number): string {
    if (!resetsAt) return '';
    const t = new Date(resetsAt).getTime();
    if (Number.isNaN(t)) return '';
    let s = Math.max(0, Math.floor((t - nowMs) / 1000));
    const d = Math.floor(s / 86400);
    s -= d * 86400;
    const h = Math.floor(s / 3600);
    s -= h * 3600;
    const m = Math.floor(s / 60);
    if (d > 0) return `${d}d ${h}h`;
    if (h > 0) return `${h}h ${m}m`;
    if (m > 0) return `${m}m`;
    return '<1m';
  }

  function fmtResetClock(resetsAt: string | null, nowMs: number): string {
    if (!resetsAt) return '';
    const dt = new Date(resetsAt);
    if (Number.isNaN(dt.getTime())) return '';
    const time = dt.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    const today = new Date(nowMs);
    const sameDay =
      dt.getFullYear() === today.getFullYear() &&
      dt.getMonth() === today.getMonth() &&
      dt.getDate() === today.getDate();
    if (sameDay) return time;
    const wd = dt.toLocaleDateString([], { weekday: 'short' });
    return `${wd} ${time}`;
  }

  // Show the widget when we have data OR we're rate-limited (placeholder /
  // stale). Stays hidden only when genuinely unavailable with no prior data
  // (e.g. not logged into Claude).
  const visible = $derived(!!snapshot || rateLimited);

  // Number of grid columns each window row emits (label + whichever elements
  // are enabled). Both rows emit the same set, so the grid's columns size to
  // the widest cell and the two rows line up — bars included — regardless of
  // label length.
  const colCount = $derived(
    1 +
      (usage.show_bar ? 1 : 0) +
      (usage.show_percentage ? 1 : 0) +
      (usage.show_countdown ? 1 : 0) +
      (usage.show_reset_clock ? 1 : 0),
  );
</script>

{#snippet windowView(label: string, full: string, w: UsageWindow | null)}
  <!-- `w` is null while we have no data yet (e.g. rate-limited at startup):
       render the same cells with "—" placeholders so the layout is stable and
       fills in once a fetch succeeds. -->
  <span class="uw">
    <span class="uw-label" title={full}>{label}</span>
    {#if usage.show_bar}
      <span class="bar">
        <span
          class="fill"
          style="width: {w ? Math.min(100, Math.max(0, w.utilization)) : 0}%"
        ></span>
      </span>
    {/if}
    {#if usage.show_percentage}
      <span class="pct">{w ? pct(w.utilization) + '%' : '—'}</span>
    {/if}
    {#if usage.show_countdown}
      <span class="cd">{w?.resets_at ? 'resets in: ' + fmtCountdown(w.resets_at, now) : '—'}</span>
    {/if}
    {#if usage.show_reset_clock}
      <span class="clk"
        >{w?.resets_at
          ? (usage.show_countdown ? 'at ' : 'resets at ') + fmtResetClock(w.resets_at, now)
          : ''}</span
      >
    {/if}
  </span>
{/snippet}

{#if usage.enabled && visible}
  <div
    class="usage-meter"
    title={rateLimited && !snapshot ? 'Claude Code usage — rate limited, retrying…' : 'Claude Code usage'}
  >
    <div class="windows" style="grid-template-columns: repeat({colCount}, max-content);">
      {@render windowView('current session (5h)', 'Rolling 5-hour session quota', snapshot?.five_hour ?? null)}
      {@render windowView('weekly session (7d)', 'Rolling 7-day weekly quota', snapshot?.seven_day ?? null)}
    </div>
  </div>
{/if}

<style>
  .usage-meter {
    display: inline-flex;
    align-items: center;
    gap: var(--space-3);
    font-size: 11px;
    line-height: 1;
    color: var(--text-secondary);
    white-space: nowrap;
    user-select: none;
  }
  /* The two windows stack as two grid rows sharing one set of columns, so
     every column (label, bar, %, reset text) lines up across both rows
     regardless of label length. Column count is set inline from `colCount`. */
  .windows {
    display: grid;
    grid-auto-flow: row;
    align-items: center;
    justify-items: start;
    column-gap: var(--space-2);
    row-gap: var(--space-1);
  }
  /* `display: contents` dissolves the per-window wrapper so each window's
     cells become direct grid items of `.windows` and participate in the
     shared column tracks. */
  .uw {
    display: contents;
  }
  .uw-label {
    font-weight: 600;
    color: var(--accent);
  }
  .bar {
    position: relative;
    display: inline-block;
    width: 44px;
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
    transition: width var(--motion-fast, 120ms) linear;
  }
  .pct {
    font-variant-numeric: tabular-nums;
    color: var(--text-primary);
    justify-self: end;
    /* Reserve room for "100%" so a digit change doesn't widen the column
       and shift the panel. */
    min-width: 4ch;
    text-align: right;
  }
  /* Extra breathing room so the "when it resets" group (countdown + clock)
     reads as separate from the "how full" group (label + bar + %). */
  .cd {
    font-variant-numeric: tabular-nums;
    margin-left: var(--space-1);
  }
  .clk {
    color: var(--text-secondary);
  }
</style>
