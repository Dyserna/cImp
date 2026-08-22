// Compose-overlay state. Two stores: a boolean for whether the sheet is
// shown, and the live textarea content. Submit writes to whichever tab is
// currently active (compose targets the on-screen tab — see the v2 design
// doc's "Compose section unchanged" note).

import { writable, get } from 'svelte/store';
import { ptyWrite } from './ipc';
import { activeTab } from './tabs/state';
import { focusTerminal } from './terminalFocus';
import { appendAttachments, DEFAULT_ATTACHMENT_FORMAT } from './compose/attachments';
import { harnessForTab } from './harness';
import { harnessInstruction } from './harnessText';

export const composeOpen = writable<boolean>(false);
export const composeContent = writable<string>('');
export const composeFocused = writable<boolean>(false);

/// V14 Phase B: absolute paths of images attached to the in-progress draft
/// (pasted → `attach.rs`-saved PNGs, or dropped → referenced in place — see
/// `ComposeOverlay.svelte`). A sibling store to `composeContent` rather than
/// a change to its type/shape, so nothing about Phase A's text-draft
/// handling (or any other existing reader of `composeContent`) changes.
/// Cleared alongside it in `closeCompose`; folded into the message text by
/// `submitCompose` via `appendAttachments`.
export const composeAttachments = writable<string[]>([]);

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

/// V14 Phase F: the Preview toolbar's Snapshot button — push an
/// already-saved PNG path (from `previewCapture`) onto the draft's
/// attachments and open compose, exactly like a pasted clipboard image
/// (Phase B). Submit still targets whatever tab is `activeTab` at send time
/// (`submitCompose`'s existing behavior) — the caller is responsible for
/// having focused the AI tab it wants the snapshot to go to before calling
/// this, the same way any other compose-targeting flow works today.
export function openComposeWithAttachment(path: string): void {
  composeAttachments.update((a) => [...a, path]);
  openCompose();
}

export function closeCompose(): void {
  composeOpen.set(false);
  composeContent.set('');
  composeAttachments.set([]);
  focusTerminal();
}

export async function submitCompose(): Promise<void> {
  const content = get(composeContent);
  const attachments = get(composeAttachments);
  // V40 Phase E: the trailing instruction is the TARGET TAB's, fetched from the
  // backend inventory (locked decision 24). Asked only when there is something
  // to attach, so a plain-text submit still costs no IPC round trip.
  const tab = get(activeTab);
  const instruction =
    attachments.length > 0 ? await harnessInstruction(tab, 'attachment') : '';
  // `appendAttachments` returns `content` unchanged when there are no
  // attachments, so an image-only draft (empty textarea, one pasted image)
  // still submits — only a truly empty draft (no text AND no attachments)
  // is a no-op.
  const message = appendAttachments(
    content,
    attachments,
    instruction,
    harnessForTab(tab)?.affordances.attachmentFormat ?? DEFAULT_ATTACHMENT_FORMAT,
  );
  if (!message) {
    closeCompose();
    return;
  }
  try {
    await ptyWrite(tab, message + '\r');
  } catch (e) {
    console.error('compose submit pty_write failed:', e);
  }
  closeCompose();
}
