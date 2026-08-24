// Per-project UI state — the storage layer behind `viewSection.ts` and
// `tabs/visibility.ts` (V42 Phase C).
//
// These are the small per-view toggles that must survive a component being
// destroyed and recreated: the sub-tab a view was last on, which usage cards
// were expanded, the Events table's column widths and hidden columns, the
// audit filters, and the set of UI-hidden tabs. They used to live in the
// webview's `localStorage`, which made them per-*machine*: hiding the
// Workbench tab in one checkout hid it in every checkout the same install
// opened. They now live in `<launch_cwd>/.cimp/ui_state.json`, owned by Rust
// (`src-tauri/src/ipc/ui_state.rs`) — per project, next to `config.json` and
// the note.
//
// ## Why a synchronous cache
//
// Every consumer reads inside a `$state(...)` initialiser, and
// `visibility.ts` reads at module-import time. Those reads happen before the
// first paint and cannot await anything — an async read would render the
// default section/collapsed card first and snap to the saved one a frame
// later. So: exactly one blocking `ui_state_get` in `main.ts` *before*
// `mount(App)` fills this module's cache, and every helper stays synchronous
// against it. See `hydrateUiState`.
//
// ## Writes
//
// Write-through to the cache (so the next synchronous read is correct), then
// a ~250 ms debounced, fire-and-forget `ui_state_set` carrying only the keys
// touched since the last flush — the same "frontend coalesces, backend
// commits" split `save_layout` uses. A failed write loses persistence and
// nothing else, matching the posture of the `localStorage` code it replaces:
// *losing view state must never break the UI.*
//
// ## Value domain
//
// Values are the exact strings the keys held in `localStorage` — including
// `events.col-widths`, which is JSON *inside* a string. Keeping the domain
// uniform makes the one-time import a literal copy and leaves every parse and
// validity check at the call site that already owned it (section-id lists,
// the `#rrggbb` regex, the severity enum, the width clamps). Nothing here
// interprets a value; neither does the Rust side.

import { invoke } from '@tauri-apps/api/core';

// ── Key vocabulary ───────────────────────────────────────────────────────
//
// This module owns the key strings (rather than `viewSection.ts`) so the
// routing predicate and the import list cannot drift apart, and so
// `visibility.ts` can depend on the storage layer without a cycle.

/// `cimp.view-section.v1.<view>` — the last-selected sub-tab of a view.
export const VIEW_SECTION_PREFIX = 'cimp.view-section.v1.';
/// `cimp.view-card-open.v1.<view>.<card>` — a named `<details>` card's state.
export const VIEW_CARD_PREFIX = 'cimp.view-card-open.v1.';
/// `cimp.view-pref.v1.<view>.<key>` — free-form per-view strings and sets.
export const VIEW_PREF_PREFIX = 'cimp.view-pref.v1.';
/// The UI-hidden tab set (a JSON array of TabId, stored as a string).
export const HIDDEN_TABS_KEY = 'cimp.hidden-tabs.v1';

/// Internal marker recording that the one-time `localStorage` import already
/// ran for this project. Stored as an ordinary entry so the backend needs no
/// metadata surface beyond `version` — see `runOneTimeImport`.
const IMPORT_MARKER_KEY = 'cimp.ui-state.imported-from-local-storage.v1';

/// Every view whose section selection persists, with the concrete view ids the
/// four callers pass (`WorkbenchView`, `CodeIntelligenceView`,
/// `ToolActivityView`, `CodeAuditView`). Only used to enumerate keys for the
/// one-time import — the section family is durable wholesale, so
/// `loadViewSection` needs no membership test.
const IMPORTED_SECTION_VIEWS = [
  'workbench',
  'code-intelligence',
  'tool-activity',
  'code-audit',
] as const;

/// Likewise for the Code Intelligence overview cards.
const IMPORTED_CARDS = [
  'code-intelligence.usage-cost',
  'code-intelligence.usage-dashboard',
  'code-intelligence.usage-effectiveness',
  'code-intelligence.usage-this-session',
  'code-intelligence.usage-advisor',
  'code-intelligence.usage-sessions',
] as const;

