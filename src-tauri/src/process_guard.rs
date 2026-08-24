//! Windows Job Object backstop for cImp-spawned child processes.
//!
//! Every offload child (`llama-server`, the warm MCP-host servers, and
//! `run_command` subprocesses) is spawned with `kill_on_drop(true)`, and the
//! graceful `CloseRequested` path explicitly kills them. Both of those only
//! fire when cImp exits *cleanly* — i.e. when Rust destructors run. They do
//! **nothing** when the process dies hard:
//!
//! * `panic = "abort"` (the release profile) terminates instantly, no Drop;
//! * a crash / OOM kill / `taskkill /F` bypasses Drop;
//! * `cargo tauri dev`'s hot-reload `TerminateProcess`-es the app to rebuild.
//!
//! Those are exactly the paths that left orphan `llama-server` processes
//! holding VRAM across dev cycles. A Job Object closes the gap at the OS
//! level: a job created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` terminates
//! every process assigned to it the instant the *last* handle to the job
//! closes. cImp holds that handle for its whole lifetime (we deliberately
//! never close it), so when cImp dies for ANY reason the kernel reaps the
//! whole group. This is the mechanism Chrome / VS Code use, and the only
//! backstop that survives a hard kill.
//!
//! The job is created lazily on first child and shared process-wide. On
//! non-Windows targets every entry point is a no-op.
//!
//! **Two entry points, because there are two kinds of child.** [`guard_child`]
//! takes a `tokio::process::Child` and reads its handle directly; [`guard_pid`]
//! (V33 contract C3) takes a raw pid, for the PTY children that come back from
//! portable-pty as a `portable_pty::Child` with no handle in that shape. Until
//! the second one existed, every AI tab — the single widest agent seam in the
//! app, per `spawn_ledger` — was outside the job entirely.

use tokio::process::Child;

