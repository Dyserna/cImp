/// V39 — the `delegation` status-bar chip, as a VALUE.
///
/// **Phase B switches what it counts, and the switch was always the plan**
/// (locked decision 7: "status-bar gets one chip, `delegation`, counting
/// in-flight delegations"). Phase A counted read-only TABS because nothing
/// could start a delegation yet and a chip with no possible non-zero state is
/// not a chip; that is why it was named `delegation` and not `read-only`.
///
/// The count is now flights, not locks, because they are not the same fact and
/// only one of them is transient: a tab the user locked stays locked until they
/// say otherwise and its own glyph says so, whereas a tab another harness is
/// driving right now is a thing happening in the app that the user may not be
/// looking at. The chip is for the second. (Nothing is lost: the auto lock is a
/// consequence of a flight, so every driven tab is counted here anyway, and the
/// per-tab lock keeps its glyph, its lock overlay and its refusal toast.)
///
/// Derived here rather than in the component for the `sandboxChip.ts` reason: a
/// `.svelte` file has no test harness in this repo, and a chip whose whole job
/// is to report a state must not be free to misreport it.
import type { InFlightView } from '../delegation';
import { harnessLabel } from '../delegation';
import type { Settings } from '../settings/types';

export interface DelegationChipState {
  /// Render the chip at all. Hidden at zero — an always-on "0" is noise in a
  /// bar that is already busy, and a delegation is by nature transient.
  visible: boolean;
  /// How many tabs are being driven right now.
  count: number;
  /// The word the chip wears.
  label: string;
  title: string;
}

/// `inFlight` is the `delegation-changed` snapshot keyed by WORKER tab id;
/// `settings` supplies display names, because a tab id is not what the user
/// called the tab.
export function delegationChipState(
  inFlight: Record<string, InFlightView>,
  settings: Settings,
): DelegationChipState {
  const rows = Object.entries(inFlight).sort(([a], [b]) => a.localeCompare(b));
  const count = rows.length;
  const nameOf = (id: string): string => {
    const t = settings.tabs.find((x) => x.id === id);
    return t ? t.name : id;
  };
  // Each entry names BOTH ends. "2 delegations" answers nothing a user who has
  // noticed the chip wants to know; which of their tabs is being typed into,
  // and by whom, is the whole question.
  const detail = rows
    .map(([tab, v]) => `${nameOf(tab)} ← ${harnessLabel(v.driver_agent)} (${v.driver_name})`)
    .join('; ');
  const waiting = rows.filter(([, v]) => v.awaiting_prompt).length;
  return {
    visible: count > 0,
    count,
    label: `DLG ${count}`,
    title:
      count === 0
        ? 'No delegation is running.'
        : `${count} tab${count === 1 ? ' is' : 's are'} being driven by another harness: ${detail}.` +
          (waiting > 0
            ? ` ${waiting} of them ${waiting === 1 ? 'is' : 'are'} waiting for your permission.`
            : '') +
          ' Take over from the tab’s ⇄ icon or its context menu.',
  };
}
