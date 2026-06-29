# Milestone V1.4-04: Terminal Background — Presets, Live Preview, Robustness, Cross-Restart Scrollback

## Purpose

V1.4-02 (skeleton: schema, four-cell rendering matrix, global UI, migration v1.4 → v1.5) and V1.4-03 (per-tab UI, PTY-rebind scrollback survival across renderer flips) shipped the core terminal-background feature. V1.4-04 closes the loose ends from those milestones' "Does NOT Do" / "Risks and Open Questions" sections in four phases under one umbrella.

The phases, in shipping order:

- **A. Robustness & polish** — bound `serializeAddon.serialize()` size, skip snapshot replay when the alt-screen buffer is active, tune the recreate-debounce stagger for many tabs.
- **B. Background presets** — named `BackgroundConfig` library; save / rename / delete / apply from the global Appearance section and from both per-tab dialog surfaces.
- **C. Live preview in Configure Tab** — per-tab background changes preview in the target terminal in real time while the dialog is open, with cancel-revert.
- **D. Cross-restart scrollback** — backend per-tab PTY ring buffer persisted to disk on graceful exit, replayed via `term.write(snapshot)` on next launch.

A → D in that order because each phase de-risks the next: A's snapshot cap and alt-screen detection bound the memory and correctness envelope that both C's preview-revert and D's launch-time replay rely on; B gives C something concrete to live-preview-apply (preset picks become indistinguishable from manual edits); D is the largest piece and reuses the `term.write(snapshot)` plumbing already exercised by V1.4-03 and re-exercised by C.

Each phase is independently shippable. The doc is one umbrella so the related work stays under one milestone tick rather than fragmenting across V1.4-04 / V1.4-05 / V1.4-06 / V1.4-07; phase boundaries inside the doc are explicit so partial landings (e.g., A+B without C+D) are clean fallback points.

Project-local relative-path resolution for `terminal.background.image` is still deferred — it depends on `FEATURE-config-scope.md` shipping, and is covered by that feature's milestone when it lands.

## What This Milestone Delivers

**Phase A — Robustness & polish**

1. New `terminal.background.snapshot_lines: u32` field (default `2000`). The `serializeAddon.serialize()` call at `terminals.ts:534` becomes `serialize({ scrollback: snapshot_lines })`, capping captured scrollback at the last N rows. Bounds JS-heap allocation on the renderer-flip path under 50k-line scrollback edge cases.
2. Alt-screen-mode detection in `queueRecreate`: when `term.buffer.active.type === 'alternate'` at capture time (i.e., the user is in `vim`, `less`, `htop`, etc.), skip both serialize-capture and snapshot replay. The PTY rebind still preserves the live shell state; the alt-buffer's visible content is dropped (documented).
3. Recreate-debounce widened from a flat 120 ms to a base-180 ms stagger (`180 + min(idx, 5) * 30` ms per tab), keeping mass-recreate stutter under two animation frames at 60 Hz when 6+ tabs flip categories simultaneously.
4. Memory-ceiling test in `src/lib/terminal/background.test.ts` covering 50k-line scrollback × 200 cols at the new cap.

**Phase B — Background presets**

