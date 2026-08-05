# V29 — xterm 6.0 renderer migration (canvas → WebGL)

**Status:** LIVE-VERIFIED (2026-08-05) — dev-build session: terminal
renders on WebGL, shell + AI tabs and renderer behavior user-tested clean
(the devtools context-loss recipe in §5 remains best-effort/unexercised).
Closed GitHub issue #20 (D-7 from the 2026-08-04 maintenance run;
GH milestone 3). Implemented in 1cf4049.
**Forced by upstream:** xterm 6.0 deleted the canvas renderer entirely —
`@xterm/addon-canvas` was removed from the monorepo, is peer-locked to 5.x,
and has been unpublished since 2024. cImp loads `CanvasAddon`
unconditionally for the fast path (`src/lib/terminals.ts:595`), so the bump
forces a WebGL-or-DOM decision in the same change.

## Prerequisite research (maintenance report 2026-08-04 §4, §D-7)

- Heavy-usage surfaces verified **typings-identical** 5.5→6.0: `onData`,
  parser CSI hooks (DECSET interception in `installAiMouseControl`),
  `attachCustomWheelEventHandler`, buffer API, markers/decorations under
  `allowProposedApi` (selection TTS).
- Breaking-but-unused options (grepped, no matches): `overviewRulerWidth`,
  `windowsMode`, `fastScrollModifier`, Alt→Ctrl+Arrow hack.
- 6.0-era addons dropped `peerDependencies` entirely — npm will NOT guard
  version mismatches. The lockfile is the only guard: **move all-or-nothing**.
- New research this milestone (verified against the published 0.19.0 tarball):
  - `WebglAddon.activate` uses the same deferred-activation pattern as the
    old canvas addon (`onWillOpen` when loaded pre-open).
  - `getContext('webgl2')` returning null makes activate **throw**
    (`"WebGL2 not supported"`). A pre-open `loadAddon` therefore defers that
    throw into `term.open()` — which must never fail. Hence D-2 below.

## Design — locked decisions

1. **Package set (one commit, all-or-nothing):**
   `@xterm/xterm` ^5.5.0 → **^6.0.0**, `@xterm/addon-fit` ^0.10.0 →
   **^0.11.0**, `@xterm/addon-serialize` ^0.13.0 → **^0.14.0**,
   `@xterm/addon-canvas` **removed**, `@xterm/addon-webgl` **^0.19.0 added**.
   These are the stable 6.0-era releases (betas on npm are 6.1-era — do not
   take them).

2. **Renderer assignment keeps the V1.4-02 structure:** fast path (no
   background image) = **WebGL addon**; image mode = **in-core DOM
   renderer** (no renderer addon) — the WebGL canvas is a single opaque
   surface exactly like the old canvas addon and would obscure the CSS image
   beneath the cells layer. The `allowTransparency` constructor logic is
   unchanged. The Settings hint ("renderer (WebGL ↔ DOM)",
   `SettingsApp.svelte:2422`) becomes literally accurate; no settings or
   schema change.

3. **Load order changes:** the renderer addon is loaded **after
   `term.open(host)`** (fit/serialize stay pre-open), inside a try/catch.
   If construction/activation throws (WebGL2 unavailable: GPU blocklist,
   RDP, headless), log a `console.warn` and continue on the DOM renderer —
   the Terminal and PTY are unaffected. `term.open()` must never be able to
   fail because of the renderer.

4. **Context-loss policy (the WebView2 unknown, D-7):** register
   `onContextLoss` on every WebglAddon instance. On loss: dispose the addon
   (xterm reverts to the DOM renderer automatically; buffer, PTY, and
   listeners survive), then attempt **one** fresh WebglAddon load. If the
   retry throws or loses its context too, stay on DOM for this Terminal
   instance (a renderer-flip recreate or tab restart naturally re-attempts).
   Bounded retry — no loops against a resetting driver.

5. **Renderer-flip path unchanged.** fast↔image still goes through the
   V1.4-03 serialize-and-replay full-Terminal recreate (`queueRecreate` →
   `pty_rebind`), so the WebGL addon never needs dynamic unload on a
   category flip. The addon handle lives in a closure/helper, not in
   `TerminalEntry`; `term.dispose()` disposes loaded addons.

6. **Comment hygiene:** every "canvas renderer / canvas addon" comment in
   `terminals.ts` (lines ~566-568, ~591-596, ~661-663) is updated to name
   WebGL and, where relevant, the fallback contract.

## Invariants (cross-module)

- Image mode NEVER loads the WebGL addon (CSS image must show through).
- `term.open()` cannot throw due to renderer availability.
- Scrollback snapshot replay (`options.scrollbackSnapshot`) still lands
  before the first live PTY byte regardless of renderer.
- AI-tab mouse-local control (`installAiMouseControl`) registers before the
  PTY binds — unchanged by addon load-order move (it hooks the parser, not
  the renderer).

## Implementation gates

- `npm install` resolves cleanly; lockfile shows exactly the five-package
  change; `@xterm/xterm/css/xterm.css` import path still valid in 6.0.
- `npm run check` (svelte-check) clean, `npm run test` (vitest) green,
  `npm run build` (vite) succeeds.
- Grep proves no `CanvasAddon`/`addon-canvas` reference survives anywhere.

## Live verification (manual, pending)

1. **Shell tab basics:** rendering, theme switch, font-size change in
   place, selection + copy-on-select, scrollback scroll.
2. **AI tab (Claude fullscreen TUI):** mouse stays local (select-to-speak,
   right-click paste), hold-Alt bypass hands mouse to the app, wheel
   scrolls the TUI.
3. **Renderer flip:** set a background image → DOM renderer + image
   visible + scrollback replayed; remove image → back to WebGL, replayed.
4. **WebGL fallback:** launch with GPU disabled (e.g.
   `--disable-gpu` WebView2 args or RDP session) → terminal still renders
   (DOM), a single console warning, no crash.
5. **Context loss (best-effort):** devtools → `WEBGL_lose_context`
   extension on the xterm canvas, or a real driver reset — terminal keeps
   rendering (DOM or recreated WebGL), PTY session intact.
