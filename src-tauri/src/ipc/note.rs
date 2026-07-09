//! Note-tab file I/O IPC commands.
//!
//! The Note tab is a rudimentary scratchpad (commands, ideas, throwaway
//! text). Its content lives in a single plain-text file inside the project's
//! `.cimp` data dir — `<launch_cwd>/.cimp/cimp.note.txt` — alongside the
//! settings overlay (`config.json`) and the code-graph store (`graph.db`).
//! Keeping it a loose `.txt` (rather than a field in settings JSON) is
//! deliberate: it's freely hand-editable, greppable, and never round-trips
//! through the settings diff/merge pipeline.
//!
//! The frontend `NoteView` autosaves via [`write_note`] (debounced on edit,
//! on a 5s timer, and on tab/app close); [`read_note`] loads the content when
//! the tab mounts and creates an empty file on first open. Writes go through
//! [`crate::settings::write_atomic`] so a crash mid-save can't truncate the
//! note, and the `.cimp` dir is created on demand.

use std::path::{Path, PathBuf};

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::ipc::AppState;

/// The per-project cImp data directory (holds `config.json`, `graph.db`, and
/// now the note). Mirrors `settings::persistence`'s private constant — kept a
/// literal here rather than shared to avoid coupling this module to the
/// settings internals.
const CIMP_DIR_NAME: &str = ".cimp";

/// The scratchpad file name inside `.cimp/`.
const NOTE_FILE_NAME: &str = "cimp.note.txt";

/// `<launch_cwd>/.cimp/cimp.note.txt`.
pub fn note_path(launch_cwd: &Path) -> PathBuf {
    launch_cwd.join(CIMP_DIR_NAME).join(NOTE_FILE_NAME)
}

/// Ensure the note file exists, creating the `.cimp` dir and an empty file if
/// it doesn't. Returns the resolved path. Called both by [`read_note`] and by
/// the tab-open flow so pressing the bottom-bar button "opens an existing note
/// or creates one".
pub fn ensure_note_file(launch_cwd: &Path) -> AppResult<PathBuf> {
    let path = note_path(launch_cwd);
    if !path.exists() {
        // `write_atomic` creates the parent `.cimp` dir and writes atomically.
        crate::settings::write_atomic(&path, b"")?;
    }
    Ok(path)
}

/// Load the note's text, creating an empty file (and `.cimp` dir) on first
/// open. A missing file therefore never surfaces as an error to the frontend.
#[tauri::command]
pub async fn read_note(state: State<'_, AppState>) -> AppResult<String> {
    let path = ensure_note_file(&state.launch.cwd)?;
    // Read bytes and lossily decode: the note is advertised as freely
    // hand-editable, so a stray non-UTF-8 byte (from an external editor or
    // tool) must degrade to a replacement char rather than making
    // `read_to_string` fail and lock the note out of the tab entirely.
    std::fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(AppError::Io)
}

/// Persist the note's text, replacing the file's contents atomically. The
/// `.cimp` dir is created on demand. Called by the frontend autosave.
#[tauri::command]
pub async fn write_note(state: State<'_, AppState>, content: String) -> AppResult<()> {
    let path = note_path(&state.launch.cwd);
    crate::settings::write_atomic(&path, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_path_lives_in_cimp_dir() {
        let cwd = Path::new("/project");
        assert_eq!(
            note_path(cwd),
            Path::new("/project").join(".cimp").join("cimp.note.txt")
        );
    }

    #[test]
    fn ensure_creates_empty_file_and_cimp_dir() {
        let dir = std::env::temp_dir().join(format!("cimp_note_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let path = ensure_note_file(&dir).unwrap();
        assert!(path.exists(), "note file should be created");
        assert_eq!(path, note_path(&dir));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert!(dir.join(".cimp").is_dir(), ".cimp dir should exist");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_preserves_existing_content() {
        let dir = std::env::temp_dir().join(format!("cimp_note_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let path = note_path(&dir);
        crate::settings::write_atomic(&path, b"kept notes").unwrap();
        let resolved = ensure_note_file(&dir).unwrap();
        assert_eq!(resolved, path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept notes");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
