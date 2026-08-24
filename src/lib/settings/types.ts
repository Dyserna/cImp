// The settings wire surface: GENERATED types re-exported, plus the value half
// that codegen cannot produce.
//
// V42 Phase E replaced ~1,850 lines of hand-written interfaces here with
// `./generated/settings.ts`, emitted from `src-tauri/src/settings/schema/`
// by `settings::codegen` (ts-rs) and committed. CI regenerates and diffs it, so
// the mirror can no longer drift — the failure mode this file existed in for
// three years. Field names are still snake_case (serde's output) and optional
// fields still arrive as `null`, never `undefined`; that is the wire, not a
// convention this file chose.
//
// What is still hand-written below, and why:
//
//   * **Values.** Defaults, constants and helpers — `defaultSettings()`,
//     `LOCAL_DATA_TOOLS`, `harnessRow()`, the spawn-baked signature helpers.
//     A type generator emits no values.
//   * **Types that mirror OTHER Rust modules** — `CheckDef`/`ParserKind`
//     (`crate::checks`), the Harness-health family (`harness::health`,
//     `harness::chp`), `AuditDetectResult` (`audit`), the `Checks*` IPC
//     payloads (`ipc::commands`). Phase E's scope was the `Settings` tree in
//     the `schema/` tree; these keep their own `include_str!` tripwires.
//   * **TS-only aliases** derived FROM the generated types (never restating
//     them) — see the block under the re-export.

import type { AiTabId } from '../tabs/types';
import { harnessList, scopedFeatureOwner } from '../harness';

import type {
  AutoVerify,
  BackgroundPresetConfigWire,
  HarnessSettings,
  HarnessVersions,
  InjectionOverride,
  Settings,
  TabConfig,
  TabInjectionOverrides,
  TerminalBackgroundSettings,
  TerminalThemeSettings,
  ToolScope,
} from './generated/settings';
import DEFAULTS from './generated/defaults.json';

export type * from './generated/settings';

/// The three tab kinds, recovered from the generated `TabConfig` union.
///
/// Rust's `TabConfig` is `#[serde(tag = "kind")]`, so ts-rs renders it as
/// `{ kind: 'ai_tool' } & AiToolTabFields | …`: the discriminator sits on the
/// UNION, not on the variant's own type. Every consumer in this codebase
/// expects it on the variant (`cfg.kind === 'ai_tool'`, object literals that
/// spell `kind`), so these aliases put it back — by `Extract`ing from the
/// generated union rather than by restating one field of it.
export type AiToolTabConfig = Extract<TabConfig, { kind: 'ai_tool' }>;
export type ShellTabConfig = Extract<TabConfig, { kind: 'shell' }>;
export type PreviewTabConfig = Extract<TabConfig, { kind: 'preview' }>;

/// A per-tab or global terminal palette override's `custom` block.
///
/// Derived from the generated field rather than declared: Rust stores it as
/// `Option<HashMap<String, String>>`, and a hand-written `Record<string,
/// string>` beside it would be one more thing that can go stale.
export type ThemeColorsWire = NonNullable<TerminalThemeSettings['custom']>;

/// The three-state per-tab background override, as it crosses the wire.
///
/// `BackgroundOverride`'s (de)serialize is hand-written on the Rust side (the
/// literal `"disabled"` OR a full config object), so the field carries an
/// explicit `#[ts(type = …)]` seam. This alias reads the union back OFF that
/// generated field — `null` (inherit) removed, since the null case is spelled
/// at each use site.
export type BackgroundOverrideWire = NonNullable<
  AiToolTabConfig['background_override']
>;

/// V40 Phase B: `StatuslineSettings` is gone. The context-bar switch is one of
/// a harness's declared `ext` rows (`harness[<id>].ext[…]`), which
/// the Settings window renders from `harness_settings_schema` like every other
/// per-harness setting — see [`HarnessSettings`].

/// One harness's row out of `Settings.harness`, or the safe absent answer.
///
/// The map may legitimately not carry a harness (a fresh install that has never
/// saved, a harness this build learned about after the file was written), and
/// the backend resolves declared defaults for exactly that case. The window
/// must not render `undefined` into a checkbox, so this supplies the same
/// answers the backend would.
export function harnessRow(
  settings: Pick<Settings, 'harness'>,
  id: string,
): HarnessSettings {
  return (
    settings.harness?.[id] ?? {
      expose_commands: true,
      expose_code_audit: true,
      last_seen: '',
      last_verified: '',
      input_profile_status: 'unverified',
      auto_verify: null,
      ext: {},
    }
  );
}

/// Write one `ext` value on a harness's row, creating the row if absent.
///
/// Returns the same object it was handed, so a Svelte `$settings` update reads
/// as one expression. The window calls this from the generic form; nothing else
/// should touch `ext`.
export function setHarnessExt(
  settings: Settings,
  id: string,
  key: string,
  value: unknown,
): Settings {
  const row = { ...harnessRow(settings, id) };
  row.ext = { ...row.ext, [key]: value };
  settings.harness = { ...(settings.harness ?? {}), [id]: row };
  return settings;
}

