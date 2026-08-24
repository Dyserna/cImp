<script lang="ts">
  /// Settings → MCP servers (#129 (c)). A thin frame around
  /// `McpManagementEditor` — the heading, the prose that names the harnesses,
  /// and the three-callback wiring V37 Phase D fixed as contract C8.
  ///
  /// The persistence seam stays in `SettingsApp.svelte` on purpose:
  /// `setMcpRegistry` writes the local snapshot only, and `applyMcpRegistry`
  /// does ONE awaited `settings_update` then ONE `offload_reload_mcp` under
  /// draftSync's push gate. Both are handed down as callbacks, so this
  /// component still never touches the store — the same rule `patch()` obeys.
  import type { Settings } from '../types';
  import type { McpRegistry } from '../mcpEditor';
  import type { McpServerHealth } from '../../offload';
  import McpManagementEditor from '../McpManagementEditor.svelte';

  let {
    snapshot,
    patch,
    harnessNamesProse,
    health,
    onedit,
    onapply,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// The enabled harnesses, in prose. Parent-owned.
    harnessNamesProse: string;
    /// Live health of the warm MCP host, from the parent's backend-status poll.
    /// A prop rather than a fetch here: `serviceStatus` is refreshed by that
    /// poll AND by `offload_reload_mcp`, and two owners of one status is how it
    /// goes stale.
    health: McpServerHealth[];
    /// Local-snapshot-only edit (text fields commit on blur, never per
    /// keystroke — per-keystroke pushes raced).
    onedit: (next: McpRegistry) => void;
    /// Awaited settings push followed by the warm-host reconcile.
    onapply: (next: McpRegistry) => Promise<void>;
  } = $props();
</script>

<section>
  <h2>MCP servers</h2>
  <small class="hint top">
    Model Context Protocol servers cImp connects to and keeps warm. Each
    server's read-class tools (web search, fetch, docs, …) can be exposed
    to <strong>{harnessNamesProse}</strong> and/or to the
    <strong>offload worker</strong> — per server, below.
    Write/destructive tools are filtered out. Exposing a server to a
    harness works whether or not offload is enabled.
  </small>
  <McpManagementEditor
    servers={snapshot.offload.mcp_servers}
    categories={snapshot.offload.mcp_categories}
    activation={snapshot.offload.mcp_activation}
    {health}
    healthIntervalSecs={snapshot.offload.mcp_health_interval_secs}
    {onedit}
    {onapply}
    onhealthinterval={(secs) =>
      patch((s) => (s.offload.mcp_health_interval_secs = secs))}
  />
</section>
