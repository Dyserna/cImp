//! V32 Phase C3 (locked decision 13) — the **detection auto-updater**.
//!
//! # Why this exists
//!
//! Signature rules decay without updates — they only match phrasings someone
//! has already written down — and tying freshness to manual maintenance runs
//! makes staleness the default. So the rule bundle is kept current on a daily
//! check, from a channel the project curates ([`manifest`]).
//!
//! The **classifier weights are not on this channel** and never were, in the
//! end: locked decision 7 ships them through the models-v1 release-asset
//! pipeline with `CHECKSUMS.txt`, at maintenance-run cadence, exactly like the
//! TTS and STT blobs. A `classifier` component was built here and removed on
//! 2026-08-08 — a released Meta checkpoint has no update stream to poll, and
//! two delivery mechanisms for one artifact is one too many.
//!
//! # The shape of one run
//!
//! ```text
//!   scheduler tick (due? per component)
//!        │
//!        ├─ fetch the manifest ─────────────────► parse boundary (manifest.rs)
//!        │                                        schema, names, sizes,
//!        │                                        digests, asset origin
//!        ├─ newer than installed? applicable to this app version?
//!        │        │
//!        │        ├─ mode = check-only ─► record "available" + Advisor card. STOP.
//!        │        └─ mode = auto ────────┐
//!        │                               ▼
//!        ├─ download each file into MEMORY, verify SHA-256, only then write to
//!        │  staging/ (nothing untrusted reaches disk before its digest is
//!        │  checked, and nothing reaches a parser before that either)
//!        │
//!        ├─ validate ───────────────────────────► the validate.rs gauntlet
//!        │        └─ fail ─► reject, wipe staging, keep old data, card + row
//!        │
//!        ├─ activate: archive the current files under previous/, move the
//!        │            staged files in, hot-reload
//!        │        └─ reload unhealthy ─► restore the archive, card + row
//!        │
//!        └─ record state (version, outcome) + activity row
//! ```
//!
//! # Invariants this module is responsible for
//!
//! - **`rules.d/local/` is never touched.** Structural, not conditional: the
//!   activation path only ever enumerates the top level of `rules.d`
//!   ([`store::managed_rule_files`]). Nothing here opens `local/`.
//! - **Checksum before content.** A downloaded byte is hashed before it is
//!   written to disk and long before it is compiled. A mismatch aborts the
//!   component's run with the staging directory wiped.
//! - **Old data stays live on any failure.** There is no path from "the new
//!   bundle is bad" to "no bundle": the live set changes in exactly one place,
//!   after the gauntlet passed, and is restored if the hot-reload disagrees.
//! - **Inert when off.** With the Phase G detection feature resolving off
//!   ([`updates_enabled`], which the L1 master also decides), or with
//!   both modes `off`, the scheduler tick returns before touching the network,
//!   the disk, or anything but those switches — and the three Settings buttons
//!   refuse for the same reason, through the same [`updates_enabled`] call.
//!
//!   **Inert is not the same as "detection is off" (#48, M-21).** That resolution
//!   is app-scoped and does not see the `offload-worker` row, so the updater can
//!   be inert while the worker is screening with the bundle on disk. The
//!   behaviour is deliberate; what had to be fixed was every sentence that
//!   explained it by claiming a layer was off. See [`worker_only_detection`].
//!
//!   **One deliberate exception, and it is not about updating**:
//!   [`recover_on_launch`] finishes a swap a crash interrupted, whatever the
//!   switches say (#48, M-12). Gating THAT on the updater's own settings is how
//!   a user who turned detection off after a crash stranded a short `rules.d`
//!   permanently — the repair is about the completeness of the data on disk,
//!   not about whether new data is wanted. It is still silent and writes
//!   nothing on a healthy install: an existence check on the journal file
//!   returns before the lock, the state file or the rules directory is touched.
//! - **A refusal and an outage are different events.** [`Outcome::Rejected`]
//!   means a document reached us and a check said no; [`Outcome::Unavailable`]
//!   means the channel never answered. Collapsing them made every install
//!   report a permanent bundle rejection for a release that simply did not
//!   exist yet (#46), which is how a security-relevant card stops being read.
//!
//! # Every signal has its consumer
//!
//! Every outcome writes an `injection_flag` Tool Activity row (screen
//! `updater`, `ok` reflecting the outcome), and the three that need a decision
//! reach the Advisor: `detection.update_available.v1` (check-only found
//! something newer), `detection.update_failed.v1` (a bundle was refused) and
//! `detection.update_stalled.v1` ([`STALLED_AFTER_CHECKS`] consecutive checks
//! left the component no fresher, for ANY reason — the "this has stopped
//! getting fresher" signal, and the only one whose dismissal ages, so a
//! component cannot be frozen silently by dismissing the other two).
//! Versions, last-check time and outcome live in
//! Settings → Injection protection → Injection detection, next to Check now,
//! Apply, Revert and Open rules folder.
//!
//! # Testability
//!
//! Every path is driven through a [`Layout`] (four directories) and a
//! [`manifest::Fetcher`], so the whole pipeline — checksum mismatch, broken
//! bundle, false-positive smoke failure, successful swap, revert — runs against
//! a temp directory and an in-memory map with **no network and no writes
//! outside that directory**.

