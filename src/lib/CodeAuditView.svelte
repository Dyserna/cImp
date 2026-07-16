<script lang="ts">
  // V23 Phase C / V25 Phase D: the Code Audit surface — formerly its own
  // reserved tab, since schema v27 the "Code audit" section inside the Tool
  // Activity tab (ToolActivityView keeps it alive hidden across section
  // switches). One surface, two sub-tabs (the Code Intelligence section
  // pattern): Security (osv-scanner / gitleaks / semgrep) and Quality
  // (language-gated linters / dead-code / spell-check), each a full
  // `AuditPanel` instance. BOTH panels stay mounted — only the inactive one
  // is display:none — so a running scan keeps streaming into a hidden
  // sub-tab.
  import AuditPanel from './AuditPanel.svelte';
  import { loadViewSection, saveViewSection } from './viewSection';

  type Section = 'security' | 'quality';
  const SECTIONS: { id: Section; label: string }[] = [
    { id: 'security', label: 'Security' },
    { id: 'quality', label: 'Quality' },
  ];
  // Selection survives the tab-closed destroy/recreate cycle and app
  // restarts — same persistence as the Code Intelligence sections.
  let section = $state<Section>(
    loadViewSection('code-audit', SECTIONS.map((s) => s.id), 'security'),
  );
  $effect(() => saveViewSection('code-audit', section));
</script>

<div class="code-audit-tab">
  <header>
    <h2>Code Audit</h2>
  </header>
  <nav class="sections">
    {#each SECTIONS as s (s.id)}
      <button
        type="button"
        class="seg"
        class:active={section === s.id}
        onclick={() => (section = s.id)}
      >{s.label}</button>
    {/each}
  </nav>
  <!-- The `view` keys are the panels' localStorage filter namespaces; they
       predate the sub-tab merge (the Quality panel was its own tab), so
       keeping them preserves users' persisted filters. -->
  <div class="panel-host" class:hidden={section !== 'security'}>
    <AuditPanel category="security" view="code-audit" />
  </div>
  <div class="panel-host" class:hidden={section !== 'quality'}>
    <AuditPanel category="quality" view="code-quality" />
  </div>
</div>

<style>
  .code-audit-tab {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    font-size: 13px;
    color: var(--text, #ddd);
  }
  /* Title above the section row — the Workbench / Code Intelligence header
     layout. The 16px side padding lives on the header/nav (not the
     container) because each AuditPanel below carries its own 16px padding. */
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 16px 16px 0;
    margin-bottom: 14px;
  }
  header h2 {
    margin: 0;
    font-size: 15px;
  }
  /* Same segmented sub-tab look as the Code Intelligence sections. */
  nav.sections {
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
    margin: 0 16px;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--border, #333);
  }
  .seg {
    padding: 4px 12px;
    border-radius: 6px;
    border: 1px solid transparent;
    background: transparent;
    color: var(--text, #ddd);
    font-size: 12px;
    cursor: pointer;
    opacity: 0.7;
  }
  .seg:hover {
    background: rgba(255, 255, 255, 0.06);
    opacity: 1;
  }
  .seg.active {
    background: var(--accent, #3b6ea5);
    color: #fff;
    opacity: 1;
    border-color: var(--accent, #3b6ea5);
  }
  /* Each AuditPanel roots an absolute-inset element; the host gives it a
     positioned, flex-filling box. `display: none` (not unmount) keeps the
     hidden panel's state + event stream alive. */
  .panel-host {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  .panel-host.hidden {
    display: none;
  }
</style>
