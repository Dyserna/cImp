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
//! still upgrades cleanly. After migration an integrity check ensures the
//! two AI builtins (claude, claude-local) exist with `builtin: true` —
//! hand-edited files that deleted them are repaired transparently. The
//! `shell-default-1` reserved id is *not* re-seeded by the integrity
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
use crate::settings::schema::{
    default_claude_local_tab, default_claude_tab, default_shell_1_tab, LayoutNodePersisted,
    Settings, TabConfig, CLAUDE_LOCAL_TAB_ID, CLAUDE_TAB_ID, SHELL_DEFAULT_TAB_ID,
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
/// merely skipped if absent and quarantined if corrupt. v1 / v1.1 files
/// found at the global path are migrated and rewritten in v1.2 shape with
/// a backup of the original alongside.
pub fn load(default_shell: &ShellSpec, launch_cwd: &Path) -> LoadOutcome {
    let global = load_global(default_shell);

    let custom_path = custom_path(launch_cwd);
    let merged_value = match read_overlay(&custom_path) {
        Some(overlay) => {
            let mut base = serde_json::to_value(&global)
                .unwrap_or_else(|_| Value::Object(Map::new()));
            deep_merge(&mut base, overlay);
            base
        }
        None => match serde_json::to_value(&global) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "settings: serialize global to value failed; using global as-is");
                return LoadOutcome {
                    settings: global.clone(),
                    global,
                };
            }
        },
    };

    let mut value = merged_value;

    // Run migration against the merged Value. The combined shape is whatever
    // global carried plus any keys the overlay added — usually current shape,
    // but we keep the migration cascade in place so a hand-imported legacy
    // file at the global path still upgrades.
    let migrated = match migration::migrate_if_needed(&mut value, &global_path_or_fallback(), default_shell) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                error = %e,
                "settings: migration aborted (backup failed); using global in-session"
            );
            return LoadOutcome {
                settings: global.clone(),
                global,
            };
        }
    };

    let mut settings: Settings = match serde_json::from_value(value) {
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

    let repaired = integrity_check(&mut settings, default_shell);

    if migrated || repaired {
        // Persist the post-migration / post-repair state back to its source
        // of truth. If a custom overlay was in play, we recompute and
        // rewrite the diff; otherwise we rewrite global.
        if custom_path.exists() {
            if let Err(e) = save(&settings, launch_cwd, &global) {
                tracing::warn!(error = %e, "settings: post-migration save (custom) failed");
            }
        } else if let Err(e) = save_global(&settings) {
            tracing::warn!(error = %e, "settings: post-migration save (global) failed");
        }
    }

    LoadOutcome { settings, global }
}

/// Read the global file. Writes seeded defaults when absent. On parse
/// failure quarantines the file and returns defaults.
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

    let value: Value = match serde_json::from_str(&text) {
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

    // Typed deserialize directly — migration runs on the merged value in
    // `load` so the global baseline here is whatever shape the file is.
    // serde(default) on every field tolerates a file that's missing keys;
    // truly old shapes get fixed up by the merge-time migration step.
    match serde_json::from_value(value) {
        Ok(s) => {
            tracing::info!(path = %path.display(), "settings: global loaded");
            s
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: typed parse of global failed; quarantining and resetting"
            );
            migration::quarantine_corrupt_file(&path);
            let s = seeded_defaults(default_shell);
            let _ = save_to(&path, &s);
            s
        }
    }
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
            fs::write(&path, text).map_err(AppError::Io)?;
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
    fs::write(path, text).map_err(AppError::Io)
}