pub mod manifest;
pub mod store;
pub mod validate;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use tracing::{info, warn};

use manifest::{Component, Fetcher, Manifest};
use store::{ComponentState, State};

use crate::activity::{ActivityEntry, ActivityKind, ActivityRecord};
use crate::offload::outbound::Screen;
use crate::settings::Settings;

// ── The seven section files ────────────────────────────────────────────────
//
// V42 R20 (#126): this file was 2,719 lines carrying nine hand-marked
// sections. Seven of them moved out VERBATIM, one file each, in the shape
// `offload/loopback/` already uses — `mod x;` beside a glob that re-exports
// it at the width that family actually needs. What stayed is the module
// docs above and the module's public face below: the entry points
// (`run_live`/`revert_live`/`check_now`) and the scheduler that drives them.
//
// The globs are load-bearing twice over. Outward, `updater::status`,
// `updater::Mode` and the other twenty-odd names `detection`, `ipc` and the
// advisor already import keep resolving. Inward, `tests.rs` — the one offload
// test module that was ALREADY external — opens with `use super::*`, so the
// 2,358 lines of end-to-end cases reach the moved internals through exactly
// the same path they did when everything lived here.
//
// Each glob is written at the width its family actually needs, as
// `loopback/mod.rs` does: `state` is the one section with no consumer
// outside `updater` at all, so its cache and run lock stay in here.
mod schedule;
mod layout;
mod state;
mod status;
mod outcome;
mod signals;
mod apply;

pub use self::schedule::*;
pub use self::layout::*;
use self::state::*;
pub use self::status::*;
pub use self::outcome::*;
pub use self::signals::*;
pub use self::apply::*;

// The three names the section files reach one level further up, into
// `detection` itself. Re-exported here so their `super::signature::…` /
// `super::Config` spellings resolve unchanged from one module deeper — which
// is what let the seven files move without a single edited reference.
pub(super) use super::{note_rules_reloaded, signature, Config};

// ── The entry points ───────────────────────────────────────────────────────

