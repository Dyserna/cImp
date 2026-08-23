import { describe, expect, test, vi } from 'vitest';

// `attachments.ts` imports the Tauri clipboard plugin and `invoke` at module
// top level (for `readClipboardImagePng`/`composeAttachImage`); neither is
// exercised by the pure-function tests below, but jsdom has no real Tauri
// runtime, so importing the module unmocked would throw. Same pattern as
// `compose/templates.test.ts`.
vi.mock('@tauri-apps/plugin-clipboard-manager', () => ({
  readImage: vi.fn(),
}));
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import {
  isImagePath,
  filterImagePaths,
  clipboardHasImage,
  appendAttachments,
} from './attachments';

describe('isImagePath', () => {
  test('accepts the supported image extensions', () => {
    expect(isImagePath('C:\\shots\\a.png')).toBe(true);
    expect(isImagePath('/home/x/b.jpg')).toBe(true);
    expect(isImagePath('/home/x/c.jpeg')).toBe(true);
    expect(isImagePath('/home/x/d.webp')).toBe(true);
    expect(isImagePath('/home/x/e.gif')).toBe(true);
  });

  test('is case-insensitive', () => {
    expect(isImagePath('/home/x/SCREENSHOT.PNG')).toBe(true);
  });

  test('rejects non-image extensions', () => {
    expect(isImagePath('/home/x/notes.txt')).toBe(false);
    expect(isImagePath('/home/x/archive.zip')).toBe(false);
    expect(isImagePath('/home/x/video.mp4')).toBe(false);
  });

  test('rejects a path with no extension', () => {
    expect(isImagePath('/home/x/README')).toBe(false);
  });
});

describe('filterImagePaths', () => {
  test('keeps only image paths, preserving order', () => {
    const paths = ['/a/one.png', '/a/notes.txt', '/a/two.jpg', '/a/archive.zip'];
    expect(filterImagePaths(paths)).toEqual(['/a/one.png', '/a/two.jpg']);
  });

  test('empty input yields empty output', () => {
    expect(filterImagePaths([])).toEqual([]);
  });

  test('an all-non-image drop yields an empty array (ignored, not an error)', () => {
    expect(filterImagePaths(['/a/notes.txt', '/a/readme.md'])).toEqual([]);
  });
});

describe('clipboardHasImage', () => {
  test('true when an image/* MIME type is present', () => {
    expect(clipboardHasImage(['image/png'])).toBe(true);
    expect(clipboardHasImage(['text/plain', 'image/jpeg'])).toBe(true);
  });

  test('false for a text-only paste', () => {
    expect(clipboardHasImage(['text/plain'])).toBe(false);
    expect(clipboardHasImage(['text/html', 'text/plain'])).toBe(false);
  });

  test('false for no clipboard types at all', () => {
    expect(clipboardHasImage([])).toBe(false);
  });
});

describe('appendAttachments', () => {
  // V40 Phase E: the instruction is the harness's, supplied by the caller from
  // `harness_instructions`. The text below is what the backend inventory holds
  // for the `attachment` slot today.
  const INSTRUCTION = 'Read the attached image file(s).';

  test('returns content unchanged when there are no attachments', () => {
    expect(appendAttachments('hello world', [], INSTRUCTION)).toBe('hello world');
    expect(appendAttachments('', [], INSTRUCTION)).toBe('');
  });

  test('appends one [image] line per attachment plus a trailing instruction', () => {
    const out = appendAttachments('check this out', ['/tmp/cimp-attach/s1/0.png'], INSTRUCTION);
    expect(out).toBe(
      'check this out\n[image] /tmp/cimp-attach/s1/0.png\nRead the attached image file(s).',
    );
  });

  test('multiple attachments each get their own line, instruction appears once', () => {
    const out = appendAttachments('msg', ['/a/0.png', '/a/1.png'], INSTRUCTION);
    expect(out).toBe('msg\n[image] /a/0.png\n[image] /a/1.png\nRead the attached image file(s).');
  });

  test('an image-only submit (empty text draft) still produces a non-empty message', () => {
    const out = appendAttachments('', ['/a/0.png'], INSTRUCTION);
    expect(out).toBe('\n[image] /a/0.png\nRead the attached image file(s).');
    expect(out.length).toBeGreaterThan(0);
  });

  test('an unavailable instruction drops the line, never the attachments', () => {
    // The backend could not be asked (IPC failure) or a harness declares no
    // attachment text: the paths must still reach the tab.
    const out = appendAttachments('msg', ['/a/0.png'], '');
    expect(out).toBe('msg\n[image] /a/0.png');
  });
});
