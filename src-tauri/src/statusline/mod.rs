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

    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
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
    Some(std::ffi::OsString::from_wide(&buf).to_string_lossy().into_owned())
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
fn render(input: &str) -> String {
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
            model: (57, 197, 207),   // cyan
            filled: (63, 185, 80),   // green
            empty: (110, 118, 129),  // brightBlack
            text: (201, 209, 217),   // foreground
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
        "GitHub Dark".to_string()
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
        assert_eq!(parse_hex(Some(&"#3fb95080".to_string())), Some((63, 185, 80)));
        assert_eq!(parse_hex(Some(&"notacolor".to_string())), None);
        assert_eq!(parse_hex(None), None);
    }

    #[test]
    fn shell_safe_path_uses_forward_slashes() {
        let p = Path::new(r"C:\Users\me\cimp\cimp.exe");
        assert_eq!(shell_safe_path(p), "C:/Users/me/cimp/cimp.exe");
    }
}
