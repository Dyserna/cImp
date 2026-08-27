<script lang="ts">
  import { toasts } from './toast';
</script>

{#if $toasts.length > 0}
  <div class="toast-stack">
    {#each $toasts as t (t.id)}
      <div class="toast" role="status">
        <span class="msg">{t.message}</span>
        {#if t.action}
          <button type="button" class="toast-action" onclick={t.action.run}>
            {t.action.label}
          </button>
        {/if}
      </div>
    {/each}
  </div>
{/if}

<style>
  .toast-stack {
    position: fixed;
    bottom: 64px;
    left: 50%;
    transform: translateX(-50%);
    display: flex;
    flex-direction: column;
    gap: 6px;
    z-index: 300;
    pointer-events: none;
  }
  /* #151: a toast wears the NOTIFICATION colour, not the chrome accent. Every
     toast in this app is an interruption — a refusal, a hint, an error — and
     under the accent it was the same colour as the tab underline behind it. The
     border and the hairline tint carry it; the text stays `--text-bright` so a
     dark user-picked colour cannot make the message itself unreadable. The
     literal is the fallback only: `--tui-notification` is set on <html> from
     the validated setting (`themes/accent.ts::applyTuiNotification`). */
  .toast {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    background: color-mix(
      in srgb,
      var(--tui-notification, #e0af68) 10%,
      var(--surface-3)
    );
    border: 1px solid var(--tui-notification, #e0af68);
    color: var(--text-bright);
    padding: var(--space-2) var(--space-4);
    border-radius: var(--radius-md);
    font-size: var(--font-size-md);
    box-shadow: var(--shadow-md);
    pointer-events: auto;
    animation: toast-in var(--motion-base) var(--easing-standard);
  }
  /* The optional one-click follow-up (#152). Outlined in the notification
     colour rather than filled: the toast is already the notice, and a filled
     button on top of it would compete with the sentence that says why it is
     there. */
  .toast-action {
    flex: 0 0 auto;
    appearance: none;
    font-family: inherit;
    font-size: var(--font-size-sm);
    color: var(--tui-notification, #e0af68);
    background: transparent;
    border: 1px solid currentColor;
    border-radius: var(--radius-sm);
    padding: 1px 8px;
    cursor: pointer;
  }
  .toast-action:hover {
    background: color-mix(in srgb, currentColor 18%, transparent);
  }
  .toast-action:focus-visible {
    outline: 2px solid currentColor;
    outline-offset: 1px;
  }
  @keyframes toast-in {
    from { opacity: 0; transform: translateY(8px); }
    to   { opacity: 1; transform: translateY(0); }
  }
</style>
