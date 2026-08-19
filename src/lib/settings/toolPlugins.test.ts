import { describe, expect, test } from 'vitest';

import { defaultSettings } from './types';
import type { Settings } from './types';
import {
  categoryState,
  errorRows,
  permissionSummary,
  permissionsOpen,
  pluginDisplayLabels,
  pluginRows,
  probeCommandName,
  revertToGlobalPath,
  setCategoryEnabled,
  setGlobalPath,
  setPluginEnabled,
  setProjectPath,
  setToolEnabled,
  setToolVariable,
  shouldAutoFill,
  siblingAutoFillTargets,
  toolPath,
} from './toolPlugins';
import type { PluginSet, PluginToolManifest, ProbeNameInput } from './toolPlugins';

const PROJECT = 'C:\\repo';

function tool(over: Partial<PluginToolManifest> = {}): PluginToolManifest {
  return {
    id: 'scan',
    label: 'Acme Scan',
    description: null,
    kind: 'security',
    enabled_by_default: true,
    runtime: 'auto',
    sandbox: 'required',
    extra_grants: [],
    variables: [],
    parameters_allowed: false,
    timeout_secs: null,
    env: [],
    argv: ['{root}'],
    transport: null,
    findings_exit_codes: [],
    applicability: { extensions: [], markers: [] },
    provider: null,
    cmd: null,
    cwd: null,
    report_file: null,
    pattern: null,
    parser: null,
    ingest: null,
    command: null,
    project_local_bin: null,
    dir_argv: [],
    ...over,
  };
}

/// One plugin, two tools in two categories — enough to exercise the group
/// toggle and the tri-state without a fixture nobody can hold in their head.
function set(): PluginSet {
  return {
    plugins: [
      {
        path: 'C:\\cimp\\plugins\\acme.json',
        provenance: 'user',
        key: 'acme@1.0.0',
        manifest: {
          manifest_version: 1,
          name: 'acme',
          version: '1.0.0',
          label: 'Acme Tools',
          description: 'Acme scanners',
          categories: [
            { id: 'sec', label: 'Security', tools: ['scan', 'audit'] },
            { id: 'misc', label: 'Misc', tools: ['fmt'] },
          ],
          tools: [
            tool({
              id: 'scan',
              variables: [{ name: 'ruleset', label: 'Ruleset', default: 'p/default' }],
              parameters_allowed: true,
            }),
            tool({ id: 'audit', label: 'Acme Audit', kind: 'audit' }),
            tool({ id: 'fmt', label: 'Acme Format', kind: 'command' }),
          ],
        },
      },
    ],
    errors: [],
    dir: 'C:\\cimp\\plugins',
    scanned_at_ms: 1,
    scan_ms: 2,
  };
}

function fresh(): Settings {
  return defaultSettings();
}