/// Fetch the manifest and run every component in `components`.
///
/// `force_auto` is what Settings' "Apply" passes: it overrides the configured
/// mode for this one run, so an explicit click applies the update without the
/// user having to flip a setting and wait for a tick. It never overrides
/// `Off` — a component the user turned off stays off, including against a
/// button press meant for the other one.
pub async fn run(
    components: &[Component],
    sched: Schedule,
    manifest_url: &str,
    force_auto: bool,
    fetcher: &dyn Fetcher,
    layout: &Layout,
    reload: Reloader<'_>,
) -> Vec<RunResult> {
    // One run at a time, process-wide. A scheduler tick and a "Check now" click
    // otherwise share `staging/<component>/`, and the loser's `wipe_dir` would
    // delete the bundle the winner had just validated. Serialized rather than
    // skipped: a click that has to wait for a tick still does what the user
    // asked, whereas a click that silently no-ops does not.
    let _run_guard = run_lock().lock().await;
    // …and one run at a time across PROCESSES, which the mutex above cannot
    // reach (#48, M-14). Nothing is recorded on contention: the peer holding
    // the lock is doing this same work against the same directories, so a
    // state write here would be a second opinion about a run in flight.
    let _file_guard = match store::acquire_run_lock(&layout.state_root, crate::activity::now_ms()) {
        Ok(l) => l,
        Err(e) => {
            warn!(
                target: "offload",
                error = %e,
                "detection updater: skipping this run; another instance holds the run lock"
            );
            return Vec::new();
        }
    };
    // Finish any swap a crash interrupted before this run can wipe the archive
    // it would have been recovered from (#48, U-2).
    recover_interrupted(layout, reload);
    let root = layout.state_root.clone();
    // Validate the manifest URL BEFORE fetching it (#48, U-1). The parse
    // boundary in `manifest.rs` already refuses a plaintext or unusable
    // channel, but it only runs on the response — by which time the document
    // whose SHA-256s gate every artifact has already travelled in the clear.
    // `detection_update_manifest_url` is a user-editable setting and the only
    // other validation site, so this is where an unusable override stops.
    //
    // `Rejected` rather than `Unavailable`: nothing was unreachable, a check
    // refused to run, and the person who typed the override is exactly who the
    // card is for. The pinned default always passes, so this is silent on
    // every install that has not set one.
    if let Err(e) = manifest::AssetAnchor::parse(manifest_url) {
        return fail_all(components, sched, &root, Outcome::Rejected, &e);
    }
    let raw = match fetcher.get(manifest_url, manifest::MAX_MANIFEST_BYTES).await {
        Ok(b) => b,
        // Transport. Nothing was refused; the channel did not answer (#46).
        //
        // Both `FetchErrorKind`s land here, deliberately, and this is the one
        // place the artifact split (#48, M-9) does NOT apply. An artifact's
        // ceiling is a size the manifest itself declares, so exceeding it is a
        // document contradicting its own index; the manifest's ceiling is a
        // blanket sanity bound, and the thing most likely to exceed it is
        // precisely what #46 is about — a proxy login page or a GitHub 404,
        // neither of which is anybody publishing anything.
        Err(e) => {
            return fail_all(
                components,
                sched,
                &root,
                Outcome::Unavailable,
                &e.to_string(),
            )
        }
    };
    let text = match String::from_utf8(raw) {
        Ok(t) => t,
        Err(_) => {
            return fail_all(
                components,
                sched,
                &root,
                Outcome::Unavailable,
                "the response was not valid UTF-8, so it is not the manifest",
            )
        }
    };
    let man = match Manifest::parse(&text, manifest_url) {
        Ok(m) => m,
        Err(e) => {
            // The one place the two failure classes are told apart. A body that
            // is not even shaped like our index means nobody is publishing here
            // (a 404 page, a proxy login, a tag that does not exist yet); a body
            // that IS shaped like it and still fails is our document being
            // refused by a parse invariant — schema, containment, file names.
            let outcome = if manifest::looks_like_manifest(&text) {
                Outcome::Rejected
            } else {
                Outcome::Unavailable
            };
            return fail_all(components, sched, &root, outcome, &e);
        }
    };

    let mut out = Vec::new();
    for c in components {
        let configured = sched.mode(*c);
        if configured == Mode::Off {
            continue;
        }
        let effective = if force_auto { Mode::Auto } else { configured };
        let result = run_component(*c, &man, effective, fetcher, layout, reload).await;
        if result.outcome == Outcome::Available {
            // The curator's note belongs to the offer, so it is recorded only
            // on the path that creates one. Remote text: stored and displayed,
            // never interpreted.
            if let Some(notes) = man.components.get(c).and_then(|e| e.notes.clone()) {
                update_state_at(&root, |s| s.get_mut(*c).available_notes = notes);
            }
        }
        out.push(result);
    }
    out
}

