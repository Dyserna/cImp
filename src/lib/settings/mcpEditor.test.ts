import { describe, expect, test } from 'vitest';

import type { McpActivation, McpCategory, McpServerConfig } from './types';
import type { McpRegistry } from './mcpEditor';
import {
  CATEGORY_FALLBACK_NAME,
  clearStaleRefs,
  cloneRegistry,
  describeVerdict,
  effectiveEnable,
  enabledCount,
  groupServers,
  hasStaleRefs,
  isServerEnabled,
  lookupOverride,
  newCategory,
  newServer,
  noActivation,
  originLabel,
  overrideState,
  resolveCategoryName,
  staleRefs,
  uniqueName,
  withMembership,
  withOverride,
} from './mcpEditor';

// The three fixture builders below mirror the Rust ones in
// `offload/mcp_host.rs`'s test module (`cfg`, `category`, `activation`) name for
// name, so the ported truth table reads as the same table.
function cfg(name: string, enabled: boolean): McpServerConfig {
  return { ...newServer([]), name, url: 'http://x/mcp', offload_access: true, enabled };
}

function category(name: string, enabled: boolean, servers: string[]): McpCategory {
  return { name, servers: [...servers], enabled };
}

function activation(
  cats: [string, boolean][],
  servers: [string, boolean][],
): McpActivation {
  const a = noActivation();
  for (const [k, v] of cats) a.categories[k] = v;
  for (const [k, v] of servers) a.servers[k] = v;
  return a;
}

function registry(over: Partial<McpRegistry> = {}): McpRegistry {
  return { servers: [], categories: [], activation: noActivation(), ...over };
}

describe('effective-enable predicate (contract C3)', () => {
  /// A VERBATIM port of the Rust `effective_enable_truth_table`
  /// (`src-tauri/src/offload/mcp_host.rs`), row for row and in the same order.
  /// Rust owns enforcement; this display mirror exists only so a row can show
  /// the state the backend will act on, and this test is what stops the two
  /// from drifting into two disagreeing rules. Changing a row here without
  /// changing it there is the bug it is looking for.
  test('the contract-C3 truth table, in full', () => {
    const noCats: McpCategory[] = [];
    const neutral = noActivation();

    // Uncategorized: the server toggle is the whole rule.
    expect(effectiveEnable(cfg('ddg', true), noCats, neutral)).toEqual({ kind: 'enabled' });
    expect(effectiveEnable(cfg('ddg', false), noCats, neutral)).toEqual({ kind: 'server-off' });

    // One category, server toggle on.
    const on = [category('research', true, ['ddg'])];
    const off = [category('research', false, ['ddg'])];
    expect(effectiveEnable(cfg('ddg', true), on, neutral)).toEqual({ kind: 'enabled' });
    expect(effectiveEnable(cfg('ddg', true), off, neutral)).toEqual({
      kind: 'categories-off',
      category: 'research',
    });
    // The server toggle wins outright: an ON category cannot resurrect it, and
    // the verdict names the SERVER level, not the category.
    expect(effectiveEnable(cfg('ddg', false), on, neutral)).toEqual({ kind: 'server-off' });

    // A category that does not contain the server is irrelevant.
    const elsewhere = [category('web', false, ['fetch'])];
    expect(effectiveEnable(cfg('ddg', true), elsewhere, neutral)).toEqual({ kind: 'enabled' });

    // Multi-category: categories OR, they do not AND.
    const oneOn = [category('research', false, ['ddg']), category('web', true, ['ddg'])];
    expect(effectiveEnable(cfg('ddg', true), oneOn, neutral)).toEqual({ kind: 'enabled' });
    const allOff = [category('research', false, ['ddg']), category('web', false, ['ddg'])];
    expect(effectiveEnable(cfg('ddg', true), allOff, neutral)).toEqual({
      // The FIRST containing category in registry order, deterministically.
      kind: 'categories-off',
      category: 'research',
    });
  });

  /// A VERBATIM port of the Rust `activation_overrides_both_levels_in_both_directions`.
  /// The maps reaching the predicate are already project-composed, so an entry
  /// is an override of the global flag — never a copy of it.
  test('activation overrides both levels in both directions', () => {
    const noCats: McpCategory[] = [];

    // Server level: overlay turns a globally-ON server off …
    expect(effectiveEnable(cfg('ddg', true), noCats, activation([], [['ddg', false]]))).toEqual({
      kind: 'server-off',
    });
    // … and a globally-OFF server on.
    expect(effectiveEnable(cfg('ddg', false), noCats, activation([], [['ddg', true]]))).toEqual({
      kind: 'enabled',
    });

    // Category level: overlay turns a globally-ON category off …
    const on = [category('research', true, ['ddg'])];
    expect(effectiveEnable(cfg('ddg', true), on, activation([['research', false]], []))).toEqual({
      kind: 'categories-off',
      category: 'research',
    });
    // … and a globally-OFF category on.
    const off = [category('research', false, ['ddg'])];
    expect(effectiveEnable(cfg('ddg', true), off, activation([['research', true]], []))).toEqual({
      kind: 'enabled',
    });

    // An entry naming something that does not exist is inert, not fatal — a
    // renamed server/category leaves stale overlay keys behind (C1).
    expect(
      effectiveEnable(
        cfg('ddg', true),
        on,
        activation([['gone', false]], [['also-gone', false]]),
      ),
    ).toEqual({ kind: 'enabled' });
  });

  test('isServerEnabled is the boolean shorthand', () => {
    const off = [category('research', false, ['ddg'])];
    expect(isServerEnabled(cfg('ddg', true), [], noActivation())).toBe(true);
    expect(isServerEnabled(cfg('ddg', false), [], noActivation())).toBe(false);
    expect(isServerEnabled(cfg('ddg', true), off, noActivation())).toBe(false);
  });

  test('the verdict label names the level that did it', () => {
    expect(describeVerdict({ kind: 'enabled' })).toBe('Active');
    expect(describeVerdict({ kind: 'server-off' })).toContain('server toggle');
    expect(describeVerdict({ kind: 'categories-off', category: 'research' })).toContain(
      'research',
    );
  });

  test('enabledCount counts the project-composed verdict, not the global flag', () => {
    const reg = registry({
      servers: [cfg('ddg', true), cfg('fetch', true), cfg('docs', false)],
      activation: activation([], [['ddg', false]]),
    });
    expect(enabledCount(reg)).toBe(1);
  });
});

