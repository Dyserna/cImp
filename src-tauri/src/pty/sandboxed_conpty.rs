//! V33 **Phase B** — a ConPTY spawned into the AppContainer.
//!
//! This is the Win32 half of sandboxed AI tabs; the policy half (which switches,
//! which grants, which environment) is [`crate::sandbox::tabs`]. It exists at
//! all because **`portable_pty` cannot carry the sandbox and has no escape
//! hatch** — verified against the pinned 0.9.0 source and written up in
//! `docs/reviews/SPIKE-S3-conpty-appcontainer-2026-08-18.md`:
//!
//! * `win::psuedocon` / `win::procthreadattr` are private modules, so the
//!   `HPCON` is unreachable from outside the crate;
//! * `ProcThreadAttributeList::with_capacity(1)` is hardcoded to ONE attribute
//!   and `set_pty` is its only setter — there is no slot for
//!   `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`;
//! * `ConPtyMasterPty` exposes nothing beyond the `MasterPty` trait.
//!
//! But the crate's **traits** are public, so the blast radius is this file plus
//! a branch in `pty::manager`: everything downstream of the spawn — the reader,
//! the writer, `resize`, `process_id` → `guard_pid`, the killer, the waiter —
//! is trait-level and does not know which backend it got.
//!
//! # The mechanism, in one paragraph
//!
//! Two pipes; `CreatePseudoConsole` over the stdin-read and stdout-write ends;
//! one `STARTUPINFOEXW` attribute list holding **both**
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and
//! `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` (spike S3 confirmed the list
//! simply grows to two entries and that the update order is irrelevant);
//! `CreateProcessW` with `EXTENDED_STARTUPINFO_PRESENT`,
//! `CREATE_UNICODE_ENVIRONMENT`, **`bInheritHandles = FALSE`** and
//! `STARTF_USESTDHANDLES` with `INVALID_HANDLE_VALUE` in all three std slots.
//! The pty is how the child gets its console; the std slots are explicitly
//! invalidated so a redirected parent handle cannot leak into it.
//!
//! # Two things that look like bugs and are not
//!
//! **1. `bInheritHandles = FALSE`, so this spawn takes the gate SHARED, not
//! exclusively.** `sandbox::windows::spawn_blocking_inner` takes
//! [`crate::spawn_gate::exclusive`] because it must make three handles
//! inheritable for the duration of its `CreateProcessW`, and that flag is a
//! process-wide property. This path inherits nothing, so it has no such window
//! to protect — and the shared leg is exactly what keeps it OUT of the sandbox
//! path's exclusive window, which is the property that matters. Spike S3 § 4
//! reaches the same conclusion, and `spawn_gate`'s own tripwire enforces that
//! the exclusive holder stays unique. `CreatePseudoConsole` is wrapped too: it
//! spawns the console host process, in cImp's own context, and that is a spawn
//! whatever library performs it.
//!
//! **2. The console host is NOT sandboxed, deliberately.**
//! `CreatePseudoConsole` starts conhost as a child of the *creating* process
//! under the user's normal token (S3 § gotcha 5). The confinement boundary
//! therefore sits between conhost and the child, not around the pty as a whole.
//! That is the same trust shape as today — the pty master has always been
//! cImp's — and it is why the child's console handles work at all. Stated here
//! rather than discovered later: *a sandboxed tab's console host is an
//! unsandboxed process cImp owns, bound to that one pty.*

use std::ffi::{c_void, OsString};
use std::io::{Error as IoError, ErrorKind, Read, Result as IoResult, Write};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};

