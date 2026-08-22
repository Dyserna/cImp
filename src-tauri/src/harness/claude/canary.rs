//! **Claude Code's L1 canaries** — the fixture-backed substantiveness checks
//! `harness/canary.rs`'s neutral runner drives through
//! [`HarnessPlugin::canaries`](crate::harness::plugin::HarnessPlugin::canaries).
//!
//! V40 Phase A, locked decision 17: the runner keeps the corpus rules, the
//! `substantive!` macro, the runtime dispatcher and the registry cross-checks;
//! the assertions themselves — what "still substantive" MEANS for a Claude
//! transcript line or a `statusLine` stdin payload — live with the harness they
//! are true of. Moved verbatim: same text, same fixtures, same negative twins.
//!
//! Read [`crate::harness::canary`]'s module docs first: they carry the *why*
//! (leniency ⇒ a rename produces zeros, not errors), the two-callers-one-code-path
//! rule and the fixture provenance discipline, and all three still apply here.

use serde_json::Value;

use crate::harness::canary::{parse_lines, substantive};
use crate::harness::plugin::Canary;

// ── the embedded corpus (V35 Phase F) ───────────────────────────────────────

/// The positive fixtures, embedded so the canaries run from a release binary. A
/// missing or renamed fixture is a **compile** error, which is the other half of
/// why they are embedded rather than path-loaded: the runtime canary can never
/// degrade to "fixture not found ⇒ skipped".
const FIXTURE_CLAUDE_USAGE: &str =
    include_str!("../../../fixtures/harness/claude/2.1.232/transcript.assistant-usage.jsonl");
const FIXTURE_CLAUDE_TOOL_RESULT: &str =
    include_str!("../../../fixtures/harness/claude/2.1.232/transcript.tool-result.jsonl");
/// V35 Phase L. The fifth, and the one whose ABSENCE was the finding: assistant
/// prose → TTS was a live Tier-C dependency with no registry row and no canary,
/// because Phase B seeded the rows it could point a *named reader function* at
/// and this one's reader (`assistant_texts`) was inlined in the drain loop.
/// Phase L needed the row anyway — a `Fallback { to: .. }` cannot point at a
/// capability that does not exist.
const FIXTURE_CLAUDE_ASSISTANT_TEXT: &str =
    include_str!("../../../fixtures/harness/claude/2.1.232/transcript.assistant-text.jsonl");
/// V39. The turn BOUNDARY the delegation completion feed derives from the
/// transcript — a different contract from the text blocks above, in a different
/// field, breaking differently: `assistant_texts` going empty makes a tab go
/// mute, `stop_reason` going missing makes every delegation into a Claude tab
/// wait out its deadline. Two lines, because the contract is a DISTINCTION.
const FIXTURE_CLAUDE_STOP_REASON: &str =
    include_str!("../../../fixtures/harness/claude/2.1.232/transcript.stop-reason.jsonl");
const FIXTURE_CLAUDE_STATUSLINE: &str =
    include_str!("../../../fixtures/harness/claude/2.1.232/statusline-stdin.json");

/// The `<harness>/<version>/<name>` paths of the fixtures above, under
/// `src-tauri/fixtures/harness/`. Declared beside the embedded bytes so
/// `canary::tests::the_embedded_fixtures_are_the_committed_files` can check one
/// against the other without a hand-kept pair list.
const PATH_USAGE: &str = "claude/2.1.232/transcript.assistant-usage.jsonl";
const PATH_TOOL_RESULT: &str = "claude/2.1.232/transcript.tool-result.jsonl";
const PATH_ASSISTANT_TEXT: &str = "claude/2.1.232/transcript.assistant-text.jsonl";
const PATH_STOP_REASON: &str = "claude/2.1.232/transcript.stop-reason.jsonl";
const PATH_STATUSLINE: &str = "claude/2.1.232/statusline-stdin.json";

