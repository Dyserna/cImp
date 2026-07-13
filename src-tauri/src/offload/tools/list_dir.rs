//! Native `list_dir` tool — enumerate a directory confined to an
//! `allowed_root`. This is the ground-truth answer to "what files exist /
//! how many" questions: without it the worker reconstructs a plausible file
//! list from memory or search snippets and gets it wrong (the V21 incident).
//!
//! Read-only, cross-platform (no `ls`/`dir` shelling), confined by the same
//! `ToolCtx::confine` machinery as `read_file`. Dependency-free filename
//! globbing, matching `code_search`'s hand-rolled name filtering (the project
//! takes no direct `glob` crate dependency).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::offload::openai::ToolDef;

use super::ToolCtx;

/// Max entries listed before the truncation marker fires.
const MAX_ENTRIES: usize = 500;
/// Hard byte ceiling on the rendered output (mirrors `run_command`'s cap).
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const DEFAULT_DEPTH: u32 = 1;
const MIN_DEPTH: u32 = 1;
const MAX_DEPTH: u32 = 3;

#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    max_depth: Option<u32>,
    #[serde(default)]
    glob: Option<String>,
}

pub fn def() -> ToolDef {
    ToolDef::function(
        "list_dir",
        "List a directory confined to the allowed roots. This is THE way to answer \"what files \
         exist here\" or \"how many files\" questions — enumerate with this tool, never \
         reconstruct a file list or count from memory or search snippets. Output is a header line \
         with the resolved directory and the total entry count, then one entry per line: \
         directories end with `/`, files are `NAME<TAB>SIZE` (bytes), sorted directories-first \
         then alphabetically. `.git` is always skipped.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (absolute, or relative to the project root)." },
                "max_depth": { "type": "integer", "description": "Recursion depth (default 1 = immediate children; clamped 1–3)." },
                "glob": { "type": "string", "description": "Optional filename filter, e.g. \"*.md\" or \"test_*\". Matches entry names case-insensitively (`*` = any run, `?` = one char)." }
            },
            "required": ["path"]
        }),
    )
}

/// One listed entry, with its display name relative to the listed root.
struct Entry {
    /// Name relative to the listed root, `/`-separated. No trailing slash —
    /// the renderer adds it for directories.
    name: String,
    is_dir: bool,
    size: u64,
}

pub async fn execute(args: serde_json::Value, ctx: &ToolCtx) -> Result<String, String> {
    let args: Args = serde_json::from_value(args).map_err(|e| format!("invalid list_dir args: {e}"))?;
    let root = ctx.confine(&args.path)?;
    let depth = args.max_depth.unwrap_or(DEFAULT_DEPTH).clamp(MIN_DEPTH, MAX_DEPTH);
    let glob = args.glob.clone();

    // The directory walk is blocking — run it off the async runtime, like
    // `code_search`.
    let walk_root = root.clone();
    let entries = tokio::task::spawn_blocking(move || walk(&walk_root, depth, glob.as_deref()))
        .await
        .map_err(|e| format!("list_dir task failed: {e}"))??;

    Ok(render(&root, entries))
}

/// Enumerate `root` to `max_depth`, applying the optional filename `glob` to
/// each entry's leaf name. Directories are always descended (except `.git`)
/// regardless of the glob, so a `*.md` filter still finds nested matches.
fn walk(root: &Path, max_depth: u32, glob: Option<&str>) -> Result<Vec<Entry>, String> {
    let meta = std::fs::metadata(root).map_err(|e| format!("list_dir failed: {e}"))?;
    if !meta.is_dir() {
        return Err(format!("`{}` is not a directory", root.display()));
    }
    let mut out: Vec<Entry> = Vec::new();
    // The glob pattern is constant for the whole walk — lowercase + collect it
    // once here rather than re-deriving it for every entry inside the loop.
    let glob_pat: Option<Vec<char>> = glob.map(|g| g.to_lowercase().chars().collect());
    // (dir, level) where `level` is the 1-based depth of that dir's children.
    let mut stack: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 1)];
    while let Some((dir, level)) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Never follow symlinks: a link under an allowed root could point
            // outside it (same guard as `code_search`).
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let leaf = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            // Always skip `.git` — never listed, never descended.
            if leaf == ".git" {
                continue;
            }
            let is_dir = file_type.is_dir();
            if !is_dir && !file_type.is_file() {
                continue; // sockets/fifos/etc.
            }
            let listed = match &glob_pat {
                Some(p) => glob_match_chars(p, &leaf),
                None => true,
            };
            if listed {
                let rel = path
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| leaf.clone());
                let size = if is_dir {
                    0
                } else {
                    entry.metadata().map(|m| m.len()).unwrap_or(0)
                };
                out.push(Entry { name: rel, is_dir, size });
            }
            if is_dir && level < max_depth {
                stack.push((path, level + 1));
            }
        }
    }
    // Directories first, then alphabetical — makes counting and filtering
    // trivial for the model.
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    Ok(out)
}

