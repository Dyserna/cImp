<script lang="ts">
  /// Settings → Workbench (#129 (c)). The vibe-coding guardrails: the tab
  /// itself, and the automatic-checkpoint knobs behind it.
  ///
  /// Pure `snapshot` / `patch`: every control here is an ordinary settings
  /// field, there is no load, no poll and no `applySettings` of its own —
  /// `patch()` owns the draftSync lost-update gate and a second writer would be
  /// a second place to get that wrong.
  ///
  /// No CSS travelled with it: the section is headings, `small.hint` prose and
  /// the two primitives, all of which are `settings-chrome.css` rules keyed on
  /// the `.settings-chrome` class the parent puts on `.root` — this renders
  /// inside that element, so they still apply through the DOM.
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
  <h2>Workbench</h2>
  <small class="hint top">
    Vibe-coding guardrails: a live diff pane, automatic checkpoints
    (a separate shadow git repo — your own <code>.git</code> is never
    touched), and a worktree manager for running parallel agents
    safely. The tab is cheap to keep around; checkpoints are a
    heavier, opt-in feature below.
  </small>
  <Toggle
    label="Show the Workbench tab"
    checked={snapshot.workbench.enabled}
    onchange={(next) => patch((s) => (s.workbench.enabled = next))}
  />

  <h3>Checkpoints</h3>
  <Toggle
    label="Enable automatic checkpoints"
    checked={snapshot.workbench.checkpoints}
    onchange={(next) => patch((s) => (s.workbench.checkpoints = next))}
  />
  <small class="hint">
    Off by default in V1 — Diff and Worktrees work without it. When
    on, cImp periodically snapshots your working tree into a separate
    shadow git repo (your own <code>.git</code> is never touched).
    Enable this to start capturing checkpoints; restore one from the
    Workbench tab's Timeline section. The per-prompt checkpoint trigger
    rides the harness prompt hook installed at tab launch (needs the code
    graph) — if context injection is off, restart the tab after enabling
    this.
  </small>
  <NumberField
    label="Max checkpoints kept"
    min="1"
    value={snapshot.workbench.checkpoint_max}
    disabled={!snapshot.workbench.checkpoints}
    onchange={(next) =>
      patch(
        (s) =>
          (s.workbench.checkpoint_max = Math.max(
            1,
            Number(next) || 100,
          )),
      )}
  />
  <NumberField
    label="Max checkpoint age (days)"
    min="1"
    value={snapshot.workbench.checkpoint_max_age_days}
    disabled={!snapshot.workbench.checkpoints}
    onchange={(next) =>
      patch(
        (s) =>
          (s.workbench.checkpoint_max_age_days = Math.max(
            1,
            Number(next) || 7,
          )),
      )}
  />
  <small class="hint">
    The burst trigger fires an "activity" checkpoint when a shell tab
    or other non-hooked flow touches several files at once — the
    fallback that covers what the per-prompt trigger can't see.
  </small>
  <NumberField
    label="Burst trigger: files changed"
    min="1"
    value={snapshot.workbench.checkpoint_burst_files}
    disabled={!snapshot.workbench.checkpoints}
    onchange={(next) =>
      patch(
        (s) =>
          (s.workbench.checkpoint_burst_files = Math.max(
            1,
            Number(next) || 5,
          )),
      )}
  />
  <NumberField
    label="Burst trigger: time window (seconds)"
    min="1"
    value={snapshot.workbench.checkpoint_burst_window_s}
    disabled={!snapshot.workbench.checkpoints}
    onchange={(next) =>
      patch(
        (s) =>
          (s.workbench.checkpoint_burst_window_s = Math.max(
            1,
            Number(next) || 60,
          )),
      )}
  />
  <small class="hint">
    The minimum gap is enforced per AI tab, not per project: with two
    tabs open on one project, each tab's prompt can still take its own
    checkpoint inside the other's cooldown — so the Timeline can show
    which checkpoint was live for a given tab. Two tabs editing one
    working tree do interleave their checkpoints, so restoring one
    tab's checkpoint can roll back the other's work.
  </small>
  <NumberField
    label="Minimum gap between snapshots, per tab (seconds)"
    min="1"
    value={snapshot.workbench.checkpoint_min_gap_s}
    disabled={!snapshot.workbench.checkpoints}
    onchange={(next) =>
      patch(
        (s) =>
          (s.workbench.checkpoint_min_gap_s = Math.max(
            1,
            Number(next) || 120,
          )),
      )}
  />
</section>
