//! Per-project UI state file I/O (V42 Phase C).
//!
//! Per-view UI state — which sub-tab a view was last on, which cards were
//! expanded, the Events table's column widths, the audit filters, the set of
//! UI-hidden tabs — used to live in the webview's `localStorage`. That made it
//! per-*machine* rather than per-*project*: hiding the Workbench tab in one
//! checkout hid it in every checkout opened by the same install, and none of
//! it survived a webview data reset. It now lives in
//! `<launch_cwd>/.cimp/ui_state.json`, next to the settings overlay
//! (`config.json`), the note (`cimp.note.txt`) and the code-graph store
//! (`graph.db`) — the same per-project data dir, the same ownership model as
//! [`crate::ipc::note`].
//!
//! Deliberately NOT part of the settings struct: these are high-frequency,
//! low-stakes view toggles. Routing them through the settings
//! diff/merge/broadcast pipeline would re-emit `settings-changed` to every
//! window on every `<details>` toggle, and would put throwaway view state into
//! the file users hand-edit and diff.
//!
//! ## Shape
//!
//! ```json
//! { "version": 1, "values": { "cimp.view-section.v1.workbench": "diff" } }
//! ```
//!
//! `values` is an opaque string -> JSON map. This layer **stores and returns
//! values verbatim** and never interprets one: every validity rule (which
//! section ids exist, the `#rrggbb` colour regex, the severity enum, the
//! column-width clamps) lives at the frontend call site that owns it, exactly
//! where it lived before the move. Keys are the frontend's own — currently the
//! literal `localStorage` key strings, so the one-time import is a copy.
//!
//! ## Failure posture
//!
//! Inherited from the code this replaces: *losing* view state must never break
//! the UI. A missing file, unreadable bytes, invalid JSON or a `version` this
//! build doesn't know all read back as "no saved state" — defaults, never an
//! error surface. Only a genuine write failure is reported, and the frontend
//! ignores that too (fire-and-forget).
//!
//! ## Writes
//!
//! [`ui_state_set`] takes a *patch*, not the whole object: only the keys the
//! caller touched are written, and a `null` value removes a key. That is what
//! makes a second webview safe — the settings window transitively bundles the
//! frontend modules that own this state, and a whole-object write from a
//! window that never hydrated would silently wipe the main window's state.
//! Each patch is a read-modify-write under [`UI_STATE_LOCK`] so two concurrent
//! callers can't lose each other's keys, committed through
//! [`crate::settings::write_atomic`]. There is no backend debounce: the
//! frontend already coalesces bursts (~250 ms) before it calls, the same
//! division of labour `save_layout` uses.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::ipc::AppState;

/// The per-project cImp data dir. Mirrors [`crate::ipc::note`]'s private
/// constant for the same reason: keeping this module uncoupled from the
/// settings internals.
const CIMP_DIR_NAME: &str = ".cimp";

/// The UI-state file name inside `.cimp/`.
const UI_STATE_FILE_NAME: &str = "ui_state.json";

/// Schema version written into every file from day one, so a future shape
/// change has something to branch on instead of guessing. A file carrying any
/// other version reads as empty (see [`read_ui_state`]).
pub const UI_STATE_VERSION: u32 = 1;

/// Serializes the read-modify-write in [`merge_ui_state`]. Two windows (main +
/// settings) can patch concurrently; without this, the later read could
/// predate the earlier write and drop its keys.
static UI_STATE_LOCK: Mutex<()> = Mutex::new(());

/// The whole file: a version stamp plus the opaque key -> value map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiState {
    pub version: u32,
    /// Opaque to this layer — see the module docs.
    #[serde(default)]
    pub values: Map<String, Value>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            version: UI_STATE_VERSION,
            values: Map::new(),
        }
    }
}

/// `<launch_cwd>/.cimp/ui_state.json`.
pub fn ui_state_path(launch_cwd: &Path) -> PathBuf {
    launch_cwd.join(CIMP_DIR_NAME).join(UI_STATE_FILE_NAME)
}

