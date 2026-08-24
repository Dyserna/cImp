//! **Where** the updater reads and writes: the component directories under the
//! exe, the state root, and the live-reload hook that puts a freshly activated
//! bundle into the running screen. [`LocalBaseline`] lives here too — the
//! record of which `rules.d/local/` files were ALREADY failing to compile,
//! without which one bad user rule reads as an unhealthy publisher bundle
//! (#48, U-4).

use super::*;

// ── Layout ─────────────────────────────────────────────────────────────────

/// The four directories one run touches. Passed as a value rather than resolved
/// from `current_exe()` at each use so the whole pipeline is drivable against a
/// temp tree — and so a reader can see, in one struct, everything the updater
/// is able to write to.
#[derive(Debug, Clone)]
pub struct Layout {
    /// `<exe-dir>/detection-updates` — state file, staging, retained versions.
    pub state_root: PathBuf,
    /// `<exe-dir>/detection/rules.d` — the live rule bundle (whose `local/`
    /// subdirectory is never enumerated).
    pub rules_dest: PathBuf,
    /// `<exe-dir>/detection/smoke` — the validation corpus.
    pub smoke_dir: PathBuf,
}

impl Layout {
    /// The real layout. `None` when the exe path has no usable parent, in which
    /// case the updater stays inert rather than guessing at directories — the
    /// same discipline `signature::rules_dir` follows.
    pub fn resolve() -> Option<Self> {
        Some(Self {
            state_root: store::state_dir()?,
            rules_dest: store::destination(Component::Rules)?,
            smoke_dir: validate::smoke_dir()?,
        })
    }

    pub fn dest(&self, c: Component) -> &Path {
        match c {
            Component::Rules => &self.rules_dest,
        }
    }
}

/// How a component is made live after its files are in place.
///
/// A function rather than a direct call to `signature::reload()` because the
/// activation path must be able to reload a *specific* directory: production
/// reloads the process-wide rule set, the tests reload a temp directory without
/// disturbing the global one every other test reads.
/// `Send + Sync` because [`run`] holds one across the manifest `await` and the
/// scheduler's task must be spawnable — a plain `&dyn Fn` would make the whole
/// future non-`Send`.
pub type Reloader<'a> = &'a (dyn Fn(Component, &Path) -> Result<String, String> + Send + Sync);

/// The production reloader: recompile the live rules / rebuild the live `ort`
/// session, and report an error when the result is not healthy.
pub fn live_reload(c: Component, dir: &Path) -> Result<String, String> {
    match c {
        Component::Rules => {
            let s = super::signature::reload();
            // E-1: same edge as the Settings "Reload rules" action — a bundle
            // the updater just activated is a rules change a live MCP surface
            // has never been screened against.
            super::note_rules_reloaded();
            health_from_rules(&s, dir)
        }
    }
}

/// Turn a rules [`Status`](super::signature::Status) into "healthy, and here is
/// the summary" or "unhealthy, and here is why". One definition, shared by the
/// live reloader and the tests' directory-scoped one, so both judge health the
/// same way.
///
/// # The seam with D-2's fix (#48)
///
/// [`signature::reload`](super::signature::reload) now KEEPS the previously
/// compiled rules when a directory compiles to nothing, rather than disarming
/// the layer. That must not make a bad bundle look healthy — and it cannot,
/// structurally: the `Status` it returns describes what the DIRECTORY compiled
/// to, never what is in the live slot, so a bundle that produced no rule set
/// still arrives here as `files_loaded: 0, rules: 0` and still fails. Keeping
/// old rules changes what is screening while the rollback runs; it changes
/// nothing about the verdict on the bundle.
///
/// The predicate itself is [`Status::healthy`](super::signature::Status::healthy)
/// — read, not restated (#48, N-3). `files_loaded == 0 || rules == 0` staying a
/// hard failure here is the never-degrade-to-nothing gate, so it must have
/// exactly one definition and every surface must bind that one.
/// The prefix [`super::signature::read_sources`] gives a file it read from the
/// user-owned overlay. One definition, because the whole U-4 fix keys on it —
/// and since #48/M-13 so does the collision rename, which lives beside the
/// reader, so the definition lives there too and this is the alias.
pub(super) const LOCAL_PREFIX: &str = super::signature::LOCAL_PREFIX;

