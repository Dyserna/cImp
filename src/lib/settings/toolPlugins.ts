// V38 Phase B: extractable logic for the Tool Plugins settings section.
//
// Same split, and the same reason, as `codeAudit.ts`: Svelte components are not
// unit-tested in this repo, so everything that DECIDES something — row building,
// the category tri-state, path scope, the permission summary, and every write
// into the settings container — lives here where Vitest can exercise it, and the
// component is left rendering what it is handed.
//
// The writes matter most. "Toggle a category" and "revert this path to the
// global one" are the two operations a hand-written component gets subtly wrong
// (a category toggle that also clears per-tool flags; a revert that writes an
// empty string instead of deleting the key), and both are one-line assertions
// here.

import type { PluginState, Settings, ToolState } from './types';

// ── The `plugins_snapshot` DTO ──────────────────────────────────────────────
//
// Mirrors the Rust `plugins::loader::PluginSet` as it serializes. Kept here
// rather than in `types.ts` because it is an IPC result, not a settings shape —
// `types.ts` mirrors what is stored on disk, and mixing the two would blur which
// of the file's interfaces a migration has to care about.

export type PluginToolKind = 'audit' | 'security' | 'check' | 'command';
export type PluginRuntimeReq =
  | 'none'
  | 'python'
  | 'node'
  | 'java'
  | 'dotnet'
  | 'go'
  | 'rust'
  | 'auto';
export type PluginSandboxReq = 'required' | 'optional' | 'unsupported';

/// One declared, user-settable variable. These declarations ARE the fields the
/// section renders (decision 10) — the pane never invents an input.
export interface PluginVariableDecl {
  name: string;
  label: string;
  default: string | null;
}

/// V38 Phase F — a tier-2 tool's provider: the MCP server that answers for it,
/// instead of a binary cImp spawns. `null` on every tier-1 tool, which is all of
/// them by default.
export interface PluginProviderRef {
  server: string;
  tool: string;
}

export interface PluginToolManifest {
  id: string;
  label: string;
  /// One line saying what the tool is for, shown beside the label.
  description: string | null;
  kind: PluginToolKind;
  /// Whether the tool is on before the user has ever touched it. `false` is for
  /// a tool its author knows is expensive or intrusive enough that nobody
  /// should get it by accident.
  enabled_by_default: boolean;
  runtime: PluginRuntimeReq;
  sandbox: PluginSandboxReq;
  extra_grants: string[];
  variables: PluginVariableDecl[];
  parameters_allowed: boolean;
  timeout_secs: number | null;
  env: [string, string][];
  argv: string[];
  transport: 'stdout' | 'report_file' | null;
  findings_exit_codes: number[];
  applicability: { extensions: string[]; markers: string[] };
  /// Tier 2 (§ 4.5): findings come from this MCP server, and nothing is spawned.
  provider: PluginProviderRef | null;
  cmd: string | null;
  cwd: string | null;
  report_file: string | null;
  pattern: string | null;
  parser: string | null;
  /// Built-in only (a scanned file carrying any of these is refused). See
  /// `plugins::manifest` for why each one is a relaxation cImp reserves for the
  /// definitions it ships.
  ingest: string | null;
  /// The bare command name a BUILT-IN resolves through `ebin` then `PATH` when
  /// no path is configured. `null` for a user plugin, which cImp never resolves
  /// a binary for.
  command: string | null;
  project_local_bin: string | null;
  dir_argv: string[];
}

export interface PluginCategoryDecl {
  id: string;
  label: string;
  tools: string[];
}

export interface PluginManifest {
  manifest_version: number;
  name: string;
  version: string;
  label: string | null;
  description: string | null;
  categories: PluginCategoryDecl[];
  tools: PluginToolManifest[];
}

export interface LoadedPlugin {
  path: string;
  provenance: 'user' | 'builtin';
  key: string;
  manifest: PluginManifest;
}