/// Assign a freshly spawned child to the process-lifetime kill-on-close job so
/// the OS reaps it even if cImp dies hard. Best-effort and idempotent-safe:
/// any failure is logged and the child simply falls back to `kill_on_drop`.
/// No-op on non-Windows.
///
/// There is a microsecond race between `spawn()` and this call: if cImp died
/// in that exact window the child would still leak. That window is irrelevant
/// for the motivating cases (crashes/hot-reloads happen long after spawn); a
/// `CREATE_SUSPENDED`-then-assign-then-resume sequence would close it fully if
/// ever needed.
pub fn guard_child(child: &Child) {
    #[cfg(windows)]
    {
        if let Some(raw) = child.raw_handle() {
            imp::assign(raw, "tokio child");
        } else {
            tracing::debug!("process_guard: child has no raw handle to guard");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = child;
    }
}

/// V33 contract C3 — the pid-taking sibling of [`guard_child`], for children
/// that are **not** `tokio::process::Child`.
///
/// The PTY child (`pty::manager`, one per AI tab) comes back from
/// portable-pty's `spawn_command` as a `portable_pty::Child`, so it has no
/// `raw_handle()` in the shape [`guard_child`] wants — and until this entry
/// point existed **every AI tab was outside the job**. That is the widest hole
/// in the kill-on-close guarantee, because the tab's child *is* the agent and
/// everything the agent runs is its descendant.
///
/// Same posture as [`guard_child`]: best-effort, never a gate. A failure here
/// leaves the tab running with whatever kill discipline it already had.
///
/// # The caller MUST still hold a live handle to `pid`
///
/// Naming a process by pid is only safe while something pins the pid against
/// reuse. Windows will not recycle a pid while any handle to that process
/// remains open, and Unix will not recycle it while the child is unreaped —
/// so calling this with a pid read out of a child object the caller is still
/// holding is race-free, and calling it with a pid from anywhere else is not.
/// The consequence of getting it wrong is not a missed assignment but a
/// *stranger's* process being adopted into a kill-on-close job, so this is a
/// hard contract rather than a nicety.
///
/// # The assign-after-spawn window (documented, not hidden)
///
/// The child is already running by the time we get its pid, so there is a real
/// window — one `OpenProcess` + one `AssignProcessToJobObject`, i.e. a few
/// microseconds — in which:
///
/// * a grandchild the child spawns is created **outside** the job and stays
///   outside it (job membership is inherited at creation, and is not applied
///   retroactively to processes that already exist); and
/// * cImp dying would leave the child unreaped.
///
/// It is accepted here for two reasons. First, it is not reachable in practice:
/// the harness binaries this guards (`claude`, `opencode`, a shell) spend tens
/// to hundreds of milliseconds on their own startup before they can spawn
/// anything, which is four or more orders of magnitude wider than the window.
/// Second, the fix that closes it fully — `CREATE_SUSPENDED`, assign, then
/// `ResumeThread` — is **not reachable through portable-pty**: its
/// `CommandBuilder` exposes no process-creation flags and its ConPTY spawn path
/// builds the `STARTUPINFOEX` itself, so closing the window would mean forking
/// the PTY spawn rather than calling into it. The same window already exists on
/// the `guard_child` path and is documented there.
pub fn guard_pid(pid: u32) {
    #[cfg(windows)]
    {
        imp::assign_pid(pid);
    }
    #[cfg(not(windows))]
    {
        // No job objects outside Windows. The Unix backstop is
        // `procutil::kill_tree`'s process-group kill plus the PTY's own
        // SIGHUP-on-master-close, not this module.
        let _ = pid;
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::RawHandle;
    use std::sync::OnceLock;

    use tracing::{debug, warn};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// Holds the raw job `HANDLE` so it can live in a `static`. We never close
    /// it: it must stay open for the whole process lifetime, and the OS closing
    /// it at exit is precisely what triggers kill-on-close. `None` means job
    /// creation failed and callers fall back to `kill_on_drop`.
    struct Job(HANDLE);
    // SAFETY: a job HANDLE is a process-wide kernel handle, safe to share and
    // use from any thread; we only ever read it.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    static JOB: OnceLock<Option<Job>> = OnceLock::new();

    fn job_handle() -> Option<HANDLE> {
        JOB.get_or_init(|| create_kill_on_close_job().map(Job))
            .as_ref()
            .map(|j| j.0)
    }

    fn create_kill_on_close_job() -> Option<HANDLE> {
        // SAFETY: standard Win32 job-object setup. `CreateJobObjectW` with null
        // attributes/name creates an anonymous job; we configure the extended
        // limit info with a zeroed struct (all limits off) plus the
        // kill-on-close flag, then verify each call's BOOL/handle result.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                warn!("process_guard: CreateJobObjectW failed; children rely on kill_on_drop only");
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                warn!(
                    "process_guard: SetInformationJobObject failed; children rely on \
                     kill_on_drop only"
                );
                // Close the freshly-created job handle — we're abandoning it, so
                // leaving it open would leak a kernel handle for the process life.
                CloseHandle(handle);
                return None;
            }
            debug!("process_guard: kill-on-job-close job created");
            Some(handle)
        }
    }

    pub fn assign(raw: RawHandle, what: &str) {
        let Some(job) = job_handle() else { return };
        assign_handle(job, raw as HANDLE, what);
    }

    /// V33 C3 — assign by pid, for children whose object does not hand us a
    /// process handle (portable-pty's `Child`). Opens the process with exactly
    /// the rights `AssignProcessToJobObject` documents it needs
    /// (`PROCESS_SET_QUOTA | PROCESS_TERMINATE`) plus
    /// `PROCESS_QUERY_LIMITED_INFORMATION` for `IsProcessInJob`, assigns, and
    /// closes the handle again — the *job* handle is the one that must stay
    /// open forever, not this one.
    pub fn assign_pid(pid: u32) {
        let Some(job) = job_handle() else { return };
        // SAFETY: a plain `OpenProcess` by pid. The caller's contract
        // (`guard_pid`) is that it still holds a live handle to this process,
        // which is what makes the pid unambiguous — Windows does not recycle a
        // pid while a handle to it is open.
        let hproc = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if hproc.is_null() {
            // SAFETY: reading the calling thread's last-error code.
            let code = unsafe { GetLastError() };
            warn!(
                pid,
                error = code,
                "process_guard: OpenProcess failed; this child is OUTSIDE the kill-on-close \
                 job and will survive a hard cImp death"
            );
            return;
        }
        assign_handle(job, hproc, "pty child");
        // SAFETY: `hproc` is a handle we just opened and no longer need. Closing
        // it does NOT remove the process from the job — job membership is a
        // property of the process, not of this handle.
        unsafe {
            CloseHandle(hproc);
        }
    }

    /// The one place a process actually joins the job. Loud on failure: the
    /// caller's fallback (`kill_on_drop` / the PTY killer) only fires when cImp
    /// exits cleanly, so a failure here silently voids the only backstop that
    /// survives a hard kill.
    fn assign_handle(job: HANDLE, hproc: HANDLE, what: &str) {
        // Idempotent by design — a re-guarded child is not an error. Ask first
        // so the "already ours" case does not have to be inferred from an
        // error code.
        if in_job(hproc, job) == Some(true) {
            debug!("process_guard: {what} is already in the job");
            return;
        }
        // SAFETY: `hproc` is a live process handle opened with (or granted)
        // PROCESS_SET_QUOTA|PROCESS_TERMINATE; `job` is our process-wide job
        // handle. Windows 8+ permits a process that is already in a job to join
        // another one (nested jobs), so this works even when a dev runner or
        // terminal already placed cImp — and therefore its children — in a job.
        let ok = unsafe { AssignProcessToJobObject(job, hproc) };
        if ok != 0 {
            return;
        }
        // SAFETY: reading the calling thread's last-error code.
        let code = unsafe { GetLastError() };
        // Diagnose rather than guess. ERROR_ACCESS_DENIED (5) on a handle that
        // has the right access means the process is in a job that refuses
        // nesting — pre-Win8 semantics, or a job with
        // JOB_OBJECT_LIMIT_BREAKAWAY_OK/silent-breakaway rules that exclude us.
        // Everything else is a genuine API failure.
        let already_in_some_job = in_job(hproc, std::ptr::null_mut());
        warn!(
            error = code,
            already_in_a_job = ?already_in_some_job,
            "process_guard: AssignProcessToJobObject failed for {what}; it is OUTSIDE the \
             kill-on-close job and will survive a hard cImp death (a clean exit still reaps \
             it via kill_on_drop / the PTY killer)"
        );
    }

    /// `Some(true/false)` = the process is / is not in `job` (a null `job` asks
    /// "in *any* job"). `None` = the query itself failed, which is a different
    /// fact from "no" and is reported as such.
    fn in_job(hproc: HANDLE, job: HANDLE) -> Option<bool> {
        let mut result: windows_sys::Win32::Foundation::BOOL = 0;
        // SAFETY: `hproc` carries PROCESS_QUERY_LIMITED_INFORMATION (the right
        // `IsProcessInJob` requires); `result` is a live stack slot.
        let ok = unsafe { IsProcessInJob(hproc, job, &mut result) };
        if ok == 0 {
            None
        } else {
            Some(result != 0)
        }
    }
}

