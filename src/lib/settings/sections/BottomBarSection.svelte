<script lang="ts">
  /// Settings → Bottom bar (#129 (c)) — everything the status strip along the
  /// bottom of the main window can show: the session-usage meter, the local
  /// machine panel, their arrangement, the per-harness context bar pointer, and
  /// the external-tool executables the quick-launch buttons run.
  ///
  /// Five `<section>`s, one nav entry, one component. Two of them are FEATURE
  /// SLOTS rather than fixed panels (V40 Phase F, locked decision 6): the usage
  /// meter mounts once per harness declaring `session_usage`, the context-bar
  /// pointer once per harness declaring `context_bar`, so a build whose roster
  /// has neither shows neither rather than a panel about a thing nothing does.
  /// Those two `$derived`s moved here with the markup — nothing else in the
  /// window read them — and they read the registry store directly, the same way
  /// `GraphSection` derives its inject-mechanism copy.
  ///
  /// `pickToolExe` moved too, with `pickFile`: a one-shot native dialog whose
  /// only effect is a `patch()`. It was the parent's last `pickFile` caller.
  import { harnesses, harnessLabels, harnessLabelsProse, labelForTabId } from '../../harness';
  import { pickFile, EXECUTABLE_EXTENSIONS } from '../pickFile';
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

  /// The harnesses that MOUNT each feature panel (V40 Phase F, locked decision
  /// 6). The two bottom-bar sections used to exist unconditionally and name one
  /// product in their headings; they are feature slots now, so a build with no
  /// harness that reports usage shows no usage panel at all rather than a panel
  /// about a thing nothing does.
  const sessionUsageHarnesses = $derived(
    $harnesses.filter((h) => h.features.includes('session_usage')),
  );
  const contextBarHarnesses = $derived(
    $harnesses.filter((h) => h.features.includes('context_bar')),
  );

  // Browse for an external-tool executable and store its path. Cancelling the
  // dialog leaves the current value untouched.
  async function pickToolExe(tool: keyof Settings['external_tools']) {
    const p = await pickFile('Executable', EXECUTABLE_EXTENSIONS);
    if (p) patch((s) => (s.external_tools[tool] = p));
  }
</script>

{#if sessionUsageHarnesses.length > 0}
<section>
  <h2>{harnessLabels(sessionUsageHarnesses)} session usage</h2>
  <small class="hint top">
    Shows the quota windows {harnessLabelsProse(sessionUsageHarnesses)}
    reports, in the bottom bar next to Layouts. The numbers come from
    that harness's own status line, so they need the context status bar
    (below) left on and one of its tabs to have sent at least one
    message; the widget hides until then, and dims when the last report
    gets old (tab closed, or idle too long).
  </small>
  <Toggle
    label="Show usage in the bottom bar"
    checked={snapshot.usage.enabled}
    onchange={(next) => patch((s) => (s.usage.enabled = next))}
  />
  <small class="hint">
    The toggles below pick which pieces of each window are shown
    (they apply to both the 5h and 7d readouts).
  </small>
  <Toggle
    label="Bar"
    checked={snapshot.usage.show_bar}
    disabled={!snapshot.usage.enabled}
    onchange={(next) => patch((s) => (s.usage.show_bar = next))}
  />
  <Toggle
    label="Percentage"
    checked={snapshot.usage.show_percentage}
    disabled={!snapshot.usage.enabled}
    onchange={(next) => patch((s) => (s.usage.show_percentage = next))}
  />
  <Toggle
    label="Countdown timer"
    checked={snapshot.usage.show_countdown}
    disabled={!snapshot.usage.enabled}
    onchange={(next) => patch((s) => (s.usage.show_countdown = next))}
  />
  <Toggle
    label="Reset clock (local time)"
    checked={snapshot.usage.show_reset_clock}
    disabled={!snapshot.usage.enabled}
    onchange={(next) => patch((s) => (s.usage.show_reset_clock = next))}
  />
  <NumberField
    label="Poll interval (seconds)"
    min="15"
    max="3600"
    step="15"
    value={snapshot.usage.poll_interval_secs}
    disabled={!snapshot.usage.enabled}
    onchange={(next) =>
      patch((s) => (s.usage.poll_interval_secs = Math.max(15, +next)))}
  />
  <small class="hint">
    How often the widget re-reads the status line's latest report (a
    local read — no network). Minimum 15s; the countdown ticks every
    second locally between refreshes.
  </small>
</section>
{/if}

