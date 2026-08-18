//! V33 Phase A — the Windows AppContainer engine (spike S1 productionized).
//!
//! Everything Win32 in the sandbox lives here so the platform-neutral `mod.rs`
//! stays readable. The three moving parts, in the order [`prepare`] uses them:
//!
//! 1. **Profile** — one *stable* AppContainer profile (`cimp.worker`). Stable
//!    because every grant is an ACL entry keyed to the container SID; an
//!    ephemeral per-spawn profile would re-ACL the toolchain dirs on every
//!    spawn and leak registered profiles. Created unelevated; re-derived on
//!    `ERROR_ALREADY_EXISTS`.
//! 2. **Grants** (decision 3, three tiers) — the program's install dir is made
//!    readable+executable to the container SID unless it already is (Program
//!    Files / Windows carry `ALL APPLICATION PACKAGES` by Windows convention).
//!    User-owned dirs get a one-time inheritable ACE via `SetEntriesInAclW` +
//!    `SetNamedSecurityInfoW`; a dir we lack `WRITE_DAC` on (Administrators-owned)
//!    fails the *whole* prepare, so the child runs unsandboxed and loud rather
//!    than half-confined. The project root gets full access the same way.
//! 3. **Drive mapping** — a free drive letter is `DefineDosDevice`-mapped to the
//!    root and the child's cwd is the drive root, so the ancestor-chain
//!    canonicalization quirk (git's `mingw_getcwd`, node's `realpathSync` — S1
//!    §"the gotcha") never sees the unlistable `C:\`. Refcounted across
//!    concurrent spawns on the same root; unmapped on last release.
//!
//! The spawn itself is a bespoke `CreateProcessW` with a two-entry attribute
//! list — `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` (the AppContainer
//! itself; std/tokio `Command` cannot attach one on stable Rust, which is the
//! whole reason this path is hand-rolled) and
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (which handles `bInheritHandles = 1`
//! actually hands over — see [`spawn_blocking_inner`]'s "the inheritance race"
//! comment). Job-object membership composes on top via
//! `process_guard::guard_pid` (assign-after-spawn, same documented race as the
//! PTY child).

use std::ffi::{c_void, OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, GENERIC_READ, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE,
    SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, IsValidSid, SECURITY_CAPABILITIES,
    SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, WinCapabilityInternetClientSid,
    DACL_SECURITY_INFORMATION, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DefineDosDeviceW, GetLogicalDrives, DDD_RAW_TARGET_PATH, DDD_REMOVE_DEFINITION,
    FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};
use windows_sys::Win32::System::Pipes::CreatePipe;

/// The one stable profile name (see module header, point 1).
const PROFILE_NAME: &str = "cimp.worker";
/// `SE_GROUP_ENABLED` — not re-exported by this `windows-sys` surface, so the
/// documented constant value, guarded by a test that a capability SID built
/// with it is valid.
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
/// `PROC_THREAD_ATTRIBUTE_INPUT` — the bit `ProcThreadAttributeValue(Number,
/// Thread, Input, Additive)` sets for an attribute the caller *supplies* (as
/// opposed to one Win32 fills in). Both attributes below are inputs, neither is
/// thread-scoped or additive, so each value is simply this bit OR'd with the
/// attribute's number. windows-sys exposes none of these symbols at this
/// surface; the encoding is stable Win32 ABI (`processthreadsapi.h`).
const PROC_THREAD_ATTRIBUTE_INPUT: usize = 0x0002_0000;
/// `ProcThreadAttributeSecurityCapabilities` = 9. Asserted against a successful
/// spawn in tests.
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = PROC_THREAD_ATTRIBUTE_INPUT | 9;
/// `ProcThreadAttributeHandleList` = 2 — the attribute that turns
/// `bInheritHandles = 1` from "every inheritable handle in this process" into
/// "exactly these". See [`spawn_blocking_inner`]'s inheritance-race comment for
/// why this is a correctness fix and not a hardening nicety.
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = PROC_THREAD_ATTRIBUTE_INPUT | 2;
/// The two attribute values, pinned: the constants above are derived from one
/// shared bit, so a typo in the derivation would silently move BOTH. These are
/// the numbers `processthreadsapi.h` produces.
const _: () = assert!(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES == 0x0002_0009);
const _: () = assert!(PROC_THREAD_ATTRIBUTE_HANDLE_LIST == 0x0002_0002);
/// `SE_GROUP_ENABLED` sanity: a capability SID must validate.
const _: () = assert!(SE_GROUP_ENABLED == 4);

/// How long the parent waits for a drain thread to deliver its result after the
/// child has exited (or been terminated).
///
/// **Why this exists (incident, 2026-08-18).** The first live sandboxed
/// `run_command` (`git --version`) wedged for 22+ minutes with no row, no error
/// and no timeout, pinning the offload worker's single slot. The child had long
/// since exited; what never returned was the parent's `join()` on a drain
/// thread whose blocking `ReadFile` never saw EOF, because a *concurrent* spawn
/// elsewhere in cImp (the shadow-repo `git`, a PTY shell, a server) had
/// inherited a copy of our pipe's write end and was holding it open. The
/// `timeout` argument bounds only `WaitForSingleObject` on the process; nothing
/// bounded the drains. [`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`] stops OUR child
/// leaking handles, but it cannot stop other spawn sites from inheriting ours,
/// so the drain wait is bounded too — belt and braces, deliberately.
const DRAIN_GRACE: Duration = Duration::from_secs(5);
/// The second wait, after [`CancelSynchronousIo`] has aborted the pending
/// `ReadFile`. Short: either the cancel took effect almost immediately or it
/// never will, and a caller that has already waited [`DRAIN_GRACE`] should not
/// wait another five seconds to learn nothing.
const DRAIN_CANCEL_GRACE: Duration = Duration::from_secs(2);

pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}
pub(crate) fn wide_str(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn last_error() -> u32 {
    // SAFETY: GetLastError reads thread-local state, no args.
    unsafe { GetLastError() }
}

// ── SIDs ────────────────────────────────────────────────────────────────────

/// A SID we own, stored as **bytes** rather than as a Win32 pointer.
///
/// Copying the SID into a `Vec<u8>` at construction (rather than holding the
/// allocator's pointer) buys three things at the cost of one `memcpy`: no
/// leaked `LocalAlloc`/`FreeSid` bookkeeping, and — the load-bearing one —
/// `Send + Sync` without an `unsafe impl`, which a raw pointer field would
/// deny. [`Prepared`] is held across an `.await` in `run_command`'s dispatch,
/// so a non-`Sync` field here makes the whole tool future non-`Send` and the
/// agent loop stops compiling.
#[derive(Clone)]
struct OwnedSid {
    bytes: Vec<u8>,
}

impl OwnedSid {
    /// The SID as Win32 wants it. Valid for as long as `self` is; every call
    /// site passes it straight into a Win32 call that does not retain it.
    fn as_psid(&self) -> *mut c_void {
        self.bytes.as_ptr() as *mut c_void
    }

    /// Copy `len` bytes of a Win32-owned PSID into an owned buffer.
    ///
    /// # Safety
    /// `psid` must point to a valid SID for the duration of this call.
    unsafe fn copy_from(psid: *mut c_void) -> Result<Self, String> {
        use windows_sys::Win32::Security::GetLengthSid;
        if psid.is_null() || IsValidSid(psid) == 0 {
            return Err("SID is invalid".into());
        }
        let len = GetLengthSid(psid) as usize;
        if len == 0 {
            return Err("SID has zero length".into());
        }
        let bytes = std::slice::from_raw_parts(psid as *const u8, len).to_vec();
        Ok(Self { bytes })
    }
}

/// The container SID for `PROFILE_NAME`, creating the profile if it does not
/// exist. Buffer-backed: the SID bytes live in `_buf` for the value's lifetime.
fn container_sid() -> Result<OwnedSid, String> {
    let name = wide_str(PROFILE_NAME);
    let display = wide_str("cImp worker sandbox");
    let desc = wide_str("V33 Phase A: agent-initiated run_command children");
    let mut sid: *mut c_void = null_mut();
    // SAFETY: all pointers are valid null-terminated wide strings / out-params.
    let hr = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            desc.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    let sid = if hr == 0 {
        sid
    } else if hr as u32 == 0x8007_0000 | ERROR_ALREADY_EXISTS {
        let mut derived: *mut c_void = null_mut();
        // SAFETY: name is a valid wide string; derived is a valid out-param.
        let dhr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut derived) };
        if dhr != 0 {
            return Err(format!(
                "AppContainer SID derivation failed (0x{:08x}) — profile exists but is unreadable",
                dhr as u32
            ));
        }
        derived
    } else {
        return Err(format!(
            "AppContainer profile creation failed (0x{:08x})",
            hr as u32
        ));
    };
    // Copy out of the Win32 allocation immediately, then free it: the owned
    // form is what everything downstream uses (see [`OwnedSid`]).
    // SAFETY: `sid` is a valid PSID from the calls above.
    let owned = unsafe { OwnedSid::copy_from(sid) };
    // SAFETY: both APIs above return a `LocalAlloc`-family block the caller
    // owns; we are done reading it whether the copy succeeded or not.
    unsafe { LocalFree(sid) };
    owned.map_err(|e| format!("AppContainer SID unusable: {e}"))
}

/// A well-known capability SID (e.g. internetClient), backed by an owned buffer.
fn capability_sid(kind: i32) -> Result<OwnedSid, String> {
    let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut cb = buf.len() as u32;
    // SAFETY: buf is sized to SECURITY_MAX_SID_SIZE; cb is its length in/out.
    let ok = unsafe {
        CreateWellKnownSid(
            kind,
            null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut cb,
        )
    };
    if ok == 0 {
        return Err(format!("CreateWellKnownSid failed ({})", last_error()));
    }
    // `cb` now holds the real length; trim the SECURITY_MAX_SID_SIZE scratch
    // down to it so `OwnedSid::bytes` is exactly the SID.
    buf.truncate(cb as usize);
    Ok(OwnedSid { bytes: buf })
}

// ── grants (decision 3) ───────────────────────────────────────────────────────

