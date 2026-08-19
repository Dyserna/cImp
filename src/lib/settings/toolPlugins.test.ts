import { describe, expect, test } from 'vitest';

import { defaultSettings } from './types';
import type { Settings } from './types';
import {
  categoryState,
  errorRows,
  permissionSummary,
  permissionsOpen,
  pluginRows,
  revertToGlobalPath,
  setCategoryEnabled,
  setGlobalPath,
  setPluginEnabled,
  setProjectPath,
  setToolEnabled,
  setToolVariable,
  toolPath,
} from './toolPlugins';
import type { PluginSet, PluginToolManifest } from './toolPlugins';

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
    // Decision 9's display form: the version is part of the name, because two
    // versions of one plugin coexist.
    expect(row.label).toBe('Acme Tools (1.0.0)');
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
