import { invoke } from '@tauri-apps/api/core';
import { writable, derived, get, type Readable } from 'svelte/store';
import type { TabId } from './tabs/types';
import {
  TAB_INJECTION_FEATURES,
  type InjectionOverride,
  type Settings,
  type TabInjectionOverrides,
} from './settings/types';
import { applySettings, settings } from './settings/store';
import { fetchDetectionStatus, rulesHealth, type RulesHealth } from './offload';
import {
  normalizeHexColor,
  DEFAULT_LATCHED_COLOR,
  DEFAULT_CONTAMINATED_COLOR,
} from './themes/accent';

/// V32 Phase F (locked decisions 14 + 15): the per-tab taint-latch state that
/// drives the tab-chrome badge and its override popover.
///
/// The same rows are on the loopback's `GET /status`, but every loopback route
/// is bearer-token authenticated — the webview has no token and must not get
/// one — so this goes through the `latch_status` IPC command, which reads the
/// backend's in-process registry directly.

/// One tab's containment state. Mirror of Rust `LatchStatus` + the flattened
/// `LatchView` (`offload/loopback.rs`).
export interface LatchRow {
  /// A registry harness id — the normalized agent vocabulary.
  consumer: string;
  /// The cImp tab id this latch belongs to.
  tab: TabId;
  /// The harness session the latch is scoped to; null while the V28 registry
  /// withholds one.
  session: string | null;
  /// `open` | `external` | `local`.
  latch: string;
  /// Whether external content has entered this conversation at all. Survives
  /// the `flip_local` override — and, since H-2 (2026-08-08), every *unarmed*
  /// session rotation too: the rotation signal is the newest `*.jsonl` in a
  /// directory the model's own Bash can write, so it cannot be the trust root
  /// for un-tainting a context window.
  ///
  /// What DOES clear it is the user, in this app's own UI — the same trust root
  /// every consent surface here uses, and one no shell can fabricate. Three
  /// ways: `clear_contamination`, `await_session_clear`, and (decision 15's
  /// 2026-08-10 amendment) `unlatch`, because restoring FULL access is the
  /// user's verdict while the flip is only a workflow step. See
  /// `offload/loopback.rs`'s `TabLatch::contaminated`.
  contaminated: boolean;
  /// Whether "switch to local" applies right now (EXTERNAL-latched only).
  can_flip_local: boolean;
  /// Whether "restore full access" applies right now (anything but open).
  can_unlatch: boolean;
  /// Step 4: whether either contamination clear applies right now. Published by
  /// the backend rather than read off `contaminated` here, for the reason
  /// `can_unlatch` is: the legality rule for a move belongs to the side that
  /// enforces it, even when it is currently one field wide.
  can_clear: boolean;
  /// Step 4: whether the user restored a checkpoint and cImp is waiting to see
  /// this tab start a new harness session before lifting `contaminated`.
  ///
  /// The UI needs it to explain why a contaminated tab is showing no "clear now"
  /// button after a restore — and to tell the user what will lift it.
  awaiting_session_clear: boolean;
  /// #48 (F-23): whether `latch` reads `local` because the USER flipped it there
  /// with the decision-15 workflow move, rather than because a local-capability
  /// tool ran. `false` for every latch that is not `local`, by construction.
  ///
  /// Carried here because this interface is declared a mirror of Rust
  /// `LatchStatus` + `LatchView`, and a mirror that quietly drops a field is the
  /// defect M-21 was: a type that says it describes the wire, and does not.
  ///
  /// **Nothing in this app renders it yet.** Its live consumer is the generated
  /// plugin's web-direction refusal, which reads the same fact off
  /// `/latch/state` to pick the refusal whose cause it actually checked. A
  /// surface that wants to say *why* a tab is `local` should read this rather
  /// than assume a tool call — `can_flip_local` is about what is OFFERED, not
  /// about what happened.
  local_by_user_flip: boolean;
}

/// The user-initiated containment moves. Mirror of Rust `LatchOverride`, which
/// rejects anything not in this set — so the two ends have to move together.
///
/// - `flip_local` / `unlatch` — the two latch moves (V32 Phase F). `unlatch`
///   also CLEARS the contamination flag (decision 15's 2026-08-10 amendment):
///   restoring full access is a verdict, and the click already hands back the
///   strictly larger risk. `flip_local` keeps the flag — it is a workflow step.
/// - `clear_contamination` — step 4's false-positive resume: the user judged the
///   flagged content harmless, so the flag goes now and nothing else changes.
/// - `await_session_clear` — step 4's restore: the flag stays set (rolling back
///   files cannot un-read a page) and lifts only once cImp observes a new
///   harness session.
export type LatchAction =
  | 'flip_local'
  | 'unlatch'
  | 'clear_contamination'
  | 'await_session_clear';

// ── V32 Phase G — the enable hierarchy, introspected ───────────────────────

/// Which of the three levels decided a feature's value. Mirror of Rust
/// `DecidedBy`. This is the whole reason introspection is part of the feature:
/// with three levels, "why is this tab not latching?" has to be answerable
/// without reading code.
export type DecidedBy = 'global' | 'feature' | 'scope';

