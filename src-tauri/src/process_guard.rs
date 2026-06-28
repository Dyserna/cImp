//! Windows Job Object backstop for ccImp-spawned child processes.
//!
//! Every offload child (`llama-server`, the warm MCP-host servers, and
//! `run_command` subprocesses) is spawned with `kill_on_drop(true)`, and the
//! graceful `CloseRequested` path explicitly kills them. Both of those only
//! fire when ccImp exits *cleanly* — i.e. when Rust destructors run. They do
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
//! closes. ccImp holds that handle for its whole lifetime (we deliberately
//! never close it), so when ccImp dies for ANY reason the kernel reaps the
//! whole group. This is the mechanism Chrome / VS Code use, and the only
//! backstop that survives a hard kill.
//!
//! The job is created lazily on first child and shared process-wide. On
//! non-Windows targets every entry point is a no-op.

use tokio::process::Child;

/// Assign a freshly spawned child to the process-lifetime kill-on-close job so
/// the OS reaps it even if ccImp dies hard. Best-effort and idempotent-safe:
/// any failure is logged and the child simply falls back to `kill_on_drop`.
/// No-op on non-Windows.
///
/// There is a microsecond race between `spawn()` and this call: if ccImp died
/// in that exact window the child would still leak. That window is irrelevant
/// for the motivating cases (crashes/hot-reloads happen long after spawn); a
/// `CREATE_SUSPENDED`-then-assign-then-resume sequence would close it fully if
/// ever needed.
pub fn guard_child(child: &Child) {
    #[cfg(windows)]
    {
        if let Some(raw) = child.raw_handle() {
            imp::assign(raw);
        } else {
            tracing::debug!("process_guard: child has no raw handle to guard");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = child;
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::RawHandle;
    use std::sync::OnceLock;

    use tracing::{debug, warn};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
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
                return None;
            }
            debug!("process_guard: kill-on-job-close job created");
            Some(handle)
        }
    }

    pub fn assign(raw: RawHandle) {
        let Some(job) = job_handle() else { return };
        // SAFETY: `raw` is a live process handle from a just-spawned child;
        // `job` is our process-wide job handle. Adding an already-assigned
        // process is harmless. Windows 8+ permits nested jobs, so this works
        // even if a dev runner/terminal already placed ccImp in a job.
        let ok = unsafe { AssignProcessToJobObject(job, raw as HANDLE) };
        if ok == 0 {
            warn!("process_guard: AssignProcessToJobObject failed; child relies on kill_on_drop");
        }
    }
}
