<script lang="ts">
  // Inline Claude Code usage tracker for the bottom status bar (right of
  // Layouts). Shows the session (5h) and weekly (7d) quota windows, each as a
  // proportional bar, a rounded percentage, a live countdown to reset, and the
  // local reset clock time. Every element is individually toggleable via
  // `settings.usage`; the whole widget hides when disabled or when no data
  // exists (no Claude tab has pushed a quota reading yet, or the last one
  // expired).
  //
  // NC-3: the same push also carries the live context-window reading, shown
  // as a second group (context used% + tokens, and the turn's cache
  // read/creation split). Historical per-turn cache stats live on the
  // transcript/graph path (Code Intelligence) — this group is the live
  // snapshot only.
  //
  // Data path: each `cimp --statusline` run inside a Claude tab persists the
  // payload's `rate_limits` (5h/7d quota) and `context_window` block to one
  // push file; the backend `get_claude_usage` command reads that file — a
  // local read, no network. We poll it on `usage.poll_interval_secs`; the
  // countdown ticks locally between polls.
  //
  // Absence rule (both groups): every part is independently absent-able —
  // `rate_limits` exists only for subscription auth after the first API
  // response, the context block only on a new enough Claude Code, and each
  // field inside either can be missing. Missing renders as "—" / an empty
  // "unknown" track, NEVER as 0%.
  import { settings } from '../settings/store';
  import { getClaudeUsage, type UsageResult, type UsageSnapshot } from '../ipc';
  import {
    cacheHitPct,
    cacheSplitLabel,
    clampPct,
    claudePushTabActive,
    contextAttribution,
    contextTitle,
    contextTokensLabel,
    contextUsedPct,
    hasContextData,
    hasQuotaData,
    humanizeTokens,
  } from './contextMeter';

  // Floor on the poll cadence so a hand-edited tiny interval can't busy-poll.
  // The read is a local file, so this is UI hygiene rather than protection of
  // a remote endpoint.
  const MIN_POLL_SECS = 15;
  // Legacy (retired endpoint-poll path): on a 429, wait this many normal
  // intervals before the next poll. The push path never reports a rate-limit,
  // so this branch is inert — kept alongside the backend's disabled poller in
  // case that data source is ever resurrected.
  const RATE_LIMIT_BACKOFF = 5;

  let snapshot = $state<UsageSnapshot | null>(null);
  // Legacy: true when the last fetch was a 429 (endpoint-poll era). Always
  // false under the push path.
  let rateLimited = $state(false);
  // True when every part of `snapshot` is an aging push — the Claude tabs that
  // produced it closed or went quiet. The two halves are written by different
  // tabs and age separately, so each also has its own flag (M14).
  let stale = $state(false);
  let quotaStale = $state(false);
  let contextStale = $state(false);
  let now = $state(Date.now());

  const usage = $derived($settings.usage);
  // The widget is worth polling for whenever *some* running AI tab can push a
  // status-line reading. That is decided by the tab's command, not its id:
  // `claude-local` and any user-created claude-command tab get the same
  // statusline injection as the subscription tab (M15). The quota half is
  // still subscription-only, but it gates itself — API-key auth reports no
  // `rate_limits`, so those tabs simply push context alone.
  const claudePushTabEnabled = $derived(
    claudePushTabActive($settings.tabs, $settings.enabled_ai_tabs),
  );
  // Derive the individual primitives the effects depend on, rather than the
  // whole `usage` object. Svelte only re-runs an effect when a value it reads
  // actually changes, so this keeps the poll/tick effects from re-arming (and
  // re-fetching) on unrelated settings edits — and collapses the
  // default→loaded settings swap at startup into a single fetch.
  const enabled = $derived(usage.enabled && claudePushTabEnabled);
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
  // `show_context` is additive with a serde default, so a settings file
  // written before NC-3 has no such key — treat a missing value as on rather
  // than as off, otherwise the row silently never appears for existing users.
  const showContextSetting = $derived(usage.show_context !== false);

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

  // Poll loop over the local push file (via the backend command).
  //   - fresh push → show snapshot undimmed, poll at pollMs.
  //   - aging push (`stale`) → show snapshot dimmed; the Claude tab that fed
  //     it has closed or gone quiet.
  //   - no data (snapshot null) → hide; poll at the normal cadence so the
  //     widget appears within one interval of a Claude tab's first push.
  //   - thrown transport / IPC error → keep last-good, back off exponentially.
  //   - the rate-limit branch below is legacy from the endpoint-poll era and
  //     can no longer trigger; see RATE_LIMIT_BACKOFF above.
  $effect(() => {
    if (!enabled) {
      snapshot = null;
      rateLimited = false;
      stale = false;
      quotaStale = false;
      contextStale = false;
      return;
    }
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let failures = 0;
    const tick = async () => {
      const result = await fetchOnce();
      if (cancelled) return;
      let delay: number;
      if (!result) {
        // A thrown transport / IPC error. Keep last-good on screen; back off.
        failures += 1;
        delay = Math.min(pollMs * 2 ** Math.min(failures, 5), MAX_BACKOFF_MS);
      } else {
        failures = 0;
        // Adopt any snapshot the backend returned — fresh (200) or the cached
        // last-good it serves (flagged stale) during a rate-limit / hiccup.
        if (result.snapshot) {
          snapshot = result.snapshot;
        } else if (!result.rate_limited) {
          // Genuine 'unavailable / not logged in' with no cache — hide.
          snapshot = null;
        }
        stale = result.stale;
        // Per-section, because the two halves come from different tabs on
        // different clocks; `stale` is only the whole-widget roll-up.
        quotaStale = result.quota_stale;
        contextStale = result.context_stale;
        rateLimited = result.rate_limited;
        if (result.rate_limited) {
          // Back off to 5× the normal cadence, but never retry before the
          // server's stated Retry-After — that guarantees recovery even when a
          // short configured interval × 5 is still inside the cooldown.
          const ra = (result.retry_after_secs ?? 0) * 1000;
          delay = Math.max(pollMs * RATE_LIMIT_BACKOFF, ra);
        } else {
          delay = pollMs;
        }
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

  // Live context reading + the derived figures the group renders. All the
  // absence/ratio logic lives in `contextMeter.ts` so it is unit-tested.
  const ctx = $derived(snapshot?.context ?? null);
  const showContext = $derived(showContextSetting && hasContextData(ctx));
  const cacheHit = $derived(cacheHitPct(ctx));
  // `used_percentage`, or `100 − remaining_percentage` when only that was
  // reported — the field otherwise has no reader at all.
  const ctxUsed = $derived(contextUsedPct(ctx));
  const ctxTokens = $derived(contextTokensLabel(ctx));
  const cacheSplit = $derived(cacheSplitLabel(ctx));
  const ctxTitle = $derived(contextTitle(ctx));
  // Which session these numbers belong to. Visible, not just in the tooltip:
  // any Claude tab can own the context slot, so a fresh-looking reading may
  // still be the other tab's (M14).
  const ctxWho = $derived(contextAttribution(ctx));

  // Quota data can be absent while context data is present (API-key auth
  // reports no `rate_limits` at all) — then the quota rows are dropped
  // entirely rather than drawn as a column of placeholders. The rate-limited
  // half is legacy and can no longer trigger.
  const showQuota = $derived(hasQuotaData(snapshot) || (rateLimited && !snapshot));

  // Show the widget when either group has something to draw. Hidden until a
  // Claude tab pushes its first reading, and again once the last push expires.
  const visible = $derived(showQuota || showContext);

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

{#if enabled && visible}
  <div
    class="usage-meter"
    title={rateLimited && !snapshot
      ? 'Claude Code usage — rate limited, retrying…'
      : stale
        ? 'Claude Code usage — last known (no recent report from a Claude tab)'
        : 'Claude Code usage'}
  >
    {#if showQuota}
      <!-- label column: name + duration in their own tracks so (5h)/(7d)
           line up across the two rows. Dimming is per group, not per widget:
           the quota half can be aging while the context half is live. -->
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
                <span class="fill" style="width: {clampPct(r.w.utilization)}%"></span>
              {/if}
            </span>
          {/each}
        </div>
      {/if}
      {#if usage.show_percentage}
        <div class="ug" class:dim={quotaStale}>
          {#each windowsList as r}
            <span class="pct">{r.w ? pct(r.w.utilization) + '%' : '—'}</span>
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
    {#if showQuota && showContext}
      <span class="vdiv" aria-hidden="true"></span>
    {/if}
    {#if showContext}
      <!-- NC-3 context group: row 1 = context window, row 2 = the latest
           turn's prompt-cache split. Same 2-row grid as the quota columns.
           Dimmed on the context slot's own age, not the widget's. -->
      <div class="ug label" class:dim={contextStale} title={ctxTitle}>
        <span class="name">context</span>
        <span class="dur">({humanizeTokens(ctx?.context_window_size)})</span>
        <span class="name">cache</span>
        <span class="dur">(turn)</span>
      </div>
      {#if usage.show_bar}
        <div class="ug" class:dim={contextStale}>
          <span
            class="bar"
            class:unknown={ctxUsed == null}
            title={ctxUsed == null ? 'not reported' : 'context window in use'}
          >
            {#if ctxUsed != null}
              <span class="fill" style="width: {clampPct(ctxUsed)}%"></span>
            {/if}
          </span>
          <span
            class="bar"
            class:unknown={cacheHit == null}
            title={cacheHit == null
              ? 'not reported'
              : 'share of this turn’s input tokens served from cache'}
          >
            {#if cacheHit != null}
              <span class="fill" style="width: {clampPct(cacheHit)}%"></span>
            {/if}
          </span>
        </div>
      {/if}
      {#if usage.show_percentage}
        <div class="ug" class:dim={contextStale}>
          <span class="pct">{ctxUsed != null ? pct(ctxUsed) + '%' : '—'}</span>
          <span class="pct">{cacheHit != null ? pct(cacheHit) + '%' : '—'}</span>
        </div>
      {/if}
      <div class="ug" class:dim={contextStale}>
        <span class="fig">{ctxTokens ?? '—'}</span>
        <span class="fig">{cacheSplit ?? '—'}</span>
      </div>
      {#if ctxWho}
        <!-- Which session the reading belongs to. Any Claude tab can own the
             context slot, so this is the only on-screen way to notice that a
             confident-looking number is the *other* tab's. -->
        <span class="who" class:dim={contextStale} title={ctxTitle}>{ctxWho}</span>
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
     hiding the data. Applied per group (quota / context) rather than to the
     whole widget, because the two halves are pushed by different Claude tabs
     and age on their own clocks — dimming both when only one is old was the
     old, misleading behavior. */
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
  /* Context/cache token figures ("25k/200k", "read 20k · new 5k"). */
  .fig {
    font-variant-numeric: tabular-nums;
    color: var(--text-primary);
  }
  /* Session attribution for the context group — secondary weight: it is an
     identifier, not a number, and must not compete with the figures. */
  .who {
    color: var(--text-secondary);
    font-style: italic;
    max-width: 14ch;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
