// Guard: every `var(--token)` used without a literal fallback must name a
// token that is actually DECLARED somewhere.
//
// Why this exists (survey 2026-08-23, defect D2): `var(--text-error)` in
// SettingsApp and `var(--fg)` in EventsView were declared in NO theme file and
// carried no fallback, so error text silently inherited the body colour —
// three "the error is red" rules that had never been red. Nothing catches that
// at build time: CSS resolves an unknown custom property to the inherited
// value rather than erroring, and svelte-check does not look at token names.
//
// The rule this pins:
//
//   var(--x)          → `--x` must be declared (theme files, app CSS, a
//                        component's own scoped block, or a runtime
//                        `setProperty('--x', …)`).
//   var(--x, <fall>)  → always OK; the fallback is the contract.
//
// Declarations are collected permissively on purpose (any `--x:` in any
// scanned file counts). A false "declared" only weakens the guard; a false
// "undeclared" would block a legitimate token, and this test must never be the
// reason a correct change is red.
//
// CRLF: CI checks out the repo with CRLF on Windows while local trees are
// mixed, so every file is read with `\r` stripped before matching — the same
// lesson the Rust source-scanners learned in V35 (green locally, red in CI).
//
// Why it lives in `tests/` rather than beside the themes it guards: it reads
// the repo off disk, and `tsconfig.json` type-checks `src/**` without
// `@types/node`, so `node:fs` there is an unresolved module in svelte-check.
// Pulling `@types/node` in for one file would put Node's `setTimeout` (and
// friends) in front of the DOM's for the whole frontend -- a much larger
// change than this guard is worth. `vite.config.ts` sits outside the same
// include for the same reason. Vitest's default discovery finds it here.

import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, test } from 'vitest';

// this file: <repo>/tests/cssTokens.test.ts
const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

function walk(dir: string, exts: string[]): string[] {
  const out: string[] = [];
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out; // an optional tree (e.g. themes/) that isn't present
  }
  for (const entry of entries) {
    if (entry.name === 'node_modules' || entry.name.startsWith('.')) continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(full, exts));
    else if (exts.some((ext) => entry.name.endsWith(ext))) out.push(full);
  }
  return out;
}

/** Read a source file line-ending agnostically (see the CRLF note above). */
function read(file: string): string {
  return readFileSync(file, 'utf8').replace(/\r/g, '');
}

/**
 * Frontend sources: the files that USE tokens (and may also declare them).
 * Test files are excluded from BOTH roles — this file's own prose names the
 * broken tokens it exists to keep out, and a test must not be able to satisfy
 * the guard by "declaring" a token nothing ships.
 */
const SRC_FILES = walk(join(REPO_ROOT, 'src'), ['.svelte', '.css', '.ts']).filter(
  (f) => !f.endsWith('.test.ts'),
);

/**
 * Everything that may DECLARE a token: the frontend sources plus the three
 * out-of-`src/` token homes — the external themes (`themes/<id>/theme.css`),
 * the backend-compiled `tui` theme, and the two entry HTML files.
 */
const DECL_FILES = [
  ...SRC_FILES,
  ...walk(join(REPO_ROOT, 'themes'), ['.css']),
  ...walk(join(REPO_ROOT, 'src-tauri', 'src', 'theming'), ['.css']),
  join(REPO_ROOT, 'index.html'),
  join(REPO_ROOT, 'settings.html'),
];

function declaredTokens(): Set<string> {
  const declared = new Set<string>();
  for (const file of DECL_FILES) {
    let text: string;
    try {
      text = read(file);
    } catch {
      continue;
    }
    // `--x: value` — a CSS declaration, in a stylesheet, a scoped <style>
    // block, or a `style="--x: …"` attribute.
    for (const m of text.matchAll(/(--[A-Za-z0-9_-]+)\s*:/g)) declared.add(m[1]);
    // `element.style.setProperty('--x', …)` — set at runtime (accent, term
    // palette, terminal background, status-bar rows).
    for (const m of text.matchAll(/setProperty\(\s*['"`](--[A-Za-z0-9_-]+)/g)) declared.add(m[1]);
  }
  return declared;
}

describe('CSS custom properties', () => {
  test('the scan actually sees the frontend (vacuity guard)', () => {
    expect(SRC_FILES.length).toBeGreaterThan(100);
    expect(declaredTokens().size).toBeGreaterThan(50);
  });

  test('every var(--token) with no fallback names a declared token', () => {
    const declared = declaredTokens();
    const offenders: string[] = [];
    for (const file of SRC_FILES) {
      const lines = read(file).split('\n');
      lines.forEach((line, i) => {
        for (const m of line.matchAll(/var\(\s*(--[A-Za-z0-9_-]+)\s*(,?)/g)) {
          if (m[2] === ',') continue; // has a literal fallback — fine
          if (declared.has(m[1])) continue;
          const where = relative(REPO_ROOT, file).split(sep).join('/');
          offenders.push(`${where}:${i + 1}  ${m[1]}  →  ${line.trim()}`);
        }
      });
    }
    expect(
      offenders,
      'These var(--token) uses reference a token no theme/app CSS declares and have no ' +
        'fallback, so they silently inherit instead of applying. Use the nearest declared ' +
        'token (e.g. --text-danger-soft for error text, --text-primary for body text), or ' +
        'add a literal fallback:\n' +
        offenders.join('\n'),
    ).toEqual([]);
  });
});