/// The production wrapper: resolve the layout, use the HTTP fetcher and the
/// live reloader.
pub async fn run_live(
    components: &[Component],
    settings: &Settings,
    force_auto: bool,
) -> Vec<RunResult> {
    let Some(layout) = Layout::resolve() else {
        warn!(target: "offload", "detection updater: no usable layout; skipping");
        return Vec::new();
    };
    run(
        components,
        Schedule::from_settings(settings),
        &manifest_url(settings),
        force_auto,
        &manifest::HttpFetcher,
        &layout,
        &live_reload,
    )
    .await
}

/// A manifest-level failure is every enabled component's failure: none of them
/// could be checked, and a silent no-op would be indistinguishable from
/// "everything is current".
///
/// `outcome` is the caller's classification — [`Outcome::Unavailable`] when the
/// channel did not produce our index at all, [`Outcome::Rejected`] when it did
/// and a parse invariant refused it. The distinction is the whole of #46, so it
/// is an argument here rather than a guess inside.
fn fail_all(
    components: &[Component],
    sched: Schedule,
    root: &Path,
    outcome: Outcome,
    reason: &str,
) -> Vec<RunResult> {
    let now = crate::activity::now_ms();
    let detail = match outcome {
        // Deliberately does NOT open with "could not reach the update channel"
        // (#48): Settings renders this line under exactly that label, and the
        // stored detail repeating it made every unavailable check read
        // "Could not reach the update channel: could not reach the update
        // channel: …". The label belongs to the surface; the detail is the
        // reason plus what it cost, which is nothing.
        Outcome::Unavailable => format!(
            "{reason}. Nothing was checked and nothing changed; the current detection data is \
             still live."
        ),
        _ => format!("update check failed: {reason}"),
    };
    components
        .iter()
        .filter(|c| sched.mode(**c) != Mode::Off)
        .map(|c| finish(*c, root, now, outcome, String::new(), detail.clone()))
        .collect()
}

/// Restore a component's previous version — the Settings Revert button.
///
/// Symmetric with activation: the files being replaced are archived under the
/// version they represent, so a revert is itself revertible.
pub fn revert(c: Component, layout: &Layout, reload: Reloader<'_>) -> RunResult {
    let now = crate::activity::now_ms();
    let root = layout.state_root.clone();
    // A revert rewrites the same two directories a run does, so it needs the
    // same cross-process exclusion (#48, M-14). `RevertFailed`, not `Rejected`:
    // nothing was fetched and nothing about the DATA is being recorded — the
    // user pressed a button at the wrong moment and should press it again.
    let _file_guard = match store::acquire_run_lock(&root, now) {
        Ok(l) => l,
        Err(e) => {
            return finish(
                c,
                &root,
                now,
                Outcome::RevertFailed,
                String::new(),
                format!("revert failed: {e}; nothing was changed — try again in a moment"),
            )
        }
    };
    // Same reason as in `run`: a revert also rewrites an archive directory, so
    // an interrupted swap has to be finished first (#48, U-2).
    recover_interrupted(layout, reload);
    let st = state_at(&root);
    let cs = st.get(c);
    let previous_version = cs.previous_version.clone();
    let current_version = cs.installed_version.clone();
    if previous_version.is_empty() {
        // `RevertFailed`, not `Rejected` (#48): nothing was fetched, so nothing
        // was refused. As `Rejected` this raised a card claiming a bundle
        // refusal AND wrote `String::new()` into the offer slot, withdrawing a
        // legitimate pending offer — two lies for one benign click.
        return finish(
            c,
            &root,
            now,
            Outcome::RevertFailed,
            String::new(),
            format!("nothing to revert to for `{}`", c.as_str()),
        );
    }
    match revert_inner(c, layout, &previous_version, &current_version, reload) {
        Ok(detail) => finish(
            c,
            &root,
            now,
            Outcome::Reverted,
            previous_version,
            detail,
        ),
        // The version rides along for the activity row's "revert-failed
        // <version>" label only: `RevertFailed` records nothing about the data,
        // which is what keeps the PREVIOUS version out of the offer slot — as
        // `Rejected` it landed there and Settings advertised a downgrade as
        // "a newer bundle is available" (#48).
        Err(e) => finish(
            c,
            &root,
            now,
            Outcome::RevertFailed,
            previous_version,
            format!("revert failed: {e}"),
        ),
    }
}

