<script lang="ts">
  // V23 Phase C / V25 Phase D: the reserved, app-rendered Code Audit tab — the
  // Security half of the audit registry (osv-scanner / gitleaks / semgrep). A
  // thin wrapper over the shared `AuditPanel` (which Code Quality also mounts
  // with `category="quality"`); rendered by the app-view keep-alive registry
  // (appViews.ts) for the `code-audit` tab id, so a scan keeps streaming while
  // the tab is hidden.
  import AuditPanel from './AuditPanel.svelte';
</script>

<AuditPanel category="security" view="code-audit" />
