//! Phase B — unified-diff parser + live diff-pane types (`FileDiff`, `Hunk`,
//! `parse_unified`, `summary`/`diff_file` over the §0.2 git harness).
//!
//! [`parse_unified`] is deliberately diff-text-agnostic (not tied to a
//! particular `git diff` invocation) — Phase C's `shadow::diff_vs_now` feeds
//! it the same way, per the milestone's "unified text → diff.rs parser" note.
//! [`summary`]/[`diff_file`] are the two entry points the Phase B IPC layer
//! (`workbench::WorkbenchService::diff_summary`/`diff_file`) calls; both run
//! entirely through [`super::git`], so a non-git root or a missing `git`
//! surface the same typed errors every other Workbench feature does.

use std::path::Path;

use serde::Serialize;

use crate::error::{AppError, AppResult};

use super::git::{self, GitCtx};

/// Files larger than this are flagged `too_large` and never diffed/rendered —
/// a giant generated/binary-ish file blowing up into a multi-MB unified diff
/// (or a multi-second synthesize-from-scratch for an untracked one) isn't
/// something the pane should ever attempt, let alone virtualize.
pub const MAX_DIFF_FILE_BYTES: u64 = 1024 * 1024; // 1 MiB

/// git's own default unified-diff context — what every diff surface renders
/// unless the caller asks for more.
pub const DEFAULT_CONTEXT: u32 = 3;

/// Upper clamp for a frontend-supplied context value. The "full file" toggle
/// sends a huge context so the whole file arrives as one hunk; anything at or
/// above any real file's line count behaves identically, so the exact value
/// only bounds the argument, it doesn't change output.
pub const MAX_CONTEXT: u32 = 10_000_000;

/// How many leading bytes of a file's content we sniff for a NUL byte to
/// call it binary — the same buffer size git itself uses for this heuristic,
/// applied here only to UNTRACKED files (tracked ones get git's own verdict
/// for free from the `Binary files … differ` marker in `git diff`'s output).
const BINARY_SNIFF_BYTES: usize = 8000;

/// One file's change, as `git status` sees it (working tree vs `HEAD`).
/// `Renamed`/`Copied` carry the path git detected as the source; everything
/// else needs no extra data. `Copied` is folded into the same shape as
/// `Renamed` (both are "this content came from `from`") — the live diff pane
/// has no use for distinguishing "still exists at `from` too" from "moved".
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed {
        from: String,
    },
    /// Not yet under version control at all (`git status`'s `??`). Never
    /// produced by [`parse_unified`] itself — only [`diff_file`]'s synthesis
    /// path sets this, since a real `git diff` never emits an entry for an
    /// untracked file in the first place.
    Untracked,
}

/// One `@@ … @@` hunk. `lines` is the hunk body in order, each entry a
/// `(marker, text)` pair where `marker` is `' '` (context), `'+'` (added), or
/// `'-'` (removed) — `text` excludes both the marker and the trailing
/// newline. A `\ No newline at end of file` marker is recorded in
/// [`no_newline_at`](Self::no_newline_at) so [`build_hunk_patch`] can
/// reproduce it faithfully (without it, reverting a hunk that touches a file's
/// unterminated final line would silently add a trailing newline).
#[derive(Clone, Debug, Serialize, PartialEq, Default)]
pub struct Hunk {
    /// The raw `@@ -a,b +c,d @@ <context>` header line, kept verbatim for
    /// display (the trailing function-context git appends is useful to a
    /// human, even though [`build_hunk_patch`] rebuilds a minimal header from
    /// the numeric fields instead of reusing this text).
    pub header: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<(char, String)>,
    /// Indices into [`lines`](Self::lines) that were followed by a `\ No
    /// newline at end of file` marker in the source diff — i.e. lines whose
    /// content is NOT newline-terminated on disk. Usually empty; at most the
    /// hunk's final `-` and/or `+` line. [`build_hunk_patch`] re-emits the
    /// marker after each so a reverse-apply doesn't mutate the file's trailing
    /// newline.
    pub no_newline_at: Vec<usize>,
    /// [`hunk_hash`] of this hunk's own content, precomputed once the hunk is
    /// fully built (by [`parse_unified`] / [`synthesize_untracked`]) and
    /// carried over the wire so the frontend has something opaque to echo
    /// back on `workbench_revert_hunk` — it never computes this itself, only
    /// round-trips whatever it was last shown. Empty (`""`) on a
    /// freshly-`Default`-constructed `Hunk` that hasn't gone through either
    /// builder (only happens in this module's own unit tests).
    pub hash: String,
}

/// One file's full parsed diff — the payload of `workbench_diff_file`.
#[derive(Clone, Debug, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub status: FileStatus,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    pub too_large: bool,
}

/// One row of the file list — the payload of `workbench_diff_summary`.
/// `added`/`removed` are line counts (best-effort: from `git diff --numstat`
/// for tracked files, from a line count of the file itself for untracked
/// ones — `0`/`0` when binary or `too_large`), used only for the ± line
/// badges in the file list; the per-hunk detail comes from a separate
/// `workbench_diff_file` call once the row is expanded.
#[derive(Clone, Debug, Serialize)]
pub struct FileDiffMeta {
    pub path: String,
    pub status: FileStatus,
    pub binary: bool,
    pub too_large: bool,
    pub added: u32,
    pub removed: u32,
}

/// Derive a [`FileDiffMeta`] row from an already-[`parse_unified`]d
/// [`FileDiff`] — FIX 7 / V13 code review's Phase C shadow-repo fallback
/// (`WorkbenchService::diff_summary`) uses this: `shadow::diff_vs_now`
/// returns ONE unified-diff blob covering every changed file, parsed once,
/// so each row can be derived straight from its parsed `FileDiff` rather than
/// a second git-style stat call — there's no git repo to ask `--numstat` in
/// the non-git case this fallback exists for. `added`/`removed` are counted
/// directly from the parsed hunks' `+`/`-` markers (exact, unlike the
/// git-backed `summary`'s `--numstat` value, but this function only ever
/// runs on an already-parsed diff, so the count is free).
pub fn file_diff_meta_from_parsed(file: &FileDiff) -> FileDiffMeta {
    let added = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|(m, _)| *m == '+')
        .count() as u32;
    let removed = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|(m, _)| *m == '-')
        .count() as u32;
    FileDiffMeta {
        path: file.path.clone(),
        status: file.status.clone(),
        binary: file.binary,
        too_large: file.too_large,
        added: if file.too_large { 0 } else { added },
        removed: if file.too_large { 0 } else { removed },
    }
}

