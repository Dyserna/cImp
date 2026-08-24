//! One component’s run, end to end: download, verify, validate, activate — and
//! the recovery that makes it survivable. Decision 13 (old data stays live on
//! any failure) is enforced here and nowhere else, which is why the rollback,
//! the activation journal and the interrupted-swap repair share the file with
//! the happy path they exist to undo.

use super::*;

// ── One component's run ────────────────────────────────────────────────────

/// The result of checking (and possibly applying) one component.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub component: Component,
    pub outcome: Outcome,
    pub version: String,
    pub detail: String,
}

/// Check one component against `man`, applying it when `mode` allows.
///
/// Separated from the manifest fetch so one download serves both components,
/// and so tests drive the whole pipeline from a parsed manifest.
pub async fn run_component(
    c: Component,
    man: &Manifest,
    mode: Mode,
    fetcher: &dyn Fetcher,
    layout: &Layout,
    reload: Reloader<'_>,
) -> RunResult {
    let now = crate::activity::now_ms();
    let root = layout.state_root.clone();
    let Some(entry) = man.components.get(&c) else {
        return finish(
            c,
            &root,
            now,
            Outcome::UpToDate,
            String::new(),
            format!(
                "the manifest lists no `{}` component; nothing to do",
                c.as_str()
            ),
        );
    };
    let installed = state_at(&root).get(c).installed_version.clone();
    if !manifest::is_newer(&entry.version, &installed) {
        return finish(
            c,
            &root,
            now,
            Outcome::UpToDate,
            entry.version.clone(),
            format!(
                "installed version `{}` is current (the manifest offers `{}`)",
                if installed.is_empty() {
                    "(shipped)"
                } else {
                    installed.as_str()
                },
                entry.version
            ),
        );
    }
    if !manifest::app_version_satisfies(entry.min_app_version.as_deref()) {
        return finish(
            c,
            &root,
            now,
            Outcome::Available,
            entry.version.clone(),
            format!(
                "version `{}` needs cImp {} or newer (this build is {}) — update the app to take it",
                entry.version,
                entry.min_app_version.as_deref().unwrap_or("?"),
                env!("CARGO_PKG_VERSION")
            ),
        );
    }
    if mode != Mode::Auto {
        return finish(
            c,
            &root,
            now,
            Outcome::Available,
            entry.version.clone(),
            format!(
                "version `{}` is available; this component is set to check-only, so nothing was \
                 downloaded and nothing on disk changed",
                entry.version
            ),
        );
    }

    // ── auto: download, verify, validate, activate ──────────────────────
    let staging = store::staging_dir(&root, c);
    store::wipe_dir(&staging);
    let applied = apply_component(c, entry, fetcher, layout, &staging, reload).await;
    // Whatever happened, the staging directory does not outlive the run — a
    // rejected bundle must leave nothing behind that a later reader could
    // mistake for validated content.
    store::wipe_dir(&staging);
    match applied {
        Ok(detail) => finish(c, &root, now, Outcome::Applied, entry.version.clone(), detail),
        // The version rides along for the row label in both cases; `finish`
        // decides what, if anything, is recorded about the DATA.
        Err(ApplyFailure::Rejected(detail)) => finish(
            c,
            &root,
            now,
            Outcome::Rejected,
            entry.version.clone(),
            detail,
        ),
        Err(ApplyFailure::Unreachable(detail)) => finish(
            c,
            &root,
            now,
            Outcome::Unavailable,
            entry.version.clone(),
            detail,
        ),
    }
}

/// Why an apply did not happen — the #46 outcome split, applied to the
/// **artifact** fetch this time (#48, M-9).
///
/// #46 split "the channel never answered" from "a document reached us and a
/// check said no" at the manifest fetch, and stopped there. Everything after it
/// funnelled through one `Result<String, String>` that
/// [`run_component`] recorded as [`Outcome::Rejected`], so an artifact 404, a
/// timeout, a proxy login page, a dropped connection mid-download — none of
/// which is a bundle being refused, and none of which is a decision anyone
/// made — raised the security card that means *someone published something we
/// would not take*, wrote an `ok:false` row, and **reset `unreachable_streak`**,
/// which is the counter whose whole job is to notice that the channel has gone
/// quiet.
///
/// That is not a corner case here. The deploy note publishes the manifest and
/// the artifacts as separate steps, so "manifest reachable, artifact not yet"
/// is the ordinary state of a half-published channel — the likely steady state
/// on the day `detection-v1` first goes up, and a daily red card for a bundle
/// that is perfectly fine.
///
/// Exactly one thing maps to `Unreachable`: a [`Fetcher::get`] transport error
/// on an artifact URL. A response that ARRIVED and disagrees with the manifest
/// — wrong size, wrong digest — is a refusal, because a document reached us and
/// a check said no. Stated as a two-variant enum rather than a string prefix so
/// a future call site has to answer the question.
enum ApplyFailure {
    /// A bundle reached us and a check refused it.
    Rejected(String),
    /// The artifact could not be fetched at all. Nothing was refused.
    Unreachable(String),
}