/// V32 Phase C3, #48 finding U-4 — which `rules.d/local/` files were **already
/// failing to compile before** an activation.
///
/// # The bug this exists to close
///
/// Validation compiles the staged bundle alone; a staging directory has no
/// `local/`. The post-activation health check compiles staged **plus `local/`**
/// and fails on `files_failed > 0`. So one malformed or identifier-colliding
/// `local/mine.yar` read as an unhealthy *bundle*: a perfectly good update was
/// rolled back, blamed on the publisher, and re-attempted — full download,
/// validate, swap, roll back — every 24 h, forever. The update channel was
/// frozen by a file the updater is contractually forbidden to touch.
///
/// The veto was incoherent on its own terms: at startup the app already
/// tolerates that same broken file (warn, keep the rest live), so the only
/// place it was fatal was the one place the user could not act on it.
///
/// # What is forgiven, and what is not
///
/// **Every `local/` failure, whether or not it predates the swap** (#48, M-13).
/// A failure in a bundle file is never forgiven at all — the prefix test
/// excludes it.
///
/// The original fix forgave only failures *present before the swap*, on the
/// reasoning that a `local/` file which compiled before and fails after is a
/// collision the bundle introduced and therefore the publisher's fault. That
/// reproduces U-4's exact symptom in the case the README tells users to expect.
/// The README's advice is "put your own rules in `rules.d/local/`"; a user who
/// takes it and happens to name a rule the way a future shipped rule is named
/// gets: bundle downloaded, validated, swapped, health check fails on their
/// file, rolled back, blamed on the publisher — every 24 h, forever. The update
/// channel is frozen by a file the updater is contractually forbidden to touch,
/// and the user is never told which file did it or that anything is wrong.
///
/// Rolling back was also incoherent with the ordering the rest of the layer
/// already commits to. [`super::signature::read_sources`] reads the shipped
/// bundle FIRST precisely so that on an identifier collision it is the *local*
/// file that loses — "losing a shipped rule to a stranger's typo would silently
/// weaken the layer". Having decided that the shipped rule wins the collision,
/// vetoing the shipped bundle over it says the opposite.
///
/// So the collision resolves the way `read_sources` already says it does: the
/// bundle goes live, the one colliding user file is skipped, and the skip is
/// **reported** — the returned sentence names it in the activity row and
/// Settings, `detection.local_rules_broken.v1` cards it with the file name and
/// the folder, and renaming one rule fixes it. That is U-4's other half doing
/// the job it was built for.
///
/// The baseline is still taken, and still matters: it is what lets the message
/// distinguish "this was already broken" from "this bundle collided with it",
/// which is the difference between a note and an apology.
///
/// And the never-degrade-to-nothing gate is untouched: `files_loaded == 0 ||
/// rules == 0` (i.e. `!Status::armed`) stays a hard failure whatever the
/// baseline says. Forgiveness only ever converts *degraded* into *degraded and
/// reported*; it can never convert *disarmed* into healthy.
#[derive(Debug, Clone, Default)]
pub struct LocalBaseline {
    /// `local/…`-prefixed names that already failed, as
    /// [`super::signature::Status::failed`] spells them.
    already_failing: BTreeSet<String>,
}

impl LocalBaseline {
    /// Compile `dest` as it stands right now and keep the `local/` failures.
    ///
    /// Uses [`super::signature::compile_report`], the pure reporter, so taking
    /// a baseline never disturbs the live rule set — this runs immediately
    /// before an activation that is about to swap that set.
    pub fn snapshot(dest: &Path) -> Self {
        let (_, status) = super::signature::compile_report(Some(dest));
        Self::from_failed(&status.failed)
    }

    /// The pure constructor, so the tests can state a baseline directly.
    pub fn from_failed(failed: &[String]) -> Self {
        Self {
            already_failing: failed
                .iter()
                .filter(|f| f.starts_with(LOCAL_PREFIX))
                .cloned()
                .collect(),
        }
    }