/// The `<view>.<key>` pref names that persist per project.
///
/// The pref family is the one that is *split*: everything else under
/// `cimp.view-pref.v1.` is deliberately ephemeral (`diff.expanded`,
/// `diff.full-view`, `worktrees.expanded-diff`, `worktrees.full-diff`,
/// `session-commits.expanded`, `git-graph.selected`, `timeline.open-diff`,
/// `code-audit.text`, `code-quality.text`). Those are per-keystroke or
/// unbounded-growth values whose staleness is by design; they stay in
/// `localStorage` and never reach the backend.
///
/// `code-audit.*` / `code-quality.*` keep their legacy namespaces verbatim —
/// they are the `view` prop `CodeAuditView` passes to the two `AuditPanel`
/// instances, and renaming them would silently drop users' saved filters.
export const DURABLE_VIEW_PREFS: ReadonlySet<string> = new Set([
  'diff.view-mode',
  'events.col-widths',
  'events.cols-hidden',
  'code-intelligence.usage-cost-mode',
  'code-intelligence.dash-model-colors',
  'code-audit.severity',
  'code-audit.hidden-tools',
  'code-quality.severity',
  'code-quality.hidden-tools',
]);

/// The full set of `localStorage` keys the one-time import moves. Derived from
/// the vocabulary above so adding a durable pref updates both the routing and
/// the import.
export const IMPORTED_KEYS: readonly string[] = [
  ...IMPORTED_SECTION_VIEWS.map((v) => VIEW_SECTION_PREFIX + v),
  ...IMPORTED_CARDS.map((c) => VIEW_CARD_PREFIX + c),
  ...[...DURABLE_VIEW_PREFS].map((p) => VIEW_PREF_PREFIX + p),
  HIDDEN_TABS_KEY,
];

/// True when `<view>.<key>` is one of the prefs that persists per project.
/// Called by `viewSection.ts` to route a pref read/write.
export function isDurablePref(view: string, key: string): boolean {
  return DURABLE_VIEW_PREFS.has(`${view}.${key}`);
}

// ── The cache ────────────────────────────────────────────────────────────

let cache: Record<string, string> = {};

/// Until `hydrateUiState` has run, this window has no idea what is on disk.
/// Reads answer `null` (⇒ every consumer's built-in default) and — critically
/// — writes are inert: a window that never hydrated must not be able to patch
/// the file with its defaults. The settings window is exactly that case. It
/// transitively bundles `viewSection.ts` / `visibility.ts` (via
/// `SettingsApp → compose/templates → terminals → avatarState → appViews`) but
/// mounts none of the views and starts no avatar-state listener, so today it
/// never calls a helper at all; this guard is what keeps that true by
/// construction if it ever does.
let hydrated = false;

/// Shape of `ui_state.json` as the backend hands it over.
interface UiStateFile {
  version: number;
  values: Record<string, unknown>;
}

/// Fill the cache from disk, then run the one-time `localStorage` import if
/// this project has never had one. Awaited exactly once per window, before
/// `mount()`, so every synchronous read below it sees the real values on the
/// first paint.
///
/// Never rejects. A backend that cannot answer leaves the window unhydrated:
/// views render their defaults and nothing is written — strictly better than
/// blocking the mount or persisting defaults over good state.
export async function hydrateUiState(): Promise<void> {
  let file: UiStateFile;
  try {
    file = await invoke<UiStateFile>('ui_state_get');
  } catch (e) {
    console.error('ui_state_get failed; view state is defaults this session:', e);
    return;
  }

  const next: Record<string, string> = {};
  for (const [k, v] of Object.entries(file?.values ?? {})) {
    // Hand-edited files can hold anything. Non-strings are outside the value
    // domain this layer defines, so they read as absent rather than as a
    // value some call site would then have to defend against.
    if (typeof v === 'string') next[k] = v;
  }
  cache = next;
  hydrated = true;

  if (cache[IMPORT_MARKER_KEY] !== '1') await runOneTimeImport();
}

/// The saved value for `key`, or `null` when there is none. Synchronous by
/// construction — see the module header.
export function getUiValue(key: string): string | null {
  return Object.prototype.hasOwnProperty.call(cache, key) ? cache[key] : null;
}