impl From<String> for ApplyFailure {
    /// Every `?` inside [`apply_component`] that is not the artifact fetch
    /// itself is a refusal. Deliberately the default, so forgetting to classify
    /// fails toward the louder card rather than toward silence.
    fn from(s: String) -> Self {
        ApplyFailure::Rejected(s)
    }
}

/// Download + verify + validate + activate. Every early return leaves the live
/// data exactly as it was.
async fn apply_component(
    c: Component,
    entry: &manifest::ComponentEntry,
    fetcher: &dyn Fetcher,
    layout: &Layout,
    staging: &Path,
    reload: Reloader<'_>,
) -> Result<String, ApplyFailure> {
    std::fs::create_dir_all(staging).map_err(|e| format!("create {}: {e}", staging.display()))?;

    // Fetch each artifact into memory, verify its size and digest, and only
    // then write it. Nothing untrusted touches disk before its checksum is
    // confirmed, and nothing reaches a parser before that either.
    for f in &entry.files {
        let bytes = match fetcher.get(&f.url, f.size).await {
            Ok(b) => b,
            // Transport, on an artifact. The manifest answered and this did
            // not; nothing was refused (#48, M-9).
            Err(e) if e.kind == manifest::FetchErrorKind::Transport => {
                return Err(ApplyFailure::Unreachable(format!(
                    "`{}` from version `{}` could not be downloaded ({e}). Nothing was written and \
                     the current detection data is still live.",
                    f.name, entry.version
                )))
            }
            // A body arrived and is bigger than the manifest says it is. The
            // symmetric case — a body SHORT of its declared size — is caught by
            // the length check below and refused, so this one must be refused
            // too, or the same disagreement would be an outage in one direction
            // and a refusal in the other.
            Err(e) => {
                return Err(ApplyFailure::Rejected(format!(
                    "`{}` is larger than the {} bytes the manifest declares ({e}) — rejected \
                     before the content was written or parsed",
                    f.name, f.size
                )))
            }
        };
        if bytes.len() as u64 != f.size {
            return Err(ApplyFailure::Rejected(format!(
                "`{}` is {} bytes but the manifest declares {} — rejected before the content was \
                 written or parsed",
                f.name,
                bytes.len(),
                f.size
            )));
        }
        let got = manifest::sha256_hex(&bytes);
        if got != f.sha256 {
            return Err(ApplyFailure::Rejected(format!(
                "checksum mismatch on `{}` (expected {}, got {}) — rejected before the content \
                 was written or parsed",
                f.name, f.sha256, got
            )));
        }
        std::fs::write(staging.join(&f.name), &bytes)
            .map_err(|e| format!("stage `{}`: {e}", f.name))?;
    }
    let names: Vec<String> = entry.files.iter().map(|f| f.name.clone()).collect();
    validate::staged_files_present(staging, c, &names)?;

    // Validation and activation are blocking work (a YARA compile bounded by
    // `validate::COMPILE_BUDGET`, an ONNX session, a handful of file moves) and
    // run inline rather than on the blocking pool. Two reasons: the reloader is
    // a borrowed closure that cannot be moved into a `spawn_blocking` task, and
    // this runs at most once a day on a background task with nothing waiting on
    // it — the seconds-scale ceiling is the whole point of having a ceiling.
    match c {
        Component::Rules => {
            let _ = &names;
            // Past the fetch, every failure is a document being refused.
            validate_and_activate_rules(staging, layout, &entry.version, reload)
                .map_err(ApplyFailure::Rejected)
        }
    }
}

/// The rules half.
/// Fraction of the live rule count a candidate bundle may not fall below.
///
/// Curation churn moves the count by a few rules; halving it is not curation.
const COVERAGE_FLOOR: usize = 2;