/// **The harness native-tool gate's frozen wire key.**
///
/// Spelled ONCE, here, because it is a persisted form: it is a
/// `TabInjectionOverrides` field in every settings file on disk and a `/status`
/// row name, so renaming it is a migration. The backend calls the feature
/// `HarnessNativeGate`; this string is the era it shipped in.
///
/// V40 Phase F: this file is on the frontend identity allowlist for exactly
/// this constant and the reason above — the same rule Rust's
/// `IDENTITY_ALLOWLIST` applies to `settings/schema/mod.rs`. Nothing reads it as an
/// identity: which harness owns the feature is `harness_list`'s
/// `scoped_features`.
export const HARNESS_NATIVE_GATE_KEY = 'opencode_native_gate';

/// Every per-tab override cell, in the order the backend's `Feature::ALL`
/// publishes them — which is the order the badge popover and the Settings matrix
/// render.
///
/// `satisfies readonly (keyof TabInjectionOverrides)[]` and used by
/// [`allOffInjectionOverrides`] below, so a cell added to the interface without
/// being added here is a compile error at the constructor rather than a control
/// that silently ships missing from a new tab.
export const TAB_INJECTION_FEATURES = [
  'taint_latch',
  'spotlighting',
  'detection',
  'ssrf_guard',
  'fetch_budgets',
  'memory_quarantine',
  'native_web',
  'consumer_hygiene',
  'tool_steering',
  HARNESS_NATIVE_GATE_KEY,
] as const satisfies readonly (keyof TabInjectionOverrides)[];

/// V39: the row a NEWLY CREATED AI tab carries — every cell explicitly `'off'`.
///
/// Mirror of Rust `TabInjectionOverrides::all_off`. The app-wide levels (L1 and
/// every L2) ship on; this row is where protection is actually engaged, per tab,
/// from the tab's shield badge.
///
/// **Not the same thing as an absent row.** A cell missing from a settings file
/// still reads `'inherit'` on both sides — that is what keeps an upgraded
/// install's posture unchanged (schema step 34 → 35 writes the word explicitly).
export function allOffInjectionOverrides(): TabInjectionOverrides {
  return Object.fromEntries(
    TAB_INJECTION_FEATURES.map((f) => [f, 'off' as InjectionOverride]),
  ) as unknown as TabInjectionOverrides;
}

/// V32: the injection features whose value is **baked into a tab when it
/// launches**, so a change to one cannot reach a tab that is already running and
/// the user is owed a restart before it means anything.
///
/// Hand-mirror of Rust `Feature::spawn_baked`
/// (`src-tauri/src/settings/injection.rs`), in `Feature::ALL` declaration order
/// — the order Rust's `spawn_sig` emits them in, so the two are diffable by eye.
/// Note that `spawn_baked` is **not** the complement of "live": `spotlighting`
/// is both (per call at the proxy, and baked into the launch addendum by
/// `fact_promotion_block`), and the predicate answers "does the user owe this
/// control a restart?".
///
/// **One source, two readers** (#48, finding **F-27**, second instance). The
/// Settings window used to hand-mirror this set TWICE — once as a tab's L3 cells
/// (`restartShape`) and once as the app-wide L2 cells (`injectionAppShape`) —
/// and BOTH went stale when `spotlighting` became spawn-baked (finding M-3), so
/// flipping Spotlighting raised no in-window restart hint at all. Nothing on this
/// side can catch Rust growing a fifth member (a Rust-side `include_str!`
/// tripwire over this file is owed, exactly as for [`LOCAL_DATA_TOOLS`]), but
/// adding one HERE is a compile error until its app-wide cell is named in
/// `SPAWN_BAKED_L2` below, and both readers then pick it up for free.
///
/// `satisfies keyof TabInjectionOverrides` because every member must also carry a
/// per-tab L3 row: Rust's `Feature::has_tab_scope` is true for all four, and
/// [`spawnBakedTabOverrides`] reads them by these exact keys. A future
/// spawn-baked feature with no tab row would fail here rather than silently read
/// `undefined`.
export const SPAWN_BAKED_INJECTION_FEATURES = [
  'spotlighting',
  'native_web',
  'consumer_hygiene',
  'tool_steering',
  HARNESS_NATIVE_GATE_KEY,
] as const satisfies readonly (keyof TabInjectionOverrides)[];

/// One of the spawn-baked feature keys.
export type SpawnBakedInjectionFeature = (typeof SPAWN_BAKED_INJECTION_FEATURES)[number];

