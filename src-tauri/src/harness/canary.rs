//! V35 Phase B — L1 fixture canaries for the four Tier-C readers.
//!
//! # What these tests assert, and why it is not "does it parse"
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
//! # Fixtures
//!
//! `src-tauri/fixtures/harness/<harness>/<version>/<name>`, loaded at test
//! runtime by [`fixture`] from `CARGO_MANIFEST_DIR` — deliberately **not**
//! `include_str!`, which would change what a release build embeds (milestone
//! deploy trap). They are synthetic-minimal and hand-authored from the reader
//! code's contract, never copied from a real transcript: real transcripts carry
//! user prompts, file contents, tool output and plausibly credentials
//! (locked decision 4). Each version directory carries a `MANIFEST.toml`
//! recording where the shape came from, and
//! [`every_fixture_version_dir_has_a_manifest`] fails the suite for a directory
//! without one — an anonymous fixture is indistinguishable from a guess.
//!
//! # One module, one naming rule
//!
//! Every canary lives here and is named `canary_<capability id with dots as
//! underscores>`, and [`row`] re-asserts on every run that the registry row it
//! claims points back at it. **A canary id IS a capability id** — never a third
//! namespace. That is what lets Phase C's matrix↔canary cross-check be
//! mechanical instead of a hand-maintained list.
//!
//! Negative canaries (a fixture with the field renamed, proving the assertion
//! actually runs) are Phase C and deliberately absent here.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::harness::contract::{self, Capability};

// ── fixture plumbing ────────────────────────────────────────────────────────

/// Root of the committed fixture corpus.
fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("harness")
}

/// Read one fixture by its `<harness>/<version>/<name>` relative path. Panics
/// with the resolved path when it is missing, because a canary that silently
/// skips is worse than no canary at all.
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
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(json)
        .collect()
}

/// The registry row a canary proves, with the join key checked in both
/// directions: the id must exist in [`contract::CAPABILITIES`], and that row's
/// `canary` must name this same id. A canary drifting away from its row is the
/// one failure mode that would make the whole suite decorative.
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

// ── claude.transcript.usage ─────────────────────────────────────────────────

/// `oob/claude.rs::parse_usage_line` still lifts all four token counters, the
/// message id and the model out of an assistant transcript line.
///
/// The failure this catches: `usage.input_tokens` is renamed upstream,
/// `unwrap_or(0)` turns that into a `0`, the row is UPSERTed as a zero-token
/// turn, and the Usage tab quietly reports a session that spent nothing.
#[test]
fn canary_claude_transcript_usage() {
    row("claude.transcript.usage");

    let raw = fixture("claude/2.1.232/transcript.assistant-usage.jsonl");
    let lines = json_lines(&raw);
    assert_eq!(lines.len(), 1, "fixture guard: expected one assistant line");

    let ev = crate::oob::claude::parse_usage_line(&lines[0], crate::graph::UsageOrigin::Session)
        .expect("claude.transcript.usage: no UsageEvent from an assistant line (`type`, `message` or `message.id` gone)");

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
        panic!("claude.transcript.usage: an assistant line produced a non-Turn event");
    };

    // Substantiveness — the whole point. Every one of these is a field a
    // rename would zero out silently in production.
    assert!(!msg_id.is_empty(), "message.id gone");
    assert!(model.is_some_and(|m| !m.is_empty()), "message.model gone");
    assert!(in_tok > 0, "message.usage.input_tokens gone");
    assert!(out_tok > 0, "message.usage.output_tokens gone");
    assert!(cache_read > 0, "message.usage.cache_read_input_tokens gone");
    assert!(
        cache_make > 0,
        "message.usage.cache_creation_input_tokens gone"
    );
    assert_eq!(origin, crate::graph::UsageOrigin::Session);
}

// ── claude.transcript.tool_result ───────────────────────────────────────────