/// Dirs already granted this session, so repeated spawns of the same toolchain
/// don't re-walk it. The ACE itself is idempotent on disk; this just skips the
/// write.
static GRANTED: Mutex<Option<std::collections::HashSet<PathBuf>>> = Mutex::new(None);

/// True if `dir` is under a location Windows already grants
/// `ALL APPLICATION PACKAGES` read+execute (Program Files, Windows). Checked by
/// prefix on the canonical path — cheap and correct for the standard installs
/// (S1 verified go/dotnet/clang/git need no grant there).
fn is_app_package_readable(dir: &Path) -> bool {
    let lower = dir.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    for env in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "SystemRoot"] {
        if let Some(base) = std::env::var_os(env) {
            let base = base.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
            if !base.is_empty() && lower.starts_with(&base) {
                return true;
            }
        }
    }
    false
}

/// Access mask meaning read+execute (traverse) — tier (a)+(b) for toolchain
/// dirs.
const RX: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
/// Full access — the project root only.
const FULL: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;

/// Add one inheritable GRANT ACE for `sid` on `dir`, merging into the existing
/// DACL. Idempotent: `SetEntriesInAclW` with the same trustee+mask replaces
/// rather than stacks. Returns a user-facing reason on failure — most often
/// `WRITE_DAC` denial on an Administrators-owned dir, which is the signal to
/// run unsandboxed (module header, tier c of the ladder).
/// Returns `Ok(true)` when an ACE was actually written (first grant of this
/// (dir, mask) in this session), `Ok(false)` when nothing needed doing — the
/// caller records the former, so the Events feed shows a machine being
/// prepared exactly once rather than on every spawn.
fn grant_dir(dir: &Path, sid: *mut c_void, mask: u32) -> Result<bool, String> {
    grant_path(dir, sid, mask, false)
}

/// The generalized form: `SE_FILE_OBJECT` names files and directories alike, so
/// the ONLY difference is inheritance — a directory's ACE is made inheritable so
/// the tree under it is covered, a file's is not (there is nothing to inherit
/// it, and asking for it would read as "the folder was granted too").
///
/// V33 Phase B needs the file form for `~/.claude.json` and `~/.gitconfig`:
/// granting their parent (`%USERPROFILE%`) instead would hand the container the
/// user's entire home directory, which is the precise opposite of the point.
fn grant_path(path: &Path, sid: *mut c_void, mask: u32, is_file: bool) -> Result<bool, String> {
    if !is_file && is_app_package_readable(path) && mask == RX {
        return Ok(false);
    }
    // Key on (path, mask, kind) so a later FULL grant on a path RX-granted
    // earlier still applies. Cheap: only run_command roots, toolchain dirs and
    // the tab seam's small harness table land here.
    let key = |p: &Path| p.join(format!("\u{1}{mask}\u{1}{}", u8::from(is_file)));
    {
        let mut g = GRANTED.lock().map_err(|_| "grant lock poisoned".to_string())?;
        let set = g.get_or_insert_with(std::collections::HashSet::new);
        let k = key(path);
        if set.contains(&k) {
            return Ok(false);
        }
        // Record optimistically; a failure below removes it so a retry re-runs.
        set.insert(k);
    }
    match grant_path_uncached(path, sid, mask, is_file) {
        Ok(()) => Ok(true),
        Err(e) => {
            // Un-record so a later attempt retries rather than trusting a
            // grant that never landed.
            if let Ok(mut g) = GRANTED.lock() {
                if let Some(set) = g.as_mut() {
                    set.remove(&key(path));
                }
            }
            Err(e)
        }
    }
}

fn grant_path_uncached(
    dir: &Path,
    sid: *mut c_void,
    mask: u32,
    is_file: bool,
) -> Result<(), String> {
    let mut path_w = wide(dir.as_os_str());

    // Read the current DACL.
    let mut psd: *mut c_void = null_mut();
    let mut old_dacl: *mut windows_sys::Win32::Security::ACL = null_mut();
    // SAFETY: path_w is a valid wide string; out-params are valid.
    let rc = unsafe {
        windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_dacl,
            null_mut(),
            &mut psd,
        )
    };
    if rc != 0 {
        return Err(format!("read DACL of {} failed ({rc})", dir.display()));
    }
    struct SdGuard(*mut c_void);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: psd came from GetNamedSecurityInfoW, freed with LocalFree.
                unsafe { LocalFree(self.0) };
            }
        }
    }
    let _sd = SdGuard(psd);

    // Build one EXPLICIT_ACCESS granting `mask`, inheritable to subdirs/files.
    let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
    ea.grfAccessPermissions = mask;
    ea.grfAccessMode = GRANT_ACCESS;
    // A file has nothing beneath it to inherit the ACE (`NO_INHERITANCE` = 0).
    ea.grfInheritance = if is_file {
        0
    } else {
        SUB_CONTAINERS_AND_OBJECTS_INHERIT
    };
    ea.Trustee = TRUSTEE_W {
        pMultipleTrustee: null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
        ptstrName: sid as *mut u16,
    };

    let mut new_dacl: *mut windows_sys::Win32::Security::ACL = null_mut();
    // SAFETY: one valid EXPLICIT_ACCESS; old_dacl may be null (treated as empty).
    let rc = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
    if rc != 0 {
        return Err(format!("build DACL for {} failed ({rc})", dir.display()));
    }
    struct AclGuard(*mut windows_sys::Win32::Security::ACL);
    impl Drop for AclGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: new_dacl came from SetEntriesInAclW, freed with LocalFree.
                unsafe { LocalFree(self.0 as *mut c_void) };
            }
        }
    }
    let _acl = AclGuard(new_dacl);

    // SAFETY: path_w valid; new_dacl valid; other handles null as documented.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            null_mut(),
        )
    };
    if rc != 0 {
        // 5 = ERROR_ACCESS_DENIED — the WRITE_DAC case the ladder names.
        return Err(if rc == 5 {
            format!(
                "cannot grant sandbox access to {} (owned by another principal; \
                 needs an elevated one-time grant or copy-into-root)",
                dir.display()
            )
        } else {
            format!("apply DACL to {} failed ({rc})", dir.display())
        });
    }
    Ok(())
}

// ── drive mapping (the canonicalization gotcha) ───────────────────────────────

/// root → (drive letter e.g. "S:", refcount). One mapping per distinct root,
/// shared by concurrent spawns; removed when the last guard drops.
static DRIVES: Mutex<Option<std::collections::HashMap<PathBuf, (String, u32)>>> =
    Mutex::new(None);

/// A live `subst` mapping. Dropping it decrements the root's refcount and
/// unmaps on zero.
pub struct DriveGuard {
    root: PathBuf,
    letter: String,
}

impl DriveGuard {
    /// The drive-root path the child should use as cwd (e.g. `S:\`).
    fn drive_root(&self) -> PathBuf {
        PathBuf::from(format!("{}\\", self.letter))
    }
}

impl Drop for DriveGuard {
    fn drop(&mut self) {
        let mut map = match DRIVES.lock() {
            Ok(m) => m,
            Err(_) => return,
        };
        let Some(m) = map.as_mut() else { return };
        let remove = if let Some((_, rc)) = m.get_mut(&self.root) {
            *rc = rc.saturating_sub(1);
            *rc == 0
        } else {
            false
        };
        if remove {
            m.remove(&self.root);
            // Must be the SAME spelling `map_drive` defined the mapping with —
            // exact-target removal string-matches against the stored target.
            let target = wide_str(&nt_target(&self.root));
            let letter = wide_str(&self.letter);
            // SAFETY: both valid wide strings; DDD_REMOVE_DEFINITION with an
            // exact target removes just this mapping.
            unsafe {
                DefineDosDeviceW(
                    DDD_REMOVE_DEFINITION | DDD_RAW_TARGET_PATH,
                    letter.as_ptr(),
                    target.as_ptr(),
                );
            }
        }
    }
}

/// The NT-object-namespace spelling of `root`, for
/// `DefineDosDeviceW(DDD_RAW_TARGET_PATH, ..)`.
///
/// A RAW target is resolved in the NT object namespace, where a Win32 drive
/// path is spelled `\??\P:\dir` — NOT `\\?\P:\dir`, which is the *Win32*
/// long-path prefix and means nothing to the object manager. The root arrives
/// here canonicalized (so usually `\\?\`-prefixed), and passing it through raw
/// defined a mapping whose target no NT lookup could resolve: the letter
/// existed, every use of it failed, and the first sandboxed spawn died with
/// `CreateProcessW failed (267)` ("the directory name is invalid" — the
/// child's cwd was the broken drive root). 2026-08-18, rc.7 live — the defect
/// the `map_drive` deadlock had been hiding.
///
/// `\\?\UNC\server\share` becomes `\??\UNC\server\share`, and an unprefixed
/// `P:\dir` becomes `\??\P:\dir`, so every input shape lands on the NT form.
fn nt_target(root: &Path) -> String {
    let s = root.as_os_str().to_string_lossy();
    let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
    format!(r"\??\{stripped}")
}

/// The first free drive letter D..=Z not in use, as "X:".
///
/// `taken`: letters this process has already handed out but whose mapping the
/// OS bitmask may not reflect yet. The caller passes the set in — `map_drive`
/// already holds [`DRIVES`] when it calls this, so this function must never
/// touch that lock itself. It used to, and the same-thread re-lock was a
/// silent self-deadlock on `map_drive`'s cache-miss path: the first sandboxed
/// spawn of an app run hung forever after the grant row, with no child, no
/// timeout and nothing recorded (2026-08-18, rc.6 live).
fn free_drive_letter(taken: &std::collections::HashSet<String>) -> Option<String> {
    // SAFETY: no args, returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    for i in 3..26u32 {
        if mask & (1 << i) == 0 {
            let letter = format!("{}:", (b'A' + i as u8) as char);
            if !taken.contains(&letter) {
                return Some(letter);
            }
        }
    }
    None
}

