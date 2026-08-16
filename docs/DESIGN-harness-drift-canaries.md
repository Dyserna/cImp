# Harness drift canaries (draft)

**Status:** draft, not implemented. Companion to
`docs/DESIGN-harness-capability-matrix.md` — the matrix declares what we
depend on; canaries prove it still holds. Canary ids are matrix capability
ids; the two files must agree (enforced, §6).

## 1. Why V16's drift rules are not enough

V16 shipped eight real drift rules (`advisor.rs:149-156`). They work, and
this design keeps every one of them. But they share a structural property:

**they are lagging statistical indicators.** `drift.usage_fields_gone.v1`
needs N Claude sessions with no token fields before it fires.
`drift.read_reason.v1` needs enough remind→reread pairs to compute a rate.
`drift.injection_unseen.v1` needs a follow-rate to collapse. Every one of
them detects drift by *watching real work degrade first*.

That is exactly the experience of "always following behind": the harness
updates, sessions quietly get worse, and some number of degraded sessions
later an Advisor card appears. Meanwhile the one *leading* signal —
`drift.harness_version.v1` — fires on every auto-update regardless of whether
anything broke, so it is trained away by reflexive "Mark verified".

**The gap is a leading, precise check**: something that runs in seconds
against the installed CLI, says which capabilities still hold, and is
believable enough that "Mark verified" stops being reflexive.

### 1.1 The specific failure mode canaries must catch

Every reader in the tree is deliberately, correctly lenient:

```rust
// oob/claude.rs::parse_usage_line
let tok = |k: &str| -> u32 {
    usage.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0) as u32
};
```

```rust
// statusline/mod.rs — "a parse failure yields Input::default()"
#[derive(Deserialize, Default)]
struct Input { #[serde(default)] model: Model, ... }
```

Leniency is the right call for a shim that must never break a user's turn.
But it means **an upstream rename produces zeros and empty strings, not
errors**. Nothing throws. Nothing logs. The usage widget just reads 0.

So the canary assertion is *not* "does it parse" — it always parses. It is
**"does real input produce substantive output"**. This is global principle 5
(*empty is not absent*) applied to harness readers, and it is the single most
important design decision in this document.

## 2. Two layers

| | L1 — fixture contract tests | L2 — live probe |
|---|---|---|
| Runs | every `cargo test` | on version change, maintenance runs, on demand |
| Input | committed fixtures | the installed CLI, live |
| Catches | *our* readers regressing | *upstream* drifting |
| Cost | none | seconds, needs the CLI present |
| Answers | "do we still parse the shape we recorded" | "is the recorded shape still real" |

Both are needed. L1 alone pins us to a snapshot of the past and passes
forever while reality moves. L2 alone cannot run in CI and cannot tell a
reader regression from an upstream change.

## 3. L1 — fixture contract tests

### 3.1 Layout

```
src-tauri/fixtures/harness/
  claude/
    2.1.14/
      transcript.assistant-usage.jsonl     # one line per shape
      transcript.tool-result.jsonl
      transcript.subagent.jsonl
      statusline-stdin.json
      hook.user-prompt-submit.json
      hook.notification.flat.json
      hook.notification.nested.json        # both shapes — docs are ambiguous
      hook.posttooluse.json
      MANIFEST.toml
  opencode/
    1.19.2/
      sse.message-updated.json
      sse.message-part-delta.json
      sse.session-idle.json
      tool-ids.json                        # GET /experimental/tool/ids
      MANIFEST.toml
```

`MANIFEST.toml` records provenance — captured-from version, date, capture
method, and whether it was redacted. A fixture with no manifest fails the
suite; an anonymous fixture is indistinguishable from a guess.

### 3.2 Fixtures are synthetic-minimal, never raw captures

**Non-negotiable:** committed fixtures are hand-minimized to the fields the
matrix names. Real transcripts contain user prompts, file contents, tool
output, and plausibly credentials — they must never land in git.

- `fixtures/harness/**` — committed, synthetic-minimal, redacted, reviewed.
- `<data-dir>/harness-captures/**` — gitignored, real captures from L2, used
  locally to *build* fixtures and to diff when something breaks.

The capture path (`cimp --harness-capture`) writes only to the gitignored
location and prints the path. Promotion to a committed fixture is a manual,
reviewed step. Reuse `processing/sanitize.rs` on the capture path so even the
local corpus is scrubbed by default.

### 3.3 The assertion shape

```rust
#[test]
fn canary_claude_transcript_usage() {
    let line = fixture("claude/2.1.14/transcript.assistant-usage.jsonl");
    let ev = parse_usage_line(&json(&line), UsageOrigin::Main)
        .expect("claude.transcript.usage: no UsageEvent from a real assistant line");

    let UsageEvent::Turn { msg_id, model, in_tok, out_tok, cache_read, cache_make, .. } = ev
        else { panic!("wrong variant") };

    // Substantiveness — the whole point. Each of these is a field that a
    // rename would zero out silently in production.
    assert!(!msg_id.is_empty(),        "message.id gone");
    assert!(model.is_some(),           "message.model gone");
    assert!(in_tok  > 0,               "usage.input_tokens gone");
    assert!(out_tok > 0,               "usage.output_tokens gone");
    assert!(cache_read > 0,            "usage.cache_read_input_tokens gone");
    assert!(cache_make > 0,            "usage.cache_creation_input_tokens gone");
}
```

Note the fixture is chosen so *every* field is non-zero. A fixture with a
legitimately-zero field cannot distinguish "absent" from "zero", which
defeats the test. Fixture selection is part of the contract.

