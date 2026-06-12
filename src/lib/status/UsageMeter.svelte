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

  // The two quota windows in display order. `w` is null while we have no
  // data yet (rate-limited at startup) — cells render "—" placeholders so
  // the layout stays stable and fills in once a fetch succeeds. Name and
  // duration are split so each column ((5h)/(7d) included) aligns across
  // both rows.
  const windowsList = $derived([
    {
      name: 'current session',
      dur: '(5h)',
      full: 'Rolling 5-hour session quota',
      w: snapshot?.five_hour ?? null,
    },
    {
      name: 'weekly session',
      dur: '(7d)',
      full: 'Rolling 7-day weekly quota',
      w: snapshot?.seven_day ?? null,
    },
  ]);
</script>

{#if usage.enabled && visible}
  <div
    class="usage-meter"
    title={rateLimited && !snapshot ? 'Claude Code usage — rate limited, retrying…' : 'Claude Code usage'}
  >
    <!-- label column: name + duration in their own tracks so (5h)/(7d)
         line up across the two rows. -->
    <div class="ug label">
      {#each windowsList as r}
        <span class="name" title={r.full}>{r.name}</span>
        <span class="dur">{r.dur}</span>
      {/each}
    </div>
    {#if usage.show_bar}
      <div class="ug">
        {#each windowsList as r}
          <span class="bar">
            <span
              class="fill"
              style="width: {r.w ? Math.min(100, Math.max(0, r.w.utilization)) : 0}%"
            ></span>
          </span>
        {/each}
      </div>
    {/if}
    {#if usage.show_percentage}
      <div class="ug">
        {#each windowsList as r}
          <span class="pct">{r.w ? pct(r.w.utilization) + '%' : '—'}</span>
        {/each}
      </div>
    {/if}
    {#if usage.show_percentage && (usage.show_countdown || usage.show_reset_clock)}
      <span class="vdiv" aria-hidden="true"></span>
    {/if}
    {#if usage.show_countdown}
      <div class="ug">
        {#each windowsList as r}
          <span class="cd">{r.w?.resets_at ? 'resets in: ' + fmtCountdown(r.w.resets_at, now) : '—'}</span>
        {/each}
      </div>
    {/if}
    {#if usage.show_countdown && usage.show_reset_clock}
      <span class="vdiv" aria-hidden="true"></span>
    {/if}
    {#if usage.show_reset_clock}
      <div class="ug">
        {#each windowsList as r}
          <span class="clk"
            >{r.w?.resets_at
              ? (usage.show_countdown ? '@ ' : 'resets @ ') + fmtResetClock(r.w.resets_at, now)
              : ''}</span
          >
        {/each}
      </div>
    {/if}
  </div>
{/if}

<style>
  /* A flex row of stacked columns (label, bar, %, countdown, clock) with
     short dividers between groups. Each column is a 2-row grid: the 5h
     window over the 7d window, so the two rows line up per column. */
  .usage-meter {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 11px;
    line-height: 1;
    color: var(--text-secondary);
    white-space: nowrap;
    user-select: none;
  }
  .ug {
    display: grid;
    grid-auto-flow: row;
    /* Uniform row height across every column so the two rows line up
       panel-wide (the bar column has no text of its own to set its row
       height) and each bar centers in an identical box — otherwise the
       short bar rows center independently and land on fractional pixels,
       making the bottom bar look thinner and off-centre. Kept close to the
       11px text height so the two lines sit as tight as the system-stats
       panel rather than spaced apart. */
    grid-auto-rows: 1.1em;
    align-items: center;
    justify-items: start;
    row-gap: var(--space-1);
  }
  /* Label column splits name + duration into two tracks so the (5h)/(7d)
     suffixes line up across the two rows regardless of name length. */
  .ug.label {
    grid-template-columns: max-content max-content;
    column-gap: 4px;
  }
  /* Short vertical divider between groups; deliberately shorter than the
     panel height so the columns still read as one component. */
  .vdiv {
    flex: 0 0 auto;
    width: 1px;
    height: 1.8em;
    background: var(--border-subtle);
  }
  .name,
  .dur {
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
    /* Reserve room for "100%" so a digit change doesn't widen the column
       and shift the panel. */
    min-width: 4ch;
    text-align: right;
  }
  .cd {
    font-variant-numeric: tabular-nums;
    color: var(--text-primary);
  }
  .clk {
    color: var(--text-secondary);
  }
</style>