/// Convenience: `global_path()` if it resolves, else a sentinel under the
/// current dir. Only used as the `path` arg for `migrate_if_needed`, which
/// uses it to write a rotation-suffixed `.bak` next to the source. If
/// `current_exe()` failed we'd already have logged the warning during
/// `load_global`; this just keeps the migration call signature happy.
fn global_path_or_fallback() -> PathBuf {
    global_path().unwrap_or_else(|_| PathBuf::from("settings.json"))
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
    integrity_check(&mut s, default_shell);
    s.tabs.push(default_shell_1_tab(default_shell));
    seed_portable_avatar_paths(&mut s);
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

/// On a fresh install, populate `avatar.images.*` and
/// `avatar.transition.path` with absolute paths to the videos staged in
/// `<exe-dir>/../avatars/` so the global settings file written on first
/// launch points at the on-disk copies users can swap. Each state is
/// only stamped when its file actually exists; missing files leave the
/// embedded `/avatar/...` URL (transition) or `None` (images) defaults
/// in place so the runtime still falls back to the bundled-in-exe copy.
fn seed_portable_avatar_paths(s: &mut Settings) {
    if let Some(dir) = portable_avatars_dir() {
        stamp_avatar_paths_from(s, &dir);
    }
}

/// Pure version of the avatar stamping step: given a directory, stamp the
/// appropriate fields from any matching files inside it. Split out so the
/// behavior is unit-testable without depending on the test binary's
/// `current_exe()` location.
fn stamp_avatar_paths_from(s: &mut Settings, dir: &Path) {
    let stamp = |slot: &mut Option<PathBuf>, file: &str| {
        let p = dir.join(file);
        if p.is_file() {
            *slot = Some(p);
        }
    };

    stamp(&mut s.avatar.images.idle, "Idle.mp4");
    stamp(&mut s.avatar.images.listening, "Listening.mp4");
    stamp(&mut s.avatar.images.thinking, "Thinking.mp4");
    stamp(&mut s.avatar.images.speaking, "Speaking.mp4");
    stamp(&mut s.avatar.images.error, "Error.mp4");

    let transition = dir.join("Transition.mp4");
    if transition.is_file() {
        s.avatar.transition.path = Some(transition);
    }
}

/// Ensure the two AI builtin entries are present and marked as builtins.
/// Returns true if anything was changed (caller may want to write back
/// to disk). Logged as a warning when an entry has to be restored — the
/// typical cause is a hand-edited file.
///
/// The order is deterministic: claude first, then claude-local, each
/// restored entry inserted at its canonical position (front,
/// after-claude). User-created Shell tabs retain their relative ordering
/// after the two pinned AI builtins. The `shell-default-1` reserved id
/// is *not* re-seeded here: it's a closable shell that ships only on
/// fresh installs (see `seeded_defaults`).
pub fn integrity_check(settings: &mut Settings, default_shell: &ShellSpec) -> bool {
    let _ = default_shell; // retained for signature stability across call sites
    let mut changed = false;

    // 1. Force builtin: true on the two AI builtins if they exist with
    //    builtin: false. Defends against hand-edits trying to flip the flag.
    let ai_builtins = [CLAUDE_TAB_ID, CLAUDE_LOCAL_TAB_ID];
    for tab in settings.tabs.iter_mut() {
        if ai_builtins.contains(&tab.id()) && !tab.builtin() {
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

    // 3. Force `use_local_provider` to its canonical value on the two
    //    AI builtins so a hand-edit can't, e.g., flip the subscription
    //    Claude tab into local-LLM mode (which would silently route the
    //    user's primary tab through their local proxy).
    for tab in settings.tabs.iter_mut() {
        if let TabConfig::AiTool(c) = tab {
            let expected = match c.id.as_str() {
                CLAUDE_TAB_ID => Some(false),
                CLAUDE_LOCAL_TAB_ID => Some(true),
                _ => None,
            };
            if let Some(want) = expected {
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

    // 4. Restore missing AI builtin entries at canonical positions.
    //    Inserting in the order claude(0), claude-local(1) works because
    //    each insert shifts later positions consistently. shell-default-1
    //    is intentionally not restored — it's a regular closable shell.
    if !settings.tabs.iter().any(|t| t.id() == CLAUDE_TAB_ID) {
        settings.tabs.insert(0, default_claude_tab());
        changed = true;
        tracing::warn!("integrity: restored missing claude tab");
    }

    if !settings.tabs.iter().any(|t| t.id() == CLAUDE_LOCAL_TAB_ID) {
        let pos = settings
            .tabs
            .iter()
            .position(|t| t.id() == CLAUDE_TAB_ID)
            .map(|p| p + 1)
            .unwrap_or(1);
        settings.tabs.insert(pos, default_claude_local_tab());
        changed = true;
        tracing::warn!("integrity: restored missing claude-local tab");
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
    fn integrity_seeds_ai_builtins_on_empty() {
        // The integrity check restores only the AI builtins; the closable
        // shell-default-1 ships via `seeded_defaults` on fresh installs and
        // is never re-created here (so closing it stays closed).
        let mut s = Settings::default();
        let shell = fake_default_shell();
        let changed = integrity_check(&mut s, &shell);
        assert!(changed);
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
        for t in &s.tabs {
            assert!(t.builtin(), "{} should be builtin", t.id());
        }
    }

    #[test]
    fn integrity_does_not_restore_shell_default_1() {
        // Closing shell-default-1 must persist across launches: the
        // integrity check should leave it absent.
        let mut s = Settings::default();
        let shell = fake_default_shell();
        integrity_check(&mut s, &shell);
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
        let shell = fake_default_shell();
        integrity_check(&mut s, &shell);
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
        let changed = integrity_check(&mut s, &shell);
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
        let shell = fake_default_shell();
        integrity_check(&mut s, &shell);
        // Tamper: flip claude's builtin to false.
        if let TabConfig::AiTool(c) = &mut s.tabs[0] {
            c.builtin = false;
        }
        let changed = integrity_check(&mut s, &shell);
        assert!(changed);
        assert!(s.tabs[0].builtin());
    }

    #[test]
    fn integrity_preserves_user_tabs() {
        let mut s = Settings::default();
        let shell = fake_default_shell();
        integrity_check(&mut s, &shell);
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
        let changed = integrity_check(&mut s, &shell);
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
        let shell = fake_default_shell();
        let mut s = Settings::default();
        integrity_check(&mut s, &shell);
        let text = serde_json::to_string(&s).unwrap();
        let parsed: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.tabs.len(), 2);
        assert_eq!(parsed.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(parsed.tabs[1].id(), CLAUDE_LOCAL_TAB_ID);
    }

    #[test]
    fn integrity_corrects_use_local_provider_on_reserved_ai_tabs() {
        // V1.4-07: a hand-edit must not be able to silently flip the
        // subscription Claude tab into local-LLM mode (or vice versa).
        let mut s = Settings::default();
        let shell = fake_default_shell();
        integrity_check(&mut s, &shell);

        // Tamper: flip claude → local, claude-local → not local.
        if let TabConfig::AiTool(c) = &mut s.tabs[0] {
            c.use_local_provider = true;
        }
        if let TabConfig::AiTool(c) = &mut s.tabs[1] {
            c.use_local_provider = false;
        }

        let changed = integrity_check(&mut s, &shell);
        assert!(changed);
        if let TabConfig::AiTool(c) = &s.tabs[0] {
            assert!(!c.use_local_provider, "claude must have use_local_provider=false");
        }
        if let TabConfig::AiTool(c) = &s.tabs[1] {
            assert!(c.use_local_provider, "claude-local must have use_local_provider=true");
        }
    }

    #[test]
    fn ui_theme_round_trip_and_default() {
        // Default file has ui.theme = "modern-dark".
        let s = Settings::default();
        assert_eq!(s.ui.theme, "modern-dark");

        // Round-trip preserves a hand-edited value.
        let mut s = Settings::default();
        s.ui.theme = "future-light".to_string();
        let text = serde_json::to_string(&s).unwrap();
        let parsed: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.ui.theme, "future-light");

        // A v1.3 file without the `ui` field still parses (serde(default)).
        let v1_3_json = r#"{"tabs":[]}"#;
        let parsed: Settings = serde_json::from_str(v1_3_json).unwrap();
        assert_eq!(parsed.ui.theme, "modern-dark");
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
        let shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global, &shell);

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
    fn save_writes_overlay_when_diff_nonempty_and_removes_when_empty() {
        let shell = fake_default_shell();
        let mut global = Settings::default();
        integrity_check(&mut global, &shell);

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
