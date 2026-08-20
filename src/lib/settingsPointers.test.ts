import { describe, it, expect } from 'vitest';

/// F-18 — every "Settings → X" pointer in the frontend names a Settings section
/// that EXISTS, enforced rather than trusted.
///
/// F-18 was the first defect found by *running* the rc.1 build, and it was pure
/// drift: the V32 injection controls lived under a sidebar entry labelled
/// "Offload task tools", while the taint popover, the chip tooltips, the spec and
/// the maintenance runbook all sent the user to "Settings → Tools → Injection
/// protection". There has never been a Tools section. A user following the app's
/// own instructions could not reach a security control — the same defect class as
/// a warning nobody enforces, and the reason `Settings → …` needs a mechanical
/// guard: it is prose, it is written far from the sidebar, and nothing about
/// renaming a section breaks a string that points at the old name.
///
/// **Scope, stated plainly.** This checks the FIRST segment — the top-level
/// sidebar entry — against `SettingsApp.svelte`'s own `SECTIONS` array. Deeper
/// segments (sub-tabs, `<h3>` groups) are not enumerable from one array, so a
/// pointer may still name a real section and a wrong heading under it. That is a
/// narrower failure than F-18's (the user lands on the right page and reads it)
/// and is left to review.
///
/// **This test covers the TypeScript/Svelte half only, and F-18 has a Rust
/// half.** `src-tauri/src/ipc/commands.rs`'s `updates_allowed` refusal, and a
/// dozen comments and messages beside it, carry the same literals. A tripwire
/// that scans only `src/` would let F-18 regress through the half nobody scans,
/// so the Rust lane owns the mirror of this test over `src-tauri/src/**`.

/// Every shipping frontend source, as text. Same mechanism and the same reasons
/// as `detectionContract.test.ts`: Vite's own glob rather than `node:fs` (the
/// app tsconfig has no node types), and `.test.ts` excluded — a test naming a
/// broken path in its prose, as this one does, is not a pointer shown to anyone.
const SOURCES = import.meta.glob(
  ['/src/**/*.ts', '/src/**/*.svelte', '!/src/**/*.test.ts'],
  { query: '?raw', import: 'default', eager: true },
) as Record<string, string>;

const SETTINGS_APP = '/src/SettingsApp.svelte';

/// The sidebar labels, parsed out of the one array that renders them. Read from
/// source rather than duplicated here on purpose: a hand-kept copy of the labels
/// would be a second thing to update when a section is renamed, i.e. this
/// finding again, one level up.
export function sidebarLabels(settingsApp: string): string[] {
  const start = settingsApp.indexOf('const SECTIONS:');
  if (start < 0) return [];
  const end = settingsApp.indexOf('];', start);
  if (end < 0) return [];
  return [...settingsApp.slice(start, end).matchAll(/label: '([^']+)'/g)].map((m) => m[1]);
}

/// A pointer literal: "Settings", an arrow, then the rest of that line. Both the
/// typographic arrow the UI uses and the ASCII one, because a `->` in a comment
/// is the same promise to the reader.
const POINTER = /Settings\s*(?:→|->)[ \t]*([^\r\n]*)/g;

/// The first segment of a pointer, i.e. the top-level section it names: the
/// captured tail up to the next arrow or sentence punctuation. Deliberately
/// tolerant of prose running on after the name ("Settings → Checks (V22 …)",
/// "Settings → Code Intelligence to draw it here") — a pointer is a phrase in a
/// sentence, not a path literal, and a checker that demanded an exact match
/// would be turned off within a week.
function firstSegment(tail: string): string {
  let cut = tail.length;
  for (const stop of ['→', '->', '.', ',', ';', ':', '(', ')', '"', "'", '`', '<', '!', '?', '—']) {
    const i = tail.indexOf(stop);
    if (i >= 0 && i < cut) cut = i;
  }
  return tail.slice(0, cut).trim();
}

