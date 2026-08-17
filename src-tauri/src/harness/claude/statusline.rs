//! **The Claude-shaped status-line payload** — the parsing half of
//! `cimp --statusline` (V35 Phase K, design § 4: "CLI entry stays put; the
//! Claude-shaped parsing moves to `harness/claude/statusline.rs`").
//!
//! Claude Code runs `cimp --statusline` and pipes one JSON object to its stdin.
//! [`crate::statusline`] still owns the *entry point* and the *rendering* — a
//! coloured bar is a cImp surface, not a harness one — but every field name in
//! that object belongs to Claude Code, which is why they now live here beside
//! the transcript tap that reads the same vocabulary. Capability row:
//! `claude.statusline.stdin` (Tier C, silent degradation — canary-covered).
//!
//! Side channel: the same stdin JSON carries `rate_limits` (the account's
//! 5h/7d subscription quota, Claude Code ≥ 2.1.80) and — NC-3 — the
//! `context_window` block with the turn's cache read/creation split plus the
//! session metadata beside it. Each invocation extracts both and persists them
//! in one payload via `crate::usage::store_pushed_usage` for the bottom-bar
//! usage widget — that push is the widget's only data source (see
//! `crate::usage` for the why and the file contract). Extraction happens only
//! after the bar has been written, and every field is optional, so upstream
//! shape drift can cost data but never the status line.

use serde::Deserialize;

/// Subset of Claude Code's status line stdin JSON that we consume. Lenient
/// by construction: every field defaults, unknown keys are ignored, and a
/// parse failure yields `Input::default()` (a usable, if bare, line).
#[derive(Deserialize, Default)]
pub(crate) struct Input {
    #[serde(default)]
    pub(crate) model: Model,
    #[serde(default)]
    pub(crate) context_window: ContextWindow,
}

#[derive(Deserialize, Default)]
pub(crate) struct Model {
    #[serde(default)]
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) id: String,
}

#[derive(Deserialize, Default)]
pub(crate) struct ContextWindow {
    /// Pre-computed percentage of the context window in use (0–100).
    /// Claude Code derives it from input + cache tokens over the window
    /// size, so we render it directly rather than recomputing.
    #[serde(default)]
    pub(crate) used_percentage: f64,
    /// Tokens currently occupying the window (input + cache). Matches the
    /// numerator behind `used_percentage`; shown as the left "(used/size)".
    #[serde(default)]
    pub(crate) total_input_tokens: u64,
    /// Maximum context size in tokens (200k, or 1M with extended context).
    #[serde(default)]
    pub(crate) context_window_size: u64,
}

/// Build the widget push from the status-line payload: the subscription quota
/// (`rate_limits`) plus the live context-window reading (NC-3). `None` when
/// the payload carries neither — nothing worth writing.
///
/// The whole extraction is walked as raw `Value` — deliberately *not* part of
/// [`Input`] — and every field is optional, so a reshaped or partial payload
/// costs fields rather than failing the parse and taking the bar down with it.
/// It also runs strictly after the bar has been written to stdout.
pub(crate) fn extract_push(input: &str) -> Option<crate::usage::UsageSnapshot> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let (five_hour, seven_day) = extract_rate_limits(&v);
    let context = extract_context(&v);
    let snapshot = crate::usage::UsageSnapshot {
        five_hour,
        seven_day,
        context,
    };
    snapshot.is_substantive().then_some(snapshot)
}

