# V29 — xterm 6.0 renderer migration (canvas → WebGL)

**Status:** LIVE-VERIFIED (2026-08-05) — dev-build session: terminal
renders on WebGL, shell + AI tabs and renderer behavior user-tested clean
(the devtools context-loss recipe in §5 remains best-effort/unexercised).
Closed GitHub issue #20 (D-7 from the 2026-08-04 maintenance run;
GH milestone 3). Implemented in 5d49a38.
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
    throw into `onWillOpen` during `term.open()`. Hence D-3 below.
    **Correction (code review 2026-08-05):** the original rationale for the
    post-open load ("open() would fail") was wrong — xterm 6's `open()` wraps
    the `onWillOpen` fire in a swallowing try/catch, so a pre-open activate
    throw is *silently eaten* and the terminal quietly ends up on the DOM
    renderer. That silence is the actual problem: post-open `loadAddon`
    activates synchronously, which is what lets **our** try/catch observe the
    failure, latch it, and emit the `console.warn` DOM-fallback diagnostic.
    Same code, true reason.

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
   the Terminal and PTY are unaffected. The point of post-open is
   **catchability**: only a synchronous `loadAddon` puts the throw where our
   try/catch (and therefore the warning + the sticky failure latch) can see
   it. See the correction under "Prerequisite research".

4. **Context-loss policy (the WebView2 unknown, D-7):** register
   `onContextLoss` on every WebglAddon instance. On loss: dispose the addon
   (xterm reverts to the DOM renderer automatically; buffer, PTY, and
   listeners survive), then attempt **one** fresh WebglAddon load. If the
   retry throws or loses its context too, stay on DOM for this Terminal
   instance (a renderer-flip recreate or tab restart naturally re-attempts).
   Bounded retry — no loops against a resetting driver.
   **Amended by D-7b below**: the retry budget is now per *visible session*
   and the "gave up" state is a sticky per-terminal flag.

5. **Renderer-flip path unchanged.** fast↔image still goes through the
   V1.4-03 serialize-and-replay full-Terminal recreate (`queueRecreate` →
   `pty_rebind`), so a category flip never needs a dynamic unload.
   *(Superseded in part by D-7b: the addon handle now lives on
   `TerminalEntry`, because visibility — not just the category flip —
   drives load/unload.)*

6. **Comment hygiene:** every "canvas renderer / canvas addon" comment in
   `terminals.ts` (lines ~566-568, ~591-596, ~661-663) is updated to name
   WebGL and, where relevant, the fallback contract.

