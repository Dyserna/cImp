# Code review — develop, f192cf0..88dff66 (2026-08-05)

Scope: all 35 commits since the last merge to main (v0.49.1) — the 2026-08-04 maintenance
run (#3–#27), V28 per-session MCP identity, V29 xterm 6, and V30 session-push Phases 0–D.
87 files, ~10.9k insertions. Method: seven parallel subsystem reviews (offload core, oob
producers/fanout, V28 identity + graph/audit, permission detection, usage/context bar,
xterm 6 + settings, deps/CI), every finding verified against callers; the two HIGHs were
re-verified independently at source by the orchestrator. Line numbers are at HEAD (88dff66).

Severity counts: **2 HIGH, 16 MEDIUM (after dedup), ~15 LOW.** No finding blocks a build;
several block the V30/V28 live-verifies or invert a milestone's stated contract.

---

## HIGH

### H1 — V28's Claude tab→session binding conflates same-project tabs; the milestone's primary case does not hold
`src-tauri/src/oob/claude.rs:132` (`newest_jsonl(&root)` per poll), root derived from the
project dir only (`oob/claude.rs:75-83`, `newest_jsonl` at `:1322`); consumed by
`graph/service.rs:1427` (`live_session_for_tab`) → `offload/loopback.rs:848-855`.

Each Claude tab's transcript tap independently tails the newest-mtime `*.jsonl` in
`~/.claude/projects/<slug(cwd)>` with no per-tab/per-process discriminator. The built-in
`claude-local` tab has `command: "claude"`, `cwd: None` (`settings/schema.rs:2823-2831`) —
identical root. With tabs A and B open on one project, whichever session wrote last wins
for **both** taps: registry becomes `{A → ses_B, B → ses_B}`, `scoped_session`
short-circuits, and `context_note` in A lands in B's scope — exactly the pre-V28 behavior
V28 exists to remove. Live-verify recipe #1 in `docs/MILESTONE-V28-session-identity.md`
will fail as written. The OpenCode half (the spec's one open question) is correct
(`oob/opencode.rs:553-580`, per-tab SSE + child filtering).

Same root cause also degrades hook-driven permission detection (see M7): two same-project
tabs make every hook event unmappable (dropped at `loopback.rs:1487-1489`), and a
launch-order window (`live_confirmed = !first_attach`, `oob/claude.rs:138`) attributes
events — badge + TTS — to the wrong tab.

Corroborating inconsistency in-range: `resolve_permission_tab` (`loopback.rs:1479-1504`)
treats two-tabs-one-session as ambiguity and refuses; the V28 lookup consumes the same
registry and treats it as proof.

Two independent reviewers converged on this mechanism; orchestrator re-verified at source.
Not a regression vs. pre-V28, but the milestone is marked IMPLEMENTED and its Phase A/B
claim (spec decision 5, "two same-agent tabs, same project — now isolated") is not met.

### H2 — `--notify-hook` is injected unconditionally, but its delivery channel only exists when `loopback_needed()`; on a default install the new primary permission signal is dead and silent
`tabs/config.rs:512-521` injects `Notification` + `PermissionDenied` hooks into every
Claude tab's `--settings` overlay with no gate. The shim's only delivery path is
`post_loopback` → `read_discovery_for` (`context_hook.rs:86-90`); the loopback server
starts only under `Settings::loopback_needed()` (`settings/schema.rs:739`, `main.rs:659`),
whose three inputs (offload, graph, code_audit) all default to **false**.

Fresh install: every Claude notification spawns a `cimp --notify-hook` process, discovery
returns `None`, the POST is dropped, hook-primary detection never fires once, and the app
silently runs on the demoted regex — zero log output (the shim is silent by design, and
`contract_drift` reporting rides the same dead channel). Invisible on the dev machine,
where graph+offload are on. The schema's own invariant comment
(`settings/schema.rs:730-738`: advertise gates must stay a subset of `loopback_needed`)
is violated by these hooks — the first such violation. The existing subset tripwire
(`tabs/config.rs:2197-2230`) covers only `--mcp-config`, not hooks.

Orchestrator re-verified injection site + gate at source.

---

## MEDIUM — V30 session push (the dominant cluster; three reviewers converged)

### M1 — Producers broadcast; Phase B's tab-addressing is production-dead
`graph/service.rs:3167` and `audit/runner.rs:706` call `push_broadcast`
(`offload/service.rs:363-386`, pick = `|_| true`). `push_to_tab`'s only callers are the
spike-gated `/push_test` and unit tests; `events_query`'s `tab=` param, `PushSubscriber.tab`,
and `handle_events`' tab parse have no production consumer. Spec Phase C
(`MILESTONE-V30-mcp-channels.md:196-198`) calls for origin-tab on `RunBody`/`/audit/run`;
no `tab` field was added. Every ≥30 s graph rebuild starts a model turn in **every**
channel-armed tab — the notice's token price multiplied by tab count.

### M2 — Graph producer has no origin/initiator gate; automatic rebuilds push
`graph/service.rs:3145` gates only on full-rebuild + ≥30 s. Non-user call sites reach it:
startup build (`main.rs:776`), settings-enable watcher (`main.rs:794`), watcher
channel-overflow recovery (`watcher.rs:118`), and `reindex_paths`' `DirWalk::TooBig`
escalation (`service.rs:3468`) — the last two falsify the producer's own doc comment
("the incremental watcher path does not call this and must not",
`graph/service.rs:3134-3140`). App launch on a big repo, or a large `git checkout`,
starts a turn in every idle tab. The audit producer has the equivalent guard
(`initiator_pushes` = Gui only, `audit/runner.rs:394`); the graph producer has no analogue.

### M3 — A cancelled audit scan still broadcasts "finished"
`audit/runner.rs:669` announces on every exit path; `cancel_scan` (`:576-585`) only fires
the token; `Outcome::Cancelled` classifies as `Failed` (`:865-869`). Scan → Cancel pushes
"cImp finished a security audit … Call security_audit for the full report (it re-runs the
same scan)" into every armed session — inviting agents to re-run the scan the user aborted.

### M4 — Spike rig is live in production paths and bypasses the `offload.session_push` gate
`mcp.rs:270-274` (spawn from `serve`), `:314-316` (spike arm checked **before**
`session_push_enabled()`), `:357-358` (`spike_slow*` in `tools/list`), `:368-370`
(dispatch), `loopback.rs:649-650` (`POST /push_test`). Sole gate: the `CIMP_CHANNEL_SPIKE`
env var, inherited by every child. Left set from the Phase 0 spike, it injects
`SPIKE_INSTRUCTIONS` into every Claude session's system prompt, advertises two fake tools,
and auto-pushes at T+20 s — all with the setting off. `/push_test` (bearer-token auth;
token readable from the discovery file next to the exe) is an arbitrary-text-injection
route into live sessions. The known "spike-rig removal" follow-up understates this: the
rig now spans two processes and overrides the settings gate.

### M5 — The two halves of the push gate are read from different sources at different times
Client half: argv flag composed at PTY spawn (`tabs/config.rs:595`). Server half: fresh
settings read at MCP `initialize` (`mcp.rs:1337` → `current_offload_settings()`), i.e. at
every child (re)handshake, not tab spawn — falsifying `mcp.rs:1330-1336` ("both halves
only ever change together on a tab restart"). Toggle on + ignore restart hint + child
crash-restart ⇒ child declares the channel, injects `CHANNEL_INSTRUCTIONS`, subscribes
`channels=1`, while Claude never registered it: every push silently dropped client-side
while reported delivered, and the "never declared" warning (`mcp.rs:1240-1249`) can't fire.

### M6 — Turning `offload.session_push` OFF does not stop pushes into running Claude tabs
Child gate is `channel_declared()`, latched write-once (`mcp.rs:185-192`); producers check
only `pushes.is_some()` (`graph/service.rs:3145`, `audit/runner.rs:690`); `PushRegistry`
holds no settings handle. Only the restart hint mitigates. Asymmetric with OpenCode, which
re-reads the gate live at delivery (`oob/opencode.rs:364`). If the latch is by-design,
the asymmetry needs an explicit accept + doc; a one-line app-side settings check in the
producers would close it.

### M7 — OpenCode push target is per-connection state: pushes to an idle tab are dropped after any SSE hiccup
`oob/opencode.rs:172,187`: `Tracker`/`last_mark` (sole source of `current_session()`) is
rebuilt per connection; a fresh `/event` stream yields only `server.connected` (verified
against `docs/spikes/v20/ev.ndjson`), so after server restart/stream error the target is
`None` until the session next does something. The queued notice then hits `NoSession` and
is dropped at `debug!` (`:363-368`) — invisible at the default Info level. This defeats
the feature's core use case (notify the idle tab). Fix shape: persist last-known main
session on `OobContext`/the tab.

### M8 — OpenCode child-session exclusion is also per-connection: post-reconnect pushes can land in a sub-agent session
`oob/opencode.rs:451,563`: `child_sessions` is populated only from `session.created`
events on the current connection. Reconnect mid sub-agent run ⇒ the child's deltas set
`last_mark` ⇒ `forward_push` POSTs into the sub-agent session (and V28 memory scoping
follows it). Contradicts the spec's "target is always the tab's MAIN session"
(`MILESTONE-V30-mcp-channels.md:36-37,206-209`); no second defence in `forward_target`.

### M9 — `CLAUDE_CODE_CHILD_SESSION` is not scrubbed from AI-tab spawns
`pty/manager.rs:142-152` only adds env vars; no `env_clear`/`env_remove` path exists.
cImp launched from inside a Claude Code session (routine during development) spawns
Claude tabs inheriting `CLAUDE_CODE_CHILD_SESSION=1` → no transcript, no history
(spike-documented, `MILESTONE-V30-mcp-channels.md:152-156`) → the oob tap has nothing to
tail: no TTS, no usage, no live-session registry entry, no V28 scoping — all silent.
Cheap fix: explicit strip list at spawn (needs an `env_remove` on `PtyLaunchSpec`).

### M10 — The 2cdfc55 skip-log contract has no consumer at the shipped log level
`oob/claude.rs:1257` logs skipped transcript lines at `debug!`; default level is Info
(`settings/schema.rs:606-615`). A format change that fails every line — the drift this
contract exists to detect — produces zero visible output. Same gap, unlogged entirely,
for unparseable OpenCode SSE payloads (`oob/opencode.rs:214`). Suggest `warn!`-once-per-file
or a counter surfaced via the harness-version tripwire.

## MEDIUM — permission detection (#5)

### M11 — Hook `Resolved` clears `awaiting_permission` while a prompt is still on screen; the latched regex cannot re-raise it
`loopback.rs:1408-1413` justifies the eager clear with "the regex fallback re-detects" —
false: `PermissionDetector::check` is edge-triggered on a latched name
(`processing/permission.rs:343-375`); `force_clear` is wired only for `Working`
(`pty/tasks.rs:404`). An auto-denied call concurrent with a visible approval prompt clears
badge + TTS and nothing re-raises until a keystroke.

### M12 — A non-empty but unrecognized `notification_type` short-circuits classification
`notify_hook.rs:121-126` accepts bare top-level `type`; `loopback.rs:1431-1436` returns
early for any non-empty kind that isn't `permission_prompt`, so the prose fallback at
`:1400` never runs. Payload shape is explicitly UNVERIFIED (`notify_hook.rs:29-38`); a
shape/rename drift makes every permission notification classify "ignored" at `debug!` —
inverting the stated goal ("degrades to ignored, never to silence", `tabs/config.rs:504-508`),
because for the permission case ignored *is* silence. Fall through to the prose check.

### M13 — The `legacy_default_sets` append-only rule is unenforced; the tripwire guards the opposite direction
`patterns_file.rs:83-98` documents "append the outgoing default set";
`current_defaults_are_not_a_legacy_set` (`:437-449`) fails only if you add the *current*
set. Forgetting the append means installs that took the interim release hold a list
matching no snapshot and never receive future default fixes — the exact cfcec66 bug,
reintroducible silently.

## MEDIUM — usage/context bar (#14)

### M14 — With two Claude tabs, the context bar structurally shows the quota-carrying tab's session, never stale
`usage/mod.rs:287-298` (`should_write`): quota-less pushes are suppressed while a
quota-carrying push is <90 s old; refresh is 30 s, so an idle `claude` tab permanently
wins over the working `claude-local` tab — fresh-looking, undimmed, wrong-session numbers
with no attribution beyond a hover tooltip. Related: a context-only push evicts quota
data at 90 s, bypassing the documented 30-min `HIDE_AFTER` window (`usage/mod.rs:208,332`)
— the stale-display contract depends on how many Claude tabs are open.

### M15 — Widget gated on the `claude` tab being enabled while `claude-local` also pushes
`UsageMeter.svelte:63,69,220`: `enabled` requires `enabled_ai_tabs.includes('claude')`,
but statusline injection is command-based (`command_is(...,"claude")`,
`tabs/config.rs:358,383`) so `claude-local` pushes too. Disable the subscription tab, work
in Claude (local): valid context snapshots arrive and the bar never renders (and the poll
effect short-circuits, so no diagnosis path).

### M16 — `cacheHitPct` fabricates its denominator from absent fields
`contextMeter.ts:65`: `?? 0` on both denominator terms violates the file's own
"absent is not zero" rule (`:6-12`). Reachable via the hoisted-field drift the backend
deliberately tolerates (`statusline/mod.rs:318,371`): hoisted `cache_read_tokens` without
`input_tokens` renders a solid 100 % cache hit; the behavior is pinned as intended by
`contextMeter.test.ts:83`, so tests won't catch it.

## MEDIUM — V29 / deps / CI

### M17 — Every kept-alive terminal holds a live WebGL2 context; ≥17 terminal tabs hit WebView2's context cap and churn
`terminals.ts:442` (call site `657-659`): all tabs get WebGL at startup (`App.svelte:153`),
parked offscreen, never torn down; no tab cap exists. Past ~16 contexts Chromium evicts
LRU → 3 s frozen terminal (webgl addon's context-loss timer) → single retry creates a
fresh context → evicts another: a bounded wave of 3 s freezes through the tab set. V29
spec never considers context count; zero test coverage of `@xterm/*`.

### M18 — MAINTENANCE.md contradicts itself about the ort re-gate and MSRV
`docs/MAINTENANCE.md:297-305` still says `webgpu` is on the dep unconditionally and
"treat `tts-cuda` as untested until re-gated" (it was re-gated in b82040d;
`Cargo.toml:150` and MAINTENANCE.md:164 say so); `:226` still declares MSRV 1.82 with the
correction listed as outstanding (`Cargo.toml:7` is 1.88, resolver v3 active). Both
directly mislead the next maintenance run, which is briefed off this file — b82040d's
message claims these were fixed.

### M19 — CI blind spots: clippy lints default features only; no CI job runs any test
`clippy.yml:78` has no `--features`, so the only feature-gated first-party code —
`tts/engine.rs:10-48`, exactly the shipped release path (`release.yml:213` builds
`stt-vulkan,tts-webgpu`) — is never linted; an `ort::ep` break first surfaces in the
40-min tag-triggered release job. No workflow runs `cargo test`/vitest, so every tripwire
added in this range (`preview::capture::lock_pins`, the osv-source pin, the `--settings`
overlay contract) has no automated consumer. `--features tts-webgpu` on the clippy job
closes the lint gap; a test job closes the rest. Related: the webgpu+cuda mutual-exclusion
silent-failure mode (`Cargo.toml:52-59`) is documented but not enforced
(`compile_error!` absent; `engine.rs:34-48` makes the combo compile and *log* a GPU
backend while running CPU-only ORT), and the shipped feature set has not been compiled by
anything since the ort dep changed (last release build predates b82040d).

---

## LOW (abbreviated; full detail in the per-slice reviews)

- **Push bus**: OpenCode taps register `channels: true` unconditionally, inflating every
  delivery count (`oob/mod.rs:230-243`) — the bus's one delivery signal is a non-signal
  for OpenCode tabs, and `push_to_tab`'s bool "lies" per its own doc (`service.rs:501-505`).
  Push POST awaited inline in the SSE select: up to 5 s stall per notice for TTS/cancel;
  tab-restart window where two subscribers share a tab id (double-inject ≤5 s).
  `record_channel_declaration` check-then-set race (fix: `compare_exchange`). Dead
  `ClientInit.capabilities` + accessor with stale "Phase B reads it" rationale. `/events`
  relay released before the `initialize` response is serialized (protocol-order violation,
  practically unreachable). OpenCode `render_channel_envelope` doesn't escape or
  empty-check `content` (Claude side does).
- **Audit producer** has no duration floor unlike its graph twin (`GRAPH_PUSH_MIN_BUILD_MS`);
  a 200 ms scan pushes a turn into every armed session.
- **Spawn-sig**: `"channels"` sig entry flips even when the flag can't change argv
  (offload/graph/MCP all off) → spurious restart hints (`tabs/config.rs:317` vs `:595`).
- **Permission regex**: `claude_permission_bare` can fire on Claude's own prose
  ("1. Yes" + "to cancel" within the 1000-char tail; untested combination); `none_of`
  vetoes evaluate the whole tail, so a nearby picker footer suppresses a real prompt;
  empty-but-present `patterns.json` accepted silently (avatar Working pattern lost too),
  0-byte file never repaired on disk.
- **Usage**: unclamped percentage text next to a clamped bar (can render "143 %");
  `hasContextData` doesn't mirror `is_substantive` despite claiming to, and
  `remaining_percentage` has no consumer anywhere; `should_write` TOCTOU can evict a
  fresh quota push for one 30 s beat; statusline context ratio can mix top-level
  numerator with block denominator on future payload drift (`statusline/mod.rs:342-343`).
- **V29**: stale rationale comment on post-open renderer load (xterm 6 swallows
  `onWillOpen` throws); settings-window reveal races theme CSS/registry (visible only
  with a user theme with `decorations: true`); `showWindowOnce` swallows `show()`
  rejection with a bare catch.
- **Deps/CI**: clippy.yml lacks `--locked` and `cache-on-failure`, paths filter omits the
  frontend it builds, MSRV 1.88 never verified by CI (runs 1.97.1); portable-readme lists
  ORT DLLs the webgpu build doesn't ship; `clipboard read-image` granted to `settings`
  window that never uses it; the rfd `wayland` feature self-inflicted the two quick-xml
  advisories (justifications in audit.toml are accurate); audit.toml has no consumer on
  the primary (osv) advisory path, so the accepted advisories keep resurfacing there.
- **kv-unified display**: dashboard per-slot context bar uses the raw `/slots` `n_ctx`
  instead of the corrected value (`metrics.rs:271-276` vs `server.rs:563-566` claim) —
  the two numbers on the same card disagree under `--kv-unified`. Display-only; the
  router/budget path is correct and the regression test is intact.

---

## Verified clean (load-bearing checks that passed)

- `--kv-unified` per-slot budget: no double-divide; `per_slot_budget_uses_props_value_directly`
  intact; child probe divides raw `/props`, not a corrected value.
- `PushRegistry` concurrency: no lock across await, bounded queue (32) with drop+warn,
  RAII deregistration on every exit path, per-instance port+token scoping.
- Audit self-push loop guard (`Initiator::Agent` never pushes) holds on both call paths;
  OpenCode tabs cannot receive a notice twice over two transports.
- Migration v28→v29: idempotent, monotonic, future-version safe, nothing dropped;
  `session_push` defaults OFF on both Rust and TS sides; restart hint wired + tested.
- WebGL→DOM fallback genuinely repaints (no silent black terminal); addon lifecycle,
  double-dispose, destroy-path retry guard all correct.
- Usage push file: atomic write (per-PID temp + rename); absent/empty/malformed handled
  on both sides; Rust↔TS field contract exact; humanize buckets agree.
- Hook + regex double-fire collapses to one TTS/notification (edge-guarded flag).
- rodio 0.22/cpal 0.17 lockstep held (single cpal in lock); ort re-gate correct, default
  build shows no CPU-fallback warning; RUSTSEC baseline justifications verified accurate.
- Clippy sweep (76e4f13): every hunk checked in graph/audit/workbench/stt/tts/audio/
  advisor/build.rs is semantically inert (incl. the `&&` short-circuit at `main.rs:659-667`
  that would have been a bug with `&`); cpal `name()` ≡ `description().name()` on WASAPI.
- Test suites: `npx vitest run` 465/465 pass; contextMeter 15/15. (Cargo tests not run by
  reviewers; findings are source-verified.)

## Suggested dispositions

Blocking for the V30 live-verify/release: H1, H2, M4 (spike rig), M7/M8 (reconnect state),
M1/M2/M3 (broadcast + origin gates — these define the feature's blast radius), M9, M12.
Fix-cheap alongside: M5/M6 (or explicitly accept the latch asymmetry + doc), M10, M11, M13.
Schedulable: M14–M16 (usage), M17 (xterm context cap — needs a policy, e.g. WebGL for the
active/visible tab only), M18 (doc fix, 10 min), M19 (CI: add `--features tts-webgpu` +
a test job). LOWs: batch at leisure; none urgent.

Per-slice reviewer reports (full LOW detail + assumptions) available in session transcripts;
this file is the consolidated record.

---

# Fix run — 2026-08-05 (same day, uncommitted working tree)

Every finding above was fixed in eight sequential batches (A: V30 push core — spike rig
removed, argv-baked child gate, live-settings/origin/cancellation/duration gates on both
producers; B: OpenCode oob — reconnect-persistent target + child set with a session.get
parentID probe, env scrub via PtyLaunchSpec.env_remove, warn-once skip logs, gate-driven
subscription, non-blocking push task, envelope hygiene; C: H1 — live_tab_roots ambiguity
predicate at the registry seam, shared ai_working_dir root definition; D: H2 gate + sig +
tripwire, Resolved force-clear + re-scan, prose fallback, append-rule tripwire, regex
hardenings, empty-patterns warning; E: format-2 push file with per-slot aging + context
ownership + visible attribution, honest cacheHitPct, command-based enablement; F:
visibility-bound WebGL policy (shouldHoldWebgl), reveal ordering, kv-unified per-slot
display; G: MAINTENANCE.md truth sweep (13 stale rows), clippy --features tts-webgpu
--locked, new tests.yml, webgpu×cuda compile_error!, capability narrowing; H: verifier
residuals — 30 s tap heartbeat vs TTL starvation, normalized root keys via
fsutil::norm_dir_key, spawn-ordering comment, blind-spot doc, Lagged→current() re-check).

Deliberate deviations from the findings as written, decided by the orchestrator:
- M1: RunBody/AuditRunBody did NOT gain a `tab` field — the spec's origin-tab producers
  were obsoleted by native auto-backgrounding; the field would be a signal with no
  consumer. Blast radius is bounded at the source instead. Recorded in the V30 spec.
- H2: default installs are regex-only BY DESIGN (hook injection follows loopback_needed
  rather than the loopback becoming always-on) — preserves the v0.48.0 serve-gate decision.
- H1 accepted residuals (documented in MILESTONE-V28): two same-root tabs degrade to
  UNSCOPED (fail-open), not isolated; external claude processes (shell tab/terminal/agent)
  remain an undetectable co-tenant that can produce confident-wrong scoping.
- G4: the both-features-on build emits 3 errors (compile_error! first) rather than 1 —
  dead precedence logic removed in exchange.

Verification: H1/H2 were adversarially re-verified by an independent agent (H2 CONFIRMED
FIXED; H1 FIXED WITH RESIDUALS → actionable residuals patched in batch H, accepted ones
documented). Final tree: cargo test --locked --bin cimp all green (~1430 tests),
npx vitest run 482/482, cargo clippy --all-targets clean on default AND tts-webgpu
feature sets, svelte-check 0 errors.
