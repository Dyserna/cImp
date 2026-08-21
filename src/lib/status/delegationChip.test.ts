import { describe, it, expect } from 'vitest';
import { delegationChipState } from './delegationChip';
import type { InFlightView } from '../delegation';
import { defaultSettings, type Settings } from '../settings/types';

/// V39 Phase B — the `delegation` chip.
///
/// It reports something happening in the app that the user is not necessarily
/// looking at: another harness typing into one of their tabs. So the chip must
/// appear exactly when at least one flight is running, count the same flights
/// the engine holds, and name BOTH ends — which tab, and who is driving it.

function flight(over: Partial<InFlightView> = {}): InFlightView {
  return {
    driver: 'opencode',
    driver_name: 'api-work',
    driver_agent: 'opencode',
    mode: 'explicit',
    started_ms: 1_000,
    awaiting_prompt: false,
    ...over,
  };
}

function inFlight(...ids: string[]): Record<string, InFlightView> {
  return Object.fromEntries(ids.map((id) => [id, flight()]));
}

const SETTINGS: Settings = defaultSettings();

describe('delegationChipState', () => {
  it('is hidden while nothing is being driven', () => {
    expect(delegationChipState({}, SETTINGS)).toMatchObject({ visible: false, count: 0 });
  });

  it('appears and counts as soon as one flight starts', () => {
    expect(delegationChipState(inFlight('claude'), SETTINGS)).toMatchObject({
      visible: true,
      count: 1,
      label: 'DLG 1',
    });
    expect(delegationChipState(inFlight('claude', 'claude-local'), SETTINGS)).toMatchObject({
      visible: true,
      count: 2,
      label: 'DLG 2',
    });
  });

  it('names both ends — the tab being driven and the harness driving it', () => {
    const { title } = delegationChipState(inFlight('claude'), SETTINGS);
    // The tab's DISPLAY name, not its id: an id is not what the user called it.
    expect(title).toContain('Claude');
    expect(title).toContain('OpenCode');
    expect(title).toContain('api-work');
    expect(title).toContain('⇄');
  });

  it('gets the plural right — a chip that says "1 tabs" reads as a bug', () => {
    expect(delegationChipState(inFlight('claude'), SETTINGS).title).toContain('1 tab is being');
    expect(delegationChipState(inFlight('claude', 'claude-local'), SETTINGS).title).toContain(
      '2 tabs are being',
    );
  });

  it('says when a flight is stalled on a prompt only the user can answer', () => {
    const stalled = { claude: flight({ awaiting_prompt: true }) };
    expect(delegationChipState(stalled, SETTINGS).title).toContain(
      '1 of them is waiting for your permission',
    );
    // …and stays quiet when none is, rather than reporting a zero.
    expect(delegationChipState(inFlight('claude'), SETTINGS).title).not.toContain(
      'waiting for your permission',
    );
  });

  it('falls back to the tab id when settings do not know the tab', () => {
    // A worker whose tab was closed mid-flight is still a live row until the
    // engine's own edge lands; the chip must render it, not drop it.
    expect(delegationChipState(inFlight('ghost-tab'), SETTINGS).title).toContain('ghost-tab');
  });
});
