import { describe, it, expect } from 'vitest';
import { delegationChipState } from './delegationChip';
import { defaultSettings, type Settings } from '../settings/types';

/// V39 Phase A — the `delegation` chip.
///
/// It reports a state the user set and then forgot: the chip must appear
/// exactly when at least one tab is refusing input, count the same tabs the
/// lock does, and name them so the user knows which tab to go fix.

function withLocked(...ids: string[]): Settings {
  const s = defaultSettings();
  for (const t of s.tabs) {
    if (t.kind === 'ai_tool' && ids.includes(t.id)) t.read_only = true;
  }
  return s;
}

describe('delegationChipState', () => {
  it('is hidden while nothing is locked', () => {
    expect(delegationChipState(defaultSettings())).toMatchObject({
      visible: false,
      count: 0,
    });
  });

  it('appears and counts as soon as one tab is locked', () => {
    expect(delegationChipState(withLocked('claude'))).toMatchObject({
      visible: true,
      count: 1,
      label: 'RO 1',
    });
    expect(delegationChipState(withLocked('claude', 'claude-local'))).toMatchObject({
      visible: true,
      count: 2,
      label: 'RO 2',
    });
  });

  it('names the locked tabs and says how to unlock them', () => {
    const { title } = delegationChipState(withLocked('claude'));
    expect(title).toContain('Claude');
    expect(title).toContain('read-only');
    expect(title).toContain('⇄');
  });

  it('gets the plural right — a chip that says "1 tabs" reads as a bug', () => {
    expect(delegationChipState(withLocked('claude')).title).toContain('1 tab read-only');
    expect(delegationChipState(withLocked('claude', 'claude-local')).title).toContain(
      '2 tabs read-only',
    );
  });

  it('counts AI tabs only — no other tab kind can carry the lock', () => {
    const s = defaultSettings();
    s.tabs = [
      ...s.tabs,
      {
        kind: 'shell',
        id: 'shell-default-1',
        builtin: false,
        name: 'Shell',
        command: 'bash',
        args: [],
        cwd: null,
        env: {},
        notifications: {
          error: { enabled: true, text: '' },
          exited: { enabled: true, text: '' },
        },
        theme_override: null,
        background_override: null,
      },
    ];
    expect(delegationChipState(s).count).toBe(0);
  });
});