/// The production wrapper for [`revert`].
///
/// **Must be called from a blocking context** (`spawn_blocking`, which is how
/// the `detection_revert` IPC command already invokes it) — it takes the same
/// [`run_lock`] a scheduler tick holds, and `blocking_lock` panics if called on
/// a runtime thread. The pure [`revert`] is the one the tests drive; it takes no
/// lock, because a test owns its own tree.
pub fn revert_live(c: Component) -> RunResult {
    let _run_guard = run_lock().blocking_lock();
    match Layout::resolve() {
        Some(layout) => revert(c, &layout, &live_reload),
        None => RunResult {
            component: c,
            outcome: Outcome::RevertFailed,
            version: String::new(),
            detail: "revert failed: no usable layout".to_string(),
        },
    }
}

fn revert_inner(
    c: Component,
    layout: &Layout,
    previous_version: &str,
    current_version: &str,
    reload: Reloader<'_>,
) -> Result<String, String> {
    let dest = layout.dest(c);
    let root = &layout.state_root;
    let archive = store::previous_dir(root, c, previous_version);
    // Where the currently-live set will be archived, so this revert is itself
    // revertible.
    let keep = store::previous_dir(root, c, current_version);
    // #48, U-4: a revert is judged by the same post-swap health check, so a
    // broken `rules.d/local/` file would veto it too — and the user pressing
    // Revert is *already* trying to get out of a bad state. Same baseline rule
    // as `validate_and_activate_rules`: pre-existing `local/` failures are
    // forgiven, anything the restore introduces is not. Inert for the
    // classifier, whose reloader never reports a `local/` failure.
    let baseline = LocalBaseline::snapshot(dest);
    let judged = |c: Component, dir: &Path| match reload(c, dir) {
        Ok(live) => Ok(live),
        Err(why) if c == Component::Rules => baseline.forgive(dir, why),
        Err(why) => Err(why),
    };
    let reload: Reloader<'_> = &judged;

    // **Revert must never wipe its own source (#48, U-2).**
    //
    // `store::sanitize_version` is lossy — every character outside
    // `[A-Za-z0-9._-]` becomes `_` and the result is trimmed — so two different
    // version strings can name one directory. The reachable case is not exotic:
    // on a fresh install `outgoing_version` is the literal `(shipped)`, which
    // sanitizes to `shipped`, so a manifest publishing a rules version of
    // `shipped` makes `keep` and `archive` THE SAME PATH. The `wipe_dir(&keep)`
    // below would then delete the very files being restored, the live directory
    // would be emptied into a directory that no longer exists, and a second
    // Revert — still enabled, because the state write never happened — would
    // destroy the surviving copy.
    //
    // Compared as PATHS, not as strings, because the collision is created by
    // the sanitizer and only the sanitized form can see it. Fail closed:
    // refusing a revert costs the user a click and a message, emptying
    // `rules.d` costs them the layer.
    if keep == archive {
        return Err(format!(
            "the retained `{previous_version}` and the installed `{current_version}` are archived \
             under the same directory (`{}`), so restoring one would destroy the other — refusing \
             rather than risking an empty rules directory. Re-publish the bundle under a version \
             that does not collide, or reinstall from \
             Settings → Injection protection → Injection detection (Check now)",
            archive.display()
        ));
    }

    let restoring = store::managed_files(&archive, c);
    if restoring.is_empty() {
        return Err(format!(
            "the retained `{previous_version}` version is empty or missing from {}",
            archive.display()
        ));
    }
    // Archive what is live now under ITS version, so this revert can be undone
    // — keeping anything the live set is missing, for the same reason
    // `activate` does (#48, M-11): those files are the current version's own
    // and this is the only copy of them.
    let mut archived: Vec<(PathBuf, PathBuf)> = prepare_archive(c, &keep, dest);
    // The same two-loop shape as `activate`, and therefore the same undos and
    // the same journal (#48, U-2): a bare `?` in either loop left the live
    // directory holding a subset with nothing put back, and this path is the
    // one a user triggers by hand.
    journal(root, c, store::Phase::Archiving, &keep, dest);
    for p in store::managed_files(dest, c) {
        let name = p.file_name().unwrap_or_default().to_os_string();
        let to = keep.join(&name);
        if let Err(e) = store::move_file(&p, &to) {
            let note = restore_only(c, root, dest, &archived, reload);
            return Err(format!(
                "archiving the current `{current_version}` files failed ({e}); nothing was \
                 restored and the current version is still live{note}"
            ));
        }
        archived.push((to, p.clone()));
    }
    journal(root, c, store::Phase::Moving, &keep, dest);
    for p in &restoring {
        let name = p.file_name().unwrap_or_default().to_os_string();
        if let Err(e) = store::move_file(p, &dest.join(&name)) {
            let note = roll_back(c, root, &keep, dest, &archived, reload);
            return Err(format!(
                "restoring `{previous_version}` failed ({e}); `{current_version}` was put \
                 back{note}"
            ));
        }
    }
    let live = match reload(c, dest) {
        Ok(live) => live,
        Err(why) => {
            let note = roll_back(c, root, &keep, dest, &archived, reload);
            return Err(format!(
                "the restored `{previous_version}` version did not load cleanly ({why}); \
                 `{current_version}` was put back{note}"
            ));
        }
    };
    store::clear_journal(root);
    update_state_at(root, |s| {
        let cs = s.get_mut(c);
        cs.installed_version = previous_version.to_string();
        cs.previous_version = current_version.to_string();
        // Same reasoning as `activate`'s success path: the live set has been
        // rewritten whole from a complete retained copy, so nothing is owed.
        cs.unrestored_files.clear();
    });
    Ok(format!(
        "reverted `{}` to `{previous_version}` ({live}); `{current_version}` is retained and can \
         be restored the same way",
        c.as_str()
    ))
}