export type PluginErrorKind = 'io' | 'invalid' | 'conflict';

export interface PluginLoadError {
  kind: PluginErrorKind;
  paths: string[];
  key: string | null;
  reason: string;
}

export interface PluginSet {
  plugins: LoadedPlugin[];
  errors: PluginLoadError[];
  dir: string;
  scanned_at_ms: number;
  scan_ms: number;
}

// ── Rows the section renders ────────────────────────────────────────────────

export type PathScope = 'project' | 'global' | 'unset';

/// A tool's binary path, split so the pane can show BOTH values at once — the
/// machine-wide one and this project's override — rather than one box whose
/// meaning depends on a chip beside it.
export interface ToolPathRow {
  /// What the tool actually resolves to. Empty ⇒ inert.
  effective: string;
  scope: PathScope;
  /// The machine-wide entry ('' when unset).
  global: string;
  /// This project's override; `null` when the project inherits.
  project: string | null;
}

export interface ToolVariableRow {
  name: string;
  label: string;
  /// The manifest's default, shown as the input's placeholder. `null` = the
  /// tool has no value until the user supplies one, which is a different state
  /// from an empty-string default.
  fallback: string | null;
  /// The user's value ('' = not set, so the default applies).
  value: string;
}

export interface ToolRow {
  toolKey: string;
  id: string;
  label: string;
  kind: PluginToolKind;
  /// One line saying what the tool is for, from its manifest.
  description: string | null;
  /// The tool's OWN flag — what the checkbox binds to.
  enabled: boolean;
  /// `plugin.enabled && tool.enabled` — what actually happens.
  effectiveEnabled: boolean;
  path: ToolPathRow;
  timeoutSecs: number | null;
  parametersAllowed: boolean;
  parameters: string[];
  variables: ToolVariableRow[];
  /// The read-only "this tool asks for:" lines — see `permissionSummary`.
  permissions: string[];
  /// The tool's sandbox declaration. Rendered nowhere on its own; it decides
  /// whether the permission list starts OPEN (see `permissionsOpen`), because
  /// anything other than `required` is the alarming ask and must not be the
  /// one that arrives collapsed.
  sandbox: PluginSandboxReq;
  /// Whether cImp will find this tool without a configured path — true only for
  /// a built-in whose manifest names a command. It changes what the empty path
  /// box MEANS ("resolve normally" against "this tool does not run"), which is
  /// the difference between a working default and an invisible one.
  resolvesByName: boolean;
  /// V38 Phase F: set for a TIER-2 tool. The pane renders the server it calls
  /// where a tier-1 tool renders its two path boxes — a provider tool has no
  /// binary, so an empty path input beside it would be an instruction the user
  /// cannot follow.
  provider: PluginProviderRef | null;
}

export type CategoryToggleState = 'on' | 'off' | 'mixed';

export interface CategoryRow {
  id: string;
  label: string;
  tools: ToolRow[];
  /// Derived from the members, never stored: a category is a management view,
  /// so persisting its state would create a second source of truth that could
  /// disagree with the tools it claims to describe.
  state: CategoryToggleState;
}

export interface PluginRow {
  key: string;
  /// cImp's own, stamped by the loader. Rendered as a badge, and the reason the
  /// pane never offers to delete the file: there is no file.
  builtin: boolean;
  /// `name (version)` — decision 9's display form. Two versions of one plugin
  /// coexist, so the version is part of the name, not a detail.
  label: string;
  manifestPath: string;
  description: string | null;
  enabled: boolean;
  categories: CategoryRow[];
  toolCount: number;
}

/// A manifest that did NOT load, rendered in the same list as an error state —
/// the settings pane is where a user goes to fix this, so a rejected file has to
/// be visible there and not only in the Events feed.
export interface PluginErrorRow {
  kind: PluginErrorKind;
  /// The identity when the file had one, else the file name(s): a file whose
  /// JSON did not parse has no trustworthy name and we do not invent one.
  label: string;
  paths: string[];
  /// Verbatim the same string the `plugin` Events row carries, so a user
  /// comparing the two is not left wondering whether they describe one problem.
  reason: string;
}

