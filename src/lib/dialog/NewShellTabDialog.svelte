<script lang="ts">
  // New Shell Tab dialog. Modal overlay; only renders when the dialog
  // store's discriminator is `'new-shell-tab'`. Defaults are populated
  // from `default_shell_spec` on open. Submit calls `create_shell_tab`;
  // backend validation errors flow back into the shared field component.
  import { get } from 'svelte/store';
  import { closeDialog, dialogState } from './store';
  import ModalShell from './ModalShell.svelte';
  import { createShellTab, defaultShellSpec, type TabLifecycleError } from '../ipc';
  import { cancelPlacement, requestTabIntoPane } from '../layout/store';
  import { tabs } from '../tabs/store';
  import { errorMessage } from '../errors';
  import ShellTabFields from './ShellTabFields.svelte';
  import EnvEditor from '../settings/EnvEditor.svelte';

  let name = $state('');
  let command = $state('');
  let argsString = $state('');
  let cwd = $state('');
  let env = $state<Record<string, string>>({});
  let notificationsError = $state('');
  let notificationsExited = $state('');
  let error = $state<TabLifecycleError | null>(null);
  let busy = $state(false);
  let showGitBashBanner = $state(false);

  let isOpen = $derived($dialogState.kind === 'new-shell-tab');
  /// Pane the `+` button was clicked in, or null for the Ctrl+T path
  /// (which targets the focused pane via the default routing).
  let paneId = $derived($dialogState.kind === 'new-shell-tab' ? $dialogState.paneId : null);

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
    env = {};
  }

  function cancel(): void {
    closeDialog();
  }

  async function submit(): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    // Enqueue the pane placement only now that a create is actually in
    // flight (pushed before the await — the tab-created event can
    // arrive before the IPC promise resolves). Cancelled dialogs never
    // reach this point, so they can no longer leak a placement.
    const placement = paneId !== null ? requestTabIntoPane(paneId) : null;
    try {
      await createShellTab({
        name,
        command,
        argsString,
        cwd: cwd.trim() === '' ? null : cwd,
        env,
        notificationsError,
        notificationsExited,
      });
      closeDialog();
    } catch (e) {
      // The create failed, so no tab-created will consume the queued
      // placement — cancel it or the next tab created anywhere would
      // be routed into this pane.
      if (placement) cancelPlacement(placement);
      // Tauri rejects with the serde-tagged error object as-is; cast it
      // through the wire shape. Anything we can't recognize is shown as
      // a generic internal error.
      const wire = e as { kind?: string } | string | null;
      if (wire && typeof wire === 'object' && 'kind' in wire) {
        error = wire as TabLifecycleError;
      } else {
        error = {
          kind: 'internal',
          message: errorMessage(e),
        };
      }
    } finally {
      busy = false;
    }
  }
</script>

<ModalShell
  open={isOpen}
  label="New shell tab"
  title="New shell tab"
  width={480}
  onCancel={cancel}
  onEscape={cancel}
  onEnter={() => void submit()}
>
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
  <div class="env-field">
    <span class="env-label">Environment variables</span>
    <EnvEditor {env} onchange={(v) => (env = v)} />
  </div>
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
  {#snippet actions()}
    <button type="button" class="cancel" onclick={cancel} disabled={busy}>
      Cancel
    </button>
    <button type="button" class="primary" onclick={submit} disabled={busy}>
      {busy ? 'Creating…' : 'Create'}
    </button>
  {/snippet}
</ModalShell>

<style>
  .env-field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-bottom: var(--space-3);
  }
  .env-label {
    font-size: var(--font-size-sm);
    color: var(--text-quiet);
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
