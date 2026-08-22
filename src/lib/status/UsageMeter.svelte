<script lang="ts">
  // Inline harness usage tracker for the bottom status bar (right of Layouts).
  // Shows one row per quota window the harness declares — for Claude Code, the
  // session (5h) and weekly (7d) windows — each as a proportional bar, a
  // rounded percentage, a live countdown to reset, and the local reset clock
  // time. Every element is individually toggleable via `settings.usage`; the
  // whole widget hides when disabled or when there is nothing to draw.
  //
  // V40 Phase D: the windows, their labels and their durations come from the
  // BACKEND (`harness_usage` answers the harness's declared windows plus the
  // readings), so this component no longer knows that Anthropic sells a 5-hour
  // and a 7-day window. A harness with no usage source at all answers
  // `source: null` and the widget stays hidden — never a row at 0%.
  //
  // The widget ends at the reset clock: the live context/cache group that used
  // to sit to its right (NC-3) was retired, together with its `usage.show_context`
  // toggle. The push file still carries the `context_window` block and the
  // terminal status line still renders it — the app widget simply has no
  // consumer for that half any more, so nothing on the backend push path moved.
  //
  // Data path: each `cimp --statusline` run inside a Claude tab persists the
  // payload's `rate_limits` (5h/7d quota) to one push file; the backend
  // `get_claude_usage` command reads that file — a local read, no network. We
  // poll it on `usage.poll_interval_secs`; the countdown ticks locally between
  // polls.
  //
  // Absence rule: every part is independently absent-able — `rate_limits`
  // exists only for subscription auth after the first API response, and each
  // field inside it can be missing. Missing renders as "—" / an empty
  // "unknown" track, NEVER as 0%.
  import { settings } from '../settings/store';
  import {
    harnessUsage,
    type HarnessUsage,
    type UsageReading,
    type UsageSourceInfo,
  } from '../ipc';
  import { clampPct, hasQuotaData, usagePushHarness } from './contextMeter';

  // Floor on the poll cadence so a hand-edited tiny interval can't busy-poll.
  // The read is a local file, so this is UI hygiene rather than protection of
  // a remote endpoint.
  const MIN_POLL_SECS = 15;

  let reading = $state<UsageReading | null>(null);
  // What the polled harness's source CAN report. Null means it has none at all
  // — a different state from "has one, nothing reported yet", and the reason
  // the widget can stay hidden for such a harness instead of drawing zeros.
  let source = $state<UsageSourceInfo | null>(null);
  // True when every part of the reading is aging — the tabs that produced it
  // closed or went quiet. The quota half keeps its own flag (`quotaStale`):
  // the push file's two halves are written by different tabs and age
  // separately, so the widget dims on the age of the data it actually draws,
  // not on the roll-up (M14).
  let stale = $state(false);
  let quotaStale = $state(false);
  let now = $state(Date.now());

  const usage = $derived($settings.usage);
  // The widget is worth polling for whenever *some* running AI tab can push a
  // status-line reading. That is decided by the tab's command, not its id:
  // `claude-local` and any user-created claude-command tab get the same
  // statusline injection as the subscription tab (M15). What is *drawn* is
  // subscription-only and gates itself — API-key auth reports no
  // `rate_limits`, so such a tab pushes context alone and the widget stays
  // hidden (it used to show the context group for those; that group is gone).
  const pushHarness = $derived(usagePushHarness($settings.tabs, $settings.enabled_ai_tabs));
  // Derive the individual primitives the effects depend on, rather than the
  // whole `usage` object. Svelte only re-runs an effect when a value it reads
  // actually changes, so this keeps the poll/tick effects from re-arming (and
  // re-fetching) on unrelated settings edits — and collapses the
  // default→loaded settings swap at startup into a single fetch.
  const enabled = $derived(usage.enabled && pushHarness !== null);
  // Coerce a non-finite interval to the floor: `Math.max(MIN, NaN)` is NaN →
  // setTimeout(…, NaN) coerces to 0 and busy-polls the usage endpoint.
  const pollMs = $derived(
    Math.max(
      MIN_POLL_SECS,
      Number.isFinite(usage.poll_interval_secs) ? usage.poll_interval_secs : MIN_POLL_SECS,
    ) * 1000,
  );
  const showCountdown = $derived(usage.show_countdown);
  const showResetClock = $derived(usage.show_reset_clock);

  // Largest backoff between polls when the endpoint is unavailable (not a 429).
  const MAX_BACKOFF_MS = 5 * 60_000;

  // Fetch once; never throws. Returns null only on a transport error (treated
  // as "unavailable") — which now includes an unregistered harness id, since
  // the backend rejects one rather than answering empty.
  async function fetchOnce(harness: string): Promise<HarnessUsage | null> {
    try {
      return await harnessUsage(harness);
    } catch (e) {
      console.warn('usage fetch failed:', e);
      return null;
    }
  }

  // Poll loop over the local push file (via the backend command).
  //   - fresh push → show snapshot undimmed, poll at pollMs.
  //   - aging push (`stale`) → show snapshot dimmed; the Claude tab that fed
  //     it has closed or gone quiet.
  //   - no data (snapshot null) → hide; poll at the normal cadence so the
  //     widget appears within one interval of a Claude tab's first push.
  //   - thrown transport / IPC error → keep last-good, back off exponentially.
  $effect(() => {
    if (!enabled || pushHarness === null) {
      reading = null;
      source = null;
      stale = false;
      quotaStale = false;
      return;
    }
    const harness = pushHarness;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let failures = 0;
    const tick = async () => {
      const result = await fetchOnce(harness);
      if (cancelled) return;
      let delay: number;
      if (!result) {
        // A thrown transport / IPC error. Keep last-good on screen; back off.
        failures += 1;
        delay = Math.min(pollMs * 2 ** Math.min(failures, 5), MAX_BACKOFF_MS);
      } else {
        failures = 0;
        source = result.source;
        // `reading: null` is a genuine 'nothing reported / expired' — hide,
        // rather than leave a stale reading on screen indefinitely.
        reading = result.reading;
        stale = result.reading?.stale ?? false;
        // Per-section, because the push file's halves come from different tabs
        // on different clocks; `stale` is only the whole-file roll-up, and the
        // context half of it has no consumer here any more.
        quotaStale = result.reading?.quota_stale ?? false;
        delay = pollMs;
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

  // Percentage *text*. Clamped through the same helper the bars use, so a
  // payload reporting 143% can never print a number its own bar contradicts
  // (the terminal renderer clamps identically).
  function pct(u: number): number {
    return Math.round(clampPct(u));
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

  // Quota data is absent entirely under API-key auth (no `rate_limits` in the
  // push at all) — the rows are then dropped rather than drawn as a column of
  // placeholders.
  const showQuota = $derived(hasQuotaData(reading));

  // Show the widget when the quota group has something to draw. Hidden until a
  // tab pushes its first reading, and again once the last push expires.
  const visible = $derived(showQuota);

  // The declared windows, in declared order, joined to their readings. A window
  // the harness declares but has no reading for renders as a hollow "not
  // reported" track rather than a confident 0% — the same absence rule the
  // backend keeps by omitting it from `reading.windows`. Label and duration are
  // separate columns so the (5h)/(7d) suffixes align across rows.
  const windowsList = $derived(
    (source?.windows ?? []).map((decl) => ({
      name: decl.label,
      dur: decl.short,
      full: decl.description,
      w: reading?.windows.find((r) => r.id === decl.id) ?? null,
    })),
  );

  // The bottom strip is as tall as the stacked usage rows it has to fit — two
  // for Claude Code's 5h + 7d pair, which is where the pre-V40 hard-coded 44px
  // came from. Declared rather than assumed, so a harness with three windows
  // grows the strip instead of overflowing it. Left at the stylesheet's default
  // when nothing declares windows: an empty strip is not a reason to reflow the
  // whole window, and the registry-driven layout is Phase F's.
  $effect(() => {
    const rows = source?.windows.length ?? 0;
    if (rows > 0) {
      document.documentElement.style.setProperty('--status-bar-rows', String(rows));
    }
  });
</script>

{#if enabled && visible}
  <div
    class="usage-meter"
    title={stale
      ? 'Harness usage — last known (no recent report from a running tab)'
      : 'Harness usage'}
  >
    {#if showQuota}
      <!-- label column: name + duration in their own tracks so (5h)/(7d)
           line up across the two rows. Dimmed on the quota slot's own age
           (`quotaStale`), not the push file's roll-up. -->
      <div class="ug label" class:dim={quotaStale}>
        {#each windowsList as r}
          <span class="name" title={r.full}>{r.name}</span>
          <span class="dur">{r.dur}</span>
        {/each}
      </div>
      {#if usage.show_bar}
        <div class="ug" class:dim={quotaStale}>
          {#each windowsList as r}
            <!-- No window ⇒ an "unknown" track with no fill: a 0%-wide fill
                 on a normal track would read as a genuine 0%. -->
            <span class="bar" class:unknown={!r.w} title={r.w ? undefined : 'not reported'}>
              {#if r.w}
                <span class="fill" style="width: {clampPct(r.w.used)}%"></span>
              {/if}
            </span>
          {/each}
        </div>
      {/if}
      {#if usage.show_percentage}
        <div class="ug" class:dim={quotaStale}>
          {#each windowsList as r}
            <span class="pct">{r.w ? pct(r.w.used) + '%' : '—'}</span>
          {/each}
        </div>
      {/if}
      {#if usage.show_percentage && (usage.show_countdown || usage.show_reset_clock)}
        <span class="vdiv" aria-hidden="true"></span>
      {/if}
      {#if usage.show_countdown}
        <div class="ug" class:dim={quotaStale}>
          {#each windowsList as r}
            <span class="cd"
              >{r.w?.resets_at ? 'resets in: ' + fmtCountdown(r.w.resets_at, now) : '—'}</span
            >
          {/each}
        </div>
      {/if}
      {#if usage.show_countdown && usage.show_reset_clock}
        <span class="vdiv" aria-hidden="true"></span>
      {/if}
      {#if usage.show_reset_clock}
        <div class="ug" class:dim={quotaStale}>
          {#each windowsList as r}
            <span class="clk"
              >{r.w?.resets_at
                ? (usage.show_countdown ? '@ ' : 'resets @ ') + fmtResetClock(r.w.resets_at, now)
                : ''}</span
            >
          {/each}
        </div>
      {/if}
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
  /* Aging numbers: dimmed so they read as "may be out of date" without
     hiding the data. Driven by the quota slot's own age rather than the push
     file's roll-up, because the file's halves are pushed by different tabs and
     age on their own clocks — dimming on the roll-up was the old, misleading
     behavior. */
  .dim {
    opacity: 0.55;
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
  /* "Not reported" track: hollow and outlined rather than an empty-but-solid
     bar, so absent data can't be mistaken for a genuine 0%. */
  .bar.unknown {
    background: transparent;
    border: 1px dashed var(--border-subtle);
    opacity: 0.7;
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
