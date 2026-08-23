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
  //      model that may already be steered. Since decision 15's 2026-08-10
  //      amendment that click ALSO clears the contamination flag, so the second
  //      click states both effects — and, on a tab that is not contaminated,
  //      states neither (a promise to clear a flag that is not set would be
  //      false, and the old single string made exactly that mistake about the
  //      injected content).
  //
  // Step 4 adds the contamination section, and it is here — in the popover the
  // badge opens — rather than only in the Workbench Timeline, because the
  // Timeline is gated on `settings.workbench.checkpoints` and a containment
  // control must not be unreachable in a configuration the user is allowed to
  // choose. Step 5's Timeline entry point calls the same action.
  //
  // The three clears differ in what they promise, and the copy has to carry
  // that:
  //   • "Not injected — clear the flag" clears NOW, behind a second click,
  //     because if the judgement is wrong a steered model gets its persistence
  //     channel back.
  //   • "I restored a checkpoint" clears NOTHING now. Restoring rolls back files
  //     and cannot remove injected text from the conversation, so the flag is
  //     kept and lifts when cImp sees the tab start a new session. That is why
  //     this one is a single click: it releases nothing.
  //   • "Restore full access" clears it too — decision 15's 2026-08-10
  //     amendment. A full unlatch is a VERDICT (the user takes the larger risk
  //     knowingly), where "switch to local" is a workflow step and therefore
  //     keeps the flag. It lives in the latch section above, not here, because
  //     the clear is a consequence of the latch move rather than its purpose.
  //
  // The old static line said restarting cImp was the only clean reset. It was
  // true under H-2 and is not any more, so it is gone — a security surface that
  // tells the user the wrong escape hatch is worse than one that says nothing.
  //
  // Positioning / dismissal mirror TabContextMenu: fixed at the click coords,
  // clamped into the viewport, dismissed by Escape or a mousedown outside.
  //
  // V32 Phase G added a fourth thing above the actions: which injection
  // controls were switched OFF for this tab, and which of the three levels
  // decided each. That was the "why is this tab not latching?" question locked
  // decision 16 requires answerable without reading code.
  //
  // **V39 turns that read-only list into the CONTROL.** The app-wide levels ship
  // fully on and a newly created tab ships every tab-scoped cell `off`, so the
  // per-tab row is where protection is actually engaged — and this popover, one
  // click from the tab's own shield, is where it is engaged from. Settings keeps
  // the full matrix (every scope at once, the app-wide levels, the numerics);
  // this is the per-tab half, reachable without leaving the window you are
  // working in.
  //
  // Three rules the list follows, each of them a cross-module invariant rather
  // than a styling choice:
  //
  //   * every row is the BACKEND's resolved row (`injection_status`). The
  //     effective word, the label, `spawn_baked` and `master_gated` all come
  //     from there; nothing here re-resolves the hierarchy, or the popover and
  //     the Settings matrix could disagree about the same tab.
  //   * a toggle writes `'on'` or `'off'` — never `'inherit'`. This surface has
  //     two states to offer and a tri-state control it cannot explain in a
  //     popover; leaving `inherit` writable here would let a click land on a
  //     value whose meaning depends on a level this window does not show.
  //   * one click ⇒ one `applySettings` with the whole Settings object, and
  //     "Enable all" / "Disable all" are ONE write each, not N. There is
  //     deliberately no `set_injection_override` IPC (`ipc/commands.rs`): a
  //     side-channel write would race the full-object save.
  import { onMount } from 'svelte';
  import {
    applyLatchOverride,
    applyTabInjectionOverrides,
    effectiveWord,
    featureStateWord,
    protectionSummary,
    setAllOverrides,
    type FeatureState,
    type LatchAction,
    type LatchRow,
  } from './latch';
  import { openSettingsWindowToSection, requestTabRestart } from './settings/ipc';
  import { latchAlsoHoldsMemory } from './timeline';
  import { findHarness, harnesses } from './harness';

  /// The in-session command that starts a fresh conversation in a tab of
  /// `harness`, or `null` when it declares none.
  ///
  /// V40 Phase F (locked decision 27): this copy used to spell one harness's
  /// slash-command as if every harness had it. A harness that declares none now
  /// gets the honest half of the sentence ("restart the tab") instead.
  function newSessionCommand(harness: string): string | null {
    return findHarness($harnesses, harness)?.affordances.newSessionCommand ?? null;
  }

  let {
    x,
    y,
    row,
    reduced = [],
    protection = [],
    masterOn = true,
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
    /// This tab's reduced-protection rows — since V39 that means rows off for a
    /// reason OTHER than this tab's own cell (an app-wide flip, the master, or
    /// the synthetic signature-health row, which is a fact about the rules
    /// directory rather than a switch). The toggle list below covers the cells.
    reduced?: FeatureState[];
    /// V39: this tab's tab-scoped SWITCHES, resolved by the backend, in
    /// `Feature::ALL` order (`tabProtectionRows`). One toggle each.
    protection?: FeatureState[];
    /// The L1 master's value. Master-gated toggles are disabled while it is off
    /// — writing a cell the master overrides would be a control that does
    /// nothing — and the one control the master does not reach stays live.
    masterOn?: boolean;
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

  /// V39: spawn-baked cells this popover has moved.
  ///
  /// A spawn-baked control is written into a tab at LAUNCH, so flipping one here
  /// does not reach the running tab — it needs a restart, and a surface that
  /// lets you flip it without saying so is a switch that appears to do nothing.
  ///
  /// Tracked per popover session rather than read from a backend signal, and
  /// deliberately: the backend's `ai-tab-restart-hint` is emitted per CONSUMER
  /// (a registry harness id), which cannot name which tab the user is standing
  /// on, and `App.svelte` already renders it as a toast for the app-wide case.
  /// What this set knows is narrower and exactly right for this button — cells
  /// *this tab* just had moved, by *this* popover.
  let restartOwed = $state<Set<string>>(new Set());

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

  /// Step 5d: null unless the latch is holding memory writes independently of
  /// the flag — see `latchAlsoHoldsMemory`.
  const memoryNote = $derived(latchAlsoHoldsMemory(row.latch));

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

  /// Whether a row's toggle is writable right now.
  ///
  /// The master short-circuits every control it is ABOUT (`master_gated`), so a
  /// cell written under it would resolve off anyway — the click would look like
  /// it failed. The one control it does not reach (V38's managed-tool steering,
  /// a token-efficiency nudge rather than a containment control) stays live,
  /// which is the whole reason the backend publishes `master_gated` per row.
  function rowEnabled(f: FeatureState): boolean {
    return !busy && (masterOn || !f.master_gated);
  }

  /// Write one cell. `'on'` / `'off'` explicitly — never `'inherit'`.
  async function setOne(f: FeatureState, on: boolean): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await applyTabInjectionOverrides(row.tab, { [f.feature]: on ? 'on' : 'off' });
      if (f.spawn_baked) restartOwed = new Set(restartOwed).add(f.feature);
      onApplied?.();
    } catch (e) {
      error = typeof e === 'string' ? e : ((e as { message?: string })?.message ?? String(e));
    }
    busy = false;
  }

  /// Every writable cell at once, as ONE settings write.
  ///
  /// Rows the master has closed are skipped rather than written: the write would
  /// be honest (the cell would say `on`) and the result would not (it resolves
  /// off), and a bulk action that leaves the user with controls reading `on`
  /// while nothing is enforced is the worst shape this surface can take.
  async function setAll(on: boolean): Promise<void> {
    if (busy) return;
    const rows = protection.filter(rowEnabled);
    if (rows.length === 0) return;
    busy = true;
    error = null;
    try {
      await applyTabInjectionOverrides(row.tab, setAllOverrides(rows, on ? 'on' : 'off'));
      const next = new Set(restartOwed);
      for (const f of rows) if (f.spawn_baked) next.add(f.feature);
      restartOwed = next;
      onApplied?.();
    } catch (e) {
      error = typeof e === 'string' ? e : ((e as { message?: string })?.message ?? String(e));
    }
    busy = false;
  }

  async function restart(): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await requestTabRestart(row.tab);
      restartOwed = new Set();
      onDismiss();
      return;
    } catch (e) {
      error = typeof e === 'string' ? e : ((e as { message?: string })?.message ?? String(e));
    }
    busy = false;
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
        session — {#if newSessionCommand(row.consumer)}run
          <code>{newSessionCommand(row.consumer)}</code> here, or restart the tab{:else}restart
          the tab{/if}, and it lifts on its own. Restoring files cannot remove
        injected text from the conversation, which is why it was kept.
      </div>
    {/if}
  {/if}

  {#if protection.length > 0}
    <div class="separator"></div>
    <div class="head">Injection protection — {protectionSummary(protection)}</div>
    {#if !masterOn}
      <!-- Decision 9: the master is off, so every control it reaches resolves
           off whatever these cells say. The toggles are disabled rather than
           hidden, because a hidden switch reads as "no such control" and the
           user's next move is to look for it. -->
      <div class="state warn">
        The global master switch is off, so these controls are inert for this
        tab. Turn it back on with the ⛨ chip in the status bar, or in
        <button type="button" class="link" onclick={() => void openSettingsWindowToSection('injection')}>
          Settings → Injection protection</button
        >.
      </div>
    {/if}
    <div class="row">
      <button type="button" class="entry" disabled={busy} onclick={() => void setAll(true)}>
        Enable all
      </button>
      <button type="button" class="entry" disabled={busy} onclick={() => void setAll(false)}>
        Disable all
      </button>
    </div>
    <ul class="toggles">
      {#each protection as f (f.feature)}
        <li>
          <label class:disabled={!rowEnabled(f)}>
            <input
              type="checkbox"
              checked={f.effective}
              disabled={!rowEnabled(f)}
              onchange={(e) => void setOne(f, (e.currentTarget as HTMLInputElement).checked)}
            />
            <span class="name">{f.label}</span>
            <span class="why">{effectiveWord(f)}</span>
          </label>
          {#if f.spawn_baked}
            <!-- Written into the tab at launch, so a running tab keeps whatever
                 it started with until it is restarted. Marked on the row rather
                 than only in the button below, because the user needs it BEFORE
                 the click, not after. -->
            <span class="baked" title="Applied when the tab starts — restart the tab for a change here to take effect."
              >restart to apply</span
            >
          {/if}
        </li>
      {/each}
    </ul>
    {#if restartOwed.size > 0}
      <div class="state warn">
        {restartOwed.size === 1 ? 'One control you just changed is' : 'Controls you just changed are'}
        applied when this tab starts. Restart it for the change to take effect.
      </div>
      <button type="button" class="entry" disabled={busy} onclick={() => void restart()}>
        Restart tab
      </button>
    {/if}
  {/if}

  {#if reduced.length > 0}
    <div class="separator"></div>
    <div class="state warn">Protection is reduced for this tab beyond its own settings:</div>
    <ul class="reduced">
      {#each reduced as f (f.feature)}
        <!-- "off" for a switch, "unknown" for a state cImp could not read
             (#48, H-10), "partial" for a layer running on part of what it needs
             (#48, M-25). Rendering either of the last two as the first is a
             smaller lie than rendering it as protected, but it is still a lie:
             it points the user at a switch to flip. The word comes from
             `featureStateWord` and the sentence from the row's own `reason`, so
             this list cannot describe a row differently from the tab tooltip.

             Since V39 this list is NOT the tab's own cells — those are the
             toggles above. What reaches here is what this tab lost from
             somewhere else: an app-wide flip, the master, or the signature
             layer's rules directory. -->
        <li class:unknown={f.unknown}>
          {f.label} — {featureStateWord(f)}
          <span class="why">({whyOff(f)})</span>
        </li>
      {/each}
    </ul>
    <!-- F-18: the path, not the name. "Injection protection" was always right —
         it is now a top-level Settings category, and "Tools" never was one. -->
    <div class="state">Change it in Settings → Injection protection.</div>
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
    <!-- Conditional on `row.contaminated`, because the two cases promise
         different things and neither string is safe for the other tab: on an
         uncontaminated tab (latched `local` by a local call that never fetched)
         "the injected content is still in this conversation" is untrue and a
         promise to clear the flag would be false; on a contaminated one, saying
         only "re-opens the web side" under-states a click that also releases
         persistence — the reporting-honesty class of M-21/M-22/M-24. -->
    {#if row.contaminated}
      <div class="state warn">
        Restoring full access re-opens the web side while the injected content is
        still in this conversation — the model can be steered by it and reach your
        files at the same time.
        <strong>It also clears this tab's contamination flag:</strong> new notes
        stop being held for review and save straight into project memory again.
        Notes already held stay held — release those from the Memory view. The
        record of what happened, and of this clear, stays on the Timeline.
        Continue?
      </div>
    {:else}
      <div class="state warn">
        Restoring full access re-opens both sides of the latch, so this session
        can hold web access and local file access at the same time — the
        combination the latch exists to prevent. Continue?
      </div>
    {/if}
    <div class="row">
      <button
        type="button"
        class="entry danger"
        disabled={busy}
        onclick={() => void run('unlatch')}
      >
        {row.contaminated ? 'Yes, restore access and clear the flag' : 'Yes, restore full access'}
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

    {#if memoryNote}
      <!-- Step 5d: the gap step 4 shipped with. `Latch::proxy_gate` quarantines
           a memory write whenever the latch is EXTERNAL, on the LATCH's own
           authority — the contamination bit only ever widens that verdict. So a
           user who marks a false positive here finds their notes still held and
           nothing on screen explaining it. The sentence comes from `timeline.ts`
           so this popover and the Workbench Timeline cannot phrase it
           differently. -->
      <div class="state warn">{memoryNote}</div>
    {/if}

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
  /* V39: the per-tab toggle list. Dense, because it is ten rows in a popover
     and the user is scanning for one of them. */
  .toggles {
    margin: 0;
    padding: 0 var(--space-3) 6px;
    list-style: none;
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
  }
  .toggles li {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    justify-content: space-between;
  }
  .toggles label {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1 1 auto;
    min-width: 0;
    cursor: pointer;
    padding: 2px 0;
  }
  .toggles label.disabled {
    cursor: default;
    color: var(--text-disabled);
  }
  .toggles .name {
    flex: 1 1 auto;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* The restart marker is a caveat, not an alarm: the control IS set, it just
     reaches the tab on its next launch. */
  .baked {
    color: var(--text-tertiary);
    font-size: var(--font-size-sm);
    white-space: nowrap;
  }
  .link {
    appearance: none;
    border: none;
    background: transparent;
    padding: 0;
    font: inherit;
    color: var(--accent);
    cursor: pointer;
    text-decoration: underline;
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
