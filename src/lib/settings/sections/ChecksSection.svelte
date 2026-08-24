<script lang="ts">
  /// Settings → Checks (#129 (c)). The body was already `ChecksEditor`; what
  /// lived in `SettingsApp.svelte` was the heading, the prose and F-12's
  /// remote-worker opt-in.
  ///
  /// Prop contract as `HarnessExtForm` set it: the window keeps `snapshot` and
  /// `patch`, and this component never touches the store. There is no
  /// `applySettings` path here on purpose — `patch()` owns the draftSync
  /// lost-update gate, and a second writer would be a second place to get that
  /// wrong.
  import type { Settings } from '../types';
  import ChecksEditor from '../ChecksEditor.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    harnessNamesProse,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// The enabled harnesses' labels joined into a sentence fragment
    /// ("X and Y"). Parent-owned: it is derived from the harness roster the
    /// window loads once, and locked decision 10(a) keeps the names themselves
    /// out of every file but the registry mirror.
    harnessNamesProse: string;
  } = $props();
</script>

<section>
  <h2>Checks</h2>
  <small class="hint">
    Project checker commands the <code>run_check</code> tool exposes to
    {harnessNamesProse}, and the offload worker — a build, typecheck, lint, or test run
    turned into bounded, deduplicated diagnostics instead of a raw dump.
    Configured per project; changes land in this project's
    <code>.cimp/config.json</code> overlay.
  </small>
  <ChecksEditor
    checks={snapshot.checks}
    allowRemoteWorker={snapshot.checks_allow_remote_worker}
    onchange={(next) => patch((s) => (s.checks = next))}
  />

  <!-- F-12's opt-in (`checks_allow_remote_worker`). Deliberately the same
       shape, heading and tone as Code Intelligence → Code graph →
       "Offload worker access" (`graph.allow_remote_worker_access`): per
       project, global across backends, denied by default. It sits in THIS
       section because the setting lives at the settings root beside
       `checks`, not inside `graph` — and because the commands it governs
       are the ones listed right above. Until this landed, the setting
       existed only in Rust and was reachable only by hand-editing
       `.cimp/config.json`.

       OWED TO THE RUST LANE (F-18's fifth site, second half): the
       `run_check` refusal in `offload/backend_gate.rs` sends the user to
       a Code-Intelligence sub-tab named Checks, which has never existed
       — Checks is a top-level section, a SIBLING of Code Intelligence.
       The real path is "Settings → Checks → Offload worker access", i.e.
       this heading. Unaffected by F-18's restructure and still wrong;
       not corrected here because the string is Rust-side and this pass
       may not edit `src-tauri/`. -->
  <h3>Offload worker access</h3>
  <Toggle
    checked={snapshot.checks_allow_remote_worker}
    onchange={(next) => patch((s) => (s.checks_allow_remote_worker = next))}
  >
    Allow a <strong>remote</strong> offload worker to run these checks
  </Toggle>
  <small class="hint">
    ⚠ <strong>Runs commands on this machine:</strong> the local offload
    worker can always run these checks. A <strong>remote</strong> backend —
    a box on your LAN or a public cloud API — cannot, unless you tick this.
    Ticking it lets that remote choose which of the checks above runs here,
    against your working tree, and hands it their output, which quotes your
    source. Denied by default; leave it off unless you trust the remote.
    An AI tab session's own <code>run_check</code> is
    unaffected — this governs the offload worker only. Applies from the
    worker's next call; no tab restart needed.
  </small>
</section>
