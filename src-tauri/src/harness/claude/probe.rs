//! **Claude Code's L2 probes** — the bodies `harness/probe.rs`'s neutral runner
//! drives through [`HarnessPlugin::probe`](crate::harness::plugin::HarnessPlugin::probe).
//!
//! V40 Phase A, locked decision 17: the runner keeps the report shape, the
//! outcome model and the `cimp --harness-canary` CLI; **what** is driven against
//! an installed Claude Code — the `--help` option column and the newest
//! transcript's tail — lives here, with the harness it is true of. Moved
//! verbatim: same text, same assertions.
//!
//! # Reading a real transcript, printing none of it
//!
//! The transcript probes tail a **real** session JSONL, which carries user
//! prompts, file contents, tool output and plausibly credentials (V35 locked
//! decision 4). So: nothing is written, nothing is copied, and every detail
//! string carries **counts and field names only** — never a payload value,
//! never the transcript path, never the session id. The single exception is the
//! CLI build string from the `version` field, which is harness metadata.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::harness::capture::{self, Observed};
use crate::harness::contract;
use crate::harness::probe::{Outcome, ProbeResult, SERVE_POLL_INTERVAL};

// ── timings and bounds, all deliberate ──────────────────────────

const HELP_TIMEOUT: Duration = Duration::from_secs(20);

/// How much of the newest transcript to read, from the END. A transcript grows
/// without bound and the probe only needs *recent* evidence, so this is a tail
/// rather than a scan — and a bounded read is also the privacy posture: the
/// less of a user's session that enters this process, the better.
const TAIL_BYTES: u64 = 512 * 1024;
/// …and at most this many parsed lines out of that window, so a transcript of
/// very short lines cannot balloon the working set.
const TAIL_LINES: usize = 600;


// ── claude: spawn flags via --help ──────────────────────────────────────────

/// Every option token `claude --help` declares. Parsed from the OPTION COLUMN
/// only (commander.js indents an option definition by exactly two spaces and
/// wraps its description far to the right), because `--settings` and friends
/// also appear inside other options' prose — a naive substring search finds
/// `--settings` in `--bare`'s description and would report a deleted flag as
/// present.
fn help_option_tokens(help: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in help.lines() {
        if !line.starts_with("  -") || line.starts_with("   ") {
            continue;
        }
        // `  -c, --continue    Continue …` → the definition is everything
        // before the first double-space gap.
        let def = line.trim_start();
        let def = def.split("  ").next().unwrap_or(def);
        for tok in def.split([',', ' ', '\t']) {
            let tok = tok.trim();
            if tok.starts_with('-') && tok.len() > 1 {
                out.insert(tok.to_string());
            }
        }
    }
    out
}

