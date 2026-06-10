<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import { fade } from 'svelte/transition';
  import {
    avatarState,
    avatarVisible,
    startAvatarStateListener,
    type AvatarState,
  } from './avatarState';
  import { avatar as avatarSettings, settings } from './settings/store';
  import { derived } from 'svelte/store';
  import {
    resolveImageSrc,
    resolveTransitionSrc,
    isVideoSrc,
    spriteManifestUrl,
    spriteBaseUrl,
    SPRITE_STATE_ANIMS,
  } from './avatarConfig';
  import { SpritePlayer } from './spritePlayer';

  // Active UI theme drives which `/avatar/<theme>/` subfolder the bundled
  // defaults resolve from. Subscribed separately because `avatarSettings`
  // is scoped to the `avatar` slice and won't fire on `ui.theme` changes.
  const uiTheme = derived(settings, (s) => s.ui.theme);

  /// Default crossfade duration when the user has disabled video transitions
  /// (empty path or duration_ms=0). Short enough to feel snappy, long enough
  /// to read as a smooth swap rather than a flicker.
  const FADE_MS = 150;

  let displayedSrc = $state<string>('');
  let displayedState: AvatarState = 'Idle';
  let transitionTimer: ReturnType<typeof setTimeout> | null = null;
  let isFirstRender = true;
  let unlisten: (() => void) | undefined;

  // --- Sprite variant ------------------------------------------------------
  // When `avatar.kind === 'sprite'`, the canvas below is shown instead of the
  // image/video element and a SpritePlayer drives it. The Rust state machine is
  // unchanged — the same 5 states map to animation rotation lists here.
  let canvasEl = $state<HTMLCanvasElement | null>(null);
  let player: SpritePlayer | null = null;
  /// Which set the live player has loaded; guards against reloading on every
  /// reactive tick when the set hasn't actually changed.
  let loadedSet = '';

  const avatarKind = $derived($avatarSettings.kind);
  const spriteSet = $derived($avatarSettings.sprite.set);

  function applySpriteState(state: AvatarState): void {
    player?.setAnims(state, SPRITE_STATE_ANIMS[state] ?? []);
  }

  // Player lifecycle + manifest (re)load. Re-runs when the render kind, the
  // chosen set, or the canvas element changes. Tears the player down whenever
  // we're not in sprite mode (the canvas is unmounted, so `canvasEl` is null).
  $effect(() => {
    const kind = avatarKind;
    const set = spriteSet;
    const el = canvasEl;
    if (kind !== 'sprite' || !el) {
      if (player) {
        player.destroy();
        player = null;
        loadedSet = '';
      }
      return;
    }
    if (!player) player = new SpritePlayer(el);
    if (set !== loadedSet) {
      loadedSet = set;
      const p = player;
      p.load(spriteManifestUrl(set), spriteBaseUrl(set))
        .then(() => {
          if (p === player) applySpriteState(displayedState);
        })
        .catch((e) => console.error('avatar sprite set failed to load', e));
    }
  });

  // Keep the canvas backing resolution in sync with the avatar size setting so
  // a resize re-fits the sprite immediately (drawing is nearest-neighbor, so
  // the backing store matches the on-screen box 1:1 and stays crisp).
  $effect(() => {
    const w = $avatarSettings.size.width_px;
    const h = $avatarSettings.size.height_px;
    const el = canvasEl;
    if (el && avatarKind === 'sprite') {
      el.width = w;
      el.height = h;
      player?.redraw();
    }
  });

  // Derived layout values for the inline style bag. These re-evaluate
  // automatically when any avatar setting changes (size/position/margin/
  // opacity), driving the live-update acceptance criteria 14–17.
  const positionStyles = $derived(
    [
      `--avatar-width: ${$avatarSettings.size.width_px}px`,
      `--avatar-height: ${$avatarSettings.size.height_px}px`,
      `--avatar-margin-x: ${$avatarSettings.margin.x_px}px`,
      `--avatar-margin-y: ${$avatarSettings.margin.y_px}px`,
      `--avatar-opacity: ${$avatarSettings.opacity}`,
      // Empty user value falls through to the theme's --waveform-color.
      `--avatar-border-color: ${$avatarSettings.waveform.color || 'var(--waveform-color)'}`,
    ].join(';'),
  );

  const positionClass = $derived($avatarSettings.position);

  // When the underlying avatar image setting or the UI theme changes
  // (e.g. user picks a new Idle.png, or switches modern-dark <-> tui),
  // re-resolve the displayed src for the *current* state. Transitions
  // only apply on state changes, not on settings changes — settings
  // updates are treated as instant swaps.
  $effect(() => {
    const images = $avatarSettings.images;
    const theme = $uiTheme;
    if (transitionTimer === null) {
      const next = resolveImageSrc(images, displayedState, theme);
      if (next !== displayedSrc) {
        displayedSrc = next;
      }
    }
  });

  const unsubState = avatarState.subscribe((newState) => {
    handleStateChange(newState);
  });

  onMount(async () => {
    // Initialize displayedSrc from current settings on first paint.
    displayedSrc = resolveImageSrc(
      get(avatarSettings).images,
      'Idle',
      get(settings).ui.theme,
    );
    unlisten = await startAvatarStateListener();
  });

  onDestroy(() => {
    unsubState();
    if (transitionTimer !== null) clearTimeout(transitionTimer);
    player?.destroy();
    player = null;
    unlisten?.();
  });

  function handleStateChange(newState: AvatarState) {
    // Sprite variant: no crossfade/transition machinery — the player just
    // switches its rotation list (a no-op when the state is unchanged). The
    // lifecycle $effect handles the very first paint once the manifest loads.
    if (get(avatarSettings).kind === 'sprite') {
      isFirstRender = false;
      displayedState = newState;
      applySpriteState(newState);
      return;
    }

    // Spec rule (M4): no transition on the very first render — the avatar
    // appears directly in its starting state.
    const theme = get(settings).ui.theme;

    if (isFirstRender) {
      isFirstRender = false;
      displayedState = newState;
      displayedSrc = resolveImageSrc(get(avatarSettings).images, newState, theme);
      return;
    }

    if (newState === displayedState && transitionTimer === null) return;

    if (transitionTimer !== null) {
      clearTimeout(transitionTimer);
      transitionTimer = null;
    }

    const a = get(avatarSettings);
    const transitionSrc = resolveTransitionSrc(a.transition, theme);
    const stateImage = resolveImageSrc(a.images, newState, theme);

    if (transitionSrc && a.transition.duration_ms > 0) {
      // Cache-bust so animated assets restart their playback when reused.
      displayedSrc = `${transitionSrc}?t=${Date.now()}`;
      transitionTimer = setTimeout(() => {
        displayedSrc = stateImage;
        displayedState = newState;
        transitionTimer = null;
      }, a.transition.duration_ms);
      displayedState = newState;
    } else {
      displayedSrc = stateImage;
      displayedState = newState;
    }
  }