/// One feature's resolved state at one scope. Mirror of Rust `FeatureState`.
export interface FeatureState {
  /// Stable key (`taint_latch`, `spotlighting`, …) — also the settings field
  /// stem and the override-row key.
  feature: string;
  label: string;
  /// The value actually in force.
  effective: boolean;
  decided_by: DecidedBy;
  /// This scope's own tri-state cell.
  override_value: 'inherit' | 'on' | 'off';
  /// Whether this scope has a row for the feature at all. `false` rows are
  /// still reported — "this control does not apply here" is sometimes the
  /// answer to "why is this tab not latching?".
  in_scope: boolean;
  /// V32 Phase H: this feature's app-wide DEFAULT. Published by the backend
  /// rather than mirrored here, so "is protection reduced?" has one definition:
  /// a control that ships off (the harness native gate) being off is the
  /// baseline, not a reduction.
  default_on: boolean;
  /// Whether changing this control only takes effect on the next tab spawn.
  /// Published (#48, F-y) so the Settings matrix renders its restart hint from
  /// the backend's `Feature::spawn_baked` instead of a hand-kept TypeScript
  /// copy of it.
  spawn_baked: boolean;
  /// Whether the L1 master switch reaches this control at all. Mirror of Rust
  /// `Feature::master_gated`, published for the same reason as `default_on`:
  /// "is protection reduced here?" must have ONE definition. V38's managed-tool
  /// steering is a token-efficiency nudge rather than a containment control, so
  /// the master switch does not close it and switching it off is not a
  /// reduction — and `default_on` cannot express that, since it ships ON.
  master_gated: boolean;
  /// Why this row is off, when `decided_by` cannot say.
  ///
  /// Absent on every row the backend publishes — those are settings, and the
  /// three `decided_by` values name the level that decided them. Present only
  /// on the frontend-composed row below, whose "off" is a fact about data on
  /// disk rather than a switch anyone flipped.
  reason?: string;
  /// This row's state could not be READ — as distinct from read-and-off (#48,
  /// H-10).
  ///
  /// Absent on every backend row: the backend resolves settings, and a setting
  /// it can publish it can read. Present only on the frontend-composed
  /// signature row, whose subject is an IPC call that can fail.
  ///
  /// `effective` is `false` on such a row because nothing may claim the layer is
  /// working — but "off" and "we cannot tell" are different sentences and every
  /// surface here renders them differently. A consumer that branches on
  /// `effective` alone collapses the third state back into the second, which is
  /// the same family of bug one layer down.
  unknown?: boolean;
  /// This row's subject is PARTLY live — on, matching, and missing part of what
  /// it should be matching with (#48, M-25).
  ///
  /// Absent on every backend row for the same reason `unknown` is: a setting is
  /// on or off. Present only on the frontend-composed signature row, whose
  /// subject is a directory of files that can fail one at a time.
  ///
  /// `effective` is `false` here too — a rule set with 3 of its 4 files failing
  /// is not intact protection and nothing may render it as such — but it is not
  /// the inert layer and nobody switched it off, so it is a fourth sentence and
  /// every surface below phrases it as one.
  partial?: boolean;
}

/// One scope's rows. `scope` is `app`, `offload-worker`, or a tab id.
export interface InjectionScope {
  scope: string;
  label: string;
  features: FeatureState[];
}

/// Mirror of the backend's `injection_status` object (also served on the
/// loopback's `GET /status`).
export interface InjectionStatus {
  /// The L1 master switch.
  protection: boolean;
  /// True when the master is off OR any feature that SHIPS ON resolves off at a
  /// scope that has a row for it. Measured against each feature's default (V32
  /// Phase H), never against `true`.
  ///
  /// **V39: a TAB row switched off by that tab's own cell does not count.** A
  /// newly created AI tab ships every tab-scoped cell off, so counting them
  /// would raise this on every tab of every fresh install. A tab row off because
  /// the app-wide flag is off still counts — see `isReducedRow`, and Rust's
  /// `protection_reduced`, which the two halves are pinned against each other on.
  ///
  /// It is no longer what makes the status chip VISIBLE (the chip is permanent
  /// now and shows the master's value); it is what makes the chip say `reduced`.
  reduced: boolean;
  scopes: InjectionScope[];
}

export async function fetchInjectionStatus(): Promise<InjectionStatus> {
  return invoke<InjectionStatus>('injection_status');
}

/// The last known hierarchy state. `null` until the first poll lands (the app
/// may still be starting) — consumers render nothing rather than guessing.
export const injectionStatus = writable<InjectionStatus | null>(null);

// ── #48, G-3 — the indicator must not fail silent ──────────────────────────

/// How many consecutive failed `injection_status` polls before the surfaces
/// stop claiming protection is intact.
///
/// Three, at the 4 s tick below: ~12 s, long enough that a slow start or a
/// single hiccup never shows the user anything, short enough that a permanently
/// broken command is announced within one glance at the status bar.
export const UNKNOWN_AFTER_FAILURES = 3;

/// The poll's own health. A pure reducer so the transition can be tested — the
/// component that renders it cannot be.
export interface PollHealth {
  /// Consecutive failures since the last success.
  failures: number;
  /// Whether the surfaces should say "unknown" rather than "protected".
  unknown: boolean;
}

export const HEALTHY_POLL: PollHealth = { failures: 0, unknown: false };

/// Fold one tick's outcome into the running health.
///
/// **Why this is not just "swallow it"** (#48, G-3). Both `catch` blocks below
/// were empty, and the doc comment said "an INDIVIDUAL failed poll is swallowed"
/// — which is the right intent and something the code could not express: a
/// permanently failing `injection_status` left the chip hidden and every tab
/// badge absent, so the app rendered as fully protected, indefinitely, with a
/// clean console. This is the surface whose stated purpose (locked decision 16)
/// is that protection "cannot be off and forgotten"; a surface that goes quiet
/// when it breaks forgets it on the user's behalf.
///
/// One success clears it: the state is "we cannot see", not "we saw something
/// bad", so it must not outlive the blindness.
export function recordPoll(prev: PollHealth, ok: boolean): PollHealth {
  if (ok) return HEALTHY_POLL;
  const failures = prev.failures + 1;
  return { failures, unknown: failures >= UNKNOWN_AFTER_FAILURES };
}

/// Whether the reduced-protection surfaces currently know what they are showing.
/// `true` ⇒ the chip renders "protection state unknown" instead of nothing.
export const injectionStatusUnknown = writable(false);

/// Whether protection is reduced anywhere. Drives the status-bar indicator.
export const protectionReduced: Readable<boolean> = derived(
  injectionStatus,
  ($s) => !!$s?.reduced,
);

