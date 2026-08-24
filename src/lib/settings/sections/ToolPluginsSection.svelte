<script lang="ts">
  /// Settings → Tool Plugins (#129 (c)) — V38's master-detail over
  /// `plugins_snapshot`.
  ///
  /// **What stays with the parent, and why.** The loader's set is read once on
  /// mount (`plugins_snapshot`, a read of already-scanned state, never a disk
  /// walk) together with the project key, and the issue's behaviour invariant
  /// keeps that eager: both arrive as props. So does `onmanualtooledit`, which
  /// reads the audit census the parent loads for the Code Audit section — the
  /// census has one owner, not two.
  ///
  /// **What is local here.** The selection, the four derived row views, the
  /// per-tool Detect results, and the three small helpers that write through
  /// `patch`. None of them is read anywhere else in the window.
  ///
  /// Every write goes through `patch()` — the clone-mutate-push contract, no
  /// `bind:`. `lib/settings/toolPlugins.ts` builds the rows and owns every
  /// write shape; this component decides nothing.
  import { harnesses } from '../../harness';
  import { auditDetectTool } from '../ipc';
  import { formatDetect } from '../codeAudit';
  import { pickFile, EXECUTABLE_EXTENSIONS } from '../pickFile';
  import { harnessRow, type AuditDetectResult, type Settings } from '../types';
  import {
    errorRows,
    permissionsOpen,
    pluginDisplayLabels,
    pluginRows,
    revertToGlobalPath,
    setCategoryEnabled,
    setGlobalPath,
    setPluginEnabled,
    setProjectPath,
    setToolEnabled,
    setToolParameters,
    setToolTimeout,
    setToolVariable,
    shouldAutoFill,
    siblingAutoFillTargets,
    type PluginRow,
    type PluginSet,
    type ToolRow,
  } from '../toolPlugins';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    pluginSet,
    pluginProjectKey,
    rescanning,
    loadError,
    onrescan,
    onmanualtooledit,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// The loader's set, read eagerly by the parent on mount.
    pluginSet: PluginSet | null;
    /// The key this project's per-tool path overrides are stored under. Asked
    /// of the backend rather than derived: canonicalizing a path touches the
    /// disk, and a second spelling rule would silently stop matching the first.
    pluginProjectKey: string;
    /// A Rescan is in flight.
    rescanning: boolean;
    /// The plugins folder could not be read.
    loadError: string | null;
    /// Ask the parent for a fresh scan — the only thing that re-reads the
    /// folder.
    onrescan: () => void;
    /// Report that the user edited a tool's checkbox by hand. The parent
    /// decides whether that switches quality auto-selection to manual, because
    /// the census that answers it is the parent's.
    onmanualtooledit: (pluginKey: string, toolId: string) => void;
  } = $props();

  /// Which plugin the detail pane shows. Purely a view concern.
  let pluginSelected = $state<string | null>(null);

  /// Per-tool Detect probe result, keyed by TOOL KEY (`cimp-audit@1/gitleaks`,
  /// or a user plugin's `name@version/tool-id`). `'probing'` while the IPC is
  /// in flight. The IPC itself writes no settings; when it answers a click on
  /// an EMPTY path box, this component stores what it found — see
  /// `detectPluginTool`.
  let auditDetect = $state<Record<string, AuditDetectResult | 'probing' | undefined>>({});

  const pluginList = $derived<PluginRow[]>(
    pluginSet ? pluginRows(pluginSet, snapshot, pluginProjectKey) : [],
  );
  const pluginErrors = $derived(pluginSet ? errorRows(pluginSet) : []);
  // Keep the selection valid across a Rescan that removed the selected plugin,
  // and land on the first one so the pane is never a blank right-hand side.
  const pluginActive = $derived<PluginRow | null>(
    pluginList.find((p) => p.key === pluginSelected) ?? pluginList[0] ?? null,
  );
  // What the LIST prints per key: the bare name, and the version only for rows
  // that would otherwise read identically (decision 9's collision case). The
  // detail pane always shows the version, so nothing is hidden — it is just not
  // repeated on every line of a list of names.
  const pluginLabels = $derived(pluginDisplayLabels(pluginList));

  function patchPlugin(updater: (s: Settings) => void): void {
    patch(updater);
  }

  /// A number input that means "no override" when blank. Shared by the timeout
  /// field so an unparseable keystroke reverts to inherit rather than to 0.
  function optionalSeconds(raw: string): number | null {
    const v = Number(raw.trim());
    return raw.trim() !== '' && Number.isFinite(v) && v >= 1 ? Math.floor(v) : null;
  }

  async function pickToolBinary(toolKey: string, scope: 'global' | 'project'): Promise<void> {
    const p = await pickFile('Executable', EXECUTABLE_EXTENSIONS);
    if (!p) return;
    patchPlugin((s) =>
      scope === 'global'
        ? setGlobalPath(s, toolKey, p)
        : setProjectPath(s, pluginProjectKey, toolKey, p),
    );
  }

  /// Detect: probe one tool, and — when the box was empty — SELECT what was
  /// found, for this tool and for the siblings of the same plugin that resolve
  /// to the same binary.
  ///
  /// Pressing Detect on an empty box is a question with one useful answer
  /// ("here it is, and it is now yours"): reporting a path the user then has to
  /// retype is the button doing nine tenths of the work and stopping. The write
  /// is deliberately HERE rather than in the IPC — `audit_detect_tool` is
  /// settings-read-only, so a probe can never change what a scan launches on its
  /// own; this is the user's click storing a path exactly as a Browse… would.
  ///
  /// A non-empty box is left alone: the probe was then asking about a path the
  /// user chose, and confirming it is not a reason to rewrite it.
  ///
  /// And a row `shouldAutoFill` refuses is left alone too, however empty its
  /// box is. A built-in that resolves by name is SUPPOSED to have an empty box —
  /// it finds its binary through `ebin` then `PATH` on every run, which is what
  /// its placeholder promises — so storing today's hit would quietly turn that
  /// live lookup into a pin and make the next drop-in update of the binary
  /// invisible. The probe result is displayed either way; only the write is
  /// refused, and per row: a sibling that does need a path still gets one.
  async function detectPluginTool(plugin: PluginRow, tool: ToolRow): Promise<void> {
    const toolKey = tool.toolKey;
    const path = tool.path.effective;
    // Read off the rows as they are NOW, before the await: the probe answers
    // milliseconds later against a settings snapshot that may have moved, and
    // "the rows the user was looking at when they clicked" is the honest
    // population to fill.
    const siblings = path.trim() === '' ? siblingAutoFillTargets(plugin, tool) : [];
    const fillClicked = shouldAutoFill(tool);
    auditDetect = { ...auditDetect, [toolKey]: 'probing' };
    try {
      // Probe the LIVE editing value, not the persisted setting — a just-typed
      // path would otherwise race the fire-and-forget applySettings push.
      const r = await auditDetectTool(toolKey, path);
      auditDetect = { ...auditDetect, [toolKey]: r };
      const targets = [...(fillClicked ? [toolKey] : []), ...siblings.map((s) => s.toolKey)];
      if (path.trim() === '' && r.found && r.path && targets.length > 0) {
        const found = r.path;
        patchPlugin((s) => {
          // One binary, several rows: `cargo build`, `cargo test`, `cargo
          // clippy` and `cargo` are four tools and one executable, and making
          // the user press Detect on each is the button stopping short.
          for (const target of targets) setGlobalPath(s, target, found);
        });
      }
    } catch (e) {
      auditDetect = {
        ...auditDetect,
        [toolKey]: { found: false, path: null, version: null, error: String(e) },
      };
    }
  }