/// Whether a first segment names a real section: it must BE a label, or begin
/// with one at a word boundary (so trailing prose is fine and "Offload" alone —
/// the shorthand that also missed, since the label is "Offload task tools" — is
/// not). Case-sensitive: "Code Graph" is not the "Code graph" the user reads.
function namesASection(segment: string, labels: readonly string[]): boolean {
  return labels.some(
    (l) => segment === l || (segment.startsWith(l) && /^[\s(,.;:—-]/.test(segment.slice(l.length))),
  );
}

/// Every pointer in one file that does not name a real section.
export function badPointers(
  path: string,
  text: string,
  labels: readonly string[],
): string[] {
  const out: string[] = [];
  text.split(/\r?\n/).forEach((line, i) => {
    for (const m of line.matchAll(POINTER)) {
      const segment = firstSegment(m[1]);
      // A pointer whose tail is empty on this line continues on the next one
      // (wrapped prose). Nothing to check here; the next line carries no
      // "Settings" and so is not a pointer of its own.
      if (segment === '') continue;
      if (!namesASection(segment, labels)) {
        out.push(`${path}:${i + 1}  Settings → ${segment}   ← no such section`);
      }
    }
  });
  return out;
}

describe('Settings pointer strings', () => {
  const labels = sidebarLabels(SOURCES[SETTINGS_APP] ?? '');

  it('reads the sidebar labels out of SettingsApp itself', () => {
    // The whole test is a grep against this list, so an empty or truncated
    // parse would pass everything.
    expect(labels.length).toBeGreaterThan(10);
    expect(labels).toContain('Injection protection');
    expect(labels).toContain('Offload task tools');
    expect(labels).toContain('Appearance');
    // …and the ids stayed in step with the labels, so a section that grew a
    // label but no id (or the reverse) cannot slip through.
    const ids = [...(SOURCES[SETTINGS_APP] ?? '').matchAll(/\{ id: '([^']+)', label: '[^']+' \}/g)];
    expect(ids.length).toBe(labels.length);
  });

  it('points every "Settings → …" literal at a section that exists', () => {
    const offenders: string[] = [];
    for (const [path, text] of Object.entries(SOURCES)) {
      offenders.push(...badPointers(path, text, labels));
    }
    expect(
      offenders,
      'A pointer names a Settings section that is not in the sidebar. Either fix the path to ' +
        'the label the user actually reads (SECTIONS in src/SettingsApp.svelte), or — if the ' +
        'section was renamed — rename it in both places. F-18: an instruction the user cannot ' +
        'follow makes the control behind it unreachable, which is the same defect as the ' +
        'control not being there.',
    ).toEqual([]);
  });

  it('catches the F-18 pointer itself', () => {
    // The positive control. Without it a broken glob, a regex that matches
    // nothing, or an over-tolerant `namesASection` would leave this suite
    // green and useless — "validation declared ≠ validation enforced" applies
    // to the validator too.
    expect(
      badPointers('/src/fake.svelte', 'Change it in Settings → Tools → Injection protection.', labels),
    ).toHaveLength(1);
    // The shorthand family that also missed: the label is "Offload task tools".
    expect(
      badPointers('/src/fake.svelte', 'add one in Settings → Offload.', labels),
    ).toHaveLength(1);
    // Case matters — "Code Graph" is not what the sidebar says.
    expect(
      badPointers('/src/fake.svelte', 'Toggle Settings → Code Graph → lean tools.', labels),
    ).toHaveLength(1);
    // …while real pointers, including ones that run on into prose or into a
    // sub-heading, stay quiet.
    for (const ok of [
      'restart them (Settings → Tabs → Restart) for these to apply.',
      'Prices are editable in Settings → LLM pricing.',
      'Turn it on in Settings → Code Intelligence to draw it here.',
      'the global Settings → Appearance section (V1.4-01 Phase 7)',
      'Change it in Settings → Injection protection.',
    ]) {
      expect(badPointers('/src/fake.svelte', ok, labels), ok).toEqual([]);
    }
  });

  it('scans the tree it thinks it is scanning', () => {
    const paths = Object.keys(SOURCES);
    expect(paths).toContain(SETTINGS_APP);
    expect(paths).toContain('/src/lib/TaintMenu.svelte');
    expect(paths).toContain('/src/lib/status/InjectionBadge.svelte');
    expect(paths.length).toBeGreaterThan(20);
    expect(paths.filter((p) => p.endsWith('.test.ts'))).toEqual([]);
    // The scan finds pointers at all — a `POINTER` regex that stopped matching
    // would otherwise read as "no offenders".
    const total = Object.entries(SOURCES).reduce(
      (n, [, text]) => n + [...text.matchAll(POINTER)].length,
      0,
    );
    expect(total).toBeGreaterThan(20);
  });

  it('keeps the injection badge deep-linked to the section its tooltips name', () => {
    // The other half of F-18: the chip whose whole job is "protection is
    // reduced, go look" opened Settings on whatever section it happened to
    // land on. The tooltip text and the click target have to agree, and both
    // live in files with no component harness — so this is where they are
    // checked.
    //
    // **V39 moved the gesture, not the rule.** The primary click flips the L1
    // master now; Settings is the SECONDARY gesture. So the tooltip has to name
    // the destination *and* the gesture that reaches it, or the chip promises a
    // navigation its click no longer performs.
    const badge = SOURCES['/src/lib/status/InjectionBadge.svelte'] ?? '';
    expect(badge).toContain("openSettingsWindowToSection('injection')");
    expect(badge).toContain('oncontextmenu');
    // The old call, at its call site (the prose above it still names it, which
    // is why this matches the `void` form rather than the bare identifier).
    expect(badge).not.toContain('void openSettingsWindow()');
    const latch = SOURCES['/src/lib/latch.ts'] ?? '';
    expect(latch).toContain('Right-click to open Settings → Injection protection.');
    // The pre-V39 promise must be gone from every tooltip: a left click no
    // longer opens anything.
    expect(latch).not.toContain('Click to open Settings');
  });

  it('keeps the sandbox chips deep-linked to the Sandboxing section', () => {
    // Same rule, same reason, for V39's two new chips: the tooltip names the
    // gesture and the section, and the component uses that exact section id.
    const badge = SOURCES['/src/lib/status/SandboxBadge.svelte'] ?? '';
    expect(badge).toContain("openSettingsWindowToSection('sandboxing')");
    expect(badge).toContain('oncontextmenu');
    const chip = SOURCES['/src/lib/status/sandboxChip.ts'] ?? '';
    expect(chip).toContain('Right-click to open Settings → Sandboxing.');
    expect(chip).not.toContain('Click to open Settings');
  });
});
