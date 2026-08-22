/// V39 review R-4 — **which driven tabs have a standing prompt right now.**
///
/// A three-line module with no imports, and that is its whole reason for
/// existing: `delegationState.ts` already imports `terminals.ts` (for the local
/// echo), so `terminals.ts` cannot import `delegationState.ts` back without a
/// cycle — and the terminal's read-only courtesy gate is exactly what needs
/// this fact.
///
/// Locked decision 5 relaxes the keyboard while the worker holds a prompt: the
/// user's answer is the only thing that lets the turn finish. The BACKEND
/// implements that (`ReadOnlyEntry::prompt_relaxed`, review M-5), but the
/// frontend gate swallowed the keystroke before `pty_write` was ever called —
/// so on a tab the user had also locked by hand, the prompt could not be
/// answered at all and the delegation ran to its deadline reporting "worker
/// awaiting permission". The gate has to know what the backend knows.
///
/// A plain mutable set rather than a store: the gate reads it synchronously
/// inside an `onData` handler, and a subscription would buy nothing.
let relaxed: ReadonlySet<string> = new Set();

/// Replace the set — the `delegation-changed` payload is a full snapshot, so
/// this is too. Called only from `delegationState.ts`.
export function setPromptRelaxedTabs(tabs: Iterable<string>): void {
  relaxed = new Set(tabs);
}

/// Whether `tabId`'s in-flight delegation is waiting on a prompt of the user's.
export function isPromptRelaxed(tabId: string): boolean {
  return relaxed.has(tabId);
}
