//! **The one confined spawn** — the walk around the OS boundary that every
//! non-PTY agent seam makes, written once.
//!
//! # Why this module exists (V42 R27)
//!
//! [`crate::audit::runner::spawn_and_capture`] and [`crate::checks`]'s
//! `spawn_capture` ran the SAME eleven steps in the same order, line for line,
//! around the Landlock/AppContainer boundary:
//!
//! `CREATE_NO_WINDOW` → [`own_process_group`](crate::procutil::own_process_group)
//! → `Plan::Sandboxed(prepared).apply(..)` → [`spawn_gate::spawn_tokio`] →
//! [`process_guard::guard_child`] → [`record_sandboxed`](super::record_sandboxed)
//! → two [`read_capped`](crate::procutil::read_capped) pumps → wait →
//! [`kill_tree`](crate::procutil::kill_tree) →
//! [`drain_capture`](crate::procutil::drain_capture) → the Linux
//! [`denial_signature`](super::denial_signature) /
//! [`record_denial`](super::record_denial) pair.
//!
//! Two copies of a security boundary is one copy too many: a fix applied to one
//! and forgotten on the other is not a hypothetical, it is the failure mode the
//! duplication guarantees. The payoff of this module is not fewer lines — it is
//! that there is now ONE place to review and ONE place to fix.
//!
//! # Amendment: the walk is no longer step-for-step the audit original
//!
//! The extraction (#127) claimed, and was reviewed as, an identical step set.
//! It is identical no longer, in exactly one step and deliberately: the
//! `kill_tree` fires on EVERY abnormal outcome, where both originals fired it
//! on cancel and timeout only. See [`needs_kill`] for what the missing case
//! leaked. This is the first fix that lands in one place instead of two —
//! which is the payoff above, being collected.
//!
//! # What it does NOT own
//!
//! Deliberately narrow. The unified walk starts at the process-creation flags
//! and ends at the denial row; everything above and below stays with the seam:
//!
//! * **The plan.** [`super::plan`], the `PREPARE_BACKSTOP` wedge row, the
//!   `required` refusal and the `record_skip` on a [`Plan::Plain`] arm all read
//!   the seam's own posture and mint the seam's own wording. The caller resolves
//!   the plan and hands it here already decided.
//! * **The Windows AppContainer twin.** `Plan::Sandboxed` on Windows never
//!   reaches this walk: it is a bespoke `CreateProcessW` through
//!   [`super::windows::spawn_and_capture`], with no spawn gate of its own to
//!   take, no `read_capped` pumps and no `kill_tree` — a different mechanism,
//!   not a different spelling of this one. Each seam keeps its own twin.
//! * **The `Command` itself.** The caller chooses the program, the argv (or the
//!   `cmd.exe` tail), and the working directory. This module only sets what the
//!   walk below it depends on: the pipes it drains, the console suppression, the
//!   process group its `kill_tree` reaps, and the forced environment `apply`
//!   composes over.
//! * **Outcome mapping.** [`ConfinedOutcome`] reports what happened in the
//!   boundary's own terms; the audit seam maps it onto its `Outcome` and the
//!   checks seam onto its `(exit_code, …, timed_out)` tuple plus its own error
//!   wording. Every error variant here carries the RAW error string precisely so
//!   the two seams can keep their own messages.
//!
//! # The divergences the unification had to express, not erase
//!
//! * **The wait.** The audit seam is cancellable mid-flight (an audit scan has a
//!   user-visible stop button); a check run is not. [`Confined::cancel`] is
//!   `Option`, and the two arms below are the two original waits verbatim.
//! * **The output cap.** 16 MiB for a scanner whose stdout IS its SARIF report,
//!   1 MiB for a checker whose output is diagnostics to parse. A parameter
//!   ([`Confined::cap`]), not a compromise value.
//! * **The wait ERROR.** `child.wait()` failing is not a spawn failure and not
//!   an exit; the audit seam drained the pumps anyway and returned the partial
//!   output, the checks seam returned early and left two reader tasks detached.
//!   Unified on the draining answer — the caller's error text is unchanged, and
//!   nothing is left running behind it.

use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{Plan, SandboxCfg};

