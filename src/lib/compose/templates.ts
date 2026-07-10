// V14 Phase A — prompt library. Backs the compose overlay's `/` picker:
// resolved-template fetch (backend IPC), subsequence-fuzzy filtering,
// `{selection}`/`{clipboard}` variable substitution, and the placeholder
// tab-stop scan `ComposeOverlay.svelte` uses to wire Tab-to-jump.
//
// Deliberately framework-light: everything below except the two IPC
// wrappers is a pure function, so the picker/placeholder logic is
// unit-testable without touching the DOM (see `templates.test.ts`).

import { invoke } from '@tauri-apps/api/core';
import { readText as clipboardReadText } from '@tauri-apps/plugin-clipboard-manager';
import { get } from 'svelte/store';
import { focusedActiveTabId } from '../layout/store';
import { getTerminal } from '../terminals';
import type { PromptTemplate } from '../settings/types';

export type { PromptTemplate };

/// A template resolved by name across the global + project scopes — what
/// the picker actually lists. Mirrors the backend's `ResolvedTemplate`.
export interface ResolvedTemplate {
  name: string;
  body: string;
  scope: 'global' | 'project';
}

/// Fetch the by-name-resolved template list (project shadows global) for
/// `root` (defaults, backend-side, to the launch directory).
export function composeTemplates(root?: string): Promise<ResolvedTemplate[]> {
  return invoke<ResolvedTemplate[]>('compose_templates', { root: root ?? null });
}

/// Raw (unshadowed) global list, for the Settings window's Compose section.
export function composeTemplatesGlobalGet(): Promise<PromptTemplate[]> {
  return invoke<PromptTemplate[]>('compose_templates_global_get');
}

/// Save the global list. Writes straight to the physical global
/// `settings.json` (see the backend command's doc comment) — NOT the normal
/// per-project `settingsUpdate` round-trip.
export function composeTemplatesGlobalSet(templates: PromptTemplate[]): Promise<void> {
  return invoke<void>('compose_templates_global_set', { templates });
}

/// Read-only project-scope listing for the Settings window's Compose section.
export function composeTemplatesProjectGet(root?: string): Promise<PromptTemplate[]> {
  return invoke<PromptTemplate[]>('compose_templates_project_get', { root: root ?? null });
}

/// Subsequence fuzzy match: every character of `query` (case-insensitive)
/// must appear in `text` in order, not necessarily contiguous — the same
/// relaxed matching convention as a command-palette / CLI slash-command
/// picker (which this popover mirrors). Empty query matches everything.
export function fuzzyMatch(query: string, text: string): boolean {
  if (!query) return true;
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  let qi = 0;
  for (let ti = 0; ti < t.length && qi < q.length; ti++) {
    if (t[ti] === q[qi]) qi++;
  }
  return qi === q.length;
}

/// Filter resolved templates by subsequence-fuzzy match against `query`
/// (matched against the template name). Preserves resolver order among
/// matches — no extra scoring; V1 is intentionally simple.
export function filterTemplates(
  templates: ResolvedTemplate[],
  query: string,
): ResolvedTemplate[] {
  return templates.filter((t) => fuzzyMatch(query, t.name));
}

// Capturing group around the inner name: `hasPlaceholder`/`nextPlaceholderRange`
// only need the whole-match span (`match[0]`/`match.index`, unaffected by the
// group), while `substituteVariables` needs the bare name to distinguish
// `{selection}`/`{clipboard}` from an arbitrary tab-stop — one pattern serves
// both rather than keeping two near-identical regexes in sync.
//
// The `(?<!\$)` lookbehind excludes `${name}` — interpolation syntax in
// JS/TS template literals and shell — because the tab-stop scan re-reads the
// LIVE draft: once a `{selection}` substitution splices in real code, any
// `${var}` inside it would otherwise become a bogus tab-stop (Tab selects a
// span of the user's own pasted code, and overtyping it silently deletes
// that code). Template authors lose the ability to write a placeholder
// immediately after a literal `$`; that's the right trade.
const PLACEHOLDER_PATTERN = /(?<!\$)\{([a-zA-Z0-9_]+)\}/g;