use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, DUPLICATE_SAME_ACCESS, HANDLE,
    INVALID_HANDLE_VALUE, STILL_ACTIVE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::{SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::sandbox::windows::{env_block_of, quote_arg, wide, wide_str, Prepared};

/// `PROC_THREAD_ATTRIBUTE_INPUT | 22` — `ProcThreadAttributePseudoConsole`.
/// windows-sys does not re-export it at this surface; the encoding is stable
/// Win32 ABI (`processthreadsapi.h`), and it is the same derivation
/// `sandbox::windows` uses for its two attributes.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0000 | 22;
/// `SE_GROUP_ENABLED` — declared in `Win32_System_SystemServices`, a feature
/// this crate does not enable, so it is spelled out here exactly as
/// `sandbox::windows` does for the same reason. Its value is stable Win32 ABI.
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
/// `ProcThreadAttributeSecurityCapabilities` = 9, spelled here rather than
/// imported so this file's attribute list reads as one unit.
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 0x0002_0000 | 9;
const _: () = assert!(PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE == 0x0002_0016);
const _: () = assert!(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES == 0x0002_0009);

/// `PSEUDOCONSOLE_INHERIT_CURSOR` — the flag this path must **never** set.
///
/// Named so [`CONPTY_FLAGS`] can be asserted against it. With this bit, conhost
/// emits a DSR cursor-position query and **blocks the child's startup until the
/// terminal answers**; the S3 harness hung for 30 s on exactly that, 90 bytes
/// in, ending at `ESC[6n`. `portable_pty` sets it because wezterm answers, and
/// cImp's xterm.js frontend answers too — but a bespoke path that gets this
/// wrong looks like a broken sandbox rather than a protocol mistake, which makes
/// it the single easiest way to misdiagnose Phase B.
const PSEUDOCONSOLE_INHERIT_CURSOR: u32 = 0x1;
/// `PSEUDOCONSOLE_RESIZE_QUIRK` — conhost's modern resize behaviour (it does not
/// reflow and reposition the cursor the legacy way when the pty is resized).
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;
/// `PSEUDOCONSOLE_WIN32_INPUT_MODE` — conhost additionally understands
/// win32-input-mode sequences on the input side. Additive: ordinary VT input
/// keeps working, which is what cImp's xterm.js frontend sends.
const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;

/// The ConPTY creation flags for a sandboxed tab: **exactly what the plain path
/// uses, minus the deadlock bit** (decision B7, as refined 2026-08-18).
///
/// `portable_pty` creates every non-sandboxed tab's console with
/// `INHERIT_CURSOR | RESIZE_QUIRK | WIN32_INPUT_MODE` (0x7). This path takes the
/// last two and drops the first, because:
///
/// * **[`PSEUDOCONSOLE_INHERIT_CURSOR`] is non-negotiable** — see its doc. It is
///   the one bit whose absence is a correctness requirement here, and the
///   `const` assertion plus `the_conpty_flags_never_inherit_the_cursor` are what
///   keep it pinned.
/// * **The other two are parity**, and parity is the point. A sandboxed tab that
///   reflowed differently from the same tab unsandboxed would be precisely the
///   "two paths silently disagree" failure this whole phase is shaped against
///   (the environment composition in `PtyManager::start_sandboxed` is the other
///   half of the same rule). Dropping `RESIZE_QUIRK` would have given a
///   sandboxed tab conhost's legacy re-flow on every window drag, and the user
///   would have had no way to connect that to the sandbox switch.
///
/// So the sandbox changes the boundary around the child and *nothing about how
/// its console behaves*, which is the only honest shape for a hardening layer.
const CONPTY_FLAGS: u32 = PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE;
const _: () = assert!(CONPTY_FLAGS & PSEUDOCONSOLE_INHERIT_CURSOR == 0);
/// …and the parity half, pinned too: dropping either of these silently
/// reintroduces a behavioural difference between a sandboxed and a plain tab.
const _: () = assert!(CONPTY_FLAGS == 0x6);

/// Everything one sandboxed tab spawn needs. A struct rather than a parameter
/// list for the same reason `sandbox::windows::SpawnRequest` is one: this is a
/// seven-value call where two of the values are paths and two are string lists,
/// which is where the wrong argument gets passed in the right position.
pub struct TabSpawn<'a> {
    /// The resolved harness binary. Absolute; cImp resolved it through `which`.
    pub program: &'a Path,
    /// `pre_args` followed by `extra_args`, already flattened by the caller —
    /// this backend does not know which is which and must not reorder them.
    pub args: &'a [String],
    /// The child's COMPLETE environment. Built by the caller from the very same
    /// `CommandBuilder` the plain path would spawn with (decision B4), so the
    /// two paths cannot disagree about what the child sees.
    pub env: &'a [(OsString, OsString)],
    /// The child's cwd — the mapped drive root (decision B6).
    pub cwd: &'a Path,
    pub size: PtySize,
}

