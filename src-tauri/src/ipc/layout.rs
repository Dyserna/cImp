//! Layout-tree and layout-preset IPC commands (V4-04).
//!
//! The frontend owns every structural mutation of the layout tree and
//! pushes the full serialized state here whenever it changes (debounced
//! frontend-side). The backend's job is to slot the new tree into the
//! settings struct and let `SettingsHandle::set` drive the broadcast +
//! debounced disk save. Presets live in a separate `layout_presets`
//! field; CRUD ops below upsert / rename / delete by name.
//!
//! `save_layout` trusts its input: the frontend just built that tree with the
//! tree ops, and re-validating every splitter frame would buy nothing. The two
//! places a tree arrives from somewhere the frontend did NOT just build it —
//! the settings file at load, and a preset saved against a different tab list —
//! both run `settings::layout`'s integrity walk: `persistence::integrity_check`
//! for the first, [`restore_layout_preset`] for the second.

use std::time::{SystemTime, UNIX_EPOCH};

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::ipc::AppState;
use crate::settings::{LayoutNodePersisted, LayoutPersisted, LayoutPreset};

/// Persist the full layout tree + focused-pane id. Called by the frontend on
/// EVERY layout-store mutation — no frontend debounce, deliberately (see
/// `installLayoutPersistence`: the 250 ms one it used to have left a
/// closing-race window where the last edit before teardown was dropped). A
/// splitter drag therefore lands here once per pointer event.
///
/// Coalescing is the settings handle's 500 ms debounce on the disk write, so a
/// fast burst writes the file once.
///
/// **Quiet write.** `mutate_quiet` instead of `mutate`: the in-memory store is
/// updated and the save is requested, but no `settings-changed` is broadcast.
/// Nothing consumes a layout change through that channel — see
/// `SettingsHandle::mutate_quiet` for the audit — and broadcasting cloned the
/// whole `Settings` struct to every webview per pointer event to deliver a
/// value none of them read.
#[tauri::command]
pub async fn save_layout(state: State<'_, AppState>, layout: LayoutPersisted) -> AppResult<()> {
    // Atomic mutate so a concurrent tab create/close or settings_update can't
    // clobber the layout with a stale whole-struct snapshot (lost-update).
    state.settings.mutate_quiet(move |snap| {
        snap.layout = Some(layout);
    });
    Ok(())
}

/// Save the current layout tree under `name`. If a preset with that
/// name already exists, replace it (keep the original `created_at` so
/// "Recent presets" ordering doesn't jump on re-save). Otherwise
/// append a fresh entry.
#[tauri::command]
pub async fn save_layout_preset(
    state: State<'_, AppState>,
    name: String,
    tree: LayoutNodePersisted,
) -> AppResult<()> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err(AppError::Settings(
            "save_layout_preset: name is empty".into(),
        ));
    }
    state.settings.mutate(move |snap| {
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

/// Restore a preset: return its tree adapted to the live tab list, ready for
/// the frontend to drop straight into the layout store.
///
/// A preset carries a tree and nothing else — no focus (restoring one is "set
/// up panes this way"; focus follows the user's next click) and no promise that
/// the tabs it names still exist. Adapting it is exactly the hydration problem:
/// drop tabs deleted since the save, place tabs created since it as orphans,
/// leave hidden tabs out, clamp ratios, collapse whatever that emptied. So it
/// runs the same [`crate::settings::layout::repair`] the load path runs, which
/// is the point of doing it here — V42 Phase B, the repair rules exist exactly
/// once, and the frontend copy that used to run at this call site is gone.
///
/// Focus is seeded from the leftmost leaf before the repair, which then
/// validates it like any other persisted focus.
///
/// **Reads, never writes.** The restored layout reaches settings through the
/// frontend's ordinary save-on-change path, like any other layout mutation — a
/// preset restore is not a special kind of write.
#[tauri::command]
pub async fn restore_layout_preset(
    state: State<'_, AppState>,
    name: String,
) -> AppResult<LayoutPersisted> {
    let snap = state.settings.current();
    let Some(preset) = snap.layout_presets.iter().find(|p| p.name == name) else {
        return Err(AppError::Settings(format!(
            "restore_layout_preset: no preset named '{name}'"
        )));
    };
    let mut layout = LayoutPersisted {
        focused_pane_id: crate::settings::layout::leftmost_pane_id(&preset.tree),
        tree: preset.tree.clone(),
    };
    let tab_ids: Vec<&str> = snap.tabs.iter().map(|t| t.id()).collect();
    // The hidden set is a small per-project file read; off the async worker for
    // the same reason `ui_state_get` is (see its note).
    let cwd = state.launch.cwd.clone();
    let hidden =
        tauri::async_runtime::spawn_blocking(move || crate::ipc::ui_state::read_hidden_tabs(&cwd))
            .await
            .map_err(|e| AppError::Settings(format!("hidden-tab read task failed: {e}")))?;
    crate::settings::layout::repair(&mut layout, &tab_ids, &hidden);
    Ok(layout)
}

/// Delete a preset by name. No-op if the name doesn't exist (callers
/// don't need to know — the popover refreshes from the next
/// `settings-changed` broadcast).
#[tauri::command]
pub async fn delete_layout_preset(state: State<'_, AppState>, name: String) -> AppResult<()> {
    // Skip the broadcast/save when the name doesn't exist; the real delete is
    // an atomic mutate so it can't clobber a concurrent settings write.
    if state
        .settings
        .current()
        .layout_presets
        .iter()
        .any(|p| p.name == name)
    {
        state.settings.mutate(move |snap| {
            snap.layout_presets.retain(|p| p.name != name);
        });
    }
    Ok(())
}

/// Rename a preset. Errors if `old_name` doesn't exist or `new_name`
/// collides with another preset (the frontend prompts before calling
/// us — collision-here means a race with another window or a name that
/// changed under us).
#[tauri::command]
pub async fn rename_layout_preset(
    state: State<'_, AppState>,
    old_name: String,
    new_name: String,
) -> AppResult<()> {
    let new_trimmed = new_name.trim().to_string();
    if new_trimmed.is_empty() {
        return Err(AppError::Settings(
            "rename_layout_preset: new name is empty".into(),
        ));
    }
    if new_trimmed == old_name {
        return Ok(());
    }
    let snap = state.settings.current();
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
    state.settings.mutate(|snap| {
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

    /// V42 Phase B. Structural because the thing worth defending is which
    /// handle method this command calls, and there is no way to observe that
    /// from a unit test without an `AppState`. Swapping it back to `mutate`
    /// would restore a whole-`Settings` broadcast to every webview once per
    /// pointer event during a splitter drag — see the doc comments on both
    /// sides for why nothing is listening. Newline-agnostic: CI checks this
    /// tree out with CRLF.
    #[test]
    fn save_layout_writes_quietly() {
        let src = include_str!("layout.rs");
        let start = src
            .find("pub async fn save_layout(")
            .expect("`save_layout` is gone — re-point this test");
        let body = &src[start..];
        let body = &body[..body.find("\n}").unwrap_or(body.len())];
        assert!(
            body.contains("mutate_quiet"),
            "`save_layout` must use `mutate_quiet`: it runs once per pointer event during a \
             splitter drag, and `mutate` clones and broadcasts the whole Settings struct to \
             every window each time"
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