describe('pluginRows', () => {
  test('an unconfigured plugin renders everything enabled, path-less and at its defaults', () => {
    const rows = pluginRows(set(), fresh(), PROJECT);
    expect(rows).toHaveLength(1);
    const row = rows[0];
    // Decision 9 is now split: the row carries the bare name and the version
    // separately, and `pluginDisplayLabels` decides which of the two the list
    // needs (see its own describe block below).
    expect(row.label).toBe('Acme Tools');
    expect(row.version).toBe('1.0.0');
    expect(row.enabled).toBe(true);
    expect(row.toolCount).toBe(3);
    expect(row.categories.map((c) => c.id)).toEqual(['sec', 'misc']);

    const scan = row.categories[0].tools[0];
    expect(scan.toolKey).toBe('acme@1.0.0/scan');
    expect(scan.enabled).toBe(true);
    expect(scan.effectiveEnabled).toBe(true);
    // Enabled but inert: nothing is bundled, so a tool cImp has not been told
    // where to find has no path and cannot run.
    expect(scan.path).toEqual({ effective: '', scope: 'unset', global: '', project: null });
    // The declared variable is rendered with its default as the placeholder,
    // NOT pre-filled — a pre-filled default would be indistinguishable from a
    // value the user chose.
    expect(scan.variables).toEqual([
      { name: 'ruleset', label: 'Ruleset', fallback: 'p/default', value: '' },
    ]);
  });

  test('disabling the plugin disables its tools without clearing their own flags', () => {
    const s = fresh();
    setToolEnabled(s, 'acme@1.0.0', 'audit', false);
    setPluginEnabled(s, 'acme@1.0.0', false);

    const [row] = pluginRows(set(), s, PROJECT);
    const sec = row.categories[0];
    expect(sec.tools.map((t) => t.effectiveEnabled)).toEqual([false, false]);
    // …and the per-tool flags are untouched, so re-enabling the plugin restores
    // the selection the user had rather than switching everything on.
    expect(sec.tools.map((t) => t.enabled)).toEqual([true, false]);

    setPluginEnabled(s, 'acme@1.0.0', true);
    const [again] = pluginRows(set(), s, PROJECT);
    expect(again.categories[0].tools.map((t) => t.effectiveEnabled)).toEqual([true, false]);
  });

  test('the category toggle is a group operation over its members only', () => {
    const s = fresh();
    const [row] = pluginRows(set(), s, PROJECT);
    setCategoryEnabled(s, 'acme@1.0.0', row.categories[0], false);

    const [after] = pluginRows(set(), s, PROJECT);
    expect(after.categories[0].state).toBe('off');
    expect(after.categories[0].tools.map((t) => t.enabled)).toEqual([false, false]);
    // The other category is untouched — a group operation, not a plugin-wide one.
    expect(after.categories[1].state).toBe('on');
  });
});

describe('categoryState', () => {
  test('mixed membership renders indeterminate rather than guessing', () => {
    const s = fresh();
    setToolEnabled(s, 'acme@1.0.0', 'scan', false);
    const [row] = pluginRows(set(), s, PROJECT);
    expect(row.categories[0].state).toBe('mixed');
    expect(categoryState([])).toBe('off');
  });
});

describe('paths', () => {
  test('project beats global, and both values stay visible', () => {
    const s = fresh();
    setGlobalPath(s, 'acme@1.0.0/scan', 'C:\\bin\\acme.exe');
    expect(toolPath(s, PROJECT, 'acme@1.0.0/scan')).toEqual({
      effective: 'C:\\bin\\acme.exe',
      scope: 'global',
      global: 'C:\\bin\\acme.exe',
      project: null,
    });

    setProjectPath(s, PROJECT, 'acme@1.0.0/scan', 'D:\\alt\\acme.exe');
    expect(toolPath(s, PROJECT, 'acme@1.0.0/scan')).toEqual({
      effective: 'D:\\alt\\acme.exe',
      scope: 'project',
      global: 'C:\\bin\\acme.exe',
      project: 'D:\\alt\\acme.exe',
    });
    // Another project still inherits the machine-wide value.
    expect(toolPath(s, 'C:\\other', 'acme@1.0.0/scan').scope).toBe('global');
  });

  test('reverting deletes the override instead of storing an empty one', () => {
    const s = fresh();
    setGlobalPath(s, 'acme@1.0.0/scan', 'C:\\bin\\acme.exe');
    setProjectPath(s, PROJECT, 'acme@1.0.0/scan', 'D:\\alt\\acme.exe');
    revertToGlobalPath(s, PROJECT, 'acme@1.0.0/scan');

    // An override of '' would read as "no path here" and make the tool inert in
    // this project — the opposite of "use the machine-wide one".
    expect(s.tool_plugins.project_paths[PROJECT]).toBeUndefined();
    expect(toolPath(s, PROJECT, 'acme@1.0.0/scan').scope).toBe('global');
  });

  test('clearing the global path removes the key rather than storing a blank', () => {
    const s = fresh();
    setGlobalPath(s, 'acme@1.0.0/scan', 'C:\\bin\\acme.exe');
    setGlobalPath(s, 'acme@1.0.0/scan', '  ');
    expect(s.tool_plugins.global_paths['acme@1.0.0/scan']).toBeUndefined();
    expect(toolPath(s, PROJECT, 'acme@1.0.0/scan').scope).toBe('unset');
  });
});

