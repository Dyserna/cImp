// Shared filesystem helpers for the repo-scanning guards under `tests/`.
//
// Why these live here rather than in `src/`: the guards read the repo off disk,
// and `tsconfig.json` type-checks `src/**` without `@types/node`, so `node:fs`
// there is an unresolved module in svelte-check. Pulling `@types/node` in for
// them would put Node's `setTimeout` (and friends) in front of the DOM's for the
// whole frontend — a much larger change than a walk is worth. Vitest's default
// discovery ignores this file (it is not a `*.test.ts`), so it is a module the
// suites import, not a suite of its own.
//
// Why they are SHARED (V42 tranche-2 review, T2-10): `cssTokens.test.ts` and
// `settingsCssOrphans.test.ts` carried the same `walk` and the same `read`,
// byte for byte, and a walk that grew an exclusion in one copy and not the
// other is a guard that quietly stops looking at part of the tree. One
// definition, two importers.
//
// CRLF: CI checks the repo out with CRLF on Windows while local trees are mixed
// (the V35 lesson: green locally, red in CI), so `read` strips `\r` and every
// scan built on it is line-ending agnostic by construction.

import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

/** The repository root. This file lives at `<repo>/tests/repoFiles.ts`. */
export const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

/**
 * Every file under `dir` whose name ends in one of `exts`, recursively.
 *
 * `node_modules` and dot-directories are skipped, and a directory that does not
 * exist contributes nothing rather than throwing — several callers scan
 * OPTIONAL trees (`themes/`, a backend theme dir) that a given checkout may not
 * have. That tolerance is why every caller also carries a vacuity guard: a
 * silent empty result must not read as a clean bill of health.
 */
export function walk(dir: string, exts: string[]): string[] {
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
export function read(file: string): string {
  return readFileSync(file, 'utf8').replace(/\r/g, '');
}

/** A repo-relative path with forward slashes, for failure messages. */
export function rel(file: string): string {
  return relative(REPO_ROOT, file).split(sep).join('/');
}
