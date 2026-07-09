import { describe, expect, test } from 'vitest';

import { computePreviewRect, DEVICE_PRESETS, isAllowedPreviewHost } from './policy';

describe('isAllowedPreviewHost', () => {
  test('localhost name is allowed', () => {
    expect(isAllowedPreviewHost('http://localhost:3000', false)).toBe(true);
    expect(isAllowedPreviewHost('http://localhost:3000/path?x=1', false)).toBe(true);
    expect(isAllowedPreviewHost('http://LOCALHOST:8080', false)).toBe(true);
  });

  test('loopback IPs are allowed', () => {
    expect(isAllowedPreviewHost('http://127.0.0.1:8080', false)).toBe(true);
    expect(isAllowedPreviewHost('http://127.5.5.5', false)).toBe(true);
    expect(isAllowedPreviewHost('http://[::1]:8080', false)).toBe(true);
  });

  test('RFC-1918 private ranges are allowed', () => {
    expect(isAllowedPreviewHost('http://10.0.0.5:3000', false)).toBe(true);
    expect(isAllowedPreviewHost('http://10.255.255.255', false)).toBe(true);
    expect(isAllowedPreviewHost('http://172.16.0.1', false)).toBe(true);
    expect(isAllowedPreviewHost('http://172.31.255.255', false)).toBe(true);
    expect(isAllowedPreviewHost('http://192.168.1.50', false)).toBe(true);
  });

  test('ranges adjacent to RFC-1918 are not mistaken for it', () => {
    expect(isAllowedPreviewHost('http://172.15.255.255', false)).toBe(false);
    expect(isAllowedPreviewHost('http://172.32.0.1', false)).toBe(false);
    expect(isAllowedPreviewHost('http://11.0.0.1', false)).toBe(false);
    expect(isAllowedPreviewHost('http://193.168.1.1', false)).toBe(false);
  });

  test('a public host is blocked unless allowRemote', () => {
    expect(isAllowedPreviewHost('https://example.com', false)).toBe(false);
    expect(isAllowedPreviewHost('https://example.com', true)).toBe(true);
    expect(isAllowedPreviewHost('http://8.8.8.8', false)).toBe(false);
    expect(isAllowedPreviewHost('http://8.8.8.8', true)).toBe(true);
  });

  test('a bare LAN hostname is not treated as local', () => {
    expect(isAllowedPreviewHost('http://my-nas.local', false)).toBe(false);
    expect(isAllowedPreviewHost('http://my-nas.local', true)).toBe(true);
  });

  test('a malformed URL is rejected regardless of allowRemote', () => {
    expect(isAllowedPreviewHost('not a url', false)).toBe(false);
    expect(isAllowedPreviewHost('not a url', true)).toBe(false);
    expect(isAllowedPreviewHost('', false)).toBe(false);
    expect(isAllowedPreviewHost('', true)).toBe(false);
  });

  test('hostless schemes are rejected regardless of allowRemote', () => {
    expect(isAllowedPreviewHost('file:///etc/passwd', true)).toBe(false);
    expect(isAllowedPreviewHost('javascript:alert(1)', true)).toBe(false);
    expect(isAllowedPreviewHost('about:blank', true)).toBe(false);
  });

  test('userinfo does not fool the host check', () => {
    expect(isAllowedPreviewHost('http://localhost@evil.com', false)).toBe(false);
  });
});

describe('computePreviewRect', () => {
  const container = { x: 10, y: 20, width: 1000, height: 600 };

  test('desktop preset (null) fills the container exactly', () => {
    expect(computePreviewRect(container, null)).toEqual(container);
  });

  test('a preset wider than the container fills it exactly, not overflows', () => {
    expect(computePreviewRect(container, 1200)).toEqual(container);
  });

  test('a narrower preset letterboxes, centered horizontally, full height', () => {
    const rect = computePreviewRect(container, 375);
    expect(rect.width).toBe(375);
    expect(rect.height).toBe(container.height);
    // Centered: left gap == right gap.
    const leftGap = rect.x - container.x;
    const rightGap = container.x + container.width - (rect.x + rect.width);
    expect(leftGap).toBeCloseTo(rightGap, 6);
  });

  test('exactly-container-width preset fills it (no spurious letterbox)', () => {
    expect(computePreviewRect(container, container.width)).toEqual(container);
  });

  test('every non-desktop preset in DEVICE_PRESETS letterboxes narrower than a wide container', () => {
    for (const preset of DEVICE_PRESETS) {
      if (preset.width === null) continue;
      const rect = computePreviewRect(container, preset.width);
      expect(rect.width).toBe(preset.width);
      expect(rect.width).toBeLessThan(container.width);
    }
  });
});