<section>
  <h2>Local machine information</h2>
  <small class="hint top">
    Live CPU / memory / GPU / network panel in the bottom bar, right of
    the session usage meter.
  </small>
  <Toggle
    label="Show local machine information"
    checked={snapshot.system_stats.enabled}
    onchange={(next) => patch((s) => (s.system_stats.enabled = next))}
  />
  <small class="hint">
    The toggles below pick which components are shown.
  </small>
  <Toggle
    label="CPU usage"
    checked={snapshot.system_stats.show_cpu}
    disabled={!snapshot.system_stats.enabled}
    onchange={(next) => patch((s) => (s.system_stats.show_cpu = next))}
  />
  <Toggle
    label="Memory"
    checked={snapshot.system_stats.show_memory}
    disabled={!snapshot.system_stats.enabled}
    onchange={(next) => patch((s) => (s.system_stats.show_memory = next))}
  />
  <Toggle
    label="GPU (usage + VRAM)"
    checked={snapshot.system_stats.show_gpu}
    disabled={!snapshot.system_stats.enabled}
    onchange={(next) => patch((s) => (s.system_stats.show_gpu = next))}
  />
  <Toggle
    label="GPU temperature"
    checked={snapshot.system_stats.show_gpu_temp}
    disabled={!snapshot.system_stats.enabled || !snapshot.system_stats.show_gpu}
    onchange={(next) => patch((s) => (s.system_stats.show_gpu_temp = next))}
  />
  <Toggle
    label="Network"
    checked={snapshot.system_stats.show_network}
    disabled={!snapshot.system_stats.enabled}
    onchange={(next) => patch((s) => (s.system_stats.show_network = next))}
  />
  <NumberField
    label="Poll interval (seconds)"
    min="1"
    max="60"
    value={snapshot.system_stats.poll_interval_secs}
    disabled={!snapshot.system_stats.enabled}
    onchange={(next) =>
      patch((s) => (s.system_stats.poll_interval_secs = Math.max(1, +next)))}
  />
  <small class="hint">
    How often CPU / GPU / network are sampled. The graphs update at this
    rate.
  </small>
</section>

<section>
  <h2>Status bar arrangement</h2>
  <small class="hint top">
    Drag the session and local-machine panels in the bottom bar to
    reorder them, or drag one sideways to leave a gap (e.g. push the
    local-machine panel to the right). Reordering clears any gaps.
  </small>
  <button
    type="button"
    class="reset-arrangement"
    onclick={() =>
      patch(
        (s) =>
          (s.ui.status_bar = {
            items: [
              { component: 'usage', gap: 0 },
              { component: 'system_stats', gap: 0 },
            ],
          }),
      )}
  >
    Reset to default arrangement
  </button>
  <small class="hint">
    Restores the default order (session, then local machine) and removes
    any spacers you added.
  </small>
</section>

{#each contextBarHarnesses as h (h.id)}
<section>
  <h2>{h.label} context bar</h2>
  <small class="hint top">
    Adds a context-window usage bar to {h.label}'s own status line inside
    each of its tabs — e.g.
    <code>model ▓▓▓▓▓░░░░░ 50% (100k/200k)</code>, themed to your
    terminal palette. cImp wires this up only for the tabs it launches;
    your own global {h.label} configuration is left untouched. The status
    line also feeds the session-usage meter above — turning it off leaves
    that meter with no data.
  </small>
  <!-- V40 Phase B: the switch is one of the harness's own declared
       settings, so it renders with the rest of them rather than being a
       control this window hard-codes for one harness. V40 Phase F: the
       section itself is mounted by the `context_bar` feature, and every
       name in it is the descriptor's. -->
  <small class="hint">
    The switch lives with the harness that has the status line:
    <strong>Tabs → {labelForTabId($harnesses, h.tab_ids[0])}</strong>.
  </small>
</section>
{/each}

<section>
  <h2>External tools</h2>
  <small class="hint top">
    The quick-launch buttons (and shell tabs) run these tools by name,
    resolved from the <code>ebin\</code> drop-in folder first, then your
    PATH. Tools are not bundled — install them yourself, or drop the
    exe into <code>ebin\</code>. To use a specific build, point cImp at
    the exe here; leave blank to resolve normally. Takes effect the
    next time you launch the tool.
  </small>
  <label>
    <span>rustnet</span>
    <div class="input-with-action">
      <input
        type="text"
        placeholder="(use ebin / PATH)"
        value={snapshot.external_tools.rustnet}
        oninput={(e) =>
          patch(
            (s) =>
              (s.external_tools.rustnet = (
                e.currentTarget as HTMLInputElement
              ).value),
          )}
      />
      <button
        type="button"
        class="secondary"
        onclick={() => void pickToolExe('rustnet')}
      >
        Browse…
      </button>
      <button
        type="button"
        class="secondary"
        onclick={() => patch((s) => (s.external_tools.rustnet = ''))}
      >
        Clear
      </button>
    </div>
  </label>
  <label>
    <span>broot</span>
    <div class="input-with-action">
      <input
        type="text"
        placeholder="(use ebin / PATH)"
        value={snapshot.external_tools.broot}
        oninput={(e) =>
          patch(
            (s) =>
              (s.external_tools.broot = (
                e.currentTarget as HTMLInputElement
              ).value),
          )}
      />
      <button
        type="button"
        class="secondary"
        onclick={() => void pickToolExe('broot')}
      >
        Browse…
      </button>
      <button
        type="button"
        class="secondary"
        onclick={() => patch((s) => (s.external_tools.broot = ''))}
      >
        Clear
      </button>
    </div>
  </label>
</section>
