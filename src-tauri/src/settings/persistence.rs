//! JSON load/save with corruption recovery + version migrations.
//!
//! Two files participate:
//!
//!   * **Global** (`<exe-dir>/settings.json`) — portable baseline. Written
//!     once on first launch when missing; never rewritten through normal
//!     edits afterwards. Hand-edit to change defaults.
//!   * **Custom overlay** (`<launch_cwd>/.cctts.custom.config.json`) — per
//!     launch-directory delta layered on top of global. Created the first
//!     time a user customizes anything from a given working directory and
//!     deleted automatically when the diff is empty.
//!
//! On-disk format for both files is the same JSON object shape (matching
//! `Settings`). The custom file is allowed to be a *partial* object — any
//! keys it doesn't carry fall through to global. Older shapes are detected
//! by their discriminator fields and routed through the `migration`
//! module after the merge so a hand-imported old file at the new path
//! still upgrades cleanly. After migration an integrity check reconciles
//! the four reserved AI builtins (claude, claude-local, aider, aider-local)
//! with `enabled_ai_tabs`: every enabled id is forced present with
//! `builtin: true`, every reserved id absent from the list is dropped.
//! The `shell-default-1` reserved id is *not* re-seeded by the integrity
//! check: it ships on fresh installs only, and stays closed once a user
//! closes it.
//!
//! `load` always returns a usable `Settings` and a snapshot of the global
//! baseline (so the save path can compute diffs without re-reading disk).
//! Missing/corrupt files become defaults; the corrupt original is moved
//! aside as a `.bak`.

use std::fs;
use std::path::{Path, PathBuf};

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::error::{AppError, AppResult};
use crate::settings::migration;
use crate::settings::write_atomic;
use crate::settings::schema::{
    default_ai_tab, default_shell_1_tab, AiTabId, LayoutNodePersisted, Settings, TabConfig,
    AIDER_LOCAL_TAB_ID, AIDER_TAB_ID, CLAUDE_LOCAL_TAB_ID, CLAUDE_TAB_ID, SHELL_DEFAULT_TAB_ID,
};
use crate::shell::ShellSpec;

const GLOBAL_FILE_NAME: &str = "settings.json";
const CUSTOM_FILE_NAME: &str = ".cctts.custom.config.json";

/// `<exe-dir>/settings.json` — the portable baseline. Falls back to the
/// current working directory if `current_exe()` is unavailable, which
/// shouldn't happen on any platform we ship to but is logged loudly if
/// it does.
pub fn global_path() -> AppResult<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| AppError::Settings(format!("current_exe failed: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| AppError::Settings("exe has no parent dir".into()))?;
    Ok(dir.join(GLOBAL_FILE_NAME))
}

/// `<launch_cwd>/.cctts.custom.config.json` — the per-folder overlay.
pub fn custom_path(launch_cwd: &Path) -> PathBuf {
    launch_cwd.join(CUSTOM_FILE_NAME)
}

/// Bundle returned from `load`: the resolved settings plus a snapshot of
/// just the global baseline. The saver needs the baseline so it can diff
/// the live state against it on every write without re-reading the file.
pub struct LoadOutcome {
    pub settings: Settings,
    pub global: Settings,
}

/// Always returns a `LoadOutcome`. Defaults are written to disk for the
/// global baseline when it's absent or corrupt; the custom overlay is
/// merely skipped if absent and quarantined if corrupt.
///
/// Migration runs *separately* on the global value and the overlay value
/// before they are merged, so a `.bak` file is written next to whichever
/// source actually carried the legacy keys. Pre-V0.6 the migration ran on
/// the merged value with the global path hardcoded as the backup target,
/// which mis-named the backup of an overlay-only legacy shape and
/// produced a confusing post-migration overlay diff against a still-old
/// global baseline.
pub fn load(default_shell: &ShellSpec, launch_cwd: &Path) -> LoadOutcome {
    // 1. Load and migrate the global baseline. After this `global` is in
    //    the current schema shape; a v1.x file on disk has been backed up
    //    next to the global path and rewritten.
    let global = load_global(default_shell);

    // 2. Load and migrate the overlay (if any). A migrated overlay's
    //    `.bak` is written next to the overlay file — the right place
    //    for a user looking at their per-folder config.
    let custom_path = custom_path(launch_cwd);
    let overlay_value = read_overlay_migrated(&custom_path, default_shell);

    // 3. Merge the (now both-current-shape) global + overlay.
    let mut merged = match serde_json::to_value(&global) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "settings: serialize global to value failed; using global as-is");
            return LoadOutcome {
                settings: global.clone(),
                global,
            };
        }
    };
    let overlay_existed = overlay_value.is_some();
    if let Some(overlay) = overlay_value {
        deep_merge(&mut merged, overlay);
    }

    let mut settings: Settings = match serde_json::from_value(merged) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "settings: typed parse failed (post-merge); using global"
            );
            return LoadOutcome {
                settings: global.clone(),
                global,
            };
        }
    };

    let repaired = integrity_check(&mut settings);

    // Re-point bundled avatar videos at the loaded theme. Existing installs
    // had absolute paths frozen to the seed-time theme written into their
    // settings file; this corrects them in memory on every launch so the
    // avatar matches `ui.theme`. Done after the merge (so the resolved
    // theme is known) and kept out of the persisted diff — it re-derives
    // from theme + on-disk folder each load, so there's nothing to save.
    apply_portable_avatar_paths(&mut settings);

    if repaired {
        // Persist the post-repair state back to its source of truth. If a
        // custom overlay was in play, we recompute and rewrite the diff;
        // otherwise we rewrite global.
        if overlay_existed {
            if let Err(e) = save(&settings, launch_cwd, &global) {
                tracing::warn!(error = %e, "settings: post-repair save (custom) failed");
            }
        } else if let Err(e) = save_global(&settings) {
            tracing::warn!(error = %e, "settings: post-repair save (global) failed");
        }
    }

    LoadOutcome { settings, global }
}

