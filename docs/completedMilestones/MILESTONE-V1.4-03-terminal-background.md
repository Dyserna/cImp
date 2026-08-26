# Milestone V1.4-03: Terminal Background — Per-Tab UI and Scrollback Survival

## Purpose

V1.4-02 shipped the terminal-background "skeleton" — schema, four-cell rendering matrix resolver, renderer-branching wiring in `terminals.ts`, migration v1.4 → v1.5, global Settings UI, README and DESIGN docs. Two pieces were explicitly deferred:

1. The **per-tab Configure Tab UI** for `background_override`. The schema, custom serde for the three-state `null` / `"disabled"` / `Custom(BackgroundConfig)` shape, and resolver all support per-tab overrides from V1.4-02 day one. Only the Configure Tab dialog row is missing.
2. **Scrollback survival** across renderer-category flips (fast ↔ image). The current recreate path destroys the xterm.js Terminal and calls `pty_restart`, which kills and respawns the shell — both the visible scrollback and the shell session (env, cwd, running processes, aider conversation state) are lost. V1.4-02 documented this as the price of the fast/slow renderer split.

V1.4-03 closes both. The per-tab UI is a refactor + a new dialog row. Scrollback survival is the harder piece: a new `pty_rebind_channel` Tauri command keeps the PTY alive across the destroy/create cycle, and `@xterm/addon-serialize` captures-and-restores the visible scrollback into the new xterm. The two pieces are independent — the per-tab UI works without scrollback survival, and scrollback survival benefits *every* renderer-category flip (whether triggered by global mode change, per-tab override change, or a single slider drag that crosses the image/no-image threshold).

## What This Milestone Delivers

1. **`BackgroundConfigEditor.svelte`** — extracted from `SettingsApp.svelte:587–729`. A self-contained component bound to a `TerminalBackgroundSettingsWire` value, owning the mode toggle (Theme default / Solid color / Image), the conditional color picker / file picker / opacity / blur / size / position / tint controls, and the user-driven-`bgMode` initialization pattern from `a4d973a`. No behavior change for the global path; the existing Settings → Appearance subsection swaps to consume the new component.
2. **Per-tab Background row — two consumers, mirroring how V1.4-01 themes shipped:**
   - **Shell tabs** — `ConfigureTabDialog.svelte` gains a Background section using the three-state radio shape from V1.4-01's theme row at `:74–120`. Save flows through `reconfigure_shell_tab`.
   - **AI tabs** (Claude, aider) — `TabSettingsSection.svelte` gains the same Background section, sitting next to the existing `selectThemeOverride` block at `:61–88`. Save flows through `applySettings` (which calls `settings_update` IPC and the backend broadcasts back). No new Tauri command for AI tabs — the whole-Settings serializer already covers `background_override`.
   - Both surfaces use the same three-state shape:
     - **"Use global default" (current: <human-readable summary>)** — writes `background_override: null`. Summary shows `"Theme background"` / `"Solid #1a2a4a"` / `"Image: cat.jpg"`.
     - **"Disabled — use theme background only"** — writes `background_override: "disabled"`. Always available; helper text: "no observable effect when global is also Theme default."
     - **"Custom for this tab"** — writes `background_override: { ...full BackgroundConfig... }` and expands a nested `BackgroundConfigEditor`. Initial Custom config seeded from the previous override (if any) or the current global background.
