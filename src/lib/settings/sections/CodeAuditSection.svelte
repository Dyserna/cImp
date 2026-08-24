<script lang="ts">
  /// Settings → Code Audit (#129 (c)) — the FEATURE's own settings. The
  /// fourteen scanners themselves are configured in Tool Plugins: since V38
  /// they are a plugin (one whose manifests cImp ships rather than one you drop
  /// in a folder), rendered by the pane that already knows how. The prose here
  /// says so and the link jumps there through `onnavigate`, the same
  /// section-jump callback `OffloadSection` uses for F-18's path corrections —
  /// `activeSection` is the window's nav state and stays the window's.
  ///
  /// **`auditCensus` is a prop, not a load of this component's.** The window
  /// fetches it once on mount (`audit_refresh_census`, which also has the
  /// backend apply auto-selection), and `noteManualToolEdit` — wired into the
  /// TOOL PLUGINS section, not this one — reads the same value. Two owners of
  /// one census is how the two panes would come to disagree about which
  /// scanners the project's languages select, so there is one, and it is the
  /// parent. Moving the fetch here would also change when it happens: on first
  /// VIEW of this section rather than on window open.
  ///
  /// `applyQualityAutoSelect` did move: it is a pure function of the census and
  /// `patch()`, and this button is its only caller.
  import { harnesses } from '../../harness';
  import { censusIsEmpty, qualityAutoSelection } from '../../codeAudit/logic';
  import type { AuditCensus } from '../../codeAudit/types';
  import { AUDIT_PLUGIN_KEY, setToolEnabled } from '../toolPlugins';
  import { harnessRow, type Settings } from '../types';
  import NumberField from '../NumberField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    auditCensus,
    harnessNames,
    onnavigate,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// The latest scan's language census. Parent-owned — see the note above:
    /// the Tool Plugins section's manual-edit hook reads the same value, and
    /// the fetch is a window-open load, not a first-view one.
    auditCensus: AuditCensus;
    /// The enabled harnesses' labels, joined. Parent-owned — three sections
    /// interpolate it.
    harnessNames: string;
    /// Jump to another Settings section (here: Tool Plugins, where the
    /// scanners live).
    onnavigate: (section: string) => void;
  } = $props();

  // The "Auto-select for this project" button: back to automatic mode, and —
  // when a census is already known — apply the project-language selection at
  // once. The rule is `codeAudit/logic`'s mirror of the backend's, and the
  // flags land in the tool-plugins container where the built-in scanners'
  // state lives.
  function applyQualityAutoSelect(): void {
    patch((s) => {
      s.code_audit.quality_auto_select = true;
      if (censusIsEmpty(auditCensus)) return;
      for (const { id, enabled } of qualityAutoSelection(auditCensus)) {
        setToolEnabled(s, AUDIT_PLUGIN_KEY, id, enabled);
      }
    });
  }
</script>

<section>
  <h2>Code Audit</h2>
  <small class="hint top">
    Aggregated security and quality scanning. cImp runs external
    scanners against the project root and merges their findings into one
    table. Nothing is bundled — each scanner resolves from the
    <code>ebin\</code> drop-in folder first, then your PATH.
    <strong>The scanners themselves are configured in
    <button type="button" class="linkish" onclick={() => onnavigate('tool-plugins')}
      >Tool Plugins</button
    ></strong>: they are a plugin cImp ships, so they are enabled, pointed
    at a binary and given their extra arguments in the same place as any
    tool you drop in yourself. What is here is the feature.
  </small>

  <Toggle
    label="Enable Code Audit (Tools → Code audit)"
    checked={snapshot.code_audit.enabled}
    onchange={(next) => patch((s) => (s.code_audit.enabled = next))}
  />

  <h3>Scan settings</h3>
  <NumberField
    label="Per-tool timeout (seconds)"
    min="1"
    value={snapshot.code_audit.timeout_secs}
    event="input"
    onchange={(next) =>
      patch((s) => {
        const v = Number(next);
        if (Number.isFinite(v) && v >= 1)
          s.code_audit.timeout_secs = Math.floor(v);
      })}
  />

  <h3>Quality tool selection</h3>
  <small class="hint">
    The quality scanners are language-gated: one only runs when the
    project contains files it applies to. In <strong>automatic</strong>
    mode cImp keeps their checkboxes following the project's languages
    (the two that run a real build or need the network stay opt-in);
    editing one of their checkboxes in Tool Plugins switches to manual so
    your choice sticks. Security scanners are never touched.
  </small>
  {#if snapshot.code_audit.quality_auto_select}
    <small class="hint audit-auto-note">
      Selection: <strong>automatic</strong> — follows this project's
      languages.
    </small>
  {:else}
    <div class="audit-auto-row">
      <button type="button" class="secondary" onclick={applyQualityAutoSelect}>
        Auto-select for this project
      </button>
      <small class="hint">
        re-select the scanners matching this project's languages and keep
        them in sync automatically
      </small>
    </div>
  {/if}

  <h3>MCP exposure</h3>
  <small class="hint">
    Advertise the <code>cimp-code-audit</code> MCP server
    (<code>security_audit</code> / <code>quality_audit</code>, native
    worker tools for offload) so AI consumers can trigger audits
    themselves. Each requires Code Audit enabled above. The server set
    is injected when an AI tab starts — after enabling Code Audit or
    flipping an exposure here, restart the {harnessNames} tab
    (Tabs → Restart) for the tools to appear.
  </small>
<!-- V40 Phase B: one box per REGISTERED harness. It was a hand-written
       two-harness pair, so Code Audit would have been unreachable from
       a third harness until someone edited this file. -->
  {#each $harnesses as h (h.id)}
    <Toggle
      checked={harnessRow(snapshot, h.id).expose_code_audit}
      onchange={(next) =>
        patch((s) => {
          const on = next;
          s.harness = {
            ...(s.harness ?? {}),
            [h.id]: { ...harnessRow(s, h.id), expose_code_audit: on },
          };
        })}
    >
      Expose to {h.label}
    </Toggle>
  {/each}
  <Toggle
    label="Expose to offload worker"
    checked={snapshot.code_audit.expose_offload}
    onchange={(next) => patch((s) => (s.code_audit.expose_offload = next))}
  />
</section>

<style>
  /* Quality auto-selection: the mode note (automatic) / re-apply row (manual).
     Both rules travelled with the markup they style — a Svelte class rule is
     scoped to whichever component holds the elements. */
  small.hint.audit-auto-note {
    display: block;
    margin: var(--space-2) 0 0;
  }
  .audit-auto-row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin-top: var(--space-2);
  }
  .audit-auto-row small.hint {
    margin: 0;
  }
</style>
