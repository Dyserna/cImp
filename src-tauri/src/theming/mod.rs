//! Externalized UI themes and terminal palettes.
//!
//! Two user-editable folders sit next to the executable, in the same dir as
//! `settings.json` / `patterns.json`:
//!
//!   * `<exe-dir>/themes/<id>/` — one folder per UI chrome theme, each with
//!     `theme.json` (metadata: id, display name, native-vs-custom titlebar,
//!     default paired palette) and `theme.css` (the `[data-theme="<id>"]`
//!     token block plus any structural overrides).
//!   * `<exe-dir>/palettes/<name>.json` — one file per terminal color palette
//!     (the 22 xterm.js `ITheme` slots).
//!
//! These ship as real files in the portable release (staged next to the exe by
//! `.github/workflows/release.yml`); the source of truth is the repo-root
//! `themes/` and `palettes/` folders, which `build.rs` also copies next to the
//! built exe so dev / local builds find them. Nothing is *seeded* at runtime:
//! the app simply discovers whatever is on disk. Every file is **verified**
//! before it joins the registry, and anything that fails verification is
//! skipped with a `tracing::warn!` (it never appears in Settings).
//!
//! The `tui` theme is different: it is *built in*, not just a fallback. Its
//! CSS is compiled into the binary (`tui_theme.css`, `include_str!`) and its
//! metadata lives in code, so it is always present and always wins over any
//! on-disk folder that tries to claim the `tui` id. Its accent color is not
//! baked into the CSS — the frontend injects the user-picked `ui.tui_accent`
//! setting as the `--tui-accent` CSS variable and the theme derives the whole
//! accent family from it. The on-disk `themes/` folder remains the extension
//! point for every other theme (nippon-dark/-light ship there today; retired
//! or future themes can be dropped in).
//!
//! The embedded palette (`OpenCode Grey`) is a last-resort fallback: even if
//! the palettes folder is missing/empty or every file on disk is malformed,
//! it is always present and the app stays usable. It is also the default new
//! installs land on. A valid on-disk palette of the same name overrides the
//! embedded copy, so the embed is invisible whenever the file is present
//! (the normal case).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The 22 xterm.js `ITheme` color slots every palette must populate. Mirrors
/// `REQUIRED_THEME_KEYS` in `src/lib/themes/index.ts`.
const REQUIRED_PALETTE_KEYS: [&str; 22] = [
    "foreground",
    "background",
    "cursor",
    "cursorAccent",
    "selectionBackground",
    "selectionForeground",
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "brightBlack",
    "brightRed",
    "brightGreen",
    "brightYellow",
    "brightBlue",
    "brightMagenta",
    "brightCyan",
    "brightWhite",
];

/// A UI theme as sent to the frontend: metadata plus the raw CSS to inject.
#[derive(Clone, Debug, Serialize)]
pub struct ThemeWire {
    pub id: String,
    pub name: String,
    /// `true` = use the OS-native window chrome; `false` = hide it and render
    /// the custom `TuiTitleBar`. Drives `setDecorations` on the frontend.
    pub decorations: bool,
    /// Terminal palette this theme pairs with when the user switches to it
    /// (unless they've chosen a "Custom" palette).
    pub palette: String,
    pub css: String,
}

/// On-disk `theme.json` shape (the `css` is read from the sibling file).
#[derive(Debug, Deserialize)]
struct ThemeMeta {
    id: String,
    name: String,
    decorations: bool,
    palette: String,
}

/// A terminal palette as sent to the frontend.
#[derive(Clone, Debug, Serialize)]
pub struct PaletteWire {
    pub name: String,
    pub colors: HashMap<String, String>,
}

/// On-disk `<name>.json` palette shape.
#[derive(Debug, Deserialize)]
struct PaletteFile {
    name: String,
    colors: HashMap<String, String>,
}

/// `<exe-dir>/themes`. `None` if `current_exe()` has no usable parent (a
/// sandbox / odd platform), in which case the registry is empty.
fn themes_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("themes"))
}

/// `<exe-dir>/palettes`. See [`themes_dir`].
fn palettes_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("palettes"))
}

// ---- verification helpers ------------------------------------------------

/// A theme id / folder name must be a non-empty `[A-Za-z0-9_-]+` token so it
/// is safe as both a CSS attribute value and a path component.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A palette color must be a real CSS hex color: `#` followed by exactly 3
/// (`#rgb`), 4 (`#rgba`), 6 (`#rrggbb`), or 8 (`#rrggbbaa`) hex digits. 5- and
/// 7-digit strings are NOT valid CSS colors — accepting them (the old `3..=8`
/// range did) lets a malformed value like `#12345` pass verification and reach
/// xterm.js as an unparseable ITheme slot.
fn valid_hex(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('#') else {
        return false;
    };
    matches!(rest.len(), 3 | 4 | 6 | 8) && rest.chars().all(|c| c.is_ascii_hexdigit())
}