/// Refuse a bundle that would sharply shrink the live rule set.
///
/// **This is a curation guard, not an anti-tamper control**, and the distinction
/// is worth being honest about (#48, N-10 / the H-6 decision). The gauntlet's
/// positive control is the shipped `smoke/hostile/` corpus — public, on every
/// user's disk — so a bundle of three rules that match exactly those documents
/// and nothing else compiles clean, scans fast, hits every hostile control,
/// misses every benign one, and activates green. `validate.rs`'s own header
/// claims to stop a bundle that "would quietly disable the layer", and that
/// bundle walks straight through.
///
/// What this does NOT defend against is a hostile publisher: anyone who can
/// write the manifest can also write the rule count, and — since the channel's
/// trust root is `contents: write` on the repo that ships the binary — can also
/// ship a cImp release with detection removed outright. That is precisely why
/// bundle signing was declined (H-6): a key reachable by the compromise it
/// defends against is ceremony.
///
/// What it DOES catch is the likelier failure by far: a curator publishing a
/// half-built bundle. That is worth ten lines.
///
/// Compares against the **shipped** set only — `store::managed_rule_files` is
/// non-recursive, so a user's `local/` rules never inflate the baseline and a
/// user who writes twenty of their own cannot make every future bundle look
/// like a regression. An unreadable or empty live set yields no baseline and
/// the check passes: a first install has nothing to compare against.
pub(super) fn coverage_floor(candidate_rules: usize, dest: &Path) -> Result<(), String> {
    let live: Vec<(String, String)> = store::managed_rule_files(dest)
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().to_string();
            Some((name, std::fs::read_to_string(&p).ok()?))
        })
        .collect();
    if live.is_empty() {
        return Ok(());
    }
    let (compiled, _failed) = super::signature::compile_sources(&live);
    let Some(compiled) = compiled else {
        // The live set does not compile, so it is not a baseline worth
        // defending — the candidate can only be an improvement.
        return Ok(());
    };
    let live_rules = compiled.iter().count();
    if live_rules > 0 && candidate_rules * COVERAGE_FLOOR < live_rules {
        return Err(format!(
            "coverage floor: the candidate bundle carries {candidate_rules} rule(s) against the \
             {live_rules} currently live — a drop that large is a half-built bundle, not curation. \
             The smoke corpus cannot catch this on its own (a bundle matching only the shipped \
             hostile controls passes every other gate), so the count is checked directly."
        ));
    }
    Ok(())
}

fn validate_and_activate_rules(
    staging: &Path,
    layout: &Layout,
    version: &str,
    reload: Reloader<'_>,
) -> Result<String, String> {
    let sources = super::signature::read_sources(staging);
    let corpus = validate::load_corpus(&layout.smoke_dir);
    let report = validate::validate_rules(&sources, &corpus)?;
    coverage_floor(report.rules, layout.dest(Component::Rules))?;
    // #48, U-4: snapshot which `local/` files are ALREADY broken, before the
    // swap, so the post-activation health check judges the BUNDLE rather than
    // the directory. Taken here (not inside `activate`) because it must be read
    // from the destination while the OLD bundle is still in it.
    let baseline = LocalBaseline::snapshot(layout.dest(Component::Rules));
    let judged = |c: Component, dir: &Path| match reload(c, dir) {
        Ok(live) => Ok(live),
        Err(why) => baseline.forgive(dir, why),
    };
    // The live description, not the validation report's counts (#48, M-13).
    // They are close but not the same number — validation compiles the bundle
    // alone, the live set includes `rules.d/local/` — and only this one can say
    // that a user file was skipped, which after M-13 is a thing an APPLIED
    // update has to be able to report. Dropping it on the floor is how U-4's
    // forgiveness message stayed invisible for two fix rounds.
    let live = activate(Component::Rules, staging, layout, version, &judged)?;
    Ok(format!(
        "activated rules `{version}`: {live}. Validated against {} benign + {} hostile control \
         document(s) ({} bundle file(s), {} rule(s); compile {} ms, slowest scan {} ms). The \
         previous bundle is retained and can be reverted from Settings; `rules.d/local/` was not \
         touched.",
        report.benign_samples,
        report.hostile_samples,
        report.files,
        report.rules,
        report.compile_ms,
        report.slowest_scan_ms
    ))
}

