<script lang="ts">
  /// Settings → Advanced (#129 (c)) — logging, content capture, terminal
  /// scrollback, and the factory reset.
  ///
  /// Three `<section>`s travel together because the sidebar renders them as one
  /// entry: splitting them into three components would give the window three
  /// mounts for one nav click and no reader a reason why.
  ///
  /// `contentOpenFolder` / `contentClear` are imported here rather than passed
  /// down: they are stateless one-shot IPC (open a folder, delete files) that
  /// hold nothing this window owns, the same call the detection panel's
  /// "Open rules folder" button makes from inside its own section.
  ///
  /// The **reset is parent-routed** (`onreset`). It is an `applySettings` call
  /// — the only write in this section that does not go through `patch()` — and
  /// `SettingsApp` is the one owner of that path, because it is the one holder
  /// of `snapshot` and of the draftSync push gate. A second `applySettings`
  /// caller is exactly the kind of second writer the gate cannot see.
  import { contentClear, contentOpenFolder } from '../../ipc';
  import type { Settings } from '../types';
  import NumberField from '../NumberField.svelte';
  import SelectField from '../SelectField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    onreset,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// Replace every setting with `defaultSettings()`. Owned by the window:
    /// it is a wholesale `applySettings` push, and it carries its own
    /// `confirm()` because it is destructive.
    onreset: () => void;
  } = $props();
</script>

<section>
  <h2>Logging</h2>
  <small class="hint top">
    Log files roll daily into <code>logs/</code> next to the cImp
    executable. Changing the level applies live; the
    <code>RUST_LOG</code> env var, when set at launch, overrides
    this until you change it here.
  </small>
  <SelectField
    label="Log level"
    value={snapshot.logging.level}
    onchange={(next) =>
      patch(
        (s) =>
          (s.logging.level = next as Settings['logging']['level']),
      )}
  >
    <option value="trace">Trace</option>
    <option value="debug">Debug</option>
    <option value="info">Info</option>
    <option value="warn">Warn</option>
    <option value="error">Error</option>
  </SelectField>
  <SelectField
    label="Retention"
    value={snapshot.logging.retention}
    onchange={(next) =>
      patch(
        (s) =>
          (s.logging.retention = next as Settings['logging']['retention']),
      )}
  >
    <option value="daily">Daily (keep 1 day)</option>
    <option value="weekly">Weekly (keep 7 days)</option>
    <option value="monthly">Monthly (keep 30 days)</option>
    <option value="never">Never (keep everything)</option>
    {#snippet after()}
      <small class="hint">
        Cleanup runs at launch and whenever this setting changes.
        Files older than the window are deleted; the active day's log
        is always kept.
      </small>
    {/snippet}
  </SelectField>

  <h3>Content capture</h3>
  <small class="hint top">
    When on, raw PTY output for every AI / shell tab is also
    written to <code>logs/content/&lt;tab-id&gt;.log.&lt;date&gt;</code>,
    rotated daily. Output includes ANSI escape codes — pipe through
    <code>sed</code> or a viewer if you want plain text.
  </small>
  <Toggle
    label="Capture full tab output"
    checked={snapshot.logging.content_capture.enabled}
    onchange={(next) => patch((s) => (s.logging.content_capture.enabled = next))}
  />
  <SelectField
    label="Retention"
    value={snapshot.logging.content_capture.retention}
    onchange={(next) =>
      patch(
        (s) =>
          (s.logging.content_capture.retention = next as Settings['logging']['content_capture']['retention']),
      )}
  >
    <option value="daily">Daily (keep 1 day)</option>
    <option value="weekly">Weekly (keep 7 days)</option>
    <option value="monthly">Monthly (keep 30 days)</option>
    <option value="never">Never (keep everything)</option>
  </SelectField>
  <div class="content-actions">
    <button
      type="button"
      onclick={() =>
        contentOpenFolder().catch((e) =>
          console.error('content_open_folder failed:', e),
        )}
    >
      Open folder
    </button>
    <button
      type="button"
      onclick={async () => {
        if (
          !confirm(
            'Delete every file inside the content folder? This cannot be undone.',
          )
        )
          return;
        try {
          await contentClear();
        } catch (e) {
          console.error('content_clear failed:', e);
        }
      }}
    >
      Delete all files
    </button>
  </div>
</section>

<section>
  <h2>Terminal scrollback</h2>
  <small class="hint top">
    Each tab's PTY output is kept in an in-memory ring buffer so
    re-opened panes and restarts can replay history.
  </small>
  <NumberField
    label="Ring buffer size (bytes per tab)"
    min="4096"
    value={snapshot.terminal.scrollback.ring_bytes}
    onchange={(next) =>
      patch(
        (s) =>
          (s.terminal.scrollback.ring_bytes = Math.max(
            4096,
            Number(next) || 262144,
          )),
      )}
  />
  <Toggle
    label="Save scrollback to disk on exit"
    checked={snapshot.terminal.scrollback.persist}
    onchange={(next) => patch((s) => (s.terminal.scrollback.persist = next))}
  />
  <small class="hint">
    On graceful exit each tab's ring is written to
    <code>scrollback/&lt;tab-id&gt;.bin</code> in the config
    directory. Terminal output can contain sensitive text — leave
    off if that shouldn't touch disk.
  </small>
  <Toggle
    label="Restore saved scrollback on launch"
    checked={snapshot.terminal.scrollback.restore_on_launch}
    onchange={(next) => patch((s) => (s.terminal.scrollback.restore_on_launch = next))}
  />
  <small class="hint">
    Replays the persisted bytes into each tab before live output
    resumes on the next launch.
  </small>
</section>

<section>
  <h2>Reset</h2>
  <small class="hint top">
    Replace every setting with its factory default. Wipes
    user-created shell tabs, saved layouts, shortcut overrides,
    and all theme / background overrides. Cannot be undone.
  </small>
  <button
    type="button"
    class="danger"
    onclick={onreset}
  >
    Reset all settings to defaults
  </button>
</section>

<style>
  /* Content capture's two-button row (Open folder / Delete all files).
     Travelled here with the markup: Svelte scopes a class rule to whichever
     component holds the elements, so leaving it in `SettingsApp` would have
     left the buttons stacked and the rule an unused selector. */
  .content-actions {
    display: flex;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }
</style>
