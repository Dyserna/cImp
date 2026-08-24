<script lang="ts">
  /// Settings → Tabs (#129 (c)) — per-tab configuration for the AI builtins
  /// and the shell tabs, behind one sub-tab per tab.
  ///
  /// **The restart-baseline machinery stays with the parent**, exactly as the
  /// issue requires. `SettingsApp` owns `tabBaselines`, `spawnStaleTabs` and
  /// the `restartRequired` derivation; this component receives the verdict as a
  /// prop and calls back to restart. The reason is not tidiness: a baseline is
  /// captured on MOUNT, and a child that re-took them every time the user
  /// switched sections would compare a tab against a snapshot taken after the
  /// user's own edits and fire the "restart them" hint with no change behind
  /// it. F-1's non-reactive `get()` inside `spawnBakedInjectionL2` is the other
  /// half of that timing, and it is deliberate.
  ///
  /// **`subSection` is a prop, not local state.** A deep link from another
  /// window ("Configure tab" on an AI tab) sets `activeSection = 'tabs'` AND
  /// the sub-tab, before this component exists; and `toggleAiTabEnabled` moves
  /// it when the user disables the tab they are looking at. Both writers are
  /// the parent's, so the state is too.
  ///
  /// **`toggleAiTabEnabled` is the parent's** for a harder reason: it writes
  /// `snapshot` directly — an optimistic update with a rollback on IPC failure
  /// — rather than going through `patch()`. Only the owner of `snapshot` can
  /// do that.
  ///
  /// The two per-harness snippets came along: both are rendered only from this
  /// section, and a snippet defined beside its one caller is easier to follow
  /// than one passed down as a prop.
  import {
    findHarnessByTabId,
    harnesses,
    harnessLoadState,
    labelForTabId,
    loadHarnesses,
  } from '../../harness';
  import type { SettingFieldView } from '../../harness';
  import type { AiTabId } from '../../tabs/types';
  import {
    findTab,
    findTabIndex,
    setHarnessExt,
    type AiToolTabConfig,
    type Settings,
    type ShellTabConfig,
    type TabConfig,
  } from '../types';
  import HarnessExtForm from '../HarnessExtForm.svelte';
  import TabSettingsSection from '../TabSettingsSection.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    aiTabIds,
    rosterReady,
    tabDefaults,
    restartRequired,
    aiTabsError,
    subSection,
    onsubsection,
    onrestart,
    ontoggleenabled,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push).
    patch: (updater: (s: Settings) => void) => void;
    /// The reserved AI tab ids, in canonical order, from the registry.
    aiTabIds: AiTabId[];
    /// Whether the harness roster has landed. Drives the two pending states.
    rosterReady: boolean;
    /// Backend-supplied per-tab defaults, for the "Reset to default" buttons.
    tabDefaults: Record<string, AiToolTabConfig | null>;
    /// Per-tab restart verdict, derived by the parent against the baselines it
    /// captured on mount. Read-only here.
    restartRequired: Record<string, boolean>;
    /// The last `set_enabled_ai_tabs` failure, rendered under the checkboxes.
    aiTabsError: string | null;
    /// Which sub-tab is open. Parent-owned — see the note above.
    subSection: string;
    /// Ask the parent to open a different sub-tab.
    onsubsection: (id: string) => void;
    /// Restart one AI tab and clear its baseline.
    onrestart: (tab: AiTabId) => void;
    /// Enable or disable one AI tab (optimistic write + rollback, parent-side).
    ontoggleenabled: (id: AiTabId, enable: boolean) => void;
  } = $props();

  /// Tabs visible in this section, in their stored order. Filtered view of
  /// `snapshot.tabs` so the template can render AI tabs and Shell tabs
  /// differently.
  const tabEntries = $derived<TabConfig[]>(snapshot.tabs);
  /// Was three {@const} tags at the top of the branch; a {@const} may not be a
  /// component root, so they are runes here. Same expressions, same order.
  const shellEntries = $derived(tabEntries.filter((e) => e.kind === 'shell'));
  const enabledAiTabs = $derived(snapshot.enabled_ai_tabs);
  const lastChecked = $derived(enabledAiTabs.length === 1 ? enabledAiTabs[0] : null);

  function aiTabAt(id: string): AiToolTabConfig | null {
    const entry = findTab(snapshot, id);
    return entry && entry.kind === 'ai_tool' ? entry : null;
  }

  function shellSummary(t: ShellTabConfig): string {
    const args = t.args.length > 0 ? ' ' + t.args.join(' ') : '';
    return `${t.command}${args}`;
  }

  /// Replace the AI-tab entry at `id` in the snapshot. Used by the
  /// TabSettingsSection's bound setter; the array shape forces the
  /// find-by-id lookup at write time.
  function patchAiTab(id: string, value: AiToolTabConfig) {
    // The value came in via a $bindable() prop spread from the child
    // (TabSettingsSection). The spread copies own keys but leaves nested
    // children as $state proxy references. Snapshotting here flattens
    // those to plain JS so structuredClone in the store subscriber and
    // Tauri's IPC serializer don't choke. See the DataCloneError that
    // surfaced when wiring the per-tab Terminal palette dropdown.
    const plain = $state.snapshot(value) as AiToolTabConfig;
    patch((s) => {
      const idx = findTabIndex(s, id);
      if (idx < 0) return;
      s.tabs[idx] = plain;
    });
  }

  /// Replace the Shell-tab entry's notification config in the snapshot.
  /// Inline-editable in the Settings window (M4) — notifications apply
  /// live, no restart needed, so the existing settings broadcast flow is
  /// all we need. Spawn-affecting fields (command/args/cwd) are read-only
  /// here; the user changes them via the tab bar's right-click → Configure.
  function patchShellNotifications(
    id: string,
    next: ShellTabConfig['notifications'],
  ) {
    patch((s) => {
      const idx = findTabIndex(s, id);
      if (idx < 0) return;
      const entry = s.tabs[idx];
      if (entry.kind !== 'shell') return;
      s.tabs[idx] = { ...entry, notifications: next };
    });
  }
