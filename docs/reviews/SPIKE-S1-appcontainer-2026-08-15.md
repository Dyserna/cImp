# Spike S1 — AppContainer on a `run_command`-shaped child (2026-08-15)

**Verdict: POSITIVE.** AppContainer meets decision 2's confinement bar — true
read denial outside the granted root, not just write denial — with **zero
elevation at any step** on this machine, no new dependencies, and one
significant gotcha (ancestor-chain canonicalization) that has a working
unelevated mitigation. Phase A is implementable on this engine today. S2
(srt-alpha) now has a concrete scorecard to beat; decision 2 stays formally
open until S2 runs, but the bar is high.

Environment: Windows 11 Pro 26200 (this dev machine). Harness: a standalone
crate in the session scratchpad (`s1-appcontainer/`), ~350 lines, using
**`windows-sys 0.59` — the exact crate+version cImp already pins for
`process_guard`**. Required features beyond the current list:
`Win32_Security_Isolation`, `Win32_Security_Authorization`. The
`windows 0.61`/`webview2-com 0.38` type-identity pin (`Cargo.toml:336-343`)
is **untouched** — the milestone's warning about adding
`Win32_Security_Isolation` to the `windows` crate is moot; `windows-sys`
carries everything needed.

## What was demonstrated, in order

| # | Claim | Result |
|---|---|---|
| 1 | `CreateAppContainerProfile` needs no elevation | **Confirmed.** Profile created from a plain user shell; `ERROR_ALREADY_EXISTS` → `DeriveAppContainerSidFromAppContainerName` handles re-runs. |
| 2 | Read denial outside root | **Confirmed.** In-container `type` of an ungranted file and of `%USERPROFILE%\.gitconfig` → `Access is denied` (error 5). This is the criterion that rejects low-integrity-only approaches. |
| 3 | Write denial outside root | **Confirmed** (error 5). RW inside the granted root works. |
| 4 | Directory enumeration | Works on granted dirs (`read_dir` OK); denied outside. |
| 5 | git read+write | **Works** — `log`, `status`, `add`, `commit` — *via the subst mitigation below*. Warnings about unreadable `~/.config/git/ignore` are confinement working, not breakage. |
| 6 | cargo / node / npm | **Work** with RX grants on their install dirs (they live in the user profile, which containers cannot read). npm additionally needs `NODE_OPTIONS=--preserve-symlinks --preserve-symlinks-main` and a cache dir inside the root. |
| 7 | Network default-deny | **Confirmed.** No capabilities ⇒ LAN (`172.21.1.11:12344`) and internet both blocked (curl exit 7). |
| 8 | Capability-gated egress | `internetClient` opens internet **and this LAN** — the NIC is profiled Public, so RFC1918 falls under "internet", and `privateNetworkClientServer` grants nothing here. Capabilities are class-granular only; **per-host scoping needs WFP (S4), exactly as decision 4 anticipated.** |
| 9 | Loopback | **NOT blocked** for profile-created (non-packaged) AppContainers on build 26200 — connected with zero capabilities, no `CheckNetIsolation` exemption present (verified against the exemption list). **The one elevated setup step decision 2 priced in does not exist on this build.** Treat as build-measured, not contractual: probe at runtime, loudly (decision 5). |

## The gotcha: ancestor-chain canonicalization

