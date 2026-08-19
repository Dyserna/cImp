<script lang="ts">
  // V37 Phase D (contract C8) — the MCP management editor: the whole of
  // Settings' "MCP servers" section below its heading. Extracted out of the
  // 9k-line `SettingsApp.svelte` the way `ChecksEditor.svelte` was, with the
  // decision logic (the effective-enable mirror, grouping, naming, the override
  // tri-state, stale-reference detection) in `mcpEditor.ts` so it is unit
  // tested without a component host. This file stays presentational.
  //
  // ## Two scopes, deliberately drawn differently
  //
  // * The **registry** — server rows (including each server's global `enabled`)
  //   and the category list — is GLOBAL. Editing it changes every project.
  // * The **activation maps** are the only per-project surface (contract C2).
  //   Each is a tri-state: inherit / force on / force off, where *inherit is the
  //   absence of the key*. Reverting deletes the key so the project keeps
  //   following the global flag; writing the current global value back would
  //   freeze the project at today's value and silently stop inheriting.
  //
  // Anything that reads as "this project" in the UI writes ONLY into
  // `activation`; anything that reads as global writes only into `servers` /
  // `categories`. Keeping those two visually distinct is this component's job.
  //
  // ## Enforcement lives in Rust
  //
  // `offload::mcp_host::effective_enable` decides what is advertised and what a
  // call is refused for. The chips here mirror it (see `mcpEditor.ts`) so the
  // user can see the state the backend will act on — they never *are* it.
  import type { McpActivation, McpCategory, McpServerConfig } from './types';
  import { describeMcpServerHealth, type McpServerHealth } from '../offload';
  import {
    clearStaleRefs,
    cloneRegistry,
    describeVerdict,
    enabledCount,
    groupServers,
    hasStaleRefs,
    newCategory,
    newServer,
    originLabel,
    overrideState,
    resolveCategoryName,
    staleRefs,
    withMembership,
    withOverride,
    type McpRegistry,
    type OverrideState,
  } from './mcpEditor';

  let {
    servers,
    categories,
    activation,
    health = [],
    healthIntervalSecs = 60,
    onedit,
    onapply,
    onhealthinterval,
  }: {
    servers: McpServerConfig[];
    categories: McpCategory[];
    activation: McpActivation;
    /// Live health of the warm host's connections, straight from
    /// `ServiceStatus.mcp_servers`. Disabled servers are not connected, so they
    /// simply have no row here — that is the truth, not a gap.
    health?: McpServerHealth[];
    /// V37 contract C6: the health checker's cadence in seconds, `0` = off.
    /// Global (it is a property of cImp's polling, not of a project), and NOT
    /// part of `McpRegistry` — it changes no server's effective state, so it
    /// must not travel through `onapply`, whose whole contract is one settings
    /// write plus one host reconcile.
    healthIntervalSecs?: number;
    /// Update the parent's local snapshot ONLY — no backend write. Used for
    /// keystrokes: persisting per keystroke raced, because fire-and-forget
    /// saves could land out of order and leave the backend holding a half-typed
    /// URL that the health watch would then flag as down.
    onedit: (next: McpRegistry) => void;
    /// Persist AND reconcile the warm host: exactly one `settings_update` plus
    /// one `offload_reload_mcp`. Every toggle, add, remove and field commit goes
    /// through here, and each is ONE call no matter how many servers it affects
    /// — contract C5's UI half.
    onapply: (next: McpRegistry) => Promise<void>;
    /// Persist the cadence. Separate from `onapply` because reconciling the warm
    /// host over a polling-interval edit would tear down and rebuild every
    /// connection to change a number the checker re-reads each tick anyway.
    onhealthinterval: (secs: number) => void;
  } = $props();

  const registry = $derived<McpRegistry>({ servers, categories, activation });
  const groups = $derived(groupServers(registry));
  const stale = $derived(staleRefs(registry));
  const healthByName = $derived(new Map(health.map((h) => [h.name, h])));
  const activeCount = $derived(enabledCount(registry));

  /// Apply `mutate` to a private copy of the registry. Hand-cloned rather than
  /// `structuredClone`d because these values arrive as Svelte `$state` proxies.
  function edited(mutate: (r: McpRegistry) => void): McpRegistry {
    const next = cloneRegistry(registry);
    mutate(next);
    return next;
  }
  /// Local-only edit (keystrokes). See the `onedit` doc above.
  function local(mutate: (r: McpRegistry) => void): void {
    onedit(edited(mutate));
  }
  /// Persist + reconcile. ONE save, ONE reload — see the `onapply` doc above.
  async function apply(mutate: (r: McpRegistry) => void): Promise<void> {
    await onapply(edited(mutate));
  }
  /// Blur/Enter handler for the text fields: the snapshot already holds the
  /// typed value (via `local`), so this just persists it and reloads.
  async function commit(): Promise<void> {
    await apply(() => {});
  }

  function overrideLabel(globallyOn: boolean): string {
    return `Inherit (global: ${globallyOn ? 'on' : 'off'})`;
  }
  function readOverride(e: Event): OverrideState {
    return (e.currentTarget as HTMLSelectElement).value as OverrideState;
  }