/// Each spawn-baked feature's APP-WIDE (L2) input, as Rust's `spawn_sig` reads
/// it. A `Record` over the union rather than a second array, so a member added
/// to the list above does not compile until its cell is named — which is the
/// drift the two hand-lists allowed.
///
/// `native_web`'s cell is the tri-mode STRING, not a boolean: `sensor` and `deny`
/// both resolve the feature "on" but launch a tab very differently, so a boolean
/// would lose a mode change. Same reconciliation Rust's `spawn_sig` makes.
///
/// V40 Phase B widened the reader from `OffloadSettings` to the whole
/// `Settings`: a harness-scoped feature's L2 is a row on `harness[<id>].ext`,
/// not a field in the offload block. [`HARNESS_NATIVE_GATE_KEY`] is that
/// feature's frozen wire key; V40 Phase F made the harness it belongs to a
/// registry lookup.
/// What a spawn-baked cell answers when the roster cannot decide it — the
/// window between mount and `harness_list`, and a build where no harness scopes
/// the feature.
///
/// A STRING, so it can never be confused with the `true`/`false` a real answer
/// takes, and stable, so two shapes computed while pending compare equal. The
/// Settings window additionally captures its restart baseline after the roster
/// lands (V40 review F-1), so this should not reach a comparison at all — it is
/// what makes the failure visible if it ever does.
export const ROSTER_PENDING = '(roster pending)';

const SPAWN_BAKED_L2: Record<
  SpawnBakedInjectionFeature,
  (s: Settings) => string | boolean
> = {
  spotlighting: (s) => s.offload.injection.spotlighting_enabled,
  native_web: (s) => s.offload.native_web_visibility,
  consumer_hygiene: (s) => s.offload.injection.consumer_hygiene_enabled,
  tool_steering: (s) => s.offload.injection.tool_steering_enabled,
  // V40 Phase F: WHICH harness holds this feature's app-wide cell is the
  // registry's answer (`harness_list`'s `scoped_features`), not a name written
  // here.
  //
  // **A roster that has not answered yet is NOT a value** (V40 review finding
  // F-1). This used to return the literal `true`, which is a guess that reads
  // as a real answer: a user who had turned the gate OFF opened Settings, the
  // restart baseline was captured in the window before `harness_list` resolved
  // (`[…, true]`), the first edit re-ran the derived against the real value
  // (`[…, false]`), and the section's "AI tabs launch differently — restart
  // them" hint fired with no user change behind it. The literal was also wrong
  // in the other direction for any future feature whose declared default is
  // `false`. [`ROSTER_PENDING`] cannot be mistaken for either, and the declared
  // default is read off the owner's field rather than assumed.
  [HARNESS_NATIVE_GATE_KEY]: (s) => {
    const list = harnessList();
    if (list.length === 0) return ROSTER_PENDING;
    const owner = scopedFeatureOwner(list, HARNESS_NATIVE_GATE_KEY);
    // No harness scopes the feature: nothing bakes it into a launch, so there
    // is no app-wide cell to compare. Its own sentinel, not a boolean that
    // would flip the moment a harness declaring it appeared.
    if (!owner) return ROSTER_PENDING;
    const stored = harnessRow(s, owner.harness.id).ext[owner.extKey];
    if (stored === undefined) {
      const declared = owner.harness.fields.find((f) => f.key === owner.extKey);
      return declared ? declared.default !== false : true;
    }
    return stored !== false;
  },
};

/// The app-wide L2 cell of every spawn-baked feature, in
/// [`SPAWN_BAKED_INJECTION_FEATURES`] order. The Settings window folds this into
/// its section-level restart hint; the L1 master rides alongside it there,
/// because it is not a feature and reaches every launch there is.
export function spawnBakedInjectionL2(s: Settings): (string | boolean)[] {
  return SPAWN_BAKED_INJECTION_FEATURES.map((f) => SPAWN_BAKED_L2[f](s));
}

/// One tab's L3 override for every spawn-baked feature, in
/// [`SPAWN_BAKED_INJECTION_FEATURES`] order.
///
/// A missing overrides object — or a missing key on one written by an older
/// build — reads as `'inherit'`, the same default the Rust resolver applies, so
/// such a tab compares equal to one that carries the row instead of looking like
/// a change.
export function spawnBakedTabOverrides(
  overrides: Partial<TabInjectionOverrides> | null | undefined,
): InjectionOverride[] {
  return SPAWN_BAKED_INJECTION_FEATURES.map((f) => overrides?.[f] ?? 'inherit');
}

/// V14 Phase F: `PreviewTabConfig` has neither `theme_override` nor
/// `background_override` — it has no terminal to theme at all (no PTY, no
/// xterm). Call sites that read/write those two fields off a `TabConfig`
/// looked up by id (`ConfigureTabDialog.svelte`, `SettingsApp.svelte`,
/// `terminals.ts`) narrow through this helper so they type-check against
/// the now-3-member union. In practice a Preview tab never reaches any of
/// them (it offers no "Configure…" — see `TabContextMenu.svelte` — and gets
/// no terminal entry — see `terminals.ts`'s `createTerminal` guard), so this
/// is a type-level narrowing, not a runtime behavior change.
export type ThemedTabConfig = AiToolTabConfig | ShellTabConfig;

