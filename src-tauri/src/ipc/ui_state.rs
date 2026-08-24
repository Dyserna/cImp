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
//! [`crate::settings::write_atomic`]. There is no backend debounce and no
//! backend coalescing: the frontend batches the writes one event handler makes
//! into a single patch — V42 review RV-2 replaced its 250 ms timer with a
//! same-tick microtask, so a window closing can no longer eat the last toggle
//! — and this side commits whatever arrives.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tauri::State;
use tracing::warn;

use crate::error::{AppError, AppResult};
// The per-project cImp data dir. One name for every owner since the V42
// review; this module used to declare a third copy of the literal.
use crate::fsutil::CIMP_DIR_NAME;
use crate::ipc::AppState;

/// The UI-state file name inside `.cimp/`.
const UI_STATE_FILE_NAME: &str = "ui_state.json";

/// The advisory lock file inside `.cimp/`, beside the state it guards.
const UI_STATE_LOCK_FILE_NAME: &str = "ui_state.lock";

/// Schema version written into every file from day one, so a future shape
/// change has something to branch on instead of guessing. A file carrying any
/// other version reads as empty (see [`read_ui_state`]).
pub const UI_STATE_VERSION: u32 = 1;

/// Serializes the read-modify-write in [`merge_ui_state`] **within this
/// process**. Two windows (main + settings) can patch concurrently; without
/// this, the later read could predate the earlier write and drop its keys.
///
/// Kept alongside the file lock below rather than replaced by it: it is free,
/// it is what actually serialises the common case (two webviews of one app),
/// and it holds even on a filesystem whose advisory locking is a no-op.
static UI_STATE_LOCK: Mutex<()> = Mutex::new(());

/// `<launch_cwd>/.cimp/ui_state.lock` — the advisory lock file.
fn ui_state_lock_path(launch_cwd: &Path) -> PathBuf {
    launch_cwd
        .join(CIMP_DIR_NAME)
        .join(UI_STATE_LOCK_FILE_NAME)
}

/// Take the **cross-process** exclusive lock for this project's UI state, held
/// until the returned handle drops.
///
/// V42 review, RV-5. [`UI_STATE_LOCK`] is process-local, and one project root
/// can legitimately have two cImp instances on it — a second launch in the
/// same directory, a `cimp --statusline` helper, a dev build beside an
/// installed one. Their read-modify-writes interleave: A reads, B reads, A
/// writes, B writes, and A's keys are gone. The window is small and the data
/// is disposable, which is why this is a lock and not a merge protocol — but
/// "small" is not "closed", and the whole point of the V42 Phase C move was to
/// stop losing per-project view state.
///
/// **A separate lock file, not a lock on `ui_state.json`.** The commit is
/// [`crate::settings::write_atomic`], which renames a fresh file over the
/// target: a lock held on the old file would guard an inode that is about to
/// stop being the state, and on Windows an open handle on the destination can
/// block the rename outright. A dedicated file has neither problem and is
/// never read or written, only locked.
///
/// **Failure is not fatal.** A filesystem that cannot create or lock the file
/// (an exotic network mount, a read-only volume — where the write is going to
/// fail anyway) degrades to the process-local mutex, which is exactly the
/// behaviour that shipped before this. Losing view state must never break the
/// UI; refusing to save it because a lock file could not be made would be the
/// opposite trade.
fn lock_ui_state(launch_cwd: &Path) -> Option<File> {
    let path = ui_state_lock_path(launch_cwd);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(error = %e, "ui_state: cannot create .cimp for the lock file; writing unlocked");
            return None;
        }
    }
    // `create(true)` + `write(true)` without `truncate`: the file's CONTENT is
    // never used, so there is nothing to truncate and nothing to read.
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            warn!(error = %e, "ui_state: cannot open the lock file; writing unlocked");
            return None;
        }
    };
    match file.lock() {
        Ok(()) => Some(file),
        Err(e) => {
            warn!(error = %e, "ui_state: cannot take the advisory lock; writing unlocked");
            None
        }
    }
}

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

