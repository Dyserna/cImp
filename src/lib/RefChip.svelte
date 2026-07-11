<script lang="ts">
  // One git ref decoration chip ("HEAD -> develop", "tag: v1", "origin/x"),
  // shared by SessionCommitsView and GitGraphView so the label rules and
  // styling can't drift between the two.
  let { r }: { r: string } = $props();

  /// "HEAD -> develop" / "tag: v1" → short chip label.
  const label = $derived(r.replace(/^HEAD -> /, '').replace(/^tag: /, '⌂ '));
</script>

<span class="ref" class:head={r.startsWith('HEAD')} title={r}>{label}</span>

<style>
  .ref {
    flex: 0 0 auto;
    font-size: var(--font-size-xs);
    color: var(--text-info, #9cf);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-sm);
    padding: 0 4px;
    max-width: 18ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ref.head {
    color: var(--text-success, #9f9);
    font-weight: var(--font-weight-semibold, 600);
  }
</style>
