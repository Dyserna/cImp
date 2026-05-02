// Bundled default avatar assets + helpers for resolving user-overridden
// paths into URLs the WebView can load. Files under `/public/avatar/` are
// served by Vite as `/avatar/...` in dev and bundled into `dist/avatar/...`
// for builds. User-picked paths from the file dialog are absolute disk
// paths and need `convertFileSrc` to become `asset://` URLs.

import { convertFileSrc } from '@tauri-apps/api/core';
import type { AvatarState } from './avatarState';
import type { AvatarImages, TransitionSettings } from './settings/types';

const BUNDLED_IMAGES: Record<AvatarState, string> = {
  Idle: '/avatar/Idle.mp4',
  Listening: '/avatar/Listening.mp4',
  Thinking: '/avatar/Thinking.mp4',
  Speaking: '/avatar/Speaking.mp4',
  Error: '/avatar/Error.mp4',
};

export const BUNDLED_TRANSITION = '/avatar/Transition.mp4';

/// Resolve the URL for the avatar image associated with `state`. User
/// overrides come through as absolute disk paths and run through
/// `convertFileSrc` so the WebView can load them via the asset protocol.
export function resolveImageSrc(images: AvatarImages, state: AvatarState): string {
  const key = state.toLowerCase() as keyof AvatarImages;
  const override = images[key];
  if (override) return resolvePath(override);
  return BUNDLED_IMAGES[state] ?? BUNDLED_IMAGES.Idle;
}

/// `null` means no transition: the avatar snaps directly between states.
/// Per the M6 spec, an empty/null path disables transitions entirely.
export function resolveTransitionSrc(transition: TransitionSettings): string | null {
  if (!transition.path) return null;
  return resolvePath(transition.path);
}

/// Distinguish bundled (vite-served) URLs from absolute disk paths. Bundled
/// URLs start with `/` and have no Windows drive letter; disk paths start
/// with a drive letter or `/` on POSIX. We use the presence of `:` or `\`
/// as the disk-path heuristic; everything else is treated as a vite URL.
function resolvePath(p: string): string {
  if (/^[a-zA-Z]:[\\/]/.test(p) || p.startsWith('\\\\') || p.startsWith('//')) {
    return convertFileSrc(p);
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
