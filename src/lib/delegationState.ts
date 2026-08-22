// V39 Phase B — the live in-flight delegation mirror, and the two things that
// have to happen the moment a flight starts.
//
// Kept OUT of `delegation.ts` on purpose: that file is the pure, tested half
// (the glyph table, the attribution line, the role rules) and importing the
// Tauri event API into it would put a transport in every test that only wanted
// a string. This file is the transport; everything it decides, it decides by
// calling into `delegation.ts`.

import { get, readable, writable, type Readable, type Writable } from 'svelte/store';
import { listen } from '@tauri-apps/api/event';
import { delegationStatuses } from './ipc';
import { writeLocalEcho, type DelegationChanged, type InFlightView } from './delegation';
import { setPromptRelaxedTabs } from './delegationPrompt';
import { getTerminal } from './terminals';
import type { TabId } from './tabs/types';

/// The Tauri event the backend publishes on every delegation edge. Spelled
/// verbatim from Rust's `delegation::engine::EVENT_DELEGATION_CHANGED`.
const EVENT_DELEGATION_CHANGED = 'delegation-changed';

/// Every in-flight delegation, keyed by WORKER tab id.
///
/// **Replaced wholesale on every event, never merged.** The backend's payload
/// is the entire in-flight set by design (its own doc comment says why: the set
/// is tiny, and a delta stream has to be replayed from a known start, so a
/// window that opens late or reloads would paint a stale glyph until the next
/// edge). Merging deltas into this map would reintroduce exactly the state the
/// snapshot exists to make impossible — an entry for a flight that ended while
/// nobody was listening.
export const delegationInFlight: Writable<Record<string, InFlightView>> = writable({});

/// Whether `tab` is being driven right now, from a snapshot.
export function drivenBy(
  inFlight: Record<string, InFlightView>,
  tabId: string,
): InFlightView | null {
  return inFlight[tabId] ?? null;
}

let initialized = false;
/// Whether the opening paint is behind us — see [`initDelegation`]. Until it
/// is, a snapshot is a BASELINE (what was already running when this window came
/// up), not an edge, and nothing about it is echoed.
let baselined = false;

function apply(rows: [string, InFlightView][]): void {
  const next: Record<string, InFlightView> = {};
  for (const [tab, view] of rows) next[tab] = view;
  const previous = get(delegationInFlight);
  delegationInFlight.set(next);
  // V39 review R-4: the terminal's read-only courtesy gate needs the standing
  // -prompt fact synchronously, inside `onData`, and cannot import this module
  // (it is imported BY this one, for the local echo). The flag module in
  // between is the whole of the arrangement. Replaced wholesale, like the
  // store above and for the same reason: the payload is a snapshot.
  setPromptRelaxedTabs(
    Object.entries(next)
      .filter(([, view]) => view.awaiting_prompt)
      .map(([tab]) => tab),
  );

  // Locked decision 2a's local echo. Fired here rather than in a component
  // because it must happen ONCE per flight, on the edge — a component that
  // wrote it from a reactive statement would repeat the line on every re-render
  // and on every tab attach.
  if (!baselined) return;
  for (const [tab, view] of Object.entries(next)) {
    if (previous[tab]) continue;
    const term = getTerminal(tab as TabId);
    // Display only — `writeLocalEcho` writes to the xterm widget. There is no
    // path from here to `pty_write`, and the worker model never sees this line.
    if (term) writeLocalEcho(term, view.driver_agent, view.driver_name);
  }
}

/// Subscribe to `delegation-changed` and take the opening snapshot.
///
/// Idempotent. The listener is registered BEFORE the initial pull so an edge
/// landing mid-pull is not lost; the pull then only fills in if no event has
/// arrived, the `initSettings` convention.
///
/// **`baselined` is set exactly once, here, when the opening sequence is over —
/// not by whichever `apply` happened to run first.** The difference is a real
/// defect the first cut had: keyed on "the first snapshot", a session in which
/// nothing was in flight at startup would swallow the echo of the FIRST
/// delegation the user ever ran, ten minutes later, because that flight's edge
/// was the first snapshot this process saw. What must be suppressed is not the
/// first snapshot but the BASELINE — a window that mounts (or reloads)
/// mid-flight is not watching a turn begin, and echoing then would stamp the
/// line into the middle of output the worker had already produced. It is set in
/// `finally` so a failed pull cannot leave the echo switched off for the rest
/// of the session.
export async function initDelegation(): Promise<void> {
  if (initialized) return;
  initialized = true;
  let gotEvent = false;
  try {
    await listen<DelegationChanged>(EVENT_DELEGATION_CHANGED, (e) => {
      gotEvent = true;
      apply(e.payload?.in_flight ?? []);
    });
  } catch (e) {
    // A failed subscribe leaves the flag set deliberately: retrying a listener
    // that threw once per app start is noise, and every surface this feeds
    // degrades to "nothing is in flight", which is the safe reading.
    console.warn('delegation-changed subscribe failed', e);
  }
  try {
    const initial = await delegationStatuses();
    if (!gotEvent) apply(initial);
  } catch (e) {
    console.warn('delegation_statuses failed; assuming nothing in flight', e);
  } finally {
    baselined = true;
  }
}

/// A once-a-second clock for the delegation banner's elapsed counter.
///
/// **Its own gate, and the gate is subscription.** Svelte's `readable` runs its
/// start function only while something is subscribed and tears it down on the
/// last unsubscribe, so the interval exists exactly while a banner is on screen
/// — the banner renders only for a pane's ACTIVE tab and only while that tab is
/// driven, so a detached or idle pane costs nothing. This is the same rule
/// `appViews.ts` states for the keep-alive views (anything periodic in a view
/// gates on `appViewVisibility`), applied where it actually bites: the banner is
/// not one of the registry's views, it is unmounted rather than detached, and a
/// second visibility store would be a gate on a timer that no longer exists.
export const delegationClock: Readable<number> = readable(Date.now(), (set) => {
  const id = setInterval(() => set(Date.now()), 1000);
  return () => clearInterval(id);
});