    /// Re-judge a post-activation health failure against this baseline.
    ///
    /// `Ok` means "the bundle is fine; these `local/` files were already broken
    /// and still are". `Err` passes the original verdict through unchanged —
    /// deliberately the original string, not a rewritten one, so the card the
    /// user reads is the one the reloader wrote.
    ///
    /// The directory is recompiled to answer this. That is one extra YARA
    /// compile, on the failure path of an operation that runs at most once a
    /// day, and it buys the alternative: threading a baseline through
    /// [`Reloader`], `activate`, `roll_back`, `reload_note`, `recover_interrupted`
    /// and `revert_inner`, four of which have no use for one. The compile is
    /// `compile_report`, the same pure function [`snapshot`](Self::snapshot)
    /// uses, so both halves of the comparison are produced by one code path.
    pub fn forgive(&self, dir: &Path, why: String) -> Result<String, String> {
        let (_, status) = super::signature::compile_report(Some(dir));
        if status.healthy {
            // The reloader and this disagree — trust the stricter answer and
            // keep the failure rather than inventing a pass.
            return Err(why);
        }
        if !status.armed {
            // The never-degrade-to-nothing gate. Not forgivable, ever.
            return Err(why);
        }
        // Only a BUNDLE file's failure vetoes. A `local/` file is the user's,
        // and it can neither be fixed by rolling back nor be blamed on the
        // publisher without freezing the channel (#48, M-13).
        let unforgiven: Vec<&String> = status
            .failed
            .iter()
            .filter(|f| !f.starts_with(LOCAL_PREFIX))
            .collect();
        if !unforgiven.is_empty() {
            return Err(why);
        }
        let (pre_existing, introduced): (Vec<&String>, Vec<&String>) = status
            .failed
            .iter()
            .partition(|f| self.already_failing.contains(*f));
        warn!(
            target: "offload",
            already_failing = %join_names(&pre_existing),
            newly_failing = %join_names(&introduced),
            dir = %dir.display(),
            "detection updater: the new bundle is live; these user rules in rules.d/local/ are \
             being skipped (detection.local_rules_broken.v1 names them to the user)"
        );
        let mut note = format!(
            "{} file(s), {} rule(s) live{}",
            status.files_loaded,
            status.rules,
            status.rename_note()
        );
        if !pre_existing.is_empty() {
            note.push_str(&format!(
                "; {} pre-existing broken file(s) in `rules.d/local/` ({}) were skipped, as they \
                 already were before this update",
                pre_existing.len(),
                join_names(&pre_existing)
            ));
        }
        if !introduced.is_empty() {
            note.push_str(&format!(
                "; {} file(s) in `rules.d/local/` ({}) stopped compiling with this bundle and are \
                 being skipped. An identifier a shipped rule has taken is normally handled by \
                 loading YOUR rule under a `{}` name instead (#48, M-13), so reaching this means \
                 the rename did not apply — the file has another compile error, or every renamed \
                 form of the identifier is taken as well. The update was NOT rolled back, because \
                 rolling it back would freeze every future update behind one file of yours",
                introduced.len(),
                join_names(&introduced),
                super::signature::CUSTOM_PREFIX
            ));
        }
        Ok(note)
    }
}

/// `", "`-join borrowed names — the one formatting helper the forgiveness note
/// needs, so the three lists in it are spelled identically.
fn join_names(names: &[&String]) -> String {
    names
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn health_from_rules(s: &super::signature::Status, dir: &Path) -> Result<String, String> {
    if !s.healthy {
        return Err(format!(
            "{} file(s) loaded, {} rule(s), {} rejected ({}) from {}",
            s.files_loaded,
            s.rules,
            s.files_failed,
            s.failed.join(", "),
            dir.display()
        ));
    }
    // #48/M-13: `rename_note` is empty unless a user rule is live under a
    // renamed identifier, so the ordinary sentence is unchanged — and when it
    // is not empty, the fact rides the ONE string every caller propagates (the
    // activation detail, the activity row, the Settings "Last check" line)
    // rather than needing a new channel of its own.
    Ok(format!(
        "{} file(s), {} rule(s) live{}",
        s.files_loaded,
        s.rules,
        s.rename_note()
    ))
}