7. **D-7b — visibility-scoped WebGL contexts (M17, code review 2026-08-05).**
   **Only currently visible terminals hold a WebGL2 context.**

   *Why.* Chromium/WebView2 caps live WebGL contexts at ~16 per process and
   force-loses the least-recently-used one past that. cImp creates every tab's
   terminal at startup (`App.svelte`) and never destroys one — the keep-alive
   registry parks the N−1 inactive hosts in the offscreen stash. Loading the
   addon at construction therefore meant *one live context per tab*, and there
   is no tab cap. At 17+ terminal tabs the cap starts evicting: each eviction
   costs a 3 s freeze (the webgl addon waits 3 s for `webglcontextrestored`
   before firing `onContextLoss`), and D-4's retry then creates a fresh
   context which evicts the next victim — a self-sustaining wave of 3 s
   freezes across the tab set. Scoping the context to visibility bounds the
   count by the number of panes on screen (single digits), which the cap
   cannot reach. The original V29 spec never considered context *count*.

   *Where.* `attachTerminal` / `detachTerminal` in `terminals.ts` — the single
   seam where a host moves between a pane slot and the offscreen stash, and
   the seam that already owns the `attached` flag. `createTerminal` no longer
   loads the addon at all (a fresh terminal is born stashed). Both transitions
   go through one idempotent `syncWebglRenderer(entry)`; the policy itself is
   the pure predicate `shouldHoldWebgl` in `terminal/background.ts`
   (unit-tested in `background.test.ts` — no xterm/WebGL needed).

   *Split views count.* The layout tree can show several panes at once, each
   with its own attached active tab, so "visible" means **every attached
   terminal**, not "the focused one". Keying the policy on the per-entry
   `attached` flag gives that for free.

   *Ordering (no flicker).* Load happens synchronously in `attachTerminal`,
   in the same task as the `appendChild` into the slot — `RenderService
   .setRenderer` does a `_fullRefresh()` that is queued before the browser
   paints the frame, so the user never sees an intermediate state. (Deferring
   the load to the post-attach rAF would expose one frame of bare host
   background.) Dispose happens synchronously in `detachTerminal` *before*
   the host is moved offscreen, so the replacement DOM renderer is created
   while the host still has real layout, while the repaint it queues lands
   on the next frame — by which time the host is already stashed and
   invisible. `WebglAddon.dispose()` never fires `onContextLoss` (that event
   comes from the canvas's own `webglcontextlost` listener, which disposal
   removes), so stashing cannot be mistaken for a driver failure.

   *Latches.* Two distinct pieces of state on `TerminalEntry`:
   - `webglRetried` — D-4's one-shot retry budget, scoped to the current
     visible session and reset on detach.
   - `webglFailed` — **sticky**: set when `loadAddon` throws, or when the
     retry also lost its context. Survives stash→show cycles, so a machine
     without usable WebGL is not re-probed (and does not re-warn) on every
     tab switch. It lives on `TerminalEntry`, so it is cleared by anything
     that builds a **new Terminal** — a renderer-flip recreate
     (`queueRecreate`) or closing and reopening the tab. Note the precision
     fix to D-4's wording: a *PTY* restart does **not** clear it (same
     Terminal object), and never did.

   *Unchanged.* Image-background terminals are still never given the addon.
   The keep-alive contract is untouched: terminals are still never destroyed
   on a tab switch, and buffer/scrollback/PTY/listeners all survive a
   renderer swap (xterm keeps rendering via the in-core DOM renderer while
   stashed). The DOM-fallback `console.warn` — the only signal that a machine
   is running unaccelerated — still fires exactly once per genuine failure.

## Invariants (cross-module)

- Image mode NEVER loads the WebGL addon (CSS image must show through).
- `term.open()` cannot throw due to renderer availability. (It could not
  anyway — xterm 6 swallows `onWillOpen` throws — but the post-open load
  keeps the failure *observable*, which is the property we actually rely on.)
- **D-7b:** the number of live WebGL2 contexts equals the number of attached
  (visible) non-image terminals, never the number of tabs. Any new hide/show
  path for a terminal host must route through `attachTerminal` /
  `detachTerminal`, or it silently breaks this bound.
- **D-7b:** disposing the addon on stash must never be mistaken for a driver
  failure — `webglFailed` is set only by a load throw or a real
  `onContextLoss`, never by `unloadWebglRenderer`.
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

### D-7b (M17) — added 2026-08-05, pending

6. **Context count is bounded:** open 20+ terminal tabs, switch through all
   of them, then count canvases:
   `document.querySelectorAll('.terminal-host canvas.xterm-link-layer, .terminal-host canvas').length`
   — or simply
   `document.querySelectorAll('#terminal-offscreen canvas').length`, which
   must be **0**. No 3 s freeze anywhere in the sweep; devtools console shows
   no `webglcontextlost`.
7. **Switch is seamless:** rapid tab switching (and Ctrl+Alt+Arrow pane
   focus moves) shows no flash, no blank frame, no geometry jump; scrollback
   and cursor position are preserved on return.
8. **Split view:** split into 2–3 panes with different tabs — each visible
   pane's terminal renders on WebGL simultaneously (canvas present in every
   attached `.terminal-slot`); moving a tab between panes keeps exactly one
   context for it.
9. **Failure stickiness:** on a machine/session with WebGL unavailable
   (RDP or `--disable-gpu`), the DOM-fallback `console.warn` appears **once
   per terminal**, not once per tab switch.
