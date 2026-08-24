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
// an immediate fire-and-forget `ui_state_set` carrying only the keys touched
// since the last flush. A failed write loses persistence and nothing else,
// matching the posture of the `localStorage` code it replaces: *losing view
// state must never break the UI.*
//
// **No time-based debounce** (V42 review, RV-2). There was a 250 ms one, and
// it re-introduced the closing race that the synchronous
// `localStorage.setItem` this replaces did not have: a toggle followed within
// 250 ms by the window closing was lost unless the `pagehide` flush won, and
// `pagehide` is not guaranteed on every teardown path (a crash, a kill, a
// webview the OS reclaims). `installLayoutPersistence`'s debounce was cited
// here as precedent and is NOT one: the layout tree it coalesces is held by
// the BACKEND, which flushes it on close from the Rust side, so a frontend
// timer dropped at teardown there costs nothing. This data had no second copy.
//
// What is left is same-tick coalescing only: the writes one event handler
// makes (an `$effect` that touches three keys does so in a single task) batch
// through `queueMicrotask`, so one turn of the event loop is still one IPC
// call. The `pagehide` flush stays as a belt for a patch queued in the last
// microtask before teardown.
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
///
/// **Write-liveness is the LAST thing this sets** (V42 review, RV-4). It used
/// to be set the moment the read landed, which left a window whose import then
/// failed both write-live and holding a cache the import had not finished
/// filling: the next `<details>` toggle would patch the file from a state
/// nobody had finished assembling, and the un-imported values would never be
/// tried again because a later launch would find the same half-story. The rule
/// now is: this window may write only once the import has either committed or
/// been confirmed unnecessary. A failed import means read-only for the session
/// — the values are still in `localStorage`, the marker is still absent, and
/// the next launch retries the whole thing.
///
/// **It is also BOUNDED** (V42 review, RV-3). `mount(App)` waits on this, so
/// an answer that never comes was a window that was never mounted — revealed
/// (`showMainWindowOnce`'s 3 s safety net fires regardless) and permanently
/// empty. A hung `ui_state_get` is not hypothetical: it is one blocking file
/// read on a path that can be a stalled network share, a locked file, or a
/// backend wedged before managed state came up. After
/// [`HYDRATE_TIMEOUT_MS`] this gives up and lets the app mount on defaults —
/// the module's stated posture, "loses persistence, never breaks the UI",
/// applied to the one place that could still break it.
///
/// The abandoned read is not allowed to land later. A late answer would fill
/// the cache under a mounted app (values snapping in a frame after paint) and,
/// worse, could start the one-time import against a window that has already
/// been rendering and writing defaults. So the timeout latches, and everything
/// past an `await` in here checks it.
export async function hydrateUiState(): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const budget = new Promise<'timeout'>((resolve) => {
    timer = setTimeout(() => resolve('timeout'), HYDRATE_TIMEOUT_MS);
  });

  const outcome = await Promise.race([hydrateFromDisk(), budget]);
  clearTimeout(timer);
  if (outcome === 'timeout') {
    // Latch first, so whatever `hydrateFromDisk` is suspended on cannot
    // mutate the cache or arm writes when it finally resumes.
    abandoned = true;
    console.warn(
      `ui_state_get did not answer within ${HYDRATE_TIMEOUT_MS} ms; ` +
        'mounting on default view state, which will not be persisted this session',
    );
  }
}

/// How long `mount(App)` may wait on the one blocking read. Generous relative
/// to a local file read (single-digit milliseconds) and well under
/// `main.ts`'s 3 s reveal net, so a window that hits this still mounts before
/// it becomes visible.
const HYDRATE_TIMEOUT_MS = 2000;

/// Set when [`hydrateUiState`] gave up waiting. One-way: a window that timed
/// out stays on defaults and write-inert for its whole life rather than
/// half-adopting a late answer.
let abandoned = false;

async function hydrateFromDisk(): Promise<'done'> {
  let file: UiStateFile;
  try {
    file = await invoke<UiStateFile>('ui_state_get');
  } catch (e) {
    console.error('ui_state_get failed; view state is defaults this session:', e);
    return 'done';
  }
  if (abandoned) return 'done';

  const next: Record<string, string> = {};
  for (const [k, v] of Object.entries(file?.values ?? {})) {
    // Hand-edited files can hold anything. Non-strings are outside the value
    // domain this layer defines, so they read as absent rather than as a
    // value some call site would then have to defend against.
    if (typeof v === 'string') next[k] = v;
  }
  cache = next;

  // Reads are live from here on regardless; only WRITES wait for the import.
  if (cache[IMPORT_MARKER_KEY] === '1') {
    hydrated = true; // no import owed — this project has been through one
    return 'done';
  }
  const imported = await runOneTimeImport();
  // Re-checked because the import is the long half. If the budget expired
  // while it was in flight the app has already mounted, and arming writes for
  // a window whose import may or may not have committed is exactly the
  // half-story RV-4 is about — so it stays read-only and the next launch
  // settles it. (The cache filled above is kept: those are the file's real
  // values, and reading them is never the risk.)
  if (!abandoned) hydrated = imported;
  return 'done';
}

