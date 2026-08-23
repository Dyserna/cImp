// V40 Phase E, locked decision 24 — **the frontend's window onto the backend's
// model-visible text inventory**.
//
// Any string the UI types into an AI tab is text a model reads, and until this
// module one of them lived here as a TypeScript literal (`compose/
// attachments.ts`'s "Read the attached image file(s)."). A literal in the
// frontend is invisible to the backend inventory that exists to answer *"what
// does cImp tell this harness?"*, and no harness can influence it — so it is
// fetched instead.
//
// Deliberately tiny and cache-free of anything long-lived: the inventory is
// per-tab (a tab's harness decides the vocabulary) and cheap to ask for, and a
// cache that outlived a tab reconfigure would hand the next harness the previous
// one's text.

import { invoke } from '@tauri-apps/api/core';

/// The slots the backend serves. Mirrors `harness::instructions::Slot::id`;
/// the frontend names only the ones it delivers.
export type InstructionSlot = 'attachment';

/// Every instruction slot for `tab`'s harness, keyed by slot id.
///
/// Returns `{}` on any failure — the caller degrades by omitting the text, never
/// by substituting one of its own (which is the state this module replaced).
export async function harnessInstructions(
  tab: string,
): Promise<Record<string, string>> {
  try {
    return await invoke<Record<string, string>>('harness_instructions', { tab });
  } catch (e) {
    console.warn('harness_instructions failed:', e);
    return {};
  }
}

/// One slot's text for `tab`'s harness, or `''` when it is unavailable.
export async function harnessInstruction(
  tab: string,
  slot: InstructionSlot,
): Promise<string> {
  const all = await harnessInstructions(tab);
  return all[slot] ?? '';
}
