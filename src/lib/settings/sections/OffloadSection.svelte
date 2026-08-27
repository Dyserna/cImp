<script lang="ts">
  /// Settings → Offload task tools (#129 (c)) — the backend pool, the server
  /// templates, the tool scope and the command policies, behind two sub-tabs.
  ///
  /// **What stays with the parent.** The 4-second status poll
  /// (`refreshBackendStatuses`) is started in `onMount` and stopped in
  /// `onDestroy`, and it fills four things this window renders in three
  /// different sections; owning it here would make it start on first VIEW and
  /// stop on every section switch. So `backendStatuses` arrives as a prop, and
  /// `statusFor` reads that prop.
  ///
  /// `enableReadonlyCommands` is the parent's for the same reason
  /// `toggleAiTabEnabled` is: it assigns the IPC's returned `Settings` straight
  /// to `snapshot`, and only the owner of `snapshot` can do that.
  ///
  /// Everything else — the pool mutations, the template popup, the policy
  /// editor, the scope radio, the local-provider registration and the test box
  /// — writes through `patch()` and moved here with the markup.
  import { facadeBackends } from '../../delegation';
  import { harnesses, type HarnessInfo } from '../../harness';
  import {
    offloadBackendRestart,
    offloadBackendStart,
    offloadBackendStop,
    offloadDeriveLocalProvider,
    offloadTest,
    describeBackendStatus,
    type BackendStatus,
  } from '../../offload';
  import {
    harnessRow,
    localDataExcludedScope,
    setHarnessExt,
    toolScopeMode,
    type BackendTier,
    type CommandPolicy,
    type OffloadBackend,
    type RemoteBackendTemplate,
    type ServerCommandTemplate,
    type Settings,
    type ToolScope,
  } from '../types';
  import NumberField from '../NumberField.svelte';
  import SelectField from '../SelectField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    backendStatuses,
    harnessNames,
    subSection,
    onsubsection,
    testInput,
    testResult,
    ontestinput,
    ontestresult,
    onenablereadonly,
    onnavigate,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// Live per-backend status rows, from the parent's 4 s poll.
    backendStatuses: BackendStatus[];
    /// The enabled harnesses' labels, joined. Parent-owned — three sections
    /// interpolate it.
    harnessNames: string;
    /// The sub-tab this section shows, and the offload test box's prompt and
    /// last answer. All parent-owned (V42 tranche-2 review, T2-5): the sidebar
    /// destroys this component on every switch away, and before #129 (c) a
    /// typed prompt and the answer to it outlived that.
    subSection: string;
    testInput: string;
    testResult: string;
    /// The user picked another sub-tab.
    onsubsection: (id: string) => void;
    /// The prompt changed / a run produced (or failed to produce) an answer.
    ontestinput: (next: string) => void;
    ontestresult: (next: string) => void;
    /// Grant the read-only command set. Parent-owned: the IPC returns a whole
    /// `Settings` that is assigned straight to `snapshot`.
    onenablereadonly: () => void;
    /// Jump to another Settings section (F-18's path correction pointer).
    onnavigate: (section: string) => void;
  } = $props();

  // V8-01 offload: a busy guard for the Start/Stop/Reset/Test buttons. Local
  // on purpose — it describes an IPC call in flight FROM THIS MOUNT, so a
  // section switch is exactly when it should reset. The prompt and the answer
  // beside it are the parent's (T2-5).
  let offloadBusy = $state<boolean>(false);
  async function runOffloadAction(action: () => Promise<void>): Promise<void> {
    offloadBusy = true;
    try {
      await action();
    } catch (e) {
      ontestresult(`Error: ${e}`);
    } finally {
      offloadBusy = false;
    }
  }

  async function runOffloadTest(): Promise<void> {
    offloadBusy = true;
    ontestresult('Running…');
    try {
      ontestresult(await offloadTest(testInput));
    } catch (e) {
      ontestresult(`Error: ${e}`);
    } finally {
      offloadBusy = false;
    }
  }

  /// The harnesses whose local-provider block the Offload card renders.
  ///
  /// V40 Phase F: the feature flag alone is not enough — a harness that
  /// declares `local_provider_config` but no block key would render a button
  /// writing an `ext` row under it, one the backend preserves untouched, so it
  /// would be stored forever and read by nobody while the real setting stayed
  /// at its default. `harness::info::tests::a_declared_config_writer_exists`
  /// makes that combination fail a Rust test, so this filter is belt and
  /// braces.
  const localProviderConfigHarnesses = $derived(
    $harnesses.filter(
      (h) =>
        h.features.includes('local_provider_config') &&
        h.affordances.localProviderConfigBlockKey &&
        h.affordances.localProviderConfigAutoKey,
    ),
  );

  // Sub-tab nav within this section: the backend pool + limits live under
  // 'pool'; native tools, allowlist, and command policies under 'tools'.
  // (MCP servers moved to their own top-level `mcp` section — they're usable by
  // the harness tabs directly now, not just the offload worker.)
  // The CHOICE is the parent's; see the prop docs above.

  // V21: register the given Local backend as `harness`'s local provider.
  // Derives base URL + model from its server command in Rust (which errors,
  // naming the missing --port/model flag, when the command is incomplete), then
  // persists the snapshot so a freshly opened tab of that harness is ready to
  // use. Overrides any existing registration; `providerMsg` reports
  // success/failure inline.
  //
  // V40 Phase F (locked decision 26/27): the harness is passed rather than
  // assumed. The button is mounted by the `local_provider_config` feature, so
  // the harness whose writer runs is the one whose button was clicked — with
  // two such harnesses the backend would otherwise have refused, asking which.
  let providerMsg = $state<{ i: number; text: string; ok: boolean } | null>(null);
  async function registerLocalProvider(i: number, h: HarnessInfo): Promise<void> {
    const backend = snapshot.offload.backends[i];
    const blockKey = h.affordances.localProviderConfigBlockKey;
    if (!backend || backend.kind.type !== 'local' || !blockKey) return;
    providerMsg = null;
    try {
      const provider = await offloadDeriveLocalProvider(h.id, backend.kind.server_command);
      // V40 Phase B: the derived block is that plugin's own `ext` row
      // (`SettingKind::Json` — written by cImp, never typed), not a field in
      // the offload block named after a harness. V40 review F-6: under the key
      // the plugin DECLARES, not the one this file used to spell.
      patch((s) => setHarnessExt(s, h.id, blockKey, provider));
      providerMsg = {
        i,
        ok: true,
        text: `Registered ${provider.model} at ${provider.base_url}. New tabs of that harness will use it by default.`,
      };
    } catch (e) {
      providerMsg = { i, ok: false, text: `${e}` };
    }
  }

  function statusFor(name: string): BackendStatus | undefined {
    return backendStatuses.find((s) => s.name === name);
  }

  // Backend-pool mutations (all go through `patch` so they persist + mark dirty).
  function uniqueBackendName(base: string): string {
    const names = new Set((snapshot.offload.backends ?? []).map((b) => b.name));
    if (!names.has(base)) return base;
    let i = 2;
    while (names.has(`${base}-${i}`)) i++;
    return `${base}-${i}`;
  }
  function addLocalBackend(): void {
    patch((s) => {
      s.offload.backends = [
        ...s.offload.backends,
        {
          name: uniqueBackendName('local'),
          enabled: true,
          kind: {
            type: 'local',
            server_command: '',
            autostart: false,
            show_command_on_start: false,
            auth_token: '',
          },
          declared_context: null,
          declared_model: '',
          tier: 'quality',
          tool_scope: { mode: 'all' },
        },
      ];
    });
  }
  function addRemoteBackend(): void {
    patch((s) => {
      s.offload.backends = [
        ...s.offload.backends,
        {
          name: uniqueBackendName('remote'),
          enabled: true,
          kind: { type: 'remote', base_url: '', auth_token: '', is_cloud: false, cloud_consent: false },
          declared_context: null,
          declared_model: '',
          tier: 'fast',
          tool_scope: { mode: 'all' },
        },
      ];
    });
  }
  // Adopt the legacy single `server_command` into the pool as one Local backend.
  function adoptLegacyServer(): void {
    patch((s) => {
      s.offload.backends = [
        {
          name: 'local',
          enabled: true,
          kind: {
            type: 'local',
            server_command: s.offload.server_command,
            autostart: s.offload.autostart,
            show_command_on_start: false,
            // The legacy single-server fields never had a token; adopting one
            // is an "no auth" backend until the user fills the field in.
            auth_token: '',
          },
          declared_context: null,
          declared_model: '',
          tier: 'quality',
          tool_scope: { mode: 'all' },
        },
      ];
    });
  }
  function removeBackend(i: number): void {
    patch((s) => {
      s.offload.backends = s.offload.backends.filter((_, idx) => idx !== i);
    });
  }
  function updateBackend(i: number, fn: (b: OffloadBackend) => void): void {
    patch((s) => {
      fn(s.offload.backends[i]);
    });
  }
  // ── Backend templates (global libraries) ───────────────────────────────
  // Save/Load/Delete controls under a backend field manage a global template
  // library shared across backends and restarts: Local backends use
  // `offload.server_command_templates` (name + command); Remote backends use
  // `offload.remote_backend_templates` (name + base URL + auth token). Only one
  // popup is open at a time; `templatePopup` records which backend (by index)
  // opened it and which mode it's in — the backend's own kind decides which
  // library the popup acts on.
  let templatePopup = $state<{ i: number; mode: 'save' | 'load' | 'delete' } | null>(null);
  let newTemplateName = $state('');
  let templateError = $state<string | null>(null);

  function openTemplatePopup(i: number, mode: 'save' | 'load' | 'delete'): void {
    // A second click on the same button closes the popup (toggle).
    if (templatePopup && templatePopup.i === i && templatePopup.mode === mode) {
      closeTemplatePopup();
      return;
    }
    templatePopup = { i, mode };
    newTemplateName = '';
    templateError = null;
  }
  function closeTemplatePopup(): void {
    templatePopup = null;
    newTemplateName = '';
    templateError = null;
  }
  // Validate the pending template name against an existing library; returns the
  // trimmed name or null (and sets `templateError`) when invalid.
  function validateTemplateName(existing: string[]): string | null {
    const name = newTemplateName.trim();
    if (!name) {
      templateError = 'Name required.';
      return null;
    }
    if (existing.includes(name)) {
      templateError = `A template named "${name}" already exists.`;
      return null;
    }
    return name;
  }
  // Local backend (server command) ───────────────────────────────
  function commitSaveLocalTemplate(i: number): void {
    const name = validateTemplateName(
      snapshot.offload.server_command_templates.map((t) => t.name),
    );
    if (!name) return;
    const backend = snapshot.offload.backends[i];
    const command = backend?.kind.type === 'local' ? backend.kind.server_command : '';
    patch((s) => {
      s.offload.server_command_templates = [
        ...s.offload.server_command_templates,
        { name, command },
      ];
    });
    closeTemplatePopup();
  }
  function loadLocalTemplate(i: number, tpl: ServerCommandTemplate): void {
    updateBackend(i, (b) => {
      if (b.kind.type === 'local') b.kind.server_command = tpl.command;
    });
    closeTemplatePopup();
  }
  function deleteLocalTemplate(name: string): void {
    patch((s) => {
      s.offload.server_command_templates =
        s.offload.server_command_templates.filter((t) => t.name !== name);
    });
  }
  // Remote backend (base URL + auth token) ───────────────────────
  function commitSaveRemoteTemplate(i: number): void {
    const name = validateTemplateName(
      snapshot.offload.remote_backend_templates.map((t) => t.name),
    );
    if (!name) return;
    const backend = snapshot.offload.backends[i];
    const base_url = backend?.kind.type === 'remote' ? backend.kind.base_url : '';
    const auth_token = backend?.kind.type === 'remote' ? backend.kind.auth_token : '';
    patch((s) => {
      s.offload.remote_backend_templates = [
        ...s.offload.remote_backend_templates,
        { name, base_url, auth_token },
      ];
    });
    closeTemplatePopup();
  }
  function loadRemoteTemplate(i: number, tpl: RemoteBackendTemplate): void {
    updateBackend(i, (b) => {
      if (b.kind.type === 'remote') {
        b.kind.base_url = tpl.base_url;
        b.kind.auth_token = tpl.auth_token;
      }
    });
    closeTemplatePopup();
  }
  function deleteRemoteTemplate(name: string): void {
    patch((s) => {
      s.offload.remote_backend_templates =
        s.offload.remote_backend_templates.filter((t) => t.name !== name);
    });
  }
  // ── Command security policies (Tools tab) ──────────────────────────────
  // All mutations route through `patch` so they persist + mark dirty, mirroring
  // the backend-pool helpers above.
  function addCommandPolicy(): void {
    patch((s) => {
      s.offload.command_policies = [
        ...s.offload.command_policies,
        { program: '', denied_flags: [], denied_subcommands: [], allowed_subcommands: [], env: [] },
      ];
    });
  }
  function removeCommandPolicy(i: number): void {
    patch((s) => {
      s.offload.command_policies = s.offload.command_policies.filter((_, idx) => idx !== i);
    });
  }
  function updatePolicy(i: number, fn: (p: CommandPolicy) => void): void {
    patch((s) => {
      fn(s.offload.command_policies[i]);
    });
  }
  // V21 F7: one-click "safe read-only commands" preset. The backend merges
  // `git` + `cargo` (metadata/tree, with its pinning policy) into the live
  // allowlist/policies atomically and returns the updated settings, which we
  // fold into the local snapshot. Idempotent + non-destructive — a merge, not a
  // mode: the user sees exactly what got added in the allowlist / policy
  // editors below and can prune any of it.

  // Comma-separated <-> string[] for the flag/subcommand inputs (mirrors the
  // allowlist input). Empty entries are dropped.
  function csvToList(value: string): string[] {
    return value
      .split(',')
      .map((c) => c.trim())
      .filter((c) => c.length > 0);
  }
  // Whether an allowlisted program currently has a hardening policy — drives
  // the transparency line next to the allowlist.
  function policyForProgram(program: string): CommandPolicy | undefined {
    const stem = program.split(/[\\/]/).pop()?.replace(/\.[^.]+$/, '').toLowerCase() ?? program;
    return snapshot.offload.command_policies.find(
      (p) => p.program.toLowerCase() === stem,
    );
  }
  // Toggling a backend's cloud flag flips its default tool scope to the safe
  // web/docs-only set (deny the local-data tools) so a cloud backend never
  // ships local file/exec tools unless the user explicitly widens it.
  function setBackendCloud(i: number, isCloud: boolean): void {
    updateBackend(i, (b) => {
      if (b.kind.type !== 'remote') return;
      b.kind.is_cloud = isCloud;
      if (isCloud) {
        b.tool_scope = localDataExcludedScope();
      } else {
        b.kind.cloud_consent = false;
        b.tool_scope = { mode: 'all' };
      }
    });
  }
  // Tool-scope picker: 'all' | 'web' (web/docs only) | custom (allexcept local-data).
  //
  // F-27: both the reader and the writer come from `settings/types` now, so the
  // radio cannot recognize a different set than the one it writes — and neither
  // depends on the exclusion list's LENGTH (a length test made a migrated
  // 7-entry list read as "custom", and clicking "web/docs only" then wrote the
  // stale 6-entry list back, dropping `run_check` from the exclusion).
  function scopeMode(scope: ToolScope): 'all' | 'web' | 'custom' {
    return toolScopeMode(scope);
  }
  function setScopeMode(i: number, mode: 'all' | 'web'): void {
    updateBackend(i, (b) => {
      b.tool_scope = mode === 'all' ? { mode: 'all' } : localDataExcludedScope();
    });
  }