// ── The scheduler ──────────────────────────────────────────────────────────

/// Spawn the background scheduler: a debounced launch check plus a periodic
/// due-ness poll.
///
/// Follows the app's existing background-task shape (a `tauri::async_runtime`
/// task around a `tokio::time::interval`, as `state::manager` and the loopback
/// heartbeats use) rather than introducing a scheduling framework. Settings are
/// re-read on every tick, so a switch, mode or interval change takes effect
/// within one [`POLL_TICK`] with no restart and no broadcast subscription to
/// keep in sync — which is why the task is still spawned unconditionally even
/// though [`tick_once`] may decline to do anything.
pub fn spawn_scheduler(settings: crate::settings::SettingsHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(LAUNCH_DELAY).await;
        let mut tick = tokio::time::interval(POLL_TICK);
        // `Delay`, not `Burst`: a machine waking from sleep must not fire every
        // missed tick at once against a release asset.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick_once(&settings).await;
            tick.tick().await;
        }
    });
}

/// One scheduler pass. Separated so the inertness property is visible in one
/// place: with [`updates_enabled`] false, or with both components `off`, this
/// returns before reading the state file and long before touching the network.
async fn tick_once(settings: &crate::settings::SettingsHandle) {
    let snap = settings.current();
    if !updates_enabled(&snap) {
        return;
    }
    let sched = Schedule::from_settings(&snap);
    if sched.is_inert() {
        return;
    }
    let st = state();
    let now = crate::activity::now_ms();
    let due: Vec<Component> = Component::ALL
        .iter()
        .copied()
        .filter(|c| {
            is_due(
                sched.mode(*c),
                now,
                st.get(*c).last_check_ms,
                sched.interval_hours,
            )
        })
        .collect();
    if due.is_empty() {
        return;
    }
    info!(
        target: "offload",
        components = %due.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(","),
        "detection updater: scheduled check"
    );
    run_live(&due, &snap, false).await;
}

#[cfg(test)]
mod tests;
