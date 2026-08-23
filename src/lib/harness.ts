/// V40 Phase F — **the harness registry, over IPC** (locked decisions 7, 11
/// and 27).
///
/// THE single place the frontend learns that harnesses exist. Before this
/// module the roster was declared eight more times in `src/`: `AI_TABS`,
/// `RESERVED_AI_TAB_IDS`, `isShellTab`'s three-way `!==` chain, two `order`
/// arrays in the Settings window, `delegation.ts`'s `HARNESS_LABELS` and its
/// second `tabHarness` classifier, a `{:else if}` body per tab id, and a CSS
/// class per harness. None of those could see the Rust registry, so a third
/// harness meant finding all of them — and missing one meant a tab that
/// existed but had no settings section, or a lane with no colour.
///
/// Everything here is DATA the backend sent. This file declares no harness id,
/// no label, no binary and no per-harness string, with exactly one documented
/// exception ([`BOOTSTRAP_RESERVED_TAB_IDS`], below), which the parity test
/// checks against the registry in both directions.
///
/// # How it loads
///
/// `loadHarnesses()` runs once per window at startup. Both windows mount
/// synchronously, so there is a first paint before the answer arrives; every
/// consumer is written to render *nothing* rather than something wrong in that
/// window, and the one question that cannot wait (is this tab id a shell?) has
/// the sanctioned synchronous fallback locked decision 7 describes.
import { invoke } from '@tauri-apps/api/core';
import { get, writable } from 'svelte/store';

/// A registry id. Deliberately `string`: the frontend never branches on its
/// value (locked decision 10(a)'s rule, applied here), it looks it up. A build
/// that meets an id it has never heard of must render it, not guess.
export type HarnessId = string;

/// What core mounts for a harness beyond the neutral path — mirror of Rust
/// `harness::registry::HarnessFeature`, wire tokens from
/// `harness::info::feature_token`.
///
/// This is core's OWN vocabulary (which panels exist), not harness identity,
/// which is why it is a closed union here and a harness id is not. The parity
/// test asserts every token the fixture carries is in [`HARNESS_FEATURES`].
export type HarnessFeature =
  | 'session_usage'
  | 'context_bar'
  | 'file_artifact'
  | 'usage_push'
  | 'local_provider_config';

/// Every feature this build knows how to mount, as data — the runtime half of
/// the union above, so the parity test can check the two against the fixture.
export const HARNESS_FEATURES: readonly HarnessFeature[] = [
  'session_usage',
  'context_bar',
  'file_artifact',
  'usage_push',
  'local_provider_config',
] as const;

/// One env var a harness synthesizes for a local-provider tab. Mirror of Rust
/// `harness::info::LocalProviderVarView`.
export interface LocalProviderVar {
  name: string;
  /// The `Settings.harness[<id>].ext` key whose value fills it; `null` for a
  /// credential, which the preview prints masked.
  extKey: string | null;
  /// Render the row only when the key's value is non-empty.
  onlyWhenSet: boolean;
}

/// The strings and per-harness UI facts the window used to hard-code. Mirror of
/// Rust `harness::info::AffordancesView` (locked decision 27).
export interface HarnessAffordances {
  newSessionCommand: string | null;
  toolListRefresh: string | null;
  webTools: string[];
  stateDirs: string[];
  installHint: string | null;
  docsUrl: string | null;
  attachmentFormat: string;
  localProvider: LocalProviderVar[] | null;
  localProviderNote: string | null;
  localProviderConfigNote: string | null;
  /// The `ext` keys the Offload card's local-provider block writes: the derived
  /// provider object, and the auto-sync flag. `null` for a harness that does
  /// not declare `local_provider_config` — and the block does not render.
  localProviderConfigBlockKey: string | null;
  localProviderConfigAutoKey: string | null;
  statuslineRows: number;
  attributionTemplate: string;
  injectMechanism: string | null;
  defaultCommand: string;
  commandExample: string | null;
  accent: string;
  tier: string;
}