/// Where a [`DiffSummary`]/[`FileDiff`] came from. `Git` is this module's own
/// `summary`/`diff_file`, over the user's real repo; `Shadow` is
/// [`super::WorkbenchService::diff_summary`]/`diff_file`'s FIX 7 (V13 code
/// review) fallback for a NON-git project with checkpoints on — diffed
/// against the latest Phase C shadow checkpoint via `shadow::diff_vs_now`
/// instead. Kept here (rather than only in `shadow.rs`) since the frontend's
/// discriminated union is keyed on this type either way.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiffSource {
    Git,
    Shadow,
}

/// The `workbench_diff_summary` payload: every changed file (status only —
/// no hunks), whether the repo is mid-merge/-rebase (revert actions must
/// refuse), and where the diff came from (`None` when neither git nor a
/// checkpoint snapshot is available — the frontend renders the requirements
/// banner in that case rather than treating it as an error).
#[derive(Clone, Debug, Serialize)]
pub struct DiffSummary {
    pub files: Vec<FileDiffMeta>,
    pub readonly: bool,
    pub source: Option<DiffSource>,
}

// ── unified-diff parsing ────────────────────────────────────────────────

/// Parse (possibly multi-file) unified diff text — e.g. the output of `git
/// diff --no-color --unified=3 HEAD` — into one [`FileDiff`] per `diff --git`
/// section. Never panics on malformed input: an unparseable hunk header or
/// stray line is simply skipped rather than aborting the whole parse, since
/// this runs on live `git` output that this module doesn't fully control.
///
/// Handles: multiple files in one diff text; multiple hunks per file;
/// rename/copy headers (`status` becomes [`FileStatus::Renamed`]); the
/// `Binary files … differ` marker (`binary: true`, no hunks — git never
/// emits hunks for a binary diff); and `\ No newline at end of file` markers
/// (consumed, not added to `lines`). [`FileStatus::Untracked`] is never
/// produced here — see [`FileStatus::Untracked`]'s doc comment.
pub fn parse_unified(diff_text: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    // Split on `\n` only, preserving any trailing `\r` as part of the line's
    // content. `str::lines()` strips `\r\n` down to LF, which silently drops
    // the `\r` from CRLF file content; `build_hunk_patch` would then emit an
    // LF-only patch that `git apply` can't match against the on-disk `\r\n`,
    // breaking revert for every CRLF file (common on Windows). Structural
    // lines from `git` (`diff --git`, `@@ `, `--- `, …) are LF-terminated and
    // carry no `\r`, so the `starts_with`/`strip_prefix` checks below are
    // unaffected — only real content lines keep their `\r`.
    let mut lines = diff_text
        .split_inclusive('\n')
        .map(|l| l.strip_suffix('\n').unwrap_or(l))
        .peekable();

    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let Some((_old_path, new_path)) = parse_diff_git_line(rest) else {
            continue;
        };

        let mut status = FileStatus::Modified;
        let mut binary = false;
        let mut renamed_from: Option<String> = None;
        // The current path as stated by the explicit header lines (`+++ b/`,
        // `rename to`, or — for a deletion, whose `+++` side is `/dev/null` —
        // the `--- a/` line). Preferred over [`parse_diff_git_line`]'s
        // first-`" b/"` split of the `diff --git` line, which mis-splits a
        // path containing that literal substring (e.g. `lib b/x.rs`). Stays
        // `None` for git's quoted-path form (`+++ "b/…"`), where the split
        // fallback applies — unquoting is out of V1 scope either way.
        let mut path_from_headers: Option<String> = None;
        let mut old_path_header: Option<String> = None;

        // Header lines between `diff --git` and the first hunk (or the
        // binary marker, or the next file's `diff --git`).
        loop {
            match lines.peek() {
                Some(l) if l.starts_with("new file mode") => {
                    status = FileStatus::Added;
                    lines.next();
                }
                Some(l) if l.starts_with("deleted file mode") => {
                    status = FileStatus::Deleted;
                    lines.next();
                }
                Some(l) if l.starts_with("old mode") || l.starts_with("new mode") => {
                    lines.next();
                }
                Some(l)
                    if l.starts_with("similarity index")
                        || l.starts_with("dissimilarity index") =>
                {
                    lines.next();
                }
                Some(l) if l.starts_with("rename from ") => {
                    renamed_from = Some(l["rename from ".len()..].to_string());
                    lines.next();
                }
                Some(l) if l.starts_with("copy from ") => {
                    renamed_from = Some(l["copy from ".len()..].to_string());
                    lines.next();
                }
                Some(l) if l.starts_with("rename to ") => {
                    path_from_headers = Some(l["rename to ".len()..].to_string());
                    lines.next();
                }
                Some(l) if l.starts_with("copy to ") => {
                    path_from_headers = Some(l["copy to ".len()..].to_string());
                    lines.next();
                }
                Some(l) if l.starts_with("index ") => {
                    lines.next();
                }
                Some(l) if l.starts_with("Binary files ") && l.ends_with("differ") => {
                    binary = true;
                    lines.next();
                    break;
                }
                Some(l) if l.starts_with("--- ") => {
                    if let Some(p) = l.strip_prefix("--- a/") {
                        old_path_header = Some(p.to_string());
                    }
                    lines.next();
                }
                Some(l) if l.starts_with("+++ ") => {
                    if let Some(p) = l.strip_prefix("+++ b/") {
                        path_from_headers = Some(p.to_string());
                    } else if *l == "+++ /dev/null" {
                        // A deletion: the current name is on the `---` side.
                        path_from_headers = old_path_header.take();
                    }
                    lines.next();
                    break;
                }
                _ => break,
            }
        }
        let new_path = path_from_headers.unwrap_or(new_path);
        if let Some(from) = renamed_from {
            status = FileStatus::Renamed { from };
        }

        let mut hunks = Vec::new();
        while let Some(l) = lines.peek() {
            if l.starts_with("diff --git ") {
                break;
            }
            let Some(rest) = l.strip_prefix("@@ ") else {
                // A stray line between hunks (e.g. `\ No newline…` attached
                // to a header-only rename, or trailing noise) — skip it
                // rather than looping forever or misparsing it as a hunk.
                lines.next();
                continue;
            };
            let header_line = (*l).to_string();
            lines.next();
            let Some(mut hunk) = parse_hunk_header(&header_line, rest) else {
                continue;
            };
            while let Some(bl) = lines.peek() {
                if bl.starts_with("@@ ") || bl.starts_with("diff --git ") {
                    break;
                }
                if *bl == "\\ No newline at end of file" {
                    // Applies to the line just pushed — record its index so
                    // `build_hunk_patch` can reproduce the marker (else a revert
                    // would silently newline-terminate an unterminated file).
                    if let Some(last) = hunk.lines.len().checked_sub(1) {
                        hunk.no_newline_at.push(last);
                    }
                    lines.next();
                    continue;
                }
                if bl.is_empty() {
                    // A blank line in the hunk body is a content line with NO
                    // marker character AND no content — `git diff` emits this
                    // for a genuinely empty context line (the file has a
                    // blank line at that position that neither side
                    // changed). Treat it as an empty context line rather than
                    // indexing `bl[1..]` on a 0-byte string below, which
                    // panics (`marker.len_utf8()` on the `unwrap_or(' ')`
                    // fallback is 1, but `bl` has 0 bytes to slice) — this
                    // function's contract is "never panics" on arbitrary diff
                    // text (see the doc comment above).
                    hunk.lines.push((' ', String::new()));
                    lines.next();
                    continue;
                }
                let marker = bl.chars().next().unwrap_or(' ');
                if marker == '+' || marker == '-' || marker == ' ' {
                    hunk.lines
                        .push((marker, bl[marker.len_utf8()..].to_string()));
                    lines.next();
                } else {
                    // Unexpected content (shouldn't happen with real `git`
                    // output) — stop this hunk rather than misparse it.
                    break;
                }
            }
            hunk.hash = hunk_hash(&hunk);
            hunks.push(hunk);
        }

        // `new_path` here is the explicit-header path when one was found
        // (see `path_from_headers` above) or the `diff --git a/X b/Y` line's
        // `Y` otherwise — which is the file's current path in every case
        // (modify/add/delete/rename); deletions still show the original name
        // there (only the `+++` line says `/dev/null`).
        files.push(FileDiff {
            path: new_path,
            status,
            binary,
            hunks,
            too_large: false,
        });
    }

    files
}

