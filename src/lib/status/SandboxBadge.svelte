<script lang="ts">
  // V39 — the OS-sandbox chips in the status bar's security section.
  //
  // Two chips from one component, because they are the same control twice over
  // and splitting them into two files would give the same three lines of markup
  // two places to drift:
  //   * `kind="sandbox"`  → `sandbox.enabled`, the V33 master for confining the
  //     processes cImp starts for the agent (run_command, run_check, the audit
  //     scanners);
  //   * `kind="network"`  → `sandbox.allow_network`, whether those confined
  //     processes get the `internetClient` capability.
  //
  // What is deliberately NOT here: `sandbox.tabs`. Confining the AI tab itself
  // confines everything the agent afterwards runs — including a `git push` whose
  // credential helper can no longer read the user's store — and it only takes
  // effect at the tab's next spawn. A one-click status-bar toggle is the wrong
  // shape for a change that large and that deferred; it stays a Settings control
  // and both tooltips say so.
  //
  // The network chip stays VISIBLE when sandboxing is off, dimmed, showing its
  // stored value. Hiding it would read as "there is no such setting" and would
  // also hide a stored `on` that takes effect the moment sandboxing is switched
  // on — the surprise this chip exists to prevent.
  //
  // Every word and class it wears comes from `sandboxChip.ts`, for the reason
  // `InjectionBadge` defers to `latch.ts`: a `.svelte` file has no test harness
  // in this repo, and a security chip's job is to not lie.
  import { openSettingsWindowToSection } from '../settings/ipc';
  import { applySettings, settings } from '../settings/store';
  import { sandboxChipState, sandboxNetworkChipState, withSandbox } from './sandboxChip';

  let { kind }: { kind: 'sandbox' | 'network' } = $props();

  const chip = $derived(
    kind === 'sandbox'
      ? sandboxChipState($settings.sandbox)
      : sandboxNetworkChipState($settings.sandbox),
  );

  /// ▣ for the sandbox itself (a box around a thing) and ⇅ for its network
  /// capability. Neither may be ⛨ (the injection shield beside it) or 🌐 (the
  /// rustnet launcher further along the same bar).
  const glyph = $derived(kind === 'sandbox' ? '▣' : '⇅');

  function flip(): void {
    const s = $settings;
    void applySettings(
      withSandbox(s, kind === 'sandbox' ? { enabled: !s.sandbox.enabled } : { allow_network: !s.sandbox.allow_network }),
    );
  }

  function onContext(e: MouseEvent): void {
    e.preventDefault();
    void openSettingsWindowToSection('sandboxing');
  }
</script>

<button
  type="button"
  class="status-button status-badge sandbox"
  class:sandbox-on={chip.on}
  class:sandbox-off={!chip.on}
  class:inert={chip.inert}
  onclick={flip}
  oncontextmenu={onContext}
  title={chip.title}
  aria-label={chip.title}
  aria-pressed={chip.on}
>
  <span class="glyph" aria-hidden="true">{glyph}</span>
  <span class="text">{chip.label}</span>
</button>

<style>
  /* Same grammar as `InjectionBadge`, and since #128 literally the same rules:
     both badges wear `.status-button.status-badge` from `src/app.css`. The
     older note here said the duplication was deliberate because the
     status-bar-wide `.status-button` had "slightly different padding" — that
     difference is exactly what the `.status-badge` modifier now names.

     Shell + focus ring therefore live in `src/app.css`. The colour stays here
     because it is the badge's meaning, not its shape. */
  .status-button {
    color: var(--awaiting);
  }
  .sandbox-on {
    color: var(--success);
  }
  /* Muted rather than danger-coloured: sandboxing ships off, so "off" is the
     baseline the user has not moved away from — not a protection they lost. */
  .sandbox-off {
    color: var(--text-tertiary);
  }
  /* A stored value with nothing to apply to. Shown, never hidden. */
  .inert {
    opacity: 0.55;
    font-style: italic;
  }
  .status-button:hover {
    background: var(--surface-3);
  }
  .glyph {
    font-size: 12px;
  }
</style>