/// What a declared field HOLDS — mirrors Rust `harness::plugin::SettingKind`.
///
/// `json` is the escape hatch for a value cImp itself writes (OpenCode's
/// derived `local-llama` provider block): stored, round-tripped, and
/// deliberately NOT rendered, because its shape is the plugin's business.
export type SettingKind = 'bool' | 'int' | 'text' | 'path' | 'enum' | 'json';

/// Every kind this build knows, as data — the runtime half of the union above,
/// so the parity test can check the two against the fixture in both directions
/// (V40 review finding F-5).
///
/// Nothing checked this before, so a kind added in Rust passed the whole
/// frontend suite and `HarnessExtForm`'s `{:else}` rendered it as a TEXT BOX
/// that wrote a `String` into a key whose declared kind was something else.
export const SETTING_KINDS: readonly SettingKind[] = [
  'bool',
  'int',
  'text',
  'path',
  'enum',
  'json',
] as const;

/// One declared `ext` field. Mirror of Rust `harness::info::SettingFieldView`.
export interface SettingFieldView {
  /// The key inside `Settings.harness[<id>].ext`.
  key: string;
  kind: SettingKind;
  /// Allowed values for `kind === 'enum'`; empty otherwise.
  options: string[];
  label: string;
  /// One sentence under the control. May be empty.
  hint: string;
  /// The value an absent key reads as — what the form shows before the user has
  /// ever touched it.
  default: unknown;
  /// Flipping it needs a tab restart; the form says so.
  spawn_baked: boolean;
  /// A credential: the form masks it behind a Show/Hide button.
  secret: boolean;
  /// This row belongs on the harness's CUSTOM-PROVIDER tab page (the tab named
  /// by `HarnessInfo.provider_tab_id`) rather than on its primary tab page.
  ///
  /// A declaration, not a prefix match on the key: the window never spells an
  /// `ext` key, so which fields describe the custom provider is the plugin's
  /// answer to give.
  provider_tab: boolean;
}

/// One registered harness. Mirror of Rust `harness::info::HarnessInfo`.
export interface HarnessInfo {
  id: HarnessId;
  label: string;
  /// Reserved built-in tab ids, in canonical order.
  tab_ids: string[];
  /// The reserved tab that launches against a custom provider, or `null` for a
  /// harness with no such tab. Its page carries the `provider_tab` fields.
  provider_tab_id: string | null;
  /// Binaries whose file stem identifies this harness.
  binaries: string[];
  features: HarnessFeature[];
  consumer: string;
  affordances: HarnessAffordances;
  /// Declared `ext` fields, in declaration order. Empty is an ordinary answer:
  /// such a harness gets an empty section and no UI work at all.
  fields: SettingFieldView[];
  /// Injection features whose app-wide switch is this harness's `ext` row.
  scoped_features: ScopedFeatureView[];
}

/// One injection feature scoped to a harness. Mirror of Rust
/// `harness::info::ScopedFeatureView`.
export interface ScopedFeatureView {
  /// The feature's stable wire key — the same string the per-tab override cells
  /// use.
  feature: string;
  /// The key inside `Settings.harness[<id>].ext` holding its app-wide value.
  extKey: string;
}

/// The registered harnesses, once the backend has answered.
///
/// Starts EMPTY rather than with a guessed roster: a synchronous fallback would
/// be the frontend re-declaring the registry, which is the thing this module
/// removes.
export const harnesses = writable<HarnessInfo[]>([]);

/// **Locked decision 7's sanctioned synchronous fallback, and the only harness
/// identity this file declares.**
///
/// `isReservedAiTabId` is asked on the terminal hot path (spawn, resize,
/// keystroke routing) by plain module functions with no store to await, and
/// both windows mount before `loadHarnesses()` can answer. Getting it wrong for
/// one frame is not cosmetic: a built-in AI tab misread as a shell gets the
/// shell close/restart/keystroke behaviours.
///
/// It is static data that cannot disagree with the registry — and
/// `harness.test.ts` asserts it equals the registry's canonical tab ids in both
/// directions against the committed fixture, so it cannot silently fall behind
/// either. Once the IPC answers, the live list is used and this is never read
/// again.
const BOOTSTRAP_RESERVED_TAB_IDS: readonly string[] = ['claude', 'claude-local', 'opencode'];

