<script lang="ts">
  // V25 Phase D: the reserved, app-rendered Code Quality tab — the Quality half
  // of the audit registry (language-gated linters / dead-code / spell-check). A
  // thin wrapper over the shared `AuditPanel` (which Code Audit also mounts with
  // `category="security"`); rendered by the app-view keep-alive registry
  // (appViews.ts) for the `code-quality` tab id, so a scan keeps streaming while
  // the tab is hidden. Chips are language-gated by the project census; a "n
  // tools hidden" line explains what the current project doesn't apply to.
  import AuditPanel from './AuditPanel.svelte';
</script>

<AuditPanel category="quality" view="code-quality" />