/// The two scope keys that are not a tab. Mirrors Rust `APP_SCOPE_KEY` /
/// `WORKER_SCOPE_KEY`; every other scope in the report is an AI tab id.
export const APP_SCOPE_KEY = 'app';
export const WORKER_SCOPE_KEY = 'offload-worker';

/// Whether a report scope key names an AI TAB rather than one of the two
/// app-level pseudo-scopes. The backend keys tab scopes by the tab id itself
/// (`loopback::injection_status`), so this is the whole test.
export function isTabScope(scope: string): boolean {
  return scope !== APP_SCOPE_KEY && scope !== WORKER_SCOPE_KEY;
}

/// Whether one report row counts as REDUCED protection **at `scope`**.
///
/// **The one definition, exported because there were three** (#48, G-2). The
/// backend's `protection_reduced` owns the rule; this is its frontend reading,
/// and every surface that asks "what is reduced here?" has to call it rather
/// than restate it — the status chip restated it without the `default_on` clause
/// and disagreed with the tab badge beside it, in the same viewport.
///
/// **V39 adds the scope, and it is not decoration.** A newly created AI tab
/// ships every tab-scoped cell `Off` and the user arms them from the tab's
/// shield badge, so a tab row switched off by the tab's OWN cell is the
/// baseline, not a reduction — counting it would raise the chip on every tab of
/// every fresh install. The filter is on *who decided*, not on which scope
/// asked: a tab row that is off because L2 is off still counts, because three
/// ships-on controls (memory quarantine, native-web visibility, consumer
/// hygiene) have a tab row and no other row, so nothing else would ever see
/// them. Rust's `protection_reduced` narrows its tab pass by exactly this
/// predicate.
///
/// Five filters, all structural rather than cosmetic:
/// - at a TAB, not a row the tab's own cell switched off (the V39 baseline);
/// - only rows the scope actually HAS — a tab is not "reduced" because the
///   worker-only canary does not apply to it;
/// - only rows that are actually off;
/// - only features that ship ON (`default_on`). V32 Phase H's harness native
///   gate defaults off by user decision, and counting it would raise the muted
///   badge on every tab of a fresh install — which is how a badge stops being
///   read. Uses the backend's own `default_on` rather than a second list of
///   defaults in TypeScript.
/// - only features the master switch is ABOUT (`master_gated`). V38's
///   managed-tool steering lives in the same hierarchy for its switches, but it
///   is a token-efficiency nudge: switching it off costs tokens, not
///   protection, and a security indicator that lights for it stops being read.
///   The backend's `protection_reduced` skips exactly these rows — this is the
///   cross-module invariant the two halves are pinned on, in
///   `injection.rs`'s `tool_steering_never_counts_as_reduced_protection` and in
///   `latch.test.ts` beside it.
///
/// The synthetic signature-health row passes: it is `in_scope`, off, and ships
/// on. It is a reduction — it just is not a *switch*, which is what `reason`
/// says and what [`reducedSummary`] counts separately.
export function isReducedRow(f: FeatureState, scope: string): boolean {
  if (isTabScope(scope) && !f.effective && f.decided_by === 'scope') return false;
  return f.in_scope && !f.effective && f.default_on && f.master_gated;
}

/// The features resolved OFF for one tab **for reasons other than that tab's
/// own cell** — the tab badge's tooltip and the status chip's per-tab share.
///
/// Since V39 this is usually empty even on a tab with every control off, which
/// is the point: that posture is the tab's baseline. What it still reports is a
/// tab losing a control it was inheriting — an app-wide flip, the master, or the
/// synthetic signature-health row, which is a fact about the rules directory and
/// not a switch anybody flipped.
export function reducedFeaturesFor(
  status: InjectionStatus | null,
  tab: TabId,
): FeatureState[] {
  const scope = status?.scopes.find((s) => s.scope === tab);
  return (scope?.features ?? []).filter((f) => isReducedRow(f, tab));
}

// ── V39 — the tab shield badge as a standing control ───────────────────────

/// A tab's tab-scoped rows that are **switches**, in the order the backend
/// publishes them (`Feature::ALL`).
///
/// The single source for the badge tint, the badge tooltip and the popover's
/// toggle list, so none of them can describe the tab differently — and it is the
/// backend's RESOLVED report, never a re-resolution of the hierarchy here.
///
/// Two filters. `in_scope` is the backend's own answer to "does this control
/// have a cell here?". The second one drops the synthetic signature-health row
/// [`withSignatureHealth`] adds: it is `in_scope` and it is a real
/// reduced-protection fact, but it is a fact about a directory on disk with no
/// cell behind it — rendering it as a toggle would offer the user a switch that
/// cannot be written, and counting it in "7 of 9 on" would mix a measurement
/// into a count of settings. It keeps reaching the badge tooltip through
/// [`reducedFeaturesFor`], which is where facts-that-are-not-switches belong.
export function tabProtectionRows(
  status: InjectionStatus | null,
  tab: TabId,
): FeatureState[] {
  const scope = status?.scopes.find((s) => s.scope === tab);
  const cells = new Set<string>(TAB_INJECTION_FEATURES);
  return (scope?.features ?? []).filter((f) => f.in_scope && cells.has(f.feature));
}

/// How much of a tab's protection is engaged. Drives the badge's colour.
///
/// - `protected` — every tab-scoped control resolves on;
/// - `partial` — some do;
/// - `off` — none do (the shape a newly created tab ships in);
/// - `unknown` — there is no report for this tab yet, so nothing may be claimed.
///   Distinct from `off` for the reason every other epistemic state here is:
///   "we have not looked" must never render as "we looked and it is off".
export type ProtectionTint = 'protected' | 'partial' | 'off' | 'unknown';

export function protectionTint(rows: FeatureState[]): ProtectionTint {
  if (rows.length === 0) return 'unknown';
  const on = rows.filter((f) => f.effective).length;
  if (on === rows.length) return 'protected';
  return on === 0 ? 'off' : 'partial';
}

