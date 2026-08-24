<script lang="ts">
  /// Settings → Sandboxing (#129 (c)) — V33 Phase A's OS-enforced boundary
  /// around the child processes an agent starts.
  ///
  /// Pure form over `snapshot`/`patch`. The two prose values it interpolates
  /// are registry-derived and read by other sections too, so they arrive as
  /// props rather than being derived a second time here.
  import type { Settings } from '../types';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    harnessNames,
    harnessStateDirs,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// The enabled harnesses' labels, joined. Parent-owned: three sections
    /// interpolate it, and it derives from the roster the window loads once.
    harnessNames: string;
    /// Where each harness keeps its own state, for the sandbox copy.
    harnessStateDirs: string;
  } = $props();
</script>

<section>
  <!--
    V33 Phase A. Locked decisions 16 (one category) and 17 (the master
    switch reaches the OS layer ONLY) — see
    `docs/MILESTONE-V33-sandboxing.md` and the S1 spike report for what
    the boundary actually is.
  -->
  <h2>Sandboxing</h2>
  <small class="hint top">
    An OS-enforced boundary around the child processes an agent starts —
    the commands the offload worker runs through <code>run_command</code>,
    the configured checks <code>run_check</code> runs, the code-audit
    scanners, and (separately switched below) AI tool tabs. Injection
    protection (above) constrains a compromised model at the tool layer;
    this makes the operating system enforce a boundary the model cannot
    negotiate with. They are separate categories because neither delivers
    the other.
  </small>
  <small class="hint top">
    <strong>These settings are machine-global.</strong> They are saved to
    the global settings file and are deliberately ignored if they appear
    in a project's <code>.cimp/config.json</code> — a boundary a project
    file could switch off would be no boundary at all, since anything
    running inside the project root can write that file.
  </small>
  <Toggle
    label="Sandbox agent-started processes (master switch)"
    checked={snapshot.sandbox.enabled}
    onchange={(next) => patch((s) => (s.sandbox.enabled = next))}
  />
  <small class="hint down">
    On Windows each allowlisted command runs inside an
    <strong>AppContainer</strong>: it can read and write the project root
    and read the operating system plus the tool's own program files — and
    nothing else. Your credentials, other projects and cImp's own tokens
    are unreadable to it. Off, the command still gets the minimal
    environment, the process-tree kill and every injection-layer control:
    this switch governs the OS boundary only, never the containment
    underneath it.
  </small>
  {#if !snapshot.sandbox.enabled}
    <small class="hint down">
      Sandboxing is <strong>off by user choice</strong>. Commands run with
      your full file access. This state and “unavailable — a prerequisite
      is missing” are recorded distinctly in the Events tab, so a failed
      prerequisite can never hide behind this setting.
    </small>
  {/if}
  <!--
    V33 Phase B (locked decision B2). A scope widener INSIDE the OS
    layer, not a second master switch — hence disabled until the master
    is on, and hence its own paragraph about what confining the agent
    itself costs.
  -->
  <Toggle
    checked={snapshot.sandbox.tabs}
    disabled={!snapshot.sandbox.enabled}
    onchange={(next) => patch((s) => (s.sandbox.tabs = next))}
  >
    Also sandbox AI tabs ({harnessNames})
  </Toggle>
  <small class="hint down">
    The tab <em>is</em> the agent, so this confines everything it later
    runs. A sandboxed tab reads and writes the project and its own
    harness state ({harnessStateDirs}),
    reads <code>~/.gitconfig</code> for your commit identity, and always
    has network access — an AI CLI without egress is a bricked tab.
    Deliberately <strong>not</strong> granted: <code>~/.ssh</code> and the
    Windows Credential Manager, so a <code>git push</code> from inside a
    sandboxed tab will be refused. Add what you want reachable under
    “Extra readable tool directories” below. Plain Shell tabs are never
    sandboxed — they are your own hands, not an agent seam. Changing this
    affects tabs started afterwards; running tabs need a restart.
  </small>
  <Toggle
    label="Allow network access from sandboxed processes"
    checked={snapshot.sandbox.allow_network}
    disabled={!snapshot.sandbox.enabled}
    onchange={(next) => patch((s) => (s.sandbox.allow_network = next))}
  />
  <small class="hint down">
    Off, a sandboxed command reaches no network at all — the right default
    for build and test probes. On, it reaches the internet <em>and</em>
    your LAN: Windows capabilities cannot separate the two on this
    network, so per-host allowlisting is not yet offered rather than
    offered and untrue. This applies to the commands, checks and audit
    scanners an agent runs; sandboxed <em>tabs</em> always have network
    access regardless of this setting.
  </small>
  <label class="field">
    <span>Extra readable tool directories</span>
    <textarea
      class="sandbox-dirs"
      rows="5"
      disabled={!snapshot.sandbox.enabled}
      value={snapshot.sandbox.extra_grant_dirs.join('\n')}
      onchange={(e) =>
        patch((s) => {
          s.sandbox.extra_grant_dirs = (e.currentTarget as HTMLTextAreaElement).value
            .split('\n')
            .map((l) => l.trim())
            .filter((l) => l.length > 0);
        })}
    ></textarea>
  </label>
  <small class="hint down">
    One path per line. The command's own program directory is granted
    automatically, which covers most tools; add a directory here when a
    toolchain reaches sideways into another (a compiler calling a linker
    from a different tree). Tools installed under Program Files need no
    entry. If a directory cannot be granted — typically one owned by
    Administrators — the command runs unsandboxed and says so in Events
    rather than failing.
  </small>
  <small class="hint down">
    Tools that need a runtime — Python, Node, a JRE, the .NET SDK, Go,
    cargo — get that runtime's own directories granted automatically,
    and their caches are redirected into
    <code>.cimp/sandbox-cache/</code> inside the project, because the
    project is the only place a sandboxed tool may write. When a runtime
    is needed but cannot be located, the tool still runs and Events says
    which piece was missing rather than leaving you a silent failure.
  </small>
  <small class="hint down">
    Some paths are refused here on purpose: credential directories
    (<code>.ssh</code>, <code>.aws</code>, <code>.gnupg</code>, the
    Windows credential stores), your user-profile root, a drive root and
    the Windows directory. A refused line is reported in Events and the
    remaining grants still apply — so a single bad entry never widens the
    boundary and never breaks the run.
  </small>
</section>

<style>
  /* Extra readable tool directories: one path per line, so it wants the full
     column width and enough rows to show a handful without scrolling. */
  textarea.sandbox-dirs {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    background: var(--surface-sunken);
    border: 1px solid var(--border-default);
    color: var(--text-primary);
    padding: 6px var(--space-2);
    border-radius: var(--radius-md);
    font-family: var(--font-mono, monospace);
    font-size: var(--font-size-sm);
    line-height: 1.4;
  }
</style>
