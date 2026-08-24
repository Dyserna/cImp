//! Layout-tree and layout-preset use cases (V4-04).
//!
//! The frontend owns every structural mutation of the layout tree and pushes
//! the full serialized state here whenever it changes (debounced frontend
//! side). This layer's job is to slot the new tree into the settings struct and
//! let `SettingsHandle::mutate` drive the broadcast + debounced disk save.
//! Presets live in a separate `layout_presets` field; the CRUD below upserts /
//! renames / deletes by name.
//!
//! No layout-tree validation happens here at runtime — the frontend's
//! `validateAndRepairLayout` covers integrity for restored presets, and
//! `persistence::integrity_check` covers the load-from-disk path. These
//! operations trust their inputs.
//!
//! ## Its own module, and no sink
//!
//! Phase 0's locked decision 4: layout has no events. Every operation here is a
//! settings write, so [`LayoutService`] borrows exactly one handle and the
//! whole module is testable with a `SettingsHandle` and a scratch directory.
//! That is also why this is a separate module rather than four more methods on
//! [`SettingsService`](crate::service::settings::SettingsService): Phase B
//! moves the layout *model* (tree ops, ratio compensation, drop-target math,
//! the integrity sieve) into Rust, and this is the module it lands in.
//!
//! ## The invariant that outlives the wrap
//!
//! Every write is an atomic `mutate`, never `current()` + `set()`. A layout
//! save races tab create/close and `settings_update`, all three of which hold a
//! whole `Settings` snapshot; a read-modify-write outside the lock loses
//! whichever landed second. That is not a style preference, it is the
//! lost-update this shape exists to prevent, and [`Self::rename_preset`] takes
//! it one step further — it re-checks its preconditions *inside* the closure
//! and reports a no-op as an error rather than a false `Ok`.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{AppError, AppResult};
use crate::settings::{LayoutNodePersisted, LayoutPersisted, LayoutPreset, SettingsHandle};

/// The layout use cases, over one borrowed handle — same shape and rationale as
/// [`crate::service::tabs::TabService`].
pub struct LayoutService<'a> {
    settings: &'a SettingsHandle,
}

impl<'a> LayoutService<'a> {
    pub fn new(settings: &'a SettingsHandle) -> Self {
        Self { settings }
    }

    /// Persist the full layout tree + focused-pane id. Called by the frontend
    /// on every layout mutation, debounced 250ms there to coalesce splitter
    /// drags. The settings handle does its own 500ms debounce on the disk
    /// write, so a fast burst writes the file once.
    pub fn save(&self, layout: LayoutPersisted) -> AppResult<()> {
        // Atomic mutate so a concurrent tab create/close or settings_update can't
        // clobber the layout with a stale whole-struct snapshot (lost-update).
        self.settings.mutate(move |snap| {
            snap.layout = Some(layout);
        });
        Ok(())
    }

