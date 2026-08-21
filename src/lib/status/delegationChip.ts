/// V39 Phase A — the `delegation` status-bar chip, as a VALUE.
///
/// Phase A counts **read-only tabs**: a tab that has silently stopped
/// accepting the keyboard is exactly the state a user forgets they set, and a
/// status bar that never mentions it is how "my Claude tab is broken" gets
/// filed. Later phases switch the count to in-flight delegations (decision 7),
/// which is why the chip is named `delegation` and not `read-only`.
///
/// Derived here rather than in the component for the `sandboxChip.ts` reason:
/// a `.svelte` file has no test harness in this repo, and a chip whose whole
/// job is to report a state must not be free to misreport it.
import type { Settings } from '../settings/types';

export interface DelegationChipState {
  /// Render the chip at all. Hidden at zero — an always-on "RO 0" is noise in
  /// a bar that is already busy, and the states this reports are transient.
  visible: boolean;
  /// How many tabs currently refuse the keyboard.
  count: number;
  /// The word the chip wears.
  label: string;
  title: string;
}

export function delegationChipState(settings: Settings): DelegationChipState {
  const locked = settings.tabs.filter((t) => t.kind === 'ai_tool' && t.read_only);
  const count = locked.length;
  const names = locked.map((t) => t.name).join(', ');
  return {
    visible: count > 0,
    count,
    label: `RO ${count}`,
    title:
      count === 0
        ? 'No tab is read-only.'
        : `${count} tab${count === 1 ? '' : 's'} read-only (${names}) — their keyboards are refused; the tabs keep running. Use the ⇄ icon on a tab to allow input again.`,
  };
}
