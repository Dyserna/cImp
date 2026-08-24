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
  import SectionNav from './SectionNav.svelte';
  import { loadViewSection } from './viewSection';

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
</script>

<div class="code-audit-tab">
  <header>
    <h2>Code Audit</h2>
  </header>
  <SectionNav
    view="code-audit"
    sections={SECTIONS}
    bind:section
    layout="inset"
  />
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
    color: var(--text-primary, #ddd);
  }
  /* Title above the section row — the Workbench / Code Intelligence header
     layout. The 16px side padding lives on the header and on the section
     strip (`SectionNav layout="inset"`), not on the container, because each
     AuditPanel below carries its own 16px padding. */
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
