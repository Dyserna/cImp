//! V32 Phase C3 — the updater's **on-disk layout and state**.
//!
//! Everything the updater persists lives under one root,
//! `<exe-dir>/detection-updates/`:
//!
//! ```text
//! detection-updates/
//!   state.json                     installed versions, last check + outcome
//!   staging/rules/                 downloads, wiped before and after every run
//!   previous/rules/<version>/      the bundle the current one replaced
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
///
/// **Still 1 after #46 and #48**, on purpose. Every field either side of those
/// issues added is additive `#[serde(default)]`, and a bump is not free: it
/// takes [`load_state`]'s unknown-schema path, which throws away
/// `installed_version` and `previous_version` — the install history Revert and
/// the "is this newer?" comparison are built on. The one thing a pre-#46 file
/// carries that MUST NOT be trusted verbatim is its failure record, and that is
/// repaired in place by [`heal_pre_split_failure`] rather than paid for with
/// everything else in the file.
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

/// Where a component's files actually live when active — rules beside the
/// binary, like `themes/` and `palettes/`.
pub fn destination(c: Component) -> Option<PathBuf> {
    match c {
        Component::Rules => super::super::signature::rules_dir(),
    }
}

/// The updater-managed files currently at `dest` for `c` — top level only.
///
/// Every `*.yar`/`*.yara` in `rules.d` itself; `local/` is a subdirectory and is
/// never enumerated.
pub fn managed_files(dest: &Path, c: Component) -> Vec<PathBuf> {
    match c {
        Component::Rules => managed_rule_files(dest),
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
    #[cfg(test)]
    if let Some(e) = fault::injected_failure(to) {
        return Err(e);
    }
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

/// Test-only fault injection for [`move_file`] (#48, U-2).
///
/// The failure this guards against is the most ordinary one Windows has — AV
/// real-time scanning, or the user having a rule file open through the panel's
/// own *Open rules folder* button, holding a handle so both `rename` and `copy`
/// fail with a sharing violation. Reproducing that for real in a test means
/// racing the OS; what actually needs testing is what the ACTIVATION does when
/// a move fails partway through a multi-file directory, so the move is the seam
/// and the fault is injected there.
///
/// Lives in the production module beside [`super::manifest::MapFetcher`], and
/// for the same reason: the point is to drive the *real* pipeline, which a
/// fault defined in a test module could not reach.
///
/// Keyed on an exact destination **file name** rather than a global switch,
/// because `cargo test` runs these on threads of one process and a global
/// switch would fail whatever move another test happened to be making. A test
/// arms a name only it uses.
///
/// [`fail_moves_to_path`] arms a full destination PATH instead, for the one
/// case a name cannot express (#48, M-10/M-11): a swap moves the same file name
/// out to the archive and back again, so arming the name fails the archive loop
/// before the interesting failure can happen. The rollback's restore and the
/// staged move share one destination path and the archive move does not, which
/// is exactly the discrimination needed to drive a *partial* rollback.
#[cfg(test)]
pub mod fault {
    use std::path::Path;
    use std::sync::{Mutex, OnceLock, PoisonError};

    fn armed() -> &'static Mutex<Vec<String>> {
        static A: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
        A.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Make every [`super::move_file`] whose destination is named `file_name`
    /// fail, until the returned guard drops.
    pub fn fail_moves_to(file_name: &str) -> Guard {
        armed()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(file_name.to_string());
        Guard(file_name.to_string())
    }

    /// Make every [`super::move_file`] whose destination is exactly `path`
    /// fail, until the returned guard drops.
    pub fn fail_moves_to_path(path: &Path) -> Guard {
        let key = path.display().to_string();
        armed()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key.clone());
        Guard(key)
    }

    /// Disarms on drop, so a panicking test cannot poison the ones after it.
    pub struct Guard(String);

    impl Drop for Guard {
        fn drop(&mut self) {
            armed()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|n| n != &self.0);
        }
    }

    pub(super) fn injected_failure(to: &Path) -> Option<String> {
        let name = to.file_name()?.to_string_lossy().to_string();
        let full = to.display().to_string();
        let hit = armed()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|n| n == &name || n == &full);
        hit.then(|| {
            format!(
                "copy -> {}: injected fault (the shape of a sharing violation)",
                to.display()
            )
        })
    }
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
    /// [`Outcome::as_str`](super::Outcome::as_str) for that check — the machine
    /// half of `last_outcome`. `last_ok` alone cannot tell "the channel could
    /// not be reached" from "the bundle was fine", and Settings has to say
    /// something different for each (#46: a transport failure is not a bundle
    /// rejection). Empty on a state file written before this field existed.
    #[serde(default)]
    pub last_outcome_kind: String,
    /// Consecutive checks that ended `unavailable` — the update channel could
    /// not be reached at all. Reset to 0 by any check that REACHED it, refusal
    /// included; a revert reaches nothing and leaves it alone (#48). Read by
    /// Settings, which says how long the silence has lasted rather than
    /// repeating one 404 forever.
    #[serde(default)]
    pub unreachable_streak: u32,
    /// Consecutive checks that did not leave this component fresher — i.e.
    /// everything except [`Outcome::Applied`](super::Outcome::Applied) and
    /// [`Outcome::UpToDate`](super::Outcome::UpToDate), the only two outcomes
    /// that prove the installed data IS the currently published data.
    ///
    /// The freshness canary decision 13 actually asks for, and deliberately
    /// **outcome-agnostic**: `unreachable_streak` cannot see a channel that is
    /// perfectly reachable and refuses every bundle it serves, which is a
    /// component frozen just as hard and — once its failure card is dismissed —
    /// with no signal at all (#48). Reverts touch it in neither direction: they
    /// are not checks and say nothing about what is published.
    /// Consumed by `updater::signals_from` at `updater::STALLED_AFTER_CHECKS`.
    #[serde(default)]
    pub stale_streak: u32,
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
    /// Version the failure above was attempting. Empty when the refusal
    /// happened at the manifest level, before any bundle had a version — which
    /// is exactly why it is NOT the card's signature (#46).
    #[serde(default)]
    pub last_failure_version: String,
    /// The failure card's dismissal signature. The version when there is one,
    /// else a digest of the reason — never empty, because an empty signature
    /// made one dismissal silence every future refusal including a containment
    /// violation. See [`super::failure_signature`].
    #[serde(default)]
    pub last_failure_signature: String,
    /// **Files a rollback could not put back** — the live directory is short of
    /// them and the archive under `previous/` still holds them (#48, M-11).
    ///
    /// [`super::restore_archived`] used to swallow a per-file failure with a
    /// `warn!` while its caller reported "the previous version was restored"
    /// verbatim, and the post-rollback health check could not contradict it:
    /// `signature::Status::healthy` compiles what IS on disk and a file that is
    /// simply absent compiles to nothing at all. So the one failure mode that
    /// permanently reduces coverage was the one with the most reassuring
    /// message.
    ///
    /// This is the **reporting mirror**; the durable truth is on disk (the
    /// files still in `previous/<version>/`, plus a `Phase::Restoring` journal
    /// that survives the run). Both are written by the same helper
    /// (`super::settle_restore`), and the disk copy is what drives the retry —
    /// a state file lost to corruption costs a card, never a file.
    ///
    /// Consumed by `detection.rules_incomplete.v1` in `advisor.rs` and by the
    /// Settings → Injection protection → Injection detection readout, and
    /// cleared the moment a retry (or a later full activation, which rewrites
    /// the whole set anyway) makes it untrue.
    #[serde(default)]
    pub unrestored_files: Vec<String>,
}

