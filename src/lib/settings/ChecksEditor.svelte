<script lang="ts">
  // V22 Phase E — the `run_check` checks editor. Follows the MCP tool-servers
  // editor pattern (SettingsApp.svelte): a list of rows with add/edit/delete,
  // text edits committed on blur (not per keystroke), toggles/dropdowns/EnvEditor
  // committed immediately. Writes go out through the `onchange` callback, which
  // the parent persists via the normal settings path (per-project overlay diff).
  //
  // The decision logic (conditional-field map, test-result classification, chip
  // derivation, auto-flag clearing) lives in `checksEditor.ts` so it's unit
  // tested without a component host; this file stays presentational.
  import EnvEditor from './EnvEditor.svelte';
  import type { CheckDef, ChecksProposal, ChecksTestResult, ParserKind } from './types';
  import {
    PARSER_KINDS,
    PARSER_LABELS,
    classifyTestResult,
    clearAutoOnEdit,
    newCheckDef,
    showsPattern,
    showsReportFile,
    type TestVerdict,
  } from './checksEditor';
  import {
    checksApplyProposals,
    checksDetect,
    checksTest,
    checksValidatePattern,
  } from '../checks';

  let {
    checks,
    onchange,
    root = undefined,
  }: {
    checks: CheckDef[];
    onchange: (next: CheckDef[]) => void;
    root?: string;
  } = $props();

  // Local editable copy — rows are owned here so a half-typed field doesn't
  // round-trip through the backend on every keystroke (the MCP editor's model).
  // Re-synced from the prop only when it diverges from what we last emitted
  // (an external change: a Detect apply, a settings reload) — the EnvEditor
  // signature-guard pattern.
  function clone(list: CheckDef[]): CheckDef[] {
    return list.map((c) => ({ ...c, env: c.env.map((e) => [e[0], e[1]] as [string, string]) }));
  }
  function sig(list: CheckDef[]): string {
    return JSON.stringify(list);
  }
  // svelte-ignore state_referenced_locally
  let rows = $state<CheckDef[]>(clone(checks));
  // svelte-ignore state_referenced_locally
  let lastCommitted = sig(checks);

  $effect(() => {
    const incoming = sig(checks);
    if (incoming !== lastCommitted) {
      rows = clone(checks);
      lastCommitted = incoming;
    }
  });

  // Persist the current rows through the parent's normal settings path.
  function commit(): void {
    const next = clone(rows);
    lastCommitted = sig(next);
    onchange(next);
  }

  // Any field edit of an auto-detected entry makes it user-owned (so a later
  // re-detection stops fighting the manual change — the `CheckDef.auto`
  // contract). Call this from every mutation.
  function edit(i: number, fn: (c: CheckDef) => void): void {
    fn(rows[i]);
    clearAutoOnEdit(rows[i]);
  }

  // ── exposure status (computed client-side: the gate is just checks non-empty)
  const exposed = $derived(rows.length > 0);

  // ── per-row transient UI state, keyed by index ──────────────────────────
  // Test results and regex-validation feedback don't belong in the persisted
  // CheckDef, so they live in parallel maps rekeyed as rows are added/removed.
  let testResults = $state<Record<number, ChecksTestResult>>({});
  let testBusy = $state<Record<number, boolean>>({});
  let patternError = $state<Record<number, string | null>>({});
  const patternTimers: Record<number, ReturnType<typeof setTimeout>> = {};

  function forgetRows(): void {
    // A delete shifts every later row's index down by one; the index-keyed
    // transient state would then attach to the wrong row. Simplest correct
    // thing: clear it all (the user re-runs Test as needed).
    testResults = {};
    testBusy = {};
    patternError = {};
  }

  // ── env <-> Record bridge for EnvEditor (CheckDef.env is an ordered pair list)
  function envRecord(c: CheckDef): Record<string, string> {
    const out: Record<string, string> = {};
    for (const [k, v] of c.env) if (k) out[k] = v;
    return out;
  }
  function setEnv(i: number, next: Record<string, string>): void {
    edit(i, (c) => (c.env = Object.entries(next)));
    commit();
  }

  // ── field mutations ─────────────────────────────────────────────────────
  function setName(i: number, v: string): void {
    edit(i, (c) => (c.name = v));
  }
  function setCmd(i: number, v: string): void {
    edit(i, (c) => (c.cmd = v));
  }
  function setTimeoutSecs(i: number, v: string): void {
    const n = Number.parseInt(v, 10);
    edit(i, (c) => (c.timeout_secs = Number.isFinite(n) && n > 0 ? n : 0));
  }
  function setCwd(i: number, v: string): void {
    const t = v.trim();
    edit(i, (c) => (c.cwd = t.length > 0 ? t : null));
  }
  function setReportFile(i: number, v: string): void {
    const t = v.trim();
    edit(i, (c) => (c.report_file = t.length > 0 ? t : null));
  }
  function setPattern(i: number, v: string): void {
    edit(i, (c) => (c.pattern = v.length > 0 ? v : null));
    scheduleValidate(i);
  }
  function setParser(i: number, v: ParserKind): void {
    edit(i, (c) => (c.parser = v));
    commit();
    if (showsPattern(v)) scheduleValidate(i);
    else patternError = { ...patternError, [i]: null };
  }

  // Live (debounced) regex validation — the same check the save path applies,
  // so the surfaced error matches what a save would reject.
  function scheduleValidate(i: number): void {
    if (patternTimers[i]) clearTimeout(patternTimers[i]);
    patternTimers[i] = setTimeout(() => void validatePattern(i), 300);
  }
  async function validatePattern(i: number): Promise<void> {
    const pat = rows[i]?.pattern ?? '';
    if (!pat) {
      patternError = { ...patternError, [i]: 'a pattern is required for the custom-regex parser' };
      return;
    }
    try {
      await checksValidatePattern(pat);
      patternError = { ...patternError, [i]: null };
    } catch (e) {
      patternError = { ...patternError, [i]: String(e) };
    }
  }

  function addCheck(): void {
    rows = [...rows, newCheckDef(uniqueName('check'))];
    // No commit — an empty-cmd row contributes nothing useful yet, and an
    // empty name would collide; the user fills it, then blur commits.
  }
  function removeCheck(i: number): void {
    rows = rows.filter((_, idx) => idx !== i);
    forgetRows();
    commit();
  }
  function uniqueName(base: string): string {
    const names = new Set(rows.map((r) => r.name));
    if (!names.has(base)) return base;
    let n = 2;
    while (names.has(`${base}-${n}`)) n++;
    return `${base}-${n}`;
  }

  // ── Test button ─────────────────────────────────────────────────────────
  async function runTest(i: number): Promise<void> {
    testBusy = { ...testBusy, [i]: true };
    try {
      const result = await checksTest($state.snapshot(rows[i]), root);
      testResults = { ...testResults, [i]: result };
    } catch (e) {
      testResults = {
        ...testResults,
        [i]: {
          exit_code: null,
          duration_ms: 0,
          timed_out: false,
          diag_count: 0,
          stdout_bytes: 0,
          stderr_bytes: 0,
          diagnostics: [],
          error: String(e),
        },
      };
    } finally {
      testBusy = { ...testBusy, [i]: false };
    }
  }

  function verdictLabel(v: TestVerdict): string {
    switch (v) {
      case 'diagnostics':
        return 'parsed';
      case 'clean':
        return 'clean — no diagnostics';
      case 'wrong-parser':
        return 'wrong parser?';
      case 'timeout':
        return 'timed out';
      case 'error':
        return 'error';
    }
  }

  // ── Detect & configure ──────────────────────────────────────────────────
  let detecting = $state(false);
  let detectError = $state<string | null>(null);
  let proposals = $state<ChecksProposal[] | null>(null);
  // Which proposals are checked for apply (valid ones pre-checked). Keyed by
  // proposal index within the current `proposals` list.
  let selected = $state<Record<number, boolean>>({});
  let applying = $state(false);

  async function detect(): Promise<void> {
    detecting = true;
    detectError = null;
    try {
      const list = await checksDetect(root);
      proposals = list;
      const sel: Record<number, boolean> = {};
      list.forEach((p, idx) => (sel[idx] = p.valid));
      selected = sel;
    } catch (e) {
      detectError = String(e);
      proposals = null;
    } finally {
      detecting = false;
    }
  }

  async function applyProposals(): Promise<void> {
    if (!proposals) return;
    const chosen = proposals.filter((p, idx) => p.valid && selected[idx]).map((p) => p.check);
    if (chosen.length === 0) {
      proposals = null;
      return;
    }
    applying = true;
    try {
      // The backend merges into settings and broadcasts the change; the updated
      // `checks` prop flows back in and re-syncs `rows` via the $effect above.
      await checksApplyProposals(chosen, root);
      proposals = null;
    } catch (e) {
      detectError = String(e);
    } finally {
      applying = false;
    }
  }

  function cancelDetect(): void {
    proposals = null;
    detectError = null;
  }
