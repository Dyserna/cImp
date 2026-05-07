import { invoke, Channel } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { TabId, TabKind } from './tabs/types';

export type BytesChannel = Channel<string>;

export function createBytesChannel(): BytesChannel {
  return new Channel<string>();
}

export async function ptyStart(
  tab: TabId,
  channel: BytesChannel,
  rows: number,
  cols: number
): Promise<void> {
  await invoke('pty_start', { tab, channel, rows, cols });
}

export async function ptyRestart(
  tab: TabId,
  channel: BytesChannel,
  rows: number,
  cols: number
): Promise<void> {
  await invoke('pty_restart', { tab, channel, rows, cols });
}

export async function ptyWrite(tab: TabId, input: string): Promise<void> {
  await invoke('pty_write', { tab, input });
}

export async function ptyResize(tab: TabId, rows: number, cols: number): Promise<void> {
  await invoke('pty_resize', { tab, rows, cols });
}

export async function ttsTest(text: string): Promise<void> {
  await invoke('tts_test', { text });
}

export async function composeContentChanged(nonEmpty: boolean): Promise<void> {
  await invoke('compose_content_changed', { nonEmpty });
}

export async function acknowledgeError(tab: TabId): Promise<void> {
  await invoke('acknowledge_error', { tab });
}

export async function tabActivate(tab: TabId): Promise<void> {
  await invoke('tab_activate', { tab });
}

export interface TabMetaWire {
  id: TabId;
  kind: TabKind;
  name: string;
  builtin: boolean;
}

/// Snapshot the live tab list. Called once from `App.svelte`'s onMount to
/// seed the tabs store deterministically; runtime add/remove arrives via
/// `tab-created`/`tab-closed` events afterward.
export async function listTabs(): Promise<TabMetaWire[]> {
  return invoke<TabMetaWire[]>('list_tabs');
}

/// Wire shape of the backend's `TabLifecycleError`. Internally tagged on
/// `kind`; struct variants flatten their fields alongside.
export type TabLifecycleError =
  | { kind: 'empty-name' }
  | { kind: 'command-not-found'; tried: string }
  | { kind: 'cwd-not-found'; path: string }
  | { kind: 'tab-not-found'; tab: TabId }
  | { kind: 'builtin-not-closable' }
  | { kind: 'wrong-kind' }
  | { kind: 'spawn-failed'; message: string }
  | { kind: 'internal'; message: string };

/// Default shell + args returned by `default_shell_spec`. Args are
/// pre-joined with spaces; the dialog drops them into a text input
/// verbatim and the backend re-splits via `shlex` on submit. The two
/// `notifications_*` fields carry the platform-default notification
/// text so the New Shell Tab dialog can pre-fill them — keeping the
/// defaults source-of-truth on the backend (M4).
export interface DefaultShellWire {
  command: string;
  args: string;
  git_bash_found: boolean;
  notifications_error: string;
  notifications_exited: string;
}

export async function defaultShellSpec(): Promise<DefaultShellWire> {
  return invoke<DefaultShellWire>('default_shell_spec');
}

export interface ShellTabConfigWire {
  name: string;
  command: string;
  args: string;
  cwd: string | null;
  env: Record<string, string>;
  notifications_error: string;
  notifications_exited: string;
}

export async function getShellTabConfig(tab: TabId): Promise<ShellTabConfigWire> {
  return invoke<ShellTabConfigWire>('get_shell_tab_config', { tab });
}

// Tauri v2 converts Rust snake_case parameter names to camelCase on the
// JS side. The `argsString` field below maps to the backend's
// `args_string: String` parameter; the dialog still sends raw shell-style
// strings and the backend re-splits via `shlex` on receive.
export interface CreateShellTabInput {
  name: string;
  command: string;
  argsString: string;
  cwd: string | null;
  env: Record<string, string>;
  notificationsError: string;
  notificationsExited: string;
}

export async function createShellTab(input: CreateShellTabInput): Promise<TabId> {
  return invoke<TabId>('create_shell_tab', input as unknown as Record<string, unknown>);
}

export async function closeTab(tab: TabId): Promise<void> {
  await invoke('close_tab', { tab });
}

export async function renameTab(tab: TabId, newName: string): Promise<void> {
  await invoke('rename_tab', { tab, newName });
}

export interface ReconfigureShellTabInput {
  tab: TabId;
  name: string;
  command: string;
  argsString: string;
  cwd: string | null;
  env: Record<string, string>;
  notificationsError: string;
  notificationsExited: string;
  /// V1.4-01 per-tab terminal palette override. `null` inherits the
  /// global `terminal.theme`. The backend stamps it onto
  /// `tabs[].theme_override` in the same write that updates command/args.
  themeOverride: import('./settings/types').TerminalThemeSettings | null;
}

export async function reconfigureShellTab(input: ReconfigureShellTabInput): Promise<void> {
  await invoke('reconfigure_shell_tab', input as unknown as Record<string, unknown>);
}

/// Restart a closed Shell tab. Backend validates the tab kind/state and
/// emits `tab-restart-requested` so the terminal registry rebinds its
/// bytes channel via `pty_restart`. The `TabClosedStateChanged
/// { closed: false }` event clears the overlay once the new PTY has
/// spawned.
export async function restartShellTab(tab: TabId): Promise<void> {
  await invoke('restart_shell_tab', { tab });
}

export interface PtyExitPayload {
  tab: TabId;
  exit: string;
}

export function onPtyExit(handler: (payload: PtyExitPayload) => void): Promise<UnlistenFn> {
  return listen<PtyExitPayload>('pty-exit', (event) => handler(event.payload));
}

export function decodeBase64(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
