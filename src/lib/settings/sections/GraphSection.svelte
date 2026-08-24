<script lang="ts">
  /// Settings → Code Intelligence (#129 (c)) — the code graph, semantic search,
  /// token efficiency and the graph visualiser, behind four sub-tabs.
  ///
  /// **`commitGraphIgnore` is parent-routed, and here is the audit the issue
  /// asked for.** It is the window's ONE direct `applySettings` caller: the
  /// ignore list is edited in place through `ArrayEditor`'s bind and pushed on
  /// commit boundaries (blur / Enter / row-remove) rather than per keystroke,
  /// because a per-keystroke push fires a graph index resync per character. It
  /// stays in `SettingsApp` — it needs `snapshot` and the store, and this
  /// component has neither — and reaches this section as `oncommitignore`.
  ///
  /// Recorded while auditing it, NOT changed here: that push does not register
  /// with `draftSync.beginPush()`, which every `patch()` does. A
  /// `settings-changed` broadcast landing during the push can therefore replace
  /// the draft — the lost-update race draftSync exists to close. Making it take
  /// the gate is a behaviour change, so it is reported rather than smuggled
  /// into a refactor.
  ///
  /// Everything else here writes through `patch()`.
  import { harnesses } from '../../harness';
  import type { GraphStatus } from '../../graph';
  import type { Settings } from '../types';
  import type { CapabilityGate } from '../types';
  import ArrayEditor from '../ArrayEditor.svelte';
  import NumberField from '../NumberField.svelte';
  import SelectField from '../SelectField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    statuses,
    busy,
    localOffloadReady,
    e1Gate,
    onrefresh,
    onrebuild,
    onignorepick,
    oncommitignore,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push).
    patch: (updater: (s: Settings) => void) => void;
    /// Per-project graph index status, from the parent's `graph_status`.
    statuses: GraphStatus[];
    /// A rebuild is in flight.
    busy: boolean;
    /// V16 Feature 6: a LOCAL offload backend is ready. The digest path is
    /// local-only by design, so turning `context_llm_digests` ON is gated on
    /// it; turning it off is always allowed. Derived from the parent's
    /// backend-status poll — one poll, one owner.
    localOffloadReady: boolean;
    /// The read-advisor capability gate, or `null` when nothing blocks it.
    /// Read off the same `harness_versions_get` payload the Harness health
    /// section renders, which is why it is the parent's.
    e1Gate: CapabilityGate | null;
    /// Re-read `graph_status`.
    onrefresh: () => void;
    /// Rebuild the index (the parent polls status while it runs).
    onrebuild: () => void;
    /// Native picker → project-relative glob, appended and committed in one
    /// step. `true` picks a folder.
    onignorepick: (folder: boolean) => void;
    /// Push the in-place-edited ignore list. See the note above.
    oncommitignore: () => void;
  } = $props();

  const e1Blocked = $derived(e1Gate !== null);

  /// How each harness receives an injected prompt. Derived from the registry
  /// store rather than passed down: nothing else in the window read it.
  const injectMechanisms = $derived(
    $harnesses
      .filter((h) => h.affordances.injectMechanism)
      .map((h) => `${h.label} via ${h.affordances.injectMechanism}`)
      .join(', '),
  );

  // Sub-tab nav within this section. Local: no deep link targets it and
  // nothing outside reads it.
  type GraphSubSection = 'graph' | 'semantic' | 'efficiency' | 'viz';
  let graphSubSection = $state<GraphSubSection>('graph');
</script>