Three independent API families walk every path component and fail on the
first one the container cannot list — and `C:\` itself is unlistable to
containers (no `ALL APPLICATION PACKAGES` ACE on the drive root):

- `GetLongPathNameW` — used by **cmd.exe** for explicit absolute paths and by
  **git-for-windows' `mingw_getcwd`**. This is why git dies with
  `fatal: Unable to read current working directory: Permission denied`.
- `GetFinalPathNameByHandleW` — `std::fs::canonicalize`; also
  `mingw_getcwd`'s fallback.
- node's `realpathSync` — lstats every ancestor; kills npm's CLI bootstrap
  (mitigated by `--preserve-symlinks{,-main}`).

Crucially this is **not** an OS execute restriction: direct `CreateProcessW`
from inside the container runs any image whose file carries an RX ACE,
regardless of ancestors (bypass-traverse applies to opens; only the
canonicalization APIs enumerate). cmd's "Access is denied" on absolute paths
was this quirk, not the kernel. PATH-search resolution inside cmd is
unaffected — and `run_command` spawns its child **directly** with
`tokio::process::Command` (`run_command.rs:483-521`), no shell, so the quirk
touches only what the child itself does internally.

**Working unelevated mitigation, verified end-to-end:** map a drive letter to
the sandbox root (`subst S: <root>` / `DefineDosDeviceW`) and run the child
with `cwd` inside `S:`. The visible ancestor chain collapses to the granted
root and git/cmd work completely. Caveats: drive letters are finite and
per-logon-session global (the user's other apps see the mapping;
`canonicalize` resolves through it to the real path and still fails — git
does not care).

**Alternative (not run):** the OS-sanctioned pattern observed on this very
machine — `C:\` and `C:\Users` each already carry a third-party capability
SID granted `(S,RD,X,RA)` / `(S,X)` non-inherited (the Edge/Chromium LPAC
pattern). A one-time **elevated** grant of list/attr on ancestor components
to a stable cImp SID would fix all three API families without subst. Setting
ACEs on drive roots must use a non-propagating API — an `icacls` attempt on
`C:\Users\Amir` walks the whole profile and had to be timed out.

## Design consequences for Phase A

1. **One stable profile name** (e.g. `cimp.runcmd`), not per-spawn profiles:
   every grant is an ACL entry keyed to the container SID, so ephemeral
   profiles would re-ACL the toolchain dirs on every spawn and leak
   registered profiles (`CreateAppContainerProfile` persists until
   `DeleteAppContainerProfile`).
2. **Grant table shape** (decision 3) confirmed: project root `(OI)(CI)(F)`;
   toolchain install dirs RX — Program Files tools (git) need nothing (AAP
   RX pre-exists); user-profile tools (`.rustup`, `.cargo`, nvm) need
   explicit RX. `icacls`-style propagation on a big toolchain tree has a
   real one-time walk cost; grant once, keyed to the stable SID.
3. **Spawn integration is a bespoke `CreateProcessW` path** —
   `STARTUPINFOEXW` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` — since
   std/tokio `Command` cannot attach attribute lists on stable Rust. The
   spike's `cmd_run` (~150 lines) is the template; stdio piping and
   `guard_pid` (job object composes with the container — assign after spawn,
   same as today) ride on top.
4. **Loopback needs no user action on this build** — the V32 loopback API
   (which is auth-gated anyway) stays reachable from sandboxed children.
   Probe, don't assume: the classic block may exist on other builds; surface
   per decision 5 if the probe fails.
5. **Egress scoping to specific LAN hosts cannot come from capabilities**
   (all-or-nothing per class, and profile-dependent). Decision 4's precise
   allowlist remains S4/WFP work. Interim honest posture: no-caps =
   everything blocked; `internetClient` = broad egress.
