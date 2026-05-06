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
    background: rgba(40, 40, 40, 0.95);
    border: 1px solid #555;
    color: #f0f0f0;
    padding: 8px 16px;
    border-radius: 4px;
    font-size: 13px;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.5);
    pointer-events: auto;
    animation: toast-in 0.16s ease;
  }
  @keyframes toast-in {
    from { opacity: 0; transform: translateY(8px); }
    to   { opacity: 1; transform: translateY(0); }
  }
</style>