</script>

<div class="checks-editor">
  <!-- Exposure status line: honest about whether run_check is advertised. -->
  <div class="exposure" class:on={exposed} class:off={!exposed}>
    {#if exposed}
      <span class="exp-dot" aria-hidden="true"></span>
      <span class="exp-text"
        >run_check exposed: MCP <strong>✓</strong> / offload worker <strong>✓</strong></span
      >
    {:else}
      <span class="exp-dot" aria-hidden="true"></span>
      <span class="exp-text">not exposed — no checks configured</span>
      <button type="button" class="detect-btn" onclick={detect} disabled={detecting}>
        {detecting ? 'Detecting…' : 'Detect & configure'}
      </button>
    {/if}
  </div>

  {#if exposed}
    <div class="toolbar">
      <button type="button" class="detect-btn" onclick={detect} disabled={detecting}>
        {detecting ? 'Detecting…' : 'Detect & configure'}
      </button>
    </div>
  {/if}

  {#if detectError}
    <p class="detect-error">{detectError}</p>
  {/if}

  <!-- Proposal picker (Phase D output rendered here). -->
  {#if proposals}
    <div class="proposals">
      <div class="proposals-head">
        Detected {proposals.length} candidate{proposals.length === 1 ? '' : 's'}
        {#if proposals.length === 0}— nothing recognized in this project.{/if}
      </div>
      {#each proposals as p, idx (idx)}
        <label class="proposal" class:invalid={!p.valid}>
          <input
            type="checkbox"
            checked={selected[idx] ?? false}
            disabled={!p.valid}
            onchange={(e) =>
              (selected = { ...selected, [idx]: (e.currentTarget as HTMLInputElement).checked })}
          />
          <span class="prop-body">
            <span class="prop-title">
              <span class="prop-eco">{p.ecosystem}</span>
              <code class="prop-name">{p.check.name}</code>
              <code class="prop-cmd">{p.check.cmd}</code>
            </span>
            <span class="prop-evidence">{p.evidence}</span>
            {#if !p.valid && p.reason}
              <span class="prop-reason">{p.reason}</span>
            {/if}
          </span>
        </label>
      {/each}
      <div class="proposals-actions">
        <button type="button" onclick={applyProposals} disabled={applying || proposals.length === 0}>
          {applying ? 'Applying…' : 'Apply selected'}
        </button>
        <button type="button" class="secondary" onclick={cancelDetect} disabled={applying}>
          Cancel
        </button>
      </div>
    </div>
  {/if}

  <!-- The configured checks. -->
  {#if rows.length === 0}
    <p class="empty">No checks configured. Add one, or use <strong>Detect &amp; configure</strong>.</p>
  {/if}

  {#each rows as row, i (i)}
    <section class="check-card">
      <div class="card-head">
        <input
          type="text"
          class="name"
          value={row.name}
          placeholder="name (run_check selects by this)"
          oninput={(e) => setName(i, (e.currentTarget as HTMLInputElement).value)}
          onchange={commit}
        />
        {#if row.auto}
          <span class="auto-tag" title="Created by auto-detection; editing makes it user-owned">
            auto
          </span>
        {/if}
        <button type="button" class="test" onclick={() => runTest(i)} disabled={testBusy[i]}>
          {testBusy[i] ? 'Testing…' : 'Test'}
        </button>
        <button type="button" class="remove" aria-label="Remove check" onclick={() => removeCheck(i)}>
          ×
        </button>
      </div>

      <label class="field">
        <span>Command</span>
        <input
          type="text"
          class="mono"
          value={row.cmd}
          placeholder="cargo check --message-format=json"
          oninput={(e) => setCmd(i, (e.currentTarget as HTMLInputElement).value)}
          onchange={commit}
        />
      </label>

      <div class="field-row">
        <label class="field">
          <span>Parser</span>
          <select
            value={row.parser}
            onchange={(e) => setParser(i, (e.currentTarget as HTMLSelectElement).value as ParserKind)}
          >
            {#each PARSER_KINDS as p (p)}
              <option value={p}>{PARSER_LABELS[p]}</option>
            {/each}
          </select>
        </label>
        <label class="field narrow">
          <span>Timeout (s)</span>
          <input
            type="number"
            min="10"
            value={row.timeout_secs}
            oninput={(e) => setTimeoutSecs(i, (e.currentTarget as HTMLInputElement).value)}
            onchange={commit}
          />
        </label>
      </div>

      <label class="field">
        <span>Working directory <small>(optional, relative to project root)</small></span>
        <input
          type="text"
          class="mono"
          value={row.cwd ?? ''}
          placeholder="src-tauri"
          oninput={(e) => setCwd(i, (e.currentTarget as HTMLInputElement).value)}
          onchange={commit}
        />
      </label>

      {#if showsReportFile(row.parser)}
        <label class="field">
          <span>Report file <small>(the tool writes this; parsed instead of stdout)</small></span>
          <input
            type="text"
            class="mono"
            value={row.report_file ?? ''}
            placeholder="target/surefire-reports/TEST-*.xml"
            oninput={(e) => setReportFile(i, (e.currentTarget as HTMLInputElement).value)}
            onchange={commit}
          />
        </label>
      {/if}

      {#if showsPattern(row.parser)}
        <label class="field">
          <span
            >Pattern <small>(named groups <code>file</code>, <code>line</code>,
              <code>message</code> required)</small
            ></span
          >
          <input
            type="text"
            class="mono"
            class:invalid={patternError[i]}
            value={row.pattern ?? ''}
            placeholder={'^(?<file>\\S+):(?<line>\\d+): (?<message>.+)$'}
            oninput={(e) => setPattern(i, (e.currentTarget as HTMLInputElement).value)}
            onchange={commit}
          />
          {#if patternError[i]}
            <span class="pattern-error">{patternError[i]}</span>
          {/if}
        </label>
      {/if}

      <div class="field">
        <span>Environment <small>(forced on the check's process)</small></span>
        <EnvEditor env={envRecord(row)} onchange={(next) => setEnv(i, next)} />
      </div>

      {#if testResults[i]}
        {@const r = testResults[i]}
        {@const v = classifyTestResult(r)}
        <div class="test-result {v}">
          <div class="tr-head">
            <span class="tr-badge">{verdictLabel(v)}</span>
            {#if r.error}
              <span class="tr-msg">{r.error}</span>
            {:else}
              <span class="tr-msg">
                {r.timed_out ? 'timed out' : `exit ${r.exit_code}`} · {r.diag_count} diagnostic{r.diag_count ===
                1
                  ? ''
                  : 's'} · {r.duration_ms} ms
              </span>
            {/if}
          </div>
          {#if v === 'wrong-parser'}
            <p class="tr-hint">
              The command produced output but this parser matched no diagnostics — likely the wrong
              parser for its output shape.
            </p>
          {/if}
          {#if r.diagnostics.length > 0}
            <ul class="tr-diags">
              {#each r.diagnostics as d, di (di)}
                <li>
                  <span class="tr-sev {d.severity}">{d.severity}</span>
                  <span class="tr-dmsg">{d.message}</span>
                  {#if d.sites.length > 0}<span class="tr-sites">{d.sites.join(', ')}</span>{/if}
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      {/if}
    </section>
  {/each}

  <button type="button" class="add" onclick={addCheck}>+ Add check</button>
</div>

<style>
  .checks-editor {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  /* Exposure status line. */
  .exposure {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-md);
    border: 1px solid var(--border-default);
    background: var(--surface-2);
    font-size: var(--font-size-sm);
  }
  .exposure .exp-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
    background: var(--text-tertiary);
  }
  .exposure.on {
    border-color: var(--border-success-strong, var(--border-default));
  }
  .exposure.on .exp-dot {
    background: var(--accent-success, #3fb950);
  }
  .exposure.on strong {
    color: var(--accent-success, #3fb950);
  }
  .exp-text {
    color: var(--text-primary);
  }
  .toolbar {
    display: flex;
    gap: var(--space-2);
  }
  .detect-btn {
    margin-left: auto;
    background: var(--surface-input);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: var(--space-1) 10px;
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--font-size-xs);
  }
  .detect-btn:hover:not(:disabled) {
    border-color: var(--accent);
  }
  .detect-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .toolbar .detect-btn {
    margin-left: 0;
  }
  .detect-error {
    color: var(--text-danger-pale, #f85149);
    font-size: var(--font-size-sm);
    margin: 0;
  }

  /* Proposal picker. */
  .proposals {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    border: 1px dashed var(--border-default);
    border-radius: var(--radius-md);
    background: var(--surface-sunken);
  }
  .proposals-head {
    font-size: var(--font-size-sm);
    color: var(--text-quiet-strong);
  }
  .proposal {
    display: flex;
    gap: var(--space-2);
    align-items: flex-start;
    padding: var(--space-1) 0;
  }
  .proposal.invalid {
    opacity: 0.55;
  }
  .prop-body {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .prop-title {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
    flex-wrap: wrap;
  }
  .prop-eco {
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .prop-name {
    color: var(--text-primary);
    font-weight: 600;
  }
  .prop-cmd {
    color: var(--text-quiet-strong);
    font-size: var(--font-size-xs);
    word-break: break-all;
  }
  .prop-evidence {
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
  }
  .prop-reason {
    font-size: var(--font-size-xs);
    color: var(--text-danger-pale, #f85149);
  }
  .proposals-actions {
    display: flex;
    gap: var(--space-2);
    margin-top: var(--space-1);
  }

  .empty {
    color: var(--text-tertiary);
    font-size: var(--font-size-sm);
    margin: 0;
  }

  /* One check. */
  .check-card {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-md);
    background: var(--surface-2);
  }
  .card-head {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .card-head .name {
    flex: 1;
    font-weight: 600;
  }
  .auto-tag {
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-sm);
    padding: 1px 6px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .card-head .test {
    background: var(--surface-input);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: var(--space-1) 10px;
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--font-size-xs);
  }
  .card-head .test:hover:not(:disabled) {
    border-color: var(--accent);
  }
  .card-head .test:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .remove {
    width: 28px;
    padding: 0;
    line-height: 24px;
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-quiet-strong);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: 16px;
  }
  .remove:hover {
    background: var(--surface-danger-bg);
    color: var(--text-danger-pale);
    border-color: var(--border-danger-strong);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .field > span {
    font-size: var(--font-size-sm);
    color: var(--text-quiet-strong);
  }
  .field small {
    color: var(--text-tertiary);
    font-weight: 400;
  }
  .field-row {
    display: flex;
    gap: var(--space-2);
  }
  .field-row .field {
    flex: 1;
  }
  .field.narrow {
    flex: 0 0 120px;
  }
  .checks-editor input,
  .checks-editor select {
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    min-width: 0;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .checks-editor input.mono {
    font-family: monospace;
  }
  .checks-editor input:focus,
  .checks-editor select:focus {
    outline: none;
    border-color: var(--accent);
  }
  .checks-editor input.invalid {
    border-color: var(--border-danger-strong, #f85149);
  }
  .pattern-error {
    color: var(--text-danger-pale, #f85149);
    font-size: var(--font-size-xs);
  }

  /* Test result panel. */
  .test-result {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: var(--space-2);
    border-radius: var(--radius-md);
    border: 1px solid var(--border-default);
    background: var(--surface-sunken);
  }
  .test-result.wrong-parser,
  .test-result.error {
    border-color: var(--border-warning-strong, #d29922);
  }
  .tr-head {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
    flex-wrap: wrap;
  }
  .tr-badge {
    font-size: var(--font-size-xs);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    padding: 1px 6px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    color: var(--text-quiet-strong);
  }
  .test-result.diagnostics .tr-badge {
    color: var(--accent);
  }
  .test-result.clean .tr-badge {
    color: var(--accent-success, #3fb950);
  }
  .test-result.wrong-parser .tr-badge,
  .test-result.timeout .tr-badge,
  .test-result.error .tr-badge {
    color: var(--text-warning, #d29922);
  }
  .tr-msg {
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
  }
  .tr-hint {
    margin: 0;
    font-size: var(--font-size-xs);
    color: var(--text-warning, #d29922);
  }
  .tr-diags {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .tr-diags li {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
    font-size: var(--font-size-xs);
    min-width: 0;
  }
  .tr-sev {
    text-transform: uppercase;
    letter-spacing: 0.03em;
    flex: none;
  }
  .tr-sev.error {
    color: var(--text-danger-pale, #f85149);
  }
  .tr-sev.warning {
    color: var(--text-warning, #d29922);
  }
  .tr-sev.note {
    color: var(--text-tertiary);
  }
  .tr-dmsg {
    color: var(--text-primary);
    white-space: pre-wrap;
    word-break: break-word;
    min-width: 0;
  }
  .tr-sites {
    color: var(--text-tertiary);
    font-family: monospace;
    flex: none;
  }

  .add {
    align-self: flex-start;
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-quiet-strong);
    padding: var(--space-1) 10px;
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--font-size-xs);
  }
  .add:hover {
    background: var(--surface-input);
    color: var(--text-primary);
  }
</style>