/// Swap the staged files into the component's destination, archiving the
/// current ones, then hot-reload.
///
/// The ordering is what "atomic-as-possible" means here. A directory swap has
/// no all-or-nothing primitive across two multi-file directories, so instead:
/// the outgoing files are archived FIRST, the incoming ones moved in second,
/// and a failure at **any** point in either step is undone before returning.
/// The window in which the destination is short of files is the move loop
/// itself, and the only way out of it is "new set live and healthy" or "old set
/// back".
///
/// The hot-reload is part of the transaction, not a follow-up: a set that moved
/// perfectly but does not LOAD (an identifier collision with a `local/` rule, a
/// file quarantined by antivirus between validation and activation) is rolled
/// back exactly like a failed move.
///
/// # The two loops need opposite undos (#48, U-2)
///
/// As first built only the *second* loop rolled back; the archive loop
/// propagated its first error with a bare `?`, so the most ordinary Windows
/// failure there is — AV real-time scanning, or the user holding a rule file
/// open through the panel's own *Open rules folder* button, making both
/// `rename` and `copy` fail with a sharing violation — left `rules.d` holding a
/// subset of its files, with no reload, no rollback and no `previous_version`
/// recorded, so Revert stayed disabled. The signature layer then ran at reduced
/// coverage across every restart: exactly the silent degradation decision 13
/// forbids.
///
/// The undos are **not** the same, which is why there are two of them:
/// [`roll_back`] clears the destination before restoring, because after the
/// move loop started the destination holds staged files that must not survive;
/// [`restore_archived`] alone is what the archive loop needs, because at that
/// point the destination still holds every file the loop has not reached and
/// clearing it would destroy the only copy of them.
fn activate(
    c: Component,
    staging: &Path,
    layout: &Layout,
    version: &str,
    reload: Reloader<'_>,
) -> Result<String, String> {
    let dest = layout.dest(c);
    let root = &layout.state_root;
    std::fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    // ONE label for the outgoing version, used both for the archive directory
    // and for the `previous_version` recorded in state. They were briefly two
    // expressions and the empty case diverged (`unknown` on disk vs.
    // `(shipped)` in state), which made Revert look for a directory that had
    // never existed — the archive path and the recorded name must be derived
    // from the same string.
    let installed = state_at(root).get(c).installed_version.clone();
    let outgoing_version = if installed.is_empty() {
        SHIPPED_VERSION.to_string()
    } else {
        installed
    };
    let archive = store::previous_dir(root, c, &outgoing_version);
    let mut archived: Vec<(PathBuf, PathBuf)> = prepare_archive(c, &archive, dest);

    // Archive the current managed set. For rules this is the top level of
    // `rules.d` only, so `local/` is untouched by construction.
    journal(root, c, store::Phase::Archiving, &archive, dest);
    for p in store::managed_files(dest, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        let to = archive.join(&name);
        if let Err(e) = store::move_file(&p, &to) {
            // Restore-only: the files this loop has NOT reached are still at
            // `dest` and are the only copy of themselves.
            let note = restore_only(c, root, dest, &archived, reload);
            return Err(format!(
                "archiving the current bundle failed ({e}); nothing was replaced and the previous \
                 version is still live{note}"
            ));
        }
        archived.push((to, p.clone()));
    }

    // Move the staged set in. On any failure, put the archive back.
    journal(root, c, store::Phase::Moving, &archive, dest);
    for p in store::managed_files(staging, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        if let Err(e) = store::move_file(&p, &dest.join(&name)) {
            let note = roll_back(c, root, &archive, dest, &archived, reload);
            return Err(format!(
                "activating the staged bundle failed ({e}); the previous version was restored{note}"
            ));
        }
    }

    match reload(c, dest) {
        Ok(live) => {
            // Cleared BEFORE the state write, deliberately. A crash in the gap
            // leaves the new files live and the state still naming the old
            // version, so the next check simply applies the same bundle again —
            // idempotent. The other order would let a crash leave a journal
            // that undoes an update the state already claims.
            store::clear_journal(root);
            update_state_at(root, |s| {
                let cs = s.get_mut(c);
                cs.previous_version = outgoing_version.clone();
                cs.installed_version = version.to_string();
                // A full swap resolves any outstanding restore debt by
                // construction: `dest` now holds a complete validated set and
                // `archive` holds a complete outgoing one (see
                // `prepare_archive`). Nothing is missing, so nothing is owed.
                cs.unrestored_files.clear();
            });
            info!(
                target: "offload",
                component = c.as_str(),
                version,
                live = %live,
                "detection updater: bundle activated"
            );
            Ok(live)
        }
        Err(why) => {
            let note = roll_back(c, root, &archive, dest, &archived, reload);
            Err(format!(
                "the activated bundle did not load cleanly ({why}); the previous version was \
                 restored{note}"
            ))
        }
    }
}

/// Ready `archive` to receive the outgoing set, **keeping any file the
/// destination is missing** (#48, M-11's other half).
///
/// This used to be a bare [`store::wipe_dir`], and with M-11 fixed that becomes
/// a data-loss path rather than hygiene. A rollback that could not put
/// `core.yar` back leaves it in `previous/<outgoing>/` — the archive of the very
/// version being replaced — as the only copy in existence. The next check then
/// downloads a newer bundle, computes the same archive path from the same
/// unchanged `installed_version`, and wipes it.
///
/// The file belongs where it already is: this archive is the outgoing version's
/// archive, and a file the destination lacks is part of that version and
/// nothing else. So it stays, and the returned `archived` list starts with it —
/// which also means a rollback puts the COMPLETE old set back, repairing the
/// debt rather than perpetuating it.
///
/// Everything else in the archive is a stale copy of a file that is still live
/// at `dest` (the previous run's archive of the same version) and is removed,
/// so the archive never accumulates.
///
/// Returns the `(in-archive, restore-to)` pairs for the files kept, in the same
/// shape the archive loop appends to.
pub(super) fn prepare_archive(
    c: Component,
    archive: &Path,
    dest: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let live: BTreeSet<std::ffi::OsString> = store::managed_files(dest, c)
        .iter()
        .map(|p| p.file_name().unwrap_or_default().to_os_string())
        .collect();
    let mut kept = Vec::new();
    for p in store::managed_files(archive, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        if live.contains(&name) {
            let _ = std::fs::remove_file(&p);
        } else {
            kept.push((p.clone(), dest.join(&name)));
        }
    }
    if kept.is_empty() {
        // Nothing worth keeping: wipe the directory itself so non-rule
        // leftovers (a partial download, a file from a build that managed a
        // different extension set) do not accumulate either.
        store::wipe_dir(archive);
    } else {
        warn!(
            target: "offload",
            component = c.as_str(),
            archive = %archive.display(),
            files = kept.len(),
            "detection updater: the retained copy still holds file(s) the live set is missing \
             from an earlier failed restore; keeping them rather than wiping the last copy"
        );
    }
    kept
}

