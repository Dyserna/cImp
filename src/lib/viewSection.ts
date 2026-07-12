// Persisted section (sub-tab) selection for the app-rendered views
// (Workbench, Code Intelligence, Tool Activity). These components are
// destroyed/recreated whenever their tab is deactivated, hidden, or the app
// restarts, so component-local $state resets to the default section — the
// selection lives here instead. localStorage rather than settings for the
// same reason as `tabs/visibility.ts`: it's a per-machine *view* preference,
// not configuration.

const KEY_PREFIX = 'cimp.view-section.v1.';

/// The last-selected section for `view`, or `fallback` if none was saved or
/// the saved value is no longer a valid section id (e.g. after a rename).
export function loadViewSection<T extends string>(
  view: string,
  valid: readonly T[],
  fallback: T,
): T {
  try {
    const raw = localStorage.getItem(KEY_PREFIX + view);
    return raw !== null && (valid as readonly string[]).includes(raw) ? (raw as T) : fallback;
  } catch {
    return fallback;
  }
}

export function saveViewSection(view: string, id: string): void {
  try {
    localStorage.setItem(KEY_PREFIX + view, id);
  } catch {
    // A quota/serialization failure loses persistence, never breaks the UI.
  }
}

// Open/collapsed state of a view's named <details> cards (e.g. the Code
// Intelligence usage cards) — same destroy/recreate problem, same home.

const CARD_KEY_PREFIX = 'cimp.view-card-open.v1.';

export function loadCardOpen(view: string, card: string, fallback = false): boolean {
  try {
    const raw = localStorage.getItem(`${CARD_KEY_PREFIX}${view}.${card}`);
    return raw === null ? fallback : raw === '1';
  } catch {
    return fallback;
  }
}

export function saveCardOpen(view: string, card: string, open: boolean): void {
  try {
    localStorage.setItem(`${CARD_KEY_PREFIX}${view}.${card}`, open ? '1' : '0');
  } catch {
    // Same posture as saveViewSection.
  }
}

// Free-form per-view prefs: single strings (a selected commit hash, a layout
// mode) and string sets (expanded file paths / commit hashes). Callers
// validate the loaded value where a stale one could mislead — a hash that no
// longer exists simply matches nothing, which every consumer here tolerates.

const PREF_KEY_PREFIX = 'cimp.view-pref.v1.';

export function loadViewString(view: string, key: string): string | null {
  try {
    return localStorage.getItem(`${PREF_KEY_PREFIX}${view}.${key}`);
  } catch {
    return null;
  }
}

export function saveViewString(view: string, key: string, value: string | null): void {
  try {
    const k = `${PREF_KEY_PREFIX}${view}.${key}`;
    if (value === null) localStorage.removeItem(k);
    else localStorage.setItem(k, value);
  } catch {
    // Same posture as saveViewSection.
  }
}

export function loadViewSet(view: string, key: string): string[] {
  try {
    const raw = localStorage.getItem(`${PREF_KEY_PREFIX}${view}.${key}`);
    const arr: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(arr) ? arr.filter((x): x is string => typeof x === 'string') : [];
  } catch {
    return [];
  }
}

export function saveViewSet(view: string, key: string, values: Iterable<string>): void {
  try {
    localStorage.setItem(`${PREF_KEY_PREFIX}${view}.${key}`, JSON.stringify([...values]));
  } catch {
    // Same posture as saveViewSection.
  }
}
