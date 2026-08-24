<script lang="ts">
  /// Settings → LLM pricing (#129 (c)). The provider/model $/MTok table behind
  /// the Code Intelligence tab's session-cost popup.
  ///
  /// **The load stays parent-owned.** `SettingsApp`'s `onMount` fires
  /// `loadLlmPricing()` up front, along with every other section's data; moving
  /// the fetch in here would make it fire on first *view* of this section
  /// instead, which is a behaviour change the issue does not sanction. So the
  /// parent keeps the four pieces of state, the `llm_pricing_get` load and the
  /// `llm_pricing_set` save (that save goes straight to the physical global
  /// settings file, NOT through `patch`/`applySettings` — an array field would
  /// otherwise land in the project overlay).
  ///
  /// What lives here is the markup plus the four row transforms, which are pure
  /// functions of the current rows: each builds the next array and hands it to
  /// `onrows`, and the parent is the one place that marks the table dirty.
  import type { LlmPricingModel } from '../types';

  let {
    rows,
    loading,
    dirty,
    error,
    onrows,
    onsave,
  }: {
    /// The pricing table, as loaded.
    rows: LlmPricingModel[];
    /// True until the first `llm_pricing_get` settles.
    loading: boolean;
    /// Unsaved edits pending. Drives the Save button and the hint beside it.
    dirty: boolean;
    /// The last load/save failure, rendered verbatim.
    error: string | null;
    /// A new row array. The parent stores it and marks the table dirty.
    onrows: (next: LlmPricingModel[]) => void;
    /// Push the table through `llm_pricing_set`.
    onsave: () => void;
  } = $props();

  function addRow(): void {
    onrows([
      ...rows,
      { provider: 'Custom', model: `model-${rows.length + 1}`, model_prefix: '', input: 0, cache_write: 0, cache_read: 0, output: 0 },
    ]);
  }
  function editText(i: number, field: 'provider' | 'model' | 'model_prefix', value: string): void {
    onrows(rows.map((r, idx) => (idx === i ? { ...r, [field]: value } : r)));
  }
  function editRate(
    i: number,
    field: 'input' | 'cache_write' | 'cache_read' | 'output',
    value: string,
  ): void {
    // Clamp garbage/negatives to 0 so a saved row can never poison the cost
    // popup's math with NaN or a negative price.
    const n = Math.max(0, Number(value) || 0);
    onrows(rows.map((r, idx) => (idx === i ? { ...r, [field]: n } : r)));
  }
  function deleteRow(i: number): void {
    onrows(rows.filter((_, idx) => idx !== i));
  }
</script>

<section>
  <h2>LLM pricing</h2>
  <small class="hint top">
    Provider/model token prices (USD per <strong>million tokens</strong>,
    "MTok") used by the Code Intelligence tab's session-cost popup and
    its Usage view's <em>est. cost</em> mode (auto-matched by the
    <em>Id prefix</em> column). Fresh installs are seeded with current
    vendor API and GitHub Copilot rates — cache-write priced at the
    1-hour-TTL 2× rate a real session actually pays; every
    value is editable, and prices drift, so corrections are yours to
    make (no auto-update). Saved to the global settings file, not this
    project's overlay.
  </small>
  {#if loading}
    <small class="hint">Loading…</small>
  {:else}
    {#if rows.length === 0}
      <small class="hint">No entries — add one below.</small>
    {:else}
      <div class="pricing-head-row">
        <span>Provider</span>
        <span>Model</span>
        <span title="Transcript model-id prefix this row auto-matches in the Usage view's cost mode. Longest match wins; empty = manual-pick only.">Id prefix</span>
        <span class="num">Input</span>
        <span class="num">Cache write</span>
        <span class="num">Cache read</span>
        <span class="num">Output</span>
        <span></span>
      </div>
      <!-- Keyed by index deliberately, same as the MCP editor: rows are
           editable and replaced (cloned) on every edit, so a value-based
           key would change mid-edit and drop input focus. -->
      {#each rows as row, i (i)}
        <div class="pricing-row">
          <input
            type="text"
            placeholder="Provider"
            value={row.provider}
            oninput={(e) =>
              editText(i, 'provider', (e.currentTarget as HTMLInputElement).value)}
          />
          <input
            type="text"
            placeholder="Model"
            value={row.model}
            oninput={(e) =>
              editText(i, 'model', (e.currentTarget as HTMLInputElement).value)}
          />
          <input
            type="text"
            placeholder="model-id prefix"
            title="Transcript model-id prefix for cost-mode auto-match (longest wins; empty = manual-pick only)"
            value={row.model_prefix}
            oninput={(e) =>
              editText(i, 'model_prefix', (e.currentTarget as HTMLInputElement).value)}
          />
          <input
            type="number"
            class="num"
            min="0"
            step="0.01"
            title="$ per MTok, input tokens"
            value={row.input}
            onchange={(e) =>
              editRate(i, 'input', (e.currentTarget as HTMLInputElement).value)}
          />
          <input
            type="number"
            class="num"
            min="0"
            step="0.01"
            title="$ per MTok, cache-write tokens"
            value={row.cache_write}
            onchange={(e) =>
              editRate(i, 'cache_write', (e.currentTarget as HTMLInputElement).value)}
          />
          <input
            type="number"
            class="num"
            min="0"
            step="0.01"
            title="$ per MTok, cache-read tokens"
            value={row.cache_read}
            onchange={(e) =>
              editRate(i, 'cache_read', (e.currentTarget as HTMLInputElement).value)}
          />
          <input
            type="number"
            class="num"
            min="0"
            step="0.01"
            title="$ per MTok, output tokens"
            value={row.output}
            onchange={(e) =>
              editRate(i, 'output', (e.currentTarget as HTMLInputElement).value)}
          />
          <button type="button" class="secondary danger" onclick={() => deleteRow(i)}>
            Delete
          </button>
        </div>
      {/each}
    {/if}
    <div class="button-row">
      <button type="button" onclick={addRow}>Add model</button>
      <button type="button" disabled={!dirty} onclick={onsave}>
        Save
      </button>
      {#if dirty}
        <small class="hint">Unsaved changes</small>
      {/if}
    </div>
    {#if error}
      <small class="error">{error}</small>
    {/if}
  {/if}
</section>

<style>
  /* LLM pricing editor: shared column template so the header row and every
     data row line up as a table. Provider/model get the flexible tracks; the
     four $/MTok fields are fixed-width numerics. */
  .pricing-head-row,
  .pricing-row {
    display: grid;
    /* V16 Feature 8 added the Id-prefix column between Model and Input. */
    grid-template-columns: minmax(6rem, 0.7fr) minmax(8rem, 1fr) minmax(7rem, 0.9fr) 5.5rem 5.5rem 5.5rem 5.5rem auto;
    gap: 0.4rem;
    align-items: center;
    margin-top: 0.4rem;
  }
  .pricing-head-row {
    font-size: var(--font-size-sm);
    color: var(--text-quiet, #999);
    margin-top: 0.8rem;
  }
  .pricing-head-row .num,
  .pricing-row input.num {
    text-align: right;
  }
</style>