/// Record an in-flight swap so a crash between the two loops is recoverable
/// ([`store::Journal`]).
pub(super) fn journal(root: &Path, c: Component, phase: store::Phase, archive: &Path, dest: &Path) {
    store::write_journal(
        root,
        &store::Journal {
            component: c.as_str().to_string(),
            phase,
            archive: archive.to_path_buf(),
            dest: dest.to_path_buf(),
        },
    );
}

/// Finish an interrupted swap, once, before this run touches anything.
///
/// The recovery decision 13's "old data stays live on any failure" needs to
/// survive a **kill**, not just an error return (#48, U-2). Without it a crash
/// between the two loops left `rules.d` short, and the next activation then
/// recomputed the archive path from the unchanged `installed_version` and
/// [`store::wipe_dir`]'d it — turning a recoverable interruption into the
/// permanent loss of the only surviving copy of the old bundle.
///
/// Called from [`run`] and [`revert`] under the run lock, so it can never race
/// the swap it is repairing. A journal whose recorded destination is not this
/// layout's is discarded untouched: the exe moved, and those paths are not
/// ours to write to.
pub(super) fn recover_interrupted(layout: &Layout, reload: Reloader<'_>) {
    let root = &layout.state_root;
    let Some(j) = store::read_journal(root) else {
        return;
    };
    let Some(c) = Component::parse(&j.component) else {
        warn!(
            target: "offload",
            component = %j.component,
            "detection updater: an activation journal names a component this build does not know; \
             discarding it"
        );
        store::clear_journal(root);
        return;
    };
    let dest = layout.dest(c);
    if j.dest != dest {
        warn!(
            target: "offload",
            recorded = %j.dest.display(),
            current = %dest.display(),
            "detection updater: an activation journal points at a different destination than this \
             layout; discarding it rather than writing to a path we no longer own"
        );
        store::clear_journal(root);
        return;
    }
    let archived: Vec<(PathBuf, PathBuf)> = store::managed_files(&j.archive, c)
        .into_iter()
        .map(|p| {
            let name = p.file_name().unwrap_or_default().to_os_string();
            (p, dest.join(&name))
        })
        .collect();
    if archived.is_empty() {
        // Nothing was archived before the interruption, or the archive is
        // already back. There is nothing to undo and nothing to lose — and no
        // debt either, so the state field is cleared with the journal.
        // Nothing owed and nothing to say: the note is for a caller composing
        // an error message, and this path has none.
        let _ = settle_restore(root, c, &[]);
        return;
    }
    match j.phase {
        // The destination still holds every file the archive loop did not
        // reach; clearing it would destroy them.
        store::Phase::Archiving => {}
        // The destination holds however many staged files landed. They must go:
        // old-plus-some-new is a set no curation step ever validated.
        store::Phase::Moving => {
            for p in store::managed_files(dest, c) {
                let _ = std::fs::remove_file(&p);
            }
        }
        // **A rollback was itself in flight (#48, M-10).** The destination has
        // already been cleared of staged files and holds however much of the
        // archive got put back. Deleting `managed_files(dest)` here — which is
        // what `Moving` does, and what this state used to be misread as — would
        // destroy exactly those restored files, and the archive no longer holds
        // a second copy. Restore-only, and idempotent, so running it again over
        // a rollback that actually finished is a no-op.
        store::Phase::Restoring => {}
    }
    let unrestored = restore_archived(&archived);
    let debt = settle_restore(root, c, &unrestored);
    if debt.is_empty() {
        warn!(
            target: "offload",
            component = c.as_str(),
            phase = ?j.phase,
            files = archived.len(),
            "detection updater: an update was interrupted mid-swap; the previous version was \
             restored"
        );
    } else {
        // Deliberately NOT the reassuring sentence above: `settle_restore` has
        // already kept the journal, so this repeats on the next run — and the
        // Advisor card is what the user actually sees.
        warn!(
            target: "offload",
            component = c.as_str(),
            phase = ?j.phase,
            files = %unrestored.join(", "),
            "detection updater: an interrupted update could only be PARTLY undone; the live set \
             is short of these files and the retry is queued"
        );
    }
    if let Err(e) = reload(c, dest) {
        warn!(
            target: "offload",
            component = c.as_str(),
            error = %e,
            "detection updater: the recovered version did not reload cleanly"
        );
    }
}