/// `oob/claude.rs::extract_tool_results` still finds both `tool_result` content
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
#[test]
fn canary_claude_transcript_tool_result() {
    row("claude.transcript.tool_result");

    let raw = fixture("claude/2.1.232/transcript.tool-result.jsonl");
    let lines = json_lines(&raw);
    assert_eq!(lines.len(), 1, "fixture guard: expected one user line");
    let line = &lines[0];

    let results = crate::oob::claude::extract_tool_results(line);
    assert_eq!(
        results.len(),
        2,
        "claude.transcript.tool_result: expected both `tool_result` blocks (string content AND \
         text-block array); got {results:?} — `type`, `message.content[].type` or `tool_use_id` \
         moved"
    );
    for (id, chars) in &results {
        assert!(!id.is_empty(), "message.content[].tool_use_id gone");
        assert!(
            *chars > 0,
            "message.content[].content produced 0 chars for `{id}` — the string form or the \
             `{{type:\"text\", text}}` block form stopped being read"
        );
    }

    // `is_error` must round-trip BOTH ways: a canary that only checks the
    // `true` case passes just as happily when the reader has been rewired to
    // return a constant.
    let parts = crate::oob::claude::message_parts(line)
        .expect("claude.transcript.tool_result: message.content[] is no longer an array");
    let flags: Vec<bool> = parts
        .iter()
        .map(crate::oob::claude::tool_result_is_error)
        .collect();
    assert_eq!(
        flags,
        vec![false, true],
        "claude.transcript.tool_result: `message.content[].is_error` no longer round-trips — a \
         failed tool result reading as success is what lets an ABORTED commit be mined for hashes"
    );
}

// ── claude.statusline.stdin ─────────────────────────────────────────────────

/// The `statusLine` stdin payload still renders a substantive bar and still
/// yields a substantive push.
///
/// This row has **no** V16 rule lagging it at all: a reshape renders a blank
/// context bar and writes no usage push, and nothing anywhere reports it.
#[test]
fn canary_claude_statusline_stdin() {
    row("claude.statusline.stdin");

    let payload = fixture("claude/2.1.232/statusline-stdin.json");
    let v = json(&payload);

    // `model.display_name` has exactly one reader — the rendered bar.
    let display = v
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        !display.is_empty(),
        "fixture guard: the fixture must carry a non-empty model.display_name, else this canary \
         cannot tell absent from empty"
    );
    let bar = crate::statusline::render(&payload);
    assert!(
        bar.contains(display),
        "claude.statusline.stdin: model.display_name no longer reaches the rendered bar (it fell \
         back to the model id or to the literal \"Claude\")"
    );

    // `rate_limits` — the account quota half of the widget.
    let (five_hour, seven_day) = crate::statusline::extract_rate_limits(&v);
    for (name, window) in [("five_hour", five_hour), ("seven_day", seven_day)] {
        let w = window
            .unwrap_or_else(|| panic!("claude.statusline.stdin: rate_limits.{name} gone"));
        assert!(
            w.utilization > 0.0,
            "rate_limits.{name}.used_percentage gone (read as 0)"
        );
        assert!(
            w.resets_at.is_some(),
            "rate_limits.{name}.resets_at gone (neither epoch seconds nor an ISO string)"
        );
    }

    // `context_window` — the context-bar half.
    let ctx = crate::statusline::extract_context(&v)
        .expect("claude.statusline.stdin: the whole context_window block stopped being read");
    assert!(
        ctx.used_percentage.is_some_and(|p| p > 0.0),
        "context_window.used_percentage gone"
    );
    assert!(
        ctx.total_input_tokens.is_some_and(|t| t > 0),
        "context_window.total_input_tokens gone"
    );
    assert!(
        ctx.context_window_size.is_some_and(|s| s > 0),
        "context_window.context_window_size gone"
    );
    // Read by `extract_context` but NOT named in the registry row's
    // `depends_on` (recorded in the Phase B report for Phase C to reconcile —
    // the canary asserts what the code reads, not what the row happens to
    // list). Each one renders as its own number in the context bar.
    assert!(
        ctx.remaining_percentage.is_some_and(|p| p > 0.0),
        "context_window.remaining_percentage gone"
    );
    assert!(
        ctx.cache_read_tokens.is_some_and(|t| t > 0),
        "context_window.current_usage.cache_read_input_tokens gone"
    );
    assert!(
        ctx.cache_creation_tokens.is_some_and(|t| t > 0),
        "context_window.current_usage.cache_creation_input_tokens gone"
    );
    assert!(
        ctx.input_tokens.is_some_and(|t| t > 0),
        "context_window.current_usage.input_tokens gone"
    );
    assert!(
        ctx.output_tokens.is_some_and(|t| t > 0),
        "context_window.current_usage.output_tokens gone"
    );

    // And the composed push the widget actually consumes: `extract_push`
    // returns `None` for a non-substantive snapshot, so a reshape that costs
    // every field writes nothing rather than writing zeros.
    let push = crate::statusline::extract_push(&payload)
        .expect("claude.statusline.stdin: the payload no longer produces a push at all");
    assert!(push.has_rate_limits(), "push lost both quota windows");
    assert!(
        push.context.is_some(),
        "push lost the context reading (NC-3)"
    );
    assert!(push.is_substantive());
}

