# Milestone V4-04: Layout Persistence and Named Presets

## Purpose

Persist the layout tree and focused-pane ID to settings, restore them on app launch, handle integrity edge cases (orphan tabs, missing tabs), and add named layout presets the user can save and restore.

After this milestone, splits and arrangements survive app restarts. The user can also save their favorite arrangements as named presets and switch between them with one click.

Read `DESIGN.md`, M1, M2, M3 first.

## What This Milestone Delivers

1. New top-level `layout` field in the settings schema, holding the serialized layout tree and `focused_pane_id`.
2. Layout persistence: on every layout change (split, close, drag, resize, focus change), the layout state is debounced-saved to settings, identical to other v1+ settings persistence.
3. Layout restoration: on app launch, the layout is loaded from settings; tabs are placed in their persisted panes; focus is restored.
4. v1.2 → v1.3 settings migration: existing v1.2 settings (which have no `layout` field) get a default single-root-pane layout containing all tabs in their `tabs` array order. The v1.2 `session.active_tab_id` becomes the default pane's `active_tab_id`. The migration writes a `config.json.v1.2.bak` backup.
5. Integrity check on load:
   - Orphan tabs (in `settings.tabs` but not in any pane in the loaded layout): added to the focused pane at the end.
   - Missing tabs (referenced by panes in the loaded layout but no longer in `settings.tabs`): silently dropped.
   - Empty panes (containing no tab IDs): collapsed if non-root; replaced with a default tab if root and the tabs array is also empty (shouldn't happen — builtins always exist — but defensive).
6. Named layout presets: a top-level `layout_presets` array. UI to save the current layout as a preset, restore a preset by name, rename, and delete.
7. A "Layouts" menu in the application title bar (or settings window) with: Save current layout..., Recent presets list, Manage presets...
8. Manage Presets dialog: list of all presets with rename and delete buttons.

## What This Milestone Does NOT Do

- No per-project / per-cwd layouts (out of scope for v1.3).
- No automatic layout switching based on context (out of scope).
- No preset import/export (deferred; users can hand-edit settings.json if needed).
- No preset preview thumbnails (deferred; a labels-only list is sufficient for v1.3).

## Implementation Steps

### 1. Settings schema

Add to `src/settings/schema.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutPersisted {
    pub tree: LayoutNodePersisted,
    pub focused_pane_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNodePersisted {
    Split {
        id: String,
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNodePersisted>,
        second: Box<LayoutNodePersisted>,
    },
    Pane {
        id: String,
        tab_ids: Vec<String>,
        active_tab_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutPreset {
    pub name: String,
    pub created_at: String,  // ISO 8601 timestamp
    pub tree: LayoutNodePersisted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    // ...existing fields...
    pub layout: Option<LayoutPersisted>,
    #[serde(default)]
    pub layout_presets: Vec<LayoutPreset>,
}
```

The `Option<LayoutPersisted>` makes the migration logic cleaner (None means "use default single-pane layout"); after migration runs, it's always Some.

### 2. v1.2 → v1.3 migration

Extend `src/settings/migration.rs` with a v1.2 → v1.3 step. Detection: if the schema has `tabs` as an array (v1.2 shape) but no `layout` field, the file is v1.2.

```rust
pub fn migrate_if_needed(value: &mut serde_json::Value, settings_path: &Path) -> Result<(), MigrationError> {
    // ... existing v1.1 → v1.2 ...

    let needs_v1_3 = value.get("tabs").is_some_and(|t| t.is_array())
        && value.get("layout").is_none();

    if needs_v1_3 {
        // 1. Backup
        let backup_path = settings_path.with_extension("json.v1.2.bak");
        if !backup_path.exists() {
            fs::write(&backup_path, serde_json::to_vec_pretty(value)?)?;
        }

        // 2. Build default layout: single root pane with all tabs
        let tabs = value.get("tabs").unwrap().as_array().unwrap();
        let tab_ids: Vec<String> = tabs.iter()
            .filter_map(|t| t.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();

        let active_tab_id = value.get("session")
            .and_then(|s| s.get("active_tab_id"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| tab_ids.first().cloned());

        let pane_id = format!("pane-{}", uuid::Uuid::new_v4());
        let layout = serde_json::json!({
            "tree": {
                "type": "pane",
                "id": pane_id,
                "tab_ids": tab_ids,
                "active_tab_id": active_tab_id,
            },
            "focused_pane_id": pane_id,
        });

        value["layout"] = layout;
        value["layout_presets"] = serde_json::Value::Array(vec![]);

        // 3. Remove the redundant session.active_tab_id field (the layout has it now)
        if let Some(session) = value.get_mut("session").and_then(|s| s.as_object_mut()) {
            session.remove("active_tab_id");
        }
    }

    Ok(())
}
```

Idempotent: running on a v1.3 file (which has `layout`) is a no-op because the detection condition fails.

### 3. Save / load Tauri commands

Add to `src/ipc/mod.rs`:

```rust
#[tauri::command]
async fn save_layout(layout: LayoutPersisted, state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.settings.write().await.layout = Some(layout);
    state.settings.request_debounced_save();
    Ok(())
}

#[tauri::command]
async fn save_layout_preset(name: String, tree: LayoutNodePersisted, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut settings = state.settings.write().await;
    let preset = LayoutPreset { name: name.clone(), created_at: now_iso8601(), tree };
    // Replace if a preset with the same name exists; otherwise append
    if let Some(existing) = settings.layout_presets.iter_mut().find(|p| p.name == name) {
        *existing = preset;
    } else {
        settings.layout_presets.push(preset);
    }
    state.settings.request_debounced_save();
    Ok(())
}

#[tauri::command]
async fn delete_layout_preset(name: String, state: tauri::State<'_, AppState>) -> Result<(), String> { ... }

#[tauri::command]
async fn rename_layout_preset(old_name: String, new_name: String, state: tauri::State<'_, AppState>) -> Result<(), String> { ... }
```

The frontend calls `save_layout` whenever the layout store mutates (debounce on the frontend side too — settle-then-save at 250ms — to avoid spamming the backend during rapid splitter drags).

### 4. Frontend layout-save subscription

In `frontend/src/stores/layout.ts`:

```typescript
// Debounced save
let saveTimeout: number | null = null;
layout.subscribe((state) => {
    if (saveTimeout) clearTimeout(saveTimeout);
    saveTimeout = setTimeout(() => {
        invoke("save_layout", {
            layout: serializeLayout(state),
        });
    }, 250);
});
```

`serializeLayout` converts the frontend `LayoutState` to the backend's `LayoutPersisted` shape (mostly a 1:1 mapping; both use the `type: "split"|"pane"` discriminator).

### 5. Frontend layout-load on launch

In the app's startup sequence, after loading settings:

```typescript
const settings = await invoke<Settings>("load_settings");
const persistedLayout = settings.layout;

if (persistedLayout) {
    const validated = validateAndRepairLayout(persistedLayout, settings.tabs);
    layout.set(validated);
} else {
    // Should only happen if settings is empty / first launch — build a default
    layout.set(defaultLayoutForTabs(settings.tabs));
}
```

`validateAndRepairLayout` performs the integrity check from step 6.

### 6. Integrity check / repair

```typescript
function validateAndRepairLayout(persisted: LayoutPersisted, tabs: TabConfig[]): LayoutState {
    const validTabIds = new Set(tabs.map(t => t.id));

    // Walk the tree, dropping unknown tab IDs from each pane
    let tree = walkPanes(persisted.tree, (pane) => {
        const filtered = pane.tab_ids.filter(id => validTabIds.has(id));
        let active = pane.active_tab_id;
        if (active && !filtered.includes(active)) {
            active = filtered[0] || null;
        }
        return { ...pane, tab_ids: filtered, active_tab_id: active };
    });

    // Find orphans: tabs in settings.tabs but not in any pane
    const placedTabIds = new Set<string>();
    walkPanes(tree, (pane) => { for (const id of pane.tab_ids) placedTabIds.add(id); return pane; });
    const orphans = tabs.filter(t => !placedTabIds.has(t.id)).map(t => t.id);

    // Place orphans in the focused pane (or first pane if focused is invalid)
    let focusedId = persisted.focused_pane_id;
    if (!findPane(tree, focusedId)) {
        focusedId = leftmostLeafPaneId(tree);
    }
    if (orphans.length > 0) {
        tree = walkPanes(tree, (pane) => {
            if (pane.id === focusedId) {
                return { ...pane, tab_ids: [...pane.tab_ids, ...orphans], active_tab_id: pane.active_tab_id ?? orphans[0] };
            }
            return pane;
        });
    }

    // Collapse empty non-root panes
    tree = collapseAllEmptyNonRoot(tree);

    // If the root pane is empty (shouldn't happen because builtins always exist + orphan placement covers it), populate with all tabs
    if (tree.type === "pane" && tree.tab_ids.length === 0) {
        tree.tab_ids = tabs.map(t => t.id);
        tree.active_tab_id = tabs[0]?.id ?? null;
    }

    return { tree, focused_pane_id: focusedId };
}
```

`walkPanes` is a tree-traversal helper that applies a transformation to every pane node and returns a new tree. `collapseAllEmptyNonRoot` recursively closes any empty non-root panes.

### 7. Layouts menu UI

Add a `Layouts` entry to the application's main menu (or as a section in the Settings window if the app has no main menu — Tauri menus are platform-dependent; for a webview-only Layouts UI, a small button in the title bar or a Settings section works).

The simplest path: add a `Layouts` button to the bottom status bar (next to the existing mute/announcements/volume controls). Clicking it opens a small popover with:

- Save current layout as... (opens a name-input prompt)
- (separator)
- Recent presets list (up to 5 most-recent by `created_at`):
  - `Build mode` (Restore)
  - `Review mode` (Restore)
- (separator)
- Manage presets...

Or: add Layouts to the Settings window as a dedicated section. The Settings window already has a Tabs section from M3 (v1.2's M3); adding a Layouts section is consistent.

Pick one. I recommend the bottom-status-bar popover — it's more discoverable and matches the user's likely workflow ("I want to switch layouts mid-session").

### 8. Save preset flow

User clicks "Save current layout as..." → a small modal appears with:

- **Name** input (default: "Layout {N}" where N is the next ordinal)
- **Save** / **Cancel**

On Save: serialize the current layout's tree (without `focused_pane_id` — presets don't carry focus) and call `save_layout_preset`.

If a preset with the same name already exists, prompt to overwrite or rename. (Or: silently overwrite, with the old version available in the pre-save backup. Either is fine — pick one, document.)

### 9. Restore preset flow

User clicks a preset name in the Layouts popover → call `restore_layout_preset(name)` (a frontend function, no backend call needed since the layout state is frontend-managed):

```typescript
function restoreLayoutPreset(name: string) {
    const settings = get(settingsStore);
    const preset = settings.layout_presets.find(p => p.name === name);
    if (!preset) return;

    // Build a layout state from the preset, with integrity check
    const newLayout: LayoutPersisted = {
        tree: preset.tree,
        focused_pane_id: leftmostLeafPaneId(preset.tree),
    };
    const validated = validateAndRepairLayout(newLayout, get(tabsStore));
    layout.set(validated);
    // The debounced save subscription will then persist the new layout
}
```

Orphans (current tabs not in the preset) are placed in the focused pane per the integrity-check rules. Missing tabs (in the preset but no longer in `settings.tabs`) are silently dropped.

### 10. Manage Presets dialog

A modal with:

- A scrollable list of all presets, each showing name + `created_at` (formatted nicely).
- Per-row buttons: **Restore**, **Rename**, **Delete**.
- A close button.

`Rename` triggers an inline edit (same pattern as the tab inline rename from v1.2). `Delete` shows a confirm: "Delete preset '{name}'? This cannot be undone." → `Delete` / `Cancel`.

### 11. Edge case: tab created after preset save

User saves a preset with 3 tabs, then creates a new Shell tab, then restores the preset. The new tab is an orphan in the preset's view. Per the integrity rules, the orphan is placed in the focused pane. UX-wise, this means restoring a preset doesn't *remove* the orphan tab; it adds it to the layout. This is what the user wants — they don't want their new tab to disappear when they switch layouts.

### 12. Edge case: tab deleted after preset save

User saves a preset, deletes a Shell tab, restores the preset. The deleted tab is gone from `settings.tabs`. The integrity check filters it out of the loaded panes. The pane that originally contained it might be empty after filtering — collapsed. The user sees the layout minus the deleted tab.

## Files Touched / Added

**Added:**
- Frontend `components/LayoutsPopover.svelte` (Layouts menu in the bottom status bar)
- Frontend `components/SaveLayoutDialog.svelte`
- Frontend `components/ManagePresetsDialog.svelte`

**Modified:**
- `src/settings/schema.rs` (`LayoutPersisted`, `LayoutNodePersisted`, `LayoutPreset`, top-level `layout` and `layout_presets` fields)
- `src/settings/migration.rs` (v1.2 → v1.3 migration)
- `src/ipc/mod.rs` (`save_layout`, `save_layout_preset`, `delete_layout_preset`, `rename_layout_preset`)
- Frontend `stores/layout.ts` (debounced save subscription, validate-and-repair on load)
- Frontend `stores/layout-presets.ts` (NEW; or fold into `stores/settings.ts`)
- Frontend root component (mount layout from persisted state)
- Frontend bottom status bar (Layouts button)

## Edge Cases and Gotchas

- **Layout save during app shutdown**: the debounce should flush on `beforeunload` (or Tauri's equivalent close-event hook). Otherwise, rapid changes followed by an immediate quit can lose state. Existing v1+ settings already handle this; verify the layout subscription participates.
- **Persisted layout pointing at non-existent panes**: e.g., user manually edited settings.json to remove a pane but kept other panes referring to its ID as a sibling. The deserialization itself can fail. Handle gracefully: catch the deserialization error, log a warning, fall back to a default single-pane layout.
- **Pane and split IDs colliding across migrations / preset restores**: IDs are generated as UUIDs; collisions are negligible. But: when restoring a preset, the preset's tree contains its original IDs. If those IDs collide with the current layout's IDs (e.g., the user saved a preset, kept using cimp, and the current layout reuses the same UUID — extremely unlikely), the post-restore tree is fine because the current layout is replaced wholesale; no merge happens.
- **Backup files accumulating**: each migration writes a backup. v1.1 → v1.2 wrote `config.json.v1.1.bak`; v1.2 → v1.3 writes `config.json.v1.2.bak`. Don't delete; users may want them. Document in the README.
- **Clean install (no settings file at all)**: settings load creates a default file with no layout; the default layout is computed at runtime (single root pane with builtins). On first save (when the user creates a tab or splits a pane), the layout is persisted. Verify cleanly.
- **Preset list ordering**: in the Layouts popover, list "Recent presets" by `created_at` descending. Rename does not update `created_at`. Document.
- **Many presets (10+)**: the popover gets long; the Manage Presets dialog handles bulk operations. Polish for 100+ presets is out of scope.
- **Active tab changes within a pane during a debounced save window**: each active-tab change triggers a layout-store update (because `pane.active_tab_id` changes) which triggers a debounced save. With 250ms debounce, rapid tab clicks coalesce into one save. Acceptable.
- **Splitter resize during debounce window**: same — coalesces. The user might be dragging the splitter for a second straight; the backend gets one save at the end.

## Manual Verification Checklist

Migration:
- [ ] On a Windows or Linux machine with an existing v1.2 settings file (after using v1.2 normally): launch v1.3, verify migration runs, `config.json.v1.2.bak` exists, the loaded layout is single-pane with all tabs.
- [ ] Launch v1.3 again: no double-migration, file unchanged, no new backup.
- [ ] Manually corrupt the layout field in settings.json: launch, app falls back to default layout, warning logged.

Persistence:
- [ ] Create a horizontal split via drag (M2), restart the app: split is restored.
- [ ] Create a complex 3-deep nested split, restart: tree is restored exactly.
- [ ] Resize a splitter, restart: ratio is preserved.
- [ ] Switch focus to a different pane, restart: focused pane is restored.
- [ ] Switch active tab within a pane, restart: per-pane active tabs are restored.
- [ ] Close a Shell tab, restart: the layout reflects the current tab set (closed tab is gone, sibling tabs intact).

Integrity check:
- [ ] Manually edit settings.json: remove a tab from a pane's `tab_ids` but keep it in `settings.tabs`. Launch: orphan tab is added to the focused pane.
- [ ] Manually edit settings.json: add a non-existent tab id to a pane's `tab_ids`. Launch: missing tab is silently dropped, pane has the remaining valid tabs.
- [ ] Manually create an empty non-root pane in the layout. Launch: empty pane is collapsed.

Presets:
- [ ] Open Layouts popover from the bottom status bar.
- [ ] Save current layout as "Test 1": preset appears in Recent presets list.
- [ ] Reset to a single pane (drag everything back into one pane, or use a debug reset).
- [ ] Restore "Test 1" from the popover: the saved layout is restored.
- [ ] Save another preset "Test 2" with a different layout. Switch between Test 1 and Test 2: layouts swap correctly.
- [ ] Open Manage Presets: both presets listed with timestamps.
- [ ] Rename "Test 1" to "Build mode": persists across restart.
- [ ] Delete "Test 2" with confirm: gone after refresh.

Edge cases:
- [ ] Save a preset, create a new tab, restore the preset: new tab appears in the focused pane (orphan handling).
- [ ] Save a preset, delete a tab that was in the preset, restore the preset: missing tab is silently dropped from the restored layout.

## Done Criteria

- All 8 "What This Milestone Delivers" items work.
- All "Manual Verification Checklist" items pass on Windows and Linux.
- v1.2 → v1.3 migration is verified with a real v1.2 settings file.
- Persistence survives across at least 10 app restarts without corruption.
- Integrity check produces sensible recovery from hand-corrupted layouts.
- No regression in v1.2, M1, M2, or M3 behavior.
- `cargo test` passes; migration unit tests pass; integrity-check unit tests pass.
