//! Shared child-process **capture utilities** for orchestrators that spawn
//! external tools and read their piped output: the V22 `checks` runner and the
//! V23 audit runner both consume these (they keep their own spawn/orchestration
//! contracts — shell-wrapped vs direct-exec, timeout-only vs cancel-token — but
//! the fiddly capture/kill/drain core lives here exactly once).

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

/// `CREATE_NO_WINDOW` — the process-creation flag every spawned subprocess in
/// cImp sets so Windows doesn't flash a console window per child. One named
/// definition instead of a bare hex literal per spawn site.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long to wait for a capture task to reach EOF once its child has exited
/// or been killed. Normally instant (the pipe's buffered remainder); bounded so
/// a surviving grandchild that inherited the write end can't hang the caller
/// forever.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Read `reader` to EOF, retaining at most `cap` bytes but continuing to drain
/// (and discard) the rest so the child never blocks on a full pipe. `None` (a
/// stream that wasn't piped) yields an empty string. Lossy UTF-8 — tool output
/// is text, and a stray invalid byte shouldn't drop the run. Returns the kept
/// text plus whether anything beyond `cap` was dropped — a truncated stream
/// must not be parsed as a complete document (e.g. SARIF).
pub async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, cap: usize) -> (String, bool) {
    let mut bytes = Vec::new();
    let mut truncated = false;
    if let Some(mut reader) = reader {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bytes.len() < cap {
                        let take = n.min(cap - bytes.len());
                        bytes.extend_from_slice(&chunk[..take]);
                        if take < n {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
            }
        }
    }
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

/// V33 contract C3 — the Unix half of the tree kill, applied at **spawn** time.
///
/// Windows can reap a tree after the fact (`taskkill /T`, and the
/// [`crate::process_guard`] job object for the hard-death case). Unix has
/// neither: `start_kill` is `kill(pid, SIGKILL)` and reaches the direct child
/// only, so a checker or scanner that forked workers leaves them running —
/// still burning CPU, and still holding the stdio pipe write ends, which is
/// what makes [`drain_capture`] time out instead of EOF-ing. Before this
/// existed, `docs/completedMilestones/MILESTONE-linux-support.md` tracked it as
/// an accepted Linux-only orphan hazard.
///
/// The Unix answer has to be set up before the child exists: make the child its
/// own **process-group leader** (`setpgid(0, 0)`, which is what
/// `process_group(0)` compiles to) so that later a single `killpg` reaps it and
/// every descendant that did not deliberately leave the group. Call this on
/// every agent-initiated spawn whose kill path is [`kill_tree`].
///
/// No-op on Windows — job/tree membership there is inherited automatically and
/// `process_group` is a Unix-only API.
///
/// The PTY seam deliberately does **not** call this: portable-pty's unix
/// backend already `setsid()`s the child (`portable-pty/src/unix.rs`), which
/// makes it a session AND group leader, and gives it a controlling terminal on
/// top — so closing the master fd hangs up the whole session for free.
pub fn own_process_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // `0` = "use the child's own pid as the new group id", i.e. the child
        // becomes the leader of a brand-new group containing only it (and,
        // by inheritance, everything it goes on to spawn).
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Kill `child` and its whole process tree. `start_kill` (TerminateProcess /
/// `kill(pid)`) reaches only the direct child; tools like semgrep fork workers
/// that inherit the stdio pipe write ends — left alive they keep running AND
/// prevent the capture tasks from ever seeing EOF. The process_guard job object
/// is no help here: it reaps on *cImp* exit, not on a per-run kill.
///
/// Two mechanisms, one per platform: `taskkill /T /F` walks the Windows parent
/// pid chain, and `killpg` signals the Unix process group that
/// [`own_process_group`] established at spawn. Both are best-effort and are
/// followed by the direct kill regardless.
pub async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let mut cmd = tokio::process::Command::new("taskkill");
        cmd.args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd.creation_flags(CREATE_NO_WINDOW);
        if let Ok(mut tk) = cmd.spawn() {
            let _ = tk.wait().await;
        }
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        kill_process_group(pid);
    }
    // Direct kill regardless — the fallback when taskkill/killpg was
    // unavailable or the child was never group-detached.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// The blocking sibling of [`kill_tree`], for the one caller that has no async
/// runtime: the V35 Phase D live probe (`harness/probe.rs`), which runs from
/// `cimp --harness-canary` before any Tauri/tokio init, exactly like the hook
/// shims.
///
/// Same two mechanisms and the same reason — `opencode serve` is a Bun binary
/// that forks children (observed: two grandchildren per server), so a bare
/// `Child::kill` leaves a live HTTP server bound to the probe's loopback port
/// after the probe has exited. Kept here rather than in `probe.rs` so the
/// "how cImp reaps a tree" idiom has exactly one home.
///
/// The Unix leg is `killpg` via the same guarded [`kill_process_group`], so the
/// blocking spawn site must set its own process group the way
/// [`own_process_group`] does for `tokio::process` — [`own_process_group_std`]
/// is that, for `std::process::Command`.
pub fn kill_tree_blocking(child: &mut std::process::Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        if let Ok(mut tk) = cmd.spawn() {
            let _ = tk.wait();
        }
    }
    #[cfg(unix)]
    kill_process_group(pid);
    #[cfg(not(any(windows, unix)))]
    let _ = pid;
    // Direct kill regardless — same fallback contract as `kill_tree`.
    let _ = child.kill();
    let _ = child.wait();
}