    /// Save the current layout tree under `name`. If a preset with that name
    /// already exists, replace it (keep the original `created_at` so "Recent
    /// presets" ordering doesn't jump on re-save). Otherwise append a fresh
    /// entry.
    pub fn save_preset(&self, name: String, tree: LayoutNodePersisted) -> AppResult<()> {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return Err(AppError::Settings(
                "save_layout_preset: name is empty".into(),
            ));
        }
        self.settings.mutate(move |snap| {
            if let Some(existing) = snap.layout_presets.iter_mut().find(|p| p.name == trimmed) {
                existing.tree = tree;
            } else {
                snap.layout_presets.push(LayoutPreset {
                    name: trimmed,
                    created_at: now_iso8601(),
                    tree,
                });
            }
        });
        Ok(())
    }

    /// Delete a preset by name. No-op if the name doesn't exist (callers don't
    /// need to know — the popover refreshes from the next `settings-changed`
    /// broadcast).
    pub fn delete_preset(&self, name: String) -> AppResult<()> {
        // Skip the broadcast/save when the name doesn't exist; the real delete is
        // an atomic mutate so it can't clobber a concurrent settings write.
        if self
            .settings
            .current()
            .layout_presets
            .iter()
            .any(|p| p.name == name)
        {
            self.settings.mutate(move |snap| {
                snap.layout_presets.retain(|p| p.name != name);
            });
        }
        Ok(())
    }

    /// Rename a preset. Errors if `old_name` doesn't exist or `new_name`
    /// collides with another preset (the frontend prompts before calling us —
    /// collision-here means a race with another window or a name that changed
    /// under us).
    pub fn rename_preset(&self, old_name: String, new_name: String) -> AppResult<()> {
        let new_trimmed = new_name.trim().to_string();
        if new_trimmed.is_empty() {
            return Err(AppError::Settings(
                "rename_layout_preset: new name is empty".into(),
            ));
        }
        if new_trimmed == old_name {
            return Ok(());
        }
        let snap = self.settings.current();
        if snap.layout_presets.iter().any(|p| p.name == new_trimmed) {
            return Err(AppError::Settings(format!(
                "rename_layout_preset: a preset named '{new_trimmed}' already exists"
            )));
        }
        if !snap.layout_presets.iter().any(|p| p.name == old_name) {
            return Err(AppError::Settings(format!(
                "rename_layout_preset: no preset named '{old_name}'"
            )));
        }
        drop(snap);
        // Perform the rename atomically, re-checking under the held lock so a
        // concurrent rename/delete can't be clobbered or produce a duplicate name.
        // Track whether the rename actually applied: if a concurrent op created
        // `new_trimmed` or removed `old_name` between our pre-check and the lock,
        // the closure no-ops — surface that as an error instead of a false `Ok`
        // that leaves the caller's UI believing the rename succeeded.
        let renamed = std::cell::Cell::new(false);
        self.settings.mutate(|snap| {
            if snap.layout_presets.iter().any(|p| p.name == new_trimmed) {
                return;
            }
            if let Some(target) = snap.layout_presets.iter_mut().find(|p| p.name == old_name) {
                target.name = new_trimmed.clone();
                renamed.set(true);
            }
        });
        if !renamed.get() {
            return Err(AppError::Settings(format!(
                "rename_layout_preset: '{old_name}' → '{new_trimmed}' did not apply (a concurrent change intervened)"
            )));
        }
        Ok(())
    }
}