/// The whole state file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    #[serde(default)]
    pub rules: ComponentState,
    // A `classifier` key may still be present in an installed `state.json`
    // (the component was removed 2026-08-08). `deny_unknown_fields` is not set,
    // so serde drops it on read and it disappears on the next write — no
    // migration, no schema bump.
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            rules: ComponentState::default(),
        }
    }
}

impl State {
    pub fn get(&self, c: Component) -> &ComponentState {
        match c {
            Component::Rules => &self.rules,
        }
    }

    pub fn get_mut(&mut self, c: Component) -> &mut ComponentState {
        match c {
            Component::Rules => &mut self.rules,
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
        Ok(mut s) if s.schema == STATE_SCHEMA => {
            for c in Component::ALL {
                heal_pre_split_failure(s.get_mut(c), c);
            }
            s
        }
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

/// Drop a failure record written by a build that predates the #46 outcome
/// split, in which a transport failure was recorded as a bundle refusal.
///
/// # Why this exists at all
///
/// #46 was **forward-only**, and on an upgrading install that made its symptom
/// worse rather than better (#48). A pre-#46 state file carries a `last_failure`
/// recorded from a 404 against a release that does not exist yet;
/// `signals_from` raises the refusal card from that field alone, and `finish`
/// clears it only on `Applied`/`UpToDate`/`Reverted` — none of which can happen
/// while the channel is unreachable, which it is by construction until
/// `detection-v1` is published. So the two "update REJECTED" cards #46 removed
/// for new installs would have fired **forever** on every existing one — and
/// re-fired even if previously dismissed, because #46 moved the dismissal key
/// off `component:version`.
///
/// # The predicate, and why it is exact
///
/// A failure with **no version and no signature** can only have been written by
/// a pre-#46 build: since #46, `finish` derives the signature inside itself and
/// [`super::failure_signature`] never returns an empty string, so every refusal
/// this build records carries one. And a pre-#46 *versionless* failure was
/// always either `fail_all`'s manifest-level failure (a 404, an unparseable
/// body, an unknown schema) or `revert`'s "nothing to revert to" — every one of
/// them an event that #46/#48 reclassify as **not a refusal**.
///
/// A pre-#46 failure that HAS a version is left exactly as it is: that was a
/// real bundle-level refusal (checksum, gauntlet, reload), its dismissal key is
/// still `component:version`, and both the card and the dismissal keep working.
///
/// Nothing is invented and nothing is hidden: if the condition is still true,
/// the next check re-records it within one interval with an honest outcome —
/// which is the same self-healing property "a card outliving its condition is
/// worse than no card" (decision 13) asks for in the other direction.
///
/// `last_outcome` / `last_ok` are deliberately NOT rewritten. They are the
/// verbatim record of what that build reported for that check, they are display
/// only, and the first check after launch overwrites them.
fn heal_pre_split_failure(cs: &mut ComponentState, c: Component) {
    if cs.last_failure.is_empty()
        || !cs.last_failure_version.is_empty()
        || !cs.last_failure_signature.is_empty()
    {
        return;
    }
    info!(
        target: "offload",
        component = c.as_str(),
        failure = %cs.last_failure,
        "detection updater: dropping a pre-#46 failure record (a transport failure recorded as a \
         bundle refusal); the next check records what is actually true"
    );
    cs.last_failure.clear();
    cs.last_failure_version.clear();
    cs.last_failure_signature.clear();
}

// ── The activation journal ─────────────────────────────────────────────────

/// The journal file, beside `state.json` under the same root.
pub const JOURNAL_FILE: &str = "activation.json";

/// Which part of a swap was in flight, and therefore how to undo it
/// (#48, U-2; third state added for #48 M-10).
///
/// The phases need **different** recoveries, and that is the whole reason this
/// is recorded rather than inferred from what is on disk:
///
/// - [`Phase::Archiving`]: the destination still holds every file that has not
///   been archived yet. Restoring the archive on top of it is right; *wiping*
///   the destination first would destroy the only copy of the files the loop
///   had not reached.
/// - [`Phase::Moving`]: the destination holds however many staged files landed
///   before the interruption, and the archive holds the complete outgoing set.
///   Here the destination MUST be cleared first, or recovery leaves a mixture
///   of old and new that no curation step ever validated as a set.
/// - [`Phase::Restoring`]: **a rollback is itself in flight** — the destination
///   has already been cleared of staged files and the archive is being moved
///   back into it, file by file. This is the state M-10 found unrepresented:
///   a kill here left the journal still reading `Moving`, so the next run's
///   recovery deleted every file the rollback had ALREADY restored (they are
///   `managed_files(dest)` like any other) and then restored only the remainder
///   of the archive. Permanent loss of the difference, reported as "the
///   previous version was restored". Recovery here is restore-only, never
///   destructive, and — because `restore_archived` is idempotent — it is also
///   what a *completed* rollback recovers to, safely, as a no-op.
///
/// A `Restoring` journal that OUTLIVES its run is the durable record of a
/// rollback that could not put every file back (M-11): it is deliberately left
/// on disk so the next run — and, since M-12, the next launch — retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Archiving,
    Moving,
    Restoring,
}

/// A swap that was in flight when the process stopped.
///
/// # Why a journal exists at all
///
/// Activation moves files out of the live directory and other files in. A kill
/// between the two loops — a crash, a reboot, task manager — leaves the live
/// directory short with nothing in memory that knows it. Worse, the *next*
/// activation recomputes the archive path from the unchanged `installed_version`
/// and [`wipe_dir`]s it, which destroys the only surviving copy of the old
/// bundle: an interruption that cost coverage until the next check turns into
/// permanent data loss.
///
/// So the swap writes down what it is about to do, before it does it, and the
/// next run reads that note and finishes the undo. Deliberately a plain file
/// with three fields and no schema version: it lives for the duration of one
/// swap, an unreadable one is treated as absent, and anything more elaborate
/// would be a second state machine to keep in sync with the first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    /// The component's wire name, so an unknown one (a state file from a newer
    /// build) is ignored rather than guessed at.
    pub component: String,
    pub phase: Phase,
    /// Where the outgoing files were being archived.
    pub archive: PathBuf,
    /// The live directory being swapped.
    pub dest: PathBuf,
}

fn journal_path(root: &Path) -> PathBuf {
    root.join(JOURNAL_FILE)
}

/// Record an in-flight swap. A failure to write is logged, not fatal: losing
/// the ability to recover from a crash that has not happened must not abort an
/// update that is otherwise fine.
pub fn write_journal(root: &Path, j: &Journal) {
    if let Err(e) = std::fs::create_dir_all(root)
        .map_err(|e| e.to_string())
        .and_then(|()| serde_json::to_string_pretty(j).map_err(|e| e.to_string()))
        .and_then(|s| std::fs::write(journal_path(root), s).map_err(|e| e.to_string()))
    {
        warn!(
            target: "offload",
            root = %root.display(),
            error = %e,
            "detection updater: could not write the activation journal; a crash mid-swap would \
             not be recoverable"
        );
    }
}

/// Clear the journal — the swap finished, one way or the other.
pub fn clear_journal(root: &Path) {
    let path = journal_path(root);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            warn!(
                target: "offload",
                path = %path.display(),
                error = %e,
                "detection updater: could not clear the activation journal"
            );
        }
    }
}