describe('variables', () => {
  test('clearing a variable deletes it so the manifest default applies again', () => {
    const s = fresh();
    setToolVariable(s, 'acme@1.0.0', 'scan', 'ruleset', 'p/ci');
    expect(pluginRows(set(), s, PROJECT)[0].categories[0].tools[0].variables[0].value).toBe('p/ci');

    setToolVariable(s, 'acme@1.0.0', 'scan', 'ruleset', '');
    expect(s.tool_plugins.plugins['acme@1.0.0'].tools['scan'].variables).toEqual({});
    const rendered = pluginRows(set(), s, PROJECT)[0].categories[0].tools[0].variables[0];
    expect(rendered.value).toBe('');
    expect(rendered.fallback).toBe('p/default');
  });
});

describe('permissionSummary', () => {
  test('names what the tool ASKS for, sandbox posture first', () => {
    expect(
      permissionSummary(
        tool({
          sandbox: 'unsupported',
          runtime: 'python',
          extra_grants: ['C:\\ProgramData\\acme'],
          env: [['ACME_TOKEN', 'x']],
        }),
      ),
    ).toEqual([
      'to run OUTSIDE the OS sandbox (this tool declares it cannot be confined)',
      "the python runtime's files",
      'access to C:\\ProgramData\\acme',
      'these environment variables set: ACME_TOKEN',
    ]);
  });

  test('a tier-2 tool asks for the SERVER, never for a sandbox it will never be in', () => {
    const lines = permissionSummary(
      tool({
        provider: { server: 'acme-mcp', tool: 'scan_repository' },
        // The manifest's defaults; validation refuses anything else here.
        sandbox: 'required',
        runtime: 'auto',
      }),
    );
    expect(lines).toHaveLength(2);
    expect(lines[0]).toContain('acme-mcp');
    expect(lines[0]).toContain('scan_repository');
    expect(lines[0]).toContain('nothing runs on this machine');
    expect(lines[1]).toContain('trusting the server');
    // The sandbox sentence would be reassuring and false: nothing is confined
    // because nothing is spawned.
    expect(lines.join(' ')).not.toContain('OS sandbox');
  });

  test('a provider tool reaches the rendered row with no path affordance', () => {
    const s = set();
    s.plugins[0].manifest.tools = [
      tool({ id: 'scan', provider: { server: 'acme-mcp', tool: 'scan_repository' }, argv: [] }),
    ];
    s.plugins[0].manifest.categories = [{ id: 'sec', label: 'Security', tools: ['scan'] }];
    const row = pluginRows(s, fresh(), PROJECT)[0].categories[0].tools[0];
    expect(row.provider).toEqual({ server: 'acme-mcp', tool: 'scan_repository' });
    // …and it is NOT resolvable by name either, so the pane cannot fall back to
    // the built-in "(use the ebin folder / PATH)" story: the provider block is
    // the only thing it can render.
    expect(row.resolvesByName).toBe(false);
  });

  test('the safe defaults still say something, so silence never reads as "asks for nothing"', () => {
    expect(permissionSummary(tool())).toEqual([
      'to run inside the OS sandbox (it refuses to run unconfined)',
    ]);
  });
});