#[cfg(test)]
mod tests {
    /// V33 contract C3 — **both halves of the process-tree kill are wired at
    /// every agent-initiated spawn seam.**
    ///
    /// This is a cross-module invariant, which is why it is a test rather than
    /// a comment: the guard lives here, the group flag lives in `procutil`, and
    /// the four seams that must call them live in four unrelated modules. The
    /// bug it pins down already happened once — `guard_child` is typed to a
    /// `tokio::process::Child`, so when the PTY seam started using
    /// portable-pty's `spawn_command` it fell out of the job silently and
    /// stayed out, with nothing anywhere reporting a gap.
    ///
    /// Sources are pinned with `include_str!` (the `spawn_ledger` /
    /// `offload::agent` house pattern) so a stale checkout or a wrong cwd
    /// cannot make it pass. It is a presence check, not a call-graph proof: it
    /// asserts the wiring exists, while `spawn_ledger::LEDGER` is what asserts
    /// the set of seams is complete. Together they mean a NEW spawn fails the
    /// ledger, and an EXISTING spawn losing its guard fails this.
    #[test]
    fn every_agent_spawn_seam_is_tree_kill_covered() {
        // Assembled with `concat!` so this file does not match its own needles.
        let job_by_handle = concat!("process_guard::", "guard_child(");
        let job_by_pid = concat!("process_guard::", "guard_pid(");
        // V42 R27 re-point: the needle was `own_process_group(&mut ` before the
        // confined walk moved, and the `&mut ` never carried any meaning — the
        // function takes `&mut tokio::process::Command`, so there is no other
        // way to call it. `run_confined` already holds a `&mut` and passes it
        // straight through. Dropping the two words is a spelling fix, not a
        // looser check; `own_process_group_std` is still excluded by the paren.
        let own_group = concat!("own_process_", "group(");
        // …and `kill_tree` is what the timeout/cancel paths actually reach for.
        // `run_command` used to settle for `kill_on_drop`, which kills the one
        // process cImp holds and leaves a timed-out `cargo`/`npm` build running.
        let kill_tree = concat!("procutil::", "kill_tree(&mut ");

        // **V42 R27 — where each seam's spawn wiring LIVES.**
        //
        // `audit/runner.rs` and `checks/mod.rs` ran the confined walk line for
        // line and now delegate it to `sandbox::confine::run_confined`, so the
        // three needles below are in the DELEGATE, not in the seam. Rather than
        // exempt the two seams — which would retire the invariant for exactly
        // the seams it was written for — each row names the file that carries
        // its wiring, and a delegating seam is checked TWICE: that it still
        // delegates, and that the delegate is wired. That is strictly more than
        // the pre-R27 check, which only ever asked whether the needle was
        // somewhere in the seam's own text.
        //
        // `run_command` and the PTY seam still walk their own spawns, so their
        // wiring file is themselves and nothing about them changes.
        let delegate = concat!("confine::", "run_confined");
        let confine = ("sandbox/confine.rs", include_str!("sandbox/confine.rs"));
        let audit = ("audit/runner.rs", include_str!("audit/runner.rs"));
        let checks = ("checks/mod.rs", include_str!("checks/mod.rs"));
        let run_command = (
            "offload/tools/run_command.rs",
            include_str!("offload/tools/run_command.rs"),
        );
        let pty = ("pty/manager.rs", include_str!("pty/manager.rs"));

        // (seam file, seam source, wiring file, wiring source). `wiring == seam`
        // means the seam walks its own spawn.
        let wired: [(&str, &str, &str, &str); 4] = [
            (audit.0, audit.1, confine.0, confine.1),
            (checks.0, checks.1, confine.0, confine.1),
            (run_command.0, run_command.1, run_command.0, run_command.1),
            (pty.0, pty.1, pty.0, pty.1),
        ];

        // Half one of the delegation proof: a seam whose wiring lives elsewhere
        // must still visibly hand its child over. Without this, a seam could go
        // back to spawning raw and the needles below would keep passing on the
        // delegate's text alone.
        for (seam, src, wiring, _) in wired {
            if seam != wiring {
                assert!(
                    src.contains(delegate),
                    "{seam}'s process-tree wiring is supposed to live in {wiring}, but {seam} no \
                     longer calls `{delegate}` — so either it spawns unguarded now, or this \
                     table is stale (V33 contract C3, V42 R27)"
                );
            }
        }

        // The job object. The PTY child is not a `tokio::process::Child`; it is
        // guarded by pid instead. That asymmetry IS contract C3.
        for (seam, wiring, src, guard) in [
            (audit.0, confine.0, confine.1, job_by_handle),
            (checks.0, confine.0, confine.1, job_by_handle),
            (run_command.0, run_command.0, run_command.1, job_by_handle),
            (pty.0, pty.0, pty.1, job_by_pid),
        ] {
            assert!(
                src.contains(guard),
                "{seam} is an AgentSpawn seam (see `spawn_ledger::LEDGER`) but {wiring} no longer \
                 calls `{guard}` — its child is OUTSIDE the kill-on-job-close job and survives a \
                 hard cImp death (V33 contract C3)"
            );
        }

        // The Unix half. `pty/manager.rs` is deliberately absent: portable-pty's
        // unix backend already `setsid()`s the child, which makes it a session
        // and group leader AND gives it a controlling terminal, so closing the
        // master fd hangs up the whole session. Adding `process_group` there
        // would be a no-op at best and would fight the PTY at worst.
        for (seam, wiring, src) in [
            (audit.0, confine.0, confine.1),
            (checks.0, confine.0, confine.1),
            (run_command.0, run_command.0, run_command.1),
        ] {
            assert!(
                src.contains(own_group),
                "{seam}'s spawn no longer calls `procutil::{own_group}..)` in {wiring} — on Unix \
                 its grandchildren survive a timeout/cancel kill, because `kill_tree`'s `killpg` \
                 only reaches a child that leads its own process group (V33 contract C3)"
            );
        }

        for (seam, wiring, src) in [
            (audit.0, confine.0, confine.1),
            (checks.0, confine.0, confine.1),
            (run_command.0, run_command.0, run_command.1),
        ] {
            assert!(
                src.contains(kill_tree),
                "{seam}'s child is no longer reaped with `{kill_tree}..)` in {wiring}, so the \
                 process group / pid tree established at spawn is never signalled (V33 contract \
                 C3)"
            );
        }
    }
}
