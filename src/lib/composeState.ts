// Compose-overlay state. Two stores: a boolean for whether the sheet is
// shown, and the live textarea content. Submit writes to whichever tab is
// currently active (compose targets the on-screen tab — see the v2 design
// doc's "Compose section unchanged" note).

import { writable, get } from 'svelte/store';
import { ptyWrite } from './ipc';
import { activeTab } from './tabs/state';
import { focusTerminal } from './terminalFocus';

export const composeOpen = writable<boolean>(false);
export const composeContent = writable<string>('');
export const composeFocused = writable<boolean>(false);

export function openCompose(): void {
  if (get(composeOpen)) return;
  composeOpen.set(true);
}

export function closeCompose(): void {
  composeOpen.set(false);
  composeContent.set('');
  focusTerminal();
}

export async function submitCompose(): Promise<void> {
  const content = get(composeContent);
  if (!content) {
    closeCompose();
    return;
  }
  try {
    await ptyWrite(get(activeTab), content + '\r');
  } catch (e) {
    console.error('compose submit pty_write failed:', e);
  }
  closeCompose();
}
