<script lang="ts">
  // V39 — the tab communication popover (locked decision 7).
  //
  // The glyph next to the shield is the ONE control surface for delegation, so
  // this popover is where a tab's role and access are set. Phase A shipped the
  // Access radio; Phase B adds the Role radio (None / Manual / Remote offload),
  // the Remote-offload knobs, and a live "driven" state with a working
  // Take over.
  //
  // Modelled on `TaintMenu.svelte`, deliberately: same fixed-position anchor
  // with a viewport clamp, same deferred mousedown + Escape dismissal, same
  // "derive state from the store, never snapshot it at click time" rule — a
  // radio rendered from a click-time copy would show the pre-click value right
  // after the user's own write landed. Every value below arrives as a prop that
  // TabBar `$derived` from the settings store and the in-flight mirror, so an
  // open popover follows the tab rather than freezing.
  //
  // **Two writes, two paths, and the split is not incidental.** The ROLE goes
  // through `tab_set_delegation_role`, because "at most one Manual tab per
  // harness" is a cross-tab rule only the backend can enforce — it MOVES the
  // role in one settings mutation, so no broadcast reader can ever observe two
  // Manual tabs of one harness. The Remote-offload KNOBS are ordinary per-tab
  // fields and ride the ordinary settings save.
  import { onMount } from 'svelte';
  import {
    delegationTakeOver,
    tabSetDelegationBackend,
    tabSetDelegationRole,
    tabSetReadOnly,
  } from './ipc';
  import {
    attributionLine,
    defaultFacadeName,
    displacedToast,
    glyphState,
    harnessLabel,
    tabHarness,
    type DelegationRole,
    type InFlightView,
    type TabAccess,
  } from './delegation';
  import { settings } from './settings/store';
  import { showToast } from './toast';
  import type { DelegationBackend } from './settings/types';
  import type { TabId } from './tabs/types';

  let {
    x,
    y,
    tab,
    tabName,
    access,
    /// This tab's persisted role. Phase A always passed `'none'`.
    role = 'none' as DelegationRole,
    /// This tab's persisted facade-backend knobs — meaningless while the role
    /// is not Remote offload, and deliberately kept anyway: a user who sets a
    /// name, switches the role away and switches it back should find it where
    /// they left it.
    backend = { name: null, tier: 'quality', declared_context: null } as DelegationBackend,
    /// The delegation driving this tab right now, or `null`. Phase A had a
    /// `driven` boolean with nothing to set it; this is the real snapshot row,
    /// so the popover can name the driver AND end the flight.
    inFlight = null as InFlightView | null,
    /// The tab that currently holds Manual for this tab's harness, when it is
    /// not this one. Named in the hint below so the user knows the click will
    /// MOVE the role rather than be refused.
    manualHolder = null as { id: string; name: string } | null,
    onDismiss,
  }: {
    x: number;
    y: number;
    tab: TabId;
    tabName: string;
    access: TabAccess;
    role?: DelegationRole;
    backend?: DelegationBackend;
    inFlight?: InFlightView | null;
    manualHolder?: { id: string; name: string } | null;
    onDismiss: () => void;
  } = $props();

  let menuEl: HTMLDivElement | undefined = $state();
  let busy = $state(false);
  let err = $state<string | null>(null);

  const driven = $derived(inFlight !== null);

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

  const glyph = $derived(
    glyphState({
      role,
      access,
      inFlight: driven,
      driverName: inFlight?.driver_name ?? null,
      driverAgent: inFlight?.driver_agent ?? null,
      backendName: backend.name,
    }),
  );

  /// This tab's harness, for the Role labels. Read from the same settings
  /// mirror the backend's own rule reads, so the two cannot disagree about
  /// which tabs are in one Manual group.
  ///
  /// V40 Phase F: `''` covers both "not an AI tab" and "runs a harness this
  /// build does not know" — `tabHarness` answers `null` for the latter now
  /// rather than naming a harness it is not (locked decision 2). The Manual row
  /// then reads "another harness", which is honest, instead of offering the tab
  /// as somebody else's delegation target.
  const harness = $derived.by(() => {
    const cfg = $settings.tabs.find((t) => t.kind === 'ai_tool' && t.id === tab);
    return (cfg && cfg.kind === 'ai_tool' ? tabHarness(cfg) : null) ?? '';
  });

  /// The backend name the requesting harness would actually see. Blank falls
  /// back to `defaultFacadeName(tab)` (V39 review L-2 — never the tab name,
  /// which would tell the asking model what its "LAN backend" really is), so
  /// the placeholder states the effective value rather than leaving the field
  /// looking unset.
  const effectiveBackendName = $derived((backend.name ?? '').trim() || defaultFacadeName(tab));

  async function setRole(next: DelegationRole): Promise<void> {
    if (busy || next === role || driven) return;
    busy = true;
    err = null;
    try {
      const change = await tabSetDelegationRole(tab, next);
      if (change.displaced) {
        // Locked decision 8: the losing tab may not be visible, and a role that
        // moved silently is a `delegate_task_*` tool that started driving a
        // different tab with nothing on screen saying so.
        const lostName =
          manualHolder && manualHolder.id === change.displaced
            ? manualHolder.name
            : change.displaced;
        showToast(displacedToast(lostName, harness, tabName), 6000);
      }
    } catch (e) {
      // In the popover, not only the console: the radio would otherwise appear
      // to have taken while the tab holds no role at all. The backend's refusal
      // names its own condition (a reserved dashboard, a non-AI tab, a harness
      // with no input profile) and is shown VERBATIM — a generic sentence in its
      // place would drop the only part the user can act on.
      err = String(e);
    } finally {
      busy = false;
    }
  }

  async function setAccess(next: TabAccess): Promise<void> {
    if (busy || next === access || driven) return;
    busy = true;
    err = null;
    try {
      await tabSetReadOnly(tab, next === 'ro');
      showToast(
        next === 'ro'
          ? `“${tabName}” is now read-only — your keyboard is refused, the tab keeps running.`
          : `“${tabName}” accepts your keyboard again.`,
      );
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }

  /// Persist one Remote-offload knob.
  ///
  /// **V39 review M-10: a narrow IPC, not a whole-document save.** This used to
  /// go through `applySettings(withTabBackend(...))`, i.e. read the store,
  /// patch three fields, send the entire `Settings` — the `40d2b32`
  /// lost-update shape, and the write most likely to be racing it is the ROLE
  /// radio one line above, which has its own command precisely because only
  /// the backend can enforce its cross-tab rule. Typing a backend name could
  /// put the role back.
  ///
  /// The three knobs are sent together (the popover holds them all, live from
  /// the store); the backend writes only those three fields, under
  /// `settings.mutate`, which composes with a concurrent write instead of
  /// overwriting the document.
  async function patchBackend(patch: Partial<DelegationBackend>): Promise<void> {
    err = null;
    try {
      await tabSetDelegationBackend(tab, { ...backend, ...patch });
    } catch (e) {
      err = String(e);
    }
  }

  async function takeOver(): Promise<void> {
    if (busy) return;
    busy = true;
    err = null;
    try {
      const wasRunning = await delegationTakeOver(tab);
      showToast(
        wasRunning
          ? `You took “${tabName}” back. The driver was told the delegation was cancelled; the worker keeps running — cImp sends it no keys.`
          : `“${tabName}” was not being driven any more — the delegation had already finished.`,
        6000,
      );
      onDismiss();
    } catch (e) {
      err = String(e);
    } finally {
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
  aria-label="Delegation and access for this tab"
>
  <div class="head">Communication — {tabName}</div>
  <div class="state">{glyph.title}</div>

  <div class="separator"></div>
  <div class="head">Role</div>
  <ul class="choices">
    <li>
      <label class:disabled={driven || busy}>
        <input
          type="radio"
          name="delegation-role-{tab}"
          checked={role === 'none'}
          disabled={driven || busy}
          onchange={() => void setRole('none')}
        />
        <span class="name">None</span>
      </label>
    </li>
    <li>
      <label class:disabled={driven || busy}>
        <input
          type="radio"
          name="delegation-role-{tab}"
          checked={role === 'manual'}
          disabled={driven || busy}
          onchange={() => void setRole('manual')}
        />
        <span class="name">Manual — the {harnessLabel(harness)} delegation target</span>
      </label>
    </li>
    <li>
      <label class:disabled={driven || busy}>
        <input
          type="radio"
          name="delegation-role-{tab}"
          checked={role === 'remote_offload'}
          disabled={driven || busy}
          onchange={() => void setRole('remote_offload')}
        />
        <span class="name">Remote offload — a backend the router may pick</span>
      </label>
    </li>
  </ul>
  <!--
    Locked decision 8's move rule, said BEFORE the click. A radio whose group
    spans tabs is not something a user can see, so the one fact that makes the
    click predictable — it moves the role off that tab rather than being refused
    — has to be written down here.
  -->
  {#if manualHolder && role !== 'manual'}
    <small class="hint">
      Manual for {harnessLabel(harness)} is on “{manualHolder.name}”. Choosing it
      here moves it — that tab drops to None.
    </small>
  {:else if role === 'manual'}
    <small class="hint">
      <code>delegate_task_{harness}</code> drives this tab. Other tabs see the tool
      on their next turn; nothing restarts.
    </small>
  {/if}

  {#if role === 'remote_offload'}
    <!--
      The facade's knobs (decision 8). They live on the tab, next to the role,
      because there is no "add backend" step — Phase C SYNTHESIZES the backend
      entry from this row, so there is no second place that can disagree.
    -->
    <div class="knobs">
      <label class="field">
        <span>Backend name</span>
        <input
          type="text"
          value={backend.name ?? ''}
          placeholder={effectiveBackendName}
          disabled={busy}
          onchange={(e) => {
            const v = (e.currentTarget as HTMLInputElement).value.trim();
            void patchBackend({ name: v.length > 0 ? v : null });
          }}
        />
      </label>
      <small class="hint">
        What the requesting harness sees — never the tab. Empty falls back to
        “{effectiveBackendName}”, a stable name derived from this tab's id: the
        asking model must not be able to tell a worker tab from a LAN box.
      </small>
      <label class="field">
        <span>Tier</span>
        <select
          value={backend.tier}
          disabled={busy}
          onchange={(e) =>
            void patchBackend({
              tier: (e.currentTarget as HTMLSelectElement).value as DelegationBackend['tier'],
            })}
        >
          <option value="fast">fast</option>
          <option value="quality">quality</option>
        </select>
      </label>
      <label class="field">
        <span>Declared context</span>
        <input
          type="number"
          min="0"
          step="1024"
          value={backend.declared_context ?? ''}
          placeholder="(a generous default)"
          disabled={busy}
          onchange={(e) => {
            const raw = (e.currentTarget as HTMLInputElement).value.trim();
            const n = Math.max(0, Math.round(Number(raw) || 0));
            void patchBackend({ declared_context: raw.length > 0 && n > 0 ? n : null });
          }}
        />
      </label>
      <small class="hint">
        Tokens this worker can actually use. Under-declared and the router sends
        it away from work it could have done; over-declared and it fails visibly,
        in its own tab.
      </small>
    </div>
  {/if}

  <div class="separator"></div>
  <div class="head">Access</div>
  <ul class="choices">
    <li>
      <label class:disabled={driven || busy}>
        <input
          type="radio"
          name="delegation-access-{tab}"
          checked={!driven && access === 'rw'}
          disabled={driven || busy}
          onchange={() => void setAccess('rw')}
        />
        <span class="name">Read/write</span>
      </label>
    </li>
    <li>
      <label class:disabled={driven || busy}>
        <input
          type="radio"
          name="delegation-access-{tab}"
          checked={!driven && access === 'ro'}
          disabled={driven || busy}
          onchange={() => void setAccess('ro')}
        />
        <span class="name">Read-only</span>
      </label>
    </li>
    <!--
      The engine's own lock, shown as a third, disabled state rather than by
      hiding the radio: a control that vanishes reads as "there is no such
      setting", and the user's next move is to go looking for it. Take over is
      how this one ends — a radio button never lifts a lock a delegation owns.
    -->
    {#if inFlight}
      <li>
        <label class="disabled">
          <input type="radio" name="delegation-access-{tab}" checked disabled />
          <span class="name">Read-only (driven by {inFlight.driver_name || 'another tab'})</span>
        </label>
      </li>
    {/if}
  </ul>

  {#if inFlight}
    <div class="state driven-line">
      {attributionLine(inFlight.driver_agent, inFlight.driver_name)}
      {#if inFlight.awaiting_prompt}
        <br />This tab is waiting for your permission — the keyboard is relaxed for
        the prompt, and the driver's wait was extended once.
      {/if}
    </div>
    <div class="row">
      <button type="button" class="entry" disabled={busy} onclick={() => void takeOver()}>
        Take over (cancel delegation)
      </button>
    </div>
    <small class="hint">
      Stops cImp waiting and unlocks your keyboard. The worker is sent nothing —
      no Escape, no interrupt — so it finishes its turn visibly.
    </small>
  {/if}

  {#if err}
    <div class="state err">{err}</div>
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
  /* The attribution line, in the popover's own voice: it is the one piece of
     text here that describes what is happening RIGHT NOW rather than what is
     configured. */
  .driven-line {
    color: var(--accent);
  }
  .err {
    color: var(--text-danger-soft);
  }
  .hint {
    display: block;
    padding: 0 var(--space-3) 6px;
    font-size: var(--font-size-sm);
    color: var(--text-tertiary);
    line-height: 1.35;
  }
  .choices {
    margin: 0;
    padding: 0 var(--space-3) 6px;
    list-style: none;
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
  }
  .choices li {
    display: flex;
    align-items: center;
  }
  .choices label {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1 1 auto;
    min-width: 0;
    cursor: pointer;
    padding: 2px 0;
  }
  .choices label.disabled {
    cursor: default;
    color: var(--text-disabled);
  }
  .choices .name {
    flex: 1 1 auto;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .knobs {
    padding: 0 var(--space-3);
  }
  .field {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 2px 0;
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
  }
  .field > span {
    flex: 0 0 auto;
    min-width: 106px;
  }
  .field input,
  .field select {
    flex: 1 1 auto;
    min-width: 0;
    background: var(--surface-2);
    color: var(--text-primary);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-sm);
    padding: 2px 4px;
    font-family: inherit;
    font-size: var(--font-size-sm);
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
