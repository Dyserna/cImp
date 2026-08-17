//! Context-window status line for cImp-launched Claude Code sessions.
//!
//! Two halves live here:
//!
//!   * [`launch_command`] builds the string we hand to Claude Code's
//!     `statusLine.command` (via a `--settings` pre-arg in
//!     `tabs::config`). It points at *this very binary* re-invoked with
//!     `--statusline`, so there is no external script to ship and no
//!     dependency on node/python/PowerShell — the bar renders in Rust.
//!   * [`run`] is that re-invocation: Claude Code runs `cimp
//!     --statusline`, pipes the session JSON to our stdin, and reads the
//!     rendered bar from our stdout. `main()` dispatches to it before any
//!     Tauri/audio/settings init so it is instant and never spins up the
//!     GUI.
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
//!
//! Scope: the `--settings` overlay *merges* (CLI flags sit above the
//! user's `settings.json` in Claude Code's precedence and only the keys
//! we set are overridden), so this affects cImp-launched Claude tabs
//! only — the user's global `~/.claude/settings.json` is never written.
//! That keeps cImp's "writes only next to the exe" portability rule
//! intact.
//!
//! Windows shell note: Claude Code runs the status line command through
//! Git Bash when present, else PowerShell. Git Bash silently eats
//! unquoted backslashes, so [`launch_command`] emits the exe path with
//! forward slashes; an *unquoted, space-free* forward-slash path is the
//! one form that executes in both shells. Paths containing a space are
//! collapsed to their Windows 8.3 short form (no spaces) so the unquoted
//! form still holds; if 8.3 generation is disabled we fall back to
//! double-quoting (Git Bash-correct).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;

use serde::Deserialize;

// ---- launch-command side (parent process) --------------------------------

/// The `statusLine.command` string for an injected `--settings` overlay,
/// i.e. `<this-exe> --statusline` with the path made shell-safe. `None`
/// when `current_exe()` can't be resolved (a sandbox / odd platform), in
/// which case the caller skips the injection and Claude Code keeps its
/// default status line.
pub fn launch_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("{} --statusline", shell_safe_path(&exe)))
}

/// V11: the `command` string for a Claude hook shim re-invoking this binary with
/// `flag`, shell-safe like [`launch_command`]. `None` when `current_exe()` can't
/// be resolved.
///
/// **Two callers left after V35 Phase J** — `--taint-beacon` and
/// `--checkpoint-beacon`, the only Claude hooks that are still separate
/// binaries. The five that used to use this (and `context_hook_command`, deleted
/// with them) are now `type: "http"` entries pointing at the loopback, so they
/// need no command string at all.
pub fn hook_command(flag: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("{} {flag}", shell_safe_path(&exe)))
}

/// Render an executable path so it survives whichever shell Claude Code
/// uses to run the status line command. Forward slashes always (Git Bash
/// strips unquoted backslashes); space-free unquoted when possible so the
/// same string works in PowerShell too; quoted only as a last resort.
fn shell_safe_path(exe: &Path) -> String {
    let fwd = exe.to_string_lossy().replace('\\', "/");
    if !fwd.contains(' ') {
        return fwd;
    }
    // Spaces break the unquoted form in at least one shell. On Windows the
    // 8.3 short path has no spaces and runs unquoted everywhere.
    #[cfg(windows)]
    if let Some(short) = short_path(exe) {
        let short_fwd = short.replace('\\', "/");
        if !short_fwd.contains(' ') {
            return short_fwd;
        }
    }
    // 8.3 unavailable (disabled on the volume) or non-Windows: quote it.
    // Git Bash executes a double-quoted path correctly; a PowerShell
    // fallback may not, but that pairing is rare and only costs the bar.
    format!("\"{fwd}\"")
}

/// Windows 8.3 short path via `GetShortPathNameW`. Declared inline so we
/// don't pull in a Win32 crate for one call — `kernel32` is linked by
/// default. Returns `None` if the call fails or the file lacks a short
/// name (8.3 generation disabled), in which case the long path stands.
#[cfg(windows)]
fn short_path(p: &Path) -> Option<String> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    extern "system" {
        fn GetShortPathNameW(
            lpsz_long_path: *const u16,
            lpsz_short_path: *mut u16,
            cch_buffer: u32,
        ) -> u32;
    }

    let wide: Vec<u16> = p
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // First call with a null buffer returns the required length (incl. NUL).
    let needed = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u16; needed as usize];
    let written = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), needed) };
    if written == 0 || written >= needed {
        return None;
    }
    buf.truncate(written as usize);
    Some(
        std::ffi::OsString::from_wide(&buf)
            .to_string_lossy()
            .into_owned(),
    )
}

