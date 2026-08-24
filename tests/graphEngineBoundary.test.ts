// Guard: the Graph view's engine state stays OUT of Svelte's reactivity.
//
// Why this exists (#130, F5). `GraphView.svelte` mutates its layout at
// animation-frame rate — `simulate` writes nine floats per node per frame, and
// `project` writes four more — over a graph that runs to `graph_viz_max_nodes`
// entries. None of that is `$state`, and the component said so in a comment:
//
//   "Non-reactive simulation state (mutated at animation-frame rate — kept out
//    of $state so every position update doesn't trigger Svelte's reactivity
//    graph)."
//
// F5 moved that state into `lib/graph/{sim,camera,render}.ts` and bundled it
// into three plain objects (`world`, `cam`, `scene`) passed by reference. The
// bundling is what makes the mistake easy: a `$state` in front of any of those
// four declarations still compiles, still renders, and turns the hot path into
// proxied property access on every node, every frame. Nothing else would catch
// it — not the type-checker, not the build, not a test that renders.
//
// So this reads the declarations back and asserts what they are. It is the
// same class of scanner as the Rust `include_str!` tests: fragile on purpose,
// re-point it if the shape changes, do not delete it.

import { join } from 'node:path';
import { describe, expect, test } from 'vitest';

import { read, REPO_ROOT } from './repoFiles';

const VIEW = join(REPO_ROOT, 'src', 'lib', 'GraphView.svelte');
const ENGINE = ['sim.ts', 'camera.ts', 'render.ts'].map((f) =>
  join(REPO_ROOT, 'src', 'lib', 'graph', f),
);

/// The four declarations that hold everything the frame loop touches, and the
/// factory each one is expected to come from. `const`, so a rebuild replaces
/// the CONTENTS and never the object the renderer is holding.
const DECLARATIONS: [name: string, initializer: string][] = [
  ['world', 'emptyWorld()'],
  ['cam', 'defaultCamera()'],
  ['tune', 'defaultTuning()'],
];

describe('the Graph view engine boundary', () => {
  const view = read(VIEW);

  test('the scan actually found the component (vacuity guard)', () => {
    expect(view.length, 'GraphView.svelte did not load').toBeGreaterThan(1000);
    expect(view, 'the engine imports are gone — re-point this test').toContain("from './graph/sim'");
  });

  for (const [name, init] of DECLARATIONS) {
    test(`\`${name}\` is a plain const, not $state`, () => {
      const decl = new RegExp(`^\\s*(let|const)\\s+${name}[^=\\n]*=\\s*(.+)$`, 'm').exec(view);
      expect(decl, `no declaration of \`${name}\` — re-point this test`).not.toBeNull();
      expect(decl![1], `\`${name}\` must be const: the renderer holds it by reference`).toBe(
        'const',
      );
      expect(decl![2].trim()).toBe(`${init};`);
      expect(
        decl![2],
        `\`${name}\` is the frame loop's state — $state would proxy every read of every ` +
          'node, every frame, and notify the reactivity graph on every write',
      ).not.toContain('$state');
    });
  }

  test('`scene` is a plain const object literal', () => {
    const decl = /^\s*(let|const)\s+scene:\s*Scene\s*=\s*(.+)$/m.exec(view);
    expect(decl, 'no `scene: Scene` declaration — re-point this test').not.toBeNull();
    expect(decl![1]).toBe('const');
    expect(decl![2].trim()).toBe('{');
    expect(decl![2]).not.toContain('$state');
  });

  test('no rune is applied to any engine identifier', () => {
    // Catches the variants the per-declaration checks above would miss:
    // `$state.raw(world)`, a `$derived` wrapper, a second copy under another
    // name. Comments are excluded — the ones explaining this rule name both.
    const offenders = view
      .split('\n')
      .map((line, i) => [i + 1, line] as const)
      .filter(([, l]) => !l.trimStart().startsWith('//'))
      .filter(([, l]) => /\$(state|derived)\b/.test(l))
      .filter(([, l]) => /\b(world|cam|scene|tune|nodeById|edgesByNode|clusterByDir)\b/.test(l))
      .map(([n, l]) => `${n}: ${l.trim()}`);
    expect(
      offenders,
      'These lines put a rune on the Graph view\'s frame-loop state:\n' + offenders.join('\n'),
    ).toEqual([]);
  });

  test('the engine modules are plain TypeScript — no runes, no Svelte imports', () => {
    // They are `.ts`, not `.svelte.ts`, so a rune would not compile anyway.
    // The point of asserting it is the day someone renames one to make "just
    // one little `$derived`" possible.
    for (const file of ENGINE) {
      const src = read(file);
      expect(src.length, `${file} did not load`).toBeGreaterThan(500);
      const runes = src
        .split('\n')
        .filter((l) => !l.trimStart().startsWith('//') && !l.trimStart().startsWith('*'))
        .filter((l) => /\$(state|derived|effect|props)\b/.test(l));
      expect(runes, `${file} uses a rune:\n${runes.join('\n')}`).toEqual([]);
      expect(src, `${file} imports from svelte`).not.toMatch(/from '\s*svelte/);
    }
  });
});
