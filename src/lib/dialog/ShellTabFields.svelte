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

  // Windows uses .exe to filter the executable picker. Linux executables
  // have no extension convention, so the picker stays unfiltered there.
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
          ? [{ name: 'Executable', extensions: ['exe'] }]
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
    background: #3a2a1a;
    border: 1px solid #5a4020;
    color: #e0c090;
    padding: 8px 12px;
    border-radius: 4px;
    font-size: 12px;
    margin-bottom: 12px;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-bottom: 12px;
  }
  .field label {
    font-size: 12px;
    color: #b0b0b0;
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
    background: #1f1f1f;
    border: 1px solid #444;
    color: #e0e0e0;
    padding: 6px 8px;
    border-radius: 3px;
    font-family: Consolas, Menlo, monospace;
    font-size: 13px;
  }
  input[type="text"]:focus {
    outline: none;
    border-color: #4a90e2;
  }
  input.error {
    border-color: #e74c3c;
  }
  .browse {
    background: #2a2a2a;
    border: 1px solid #444;
    color: #c0c0c0;
    padding: 6px 12px;
    border-radius: 3px;
    cursor: pointer;
    font-size: 12px;
  }
  .browse:hover {
    background: #383838;
    color: #e0e0e0;
  }
  .field-error {
    color: #e74c3c;
    font-size: 11px;
  }
  .hint {
    color: #808080;
    font-size: 11px;
  }
  code {
    background: #1f1f1f;
    padding: 1px 4px;
    border-radius: 2px;
  }
</style>