/// Split a `diff --git a/<old> b/<new>` line's tail (everything after `"diff
/// --git "`) into `(old, new)`. Splits on the first `" b/"` — mis-splits a
/// path that itself contains that literal substring, which is why
/// [`parse_unified`] prefers the explicit `+++ b/` / `rename to` header lines
/// when present and only falls back to this split (git QUOTES such paths in
/// the header lines too; unquoting isn't implemented in V1, mirroring the
/// "mechanical, no exotic edge cases" scope of this milestone).
fn parse_diff_git_line(rest: &str) -> Option<(String, String)> {
    let idx = rest.find(" b/")?;
    let old = rest[..idx]
        .strip_prefix("a/")
        .unwrap_or(&rest[..idx])
        .to_string();
    let new = rest[idx + 3..].to_string();
    Some((old, new))
}

/// Parse a hunk header's numeric fields. `full_line` is kept verbatim as
/// [`Hunk::header`]; `rest` is the text after `"@@ "` (e.g. `"-12,3 +15,4 @@
/// fn foo() {"`).
fn parse_hunk_header(full_line: &str, rest: &str) -> Option<Hunk> {
    let end = rest.find(" @@")?;
    let mut parts = rest[..end].split_whitespace();
    let (old_start, old_lines) = parse_range(parts.next()?, '-')?;
    let (new_start, new_lines) = parse_range(parts.next()?, '+')?;
    Some(Hunk {
        header: full_line.to_string(),
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: Vec::new(),
        no_newline_at: Vec::new(),
        // Filled in by the caller once `lines` is populated — see the
        // `hunk.hash = hunk_hash(&hunk)` right before this hunk is pushed.
        hash: String::new(),
    })
}

/// Parse one side of a hunk range token (`"-12,3"` / `"+4"`) into
/// `(start, len)`. A comma-less token means a single-line range (git's own
/// shorthand for `len == 1`).
fn parse_range(tok: &str, sign: char) -> Option<(u32, u32)> {
    let spec = tok.strip_prefix(sign)?;
    let mut it = spec.splitn(2, ',');
    let start: u32 = it.next()?.parse().ok()?;
    let len: u32 = match it.next() {
        Some(s) => s.parse().ok()?,
        None => 1,
    };
    Some((start, len))
}

// ── hunk fingerprint + minimal-patch reconstruction (used by B2 revert) ───

/// A stable fingerprint of a hunk's content, used as the staleness guard on
/// revert: the frontend echoes back the hash it was shown, and
/// [`super::WorkbenchService::revert_hunk`] refuses to apply if the file's
/// current hunk at that index no longer matches — an agent edit raced the
/// UI. `DefaultHasher::new()` uses fixed (non-randomized) keys, unlike a
/// `HashMap`'s default `RandomState`, so this is stable across calls within
/// (and across) a process — exactly what a "did this change" comparison
/// needs.
pub fn hunk_hash(hunk: &Hunk) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hunk.old_start.hash(&mut hasher);
    hunk.old_lines.hash(&mut hasher);
    hunk.new_start.hash(&mut hasher);
    hunk.new_lines.hash(&mut hasher);
    hunk.lines.hash(&mut hasher);
    hunk.no_newline_at.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Reconstruct the minimal single-hunk patch text `git apply --reverse
/// --unidiff-zero -` needs to revert just `hunk` of `file` — a `---`/`+++`
/// header pair (routed through `/dev/null` for whole-file add/delete, per
/// `file.status`) plus the one hunk's header (rebuilt from its numeric
/// fields, not [`Hunk::header`]'s verbatim text — cheaper than stripping the
/// trailing function-context git appends, and `git apply` doesn't care) and
/// body. Both sides always name `file.path` (the file's CURRENT path) even
/// for a rename — the physical file being patched on disk lives at the new
/// path; the old name from `FileStatus::Renamed` is display-only.
///
/// Reproduces `\ No newline at end of file` markers from
/// [`Hunk::no_newline_at`] so reverting a hunk that touches an unterminated
/// final line doesn't add (or, on reverse-apply, strip) a trailing newline the
/// user never had.
pub fn build_hunk_patch(file: &FileDiff, hunk: &Hunk) -> Vec<u8> {
    let (old_side, new_side) = match &file.status {
        FileStatus::Added | FileStatus::Untracked => {
            ("/dev/null".to_string(), format!("b/{}", file.path))
        }
        FileStatus::Deleted => (format!("a/{}", file.path), "/dev/null".to_string()),
        FileStatus::Modified | FileStatus::Renamed { .. } => {
            (format!("a/{}", file.path), format!("b/{}", file.path))
        }
    };
    let mut out = String::new();
    out.push_str("--- ");
    out.push_str(&old_side);
    out.push('\n');
    out.push_str("+++ ");
    out.push_str(&new_side);
    out.push('\n');
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
    ));
    for (i, (marker, text)) in hunk.lines.iter().enumerate() {
        out.push(*marker);
        out.push_str(text);
        out.push('\n');
        if hunk.no_newline_at.contains(&i) {
            // The preceding line has no newline on disk; the marker tells `git
            // apply` so it doesn't add (or, on reverse, strip) one.
            out.push_str("\\ No newline at end of file\n");
        }
    }
    out.into_bytes()
}