/// Map (or reuse a mapping of) `root` to a drive letter.
fn map_drive(root: &Path) -> Result<DriveGuard, String> {
    let mut map = DRIVES.lock().map_err(|_| "drive lock poisoned".to_string())?;
    let m = map.get_or_insert_with(std::collections::HashMap::new);
    if let Some((letter, rc)) = m.get_mut(root) {
        *rc += 1;
        return Ok(DriveGuard {
            root: root.to_path_buf(),
            letter: letter.clone(),
        });
    }
    // The taken set comes from the guard this function already holds —
    // `free_drive_letter` must not (re-)lock `DRIVES` itself, see its doc.
    let taken: std::collections::HashSet<String> =
        m.values().map(|(l, _)| l.clone()).collect();
    let letter = free_drive_letter(&taken).ok_or("no free drive letter for the sandbox root")?;
    let letter_w = wide_str(&letter);
    let target = wide_str(&nt_target(root));
    // SAFETY: both valid wide strings; RAW_TARGET_PATH maps the letter to the
    // NT-namespace spelling of `root` (see `nt_target`).
    let ok = unsafe {
        DefineDosDeviceW(DDD_RAW_TARGET_PATH, letter_w.as_ptr(), target.as_ptr())
    };
    if ok == 0 {
        return Err(format!(
            "drive mapping of {} failed ({})",
            root.display(),
            last_error()
        ));
    }
    // **Defined is not usable.** `DefineDosDeviceW` only writes a symbolic-link
    // object; it never resolves the target, so ANY target string it accepts
    // yields a letter that exists. Twice now that has produced a live incident
    // whose only symptom was `CreateProcessW failed (267)` at the first spawn —
    // rc.7's Win32-prefixed raw target, and rc.9's *relative* project root
    // (`.`, from a `/graph_run` body with no `cwd`), which maps the letter to
    // `\??\.` — a name no NT lookup can serve. Both are indistinguishable from
    // "the sandbox is broken" at the seam, because the letter looked fine.
    //
    // So the mapping is USED here, once, before anyone builds a boundary on it:
    // a `stat` of the drive root. A letter that cannot be statted is torn down
    // and reported as an unavailable sandbox — the loud degradation `plan`
    // already knows how to surface — instead of being handed to a spawn that
    // can only fail with an unattributable Win32 error code.
    let probe = PathBuf::from(format!("{letter}\\"));
    if let Err(e) = std::fs::metadata(&probe) {
        // SAFETY: both valid wide strings; exact-target removal takes back the
        // definition made three lines up, so a failed mapping leaks no letter.
        unsafe {
            DefineDosDeviceW(
                DDD_REMOVE_DEFINITION | DDD_RAW_TARGET_PATH,
                letter_w.as_ptr(),
                target.as_ptr(),
            );
        }
        return Err(format!(
            "the sandbox drive mapping of {} ({letter} → {}) is not usable ({e}) — the project \
             root must be an existing ABSOLUTE path",
            root.display(),
            nt_target(root)
        ));
    }
    m.insert(root.to_path_buf(), (letter.clone(), 1));
    Ok(DriveGuard {
        root: root.to_path_buf(),
        letter,
    })
}

// ── the prepared spawn ────────────────────────────────────────────────────────

/// Everything a sandboxed spawn needs, assembled by [`prepare`]. Holds the
/// drive guard, so dropping a `Prepared` releases the mapping.
pub struct Prepared {
    container: OwnedSid,
    caps: Vec<OwnedSid>,
    drive: DriveGuard,
    /// Env names→values to add/override for the child (TEMP/TMP/HOME/USERPROFILE
    /// pointed inside the mapped root).
    ///
    /// **The tab seam deliberately does not consume this** (V33 Phase B,
    /// decision B4): a tab CLI lives off its real state directories, so
    /// `HOME`/`USERPROFILE` must stay real there and only the scratch dirs move.
    /// `sandbox::tabs::compose_env` builds its own overrides for that reason —
    /// which is a fact worth knowing before "make the tab path reuse
    /// `env_overrides`" looks like a tidy-up.
    pub env_overrides: Vec<(String, OsString)>,
}

/// The AppContainer identity of a [`Prepared`], as raw `PSID`s carried as
/// `usize` — the shuttle `spawn_and_capture` already uses to move them onto a
/// blocking thread, because a `*mut c_void` is neither `Send` nor `Sync`.
///
/// # Safety contract
///
/// The pointers borrow buffers owned by the `Prepared` this came from. They are
/// valid only while that value is alive, and the caller must not outlive it —
/// which is why this is `pub(crate)` and produced by a method rather than being
/// a free-standing value anyone can construct.
pub(crate) struct SecurityIdentity {
    pub container: usize,
    pub caps: Vec<usize>,
}

impl Prepared {
    /// The container + capability SIDs for a bespoke `CreateProcessW`.
    ///
    /// V33 Phase B: the sandboxed ConPTY backend lives in `pty/` (it is a PTY
    /// mechanism, not a sandbox one) and needs exactly these two things from the
    /// sandbox engine. See [`SecurityIdentity`]'s safety contract.
    pub(crate) fn security(&self) -> SecurityIdentity {
        SecurityIdentity {
            container: self.container.as_psid() as usize,
            caps: self.caps.iter().map(|c| c.as_psid() as usize).collect(),
        }
    }

    /// The cwd the child runs in (the drive-root, so getcwd never walks `C:\`).
    pub fn cwd(&self) -> PathBuf {
        self.drive.drive_root()
    }

    /// The child's cwd for a directory *under* the mapped root, expressed on
    /// the mapped drive.
    ///
    /// The `run_check` seam needs this: `CheckDef::cwd` runs a check in a
    /// subdirectory of the project (this repo's own `src-tauri/`, any monorepo
    /// package), and handing the child the drive ROOT instead would silently
    /// run every nested check in the wrong directory — a green run of the wrong
    /// thing, which is worse than a failure. `dir` outside the root (or equal to
    /// it) falls back to the drive root.
    pub fn cwd_under(&self, dir: &Path) -> PathBuf {
        cwd_under_root(&self.drive.drive_root(), &self.drive.root, dir)
    }
}

/// [`Prepared::cwd_under`]'s rule, as a pure function so it is testable without
/// a live drive mapping: re-express `dir` (a path under `root`) on the mapped
/// drive, falling back to the drive root when `dir` is the root itself or is
/// not under it at all.
fn cwd_under_root(drive_root: &Path, root: &Path, dir: &Path) -> PathBuf {
    match dir.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => drive_root.join(rel),
        Ok(_) => drive_root.to_path_buf(),
        Err(_) => {
            // `dir` is not under `root` at all. The drive root is the only path
            // that can be expressed here, but it is NOT the directory the
            // caller asked for — so the fallback is stated rather than taken
            // silently. Today the checks seam derives `dir` as `root.join(rel)`
            // and cannot land here; a future caller that mixes path SPELLINGS
            // (a canonicalized `\\?\P:\…` root against a plain `P:\…` dir)
            // would, and would otherwise get a check that ran one directory up
            // with nothing anywhere saying so.
            tracing::warn!(
                root = %root.display(),
                dir = %dir.display(),
                "sandbox: the requested cwd is not under the sandbox root — falling back to the \
                 drive root, so this child does NOT run where it was asked to"
            );
            drive_root.to_path_buf()
        }
    }
}

/// Do all sandbox preparation for one agent-initiated child, or return a
/// user-facing reason the caller records and then runs the child plain.
///
/// `hints` carries the grants a seam needs beyond its own program's install
/// directory — see [`super::GrantHints`].
pub async fn prepare(
    cfg: &super::SandboxCfg,
    seam: &str,
    program: &Path,
    hints: &super::GrantHints,
    root: &Path,
    _env: &[(&str, OsString)],
) -> Result<Prepared, String> {
    // Blocking Win32 (ACL walks, profile creation) off the async worker.
    let cfg = cfg.clone();
    let seam = seam.to_string();
    let program = program.to_path_buf();
    let hints = hints.clone();
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || prepare_blocking(&cfg, &seam, &program, &hints, &root))
        .await
        .map_err(|e| format!("sandbox prepare task failed: {e}"))?
}

