# Feature: Configuration Scope (Project-Local Settings)

## Purpose

Broaden where settings live: in addition to the single global config at `%APPDATA%\cctts\settings.json` (Windows) / platform equivalents, allow each project directory to carry its own complete settings file. On launch, cctts checks the cwd for a project-local file; if found, that file becomes the source of truth for the session — *every* setting (TTS, avatar, themes, background image, tabs, layout, presets, shortcuts) loads from and persists to it. The global file is left untouched.

This subsumes the older "per-cwd / per-project layout memory" item from `FUTURE-FEATURES.md` — instead of a `Map<cwd, LayoutPersisted>` inside one global file, we generalize to "a whole settings file per directory." Same shape, different location, simpler runtime architecture.

See `FUTURE-FEATURES.md` § "Project-local settings file" for the full per-decision rationale; this doc captures the implementation strategy.

## Why one feature, not two

The deferred per-cwd-layout item proposed keying just the `layout` field by cwd. Once you have any project-scoped state, every setting is a candidate (themes per project, background images per project, tab sets per project, shortcuts per project). The runtime architecture is *simpler* if the entire `SettingsHandle` operates on a different file path than if a single field is keyed by cwd. Picking the per-file approach retires the per-cwd-layout entry.

## Architecture

### Path resolution

Today, `src-tauri/src/settings/persistence.rs` computes one path: `<config_dir>/cctts/settings.json`. After this feature:

```rust
fn resolved_config_path(launch_cwd: &Path) -> ResolvedPath {
    if let Some(decision) = read_cwd_decisions().get(launch_cwd) {
        match decision {
            "project-local" => ProjectLocal(launch_cwd.join(".cctts/settings.json")),
            "global"        => Global(global_path()),
        }
    } else {
        // First launch in this cwd. Probe filesystem.
        let project_path = launch_cwd.join(".cctts/settings.json");
        if project_path.exists() {
            // Probably created by another cctts user (committed to repo) or
            // by this user previously. Don't auto-adopt — prompt at startup.
            Unresolved(launch_cwd, project_path)
        } else {
            Unresolved(launch_cwd, project_path) // file doesn't exist; prompt offers to create
        }
    }
}
```

Once resolved, the existing `SettingsHandle` runtime is unchanged — it reads/writes whichever path it was given. This is the architectural payoff.

### Per-cwd preference cache

A small file in the global dir tracks the user's choice for each known cwd, so the prompt only fires on first launch in a directory:

`<config_dir>/cctts/cwd_decisions.json`:
```json
{
  "/abs/path/to/project-a": "project-local",
  "/abs/path/to/project-b": "global",
  ...
}
```

This file is *global metadata about the user's choices*, not project state. Storing it globally is correct.

A "Reset preference for this directory" entry in the Layouts/Settings menu un-records the decision for the active cwd, so the next launch re-prompts.

### Startup prompt UX

A small modal on first launch in an unrecognized cwd. Three explicit buttons:

- **Use global config** — sets `cwd_decisions[cwd] = "global"`. No project file created.
- **Create project config here** — copies current in-memory global state to `<cwd>/.cctts/settings.json`, sets `cwd_decisions[cwd] = "project-local"`. Reload the SettingsHandle from the new path.
- **Cancel** — uses global for *this session only*, doesn't record a preference. Re-prompts next launch.

Dismissable, never blocks the app, only appears on truly first launch in a cwd.

The "Don't ask again" checkbox is implicit — picking either of the first two options records the preference.

### Switching modes mid-session: out of scope

If a user wants to switch a directory from global to project-local, they restart cctts after creating the file (or via a menu entry that does the copy + initiates a clean restart). Hot-swap mid-run would require tearing down and re-broadcasting every settings subscriber (TTS pipeline, audio output, tab registry, frontend store) — doable but the complexity isn't justified for a workflow that naturally aligns with launching cctts per project.

### No upward directory walk

Only the exact launch cwd is checked. Don't traverse parents looking for `.cctts/`. Predictable behavior ("I launched here, so this is what's active"), and avoids picking up a stale config from an unrelated parent directory. Document and leave.

### Migration: existing v1.3 users

First launch after this lands: synthesize a `cwd_decisions[current_cwd] = "global"` entry, so no prompt fires for the directory the user is already running in. Subsequent launches in *new* cwds prompt as designed. No data loss — the global file remains the source of truth until the user explicitly creates a project file.

### File location: `.cctts/settings.json`

Hidden directory, namespaced — same idiom as `.git/`, `.vscode/`. Migration backups (`config.json.v1.X.bak`) live alongside whichever file is active.

### Avatar / background / image paths inside project configs

Today, asset paths in settings are absolute. For shared (gitted) project configs, users want path *relative-ness* — an asset stored at `.cctts/avatars/idle.png` should resolve regardless of where the repo is cloned.

**Rule**: when the active settings file is project-local, paths starting with `./` or relative paths are resolved relative to the project dir; absolute paths still work. Same rule applies to background images. Global config keeps absolute-paths-only behavior since there's no project root to resolve against.

This rule is implemented at *every site that reads an asset path*, not at load time — the settings file stores the path verbatim (so editing it preserves user intent), and resolution happens at consumption. Sites: `AvatarOverlay.svelte` (asset src attribute), `terminals.ts` (background image), any future per-tab avatar resolution.

