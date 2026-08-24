<script lang="ts">
  /// Settings → Appearance (#129 (c)) — UI theme, terminal palette, the
  /// background editor and the background presets.
  ///
  /// Everything here is a pure function of `snapshot` plus a write through
  /// `patch()`, so the whole cluster moved: the preset save/manage panels
  /// (V1.4-04 B.5, inline rather than modal to match the window's flow), the
  /// active-tab metadata behind "Apply to global", and `pairedPalette`. None of
  /// it was read anywhere else — the only reason it lived in `SettingsApp` is
  /// that the section did.
  ///
  /// Note what did NOT come along: `themeWaveformColor`, which reads
  /// `getComputedStyle` after a theme change, belongs to the Avatar section and
  /// stays there.
  import { themeRegistry, paletteRegistry } from '../../themes/registry';
  import { resolveBundledTheme, defaultPalette } from '../../themes';
  import {
    TUI_THEME_ID,
    TUI_ACCENT_PRESETS,
    normalizeTuiAccent,
    normalizeHexColor,
    DEFAULT_LATCHED_COLOR,
    DEFAULT_CONTAMINATED_COLOR,
  } from '../../themes/accent';
  import {
    asThemedTabConfig,
    toPresetConfig,
    type Settings,
    type ThemeColorsWire,
  } from '../types';
  import BackgroundConfigEditor from '../BackgroundConfigEditor.svelte';
  import CustomThemeEditor from '../CustomThemeEditor.svelte';
  import NumberField from '../NumberField.svelte';
  import SelectField from '../SelectField.svelte';
  import ThemeSwatch from '../ThemeSwatch.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
  } = $props();

  /// Terminal palette paired with a UI chrome theme: each theme's metadata
  /// carries its default palette. Selecting a UI theme re-points the terminal
  /// palette to its pairing (a manual palette pick afterward sticks until the
  /// next theme switch). An unknown theme leaves the palette untouched.
  function pairedPalette(themeId: string): string | undefined {
    return $themeRegistry.find((t) => t.id === themeId)?.palette;
  }

  // V1.4-04 B.5: inline UI for save/manage presets. Implemented as
  // toggleable inline panels rather than modal dialogs to match the
  // SettingsApp's existing flow (no <dialog> elements elsewhere).
  let savingPreset = $state(false);
  let newPresetName = $state('');
  let savePresetError = $state<string | null>(null);
  let managingPresets = $state(false);

  function startSavePreset() {
    savingPreset = true;
    newPresetName = '';
    savePresetError = null;
  }

  function cancelSavePreset() {
    savingPreset = false;
    newPresetName = '';
    savePresetError = null;
  }

  function commitSavePreset() {
    const name = newPresetName.trim();
    if (!name) {
      savePresetError = 'Name required.';
      return;
    }
    if (snapshot.terminal.background.presets.some((p) => p.name === name)) {
      savePresetError = `A preset named "${name}" already exists.`;
      return;
    }
    patch((s) => {
      const cfg = toPresetConfig(s.terminal.background);
      s.terminal.background.presets = [
        ...s.terminal.background.presets,
        { name, config: cfg },
      ];
    });
    savingPreset = false;
    newPresetName = '';
    savePresetError = null;
  }

  function deletePreset(name: string) {
    patch((s) => {
      s.terminal.background.presets = s.terminal.background.presets.filter(
        (p) => p.name !== name,
      );
    });
  }

  function renamePreset(oldName: string, nextName: string) {
    const trimmed = nextName.trim();
    if (!trimmed || trimmed === oldName) return;
    if (snapshot.terminal.background.presets.some((p) => p.name === trimmed)) {
      // Silent reject — duplicate. The input value reverts on next
      // store flush via the {#each} key change.
      return;
    }
    patch((s) => {
      const idx = s.terminal.background.presets.findIndex(
        (p) => p.name === oldName,
      );
      if (idx < 0) return;
      s.terminal.background.presets[idx].name = trimmed;
    });
  }

  /// Active-tab metadata, used by the "Apply to global" button. The session's
  /// `active_tab_id` is the canonical "currently focused tab" reference at the
  /// settings layer; if nothing has set it yet (fresh install before any tab
  /// focus), fall back to the first tab so the action remains useful.
  const activeTabId = $derived(
    snapshot.session.active_tab_id ?? snapshot.tabs[0]?.id ?? null,
  );
  const activeTab = $derived(
    activeTabId
      ? asThemedTabConfig(snapshot.tabs.find((t) => t.id === activeTabId)) ?? null
      : null,
  );
  const activeTabHasOverrides = $derived(
    activeTab !== null &&
      (activeTab.theme_override !== null ||
        (activeTab.background_override !== null &&
          activeTab.background_override !== 'disabled')),
  );

  /// Promote the active tab's terminal palette + background overrides
  /// to the global terminal settings, then clear overrides on every
  /// tab so all tabs inherit the new global. The 'disabled' literal
  /// in `background_override` is an opt-out, not a config, so it does
  /// not get promoted.
  function applyActiveTabOverridesToGlobal() {
    patch((s) => {
      const id = s.session.active_tab_id ?? s.tabs[0]?.id;
      if (!id) return;
      const src = asThemedTabConfig(s.tabs.find((t) => t.id === id));
      if (!src) return;
      if (src.theme_override) {
        s.terminal.theme = src.theme_override;
      }
      if (
        src.background_override !== null &&
        src.background_override !== 'disabled'
      ) {
        s.terminal.background = {
          ...s.terminal.background,
          ...src.background_override,
        };
      }
      // Preview tabs carry neither field — nothing to clear on them.
      for (const t of s.tabs) {
        if (t.kind === 'preview') continue;
        t.theme_override = null;
        t.background_override = null;
      }
    });
  }
