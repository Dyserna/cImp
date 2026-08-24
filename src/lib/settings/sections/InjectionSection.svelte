<script lang="ts">
  /// Settings → Injection protection (#129 (c)) — V32's enable hierarchy, the
  /// per-scope override matrix and the detection-rule updater.
  ///
  /// **The four things that stay with the parent, and the one invariant behind
  /// them.** `SettingsApp` runs ONE 4-second poll (`refreshBackendStatuses`)
  /// that fills `backendStatuses`, `serviceStatus`, `detection` AND `injection`
  /// — started in `onMount`, stopped in `onDestroy`. That poll also READS
  /// `detectionBusy` to skip a refresh while an updater run is mid-swap, since
  /// a poll landing between download and swap would paint a half-applied
  /// snapshot over the button row and the run's own return value is the
  /// authoritative one. So the busy flag and the poll's skip guard are one
  /// variable with one owner: `detection`, `injection`, `detectionBusy` and the
  /// three updater actions all stay in the parent, and reach this component as
  /// props and callbacks.
  ///
  /// **What moved.** Everything downstream of those: the feature-meta table,
  /// the row and scope-row derivations, the native-web mode words and
  /// `setInjectionOverride`. All are pure functions of `snapshot` + the two
  /// payloads, and none was read outside this section.
  ///
  /// Overrides are written through the ordinary `patch` save path — there is
  /// deliberately no side-channel command, so the window has one write path and
  /// cannot race its own full-object save.
  import { harnesses } from '../../harness';
  import { detectionOpenRulesFolder, type DetectionStatus } from '../../offload';
  import type { InjectionStatus } from '../../latch';
  import {
    HARNESS_NATIVE_GATE_KEY,
    type InjectionSettings,
    type Settings,
  } from '../types';
  import NumberField from '../NumberField.svelte';
  import SelectField from '../SelectField.svelte';
  import Toggle from '../Toggle.svelte';

  let {
    snapshot,
    patch,
    detection,
    injection,
    detectionBusy,
    appRestartRequired,
    onreloadrules,
    oncheckupdate,
    onrevert,
  }: {
    /// The live settings snapshot values are read from.
    snapshot: Settings;
    /// The window's own settings mutator (clone-mutate-push; no `bind:`).
    patch: (updater: (s: Settings) => void) => void;
    /// Read-only disk facts about the detection surface (rule files compiled,
    /// classifier weights present), from the parent's poll.
    detection: DetectionStatus | null;
    /// The RESOLVED enable hierarchy, from the backend's single resolver. Never
    /// recomputed in TypeScript: a second implementation of the resolution rule
    /// is exactly the drift the one-resolver invariant exists to prevent.
    injection: InjectionStatus | null;
    /// Which component has a check/apply/revert in flight, or `null`. A single
    /// slot rather than a per-button flag because all three buttons on both
    /// rows drive the same updater and a second concurrent run would race the
    /// same staging directory. Parent-owned because the poll reads it.
    detectionBusy: string | null;
    /// The app-wide injection cells changed since the window opened, so open
    /// tabs launched with the old ones. Derived from the parent's restart
    /// baselines.
    appRestartRequired: boolean;
    /// Recompile the rules from disk and fold the fresh counts back in.
    onreloadrules: () => void;
    /// Check (and optionally apply) an update for one component.
    oncheckupdate: (component: string, apply: boolean) => void;
    /// Roll one component back to its previous bundle.
    onrevert: (component: string) => void;
  } = $props();

  /// Each harness's OWN web tools, spelled the way it spells them — one
  /// harness capitalises them and another does not, which is why no single
  /// spelling could serve both (locked decision 27).
  const nativeWebToolsByHarness = $derived(
    $harnesses
      .filter((h) => h.affordances.webTools.length > 0)
      .map((h) => `${h.label}'s ${h.affordances.webTools.join('/')}`)
      .join(' and '),
  );


  /// Whether the detection updater may run at all — **the backend's own
  /// `updates_enabled`, read verbatim** (#48, M-21).
  ///
  /// It used to be re-derived here from the resolved-scope matrix (`app` scope +
  /// `detection`), which is the same conjunction and was correct — but it was a
  /// SECOND reading of the question the two IPC commands enforce with the first,
  /// assembled from a different poll's payload. Two predicates for one gate can
  /// drift; one cannot. `updates_allowed` in `ipc/commands.rs` and this line now
  /// resolve through the same `updater::updates_enabled`, so a greyed button and
  /// a served command cannot disagree about the state, only about the moment.
  ///
  /// Defaults to `true` while the first poll is in flight — and if a build ever
  /// omits the field: this only disables buttons, and the enforcement is in the
  /// IPC command, which refuses with the sentence below. Greying out three
  /// controls for a second at startup would be the more visible bug.
  const detectionUpdatesEnabled = $derived(detection?.updater.updates_enabled ?? true);

  /// WHY the updater is inert, or `undefined` when it is not (#48, M-21).
  ///
  /// One string, rendered on the button row, on all three buttons and in the
  /// prose above them, because those four used to carry four hand-written copies
  /// of a single claim — *"injection detection is off"* — and that claim is FALSE
  /// in one of the three states that produce it. A worker-only override leaves
  /// this updater off while the offload worker is still screening every fetched
  /// page with the bundle on disk; telling that user their detection is switched
  /// off is a false statement about a running security layer, and it is the one
  /// they would act on.
  ///
  /// **Reporting only.** Nothing here decides anything: `detectionUpdatesEnabled`
  /// alone gates the controls, and the IPC commands refuse independently. The
  /// sentences deliberately mirror `ipc::commands::updates_allowed`'s two
  /// refusals — the same state must not get two different explanations depending
  /// on whether the user read a tooltip or clicked and got an error.
  ///
  /// The three states, in the order they are checked:
  /// 1. `worker_only_detection` — off here, ON in the worker. Backend-published,
  ///    never inferred: absent (an older backend) reads `false`, which serves the
  ///    generic sentence rather than claiming a layer is running.
  /// 2. the L1 master is off, which resolves detection off with it. Claimed only
  ///    when `injection` has actually been read — an unread hierarchy falls
  ///    through to (3), whose parenthetical covers the master either way.
  /// 3. detection is off app-wide and nowhere else is running it. The backend's
  ///    own wording for exactly this branch.
  const detectionUpdatesOffReason = $derived.by((): string | undefined => {
    if (detectionUpdatesEnabled) return undefined;
    if (detection?.updater.worker_only_detection === true) {
      return (
        'Injection detection is switched off app-wide and for every AI tab, so nothing is ' +
        'polled or swapped — not on the daily schedule and not from these buttons. It is ' +
        'still switched ON for the offload worker, which keeps screening with the rule ' +
        'bundle already on disk: the updater follows the app-wide answer, and one worker ' +
        'override does not start it. To keep that bundle current, turn injection detection ' +
        'back on app-wide above.'
      );
    }
    if (injection?.protection === false) {
      return (
        'Injection protection is switched off at the master switch above, which resolves ' +
        'injection detection off with it — so nothing is polled or swapped, not on the ' +
        'daily schedule and not from these buttons. Turn the master switch, and injection ' +
        'detection under it, back on.'
      );
    }
    return (
      'Injection detection is switched off, so nothing is polled or swapped — not on the ' +
      'daily schedule and not from these buttons. Turn it (and the injection-protection ' +
      'master above it) back on.'
    );
  });

  /// The per-feature copy this window still owns: the hint text, and the L2
  /// settings key when it cannot be derived.
  ///
  /// **Everything else now comes from the backend's report** (#48, F-y). This
  /// table used to carry eleven literal rows duplicating each feature's key,
  /// label, `spawnBaked` and scope predicates — a hand-kept mirror of
  /// `Feature::ALL`, `label()`, `spawn_baked()` and `has_tab_scope()` /
  /// `has_worker_scope()`, with no drift guard. #47 made every *Rust* mirror a
  /// compile error, which quietly made this worse: the seven errors a new
  /// variant now produces all point at Rust files, so the prompt that used to
  /// sit beside a hand-edited `const ALL` array is gone and this was the only
  /// enumeration left with no signal at all. A V33 control would have shipped
  /// with a status-bar warning naming it and no checkbox here to change it.
  ///
  /// So the matrix renders from `injection.scopes`, and a feature missing from
  /// this table is missing its HINT, not its control.
  type InjectionFeatureMeta = {
    /// The L2 settings key on `offload.injection`. Omit to derive it by the
    /// `<feature>_enabled` convention every flag follows; `null` for a feature
    /// with no boolean L2 at all, whose row is then read-only.
    ///
    /// Typed as a keyof rather than a bare string so a renamed flag is a compile
    /// error here, not a silently dead checkbox. It was previously `'protection'`
    /// — the GLOBAL MASTER — as filler on the native-web row; doubly guarded and
    /// inert, but `keyof InjectionSettings` permitted it, and one regressed guard
    /// would have made that checkbox toggle L1.
    field?: keyof InjectionSettings | null;
    hint: string;
  };

  const INJECTION_FEATURE_META: Record<string, InjectionFeatureMeta> = {
    taint_latch: {
      hint: 'Bidirectional mutual exclusion between external (web/MCP) tools and local file/source-text tools, per task and per tab session. Off: no latching, no refusals, and the offload worker advertises its whole tool surface all run.',
    },
    spotlighting: {
      hint: 'Wraps every external tool result and every recalled memory in nonced data-not-instructions markers. Off: results arrive as raw text, with no standing instruction around them.',
    },
    detection: {
      hint: 'Parent of the signature and classifier layers below — off here disables both regardless of their own toggles.',
    },
    ssrf_guard: {
      hint: 'Screens every outbound fetch URL against the private/loopback/link-local ranges before the call leaves the machine. Off: an injected page can point a fetch at your LAN.',
    },
    fetch_budgets: {
      hint: 'The on/off above the call/byte caps below. Off: neither cap applies, whatever their numbers say.',
    },
    canary: {
      hint: 'A per-task marker planted in the worker’s system context; seeing it leave in a tool argument aborts the task. Worker-only — a harness’s own system prompt is not ours to mark.',
    },
    memory_quarantine: {
      hint: 'Notes written by a conversation that has read external content are stored held-for-review instead of entering project memory. Off: they are stored normally. Notes ALREADY held stay held — turning this off never releases them.',
    },
    native_web: {
      // No boolean L2: `native_web_visibility`'s tri-mode IS this feature's
      // app-wide switch (the Phase G reconciliation), so there is no field to
      // bind and the checkbox is read-only.
      //
      // F-18's companion defect: a read-only checkbox can only say on/off, and
      // `injectionL2On` ticks it for BOTH live modes — so at `sensor`, the
      // shipped default, the row read plain "on" and a user took that to mean
      // the harness was refusing its own web tools when it never denies one.
      // Locked decision 14 makes `sensor` a posture, not a bug, so the fix is
      // to name which of the three modes is in force (rendered beside the label
      // from `nativeWebModeWord`) rather than to change the default.
      field: null,
      hint: 'Set by the Native web tools mode below, which is this feature’s app-wide switch: its "off" IS this control off, and its "sensor" — the shipped default — is this control ON but REPORT-ONLY, raising the taint badge without ever refusing a call. Only "deny" blocks anything. Use the per-tab overrides here to exempt or force one tab.',
    },
    consumer_hygiene: {
      hint: 'The pinned harness permission block and the data-not-instructions paragraph in the session guidance. Off: the harness inherits its upstream defaults and the session is never told how to read cImp’s markers.',
    },
    tool_steering: {
      hint: 'One fixed paragraph in the session guidance asking the harness to prefer the `run_check` and `run_command` MCP tools over running the same commands in its own shell. It names no check, binary or path — it points at the tools’ own enums, which update live — so editing the tool registry never changes it. The `run_command` half is written only when that tool is exposed to this consumer (Tool Plugins → advertise commands). Off: nothing is injected and the harness reaches for its shell as it would without cImp.',
    },
    [HARNESS_NATIVE_GATE_KEY]: {
      hint: 'App-wide ON since V39, like every other control here — but a newly created tab has all of its own switches OFF, so this denies nothing until you enable it for a tab (its shield badge, or the per-tab override below). It shipped app-wide OFF under locked decision 17, for the reason the sentence after this one gives; V39 kept the judgement and moved it a level down. With it on, a tab of the harness that HAS this gate, once it has read external content, is refused its OWN shell/read/edit/write/patch/glob/grep for the rest of the session (and, having gone local first, its own web tools instead). Whole-surface by design: a partial gate is routed around. Policy, not containment — it runs inside the harness’s own process, so a nested ungated copy of it, its pure mode, a user-typed !shell and the raw terminal all bypass it. A per-tab override is the usual way in; it does nothing on a tab whose harness has no native gate.',
    },
    terminal_escape_hygiene: {
      hint: 'Strips ANSI/OSC control sequences (including OSC 52 clipboard writes) out of external text cImp composes into spoken/toast output. Off: a fetched page’s escape sequences travel with the text.',
    },
  };

  /// One matrix row, composed from the backend report plus the local meta.
  type InjectionFeatureRow = {
    key: string;
    label: string;
    spawnBaked: boolean;
    /// Whether the master switch above reaches this control at all
    /// (`Feature::master_gated`). The one row that says `false` today —
    /// managed-tool steering, a token-efficiency nudge rather than a
    /// containment control — must stay EDITABLE while the master is off, or the
    /// window would show a greyed checkbox for a switch that is in force.
    masterGated: boolean;
    /// `null` ⇒ no boolean L2 to bind; the checkbox is read-only.
    field: keyof InjectionSettings | null;
    hint: string;
  };

  /// The L2 settings key for a feature: the meta table's, or the convention
  /// every flag follows. Checked against the live snapshot rather than assumed,
  /// so a convention-derived name that does not exist yields `null` (a read-only
  /// row) instead of a checkbox bound to `undefined`.
  function injectionL2Field(key: string): keyof InjectionSettings | null {
    const meta = INJECTION_FEATURE_META[key];
    if (meta && meta.field !== undefined) return meta.field;
    const derived = `${key}_enabled`;
    return snapshot && derived in snapshot.offload.injection
      ? (derived as keyof InjectionSettings)
      : null;
  }

  /// The matrix rows, in the backend's `Feature::ALL` order. Every scope reports
  /// every feature, so the first scope that has been reported is enough; the
  /// union is taken anyway so a future partial report loses nothing.
  const injectionRows = $derived.by((): InjectionFeatureRow[] => {
    const seen = new Map<string, InjectionFeatureRow>();
    for (const scope of injection?.scopes ?? []) {
      for (const f of scope.features) {
        if (seen.has(f.feature)) continue;
        seen.set(f.feature, {
          key: f.feature,
          label: f.label,
          spawnBaked: f.spawn_baked,
          masterGated: f.master_gated,
          field: injectionL2Field(f.feature),
          hint: INJECTION_FEATURE_META[f.feature]?.hint ?? '',
        });
      }
    }
    return [...seen.values()];
  });

  /// The app-wide native-web mode, normalized exactly as the backend's single
  /// reader does it (`injection::NativeWebMode::parse`): trimmed, and anything
  /// unrecognized resolves to `sensor` rather than `off` — a typo must not blind
  /// the latch. One normalizer here too, so the matrix checkbox and the mode
  /// word below cannot disagree about which mode is in force.
  const nativeWebMode = $derived.by((): 'off' | 'sensor' | 'deny' => {
    const raw = snapshot.offload.native_web_visibility.trim() ?? '';
    return raw === 'off' ? 'off' : raw === 'deny' ? 'deny' : 'sensor';
  });

  /// F-18's companion defect, as a value: which of the three modes is in force,
  /// in words that say what it DOES. The matrix row renders this beside the
  /// feature label, because that row's checkbox is a boolean collapse of a
  /// tri-mode switch and a ticked box at `sensor` — the shipped default — was
  /// read as "the harness is blocking its web tools" when `sensor` never denies
  /// a call. Wording deliberately echoes the select's own option text in "Native
  /// web tools" below: two surfaces, one claim.
  ///
  /// A stored value that is not one of the three is NAMED rather than swallowed:
  /// it resolves to `sensor`, and a hand-edited config must not read as a mode
  /// the user chose.
  const NATIVE_WEB_MODE_WORDS: Record<'off' | 'sensor' | 'deny', string> = {
    off: 'off — no hook, no visibility',
    sensor: 'sensor — reports and taints, never denies a call',
    deny: 'deny — the harness refuses its own web tools',
  };
  const nativeWebModeWord = $derived.by(() => {
    const raw = snapshot.offload.native_web_visibility.trim();
    const word = NATIVE_WEB_MODE_WORDS[nativeWebMode];
    return raw === nativeWebMode ? word : `${word} (stored as “${raw}”)`;
  });

  /// A feature's app-wide L2 value, for the read-only display and for the
  /// "Inherit (on/off)" label on every override cell. One reader, because the
  /// tri-mode exception below used to be spelled out at each of them.
  function injectionL2On(f: InjectionFeatureRow): boolean {
        if (f.field) return snapshot.offload.injection[f.field] as boolean;
    // The one feature with no boolean L2 (see the meta table). Its app-wide
    // switch is the `native_web_visibility` select below; `off` IS this control
    // off, and BOTH other modes are it on — which is why the row also renders
    // `nativeWebModeWord` (F-18's companion defect: on ≠ blocking).
    return f.key === 'native_web' ? nativeWebMode !== 'off' : false;
  }

  /// One override cell, resolved for display.
  ///
  /// Which scopes a feature HAS comes from the report's `in_scope` (#48, F-y) —
  /// it is `Feature::has_tab_scope` / `has_worker_scope` as the backend answers
  /// them, rather than a TypeScript copy of the same two predicates. The stored
  /// cell value still comes from `snapshot`, which is live: the report reflects
  /// SAVED settings, so binding the select to it would make a just-changed cell
  /// snap back until the debounced save and the next poll landed.
  function injectionScopeRows(f: InjectionFeatureRow): Array<{
    scope: string;
    label: string;
    value: string;
    inherited: boolean;
    resolved: string;
  }> {
        const out: Array<{
      scope: string;
      label: string;
      value: string;
      inherited: boolean;
      resolved: string;
    }> = [];
    const inherited = injectionL2On(f);
    for (const scope of injection?.scopes ?? []) {
      // The app scope has no override cells — it is the level the cells inherit
      // FROM — so it is reported but never rendered as a row here.
      if (scope.scope === 'app') continue;
      const row = scope.features.find((x) => x.feature === f.key);
      if (!row?.in_scope) continue;
      const stored =
        scope.scope === 'offload-worker'
          ? (snapshot.offload.injection.worker as unknown as Record<string, string>)[f.key]
          : (
              snapshot.tabs.find((t) => t.kind === 'ai_tool' && t.id === scope.scope) as
                | { injection_overrides?: Record<string, string> }
                | undefined
            )?.injection_overrides?.[f.key];
      const why =
        row.decided_by === 'global'
          ? 'master'
          : row.decided_by === 'scope'
            ? 'this scope'
            : 'app-wide';
      out.push({
        scope: scope.scope,
        label: scope.label,
        value: stored ?? 'inherit',
        inherited,
        resolved: `→ ${row.effective ? 'on' : 'off'} (${why})`,
      });
    }
    return out;
  }

  /// Write one L3 cell. Goes through the ordinary `patch` save path like every
  /// other setting — there is deliberately no side-channel command, so the
  /// Settings window has one write path and cannot race its own full-object
  /// save.
  function setInjectionOverride(scope: string, key: string, value: string): void {
    patch((s) => {
      if (scope === 'offload-worker') {
        (s.offload.injection.worker as unknown as Record<string, string>)[key] = value;
        return;
      }
      for (const t of s.tabs) {
        if (t.kind === 'ai_tool' && t.id === scope) {
          (t.injection_overrides as unknown as Record<string, string>)[key] = value;
        }
      }
    });
  }