/// Read the overlay file, run any pending migration on it (writing a `.bak`
/// next to the overlay file itself), and return the migrated Value. Returns
/// `None` when the overlay is absent or the file was quarantined for
/// corruption. On migration-backup failure we still return the raw value —
/// callers can choose to abort if they want stricter behavior; here we
/// prefer "boot up with the user's settings" over "boot defaults because
/// we couldn't snapshot a backup".
fn read_overlay_migrated(path: &Path, default_shell: &ShellSpec) -> Option<Value> {
    let mut value = read_overlay(path)?;
    match migration::migrate_if_needed(&mut value, path, default_shell) {
        Ok(true) => {
            tracing::info!(path = %path.display(), "settings: overlay migrated in place");
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: overlay migration backup failed; using overlay raw",
            );
        }
    }
    Some(value)
}

/// Read the global file. Writes seeded defaults when absent. On parse
/// failure quarantines the file and returns defaults. Runs migration on
/// the global file in place — backup goes next to the global path itself,
/// not next to whatever path the merged result resolved to.
fn load_global(default_shell: &ShellSpec) -> Settings {
    let path = match global_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "settings: cannot resolve global path; using defaults");
            return seeded_defaults(default_shell);
        }
    };

    if !path.exists() {
        let s = seeded_defaults(default_shell);
        if let Err(e) = save_to(&path, &s) {
            tracing::warn!(error = %e, path = %path.display(), "settings: write global defaults failed");
        } else {
            tracing::info!(path = %path.display(), "settings: wrote global defaults");
        }
        return s;
    }

    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "settings: read global failed; using defaults");
            return seeded_defaults(default_shell);
        }
    };

    let mut value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: parse global failed; quarantining and resetting"
            );
            migration::quarantine_corrupt_file(&path);
            let s = seeded_defaults(default_shell);
            let _ = save_to(&path, &s);
            return s;
        }
    };

    // Migrate the global file in place. Backup is named after the global
    // file, which is the source of truth for the global baseline shape.
    let migrated = match migration::migrate_if_needed(&mut value, &path, default_shell) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                error = %e,
                path = %path.display(),
                "settings: global migration aborted (backup failed); using defaults"
            );
            return seeded_defaults(default_shell);
        }
    };

    let typed: Settings = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: typed parse of global failed; quarantining and resetting"
            );
            migration::quarantine_corrupt_file(&path);
            let s = seeded_defaults(default_shell);
            let _ = save_to(&path, &s);
            return s;
        }
    };

    if migrated {
        // Persist the migrated shape back to disk so future launches don't
        // re-migrate. Atomic write inside save_to keeps this safe under
        // crash.
        if let Err(e) = save_to(&path, &typed) {
            tracing::warn!(error = %e, path = %path.display(), "settings: post-migration global save failed");
        } else {
            tracing::info!(path = %path.display(), "settings: global migrated and rewritten");
        }
    } else {
        tracing::info!(path = %path.display(), "settings: global loaded");
    }
    typed
}

/// Read and parse the custom overlay file as a generic `Value`. Returns
/// `None` if absent. On parse failure the file is quarantined and `None`
/// is returned — we want the app to come up cleanly even if a hand-edit
/// broke the overlay.
fn read_overlay(path: &Path) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "settings: read overlay failed; ignoring");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: parse overlay failed; quarantining"
            );
            migration::quarantine_corrupt_file(path);
            None
        }
    }
}

/// Write the diff between `settings` and `global` to the custom overlay
/// file in `launch_cwd`. If the diff is empty, deletes any existing
/// overlay (so a user who reverts every change ends up with a clean
/// directory).
pub fn save(settings: &Settings, launch_cwd: &Path, global: &Settings) -> AppResult<()> {
    let path = custom_path(launch_cwd);
    let current = serde_json::to_value(settings)
        .map_err(|e| AppError::Settings(format!("serialize current: {e}")))?;
    let baseline = serde_json::to_value(global)
        .map_err(|e| AppError::Settings(format!("serialize global: {e}")))?;

    match diff(&current, &baseline) {
        Some(delta) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(AppError::Io)?;
            }
            let text = serde_json::to_string_pretty(&delta)
                .map_err(|e| AppError::Settings(format!("serialize overlay: {e}")))?;
            write_atomic(&path, text.as_bytes())?;
        }
        None => {
            if path.exists() {
                if let Err(e) = fs::remove_file(&path) {
                    tracing::warn!(error = %e, path = %path.display(), "settings: remove empty overlay failed");
                }
            }
        }
    }
    Ok(())
}

/// Write the full settings to the global file. Only used during initial
/// seeding and to finalize a post-migration / post-integrity rewrite when
/// no custom overlay is in play.
pub fn save_global(settings: &Settings) -> AppResult<()> {
    let path = global_path()?;
    save_to(&path, settings)
}

fn save_to(path: &Path, settings: &Settings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|e| AppError::Settings(format!("serialize: {e}")))?;
    write_atomic(path, text.as_bytes())
}

/// Recursively merge `overlay` into `base`. Objects are merged key-by-key;
/// every other value (arrays, primitives, null) replaces wholesale.
fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (k, v) in overlay_map {
                match base_map.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (slot, overlay) => {
            *slot = overlay;
        }
    }
}

/// Compute the minimal `current - baseline` overlay. Returns `None` when
/// the two are equal (i.e. nothing to write). Objects are diffed
/// key-by-key — keys whose values match the baseline are omitted, keys
/// that differ are included with the current value, keys present in the
/// baseline but missing from current are emitted as JSON null so the
/// reverse merge can reconstruct the deletion. Arrays and primitives are
/// included whole if they differ at all.
fn diff(current: &Value, baseline: &Value) -> Option<Value> {
    if current == baseline {
        return None;
    }
    match (current, baseline) {
        (Value::Object(c), Value::Object(b)) => {
            let mut out = Map::new();
            for (k, cv) in c {
                match b.get(k) {
                    Some(bv) => {
                        if let Some(sub) = diff(cv, bv) {
                            out.insert(k.clone(), sub);
                        }
                    }
                    None => {
                        out.insert(k.clone(), cv.clone());
                    }
                }
            }
            for (k, _) in b {
                if !c.contains_key(k) {
                    out.insert(k.clone(), Value::Null);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Object(out))
            }
        }
        _ => Some(current.clone()),
    }
}