/// Everything the push needs about *which* session produced this payload
/// (M14). Never rendered — it decides which tab owns the shared context slot
/// (see `crate::usage::merge_push`), so it deliberately stays out of
/// `ContextSnapshot` and out of the Rust↔TS contract.
///
/// What the status-line payload offers, in preference order:
///   * `session_id` — Claude Code's per-session UUID, the stable key.
///   * `transcript_path` — one file per session; a fine substitute.
///   * `session_name` — human-set, optional, not guaranteed unique, but
///     better than nothing.
///
/// As an *activity* discriminator it also takes the `cost` block's
/// `total_api_duration_ms` / `total_cost_usd`, which move only when the
/// session actually calls the API. (`total_duration_ms` is deliberately not
/// used: wall-clock keeps ticking while a session sits idle, which would make
/// every idle beat look like work.) `None` for anything the payload omits —
/// the merge degrades to last-writer-wins rather than misattributing.
pub(crate) fn extract_push_meta(input: &str) -> crate::usage::PushMeta {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return crate::usage::PushMeta::default();
    };
    let session_key = non_empty_string(v.get("session_id"))
        .or_else(|| non_empty_string(v.get("transcript_path")))
        .or_else(|| non_empty_string(v.get("session_name")));
    let cost = v.get("cost");
    let activity = cost.and_then(|c| {
        let api_ms = num_f64(c, "total_api_duration_ms");
        let usd = num_f64(c, "total_cost_usd");
        (api_ms.is_some() || usd.is_some()).then(|| {
            format!(
                "{}/{}",
                api_ms.map(|n| n.to_string()).unwrap_or_default(),
                usd.map(|n| n.to_string()).unwrap_or_default(),
            )
        })
    });
    crate::usage::PushMeta {
        session_key,
        activity,
    }
}

/// Pull the subscription quota out of the payload's `rate_limits` object
/// (documented shape: `used_percentage` 0–100, `resets_at` Unix epoch
/// seconds; either window independently absent).
pub(crate) fn extract_rate_limits(
    v: &serde_json::Value,
) -> (
    Option<crate::usage::UsageWindow>,
    Option<crate::usage::UsageWindow>,
) {
    let Some(rl) = v.get("rate_limits") else {
        return (None, None);
    };
    let window = |key: &str| -> Option<crate::usage::UsageWindow> {
        let w = rl.get(key)?;
        let utilization = w.get("used_percentage")?.as_f64()?;
        // Docs say epoch seconds; accept an ISO string too in case the
        // upstream field ever changes representation.
        let resets_at = w.get("resets_at").and_then(|r| {
            r.as_str()
                .map(str::to_string)
                .or_else(|| r.as_i64().and_then(crate::usage::epoch_secs_to_iso))
        });
        Some(crate::usage::UsageWindow {
            utilization,
            resets_at,
        })
    };
    (window("five_hour"), window("seven_day"))
}

/// Pull the live context reading out of the payload's `context_window` block
/// (plus the session metadata beside it) for the GUI context bar.
///
/// Documented shape:
/// ```json
/// { "session_name": "…", "agent": { "name": "…" },
///   "effort": "high", "thinking": true, "fast_mode": false,
///   "context_window": {
///     "used_percentage": 12.5, "remaining_percentage": 87.5,
///     "total_input_tokens": 25004, "context_window_size": 200000,
///     "current_usage": { "input_tokens": 4, "output_tokens": 1,
///       "cache_read_input_tokens": 20000,
///       "cache_creation_input_tokens": 5000 } } }
/// ```
/// Missing pieces stay `None` (the UI renders "unknown", never 0). `None`
/// overall only when there is no metadata *and* no `context_window` object.
pub(crate) fn extract_context(v: &serde_json::Value) -> Option<crate::usage::ContextSnapshot> {
    let cw = v.get("context_window");
    // `current_usage` holds the cache split; tolerate it having been hoisted
    // to the `context_window` level, which costs one `or` and survives that
    // particular reshape.
    let usage = cw.and_then(|c| c.get("current_usage")).or(cw);

    let mut ctx = crate::usage::ContextSnapshot {
        used_percentage: cw.and_then(|c| num_f64(c, "used_percentage")),
        remaining_percentage: cw.and_then(|c| num_f64(c, "remaining_percentage")),
        total_input_tokens: cw.and_then(|c| num_u64(c, "total_input_tokens")),
        context_window_size: cw.and_then(|c| num_u64(c, "context_window_size")),
        cache_read_tokens: usage
            .and_then(|u| first_num_u64(u, &["cache_read_input_tokens", "cache_read_tokens"])),
        cache_creation_tokens: usage.and_then(|u| {
            first_num_u64(u, &["cache_creation_input_tokens", "cache_creation_tokens"])
        }),
        input_tokens: usage.and_then(|u| num_u64(u, "input_tokens")),
        output_tokens: usage.and_then(|u| num_u64(u, "output_tokens")),
        session_name: non_empty_string(v.get("session_name")),
        agent_name: v
            .get("agent")
            .and_then(|a| non_empty_string(a.get("name")).or_else(|| non_empty_string(Some(a)))),
        effort: scalar_string(v.get("effort")),
        thinking: scalar_string(v.get("thinking")),
        fast_mode: v.get("fast_mode").and_then(|f| f.as_bool()),
    };
    // `total_input_tokens` also lives at the top level in some payloads — but
    // only consult it when there is no `context_window` block at all. It is
    // the numerator of the "used / window size" pair the UI draws, and the
    // denominator is block-only: pairing a top-level number (whose semantics
    // could drift independently) with a block-level window size would silently
    // mix two populations. With no block there is no denominator to mix with,
    // so the lone figure is safe (it renders as "25k/?").
    if ctx.total_input_tokens.is_none() && cw.is_none() {
        ctx.total_input_tokens = num_u64(v, "total_input_tokens");
    }
    let has_metadata = ctx.session_name.is_some()
        || ctx.agent_name.is_some()
        || ctx.effort.is_some()
        || ctx.thinking.is_some()
        || ctx.fast_mode.is_some();
    (ctx.is_substantive() || has_metadata).then_some(ctx)
}