5. New `terminal.background.presets: Vec<BackgroundPreset>` settings field (default `[]`).
6. `BackgroundPreset { name: String, config: BackgroundPresetConfig }` — sister struct `BackgroundPresetConfig` is `TerminalBackgroundSettings` minus the recursive `presets` field, so presets can't contain presets.
7. `BackgroundConfigEditor.svelte` (V1.4-03's extracted component at `src/lib/settings/`) gains a "Load preset…" dropdown above the mode toggle. Selection overwrites the bound `config` with the preset's contents — a one-shot copy; subsequent edits are independent.
8. Settings → Appearance gains "**Save as preset…**" and "**Manage presets…**" buttons next to the existing background subsection.
9. The same dropdown lights up automatically inside the per-tab "Custom for this tab" branch in both `ConfigureTabDialog.svelte` and `TabSettingsSection.svelte` — they already nest `BackgroundConfigEditor` (V1.4-03 step 2). No per-surface plumbing.
10. Settings file migration **v1.5 → v1.6**: stamp `terminal.background.presets: []`. Backup `config.json.v1.5.bak.<ts>`.

**Phase C — Live preview in Configure Tab**

11. Per-tab background changes apply to the target terminal in real time while the dialog is open. Every control change writes through to the global store via `applySettings` and the existing `unsubAppearance` subscription at `terminals.ts` repaints (in place for color/opacity/etc., or via the recreate path for category flips).
12. **Cancel-revert**: dialog open snapshots the original `background_override`; closing without Save restores it. Save is a no-op for the background row (changes already applied) but remains meaningful for other dialog fields.
13. New `terminal.background.preview_category_flips: bool` setting (default `true`). When `false`, the preview only applies in-place changes (color / opacity / blur / size / position / tint); image-path swaps and category flips wait for Save. Lets users with many tabs and a slow machine opt out of slider-driven recreate cascades.

**Phase D — Cross-restart scrollback (PTY ring buffer)**

14. `PtyHandle` gains a `scrollback: Arc<Mutex<VecDeque<u8>>>` ring buffer fed by the existing reader task. Capped at `terminal.scrollback.ring_bytes` (default `262_144` — 256 KB per tab, ~600 lines of dense ANSI).
15. New `pty_get_scrollback(tab) -> Result<String>` Tauri command for diagnostics and external use; the launch-replay path uses an internal API rather than the public command.
16. **Graceful-exit persistence**: a `tauri::RunEvent::ExitRequested` handler walks the tab registry and writes each ring to `<config-dir>/scrollback/<tab-id>.bin`.
17. **Restore on launch**: when `pty_start` runs for a tab whose persisted file exists, the bytes are read once, returned to the frontend, and `term.write(restored)` runs before the live channel binding — same code path V1.4-03 uses for the renderer-flip snapshot replay.
18. **Cleanup**: deleting a tab via the UI also deletes its scrollback file; on launch, orphaned files (whose tab IDs aren't in the current settings) are pruned.
19. New `terminal.scrollback` settings group: `ring_bytes: usize` (default `262144`), `persist: bool` (default `true`), `restore_on_launch: bool` (default `true`).
20. Settings file migration **v1.6 → v1.7**: stamp `terminal.scrollback` defaults *and* `terminal.background.preview_category_flips` (added with Phase C, but the migration is one transform per version bump). Backup `config.json.v1.6.bak.<ts>`.

**Cross-cutting**

21. README updates per phase: a paragraph on presets under the V1.4-02 / V1.4-03 background sections, a positive note about cross-restart scrollback ("your shell history survives a cimp restart"), a heads-up on the preview opt-out for power users.
22. DESIGN.md gains paragraphs on (a) the alt-screen replay trade-off, (b) the on-disk scrollback format and its lifecycle, (c) the cancel-revert protocol for live preview.

## Key Deltas vs V1.4-03

- **Two settings-version bumps in one milestone.** V1.4-01 / -02 / -03 each had at most one. Phase B is v1.5 → v1.6; Phase D is v1.6 → v1.7. Migration cascade tests grow accordingly: a v1.4 file lands at v1.7 with three new backups (`config.json.v1.4.bak.<ts>` and `config.json.v1.5.bak.<ts>` and `config.json.v1.6.bak.<ts>`). A v1.2 cold-start lands at v1.7 with five backups across the cascade. Naming follows existing convention: the backup is named for the version being upgraded *from*.
- **First per-tab runtime state persisted to disk outside settings.json.** Until V1.4-04, `<config-dir>` held only `settings.json` and rotation backups. Phase D adds `<config-dir>/scrollback/<tab-id>.bin` files. New disk-cleanup concerns (orphan pruning on launch, cleanup on tab deletion) are addressed in step D.5.
- **First dialog-mediated feature that bypasses Save.** V1.4-01 (themes) and V1.4-03 (background per-tab UI) both batched changes through Save. Phase C's live preview applies to the running terminal *before* Save, then reverts on Cancel. Cancel-revert needs an explicit snapshot-of-original on dialog open. The persisted settings are unchanged until Save; only the *displayed* state of the target terminal changes during the open dialog.
- **Phase A's snapshot cap is the smallest change with the highest leverage.** A single `serialize({ scrollback: N })` argument bounds memory across both V1.4-03's renderer-flip path and Phase C's live-preview path and Phase D's persisted-snapshot file size (Phase D's ring buffer is independent of `snapshot_lines`, but the symmetry is intentional — both paths cap at user-controllable limits).
- **Phase D's ring buffer is independent of V1.4-03's `serializeAddon` snapshot.** Both paths end at `term.write(snapshot)` in the new xterm, but the snapshot source differs:
  - Renderer-flip (V1.4-03): `serializeAddon.serialize()` on the *outgoing* xterm — produces synthetic ANSI escapes that recreate cell state from xterm's internal grid model.
  - Cross-restart (V1.4-04 D): raw PTY bytes captured *before* xterm parsing — the same bytes the live shell originally emitted, replayed verbatim.

  This means Phase D survives PTY exit (which kills xterm and its serialize state) but Phase D doesn't replay alt-screen-buffer content (because the raw bytes include the alt-screen-enter escape but the buffer state was xterm-side, not PTY-side). Same Ctrl+L caveat as Phase A.2.
- **Phase B's recursion problem is solved by a sister struct, not a runtime invariant.** A naive `BackgroundPreset { name, config: TerminalBackgroundSettings }` lets `config.presets` exist (because `TerminalBackgroundSettings` itself contains `presets`) — a runtime "presets-don't-have-presets" invariant. Sister struct `BackgroundPresetConfig` (the same fields minus `presets`) makes the constraint structural. Two extra type definitions plus `From`/`Into` impls are worth the simpler invariant.
- **Live preview reuses V1.4-02's existing live-update plumbing.** V1.4-02 step 5b's `unsubAppearance` subscription already repaints any tab when its resolved background changes. Phase C's mechanism is just "write through to settings on every dialog control change instead of batching." No new subscription wiring — the settings store is already the source of truth for live updates.

## What This Milestone Does NOT Do

- **Project-local relative-path resolution for `terminal.background.image`** (and now also for paths inside presets). Still pending `FEATURE-config-scope.md`. Until then, all paths are absolute.
- **Animated/video backgrounds.** Still out of scope per V1.4-02.
- **Cross-app-restart alt-screen-buffer state.** D's ring buffer captures raw PTY bytes; the alt buffer's contents were xterm-side. A user who exits cimp while in `vim` will, on restore, see the bytes that took them into the alt screen but not the buffer's contents. Document; no fix.
- **Snapshot compression on disk.** Phase D's on-disk format is uncompressed bytes. At 256 KB cap × 20 tabs = 5 MB on disk. Acceptable; revisit only if reports surface.
- **Per-preset image-file copy.** Presets reference image paths by absolute path same as the live config. Saving a preset doesn't snapshot the image into cimp data dir. If the user moves the image, the preset breaks — same way the live config would. Documented in the preset save dialog's helper text.
- **Sharing presets across machines.** Presets live in the global settings.json. Project-local presets (when `FEATURE-config-scope.md` ships) inherit naturally; cross-machine export is a separate feature.
- **Live preview on the global Settings → Appearance page.** Phase C is per-tab-dialog only. The global page already updates live (V1.4-02 wired this) and has no Cancel button; if the user changes the global background and decides they don't like it, they manually change it back. Adding cancel-revert to the global page is a different shape (no dialog-open lifecycle to snapshot from) and out of scope.
- **Asymmetric preview opt-out.** The opt-out is global (one bool in settings). Per-tab opt-out for preview ("preview on Claude tab but not on shells") is unwarranted complexity for a polish toggle.

## Implementation Steps

The phases are ordered A → B → C → D and each is independently shippable. Within each phase, sub-steps run sequentially.

### Phase A — Robustness & polish

#### A.1 Snapshot size cap

Add `snapshot_lines: u32` to `TerminalBackgroundSettings` in `src-tauri/src/settings/schema.rs:622`:

```rust
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TerminalBackgroundSettings {
    // ... existing fields ...
    pub snapshot_lines: u32,
}

impl Default for TerminalBackgroundSettings {
    fn default() -> Self {
        Self {
            // ... existing ...
            snapshot_lines: 2000,
        }
    }
}
```

Mirror in `src/lib/settings/types.ts`:

```ts
export interface TerminalBackgroundSettingsWire {
  // ... existing ...
  snapshot_lines: number;
}
```

`defaultSettings()` in `src/lib/settings/store.ts` adds `snapshot_lines: 2000`.

Update the serialize call at `src/lib/terminals.ts:534`:

```ts
const cap = get(settingsStore).terminal.background.snapshot_lines;
const snapshot = old.serializeAddon.serialize({ scrollback: cap });
```

Schema version stays at v1.5 — `#[serde(default)]` plus the `Default` impl means existing v1.5 files without the field deserialize to `2000` automatically. No migration. (Phase B's migration to v1.6 is the first version bump in this milestone; Phase A piggybacks on serde-default.)

#### A.2 Alt-screen-mode handling

xterm.js exposes `term.buffer.active.type` as `'normal' | 'alternate'`. When the user is in a fullscreen-TUI app (vim, less, htop, top) the alt buffer is active.

In `queueRecreate` at `src/lib/terminals.ts:518`, gate the snapshot capture on the buffer type:

```ts
const wasAltScreen = old.term.buffer.active.type === 'alternate';
const snapshot = wasAltScreen
  ? null
  : old.serializeAddon.serialize({ scrollback: cap });
const { rows, cols } = old.term;
```

`createTerminal`'s `scrollbackSnapshot` option (V1.4-03 step 6) tolerates `null`: when null, skip the `term.write(snapshot)` step. The new xterm comes up blank; the live PTY bytes (whatever the alt-screen app emits next) repaint it on first input or screen-redraw.

The user-facing contract: toggling background mid-`vim` clears the visible TUI. Press `Ctrl+L` (vim's redraw shortcut) or close-and-reopen the editor. **Shell scrollback is unaffected** — the PTY rebind preserves the underlying shell session.

Document in `docs/DESIGN.md` under the V1.4-03 PTY-rebind paragraph: replace "your shell session, scrollback, and running processes are all preserved" with a more accurate variant noting the alt-screen exception.

#### A.3 Recreate-debounce stagger

Today's `queueRecreate` at `src/lib/terminals.ts:518` uses a flat `setTimeout(..., 120)`. With 6+ tabs all inheriting a global background, a global category flip queues 6 timers that all fire in the same animation frame.

Replace with a base-and-stagger formula:

```ts
function queueRecreate(tabId: TabId): void {
  const existing = recreateTimers.get(tabId);
  if (existing) clearTimeout(existing);

  // Spread mass recreates across two animation frames at 60 Hz so a
  // global category flip with N tabs doesn't destroy and rebuild all
  // N xterms in the same frame. Cap stagger at 5 (150ms) so 20-tab
  // worst case is ~330ms total instead of 720ms.
  const tabs = Array.from(entries.keys());
  const idx = tabs.indexOf(tabId);
  const stagger = Math.min(idx, 5) * 30;
  const delay = 180 + stagger;

  recreateTimers.set(tabId, setTimeout(() => { /* ... */ }, delay));
}
```

The base 180 ms is still well under the user-perceptible-lag threshold (~250 ms) and gives the JS event loop more room when a slider drag fires multiple updates.

#### A.4 Tests

`src/lib/terminal/background.test.ts`:

```ts
describe('snapshot cap', () => {
  it('passes snapshot_lines to serializeAddon.serialize', () => {
    // mock serializeAddon; assert .serialize({ scrollback: N }) called
    // with N from settings.
  });

  it('skips serialize when alt-screen buffer is active', () => {
    // mock term.buffer.active.type === 'alternate'; assert serialize
    // is not called and createTerminal is invoked with
    // scrollbackSnapshot: null.
  });

  it('staggers recreate timers across tabs', () => {
    // queue recreate for 3 tabs in quick succession; assert delays
    // 180 / 210 / 240 ms.
  });
});
```

A 50k-line memory test isn't feasible in the unit suite (jsdom doesn't render pixels) — covered by manual test plan instead.

### Phase B — Background presets

#### B.1 Schema additions

`src-tauri/src/settings/schema.rs`. Two new types and one new field:

```rust
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct BackgroundPresetConfig {
    pub image: Option<PathBuf>,
    pub color: Option<String>,
    pub opacity: f32,
    pub blur: u32,
    pub size: BackgroundSize,
    pub position: String,
    pub snapshot_lines: u32,
}

impl Default for BackgroundPresetConfig {
    fn default() -> Self {
        // mirrors TerminalBackgroundSettings::default minus presets
        Self {
            image: None,
            color: None,
            opacity: 0.4,
            blur: 0,
            size: BackgroundSize::Cover,
            position: "center".to_string(),
            snapshot_lines: 2000,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct BackgroundPreset {
    pub name: String,
    pub config: BackgroundPresetConfig,
}

// Add to TerminalBackgroundSettings:
#[serde(default)]
pub presets: Vec<BackgroundPreset>,
```

`From` / `Into` impls bridge the sister structs:

```rust
impl From<&TerminalBackgroundSettings> for BackgroundPresetConfig {
    fn from(s: &TerminalBackgroundSettings) -> Self {
        Self {
            image: s.image.clone(),
            color: s.color.clone(),
            opacity: s.opacity,
            blur: s.blur,
            size: s.size,
            position: s.position.clone(),
            snapshot_lines: s.snapshot_lines,
        }
    }
}

// Inverse goes through TerminalBackgroundSettings { presets: vec![], ..from_preset }.
impl From<BackgroundPresetConfig> for TerminalBackgroundSettings {
    fn from(p: BackgroundPresetConfig) -> Self {
        Self {
            image: p.image,
            color: p.color,
            opacity: p.opacity,
            blur: p.blur,
            size: p.size,
            position: p.position,
            snapshot_lines: p.snapshot_lines,
            presets: Vec::new(),
        }
    }
}
```

The sister struct is also what `BackgroundOverride::Custom(...)` should arguably wrap — but changing that breaks the v1.5 wire format (the existing `BackgroundOverride::Custom(TerminalBackgroundSettings)` round-trips an object with an absent `presets` field that defaults to `[]`, which is equivalent in practice). Don't change `BackgroundOverride` in V1.4-04; the empty `presets: []` riding inside an override is harmless.

#### B.2 TS mirror

`src/lib/settings/types.ts`:

```ts
export interface BackgroundPresetConfigWire {
  image: string | null;
  color: string | null;
  opacity: number;
  blur: number;
  size: 'cover' | 'contain' | 'tile';
  position: string;
  snapshot_lines: number;
}

export interface BackgroundPresetWire {
  name: string;
  config: BackgroundPresetConfigWire;
}

export interface TerminalBackgroundSettingsWire {
  // ... existing ...
  presets: BackgroundPresetWire[];
}
```

`defaultSettings()` adds `presets: []`.

#### B.3 Migration v1.5 → v1.6

`src-tauri/src/settings/migration.rs`. Pattern matches v1.4 → v1.5 at `:491`:

```rust
fn looks_v1_5(value: &Value) -> bool {
    // Has terminal.background (v1.5) but lacks terminal.background.presets (v1.6).
    let Some(obj) = value.as_object() else { return false };
    obj.get("terminal")
        .and_then(|t| t.get("background"))
        .map(|bg| bg.get("presets").is_none())
        .unwrap_or(false)
}

fn migrate_v1_5_to_v1_6(value: &mut Value) {
    let Some(root) = value.as_object_mut() else { return };
    if let Some(bg) = root.get_mut("terminal")
        .and_then(|t| t.as_object_mut())
        .and_then(|t| t.get_mut("background"))
        .and_then(Value::as_object_mut)
    {
        bg.insert("presets".to_string(), json!([]));
    }
}
```

Add `migrate_if_needed` branch after the v1.4 → v1.5 block, with backup at `config.json.v1.5.bak.<ts>`.

Tests (mirroring V1.4-02's coverage):
- `v1_5_to_v1_6_adds_empty_presets_array`
- `v1_6_file_is_not_re_detected`
- `v1_2_cascades_through_v1_3_v1_4_v1_5_and_v1_6` — four backups.

#### B.4 BackgroundConfigEditor — "Load preset…" dropdown

`src/lib/settings/BackgroundConfigEditor.svelte`. The component takes `config: TerminalBackgroundSettingsWire` (V1.4-03 step 1). Add a top-of-component dropdown that reads from the settings store and writes to the bound `config`:

```svelte
<script lang="ts">
  import { settings as settingsStore } from '../store';

  export let config: TerminalBackgroundSettingsWire;
  // ... existing bgMode state ...

  function loadPreset(name: string): void {
    if (!name) return;
    const presets = $settingsStore.terminal.background.presets;
    const preset = presets.find((p) => p.name === name);
    if (!preset) return;
    // Sister-struct-to-full-settings inverse: preset.config has no
    // .presets field; the bound config keeps its existing .presets.
    config = {
      ...config,
      ...preset.config,
    };
    // Re-derive bgMode from the loaded preset (this is a deliberate
    // user action — the c6e3e8a "snapshot doesn't reset bgMode"
    // protection applies to settings refreshes, not to explicit user
    // intent).
    bgMode = config.image ? 'image' : (config.color ? 'color' : 'theme');
  }
</script>

<div class="preset-row">
  <select on:change={(e) => { loadPreset(e.currentTarget.value); e.currentTarget.value = ''; }}>
    <option value="">Load preset…</option>
    {#each $settingsStore.terminal.background.presets as p}
      <option value={p.name}>{p.name}</option>
    {/each}
  </select>
</div>
```

Resetting the select to `''` after each pick keeps the dropdown a one-shot picker rather than a persistent state indicator (the editor's mode toggle is the source of truth for what's currently configured).

#### B.5 Save / manage presets UI

Settings → Appearance, next to the existing background subsection:

```svelte
<div class="preset-actions">
  <button on:click={openSavePresetDialog}>Save as preset…</button>
  <button on:click={openManagePresetsDialog}>Manage presets…</button>
</div>
```

`openSavePresetDialog` opens a small modal asking for a name; on confirm:

```ts
async function savePreset(name: string): Promise<void> {
  const updated = structuredClone($settingsStore);
  if (updated.terminal.background.presets.some((p) => p.name === name)) {
    showError(`A preset named "${name}" already exists.`);
    return;
  }
  const { presets, ...rest } = updated.terminal.background;
  updated.terminal.background.presets = [
    ...presets,
    { name, config: rest as BackgroundPresetConfigWire },
  ];
  await applySettings(updated);
}
```

`openManagePresetsDialog` opens a modal listing each preset with rename / delete actions. Inline editing of names; delete with confirm. All mutations write through `applySettings`.

Save modal helper text: "Presets reference image paths by absolute location — moving an image file breaks any preset that uses it."

#### B.6 Per-tab preset apply

Already automatic: `ConfigureTabDialog.svelte` and `TabSettingsSection.svelte` both nest `BackgroundConfigEditor` inside the "Custom for this tab" branch (V1.4-03 step 2). The dropdown added in B.4 lights up there with no per-surface change.

#### B.7 Tests

Rust:
- `BackgroundPresetConfig` round-trips through serde with all field types.
- `From<&TerminalBackgroundSettings>` and `From<BackgroundPresetConfig>` impls are inverse on the shared subset.
- Migration v1.5 → v1.6 idempotent and stamps `presets: []`.

TypeScript:
- `loadPreset` overwrites only the shared fields; `config.presets` is untouched.
- `savePreset` rejects duplicate names.
- `savePreset` strips `presets` from the saved config (no recursion).

### Phase C — Live preview in Configure Tab

#### C.1 Snapshot original state on dialog open

`src/lib/dialog/ConfigureTabDialog.svelte` (shell tabs) and `src/lib/settings/TabSettingsSection.svelte` (AI tabs). Both already initialize from `get(settingsStore).tabs.find(...)` at open time (V1.4-03 step 2). Add an explicit snapshot:

```ts
let originalBackgroundOverride: BackgroundOverrideWire | null = null;

function initFields(tabId: TabId): void {
  // ... existing init ...
  const liveTab = get(settingsStore).tabs.find((t) => t.id === tabId);
  originalBackgroundOverride = liveTab?.background_override ?? null;
}
```

For `TabSettingsSection.svelte`, which uses Svelte 5 runes, the snapshot lives in component state initialized in the same place that mirrors `settings.background_override` today.

#### C.2 Write-through control changes

The V1.4-03 mode-change handler `selectBgOverride` writes to a local variable and waits for Save. Change to write through immediately:

```ts
async function selectBgOverride(next: BgOverrideMode): Promise<void> {
  bgOverrideMode = next;
  let nextOverride: BackgroundOverrideWire | null;
  if (next === '__inherit') nextOverride = null;
  else if (next === '__disabled') nextOverride = 'disabled';
  else {
    // '__custom': seed from existing override, or fall back to global.
    if (backgroundOverride && typeof backgroundOverride === 'object') {
      nextOverride = backgroundOverride;
    } else {
      const liveGlobal = get(settingsStore).terminal.background;
      // Strip presets when descending into an override (the override
      // is a config, not a config-with-presets).
      const { presets, ...rest } = liveGlobal;
      nextOverride = rest as TerminalBackgroundSettingsWire;
    }
  }
  await writeBackgroundOverride(tabId, nextOverride);
  backgroundOverride = nextOverride;
}

async function writeBackgroundOverride(
  tabId: TabId,
  next: BackgroundOverrideWire | null,
): Promise<void> {
  const updated = structuredClone(get(settingsStore));
  const tab = updated.tabs.find((t) => t.id === tabId);
  if (!tab) return;
  tab.background_override = next;
  await applySettings(updated);
}
```

Inside the `BackgroundConfigEditor` nested in the "Custom for this tab" branch, every individual control change (color picker, opacity slider, etc.) flows back through the bound `config` — V1.4-03's structure already does this. Add a write-through subscription so each binding update propagates to the store:

```svelte
{#if bgOverrideMode === '__custom' && backgroundOverride && typeof backgroundOverride === 'object'}
  <BackgroundConfigEditor
    bind:config={backgroundOverride}
    on:change={() => writeBackgroundOverride(tabId, backgroundOverride)}
  />
{/if}
```

`BackgroundConfigEditor` dispatches a `change` event after every internal write to its `config` prop. (Today the component mutates the bound object and Svelte's reactivity does the rest; making the change event explicit lets the dialog opt into the write-through.)

The `unsubAppearance` subscription at `terminals.ts:362-407` (V1.4-02 step 5b) already picks up these store mutations and either repaints in place or queues a recreate. **No `terminals.ts` changes for live preview** — the existing live-update path is the live-preview path.

#### C.3 Cancel-revert

Both dialog surfaces have a close handler. Wire it to the original snapshot:

```ts
async function close(saved: boolean): Promise<void> {
  if (!saved) {
    await writeBackgroundOverride(tabId, originalBackgroundOverride);
  }
  dispatchClose();
}
```

`Save` is now a no-op for the background row (the changes have already been applied), but the dialog still has other fields (theme, env, etc.) that batch through Save normally. Keep the existing `save()` function; just stop including `background_override` in its payload (it's already persisted by the write-through path).

For the AI-tab surface (`TabSettingsSection.svelte`), there is no Save button — that surface auto-saves all fields today. Cancel-revert there means binding to a different lifecycle: the user closes the Settings panel without committing. Two options:

- (a) Track when the user opens the Settings panel for a tab and snapshot then. Closing the panel without an explicit "Apply" reverts.
- (b) AI tabs don't get cancel-revert — every change sticks immediately.

Plan: ship (b) for AI tabs. The AI-tab settings UX is already auto-save, and adding a panel-level revert breaks that pattern. Document: "AI-tab background changes apply immediately and persist; use the per-tab Background dropdown to revert manually." The shell-tab dialog's cancel-revert is the headline feature here.

#### C.4 Preview opt-out

New setting:

```rust
// schema.rs, alongside other terminal.background fields.
#[serde(default)]
pub preview_category_flips: bool,
```

`Default` impl: `preview_category_flips: true`.

In the dialog write-through path, gate category-flipping changes:

```ts
async function writeBackgroundOverride(
  tabId: TabId,
  next: BackgroundOverrideWire | null,
): Promise<void> {
  const previewFlips = get(settingsStore).terminal.background.preview_category_flips;
  if (!previewFlips) {
    // Compare resolved category before / after; if different, defer
    // to Save.
    const currentMode = effectiveBackgroundMode(/*...*/);
    const nextMode = effectiveBackgroundMode(/*... with `next` ...*/);
    if (categoryOf(currentMode) !== categoryOf(nextMode)) {
      // Stash the pending change in dialog state; Save commits it.
      pendingBackgroundOverride = next;
      return;
    }
  }
  // Normal in-place preview path.
  // ...
}
```

The Save handler then writes any `pendingBackgroundOverride` through. UI feedback: when a pending change exists, badge the Save button "Save (preview off — reload required)." Helper text on the per-tab Background row notes: "Preview is off in Settings → Appearance. Image changes apply on Save."

Settings → Appearance gains a "**Preview category flips in Configure Tab**" checkbox under the existing background controls. Default on.

The migration for this field rides Phase D's v1.6 → v1.7 transform (one transform per version bump; both new fields land together). Until Phase D ships, `preview_category_flips` is added with a serde default and behaves correctly without a version bump.

#### C.5 Tests

`src/lib/dialog/ConfigureTabDialog.test.ts` (or new):
- Opening the dialog snapshots `background_override`.
- A control change writes through to the store within one tick.
- Closing without Save restores the original `background_override`.
- Closing with Save leaves the post-edit `background_override` in place.
- With `preview_category_flips: false`, an image-toggle change is deferred to Save.

### Phase D — Cross-restart scrollback (PTY ring buffer)

#### D.1 Settings group

`src-tauri/src/settings/schema.rs`:

```rust
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ScrollbackSettings {
    pub ring_bytes: usize,
    pub persist: bool,
    pub restore_on_launch: bool,
}

impl Default for ScrollbackSettings {
    fn default() -> Self {
        Self {
            ring_bytes: 262_144,    // 256 KB per tab
            persist: true,
            restore_on_launch: true,
        }
    }
}

// In TerminalSettings:
pub scrollback: ScrollbackSettings,
```

TS mirror in `src/lib/settings/types.ts` and `defaultSettings()` accordingly.

#### D.2 Migration v1.6 → v1.7

`src-tauri/src/settings/migration.rs`:

```rust
fn looks_v1_6(value: &Value) -> bool {
    // Has terminal.background.presets (v1.6) but lacks
    // terminal.scrollback (v1.7).
    let Some(root) = value.as_object() else { return false };
    let has_presets = root.get("terminal")
        .and_then(|t| t.get("background"))
        .and_then(|bg| bg.get("presets"))
        .is_some();
    let has_scrollback = root.get("terminal")
        .and_then(|t| t.get("scrollback"))
        .is_some();
    has_presets && !has_scrollback
}

fn migrate_v1_6_to_v1_7(value: &mut Value) {
    let Some(root) = value.as_object_mut() else { return };
    if let Some(terminal) = root.get_mut("terminal").and_then(Value::as_object_mut) {
        terminal.insert("scrollback".to_string(), json!({
            "ring_bytes": 262144,
            "persist": true,
            "restore_on_launch": true,
        }));
        // Phase C's preview_category_flips lives under terminal.background.
        if let Some(bg) = terminal.get_mut("background").and_then(Value::as_object_mut) {
            bg.entry("preview_category_flips").or_insert(json!(true));
        }
    }
}
```

Add `migrate_if_needed` branch. Backup at `config.json.v1.6.bak.<ts>`. Tests mirror v1.5 → v1.6's pattern.

#### D.3 Ring buffer in `PtyManager`

`src-tauri/src/pty/manager.rs`:

```rust
pub struct PtyHandle {
    // ... existing fields ...
    pub scrollback: Arc<TokioMutex<VecDeque<u8>>>,
    pub scrollback_cap: usize,
}
```

At `PtyManager::start`:

```rust
let scrollback_cap = settings.read().terminal.scrollback.ring_bytes;
let scrollback = Arc::new(TokioMutex::new(VecDeque::with_capacity(scrollback_cap)));
let scrollback_for_reader = Arc::clone(&scrollback);
```

The reader task at `src-tauri/src/pty/tasks.rs` (the one that copies bytes from the PTY master into `bytes_tx`) gains a sibling write:

```rust
async fn reader_task(
    mut master: PtyMaster,
    bytes_tx: mpsc::Sender<Vec<u8>>,
    scrollback: Arc<TokioMutex<VecDeque<u8>>>,
    scrollback_cap: usize,
    cancel: CancellationToken,
) {
    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            n = master.read(&mut buf) => {
                let n = match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let bytes = buf[..n].to_vec();

                // Existing path: feed processor.
                if bytes_tx.send(bytes.clone()).await.is_err() { break }

                // V1.4-04 D: feed ring buffer (cap-bounded).
                {
                    let mut ring = scrollback.lock().await;
                    ring.extend(&bytes);
                    while ring.len() > scrollback_cap {
                        ring.pop_front();
                    }
                }
            }
        }
    }
}
```

Lock contention: minimal. The reader is the only writer; the persistence path and the launch-replay path are the only readers and run at known times (graceful exit, cold start). `tokio::sync::Mutex` because the lock is held across awaits in some paths — though the reader's lock is short, persistence-path reads can be long.

#### D.4 `pty_get_scrollback` Tauri command

`src-tauri/src/ipc/commands.rs`:

```rust
#[tauri::command]
pub async fn pty_get_scrollback(
    state: State<'_, AppState>,
    tab: TabId,
) -> AppResult<Vec<u8>> {
    let registry = state.tabs.lock().await;
    let entry = registry.entry_for(&tab).ok_or(AppError::NotStarted)?;
    let ring = entry.pty.scrollback.lock().await;
    let (a, b) = ring.as_slices();
    let mut out = Vec::with_capacity(a.len() + b.len());
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    Ok(out)
}
```

Returns raw bytes. The frontend converts to a string via `TextDecoder('utf-8', { fatal: false })` before passing to `term.write` (xterm tolerates incomplete UTF-8 sequences mid-string; the lossy conversion is fine for ANSI-escape replay).

This command is exposed but not used by the launch-replay path itself (which uses an internal API for efficiency — D.6). It exists for diagnostics and future external use.

#### D.5 Persist on graceful exit

In `src-tauri/src/main.rs` (or wherever the Tauri builder is configured):

```rust
.on_window_event(/* ... */)
.run(tauri::generate_context!())
```

becomes:

```rust
let app = tauri::Builder::default()
    .setup(/* ... */)
    .build(tauri::generate_context!())?;

app.run(|handle, event| {
    if let tauri::RunEvent::ExitRequested { .. } = event {
        let state = handle.state::<AppState>();
        if state.settings.read().terminal.scrollback.persist {
            // Block on persist; we're in the exit path.
            tauri::async_runtime::block_on(async {
                if let Err(e) = persist_all_scrollback(&state).await {
                    eprintln!("scrollback persist failed: {e:?}");
                }
            });
        }
    }
});
```

`persist_all_scrollback`:

```rust
async fn persist_all_scrollback(state: &AppState) -> std::io::Result<()> {
    let dir = settings::config_dir().join("scrollback");
    std::fs::create_dir_all(&dir)?;
    let registry = state.tabs.lock().await;
    for (tab_id, entry) in registry.iter() {
        let ring = entry.pty.scrollback.lock().await;
        let (a, b) = ring.as_slices();
        let path = dir.join(format!("{}.bin", tab_id));
        let mut f = std::fs::File::create(&path)?;
        std::io::Write::write_all(&mut f, a)?;
        std::io::Write::write_all(&mut f, b)?;
    }
    Ok(())
}
```

Crash safety: `ExitRequested` doesn't fire on hard kill (SIGKILL, power loss, taskkill). Those paths lose the scrollback. Acceptable — this is best-effort recovery, not durable storage. Document in DESIGN.md.

#### D.6 Restore on launch

When `pty_start` runs for a tab, check for a persisted file before spawning. Modify `pty_start` (or wrap with a thin `pty_start_with_restore`) to return the restored bytes:

```rust
#[tauri::command]
pub async fn pty_start(
    state: State<'_, AppState>,
    tab: TabId,
    channel: Channel<String>,
    rows: u16,
    cols: u16,
) -> AppResult<PtyStartResult> {
    let restored = if state.settings.read().terminal.scrollback.restore_on_launch {
        try_read_persisted_scrollback(&tab).ok().flatten()
    } else {
        None
    };
    state.pty_start_inner(tab.clone(), channel, rows, cols).await?;
    // After successful start, delete the persisted file so it doesn't
    // replay twice on a subsequent crash-restart cycle.
    if restored.is_some() {
        let _ = delete_persisted_scrollback(&tab);
    }
    Ok(PtyStartResult { restored_scrollback: restored })
}

#[derive(Serialize)]
pub struct PtyStartResult {
    pub restored_scrollback: Option<Vec<u8>>,
}
```

Frontend `src/lib/terminals.ts`'s `attemptSpawn` for `mode === 'start'` checks the result:

```ts
const result = await ptyStart(entry.tabId, channel, rows, cols);
if (result.restored_scrollback) {
  const decoded = new TextDecoder('utf-8', { fatal: false }).decode(
    new Uint8Array(result.restored_scrollback),
  );
  entry.term.write(decoded);
}
```

xterm's write queue is FIFO (V1.4-03 step 7); the restored snapshot lands before any live PTY bytes from the freshly-started shell. The user sees their previous session's output, then a fresh prompt below it.

The restored bytes also seed the ring for the new PTY:

```rust
// In pty_start_inner, after creating the new ring:
if let Some(bytes) = &restored_for_seed {
    let mut ring = scrollback.lock().await;
    ring.extend(bytes);
    while ring.len() > scrollback_cap { ring.pop_front(); }
}
```

So a user who restarts cimp twice in a row preserves continuity across both restarts (truncated by the cap on the second restart, naturally).

#### D.7 Cleanup

**On tab deletion via UI** — wire a cleanup call into the existing tab-deletion path (`src-tauri/src/ipc/tab_lifecycle.rs`'s tab-removal command):

```rust
let _ = delete_persisted_scrollback(&tab_id);
```

`delete_persisted_scrollback` ignores not-found errors.

**Orphan pruning on launch** — once at startup, after settings load:

```rust
fn prune_orphan_scrollback(known_tab_ids: &HashSet<TabId>) -> std::io::Result<()> {
    let dir = settings::config_dir().join("scrollback");
    if !dir.exists() { return Ok(()) }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let parsed: Result<TabId, _> = stem.parse();
        match parsed {
            Ok(id) if !known_tab_ids.contains(&id) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => { let _ = std::fs::remove_file(&path); }  // unparseable filename
            _ => {}
        }
    }
    Ok(())
}
```

#### D.8 Tests

Rust:
- `ring_buffer_caps_at_ring_bytes` — write 300 KB into a 256 KB ring, assert size == 256 KB and contents == last 256 KB.
- `persist_round_trip` — start a PTY, emit some bytes, persist, read back from disk, assert bytes match the ring.
- `restore_on_launch_replays_then_deletes_file` — file exists pre-launch, post-launch result contains bytes, file is gone.
- `orphan_pruning_deletes_unknown_files` — drop a `bogus-id.bin` into the dir, run prune, assert removed.
- `crash_skips_persist` — spawn a PTY, simulate hard exit (drop the app handle without RunEvent::ExitRequested), assert no file written. (Edge case; lock down the contract.)

TypeScript:
- `pty_start_with_restored_scrollback_writes_before_live_bytes` — mock the new `PtyStartResult.restored_scrollback`, assert `term.write` runs before `bindBytesChannel`'s callback.

Manual:
- Start cimp, run `seq 1 200` in a Shell tab. Exit cimp gracefully (window close). Restart. The Shell tab restores with the 200 lines visible above the new prompt.
- Start cimp, run `seq 1 200000` (way over the 256 KB cap). Exit. Restart. The last few thousand lines are visible; earlier ones are gone (cap behavior).
- Disable `terminal.scrollback.persist`. Run a session, exit, restart — no restoration; tab starts fresh. Confirm no file was written either.
- Hard-kill cimp (Task Manager / `taskkill /F`). Restart. Tab starts fresh; no file or stale file from previous session (no contract guarantee for crash; documented).

### Phase ordering and dependency notes

- **A → B**: A's `snapshot_lines` field is on `TerminalBackgroundSettings`. B's `BackgroundPresetConfig` sister struct includes `snapshot_lines`, so B must read A's schema as final. If A and B land separately, ship A first.
- **B → C**: C's "Custom for this tab" branch nests `BackgroundConfigEditor`, which gains the preset dropdown in B. Functionally C works without B (no presets to load), but the doc's flow assumes B is in.
- **C → D**: C's `preview_category_flips` field rides D's v1.6 → v1.7 migration. If C and D land separately, ship C first with `#[serde(default)]` only — when D lands later, the migration retroactively stamps the field on existing files (no behavior change because serde-default already gave them `true`).
- **D's settings group is independent** of A/B/C's `terminal.background` work. D could ship before C if order changes, but the in-doc ordering (D last) is right because D is the largest piece.

## Test Plan

### Phase A
- **Unit (TS)** — see A.4. snapshot_lines is passed; alt-screen skips capture; stagger formula correct.
- **Manual** — Run `tail -F` on a 50k-line log. Toggle global background image. Confirm: no perceptible JS-heap stutter, `snapshot_lines` honored (last 2000 visible after recreate, earlier lines lost).
- **Manual — alt-screen** — Open `vim`. Toggle global background image. Confirm: vim's screen clears; `Ctrl+L` redraws; `:q` exits to a shell prompt with prior shell scrollback intact.
- **Manual — mass recreate** — 8 Shell tabs all inheriting global, toggle global image. Confirm: visible stagger (tabs flip in two-frame waves rather than simultaneously); no dropped frames.

### Phase B
- **Unit (Rust)** — see B.7.
- **Manual — save / load preset** — Configure a non-trivial background (image + opacity 0.5 + blur 10), Save as preset "Frosted." Open a per-tab Custom branch, Load preset "Frosted," confirm fields populate identically.
- **Manual — duplicate name rejection** — Save "Frosted" twice; second save shows error.
- **Manual — manage presets** — Rename "Frosted" → "Glass," confirm rename in dropdowns. Delete, confirm dropdown clears.
- **Manual — preset survives migration cascade** — Hand-build a v1.4 file, launch cimp, confirm v1.4 → v1.5 → v1.6 cascade lands at v1.6 with `presets: []`.

### Phase C
- **Unit (TS)** — see C.5.
- **Manual — happy path** — Open Configure Tab on a Shell tab. Drag opacity slider; the tab's terminal repaints in real time. Click Cancel; opacity reverts.
- **Manual — Save commit** — Same dialog, slide opacity, click Save. Reopen the dialog; the saved value appears.
- **Manual — category flip preview** — In the dialog, switch from Solid to Image. Tab recreates (V1.4-03 path); preview shows. Cancel; tab recreates back to original (Phase A's debounce makes this two recreates with visible scrollback survival each time).
- **Manual — preview opt-out** — Set `terminal.background.preview_category_flips: false`. In the dialog, switch to Image. Tab does *not* recreate. Save; *now* it recreates. Cancel before Save; tab unchanged.
- **Manual — AI tab no-revert behavior** — In a Claude tab's settings panel, change background. Close the panel without explicit revert. Change persists. (Documented behavior.)

### Phase D
- **Unit (Rust)** — see D.8.
- **Manual — happy path** — Run shell commands in a tab, exit cimp gracefully, relaunch. Tab restores scrollback above a fresh prompt. Live commands work normally afterward.
- **Manual — cap behavior** — `seq 1 200000`, exit, relaunch. Last few thousand lines visible; counter near 200000 at the bottom.
- **Manual — opt-out** — `terminal.scrollback.persist: false`, exit, relaunch — no restoration, no file written. Re-enable, exit, relaunch — restoration works.
- **Manual — orphan prune** — Delete a tab via UI, confirm `<config-dir>/scrollback/<tab-id>.bin` is gone. Drop a fake `99999999-9999-9999-9999-999999999999.bin` into the dir, restart cimp, confirm pruned.
- **Manual — alt-screen on restore** — Run `vim`, exit cimp. Relaunch. The tab restores the bytes that *led into* vim, but vim's buffer content is gone (Ctrl+L doesn't recover it because the alt buffer was xterm-side, not PTY-side). Documented behavior.
- **Manual — double restart** — Restore once, run more commands, exit, relaunch. Continuity preserved across both restarts (truncated by cap).

## Files Most Likely Touched

**Phase A**
- `src-tauri/src/settings/schema.rs` — `snapshot_lines` field + Default
- `src/lib/settings/types.ts`, `src/lib/settings/store.ts` — TS mirror
- `src/lib/terminals.ts` — serialize cap, alt-screen gate, debounce stagger
- `src/lib/terminal/background.test.ts` — A.4 tests
- `docs/DESIGN.md` — alt-screen caveat under V1.4-03 paragraph

**Phase B**
- `src-tauri/src/settings/schema.rs` — `BackgroundPresetConfig`, `BackgroundPreset`, `presets` field, `From`/`Into` impls
- `src-tauri/src/settings/migration.rs` — v1.5 → v1.6 transform + tests
- `src/lib/settings/types.ts`, `src/lib/settings/store.ts` — TS mirror, defaults
- `src/lib/settings/BackgroundConfigEditor.svelte` — "Load preset…" dropdown
- `src/SettingsApp.svelte` (or wherever the Appearance section lives) — Save / Manage buttons + their modals
- `src/lib/settings/PresetManagementDialog.svelte` (new) — manage modal
- `README.md` — preset paragraph

**Phase C**
- `src-tauri/src/settings/schema.rs` — `preview_category_flips` field
- `src/lib/settings/types.ts`, `src/lib/settings/store.ts` — TS mirror
- `src/lib/dialog/ConfigureTabDialog.svelte` — original snapshot, write-through, cancel-revert, opt-out gate
- `src/lib/settings/TabSettingsSection.svelte` — write-through (no cancel-revert per C.3)
- `src/lib/settings/BackgroundConfigEditor.svelte` — `change` event dispatch on internal config writes
- `src/lib/dialog/ConfigureTabDialog.test.ts` — C.5 tests
- `docs/DESIGN.md` — cancel-revert paragraph

**Phase D**
- `src-tauri/src/settings/schema.rs` — `ScrollbackSettings`, `terminal.scrollback` field
- `src-tauri/src/settings/migration.rs` — v1.6 → v1.7 transform (also stamps Phase C's `preview_category_flips`)
- `src-tauri/src/pty/manager.rs` — `scrollback` ring on `PtyHandle`, ring-buffer wiring at start
- `src-tauri/src/pty/tasks.rs` — reader-task ring write
- `src-tauri/src/ipc/commands.rs` — `pty_get_scrollback`, modified `pty_start` returning `PtyStartResult`
- `src-tauri/src/ipc/tab_lifecycle.rs` — cleanup-on-tab-delete
- `src-tauri/src/main.rs` — `RunEvent::ExitRequested` handler, orphan prune at startup
- `src-tauri/src/settings/persistence.rs` — `config_dir().join("scrollback")` path helper
- `src/lib/terminals.ts` — `attemptSpawn` for `'start'` mode reads `result.restored_scrollback` and writes before live binding
- `src/lib/ipc.ts` — updated `ptyStart` binding signature returning `PtyStartResult`
- `README.md` — cross-restart scrollback paragraph
- `docs/DESIGN.md` — on-disk format and lifecycle paragraph

## Risks and Open Questions

### Phase A
- **Snapshot cap user expectation** — A user with 50k lines of scrollback who toggles background expects to keep all 50k. The cap (default 2000) means they lose 48k. Mitigation: the default is a reasonable trade-off (xterm's own scrollback default is 1000), the field is user-tunable, and the cross-app-restart cap in Phase D is similar (256 KB ≈ 600 lines), so the renderer-flip cap aligns with a constraint users will see anyway. Document the default and the field in README.
- **Alt-screen detection edge case** — Some legacy programs use private-mode escapes (`ESC[?47h` instead of `ESC[?1049h`) that may not all flip `term.buffer.active.type`. Verify against vim, less, htop, top, tmux. If a particular program's alt mode isn't detected, the snapshot will replay garbled — the failure mode is visible (broken screen), the user presses Ctrl+L. Acceptable.

### Phase B
- **Image path portability across machines** — A preset that references `C:\Users\Amir\images\frost.jpg` is useless on a different machine. Phase B doesn't address this; it's the same constraint as the live config. The "save preset" dialog notes this. When `FEATURE-config-scope.md` ships and presets can live in project-local files, relative-path resolution helps — but cross-machine *global* presets stay broken. Acceptable; matches prior behavior.
- **Recursion via `BackgroundOverride::Custom(TerminalBackgroundSettings)`** — `BackgroundOverride::Custom` wraps `TerminalBackgroundSettings` (V1.4-02 final shape), which now contains `presets: Vec<BackgroundPreset>`. So a per-tab Custom override technically carries a `presets` array, which is meaningless (presets live globally). The `From<&TerminalBackgroundSettings>` for `BackgroundPresetConfig` strips presets. The wire format pays the cost of an empty `presets: []` riding inside every Custom override — ~13 bytes. Document; don't change `BackgroundOverride::Custom` to wrap `BackgroundPresetConfig` because that's a wire-format break.

### Phase C
- **Live preview during a slider drag** — A user dragging the opacity slider fires 60 settings updates per second. Each one runs through `applySettings` → backend serialization → frontend store update → `unsubAppearance` → `term.options.theme = ...`. The IPC roundtrip is the cost. V1.4-02 step 8 already debounces global-page slider IPC at 150 ms; Phase C should debounce the dialog's slider IPC similarly. Untested under high-tab-count load — flag as risk, debounce-tune if reports surface.
- **Preview state leaks if dialog crashes** — If the dialog component throws between "snapshot original" and "wire close handler," the user is left with an unwanted in-place change. Mitigation: the close handler is set up before any interactive control mounts; component-level error boundaries (Svelte's `onerror`) call the revert path. Add a manual test: forcibly throw inside the dialog and confirm revert.
- **Cancel-revert vs. external settings change during dialog open** — If the user has the Configure Tab dialog open and *also* a Settings → Appearance window, and they edit the global background while the dialog is open, the per-tab override is unaffected (it's a different field) — no conflict. But if they change the *per-tab* override from somewhere else (e.g., a future scriptable settings API), the original snapshot becomes stale and a Cancel reverts to a value the user no longer wanted. No real surface for this today; flag as latent risk if/when scriptable settings ship.

### Phase D
- **Ring-buffer mid-byte truncation produces broken UTF-8 / ANSI** — When the ring overflows, `pop_front` removes bytes one at a time until cap is met. If a multi-byte UTF-8 codepoint or a multi-byte ANSI escape sequence is straddling the truncation point, the leading bytes get dropped while the trailing bytes survive. xterm tolerates broken sequences (drops them visually), and `TextDecoder({ fatal: false })` replaces orphan bytes with `U+FFFD`. So replay shows a brief glitch at the start of the scrollback, then settles into normal output. Acceptable; the alternative (truncate to next codepoint boundary) adds complexity for a one-frame-of-display benefit.
- **PTY rebinds and ring buffer interaction** — V1.4-03's PTY rebind keeps the same `PtyHandle` (and thus the same ring) across renderer flips. Good — no special handling. But if a user-initiated Restart fires (`pty_restart`), the PTY's child is killed and respawned but the same `PtyHandle` persists. Should the ring carry over? Plan: clear the ring on restart (the user explicitly asked to restart; old scrollback is no longer relevant). Add a `clear_scrollback` step to `pty_restart`'s respawn flow. Document.
- **Hard-kill loses scrollback even with persist enabled** — `RunEvent::ExitRequested` doesn't fire on SIGKILL / Task Manager / power loss. Users who hard-kill cimp and complain that scrollback didn't restore should see a short README note: this is best-effort recovery, not durable storage. A future enhancement could write the ring to disk periodically (every N bytes or N seconds), trading I/O for crash robustness; defer until the use case is real.
- **Restored bytes seeding the new ring is lossy on the cap boundary** — When restoring 256 KB into a 256 KB ring, the ring starts full. Subsequent live bytes immediately push out the restored bytes. After ~256 KB of new output, the restored bytes are gone. Acceptable — by then the user has had plenty of new context — but worth mentioning in DESIGN.md.

## Followups Tracked Elsewhere

- **Project-local relative paths** — `FEATURE-config-scope.md` covers `terminal.background.image` paths and presets' image paths.
- **Scrollback compression on disk** — Future-features candidate if 256 KB × 20 tabs becomes an issue.
- **Periodic ring persistence for crash robustness** — Future-features candidate if hard-kill scrollback loss becomes a felt pain.
- **Scrollback restoration UI indicator** — A faint "↑ restored from previous session" line above the restored content. Polish; defer until users ask.