export function asThemedTabConfig(t: TabConfig | undefined): ThemedTabConfig | undefined {
  return t && t.kind !== 'preview' ? t : undefined;
}

/// Project the shared subset of a `TerminalBackgroundSettings` into a
/// `BackgroundPresetConfigWire`. The reverse is achieved by spreading
/// the preset config into a `TerminalBackgroundSettings` with a fresh
/// `presets: []`, which the editor's `loadPreset` does inline.
export function toPresetConfig(
  s: TerminalBackgroundSettings,
): BackgroundPresetConfigWire {
  return {
    image: s.image,
    color: s.color,
    opacity: s.opacity,
    blur: s.blur,
    size: s.size,
    position: s.position,
    snapshot_lines: s.snapshot_lines,
  };
}

/// Type guard: distinguishes the `'disabled'` literal from the object
/// branch so callers can narrow safely without struct-vs-string runtime
/// checks scattered around the codebase.
export function isBackgroundDisabled(
  o: BackgroundOverrideWire | null,
): o is 'disabled' {
  return o === 'disabled';
}

/// Placeholder stamped into `defaultSettings()` before the backend's first
/// `settings-changed` broadcast arrives. Deliberately NOT the real version.
///
/// There used to be a `CURRENT_SCHEMA_VERSION = 21` here, described as
/// mirroring `src-tauri/src/settings/schema/mod.rs`. It drifted to nine versions
/// behind (the Rust constant reached 31 in V33 Phase E) without anything
/// noticing — which is the proof that no frontend logic depends on it. A
/// mirror that nothing checks does not stay a mirror, and a number that is
/// confidently wrong is worse than an obviously absent one, so the mirror is
/// gone rather than corrected: **the backend is the sole authority on schema
/// version.** Deleted by user decision, 2026-08-13.
const SCHEMA_VERSION_UNKNOWN = 0;

/// One harness capability's gate verdict, computed in Rust. Mirror of
/// `harness::contract::Gate`.
///
/// `reason` is ready to render and is non-empty exactly when `blocked` — so a
/// card never has to invent an explanation, and never shows an empty one.
export interface CapabilityGate {
  id: string;
  blocked: boolean;
  reason: string;
}

/// The `harness_versions_get` payload. Mirror of Rust
/// `ipc::commands::HarnessStatus`: the raw out-of-band record, the **computed**
/// gate verdicts for every gated capability, and (V35 Phase G) the whole
/// *Harness health* read-model.
export interface HarnessStatus {
  versions: HarnessVersions;
  capability_gates: CapabilityGate[];
  /// One entry per harness, in display order, each already ordered
  /// riskiest-tier-first. Computed in Rust — the panel groups and paints, it
  /// does not decide.
  harness_health: HarnessHealth[];
  /// A verify run is in flight, so "Run checks now" is a no-op and the panel
  /// keeps polling until it clears.
  verify_in_flight: boolean;
  /// V40 Phase F: the gated capability ids, keyed by the neutral CONTROL each
  /// one gates (mirror of Rust `contract::GATED_CONTROLS`). The window looks a
  /// control's id up here instead of holding one — see
  /// [`CONTROL_READ_ADVISOR`].
  gated_controls: Record<string, string>;
}

/// V35 Phase G: what cImp does when a capability is known-broken. Mirror of
/// Rust `harness::health::DegradationView`.
///
/// `label` is the sentence, written once in Rust — never re-derived from
/// `kind` here, which would be a fifth place for the four variants to be
/// spelled.
export interface DegradationView {
  /// `'silent' | 'visible_off' | 'fail_closed' | 'fallback'`. The dangerous one
  /// is `'silent'`.
  kind: string;
  label: string;
  /// What the user is told when a `'visible_off'` row breaks.
  user_message?: string | null;
  /// The capability id that takes over for a `'fallback'` row — a join key, so
  /// the panel can point at the row.
  fallback_to?: string | null;
}

/// V35 Phase G: what actually checks a capability. Mirror of Rust
/// `harness::health::Coverage`.
export interface Coverage {
  /// The L1 embedded-fixture canary id (which IS the capability id).
  canary?: string | null;
  /// The L2 live-probe id (likewise).
  probe?: string | null;
  /// The accepted-residual note: why nothing mechanical covers this row yet.
  waiver?: string | null;
  /// Degrades SILENTLY and is covered by prose alone — the weakest state on
  /// the board, and the one the panel must not let look like a canaried row.
  /// Computed in Rust; never re-derive it from the three fields above.
  unproven: boolean;
}

