<script lang="ts">
  import { avatarError, clearAvatarError } from './avatarState';
  import { acknowledgeError } from './ipc';
  import { requestTabRestart } from './settings/ipc';

  // The displayed banner is for the active tab (avatarError is a derived
  // store over (per-tab error map, activeTab)). Acknowledgement is routed
  // back to the same tab so the backend drops that tab's Error pin.
  async function handleRecover() {
    const info = $avatarError;
    if (!info) return;
    try {
      if (info.kind === 'subprocess-exited') {
        await requestTabRestart(info.tab);
      }
    } catch (e) {
      console.error('recovery failed:', e);
    }
    await acknowledgeError(info.tab).catch((e) =>
      console.error('acknowledge_error failed:', e),
    );
    clearAvatarError(info.tab);
  }

  async function handleDismiss() {
    const info = $avatarError;
    if (!info) return;
    await acknowledgeError(info.tab).catch((e) =>
      console.error('acknowledge_error failed:', e),
    );
    clearAvatarError(info.tab);
  }
</script>

{#if $avatarError}
  <div class="banner" role="alert">
    <span class="msg">{$avatarError.message}</span>
    {#if $avatarError.kind === 'subprocess-exited'}
      <button class="primary" onclick={handleRecover}>Restart</button>
    {/if}
    <button class="ghost" onclick={handleDismiss} aria-label="Dismiss">
      Dismiss
    </button>
  </div>
{/if}

<style>
  .banner {
    position: absolute;
    top: 0;
    left: 50%;
    transform: translateX(-50%);
    margin-top: var(--space-3);
    background: var(--surface-danger-bg);
    border: 1px solid var(--border-danger);
    color: var(--text-danger-extra);
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
    display: flex;
    align-items: center;
    gap: var(--space-3);
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: var(--font-size-md);
    z-index: 100;
    max-width: 80vw;
  }
  .msg {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  button {
    border: 1px solid var(--border-danger);
    background: var(--surface-danger-deep);
    color: var(--text-danger-extra);
    padding: var(--space-1) 10px;
    border-radius: var(--radius-sm);
    font-size: var(--font-size-sm);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--easing-standard);
  }
  button:hover {
    background: var(--surface-danger-bg);
  }
  button.primary {
    background: var(--text-danger-faint);
    color: var(--text-on-accent);
    border-color: var(--text-danger-faint);
  }
  button.primary:hover {
    background: var(--text-danger-quiet);
  }
  button.ghost {
    background: transparent;
  }
</style>