/// Load the saved state. Total function: absent file, unreadable bytes,
/// non-UTF-8, malformed JSON, a JSON value that isn't an object, or a
/// `version` this build doesn't recognise all yield [`UiState::default`] —
/// "no saved view state", which every consumer already renders as its
/// built-in default. Nothing here is worth an error dialog.
pub fn read_ui_state(launch_cwd: &Path) -> UiState {
    let path = ui_state_path(launch_cwd);
    let Ok(bytes) = std::fs::read(&path) else {
        return UiState::default();
    };
    match serde_json::from_slice::<UiState>(&bytes) {
        Ok(state) if state.version == UI_STATE_VERSION => state,
        // A newer/older/absent version is not upgraded in place: the next
        // write replaces the file wholesale. Acceptable precisely because the
        // payload is disposable view state; a future migration hooks in here.
        _ => UiState::default(),
    }
}

/// Apply `patch` to the saved state and persist the result.
///
/// A `null` value removes its key (the frontend's `saveViewString(view, key,
/// null)` path); any other value replaces it verbatim. An empty patch still
/// materialises the file, which is what lets the frontend's one-time import
/// record its marker on a project that has nothing else to save yet.
pub fn merge_ui_state(launch_cwd: &Path, patch: Map<String, Value>) -> AppResult<UiState> {
    // Poison recovery: a panic in another writer must not permanently disable
    // saving view state. The data behind the guard is `()`; there is no
    // invariant a panic could have half-broken.
    let _guard = UI_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut state = read_ui_state(launch_cwd);
    for (key, value) in patch {
        if value.is_null() {
            state.values.remove(&key);
        } else {
            state.values.insert(key, value);
        }
    }
    state.version = UI_STATE_VERSION;

    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|e| AppError::Settings(format!("failed to serialize ui_state.json: {e}")))?;
    crate::settings::write_atomic(&ui_state_path(launch_cwd), &bytes)?;
    Ok(state)
}

/// Read the whole UI-state object. Called exactly once per webview, before
/// `mount(App)`, to fill the frontend's synchronous cache — the loads it backs
/// all run inside `$state(...)` initialisers and must answer before first
/// paint, so there is no place for a per-key async read.
#[tauri::command]
pub async fn ui_state_get(state: State<'_, AppState>) -> AppResult<UiState> {
    Ok(read_ui_state(&state.launch.cwd))
}