/// Format the current UTC time as an RFC 3339 / ISO 8601 timestamp with
/// second precision (e.g. `2026-05-06T14:22:00Z`). Avoids pulling in
/// chrono for one call site — date math is straightforward at the
/// granularity we need (the popover only displays this, never parses
/// it).
fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert UNIX epoch seconds (UTC) into civil (Y, M, D, h, m, s).
/// Algorithm: Howard Hinnant's days_from_civil inverse — public domain
/// algorithm, leap-year-correct through year 9999.
fn epoch_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let hour = (tod / 3600) as u32;
    let minute = ((tod % 3600) / 60) as u32;
    let second = (tod % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (if month <= 2 { y + 1 } else { y }) as i32;
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use std::path::PathBuf;
    use uuid::Uuid;

    /// A throwaway directory to point [`SettingsHandle`] at, so the debounced
    /// saver writes its `.cimp/config.json` somewhere disposable. Same
    /// hand-rolled shape (and the same best-effort removal) as the tab and
    /// settings services'.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cimp-laysvc-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One handle and a scratch dir — the whole cost of making the layout
    /// popover headless.
    struct Fixture {
        settings: SettingsHandle,
        _scratch: ScratchDir,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = ScratchDir::new();
            let defaults = Settings::default();
            let settings = SettingsHandle::new(defaults.clone(), defaults, scratch.0.clone());
            Self {
                settings,
                _scratch: scratch,
            }
        }

        fn service(&self) -> LayoutService<'_> {
            LayoutService::new(&self.settings)
        }

        fn preset_names(&self) -> Vec<String> {
            self.settings
                .current()
                .layout_presets
                .iter()
                .map(|p| p.name.clone())
                .collect()
        }
    }

    /// A one-pane tree — these operations never read it, they only carry it.
    fn a_tree() -> LayoutNodePersisted {
        LayoutNodePersisted::Pane {
            id: "p1".to_string(),
            tab_ids: Vec::new(),
            active_tab_id: None,
        }
    }

    /// **Previously "user clicks in the app".** The live-verify recipe is the
    /// layout popover: save a preset, re-save it under the same name, rename
    /// it, delete it, and check the list after each step. Every one of those
    /// steps is a settings write with a precondition, and none of them had a
    /// test.
    #[test]
    fn preset_save_rename_delete_round_trip() {
        let fixture = Fixture::new();
        let svc = fixture.service();

        // Blank names are refused on both write paths.
        assert!(svc.save_preset("   ".to_string(), a_tree()).is_err());

        svc.save_preset("  Focus  ".to_string(), a_tree())
            .expect("save");
        assert_eq!(fixture.preset_names(), vec!["Focus"], "the name is trimmed");
        let created_at = fixture.settings.current().layout_presets[0]
            .created_at
            .clone();

        // Re-saving the same name replaces the tree and KEEPS `created_at`, so
        // "Recent presets" ordering doesn't jump under the user.
        svc.save_preset("Focus".to_string(), a_tree())
            .expect("re-save");
        assert_eq!(fixture.preset_names(), vec!["Focus"], "no duplicate row");
        assert_eq!(fixture.settings.current().layout_presets[0].created_at, created_at);

        // Rename: collisions and unknown names are errors, not silent no-ops.
        svc.save_preset("Wide".to_string(), a_tree()).expect("save");
        assert!(svc
            .rename_preset("Focus".to_string(), "Wide".to_string())
            .is_err());
        assert!(svc
            .rename_preset("Nope".to_string(), "Other".to_string())
            .is_err());
        assert!(svc
            .rename_preset("Focus".to_string(), "   ".to_string())
            .is_err());
        svc.rename_preset("Focus".to_string(), "Focus".to_string())
            .expect("renaming to the same name is a no-op, not an error");

        svc.rename_preset("Focus".to_string(), "Reading".to_string())
            .expect("rename");
        assert_eq!(fixture.preset_names(), vec!["Reading", "Wide"]);

        // Delete by name; an unknown name is a no-op the caller need not know
        // about.
        svc.delete_preset("Reading".to_string()).expect("delete");
        svc.delete_preset("Reading".to_string())
            .expect("deleting a name that is gone is not an error");
        assert_eq!(fixture.preset_names(), vec!["Wide"]);
    }

    /// **Previously "user clicks in the app".** `save_layout` is the command
    /// the frontend fires on every splitter drag, and the only observable is
    /// that the next launch restores what the user last saw.
    #[test]
    fn save_layout_lands_in_settings_and_broadcasts() {
        let fixture = Fixture::new();
        let mut watch = fixture.settings.subscribe();
        assert!(fixture.settings.current().layout.is_none());

        let layout = LayoutPersisted {
            tree: a_tree(),
            focused_pane_id: "p1".to_string(),
        };
        fixture.service().save(layout).expect("save");

        assert!(fixture.settings.current().layout.is_some());
        assert!(
            watch.try_recv().expect("a broadcast").layout.is_some(),
            "the settings-changed broadcast carries the new layout"
        );
    }

    #[test]
    fn epoch_zero_is_unix_epoch() {
        assert_eq!(epoch_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn known_timestamps_round_trip() {
        // 2026-05-06T00:00:00Z = 1778025600
        assert_eq!(epoch_to_ymdhms(1_778_025_600), (2026, 5, 6, 0, 0, 0));
        // 2000-02-29T12:34:56Z (leap day in a century year that's
        // divisible by 400) = 951_827_696
        assert_eq!(epoch_to_ymdhms(951_827_696), (2000, 2, 29, 12, 34, 56));
    }
}