/// Where the roster fetch stands.
///
/// **`'loading'` and `'failed'` are different answers and consumers must be able
/// to tell them apart** (V40 review finding F-3 / frontend H-2). Until this
/// existed the store simply stayed `[]` on failure, forever and silently, so a
/// window whose `harness_list` call had failed was indistinguishable from one
/// still waiting — and what a user saw was the per-harness settings form never
/// mounting, the per-tab *Use local provider* checkbox missing, the MCP access
/// columns gone and the usage widget hidden, with nothing anywhere saying why.
export type HarnessLoadState = 'loading' | 'ready' | 'failed';

/// See [`HarnessLoadState`]. Read as `$harnessLoadState` in components.
export const harnessLoadState = writable<HarnessLoadState>('loading');

/// How many times [`loadHarnesses`] tries before giving up, and how long it
/// waits between tries. Short and few: the backend is in the same process and
/// the realistic failure is a startup race, not a network.
const LOAD_ATTEMPTS = 3;
const LOAD_BACKOFF_MS = [200, 600];

/// Fetch the registry, retrying a transient failure.
///
/// Still best-effort in the sense that it never throws — a window that refuses
/// to open over a harness list is worse than one missing a section — but it no
/// longer fails *silently*: [`harnessLoadState`] ends at `'failed'` and the
/// Settings window says so, with a button that calls this again.
export async function loadHarnesses(): Promise<void> {
  harnessLoadState.set(get(harnesses).length > 0 ? 'ready' : 'loading');
  for (let attempt = 0; attempt < LOAD_ATTEMPTS; attempt++) {
    try {
      const list = await invoke<HarnessInfo[]>('harness_list');
      // An EMPTY roster is not a successful load: the registry always has at
      // least one entry, so `[]` means the call answered something this build
      // cannot use. Treated as a failure so the retry runs and the banner
      // shows, rather than rendering a window with every harness section gone.
      if (list && list.length > 0) {
        harnesses.set(list);
        harnessLoadState.set('ready');
        return;
      }
      console.error('harness_list returned an empty roster');
    } catch (e) {
      console.error('harness_list failed:', e);
    }
    const wait = LOAD_BACKOFF_MS[attempt];
    if (wait !== undefined) {
      await new Promise((r) => setTimeout(r, wait));
    }
  }
  harnessLoadState.set(get(harnesses).length > 0 ? 'ready' : 'failed');
}

/// The current list, for the module functions that cannot take a store
/// subscription (terminal glue, pure helpers called from event handlers).
/// Svelte components read `$harnesses` instead, so they re-render when it
/// arrives.
export function harnessList(): HarnessInfo[] {
  return get(harnesses);
}

// ── lookups ─────────────────────────────────────────────────────────────────
//
// All of these take the list explicitly so they are unit-testable against a
// fixture, and each has a store-reading wrapper below for the callers that have
// no list in hand.

/// The harness with this id, or `null`. An id nobody declared is `null` — never
/// a shipped harness (locked decision 2, frontend half).
export function findHarness(list: readonly HarnessInfo[], id: string | null | undefined): HarnessInfo | null {
  const key = (id ?? '').trim();
  if (!key) return null;
  return list.find((h) => h.id === key) ?? null;
}

/// The harness that owns a reserved built-in tab id. `null` for a user-created
/// `ai-<uuid>` tab — those are classified by their command.
export function findHarnessByTabId(
  list: readonly HarnessInfo[],
  tabId: string | null | undefined,
): HarnessInfo | null {
  const key = (tabId ?? '').trim();
  if (!key) return null;
  return list.find((h) => h.tab_ids.includes(key)) ?? null;
}

