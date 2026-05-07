# Milestone V1.4-02: Terminal Background — Image and Solid Color (Skeleton)

## Purpose

Item 2 from `FEATURE-per-tab-overrides.md`, extended. A user-supplied image OR a custom solid color displayed beneath terminal text, with opacity / blur / size controls applying to the image variant. Schema and resolver land complete; the per-tab override **UI** is staged to a follow-on. Read V1.4-01 first — V1.4-02 follows the same milestone shape (schema → resolver → wiring → migration → UI).

The solid-color path is the cheap mode: it sets the resolved theme's `background` field and uses xterm.js's default canvas renderer. The image path is the expensive mode: `allowTransparency: true`, DOM renderer (2-5× slower for high-throughput output), and CSS background styling on the host. The two modes are mutually exclusive at the rendering layer; the schema lets a config carry both because the color doubles as the image's dimming-overlay tint when both are present.

## What This Milestone Delivers

1. New `terminal.background` settings group:
   ```
   image:    Option<PathBuf>     // file path, None = no image
   color:    Option<String>      // hex like "#1a2b3c", None = use theme bg
   opacity:  f32                 // 0.0-1.0, applies only when image is set (default 0.4)
   blur:     u32                 // px, applies only when image is set (default 0)
   size:     "cover" | "contain" | "tile"   (default "cover")
   position: String              // CSS background-position (default "center")
   ```
2. `background_override: Option<BackgroundOverride>` on both tab variants, where `BackgroundOverride = Disabled | Custom(BackgroundConfig)` (`"disabled"` string or full config object on the wire — three-state per the feature doc). The override carries both `image` and `color` because they're sibling fields in the same struct.
3. `effectiveBackground(tab)` resolver returning either a `BackgroundConfig` or `null`. The resolver also distinguishes the rendering path the caller should take (color-only vs. image vs. both vs. neither) — see the four-state matrix in Key Deltas.
4. Three rendering paths in `terminals.ts`, picked at Terminal-construction time:
   - **No override** (config null OR `image: None, color: None`): canvas renderer, theme background unchanged. Today's behavior.
   - **Color only** (`image: None, color: Some(c)`): canvas renderer, the resolved theme's `background` is rewritten to `c` before passing to xterm.js. **No `allowTransparency`, no perf cost** — this is the headline benefit of separating color from image.
   - **Image present** (`image: Some(...)`, `color` optional): `allowTransparency: true`, DOM renderer. Theme `background` set to `rgba(<color or 0,0,0>, opacity)` so the dimming overlay tints either to the user's color or to neutral black. CSS image styles applied to host.
5. Settings file migration v1.4 → v1.5: writes default `terminal.background` group (all fields at their defaults — no image, no color, no override) and stamps `background_override: null` on every existing tab. Backup `config.json.v1.4.bak.<ts>`.
6. **Global UI** (Settings → Appearance): a small mode toggle "**Theme default** / **Solid color** / **Image**" controls which of `image` / `color` is set. Solid color reveals a single color picker. Image reveals the file picker plus opacity / blur / size / position controls, with a secondary "Tint color" picker (optional, defaults to black) that drives the dimming overlay.
7. **Per-tab UI**: deferred to a follow-on release per the feature doc. The schema and resolver fully support `null` / `"disabled"` / explicit override states from day one — only the Configure Tab UI rows are absent.
8. README adds a "Terminal background" subsection that calls out the renderer trade-off explicitly: setting an **image** forces the slower DOM renderer for that terminal (2-5× slower for high-throughput output like `tail -F`); a **solid color** has no perf impact. Changing the image setting mid-session resets that tab's scrollback (the PTY survives, the xterm.js frontend is recreated). Changing only the **color** does not — color updates apply in place via `term.options.theme = next`.

## Key Deltas vs V1.4-01 (Themes)

