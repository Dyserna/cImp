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
// The per-project cImp data directory (holds `config.json`, `graph.db`, and
// now the note). This was a private literal re-spelled here; the V42 review
// folded the three copies into one — see [`crate::fsutil::CIMP_DIR_NAME`].
use crate::fsutil::CIMP_DIR_NAME;
use crate::ipc::AppState;

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

/// Load one project's note text, creating an empty file (and `.cimp` dir) on
/// first open. A missing file therefore never surfaces as an error to the
/// frontend.
///
/// **Bytes, then a lossy decode — never `read_to_string`.** The note is
/// advertised as freely hand-editable, so a stray non-UTF-8 byte (an external
/// editor saving latin-1, a tool appending raw bytes) must degrade to a
/// replacement char. `read_to_string` would return `Err` instead, and the tab
/// would report a load failure for a file the user can open perfectly well in
/// any editor — locking them out of their own scratchpad with no way back but
/// deleting it.
///
/// Split from the command (V42 Phase A1-3) so that rule is checkable. The
/// command adds nothing but `AppState`, and the input that exercises the rule
/// is a byte only the filesystem can plant — not something a WebView can be
/// asked to produce.
pub fn read_note_at(launch_cwd: &Path) -> AppResult<String> {
    let path = ensure_note_file(launch_cwd)?;
    std::fs::read(&path)
        .map(|bytes| decode(&bytes))
        .map_err(AppError::Io)
}

/// The note file's bytes as text, never failing. See [`read_note_at`].
pub fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Load the note's text. See [`read_note_at`] for the decode rule.
#[tauri::command]
pub async fn read_note(state: State<'_, AppState>) -> AppResult<String> {
    read_note_at(&state.launch.cwd)
}

/// Persist the note's text, replacing the file's contents atomically. The
/// `.cimp` dir is created on demand. Called by the frontend autosave.
///
/// **Left as a direct call** (V42 Phase A): the body is [`note_path`] and one
/// [`crate::settings::write_atomic`], both of which the tests below already
/// drive without a WebView.
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

    /// **A stray non-UTF-8 byte must not lock the user out of their own note.**
    ///
    /// The note is a plain `.txt` the user is invited to edit with anything, so
    /// a latin-1 save or a tool that appended raw bytes is a normal event, not
    /// a corruption. `read_to_string` would fail on it and the tab would report
    /// a load error for a file every editor opens fine. This asserts the
    /// degrade: the bad byte becomes U+FFFD and the text on BOTH sides of it
    /// survives — a reader that stopped at the first bad byte would pass an
    /// assertion about the prefix alone.
    #[test]
    fn a_stray_non_utf8_byte_degrades_and_never_locks_the_note_out() {
        let dir = std::env::temp_dir().join(format!("cimp_note_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut planted = b"before ".to_vec();
        planted.push(0xff);
        planted.extend_from_slice(b" after");
        assert!(
            String::from_utf8(planted.clone()).is_err(),
            "the fixture must not be valid UTF-8, or this proves nothing"
        );
        crate::settings::write_atomic(&note_path(&dir), &planted).unwrap();

        let text = read_note_at(&dir).expect("a hand-edited note still loads");
        assert_eq!(
            text,
            format!("before {} after", char::REPLACEMENT_CHARACTER)
        );
        // …and the decode is the same one whether or not a file is involved.
        assert_eq!(decode(&planted), text);

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