6. `HOME`/`USERPROFILE` for the child should point inside the sandbox root
   (C2's env table already carries them) — git then reads no global config
   and the `~/.config/git/ignore` warnings disappear.
7. **Grant-on-first-use, with a three-tier treatment of tool state**
   (2026-08-15 discussion). `run_command` already resolves the top-level
   program before spawning; at that point: Program Files/Windows ⇒ nothing
   to do (AAP RX pre-exists); user-owned install dir ⇒ cImp stamps a
   one-time RX ACE for the stable container SID, unelevated (verified:
   cargo, nvm, python); `WRITE_DAC` refused (Administrators-owned custom
   dirs, e.g. a `P:\WorkSync` JDK) ⇒ one elevated grant pass, or
   copy-into-root, or unsandboxed-with-badge — the user picks, loudly
   (decision 5). Tool state splits three ways, not two:
   **(a) binaries+libs — grant RX; (b) operational config and caches —
   grant, wherever they live** (withholding them breaks the tool in ways
   indistinguishable from sandbox bugs; git degrades gracefully without
   global config, a private-registry `npm install` does not — this is
   decision 3's "own state dir is not a hole" extended to config);
   **(c) the short known list of pure-credential files** (`~/.ssh`,
   `~/.aws`, `.git-credentials`, `.cargo/credentials.toml`) — explicit
   per-file **deny ACEs keyed to the container SID**, which cost normal
   processes nothing and beat any inherited allow. A workflow that needs
   one of these inside the sandbox (publishing from a child) is a named,
   surfaced, per-tool accepted residual — never a silent grant. The honest
   boundary statement: *writes nowhere but the root; reads = root + system
   + each tool's own code/config/caches; credentials and everything
   ungranted stay dark.*

## Addendum (same day): the general-toolchain matrix — javac, go, dotnet, clang, python

cImp is not a rust/npm shop; the same session extended the inventory to every
other toolchain present on this machine. Every result generalizes to two
rules — (a) **install location decides the grant**, (b) **state dirs get
env-redirected into the root** — plus per-tool quirks:

| Toolchain | Location | Result |
|---|---|---|
| go 1.26 | `C:\Program Files\Go` | **Works out of the box** (AAP RX pre-exists). Full `go build` + run verified with `GOCACHE`/`GOPATH`/`GOTMPDIR` redirected inside the root. |
| javac/java (JDK 21) | `P:\WorkSync\JavaJDK` | Executes, compiles and runs — **but the grant itself was refused unelevated**: the dir is Administrators-owned and `Authenticated Users:(M)` carries no `WRITE_DAC`. Zero-elevation fallback verified: **copy the JDK into the root** (317 MB, 1.4 s) — works completely. The modern JVM is notably robust: a classpath entry whose ancestors are unlistable still loads (`java -cp C:\<granted-but-C:-blocked-chain>` ran fine). |
| dotnet 8 | `C:\Program Files\dotnet` | **Works** with `DOTNET_CLI_HOME` redirected inside the root (+ telemetry opt-out). |
| clang 22 | `C:\Program Files\LLVM` | Executes; **MSVC/SDK header auto-discovery fails in-container only** (host control compiles). Explicit `INCLUDE` env (the vcvars-style paths a build harness sets anyway) → compiles clean. Windows Kits headers under `Program Files (x86)` are AAP-readable. |
| python 3.14 | real install in profile | **Works** with an RX grant on the install dir (same class as cargo/nvm). Emits a benign "Failed to find real location of python.exe" — realpath hitting the ancestor wall, tolerated. pip/site-packages would need cache redirects, untested. |
| python (Store alias) | `…\Microsoft\WindowsApps` | **Broken and not worth fixing** — the appExecutionAlias dir is unlistable profile territory and the alias is a reparse point; PATH search never sees it. Users with only Store Python get a documented failure, not a workaround. |

Two additions to the grant-table design from this pass:

- **Granting needs `WRITE_DAC` on the toolchain dir.** Anything installed
  elevated (or synced with Administrators ownership) refuses an unelevated
  `icacls` grant. The fallback ladder is: AAP already present (Program
  Files) → nothing to do; user-owned dir → grant once; Administrators-owned
  dir → one elevated grant pass **or** copy the toolchain into the sandbox
  root (fast, and the copy inherits the root's ACE).
- **cmd's bare-name resolution of a cwd-relative exe** (e.g. running a
  freshly built `hello-go.exe`) hits the canonicalization quirk even inside
  the subst root; invoking it as `S:\hello-go.exe` works. Same family as the
  absolute-path cmd gotcha, same mitigation.

## What S2 must beat

Zero elevation, zero new deps, ~350-line wrapper, confinement verified by
direct probes, one known gotcha with a working mitigation. srt-alpha's
`windows-install` requires UAC before anything runs (user action still
pending, deferred 2026-08-13), and its maintenance risk is alpha-external
vs. ours-to-own. S3 (ConPTY under AppContainer) remains the open unknown for
Phase B either way — the spike harness can be extended with a
`PSEUDOCONSOLE` attribute next to the capabilities attribute when S3 runs.

## Repro

Scratchpad crate `s1-appcontainer` (session scratchpad, 2026-08-15; rebuild
from this report if the scratchpad is gone — every API named above). All
machine state was reverted: profile deleted, subst removed, fixture dirs
deleted, the two toolchain-dir ACEs removed (`icacls … /remove *<SID>`), the
timed-out `C:\Users` icacls left **no** ACE behind (verified).
