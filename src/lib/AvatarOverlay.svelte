<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import {
    avatarState,
    avatarVisible,
    startAvatarStateListener,
    type AvatarState,
  } from './avatarState';
  import { avatarConfig } from './avatarConfig';

  let displayedSrc = $state<string>(avatarConfig.images.Idle);
  let displayedState: AvatarState = 'Idle';
  let transitionTimer: ReturnType<typeof setTimeout> | null = null;
  let isFirstRender = true;
  let unlisten: (() => void) | undefined;

  // Track the latest state via subscription so we can run transition logic
  // imperatively (which a `$:`-style block in Svelte 5 wouldn't give us).
  const unsubState = avatarState.subscribe((newState) => {
    handleStateChange(newState);
  });

  onMount(async () => {
    unlisten = await startAvatarStateListener();
  });

  onDestroy(() => {
    unsubState();
    if (transitionTimer !== null) clearTimeout(transitionTimer);
    unlisten?.();
  });

  function handleStateChange(newState: AvatarState) {
    // Spec rule 22: no transition on the very first render — the avatar
    // appears directly in its starting state.
    if (isFirstRender) {
      isFirstRender = false;
      displayedState = newState;
      displayedSrc = avatarConfig.images[newState] ?? avatarConfig.images.Idle;
      return;
    }

    // No-op if the state didn't actually change AND we aren't currently
    // mid-transition. Mid-transition + same state shouldn't happen, but if
    // it ever does we let the running transition complete on its own.
    if (newState === displayedState && transitionTimer === null) return;

    if (transitionTimer !== null) {
      clearTimeout(transitionTimer);
      transitionTimer = null;
    }

    const transition = avatarConfig.transition;
    const stateImage = avatarConfig.images[newState] ?? avatarConfig.images.Idle;

    if (transition.path && transition.durationMs > 0) {
      // Cache-bust the transition asset so an animated GIF/WebP restarts
      // its animation each time it plays — without this, the second play
      // can render the last frame instantly because the browser keeps the
      // animation paused at end.
      displayedSrc = `${transition.path}?t=${Date.now()}`;
      transitionTimer = setTimeout(() => {
        displayedSrc = stateImage;
        displayedState = newState;
        transitionTimer = null;
      }, transition.durationMs);
      displayedState = newState;
    } else {
      displayedSrc = stateImage;
      displayedState = newState;
    }
  }

  function toggleVisibility() {
    avatarVisible.update((v) => !v);
  }

  function openSettings() {
    // M6: open settings window. M4 placeholder.
    console.log('settings clicked');
  }

  // CSS variable bag for the configured layout. Recomputed if layout ever
  // becomes reactive (M6 settings live-update); for now this only runs once.
  const positionStyles = (() => {
    const { widthPx, heightPx, marginPx, opacity } = avatarConfig.layout;
    return [
      `--avatar-width: ${widthPx}px`,
      `--avatar-height: ${heightPx}px`,
      `--avatar-margin: ${marginPx}px`,
      `--avatar-opacity: ${opacity}`,
    ].join(';');
  })();

  const positionClass = avatarConfig.layout.position;

  // Choose <video> vs <img> per asset so the same config slot can hold either
  // kind. Strip the cache-bust query before checking the extension.
  function isVideoSrc(src: string): boolean {
    const path = src.split('?')[0].toLowerCase();
    return path.endsWith('.mp4') || path.endsWith('.webm') || path.endsWith('.mov');
  }
</script>

<div class="avatar-container {positionClass}" style={positionStyles}>
  {#if $avatarVisible}
    <div class="avatar-overlay">
      {#if isVideoSrc(displayedSrc)}
        <!-- {#key} remounts the element on src change so autoplay restarts
             playback. Without this, swapping src on an existing <video>
             does not reliably reset currentTime back to 0. -->
        {#key displayedSrc}
          <video
            src={displayedSrc}
            class="avatar-image"
            autoplay
            loop
            muted
            playsinline
          ></video>
        {/key}
      {:else}
        <img src={displayedSrc} alt="Avatar" class="avatar-image" />
      {/if}
      <button
        class="settings-button"
        onclick={openSettings}
        aria-label="Settings"
      >
        ⚙
      </button>
    </div>
  {/if}
  <button
    class="toggle-button"
    onclick={toggleVisibility}
    aria-label={$avatarVisible ? 'Hide avatar' : 'Show avatar'}
  >
    {$avatarVisible ? '›' : '‹'}
  </button>
</div>

<style>
  .avatar-container {
    position: absolute;
    display: flex;
    align-items: stretch;
    /* Container itself doesn't capture clicks — only its interactive
       children. Empty space (e.g. between the avatar and the toggle button
       in a different layout) passes clicks through to the terminal. */
    pointer-events: none;
    z-index: 10;
  }
  .avatar-container.top-right {
    top: var(--avatar-margin);
    right: var(--avatar-margin);
    flex-direction: row;
  }
  .avatar-container.top-left {
    top: var(--avatar-margin);
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
    width: 100%;
    height: 100%;
    object-fit: contain;
    /* No background — source-image alpha composes against the terminal. */
    user-select: none;
    -webkit-user-drag: none;
  }

  .settings-button {
    position: absolute;
    top: 8px;
    right: 8px;
    background: rgba(0, 0, 0, 0.5);
    border: none;
    color: #fff;
    width: 32px;
    height: 32px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 18px;
    line-height: 1;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .settings-button:hover {
    background: rgba(0, 0, 0, 0.7);
  }

  .toggle-button {
    width: 16px;
    height: var(--avatar-height);
    background: rgba(0, 0, 0, 0.4);
    border: none;
    color: #fff;
    cursor: pointer;
    pointer-events: auto;
    font-size: 14px;
    /* Toggle sits OUTSIDE .avatar-overlay so M5's waveform sibling can have
       independent opacity. We still apply --avatar-opacity here so the
       toggle visually matches the avatar. One-line change to decouple. */
    opacity: var(--avatar-opacity);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
  }
  .toggle-button:hover {
    background: rgba(0, 0, 0, 0.65);
  }
</style>