// ---- render side (`cimp --statusline` child process) --------------------

/// Entry point for the `--statusline` subcommand. Reads Claude Code's
/// session JSON from stdin and writes one rendered status line to stdout.
/// All failures degrade quietly to a minimal line — a status line command
/// that errors or hangs would garble Claude Code's UI, so we never panic
/// and never block.
pub fn run() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let line = render(&input);
    let mut out = std::io::stdout();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
    // Bar first, push second: Claude Code is waiting on our stdout, the
    // usage widget can wait a few ms. A payload with neither `rate_limits`
    // nor context numbers skips the write so a good push is never clobbered;
    // `store_pushed_usage` merges what we do have over the file's other slot,
    // so no tab's push can evict another tab's still-valid data (M14).
    if let Some(snapshot) = extract_push(&input) {
        crate::usage::store_pushed_usage(&snapshot, &extract_push_meta(&input));
    }
}

/// Subset of Claude Code's status line stdin JSON that we consume. Lenient
/// by construction: every field defaults, unknown keys are ignored, and a
/// parse failure yields `Input::default()` (a usable, if bare, line).
#[derive(Deserialize, Default)]
struct Input {
    #[serde(default)]
    model: Model,
    #[serde(default)]
    context_window: ContextWindow,
}

#[derive(Deserialize, Default)]
struct Model {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    id: String,
}

#[derive(Deserialize, Default)]
struct ContextWindow {
    /// Pre-computed percentage of the context window in use (0–100).
    /// Claude Code derives it from input + cache tokens over the window
    /// size, so we render it directly rather than recomputing.
    #[serde(default)]
    used_percentage: f64,
    /// Tokens currently occupying the window (input + cache). Matches the
    /// numerator behind `used_percentage`; shown as the left "(used/size)".
    #[serde(default)]
    total_input_tokens: u64,
    /// Maximum context size in tokens (200k, or 1M with extended context).
    #[serde(default)]
    context_window_size: u64,
}

/// Number of cells in the bar.
const BAR_CELLS: usize = 10;