/// What a successful sandboxed spawn hands back: the two trait objects
/// `pty::manager` already knows how to drive.
pub struct SandboxedPty {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

/// Open a pseudoconsole and spawn `req.program` into it, inside `prepared`'s
/// AppContainer.
///
/// Synchronous and short: two `CreatePipe`s, one `CreatePseudoConsole`, one
/// `CreateProcessW`. There is no wait, no drain and no deadline here — unlike
/// `sandbox::windows::spawn_and_capture`, whose whole body is a bounded wait,
/// this returns as soon as the child exists and the tab's normal reader/waiter
/// tasks take over. That is why it needs no backstop of its own: the wedge shape
/// those constants exist for (a blocking wait that never returns) is not present.
pub fn open_and_spawn(prepared: &Prepared, req: TabSpawn<'_>) -> Result<SandboxedPty, String> {
    // ── pipes ──
    //
    // conhost gets the READ end of the child's input pipe and the WRITE end of
    // its output pipe; cImp keeps the other two. None of the four is ever marked
    // inheritable: `CreatePipe` with null attributes returns non-inheritable
    // handles and this spawn passes `bInheritHandles = FALSE`, so there is no
    // inheritable window to protect (module header, note 1).
    let (stdin_rd, stdin_wr) = make_pipe()?;
    let (stdout_rd, stdout_wr) = match make_pipe() {
        Ok(p) => p,
        Err(e) => {
            close_all(&[stdin_rd, stdin_wr]);
            return Err(e);
        }
    };

    // ── the pseudoconsole ──
    let size = COORD {
        X: req.size.cols.max(1) as i16,
        Y: req.size.rows.max(1) as i16,
    };
    let mut hpc: HPCON = 0;
    // `CreatePseudoConsole` starts the console host process, so it goes through
    // the spawn gate like every other spawn cImp causes — shared, because the
    // exclusive leg belongs to the one path that needs inheritable handles.
    let hr = crate::spawn_gate::with_shared(|| {
        // SAFETY: both handles are live; `hpc` is a valid out-param. conhost
        // duplicates the two handles it is given, so ours stay ours.
        unsafe { CreatePseudoConsole(size, stdin_rd, stdout_wr, CONPTY_FLAGS, &mut hpc) }
    });
    // conhost owns its copies now; ours are dead weight and must be closed or
    // the child's stdin never sees EOF and our reader never sees the real one.
    close_all(&[stdin_rd, stdout_wr]);
    if hr != 0 {
        close_all(&[stdin_wr, stdout_rd]);
        return Err(format!(
            "CreatePseudoConsole failed (HRESULT 0x{:08x}) — this system may predate ConPTY \
             (Windows 10 1809)",
            hr as u32
        ));
    }
    let con = Arc::new(PseudoCon(hpc));

    match spawn_into(prepared, &req, hpc) {
        Ok(process) => {
            let inner = Arc::new(Mutex::new(Inner {
                _con: Arc::clone(&con),
                readable: stdout_rd,
                writable: Some(stdin_wr),
                size: req.size,
            }));
            Ok(SandboxedPty {
                master: Box::new(SandboxedMaster {
                    inner,
                    con: Arc::clone(&con),
                }),
                child: Box::new(SandboxedChild {
                    proc: Arc::new(Mutex::new(OwnedProcess(process.0))),
                    pid: process.1,
                }),
            })
        }
        Err(e) => {
            // Nothing survives a failed spawn: closing the pty tears conhost
            // down, and both remaining pipe ends are ours.
            drop(con);
            close_all(&[stdin_wr, stdout_rd]);
            Err(e)
        }
    }
}

/// The `CreateProcessW` half. Returns `(process handle, pid)`.
fn spawn_into(
    prepared: &Prepared,
    req: &TabSpawn<'_>,
    hpc: HPCON,
) -> Result<(HANDLE, u32), String> {
    // Command line: the program then each argument, quoted by the SANDBOX's
    // routine (`quote_arg`) rather than a second copy of the CRT rules — see
    // its doc comment for why reimplementing it is a silent-divergence hazard.
    let mut cmdline = quote_arg(&req.program.to_string_lossy());
    for a in req.args {
        cmdline.push(' ');
        cmdline.push_str(&quote_arg(a));
    }
    let mut cmdline_w = wide_str(&cmdline);
    let mut env_block = env_block_of(req.env);
    let cwd_w = wide(req.cwd.as_os_str());

    // ── security capabilities ──
    let identity = prepared.security();
    let mut cap_attrs: Vec<SID_AND_ATTRIBUTES> = identity
        .caps
        .iter()
        .map(|p| SID_AND_ATTRIBUTES {
            Sid: *p as *mut c_void,
            Attributes: SE_GROUP_ENABLED,
        })
        .collect();
    let mut sec_caps = SECURITY_CAPABILITIES {
        AppContainerSid: identity.container as *mut c_void,
        Capabilities: if cap_attrs.is_empty() {
            null_mut()
        } else {
            cap_attrs.as_mut_ptr()
        },
        CapabilityCount: cap_attrs.len() as u32,
        Reserved: 0,
    };

    // ── the two-attribute list (the spike's headline result) ──
    const ATTR_COUNT: u32 = 2;
    let mut size: usize = 0;
    // SAFETY: the first call only sizes the list.
    unsafe { InitializeProcThreadAttributeList(null_mut(), ATTR_COUNT, 0, &mut size) };
    let mut list_buf = vec![0u8; size];
    let attr_list = list_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    // SAFETY: list_buf is sized by the call above; same count both calls.
    if unsafe { InitializeProcThreadAttributeList(attr_list, ATTR_COUNT, 0, &mut size) } == 0 {
        return Err(format!(
            "InitializeProcThreadAttributeList failed ({})",
            last_error()
        ));
    }
    struct AttrGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for AttrGuard {
        fn drop(&mut self) {
            // SAFETY: initialized above; deleted exactly once.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _attr_guard = AttrGuard(attr_list);

    let mut hpc_value = hpc;
    // SAFETY: attr_list is initialized with room for two attributes; `hpc_value`
    // outlives the CreateProcessW below. Order between the two updates is
    // irrelevant (S3 claim 2, tested both ways).
    if unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            &mut hpc_value as *mut _ as *const c_void,
            std::mem::size_of::<HPCON>(),
            null_mut(),
            null(),
        )
    } == 0
    {
        return Err(format!(
            "UpdateProcThreadAttribute (pseudoconsole) failed ({})",
            last_error()
        ));
    }
    // SAFETY: same list; sec_caps outlives the call below.
    if unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            &mut sec_caps as *mut _ as *const c_void,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        )
    } == 0
    {
        return Err(format!(
            "UpdateProcThreadAttribute (security capabilities) failed ({})",
            last_error()
        ));
    }

    // ── startup info ──
    //
    // The std slots are explicitly INVALID (not the pipe ends): the pty is how
    // the child gets its console, and naming real handles here would let a
    // redirected parent handle reach a process that is supposed to talk only to
    // its pseudoconsole. This is `portable_pty`'s own reasoning, kept.
    let mut siex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    siex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    siex.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    siex.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    siex.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    siex.lpAttributeList = attr_list;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT (0x400).
    // Deliberately NO `CREATE_NO_WINDOW`: the child's console IS the
    // pseudoconsole, and there is no window to suppress.
    let flags = EXTENDED_STARTUPINFO_PRESENT | 0x0000_0400;

    let (ok, create_err) = crate::spawn_gate::with_shared(|| {
        // SAFETY: cmdline_w is a mutable null-terminated wide buffer
        // (CreateProcessW may write to it); env block and cwd are valid; the
        // startup info and attribute list are populated. `bInheritHandles = 0`
        // — this child inherits nothing at all.
        let ok = unsafe {
            CreateProcessW(
                null(),
                cmdline_w.as_mut_ptr(),
                null(),
                null(),
                0,
                flags,
                env_block.as_mut_ptr() as *mut c_void,
                cwd_w.as_ptr(),
                &siex.StartupInfo,
                &mut pi,
            )
        };
        // Read the error INSIDE the closure and before anything else runs:
        // `CreateProcessW` is documented to leave a stale last-error behind on
        // success (S3 gotcha 6 saw 4390 and 6), and the lock release at the end
        // of this scope is entitled to clobber it too.
        (ok, last_error())
    });
    if ok == 0 {
        return Err(format!("CreateProcessW failed ({create_err})"));
    }
    // The thread handle is never used; closing it here is what keeps a sandboxed
    // tab from leaking one handle per launch.
    // SAFETY: a live handle from CreateProcess, closed exactly once.
    unsafe { CloseHandle(pi.hThread) };
    Ok((pi.hProcess, pi.dwProcessId))
}