</script>

<div class="mcp-editor">
  <h3>Server status</h3>
  <small class="hint top">
    Live health of the warm MCP host's connections. Updates as you add, remove,
    enable or disable servers below — no cImp restart needed. A disabled server
    is not connected at all, so it has no health row.
  </small>
  <!-- V37 contract C6. Next to the chips because it is the thing that MOVES
       them: a reader wondering why a chip is stale is looking at the answer.
       Committed on change rather than per keystroke, and never routed through
       `onapply` — see `onhealthinterval`. -->
  <label class="mcp-cadence">
    <span>Health check every</span>
    <input
      type="number"
      min="0"
      max="3600"
      step="5"
      value={healthIntervalSecs}
      onchange={(e) => {
        const n = Number((e.currentTarget as HTMLInputElement).value);
        onhealthinterval(Number.isFinite(n) && n >= 0 ? Math.round(n) : 60);
      }}
    />
    <span>seconds</span>
  </label>
  <small class="hint">
    How often cImp probes each connected server — a process check for stdio, a
    small <code>tools/list</code> for HTTP. Two consecutive failures mark a
    server unhealthy and withdraw its tools; the next success brings them back,
    and both write a row in the Events feed. <strong>0 turns the checker off</strong>,
    and values are clamped to 5–3600 seconds.
  </small>
  {#if servers.length === 0}
    <small class="hint">No MCP servers configured yet.</small>
  {:else}
    <small class="hint">
      {activeCount} of {servers.length} configured server(s) active in this
      project{health.length === 0
        ? ' — health appears once the warm MCP host is running (it starts when offload is enabled or any server is exposed to Claude Code).'
        : '.'}
    </small>
  {/if}

  {#if hasStaleRefs(stale)}
    <!-- Contract C1 makes a rename a NEW identity, so renaming a server or a
         category leaves references behind that match nothing. Rust treats them
         as inert, which is right — but inert is not invisible: a user whose
         project override quietly stopped applying needs to see why. Surfaced
         with one action; never pruned behind the user's back. -->
    <div class="mcp-stale">
      <strong>Unresolved references</strong>
      <small class="hint">
        These name a server or category that no longer exists — a rename creates
        a new identity, and the old name is left behind doing nothing. They are
        harmless, but they are also not doing what they look like they do.
      </small>
      <ul>
        {#each stale.members as m (m.category)}
          <li>
            Category <code>{m.category}</code> lists missing server(s):
            <code>{m.servers.join(', ')}</code>
          </li>
        {/each}
        {#each stale.activationCategories as name (name)}
          <li>This project overrides a missing category: <code>{name}</code></li>
        {/each}
        {#each stale.activationServers as name (name)}
          <li>This project overrides a missing server: <code>{name}</code></li>
        {/each}
      </ul>
      <div class="button-row">
        <button type="button" class="secondary" onclick={() => apply((r) => {
          const cleaned = clearStaleRefs(r);
          r.categories = cleaned.categories;
          r.activation = cleaned.activation;
        })}>
          Clear unresolved references
        </button>
      </div>
    </div>
  {/if}

  <h3>Categories</h3>
  <small class="hint top">
    Groups over the servers below, so one switch can turn a whole set of tools
    on or off. A server with no category rides its own toggle alone; a server in
    several categories stays available while <em>any</em> of them is on, and is
    off when they are all off. Categories and their names are global — the name
    is the identity, so renaming one is a new category and any project override
    keyed by the old name stops applying (it is then listed above).
  </small>
  {#if categories.length === 0}
    <small class="hint">No categories yet — every server rides its own toggle.</small>
  {/if}
  <!-- Keyed by index for the same reason the server rows are: the name is an
       editable text field, so a name key would change mid-typing and drop
       input focus. -->
  {#each categories as cat, ci (ci)}
    <div class="mcp-cat-row">
      <div class="mcp-line">
        <label class="mcp-field grow">
          <span>Category name (global)</span>
          <input
            type="text"
            placeholder="research"
            value={cat.name}
            oninput={(e) =>
              local((r) => (r.categories[ci].name = (e.currentTarget as HTMLInputElement).value))}
            onchange={() =>
              apply((r) => (r.categories[ci].name = resolveCategoryName(r.categories[ci].name, r.categories, ci)))}
          />
        </label>
        <button type="button" class="secondary danger" onclick={() => apply((r) => {
          r.categories = r.categories.filter((_, i) => i !== ci);
        })}>
          Delete
        </button>
      </div>
      <div class="mcp-enable-row">
        <!-- ONE settings write + ONE host reload for the whole category,
             however many servers it holds (contract C5's UI half). The member
             servers' own toggles are deliberately NOT touched: the effective
             state is derived, so cascading would both multiply writes and
             destroy the user's per-server intent. -->
        <label class="mcp-enable" title="Global on/off for every server in this category">
          <input
            type="checkbox"
            checked={cat.enabled}
            onchange={(e) =>
              apply((r) => (r.categories[ci].enabled = (e.currentTarget as HTMLInputElement).checked))}
          />
          <span>Enabled (global)</span>
        </label>
        <label class="inline-override">
          <span>This project</span>
          <select
            value={overrideState(activation.categories, cat.name)}
            onchange={(e) => {
              const state = readOverride(e);
              apply((r) => {
                r.activation.categories = withOverride(r.activation.categories, cat.name, state);
              });
            }}
          >
            <option value="inherit">{overrideLabel(cat.enabled)}</option>
            <option value="on">Force on (this project)</option>
            <option value="off">Force off (this project)</option>
          </select>
        </label>
        {#if overrideState(activation.categories, cat.name) !== 'inherit'}
          <span class="mcp-badge override">overridden here</span>
        {/if}
        <span class="mcp-count">{cat.servers.length} server(s)</span>
      </div>
    </div>
  {/each}
  <div class="button-row mcp-add">
    <button type="button" onclick={() => apply((r) => {
      r.categories = [...r.categories, newCategory(r.categories)];
    })}>
      Add category
    </button>
  </div>

  <h3>Tool servers</h3>
  <small class="hint top">
    Add an HTTP MCP endpoint by name + URL. cImp's warm MCP host aggregates the
    read-class tools from these servers and keeps the connections warm;
    write/destructive tools are filtered out. <strong>Every switch on this page
    — the category and server toggles, and the per-harness access boxes —
    applies to tabs you already have open.</strong> <strong>OpenCode</strong>
    refreshes its tool list in the same session, <strong>Claude Code</strong>
    picks the new surface up on its next turn, and restarting the tab
    (Tabs → Restart) is only a fallback if one still shows a stale list. A call
    that reaches a disabled server is refused with a message naming which toggle
    did it, so a stale tool list can never quietly do the thing you turned off.
    Advanced stdio servers (command/args/env) remain editable in
    <code>settings.json</code> under <code>offload.mcp_servers</code>.
  </small>
  {#each groups as group, gi (gi)}
    {#if group.category !== null || group.rows.length > 0 || groups.length === 1}
      <div class="mcp-group">
        <h4>
          {group.category ? group.category.name : 'Uncategorized'}
          {#if group.category}
            <span class="mcp-badge" class:off={!group.category.enabled}>
              {group.category.enabled ? 'category on' : 'category off'}
            </span>
          {/if}
        </h4>
        {#if group.rows.length === 0}
          <small class="hint">No servers in this category yet.</small>
        {/if}
        <!-- Keyed by the server's array index deliberately: name/url are
             editable and the snapshot is replaced (cloned) on every edit, so a
             name or object key would change mid-edit and drop input focus.
             Inputs are controlled (`value={…}`), so values always track the data
             after a removal/reorder, and removal is button-triggered (no focused
             text field to bleed) — the index-key caveat is harmless here. -->
        {#each group.rows as row (row.index)}
          {@const i = row.index}
          {@const srv = row.server}
          {@const h = healthByName.get(srv.name)}
          <div class="mcp-row">
            <div class="mcp-line">
              <label class="mcp-field grow">
                <span>Name</span>
                <input
                  type="text"
                  placeholder="duckduckgo"
                  value={srv.name}
                  oninput={(e) =>
                    local((r) => (r.servers[i].name = (e.currentTarget as HTMLInputElement).value.trim()))}
                  onchange={commit}
                />
              </label>
              <button type="button" class="secondary danger" onclick={() => apply((r) => {
                r.servers = r.servers.filter((_, idx) => idx !== i);
              })}>
                Remove
              </button>
            </div>
            <div class="mcp-chips">
              <span class="mcp-badge origin">{originLabel(srv.origin)}</span>
              <span class="mcp-badge" class:off={row.verdict.kind !== 'enabled'}>
                {describeVerdict(row.verdict)}
              </span>
              {#if row.verdict.kind !== row.globalVerdict.kind}
                <span class="mcp-badge override" title="This project differs from the global registry">
                  globally: {describeVerdict(row.globalVerdict)}
                </span>
              {/if}
              {#if h}
                <span class="mcp-badge health" class:healthy={h.healthy} class:down={!h.healthy}>
                  {describeMcpServerHealth(h)}
                </span>
              {:else if row.verdict.kind === 'enabled'}
                <span class="mcp-badge">no health yet</span>
              {/if}
              {#if row.categories.length > 1}
                <span class="mcp-count">also in: {row.categories.slice(1).join(', ')}</span>
              {/if}
            </div>
            <label class="mcp-field">
              <span>URL</span>
              <input
                type="text"
                placeholder="http://host:port/mcp"
                value={srv.url}
                oninput={(e) =>
                  local((r) => (r.servers[i].url = (e.currentTarget as HTMLInputElement).value.trim()))}
                onchange={commit}
              />
            </label>
            <!-- V33: the token belongs on this row, not in settings.json with
                 command/args/env, because it is an HTTP concern — it becomes an
                 `Authorization` header on the URL above, and this editor IS the
                 HTTP editor. stdio servers pass secrets through `env` and are
                 unaffected. Same commit path as name/url: keystrokes update the
                 snapshot only, `onchange` persists AND reloads the warm host —
                 the token is part of the Rust `config_sig`, so without that
                 reload an edit here would never reconnect. -->
            <label class="mcp-field">
              <span>Auth token (HTTP, optional)</span>
              <input
                type="password"
                placeholder="Bearer token — leave empty for no auth"
                value={srv.auth_token ?? ''}
                oninput={(e) =>
                  local((r) => (r.servers[i].auth_token = (e.currentTarget as HTMLInputElement).value))}
                onchange={commit}
              />
            </label>
            <div class="mcp-enable-row">
              <label class="mcp-enable" title="Does this server exist at all — globally, for every project">
                <input
                  type="checkbox"
                  checked={srv.enabled}
                  onchange={(e) =>
                    apply((r) => (r.servers[i].enabled = (e.currentTarget as HTMLInputElement).checked))}
                />
                <span>Enabled (global)</span>
              </label>
              <label class="inline-override">
                <span>This project</span>
                <select
                  value={overrideState(activation.servers, srv.name)}
                  onchange={(e) => {
                    const state = readOverride(e);
                    apply((r) => {
                      r.activation.servers = withOverride(r.activation.servers, srv.name, state);
                    });
                  }}
                >
                  <option value="inherit">{overrideLabel(srv.enabled)}</option>
                  <option value="on">Force on (this project)</option>
                  <option value="off">Force off (this project)</option>
                </select>
              </label>
              {#if overrideState(activation.servers, srv.name) !== 'inherit'}
                <span class="mcp-badge override">overridden here</span>
              {/if}
            </div>
            {#if categories.length > 0}
              <!-- Membership is edited on the SERVER row, not on the category:
                   a server can belong to several categories, and this is where
                   the user is looking at the one they mean. Global, like the
                   category list itself. -->
              <div class="mcp-enable-row">
                <span class="mcp-count">Categories (global):</span>
                {#each categories as cat (cat.name)}
                  <label class="mcp-enable" title={`Put ${srv.name} in ${cat.name}`}>
                    <input
                      type="checkbox"
                      checked={cat.servers.includes(srv.name)}
                      onchange={(e) => {
                        const member = (e.currentTarget as HTMLInputElement).checked;
                        apply((r) => {
                          r.categories = withMembership(r.categories, cat.name, srv.name, member);
                        });
                      }}
                    />
                    <span>{cat.name}</span>
                  </label>
                {/each}
              </div>
            {/if}
            <!-- Exposure, NOT enablement: `*_access` says who may see this
                 server, `enabled` says whether it exists. They are deliberately
                 orthogonal and the access boxes do not follow the toggles — a
                 decision record, not an oversight. One save + one reload per
                 box, as before.

                 V37 Phase F: these boxes are no longer spawn-baked. The
                 `cimp-offload` proxy child rides every AI tab whether or not
                 anything is granted, so a box ticked here reaches a tab that is
                 already open — which is why the hint below says so instead of
                 telling the user to open a fresh one. -->
            <div class="mcp-enable-row">
              <label class="mcp-enable" title="Expose this server's tools to Claude Code">
                <input
                  type="checkbox"
                  checked={srv.claude_access}
                  onchange={(e) =>
                    apply((r) => (r.servers[i].claude_access = (e.currentTarget as HTMLInputElement).checked))}
                />
                <span>Claude Code</span>
              </label>
              <label class="mcp-enable" title="Expose this server's tools to the offload worker">
                <input
                  type="checkbox"
                  checked={srv.offload_access}
                  onchange={(e) =>
                    apply((r) => (r.servers[i].offload_access = (e.currentTarget as HTMLInputElement).checked))}
                />
                <span>Offload</span>
              </label>
              <label class="mcp-enable" title="Expose this server's tools to OpenCode">
                <input
                  type="checkbox"
                  checked={srv.opencode_access}
                  onchange={(e) =>
                    apply((r) => (r.servers[i].opencode_access = (e.currentTarget as HTMLInputElement).checked))}
                />
                <span>OpenCode</span>
              </label>
            </div>
            <small class="hint mcp-access-note">
              Applies to open tabs: OpenCode refreshes in the same session,
              Claude Code on its next turn.
            </small>
          </div>
        {/each}
      </div>
    {/if}
  {/each}
  <div class="button-row mcp-add">
    <button type="button" onclick={() => local((r) => {
      // No reload: a brand-new row has an empty URL, so there is nothing to
      // connect to yet. It persists with the first field commit.
      r.servers = [...r.servers, newServer(r.servers)];
    })}>
      Add MCP server
    </button>
  </div>
</div>

<style>
  /* Base look for the section chrome this component renders. `SettingsApp`'s
     equivalents are scoped to that file, so an extracted component has to carry
     its own copies — these are lifted verbatim from there so the section reads
     identically either side of the extraction. Element-level selectors
     deliberately (the lowest scoped specificity), so a TUI theme's flat
     `html[data-theme] section button` reset still outranks them; a class
     selector here would win instead and draw a box AROUND the theme's brackets.
     This is the same reasoning `ChecksEditor.svelte` records. */
  h3 {
    font-size: var(--font-size-md);
    font-weight: 600;
    margin: var(--space-5) 0 var(--space-2) 0;
    padding-top: var(--space-3);
    border-top: 1px solid var(--border-faint);
    color: var(--text-primary);
  }
  label {
    display: block;
    margin-bottom: var(--space-3);
  }
  label > span:first-child {
    display: block;
    margin-bottom: var(--space-1);
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
  }
  input[type='text'],
  input[type='password'],
  input[type='number'],
  select {
    width: 100%;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: inherit;
    font-size: var(--font-size-md);
    box-sizing: border-box;
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  input[type='text']:focus,
  input[type='password']:focus,
  input[type='number']:focus,
  select:focus {
    outline: none;
    border-color: var(--accent);
  }
  button {
    background: var(--surface-2);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-3);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  button:hover:not(:disabled) {
    background: var(--surface-input);
    border-color: var(--border-strong);
  }
  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  button.danger {
    color: var(--text-danger-bright);
    border-color: var(--border-danger);
  }
  button.danger:hover:not(:disabled) {
    background: var(--surface-danger-soft);
    border-color: var(--border-danger-strong);
  }
  .button-row {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.4rem;
    flex-wrap: wrap;
  }
  /* The cadence row: label, number, unit on one line, so it reads as a
     sentence rather than as a form. */
  .mcp-cadence {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin: 0.35rem 0 0.15rem;
    font-size: 0.85rem;
  }
  .mcp-cadence input {
    width: 5rem;
  }
  small.hint {
    display: block;
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    line-height: 1.6;
    margin: -8px 0 var(--space-3) 0;
  }
  small.hint code {
    font-family: Consolas, Menlo, monospace;
    font-size: 0.95em;
    line-height: 1;
  }
  /* A hint placed directly under an h3 has no preceding label for the negative
     top margin to tuck under. */
  small.hint.top {
    margin-top: 0;
    margin-bottom: var(--space-3);
  }
  .mcp-editor {
    display: flex;
    flex-direction: column;
  }
  /* Editable MCP server groups: stacked lines per server — name + remove,
     chips, full-width URL/token, then the toggle and access rows. */
  .mcp-row,
  .mcp-cat-row {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    margin-top: 0.4rem;
  }
  .mcp-row + .mcp-row {
    /* Two blank-line-ish breathing room between server info groups. */
    margin-top: 1.5rem;
  }
  .mcp-cat-row + .mcp-cat-row {
    margin-top: 1rem;
  }
  .button-row.mcp-add {
    margin-top: 1.5rem;
  }
  .mcp-line {
    display: flex;
    align-items: flex-end;
    gap: 0.5rem;
  }
  .mcp-access-note {
    /* Sits directly under the access boxes it explains, indented with them so
       it reads as part of that row rather than as a new subsection. */
    display: block;
    margin-top: 0.15rem;
  }

  .mcp-enable-row,
  .mcp-chips {
    display: flex;
    align-items: center;
    gap: 1rem;
    flex-wrap: wrap;
  }
  .mcp-chips {
    gap: 0.4rem;
  }
  .mcp-field {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    /* Cancel the global `label` bottom margin: the .mcp-row column gap owns
       line spacing, and the stray margin misaligns the Remove button. */
    margin-bottom: 0;
  }
  .mcp-field.grow {
    flex: 1 1 16rem;
  }
  .mcp-field input {
    width: 100%;
  }
  .mcp-enable {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    white-space: nowrap;
    margin-bottom: 0;
  }
  .mcp-enable input {
    width: auto;
  }
  /* One per-scope override cell, laid out inline so a row's global switch and
     its project override read as one line rather than a column of selects
     (the shape the V32 per-scope override cells use). */
  .inline-override {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    font-size: var(--font-size-xs);
    color: var(--text-secondary);
    margin-bottom: 0;
  }
  .inline-override select {
    font-size: var(--font-size-xs);
    padding: 1px 4px;
  }
  /* Status chips: effective state, provenance, health. Neutral by default —
     colour is reserved for the two states the user may need to act on. */
  .mcp-badge {
    font-size: var(--font-size-xs);
    padding: 1px 6px;
    border-radius: var(--radius-sm, 4px);
    border: 1px solid var(--border-subtle);
    color: var(--text-secondary);
    white-space: nowrap;
  }
  .mcp-badge.off {
    color: var(--danger, #d08770);
    border-color: var(--danger, #d08770);
  }
  .mcp-badge.health.healthy {
    color: var(--success, #3fb950);
    border-color: var(--success, #3fb950);
  }
  .mcp-badge.health.down {
    color: var(--danger, #d08770);
    border-color: var(--danger, #d08770);
  }
  .mcp-badge.override {
    /* The one chip that says "this project differs" — accented rather than
       coloured by severity, because an override is neither good nor bad. */
    color: var(--accent, #58a6ff);
    border-color: var(--accent, #58a6ff);
  }
  .mcp-count {
    font-size: var(--font-size-xs);
    color: var(--text-secondary);
  }
  .mcp-group {
    margin-top: 0.6rem;
  }
  .mcp-stale {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    margin: 0.6rem 0;
    padding: var(--space-2);
    border: 1px solid var(--danger, #d08770);
    border-radius: var(--radius-sm, 4px);
  }
  .mcp-stale ul {
    margin: 0;
    padding-left: 1.2rem;
    font-size: var(--font-size-sm);
  }
  h4 {
    font-size: var(--font-size-sm);
    font-weight: 600;
    margin: var(--space-4) 0 var(--space-1) 0;
    color: var(--text-primary);
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
</style>
