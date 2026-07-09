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

/// V14 Phase A: bumped every time something wants the compose overlay to
/// open WITH its prompt-template picker already showing (the
/// `open_compose_picker` shortcut). A plain counter rather than a boolean
/// so `ComposeOverlay.svelte`'s `$effect` can detect "fired again" even
/// when the picker is already open (a boolean flip-to-true wouldn't notify
/// a second press while still true). The overlay owns resetting its own
/// "last seen" bookkeeping; this store only ever counts up.
export const composeOpenPickerSignal = writable<number>(0);

/// Open compose (if not already open) and request the picker. Bound to the
/// `open_compose_picker` shortcut in `App.svelte` and to any other future
/// caller that wants "compose, ready to pick a template" in one call.
export function openComposeWithPicker(): void {
  openCompose();
  composeOpenPickerSignal.update((n) => n + 1);
}

/// V13 Phase B: open the compose overlay with `text` appended to whatever
/// draft is already there — the Diff pane's "Send to agent" hunk action
/// (`workbench_send_hunk`'s formatted fenced block). Appends rather than
/// replaces so sending a second hunk while composing a message doesn't
/// clobber what the user already typed; a blank existing draft just becomes
/// `text` with no leading separator. The submit path is unchanged — this
/// only ever populates the draft, never sends it.
export function openComposeWith(text: string): void {
  const existing = get(composeContent);
  composeContent.set(existing ? `${existing}\n${text}` : text);
  openCompose();
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
