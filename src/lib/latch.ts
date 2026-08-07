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
  /// every override; only a tab restart (session rotation) clears it.
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

/// Whether protection is reduced anywhere. Drives the status-bar indicator.
export const protectionReduced: Readable<boolean> = derived(
  injectionStatus,
  ($s) => !!$s?.reduced,
);

/// The features resolved OFF for one tab, for the tab badge and its popover.
///
/// Two filters, both structural rather than cosmetic:
/// - only rows the scope actually HAS — a tab is not "reduced" because the
///   worker-only canary does not apply to it;
/// - only features that ship ON (`default_on`). V32 Phase H's OpenCode native
///   gate defaults off by user decision, and counting it would raise the muted
///   badge on every tab of a fresh install — which is how a badge stops being
///   read. Mirrors the backend's `protection_reduced`, using the backend's own
///   `default_on` rather than a second list of defaults in TypeScript.
export function reducedFeaturesFor(
  status: InjectionStatus | null,
  tab: TabId,
): FeatureState[] {
  const scope = status?.scopes.find((s) => s.scope === tab);
  return (scope?.features ?? []).filter((f) => f.in_scope && !f.effective && f.default_on);
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
/// stop more than once. An individual failed poll is swallowed (the app may
/// still be starting) — the store keeps its last known value rather than
/// flickering every badge off.
export function startLatchPolling(): () => void {
  let stopped = false;
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
      if (!stopped) injectionStatus.set(withSignatureHealth(status, detection?.rules ?? null));
    } catch {
      /* app still starting — keep the last known hierarchy state */
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
    } catch {
      /* app still starting, or the command is unavailable — keep last state */
    }
  };
  void tick();
  const handle = setInterval(() => void tick(), POLL_MS);
  return () => {
    stopped = true;
    clearInterval(handle);
  };
}