A small helper `resolveAssetPath(rawPath, settingsScope)` centralizes the rule. Add to `src/lib/settings/store.ts` or a sibling.

### Settings window scope

The Settings window operates on whatever file is currently active. A small indicator in the title bar shows "Editing project config (`/abs/path/.cctts`)" vs. "Editing global config" so the user knows which file they're modifying. **No mode-switching dropdown** — the file is fixed at launch.

### Layout presets

Project-local presets only apply to that project. Cross-project preset sharing is a separate feature (would require a third "user-level presets" tier) and is out of scope.

### Git/VCS interaction

A project file at `.cctts/settings.json` is naturally project-scoped — some users will commit it (team shares workspace defaults), others will gitignore it (personal preferences don't leak). cctts doesn't enforce either. Document the trade-off in the README.

**Auto-write a sibling `.gitignore`**: when creating a project file, also write `.cctts/.gitignore` containing at least `*.bak\n` so backup files never end up in version control even if `settings.json` itself is tracked. Worth doing — backups are user-machine state regardless of whether config is shared.

## Implementation outline

The work splits along three reasonably-independent seams:

### Seam 1: Path resolution + decision cache (backend)

- Add `cwd_decisions.json` schema and read/write in `src-tauri/src/settings/persistence.rs`.
- Refactor `config_path()` → `resolved_config_path(launch_cwd)` returning a `Resolved` enum.
- Wire `SettingsHandle` initialization to receive the resolved path.
- Migration step on first launch: synthesize `cwd_decisions[current_cwd] = "global"`.
- Backend ready before any UI lands.

### Seam 2: Startup prompt + reload (frontend + Tauri command)

- New Tauri command `prompt_cwd_decision()` that returns the current resolved path's status (resolved or unresolved).
- New frontend modal triggered on app mount when status is `Unresolved`.
- New Tauri commands `record_cwd_decision(decision: "global" | "project-local")` and `reset_cwd_decision()`. The "project-local" path triggers a `copy_global_to_project()` step in Rust.
- Frontend reload: after recording a decision that changes the active file, reload the page (cleanest restart of every settings subscriber).
- Frontend Settings window title-bar indicator showing active scope.

### Seam 3: Asset path resolution

- Add `resolveAssetPath(rawPath, settingsScope)` helper.
- Audit all asset-path consumers: avatar overlay, terminal background image (when it ships), any other site reading a file path from settings.
- Update each site to use the helper.
- This seam can ship before or after seams 1+2 depending on whether the per-tab background feature has shipped — it's most valuable once asset paths are common in settings.

## Open questions

- **What if `.cctts/settings.json` exists in the cwd but the user has never launched cctts there?** (e.g., they cloned a repo with a committed project config.) The startup prompt copy needs to handle this: offer a third option "**Use existing project config**" that adopts the file as-is. Decide at implementation time whether that's a separate button or whether the existing "Use this project config" button just behaves contextually based on whether the file pre-exists.
- **Settings file format compatibility across cctts versions**: a teammate runs cctts v1.5 with a committed project config; a coworker on cctts v1.4 clones the repo. The v1.4 user's migration logic kicks in. Migration is already idempotent and additive, so this should work — but verify with a test (drop a v1.5-shaped file in front of a v1.4 build).
- **Reset cwd preference UI affordance**: Layouts menu, Settings menu, or both? Probably Settings (Appearance or General tab), since Layouts is reserved for layout presets.

## Milestone recommendation

**Milestones needed.** Carve along the three seams:

- `MILESTONE-V1.4-XX-config-scope-backend.md` — Seam 1 only. Path resolution, decision cache, migration. No UI changes; existing UI continues to work against whichever path was resolved (which on first launch will always be the global one for existing users).
- `MILESTONE-V1.4-XX-config-scope-prompt.md` — Seam 2. Startup prompt, record/reset commands, frontend reload, scope indicator. After this lands, the feature is user-visible and complete.
- `MILESTONE-V1.4-XX-config-scope-asset-paths.md` — Seam 3. Asset-path resolver, audit and update all consumers. Can ship before or after seam 2 in calendar order; depends on whether asset paths in settings are common at the time. The seams have no hard dependency between them — Seam 3 only matters once a project-local file actually exists.

**When implementation starts, write the milestones in detail then.** This doc captures the strategy; the milestones capture the step-by-step.

**Trigger**: per `FUTURE-FEATURES.md`, the strongest trigger is when both Terminal Color Themes and Terminal Background Image (from the per-tab-overrides feature) have shipped. Visual identity per project is the killer use case that makes the prompt-at-launch UX feel earned. Earlier triggers exist (multi-client work, team-shared configs) but the visual-identity case is most likely.

## Files most likely touched

- `src-tauri/src/settings/persistence.rs` — path resolution, decision cache file.
- `src-tauri/src/settings/migration.rs` — synthesize cwd decision on first launch.
- `src-tauri/src/settings/mod.rs` — wire resolved path into `SettingsHandle` init.
- `src-tauri/src/main.rs` (or wherever Tauri commands are registered) — new commands.
- `src/lib/settings/{ipc,store,types}.ts` — frontend reload helper, scope indicator state.
- New file: `src/lib/dialog/CwdDecisionDialog.svelte` (startup modal).
- `src/lib/AvatarOverlay.svelte`, `src/lib/terminals.ts` — asset path resolution sites.
