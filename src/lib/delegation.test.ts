import { describe, it, expect } from 'vitest';
import {
  accessOf,
  attributionLine,
  backendOf,
  displacedToast,
  drivenReason,
  FACADE_NAME_TAKEN,
  courtesyRefusal,
  defaultFacadeName,
  facadeBackends,
  elapsedLabel,
  glyphState,
  harnessLabel,
  hasCommIcon,
  isMouseWheel,
  isTerminalReply,
  manualHolderFor,
  readOnlyExempt,
  readOnlyAdvice,
  readOnlyReason,
  readOnlyRefusalMessage,
  roleOf,
  tabHarness,
  withTabReadOnly,
  writeLocalEcho,
  READ_ONLY_USER_REASON,
  type DelegationRole,
  type TabAccess,
} from './delegation';
import { isPromptRelaxed, setPromptRelaxedTabs } from './delegationPrompt';
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
    for (const role of ['none', 'manual', 'remote_offload'] as DelegationRole[]) {
      expect(glyphState({ role, access: 'ro', inFlight: false }).locked).toBe(true);
      expect(glyphState({ role, access: 'rw', inFlight: false }).locked).toBe(false);
    }
  });

  it('shows the role while idle', () => {
    expect(glyphState({ role: 'manual', access: 'rw', inFlight: false }).state).toBe('manual');
    expect(glyphState({ role: 'remote_offload', access: 'rw', inFlight: false }).state).toBe('remote');
  });

  it('lets driven win over every role while a delegation is in flight', () => {
    for (const role of ['none', 'manual', 'remote_offload'] as DelegationRole[]) {
      for (const access of ['rw', 'ro'] as TabAccess[]) {
        expect(glyphState({ role, access, inFlight: true }).state).toBe('driven');
      }
    }
  });

  /// Phase B: the in-flight title IS the attribution line of locked decision
  /// 2a — the same sentence the banner and the local echo carry, so the three
  /// surfaces cannot drift into three paraphrases of one fact.
  it('names the driver in the in-flight title, and never leaves it blank', () => {
    expect(
      glyphState({
        role: 'none',
        access: 'rw',
        inFlight: true,
        driverAgent: 'opencode',
        driverName: 'api-work',
      }).title,
    ).toContain('[delegated by OpenCode \u00b7 tab "api-work" \u00b7 via cImp]');
    expect(glyphState({ role: 'none', access: 'rw', inFlight: true }).title).toContain(
      'tab "another tab"',
    );
    expect(
      glyphState({ role: 'none', access: 'rw', inFlight: true, driverName: '  ' }).title,
    ).toContain('tab "another tab"');
  });

  it('names the backend of a remote-offload tab when there is one', () => {
    expect(
      glyphState({ role: 'remote_offload', access: 'rw', inFlight: false, backendName: 'lan-worker-2' })
        .title,
    ).toContain('lan-worker-2');
    // …and reads cleanly when there is not.
    expect(glyphState({ role: 'remote_offload', access: 'rw', inFlight: false }).title).not.toContain(
      '""',
    );
  });

  it('always says what the state is and what a click does', () => {
    for (const role of ['none', 'manual', 'remote_offload'] as DelegationRole[]) {
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

// ── V39 Phase B ─────────────────────────────────────────────────────────────

describe('harnessLabel + attributionLine', () => {
  it('names the two harnesses cImp ships with', () => {
    expect(harnessLabel('claude')).toBe('Claude Code');
    expect(harnessLabel('opencode')).toBe('OpenCode');
  });

  it('renders an unknown harness as its own id, never as a guess', () => {
    // A harness added after this build must read as itself, not as "unknown"
    // and not as one of the two above.
    expect(harnessLabel('aider')).toBe('aider');
    expect(harnessLabel('')).toBe('another harness');
    expect(harnessLabel(null)).toBe('another harness');
  });

  it('is the decision-2a line, exactly', () => {
    expect(attributionLine('opencode', 'api-work')).toBe(
      '[delegated by OpenCode \u00b7 tab "api-work" \u00b7 via cImp]',
    );
  });

  it('never renders an empty tab name', () => {
    expect(attributionLine('claude', '   ')).toContain('tab "another tab"');
    expect(attributionLine('claude', null)).toContain('tab "another tab"');
  });
});

describe('writeLocalEcho', () => {
  /// **The invariant of locked decision 2a.** The attribution is client-side
  /// display: it goes to the xterm widget the user is looking at, and there is
  /// no path from it to the PTY. The test is the whole point — a helper that
  /// one day reached for `pty_write` would put provenance text into the worker
  /// model's context, which is exactly what 2a forbids.
  it('writes to the terminal object and calls nothing else', () => {
    const written: string[] = [];
    const term = {
      writeln(data: string) {
        written.push(data);
      },
    };
    writeLocalEcho(term, 'opencode', 'api-work');
    expect(written).toHaveLength(1);
    expect(written[0]).toContain('[delegated by OpenCode \u00b7 tab "api-work" \u00b7 via cImp]');
  });

  it('styles the line dim italic and resets, so nothing leaks into PTY output', () => {
    const written: string[] = [];
    writeLocalEcho({ writeln: (d: string) => written.push(d) }, 'claude', 'main');
    expect(written[0]).toContain('\x1b[2;3m');
    expect(written[0].endsWith('\x1b[0m')).toBe(true);
  });
});

/// **Locked decision 2a, as a structural guard.** The attribution is
/// client-side display: the worker model receives the task verbatim, with no
/// header and no marker, so a path from any of this text to the backend would
/// be the one defect 2a exists to prevent. A behavioural test can only show
/// that TODAY's helper does not call `invoke`; this shows the module has no way
/// to. Raw-source mechanism per `activity.test.ts`'s StatusChip scan: Vite's
/// glob, not node:fs.
const DELEGATION_SOURCE = import.meta.glob('/src/lib/delegation.ts', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

/// The banner's own source, for the same reason: a `.svelte` file has no test
/// harness here, and the rule under test (V39 review L-5) is a CSS property
/// whose absence is invisible until a user tries to click the terminal's first
/// row on a driven tab.
const BANNER_SOURCE = import.meta.glob('/src/lib/DelegationBanner.svelte', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

describe('the banner overlay does not eat the terminal it covers', () => {
  /// It is a full-width `position: absolute` strip at `top: 0` of
  /// `.pane-content` (an overlay on purpose — a real row would refit xterm and
  /// resize the PTY mid-turn). With `pointer-events: auto` on the strip, every
  /// click and wheel report aimed at the first row of an alt-screen TUI landed
  /// on the banner instead. The strip is transparent; only the button is not.
  it('makes the strip pointer-transparent and re-enables only the button', () => {
    const src = Object.values(BANNER_SOURCE)[0] ?? '';
    expect(src.length).toBeGreaterThan(0);
    // Comments first: the rules below are EXPLAINED in prose that names the
    // property they replace, and a scan that could not tell prose from CSS
    // would either fail on the documentation or force it to be deleted.
    const css = src.slice(src.indexOf('<style>')).replace(/\/\*[\s\S]*?\*\//g, '');
    const rule = (selector: string): string => {
      const at = css.indexOf(selector);
      expect(at, `${selector} must exist`).toBeGreaterThan(-1);
      const open = css.indexOf('{', at);
      return css.slice(open, css.indexOf('}', open));
    };
    expect(rule('.delegation-banner {')).toMatch(/pointer-events:\s*none/);
    expect(rule('.takeover {')).toMatch(/pointer-events:\s*auto/);
    // …and nothing else on the strip claims the pointer back.
    const claims = css.match(/pointer-events:\s*auto/g) ?? [];
    expect(claims).toHaveLength(1);
  });
});

describe('the attribution never reaches the backend', () => {
  it('leaves delegation.ts with no transport at all', () => {
    const src = Object.values(DELEGATION_SOURCE)[0] ?? '';
    expect(src.length).toBeGreaterThan(0);
    // Strip comments first: `pty_write` and `invoke` are NAMED in the doc
    // comments on purpose (they say what must not happen), and a scan that
    // could not tell prose from code would either fail on the documentation or
    // force it to be deleted.
    const code = src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');
    expect(code).not.toMatch(/\binvoke\s*[<(]/);
    expect(code).not.toMatch(/\bpty_write\b/);
    expect(code).not.toMatch(/from '\.\/ipc'/);
    expect(code).not.toMatch(/from '@tauri-apps/);
  });

  it('writes the echo through the passed terminal and nowhere else', () => {
    // The helper takes its target as an argument, so there is no module-level
    // handle it could reach for instead.
    const calls: string[] = [];
    const target = {
      writeln(d: string) {
        calls.push(d);
      },
    };
    writeLocalEcho(target, 'opencode', 'api-work');
    expect(calls).toHaveLength(1);
    expect(Object.keys(target)).toEqual(['writeln']);
  });
});

describe('elapsedLabel', () => {
  it('counts seconds under a minute', () => {
    expect(elapsedLabel(1_000, 1_000)).toBe('0s');
    expect(elapsedLabel(1_000, 10_400)).toBe('9s');
    expect(elapsedLabel(0, 59_999)).toBe('59s');
  });

  it('switches to minutes so a long flight stays readable', () => {
    expect(elapsedLabel(0, 60_000)).toBe('1m 00s');
    expect(elapsedLabel(0, 247_000)).toBe('4m 07s');
  });

  it('never goes backwards when the clock does', () => {
    expect(elapsedLabel(10_000, 1_000)).toBe('0s');
  });
});

describe('roles', () => {
  /// The fixture's two builtin AI tabs both run `claude`, so a second harness
  /// has to be added by hand — which is the case that matters: the Manual rule
  /// is per harness, not per app.
  function withRoles(roles: Record<string, DelegationRole>): Settings {
    const s = settingsWith();
    s.tabs = [
      ...s.tabs,
      {
        ...(s.tabs.find((t) => t.kind === 'ai_tool') as Extract<
          Settings['tabs'][number],
          { kind: 'ai_tool' }
        >),
        id: 'opencode',
        name: 'OpenCode',
        command: 'opencode',
      },
    ];
    for (const t of s.tabs) {
      if (t.kind === 'ai_tool' && roles[t.id]) t.delegation_role = roles[t.id];
    }
    return s;
  }

  it('reads the persisted role, and gives a non-AI tab none', () => {
    expect(roleOf(withRoles({ claude: 'manual' }), 'claude')).toBe('manual');
    expect(roleOf(withRoles({}), 'claude')).toBe('none');
    expect(roleOf(withRoles({}), 'shell-default-1')).toBe('none');
    expect(roleOf(withRoles({}), 'no-such-tab')).toBe('none');
  });

  it('classifies a tab by harness the way tab_consumer does', () => {
    const s = withRoles({});
    const claude = s.tabs.find((t) => t.id === 'claude');
    const opencode = s.tabs.find((t) => t.id === 'opencode');
    expect(claude && claude.kind === 'ai_tool' && tabHarness(claude)).toBe('claude');
    expect(opencode && opencode.kind === 'ai_tool' && tabHarness(opencode)).toBe('opencode');
  });

  it('matches on the file stem, case-insensitively, like command_is', () => {
    const base = settingsWith().tabs.find((t) => t.kind === 'ai_tool') as Extract<
      Settings['tabs'][number],
      { kind: 'ai_tool' }
    >;
    expect(tabHarness({ ...base, command: 'C:\\bin\\Claude.EXE' })).toBe('claude');
    expect(tabHarness({ ...base, command: '/usr/bin/claude' })).toBe('claude');
    // A wrapper is `opencode` to BOTH ends — deliberately the same answer Rust
    // gives, not a better one: the popover must not name a tab the backend
    // would never have displaced.
    expect(tabHarness({ ...base, command: 'claude-code.cmd' })).toBe('opencode');
    expect(tabHarness({ ...base, command: '' })).toBe('opencode');
  });

  it('names the tab that currently holds Manual for this tab s harness', () => {
    const s = withRoles({ claude: 'manual', opencode: 'manual' });
    // `claude-local` also runs `claude`, so its Manual holder is the Claude tab.
    expect(manualHolderFor(s, 'claude-local')).toMatchObject({ id: 'claude' });
    // The holder itself is not its own holder.
    expect(manualHolderFor(s, 'claude')).toBeNull();
    // …and the OpenCode tab's holder is never a Claude tab: one Manual PER
    // HARNESS, so the two roles coexist.
    expect(manualHolderFor(s, 'opencode')).toBeNull();
  });

  it('has no holder to name when nobody holds it, or the tab is not an AI tab', () => {
    expect(manualHolderFor(withRoles({}), 'claude-local')).toBeNull();
    expect(manualHolderFor(withRoles({ claude: 'manual' }), 'shell-default-1')).toBeNull();
  });

  it('says what moved, in the displaced tab s own words', () => {
    expect(displacedToast('api-work', 'claude', 'review')).toBe(
      '\u201capi-work\u201d is no longer the Manual Claude Code tab \u2014 moved to \u201creview\u201d.',
    );
  });
});

/// V39 Phase C — the facade backends the Settings window lists read-only.
///
/// The list is a re-derivation of the Rust synthesis, so what these pin is the
/// two places the two could disagree: WHICH tabs contribute a backend, and what
/// a tab with no chosen backend name is called.
describe('facadeBackends', () => {
  function withRole(role: DelegationRole, name: string | null): Settings {
    const s = defaultSettings();
    for (const t of s.tabs) {
      if (t.kind === 'ai_tool' && t.id === 'claude') {
        t.name = 'api-work';
        t.delegation_role = role;
        t.delegation_backend = { name, tier: 'fast', declared_context: 128000 };
      }
    }
    return s;
  }

  it('lists one row per Remote-offload tab, and nothing else', () => {
    expect(facadeBackends(withRole('remote_offload', 'lan-worker-2'))).toEqual([
      {
        tabId: 'claude',
        tabName: 'api-work',
        name: 'lan-worker-2',
        tier: 'fast',
        declaredContext: 128000,
        inPool: true,
        droppedReason: null,
      },
    ]);
    // The other two roles are not backends — one enum, not two flags.
    expect(facadeBackends(withRole('manual', 'lan-worker-2'))).toEqual([]);
    expect(facadeBackends(withRole('none', 'lan-worker-2'))).toEqual([]);
    expect(facadeBackends(defaultSettings())).toEqual([]);
  });

  /// **A collision is SAID, not silently dropped** (V39 review M-9).
  ///
  /// `Settings::effective_offload_backends` drops a facade whose name is
  /// already taken — rather than renaming it, because the router keys on the
  /// name — and warns once into the log. This list is the only surface a user
  /// looks at, and it used to render the row as if it were live: a backend the
  /// router would never pick, with nothing anywhere saying why.
  it('marks a facade whose name is taken as not in the pool', () => {
    const s = withRole('remote_offload', 'lan-worker-2');
    s.offload.backends = [
      { ...s.offload.backends[0], name: 'lan-worker-2' } as (typeof s.offload.backends)[0],
    ];
    const [row] = facadeBackends(s);
    expect(row.inPool).toBe(false);
    expect(row.droppedReason).toBe(FACADE_NAME_TAKEN);
    expect(row.name).toBe('lan-worker-2');
  });

  it('drops the SECOND of two facades answering to one name', () => {
    const s = withRole('remote_offload', 'twin');
    for (const t of s.tabs) {
      if (t.kind === 'ai_tool' && t.id === 'claude-local') {
        t.delegation_role = 'remote_offload';
        t.delegation_backend = { name: 'twin', tier: 'quality', declared_context: null };
      }
    }
    const rows = facadeBackends(s);
    expect(rows).toHaveLength(2);
    expect(rows.map((r) => r.inPool)).toEqual([true, false]);
    expect(rows[1].droppedReason).toBe(FACADE_NAME_TAKEN);
  });

  it('falls back to an opaque per-tab name, never the tab name', () => {
    // Both spellings of "no name chosen": the field never set, and a text input
    // the user cleared. V39 review L-2: the old fallback was the tab's DISPLAY
    // NAME, which `offload::mcp::backend_label` renders into `offload_task`'s
    // description — the asking model could read off what its "LAN backend"
    // really was.
    const rows = facadeBackends(withRole('remote_offload', null));
    expect(rows[0].name).toBe(defaultFacadeName(rows[0].tabId));
    expect(rows[0].name).not.toContain('api-work');
    expect(facadeBackends(withRole('remote_offload', '   '))[0].name).toBe(rows[0].name);
  });

  /// The Rust side (`settings::facade_default_name`) pins the same three
  /// strings: the popover's placeholder and the Settings list render this
  /// before any backend call, so the two implementations have to agree byte for
  /// byte, not merely in shape.
  it('answers exactly what the Rust default answers', () => {
    expect(defaultFacadeName('t1')).toBe('worker-0844');
    expect(defaultFacadeName('t2')).toBe('worker-0744');
    expect(defaultFacadeName('ai-9f3c')).toBe('worker-f0cb');
    expect(defaultFacadeName('t1')).toBe(defaultFacadeName('t1'));
    expect(defaultFacadeName('t1')).not.toBe(defaultFacadeName('t2'));
    expect(defaultFacadeName('t1')).toMatch(/^worker-[0-9a-f]{4}$/);
  });
});

describe('courtesyRefusal', () => {
  /// **V39 review R-4, and the end-to-end half of M-5.** The backend opens the
  /// keyboard while a driven worker holds a prompt (locked decision 5), but the
  /// terminal's courtesy gate swallowed the keystroke before `pty_write` was
  /// ever reached — so on a tab the user had ALSO locked by hand, the one
  /// prompt only the user can answer could not be answered, and the delegation
  /// ran to its deadline reporting "worker awaiting permission".
  it('forwards the answer to a standing prompt on a user-locked driven tab', () => {
    const locked = withTabReadOnly(settingsWith(), 'claude', true);
    expect(readOnlyReason(locked, 'claude')).toBe(READ_ONLY_USER_REASON);
    // Driven, and the worker is waiting on the user.
    expect(courtesyRefusal(locked, 'claude', true)).toBeNull();
  });

  it('still refuses when no prompt is standing', () => {
    const locked = withTabReadOnly(settingsWith(), 'claude', true);
    expect(courtesyRefusal(locked, 'claude', false)).toBe(READ_ONLY_USER_REASON);
  });

  it('says nothing about a tab that was never locked', () => {
    expect(courtesyRefusal(settingsWith(), 'claude', false)).toBeNull();
    expect(courtesyRefusal(settingsWith(), 'claude', true)).toBeNull();
  });
});

describe('the prompt-relaxed mirror', () => {
  /// The module exists to break an import cycle (`delegationState` → `terminals`
  /// already), so what is worth pinning is that it is a plain snapshot: replaced
  /// wholesale, exactly like the store the `delegation-changed` payload feeds.
  it('replaces the set wholesale, like the snapshot it comes from', () => {
    setPromptRelaxedTabs(['claude', 'opencode']);
    expect(isPromptRelaxed('claude')).toBe(true);
    expect(isPromptRelaxed('opencode')).toBe(true);
    expect(isPromptRelaxed('claude-local')).toBe(false);

    setPromptRelaxedTabs(['claude-local']);
    expect(isPromptRelaxed('claude')).toBe(false);
    expect(isPromptRelaxed('claude-local')).toBe(true);

    setPromptRelaxedTabs([]);
    expect(isPromptRelaxed('claude-local')).toBe(false);
  });
});

describe('readOnlyAdvice', () => {
  /// The two locks end differently, so they must not give the same advice: the
  /// Access radio is DISABLED for the whole flight, and Take over is what ends
  /// one.
  it('sends a driven tab to Take over, not to the access radio', () => {
    const advice = readOnlyAdvice('tab `claude` is driven by api-work');
    expect(advice).toContain('Take over');
    expect(advice).not.toContain('allow input again');
  });

  it('sends a user-locked tab to the glyph', () => {
    const advice = readOnlyAdvice('tab `claude` is read-only (user)');
    expect(advice).toContain('allow input again');
    expect(advice).not.toContain('Take over');
  });
});

/// The popover's source, for the M-10 guard below. Same raw-glob mechanism as
/// the banner scan above.
const POPOVER_SOURCE = import.meta.glob('/src/lib/DelegationPopover.svelte', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

describe('the facade knobs are a narrow write', () => {
  /// **V39 review M-10.** The knobs used to be saved by cloning the whole
  /// `Settings` and calling `applySettings` — a document written from a
  /// snapshot taken before some other write landed silently reverts it (the
  /// `40d2b32` class), and the write most likely to be racing it is the ROLE
  /// radio one line above in this same popover, which has its own command
  /// precisely because only the backend can enforce its cross-tab rule.
  ///
  /// Asserted on the source because a `.svelte` file has no test harness here,
  /// and what must hold is that the whole-document path is not reachable from
  /// this component at all.
  it('leaves the popover with no whole-document settings save', () => {
    const src = Object.values(POPOVER_SOURCE)[0] ?? '';
    expect(src.length).toBeGreaterThan(0);
    const code = src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');
    expect(code).not.toMatch(/\bapplySettings\b/);
    expect(code).toMatch(/\btabSetDelegationBackend\(/);
    expect(code).toMatch(/\btabSetDelegationRole\(/);
  });

  /// …and the helper that built such a document is gone, not merely unused: a
  /// clone-the-document helper one import away is the same defect waiting for
  /// the next component.
  it('leaves no whole-document backend helper in delegation.ts', () => {
    const src = Object.values(DELEGATION_SOURCE)[0] ?? '';
    const code = src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');
    expect(code).not.toMatch(/\bwithTabBackend\b/);
  });
});

describe('backendOf', () => {
  it('gives an unknown tab the documented defaults rather than throwing', () => {
    expect(backendOf(settingsWith(), 'no-such-tab')).toEqual({
      name: null,
      tier: 'quality',
      declared_context: null,
    });
  });
});