The same shape covers: tool_result extraction (id present, chars > 0,
`is_error` round-trips both ways), subagent file discovery (≥1 file found,
≥1 event parsed), statusline (`used_percentage > 0`,
`context_window_size > 0`, `rate_limits` present), OpenCode SSE (session id,
message id, and delta text all non-empty), and the notification payload in
**both** flat and nested forms.

### 3.4 Negative canaries

For each capability, one fixture with the field *renamed*, asserting the
reader reports it rather than silently returning a default. These are the
tests that prove the canary itself works — without them the suite can pass
because the assertion never ran.

```rust
#[test]
fn canary_detects_a_renamed_usage_field() {
    let line = fixture("claude/_synthetic/usage-renamed-input-tokens.jsonl");
    let ev = parse_usage_line(&json(&line), UsageOrigin::Main).unwrap();
    // Today this silently yields 0. The canary must fail here, loudly.
    assert_eq!(in_tok_of(&ev), 0, "guard: this fixture models the drift case");
}
```

## 4. L2 — live probe

`cimp --harness-canary [--json]` — no app instance required, exits non-zero
on failure so it is usable from CI and from a maintenance script.

Per capability, drive the *real* surface:

| Capability | Probe |
|---|---|
| `claude.statusline.stdin` | run `cimp --statusline` against a captured-live stdin from the installed CLI; assert the bar renders with non-default values |
| `claude.hook.*` | install a temporary echo hook via `--settings`, run one scripted turn, capture the payload, assert required fields present |
| `claude.flag.session_id` / `settings_overlay` | `claude --help` contains the flag; overlay round-trips without an "unknown key" rejection |
| `claude.transcript.*` | tail the newest session JSONL, assert ≥1 line of each shape parses substantively |
| `opencode.tool_registry` | `opencode serve` + `GET /experimental/tool/ids`, diff against `OPENCODE_NATIVE_TABLE`, **fail on any unclassified id** |
| `opencode.sse.events` | connect `GET /event`, run one scripted turn, assert each event kind arrives with its `properties.*` fields |
| `opencode.route.noauth` | unauthenticated call succeeds (a 401 means auth landed — flip the capability, do not fail the build) |

Two probe outcomes are *not* failures and must be modelled distinctly, or the
suite cries wolf and gets ignored — the exact fate of the version tripwire:

- **Unavailable** — CLI not installed, or no session to tail. Report as
  unknown, never as broken.
- **Changed-but-better** — e.g. OpenCode grew auth. Surfaces as a capability
  transition, not a red test.

### 4.1 Capture-on-success

A passing probe writes its observed payloads to the gitignored capture dir
stamped with the CLI version. Over time this builds a per-version corpus, so
when something does break the first diagnostic is a diff between the last
known-good capture and today's, instead of reverse-engineering from symptoms.
This is the cheapest part of the design and probably the highest-leverage
during an actual breakage.

## 5. Replacing the noisy tripwire

Today: `claude_last_seen != claude_last_verified` ⇒ raise
`drift.harness_version.v1` ⇒ user clicks **Mark verified** (usually without
running the recipes, because running them costs ten minutes and the update
almost never broke anything).

Proposed:

1. Version change detected by the OOB tap (unchanged).
2. **L1 runs automatically** (it is already in the binary, costs nothing).
3. **L2 runs automatically** if the CLI is reachable.
4. The Advisor notice is raised **only for capabilities that actually
   failed**, naming them, with the matrix's `wired_in` paths as the fix
   pointer. If everything passed, `claude_last_verified` advances **on its
   own** — no click.
5. **Mark verified** survives only for Tier-D `Behavior` deps that no probe
   can settle (the D0/E1-style spikes). That is a handful of rows, so the
   button means something again.

This is the change that most directly addresses "constantly adapting": most
upstream updates become a silent auto-verify, and the ones that matter arrive
as a specific named capability with a file path — before a user hits it.

## 6. Wiring back to the matrix

- Canary ids **are** capability ids.
- A `Silent` capability with no canary and no recorded waiver fails the build
  (matrix doc §5) — the rule that keeps new fragile dependencies from
  entering unrecorded.
- A canary whose id is not in the matrix fails too, so the suite cannot drift
  into testing things nobody declared.

## 7. Phasing

| Phase | Work | Value |
|---|---|---|
| **A** | Fixture layout + manifest + L1 for the four Tier-C readers (transcript usage, tool_result, statusline, OpenCode SSE) | The silent-zeros class becomes a red test |
| **B** | Negative canaries + the matrix cross-check | The canaries are themselves proven |
| **C** | `cimp --harness-canary` with the `opencode.tool_registry` diff first | Automates the one manual security-relevant check |
| **D** | Auto-run on version change; auto-advance `claude_last_verified` | The tripwire stops crying wolf |
| **E** | Capture-on-success corpus | Breakages become a diff, not an investigation |

Phase A + C alone would have caught the Task→Agent subagent rename and would
close the standing `OPENCODE_NATIVE_TABLE` maintenance obligation, which is
currently a human remembering to run a diff.

## 8. Honest limits

- **Behavior contracts stay manual.** Whether a `PreToolUse` deny reason
  reaches the *model*, whether a hook timeout blocks — no payload reveals
  these. They remain spikes (D0/E1/OpenCode-veto). Canaries reduce the manual
  surface to roughly those three rows; they do not eliminate it.
- **L2 needs a scripted turn** for the hook and SSE probes, which means an
  API call and a few seconds. Acceptable on version-change and maintenance
  cadence; not something to run at every tab spawn.
- **Fixtures rot.** A committed fixture from an old version keeps L1 green
  while reality moves — which is precisely why L2 exists, and why L2 should
  refresh fixtures on success rather than only asserting against them.
- **This does not reduce the number of things cImp depends on.** It makes
  them enumerable, tested, and loud. Reducing the count is the D→C→B→A
  migration work the matrix makes visible — a separate, ongoing effort.