/// Build the status line string from raw stdin JSON. Pure (no IO) so it is
/// unit-testable; `run` handles the stdin/stdout edges.
/// `pub(crate)` for the V35 canary suite (harness/canary.rs).
pub(crate) fn render(input: &str) -> String {
    let data: Input = serde_json::from_str(input).unwrap_or_default();
    let pct = data.context_window.used_percentage.clamp(0.0, 100.0);
    let size = data.context_window.context_window_size;
    let used = data.context_window.total_input_tokens;

    let model = if !data.model.display_name.is_empty() {
        data.model.display_name.as_str()
    } else if !data.model.id.is_empty() {
        data.model.id.as_str()
    } else {
        "Claude"
    };

    let palette = Palette::load();

    let filled = ((pct / 100.0 * BAR_CELLS as f64).round() as usize).min(BAR_CELLS);
    let bar_filled: String = "▓".repeat(filled);
    let bar_empty: String = "░".repeat(BAR_CELLS - filled);
    let bar = format!(
        "{}{}",
        palette.paint(Slot::Filled, &bar_filled),
        palette.paint(Slot::Empty, &bar_empty),
    );

    let pct_str = format!("{}%", pct.round() as u64);

    // Token count only when the window size is known — without it the
    // "(used/size)" pair would be misleading (e.g. "(0/0)").
    let tokens = if size > 0 {
        format!(" ({}/{})", humanize(used), humanize(size))
    } else {
        String::new()
    };

    format!(
        "{}  {} {}{}",
        palette.paint(Slot::Model, model),
        bar,
        palette.paint(Slot::Text, &pct_str),
        palette.paint(Slot::Text, &tokens),
    )
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
fn extract_push_meta(input: &str) -> crate::usage::PushMeta {
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

/// Compact a token count for display: `940`, `12k`, `200k`, `1.0M`.
fn humanize(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 999_500 {
        // Upper bound is 999_500, not 1_000_000: above it the rounded
        // thousands hit 1000 and would render the nonsensical "1000k".
        format!("{}k", ((n as f64) / 1_000.0).round() as u64)
    } else {
        format!("{:.1}M", (n as f64) / 1_000_000.0)
    }
}

// ---- palette-aware coloring ----------------------------------------------

/// The four roles we color, each mapped to a terminal palette slot.
#[derive(Clone, Copy)]
enum Slot {
    /// Model name.
    Model,
    /// Filled portion of the bar.
    Filled,
    /// Empty portion of the bar.
    Empty,
    /// Percentage and token text.
    Text,
}

/// RGB colors resolved from the user's active terminal palette, with
/// built-in GitHub-Dark-ish fallbacks so the bar is always colored even
/// when no palette file can be read.
struct Palette {
    model: (u8, u8, u8),
    filled: (u8, u8, u8),
    empty: (u8, u8, u8),
    text: (u8, u8, u8),
}

impl Palette {
    /// Resolve colors from the active palette named in the portable
    /// `settings.json` next to the exe (`terminal.theme.name`, or its
    /// inline `custom` map when set to `"Custom"`). Any failure along the
    /// way leaves the corresponding fallback in place.
    fn load() -> Self {
        let mut p = Palette {
            model: (57, 197, 207),  // cyan
            filled: (63, 185, 80),  // green
            empty: (110, 118, 129), // brightBlack
            text: (201, 209, 217),  // foreground
        };
        let colors = active_palette_colors();
        if let Some(c) = parse_hex(colors.get("cyan")) {
            p.model = c;
        }
        if let Some(c) = parse_hex(colors.get("green")) {
            p.filled = c;
        }
        if let Some(c) = parse_hex(colors.get("brightBlack")) {
            p.empty = c;
        }
        if let Some(c) = parse_hex(colors.get("foreground")) {
            p.text = c;
        }
        p
    }

    /// Wrap `s` in a truecolor SGR sequence for the given slot. Empty
    /// strings pass through uncolored so we don't emit dangling resets.
    fn paint(&self, slot: Slot, s: &str) -> String {
        if s.is_empty() {
            return String::new();
        }
        let (r, g, b) = match slot {
            Slot::Model => self.model,
            Slot::Filled => self.filled,
            Slot::Empty => self.empty,
            Slot::Text => self.text,
        };
        format!("\x1b[38;2;{r};{g};{b}m{s}\x1b[0m")
    }
}

/// Read the active terminal palette's color map from the portable
/// `settings.json` next to the exe. Returns an empty map on any failure;
/// the caller falls back per-color.
fn active_palette_colors() -> HashMap<String, String> {
    let Some(dir) = exe_dir() else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(dir.join("settings.json")) else {
        return HashMap::new();
    };
    let Ok(probe) = serde_json::from_str::<SettingsProbe>(&text) else {
        return HashMap::new();
    };
    let theme = probe.terminal.theme;
    // "Custom" carries the 22-slot map inline; a named palette lives in
    // <exe-dir>/palettes/<name>.json.
    if theme.name == "Custom" {
        if let Some(custom) = theme.custom {
            return custom;
        }
    }
    let name = if theme.name.is_empty() {
        // The default terminal palette (paired with the default built-in tui UI
        // theme) — same value TerminalThemeSettings::default() writes.
        "OpenCode Grey".to_string()
    } else {
        theme.name
    };
    read_palette_file(&dir, &name).unwrap_or_default()
}

/// Read `<dir>/palettes/<name>.json` and return its `colors` map.
fn read_palette_file(dir: &Path, name: &str) -> Option<HashMap<String, String>> {
    // `name` comes from settings.json; refuse any path component so a crafted
    // name (`../../secret`) can't traverse out of the `palettes/` directory.
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return None;
    }
    let path = dir.join("palettes").join(format!("{name}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    let file: PaletteFileProbe = serde_json::from_str(&text).ok()?;
    Some(file.colors)
}

/// `<exe-dir>` — where the portable `settings.json` and `palettes/` live.
fn exe_dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent().map(Path::to_path_buf)
}

/// Parse a `#rrggbb` / `#rgb` hex string into an RGB triple. `None` for
/// anything else (including the `#rrggbbaa` alpha form, which we ignore
/// for terminal SGR — alpha has no meaning here, so we read the first six
/// digits when present).
fn parse_hex(s: Option<&String>) -> Option<(u8, u8, u8)> {
    let s = s?.strip_prefix('#')?;
    let expand = |c: &str| u8::from_str_radix(c, 16).ok();
    match s.len() {
        3 | 4 => {
            // #rgb / #rgba → each nibble doubled; alpha (4th) ignored, as the
            // theming layer accepts 4-digit hex and we'd otherwise silently
            // fall back to defaults for it.
            let r = expand(&s[0..1])?;
            let g = expand(&s[1..2])?;
            let b = expand(&s[2..3])?;
            Some((r * 17, g * 17, b * 17))
        }
        6 | 8 => {
            let r = expand(&s[0..2])?;
            let g = expand(&s[2..4])?;
            let b = expand(&s[4..6])?;
            Some((r, g, b))
        }
        _ => None,
    }
}

/// Lenient probe over `settings.json` for just the active palette.
#[derive(Deserialize, Default)]
struct SettingsProbe {
    #[serde(default)]
    terminal: TerminalProbe,
}

#[derive(Deserialize, Default)]
struct TerminalProbe {
    #[serde(default)]
    theme: ThemeProbe,
}

#[derive(Deserialize, Default)]
struct ThemeProbe {
    #[serde(default)]
    name: String,
    #[serde(default)]
    custom: Option<HashMap<String, String>>,
}

/// Lenient probe over a `palettes/<name>.json` file.
#[derive(Deserialize)]
struct PaletteFileProbe {
    colors: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR sequences so assertions can target the visible text.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip until the SGR terminator 'm'.
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn renders_bar_tokens_and_percent() {
        let json = r#"{
            "model": { "display_name": "Opus", "id": "claude-opus-4-8" },
            "context_window": {
                "used_percentage": 50.0,
                "total_input_tokens": 100000,
                "context_window_size": 200000
            }
        }"#;
        let line = strip_ansi(&render(json));
        assert_eq!(line, "Opus  ▓▓▓▓▓░░░░░ 50% (100k/200k)");
    }

    #[test]
    fn full_bar_at_100_percent() {
        let json = r#"{"model":{"display_name":"Sonnet"},
            "context_window":{"used_percentage":100.0,"total_input_tokens":200000,"context_window_size":200000}}"#;
        let line = strip_ansi(&render(json));
        assert_eq!(line, "Sonnet  ▓▓▓▓▓▓▓▓▓▓ 100% (200k/200k)");
    }

    #[test]
    fn empty_bar_at_zero() {
        let json = r#"{"model":{"display_name":"Haiku"},
            "context_window":{"used_percentage":0.0,"total_input_tokens":0,"context_window_size":200000}}"#;
        let line = strip_ansi(&render(json));
        assert_eq!(line, "Haiku  ░░░░░░░░░░ 0% (0/200k)");
    }

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

    #[test]
    fn falls_back_to_id_then_generic_model_name() {
        let by_id = r#"{"model":{"id":"claude-x"},"context_window":{"context_window_size":0}}"#;
        assert!(strip_ansi(&render(by_id)).starts_with("claude-x  "));

        let none = r#"{"context_window":{"context_window_size":0}}"#;
        assert!(strip_ansi(&render(none)).starts_with("Claude  "));
    }

    #[test]
    fn omits_tokens_when_window_size_unknown() {
        // No context_window_size → no "(used/size)" suffix.
        let json = r#"{"model":{"display_name":"Opus"},"context_window":{"used_percentage":30.0}}"#;
        let line = strip_ansi(&render(json));
        assert_eq!(line, "Opus  ▓▓▓░░░░░░░ 30%");
    }

    #[test]
    fn garbage_input_degrades_to_bare_line() {
        let line = strip_ansi(&render("not json"));
        // Default model name, zero bar, no token suffix (size 0).
        assert_eq!(line, "Claude  ░░░░░░░░░░ 0%");
    }

    #[test]
    fn percentage_rounds_to_nearest_cell() {
        let json = r#"{"model":{"display_name":"M"},
            "context_window":{"used_percentage":82.0,"total_input_tokens":164000,"context_window_size":200000}}"#;
        let line = strip_ansi(&render(json));
        assert_eq!(line, "M  ▓▓▓▓▓▓▓▓░░ 82% (164k/200k)");
    }

    #[test]
    fn humanize_buckets() {
        assert_eq!(humanize(0), "0");
        assert_eq!(humanize(940), "940");
        assert_eq!(humanize(12_345), "12k");
        assert_eq!(humanize(200_000), "200k");
        assert_eq!(humanize(1_000_000), "1.0M");
    }

    #[test]
    fn parse_hex_forms() {
        assert_eq!(parse_hex(Some(&"#fff".to_string())), Some((255, 255, 255)));
        assert_eq!(parse_hex(Some(&"#3fb950".to_string())), Some((63, 185, 80)));
        // Alpha form: read the RGB, ignore alpha.
        assert_eq!(
            parse_hex(Some(&"#3fb95080".to_string())),
            Some((63, 185, 80))
        );
        assert_eq!(parse_hex(Some(&"notacolor".to_string())), None);
        assert_eq!(parse_hex(None), None);
    }

    #[test]
    fn shell_safe_path_uses_forward_slashes() {
        let p = Path::new(r"C:\Users\me\cimp\cimp.exe");
        assert_eq!(shell_safe_path(p), "C:/Users/me/cimp/cimp.exe");
    }
}