describe('per-project override tri-state', () => {
  /// Absent and present-but-false are different states, and the whole
  /// inherited-vs-overridden UI rests on telling them apart.
  test('lookupOverride distinguishes absent from present-and-false', () => {
    expect(lookupOverride({}, 'ddg')).toBeUndefined();
    expect(lookupOverride({ ddg: false }, 'ddg')).toBe(false);
    expect(lookupOverride({ ddg: true }, 'ddg')).toBe(true);
  });

  test('overrideState maps the map to the three states', () => {
    expect(overrideState({}, 'ddg')).toBe('inherit');
    expect(overrideState({ ddg: true }, 'ddg')).toBe('on');
    expect(overrideState({ ddg: false }, 'ddg')).toBe('off');
  });

  /// The contract that makes revert honest: `inherit` DELETES the key. Writing
  /// the current global value instead would freeze the project at today's value
  /// and silently stop following later global changes.
  test('reverting to inherit deletes the key rather than writing the global value', () => {
    const overridden = withOverride({}, 'ddg', 'off');
    expect(overridden).toEqual({ ddg: false });
    const reverted = withOverride(overridden, 'ddg', 'inherit');
    expect(Object.prototype.hasOwnProperty.call(reverted, 'ddg')).toBe(false);
    expect(reverted).toEqual({});
  });

  test('withOverride does not mutate its input', () => {
    const before = { ddg: true };
    const after = withOverride(before, 'fetch', 'off');
    expect(before).toEqual({ ddg: true });
    expect(after).toEqual({ ddg: true, fetch: false });
  });
});

describe('names are ids (contract C1)', () => {
  test('uniqueName suffixes until free', () => {
    expect(uniqueName('server', [])).toBe('server');
    expect(uniqueName('server', ['server'])).toBe('server-2');
    expect(uniqueName('server', ['server', 'server-2'])).toBe('server-3');
  });

  test('a category rename is trimmed, never empty, never a collision', () => {
    const cats = [category('research', true, []), category('web', true, [])];
    // Trimmed.
    expect(resolveCategoryName('  research  ', cats, 0)).toBe('research');
    // Renaming row 0 onto row 1's name is a collision, even after a trim.
    expect(resolveCategoryName(' web ', cats, 0)).toBe('web-2');
    // Its own current name is not a collision with itself.
    expect(resolveCategoryName('research', cats, 0)).toBe('research');
    // Cleared field falls back rather than committing an unnameable category.
    expect(resolveCategoryName('   ', cats, 0)).toBe(CATEGORY_FALLBACK_NAME);
  });

  test('new rows never collide with an existing one', () => {
    const cats = [newCategory([])];
    expect(cats[0].name).toBe(CATEGORY_FALLBACK_NAME);
    expect(newCategory(cats).name).toBe(`${CATEGORY_FALLBACK_NAME}-2`);
    const servers = [newServer([])];
    expect(servers[0].name).toBe('server');
    expect(newServer(servers).name).toBe('server-2');
  });

  /// The V37 fields a hand-added row must carry, or it would be invisible to
  /// the C3 predicate's defaults on the frontend side.
  test('a hand-added server is external and exists', () => {
    const s = newServer([]);
    expect(s.origin).toBe('external');
    expect(s.enabled).toBe(true);
    expect(originLabel(s.origin)).toBe('external');
    expect(originLabel('internal')).toBe('internal');
  });
});

