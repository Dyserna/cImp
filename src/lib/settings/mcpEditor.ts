// V37 Phase D (contract C8) — pure decision logic for the MCP management
// editor, extracted so it can be unit-tested without a Svelte/Tauri host (the
// frontend test infra is plain vitest, no component testing). Precedent:
// `checksEditor.ts` / `checksEditor.test.ts`. `McpManagementEditor.svelte`
// stays presentational and delegates grouping, naming, the override tri-state
// and the effective-state predicate to the functions here.
//
// **The enforcement owner is Rust.** `offload::mcp_host::effective_enable` is
// the single source of truth for "does this server exist right now" — it is
// what advertisement and dispatch both read (contract C3/C4). The port below is
// a *display* mirror: it exists so a row can show the user the state the
// backend will act on, and its test is a verbatim port of the Rust truth table
// so the mirror cannot drift into a second, disagreeing rule. If the two ever
// differ, Rust wins and this file is the bug.

import type { McpActivation, McpCategory, McpOrigin, McpServerConfig } from './types';

/// The three registry fields the editor owns, travelling together. Everything
/// here lives under `Settings.offload`; the editor never sees the rest of the
/// settings tree, and the parent is what folds a returned registry back into
/// the snapshot it persists.
///
/// Scope, which the UI must keep visible: `servers` and `categories` are
/// **global** registry state, while `activation` is the ONLY per-project
/// surface (contract C2 — it is a pair of maps precisely so a project overlay
/// composes key-by-key instead of replacing the list wholesale).
export interface McpRegistry {
  servers: McpServerConfig[];
  categories: McpCategory[];
  activation: McpActivation;
}

/// V37 contract C3 — the verdict of the effective-enable predicate, mirroring
/// the Rust `EnableVerdict`. Not a boolean, for the same reason it is not one
/// in Rust: "you turned this server off" and "the category it sits in is off"
/// are different user mistakes with different fixes, and the row has to say
/// which.
export type EnableVerdict =
  | { kind: 'enabled' }
  | { kind: 'server-off' }
  /// Carries the FIRST containing category in registry order — with several
  /// off categories any of them is a true answer, and registry order makes the
  /// choice deterministic (and equal to Rust's).
  | { kind: 'categories-off'; category: string };

/// An activation map lookup that distinguishes **absent** (inherit the global
/// flag) from **present-and-false** (an override that turns it off). A plain
/// `map[key] ?? fallback` reads the same for both only by luck of `false`
/// not being nullish; spelling it out is what keeps "empty is not absent"
/// honest here, and it is the primitive the revert action is defined against.
export function lookupOverride(
  map: Record<string, boolean>,
  key: string,
): boolean | undefined {
  return Object.prototype.hasOwnProperty.call(map, key) ? map[key] : undefined;
}

/// V37 contract C3 — the effective-enable predicate, ported from Rust
/// `offload::mcp_host::effective_enable` for display only (see the file header).
///
/// ```text
/// enabled(server) :=
///   (server.enabled, overridden by activation.servers[name] if present)
///   AND ( no category contains the server
///         OR at least one containing category is effectively enabled,
///            where a category's effective state = category.enabled
///            overridden by activation.categories[name] if present )
/// ```
///
/// Uncategorized servers ride the server toggle alone; one enabled category is
/// enough (categories OR, they do not AND); the server toggle wins outright.
export function effectiveEnable(
  server: McpServerConfig,
  categories: McpCategory[],
  activation: McpActivation,
): EnableVerdict {
  const serverOn = lookupOverride(activation.servers, server.name) ?? server.enabled;
  if (!serverOn) return { kind: 'server-off' };
  let firstContaining: string | null = null;
  for (const c of categories) {
    if (!c.servers.includes(server.name)) continue;
    const on = lookupOverride(activation.categories, c.name) ?? c.enabled;
    if (on) return { kind: 'enabled' };
    if (firstContaining === null) firstContaining = c.name;
  }
  // At least one category contains it and none of them is on …
  if (firstContaining !== null) return { kind: 'categories-off', category: firstContaining };
  // … otherwise it is uncategorized and the server toggle (already checked) is
  // the whole rule.
  return { kind: 'enabled' };
}