fn prepare_blocking(
    cfg: &super::SandboxCfg,
    seam: &str,
    program: &Path,
    hints: &super::GrantHints,
    root: &Path,
) -> Result<Prepared, String> {
    let container = container_sid()?;

    // Grant the project root full access, and the program's install dir R+X.
    //
    // Each first-time grant is recorded (decision 5's surface applied to the
    // preparation, not just the failure): stamping an ACE on a toolchain
    // directory is a durable change to the user's machine, so it says so once,
    // in the same lane that reports an unsandboxed run.
    let mut granted: Vec<String> = Vec::new();
    if grant_dir(root, container.as_psid(), FULL)? {
        granted.push(format!("{} (read+write)", root.display()));
    }
    // A program's own directory, plus the interpreter root behind it when the
    // directory is a launcher directory (`…\Scripts\tool.exe`) — see
    // [`super::interpreter_root`] for the convention and the live failure that
    // named it. Both grants are RX and both are recorded; the second names why
    // it is wider than "the program's directory".
    let mut grant_program_dir = |install: &Path, why: &str| -> Result<(), String> {
        if grant_dir(install, container.as_psid(), RX)? {
            granted.push(if why.is_empty() {
                format!("{} (read+execute)", install.display())
            } else {
                format!("{} (read+execute, {why})", install.display())
            });
        }
        if let Some(interp) = super::interpreter_root(install) {
            if grant_dir(interp, container.as_psid(), RX)? {
                granted.push(format!(
                    "{} (read+execute, the interpreter root behind {})",
                    interp.display(),
                    install.display()
                ));
            }
        }
        Ok(())
    };
    if let Some(install) = program.parent() {
        grant_program_dir(install, "")?;
    }
    // Grant inference for a seam whose spawned program is not the program that
    // does the work: `run_check` spawns `cmd.exe` (which needs no grant — it
    // lives under `System32`, where Windows already gives ALL APPLICATION
    // PACKAGES read+execute) and the check's own first token — `cargo` in
    // `cargo test --bin cimp` — lives in the user's profile, which does not.
    // Later tokens of a compound command line are NOT inferred; they rely on an
    // AAP-readable install dir or a settings `extra_grant_dirs` row, and a tool
    // that then cannot start surfaces as a DENIAL row rather than a silent
    // unsandboxed retry.
    for extra in &hints.programs {
        if let Some(install) = extra.parent() {
            grant_program_dir(install, "inferred from the check command")?;
        }
    }
    // Write grants the seam asked for — cImp-owned scratch a tool is handed an
    // absolute path into (today: the audit runner's SARIF report directory).
    // FULL, not RX: the point is that the child WRITES there. Recorded like
    // every other grant, because an inheritable write ACE on a directory is a
    // durable change to the user's machine whatever cImp owns it for.
    for dir in &hints.full_dirs {
        if grant_dir(dir, container.as_psid(), FULL)? {
            granted.push(format!(
                "{} (read+write, tool report scratch)",
                dir.display()
            ));
        }
    }
    // V33 Phase B: the reviewed grant TABLE — a seam's own rows, each with its
    // width, its kind and its reason (see [`super::GrantRow`]). The tab seam's
    // per-harness state paths arrive here.
    //
    // An OPTIONAL row whose path does not exist is skipped rather than failing:
    // most harness state is created on first use, so "absent" is the normal
    // state of half the table on a fresh machine, and refusing to sandbox a tab
    // because the user has no `~/.config/git` would be the prerequisite check
    // punishing a healthy machine. A REQUIRED row that is missing still fails
    // the whole prepare, loudly, exactly like an ungrantable directory.
    for row in &hints.rows {
        if !row.required && !row.path.exists() {
            continue;
        }
        let mask = match row.access {
            super::GrantAccess::ReadExecute => RX,
            super::GrantAccess::Full => FULL,
        };
        if grant_path(&row.path, container.as_psid(), mask, row.is_file)? {
            granted.push(format!(
                "{} ({}, {})",
                row.path.display(),
                match row.access {
                    super::GrantAccess::ReadExecute => "read+execute",
                    super::GrantAccess::Full => "read+write",
                },
                row.reason
            ));
        }
    }
    for extra in &cfg.extra_grant_dirs {
        if grant_dir(extra, container.as_psid(), RX)? {
            granted.push(format!("{} (read+execute, from settings)", extra.display()));
        }
    }
    if !granted.is_empty() {
        super::record_event(
            seam,
            root,
            "grant",
            format!("{} sandbox grant(s) applied", granted.len()),
            granted.join("\n"),
            true,
        );
    }

    // Capabilities (module header, point network scoping is all/none today).
    let mut caps = Vec::new();
    if cfg.allow_network {
        caps.push(capability_sid(WinCapabilityInternetClientSid)?);
    }

    let drive = map_drive(root)?;
    let drive_root = drive.drive_root();

    // Redirect scratch + home inside the mapped root so a child that writes
    // config/temp lands in the one writable place (and getcwd stays shallow).
    let mut env_overrides = Vec::new();
    for name in ["TEMP", "TMP"] {
        env_overrides.push((name.to_string(), drive_root.as_os_str().to_os_string()));
    }
    for name in ["HOME", "USERPROFILE"] {
        env_overrides.push((name.to_string(), drive_root.as_os_str().to_os_string()));
    }

    Ok(Prepared {
        container,
        caps,
        drive,
        env_overrides,
    })
}

/// The captured result of a sandboxed run, matching what `run_command` formats.
pub struct CapturedRun {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_capped: bool,
    pub stderr_capped: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// At least one drain thread never finished, even after
    /// [`CancelSynchronousIo`] — its pipe was still held open by a handle that
    /// leaked to some other child (see [`DRAIN_GRACE`]). The run's output is
    /// therefore INCOMPLETE (that stream reads as empty), the thread is
    /// detached and its read handle deliberately leaked. The caller says so in
    /// the model-visible output rather than presenting a truncated capture as
    /// the whole answer.
    pub drains_leaked: bool,
    /// The caller's [`SpawnRequest::cancel`] flag was raised while the child
    /// was running, so the child was terminated. Distinct from `timed_out`:
    /// a cancel is the user (or a shutting-down scan) asking to stop, and the
    /// audit runner reports it as `Outcome::Cancelled`, not as a timeout.
    pub cancelled: bool,
}

/// Everything one sandboxed spawn needs. A struct rather than a parameter list
/// because the three seams need different things from it (a raw shell tail, a
/// cancel signal) and an eight-argument function is where the wrong argument
/// gets passed in the right position.
pub struct SpawnRequest<'a> {
    /// The program to run. Always an absolute path resolved by cImp.
    pub program: &'a Path,
    /// Arguments, each quoted per the CRT rules. Ignored when
    /// [`SpawnRequest::raw_tail`] is set.
    pub args: &'a [String],
    /// **The `cmd.exe /C` escape hatch.** When set, the command line becomes
    /// `<program> <raw_tail>` with the tail appended VERBATIM.
    ///
    /// `cmd.exe` parses its `/C` payload with its OWN quoting rules, not the
    /// `CommandLineToArgvW` rules [`quote_arg`] implements, so a check command
    /// that contains its own quotes (`"C:\Program Files\...\tsc.cmd" --noEmit`)
    /// comes out double-escaped and unparseable if it is quoted as an argument.
    /// This is the same reason `checks::shell_command` uses `raw_arg` rather
    /// than `arg` on the plain path — the two paths have to agree, or a check
    /// would run differently depending on whether the sandbox is on.
    pub raw_tail: Option<&'a str>,
    /// The child's complete environment (see
    /// [`crate::sandbox::child_env::ChildEnv`]).
    pub env: &'a [(OsString, OsString)],
    pub cwd: &'a Path,
    /// Per-stream capture cap in bytes — each seam's own.
    pub cap: usize,
    /// The CHILD's deadline (see the note on [`spawn_and_capture`]).
    pub timeout: std::time::Duration,
    /// Optional cooperative cancel. `None` (the `run_command` seam) keeps the
    /// single blocking `WaitForSingleObject` the engine has always used; `Some`
    /// makes the wait poll so a cancelled audit scan can terminate its child
    /// promptly instead of waiting out a multi-minute tool budget.
    pub cancel: Option<super::CancelFlag>,
}