/// `obj[key]` as f64 (accepts integers too). `None` for missing/non-numeric.
fn num_f64(obj: &serde_json::Value, key: &str) -> Option<f64> {
    obj.get(key)?.as_f64().filter(|f| f.is_finite())
}

/// `obj[key]` as u64. Floats are accepted and rounded (a token count sent as
/// `25004.0` is still a token count); negatives and non-finite values are not.
fn num_u64(obj: &serde_json::Value, key: &str) -> Option<u64> {
    let v = obj.get(key)?;
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f.round() as u64)
    })
}

/// First of `keys` present as a number — upstream has used both the
/// `*_input_tokens` and the shorter `*_tokens` spelling for the cache split.
fn first_num_u64(obj: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| num_u64(obj, k))
}

/// A non-empty string value, or `None` (empty strings are absence).
fn non_empty_string(v: Option<&serde_json::Value>) -> Option<String> {
    let s = v?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Stringify a scalar leniently: strings pass through, booleans become
/// `"on"`/`"off"`, numbers their decimal form. Used for `effort` / `thinking`,
/// which upstream has expressed as both flags and levels — storing the string
/// keeps the display honest under either.
fn scalar_string(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        serde_json::Value::Bool(b) => Some(if *b { "on".into() } else { "off".into() }),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rate_limits_with_epoch_reset() {
        // The documented payload shape: percentages + epoch-seconds resets.
        let json = r#"{"model":{"display_name":"Opus"},
            "rate_limits":{
                "five_hour":{"used_percentage":23.5,"resets_at":1738425600},
                "seven_day":{"used_percentage":41.2,"resets_at":1738857600}}}"#;
        let snap = extract_push(json).expect("snapshot extracted");
        let five = snap.five_hour.expect("five_hour window");
        assert_eq!(five.utilization, 23.5);
        assert_eq!(five.resets_at.as_deref(), Some("2025-02-01T16:00:00+00:00"));
        assert_eq!(snap.seven_day.expect("seven_day window").utilization, 41.2);
    }

    #[test]
    fn extracts_partial_and_stringly_rate_limits() {
        // One window absent, the other with an ISO-string reset (future-proof
        // leniency) — extraction still yields the present window.
        let json = r#"{"rate_limits":{
            "seven_day":{"used_percentage":9.0,"resets_at":"2026-08-05T12:00:00+02:00"}}}"#;
        let snap = extract_push(json).expect("snapshot extracted");
        assert!(snap.five_hour.is_none());
        let seven = snap.seven_day.expect("seven_day window");
        assert_eq!(seven.utilization, 9.0);
        assert_eq!(
            seven.resets_at.as_deref(),
            Some("2026-08-05T12:00:00+02:00")
        );
    }

    #[test]
    fn no_push_without_usable_data() {
        // Absent object, empty object, and malformed windows all yield None
        // (nothing is written over a previous good push) — as long as no
        // context data rides along either.
        assert!(extract_push(r#"{"model":{"display_name":"Opus"}}"#).is_none());
        assert!(extract_push(r#"{"rate_limits":{}}"#).is_none());
        assert!(extract_push(
            r#"{"rate_limits":{"five_hour":{"used_percentage":"not-a-number"}}}"#
        )
        .is_none());
        assert!(extract_push("not json").is_none());
        // Metadata with no numbers anywhere is not worth a push.
        assert!(extract_push(r#"{"session_name":"refactor","effort":"high"}"#).is_none());
    }

    #[test]
    fn extracts_context_window_and_cache_split() {
        // NC-3: the documented context payload rides the same push.
        let json = r#"{
            "model":{"display_name":"Opus"},
            "session_name":"refactor the parser",
            "agent":{"name":"reviewer"},
            "effort":"high","thinking":true,"fast_mode":false,
            "context_window":{
                "used_percentage":12.5,"remaining_percentage":87.5,
                "total_input_tokens":25004,"context_window_size":200000,
                "current_usage":{"input_tokens":4,"output_tokens":1,
                    "cache_read_input_tokens":20000,
                    "cache_creation_input_tokens":5000}}}"#;
        let snap = extract_push(json).expect("snapshot extracted");
        assert!(snap.five_hour.is_none() && snap.seven_day.is_none());
        let ctx = snap.context.expect("context block");
        assert_eq!(ctx.used_percentage, Some(12.5));
        assert_eq!(ctx.remaining_percentage, Some(87.5));
        assert_eq!(ctx.total_input_tokens, Some(25_004));
        assert_eq!(ctx.context_window_size, Some(200_000));
        assert_eq!(ctx.cache_read_tokens, Some(20_000));
        assert_eq!(ctx.cache_creation_tokens, Some(5_000));
        assert_eq!(ctx.input_tokens, Some(4));
        assert_eq!(ctx.output_tokens, Some(1));
        assert_eq!(ctx.session_name.as_deref(), Some("refactor the parser"));
        assert_eq!(ctx.agent_name.as_deref(), Some("reviewer"));
        assert_eq!(ctx.effort.as_deref(), Some("high"));
        assert_eq!(ctx.thinking.as_deref(), Some("on"));
        assert_eq!(ctx.fast_mode, Some(false));
    }

    #[test]
    fn context_and_rate_limits_ride_the_same_push() {
        let json = r#"{"rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":null}},
            "context_window":{"used_percentage":50.0,"context_window_size":200000}}"#;
        let snap = extract_push(json).expect("snapshot extracted");
        assert_eq!(snap.five_hour.expect("five_hour").utilization, 23.5);
        assert_eq!(
            snap.context.expect("context").context_window_size,
            Some(200_000)
        );
    }

    #[test]
    fn missing_context_fields_stay_none_not_zero() {
        // A partial block must not fabricate zeros — the UI has to be able to
        // tell "0 tokens" from "not reported".
        let json = r#"{"context_window":{"used_percentage":30.0}}"#;
        let ctx = extract_push(json)
            .expect("snapshot extracted")
            .context
            .expect("context block");
        assert_eq!(ctx.used_percentage, Some(30.0));
        assert!(ctx.total_input_tokens.is_none());
        assert!(ctx.context_window_size.is_none());
        assert!(ctx.cache_read_tokens.is_none());
        assert!(ctx.cache_creation_tokens.is_none());
        assert!(ctx.session_name.is_none());
        assert!(ctx.fast_mode.is_none());
    }

    #[test]
    fn reshaped_context_block_degrades_field_by_field() {
        // Wrong types, a hoisted cache split, the alternate cache spelling and
        // an empty session name: whatever still parses is kept, the rest is
        // simply absent — never a failed extraction.
        let json = r#"{
            "session_name":"   ",
            "context_window":{
                "used_percentage":"lots","total_input_tokens":25004.0,
                "cache_read_tokens":7,"cache_creation_tokens":9},
            "fast_mode":"yes"}"#;
        let ctx = extract_push(json)
            .expect("snapshot extracted")
            .context
            .expect("context block");
        assert!(ctx.used_percentage.is_none());
        assert_eq!(ctx.total_input_tokens, Some(25_004));
        assert_eq!(ctx.cache_read_tokens, Some(7));
        assert_eq!(ctx.cache_creation_tokens, Some(9));
        assert!(ctx.session_name.is_none());
        // A non-boolean fast_mode is dropped rather than coerced.
        assert!(ctx.fast_mode.is_none());
    }

    #[test]
    fn context_token_numerator_never_mixes_sources() {
        // A `context_window` block that lost its `total_input_tokens` must NOT
        // borrow the top-level one: the window size (denominator) comes from
        // the block, so a top-level numerator would mix populations.
        let json = r#"{"total_input_tokens":999999,
            "context_window":{"used_percentage":30.0,"context_window_size":200000}}"#;
        let ctx = extract_push(json)
            .expect("snapshot extracted")
            .context
            .expect("context block");
        assert!(ctx.total_input_tokens.is_none());
        assert_eq!(ctx.context_window_size, Some(200_000));

        // With no block at all there is no denominator to mix with, so the
        // lone top-level figure is still worth showing.
        let json = r#"{"total_input_tokens":25004}"#;
        let ctx = extract_push(json)
            .expect("snapshot extracted")
            .context
            .expect("context block");
        assert_eq!(ctx.total_input_tokens, Some(25_004));
        assert!(ctx.context_window_size.is_none());
    }

    #[test]
    fn push_meta_prefers_session_id_and_api_activity() {
        let json = r#"{"session_id":"abc-123","transcript_path":"C:/t/abc.jsonl",
            "session_name":"refactor",
            "cost":{"total_cost_usd":0.42,"total_duration_ms":900000,
                    "total_api_duration_ms":12000},
            "context_window":{"used_percentage":12.5}}"#;
        let meta = extract_push_meta(json);
        assert_eq!(meta.session_key.as_deref(), Some("abc-123"));
        let activity = meta.activity.expect("activity counters");
        assert!(activity.contains("12000"), "got: {activity}");
        assert!(activity.contains("0.42"), "got: {activity}");
        // Wall-clock session duration must not leak in: it moves while idle.
        assert!(!activity.contains("900000"), "got: {activity}");
    }

    #[test]
    fn push_meta_degrades_field_by_field() {
        // No session id → transcript path → session name → nothing at all.
        assert_eq!(
            extract_push_meta(r#"{"transcript_path":"C:/t/a.jsonl","session_name":"x"}"#)
                .session_key
                .as_deref(),
            Some("C:/t/a.jsonl")
        );
        assert_eq!(
            extract_push_meta(r#"{"session_id":"  ","session_name":"x"}"#)
                .session_key
                .as_deref(),
            Some("x")
        );
        let bare = extract_push_meta(r#"{"context_window":{"used_percentage":1.0}}"#);
        assert!(bare.session_key.is_none() && bare.activity.is_none());
        // A cost block with nothing numeric in it is absence, not "0/0".
        assert!(extract_push_meta(r#"{"cost":{"total_lines_added":3}}"#)
            .activity
            .is_none());
        assert!(extract_push_meta("not json").session_key.is_none());
    }

    #[test]
    fn rate_limits_missing_reset_is_tolerated() {
        // Windows at 0% can omit/null the reset time; the window still pushes.
        let json = r#"{"rate_limits":{"five_hour":{"used_percentage":0.0,"resets_at":null}}}"#;
        let snap = extract_push(json).expect("snapshot extracted");
        let five = snap.five_hour.expect("five_hour window");
        assert_eq!(five.utilization, 0.0);
        assert!(five.resets_at.is_none());
    }
}