/// [`own_process_group`] for a blocking `std::process::Command`. Same contract,
/// same no-op on Windows; separate only because the two `Command` types share
/// no trait.
pub fn own_process_group_std(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// `killpg` the group `pid` leads — but **only** if it actually leads one.
///
/// Why the check is not paranoia. `killpg` takes a process-GROUP id, and
/// passing a pid that is not a group id is not merely useless: if it ever named
/// a group cImp did not create, this would signal a set of processes chosen by
/// accident. The guard makes that impossible rather than improbable:
///
/// * `getpgid(pid) == pid` is true exactly when the child is its own group
///   leader, which is exactly what [`own_process_group`] arranges. If a caller
///   forgot to call it, we skip and fall back to the direct kill.
/// * The pid itself is unambiguous because the caller holds an unreaped
///   `Child`: Unix does not recycle a pid until it is waited on, and
///   [`kill_tree`] runs before its own `wait()`. (`Child::id()` already returns
///   `None` once tokio has reaped it.)
///
/// `SIGKILL`, not `SIGTERM`: this is the Unix counterpart of `taskkill /F`, and
/// its callers reach it only after a timeout or an explicit cancel — the point
/// at which a process that has not exited has already declined to.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let pid = pid as libc::pid_t;
    // SAFETY: `getpgid` is a pure query on a pid the caller still holds
    // unreaped; it returns -1 on failure, which fails the equality below.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid != pid {
        tracing::debug!(
            pid,
            pgid,
            "kill_tree: child is not its own process-group leader; killing it directly only \
             (its descendants, if any, will survive — the spawn site should call \
             `own_process_group`)"
        );
        return;
    }
    // SAFETY: `pgid` is a process-group id we just proved the child leads.
    let rc = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if rc != 0 {
        tracing::debug!(
            pgid,
            error = %std::io::Error::last_os_error(),
            "kill_tree: killpg failed; falling back to the direct kill"
        );
    }
}

/// Await a [`read_capped`] capture task, bounded by [`DRAIN_TIMEOUT`]. If the
/// pipe never EOFs (a surviving grandchild still holds the write end) give up
/// rather than hang forever; the lost output counts as truncated.
pub async fn drain_capture(mut task: tokio::task::JoinHandle<(String, bool)>) -> (String, bool) {
    match tokio::time::timeout(DRAIN_TIMEOUT, &mut task).await {
        Ok(res) => res.unwrap_or_default(),
        Err(_) => {
            task.abort();
            (String::new(), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output past the cap sets the truncated flag (and only then).
    #[tokio::test]
    async fn read_capped_flags_truncation() {
        let (text, truncated) = read_capped(Some(&b"hello world"[..]), 5).await;
        assert_eq!(text, "hello");
        assert!(truncated);

        let (text, truncated) = read_capped(Some(&b"hello"[..]), 5).await;
        assert_eq!(text, "hello");
        assert!(!truncated);

        // An unpiped stream is empty, not truncated.
        let (text, truncated) = read_capped(None::<&[u8]>, 5).await;
        assert_eq!(text, "");
        assert!(!truncated);
    }
}
