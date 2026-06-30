<script lang="ts">
  // Single tab button. Renders the indicator (status dot), the tab label,
  // and — for non-builtin tabs — a close button (×) on hover or when
  // active. Clicking × on a still-running shell shows an inline confirm
  // (Close? Yes / No); clicking × on an already-closed shell skips the
  // confirm and calls close_tab immediately.
  import type { AvatarState } from './avatarState';

  let {
    tabId,
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
    onnew,
    oncontextmenu,
    onpointerdowndrag,
    onrename,
  }: {
    /// Stable tab id, surfaced as `data-tab-id` so the M2 drop-target
    /// hit-tester can resolve reorder insertion indices by walking
    /// the tab bar's children.
    tabId: string;
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
    /// Invoked when the user clicks the `+` (spawn duplicate). Only set
    /// for AI builtins, so the button renders exactly where `×` would on
    /// closable tabs. The two are mutually exclusive in practice.
    onnew?: () => void;
    /// Invoked on right-click. Receives the click event so the caller
    /// can position a menu at the cursor; preventDefault is the caller's
    /// responsibility.
    oncontextmenu?: (e: MouseEvent) => void;
    /// Invoked on pointerdown so the M2 drag layer can begin a
    /// pending drag. The handler decides itself whether to promote
    /// the pending state to a real drag (4px threshold).
    onpointerdowndrag?: (e: PointerEvent) => void;
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

  function onNewClick(e: MouseEvent): void {
    // Don't let the click also activate/drag the tab.
    e.stopPropagation();
    onnew?.();
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
    // Stop propagation so the tab-bar background's contextmenu handler
    // doesn't also fire and open a second (overlapping) pane menu.
    // Tab right-click owns the event; the merged tab+pane menu lives
    // in TabContextMenu when tab info is supplied.
    e.stopPropagation();
    oncontextmenu(e);
  }
</script>

<button
  type="button"
  class="tab"
  class:active
  data-tab-id={tabId}
  {onclick}
  onpointerdown={onpointerdowndrag}
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
    {#if onnew}
      <span
        class="spawn"
        role="button"
        tabindex="0"
        aria-label="New tab of this type"
        title="New tab of this type"
        onclick={onNewClick}
        onkeydown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            onNewClick(e as unknown as MouseEvent);
          }
        }}
      >
        +
      </span>
    {/if}
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
    color: var(--text-secondary);
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: var(--font-size-md);
    padding: 0 var(--space-3);
    height: calc(100% - 8px);
    margin: 4px 2px;
    cursor: grab;
    border: none;
    border-radius: var(--radius-md);
    line-height: 24px;
    user-select: none;
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    position: relative;
    /* Don't shrink below the content / min width — overflow on the
       parent .tab-list scrolls instead. The min-width keeps short
       labels readable when the bar is wide; the max-width clips long
       ones with ellipsis on .label so a single huge tab can't
       monopolize the bar. */
    flex: 0 0 auto;
    min-width: 80px;
    max-width: 200px;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard);
  }
  .tab:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .label {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
    flex: 1 1 auto;
    min-width: 0;
  }
  .tab:active {
    cursor: grabbing;
  }
  .tab:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .tab.active {
    /* Two-tier active-state pattern: section selection uses surface
       elevation + bright text. No border accent; the elevated pill
       IS the indicator. */
    color: var(--text-bright);
    background: var(--surface-3);
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
    background: var(--awaiting);
    animation: pulse-strong 1s ease-in-out infinite;
  }
  .indicator-done {
    background: var(--success);
  }
  .indicator-error {
    background: var(--danger);
  }
  .close {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    border-radius: var(--radius-sm);
    color: var(--text-tertiary);
    font-size: 14px;
    line-height: 1;
    margin-left: var(--space-1);
    opacity: 0;
    transition: opacity var(--motion-fast) var(--easing-standard);
    cursor: pointer;
  }
  .tab:hover .close,
  .tab.active .close {
    opacity: 1;
  }
  .close:hover {
    background: var(--surface-danger);
    color: var(--text-danger-soft);
  }
  /* Spawn-duplicate (+) button. Mirrors .close's hover/active reveal so
     the AI builtins get the same affordance footprint a closable tab
     gets for ×; hover tint uses the accent rather than the danger color. */
  .spawn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    border-radius: var(--radius-sm);
    color: var(--text-tertiary);
    font-size: 16px;
    line-height: 1;
    margin-left: var(--space-1);
    opacity: 0;
    transition: opacity var(--motion-fast) var(--easing-standard);
    cursor: pointer;
  }
  .tab:hover .spawn,
  .tab.active .spawn {
    opacity: 1;
  }
  .spawn:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .confirm-label {
    color: var(--text-primary);
    font-size: var(--font-size-sm);
  }
  .confirm-actions {
    display: inline-flex;
    gap: var(--space-1);
    align-items: center;
  }
  .confirm-btn {
    font-size: var(--font-size-xs);
    padding: 2px var(--space-2);
    border-radius: var(--radius-sm);
    cursor: pointer;
    user-select: none;
  }
  .confirm-yes {
    background: var(--surface-danger-strong);
    color: var(--text-danger-soft);
  }
  .confirm-yes:hover {
    background: var(--surface-danger-hover);
    color: var(--text-danger-strong);
  }
  .confirm-no {
    background: var(--surface-2);
    color: var(--text-secondary);
  }
  .confirm-no:hover {
    background: var(--surface-4);
    color: var(--text-primary);
  }
  .rename-input {
    background: var(--surface-1);
    border: 1px solid var(--accent);
    color: var(--text-bright);
    padding: 2px 6px;
    border-radius: var(--radius-sm);
    font-size: var(--font-size-md);
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
