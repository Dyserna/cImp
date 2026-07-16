// One-shot "select this file in the Graph view" channel — the Workbench's
// per-file jump button (and the Code Audit finding jump) writes here;
// ToolActivityView watches it to flip to its Graph view section, and
// GraphView.svelte consumes it. Same imperative store-bus pattern as
// composeState.ts; the nonce makes repeat requests for the same path
// observable, and GraphView clears the store after consuming so a later
// (re)mount doesn't replay a stale request.
import { writable } from 'svelte/store';

export interface GraphRevealRequest {
  /// Repo-relative forward-slashed path (the Workbench diff format, which is
  /// also exactly the graph's node-id suffix: id === `file:` + path).
  path: string;
  nonce: number;
}

let nonce = 0;

export const graphReveal = writable<GraphRevealRequest | null>(null);

export function revealFileInGraph(path: string): void {
  nonce += 1;
  graphReveal.set({ path, nonce });
}

export function clearGraphReveal(): void {
  graphReveal.set(null);
}
