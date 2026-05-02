<script lang="ts">
  import { avatarError } from './avatarState';
  import { acknowledgeError } from './ipc';
  import { requestClaudeCodeRestart } from './settings/ipc';

  // Recovery action varies by kind: SubprocessExited gets a restart button;
  // TTS/audio errors are advisory and dismissable. We don't auto-clear on
  // backend recovery (e.g. successful TTS retry) — those clear themselves
  // once state leaves Error, which the avatar listener handles.
  async function handleRecover() {
    const info = $avatarError;
    if (!info) return;
    try {
      if (info.kind === 'subprocess-exited') {
        await requestClaudeCodeRestart();
      }
    } catch (e) {
      console.error('recovery failed:', e);
    }
    await acknowledgeError().catch((e) =>
      console.error('acknowledge_error failed:', e),
    );
  }

  async function handleDismiss() {
    await acknowledgeError().catch((e) =>
      console.error('acknowledge_error failed:', e),
    );
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
    margin-top: 12px;
    background: #3a1f1f;
    border: 1px solid #a85050;
    color: #f0d0d0;
    padding: 8px 12px;
    border-radius: 4px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.4);
    display: flex;
    align-items: center;
    gap: 12px;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 13px;
    z-index: 100;
    max-width: 80vw;
  }
  .msg {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  button {
    border: 1px solid #a85050;
    background: #2a1515;
    color: #f0d0d0;
    padding: 4px 10px;
    border-radius: 3px;
    font-size: 12px;
    cursor: pointer;
  }
  button:hover {
    background: #3a1f1f;
  }
  button.primary {
    background: #a85050;
    color: #fff;
    border-color: #a85050;
  }
  button.primary:hover {
    background: #c46060;
  }
  button.ghost {
    background: transparent;
  }
</style>
