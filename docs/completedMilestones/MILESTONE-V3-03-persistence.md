# Milestone V3-03: Tab Persistence and Settings Migration

## Purpose

Reshape the `tabs` settings schema from v1.1's two-key object (`tabs.claude` / `tabs.aider`) into v1.2's ordered array (`tabs: [...]`), implement a one-way migration from the v1.1 shape, and persist user-created Shell tabs (configuration, order, active-tab restoration) across app launches. After this milestone, Shell tabs created via M2's UI survive app restarts.

The Settings window's Tabs section also lands here — the canonical place to view and edit tab configuration in detail. The right-click → Configure dialog from M2 remains as a shortcut into the same UI.

Read `DESIGN.md` (especially the "Settings Schema Changes" and "Tab Persistence" sections), `MILESTONE-V3-01`, and `MILESTONE-V3-02` first.

## What This Milestone Delivers

1. The new `tabs` array schema replaces the old `tabs.{claude,aider}` object.
2. A v1.1 → v1.2 migration that runs once on first launch with an old settings file, transforms the shape in place, and writes a backup of the pre-migration file.
3. The `_shell_1_tmp` interim key from M1 is removed by the migration; Shell 1's settings (notification text) move into its array entry.
4. A startup integrity check that ensures both builtins are present in the loaded array; if missing, prepended with defaults.
5. `create_shell_tab` writes the new tab into the persisted array (debounced).
6. `close_tab`, `rename_tab`, and `reconfigure_shell_tab` update the persisted array (debounced).
7. Per-tab spawn config (command, args, cwd, env) is read from settings at launch — no more per-tab side table; settings is the source of truth.
8. `session.active_tab_id` field tracks the active tab and is restored on launch.
9. Command-not-found at launch handling: a tab whose `command` no longer resolves is registered in the closed state with `closed_exit_code: None` and a "Shell command not found" message in the closed overlay.
10. A Settings window Tabs section listing all tabs, with edit controls per tab (the same fields as the Configure dialog for Shell tabs, plus the existing v1.1 fields for builtins).

## What This Milestone Does NOT Do

- No `{code}` placeholder interpolation in the `exited` notification text (M4).
- No env var UI (out of scope for v1.2; the schema field exists and is preserved across migrations, but no UI surfaces it).
- No drag-to-reorder in the Settings window's Tabs section (out of scope).
- No restart-shell context menu action (M4).
- No multi-version migration chain (e.g., v1.0 → v1.2). v1.1's startup already handled v1.0 → v1.1; M3 only handles v1.1 → v1.2.

## Implementation Steps

### 1. Define the v1.2 schema in Rust