/// `Settings::default()` with the three default tab entries seeded for the
/// host platform and the portable on-disk avatar paths stamped in (when
/// present). Used on fresh installs and as the recovery fallback when the
/// file is unrecoverable.
///
/// The integrity check restores the AI builtins; we additionally seed
/// `shell-default-1` here so a brand-new install gets one ready-to-use
/// shell tab. The integrity check no longer re-creates that tab on its
/// own, so closing it once is permanent (which is what we want for a
/// closable tab).
fn seeded_defaults(default_shell: &ShellSpec) -> Settings {
    let mut s = Settings::default();
    integrity_check(&mut s);
    s.tabs.push(default_shell_1_tab(default_shell));
    apply_portable_avatar_paths(&mut s);
    s
}

/// `<exe-dir>/../avatars/` — the portable avatar folder shipped in the
/// release zip. `None` if the exe path can't be resolved (which would
/// only happen on platforms or sandboxes where `current_exe()` fails) or
/// the folder doesn't actually exist (dev `cargo run`, or someone built
/// from source without staging).
fn portable_avatars_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("avatars");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Point the bundled avatar defaults at the active theme's videos under
/// `<exe-dir>/../avatars/<theme>/` so the on-disk copies users can swap are
/// the live source. Re-pointing follows the active theme, so callers run it
/// on first-launch seeding, on load (to repair a settings file seeded under
/// a different theme), and on every theme change. Genuine user overrides
/// (paths outside the portable avatars dir) are preserved; see
/// [`stamp_avatar_paths_from`]. No-op when the portable folder is absent
/// (dev `cargo run` / source builds), leaving the embedded `/avatar/...`
/// defaults in place.
pub fn apply_portable_avatar_paths(s: &mut Settings) {
    if let Some(dir) = portable_avatars_dir() {
        stamp_avatar_paths_from(s, &dir);
    }
}

/// Pure version of the avatar stamping step: given a directory, re-point the
/// bundled avatar defaults at the active theme's videos inside it. Split out
/// so the behavior is unit-testable without depending on the test binary's
/// `current_exe()` location.
///
/// Layout-aware: prefers the active theme's subfolder
/// (`<dir>/<ui.theme>/<file>`) so the avatar follows the chrome theme.
/// Falls back to a flat `<dir>/<file>` layout for legacy zips produced
/// before the per-theme split, so existing folders keep working. Per the
/// theme isolation policy (see `src/theme.css`), every theme owns its own
/// avatar folder — there is no cross-theme mapping here.
///
/// Idempotent and safe to call on every load and theme change: it only
/// re-points fields that are *bundled defaults* — unset, a bundled
/// `/avatar/...` URL, or a path we previously stamped inside `dir` (for any
/// theme). An absolute path *outside* `dir` is a deliberate user override
/// (picked from elsewhere on disk via the file dialog) and is left alone.
/// This is what makes the avatar videos follow theme switches instead of
/// staying frozen on whichever theme was active when the global settings
/// file was first seeded.
fn stamp_avatar_paths_from(s: &mut Settings, dir: &Path) {
    let theme_dir = dir.join(&s.ui.theme);

    let pick = |file: &str| -> Option<PathBuf> {
        let themed = theme_dir.join(file);
        if themed.is_file() {
            return Some(themed);
        }
        let flat = dir.join(file);
        if flat.is_file() {
            return Some(flat);
        }
        None
    };

    // Bundled default = re-pointable. A None field, a bundled `/avatar/...`
    // URL, or any path under `dir` (something we stamped before, possibly
    // for a different theme) all qualify. A path outside `dir` is a real
    // user override and is never disturbed.
    let is_bundled = |p: &Option<PathBuf>| match p {
        None => true,
        Some(path) => {
            path.starts_with(dir)
                || path.to_str().is_some_and(|s| s.starts_with("/avatar/"))
        }
    };

    if is_bundled(&s.avatar.images.idle) {
        s.avatar.images.idle = pick("Idle.mp4");
    }
    if is_bundled(&s.avatar.images.listening) {
        s.avatar.images.listening = pick("Listening.mp4");
    }
    if is_bundled(&s.avatar.images.thinking) {
        s.avatar.images.thinking = pick("Thinking.mp4");
    }
    if is_bundled(&s.avatar.images.speaking) {
        s.avatar.images.speaking = pick("Speaking.mp4");
    }
    if is_bundled(&s.avatar.images.error) {
        s.avatar.images.error = pick("Error.mp4");
    }
    // Transition keeps a non-None bundled fallback (`/avatar/Transition.mp4`,
    // which the frontend redirects into the active theme) when the theme
    // ships no on-disk file, so a theme switch never silently disables the
    // crossfade by leaving it None.
    if is_bundled(&s.avatar.transition.path) {
        s.avatar.transition.path =
            pick("Transition.mp4").or_else(|| Some(PathBuf::from("/avatar/Transition.mp4")));
    }
}

/// Drop any reserved AI tab that is not in `enabled_ai_tabs`. Returns
/// `true` if any entry was removed.
fn drop_disabled_ai_builtins(settings: &mut Settings) -> bool {
    let want: std::collections::HashSet<&'static str> = settings
        .enabled_ai_tabs
        .iter()
        .map(|id| id.as_str())
        .collect();
    let before = settings.tabs.len();
    settings.tabs.retain(|t| {
        // Keep anything that isn't a reserved AI id, plus reserved ids
        // that are in the want-set.
        let id = t.id();
        if AiTabId::from_id(id).is_some() {
            want.contains(id)
        } else {
            true
        }
    });
    let removed = before != settings.tabs.len();
    if removed {
        tracing::warn!("integrity: dropped reserved AI tab(s) not in enabled_ai_tabs");
    }
    removed
}