</script>

<div class="avatar-container {positionClass}" style={positionStyles}>
  {#if $avatarVisible}
    <div class="avatar-overlay" class:borderless={!$avatarSettings.show_border}>
      {#if avatarKind === 'sprite'}
        <!-- Sprite variant: a single persistent canvas driven by SpritePlayer.
             No {#key} remount — the player paints frames in place. -->
        <canvas bind:this={canvasEl} class="avatar-image avatar-sprite"></canvas>
      {:else}
        <!-- {#key} remounts the element on src change so video autoplay
             restarts and image swaps get the fade transition. Both branches
             are absolute-positioned so the old + new elements briefly overlap
             during the fade — giving a true crossfade rather than a flicker. -->
        {#key displayedSrc}
          {#if isVideoSrc(displayedSrc)}
            <video
              src={displayedSrc}
              class="avatar-image"
              autoplay
              loop
              muted
              playsinline
              transition:fade={{ duration: FADE_MS }}
            ></video>
          {:else}
            <img
              src={displayedSrc}
              alt="Avatar"
              class="avatar-image"
              transition:fade={{ duration: FADE_MS }}
            />
          {/if}
        {/key}
      {/if}
    </div>
  {/if}
</div>

<style>
  .avatar-container {
    position: absolute;
    display: flex;
    align-items: stretch;
    /* Container itself doesn't capture clicks — only the avatar image.
       Empty space passes clicks through to the terminal underneath. */
    pointer-events: none;
    z-index: 10;
  }
  /* Top-positioned variants add 32px (one per-pane tab bar height,
     declared in TabBar.svelte) on top of the user's Y margin so the
     avatar clears the focused pane's tab bar instead of obscuring it.
     Without this, top-right / top-left avatars sit over the tab bar
     of whichever pane occupies that corner. Bottom-* positions don't
     need clearance — panes have no bottom bar. */
  .avatar-container.top-right {
    top: calc(var(--avatar-margin-y) + 32px);
    right: var(--avatar-margin-x);
    flex-direction: row;
  }
  .avatar-container.top-left {
    top: calc(var(--avatar-margin-y) + 32px);
    left: var(--avatar-margin-x);
    flex-direction: row-reverse;
  }
  .avatar-container.bottom-right {
    bottom: var(--avatar-margin-y);
    right: var(--avatar-margin-x);
    flex-direction: row;
  }
  .avatar-container.bottom-left {
    bottom: var(--avatar-margin-y);
    left: var(--avatar-margin-x);
    flex-direction: row-reverse;
  }

  .avatar-overlay {
    position: relative;
    width: var(--avatar-width);
    height: var(--avatar-height);
    opacity: var(--avatar-opacity);
    pointer-events: auto;
    /* 1px frame in the user's waveform color; outline keeps the image's
       layout box untouched (no inward squeeze) and renders cleanly over
       the terminal underneath. */
    outline: 1px solid var(--avatar-border-color);
  }

  /* Border toggled off via Settings → Avatar → Show border. */
  .avatar-overlay.borderless {
    outline: none;
  }

  .avatar-image {
    /* Absolute fill so the old and new key'd elements occupy the same box
       during a fade — without this, the new mount would push the old one
       sideways before its out-transition completes. */
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    object-fit: contain;
    /* No background — source-image alpha composes against the terminal. */
    user-select: none;
    -webkit-user-drag: none;
  }

  .avatar-sprite {
    /* Pixel art: never interpolate when the canvas element is scaled by the
       layout. The SpritePlayer also disables context smoothing when it draws
       the upscaled frame, so the sprite stays crisp end-to-end. */
    image-rendering: pixelated;
    image-rendering: crisp-edges;
  }

</style>
