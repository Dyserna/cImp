// V14 Phase B — image paste/drop attachments for the compose overlay.
//
// Three layers, split for testability (mirrors `compose/templates.ts`'s
// split between pure logic and Tauri/DOM glue):
//   - Pure functions (`isImagePath`, `filterImagePaths`, `clipboardHasImage`,
//     `appendAttachments`) — no DOM, no Tauri runtime. Exercised directly by
//     `attachments.test.ts`.
//   - `readClipboardImagePng` — converts the clipboard's raw RGBA image
//     (read via the Tauri clipboard plugin's `readImage`, NEVER
//     `navigator.clipboard` — WebView2 denies that, see the AI-tab
//     clipboard work) to PNG bytes using an offscreen canvas. DOM/canvas-
//     heavy and not unit-tested (jsdom has no real 2D canvas backend) —
//     `ComposeOverlay.svelte`'s paste handler is the only caller.
//   - `composeAttachImage` — the IPC wrapper that writes the bytes to this
//     app run's session-scoped attach dir (`attach.rs`) and returns the
//     saved path.

import { invoke } from '@tauri-apps/api/core';
import { readImage } from '@tauri-apps/plugin-clipboard-manager';

const IMAGE_EXTENSIONS = ['.png', '.jpg', '.jpeg', '.webp', '.gif'];

/// Whether `path`'s extension (case-insensitive) is one of the compose
/// overlay's accepted image types. Used to filter `tauri://drag-drop`
/// payload paths (B3) — non-image drops are ignored so files dropped on the
/// terminal underneath keep whatever behavior it already has.
export function isImagePath(path: string): boolean {
  const lower = path.toLowerCase();
  return IMAGE_EXTENSIONS.some((ext) => lower.endsWith(ext));
}

/// Filters a `tauri://drag-drop` payload's paths down to the accepted image
/// extensions, preserving order.
export function filterImagePaths(paths: string[]): string[] {
  return paths.filter(isImagePath);
}

/// Whether a `paste` event's clipboard carries an image (any MIME type
/// starting `image/`). Pure — takes the already-read `DataTransferItem`
/// type strings rather than the event itself, so the paste handler's
/// image-vs-text branch is testable without constructing a real
/// `ClipboardEvent`/`DataTransfer` in jsdom. A text-only paste (or a paste
/// with no clipboard data at all) returns `false`, and the caller leaves the
/// event completely alone — normal text paste is untouched.
export function clipboardHasImage(types: readonly string[]): boolean {
  return types.some((t) => t.startsWith('image/'));
}

/// Appends one `[image] <path>` line per attachment, followed by a single
/// trailing instruction line, to `content`. Both Claude Code and OpenCode
/// accept local image paths dropped straight into the prompt text as plain
/// path text — no special markup needed (verified against both; milestone
/// Decision 3). Returns `content` unchanged when there are no attachments,
/// so a plain-text submit is byte-identical to before Phase B.
export function appendAttachments(content: string, attachments: string[]): string {
  if (attachments.length === 0) return content;
  const lines = attachments.map((p) => `\n[image] ${p}`).join('');
  return `${content}${lines}\nRead the attached image file(s).`;
}

/// Reads the system clipboard's image (via the Tauri plugin — never
/// `navigator.clipboard`, which WebView2 denies) and re-encodes its raw RGBA
/// pixels to PNG bytes via an offscreen canvas, matching `save_png`'s `.png`
/// naming on the backend. Returns `null` when the clipboard has no image, or
/// re-encoding fails for any reason (missing canvas 2D context, zero-size
/// image) — callers treat that as "nothing to attach", not an error.
export async function readClipboardImagePng(): Promise<Uint8Array | null> {
  let rgba: Uint8Array;
  let width: number;
  let height: number;
  try {
    const image = await readImage();
    const [pixels, size] = await Promise.all([image.rgba(), image.size()]);
    rgba = pixels;
    width = size.width;
    height = size.height;
  } catch (e) {
    console.warn('clipboard readImage failed:', e);
    return null;
  }
  if (width === 0 || height === 0) return null;

  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  const ctx = canvas.getContext('2d');
  if (!ctx) return null;
  ctx.putImageData(new ImageData(new Uint8ClampedArray(rgba), width, height), 0, 0);

  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, 'image/png'));
  if (!blob) return null;
  return new Uint8Array(await blob.arrayBuffer());
}

/// IPC: writes PNG-encoded `bytes` to this app run's session-scoped attach
/// dir (`attach::save_png` backend-side) and returns the absolute path.
/// Tauri serializes `Vec<u8>` as a plain `number[]` (see `ipc.ts`'s note on
/// `ptyStart`) — convert both ways at the call boundary.
export function composeAttachImage(bytes: Uint8Array): Promise<string> {
  return invoke<string>('compose_attach_image', { bytes: Array.from(bytes) });
}