/// The one-line summary the badge tooltip opens with: `Protection: 7 of 9 on`.
export function protectionSummary(rows: FeatureState[]): string {
  if (rows.length === 0) return 'Protection: not known yet';
  return `Protection: ${rows.filter((f) => f.effective).length} of ${rows.length} on`;
}

/// One row's effective state as the popover words it.
export function effectiveWord(f: FeatureState): 'on' | 'off' {
  return f.effective ? 'on' : 'off';
}

// ── V39 — writing L3 cells from the main window ────────────────────────────

/// Clone `current` with `changes` applied to one AI tab's L3 injection row.
///
/// **Clone-and-patch, never mutate.** The store holds the object the rest of the
/// window is rendering from; editing it in place would move every subscriber's
/// view before the backend had accepted anything, and `applySettings`'s rollback
/// would then have nothing to roll back to.
///
/// A pure function so the "Enable all" / "Disable all" property — ONE settings
/// object carrying every cell, not N writes — is testable without a backend.
export function withTabInjectionOverrides(
  current: Settings,
  tab: TabId,
  changes: Partial<TabInjectionOverrides>,
): Settings {
  return {
    ...current,
    tabs: current.tabs.map((t) =>
      t.kind === 'ai_tool' && t.id === tab
        ? { ...t, injection_overrides: { ...t.injection_overrides, ...changes } }
        : t,
    ),
  };
}

/// The patch that sets every row in `rows` to `value`.
///
/// Keyed off the BACKEND's rows rather than a TypeScript list of features, so
/// "all" means the controls this tab actually has — a control added in Rust is
/// covered the day it is declared, and one the scope does not carry is never
/// written into a cell that does not exist.
export function setAllOverrides(
  rows: FeatureState[],
  value: InjectionOverride,
): Partial<TabInjectionOverrides> {
  const out: Record<string, InjectionOverride> = {};
  for (const f of rows) out[f.feature] = value;
  return out as Partial<TabInjectionOverrides>;
}

/// Write one tab's L3 cells through the ordinary full-object save path.
///
/// There is deliberately no `set_injection_override` IPC (see
/// `ipc/commands.rs`): an L3 cell is an ordinary settings field, and a
/// side-channel command would give the app a second write path that can race the
/// full-object save. One call ⇒ one `applySettings`, however many cells moved.
export async function applyTabInjectionOverrides(
  tab: TabId,
  changes: Partial<TabInjectionOverrides>,
): Promise<void> {
  await applySettings(withTabInjectionOverrides(get(settings), tab, changes));
}

/// Flip the L1 master switch, as one full-object write.
///
/// Pure patch + thin caller for the same reason as above: the status-bar chip is
/// a control now, not a link, and what it writes has to be testable.
export function withMasterProtection(current: Settings, on: boolean): Settings {
  return {
    ...current,
    offload: {
      ...current.offload,
      injection: { ...current.offload.injection, protection: on },
    },
  };
}

export async function applyMasterProtection(on: boolean): Promise<void> {
  await applySettings(withMasterProtection(get(settings), on));
}

/// What the status chip says it found, as the chip's tooltip phrases it.
///
/// Lives here rather than in `InjectionBadge.svelte` for two reasons: it is the
/// same rule as the tab badge's and must not drift from it again (#48, G-2), and
/// `.svelte` files have no test harness in this repo while this file does.
///
/// Two counts, not one:
/// - **switched off** — controls someone turned off. Counted over DISTINCT
///   features rather than over (scope, feature) pairs: one app-wide flip lands
///   on the worker row and every tab row, and "4 controls switched off" for one
///   unticked checkbox is the same magnitude confusion the finding is about. The
///   click leads to Settings, which names the scopes exactly.
/// - **inert** — rows that carry their own `reason` (today: the signature layer
///   switched on with no rules to match with). Nobody flipped these, so folding
///   them into the first number made the chip's sentence untrue — spec residual
///   (g).
export interface ReducedCounts {
  /// Controls someone turned off.
  switched: number;
  /// Rows that are on and doing nothing (they carry their own `reason`).
  inert: number;
  /// Rows whose state could not be read at all (#48, H-10). Never folded into
  /// either of the other two: "off" and "we cannot tell" are different claims,
  /// and a surface that says the first when it means the second is the defect
  /// this counter exists to make impossible.
  unreadable: number;
  /// Rows that are on and working with only part of what they need (#48,
  /// M-25). Its own bucket for the reason `unreadable` is one: counting a rule
  /// set that lost 3 of 4 files as `inert` would tell the user the layer is
  /// doing nothing when it is doing some of it, and counting it as `switched`
  /// would send them to a control nobody touched.
  partial: number;
}

/// [`reducedSummary`]'s counts, for surfaces that must BRANCH on the shape of
/// the reduction rather than phrase it — the status chip picks its word from
/// these. Exported so no component re-derives them (#48, G-2).
export function reducedCounts(status: InjectionStatus | null): ReducedCounts {
  const switched = new Set<string>();
  const inert = new Set<string>();
  const unreadable = new Set<string>();
  const partial = new Set<string>();
  for (const scope of status?.scopes ?? []) {
    for (const f of scope.features) {
      if (!isReducedRow(f, scope.scope)) continue;
      (f.unknown ? unreadable : f.partial ? partial : f.reason ? inert : switched).add(f.feature);
    }
  }
  return {
    switched: switched.size,
    inert: inert.size,
    unreadable: unreadable.size,
    partial: partial.size,
  };
}

export function reducedSummary(status: InjectionStatus | null): string {
  const { switched, inert, unreadable, partial } = reducedCounts(status);
  const parts: string[] = [];
  if (switched > 0) {
    parts.push(`${switched} control${switched === 1 ? '' : 's'} switched off`);
  }
  if (inert > 0) {
    parts.push(`${inert} layer${inert === 1 ? '' : 's'} switched on but inert`);
  }
  if (partial > 0) {
    parts.push(`${partial} layer${partial === 1 ? '' : 's'} only partly loaded`);
  }
  if (unreadable > 0) {
    parts.push(`${unreadable} layer${unreadable === 1 ? '' : 's'} whose state could not be read`);
  }
  // The backend can report `reduced` for a reason no row expresses (it is the
  // source of truth and this side must not argue with it), so the chip still
  // has something to say when nothing here matches.
  return parts.join(', ') || 'something is off';
}