/// Render the header (resolved dir + total count *before* capping) followed by
/// the entry lines, capped at [`MAX_ENTRIES`] / [`MAX_OUTPUT_BYTES`]. The
/// header count always reflects the true total, so a truncated listing can
/// never masquerade as a complete one.
fn render(root: &Path, entries: Vec<Entry>) -> String {
    let total = entries.len();
    let header = format!(
        "{} ({} entr{})",
        root.display(),
        total,
        if total == 1 { "y" } else { "ies" }
    );
    let mut out = header;
    let mut shown = 0usize;
    let mut truncated = false;
    for e in &entries {
        if shown >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let line = if e.is_dir {
            format!("\n{}/", e.name)
        } else {
            format!("\n{}\t{}", e.name, e.size)
        };
        if out.len() + line.len() > MAX_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if truncated {
        out.push_str(&format!(
            "\n[result truncated — showed {shown} of {total} entries; narrow with `glob` or a lower `max_depth`]"
        ));
    }
    out
}

/// Case-insensitive filename glob supporting `*` (any run, incl. empty) and
/// `?` (exactly one char). Filename-only — no path separators or char classes;
/// it matches an entry's leaf name. Standard two-pointer wildcard match with
/// backtracking on `*`.
///
/// `pattern` is expected already lowercased (so a walk can lowercase + collect
/// it once, not per entry); only the `name` side is lowered here.
fn glob_match_chars(pattern: &[char], name: &str) -> bool {
    let p = pattern;
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Convenience `&str`-pattern wrapper over [`glob_match_chars`] — lowercases and
/// collects the pattern for one-shot callers (tests). The walk path uses
/// [`glob_match_chars`] directly with a pattern hoisted out of the entry loop.
#[cfg(test)]
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    glob_match_chars(&p, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::OffloadToolToggles;

    /// A fresh, unique temp directory for a test to populate.
    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cimp-list-dir-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx_for(root: &Path) -> ToolCtx {
        ToolCtx::new(vec![root.to_path_buf()], vec![], vec![], root)
    }

    fn write(root: &Path, rel: &str, contents: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, contents).unwrap();
    }

    #[test]
    fn glob_matches_star_and_question() {
        assert!(glob_match("*.md", "README.md"));
        assert!(glob_match("*.md", "a.MD")); // case-insensitive
        assert!(!glob_match("*.md", "notes.txt"));
        assert!(glob_match("test_*", "test_foo.rs"));
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
    }

    #[tokio::test]
    async fn lists_dirs_first_then_files_with_sizes() {
        let root = temp_root("basic");
        write(&root, "zebra.txt", "abc"); // 3 bytes
        write(&root, "apple.rs", "");
        std::fs::create_dir_all(root.join("subdir")).unwrap();
        std::fs::create_dir_all(root.join("aaa_dir")).unwrap();
        let ctx = ctx_for(&root);
        let out = execute(json!({ "path": root.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("(4 entries)"), "header: {}", lines[0]);
        // dirs first (alpha), then files (alpha)
        assert_eq!(lines[1], "aaa_dir/");
        assert_eq!(lines[2], "subdir/");
        assert_eq!(lines[3], "apple.rs\t0");
        assert_eq!(lines[4], "zebra.txt\t3");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn glob_filters_entries() {
        let root = temp_root("glob");
        write(&root, "a.md", "");
        write(&root, "b.md", "");
        write(&root, "c.rs", "");
        let ctx = ctx_for(&root);
        let out = execute(json!({ "path": root.to_string_lossy(), "glob": "*.md" }), &ctx)
            .await
            .unwrap();
        assert!(out.contains("(2 entries)"), "out: {out}");
        assert!(out.contains("a.md"));
        assert!(out.contains("b.md"));
        assert!(!out.contains("c.rs"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn depth_is_clamped_and_controls_recursion() {
        let root = temp_root("depth");
        write(&root, "top.txt", "");
        write(&root, "sub/nested.txt", "");
        write(&root, "sub/deeper/deep.txt", "");
        let ctx = ctx_for(&root);

        // Depth 1 (default): only top-level entries (top.txt + sub/).
        let d1 = execute(json!({ "path": root.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(d1.contains("(2 entries)"), "d1: {d1}");
        assert!(!d1.contains("nested.txt"));

        // Depth 2: adds sub/nested.txt and sub/deeper (but not deep.txt).
        let d2 = execute(json!({ "path": root.to_string_lossy(), "max_depth": 2 }), &ctx)
            .await
            .unwrap();
        assert!(d2.contains("sub/nested.txt"));
        assert!(d2.contains("sub/deeper/"));
        assert!(!d2.contains("deep.txt"));

        // Over-max clamps to 3, reaching deep.txt.
        let d99 = execute(json!({ "path": root.to_string_lossy(), "max_depth": 99 }), &ctx)
            .await
            .unwrap();
        assert!(d99.contains("sub/deeper/deep.txt"), "d99: {d99}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn git_dir_is_skipped() {
        let root = temp_root("git");
        write(&root, ".git/config", "x");
        write(&root, "keep.txt", "");
        let ctx = ctx_for(&root);
        let out = execute(json!({ "path": root.to_string_lossy(), "max_depth": 3 }), &ctx)
            .await
            .unwrap();
        assert!(out.contains("(1 entry)"), "out: {out}");
        assert!(out.contains("keep.txt"));
        assert!(!out.contains(".git"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn entry_cap_marks_truncation_but_header_stays_accurate() {
        let root = temp_root("cap");
        for i in 0..(MAX_ENTRIES + 25) {
            write(&root, &format!("f{i:04}.txt"), "");
        }
        let total = MAX_ENTRIES + 25;
        let ctx = ctx_for(&root);
        let out = execute(json!({ "path": root.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        // Header reports the true total (before capping).
        assert!(out.contains(&format!("({total} entries)")), "header: {}", out.lines().next().unwrap());
        assert!(out.contains("[result truncated"), "no marker: {}", out.lines().last().unwrap());
        assert!(out.contains(&format!("showed {MAX_ENTRIES} of {total}")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn confinement_rejects_dotdot_escape() {
        let base = temp_root("confine-base");
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        // A sibling directory outside `root` but under `base`.
        std::fs::create_dir_all(base.join("outside")).unwrap();
        let ctx = ctx_for(&root);
        let err = execute(json!({ "path": "../outside" }), &ctx).await;
        assert!(err.is_err(), "expected `..` escape to be rejected: {err:?}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn confinement_rejects_absolute_outside_root() {
        let root = temp_root("confine-abs");
        let outside = temp_root("confine-outside");
        let ctx = ctx_for(&root);
        let err = execute(json!({ "path": outside.to_string_lossy() }), &ctx).await;
        assert!(err.is_err(), "expected absolute-outside path to be rejected: {err:?}");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn toggle_off_removes_from_enabled_defs() {
        let mut toggles = OffloadToolToggles::default();
        assert!(toggles.list_dir, "default should be on");
        assert!(
            super::super::enabled_defs(&toggles).iter().any(|d| d.function.name == "list_dir"),
            "list_dir should be advertised when its toggle is on"
        );
        toggles.list_dir = false;
        assert!(
            !super::super::enabled_defs(&toggles).iter().any(|d| d.function.name == "list_dir"),
            "list_dir must not be advertised when its toggle is off"
        );
    }

    #[tokio::test]
    async fn dispatch_routes_to_list_dir() {
        let root = temp_root("dispatch");
        write(&root, "hello.txt", "");
        let ctx = ctx_for(&root);
        let out = super::super::dispatch("list_dir", json!({ "path": root.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(out.contains("hello.txt"), "dispatch did not route: {out}");
        std::fs::remove_dir_all(&root).ok();
    }
}
