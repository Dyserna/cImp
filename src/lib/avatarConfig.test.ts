// Unit tests for avatar asset URL resolution: bundled vs user-override paths,
// theme fallback, and the legacy pre-theming path redirect.

import { describe, it, expect, vi } from 'vitest';
import type { AvatarImages, TransitionSettings } from './settings/types';

vi.mock('@tauri-apps/api/core', () => ({
  convertFileSrc: (p: string) => `asset://${p}`,
}));

const {
  resolveImageSrc,
  resolveTransitionSrc,
  isVideoSrc,
  spriteSetName,
  spriteManifestUrl,
} = await import('./avatarConfig');

const NO_IMAGES: AvatarImages = {
  idle: null,
  listening: null,
  thinking: null,
  speaking: null,
  error: null,
};

describe('resolveImageSrc', () => {
  it('serves the bundled default for the active theme when no override exists', () => {
    expect(resolveImageSrc(NO_IMAGES, 'Speaking', 'tui-grey')).toBe(
      '/avatar/tui-grey/Speaking.mp4',
    );
  });

  it('falls back to the default theme folder for unknown themes', () => {
    expect(resolveImageSrc(NO_IMAGES, 'Idle', 'my-custom-theme')).toBe(
      '/avatar/tui-orange/Idle.mp4',
    );
  });

  it('converts a user override disk path to an asset URL', () => {
    const images = { ...NO_IMAGES, idle: 'C:\\pics\\idle.png' };
    expect(resolveImageSrc(images, 'Idle', 'tui-orange')).toBe('asset://C:\\pics\\idle.png');
  });

  it('converts UNC paths to asset URLs', () => {
    const images = { ...NO_IMAGES, error: '\\\\server\\share\\err.png' };
    expect(resolveImageSrc(images, 'Error', 'tui-orange')).toBe(
      'asset://\\\\server\\share\\err.png',
    );
  });

  it('redirects a legacy top-level bundled override into the active theme folder', () => {
    const images = { ...NO_IMAGES, thinking: '/avatar/Thinking.mp4' };
    expect(resolveImageSrc(images, 'Thinking', 'tui-grey')).toBe(
      '/avatar/tui-grey/Thinking.mp4',
    );
  });

  it('passes already-themed bundled overrides through unchanged', () => {
    const images = { ...NO_IMAGES, listening: '/avatar/tui-grey/Listening.mp4' };
    expect(resolveImageSrc(images, 'Listening', 'tui-orange')).toBe(
      '/avatar/tui-grey/Listening.mp4',
    );
  });
});

describe('resolveTransitionSrc', () => {
  it('returns null when transitions are disabled (empty/null path)', () => {
    const off: TransitionSettings = { path: null, duration_ms: 400 };
    expect(resolveTransitionSrc(off, 'tui-orange')).toBeNull();
    expect(resolveTransitionSrc({ path: '', duration_ms: 400 }, 'tui-orange')).toBeNull();
  });

  it('redirects the legacy bundled transition into the theme folder', () => {
    const t: TransitionSettings = { path: '/avatar/Transition.mp4', duration_ms: 400 };
    expect(resolveTransitionSrc(t, 'tui-grey')).toBe('/avatar/tui-grey/Transition.mp4');
  });
});

describe('isVideoSrc', () => {
  it('detects video extensions, ignoring query strings and case', () => {
    expect(isVideoSrc('/avatar/tui-orange/Idle.mp4?t=123')).toBe(true);
    expect(isVideoSrc('asset://C:\\vid\\x.MOV')).toBe(true);
    expect(isVideoSrc('/avatar/tui-orange/Idle.png')).toBe(false);
  });
});

describe('spriteSetName', () => {
  it('falls back to the default set for unknown names', () => {
    expect(spriteSetName('claudeSprites')).toBe('claudeSprites');
    expect(spriteSetName('typo-set')).toBe('impSprites');
    expect(spriteManifestUrl('typo-set')).toBe('/sprites/impSprites/manifest.json');
  });
});