/// Finish an interrupted swap against `layout`, taking the run lock — the entry
/// point for callers that are not already inside a [`run`].
///
/// Only [`store::acquire_run_lock`] is taken, not the process-local
/// [`run_lock`] mutex as well, and that is deliberate: the file lock excludes
/// **every** contender including this process (its staleness rule is age, never
/// pid — see [`store::acquire_run_lock`]), so it is sufficient on its own, and
/// taking the async mutex from a synchronous launch path would mean either
/// `blocking_lock` (which panics if a runtime is ever wrapped around startup)
/// or `try_lock` (which would silently skip the repair whenever anything else
/// happened to hold it). One lock, one story.
///
/// Declining is safe: a peer holding the lock is inside `run`, which does this
/// same recovery on the way in.
pub fn recover_now(layout: &Layout, reload: Reloader<'_>) {
    let file_lock = match store::acquire_run_lock(&layout.state_root, crate::activity::now_ms()) {
        Ok(l) => l,
        Err(e) => {
            warn!(
                target: "offload",
                error = %e,
                "detection updater: skipping crash recovery; another instance holds the run lock"
            );
            return;
        }
    };
    recover_interrupted(layout, reload);
    drop(file_lock);
}

/// **Crash recovery at launch, unconditionally (#48, M-12).**
///
/// Recovery used to reach the disk from exactly one place: [`run`], which
/// [`tick_once`] calls only when [`updates_enabled`] resolves true AND a
/// component is `check`/`auto` AND [`is_due`] says so. Every one of those is a
/// question about *fetching updates*, and none of them is a question about
/// *whether the rule set on disk is complete*.
///
/// So the failure was: a crash mid-swap leaves `rules.d` short; the user — quite
/// reasonably, having just seen the app die — switches detection off, or sets
/// the component to `off`, or simply is not due for another 23 hours. The
/// journal then sits there and `rules.d` stays short across every restart,
/// which is the silent permanent degradation decision 13 exists to forbid,
/// reached by a switch that has nothing to do with it. "Never degrade to no
/// rules" must not be conditional on an unrelated preference.
///
/// Called from [`detection::init`](crate::offload::detection::init), before the first
/// [`signature::reload`](super::signature::reload), so the set that compiles at
/// startup is the repaired one. Takes no `Settings` **by construction** — there
/// is no switch it could consult and no way for a future edit to gate it on one
/// without changing this signature.
pub fn recover_on_launch() {
    let Some(layout) = Layout::resolve() else {
        return;
    };
    // An unlocked peek first, so the overwhelmingly common case — no journal —
    // costs one failed `read_to_string` and writes NOTHING. Taking the lock
    // straight away would create `detection-updates/` on every launch of every
    // install, including one with detection switched off, which would quietly
    // spend the module header's "inert when off" promise on a repair that is
    // almost never needed.
    //
    // Not a race: this only decides whether to bother. If a peer wrote the
    // journal a moment ago we take the lock and act; if a peer cleared it, the
    // authoritative re-read inside `recover_interrupted` (under the lock) finds
    // nothing and returns. The unsafe direction — acting on a stale read — is
    // the one the lock covers.
    // `has_journal`, not `read_journal`: the latter deletes what it cannot
    // parse, and `write_journal` is a plain `fs::write`, so an unlocked reader
    // can catch a peer's journal half-written and destroy the record of a swap
    // that is in flight.
    if !store::has_journal(&layout.state_root) {
        return;
    }
    recover_now(&layout, &live_reload);
}

/// The label recorded for the bundle that shipped with the app — the one
/// version that has no manifest entry. Displayed in Settings as the revert
/// target, so it has to read as something a user recognizes.
pub const SHIPPED_VERSION: &str = "(shipped)";

