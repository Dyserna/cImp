// V14 Phase F: tracks which Preview tab ids currently have a LIVE backend
// child webview (open, possibly hidden) — read synchronously by
// `PreviewToolbar.svelte`'s mount effect to decide `previewShow` (the
// backend webview already exists, just hidden — e.g. the pane switched back
// to this tab) vs `previewOpen` (first time, or after `previewClose`).
//
// Not a Svelte store: nothing renders off this directly, and a plain module
// -level `Set` is simpler than the store machinery for something read once
// per mount/unmount rather than subscribed to.
//
// This is the frontend half of the "hide (not destroy) on tab-switch away;
// destroy on close" lifecycle (Phase F2): `PreviewToolbar` mounts/unmounts
// with Svelte's `{#if}` in `Pane.svelte` (there is no persistent per-tab
// component the way xterm hosts get via `terminals.ts`'s attach/detach), so
// on unmount it must tell hide vs. close apart — checking whether the tab
// still exists in the `tabs` store (switched away) vs. no longer exists
// (closed) — and mark/unmark this set accordingly.
const openIds = new Set<string>();

export function markPreviewOpen(tabId: string): void {
  openIds.add(tabId);
}

export function markPreviewClosed(tabId: string): void {
  openIds.delete(tabId);
}

export function isPreviewBackendOpen(tabId: string): boolean {
  return openIds.has(tabId);
}