/// Whether `text` still contains at least one literal `{name}` placeholder.
/// Drives whether the compose overlay's Tab-jump handler is active at all —
/// it must never fight the textarea's normal Tab behavior once every
/// placeholder has been overtyped.
export function hasPlaceholder(text: string): boolean {
  PLACEHOLDER_PATTERN.lastIndex = 0;
  return PLACEHOLDER_PATTERN.test(text);
}

export interface PlaceholderRange {
  start: number;
  end: number;
}

/// Find the next literal `{placeholder}` span in `text` at or after
/// `fromIndex`, wrapping around to the first one if none remain past that
/// point. Re-scans the LIVE text every call (rather than tracking offsets
/// captured at insertion time), so the tab-stop cycle self-heals as the
/// user edits: once a placeholder is overtyped, its `{...}` span is simply
/// gone from the text and stops matching — no stale-offset bookkeeping
/// needed. Returns `null` when no placeholder remains at all.
export function nextPlaceholderRange(text: string, fromIndex: number): PlaceholderRange | null {
  PLACEHOLDER_PATTERN.lastIndex = Math.max(0, fromIndex);
  const forward = PLACEHOLDER_PATTERN.exec(text);
  if (forward) {
    return { start: forward.index, end: forward.index + forward[0].length };
  }
  PLACEHOLDER_PATTERN.lastIndex = 0;
  const wrapped = PLACEHOLDER_PATTERN.exec(text);
  if (wrapped) {
    return { start: wrapped.index, end: wrapped.index + wrapped[0].length };
  }
  return null;
}

/// Pure variable substitution: replaces `{selection}` / `{clipboard}` in
/// `body` with the given values, leaving every other `{name}` token
/// literal. Split out from `substituteTemplate` so the substitution rule
/// itself is testable without mocking the terminal registry or the
/// clipboard plugin.
export function substituteVariables(body: string, selection: string, clipboard: string): string {
  // Defensive: `String.replace` resets a global regex's `lastIndex` itself,
  // but the reset is explicit here too since `PLACEHOLDER_PATTERN` is a
  // shared, stateful `g`-flag regex also mutated by `hasPlaceholder` /
  // `nextPlaceholderRange` — never assume it starts at 0.
  PLACEHOLDER_PATTERN.lastIndex = 0;
  return body.replace(PLACEHOLDER_PATTERN, (token, name: string) => {
    if (name === 'selection') return selection;
    if (name === 'clipboard') return clipboard;
    return token; // unresolved placeholder — stays literal as a tab-stop
  });
}

/// The focused pane's active terminal's current selection, or '' if there
/// is no focused pane / no terminal / no selection. `focusedActiveTabId` is
/// the FOCUSED pane's active tab specifically — not just "the" active tab —
/// matching the milestone's "focused pane's terminal selection" wording.
function focusedSelection(): string {
  const tabId = get(focusedActiveTabId);
  if (!tabId) return '';
  return getTerminal(tabId)?.getSelection() ?? '';
}

/// On-insert substitution: resolves `{selection}` from the focused pane's
/// terminal and `{clipboard}` via the Tauri clipboard plugin (WebView2
/// denies `navigator.clipboard.readText` — the established workaround used
/// throughout the app, see `terminals.ts`'s own paste handling). Every
/// other `{name}` token is left literal for the caller to turn into a
/// tab-stop via `nextPlaceholderRange`.
export async function substituteTemplate(body: string): Promise<string> {
  const selection = focusedSelection();
  let clipboard = '';
  try {
    clipboard = (await clipboardReadText()) ?? '';
  } catch (e) {
    console.warn('template clipboard substitution failed:', e);
  }
  return substituteVariables(body, selection, clipboard);
}