</script>

<section>
  <h2>Tool Plugins</h2>
  <small class="hint top">
    Tool definitions. A plugin is one JSON file describing tools cImp can
    run — no rebuild, and <strong>no binaries</strong>: the plugin says
    how to call a tool, you say where that tool lives. Drop your own into
    the <code>plugins\</code> folder beside cImp. The ones marked
    <strong>built in</strong> ship with cImp (the Code Audit scanners
    live here) and are the only ones that resolve a binary for you, from
    the <code>ebin\</code> folder then your PATH; for every other tool an
    unset path means it does not run. Enables, timeouts and paths are
    machine-wide (they describe this computer); the per-project path
    override and the declared variables are per project.
  </small>

  <h3>Command tools in AI tabs</h3>
  <small class="hint">
    Let AI tabs run this project's
    <strong>command</strong> tools through the
    <code>run_command</code> MCP tool — the enabled ones with a path set,
    and nothing else. It runs the registered binary directly with the
    arguments the model passes (no shell) in the project root. Hidden
    while no command tool is runnable. A harness that caches its tool
    list at connect picks a change here up only after a tab restart
    (Tabs → Restart). A change here also rewrites the managed-tool
    steering paragraph a tab is launched with (Injection protection →
    Managed-tool steering), which is spawn-baked on both harnesses — so
    open tabs are owed a restart either way.
  </small>