/// The word the status chip wears: the L1 master switch's own value.
///
/// **V39 made the chip a CONTROL rather than a warning light.** It used to be
/// silent while everything was on and to name what was wrong when it was not
/// (`reduced` / `off` / `unknown` / `unverified`). That satisfied locked
/// decision 16 only as long as the user knew a missing chip meant "protected" —
/// which is the same "absence reads as fine" assumption #48's G-3 found to be
/// false in practice. It is permanent and colour-coded now, it says which way
/// the master is set, and clicking it flips the master. The four states it used
/// to wear as WORDS are still all rendered — they moved to [`note`], as
/// modifiers on top of the on/off it now shows.
export type InjectionChipLabel = 'on' | 'off';

/// What the chip knows BESIDES the master's value.
///
/// - `null` — everything readable is on and readable.
/// - `reduced` — at least one control that ships on really is off somewhere.
/// - `unverified` — everything we CAN read is on and the one thing we cannot is
///   the signature layer's armed-ness (#48, H-10). Not `reduced`: nobody turned
///   anything off.
/// - `unknown` — the hierarchy poll itself has been failing, so we cannot see
///   any of it (#48, G-3).
///
/// Kept separate from [`InjectionChipLabel`] deliberately: the master's value is
/// a setting the app always knows (it is in the settings store), while these
/// three are claims about what cImp could observe. Collapsing them back into one
/// word is what made "off" and "we cannot tell" indistinguishable before.
export type InjectionChipNote = null | 'reduced' | 'unverified' | 'unknown';

export interface InjectionChipState {
  /// Kept for callers, and now always `true`: the chip is a standing control.
  /// Silence would mean "there is no such switch", which is the one thing this
  /// surface must never say.
  visible: boolean;
  label: InjectionChipLabel;
  /// The master switch's value — what a click will invert.
  on: boolean;
  note: InjectionChipNote;
  /// Whether the chip wears the dashed "this is not a confident claim"
  /// treatment. True for both epistemic states.
  degraded: boolean;
  title: string;
}

/// The whole status chip, as a value.
///
/// Lives here rather than in `InjectionBadge.svelte` for the reason given on
/// [`reducedSummary`]: `.svelte` files have no test harness in this repo, and
/// the chip's job is to not lie.
///
/// F-18: every tooltip below names its destination, because the badge
/// deep-links to the "Injection protection" section rather than opening Settings
/// on whatever section it happens to land on. Since V39 the PRIMARY click flips
/// the master instead, so each tooltip states both gestures — a control whose
/// click does something other than what its tooltip says is worse than one that
/// only links.
export function injectionChipState(
  status: InjectionStatus | null,
  pollUnknown: boolean,
): InjectionChipState {
  const counts = reducedCounts(status);
  const summary = reducedSummary(status);
  // The master's value comes from the report, which is the same resolver every
  // other surface reads. Before the first poll lands there is nothing to claim,
  // and `pollUnknown` below is what says so; `true` is the shipping value and
  // the only honest placeholder for one tick.
  const on = status?.protection ?? true;
  const settingsHint = 'Right-click to open Settings → Injection protection.';
  const flip = on
    ? 'Click to turn it OFF (every containment control goes inert — the taint latch, the spotlighting envelope, the SSRF guard, memory quarantine; managed-tool steering is not a containment control and keeps running).'
    : 'Click to turn it back ON.';
  // A running AI tab keeps the posture it launched with: the master is
  // spawn-baked, so the backend's `ai-tab-restart-hint` fires on the save and
  // the main window toasts it. Said here too, because the toast is gone in eight
  // seconds and this tooltip is not.
  const restart = 'Tabs already running keep their launch posture until restarted.';
  const base = on
    ? `Injection protection is ON. ${flip} ${restart} ${settingsHint}`
    : `Injection protection is OFF — every V32 containment control is disabled, for every tab and the offload worker. ${flip} ${restart} ${settingsHint}`;
  if (pollUnknown) {
    return {
      visible: true,
      label: on ? 'on' : 'off',
      on,
      note: 'unknown',
      degraded: true,
      title: `${base} cImp has not been able to READ the protection state for several polls, so what is switched on beneath the master cannot be shown. Check the console.`,
    };
  }
  const unreadable = counts.unreadable > 0;
  // A partly-loaded layer is a thing we DID read and that IS reduced, so it
  // keeps the chip on the confident word (#48, M-25) — `unverified` is reserved
  // for "nobody could tell", and reaching it with a known partial set would
  // understate a real loss of coverage.
  const onlyUnreadable =
    unreadable && counts.switched === 0 && counts.inert === 0 && counts.partial === 0;
  // With the master off the report already says everything is off; a `reduced`
  // note beside an `off` label would be the same fact twice, in a weaker word.
  const note: InjectionChipNote = !on
    ? null
    : onlyUnreadable
      ? 'unverified'
      : status?.reduced
        ? 'reduced'
        : null;
  return {
    visible: true,
    label: on ? 'on' : 'off',
    on,
    note,
    degraded: on && unreadable,
    title:
      note === 'unverified'
        ? `${base} Part of it cannot be verified — ${summary}. It is not switched off; cImp cannot currently tell whether it is working.`
        : note === 'reduced'
          ? unreadable
            ? `${base} Beneath it, protection is reduced and part of it cannot be verified — ${summary}.`
            : `${base} Beneath it, protection is reduced — ${summary}.`
          : base,
  };
}

