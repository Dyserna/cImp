//! V35 Phase B — L1 canaries for the five Tier-C readers.
//!
//! # What these assert, and why it is not "does it parse"
//!
//! Every reader pinned here is deliberately lenient. `parse_usage_line` ends
//! each token lookup in `unwrap_or(0)`; `statusline/mod.rs` documents that "a
//! parse failure yields `Input::default()`" and walks the push payload as a raw
//! `Value` where every field is an `Option`; `Tracker::handle` `match`es on the
//! SSE event `type` and ignores everything it does not know. That leniency is
//! correct — a shim must never break a user's turn over an unexpected field —
//! but it means **an upstream rename produces zeros and empty strings, not
//! errors**. Nothing throws, nothing logs, the usage widget just reads 0.
//!
//! So a canary asserts **substantiveness**: fed a fixture of the shape we
//! recorded, does the reader still produce a non-zero, non-empty result?
//! (Milestone locked decision 3; global principle 5, *empty is not absent*.)
//! Every fixture is authored so that every asserted field is legitimately
//! non-zero — a fixture with a real zero in it cannot tell "absent" from
//! "zero" and quietly defeats the test. **Fixture selection is part of the
//! contract.**
//!
//! # Two callers, ONE code path (V35 Phase F)
//!
//! Phase B shipped this module as `#[cfg(test)]`, which was enough while
//! `cargo test` was the only consumer. Phase F made the canaries run **in the
//! shipped binary**, in the background, whenever the installed Claude Code
//! version changes — so the five positive assertions are now ordinary
//! functions returning `Result<(), String>` ([`claude_transcript_usage`],
//! [`claude_transcript_tool_result`], [`claude_transcript_assistant_text`],
//! [`claude_statusline_stdin`],
//! [`opencode_sse_events`], dispatched by [`run_embedded`]), and the `#[test]`s
//! below are thin wrappers that assert they return `Ok`.
//!
//! That shape is the point: if `cargo test` drove a *copy* of the assertions,
//! coverage would fork — the suite could go green while the auto-verify that
//! advances `claude_last_verified` checked something else. The negative
//! canaries additionally assert that the very same functions return `Err` on a
//! drift fixture, so "the canary fires" is proven about the production path and
//! not merely about a test.
//!
//! # Fixtures
//!
//! `src-tauri/fixtures/harness/<harness>/<version>/<name>`. The five
//! **positive** fixtures are `include_str!`-embedded (a release binary has no
//! repo tree to load them from — the milestone deploy trap allows exactly this:
//! "`include_str!` only for the small synthetic fixtures"). Everything else —
//! the `_synthetic/` drift models and the manifest walker — loads from disk
//! through [`fixture`] at test runtime, where the tree exists.
//!
//! They are synthetic-minimal and hand-authored from the reader code's
//! contract, never copied from a real transcript: real transcripts carry user
//! prompts, file contents, tool output and plausibly credentials (locked
//! decision 4). Each version directory carries a `MANIFEST.toml` recording where
//! the shape came from, and [`tests::every_fixture_version_dir_has_a_manifest`]
//! fails the suite for a directory without one — an anonymous fixture is
//! indistinguishable from a guess.
//!
//! The Phase C drift models live beside the version directories in
//! `<harness>/_synthetic/` and carry a manifest under the *same* rule plus one
//! extra key (`models_version`, which must name a real sibling version
//! directory). `_synthetic` is deliberately **not** exempted from the walker:
//! an exemption is exactly the silent hole through which undated fixtures
//! would accumulate.
//!
//! # One module, one naming rule
//!
//! Every canary lives here and is named `canary_<capability id with dots as
//! underscores>`, its negative twin `negative_canary_<same>`, and [`tests::row`]
//! re-asserts on every run that the registry row it claims points back at it.
//! **A canary id IS a capability id** — never a third namespace. That is what
//! lets [`tests::canaries_and_the_matrix_agree`] cross-check the suite against
//! the registry mechanically instead of against a hand-maintained list, and what
//! lets [`EMBEDDED`] be checked against the registry's `canary` column rather
//! than trusted.
//!
//! # Negative canaries (Phase C)
//!
//! A positive canary that never actually ran passes just as green as one that
//! did. So each covered capability also gets a **drift model**: the same
//! fixture with one load-bearing field renamed, and a test asserting the reader
//! answers with its degraded default — zero, empty, `None`, no speech. Phase B
//! established this by hand-mutating fixtures once; Phase C makes it permanent.
//! Every one of them is a `guard: this fixture models the drift case` assertion
//! (design doc § 3.4): it does not describe desired behavior, it pins today's
//! silent-degradation behavior so the positive canary's assertion is proven to
//! be load-bearing. Each also asserts the *untouched* half of the same fixture
//! still works, so a broken fixture cannot masquerade as a proven mechanism.

use serde_json::Value;

// ── the embedded corpus (V35 Phase F) ───────────────────────────────────────

/// The five positive fixtures, embedded so the canaries run from a release
/// binary. A missing or renamed fixture is a **compile** error, which is the
/// other half of why these five are embedded rather than path-loaded: the
/// runtime canary can never degrade to "fixture not found ⇒ skipped".
const FIXTURE_CLAUDE_USAGE: &str =
    include_str!("../../fixtures/harness/claude/2.1.232/transcript.assistant-usage.jsonl");
const FIXTURE_CLAUDE_TOOL_RESULT: &str =
    include_str!("../../fixtures/harness/claude/2.1.232/transcript.tool-result.jsonl");
/// V35 Phase L. The fifth, and the one whose ABSENCE was the finding: assistant
/// prose → TTS was a live Tier-C dependency with no registry row and no canary,
/// because Phase B seeded the rows it could point a *named reader function* at
/// and this one's reader (`assistant_texts`) was inlined in the drain loop.
/// Phase L needed the row anyway — a `Fallback { to: .. }` cannot point at a
/// capability that does not exist.
const FIXTURE_CLAUDE_ASSISTANT_TEXT: &str =
    include_str!("../../fixtures/harness/claude/2.1.232/transcript.assistant-text.jsonl");
/// V39. The turn BOUNDARY the delegation completion feed derives from the
/// transcript — a different contract from the text blocks above, in a different
/// field, breaking differently: `assistant_texts` going empty makes a tab go
/// mute, `stop_reason` going missing makes every delegation into a Claude tab
/// wait out its deadline. Two lines, because the contract is a DISTINCTION.
const FIXTURE_CLAUDE_STOP_REASON: &str =
    include_str!("../../fixtures/harness/claude/2.1.232/transcript.stop-reason.jsonl");
const FIXTURE_CLAUDE_STATUSLINE: &str =
    include_str!("../../fixtures/harness/claude/2.1.232/statusline-stdin.json");
const FIXTURE_OPENCODE_SSE: &str =
    include_str!("../../fixtures/harness/opencode/1.18.13/sse.assistant-turn.jsonl");