/// Whether a journal file exists at all — no parse, and **no side effects**.
///
/// [`read_journal`] deletes what it cannot parse, which is right under the run
/// lock and wrong outside it: [`write_journal`] is a plain `fs::write`, so a
/// reader without the lock can catch a peer's journal half-written and would
/// delete the record of a swap that is in flight. [`super::recover_on_launch`]
/// needs only "is there anything to do?" before deciding whether to take the
/// lock, so it asks this instead.
pub fn has_journal(root: &Path) -> bool {
    journal_path(root).exists()
}

/// The in-flight swap recorded under `root`, if any. An unreadable or
/// unparseable journal reads as absent and is removed: it cannot be acted on,
/// and leaving it would make every future run try again. **Call under the run
/// lock** — see [`has_journal`] for the unlocked question.
pub fn read_journal(root: &Path) -> Option<Journal> {
    let path = journal_path(root);
    let raw = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<Journal>(&raw) {
        Ok(j) => Some(j),
        Err(e) => {
            warn!(
                target: "offload",
                path = %path.display(),
                error = %e,
                "detection updater: the activation journal is unreadable; discarding it"
            );
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

// ── The cross-process run lock (#48, M-14) ─────────────────────────────────

/// The lock file, beside `state.json` under the same root.
pub const LOCK_FILE: &str = "update.lock";

/// How long a lock file may go unrefreshed before it is treated as abandoned.
///
/// The staleness story a lockfile owes: a hard kill (task manager, power loss,
/// the OS reclaiming a hung process) leaves the file behind, and a lock nobody
/// holds must never wedge the updater permanently — that would be the
/// never-degrade-to-no-rules invariant lost to a crash, which is the very thing
/// the journal exists to prevent.
///
/// Thirty minutes, chosen to be comfortably longer than any legitimate run and
/// comfortably shorter than the 24 h check interval. A run's ceiling is a
/// handful of artifacts at [`HttpFetcher`](super::manifest::HttpFetcher)'s
/// 300 s per-request timeout plus a YARA compile bounded by
/// `validate::COMPILE_BUDGET`; 30 minutes covers a pathological one several
/// times over, and a user who hits Check now half an hour after a crash gets
/// their click rather than a permanent refusal.
pub const LOCK_MAX_AGE_MS: u64 = 30 * 60 * 1000;

/// What a held lock records — `pid` for the same reason the loopback discovery
/// files carry one: so a human looking at a stuck updater can see who has it.
///
/// Deliberately **not** used to decide staleness. "A lock naming our own pid is
/// a leftover" is true of the discovery files and false here: this lock is the
/// only thing standing between an in-flight `run` and a concurrent
/// [`super::recover_now`] in the SAME process, so self-exclusion is the
/// property, not the exception.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockRecord {
    pid: u32,
    started_ms: u64,
}

/// A held cross-process lock. Released on drop, including on unwind.
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    target: "offload",
                    path = %self.path.display(),
                    error = %e,
                    "detection updater: could not release the run lock"
                );
            }
        }
    }
}

