<script lang="ts">
  // Single tab button. Renders the indicator (status dot), the tab label,
  // and — for non-builtin tabs — a close button (×) on hover or when
  // active. Clicking × on a still-running shell shows an inline confirm
  // (Close? Yes / No); clicking × on an already-closed shell skips the
  // confirm and calls close_tab immediately.
  import type { AvatarState } from './avatarState';
  import type { FeatureState, LatchRow } from './latch';

  let {
    tabId,
    label,
    active = false,
    builtin = false,
    canSkipCloseConfirm = false,
    renaming = $bindable(false),
    showIndicator = true,
    avatarState = 'Idle' as AvatarState,
    awaitingPermission = false,
    doneWhileAway = false,
    taint = null,
    reduced = [],
    onclick,
    onclose,
    onnew,
    oncontextmenu,
    onpointerdowndrag,
    onrename,
    ontaint,
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
    /// Whether to reserve the status-dot slot. Only AI-tool tabs ever light
    /// an indicator (permission waits, thinking, done-while-away); every
    /// other tab omits the slot entirely so its label sits flush left.
    showIndicator?: boolean;
    avatarState?: AvatarState;
    awaitingPermission?: boolean;
    doneWhileAway?: boolean;
    /// V32 Phase F: this tab's taint-latch row, or null when the tab is
    /// clean (or has never made a gated call). Non-null renders the badge.
    /// Kept as the whole row rather than a boolean so the badge's tooltip
    /// can state WHICH boundary is in force without a second lookup.
    taint?: LatchRow | null;
    /// V32 Phase G: the injection controls resolved OFF for this tab. Renders
    /// the badge even on a clean tab, because "this tab's containment is
    /// switched off" is exactly as much a containment state as "this tab is
    /// latched" — and locked decision 16 requires a reduced-protection state to
    /// be visible outside Settings.
    reduced?: FeatureState[];
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
    /// V32 Phase F: the taint badge was activated. Receives the event so the
    /// caller can anchor the override popover at the badge, the same way
    /// `oncontextmenu` anchors the tab menu.
    ontaint?: (e: MouseEvent) => void;
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
    // Keep every key local to the input. It is nested inside the tab
    // <button>, so a bubbled Space (or Enter) keyboard-activates the button —
    // simulated click → tab switch steals focus → blur-submit closes the
    // editor mid-typing. Capture-phase global shortcuts have already run by
    // the time this fires, so they are unaffected.
    e.stopPropagation();
    if (e.key === 'Enter') {
      e.preventDefault();
      submitRename();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      cancelRename();
    }
  }

  function onRenameKeyUp(e: KeyboardEvent): void {
    // Space activation of a <button> fires on keyup — stop that too.
    e.stopPropagation();
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

  /// V32 Phase F: the badge's tooltip. States the boundary in force, because
  /// a shield glyph alone says "something happened" and the user's next
  /// question is always "so what can this tab still do?".
  ///
  /// V32 Phase G appends the reduced-protection line. The two facts sit
  /// together because they answer the same question from opposite directions:
  /// the latch says what this tab may no longer do, the reduced line says which
  /// protections are not watching it in the first place.
  let taintTitle = $derived(
    [
      !taint
        ? ''
        : taint.latch === 'external'
          ? 'This session has used web / external content: local file and source-text tools are closed for it.'
          : taint.latch === 'local'
            ? 'This session has used local file / source-text tools: web and other external tools are closed for it.'
            : 'This session has read external content. Memory writes stay quarantined for this tab until cImp is restarted.',
      reduced.length === 0
        ? ''
        : `Injection protection reduced for this tab: ${reduced.map((f) => f.label).join(', ')} off.`,
    ]
      .filter(Boolean)
      .join(' '),
  );

  /// Whether the badge renders at all. Either containment state counts.
  let showTaintBadge = $derived(!!taint || reduced.length > 0);

  function onTaintClick(e: MouseEvent): void {
    // Don't let the click also activate or drag the tab.
    e.stopPropagation();
    ontaint?.(e);
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
    {#if showIndicator}
      <span
        class="indicator indicator-{indicator ?? 'none'}"
        aria-label={indicator ? `status: ${indicator}` : undefined}
        aria-hidden={indicator ? undefined : true}
      ></span>
    {/if}
    <input
      bind:this={renameInputEl}
      bind:value={renameValue}
      class="rename-input"
      type="text"
      onkeydown={onRenameKeyDown}
      onkeyup={onRenameKeyUp}
      onblur={submitRename}
      onclick={(e) => e.stopPropagation()}
      onpointerdown={(e) => e.stopPropagation()}
    />
  {:else}
    {#if showIndicator}
      <span
        class="indicator indicator-{indicator ?? 'none'}"
        aria-label={indicator ? `status: ${indicator}` : undefined}
        aria-hidden={indicator ? undefined : true}
      ></span>
    {/if}
    <span class="label" class:label-left={!showIndicator}>{label}</span>
    {#if showTaintBadge}
      <!--
        V32 Phase F taint badge. Always visible while the tab is latched or
        contaminated (unlike × / +, which reveal on hover): it reports a
        containment state the user needs to see without going looking, and
        clicking it opens the override popover.

        V32 Phase G: it also appears when one of this tab's injection controls
        is switched off, in its own colour — a tab whose protections are off is
        a state the user must be able to notice without opening Settings, and
        it is not the same state as a latched tab.
      -->
      <span
        class="taint"
        class:taint-contaminated={taint?.contaminated}
        class:taint-reduced={!taint && reduced.length > 0}
        role="button"
        tabindex="0"
        aria-label="Containment state for this tab"
        title={taintTitle}
        onclick={onTaintClick}
        onpointerdown={(e) => e.stopPropagation()}
        onkeydown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            onTaintClick(e as unknown as MouseEvent);
          }
        }}
      >
        ⛨
      </span>
    {/if}
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
  /* Without an indicator slot the label owns the whole tab width; the
     button's UA text-align: center would float short titles mid-tab. */
  .label-left {
    text-align: left;
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
  /* The dot's slot is ALWAYS rendered (indicator-none = transparent) so a
     tab's width never changes as activity flips on/off — with several tabs
     rapidly switching states, conditional rendering made the whole bar
     jitter. */
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
  /* V32 Phase F taint badge. Warning-coloured and always opaque — this is a
     security state, not an affordance that should hide until hovered. */
  .taint {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    border-radius: var(--radius-sm);
    color: var(--awaiting);
    font-size: 13px;
    line-height: 1;
    margin-left: var(--space-1);
    cursor: pointer;
    user-select: none;
  }
  /* Contamination outlives the latch, so it gets the stronger colour: the
     latch may read `open` again after an override while the conversation is
     still carrying whatever it read. */
  .taint-contaminated {
    color: var(--danger);
  }
  /* V32 Phase G: a CLEAN tab whose protections are switched off. Muted rather
     than warning-coloured — nothing has happened to this session; a control the
     user turned off is a configuration fact, not an event. */
  .taint-reduced {
    color: var(--text-tertiary);
  }
  .taint:hover {
    background: var(--surface-3);
  }
  .taint:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
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
