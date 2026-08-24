<script lang="ts">
  // V32 Phase G (locked decision 16) → **V39: the master switch itself.**
  //
  // Locked decision 16 requires that a reduced-protection state be "visible
  // outside Settings too … so protection cannot be off and forgotten". This chip
  // used to meet that by staying SILENT while everything was on and speaking up
  // when something was not — on the reasoning that a permanent "protected" chip
  // would be noise 99% of the time.
  //
  // V39 overrules that, and the reason is the same one #48's G-3 found one layer
  // down: a surface that says nothing when things are fine is indistinguishable
  // from a surface that says nothing because it is broken, or because the user
  // never knew it existed. "No chip" reads as "protected" only to someone who
  // already knows the chip exists. So it is permanent and colour-coded, it
  // states which way the L1 master is set, and clicking it FLIPS the master —
  // one `applySettings` write of the whole Settings object, like every other
  // write in this window.
  //
  // The four states it used to wear as its label did not go away, they moved:
  //   * the MASTER's value is the label now (`on` / `off`), because that is what
  //     the click changes and a control must say what it will do;
  //   * `reduced` — something beneath the master is off — is a modifier;
  //   * `unverified` — everything readable is on and the signature layer's
  //     armed-ness is not readable (#48, H-10);
  //   * `unknown` — the poll behind this chip has failed for several ticks
  //     (#48, G-3), so nothing beneath the master can be claimed at all.
  // The last two keep the dashed treatment: they are a broken instrument, not a
  // posture anyone chose.
  //
  // Settings stays one gesture away (F-18: AT the matrix, which says exactly
  // which control and why — it used to call `openSettingsWindow()` with no
  // argument and land on Appearance). Right-click, and the tooltip says so:
  // a control whose click does something other than what its tooltip promises is
  // worse than one that only links.
  //
  // The whole decision — which word, which tooltip, which modifier — is
  // `latch.ts`'s `injectionChipState`, because a chip that must not lie needs
  // tests and `.svelte` files have no harness in this repo.
  import { openSettingsWindowToSection } from '../settings/ipc';
  import {
    applyMasterProtection,
    injectionStatus,
    injectionStatusUnknown,
    injectionChipState,
  } from '../latch';

  const chip = $derived(injectionChipState($injectionStatus, $injectionStatusUnknown));

  function onContext(e: MouseEvent): void {
    e.preventDefault();
    void openSettingsWindowToSection('injection');
  }
</script>

<button
  type="button"
  class="status-button status-badge injection"
  class:master-on={chip.on}
  class:master-off={!chip.on}
  class:unknown={chip.degraded || chip.note === 'unknown'}
  class:reduced={chip.note === 'reduced'}
  onclick={() => void applyMasterProtection(!chip.on)}
  oncontextmenu={onContext}
  title={chip.title}
  aria-label={chip.title}
  aria-pressed={chip.on}
>
  <span class="glyph" aria-hidden="true">⛨</span>
  <span class="text">{chip.label}</span>
</button>

<style>
  /* Shell + focus ring: `.status-button.status-badge` in `src/app.css`. The
     colour stays here because it is the badge's meaning, not its shape. */
  .status-button {
    color: var(--awaiting);
  }
  /* V39: on and off are both permanent states now, so both need a colour that
     says which one you are looking at without reading the word. */
  .master-on {
    color: var(--success);
    border-color: transparent;
  }
  /* The master switch is the loudest state there is: it is not "one control is
     off", it is "none of them are on". */
  .master-off {
    color: var(--text-danger-soft);
    border-color: var(--text-danger-soft);
  }
  /* Something beneath an ON master is off. Warning-coloured rather than danger:
     the posture is reduced, not absent. */
  .master-on.reduced {
    color: var(--awaiting);
    border-color: var(--awaiting);
  }
  /* "Unknown" / "unverified" are not "off": they are a broken instrument, not a
     posture the user chose. Dashed so it does not read as a confident claim
     about anything (#48, G-3, H-10). */
  .unknown {
    border-style: dashed;
    border-color: var(--awaiting);
  }
  .status-button:hover {
    background: var(--surface-3);
  }
  .glyph {
    font-size: 12px;
  }
</style>