describe('grouping', () => {
  test('uncategorized servers go in the last group', () => {
    const reg = registry({
      servers: [cfg('ddg', true), cfg('loose', true)],
      categories: [category('research', true, ['ddg'])],
    });
    const groups = groupServers(reg);
    expect(groups.map((g) => g.category?.name ?? null)).toEqual(['research', null]);
    expect(groups[0].rows.map((r) => r.server.name)).toEqual(['ddg']);
    expect(groups[1].rows.map((r) => r.server.name)).toEqual(['loose']);
  });

  test('an empty category is still listed', () => {
    const reg = registry({ categories: [category('research', true, [])] });
    const groups = groupServers(reg);
    expect(groups).toHaveLength(2);
    expect(groups[0].rows).toEqual([]);
  });

  /// One row per server, no matter how many categories contain it — the other
  /// memberships travel on the row instead of cloning the editable fields.
  test('a multi-category server is listed once, under its first category', () => {
    const reg = registry({
      servers: [cfg('ddg', true)],
      categories: [category('research', true, ['ddg']), category('web', true, ['ddg'])],
    });
    const groups = groupServers(reg);
    expect(groups[0].rows.map((r) => r.server.name)).toEqual(['ddg']);
    expect(groups[1].rows).toEqual([]);
    expect(groups[0].rows[0].categories).toEqual(['research', 'web']);
  });

  test('rows carry their array index, not their name, as identity', () => {
    const reg = registry({ servers: [cfg('a', true), cfg('b', true)] });
    expect(groupServers(reg)[0].rows.map((r) => r.index)).toEqual([0, 1]);
  });

  /// The row shows both verdicts so "off here, on everywhere else" is legible.
  test('a row carries the project verdict and the global one', () => {
    const reg = registry({
      servers: [cfg('ddg', true)],
      activation: activation([], [['ddg', false]]),
    });
    const row = groupServers(reg)[0].rows[0];
    expect(row.verdict).toEqual({ kind: 'server-off' });
    expect(row.globalVerdict).toEqual({ kind: 'enabled' });
  });
});

describe('membership editing', () => {
  test('adding and removing membership is idempotent and leaves others alone', () => {
    const cats = [category('research', true, []), category('web', true, ['ddg'])];
    const added = withMembership(cats, 'research', 'ddg', true);
    expect(added[0].servers).toEqual(['ddg']);
    expect(added[1].servers).toEqual(['ddg']);
    expect(withMembership(added, 'research', 'ddg', true)[0].servers).toEqual(['ddg']);
    const removed = withMembership(added, 'research', 'ddg', false);
    expect(removed[0].servers).toEqual([]);
    expect(removed[1].servers).toEqual(['ddg']);
  });

  test('membership edits do not mutate the input categories', () => {
    const cats = [category('research', true, [])];
    withMembership(cats, 'research', 'ddg', true);
    expect(cats[0].servers).toEqual([]);
  });
});

describe('stale references (a rename is a new identity)', () => {
  test('a renamed server leaves stale membership and activation keys, surfaced not pruned', () => {
    const reg = registry({
      servers: [cfg('ddg-2', true)],
      categories: [category('research', true, ['ddg', 'ddg-2'])],
      activation: activation([['gone', false]], [['ddg', false]]),
    });
    const s = staleRefs(reg);
    expect(hasStaleRefs(s)).toBe(true);
    expect(s.activationServers).toEqual(['ddg']);
    expect(s.activationCategories).toEqual(['gone']);
    expect(s.members).toEqual([{ category: 'research', servers: ['ddg'] }]);

    const cleared = clearStaleRefs(reg);
    expect(hasStaleRefs(staleRefs(cleared))).toBe(false);
    expect(cleared.categories[0].servers).toEqual(['ddg-2']);
    expect(cleared.activation).toEqual({ categories: {}, servers: {} });
    // Clearing is a copy: the live registry is untouched until the parent
    // persists the returned one.
    expect(reg.categories[0].servers).toEqual(['ddg', 'ddg-2']);
  });

  test('a clean registry reports nothing', () => {
    const reg = registry({
      servers: [cfg('ddg', true)],
      categories: [category('research', true, ['ddg'])],
      activation: activation([['research', false]], [['ddg', true]]),
    });
    expect(hasStaleRefs(staleRefs(reg))).toBe(false);
  });
});

describe('cloneRegistry', () => {
  test('every mutated array and map gets its own instance', () => {
    const reg = registry({
      servers: [{ ...cfg('ddg', true), args: ['--x'], env: { K: 'v' } }],
      categories: [category('research', true, ['ddg'])],
      activation: activation([['research', true]], [['ddg', false]]),
    });
    const copy = cloneRegistry(reg);
    expect(copy).toEqual(reg);
    copy.servers[0].args.push('--y');
    copy.servers[0].env.K = 'other';
    copy.categories[0].servers.push('fetch');
    copy.activation.servers.ddg = true;
    delete copy.activation.categories.research;
    expect(reg.servers[0].args).toEqual(['--x']);
    expect(reg.servers[0].env).toEqual({ K: 'v' });
    expect(reg.categories[0].servers).toEqual(['ddg']);
    expect(reg.activation.servers).toEqual({ ddg: false });
    expect(reg.activation.categories).toEqual({ research: true });
  });
});