</script>

<section>
  <h2>Theme</h2>

  <h3>UI theme</h3>
  <small class="hint top">
    Governs the cImp chrome — tab bar, status bar, dialogs.
    Distinct from the terminal palette below.
  </small>
  <SelectField
    label="Theme"
    value={snapshot.ui.theme}
    onchange={(next) => {
      const theme = next;
      patch((s) => {
        s.ui.theme = theme;
        // Pair the terminal palette to the chosen theme. Skipped for
        // a user "Custom" palette so a hand-tuned palette isn't lost
        // on a theme switch.
        const paired = pairedPalette(theme);
        if (paired && s.terminal.theme.name !== 'Custom') {
          s.terminal.theme.name = paired;
          s.terminal.theme.custom = null;
        }
      });
    }}
  >
    {#each $themeRegistry as t}
      <option value={t.id}>{t.name}</option>
    {/each}
  </SelectField>

  {#if snapshot.ui.theme === TUI_THEME_ID}
    <!-- Accent picker — TUI-only: the built-in theme derives its whole
         accent family from this one color; disk themes carry their own
         fixed accents, so the control hides for them. -->
    <div class="accent-row">
      <span>Accent color</span>
      <div class="accent-controls">
        {#each TUI_ACCENT_PRESETS as p}
          <button
            type="button"
            class="icon accent-swatch"
            class:selected={normalizeTuiAccent(snapshot.ui.tui_accent) === p.color}
            style:background={p.color}
            title={p.name}
            aria-label={`Accent: ${p.name}`}
            onclick={() => patch((s) => (s.ui.tui_accent = p.color))}
          ></button>
        {/each}
        <input
          type="color"
          aria-label="Custom accent color"
          value={normalizeTuiAccent(snapshot.ui.tui_accent)}
          oninput={(e) => {
            const color = (e.currentTarget as HTMLInputElement).value;
            patch((s) => (s.ui.tui_accent = color));
          }}
        />
      </div>
    </div>
    <!-- `top`: this hint follows the swatch row, not a label — the
         default hint's -8px pull-up would drag it into the swatches. -->
    <small class="hint top">
      Tints buttons, borders, tabs, and the waveform. Presets match
      the four classic TUI accents; the swatch on the right picks
      anything.
    </small>
  {/if}

  <!-- V32 containment colors. Theme-independent (unlike the TUI
       accent above): the taint badge and pane frame render under
       every theme, so their colors are always editable. Two states,
       two colors — matching the badge's own distinction, where
       contamination outlives the latch and wears the stronger one. -->
  <h3>Containment colors</h3>
  <small class="hint top">
    Worn by a tab's ⛨ shield badge and drawn as a frame around that
    tab's content while containment applies — so a latched or
    contaminated tab is visible without reading the tab strip.
  </small>
  <div class="accent-row">
    <span>Latched session</span>
    <div class="accent-controls">
      <input
        type="color"
        aria-label="Latched tab color"
        title="The session used a gated tool (web/external or local), so the opposite tool family is closed for it"
        value={normalizeHexColor(snapshot.ui.latched_color, DEFAULT_LATCHED_COLOR)}
        oninput={(e) => {
          const color = (e.currentTarget as HTMLInputElement).value;
          patch((s) => (s.ui.latched_color = color));
        }}
      />
      <button
        type="button"
        class="secondary"
        disabled={normalizeHexColor(snapshot.ui.latched_color, DEFAULT_LATCHED_COLOR) ===
          DEFAULT_LATCHED_COLOR}
        onclick={() => patch((s) => (s.ui.latched_color = DEFAULT_LATCHED_COLOR))}
        >Reset</button
      >
    </div>
  </div>
  <div class="accent-row">
    <span>Contaminated session</span>
    <div class="accent-controls">
      <input
        type="color"
        aria-label="Contaminated tab color"
        title="External content entered the conversation — the stronger state; it outlives the latch"
        value={normalizeHexColor(
          snapshot.ui.contaminated_color,
          DEFAULT_CONTAMINATED_COLOR,
        )}
        oninput={(e) => {
          const color = (e.currentTarget as HTMLInputElement).value;
          patch((s) => (s.ui.contaminated_color = color));
        }}
      />
      <button
        type="button"
        class="secondary"
        disabled={normalizeHexColor(
          snapshot.ui.contaminated_color,
          DEFAULT_CONTAMINATED_COLOR,
        ) === DEFAULT_CONTAMINATED_COLOR}
        onclick={() =>
          patch((s) => (s.ui.contaminated_color = DEFAULT_CONTAMINATED_COLOR))}
        >Reset</button
      >
    </div>
  </div>

  <h3>Terminal palette</h3>
  <small class="hint top">
    Colors used inside terminal tabs. Each tab can override this in
    its Configure dialog.
  </small>
  <label class="palette-row">
    <span>Palette</span>
    <select
      value={snapshot.terminal.theme.name}
      onchange={(e) => {
        const name = (e.currentTarget as HTMLSelectElement).value;
        patch((s) => {
          // Read the previous name from `s` itself — `patch`'s
          // working copy holds the pre-update value at entry,
          // which is what we want for seeding.
          const previousName = s.terminal.theme.name;
          s.terminal.theme.name = name;
          if (name === 'Custom') {
            if (!s.terminal.theme.custom) {
              const seed =
                previousName === 'Custom'
                  ? defaultPalette()
                  : resolveBundledTheme(previousName);
              s.terminal.theme.custom = { ...seed } as ThemeColorsWire;
            }
          } else {
            s.terminal.theme.custom = null;
          }
        });
      }}
    >
      {#each $paletteRegistry as p}
        <option value={p.name}>{p.name}</option>
      {/each}
      <option value="Custom">Custom…</option>
    </select>
    <ThemeSwatch
      name={snapshot.terminal.theme.name}
      custom={snapshot.terminal.theme.custom}
    />
  </label>

  {#if snapshot.terminal.theme.name === 'Custom' && snapshot.terminal.theme.custom}
    <CustomThemeEditor
      value={snapshot.terminal.theme.custom}
      onchange={(next) =>
        patch((s) => {
          s.terminal.theme.custom = next;
        })}
    />
  {/if}
</section>
<section>
  <h2>Terminal background</h2>
  <small class="hint top">
    Image, color, and gradient options applied behind every
    terminal tab. Per-tab overrides live in each tab's Configure
    dialog.
  </small>

  <BackgroundConfigEditor
    bind:config={
      () => snapshot.terminal.background,
      (v) =>
        patch((s) => {
          s.terminal.background = v;
        })
    }
  />

  <h3>Presets</h3>
  <div class="preset-actions">
    <button type="button" onclick={startSavePreset}>Save as preset…</button>
    <button
      type="button"
      onclick={() => (managingPresets = !managingPresets)}
    >
      {managingPresets ? 'Done managing' : 'Manage presets…'}
    </button>
  </div>

  {#if savingPreset}
    <div class="preset-save">
      <input
        type="text"
        placeholder="Preset name"
        bind:value={newPresetName}
        onkeydown={(e) => {
          if (e.key === 'Enter') commitSavePreset();
          if (e.key === 'Escape') cancelSavePreset();
        }}
      />
      <button type="button" onclick={commitSavePreset}>Save</button>
      <button type="button" onclick={cancelSavePreset}>Cancel</button>
      {#if savePresetError}
        <small class="error">{savePresetError}</small>
      {/if}
    </div>
    <small class="hint">
      Presets reference image paths by absolute location — moving an
      image file breaks any preset that uses it.
    </small>
  {/if}

  {#if managingPresets}
    {#if snapshot.terminal.background.presets.length === 0}
      <small class="hint">No presets saved yet.</small>
    {:else}
      <ul class="preset-list">
        {#each snapshot.terminal.background.presets as p (p.name)}
          <li>
            <input
              type="text"
              value={p.name}
              onchange={(e) =>
                renamePreset(
                  p.name,
                  (e.currentTarget as HTMLInputElement).value,
                )}
            />
            <button type="button" onclick={() => deletePreset(p.name)}>
              Delete
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  {/if}

  <h3>Preview</h3>
  <Toggle
    label="Preview image / category changes in Configure Tab dialog"
    checked={snapshot.terminal.background.preview_category_flips}
    onchange={(next) => patch((s) => (s.terminal.background.preview_category_flips = next))}
  />
  <small class="hint">
    When off, image-toggle and category-flip changes wait for Save in
    the Configure Tab dialog. Color, opacity, blur, size, position,
    and tint always preview live.
  </small>
  <NumberField
    label="Scrollback kept across renderer switches (lines)"
    min="0"
    value={snapshot.terminal.background.snapshot_lines}
    onchange={(next) =>
      patch((s) => {
        const n = Number(next);
        s.terminal.background.snapshot_lines = Number.isFinite(n)
          ? Math.max(0, Math.floor(n))
          : 2000;
      })}
  />
  <small class="hint">
    Rows re-painted when a background change switches the terminal
    renderer (WebGL ↔ DOM). Higher keeps more history through the
    flip at the cost of a bigger in-memory snapshot.
  </small>
</section>
<section>
  <h2>Display</h2>
  <label>
    <span>Terminal font family</span>
    <input
      type="text"
      value={snapshot.display.terminal_font_family}
      onchange={(e) =>
        patch((s) => (s.display.terminal_font_family = (e.currentTarget as HTMLInputElement).value))}
    />
  </label>
  <NumberField
    label="Terminal font size (px)"
    min="8"
    max="48"
    value={snapshot.display.terminal_font_size}
    onchange={(next) =>
      patch((s) => (s.display.terminal_font_size = Math.max(8, +next)))}
  />
</section>

<section>
  <h2>Compose</h2>
  <small class="hint top">
    Sizing of the multi-line compose box that opens for prompts.
  </small>
  <div class="row">
    <NumberField
      label="Min height (px)"
      min="40"
      max="400"
      value={snapshot.compose.min_height_px}
      onchange={(next) =>
        patch((s) => (s.compose.min_height_px = Math.max(40, +next)))}
    />
    <NumberField
      label="Max height (px)"
      min="60"
      max="800"
      value={snapshot.compose.max_height_px}
      onchange={(next) =>
        patch((s) => (s.compose.max_height_px = Math.max(60, +next)))}
    />
  </div>
</section>

<section>
  <h2>Per-tab overrides</h2>
  <small class="hint top">
    Promote the active tab's terminal palette and background
    overrides to the global defaults, then clear the overrides
    on every tab so they inherit the new global. Useful after
    dialing in one tab and wanting the rest to match.
  </small>
  <button
    type="button"
    class="promote-overrides"
    onclick={applyActiveTabOverridesToGlobal}
    disabled={!activeTabHasOverrides}
  >
    {#if activeTab && activeTabHasOverrides}
      Apply "{activeTab.name}" overrides to global
    {:else if activeTab}
      No overrides on "{activeTab.name}" to promote
    {:else}
      No active tab
    {/if}
  </button>
</section>

<style>
  .preset-actions {
    display: flex;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }
  .preset-save {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    margin-bottom: var(--space-2);
  }
  .preset-save input[type='text'] {
    flex: 1;
  }
  .preset-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .preset-list li {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .preset-list li input[type='text'] {
    flex: 1;
  }
  .palette-row {
    display: grid;
    grid-template-columns: 1fr auto;
    grid-column-gap: var(--space-2);
    align-items: end;
  }
  .palette-row > span:first-child {
    grid-column: 1 / -1;
  }
  .accent-row {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }
  .accent-controls {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  /* Preset swatches are icon-class buttons (no TUI bracket framing) painted
     in their accent color; the active one gets a bordered frame. */
  .accent-swatch {
    width: 22px;
    height: 22px;
    padding: 0;
    border: 1px solid var(--border-default);
    cursor: pointer;
  }
  .accent-swatch.selected {
    outline: 1px solid var(--text-bright);
    outline-offset: 1px;
  }
  .accent-controls input[type='color'] {
    width: 44px;
    height: 22px;
    padding: 0;
    border: 1px solid var(--border-default);
    background: transparent;
    cursor: pointer;
  }
</style>