/// **Everything the boundary needs that the `Command` does not carry.**
///
/// One struct rather than eleven arguments, for the reason
/// [`crate::audit::runner`]'s `RunCtx` exists: a boundary walk whose inputs are
/// positional is a boundary walk where two of them can be swapped silently.
/// Every field is required and none has a default — the seam already resolved
/// all of them.
///
/// # Why the `dead_code` allowance is `cfg`'d rather than blanket
///
/// Seven of these fields are read only inside this module's
/// `#[cfg(target_os = "linux")]` blocks — the Landlock hook and the two row
/// mints ARE the Linux engine, and on Windows the confined arm never reaches
/// this walk at all (it is a bespoke `CreateProcessW`; see the module doc). So a
/// Windows build sees them as never-read, and a blanket `allow(dead_code)` would
/// be the easy answer — and would also switch the check off on the one platform
/// where it still means something. Allowing it only where the fields are
/// genuinely unreachable keeps the Linux build as the enforcing view: a field
/// that stops being read THERE is still an error, and Linux CI is where
/// `CIMP_EXPECT_LANDLOCK` already makes this module's skips self-enforcing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Confined<'a> {
    /// The resolved plan for this spawn. `Plan::Sandboxed` here means the
    /// LINUX engine (the Windows one never reaches this walk — see the module
    /// doc); `Plan::Plain` means the boundary is off or unavailable and was
    /// already recorded as such by the caller.
    pub plan: &'a Plan,
    /// The seam label every `sandbox` Events row this walk writes is filed
    /// under (`audit:semgrep`, `run_check`).
    pub seam: &'a str,
    /// The project root — the sandbox's granted area and the root every row is
    /// filed under. NOT necessarily the child's cwd: a check may run in a
    /// directory beneath it.
    pub root: &'a Path,
    /// The row subject. The seam's choice, and the seams disagree on purpose —
    /// the audit seam names the PROGRAM (one row per scanner), the checks seam
    /// names the CHECK (the program is `cmd.exe` for every one of them).
    pub subject: &'a str,
    /// argv as a denial row should show it. Again the seam's choice: real argv
    /// for the audit seam, the single check command line for `run_check`.
    pub argv: &'a [String],
    /// The EFFECTIVE sandbox config — already narrowed by the caller for a tool
    /// that declared itself unsandboxable. Read for `allow_network` and for the
    /// posture the rows render.
    pub sandbox: &'a SandboxCfg,
    /// The C2 minimal environment base (empty when the sandbox is off), which
    /// `apply` composes the child's environment from under locked decision L4.
    pub base_env: &'a [(&'a str, std::ffi::OsString)],
    /// The seam's forced variables. Applied to the plain command AND handed to
    /// `apply` as the overlay, because a confined child's environment is
    /// composed from scratch (`confine` clears it) rather than inherited.
    pub env: &'a [(String, String)],
    /// Per-stream capture cap. 16 MiB where stdout is the report, 1 MiB where
    /// it is diagnostics — see the module doc.
    pub cap: usize,
    /// The child's wall-clock budget.
    pub timeout: Duration,
    /// The seam's cancel token, where the seam has one. `None` is not "cannot
    /// be stopped" — a dropped future still kills the child through
    /// `kill_on_drop` — it is "this seam has no mid-flight stop".
    pub cancel: Option<&'a CancellationToken>,
}

/// How the confined child's life ended, in the boundary's terms.
///
/// The three failure variants carry the RAW error string. Neither seam's user
/// sees this enum: each maps it onto its own error type with its own wording,
/// and unifying that wording would have changed two user-visible messages for
/// no gain.
#[derive(Debug)]
pub enum ConfinedOutcome {
    /// The child ran to completion. `None` where the platform reports a signal
    /// death rather than a code.
    Exited(Option<i32>),
    /// The budget expired; the tree was killed.
    TimedOut,
    /// The seam's token fired; the tree was killed. Only reachable when
    /// [`Confined::cancel`] is `Some`.
    Cancelled,
    /// `Prepared::apply` refused BEFORE the spawn (Linux). Decision D3: a
    /// boundary that cannot be installed refuses the child rather than running
    /// it unconfined, so nothing was spawned and both streams are empty.
    ///
    /// Constructed on Linux only; matched everywhere, because a seam that
    /// handles it on one platform and not the other is a seam that forgets.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ApplyRefused(String),
    /// The spawn gate / the OS refused to start the child. Nothing ran.
    SpawnFailed(String),
    /// `child.wait()` itself failed — the child may have run and may have
    /// printed something, so the tree is killed (see [`needs_kill`]) and the
    /// pumps are still drained, and whatever they captured is returned
    /// alongside this.
    WaitFailed(String),
}