/// The capability ids with an embedded, runtime-callable canary, in the order
/// [`crate::harness::verify`] runs them.
///
/// Set-compared against the registry's `canary: Some(..)` column in both
/// directions by [`tests::embedded_canaries_are_exactly_the_declared_ones`]: a
/// declared canary missing from here would be a row the auto-verify silently
/// never checks, and an entry here with no row would be a check nobody
/// declared.
pub const EMBEDDED: &[&str] = &[
    "claude.transcript.usage",
    "claude.transcript.tool_result",
    "claude.transcript.assistant_text",
    "claude.transcript.stop_reason",
    "claude.statusline.stdin",
    "opencode.sse.events",
];

/// Run one embedded canary by capability id. `None` when the id has no
/// embedded canary — deliberately distinct from `Some(Err(..))`, because
/// "nothing checks this" and "this failed" must never be the same value
/// (a `Fail` blocks the auto-advance; an absent canary must not).
///
/// **Blocking.** `opencode.sse.events` drives an `async` reader, so this parks
/// a private current-thread runtime on it — which means this function must NOT
/// be called from inside an async context (it would panic). The auto-verify
/// worker is a plain OS thread; the async test drives
/// [`opencode_sse_events`] directly instead.
pub fn run_embedded(id: &str) -> Option<Result<(), String>> {
    match id {
        "claude.transcript.usage" => Some(claude_transcript_usage()),
        "claude.transcript.tool_result" => Some(claude_transcript_tool_result()),
        "claude.transcript.assistant_text" => Some(claude_transcript_assistant_text()),
        "claude.transcript.stop_reason" => Some(claude_transcript_stop_reason()),
        "claude.statusline.stdin" => Some(claude_statusline_stdin()),
        "opencode.sse.events" => Some(block_on_current_thread(opencode_sse_events())),
        _ => None,
    }
}

/// Drive one future to completion on a private current-thread runtime — the
/// idiom `offload::mcp` and `sandbox` already use for "an async call from a
/// blocking context". A runtime that cannot be built is reported as a canary
/// failure rather than swallowed: it means the check did not run, and the whole
/// point of Phase F is that an unrun check never looks like a passing one.
fn block_on_current_thread(fut: impl std::future::Future<Output = Result<(), String>>) -> Result<(), String> {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(fut),
        Err(e) => Err(format!(
            "the canary could not be run at all: building a current-thread runtime failed ({e})"
        )),
    }
}

/// `return Err(format!(..))` unless the condition holds — the runtime canaries'
/// `assert!`. Spelled as a macro so each check reads like the assertion it
/// replaced and keeps its message verbatim.
// Written as `if cond {} else {}` rather than `if !cond {}` on purpose: several
// of the conditions are float comparisons, and negating a `PartialOrd`
// comparison is a clippy denial (`neg_cmp_op_on_partial_ord`) — for a good
// reason, since `!(x > 0.0)` is also true for `NaN`.
macro_rules! substantive {
    ($cond:expr, $($msg:tt)*) => {
        if $cond {
        } else {
            return Err(format!($($msg)*));
        }
    };
}

/// The non-empty lines of a `.jsonl` fixture, parsed. A malformed fixture is a
/// defect in the fixture rather than a drift signal, so it says so.
fn parse_lines(raw: &str) -> Result<Vec<Value>, String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l)
                .map_err(|e| format!("fixture is not valid JSON ({e}): {}", l.trim()))
        })
        .collect()
}

// ── claude.transcript.usage ─────────────────────────────────────────────────

/// `harness/claude/read.rs::parse_usage_line` still lifts all four token counters, the
/// message id and the model out of an assistant transcript line.
///
/// The failure this catches: `usage.input_tokens` is renamed upstream,
/// `unwrap_or(0)` turns that into a `0`, the row is UPSERTed as a zero-token
/// turn, and the Usage tab quietly reports a session that spent nothing.
pub fn claude_transcript_usage() -> Result<(), String> {
    check_claude_transcript_usage(FIXTURE_CLAUDE_USAGE)
}

/// The assertion, over an arbitrary fixture body — so the negative twin can
/// prove this exact function answers `Err` on the drift model.
fn check_claude_transcript_usage(raw: &str) -> Result<(), String> {
    let lines = parse_lines(raw)?;
    substantive!(
        lines.len() == 1,
        "fixture guard: expected one assistant line, got {}",
        lines.len()
    );

    let Some(ev) =
        crate::harness::claude::read::parse_usage_line(&lines[0], crate::graph::UsageOrigin::Session)
    else {
        return Err("claude.transcript.usage: no UsageEvent from an assistant line (`type`, \
                    `message` or `message.id` gone)"
            .to_string());
    };
    let crate::graph::UsageEvent::Turn {
        msg_id,
        model,
        in_tok,
        out_tok,
        cache_read,
        cache_make,
        origin,
    } = ev
    else {
        return Err(
            "claude.transcript.usage: an assistant line produced a non-Turn event".to_string(),
        );
    };

    // Substantiveness — the whole point. Every one of these is a field a
    // rename would zero out silently in production.
    substantive!(!msg_id.is_empty(), "message.id gone");
    substantive!(
        model.is_some_and(|m| !m.is_empty()),
        "message.model gone"
    );
    substantive!(in_tok > 0, "message.usage.input_tokens gone");
    substantive!(out_tok > 0, "message.usage.output_tokens gone");
    substantive!(cache_read > 0, "message.usage.cache_read_input_tokens gone");
    substantive!(
        cache_make > 0,
        "message.usage.cache_creation_input_tokens gone"
    );
    substantive!(
        origin == crate::graph::UsageOrigin::Session,
        "the requested UsageOrigin did not survive the parse"
    );
    Ok(())
}

// ── claude.transcript.tool_result ───────────────────────────────────────────

/// `harness/claude/read.rs::extract_tool_results` still finds both `tool_result` content
/// shapes in a user line, and `tool_result_is_error` still reads the flag in
/// both directions.
///
/// Note which reader owns which field: `extract_tool_results` reads `type`,
/// `message.content[]`, `tool_use_id` and `content` (string **or** array of
/// `{type:"text", text}` blocks) — it does **not** look at `is_error` at all.
/// `is_error` is read by `tool_result_is_error`, whose consumer is the
/// session→commit provenance tap (`record_commit_events`): a failed `git
/// commit` must never be mined for hashes. Both readers sit behind the same
/// registry row, so the canary drives both.
pub fn claude_transcript_tool_result() -> Result<(), String> {
    check_claude_transcript_tool_result(FIXTURE_CLAUDE_TOOL_RESULT)
}

