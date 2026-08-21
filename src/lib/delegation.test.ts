import { describe, it, expect } from 'vitest';
import {
  accessOf,
  drivenReason,
  glyphState,
  hasCommIcon,
  isMouseWheel,
  isTerminalReply,
  readOnlyExempt,
  readOnlyReason,
  readOnlyRefusalMessage,
  withTabReadOnly,
  READ_ONLY_USER_REASON,
  type DelegationRole,
  type TabAccess,
} from './delegation';
import { defaultSettings, type Settings } from './settings/types';

/// V39 Phase A — the communication glyph and the read-only lock.
///
/// The properties worth pinning are the ones about not lying: the glyph says
/// *driven* exactly when a delegation is running, the lock overlay tracks the
/// stored access, every tooltip says what a click does, and the refusal
/// sentence the widget shows is the same one the backend sends.

/// The default settings mirror seeds the two Claude AI tabs and no shell, so
/// the fixture adds one: half of what these functions promise is that a Shell
/// tab never grows a communication icon or a lock.
function settingsWith(readOnly: string[] = []): Settings {
  const s = defaultSettings();
  for (const t of s.tabs) {
    if (t.kind === 'ai_tool' && readOnly.includes(t.id)) t.read_only = true;
  }
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
  return s;
}

describe('glyphState', () => {
  it('is off and unlocked on an ordinary tab — the Phase A resting state', () => {
    expect(glyphState({ role: 'none', access: 'rw', inFlight: false })).toMatchObject({
      state: 'off',
      locked: false,
    });
  });

  it('wears the lock whenever access is read-only, in every role', () => {
    for (const role of ['none', 'manual', 'remote'] as DelegationRole[]) {
      expect(glyphState({ role, access: 'ro', inFlight: false }).locked).toBe(true);
      expect(glyphState({ role, access: 'rw', inFlight: false }).locked).toBe(false);
    }
  });

  it('shows the role while idle', () => {
    expect(glyphState({ role: 'manual', access: 'rw', inFlight: false }).state).toBe('manual');
    expect(glyphState({ role: 'remote', access: 'rw', inFlight: false }).state).toBe('remote');
  });

  it('lets driven win over every role while a delegation is in flight', () => {
    for (const role of ['none', 'manual', 'remote'] as DelegationRole[]) {
      for (const access of ['rw', 'ro'] as TabAccess[]) {
        expect(glyphState({ role, access, inFlight: true }).state).toBe('driven');
      }
    }
  });

  it('names the driver in the in-flight title, and never leaves it blank', () => {
    expect(
      glyphState({ role: 'none', access: 'rw', inFlight: true, driverName: 'api-work' }).title,
    ).toContain('driven by api-work');
    expect(glyphState({ role: 'none', access: 'rw', inFlight: true }).title).toContain(
      'driven by another tab',
    );
    expect(
      glyphState({ role: 'none', access: 'rw', inFlight: true, driverName: '  ' }).title,
    ).toContain('driven by another tab');
  });

  it('names the backend of a remote-offload tab when there is one', () => {
    expect(
      glyphState({ role: 'remote', access: 'rw', inFlight: false, backendName: 'lan-worker-2' })
        .title,
    ).toContain('lan-worker-2');
    // …and reads cleanly when there is not.
    expect(glyphState({ role: 'remote', access: 'rw', inFlight: false }).title).not.toContain(
      '""',
    );
  });

  it('always says what the state is and what a click does', () => {
    for (const role of ['none', 'manual', 'remote'] as DelegationRole[]) {
      for (const access of ['rw', 'ro'] as TabAccess[]) {
        for (const inFlight of [false, true]) {
          const { title } = glyphState({ role, access, inFlight });
          expect(title.trim().length).toBeGreaterThan(0);
          expect(title).toContain('Click to change access');
        }
      }
    }
  });

  it('states the read-only reason in the same words the backend uses', () => {
    expect(glyphState({ role: 'none', access: 'ro', inFlight: false }).title).toContain(
      READ_ONLY_USER_REASON,
    );
  });
});

describe('access + reason', () => {
  it('reads the persisted flag, and treats an unknown tab as writable', () => {
    expect(accessOf(settingsWith(), 'claude')).toBe('rw');
    expect(accessOf(settingsWith(['claude']), 'claude')).toBe('ro');
    expect(accessOf(settingsWith(['claude']), 'no-such-tab')).toBe('rw');
  });

  it('gives a reason exactly when it refuses', () => {
    expect(readOnlyReason(settingsWith(), 'claude')).toBeNull();
    expect(readOnlyReason(settingsWith(['claude']), 'claude')).toBe(READ_ONLY_USER_REASON);
    expect(readOnlyReason(settingsWith(['claude']), 'shell-default-1')).toBeNull();
  });

  it('puts the icon on AI tabs only', () => {
    expect(hasCommIcon(settingsWith(), 'claude')).toBe(true);
    expect(hasCommIcon(settingsWith(), 'shell-default-1')).toBe(false);
    expect(hasCommIcon(settingsWith(), 'events')).toBe(false);
  });

  it('never renders a blank driver name', () => {
    expect(drivenReason('api-work')).toBe('driven by api-work');
    expect(drivenReason('   ')).toBe('driven by another tab');
  });
});

