# V33 — OS Sandboxing & Max Paranoia Mode

**Status:** SPEC — not yet coded (2026-08-06). GitHub issue: TBD.
**Builds on:** the two spawn seams cImp owns — `pty/manager.rs::PtyLaunchSpec:20`
(every AI tab: Claude, OpenCode, future harnesses) and the offload worker's
`run_command` children (`offload/tools/run_command.rs`, plain non-PTY `Stdio`
spawns) — plus the `--settings` overlay injection mechanism
(`tabs/config.rs`, same seam as the statusline overlay) and the Linux
milestone (`docs/MILESTONE-linux-support.md`).
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
2. **Windows tier 2 engine is decided by spike, AppContainer vs srt-alpha,
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
- **Phase B — tab spawns sandboxed** (S3-gated) + hardened Claude profile in
  the overlay (decision 6 — can ship with Phase A since it is
  platform-gated config).
- **Phase C — Max Paranoia Mode, Windows** (S4+S5-gated): `.wsb` generation,
  bootstrap, SSH tab wiring, WFP scoping, teardown, UI toggle named
  "Max Paranoia".
- **Phase D — Linux tier 2 + Max Paranoia** (rides the Linux milestone):
  Landlock `pre_exec` + grant table; podman mode; nesting spike (Claude's
  bwrap starting under our Landlock wrapper — expected fine, unverified).
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
  the identical-tree dedupe (first call after a prompt no-ops naturally).
  Metadata-format care: the fields parser must tolerate a missing `source`
  (old checkpoints) — and old readers parse `"tool"` as `Manual`, an
  accepted degradation. Timeline UI renders the source on the row.

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