/// What [`crate::harness::claude::PLUGIN`] declares to the runner: one row per
/// capability this harness has a leading canary for, in the order auto-verify
/// runs them.
///
/// A canary id **is** a capability id — never a third namespace — which is what
/// lets `canary::tests::embedded_canaries_are_exactly_the_declared_ones`
/// set-compare this against the registry's `canary` column in both directions.
pub const CANARIES: &[Canary] = &[
    Canary {
        id: "claude.transcript.usage",
        fixture: FIXTURE_CLAUDE_USAGE,
        fixture_path: PATH_USAGE,
        run: check_claude_transcript_usage,
    },
    Canary {
        id: "claude.transcript.tool_result",
        fixture: FIXTURE_CLAUDE_TOOL_RESULT,
        fixture_path: PATH_TOOL_RESULT,
        run: check_claude_transcript_tool_result,
    },
    Canary {
        id: "claude.transcript.assistant_text",
        fixture: FIXTURE_CLAUDE_ASSISTANT_TEXT,
        fixture_path: PATH_ASSISTANT_TEXT,
        run: check_claude_transcript_assistant_text,
    },
    Canary {
        id: "claude.transcript.stop_reason",
        fixture: FIXTURE_CLAUDE_STOP_REASON,
        fixture_path: PATH_STOP_REASON,
        run: check_claude_transcript_stop_reason,
    },
    Canary {
        id: "claude.statusline.stdin",
        fixture: FIXTURE_CLAUDE_STATUSLINE,
        fixture_path: PATH_STATUSLINE,
        run: check_claude_statusline_stdin,
    },
];

// ── claude.transcript.usage ─────────────────────────────────────────────────

/// `harness/claude/read.rs::parse_usage_line` still lifts all four token counters, the
/// message id and the model out of an assistant transcript line.
///
/// The failure this catches: `usage.input_tokens` is renamed upstream,
/// `unwrap_or(0)` turns that into a `0`, the row is UPSERTed as a zero-token
/// turn, and the Usage tab quietly reports a session that spent nothing.
/// The assertion, over an arbitrary fixture body — so the negative twin can
/// prove this exact function answers `Err` on the drift model.
pub fn check_claude_transcript_usage(raw: &str) -> Result<(), String> {
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
pub fn check_claude_transcript_tool_result(raw: &str) -> Result<(), String> {
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
pub fn check_claude_transcript_assistant_text(raw: &str) -> Result<(), String> {
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
pub fn check_claude_transcript_stop_reason(raw: &str) -> Result<(), String> {
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
pub fn check_claude_statusline_stdin(payload: &str) -> Result<(), String> {
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::canary::support::{fixture, json, json_lines, row};
    use crate::harness::canary::run_embedded;

    // ── the five positive canaries ──────────────────────────────────────────
    //
    // Each is a WRAPPER over the runtime function the auto-verify calls
    // (V35 Phase F). Re-implementing the assertions here would let `cargo test`
    // stay green while the check that advances `claude_last_verified` drifted
    // away from it — the one thing this arrangement exists to prevent.

    #[test]
    fn canary_claude_transcript_usage() {
        row("claude.transcript.usage");
        check_claude_transcript_usage(FIXTURE_CLAUDE_USAGE).unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn canary_claude_transcript_tool_result() {
        row("claude.transcript.tool_result");
        check_claude_transcript_tool_result(FIXTURE_CLAUDE_TOOL_RESULT).unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn canary_claude_statusline_stdin() {
        row("claude.statusline.stdin");
        check_claude_statusline_stdin(FIXTURE_CLAUDE_STATUSLINE).unwrap_or_else(|e| panic!("{e}"));
    }

    /// V35 Phase L. Proves the FALLBACK, which is the state a reader is most
    /// likely to rot in: on a tab whose `Stop` hook pushes, nothing here runs,
    /// so the day the push breaks is the day this reader has to work.
    #[test]
    fn canary_claude_transcript_assistant_text() {
        row("claude.transcript.assistant_text");
        check_claude_transcript_assistant_text(FIXTURE_CLAUDE_ASSISTANT_TEXT).unwrap_or_else(|e| panic!("{e}"));
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
        check_claude_transcript_stop_reason(FIXTURE_CLAUDE_STOP_REASON).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            run_embedded("claude.transcript.stop_reason"),
            Some(Ok(()))
        ));
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
}