</script>

<!-- V40 review F-2/F-3: what a block that renders per harness shows while the
     roster is not in. `loadHarnesses` retries and reports, so the two states are
     distinguishable and the failed one offers the retry rather than leaving the
     window permanently missing its per-harness controls with no explanation. -->
{#snippet rosterPending()}
  {#if $harnessLoadState === 'failed'}
    <div class="roster-error" role="status">
      <span
        >The harness registry could not be read, so the per-harness controls are
        hidden. Nothing is lost — your settings are untouched.</span
      >
      <button type="button" onclick={() => void loadHarnesses()}>Try again</button>
    </div>
  {:else}
    <small class="hint">Loading the harness registry…</small>
  {/if}
{/snippet}

<!-- V40 Phase B (locked decision 6) — one harness's DECLARED settings.
     A snippet rather than a copy per section: the sections that host it are
     per TAB, what it renders is per HARNESS, and this window should not be
     where those two get confused. A harness that declares no fields renders
     nothing, with no work here. -->
{#snippet harnessSettingsFor(harnessId: string, filter: (f: SettingFieldView) => boolean = () => true)}
  {#each $harnesses.filter((h) => h.id === harnessId) as h (h.id)}
    <HarnessExtForm
      harness={h}
      snapshot={snapshot}
      patch={(id, key, value) => patch((s) => setHarnessExt(s, id, key, value))}
      {filter}
    />
  {/each}
{/snippet}

<section>
  <h2>Tabs</h2>
  <fieldset class="ai-tabs-radio">
    <legend>AI tabs enabled</legend>
    <small class="hint">
      Pick which AI-tool tabs to keep. Toggling a checkbox opens
      or closes the matching tab (the closed tab's PTY is killed
      and its scrollback dropped). At least one tab must remain
      checked.
    </small>
    <!-- V40 review F-2: not before the roster is in. These are
         DESTRUCTIVE controls (a tick kills a PTY), and until the
         registry answers there is no label to put on them. -->
    {#if !rosterReady}
      {@render rosterPending()}
    {:else}
    <div class="radio-row">
      <!-- V40 Phase F (locked decision 7): one checkbox per RESERVED
           tab id the registry declares, in its canonical order. It was
           three hand-written boxes, so a third harness's tabs could not
           be turned on at all without editing this file. -->
      {#each aiTabIds as aiTabId (aiTabId)}
        <label>
          <input
            type="checkbox"
            name="ai-tabs-enabled"
            value={aiTabId}
            checked={enabledAiTabs.includes(aiTabId)}
            disabled={lastChecked === aiTabId}
            onchange={(e) =>
              ontoggleenabled(
                aiTabId,
                (e.currentTarget as HTMLInputElement).checked,
              )}
          />
          {labelForTabId($harnesses, aiTabId)}
        </label>
      {/each}
    </div>
    {/if}
    {#if aiTabsError}
      <small class="error">{aiTabsError}</small>
    {/if}
  </fieldset>
  <Toggle
    checked={snapshot.ui.tool_activity_tab}
    onchange={(next) => patch((s) => (s.ui.tool_activity_tab = next))}
  >
    Show the <strong>Tools</strong> tab
  </Toggle>
  <small class="hint">
    One place to watch tool usage: a unified feed of code-intelligence
    graph calls and offload requests, plus the graph/offload tool
    reference lists.
  </small>
  <Toggle
    checked={snapshot.ui.events_tab}
    onchange={(next) => patch((s) => (s.ui.events_tab = next))}
  >
    Show the <strong>Events</strong> tab
  </Toggle>
  <small class="hint">
    The same recorded activity, read as events: every row says which
    tab and which session it came from, and the feed filters by kind,
    source/screen and tab. Independent of the Tools tab — turning one
    off leaves the other alone.
  </small>
  <Toggle
    checked={snapshot.preview_allow_remote}
    onchange={(next) => patch((s) => (s.preview_allow_remote = next))}
  >
    Allow <strong>Preview</strong> tabs to load remote URLs
  </Toggle>
  <small class="hint">
    Off (default) restricts Preview-tab navigation to localhost and
    private-network (RFC&nbsp;1918) hosts — the tab is meant for your
    own dev servers. On lets a Preview tab load any http(s) URL in its
    embedded webview.
  </small>
  <div class="sub-tabs" role="tablist" aria-label="Tabs sub-sections">
    <!-- V40 Phase F: one sub-tab per reserved AI tab id, from the
         registry (locked decision 7). Three hand-written buttons
         before, each naming a tab id and a product. -->
    {#if rosterReady}
      {#each aiTabIds as aiTabId (aiTabId)}
        <button
          type="button"
          role="tab"
          class:active={subSection === aiTabId}
          aria-selected={subSection === aiTabId}
          onclick={() => onsubsection(aiTabId)}
        >
          {labelForTabId($harnesses, aiTabId)}
        </button>
      {/each}
    {/if}
    <button
      type="button"
      role="tab"
      class:active={subSection === 'shells'}
      aria-selected={subSection === 'shells'}
      onclick={() => onsubsection('shells')}
    >
      Shells
      {#if shellEntries.length > 0}
        <span class="sub-tab-count">{shellEntries.length}</span>
      {/if}
    </button>
  </div>

  {#if !rosterReady && subSection !== 'shells'}
    {@render rosterPending()}
  {:else if rosterReady && aiTabIds.includes(subSection)}
    <!--
      V40 Phase F (locked decision 7): ONE body for every reserved AI
      tab, instead of a `{:else if}` per tab id. The two facts that used
      to be spelled per branch are registry lookups now:

      * the harness's declared settings render under its FIRST reserved
        tab, because they are the harness's and not the tab's — with
        ONE declared exception (issue #109): the rows a plugin marks
        `provider_tab` describe the custom-provider variant, so they
        render on THAT tab's page instead, next to the tab they
        configure. A reserved tab that is neither gets a pointer rather
        than a second copy of the form;
      * every name comes from the descriptor, so a harness added over
        IPC arrives with its own heading and no markup here.
    -->
    {@const harness = findHarnessByTabId($harnesses, subSection)}
    {@const live = aiTabAt(subSection)}
    <!--
      Where a declared field renders: its harness's custom-provider tab
      if it is marked `provider_tab` AND such a tab exists, otherwise
      the harness's first reserved tab. The fallback is deliberate — a
      harness that declares provider rows and no provider tab shows them
      rather than hiding them (no shipped harness does).
    -->
    {@const fieldHome = (f: SettingFieldView) =>
      f.provider_tab && harness?.provider_tab_id
        ? harness.provider_tab_id
        : (harness?.tab_ids[0] ?? '')}
    {@const ownsForm =
      harness?.tab_ids[0] === subSection ||
      harness?.provider_tab_id === subSection}
    <div id="tab-section-{subSection}">
      {#if live}
        <TabSettingsSection
          tabId={subSection}
          displayName={labelForTabId($harnesses, subSection)}
          bind:settings={
            () => live,
            (v) => patchAiTab(subSection, v)
          }
          defaults={tabDefaults[subSection] ?? null}
          restartRequired={restartRequired[subSection] ?? false}
          onchange={() => {}}
          onrestart={() => onrestart(subSection as AiTabId)}
        />
      {:else}
        <small class="hint top"
          >{labelForTabId($harnesses, subSection)} tab is disabled — tick the
          checkbox above to enable it.</small
        >
      {/if}
      {#if harness && ownsForm}
        {@render harnessSettingsFor(
          harness.id,
          (f) => fieldHome(f) === subSection,
        )}
      {:else if harness}
        <small class="hint top">
          This tab's custom-provider values (and everything else this
          harness declares) are {harness.label}'s own settings — see
          <strong>Tabs → {labelForTabId($harnesses, harness.tab_ids[0])}</strong>.
        </small>
      {/if}
    </div>
  {:else}
    <small class="hint top">
      Shell tabs in their stored order. Each row shows notification
      text — edit command / args / cwd via right-click → Configure
      on the tab bar.
    </small>
    {#if shellEntries.length === 0}
      <small class="hint top">No shell tabs configured.</small>
    {:else}
      <div class="tabs-grid">
        {#each shellEntries as entry (entry.id)}
          {#if entry.kind === 'shell'}
            <details id="tab-section-{entry.id}">
              <summary>
                {entry.name}
                <span class="kind-badge shell">Shell</span>
                {#if entry.builtin}
                  <span class="builtin-tag">builtin</span>
                {/if}
              </summary>
              <div class="shell-edit">
                <label>
                  <span>Command</span>
                  <input type="text" value={shellSummary(entry)} disabled readonly />
                  <small class="hint">
                    To change the command, args, or working directory,
                    right-click the tab in the tab bar and choose
                    Configure…
                  </small>
                </label>
                <div class="shell-notif-row">
                  <label class="row-toggle">
                    <input
                      type="checkbox"
                      checked={entry.notifications.error.enabled}
                      onchange={(e) =>
                        patchShellNotifications(entry.id, {
                          ...entry.notifications,
                          error: {
                            ...entry.notifications.error,
                            enabled: (e.currentTarget as HTMLInputElement)
                              .checked,
                          },
                        })}
                    />
                    <span>Error notification</span>
                  </label>
                  <input
                    type="text"
                    value={entry.notifications.error.text}
                    disabled={!entry.notifications.error.enabled}
                    oninput={(e) =>
                      patchShellNotifications(entry.id, {
                        ...entry.notifications,
                        error: {
                          ...entry.notifications.error,
                          text: (e.currentTarget as HTMLInputElement).value,
                        },
                      })}
                  />
                  <small class="hint">
                    Spoken when this tab errors while you're on a different
                    tab.
                  </small>
                </div>
                <div class="shell-notif-row">
                  <label class="row-toggle">
                    <input
                      type="checkbox"
                      checked={entry.notifications.exited.enabled}
                      onchange={(e) =>
                        patchShellNotifications(entry.id, {
                          ...entry.notifications,
                          exited: {
                            ...entry.notifications.exited,
                            enabled: (e.currentTarget as HTMLInputElement)
                              .checked,
                          },
                        })}
                    />
                    <span>Exited notification</span>
                  </label>
                  <input
                    type="text"
                    value={entry.notifications.exited.text}
                    disabled={!entry.notifications.exited.enabled}
                    oninput={(e) =>
                      patchShellNotifications(entry.id, {
                        ...entry.notifications,
                        exited: {
                          ...entry.notifications.exited,
                          text: (e.currentTarget as HTMLInputElement).value,
                        },
                      })}
                  />
                  <small class="hint">
                    Spoken when this shell exits while you're on a different
                    tab. Use <code>{'{code}'}</code> to insert the exit code.
                  </small>
                </div>
              </div>
            </details>
          {/if}
        {/each}
      </div>
    {/if}
  {/if}
</section>

<style>
  /* V40 review F-3: the roster-load failure banner. Same weight as a field
     error — it explains why a block is missing, and carries the retry. */
  .roster-error {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
    color: var(--text-danger-soft);
    font-size: var(--font-size-xs);
  }
  .ai-tabs-radio {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    padding: var(--space-3) var(--space-4);
    margin: 0 0 var(--space-4) 0;
    background: var(--surface-1);
  }
  .ai-tabs-radio legend {
    padding: 0 var(--space-2);
    font-size: var(--font-size-sm);
    font-weight: 500;
    color: var(--text-primary);
  }
  .ai-tabs-radio .hint {
    display: block;
    margin: 0 0 var(--space-3) 0;
    color: var(--text-quiet);
  }
  .ai-tabs-radio .radio-row {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
  }
  .ai-tabs-radio .radio-row label {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: var(--font-size-sm);
    cursor: pointer;
  }
  .tabs-grid {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: var(--space-2);
  }
  .kind-badge {
    display: inline-block;
    font-size: 9px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    padding: 1px 6px;
    border-radius: var(--radius-pill);
    margin-left: 6px;
    vertical-align: middle;
    font-weight: 600;
  }
  .kind-badge.shell {
    background: var(--surface-success);
    border: 1px solid var(--text-success-bright);
    color: var(--text-success);
  }
  .builtin-tag {
    display: inline-block;
    font-size: 9px;
    font-weight: var(--font-weight-medium);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-tertiary);
    border: 1px solid var(--border-default);
    padding: 1px 6px;
    border-radius: var(--radius-pill);
    margin-left: 6px;
    vertical-align: middle;
  }
  .shell-edit {
    padding: var(--space-3) 14px;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .shell-edit label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--font-size-sm);
    color: var(--text-quiet);
  }
  .shell-edit input[type="text"] {
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: Consolas, Menlo, monospace;
    font-size: var(--font-size-md);
    transition: border-color var(--motion-fast) var(--easing-standard);
  }
  .shell-edit input[type="text"]:focus {
    outline: none;
    border-color: var(--accent);
  }
  .shell-edit input[disabled] {
    color: var(--text-tertiary);
    background: var(--surface-deep);
  }
  .shell-edit code {
    background: var(--surface-1);
    padding: 1px var(--space-1);
    border-radius: var(--radius-sm);
    font-size: var(--font-size-xs);
  }
  /* V1.11 per-slot notification row: enabled checkbox above a text
     input. The disabled-text style mirrors `.shell-edit input[disabled]`
     so a toggled-off slot reads as visually quiet without the
     readonly-Command "this is informational" feel. */
  .shell-edit .shell-notif-row {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .shell-edit .row-toggle {
    flex-direction: row;
    align-items: center;
    gap: 6px;
    cursor: pointer;
  }
  .shell-edit .row-toggle input[type="checkbox"] {
    margin: 0;
  }
  .shell-edit .shell-notif-row > input[type="text"]:disabled {
    opacity: 0.5;
  }
</style>