In `src/settings/schema.rs`, define the v1.2 tabs entry types:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabConfig {
    AiTool(AiToolTabConfig),
    Shell(ShellTabConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiToolTabConfig {
    pub id: String,
    pub ai_tool_kind: AiToolKind,
    #[serde(default = "default_true")]
    pub builtin: bool,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub tts_injection: TtsInjectionConfig,
    pub notifications: AiNotificationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellTabConfig {
    pub id: String,
    #[serde(default)]
    pub builtin: bool,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub notifications: ShellNotificationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiNotificationConfig {
    pub idle: String,
    pub awaiting_permission: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellNotificationConfig {
    pub error: String,
    pub exited: String,
}
```

The v1.2 `Settings` struct's `tabs` field is now `Vec<TabConfig>`. The discriminator key (`kind: "ai_tool"` vs `kind: "shell"`) makes deserialization unambiguous.

A new `Session` struct holds the active-tab pointer:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(default)]
    pub active_tab_id: Option<String>,
}
```

Added as a top-level field on `Settings`.

### 2. Implement v1.1 → v1.2 migration

Create `src/settings/migration.rs`. Strategy: at load time, read the JSON file as a `serde_json::Value` first (untyped), inspect the `tabs` field's shape, and branch on it.

```rust
pub fn migrate_if_needed(value: &mut serde_json::Value, settings_path: &Path) -> Result<(), MigrationError> {
    let needs_migration = match value.get("tabs") {
        Some(Value::Object(_)) => true,   // v1.1 shape
        Some(Value::Array(_)) => false,   // already v1.2
        _ => false,                        // missing or wrong type — fall through to defaults
    };

    if !needs_migration { return Ok(()); }

    // 1. Backup
    let backup_path = settings_path.with_extension("json.v1.1.bak");
    fs::write(&backup_path, serde_json::to_vec_pretty(value)?)?;

    // 2. Transform
    let old_tabs = value.get("tabs").cloned().unwrap_or(Value::Object(Default::default()));
    let mut new_tabs = Vec::new();

    if let Some(claude) = old_tabs.get("claude") {
        let mut entry = transform_ai_tool_entry(claude, "claude", "claude_code", "Claude")?;
        new_tabs.push(entry);
    } else {
        new_tabs.push(default_claude_entry());
    }

    if let Some(aider) = old_tabs.get("aider") {
        let entry = transform_ai_tool_entry(aider, "aider", "aider", "Aider")?;
        new_tabs.push(entry);
    } else {
        new_tabs.push(default_aider_entry());
    }

    // 3. Migrate the M1 interim Shell tab settings
    if let Some(shell_tmp) = value.get("_shell_1_tmp") {
        new_tabs.push(transform_shell_1_from_interim(shell_tmp)?);
    } else {
        new_tabs.push(default_shell_1_entry());
    }

    value["tabs"] = Value::Array(new_tabs);
    value.as_object_mut().unwrap().remove("_shell_1_tmp");

    Ok(())
}
```

The `transform_ai_tool_entry` helper:

1. Reads the v1.1 entry's fields (`extra_cli_flags`, `tts_injection`, `notifications`).
2. Sets `id`, `ai_tool_kind`, `builtin: true`, `name` from the function args.
3. Maps `command` to the tool's canonical command (`"claude"` / `"aider"`).
4. Sets `args` to the v1.1 `extra_cli_flags` value (collapsing `extra_cli_flags` into `args`, per the design doc).
5. Carries through `tts_injection`, `notifications` verbatim.

`transform_shell_1_from_interim`:

1. Generates a fresh ID like `shell-{uuid}` for Shell 1.
2. Picks up the M1 hardcoded defaults (auto-detected shell command + args).
3. Reads the M1 `_shell_1_tmp.notifications` for error/exited text.

After migration, the next debounced settings save writes the v1.2 shape to disk. The `.v1.1.bak` file remains untouched as a recovery option.

### 3. Wire migration into settings load

In `src/settings/mod.rs`'s load function:

```rust
pub fn load(path: &Path) -> Result<Settings, SettingsError> {
    let bytes = fs::read(path).or_else(|_| Ok(b"{}".to_vec()))?;
    let mut value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Object(Default::default()));

    migration::migrate_if_needed(&mut value, path)?;

    let mut settings: Settings = serde_json::from_value(value)?;
    integrity_check(&mut settings);
    Ok(settings)
}
```

The integrity check ensures both builtins exist in the array; if missing, prepend with defaults. Logs a warning when this happens (corrupted or hand-edited file).

### 4. Builtin defaults

Define the default entries for Claude, aider, and Shell 1 as functions returning `TabConfig`. These are used by the migration step (when an entry is missing from the old file) and by the integrity check (when an entry is missing from the v1.2 file).

The Claude default: `command: "claude"`, args empty, cwd null, env empty, tts_injection enabled with v1.1's TTS markup instructions, notifications matching v1.1 defaults.

The aider default: `command: "aider"`, args empty, tts_injection disabled, notifications matching v1.1 defaults.

The Shell 1 default: `command: <auto-detected default shell>`, args matching the platform default, no cwd, no env, `error: "Shell encountered an error"`, `exited: "Shell exited (code {code})"`.

For Shell 1's default ID: rather than a fixed string, generate `shell-default-1` once and persist it. Or use a fixed reserved id like `shell-default-1` so the integrity check can identify "the original Shell 1." Reserved IDs are fine since they're prefixed `shell-default-` and user-created tabs use `shell-{uuid}` which won't collide.

### 5. Read tab spawn config from settings at launch

In `src/main.rs`, replace the M1/M2 launch path that hardcodes Claude/aider/Shell 1 spawns. The new flow:

```rust
let settings = settings::load(&settings_path)?;
for tab_config in &settings.tabs {
    spawn_tab_from_config(tab_config, &state_manager, &processing_factory).await?;
}
```

`spawn_tab_from_config`:

1. Build the `TabState` from the config (kind, id, name).
2. Resolve `command`: if the file doesn't exist or isn't on PATH, register the tab in the closed state with `closed_exit_code: None` and a special "command not found" message (see step 8). Skip the PTY spawn for this case.
3. Otherwise, call `pty::spawn(...)` with the configured command/args/cwd/env.
4. Wire processing layer + reader task.
5. Register in the state manager.

The per-tab side table from M2 (`tab_id → ShellSpec`) becomes redundant — settings is the source of truth. Replace `restart_shell_tab`'s lookup to read from settings instead of the side table.

### 6. Persistence on lifecycle operations

Update the Tauri commands from M2:

- `create_shell_tab`: after successful spawn and TabState registration, append a `ShellTabConfig` to `settings.tabs` and request a debounced save.
- `close_tab`: after removing the TabState, remove the matching entry from `settings.tabs` (by id) and request a debounced save.
- `rename_tab`: after updating the TabState's name, also update `settings.tabs[i].name` and request a debounced save.
- `reconfigure_shell_tab`: update `settings.tabs[i]` fields (name, command, args, cwd, env) and request a debounced save.

The debounce is the existing v1 mechanism (~500ms after last change). No new debounce machinery.

### 7. Active-tab persistence

Add a Tauri command `set_active_tab(tab_id)` (or use the existing tab-switch event handler) that updates `settings.session.active_tab_id` and requests a debounced save. The frontend already calls something equivalent for tab switching; just route it through settings now.

At launch, after spawning all tabs:

```rust
let active_id = settings.session.active_tab_id
    .as_ref()
    .filter(|id| settings.tabs.iter().any(|t| t.id() == **id))
    .cloned()
    .unwrap_or_else(|| settings.tabs[0].id().to_string());
state_manager.set_active(active_id);
```

If the persisted ID doesn't match any current tab (e.g., the user manually edited settings to remove that tab), fall back to the first tab.

### 8. Command-not-found at launch

In `spawn_tab_from_config`, when command resolution fails:

1. Register the TabState with `closed: true`, `closed_exit_code: None`.
2. Set a tab-level field for the closed message: `closed_message: Option<String>`. Defaults to None, indicating the standard "Shell exited (code N)" message. Set it to `Some("Shell command not found: {command}. Reconfigure or close this tab.")` when the command resolution failed.

The `ClosedShellOverlay.svelte` from M1 reads `closed_message` and displays it in place of the standard message when present. The Enter-to-restart handler should detect this special case: pressing Enter on a "command not found" tab does *not* try to respawn (it'll just fail again). Instead, Enter opens the Configure Tab dialog so the user can fix the command.

Alternative considered: just attempt the spawn anyway and let it fail with a normal SubprocessExited code. Rejected because the failure mode for "binary doesn't exist" varies by platform (ConPTY can hang, certain shells produce confusing errors), and the up-front check gives a cleaner UX.

### 9. Settings window Tabs section

Add a new section in the settings window: `frontend/src/components/settings/SettingsTabsSection.svelte`. Layout:

- A list of tabs in their current order, each row showing:
  - Name
  - Kind badge (`AI` / `Shell`)
  - Command summary (e.g., `bash --login -i` for Shell tabs)
  - For builtins: `Edit` button → opens an inline expanded view with the v1.1 builtin fields (extra_cli_flags collapsed into args, tts_injection, notifications). Same fields v1.1 surfaced under `tabs.claude` / `tabs.aider` settings UI, just relocated.
  - For user Shell tabs: `Edit` button → opens the Configure Tab dialog (same component as right-click → Configure from M2).
- No add/remove buttons in this section in v1.2 (creation is via the `+` button on the tab bar; deletion via the close button or `Ctrl+W`).
- A reorder affordance is *not* present in v1.2 (out of scope).

The v1.1 settings window had separate sections for Claude and aider tab settings. M3 collapses these under the new Tabs section. Update the settings window's nav/index accordingly.

### 10. Backup file rotation policy

When the migration backup is written (step 2), don't overwrite an existing backup. If `config.json.v1.1.bak` already exists (e.g., the user somehow rolled back and re-migrated), keep the original and write the new one as `config.json.v1.1.bak.{timestamp}`. This is paranoid but cheap.

## Files Touched / Added

**Added:**
- `src/settings/migration.rs`
- Frontend `components/settings/SettingsTabsSection.svelte`

**Modified:**
- `src/settings/schema.rs` (new tabs array shape, SessionState, removal of v1.1 keys)
- `src/settings/mod.rs` (load-time migration call, integrity check)
- `src/main.rs` (read tabs from settings; remove hardcoded spawns; handle command-not-found)
- `src/state/manager.rs` (closed_message field; Enter-on-not-found routing)
- `src/ipc/mod.rs` (`create_shell_tab` / `close_tab` / `rename_tab` / `reconfigure_shell_tab` write to settings; `set_active_tab` if not already)
- Backend `src/state/manager.rs`'s `restart_shell_tab` (reads spawn config from settings instead of side table)
- Frontend `ClosedShellOverlay.svelte` (handle `closed_message` field; route Enter to Configure dialog when command not found)
- Frontend settings window root component (Tabs section in nav)
- Frontend `stores/settings.ts` (new schema typing)

**Removed:**
- The per-tab spawn config side table from M2 (replaced by settings).
- The `_shell_1_tmp` interim settings key (migrated away).
- The v1.1 separate Claude/aider sections in the settings window (folded into Tabs section).

## Edge Cases and Gotchas

- **Migration runs but the subsequent save fails**: keep the in-memory v1.2 shape; the next debounced save retries. Don't silently revert to v1.1. If the file is unwritable for an extended period (permission issues), the user sees v1.2 behavior in-session but loses changes on restart — log a clear warning.
- **Migration on a corrupted JSON file**: `serde_json::from_slice` fails. Treat as if the file didn't exist: reset to defaults, write the v1.2 shape. Back up the corrupted file as `config.json.corrupted.{timestamp}.bak`.
- **Migration runs twice**: the schema discriminator (`tabs` is array vs object) makes the migration a no-op the second time. Verify by running the app twice with a v1.1 file.
- **A v1.2 settings file with extra unknown fields**: serde with `#[serde(default)]` on optional fields and no `deny_unknown_fields` is forgiving. Unknown fields are dropped on save (which the user may not want for forward compatibility). Acceptable for v1.2; revisit if forward-compat becomes a concern.
- **The migration's backup write fails (e.g., disk full)**: fail the migration loudly. Don't proceed without a backup. The user sees an error dialog: "Settings migration could not back up your existing config. Free disk space and restart."
- **A user-edited settings file with a builtin entry having `builtin: false`**: the integrity check ignores the field on builtins (force `builtin: true` for `claude` / `aider` IDs). This handles users intentionally trying to delete builtins via settings — they fail, but cleanly.
- **A user creates a tab, then editing settings.json by hand removes the entry while cctts is running**: cctts doesn't watch the file for external changes in v1.2 (this matches v1.1 behavior). The user's hand-edit is overwritten on the next debounced save. Acceptable; document.
- **`active_tab_id` points to a closed shell**: closed Shell tabs are still real tabs (with a closed overlay). The active-tab restoration honors them; the user sees the closed overlay on launch and can press Enter to restart.
- **`active_tab_id` points to a tab that hits "command not found" at launch**: same as above — the tab exists in closed state, restoration works, the user sees the "command not found" message.
- **Switching schema mid-debounce**: a tab is created (creates a debounced write), the user closes it before the debounce fires. The final state should be no tab in the array. This is the standard "last write wins" behavior of the debounce; verify it works as expected (the close handler should mutate the in-memory settings before the debounced write fires).
- **Two cctts instances writing to the same settings file**: not supported in v1.2 (and not supported in v1.1 either). Document.
- **Settings file location**: use the existing v1 path (`%APPDATA%\<app>\config.json` on Windows, `~/.config/<app>/config.json` on Linux). Don't change paths during a migration; that adds complications.

## Manual Verification Checklist

Migration path:

- [ ] On a Windows machine with an existing v1.1 settings file: launch v1.2, verify the migration runs, the file is now in v1.2 shape, a `config.json.v1.1.bak` exists, and Claude / aider / Shell 1 are present in the tab bar with their v1.1 settings preserved (including any custom `extra_cli_flags` now in `args`).
- [ ] Same on Linux.
- [ ] Launch v1.2 a second time: file is unchanged (no double-migration), no new backup written, behavior identical.
- [ ] Manually corrupt the settings file (e.g., truncate to half), launch: app starts with defaults, corrupted file backed up.

Tab persistence:

- [ ] Create three new Shell tabs via the `+` button: Shell 2, Shell 3, Shell 4.
- [ ] Restart the app: all four user tabs (Shell 1, Shell 2, Shell 3, Shell 4) reappear in the same order.
- [ ] Switch to Shell 3 and quit: relaunch, Shell 3 is active.
- [ ] Rename Shell 2 to "Build Watch": restart, name persists.
- [ ] Configure Shell 3 to point at `wsl.exe` (Windows) with args `["-d", "Ubuntu"]`: save, exit Shell 3, press Enter to restart, the new shell uses WSL. Restart the app: still uses WSL.
- [ ] Close Shell 4: restart, Shell 4 stays gone.
- [ ] Close all user-created Shell tabs except Shell 1: restart, only Claude / aider / Shell 1 remain.
- [ ] Manually edit settings.json to remove the Claude entry from `tabs`: relaunch, integrity check restores Claude with defaults; warning logged.
- [ ] Manually edit `active_tab_id` to a non-existent value: relaunch, app falls back to the first tab.

Command-not-found:

- [ ] Configure a Shell tab to point at a non-existent path (`C:\nonexistent\bash.exe`): save, restart the shell from the closed overlay — fails. Restart the app: the tab launches in closed state with the "command not found" message.
- [ ] Pressing Enter on the not-found message opens the Configure dialog (does not try to respawn).
- [ ] Fix the command in Configure, save, the closed overlay updates to the standard exit message; press Enter, the shell starts.

Settings window:

- [ ] Open Settings → Tabs. All current tabs are listed in order.
- [ ] Edit Claude's notifications: changes persist across restart.
- [ ] Edit a user Shell tab from the settings window: the change applies on next shell restart, persists across app restart.

## Done Criteria

- All ten "What This Milestone Delivers" items work end-to-end on Windows and Linux.
- Migration from a v1.1 settings file is verified on both platforms with a real v1.1 settings file.
- Tab persistence is verified across restarts for all four lifecycle operations (create, close, rename, configure).
- Integrity check produces sensible recovery from a hand-corrupted file.
- No regression in v1.1, M1, or M2 behavior.
- `cargo test` passes; the migration has unit-test coverage for the v1.1 → v1.2 transformation.