fn check_claude_transcript_tool_result(raw: &str) -> Result<(), String> {
    let lines = parse_lines(raw)?;
    substantive!(
        lines.len() == 1,
        "fixture guard: expected one user line, got {}",
        lines.len()
    );
    let line = &lines[0];

    let results = crate::harness::claude::read::extract_tool_results(line);
    substantive!(
        results.len() == 2,
        "claude.transcript.tool_result: expected both `tool_result` blocks (string content AND \
         text-block array); got {results:?} — `type`, `message.content[].type` or `tool_use_id` \
         moved"
    );
    for (id, chars) in &results {
        substantive!(!id.is_empty(), "message.content[].tool_use_id gone");
        substantive!(
            *chars > 0,
            "message.content[].content produced 0 chars for `{id}` — the string form or the \
             `{{type:\"text\", text}}` block form stopped being read"
        );
    }

    // `is_error` must round-trip BOTH ways: a canary that only checks the
    // `true` case passes just as happily when the reader has been rewired to
    // return a constant.
    let Some(parts) = crate::harness::claude::read::message_parts(line) else {
        return Err(
            "claude.transcript.tool_result: message.content[] is no longer an array".to_string(),
        );
    };
    let flags: Vec<bool> = parts
        .iter()
        .map(crate::harness::claude::read::tool_result_is_error)
        .collect();
    substantive!(
        flags == vec![false, true],
        "claude.transcript.tool_result: `message.content[].is_error` no longer round-trips — a \
         failed tool result reading as success is what lets an ABORTED commit be mined for hashes"
    );
    Ok(())
}

// ── claude.transcript.assistant_text ────────────────────────────────────────

/// `harness/claude/read.rs::assistant_texts` still lifts speakable prose — and
/// only speakable prose — out of an assistant transcript line.
///
/// **This canary now proves a FALLBACK** (V35 Phase L). Assistant prose reaches
/// TTS from the `Stop` hook on a tab that declares `assistant_text`, and from
/// this reader on every tab that does not — a pre-upgrade tab, a tab on an
/// install with no loopback, or a harness whose plugin cannot push (OpenCode,
/// declared). "Fallback" is exactly the state in which a reader rots unnoticed,
/// so the leading check matters MORE after the migration than before it, not
/// less. The same is true of `claude.transcript.tool_result` above.
///
/// The failure this catches: `message.content[]` reshapes, `assistant_texts`
/// returns an empty vector, `ctx.speak` is never called, and a tab whose push
/// path is also absent simply goes mute — no error, no log, no row.
pub fn claude_transcript_assistant_text() -> Result<(), String> {
    check_claude_transcript_assistant_text(FIXTURE_CLAUDE_ASSISTANT_TEXT)
}

fn check_claude_transcript_assistant_text(raw: &str) -> Result<(), String> {
    let lines = parse_lines(raw)?;
    substantive!(
        lines.len() == 1,
        "fixture guard: expected one assistant line, got {}",
        lines.len()
    );
    let blocks = crate::harness::claude::read::assistant_texts(&lines[0]);
    substantive!(
        blocks.len() == 2,
        "claude.transcript.assistant_text: expected both `text` blocks of the assistant line, got \
         {} — `type == \"assistant\"`, `message.content[]` or `content[].type == \"text\"` has \
         moved, and a tab with no push path goes silently mute",
        blocks.len()
    );
    // Substantiveness: prose out, not empty strings, and the dedup key is a key.
    for (key, text) in &blocks {
        substantive!(
            !text.trim().is_empty(),
            "claude.transcript.assistant_text: a text block came back empty — `content[].text` gone"
        );
        substantive!(
            !key.is_empty() && key.contains(':'),
            "claude.transcript.assistant_text: the dedup key lost its `message.id` prefix, so one \
             message would be re-spoken on every drain tick"
        );
    }
    // …and NOTHING else. A `thinking` or `tool_use` block reaching TTS is the
    // failure the `type == "text"` filter exists to prevent, and it is a
    // user-visible one: cImp would read the model's reasoning out loud.
    let spoken = blocks
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    substantive!(
        !spoken.contains("reasoning") && !spoken.contains("canary.rs"),
        "claude.transcript.assistant_text: a non-text block reached the speech path — thinking or \
         tool_use content is being spoken aloud"
    );
    Ok(())
}

// ── claude.transcript.stop_reason ───────────────────────────────────────────

/// `harness/claude/read.rs::is_turn_end` still tells a turn that CONTINUES from
/// a turn that is OVER, from `message.stop_reason` alone.
///
/// This is V39's turn boundary for the fallback reader: the delegation
/// completion is filed once per turn, carrying that turn's final assistant
/// message, and on a tab whose `Stop` hook does not push, this field is the
/// only thing in the transcript that says which message that is.
///
/// The failure this catches: `stop_reason` is renamed or dropped, `is_turn_end`
/// answers `false` for every line, no completion is ever filed, and a
/// delegation into that tab waits out its entire deadline (ten minutes by
/// default) before reporting `timeout` for a turn that ended in seconds. The
/// worker looks fine, the transcript looks fine, and nothing errors.
pub fn claude_transcript_stop_reason() -> Result<(), String> {
    check_claude_transcript_stop_reason(FIXTURE_CLAUDE_STOP_REASON)
}

fn check_claude_transcript_stop_reason(raw: &str) -> Result<(), String> {
    let lines = parse_lines(raw)?;
    substantive!(
        lines.len() == 2,
        "fixture guard: expected a mid-turn line and a turn-final one, got {}",
        lines.len()
    );

    // Both halves of the distinction, because either one alone is satisfied by
    // a reader that has stopped reading the field at all: a rename makes
    // EVERYTHING answer `false`, which the second assertion catches, and a
    // reader that answered `true` unconditionally would file a completion on
    // the preamble — HIGH-1's defect restored — which the first one catches.
    substantive!(
        !crate::harness::claude::read::is_turn_end(&lines[0]),
        "claude.transcript.stop_reason: a `tool_use` stop reason no longer reads as MID-turn — \
         the delegation completion would be filed on the preamble, and the worker's slot released \
         while it was still working"
    );
    substantive!(
        crate::harness::claude::read::is_turn_end(&lines[1]),
        "claude.transcript.stop_reason: an `end_turn` line no longer reads as the end of a turn — \
         `message.stop_reason` has moved, and every delegation into a Claude tab with no `Stop` \
         push now waits out its whole deadline before reporting `timeout`"
    );

    // …and both lines really are assistant lines with prose in them, so neither
    // answer above can be an artefact of a line the reader skipped wholesale.
    for (i, line) in lines.iter().enumerate() {
        substantive!(
            !crate::harness::claude::read::assistant_texts(line).is_empty(),
            "fixture guard: line {i} carries no assistant text, so the boundary answer above \
             proves nothing about a real turn"
        );
    }
    Ok(())
}

// ── claude.statusline.stdin ─────────────────────────────────────────────────

/// The `statusLine` stdin payload still renders a substantive bar and still
/// yields a substantive push.
///
/// This row has **no** V16 rule lagging it at all: a reshape renders a blank
/// context bar and writes no usage push, and nothing anywhere reports it.
pub fn claude_statusline_stdin() -> Result<(), String> {
    check_claude_statusline_stdin(FIXTURE_CLAUDE_STATUSLINE)
}

