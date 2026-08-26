# V33 — OS Sandboxing & Max Paranoia Mode

**Status:** IMPLEMENTED (2026-08-19) — all four seams on Windows, three on Linux
(Landlock; tabs need an upstream portable_pty hook). Phase A live-verified on
rc.8; the rc.9 live battery found and closed four real defects (below).
Live-verify tracked in #72. GitHub: milestone 6, umbrella #30.

> **⚡ Corrections from the rc.9 live battery (2026-08-18/19) — read these before
> trusting any earlier sentence in this document about what the sandbox can do.**
>
> 1. **A contained process CAN spawn children.** A round-2 claim that it cannot
>    (shipped briefly in `002210c`) was FALSE and is retracted in code. A full
>    bisect reproducing cImp's exact production spawn shape — profile, handle
>    list, `bInheritHandles`, kill-on-close job, minimal env, piped stdio — found
>    grandchildren work under every combination, including a real
>    `cargo → rustc → build script → link.exe` chain.
> 2. **The real rule: `cmd.exe` cannot resolve a DRIVE-QUALIFIED program path
>    inside the container.** `C:\` (the volume root) carries no
>    `ALL APPLICATION PACKAGES` ACE, and cmd opens the drive root when resolving
>    a drive-designated path — so `cmd /c C:\…\tool.exe` is denied before any
>    spawn, while a bare name via `PATH`, a drive-less path, or the sandbox's own
>    mapped drive all run. `dir` and `cd /d C:\…` fail for the same reason;
>    plain file opens do not (bypass-traverse is present and works).
>    **Fix:** the sandboxed check path resolves the program once, rewrites a
>    drive-qualified token to its file name, and leads the child's `PATH` with
>    the resolved directory. Bare tokens keep their spelling (rewriting them
>    would change which `echo` runs); drive-less paths are left untouched.
> 3. **Toolchain state dirs are part of the boundary.** The engine redirects
>    `HOME`/`USERPROFILE` into the sandbox root, so an unset `CARGO_HOME`/
>    `RUSTUP_HOME` resolved against empty scratch and rustup could not choose a
>    toolchain; `~/.rustup` was never granted at all. Granted program dirs now
>    also grant their state dirs RX, and the pointers are appended AFTER the
>    HOME redirect. With that, `cargo check --offline` compiles inside the
>    container.
> 4. **A project overlay must never configure the boundary (SECURITY).**
>    `.cimp/config.json` lives inside a FULL-granted root, so anything running
>    in the project can write it — and `load_readonly` merged it on every
>    MCP-child call. An overlay could set `sandbox.enabled = false`, or name
>    `~/.ssh` in `extra_grant_dirs` and make cImp stamp a durable ACE granting
>    the container read access to credentials. **`sandbox` is now an
>    overlay-banned key (machine/global scope, with a write-through so the
>    setting still saves), and `extra_grant_dirs` is screened at the grant site**
>    on both engines: credential dirs, the user-profile root, volume roots and
>    the Windows directory are refused with an Events row, and the remaining
>    grants still apply. `injection` is reachable by the identical path and is
>    NOT yet banned — open decision, see #72.
>
> **Known residuals, measured:** `link.exe` exits `0xC0000142` in the container
> (a further grant question, not a spawn one); grants are RX, so a *fetching*
> cargo is denied on `CARGO_HOME` (deliberate); interpreter tools whose
> resolution walks to the volume root (semgrep's `scan`) still fail — closing
> that needs machine-wide ACL weakening, which is refused. 7 of the 14 audit
> tools are single static binaries and work sandboxed today; the rest depend on
> a runtime and are candidates for a per-tool `sandbox: required|optional|
> unsupported` declaration when V38 makes tools manifest-driven.
**Amendments:** 2026-08-09 — Phase F's throttling note, made false by V32's
`f8e1097` (checkpoint throttle re-keyed per `(root, tab)`). **2026-08-13 — a
six-agent investigation verified this spec against the live tree and found
several of its load-bearing claims false**; each correction is dated in place
under the claim it corrects — the "Builds on" line below, decision 1, decision 7,
decision 11, Phase D and Phase F — with the original left standing above it. The
four user decisions taken the same day are **locked decisions 12–15**; **2026-08-14
added decisions 16–17 (Settings placement and the master off switch)**, and the
implementation contracts derived from that pass live in
[IMPL-PLAN-V33-sandboxing.md](IMPL-PLAN-V33-sandboxing.md): that file is the
source of truth for the build, this one for the design. **Locked decisions now
run 1–15.** Amendments are
dated in place rather than edited silently, matching the V32 spec's convention.

**Amendment 2026-08-18 (user decision) — RE-SCOPE: the full-environment tier
moves to V36.** Phase C (Max Paranoia Mode), spikes S4 and S5, the podman/bwrap
half of Phase D, and decision 6's WSL2-gated hardened Claude `sandbox.*`
profile are re-homed in
[MILESTONE-V36-sandbox-containerization.md](MILESTONE-V36-sandbox-containerization.md)
(GitHub milestone 10, umbrella #76). Rationale: tier 3 of the ladder below is a
step beyond per-process sandboxing, and its platform story diverges — WSL2 and
Windows Sandbox do not exist on Linux, where the same tier is a rootless
container; V36 carries the Windows (`.wsb`) and Linux (podman/bwrap) legs as
first-class peers. **V33 closes on per-process OS sandboxing alone:** Phase A
(implemented `787ade0`, live-verified 2026-08-18 on v0.52.0-rc.8 in both
network modes), the `run_check`/`audit` seam increment, Phase B (S3-gated tab
spawns), Phase D's Landlock half, and live-verify #72. The moved sections below
(§ Max Paranoia Mode — platform designs, S4/S5, Phase C, decisions 4/9's
Max-Paranoia legs) are left standing per this doc's convention; V36's doc is
authoritative for them from this date.
**Builds on:** the two spawn seams cImp owns — `pty/manager.rs::PtyLaunchSpec:20`
(every AI tab: Claude, OpenCode, future harnesses) and the offload worker's
`run_command` children (`offload/tools/run_command.rs`, plain non-PTY `Stdio`
spawns) — plus the `--settings` overlay injection mechanism
(`tabs/config.rs`, same seam as the statusline overlay) and the Linux
milestone (`docs/MILESTONE-linux-support.md`).

**Amendment 2026-08-13 (investigation, verified against the live tree) — two
claims in the "Builds on" line above are false.** Corrected here rather than
overwritten, because both are load-bearing elsewhere in this document:

- **"the two spawn seams cImp owns" is FOUR.** Beyond `pty/manager.rs:199`
  (every AI tab) and `offload/tools/run_command.rs:258`, agent-initiated process
  execution also happens at **`checks/mod.rs:734`** — `run_check`, which runs its
  command through a *shell* — and at **`audit/runner.rs:1235`**
  (`security_audit`/`quality_audit`). Those last two are reachable both from the
  offload worker and from Claude/OpenCode over MCP, so they carry exactly the
  exposure decision 1 exists to bound. `offload/mcp_host.rs:1230` is a fifth,
  agent-*serving* rather than agent-initiated. See the amendment under decision 1
  for what this does to that decision's tripwire.
- **The Linux milestone link is dead, and the milestone shipped.** The file is
  `docs/completedMilestones/MILESTONE-linux-support.md`. See the amendment under
  Phase D.

**Companion milestone:** [MILESTONE-V32-injection-hardening.md](MILESTONE-V32-injection-hardening.md)
— V32 constrains a compromised model at the tool layer; V33 makes the OS
enforce boundaries the model cannot negotiate with. V32's tool classes inform
V33's network policy (EXTERNAL-capable environments get egress allowlists).
Independent ship order; V32 first is expected.

## Why

Every current containment layer is policy in a process the agent can reach:
allowlists, permission prompts, path checks. Assume the model is compromised
(V32's stance) and the question becomes: what does the OS let the process do?
Today, on native Windows, the answer is "everything the user can" — and
**Claude Code's built-in sandbox does not exist on native Windows** (verified
2026-08-06; docs: macOS Seatbelt, Linux/WSL2 bubblewrap only, "Native Windows
is not supported"). Anything we add is not defense-in-depth redundancy; it is
the only OS layer this machine has.

Scope of confinement (the user-stated goal): agent processes access only the
folder they are started in (plus each tool's own state dirs) and execute only
within it. The **cImp host process is explicitly out of scope** — it needs
`~/.claude/projects` transcripts, model files next to the exe, global
settings, P:\WorkSync deploys. The boundary belongs around the *agent*
children, at the two seams above.

## The tier ladder (context)

- **Tier 0 (today):** app-layer scoping — `run_command` deny-by-default
  allowlist + bare-name/PATH resolution + per-command flag denylists;
  `ToolCtx` allowed roots; harness permission systems.
- **Tier 1 (V32):** taint latch + spotlighting + detection surface.
- **Tier 2 (this milestone):** per-spawn OS sandbox.
- **Tier 3 (this milestone):** **Max Paranoia Mode** — disposable, fully
  isolated environment per tab; nothing survives but the project folder.

Adjacent, already shipped, no work here: **damage recovery** exists as the
Workbench shadow-checkpoint repo (`workbench/shadow.rs` — auto-snapshots on
agent prompts (`Trigger::Prompt`) and activity bursts (`Trigger::Burst`),
Timeline restore with pre-restore safety snapshot). Sandboxing bounds where
an agent can write; the shadow repo is the undo for what it writes inside
those bounds. This milestone inherits the pairing and adds ONE checkpoint
extension — tool-sourced checkpoints, Phase F below. **Rollout prerequisite
(zero code): `workbench.checkpoints` defaults
OFF — enabling it is step one of adopting this milestone's posture.** A
recovery layer that isn't switched on protects nothing; the rollout notes /
release checklist for Phase A must include flipping it on (and the
Settings → Workbench toggle is the only action needed).

## Verified platform facts (2026-08-06)

- Claude Code built-in sandbox: Bash tool + its children ONLY (not MCP
  servers, not hooks, not Read/Edit/Write); macOS/Linux/WSL2; config via
  `sandbox.*` settings; `--settings` is a trusted source allowed to set the
  restricted keys project settings cannot; default READ policy is the whole
  machine (`~/.ssh` readable) unless `filesystem.denyRead` /
  `sandbox.credentials` are configured; silent degradation when bubblewrap is
  missing unless `failIfUnavailable: true`; the model can retry outside the
  sandbox unless `allowUnsandboxedCommands: false`.
- `@anthropic-ai/sandbox-runtime` (`srt`): wraps a WHOLE process (MCP
  servers, hooks, file tools included). Has an **alpha native-Windows
  implementation**: dedicated `srt-sandbox` local user + NTFS ACLs + Windows
  Filtering Platform egress fence keyed on that SID; one-time
  `npx @anthropic-ai/sandbox-runtime windows-install` (UAC). Not referenced
  by Claude Code docs as a supported Windows path — treat as experimental.
  Gotcha: with no valid settings file it starts anyway in default-deny — a
  clean start is not proof config loaded; always pass explicit settings and
  verify.
- WSL2: full built-in sandbox works, but sandboxed commands cannot launch
  Windows binaries or `/mnt/*` paths — a WSL2 tab mode requires the repo to
  live in the WSL filesystem (workflow change; our repos live on `P:\`).
- Windows Sandbox: `wsb.exe` CLI since Windows 11 24H2 (`start`/`exec`/
  `connect`/`ip`/`stop`); **`wsb exec` has no process I/O** (cannot stream
  output); `.wsb` networking is all-or-nothing (no domain allowlist).
- Landlock (Linux): unprivileged kernel path confinement; ABI v1 = 5.13 (fs),
  v4 = 6.7 (TCP bind/connect by port), v6 = 6.12 (scoped signals/abstract
  sockets); TCP-only network control (no UDP/DNS); `landlock` crate
  (kernel-maintainer-owned) does runtime best-effort ABI adaptation;
  inherited by all descendants, irrevocable once applied.
- OpenCode: no first-party sandbox; community `opencode-sandbox` plugin
  wraps bash via `srt` (low confidence, unmaintained-risk — not a
  dependency we take). Our wrapper at the spawn seam covers OpenCode
  uniformly instead.

## Design — locked decisions

1. **Sandbox at the two seams cImp owns; never sandbox the host.**
   `PtyLaunchSpec` gains an optional sandbox spec (like `env_remove`, a
   portable field the platform backend interprets); `run_command` child
   spawns get the same wrapper. No third seam may spawn agent work without
   going through one of these (tripwire test: grep-level assertion on spawn
   call sites, same spirit as `spawn_inject_sig`).

   **Amendment 2026-08-13 (investigation) — the seam count is wrong, and the
   tripwire this decision proposes would have failed its own premise on the day
   it was written.** "The two seams cImp owns" is four — `pty/manager.rs:199`,
   `offload/tools/run_command.rs:258`, `checks/mod.rs:734` (`run_check`, executed
   through a *shell*) and `audit/runner.rs:1235`
   (`security_audit`/`quality_audit`), with `offload/mcp_host.rs:1230` a fifth,
   agent-serving one. The *shape* of the decision survives intact: sandbox at the
   seams cImp owns, never the host. What does not survive is the sentence "no
   third seam may spawn agent work without going through one of these" — a third
   and a fourth already did, and both are reachable from an agent over MCP, which
   is the reachability this milestone assumes is hostile.

   The assertion is therefore retained and **promoted from a guard to a ledger**:
   an exhaustive `(file, reason)` table in code, `include_str!`-asserted, in which
   every spawn site is classified `AgentSpawn` or `HostSpawn` with its reason
   recorded. A grep that merely counts call sites cannot express the distinction
   that matters — host spawns which must **never** be sandboxed are as much a part
   of the contract as agent spawns which must always be, and the
   shadow-checkpoint repo (`workbench/git.rs:247`) is the sharp case: sandboxing
   it breaks restore, i.e. breaks the recovery layer this milestone is paired
   with. The ledger's contract, including the `#[cfg(test)]` exclusion the
   assertion needs to avoid tripping on ~15 files' test-only `Command::new`, is
   `IMPL-PLAN-V33-sandboxing.md` §C1.
2. **DECIDED 2026-08-15: the Windows engine is AppContainer.** Closed by the
   user on spike S1's evidence
   (`docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md`) without running S2:
   S1 met every selection criterion below — read denial outside root, a full
   toolchain inventory with working mitigations, **zero elevation at any
   step**, no new dependency (`windows-sys 0.59` + two features), and the
   engine is ours to own. S2 (srt-alpha) is not run: it needs a UAC
   `windows-install` the user has not performed, it is alpha-external where
   this is first-party, and its verdict could only change the answer by
   beating a bar S1 already cleared. **If srt is ever installed, the report's
   "What S2 must beat" section is the scorecard to re-open this with.**
   Phase A is IMPLEMENTED on this engine (see the phase entry below).

   *Original text, retained because the criteria it names are what the
   decision was measured against:*
   **Windows tier 2 engine is decided by spike, AppContainer vs srt-alpha,
   on `run_command` children first** (no ConPTY involved, output captured —
   the cheap, high-value case). Selection criteria: confinement correctness
   (read denial outside root — low-integrity-only approaches are REJECTED
   because integrity levels don't block reads), toolchain breakage inventory
   (git/npm/cargo), setup burden (loopback exemption `CheckNetIsolation` /
   `windows-install` UAC), maintenance risk (srt is alpha; AppContainer is
   ours to own via the `windows` crate).
3. **Grant lists are curated per tool, not per project.** An agent container
   gets: project root RW, its own state dirs (Claude: `~/.claude`,
   `~/.claude.json`; git: `.gitconfig` read; npm/cargo caches when the
   toolchain needs them), TEMP, read/execute on system + toolchain dirs —
   and nothing else. Granting a tool its OWN state dir is not a hole; the
   win is everything not granted (other projects, `~/.ssh`, browser
   profiles). The grant list is a data table in code, reviewed like the V32
   class table.
4. **Network policy follows V32 classes.** Sandboxed agent environments get
   egress scoped to: the Anthropic API endpoints, cImp's loopback (needs the
   AppContainer loopback exemption or srt's WFP equivalent), and the LAN
   LLM/embedding servers (172.21.1.11). Wide-open egress from a confined
   filesystem is still an exfil channel for whatever IS readable.
5. **Degradation is loud, never silent** (global principle: every signal
   needs a consumer). If the sandbox layer is unavailable (missing
   prerequisite, failed spike path at runtime), the tab/worker surfaces a
   visible "UNSANDBOXED" state (tab badge + `/status` + Tool Activity row) —
   mirroring our `failIfUnavailable` stance toward Claude's own sandbox.
   *Rejected:* fail-closed refusal to spawn — this is a hardening layer over
   a working product, not a gate that bricks tabs on a missing dependency;
   the consumer of the signal is the user deciding whether to proceed.
6. **Hardened Claude sandbox profile ships NOW, platform-gated.** Written
   once into the `--settings` overlay and active wherever the built-in
   sandbox exists (WSL2 today, native Windows if it ever ships):
   `sandbox.enabled`, `failIfUnavailable: true`,
   `allowUnsandboxedCommands: false`, `filesystem.denyRead` on credential
   dirs (`~/.ssh`, `~/.aws`, browser profile dirs),
   `network.strictAllowlist` + `allowedDomains` per decision 4, and the
   `sandbox.credentials` masking block. Restricted keys ride `--settings`
   (trusted source) — never seeded into `~/.claude` (established overlay
   discipline). Note: the built-in sandbox auto-denies writes to settings
   scopes and `.mcp.json`, so the injected config is tamper-proof from
   inside.
7. **Job objects (Windows) / cgroups (Linux) under every agent spawn**
   regardless of which sandbox engine wins: process-tree kill-on-close,
   child-count and memory ceilings. Cheap, orthogonal, closes the runaway /
   orphan cleanup gap.

   **Amendment 2026-08-13 (investigation) — the Windows half already exists; the
   gap is not that job objects are missing but WHERE they do not reach.**
   `process_guard.rs` creates the job (`CreateJobObjectW:89`) with
   `KILL_ON_JOB_CLOSE:95`, and four spawn sites already call `guard_child` —
   `run_command:263`, `checks:739`, `audit/runner:1247`, `mcp_host:1233`. So the
   claim to keep from this decision is the **coverage** claim, not the capability
   claim, and the hole it hides is the one that matters most here:
   **`guard_child` is typed to `tokio::process::Child`, so the PTY child — every
   AI tab — is outside the job entirely.** The remedy is a pid-taking entry point
   beside `guard_child`, assigning `portable_pty::Child`'s `process_id()`; the
   assign-after-spawn race window that this opens is to be **documented, not
   hidden** (decision 5's stance applied to our own layer).

   On Linux the decision is unimplemented rather than partial: `process_guard.rs:46`
   is a no-op and `procutil.rs:61-73` kills only the direct child, so there is no
   process-tree kill at all. "cgroups (Linux)" is restated as the cheaper backstop
   actually wanted at this seam — process-group spawn plus `killpg` in
   `procutil::kill_tree`, and `PR_SET_PDEATHSIG` via `pre_exec` if it fits the
   existing spawn sites without restructuring them. That is also what introduces
   this project's first `[target.'cfg(unix)'.dependencies]`.
8. **Linux tier 2 is Landlock-first, applied `pre_exec` in our own spawn
   path** — no external binaries, no setuid, inherited and irrevocable.
   Filesystem rules per decision 3's grant table; TCP port rules on ABI ≥ v4,
   feature-detected (never hard-required — WSL2/older kernels degrade to
   fs-only, loudly per decision 5). bubblewrap is the optional second layer
   where mount/pid/net namespace isolation is wanted (Ubuntu 24.04+ needs an
   AppArmor profile for unprivileged userns). *Timing:* later phase, lands
   with the Linux milestone; specified now so Windows decisions don't
   foreclose it (the `PtyLaunchSpec` sandbox field is platform-neutral).
9. **Max Paranoia Mode is a per-tab toggle, named exactly that in the UI.**
   Semantics on both platforms: disposable environment, project folder is
   the ONLY persistent surface, egress per decision 4, everything else
   discarded on tab close. It subsumes tier 2 (no need to compose both).
10. **`run_command` children get an explicit minimal environment, not
    inheritance.** cImp's process env can carry API keys and tokens; a child
    of an allowlisted command (or anything it execs) must not see them.
    First verify current behavior (tokio `Command` inherits by default),
    then switch to an explicit allowlist env — PATH, TEMP/TMP, SystemRoot,
    the essentials a build/test probe needs — composed in one function next
    to the spawn. Tab spawns keep their existing `env_remove` discipline
    (they legitimately need more env); the worker's children get the
    stricter build-up-from-nothing model because nothing they run should
    need cImp's secrets. *Rejected:* inherit-and-subtract for the worker —
    a denylist of secret-shaped names is a guess; the allowlist is not.
11. **LAN inference services get authentication.** The llama-servers
    (offload + embedding on 172.21.1.11) and the HTTP MCP endpoints
    (:17201/:17202) currently speak plain unauthenticated HTTP: anyone on
    the LAN can use them or poison what they return — and poisoned
    *embeddings* are the insidious case (silently corrupted semantic search,
    no visible failure). Minimum bar: `--api-key` on both llama-servers
    (native support; key in cImp settings, sent by the warm pool / embedder
    clients) and a shared bearer token on the MCP endpoints (checked by
    those servers, sent by the MCP host). Stronger option (WireGuard/SSH
    tunnel) is documented but not required by this milestone. The trust
    statement — "the LAN segment is not a trust boundary once keys are in
    place" — goes into ARCHITECTURE.md.

    **Amendment 2026-08-13 (investigation) — true, and larger than stated: all
    four LAN services in use are unauthenticated, and the only backend that
    supports auth is the one not in use.** Bearer auth exists today on exactly one
    thing, `OffloadBackendKind::Remote` (`schema.rs:3065`) — which this install has
    configured **empty**. The active backend is `local`, which launches
    `llama-server --host 0.0.0.0 --port 12344` with no `--api-key`, and the
    `Local` variant carries no token field at all. The embedding endpoint
    (`172.21.1.11:12344`) and both HTTP MCP endpoints (`:17201`/`:17202`) are
    likewise unauthenticated. Phase E is therefore not "send a key from a client
    that already has one": it adds the field to `Local`, to `GraphSettings` and to
    `McpServerConfig`, and each of the three carries a trap that fails
    *silently* — a variant-level `#[serde(default)]` (serde's container default
    does not reach enum variants, so omitting it makes every existing settings
    file fail to deserialize), a hand-rolled `Debug` for a struct whose doc
    currently promises "No secrets here", and a `config_sig` entry without which
    editing the token in Settings never reconnects the server. All three are
    enumerated as `IMPL-PLAN-V33-sandboxing.md` §C7.

    Two adjacent corrections from the same pass, recorded here because briefs
    around Phase E carry them even though this document never states them: the
    settings schema constant is **`CURRENT_SCHEMA_VERSION`** (`schema.rs:163`),
    not `settings_version`; and there is **no `/v1/models` call and no
    non-streaming chat path anywhere in the tree**, so neither is a site that
    needs a bearer header attached to it.

12. **Stages 1–3 — everything with no OS dependency — are built NOW; spikes
    S1/S2 run after (user decision 2026-08-13).** The work that depends on no
    unanswered platform question is the majority of this milestone's surface: the
    decision-1 seam ledger, the decision-10 minimal environment, decision 7's Job
    Object coverage including the PTY child, Phase E's client side, Phase F end to
    end, and the app-layer half of the carried V32 findings below. None of it
    changes shape depending on whether AppContainer or srt-alpha wins.

    *Accepted cost, and it is the whole of the argument against:* **decision 2
    stays open, so Phases A and B cannot close.** A `run_command` child gets a
    minimal environment and a job object but no OS confinement; a tab gets a job
    object and nothing more. A green stage-3 tree read as "Phase A done" would be
    a misreading, and the tier ladder above still places every one of these
    deliverables at tier 0–1.
    *Rejected:* running S1/S2 first. They gate two phases and nothing else, and
    holding six work-streams behind a spike whose verdict changes none of them
    buys ordering certainty with idleness.

13. **F-5's fail-open STAYS; only the `cwd` half of the M-7 residual is fixed
    (user decision 2026-08-13).** V32's locked posture — a tool call must never
    fail for lack of identity — is unchanged. What is fixed is narrower and is a
    real execution primitive rather than a reporting one: `/context/post_edit`
    runs the project's user-vetted check commands in a directory named by the
    **request body**, with no ancestor check (`loopback.rs:6474-6479`).
    `/audit/run` already carries the pattern to copy (`:4709-4718`) — roots derive
    from the configured tabs and the served root, **never from the request**. The
    siblings `/context/should_read` (`:5286`) and `/context/compaction` (`:5372`)
    execute nothing and are deliberately left alone; that is recorded rather than
    left implicit, so the next reader does not mistake their absence for an
    oversight.

    *Accepted cost:* an identity-less caller is still **admitted** to the route.
    The hole becomes "may run the vetted checks, but only inside a root cImp
    already serves" — narrowed, not closed.
    *Rejected:* fail-closed everywhere. Four live behaviours currently rest on
    that fail-open — `loopback.rs:3076`, `graph/mcp.rs:752`, M-8's residual and
    M-7's second residual — so flipping it is not a hardening increment but a
    behaviour change across four surfaces, taken in the same tree that is about to
    grow three stages of new code.

14. **The yara-x/wasmtime exposure is V33 WORK, not an accepted residual (user
    decision 2026-08-13).** User-authored `rules.d/local/*.yar` files are compiled
    and executed under a wasm sandbox that V32's own review states **"is not the
    boundary"**, against 16 open wasmtime advisories. The bar is binary and either
    half discharges it: **either that sentence stops being true, or the capability
    becomes opt-in with a user-visible signal.** Investigate and propose before
    implementing — this decision fixes that there is to be a fix, not what it is
    (`IMPL-PLAN-V33-sandboxing.md` §C9).

    *Rejected:* recording it as an accepted risk. The review that raised it warned
    in the terms this decision adopts verbatim — folding it into an unrelated
    acceptance would be **"laundering a deferral into an acceptance"**, which is
    the shape global principle 10 names: a finding sitting in a backlog while its
    symptom is live is a process failure, not a detection failure.
    *Accepted cost:* it is scope this milestone did not previously carry, and it
    is the one stage-3 item whose size is genuinely unknown before the
    investigation returns.

15. **No prerequisite user actions are planned around; the code lands first (user
    decision 2026-08-13).** Windows Sandbox stays disabled, `srt` is not
    installed, and the LAN servers keep running with no `--api-key` for now. This
    milestone is written as though those states persist, and nothing in stages 1–3
    asks the user to change a machine.

    *Accepted cost, in three parts:* **Phase C cannot start** (no Windows Sandbox
    means no `.wsb` to generate against, and S5 has nothing to bootstrap),
    **spike S2 cannot run** (`srt` absent, so decision 2's comparison is
    one-sided), and **Phase E's live verification — item 8 below — is not
    meaningful until the servers hold keys**: a client that sends a bearer token
    to a server which ignores it proves nothing. Phase E's client side lands
    regardless and is inert against a keyless server by contract (§C7: an empty
    token field sends no header), so the day keys are deployed is a settings edit
    rather than a release.
    *Rejected:* gating the code on the deployment. Client and server key are
    independent changes, and the ordering that costs least is the one that leaves
    nothing to build when the decision to deploy is taken.

16. **Every sandboxing setting goes in ONE top-level Settings category of its
    own (user decision 2026-08-14).** Decided before any such setting exists, so
    that none is ever placed by default.

    As of 2026-08-14 **there is no sandboxing setting in the schema** — verified,
    not assumed: no `sandbox`, `paranoia`, `unsandboxed`, `landlock`,
    `appcontainer` or `job_object` key exists. Stage 1's containment (the seam
    ledger, the `run_command` env allowlist, job-object coverage, the Linux
    process-group backstop) is entirely code with no user-facing switch, and
    stage 3b's compile limits are constants, not settings.

    The settings that are coming, and would otherwise scatter across three
    unrelated sections: the **UNSANDBOXED** degradation state (decision 5, Phase
    A), the **engine selection** (decision 2, Phase B), the **hardened Claude
    `sandbox.*` profile** (decision 6, Phase B), and the per-tab **Max Paranoia**
    toggle (decision 9, Phase C — which this spec already requires be named
    exactly that in the UI). Left to land where each phase happens to touch, they
    would go to Tabs, Local task offload and Per-tab overrides respectively.

    This is **F-18's lesson applied before the fact rather than after it.** V32
    shipped its injection controls scattered across sections, discovered that 36
    pointers and 15 user-visible strings named a "Tools" section that did not
    exist, and had to consolidate them into a top-level `Injection protection`
    category (`SettingsApp.svelte:4955`) as a fix. Sandboxing is the same shape
    of feature — a posture the user turns on, not an option belonging to any one
    subsystem — so it gets the same treatment, and gets it first.

    **Explicitly NOT in this category — Phase E's three `auth_token` fields.**
    They are per-service credentials, not containment controls: a bearer token
    belongs beside the URL it authenticates (`offload.backends[].kind.auth_token`
    under *Local task offload*, `graph.embedding_auth_token` under *Code
    Intelligence*, `offload.mcp_servers[].auth_token` on the MCP row). The
    category's membership test is *"does this control the boundary the OS
    enforces?"*, not *"did V33 add it?"*.

    **Also NOT moving: `workbench.checkpoints`.** Phase F depends on it, but it
    is the shadow-repo master switch shared by the prompt and burst triggers and
    predates this milestone; it stays under *Workbench*. Named here so its
    absence from the sandboxing category reads as a decision rather than an
    oversight.

    *Accepted cost:* a per-tab toggle (Max Paranoia) rendered in a global
    category needs a tab selector or a per-tab override row, which is more UI
    than dropping it into *Per-tab overrides* would have been. Judged worth it:
    a user hardening the app should find every containment control in one place,
    and V32 has already paid for the alternative.
    *Rejected:* folding these into `Injection protection`. V32 constrains a
    compromised model at the tool layer; V33 makes the OS enforce a boundary the
    model cannot negotiate with. Those are different guarantees with different
    failure modes, and merging them would let a user reasonably believe that
    switching one on delivers the other.

17. **Sandboxing has a master off switch, and it governs the OS layer only
    (user decision 2026-08-14).** The user must be able to turn the whole
    sandboxing posture off. Off means: no per-spawn OS wrapper (whichever engine
    decision 2 selects), no Landlock rules, no Max Paranoia, and **the hardened
    `sandbox.*` profile of decision 6 is not written into the `--settings`
    overlay at all** — not written-and-disabled, absent, so nothing inside the
    tab can read a policy that is not in force.

    **What the switch does NOT reach, and why.** Three stage-1 controls stay
    unconditional, because each is something other than confinement:
    - **Job-object kill-on-close** (`process_guard`, incl. the PTY coverage added
      in stage 1b) is process *lifecycle correctness*. Making it switchable would
      not relax a boundary, it would reintroduce orphaned agent children on hard
      kill — a bug, not a freedom.
    - **`run_command`'s minimal environment** (decision 10). A child of an
      allowlisted command has no business seeing cImp's API keys or loopback
      token under any posture. This one is the closest call on the list — it *is*
      a containment control — and it is kept on the ground that the thing it
      withholds is credentials rather than capability. **If a real build probe is
      ever found that needs a variable the allowlist withholds, the answer is to
      add that variable to the table with a recorded reason, not to switch the
      table off.**
    - The loopback root allowlist, the `(consumer, tab)` predicate and the
      plugin's `fetch` binding are injection-layer fixes that predate any OS
      boundary; they are governed by V32's own settings, not this switch.

    So the switch's membership test is the same as decision 16's category test:
    *does this control the boundary the OS enforces?*

    **Off is a distinct state from unavailable.** Decision 5 already requires a
    visible `UNSANDBOXED` surface when a prerequisite is missing. That surface
    now carries **two** states, and consumers must not collapse them: *off by
    user choice* and *unavailable — a prerequisite is missing*. Conflating them
    is precisely how a broken prerequisite hides behind a deliberate setting, and
    it is the failure decision 5 exists to prevent. Neither state nags and
    neither blocks a spawn; no surface may ever claim containment it is not
    delivering.

    *Accepted cost:* an extra state threaded through every consumer of the
    sandbox status (tab badge, `/status`, the Tool Activity row), and a switch
    that a compromised model would very much like flipped — mitigated only by it
    being a settings write, which the model cannot perform through any tool cImp
    exposes. **Whoever implements this must verify that claim rather than
    inherit it**; the V32 run found a comment standing in for a check six times.
    *Rejected:* a fail-closed posture with no off switch. This is a hardening
    layer over a working product, not a gate that bricks tabs — the same argument
    decision 5 already makes for the unavailable case, and the same one V32's
    decision 16 makes about a control that gets answered by switching the whole
    feature off.

## Carried V32 findings — corrected filing (2026-08-13)

The 2026-08-08 V32 review's disposition parked **H-7 and F-4…F-8** in "V33 /
OS-containment territory"
(`docs/reviews/code-review-V32-2026-08-08.md:139-140`). The 2026-08-13
investigation contradicts that filing — and so, on inspection, does the review:
its own disposition section names **only H-7 and M-16** as properly V33 (`:1455`).
**Five of the six need no OS sandboxing at all.** They were filed by adjacency to
this milestone rather than by mechanism, and left there they would sit behind
spikes that have no bearing on them.

Per finding, what each actually is:

- **F-4 — `(consumer, tab)` is verified nowhere.** An app-layer predicate:
  `is_configured_tab` (`loopback.rs:1556`) is agent-agnostic. It becomes
  consumer-scoped, and the empty-list availability floor is **preserved but
  narrowed** — it applies when the *asserted consumer* has zero configured tabs,
  not when any consumer does. No OS layer appears anywhere in that fix.
- **F-5 — `/graph_run` and `/mcp/call` share H-8's tab half.** The review filed
  it as "a decision, not a bug", and it was exactly that; the decision is now
  taken as **locked decision 13** above. Nothing carries forward except that
  decision's `cwd` half.
- **F-6 — H-2's decode proof degrades silently if the CLI drops `sessionId`.** A
  drift canary, which is a detection concern and not a containment one.
  Sandboxing the process it watches would not make its silence louder; the
  deliverable is a consumer for the signal, per global principle 3.
- **F-7 — auto-injection still pushes source signatures into a contaminated
  tab. DECIDED NOT TO FIX, and that decision is not reopened here.** Injection is
  a **push** channel keyed on the *user's* prompt: the model cannot request it,
  cannot steer which file it names, and it crosses no gate by construction.
  Cutting it would degrade auto-injection for every clean session in order to
  close a channel the attacker does not control. Planning it as V33 work would
  re-litigate a recorded decision. What the row is for is the inference it
  blocks — *"after H-1 a contaminated tab never sees a source line again"* is
  **false** — and that value is preserved by leaving it recorded, not by
  scheduling it.
- **F-8 — a denied URL still leaks its hostname to DNS.** Inherent rather than
  introduced: deciding whether a *name* points into a denied *address* range
  requires resolving the name, so resolution necessarily precedes the verdict. No
  sandbox removes that ordering, and no phase here would. The deliverable is
  **honesty in the wording** — the activity row and the user-facing story both say
  *denied* without qualifying what already left the machine — plus, optionally, a
  literal-IP fast path that skips resolution when there is no name to resolve.
  That fast path optimises the honest case; it does not fix the named one.
- **H-7 — a cloned repo's `opencode.json` is executed configuration.** The one
  finding the review's disposition line does place here, and it splits. The
  **cheap, app-layer half lands now**: the OpenCode plugin template binds `fetch`
  into a private module-scope constant at load, so a later `globalThis.fetch`
  swap cannot neuter the beacon or the gate, and the additive-posture doc at
  `tabs/config.rs:2302-2308` is corrected to stop presenting that posture as
  benign. **`OPENCODE_DISABLE_PROJECT_CONFIG` is NOT shipped in this run** — its
  population question is entangled with F-31's default-off gate, so shipping it
  alone would imply containment the gate does not deliver. It is recorded as
  **owed**, which is the reason for writing this list down rather than absorbing
  it into a phase.

Net effect on scope: F-4, F-5's `cwd` residual and H-7's cheap half are stage-1/2
work in this milestone; F-6 and F-8 need no phase here and are gated by none;
F-7 is closed and stays closed.

## Max Paranoia Mode — platform designs

### Windows: Windows Sandbox via `wsb.exe` (24H2+)

- cImp generates a `.wsb`: `MappedFolders` = the project root (RW) and a
  read-only bootstrap folder; `LogonCommand` runs the bootstrap script.
- **Transport is SSH, not `wsb exec`** (no process I/O — verified). The
  bootstrap installs/starts OpenSSH server inside the sandbox with a
  per-boot generated key from the bootstrap folder; cImp gets the address
  via `wsb ip` and the tab's PTY command is simply `ssh.exe ...` — the PTY
  layer already treats it like any command. Claude/OpenCode + toolchain are
  installed by the bootstrap (or staged in the read-only mapped folder to
  avoid re-downloads).
- **Egress:** `.wsb` networking stays Enabled; scoping is host-side WFP
  rules on the sandbox's NAT adapter allowing only decision-4 endpoints.
  (Same WFP ground the srt-alpha walks — spike S4 covers both.)
- Teardown: `wsb stop` discards the whole environment. Nothing an injected
  agent did survives except writes into the mapped project folder — which is
  exactly the surface the user reviews anyway (git diff).
- Prereqs: Windows Pro + virtualization (dev machine qualifies); document
  that GPU is absent inside (irrelevant: LLM/TTS/STT all run host-side or on
  LAN).

### Linux: rootless podman, devcontainer-pattern

- Reference design is Anthropic's own devcontainer (init script with
  iptables egress allowlisting) — adapted, not depended on: rootless podman
  (no daemon, no root), project bind-mounted, egress via the iptables
  pattern or a proxy sidecar, container removed on tab close.
- *Rejected:* microVMs (Firecracker/cloud-hypervisor) — isolation beyond
  need at real operational cost; container + egress allowlist already
  exceeds the Windows Sandbox bar.
- WSL2 bonus: this same design is runnable from Windows today for a repo
  living in WSL — a stepping stone before native Linux support lands.

## Phases & spikes

- **S1 (gate for Phase A):** AppContainer wrapper on a `run_command` child —
  read denial outside root, loopback exemption workflow, git/npm breakage
  inventory.

  > **Amendment 2026-08-15 (spike run) — S1 is DONE, verdict POSITIVE:**
  > `docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md`. Read denial outside
  > root confirmed; **no elevation at any step** — profile creation is
  > unelevated and, on build 26200, profile-created AppContainers are **not
  > loopback-blocked**, so the `CheckNetIsolation` step decision 2 priced in
  > does not exist here (probe at runtime, per decision 5). git/cargo/node/npm
  > all work, git via a `subst`-drive mitigation for the one real gotcha
  > (ancestor-chain canonicalization: `GetLongPathNameW`/
  > `GetFinalPathNameByHandleW`/node `realpathSync` walk ancestors and die on
  > unlistable `C:\`). `windows-sys 0.59` + two added features suffice — the
  > `windows 0.61` pin is untouched. Decision 4's per-host egress scoping is
  > confirmed to need WFP (S4): capabilities are class-granular and this LAN
  > falls under `internetClient` on a Public-profile NIC. Decision 2 stays
  > open only until S2 runs; the report states what srt-alpha must beat.
- **S2 (gate for Phase A):** srt-alpha on the same child — `windows-install`
  UX, config-actually-loaded verification, WFP egress behavior. S1 vs S2
  verdict picks the Windows engine (decision 2).
- **S3 (gate for Phase B):** ConPTY under the chosen engine (tab spawns).
  AppContainer+ConPTY has no known precedent either way; srt-alpha inside a
  PTY is undocumented. Negative verdict ⇒ tabs stay tier-1-only on native
  Windows and Max Paranoia Mode becomes the only tab-level OS boundary
  (raising its priority).
- **S4 (gate for Phase C):** WFP egress scoping — one implementation serving
  both the srt path and the Windows Sandbox NAT adapter.
- **S5 (gate for Phase C):** Windows Sandbox bootstrap — OpenSSH via
  `LogonCommand`, `wsb ip` + ssh tab end-to-end, agent install time to first
  prompt (if minutes, add the staged-tooling mapped folder).
- **Phase A — worker children sandboxed** (chosen engine + job objects +
  loud degradation + the decision-10 minimal env, which lands here even if
  the engine spikes drag — it has no OS dependency). Ships alone; immediate
  value composing with V32's latch.

  > **Amendment 2026-08-15 — IMPLEMENTED on AppContainer.** `src-tauri/src/
  > sandbox/` (`mod.rs` platform-neutral, `windows.rs` the engine), wired at
  > the `run_command` seam, opted in from `Settings::sandbox` by the in-app
  > worker. **Default off** — the grant ladder needs to soak on real machines
  > before it can be on by default, the same posture
  > `workbench.checkpoints` shipped with. What landed:
  >
  > - **Profile:** one *stable* `cimp.worker` AppContainer (grants are ACL
  >   entries keyed to its SID, so a per-spawn profile would re-ACL toolchain
  >   dirs on every call and leak registrations). Created unelevated.
  > - **Grant-on-first-use, three tiers** (decision 3): Program Files/Windows
  >   need nothing (`ALL APPLICATION PACKAGES` pre-exists); user-owned dirs
  >   get a one-time inheritable ACE via `SetEntriesInAclW` +
  >   `SetNamedSecurityInfoW`; a dir cImp lacks `WRITE_DAC` on fails the whole
  >   prepare, so the child runs **unsandboxed and loud** rather than
  >   half-confined. Each first grant is recorded — it durably changes the
  >   user's machine.
  > - **Drive mapping:** a refcounted `DefineDosDeviceW` per sandbox root, so
  >   the child's cwd is a drive root and the ancestor-canonicalization
  >   gotcha (git's `mingw_getcwd`, node's `realpathSync`) never fires.
  > - **Spawn:** bespoke `CreateProcessW` + `PROC_THREAD_ATTRIBUTE_SECURITY_
  >   CAPABILITIES`, piped stdio drained on two threads, timeout, and
  >   `guard_pid` for job membership — so decision 7's kill-on-close holds on
  >   the sandboxed path exactly as on the plain one.
  > - **Decision 17 enforced structurally:** `SandboxCfg` carries *only*
  >   OS-boundary knobs, so the master switch has no reach into the job
  >   object, the C2 minimal environment or the injection-layer controls; a
  >   test destructures the struct so a future field that violates this is a
  >   review event.
  > - **Decision 5's surface:** a new `sandbox` activity kind with its own
  >   retention lane, carrying both negative states distinctly, deduplicated
  >   by reason per session.
  > - **Decision 16's category:** one new top-level `Sandboxing` section in
  >   Settings, sibling to `Injection protection`.
  >
  > ~~**Still open in Phase A, deliberately:** the layer covers `run_command`
  > only — the `run_check` (`checks/mod.rs`) and audit (`audit/runner.rs`)
  > agent seams still spawn plain~~ — **CLOSED 2026-08-18 (seam increment).**
  > All three model-reachable seams now share the one engine, switch, Events
  > lane and wedge backstops. Mechanics that differ per seam: `run_check`
  > spawns the shell with the check's command appended VERBATIM
  > (`SpawnRequest::raw_tail` — `cmd /C` tails cannot go through CRT arg
  > quoting), grant inference resolves the check command's first token via
  > PATH, nested `CheckDef::cwd` maps through `Prepared::cwd_under`, and rows
  > are keyed by the configured check name. The audit seam grants the
  > container RW on `%TEMP%\cimp-audit` when (and only when) the tool's
  > transport is a report file, cancel terminates the sandboxed child via a
  > polled flag (the future is never abandoned — dropping `Prepared` unmaps
  > the subst drive under a live child), and rows read `audit:<tool>`.
  > Accepted residuals, on record: diagnostics printed by a sandboxed tool
  > name `S:\…` paths and skip prefix-relativization (cosmetic, same class as
  > `run_command`'s); a failed `%TEMP%` dir creation degrades the audit seam
  > to loud-unsandboxed via the grant-ladder contract rather than refusing.
  > Still unproven live: a real `git`/`cargo` probe under a sandboxed
  > `run_check` on a machine other than the spike's fixture — live-verify
  > below.
- **Phase B — tab spawns sandboxed** (S3-gated) ~~+ hardened Claude profile in
  the overlay (decision 6 — can ship with Phase A since it is
  platform-gated config)~~ (decision 6 moved to #76).

  > **IMPLEMENTED 2026-08-18.** S3 ran POSITIVE the same day
  > (`docs/reviews/SPIKE-S3-conpty-appcontainer-2026-08-18.md`: dual
  > `PSEUDOCONSOLE`+`SECURITY_CAPABILITIES` attributes work in either order;
  > confinement token-proven in child and grandchild through the pty;
  > `claude.exe` ran sandboxed in a ConPTY). As built:
  > - **Backend:** `pty/sandboxed_conpty.rs`, a cImp-owned ConPTY spawn
  >   implementing portable_pty's public traits (its own ConPTY internals are
  >   private); portable_pty stays byte-identical for plain tabs.
  >   `CONPTY_FLAGS = 0x6` — parity with the plain path minus only the
  >   `INHERIT_CURSOR` DSR-deadlock bit, const-asserted both ways.
  > - **Policy:** `sandbox/tabs.rs` (platform-neutral). New `sandbox.tabs`
  >   switch, default OFF, effective only with the master switch, spawn-baked
  >   (`spawn_inject_sig` slot). AI-tool tabs only; shell tabs are the user's
  >   own hands. `internetClient` is unconditional for sandboxed tabs — an AI
  >   CLI without egress is a bricked tab; per-host scoping is V36/WFP.
  > - **Grants:** per-harness table with a reason per row (Claude: `~/.claude`
  >   RW, `~/.claude.json`(+backup) as FILE ACEs, `~/.local/share/claude`
  >   READ-ONLY on purpose — an agent that can rewrite its own image persists
  >   across the boundary, so in-tab auto-update is refused; OpenCode: its
  >   three XDG dirs RW). Never `~/.ssh`, never Credential Manager, never
  >   `%USERPROFILE%` itself (test-enforced). HOME stays REAL; TEMP/TMP go to
  >   per-tab scratch under the mapped drive.
  > - **Env:** the sandboxed child reads the RESOLVED env back out of the same
  >   `CommandBuilder` as the plain path (which re-reads the Environment
  >   registry keys and concatenates system+user `PATH` — a hand-rolled
  >   snapshot would silently diverge). Residual: non-UTF-8 env vars don't
  >   reach a sandboxed child.
  > - **Degradation = Phase A semantics** (off/unavailable → plain + loud skip
  >   row; wedged prepare → refusal; Win32 failure after grants → denial row +
  >   visible tab error, never a silent plain retry). `Prepared`/drive guard
  >   live for the tab session; the subst mapping is refcounted per root.
  > - **Documented consequences, accepted:** a sandboxed tab sees `S:\…`, so
  >   Claude keys per-project state under a different slug than the same tab
  >   unsandboxed (toggling the switch looks to Claude like a new project);
  >   conhost runs OUTSIDE the container (the boundary is around the child,
  >   not the pty); a `~/.claude.json` atomic-rewrite (temp+rename) would shed
  >   its file ACE mid-session — live-verify item, no static answer.
- **Phase C — Max Paranoia Mode, Windows** (S4+S5-gated): `.wsb` generation,
  bootstrap, SSH tab wiring, WFP scoping, teardown, UI toggle named
  "Max Paranoia".
- **Phase D — Linux tier 2 + Max Paranoia** (rides the Linux milestone):
  Landlock `pre_exec` + grant table; ~~podman mode; nesting spike (Claude's
  bwrap starting under our Landlock wrapper — expected fine, unverified)~~
  (moved to #76).

  > **Landlock half IMPLEMENTED 2026-08-18** (`sandbox/linux.rs`, `landlock
  > 0.4.7`, the same three settings fields govern both engines — no new keys).
  > As built:
  > - **Probe in the parent, before any fork:** the raw
  >   `landlock_create_ruleset(NULL, 0, VERSION)` syscall (the crate's own probe
  >   is private) — unsupported kernel ⇒ `Plain(Unavailable)`, loud. The ruleset
  >   is built PRE-fork for exactly the probed ABI (requested == enforced by
  >   construction); `pre_exec` only calls `restrict_self` (allocation-free,
  >   verified) and FAILS THE SPAWN on any error or a `NotEnforced` status —
  >   between "sandboxed" and "error" the code chooses error; loud-plain exists
  >   only via the pre-fork Unavailable.
  > - **Grant tiers:** RW = root + hint full-dirs; `/dev` = read + WriteFile/
  >   Truncate/Ioctl (so `2>/dev/null` works) but never create/unlink; RX =
  >   the system-dirs list + program parents + extra_grant_dirs, existence-
  >   filtered, widest-tier dedup. NOT granted: `$HOME` (so `~/.ssh`, other
  >   projects), `~/.cargo`/`~/.rustup` (opt-in, denial rows otherwise), `/tmp`
  >   (TMPDIR/HOME redirect into the root).
  > - **Network:** `allow_network=false` + ABI≥4 ⇒ deny-all TCP bind+connect.
  >   **UDP — and thus direct-socket DNS — is never restricted**; stated in the
  >   rows and `posture()`, never implied away. `NAME_RESOLUTION_IS_A_BOUNDARY_
  >   SIGNAL = cfg!(windows)` keeps the DNS-failure classifier truthful per
  >   platform. Landlock refuses scoped TCP with **EACCES** (not EPERM), so the
  >   socket markers are operation-naming phrases and the classifier checks
  >   socket before filesystem.
  > - **Tabs on Linux: out of scope** (portable_pty exposes no pre_exec hook and
  >   its unix spawn registers its own; a small upstream ask — `pre_exec` on
  >   CommandBuilder or a public `as_command()` — would unlock it; recorded in
  >   `sandbox/tabs.rs`). The tab plan on Linux degrades to plain with a loud
  >   skip row.
  > - **Verification:** Linux-half type-checked locally via a scratchpad
  >   cross-check harness (clippy-clean for the linux target); live enforcement
  >   tests are Linux-CI-only and SKIP LOUDLY on a Landlock-less kernel — a
  >   green run must show them exercised, not skipped. The ABI-4 TCP-denial leg
  >   has NO test route (dash has no /dev/tcp; curl not guaranteed) — it needs
  >   the in-app battery, tracked in #72.

  > **Amendment 2026-08-13 (investigation) — "rides the Linux milestone" is
  > stale: the Linux port SHIPPED.** Full GPU parity on Ubuntu 24.04, a
  > `build-linux` job in `release.yml:762`, tarballs published; the spec moved to
  > `docs/completedMilestones/MILESTONE-linux-support.md`, which is why the link
  > in the header block above resolves to nothing. There is no longer a milestone
  > for this phase to ride, so its gating is restated as three concrete items:
  >
  > - **(a) a Linux `cargo test` CI job.** `tests.yml:63` and `clippy.yml:57` are
  >   `windows-latest` only, so a Linux-only break surfaces at **tag** time — the
  >   same fail-late shape V32's locked decision 40 exists to prevent, arriving
  >   through a different door.
  > - **(b) the `process_guard`/`procutil` Linux backstop**, per decision 7's
  >   amendment: today there is no process-tree kill on Linux at all, so
  >   Landlock would confine a tree that nothing reliably reaps.
  > - **(c) `docs/LINUX-VALIDATION.md`**, the hardware runbook, which stands at
  >   **0 of 12 items ticked**.
  >
  > Phase D is blocked on exactly those three and on nothing about the platform
  > itself.
- **Phase E — LAN service authentication (decision 11).** Independent of
  every spike; can ship first. `--api-key` on both llama-servers + key
  fields in cImp settings + client wiring (warm pool, embedder, MCP host);
  bearer token on the :17201/:17202 MCP endpoints; ARCHITECTURE.md trust
  statement.
- **Phase F — tool-sourced checkpoints (recovery attribution).** New
  `shadow::Trigger::Tool` + a `source` metadata field
  (`harness:tool_name` — `claude:Bash`, `offload:run_command`,
  `opencode:edit`); the checkpoint fires immediately BEFORE a
  filesystem-mutating tool call runs, so the Timeline attributes damage to
  the exact call and rewinds to just-before-it. Which tools qualify comes
  from the V32 class table gaining a `mutates_fs` attribute (single source
  of truth — a future tool marked `mutates_fs: true` gets pre-checkpoints
  automatically, no separate registry to forget). Fire seams: worker
  dispatch + loopback `/graph_run`//`/mcp/call` (all proxied tools),
  an observe-only `PreToolUse` shim for Claude-native Edit/Write/Bash
  (same hook family as the read advisor / notify hook), OpenCode via
  `tool.execute.before` — the SAME unverified hook as V32 spike E2 (one
  spike answers both; negative ⇒ OpenCode keeps prompt+burst coverage
  only, documented). No new throttling: shares `checkpoint_min_gap_s` and
  the identical-tree dedupe.

  > **Amendment 2026-08-09 (V32 `f8e1097`).** The parenthetical here used to
  > read "(first call after a prompt no-ops naturally)". That was true when the
  > `checkpoint_min_gap_s` throttle was keyed per **root**; `f8e1097` re-keyed
  > it per **(root, tab)** — see `workbench::CheckpointKey` and
  > `Workbench::maybe_snapshot`. A Phase F pre-tool checkpoint carries a tab,
  > so it lands in that tab's bucket: still debounced by that tab's own
  > prompt-tap, **no longer** debounced by a different tab's. The
  > identical-tree dedupe is unaffected — `shadow::snapshot` compares against
  > the last checkpoint's tree regardless of bucket, so a second tab's first
  > pre-tool checkpoint over an unchanged tree still returns that existing
  > checkpoint and commits nothing.
  >
  > **Design consequence for whoever schedules Phase F** (flagged, not
  > redesigned): the worst case is N tabs on one root each taking their own
  > pre-tool checkpoint inside one `checkpoint_min_gap_s` window, where the
  > pre-`f8e1097` reading of this line promised one. Every one of those is
  > deduped away *if* the tree is unchanged, so the cost only appears when the
  > tabs are genuinely interleaving edits on a shared root — which is also the
  > case where per-tab attribution is the point of the feature. Decide
  > explicitly whether that is the wanted behaviour, or whether the tool
  > trigger should consult the root-wide gap instead of its tab's; do not
  > assume the old sentence still covers it.

  Metadata-format care: the fields parser must tolerate a missing `source`
  (old checkpoints) — and old readers parse `"tool"` as `Manual`, an
  accepted degradation. Timeline UI renders the source on the row.

  > **Amendment 2026-08-13 (investigation) — four of this phase's premises are
  > already satisfied, and one of its two loopback fire seams would fire
  > nothing.** Phase F was specified as more work than it is, and in one place as
  > less certain than it is. In the order the paragraphs above raise them:
  >
  > - **"the V32 class table gaining a `mutates_fs` attribute" already landed in
  >   V32.** It is `toolclass.rs:157-179`, its accessor sits at `:502-508` behind
  >   `allow(dead_code)`, and its doc comment `:161-166` names *this phase* as the
  >   consumer that will retire that attribute. Four rows are already true —
  >   `run_command:217`, `Edit:371`, `Write:372`, `Bash:373`. The single source of
  >   truth this phase asks for is a table to **read**, not one to build; what
  >   remains is extending it to the OpenCode native table and giving `MultiEdit`
  >   the row it never had.
  > - **The loopback fire seams would fire ZERO checkpoints.** No tool served by
  >   either `/graph_run` or `/mcp/call` carries `mutates_fs: true`; the one
  >   routed mutating tool, `run_command`, is reached solely through worker
  >   dispatch (`tools/mod.rs:183`). They are therefore **deliberately not wired
  >   in this run**, and the reason is recorded here so their absence is not
  >   re-raised later as an omission: wiring them would add two call sites whose
  >   only observable behaviour is a branch never taken, and an untaken branch is
  >   a claim of coverage nothing tests. The seams that do get wired are worker
  >   dispatch, a Claude `PreToolUse` shim and the OpenCode report-only half.
  >   **If a proxied tool is ever marked `mutates_fs: true`, this paragraph is the
  >   one that becomes wrong** — that is the tripwire, stated in prose because the
  >   condition is a data change, not a code change.
  > - **The OpenCode hook is not unverified, and V32 spike E2 is answered
  >   positively.** `tool.execute.before` is implemented and shipped at
  >   `tabs/config.rs:1938-2064`, where it is load-bearing for V32's native gate,
  >   with tests at `:4844-4859`. The contingency attached to it above — "one
  >   spike answers both; negative ⇒ OpenCode keeps prompt+burst coverage only,
  >   documented" — does not arise, and OpenCode is in scope for tool-sourced
  >   checkpoints from the start.
  > - **"the fields parser must tolerate a missing `source`" is already satisfied
  >   by construction.** `CORE_FIELDS = 8` is compared with a `<` guard
  >   (`shadow.rs:861-863`) and every field past it is read through `fields.get()`
  >   (`:878-881`), a rule the module states outright at `:806-825`. Nothing has to
  >   be *made* tolerant. The companion sentence is likewise already true rather
  >   than aspirational: `Trigger::parse` maps any unknown wire value to `Manual`
  >   (`:138-145`), so "old readers parse `"tool"` as `Manual`, an accepted
  >   degradation" describes shipped behaviour, and it is the accepted degradation
  >   this phase asks for. What stays real in that paragraph is the placement
  >   discipline it implies — the new `Source:` trailer appends **after `Tab:` at
  >   the tail**, read at index 10; `CORE_FIELDS` stays `8`; the guard stays `<`
  >   (`IMPL-PLAN-V33-sandboxing.md` §C8).

## Non-goals

- Sandboxing the cImp host process.
- Sandboxie-Plus or any third-party kernel driver dependency.
- WSL1 (unsupported by every primitive involved).
- Low-integrity-level-only confinement (does not block reads — fails the
  exfil half of the threat model).
- Per-domain network filtering via Landlock (TCP-port-only by design; domain
  policy needs the proxy/iptables layer).

## Live verification (definition of done)

1. Sandboxed `run_command` child: `type %USERPROFILE%\.ssh\id_rsa`-shaped
   probe fails with access denied; same command unsandboxed succeeds
   (control); Tool Activity shows sandboxed provenance.
2. Egress: sandboxed child can reach the loopback proxy and 172.21.1.11 but
   not an arbitrary internet host.
3. Tab under Phase B: Claude Bash `cat` outside the project root denied at
   the OS layer even with permission prompts auto-accepted.
4. Degradation: remove a prerequisite → tab shows UNSANDBOXED badge and a
   Tool Activity row; nothing silently proceeds confined-looking.
5. Max Paranoia tab: full session (spawn → agent work → file written into
   project → tab close); after `wsb stop`, project write persists, sandbox
   environment gone; egress probe from inside blocked per (2).
6. WSL2 hardened profile: Claude tab in WSL2 with the injected sandbox
   settings — `~/.ssh` read denied by `denyRead`, non-allowlisted domain
   prompts/refuses per `strictAllowlist`.
7. Minimal env: a sandboxed `run_command` child running `set` (or `env`)
   shows only the allowlisted variables — no API keys, no cImp-internal
   vars; `git log` and a build probe still work (PATH intact).
8. LAN auth: with keys deployed, a raw `curl` to the llama-server /
   embedding server / MCP endpoints WITHOUT the key is refused; cImp's
   offload, semantic search, and ddg/context7 proxying all still work.
9. Tool-sourced checkpoints: with checkpoints on, let Claude run a Bash
   command that edits files >2 min after the prompt — Timeline shows a
   `tool` checkpoint sourced `claude:Bash` taken BEFORE the edit (restore
   to it recovers the pre-command state); an offload task's `run_command`
   produces one sourced `offload:run_command`; read-only tools
   (`graph_outline`, `fetch_content`) produce none.

## Sources (verified 2026-08-06)

- Claude Code sandboxing: https://code.claude.com/docs/en/sandboxing,
  https://code.claude.com/docs/en/sandbox-environments
- sandbox-runtime (srt): https://github.com/anthropic-experimental/sandbox-runtime
- Windows Sandbox CLI: https://learn.microsoft.com/en-us/windows/security/application-security/application-isolation/windows-sandbox/windows-sandbox-cli
- `.wsb` configuration: https://learn.microsoft.com/en-us/windows/security/application-security/application-isolation/windows-sandbox/windows-sandbox-configure-using-wsb-file
- Landlock: https://docs.kernel.org/userspace-api/landlock.html,
  https://docs.rs/landlock (ABI enum), https://man7.org/linux/man-pages/man7/landlock.7.html