/// Boolean shorthand over [`effectiveEnable`], mirroring Rust `server_enabled`.
export function isServerEnabled(
  server: McpServerConfig,
  categories: McpCategory[],
  activation: McpActivation,
): boolean {
  return effectiveEnable(server, categories, activation).kind === 'enabled';
}

/// A neutral activation map — no overrides at all. Used to compute the GLOBAL
/// verdict of a row (what every other project sees) beside its project-composed
/// one, so the UI can say "on globally, off here" rather than just "off".
export function noActivation(): McpActivation {
  return { categories: {}, servers: {} };
}

/// The effective state of one category: its global flag, overridden by a
/// project activation entry when one is present.
export function categoryEnabled(category: McpCategory, activation: McpActivation): boolean {
  return lookupOverride(activation.categories, category.name) ?? category.enabled;
}

/// One-line label for a verdict, for the row's effective-state chip.
export function describeVerdict(v: EnableVerdict): string {
  switch (v.kind) {
    case 'enabled':
      return 'Active';
    case 'server-off':
      return 'Off — server toggle';
    case 'categories-off':
      return `Off — category "${v.category}" is off`;
  }
}

/// The three states of a per-project override. `inherit` is **absence of the
/// key**, not a value: reverting deletes the entry so the project follows the
/// global flag from then on, including through later global changes. Writing
/// the current global value instead would freeze the project at today's value
/// and silently stop inheriting — the bug this tri-state exists to prevent.
export type OverrideState = 'inherit' | 'on' | 'off';

/// Which of the three states a key is in right now.
export function overrideState(map: Record<string, boolean>, key: string): OverrideState {
  const v = lookupOverride(map, key);
  if (v === undefined) return 'inherit';
  return v ? 'on' : 'off';
}

/// A copy of `map` with `key` moved to `state`. `inherit` DELETES the key —
/// see [`OverrideState`].
export function withOverride(
  map: Record<string, boolean>,
  key: string,
  state: OverrideState,
): Record<string, boolean> {
  const next: Record<string, boolean> = { ...map };
  if (state === 'inherit') delete next[key];
  else next[key] = state === 'on';
  return next;
}

/// `base`, or `base-2` / `base-3` / … — the first name not already taken.
/// Generalized from `SettingsApp.svelte`'s `uniqueMcpName` so servers and
/// categories share one rule: in both namespaces the name IS the id (contract
/// C1), so a duplicate is not a cosmetic problem but two rows claiming one
/// identity.
export function uniqueName(base: string, taken: Iterable<string>): string {
  const names = new Set(taken);
  if (!names.has(base)) return base;
  let i = 2;
  while (names.has(`${base}-${i}`)) i++;
  return `${base}-${i}`;
}

/// Fallback name for a category whose field was cleared.
export const CATEGORY_FALLBACK_NAME = 'category';

/// Resolve what a category rename should actually commit to: trimmed, never
/// empty, never colliding with another category. Applied on blur (not per
/// keystroke), so typing a name that passes through a prefix of an existing one
/// is not punished mid-word.
///
/// `selfIndex` is the row being renamed — its own current name is not a
/// collision with itself.
export function resolveCategoryName(
  desired: string,
  categories: McpCategory[],
  selfIndex: number,
): string {
  const base = desired.trim() === '' ? CATEGORY_FALLBACK_NAME : desired.trim();
  const taken = categories
    .filter((_, i) => i !== selfIndex)
    .map((c) => c.name.trim());
  return uniqueName(base, taken);
}