describe('permissionsOpen', () => {
  test('a single line stays collapsed only when the tool declares `required`', () => {
    expect(permissionsOpen({ permissions: ['one'], sandbox: 'required' })).toBe(false);
    // D-1: the most alarming ask is a ONE-line summary, so length alone left
    // it collapsed — the declaration has to open it on its own.
    expect(permissionsOpen({ permissions: ['one'], sandbox: 'unsupported' })).toBe(true);
    expect(permissionsOpen({ permissions: ['one'], sandbox: 'optional' })).toBe(true);
    expect(permissionsOpen({ permissions: ['one', 'two'], sandbox: 'required' })).toBe(true);
  });

  test('a rendered row carries the declaration the heuristic reads', () => {
    const row = pluginRows(set(), fresh(), PROJECT)[0].categories[0].tools[0];
    expect(row.sandbox).toBe('required');
  });
});

describe('errorRows', () => {
  test('a rejected file is labelled by its identity, or by its file names when it has none', () => {
    const s = set();
    s.errors = [
      {
        kind: 'invalid',
        paths: ['C:\\cimp\\plugins\\broken.json'],
        key: null,
        reason: 'not a valid manifest: expected value at line 1 column 3',
      },
      {
        kind: 'conflict',
        paths: ['C:\\cimp\\plugins\\a.json', 'C:\\cimp\\plugins\\b.json'],
        key: 'dup@1.0.0',
        reason: '2 files declare the plugin `dup@1.0.0`, so NEITHER was loaded',
      },
    ];
    const rows = errorRows(s);
    // A file whose JSON did not parse has no trustworthy identity, so the row
    // shows the file name rather than inventing one.
    expect(rows[0].label).toBe('broken.json');
    expect(rows[1].label).toBe('dup@1.0.0');
    // Both offending paths survive: "a duplicate exists" is useless without them.
    expect(rows[1].paths).toHaveLength(2);
    // The reason is verbatim the backend's, so it matches the Events row.
    expect(rows[1].reason).toContain('NEITHER was loaded');
  });
});

describe('stored state outlives its plugin', () => {
  test('a key whose manifest is not loaded is left alone, not pruned', () => {
    const s = fresh();
    setToolEnabled(s, 'gone@9.9.9', 'x', false);
    setGlobalPath(s, 'gone@9.9.9/x', 'C:\\bin\\gone.exe');

    // It renders nothing (the loader did not find it)…
    expect(pluginRows(set(), s, PROJECT).map((r) => r.key)).toEqual(['acme@1.0.0']);
    // …and it is still there when the plugin comes back.
    expect(s.tool_plugins.plugins['gone@9.9.9'].tools['x'].enabled).toBe(false);
    expect(s.tool_plugins.global_paths['gone@9.9.9/x']).toBe('C:\\bin\\gone.exe');
  });
});

// ── The Detect probe ────────────────────────────────────────────────────────

function probeInput(over: Partial<ProbeNameInput> = {}): ProbeNameInput {
  return { id: 'x', kind: 'command', cmd: null, command: null, resolvesByName: false, ...over };
}

