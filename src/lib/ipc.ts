import { invoke, Channel } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export type BytesChannel = Channel<string>;

export function createBytesChannel(): BytesChannel {
  return new Channel<string>();
}

export async function ptyStart(
  channel: BytesChannel,
  rows: number,
  cols: number
): Promise<void> {
  await invoke('pty_start', { channel, rows, cols });
}

export async function ptyRestart(
  channel: BytesChannel,
  rows: number,
  cols: number
): Promise<void> {
  await invoke('pty_restart', { channel, rows, cols });
}

export async function ptyWrite(input: string): Promise<void> {
  await invoke('pty_write', { input });
}

export async function ptyResize(rows: number, cols: number): Promise<void> {
  await invoke('pty_resize', { rows, cols });
}

export async function ttsTest(text: string): Promise<void> {
  await invoke('tts_test', { text });
}

export async function composeContentChanged(nonEmpty: boolean): Promise<void> {
  await invoke('compose_content_changed', { nonEmpty });
}

export async function acknowledgeError(): Promise<void> {
  await invoke('acknowledge_error');
}

export function onPtyExit(handler: (payload: string) => void): Promise<UnlistenFn> {
  return listen<string>('pty-exit', (event) => handler(event.payload));
}

export function decodeBase64(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