/// Build a verified `ThemeWire` from a `theme.json` body, its sibling
/// `theme.css`, and the id implied by the containing folder. Returns the
/// reason on failure so the caller can log it.
fn build_theme(folder_id: &str, json: &str, css: &str) -> Result<ThemeWire, String> {
    let meta: ThemeMeta =
        serde_json::from_str(json).map_err(|e| format!("theme.json parse: {e}"))?;
    if !valid_id(&meta.id) {
        return Err(format!("invalid theme id {:?}", meta.id));
    }
    if meta.id != folder_id {
        return Err(format!(
            "theme id {:?} does not match folder {:?}",
            meta.id, folder_id
        ));
    }
    if meta.name.trim().is_empty() {
        return Err("theme name is empty".into());
    }
    if meta.palette.trim().is_empty() {
        return Err("theme palette is empty".into());
    }
    if css.trim().is_empty() {
        return Err("theme.css is empty".into());
    }
    let selector = format!("[data-theme=\"{}\"]", meta.id);
    if !css.contains(&selector) {
        return Err(format!(
            "theme.css does not contain the {selector} selector"
        ));
    }
    Ok(ThemeWire {
        id: meta.id,
        name: meta.name,
        decorations: meta.decorations,
        palette: meta.palette,
        css: css.to_string(),
    })
}

/// Build a verified `PaletteWire` from a palette JSON body. Returns the reason
/// on failure so the caller can log it.
fn build_palette(json: &str) -> Result<PaletteWire, String> {
    let file: PaletteFile =
        serde_json::from_str(json).map_err(|e| format!("palette parse: {e}"))?;
    if file.name.trim().is_empty() {
        return Err("palette name is empty".into());
    }
    for key in REQUIRED_PALETTE_KEYS {
        match file.colors.get(key) {
            None => return Err(format!("palette {:?} missing key {key}", file.name)),
            Some(v) if !valid_hex(v) => {
                return Err(format!(
                    "palette {:?} key {key} has invalid hex {v:?}",
                    file.name
                ))
            }
            Some(_) => {}
        }
    }
    Ok(PaletteWire {
        name: file.name,
        colors: file.colors,
    })
}

// ---- built-in theme + embedded palette -----------------------------------
//
// The `tui` theme is hardcoded: CSS compiled in via `include_str!`, metadata
// in code. It is always in the registry and always wins over a same-id disk
// folder — the default look can never be broken or shadowed by disk state.
// `OpenCode Grey` is embedded as a last-resort palette fallback and is the
// default new installs land on.

/// Id of the built-in, always-available TUI theme.
pub const TUI_THEME_ID: &str = "tui";

const TUI_THEME_CSS: &str = include_str!("tui_theme.css");

const EMBEDDED_PALETTE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../palettes/OpenCode Grey.json"
));

/// The compiled-in `tui` theme. Metadata mirrors what a theme.json would
/// say: custom title bar (no OS decorations), paired with the OpenCode Grey
/// terminal palette (the embedded + default palette).
fn builtin_tui_theme() -> ThemeWire {
    ThemeWire {
        id: TUI_THEME_ID.to_string(),
        name: "TUI".to_string(),
        decorations: false,
        palette: "OpenCode Grey".to_string(),
        css: TUI_THEME_CSS.to_string(),
    }
}

/// The compiled-in `OpenCode Grey` palette. See [`embedded_theme`].
fn embedded_palette() -> Option<PaletteWire> {
    match build_palette(EMBEDDED_PALETTE_JSON) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::error!(error = %e, "theming: embedded OpenCode Grey failed verification");
            None
        }
    }
}

// ---- load ----------------------------------------------------------------

