//! Loader for `<exe-dir>/patterns.json` — the user-editable list of
//! prompt-detection substrings consumed by [`PermissionDetector`].
//!
//! On first launch the file doesn't exist; we write the bundled defaults
//! (see [`super::permission::default_patterns`]) so the user can hand-edit
//! to add or refine patterns without rebuilding. A missing or corrupt file
//! falls back to defaults in-memory so detection always works.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::processing::permission::{default_patterns, PermissionPattern};

const PATTERNS_FILE_NAME: &str = "patterns.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PatternsFile {
    /// Free-form notes the user can leave in the JSON. Not consumed by the
    /// detector — keeps a place for "how to edit this file" docs that
    /// survive round-trips through serde.
    #[serde(rename = "_doc", skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub patterns: Vec<PermissionPattern>,
}

/// Default file body, including the `_doc` header. Written verbatim on
/// first launch and used as the fallback when load fails.
fn default_file() -> PatternsFile {
    PatternsFile {
        doc: Some(
            "Edit this file to add or refine prompt-detection substrings. \
             Each pattern's `all_of` is a list of substrings that must ALL be \
             present in the rendered terminal tail for the pattern to match. \
             `kind` is `permission` or `question`. Set `disabled: true` to \
             keep an entry in the file without enabling it. Capture live \
             chrome with RUST_LOG=perm_capture=debug. Patterns are tested in \
             declaration order; first match per kind wins."
                .to_string(),
        ),
        patterns: default_patterns(),
    }
}

/// Resolve the file path next to the running executable (same directory
/// as `settings.json`). Returns `None` if `current_exe()` doesn't yield a
/// usable parent, in which case we just use defaults in memory and don't
/// try to write anything.
pub fn patterns_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join(PATTERNS_FILE_NAME))
}

/// Load patterns from disk, writing default content if the file is absent.
/// Always returns a usable `Vec<PermissionPattern>` — a corrupt file is
/// logged and defaults are used. The `disabled` flag is honored as
/// declared.
pub fn load_or_seed() -> Vec<PermissionPattern> {
    let Some(path) = patterns_path() else {
        tracing::warn!(
            "patterns: cannot resolve exe dir; using built-in defaults in memory"
        );
        return default_patterns();
    };

    if !path.exists() {
        let body = default_file();
        if let Err(e) = write_file(&path, &body) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "patterns: write defaults failed; using in-memory defaults"
            );
        } else {
            tracing::info!(path = %path.display(), "patterns: wrote default file");
        }
        return body.patterns;
    }

    match read_file(&path) {
        Ok(file) => {
            tracing::info!(
                path = %path.display(),
                count = file.patterns.len(),
                "patterns: loaded"
            );
            file.patterns
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "patterns: parse failed; using built-in defaults"
            );
            default_patterns()
        }
    }
}

fn read_file(path: &Path) -> Result<PatternsFile, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))
}

fn write_file(path: &Path, body: &PatternsFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let text =
        serde_json::to_string_pretty(body).map_err(|e| format!("serialize: {e}"))?;
    fs::write(path, text).map_err(|e| format!("write: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processing::permission::PatternKind;

    #[test]
    fn default_file_round_trips_through_json() {
        let body = default_file();
        let text = serde_json::to_string_pretty(&body).unwrap();
        let parsed: PatternsFile = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.patterns.len(), body.patterns.len());
        assert_eq!(parsed.doc, body.doc);
        // First pattern is the live permission match.
        assert_eq!(parsed.patterns[0].name, "claude_permission");
        assert_eq!(parsed.patterns[0].kind, PatternKind::Permission);
        assert!(!parsed.patterns[0].disabled);
        // Second pattern: a second permission entry shipped disabled,
        // there as a multi-pattern declaration example.
        assert_eq!(parsed.patterns[1].kind, PatternKind::Permission);
        assert!(parsed.patterns[1].disabled);
        // Third is the disabled question template.
        assert_eq!(parsed.patterns[2].kind, PatternKind::Question);
        assert!(parsed.patterns[2].disabled);
    }

    #[test]
    fn missing_file_seeds_with_defaults() {
        let dir = std::env::temp_dir().join(format!(
            "cctts_patterns_seed_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PATTERNS_FILE_NAME);
        assert!(!path.exists());
        // Force the seed path explicitly (unit test) — load_or_seed uses
        // current_exe() so we drive write_file directly.
        let body = default_file();
        write_file(&path, &body).unwrap();
        assert!(path.exists());
        let parsed = read_file(&path).unwrap();
        assert_eq!(parsed.patterns.len(), body.patterns.len());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_returns_err() {
        let dir = std::env::temp_dir().join(format!(
            "cctts_patterns_corrupt_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PATTERNS_FILE_NAME);
        fs::write(&path, b"{ not valid json").unwrap();
        assert!(read_file(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_default_file_matches_code_defaults() {
        // `scripts/patterns.default.json` is copied verbatim into the full
        // release zip as `bin/patterns.json` so the file is discoverable on
        // a fresh install (the loader otherwise seeds it on first run). This
        // test fails if that committed copy drifts from `default_file()` —
        // regenerate it from the body below when the defaults change.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("patterns.default.json");
        let text = fs::read_to_string(&path)
            .expect("read scripts/patterns.default.json");
        let shipped: PatternsFile =
            serde_json::from_str(&text).expect("parse scripts/patterns.default.json");
        // Compare semantically (re-serialize both) so on-disk whitespace
        // isn't load-bearing — only the content has to match.
        let shipped_norm = serde_json::to_string_pretty(&shipped).unwrap();
        let code_norm = serde_json::to_string_pretty(&default_file()).unwrap();
        assert_eq!(
            shipped_norm, code_norm,
            "scripts/patterns.default.json is out of sync with default_file(); \
             regenerate it from the current defaults"
        );
    }

    #[test]
    fn empty_object_yields_empty_pattern_list() {
        // A user who deletes everything ends up with no patterns —
        // detection effectively off. That's a valid choice; we don't
        // re-seed in that case.
        let parsed: PatternsFile = serde_json::from_str("{}").unwrap();
        assert!(parsed.patterns.is_empty());
        assert!(parsed.doc.is_none());
    }
}
