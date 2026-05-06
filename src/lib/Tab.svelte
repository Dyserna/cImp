<script lang="ts">
  // Single tab button. Renders the indicator (status dot), the tab label,
  // and — for non-builtin tabs — a close button (×) on hover or when
  // active. Clicking × on a still-running shell shows an inline confirm
  // (Close? Yes / No); clicking × on an already-closed shell skips the
  // confirm and calls close_tab immediately.
  import type { AvatarState } from './avatarState';

  let {
    label,
    active = false,
    builtin = false,
    canSkipCloseConfirm = false,
    renaming = $bindable(false),
    avatarState = 'Idle' as AvatarState,
    awaitingPermission = false,
    doneWhileAway = false,
    onclick,
    onclose,
    oncontextmenu,
    onrename,
  }: {
    label: string;
    active?: boolean;
    builtin?: boolean;
    /// When true (the tab's shell has already exited), the close button
    /// skips the confirm step — the user has nothing to lose by closing
    /// a defunct PTY.
    canSkipCloseConfirm?: boolean;
    /// Two-way bound: parent toggles to true to enter rename mode (e.g.
    /// from the context menu); the input here flips it back on submit
    /// or cancel.
    renaming?: boolean;
    avatarState?: AvatarState;
    awaitingPermission?: boolean;
    doneWhileAway?: boolean;
    onclick: () => void;
    /// Invoked when the user confirms the close (or skips the confirm).
    /// Optional because builtin tabs render no close button.
    onclose?: () => void;
    /// Invoked on right-click. Receives the click event so the caller
    /// can position a menu at the cursor; preventDefault is the caller's
    /// responsibility.
    oncontextmenu?: (e: MouseEvent) => void;
    /// Invoked when the user submits a rename. Empty / whitespace-only
    /// strings are filtered upstream — by the time this fires the parent
    /// can call rename_tab directly.
    onrename?: (newName: string) => void;
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

  let confirming = $state(false);
  let renameValue = $state('');
  let renameInputEl: HTMLInputElement | undefined = $state();

  // Auto-focus + select the rename input the moment we enter rename mode.
  $effect(() => {
    if (renaming) {
      renameValue = label;
      // Defer focus until the input is in the DOM.
      queueMicrotask(() => {
        renameInputEl?.focus();
        renameInputEl?.select();
      });
    }
  });

  function submitRename(): void {
    const trimmed = renameValue.trim();
    renaming = false;
    if (trimmed === '' || trimmed === label) return;
    onrename?.(trimmed);
  }

  function cancelRename(): void {
    renaming = false;
  }

  function onRenameKeyDown(e: KeyboardEvent): void {
    if (e.key === 'Enter') {
      e.preventDefault();
      e.stopPropagation();
      submitRename();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      cancelRename();
    }
  }

  function onCloseClick(e: MouseEvent): void {
    e.stopPropagation();
    if (!onclose) return;
    if (canSkipCloseConfirm) {
      onclose();
      return;
    }
    confirming = true;
  }

  function onConfirmYes(e: MouseEvent): void {
    e.stopPropagation();
    confirming = false;
    onclose?.();
  }

  function onConfirmNo(e: MouseEvent): void {
    e.stopPropagation();
    confirming = false;
  }

  function onContextMenuInternal(e: MouseEvent): void {
    if (!oncontextmenu) return;
    e.preventDefault();
    oncontextmenu(e);
  }
</script>

<button
  type="button"
  class="tab"
  class:active
  {onclick}
  oncontextmenu={onContextMenuInternal}
  aria-pressed={active}
>
  {#if confirming}
    <span class="confirm-label">Close?</span>
    <span class="confirm-actions">
      <span
        class="confirm-btn confirm-yes"
        role="button"
        tabindex="0"
        onclick={onConfirmYes}
        onkeydown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            onConfirmYes(e as unknown as MouseEvent);
          }
        }}
      >
        Yes
      </span>
      <span
        class="confirm-btn confirm-no"
        role="button"
        tabindex="0"
        onclick={onConfirmNo}
        onkeydown={(e) => {
          if (e.key === 'Enter' || e.key === ' ' || e.key === 'Escape') {
            e.preventDefault();
            onConfirmNo(e as unknown as MouseEvent);
          }
        }}
      >
        No
      </span>
    </span>
  {:else if renaming}
    {#if indicator}
      <span
        class="indicator indicator-{indicator}"
        aria-label={`status: ${indicator}`}
      ></span>
    {/if}
    <input
      bind:this={renameInputEl}
      bind:value={renameValue}
      class="rename-input"
      type="text"
      onkeydown={onRenameKeyDown}
      onblur={submitRename}
      onclick={(e) => e.stopPropagation()}
    />
  {:else}
    {#if indicator}
      <span
        class="indicator indicator-{indicator}"
        aria-label={`status: ${indicator}`}
      ></span>
    {/if}
    <span class="label">{label}</span>
    {#if !builtin && onclose}
      <span
        class="close"
        role="button"
        tabindex="0"
        aria-label="Close tab"
        title="Close tab"
        onclick={onCloseClick}
        onkeydown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            onCloseClick(e as unknown as MouseEvent);
          }
        }}
      >
        ×
      </span>
    {/if}
  {/if}
</button>

<style>
  .tab {
    appearance: none;
    border: none;
    background: transparent;
    color: #c0c0c0;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 13px;
    padding: 0 16px 0 12px;
    height: 100%;
    cursor: pointer;
    border-right: 1px solid #2a2a2a;
    border-bottom: 2px solid transparent;
    line-height: 30px;
    user-select: none;
    display: inline-flex;
    align-items: center;
    gap: 8px;
    position: relative;
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
  .close {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    border-radius: 3px;
    color: #888;
    font-size: 14px;
    line-height: 1;
    margin-left: 4px;
    opacity: 0;
    transition: opacity 0.1s ease;
    cursor: pointer;
  }
  .tab:hover .close,
  .tab.active .close {
    opacity: 1;
  }
  .close:hover {
    background: #4a3a3a;
    color: #ffaaaa;
  }
  .confirm-label {
    color: #e0e0e0;
    font-size: 12px;
  }
  .confirm-actions {
    display: inline-flex;
    gap: 4px;
    align-items: center;
  }
  .confirm-btn {
    font-size: 11px;
    padding: 2px 8px;
    border-radius: 3px;
    cursor: pointer;
    user-select: none;
  }
  .confirm-yes {
    background: #5a2a2a;
    color: #ffaaaa;
  }
  .confirm-yes:hover {
    background: #7a3030;
    color: #ffd0d0;
  }
  .confirm-no {
    background: #2a2a2a;
    color: #c0c0c0;
  }
  .confirm-no:hover {
    background: #3a3a3a;
    color: #e0e0e0;
  }
  .rename-input {
    background: #1f1f1f;
    border: 1px solid #4a90e2;
    color: #ffffff;
    padding: 2px 6px;
    border-radius: 3px;
    font-size: 13px;
    font-family: inherit;
    min-width: 80px;
    max-width: 200px;
    outline: none;
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
