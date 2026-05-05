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

  // For aider we can name the upstream — there's a stable installation page
  // and the most common failure is "not on PATH". For other tabs we keep it
  // generic.
  const installHint = $derived.by(() => {
    if (tabId !== 'aider') return null;
    return 'Make sure aider is installed and on your PATH. Installation instructions: https://aider.chat';
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
    background: #1f1414;
    border: 1px solid #a85050;
    color: #f0d0d0;
    border-radius: 6px;
    padding: 16px 18px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
    font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
  }
  h2 {
    margin: 0 0 10px 0;
    font-size: 14px;
    font-weight: 600;
    color: #ffd0d0;
  }
  .raw {
    margin: 0 0 10px 0;
    padding: 8px 10px;
    background: #150a0a;
    border: 1px solid #6a3030;
    border-radius: 4px;
    color: #f0c0c0;
    font-family: monospace;
    font-size: 11px;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 200px;
    overflow-y: auto;
  }
  .hint {
    margin: 0 0 12px 0;
    font-size: 12px;
    line-height: 1.5;
    color: #e0c0c0;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  button {
    border: 1px solid #a85050;
    background: #2a1515;
    color: #f0d0d0;
    padding: 6px 14px;
    border-radius: 4px;
    font-size: 12px;
    cursor: pointer;
  }
  button:hover:not(:disabled) {
    background: #3a1f1f;
  }
  button.primary {
    background: #a85050;
    color: #fff;
    border-color: #a85050;
  }
  button.primary:hover:not(:disabled) {
    background: #c46060;
  }
  button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
