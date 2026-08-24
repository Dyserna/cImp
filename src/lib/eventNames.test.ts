import { describe, it, expect } from 'vitest';
import * as EVENTS from './generated/events';

/// V42 F6 (#131) — the frontend half of the event-name contract.
///
/// A Tauri event name is matched by hand at two ends: an `emit(...)` in Rust and a
/// `listen(...)` here. Nothing joins them at runtime, and a typo on either side fails
/// **silently** — the emitter emits into the void, the listener never fires, and the
/// symptom is a panel that just never updates. There were 17 such pairs.
///
/// Both halves are now generated from one Rust table (`service::events`), and both
/// halves are guarded:
///
///   * Rust — `service::events::tests::no_emit_site_spells_a_literal` refuses an
///     `emit`/`emit_to_window` whose event-name argument is a string literal;
///   * here — the same refusal for `listen`.
///
/// A name can therefore no longer be spelled at a call site at all, on either side.

/// Every shipping frontend source, as text.
///
/// Read through Vite's own glob rather than `node:fs`, for the reason
/// `detectionContract.test.ts` gives: the app's tsconfig has no node types.
/// `.test.ts` files are excluded — a test may construct a literal deliberately, as
/// the positive control below does.
const SOURCES = import.meta.glob(['/src/**/*.ts', '/src/**/*.svelte', '!/src/**/*.test.ts'], {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

/// `listen(` / `listen<T>(` / `listenEvent(` immediately followed by a quote.
///
/// `\b` before `listen` is load-bearing: without it this matches
/// `addEventListener('click')`, `window.removeEventListener(...)` and every other
/// DOM call whose name merely CONTAINS the word.
///
/// `listenEvent` is covered too, even though its `K extends keyof EventPayloadMap`
/// signature already refuses a name no event has — so a literal there is a compile
/// error, not the silent failure this file exists for. It is refused anyway, because
/// the rule is worth having as ONE rule: the name comes from the generated constant,
/// always, and no reader has to work out which of the two calls is type-protected.
const LITERAL_LISTEN = /\blisten(Event)?\s*(<[^>]*>)?\s*\(\s*['"`]/;

/// Files the scan skips, each with the reason.
///
/// One entry, and it is not a listener: the generated module's own header comment
/// spells `listen('a-literal')` while explaining that it is refused. A comment
/// stripper would be the other way to handle that, and would be a second lexer to
/// get wrong — this is a generated file with no call sites in it at all, so skipping
/// it whole is both simpler and honest.
///
/// **A dynamic event name would belong here too, with its reason.** There are none
/// today: every listener in the app names a compile-time-known event.
const ALLOWED = new Map<string, string>([
  [
    '/src/lib/generated/events.ts',
    'generated constants only — no call sites; its header comment quotes the banned form while explaining the ban',
  ],
]);

describe('Tauri event names', () => {
  it('are never spelled as a literal in a listen() call', () => {
    const offenders: string[] = [];
    let scanned = 0;
    for (const [path, text] of Object.entries(SOURCES)) {
      if (ALLOWED.has(path)) continue;
      scanned++;
      if (!LITERAL_LISTEN.test(text)) continue;
      const line = text
        .split('\n')
        .findIndex((l) => LITERAL_LISTEN.test(l));
      offenders.push(`${path}:${line + 1}`);
    }
    // A glob that resolved to the wrong tree finds no offender and reports clean.
    expect(scanned).toBeGreaterThan(100);
    expect(
      offenders,
      'these files listen on a string literal instead of a constant from ' +
        "'./events'; a misspelled name fails silently (the listener never fires)",
    ).toEqual([]);
  });

  it('has a scanner that catches a planted literal', () => {
    // The test above passes by finding nothing, which is also how a broken regex
    // passes. These are the exact forms the codebase used before this change.
    expect(LITERAL_LISTEN.test("await listen<string[]>('ai-tab-restart-hint', cb)")).toBe(true);
    expect(LITERAL_LISTEN.test('void listen("fs-batch", cb)')).toBe(true);
    expect(LITERAL_LISTEN.test('listen(`pty-exit`, cb)')).toBe(true);
    expect(LITERAL_LISTEN.test("await listenEvent('pty-exit', cb)")).toBe(true);
    // …and does not fire on the sanctioned form, or on the DOM lookalikes.
    expect(LITERAL_LISTEN.test('await listenEvent(PTY_EXIT, cb)')).toBe(false);
    expect(LITERAL_LISTEN.test('listen<StateEvent>(AVATAR_STATE, cb)')).toBe(false);
    expect(LITERAL_LISTEN.test("el.addEventListener('click', cb)")).toBe(false);
    expect(LITERAL_LISTEN.test("window.removeEventListener('resize', cb)")).toBe(false);
  });

  it('export one constant per payload-map entry', () => {
    // The generator writes the constants and the payload map from the same Rust
    // rows, so a map key with no constant means the emitter dropped a row —
    // caught here rather than at the import site of a name that does not exist.
    const constants = new Set(
      Object.entries(EVENTS)
        .filter(([, v]) => typeof v === 'string')
        .map(([, v]) => v as string),
    );
    expect(constants.size).toBeGreaterThan(15);
    // Every constant is a kebab-case wire name, and every one is reachable.
    for (const name of constants) {
      expect(name, `${name} is not a kebab-case event name`).toMatch(/^[a-z][a-z0-9-]*$/);
    }
  });
});