/// Put archived files back where they came from, and report the ones that
/// would not go (#48, M-11).
///
/// The half of a rollback that is safe at **any** point of a swap, because it
/// only ever writes files the archive already holds — and therefore idempotent,
/// which is what makes [`store::Phase::Restoring`] recoverable by simply
/// running it again.
///
/// **The return value is the whole of M-11.** This used to be `-> ()` with a
/// `warn!` per failure, and every caller then reported "the previous version
/// was restored" verbatim. Nothing downstream could contradict it: the
/// post-rollback health check compiles what IS on disk, and a file that is
/// absent contributes no compile error, no `files_failed`, and no missing
/// `rules` beyond the ones it carried — so `Status::healthy` came back true
/// about a rule set that had silently lost a file. The one outcome that
/// permanently reduces coverage had the most reassuring message in the module.
///
/// A failed restore leaves the file in the archive, which is why it is
/// recoverable at all: see [`settle_restore`] for what is done with this list.
#[must_use]
fn restore_archived(archived: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let mut unrestored = Vec::new();
    for (from, to) in archived {
        if let Err(e) = store::move_file(from, to) {
            warn!(
                target: "offload",
                from = %from.display(),
                to = %to.display(),
                error = %e,
                "detection updater: could not restore a previous file"
            );
            unrestored.push(
                to.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }
    unrestored
}

/// Record what a restore attempt achieved, and leave the disk in a state the
/// next run can finish (#48, M-10 + M-11).
///
/// # Loud and degraded, not escalated
///
/// A restore that could not put every file back leaves the layer running on a
/// short rule set. The tempting "escalate" — refuse to run the updater, or
/// disarm the layer until a human intervenes — is exactly backwards: it would
/// trade a *partial* rule set for *no* rule set, which is the one thing
/// decision 13 forbids, over a condition whose overwhelmingly likely cause
/// (a sharing violation from AV real-time scanning, or a file held open through
/// the panel's own *Open rules folder* button) clears by itself within minutes.
///
/// So: loud, degraded, and repaired automatically.
///
/// - **Durable.** The `Restoring` journal is left on disk, so the missing files
///   stay in the archive and every later run — and, since M-12, every launch —
///   retries the move. The retry is [`restore_archived`] itself, which is
///   idempotent.
/// - **Visible.** The names land in
///   [`store::ComponentState::unrestored_files`], which Settings renders and
///   `detection.rules_incomplete.v1` cards. The rule set really is short, and
///   the user is the only one who can unlock a locked file.
/// - **Honest.** The returned sentence is appended to the caller's own message,
///   so no path can say "the previous version was restored" full stop while
///   this is non-empty.
///
/// Empty on the ordinary path: the journal is cleared, the state field is
/// cleared, and the returned note is the empty string, so every existing
/// message is unchanged.
#[must_use]
fn settle_restore(root: &Path, c: Component, unrestored: &[String]) -> String {
    update_state_at(root, |s| {
        s.get_mut(c).unrestored_files = unrestored.to_vec();
    });
    if unrestored.is_empty() {
        store::clear_journal(root);
        return String::new();
    }
    warn!(
        target: "offload",
        component = c.as_str(),
        files = %unrestored.join(", "),
        "detection updater: a rollback could not put every file back; the live set is short of \
         them and the journal is kept so the next run retries"
    );
    format!(
        " — but {} file(s) could not be put back ({}), so the live set is running SHORT of them; \
         they are still in the retained copy and every later check (and the next launch) retries \
         the restore",
        unrestored.len(),
        unrestored.join(", ")
    )
}

/// Remove whatever is at `dest` for this component, put the archive back, and
/// reload. The undo for a failure **after** the staged set started landing.
///
/// Returns the note the caller appends to its own error: a rollback that put
/// the files back but could not recompile them is a second, separate problem,
/// and it used to be visible only as a `warn!` in a log nobody was reading
/// (#48). Empty on the ordinary path, so the existing messages are unchanged.
///
/// # The journal moves to `Restoring` between the two halves (#48, M-10)
///
/// The destructive half runs while the journal still reads `Moving`, which is
/// the correct undo for a kill inside it: the destination holds staged files
/// and the archive holds the complete outgoing set, so "clear the destination,
/// restore the archive" is right whether or not the delete loop finished.
///
/// The moment the destination is clear, that stops being true — from here on
/// the destination holds RESTORED files, and `Moving`'s recovery would delete
/// them and then restore only whatever remained in the archive. That is M-10:
/// a crash mid-rollback destroyed the difference, permanently, and reported
/// "the previous version was restored". So the phase is advanced first, and
/// `Restoring`'s recovery never deletes anything.
#[must_use]
pub(super) fn roll_back(
    c: Component,
    root: &Path,
    archive: &Path,
    dest: &Path,
    archived: &[(PathBuf, PathBuf)],
    reload: Reloader<'_>,
) -> String {
    for p in store::managed_files(dest, c) {
        let _ = std::fs::remove_file(&p);
    }
    journal(root, c, store::Phase::Restoring, archive, dest);
    let unrestored = restore_archived(archived);
    let debt = settle_restore(root, c, &unrestored);
    format!("{}{debt}", reload_note(c, dest, reload))
}

/// The archive loop's undo: restore only, never clear the destination (see
/// [`activate`]), with the same debt handling as [`roll_back`].
///
/// No phase change is needed on the way in — the journal already reads
/// `Archiving`, whose recovery is restore-only too, so a kill anywhere in here
/// recovers to exactly the place this is heading.
#[must_use]
pub(super) fn restore_only(
    c: Component,
    root: &Path,
    dest: &Path,
    archived: &[(PathBuf, PathBuf)],
    reload: Reloader<'_>,
) -> String {
    let unrestored = restore_archived(archived);
    let debt = settle_restore(root, c, &unrestored);
    format!("{}{debt}", reload_note(c, dest, reload))
}

/// Reload after a rollback and turn a failure into a sentence the caller can
/// append. Warned as well as returned: the return reaches the Advisor card and
/// the activity row, the log reaches whoever is diagnosing the machine.
#[must_use]
fn reload_note(c: Component, dest: &Path, reload: Reloader<'_>) -> String {
    match reload(c, dest) {
        Ok(_) => String::new(),
        Err(e) => {
            warn!(
                target: "offload",
                component = c.as_str(),
                error = %e,
                "detection updater: the restored version did not reload cleanly"
            );
            format!(" — but the restored version did not reload cleanly either ({e})")
        }
    }
}

/// Record the outcome, write the row, and return it.
///
/// One funnel, so no path can produce an outcome without also producing its
/// consumers: the state record, the activity row, and (through the state) the
/// Advisor card.
pub(super) fn finish(
    c: Component,
    root: &Path,
    now_ms: u64,
    outcome: Outcome,
    version: String,
    detail: String,
) -> RunResult {
    update_state_at(root, |s| {
        let cs: &mut ComponentState = s.get_mut(c);
        cs.last_check_ms = now_ms;
        cs.last_outcome = detail.clone();
        cs.last_ok = outcome.ok();
        cs.last_outcome_kind = outcome.as_str().to_string();
        // Did this outcome prove the channel answered? An exhaustive match and
        // not `!= Unavailable` (#48): the negated form silently counted the two
        // REVERT outcomes as proof of reachability, so a user on a permanently
        // blocked proxy could zero a six-check streak by clicking Revert, and
        // clicking it weekly would suppress the stall card indefinitely. A
        // revert reaches nothing. Stated as a match so a future outcome has to
        // answer the question rather than inherit an answer.
        match outcome {
            // The channel produced a document — being told "no" proves it
            // answered just as well as being told "here it is".
            Outcome::UpToDate | Outcome::Available | Outcome::Applied | Outcome::Rejected => {
                cs.unreachable_streak = 0;
            }
            Outcome::Unavailable => {
                cs.unreachable_streak = cs.unreachable_streak.saturating_add(1);
            }
            // Local file work. It touches no network in either direction, so it
            // is neither evidence of reachability nor of silence.
            Outcome::Reverted | Outcome::RevertFailed => {}
        }
        // Did this outcome leave the component FRESHER? The canary decision 13
        // asks for, and the one that survives a dismissed failure card (#48).
        match outcome {
            // The only two outcomes that prove the installed data IS the
            // currently published data.
            Outcome::Applied | Outcome::UpToDate => cs.stale_streak = 0,
            // Everything else is a check that came and went with the component
            // no fresher than before — refused, unreachable, or offered
            // something it did not take.
            Outcome::Available | Outcome::Rejected | Outcome::Unavailable => {
                cs.stale_streak = cs.stale_streak.saturating_add(1);
            }
            // A revert is not a check. It says nothing about what is published,
            // so it neither confirms freshness nor counts against it.
            Outcome::Reverted | Outcome::RevertFailed => {}
        }
        match outcome {
            Outcome::Available => {
                cs.available_version = version.clone();
            }
            // A successful apply, a clean check or a revert clears both the
            // pending offer and the failure record: the condition each card
            // reports is over, and a card outliving its condition is worse
            // than no card.
            Outcome::Applied | Outcome::UpToDate | Outcome::Reverted => {
                cs.available_version.clear();
                cs.available_notes.clear();
                cs.last_failure.clear();
                cs.last_failure_version.clear();
                cs.last_failure_signature.clear();
            }
            Outcome::Rejected => {
                cs.last_failure = detail.clone();
                cs.last_failure_version = version.clone();
                cs.last_failure_signature = failure_signature(&version, &detail);
                // The offer stands: the user may still want to retry, and the
                // Settings row should keep saying which version was refused.
                //
                // Only when there IS one. A manifest-level refusal carries no
                // version, and writing that empty string into the offer slot
                // silently withdrew a legitimate pending offer (#48). The slot
                // holds a version some manifest actually offered; a refusal
                // that never got as far as a version has nothing to say about
                // it either way.
                if !version.is_empty() {
                    cs.available_version = version.clone();
                }
            }
            // Nothing was checked, and a revert checked nothing either, so
            // nothing recorded about the DATA changes: a standing offer stays
            // offered and a standing refusal stays refused, because a check
            // that never happened resolves neither. The counters above are the
            // only things that move.
            Outcome::Unavailable | Outcome::RevertFailed => {}
        }
    });
    record_row(c, outcome, &version, &detail);
    if outcome == Outcome::Rejected {
        warn!(
            target: "offload",
            component = c.as_str(),
            version = %version,
            detail = %detail,
            "detection updater: bundle rejected; the previous data is still active"
        );
    } else if outcome == Outcome::Unavailable {
        // Logged, not carded (#46). `info` and not `warn`: on a machine that is
        // simply offline this is the expected result of every check, and a WARN
        // per component per day would train the log to be ignored.
        info!(
            target: "offload",
            component = c.as_str(),
            streak = state_at(root).get(c).unreachable_streak,
            detail = %detail,
            "detection updater: update channel unreachable; the current data stays live"
        );
    } else if outcome == Outcome::RevertFailed {
        // A user action that did not do what it said, so `warn` — but nothing
        // about the bundle or the channel is implied, which is the whole reason
        // this is not `Rejected` (#48).
        warn!(
            target: "offload",
            component = c.as_str(),
            detail = %detail,
            "detection updater: revert did not complete; the current data is unchanged"
        );
    } else {
        info!(
            target: "offload",
            component = c.as_str(),
            outcome = outcome.as_str(),
            version = %version,
            "detection updater: check complete"
        );
    }
    RunResult {
        component: c,
        outcome,
        version,
        detail,
    }
}