fn lock_path(root: &Path) -> PathBuf {
    root.join(LOCK_FILE)
}

/// How long ago `path` was last written, in ms. `None` when the filesystem
/// cannot say (no mtime, or a clock that puts it in the future).
fn file_age_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = std::time::SystemTime::now().duration_since(modified).ok()?;
    Some(age.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// Take the run lock for `root`, or report why not.
///
/// # Why a file and not a mutex (#48, M-14)
///
/// `super::run_lock` is a `tokio::sync::Mutex` in *this* process's address
/// space. Two cImp instances started from one exe directory — a portable build
/// on a shared drive, a second launch while the first is minimised — share
/// `<exe-dir>/detection-updates` and `<exe-dir>/detection/rules.d` and are
/// invisible to each other's mutex. Both would archive to the same
/// `previous/<version>/` path, and [`wipe_dir`] on the second one destroys the
/// only copy of the old bundle *while the first one's journal still points at
/// it* — recovery then finds an empty archive and clears the journal, which is
/// the permanent loss of the live rule set that decision 13 forbids.
///
/// # The mechanism
///
/// `create_new` — the O_EXCL create the attach allocator
/// ([`crate::attach`]) already uses for exactly this "no other process may
/// observe the same slot" property. Atomic on both Windows and POSIX, needs no
/// new dependency, and leaves an inspectable file behind rather than an
/// invisible OS handle.
///
/// A lock is broken (and re-taken) only on **age**: older than
/// [`LOCK_MAX_AGE_MS`], or a start time in the FUTURE — the same
/// clock-went-backwards case [`super::is_due`] defends against, and for the
/// same reason: a state directory copied from another machine must not park the
/// updater forever.
///
/// A lock whose body will not parse is aged by its **mtime**, not treated as
/// stale outright. The create and the write cannot be one operation under
/// `create_new` (a write-then-rename would overwrite, which is the opposite of
/// what is wanted), so there is a window in which a live peer's lock is a
/// zero-byte file — and "unparseable ⇒ break it" would race exactly into it.
/// mtime closes that window and still guarantees no lock outlives the ceiling.
///
/// The pid is **not** consulted, deliberately: see [`LockRecord`].
pub fn acquire_run_lock(root: &Path, now_ms: u64) -> Result<RunLock, String> {
    let path = lock_path(root);
    std::fs::create_dir_all(root).map_err(|e| format!("create {}: {e}", root.display()))?;
    // Two attempts at most: take it, or break exactly one stale lock and take
    // it. A loop here would be a race against a peer that is legitimately
    // re-taking the lock, and losing that race is not an error worth retrying.
    for attempt in 0..2 {
        let record = LockRecord {
            pid: std::process::id(),
            started_ms: now_ms,
        };
        let body = serde_json::to_string(&record).unwrap_or_default();
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                // A lock whose body could not be written is still a held lock:
                // the exclusion comes from the file existing. Only the
                // staleness metadata is lost, and an unreadable record reads as
                // stale, which fails safe (a peer may break it after
                // `LOCK_MAX_AGE_MS`, never before).
                if let Err(e) = f.write_all(body.as_bytes()) {
                    warn!(
                        target: "offload",
                        path = %path.display(),
                        error = %e,
                        "detection updater: took the run lock but could not record its owner"
                    );
                }
                return Ok(RunLock { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if attempt > 0 {
                    return Err("another cImp instance is updating detection data".to_string());
                }
                let held = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<LockRecord>(&s).ok());
                let stale_reason = match &held {
                    Some(r) if r.started_ms > now_ms => Some(format!(
                        "it was taken {} ms in the future (the clock moved, or the state \
                         directory came from another machine)",
                        r.started_ms.saturating_sub(now_ms)
                    )),
                    Some(r) if now_ms.saturating_sub(r.started_ms) > LOCK_MAX_AGE_MS => {
                        Some(format!(
                            "it has been held for {} ms, past the {LOCK_MAX_AGE_MS} ms ceiling",
                            now_ms.saturating_sub(r.started_ms)
                        ))
                    }
                    Some(_) => None,
                    // No readable record: age it by mtime rather than breaking
                    // it, so the create/write window of a live peer is not a
                    // race. A file we cannot even stat has nothing left to
                    // vouch for it.
                    None => match file_age_ms(&path) {
                        Some(age) if age > LOCK_MAX_AGE_MS => Some(format!(
                            "its owner is unreadable and it was last touched {age} ms ago, past \
                             the {LOCK_MAX_AGE_MS} ms ceiling"
                        )),
                        Some(_) => None,
                        None => Some("its owner is unreadable and it cannot be aged".to_string()),
                    },
                };
                let Some(why) = stale_reason else {
                    return Err(match held {
                        Some(r) => format!(
                            "another cImp instance (pid {}) is updating detection data",
                            r.pid
                        ),
                        None => "another cImp instance is updating detection data".to_string(),
                    });
                };
                warn!(
                    target: "offload",
                    path = %path.display(),
                    reason = %why,
                    "detection updater: breaking a stale run lock"
                );
                if let Err(e) = std::fs::remove_file(&path) {
                    return Err(format!("could not break the stale run lock: {e}"));
                }
            }
            Err(e) => return Err(format!("take {}: {e}", path.display())),
        }
    }
    Err("another cImp instance is updating detection data".to_string())
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
        s.get_mut(Component::Rules).available_version = "2026.09.01".into();
        save_state(&dir, &s).expect("save");
        let back = load_state(&dir);
        assert_eq!(back.get(Component::Rules).installed_version, "2026.08.07");
        assert_eq!(back.get(Component::Rules).last_check_ms, 42);
        assert_eq!(back.get(Component::Rules).available_version, "2026.09.01");

        std::fs::write(dir.join(STATE_FILE), "{ not json").unwrap();
        assert_eq!(load_state(&dir), State::default());
        std::fs::write(dir.join(STATE_FILE), r#"{"schema":99}"#).unwrap();
        assert_eq!(load_state(&dir), State::default());
        std::fs::remove_dir_all(&dir).ok();
        // An absent directory is the cold-start case.
        assert_eq!(load_state(&dir), State::default());
    }

    /// #48/A1-1: the upgrade case, which no other test covers because every
    /// other one starts from a fresh `Tree`.
    ///
    /// A state file written by a build that predates the #46 split carries a
    /// 404 recorded as a bundle refusal. Loaded verbatim it would card
    /// "REJECTED" forever — nothing clears `last_failure` while the channel
    /// stays unreachable — and #46's new dismissal key means a prior dismissal
    /// would not even hold. So the failure record is dropped on load, and
    /// everything else in the file (which a `STATE_SCHEMA` bump would have
    /// thrown away) survives.
    #[test]
    fn a_pre_split_transport_failure_is_dropped_on_load_and_the_history_kept() {
        let dir = tmp();
        // Exactly the shape f645af4's predecessor wrote: no `last_outcome_kind`,
        // no `unreachable_streak`, no `last_failure_signature`, and a manifest
        // level failure signed with an empty version.
        std::fs::write(
            dir.join(STATE_FILE),
            r#"{
              "schema": 1,
              "rules": {
                "installed_version": "2026.08.07",
                "previous_version": "(shipped)",
                "last_check_ms": 1770000000000,
                "last_outcome": "update check failed: GET https://…/manifest.json: HTTP 404",
                "last_ok": false,
                "last_failure": "update check failed: GET https://…/manifest.json: HTTP 404",
                "last_failure_version": ""
              },
              "classifier": {
                "last_check_ms": 1770000000000,
                "last_outcome": "update check failed: GET https://…/manifest.json: HTTP 404",
                "last_ok": false,
                "last_failure": "update check failed: GET https://…/manifest.json: HTTP 404",
                "last_failure_version": ""
              }
            }"#,
        )
        .unwrap();

        let st = load_state(&dir);
        for c in Component::ALL {
            let cs = st.get(c);
            assert!(
                cs.last_failure.is_empty(),
                "{c:?} still carries the pre-split failure: {}",
                cs.last_failure
            );
            assert!(cs.last_failure_signature.is_empty());
        }
        // The install history — what a schema bump would have cost — is intact.
        let rules = st.get(Component::Rules);
        assert_eq!(rules.installed_version, "2026.08.07");
        assert_eq!(rules.previous_version, "(shipped)");
        assert_eq!(rules.last_check_ms, 1_770_000_000_000);
        // …and the verbatim record of that check is untouched: it is display
        // only, and the next check overwrites it.
        assert!(rules.last_outcome.contains("HTTP 404"));
        // Nothing reaches the Advisor from a healed file.
        let (available, failed, stalled) = super::super::signals_from(&st);
        assert!(available.is_empty() && failed.is_empty() && stalled.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The other half of the predicate: a pre-split failure that HAS a version
    /// was a real bundle refusal, its dismissal key is unchanged, and it must
    /// survive the load untouched.
    #[test]
    fn a_pre_split_bundle_refusal_keeps_its_record_and_its_dismissal_key() {
        let dir = tmp();
        std::fs::write(
            dir.join(STATE_FILE),
            r#"{
              "schema": 1,
              "rules": {
                "last_check_ms": 1770000000000,
                "last_ok": false,
                "last_failure": "checksum mismatch on `core.yar`",
                "last_failure_version": "2026.08.08"
              },
              "classifier": {}
            }"#,
        )
        .unwrap();

        let st = load_state(&dir);
        let rules = st.get(Component::Rules);
        assert_eq!(rules.last_failure, "checksum mismatch on `core.yar`");
        let (_, failed, _) = super::super::signals_from(&st);
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0].signature, "2026.08.08",
            "the pre-#46 key was the version, and it still is"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