// ── Building the rows ───────────────────────────────────────────────────────

const EMPTY_STATE: PluginState = { enabled: true, tools: {} };

function toolState(plugin: PluginState | undefined, toolId: string): ToolState | undefined {
  return plugin?.tools?.[toolId];
}

/// The plugin key cImp's own fourteen audit scanners live under.
///
/// A mirror of Rust's `plugins::builtin::AUDIT_PLUGIN_KEY`, pinned by
/// `builtin_audit_tool_ids_are_mirrored_in_the_frontend_union`. It is a
/// SETTINGS key — the v33 → v34 migration writes these exact strings — so the
/// version in it is the identity of the shipped set, not the cImp release.
export const AUDIT_PLUGIN_KEY = 'cimp-audit@1';

export function toolKeyOf(pluginKey: string, toolId: string): string {
  return `${pluginKey}/${toolId}`;
}

/// The tool's path, resolved the way `plugins::registry` resolves it: this
/// project's entry, else the machine-wide entry, else unset. A whitespace-only
/// value is NOT a path — it is what a cleared input leaves behind.
export function toolPath(
  settings: Settings,
  projectKey: string,
  toolKey: string,
): ToolPathRow {
  const store = settings.tool_plugins;
  const global = store?.global_paths?.[toolKey] ?? '';
  const raw = projectKey ? store?.project_paths?.[projectKey]?.[toolKey] : undefined;
  const project = raw === undefined ? null : raw;
  if (project !== null && project.trim() !== '') {
    return { effective: project, scope: 'project', global, project };
  }
  if (global.trim() !== '') return { effective: global, scope: 'global', global, project };
  return { effective: '', scope: 'unset', global, project };
}

/// What a tool would be granted if it ran — rendered beside its enable toggle,
/// read-only, in the user's language rather than the manifest's.
///
/// The phone-app pattern, and deliberately only the SHOWING half: the screening
/// that refuses a credential directory is `sandbox::extra_grant_refusal` at
/// spawn time (Phase C). A summary that implied it had already vetted these
/// would be worse than none, so it says what was *asked for*, not what will be
/// allowed.
export function permissionSummary(tool: PluginToolManifest): string[] {
  const out: string[] = [];
  // Tier 2 asks for something else entirely, and saying "to run inside the OS
  // sandbox" about a tool that never runs here would be the worst kind of
  // reassuring. The trust it asks for is the SERVER's — see § 4.5's statement.
  if (tool.provider) {
    out.push(
      `to call the MCP server "${tool.provider.server}" you configured (its "${tool.provider.tool}" tool) — nothing runs on this machine`,
    );
    out.push("to read that server's answer as findings, which means trusting the server");
    return out;
  }
  if (tool.sandbox === 'unsupported') {
    out.push('to run OUTSIDE the OS sandbox (this tool declares it cannot be confined)');
  } else if (tool.sandbox === 'optional') {
    out.push('to run outside the OS sandbox if it cannot be confined (degraded, with a visible event)');
  } else {
    out.push('to run inside the OS sandbox (it refuses to run unconfined)');
  }
  if (tool.runtime === 'none') {
    out.push('no interpreter — a single binary, its own directory is the whole grant');
  } else if (tool.runtime !== 'auto') {
    out.push(`the ${tool.runtime} runtime's files`);
  }
  for (const grant of tool.extra_grants) {
    out.push(`access to ${grant}`);
  }
  if (tool.env.length > 0) {
    out.push(`these environment variables set: ${tool.env.map(([k]) => k).join(', ')}`);
  }
  return out;
}

