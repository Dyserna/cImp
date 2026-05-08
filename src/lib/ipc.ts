import { invoke, Channel } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { TabId, TabKind } from './tabs/types';

export type BytesChannel = Channel<string>;

export function createBytesChannel(): BytesChannel {
  return new Channel<string>();
}

/// V1.4-04 D.5: `pty_start` now returns the persisted-scrollback bytes
/// (if any) from the previous session. The caller writes them to the
/// new xterm before binding the live channel so the user sees their
/// last shell output above a fresh prompt. Returns `null` when:
///   - `terminal.scrollback.restore_on_launch` is `false`
///   - no persisted file exists (cold install, or already consumed
///     earlier in this session)
///   - the persisted-file read failed (logged backend-side; the
///     spawn proceeds either way)
///
/// Tauri serializes Rust `Vec<u8>` as `number[]`. Callers convert to
/// `Uint8Array` + `TextDecoder` to feed `term.write`.
export async function ptyStart(
  tab: TabId,
  channel: BytesChannel,
  rows: number,
  cols: number,
): Promise<number[] | null> {
  return invoke<number[] | null>('pty_start', { tab, channel, rows, cols });
}

export async function ptyRestart(
  tab: TabId,
  channel: BytesChannel,
  rows: number,
  cols: number
): Promise<void> {
  await invoke('pty_restart', { tab, channel, rows, cols });
}

/// V1.4-03: re-point a still-running PTY's bytes at a fresh channel.
/// Used by the renderer-flip recreate path so the shell session, env,
/// cwd, and running processes survive the xterm.js destroy/create
/// cycle. Errors when the PTY isn't registered or has already exited
/// — the caller handles the fallback to `ptyStart`.
export async function ptyRebindChannel(
  tab: TabId,
  channel: BytesChannel,
): Promise<void> {
  await invoke('pty_rebind_channel', { tab, channel });
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
  /// V1.4-03 per-tab terminal background override. Three-state:
  ///   `null`        → inherit global `terminal.background`
  ///   `'disabled'`  → opt this tab out, even if global has an image/color
  ///   `{...config}` → use this config for this tab specifically
  /// Stamped onto `tabs[].background_override` in the same write.
  backgroundOverride:
    | import('./settings/types').BackgroundOverrideWire
    | null;
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

/// Apply a new `claude_tabs_enabled` value: opens / closes the AI builtin
/// tabs as needed. The backend kills the PTY and drops scrollback for
/// any newly-disabled tab; newly-enabled tabs spawn fresh on the next
/// frontend mount. Switches the active tab off any soon-to-be-removed
/// tab onto the surviving Claude tab so the avatar/TTS gate doesn't
/// dangle.
export async function setClaudeTabsEnabled(
  value: import('./settings/types').ClaudeTabsEnabled,
): Promise<void> {
  await invoke('set_claude_tabs_enabled', { value });
}

/// Open `<exe-dir>/logs/content/` in the OS file manager. Backend
/// creates the folder first if it doesn't exist.
export async function contentOpenFolder(): Promise<void> {
  await invoke('content_open_folder');
}

/// Delete every file inside `<exe-dir>/logs/content/`. Returns the
/// count of removed files.
export async function contentClear(): Promise<number> {
  return invoke<number>('content_clear');
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
