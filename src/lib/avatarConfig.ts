// Bundled default avatar assets + helpers for resolving user-overridden
// paths into URLs the WebView can load. Source files live at the
// top-level `avatars/<theme>/` folder; the `ccimp-avatars` Vite plugin
// (see `vite.config.ts`) serves them at `/avatar/<theme>/...` in dev and
// copies them into `dist/avatar/<theme>/...` for builds. User-picked
// paths from the file dialog are absolute disk paths and need
// `convertFileSrc` to become `asset://` URLs.

import { convertFileSrc } from '@tauri-apps/api/core';
import type { AvatarState } from './avatarState';
import type { AvatarImages, TransitionSettings } from './settings/types';

const STATE_FILES: Record<AvatarState, string> = {
  Idle: 'Idle.mp4',
  Listening: 'Listening.mp4',
  Thinking: 'Thinking.mp4',
  Speaking: 'Speaking.mp4',
  Error: 'Error.mp4',
};

/// Themes that ship a bundled avatar set under `avatars/<theme>/`.
/// Unknown values (custom themes, typos, legacy strings) fall back to
/// `modern-dark`, the original avatar set, so the overlay never breaks.
/// Per the theme isolation policy in `src/theme.css`, every theme owns
/// its own avatar folder — derivative themes get a copy of the source
/// folder, never a shared one.
const KNOWN_THEMES = new Set(['modern-dark', 'tui-yellow', 'tui-purple', 'tui-orange']);
const FALLBACK_THEME = 'modern-dark';

function themeFolder(theme: string): string {
  return KNOWN_THEMES.has(theme) ? theme : FALLBACK_THEME;
}

function bundledImage(theme: string, state: AvatarState): string {
  return `/avatar/${themeFolder(theme)}/${STATE_FILES[state] ?? STATE_FILES.Idle}`;
}

/// Resolve the URL for the avatar image associated with `state`. User
/// overrides come through as absolute disk paths and run through
/// `convertFileSrc` so the WebView can load them via the asset protocol.
export function resolveImageSrc(
  images: AvatarImages,
  state: AvatarState,
  theme: string,
): string {
  const key = state.toLowerCase() as keyof AvatarImages;
  const override = images[key];
  if (override) return resolvePath(override, theme);
  return bundledImage(theme, state);
}

/// `null` means no transition: the avatar snaps directly between states.
/// Per the M6 spec, an empty/null path disables transitions entirely.
export function resolveTransitionSrc(
  transition: TransitionSettings,
  theme: string,
): string | null {
  if (!transition.path) return null;
  return resolvePath(transition.path, theme);
}

/// Distinguish bundled (vite-served) URLs from absolute disk paths. Bundled
/// URLs start with `/` and have no Windows drive letter; disk paths start
/// with a drive letter or `/` on POSIX. We use the presence of `:` or `\`
/// as the disk-path heuristic; everything else is treated as a vite URL.
///
/// Legacy bundled URLs like `/avatar/Transition.mp4` (the pre-theming
/// schema default) get redirected into the active theme's subfolder so
/// settings.json files written before this change auto-follow the theme.
function resolvePath(p: string, theme: string): string {
  if (/^[a-zA-Z]:[\\/]/.test(p) || p.startsWith('\\\\') || p.startsWith('//')) {
    return convertFileSrc(p);
  }
  if (p.startsWith('/avatar/')) {
    const rest = p.slice('/avatar/'.length);
    // Top-level bundled file (no further slash) = legacy default; redirect
    // into the active theme's subfolder. Already-themed paths pass through.
    if (rest.length > 0 && !rest.includes('/')) {
      return `/avatar/${themeFolder(theme)}/${rest}`;
    }
    return p;
  }
  if (p.startsWith('/')) {
    return p; // bundled URL
  }
  return convertFileSrc(p);
}

export function isVideoSrc(src: string): boolean {
  const path = src.split('?')[0].toLowerCase();
  return path.endsWith('.mp4') || path.endsWith('.webm') || path.endsWith('.mov');
}

// --- Sprite avatar variant -------------------------------------------------
//
// Sprite sets live under the top-level `sprites/<set>/` folder, served to the
// WebView at `/sprites/<set>/...` by the `ccimp-sprites` Vite plugin (dev) and
// embedded under `dist/sprites/` for builds — exactly mirroring how `avatars/`
// maps to `/avatar/`. Each set holds a `manifest.json` (Clawdmeter format:
// `{ tile, animations: { "<name>": { slug, category, frames: [{file, hold_ms}] } } }`)
// plus one frame subfolder per animation.

/// Sprite sets that ship bundled under `sprites/`. Unknown values fall back to
/// `claudeSprites` so a stale/typo'd setting never leaves the overlay blank.
/// Add new bundled sets here (and drop the folder under `sprites/`).
const KNOWN_SPRITE_SETS = new Set(['claudeSprites']);
const FALLBACK_SPRITE_SET = 'claudeSprites';

export function spriteSetName(set: string): string {
  return KNOWN_SPRITE_SETS.has(set) ? set : FALLBACK_SPRITE_SET;
}

/// Base URL for a bundled sprite set's assets. Frame files from the manifest
/// are appended to this (`<base>/<frame.file>`).
export function spriteBaseUrl(set: string): string {
  return `/sprites/${spriteSetName(set)}`;
}

export function spriteManifestUrl(set: string): string {
  return `${spriteBaseUrl(set)}/manifest.json`;
}

// Per-state animation behaviour now lives in each set's `manifest.json`
// `groups` (state -> animation list, rotated when >1) and is resolved by
// SpritePlayer.groupFor() in AvatarOverlay — no longer hardcoded here, so a new
// sprite set fully defines its own behaviour without touching app code.