/// V35 Phase G: the last thing any check said about one capability. Mirror of
/// Rust `harness::health::VerifyView`.
///
/// `outcome` is `'pass' | 'fail' | 'unknown' | 'transition'` when `from_run`
/// (a full answer from a run made since launch), or `'no_failure'` when read
/// out of the stored record — which keeps FAILURES only, so a row it does not
/// name might equally have passed or have been uncheckable. Render
/// `'no_failure'` as the weaker statement it is, never as a pass.
export interface VerifyView {
  outcome: string;
  evidence: string;
  detail: string;
  at_ms: number;
  version: string;
  from_run: boolean;
}

/// V35 Phase G: one registry row as the panel shows it. Mirror of Rust
/// `harness::health::CapabilityHealth`.
export interface CapabilityHealth {
  /// The join key, displayed verbatim — it is the vocabulary the Advisor cards
  /// speak, so a user must be able to match a card to a row by eye.
  id: string;
  harness: string;
  /// `'A'`..`'D'` — the seam, which predicts how it breaks.
  tier: string;
  contract: string;
  /// What the user loses when this row is broken, in the user's words
  /// (`Capability::user_effect`). The status-first view shows this for a
  /// failing or gated-off row; `contract` is maintainer detail.
  user_effect: string;
  degradation: DegradationView;
  coverage: Coverage;
  /// The TCB column: security controls that EXECUTE inside this capability.
  /// Marked distinctly — these rows are not data pipes.
  controls: string[];
  /// The modules that break if this drifts.
  wired_in: string[];
  /// The Phase E gate verdict, when this capability has one at all. Absent =
  /// ungated, which is a different statement from "gated and currently fine".
  gate?: CapabilityGate | null;
  /// Absent = no check has ever spoken about this row.
  last_verify?: VerifyView | null;
}

/// V35 Phase G: the tally of a run made in this process. Mirror of Rust
/// `harness::health::RunView`. In-memory only — it is the visible consequence
/// of "Run checks now", and the only place a run is reported for a harness
/// with no persisted record.
export interface RunView {
  at_ms: number;
  version: string;
  pass: number;
  fail: number;
  unknown: number;
  transition: number;
  /// The time budget was spent before the L2 probes started, so they did not
  /// run. Recorded, never scored.
  capped: boolean;
}

/// V35 Phase I: one tab whose spawn-baked harness artifact is out of step with
/// the running cImp build. Mirror of Rust `harness::chp::StalePlugin`.
///
/// The generated plugin is written to disk at TAB LAUNCH and outlives the binary
/// that wrote it, so upgrading cImp with a tab still open leaves an old artifact
/// talking to new loopback code. V32 met that four times as "needs a FRESH TAB
/// or it reads as a failure"; the `chp` field on the wire is what turns it into
/// a report. Nothing is refused on the strength of it.
///
/// `note` is the sentence, written once in Rust — never re-derived here from
/// `kind`/`seen_chp`/`expected`, which would be a second place for the rule to
/// be wrong.
export interface StalePlugin {
  tab: string;
  agent: string;
  /// The CHP version this tab's artifact actually sends. `0` = it sends none,
  /// i.e. it predates CHP entirely.
  seen_chp: number;
  /// The CHP version this build writes into a freshly generated artifact.
  expected: number;
  /// `'old_plugin' | 'new_plugin' | 'harness_version'`.
  kind: string;
  note: string;
}

/// V35 Phase G: one harness's header plus its rows. Mirror of Rust
/// `harness::health::HarnessHealth`.
export interface HarnessHealth {
  /// A registry harness id — passed straight back to `harness_run_checks`.
  harness: string;
  label: string;
  last_seen: string;
  /// Absent for a harness with no verified column at all —
  /// deliberately not `''`, which would read as "verified against nothing".
  last_verified?: string | null;
  /// The persisted Phase F record, when this harness has one.
  auto_verify?: AutoVerify | null;
  /// The last run made since launch, when there is one.
  last_run?: RunView | null;
  /// V35 Phase I: tabs of this harness running an out-of-step artifact. Empty
  /// is the normal state and renders as nothing.
  stale_plugins: StalePlugin[];
  capabilities: CapabilityHealth[];
}

/// The `VerifyView.outcome` token meaning "the stored record did not name this
/// capability among its failures" — which is NOT a pass. Spelled here because
/// `harness::health::tests::the_health_field_names_reach_the_frontend` fails
/// the Rust build if the panel stops knowing the distinction.
export const OUTCOME_NO_FAILURE = 'no_failure';

/// The CONTROL name the redundant-read advisor's gate is published under
/// (`HarnessStatus.gated_controls`).
///
/// **V40 Phase F (locked decision 27).** This used to be
/// `CAP_PRETOOLUSE_DENY` — the capability id itself, a harness-namespaced hook
/// name spelled in TypeScript so the window could join on it. The id now
/// travels in the payload keyed by this neutral name, and
/// `harness::contract::tests::the_gated_capability_ids_reach_the_frontend`
/// fails the Rust build if a harness-namespaced gated id reappears in this
/// file.
export const CONTROL_READ_ADVISOR = 'read_advisor';

