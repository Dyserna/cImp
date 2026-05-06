// Minimal transient-toast store. Mount `Toast.svelte` once at the app
// root; call `showToast(...)` from anywhere to enqueue a message.
// Toasts auto-dismiss after `durationMs` (default 2.5s).

import { writable, type Writable } from 'svelte/store';

export interface Toast {
  id: number;
  message: string;
}

let nextId = 0;
export const toasts: Writable<Toast[]> = writable([]);

export function showToast(message: string, durationMs = 2500): void {
  const id = nextId++;
  toasts.update((list) => [...list, { id, message }]);
  setTimeout(() => {
    toasts.update((list) => list.filter((t) => t.id !== id));
  }, durationMs);
}