// ── RAII wrappers ────────────────────────────────────────────────────────────

/// The pseudoconsole handle. Closing it tears down the console host and signals
/// EOF to the child; shared by the master (which resizes it) and the pty's
/// `Inner` (which owns the pipes it feeds), so the console outlives whichever of
/// the two is dropped first.
struct PseudoCon(HPCON);
// SAFETY: an `HPCON` is a kernel handle, not a pointer into this process's
// address space, and every call this module makes on it (`ResizePseudoConsole`,
// `ClosePseudoConsole`) is documented as safe from any thread. The `Arc<Mutex>`
// around the pipes is what serializes the pty's own state; the console handle
// itself needs no further synchronization.
unsafe impl Send for PseudoCon {}
unsafe impl Sync for PseudoCon {}

impl Drop for PseudoCon {
    fn drop(&mut self) {
        // SAFETY: created by CreatePseudoConsole, closed exactly once (the Arc
        // guarantees one drop).
        unsafe { ClosePseudoConsole(self.0) };
    }
}

/// A process handle we own.
struct OwnedProcess(HANDLE);
// SAFETY: same reasoning as `PseudoCon` — a kernel handle, used only through
// Win32 calls that are thread-safe, and always behind a `Mutex` here.
unsafe impl Send for OwnedProcess {}
unsafe impl Sync for OwnedProcess {}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: from CreateProcess, closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The pty's shared state: the console, the readable end of the child's output
/// and the (take-once) writable end of its input.
struct Inner {
    _con: Arc<PseudoCon>,
    readable: HANDLE,
    writable: Option<HANDLE>,
    size: PtySize,
}
// SAFETY: both fields are kernel handles (see `PseudoCon`); `Inner` is only ever
// reached through an `Arc<Mutex<_>>`.
unsafe impl Send for Inner {}