describe('withTabReadOnly', () => {
  it('clones rather than mutating, and moves only the named tab', () => {
    const before = settingsWith();
    const snapshot = JSON.stringify(before);
    const after = withTabReadOnly(before, 'claude', true);
    expect(JSON.stringify(before)).toBe(snapshot);
    expect(after).not.toBe(before);
    expect(accessOf(after, 'claude')).toBe('ro');
    expect(after.tabs[1]).toBe(before.tabs[1]);
  });

  it('is a no-op for a tab that is not an AI tab', () => {
    const shell = withTabReadOnly(settingsWith(), 'shell-default-1', true).tabs.at(-1);
    expect(shell).toMatchObject({ kind: 'shell' });
    expect(shell && 'read_only' in shell).toBe(false);
  });
});

describe('isTerminalReply', () => {
  /// The same fixtures the Rust side asserts on
  /// (`terminal_protocol_replies_are_not_refused_by_the_lock`) — the courtesy
  /// gate and the enforcement point must agree about what counts as input, or
  /// one of them wedges a TUI the other was happy to answer.
  it('passes the terminal answering the program', () => {
    for (const reply of ['\x1b[24;80R', '\x1b[?1;2c', '\x1b[0n', '\x1b[I', '\x1b[O']) {
      expect(isTerminalReply(reply)).toBe(true);
    }
  });

  it('does not pass anything a person typed', () => {
    for (const keys of ['\x1b[A', '\x1b', '\r', 'y', '\x1b[200~pasted\x1b[201~', '']) {
      expect(isTerminalReply(keys)).toBe(false);
    }
  });
});

/// The same fixture table the Rust side asserts on
/// (`mouse_wheel_passes_the_lock_under_either_source` and its two neighbours).
/// The courtesy gate and the enforcement point must agree about every row, or
/// one of them refuses a scroll the other allowed.
const WHEEL_REPORTS = [
  '\x1b[<64;10;5M', // wheel up (SGR)
  '\x1b[<65;10;5M', // wheel down
  '\x1b[<66;1;1M', // wheel left
  '\x1b[<67;1;1M', // wheel right
  '\x1b[<80;3;4M', // ctrl+wheel up (64 + modifier 16)
  '\x1b[<68;3;4M', // shift+wheel up (64 + modifier 4)
  '\x1b[M`!!', // wheel up, legacy X10 encoding
  '\x1b[Ma!!', // wheel down, legacy X10 encoding
  '\x1b[<64;1;1M\x1b[<64;1;1M', // a fast scroll, coalesced
];

const CLICKS_AND_PASTES = [
  '\x1b[<0;10;5M', // left press
  '\x1b[<0;10;5m', // left release
  '\x1b[<1;1;1M', // middle press
  '\x1b[<2;1;1M', // right press
  '\x1b[<32;5;5M', // drag with button 0 held (motion bit)
  '\x1b[<35;5;5M', // bare motion
  '\x1b[M !!', // left press, legacy X10 encoding
  '\x1b[M#!!', // release, legacy X10 encoding
  '\x1b[200~x\x1b[201~', // a bracketed paste
];

const SMUGGLED = [
  '\x1b[<64;1;1My', // wheel then a keystroke
  'y\x1b[<64;1;1M', // keystroke then wheel
  '\x1b[<64;1;1M\r', // wheel then Enter
  '\x1b[<64;1;1M\x1b[<0;1;1M', // wheel then a click
  '\x1b[<64;1;1', // truncated: no terminator
  '\x1b[M`!', // truncated X10: two coord bytes
  '',
];

describe('isMouseWheel', () => {
  it('passes the wheel — scrolling is reading, and a read-only tab is for watching', () => {
    for (const wheel of WHEEL_REPORTS) {
      expect(isMouseWheel(wheel), wheel).toBe(true);
      expect(readOnlyExempt(wheel), wheel).toBe(true);
    }
  });

  it('refuses clicks, drags and pastes — those activate controls', () => {
    for (const click of CLICKS_AND_PASTES) {
      expect(isMouseWheel(click), click).toBe(false);
      expect(readOnlyExempt(click), click).toBe(false);
    }
  });

  it('cannot carry a passenger: a wheel report plus anything else is refused', () => {
    for (const smuggled of SMUGGLED) {
      expect(isMouseWheel(smuggled), smuggled).toBe(false);
      expect(readOnlyExempt(smuggled), smuggled).toBe(false);
    }
  });

  it('leaves isTerminalReply alone — the wheel is not an automatic reply', () => {
    for (const wheel of WHEEL_REPORTS) {
      expect(isTerminalReply(wheel), wheel).toBe(false);
    }
  });

  it('still exempts the terminal replies it always did', () => {
    for (const reply of ['\x1b[24;80R', '\x1b[?1;2c', '\x1b[0n', '\x1b[I', '\x1b[O']) {
      expect(readOnlyExempt(reply), reply).toBe(true);
    }
  });

  it('refuses ordinary keyboard input', () => {
    for (const keys of ['\x1b[A', '\x1b', '\r', 'y']) {
      expect(readOnlyExempt(keys), keys).toBe(false);
    }
  });
});

describe('readOnlyRefusalMessage', () => {
  it('recognizes the backend refusal and hands back the whole sentence', () => {
    expect(readOnlyRefusalMessage('tab `claude` is read-only (user)')).toBe(
      'tab `claude` is read-only (user)',
    );
    expect(readOnlyRefusalMessage('tab `claude` is driven by api-work')).toContain(
      'driven by api-work',
    );
  });

  it('leaves unrelated PTY failures alone', () => {
    expect(readOnlyRefusalMessage('PTY operation failed: pipe closed')).toBeNull();
    expect(readOnlyRefusalMessage(new Error('unknown tab'))).toBeNull();
    expect(readOnlyRefusalMessage(null)).toBeNull();
    expect(readOnlyRefusalMessage({ kind: 'whatever' })).toBeNull();
  });
});
