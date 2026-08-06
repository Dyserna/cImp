//! V32 Phase C3 — the updater's **on-disk layout and state**.
//!
//! Everything the updater persists lives under one root,
//! `<exe-dir>/detection-updates/`:
//!
//! ```text
//! detection-updates/
//!   state.json                     installed versions, last check + outcome
//!   staging/rules/                 downloads, wiped before and after every run
//!   staging/classifier/
//!   previous/rules/<version>/      the bundle the current one replaced
//!   previous/classifier/<version>/
//! ```
//!
//! # Why a sibling of `detection/`, not a subdirectory
//!
//! `detection/` is **shipped data**: `src-tauri/build.rs` mirrors the repo-root
//! folder next to the built binary and *prunes destination entries the source
//! no longer has*, and `release.yml` copies it wholesale into both zips. A
//! `previous/` or `state.json` inside it would therefore be deleted by the next
//! dev build and would be a candidate for accidentally shipping. Runtime state
//! belongs outside the mirrored tree.
//!
//! # `rules.d/local/` is untouched by construction
//!
//! The activation path enumerates **only** the top level of `rules.d`
//! ([`managed_rule_files`]) — it never opens, lists, moves or deletes anything
//! inside `local/`. That is the whole reason the two directories are separate
//! (decision 13), and it is a structural property here rather than a filter: a
//! future edit would have to *add* a recursive walk to break it, not forget a
//! condition.
//!
//! # Moves, and what "atomic-as-possible" means
//!
//! A directory swap on Windows has no all-or-nothing primitive across two
//! multi-file directories. [`move_file`] is a rename with a copy+remove
//! fallback (renames fail across volumes; a portable root's `models/` may sit
//! on a different one from the exe). The activation sequence archives the old
//! files first and restores them if the new set fails to land, so the failure
//! window is "old files back in place", never "no rules at all".

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::manifest::Component;

/// The updater's state directory name, beside the shipped `detection/` folder.
pub const STATE_DIR: &str = "detection-updates";
/// The state file inside it.
pub const STATE_FILE: &str = "state.json";
/// Schema of [`State`]. Bumped only for an incompatible change; an unknown
/// version is treated as "no state" (see [`load_state`]).
pub const STATE_SCHEMA: u32 = 1;

/// `<exe-dir>/detection-updates`. `None` only when `current_exe` has no usable
/// parent, in which case the whole updater stays inert rather than guessing at
/// a path — the same discipline `signature::rules_dir` follows.
pub fn state_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(STATE_DIR))
}

/// Staging directory for one component's downloads.
pub fn staging_dir(root: &Path, c: Component) -> PathBuf {
    root.join("staging").join(c.as_str())
}

/// Where the version being replaced is archived.
pub fn previous_dir(root: &Path, c: Component, version: &str) -> PathBuf {
    root.join("previous")
        .join(c.as_str())
        .join(sanitize_version(version))
}

/// A version string reduced to something safe as a single path segment. Version
/// strings come from the manifest, so they are remote input reaching a path
/// join; anything outside `[A-Za-z0-9._-]` becomes `_`, and an empty result
/// becomes `unknown` (which is also what an un-versioned pre-updater bundle
/// archives as).
pub fn sanitize_version(v: &str) -> String {
    let cleaned: String = v
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['.', '_', '-']).to_string();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    }
}

// ── The live destination for each component ────────────────────────────────

/// Where a component's files actually live when active.
///
/// The two destinations differ because the two artifacts ship differently
/// (rules beside the binary like `themes/`, weights in the portable root's
/// shared `models/`), and pretending otherwise would mean one of them moving
/// house for the updater's convenience.
pub fn destination(c: Component) -> Option<PathBuf> {
    match c {
        Component::Rules => super::super::signature::rules_dir(),
        Component::Classifier => super::super::classifier::model_dir(),
    }
}

/// The updater-managed files currently at `dest` for `c` — top level only.
///
/// For rules this is every `*.yar`/`*.yara` in `rules.d` itself; `local/` is a
/// subdirectory and is never enumerated. For the classifier it is the two known
/// artifact names, so a user's own scratch file in the model directory is not
/// swept into the archive either.
pub fn managed_files(dest: &Path, c: Component) -> Vec<PathBuf> {
    match c {
        Component::Rules => managed_rule_files(dest),
        Component::Classifier => [
            super::super::classifier::MODEL_FILE,
            super::super::classifier::TOKENIZER_FILE,
        ]
        .iter()
        .map(|n| dest.join(n))
        .filter(|p| p.is_file())
        .collect(),
    }
}