/// The saved value for `key`, or `null` when there is none. Synchronous by
/// construction — see the module header.
export function getUiValue(key: string): string | null {
  return Object.prototype.hasOwnProperty.call(cache, key) ? cache[key] : null;
}

/// Save `key`, or remove it when `value` is `null`. Write-through to the
/// cache, then an immediate flush (coalesced with anything else written in the
/// same tick — see the module header). Fire-and-forget: the caller never
/// learns whether the disk write worked, exactly as `localStorage.setItem` in
/// a `try/catch` never told it either.
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

// ── Same-tick flush ──────────────────────────────────────────────────────

/// Keys touched since the last flush. Only these are sent: the backend merges
/// a patch rather than replacing the object, so a burst from one window can
/// never drop a key another window wrote.
const dirty = new Set<string>();

/// Whether a microtask is already queued to send `dirty`. Not a timer — see
/// the module header on why the 250 ms debounce is gone (V42 review, RV-2).
let flushQueued = false;

function scheduleFlush(): void {
  if (flushQueued) return;
  flushQueued = true;
  // A microtask, so several `setUiValue`s inside one handler still cost one
  // IPC call, but the call is on its way before the browser can run another
  // task — including whatever teardown a close begins.
  queueMicrotask(() => {
    if (!flushQueued) return; // an explicit flush already took this batch
    void flushUiState();
  });
}

/// Send the pending patch now. Also the app-close hook, for the window between
/// a `setUiValue` and its microtask.
export async function flushUiState(): Promise<void> {
  flushQueued = false;
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
    reportWriteFailure(e);
  }
}

/// Write failures are reported ONCE per window.
///
/// Two of them repeat rather than resolve — a read-only volume, and the
/// version refusal `ui_state_set` answers with when the file on disk is newer
/// than this build understands (V42 review, RV-10). Both would otherwise put a
/// console line behind every `<details>` toggle for the rest of the session,
/// which is noise, not a signal. The cache stays live either way: this window
/// keeps working, it just stops persisting.
let writeFailureLogged = false;

function reportWriteFailure(e: unknown): void {
  if (writeFailureLogged) return;
  writeFailureLogged = true;
  console.error(
    'ui_state_set failed; view state is not being persisted this session ' +
      '(this is logged once):',
    e,
  );
}

if (typeof window !== 'undefined') {
  // Best effort on teardown. `installReloadBlocker` rules out a user-triggered
  // reload, so the only path here is the window actually closing.
  window.addEventListener('pagehide', () => void flushUiState());
}

// ── One-time import ──────────────────────────────────────────────────────

/// **Copy** the durable `localStorage` values into `ui_state.json` once per
/// project, leaving the originals exactly where they are.
///
/// Returns whether the project is now imported — which is what makes this
/// window write-live (RV-4, see [`hydrateUiState`]).
///
/// **A copy, not a move** (V42 review, RV-1). It used to `removeItem` each key
/// after the backend confirmed the write, and `localStorage` is per-*machine*
/// while the marker that stops the import is per-*project*: the first checkout
/// launched after upgrading therefore MOVED the machine-wide state into its own
/// `ui_state.json`, and every other checkout on that machine imported nothing
/// because there was nothing left to read. Leaving the keys makes the import
/// lossless for all of them — each project takes its own copy of the same seed
/// values and diverges from there.
///
/// The leftovers are inert: once a project's marker is set nothing in this
/// module reads `localStorage` again, and the ephemeral prefs
/// (`diff.expanded`, `code-audit.text`, …) that were always going to stay there
/// mean the origin was never going to be emptied anyway. Their cost is a few
/// kilobytes per install; the cost of the alternative is a user's other
/// checkouts silently losing their view state, once, unrecoverably.
///
/// Values are copied verbatim. A corrupt one (a truncated JSON string, a
/// section id that no longer exists) is carried across unchanged and rejected
/// by the same call-site validation that rejects it today — the import is a
/// copy, not a repair.
async function runOneTimeImport(): Promise<boolean> {
  const patch: Record<string, string> = {};
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
  }
  // Recorded even when nothing moved — "this project has been through the
  // import" is what the marker means, not "something was imported".
  patch[IMPORT_MARKER_KEY] = '1';

  try {
    await invoke('ui_state_set', { patch });
  } catch (e) {
    // Write-inert for the session (RV-4): the cache is missing whatever the
    // import would have added, so a patch from this window would persist a
    // partial picture. Nothing was deleted, the marker is still absent, and
    // the next launch runs the whole import again.
    console.error('ui_state import failed; view state is read-only until the next launch:', e);
    return false;
  }

  // Committed: adopt the values into the cache, so this window — already past
  // the point where they would have been read from `localStorage` — answers
  // with them for the rest of the session. The originals stay put; see the
  // doc comment.
  Object.assign(cache, patch);
  return true;
}
