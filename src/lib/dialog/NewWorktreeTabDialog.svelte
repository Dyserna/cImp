<script lang="ts">
  // V13 Phase D D3 — "New <harness> tab in worktree…". A name
  // prompt: creates a fresh cImp worktree (`.cimp/worktrees/<slug>`, branch
  // `cimp/<slug>` cut from HEAD) and spawns a duplicate of the source AI
  // tab's config into it (`cwd` set to the worktree's path — D2). The new
  // tab's title is prefixed `⑂ <slug>` by the backend.
  //
  // Mounted once in App.svelte alongside the other modal dialogs; renders
  // only when the dialog store's discriminator is 'new-worktree-tab'.
  import { closeDialog, dialogState } from './store';
  import ModalShell from './ModalShell.svelte';
  import { createAiTabInWorktree, type TabLifecycleError } from '../ipc';
  import { cancelPlacement, requestTabIntoPane } from '../layout/store';
  import { bumpWorkbenchWorktreesVersion } from '../workbench';
  import { errorMessage } from '../errors';

  let isOpen = $derived($dialogState.kind === 'new-worktree-tab');
  let template = $derived($dialogState.kind === 'new-worktree-tab' ? $dialogState.template : null);
  let paneId = $derived($dialogState.kind === 'new-worktree-tab' ? $dialogState.paneId : null);

  let slug = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);

  let wasOpen = false;
  $effect(() => {
    if (isOpen && !wasOpen) {
      wasOpen = true;
      slug = '';
      busy = false;
      error = null;
    } else if (!isOpen && wasOpen) {
      wasOpen = false;
    }
  });

  // Same character set the backend's `sanitize_slug` accepts — checked
  // client-side only for immediate feedback; the backend is the source of
  // truth and re-validates regardless.
  const SLUG_RE = /^[A-Za-z0-9][A-Za-z0-9_-]*$/;
  let trimmed = $derived(slug.trim());
  let slugValid = $derived(trimmed.length > 0 && trimmed.length <= 60 && SLUG_RE.test(trimmed));

  function cancel(): void {
    if (busy) return;
    closeDialog();
  }

  async function submit(): Promise<void> {
    if (busy || !template || !paneId || !slugValid) return;
    busy = true;
    error = null;
    const placement = requestTabIntoPane(paneId);
    try {
      await createAiTabInWorktree(template, trimmed);
      bumpWorkbenchWorktreesVersion();
      closeDialog();
    } catch (e) {
      // The create failed (duplicate slug, detached HEAD, …), so the pane
      // placement queued above will never be consumed — cancel it, or the
      // next tab created ANYWHERE would be silently routed into this pane
      // (the layout store's placement contract).
      cancelPlacement(placement);
      const wire = e as TabLifecycleError | string | null;
      if (wire && typeof wire === 'object' && 'kind' in wire) {
        error = wire.kind === 'internal' ? wire.message : wire.kind;
      } else {
        error = errorMessage(e);
      }
    } finally {
      busy = false;
    }
  }
</script>

<ModalShell
  open={isOpen}
  label="New tab in worktree"
  title="New tab in worktree"
  width={420}
  titleGap="md"
  onCancel={cancel}
  onEscape={cancel}
  onEnter={() => void submit()}
>
  <p class="note">
    Creates an isolated git worktree + branch (<code>cimp/&lt;name&gt;</code>)
    cut from the current branch's <code>HEAD</code>, then opens a tab there.
    Uncommitted changes in the main working tree are NOT included — the
    worktree starts from the last commit.
  </p>
  <label class="field">
    <span>Name</span>
    <!-- svelte-ignore a11y_autofocus -->
    <input
      type="text"
      bind:value={slug}
      placeholder="fix-login-bug"
      autofocus
      disabled={busy}
    />
  </label>
  {#if trimmed.length > 0 && !slugValid}
    <p class="msg err">
      Letters, digits, '-', and '_' only, starting with a letter or digit
      (max 60 characters).
    </p>
  {/if}
  {#if error}
    <p class="msg err">{error}</p>
  {/if}
  {#snippet actions()}
    <button type="button" class="cancel" onclick={cancel} disabled={busy}>Cancel</button>
    <button type="button" class="primary" onclick={submit} disabled={busy || !slugValid}>
      {busy ? 'Creating…' : 'Create'}
    </button>
  {/snippet}
</ModalShell>

<style>
  .note {
    margin: 0 0 var(--space-3);
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
    line-height: 1.4;
  }
  .note code {
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: 0.9em;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-bottom: var(--space-2);
  }
  .field span {
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
  }
  .field input {
    padding: 6px 8px;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border-default);
    background: var(--surface-input);
    color: var(--text-primary);
    font-size: var(--font-size-md);
  }
  .msg {
    margin: 0 0 var(--space-2);
    font-size: var(--font-size-sm);
  }
  .msg.err {
    color: var(--text-danger-soft);
  }
  .cancel {
    background: var(--surface-4);
    color: var(--text-secondary);
  }
  .cancel:hover:not([disabled]) {
    background: var(--surface-input);
    color: var(--text-primary);
  }
  .primary {
    background: var(--accent);
    color: var(--accent-fg);
    border-color: var(--accent);
    font-weight: var(--font-weight-semibold);
  }
  .primary:hover:not([disabled]) {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }
  button[disabled] {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
