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

## Files Most Likely Touched

- `src-tauri/src/settings/schema.rs` — `TerminalBackgroundSettings`, the three-state `BackgroundOverride` enum, `background_override` on tabs
- `src-tauri/src/settings/migration.rs` — v1.4 → v1.5 transform + backup
- `src/lib/themes/resolve.ts` (or new `src/lib/terminal/background.ts`) — `effectiveBackground` resolver
- `src/lib/terminals.ts` — renderer branch at construction, host CSS application, recreate-on-toggle path
- `src/lib/settings/AppearanceSection.svelte` — global Background controls, file picker
- README.md, docs/DESIGN.md — renderer / scrollback caveats

## Risks and Open Questions

- **Recreation flow correctness.** Today's `terminals.destroyForTab` / `createForTab` flow doesn't normally fire mid-session; toggling background **image** is the first runtime trigger. Verify the PTY listener wiring survives recreation and the new xterm instance picks up the live byte stream cleanly. If the byte channel needs explicit re-binding, document it. (Toggling color only avoids this entirely — no recreation, just `term.options.theme = next`.)
- **Performance regression risk for power users.** Anyone with `tail -F` on a large log + a global background **image** will see visible lag. Make the README warning obvious; don't treat as a bug if reported. The color path doesn't have this risk and is a no-cost alternative for users who only wanted "different background color than the theme."
- **Blur-with-cover surprise**: `backdrop-filter: blur` blurs *what's beneath the element*, not the background-image directly. The cell layer needs to sit *above* the image but blur the image — this is a CSS layering gotcha worth a short prototype before committing the implementation steps. Only matters in the image path.
- **Global change → mass recreation stutter** with many tabs that have an image. If it's too noisy, debounce the recreate path (don't recreate immediately on every slider tick during live preview — wait for blur or release). Color-only changes don't recreate; they're cheap.
- **Color overlap with V1.4-01 Custom palette.** A user who wants *just* a custom background color now has two ways to do it: V1.4-01 Custom palette with only the `background` field changed, or V1.4-02 `terminal.background.color`. They're behaviorally equivalent for the no-image case. The mental model: V1.4-01 is for "I want different terminal text/cell colors" (the whole palette); V1.4-02 color is for "I want a different fill behind the text without touching the palette." The Settings UI should make this distinction obvious — e.g., the V1.4-02 color picker is labeled "Override theme background color" and notes that V1.4-01's Custom palette is the route for tuning ANSI colors.
- **Migration of "image-only" intent**: if a future version splits the `BackgroundConfig` into `BackgroundImage(...)` and `BackgroundColor(...)` discriminated variants, the current "both fields optional in one struct" shape is the migration cost. Acceptable today; record the trade-off.