/// One plugin shaped like the shipped `rust-toolchain` pack: three checks and a
/// command tool, all of them one executable, plus rows that must NOT be filled.
function toolchain(): PluginSet {
  return {
    plugins: [
      {
        path: 'C:\\cimp\\plugins\\rust-toolchain.json',
        provenance: 'user',
        key: 'rust-toolchain@1.0.0',
        manifest: {
          manifest_version: 1,
          name: 'rust-toolchain',
          version: '1.0.0',
          label: 'Rust toolchain',
          description: null,
          categories: [
            {
              id: 'cargo',
              label: 'Cargo',
              tools: ['cargo-build', 'cargo-test', 'cargo', 'cargo-pinned', 'rustfmt', 'remote'],
            },
          ],
          tools: [
            tool({
              id: 'cargo-build',
              label: 'cargo build',
              kind: 'check',
              argv: [],
              cmd: 'cargo build --message-format=json',
            }),
            tool({
              id: 'cargo-test',
              label: 'cargo test',
              kind: 'check',
              argv: [],
              cmd: 'cargo test',
            }),
            tool({ id: 'cargo', label: 'cargo', kind: 'command', argv: [] }),
            // Same binary, but the user already pointed this one somewhere.
            tool({
              id: 'cargo-pinned',
              label: 'cargo (pinned)',
              kind: 'check',
              argv: [],
              cmd: 'cargo fmt',
            }),
            // A different program entirely.
            tool({
              id: 'rustfmt',
              label: 'rustfmt',
              kind: 'check',
              argv: [],
              cmd: 'rustfmt --check',
            }),
            // Tier 2: nothing is spawned for it, so a stored path would describe
            // nothing. (Validation would refuse this exact combination in a real
            // manifest; the filter exists because the row type allows it.)
            tool({
              id: 'remote',
              label: 'Remote cargo',
              kind: 'command',
              argv: [],
              provider: { server: 'acme', tool: 'cargo' },
            }),
          ],
        },
      },
      // A SECOND plugin naming the same binary: one Detect click must not reach
      // across plugins, because the plugin is the unit its author reasoned about.
      {
        path: 'C:\\cimp\\plugins\\other.json',
        provenance: 'user',
        key: 'other@1.0.0',
        manifest: {
          manifest_version: 1,
          name: 'other',
          version: '1.0.0',
          label: 'Other',
          description: null,
          categories: [{ id: 'c', label: 'C', tools: ['cargo'] }],
          tools: [tool({ id: 'cargo', label: 'cargo', kind: 'command', argv: [] })],
        },
      },
    ],
    errors: [],
    dir: 'C:\\cimp\\plugins',
    scanned_at_ms: 1,
    scan_ms: 2,
  };
}

/// A BUILT-IN pack: same binary behind every row, but one of them declares a
/// `command`, which is what makes it resolve through `ebin` → `PATH` at run
/// time. Provenance is per-plugin, so this cannot be folded into `toolchain()`.
function builtinPack(): PluginSet {
  return {
    plugins: [
      {
        path: 'C:\\cimp\\builtin\\cargo.json',
        provenance: 'builtin',
        key: 'cimp-builtin@1',
        manifest: {
          manifest_version: 1,
          name: 'cimp-builtin',
          version: '1',
          label: 'Built-in cargo',
          description: null,
          categories: [
            { id: 'cargo', label: 'Cargo', tools: ['cargo', 'cargo-check', 'cargo-fmt', 'remote'] },
          ],
          tools: [
            // Declares a command ⇒ resolvesByName ⇒ never pinned.
            tool({ id: 'cargo', label: 'cargo', kind: 'command', argv: [], command: 'cargo' }),
            tool({ id: 'cargo-check', label: 'cargo check', kind: 'check', argv: [], cmd: 'cargo check' }),
            tool({ id: 'cargo-fmt', label: 'cargo fmt', kind: 'check', argv: [], cmd: 'cargo fmt' }),
            tool({
              id: 'remote',
              label: 'Remote cargo',
              kind: 'command',
              argv: [],
              provider: { server: 'acme', tool: 'cargo' },
            }),
          ],
        },
      },
    ],
    errors: [],
    dir: 'C:\\cimp\\builtin',
    scanned_at_ms: 1,
    scan_ms: 2,
  };
}