/// Every `*.yar`/`*.yara` directly in `dir`, sorted. **Non-recursive** — this
/// is the function that makes "the updater never touches `local/`" structural.
pub fn managed_rule_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "yar" | "yara"))
        })
        .collect();
    out.sort();
    out
}

// ── File primitives ────────────────────────────────────────────────────────

/// Move `from` to `to`, creating `to`'s parent. Rename first; on failure (a
/// cross-volume move, or a destination the OS will not overwrite) fall back to
/// copy-then-remove. A failed *remove* after a successful copy is a warning,
/// not an error: the content is where it needs to be, and the leftover is
/// swept by the next staging wipe.
pub fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // Windows `rename` fails if the destination exists; remove it first so a
    // re-run over a half-finished previous attempt still lands.
    if to.exists() {
        let _ = std::fs::remove_file(to);
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
    if let Err(e) = std::fs::remove_file(from) {
        warn!(
            target: "offload",
            path = %from.display(),
            error = %e,
            "detection updater: copied a file but could not remove the source"
        );
    }
    Ok(())
}

/// Delete `dir` and everything under it, ignoring "it was not there".
pub fn wipe_dir(dir: &Path) {
    if !dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(dir) {
        warn!(
            target: "offload",
            path = %dir.display(),
            error = %e,
            "detection updater: could not clear a directory"
        );
    }
}

// ── Persisted state ────────────────────────────────────────────────────────

/// One component's recorded history. Everything Settings shows about update
/// status comes from here, so a field that no surface reads does not belong.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ComponentState {
    /// Version currently active on disk. Empty before the first successful
    /// update — the shipped bundle has no manifest version, and inventing one
    /// would make the first real update look like a downgrade.
    #[serde(default)]
    pub installed_version: String,
    /// Version archived under `previous/`, if any. Non-empty is exactly the
    /// condition that enables the Revert button.
    #[serde(default)]
    pub previous_version: String,
    /// Epoch-ms of the last completed check (successful or not). Drives the
    /// scheduler's due-ness decision AND the "last checked" readout.
    #[serde(default)]
    pub last_check_ms: u64,
    /// One-line result of that check, shown verbatim in Settings.
    #[serde(default)]
    pub last_outcome: String,
    /// Whether that outcome was healthy. Separated from the text so the UI can
    /// colour it without parsing prose.
    #[serde(default)]
    pub last_ok: bool,
    /// A newer version the last check found but did not apply — set in
    /// `check-only` mode, and cleared on a successful apply. This is the field
    /// the "update available" Advisor card and the Apply button read.
    #[serde(default)]
    pub available_version: String,
    /// The curator's note for `available_version`, when the manifest carried
    /// one. Remote text: displayed, never interpreted.
    #[serde(default)]
    pub available_notes: String,
    /// Why the last attempt was rejected, when it was. Kept separately from
    /// `last_outcome` so a later successful check does not erase the record the
    /// failure card is keyed to until the failure is actually resolved.
    #[serde(default)]
    pub last_failure: String,
    /// Version the failure above was attempting. The failure card's signature,
    /// so a dismissal holds for that bundle and re-fires on the next one.
    #[serde(default)]
    pub last_failure_version: String,
}

/// The whole state file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    #[serde(default)]
    pub rules: ComponentState,
    #[serde(default)]
    pub classifier: ComponentState,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            rules: ComponentState::default(),
            classifier: ComponentState::default(),
        }
    }
}

impl State {
    pub fn get(&self, c: Component) -> &ComponentState {
        match c {
            Component::Rules => &self.rules,
            Component::Classifier => &self.classifier,
        }
    }

    pub fn get_mut(&mut self, c: Component) -> &mut ComponentState {
        match c {
            Component::Rules => &mut self.rules,
            Component::Classifier => &mut self.classifier,
        }
    }
}