/// For each id in `enabled_ai_tabs` that's missing from `tabs`, insert
/// the default config at the canonical position. Returns `true` if any
/// entry was inserted.
fn restore_enabled_ai_builtins(settings: &mut Settings) -> bool {
    // Iterate in canonical order (claude → claude-local → aider →
    // aider-local) so successive insertions land in the right relative
    // slot regardless of the user's `enabled_ai_tabs` ordering.
    let order = [
        AiTabId::Claude,
        AiTabId::ClaudeLocal,
        AiTabId::Aider,
        AiTabId::AiderLocal,
    ];
    let mut changed = false;
    for &id in &order {
        if !settings.enabled_ai_tabs.contains(&id) {
            continue;
        }
        if settings.tabs.iter().any(|t| t.id() == id.as_str()) {
            continue;
        }
        let pos = canonical_insert_position(&settings.tabs, id);
        settings.tabs.insert(pos, default_ai_tab(id));
        changed = true;
        tracing::warn!(id = id.as_str(), "integrity: restored missing AI builtin tab");
    }
    changed
}

/// Position to insert a freshly-restored reserved AI tab so the AI
/// builtins keep their canonical leading order. Walks the existing
/// `tabs[]` and lands the new entry after the last reserved AI tab
/// whose canonical order is < `id`'s — so reinserting `aider` after the
/// user has `claude`, `claude-local`, and a shell tab places aider at
/// index 2 (in front of the shell), not at the end.
fn canonical_insert_position(tabs: &[TabConfig], id: AiTabId) -> usize {
    let target = id.canonical_order();
    let mut pos = 0usize;
    for (idx, tab) in tabs.iter().enumerate() {
        match AiTabId::from_id(tab.id()) {
            Some(other) if other.canonical_order() < target => {
                pos = idx + 1;
            }
            // First non-reserved-AI tab (or a higher-canonical-order AI
            // tab) marks the upper bound — anything past here is a
            // shell or a later-AI tab and we should insert before it.
            _ => return pos,
        }
    }
    pos
}

/// All four reserved AI tab ids. Used by the integrity check's "is this
/// id one of our reserved AI builtins?" loops; a single source of truth
/// keeps the `ai_builtins` membership check, the `use_local_provider`
/// expectation table, and the drop-disabled-tab pass in sync.
const AI_BUILTIN_IDS: [&str; 4] = [
    CLAUDE_TAB_ID,
    CLAUDE_LOCAL_TAB_ID,
    AIDER_TAB_ID,
    AIDER_LOCAL_TAB_ID,
];

/// Reconcile the `tabs` array with `enabled_ai_tabs`. Every enabled AI
/// id is forced present and marked `builtin: true`; every reserved AI
/// id absent from the list is dropped from `tabs`. Returns true if
/// anything was changed (caller may want to write back to disk). Logged
/// as a warning when an entry has to be restored — the typical cause is
/// a hand-edited file.
///
/// Restored AI tabs land at their canonical position (claude → 0,
/// claude-local → after claude, aider → after claude-local,
/// aider-local → after aider). User-created Shell tabs retain their
/// relative ordering after the AI builtins. The `shell-default-1`
/// reserved id is *not* re-seeded here: it's a closable shell that
/// ships only on fresh installs (see `seeded_defaults`).
///
/// Empty `enabled_ai_tabs` (a hand-edit, or a malformed migration) is
/// repaired by forcing it back to `[claude]` so the user always boots
/// with at least one AI tab.
pub fn integrity_check(settings: &mut Settings) -> bool {
    let mut changed = false;

    // 0. Empty enabled_ai_tabs is invalid — repair to [claude].
    if settings.enabled_ai_tabs.is_empty() {
        settings.enabled_ai_tabs = vec![AiTabId::Claude];
        changed = true;
        tracing::warn!("integrity: enabled_ai_tabs was empty; reset to [claude]");
    }

    // 1. Force builtin: true on every reserved AI id if it exists with
    //    builtin: false. Defends against hand-edits trying to flip the flag.
    for tab in settings.tabs.iter_mut() {
        if AI_BUILTIN_IDS.contains(&tab.id()) && !tab.builtin() {
            tab.set_builtin(true);
            changed = true;
            tracing::warn!(id = tab.id(), "integrity: forced builtin: true on AI builtin");
        }
    }

    // 2. Force builtin: false on `shell-default-1`: older settings files
    //    persisted it as `builtin: true`, which would block close_tab.
    //    Closability is now uniform across all shell tabs, so demote any
    //    surviving entry on load.
    for tab in settings.tabs.iter_mut() {
        if tab.id() == SHELL_DEFAULT_TAB_ID && tab.builtin() {
            tab.set_builtin(false);
            changed = true;
            tracing::warn!("integrity: demoted shell-default-1 to builtin: false");
        }
    }

    // 3. Force `use_local_provider` to its canonical value on every
    //    reserved AI tab so a hand-edit can't, e.g., flip the
    //    subscription Claude tab into local-LLM mode (which would
    //    silently route the user's primary tab through their local
    //    proxy).
    for tab in settings.tabs.iter_mut() {
        if let TabConfig::AiTool(c) = tab {
            if let Some(reserved) = AiTabId::from_id(c.id.as_str()) {
                let want = reserved.uses_local_provider();
                if c.use_local_provider != want {
                    c.use_local_provider = want;
                    changed = true;
                    tracing::warn!(
                        id = c.id,
                        "integrity: corrected use_local_provider on reserved AI tab"
                    );
                }
            }
        }
    }

    // 4. Reconcile AI builtin entries with `enabled_ai_tabs`. The list
    //    is the source of truth for which AI tabs exist: every enabled
    //    id is restored at its canonical position; every reserved id
    //    that's not enabled is dropped (the runtime's
    //    set-enabled-ai-tabs IPC kills the PTY in-session, but on cold
    //    load we just normalize the settings file). shell-default-1 is
    //    intentionally untouched here — it's a regular closable shell.
    if drop_disabled_ai_builtins(settings) {
        changed = true;
    }
    if restore_enabled_ai_builtins(settings) {
        changed = true;
    }

    // 5. Backend layout sanity. The frontend owns the deep integrity
    //    walk (orphan placement, empty-pane collapse) — it has the tree
    //    helpers. The backend's job here is just to keep the file
    //    deserializable and stop a hand-edit from referring to dead tab
    //    ids: drop tab_ids that don't exist, and clear invalid
    //    `focused_pane_id` so the frontend's leftmost-leaf fallback
    //    kicks in.
    if let Some(layout) = settings.layout.as_mut() {
        let valid_ids: HashSet<&str> =
            settings.tabs.iter().map(|t| t.id()).collect();
        let mut pane_ids: HashSet<String> = HashSet::new();
        if filter_layout_tab_ids(&mut layout.tree, &valid_ids, &mut pane_ids) {
            changed = true;
            tracing::warn!("integrity: dropped unknown tab ids from layout");
        }
        if !pane_ids.contains(&layout.focused_pane_id) {
            // Pick the leftmost-leaf pane id as a deterministic fallback.
            if let Some(replacement) = leftmost_pane_id(&layout.tree) {
                if layout.focused_pane_id != replacement {
                    tracing::warn!(
                        previous = %layout.focused_pane_id,
                        new = %replacement,
                        "integrity: focused_pane_id no longer exists; reset to leftmost leaf"
                    );
                    layout.focused_pane_id = replacement;
                    changed = true;
                }
            }
        }
    }

    changed
}

