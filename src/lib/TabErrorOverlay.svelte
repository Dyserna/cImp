<script lang="ts">
  import type { TabId } from './tabs/types';
  import { tabErrors } from './tabs/errorState';

  // Renders an in-tab error card over the terminal when the tab's spawn
  // (or mid-session subprocess) has failed. The host Terminal owns the
  // retry path because it has the channel/sizing context; we just call
  // back to it.
  let {
    tabId,
    onretry,
  }: {
    tabId: TabId;
    onretry: () => void;
  } = $props();

  const err = $derived($tabErrors[tabId]);
  let retrying = $state(false);

  async function handleRetry() {
    if (retrying) return;
    retrying = true;
    try {
      await onretry();
    } finally {
      retrying = false;
    }
  }

  // V1.4-07: when a Claude tab fails to launch, the most common cause is
  // that `claude` isn't on PATH yet (the user hasn't installed Claude Code
  // CLI, or it's installed but not on this shell's PATH).
  const installHint = $derived.by(() => {
    if (tabId !== 'claude' && tabId !== 'claude-local') return null;
    return 'Make sure Claude Code is installed and on your PATH. Installation instructions: https://docs.anthropic.com/en/docs/claude-code/setup';
  });
</script>

{#if err}
  <div class="overlay" role="alert">
    <div class="card">
      <h2>{err.headline}</h2>
      <pre class="raw">{err.raw}</pre>
      {#if err.hint}
        <p class="hint">{err.hint}</p>
      {:else if installHint}
        <p class="hint">{installHint}</p>
      {/if}
      <div class="actions">
        <button class="primary" onclick={handleRetry} disabled={retrying}>
          {retrying ? 'Retrying…' : 'Retry'}
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
    background: rgba(0, 0, 0, 0.7);
    z-index: 5;
    padding: 24px;
    box-sizing: border-box;
  }
  .card {
    max-width: 560px;
    width: 100%;
    background: var(--surface-danger-faint);
    border: 1px solid var(--border-danger);
    color: var(--text-danger-extra);
    border-radius: var(--radius-lg);
    padding: var(--space-4) 18px;
    box-shadow: var(--shadow-lg);
    font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
  }
  h2 {
    margin: 0 0 10px 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-danger-strong);
  }
  .raw {
    margin: 0 0 10px 0;
    padding: var(--space-2) 10px;
    background: var(--surface-danger-deep);
    border: 1px solid var(--border-danger-strong);
    border-radius: var(--radius-sm);
    color: var(--text-danger-pastel);
    font-family: monospace;
    font-size: var(--font-size-xs);
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 200px;
    overflow-y: auto;
  }
  .hint {
    margin: 0 0 var(--space-3) 0;
    font-size: var(--font-size-sm);
    line-height: 1.5;
    color: var(--text-danger-mute);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }
  button {
    border: 1px solid var(--border-danger);
    background: var(--surface-danger-deep);
    color: var(--text-danger-extra);
    padding: 6px 14px;
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    cursor: pointer;
    transition: background var(--motion-fast) var(--easing-standard);
  }
  button:hover:not(:disabled) {
    background: var(--surface-danger-bg);
  }
  button.primary {
    background: var(--text-danger-faint);
    color: var(--text-on-accent);
    border-color: var(--text-danger-faint);
  }
  button.primary:hover:not(:disabled) {
    background: var(--text-danger-quiet);
  }
  button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