- **Renderer switch is the headline risk — but only for the image path.** Themes are pure data passed to xterm.js; an image background changes *how* xterm.js renders. Construction-time decision: image-bearing configs use the DOM renderer; everything else (no override, color-only override) stays on canvas. Toggling image on/off triggers full Terminal recreation (destroy + recreate via the existing portal flow); toggling color or opacity only does not. Document the scrollback-loss-on-image-toggle caveat.
- **Four-state rendering matrix** (`image` × `color`, each Some/None):

  | image | color | renderer | how applied |
  |-------|-------|----------|-------------|
  | None  | None  | canvas   | theme bg unchanged (today's behavior) |
  | None  | Some  | canvas   | theme bg rewritten to color, no transparency |
  | Some  | None  | DOM      | theme bg = rgba(0,0,0,opacity); image on host |
  | Some  | Some  | DOM      | theme bg = rgba(color,opacity); image on host |

  This is the *real* design surface of V1.4-02. Each cell's behavior is testable in isolation.

- **Three-state override** (`null` / `"disabled"` / `BackgroundConfig`) — themes only have two states. The `"disabled"` literal is needed because users want "global image, but plain on aider where I'm reading diffs." `"disabled"` means "use theme background entirely — ignore both global image AND global color." Encode as either a JSON string `"disabled"` or an object — serde's `untagged` enum with a small custom deserializer handles this.
- **Color-vs-theme interaction.** The custom color *replaces* the resolved theme's background field — it does not blend. A user who picks Solarized Light + custom navy bg gets Solarized's foreground colors over navy, not Solarized's bg tinted toward navy. This is the right semantics (predictable; the user explicitly chose the color) but worth a one-liner in the Settings UI: "Overrides the theme's background color."
- **Image storage is by absolute path.** No copy-into-data-dir. Invalid paths surface a Settings error and resolve to `image: None` for rendering (the color path, if set, still applies). Project-local settings (when `FEATURE-config-scope.md` ships) will resolve relative paths against the project root.
- **Global change cost**: with N tabs all inheriting a global image, changing the global image recreates all N Terminal instances. Changing the global color does not — it's a live update via `term.options.theme = next`, same path V1.4-01 uses. Color-only changes are cheap.
- **CSS surface is non-trivial in the image path**: the host `<div>` gets `background-image` / `background-size` / `background-position`; if `blur > 0`, wrap the cells layer in a `backdrop-filter` container; the xterm.js theme `background` is set to `rgba(<color or 0>,opacity)`. The color-only path touches none of this — just a single `term.options.theme.background = color` reassignment. Test the image path against Dracula + image, Solarized Light + image + custom tint, plus blur=0 vs blur=20.

## What This Milestone Does NOT Do

- **Per-tab Configure Tab UI**. Schema is in place; the Configure Tab dialog gains no Background row in V1.4-02. Add it in a follow-on once real-use feedback shows the global-only constraint pinches (most likely first ask: the "explicitly disable on aider" use case).
- **Animated/video backgrounds**. Static images only — performance much higher cost, use case dubious. Out of scope.
- **Scrollback replay across renderer switch**. When a tab's renderer recreates, scrollback resets. Replaying from the PTY frontend buffer is a separate, larger feature.
- **Project-local relative-path resolution**. The schema accepts the absolute path string today; relative resolution is `FEATURE-config-scope.md`'s responsibility.

## Implementation Steps

### 1. Renderer baseline — adopt `@xterm/addon-canvas` as the fast path

`package.json` ships only `@xterm/addon-fit`. In xterm.js 5.x the in-core renderer is the DOM renderer; canvas and WebGL are addons. So today's "default canvas" framing is aspirational, not literal — the project is currently on DOM for every terminal regardless of background config.

This milestone formalises the fast/slow split. Add `@xterm/addon-canvas` (`^0.7.0`) and load it at construction for every Terminal whose effective rendering mode is **not** image. The image path remains on the in-core DOM renderer so `allowTransparency: true` and CSS layering compose correctly.

```ts
import { CanvasAddon } from '@xterm/addon-canvas';
// inside createTerminal:
if (mode.kind !== 'image') {
  term.loadAddon(new CanvasAddon());
}
```

The addon is loaded once per Terminal instance — switching paths means recreating the Terminal (Step 5).

### 2. Settings schema — `TerminalBackgroundSettings` + three-state `BackgroundOverride`

`src-tauri/src/settings/schema.rs`:

```rust
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct TerminalSettings {
    pub theme: TerminalThemeSettings,
    /// V1.4-02: image and/or solid-color background, with opacity/blur/size
    /// controls applying only when an image is set. See the four-state
    /// rendering matrix in MILESTONE-V1.4-02 for resolution semantics.
    pub background: TerminalBackgroundSettings,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TerminalBackgroundSettings {
    pub image: Option<PathBuf>,
    pub color: Option<String>,
    pub opacity: f32,
    pub blur: u32,
    pub size: BackgroundSize,
    pub position: String,
}

impl Default for TerminalBackgroundSettings {
    fn default() -> Self {
        Self {
            image: None,
            color: None,
            opacity: 0.4,
            blur: 0,
            size: BackgroundSize::Cover,
            position: "center".to_string(),
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundSize { Cover, Contain, Tile }

impl Default for BackgroundSize {
    fn default() -> Self { Self::Cover }
}
```

The three-state `BackgroundOverride` is a custom (de)serializer — `serde(untagged)` alone can't represent the literal-string `"disabled"` cleanly:

```rust
#[derive(Clone, Debug)]
pub enum BackgroundOverride {
    Disabled,
    Custom(TerminalBackgroundSettings),
}

impl Serialize for BackgroundOverride {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Disabled => s.serialize_str("disabled"),
            Self::Custom(c) => c.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for BackgroundOverride {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(ref s) if s == "disabled" => Ok(Self::Disabled),
            serde_json::Value::Object(_) => serde_json::from_value(v)
                .map(Self::Custom)
                .map_err(D::Error::custom),
            _ => Err(D::Error::custom(
                "background_override: expected \"disabled\" string or object",
            )),
        }
    }
}
```

Add to both tab variants:

```rust
pub background_override: Option<BackgroundOverride>,  // None | Some(Disabled) | Some(Custom)
```

`Default` impls and the three `default_*_tab()` constructors gain `background_override: None`.

Round-trip unit tests at `schema.rs#tests`:
- `null` round-trips to `None`.
- `"disabled"` round-trips to `Some(Disabled)`.
- Full object round-trips to `Some(Custom(...))` with field equality.
- Garbage (`42`, `[]`, `"random"`) errors with the explicit message.

### 3. TS settings types — mirror schema

`src/lib/settings/types.ts`:

```ts
export interface TerminalBackgroundSettingsWire {
  image: string | null;
  color: string | null;
  opacity: number;
  blur: number;
  size: 'cover' | 'contain' | 'tile';
  position: string;
}

export type BackgroundOverrideWire = 'disabled' | TerminalBackgroundSettingsWire;

export interface TerminalSettings {
  theme: TerminalThemeSettings;
  background: TerminalBackgroundSettingsWire;
}

export interface AiToolTabConfigWire {
  // ...
  background_override: BackgroundOverrideWire | null;
}
export interface ShellTabConfigWire {
  // ...
  background_override: BackgroundOverrideWire | null;
}
```

Type guard for the `'disabled'` discriminator (TypeScript can't narrow string-vs-object on a structural union without one):

```ts
export function isBackgroundDisabled(o: BackgroundOverrideWire | null): o is 'disabled' {
  return o === 'disabled';
}
```

`defaultSettings()` adds `terminal.background` with the same defaults as the Rust `Default` impl.

### 4. Resolver — `effectiveBackgroundMode` returns a `RenderingMode`

`src/lib/terminal/background.ts` (new — separate from `themes/resolve.ts` because background is its own concern, and the resolver returns a discriminated mode rather than a single shape):

```ts
export type RenderingMode =
  | { kind: 'none' }
  | { kind: 'color'; color: string }
  | { kind: 'image'; cfg: TerminalBackgroundSettingsWire; tint: string | null };

export function effectiveBackgroundMode(
  tab: TabWithBackgroundOverride,
  global: TerminalBackgroundSettingsWire,
): RenderingMode {
  // Three-state override resolution.
  if (tab.background_override === 'disabled') return { kind: 'none' };
  const cfg = (tab.background_override ?? global) as TerminalBackgroundSettingsWire;

  // Four-cell matrix: image trumps color-only; both produces image-with-tint;
  // neither produces 'none'.
  if (cfg.image) return { kind: 'image', cfg, tint: cfg.color };
  if (cfg.color) return { kind: 'color', color: cfg.color };
  return { kind: 'none' };
}

export function categoryOf(mode: RenderingMode): 'fast' | 'image' {
  return mode.kind === 'image' ? 'image' : 'fast';
}
```

`categoryOf` is what the live-update subscriber compares to decide recreate-vs-in-place (Step 5b).

`composeTheme` applies the mode on top of the V1.4-01 theme — colocated in the same file:

```ts
export function composeTheme(theme: ITheme, mode: RenderingMode): ITheme {
  if (mode.kind === 'none') return theme;
  if (mode.kind === 'color') return { ...theme, background: mode.color };
  // image: theme bg becomes a translucent tint (defaults to black if no color set).
  return { ...theme, background: rgbaFrom(mode.tint ?? '#000000', mode.cfg.opacity) };
}
```

Tests in `src/lib/terminal/background.test.ts` exercise the full 4×3 matrix (4 image/color combos × 3 override states).

### 5. Wire into `terminals.ts` — renderer branch + recreate-on-toggle

Two distinct touch points: construction (`createTerminal`) and the live-update subscription (`unsubTheme`, which V1.4-02 widens into `unsubAppearance`).

**5a. At construction.** Replace the V1.4-01 theme block at `terminals.ts:201-224`:

```ts
const allSettings = get(settingsStore);
const initialTab = allSettings.tabs.find((t) => t.id === tabId);
const initialTheme = initialTab
  ? effectiveTheme(initialTab, allSettings.terminal.theme)
  : themeFromSetting(allSettings.terminal.theme);
const initialMode = initialTab
  ? effectiveBackgroundMode(initialTab, allSettings.terminal.background)
  : effectiveBackgroundMode(
      { background_override: null } as TabWithBackgroundOverride,
      allSettings.terminal.background,
    );

const term = new Terminal({
  fontFamily: display.terminal_font_family,
  fontSize: display.terminal_font_size,
  cursorBlink: true,
  allowProposedApi: true,
  theme: composeTheme(initialTheme, initialMode),
  ...(initialMode.kind === 'image' ? { allowTransparency: true } : {}),
});

if (initialMode.kind !== 'image') {
  term.loadAddon(new CanvasAddon());
}
const fitAddon = new FitAddon();
term.loadAddon(fitAddon);
term.open(host);
applyHostBackgroundCss(host, initialMode);
```

Track the initial mode's category on the entry so the live subscription can detect a category transition:

```ts
interface TerminalEntry {
  // ... existing fields ...
  bgCategory: 'fast' | 'image';
}
```

**5b. Live updates.** The current `unsubTheme` subscription (`terminals.ts:278-292`) becomes `unsubAppearance` and handles theme + background together:

```ts
let firstAppearance = true;
entry.unsubAppearance = settingsStore.subscribe((s) => {
  if (firstAppearance) { firstAppearance = false; return; }
  const tab = s.tabs.find((t) => t.id === tabId);
  const theme = tab ? effectiveTheme(tab, s.terminal.theme)
                    : themeFromSetting(s.terminal.theme);
  const mode = tab ? effectiveBackgroundMode(tab, s.terminal.background)
                   : effectiveBackgroundMode({ background_override: null }, s.terminal.background);

  if (categoryOf(mode) !== entry.bgCategory) {
    // Renderer category flipped (fast ↔ image). Recreate the Terminal.
    // The PTY survives the destroy; we restart the byte stream against
    // the new xterm via pty_restart (scrollback loss is documented).
    queueRecreate(tabId);
    return;
  }

  // Same category — apply in place.
  term.options.theme = composeTheme(theme, mode);
  applyHostBackgroundCss(host, mode);
});
```

`queueRecreate(tabId)` debounces (~120ms) so live slider drags during a global edit don't thrash. On fire it runs:

```ts
destroyTerminal(tabId);    // tears down xterm + listeners
createTerminal(tabId);     // rebuilds with the latest settings; calls pty_start
                           //   (the existing flow; pty_start internally tolerates
                           //   a still-running PTY by going through pty_restart's
                           //   shutdown path — verify at impl time)
```

**Open at impl time:** `attemptSpawn(entry, false)` calls `ptyStart`, but on recreation the PTY is still alive. Two options:
- (a) Call `attemptSpawn(entry, true)` (i.e., `ptyRestart`) on recreate — accepts scrollback loss, which the doc already documents.
- (b) Add `pty_rebind_channel(tab, channel)` Tauri command that points the running PTY's bytes at the new xterm without restarting.

Plan: ship (a). The doc's "scrollback resets when renderer recreates" caveat is the contract; (b) is a separate quality-of-life improvement that doesn't need to land in V1.4-02. Add a TODO with a `// V1.4-02-followup` marker pointing at FEATURE-per-tab-overrides.md so the option (b) path is discoverable.

**5c. Color/opacity/blur/path/size/position changes (renderer stays the same).** All in-place via `term.options.theme = composeTheme(...)` plus `applyHostBackgroundCss(host, mode)`. No recreate. The image-with-image case (path swap) is in-place because `host.style.backgroundImage` reassignment is cheap and xterm.js doesn't care. The opacity/tint case re-runs `composeTheme` because the rgba bg is a derived value.

`applyHostBackgroundCss(host, mode)`:

```ts
function applyHostBackgroundCss(host: HTMLDivElement, mode: RenderingMode): void {
  if (mode.kind !== 'image') {
    host.classList.remove('bg-image');
    host.style.removeProperty('--bg-image');
    host.style.removeProperty('--bg-size');
    host.style.removeProperty('--bg-position');
    host.style.removeProperty('--bg-blur');
    return;
  }
  host.classList.add('bg-image');
  host.style.setProperty('--bg-image', `url('${pathToFileURL(mode.cfg.image!)}')`);
  host.style.setProperty('--bg-size', cssSizeFor(mode.cfg.size));
  host.style.setProperty('--bg-position', mode.cfg.position);
  host.style.setProperty('--bg-blur', `${mode.cfg.blur}px`);
}
```

`pathToFileURL` is a small helper that produces a `file://` URL from an absolute path; on Windows the drive letter and path-separator handling matters. Tauri's `convertFileSrc` is the canonical helper if file:// turns out to be blocked by CSP — confirm at impl.

**5d. `TerminalEntry` updates and destroy path.** Rename `unsubTheme` → `unsubAppearance` at the field level. `destroyTerminal` already calls `entry.unsubTheme()`; rename the call site too. Add `bgCategory` to the entry; initialise from `categoryOf(initialMode)`.

### 6. CSS layering for image + blur

xterm.js opens directly into the host element, so a sibling-div approach forces DOM restructuring. Use a `::before` pseudo-element for the image and `backdrop-filter` on the `.xterm` cells layer instead — no DOM changes required.

`src/lib/terminals.css` (or wherever the host styling lives — likely already inline in `terminals.ts:208-214`; lift to a stylesheet for V1.4-02):

```css
.terminal-host.bg-image {
  position: relative;
}
.terminal-host.bg-image::before {
  content: '';
  position: absolute;
  inset: 0;
  background-image: var(--bg-image);
  background-size: var(--bg-size);
  background-position: var(--bg-position);
  background-repeat: no-repeat;
  z-index: 0;
}
.terminal-host.bg-image .xterm {
  position: relative;
  z-index: 1;
  backdrop-filter: blur(var(--bg-blur, 0));
}
```

`backdrop-filter` blurs *what's behind the element it's applied to*. With `.xterm` at `z-index: 1` and the `::before` at `z-index: 0`, the `.xterm` layer's backdrop is the image — exactly the visual we want. **Prototype this in isolation** before Step 5 wiring (a 30-line static HTML page with xterm 5.5 + a known image), since the blur layering is the highest-uncertainty piece of the milestone per the doc's risks. Test with blur=0 (effectively a no-op filter, should match no-image perf), blur=20 (visibly soft), and the `tile` size mode (CSS `background-repeat: repeat` instead of `no-repeat` — handle in `cssSizeFor`).

`background-size: cover | contain | tile` doesn't map 1:1 to CSS. `cover` and `contain` map directly; `tile` becomes `background-size: auto` with `background-repeat: repeat`. `cssSizeFor` returns the size value and a sibling write to `background-repeat` handles the tile case.

### 7. Migration v1.4 → v1.5

Pattern matches v1.3 → v1.4. Strict additions; nothing to remove. Backup at `config.json.v1.4.bak.<ts>`.

`src-tauri/src/settings/migration.rs`:

```rust
// In migrate_if_needed, after the v1.3 → v1.4 branch:
if looks_v1_4(value) {
    write_backup(path, "v1.4", value)?;
    migrate_v1_4_to_v1_5(value);
    changed = true;
}

fn looks_v1_4(value: &Value) -> bool {
    // Has terminal (v1.4) but lacks terminal.background (v1.5). Files
    // already at v1.5 carry both.
    let Some(obj) = value.as_object() else { return false };
    let has_terminal = obj.contains_key("terminal");
    let has_terminal_background = obj
        .get("terminal")
        .and_then(|t| t.get("background"))
        .is_some();
    has_terminal && !has_terminal_background
}

fn migrate_v1_4_to_v1_5(value: &mut Value) {
    let Some(root) = value.as_object_mut() else { return };

    if let Some(terminal) = root.get_mut("terminal").and_then(Value::as_object_mut) {
        terminal.insert("background".to_string(), json!({
            "image": null,
            "color": null,
            "opacity": 0.4,
            "blur": 0,
            "size": "cover",
            "position": "center"
        }));
    }

    if let Some(tabs) = root.get_mut("tabs").and_then(Value::as_array_mut) {
        for tab in tabs.iter_mut() {
            if let Some(obj) = tab.as_object_mut() {
                obj.insert("background_override".to_string(), Value::Null);
            }
        }
    }
}
```

Tests (mirroring V1.4-01's coverage):
- `v1_4_to_v1_5_adds_background_and_stamps_overrides` — `terminal.background` populated with defaults, every tab has `background_override: null`.
- `v1_5_file_is_not_re_detected` — file with `terminal.background` present skips re-migration, no extra backup.
- `v1_2_cascades_through_v1_3_v1_4_and_v1_5` — a v1.2 cold-start lands at v1.5 with three backups.
- `background_override_disabled_string_round_trips` — typed deserialize of a hand-edited `"disabled"` survives serde and re-serializes as `"disabled"`.

### 8. Global UI — Settings → Appearance gains a "Terminal background" subsection

Place under the existing Terminal palette controls in the same Appearance section (V5-01 / V1.4-01 territory).

The mode toggle is a derived view, not a fourth schema field: the UI computes `'theme' | 'color' | 'image'` from `(image, color)` at render time, and writing a mode rewrites those two fields. This keeps the schema additive and the wire format flat.

```svelte
<script lang="ts">
  import { settings as settingsStore } from '../store';
  import { applySettings } from '../store';

  $: bg = $settingsStore.terminal.background;
  $: mode = bg.image ? 'image' : (bg.color ? 'color' : 'theme');

  function setMode(next: 'theme' | 'color' | 'image') {
    const updated = structuredClone($settingsStore);
    if (next === 'theme') {
      updated.terminal.background.image = null;
      updated.terminal.background.color = null;
    } else if (next === 'color') {
      updated.terminal.background.image = null;
      if (!updated.terminal.background.color) updated.terminal.background.color = '#1a1a1a';
    } else {
      // 'image' — leave color alone (it's the optional tint); user picks file next.
    }
    void applySettings(updated);
  }
</script>

<section class="terminal-background">
  <h3>Terminal background</h3>
  <RadioToggle value={mode} on:change={(e) => setMode(e.detail)}
    options={[
      { v: 'theme', l: 'Theme default' },
      { v: 'color', l: 'Solid color' },
      { v: 'image', l: 'Image' },
    ]} />

  {#if mode === 'color'}
    <ColorInput bind:value={bg.color}
      label="Background color"
      hint="Overrides the theme's background color." />
  {:else if mode === 'image'}
    <FilePicker bind:path={bg.image} accept="image/*" />
    <RangeInput bind:value={bg.opacity} min={0} max={1} step={0.05} label="Opacity" />
    <RangeInput bind:value={bg.blur} min={0} max={40} step={1} label="Blur (px)" />
    <Select bind:value={bg.size} options={['cover', 'contain', 'tile']} label="Size" />
    <TextInput bind:value={bg.position} label="Position (CSS)" placeholder="center" />
    <ColorInput bind:value={bg.color}
      label="Tint color (optional)"
      hint="Tints the dimming overlay. Defaults to black when unset." />
  {/if}
</section>
```

`FilePicker` wraps `@tauri-apps/plugin-dialog`'s `open()` with `multiple: false` and a filter for image MIME types. It writes the absolute path string to `bg.image`.

Each control writes through `applySettings` (already used elsewhere in the section). For the sliders, debounce the IPC at ~150ms so a drag doesn't fire 60 writes per second; the value displays update immediately from the local binding. The recreate debounce in Step 5b (`queueRecreate`) is a separate stage downstream.

### 9. Per-tab UI — explicitly deferred

Schema and resolver fully support `null` / `'disabled'` / explicit override from V1.4-02 ship. The Configure Tab dialog gains no new row. Verify schema/serde tests cover all three states regardless (Step 2's round-trip tests). The follow-on milestone that adds the per-tab UI rows can land without any further schema work.

### 10. README and DESIGN.md

README — add a paragraph under the V1.4-01 "Terminal palette" section:

> **Terminal background.** Settings → Appearance lets you set a background image or solid color for terminal panes. **Solid color** has no performance cost — it's a one-line theme tweak. **Image** mode forces the slower DOM renderer (2-5× slower than canvas for high-throughput output like `tail -F`) and resets the tab's scrollback when toggled on or off, both of which are unavoidable trade-offs of xterm.js's renderer split. The image path also exposes opacity, blur, size, position, and an optional tint color. Per-tab background overrides arrive in a follow-on release.

DESIGN.md — extend the Settings section with two-three sentences on the four-cell rendering matrix and the three-state override; link to this milestone for full detail.

## Test Plan

- **Unit tests (Rust)**:
  - `BackgroundOverride` round-trip: `null`, `"disabled"`, full object — each survives serialize+deserialize. Garbage (`42`, `[]`, unknown string) errors with the explicit message.
  - Migration v1.4 → v1.5: backup written exactly once, `terminal.background` defaulted, every tab has `background_override: null`.
  - Idempotency: second pass on a v1.5 file is a no-op, no second backup.
  - Cascade: a v1.2 file lands at v1.5 with three backups (`v1.2`, `v1.3`, `v1.4`).

- **Unit tests (TS)**:
  - `effectiveBackgroundMode`: full 4×3 matrix (4 image/color combos × 3 override states) produces the expected `RenderingMode`.
  - `composeTheme`: image + tint produces correct `rgba(...)`; color-only replaces `theme.background`; none preserves theme; image without tint defaults to black.
  - `categoryOf`: `image` mode → `'image'`, all others → `'fast'`.

- **Manual — color path** (no perf cost, no recreation):
  - Set global `terminal.background.color` to `#1a2a4a`. All terminals repaint instantly. Scrollback intact. Renderer still on canvas.
  - Combine with V1.4-01 themes: Dracula + custom navy bg → Dracula's foreground over navy.
  - Toggle back to "Theme default" — repaints to theme bg, no recreate.

- **Manual — image path** (recreate on toggle):
  - Set global image to a 1920×1080 JPG. Every terminal recreates; scrollback resets per the documented contract; image visible behind cells.
  - Set `opacity = 0.7`, `blur = 20`, `size = "cover"` — cells legible over blurred image; opacity/blur changes apply in place (no recreate).
  - Toggle the image off — recreates back to canvas; scrollback resets again.

- **Manual — perf check**:
  - With image enabled, run `tail -F` on a noisy log; observe perceptible lag versus color-only. Confirms README's renderer warning.
  - With color-only, run the same — no lag relative to today's behavior.

- **Manual — bad image path**:
  - Hand-edit settings.json: `terminal.background.image = "C:\\does\\not\\exist.png"`. App loads, Settings shows an error indicator, terminals render as if image were `null` (color path applies if set).

- **Manual — three-state override (schema-only, no UI)**:
  - Hand-edit a tab to `"background_override": "disabled"`. That tab uses theme background only, even when the global has an image.
  - Hand-edit to a full object override. That tab uses its own image, distinct from global.
  - Both round-trip through save/reload (no UI, no rewrite to `null`).

- **Manual — mass recreation**:
  - With 4-6 tabs all inheriting a global image, toggle global image off → on. Confirm the recreate-debounce keeps the transition acceptably smooth. If it stutters, queue debounce-tuning as follow-up.

## Files Most Likely Touched

- `src-tauri/src/settings/schema.rs` — `TerminalBackgroundSettings`, `BackgroundSize`, three-state `BackgroundOverride` with custom (de)serialize, `background_override` on both tab variants, default-tab updates
- `src-tauri/src/settings/migration.rs` — v1.4 → v1.5 transform + backup + tests
- `src/lib/terminal/background.ts` (new) — `effectiveBackgroundMode`, `categoryOf`, `composeTheme`, `applyHostBackgroundCss`, `pathToFileURL`/`convertFileSrc` helper
- `src/lib/terminal/background.test.ts` (new) — 4×3 matrix tests + composeTheme tests
- `src/lib/terminals.ts` — renderer branch at construction, `bgCategory` on `TerminalEntry`, `unsubTheme` → `unsubAppearance`, `queueRecreate` debouncer, host CSS class toggling
- `src/lib/terminals.css` (new — lift inline styles) — `.terminal-host.bg-image::before` + `.xterm` blur stack
- `src/lib/settings/types.ts`, `src/lib/settings/store.ts` — TS mirror of `terminal.background`, `BackgroundOverrideWire`, `defaultSettings`
- `src/lib/settings/AppearanceSection.svelte` (path TBD per V5-01 layout) — Background subsection: mode toggle + conditional controls
- `src/lib/settings/FilePicker.svelte` (new — small wrapper over `@tauri-apps/plugin-dialog`)
- `package.json` — add `@xterm/addon-canvas` (`^0.7.0`)
- `README.md`, `docs/DESIGN.md` — renderer / scrollback caveats

## Risks and Open Questions

- **Recreation flow correctness.** Today's `terminals.destroyForTab` / `createForTab` flow doesn't normally fire mid-session; toggling background **image** is the first runtime trigger. Verify the PTY listener wiring survives recreation and the new xterm instance picks up the live byte stream cleanly. If the byte channel needs explicit re-binding, document it. (Toggling color only avoids this entirely — no recreation, just `term.options.theme = next`.)
- **Performance regression risk for power users.** Anyone with `tail -F` on a large log + a global background **image** will see visible lag. Make the README warning obvious; don't treat as a bug if reported. The color path doesn't have this risk and is a no-cost alternative for users who only wanted "different background color than the theme."
- **Blur-with-cover surprise**: `backdrop-filter: blur` blurs *what's beneath the element*, not the background-image directly. The cell layer needs to sit *above* the image but blur the image — this is a CSS layering gotcha worth a short prototype before committing the implementation steps. Only matters in the image path.
- **Global change → mass recreation stutter** with many tabs that have an image. If it's too noisy, debounce the recreate path (don't recreate immediately on every slider tick during live preview — wait for blur or release). Color-only changes don't recreate; they're cheap.
- **Color overlap with V1.4-01 Custom palette.** A user who wants *just* a custom background color now has two ways to do it: V1.4-01 Custom palette with only the `background` field changed, or V1.4-02 `terminal.background.color`. They're behaviorally equivalent for the no-image case. The mental model: V1.4-01 is for "I want different terminal text/cell colors" (the whole palette); V1.4-02 color is for "I want a different fill behind the text without touching the palette." The Settings UI should make this distinction obvious — e.g., the V1.4-02 color picker is labeled "Override theme background color" and notes that V1.4-01's Custom palette is the route for tuning ANSI colors.
- **Migration of "image-only" intent**: if a future version splits the `BackgroundConfig` into `BackgroundImage(...)` and `BackgroundColor(...)` discriminated variants, the current "both fields optional in one struct" shape is the migration cost. Acceptable today; record the trade-off.