/// A fresh category, named so it does not collide with an existing one.
/// Enabled by default: a category the user just made and cannot see the effect
/// of is a worse default than one that changes nothing until servers join it.
export function newCategory(categories: McpCategory[]): McpCategory {
  return {
    name: uniqueName(CATEGORY_FALLBACK_NAME, categories.map((c) => c.name)),
    servers: [],
    enabled: true,
  };
}

/// A fresh server row. Same defaults as the pre-V37 `addMcpServer`: an HTTP
/// endpoint with an empty URL (shows "down" until filled), offload-only
/// exposure, plus the two V37 fields — a hand-added row is an external endpoint
/// and it exists.
export function newServer(servers: McpServerConfig[]): McpServerConfig {
  return {
    name: uniqueName('server', servers.map((s) => s.name)),
    command: '',
    args: [],
    env: {},
    url: '',
    claude_access: false,
    offload_access: true,
    opencode_access: false,
    auth_token: '',
    origin: 'external',
    enabled: true,
  };
}

/// One server, as the editor renders it.
export interface McpServerRow {
  server: McpServerConfig;
  /// Index into `McpRegistry.servers`. Rows are keyed by it, not by name: the
  /// name is an editable text field and a name key would change mid-typing and
  /// drop input focus (the reason the pre-V37 editor keyed by index too).
  index: number;
  /// Every category containing the server, in registry order.
  categories: string[];
  /// The project-composed C3 verdict — what this project's tabs get.
  verdict: EnableVerdict;
  /// The same verdict with no project overrides at all — what every OTHER
  /// project gets. Equal to `verdict` unless an activation entry applies.
  globalVerdict: EnableVerdict;
}

/// Servers grouped for display. A group is one category plus the rows it owns.
export interface McpServerGroup {
  /// `null` for the uncategorized group, which always renders LAST.
  category: McpCategory | null;
  rows: McpServerRow[];
}

/// Group servers by category for display, uncategorized last.
///
/// A server in several categories is listed **once**, under the first category
/// containing it in registry order — the same "first containing category" the
/// C3 predicate names in a `categories-off` verdict, so the row the user reads
/// and the category the refusal blames are the same one. Its other memberships
/// travel on the row (`McpServerRow.categories`) and the editor shows them; the
/// alternative, repeating a full editable row under every category, would give
/// one server two live copies of every text field.
///
/// Empty categories are still listed: a category with no members is exactly the
/// state a user is in halfway through creating one.
export function groupServers(reg: McpRegistry): McpServerGroup[] {
  const neutral = noActivation();
  const groups: McpServerGroup[] = reg.categories.map((c) => ({ category: c, rows: [] }));
  const uncategorized: McpServerRow[] = [];
  reg.servers.forEach((server, index) => {
    const containing = reg.categories
      .filter((c) => c.servers.includes(server.name))
      .map((c) => c.name);
    const row: McpServerRow = {
      server,
      index,
      categories: containing,
      verdict: effectiveEnable(server, reg.categories, reg.activation),
      globalVerdict: effectiveEnable(server, reg.categories, neutral),
    };
    if (containing.length === 0) {
      uncategorized.push(row);
      return;
    }
    const primary = groups.find((g) => g.category?.name === containing[0]);
    // `primary` is always found (the name came from this same list), but the
    // fallback keeps a server from vanishing from the UI if that ever stops
    // holding — an invisible server is not something the user can fix.
    if (primary) primary.rows.push(row);
    else uncategorized.push(row);
  });
  groups.push({ category: null, rows: uncategorized });
  return groups;
}

/// Add or remove one server's membership in one category. Membership is edited
/// on the SERVER row (a server can be in several categories, and this is where
/// the user is looking at the one they mean); the category block owns the
/// category's own name and toggles.
export function withMembership(
  categories: McpCategory[],
  categoryName: string,
  serverName: string,
  member: boolean,
): McpCategory[] {
  return categories.map((c) => {
    if (c.name !== categoryName) return c;
    const has = c.servers.includes(serverName);
    if (member === has) return c;
    return {
      ...c,
      servers: member ? [...c.servers, serverName] : c.servers.filter((s) => s !== serverName),
    };
  });
}