/// Walk the layout tree, dropping any `tab_ids` entries that aren't in
/// `valid_ids` (and clearing `active_tab_id` if it was dropped or no
/// longer matches a remaining id). Records every encountered pane id in
/// `pane_ids` so the caller can validate `focused_pane_id` afterwards.
/// Returns `true` if anything was changed.
fn filter_layout_tab_ids(
    node: &mut LayoutNodePersisted,
    valid_ids: &HashSet<&str>,
    pane_ids: &mut HashSet<String>,
) -> bool {
    match node {
        LayoutNodePersisted::Pane {
            id,
            tab_ids,
            active_tab_id,
        } => {
            pane_ids.insert(id.clone());
            let before = tab_ids.len();
            tab_ids.retain(|t| valid_ids.contains(t.as_str()));
            let mut changed = tab_ids.len() != before;
            if let Some(active) = active_tab_id.as_deref() {
                if !tab_ids.iter().any(|t| t == active) {
                    *active_tab_id = tab_ids.first().cloned();
                    changed = true;
                }
            }
            changed
        }
        LayoutNodePersisted::Split { first, second, .. } => {
            let mut changed = filter_layout_tab_ids(first, valid_ids, pane_ids);
            changed |= filter_layout_tab_ids(second, valid_ids, pane_ids);
            changed
        }
    }
}