impl Drop for Inner {
    fn drop(&mut self) {
        close_all(&[self.readable]);
        if let Some(w) = self.writable.take() {
            close_all(&[w]);
        }
    }
}

struct SandboxedMaster {
    inner: Arc<Mutex<Inner>>,
    con: Arc<PseudoCon>,
}

impl MasterPty for SandboxedMaster {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("sandboxed pty state poisoned"))?;
        let coord = COORD {
            X: size.cols.max(1) as i16,
            Y: size.rows.max(1) as i16,
        };
        // SAFETY: `con` is a live pseudoconsole for as long as this master is.
        let hr = unsafe { ResizePseudoConsole(self.con.0, coord) };
        if hr != 0 {
            anyhow::bail!(
                "ResizePseudoConsole to {}x{} failed (HRESULT 0x{:08x})",
                size.cols,
                size.rows,
                hr as u32
            );
        }
        inner.size = size;
        Ok(())
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("sandboxed pty state poisoned"))?;
        Ok(inner.size)
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("sandboxed pty state poisoned"))?;
        let dup = duplicate(inner.readable)?;
        Ok(Box::new(HandleStream { handle: dup }))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("sandboxed pty state poisoned"))?;
        // MOVED out, not duplicated — `portable_pty`'s exact semantics
        // ("dropping the writer will send EOF to the slave end", and taking it
        // twice is an error). Keeping a second copy alive inside `Inner` would
        // quietly withhold that EOF from a sandboxed child and from nothing
        // else, which is the kind of divergence that shows up months later as
        // "the sandboxed tab doesn't exit on close".
        let h = inner
            .writable
            .take()
            .ok_or_else(|| anyhow::anyhow!("writer already taken"))?;
        Ok(Box::new(HandleStream { handle: h }))
    }
}