/// The harness a configured command launches, compared on the path's file stem
/// so `opencode`, `C:\bin\opencode.exe` and `/usr/local/bin/opencode.cmd` all
/// resolve. Mirror of Rust `HarnessId::from_command`.
///
/// **Deliberately MORE forgiving than the Rust twin on two inputs** (V40 review
/// L-11), and this is where the difference is written down: Rust's
/// `Path::file_stem` does not trim, and on Linux it does not split a Windows
/// path. So a trailing space typed into the Settings command box, and a
/// Windows-written absolute path read on Linux, resolve here and not there.
/// Both are the SAFE direction for this side — the window offers the harness's
/// affordances for a command the backend will treat as a shell, which shows the
/// user their typo instead of hiding it — and neither reaches a grant: every
/// gate that matters (sandbox rows, MCP grants, delegation) is answered
/// backend-side. `harness.test.ts` pins both inputs so the divergence stays
/// deliberate rather than becoming a surprise.
///
/// **`null` is a first-class answer**: a tab whose command is nobody's binary is
/// a shell tab, not the default harness (locked decision 2). Both separators are
/// accepted because Windows is the primary platform and a config written there
/// can be read anywhere.
export function findHarnessByCommand(
  list: readonly HarnessInfo[],
  command: string | null | undefined,
): HarnessInfo | null {
  if (!command) return null;
  const base = command.trim().replace(/[\\/]+$/, '').split(/[\\/]/).pop() ?? '';
  // `Path::file_stem` strips one trailing extension, and only when it isn't the
  // whole name (a bare dotfile has no stem to speak of).
  const stem = base.replace(/(?!^)\.[^.]*$/, '').toLowerCase();
  if (!stem) return null;
  return list.find((h) => h.binaries.some((b) => b.toLowerCase() === stem)) ?? null;
}

/// Every reserved built-in AI tab id, in canonical order across harnesses —
/// the registry's declaration order flattened through `tab_ids`.
///
/// Falls back to [`BOOTSTRAP_RESERVED_TAB_IDS`] while the list is empty; see
/// that constant for why this one question gets a fallback and nothing else
/// does.
export function reservedAiTabIds(list: readonly HarnessInfo[]): string[] {
  if (list.length === 0) return [...BOOTSTRAP_RESERVED_TAB_IDS];
  return list.flatMap((h) => h.tab_ids);
}

/// True when `tabId` is one of the reserved built-in AI tab ids.
export function isReservedAiTabId(list: readonly HarnessInfo[], tabId: string): boolean {
  return reservedAiTabIds(list).includes(tabId);
}

/// The tab id a fresh window lands on: the first reserved tab of the first
/// registered harness. Empty while the list is loading, which callers treat as
/// "no selection yet" rather than as a tab id.
export function defaultTabId(list: readonly HarnessInfo[]): string {
  return reservedAiTabIds(list)[0] ?? '';
}

/// The display name for a harness id. An id this build has never heard of
/// renders as ITSELF rather than as a guess — which is also how a harness added
/// after this build renders.
export function labelForHarness(list: readonly HarnessInfo[], id: string | null | undefined): string {
  const key = (id ?? '').trim();
  if (!key) return 'another harness';
  return findHarness(list, key)?.label ?? key;
}

/// The name a reserved built-in tab carries in headers, nav buttons and the
/// terminal's own messages.
///
/// A harness's first tab id is the harness itself (`claude` → "Claude Code");
/// a further reserved tab is a VARIANT of it, and its id is the harness id plus
/// a suffix (`claude-local` → "Claude Code (local)"). Derived rather than
/// declared: the suffix is already in the tab id the registry publishes, and a
/// second label table would be one more thing to keep in step.
export function labelForTabId(list: readonly HarnessInfo[], tabId: string): string {
  const h = findHarnessByTabId(list, tabId);
  // **The id itself, never the empty string** (V40 review F-2 / frontend H-1).
  // `reservedAiTabIds` has a bootstrap fallback and this did not, so between
  // mount and the roster's arrival the Settings window rendered three AI-tab
  // enable checkboxes with NO label — and clicking one kills that tab's PTY and
  // drops its scrollback. Same posture `labelForHarness` already takes for an
  // id it does not know: render it, do not guess and do not vanish.
  if (!h) return (tabId ?? '').trim();
  const prefix = `${h.id}-`;
  return tabId.startsWith(prefix) ? `${h.label} (${tabId.slice(prefix.length)})` : h.label;
}

/// Every harness that declares `feature`, in registry order.
export function harnessesWith(
  list: readonly HarnessInfo[],
  feature: HarnessFeature,
): HarnessInfo[] {
  return list.filter((h) => h.features.includes(feature));
}