/// How often the wait loop looks at the cancel flag. Only reached when a caller
/// supplied one; without a flag the wait is a single call with the full
/// deadline, exactly as before.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// A `CreateProcessW` environment block: NUL-separated `K=V` pairs, double-NUL
/// terminated, **sorted case-insensitively** — Windows requires the sort, and an
/// unsorted block is the kind of defect that works until the one variable that
/// happens to be looked up by the loader is in the wrong place.
///
/// One function since V33 Phase B, shared by the capture spawn above and the
/// sandboxed ConPTY spawn (`pty::sandboxed_conpty`), because two copies of this
/// are two chances for a sandboxed tab and a sandboxed command to disagree about
/// what an environment block is.
pub(crate) fn env_block_of(env: &[(OsString, OsString)]) -> Vec<u16> {
    let mut pairs: Vec<(OsString, OsString)> = env.to_vec();
    pairs.sort_by(|a, b| {
        a.0.to_string_lossy()
            .to_ascii_uppercase()
            .cmp(&b.0.to_string_lossy().to_ascii_uppercase())
    });
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in &pairs {
        // An empty name cannot be spelled in a block (`=VALUE` is the reserved
        // per-drive-cwd form), and Windows fails the spawn outright on one.
        if k.is_empty() {
            continue;
        }
        block.extend(k.encode_wide());
        block.push('=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// Spawn `program args…` inside the container with the given final environment
/// and capture output, bounded by `req.cap` bytes per stream and `req.timeout`.
///
/// **`timeout` bounds the child, not this call.** It is what the wait loop
/// gets; the drains that follow have their own bound ([`DRAIN_GRACE`]), and the
/// caller carries a backstop over the whole future
/// ([`crate::sandbox::backstop_for`]) because a helper cannot be its own last
/// line of defence.
///
/// **Cancellation must not be implemented by dropping this future.** Dropping
/// it drops the caller's [`Prepared`], which unmaps the subst drive out from
/// under a live child. Callers raise [`SpawnRequest::cancel`] and keep awaiting
/// the same future — see `audit::runner::spawn_sandboxed`.
///
/// Runs the whole synchronous Win32 dance on a blocking thread: two reader
/// threads drain stdout/stderr (so neither pipe deadlocks the other), the main
/// path waits with a deadline, assigns the pid to the kill-on-close job, and
/// terminates on timeout or cancel.
pub async fn spawn_and_capture(
    prepared: &Prepared,
    req: SpawnRequest<'_>,
) -> Result<CapturedRun, String> {
    let SpawnRequest {
        program,
        args,
        raw_tail,
        env,
        cwd,
        cap,
        timeout,
        cancel,
    } = req;
    // Command line: quote the program, then either the raw shell tail verbatim
    // or each argument quoted. cImp resolved `program` to an absolute path
    // already; args come from the model and may contain spaces.
    let mut cmdline = quote_arg(&program.to_string_lossy());
    match raw_tail {
        Some(tail) => {
            cmdline.push(' ');
            cmdline.push_str(tail);
        }
        None => {
            for a in args {
                cmdline.push(' ');
                cmdline.push_str(&quote_arg(a));
            }
        }
    }

    let env_block = env_block_of(env);

    // Snapshot the security capabilities into a heap-stable form the blocking
    // closure owns.
    let container_psid = prepared.container.as_psid() as usize;
    let cap_psids: Vec<usize> = prepared.caps.iter().map(|c| c.as_psid() as usize).collect();
    let cmdline_w: Vec<u16> = wide_str(&cmdline);
    let cwd_w: Vec<u16> = wide(cwd.as_os_str());

    tokio::task::spawn_blocking(move || {
        spawn_blocking_inner(
            container_psid,
            &cap_psids,
            cmdline_w,
            env_block,
            cwd_w,
            cap,
            timeout,
            cancel,
        )
    })
    .await
    .map_err(|e| format!("sandbox spawn task failed: {e}"))?
}

#[allow(clippy::too_many_arguments)]
fn spawn_blocking_inner(
    container_psid: usize,
    cap_psids: &[usize],
    mut cmdline_w: Vec<u16>,
    env_block: Vec<u16>,
    cwd_w: Vec<u16>,
    cap: usize,
    timeout: std::time::Duration,
    cancel: Option<super::CancelFlag>,
) -> Result<CapturedRun, String> {
    // ── pipes ──
    let (out_rd, out_wr) = make_pipe()?;
    let (err_rd, err_wr) = make_pipe()?;
    // NOTHING is inheritable yet — not even the write ends. Marking a handle
    // inheritable is a PROCESS-WIDE fact (it is a flag on the handle, and
    // `bInheritHandles` reads every such flag), so the interval in which these
    // are inheritable is the interval in which any other cImp spawn could
    // capture a copy. That interval is opened, used and closed inside the
    // `spawn_gate::exclusive()` scope further down, and it is a handful of
    // syscalls wide.
    //
    // The read ends are never inheritable at all; the explicit `false` says so
    // rather than relying on `CreatePipe`'s default with null attributes.
    set_inherit(out_rd, false);
    set_inherit(err_rd, false);
    set_inherit(out_wr, false);
    set_inherit(err_wr, false);
    // Stdin is the NUL device rather than `INVALID_HANDLE_VALUE`. Two reasons:
    // a pseudo-handle cannot appear in the handle list below (and every handle
    // named in `STARTUPINFO`'s std slots must), and NUL is what "no stdin" has
    // always meant here — a child that reads stdin now gets EOF instead of an
    // invalid handle.
    let nul = match open_nul() {
        Ok(h) => h,
        Err(e) => {
            close_all(&[out_rd, out_wr, err_rd, err_wr]);
            return Err(e);
        }
    };

    // ── security capabilities + attribute list ──
    let mut cap_attrs: Vec<SID_AND_ATTRIBUTES> = cap_psids
        .iter()
        .map(|p| SID_AND_ATTRIBUTES {
            Sid: *p as *mut c_void,
            Attributes: SE_GROUP_ENABLED,
        })
        .collect();
    let mut sec_caps = SECURITY_CAPABILITIES {
        AppContainerSid: container_psid as *mut c_void,
        Capabilities: if cap_attrs.is_empty() {
            null_mut()
        } else {
            cap_attrs.as_mut_ptr()
        },
        CapabilityCount: cap_attrs.len() as u32,
        Reserved: 0,
    };

    // Two attributes: the security capabilities and the handle list.
    const ATTR_COUNT: u32 = 2;
    let mut size: usize = 0;
    // SAFETY: first call just sizes the list.
    unsafe { InitializeProcThreadAttributeList(null_mut(), ATTR_COUNT, 0, &mut size) };
    let mut list_buf = vec![0u8; size];
    let attr_list = list_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    // SAFETY: list_buf is sized by the call above; same count both calls.
    if unsafe { InitializeProcThreadAttributeList(attr_list, ATTR_COUNT, 0, &mut size) } == 0 {
        close_all(&[nul, out_rd, out_wr, err_rd, err_wr]);
        return Err(format!("InitializeProcThreadAttributeList failed ({})", last_error()));
    }
    struct AttrGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for AttrGuard {
        fn drop(&mut self) {
            // SAFETY: initialized above; deleted exactly once.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _attr_guard = AttrGuard(attr_list);
    // SAFETY: attr_list initialized; sec_caps outlives CreateProcess below.
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
        close_all(&[nul, out_rd, out_wr, err_rd, err_wr]);
        return Err(format!("UpdateProcThreadAttribute failed ({})", last_error()));
    }

    // ── the inheritance race, closed ──
    //
    // `bInheritHandles = 1` without a handle list means "inherit EVERY
    // inheritable handle this process holds at spawn time" — and cImp spawns
    // children constantly from other threads (the shadow-repo `git` on every
    // prompt tap, PTY shells, the offload server). Two races follow from that,
    // and this attribute closes one of them:
    //
    //  * OUR child inheriting some other spawn's in-flight handles — closed
    //    here, exactly, by naming the three handles it may have;
    //  * some OTHER spawn inheriting the write ends below before we close them
    //    — not closable with a handle list (the handles must be inheritable
    //    during our own `CreateProcessW`, and Windows has no per-spawn scoping
    //    for that), and closed instead by [`crate::spawn_gate`]: every spawn
    //    cImp makes takes that gate SHARED, this one takes it EXCLUSIVELY, and
    //    the inheritable window below is entirely inside the exclusive scope.
    //    That leak is what wedged the first live sandboxed run: the write end
    //    stayed open in a stranger's process, our reader never saw EOF, and the
    //    parent's `join()` never returned. Spawns made by third-party code deep
    //    inside libraries are still outside the gate, so bounding the drains
    //    (see [`collect_drain`]) remains the defence for that residue.
    //
    // The array must outlive `CreateProcessW` — same lifetime discipline as
    // `sec_caps` — so it is a plain local declared before the call.
    let mut inherit_handles: [HANDLE; 3] = [nul, out_wr, err_wr];
    // SAFETY: attr_list initialized with room for 2 attributes; the handle
    // array outlives the CreateProcessW call below.
    if unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            inherit_handles.as_mut_ptr() as *const c_void,
            std::mem::size_of::<HANDLE>() * inherit_handles.len(),
            null_mut(),
            null(),
        )
    } == 0
    {
        close_all(&[nul, out_rd, out_wr, err_rd, err_wr]);
        return Err(format!(
            "UpdateProcThreadAttribute (handle list) failed ({})",
            last_error()
        ));
    }

    // ── startup info ──
    let mut siex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    siex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    siex.StartupInfo.hStdInput = nul; // no stdin (matches Stdio::null)
    siex.StartupInfo.hStdOutput = out_wr;
    siex.StartupInfo.hStdError = err_wr;
    siex.lpAttributeList = attr_list;

    let mut env_block = env_block; // owned, mutable pointer below
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // CREATE_UNICODE_ENVIRONMENT (0x400) | EXTENDED_STARTUPINFO_PRESENT |
    // CREATE_NO_WINDOW (0x0800_0000).
    let flags = EXTENDED_STARTUPINFO_PRESENT | 0x0000_0400 | 0x0800_0000;

    // ── the exclusive window ──────────────────────────────────────────────
    //
    // Everything above (attribute list, env block, startup info) and everything
    // below (guard_pid, drains, waits) is deliberately OUTSIDE this scope. What
    // is inside it is only what has to be: the three handle flips, the spawn,
    // and closing our copies again — a few syscalls, single-digit milliseconds.
    // The write lock stalls every other spawn in the app for exactly as long as
    // this scope lasts, so widening it buys no correctness and costs throughput
    // everywhere else.
    //
    // **No other lock may be taken in here.** `RwLock` is not reentrant and the
    // gate is process-wide: a spawn (or anything that could reach one) attempted
    // from inside this scope deadlocks cImp against itself. The only calls
    // present are `set_inherit`, `CreateProcessW`, `last_error` and `close_all`,
    // all of them thin Win32 wrappers that take nothing.
    let (ok, create_err) = {
        let _spawn_window = crate::spawn_gate::exclusive();
        // NOW the three handles the child may inherit become inheritable —
        // with every other cImp spawn locked out. The handle list narrows what
        // this child gets; the gate is what stops anyone else's child from
        // getting it too.
        set_inherit(nul, true);
        set_inherit(out_wr, true);
        set_inherit(err_wr, true);
        // SAFETY: cmdline_w is a mutable, null-terminated wide buffer (CreateProcessW
        // may write to it); env/cwd are valid; startup info and attribute list are
        // populated; handle inheritance is on.
        let ok = unsafe {
            CreateProcessW(
                null(),
                cmdline_w.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                env_block.as_mut_ptr() as *mut c_void,
                cwd_w.as_ptr(),
                &siex.StartupInfo,
                &mut pi,
            )
        };
        // Read before `close_all`: `CloseHandle` is entitled to clobber the
        // thread's last-error value, and so is the lock release at the end of
        // this scope.
        let create_err = last_error();
        // The child owns the write ends now; close ours so EOF arrives when it
        // exits — and so the window in which they are inheritable ends here,
        // inside the guard, rather than at some later point where another
        // spawn could see them.
        close_all(&[nul, out_wr, err_wr]);
        (ok, create_err)
    };
    if ok == 0 {
        close_all(&[out_rd, err_rd]);
        return Err(format!("CreateProcessW failed ({create_err})"));
    }

    // Kill-on-close job membership (assign-after-spawn; hProcess pins the pid).
    crate::process_guard::guard_pid(pi.dwProcessId);

    // ── drain both pipes on their own threads ──
    //
    // Each thread hands its result back over a channel rather than through its
    // return value, so the parent's collection can be BOUNDED — a `join()` on a
    // thread parked in `ReadFile` is unbounded by construction, which is the
    // shape of the 2026-08-18 wedge (see [`DRAIN_GRACE`]).
    let (t_out, out_rx) = spawn_drain(out_rd, cap);
    let (t_err, err_rx) = spawn_drain(err_rd, cap);

    // ── wait with deadline (and, when the caller asked for it, cancel) ──
    //
    // With no cancel flag the loop below runs exactly one `WaitForSingleObject`
    // with the full deadline — byte-for-byte the behaviour the `run_command`
    // seam has always had. With a flag, the same deadline is served in
    // [`CANCEL_POLL`] slices so a cancelled audit scan does not have to wait out
    // a multi-minute per-tool budget before its child is terminated.
    let deadline = std::time::Instant::now() + timeout;
    let slice = match &cancel {
        Some(_) => CANCEL_POLL,
        None => timeout,
    };
    let mut timed_out = false;
    let mut cancelled = false;
    let mut exit_code = None;
    let mut exited = false;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        let ms = remaining.min(slice).as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: pi.hProcess is a live process handle from CreateProcess.
        let wait = unsafe { WaitForSingleObject(pi.hProcess, ms) };
        if wait == WAIT_OBJECT_0 {
            exited = true;
            break;
        }
        if wait != WAIT_TIMEOUT {
            // WAIT_FAILED / WAIT_ABANDONED — the pre-existing "no code" arm.
            break;
        }
        if cancel
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst))
        {
            cancelled = true;
            break;
        }
    }
    if exited {
        let mut code: u32 = 0;
        // SAFETY: live handle; code is a valid out-param.
        unsafe { GetExitCodeProcess(pi.hProcess, &mut code) };
        exit_code = Some(code as i32);
    } else if timed_out || cancelled {
        // SAFETY: live handle; 124 is our timeout/cancel sentinel.
        unsafe { TerminateProcess(pi.hProcess, 124) };
        // The job object also reaps the tree; give the child a moment then read.
        // SAFETY: live handle.
        unsafe { WaitForSingleObject(pi.hProcess, 2000) };
        exit_code = Some(124);
    }

    let out = collect_drain(&out_rx, t_out, DRAIN_GRACE, DRAIN_CANCEL_GRACE);
    let err = collect_drain(&err_rx, t_err, DRAIN_GRACE, DRAIN_CANCEL_GRACE);

    // SAFETY: handles from CreateProcess, closed exactly once here.
    unsafe {
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }

    // A read handle is closed only if the thread that was reading it is DONE.
    // Closing a handle another thread is blocked in `ReadFile` on is
    // UB-adjacent (the kernel object can be recycled under the pending IO); a
    // leaked handle pair per wedged run is the far cheaper of the two costs,
    // and the wedge is reported rather than hidden.
    let mut drains_leaked = false;
    let (stdout, stdout_capped) = match out {
        DrainOutcome::Done(bytes, capped) => {
            close_all(&[out_rd]);
            (bytes, capped)
        }
        DrainOutcome::Leaked => {
            drains_leaked = true;
            (Vec::new(), false)
        }
    };
    let (stderr, stderr_capped) = match err {
        DrainOutcome::Done(bytes, capped) => {
            close_all(&[err_rd]);
            (bytes, capped)
        }
        DrainOutcome::Leaked => {
            drains_leaked = true;
            (Vec::new(), false)
        }
    };

    Ok(CapturedRun {
        stdout,
        stderr,
        stdout_capped,
        stderr_capped,
        exit_code,
        timed_out,
        drains_leaked,
        cancelled,
    })
}

