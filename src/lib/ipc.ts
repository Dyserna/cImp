import { invoke, Channel } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { TabId } from './tabs/types';

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

/// Restart a closed Shell tab. Backend validates the tab kind/state and
/// emits `tab-restart-requested` so Terminal.svelte rebinds its bytes
/// channel via `pty_restart`. The `TabClosedStateChanged { closed: false }`
/// event clears the overlay once the new PTY has spawned.
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