/// Pane id of the leftmost leaf in `node`. Used as the deterministic
/// fallback when `focused_pane_id` no longer maps to an existing pane.
fn leftmost_pane_id(node: &LayoutNodePersisted) -> Option<String> {
    match node {
        LayoutNodePersisted::Pane { id, .. } => Some(id.clone()),
        LayoutNodePersisted::Split { first, .. } => leftmost_pane_id(first),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_default_shell() -> ShellSpec {
        ShellSpec {
            command: PathBuf::from("/bin/bash"),
            args: vec!["-i".to_string()],
        }
    }

    #[test]
    fn integrity_seeds_only_claude_on_empty_with_default_setting() {
        // Default `enabled_ai_tabs = [claude]` means a fresh install
        // gets the subscription Claude tab only; the integrity check
        // mustn't re-seed claude-local. The closable shell-default-1
        // ships via `seeded_defaults`, not the integrity check.
        let mut s = Settings::default();
        let _shell = fake_default_shell();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert!(s.tabs[0].builtin());
    }

    #[test]
    fn integrity_seeds_both_when_enabled_ai_tabs_is_both_claudes() {
        let mut s = Settings::default();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        let _shell = fake_default_shell();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
    }

    #[test]
    fn integrity_seeds_only_claude_local_when_setting_is_claude_local_only() {
        let mut s = Settings::default();
        s.enabled_ai_tabs = vec![AiTabId::ClaudeLocal];
        let _shell = fake_default_shell();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].id(), CLAUDE_LOCAL_TAB_ID);
    }

    #[test]
    fn integrity_seeds_aider_pair_at_canonical_positions() {
        let mut s = Settings::default();
        s.enabled_ai_tabs = vec![
            AiTabId::Claude,
            AiTabId::Aider,
            AiTabId::AiderLocal,
        ];
        integrity_check(&mut s);
        assert_eq!(s.tabs.len(), 3);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), AIDER_TAB_ID);
        assert_eq!(s.tabs[2].id(), AIDER_LOCAL_TAB_ID);
    }

    #[test]
    fn integrity_inserts_aider_between_claude_local_and_user_shell() {
        // User has [claude, claude-local, shell-foo] and now enables
        // aider. The new tab should land at index 2 (after claude-local,
        // before the shell), not at the end.
        let mut s = Settings::default();
        s.enabled_ai_tabs = vec![
            AiTabId::Claude,
            AiTabId::ClaudeLocal,
            AiTabId::Aider,
        ];
        integrity_check(&mut s);
        // Insert a user shell tab to simulate the existing layout.
        s.tabs.push(TabConfig::Shell(crate::settings::schema::ShellTabConfig {
            id: "shell-foo".to_string(),
            builtin: false,
            name: "Foo".to_string(),
            command: "/bin/bash".to_string(),
            args: vec!["-i".to_string()],
            cwd: None,
            env: Default::default(),
            notifications: Default::default(),
            theme_override: None,
            background_override: None,
        }));
        // Drop aider, then re-add via integrity.
        s.tabs.retain(|t| t.id() != AIDER_TAB_ID);
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
        assert_eq!(s.tabs[2].id(), AIDER_TAB_ID);
        assert_eq!(s.tabs[3].id(), "shell-foo");
    }

    #[test]
    fn integrity_drops_disabled_ai_tab() {
        // Loading a file where the setting and tabs disagree (e.g. a
        // hand-edit, or post-migration drift) reconciles to the setting.
        let mut s = Settings::default();
        let _shell = fake_default_shell();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        integrity_check(&mut s);
        assert_eq!(s.tabs.len(), 2);

        s.enabled_ai_tabs = vec![AiTabId::Claude];
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
    }

    #[test]
    fn integrity_repairs_empty_enabled_ai_tabs() {
        // A hand-edited file with `enabled_ai_tabs: []` is invalid;
        // integrity forces it back to [claude] so the user always boots
        // with at least one AI tab.
        let mut s = Settings::default();
        s.enabled_ai_tabs = Vec::new();
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.enabled_ai_tabs, vec![AiTabId::Claude]);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
    }

    #[test]
    fn integrity_does_not_restore_shell_default_1() {
        // Closing shell-default-1 must persist across launches: the
        // integrity check should leave it absent.
        let mut s = Settings::default();
        let _shell = fake_default_shell();
        integrity_check(&mut s);
        assert!(s
            .tabs
            .iter()
            .all(|t| t.id() != SHELL_DEFAULT_TAB_ID));
    }

    #[test]
    fn integrity_demotes_legacy_shell_default_1_to_non_builtin() {
        // Older settings files persisted shell-default-1 with builtin: true.
        // Loading those files must demote the entry so the close button
        // works.
        let mut s = Settings::default();
        let _shell = fake_default_shell();
        integrity_check(&mut s);
        // Insert a legacy-shaped shell-default-1 with builtin: true.
        s.tabs.push(TabConfig::Shell(
            crate::settings::schema::ShellTabConfig {
                id: SHELL_DEFAULT_TAB_ID.to_string(),
                builtin: true,
                name: "Shell 1".to_string(),
                command: "/bin/bash".to_string(),
                args: vec!["-i".to_string()],
                cwd: None,
                env: Default::default(),
                notifications: Default::default(),
                theme_override: None,
                background_override: None,
            },
        ));
        let changed = integrity_check(&mut s);
        assert!(changed);
        let entry = s
            .tabs
            .iter()
            .find(|t| t.id() == SHELL_DEFAULT_TAB_ID)
            .expect("shell-default-1 still present");
        assert!(!entry.builtin());
    }

    #[test]
    fn integrity_forces_builtin_true_on_ai_builtins() {
        let mut s = Settings::default();
        let _shell = fake_default_shell();
        integrity_check(&mut s);
        // Tamper: flip claude's builtin to false.
        if let TabConfig::AiTool(c) = &mut s.tabs[0] {
            c.builtin = false;
        }
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert!(s.tabs[0].builtin());
    }

    #[test]
    fn integrity_preserves_user_tabs() {
        let mut s = Settings::default();
        let _shell = fake_default_shell();
        integrity_check(&mut s);
        // Insert a user shell tab.
        s.tabs.push(TabConfig::Shell(crate::settings::schema::ShellTabConfig {
            id: "shell-user-1".to_string(),
            builtin: false,
            name: "Build Watch".to_string(),
            command: "/bin/bash".to_string(),
            args: vec!["-i".to_string()],
            cwd: None,
            env: Default::default(),
            notifications: Default::default(),
            theme_override: None,
            background_override: None,
        }));
        let user_pos_before = s.tabs.len() - 1;

        // Delete claude — integrity should restore it without disturbing
        // the user tab's relative position.
        s.tabs.retain(|t| t.id() != CLAUDE_TAB_ID);
        let changed = integrity_check(&mut s);
        assert!(changed);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        let user_pos_after = s
            .tabs
            .iter()
            .position(|t| t.id() == "shell-user-1")
            .unwrap();
        // User tab should still be at the end.
        assert_eq!(user_pos_after, s.tabs.len() - 1);
        let _ = user_pos_before;
    }

    #[test]
    fn v1_2_round_trip() {
        let _shell = fake_default_shell();
        let mut s = Settings::default();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        integrity_check(&mut s);
        let text = serde_json::to_string(&s).unwrap();
        let parsed: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.tabs.len(), 2);
        assert_eq!(parsed.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(parsed.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
    }

    #[test]
    fn integrity_corrects_use_local_provider_on_reserved_ai_tabs() {
        // A hand-edit must not be able to silently flip the
        // subscription Claude tab into local-LLM mode (or vice versa).
        // Enable both so the check has both AI tabs to validate.
        let mut s = Settings::default();
        s.enabled_ai_tabs = vec![AiTabId::Claude, AiTabId::ClaudeLocal];
        let _shell = fake_default_shell();
        integrity_check(&mut s);

        // Tamper: flip claude → local, claude-local → not local.
        if let TabConfig::AiTool(c) = &mut s.tabs[0] {
            c.use_local_provider = true;
        }
        if let TabConfig::AiTool(c) = &mut s.tabs[1] {
            c.use_local_provider = false;
        }

        let changed = integrity_check(&mut s);
        assert!(changed);
        if let TabConfig::AiTool(c) = &s.tabs[0] {
            assert!(!c.use_local_provider, "claude must have use_local_provider=false");
        }
        if let TabConfig::AiTool(c) = &s.tabs[1] {
            assert!(c.use_local_provider, "claude-local must have use_local_provider=true");
        }
    }

    #[test]
    fn integrity_corrects_use_local_provider_on_aider_pair() {
        let mut s = Settings::default();
        s.enabled_ai_tabs = vec![AiTabId::Aider, AiTabId::AiderLocal];
        integrity_check(&mut s);
        // Tamper: aider → local, aider-local → not local.
        if let TabConfig::AiTool(c) = s.tabs.iter_mut().find(|t| t.id() == AIDER_TAB_ID).unwrap() {
            c.use_local_provider = true;
        }
        if let TabConfig::AiTool(c) =
            s.tabs.iter_mut().find(|t| t.id() == AIDER_LOCAL_TAB_ID).unwrap()
        {
            c.use_local_provider = false;
        }
        let changed = integrity_check(&mut s);
        assert!(changed);
        if let TabConfig::AiTool(c) = s.tabs.iter().find(|t| t.id() == AIDER_TAB_ID).unwrap() {
            assert!(!c.use_local_provider, "aider must have use_local_provider=false");
        }
        if let TabConfig::AiTool(c) =
            s.tabs.iter().find(|t| t.id() == AIDER_LOCAL_TAB_ID).unwrap()
        {
            assert!(c.use_local_provider, "aider-local must have use_local_provider=true");
        }
    }

    #[test]
    fn ui_theme_round_trip_and_default() {
        // Default file has ui.theme = "tui-yellow" (new installs land here).
        let s = Settings::default();
        assert_eq!(s.ui.theme, "tui-yellow");

        // Round-trip preserves a hand-edited value (here: a user who
        // switched back to modern-dark or set a future theme).
        let mut s = Settings::default();
        s.ui.theme = "modern-dark".to_string();
        let text = serde_json::to_string(&s).unwrap();
        let parsed: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.ui.theme, "modern-dark");

        // A v1.3 file without the `ui` field still parses (serde(default)).
        let v1_3_json = r#"{"tabs":[]}"#;
        let parsed: Settings = serde_json::from_str(v1_3_json).unwrap();
        assert_eq!(parsed.ui.theme, "tui-yellow");
    }

    // --- Layered config (global + custom overlay) -----------------------

    #[test]
    fn deep_merge_object_keys() {
        let mut base = serde_json::json!({
            "a": 1,
            "nested": { "x": 1, "y": 2 },
        });
        let overlay = serde_json::json!({
            "a": 99,
            "nested": { "y": 22, "z": 33 },
            "new_key": "added",
        });
        deep_merge(&mut base, overlay);
        assert_eq!(
            base,
            serde_json::json!({
                "a": 99,
                "nested": { "x": 1, "y": 22, "z": 33 },
                "new_key": "added",
            })
        );
    }

    #[test]
    fn deep_merge_arrays_replace_wholesale() {
        let mut base = serde_json::json!({ "tabs": [1, 2, 3] });
        let overlay = serde_json::json!({ "tabs": [9] });
        deep_merge(&mut base, overlay);
        assert_eq!(base, serde_json::json!({ "tabs": [9] }));
    }

    #[test]
    fn diff_identical_returns_none() {
        let v = serde_json::json!({ "a": 1, "b": [1, 2] });
        assert!(diff(&v, &v).is_none());
    }

    #[test]
    fn diff_drops_matching_keys() {
        let current = serde_json::json!({
            "a": 1,
            "nested": { "x": 1, "y": 99 },
        });
        let baseline = serde_json::json!({
            "a": 1,
            "nested": { "x": 1, "y": 2 },
        });
        let d = diff(&current, &baseline).unwrap();
        assert_eq!(d, serde_json::json!({ "nested": { "y": 99 } }));
    }

    #[test]
    fn diff_emits_null_for_keys_only_in_baseline() {
        // Models a removal in the overlay so the reverse merge can
        // reconstruct it. (Not common in our typed Settings — every field
        // has a default — but the diff/merge pair must round-trip
        // generic JSON objects cleanly.)
        let current = serde_json::json!({ "a": 1 });
        let baseline = serde_json::json!({ "a": 1, "b": 2 });
        let d = diff(&current, &baseline).unwrap();
        assert_eq!(d, serde_json::json!({ "b": null }));
    }

    #[test]
    fn diff_arrays_replace_whole() {
        let current = serde_json::json!({ "tabs": [1, 2, 3] });
        let baseline = serde_json::json!({ "tabs": [1, 2] });
        let d = diff(&current, &baseline).unwrap();
        assert_eq!(d, serde_json::json!({ "tabs": [1, 2, 3] }));
    }

    #[test]
    fn merge_then_diff_round_trip_typed_settings() {
        // Take default Settings as global, mutate one nested field, diff
        // it, and verify that re-applying the diff to global recovers the
        // mutated state.
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        let mut customized = global.clone();
        customized.ui.theme = "future-light".to_string();

        let g_value = serde_json::to_value(&global).unwrap();
        let c_value = serde_json::to_value(&customized).unwrap();
        let delta = diff(&c_value, &g_value).expect("non-empty diff");

        // The delta should be tiny — just the ui.theme branch.
        assert_eq!(delta, serde_json::json!({ "ui": { "theme": "future-light" } }));

        // Reverse: apply delta to global, deserialize, confirm we get
        // `customized` back.
        let mut reapplied = g_value.clone();
        deep_merge(&mut reapplied, delta);
        let recovered: Settings = serde_json::from_value(reapplied).unwrap();
        assert_eq!(recovered.ui.theme, "future-light");
    }

    #[test]
    fn stamp_avatar_paths_uses_files_present_in_dir() {
        let dir = std::env::temp_dir()
            .join(format!("cctts_avatars_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        // Stage two of the five state videos plus the transition; the
        // remaining three should be left untouched (None / embedded URL).
        for f in ["Idle.mp4", "Speaking.mp4", "Transition.mp4"] {
            fs::write(dir.join(f), b"").unwrap();
        }

        let mut s = Settings::default();
        // Sanity: defaults start with images None and transition pointing
        // at the embedded `/avatar/...` URL.
        assert!(s.avatar.images.idle.is_none());
        assert_eq!(
            s.avatar.transition.path.as_deref().map(|p| p.to_string_lossy().to_string()),
            Some("/avatar/Transition.mp4".to_string())
        );

        stamp_avatar_paths_from(&mut s, &dir);

        assert_eq!(s.avatar.images.idle.as_deref(), Some(dir.join("Idle.mp4").as_path()));
        assert_eq!(s.avatar.images.speaking.as_deref(), Some(dir.join("Speaking.mp4").as_path()));
        assert!(s.avatar.images.listening.is_none(), "missing files should not be stamped");
        assert!(s.avatar.images.thinking.is_none());
        assert!(s.avatar.images.error.is_none());
        assert_eq!(s.avatar.transition.path.as_deref(), Some(dir.join("Transition.mp4").as_path()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_prefers_theme_subfolder() {
        let dir = std::env::temp_dir()
            .join(format!("cctts_avatars_themed_{}", uuid::Uuid::new_v4()));
        let modern = dir.join("modern-dark");
        let tui_yellow = dir.join("tui-yellow");
        fs::create_dir_all(&modern).unwrap();
        fs::create_dir_all(&tui_yellow).unwrap();

        // Stage the same files in both theme folders so we can prove the
        // active theme drives the selection rather than alphabetical luck.
        for f in ["Idle.mp4", "Speaking.mp4", "Transition.mp4"] {
            fs::write(modern.join(f), b"").unwrap();
            fs::write(tui_yellow.join(f), b"").unwrap();
        }

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        stamp_avatar_paths_from(&mut s, &dir);

        assert_eq!(s.avatar.images.idle.as_deref(), Some(tui_yellow.join("Idle.mp4").as_path()));
        assert_eq!(
            s.avatar.images.speaking.as_deref(),
            Some(tui_yellow.join("Speaking.mp4").as_path()),
        );
        assert_eq!(
            s.avatar.transition.path.as_deref(),
            Some(tui_yellow.join("Transition.mp4").as_path()),
        );

        // Switching themes restamps from the other folder.
        let mut s2 = Settings::default();
        s2.ui.theme = "modern-dark".to_string();
        stamp_avatar_paths_from(&mut s2, &dir);
        assert_eq!(
            s2.avatar.images.idle.as_deref(),
            Some(modern.join("Idle.mp4").as_path()),
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_falls_back_to_flat_layout() {
        // Legacy zips (pre per-theme split) staged the videos at the top
        // of `avatars/`. Verify those still get picked up when the active
        // theme's subfolder is missing.
        let dir = std::env::temp_dir()
            .join(format!("cctts_avatars_flat_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        for f in ["Idle.mp4", "Transition.mp4"] {
            fs::write(dir.join(f), b"").unwrap();
        }

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string(); // tui-yellow/ subfolder does not exist
        stamp_avatar_paths_from(&mut s, &dir);

        assert_eq!(s.avatar.images.idle.as_deref(), Some(dir.join("Idle.mp4").as_path()));
        assert_eq!(
            s.avatar.transition.path.as_deref(),
            Some(dir.join("Transition.mp4").as_path()),
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_noop_when_dir_empty() {
        let dir = std::env::temp_dir()
            .join(format!("cctts_avatars_empty_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut s = Settings::default();
        let before_transition = s.avatar.transition.path.clone();
        stamp_avatar_paths_from(&mut s, &dir);

        assert!(s.avatar.images.idle.is_none());
        assert!(s.avatar.images.listening.is_none());
        assert!(s.avatar.images.thinking.is_none());
        assert!(s.avatar.images.speaking.is_none());
        assert!(s.avatar.images.error.is_none());
        assert_eq!(s.avatar.transition.path, before_transition);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_preserves_user_override_outside_dir() {
        let dir = std::env::temp_dir()
            .join(format!("cctts_avatars_ovr_{}", uuid::Uuid::new_v4()));
        let theme = dir.join("tui-yellow");
        fs::create_dir_all(&theme).unwrap();
        fs::write(theme.join("Idle.mp4"), b"").unwrap();

        // A genuine override the user picked from elsewhere on disk.
        let custom = std::env::temp_dir()
            .join(format!("cctts_custom_{}.mp4", uuid::Uuid::new_v4()));
        fs::write(&custom, b"").unwrap();

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        s.avatar.images.idle = Some(custom.clone());
        stamp_avatar_paths_from(&mut s, &dir);

        // The override (outside `dir`) survives; bundled fields are stamped.
        assert_eq!(s.avatar.images.idle.as_deref(), Some(custom.as_path()));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&custom);
    }

    #[test]
    fn stamp_avatar_paths_repoints_on_theme_switch() {
        let dir = std::env::temp_dir()
            .join(format!("cctts_avatars_switch_{}", uuid::Uuid::new_v4()));
        let yellow = dir.join("tui-yellow");
        let purple = dir.join("tui-purple");
        fs::create_dir_all(&yellow).unwrap();
        fs::create_dir_all(&purple).unwrap();
        fs::write(yellow.join("Idle.mp4"), b"").unwrap();
        fs::write(purple.join("Idle.mp4"), b"").unwrap();

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(s.avatar.images.idle.as_deref(), Some(yellow.join("Idle.mp4").as_path()));

        // The previously-stamped path (inside `dir`) is re-pointed to the new
        // theme, NOT mistaken for a user override. This is the actual bug fix.
        s.ui.theme = "tui-purple".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(s.avatar.images.idle.as_deref(), Some(purple.join("Idle.mp4").as_path()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_avatar_paths_resets_when_new_theme_has_no_files() {
        let dir = std::env::temp_dir()
            .join(format!("cctts_avatars_reset_{}", uuid::Uuid::new_v4()));
        let yellow = dir.join("tui-yellow");
        fs::create_dir_all(&yellow).unwrap();
        fs::write(yellow.join("Idle.mp4"), b"").unwrap();
        fs::write(yellow.join("Transition.mp4"), b"").unwrap();

        let mut s = Settings::default();
        s.ui.theme = "tui-yellow".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert_eq!(s.avatar.images.idle.as_deref(), Some(yellow.join("Idle.mp4").as_path()));
        assert_eq!(
            s.avatar.transition.path.as_deref(),
            Some(yellow.join("Transition.mp4").as_path()),
        );

        // Switch to a theme with no on-disk folder: the image resets to None
        // (frontend uses the embedded bundled video) and the transition
        // reverts to the redirectable `/avatar/` URL — never a stale
        // tui-yellow path, never a disabling None.
        s.ui.theme = "modern-dark".to_string();
        stamp_avatar_paths_from(&mut s, &dir);
        assert!(s.avatar.images.idle.is_none());
        assert_eq!(
            s.avatar.transition.path.as_deref().map(|p| p.to_string_lossy().to_string()),
            Some("/avatar/Transition.mp4".to_string())
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writes_overlay_when_diff_nonempty_and_removes_when_empty() {
        let _shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global);

        // Use a unique subdir under the system temp root so parallel test
        // runs don't collide. Cleaned up at the end of the test.
        let dir = std::env::temp_dir()
            .join(format!("cctts_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let overlay = dir.join(CUSTOM_FILE_NAME);

        // Customized: should write a non-empty overlay.
        let mut customized = global.clone();
        customized.ui.theme = "future-light".to_string();
        save(&customized, &dir, &global).unwrap();
        assert!(overlay.exists());
        let text = fs::read_to_string(&overlay).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, serde_json::json!({ "ui": { "theme": "future-light" } }));

        // Reverted to identical: should remove the overlay.
        save(&global, &dir, &global).unwrap();
        assert!(!overlay.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
