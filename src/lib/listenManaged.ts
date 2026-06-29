import { onDestroy } from 'svelte';
import type { UnlistenFn } from '@tauri-apps/api/event';

/**
 * Register a Tauri event listener whose teardown is guaranteed to run when the
 * current component is destroyed — even if the component unmounts *before* the
 * async `register()` (i.e. `listen(...)`) promise resolves.
 *
 * The common leak this prevents: an `async onMount` that does
 * `unlisten = await listen(...)`, while `onDestroy` already ran (with `unlisten`
 * still null) — so the listener registers after teardown and is never removed.
 *
 * MUST be called synchronously during component init (it calls `onDestroy`),
 * not from inside an `async onMount` body.
 */
export function listenManaged(register: () => Promise<UnlistenFn>): void {
  let unlisten: UnlistenFn | null = null;
  let destroyed = false;
  void register().then((fn) => {
    // Lost the race: the component was already destroyed by the time the
    // listener registered — tear it down immediately so it can't leak.
    if (destroyed) fn();
    else unlisten = fn;
  });
  onDestroy(() => {
    destroyed = true;
    unlisten?.();
    unlisten = null;
  });
}