/// V39 Phase B: the capability id cross-harness delegation is gated on — the
/// registry's first harness-NEUTRAL row (`Harness::Any`). Shared verbatim with
/// Rust's `contract::CAP_DELEGATION_WORKER` and pinned by the same test, which
/// permits THIS one to be spelled here precisely because it names no vendor. A
/// blocked verdict means no tab can be driven and no `delegate_task_*` tool is
/// advertised; the reason string says which spike recorded what.
export const CAP_DELEGATION_WORKER = 'delegation.worker';

/// Whether `status` says a capability is gated off. A lookup, deliberately not
/// a rule: the verdict was computed by `harness::contract::gate` against the
/// SAME settings the tab spawn uses, so the Settings toggle and the installed
/// hook cannot disagree. A capability with no gate (or a payload that has not
/// arrived yet) is not blocked.
export function capabilityBlocked(
  status: HarnessStatus | null | undefined,
  id: string,
): CapabilityGate | null {
  return status?.capability_gates.find((g) => g.id === id && g.blocked) ?? null;
}

/// The verdict for the capability a neutral CONTROL name gates, resolved
/// through `HarnessStatus.gated_controls`.
///
/// **A control the payload does not carry fails CLOSED** (V40 review finding
/// M-4). The window used to do `gated_controls?.[CONTROL] ?? ''` and hand the
/// empty string to [`capabilityBlocked`], which answers "not blocked" — so a
/// control renamed in Rust, or dropped from `GATED_CONTROLS`, silently
/// UN-GATED the toggle it protects. That toggle installs a `PreToolUse` hook on
/// a contract the E1 spike may have recorded as broken; the whole point of the
/// gate is that it is the one thing standing between a `fail` and a hook that
/// denies the model's reads.
///
/// `null` while the payload has not arrived at all — that is "not yet", not "no
/// gate", and it is the pre-Phase-E behaviour (`snapshot` is null then too).
export function controlBlocked(
  status: HarnessStatus | null | undefined,
  control: string,
): CapabilityGate | null {
  if (!status) return null;
  const id = status.gated_controls?.[control];
  if (!id) {
    return {
      id: control,
      blocked: true,
      reason:
        'this build cannot find the gate for this control (the harness registry did not ' +
        'publish it), so it stays off rather than running ungated',
    } as CapabilityGate;
  }
  return capabilityBlocked(status, id);
}

/// One of the four reserved AI-tool tab ids. Wire format mirrors the
/// backend's `AiTabId` enum (kebab-case strings). Canonical definition
/// lives in `../tabs/types` (alongside `AI_TABS` and the type guards);
/// re-exported here so settings consumers keep a single source of truth.
export type { AiTabId };

/// Which built-in parser decodes a check's output (mirror of Rust
/// `ParserKind`). Wire format is kebab-case.
export type ParserKind =
  | 'cargo-json'
  | 'tsc'
  | 'eslint-json'
  | 'pytest'
  | 'cargo-test'
  | 'jest-json'
  | 'sarif'
  | 'go'
  | 'go-test-json'
  | 'dotnet'
  | 'junit-xml'
  | 'typos-jsonl'
  | 'knip-json'
  | 'machete-text'
  | 'regex-custom'
  | 'generic-gcc';

/// One configured project check the `run_check` MCP tool can run (mirror of
/// Rust `CheckDef`). `cmd` is the full shell command line (cwd = project
/// root); `name` is what a model-supplied `run_check` tool call selects by.
export interface CheckDef {
  name: string;
  cmd: string;
  parser: ParserKind;
  timeout_secs: number;
  /// V22 Phase B: run `cmd` in this directory instead of the project root — a
  /// path relative to the root, confined strictly beneath it (absolute/escaping
  /// paths rejected). Diagnostic file paths are re-rooted back to the project
  /// root, so the report stays root-relative. Always present on the wire
  /// (Rust serializes it unconditionally); `null` means "run at the root".
  cwd: string | null;
  /// V22 Phase B: environment variables forced on the spawned child, as ordered
  /// `[key, value]` pairs (mirror of Rust `Vec<(String, String)>`).
  env: [string, string][];
  /// V22 Phase B2: when set, the parser reads this file's content after the run
  /// instead of stdout — for junit-xml / sarif tools that write a report to
  /// disk. Resolved relative to the check's working directory (`cwd` if set,
  /// else the project root), confined strictly beneath the root — matching how
  /// tools document their output paths (e.g. `mvn` writes `target/surefire-reports`
  /// under its module dir). For back-compat, a `cwd`-set config whose path only
  /// exists at the old root-relative location falls back to that. `null` means
  /// "parse stdout".
  report_file: string | null;
  /// V22 Phase C: the regex for the `regex-custom` parser (ignored by every
  /// other parser). Named groups `file`/`line`/`message` are mandatory,
  /// `col`/`severity` optional; validated at save time (see the Rust
  /// `parsers::validate_pattern`). Always present on the wire; `null` when
  /// unused. Mirror of Rust `Option<String>`.
  pattern: string | null;
  /// V22 Phase D: `true` when this entry was created by language auto-detection
  /// (`checks/detect.rs`) rather than hand-authored. Re-detection may refresh
  /// `auto === true` entries but never touches a `false` one. The ChecksEditor
  /// (Phase E) MUST clear this flag (set `false`) whenever the user edits an
  /// auto entry, so a later re-detection stops fighting the manual change.
  /// Mirror of Rust `CheckDef::auto`; always present on the wire.
  auto: boolean;
}