/// References that name something no longer in the registry.
///
/// Contract C1 makes a rename a NEW identity, so renaming a server or a
/// category leaves entries behind that point at the old name. Rust treats them
/// as inert (they simply never match), which is right — but inert is not the
/// same as invisible: a user who renamed a category and wonders why their
/// project override stopped applying needs to see the stale key, not guess. The
/// editor surfaces this and offers one action to clear it; nothing is pruned
/// behind the user's back.
export interface McpStaleRefs {
  /// `activation.servers` keys naming no server.
  activationServers: string[];
  /// `activation.categories` keys naming no category.
  activationCategories: string[];
  /// Category membership entries naming no server.
  members: { category: string; servers: string[] }[];
}

/// Whether a stale-reference report has anything in it.
export function hasStaleRefs(s: McpStaleRefs): boolean {
  return (
    s.activationServers.length > 0 ||
    s.activationCategories.length > 0 ||
    s.members.length > 0
  );
}

/// Find every reference that names something that no longer exists.
export function staleRefs(reg: McpRegistry): McpStaleRefs {
  const serverNames = new Set(reg.servers.map((s) => s.name));
  const categoryNames = new Set(reg.categories.map((c) => c.name));
  const members = reg.categories
    .map((c) => ({ category: c.name, servers: c.servers.filter((s) => !serverNames.has(s)) }))
    .filter((m) => m.servers.length > 0);
  return {
    activationServers: Object.keys(reg.activation.servers).filter((k) => !serverNames.has(k)),
    activationCategories: Object.keys(reg.activation.categories).filter(
      (k) => !categoryNames.has(k),
    ),
    members,
  };
}

/// A copy of the registry with every stale reference dropped. User-invoked
/// only — see [`McpStaleRefs`].
export function clearStaleRefs(reg: McpRegistry): McpRegistry {
  const serverNames = new Set(reg.servers.map((s) => s.name));
  const categoryNames = new Set(reg.categories.map((c) => c.name));
  const next = cloneRegistry(reg);
  next.categories = next.categories.map((c) => ({
    ...c,
    servers: c.servers.filter((s) => serverNames.has(s)),
  }));
  for (const k of Object.keys(next.activation.servers)) {
    if (!serverNames.has(k)) delete next.activation.servers[k];
  }
  for (const k of Object.keys(next.activation.categories)) {
    if (!categoryNames.has(k)) delete next.activation.categories[k];
  }
  return next;
}

/// A deep-enough copy for editing: every array and map the editor mutates gets
/// its own instance. Hand-written rather than `structuredClone` because the
/// values arriving here are Svelte `$state` proxies, which `structuredClone`
/// refuses; reading through a proxy into fresh plain objects does not.
export function cloneRegistry(reg: McpRegistry): McpRegistry {
  return {
    servers: reg.servers.map((s) => ({ ...s, args: [...s.args], env: { ...s.env } })),
    categories: reg.categories.map((c) => ({ ...c, servers: [...c.servers] })),
    activation: {
      categories: { ...reg.activation.categories },
      servers: { ...reg.activation.servers },
    },
  };
}

/// Badge text for a server's provenance (contract C2). `internal` is reserved
/// for servers cImp itself manages (#41); anything the user pastes in is
/// external, which is also what a pre-v32 entry defaults to.
export function originLabel(origin: McpOrigin): string {
  return origin === 'internal' ? 'internal' : 'external';
}

/// How many of the registry's servers are effectively enabled in this project —
/// the "N of M" summary above the list.
export function enabledCount(reg: McpRegistry): number {
  return reg.servers.filter((s) => isServerEnabled(s, reg.categories, reg.activation)).length;
}
