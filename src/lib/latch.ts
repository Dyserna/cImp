import { invoke } from '@tauri-apps/api/core';
import { writable, derived, type Readable } from 'svelte/store';
import type { TabId } from './tabs/types';
import { detectionStatus } from './offload';

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
  /// `claude` / `opencode` — the normalized agent vocabulary.
  consumer: string;
  /// The cImp tab id this latch belongs to.
  tab: TabId;
  /// The harness session the latch is scoped to; null while the V28 registry
  /// withholds one.
  session: string | null;
  /// `open` | `external` | `local`.
  latch: string;
  /// Whether external content has entered this conversation at all. Survives
  /// every override — and, since H-2 (2026-08-08), every session rotation too:
  /// the rotation signal is the newest `*.jsonl` in a directory the model's own
  /// Bash can write, so it cannot be the trust root for un-tainting a context
  /// window. Nothing in a running cImp clears this bit; see
  /// `offload/loopback.rs`'s `TabLatch::contaminated`.
  contaminated: boolean;
  /// Whether "switch to local" applies right now (EXTERNAL-latched only).
  can_flip_local: boolean;
  /// Whether "restore full access" applies right now (anything but open).
  can_unlatch: boolean;
}

/// The two user-initiated moves. Mirror of Rust `LatchOverride`.
export type LatchAction = 'flip_local' | 'unlatch';

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
  /// a control that ships off (the OpenCode native gate) being off is the
  /// baseline, not a reduction.
  default_on: boolean;
  /// Whether changing this control only takes effect on the next tab spawn.
  /// Published (#48, F-y) so the Settings matrix renders its restart hint from
  /// the backend's `Feature::spawn_baked` instead of a hand-kept TypeScript
  /// copy of it.
  spawn_baked: boolean;
  /// Why this row is off, when `decided_by` cannot say.
  ///
  /// Absent on every row the backend publishes — those are settings, and the
  /// three `decided_by` values name the level that decided them. Present only
  /// on the frontend-composed row below, whose "off" is a fact about data on
  /// disk rather than a switch anyone flipped.
  reason?: string;
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
  /// True when the master is off OR any feature that SHIPS ON resolves off at
  /// any scope — the predicate behind the out-of-Settings indicator, so
  /// protection cannot be off and forgotten. Measured against each feature's
  /// default (V32 Phase H), never against `true`.
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

/// Whether one report row counts as REDUCED protection.
///
/// **The one definition, exported because there were three** (#48, G-2). The
/// backend's `protection_reduced` owns the rule; this is its frontend reading,
/// and every surface that asks "what is reduced here?" has to call it rather
/// than restate it — the status chip restated it without the `default_on` clause
/// and disagreed with the tab badge beside it, in the same viewport.
///
/// Three filters, all structural rather than cosmetic:
/// - only rows the scope actually HAS — a tab is not "reduced" because the
///   worker-only canary does not apply to it;
/// - only rows that are actually off;
/// - only features that ship ON (`default_on`). V32 Phase H's OpenCode native
///   gate defaults off by user decision, and counting it would raise the muted
///   badge on every tab of a fresh install — which is how a badge stops being
///   read. Uses the backend's own `default_on` rather than a second list of
///   defaults in TypeScript.
///
/// The synthetic signature-health row passes: it is `in_scope`, off, and ships
/// on. It is a reduction — it just is not a *switch*, which is what `reason`
/// says and what [`reducedSummary`] counts separately.
export function isReducedRow(f: FeatureState): boolean {
  return f.in_scope && !f.effective && f.default_on;
}

/// The features resolved OFF for one tab, for the tab badge and its popover.
export function reducedFeaturesFor(
  status: InjectionStatus | null,
  tab: TabId,
): FeatureState[] {
  const scope = status?.scopes.find((s) => s.scope === tab);
  return (scope?.features ?? []).filter(isReducedRow);
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
export function reducedSummary(status: InjectionStatus | null): string {
  const switched = new Set<string>();
  const inert = new Set<string>();
  for (const scope of status?.scopes ?? []) {
    for (const f of scope.features) {
      if (isReducedRow(f)) (f.reason ? inert : switched).add(f.feature);
    }
  }
  const parts: string[] = [];
  if (switched.size > 0) {
    parts.push(`${switched.size} control${switched.size === 1 ? '' : 's'} switched off`);
  }
  if (inert.size > 0) {
    parts.push(`${inert.size} layer${inert.size === 1 ? '' : 's'} switched on but inert`);
  }
  // The backend can report `reduced` for a reason no row expresses (it is the
  // source of truth and this side must not argue with it), so the chip still
  // has something to say when nothing here matches.
  return parts.join(', ') || 'something is off';
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

/// Fold "the signature layer is on and has no rules to match with" into the
/// injection hierarchy as an extra row.
///
/// **Why this exists (#48, D-2).** The reduced-protection surfaces are derived
/// entirely from settings toggles, so a signature layer that had been disarmed
/// — a rules directory that compiles to nothing, after which `scan` returns
/// empty and every page reports clean — rendered *full protection* as long as
/// the toggle was on. Decision 16 gives these surfaces one job ("protection
/// cannot be off and forgotten"), and a layer that is switched on and doing
/// nothing is the clearest case of exactly that.
///
/// Added **only to scopes where the detection feature both applies and
/// resolves on**: a scope that never screens, or one the user switched off, is
/// not reduced by a rules directory it does not read. `reduced` follows the
/// same rule, so it can never be raised by a row nobody got.
export function withSignatureHealth(
  status: InjectionStatus | null,
  rules: { armed: boolean } | null,
): InjectionStatus | null {
  if (!status || !rules || rules.armed) return status;
  const row: FeatureState = {
    feature: SIGNATURE_RULES_FEATURE,
    label: 'Signature rules loaded',
    effective: false,
    decided_by: 'feature',
    override_value: 'inherit',
    in_scope: true,
    // It ships loaded, so "not loaded" is a reduction and not a baseline.
    default_on: true,
    // Nothing to bake into a spawn: this row is a fact about the rules
    // directory, not a switch. It never reaches the Settings matrix.
    spawn_baked: false,
    reason: 'the rules directory compiled to no usable rules',
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
export function startLatchPolling(): () => void {
  let stopped = false;
  let health = HEALTHY_POLL;
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
      // read. `detectionStatus` swallows its own failure and returns null,
      // which leaves the hierarchy exactly as the backend published it.
      const detection = await detectionStatus();
      if (!stopped) {
        injectionStatus.set(withSignatureHealth(status, detection?.rules ?? null));
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