/// Save `key`, or remove it when `value` is `null`. Write-through to the
/// cache, then a debounced flush. Fire-and-forget: the caller never learns
/// whether the disk write worked, exactly as `localStorage.setItem` in a
/// `try/catch` never told it either.
export function setUiValue(key: string, value: string | null): void {
  if (!hydrated) return;
  if (value === null) {
    if (!Object.prototype.hasOwnProperty.call(cache, key)) return;
    delete cache[key];
  } else {
    if (cache[key] === value) return;
    cache[key] = value;
  }
  dirty.add(key);
  scheduleFlush();
}

// ── Debounced flush ──────────────────────────────────────────────────────

/// Keys touched since the last flush. Only these are sent: the backend merges
/// a patch rather than replacing the object, so a burst from one window can
/// never drop a key another window wrote.
const dirty = new Set<string>();
let flushTimer: ReturnType<typeof setTimeout> | null = null;

/// Matches `installLayoutPersistence`'s debounce. Long enough to coalesce a
/// drag of the Events column splitter into one write, short enough that the
/// value is on disk well before a user could close the window after a click.
const FLUSH_DELAY_MS = 250;

function scheduleFlush(): void {
  if (flushTimer !== null) return;
  flushTimer = setTimeout(() => {
    flushTimer = null;
    void flushUiState();
  }, FLUSH_DELAY_MS);
}

/// Send the pending patch now. Also the app-close hook — a pending debounce
/// would otherwise lose the last toggle, which the synchronous
/// `localStorage.setItem` this replaces could not.
export async function flushUiState(): Promise<void> {
  if (flushTimer !== null) {
    clearTimeout(flushTimer);
    flushTimer = null;
  }
  if (dirty.size === 0) return;
  const patch: Record<string, string | null> = {};
  for (const k of dirty) patch[k] = getUiValue(k);
  dirty.clear();
  try {
    await invoke('ui_state_set', { patch });
  } catch (e) {
    // Loses persistence, never breaks the UI. Not re-queued: the next toggle
    // of the same key will carry the current value anyway, and retrying a
    // failing backend on a timer would just multiply the noise.
    console.error('ui_state_set failed; view state not persisted:', e);
  }
}

if (typeof window !== 'undefined') {
  // Best effort on teardown. `installReloadBlocker` rules out a user-triggered
  // reload, so the only path here is the window actually closing.
  window.addEventListener('pagehide', () => void flushUiState());
}

// ── One-time import ──────────────────────────────────────────────────────

/// Move the durable `localStorage` values into `ui_state.json` once per
/// project, then delete them.
///
/// Order matters: the keys are removed only *after* the backend has confirmed
/// the write. A failed write leaves both the values and the absent marker
/// alone, so the next launch simply tries again — the one thing that must
/// never happen is deleting the only copy of a user's state because the file
/// could not be written.
///
/// Values are copied verbatim. A corrupt one (a truncated JSON string, a
/// section id that no longer exists) is carried across unchanged and rejected
/// by the same call-site validation that rejects it today — the import is a
/// move, not a repair.
async function runOneTimeImport(): Promise<void> {
  const patch: Record<string, string> = {};
  const moved: string[] = [];
  for (const key of IMPORTED_KEYS) {
    let raw: string | null = null;
    try {
      raw = localStorage.getItem(key);
    } catch {
      // No `localStorage` at all (or it threw): nothing to import. The marker
      // is still recorded below so this doesn't retry on every launch.
      break;
    }
    if (raw === null) continue;
    patch[key] = raw;
    moved.push(key);
  }
  // Recorded even when nothing moved — "this project has been through the
  // import" is what the marker means, not "something was imported".
  patch[IMPORT_MARKER_KEY] = '1';

  try {
    await invoke('ui_state_set', { patch });
  } catch (e) {
    console.error('ui_state import failed; retrying next launch:', e);
    return;
  }

  // Committed: adopt the values into the cache (this window is already past
  // the point where they would have been read from `localStorage`) and drop
  // the originals so the next launch is a plain hydrate.
  Object.assign(cache, patch);
  for (const key of moved) {
    try {
      localStorage.removeItem(key);
    } catch {
      // A leftover key is inert — the marker means it will never be read
      // again.
    }
  }
}
