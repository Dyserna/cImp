<script lang="ts">
  // Shared field UI for the New Shell Tab dialog and the Configure Tab
  // dialog. Holds four inputs (name, command, args, cwd) plus their
  // browse buttons and per-field error rendering. The parent owns the
  // values via $bindable; this component only triggers OS pickers and
  // surfaces validation messages.
  import { open as openOsDialog } from '@tauri-apps/plugin-dialog';
  import type { TabLifecycleError } from '../ipc';

  let {
    name = $bindable(),
    command = $bindable(),
    argsString = $bindable(),
    cwd = $bindable(),
    notificationsError = $bindable(),
    notificationsExited = $bindable(),
    error = null,
    showGitBashBanner = false,
  }: {
    name: string;
    command: string;
    argsString: string;
    cwd: string;
    notificationsError: string;
    notificationsExited: string;
    error?: TabLifecycleError | null;
    showGitBashBanner?: boolean;
  } = $props();

  // Windows filters the executable picker to the runnable extensions —
  // including .cmd/.bat launcher shims (npm bins, pmd.bat), which spawn fine
  // through cmd.exe. Linux executables have no extension convention, so the
  // picker stays unfiltered there.
  // We can't reliably check platform from the renderer without a Tauri
  // helper; the file dialog takes filters as a hint anyway.
  const isWindows = typeof navigator !== 'undefined'
    && navigator.userAgent.toLowerCase().includes('windows');

  async function browseCommand(): Promise<void> {
    try {
      const picked = await openOsDialog({
        directory: false,
        multiple: false,
        filters: isWindows
          ? [{ name: 'Executable', extensions: ['exe', 'cmd', 'bat', 'com'] }]
          : undefined,
      });
      if (typeof picked === 'string') {
        command = picked;
      }
    } catch (e) {
      console.error('browseCommand failed:', e);
    }
  }

  async function browseCwd(): Promise<void> {
    try {
      const picked = await openOsDialog({
        directory: true,
        multiple: false,
      });
      if (typeof picked === 'string') {
        cwd = picked;
      }
    } catch (e) {
      console.error('browseCwd failed:', e);
    }
  }

  function fieldError(field: 'name' | 'command' | 'cwd'): string | null {
    if (!error) return null;
    if (field === 'name' && error.kind === 'empty-name') {
      return 'Name cannot be empty.';
    }
    if (field === 'command' && error.kind === 'command-not-found') {
      return `Command not found: ${error.tried}`;
    }
    if (field === 'cwd' && error.kind === 'cwd-not-found') {
      return `Directory does not exist: ${error.path}`;
    }
    return null;
  }

  let nameError = $derived(fieldError('name'));
  let commandError = $derived(fieldError('command'));
  let cwdError = $derived(fieldError('cwd'));
</script>

{#if showGitBashBanner}
  <div class="banner">
    <strong>Git Bash not detected.</strong> Defaulting to PowerShell.
    Linux tools (grep, cat, nano) will not be available. Install Git for
    Windows to enable Git Bash by default, or set a custom shell below.
  </div>
{/if}

<div class="field">
  <label for="shell-tab-name">Name</label>
  <input
    id="shell-tab-name"
    type="text"
    bind:value={name}
    class:error={nameError}
  />
  {#if nameError}<small class="field-error">{nameError}</small>{/if}
</div>

<div class="field">
  <label for="shell-tab-command">Shell command</label>
  <div class="row">
    <input
      id="shell-tab-command"
      type="text"
      bind:value={command}
      class:error={commandError}
    />
    <button type="button" class="browse" onclick={browseCommand}>Browse…</button>
  </div>
  {#if commandError}<small class="field-error">{commandError}</small>{/if}
</div>

<div class="field">
  <label for="shell-tab-args">Arguments</label>
  <input
    id="shell-tab-args"
    type="text"
    bind:value={argsString}
    placeholder='e.g. --login -i'
  />
  <small class="hint">
    Use double quotes for arguments containing spaces, e.g.
    <code>--config "C:\My Folder\config.toml"</code>
  </small>
</div>

<div class="field">
  <label for="shell-tab-cwd">Working directory</label>
  <div class="row">
    <input
      id="shell-tab-cwd"
      type="text"
      bind:value={cwd}
      placeholder="(launch directory)"
      class:error={cwdError}
    />
    <button type="button" class="browse" onclick={browseCwd}>Browse…</button>
  </div>
  {#if cwdError}<small class="field-error">{cwdError}</small>{/if}
</div>

<div class="field">
  <label for="shell-tab-notif-error">Error notification text</label>
  <input
    id="shell-tab-notif-error"
    type="text"
    bind:value={notificationsError}
  />
  <small class="hint">
    Spoken when this tab errors while you're on a different tab. Leave blank
    to disable.
  </small>
</div>

<div class="field">
  <label for="shell-tab-notif-exited">Exited notification text</label>
  <input
    id="shell-tab-notif-exited"
    type="text"
    bind:value={notificationsExited}
  />
  <small class="hint">
    Spoken when this shell exits while you're on a different tab. Use
    <code>{'{code}'}</code> to insert the exit code. Leave blank to disable.
  </small>
</div>

<style>
  .banner {
    background: var(--surface-warning-deep);
    border: 1px solid var(--border-warning);
    color: var(--text-warning-bright);
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    margin-bottom: var(--space-3);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-bottom: var(--space-3);
  }
  .field label {
    font-size: var(--font-size-sm);
    color: var(--text-quiet);
  }
  .row {
    display: flex;
    gap: 6px;
  }
  .row input {
    flex: 1 1 auto;
    min-width: 0;
  }
  input[type="text"] {
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: Consolas, Menlo, monospace;
    font-size: var(--font-size-md);
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  input[type="text"]:focus {
    outline: none;
    border-color: var(--accent);
  }
  input.error {
    border-color: var(--danger);
  }
  .browse {
    background: var(--surface-4);
    border: 1px solid var(--border-default);
    color: var(--text-secondary);
    padding: 6px var(--space-3);
    border-radius: var(--radius-md);
    cursor: pointer;
    font-size: var(--font-size-sm);
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard);
  }
  .browse:hover {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  .browse:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .field-error {
    color: var(--danger);
    font-size: var(--font-size-xs);
  }
  .hint {
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
  }
  code {
    background: var(--surface-sunken);
    padding: 1px var(--space-1);
    border-radius: var(--radius-sm);
  }
</style>