<section>
  <h2>Code Intelligence</h2>
  <small class="hint top">
    Build a per-project graph of your code and docs (symbols, calls,
    imports, doc-comments), stored at
    <code>&lt;project&gt;/.cimp/graph.db</code> and kept live by a file
    watcher. The harness session queries it through
    <code>graph_*</code> tools (re-launch a tab to pick them up) instead
    of grepping. Off by default; everything stays on this machine.
  </small>
  <Toggle
    label="Enable code graph"
    checked={snapshot.graph.enabled}
    onchange={(next) => patch((s) => (s.graph.enabled = next))}
  />

  {#if snapshot.graph.enabled}
    <hr class="card-divider lg" />
    <div class="sub-tabs" role="tablist" aria-label="Code Intelligence sub-sections">
      <button
        type="button"
        role="tab"
        class:active={graphSubSection === 'graph'}
        aria-selected={graphSubSection === 'graph'}
        onclick={() => (graphSubSection = 'graph')}
      >
        Code graph
      </button>
      <button
        type="button"
        role="tab"
        class:active={graphSubSection === 'semantic'}
        aria-selected={graphSubSection === 'semantic'}
        onclick={() => (graphSubSection = 'semantic')}
      >
        Semantic search
      </button>
      <button
        type="button"
        role="tab"
        class:active={graphSubSection === 'efficiency'}
        aria-selected={graphSubSection === 'efficiency'}
        onclick={() => (graphSubSection = 'efficiency')}
      >
        Token efficiency
      </button>
      <button
        type="button"
        role="tab"
        class:active={graphSubSection === 'viz'}
        aria-selected={graphSubSection === 'viz'}
        onclick={() => (graphSubSection = 'viz')}
      >
        Graph view
      </button>
    </div>

    {#if graphSubSection === 'graph'}
    <div class="button-row">
      <button type="button" disabled={busy} onclick={onrebuild}>
        {busy ? 'Rebuilding…' : 'Rebuild index'}
      </button>
      <button type="button" class="secondary" disabled={busy} onclick={onrefresh}>
        Refresh status
      </button>
    </div>
    {#if statuses.length === 0}
      <small class="hint">No index built yet — click <strong>Rebuild index</strong>.</small>
    {:else}
      {#each statuses as gs (gs.root)}
        <small class="hint">
          <strong>{gs.state}</strong> · {gs.files} files · {gs.symbols} symbols ·
          {gs.edges} edges
          {#if gs.last_error}<br />Error: {gs.last_error}{/if}
        </small>
      {/each}
    {/if}

    <h3>Indexing</h3>
    <label>
      <span>Languages (comma-separated)</span>
      <input
        type="text"
        value={snapshot.graph.languages.join(', ')}
        onchange={(e) =>
          patch(
            (s) =>
              (s.graph.languages = (e.currentTarget as HTMLInputElement).value
                .split(',')
                .map((x) => x.trim().toLowerCase())
                .filter((x) => x.length > 0)),
          )}
      />
    </label>
    <small class="hint">
      Full symbol + call graph: <code>rust</code>, <code>typescript</code>,
      <code>javascript</code>, <code>python</code>, <code>go</code>,
      <code>java</code>, <code>c</code>, <code>cpp</code>, <code>csharp</code>,
      <code>php</code>, <code>bash</code>, <code>scala</code>, <code>ocaml</code>,
      <code>ruby</code>, <code>haskell</code>, <code>kotlin</code>,
      <code>swift</code>, <code>sql</code>, <code>erlang</code>, <code>r</code>,
      <code>perl</code>, <code>ada</code>. Docs: <code>markdown</code>.
      Struct-search only (add to enable): <code>html</code>, <code>css</code>,
      <code>json</code>, <code>yaml</code>, <code>xml</code>, <code>asm</code>.
    </small>
    <Toggle
      label="Index Markdown docs + doc-comments (powers doc search)"
      checked={snapshot.graph.index_docs}
      onchange={(next) => patch((s) => (s.graph.index_docs = next))}
    />
    <NumberField
      label="Max file size (bytes)"
      min="1024"
      value={snapshot.graph.max_file_bytes}
      onchange={(next) =>
        patch(
          (s) =>
            (s.graph.max_file_bytes = Math.max(
              1024,
              Number(next) || 1048576,
            )),
        )}
    />
    <NumberField
      label="Watcher debounce (ms)"
      min="50"
      value={snapshot.graph.watch_debounce_ms}
      onchange={(next) =>
        patch(
          (s) =>
            (s.graph.watch_debounce_ms = Math.max(
              50,
              Number(next) || 300,
            )),
        )}
    />

    <h3>Ignored files & folders</h3>
    <small class="hint">
      Gitignore-style globs, relative to the project root (e.g.
      <code>/docs/generated/</code>, <code>*.snap</code>,
      <code>!keep-this.md</code>). Applied on top of your
      <code>.gitignore</code>. Changes take effect immediately: newly
      ignored files are dropped from the index, un-ignored ones are
      indexed.
    </small>
    <ArrayEditor
      bind:items={snapshot.graph.ignore}
      placeholder="e.g. /vendor/ or *.gen.ts"
      oncommit={oncommitignore}
    />
    <div class="button-row">
      <button type="button" class="secondary" onclick={() => onignorepick(false)}>
        Add file…
      </button>
      <button type="button" class="secondary" onclick={() => onignorepick(true)}>
        Add folder…
      </button>
    </div>

    <h3>Tool surface</h3>
    <Toggle
      label="Lean tool surface (hide cold-tail graph tools)"
      checked={snapshot.graph.lean_tools}
      onchange={(next) => patch((s) => (s.graph.lean_tools = next))}
    />
    <small class="hint">
      Drop <code>graph_cycles</code>, <code>graph_dead_exports</code>,
      <code>graph_struct_search</code>, <code>graph_path</code>, and
      <code>graph_architecture</code> from the tool list advertised to the
      cloud session and the offload worker — trimming the descriptors
      cache-written once per session. Advertisement-only: each hidden tool
      still answers if an agent calls it by name. The Code Intelligence tab
      shows the current surface size.
    </small>

    <h3>Architecture &amp; path tracing</h3>
    <small class="hint">
      Tune V15's code-intelligence features: <code>graph_path</code>
      (shortest-path tracing), <code>graph_architecture</code> (god
      nodes, subsystems, surprising edges), and the live Graph view
      (Tools tab).
      Edge confidence (extracted/inferred/ambiguous) is always on.
    </small>
    <NumberField
      label="Path tracing max hops (1–32)"
      min="1"
      max="32"
      value={snapshot.graph.path_max_hops}
      onchange={(next) =>
        patch(
          (s) =>
            (s.graph.path_max_hops = Math.min(
              32,
              Math.max(1, Number(next) || 8),
            )),
        )}
    />
    <NumberField
      label="Max subsystems reported"
      min="1"
      value={snapshot.graph.arch_max_communities}
      onchange={(next) =>
        patch(
          (s) =>
            (s.graph.arch_max_communities = Math.max(
              1,
              Number(next) || 12,
            )),
        )}
    />
    <NumberField
      label="Minimum subsystem size"
      min="1"
      value={snapshot.graph.arch_min_community_size}
      onchange={(next) =>
        patch(
          (s) =>
            (s.graph.arch_min_community_size = Math.max(
              1,
              Number(next) || 3,
            )),
        )}
    />

    <h3>Offload worker access</h3>
    <Toggle
      checked={snapshot.graph.allow_remote_worker_access}
      onchange={(next) => patch((s) => (s.graph.allow_remote_worker_access = next))}
    >
      Allow a <strong>remote</strong> offload worker to query the graph
    </Toggle>
    <small class="hint">
      ⚠ <strong>Privacy:</strong> the local offload worker can always
      query the graph. A <strong>remote</strong> backend — whether a box
      on your LAN or a public cloud API — would receive your project's
      code structure (symbol names, call relationships, doc snippets).
      Leave this off unless you trust the remote. A harness tab's own
      <code>graph_*</code> tools are unaffected by this
      setting.
    </small>
    {:else if graphSubSection === 'semantic'}
    <h3>Semantic search</h3>
    <Toggle
      label="Enable semantic (embedding) doc search"
      checked={snapshot.graph.semantic_search}
      onchange={(next) => patch((s) => (s.graph.semantic_search = next))}
    />
    <small class="hint">
      Needs an OpenAI-compatible <code>/v1/embeddings</code> endpoint
      (e.g. a <code>llama-server --embedding</code> on a spare GPU box).
      Degrades to full-text search when the endpoint is unreachable; the
      structural graph never depends on it. Toggling this changes the
      tools and guidance an AI tab sees — restart AI tabs
      to pick it up.
    </small>
    <h3>Embedding server</h3>
      <label>
        <span>Embedding endpoint</span>
        <input
          type="text"
          placeholder="http://host:8081"
          value={snapshot.graph.embedding_endpoint}
          onchange={(e) =>
            patch(
              (s) =>
                (s.graph.embedding_endpoint = (
                  e.currentTarget as HTMLInputElement
                ).value.trim()),
            )}
        />
      </label>
      <label>
        <span>Auth token (optional)</span>
        <!-- V33: the embedding server is usually a llama-server on a
             spare box, i.e. on the LAN. `?? ''` guards the pre-V33
             settings file that has no such key. Not `.trim()`-ed on
             write like the endpoint above: a token is opaque and
             trimming it would silently alter a credential. -->
        <input
          type="password"
          value={snapshot.graph.embedding_auth_token ?? ''}
          onchange={(e) =>
            patch(
              (s) =>
                (s.graph.embedding_auth_token = (
                  e.currentTarget as HTMLInputElement
                ).value),
            )}
        />
        <small class="hint">
          Sent as a <code>Bearer</code> header to the endpoint above.
          Leave empty for no auth. Stored cleartext in
          <code>settings.json</code>.
        </small>
      </label>
      <label>
        <span>Embedding model</span>
        <input
          type="text"
          placeholder="nomic-embed-text"
          value={snapshot.graph.embedding_model}
          onchange={(e) =>
            patch(
              (s) =>
                (s.graph.embedding_model = (
                  e.currentTarget as HTMLInputElement
                ).value.trim()),
            )}
        />
      </label>
      <NumberField
        label="Embedding dimensions (0 = auto-probe)"
        min="0"
        value={snapshot.graph.embedding_dims}
        onchange={(next) =>
          patch(
            (s) =>
              (s.graph.embedding_dims = Math.max(
                0,
                Number(next) || 0,
              )),
          )}
      />
      <NumberField
        label="Embedding max tokens (0 = auto-detect)"
        min="0"
        value={snapshot.graph.embedding_max_tokens}
        onchange={(next) =>
          patch(
            (s) =>
              (s.graph.embedding_max_tokens = Math.max(
                0,
                Number(next) || 0,
              )),
          )}
      />
      <small class="hint">
        0 = auto-detect from the server (a <code>llama-server</code>
        reports its context window on <code>/props</code>). Longer texts
        are truncated to fit before they're sent — without this, one
        oversized chunk makes the endpoint reject the whole batch. Set it
        manually only for a server that exposes no <code>/props</code>.
      </small>
      <small class="hint">
        Changing the model or dimensions starts a background re-embed.
        Use <strong>Rebuild embeddings</strong> in Tools →
        Graph index after a silent model swap behind the same name.
      </small>
    {:else if graphSubSection === 'efficiency'}
    <h3>Context injection</h3>
    <Toggle
      label="Auto-inject relevant file digests into each prompt"
      checked={snapshot.graph.context_injection}
      onchange={(next) => patch((s) => (s.graph.context_injection = next))}
    />
    <small class="hint">
      Prepends a budget-bounded digest of the most relevant files to each
      prompt ({injectMechanisms}). Off by default — it
      changes what the agent sees. Re-launch a tab to pick it up. Tune and
      preview it on the <strong>Context</strong> section of the Code
      Intelligence tab.
    </small>
    {#if snapshot.graph.context_injection}
      <NumberField
        label="Per-file budget (chars)"
        min="100"
        value={snapshot.graph.context_per_file_chars}
        onchange={(next) =>
          patch(
            (s) =>
              (s.graph.context_per_file_chars = Math.max(
                100,
                Number(next) || 800,
              )),
          )}
      />
      <NumberField
        label="Per-turn budget (chars)"
        min="500"
        value={snapshot.graph.context_turn_budget_chars}
        onchange={(next) =>
          patch(
            (s) =>
              (s.graph.context_turn_budget_chars = Math.max(
                500,
                Number(next) || 6000,
              )),
          )}
      />
      <NumberField
        label="Min relevance score (skip below)"
        min="0"
        value={snapshot.graph.context_min_score}
        onchange={(next) =>
          patch((s) => {
            // 0 is a valid value (no threshold), so keep it — a bare
            // `|| 3` would treat the falsy 0 as "unset" and revert it.
            const n = Number(next);
            s.graph.context_min_score = Number.isFinite(n) ? Math.max(0, n) : 3;
          })}
      />
      <Toggle
        label="Rank session-hot files first (from Memory)"
        checked={snapshot.graph.context_include_session}
        onchange={(next) => patch((s) => (s.graph.context_include_session = next))}
      />
      <NumberField
        label="Dedup TTL (turns, 0 = re-inject every turn)"
        min="0"
        value={snapshot.graph.context_dedup_ttl_turns}
        onchange={(next) =>
          patch((s) => {
            // 0 is a valid value (dedup off), so keep it — a bare
            // `|| 10` would treat the falsy 0 as "unset" and revert it.
            const n = Number(next);
            s.graph.context_dedup_ttl_turns = Number.isFinite(n) ? Math.max(0, n) : 10;
          })}
      />
      <small class="hint">
        A file injected in full is demoted to a one-line "unchanged"
        reminder on later turns until it changes or this many turns pass.
      </small>
      <Toggle
        label="Prepend the project map to each new session's first turn"
        checked={snapshot.graph.repo_map_on_session_start}
        onchange={(next) => patch((s) => (s.graph.repo_map_on_session_start = next))}
      />
      <Toggle
        label="Feed working set + pinned notes to the harness's compactor"
        checked={snapshot.graph.compaction_context}
        onchange={(next) => patch((s) => (s.graph.compaction_context = next))}
      />
      <small class="hint">
        On compaction (<code>PreCompact</code> hook) the session's working
        set and pinned notes are handed to the summarizer so they survive.
        Costs a few hundred chars once per compaction. Re-launch the tab to
        pick up changes.
      </small>
    {/if}

    <Toggle
      label="Redundant-read advisor"
      checked={snapshot.graph.read_advisor}
      disabled={e1Blocked}
      onchange={(next) => patch((s) => (s.graph.read_advisor = next))}
    />
    {#if e1Gate}
      <!--
        V35 Phase E: the sentence comes from the gate itself
        (`harness::contract::gate`) rather than being written out again
        here. The rule and the explanation of the rule were two things
        to keep in sync; now the code that decides is the code that
        says why, and a new gate arrives with its own wording.
      -->
      <small class="hint">Blocked: {e1Gate.reason}</small>
    {:else}
      <small class="hint">
        Intercepts a <code>Read</code> of a file already read unchanged this
        session and answers with a cheap outline reminder instead of
        re-reading it. Changes the agent's tool behaviour — strictly opt-in.
        It needs a harness that can deny a tool call before it runs, so it
        reaches only tabs whose harness declares that. Re-launch the tab to
        pick it up.
      </small>
    {/if}
    {#if snapshot.graph.read_advisor && !e1Blocked}
      <NumberField
        label="Min file size to advise (lines)"
        min="0"
        value={snapshot.graph.read_advisor_min_lines}
        onchange={(next) =>
          patch(
            (s) =>
              (s.graph.read_advisor_min_lines = Math.max(
                0,
                Number(next) || 300,
              )),
          )}
      />
      <small class="hint">
        Files with fewer lines than this always pass — a small file is
        cheap to re-read; the reminder isn't worth it.
      </small>
      <SelectField
        label="Reminder mode"
        value={snapshot.graph.read_advisor_mode}
        onchange={(next) => patch((s) => (s.graph.read_advisor_mode = next))}
      >
        <option value="advise">Advise — outline reminder only</option>
        <option value="substitute">Substitute — outline + most relevant symbol body</option>
      </SelectField>
      <NumberField
        label="Trust TTL (retrieve turns, 0 = whole session)"
        min="0"
        value={snapshot.graph.read_advisor_ttl_turns}
        onchange={(next) =>
          patch((s) => {
            // 0 is a valid value (TTL off), so keep it — a bare
            // `|| 0` happens to coincide here, but stay explicit.
            const n = Number(next);
            s.graph.read_advisor_ttl_turns = Number.isFinite(n) ? Math.max(0, n) : 0;
          })}
      />
      <small class="hint">
        After this many retrieval turns since the advisor last saw the
        file read in full, a <code>Read</code> passes again — bounds how
        long the agent's memory is trusted across context loss the
        advisor can't observe (context editing, tool-result truncation).
      </small>
      <Toggle
        label="Diff-substitute changed-file re-reads"
        checked={snapshot.graph.read_advisor_diffs}
        onchange={(next) => patch((s) => (s.graph.read_advisor_diffs = next))}
      />
      <small class="hint">
        When you re-read a file <em>after it changed</em>, answer with a
        line-level unified diff against what you last read instead of the
        whole file — exact, so it's safe on the edit-then-verify loop.
        Falls back to a normal read when no snapshot survives or the diff
        would be more than half the new file.
      </small>
      <Toggle
        label="Intercept whole-file shell reads"
        checked={snapshot.graph.read_advisor_shell}
        onchange={(next) => patch((s) => (s.graph.read_advisor_shell = next))}
      />
      <small class="hint">
        Also advise on a whole-file shell read
        (<code>cat</code>, <code>Get-Content</code>, <code>type</code>,
        <code>gc</code>) of an already-read file, the same as a
        <code>Read</code>. Strict — only a provable whole-file read of one
        file is intercepted; anything with a pipe, redirect, glob, second
        path, or a partial-read verb (<code>sed</code>, <code>head</code>)
        runs untouched. Installs a second hook matcher — re-launch the
        tab to pick it up.
      </small>
      <NumberField
        label="First-read digest tier (KiB, 0 = off)"
        min="0"
        value={snapshot.graph.read_advisor_first_read_kb}
        onchange={(next) =>
          patch((s) => {
            const n = Number(next);
            s.graph.read_advisor_first_read_kb = Number.isFinite(n)
              ? Math.max(0, Math.trunc(n))
              : 0;
          })}
      />
      <small class="hint">
        Answer the <em>first</em> read of a large non-code file (log,
        lockfile, generated JSON, data dump) at or above this size with the
        cached local-model digest plus a head/tail sample instead of the
        full content. Source files (anything with a parsed outline) never
        qualify, and a sliced <code>Read</code> always passes. Needs a
        cached digest — the first encounter enqueues one and passes, so
        protection begins on the next. Off by default; try <code>256</code>.
      </small>
    {/if}
    <Toggle
      label="Local-model digests for outline-poor files"
      checked={snapshot.graph.context_llm_digests}
      disabled={!snapshot.graph.context_llm_digests && !localOffloadReady}
      onchange={(next) => patch((s) => (s.graph.context_llm_digests = next))}
    />
    <small class="hint">
      For files with no useful outline (docs, configs, long scripts), the
      <strong>local</strong> offload backend writes a 3-line semantic
      digest, cached in <code>graph.db</code>. Needs a ready local offload
      backend; never leaves this machine.
      {#if !localOffloadReady}
        <strong>No local offload backend is ready</strong> — start one in
        Settings → Offload task tools to enable this.
      {/if}
    </small>

    {:else if graphSubSection === 'viz'}
    <Toggle
      checked={snapshot.graph.graph_viz}
      onchange={(next) => patch((s) => (s.graph.graph_viz = next))}
    >
      Enable the <strong>Graph view</strong> (live 3D force graph)
    </Toggle>
    <small class="hint">
      Draws the code graph and pulses nodes as agents read/edit/query
      the codebase, in the Tools tab's "Graph view" section.
      Off by default — it's a human-facing visual, not on any agent
      path.
    </small>
    {#if snapshot.graph.graph_viz}
      <NumberField
        label="Max rendered nodes"
        min="50"
        value={snapshot.graph.graph_viz_max_nodes}
        onchange={(next) =>
          patch(
            (s) =>
              (s.graph.graph_viz_max_nodes = Math.max(
                50,
                Number(next) || 1500,
              )),
          )}
      />
      <h3>Graph view tuning</h3>
      <small class="hint">
        Multipliers on the built-in layout/appearance (1.0 = default;
        0.2–5, folder spacing up to 50). One size doesn't fit every
        repo — a dense monorepo usually wants smaller nodes and wider
        spacing than a small project. Changes apply live to an open
        Graph view.
      </small>
      {#each [
        { key: 'graph_viz_node_scale', label: 'File node size', max: 5 },
        { key: 'graph_viz_dir_scale', label: 'Folder cluster size', max: 5 },
        { key: 'graph_viz_edge_width', label: 'Edge line width', max: 5 },
        { key: 'graph_viz_node_spacing', label: 'Spacing between files', max: 5 },
        { key: 'graph_viz_cluster_spacing', label: 'Spacing between folders', max: 50 },
        { key: 'graph_viz_cluster_strength', label: 'Folder grouping tightness', max: 5 },
      ] as knob (knob.key)}
        <NumberField
          label={knob.label}
          min="0.2"
          max={knob.max}
          step="0.1"
          value={(snapshot.graph as unknown as Record<string, number>)[knob.key]}
          onchange={(next) =>
            patch(
              (s) =>
                ((s.graph as unknown as Record<string, number>)[knob.key] = Math.min(
                  knob.max,
                  Math.max(0.2, Number(next) || 1),
                )),
            )}
        />
      {/each}
      <div class="row">
        <label>
          <span>Call edge color</span>
          <input
            type="color"
            value={snapshot.graph.graph_viz_color_call}
            onchange={(e) =>
              patch(
                (s) => (s.graph.graph_viz_color_call = (e.currentTarget as HTMLInputElement).value),
              )}
          />
        </label>
        <label>
          <span>Import edge color</span>
          <input
            type="color"
            value={snapshot.graph.graph_viz_color_import}
            onchange={(e) =>
              patch(
                (s) => (s.graph.graph_viz_color_import = (e.currentTarget as HTMLInputElement).value),
              )}
          />
        </label>
      </div>
    {/if}
    {/if}
  {/if}
</section>