/// How one reduced row reads in a list: `off` for a switch, `unknown` for a
/// state nobody could read, `partial` for a layer running on part of what it
/// needs (#48, M-25). The popover and the tab tooltip both call it, so they
/// cannot describe the same row differently (#48, H-10 / G-2).
export function featureStateWord(f: FeatureState): 'off' | 'unknown' | 'partial' {
  return f.unknown ? 'unknown' : f.partial ? 'partial' : 'off';
}

/// The tab badge's tooltip sentence for its reduced rows.
///
/// One sentence per KIND of claim, because the old single sentence ended in the
/// word "off" and would have said it of a row whose state we failed to read
/// (#48, H-10) — and, later, of one that is running on a partial rule set (#48,
/// M-25). Neither is off, and a tooltip that says so points the user at a
/// switch instead of at the files.
export function reducedTabLine(reduced: FeatureState[]): string {
  const off = reduced.filter((f) => !f.unknown && !f.partial).map((f) => f.label);
  const partial = reduced.filter((f) => !f.unknown && f.partial).map((f) => f.label);
  const unreadable = reduced.filter((f) => f.unknown).map((f) => f.label);
  const parts: string[] = [];
  if (off.length > 0) {
    parts.push(`Injection protection reduced for this tab: ${off.join(', ')} off.`);
  }
  if (partial.length > 0) {
    parts.push(`Running on only part of what it needs: ${partial.join(', ')}.`);
  }
  if (unreadable.length > 0) {
    parts.push(`cImp could not read the state of: ${unreadable.join(', ')}.`);
  }
  return parts.join(' ');
}

// ── V32 Phase C / #48 D-2 — a disarmed signature layer is reduced protection ─

/// The synthetic row's feature key.
///
/// Deliberately not a `Feature::key` from the backend: there is no switch here
/// to resolve. It is composed in the frontend because it is the one
/// reduced-protection fact that is not a setting, and every surface that
/// already asks "what is reduced here?" should get it without a second
/// question.
export const SIGNATURE_RULES_FEATURE = 'signature_rules_live';

/// What one poll knows about the signature layer — THREE outcomes, spelled out
/// because two of them used to share a `null` (#48, H-10).
///
/// - [`RulesHealth`] — we read it. `healthy` is the only field that renders
///   protected (#48, M-25); `armed` answers the weaker question "can it match
///   anything at all?" and this union used to carry nothing else, so a rule
///   directory with 3 of 4 files failing rendered as full protection.
/// - `'unknown'` — we could not read it, for long enough to say so
///   ([`UNKNOWN_AFTER_FAILURES`] consecutive failures). Renders as its own state,
///   never as armed and never as "switched off".
/// - `'pending'` — no reading yet, and no reason to alarm anyone: the first
///   ticks after launch, or a transient failure still inside the grace window.
///   Publishes the backend's hierarchy unchanged.
///
/// The union is deliberately closed and has no `null` member: the old signature
/// took `{ armed } | null`, and `null` was produced BOTH by a swallowed IPC
/// failure and by "nothing to add", so a broken `detection_status` rendered the
/// signature layer as fully armed indefinitely. Anything that wants the
/// pass-through has to type the word `'pending'` and mean it.
///
/// The read arm carries the backend's whole verdict rather than one boolean
/// this side picked, for the same reason: a member of the union that is not on
/// it cannot be branched on by mistake, and `rules.armed` was the wrong member.
export type SignatureHealth = RulesHealth | 'unknown' | 'pending';

/// One tick's detection read, folded into the running health.
///
/// **Why the detection read gets its own accounting** (#48, H-10). It used to
/// have none: `detectionStatus()` swallowed its failure to `null`, `null` took
/// the same branch as `armed: true`, and the enclosing `try` still called
/// `recordPoll(health, true)` — so "armed", "not armed" and "the IPC is broken"
/// all rendered as fully protected, with a healthy tick and no user-visible
/// trace. This is D-2 one layer up: D-2 was *a failed reload silently disarms
/// the layer*; H-10 was *a failed status read silently renders it armed*.
///
/// Reuses [`recordPoll`] rather than inventing a second debounce — same
/// reducer, same [`UNKNOWN_AFTER_FAILURES`], same 4 s tick. Its own COUNTER
/// though, and not [`injectionStatusUnknown`]: that flag means "the hierarchy
/// itself is unreadable", and raising it for a failed detection read would have
/// the chip claim total blindness while it can in fact see every switch. The
/// detection blindness travels as a ROW in the hierarchy instead, which is where
/// per-scope facts already live and which every surface already renders.
///
/// `last` keeps the most recent successful reading across the grace window, so
/// two hiccups in a row do not blank a state we know; the Nth consecutive
/// failure discards it and says `'unknown'`.
export interface SignatureRead {
  health: PollHealth;
  /// The last reading that actually landed, or `null` before the first one.
  last: RulesHealth | null;
  /// What [`withSignatureHealth`] should be told this tick.
  rules: SignatureHealth;
}

/// Nothing read yet — the state at the moment polling starts.
export const SIGNATURE_UNREAD: SignatureRead = {
  health: HEALTHY_POLL,
  last: null,
  rules: 'pending',
};

/// Fold one detection read into [`SignatureRead`]. `read === null` means the
/// IPC call FAILED — the one thing the old code could not say.
export function recordSignatureRead(
  prev: SignatureRead,
  read: RulesHealth | null,
): SignatureRead {
  if (read) return { health: recordPoll(prev.health, true), last: read, rules: read };
  const health = recordPoll(prev.health, false);
  return {
    health,
    last: prev.last,
    // Inside the grace window: keep showing the last thing we actually read, or
    // stay quiet if we never read one. Past it: say we cannot see.
    rules: health.unknown ? 'unknown' : (prev.last ?? 'pending'),
  };
}

