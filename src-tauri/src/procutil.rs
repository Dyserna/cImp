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

/// Kill `child` and, on Windows, its whole process tree. `start_kill`
/// (TerminateProcess) reaches only the direct child; tools like semgrep fork
/// workers that inherit the stdio pipe write ends — left alive they keep
/// running AND prevent the capture tasks from ever seeing EOF. The
/// process_guard job object is no help here: it reaps on *cImp* exit, not on a
/// per-run kill.
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
    // Direct kill regardless — the fallback when taskkill is unavailable/failed,
    // and the whole mechanism on non-Windows.
    let _ = child.start_kill();
    let _ = child.wait().await;
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
