<script lang="ts">
  // V32 Phase F (locked decision 15): the per-tab taint-latch override
  // popover, opened from the tab-chrome badge.
  //
  // Three things, in the order the user needs them:
  //   1. What is in force right now (the latch, and whether the conversation
  //      is contaminated). A security control that does not say what it is
  //      doing gets clicked past.
  //   2. "Switch to local" — the workflow button. Flips an EXTERNAL latch to
  //      Local: the proxied local-capability tools come back and the external
  //      side closes in the same move, so the session never holds web and
  //      private-data access at once.
  //   3. "Restore full access" — at-own-risk, behind an explicit second click
  //      that spells out WHY it is risky: the injected content is still in the
  //      conversation, so re-opening both sides recreates the trifecta with a
  //      model that may already be steered.
  //
  // Step 4 adds the contamination section, and it is here — in the popover the
  // badge opens — rather than only in the Workbench Timeline, because the
  // Timeline is gated on `settings.workbench.checkpoints` and a containment
  // control must not be unreachable in a configuration the user is allowed to
  // choose. Step 5's Timeline entry point calls the same action.
  //
  // The two clears differ in what they promise, and the copy has to carry that:
  //   • "Not injected — clear the flag" clears NOW, behind a second click,
  //     because if the judgement is wrong a steered model gets its persistence
  //     channel back.
  //   • "I restored a checkpoint" clears NOTHING now. Restoring rolls back files
  //     and cannot remove injected text from the conversation, so the flag is
  //     kept and lifts when cImp sees the tab start a new session. That is why
  //     this one is a single click: it releases nothing.
  //
  // The old static line said restarting cImp was the only clean reset. It was
  // true under H-2 and is not any more, so it is gone — a security surface that
  // tells the user the wrong escape hatch is worse than one that says nothing.
  //
  // Positioning / dismissal mirror TabContextMenu: fixed at the click coords,
  // clamped into the viewport, dismissed by Escape or a mousedown outside.
  //
  // V32 Phase G adds a fourth thing, above the actions: which injection
  // controls are switched OFF for this tab, and which of the three levels
  // decided each. That is the "why is this tab not latching?" question locked
  // decision 16 requires answerable without reading code — and the badge is
  // where a user looks first, long before Settings.
  import { onMount } from 'svelte';
  import {
    applyLatchOverride,
    featureStateWord,
    type FeatureState,
    type LatchAction,
    type LatchRow,
  } from './latch';

  let {
    x,
    y,
    row,
    reduced = [],
    onDismiss,
    onApplied,
  }: {
    x: number;
    y: number;
    /// The tab's current latch row — the popover renders entirely from it,
    /// including which actions are legal (the backend owns that rule and
    /// publishes it as `can_flip_local` / `can_unlatch`; re-deriving it here
    /// from the label would put the state machine in two places).
    row: LatchRow;
    /// V32 Phase G: this tab's injection controls that resolve OFF, each
    /// carrying the level that decided it. Empty on a fully protected tab, in
    /// which case the section renders nothing at all.
    reduced?: FeatureState[];
    onDismiss: () => void;
    /// Fired after a successful override so the caller can refresh its
    /// snapshot without waiting for the next poll tick.
    onApplied?: () => void;
  } = $props();

  let menuEl: HTMLDivElement | undefined = $state();
  let confirmingUnlatch = $state(false);
  /// Step 4: the same two-click shape as `confirmingUnlatch`, for the same
  /// reason — this one releases containment on the user's judgement alone.
  let confirmingClear = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);

  // svelte-ignore state_referenced_locally
  let posX = $state(x);
  // svelte-ignore state_referenced_locally
  let posY = $state(y);
  $effect(() => {
    const wantX = x;
    const wantY = y;
    if (!menuEl) {
      posX = wantX;
      posY = wantY;
      return;
    }
    const rect = menuEl.getBoundingClientRect();
    const margin = 4;
    posX = Math.max(margin, Math.min(wantX, window.innerWidth - rect.width - margin));
    posY = Math.max(margin, Math.min(wantY, window.innerHeight - rect.height - margin));
  });

  const latchLine = $derived(
    row.latch === 'external'
      ? 'Web / external content in use — local file and source-text tools are closed.'
      : row.latch === 'local'
        ? 'Local file / source-text tools in use — web and other external tools are closed.'
        : 'Not latched.',
  );

  /// Which level switched a control off, in the user's words. This is the
  /// whole point of the backend publishing `decided_by`: "off" alone sends the
  /// user hunting through three levels of Settings for the one that did it.
  function whyOff(f: FeatureState): string {
    // A row that carries its own reason is not a setting anyone flipped — see
    // `latch.ts`'s `withSignatureHealth` (#48, D-2). The three `decided_by`
    // levels answer "who decided this switch", which is the wrong question for
    // a fact about data on disk, and doubly wrong for a fact cImp could not
    // read at all (#48, H-10 — that row's word is "unknown", not "off").
    if (f.reason) return f.reason;
    switch (f.decided_by) {
      case 'global':
        return 'the global master switch is off';
      case 'scope':
        return 'this tab overrides it off';
      default:
        return 'switched off app-wide';
    }
  }

  async function run(action: LatchAction): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await applyLatchOverride(row.tab, row.consumer, action);
      onApplied?.();
      onDismiss();
    } catch (e) {
      // Show the backend's own message. A control that appears to do nothing
      // when clicked is worse than one that explains why it declined.
      error = typeof e === 'string' ? e : ((e as { message?: string })?.message ?? String(e));
      busy = false;
    }
  }

  function onWindowMouseDown(e: MouseEvent): void {
    const target = e.target as Node | null;
    if (target && menuEl && menuEl.contains(target)) return;
    onDismiss();
  }

  function onWindowKeyDown(e: KeyboardEvent): void {
    if (e.key === 'Escape') {
      e.preventDefault();
      onDismiss();
    }
  }

  onMount(() => {
    // Defer by a tick so the click that opened the popover doesn't close it.
    const id = setTimeout(() => {
      window.addEventListener('mousedown', onWindowMouseDown);
    }, 0);
    window.addEventListener('keydown', onWindowKeyDown);
    return () => {
      clearTimeout(id);
      window.removeEventListener('mousedown', onWindowMouseDown);
      window.removeEventListener('keydown', onWindowKeyDown);
    };
  });