/// `claude --help`, or an `unknown` reason. Stdout and stderr are joined
/// because a CLI is free to print usage to either.
fn claude_help() -> Result<String, String> {
    let binary = crate::pty::resolve_command("claude").map_err(|_| {
        "`claude` is not on PATH (nor in ebin/) — nothing to probe. Not a failure: an \
         uninstalled harness cannot drift."
            .to_string()
    })?;
    let mut cmd = Command::new(&binary);
    cmd.arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::procutil::CREATE_NO_WINDOW);
    }
    // `output()` reads both pipes to EOF, so there is no deadlock to bound —
    // but a hung child would hang the probe, so it is spawned and reaped with a
    // deadline rather than blocked on.
    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let mut child = crate::spawn_gate::spawn_std(&mut cmd)
        .map_err(|e| format!("`claude --help` could not be spawned: {e}"))?;
    let deadline = Instant::now() + HELP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(SERVE_POLL_INTERVAL),
            Ok(None) => {
                crate::procutil::reap_probe_child(super::plugin::me(), &mut child);
                return Err(format!(
                    "`claude --help` did not exit within {}s",
                    HELP_TIMEOUT.as_secs()
                ));
            }
            Err(e) => return Err(format!("`claude --help` could not be waited on: {e}")),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("`claude --help` output could not be read: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

/// The declared flags of one capability row, in declaration order. Read from
/// the registry rather than repeated here, so a row that grows a flag grows the
/// probe with it.
fn declared_flags(id: &str) -> Vec<&'static str> {
    contract::get(id)
        .map(|c| {
            c.depends_on
                .iter()
                .filter_map(|d| match d {
                    contract::Dep::Flag(f) => Some(*f),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The two Tier-B spawn-flag rows. Both are answered from one `claude --help`.
///
/// Returns that help text alongside the rows when it was readable **and
/// parseable as an option list** (V35 Phase H) — an unrecognizable help screen
/// makes both rows `unknown`, and filing an unreadable one as a known-good
/// capture would seed the corpus with the shape a later diff is supposed to
/// flag.
pub(in crate::harness) fn probe_claude_flags() -> (Vec<ProbeResult>, Option<String>) {
    let (session_id, settings) = ("claude.flag.session_id", "claude.flag.settings_overlay");
    let help = match claude_help() {
        Ok(h) => h,
        Err(why) => {
            return (
                vec![
                    ProbeResult::new(session_id, Outcome::Unknown { why: why.clone() }),
                    ProbeResult::new(settings, Outcome::Unknown { why }),
                ],
                None,
            );
        }
    };
    let tokens = help_option_tokens(&help);
    // The anti-cry-wolf guard. If the help format changes enough that the
    // option column stops being recognizable, EVERY flag reads as missing and
    // the probe reports two loud false failures. Below this floor the parse
    // itself is what is unknown, so say that instead.
    if tokens.len() < 10 {
        let why = format!(
            "`claude --help` no longer parses as an option list ({} option tokens found, expected \
             dozens) — the probe cannot tell a renamed flag from a reformatted help screen",
            tokens.len()
        );
        return (
            vec![
                ProbeResult::new(session_id, Outcome::Unknown { why: why.clone() }),
                ProbeResult::new(settings, Outcome::Unknown { why }),
            ],
            None,
        );
    }

    let mut out = Vec::new();
    for id in [session_id, settings] {
        let declared = declared_flags(id);
        let missing: Vec<&str> = declared
            .iter()
            .copied()
            .filter(|f| !tokens.contains(*f))
            .collect();
        let outcome = if declared.is_empty() {
            Outcome::Unknown {
                why: "the registry row declares no `Dep::Flag`, so there is nothing to check"
                    .to_string(),
            }
        } else if missing.is_empty() {
            let extra = if id == settings {
                " NOTE: the deeper half of this row — whether the installed CLI still HONORS the \
                 `hooks` / `statusLine` / `permissions` keys inside the overlay — needs a scripted \
                 turn and is NOT covered here (issue #64 stays open)."
            } else {
                ""
            };
            Outcome::Pass {
                detail: format!(
                    "all {} declared flag(s) still declared by `claude --help`: {}.{extra}",
                    declared.len(),
                    declared.join(", ")
                ),
            }
        } else {
            Outcome::Fail {
                detail: format!(
                    "`claude --help` no longer declares: {}. Declared flags still present: {}. \
                     A vanished selector is not cosmetic — cImp puts these on the child's argv.",
                    missing.join(", "),
                    declared
                        .iter()
                        .filter(|f| tokens.contains(**f))
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        };
        out.push(ProbeResult::new(id, outcome));
    }
    (out, Some(help))
}
// ── claude: the transcript tail ─────────────────────────────────────────────

/// A bounded window onto the newest real transcript. The parsed lines leave
/// this module only through [`Observed`], on the way to a scrub; `session_id` is
/// used only as the expected value for
/// [`crate::harness::claude::read::record_names_session`].
struct Tail {
    lines: Vec<Value>,
    session_id: String,
    /// Non-JSON / non-object lines in the window, reported as a count so a
    /// wholesale format change (JSONL → something else) is visible rather than
    /// silently reducing the sample.
    unparsed: usize,
}

/// The newest `*.jsonl` under `~/.claude/projects/`, preferring the project the
/// probe was run in. Path discovery goes through `harness::claude::read` — the tap's own
/// helpers — so the probe cannot verify a layout the tap does not read.
fn newest_transcript() -> Option<PathBuf> {
    let root = crate::harness::claude::read::projects_root()?;
    if let Some(here) = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::harness::claude::read::project_root(&cwd))
        .and_then(|dir| crate::harness::claude::read::newest_jsonl(&dir))
    {
        return Some(here);
    }
    // No transcript for this project: fall back to the newest anywhere, since
    // the shapes being probed are harness-wide and not project-specific.
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(candidate) = crate::harness::claude::read::newest_jsonl(&entry.path()) else {
            continue;
        };
        let Ok(mtime) = candidate.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, candidate));
        }
    }
    newest.map(|(_, p)| p)
}

/// Read the last [`TAIL_BYTES`] of `path` and parse up to [`TAIL_LINES`]
/// trailing JSON objects out of it. The first (probably partial) line of the
/// window is dropped.
fn read_tail(path: &PathBuf) -> Option<Tail> {
    use std::io::{Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let text = if from > 0 {
        // Mid-file window: the first line is a fragment, not a record.
        text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        text.as_str()
    };

    let mut lines: Vec<Value> = Vec::new();
    let mut unparsed = 0usize;
    for raw in text.lines().rev() {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(raw) {
            Ok(v) if v.is_object() => lines.push(v),
            _ => unparsed += 1,
        }
        if lines.len() >= TAIL_LINES {
            break;
        }
    }
    lines.reverse();
    Some(Tail {
        lines,
        session_id: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string(),
        unparsed,
    })
}

/// The three `claude.transcript.*` rows, all read from one tail — plus, since
/// V35 Phase H, up to [`capture::LINES_PER_CAPABILITY`] of the lines that
/// actually satisfied each row's substantiveness predicate, and the CLI build
/// string those lines carry.
pub(in crate::harness) fn probe_claude_transcript() -> (Vec<ProbeResult>, Vec<Observed>, String) {
    let ids = [
        "claude.transcript.usage",
        "claude.transcript.tool_result",
        "claude.transcript.identity",
        "claude.transcript.assistant_text",
        "claude.transcript.stop_reason",
    ];
    let unknown = |why: String| {
        (
            ids.iter()
                .map(|id| ProbeResult::new(id, Outcome::Unknown { why: why.clone() }))
                .collect::<Vec<_>>(),
            Vec::new(),
            String::new(),
        )
    };
    let tail = newest_transcript().and_then(|p| read_tail(&p));
    let Some(tail) = tail else {
        return unknown(
            "no Claude Code session transcript found under ~/.claude/projects — nothing to tail. \
             Not a failure: an unused harness cannot drift."
                .to_string(),
        );
    };
    if tail.lines.is_empty() {
        return unknown(format!(
            "the newest transcript's last {} KiB held no parseable JSON object ({} unparsed \
             lines) — the artifact may no longer be JSONL",
            TAIL_BYTES / 1024,
            tail.unparsed
        ));
    }

    let results = vec![
        ProbeResult::new(ids[0], usage_outcome(&tail)),
        ProbeResult::new(ids[1], tool_result_outcome(&tail)),
        ProbeResult::new(ids[2], identity_outcome(&tail)),
        ProbeResult::new(ids[3], assistant_text_outcome(&tail)),
        ProbeResult::new(ids[4], stop_reason_outcome(&tail)),
    ];
    // Only rows that PASSED contribute lines. A line that failed the
    // substantiveness predicate is not a known-good shape, and the harness-level
    // gate in `capture::on_success` would not save us here: an `unknown` row
    // sits in a run that can still be all-pass overall.
    let mut observed = Vec::new();
    for (id, lines) in [
        (ids[0], substantive_lines(&tail, usage_is_substantive)),
        (ids[1], substantive_lines(&tail, tool_result_is_substantive)),
        (
            ids[2],
            substantive_lines(&tail, |l| identity_is_substantive(l, &tail.session_id)),
        ),
        (ids[3], substantive_lines(&tail, assistant_text_is_substantive)),
        (ids[4], substantive_lines(&tail, stop_reason_is_substantive)),
    ] {
        let passed = results
            .iter()
            .any(|r| r.id == id && matches!(r.outcome, Outcome::Pass { .. }));
        if passed && !lines.is_empty() {
            observed.push(Observed::new(id, "jsonl", lines.join("\n")));
        }
    }
    (results, observed, newest_cli_version(&tail))
}

/// Up to [`capture::LINES_PER_CAPABILITY`] transcript lines satisfying `keep`,
/// re-serialized. The **newest** ones (the tail is in file order), because a
/// shape that changed recently is the one a diff is looking for.
///
/// Re-serialized rather than kept as raw text: `read_tail` already parsed them,
/// carrying the raw bytes alongside would double the window's footprint, and a
/// canonical form makes the corpus diff on structure instead of on whitespace.
fn substantive_lines(tail: &Tail, keep: impl Fn(&Value) -> bool) -> Vec<String> {
    tail.lines
        .iter()
        .rev()
        .filter(|l| keep(l))
        .take(capture::LINES_PER_CAPABILITY)
        .filter_map(|l| serde_json::to_string(l).ok())
        .collect()
}

/// The newest CLI build string in the window — the version a Claude capture is
/// stamped with (V35 Phase H, locked decision 6).
///
/// Read from the transcript's own `version` field, which is what
/// `claude.transcript.identity` already proves is there and what the
/// `harness_versions` tripwire is fed by. Newest rather than the whole set: a
/// window can straddle an auto-update, and a capture belongs to the build that
/// produced its newest lines.
fn newest_cli_version(tail: &Tail) -> String {
    tail.lines
        .iter()
        .rev()
        .find_map(crate::harness::claude::read::cli_version_of)
        .unwrap_or_default()
        .to_string()
}

/// `message.usage.*` still produces substantive turns.
///
/// The failure predicate needs an INDEPENDENT witness that a turn happened,
/// because `parse_usage_line` returning nothing is equally consistent with "the
/// field moved" and "this window holds no assistant lines". The witness is the
/// count of `type == "assistant"` lines carrying a `message`: if there are
/// some and none of them yields a substantive `Turn`, the shape moved.
fn usage_outcome(tail: &Tail) -> Outcome {
    let assistant = tail
        .lines
        .iter()
        .filter(|l| {
            l.get("type").and_then(Value::as_str) == Some("assistant") && l.get("message").is_some()
        })
        .count();
    if assistant == 0 {
        return Outcome::Unknown {
            why: format!(
                "no `type: \"assistant\"` line in the last {} transcript lines — nothing to read \
                 a usage block out of",
                tail.lines.len()
            ),
        };
    }

    let (mut turns, mut substantive, mut cached) = (0usize, 0usize, 0usize);
    for line in &tail.lines {
        let Some(crate::graph::UsageEvent::Turn {
            cache_read,
            cache_make,
            ..
        }) = crate::harness::claude::read::parse_usage_line(line, crate::harness::claude::usage::ORIGIN_SESSION)
        else {
            continue;
        };
        turns += 1;
        if usage_is_substantive(line) {
            substantive += 1;
        }
        if cache_read > 0 || cache_make > 0 {
            cached += 1;
        }
    }

    if substantive == 0 {
        return Outcome::Fail {
            detail: format!(
                "{assistant} assistant line(s) in the window, {turns} parsed as a usage Turn, but \
                 NONE carried a substantive reading (non-empty `message.id` + `message.model` + \
                 `usage.input_tokens` > 0 + `usage.output_tokens` > 0). Every one of those \
                 readers ends in `unwrap_or(0)`, so a rename shows up as zeros in the Usage tab \
                 and nothing errors."
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "{substantive}/{turns} usage Turn(s) substantive out of {assistant} assistant line(s); \
             cache counters non-zero on {cached}. (The cache pair is REPORTED, not asserted: \
             prompt caching can legitimately be off for an account, so failing on it would be a \
             false alarm.)"
        ),
    }
}

/// One line yields a substantive usage `Turn`: non-empty `message.id`, a
/// `message.model`, and both token counts above zero.
///
/// **The one spelling of the predicate** (V35 Phase H). [`usage_outcome`] counts
/// with it and [`substantive_lines`] selects with it, so the corpus can never
/// contain a line the probe would not have accepted — two spellings would let
/// the capture drift into recording shapes the canary already rejects.
fn usage_is_substantive(line: &Value) -> bool {
    let Some(crate::graph::UsageEvent::Turn {
        msg_id,
        model,
        in_tok,
        out_tok,
        ..
    }) = crate::harness::claude::read::parse_usage_line(line, crate::harness::claude::usage::ORIGIN_SESSION)
    else {
        return false;
    };
    !msg_id.is_empty() && model.is_some_and(|m| !m.is_empty()) && in_tok > 0 && out_tok > 0
}

/// One `tool_result` block was read with an id and a non-empty body. See
/// [`usage_is_substantive`] for why this is a named function.
fn tool_result_is_sized(id: &str, chars: usize) -> bool {
    !id.is_empty() && chars > 0
}

/// One line carries at least one sized `tool_result`.
fn tool_result_is_substantive(line: &Value) -> bool {
    crate::harness::claude::read::extract_tool_results(line)
        .iter()
        .any(|(id, chars)| tool_result_is_sized(id, *chars))
}

/// One line yields at least one non-empty speakable block, with a keyed dedup
/// id. See [`usage_is_substantive`] for why this is a named function.
fn assistant_text_is_substantive(line: &Value) -> bool {
    crate::harness::claude::read::assistant_texts(line)
        .iter()
        .any(|(key, text)| key.contains(':') && !text.trim().is_empty())
}

/// `message.content[].text` still yields speakable prose (V35 Phase L).
///
/// Independent witness, on the same discipline as its two siblings: the count
/// of assistant lines that carry a `message.content` ARRAY. A window with such
/// lines and no readable text block is drift — the reader would run, find
/// nothing, and the tab would go mute with no error anywhere. A window with no
/// assistant content arrays at all is simply not evidence.
///
/// Deliberately NOT a failure when every content array holds only `thinking` or
/// `tool_use` blocks: that is a real and normal shape (a turn that only called
/// tools), and treating it as drift would fire on ordinary sessions.
fn assistant_text_outcome(tail: &Tail) -> Outcome {
    let with_content = tail
        .lines
        .iter()
        .filter(|l| {
            l.get("type").and_then(Value::as_str) == Some("assistant")
                && crate::harness::claude::read::message_parts(l).is_some()
        })
        .count();
    if with_content == 0 {
        return Outcome::Unknown {
            why: format!(
                "no assistant line with a `message.content[]` array in the last {} transcript \
                 lines — nothing to read a text block out of",
                tail.lines.len()
            ),
        };
    }
    let text_blocks: usize = tail
        .lines
        .iter()
        .map(|l| crate::harness::claude::read::assistant_texts(l).len())
        .sum();
    let substantive = tail
        .lines
        .iter()
        .filter(|l| assistant_text_is_substantive(l))
        .count();
    if substantive == 0 {
        return Outcome::Fail {
            detail: format!(
                "{with_content} assistant line(s) carry a `message.content[]` array but NONE \
                 yielded a speakable text block ({text_blocks} extracted) — \
                 `content[].type == \"text\"` or `content[].text` has moved. A tab with no `Stop` \
                 push would go silently mute."
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "{substantive}/{with_content} assistant line(s) yielded speakable prose \
             ({text_blocks} text block(s) total)"
        ),
    }
}

/// One line declares a stop reason at all — the field the turn boundary is
/// read from. See [`usage_is_substantive`] for why this is a named function.
fn stop_reason_is_substantive(line: &Value) -> bool {
    line.get("type").and_then(Value::as_str) == Some("assistant")
        && line
            .get("message")
            .and_then(|m| m.get("stop_reason"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
}

/// `message.stop_reason` still says where a turn ends (V39).
///
/// The delegation completion feed's boundary on the FALLBACK path: with no
/// `Stop` push for a tab, this field is the only thing in the transcript that
/// says which assistant message is the turn's last one.
///
/// Independent witness on the same discipline as its three siblings, and a
/// different field path from the thing witnessed: the count of assistant lines
/// carrying a `message` object at all. Such lines with NO readable stop reason
/// between them is drift — every turn then reads as still running, no
/// completion is ever filed, and a delegation waits out its whole deadline
/// before reporting `timeout` for a turn that ended in seconds.
///
/// Deliberately NOT a failure when every stop reason in the window is
/// `tool_use`: a window can legitimately hold nothing but tool-calling turns.
/// What is asserted is that the FIELD still reads; the split between turn-end
/// and mid-turn is reported instead, so a build that started answering the same
/// value for everything is visible in the detail line rather than silently
/// passing.
fn stop_reason_outcome(tail: &Tail) -> Outcome {
    let assistant = tail
        .lines
        .iter()
        .filter(|l| {
            l.get("type").and_then(Value::as_str) == Some("assistant") && l.get("message").is_some()
        })
        .count();
    if assistant == 0 {
        return Outcome::Unknown {
            why: format!(
                "no assistant line in the last {} transcript lines — nothing to read a stop \
                 reason out of",
                tail.lines.len()
            ),
        };
    }
    let declared = tail
        .lines
        .iter()
        .filter(|l| stop_reason_is_substantive(l))
        .count();
    if declared == 0 {
        return Outcome::Fail {
            detail: format!(
                "{assistant} assistant line(s) in the window and NONE carries a readable \
                 `message.stop_reason` — the fallback reader can no longer tell a turn's end \
                 from a tool pause, so a delegation into a tab with no `Stop` push files no \
                 completion at all and waits out its entire deadline"
            ),
        };
    }
    let ended = tail
        .lines
        .iter()
        .filter(|l| crate::harness::claude::read::is_turn_end(l))
        .count();
    Outcome::Pass {
        detail: format!(
            "{declared}/{assistant} assistant line(s) declare a stop reason; {ended} read as the \
             END of a turn and {} as mid-turn (a window of nothing but tool-calling turns is \
             normal, so only the field itself is asserted)",
            declared.saturating_sub(ended)
        ),
    }
}

/// One line carries BOTH identity fields: a top-level `sessionId` naming its own
/// file, and a `version`.
///
/// Stricter than [`identity_outcome`]'s pass condition, which accepts the two
/// facts from different lines — deliberately, because a capture wants one line
/// that demonstrates the whole shape rather than two that each demonstrate half.
fn identity_is_substantive(line: &Value, session_id: &str) -> bool {
    crate::harness::claude::read::record_names_session(line, session_id)
        && crate::harness::claude::read::cli_version_of(line).is_some()
}

/// `message.content[].tool_result` still yields sized results.
///
/// Independent witness again, and a different field path so the witness cannot
/// fail for the same reason as the thing witnessed: `tool_use` blocks share
/// `message.content[]` with `tool_result` but none of `tool_use_id` /
/// `is_error` / `content`. A window with tool calls but no readable results is
/// drift; a window with no tool calls at all is simply not evidence.
fn tool_result_outcome(tail: &Tail) -> Outcome {
    let tool_uses = tail
        .lines
        .iter()
        .filter_map(crate::harness::claude::read::message_parts)
        .flat_map(|parts| parts.iter())
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool_use"))
        .count();

    let mut results = 0usize;
    let mut sized = 0usize;
    for line in &tail.lines {
        for (id, chars) in crate::harness::claude::read::extract_tool_results(line) {
            results += 1;
            if tool_result_is_sized(&id, chars) {
                sized += 1;
            }
        }
    }
    // `is_error` is a SECOND reader of the same blocks (`tool_result_is_error`,
    // the commit-provenance guard) — counted so a flag stuck at one value is
    // visible, but never asserted: a healthy session may contain no failed
    // tool call at all.
    let errors = tail
        .lines
        .iter()
        .filter_map(crate::harness::claude::read::message_parts)
        .flat_map(|parts| parts.iter())
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter(|p| crate::harness::claude::read::tool_result_is_error(p))
        .count();

    if tool_uses == 0 && results == 0 {
        return Outcome::Unknown {
            why: format!(
                "no tool call in the last {} transcript lines (neither a `tool_use` block nor a \
                 readable `tool_result`), so an empty result set proves nothing",
                tail.lines.len()
            ),
        };
    }
    if sized == 0 {
        return Outcome::Fail {
            detail: format!(
                "{tool_uses} `tool_use` block(s) in the window but {results} readable \
                 `tool_result`(s) with a non-empty `tool_use_id` and >0 chars. \
                 `extract_tool_results` skips a block whose id it cannot read, so this degrades to \
                 an EMPTY set — indistinguishable from a user turn that ran no tools, and the row \
                 has no V16 rule lagging it."
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "{sized}/{results} tool_result block(s) read with an id and >0 chars, against \
             {tool_uses} `tool_use` block(s); `is_error` true on {errors} (reported, not \
             asserted — a session with no failed tool call is normal)."
        ),
    }
}

/// Top-level `sessionId` and `version` still identify a transcript line.
///
/// This row is the inverse of a lagging indicator: `drift.harness_version.v1`
/// is *fed* by `version`, so losing the field silences the tripwire instead of
/// firing it. That is precisely why it gets a leading check.
fn identity_outcome(tail: &Tail) -> Outcome {
    let named = tail
        .lines
        .iter()
        .filter(|l| crate::harness::claude::read::record_names_session(l, &tail.session_id))
        .count();
    let versions: BTreeSet<&str> = tail
        .lines
        .iter()
        .filter_map(crate::harness::claude::read::cli_version_of)
        .collect();
    let sidechain = tail
        .lines
        .iter()
        .filter(|l| l.get("isSidechain").and_then(Value::as_bool) == Some(true))
        .count();
    let meta = tail
        .lines
        .iter()
        .filter(|l| l.get("isMeta").and_then(Value::as_bool) == Some(true))
        .count();

    let mut broken: Vec<&str> = Vec::new();
    if named == 0 {
        broken.push("`sessionId` (no line in the window names its own file's session)");
    }
    if versions.is_empty() {
        broken.push("`version` (no line carries a CLI build string)");
    }
    if !broken.is_empty() {
        return Outcome::Fail {
            detail: format!(
                "over {} transcript line(s) the identity fields are gone: {}. Losing `sessionId` \
                 breaks the H-2 own-record predicate the live-session registry is gated on; \
                 losing `version` SILENCES `drift.harness_version.v1` rather than firing it.",
                tail.lines.len(),
                broken.join(" and ")
            ),
        };
    }
    Outcome::Pass {
        detail: format!(
            "{named}/{} line(s) carry a matching top-level `sessionId`; CLI build string(s) seen: \
             {}; `isSidechain` on {sidechain}, `isMeta` on {meta} (both reported, not asserted — \
             a session with no sub-agent and no synthetic line is normal).",
            tail.lines.len(),
            versions.into_iter().collect::<Vec<_>>().join(", ")
        ),
    }
}

/// This probe's row in the external-process spawn ledger (V40 Phase E, locked
/// decision 26).
///
/// The ledger's tripwire scans every `.rs` file for spawn constructors and
/// demands a matching row, so the row has to exist — but `claude --help` is one
/// product's command line, and core is not where a table of those belongs. The
/// plugin hands it over through `HarnessPlugin::spawn_sites`.
pub(crate) const SPAWN_SITES: &[crate::spawn_ledger::SpawnSite] = &[
    crate::spawn_ledger::SpawnSite {
        file: "harness/claude/probe.rs",
        symbol: "claude_help",
        spawns: "claude --help",
        class: crate::spawn_ledger::SpawnClass::HostSpawn,
        count: 1,
        reason: "V35 Phase D's L2 live probe, reached only from `cimp --harness-canary` — a \
                 maintenance command a human (or a scheduled script) runs, never a tool call, \
                 and it exits before any Tauri/app init. The program is a FIXED name resolved \
                 through `pty::resolve_command`, the argv is a literal in code, and no model \
                 input reaches it. Sandboxing it would be self-defeating: the entire point is to \
                 observe what the user's REAL installed harness does. V40 Phase A moved this \
                 half of the old `harness/probe.rs` row into the plugin that owns it (locked \
                 decision 17); the runner spawns nothing.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The option-column parser is the whole reason the two flag probes can
    /// fail honestly, and it has one specific trap: `claude --help` mentions
    /// `--settings` inside `--bare`'s *description*, so a substring search
    /// would report a deleted flag as present. Anchored on the two-space
    /// option column, with a wrapped continuation line to prove the anchor.
    #[test]
    fn help_option_tokens_reads_the_option_column_only() {
        let help = "\
Usage: claude [options] [command] [prompt]

Options:
  --add-dir <directories...>            Additional directories to allow tool
                                        access to
  --bare                                Minimal mode: … Explicitly provide
                                        context via: --system-prompt[-file],
                                        --settings, --agents, --plugin-dir.
  -c, --continue                        Continue the most recent conversation
  --allowedTools, --allowed-tools <tools...>
      Comma or space-separated list of tool names
";
        let tokens = help_option_tokens(help);
        assert!(tokens.contains("--add-dir"));
        assert!(tokens.contains("--bare"));
        assert!(tokens.contains("-c"));
        assert!(tokens.contains("--continue"));
        // The wrapped definition line is still an option column line.
        assert!(tokens.contains("--allowedTools"));
        assert!(tokens.contains("--allowed-tools"));
        // …and the trap: named only in prose, so NOT declared.
        assert!(
            !tokens.contains("--settings"),
            "a flag mentioned inside another option's description must not read as declared — \
             that is how a DELETED flag would probe as present"
        );
        assert!(!tokens.contains("--agents"));
        assert!(!tokens.contains("--system-prompt[-file],"));
    }

    /// The registry is what tells the flag probe which flags to look for, so a
    /// row that grows a `Dep::Flag` grows the probe with it — and a row that
    /// lost them all must not silently probe nothing (that is the
    /// `declared.is_empty()` → `unknown` branch).
    #[test]
    fn declared_flags_comes_from_the_registry() {
        let session = declared_flags("claude.flag.session_id");
        assert!(session.contains(&"--session-id"), "{session:?}");
        assert!(
            session.len() >= 4,
            "the row declares the competing selectors too: {session:?}"
        );
        assert_eq!(declared_flags("claude.flag.settings_overlay"), ["--settings"]);
        // Only `Dep::Flag` — a `ConfigKey` is not a command-line flag and must
        // not be looked for in `--help`.
        assert!(declared_flags("claude.transcript.usage").is_empty());
        assert!(declared_flags("no.such.capability").is_empty());
    }

    /// A transcript window with no evidence in it is `unknown`; one with an
    /// independent witness but no readable shape is `fail`. This is the
    /// distinction that keeps the transcript probes from crying wolf on a fresh
    /// session while still catching a real rename.
    #[test]
    fn transcript_probes_need_an_independent_witness_to_fail() {
        let tail = |lines: &[&str]| Tail {
            lines: lines
                .iter()
                .map(|l| serde_json::from_str(l).expect("test fixture json"))
                .collect(),
            session_id: "sess-1".to_string(),
            unparsed: 0,
        };

        // No assistant line at all ⇒ nothing to read usage out of.
        let quiet = tail(&[r#"{"type":"user","sessionId":"sess-1","version":"2.1.232"}"#]);
        assert!(matches!(usage_outcome(&quiet), Outcome::Unknown { .. }));
        // …and no tool call at all ⇒ an empty result set proves nothing.
        assert!(matches!(
            tool_result_outcome(&quiet),
            Outcome::Unknown { .. }
        ));

        // An assistant line whose token fields were renamed: the witness says a
        // turn happened, the reader says zero. That is the silent-zeros class.
        let renamed = tail(&[
            r#"{"type":"assistant","sessionId":"sess-1","version":"2.1.232","message":
                {"id":"m1","model":"claude-x","usage":{"inputTokens":10,"outputTokens":5}}}"#,
        ]);
        let outcome = usage_outcome(&renamed);
        assert!(outcome.is_fail(), "{outcome:?}");

        // The healthy shape passes.
        let healthy = tail(&[
            r#"{"type":"assistant","sessionId":"sess-1","version":"2.1.232","message":
                {"id":"m1","model":"claude-x","usage":{"input_tokens":10,"output_tokens":5,
                "cache_read_input_tokens":7,"cache_creation_input_tokens":3}}}"#,
        ]);
        assert!(matches!(usage_outcome(&healthy), Outcome::Pass { .. }));

        // tool_result: a `tool_use` block is the witness; a renamed
        // `tool_use_id` empties the result set silently.
        let tools_broken = tail(&[
            r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"m1","content":
                [{"type":"tool_use","id":"t1","name":"Read"}]}}"#,
            r#"{"type":"user","sessionId":"sess-1","message":{"content":
                [{"type":"tool_result","toolUseId":"t1","content":"hello"}]}}"#,
        ]);
        assert!(tool_result_outcome(&tools_broken).is_fail());

        let tools_ok = tail(&[
            r#"{"type":"assistant","sessionId":"sess-1","message":{"id":"m1","content":
                [{"type":"tool_use","id":"t1","name":"Read"}]}}"#,
            r#"{"type":"user","sessionId":"sess-1","message":{"content":
                [{"type":"tool_result","tool_use_id":"t1","content":"hello"}]}}"#,
        ]);
        assert!(matches!(
            tool_result_outcome(&tools_ok),
            Outcome::Pass { .. }
        ));

        // identity: both fields present ⇒ pass; either gone ⇒ fail.
        assert!(matches!(identity_outcome(&healthy), Outcome::Pass { .. }));
        let no_version = tail(&[r#"{"type":"user","sessionId":"sess-1"}"#]);
        assert!(identity_outcome(&no_version).is_fail());
        let wrong_session = tail(&[r#"{"type":"user","sessionId":"other","version":"2.1.232"}"#]);
        assert!(identity_outcome(&wrong_session).is_fail());
    }

    /// Nothing the probe prints may carry transcript CONTENT. The detail
    /// strings are counts and field names by construction; this pins the
    /// construction, because the readers are handed real user data and the
    /// report is meant to be pasted into an issue.
    #[test]
    fn transcript_details_carry_no_payload() {
        let secret = "hunter2-do-not-print";
        let tail = Tail {
            lines: vec![
                serde_json::from_str(&format!(
                    r#"{{"type":"assistant","sessionId":"s","version":"2.1.232","message":
                       {{"id":"m1","model":"claude-x","usage":{{"input_tokens":1,
                       "output_tokens":1}},"content":[{{"type":"tool_use","id":"t1",
                       "name":"Read","input":{{"file_path":"{secret}"}}}}]}}}}"#
                ))
                .unwrap(),
                serde_json::from_str(&format!(
                    r#"{{"type":"user","sessionId":"s","message":{{"content":
                       [{{"type":"tool_result","tool_use_id":"t1","content":"{secret}"}}]}}}}"#
                ))
                .unwrap(),
            ],
            session_id: "s".to_string(),
            unparsed: 0,
        };
        for outcome in [
            usage_outcome(&tail),
            tool_result_outcome(&tail),
            identity_outcome(&tail),
        ] {
            assert!(
                !outcome.detail().contains(secret),
                "a probe detail leaked transcript payload: {}",
                outcome.detail()
            );
        }
    }
}
