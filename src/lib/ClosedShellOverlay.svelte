<script lang="ts">
  import type { TabId } from './tabs/types';
  import { perTabClosedState } from './avatarState';
  import { activeTab } from './tabs/state';
  import { restartShellTab } from './ipc';

  // Shown when a Shell tab's subprocess has exited. The card displays the
  // exit code and a Restart button; pressing Enter while this tab is active
  // also triggers the restart (the parent Terminal forwards the keypress —
  // this component just owns the visible affordance + IPC call).
  let { tabId }: { tabId: TabId } = $props();

  const closedState = $derived($perTabClosedState[tabId]);
  const isActive = $derived($activeTab === tabId);
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

  // The host Terminal's keydown handler invokes this when the user presses
  // Enter while this overlay is visible. Exporting via the global `window`
  // would be ugly; instead we expose a small handler directly to the host
  // through the parent's wiring.
  export function pressedEnter() {
    if (closedState?.closed && isActive) {
      void restart();
    }
  }

  const code = $derived(closedState?.exit_code ?? null);
  const codeIsError = $derived(code !== null && code !== 0);
</script>

{#if closedState?.closed}
  <div class="overlay" role="status" aria-live="polite">
    <div class="card">
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
    background: #1a1a1a;
    border: 1px solid #555;
    color: #ddd;
    border-radius: 6px;
    padding: 18px 20px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
    font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
    text-align: center;
  }
  h2 {
    margin: 0 0 8px 0;
    font-size: 14px;
    font-weight: 600;
    color: #f0f0f0;
  }
  .detail {
    margin: 0 0 12px 0;
    font-family: monospace;
    font-size: 13px;
    color: #c0c0c0;
  }
  .detail .error {
    color: #ff8080;
    font-weight: 600;
  }
  .hint {
    margin: 0 0 14px 0;
    font-size: 12px;
    color: #a0a0a0;
  }
  .actions {
    display: flex;
    justify-content: center;
  }
  button {
    border: 1px solid #555;
    background: #2a2a2a;
    color: #ddd;
    padding: 6px 14px;
    border-radius: 4px;
    font-size: 12px;
    cursor: pointer;
  }
  button.primary {
    background: #4a6fa5;
    color: #fff;
    border-color: #4a6fa5;
  }
  button.primary:hover:not(:disabled) {
    background: #5a85c1;
  }
  button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