<!-- V40 Phase B: one box per registered harness, same reason as the
       Code Audit set above. -->
  {#each $harnesses as h (h.id)}
    <Toggle
      checked={harnessRow(snapshot, h.id).expose_commands}
      onchange={(next) =>
        patch((s) => {
          const on = next;
          s.harness = {
            ...(s.harness ?? {}),
            [h.id]: { ...harnessRow(s, h.id), expose_commands: on },
          };
        })}
    >
      Expose to {h.label}
    </Toggle>
  {/each}

  {#snippet toolPluginRow(plugin: PluginRow, tool: ToolRow)}
    <div class="audit-tool">
      <label class="checkbox">
        <input
          type="checkbox"
          checked={tool.enabled}
          onchange={(e) =>
            {
              const on = (e.currentTarget as HTMLInputElement).checked;
              patchPlugin((s) => setToolEnabled(s, plugin.key, tool.id, on));
              // A manual edit of a built-in QUALITY scanner switches
              // auto-selection to manual mode, so the choice sticks
              // across census refreshes instead of being re-derived at
              // the next scan.
              onmanualtooledit(plugin.key, tool.id);
            }}
        />
        <span class="audit-name">{tool.label}</span>
        <span class="audit-role">{tool.description ?? tool.kind}</span>
        <span class="audit-scope" class:local={tool.path.scope === 'project'}>
          {tool.provider
            ? 'MCP'
            : tool.path.scope === 'unset'
              ? 'no path'
              : tool.path.scope}
        </span>
      </label>

      <!-- The phone-app pattern: what this tool ASKS for, in one place,
           beside the switch that grants it. Read-only — the screening
           that can refuse a grant happens at spawn time. -->
      <details class="plugin-perms" open={permissionsOpen(tool)}>
        <summary>This tool asks for…</summary>
        <ul>
          {#each tool.permissions as line (line)}
            <li>{line}</li>
          {/each}
        </ul>
      </details>

      {#if !plugin.enabled}
        <small class="hint audit-na">off — the plugin is disabled</small>
      {:else if !tool.provider && tool.path.effective === '' && !tool.resolvesByName}
        <small class="hint audit-na">
          no path set, so this tool does not run
        </small>
      {/if}

      {#if tool.provider}
        <!-- V38 Phase F, tier 2: no binary, so no path boxes. The pane
             shows the server this tool calls instead — an empty path
             input beside it would be an instruction nobody can follow,
             and a "no path set, so this tool does not run" hint would be
             simply false. Editing the server is MCP-registry work and
             lives in the MCP servers section, so this is read-only. -->
        <small class="hint plugin-field">Answered by an MCP server</small>
        <small class="hint">
          <code>{tool.provider.server}</code> → <code>{tool.provider.tool}</code>
          — configure and enable it under <strong>MCP servers</strong>.
          Nothing is installed or spawned for this tool on this machine.
        </small>
      {:else}
        <small class="hint plugin-field">Path on this machine</small>
        <div class="input-with-action">
          <input
            type="text"
            placeholder={tool.resolvesByName
              ? '(use the ebin folder / PATH)'
              : '(not set — the tool will not run)'}
            value={tool.path.global}
            oninput={(e) =>
              patchPlugin((s) =>
                setGlobalPath(
                  s,
                  tool.toolKey,
                  (e.currentTarget as HTMLInputElement).value,
                ),
              )}
          />
          <button
            type="button"
            class="secondary"
            onclick={() => void detectPluginTool(plugin, tool)}
          >
            Detect
          </button>
          <button
            type="button"
            class="secondary"
            onclick={() => void pickToolBinary(tool.toolKey, 'global')}
          >
            Browse…
          </button>
          <button
            type="button"
            class="secondary"
            onclick={() => patchPlugin((s) => setGlobalPath(s, tool.toolKey, ''))}
          >
            Clear
          </button>
        </div>
        {#if formatDetect(auditDetect[tool.toolKey]).kind !== 'idle'}
          {@const disp = formatDetect(auditDetect[tool.toolKey])}
          <small
            class="hint audit-detect"
            class:ok={disp.kind === 'found'}
            class:bad={disp.kind === 'not-found'}
          >
            {disp.text}
          </small>
        {/if}

        {#if pluginProjectKey}
          <small class="hint plugin-field">
            This project
            {tool.path.project === null ? '(inherited)' : '(overridden)'}
          </small>
          <div class="input-with-action">
            <input
              type="text"
              placeholder="(use the machine-wide path above)"
              value={tool.path.project ?? ''}
              oninput={(e) =>
                patchPlugin((s) =>
                  setProjectPath(
                    s,
                    pluginProjectKey,
                    tool.toolKey,
                    (e.currentTarget as HTMLInputElement).value,
                  ),
                )}
            />
            <button
              type="button"
              class="secondary"
              onclick={() => void pickToolBinary(tool.toolKey, 'project')}
            >
              Browse…
            </button>
            <button
              type="button"
              class="secondary"
              disabled={tool.path.project === null}
              onclick={() =>
                patchPlugin((s) =>
                  revertToGlobalPath(s, pluginProjectKey, tool.toolKey),
                )}
            >
              Use machine-wide
            </button>
          </div>
        {/if}
      {/if}

      {#each tool.variables as variable (variable.name)}
        <label class="audit-timeout">
          <span>{variable.label}</span>
          <input
            class="plugin-var"
            type="text"
            placeholder={variable.fallback ?? '(no default — set a value)'}
            value={variable.value}
            oninput={(e) =>
              patchPlugin((s) =>
                setToolVariable(
                  s,
                  plugin.key,
                  tool.id,
                  variable.name,
                  (e.currentTarget as HTMLInputElement).value,
                ),
              )}
          />
        </label>
      {/each}

      <label class="audit-timeout">
        <span>Timeout override (seconds — blank uses the plugin's)</span>
        <input
          type="number"
          min="1"
          placeholder="(plugin default)"
          value={tool.timeoutSecs ?? ''}
          oninput={(e) =>
            patchPlugin((s) =>
              setToolTimeout(
                s,
                plugin.key,
                tool.id,
                optionalSeconds((e.currentTarget as HTMLInputElement).value),
              ),
            )}
        />
      </label>

      {#if tool.parametersAllowed}
        <small class="hint">
          Extra arguments (appended after the tool's own):
        </small>
        {#each tool.parameters as parameter, i (i)}
          <div class="input-with-action">
            <input
              type="text"
              value={parameter}
              oninput={(e) =>
                patchPlugin((s) =>
                  setToolParameters(
                    s,
                    plugin.key,
                    tool.id,
                    tool.parameters.map((p, j) =>
                      j === i ? (e.currentTarget as HTMLInputElement).value : p,
                    ),
                  ),
                )}
            />
            <button
              type="button"
              class="secondary"
              onclick={() =>
                patchPlugin((s) =>
                  setToolParameters(
                    s,
                    plugin.key,
                    tool.id,
                    tool.parameters.filter((_, j) => j !== i),
                  ),
                )}
            >
              Remove
            </button>
          </div>
        {/each}
        <div class="button-row">
          <button
            type="button"
            class="secondary"
            onclick={() =>
              patchPlugin((s) =>
                setToolParameters(s, plugin.key, tool.id, [...tool.parameters, '']),
              )}
          >
            Add argument
          </button>
        </div>
      {/if}
    </div>
  {/snippet}

  <div class="button-row">
    <button
      type="button"
      class="secondary"
      disabled={rescanning}
      onclick={() => onrescan()}
    >
      {rescanning ? 'Rescanning…' : 'Rescan'}
    </button>
    {#if pluginSet?.dir}
      <code class="plugin-dir">{pluginSet.dir}</code>
    {/if}
  </div>
  {#if loadError}
    <small class="hint audit-detect bad">{loadError}</small>
  {/if}

  {#if pluginErrors.length > 0}
    <h3>Not loaded</h3>
    <small class="hint">
      These files are in the folder and were refused. Each one is also a
      row in the Events tab, with the same reason.
    </small>
    {#each pluginErrors as e (e.paths.join('|'))}
      <div class="plugin-error">
        <div class="plugin-error-head">
          <span class="audit-name">{e.label}</span>
          <span class="audit-scope local">{e.kind}</span>
        </div>
        <small class="hint audit-detect bad">{e.reason}</small>
        {#each e.paths as p (p)}
          <code class="plugin-dir">{p}</code>
        {/each}
      </div>
    {/each}
  {/if}

  {#if pluginList.length === 0}
    <p class="hint">
      No plugins loaded yet. Put a manifest in
      <code class="plugin-dir">{pluginSet?.dir ?? 'the plugins folder beside cimp.exe'}</code>
      and press Rescan.
    </p>
  {:else}
    <div class="plugin-split">
      <ul class="plugin-list">
        {#each pluginList as p (p.key)}
          <li>
            <!-- One line, styled as the settings sidebar's entries are:
                 this IS a category list, and a two-line bordered card
                 per plugin made the pane read as a different app than
                 the one it lives in. What each plugin IS (built in, how
                 many tools, where its manifest is) belongs to the row
                 the user selected, not to all of them at once — so it
                 moved into the detail. The one piece of state that
                 cannot wait for a click is "off", because a list that
                 looks uniform while half of it is inert is a lie. -->
            <!-- `icon` opts out of the TUI themes' `[ … ]` bracket framing:
                 these are list entries, not actions. -->
            <button
              type="button"
              class="plugin-list-entry icon"
              class:active={pluginActive?.key === p.key}
              class:off={!p.enabled}
              onclick={() => (pluginSelected = p.key)}
            >
              {pluginLabels.get(p.key) ?? p.label}{p.enabled ? '' : ' · off'}
            </button>
          </li>
        {/each}
      </ul>

      <div class="plugin-detail">
        {#if pluginActive}
          {@const plugin = pluginActive}
          <label class="checkbox">
            <input
              type="checkbox"
              checked={plugin.enabled}
              onchange={(e) =>
                patchPlugin((s) =>
                  setPluginEnabled(
                    s,
                    plugin.key,
                    (e.currentTarget as HTMLInputElement).checked,
                  ),
                )}
            />
            <span class="audit-name">{plugin.label}</span>
            <!-- Decision 9's version, shown here rather than in the
                 list: it identifies the plugin the user is looking AT,
                 and two coexisting versions are told apart by this line
                 plus the manifest path below it. -->
            <span class="plugin-version">{plugin.version}</span>
          </label>
          <small class="hint plugin-origin">
            {plugin.builtin ? 'built in · ' : ''}{plugin.toolCount}
            {plugin.toolCount === 1 ? 'tool' : 'tools'}
          </small>
          {#if plugin.description}
            <small class="hint">{plugin.description}</small>
          {/if}
          <code class="plugin-dir">{plugin.manifestPath}</code>
          {#if !plugin.enabled}
            <small class="hint audit-na">
              Every tool below is off while the plugin is. Their own
              checkboxes keep what you set, so switching the plugin back
              on restores this selection.
            </small>
          {/if}

          {#each plugin.categories as category (category.id)}
            <div class="plugin-category">
              <label class="checkbox">
                <input
                  type="checkbox"
                  checked={category.state === 'on'}
                  indeterminate={category.state === 'mixed'}
                  onchange={(e) =>
                    patchPlugin((s) =>
                      setCategoryEnabled(
                        s,
                        plugin.key,
                        category,
                        (e.currentTarget as HTMLInputElement).checked,
                      ),
                    )}
                />
                <span class="audit-name">{category.label}</span>
                <span class="audit-role">
                  {category.tools.filter((t) => t.enabled).length}/{category.tools.length}
                  on
                </span>
              </label>

              {#each category.tools as tool (tool.toolKey)}
                {@render toolPluginRow(plugin, tool)}
              {/each}
            </div>
          {/each}
        {/if}
      </div>
    </div>
  {/if}

</section>

<style>
  /* V23 Phase A: Code Audit per-tool row — since V38 the scanners are a plugin,
     so the row is this section's, not Code Audit's. */
  .audit-tool {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) 0;
    border-top: 1px solid var(--border-default, rgba(128, 128, 128, 0.25));
  }
  .audit-tool .audit-name {
    font-weight: 600;
  }
  .audit-tool .audit-role {
    margin-left: var(--space-2);
    opacity: 0.7;
    font-size: 0.85em;
  }
  /* Where the effective path came from: `project` when a per-project override
     is in force, otherwise the machine-wide value. */
  .audit-tool .audit-scope {
    font-size: 0.75em;
    opacity: 0.6;
    border: 1px solid var(--border-subtle);
    border-radius: 3px;
    padding: 0 0.3rem;
    white-space: nowrap;
  }
  .audit-tool .audit-scope.local {
    color: var(--accent, #d77757);
    border-color: currentcolor;
  }
  /* The Detect readout. #129 (a) had to park this in `settings-chrome.css`
     because it ties at (0,3,1) with that sheet's `.button-row + small.hint`
     reset and one call site — the plugin-load error — sits right after a
     `.button-row`; in the old single style block the adjacency rule came last
     and won there. Moving it here would silently flip that (a child's CSS is
     emitted after the chrome sheet), so the exception is now WRITTEN DOWN
     instead of depending on source order: the rule below pins the adjacency
     case to the value it has always computed to. */
  small.hint.audit-detect {
    margin: 0;
    font-family: var(--font-mono, monospace);
    word-break: break-all;
  }
  .button-row + small.hint.audit-detect {
    margin-top: var(--space-1);
  }
  small.hint.audit-detect.ok {
    color: var(--success, #4caf50);
  }
  small.hint.audit-detect.bad {
    color: var(--danger, #e06c75);
  }
  small.hint.audit-na {
    margin: 0;
    font-style: italic;
    color: var(--warning, #e3b341);
    opacity: 0.85;
  }
  .audit-timeout {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 0.85em;
    opacity: 0.85;
  }
  .audit-timeout input {
    width: 7rem;
  }  /* V38: Tool Plugins master-detail. */
  .plugin-split {
    display: flex;
    gap: var(--space-4);
    align-items: stretch;
    margin-top: var(--space-3);
  }
  /* The plugin list is the settings sidebar's idiom applied inside a section:
     a column of single-line entries, separated from what they select by the
     same hairline the window's own .sidebar uses against .content. */
  .plugin-list {
    flex: 0 0 13rem;
    list-style: none;
    margin: 0;
    padding: 0 var(--space-3) 0 0;
    border-right: 1px solid var(--border-faint);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .plugin-list-entry {
    display: block;
    width: 100%;
    text-align: left;
    padding: 7px 12px;
    background: transparent;
    border: 1px solid transparent;
    color: var(--text-quiet);
    border-radius: var(--radius-md);
    font-size: var(--font-size-sm);
    cursor: pointer;
    /* A long plugin name shortens rather than reflowing: the entries are one
       line each, and a wrapped one would break the rhythm of the column. */
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard);
  }
  /* A disabled plugin keeps its place in the list — it is still the thing you
     click to switch it back on — but reads as inactive, and says so. */
  .plugin-list-entry.off:not(.active) {
    color: var(--text-tertiary);
  }
  /* One step raised from the surface behind them, exactly as .sidebar's entries
     are against .sidebar. That surface is --surface-1 here (a settings section)
     rather than --surface-deep, so the same RELATIONSHIP is one token up. */
  .plugin-list-entry:hover:not(.active) {
    background: var(--surface-2);
    color: var(--text-primary);
  }
  .plugin-list-entry.active {
    background: var(--surface-2);
    color: var(--accent-purple);
    font-weight: 600;
    border-color: var(--border-subtle);
  }
  .plugin-list-entry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .plugin-detail {
    flex: 1;
    min-width: 0;
  }
  .plugin-version {
    margin-left: var(--space-2);
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
  }
  /* "built in · N tools" — the provenance/size line the list used to carry
     under every entry. It belongs to the plugin's identity, so it sits tight
     under the enable checkbox rather than a paragraph away from it. (Top margin
     comes from the `label.checkbox + small.hint` rule further down.) */
  small.hint.plugin-origin {
    margin-bottom: var(--space-2);
  }
  .plugin-category {
    margin-top: var(--space-3);
  }
  .plugin-error {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) 0;
    border-top: 1px solid var(--border-default, rgba(128, 128, 128, 0.25));
  }
  .plugin-error-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  code.plugin-dir {
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
    word-break: break-all;
  }
  .plugin-perms {
    font-size: 0.85em;
    opacity: 0.85;
  }
  .plugin-perms summary {
    cursor: pointer;
  }
  .plugin-perms ul {
    margin: var(--space-2) 0 0;
    padding-left: 1.2rem;
  }
  .audit-timeout input.plugin-var {
    width: 14rem;
  }
  small.hint.plugin-field {
    margin: var(--space-2) 0 0;
    font-size: 0.85em;
  }
</style>