/// The CSS colour a harness's rows and glyphs are accented with, or `''` for a
/// harness that declares none — including one this build does not know, which
/// renders in the default colour rather than in somebody else's.
export function accentFor(list: readonly HarnessInfo[], id: string | null | undefined): string {
  return findHarness(list, id)?.affordances.accent ?? '';
}

/// The harness that owns `feature`'s app-wide switch, and the `ext` key holding
/// it — or `null` when no harness scopes that feature.
///
/// The frontend's spawn-signature mirror needs the app-wide value of every
/// spawn-baked injection feature, and one of them lives on a harness's `ext`
/// rather than in core (locked decision 6). Asking the registry is what keeps
/// `settings/types.ts` from naming the harness that happens to have it.
export function scopedFeatureOwner(
  list: readonly HarnessInfo[],
  feature: string,
): { harness: HarnessInfo; extKey: string } | null {
  for (const h of list) {
    const row = h.scoped_features.find((f) => f.feature === feature);
    if (row) return { harness: h, extKey: row.extKey };
  }
  return null;
}

/// The registered harnesses' labels, as a sentence fragment: `"Claude Code"`,
/// `"Claude Code / OpenCode"`, `"Claude Code / OpenCode / Codex"`.
///
/// Every piece of copy that used to enumerate the shipped harnesses by hand
/// ("restart the Claude/OpenCode tab") interpolates this instead, so the
/// sentence a user reads is the roster the app actually has.
export function harnessLabels(list: readonly HarnessInfo[], sep = ' / '): string {
  return list.map((h) => h.label).join(sep);
}

/// The same, joined with "and" — for the sentences that read as prose rather
/// than as a slash-separated pair.
export function harnessLabelsProse(list: readonly HarnessInfo[]): string {
  const names = list.map((h) => h.label);
  if (names.length === 0) return '';
  if (names.length === 1) return names[0];
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`;
}

/// V39's attribution line, from the driver harness's declared template
/// (locked decision 27, amendment 0-d).
///
/// `[delegated by OpenCode · tab "api-work" · via cImp]`
///
/// **Client-side only.** Rendered in the banner, echoed into the xterm widget
/// and repeated in the glyph title — it never reaches the PTY.
export function renderAttribution(
  list: readonly HarnessInfo[],
  driverHarness: string | null | undefined,
  driverTabName: string | null | undefined,
): string {
  const who = (driverTabName ?? '').trim();
  const tab = who.length > 0 ? who : 'another tab';
  const template =
    findHarness(list, driverHarness)?.affordances.attributionTemplate ??
    DEFAULT_ATTRIBUTION_TEMPLATE;
  return template
    .replace('{label}', labelForHarness(list, driverHarness))
    .replace('{tab}', tab);
}

/// What an unregistered driver's attribution falls back to. cImp's own wording,
/// not a harness's — the same string every plugin inherits by default, kept
/// here so a delegation from a harness this build does not know still renders a
/// complete line.
const DEFAULT_ATTRIBUTION_TEMPLATE = '[delegated by {label} · tab "{tab}" · via cImp]';

// ── store-reading wrappers ──────────────────────────────────────────────────

/// [`isReservedAiTabId`] against the live list.
export function isReservedAiTab(tabId: string): boolean {
  return isReservedAiTabId(harnessList(), tabId);
}

/// [`findHarnessByTabId`] against the live list.
export function harnessForTab(tabId: string | null | undefined): HarnessInfo | null {
  return findHarnessByTabId(harnessList(), tabId);
}

/// [`findHarnessByCommand`] against the live list.
export function harnessForCommand(command: string | null | undefined): HarnessInfo | null {
  return findHarnessByCommand(harnessList(), command);
}

/// [`labelForHarness`] against the live list.
export function harnessLabel(id: string | null | undefined): string {
  return labelForHarness(harnessList(), id);
}

/// [`labelForTabId`] against the live list.
export function tabLabel(tabId: string): string {
  return labelForTabId(harnessList(), tabId);
}

/// [`renderAttribution`] against the live list.
export function attributionLine(
  driverHarness: string | null | undefined,
  driverTabName: string | null | undefined,
): string {
  return renderAttribution(harnessList(), driverHarness, driverTabName);
}
