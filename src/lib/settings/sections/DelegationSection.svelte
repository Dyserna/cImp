<script lang="ts">
  /// Settings → Delegation (#148) — the two cross-harness delegation knobs
  /// that are not per tab.
  ///
  /// **Why it is its own top-level category**, and not the `<h3>` at the bottom
  /// of Offload task tools → Tools it used to be. Delegation is one tab driving
  /// another tab's harness; offload is cImp handing a task to a local model
  /// server. They meet in exactly one place — the facade backends list, which
  /// stays in the Offload section because it is a BACKEND entry — and nowhere
  /// else. Filed under Offload, these two knobs governed every AI tab from
  /// inside a section about a model server, two sub-tabs deep, which is the
  /// same "a control the user cannot find is a control that is not there"
  /// failure F-18 raised and V33 decision 16 created Sandboxing to avoid.
  ///
  /// Pure `snapshot` / `patch`, like `WorkbenchSection`: no load, no poll, no
  /// `applySettings` of its own — `patch()` owns the draftSync lost-update
  /// gate, and a second writer would be a second place to get that wrong.
  ///
  /// No CSS travelled with it: headings, `small.hint` prose and the two
  /// primitives are all `settings-chrome.css` rules keyed on the
  /// `.settings-chrome` class the parent puts on `.root`.
  import type { Settings } from '../types';
  import NumberField from '../NumberField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
  } = $props();
</script>

<section>
  <h2>Cross-harness delegation</h2>
  <small class="hint top">
    V39: one tab drives another — cImp types a request into an open
    harness tab exactly as you would, waits for the turn to finish, and
    hands the answer back to the tab that asked. Which tabs may be driven
    is set per tab, from that tab's <code>⇄</code> icon; these are
    the two knobs that are not per tab. Every run is a row in the Events
    tab under <strong>delegation</strong>.
  </small>
  <Toggle
    label="Lock a tab's keyboard while another harness is driving it"
    checked={snapshot.delegation.auto_read_only}
    onchange={(next) => patch((s) => (s.delegation.auto_read_only = next))}
  />
  <small class="hint">
    On by default. While cImp is typing into a tab, a stray keystroke of
    yours lands in the middle of someone else's turn. A courtesy lock over
    your own hands, not a security boundary: a permission or question
    prompt relaxes it for that prompt, and <strong>Take over</strong> — on
    the tab's <code>⇄</code> popover and its context menu — clears it
    outright and ends the delegation. Turning it off leaves the tab
    writable throughout; the banner and the glyph still say it is being
    driven.
  </small>
  <NumberField
    label="Default timeout (seconds)"
    min="1"
    max="86400"
    step="1"
    value={snapshot.delegation.default_timeout_s}
    onchange={(next) =>
      patch(
        (s) =>
          (s.delegation.default_timeout_s = Math.max(
            1,
            Math.round(+next || 600),
          )),
      )}
  >
    <small class="hint">
      How long cImp waits for a worker's reply when the caller named no
      timeout of its own. On expiry the asking tab is told
      <code>timeout</code> and <strong>no keys are ever sent</strong> to
      cancel the worker — it finishes its turn visibly, in its own tab.
      A standing permission prompt buys one bounded extension, so a run
      waiting on you does not expire while you walk over to it.
    </small>
  </NumberField>
  <small class="hint">
    A remote-offload worker tab is configured from the same <code>⇄</code>
    popover; the facade it advertises to the asking harness is a backend
    entry, so it lives in <strong>Settings → Offload task tools</strong>
    → Pool with the rest of the pool.
  </small>
</section>