fn check_claude_statusline_stdin(payload: &str) -> Result<(), String> {
    let v: Value = serde_json::from_str(payload)
        .map_err(|e| format!("fixture is not valid JSON ({e})"))?;

    // `model.display_name` has exactly one reader — the rendered bar.
    let display = v
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    substantive!(
        !display.is_empty(),
        "fixture guard: the fixture must carry a non-empty model.display_name, else this canary \
         cannot tell absent from empty"
    );
    let bar = crate::statusline::render(payload);
    substantive!(
        bar.contains(display),
        "claude.statusline.stdin: model.display_name no longer reaches the rendered bar (it fell \
         back to the model id or to the literal \"Claude\")"
    );

    // `rate_limits` — the account quota half of the widget.
    let (five_hour, seven_day) = crate::harness::claude::statusline::extract_rate_limits(&v);
    for (name, window) in [("five_hour", five_hour), ("seven_day", seven_day)] {
        let Some(w) = window else {
            return Err(format!("claude.statusline.stdin: rate_limits.{name} gone"));
        };
        substantive!(
            w.utilization > 0.0,
            "rate_limits.{name}.used_percentage gone (read as 0)"
        );
        substantive!(
            w.resets_at.is_some(),
            "rate_limits.{name}.resets_at gone (neither epoch seconds nor an ISO string)"
        );
    }

    // `context_window` — the context-bar half.
    let Some(ctx) = crate::harness::claude::statusline::extract_context(&v) else {
        return Err(
            "claude.statusline.stdin: the whole context_window block stopped being read".to_string(),
        );
    };
    substantive!(
        ctx.used_percentage.is_some_and(|p| p > 0.0),
        "context_window.used_percentage gone"
    );
    substantive!(
        ctx.total_input_tokens.is_some_and(|t| t > 0),
        "context_window.total_input_tokens gone"
    );
    substantive!(
        ctx.context_window_size.is_some_and(|s| s > 0),
        "context_window.context_window_size gone"
    );
    // The rest of what `extract_context` reads. Phase B found these asserted
    // here but undeclared on the registry row; Phase C declared them (the
    // canary asserts what the code reads, and the row now says the same). Each
    // renders as its own number in the context bar, and each one alone is
    // enough to make the snapshot substantive — so losing one is invisible.
    substantive!(
        ctx.remaining_percentage.is_some_and(|p| p > 0.0),
        "context_window.remaining_percentage gone"
    );
    substantive!(
        ctx.cache_read_tokens.is_some_and(|t| t > 0),
        "context_window.current_usage.cache_read_input_tokens gone"
    );
    substantive!(
        ctx.cache_creation_tokens.is_some_and(|t| t > 0),
        "context_window.current_usage.cache_creation_input_tokens gone"
    );
    substantive!(
        ctx.input_tokens.is_some_and(|t| t > 0),
        "context_window.current_usage.input_tokens gone"
    );
    substantive!(
        ctx.output_tokens.is_some_and(|t| t > 0),
        "context_window.current_usage.output_tokens gone"
    );

    // And the composed push the widget actually consumes: `extract_push`
    // returns `None` for a non-substantive snapshot, so a reshape that costs
    // every field writes nothing rather than writing zeros.
    let Some(push) = crate::harness::claude::statusline::extract_push(payload) else {
        return Err(
            "claude.statusline.stdin: the payload no longer produces a push at all".to_string(),
        );
    };
    substantive!(push.has_rate_limits(), "push lost both quota windows");
    substantive!(
        push.context.is_some(),
        "push lost the context reading (NC-3)"
    );
    substantive!(
        push.is_substantive(),
        "the composed push is no longer substantive"
    );
    Ok(())
}

// ── opencode.sse.events ─────────────────────────────────────────────────────

/// `harness/opencode/read.rs::Tracker::handle` still turns one turn's SSE envelopes into
/// spoken assistant text and still binds the tab to the session.
///
/// Driven as an ordered stream rather than as isolated events, because
/// `Tracker` is a state machine: `message.updated` declares the message
/// assistant, `message.part.updated` types the part, `message.part.delta`
/// accumulates the text, and only the completed `message.updated` flushes.
/// Anything less than the whole sequence cannot show that text still comes out
/// the other end.
pub async fn opencode_sse_events() -> Result<(), String> {
    check_opencode_sse_events(FIXTURE_OPENCODE_SSE).await
}

async fn check_opencode_sse_events(raw: &str) -> Result<(), String> {
    let events = parse_lines(raw)?;
    substantive!(
        events.len() >= 4,
        "fixture guard: the turn needs message.updated + part.updated + part.delta + a completed \
         message.updated to prove anything"
    );

    // What the fixture says should come out: the concatenated deltas, and the
    // session id every session-scoped event carries.
    let expected_text: String = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("message.part.delta"))
        .filter_map(|e| {
            e.get("properties")
                .and_then(|p| p.get("delta"))
                .and_then(Value::as_str)
        })
        .collect();
    substantive!(
        !expected_text.trim().is_empty(),
        "fixture guard: the fixture must carry non-empty delta text"
    );
    let Some(expected_session) = events[0]
        .get("properties")
        .and_then(|p| p.get("sessionID"))
        .and_then(Value::as_str)
    else {
        return Err(
            "fixture guard: every session-scoped event carries properties.sessionID".to_string(),
        );
    };

    let (ctx, mut tts_rx, _signals) = opencode_ctx();
    let mut tracker = crate::harness::opencode::read::Tracker::default();
    for ev in &events {
        tracker.handle(ev, &ctx).await;
    }

    match tts_rx.try_recv() {
        Ok(crate::tts::TtsRequest::Synthesize { text, .. }) => {
            // Non-empty is the substantiveness check; equality additionally
            // proves the delta path (not just the `message.part.updated`
            // snapshot) is still wired. A missing `properties.info.id` or
            // `properties.part.messageID` shows up here too: the flush is keyed
            // by message id, so losing it produces silence, not an error.
            substantive!(!text.trim().is_empty(), "spoken text is empty");
            substantive!(
                text == expected_text,
                "opencode.sse.events: the assistant text no longer survives the stream — check \
                 properties.part.messageID / properties.partID / properties.delta"
            );
        }
        other => {
            return Err(format!(
                "opencode.sse.events: a completed assistant message produced no speech ({other:?}) \
                 — something in the chain moved: `message.updated` / `properties.info.role` / \
                 `properties.info.time.completed` (no flush), or `properties.part.messageID` / \
                 `properties.messageID` / `properties.partID` (nothing registered under the \
                 message)"
            ))
        }
    }

    substantive!(
        tracker.current_session().as_deref() == Some(expected_session),
        "opencode.sse.events: properties.sessionID no longer binds the tab to its session (V28 \
         per-tab identity, and the V30 push target)"
    );
    Ok(())
}