</script>

<div
  bind:this={menuEl}
  class="menu"
  style="left: {posX}px; top: {posY}px;"
  role="dialog"
  aria-label="Containment state for this tab"
>
  <div class="head">Containment — {row.consumer} · {row.tab}</div>
  <div class="state">{latchLine}</div>
  {#if row.contaminated}
    <div class="state warn">
      This conversation has read external content. Memory writes stay
      quarantined and external results stay wrapped — whatever the latch says.
    </div>
    {#if row.awaiting_session_clear}
      <!-- Step 4: the arm is set. Say what will lift it, and what will not —
           a user who restored and then waits without knowing that a NEW session
           is the trigger has no way to guess it. -->
      <div class="state">
        A checkpoint was restored. The flag stays until this tab starts a new
        session — run <code>/clear</code> here, or restart the tab, and it lifts
        on its own. Restoring files cannot remove injected text from the
        conversation, which is why it was kept.
      </div>
    {/if}
  {/if}

  {#if reduced.length > 0}
    <div class="separator"></div>
    <div class="state warn">Injection protection is reduced for this tab:</div>
    <ul class="reduced">
      {#each reduced as f (f.feature)}
        <!-- "off" for a switch, "unknown" for a state cImp could not read
             (#48, H-10). Rendering the second as the first is a smaller lie
             than rendering it as protected, but it is still a lie: it points
             the user at a switch to flip. -->
        <li class:unknown={f.unknown}>
          {f.label} — {featureStateWord(f)}
          <span class="why">({whyOff(f)})</span>
        </li>
      {/each}
    </ul>
    <div class="state">Change it in Settings → Tools → Injection protection.</div>
  {/if}

  <div class="separator"></div>

  <button
    type="button"
    class="entry"
    disabled={!row.can_flip_local || busy}
    onclick={() => void run('flip_local')}
  >
    Switch to local — closes web access
  </button>

  {#if confirmingUnlatch}
    <div class="state warn">
      Restoring full access re-opens the web side while the injected content is
      still in this conversation — the model can be steered by it and reach your
      files at the same time. Continue?
    </div>
    <div class="row">
      <button
        type="button"
        class="entry danger"
        disabled={busy}
        onclick={() => void run('unlatch')}
      >
        Yes, restore full access
      </button>
      <button type="button" class="entry" onclick={() => (confirmingUnlatch = false)}>
        Cancel
      </button>
    </div>
  {:else}
    <button
      type="button"
      class="entry"
      disabled={!row.can_unlatch || busy}
      onclick={() => (confirmingUnlatch = true)}
    >
      Restore full access (at your own risk)…
    </button>
  {/if}

  {#if row.can_clear}
    <div class="separator"></div>
    <div class="head">Contamination flag</div>

    {#if confirmingClear}
      <div class="state warn">
        Clearing says the flagged content was harmless. If it was not, this tab's
        memory writes stop being held for review while a model that read it is
        still running. The conversation is not changed — nothing is restarted and
        nothing is rolled back. Continue?
      </div>
      <div class="row">
        <button
          type="button"
          class="entry danger"
          disabled={busy}
          onclick={() => void run('clear_contamination')}
        >
          Yes, clear the flag
        </button>
        <button type="button" class="entry" onclick={() => (confirmingClear = false)}>
          Cancel
        </button>
      </div>
    {:else}
      <button
        type="button"
        class="entry"
        disabled={busy}
        onclick={() => (confirmingClear = true)}
      >
        Not injected — clear the flag now…
      </button>
    {/if}

    <!-- Single click, deliberately: this releases nothing. It records that a
         restore happened and defers the clear to an observed new session. -->
    <button
      type="button"
      class="entry"
      disabled={row.awaiting_session_clear || busy}
      onclick={() => void run('await_session_clear')}
    >
      I restored a checkpoint — clear after I start a new session
    </button>
  {/if}

  {#if error}
    <div class="state err">{error}</div>
  {/if}
</div>

<style>
  .menu {
    position: fixed;
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
    padding: var(--space-1);
    min-width: 260px;
    max-width: 340px;
    z-index: 200;
  }
  .head {
    padding: 4px var(--space-3);
    font-size: var(--font-size-sm);
    color: var(--text-tertiary);
  }
  .state {
    padding: 2px var(--space-3) 6px;
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
    line-height: 1.4;
  }
  .warn {
    color: var(--awaiting);
  }
  .reduced {
    margin: 0;
    padding: 0 var(--space-3) 6px calc(var(--space-3) + 14px);
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
    line-height: 1.4;
  }
  .why {
    color: var(--text-tertiary);
  }
  .state code {
    font-family: var(--font-mono, monospace);
    color: var(--text-primary);
  }
  /* A row whose state could not be read is not a confident claim — the same
     dashed treatment the status chip uses for its unknown state. */
  .reduced li.unknown {
    border-bottom: 1px dashed var(--border-default);
  }
  .err {
    color: var(--text-danger-soft);
  }
  .row {
    display: flex;
    gap: var(--space-1);
  }
  .entry {
    appearance: none;
    border: none;
    background: transparent;
    color: var(--text-primary);
    text-align: left;
    width: 100%;
    padding: 6px var(--space-3);
    font-size: var(--font-size-md);
    font-family: inherit;
    cursor: pointer;
    border-radius: var(--radius-sm);
    transition: background var(--motion-fast) var(--easing-standard);
  }
  .entry:hover:not([disabled]) {
    background: var(--surface-4);
  }
  .entry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .entry.danger:hover:not([disabled]) {
    background: var(--surface-danger-soft);
    color: var(--text-danger-soft);
  }
  .entry[disabled] {
    color: var(--text-disabled);
    cursor: default;
  }
  .separator {
    height: 1px;
    background: var(--border-default);
    margin: var(--space-1) 0;
  }
</style>
