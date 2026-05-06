//! JSON load/save with corruption recovery + version migrations.
//!
//! On-disk format is a single JSON object matching `Settings` (v1.2). Older
//! shapes are detected by their discriminator fields and routed through the
//! `migration` module. After the optional migration step, an integrity
//! check ensures the three reserved-id tab entries (claude, aider,
//! shell-default-1) exist with `builtin: true` — hand-edited files that
//! deleted them are repaired transparently.
//!
//! Either way `load` always returns a usable `Settings` — missing/corrupt
//! files become defaults (the corrupt original is moved aside as a `.bak`).

use std::fs;
use std::path::{Path, PathBuf};

use std::collections::HashSet;

use crate::error::{AppError, AppResult};
use crate::settings::migration;
use crate::settings::schema::{
    default_aider_tab, default_claude_tab, default_shell_1_tab, AiToolKindWire,
    LayoutNodePersisted, Settings, TabConfig, AIDER_TAB_ID, CLAUDE_TAB_ID, SHELL_DEFAULT_TAB_ID,
};
use crate::shell::ShellSpec;

const FILE_NAME: &str = "settings.json";

/// `%APPDATA%\cctts\settings.json` on Windows; the analogous config dir on
/// other platforms.
pub fn config_path() -> AppResult<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| AppError::Settings("no config dir on this platform".into()))?;
    Ok(dir.join("cctts").join(FILE_NAME))
}

/// Always returns a `Settings`. Defaults are written to disk when the file
/// is absent or corrupt; v1 / v1.1 files are migrated and rewritten in v1.2
/// shape with a backup of the original alongside.
pub fn load(default_shell: &ShellSpec) -> Settings {
    let path = match config_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "settings: cannot resolve config path; using defaults");
            return seeded_defaults(default_shell);
        }
    };

    if !path.exists() {
        let s = seeded_defaults(default_shell);
        if let Err(e) = save_to(&path, &s) {
            tracing::warn!(error = %e, path = %path.display(), "settings: write defaults failed");
        } else {
            tracing::info!(path = %path.display(), "settings: wrote defaults");
        }
        return s;
    }

    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "settings: read failed; using defaults");
            return seeded_defaults(default_shell);
        }
    };

    let mut value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: parse failed; quarantining file and resetting to defaults"
            );
            migration::quarantine_corrupt_file(&path);
            let s = seeded_defaults(default_shell);
            let _ = save_to(&path, &s);
            return s;
        }
    };

    let migrated = match migration::migrate_if_needed(&mut value, &path, default_shell) {
        Ok(b) => b,
        Err(e) => {
            // Backup write failed — don't proceed with the migration since
            // we'd lose the user's original. Surface defaults in-memory so
            // the app still launches; the file on disk is untouched.
            tracing::error!(
                error = %e,
                path = %path.display(),
                "settings: migration aborted (backup failed); using defaults in-session"
            );
            return seeded_defaults(default_shell);
        }
    };

    let mut settings: Settings = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: typed parse failed (post-migration); quarantining and resetting"
            );
            migration::quarantine_corrupt_file(&path);
            let s = seeded_defaults(default_shell);
            let _ = save_to(&path, &s);
            return s;
        }
    };

    let repaired = integrity_check(&mut settings, default_shell);

    if migrated || repaired {
        if let Err(e) = save_to(&path, &settings) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "settings: post-migration save failed (in-memory state retained)"
            );
        } else {
            tracing::info!(
                path = %path.display(),
                migrated,
                repaired,
                "settings: file refreshed after migration/integrity"
            );
        }
    } else {
        tracing::info!(path = %path.display(), "settings: loaded");
    }

    settings
}

