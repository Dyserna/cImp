//! What the Settings pane renders: [`UpdaterStatus`] and its per-component rows,
//! built from the cached [`State`] plus live settings. Serialization only — no
//! decision in this file changes what the updater will do.

use super::*;

// ── What Settings renders ──────────────────────────────────────────────────

/// One component's updater readout.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ComponentStatus {
    pub component: &'static str,
    /// The mode as its settings string (`off`/`check`/`auto`), so the Settings
    /// select can compare it to the value it writes without a mapping table.
    pub mode: &'static str,
    /// Empty before the first successful update: the shipped bundle carries no
    /// manifest version, and inventing one would make the first real update
    /// look like a downgrade.
    pub installed_version: String,
    pub previous_version: String,
    /// True exactly when Revert has something to restore.
    pub can_revert: bool,
    pub available_version: String,
    pub available_notes: String,
    pub last_check_ms: u64,
    pub last_outcome: String,
    pub last_ok: bool,
    /// [`Outcome::as_str`] for that check. Settings branches on `unavailable`
    /// to say "could not reach the update channel" instead of "an update was
    /// refused" — the distinction #46 is about, which `last_ok` cannot carry.
    pub last_outcome_kind: String,
    /// Consecutive unreachable checks, so Settings can say how long this has
    /// been going on rather than repeating one 404 forever.
    pub unreachable_streak: u32,
    pub last_failure: String,
    /// Files a rollback could not put back, so the live set is short of them
    /// (#48, M-11). Empty is the healthy steady state; non-empty means reduced
    /// coverage that no other field on this struct can express — `last_ok` is
    /// about the last CHECK, and the rule counts are about what compiled, not
    /// about what should have been there.
    pub unrestored_files: Vec<String>,
}

/// The whole updater surface, folded into `DetectionStatus` so the Settings
/// poller that already asks for rule counts gets this for free.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdaterStatus {
    pub components: Vec<ComponentStatus>,
    /// The manifest URL actually in use, so "nothing ever updates" is
    /// diagnosable without opening the settings file.
    pub manifest_url: String,
    pub interval_hours: u32,
    /// What "Open rules folder" opens — shown as text too, so the path is
    /// visible even when the button cannot open a file manager.
    pub rules_dir: String,
    pub state_dir: String,
    /// #48 (M-21): whether the updater may do anything at all —
    /// [`updates_enabled`], published so a surface that greys a control reads the
    /// SAME predicate the two IPC commands enforce instead of a second opinion
    /// assembled from the resolved-scope matrix.
    pub updates_enabled: bool,
    /// #48 (M-21): [`worker_only_detection`] — detection is armed for the offload
    /// worker while this updater is inert.
    ///
    /// It exists so no surface has to say "detection is off" about a layer that is
    /// running. Only ever true when `updates_enabled` is false, so a reader can
    /// treat the pair as one three-valued answer: on / off / off here but on in
    /// the worker.
    pub worker_only_detection: bool,
}

/// Build the status from the cached state plus the live settings.
pub fn status(settings: &Settings) -> UpdaterStatus {
    let st = state();
    let sched = Schedule::from_settings(settings);
    UpdaterStatus {
        components: Component::ALL
            .iter()
            .map(|c| {
                let cs = st.get(*c);
                ComponentStatus {
                    component: c.as_str(),
                    mode: sched.mode(*c).as_str(),
                    installed_version: cs.installed_version.clone(),
                    previous_version: cs.previous_version.clone(),
                    can_revert: !cs.previous_version.is_empty(),
                    available_version: cs.available_version.clone(),
                    available_notes: cs.available_notes.clone(),
                    last_check_ms: cs.last_check_ms,
                    last_outcome: cs.last_outcome.clone(),
                    last_ok: cs.last_ok,
                    last_outcome_kind: cs.last_outcome_kind.clone(),
                    unreachable_streak: cs.unreachable_streak,
                    last_failure: cs.last_failure.clone(),
                    unrestored_files: cs.unrestored_files.clone(),
                }
            })
            .collect(),
        manifest_url: manifest_url(settings),
        interval_hours: sched.interval_hours.max(MIN_INTERVAL_HOURS),
        rules_dir: super::signature::rules_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        state_dir: store::state_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        // #48 (M-21): both from the predicates themselves, in one snapshot, so the
        // readout cannot disagree with the buttons' enforcement.
        updates_enabled: updates_enabled(settings),
        worker_only_detection: worker_only_detection(settings),
    }
}