/// Read the state file, or a default one.
///
/// Every failure mode — absent, unreadable, malformed, unknown schema — yields
/// the default rather than an error. The state file is a *record of past
/// updates*, not a security control: a corrupted one must cost at most one
/// redundant re-check, never a refusal to run. The one thing that would be
/// dangerous is silently trusting an unknown schema's fields, so that is the
/// case that resets.
pub fn load_state(root: &Path) -> State {
    let path = root.join(STATE_FILE);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return State::default();
    };
    match serde_json::from_str::<State>(&raw) {
        Ok(s) if s.schema == STATE_SCHEMA => s,
        Ok(s) => {
            warn!(
                target: "offload",
                found = s.schema,
                expected = STATE_SCHEMA,
                "detection updater: state file has an unknown schema; starting fresh"
            );
            State::default()
        }
        Err(e) => {
            warn!(
                target: "offload",
                path = %path.display(),
                error = %e,
                "detection updater: state file is unreadable; starting fresh"
            );
            State::default()
        }
    }
}

/// Persist `state`. Written whole, via a temp file and a rename, so a crash
/// mid-write cannot leave a half-serialized file that the next launch reads as
/// "nothing installed" and re-downloads over.
pub fn save_state(root: &Path, state: &State) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("create {}: {e}", root.display()))?;
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| format!("serialize updater state: {e}"))?;
    let tmp = root.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    let final_path = root.join(STATE_FILE);
    if final_path.exists() {
        let _ = std::fs::remove_file(&final_path);
    }
    std::fs::rename(&tmp, &final_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename updater state into place: {e}")
    })?;
    info!(
        target: "offload",
        path = %final_path.display(),
        "detection updater: state recorded"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cimp-updater-store-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&p).expect("temp dir");
        p
    }

    /// The load-bearing property of the whole activation path: enumerating the
    /// managed set never descends into `local/`, so a hand-written rule cannot
    /// be archived or deleted by an update.
    #[test]
    fn managed_rule_files_never_descends_into_local() {
        let dir = tmp();
        std::fs::create_dir_all(dir.join("local")).unwrap();
        std::fs::write(dir.join("shipped.yar"), "x").unwrap();
        std::fs::write(dir.join("other.yara"), "x").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("local").join("mine.yar"), "x").unwrap();

        let found = managed_rule_files(&dir);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["other.yara", "shipped.yar"], "sorted, top level");
        assert!(
            !found.iter().any(|p| p.to_string_lossy().contains("local")),
            "local/ must never appear: {found:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_version_string_cannot_escape_its_path_segment() {
        assert_eq!(sanitize_version("2026.08.07"), "2026.08.07");
        assert_eq!(sanitize_version("1.0-beta_2"), "1.0-beta_2");
        // Separators become `_`, and the leading run of punctuation is trimmed
        // — what matters is that nothing resembling a traversal survives.
        assert_eq!(sanitize_version("../../etc"), "etc");
        assert_eq!(sanitize_version("(shipped)"), "shipped");
        assert_eq!(sanitize_version("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_version(""), "unknown");
        assert_eq!(sanitize_version("..."), "unknown");
    }

    #[test]
    fn move_file_creates_the_destination_tree_and_overwrites() {
        let dir = tmp();
        let from = dir.join("a.txt");
        std::fs::write(&from, "one").unwrap();
        let to = dir.join("deep").join("nested").join("a.txt");
        move_file(&from, &to).expect("move");
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "one");
        assert!(!from.exists());

        std::fs::write(&from, "two").unwrap();
        move_file(&from, &to).expect("overwrite");
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "two");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// State round-trips, and every unhealthy file shape degrades to a default
    /// rather than to an error — a corrupted record costs one re-check.
    #[test]
    fn state_round_trips_and_degrades_to_default_on_anything_unreadable() {
        let dir = tmp();
        let mut s = State::default();
        s.get_mut(Component::Rules).installed_version = "2026.08.07".into();
        s.get_mut(Component::Rules).last_check_ms = 42;
        s.get_mut(Component::Classifier).available_version = "22m-2".into();
        save_state(&dir, &s).expect("save");
        let back = load_state(&dir);
        assert_eq!(back.get(Component::Rules).installed_version, "2026.08.07");
        assert_eq!(back.get(Component::Rules).last_check_ms, 42);
        assert_eq!(back.get(Component::Classifier).available_version, "22m-2");

        std::fs::write(dir.join(STATE_FILE), "{ not json").unwrap();
        assert_eq!(load_state(&dir), State::default());
        std::fs::write(dir.join(STATE_FILE), r#"{"schema":99}"#).unwrap();
        assert_eq!(load_state(&dir), State::default());
        std::fs::remove_dir_all(&dir).ok();
        // An absent directory is the cold-start case.
        assert_eq!(load_state(&dir), State::default());
    }
}