</script>

<section>
  <!--
    V32 Phase G (locked decision 16): the three-level enable hierarchy.
    Placed AHEAD of the individual V32 blocks below (budgets, native
    web, detection) because it governs all of them: a user who has come
    here to turn something off should meet the master switch before the
    tuning knobs.

    F-18: this whole group used to be three headings at the BOTTOM of
    "Offload task tools" → Pool, below the backend list and the limits,
    while every pointer to it in the app and in the docs sent the user to
    a Settings "Tools" section that has never existed. It governs every
    AI tab and the MCP surface as much as the offload worker, so it is
    its own top-level category now; the group heading became the
    section heading, and nothing else about these controls moved.
    `settingsPointers.test.ts` is the tripwire that keeps the pointers
    and this sidebar's labels from drifting apart again.
  -->
  <h2>Injection protection</h2>
  <small class="hint top">
    Every V32 containment control has three levels of switch: this master,
    a per-feature switch app-wide, and a per-scope override. A control is
    on when the master is on <em>and</em> either the scope says so or the
    feature does. An override can re-enable a feature its app-wide switch
    disabled; nothing re-enables a containment control past the master.
    Since V39 this master and every per-feature switch ship <em>on</em>,
    and a <strong>newly created AI tab has all of its own overrides off</strong>
    — so a tab's own row is where protection is actually engaged, from
    the shield badge on the tab itself or from the per-scope selects
    below. Tabs that existed before V39 keep their behaviour: the upgrade
    wrote <code>inherit</code> into every cell they had not set.
    (<em>Managed-tool steering</em> below is not a containment control —
    it is a token-efficiency nudge — so the master switch does not reach
    it; its own two switches still do.)
  </small>
  <Toggle
    label="Injection protection (master switch)"
    checked={snapshot.offload.injection.protection}
    onchange={(next) => patch((s) => (s.offload.injection.protection = next))}
  />
  {#if !snapshot.offload.injection.protection}
    <small class="hint down">
      ⚠ <strong>Every containment control is off</strong> — for every tab
      and the offload worker. No taint latch, no spotlighting envelope, no
      SSRF screen, no fetch budgets, no canary, no memory quarantine, no
      native-web visibility, no consumer hygiene, no escape stripping.
      Fetched pages reach the model as raw text and a research session can
      read your files and call out to the web in the same turn. This is the
      documented escape hatch for when a control misfires on real work; the
      per-feature switches below are the smaller instrument.
      <br />
      <em>Managed-tool steering</em> is the one row below this switch does
      not touch: it injects no protection, only a paragraph asking the
      harness to prefer cImp's <code>run_check</code> / <code>run_command</code>
      tools over its own shell. This switch reduces your security posture; a
      token-budget preference is not posture, so that row stays live and
      keeps its own switches.
    </small>
  {/if}
  {#if appRestartRequired}
    <!--
      #48 (F-x): the app-wide half of the restart hint. The per-tab hint
      in the Tabs section diffs a tab's own L3 cells; the master switch
      and the three app-wide L2 inputs move the backend's spawn
      signature too, and until now nothing in this window said so. It
      stays up until a tab is restarted from Settings, because there is
      no way to tell from here that they all have been.
    -->
    <small class="hint down">
      ⚠ Spawn-baked changes are pending. The master switch, the
      spotlighting envelope, the native web tools mode, consumer hygiene,
      managed-tool steering and harness native-tool gating are baked
      into an AI tab when it launches, so every running tab
      keeps the posture it started with — restart them (Settings → Tabs
      → Restart) for these to apply.
    </small>
  {/if}
  {#if injectionRows.length === 0}
    <small class="hint down">
      ⚠ The resolved injection state could not be read from the backend,
      so the per-feature matrix cannot be rendered — it is built from
      that report rather than from a second copy of the feature list.
      The master switch above still applies. Check the console.
    </small>
  {/if}
  {#each injectionRows as f (f.key)}
    <div class="updater-row">
      <label class="checkbox">
        <input
          type="checkbox"
          disabled={(f.masterGated && !snapshot.offload.injection.protection) ||
            f.field === null}
          checked={injectionL2On(f)}
          onchange={(e) => {
            // A feature with no boolean L2 (native-web: its L2 IS the
            // tri-mode select in "Native web tools" below) shows the
            // derived value read-only. Guarded as well as `disabled`
            // because a checkbox that could write here would put the
            // same decision in two controls — the contradictory state
            // the Phase G reconciliation exists to prevent.
            const field = f.field;
            if (field === null) return;
            const on = (e.currentTarget as HTMLInputElement).checked;
            patch((s) => ((s.offload.injection[field] as boolean) = on));
          }}
        />
        <!-- The mode word is rendered for the ONE feature whose L2 is a
             tri-mode rather than a boolean, because its checkbox cannot
             say which of the two live modes is in force and "on" was
             read as "denying" (F-18's companion defect). -->
        <span
          >{f.label}{f.spawnBaked ? ' (needs a tab restart)' : ''}{#if f.key === 'native_web'}
            — <strong>{nativeWebModeWord}</strong>
          {/if}</span
        >
      </label>
      {#if f.hint}<small class="hint">{f.hint}</small>{/if}
      {#if injectionScopeRows(f).length > 0}
        <div class="row">
          {#each injectionScopeRows(f) as sc (sc.scope)}
            <label class="inline-override">
              <span>{sc.label}</span>
              <select
                value={sc.value}
                onchange={(e) =>
                  setInjectionOverride(
                    sc.scope,
                    f.key,
                    (e.currentTarget as HTMLSelectElement).value,
                  )}
              >
                <option value="inherit">Inherit ({sc.inherited ? 'on' : 'off'})</option>
                <option value="on">On</option>
                <option value="off">Off</option>
              </select>
              <span class="mcp-detail">{sc.resolved}</span>
            </label>
          {/each}
        </div>
      {:else}
        <small class="hint">
          App-wide only — the backend reports no per-tab or per-worker
          override row for this control, so there is nothing narrower to
          set. (Terminal escape hygiene is the case that exists today:
          TTS and toasts are global surfaces.)
        </small>
      {/if}
    </div>
  {/each}

  <NumberField
    label="External fetch budget — calls (0 = unlimited)"
    min="0"
    value={snapshot.offload.external_fetch_max_calls}
    onchange={(next) =>
      patch(
        (s) =>
          (s.offload.external_fetch_max_calls = Math.max(
            0,
            Math.floor(+next) || 0,
          )),
      )}
  >
    <small class="hint">
      How many external (web / MCP-server) tool calls one offload task —
      or one AI tab session — may make before further ones
      are refused. Generous by design: it stops runaway fetch loops and
      bulk data staging, not research.
    </small>
  </NumberField>
  <NumberField
    label="External fetch budget — bytes (0 = unlimited)"
    min="0"
    value={snapshot.offload.external_fetch_max_bytes}
    onchange={(next) =>
      patch(
        (s) =>
          (s.offload.external_fetch_max_bytes = Math.max(
            0,
            Math.floor(+next) || 0,
          )),
      )}
  >
    <small class="hint">
      Cumulative bytes of external content one task/session may pull.
      Exhausting either budget refuses further external calls and writes
      one flagged row to Tools → Activities.
    </small>
  </NumberField>

  <h3>Native web tools</h3>
  <small class="hint top">
    cImp's containment latch only sees web access that goes through its
    own proxy. {nativeWebToolsByHarness} bypass it
    entirely, so without one of the modes below a tab can read a hostile
    page while cImp still believes it is clean. Takes effect when an AI
    tab is <strong>restarted</strong>.
  </small>
  <label>
    <span>Native web visibility</span>
    <select
      value={snapshot.offload.native_web_visibility}
      onchange={(e) => {
        const v = (e.currentTarget as HTMLSelectElement).value;
        patch((s) => (s.offload.native_web_visibility = v));
      }}
    >
      <option value="off">Off — no interference, no visibility</option>
      <option value="sensor">Sensor — report only (default)</option>
      <option value="deny">Deny — the harness refuses its own web tools</option>
    </select>
    <small class="hint">
      <strong>Sensor</strong> installs a report-only hook on the two web
      tools (nothing else — no cost on Read/Grep/Bash): using one engages
      that tab's external latch and raises its taint badge, exactly as a
      proxied fetch would. It never blocks a call, and a failure is
      silent.
      <strong>Deny</strong> closes the route by configuration, so all web
      flows through the proxied <code>ddg</code>/MCP tools where the latch
      is fully effective — pair it with local/proxied web servers.
      <strong>Off</strong> is the escape hatch if a hook misbehaves.
      In every mode, shell-level access (<code>curl</code> in Bash) stays
      invisible.
    </small>
    <!-- F-18's companion defect, at the source of it: a `<select>` whose
         stored value matches none of its options renders BLANK, which
         reads as "not set" while the backend is enforcing `sensor`
         regardless. The mode in force is stated rather than left to the
         widget — and it is the same string the matrix row above shows,
         so the two cannot disagree. -->
    <small class="hint">
      In force now: <strong>{nativeWebModeWord}</strong>. Only
      <strong>deny</strong> refuses a call.
    </small>
  </label>

  <h3>Injection detection</h3>
  <small class="hint top">
    Screens the text every external tool brings back (fetched pages, docs
    lookups) for prompt-injection content. Both layers are
    <strong>surface-only</strong>: a hit prepends a warning header for the
    reading model and writes a flagged row to Tools → Activities — nothing
    is ever blocked, withheld or modified, so a false positive costs a line
    of noise, not a broken task.
  </small>
  {#if detection}
    <ul class="mcp-health">
      <!-- #48/N-3: the dot binds the BACKEND's predicate. Deriving it
           here as `files_failed === 0 && files_loaded > 0` omitted
           `rules`, which the updater's own health check requires, so a
           .yar file that parsed and defined nothing rendered green
           beside "1 file(s) loaded, 0 rule(s)" while scan returned
           empty. One predicate, in one language. -->
      <li class:healthy={detection.rules.healthy} class:down={!detection.rules.healthy}>
        <span class="mcp-dot" aria-hidden="true"></span>
        <span class="mcp-name">Signature rules</span>
        <span class="mcp-detail" title={detection.rules.dir}>
          {detection.rules.files_loaded} file(s) loaded, {detection.rules.rules} rule(s){detection.rules.files_failed > 0
            ? ` — ${detection.rules.files_failed} failed: ${detection.rules.failed.join(', ')}`
            : ''}{!detection.rules.armed
            ? ' — the signature layer has nothing to match with'
            : ''}
        </span>
      </li>
      {#if detection.local_rules_broken?.failed.length}
        <!-- #48/U-4's other half: once a broken `local/` rule stopped
             vetoing the update channel it stopped being loud, and its
             only trace was a `warn!` line in a log nobody has open.
             The Advisor card is the nudge; this is the row in the place
             the user goes to look. Same backend predicate, so the two
             cannot disagree about whether their rules are live. -->
        <li class="down">
          <span class="mcp-dot" aria-hidden="true"></span>
          <span class="mcp-name">Your rule files</span>
          <span class="mcp-detail" title={detection.local_rules_broken.dir}>
            {detection.local_rules_broken.failed.length} file(s) in
            <code>rules.d/local/</code> did not compile and are NOT matching:
            {detection.local_rules_broken.failed.join(', ')} — the rest of the
            set ({detection.local_rules_broken.rules} rule(s)) is live. Fix the
            file and press Reload rules.
          </span>
        </li>
      {/if}
      {#if detection.local_rules_broken?.renamed.length}
        <!-- #48/M-13: a rule of the user's whose identifier a shipped
             rule has taken. It IS live and IS matching, so this is
             deliberately NOT the `down` row above — describing it in
             the broken file's words would be the same "degraded path
             reporting the wrong thing" shape this milestone keeps
             finding. It still needs a row: the identifier a hit
             reports is no longer the one their file spells. -->
        <li class="healthy">
          <span class="mcp-dot" aria-hidden="true"></span>
          <span class="mcp-name">Your renamed rules</span>
          <span class="mcp-detail" title={detection.local_rules_broken.dir}>
            {detection.local_rules_broken.renamed.length} rule(s) in
            <code>rules.d/local/</code> declare an identifier the shipped bundle
            also uses, so cImp loaded yours under a renamed one and they keep
            matching:
            {detection.local_rules_broken.renamed
              .map((r) => `${r.from} → ${r.to} (${r.file})`)
              .join(', ')} — a hit reports the NEW identifier. Your files were
            not modified; rename the rule yourself to take the name back.
          </span>
        </li>
      {/if}
      <li class:healthy={detection.classifier.present} class:down={!detection.classifier.present}>
        <span class="mcp-dot" aria-hidden="true"></span>
        <span class="mcp-name">Classifier</span>
        <span class="mcp-detail" title={detection.classifier.dir}>
          {detection.classifier.present
            ? 'Prompt Guard 2 weights loaded'
            : (detection.classifier.error ?? 'weights not installed')}
        </span>
      </li>
    </ul>
  {:else}
    <!-- #48/H-10: "unavailable" is a third state, not a quiet "fine".
         The old parenthetical guessed a cause ("still starting") that a
         permanently failing `detection_status` makes untrue, and this
         panel is where a user goes to check. -->
    <small class="hint">
      Detection status unavailable — cImp could not read it. It may still
      be starting; if this persists, the layers below are UNVERIFIED
      rather than known to be off. Check the console.
    </small>
  {/if}
  <div class="row">
    <button type="button" onclick={onreloadrules}>Reload rules</button>
    <button type="button" onclick={() => void detectionOpenRulesFolder()}>
      Open rules folder
    </button>
  </div>
  <small class="hint">
    Rules are plain <code>.yar</code> files next to cimp.exe under
    <code>detection/rules.d/</code>. Drop your own in the
    <code>local/</code> subfolder — the auto-updater below replaces the
    shipped bundle but never touches <code>local/</code>. A file that
    fails to compile is skipped and the rest still load.
  </small>
  <Toggle
    label="Signature screen (YARA rules)"
    checked={snapshot.offload.detection_signature_enabled}
    onchange={(next) => patch((s) => (s.offload.detection_signature_enabled = next))}
  />
  <Toggle
    label="Classifier screen (Prompt Guard 2)"
    checked={snapshot.offload.detection_classifier_enabled}
    onchange={(next) => patch((s) => (s.offload.detection_classifier_enabled = next))}
  />
  {#if detection && !detection.classifier.present}
    <small class="hint">
      Optional and not bundled — cImp does not ship these weights, because
      they are under the Llama Community Licence rather than the permissive
      licences the TTS and speech models use. The layer stays inert until
      you install them, and that is a supported configuration: the YARA
      signature screen carries detection on its own. To enable it, put
      <code>model.onnx</code> and <code>tokenizer.json</code> in
      <code>models/promptguard2-22m/</code> and restart. An ungated ONNX
      export lives at
      <code>huggingface.co/gravitee-io/Llama-Prompt-Guard-2-22M-onnx</code>
      — it offers a 284&nbsp;MB fp32 build and a 72&nbsp;MB int8 one
      (<code>model.quant.onnx</code>, rename it). Digests to verify against,
      and the requirements any other export must meet, are in
      <code>models/CHECKSUMS.txt</code>.
    </small>
  {/if}
  <NumberField
    label="Classifier threshold (0–1)"
    min="0"
    max="1"
    step="0.01"
    value={snapshot.offload.detection_classifier_threshold}
    onchange={(next) =>
      patch(
        (s) =>
          (s.offload.detection_classifier_threshold = Math.min(
            1,
            Math.max(0, +next || 0),
          )),
      )}
  >
    <small class="hint">
      Probability at or above which the classifier flags a result. Lower
      catches more and warns more often; 0.9 is the conservative default,
      because a header on every page trains the model to ignore it.
    </small>
  </NumberField>

  <h4>Detection updates</h4>
  <small class="hint top">
    Signature rules go stale: they only match phrasings someone has
    already written down. cImp checks a curated manifest
    (its own GitHub release, never third-party repos) on a daily
    interval. A candidate bundle is verified by SHA-256, compiled, and
    run against shipped control documents — it must catch the known
    attacks and must NOT flag the benign ones — before it goes live. If
    a candidate bundle is refused, the old data stays active and you get
    a card; detection never silently degrades to nothing. If the channel
    simply cannot be reached — offline, a proxy, a release that is not
    published yet — that is reported here and nowhere else, until it has
    been unreachable long enough to mean this component has stopped
    getting fresher. Every check follows the <em>app-wide</em> answer of
    the <em>Injection detection</em> switch above (and the master switch
    above that): with detection off app-wide, nothing is polled or
    swapped — not on the daily schedule and not from the buttons below.
    Switching it on for one AI tab counts as app-wide here, because there
    is one rule bundle on disk for the whole app; switching it on for the
    offload worker alone does not start the updater, and the worker goes
    on screening with the bundle it already has.
  </small>
  <!--
    #48 (M-21): and WHICH of those states is in force, in the words
    `ipc::commands::updates_allowed` refuses with. The paragraph above
    states the rule; this states the fact, and it is rendered rather than
    left to the tooltips because a disabled button does not reliably
    raise one (the same reason the button row carries a title of its
    own). Absent — including when the status could not be read at all —
    nothing is claimed.
  -->
  {#if detectionUpdatesOffReason}
    <small class="hint">{detectionUpdatesOffReason}</small>
  {/if}
  {#if detection}
    {#each detection.updater.components as comp (comp.component)}
      <div class="updater-row">
        <div class="row">
          <strong>Signature rules</strong>
          <span class="mcp-detail">
            installed: <code>{comp.installed_version || '(shipped)'}</code>
            {#if comp.available_version && comp.available_version !== comp.installed_version}
              · available: <code>{comp.available_version}</code>
            {/if}
            {#if comp.last_check_ms > 0}
              · checked {new Date(comp.last_check_ms).toLocaleString()}
            {:else}
              · never checked
            {/if}
          </span>
        </div>
        <SelectField
          label="Update mode"
          value={snapshot.offload.detection_update_rules_mode}
          onchange={(next) => {
            const v = next;
            patch((s) => {
              s.offload.detection_update_rules_mode = v;
            });
          }}
        >
          <option value="off">Off — never check</option>
          <option value="check">Check only — tell me, change nothing</option>
          <option value="auto">Auto — validate and apply</option>
        </SelectField>
        <!--
          #48: all three are gated on the resolved detection feature,
          not only on `detectionBusy`. The IPC commands refuse too — a
          disabled attribute is a courtesy, not a control — and the
          tooltip sits on the ROW as well as on each button, because a
          disabled button does not reliably raise one.

          #48 (M-21): all four tooltips render the SAME
          `detectionUpdatesOffReason`. They used to carry four separate
          literals of one claim, and that claim named a cause nobody had
          checked. What gates the buttons is unchanged — the reason
          string decides nothing and is never read by `disabled`.
        -->
        <div class="row" title={detectionUpdatesOffReason}>
          <button
            type="button"
            onclick={() => oncheckupdate(comp.component, false)}
            disabled={detectionBusy !== null || !detectionUpdatesEnabled}
            title={detectionUpdatesOffReason}
          >
            {detectionBusy === comp.component ? 'Checking…' : 'Check now'}
          </button>
          <button
            type="button"
            onclick={() => oncheckupdate(comp.component, true)}
            disabled={detectionBusy !== null ||
              !detectionUpdatesEnabled ||
              !comp.available_version}
            title={detectionUpdatesOffReason}
          >
            Apply update
          </button>
          <button
            type="button"
            onclick={() => onrevert(comp.component)}
            disabled={detectionBusy !== null ||
              !detectionUpdatesEnabled ||
              !comp.can_revert}
            title={detectionUpdatesOffReason}
          >
            Revert to {comp.previous_version || 'previous'}
          </button>
        </div>
        {#if comp.last_outcome_kind === 'unavailable'}
          <!--
            #46: the channel could not be REACHED. Nothing was refused,
            so this is deliberately NOT the unhealthy colour — a 404, a
            proxy or an offline laptop is not a security event, and
            painting it red is what made the real rejection colour
            meaningless. The streak is shown because "for how long" is
            the part that eventually matters.
          -->
          <small class="hint">
            Could not reach the update channel: {comp.last_outcome}
            {#if comp.unreachable_streak > 1}
              ({comp.unreachable_streak} checks in a row — the installed
              data is still live, but this component is not getting
              fresher.)
            {/if}
          </small>
        {:else if comp.last_outcome}
          <small class="hint" class:down={!comp.last_ok}>
            Last check: {comp.last_outcome}
          </small>
        {/if}
        {#if comp.unrestored_files.length}
          <!--
            #48/M-11: the only line in this block that means "degraded
            RIGHT NOW". A failed rollback left the live directory short
            of files that are still in the retained copy — the state no
            other readout here can express, because everything that is
            present compiles clean. Unhealthy colour, unconditionally:
            unlike a refusal, this is not "the old data is still live".
          -->
          <small class="hint down">
            Incomplete rule set: {comp.unrestored_files.length} file(s) could
            not be restored ({comp.unrestored_files.join(', ')}) and are
            missing from the live folder. They are still retained, and cImp
            retries on every check and every launch — close anything holding
            them open (antivirus, an editor) and restart.
          </small>
        {/if}
        {#if comp.available_notes}
          <small class="hint">Release note: {comp.available_notes}</small>
        {/if}
      </div>
    {/each}
    <small class="hint">
      Manifest: <code>{detection.updater.manifest_url}</code>
    </small>
  {/if}
  <NumberField
    label="Check interval (hours)"
    min="1"
    max="720"
    step="1"
    value={snapshot.offload.detection_update_interval_hours}
    onchange={(next) =>
      patch(
        (s) =>
          (s.offload.detection_update_interval_hours = Math.max(
            1,
            Math.round(+next || 24),
          )),
      )}
  >
    <small class="hint">
      Also checked once shortly after launch, and skipped if the last
      check was inside this window — a restart does not re-download.
      Floored at 1 hour.
    </small>
  </NumberField>
  <label>
    <span>Manifest URL override</span>
    <input
      type="text"
      placeholder="(the pinned cImp detection manifest)"
      value={snapshot.offload.detection_update_manifest_url}
      onchange={(e) =>
        patch(
          (s) =>
            (s.offload.detection_update_manifest_url = (
              e.currentTarget as HTMLInputElement
            ).value.trim()),
        )}
    />
    <small class="hint">
      Leave empty for the pinned URL. Downloads must live under the same
      directory as whatever manifest is in force, so an override
      relocates the whole bundle rather than letting a manifest point at
      a host of its choosing. Mainly for testing a staged bundle.
    </small>
  </label>
</section>

<style>
  .mcp-health {
    list-style: none;
    margin: 0.4rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .mcp-health li {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    font-size: var(--font-size-sm);
  }
  .mcp-dot {
    flex: 0 0 auto;
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 50%;
    background: var(--text-secondary);
    align-self: center;
  }
  .mcp-health li.healthy .mcp-dot {
    background: var(--success, #3fb950);
  }
  .mcp-health li.down .mcp-dot {
    background: var(--danger, #d08770);
  }
  .mcp-name {
    font-weight: 600;
    min-width: 6rem;
  }
  .mcp-detail {
    color: var(--text-secondary);
  }
  /* V32 C3 — one updatable detection component: a header line, its mode
     select, and the three buttons. Boxed like the MCP editor's server rows
     so the two components
     read as two units rather than one long column of controls. */
  .updater-row {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    padding: var(--space-2) 0;
    border-top: 1px solid var(--border-faint);
  }
  .updater-row .mcp-detail {
    font-size: var(--font-size-xs);
  }
  /* V32 Phase G — one per-scope override cell inside a feature row. Laid out
     inline so a feature's scopes read as a short matrix row rather than as a
     column of full-width selects, which at ten features would bury the
     per-feature switches they hang off. */
  .inline-override {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    font-size: var(--font-size-xs);
    color: var(--text-secondary);
  }
  .inline-override select {
    font-size: var(--font-size-xs);
    padding: 1px 4px;
  }
</style>