3. **`background_override` plumbed through `reconfigure_shell_tab`** at `src-tauri/src/ipc/tab_lifecycle.rs:355–427`. Mirrors the existing `theme_override` parameter at `:365` and the `cfg.theme_override = theme_override` assignment at `:404`. **AI tabs need no IPC plumbing change** — `applySettings` already serializes the whole `Settings` struct including any field on tab variants, and the backend's `settings_update` writes it through.
4. **`pty_rebind_channel(tab, channel) -> Result<()>`** Tauri command. Atomically swaps a still-running PTY's output target from the old `Channel<String>` to a new one without restarting the shell. Errors with a typed variant if no PTY is registered for the tab.
5. **Backend rebind plumbing** in `PtyManager` — a control mpsc owned by the processor task receives `ChannelChange(new_channel)` events and swaps its local `channel` binding atomically between byte batches. Bytes that arrive *during* the rebind window (between JS `destroyTerminal` and the rebind callback resolving) are buffered in the processor's existing `mpsc::Receiver<Vec<u8>>` queue — the reader task is not paused, so no PTY-side flow control is needed.
6. **`terminals.ts` recreate path uses `pty_rebind`** instead of `pty_restart`. PTY survives the destroy/create cycle. Shell session, env, cwd, running processes, aider/claude conversation state — all preserved.
7. **Scrollback capture-and-restore** via `@xterm/addon-serialize` (^0.13.0). Before `destroyTerminal` tears down xterm, capture a serialized snapshot of the visible scrollback. After the new Terminal is constructed, `term.write(snapshot)` replays the snapshot. The new xterm sees a stream of ANSI escapes that recreate cell state, scrollback included.
8. **README updated** — replace the "scrollback resets when image toggles" caveat with a positive statement: *"Toggling the image background switches xterm.js renderers cleanly. Your shell session, scrollback, and running processes are all preserved."* The DOM-vs-canvas perf trade-off remains (it's structural, not solvable here).
9. **DESIGN.md** — extend the Settings section with a paragraph on the rebind protocol: what survives, what doesn't (cross-app-restart scrollback is still lost — that needs a PTY-side ring buffer, deferred), and the bytes-during-rebind buffer semantics.

## Key Deltas vs V1.4-02

- **No new schema, no migration, no settings version bump.** Schema landed in V1.4-02 (the three-state `BackgroundOverride` with custom serde at `schema.rs:688–715` round-trips today). V1.4-03 is wiring + UI + a new Tauri command. Settings stays at v1.5.
- **Three-state override row, not two.** V1.4-01's theme row is `null | explicit`. The background override row is `null | "disabled" | explicit`. The `"disabled"` state is not a placeholder — it has observable behavior when the global has any background set ("force this tab to plain theme bg even though my global is an image"). Hide-when-irrelevant is wrong here because the global state can change; the row needs to render consistently regardless. Helper text instead.
- **Recreate path becomes asymmetric across operations:**
  - First creation: `pty_start(tab, channel)` (unchanged).
  - Renderer-category flip: serialize-capture → `destroyTerminal` (xterm only) → `createTerminal` → `pty_rebind(tab, new_channel)` → `term.write(snapshot)`. PTY persists.
  - Tab close: `pty_stop` (unchanged).
  - User-initiated Restart: `pty_restart` (unchanged — explicit re-run is the user's intent).
- **Bytes-during-rebind handling.** A PTY can emit bytes between "destroy old xterm" and "rebind to new channel." The current `mpsc::Receiver<Vec<u8>>` between the reader task and the processor task is a bounded buffer (channel size 256 at `manager.rs:135`); during the rebind window bytes accumulate there. The processor's select loop polls `rx.recv()` and `control_rx.recv()` together; whichever wins, the other future's state is preserved (`mpsc::Receiver::recv` is cancel-safe — same property the existing `settings_rx.recv()` arm at `pty/tasks.rs:151` relies on today). When the control arm fires `ChannelChange`, the processor swaps `channel = next` and returns to the select; the next byte batch in `rx` dispatches against the new channel. No byte loss.
- **Scrollback memory cost is non-zero, transient, and frontend-side.** `@xterm/addon-serialize` produces an ANSI string sized roughly proportional to (visible columns × scrollback lines × avg-bytes-per-cell). At 200 cols × 1000 lines × 3 bytes ≈ 600 KB per recreate. This is in JS heap, lives only between capture and the `term.write` resolution. No backend cost.
- **The "Disabled" state's relationship to global.** When global is `image: None, color: None` (the default), a tab with `background_override: "disabled"` and a tab with `background_override: null` look identical. That's by design — the override semantics are independent of the global value at any given time. The dialog says so in its hint text rather than dynamically hiding the Disabled option, which would create a confusing toggle when the user changes the global.
- **Live-update parity for per-tab edits.** When a per-tab override is changed via the dialog, the existing `unsubAppearance` subscription at `terminals.ts:330` already picks it up — the resolver is keyed on the tab and runs against the current store. A category flip triggers `queueRecreate` which now goes through the rebind path. No new subscription wiring.

## What This Milestone Does NOT Do

- **Cross-app-restart scrollback.** When cimp exits, the PTY exits with it. Scrollback is gone. Restoring it across cimp restarts requires a backend ring buffer with persistence, which is a separate, larger feature (memory cap, on-disk format, replay timing). Defer until users ask.
- **Animated/video backgrounds.** Still out of scope per V1.4-02.
- **Project-local relative-path resolution for `terminal.background.image`.** Still pending `FEATURE-config-scope.md`.
- **Background-config presets.** No "save this background as 'Pinky Floyd' and apply to multiple tabs." Users hand-edit JSON or copy the override structure manually for now.
- **Live preview in the dialog.** Changes inside the Configure Tab dialog only apply on Save (matching the existing theme row's behavior). A live-preview model would need explicit cancel-revert plumbing; the Save-on-OK pattern is consistent with the rest of the dialog.

## Implementation Steps

### 1. Extract `BackgroundConfigEditor.svelte`

Lift the existing markup and logic from `SettingsApp.svelte:587–729` into a new component. Props:

```ts
export let config: TerminalBackgroundSettingsWire; // bindable
export let onChange: (next: TerminalBackgroundSettingsWire) => void;
```

Two-way bind via `bind:config` from each consumer; the component owns `bgMode` as internal state.

The a4d973a fix — *"keep bgMode user-driven after first snapshot load"* — is the load-bearing piece. Preserve the pattern exactly:

```svelte
<script lang="ts">
  let bgMode: 'theme' | 'color' | 'image' = config.image
    ? 'image'
    : (config.color ? 'color' : 'theme');
  // bgMode is intent; (config.image, config.color) are state. After first
  // load, user clicks set bgMode; settings snapshots do not re-derive it.
  // This prevents the click-Image-then-snapshot-flip-back regression that
  // a4d973a fixed.

  function setMode(next: 'theme' | 'color' | 'image'): void {
    bgMode = next;
    if (next === 'theme') {
      config = { ...config, image: null, color: null };
    } else if (next === 'color') {
      config = { ...config, image: null, color: config.color ?? '#1a1a1a' };
    } else {
      // 'image' — leave color alone (it's the optional tint); user picks
      // the file via the FilePicker that this branch exposes.
    }
  }
</script>
```

The Settings → Appearance subsection collapses to:

```svelte
<BackgroundConfigEditor bind:config={$settingsStore.terminal.background} />
```

(plus the wrapping section header / hint paragraph). Existing behavior is unchanged; the test at `src/lib/terminal/background.test.ts` continues to pass without modification because the editor is a pure UI extraction.

### 2. Per-tab Background section — `ConfigureTabDialog.svelte` (shell tabs) and `TabSettingsSection.svelte` (AI tabs)

V1.4-01 shipped per-tab themes in *both* surfaces because shell tabs and AI tabs have different settings UIs. Background follows the same split.

**2a. Shell tabs — `ConfigureTabDialog.svelte`.** Mirror the theme row's shape at `:74–120`. New state:

```ts
type BgOverrideMode = '__inherit' | '__disabled' | '__custom';

let backgroundOverride: BackgroundOverrideWire | null = null;
let bgOverrideMode: BgOverrideMode = '__inherit';

// In initFields(tab): seed from the live store, same source of truth as
// theme_override.
const liveTab = get(settingsStore).tabs.find((t) => t.id === tab);
backgroundOverride = liveTab?.background_override ?? null;
bgOverrideMode =
  backgroundOverride === null ? '__inherit'
  : backgroundOverride === 'disabled' ? '__disabled'
  : '__custom';
```

Mode-change handler (analogous to `selectOverride` for themes):

```ts
function selectBgOverride(next: BgOverrideMode): void {
  bgOverrideMode = next;
  if (next === '__inherit') {
    backgroundOverride = null;
    return;
  }
  if (next === '__disabled') {
    backgroundOverride = 'disabled';
    return;
  }
  // '__custom' — seed from prior custom override, or fall back to global.
  if (backgroundOverride && typeof backgroundOverride === 'object') return; // already custom
  const liveGlobal = get(settingsStore).terminal.background;
  backgroundOverride = { ...liveGlobal };
}
```

Markup — placed alongside the existing theme row:

```svelte
<section class="bg-row">
  <h4>Background</h4>
  <select value={bgOverrideMode} on:change={(e) => selectBgOverride(e.currentTarget.value as BgOverrideMode)}>
    <option value="__inherit">Use global default — {globalBgSummary($settingsStore.terminal.background)}</option>
    <option value="__disabled">Disabled — use theme background only</option>
    <option value="__custom">Custom for this tab</option>
  </select>

  {#if bgOverrideMode === '__custom' && backgroundOverride && typeof backgroundOverride === 'object'}
    <BackgroundConfigEditor bind:config={backgroundOverride} />
  {/if}

  <p class="hint">
    {#if bgOverrideMode === '__disabled'}
      No observable effect when the global background is also "Theme default."
    {/if}
  </p>
</section>
```

`globalBgSummary(bg)` is a small pure function:

```ts
function globalBgSummary(bg: TerminalBackgroundSettingsWire): string {
  if (bg.image) return `image (${bg.image.split(/[\\/]/).pop()})`;
  if (bg.color) return `solid ${bg.color}`;
  return 'theme background';
}
```

**2b. AI tabs — `TabSettingsSection.svelte`.** The existing theme block uses Svelte 5 runes (`$derived`, `$props`) and an `update<K>(key, value)` helper at `:37–40` that writes back through the parent's `onchange`. Mirror the same shape for background:

```ts
let bgOverrideMode: BgOverrideMode = $derived(
  settings.background_override === null ? '__inherit'
  : settings.background_override === 'disabled' ? '__disabled'
  : '__custom'
);

function selectBgOverride(value: BgOverrideMode): void {
  if (value === '__inherit') {
    update('background_override', null);
    return;
  }
  if (value === '__disabled') {
    update('background_override', 'disabled');
    return;
  }
  // '__custom' — seed from existing override or fall back to global.
  if (settings.background_override && typeof settings.background_override === 'object') return;
  const liveGlobal = get(settingsStore).terminal.background;
  update('background_override', { ...liveGlobal });
}

function updateCustomConfig(next: TerminalBackgroundSettingsWire): void {
  update('background_override', next);
}
```

The `BackgroundConfigEditor` is nested inside the Custom branch, bound to `settings.background_override` (when it's an object). All persistence goes through `update` → `onchange` → parent's `applySettings` — same path as the existing theme override on AI tabs. **No IPC change needed for AI tabs.**

### 3. Plumb `background_override` through `reconfigure_shell_tab` (shell tabs only)

`src-tauri/src/ipc/tab_lifecycle.rs:355–422` — extend the `reconfigure_shell_tab` signature to take `background_override: Option<BackgroundOverride>` alongside the existing `theme_override`. Persistence at `settings/persistence.rs:375` already has the field; the IPC just needs to thread the new arg into the same write path.

```rust
#[tauri::command]
pub async fn reconfigure_shell_tab(
    // ... existing args ...
    theme_override: Option<crate::settings::TerminalThemeSettings>,
    background_override: Option<crate::settings::BackgroundOverride>,
) -> Result<(), TabLifecycleError> {
    // ... existing validation ...
    settings_handle.update(|s| {
        if let Some(tab) = s.tabs.iter_mut().find(|t| t.id() == tab_id) {
            tab.set_theme_override(theme_override);
            tab.set_background_override(background_override);
        }
    });
    // ...
}
```

`TabConfig` needs `set_background_override` mirroring `set_theme_override` — likely in `schema.rs` near the existing setter.

**AI tabs are not part of this step.** Verified by inspection: `TabSettingsSection.svelte:61–88` writes `theme_override` via the local `update` helper, which propagates through `onchange` to the parent's `applySettings` (`src/lib/settings/store.ts:48`). `applySettings` calls `settings_update`, which serializes the whole `Settings` struct. Since `background_override` is already a field on both `AiToolTabConfig` and `ShellTabConfig` (from V1.4-02), it round-trips through this path automatically — no AI-specific IPC needed.

Frontend wiring in `ConfigureTabDialog.svelte:save()`:

```ts
await reconfigureShellTab({
  // ... existing args ...
  themeOverride,
  backgroundOverride,
});
```

The IPC binding in `src/lib/ipc.ts` (or wherever `reconfigureShellTab` is defined) gains the new field — straight serde of `BackgroundOverrideWire`.

### 4. New Tauri command — `pty_rebind_channel`

`src-tauri/src/ipc/commands.rs` — add alongside `pty_start` / `pty_restart`:

```rust
#[tauri::command]
pub async fn pty_rebind_channel(
    state: State<'_, AppState>,
    tab: TabId,
    channel: Channel<String>,
) -> AppResult<()> {
    let registry = state.tabs.lock().await;
    registry.rebind_tab_channel(tab, channel).await
}
```

`TabRegistry::rebind_tab_channel` lives in `src-tauri/src/tabs/registry.rs`:

```rust
pub async fn rebind_tab_channel(
    &self,
    tab: TabId,
    new_channel: Channel<String>,
) -> AppResult<()> {
    let entry = self.entry_for(&tab).ok_or(AppError::NotStarted)?;
    entry.pty.rebind_channel(new_channel).await
}
```

The error type — add `AppError::NoActivePty` (or reuse `NotStarted`) for "no PTY registered for this tab id." The frontend will fall back to a fresh `pty_start` in that case (Step 5b).

### 5. Backend rebind plumbing in `PtyManager`

The processor task at `pty/tasks.rs:107` owns the `Channel<String>` directly. To swap channels without restarting the task, add a control mpsc that the processor's select loop reads from. This is the cleanest option among the three considered (vs. `Arc<Mutex<Channel<String>>>` per-byte locking, or task-cancel-and-respawn which loses `ProcessingLayer` state).

**5a. New control message and channel** in `pty/manager.rs`:

```rust
pub enum ProcessorControl {
    ChannelChange(Channel<String>),
}

struct PtyHandle {
    // ... existing fields ...
    /// Sender for processor control messages. Currently used to swap the
    /// output channel on renderer recreate without restarting the PTY.
    control_tx: mpsc::Sender<ProcessorControl>,
}
```

Construct the channel at `PtyManager::start` time (capacity 4 is plenty — control messages are rare):

```rust
let (control_tx, control_rx) = mpsc::channel::<ProcessorControl>(4);

tasks::spawn_processor(
    tab.clone(),
    bytes_rx,
    output_channel,
    control_rx,    // <-- new
    tts_segments,
    cancel.clone(),
    user_typed_tts,
    state_signals.clone(),
    settings,
);
```

**5b. Processor task swaps its `channel` binding** at `pty/tasks.rs:108`:

```rust
pub fn spawn_processor(
    tab: TabId,
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut channel: Channel<String>,  // mut, not const
    mut control_rx: mpsc::Receiver<ProcessorControl>,
    // ... existing args ...
) {
    tokio::spawn(async move {
        // ... existing setup ...
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                changed = settings_rx.recv() => { /* unchanged */ }
                ctrl = control_rx.recv() => {
                    match ctrl {
                        Some(ProcessorControl::ChannelChange(next)) => {
                            channel = next;
                            // No replay; bytes that arrive between
                            // destroy and rebind sit in `rx`'s queue and
                            // are dispatched against the new channel on
                            // the next iteration. Frontend handles
                            // visual continuity via the serialize
                            // snapshot replay.
                        }
                        None => { /* control sender dropped; treat as cancel */ break }
                    }
                }
                maybe = rx.recv() => { /* unchanged dispatch path */ }
                _ = tick.tick() => { /* unchanged */ }
            }
        }
    });
}
```

**5c. `PtyManager::rebind_channel`** at `pty/manager.rs`:

```rust
impl PtyManager {
    pub async fn rebind_channel(&self, new_channel: Channel<String>) -> AppResult<()> {
        let control_tx = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            handle.control_tx.clone()
        };
        control_tx
            .send(ProcessorControl::ChannelChange(new_channel))
            .await
            .map_err(|_| AppError::Pty("processor task gone".into()))?;
        Ok(())
    }
}
```

Lock held only long enough to clone the sender — the `await` on `send` happens outside the guard, so a slow processor doesn't block the lock.

**Bytes-during-rebind regression test.** Cancel-safety is verified by inspection (Tokio docs + the existing `settings_rx.recv()` arm), but a unit test guards against future regressions:

```rust
#[tokio::test]
async fn channel_rebind_preserves_byte_order() {
    // Spawn processor with channel A.
    // Send bytes B1.
    // Send ChannelChange(channel B).
    // Send bytes B2.
    // Assert: A receives B1, B receives B2, no overlap.
}
```

### 6. `terminals.ts` recreate path — capture geometry, rebind PTY, replay snapshot

Replace the `pty_restart` branch in `queueRecreate` at `terminals.ts:443–456` with the rebind sequence. **Critical:** capture the old terminal's `rows` and `cols` along with the snapshot — the new `Terminal` constructor at `:248` does NOT pass rows/cols today (xterm defaults to 24×80), and `fitAddon.fit()` runs *lazily* on attach via the ResizeObserver / `unsubFont` paths, not during construction. Replaying a snapshot captured at 60×200 cells into a default 24×80 grid wraps badly. Pass the captured geometry to the new constructor so the snapshot replays into a matching grid; fit-on-attach later corrects any minor host-size differences.

```ts
function queueRecreate(tabId: TabId): void {
  const existing = recreateTimers.get(tabId);
  if (existing) clearTimeout(existing);
  recreateTimers.set(
    tabId,
    setTimeout(async () => {
      recreateTimers.delete(tabId);
      const old = entries.get(tabId);
      if (!old) return;

      // V1.4-03 coordination: a user-initiated Restart that fires during
      // the 120ms debounce wins. Skip the recreate if the entry is
      // mid-restart — the restart already replaced the PTY's child, and
      // a recreate-on-top would tear down the freshly-restarted xterm
      // and rebind to a PTY whose bytes channel was just rebound by the
      // restart. See Step 6.5.
      if (old.restarting) return;

      // V1.4-03: capture both the scrollback snapshot AND the geometry.
      // The new Terminal must be constructed at the same rows/cols so
      // the replayed cursor positions land on the right cells.
      const snapshot = old.serializeAddon.serialize();
      const { rows, cols } = old.term;

      destroyTerminal(tabId);
      createTerminal(tabId, {
        rebindPty: true,
        scrollbackSnapshot: snapshot,
        initialGeometry: { rows, cols },
      });
    }, 120),
  );
}
```

`createTerminal` options shape grows:

```ts
export function createTerminal(
  tabId: TabId,
  options: {
    restartPty?: boolean;       // existing; user-initiated Restart action
    rebindPty?: boolean;        // V1.4-03: renderer-category flip
    scrollbackSnapshot?: string; // V1.4-03: replay into the new xterm
    initialGeometry?: { rows: number; cols: number }; // V1.4-03: match snapshot grid
  } = {},
): void { ... }
```

In the `new Terminal({ ... })` call at `:248–258`, thread the geometry through:

```ts
const term = new Terminal({
  fontFamily: display.terminal_font_family,
  fontSize: display.terminal_font_size,
  cursorBlink: true,
  allowProposedApi: true,
  theme: initialTheme,
  ...(initialCategory === 'image' ? { allowTransparency: true } : {}),
  ...(options.initialGeometry ?? {}),  // { rows, cols } when present
});
```

### 6.5. Restart vs. rebind coordination

Today's `queueRecreate` does NOT check `entry.restarting`, and the Restart handler at `terminals.ts:400–419` does NOT clear pending recreate timers. A bug exists today (V1.4-02): if a user clicks Restart during the 120ms recreate debounce, the recreate fires after the restart and tears down the freshly-restarted terminal. Visible only in the unlikely race where a user toggles background image and clicks Restart within 120ms — but with V1.4-03's rebind path the failure mode is worse (rebind to a freshly-respawned PTY, channel rebinds twice, byte ordering scrambled).

Two-way fix:

1. **`queueRecreate` skips on `entry.restarting`** — already shown above. The restart wins; no recreate.
2. **Restart handler clears the recreate timer** — at `terminals.ts:400–419`, add a `recreateTimers.delete(tabId)` + `clearTimeout` block before the existing body. If a recreate is debouncing when the user clicks Restart, drop it.

Both gates are needed because they cover different orderings: (1) covers "recreate fires after restart starts," (2) covers "recreate is queued, restart starts before timer fires."

Add a small unit test in the recreate test suite (Step 9) that asserts a Restart-during-debounce results in exactly one PTY operation (the restart), not two.

`attemptSpawn` gains a third mode:

```ts
async function attemptSpawn(
  entry: TerminalEntry,
  mode: 'start' | 'restart' | 'rebind',
): Promise<void> {
  const channel = bindBytesChannel(entry);
  const { rows, cols } = entry.term;
  try {
    if (mode === 'rebind') {
      try {
        await ptyRebindChannel(entry.tabId, channel);
      } catch (e) {
        // PTY may have died between destroy and rebind. Fall back to a
        // cold start so the tab is still usable; user will see scrollback
        // reset and a brief shell respawn, but the alternative is a dead
        // tab.
        console.warn(`pty_rebind fell back to pty_start for ${entry.tabId}:`, e);
        await ptyStart(entry.tabId, channel, rows, cols);
      }
    } else if (mode === 'restart') {
      await ptyRestart(entry.tabId, channel, rows, cols);
    } else {
      await ptyStart(entry.tabId, channel, rows, cols);
    }
    clearTabError(entry.tabId);
    entry.term.focus();
  } catch (e) { /* unchanged */ }
}
```

Call sites:

- First creation in `createTerminal`: `attemptSpawn(entry, 'start')`.
- Tab-restart-requested handler at `terminals.ts:407`: `attemptSpawn(entry, 'restart')`.
- Recreate path (this milestone): `attemptSpawn(entry, 'rebind')`.

### 7. Scrollback capture-and-restore via `@xterm/addon-serialize`

`package.json` — add `@xterm/addon-serialize` (`^0.13.0`).

`TerminalEntry` gains:

```ts
serializeAddon: SerializeAddon;
```

Loaded in `createTerminal` alongside the existing addons (after line 260's `term.loadAddon(fitAddon)`, before `term.open(host)`):

```ts
import { SerializeAddon } from '@xterm/addon-serialize';
// ...
const serializeAddon = new SerializeAddon();
term.loadAddon(serializeAddon);
```

The snapshot write must happen *after* `term.open(host)` (xterm needs a DOM target before it can process writes) and *before* `attemptSpawn` (so the snapshot lands before any live PTY bytes from the rebind). Inside `createTerminal`, after the existing `term.open(host)` at `:267`:

```ts
term.open(host);

// V1.4-03: replay scrollback snapshot before binding the PTY channel.
// xterm processes its write buffer in order, so the snapshot lands
// before any live bytes that arrive once attemptSpawn resolves.
if (options.scrollbackSnapshot) {
  term.write(options.scrollbackSnapshot);
}
```

**Sequencing.** The full recreate sequence is:

1. (At `queueRecreate`) Capture `serializeAddon.serialize()` and `{ rows, cols }` from the old entry.
2. `destroyTerminal` — tears down the old xterm and unbinds its channel. The PTY is still running; its processor task is suspended on `rx.recv()` because no bytes are arriving from a paused-output tab. (For an actively streaming tab like `tail -F`, bytes accumulate in the reader→processor mpsc.)
3. `createTerminal` constructs the new `Terminal` with old `rows`/`cols`.
4. `term.open(host)` mounts the cell layer.
5. `term.write(snapshot)` enqueues the snapshot replay. xterm's internal write queue is FIFO.
6. `attemptSpawn(entry, 'rebind')` calls `bindBytesChannel` (attaches the new channel's callback to `term.write`), then `pty_rebind_channel(tab, channel)`.
7. Backend's `PtyManager::rebind_channel` sends `ProcessorControl::ChannelChange(new_channel)` over `control_tx`.
8. The processor's select loop wakes, swaps `channel = next`, returns to `rx.recv()`.
9. Pending bytes in `rx` (buffered during the rebind window) drain to the new channel; new live bytes follow.
10. The new channel's `onmessage` callback enqueues those bytes via `term.write`. They land in the queue *after* the snapshot.

**No race**: xterm's write queue is single-threaded and FIFO; the snapshot's final cursor-restore lands before any live byte arrives because the live bytes can't even be enqueued until step 10, which depends on step 6's channel binding. The snapshot enqueue at step 5 is unconditional and synchronous from the caller's point of view.

### 8. Backend control-channel test coverage

`src-tauri/src/pty/manager.rs` or a dedicated test module — new tests:

- **`channel_rebind_preserves_byte_order`** — bytes before rebind reach the old channel; bytes after reach the new; no overlap, no loss.
- **`rebind_with_no_pty_errors`** — `PtyManager::rebind_channel` on an empty `inner` returns `AppError::NotStarted`.
- **`processor_survives_rapid_rebinds`** — three rebinds in quick succession (mimicking a slider drag that crosses image/no-image threshold three times). All succeed; final channel receives subsequent bytes.

Integration test (Tauri command boundary): not strictly necessary for V1.4-03 — the unit tests cover the manager logic, and the command is a thin wrapper. Add if mocking turns out to be cheap.

### 9. Frontend recreate test coverage

`src/lib/terminal/background.test.ts` (or a new `terminals.test.ts` if recreate logic is missing direct coverage):

- **`queueRecreate captures snapshot before destroy`** — mock `serializeAddon.serialize` and `destroyTerminal`; assert serialize is called before destroy.
- **`createTerminal with scrollbackSnapshot writes after open`** — mock `term.write`; assert the snapshot is written before any pty_rebind call resolves.
- **`pty_rebind failure falls back to pty_start`** — mock `ptyRebindChannel` to reject; assert `ptyStart` is called with the same channel.

### 10. README and DESIGN.md

README — replace the "scrollback resets when image toggles" sentence in the Terminal background paragraph with:

> Toggling the image background switches xterm.js renderers cleanly. Your shell session, scrollback, and running processes are all preserved across the switch. The DOM-vs-canvas perf trade-off remains: image-mode terminals run on the slower DOM renderer (2-5× slower for high-throughput output like `tail -F`).

Add a per-tab background sentence near the existing per-tab themes mention:

> The per-tab Background row in Configure Tab works the same way: "Use global default" inherits, "Disabled" forces this tab to plain theme background regardless of global, and "Custom for this tab" gives the tab its own image or color independent of the global setting.

DESIGN.md — extend the Settings section with a paragraph:

> **PTY rebind protocol (V1.4-03).** The xterm.js renderer is decided at Terminal construction (`allowTransparency` and the canvas vs. DOM split are constructor-only). Toggling the image background therefore requires destroying the xterm Terminal and constructing a new one. To preserve the shell session across this destroy/create cycle, cimp uses `pty_rebind_channel` — the PTY and its child process stay alive, only the IPC `Channel<String>` is swapped. `@xterm/addon-serialize` captures a snapshot of the visible scrollback before destroy and replays it into the new xterm after construct. Bytes emitted by the PTY during the rebind window queue in the existing reader→processor mpsc and dispatch to the new channel on the next select-loop iteration. Cross-app-restart scrollback survival is *not* in scope — when cimp exits, the PTYs exit with it. A backend ring buffer would close that gap and is tracked separately.

## Test Plan

- **Unit tests (Rust)** — see Step 8.
- **Unit tests (TS)** — see Step 9. Existing `background.test.ts` 4×3 matrix continues to pass.
- **Manual — per-tab UI happy path:**
  - Open Configure Tab on a Shell tab. Background defaults to "Use global default."
  - Switch to "Custom for this tab" and pick a solid color. Save. The tab's terminal repaints to the chosen color *immediately* (no recreate — color-only change stays on canvas).
  - Open the dialog again — Background row reflects the saved Custom state.
  - Switch to "Disabled." Save. The tab's terminal repaints to its theme background, ignoring any global background.
  - Switch back to "Use global default." Save. Tab inherits global again.
- **Manual — per-tab UI category flip:**
  - With global = solid color, set a per-tab override to Custom with an image. Save. The tab's terminal recreates *but keeps its scrollback* — the previous output is still visible after the renderer flip. Shell prompt is the same one, no `[restarting tab]` banner.
  - In an AI tab (Claude), do the same. The Claude conversation thread continues after the flip — the agent's running state and history survive.
- **Manual — scrollback survival under load:**
  - In a Shell tab, run `seq 1 5000` to fill scrollback. Toggle global background image on. Tab recreates; scroll up, confirm the 5000-line output is still there.
  - Toggle image off. Tab recreates again; scrollback still there.
- **Manual — bytes-during-rebind:**
  - In a Shell tab, run `tail -F /tmp/test.log` (or the Windows equivalent). In another shell, append lines continuously. Toggle global image while lines are streaming. Confirm: no missing lines after the rebind, no duplicate lines, ordering preserved.
- **Manual — rebind fallback:**
  - Kill the PTY child manually (e.g., `kill -9` the shell process from another terminal). Then trigger a renderer flip. Confirm the JS-side fallback to `pty_start` runs cleanly — tab respawns with empty scrollback, no error dialog.
- **Manual — slider drag stress:**
  - With image enabled, drag the opacity slider rapidly. No category flip occurs (opacity changes stay in-place); confirm in-place updates are smooth.
  - Drag from 0% opacity to 100% with image present — no flip. Toggle from "Image" to "Theme default" — single recreate (debounced), scrollback survives.
- **Manual — three-state override interaction:**
  - Global = image. Tab A inherits. Tab B = "Disabled." Tab C = Custom (different image). Confirm each tab renders distinctly.
  - Change global to solid color. Tab A repaints in place to the solid color. Tab B unchanged (still theme bg). Tab C unchanged (still its custom image).

## Files Most Likely Touched

- `src/lib/settings/BackgroundConfigEditor.svelte` (new) — extracted from `SettingsApp.svelte`
- `src/SettingsApp.svelte` — swap inline background controls for the new component
- `src/lib/dialog/ConfigureTabDialog.svelte` — Background section for shell tabs (mirrors theme row)
- `src/lib/settings/TabSettingsSection.svelte` — Background section for AI tabs (mirrors `selectThemeOverride` block at `:61–88`)
- `src/lib/terminals.ts` — `attemptSpawn(entry, 'rebind')` mode, `serializeAddon` on `TerminalEntry`, snapshot capture/replay, `queueRecreate` rewrite
- `src/lib/ipc.ts` (or wherever `reconfigureShellTab` is bound) — `backgroundOverride` arg + new `ptyRebindChannel` binding
- `src-tauri/src/ipc/commands.rs` — `pty_rebind_channel` Tauri command
- `src-tauri/src/ipc/tab_lifecycle.rs` — `background_override` arg on `reconfigure_shell_tab`
- `src-tauri/src/tabs/registry.rs` — `rebind_tab_channel` method
- `src-tauri/src/pty/manager.rs` — `ProcessorControl`, `control_tx` on `PtyHandle`, `PtyManager::rebind_channel`
- `src-tauri/src/pty/tasks.rs` — control mpsc in `spawn_processor` select loop
- `src-tauri/src/error.rs` — possibly `AppError::NoActivePty` (or reuse `NotStarted`)
- `src-tauri/src/settings/schema.rs` — `set_background_override` setter on `TabConfig` if not already present
- `package.json` — `@xterm/addon-serialize` (`^0.13.0`)
- `README.md`, `docs/DESIGN.md` — see Step 10

## Risks and Open Questions

- **`@xterm/addon-serialize` fidelity.** The serialize addon outputs ANSI escapes that recreate cell state. It does NOT perfectly preserve every xterm.js feature — notably, alternate-screen-buffer state (used by `vim`, `less`, `htop`) and certain DEC private modes can have subtle differences after replay. For the renderer-flip use case this is acceptable: most users toggle backgrounds while the shell is at a normal prompt, not mid-`vim`. Document the imperfect-replay caveat in DESIGN.md and add a manual test that confirms `vim`-then-flip-renderer doesn't corrupt the screen badly enough to require user action (worst case: user presses Ctrl+L and the screen redraws cleanly).
- **Memory ceiling for long scrollback.** A user with 50,000 lines of scrollback × 200 cols × 3 bytes ≈ 30 MB per recreate. Capture and replay each cost an allocation. For a single recreate this is fine; for a slider drag that triggers multiple rapid recreates (despite the debounce), GC pressure could show up as a stutter. The 120 ms debounce should keep this under one recreate per drag, but if reports come in, cap snapshot size to the last N lines via `serializeAddon.serialize({ scrollback: 1000 })` instead of unbounded.
- **Rebind during PTY exit.** Verified by inspection: when the child exits, `spawn_waiter` at `pty/tasks.rs:329` calls `cancel.cancel()`, the processor task's `_ = cancel.cancelled() => break` arm fires, the processor exits, and its `mpsc::Receiver<ProcessorControl>` is dropped. `PtyManager::inner` still holds the `PtyHandle` (it's only cleared on explicit `pty_stop`). A `pty_rebind_channel` call after exit therefore reaches `PtyManager::rebind_channel`, finds the handle, but `control_tx.send` fails because the receiver dropped. The error path is mapped to `AppError::Pty("processor task gone".into())` and the frontend falls back to `pty_start`, which is already covered in Step 6's `attemptSpawn(entry, 'rebind')` catch block. Worth a manual test: trigger a renderer flip on a tab whose shell is exiting (`exit` followed immediately by a slider toggle).

## Verified by Inspection (was: open questions)

- **Tokio `select!` cancel-safety.** The existing select loop at `pty/tasks.rs:145` already runs four arms together — `cancel.cancelled()`, `settings_rx.recv()`, `rx.recv()`, `tick.tick()` — and V1.4-01 is shipping. `mpsc::Receiver::recv()` is documented as cancel-safe; the practical proof is that settings updates don't cause byte loss today. Adding a fifth `control_rx.recv()` arm is structurally identical. The `channel_rebind_preserves_byte_order` test in Step 8 covers regressions; the original "two-phase pause/resume" fallback is no longer needed in the plan.
- **AI-tab override persistence.** Verified by inspection: `TabSettingsSection.svelte:37–40` writes via the local `update` helper → parent's `onchange` → `applySettings` (`store.ts:48`) → `settings_update` IPC. `Settings` is serialized whole; any field on `AiToolTabConfig` (including `background_override`, already added in V1.4-02) round-trips automatically. No new Tauri command needed for AI tabs.
- **Snapshot replay vs. geometry.** Verified by inspection: `Terminal` constructor at `terminals.ts:248–258` does NOT pass `rows`/`cols`, so xterm defaults to 24×80. `fitAddon.fit()` runs lazily — only from the `unsubFont` subscription (which skips the initial dispatch via `firstFont`) and the `ResizeObserver` (which fires when the host gets real layout). Step 6 captures the old terminal's `rows`/`cols` and passes them to the new constructor so the snapshot replays into a matching grid before any fit happens.
- **Restart vs. recreate collision.** Verified by inspection: today's `queueRecreate` at `:443–456` does NOT check `entry.restarting`, and the Restart handler at `:400–419` does NOT clear pending recreate timers. This is a latent bug today (V1.4-02) — visible only in the rare race where a user toggles background image and clicks Restart within 120ms, but the failure mode worsens with rebind. Step 6.5 closes both ordering windows: `queueRecreate` skips on `entry.restarting`, and the Restart handler clears the recreate timer.
