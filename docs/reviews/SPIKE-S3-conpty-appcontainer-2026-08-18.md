# Spike S3 — ConPTY under AppContainer (2026-08-18)

**Verdict: POSITIVE.** A ConPTY child runs inside an AppContainer with **zero
elevation**, a single `STARTUPINFOEXW` attribute list carrying *both*
`PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and
`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, in **either update order**.
The child is a genuine AppContainer process (`TokenIsAppContainer = 1`) *and*
a genuine console process (`GetConsoleMode` succeeds, `GetConsoleScreenBufferInfo`
reports the pty size, node reports `isTTY=true`), and confinement is unchanged
from S1 — read and write denial outside the granted root, error 5 / `EPERM`.
Resize, kill-on-close job assignment, interactive stdin, and nested in-container
spawns all work. **Phase B is implementable on this engine today.** The one
non-negotiable consequence: **cImp cannot keep `portable_pty` for sandboxed
tabs** — the crate hardcodes a one-attribute list and hides the `HPCON` behind
private modules — but it can keep every `portable_pty` *trait* and swap only the
`PtySystem` implementation, so `pty/manager.rs` changes by one line plus a new
backend module.

Environment: Windows 11 Pro 26200.9168 (this dev machine), plain user shell, no
elevation at any step. Harness: `s3-conpty/` in the session scratchpad — the S1
crate copied to a sibling dir and extended with a `ptyrun` subcommand
(S1 left pristine). Same dependency as S1 and as cImp itself, `windows-sys 0.59`;
S3 needed three further **features** and no new crate:
`Win32_System_Pipes`, `Win32_System_IO` (gates `ReadFile`/`WriteFile`),
`Win32_System_JobObjects`. The `windows 0.61` / `webview2-com 0.38`
type-identity pin is untouched.

## What was demonstrated, in order

| # | Claim | Result |
|---|---|---|
| 1 | `InitializeProcThreadAttributeList` accepts count 2 | **Confirmed.** 48 bytes for one attribute, **72 bytes for two** — the list simply grows; no special casing. |
| 2 | Both `UpdateProcThreadAttribute` calls succeed | **Confirmed, in both orderings.** `PSEUDOCONSOLE` (`0x00020016`) then `SECURITY_CAPABILITIES` (`0x00020009`) → both `ok=1`; reversed → both `ok=1`. **Order is irrelevant.** |
| 3 | `CreateProcessW` succeeds with the dual list | **Confirmed.** `ok=1`, `EXTENDED_STARTUPINFO_PRESENT`, `bInheritHandles = FALSE`. `cmd.exe /C echo hello` → `hello`, exit 0, byte-identical to the no-container ConPTY baseline (93 captured bytes both ways). |
| 4 | The child is *really* in the container | **Confirmed by token, not by inference.** An in-root probe run as the ConPTY child reports `TokenIsAppContainer ok=1 value=1`; the same binary run normally reports `value=0`. A **grandchild** (probe re-spawned by the in-container `cmd.exe`) also reports `value=1` — containment is inherited down the console's process tree. |
| 5 | Output flows through the ConPTY pipe | **Confirmed.** Full VT stream captured: `ESC[?9001h ESC[?1004h ESC[?25l ESC[2J ESC[m ESC[H`, OSC-0 title sets (`ESC]0;…BEL`), cursor moves, `ESC[K` erases. Not a passthrough pipe — real conhost rendering. |
| 6 | Input flows and the session is interactive | **Confirmed.** `cmd.exe` fed `echo USER=%USERNAME%\r\n` echoed the typed line and printed `USER=Amir`; the `S:\>` prompt re-rendered between commands; `exit\r\n` ended the session. |
| 7 | The child believes it has a console | **Confirmed, three ways.** `GetConsoleMode(stdin) = 0x000001f7`, `GetConsoleMode(stdout) = 0x00000007` (includes `ENABLE_VIRTUAL_TERMINAL_PROCESSING`); `GetConsoleScreenBufferInfo` → `120x30`, exactly the requested pty size. Same probe outside a ConPTY: all three fail with error 6 (`ERROR_INVALID_HANDLE`). node reports `isTTY=true`, `columns/rows = 120/30`, and `process.stdin.setRawMode(true)` succeeds. |
| 8 | Confinement holds under ConPTY — read | **Confirmed.** In-container direct `CreateFile` of `%USERPROFILE%\.gitconfig` → `Os { code: 5, PermissionDenied }`; the **same command through a `--plain` ConPTY** (no container) reads 416 bytes. In-console `type "%USERPROFILE%\.gitconfig"` → `Access is denied.`, while `type S:\ok.txt` → `granted-root-content`. |
| 9 | Confinement holds under ConPTY — write | **Confirmed.** node in the sandboxed ConPTY: `WRITE_ROOT=OK`, `WRITE_OUTSIDE=DENIED code=EPERM`. Plain-ConPTY control: both `OK`. |
| 10 | `ResizePseudoConsole` mid-session | **Confirmed.** `hr=0x00000000` on a live sandboxed session; a probe run *after* the resize reports `CSBI 60x15` where the probe run *before* reported `120x30`. The AppContainer child observes the resize. |
| 11 | Kill-on-close Job Object after spawn | **Confirmed.** `SetInformationJobObject` = 1, `AssignProcessToJobObject` = 1, `lasterr=0` against the AppContainer child. Killing the harness took the sandboxed `cmd.exe` with it (`CHILD GONE`). S1's `guard_pid` pattern composes with ConPTY unchanged. |
| 12 | Real Phase B children | **Confirmed.** `git --version` → `git version 2.54.0.windows.1`, exit 0. `node --version` → `v25.9.0`, exit 0. A multi-file node CLI in the granted root: `require('./lib/dep')`, `fs.realpathSync`, `setRawMode` all succeed. **`claude.exe --version` → `2.1.234 (Claude Code)`, exit 0, sandboxed, in a ConPTY** — the actual Phase B child, after one *unelevated* RX grant on `C:\Users\Amir\.local` (110 files). |
| 13 | New asymmetries vs S1 | **None found in the confinement or canonicalization behaviour.** Every S1 result reproduced identically through the ConPTY. Two new *mechanism* facts are in Gotchas below. |