/// Whether a tool's "this tool asks for…" list renders expanded.
///
/// Two triggers, and the second is the one the Phase B review added (D-1): a
/// list with more than one line is worth opening, AND any sandbox declaration
/// other than `required` is worth opening on its own — `unsupported` (runs
/// outside the boundary) is the single most alarming thing a manifest can ask
/// for, and as a one-line summary it used to arrive collapsed.
export function permissionsOpen(tool: {
  permissions: string[];
  sandbox: PluginSandboxReq;
}): boolean {
  return tool.permissions.length > 1 || tool.sandbox !== 'required';
}

/// A category's toggle state, derived from its members. `mixed` renders
/// indeterminate; an empty category (which validation forbids) reads as off
/// rather than as a checked box describing nothing.
export function categoryState(tools: ToolRow[]): CategoryToggleState {
  if (tools.length === 0) return 'off';
  const on = tools.filter((t) => t.enabled).length;
  if (on === 0) return 'off';
  if (on === tools.length) return 'on';
  return 'mixed';
}

/// Every loaded plugin as a row, in the order the backend returned (sorted by
/// key), each with its categories in manifest order.
export function pluginRows(set: PluginSet, settings: Settings, projectKey: string): PluginRow[] {
  return set.plugins.map((p) => {
    const state = settings.tool_plugins?.plugins?.[p.key] ?? EMPTY_STATE;
    const pluginEnabled = state.enabled ?? true;
    const byId = new Map(p.manifest.tools.map((t) => [t.id, t]));

    const categories: CategoryRow[] = p.manifest.categories.map((c) => {
      const tools: ToolRow[] = [];
      for (const id of c.tools) {
        const t = byId.get(id);
        if (!t) continue; // Validation forbids it; rendering nothing is honest.
        const ts = toolState(state, id);
        // No stored state ⇒ the MANIFEST's default, which is how the two
        // built-in heavyweights stay off on a fresh install.
        const enabled = ts?.enabled ?? t.enabled_by_default ?? true;
        tools.push({
          toolKey: toolKeyOf(p.key, id),
          id,
          label: t.label,
          description: t.description ?? null,
          kind: t.kind,
          enabled,
          effectiveEnabled: pluginEnabled && enabled,
          path: toolPath(settings, projectKey, toolKeyOf(p.key, id)),
          timeoutSecs: ts?.timeout_secs ?? null,
          parametersAllowed: t.parameters_allowed,
          parameters: t.parameters_allowed ? (ts?.parameters ?? []) : [],
          variables: t.variables.map((v) => ({
            name: v.name,
            label: v.label,
            fallback: v.default,
            value: ts?.variables?.[v.name] ?? '',
          })),
          permissions: permissionSummary(t),
          sandbox: t.sandbox,
          resolvesByName: p.provenance === 'builtin' && !!t.command,
          provider: t.provider ?? null,
        });
      }
      return { id: c.id, label: c.label, tools, state: categoryState(tools) };
    });

    return {
      key: p.key,
      builtin: p.provenance === 'builtin',
      label: `${p.manifest.label ?? p.manifest.name} (${p.manifest.version})`,
      manifestPath: p.path,
      description: p.manifest.description,
      enabled: pluginEnabled,
      categories,
      toolCount: categories.reduce((n, c) => n + c.tools.length, 0),
    };
  });
}

export function errorRows(set: PluginSet): PluginErrorRow[] {
  return set.errors.map((e) => ({
    kind: e.kind,
    label: e.key ?? e.paths.map(fileName).join(' + '),
    paths: e.paths,
    reason: e.reason,
  }));
}

function fileName(p: string): string {
  const parts = p.split(/[\\/]/);
  return parts[parts.length - 1] || p;
}

// ── Writes into the settings container ──────────────────────────────────────
//
// Every one of these MUTATES a draft `Settings` in place, because that is the
// shape the component's `patch()` helper hands them. They create the container
// entries they need and never delete a plugin key: state outlives its plugin's
// file on purpose (see the Rust `ToolPluginsSettings` docs).