describe('probeCommandName', () => {
  test("a built-in's declared command wins over what the kind rules would derive", () => {
    expect(
      probeCommandName(
        probeInput({
          id: 'gitleaks-x',
          kind: 'security',
          command: 'gitleaks',
          resolvesByName: true,
        }),
      ),
    ).toBe('gitleaks');
    // `resolvesByName` is "built-in AND a declared command"; without it the kind
    // rules decide, and a security tool has no name to derive.
    expect(
      probeCommandName(probeInput({ id: 'gitleaks-x', kind: 'security', command: 'gitleaks' })),
    ).toBe(null);
  });

  test('a check probes the first token of its command line', () => {
    expect(
      probeCommandName(probeInput({ kind: 'check', cmd: 'cargo build --message-format=json' })),
    ).toBe('cargo');
    expect(probeCommandName(probeInput({ kind: 'check', cmd: '  npm   run lint ' }))).toBe('npm');
    expect(probeCommandName(probeInput({ kind: 'check', cmd: 'git' }))).toBe('git');
    // Nothing to split ⇒ nothing to guess.
    expect(probeCommandName(probeInput({ kind: 'check', cmd: null }))).toBe(null);
    expect(probeCommandName(probeInput({ kind: 'check', cmd: '   ' }))).toBe(null);
  });

  test('a command-kind tool probes its id, and a findings tool derives nothing', () => {
    expect(probeCommandName(probeInput({ id: 'git', kind: 'command' }))).toBe('git');
    for (const kind of ['audit', 'security'] as const) {
      expect(probeCommandName(probeInput({ id: 'semgrep', kind }))).toBe(null);
    }
  });

  test('the guard refuses anything that is not a bare name', () => {
    for (const hostile of [
      '..\\evil.exe',
      '../evil.exe',
      'C:\\Windows\\System32\\calc.exe',
      '/usr/bin/id',
      'a/b',
      'a\\b',
      '..',
    ]) {
      // Through the id (rule 3)…
      expect(probeCommandName(probeInput({ id: hostile, kind: 'command' }))).toBe(null);
      // …and through the cmd's first token (rule 2). A manifest is untrusted
      // input, and a probe aimed at a path of its choosing would make Detect an
      // execution primitive rather than a lookup.
      expect(probeCommandName(probeInput({ kind: 'check', cmd: hostile + ' --version' }))).toBe(
        null,
      );
    }
    // An id that is empty or only whitespace is not a name either. (The `cmd`
    // arm of this case is the `null`/`'   '` pair above: a blank command line
    // has no first token at all.)
    for (const blank of ['', '   ']) {
      expect(probeCommandName(probeInput({ id: blank, kind: 'command' }))).toBe(null);
    }
    // A built-in `command` that fails the guard yields nothing rather than
    // falling back to the id.
    expect(
      probeCommandName(
        probeInput({
          id: 'cargo',
          kind: 'command',
          command: '..\\evil.exe',
          resolvesByName: true,
        }),
      ),
    ).toBe(null);
  });
});

describe('siblingAutoFillTargets', () => {
  test('one hit fills every path-less row of the same plugin that resolves to the same binary', () => {
    const s = fresh();
    setGlobalPath(s, 'rust-toolchain@1.0.0/cargo-pinned', 'D:\\pinned\\cargo.exe');
    const plugin = pluginRows(toolchain(), s, PROJECT)[0];
    const tools = plugin.categories[0].tools;
    const clicked = tools.find((t) => t.id === 'cargo-build')!;

    const keys = siblingAutoFillTargets(plugin, clicked).map((t) => t.toolKey);
    expect(keys).toEqual(['rust-toolchain@1.0.0/cargo-test', 'rust-toolchain@1.0.0/cargo']);
    // The clicked row is the caller's own business (it is filled directly), a
    // row with a path is a decision this must not overwrite, `rustfmt` is a
    // different program, the provider row spawns nothing, and the OTHER
    // plugin's `cargo` is out of scope.
    expect(keys).not.toContain(clicked.toolKey);
    expect(keys).not.toContain('rust-toolchain@1.0.0/cargo-pinned');
    expect(keys).not.toContain('rust-toolchain@1.0.0/rustfmt');
    expect(keys).not.toContain('rust-toolchain@1.0.0/remote');
    expect(keys.every((k) => k.startsWith('rust-toolchain@1.0.0/'))).toBe(true);
  });

  test('a tool with no derivable name fills nothing', () => {
    const plugin = pluginRows(set(), fresh(), PROJECT)[0];
    const scan = plugin.categories[0].tools.find((t) => t.id === 'scan')!;
    expect(probeCommandName(scan)).toBe(null);
    expect(siblingAutoFillTargets(plugin, scan)).toEqual([]);
  });

  test('a rendered row carries what the derivation reads', () => {
    const plugin = pluginRows(toolchain(), fresh(), PROJECT)[0];
    const build = plugin.categories[0].tools[0];
    expect(build.cmd).toBe('cargo build --message-format=json');
    expect(build.command).toBe(null);
    expect(probeCommandName(build)).toBe('cargo');
  });

  test('a sibling that resolves through ebin/PATH is left empty on purpose', () => {
    const plugin = pluginRows(builtinPack(), fresh(), PROJECT)[0];
    const tools = plugin.categories[0].tools;
    const clicked = tools.find((t) => t.id === 'cargo-check')!;
    const byName = tools.find((t) => t.id === 'cargo')!;
    // Both rows probe for the same binary and both boxes are blank, so only the
    // exemption keeps the by-name row out: pinning it would freeze a lookup
    // that is meant to re-run against `ebin` on every launch.
    expect(probeCommandName(clicked)).toBe('cargo');
    expect(probeCommandName(byName)).toBe('cargo');
    expect(byName.resolvesByName).toBe(true);
    expect(byName.path.effective).toBe('');
    expect(siblingAutoFillTargets(plugin, clicked).map((t) => t.toolKey)).toEqual([
      'cimp-builtin@1/cargo-fmt',
    ]);
  });
});

