// Compose-overlay state. Two stores: a boolean for whether the sheet is
// shown, and the live textarea content. Cancel and submit both clear the
// content (cancel discards; submit has already shipped the bytes to the
// PTY by the time the close runs). The store is also subscribed to from
// App.svelte to emit a state-machine signal on the empty/non-empty edge.

import { writable, get } from 'svelte/store';
import { ptyWrite } from './ipc';
import { focusTerminal } from './terminalFocus';

export const composeOpen = writable<boolean>(false);
export const composeContent = writable<string>('');
/// Whether the compose textarea currently has DOM focus. The
/// `submit_compose` shortcut uses this as its active predicate so
/// `Ctrl+Enter` only fires when focus is in the textarea (and otherwise
/// flows to xterm.js as normal).
export const composeFocused = writable<boolean>(false);

export function openCompose(): void {
  // No-op if already open — the milestone explicitly disallows toggle on
  // the open shortcut. Cancel-shortcut is the only way to close.
  if (get(composeOpen)) return;
  composeOpen.set(true);
}

/// Close the sheet and discard any draft. Used by both cancel and post-
/// submit cleanup; submission is responsible for sending bytes BEFORE
/// calling this. Restores focus to the terminal so the user can keep
/// typing without an extra click.
export function closeCompose(): void {
  composeOpen.set(false);
  composeContent.set('');
  focusTerminal();
}

/// Append-mode submit: write the textarea content + carriage return to the
/// PTY, then close the sheet. Empty content closes without writing. We
/// send `\r` (not `\n`) because xterm.js / TUIs read CR as Enter — LF is
/// often a no-op or inserts a literal newline, which would NOT submit.
export async function submitCompose(): Promise<void> {
  const content = get(composeContent);
  if (!content) {
    closeCompose();
    return;
  }
  try {
    await ptyWrite(content + '\r');
  } catch (e) {
    console.error('compose submit pty_write failed:', e);
  }
  closeCompose();
}