/// The `version` stamped on the file **as bytes on disk**, when there is one.
///
/// [`read_ui_state`] deliberately answers `default()` for every unreadable
/// shape, which erases the difference between "corrupt" and "written by a
/// build newer than this one". Writing needs that difference, so it reads the
/// raw JSON instead. `None` means absent, unreadable, not JSON, not an object,
/// or carrying no numeric `version` — none of which is a future file, and all
/// of which the replace path repairs.
fn on_disk_version(launch_cwd: &Path) -> Option<u64> {
    let bytes = std::fs::read(ui_state_path(launch_cwd)).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("version")?
        .as_u64()
}

/// Apply `patch` to the saved state and persist the result.
///
/// A `null` value removes its key (the frontend's `saveViewString(view, key,
/// null)` path); any other value replaces it verbatim. An empty patch still
/// materialises the file, which is what lets the frontend's one-time import
/// record its marker on a project that has nothing else to save yet.
///
/// **Refuses to write over a FUTURE file** (V42 review, RV-10). A `version`
/// this build does not recognise reads as empty ([`read_ui_state`]), which is
/// right for a read and was catastrophic for a write: the frontend's one-time
/// import saw no marker, patched its marker in, and this function replaced the
/// whole newer file with `{version: 1, values: {marker}}`. Downgrading a build
/// — running an older cImp against a project a newer one has opened — silently
/// destroyed that project's view state. The refusal surfaces as an `Err` the
/// frontend swallows (it logs once and keeps its in-memory cache), so an old
/// build stays perfectly usable on such a project; it just stops persisting.
/// A CORRUPT file is not a future one and still takes the replace path: that
/// is repair, not destruction.
pub fn merge_ui_state(launch_cwd: &Path, patch: Map<String, Value>) -> AppResult<UiState> {
    // Poison recovery: a panic in another writer must not permanently disable
    // saving view state. The data behind the guard is `()`; there is no
    // invariant a panic could have half-broken.
    let _guard = UI_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Cross-process, dropped with the function. Everything from the version
    // probe through the rename is inside it, so another instance cannot slip a
    // write between this read and this commit.
    let _file_guard = lock_ui_state(launch_cwd);

    if let Some(on_disk) = on_disk_version(launch_cwd) {
        if on_disk > u64::from(UI_STATE_VERSION) {
            return Err(AppError::Settings(format!(
                "ui_state.json is version {on_disk}, newer than this build understands \
                 ({UI_STATE_VERSION}); refusing to overwrite it"
            )));
        }
    }

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
///
/// On a blocking pool, like [`ui_state_set`] — see its note.
///
/// **Left as a direct call** (V42 Phase A): the body is one blocking hop over
/// [`read_ui_state`], which is a free function in this module that the tests
/// below already drive against a scratch project. Everything a service could
/// hold — the version probe, the merge rule, the cross-process lock — is in
/// those functions, not in the command.
#[tauri::command]
pub async fn ui_state_get(state: State<'_, AppState>) -> AppResult<UiState> {
    let cwd = state.launch.cwd.clone();
    tauri::async_runtime::spawn_blocking(move || read_ui_state(&cwd))
        .await
        .map_err(|e| AppError::Settings(format!("ui_state read task failed: {e}")))
}

/// Merge a patch of touched keys into the saved state. See the module docs for
/// why this is a patch rather than a whole-object replace.
///
/// **On a blocking pool, not the async worker.** Every step here is blocking
/// filesystem work — the version probe, the read, and
/// [`crate::settings::write_atomic`]'s `sync_all()`, which is a real fsync and
/// on a slow or networked volume can take tens of milliseconds. Two of the
/// waits are unbounded by construction: the cross-process advisory lock in
/// [`lock_ui_state`], and the process-local `Mutex` behind it. An `async fn`
/// tauri command runs on a tokio worker, and parking one on an fsync or a lock
/// stalls every other future scheduled on it — for a `<details>` toggle. It
/// also blocks the frontend's pre-mount `ui_state_get`, which is the one call
/// with a deadline (V42 review RV-3).
///
/// **Left as a direct call**, for [`ui_state_get`]'s reason: [`merge_ui_state`]
/// is where the patch semantics live, and it is already a free function with
/// tests.
#[tauri::command]
pub async fn ui_state_set(state: State<'_, AppState>, patch: Map<String, Value>) -> AppResult<()> {
    let cwd = state.launch.cwd.clone();
    tauri::async_runtime::spawn_blocking(move || merge_ui_state(&cwd, patch))
        .await
        .map_err(|e| AppError::Settings(format!("ui_state write task failed: {e}")))?
        .map(|_| ())
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

    /// V42 review RV-10. Reading a future file as empty is right; WRITING over
    /// it on the strength of that reading is how a downgraded build destroys a
    /// project's view state — the frontend sees no import marker, patches one
    /// in, and the whole newer file is replaced by `{version: 1, {marker}}`.
    #[test]
    fn a_future_version_file_refuses_the_write_and_keeps_its_bytes() {
        let p = Project::new();
        let future = r#"{"version": 999, "values": {"a": "1"}}"#;
        p.poison(future);

        let err = merge_ui_state(p.path(), patch(&[("b", json!("2"))]))
            .expect_err("a build that cannot read the file must not replace it");
        assert!(
            format!("{err}").contains("999"),
            "the refusal should name the version it found: {err}"
        );

        assert_eq!(
            std::fs::read_to_string(ui_state_path(p.path())).unwrap(),
            future,
            "not one byte of the newer file may change"
        );
    }

    #[test]
    fn a_corrupt_file_is_not_a_future_file() {
        // The distinction the refusal rests on: `read_ui_state` answers
        // `default()` for both, so the writer reads the raw JSON instead.
        // Garbage still gets repaired by the next write; only a readable,
        // higher `version` is protected.
        let p = Project::new();
        for garbage in [
            "{ this is not json",
            "[1, 2, 3]",
            "null",
            r#"{"values": {"a": "1"}}"#,        // no version at all
            r#"{"version": "999", "values": {}}"#, // version is not a number
        ] {
            p.poison(garbage);
            merge_ui_state(p.path(), patch(&[("k", json!("v"))]))
                .unwrap_or_else(|e| panic!("{garbage} must be repairable, got {e}"));
            assert_eq!(read_ui_state(p.path()).values.get("k"), Some(&json!("v")));
        }
    }

    #[test]
    fn an_older_version_file_is_still_replaceable() {
        // Only a FUTURE version is protected. A file this build has outgrown
        // reads as empty and the next write replaces it, which is the
        // migration path the version stamp exists for.
        let p = Project::new();
        p.poison(r#"{"version": 0, "values": {"a": "1"}}"#);
        merge_ui_state(p.path(), patch(&[("k", json!("v"))])).unwrap();
        let state = read_ui_state(p.path());
        assert_eq!(state.version, UI_STATE_VERSION);
        assert_eq!(state.values.get("k"), Some(&json!("v")));
    }

    /// V42 review RV-5. The process-local `Mutex` says nothing about a second
    /// cImp on the same project root, and their read-modify-writes interleave.
    /// Two handles on the lock file stand in for two processes: `flock` on
    /// Unix and `LockFileEx` on Windows both conflict per-HANDLE, so a second
    /// holder is refused inside one process exactly as it is across two.
    #[test]
    fn the_advisory_lock_excludes_a_second_holder() {
        let p = Project::new();
        merge_ui_state(p.path(), Map::new()).unwrap();
        assert!(
            ui_state_lock_path(p.path()).exists(),
            "a write must have materialised the lock file"
        );

        let held = lock_ui_state(p.path()).expect("the first holder takes the lock");

        let second = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(ui_state_lock_path(p.path()))
            .expect("open the lock file again");
        assert!(
            second.try_lock().is_err(),
            "a second holder was admitted while the first still holds the lock — \
             two instances on one project root would clobber each other's keys"
        );

        // …and it is released with the handle, not held for the process.
        drop(held);
        assert!(
            second.try_lock().is_ok(),
            "the lock outlived its holder"
        );
    }

    #[test]
    fn the_lock_file_lives_beside_the_state_it_guards() {
        assert_eq!(
            ui_state_lock_path(Path::new("/project")),
            Path::new("/project").join(".cimp").join("ui_state.lock")
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
