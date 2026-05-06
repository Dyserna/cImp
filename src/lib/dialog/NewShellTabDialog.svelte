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
      // Banner only on Windows when Git Bash detection failed.
      const isWindows = typeof navigator !== 'undefined'
        && navigator.userAgent.toLowerCase().includes('windows');
      showGitBashBanner = isWindows && !spec.git_bash_found;
    } catch (e) {
      console.error('default_shell_spec failed:', e);
      command = '';
      argsString = '';
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
    background: #2a2a2a;
    border: 1px solid #444;
    border-radius: 6px;
    padding: 20px 24px;
    width: 480px;
    max-width: calc(100vw - 40px);
    color: #e0e0e0;
    z-index: 101;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.6);
  }
  h2 {
    margin: 0 0 16px;
    font-size: 16px;
    font-weight: 600;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .actions button {
    padding: 6px 16px;
    border-radius: 3px;
    cursor: pointer;
    font-size: 13px;
    border: 1px solid #444;
  }
  .cancel {
    background: #2a2a2a;
    color: #c0c0c0;
  }
  .cancel:hover:not([disabled]) {
    background: #383838;
  }
  .primary {
    background: #4a90e2;
    color: white;
    border-color: #4a90e2;
  }
  .primary:hover:not([disabled]) {
    background: #5aa0f2;
  }
  button[disabled] {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .generic-error {
    color: #e74c3c;
    font-size: 12px;
    margin-top: -6px;
    margin-bottom: 8px;
  }
</style>
