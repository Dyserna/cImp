// The frontend's half of the Tauri event contract (V42 F6, #131).
//
// The names and payload types are GENERATED from `src-tauri/src/service/events.rs`
// into `./generated/events.ts` and re-exported here, so `import { PTY_EXIT } from
// './events'` is the one way to name an event. `src/lib/eventNames.test.ts` refuses a
// string literal in a `listen(...)` call, and the Rust side refuses one in an
// `emit(...)` call — the two halves can no longer be spelled differently, which is
// what used to fail silently (the listener simply never fired).
//
// This file is hand-written and holds only what codegen cannot: the typed `listen`
// wrapper below. Everything else comes from the generated module.

import { listen, type Event, type UnlistenFn } from '@tauri-apps/api/event';

export * from './generated/events';
import type { EventPayloadMap } from './generated/events';

/**
 * `listen`, with the payload type looked up from the event name.
 *
 * Prefer this over a bare `listen<T>(NAME, …)`: the payload type stops being a
 * hand-written generic at the call site and becomes the one the generated map
 * declares, which is derived from the Rust row. Callers that deliberately narrow to a
 * partial shape — `avatar-state`, whose union no single frontend type covers — keep
 * their own explicit `listen<Narrow>(NAME, …)`; that is still typo-proof, because the
 * NAME is a constant either way.
 */
export function listenEvent<K extends keyof EventPayloadMap>(
  name: K,
  handler: (event: Event<EventPayloadMap[K]>) => void,
): Promise<UnlistenFn> {
  return listen<EventPayloadMap[K]>(name, handler);
}
