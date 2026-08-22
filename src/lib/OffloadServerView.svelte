<script lang="ts">
  // V8-03 / V8-04: the Offload server dashboard — a live view with one card
  // per enabled backend, grouped into Local and Remote sections. Formerly the
  // reserved Offload Server tab's content; it now renders as the "Offload
  // server" section INSIDE the Tool Activity tab (ToolActivityView.svelte),
  // which mounts it while the section is selected. Driven by the backend
  // metrics poller via the `offload-server-metrics` event (one row per
  // backend). Each card renders its own slots/throughput/history (and, for
  // Local backends, a collapsible raw server log).
  import { onMount } from 'svelte';
  import BackendDashboardCard from './BackendDashboardCard.svelte';
  import {
    offloadServerMetrics,
    onOffloadServerMetrics,
    offloadBackendStart,
    offloadBackendStop,
    offloadBackendRestart,
    type BackendDashboard,
  } from './offload';
  import { listenManaged } from './listenManaged';
  import { get } from 'svelte/store';
  import { settings } from './settings/store';
  import { openOffloadStartCommandDialog } from './dialog/store';

  // The offload tool reference list moved to the Tool Activity tab
  // (ToolActivityView.svelte), alongside the graph tools.

  let dashboards = $state<BackendDashboard[]>([]);

  // Armed at init so teardown survives an unmount during the async await.
  listenManaged(() => onOffloadServerMetrics((rows) => (dashboards = rows)));

  onMount(async () => {
    dashboards = await offloadServerMetrics();
  });

  const local = $derived(dashboards.filter((d) => d.kind === 'local'));
  const remote = $derived(dashboards.filter((d) => d.kind === 'lan' || d.kind === 'cloud'));
  // V39 Phase C: the facades get their own group rather than joining "Remote".
  // They are neither — no process to start, no endpoint to reach — and the one
  // thing a reader needs from this dashboard is which rows have a server behind
  // them. A filter written as "everything that is not local" would also have
  // swallowed the next kind silently.
  const facades = $derived(dashboards.filter((d) => d.kind === 'harness'));

  // Start/Stop/Reset for the local offload server, mirroring the per-backend
  // controls in Settings → Offload task tools. cImp normally owns a single local
  // llama-server, so these act on the first local backend. `busy` disables the
  // row while an action is in flight so a double-click can't race two lifecycle
  // ops.
  const localName = $derived(local[0]?.name ?? null);
  let busy = $state(false);

  async function runLifecycle(action: (name: string) => Promise<void>): Promise<void> {
    const name = localName;
    if (busy || !name) return;
    busy = true;
    try {
      await action(name);
    } catch (e) {
      console.error('offload local lifecycle action failed:', e);
    } finally {
      busy = false;
    }
  }

  // "Show command on start" (Settings → Offload task tools → Pool, on the local
  // backend's card): Start first
  // opens the configured server command in an editable confirm dialog
  // (OffloadStartCommandDialog, mounted in App.svelte); the edited command is
  // used for that launch only — never persisted. Reads the live settings
  // store synchronously, so there's no async gap for a double-click to race
  // through. A legacy pool-empty config has no per-backend flag → plain start.
  function onStartClick(): void {
    const name = localName;
    if (busy || !name) return;
    const backend = get(settings).offload.backends.find((b) => b.name === name);
    const kind = backend?.kind.type === 'local' ? backend.kind : null;
    if (kind?.show_command_on_start) {
      openOffloadStartCommandDialog(name, kind.server_command);
    } else {
      void runLifecycle(offloadBackendStart);
    }
  }
</script>

<div class="dash">
  {#if dashboards.length === 0}
    <div class="offline">
      <span class="dot off"></span>
      <span>No offload backend configured — add one in Settings → Offload task tools.</span>
    </div>
  {:else}
    {#if local.length > 0}
      <section class="group">
        <h3 class="group-head">Local</h3>
        <div class="lifecycle-row">
          <button
            type="button"
            class="lc-btn"
            disabled={busy || !localName}
            onclick={onStartClick}
          >Start</button>
          <button
            type="button"
            class="lc-btn secondary"
            disabled={busy || !localName}
            onclick={() => runLifecycle(offloadBackendStop)}
          >Stop</button>
          <button
            type="button"
            class="lc-btn secondary"
            disabled={busy || !localName}
            onclick={() => runLifecycle(offloadBackendRestart)}
          >Reset</button>
        </div>
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

    {#if facades.length > 0}
      <section class="group">
        <h3 class="group-head">Tab workers</h3>
        {#each facades as d (d.name)}
          <BackendDashboardCard dash={d} />
        {/each}
      </section>
    {/if}
  {/if}
</div>

<style>
  /* In normal flow: the host section (ToolActivityView) owns scrolling,
     padding, and the backdrop — the cards carry their own surfaces. */
  .dash {
    position: relative;
    font-family: var(--font-sans, system-ui, sans-serif);
    font-size: var(--font-size-md, 13px);
    color: var(--text-primary, #c9d1d9);
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
  /* Start/Stop/Reset row at the top of the Local box, above the status card. */
  .lifecycle-row {
    display: flex;
    gap: 0.5rem;
    flex-wrap: wrap;
    margin-bottom: 0.6rem;
  }
  .lc-btn {
    appearance: none;
    font: inherit;
    font-size: 0.9em;
    padding: 0.28rem 0.8rem;
    border-radius: 5px;
    border: 1px solid var(--accent, #58a6ff);
    background: var(--accent, #58a6ff);
    color: var(--accent-fg, #fff);
    cursor: pointer;
    transition:
      background 0.12s ease,
      border-color 0.12s ease,
      opacity 0.12s ease;
  }
  .lc-btn.secondary {
    background: var(--surface-sunken, #161b22);
    color: var(--text-primary, #c9d1d9);
    border-color: var(--border-default, #30363d);
  }
  .lc-btn:hover:not(:disabled) {
    filter: brightness(1.08);
  }
  .lc-btn:focus-visible {
    outline: 2px solid var(--accent, #58a6ff);
    outline-offset: 2px;
  }
  .lc-btn:disabled {
    opacity: 0.5;
    cursor: default;
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
