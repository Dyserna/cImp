<script lang="ts">
  // Avatar visibility toggle for the bottom status bar. Replaces the
  // side-chevron that used to hang off the avatar — moved here so the
  // control is reachable when no avatar is rendered (e.g. user hid it,
  // wants it back). aria-pressed flips on `hidden` to match the
  // AnnouncementsButton / MuteButton convention: the engaged "I've
  // suppressed this" state gets the accent treatment.
  import { avatarVisible, toggleAvatarVisible } from '../avatarState';
</script>

<button
  type="button"
  class="status-button"
  onclick={toggleAvatarVisible}
  title={$avatarVisible ? 'Hide avatar' : 'Show avatar'}
  aria-label={$avatarVisible ? 'Hide avatar' : 'Show avatar'}
  aria-pressed={!$avatarVisible}
>
  <span class="glyph" class:hidden={!$avatarVisible} aria-hidden="true">
    👤
  </span>
</button>

<style>
  .status-button {
    appearance: none;
    background: transparent;
    border: 1px solid transparent;
    color: var(--text-secondary);
    cursor: pointer;
    width: 26px;
    height: 22px;
    border-radius: var(--radius-pill);
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .status-button:hover:not([aria-pressed='true']) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button[aria-pressed='true'] {
    background: var(--accent-muted);
    border-color: var(--accent);
    color: var(--accent);
  }
  .status-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .glyph {
    font-size: 13px;
  }
  /* Strikethrough the person glyph when avatar is hidden — visually
     reinforces the suppressed state on top of the accent button bg. */
  .glyph.hidden {
    text-decoration: line-through;
    text-decoration-thickness: 1.5px;
  }
</style>