pub fn save(settings: &Settings) -> AppResult<()> {
    let path = config_path()?;
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

/// `Settings::default()` with the three reserved-id tab entries seeded for
/// the host platform. Used on fresh installs and as the recovery fallback
/// when the file is unrecoverable. Equivalent to running the integrity
/// check against an empty `tabs` array.
fn seeded_defaults(default_shell: &ShellSpec) -> Settings {
    let mut s = Settings::default();
    integrity_check(&mut s, default_shell);
    s
}

/// Ensure the three reserved-id tab entries are present and marked as
/// builtins. Returns true if anything was changed (caller may want to
/// write back to disk). Logged as a warning when an entry has to be
/// restored — the typical cause is a hand-edited file.
///
/// The order is deterministic: claude first, then aider, then
/// shell-default-1, with each restored entry inserted at its canonical
/// position (front, after-claude, after-aider). User-created Shell tabs
/// retain their relative ordering after the three pinned entries.
pub fn integrity_check(settings: &mut Settings, default_shell: &ShellSpec) -> bool {
    let mut changed = false;

    // 1. Force builtin: true on the three reserved ids if they exist with
    //    builtin: false. Defends against hand-edits trying to flip the flag.
    let reserved = [CLAUDE_TAB_ID, AIDER_TAB_ID, SHELL_DEFAULT_TAB_ID];
    for tab in settings.tabs.iter_mut() {
        if reserved.contains(&tab.id()) && !tab.builtin() {
            tab.set_builtin(true);
            changed = true;
            tracing::warn!(id = tab.id(), "integrity: forced builtin: true on reserved tab");
        }
    }

    // 2. Coerce the AI builtins' ai_tool_kind in case a hand-edit set them
    //    to the wrong value (e.g. swapped claude/aider). The id is the
    //    canonical key; ai_tool_kind must follow.
    for tab in settings.tabs.iter_mut() {
        if let TabConfig::AiTool(c) = tab {
            let expected = match c.id.as_str() {
                CLAUDE_TAB_ID => Some(AiToolKindWire::ClaudeCode),
                AIDER_TAB_ID => Some(AiToolKindWire::Aider),
                _ => None,
            };
            if let Some(want) = expected {
                if c.ai_tool_kind != want {
                    c.ai_tool_kind = want;
                    changed = true;
                    tracing::warn!(
                        id = c.id,
                        "integrity: corrected ai_tool_kind on reserved AI tab"
                    );
                }
            }
        }
    }

    // 3. Restore missing reserved entries at canonical positions. Inserting
    //    in the order claude(0), aider(1), shell-default-1(after aider)
    //    works because each insert shifts later positions consistently.
    if !settings.tabs.iter().any(|t| t.id() == CLAUDE_TAB_ID) {
        settings.tabs.insert(0, default_claude_tab());
        changed = true;
        tracing::warn!("integrity: restored missing claude tab");
    }

    if !settings.tabs.iter().any(|t| t.id() == AIDER_TAB_ID) {
        let pos = settings
            .tabs
            .iter()
            .position(|t| t.id() == CLAUDE_TAB_ID)
            .map(|p| p + 1)
            .unwrap_or(1);
        settings.tabs.insert(pos, default_aider_tab());
        changed = true;
        tracing::warn!("integrity: restored missing aider tab");
    }

    if !settings.tabs.iter().any(|t| t.id() == SHELL_DEFAULT_TAB_ID) {
        let pos = settings
            .tabs
            .iter()
            .position(|t| t.id() == AIDER_TAB_ID)
            .map(|p| p + 1)
            .unwrap_or(2);
        settings
            .tabs
            .insert(pos, default_shell_1_tab(default_shell));
        changed = true;
        tracing::warn!("integrity: restored missing default shell tab");
    }

    // 4. Backend layout sanity. The frontend owns the deep integrity
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
    fn integrity_seeds_three_reserved_tabs_on_empty() {
        let mut s = Settings::default();
        let shell = fake_default_shell();
        let changed = integrity_check(&mut s, &shell);
        assert!(changed);
        assert_eq!(s.tabs.len(), 3);
        assert_eq!(s.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(s.tabs[1].id(), AIDER_TAB_ID);
        assert_eq!(s.tabs[2].id(), SHELL_DEFAULT_TAB_ID);
        for t in &s.tabs {
            assert!(t.builtin(), "{} should be builtin", t.id());
        }
    }

    #[test]
    fn integrity_forces_builtin_true_on_reserved_ids() {
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
        assert_eq!(parsed.tabs.len(), 3);
        assert_eq!(parsed.tabs[0].id(), CLAUDE_TAB_ID);
        assert_eq!(parsed.tabs[1].id(), AIDER_TAB_ID);
        assert_eq!(parsed.tabs[2].id(), SHELL_DEFAULT_TAB_ID);
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
}