/// A tap context wired to the built-in OpenCode tab, so the per-tab TTS gate is
/// satisfied and `ctx.speak` actually delivers. Mirrors `oob::opencode`'s own
/// `ctx_with`; kept local rather than hoisted so this module can be read (and
/// moved, in Phase K) on its own.
fn opencode_ctx() -> (
    crate::harness::OobContext,
    tokio::sync::mpsc::Receiver<crate::tts::TtsRequest>,
    tokio::sync::mpsc::Receiver<crate::state::StateSignal>,
) {
    let (tts_tx, tts_rx) = tokio::sync::mpsc::channel(64);
    let (sig_tx, sig_rx) = tokio::sync::mpsc::channel(64);
    // `Settings::default()` ships no tabs (the real app seeds them from
    // persistence), and an unknown tab speaks nothing.
    let mut defaults = crate::settings::Settings::default();
    defaults.tabs.push(crate::settings::default_opencode_tab());
    let settings = crate::settings::SettingsHandle::new(
        defaults.clone(),
        defaults,
        std::env::temp_dir(),
    );
    let ctx = crate::harness::OobContext {
        tab: crate::state::TabId::from_str(crate::settings::OPENCODE_TAB_ID),
        tts: tts_tx,
        state_signals: sig_tx,
        settings,
        cancel: tokio_util::sync::CancellationToken::new(),
        mem: None,
        pushes: None,
    };
    (ctx, tts_rx, sig_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use crate::harness::contract::{self, Capability};

    // ── fixture plumbing (test-only: the tree exists here) ──────────────────

    /// Root of the committed fixture corpus.
    fn fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("harness")
    }

    /// Read one fixture by its `<harness>/<version>/<name>` relative path.
    /// Panics with the resolved path when it is missing, because a canary that
    /// silently skips is worse than no canary at all.
    fn fixture(relpath: &str) -> String {
        let path = fixtures_root().join(relpath);
        std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "harness fixture `{relpath}` could not be read at {}: {e}",
                path.display()
            )
        })
    }

    /// Parse one fixture line as JSON. A malformed fixture is a defect in the
    /// fixture, not a drift signal, so it panics distinctly.
    fn json(raw: &str) -> Value {
        serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("fixture is not valid JSON ({e}): {}", raw.trim()))
    }

    /// The non-empty lines of a `.jsonl` fixture, parsed.
    fn json_lines(raw: &str) -> Vec<Value> {
        parse_lines(raw).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The registry row a canary proves, with the join key checked in both
    /// directions: the id must exist in [`contract::CAPABILITIES`], and that
    /// row's `canary` must name this same id. A canary drifting away from its
    /// row is the one failure mode that would make the whole suite decorative.
    fn row(id: &'static str) -> &'static Capability {
        let cap = contract::get(id).unwrap_or_else(|| {
            panic!("canary `{id}` names no capability — canary ids ARE capability ids")
        });
        assert_eq!(
            cap.canary,
            Some(id),
            "capability `{id}` does not claim its canary: set `canary: Some(\"{id}\")` on the \
             registry row (and drop the waiver it replaces)"
        );
        cap
    }

    // ── the five positive canaries ──────────────────────────────────────────
    //
    // Each is a WRAPPER over the runtime function the auto-verify calls
    // (V35 Phase F). Re-implementing the assertions here would let `cargo test`
    // stay green while the check that advances `claude_last_verified` drifted
    // away from it — the one thing this arrangement exists to prevent.

    #[test]
    fn canary_claude_transcript_usage() {
        row("claude.transcript.usage");
        claude_transcript_usage().unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn canary_claude_transcript_tool_result() {
        row("claude.transcript.tool_result");
        claude_transcript_tool_result().unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn canary_claude_statusline_stdin() {
        row("claude.statusline.stdin");
        claude_statusline_stdin().unwrap_or_else(|e| panic!("{e}"));
    }

    /// V35 Phase L. Proves the FALLBACK, which is the state a reader is most
    /// likely to rot in: on a tab whose `Stop` hook pushes, nothing here runs,
    /// so the day the push breaks is the day this reader has to work.
    #[test]
    fn canary_claude_transcript_assistant_text() {
        row("claude.transcript.assistant_text");
        claude_transcript_assistant_text().unwrap_or_else(|e| panic!("{e}"));
        // …and the runtime dispatcher reaches it, so auto-verify runs the same
        // check `cargo test` does.
        assert!(matches!(
            run_embedded("claude.transcript.assistant_text"),
            Some(Ok(()))
        ));
    }

    /// V39. The delegation turn boundary — the one contract whose loss is
    /// invisible in the transcript itself: every line still parses, every
    /// message is still spoken, and only the driver waiting on the other side
    /// of a delegation ever finds out.
    #[test]
    fn canary_claude_transcript_stop_reason() {
        row("claude.transcript.stop_reason");
        claude_transcript_stop_reason().unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            run_embedded("claude.transcript.stop_reason"),
            Some(Ok(()))
        ));
    }

    #[tokio::test]
    async fn canary_opencode_sse_events() {
        row("opencode.sse.events");
        // Awaited directly rather than through `run_embedded`, which parks its
        // own runtime and must not be called from an async context.
        opencode_sse_events().await.unwrap_or_else(|e| panic!("{e}"));
    }

    // ── the negative twins ──────────────────────────────────────────────────

    /// Negative twin: `message.stop_reason` renamed to `stopReason`.
    ///
    /// `is_turn_end` answers `false` for BOTH lines — the mid-turn one (right,
    /// by accident) and the turn-final one (wrong, silently). That is the
    /// production failure verbatim: no completion is ever filed, the driver
    /// waits out its whole deadline, and the Events row says `timeout` for a
    /// turn that ended in seconds. The untouched half is asserted too, because
    /// a fixture that broke the LINE rather than the field would prove nothing:
    /// both lines still yield their assistant text, so the tab still speaks
    /// while delegation quietly stops completing.
    #[test]
    fn negative_canary_claude_transcript_stop_reason() {
        row("claude.transcript.stop_reason");

        let raw = fixture("claude/_synthetic/stop-reason-renamed.jsonl");
        let lines = json_lines(&raw);
        assert_eq!(lines.len(), 2, "fixture guard: expected both lines");

        for (i, line) in lines.iter().enumerate() {
            assert!(
                !crate::harness::claude::read::is_turn_end(line),
                "guard: this fixture models the drift case — line {i} must read as MID-turn once \
                 `stop_reason` is renamed, which is exactly why the loss is silent"
            );
            assert!(
                !crate::harness::claude::read::assistant_texts(line).is_empty(),
                "guard: only ONE field may differ from the positive twin — the text blocks must \
                 still read, or this fixture proves nothing about the renamed field"
            );
        }

        // And the positive fixture's own answer is not an accident of the
        // checker: the same function, on the untouched twin, still distinguishes.
        assert!(check_claude_transcript_stop_reason(FIXTURE_CLAUDE_STOP_REASON).is_ok());
        assert!(
            check_claude_transcript_stop_reason(&raw).is_err(),
            "the canary must FAIL on the drift model — otherwise it is decorative"
        );
    }

    /// Negative twin: `message.usage.input_tokens` renamed to `inputTokens`.
    ///
    /// `parse_usage_line` still returns a `Turn` — same message id, same model,
    /// three of four counters intact — and the fourth silently reads 0. That is
    /// the production failure verbatim: a row is UPSERTed claiming the turn
    /// spent no input tokens, and nothing anywhere errors. If this ever stops
    /// being true, the positive canary above is no longer proving what it
    /// claims.
    #[test]
    fn negative_canary_claude_transcript_usage() {
        row("claude.transcript.usage");

        let raw = fixture("claude/_synthetic/usage-renamed-input-tokens.jsonl");
        let lines = json_lines(&raw);
        assert_eq!(lines.len(), 1, "fixture guard: expected one assistant line");

        let ev = crate::harness::claude::read::parse_usage_line(&lines[0], crate::graph::UsageOrigin::Session)
            .expect("guard: this fixture models the drift case — a renamed token field must NOT stop the line parsing, that is precisely why it is silent");

        let crate::graph::UsageEvent::Turn {
            in_tok,
            out_tok,
            cache_read,
            cache_make,
            ..
        } = ev
        else {
            panic!("guard: this fixture models the drift case — it must still be a Turn");
        };

        assert_eq!(
            in_tok, 0,
            "guard: this fixture models the drift case — `inputTokens` must read as 0 via \
             `unwrap_or(0)`. A non-zero here means the reader grew an alias and the positive canary \
             can no longer detect this rename."
        );
        // The rest of the line is untouched, so the loss is ONE number in an
        // otherwise healthy-looking row — and a fixture that broke wholesale would
        // pass the assertion above while proving nothing.
        assert!(out_tok > 0 && cache_read > 0 && cache_make > 0);

        // …and the canary the AUTO-VERIFY runs sees it (V35 Phase F). Asserted
        // about the production function, not a copy of its assertions.
        assert!(
            check_claude_transcript_usage(&raw).is_err(),
            "the runtime canary must FAIL on the drift model — otherwise auto-verify would \
             advance `claude_last_verified` straight past this rename"
        );
    }

    /// Negative twin: `message.content[].text` renamed to `body` on both text
    /// blocks (V35 Phase L).
    ///
    /// `assistant_texts` `filter_map`s past a block with no `text`, so the whole
    /// line yields an EMPTY vector — and an empty vector is indistinguishable
    /// from an assistant turn that only called tools. Nothing errors, nothing
    /// logs, and a tab whose `Stop` hook is also absent (pre-upgrade, or an
    /// install with no loopback) simply stops speaking. This row has no V16 rule
    /// lagging it either, which is why the empty case is the whole point.
    #[test]
    fn negative_canary_claude_transcript_assistant_text() {
        row("claude.transcript.assistant_text");

        let raw = fixture("claude/_synthetic/assistant-text-renamed-text.jsonl");
        let lines = json_lines(&raw);
        assert_eq!(lines.len(), 1, "fixture guard: expected one assistant line");
        let line = &lines[0];

        let blocks = crate::harness::claude::read::assistant_texts(line);
        assert!(
            blocks.is_empty(),
            "guard: this fixture models the drift case — a renamed `text` must yield NO speakable \
             blocks, silently. Got {blocks:?}, which means the reader grew an alias and the \
             positive canary can no longer detect this rename."
        );

        // The line is otherwise intact: all four content blocks are still there
        // and still typed, so the test cannot pass on a merely malformed fixture.
        let parts = crate::harness::claude::read::message_parts(line)
            .expect("guard: the drift fixture must still be a well-formed assistant line");
        assert_eq!(parts.len(), 4, "guard: every content block is still present");
        assert_eq!(
            parts
                .iter()
                .filter(|p| p.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                .count(),
            2,
            "guard: only the field name was renamed, not the block types"
        );

        assert!(
            check_claude_transcript_assistant_text(&raw).is_err(),
            "the runtime canary must FAIL on the drift model — otherwise auto-verify would \
             advance `claude_last_verified` straight past a rename that mutes every fallback tab"
        );
    }

    /// Negative twin: `message.content[].tool_use_id` renamed to `toolUseId` on
    /// both blocks.
    ///
    /// `extract_tool_results` `continue`s past a block with no `tool_use_id`, so
    /// the whole line yields an EMPTY result set — and an empty result set is
    /// indistinguishable from a user turn that simply ran no tools. This is the
    /// row with no V16 rule lagging it at all, which is why the empty case
    /// matters more here than anywhere else.
    #[test]
    fn negative_canary_claude_transcript_tool_result() {
        row("claude.transcript.tool_result");

        let raw = fixture("claude/_synthetic/tool-result-renamed-tool-use-id.jsonl");
        let lines = json_lines(&raw);
        assert_eq!(lines.len(), 1, "fixture guard: expected one user line");
        let line = &lines[0];

        let results = crate::harness::claude::read::extract_tool_results(line);
        assert!(
            results.is_empty(),
            "guard: this fixture models the drift case — a renamed `tool_use_id` must yield NO \
             results, silently. Got {results:?}, which means the reader grew an alias and the \
             positive canary can no longer detect this rename."
        );

        // The line itself is otherwise intact: both blocks are still there, still
        // `tool_result`, and `is_error` still round-trips. Without this the test
        // would also pass on a fixture that was merely malformed.
        let parts = crate::harness::claude::read::message_parts(line)
            .expect("guard: the drift fixture must still be a well-formed user line");
        assert_eq!(parts.len(), 2, "guard: both tool_result blocks are still present");
        let flags: Vec<bool> = parts
            .iter()
            .map(crate::harness::claude::read::tool_result_is_error)
            .collect();
        assert_eq!(flags, vec![false, true], "guard: only `tool_use_id` was renamed");

        assert!(
            check_claude_transcript_tool_result(&raw).is_err(),
            "the runtime canary must FAIL on the drift model (V35 Phase F)"
        );
    }

    /// Negative twin: the whole `context_window` block renamed to `contextWindow`
    /// — the exact reshape live-verify recipe 3 exercises by hand.
    ///
    /// Three degraded defaults at once, and the third is the nasty one:
    ///   * `extract_context` returns `None` (no numbers, and this payload carries
    ///     none of the session metadata that would keep it `Some`);
    ///   * `render` draws a bar with no token pair at all — `size == 0` suppresses
    ///     the "(used/size)" suffix, so the bar looks *deliberate* rather than
    ///     broken, and the model name still renders beside it;
    ///   * `extract_push` still returns `Some` and still reports `has_rate_limits`,
    ///     because `rate_limits` is a sibling of the renamed block. The push keeps
    ///     flowing and keeps looking healthy while the context slot goes dark.
    #[test]
    fn negative_canary_claude_statusline_stdin() {
        row("claude.statusline.stdin");

        let payload = fixture("claude/_synthetic/statusline-renamed-context-window.json");
        let v = json(&payload);

        assert!(
            crate::harness::claude::statusline::extract_context(&v).is_none(),
            "guard: this fixture models the drift case — with `context_window` renamed and no session \
             metadata beside it, the whole context reading must vanish. A `Some` here means the reader \
             grew an alias and the positive canary can no longer detect this reshape."
        );

        let bar = crate::statusline::render(&payload);
        assert!(
            !bar.contains("25k") && !bar.contains("200k"),
            "guard: this fixture models the drift case — the token pair must be gone from the bar, \
             got {bar:?}"
        );
        // The half that survives, which is what makes the loss hard to notice.
        assert!(
            bar.contains("Canary Sonnet 4.5"),
            "guard: only `context_window` was renamed — the model name must still render"
        );

        let push = crate::harness::claude::statusline::extract_push(&payload)
            .expect("guard: the push must still be written — `rate_limits` is untouched");
        assert!(
            push.context.is_none(),
            "guard: this fixture models the drift case — the push must lose its context reading"
        );
        assert!(
            push.has_rate_limits(),
            "guard: only `context_window` was renamed — the quota half must still push"
        );

        assert!(
            check_claude_statusline_stdin(&payload).is_err(),
            "the runtime canary must FAIL on the drift model (V35 Phase F)"
        );
    }

    /// Negative twin: `properties.partID` renamed to `partId` on the
    /// `message.part.delta` event.
    ///
    /// `Tracker::handle` destructures `partID`/`messageID`/`delta` as one tuple, so
    /// the delta is dropped whole; the part's only other text source is the empty
    /// `message.part.updated` snapshot, and `flush` speaks nothing when the joined
    /// text is blank. Result: the turn completes, the tab stays bound to its
    /// session, `session.idle` arrives — and the assistant's answer is never
    /// spoken. No error, no log, no unknown-event branch: the reader `match`es on
    /// the event `type`, which did not change.
    #[tokio::test]
    async fn negative_canary_opencode_sse_events() {
        row("opencode.sse.events");

        let raw = fixture("opencode/_synthetic/sse-renamed-part-id.jsonl");
        let events = json_lines(&raw);
        assert!(events.len() >= 4, "fixture guard: the whole turn must be present");

        let (ctx, mut tts_rx, _signals) = opencode_ctx();
        let mut tracker = crate::harness::opencode::read::Tracker::default();
        for ev in &events {
            tracker.handle(ev, &ctx).await;
        }

        assert!(
            tts_rx.try_recv().is_err(),
            "guard: this fixture models the drift case — a renamed `partID` must produce SILENCE. \
             Speech here means the reader grew an alias (or started falling back to the part \
             snapshot) and the positive canary can no longer detect this rename."
        );
        // Everything else about the stream still worked, which is exactly why this
        // degradation is invisible in production: the tab looks live and bound.
        assert_eq!(
            tracker.current_session().as_deref(),
            Some("ses_canary_main_0001"),
            "guard: only `partID` was renamed — the session binding must survive"
        );

        assert!(
            check_opencode_sse_events(&raw).await.is_err(),
            "the runtime canary must FAIL on the drift model (V35 Phase F)"
        );
    }

    // ── the runtime half (V35 Phase F) ──────────────────────────────────────

    /// [`EMBEDDED`] and the registry's `canary` column are the same set.
    ///
    /// This is the join that makes auto-verify's coverage checkable: the
    /// registry says which rows claim a canary, and this list says which ones
    /// the *shipped binary* can actually run. A declared canary missing here
    /// would be a row auto-verify silently never checks (and would still count
    /// as coverage in `every_silent_degradation_has_a_canary_or_a_probe_or_a_waiver`);
    /// an entry here with no row would be a check nobody declared.
    #[test]
    fn embedded_canaries_are_exactly_the_declared_ones() {
        let embedded: BTreeSet<&str> = EMBEDDED.iter().copied().collect();
        assert_eq!(
            embedded.len(),
            EMBEDDED.len(),
            "EMBEDDED has a duplicate id: {EMBEDDED:?}"
        );
        let declared: BTreeSet<&str> = contract::CAPABILITIES
            .iter()
            .filter_map(|c| c.canary)
            .collect();
        assert_eq!(
            embedded, declared,
            "the runtime canary list and the registry's `canary` column have diverged — \
             auto-verify would run a different set than the matrix claims is covered"
        );
    }

    /// The production entry point answers for every embedded id, and every
    /// answer is `Ok` today.
    ///
    /// Deliberately a plain `#[test]`: [`run_embedded`] parks its own runtime
    /// for the async reader, which is exactly how the auto-verify worker (a
    /// plain OS thread) calls it. Running it from a `#[tokio::test]` would
    /// panic — and that panic is the reason this test exists in this form.
    #[test]
    fn run_embedded_answers_for_every_embedded_id() {
        for id in EMBEDDED {
            match run_embedded(id) {
                Some(Ok(())) => {}
                Some(Err(e)) => panic!("embedded canary `{id}` failed: {e}"),
                None => panic!(
                    "`{id}` is declared in EMBEDDED but `run_embedded` has no arm for it — \
                     auto-verify would report it as uncovered rather than as checked"
                ),
            }
        }
        assert!(
            run_embedded("claude.hook.precompact").is_none(),
            "an id with no embedded canary must answer None, NEVER Err — `Err` blocks the \
             auto-advance, and 'nothing checks this' is not a failure"
        );
    }

    // ── the suite ↔ the matrix ──────────────────────────────────────────────

    /// This module's own source, read at compile time. The cross-check below
    /// needs the *call sites*, and a hand-kept list of them is precisely the
    /// drift the check exists to prevent — so the list is derived from the text
    /// instead. Test-only, so nothing extra lands in a release binary; the
    /// `_synthetic` fixtures stay file-loaded for that reason, this does not.
    const THIS_SOURCE: &str = include_str!("canary.rs");

    /// Every capability id passed to [`row`] anywhere in this file.
    ///
    /// The needle is assembled from pieces on purpose: written as one literal it
    /// would appear in this function's own source and the scan would match itself,
    /// which is how an extractor ends up "finding" ids nobody wrote.
    fn canaried_ids(src: &'static str) -> BTreeSet<&'static str> {
        let needle = concat!("row", "(\"");
        let mut out = BTreeSet::new();
        let mut rest = src;
        while let Some(at) = rest.find(needle) {
            let after = &rest[at + needle.len()..];
            let Some(end) = after.find('"') else { break };
            let (id, tail) = after.split_at(end);
            // A call site, not prose: ids are `[a-z0-9_.]` and the string literal
            // closes immediately before the `)`.
            if !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
                && tail.starts_with("\")")
            {
                out.insert(id);
            }
            rest = tail;
        }
        out
    }

    /// The suite and the registry name the same capabilities, in both directions
    /// (design doc § 6).
    ///
    /// A canary id **is** a capability id, so this is a set comparison rather than
    /// a mapping: the ids the registry declares canaried must be exactly the ids
    /// this file drives through [`row`]. A declared canary with no test is a row
    /// that traded a waiver for nothing; a test whose id no row declares is the
    /// suite drifting into checking things nobody wrote down. Positive and negative
    /// twins both call [`row`], and the comparison is over ids, so the duplication
    /// is free.
    ///
    /// Deliberately **not** checked here: that every declared canary also has a
    /// negative twin. The five Tier-C readers have one, but Phase D's live-probe
    /// canaries cover `Behavior` deps where a "renamed field" fixture is
    /// meaningless — recorded rather than assumed, so the omission is a decision
    /// and not an oversight.
    #[test]
    fn canaries_and_the_matrix_agree() {
        let tested = canaried_ids(THIS_SOURCE);
        // A silently-empty extraction would make everything below vacuously true.
        assert!(
            tested.len() >= 4,
            "the canary-call-site scan found only {tested:?} — it has stopped matching this file's \
             own call sites, and every assertion below is now vacuous"
        );

        let mut declared: BTreeSet<&str> = BTreeSet::new();
        for c in contract::CAPABILITIES {
            if let Some(canary) = c.canary {
                // The join key, asserted for EVERY row rather than only for the
                // ones with a test: `row` cannot catch a row whose canary names
                // some other capability, because nothing would call it.
                assert_eq!(
                    canary, c.id,
                    "capability `{}` declares canary `{canary}` — a canary id IS the capability id, \
                     never a third namespace",
                    c.id
                );
                declared.insert(canary);
            }
        }

        let untested: Vec<&str> = declared.difference(&tested).copied().collect();
        assert!(
            untested.is_empty(),
            "declared canary has no test: {untested:?} carry `canary: Some(..)` in \
             `harness::contract::CAPABILITIES` but nothing in harness/canary.rs drives them. Write \
             the canary, or put the waiver back."
        );

        let undeclared: Vec<&str> = tested.difference(&declared).copied().collect();
        assert!(
            undeclared.is_empty(),
            "canary exists outside the matrix: {undeclared:?} are driven by harness/canary.rs but no \
             registry row declares them. Add the row (or set `canary: Some(..)` on it) — the suite \
             must not test dependencies the matrix has not recorded."
        );
    }

    // ── the corpus itself ───────────────────────────────────────────────────

    /// The four keys every `MANIFEST.toml` must carry.
    const MANIFEST_KEYS: [&str; 4] = ["captured_from", "date", "method", "redaction"];

    /// The one directory under a harness that is not a CLI version: the Phase C
    /// drift models. It is checked by the SAME walker rather than skipped by it —
    /// an exemption is how a corpus grows an undated corner — and additionally
    /// must declare [`MODELS_VERSION_KEY`].
    const SYNTHETIC_DIR: &str = "_synthetic";

    /// The fifth key a `_synthetic/` manifest carries: the sibling version
    /// directory whose fixtures it mutates. Checked to be a real directory, so a
    /// drift model cannot outlive the fixture it was derived from — the two must
    /// stay byte-identical apart from the renamed field, and that claim is
    /// unverifiable once the twin is gone.
    const MODELS_VERSION_KEY: &str = "models_version";

    /// `manifest[key]`, trimmed of quotes and whitespace. `""` when the key is
    /// absent — present-but-blank and absent are treated alike by the callers,
    /// which is the point (global principle 5).
    fn manifest_value(manifest: &str, key: &str) -> String {
        manifest
            .lines()
            .map(str::trim_start)
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(|rest| rest.trim().trim_matches('"').trim().to_string())
            .unwrap_or_default()
    }

    /// Every `<harness>/<version>/` directory carries a `MANIFEST.toml` with all
    /// four provenance keys, and at least one fixture beside it. `_synthetic/`
    /// (the drift models) is held to the same rule plus `models_version`.
    ///
    /// Locked decision 4: an anonymous fixture is indistinguishable from a guess.
    /// Without this the corpus silently accumulates files nobody can date, and the
    /// first question during a real breakage — "is this shape still what upstream
    /// sends, or did we invent it in 2026?" — has no answer.
    #[test]
    fn every_fixture_version_dir_has_a_manifest() {
        let root = fixtures_root();
        let harnesses = read_dirs(&root);
        assert!(
            !harnesses.is_empty(),
            "no harness fixtures at all under {}",
            root.display()
        );

        let mut checked = 0usize;
        for harness in harnesses {
            let versions = read_dirs(&harness);
            assert!(
                !versions.is_empty(),
                "{}: a harness directory with no version directory — fixtures are versioned by the \
                 CLI build they were modelled on",
                harness.display()
            );
            for version in versions {
                let manifest_path = version.join("MANIFEST.toml");
                let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
                    panic!(
                        "{}: no readable MANIFEST.toml ({e}) — record captured_from / date / method / \
                         redaction, or delete the fixtures",
                        version.display()
                    )
                });
                for key in MANIFEST_KEYS {
                    // Present-but-blank is absent with extra steps.
                    assert!(
                        manifest_value(&manifest, key).len() > 3,
                        "{}: MANIFEST.toml key `{key}` is missing or blank",
                        manifest_path.display()
                    );
                }
                // The drift models are not a CLI version, so they answer one extra
                // question instead: which version's fixtures did you mutate?
                if version.file_name().is_some_and(|n| n == SYNTHETIC_DIR) {
                    let models = manifest_value(&manifest, MODELS_VERSION_KEY);
                    assert!(
                        !models.is_empty()
                            && version
                                .parent()
                                .is_some_and(|p| p.join(&models).is_dir()),
                        "{}: `{MODELS_VERSION_KEY}` must name a sibling version directory that still \
                         exists (got {models:?}) — a drift model whose twin is gone can no longer be \
                         shown to differ from it in exactly one field",
                        manifest_path.display()
                    );
                }
                let fixtures: Vec<PathBuf> = std::fs::read_dir(&version)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n != "MANIFEST.toml"))
                    .collect();
                assert!(
                    !fixtures.is_empty(),
                    "{}: a manifest with no fixtures beside it",
                    version.display()
                );
                checked += 1;
            }
        }
        assert!(checked >= 2, "expected fixtures for both harnesses");
    }

    /// The embedded copies really are the committed files.
    ///
    /// `include_str!` resolves at compile time and the walker checks the file on
    /// disk, so without this the two could describe different corpora after a
    /// path edit that still compiles (a fixture copied to a new version dir,
    /// say). Cheap, and it keeps "the fixtures are provenance-checked" true of
    /// the bytes the shipped canary actually runs.
    #[test]
    fn the_embedded_fixtures_are_the_committed_files() {
        for (embedded, relpath) in [
            (
                FIXTURE_CLAUDE_USAGE,
                "claude/2.1.232/transcript.assistant-usage.jsonl",
            ),
            (
                FIXTURE_CLAUDE_TOOL_RESULT,
                "claude/2.1.232/transcript.tool-result.jsonl",
            ),
            (
                FIXTURE_CLAUDE_STOP_REASON,
                "claude/2.1.232/transcript.stop-reason.jsonl",
            ),
            (FIXTURE_CLAUDE_STATUSLINE, "claude/2.1.232/statusline-stdin.json"),
            (
                FIXTURE_OPENCODE_SSE,
                "opencode/1.18.13/sse.assistant-turn.jsonl",
            ),
        ] {
            assert_eq!(
                embedded,
                fixture(relpath),
                "the embedded copy of `{relpath}` is not the file the manifest walker checks"
            );
        }
    }

    /// Immediate sub-directories of `dir`, sorted, ignoring files.
    fn read_dirs(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: unreadable ({e})", dir.display()))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        out.sort();
        out
    }
}