/// Format one hunk as a fenced code block with a `path:line` header, for
/// `workbench_send_hunk` — dropped into the compose overlay's draft so the
/// agent sees exactly what the user is pointing at.
pub fn format_hunk_for_agent(path: &str, hunk: &Hunk) -> String {
    let mut out = format!("`{path}:{}`\n```diff\n", hunk.new_start);
    for (marker, text) in &hunk.lines {
        out.push(*marker);
        out.push_str(text);
        out.push('\n');
    }
    out.push_str("```\n");
    out
}

// ── git-backed summary / per-file diff ─────────────────────────────────

/// One `git status --porcelain=v1 -z` record, parsed.
struct StatusEntry {
    path: String,
    status: FileStatus,
}

/// Parse `git status --porcelain=v1 -z` output (NUL-terminated records — see
/// `checks::gitls`'s doc comment for why `-z` is mandatory: without it, a
/// wholly-untracked new directory collapses into one `?? dir/` entry and
/// paths with special characters get C-quoted). Each record is `XY
/// <path>`; a rename/copy record (`X` is `R`/`C`) is followed by a second
/// NUL-terminated record holding the original path.
fn parse_status_z(raw: &str) -> Vec<StatusEntry> {
    let mut out = Vec::new();
    let mut parts = raw.split('\0').peekable();
    while let Some(rec) = parts.next() {
        if rec.len() < 3 {
            continue;
        }
        let (xy, rest) = rec.split_at(2);
        let path = rest.strip_prefix(' ').unwrap_or(rest).replace('\\', "/");
        // Guard against a `"XY "` record (no path) slipping through the
        // `len() < 3` check above as an empty-path entry.
        if path.is_empty() {
            continue;
        }
        let mut chars = xy.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        let status = if x == '?' && y == '?' {
            FileStatus::Untracked
        } else if x == 'R' || x == 'C' {
            let from = parts.next().unwrap_or("").replace('\\', "/");
            FileStatus::Renamed { from }
        } else if x == 'D' || y == 'D' {
            // Worktree/index deletion takes precedence over `x == 'A'`: an `AD`
            // record (staged add, then deleted on disk) must report as Deleted,
            // not Added — the file no longer exists, so classifying it Added
            // would point `diff_file` at a nonexistent path.
            FileStatus::Deleted
        } else if x == 'A' {
            FileStatus::Added
        } else {
            FileStatus::Modified
        };
        out.push(StatusEntry { path, status });
    }
    out
}

/// `true` when `root`'s repo is mid-merge or mid-rebase (`MERGE_HEAD` /
/// `REBASE_HEAD` resolves) — the special-state guard that puts the Diff
/// section into read-only mode (no hunk reverts) per the milestone's edge
/// cases. Both probes run regardless of git availability being pre-checked
/// by the caller; a `git` failure here just reads as "not in a special
/// state", which is the safe default (worst case a revert is attempted and
/// `git apply` itself rejects it).
async fn is_special_state(ctx: &GitCtx) -> bool {
    for head in ["MERGE_HEAD", "REBASE_HEAD"] {
        if let Ok(out) = git::run(ctx, &["rev-parse", "-q", "--verify", head], None).await {
            if out.success() {
                return true;
            }
        }
    }
    false
}

/// Parse `git diff --numstat -z HEAD` into a `path -> (added, removed,
/// binary)` map. With `-z`, a rename record's tab-separated fields are
/// followed by two additional NUL-separated path records (old, new) instead
/// of the non-`-z` form's single `"old => new"` field — see this function's
/// test for the exact byte shape. `added`/`removed` are `None` for a binary
/// entry (git prints `-\t-\t…`).
fn parse_numstat_z(raw: &str) -> std::collections::HashMap<String, (Option<u32>, Option<u32>)> {
    let mut out = std::collections::HashMap::new();
    let tokens: Vec<&str> = raw.split('\0').collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if tok.is_empty() {
            i += 1;
            continue;
        }
        let mut fields = tok.splitn(3, '\t');
        let (Some(added_s), Some(removed_s), Some(third)) =
            (fields.next(), fields.next(), fields.next())
        else {
            i += 1;
            continue;
        };
        let added = added_s.parse::<u32>().ok();
        let removed = removed_s.parse::<u32>().ok();
        if third.is_empty() {
            // Rename: the new path is the next-but-one record (old, then
            // new); `i + 2` must exist or this entry is malformed — skip.
            if i + 2 < tokens.len() {
                let new_path = tokens[i + 2].replace('\\', "/");
                out.insert(new_path, (added, removed));
                i += 3;
            } else {
                i += 1;
            }
        } else {
            out.insert(third.replace('\\', "/"), (added, removed));
            i += 1;
        }
    }
    out
}