/// Materialize the container before writing into it. The backend always sends
/// it (it is `#[serde(default)]` on `Settings`), but a Settings window opened
/// against an older backend would otherwise throw on the first click rather
/// than degrade — and an empty container is exactly the right fallback.
function container(settings: Settings): Settings['tool_plugins'] {
  return (settings.tool_plugins ??= { plugins: {}, project_paths: {}, global_paths: {} });
}

function pluginEntry(settings: Settings, pluginKey: string): PluginState {
  const map = (container(settings).plugins ??= {});
  return (map[pluginKey] ??= { enabled: true, tools: {} });
}

function toolEntry(settings: Settings, pluginKey: string, toolId: string): ToolState {
  const p = pluginEntry(settings, pluginKey);
  p.tools ??= {};
  return (p.tools[toolId] ??= {
    enabled: true,
    timeout_secs: null,
    parameters: [],
    variables: {},
  });
}

export function setPluginEnabled(settings: Settings, pluginKey: string, on: boolean): void {
  pluginEntry(settings, pluginKey).enabled = on;
}

export function setToolEnabled(
  settings: Settings,
  pluginKey: string,
  toolId: string,
  on: boolean,
): void {
  toolEntry(settings, pluginKey, toolId).enabled = on;
}

/// Toggling a category toggles its member tools as a unit. The category itself
/// stores nothing — it is a view — so this writes only the per-tool flags, which
/// is also what makes the tri-state re-derive correctly afterwards.
export function setCategoryEnabled(
  settings: Settings,
  pluginKey: string,
  category: CategoryRow,
  on: boolean,
): void {
  for (const t of category.tools) setToolEnabled(settings, pluginKey, t.id, on);
}

export function setToolTimeout(
  settings: Settings,
  pluginKey: string,
  toolId: string,
  secs: number | null,
): void {
  toolEntry(settings, pluginKey, toolId).timeout_secs = secs;
}

export function setToolParameters(
  settings: Settings,
  pluginKey: string,
  toolId: string,
  parameters: string[],
): void {
  toolEntry(settings, pluginKey, toolId).parameters = parameters;
}

/// Set (or clear) one declared variable. An empty value DELETES the entry
/// rather than storing `''`: "unset, so the manifest's default applies" and
/// "explicitly the empty string" would otherwise be the same stored shape, and
/// the first is what clearing an input means.
export function setToolVariable(
  settings: Settings,
  pluginKey: string,
  toolId: string,
  name: string,
  value: string,
): void {
  const t = toolEntry(settings, pluginKey, toolId);
  t.variables ??= {};
  if (value === '') delete t.variables[name];
  else t.variables[name] = value;
}

/// Write the machine-wide path for a tool. Empty clears the entry.
export function setGlobalPath(settings: Settings, toolKey: string, path: string): void {
  const map = (container(settings).global_paths ??= {});
  if (path.trim() === '') delete map[toolKey];
  else map[toolKey] = path;
}

/// Override the path for THIS project. Empty clears the override, which is not
/// the same as setting it to '' — an override of '' would read as "no path" and
/// make the tool inert here instead of inheriting the machine-wide value.
export function setProjectPath(
  settings: Settings,
  projectKey: string,
  toolKey: string,
  path: string,
): void {
  if (!projectKey) return;
  const byProject = (container(settings).project_paths ??= {});
  const map = (byProject[projectKey] ??= {});
  if (path.trim() === '') {
    delete map[toolKey];
    if (Object.keys(map).length === 0) delete byProject[projectKey];
  } else {
    map[toolKey] = path;
  }
}

/// "Use the machine-wide path" — drop this project's override entirely.
export function revertToGlobalPath(
  settings: Settings,
  projectKey: string,
  toolKey: string,
): void {
  setProjectPath(settings, projectKey, toolKey, '');
}