/// V22 Phase D: one auto-detection proposal (mirror of Rust
/// `checks::detect::Proposal`). Returned by the `checks_detect` IPC; the Phase E
/// editor renders `check` with a checkbox, greying items where `valid === false`
/// and showing `reason`.
export interface ChecksProposal {
  check: CheckDef;
  /// Human ecosystem label (`"Rust"`, `"Go"`, `"TypeScript/JavaScript"`, … ).
  ecosystem: string;
  /// What triggered it — the marker file(s) and/or the code-graph stat.
  evidence: string;
  /// Whether the machine could validate it (marker present + binary on PATH).
  valid: boolean;
  /// Why an invalid proposal can't run; `null` when `valid`.
  reason: string | null;
}

/// V22 Phase D: the passive-nudge payload (mirror of Rust
/// `ChecksSuggestion`). `count` is how many VALID proposals detection found for
/// a project whose `checks` is empty; `dismissed` reflects the per-project
/// `checks_suggestion_dismissed` flag. The chip shows only when
/// `count > 0 && !dismissed`.
export interface ChecksSuggestion {
  count: number;
  dismissed: boolean;
  /// Mirror of `checks_auto_configure` — the chip notes when auto-apply is on.
  auto_configure: boolean;
}

/// V22 Phase D: the `checks_apply_proposals` result — the names actually written
/// (added or refreshed) after the `auto`-ownership merge. Mirror of Rust
/// `ApplySummary`.
export interface ChecksApplySummary {
  applied: string[];
}

/// V22 Phase E: the `checks_test` dry-run result the ChecksEditor renders inline
/// (mirror of Rust `checks::ChecksTestResult`). `diag_count` is the number of
/// deduplicated diagnostic groups; `diagnostics` is the first few of them.
/// `stdout_bytes`/`stderr_bytes` are the raw captured output sizes — the
/// "did the command produce output at all?" signal `classifyTestResult`
/// (`checksEditor.ts`) uses to flag a wrong-parser config (output produced, zero
/// diagnostics) apart from a genuinely clean run. `error` is set (and the rest
/// zeroed) when validation/spawn failed before a report was produced.
export interface ChecksTestResult {
  exit_code: number | null;
  duration_ms: number;
  timed_out: boolean;
  diag_count: number;
  stdout_bytes: number;
  stderr_bytes: number;
  diagnostics: ChecksTestDiag[];
  error: string | null;
}

/// V22 Phase E: one diagnostic group summarized for the Test-button preview
/// (mirror of Rust `checks::TestDiag`).
export interface ChecksTestDiag {
  severity: string;
  message: string;
  /// `"file:line"` sample locations; a location-less group has an empty list.
  sites: string[];
}

/// V40 Phase B: the per-harness local-provider group is gone. Its fields are a
/// harness's
/// declared `ext` rows (`local.base_url` / `local.auth_token` /
/// `local.model_alias`), rendered by the generic per-harness form and redacted
/// in backend logs by the declaration's `secret` column.

/// V23 Phase A: the `audit_detect_tool` IPC result (mirror of Rust
/// `AuditDetectResult`). Display-only — the Detect button renders it inline and
/// never writes the resolved path back into the tool's `path` field.
export interface AuditDetectResult {
  found: boolean;
  path: string | null;
  version: string | null;
  error: string | null;
}

/// V8-02: native + MCP tool names treated as local-data (denied to cloud
/// backends by default). Mirrors Rust `LOCAL_DATA_TOOLS`
/// (`src-tauri/src/settings/schema/mcp.rs`) — kept in the same order so the two are
/// diffable by eye.
///
/// **This is a hand-mirrored constant with no compile-time link to the Rust
/// one** (#48, finding **F-27**): the Settings window WRITES this list into a
/// backend's `tool_scope` when its cloud flag is toggled, so a stale copy here
/// silently narrows the exclusion Rust intended — which is exactly what F-27
/// was: `run_check` joined the Rust set for finding **F-12** and this array was
/// left at six entries, so the `LOCAL_DATA_TOOLS` half of F-12's fix had no
/// production effect (no hole opened — `BackendGate`'s call-time rule still
/// refuses `run_check` on a remote backend). A Rust-side `include_str!` tripwire
/// over this file is owed; until it exists, any edit to Rust's
/// `LOCAL_DATA_TOOLS` has to be repeated here in the same commit.
///
/// Consumers must treat it as a SET, never by length — see [`toolScopeMode`].
export const LOCAL_DATA_TOOLS = [
  'read_file',
  'list_dir',
  'code_search',
  'run_command',
  'run_check',
  'filesystem',
  'git',
];