/// Fold what we know about the signature layer into the injection hierarchy as
/// an extra row: "on and with no rules to match with", or "on and we could not
/// tell".
///
/// **Why this exists (#48, D-2).** The reduced-protection surfaces are derived
/// entirely from settings toggles, so a signature layer that had been disarmed
/// — a rules directory that compiles to nothing, after which `scan` returns
/// empty and every page reports clean — rendered *full protection* as long as
/// the toggle was on. Decision 16 gives these surfaces one job ("protection
/// cannot be off and forgotten"), and a layer that is switched on and doing
/// nothing is the clearest case of exactly that.
///
/// **And why it takes three states (#48, H-10).** A layer whose status could not
/// be READ is not the same fact as one that reported itself disarmed, and
/// neither is "fully protected". The row carries `unknown` so the three stay
/// three all the way to the pixels; `reduced` is raised for both, because that
/// flag is what makes the surfaces speak at all and silence is the one rendering
/// an unreadable state must never get.
///
/// **And why it now takes four (#48, M-25).** The pass-through was gated on
/// `armed` — "can this rule set match anything at all?" — where the question
/// being asked is "is the rule set on disk live?", which is `healthy`. A
/// directory of 4 rule files with 3 failing to compile is `armed` and not
/// `healthy`: it matches with a quarter of the signatures the user believes are
/// running, and every surface here rendered it as full protection. That is the
/// H-10 family of bug in its worst direction — not a missing indicator but a
/// confident, wrong one — so `healthy` is the gate, and the partial case gets
/// its own `partial` flag rather than borrowing the disarmed row's sentence
/// ("compiled to no usable rules", which would be false of it).
///
/// Added **only to scopes where the detection feature both applies and
/// resolves on**: a scope that never screens, or one the user switched off, is
/// not reduced by a rules directory it does not read — and is not made
/// *uncertain* by a status read either, which is why the same gate covers the
/// unknown case. "Off by configuration" stays the feature's own row.
export function withSignatureHealth(
  status: InjectionStatus | null,
  rules: SignatureHealth,
): InjectionStatus | null {
  if (!status || rules === 'pending') return status;
  const unreadable = rules === 'unknown';
  // `healthy`, never `armed` and never a local restatement of it: the backend
  // owns this predicate (#48, N-3 / M-25) and `offload.ts::rulesHealth` is the
  // only place it is read.
  if (!unreadable && rules.healthy) return status;
  // Read, matching, and incomplete — the fourth state. `armed` earns its keep
  // only here, once `healthy` has already said the set is not whole.
  const partial = !unreadable && rules.armed;
  let reason: string;
  if (unreadable) {
    reason = 'cImp could not read the detection layer’s status';
  } else if (!partial) {
    reason = 'the rules directory compiled to no usable rules';
  } else if (rules.files_failed > 0) {
    const n = rules.files_failed;
    reason = `${n} rule file${n === 1 ? '' : 's'} failed to compile — those signatures are not matching`;
  } else {
    // `healthy` is the backend's to define and this side must not argue with
    // it: if it ever says "armed but not whole" for a reason the file counts do
    // not show, say the true general thing rather than invent a number.
    reason = 'part of the rule set on disk is not live';
  }
  const row: FeatureState = {
    feature: SIGNATURE_RULES_FEATURE,
    label: 'Signature rules loaded',
    // Nothing may claim the layer is working. When the state is UNREADABLE this
    // is "we cannot say it is on", not "it is off" — `unknown` below carries
    // that distinction, and every surface renders the two differently. When it
    // is PARTIAL it is "not all of it is working": still not something any
    // surface may render as protection, which is why this stays `false` (it is
    // also what keeps the row inside `isReducedRow` and therefore visible at
    // all), with `partial` carrying the difference.
    effective: false,
    decided_by: 'feature',
    override_value: 'inherit',
    in_scope: true,
    // It ships loaded, so "not loaded" is a reduction and not a baseline.
    default_on: true,
    // Nothing to bake into a spawn: this row is a fact about the rules
    // directory, not a switch. It never reaches the Settings matrix.
    spawn_baked: false,
    // It IS a protection fact — the signature layer not matching is less
    // containment — so it stays inside `isReducedRow`'s master-gated filter.
    master_gated: true,
    unknown: unreadable,
    partial,
    reason,
  };
  let anywhere = false;
  const scopes = status.scopes.map((s) => {
    const detection = s.features.find((f) => f.feature === 'detection');
    if (!detection?.in_scope || !detection.effective) return s;
    anywhere = true;
    return { ...s, features: [...s.features, row] };
  });
  return { ...status, reduced: status.reduced || anywhere, scopes };
}

export async function fetchLatchStatus(): Promise<LatchRow[]> {
  return invoke<LatchRow[]>('latch_status');
}

/// Apply an override and return the tab's new state. Rejects with the
/// backend's message (unknown action, nothing latched, illegal transition) —
/// the popover shows it verbatim rather than failing silently.
export async function applyLatchOverride(
  tab: TabId,
  consumer: string,
  action: LatchAction,
): Promise<void> {
  await invoke('latch_override', { tab, consumer, action });
}

/// Every latch row the backend knows about, keyed by tab id. A tab absent from
/// the map has never had a gated call — no badge.
export const latchByTab = writable<Partial<Record<TabId, LatchRow>>>({});

/// Whether a tab should show the taint badge at all: it is latched in either
/// direction, or its conversation is contaminated. An `open` + uncontaminated
/// row (a tab that only ever used TRUSTED tools) shows nothing — the badge has
/// to mean something when it appears.
export function isTainted(row: LatchRow | undefined): boolean {
  return !!row && (row.latch !== 'open' || row.contaminated);
}