/// What the walk captured, whatever the outcome.
#[derive(Debug)]
pub struct ConfinedRun {
    pub stdout: String,
    /// The stdout pump hit [`Confined::cap`], or its drain leaked. Either way
    /// the capture is INCOMPLETE — a stdout-transport scanner must refuse to
    /// read a half report as a clean bill.
    pub stdout_truncated: bool,
    pub stderr: String,
    pub outcome: ConfinedOutcome,
}

impl ConfinedRun {
    /// The shape every pre-spawn refusal returns: nothing ran, so there is
    /// nothing to report but why.
    fn nothing_ran(outcome: ConfinedOutcome) -> Self {
        Self {
            stdout: String::new(),
            stdout_truncated: false,
            stderr: String::new(),
            outcome,
        }
    }
}

/// Whether an outcome leaves a process tree that has to be reaped before the
/// pumps are drained.
///
/// **Every abnormal outcome does** — which is a DELIBERATE HARDENING over the
/// audit original this walk was extracted from (V42 #127). That copy killed on
/// cancel and on timeout only, so a `WaitFailed` — `child.wait()` itself
/// erroring — left the child and everything it forked alive, holding the pipe
/// write ends the two drains are reading. The drains are bounded, so the seam
/// does not hang; what it does instead is return `DRAIN_TIMEOUT` late with a
/// half capture, and leak a process tree that goes on working. `kill_on_drop`
/// is not the answer either: it is a backstop that fires when this future is
/// dropped, and this future is not dropped — it returns.
///
/// The two variants that mean NOTHING WAS SPAWNED cannot reach the caller
/// below (both return before a child exists), and are `false` here because
/// "there is no tree" is the honest answer for them, not because they are
/// unreachable. The match is exhaustive on purpose: a new outcome variant is a
/// compile error here rather than a silently unreaped tree.
fn needs_kill(outcome: &ConfinedOutcome) -> bool {
    match outcome {
        ConfinedOutcome::Exited(_) => false,
        ConfinedOutcome::TimedOut | ConfinedOutcome::Cancelled | ConfinedOutcome::WaitFailed(_) => {
            true
        }
        ConfinedOutcome::ApplyRefused(_) | ConfinedOutcome::SpawnFailed(_) => false,
    }
}

