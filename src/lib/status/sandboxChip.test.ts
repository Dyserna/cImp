import { describe, it, expect } from 'vitest';
import {
  sandboxChipState,
  sandboxNetworkChipState,
  withSandbox,
} from './sandboxChip';
import { defaultSettings, type SandboxSettings } from '../settings/types';

/// V39 — the two OS-sandbox status-bar chips.
///
/// They are security chips, so the properties worth pinning are the ones about
/// not lying: the word matches the stored value, a setting that currently has no
/// effect says so rather than disappearing, and the tooltip promises exactly
/// what the click does.

function sandbox(over: Partial<SandboxSettings> = {}): SandboxSettings {
  return { enabled: false, tabs: false, allow_network: false, extra_grant_dirs: [], ...over };
}

describe('sandboxChipState', () => {
  it('wears the stored value as its word, both ways', () => {
    expect(sandboxChipState(sandbox({ enabled: true }))).toMatchObject({
      label: 'on',
      on: true,
      inert: false,
    });
    expect(sandboxChipState(sandbox({ enabled: false }))).toMatchObject({
      label: 'off',
      on: false,
      inert: false,
    });
  });

  it('promises what the click does, and where the full controls are', () => {
    expect(sandboxChipState(sandbox({ enabled: true })).title).toContain('Click to turn it off');
    expect(sandboxChipState(sandbox()).title).toContain('Click to turn it on');
    for (const s of [sandbox(), sandbox({ enabled: true })]) {
      expect(sandboxChipState(s).title).toContain('Right-click to open Settings → Sandboxing');
    }
  });

  it('names the seams it governs — not "everything"', () => {
    const title = sandboxChipState(sandbox({ enabled: true })).title;
    expect(title).toContain('run_command');
    expect(title).toContain('run_check');
    expect(title).toContain('Code Audit');
  });

  /// `sandbox.tabs` is deliberately not toggled from the status bar: confining
  /// the AI tab confines everything the agent afterwards runs, and it only takes
  /// effect at the tab's next spawn. The chip has to SAY that, or a user who
  /// turns sandboxing on here is left wondering why their tabs are unconfined.
  it('states that AI-tool tabs are a separate, restart-scoped Settings switch', () => {
    const off = sandboxChipState(sandbox({ enabled: true, tabs: false })).title;
    expect(off).toContain('AI-tool tabs are NOT sandboxed');
    expect(off).toContain('next starts');
    const on = sandboxChipState(sandbox({ enabled: true, tabs: true })).title;
    expect(on).toContain('AI-tool tabs are also sandboxed');
    expect(on).toContain('next starts');
  });
});

describe('sandboxNetworkChipState', () => {
  it('wears its own stored value, independent of the master', () => {
    expect(sandboxNetworkChipState(sandbox({ allow_network: true }))).toMatchObject({
      label: 'on',
      on: true,
    });
    expect(sandboxNetworkChipState(sandbox({ allow_network: false }))).toMatchObject({
      label: 'off',
      on: false,
    });
  });

  /// The stored value is SHOWN while sandboxing is off, flagged inert. Hiding it
  /// would read as "there is no such setting", and would also hide a stored `on`
  /// that takes effect the moment sandboxing is switched on.
  it('shows a value that has no effect yet, and says it has none', () => {
    const dark = sandboxNetworkChipState(sandbox({ enabled: false, allow_network: true }));
    expect(dark).toMatchObject({ label: 'on', on: true, inert: true });
    expect(dark.title).toContain('no effect while sandboxing itself is off');

    const live = sandboxNetworkChipState(sandbox({ enabled: true, allow_network: true }));
    expect(live.inert).toBe(false);
    expect(live.title).not.toContain('no effect while sandboxing itself is off');
  });

  /// The two facts a user cannot guess from the word "network": it is the tool
  /// seams only (a sandboxed AI tab always has egress), and the capability is
  /// all-or-nothing — it opens the LAN as well as the internet.
  it('states the scope and the breadth the Rust field documents', () => {
    const title = sandboxNetworkChipState(sandbox({ enabled: true })).title;
    expect(title).toContain('sandboxed tool processes only');
    expect(title).toContain('a sandboxed AI tab always has network access');
    expect(title).toContain('all-or-nothing');
    expect(title).toContain('local network as well as the internet');
  });
});

describe('withSandbox', () => {
  it('clones rather than mutating, and moves only the field it was given', () => {
    const before = defaultSettings();
    const snapshot = JSON.stringify(before);
    const after = withSandbox(before, { enabled: !before.sandbox.enabled });
    expect(JSON.stringify(before)).toBe(snapshot);
    expect(after).not.toBe(before);
    expect(after.sandbox.enabled).toBe(!before.sandbox.enabled);
    expect(after.sandbox.tabs).toBe(before.sandbox.tabs);
    expect(after.sandbox.allow_network).toBe(before.sandbox.allow_network);
    expect(after.sandbox.extra_grant_dirs).toEqual(before.sandbox.extra_grant_dirs);
    // Nothing outside `sandbox` moved.
    expect(after.offload).toBe(before.offload);
  });

  it('never touches `tabs` on the path the status bar uses', () => {
    const s = withSandbox(defaultSettings(), { allow_network: true });
    expect(s.sandbox.tabs).toBe(false);
  });
});