// ── opencode.sse.events ─────────────────────────────────────────────────────

/// `oob/opencode.rs::Tracker::handle` still turns one turn's SSE envelopes into
/// spoken assistant text and still binds the tab to the session.
///
/// Driven as an ordered stream rather than as isolated events, because
/// `Tracker` is a state machine: `message.updated` declares the message
/// assistant, `message.part.updated` types the part, `message.part.delta`
/// accumulates the text, and only the completed `message.updated` flushes.
/// Anything less than the whole sequence cannot show that text still comes out
/// the other end.
#[tokio::test]
async fn canary_opencode_sse_events() {
    row("opencode.sse.events");

    let raw = fixture("opencode/1.18.13/sse.assistant-turn.jsonl");
    let events = json_lines(&raw);
    assert!(
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
    assert!(
        !expected_text.trim().is_empty(),
        "fixture guard: the fixture must carry non-empty delta text"
    );
    let expected_session = events[0]
        .get("properties")
        .and_then(|p| p.get("sessionID"))
        .and_then(Value::as_str)
        .expect("fixture guard: every session-scoped event carries properties.sessionID");

    let (ctx, mut tts_rx, _signals) = opencode_ctx();
    let mut tracker = crate::oob::opencode::Tracker::default();
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
            assert!(!text.trim().is_empty(), "spoken text is empty");
            assert_eq!(
                text, expected_text,
                "opencode.sse.events: the assistant text no longer survives the stream — check \
                 properties.part.messageID / properties.partID / properties.delta"
            );
        }
        other => panic!(
            "opencode.sse.events: a completed assistant message produced no speech ({other:?}) — \
             something in the chain moved: `message.updated` / `properties.info.role` / \
             `properties.info.time.completed` (no flush), or `properties.part.messageID` / \
             `properties.messageID` / `properties.partID` (nothing registered under the message)"
        ),
    }

    assert_eq!(
        tracker.current_session().as_deref(),
        Some(expected_session),
        "opencode.sse.events: properties.sessionID no longer binds the tab to its session (V28 \
         per-tab identity, and the V30 push target)"
    );
}

/// A tap context wired to the built-in OpenCode tab, so the per-tab TTS gate is
/// satisfied and `ctx.speak` actually delivers. Mirrors `oob::opencode`'s own
/// `ctx_with`; kept local rather than hoisted so this module can be read (and
/// moved, in Phase K) on its own.
fn opencode_ctx() -> (
    crate::oob::OobContext,
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
    let ctx = crate::oob::OobContext {
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

// ── the corpus itself ───────────────────────────────────────────────────────

/// The four keys every `MANIFEST.toml` must carry.
const MANIFEST_KEYS: [&str; 4] = ["captured_from", "date", "method", "redaction"];

/// Every `<harness>/<version>/` directory carries a `MANIFEST.toml` with all
/// four provenance keys, and at least one fixture beside it.
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
                let value = manifest
                    .lines()
                    .map(str::trim_start)
                    .find_map(|l| l.strip_prefix(key))
                    .and_then(|rest| rest.trim_start().strip_prefix('='))
                    .map(|rest| rest.trim().trim_matches('"').trim())
                    .unwrap_or("");
                // Present-but-blank is absent with extra steps.
                assert!(
                    value.len() > 3,
                    "{}: MANIFEST.toml key `{key}` is missing or blank",
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
