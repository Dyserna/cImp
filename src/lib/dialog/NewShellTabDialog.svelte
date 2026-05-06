<script lang="ts">
  // New Shell Tab dialog. Modal overlay; only renders when the dialog
  // store's discriminator is `'new-shell-tab'`. Defaults are populated
  // from `default_shell_spec` on open. Submit calls `create_shell_tab`;
  // backend validation errors flow back into the shared field component.
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { closeDialog, dialogState } from './store';
  import { createShellTab, defaultShellSpec, type TabLifecycleError } from '../ipc';
  import { tabs } from '../tabs/store';
  import ShellTabFields from './ShellTabFields.svelte';

  let name = $state('');
  let command = $state('');
  let argsString = $state('');
  let cwd = $state('');
  let notificationsError = $state('');
  let notificationsExited = $state('');
  let error = $state<TabLifecycleError | null>(null);
  let busy = $state(false);
  let showGitBashBanner = $state(false);

  let isOpen = $derived($dialogState.kind === 'new-shell-tab');

  /// Reset + populate defaults when the dialog opens. We re-fetch
  /// defaults each time so a Git Bash install/uninstall during the
  /// session is reflected.
  let lastOpenSeen = false;
  $effect(() => {
    if (isOpen && !lastOpenSeen) {
      lastOpenSeen = true;
      void initFields();
    } else if (!isOpen && lastOpenSeen) {
      lastOpenSeen = false;
    }
  });

  async function initFields(): Promise<void> {
    error = null;
    busy = false;
    const shellCount = get(tabs).filter((m) => !m.builtin).length;
    name = `Shell ${shellCount + 1}`;
    try {
      const spec = await defaultShellSpec();
      command = spec.command;
      argsString = spec.args;
      notificationsError = spec.notifications_error;
      notificationsExited = spec.notifications_exited;
      // Banner only on Windows when Git Bash detection failed.
      const isWindows = typeof navigator !== 'undefined'
        && navigator.userAgent.toLowerCase().includes('windows');
      showGitBashBanner = isWindows && !spec.git_bash_found;
    } catch (e) {
      console.error('default_shell_spec failed:', e);
      command = '';
      argsString = '';
      // Hardcoded fallbacks mirror the backend's ShellNotificationConfig::default()
      // — only used if the IPC fetch failed (rare).
      notificationsError = 'Shell encountered an error';
      notificationsExited = 'Shell exited (code {code})';
      showGitBashBanner = false;
    }
    cwd = '';
  }

  function cancel(): void {
    closeDialog();
  }

  async function submit(): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await createShellTab({
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
      // Tauri rejects with the serde-tagged error object as-is; cast it
      // through the wire shape. Anything we can't recognize is shown as
      // a generic internal error.
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
      // Enter on any input submits — convention for short modal forms.
      e.preventDefault();
      void submit();
    }
  }

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  });
</script>

{#if isOpen}
  <div class="backdrop" onclick={cancel} role="presentation"></div>
  <div class="card" role="dialog" aria-label="New shell tab">
    <h2>New shell tab</h2>
    <ShellTabFields
      bind:name
      bind:command
      bind:argsString
      bind:cwd
      bind:notificationsError
      bind:notificationsExited
      {error}
      {showGitBashBanner}
    />
    {#if error && !['empty-name', 'command-not-found', 'cwd-not-found'].includes(error.kind)}
      <div class="generic-error">
        {#if error.kind === 'spawn-failed'}
          Failed to spawn: {error.message}
        {:else if error.kind === 'internal'}
          {error.message}
        {:else}
          {error.kind}
        {/if}
      </div>
    {/if}
    <div class="actions">
      <button type="button" class="cancel" onclick={cancel} disabled={busy}>
        Cancel
      </button>
      <button type="button" class="primary" onclick={submit} disabled={busy}>
        {busy ? 'Creating…' : 'Create'}
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
    margin-bottom: 8px;
  }
</style>