/// A blocking `Read`/`Write` over one pipe handle this value owns outright.
///
/// **It deliberately holds NO reference to the pty state**, which is a
/// correctness property and not an omission. `portable_pty`'s reader is a bare
/// cloned file descriptor for the same reason: dropping the master must tear the
/// pseudoconsole down, which closes conhost's copy of the write end, which is
/// what finally gives the reader its EOF. A reader that kept the pty alive would
/// keep the console alive, never see EOF, and park a blocking-pool thread
/// forever — one per sandboxed tab closed.
struct HandleStream {
    handle: HANDLE,
}
// SAFETY: a duplicated pipe handle, owned exclusively by this value.
unsafe impl Send for HandleStream {}

impl Drop for HandleStream {
    fn drop(&mut self) {
        close_all(&[self.handle]);
    }
}

impl Read for HandleStream {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let mut read: u32 = 0;
        // SAFETY: a live read handle and a caller-owned buffer.
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                null_mut(),
            )
        };
        if ok == 0 {
            // A broken pipe is how a ConPTY reports "the child is gone"; the
            // reader task treats `Ok(0)` as EOF and exits cleanly, which is the
            // behaviour the plain backend produces for the same event.
            let err = IoError::last_os_error();
            return match err.kind() {
                ErrorKind::BrokenPipe => Ok(0),
                _ => Err(err),
            };
        }
        Ok(read as usize)
    }
}

impl Write for HandleStream {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        let mut written: u32 = 0;
        // SAFETY: a live write handle and a caller-owned buffer.
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(IoError::last_os_error());
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> IoResult<()> {
        // A pipe write is already delivered; there is no user-space buffer here
        // to push, and `FlushFileBuffers` on a pipe blocks until the *reader*
        // has consumed everything — which would park the tab's writer behind the
        // child's read loop.
        Ok(())
    }
}

/// The child, in the shape `pty::tasks::spawn_waiter` already drives.
struct SandboxedChild {
    proc: Arc<Mutex<OwnedProcess>>,
    pid: u32,
}

impl std::fmt::Debug for SandboxedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxedChild")
            .field("pid", &self.pid)
            .finish()
    }
}

impl SandboxedChild {
    fn status(&self, wait_ms: u32) -> IoResult<Option<ExitStatus>> {
        let proc = self
            .proc
            .lock()
            .map_err(|_| IoError::other("sandboxed child handle poisoned"))?;
        // SAFETY: a live process handle.
        let wait = unsafe { WaitForSingleObject(proc.0, wait_ms) };
        if wait != WAIT_OBJECT_0 {
            return Ok(None);
        }
        let mut code: u32 = 0;
        // SAFETY: a live process handle and a valid out-param.
        if unsafe { GetExitCodeProcess(proc.0, &mut code) } == 0 {
            return Err(IoError::last_os_error());
        }
        // A signalled process cannot still be active; if the OS says otherwise
        // report "not yet" rather than inventing an exit code of 259.
        if code == STILL_ACTIVE as u32 {
            return Ok(None);
        }
        Ok(Some(ExitStatus::with_exit_code(code)))
    }
}