/// The color the containment surfaces wear for one tab — the taint badge in
/// the tab strip AND the frame Pane.svelte draws around the tab's content —
/// or `null` when the tab is not tainted (the frame doesn't render; the badge,
/// if visible for a reduced-protection state, keeps its own muted CSS).
///
/// One function so the two surfaces cannot disagree about either the
/// predicate or the color. Contamination wins over the latch state for the
/// reason Tab.svelte's `.taint-contaminated` gives: contamination outlives
/// the latch, so it gets the stronger color. `latched` / `contaminated` are
/// the raw `ui.latched_color` / `ui.contaminated_color` settings values —
/// validated here (never trust a hand-edited settings.json), falling back to
/// the historical badge colors.
export function taintColor(
  row: LatchRow | null | undefined,
  latched: string,
  contaminated: string,
): string | null {
  if (!row || !isTainted(row)) return null;
  return row.contaminated
    ? normalizeHexColor(contaminated, DEFAULT_CONTAMINATED_COLOR)
    : normalizeHexColor(latched, DEFAULT_LATCHED_COLOR);
}

/// Tab ids currently showing a badge. Exposed mostly for tests and debugging;
/// the tab bar reads `latchByTab` directly so it can render the row's detail.
export const taintedTabs: Readable<TabId[]> = derived(latchByTab, ($m) =>
  Object.entries($m)
    .filter(([, row]) => isTainted(row as LatchRow))
    .map(([tab]) => tab as TabId),
);

/// How often the badge state is refreshed.
///
/// Polled rather than evented: the latch moves inside the loopback's request
/// handlers, which hold no `AppHandle` to emit from, and the read is a mutex
/// plus a handful of map entries — no I/O, no allocation worth naming. 4s is
/// well under the time it takes a user to notice a fetch happened and far
/// above any cost worth optimizing.
const POLL_MS = 4000;

/// Start polling `latch_status`. Returns a stop function; safe to call the
/// stop more than once.
///
/// An individual failed poll keeps the store's last known value rather than
/// flickering every badge off — the app may still be starting. But it is no
/// longer *silent*: both arms warn, matching `SettingsApp.svelte`'s handling of
/// the same call, and [`UNKNOWN_AFTER_FAILURES`] consecutive failures of the
/// hierarchy poll flip [`injectionStatusUnknown`], which the status chip renders
/// as "protection state unknown". See [`recordPoll`] for why.
///
/// Three reads, three failure treatments, none of them silent (#48, H-10):
/// - `injection_status` — last value kept; [`injectionStatusUnknown`] after N.
/// - `detection_status` — last value kept; an `unknown` ROW in the hierarchy
///   after N ([`recordSignatureRead`]). Not the app-wide flag: we can still see
///   every switch, just not whether the signature layer is armed.
/// - `latch_status` — last value kept, warned, no sentinel (an absent latch row
///   already means "no gated call yet" by design).
export function startLatchPolling(): () => void {
  let stopped = false;
  let health = HEALTHY_POLL;
  let signature = SIGNATURE_UNREAD;
  const tick = async (): Promise<void> => {
    // V32 Phase G rides the same tick. Settings changes are rarer than latch
    // moves, but the two are read together everywhere they are shown (the tab
    // badge means one thing when the latch is engaged and another when the
    // latch feature is switched off), so fetching them apart would let the
    // badge and its explanation disagree for up to a poll interval.
    try {
      const status = await fetchInjectionStatus();
      // #48/D-2: the signature layer's actual armed-ness rides the same tick as
      // the toggles, for the same reason the latch does — a badge and its
      // explanation disagreeing for a poll interval is how a badge stops being
      // read.
      //
      // #48/H-10: read through the NON-swallowing `fetchDetectionStatus`, and
      // hand the failure to [`recordSignatureRead`] rather than to nobody. The
      // swallowing `detectionStatus()` returned a `null` that the hierarchy
      // could not distinguish from "armed", so a permanently broken
      // `detection_status` rendered the signature layer as fully protected for
      // as long as the app ran. Caught HERE rather than left to the outer
      // `catch`, because the hierarchy the backend just handed us is still good
      // and still wants publishing — with the uncertainty attached to it.
      //
      // #48/M-25: through `rulesHealth`, which is the only reader of the rule
      // fields. This line used to lift `rules.armed` out of the status by hand
      // — the weaker predicate, three lines under the comment saying `healthy`
      // is the one to read — so a partly-broken rules directory reached every
      // surface as "armed" and every surface believed it.
      let read: RulesHealth | null = null;
      try {
        read = rulesHealth(await fetchDetectionStatus());
      } catch (e) {
        console.warn(
          `detection_status poll failed (${signature.health.failures + 1} in a row)`,
          e,
        );
      }
      signature = recordSignatureRead(signature, read);
      if (!stopped) {
        injectionStatus.set(withSignatureHealth(status, signature.rules));
        health = recordPoll(health, true);
        injectionStatusUnknown.set(health.unknown);
      }
    } catch (e) {
      // Keep the last known hierarchy state (the app may still be starting) —
      // but say so, and stop claiming to know once it has failed for a while.
      if (!stopped) {
        health = recordPoll(health, false);
        injectionStatusUnknown.set(health.unknown);
        console.warn(`injection_status poll failed (${health.failures} in a row)`, e);
      }
    }
    try {
      const rows = await fetchLatchStatus();
      if (stopped) return;
      const next: Partial<Record<TabId, LatchRow>> = {};
      for (const row of rows) {
        // One tab id belongs to exactly one agent in practice. If two rows
        // ever collide, the latched/contaminated one wins: under-reporting
        // taint is the failure this whole surface exists to prevent.
        const prev = next[row.tab];
        if (!prev || (!isTainted(prev) && isTainted(row))) next[row.tab] = row;
      }
      latchByTab.set(next);
    } catch (e) {
      // Keep the last state (app still starting, or the command is
      // unavailable). No sentinel of its own: an absent latch row means "this
      // tab has had no gated call", which is indistinguishable from a stale one
      // by design — the hierarchy poll above is the surface that must never go
      // quiet. Still warned, because the asymmetry with `SettingsApp` was
      // unintentional and a silent badge is how G-3 stayed invisible.
      if (!stopped) console.warn('latch_status poll failed', e);
    }
  };
  void tick();
  const handle = setInterval(() => void tick(), POLL_MS);
  return () => {
    stopped = true;
    clearInterval(handle);
  };
}