describe('shouldAutoFill', () => {
  test('a Detect hit is stored for an ordinary tool only', () => {
    const rows = pluginRows(builtinPack(), fresh(), PROJECT)[0].categories[0].tools;
    const byId = (id: string) => rows.find((t) => t.id === id)!;
    // The plain rows: a path is the only way cImp would find them.
    expect(shouldAutoFill(byId('cargo-check'))).toBe(true);
    expect(shouldAutoFill(byId('cargo-fmt'))).toBe(true);
    // Resolves through `ebin` → `PATH`; a stored path would replace that live
    // lookup with today's answer, so the empty box is the working default.
    expect(shouldAutoFill(byId('cargo'))).toBe(false);
    // Tier 2 spawns nothing, so a path would describe nothing.
    expect(shouldAutoFill(byId('remote'))).toBe(false);
  });
});

describe('pluginDisplayLabels', () => {
  test('the version appears only where the bare names collide', () => {
    const labels = pluginDisplayLabels([
      { key: 'acme@1.0.0', label: 'Acme Tools', version: '1.0.0' },
      { key: 'acme@2.0.0', label: 'Acme Tools', version: '2.0.0' },
      { key: 'solo@1.0.0', label: 'Solo', version: '1.0.0' },
    ]);
    // Both sides of a collision carry it: "Acme Tools" beside "Acme Tools
    // (2.0.0)" would read as two different plugins rather than two versions.
    expect(labels.get('acme@1.0.0')).toBe('Acme Tools (1.0.0)');
    expect(labels.get('acme@2.0.0')).toBe('Acme Tools (2.0.0)');
    expect(labels.get('solo@1.0.0')).toBe('Solo');
  });

  test('one of each is just the name, and an empty list is an empty map', () => {
    const rows = pluginRows(set(), fresh(), PROJECT);
    expect(pluginDisplayLabels(rows).get('acme@1.0.0')).toBe('Acme Tools');
    expect(pluginDisplayLabels([]).size).toBe(0);
  });

  test('different plugins sharing one label still collide — the label is what the user reads', () => {
    const labels = pluginDisplayLabels([
      { key: 'a@1.0.0', label: 'Scanners', version: '1.0.0' },
      { key: 'b@1.0.0', label: 'Scanners', version: '1.0.0' },
    ]);
    expect(labels.get('a@1.0.0')).toBe('Scanners (1.0.0)');
    expect(labels.get('b@1.0.0')).toBe('Scanners (1.0.0)');
  });
});