## Gotchas

**1. `PSEUDOCONSOLE_INHERIT_CURSOR` deadlocks a terminal that does not answer
DSR.** The first run of this harness hung for 30 s with 90 bytes captured,
ending in `ESC[6n`. With that flag (0x1) conhost queries the cursor position and
**blocks the child's startup until the terminal replies**. `portable_pty` sets
`INHERIT_CURSOR | RESIZE_QUIRK | WIN32_INPUT_MODE` (0x7) because wezterm answers;
cImp's xterm.js frontend answers too, so production is unaffected — but any
bespoke ConPTY path must either answer DSR or drop the flag. The measurements
above used 0x6 (`RESIZE_QUIRK | WIN32_INPUT_MODE`). **This is a ConPTY-protocol
gotcha, not a sandboxing one**, and it is the single easiest way to mistake a
working Phase B for a broken one.

**2. The top-level image is opened with the *creator's* token, not the
container's.** `node.exe` lives in `C:\nvm4w\nodejs`, which carries **no
`ALL APPLICATION PACKAGES` ACE anywhere in its chain** (verified with `icacls` on
the file, its directory, `C:\nvm4w`, and `C:\`) — and it launched and ran
*confined* anyway. This is **not** ConPTY-specific: S1's plain-stdio leg behaves
identically (A/B run). The rule is: the exe cImp names is opened before the
AppContainer token applies, so **the top-level binary needs no grant**; every
file the tool reads *afterwards* does. Direct evidence of the second half:
`node C:\nvm4w\…\npm-cli.js` → `Error: Cannot find module …` /
`MODULE_NOT_FOUND`, and no `NODE_OPTIONS` incantation helps, because the file is
simply unreadable. S1 design consequence 7 stands unchanged in substance; only
its scope narrows — grant-on-first-use is about the tool's **code, config and
caches**, not about its entry-point exe.

**3. The grant ladder's third rung is live on this machine.** `icacls C:\nvm4w
/grant *<SID>:(OI)(CI)(RX) /T` → `Access is denied` unelevated (Administrators-
owned, `Authenticated Users:(M)` carries no `WRITE_DAC`) — exactly S1's
addendum. `C:\Users\Amir\.local` (user-owned, where the Claude CLI lives) granted
fine, 110 files, unelevated. So the Phase B toolchain story is the S1 story
verbatim: Program Files ⇒ nothing to do, user-owned ⇒ grant once,
Administrators-owned ⇒ elevate once, copy into root, or unsandboxed-with-badge.

**4. S1's ancestor-canonicalization gotcha reproduces through ConPTY,
unchanged, with the same fix.** `git status` with `cwd` on the real deep path →
`fatal: Unable to read current working directory: Permission denied`; the same
command with `cwd` on the `subst` drive → `On branch master`, exit 0. Worth
noting for scope: **node and `cmd.exe` do *not* need the subst drive** — both ran
happily with the real 130-character profile path as `cwd`, `%CD%` rendered
correctly, and node's `realpathSync` resolved inside the root. The drive mapping
is required by the *git family* (`mingw_getcwd`), not by the pty. Since cImp's
agent tabs shell out to git constantly, keep the mapping anyway — but it is a
tool mitigation, not a ConPTY one.

**5. conhost runs outside the container, in cImp's own context.**
`CreatePseudoConsole` spawns its console host as a child of the *creating*
process, under the user's normal token (`owner = DESKTOP-…\Amir`, parented to the
harness pid). The confinement boundary therefore sits **between conhost and the
sandboxed child**, not around the pty as a whole. That is the same trust shape as
today (the pty master is cImp's), and it is why the child's console handles work
at all — but it should be stated in the boundary description rather than
discovered later: *a sandboxed tab's console host is an unsandboxed process cImp
owns, bound to that one pty.*

**6. Cosmetic:** `CreateProcessW` returns success while leaving a stale
`GetLastError` behind — `4390` (`ERROR_NO_TASK_QUEUE`) and `6` were both observed
after `ok=1` calls. Read the return value, never the last error, on this path.

## Implementation shape for Phase B

**`portable_pty` cannot carry the sandbox, and there is no escape hatch.**
Checked against the pinned source (`portable-pty 0.9.0`,
`src-tauri/Cargo.toml:99`):

- `src/win/psuedocon.rs` and `src/win/procthreadattr.rs` are **private modules**
  (`mod`, not `pub mod`) inside a `pub mod win` — `PsuedoCon` and the `HPCON` are
  unreachable from outside the crate.
- `ProcThreadAttributeList::with_capacity(1)` is **hardcoded to one attribute**,
  and `set_pty` is the only setter. There is no seam to add a second attribute.
- `ConPtyMasterPty` exposes nothing beyond the `MasterPty` trait
  (`resize` / `get_size` / `try_clone_reader` / `take_writer`); the `Inner`
  struct holding the `PsuedoCon` is private. No `as_raw_handle` on the master.
- `PsuedoCon::spawn_command` is the sole spawn path and builds its own
  `STARTUPINFOEXW` with the one-attribute list.

**But the crate's traits are public, so the blast radius is one line.**
`PtySystem`, `MasterPty`, `SlavePty`, `Child`, `ChildKiller`, `PtySize` and
`ExitStatus` are all public and implementable. The minimal seam:

1. New module `pty/sandboxed_conpty.rs` implementing
   `portable_pty::PtySystem` — pipes, `CreatePseudoConsole`, the **two-attribute**
   list, `CreateProcessW`, and thin `Read`/`Write`/`Child` adapters over the pipe
   handles and the process handle. The spike's `cmd_ptyrun` (~230 lines with
   diagnostics; ~150 without) is the template, and it is a superset of S1's
   `cmd_run` — one attribute list serves both.
2. `pty/manager.rs:171` picks the system:
   `let pty_system = if sandboxed { sandboxed_conpty::system(&profile) } else { native_pty_system() };`
   Everything downstream — `openpty`, `slave.spawn_command`, `try_clone_reader`,
   `take_writer`, `master.resize` at line 420, `child.process_id()` →
   `guard_pid` at line 227, `clone_killer` — compiles and behaves unchanged,
   because it is all trait-level.
3. **Do not try to reuse `CommandBuilder` on the sandboxed path.** Its
   `cmdline()`, `environment_block()` and `current_directory()` are
   `pub(crate)`, and the Win32 quoting routine `append_quoted` is private;
   reimplementing quoting is a silent-divergence hazard. `manager.rs` already
   holds the raw spec (`spec.binary`, `pre_args`, `extra_args`, `working_dir`,
   `env`, `env_remove`) *before* it builds the `CommandBuilder`, so the sandboxed
   backend should take that spec directly and build its own command line.
   Consequence to write down: `apply_env`'s inherited-env snapshot semantics
   (manager.rs:58-66) must be reproduced in the sandboxed backend, or the two
   paths will disagree about the child's environment.
4. `spawn_gate::with_shared` still wraps the spawn: the sandboxed backend calls
   `CreateProcessW` itself with `bInheritHandles = FALSE`, so it does not need the
   inheritable window — but the gate's *shared* leg is what keeps it off the
   sandbox path's exclusive leg. Wrap it exactly as `native_pty_system()` is
   wrapped today.
5. Reuse Phase A wholesale for the token side: the same stable profile
   (`cimp.worker` or a tab-scoped sibling), the same grant table, the same
   drive-mapping mitigation, the same `guard_pid` job. Nothing about Phase A's
   design needs revisiting for Phase B; Phase B adds a pseudoconsole and an
   attribute-list slot, and nothing else.

Honest boundary statement for a sandboxed tab, unchanged from S1: *writes
nowhere but the root; reads = root + system + each tool's own code/config/caches;
credentials and everything ungranted stay dark* — plus *the console host itself
is cImp's, not the child's*.

## Repro

Scratchpad crate `s3-conpty` (session scratchpad, 2026-08-18), a copy of
`s1-appcontainer` plus `ptyrun` / `consoleprobe` / `readfile`. Representative
invocation:

```
s3-conpty ptyrun --cwd S:\ --job --cols 120 --rows 30 \
  --feed "S:\probe.exe consoleprobe" --feed "@resize" \
  --feed "S:\probe.exe consoleprobe" --feed "exit" -- cmd.exe
```

`--plain` drops the `SECURITY_CAPABILITIES` attribute for the A/B control;
`--order sc` reverses the two `UpdateProcThreadAttribute` calls.

All machine state was reverted: AppContainer profile `cimp.s3.spike` deleted,
`subst S:` unmapped, fixture root deleted, the `C:\Users\Amir\.local` RX ACE
removed and verified absent, `C:\nvm4w` verified to carry no residual ACE (its
grant had been refused), and the one file the *unsandboxed control* wrote into
the profile deleted.
