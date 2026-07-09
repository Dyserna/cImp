// V14 Phase F — pure, testable Preview-tab helpers: the navigation-policy
// host classifier and the device-preset letterbox rect math. Both are pure
// (no DOM, no Tauri runtime) so `policy.test.ts` exercises them directly,
// mirroring `compose/attachments.ts`'s split between pure logic and glue.
//
// `isAllowedPreviewHost` is a FRONTEND MIRROR of the Rust
// `preview::is_allowed_preview_host` — used for the toolbar's own pre-flight
// check (so an obviously-disallowed URL never even round-trips to the
// backend before the user sees why), NOT the enforcement point. The backend
// re-checks independently at `preview_open`/`preview_navigate` and again at
// the webview's own `on_navigation`/`on_new_window` handlers — a frontend
// bypass of this function can't widen what the embedded webview actually
// loads.

/// Fallback URL for a freshly created Preview tab with no remembered
/// `Settings.preview_last_url` — mirrors `preview::DEFAULT_PREVIEW_URL`
/// (Rust).
export const DEFAULT_PREVIEW_URL = 'http://localhost:3000';

/// True when `url` may be loaded directly in a Preview tab. Mirrors the
/// Rust classifier bit-for-bit: `localhost` / `127.0.0.1` / `::1` / RFC-1918
/// private ranges (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16) are allowed
/// unconditionally; anything else needs `allowRemote`. A malformed URL, or
/// one with no host at all (`file://`, `javascript:`, `about:blank`), is
/// rejected UNCONDITIONALLY — `allowRemote` widens which HOSTS are trusted,
/// never what counts as a well-formed navigation target.
export function isAllowedPreviewHost(url: string, allowRemote: boolean): boolean {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  // `URL.hostname` resolves the real host (ignoring any userinfo like
  // `user@`), and is empty for hostless schemes (file:/javascript:/about:).
  const host = parsed.hostname;
  if (!host) {
    return false;
  }
  if (allowRemote) {
    return true;
  }
  return isLocalHost(host);
}

/// `localhost` (name), `::1`, or an RFC-1918/loopback IPv4 literal. A bare
/// hostname other than `localhost` (e.g. a LAN mDNS name) is NOT local —
/// classifying it would need a DNS lookup, which this pure function
/// deliberately never performs.
function isLocalHost(host: string): boolean {
  // `URL.hostname` keeps IPv6 literals bracketed (`"[::1]"`); strip them
  // before comparing/parsing.
  const bare = host.startsWith('[') && host.endsWith(']') ? host.slice(1, -1) : host;
  if (bare.toLowerCase() === 'localhost') {
    return true;
  }
  if (bare === '::1') {
    return true;
  }
  const octets = parseIpv4(bare);
  if (!octets) {
    return false;
  }
  const [a, b] = octets;
  if (a === 127) return true; // 127.0.0.0/8 loopback
  if (a === 10) return true; // 10.0.0.0/8
  if (a === 172 && b >= 16 && b <= 31) return true; // 172.16.0.0/12
  if (a === 192 && b === 168) return true; // 192.168.0.0/16
  return false;
}

/// Parses a dotted-quad IPv4 literal, rejecting anything with an octet over
/// 255 (so e.g. "999.0.0.1" doesn't get treated as a valid address at all).
/// `null` for anything that isn't exactly 4 dot-separated numbers.
function parseIpv4(s: string): [number, number, number, number] | null {
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(s);
  if (!m) {
    return null;
  }
  const nums = [m[1], m[2], m[3], m[4]].map(Number);
  if (nums.some((n) => n > 255)) {
    return null;
  }
  return nums as [number, number, number, number];
}

// ── Device presets + letterbox rect math ────────────────────────────────

export interface DevicePreset {
  id: 'mobile' | 'tablet' | 'desktop';
  label: string;
  /// `null` for "desktop" — fill the available rect, no letterboxing.
  width: number | null;
}

/// The toolbar's device-width presets (Phase F3). Order is the display
/// order of the toolbar's preset buttons.
export const DEVICE_PRESETS: readonly DevicePreset[] = [
  { id: 'mobile', label: 'Mobile', width: 375 },
  { id: 'tablet', label: 'Tablet', width: 768 },
  { id: 'desktop', label: 'Desktop', width: null },
];

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/// Given the pane's measured content rect and an optional device-width
/// preset, compute the (possibly narrower, horizontally centered) rect the
/// embedded child webview should occupy. `null` (desktop) or a preset wider
/// than the container fills it exactly; a narrower preset letterboxes —
/// centered horizontally, full height — so e.g. the "Mobile" preset doesn't
/// stretch to the pane's full width and a later snapshot isn't inflated
/// beyond the device's own viewport (token cost of oversized screenshots is
/// the point of the feature). All values are logical (CSS) pixels, matching
/// what `getBoundingClientRect()` reports regardless of the OS display scale
/// factor — see `preview_set_rect`'s doc comment for why that also keeps a
/// capture at CSS-pixel, not HiDPI-inflated, scale.
export function computePreviewRect(container: Rect, deviceWidth: number | null): Rect {
  if (deviceWidth === null || deviceWidth >= container.width) {
    return { ...container };
  }
  const x = container.x + (container.width - deviceWidth) / 2;
  return { x, y: container.y, width: deviceWidth, height: container.height };
}
