<script lang="ts">
  // Configure Tab dialog. Reuses the same field component as the New
  // Shell Tab dialog. Pre-fills with the target tab's current name; the
  // dialog asks the backend for the live shell config via a new IPC
  // call (M3 will move this to settings; for M2 we read the registry).
  //
  // Save calls `reconfigure_shell_tab`; the running PTY does NOT
  // restart — per design, the new config takes effect on next restart.
  // A footer note communicates this to the user.
  import { onMount } from 'svelte';
  import { closeDialog, dialogState } from './store';
  import {
    getShellTabConfig,
    reconfigureShellTab,
    type TabLifecycleError,
  } from '../ipc';
  import ShellTabFields from './ShellTabFields.svelte';

  let name = $state('');
  let command = $state('');
  let argsString = $state('');
  let cwd = $state('');
  let notificationsError = $state('');
  let notificationsExited = $state('');
  let error = $state<TabLifecycleError | null>(null);
  let busy = $state(false);

  let isOpen = $derived($dialogState.kind === 'configure-tab');
  let targetTab = $derived(
    $dialogState.kind === 'configure-tab' ? $dialogState.tab : null,
  );

  let lastTab: string | null = null;
  $effect(() => {
    const t = targetTab;
    if (isOpen && t && t !== lastTab) {
      lastTab = t;
      void initFields(t);
    } else if (!isOpen && lastTab) {
      lastTab = null;
    }
  });

  async function initFields(tab: string): Promise<void> {
    error = null;
    busy = false;
    try {
      const cfg = await getShellTabConfig(tab);
      name = cfg.name;
      command = cfg.command;
      argsString = cfg.args;
      cwd = cfg.cwd ?? '';
      notificationsError = cfg.notifications_error;
      notificationsExited = cfg.notifications_exited;
    } catch (e) {
      console.error('get_shell_tab_config failed:', e);
      // Fall back to empty fields on read failure; the user can re-enter.
      name = '';
      command = '';
      argsString = '';
      cwd = '';
      notificationsError = '';
      notificationsExited = '';
    }
  }

  function cancel(): void {
    closeDialog();
  }

  async function save(): Promise<void> {
    if (busy || !targetTab) return;
    busy = true;
    error = null;
    try {
      await reconfigureShellTab({
        tab: targetTab,
        name,
        command,
        argsString,
        cwd: cwd.trim() === '' ? null : cwd,
        env: {},
        notificationsError,
        notificationsExited,
      });
      closeDialog();
    } catch (e) {
      const wire = e as { kind?: string } | string | null;
      if (wire && typeof wire === 'object' && 'kind' in wire) {
        error = wire as TabLifecycleError;
      } else {
        error = {
          kind: 'internal',
          message: typeof e === 'string' ? e : JSON.stringify(e),
        };
      }
    } finally {
      busy = false;
    }
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (!isOpen) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      cancel();
    } else if (e.key === 'Enter' && (e.target as HTMLElement)?.tagName !== 'BUTTON') {
      e.preventDefault();
      void save();
    }
  }

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  });
</script>

{#if isOpen}
  <div class="backdrop" onclick={cancel} role="presentation"></div>
  <div class="card" role="dialog" aria-label="Configure tab">
    <h2>Configure tab</h2>
    <ShellTabFields
      bind:name
      bind:command
      bind:argsString
      bind:cwd
      bind:notificationsError
      bind:notificationsExited
      {error}
    />
    {#if error && !['empty-name', 'command-not-found', 'cwd-not-found'].includes(error.kind)}
      <div class="generic-error">
        {#if error.kind === 'wrong-kind'}
          This tab cannot be reconfigured.
        {:else if error.kind === 'tab-not-found'}
          Tab not found.
        {:else if error.kind === 'internal'}
          {error.message}
        {:else}
          {error.kind}
        {/if}
      </div>
    {/if}
    <small class="footer-note">
      Changes apply on next shell restart.
    </small>
    <div class="actions">
      <button type="button" class="cancel" onclick={cancel} disabled={busy}>
        Cancel
      </button>
      <button type="button" class="primary" onclick={save} disabled={busy}>
        {busy ? 'Saving…' : 'Save'}
      </button>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 100;
  }
  .card {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    padding: 20px var(--space-5);
    width: 480px;
    max-width: calc(100vw - 40px);
    color: var(--text-primary);
    z-index: 101;
    box-shadow: var(--shadow-lg);
  }
  h2 {
    margin: 0 0 var(--space-4);
    font-size: 16px;
    font-weight: 600;
  }
  .footer-note {
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    display: block;
    margin-top: var(--space-2);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    margin-top: var(--space-4);
  }
  .actions button {
    padding: 6px var(--space-4);
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: var(--font-size-md);
    border: 1px solid var(--border-default);
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .actions button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .cancel {
    background: var(--surface-4);
    color: var(--text-secondary);
  }
  .cancel:hover:not([disabled]) {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  .primary {
    background: var(--accent);
    color: var(--accent-fg);
    border-color: var(--accent);
    font-weight: var(--font-weight-semibold);
  }
  .primary:hover:not([disabled]) {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }
  button[disabled] {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .generic-error {
    color: var(--danger);
    font-size: var(--font-size-sm);
    margin-top: -6px;
    margin-bottom: var(--space-2);
  }
</style>
