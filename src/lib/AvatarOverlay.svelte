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
  import { avatar as avatarSettings } from './settings/store';
  import {
    resolveImageSrc,
    resolveTransitionSrc,
    isVideoSrc,
  } from './avatarConfig';

  /// Default crossfade duration when the user has disabled video transitions
  /// (empty path or duration_ms=0). Short enough to feel snappy, long enough
  /// to read as a smooth swap rather than a flicker.
  const FADE_MS = 150;

  let displayedSrc = $state<string>('');
  let displayedState: AvatarState = 'Idle';
  let transitionTimer: ReturnType<typeof setTimeout> | null = null;
  let isFirstRender = true;
  let unlisten: (() => void) | undefined;

  // Derived layout values for the inline style bag. These re-evaluate
  // automatically when any avatar setting changes (size/position/margin/
  // opacity), driving the live-update acceptance criteria 14–17.
  const positionStyles = $derived(
    [
      `--avatar-width: ${$avatarSettings.size.width_px}px`,
      `--avatar-height: ${$avatarSettings.size.height_px}px`,
      `--avatar-margin: ${$avatarSettings.margin_px}px`,
      `--avatar-opacity: ${$avatarSettings.opacity}`,
    ].join(';'),
  );

  const positionClass = $derived($avatarSettings.position);

  // When the underlying avatar image setting changes (e.g. user picks a new
  // Idle.png), re-resolve the displayed src for the *current* state.
  // Transitions only apply on state changes, not on settings changes —
  // settings updates are treated as instant swaps.
  $effect(() => {
    // Read the images slice so the effect re-runs on any image-setting change.
    const _ = $avatarSettings.images;
    if (transitionTimer === null) {
      const next = resolveImageSrc(_, displayedState);
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
    displayedSrc = resolveImageSrc(get(avatarSettings).images, 'Idle');
    unlisten = await startAvatarStateListener();
  });

  onDestroy(() => {
    unsubState();
    if (transitionTimer !== null) clearTimeout(transitionTimer);
    unlisten?.();
  });

  function handleStateChange(newState: AvatarState) {
    // Spec rule (M4): no transition on the very first render — the avatar
    // appears directly in its starting state.
    if (isFirstRender) {
      isFirstRender = false;
      displayedState = newState;
      displayedSrc = resolveImageSrc(get(avatarSettings).images, newState);
      return;
    }

    if (newState === displayedState && transitionTimer === null) return;

    if (transitionTimer !== null) {
      clearTimeout(transitionTimer);
      transitionTimer = null;
    }

    const settings = get(avatarSettings);
    const transitionSrc = resolveTransitionSrc(settings.transition);
    const stateImage = resolveImageSrc(settings.images, newState);

    if (transitionSrc && settings.transition.duration_ms > 0) {
      // Cache-bust so animated assets restart their playback when reused.
      displayedSrc = `${transitionSrc}?t=${Date.now()}`;
      transitionTimer = setTimeout(() => {
        displayedSrc = stateImage;
        displayedState = newState;
        transitionTimer = null;
      }, settings.transition.duration_ms);
      displayedState = newState;
    } else {
      displayedSrc = stateImage;
      displayedState = newState;
    }
  }
</script>

<div class="avatar-container {positionClass}" style={positionStyles}>
  {#if $avatarVisible}
    <div class="avatar-overlay">
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
     declared in TabBar.svelte) on top of the user's margin so the
     avatar clears the focused pane's tab bar instead of obscuring it.
     Without this, top-right / top-left avatars sit over the tab bar
     of whichever pane occupies that corner. Bottom-* positions don't
     need clearance — panes have no bottom bar. */
  .avatar-container.top-right {
    top: calc(var(--avatar-margin) + 32px);
    right: var(--avatar-margin);
    flex-direction: row;
  }
  .avatar-container.top-left {
    top: calc(var(--avatar-margin) + 32px);
    left: var(--avatar-margin);
    flex-direction: row-reverse;
  }
  .avatar-container.bottom-right {
    bottom: var(--avatar-margin);
    right: var(--avatar-margin);
    flex-direction: row;
  }
  .avatar-container.bottom-left {
    bottom: var(--avatar-margin);
    left: var(--avatar-margin);
    flex-direction: row-reverse;
  }

  .avatar-overlay {
    position: relative;
    width: var(--avatar-width);
    height: var(--avatar-height);
    opacity: var(--avatar-opacity);
    pointer-events: auto;
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

</style>