/// Discover + verify every theme under `<exe-dir>/themes`, plus the built-in
/// `tui` theme — inserted last so it always exists and a disk folder can
/// never override it (it's hardcoded by design; see the module docs).
/// Sorted by id.
fn load_themes() -> Vec<ThemeWire> {
    let mut map: BTreeMap<String, ThemeWire> = BTreeMap::new();

    if let Some(dir) = themes_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(folder_id) = path.file_name().and_then(|n| n.to_str()) else {
                    tracing::warn!(path = %path.display(), "theming: non-UTF-8 theme folder name; skipped");
                    continue;
                };
                // Hidden/service folders (e.g. a stray `.cimp/` graph dir from
                // a tool run rooted at themes/) are never themes — skip them
                // silently rather than warning about a missing theme.json.
                if folder_id.starts_with('.') {
                    continue;
                }
                // The `tui` id belongs to the built-in theme, which is
                // hardcoded and non-overridable; a disk folder claiming it
                // is ignored loudly.
                if folder_id == TUI_THEME_ID {
                    tracing::warn!(
                        theme = folder_id,
                        "theming: the `tui` theme is built in and cannot be overridden; folder skipped"
                    );
                    continue;
                }
                let json = std::fs::read_to_string(path.join("theme.json"));
                let css = std::fs::read_to_string(path.join("theme.css"));
                let (Ok(json), Ok(css)) = (json, css) else {
                    tracing::warn!(
                        theme = folder_id,
                        "theming: theme folder missing theme.json/theme.css; skipped"
                    );
                    continue;
                };
                match build_theme(folder_id, &json, &css) {
                    Ok(t) => {
                        map.insert(t.id.clone(), t);
                    }
                    Err(e) => {
                        tracing::warn!(theme = folder_id, error = %e, "theming: theme failed verification; skipped")
                    }
                }
            }
        }
    }

    // Built-in theme goes in last: always present, never overridden.
    let tui = builtin_tui_theme();
    map.insert(tui.id.clone(), tui);

    map.into_values().collect()
}

/// Discover + verify every palette under `<exe-dir>/palettes`, with the
/// embedded `GitHub Dark` as a base so the list is never empty. A valid
/// on-disk palette with the same name overrides the embedded copy. Sorted by
/// name.
fn load_palettes() -> Vec<PaletteWire> {
    let mut map: BTreeMap<String, PaletteWire> = BTreeMap::new();
    if let Some(p) = embedded_palette() {
        map.insert(p.name.clone(), p);
    }

    // Filenames already loaded from disk, keyed by the palette's internal
    // `name` — nothing enforces name-vs-filename agreement (unlike themes,
    // whose id must match the folder), so two files can declare the same name
    // and whichever `read_dir` enumerates last silently wins, in an
    // unspecified order. That stays last-wins (changing it could break
    // existing setups) but is now warned about, so "my edits don't show up"
    // is diagnosable. Overriding the *embedded* namesake is the normal case
    // and stays silent.
    let mut disk_sources: HashMap<String, String> = HashMap::new();

    if let Some(dir) = palettes_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string();
                let Ok(json) = std::fs::read_to_string(&path) else {
                    tracing::warn!(file = %file_name, "theming: palette file unreadable; skipped");
                    continue;
                };
                match build_palette(&json) {
                    Ok(p) => {
                        if let Some(earlier) = disk_sources.get(&p.name) {
                            tracing::warn!(
                                palette = %p.name, file = %file_name, overrides = %earlier,
                                "theming: duplicate palette name across files; this file wins"
                            );
                        }
                        disk_sources.insert(p.name.clone(), file_name);
                        map.insert(p.name.clone(), p);
                    }
                    Err(e) => {
                        tracing::warn!(file = %file_name, error = %e, "theming: palette failed verification; skipped")
                    }
                }
            }
        }
    }

    map.into_values().collect()
}

// ---- IPC commands --------------------------------------------------------

/// Every verified theme found on disk. The frontend fetches this once at
/// startup, injects each theme's CSS into <head>, and lists them in
/// Settings → Appearance.
///
/// **Left as a direct call** (V42 Phase A): the body is one call on
/// [`load_themes`], a free function in this module with its own tests. There is
/// no `State`, no `AppHandle` and nothing to shape — this command was headless
/// the day it was written. Same for [`palettes_list`].
#[tauri::command]
pub fn themes_list() -> Vec<ThemeWire> {
    load_themes()
}