/// The "web/docs only" preset: everything except the local-data set. The one
/// place that materializes it, so a writer cannot ship a list that
/// [`toolScopeMode`] would then fail to recognize.
export function localDataExcludedScope(): ToolScope {
  return { mode: 'allexcept', tools: [...LOCAL_DATA_TOOLS] };
}

/// Which tool-scope preset a backend's scope corresponds to: `all` (no
/// restriction), `web` (the web/docs-only preset — everything except the
/// local-data set), or `custom` (a hand-picked list).
///
/// F-27: this compares **set membership in both directions**, not array length.
/// The length test it replaces made a *correct* list read as "custom" the moment
/// Rust's set grew and this mirror lagged — and clicking the "web/docs only"
/// radio then wrote the shorter list back, silently dropping the new member from
/// the exclusion. Order and duplicates are irrelevant to what the scope means,
/// so they are irrelevant here; a list that merely CONTAINS the preset plus
/// extras is stricter than the preset and stays `custom`.
export function toolScopeMode(scope: ToolScope): 'all' | 'web' | 'custom' {
  if (scope.mode === 'all') return 'all';
  if (scope.mode !== 'allexcept') return 'custom';
  const excluded = new Set(scope.tools);
  const preset = new Set(LOCAL_DATA_TOOLS);
  const coversPreset = LOCAL_DATA_TOOLS.every((t) => excluded.has(t));
  const noExtras = [...excluded].every((t) => preset.has(t));
  return coversPreset && noExtras ? 'web' : 'custom';
}


/// The reserved id of the default Shell tab — mirror of
/// `crate::settings::SHELL_DEFAULT_TAB_ID`. User-created shell tabs use
/// uuid-based ids that never collide with it.
///
/// V40 Phase F: the reserved AI tab ids that used to sit beside it are gone.
/// They are the registry's (`harness_list`'s `tab_ids`), and mirroring them
/// here was the frontend declaring the roster — see `src/lib/harness.ts`.
export const SHELL_DEFAULT_TAB_ID = 'shell-default-1';

/// Look up a tab entry by id. Returns undefined for unknown ids; callers
/// treat that as a transient state (tab gone).
export function findTab(settings: Settings, id: string): TabConfig | undefined {
  return settings.tabs.find((t) => t.id === id);
}

/// Index of the tab entry; useful when callers need to mutate via array
/// index (e.g. spreading the new entry into a new array for state setters).
export function findTabIndex(settings: Settings, id: string): number {
  return settings.tabs.findIndex((t) => t.id === id);
}

/// The store's pre-init settings value, sourced from `generated/defaults.json`.
///
/// That file is `serde_json::to_string_pretty(&Settings::default())`, written
/// by `settings::codegen` and committed — so these ARE the backend's defaults
/// rather than a 400-line hand-kept restatement of them that has to be edited
/// twice. (Writing it out found one field the old restatement had never
/// carried at all: `pricing_seeded_generation`. That is the class of bug this
/// replaces.)
///
/// The store holds this only until the backend's first `settings-changed`
/// broadcast, which it re-emits on init — a few milliseconds, and nothing
/// renders from it except as a shape.
///
/// A fresh deep clone per call, because callers mutate what they get back.
export function defaultSettings(): Settings {
  const s = JSON.parse(JSON.stringify(DEFAULTS)) as Settings;

  // ── The four deliberate divergences from `Settings::default()` ──────────
  //
  // Each is a decision on the record, not drift. Everything NOT listed here
  // now comes from Rust verbatim.

  /// The backend is the sole authority on schema version, so the placeholder
  /// must be unmistakably a placeholder — see [`SCHEMA_VERSION_UNKNOWN`].
  /// (User decision, 2026-08-13.)
  s.schema_version = SCHEMA_VERSION_UNKNOWN;

  /// V40 Phase F (locked decision 7): EMPTY, deliberately. Rust's default
  /// seeds the first registered harness's reserved tab; carrying that here
  /// would be the frontend declaring the roster a second time. Every tab
  /// surface reads the backend's answer, and an empty list for one frame is
  /// the same state a fresh install has before its first save.
  s.enabled_ai_tabs = [];

  /// V40 Phase B: empty for the same reason. The backend materializes a row
  /// per registered harness at its declared defaults, and `harnessRow()`
  /// answers those same defaults for a key that is not there.
  s.harness = {};

  /// Seeded Rust-side (`default_llm_pricing`) and read/written out of band
  /// through the `llm_pricing_*` IPC, never through `settingsUpdate` — so a
  /// local copy of the price table would be dead weight in the bundle and a
  /// second place for it to be stale.
  s.llm_pricing = [];

  return s;
}
