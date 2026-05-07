<script lang="ts">
  import { toasts } from './toast';
</script>

{#if $toasts.length > 0}
  <div class="toast-stack">
    {#each $toasts as t (t.id)}
      <div class="toast" role="status">{t.message}</div>
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
  .toast {
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    color: var(--text-bright);
    padding: var(--space-2) var(--space-4);
    border-radius: var(--radius-md);
    font-size: var(--font-size-md);
    box-shadow: var(--shadow-md);
    pointer-events: auto;
    animation: toast-in var(--motion-base) var(--easing-standard);
  }
  @keyframes toast-in {
    from { opacity: 0; transform: translateY(8px); }
    to   { opacity: 1; transform: translateY(0); }
  }
</style>
