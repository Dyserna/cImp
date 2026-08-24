<script lang="ts">
  /// Settings → Compose (#129 (c)) — V14 Phase A's prompt library.
  ///
  /// Same division as `PricingSection`, for the same reason: the eager
  /// `loadComposeTemplates()` in `SettingsApp`'s `onMount` stays parent-owned
  /// (moving the fetch here would make it fire on first view instead), as does
  /// the `compose_templates_global_set` save — global-only, deliberately NOT
  /// through `patch`/`applySettings`. The four global-template transforms are
  /// pure functions of the current list and live here with the markup; the
  /// parent marks the list dirty in one place.
  ///
  /// Project templates are a read-only listing: they live in the project's
  /// `.cimp/config.json` and are edited by hand.
  import type { PromptTemplate } from '../types';

  let {
    globals,
    projects,
    loading,
    dirty,
    error,
    onglobals,
    onsave,
  }: {
    /// Global templates, editable here.
    globals: PromptTemplate[];
    /// This project's templates — read-only.
    projects: PromptTemplate[];
    /// True until the first load settles.
    loading: boolean;
    /// Unsaved edits pending.
    dirty: boolean;
    /// The last load/save failure, rendered verbatim.
    error: string | null;
    /// A new global-template list. The parent stores it and marks it dirty.
    onglobals: (next: PromptTemplate[]) => void;
    /// Push the global list through `compose_templates_global_set`.
    onsave: () => void;
  } = $props();

  function addTemplate(): void {
    onglobals([...globals, { name: `template-${globals.length + 1}`, body: '' }]);
  }
  function renameTemplate(i: number, name: string): void {
    onglobals(globals.map((t, idx) => (idx === i ? { ...t, name } : t)));
  }
  function editTemplateBody(i: number, body: string): void {
    onglobals(globals.map((t, idx) => (idx === i ? { ...t, body } : t)));
  }
  function deleteTemplate(i: number): void {
    onglobals(globals.filter((_, idx) => idx !== i));
  }
</script>

<section>
  <h2>Compose</h2>
  <small class="hint top">
    Saved prompt templates, insertable from the compose overlay's
    <code>/</code> picker (type <code>/</code> on an empty line, or
    click the 📋 button beside the textarea). Variables:
    <code>{'{selection}'}</code> (the focused pane's terminal
    selection) and <code>{'{clipboard}'}</code> (the system
    clipboard) are filled in immediately; any other
    <code>{'{name}'}</code> becomes a tab-stop you Tab between and
    overtype after inserting.
  </small>

  <h3>Global templates</h3>
  <small class="hint">
    Available from every project. Saved directly to the global
    settings file, not this project's overlay.
  </small>
  {#if loading}
    <small class="hint">Loading…</small>
  {:else}
    {#if globals.length === 0}
      <small class="hint">No templates yet — add one below.</small>
    {:else}
      <ul class="template-list compose-template-list">
        {#each globals as t, i (i)}
          <li class="compose-template-row">
            <input
              type="text"
              class="compose-template-name"
              placeholder="name"
              value={t.name}
              oninput={(e) =>
                renameTemplate(i, (e.currentTarget as HTMLInputElement).value)}
            />
            <textarea
              class="compose-template-body"
              placeholder={'Template body — use {selection}, {clipboard}, or {any-name} for tab-stops'}
              rows="2"
              value={t.body}
              oninput={(e) =>
                editTemplateBody(i, (e.currentTarget as HTMLTextAreaElement).value)}
            ></textarea>
            <button type="button" class="danger" onclick={() => deleteTemplate(i)}
              >Delete</button
            >
          </li>
        {/each}
      </ul>
    {/if}
    <div class="button-row">
      <button type="button" onclick={addTemplate}>Add template</button>
      <button
        type="button"
        disabled={!dirty}
        onclick={onsave}
        >Save</button
      >
      {#if dirty}
        <small class="hint">Unsaved changes</small>
      {/if}
    </div>
    {#if error}
      <small class="error">{error}</small>
    {/if}
  {/if}

  <h3>Project templates</h3>
  <small class="hint">
    Read-only here — project-scope templates live in this project's
    <code>.cimp/config.json</code> (a top-level
    <code>prompt_templates</code> array), edited by hand or committed
    for team sharing. A project template shadows a global one of the
    same name.
  </small>
  {#if projects.length === 0}
    <small class="hint">None for this project.</small>
  {:else}
    <ul class="template-list">
      {#each projects as t (t.name)}
        <li>
          <span class="template-name" title={t.body}>{t.name}</span>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  /* V14 Phase A: Compose section's global-template editor rows — a name
     field, a growable body textarea, and a delete button. Unlike the
     read-only `.template-list` rows (now in `settings-chrome.css`, shared
     with the offload section's saved-server lists), each entry here is
     directly editable.

     `.compose-template-list` ties on specificity (0,2,0) with the chrome
     sheet's `.settings-chrome .template-list` it overrides. The tie is
     resolved by order: SettingsApp imports the chrome sheet FIRST, ahead of
     every child component's CSS, exactly so a child's own rule wins a tie. */
  .compose-template-list {
    gap: var(--space-3);
  }
  .compose-template-row {
    align-items: flex-start !important;
  }
  .compose-template-name {
    flex: 0 0 10rem;
  }
  .compose-template-body {
    flex: 1 1 auto;
    min-width: 0;
    font-family: inherit;
    font-size: var(--font-size-sm, 13px);
    resize: vertical;
    padding: 6px 8px;
  }
</style>
