<script lang="ts">
  // Single tab button. v2-03 adds the status indicator: a small dot rendered
  // before the label whose color/animation reflects the highest-priority
  // state flag on the tab. Priority: Error > AwaitingPermission >
  // DoneWhileAway (only if inactive) > Working (Thinking/Speaking) > none.
  import type { AvatarState } from './avatarState';

  let {
    label,
    active = false,
    avatarState = 'Idle' as AvatarState,
    awaitingPermission = false,
    doneWhileAway = false,
    onclick,
  }: {
    label: string;
    active?: boolean;
    avatarState?: AvatarState;
    awaitingPermission?: boolean;
    doneWhileAway?: boolean;
    onclick: () => void;
  } = $props();

  type Indicator = 'error' | 'awaiting' | 'done' | 'working' | null;

  function pickIndicator(
    avatarState: AvatarState,
    awaitingPermission: boolean,
    doneWhileAway: boolean,
    active: boolean,
  ): Indicator {
    if (avatarState === 'Error') return 'error';
    if (awaitingPermission) return 'awaiting';
    if (doneWhileAway && !active) return 'done';
    if (avatarState === 'Thinking' || avatarState === 'Speaking') return 'working';
    return null;
  }

  let indicator = $derived(
    pickIndicator(avatarState, awaitingPermission, doneWhileAway, active),
  );
</script>

<button
  type="button"
  class="tab"
  class:active
  {onclick}
  aria-pressed={active}
>
  {#if indicator}
    <span
      class="indicator indicator-{indicator}"
      aria-label={`status: ${indicator}`}
    ></span>
  {/if}
  <span class="label">{label}</span>
</button>

<style>
  .tab {
    appearance: none;
    border: none;
    background: transparent;
    color: #c0c0c0;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 13px;
    padding: 0 16px;
    height: 100%;
    cursor: pointer;
    border-right: 1px solid #2a2a2a;
    border-bottom: 2px solid transparent;
    line-height: 30px;
    user-select: none;
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }
  .tab:hover {
    background: #303030;
    color: #e0e0e0;
  }
  .tab.active {
    color: #ffffff;
    background: #1f1f1f;
    border-bottom-color: #4a90e2;
  }
  .indicator {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: 0 0 auto;
  }
  .indicator-working {
    background: currentColor;
    opacity: 0.7;
    animation: pulse-subtle 1.6s ease-in-out infinite;
  }
  .indicator-awaiting {
    background: #f0a020;
    animation: pulse-strong 1s ease-in-out infinite;
  }
  .indicator-done {
    background: #4caf50;
  }
  .indicator-error {
    background: #e74c3c;
  }
  @keyframes pulse-subtle {
    0%, 100% { opacity: 0.4; }
    50%      { opacity: 0.9; }
  }
  @keyframes pulse-strong {
    0%, 100% { opacity: 0.6; transform: scale(1); }
    50%      { opacity: 1;   transform: scale(1.2); }
  }
</style>
