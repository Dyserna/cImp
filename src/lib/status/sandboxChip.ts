/// V39 — the two OS-sandbox status-bar chips, as VALUES.
///
/// The chips live beside the injection shield in the status bar's security
/// section and follow the same grammar: a glyph, the word `on` or `off`, a
/// colour that says which, and a click that flips the setting through one
/// full-object `applySettings` write.
///
/// The derivation lives here rather than in the components for the reason
/// `latch.ts`'s `injectionChipState` gives: a `.svelte` file has no test harness
/// in this repo, and a security chip's whole job is to not lie about the state
/// it is reporting.
///
/// **These read `settings.sandbox`, which is the V33 layer** — OS-level
/// confinement of the processes cImp starts on the agent's behalf. It is a
/// different mechanism from injection protection (which is about what content
/// may reach a model and what a latched session may call), which is why they are
/// separate chips rather than one "security: on" light.
import type { SandboxSettings, Settings } from '../settings/types';

/// One binary chip.
export interface ToggleChipState {
  /// The word the chip wears.
  label: 'on' | 'off';
  /// The stored value — what a click inverts.
  on: boolean;
  /// The stored value is real and shown, but currently has no effect: the
  /// switch above it is off.
  ///
  /// Rendered dimmed, never hidden. A hidden control reads as "there is no such
  /// setting", which sends the user looking for one that is right there — and it
  /// would also hide a stored `on` that will take effect the moment sandboxing
  /// is switched on, which is exactly the surprise this chip exists to prevent.
  inert: boolean;
  title: string;
}

/// The sandbox master chip: `sandbox.enabled`.
///
/// What it governs, in the words the tooltip uses: the tool seams cImp runs on
/// the agent's behalf — `run_command`, `run_check`, and the Code Audit scanners.
///
/// It deliberately does **not** offer `sandbox.tabs`. That flag confines the AI
/// tab itself, so everything the agent afterwards runs is confined with it
/// (including a `git push` whose credential helper can no longer read the user's
/// store), and it only takes effect at tab spawn. A one-click status-bar toggle
/// is the wrong shape for a change that big and that deferred: it stays a
/// Settings control, and the tooltip says so rather than leaving the user to
/// wonder why their tabs are not confined.
export function sandboxChipState(s: SandboxSettings): ToggleChipState {
  const scope =
    'OS sandboxing confines the processes cImp starts for the agent — run_command, run_check and the Code Audit scanners.';
  const tabsNote = s.tabs
    ? 'AI-tool tabs are also sandboxed (Settings → Sandboxing); that widening only applies to a tab when it next starts.'
    : 'AI-tool tabs are NOT sandboxed — that is a separate Settings → Sandboxing switch, and it takes effect only when a tab next starts.';
  const hint = 'Right-click to open Settings → Sandboxing.';
  return {
    label: s.enabled ? 'on' : 'off',
    on: s.enabled,
    inert: false,
    title: s.enabled
      ? `Sandboxing is ON. ${scope} Click to turn it off. ${tabsNote} ${hint}`
      : `Sandboxing is OFF — those processes run with your full user rights. ${scope} Click to turn it on. ${tabsNote} ${hint}`,
  };
}

/// The sandbox network chip: `sandbox.allow_network`.
///
/// Semantics taken from the Rust field rather than guessed: it grants the
/// `internetClient` capability to sandboxed children, it is all-or-nothing (the
/// capability opens the LAN as well as the internet — per-host scoping is
/// unbuilt WFP work), and it governs **the tool seams only**. A sandboxed AI tab
/// always gets network access, because an AI CLI that cannot reach its own model
/// endpoint is a bricked tab rather than a hardened one.
///
/// When sandboxing is off the chip shows the stored value and says it is inert —
/// see [`ToggleChipState.inert`].
export function sandboxNetworkChipState(s: SandboxSettings): ToggleChipState {
  const scope =
    'Applies to sandboxed tool processes only (run_command / run_check / audit scanners); a sandboxed AI tab always has network access.';
  const breadth =
    'It is all-or-nothing: the capability opens the local network as well as the internet.';
  const hint = 'Right-click to open Settings → Sandboxing.';
  const inert = !s.enabled;
  const inertNote = inert
    ? ' It has no effect while sandboxing itself is off — this is the value that will apply once you turn sandboxing on.'
    : '';
  return {
    label: s.allow_network ? 'on' : 'off',
    on: s.allow_network,
    inert,
    // The breadth is stated in BOTH directions, deliberately: the user who most
    // needs to know that this capability opens the LAN too is the one about to
    // switch it on.
    title: s.allow_network
      ? `Sandbox network access is ON. ${scope} ${breadth} Click to turn it off.${inertNote} ${hint}`
      : `Sandbox network access is OFF — sandboxed tool processes cannot reach the network. ${scope} ${breadth} Click to turn it on.${inertNote} ${hint}`,
  };
}

/// Clone `current` with one `sandbox` field changed.
///
/// Clone-and-patch, never a mutation of the store's object — the same discipline
/// `latch.ts`'s `withTabInjectionOverrides` follows, and for the same reason:
/// `applySettings` rolls back to the previous object when the backend rejects
/// the write, which only works if that object was never edited in place.
export function withSandbox(
  current: Settings,
  changes: Partial<SandboxSettings>,
): Settings {
  return { ...current, sandbox: { ...current.sandbox, ...changes } };
}