// ── bounded drains (the 2026-08-18 wedge) ─────────────────────────────────────

/// One drain thread's product: the captured bytes and whether more was produced
/// than the cap.
type DrainResult = (Vec<u8>, bool);

/// What [`collect_drain`] managed to get out of one drain thread.
enum DrainOutcome {
    Done(Vec<u8>, bool),
    /// The thread never delivered, even after its pending `ReadFile` was
    /// cancelled. It is detached and its read handle is deliberately leaked.
    Leaked,
}

/// Start one drain thread, returning its join handle and the channel it will
/// deliver on. The thread still returns normally — the channel exists so the
/// PARENT's wait can be bounded, not to change the thread's own lifecycle.
fn spawn_drain(rd: HANDLE, cap: usize) -> (std::thread::JoinHandle<()>, Receiver<DrainResult>) {
    let (tx, rx) = std::sync::mpsc::channel::<DrainResult>();
    // `HANDLE` is a raw pointer and therefore not `Send`; the value is a kernel
    // handle, not a memory address, and moving it to the reader is the whole
    // point — same `as usize` shuttle the pre-existing code used.
    let rd_val = rd as usize;
    let t = std::thread::spawn(move || {
        let _ = tx.send(drain_pipe(rd_val as HANDLE, cap));
    });
    (t, rx)
}

/// Collect one drain thread's result without ever blocking indefinitely.
///
/// Three stages, in order of preference:
///
/// 1. `recv_timeout(grace)` — the normal path; the child exited, the pipe hit
///    EOF, the thread already sent.
/// 2. Still nothing ⇒ the `ReadFile` is parked on a pipe whose write end leaked
///    to some other process. [`CancelSynchronousIo`] aborts that pending IO on
///    the reader's own thread, which makes `ReadFile` fail and `drain_pipe`
///    return what it has; `recv_timeout(cancel_grace)` picks it up.
/// 3. Still nothing ⇒ give up on the thread. Detach it (dropping a
///    `JoinHandle` does not stop the thread) and report [`DrainOutcome::Leaked`]
///    so the caller can say the capture is incomplete instead of presenting an
///    empty stream as the truth.
///
/// The two graces are parameters rather than the consts directly so the
/// machinery is testable in milliseconds; production passes [`DRAIN_GRACE`] and
/// [`DRAIN_CANCEL_GRACE`].
fn collect_drain(
    rx: &Receiver<DrainResult>,
    thread: std::thread::JoinHandle<()>,
    grace: Duration,
    cancel_grace: Duration,
) -> DrainOutcome {
    match rx.recv_timeout(grace) {
        Ok((bytes, capped)) => {
            // The thread has already produced its value; this join is bounded
            // by "return from send" and cannot re-enter `ReadFile`.
            let _ = thread.join();
            return DrainOutcome::Done(bytes, capped);
        }
        // The sender was dropped without sending — the thread panicked. Nothing
        // to wait for and nothing pending; the handle is ours to close again.
        Err(RecvTimeoutError::Disconnected) => {
            let _ = thread.join();
            return DrainOutcome::Done(Vec::new(), false);
        }
        Err(RecvTimeoutError::Timeout) => {}
    }
    // SAFETY: the JoinHandle owns a live thread handle for as long as it is
    // alive, which is this whole scope. `CancelSynchronousIo` on a thread with
    // no pending synchronous IO simply fails with ERROR_NOT_FOUND, which the
    // second recv below covers.
    unsafe { CancelSynchronousIo(thread.as_raw_handle() as HANDLE) };
    match rx.recv_timeout(cancel_grace) {
        Ok((bytes, capped)) => {
            let _ = thread.join();
            DrainOutcome::Done(bytes, capped)
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = thread.join();
            DrainOutcome::Done(Vec::new(), false)
        }
        Err(RecvTimeoutError::Timeout) => {
            // Detach. Do NOT join (that is the hang we are here to avoid) and
            // do NOT close the read handle (the caller's contract).
            drop(thread);
            DrainOutcome::Leaked
        }
    }
}

// ── small Win32 helpers ───────────────────────────────────────────────────────

fn make_pipe() -> Result<(HANDLE, HANDLE), String> {
    let mut rd: HANDLE = INVALID_HANDLE_VALUE;
    let mut wr: HANDLE = INVALID_HANDLE_VALUE;
    // SAFETY: out-params valid; default security; default buffer size.
    let ok = unsafe { CreatePipe(&mut rd, &mut wr, null(), 0) };
    if ok == 0 {
        return Err(format!("CreatePipe failed ({})", last_error()));
    }
    Ok((rd, wr))
}

/// A read handle on the NUL device — the child's stdin.
///
/// Replaces the `INVALID_HANDLE_VALUE` that used to sit in `hStdInput`: a
/// pseudo-handle cannot appear in [`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`], and
/// every handle named in `STARTUPINFO`'s std slots must. Opened by the PARENT
/// (outside the container), so the child's own token never has to reach the
/// device.
///
/// Returned NON-inheritable. It is flipped inheritable, used and closed inside
/// the `spawn_gate::exclusive()` scope in [`spawn_blocking_inner`], together
/// with the two pipe write ends — see the comment there for why the window has
/// to be that narrow.
fn open_nul() -> Result<HANDLE, String> {
    let name = wide_str("NUL");
    // SAFETY: name is a valid wide string; null security attributes and a null
    // template handle are the documented defaults.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(format!("opening NUL for the child's stdin failed ({})", last_error()));
    }
    set_inherit(h, false);
    Ok(h)
}

fn set_inherit(h: HANDLE, inherit: bool) {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    // SAFETY: h is a valid handle; flag mask constant.
    unsafe {
        SetHandleInformation(
            h,
            HANDLE_FLAG_INHERIT,
            if inherit { HANDLE_FLAG_INHERIT } else { 0 },
        );
    }
}

fn close_all(handles: &[HANDLE]) {
    for &h in handles {
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            // SAFETY: each is a valid handle we own; closed once (callers pass
            // disjoint sets).
            unsafe { CloseHandle(h) };
        }
    }
}