/// Every verified terminal palette found on disk.
///
/// **Left as a direct call**, for [`themes_list`]'s reason.
#[tauri::command]
pub fn palettes_list() -> Vec<PaletteWire> {
    load_palettes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The repo-root `themes/` folder — the source of truth that ships in the
    /// release and is copied next to the exe by build.rs. `CARGO_MANIFEST_DIR`
    /// is `src-tauri/`, so the repo root is one level up.
    fn repo_themes() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("themes")
    }

    fn repo_palettes() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("palettes")
    }

    #[test]
    fn shipped_themes_all_verify() {
        let dir = repo_themes();
        let mut count = 0;
        for entry in std::fs::read_dir(&dir).expect("themes/ dir").flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let id = path.file_name().unwrap().to_str().unwrap().to_string();
            // Same rule as load_themes: hidden/service folders are not themes.
            if id.starts_with('.') {
                continue;
            }
            let json = std::fs::read_to_string(path.join("theme.json")).expect("theme.json");
            let css = std::fs::read_to_string(path.join("theme.css")).expect("theme.css");
            build_theme(&id, &json, &css)
                .unwrap_or_else(|e| panic!("shipped theme {id} invalid: {e}"));
            count += 1;
        }
        // The TUI theme is built into the binary now; only the nippon pair
        // ships as folders.
        assert_eq!(count, 2, "expected 2 shipped themes");
    }

    #[test]
    fn shipped_palettes_all_verify() {
        let dir = repo_palettes();
        let mut count = 0;
        for entry in std::fs::read_dir(&dir).expect("palettes/ dir").flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let json = std::fs::read_to_string(&path).unwrap();
            build_palette(&json)
                .unwrap_or_else(|e| panic!("shipped palette {:?} invalid: {e}", path));
            count += 1;
        }
        assert_eq!(count, 15, "expected 15 shipped palettes");
    }

    #[test]
    fn load_always_includes_builtin_when_disk_empty() {
        // The test binary runs from target/debug/deps/, which has no themes/ or
        // palettes/ folder next to it (build.rs stages those in target/debug/,
        // one level up) — so this exercises the real "nothing on disk" path.
        // The built-in theme and embedded palette must still be present.
        assert!(
            load_themes().iter().any(|t| t.id == TUI_THEME_ID),
            "load_themes must include the built-in tui theme"
        );
        assert!(
            load_palettes().iter().any(|p| p.name == "OpenCode Grey"),
            "load_palettes must include the embedded OpenCode Grey fallback"
        );
    }

    #[test]
    fn builtin_theme_and_embedded_palette_verify() {
        // The built-in theme must satisfy the same contract build_theme
        // enforces for disk themes: valid id, matching CSS selector,
        // non-empty name/palette/css — plus the documented metadata (custom
        // title bar, paired with the default OpenCode Grey palette).
        let t = builtin_tui_theme();
        assert!(valid_id(&t.id));
        assert!(t.css.contains("[data-theme=\"tui\"]"));
        assert!(!t.decorations);
        assert_eq!(t.palette, "OpenCode Grey");
        // The accent family must key off the injected user accent — the
        // whole point of the single built-in theme.
        assert!(t.css.contains("--tui-accent"));
        assert!(t.css.contains("--tui-text-on-accent"));

        let palette = embedded_palette().expect("embedded OpenCode Grey verifies");
        assert_eq!(palette.name, "OpenCode Grey");
        assert_eq!(palette.colors.len(), REQUIRED_PALETTE_KEYS.len());
    }

    #[test]
    fn valid_hex_accepts_common_forms() {
        for ok in ["#000", "#ffffff", "#1d1f21", "#abcd", "#12345678"] {
            assert!(valid_hex(ok), "{ok} should be valid");
        }
        for bad in ["", "#", "#12", "fff", "#gghhii", "#1234567890", "blue"] {
            assert!(!valid_hex(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn build_palette_rejects_missing_key() {
        let json = r##"{"name":"Half","colors":{"foreground":"#fff","background":"#000"}}"##;
        let err = build_palette(json).unwrap_err();
        assert!(err.contains("missing key"), "got: {err}");
    }

    #[test]
    fn build_palette_rejects_bad_hex() {
        // A complete-looking palette but one slot is not a hex color.
        let mut colors = String::from("{");
        for (i, k) in REQUIRED_PALETTE_KEYS.iter().enumerate() {
            let v = if i == 5 { "notacolor" } else { "#abcdef" };
            colors.push_str(&format!("\"{k}\":\"{v}\","));
        }
        colors.pop();
        colors.push('}');
        let json = format!("{{\"name\":\"Bad\",\"colors\":{colors}}}");
        let err = build_palette(&json).unwrap_err();
        assert!(err.contains("invalid hex"), "got: {err}");
    }

    #[test]
    fn build_theme_rejects_missing_selector() {
        let json = r#"{"id":"foo","name":"Foo","decorations":true,"palette":"Default"}"#;
        let err = build_theme("foo", json, "body { color: red; }").unwrap_err();
        assert!(err.contains("selector"), "got: {err}");
    }

    #[test]
    fn build_theme_rejects_id_folder_mismatch() {
        let json = r#"{"id":"foo","name":"Foo","decorations":true,"palette":"Default"}"#;
        let css = "[data-theme=\"foo\"] { color: red; }";
        let err = build_theme("bar", json, css).unwrap_err();
        assert!(err.contains("does not match folder"), "got: {err}");
    }

    #[test]
    fn build_theme_accepts_valid() {
        let json = r#"{"id":"foo","name":"Foo","decorations":false,"palette":"Default"}"#;
        let css = "[data-theme=\"foo\"] { --accent: #fff; }";
        let t = build_theme("foo", json, css).expect("valid theme");
        assert_eq!(t.id, "foo");
        assert!(!t.decorations);
    }
}