/// **Spawn `cmd` inside the boundary `c.plan` decided on, and capture it.**
///
/// The caller has set the program, its arguments and its working directory;
/// everything else the walk depends on is set here, in the order the two
/// original seams set it — the environment before `apply`, because a confined
/// child's environment is composed from scratch and must win.
///
/// Never panics and never returns early past the drains once a child exists: a
/// killed child still yields what it printed, which is the "parse partial
/// output" half of the timeout contract both seams promise their callers.
///
/// # The two callers, and where they differ
///
/// `audit::runner::spawn_and_capture` passes `cap` = 16 MiB and a
/// `Some(cancel)`: a stdout-transport scanner's stdout IS its SARIF report, and
/// an audit scan has a stop button. `checks::spawn_capture` passes `cap` = 1 MiB
/// and `None`: a checker's output is diagnostics to parse, and a check run is
/// bounded by its own timeout. Both asymmetries are parameters rather than a
/// compromise value — the caps in particular differ by sixteen times, and a
/// single constant here would either truncate reports or let a runaway checker
/// buffer sixteen megabytes of noise.
pub async fn run_confined(cmd: &mut tokio::process::Command, c: Confined<'_>) -> ConfinedRun {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Kill the child if this future is dropped, so an aborted caller never
        // leaks an orphaned process.
        .kill_on_drop(true);
    // Force the seam's env onto the child (values redacted in `Debug`). On the
    // Linux confined arm `apply` below clears and recomposes it — deliberately,
    // and these values go back on as its overlay.
    for (k, v) in c.env {
        cmd.env(k, v);
    }
    // Don't flash a console window per spawned child on Windows — the same
    // CREATE_NO_WINDOW convention as every other spawned subprocess.
    #[cfg(windows)]
    cmd.creation_flags(crate::procutil::CREATE_NO_WINDOW);
    // V33 C3: Unix-only — own process group, so the cancel/timeout `kill_tree`
    // below reaps the child's forked workers the same way `taskkill /T` does on
    // Windows. A shell seam needs this most: the process cImp holds is `sh`, and
    // everything the check actually runs is its child.
    crate::procutil::own_process_group(cmd);

    // V33 Phase D — on Linux this IS the sandboxed path: Landlock is applied to
    // the command built above rather than through a second spawn mechanism.
    // Locked decision L4 is enforced by `apply`: the C2 minimal base, then the
    // seam's forced variables, then the sandbox's redirections last. A failure
    // REFUSES the child (decision D3) rather than running it with the boundary
    // quietly missing.
    #[cfg(target_os = "linux")]
    if let Plan::Sandboxed(prepared) = c.plan {
        if let Err(e) = prepared.apply(
            cmd,
            c.base_env,
            c.env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        ) {
            return ConfinedRun::nothing_ran(ConfinedOutcome::ApplyRefused(e));
        }
    }

    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let mut child = match crate::spawn_gate::spawn_tokio(cmd) {
        Ok(c) => c,
        Err(e) => return ConfinedRun::nothing_ran(ConfinedOutcome::SpawnFailed(e.to_string())),
    };
    // Backstop reaper if cImp dies hard before `kill_on_drop` fires.
    crate::process_guard::guard_child(&child);
    // The confirmation row, once per subject per session.
    #[cfg(target_os = "linux")]
    if matches!(c.plan, Plan::Sandboxed(_)) {
        super::record_sandboxed(c.seam, c.root, c.subject, c.sandbox);
    }

    // Drain stdout/stderr on their own tasks so the buffers survive a kill on
    // the wait below — killing the child only closes its pipes (which cleanly
    // EOFs these readers), it doesn't discard what was already captured.
    let out_task = tokio::spawn(crate::procutil::read_capped(child.stdout.take(), c.cap));
    let err_task = tokio::spawn(crate::procutil::read_capped(child.stderr.take(), c.cap));

    // The one divergence the two seams genuinely had. A seam WITH a token races
    // it against the budget and the child; a seam without one is the same race
    // with the cancel arm removed, which is what `tokio::time::timeout` is.
    // Only the `child.wait()` arm borrows `child`, so `kill_tree` below is free
    // to use it once the wait resolves.
    let outcome = match c.cancel {
        Some(cancel) => {
            let sleep = tokio::time::sleep(c.timeout);
            tokio::pin!(sleep);
            tokio::select! {
                _ = cancel.cancelled() => ConfinedOutcome::Cancelled,
                _ = &mut sleep => ConfinedOutcome::TimedOut,
                res = child.wait() => match res {
                    Ok(status) => ConfinedOutcome::Exited(status.code()),
                    Err(e) => ConfinedOutcome::WaitFailed(e.to_string()),
                },
            }
        }
        None => match tokio::time::timeout(c.timeout, child.wait()).await {
            Ok(Ok(status)) => ConfinedOutcome::Exited(status.code()),
            Ok(Err(e)) => ConfinedOutcome::WaitFailed(e.to_string()),
            Err(_) => ConfinedOutcome::TimedOut,
        },
    };
    if needs_kill(&outcome) {
        // Whole-tree kill: forked workers must not survive holding the pipe
        // write ends (they'd keep working and stall the drains). `kill_on_drop`
        // is a backstop, not a guarantee the process is gone by the time the
        // buffers below are read.
        crate::procutil::kill_tree(&mut child).await;
    }

    // Bounded (`procutil::DRAIN_TIMEOUT`): a grandchild still holding a pipe
    // write end must not hang the seam forever.
    let (stdout, stdout_truncated) = crate::procutil::drain_capture(out_task).await;
    let (stderr, _) = crate::procutil::drain_capture(err_task).await;

    // V33 Phase D — the Linux denial row, minted where the raw exit code and
    // stderr still exist. Only for a child that actually RAN, inside the
    // boundary: a cancel, a timeout and a failed wait are not access-denial
    // signatures, and guessing would put noise in the one lane that is supposed
    // to mean something.
    #[cfg(target_os = "linux")]
    {
        let confined_exit = match &outcome {
            ConfinedOutcome::Exited(code) => matches!(c.plan, Plan::Sandboxed(_)).then_some(*code),
            _ => None,
        };
        let class = confined_exit
            .and_then(|code| super::denial_signature(code, &stderr, c.sandbox.allow_network));
        if let Some(class) = class {
            super::record_denial(
                c.seam,
                c.root,
                c.subject,
                c.argv,
                confined_exit.flatten(),
                &stderr,
                class,
                c.sandbox,
            );
        }
    }

    ConfinedRun {
        stdout,
        stdout_truncated,
        stderr,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SkipReason;

    /// A shell that prints `text` and exits 0 — the portable way to hand the
    /// walk a child with real output on both platforms this crate builds for.
    fn echoing(text: &str) -> tokio::process::Command {
        #[cfg(windows)]
        {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", &format!("echo {text}")]);
            c
        }
        #[cfg(not(windows))]
        {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", &format!("echo {text}")]);
            c
        }
    }

    /// A portable long-running child, the same shape the audit suite's
    /// timeout/cancel tests use.
    fn sleeper() -> tokio::process::Command {
        #[cfg(windows)]
        {
            let mut c = tokio::process::Command::new("ping");
            c.args(["-n", "60", "127.0.0.1"]);
            c
        }
        #[cfg(not(windows))]
        {
            let mut c = tokio::process::Command::new("sleep");
            c.arg("60");
            c
        }
    }

    /// Everything a test does not care about, filled in once. Deliberately
    /// UNsandboxed: these assert the walk's own contract, and routing them
    /// through a real boundary would ACL-stamp the developer's toolchain dirs
    /// as a side effect of running the suite (the `run_command` precedent the
    /// audit suite's own timeout test cites).
    fn confined<'a>(
        plan: &'a Plan,
        sandbox: &'a SandboxCfg,
        root: &'a Path,
        cap: usize,
        timeout: Duration,
        cancel: Option<&'a CancellationToken>,
    ) -> Confined<'a> {
        Confined {
            plan,
            seam: "test:confine",
            root,
            subject: "test-subject",
            argv: &[],
            sandbox,
            base_env: &[],
            env: &[],
            cap,
            timeout,
            cancel,
        }
    }

    /// **The walk runs a plain child and reports its exit and its output.**
    ///
    /// The floor under both seams: before any of the boundary's own behaviour
    /// matters, `run_confined` has to be a correct spawn-and-capture.
    #[tokio::test]
    async fn a_plain_child_exits_and_its_output_is_captured() {
        let plan = Plan::Plain(SkipReason::OffUser);
        let cfg = SandboxCfg::disabled();
        let root = std::env::temp_dir();
        let mut cmd = echoing("hello-from-the-walk");
        let run = run_confined(
            &mut cmd,
            confined(
                &plan,
                &cfg,
                &root,
                64 * 1024,
                Duration::from_secs(30),
                None,
            ),
        )
        .await;
        assert!(
            matches!(run.outcome, ConfinedOutcome::Exited(Some(0))),
            "expected a clean exit, got {:?}",
            run.outcome
        );
        assert!(
            run.stdout.contains("hello-from-the-walk"),
            "stdout was {:?}",
            run.stdout
        );
        assert!(!run.stdout_truncated, "64 KiB is not a truncating cap here");
    }

    /// **The CALLER's cap is the cap the pumps use** (V42 R27, divergence D2).
    ///
    /// The two seams differ by sixteen times here — 16 MiB for a scanner whose
    /// stdout IS its SARIF report, 1 MiB for a checker's diagnostics — so the
    /// cap had to become a parameter rather than a shared constant. A parameter
    /// that quietly stopped being wired to [`crate::procutil::read_capped`]
    /// would look exactly like a working extraction right up until a scanner's
    /// report came back truncated at 1 MiB, or a runaway checker buffered
    /// sixteen megabytes of noise. Asserted at a cap far below any plausible
    /// default, so nothing can pass this by accident.
    #[tokio::test]
    async fn the_callers_cap_is_what_truncates_the_capture() {
        let plan = Plan::Plain(SkipReason::OffUser);
        let cfg = SandboxCfg::disabled();
        let root = std::env::temp_dir();
        let mut cmd = echoing(&"x".repeat(400));
        let run = run_confined(
            &mut cmd,
            confined(&plan, &cfg, &root, 16, Duration::from_secs(30), None),
        )
        .await;
        assert!(
            run.stdout_truncated,
            "a 400-byte child under a 16-byte cap must report truncation"
        );
        assert!(
            run.stdout.len() <= 16,
            "the capture must not exceed the caller's cap; got {} bytes",
            run.stdout.len()
        );
    }

    /// **A cancel token stops the child, and outranks the budget.**
    ///
    /// The audit seam is the only caller that passes one, and this pins the arm
    /// itself rather than that seam's mapping of it — the budget here is far
    /// beyond the cancel, so a `Cancelled` outcome cannot be a timeout wearing
    /// the wrong name.
    #[tokio::test]
    async fn a_cancel_token_stops_the_child_and_outranks_the_timeout() {
        let plan = Plan::Plain(SkipReason::OffUser);
        let cfg = SandboxCfg::disabled();
        let root = std::env::temp_dir();
        let mut cmd = sleeper();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let started = std::time::Instant::now();
        let run = run_confined(
            &mut cmd,
            confined(
                &plan,
                &cfg,
                &root,
                64 * 1024,
                Duration::from_secs(120),
                Some(&cancel),
            ),
        )
        .await;
        assert!(
            matches!(run.outcome, ConfinedOutcome::Cancelled),
            "expected Cancelled, got {:?}",
            run.outcome
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the child was not killed on cancel (took {:?})",
            started.elapsed()
        );
    }

    /// **Without a token, the budget is the only stop** — the `checks` arm of
    /// the one divergence this module exists to express.
    #[tokio::test]
    async fn without_a_token_the_timeout_still_kills_the_child() {
        let plan = Plan::Plain(SkipReason::OffUser);
        let cfg = SandboxCfg::disabled();
        let root = std::env::temp_dir();
        let mut cmd = sleeper();
        let started = std::time::Instant::now();
        let run = run_confined(
            &mut cmd,
            confined(
                &plan,
                &cfg,
                &root,
                64 * 1024,
                Duration::from_millis(300),
                None,
            ),
        )
        .await;
        assert!(
            matches!(run.outcome, ConfinedOutcome::TimedOut),
            "expected TimedOut, got {:?}",
            run.outcome
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the child was not killed on timeout (took {:?})",
            started.elapsed()
        );
    }

    /// **A program that does not exist is a spawn failure, not an exit** — and
    /// it carries the RAW error, which is the whole reason
    /// [`ConfinedOutcome`] keeps its three failure variants apart instead of
    /// wording them here.
    #[tokio::test]
    async fn a_missing_program_is_a_spawn_failure_carrying_the_raw_error() {
        let plan = Plan::Plain(SkipReason::OffUser);
        let cfg = SandboxCfg::disabled();
        let root = std::env::temp_dir();
        let mut cmd = tokio::process::Command::new("cimp-no-such-program-exists-here");
        let run = run_confined(
            &mut cmd,
            confined(
                &plan,
                &cfg,
                &root,
                64 * 1024,
                Duration::from_secs(30),
                None,
            ),
        )
        .await;
        match run.outcome {
            ConfinedOutcome::SpawnFailed(e) => {
                assert!(!e.is_empty(), "the raw spawn error was dropped")
            }
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
        assert!(run.stdout.is_empty() && run.stderr.is_empty());
        assert!(!run.stdout_truncated);
    }

    /// **Every abnormal outcome reaps the tree.**
    ///
    /// A `WaitFailed` used to be the hole: it left the child and its forked
    /// workers alive holding the pipe write ends, so the bounded drains came
    /// back late with a half capture and the tree went on working. There is no
    /// portable way to MAKE `child.wait()` fail, so the rule is pinned where it
    /// is decided — one exhaustive predicate, asserted variant by variant, so
    /// that a new outcome cannot be added without answering the question.
    ///
    /// The two "nothing was spawned" variants answer `false` because there is
    /// no tree, not because they are unreachable here.
    #[test]
    fn every_abnormal_outcome_reaps_the_tree() {
        assert!(!needs_kill(&ConfinedOutcome::Exited(Some(0))));
        assert!(!needs_kill(&ConfinedOutcome::Exited(None)));
        assert!(needs_kill(&ConfinedOutcome::TimedOut));
        assert!(needs_kill(&ConfinedOutcome::Cancelled));
        assert!(needs_kill(&ConfinedOutcome::WaitFailed("boom".into())));
        assert!(!needs_kill(&ConfinedOutcome::SpawnFailed("boom".into())));
        assert!(!needs_kill(&ConfinedOutcome::ApplyRefused("boom".into())));
    }
}