impl Child for SandboxedChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.status(0)
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        loop {
            if let Some(st) = self.status(u32::MAX)? {
                return Ok(st);
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        self.proc
            .lock()
            .ok()
            .map(|p| p.0 as std::os::windows::io::RawHandle)
    }
}

impl ChildKiller for SandboxedChild {
    fn kill(&mut self) -> IoResult<()> {
        kill_process(&self.proc)
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(SandboxedKiller {
            proc: Arc::clone(&self.proc),
        })
    }
}

/// The detachable killer the manager holds so `shutdown` can terminate a tab
/// while the waiter task is parked in `wait()`.
struct SandboxedKiller {
    proc: Arc<Mutex<OwnedProcess>>,
}

impl std::fmt::Debug for SandboxedKiller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SandboxedKiller")
    }
}

impl ChildKiller for SandboxedKiller {
    fn kill(&mut self) -> IoResult<()> {
        kill_process(&self.proc)
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(SandboxedKiller {
            proc: Arc::clone(&self.proc),
        })
    }
}

/// Terminate the child. The tab's process tree is reaped by the kill-on-close
/// job object it was assigned to (`process_guard::guard_pid`) and, on an
/// ordinary restart, by `procutil::kill_tree`; this is the direct hit on the
/// process cImp holds a handle to.
fn kill_process(proc: &Arc<Mutex<OwnedProcess>>) -> IoResult<()> {
    let proc = proc
        .lock()
        .map_err(|_| IoError::other("sandboxed child handle poisoned"))?;
    // SAFETY: a live process handle; 1 is the conventional "killed" code.
    if unsafe { TerminateProcess(proc.0, 1) } == 0 {
        let err = IoError::last_os_error();
        // Terminating a process that has already exited fails with
        // ERROR_ACCESS_DENIED (5); that is success as far as the caller's intent
        // goes, and reporting it would make every ordinary tab close log a
        // failed kill.
        if self_already_dead(proc.0) {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

fn self_already_dead(h: HANDLE) -> bool {
    let mut code: u32 = 0;
    // SAFETY: a live process handle and a valid out-param.
    if unsafe { GetExitCodeProcess(h, &mut code) } == 0 {
        return false;
    }
    code != STILL_ACTIVE as u32
}

// ── small Win32 helpers ──────────────────────────────────────────────────────

fn last_error() -> u32 {
    // SAFETY: reads thread-local state, no args.
    unsafe { GetLastError() }
}

fn make_pipe() -> Result<(HANDLE, HANDLE), String> {
    let mut rd: HANDLE = INVALID_HANDLE_VALUE;
    let mut wr: HANDLE = INVALID_HANDLE_VALUE;
    // SAFETY: valid out-params; default security (NON-inheritable) and buffer.
    let ok = unsafe { CreatePipe(&mut rd, &mut wr, null(), 0) };
    if ok == 0 {
        return Err(format!("CreatePipe failed ({})", last_error()));
    }
    Ok((rd, wr))
}

/// A non-inheritable duplicate of `h` in this process.
fn duplicate(h: HANDLE) -> anyhow::Result<HANDLE> {
    let mut out: HANDLE = INVALID_HANDLE_VALUE;
    // SAFETY: `h` is live; `out` is a valid out-param; source and target
    // processes are both this one.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            h,
            GetCurrentProcess(),
            &mut out,
            0,
            0, // NOT inheritable
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        anyhow::bail!("DuplicateHandle failed ({})", last_error());
    }
    Ok(out)
}

fn close_all(handles: &[HANDLE]) {
    for &h in handles {
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            // SAFETY: each is a handle this module owns; callers pass disjoint
            // sets and each set exactly once.
            unsafe { CloseHandle(h) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The deadlock flag must never be set, and nothing else may differ from
    /// the plain path** (decision B7, S3 gotcha 1).
    ///
    /// `const` assertions already pin both halves at compile time; this test
    /// exists so a failure carries a *sentence* rather than "evaluation of
    /// constant value failed" — the symptom the first half prevents (a tab that
    /// starts, emits ~90 bytes ending in `ESC[6n`, and hangs) reads as a broken
    /// sandbox and would be diagnosed as one.
    #[test]
    fn the_conpty_flags_are_the_plain_paths_minus_the_deadlock_bit() {
        assert_eq!(
            CONPTY_FLAGS & PSEUDOCONSOLE_INHERIT_CURSOR,
            0,
            "PSEUDOCONSOLE_INHERIT_CURSOR makes conhost block the child's startup on a DSR \
             reply; a bespoke ConPTY path that sets it hangs every sandboxed tab"
        );
        // Parity with `portable_pty`'s 0x7, minus that one bit. A sandboxed tab
        // must behave like a plain tab in everything except the boundary around
        // it — a different re-flow on window drag is exactly the "two paths
        // silently disagree" class this phase is built against.
        assert_eq!(
            CONPTY_FLAGS,
            PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE,
            "the sandboxed console must keep the plain path's resize quirk and win32 input mode"
        );
        assert_eq!(CONPTY_FLAGS, 0x6);
        // The plain path's value, spelled out, so the relationship is visible
        // rather than asserted: 0x7 is what portable_pty passes.
        const PORTABLE_PTY_FLAGS: u32 =
            PSEUDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE;
        assert_eq!(
            PORTABLE_PTY_FLAGS & !PSEUDOCONSOLE_INHERIT_CURSOR,
            CONPTY_FLAGS,
            "the ONLY flag this path drops is the DSR-deadlock bit"
        );
    }

    /// The attribute numbers, spelled out. They are derived from one shared
    /// input bit, so a typo in the derivation would move BOTH silently — the
    /// same guard `sandbox::windows` carries for its own pair.
    #[test]
    fn the_attribute_values_are_the_documented_ones() {
        assert_eq!(PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, 0x0002_0016);
        assert_eq!(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, 0x0002_0009);
        assert_ne!(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            "two attributes with one number is a one-attribute list that silently drops the \
             sandbox (or the console)"
        );
    }

    /// A pipe pair round-trips, and closing the write end reports EOF as
    /// `Ok(0)` rather than an error — the property the tab's reader task relies
    /// on to exit cleanly when a child dies. No child and no container needed:
    /// this is a pipe property.
    #[test]
    fn the_handle_stream_reads_writes_and_reports_eof() {
        let (rd, wr) = make_pipe().expect("pipe");
        let mut reader = HandleStream { handle: rd };
        let mut writer = HandleStream { handle: wr };
        writer.write_all(b"hello pty").expect("write");
        writer.flush().expect("flush");
        let mut buf = [0u8; 32];
        let n = reader.read(&mut buf).expect("read");
        assert_eq!(&buf[..n], b"hello pty");
        // Dropping the writer closes the last write end ⇒ EOF, not an error.
        drop(writer);
        assert_eq!(reader.read(&mut buf).expect("eof reads as 0"), 0);
    }

    /// `duplicate` yields an independent handle: closing the duplicate must not
    /// disturb the original, which is what makes `try_clone_reader` safe to call
    /// more than once.
    #[test]
    fn a_duplicated_handle_is_independent() {
        let (rd, wr) = make_pipe().expect("pipe");
        let dup = duplicate(rd).expect("duplicate");
        assert_ne!(dup, rd);
        close_all(&[dup]);
        // The original still works.
        let mut reader = HandleStream { handle: rd };
        close_all(&[wr]);
        let mut buf = [0u8; 4];
        assert_eq!(reader.read(&mut buf).expect("eof"), 0);
    }
}