</script>

<section>
  <h2>Local task offload</h2>
  <small class="hint top">
    Run a local <code>llama-server</code> and expose an
    <code>offload_task</code> tool into cImp-launched AI tabs.
    The main session can hand token-heavy subtasks (broad codebase
    searches, large-file/log summarization, web research) to the
    local model and get back only the synthesized result —
    conserving its context window. Everything stays local. Off by
    default; the model is user-supplied (not bundled).
  </small>
  <Toggle
    label="Enable offload"
    checked={snapshot.offload.enabled}
    onchange={(next) => patch((s) => (s.offload.enabled = next))}
  />
  <Toggle
    label="Inject offload guidance into the system prompt"
    checked={snapshot.offload.inject_guidance}
    onchange={(next) => patch((s) => (s.offload.inject_guidance = next))}
  />
  <small class="hint">
    The <code>offload_task</code> tool and its guidance are injected
    when an AI tab starts — restart the {harnessNames} tab
    (Tabs → Restart) after changing either toggle.
  </small>
  <Toggle
    label="Session push (experimental)"
    checked={snapshot.offload.session_push}
    onchange={(next) => patch((s) => (s.offload.session_push = next))}
  />
  <small class="hint">
    Lets cImp push notices — offload results, audit and graph-index
    completions — straight into a live AI tab.
    A tab whose harness can be PUSHED to receives them as
    <code>&lt;channel source="cimp-offload"&gt;</code> messages at the
    next turn boundary, which <em>starts a turn</em> when the tab is
    idle; that half is baked in at launch, so restart the tab after
    toggling — cImp shows the restart hint automatically. It also needs
    the <code>cimp-offload</code> MCP server to be injected, i.e.
    offload or the code graph enabled.
    A tab whose harness takes silently injected context
    (<code>noReply</code>) instead receives the same envelope with
    nothing started: the model picks it up on its next turn, and that
    half is read live — no tab restart needed.
    <strong>Experimental:</strong> the push half rides a harness
    research-preview flag that may change or disappear, the harness
    paints a
    persistent "Channels (experimental)" banner (plus a harmless
    "no MCP server configured with that name" warning) in every tab it
    registers, and a push that can't be delivered is silently dropped.
    Off by default.
  </small>

  <hr class="card-divider lg" />
  <div class="sub-tabs" role="tablist" aria-label="Offload sub-sections">
    <button
      type="button"
      role="tab"
      class:active={subSection === 'pool'}
      aria-selected={subSection === 'pool'}
      onclick={() => onsubsection('pool')}
    >
      Pool
    </button>
    <button
      type="button"
      role="tab"
      class:active={subSection === 'tools'}
      aria-selected={subSection === 'tools'}
      onclick={() => onsubsection('tools')}
    >
      Tools
    </button>
  </div>

  {#if subSection === 'pool'}
  <h3>Backend pool</h3>
  <small class="hint top">
    V8-02: route each offload to the right backend. Add a LAN box or a
    cloud API alongside the local server; the router picks one per task
    by tool need, required context, tier, and availability. The single
    <code>Server command</code> above is used as one local backend when
    the pool below is empty.
  </small>

  {#if snapshot.offload.backends.length === 0}
    <div class="button-row">
      <button type="button" onclick={adoptLegacyServer} disabled={!snapshot.offload.server_command.trim()}>
        Adopt the server command above as a Local backend
      </button>
    </div>
  {/if}

  {#each snapshot.offload.backends as backend, i (i)}
    <div class="backend-card">
      <div class="backend-head">
        <input
          class="backend-name"
          type="text"
          value={backend.name}
          oninput={(e) => updateBackend(i, (b) => (b.name = (e.currentTarget as HTMLInputElement).value))}
          placeholder="name"
        />
        <select
          value={backend.tier}
          onchange={(e) => updateBackend(i, (b) => (b.tier = (e.currentTarget as HTMLSelectElement).value as BackendTier))}
        >
          <option value="quality">quality</option>
          <option value="fast">fast</option>
        </select>
        <label class="checkbox inline">
          <input
            type="checkbox"
            checked={backend.enabled}
            onchange={(e) => updateBackend(i, (b) => (b.enabled = (e.currentTarget as HTMLInputElement).checked))}
          />
          <span>enabled</span>
        </label>
        {#if backend.kind.type === 'local'}
          <label class="checkbox inline">
            <input
              type="checkbox"
              checked={backend.kind.autostart}
              onchange={(e) =>
                updateBackend(i, (b) => {
                  if (b.kind.type === 'local') b.kind.autostart = (e.currentTarget as HTMLInputElement).checked;
                })}
            />
            <span>Start on launch</span>
          </label>
        {/if}
        <button type="button" class="secondary danger" onclick={() => removeBackend(i)}>Remove</button>
      </div>

      {#if statusFor(backend.name)}
        {@const st = statusFor(backend.name)!}
        <div class="offload-status">
          <span class="offload-status-label">{st.kind}:</span>
          <span class:status-error={st.state === 'error'}>{describeBackendStatus(st)} · {st.tool_scope}</span>
          {#if st.cloud_blocked}<span class="badge warn">consent required</span>{/if}
        </div>
      {/if}

      {#if backend.kind.type === 'local'}
        <hr class="card-divider lg" />
        <label>
          <span>Server command</span>
          <textarea
            class="server-command"
            rows="6"
            wrap="soft"
            value={backend.kind.server_command}
            oninput={(e) =>
              updateBackend(i, (b) => {
                if (b.kind.type === 'local') b.kind.server_command = (e.currentTarget as HTMLTextAreaElement).value;
              })}
            placeholder="llama-server --model … --port 8080 --jinja -ngl 99 --ctx-size 150000"
          ></textarea>
        </label>
        <label>
          <span>Auth token (optional)</span>
          <!-- V33: a Local backend is only "local" in the sense that
               cImp owns the process — the server still listens on a
               socket, and `--host 0.0.0.0` puts it on the LAN. Same
               `type="password"`, cleartext-on-disk treatment as the
               Remote token below; `?? ''` because a settings file
               written before V33 has no key here and an `undefined`
               must render as an empty field, never blank the card. -->
          <input
            type="password"
            value={backend.kind.auth_token ?? ''}
            oninput={(e) =>
              updateBackend(i, (b) => {
                if (b.kind.type === 'local') b.kind.auth_token = (e.currentTarget as HTMLInputElement).value;
              })}
            placeholder="Matches --api-key in the command above"
          />
          <small class="hint">
            Sent as a <code>Bearer</code> header to this server. Leave
            empty for no auth. Set it when the command above passes
            <code>--api-key</code> — the two must match, and a server
            bound to <code>--host 0.0.0.0</code> is reachable by
            anything on your LAN without one.
          </small>
        </label>
        <Toggle
          label="Show command on start"
          checked={backend.kind.show_command_on_start}
          onchange={(next) =>
            updateBackend(i, (b) => {
              if (b.kind.type === 'local')
                b.kind.show_command_on_start = next;
            })}
        />
        <small class="hint">
          The Start button in Tools → Offload server opens the
          command in an editable popup first — edits apply to that
          launch only and are not saved here.
        </small>
        <div class="button-row template-actions">
          <button
            type="button"
            class="secondary"
            class:active={templatePopup?.i === i && templatePopup?.mode === 'save'}
            onclick={() => openTemplatePopup(i, 'save')}
          >Save</button>
          <button
            type="button"
            class="secondary"
            class:active={templatePopup?.i === i && templatePopup?.mode === 'load'}
            onclick={() => openTemplatePopup(i, 'load')}
          >Load</button>
          <button
            type="button"
            class="secondary"
            class:active={templatePopup?.i === i && templatePopup?.mode === 'delete'}
            onclick={() => openTemplatePopup(i, 'delete')}
          >Delete</button>
        </div>

        {#if templatePopup?.i === i}
          {@const templates = snapshot.offload.server_command_templates}
          <div class="template-popup" role="group">
            {#if templatePopup.mode === 'save'}
              <div class="template-save">
                <input
                  type="text"
                  placeholder="Template name"
                  bind:value={newTemplateName}
                  onkeydown={(e) => {
                    if (e.key === 'Enter') commitSaveLocalTemplate(i);
                    if (e.key === 'Escape') closeTemplatePopup();
                  }}
                />
                <button type="button" onclick={() => commitSaveLocalTemplate(i)}>Save</button>
                <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
              </div>
              {#if templateError}
                <small class="error">{templateError}</small>
              {/if}
              <!-- Said out loud because the Remote popup's counterpart
                   DOES save its token, so silence here would read as
                   "the same, plus the token". -->
              <small class="hint">Saves the server command only — not the auth token.</small>
            {:else if templatePopup.mode === 'load'}
              {#if templates.length === 0}
                <small class="hint">No saved commands yet.</small>
              {:else}
                <ul class="template-list">
                  {#each templates as t (t.name)}
                    <li>
                      <span class="template-name" title={t.command}>{t.name}</span>
                      <button type="button" onclick={() => loadLocalTemplate(i, t)}>Load</button>
                    </li>
                  {/each}
                </ul>
              {/if}
              <div class="button-row">
                <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
              </div>
            {:else if templatePopup.mode === 'delete'}
              {#if templates.length === 0}
                <small class="hint">No saved commands yet.</small>
              {:else}
                <ul class="template-list">
                  {#each templates as t (t.name)}
                    <li>
                      <span class="template-name" title={t.command}>{t.name}</span>
                      <button type="button" class="danger" onclick={() => deleteLocalTemplate(t.name)}>Delete</button>
                    </li>
                  {/each}
                </ul>
              {/if}
              <div class="button-row">
                <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
              </div>
            {/if}
          </div>
        {/if}
      {:else if backend.kind.type === 'remote'}
        <hr class="card-divider lg" />
        <label>
          <span>Base URL</span>
          <input
            type="text"
            value={backend.kind.base_url}
            oninput={(e) =>
              updateBackend(i, (b) => {
                if (b.kind.type === 'remote') b.kind.base_url = (e.currentTarget as HTMLInputElement).value;
              })}
            placeholder="http://192.168.1.50:8080  or  https://api.example.com/v1"
          />
        </label>
        <label>
          <span>Auth token (optional)</span>
          <input
            type="password"
            value={backend.kind.auth_token}
            oninput={(e) =>
              updateBackend(i, (b) => {
                if (b.kind.type === 'remote') b.kind.auth_token = (e.currentTarget as HTMLInputElement).value;
              })}
            placeholder="Bearer token for cloud APIs"
          />
        </label>
        <div class="button-row template-actions">
          <button
            type="button"
            class="secondary"
            class:active={templatePopup?.i === i && templatePopup?.mode === 'save'}
            onclick={() => openTemplatePopup(i, 'save')}
          >Save</button>
          <button
            type="button"
            class="secondary"
            class:active={templatePopup?.i === i && templatePopup?.mode === 'load'}
            onclick={() => openTemplatePopup(i, 'load')}
          >Load</button>
          <button
            type="button"
            class="secondary"
            class:active={templatePopup?.i === i && templatePopup?.mode === 'delete'}
            onclick={() => openTemplatePopup(i, 'delete')}
          >Delete</button>
        </div>

        {#if templatePopup?.i === i}
          {@const templates = snapshot.offload.remote_backend_templates}
          <div class="template-popup" role="group">
            {#if templatePopup.mode === 'save'}
              <div class="template-save">
                <input
                  type="text"
                  placeholder="Template name"
                  bind:value={newTemplateName}
                  onkeydown={(e) => {
                    if (e.key === 'Enter') commitSaveRemoteTemplate(i);
                    if (e.key === 'Escape') closeTemplatePopup();
                  }}
                />
                <button type="button" onclick={() => commitSaveRemoteTemplate(i)}>Save</button>
                <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
              </div>
              {#if templateError}
                <small class="error">{templateError}</small>
              {/if}
              <small class="hint">Saves the base URL and auth token above.</small>
            {:else if templatePopup.mode === 'load'}
              {#if templates.length === 0}
                <small class="hint">No saved endpoints yet.</small>
              {:else}
                <ul class="template-list">
                  {#each templates as t (t.name)}
                    <li>
                      <span class="template-name" title={t.base_url}>{t.name}</span>
                      <span class="template-sub">{t.base_url}</span>
                      <button type="button" onclick={() => loadRemoteTemplate(i, t)}>Load</button>
                    </li>
                  {/each}
                </ul>
              {/if}
              <div class="button-row">
                <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
              </div>
            {:else if templatePopup.mode === 'delete'}
              {#if templates.length === 0}
                <small class="hint">No saved endpoints yet.</small>
              {:else}
                <ul class="template-list">
                  {#each templates as t (t.name)}
                    <li>
                      <span class="template-name" title={t.base_url}>{t.name}</span>
                      <span class="template-sub">{t.base_url}</span>
                      <button type="button" class="danger" onclick={() => deleteRemoteTemplate(t.name)}>Delete</button>
                    </li>
                  {/each}
                </ul>
              {/if}
              <div class="button-row">
                <button type="button" class="secondary" onclick={closeTemplatePopup}>Cancel</button>
              </div>
            {/if}
          </div>
        {/if}
        <hr class="card-divider lg" />
        <Toggle
          label="Cloud backend (data leaves this machine)"
          checked={backend.kind.is_cloud}
          onchange={(next) => setBackendCloud(i, next)}
        />
        {#if backend.kind.is_cloud}
          <label class="checkbox cloud-consent">
            <input
              type="checkbox"
              checked={backend.kind.cloud_consent}
              onchange={(e) =>
                updateBackend(i, (b) => {
                  if (b.kind.type === 'remote') b.kind.cloud_consent = (e.currentTarget as HTMLInputElement).checked;
                })}
            />
            <span>
              I understand: offloading to this backend sends the task text
              (and any tool results scoped in) to a third party. Unusable
              until checked.
            </span>
          </label>
        {/if}
        <NumberField
          label="Declared context (tokens, when /props is absent)"
          min="0"
          placeholder="e.g. 16000"
          value={backend.declared_context ?? ''}
          event="input"
          onchange={(next) =>
            updateBackend(i, (b) => {
              const v = next;
              const n = +v;
              // Empty / non-numeric → null (use /props), never NaN.
              b.declared_context =
                v === '' || Number.isNaN(n) ? null : Math.max(0, n);
            })}
        />
        <label>
          <span>Declared model name (when /props is absent)</span>
          <input
            type="text"
            placeholder="e.g. qwen3-32b"
            value={backend.declared_model}
            oninput={(e) =>
              updateBackend(i, (b) => {
                b.declared_model = (e.currentTarget as HTMLInputElement).value.trim();
              })}
          />
          <small class="hint">
            Cosmetic label shown for this backend when the endpoint
            doesn't report its model.
          </small>
        </label>
      {/if}

      <hr class="card-divider lg" />
      <SelectField
        label="Tool scope"
        value={scopeMode(backend.tool_scope)}
        disabled={scopeMode(backend.tool_scope) === 'custom'}
        onchange={(next) => setScopeMode(i, next as 'all' | 'web')}
      >
        <option value="all">All tools</option>
        <option value="web">Web/docs only (deny local files, code, commands, git)</option>
        {#if scopeMode(backend.tool_scope) === 'custom'}
          <option value="custom">Custom (edit in settings.json)</option>
        {/if}
        {#snippet after()}
          <small class="hint">
            Cloud backends default to web/docs only so local file contents
            never leave the machine. Widen a cloud backend only with intent.
          </small>
        {/snippet}
      </SelectField>

      {#if backend.kind.type === 'local'}
        <hr class="card-divider" />
        <!-- V40 Phase F: one block per harness that declares
             `local_provider_config` — i.e. one cImp can WRITE a provider
             block for. It was hard-coded for one harness, so a second
             one with a config writer would have had no button at all. -->
        {#each localProviderConfigHarnesses as h (h.id)}
          {@const autoKey = h.affordances.localProviderConfigAutoKey ?? ''}
          <div class="button-row">
            <button
              type="button"
              class="secondary"
              onclick={() => registerLocalProvider(i, h)}
            >Add to {h.label}</button>
            <label class="checkbox inline">
              <input
                type="checkbox"
                checked={harnessRow(snapshot, h.id).ext?.[autoKey] === true}
                onchange={(e) =>
                  patch((s) =>
                    setHarnessExt(
                      s,
                      h.id,
                      autoKey,
                      (e.currentTarget as HTMLInputElement).checked,
                    ),
                  )}
              />
              <span>Auto-sync while offload enabled</span>
            </label>
          </div>
          {#if h.affordances.localProviderConfigNote}
            <small class="hint provider-desc">{h.affordances.localProviderConfigNote}</small>
          {/if}
        {/each}
        {#if providerMsg && providerMsg.i === i}
          <small class={providerMsg.ok ? 'hint' : 'error'}>{providerMsg.text}</small>
        {/if}
        <div class="button-row offload-lifecycle-row">
          <button type="button" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendStart(backend.name))}>Start</button>
          <button type="button" class="secondary" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendStop(backend.name))}>Stop</button>
          <button type="button" class="secondary" disabled={offloadBusy} onclick={() => runOffloadAction(() => offloadBackendRestart(backend.name))}>Reset</button>
        </div>
      {/if}
    </div>
  {/each}

  <!--
    V39 Phase C — the facade backends, READ-ONLY.

    They are not in `offload.backends` and never will be: a Remote-offload
    tab IS the backend (locked decision 8), so there is exactly one place
    to change one, and it is that tab's own popover. Listing them here
    anyway is the point — a backend the router can pick but the backend
    list does not mention is a backend the user cannot account for.
  -->
  {#each facadeBackends(snapshot) as facade (facade.tabId)}
    <div class="backend-card facade">
      <div class="backend-head">
        <span class="backend-name-static" title="The name the requesting harness sees">{facade.name}</span>
        <span class="facade-kind">tab worker</span>
        <span class="facade-kind">{facade.tier}</span>
        {#if facade.declaredContext}
          <span class="facade-kind">~{Math.max(1, Math.round(facade.declaredContext / 1000))}k ctx</span>
        {/if}
      </div>
      <!--
        V39 review M-9: a name collision DROPS the facade from the pool
        (the router, the run log and the dashboard all key on the name),
        and the drop used to be a `warn!` in the log and nothing else —
        this list showed the row as if it were live. Rendered rather
        than hidden: the row is where the user can see what to rename.
      -->
      {#if facade.droppedReason}
        <small class="error">{facade.droppedReason}</small>
      {/if}
      <small class="hint">
        Configured on the tab “{facade.tabName}” — set its role, backend name,
        tier and context in that tab's ⇄ popover. It is offered to
        <code>offload_task</code> under the name above and never as a tab;
        it is ready while the tab is open and idle.
      </small>
    </div>
  {/each}

  <div class="button-row">
    <button type="button" onclick={addLocalBackend}>+ Local backend</button>
    <button type="button" onclick={addRemoteBackend}>+ Remote backend</button>
  </div>

  <hr class="card-divider lg" />
  <h3>Limits</h3>
  <NumberField
    label="Working-budget high-water (%)"
    min="10"
    max="100"
    value={snapshot.offload.budget_high_water_pct}
    onchange={(next) =>
      patch(
        (s) =>
          (s.offload.budget_high_water_pct = Math.min(
            100,
            Math.max(10, +next || 10),
          )),
      )}
  >
    <small class="hint">
      Fraction of the per-slot window the loop works against,
      reserving the rest for reasoning + the answer (~80%).
    </small>
  </NumberField>
  <NumberField
    label="Per-tool-result token cap"
    min="256"
    value={snapshot.offload.per_tool_result_token_cap}
    onchange={(next) =>
      patch(
        (s) =>
          (s.offload.per_tool_result_token_cap = Math.max(
            256,
            +next || 256,
          )),
      )}
  />
  <NumberField
    label="Max steps"
    min="1"
    value={snapshot.offload.max_steps}
    onchange={(next) =>
      patch(
        (s) =>
          (s.offload.max_steps = Math.max(
            1,
            +next || 1,
          )),
      )}
  />
  <NumberField
    label="Per-task timeout (seconds)"
    min="30"
    value={snapshot.offload.offload_timeout_secs}
    onchange={(next) =>
      patch(
        (s) =>
          (s.offload.offload_timeout_secs = Math.max(
            30,
            +next || 30,
          )),
      )}
  >
    <small class="hint">Bounds each offload, including the wait for a free slot.</small>
  </NumberField>
  <NumberField
    label="Max queue depth (blank = unlimited)"
    min="0"
    placeholder="unlimited"
    value={snapshot.offload.max_queue_depth ?? ''}
    onchange={(next) => {
      const raw = next.trim();
      const n = Math.floor(+raw);
      patch(
        (s) =>
          (s.offload.max_queue_depth =
            raw === '' || !Number.isFinite(n) || n <= 0 ? null : n),
      );
    }}
  >
    <small class="hint">
      When every slot is busy and this many tasks are already waiting,
      new offloads are rejected immediately instead of queuing. Blank
      keeps the unbounded queue (each waits up to the timeout above).
    </small>
  </NumberField>
  <NumberField
    label="Global concurrency (blank = auto)"
    min="1"
    placeholder="auto"
    value={snapshot.offload.global_concurrency ?? ''}
    onchange={(next) => {
      const raw = next.trim();
      const n = Math.floor(+raw);
      patch(
        (s) =>
          (s.offload.global_concurrency =
            raw === '' || !Number.isFinite(n) || n <= 0 ? null : n),
      );
    }}
  >
    <small class="hint">
      Cap on offload tasks in flight across the whole app. Blank
      auto-sizes from the summed per-backend slot counts.
    </small>
  </NumberField>
  <!--
    F-18: the injection controls used to sit right here, at the bottom
    of this sub-tab. This is where anyone who remembers that, or who
    read one of the pointers that named a "Tools" section, will look.
    A breadcrumb rather than an alias in the deep-link router: this
    section id is live and still means the offload pool, so aliasing it
    would hijack every legitimate link to this page.
  -->
  <hr class="card-divider lg" />
  <small class="hint">
    <strong>Injection protection moved.</strong> The master switch, the
    per-feature matrix, the external fetch budgets, native web tools and
    injection detection are now a top-level Settings category of their
    own — they govern every AI tab, not just the offload worker.
  </small>
  <div class="button-row">
    <button type="button" class="secondary" onclick={() => onnavigate('injection')}>
      Open Injection protection
    </button>
  </div>

  <Toggle
    label="Escalate partial fast-tier answers to the quality backend"
    checked={snapshot.offload.escalate_partial}
    onchange={(next) => patch((s) => (s.offload.escalate_partial = next))}
  />
  <small class="hint">
    When a fast-tier offload comes back only partially verified, re-run it
    once on a distinct, ready quality backend and keep the better answer.
    Inert unless a second, quality-tier backend is configured.
  </small>
  {:else}
  <h3>Native tools</h3>
  <Toggle
    label="read_file — bounded file reads"
    checked={snapshot.offload.tools.read_file}
    onchange={(next) => patch((s) => (s.offload.tools.read_file = next))}
  />
  <Toggle
    label="list_dir — enumerate a directory (what files exist / how many)"
    checked={snapshot.offload.tools.list_dir}
    onchange={(next) => patch((s) => (s.offload.tools.list_dir = next))}
  />
  <Toggle
    label="code_search — literal search across the roots"
    checked={snapshot.offload.tools.code_search}
    onchange={(next) => patch((s) => (s.offload.tools.code_search = next))}
  />
  <Toggle
    label="run_command — allowlisted, read-only commands"
    checked={snapshot.offload.tools.run_command}
    onchange={(next) => patch((s) => (s.offload.tools.run_command = next))}
  />
  <Toggle
    checked={snapshot.offload.tools.run_check}
    onchange={(next) => patch((s) => (s.offload.tools.run_check = next))}
  >
    run_check — run a configured project check (build/typecheck/lint/test).
    Inert until the project's <code>checks</code> are configured.
  </Toggle>

  <label>
    <span>Allowed roots (one per line)</span>
    <textarea
      rows="3"
      value={snapshot.offload.allowed_roots.join('\n')}
      oninput={(e) =>
        patch(
          (s) =>
            (s.offload.allowed_roots = (e.currentTarget as HTMLTextAreaElement).value
              .split('\n')
              .map((r) => r.trim())
              .filter((r) => r.length > 0)),
        )}
      placeholder="Leave empty to confine to the launch project root"
    ></textarea>
    <small class="hint">
      <code>code_search</code>/<code>read_file</code>/<code>run_command</code>
      are confined to these. Empty = the launch project root.
    </small>
  </label>
  <label>
    <span>Command allowlist (comma-separated)</span>
    <input
      type="text"
      value={snapshot.offload.command_allowlist.join(', ')}
      oninput={(e) =>
        patch(
          (s) =>
            (s.offload.command_allowlist = (e.currentTarget as HTMLInputElement).value
              .split(',')
              .map((c) => c.trim())
              .filter((c) => c.length > 0)),
        )}
      placeholder="git, cargo"
    />
    <small class="hint">
      <code>run_command</code> runs nothing unless its program is
      listed here (deny by default).
    </small>
  </label>

  <div class="button-row">
    <button type="button" class="secondary" onclick={onenablereadonly}>
      Enable safe read-only commands
    </button>
  </div>
  <small class="hint">
    Adds <code>git</code> and <code>cargo</code> to the allowlist and
    installs a <code>cargo</code> policy that permits only
    <code>metadata</code> / <code>tree</code> (never
    <code>run</code>/<code>build</code>). A one-time merge — it never
    overwrites your own entries, and you can prune anything it adds below.
  </small>

  {#if snapshot.offload.command_allowlist.length > 0}
    <ul class="policy-status">
      {#each snapshot.offload.command_allowlist as prog (prog)}
        {@const pol = policyForProgram(prog)}
        <li>
          <code>{prog}</code>
          {#if pol}
            <span class="hardened">✓ hardened by policy</span>
          {:else}
            <span class="unguarded">— no extra guards (allowlist + bare-name only)</span>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}

  <h3>Command security policies</h3>
  <small class="hint top">
    Per-program hardening layered on top of the allowlist:
    <code>run_command</code> refuses the listed flags/subcommands and
    forces the listed environment variables at spawn. <code>program</code>
    matches an allowlisted command by name (file-stem, case-insensitive).
    The default <code>git</code> policy blocks the config-injection and
    root-escape vectors and neutralizes the pager/ssh hooks. You can edit
    or remove any policy — weakening one can reopen an
    arbitrary-code-execution path, so change with care.
  </small>
  {#each snapshot.offload.command_policies as policy, i (i)}
    <fieldset class="policy-card">
      <div class="policy-head">
        <label class="policy-program">
          <span>Program</span>
          <input
            type="text"
            value={policy.program}
            oninput={(e) =>
              updatePolicy(i, (p) => (p.program = (e.currentTarget as HTMLInputElement).value.trim()))}
            placeholder="git"
          />
        </label>
        <button type="button" class="secondary danger" onclick={() => removeCommandPolicy(i)}>
          Remove
        </button>
      </div>
      <label>
        <span>Denied flags (comma-separated)</span>
        <input
          type="text"
          value={policy.denied_flags.join(', ')}
          oninput={(e) =>
            updatePolicy(i, (p) => (p.denied_flags = csvToList((e.currentTarget as HTMLInputElement).value)))}
          placeholder="-c, --git-dir, --work-tree"
        />
      </label>
      <label>
        <span>Denied subcommands (comma-separated)</span>
        <input
          type="text"
          value={policy.denied_subcommands.join(', ')}
          oninput={(e) =>
            updatePolicy(i, (p) => (p.denied_subcommands = csvToList((e.currentTarget as HTMLInputElement).value)))}
          placeholder="config"
        />
      </label>
      <label>
        <span>Allowed subcommands (comma-separated)</span>
        <input
          type="text"
          value={policy.allowed_subcommands.join(', ')}
          oninput={(e) =>
            updatePolicy(i, (p) => (p.allowed_subcommands = csvToList((e.currentTarget as HTMLInputElement).value)))}
          placeholder="metadata, tree"
        />
        <small class="hint">
          When set, ONLY these subcommands may run — every other, and a
          bare invocation, is refused. Leave empty to allow all except
          the denied ones.
        </small>
      </label>
      <div class="policy-env">
        <span class="policy-env-label">Spawn environment (forced)</span>
        {#each policy.env as ev, j (j)}
          <div class="env-row">
            <input
              type="text"
              value={ev.key}
              oninput={(e) =>
                updatePolicy(i, (p) => (p.env[j].key = (e.currentTarget as HTMLInputElement).value))}
              placeholder="GIT_PAGER"
            />
            <input
              type="text"
              value={ev.value}
              oninput={(e) =>
                updatePolicy(i, (p) => (p.env[j].value = (e.currentTarget as HTMLInputElement).value))}
              placeholder="cat"
            />
            <!-- `icon` opts out of the TUI themes' `[ … ]` bracket
                 framing — brackets around a lone × wrap it tall. -->
            <button
              type="button"
              class="secondary icon"
              aria-label="Remove environment variable"
              onclick={() => updatePolicy(i, (p) => (p.env = p.env.filter((_, idx) => idx !== j)))}
            >
              ×
            </button>
          </div>
        {/each}
        <div class="button-row">
          <button
            type="button"
            class="secondary"
            onclick={() => updatePolicy(i, (p) => (p.env = [...p.env, { key: '', value: '' }]))}
          >
            Add env var
          </button>
        </div>
      </div>
    </fieldset>
  {/each}
  <div class="button-row">
    <button type="button" onclick={addCommandPolicy}>Add command policy</button>
  </div>

  <!-- #148: the "Cross-harness delegation" block that used to close this
       sub-tab is now its own sidebar category (`DelegationSection.svelte`).
       Only those two knobs moved — the FACADE backends a remote-offload worker
       advertises are backend-pool entries and stay in the Pool sub-tab above,
       where the rest of the pool is. -->
  {/if}

  <hr class="card-divider lg" />
  <label>
    <span>Test offload</span>
    <input
      type="text"
      value={testInput}
      oninput={(e) => ontestinput((e.currentTarget as HTMLInputElement).value)}
      placeholder="Leave empty for a canned reachability check, or type a task…"
    />
    <div class="button-row">
      <button type="button" disabled={offloadBusy} onclick={runOffloadTest}>
        Run test
      </button>
    </div>
    {#if testResult}
      <pre class="offload-test-result">{testResult}</pre>
    {/if}
  </label>
  <small class="hint">
    Watch the local model load + server logs live in the
    <strong>Tools</strong> tab's <strong>Offload server</strong>
    section.
  </small>
</section>

<style>
  /* …and 2 blank lines between that description and the Start/Stop/Reset
     lifecycle row, which now sits at the bottom of the card. Overrides the
     default `.button-row` top margin. */
  .button-row.offload-lifecycle-row {
    margin-top: 3rem;
  }
  .offload-status {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    margin: 0.6rem 0 0.2rem;
  }
  .offload-status-label {
    font-weight: 600;
    color: var(--text-secondary);
  }
  /* Per-MCP-server health readout */
  .status-error {
    color: var(--danger, #d08770);
    font-weight: 600;
  }
  .offload-test-result {
    margin-top: 0.5rem;
    max-height: 16rem;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
    background: var(--surface-sunken);
    border: 1px solid var(--border-subtle);
    border-radius: 4px;
    padding: 0.5rem;
    font-size: var(--font-size-sm);
  }
  /* Command security policies (Tools sub-tab) */
  .policy-status {
    list-style: none;
    margin: 0.25rem 0 0.75rem;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    font-size: var(--font-size-sm);
  }
  .policy-status li {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
  }
  .policy-status .hardened {
    color: var(--accent, #6abf69);
  }
  .policy-status .unguarded {
    color: var(--text-quiet, #999);
  }
  .policy-card {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.75rem;
    margin-bottom: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .policy-head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 0.5rem;
  }
  .policy-program {
    flex: 1;
  }
  .policy-env-label {
    display: block;
    font-size: var(--font-size-sm);
    margin-bottom: 0.25rem;
  }
  .env-row {
    display: flex;
    gap: 0.4rem;
    margin-bottom: 0.4rem;
  }
  .env-row input {
    flex: 1;
  }
  /* V8-02 backend pool editor */
  .backend-card {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.6rem 0.75rem;
    margin: 0.6rem 0;
    background: var(--surface-sunken);
  }
  /* V39 Phase C: a facade is read-only here, and it should look it. */
  .backend-card.facade {
    border-style: dashed;
  }
  .backend-name-static {
    font-weight: 600;
  }
  .facade-kind {
    font-size: 0.8em;
    opacity: 0.75;
    border: 1px solid var(--border-subtle);
    border-radius: 4px;
    padding: 0 0.35rem;
  }
  .backend-head {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    flex-wrap: wrap;
    margin-bottom: 0.3rem;
  }
  .backend-name {
    flex: 1 1 8rem;
    min-width: 6rem;
    font-weight: 600;
  }
  .checkbox.inline {
    margin: 0;
  }
  .cloud-consent {
    border-left: 3px solid var(--accent, #d08770);
    padding-left: 0.5rem;
  }
  /* Multiline, word-wrapping Server command field so every argument of a long
     llama-server invocation stays visible without horizontal scrolling. */
  textarea.server-command {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    min-height: 7.8rem;
    font-family: var(--font-mono, monospace);
    font-size: var(--font-size-sm);
    line-height: 1.4;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .template-actions {
    margin-top: 0.35rem;
  }
  .template-popup {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md, 6px);
    padding: 0.5rem 0.6rem;
    margin: 0.4rem 0 0.2rem;
    background: var(--surface-1, var(--surface-sunken));
  }
  .template-save {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    flex-wrap: wrap;
  }
  .template-save input[type='text'] {
    flex: 1 1 10rem;
    min-width: 8rem;
  }

  /* ORDER-PRESERVATION, now written down. #129 (a) parked this in
     `settings-chrome.css` because it ties at (0,3,1) with that sheet's
     `.button-row + small.hint` reset and its single call site sits directly
     after a `.button-row` — in the old single style block the adjacency rule
     came last and WON there, so the hint gets `--space-1`, not 1.5rem. A
     child's CSS is emitted after the chrome sheet, so moving the rule here
     would silently flip that. The exception is therefore stated rather than
     left to source order; the computed value at the one call site is
     unchanged. Whether 1.5rem was ever meant to apply is a separate question
     from this refactor. */
  small.hint.provider-desc {
    margin-top: 1.5rem;
  }
  .button-row + small.hint.provider-desc {
    margin-top: var(--space-1);
  }
</style>
