// Persisted section (sub-tab) selection for the app-rendered views
// (Workbench, Code Intelligence, Tool Activity, Code Audit). These components
// are destroyed/recreated whenever their tab is deactivated, hidden, or the
// app restarts, so component-local $state resets to the default section — the
// selection lives here instead.
//
// V42 Phase C moved the durable half of this state out of `localStorage` and
// into the per-project `.cimp/ui_state.json` (see `uiState.ts`): it is a
// *view* preference, not configuration, but it belongs to the project rather
// than to the machine. Every function here stays synchronous — `uiState.ts`
// hydrates a cache before `mount()` precisely so these can be called from
// `$state(...)` initialisers without a first-paint flash.

import {
  VIEW_CARD_PREFIX,
  VIEW_PREF_PREFIX,
  VIEW_SECTION_PREFIX,
  getUiValue,
  isDurablePref,
  setUiValue,
} from './uiState';

// The pref family is the one that is split: the durable names listed in
// `uiState.ts` go to the project file, the rest (per-keystroke filter text,
// expanded-row sets that grow without bound) stay in `localStorage`, where
// their staleness is by design. These two helpers are the only remaining
// `localStorage` touch points in the view-prefs path.

function loadEphemeral(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function saveEphemeral(key: string, value: string | null): void {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    // A quota/serialization failure loses persistence, never breaks the UI.
  }
}

/// The last-selected section for `view`, or `fallback` if none was saved or
/// the saved value is no longer a valid section id (e.g. after a rename).
export function loadViewSection<T extends string>(
  view: string,
  valid: readonly T[],
  fallback: T,
): T {
  const raw = getUiValue(VIEW_SECTION_PREFIX + view);
  return raw !== null && (valid as readonly string[]).includes(raw) ? (raw as T) : fallback;
}

export function saveViewSection(view: string, id: string): void {
  setUiValue(VIEW_SECTION_PREFIX + view, id);
}

// Open/collapsed state of a view's named <details> cards (e.g. the Code
// Intelligence usage cards) — same destroy/recreate problem, same home.

export function loadCardOpen(view: string, card: string, fallback = false): boolean {
  const raw = getUiValue(`${VIEW_CARD_PREFIX}${view}.${card}`);
  return raw === null ? fallback : raw === '1';
}

export function saveCardOpen(view: string, card: string, open: boolean): void {
  setUiValue(`${VIEW_CARD_PREFIX}${view}.${card}`, open ? '1' : '0');
}

// Free-form per-view prefs: single strings (a selected commit hash, a layout
// mode) and string sets (expanded file paths / commit hashes). Callers
// validate the loaded value where a stale one could mislead — a hash that no
// longer exists simply matches nothing, which every consumer here tolerates.

export function loadViewString(view: string, key: string): string | null {
  const k = `${VIEW_PREF_PREFIX}${view}.${key}`;
  return isDurablePref(view, key) ? getUiValue(k) : loadEphemeral(k);
}

export function saveViewString(view: string, key: string, value: string | null): void {
  const k = `${VIEW_PREF_PREFIX}${view}.${key}`;
  if (isDurablePref(view, key)) setUiValue(k, value);
  else saveEphemeral(k, value);
}

export function loadViewSet(view: string, key: string): string[] {
  try {
    const raw = loadViewString(view, key);
    const arr: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(arr) ? arr.filter((x): x is string => typeof x === 'string') : [];
  } catch {
    return [];
  }
}

export function saveViewSet(view: string, key: string, values: Iterable<string>): void {
  try {
    saveViewString(view, key, JSON.stringify([...values]));
  } catch {
    // Same posture as saveViewSection.
  }
}
