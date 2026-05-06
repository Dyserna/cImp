<script lang="ts">
  import type { TabId } from './tabs/types';
  import { perTabClosedState } from './avatarState';
  import { activeTab } from './tabs/state';
  import { restartShellTab } from './ipc';
  import { openConfigureTabDialog } from './dialog/store';

  // Shown when a Shell tab's subprocess has exited or its launch failed.
  // The card displays the exit code (or a custom message for launch
  // failures like command-not-found) and a primary action button. The
  // host Terminal forwards the Enter keypress to `pressedEnter`; when the
  // closed state has a `closed_message`, Enter opens the Configure dialog
  // (so the user can fix the broken command) instead of attempting a
  // restart that would fail the same way.
  let { tabId }: { tabId: TabId } = $props();

  const closedState = $derived($perTabClosedState[tabId]);
  const isActive = $derived($activeTab === tabId);
  const launchFailed = $derived(
    !!closedState?.closed && !!closedState.closed_message,
  );
  let restarting = $state(false);

  async function restart() {
    if (restarting) return;
    restarting = true;
    try {
      await restartShellTab(tabId);
    } catch (e) {
      console.error('restart_shell_tab failed:', e);
    } finally {
      restarting = false;
    }
  }

  function configure() {
    openConfigureTabDialog(tabId);
  }

  // The host Terminal's keydown handler invokes this when the user presses
  // Enter while this overlay is visible. Routes to Configure for launch
  // failures, restart otherwise.
  export function pressedEnter() {
    if (!closedState?.closed || !isActive) return;
    if (launchFailed) {
      configure();
    } else {
      void restart();
    }
  }

  const code = $derived(closedState?.exit_code ?? null);
  const codeIsError = $derived(code !== null && code !== 0);
</script>

{#if closedState?.closed}
  <div class="overlay" role="status" aria-live="polite">
    <div class="card">
      {#if launchFailed}
        <h2>Shell launch failed</h2>
        <p class="detail launch-failed">{closedState.closed_message}</p>
        <p class="hint">Press Enter to configure this tab.</p>
        <div class="actions">
          <button class="primary" onclick={configure}>Configure…</button>
        </div>
      {:else}
        <h2>Shell exited</h2>
        <p class="detail">
          {#if code === null}
            (no exit code reported)
          {:else}
            exit code <span class:error={codeIsError}>{code}</span>
          {/if}
        </p>
        <p class="hint">Press Enter to restart, or close this tab.</p>
        <div class="actions">
          <button class="primary" onclick={restart} disabled={restarting}>
            {restarting ? 'Restarting…' : 'Restart'}
          </button>
        </div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .overlay {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgba(0, 0, 0, 0.75);
    z-index: 5;
    padding: 24px;
    box-sizing: border-box;
  }
  .card {
    max-width: 460px;
    width: 100%;
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    color: var(--text-primary);
    border-radius: var(--radius-lg);
    padding: 18px 20px;
    box-shadow: var(--shadow-lg);
    font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
    text-align: center;
  }
  h2 {
    margin: 0 0 var(--space-2) 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-bright);
  }
  .detail {
    margin: 0 0 var(--space-3) 0;
    font-family: monospace;
    font-size: var(--font-size-md);
    color: var(--text-secondary);
  }
  .detail.launch-failed {
    color: var(--text-danger-bright);
    font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
    font-size: var(--font-size-sm);
    line-height: 1.4;
  }
  .detail .error {
    color: var(--text-danger-bright);
    font-weight: 600;
  }
  .hint {
    margin: 0 0 14px 0;
    font-size: var(--font-size-sm);
    color: var(--text-quiet-strong);
  }
  .actions {
    display: flex;
    justify-content: center;
  }
  button {
    border: 1px solid var(--border-default);
    background: var(--surface-4);
    color: var(--text-primary);
    padding: 6px 14px;
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    cursor: pointer;
    transition: background var(--motion-fast) var(--easing-standard);
  }
  button.primary {
    background: var(--accent);
    color: var(--accent-fg);
    border-color: var(--accent);
  }
  button.primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
