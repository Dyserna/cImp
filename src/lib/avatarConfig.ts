import type { AvatarState } from './avatarState';

export type AvatarPosition = 'top-right' | 'top-left' | 'bottom-right' | 'bottom-left';

export interface AvatarConfig {
  images: Record<AvatarState, string>;
  transition: { path: string | null; durationMs: number };
  layout: {
    widthPx: number;
    heightPx: number;
    position: AvatarPosition;
    marginPx: number;
    opacity: number;
  };
}

// Hardcoded for M4. Settings UI / persistence land in M6.
//
// All asset paths are absolute from the WebView root — files live under
// /public/avatar/ in the repo, which Vite serves as `/avatar/...` in dev
// and bundles into `dist/avatar/...` for builds.
export const avatarConfig: AvatarConfig = {
  images: {
    Idle: '/avatar/Idle.mp4',
    Listening: '/avatar/Listening.mp4',
    Thinking: '/avatar/Thinking.mp4',
    Speaking: '/avatar/Speaking.mp4',
    Error: '/avatar/Error.mp4',
  },
  transition: {
    path: '/avatar/Transition.mp4',
    durationMs: 400,
  },
  layout: {
    widthPx: 240,
    heightPx: 240,
    position: 'top-right',
    marginPx: 16,
    opacity: 0.8,
  },
};
