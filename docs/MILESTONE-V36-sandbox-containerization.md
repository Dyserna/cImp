# V36 — Sandbox Containerization

**Status:** SPEC — not yet coded (2026-08-18). GitHub: milestone 10, umbrella #76.
**Origin:** re-scoped out of [MILESTONE-V33-sandboxing.md](MILESTONE-V33-sandboxing.md)
on 2026-08-18 (user decision, recorded there as a dated amendment and on #30).
V33 keeps per-process OS sandboxing (AppContainer / Landlock at the spawn
seams); this milestone is the tier above it — **a disposable, containerized
environment per tab**, where an injected agent's blast radius is the container,
and teardown discards everything except writes into the mapped project folder.

**Why a separate milestone:** the platform story diverges at this tier. The
Windows implementation (Windows Sandbox, `.wsb`) and the WSL2-gated pieces have
no Linux counterpart — on Linux the same tier is a rootless container. Bundling
them under V33 hid that the two legs are peers with different gates, and made
V33 unclosable against work that is really a second project.

**Design authority:** the sections written under V33 remain the source design —
§ "Max Paranoia Mode — platform designs", spikes S4/S5, Phase C, and the
Max-Paranoia legs of V33 decisions 4 (egress allowlist) and 9 (per-tab toggle
named "Max Paranoia"). They are quoted or summarized here; where this doc and
V33's grow apart, this one wins from 2026-08-18 on.

## Scope

### Windows leg — Max Paranoia Mode (was V33 Phase C)

- **S4 — WFP egress scoping spike** (gate): host-side WFP rules on the sandbox
  NAT adapter allowing only the decision-4 endpoints. Confirmed necessary by
  V33's S1: AppContainer capabilities are class-granular (`internetClient`
  opens the LAN too), so per-host scoping needs WFP regardless of engine.
- **S5 — Windows Sandbox bootstrap spike** (gate): OpenSSH via `LogonCommand`,
  `wsb ip` + ssh tab end-to-end, time-to-first-prompt (staged-tooling mapped
  folder if minutes). **User prerequisite:** enable
  `Containers-DisposableClientVM` (elevation + reboot); `wsb.exe` was absent on
  the dev machine as of 2026-08-13.
- **Max Paranoia Mode** (S4+S5-gated): per-tab toggle named exactly that;
  `.wsb` generation (project root RW + read-only bootstrap folder); **SSH
  transport, not `wsb exec`** (no process I/O — verified); WFP egress on the
  NAT adapter; `wsb stop` discard-on-close. GPU absence inside is irrelevant —
  LLM/TTS/STT run host-side or on LAN.

### Linux leg — containerized tabs (was the podman half of V33 Phase D)

The native-Linux alternative — WSL2 and Windows Sandbox do not exist there.

- **Rootless podman, devcontainer-pattern:** no daemon, no root; project
  bind-mounted; egress via the iptables allowlist pattern (Anthropic's
  devcontainer as reference — adapted, not depended on) or a proxy sidecar;
  container removed on tab close. *Rejected already (V33):* microVMs
  (Firecracker/cloud-hypervisor) — isolation beyond need at real operational
  cost; container + egress allowlist already exceeds the Windows Sandbox bar.
- **bwrap nesting spike:** Claude's own bwrap starting under the container —
  expected fine, unverified.
- **Egress parity decision:** netns/iptables vs proxy sidecar — one answer,
  the WFP analog.
- **WSL2 stepping stone:** the same podman design is runnable from Windows
  today for a WSL-resident repo, before native Linux support matures.

### Shared

- **Hardened Claude `sandbox.*` profile** (V33 decision 6, platform-gated):
  written once into the `--settings` overlay, active wherever Claude's
  built-in sandbox exists (WSL2 today; the Linux container makes it live
  there). Ships without any spike and has not.
- **Transport/UX seam:** the tab's PTY command becomes `ssh ...` /
  `podman exec ...` — the PTY layer already treats it like any command; verify
  TUI fidelity (mouse, resize, scrollback) through each transport.

## Non-goals

- Per-process sandboxing — that is V33, and it composes with (not replaces)
  this tier.
- Per-host egress for unsandboxed spawns.
- Any model-writable path to an isolation setting (V33's C10 applies here
  identically: the toggles are user-only).
- Third-party container images as a trust boundary — the bootstrap installs
  from the same sources the host would.

## Definition of done (carried from V33's live-verify list, item 5)

A full Max-Paranoia session on each platform leg: spawn → agent work → file
written into the mapped project folder survives → teardown discards everything
else — plus an egress probe from inside showing only allowlisted endpoints
reachable.