/// Read `h` to EOF, keeping at most `cap` bytes but draining the rest so the
/// child never blocks on a full pipe. Returns (bytes, capped?).
fn drain_pipe(h: HANDLE, cap: usize) -> (Vec<u8>, bool) {
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    let mut out: Vec<u8> = Vec::new();
    let mut capped = false;
    let mut chunk = [0u8; 8192];
    loop {
        let mut read: u32 = 0;
        // SAFETY: h is a valid read handle; chunk/read are valid buffers.
        let ok = unsafe {
            ReadFile(
                h,
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                &mut read,
                null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break; // broken pipe on child exit, or EOF
        }
        let n = read as usize;
        if out.len() < cap {
            let take = n.min(cap - out.len());
            out.extend_from_slice(&chunk[..take]);
            if take < n {
                capped = true;
            }
        } else {
            capped = true;
        }
    }
    (out, capped)
}

/// Quote one command-line argument per the Windows CRT rules (backslashes
/// before a quote double; the whole thing wrapped in quotes if it has spaces
/// or quotes). Absolute program paths and model args both pass through here.
///
/// `pub(crate)` since V33 Phase B: the sandboxed ConPTY backend
/// (`pty::sandboxed_conpty`) builds its own command line and MUST use this one
/// rather than reimplementing the rules — `portable_pty`'s equivalent
/// (`CommandBuilder::cmdline`) is `pub(crate)` to that crate, and two quoting
/// routines that are supposed to agree is exactly the silent-divergence hazard
/// spike S3 called out.
pub(crate) fn quote_arg(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return s.to_string();
    }
    let mut q = String::from("\"");
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
            }
            '"' => {
                q.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                backslashes = 0;
                q.push('"');
            }
            _ => {
                q.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                q.push(c);
            }
        }
    }
    q.extend(std::iter::repeat_n('\\', backslashes * 2));
    q.push('"');
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real environment, in the block shape `spawn_and_capture` builds:
    /// case-insensitively sorted "K=V" pairs, NUL-separated, double-NUL
    /// terminated. (A hand-made two-variable block is not enough — Windows
    /// itself fails the spawn with ERROR_ENVVAR_NOT_FOUND.)
    fn inherited_env_block() -> Vec<u16> {
        let mut pairs: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        pairs.sort_by(|a, b| {
            a.0.to_string_lossy()
                .to_ascii_uppercase()
                .cmp(&b.0.to_string_lossy().to_ascii_uppercase())
        });
        let mut env_block: Vec<u16> = Vec::new();
        for (k, v) in &pairs {
            if k.is_empty() {
                continue;
            }
            env_block.extend(k.encode_wide());
            env_block.push('=' as u16);
            env_block.extend(v.encode_wide());
            env_block.push(0);
        }
        env_block.push(0);
        env_block
    }

    /// `System32\cmd.exe`, or `None` when this machine cannot offer it.
    fn system32_cmd() -> Option<(PathBuf, PathBuf)> {
        let system32 = std::env::var_os("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32"))
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"));
        let cmd = system32.join("cmd.exe");
        cmd.exists().then_some((system32, cmd))
    }

    #[test]
    fn quote_arg_rules() {
        assert_eq!(quote_arg("simple"), "simple");
        assert_eq!(quote_arg("has space"), "\"has space\"");
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        // No spaces or quotes ⇒ no quoting, and a trailing backslash is only
        // special when a closing quote would follow it.
        assert_eq!(quote_arg(r"C:\path\"), r"C:\path\");
        // With a space the arg IS quoted, so the trailing backslashes must
        // double or they would escape the closing quote.
        assert_eq!(quote_arg(r"C:\my path\"), "\"C:\\my path\\\\\"");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn app_package_readable_covers_program_files() {
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            let git = PathBuf::from(pf).join("Git").join("cmd");
            assert!(is_app_package_readable(&git));
        }
        // A user-profile dir is never app-package readable.
        assert!(!is_app_package_readable(Path::new(
            r"C:\Users\someone\.cargo\bin"
        )));
    }

    /// A nested `CheckDef::cwd` must land in the nested directory ON the mapped
    /// drive — not at the drive root. Running `cargo` at the repo root instead
    /// of in `src-tauri/` is a check that passes by looking at nothing, which is
    /// the worst possible failure shape for a verification tool.
    #[test]
    fn a_nested_check_cwd_is_re_expressed_on_the_mapped_drive() {
        let drive = Path::new(r"S:\");
        let root = Path::new(r"P:\projects\cimp");
        assert_eq!(
            cwd_under_root(drive, root, Path::new(r"P:\projects\cimp\src-tauri")),
            PathBuf::from(r"S:\src-tauri")
        );
        assert_eq!(
            cwd_under_root(drive, root, Path::new(r"P:\projects\cimp\a\b")),
            PathBuf::from(r"S:\a\b")
        );
        // The root itself is the drive root.
        assert_eq!(cwd_under_root(drive, root, root), PathBuf::from(r"S:\"));
        // A directory that is not under the root cannot be expressed on the
        // drive at all; the drive root is the only safe answer (and the sandbox
        // would deny the outside path anyway).
        assert_eq!(
            cwd_under_root(drive, root, Path::new(r"C:\elsewhere")),
            PathBuf::from(r"S:\")
        );
    }

    #[test]
    fn free_drive_letter_is_plausible() {
        // Whatever it returns must be an unused X: form in D..=Z.
        if let Some(l) = free_drive_letter(&Default::default()) {
            assert_eq!(l.len(), 2);
            assert!(l.ends_with(':'));
            let c = l.as_bytes()[0];
            assert!((b'D'..=b'Z').contains(&c), "got {l}");
            // A letter this process has handed out but the OS bitmask does not
            // reflect yet must not be handed out twice.
            let taken: std::collections::HashSet<String> = [l.clone()].into();
            if let Some(next) = free_drive_letter(&taken) {
                assert_ne!(next, l);
            }
        }
    }

    /// The 2026-08-18 rc.6 wedge, pinned: `map_drive`'s cache-miss path called
    /// `free_drive_letter`, which re-locked [`DRIVES`] while `map_drive` held
    /// it — a same-thread self-deadlock that hung sandbox preparation forever
    /// (grant row minted, then nothing: no child, no backstop, worker slot
    /// pinned). The property under test is that `map_drive` RETURNS — mapping
    /// success depends on the runner's privileges and is not asserted.
    #[test]
    fn map_drive_returns_instead_of_deadlocking() {
        let dir = std::env::temp_dir().join("cimp-map-drive-regression");
        let _ = std::fs::create_dir_all(&dir);
        // Canonicalized, so the root is `\\?\`-prefixed — the exact shape the
        // live path feeds in, and the shape whose raw-target spelling rc.7
        // got wrong (see `nt_target`).
        let dir = std::fs::canonicalize(&dir).expect("canonicalize temp dir");
        let (tx, rx) = std::sync::mpsc::channel();
        let d = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send(map_drive(&d));
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(guard)) => {
                // The mapping must be USABLE, not merely defined: rc.7's
                // follow-up defect was a letter that existed while resolving
                // to an NT path no lookup could serve, so the first sandboxed
                // spawn died with CreateProcessW error 267 (invalid cwd).
                std::fs::metadata(guard.drive_root()).unwrap_or_else(|e| {
                    panic!(
                        "the mapped drive root is not usable ({e}) — the DefineDosDeviceW \
                         raw target is mis-spelled for the NT namespace again"
                    )
                });
                // The guard's drop removes the mapping again.
                drop(guard);
            }
            // Mapping can legitimately fail on a locked-down runner (no free
            // letter, no privilege) — the deadlock regression under test is
            // that map_drive RETURNS.
            Ok(Err(e)) => eprintln!("map_drive returned an error (acceptable here): {e}"),
            Err(_) => panic!(
                "map_drive did not return within 10s — the DRIVES self-deadlock is back"
            ),
        }
    }

    /// **The rc.9 defect, pinned: a mapping that DEFINES but does not RESOLVE.**
    ///
    /// `DefineDosDeviceW` writes a symbolic-link object without ever resolving
    /// its target, so a bogus target still yields a letter that exists — and
    /// the only symptom is `CreateProcessW failed (267)` at the first spawn,
    /// an error code that names neither the root nor the mapping. A RELATIVE
    /// project root (`"."`, which reached the checks seam live from a
    /// `/graph_run` body with no `cwd`) produces exactly that: the NT target is
    /// `\??\.`, which no lookup can serve.
    ///
    /// `map_drive` must therefore refuse it, and leave nothing behind: no cache
    /// entry, and no surviving DOS-device definition (taken back on the failure
    /// path, so a rejected root cannot burn a letter per attempt). Nothing here
    /// touches ACLs or a real toolchain directory; the root is a name, not a
    /// place.
    #[test]
    fn map_drive_refuses_a_root_that_maps_to_an_unresolvable_target() {
        let err = match map_drive(Path::new(".")) {
            Err(e) => e,
            Ok(_) => panic!(
                "a relative root maps to `\\??\\.`, which resolves to nothing — it must not \
                 come back as a usable mapping"
            ),
        };
        assert!(
            err.contains("not usable") && err.contains("ABSOLUTE"),
            "the reason must name what is wrong with the root: {err}"
        );
        // No cache entry either: a rejected root must not be reusable as a hit
        // on the next call, which would hand out the poisoned letter anyway.
        // (The OS-level definition is removed on the same path; asserting *that*
        // by letter would race the other tests mapping drives in parallel.)
        assert!(
            DRIVES
                .lock()
                .expect("DRIVES")
                .as_ref()
                .map(|m| !m.contains_key(Path::new(".")))
                .unwrap_or(true),
            "a refused mapping must not be cached"
        );
    }

    /// **A NESTED directory on the mapped drive is usable as a spawn cwd.**
    ///
    /// `map_drive_returns_instead_of_deadlocking` asserts the drive ROOT is
    /// usable, which is what `run_command` and the audit seam ask of it. The
    /// `run_check` seam asks for more — [`Prepared::cwd_under`] hands the child
    /// a directory *beneath* the root — and "the root statted fine" says
    /// nothing about that, so the property is asserted here against a real
    /// mapping and a real `CreateProcessW`, which is the only place a broken
    /// nested path shows up (as error 267).
    ///
    /// Temp directories only: no ACL is stamped, and the AppContainer needs no
    /// grant to run `System32\cmd.exe`. A machine that refuses profiles or has
    /// no free letter skips, exactly like its sibling tests.
    #[test]
    fn a_nested_directory_on_the_mapped_drive_is_a_usable_spawn_cwd() {
        let base = std::env::temp_dir().join("cimp-nested-cwd-regression");
        let nested = base.join("pkg");
        std::fs::create_dir_all(&nested).expect("create the nested fixture");
        let base = std::fs::canonicalize(&base).expect("canonicalize the fixture");
        let Ok(guard) = map_drive(&base) else {
            return; // no free letter / no privilege — the sibling test owns that
        };
        let on_drive = cwd_under_root(&guard.drive_root(), &base, &base.join("pkg"));
        assert_eq!(on_drive, guard.drive_root().join("pkg"));
        std::fs::metadata(&on_drive)
            .unwrap_or_else(|e| panic!("the nested directory {on_drive:?} is not reachable: {e}"));

        let Ok(container) = container_sid() else {
            return; // environment refuses profiles
        };
        let Some((_system32, cmd)) = system32_cmd() else {
            return;
        };
        let cmdline = format!("{} /c exit 7", quote_arg(&cmd.to_string_lossy()));
        match spawn_blocking_inner(
            container.as_psid() as usize,
            &[],
            wide_str(&cmdline),
            inherited_env_block(),
            wide(on_drive.as_os_str()),
            4096,
            Duration::from_secs(30),
            None,
        ) {
            Ok(run) => assert_eq!(
                run.exit_code,
                Some(7),
                "the child ran but did not survive its own exit code"
            ),
            Err(e) => panic!(
                "a nested cwd on the mapped drive was rejected by CreateProcessW: {e} \
                 (267 = ERROR_DIRECTORY — the mapping does not serve nested paths)"
            ),
        }
        drop(guard);
    }

    /// `nt_target` is a pure spelling function; pin every input shape it
    /// claims to handle (doc comment) so a refactor cannot quietly reintroduce
    /// the Win32-prefix-as-NT-path confusion.
    #[test]
    fn nt_target_spells_the_object_namespace_form() {
        assert_eq!(nt_target(Path::new(r"\\?\P:\proj")), r"\??\P:\proj");
        assert_eq!(nt_target(Path::new(r"C:\plain\dir")), r"\??\C:\plain\dir");
        assert_eq!(
            nt_target(Path::new(r"\\?\UNC\srv\share\dir")),
            r"\??\UNC\srv\share\dir"
        );
    }

    // ── bounded drains (the 2026-08-18 wedge) ──
    //
    // This whole module is `#[cfg(windows)]` (see `sandbox/mod.rs`), so these
    // tests are Windows-only by construction. They need no AppContainer and no
    // child process: the wedge is a *pipe* property, so a pipe plus a held-open
    // write end reproduces it exactly.

    /// The normal path: a stream that reaches EOF is collected whole, well
    /// inside the grace.
    #[test]
    fn a_closed_write_end_drains_normally() {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        let (rd, wr) = make_pipe().expect("pipe");
        let payload = b"hello from the child\n";
        let mut written: u32 = 0;
        // SAFETY: wr is a live write handle; the buffer outlives the call.
        let ok = unsafe {
            WriteFile(
                wr,
                payload.as_ptr(),
                payload.len() as u32,
                &mut written,
                null_mut(),
            )
        };
        assert!(ok != 0, "WriteFile failed ({})", last_error());
        // EOF only arrives once every write end is closed.
        close_all(&[wr]);
        let (t, rx) = spawn_drain(rd, 1024);
        match collect_drain(&rx, t, Duration::from_secs(5), Duration::from_secs(2)) {
            DrainOutcome::Done(bytes, capped) => {
                assert_eq!(bytes, payload);
                assert!(!capped);
            }
            DrainOutcome::Leaked => panic!("a closed pipe must drain, not leak"),
        }
        close_all(&[rd]);
    }

    /// The incident, reproduced: the write end is still open in (what stands in
    /// for) another process, so `ReadFile` never sees EOF. The parent must NOT
    /// hang — the grace must elapse and `CancelSynchronousIo` must break the
    /// parked read so the second `recv_timeout` gets a result.
    #[test]
    fn a_leaked_write_end_never_hangs_the_parent() {
        let (rd, wr) = make_pipe().expect("pipe");
        let (t, rx) = spawn_drain(rd, 1024);
        // Give the reader time to actually park inside ReadFile before the
        // grace is measured against it.
        std::thread::sleep(Duration::from_millis(50));
        let grace = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let outcome = collect_drain(&rx, t, grace, Duration::from_secs(5));
        let elapsed = started.elapsed();
        // (a) the grace path triggered — a drain that had already delivered
        // would have returned in microseconds.
        assert!(
            elapsed >= grace,
            "the grace path did not trigger (returned in {elapsed:?})"
        );
        // (b) the cancel unblocked the read and the second recv got a result.
        match outcome {
            DrainOutcome::Done(bytes, _) => assert!(bytes.is_empty()),
            DrainOutcome::Leaked => {
                panic!("CancelSynchronousIo did not unblock the parked ReadFile")
            }
        }
        // The reader is done, so both ends are ours to close.
        close_all(&[rd, wr]);
    }

    /// The two attribute values, as the reader of a diff would want them
    /// stated: one shared input bit, two distinct attribute numbers.
    #[test]
    fn proc_thread_attribute_values_are_the_documented_ones() {
        assert_eq!(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, 0x0002_0009);
        assert_eq!(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, 0x0002_0002);
        assert_ne!(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST
        );
    }

    /// The child's stdin is a real device handle, not a pseudo-handle — the
    /// handle list requires it, and `Stdio::null` semantics want it.
    ///
    /// It comes back NON-inheritable by design: inheritability is granted for
    /// the few syscalls inside the `spawn_gate::exclusive()` window and taken
    /// away again there. (Renamed from `..._inheritable_handle` when that moved.)
    #[test]
    fn nul_opens_as_a_real_device_handle() {
        let h = open_nul().expect("NUL must be openable");
        assert!(h != INVALID_HANDLE_VALUE && !h.is_null());
        close_all(&[h]);
    }

    /// The composition itself, end to end: a real AppContainer child spawned
    /// through the two-attribute list, with a real NUL stdin and a handle list.
    ///
    /// This is the assertion the unit tests above cannot make. The handle list
    /// imposes a rule the old code did not have to satisfy — *every* handle in
    /// `STARTUPINFO`'s std slots must be listed, and a pseudo-handle cannot be
    /// — so a wrong answer here is not a subtle degradation but
    /// `ERROR_INVALID_PARAMETER` on every sandboxed spawn. `cmd.exe` under
    /// `System32` needs no grant (Windows gives ALL APPLICATION PACKAGES
    /// read+execute there), so this touches no ACLs and maps no drive.
    ///
    /// Skips cleanly when the environment refuses AppContainer profiles, and
    /// asserts on the *spawn*, not on the child's output — a container without
    /// grants may well fail to run `echo`, and that is not what is under test.
    #[test]
    fn the_attribute_list_composition_actually_spawns() {
        let Ok(container) = container_sid() else {
            return; // environment refuses profiles; container_sid's own test says so
        };
        let system32 = std::env::var_os("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32"))
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"));
        let cmd = system32.join("cmd.exe");
        if !cmd.exists() {
            return;
        }
        let cmdline = format!("{} /c exit 7", quote_arg(&cmd.to_string_lossy()));
        let env_block = inherited_env_block();
        let run = spawn_blocking_inner(
            container.as_psid() as usize,
            &[],
            wide_str(&cmdline),
            env_block,
            wide(system32.as_os_str()),
            4096,
            Duration::from_secs(30),
            None,
        );
        match run {
            Ok(run) => {
                assert!(!run.timed_out, "the child hung");
                assert!(!run.cancelled, "nothing cancelled this child");
                assert!(!run.drains_leaked, "a drain leaked on a well-behaved child");
                assert_eq!(run.exit_code, Some(7), "the child's exit code must survive");
            }
            Err(e) => {
                // A refused spawn is tolerable ONLY if the container itself was
                // refused — `ERROR_INVALID_PARAMETER` (87) is the handle-list
                // rule being violated and must never be tolerated.
                assert!(
                    !e.contains("(87)"),
                    "the attribute list is malformed — CreateProcessW rejected it: {e}"
                );
            }
        }
    }

    /// **Cancellation terminates the child instead of waiting out its budget.**
    ///
    /// New behaviour in a Win32 wait path that has wedged twice, so it is
    /// asserted against a real child rather than reasoned about. The child is a
    /// pure `cmd.exe` busy loop: no network (an AppContainer without
    /// `internetClient` would kill a `ping` before the cancel could be
    /// observed), no filesystem outside `System32` (which needs no grant), so
    /// the test ACL-stamps nothing and maps no drive.
    ///
    /// The child's own budget is 20 s and the cancel is raised at ~300 ms; the
    /// elapsed assertion is 10 s, generous enough that a loaded CI box cannot
    /// flake it and tight enough that "the cancel was ignored and the timeout
    /// fired" fails. A broken cancel bounds the test at the 20 s budget rather
    /// than hanging it.
    #[test]
    fn a_cancel_terminates_the_sandboxed_child_instead_of_waiting_out_its_budget() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let Ok(container) = container_sid() else {
            return; // environment refuses profiles; container_sid's own test says so
        };
        let Some((system32, cmd)) = system32_cmd() else {
            return;
        };
        // A long busy loop: `rem` is a no-op, so this burns CPU inside cmd.exe
        // and touches nothing.
        let cmdline = format!(
            "{} /c for /l %x in (1,1,2000000000) do @rem",
            quote_arg(&cmd.to_string_lossy())
        );

        let cancel: super::super::CancelFlag = Arc::new(AtomicBool::new(false));
        let raiser = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            raiser.store(true, Ordering::SeqCst);
        });

        let started = std::time::Instant::now();
        let run = spawn_blocking_inner(
            container.as_psid() as usize,
            &[],
            wide_str(&cmdline),
            inherited_env_block(),
            wide(system32.as_os_str()),
            4096,
            Duration::from_secs(20),
            Some(cancel),
        );
        let elapsed = started.elapsed();
        match run {
            Ok(run) => {
                assert!(
                    run.cancelled,
                    "the cancel flag was raised but the run did not report a cancel \
                     (timed_out = {}, exit = {:?})",
                    run.timed_out, run.exit_code
                );
                assert!(
                    !run.timed_out,
                    "a cancel must not be reported as a timeout — the audit runner tells the \
                     two apart in the user-visible tool status"
                );
                assert_eq!(run.exit_code, Some(124), "the terminate sentinel must survive");
                assert!(
                    elapsed < Duration::from_secs(10),
                    "the cancel took {elapsed:?} — the child's 20 s budget was waited out \
                     instead of the flag being polled"
                );
            }
            Err(e) => {
                // Same rule as the composition test: only a refused container is
                // tolerable, never a malformed attribute list.
                assert!(
                    !e.contains("(87)"),
                    "the attribute list is malformed — CreateProcessW rejected it: {e}"
                );
            }
        }
    }

    /// The no-cancel path keeps the single full-deadline wait `run_command` has
    /// always had: with `None`, the poll slice IS the timeout, so the loop makes
    /// exactly one `WaitForSingleObject` call. Asserted on the constant rather
    /// than by instrumenting the loop, because the constant is what encodes it.
    #[test]
    fn the_cancel_poll_only_applies_when_a_caller_supplies_a_flag() {
        assert!(
            CANCEL_POLL < Duration::from_secs(1),
            "a cancel poll slower than a second makes a cancelled scan feel hung"
        );
        assert!(
            CANCEL_POLL >= Duration::from_millis(10),
            "polling faster than 10 ms burns a blocking thread for no benefit"
        );
    }

    #[test]
    fn container_sid_creates_unelevated() {
        // The S1 headline claim, as a regression: profile creation needs no
        // elevation. Skips cleanly if the environment forbids it.
        match container_sid() {
            Ok(sid) => {
                // SAFETY: valid PSID from container_sid.
                assert!(unsafe { IsValidSid(sid.as_psid()) } != 0);
            }
            Err(e) => {
                // Only tolerate a genuine environment refusal, not a logic bug.
                assert!(e.contains("failed"), "unexpected error shape: {e}");
            }
        }
    }
}