/// Merge a patch of touched keys into the saved state. See the module docs for
/// why this is a patch rather than a whole-object replace.
#[tauri::command]
pub async fn ui_state_set(state: State<'_, AppState>, patch: Map<String, Value>) -> AppResult<()> {
    merge_ui_state(&state.launch.cwd, patch).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scratch project dir that cleans itself up.
    struct Project(PathBuf);

    impl Project {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("cimp_ui_state_{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        /// Write raw bytes over the ui-state file, bypassing the writer.
        fn poison(&self, raw: &str) {
            crate::settings::write_atomic(&ui_state_path(&self.0), raw.as_bytes()).unwrap();
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn patch(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn ui_state_lives_beside_the_note_in_the_cimp_dir() {
        assert_eq!(
            ui_state_path(Path::new("/project")),
            Path::new("/project").join(".cimp").join("ui_state.json")
        );
    }

    #[test]
    fn a_project_with_no_file_reads_as_empty_not_an_error() {
        let p = Project::new();
        let state = read_ui_state(p.path());
        assert_eq!(state, UiState::default());
        assert!(state.values.is_empty());
        // Reading must not create the file — only a write does.
        assert!(!ui_state_path(p.path()).exists());
    }

    #[test]
    fn a_patch_round_trips_verbatim_and_stamps_the_version() {
        let p = Project::new();
        merge_ui_state(
            p.path(),
            patch(&[
                ("cimp.view-section.v1.workbench", json!("diff")),
                // The Events column widths are JSON *inside* a string. This
                // layer must not "helpfully" parse or re-encode that.
                ("cimp.view-pref.v1.events.col-widths", json!("{\"ts\":120}")),
            ]),
        )
        .unwrap();

        let state = read_ui_state(p.path());
        assert_eq!(state.version, UI_STATE_VERSION);
        assert_eq!(
            state.values.get("cimp.view-section.v1.workbench"),
            Some(&json!("diff"))
        );
        assert_eq!(
            state.values.get("cimp.view-pref.v1.events.col-widths"),
            Some(&json!("{\"ts\":120}"))
        );
    }

    #[test]
    fn a_patch_merges_rather_than_replacing_the_whole_object() {
        let p = Project::new();
        merge_ui_state(p.path(), patch(&[("a", json!("1")), ("b", json!("2"))])).unwrap();
        merge_ui_state(p.path(), patch(&[("b", json!("changed"))])).unwrap();

        let state = read_ui_state(p.path());
        assert_eq!(state.values.get("a"), Some(&json!("1")), "untouched key kept");
        assert_eq!(state.values.get("b"), Some(&json!("changed")));
    }

    #[test]
    fn a_null_value_removes_its_key() {
        let p = Project::new();
        merge_ui_state(
            p.path(),
            patch(&[("gone", json!("here")), ("kept", json!("x"))]),
        )
        .unwrap();
        merge_ui_state(p.path(), patch(&[("gone", Value::Null)])).unwrap();

        let state = read_ui_state(p.path());
        assert!(!state.values.contains_key("gone"));
        assert_eq!(state.values.get("kept"), Some(&json!("x")));
    }

    #[test]
    fn an_empty_patch_still_materialises_the_file() {
        let p = Project::new();
        merge_ui_state(p.path(), Map::new()).unwrap();
        assert!(ui_state_path(p.path()).exists());
        assert_eq!(read_ui_state(p.path()), UiState::default());
    }

    #[test]
    fn a_corrupt_file_reads_as_empty_and_the_next_write_repairs_it() {
        let p = Project::new();
        p.poison("{ this is not json");
        assert_eq!(read_ui_state(p.path()), UiState::default());

        // The repair path: a normal write over the garbage must succeed, not
        // inherit the corruption.
        merge_ui_state(p.path(), patch(&[("k", json!("v"))])).unwrap();
        assert_eq!(read_ui_state(p.path()).values.get("k"), Some(&json!("v")));
    }

    #[test]
    fn a_non_object_json_file_reads_as_empty() {
        let p = Project::new();
        p.poison("[1, 2, 3]");
        assert_eq!(read_ui_state(p.path()), UiState::default());
    }

    #[test]
    fn an_unknown_version_reads_as_empty() {
        let p = Project::new();
        p.poison(r#"{"version": 999, "values": {"a": "1"}}"#);
        assert_eq!(
            read_ui_state(p.path()),
            UiState::default(),
            "a file this build doesn't understand is defaults, never a parse error"
        );
    }

    #[test]
    fn a_file_missing_the_values_map_reads_as_empty_at_the_right_version() {
        let p = Project::new();
        p.poison(r#"{"version": 1}"#);
        let state = read_ui_state(p.path());
        assert_eq!(state.version, UI_STATE_VERSION);
        assert!(state.values.is_empty());
    }

    #[test]
    fn the_file_is_pretty_printed_so_it_stays_hand_inspectable() {
        let p = Project::new();
        merge_ui_state(p.path(), patch(&[("a", json!("1"))])).unwrap();
        let text = std::fs::read_to_string(ui_state_path(p.path())).unwrap();
        assert!(text.contains('\n'), "expected pretty JSON, got: {text}");
        assert!(text.contains("\"version\""));
    }

    #[test]
    fn concurrent_patches_do_not_lose_each_others_keys() {
        let p = Project::new();
        let dir = p.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    merge_ui_state(&dir, patch(&[(&format!("k{i}"), json!(i))])).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let state = read_ui_state(p.path());
        for i in 0..8 {
            assert_eq!(
                state.values.get(&format!("k{i}")),
                Some(&json!(i)),
                "writer {i} lost its key"
            );
        }
    }
}