/// The Phase B `workbench_diff_summary` entry point. `Ok` with `source:
/// None` (not an error) when `root` isn't a git repo — this module has no
/// `Settings` access, so it can't know whether checkpoints are even on; the
/// non-git shadow-repo fallback (FIX 7 / V13 code review) is layered on top
/// of THIS result by [`super::WorkbenchService::diff_summary`], which does
/// have `Settings`. A bare `source: None` here still means "the frontend
/// should render the requirements banner", but only once the service-layer
/// caller has confirmed the shadow fallback doesn't apply either.
pub async fn summary(root: &Path) -> AppResult<DiffSummary> {
    if !git::is_repo(root).await {
        return Ok(DiffSummary {
            files: Vec::new(),
            readonly: false,
            source: None,
        });
    }
    let ctx = GitCtx::discover(root);

    let status_out = git::run(&ctx, &["status", "--porcelain=v1", "-z"], None).await?;
    let entries = parse_status_z(&status_out.stdout);

    // Best-effort: an unborn HEAD (brand-new repo, no commits yet) makes
    // `git diff … HEAD` fail — degrade to "no line-count data" rather than
    // failing the whole summary, mirroring `graph::impact`'s `if let Ok(..)`
    // tolerance of the same case.
    let numstat = match git::run(&ctx, &["diff", "--numstat", "-z", "HEAD"], None).await {
        Ok(out) if out.success() => parse_numstat_z(&out.stdout),
        _ => std::collections::HashMap::new(),
    };

    let readonly = is_special_state(&ctx).await;

    // The per-entry work below is blocking filesystem I/O (a `stat` per file,
    // plus an up-to-`MAX_DIFF_FILE_BYTES` read per UNTRACKED file to line-count
    // it) with no `.await` in the loop — offload the whole batch to a blocking
    // thread so a slow/large tree can't stall a tokio runtime worker.
    let root_owned = root.to_path_buf();
    let files = tokio::task::spawn_blocking(move || {
        let mut files = Vec::with_capacity(entries.len());
        for entry in entries {
            let (added, removed, binary) = match numstat.get(&entry.path) {
                Some((Some(a), Some(r))) => (*a, *r, false),
                Some((None, None)) => (0, 0, true), // git's "-\t-" binary marker
                _ => untracked_line_estimate(&root_owned, &entry),
            };
            let too_large = std::fs::metadata(root_owned.join(&entry.path))
                .map(|m| m.len() > MAX_DIFF_FILE_BYTES)
                .unwrap_or(false);
            files.push(FileDiffMeta {
                path: entry.path,
                status: entry.status,
                binary,
                too_large,
                added: if too_large { 0 } else { added },
                removed: if too_large { 0 } else { removed },
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    })
    .await
    .map_err(|e| AppError::Workbench(format!("diff summary worker panicked: {e}")))?;

    Ok(DiffSummary {
        files,
        readonly,
        source: Some(DiffSource::Git),
    })
}

/// Best-effort line-count for an untracked file (not present in `git diff
/// --numstat`'s output at all — untracked files aren't part of a `diff
/// HEAD`): read it and count lines, capping at [`MAX_DIFF_FILE_BYTES`] and
/// sniffing for binary content the same way [`diff_file`]'s synthesis path
/// does. Returns `(added, removed, binary)` — `removed` is always `0` (an
/// untracked file has no "before" state).
fn untracked_line_estimate(root: &Path, entry: &StatusEntry) -> (u32, u32, bool) {
    if entry.status != FileStatus::Untracked {
        return (0, 0, false);
    }
    let Ok(meta) = std::fs::metadata(root.join(&entry.path)) else {
        return (0, 0, false);
    };
    if meta.len() > MAX_DIFF_FILE_BYTES {
        return (0, 0, false);
    }
    let Ok(bytes) = std::fs::read(root.join(&entry.path)) else {
        return (0, 0, false);
    };
    if looks_binary(&bytes) {
        return (0, 0, true);
    }
    let text = String::from_utf8_lossy(&bytes);
    let added = text.lines().count() as u32;
    (added, 0, false)
}

/// git's own binary heuristic, applied to a byte slice we already have in
/// memory: a NUL byte anywhere in the first [`BINARY_SNIFF_BYTES`] bytes.
fn looks_binary(bytes: &[u8]) -> bool {
    let n = bytes.len().min(BINARY_SNIFF_BYTES);
    bytes[..n].contains(&0)
}

/// The Phase B `workbench_diff_file` entry point: the full parsed diff for
/// one file. Re-derives the file's status from a fresh `git status` scan
/// (rather than trusting a caller-supplied hint) so a rename is diffed with
/// both paths — `git diff HEAD -- <path>` alone, scoped to just the new
/// path, can't see the old one and would show a rename as a from-scratch
/// add (verified against real `git` output; see this module's tests).
pub async fn diff_file(root: &Path, path: &str) -> AppResult<FileDiff> {
    diff_file_ctx(root, path, DEFAULT_CONTEXT).await
}

/// [`diff_file`] with an explicit unified-context width — the frontend's
/// per-file "diff ↔ full file" toggle passes a huge `context` so the whole
/// file arrives as one hunk (change highlighting intact). Everything else
/// (revert, send-to-agent) goes through the [`DEFAULT_CONTEXT`] wrapper so
/// hunk indices/hashes stay stable.
pub async fn diff_file_ctx(root: &Path, path: &str, context: u32) -> AppResult<FileDiff> {
    let ctx = GitCtx::discover(root);
    let status_out = git::run(&ctx, &["status", "--porcelain=v1", "-z"], None).await?;
    let entries = parse_status_z(&status_out.stdout);
    let Some(entry) = entries.into_iter().find(|e| e.path == path) else {
        // Not in the dirty set (already reverted/staged-and-committed
        // elsewhere, or the caller raced a refresh) — a clean "no changes"
        // result rather than an error; the frontend re-fetches the summary
        // and drops the row.
        return Ok(FileDiff {
            path: path.to_string(),
            status: FileStatus::Modified,
            binary: false,
            hunks: Vec::new(),
            too_large: false,
        });
    };

    if entry.status == FileStatus::Untracked {
        // `synthesize_untracked` reads the file (up to `MAX_DIFF_FILE_BYTES`)
        // synchronously — run it off the async runtime.
        let root = root.to_path_buf();
        let path = path.to_string();
        return tokio::task::spawn_blocking(move || synthesize_untracked(&root, &path))
            .await
            .map_err(|e| AppError::Workbench(format!("untracked diff worker panicked: {e}")))?;
    }

    let too_large = tokio::fs::metadata(root.join(path))
        .await
        .map(|m| m.len() > MAX_DIFF_FILE_BYTES)
        .unwrap_or(false);
    if too_large {
        return Ok(FileDiff {
            path: path.to_string(),
            status: entry.status,
            binary: false,
            hunks: Vec::new(),
            too_large: true,
        });
    }

    let unified = format!("--unified={}", context.min(MAX_CONTEXT));
    let diff_out = if let FileStatus::Renamed { from } = &entry.status {
        // Scope the diff to BOTH paths so git's rename detection (default-on
        // for `git diff`) has the old content to compare against; `-M` makes
        // that explicit rather than relying on the ambient `diff.renames`
        // config.
        git::run(
            &ctx,
            &[
                "-c",
                "core.quotePath=false",
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &unified,
                "-M",
                "HEAD",
                "--",
                from.as_str(),
                path,
            ],
            None,
        )
        .await?
    } else {
        git::run(
            &ctx,
            &[
                "-c",
                "core.quotePath=false",
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &unified,
                "HEAD",
                "--",
                path,
            ],
            None,
        )
        .await?
    };

    let mut parsed = parse_unified(&diff_out.stdout);
    // `parse_unified` already sets `binary: true` from the `Binary files …
    // differ` marker (it still emits one `FileDiff` for that case — see
    // `parse_unified_binary_marker`), so an empty `parsed` here means `git
    // diff` produced no `diff --git` section at all: the file turned out
    // clean (a summary→diff_file race — the caller re-fetches and moves on)
    // rather than "binary". `status` is overridden below regardless, since
    // `parse_unified` derives it from the diff text and can never produce
    // `Untracked` — `git status`'s verdict is the more authoritative source
    // anyway.
    //
    // Pick the section whose path matches the requested one, not blindly the
    // last: when `git status` says rename but `git diff -M` doesn't agree
    // (similarity below threshold), the two-path Renamed invocation above
    // emits TWO sections — old-path all-deleted and new-path all-added — and
    // "last" would be whichever sorts later, potentially the wrong file's
    // hunks presented under this path. Fall back to the last section when
    // nothing matches (e.g. the quoted-path form, where the parsed path
    // isn't byte-identical to `path`).
    let matched = parsed.iter().position(|f| f.path == path);
    let picked = match matched {
        Some(i) => Some(parsed.swap_remove(i)),
        None => parsed.pop(),
    };
    let Some(mut file) = picked else {
        return Ok(FileDiff {
            path: path.to_string(),
            status: entry.status,
            binary: false,
            hunks: Vec::new(),
            too_large: false,
        });
    };
    file.status = entry.status;
    file.path = path.to_string();
    Ok(file)
}

/// Synthesize an all-added [`FileDiff`] for an untracked file: one hunk
/// spanning the whole file, every line marked `+`. Mirrors what `git diff
/// --no-index /dev/null <path>` would show, without spawning git for it (the
/// file is right there on disk).
fn synthesize_untracked(root: &Path, path: &str) -> AppResult<FileDiff> {
    let abs = root.join(path);
    let meta = std::fs::metadata(&abs)
        .map_err(|e| AppError::Workbench(format!("stat {}: {e}", abs.display())))?;
    if meta.len() > MAX_DIFF_FILE_BYTES {
        return Ok(FileDiff {
            path: path.to_string(),
            status: FileStatus::Untracked,
            binary: false,
            hunks: Vec::new(),
            too_large: true,
        });
    }
    let bytes = std::fs::read(&abs)
        .map_err(|e| AppError::Workbench(format!("read {}: {e}", abs.display())))?;
    if looks_binary(&bytes) {
        return Ok(FileDiff {
            path: path.to_string(),
            status: FileStatus::Untracked,
            binary: true,
            hunks: Vec::new(),
            too_large: false,
        });
    }
    let text = String::from_utf8_lossy(&bytes);
    // Preserve trailing `\r` (see `parse_unified`): a CRLF untracked file must
    // round-trip through `build_hunk_patch` as `\r\n` or its revert won't apply.
    let body: Vec<(char, String)> = text
        .split_inclusive('\n')
        .map(|l| ('+', l.strip_suffix('\n').unwrap_or(l).to_string()))
        .collect();
    if body.is_empty() {
        return Ok(FileDiff {
            path: path.to_string(),
            status: FileStatus::Untracked,
            binary: false,
            hunks: Vec::new(),
            too_large: false,
        });
    }
    let n = body.len() as u32;
    let mut hunk = Hunk {
        header: format!("@@ -0,0 +1,{n} @@"),
        old_start: 0,
        old_lines: 0,
        new_start: 1,
        new_lines: n,
        lines: body,
        no_newline_at: Vec::new(),
        hash: String::new(),
    };
    hunk.hash = hunk_hash(&hunk);
    Ok(FileDiff {
        path: path.to_string(),
        status: FileStatus::Untracked,
        binary: false,
        hunks: vec![hunk],
        too_large: false,
    })
}

/// `true` when `root`'s repo is mid-merge/mid-rebase — re-exported for
/// `super::mod`'s revert-hunk guard so it doesn't duplicate the probe.
pub(super) async fn readonly(root: &Path) -> bool {
    is_special_state(&GitCtx::discover(root)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_git() -> bool {
        crate::pty::resolve_command("git").is_ok()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn setup_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-diff-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "core.autocrlf", "false"]);
        dir
    }

    // ── parse_unified ───────────────────────────────────────────────────

    #[test]
    fn parse_unified_multi_hunk_single_file() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
index e4a1e5d..76bfa7d 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 line1
-line2
+line2X
 line3
@@ -10,3 +10,3 @@ fn context() {
 line10
-line11
+line11X
 line12
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, "src/a.rs");
        assert_eq!(f.status, FileStatus::Modified);
        assert!(!f.binary);
        assert_eq!(f.hunks.len(), 2);
        assert_eq!(f.hunks[0].old_start, 1);
        assert_eq!(f.hunks[0].new_start, 1);
        assert_eq!(
            f.hunks[0].lines,
            vec![
                (' ', "line1".to_string()),
                ('-', "line2".to_string()),
                ('+', "line2X".to_string()),
                (' ', "line3".to_string()),
            ]
        );
        assert_eq!(f.hunks[1].old_start, 10);
        assert_eq!(f.hunks[1].lines[1], ('-', "line11".to_string()));
    }

    #[test]
    fn parse_unified_multi_file() {
        let diff = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,1 +1,1 @@
-old
+new
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1,1 +1,1 @@
-foo
+bar
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.rs");
        assert_eq!(files[1].path, "b.rs");
    }

    #[test]
    fn parse_unified_prefers_the_explicit_header_path_over_the_diff_git_split() {
        // A path containing the literal " b/" mis-splits the `diff --git`
        // line (first-occurrence split) — the `+++ b/` header line is
        // authoritative.
        let diff = "\
diff --git a/lib b/x.rs b/lib b/x.rs
index 1111111..2222222 100644
--- a/lib b/x.rs
+++ b/lib b/x.rs
@@ -1,1 +1,1 @@
-old
+new
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "lib b/x.rs");
    }

    #[test]
    fn parse_unified_deletion_takes_the_current_path_from_the_minus_header() {
        // For a deletion the `+++` side is `/dev/null`; the `--- a/` line
        // carries the name, and must win over the `diff --git` split when the
        // path contains " b/".
        let diff = "\
diff --git a/lib b/x.rs b/lib b/x.rs
deleted file mode 100644
index 1111111..0000000
--- a/lib b/x.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-gone
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "lib b/x.rs");
        assert_eq!(files[0].status, FileStatus::Deleted);
    }

    #[test]
    fn parse_unified_rename_with_content_change() {
        let diff = "\
diff --git a/old.txt b/new.txt
similarity index 59%
rename from old.txt
rename to new.txt
index e4a1e5d..b77e620 100644
--- a/old.txt
+++ b/new.txt
@@ -3,3 +3,4 @@ b
 c
 d
 modtest
+more
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(
            files[0].status,
            FileStatus::Renamed {
                from: "old.txt".to_string()
            }
        );
        assert_eq!(files[0].hunks.len(), 1);
    }

    #[test]
    fn parse_unified_pure_rename_no_hunks() {
        // A 100%-similarity rename emits no `---`/`+++`/hunks at all.
        let diff = "\
diff --git a/old.txt b/new.txt
similarity index 100%
rename from old.txt
rename to new.txt
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].status,
            FileStatus::Renamed {
                from: "old.txt".to_string()
            }
        );
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn parse_unified_binary_marker() {
        let diff = "\
diff --git a/bin.dat b/bin.dat
index 91652c9..eb428da 100644
Binary files a/bin.dat and b/bin.dat differ
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "bin.dat");
        assert!(files[0].binary);
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn parse_unified_added_and_deleted_files() {
        let diff = "\
diff --git a/added.txt b/added.txt
new file mode 100644
index 0000000..ce01362
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,1 @@
+hello
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index ce01362..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-hello
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, FileStatus::Added);
        assert_eq!(files[1].status, FileStatus::Deleted);
    }

    #[test]
    fn parse_unified_no_newline_at_eof_marker_is_consumed_not_a_line() {
        let diff = "\
diff --git a/nn.txt b/nn.txt
index 0a207c0..2d3cebd 100644
--- a/nn.txt
+++ b/nn.txt
@@ -1,2 +1,2 @@
 a
-b
\\ No newline at end of file
+bX
\\ No newline at end of file
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].hunks[0].lines,
            vec![
                (' ', "a".to_string()),
                ('-', "b".to_string()),
                ('+', "bX".to_string()),
            ]
        );
    }

    /// FIX 3 / V13 code review: a genuinely empty line in the hunk body
    /// (`bl` is a 0-byte string, not `" "` with a space marker) used to panic
    /// — `bl.chars().next().unwrap_or(' ')` falls back to `' '`, then
    /// `bl[marker.len_utf8()..]` slices `[1..]` on a 0-byte string, which is
    /// out of bounds. This violated `parse_unified`'s documented "never
    /// panics" contract; this test crashes the test binary against the
    /// pre-fix code and passes cleanly against the fix.
    #[test]
    fn parse_unified_empty_line_in_hunk_body_does_not_panic() {
        let diff =
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ a/a.rs\n@@ -1,3 +1,3 @@\n line1\n\n+line2\n";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        // The blank line is treated as an empty context line rather than
        // dropped or misparsed as something else.
        assert!(files[0].hunks[0]
            .lines
            .iter()
            .any(|(m, t)| *m == ' ' && t.is_empty()));
    }

    #[test]
    fn parse_unified_ignores_content_line_that_looks_like_a_header() {
        let diff = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,0 +2,2 @@
+++ looks like a header but isn't
+real content
";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!(
            files[0].hunks[0].lines[0],
            ('+', "++ looks like a header but isn't".to_string())
        );
    }

    // ── file_diff_meta_from_parsed (FIX 7 shadow-repo fallback) ─────────

    #[test]
    fn file_diff_meta_from_parsed_counts_added_and_removed_lines() {
        let diff = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,3 +1,3 @@
 line1
-line2
+line2X
+line2Y
 line3
";
        let files = parse_unified(diff);
        let meta = file_diff_meta_from_parsed(&files[0]);
        assert_eq!(meta.path, "a.rs");
        assert_eq!(meta.added, 2);
        assert_eq!(meta.removed, 1);
        assert!(!meta.binary);
        assert!(!meta.too_large);
    }

    #[test]
    fn file_diff_meta_from_parsed_zeroes_counts_when_too_large() {
        let mut file = FileDiff {
            path: "big.txt".into(),
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![('-', "old".into()), ('+', "new".into())],
                no_newline_at: Vec::new(),
                hash: String::new(),
            }],
            too_large: true,
        };
        let meta = file_diff_meta_from_parsed(&file);
        assert_eq!(meta.added, 0);
        assert_eq!(meta.removed, 0);
        file.too_large = false;
        let meta2 = file_diff_meta_from_parsed(&file);
        assert_eq!(meta2.added, 1);
        assert_eq!(meta2.removed, 1);
    }

    // ── hunk_hash / build_hunk_patch ────────────────────────────────────

    #[test]
    fn hunk_hash_changes_when_content_changes_stable_otherwise() {
        let h1 = Hunk {
            header: "@@ -1,1 +1,1 @@".into(),
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            lines: vec![('-', "a".into()), ('+', "b".into())],
            no_newline_at: vec![],
            hash: String::new(),
        };
        let h1_again = h1.clone();
        let mut h2 = h1.clone();
        h2.lines[1] = ('+', "c".into());
        assert_eq!(hunk_hash(&h1), hunk_hash(&h1_again));
        assert_ne!(hunk_hash(&h1), hunk_hash(&h2));
    }

    #[test]
    fn parse_unified_preserves_trailing_cr_in_content_lines() {
        // H1: CRLF content reaches the parser as ` line\r` / `-line\r`; the
        // `\r` must be kept in `Hunk::lines` so `build_hunk_patch` can rebuild
        // a `\r\n` patch. Structural lines (`@@ `, `--- `, `diff --git`) are
        // LF-only and must NOT carry a `\r`.
        let diff = "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n context\r\n-old\r\n+new\r\n";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(
            files[0].hunks[0].lines,
            vec![
                (' ', "context\r".to_string()),
                ('-', "old\r".to_string()),
                ('+', "new\r".to_string()),
            ]
        );
        // The rebuilt patch reproduces the `\r\n` terminators verbatim.
        let patch = String::from_utf8(build_hunk_patch(&files[0], &files[0].hunks[0])).unwrap();
        assert!(
            patch.ends_with(" context\r\n-old\r\n+new\r\n"),
            "patch: {patch:?}"
        );
    }

    #[test]
    fn parse_unified_records_and_rebuilds_no_newline_marker() {
        // L5: a hunk whose final `-`/`+` lines lack a trailing newline must
        // record the marker and reproduce it in the rebuilt patch, so a revert
        // doesn't newline-terminate a file that had no terminator.
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
        let files = parse_unified(diff);
        assert_eq!(files.len(), 1);
        let hunk = &files[0].hunks[0];
        // Both body lines (indices 0 and 1) are unterminated.
        assert_eq!(hunk.no_newline_at, vec![0, 1]);
        let patch = String::from_utf8(build_hunk_patch(&files[0], hunk)).unwrap();
        assert!(
            patch.ends_with(
                "-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n"
            ),
            "patch: {patch:?}"
        );
    }

    #[test]
    fn parse_status_z_add_then_delete_is_deleted_and_empty_path_skipped() {
        // L7: `AD` (staged add, deleted on disk) must classify as Deleted, not
        // Added; a pathless 3-byte `"XY "` record must be skipped.
        let raw = "AD gone.txt\0 M \0 M kept.txt\0";
        let entries = parse_status_z(raw);
        assert_eq!(
            entries.len(),
            2,
            "the empty-path ` M ` record must be dropped"
        );
        assert_eq!(entries[0].path, "gone.txt");
        assert_eq!(entries[0].status, FileStatus::Deleted);
        assert_eq!(entries[1].path, "kept.txt");
        assert_eq!(entries[1].status, FileStatus::Modified);
    }

    #[test]
    fn build_hunk_patch_modified_file_shape() {
        let file = FileDiff {
            path: "f.txt".into(),
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![],
            too_large: false,
        };
        let hunk = Hunk {
            header: "x".into(),
            old_start: 2,
            old_lines: 1,
            new_start: 2,
            new_lines: 1,
            lines: vec![('-', "old".into()), ('+', "new".into())],
            no_newline_at: vec![],
            hash: String::new(),
        };
        let patch = String::from_utf8(build_hunk_patch(&file, &hunk)).unwrap();
        assert!(patch.starts_with("--- a/f.txt\n+++ b/f.txt\n@@ -2,1 +2,1 @@\n-old\n+new\n"));
    }

    #[test]
    fn build_hunk_patch_added_file_routes_old_side_through_dev_null() {
        let file = FileDiff {
            path: "new.txt".into(),
            status: FileStatus::Added,
            binary: false,
            hunks: vec![],
            too_large: false,
        };
        let hunk = Hunk {
            header: "x".into(),
            old_start: 0,
            old_lines: 0,
            new_start: 1,
            new_lines: 1,
            lines: vec![('+', "hi".into())],
            no_newline_at: vec![],
            hash: String::new(),
        };
        let patch = String::from_utf8(build_hunk_patch(&file, &hunk)).unwrap();
        assert!(patch.starts_with("--- /dev/null\n+++ b/new.txt\n"));
    }

    // ── parse_status_z / parse_numstat_z ────────────────────────────────

    #[test]
    fn parse_status_z_handles_rename_added_deleted_untracked() {
        let raw = "A  added.txt\0RM new.txt\0old.txt\0?? u.txt\0 D gone.txt\0";
        let entries = parse_status_z(raw);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].path, "added.txt");
        assert_eq!(entries[0].status, FileStatus::Added);
        assert_eq!(entries[1].path, "new.txt");
        assert_eq!(
            entries[1].status,
            FileStatus::Renamed {
                from: "old.txt".to_string()
            }
        );
        assert_eq!(entries[2].path, "u.txt");
        assert_eq!(entries[2].status, FileStatus::Untracked);
        assert_eq!(entries[3].path, "gone.txt");
        assert_eq!(entries[3].status, FileStatus::Deleted);
    }

    #[test]
    fn parse_numstat_z_plain_and_binary_and_rename() {
        let raw =
            "0\t1\tadded.txt\0-\t-\tbin.dat\01\t1\tnonewline.txt\02\t0\t\0new.txt\0renamed.txt\0";
        let map = parse_numstat_z(raw);
        assert_eq!(map.get("added.txt"), Some(&(Some(0), Some(1))));
        assert_eq!(map.get("bin.dat"), Some(&(None, None)));
        assert_eq!(map.get("nonewline.txt"), Some(&(Some(1), Some(1))));
        assert_eq!(map.get("renamed.txt"), Some(&(Some(2), Some(0))));
    }

    // ── summary / diff_file (tempdir integration, skip without git) ────

    #[tokio::test]
    async fn summary_non_git_root_has_null_source() {
        let dir = std::env::temp_dir().join(format!("wb-diff-nogit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = summary(&dir).await.expect("summary");
        assert!(s.files.is_empty());
        assert!(s.source.is_none());
        assert!(!s.readonly);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn summary_and_diff_file_modified_added_untracked() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("summary");
        std::fs::write(dir.join("tracked.txt"), "a\nb\nc\n").unwrap();
        git(&dir, &["add", "tracked.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("tracked.txt"), "a\nB\nc\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "hello\nworld\n").unwrap();

        let s = summary(&dir).await.expect("summary");
        assert_eq!(s.source, Some(DiffSource::Git));
        assert!(!s.readonly);
        let tracked = s
            .files
            .iter()
            .find(|f| f.path == "tracked.txt")
            .expect("tracked row");
        assert_eq!(tracked.status, FileStatus::Modified);
        assert_eq!(tracked.added, 1);
        assert_eq!(tracked.removed, 1);
        let fresh = s
            .files
            .iter()
            .find(|f| f.path == "fresh.txt")
            .expect("fresh row");
        assert_eq!(fresh.status, FileStatus::Untracked);
        assert_eq!(fresh.added, 2);

        let fd = diff_file(&dir, "tracked.txt")
            .await
            .expect("diff_file tracked");
        assert_eq!(fd.status, FileStatus::Modified);
        assert_eq!(fd.hunks.len(), 1);

        let fd2 = diff_file(&dir, "fresh.txt")
            .await
            .expect("diff_file untracked");
        assert_eq!(fd2.status, FileStatus::Untracked);
        assert_eq!(fd2.hunks.len(), 1);
        assert_eq!(fd2.hunks[0].lines.len(), 2);
        assert!(fd2.hunks[0].lines.iter().all(|(m, _)| *m == '+'));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn diff_file_rename_uses_both_paths_for_content_diff() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("rename");
        std::fs::write(dir.join("old.txt"), "a\nb\nc\n").unwrap();
        git(&dir, &["add", "old.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        git(&dir, &["mv", "old.txt", "new.txt"]);
        std::fs::write(dir.join("new.txt"), "a\nb\nc\nd\n").unwrap();

        let fd = diff_file(&dir, "new.txt").await.expect("diff_file rename");
        assert_eq!(
            fd.status,
            FileStatus::Renamed {
                from: "old.txt".to_string()
            }
        );
        // Content diff (not a from-scratch add): exactly the appended line,
        // not the whole file.
        assert_eq!(fd.hunks.len(), 1);
        let added: Vec<&str> = fd.hunks[0]
            .lines
            .iter()
            .filter(|(m, _)| *m == '+')
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(added, vec!["d"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn diff_file_binary_flags_binary_no_hunks() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("binary");
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        git(&dir, &["add", "bin.dat"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3, 4, 5]).unwrap();

        let fd = diff_file(&dir, "bin.dat").await.expect("diff_file binary");
        assert!(fd.binary);
        assert!(fd.hunks.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn too_large_file_is_flagged_not_diffed() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("toolarge");
        let big = vec![b'x'; (MAX_DIFF_FILE_BYTES + 1) as usize];
        std::fs::write(dir.join("big.txt"), &big).unwrap();

        let fd = diff_file(&dir, "big.txt")
            .await
            .expect("diff_file too_large");
        assert!(fd.too_large);
        assert!(fd.hunks.is_empty());

        let s = summary(&dir).await.expect("summary too_large");
        let row = s.files.iter().find(|f| f.path == "big.txt").expect("row");
        assert!(row.too_large);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn readonly_true_mid_merge_conflict() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("merge");
        // Name the initial branch explicitly (before the first commit) so
        // this doesn't depend on the host's `init.defaultBranch` config.
        git(&dir, &["checkout", "-qb", "trunk"]);
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        git(&dir, &["add", "f.txt"]);
        git(&dir, &["commit", "-q", "-m", "base"]);
        git(&dir, &["checkout", "-qb", "side"]);
        std::fs::write(dir.join("f.txt"), "side\n").unwrap();
        git(&dir, &["commit", "-qam", "side change"]);
        git(&dir, &["checkout", "-q", "trunk"]);
        std::fs::write(dir.join("f.txt"), "main\n").unwrap();
        git(&dir, &["commit", "-qam", "main change"]);
        // This merge conflicts (both sides touched the same line) and
        // leaves MERGE_HEAD set — exactly the state the guard detects.
        let _ = std::process::Command::new("git")
            .args(["merge", "side"])
            .current_dir(&dir)
            .output();

        let s = summary(&dir).await.expect("summary mid-merge");
        assert!(s.readonly);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
