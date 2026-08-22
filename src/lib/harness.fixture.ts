/// V40 Phase F — **the committed registry, for tests** (locked decision 11).
///
/// `src-tauri/fixtures/harness/registry.json` is written by the Rust test
/// `harness::info::tests::the_committed_registry_fixture_matches_the_registry`,
/// which fails the build when the file on disk differs from `HARNESSES`. This
/// module is the TypeScript side of that seam: `harness.test.ts` checks the
/// unions and key sets against it, and every other test that needs a harness id
/// takes it from HERE rather than typing one.
///
/// That is the rule the phase's acceptance test enforces: no test embeds a
/// harness identity literal, so renaming a harness in Rust re-points the whole
/// frontend suite instead of leaving a dozen green tests asserting a name that
/// no longer exists.
import registry from '../../src-tauri/fixtures/harness/registry.json';

import { harnesses, type HarnessInfo } from './harness';
import { allOffInjectionOverrides, type AiToolTabConfig } from './settings/types';

/// Every registered harness, exactly as the backend serves it.
export const FIXTURE_HARNESSES = registry as unknown as HarnessInfo[];

/// The first registered harness. Tests that need "a harness with a usage
/// source" or "the one that owns two reserved tabs" should assert on its
/// declared data rather than on which product it happens to be.
export const FIRST_HARNESS: HarnessInfo = FIXTURE_HARNESSES[0];

/// The second registered harness — the "some other harness" of any test about
/// two of them. Undefined if only one is registered, which no shipped build
/// has been.
export const SECOND_HARNESS: HarnessInfo = FIXTURE_HARNESSES[1];

/// The first registered harness that declares `feature`, or `undefined`.
export function fixtureHarnessWith(feature: string): HarnessInfo | undefined {
  return FIXTURE_HARNESSES.find((h) => (h.features as string[]).includes(feature));
}

/// The first registered harness that does NOT declare `feature`.
export function fixtureHarnessWithout(feature: string): HarnessInfo | undefined {
  return FIXTURE_HARNESSES.find((h) => !(h.features as string[]).includes(feature));
}

/// Fill the live `harnesses` store, for the modules whose store-reading
/// wrappers are under test. Call it in a `beforeEach`; `harnesses.set([])`
/// restores the pre-IPC state a first-paint test wants.
export function installFixtureHarnesses(): void {
  harnesses.set(FIXTURE_HARNESSES);
}

/// One reserved AI tab per registered tab id, in canonical order — the roster
/// `defaultSettings()` used to seed.
///
/// V40 Phase F emptied `DEFAULT_SETTINGS.tabs` (it was the frontend declaring
/// the roster; the real one arrives from the backend). The suites that need
/// "the AI tabs a normal install has" build them from the registry instead, so
/// a harness added in Rust is exercised by them without an edit here.
///
/// The command is the harness's FIRST declared binary, which is what makes
/// `tabHarness` and `usagePushHarness` resolve these tabs the way the backend
/// resolves a real one.
export function fixtureAiTabs(): AiToolTabConfig[] {
  return FIXTURE_HARNESSES.flatMap((h) =>
    h.tab_ids.map((id) => ({
      kind: 'ai_tool' as const,
      id,
      builtin: true,
      name: id,
      command: h.binaries[0],
      args: [],
      cwd: null,
      env: {},
      tts_injection: { enabled: true },
      notifications: {
        idle: { enabled: true, text: '' },
        awaiting_permission: { enabled: true, text: '' },
        question: { enabled: true, text: '' },
        error: { enabled: true, text: '' },
      },
      first_launch_notice_dismissed: true,
      theme_override: null,
      background_override: null,
      use_local_provider: false,
      injection_overrides: allOffInjectionOverrides(),
      read_only: false,
      delegation_role: 'none' as const,
      delegation_backend: { name: null, tier: 'quality' as const, declared_context: null },
    })),
  );
}
