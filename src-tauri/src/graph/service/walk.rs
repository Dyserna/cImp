//! The graph service's **free-function floor** — the tree walk, the language
//! census, the embed plumbing, and the warm-index open.
//!
//! Split out of [`super`] by V42 R6 (#117) as pure code motion. Unlike the two
//! subsystems beside it, this was never a state bucket: it is ~790 lines of
//! free functions with not one `&self` between them, which is precisely why it
//! was already testable against a bare `GraphIndex` and why moving it costs
//! nothing. [`GraphService`] calls in: `rebuild_blocking` → [`build_tree`],
//! `ignore_resync_blocking` → [`resync_tree`], `reindex_paths` →
//! [`index_dir_tree`], `embed_backfill` → [`embed_batch_isolated`],
//! `index_for` → [`warm_index`].
//!
//! Sixteen items are `pub(super)` because the parent calls them; the two that
//! were already `pub` ([`LangCensus`], [`RebuildOrigin`]) stay `pub` and are
//! re-exported from [`super`], so `graph::{LangCensus, RebuildOrigin}` is the
//! same path it has always been.

use super::*;

/// V12 Phase F (6c): pure gate for [`GraphService::run_analyses_trigger`] —
/// whether the freshly computed counts (`cur`, the `"{dead},{cycles}"` string)
/// differ from what was last stored (`prev`). Factored out so "the event
/// fires only when counts changed" is testable without a `GraphIndex`/
/// `AppHandle`. `None` (nothing stored yet) always counts as a change — the
/// first successful pass this project has ever run IS new information.
pub(super) fn analyses_changed(prev: Option<&str>, cur: &str) -> bool {
    prev != Some(cur)
}

/// V30 (review M2): who asked for a full rebuild
/// ([`GraphService::spawn_rebuild`]). The graph twin of the audit runner's
/// `Initiator` (`audit/runner.rs`): only a rebuild a human actually
/// requested may announce itself on the session-push bus, because delivering a
/// notice to an idle Claude tab **starts a model turn**. Everything automatic —
/// the startup build, the settings-enable watcher, the watcher's
/// channel-overflow recovery, the schema-migration repair, the moved-in-directory
/// escalation out of the incremental path — happens without anyone waiting, and
/// would otherwise start a turn in every armed tab on app launch or after a
/// large `git checkout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildOrigin {
    /// A user action: the Code Intelligence tab's Rebuild button
    /// (`graph_rebuild`) or a Settings language toggle
    /// (`graph_set_language_enabled`).
    User,
    /// Machinery: startup, runtime enable, watcher recovery, schema migration,
    /// incremental-walk escalation. Never pushes.
    Automatic,
}

/// V30 Phase C: the wall-clock floor for the index-completion push. Below this
/// the build was cheap enough that nobody was waiting on it, and the notice
/// would cost more (an idle Claude tab starts a model turn on delivery) than the
/// information is worth.
pub(super) const GRAPH_PUSH_MIN_BUILD_MS: u64 = 30_000;

/// V30 Phase C: the complete gate for
/// [`GraphService::announce_index_complete`] — pure so "only expensive builds a
/// user asked for, and only while the feature is on, announce themselves" is
/// testable without an `AppHandle`, a store, or a push bus. The
/// full-vs-incremental half is structural (only `spawn_rebuild` calls the
/// producer at all).
///
/// `session_push` comes from a LIVE settings read at fire time (review M6): the
/// child-side declaration is latched per tab until restart, so the producer is
/// the half that can make "off" mean off immediately.
pub(super) fn index_push_worthy(
    session_push: bool,
    origin: RebuildOrigin,
    elapsed_ms: u64,
) -> bool {
    session_push
        && matches!(origin, RebuildOrigin::User)
        && elapsed_ms >= GRAPH_PUSH_MIN_BUILD_MS
}

/// Make a `&str` `path` project-relative to `root` with `/` separators (empty in
/// → empty out). Delegates to [`rel_path`] so memory-event paths and the
/// indexer's stored file paths are relativized identically.
pub(super) fn relativize_path(root: &Path, path: &str) -> String {
    if path.trim().is_empty() {
        return String::new();
    }
    rel_path(root, Path::new(path))
}

/// Reset `idx` and re-index every supported file under `root`, honoring
/// gitignore (+ global/exclude) and the configured language/size filters.
/// Returns `(files_visited, final_stats)`. Free function (no `self`) so the
/// build is unit-testable against a bare [`GraphIndex`].
pub(super) fn build_tree(
    idx: &GraphIndex,
    root: &Path,
    snap: &GraphSettings,
    db_subdir: &str,
) -> AppResult<(u64, GraphStats)> {
    // A full rebuild starts clean so deleted files don't leave stale rows.
    idx.reset()?;

    let max_bytes = snap.max_file_bytes.max(1);
    // V11 Phase G: a simple project-wide count cap on `code_chunk` rows
    // (order-dependent on the walk order, which is acceptable for V1 — see
    // `GraphSettings::semantic_code_max_chunks`). Only enforced on a full
    // rebuild; the incremental watcher path doesn't re-check the running
    // total against the rest of the project.
    let mut code_chunk_budget = snap.semantic_code_max_chunks as usize;

    let mut indexed: u64 = 0;
    for entry in build_walker(root, snap) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                debug!(error = %e, "graph: walk entry error (skipped)");
                continue;
            }
        };
        if entry.file_type().map(|t| !t.is_file()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();

        // Never index our own store directory.
        if path.components().any(|c| c.as_os_str() == db_subdir) {
            continue;
        }

        let Some(lang) = lang_for(path, &snap.languages) else {
            continue;
        };
        // `index_docs` off → skip pure-doc (markdown) files; code doc-comments
        // still ride along with their symbols.
        if lang == Lang::Markdown && !snap.index_docs {
            continue;
        }

        // Size guard before reading.
        match entry.metadata() {
            Ok(m) if m.len() > max_bytes => continue,
            Ok(_) => {}
            Err(_) => continue,
        }

        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue, // binary / non-UTF-8 / unreadable — skip
        };

        let rel = rel_path(root, path);
        let mut fg = parse_file(&rel, &src, lang);
        if fg.code_chunks.len() > code_chunk_budget {
            fg.code_chunks.truncate(code_chunk_budget);
        }
        code_chunk_budget = code_chunk_budget.saturating_sub(fg.code_chunks.len());
        if let Err(e) = idx.index_file_graph(&fg) {
            warn!(file = %rel, error = %e, "graph: index_file_graph failed (skipped)");
            continue;
        }
        indexed += 1;
    }

    // `reset()` deliberately keeps the vector store (so unchanged chunks aren't
    // needlessly re-embedded), so vectors for files that vanished since the
    // last build are now orphans — drop them before reporting stats.
    let _ = idx.prune_orphan_vectors();
    let _ = idx.prune_orphan_code_vectors();
    // V11 Phase F: likewise drop cached digests for files that vanished.
    let _ = idx.prune_orphan_digests();

    // V12 Phase D: refresh git churn metadata for the ranking boost + digest
    // trailers. `commit_touch` is additive (outside `RELATIONS`, ensured by
    // `ensure_memory_relations`), so it survives the `reset()` above and just
    // gets repopulated here every full pass. `collect` itself degrades to an
    // empty vec (never an error) when `root` isn't a git repo, so this is
    // always safe to call — a non-git project just gets no churn boost.
    if let Ok(churn) = crate::graph::gitmeta::collect(root) {
        let _ = idx.put_commit_touches(&churn);
    }

    Ok((indexed, idx.stats()?))
}

/// The shared tree walker for a rebuild and the language census, so the two
/// agree exactly on what counts as "in the project" (gitignore + global +
/// exclude + parents, dotfiles included, plus the user's extra `ignore` globs).
/// The db-subdir and per-file size/language filtering are applied by callers.
///
/// V13 Phase D: the `<db_subdir>/` override below (default `.cimp/`) is
/// unconditional — not gated on the user's own `.gitignore` containing it —
/// so this walker never DESCENDS into it at all, rather than relying on
/// callers' post-hoc `path.components().any(|c| c.as_os_str() == db_subdir)`
/// filter alone. That filter is still correct (and kept, as defense in
/// depth), but a project that hasn't gitignored `.cimp/` would otherwise have
/// this walker step through the shadow checkpoint repo's whole object store
/// AND every worktree's full checkout under `.cimp/worktrees/<slug>/` on
/// every rebuild — the worktree case in particular can be as large as the
/// project itself, multiplied per open worktree.
fn build_walker(root: &Path, snap: &GraphSettings) -> ignore::Walk {
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false) // index dotfiles like `.github/*.md`; the db dir is filtered by callers
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true);
    // Honor the user's extra ignore globs (additive to `.gitignore`), plus
    // the always-on `<db_subdir>/` exclusion above. An `Override` whose
    // patterns are *ignore* globs needs each prefixed with `!` (overrides are
    // whitelists; a leading `!` flips one to a blacklist).
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    let _ = ob.add(&format!("!{}/", snap.effective_db_subdir()));
    for pat in &snap.ignore {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        let rule = if let Some(stripped) = pat.strip_prefix('!') {
            stripped.to_string() // already a re-include
        } else {
            format!("!{pat}") // ignore this glob
        };
        let _ = ob.add(&rule);
    }
    if let Ok(ov) = ob.build() {
        wb.overrides(ov);
    }
    wb.build()
}

/// Reconcile the store with the CURRENT `graph.ignore` globs without a full
/// rebuild: drop every indexed file the globs now exclude, then (hash-skip)
/// index every eligible file they no longer exclude. Unlike [`build_tree`]
/// there's no `reset()`, so unchanged files keep their rows and vectors.
/// Returns `(removed, added, added_rels)`.
///
/// Two passes because they answer different questions: the walker below can
/// only visit files that exist OUTSIDE ignored trees — it can never say "this
/// stored row is now ignored" — so pass 1 tests each stored path against the
/// glob matcher directly, and pass 2 is the same walk as a full rebuild
/// (which honors the new globs via its overrides) with a stored-hash check so
/// an already-indexed unchanged file costs one read+hash, not a re-parse.
pub(super) fn resync_tree(
    idx: &GraphIndex,
    root: &Path,
    snap: &GraphSettings,
    db_subdir: &str,
) -> AppResult<(u64, u64, Vec<String>)> {
    let matcher = gitignore_from_globs(root, &snap.ignore);
    let mut removed = 0u64;
    for rel in idx.all_file_paths()? {
        let abs = root.join(&rel);
        if matcher.matched_path_or_any_parents(&abs, false).is_ignore()
            && idx.remove_file(&rel).is_ok()
        {
            removed += 1;
        }
    }

    let max_bytes = snap.max_file_bytes.max(1);
    let mut added = 0u64;
    let mut added_rels: Vec<String> = Vec::new();
    for entry in build_walker(root, snap) {
        let Ok(entry) = entry else { continue };
        if entry.file_type().map(|t| !t.is_file()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        if path.components().any(|c| c.as_os_str() == db_subdir) {
            continue;
        }
        let Some(lang) = lang_for(path, &snap.languages) else {
            continue;
        };
        if lang == Lang::Markdown && !snap.index_docs {
            continue;
        }
        match entry.metadata() {
            Ok(m) if m.len() > max_bytes => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = rel_path(root, path);
        if idx.stored_file_hash(&rel).ok().flatten().as_deref()
            == Some(crate::graph::model::fnv1a_hex(&src).as_str())
        {
            continue;
        }
        let fg = parse_file(&rel, &src, lang);
        if let Err(e) = idx.index_file_graph(&fg) {
            debug!(file = %rel, error = %e, "graph: ignore-resync index failed (skipped)");
            continue;
        }
        added += 1;
        added_rels.push(rel);
    }

    if removed > 0 || added > 0 {
        let _ = idx.prune_orphan_vectors();
        let _ = idx.prune_orphan_code_vectors();
        let _ = idx.prune_orphan_digests();
    }
    Ok((removed, added, added_rels))
}

/// Outcome of [`index_dir_tree`] for one moved-in/created directory.
pub(super) enum DirWalk {
    /// Walked and indexed inline: how many files changed, and their rel paths
    /// (for the caller's incremental churn refresh).
    Indexed { indexed: u64, rels: Vec<String> },
    /// The subtree holds more eligible files than the incremental cap; the
    /// caller should fall back to a full rebuild.
    TooBig,
}

/// Index every eligible file under `dir` (a directory that just appeared in a
/// watcher batch). Windows reports an atomic directory rename as one
/// dir-level OLD/NEW event pair — the children are never re-reported — so
/// without this walk a renamed/moved-in subtree would stay missing from the
/// graph until an unrelated full rebuild. Uses the same tree walker as a full
/// rebuild (gitignore + user ignore globs + db-subdir exclusion), the same
/// language/size gates as the per-file incremental path, and skips children
/// whose stored content hash already matches (e.g. their own file events
/// landed in the same batch), so a redundant directory event costs one
/// read+hash per child, not a re-parse.
///
/// `gi` must cover `dir` itself: the walker below STARTS at `dir`, and the
/// `ignore` crate never matches a walk root against ignore rules — so a
/// gitignore rule that excludes the directory (e.g. `dist` written by a
/// frontend build) would silently not fire, and the whole artifact subtree
/// (minified bundles included) would be parsed into the graph. That exact
/// leak polluted this repo's own graph with `dist/assets/*.js`.
pub(super) fn index_dir_tree(
    idx: &GraphIndex,
    root: &Path,
    dir: &Path,
    snap: &GraphSettings,
    sub: &str,
    max_bytes: u64,
    gi: &Gitignore,
) -> DirWalk {
    const MAX_DIR_WALK: usize = 4096;
    if gi.matched_path_or_any_parents(dir, true).is_ignore() {
        return DirWalk::Indexed {
            indexed: 0,
            rels: Vec::new(),
        };
    }
    let mut eligible = 0usize;
    let mut indexed = 0u64;
    let mut rels: Vec<String> = Vec::new();
    for entry in build_walker(dir, snap) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let child = entry.into_path();
        if child.components().any(|c| c.as_os_str() == sub) {
            continue;
        }
        let Some(lang) = lang_for(&child, &snap.languages) else {
            continue;
        };
        if lang == Lang::Markdown && !snap.index_docs {
            continue;
        }
        match std::fs::metadata(&child) {
            Ok(m) if m.len() > max_bytes => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        eligible += 1;
        if eligible > MAX_DIR_WALK {
            return DirWalk::TooBig;
        }
        let src = match std::fs::read_to_string(&child) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = rel_path(root, &child);
        if idx.stored_file_hash(&rel).ok().flatten().as_deref()
            == Some(crate::graph::model::fnv1a_hex(&src).as_str())
        {
            continue;
        }
        let fg = parse_file(&rel, &src, lang);
        match idx.index_file_graph(&fg) {
            Ok(()) => {
                indexed += 1;
                rels.push(rel);
            }
            Err(e) => debug!(file = %rel, error = %e, "graph: incremental index failed"),
        }
    }
    DirWalk::Indexed { indexed, rels }
}

/// One row of the project **language census**: a language present on disk, how
/// many files it has, and how the graph relates to it. Drives the Code Graph
/// tab's green/yellow/red language buttons.
///
/// - `supported && enabled` → green (indexed by the graph).
/// - `supported && !enabled` → yellow (the engine can index it, but it isn't in
///   `GraphSettings.languages`).
/// - `!supported` → red (a known-but-unsupported programming language, or the
///   catch-all "other" bucket).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LangCensus {
    /// Stable key: a supported [`Lang`] tag (`"rust"`), a known-unsupported
    /// programming-language slug (`"zig"`), or `"other"` for the catch-all.
    pub key: String,
    /// Human display label (`"Rust"`, `"Zig"`, `"Other"`).
    pub label: String,
    /// Number of files of this language found in the project tree.
    pub files: u64,
    /// The graph engine can index this language (a concrete `Lang` variant).
    pub supported: bool,
    /// The language's tag is currently in `GraphSettings.languages`.
    pub enabled: bool,
}

/// Group-and-files sort rank for the census: green (0) → yellow (1) → red-known
/// (2) → the "other" bucket (3, always last).
fn census_rank(e: &LangCensus) -> u8 {
    if e.key == "other" {
        3
    } else if !e.supported {
        2
    } else if e.enabled {
        0
    } else {
        1
    }
}

/// Walk `root` and tally every source file by detected language, *without* the
/// `languages` allowlist filter — so the result includes supported-but-not-
/// indexed languages (yellow) and unsupported ones (red), which the indexed
/// `file` relation never records. Reuses [`build_walker`] so the file set
/// matches a rebuild's exactly. Best-effort and non-fatal: unreadable entries
/// are skipped, never surfaced as errors.
pub(super) fn language_census(
    root: &Path,
    snap: &GraphSettings,
    db_subdir: &str,
) -> Vec<LangCensus> {
    use std::collections::BTreeMap;

    let mut supported: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut known: BTreeMap<&'static str, (&'static str, u64)> = BTreeMap::new();
    let mut other: u64 = 0;

    for entry in build_walker(root, snap) {
        let Ok(entry) = entry else { continue };
        if entry.file_type().map(|t| !t.is_file()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        // Never count our own store directory.
        if path.components().any(|c| c.as_os_str() == db_subdir) {
            continue;
        }
        let lang = Lang::from_path(path);
        if lang != Lang::Other {
            *supported.entry(lang.tag()).or_default() += 1;
        } else if let Some((slug, label)) = crate::graph::model::unsupported_lang_name(path) {
            let e = known.entry(slug).or_insert((label, 0));
            e.1 += 1;
        } else {
            other += 1;
        }
    }

    let langs = &snap.languages;
    let mut out: Vec<LangCensus> = Vec::new();
    for (tag, files) in supported {
        out.push(LangCensus {
            key: tag.to_string(),
            label: Lang::from_tag(tag).label().to_string(),
            files,
            supported: true,
            enabled: langs.iter().any(|l| l == tag),
        });
    }
    for (slug, (label, files)) in known {
        out.push(LangCensus {
            key: slug.to_string(),
            label: label.to_string(),
            files,
            supported: false,
            enabled: false,
        });
    }
    if other > 0 {
        out.push(LangCensus {
            key: "other".to_string(),
            label: "Other".to_string(),
            files: other,
            supported: false,
            enabled: false,
        });
    }

    out.sort_by(|a, b| {
        census_rank(a)
            .cmp(&census_rank(b))
            .then_with(|| b.files.cmp(&a.files))
            .then_with(|| a.label.cmp(&b.label))
    });
    out
}

/// Project-relative path with forward slashes, matching what the parser stores
/// and the MCP tools query against. The fs walk always strips cleanly; the
/// case-insensitive fallback exists for agent-supplied absolute paths (memory
/// events) that on Windows can differ from `root` only in drive/dir case — the
/// tail keeps its original casing, which matches the indexed file. Returns the
/// forward-slashed path unchanged when it isn't under `root`.
pub(super) fn rel_path(root: &Path, path: &Path) -> String {
    // Fast, exact path (always taken by the indexer's own walk).
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    // Case-insensitive root-prefix strip.
    let path_s = path.to_string_lossy().replace('\\', "/");
    let root_s = root.to_string_lossy().replace('\\', "/");
    let root_trim = root_s.trim_end_matches('/');
    let rl = root_trim.len();
    if !root_trim.is_empty()
        && path_s.len() > rl
        && path_s.is_char_boundary(rl)
        && path_s.as_bytes()[rl] == b'/'
        && path_s[..rl].eq_ignore_ascii_case(root_trim)
    {
        return path_s[rl + 1..].to_string();
    }
    path_s
}

/// The embedding "epoch" fingerprint — a vector is only comparable to others
/// sharing its `{model, dim, schema}`. A change to any of these bumps the
/// epoch, scoping k-NN to matching vectors and triggering a background
/// re-embed. Kept short and human-glanceable (model + dim + a schema tag).
pub(super) fn embedding_epoch(model: &str, dim: usize) -> String {
    let m = model.trim();
    let m = if m.is_empty() { "default" } else { m };
    format!("{m}|{dim}|{EMBED_SCHEMA}")
}

/// Embed one batch with **per-item failure isolation**, shared by the doc and
/// code backfill loops.
///
/// The failure this exists for: one chunk the server refuses (typically
/// oversized) fails the *whole* batch with a non-2xx, and because the same
/// chunk is re-selected on the next pass, embedding stalls permanently. So a
/// batch failure is never fatal on its own — the items are retried one at a
/// time, and only the individual offender is dropped.
///
/// Returns the vectors that DID embed (possibly fewer than `pending`, possibly
/// none) and grows `skipped` with the chunk ids that were given up on.
/// `Err` is reserved for failures that mean the endpoint is gone or the model
/// behind it changed ([`embed::is_item_level_error`] draws the line) — those
/// must still degrade-and-stop, because retrying per item would fail
/// identically for every item and misreport an outage as skipped chunks.
pub(super) async fn embed_batch_isolated(
    embedder: &mut Embedder,
    pending: &[(String, String, String)],
    skipped: &mut HashSet<String>,
) -> Result<Vec<(String, String, Vec<f32>)>, String> {
    let texts: Vec<String> = pending.iter().map(|(_, _, t)| t.clone()).collect();
    match embedder.embed(&texts).await {
        Ok(vectors) if vectors.len() == pending.len() => {
            return Ok(pending
                .iter()
                .zip(vectors)
                .map(|((id, hash, _), v)| (id.clone(), hash.clone(), v))
                .collect());
        }
        // `embed` already guarantees the count matches, so this is defensive:
        // treat a short response like any other per-request rejection.
        Ok(_) => {}
        Err(e) if !embed::is_item_level_error(&e) => return Err(e),
        Err(e) => {
            debug!(error = %e, items = pending.len(), "embed batch rejected — retrying per item");
        }
    }
    let mut rows = Vec::with_capacity(pending.len());
    for (id, hash, text) in pending {
        match embed_item_isolated(embedder, text).await {
            ItemOutcome::Ok(v) => rows.push((id.clone(), hash.clone(), v)),
            ItemOutcome::Down(e) => return Err(e),
            ItemOutcome::Skip(e) => {
                warn!(chunk = %id, error = %e, "embedder rejected chunk — skipping it this run");
                skipped.insert(id.clone());
            }
        }
    }
    Ok(rows)
}

/// What happened to one isolated item.
enum ItemOutcome {
    Ok(Vec<f32>),
    /// The endpoint (not the item) is the problem — abort the run.
    Down(String),
    /// The server refuses this item at any size we're willing to try.
    Skip(String),
}

/// Embed a single item, halving the token budget on failure down to
/// [`embed::MIN_TOKEN_LIMIT`].
///
/// Why shrink at all: `/props` reports `n_ctx`, but a llama-server's real
/// per-request bound for *pooled* embeddings can be the physical batch size
/// (`n_ubatch`), which `/props` does not report. Detection can therefore
/// overestimate, and the only way to find the true bound is to measure it. A
/// size that works is fed back via `lower_max_tokens`, so the run (and every
/// later handle in this process) self-heals to the real bound instead of
/// repeating the search for every item.
async fn embed_item_isolated(embedder: &mut Embedder, text: &str) -> ItemOutcome {
    let input = [text.to_string()];
    let first = match embedder.embed(&input).await {
        Ok(mut v) if v.len() == 1 => return ItemOutcome::Ok(v.pop().unwrap_or_default()),
        Ok(_) => "empty embedding response".to_string(),
        Err(e) if !embed::is_item_level_error(&e) => return ItemOutcome::Down(e),
        Err(e) => e,
    };
    // Nothing to shrink against (no detected window, no override): the server
    // dislikes this item for a reason we can't act on.
    let Some(start) = embedder.max_tokens() else {
        return ItemOutcome::Skip(first);
    };
    let mut limit = start;
    let mut last = first;
    while limit > embed::MIN_TOKEN_LIMIT {
        limit = (limit / 2).max(embed::MIN_TOKEN_LIMIT);
        // Trial on a clone so a failed attempt can't shrink the run's budget.
        let mut trial = embedder.clone();
        trial.set_max_tokens(limit);
        match trial.embed(&input).await {
            Ok(mut v) if v.len() == 1 => {
                embedder.lower_max_tokens(limit);
                return ItemOutcome::Ok(v.pop().unwrap_or_default());
            }
            Ok(_) => last = "empty embedding response".to_string(),
            Err(e) if !embed::is_item_level_error(&e) => return ItemOutcome::Down(e),
            Err(e) => last = e,
        }
    }
    ItemOutcome::Skip(last)
}

/// The indexable language for `path`, or `None` if its extension is unknown or
/// not in the configured `languages`. Shared by the full walk and the watcher
/// so they agree on what's in scope.
pub(super) fn lang_for(path: &Path, languages: &[String]) -> Option<Lang> {
    let lang = Lang::from_path(path);
    if lang == Lang::Other || !languages.iter().any(|l| l == lang.tag()) {
        None
    } else {
        Some(lang)
    }
}

/// Build a gitignore matcher for per-path filtering in the watcher (the full
/// walk gets this for free via `WalkBuilder`). Merges every `.gitignore` from
/// `root` down to each changed path's directory so the watcher agrees with the
/// full walk on nested ignores — a subdirectory `.gitignore` (e.g.
/// `src/gen/.gitignore`) is honored, not just the root one. Only the dirs
/// touched by this batch are scanned, so it stays cheap. An empty matcher
/// (missing/invalid files) simply ignores nothing.
///
/// `extra` is the settings `graph.ignore` globs, appended AFTER the
/// `.gitignore` files (so, per last-match-wins, they take precedence).
/// Without them the watcher path disagreed with `build_walker`: a file
/// excluded only by the settings globs was re-indexed on its next save,
/// silently undoing the exclusion until the next full rebuild.
pub(super) fn build_gitignore(root: &Path, paths: &[PathBuf], extra: &[String]) -> Gitignore {
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    let mut dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    dirs.insert(root.to_path_buf());
    for p in paths {
        for anc in p.ancestors() {
            if !anc.starts_with(root) {
                break;
            }
            if anc.is_dir() {
                dirs.insert(anc.to_path_buf());
            }
            if anc == root {
                break;
            }
        }
    }
    for dir in dirs {
        let gi = dir.join(".gitignore");
        if gi.is_file() {
            let _ = b.add(gi);
        }
    }
    for pat in extra {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        let _ = b.add_line(None, pat);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// A gitignore-semantics matcher built from the settings `graph.ignore` globs
/// alone (rooted at `root`) — the same lines `build_walker` feeds its
/// overrides, so the resync drop-pass and the walk agree on what's excluded.
/// `!` re-includes work natively (whitelist lines); invalid or empty globs are
/// skipped like everywhere else.
fn gitignore_from_globs(root: &Path, globs: &[String]) -> Gitignore {
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    for pat in globs {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        let _ = b.add_line(None, pat);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The warm-handle cache core of [`GraphService::index_for`], free of the
/// `AppHandle` so the keying invariant is directly testable. Returns the
/// cached-or-freshly-opened handle plus whether THIS open migrated a stale
/// store. The lock is held across the whole check-open-insert (see
/// `index_for`'s doc for why), and the canonicalized root is used for both the
/// key and the open so one SQLite file never backs two cozo storages.
pub(super) fn warm_index(
    indices: &StdMutex<HashMap<PathBuf, Arc<GraphIndex>>>,
    root: &Path,
    db_subdir: &str,
) -> AppResult<(Arc<GraphIndex>, bool)> {
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut guard = indices.lock().unwrap();
    if let Some(idx) = guard.get(&key).cloned() {
        return Ok((idx, false));
    }
    let idx = Arc::new(GraphIndex::open(&key, db_subdir)?);
    // Read (once) whether this open had to reset a stale-schema store.
    let migrated = idx.take_schema_reset();
    guard.insert(key, idx.clone());
    Ok((idx, migrated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::GraphSettings;

    /// One project dir reached under two spellings must yield the SAME warm
    /// handle. The loopback canonicalizes its root (`\\?\P:\…` on Windows)
    /// while IPC and the taps pass the plain spelling; keying the cache by the
    /// raw `PathBuf` opened a second cozo storage over the same `graph.db` in
    /// one process, with independent locks — the flap this guards against.
    #[test]
    fn one_root_two_spellings_share_one_warm_handle() {
        let dir = std::env::temp_dir().join(format!("ckg-key-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let indices = StdMutex::new(HashMap::new());
        let sub = ".ckg-test";

        // The plain spelling, then a second `PathBuf` that names the SAME
        // directory. On Windows that second spelling is the canonicalized
        // verbatim `\\?\P:\…` form — the production case in the doc above.
        //
        // It cannot be `canonicalize` everywhere: on Linux `temp_dir()` is
        // `/tmp`, a real directory with no symlinks to resolve, so
        // `canonicalize` returns the path UNCHANGED and the `assert_ne!` below
        // fails on the fixture rather than on the behaviour.
        //
        // Nor can the second spelling be `dir.join(".")`: `Path`'s `PartialEq`
        // compares `components()`, and `Components` DISCARDS `CurDir`, so
        // `/tmp/x/.` compares EQUAL to `/tmp/x` and the precondition fails.
        // (That spelling was the first Linux fix here and CI caught it — the
        // trap is that the same normalization makes it look right in a REPL.)
        // A `..` step off a real subdirectory is preserved by `components()`,
        // so the two `PathBuf`s genuinely differ, and `warm_index`'s own
        // `canonicalize` folds it back to `dir` — which is the property under
        // test. The subdirectory must EXIST: `canonicalize` resolves the whole
        // prefix, so `..` off a missing component fails outright.
        let (plain, _) = warm_index(&indices, &dir, sub).expect("open plain");
        let other_spelling = if cfg!(windows) {
            std::fs::canonicalize(&dir).expect("canonicalize")
        } else {
            std::fs::create_dir_all(dir.join("step")).unwrap();
            dir.join("step").join("..")
        };
        assert_ne!(other_spelling, dir, "the two spellings must actually differ");
        let (verbatim, _) = warm_index(&indices, &other_spelling, sub).expect("open canonical");

        assert!(Arc::ptr_eq(&plain, &verbatim), "one file, one handle");
        assert_eq!(indices.lock().unwrap().len(), 1, "one cache entry");

        drop(plain);
        drop(verbatim);
        indices.lock().unwrap().clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A full rebuild over a tiny on-disk Rust project: the store ends up with
    /// the file's symbols, deleted files don't survive a second build, and the
    /// db dir itself is never indexed. Drives the free `build_tree` core
    /// directly, so no `AppHandle`/`SettingsHandle` is needed.
    #[test]
    fn rebuild_indexes_tree_and_prunes_deleted() {
        let dir = std::env::temp_dir().join(format!("ckg-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "/// Doc.\npub fn alpha() -> i32 { beta() }\nfn beta() -> i32 { 1 }\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/extra.rs"), "pub fn gamma() {}\n").unwrap();

        // Distinct subdir so the test never touches a real `.cimp`.
        let sub = ".ckg-test";
        let snap = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");

        let (visited, stats) = build_tree(&idx, &dir, &snap, sub).expect("rebuild");
        assert_eq!(visited, 2);
        assert!(stats.symbols >= 3, "alpha/beta/gamma at least: {stats:?}");
        assert_eq!(stats.files, 2);

        // The index can answer a lookup against the freshly built store, and
        // the db dir itself was excluded (only the 2 source files counted).
        assert!(idx
            .find_symbol("alpha")
            .unwrap()
            .iter()
            .any(|s| s.name == "alpha"));

        // Delete one file and rebuild: its rows must be gone (reset prunes).
        std::fs::remove_file(dir.join("src/extra.rs")).unwrap();
        let (_, stats2) = build_tree(&idx, &dir, &snap, sub).expect("rebuild2");
        assert_eq!(stats2.files, 1);
        assert!(idx.find_symbol("gamma").unwrap().is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `graph.ignore` resync (a Settings edit) in both directions: adding
    /// a glob drops the matching indexed file WITHOUT a reset (the untouched
    /// neighbor's rows survive), removing the glob indexes the file again —
    /// and only it, since the hash-skip spares the unchanged neighbor.
    #[test]
    fn ignore_resync_drops_and_restores() {
        let dir = std::env::temp_dir().join(format!("ckg-ign-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("gen")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("gen/out.rs"), "pub fn generated() {}\n").unwrap();

        let sub = ".ckg-test";
        let mut snap = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");
        build_tree(&idx, &dir, &snap, sub).expect("build");
        assert!(!idx.find_symbol("generated").unwrap().is_empty());

        // Ignore `/gen/`: its file's rows drop, the neighbor's survive.
        snap.ignore = vec!["/gen/".to_string()];
        let (removed, added, _) = resync_tree(&idx, &dir, &snap, sub).expect("resync drop");
        assert_eq!((removed, added), (1, 0));
        assert!(idx.find_symbol("generated").unwrap().is_empty());
        assert!(!idx.find_symbol("alpha").unwrap().is_empty());

        // Un-ignore: the file is indexed again — and ONLY it (hash-skip).
        snap.ignore.clear();
        let (removed2, added2, rels) = resync_tree(&idx, &dir, &snap, sub).expect("resync add");
        assert_eq!((removed2, added2), (0, 1));
        assert_eq!(rels, vec!["gen/out.rs".to_string()]);
        assert!(!idx.find_symbol("generated").unwrap().is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression for the directory-rename staleness bug: a moved-in directory
    /// arrives from the watcher as a single dir-level path (Windows never
    /// re-reports the children), and used to be a silent no-op — the subtree
    /// stayed missing until an unrelated full rebuild. `index_dir_tree` must
    /// walk and index it, and a second pass must skip unchanged children.
    #[test]
    fn moved_in_directory_is_walked_and_indexed() {
        let dir = std::env::temp_dir().join(format!("ckg-mvdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();

        let sub = ".ckg-test";
        let snap = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");
        build_tree(&idx, &dir, &snap, sub).expect("initial build");
        assert!(!idx.find_symbol("alpha").unwrap().is_empty());

        // Simulate `mv src srcnew`: the watcher batch carries only the two
        // directory paths — the removal branch drops the old side...
        std::fs::rename(dir.join("src"), dir.join("srcnew")).unwrap();
        idx.remove_files_under("src").expect("remove old side");
        assert!(idx.find_symbol("alpha").unwrap().is_empty());

        // ...and the walk must index the new side.
        match index_dir_tree(
            &idx,
            &dir,
            &dir.join("srcnew"),
            &snap,
            sub,
            u64::MAX,
            &Gitignore::empty(),
        ) {
            DirWalk::Indexed { indexed, rels } => {
                assert_eq!(indexed, 1, "one child file indexed");
                assert_eq!(rels, vec!["srcnew/lib.rs".to_string()]);
            }
            DirWalk::TooBig => panic!("one file is not too big"),
        }
        let hits = idx.find_symbol("alpha").unwrap();
        assert!(
            hits.iter().any(|s| s.file == "srcnew/lib.rs"),
            "alpha lives under the new directory: {hits:?}"
        );

        // Idempotence: unchanged children are hash-skipped on a repeat event.
        match index_dir_tree(
            &idx,
            &dir,
            &dir.join("srcnew"),
            &snap,
            sub,
            u64::MAX,
            &Gitignore::empty(),
        ) {
            DirWalk::Indexed { indexed, .. } => assert_eq!(indexed, 0, "nothing re-indexed"),
            DirWalk::TooBig => panic!("one file is not too big"),
        }

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression for the gitignored-directory leak: a dir event for an
    /// ignored directory (a frontend build recreating `dist/`) used to be
    /// walked anyway — `index_dir_tree`'s walker starts INSIDE the dir, so the
    /// parent `.gitignore` rule excluding the dir itself never fired, and the
    /// minified bundles were parsed into the graph (thousands of one-letter
    /// symbols + `new`/`get`/`set` hubs that then exploded the viz snapshot).
    #[test]
    fn ignored_directory_event_is_not_walked() {
        let dir = std::env::temp_dir().join(format!("ckg-igdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("dist/assets")).unwrap();
        std::fs::write(dir.join(".gitignore"), "dist\n").unwrap();
        std::fs::write(
            dir.join("dist/assets/app.js"),
            "export function bundled() {}\n",
        )
        .unwrap();

        let sub = ".ckg-test";
        let snap = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");

        // Same matcher construction as `reindex_paths` for this batch.
        let gi = build_gitignore(&dir, &[dir.join("dist/assets")], &[]);

        // The dir itself and any subdir of it must both be no-ops.
        for target in ["dist", "dist/assets"] {
            match index_dir_tree(&idx, &dir, &dir.join(target), &snap, sub, u64::MAX, &gi) {
                DirWalk::Indexed { indexed, rels } => {
                    assert_eq!(indexed, 0, "{target}: nothing indexed");
                    assert!(rels.is_empty(), "{target}: no touched rels");
                }
                DirWalk::TooBig => panic!("{target}: ignored dir must not be walked at all"),
            }
        }
        assert!(
            idx.find_symbol("bundled").unwrap().is_empty(),
            "bundle symbol never indexed"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The language census sees every language on disk (not just indexed ones)
    /// and classifies each: a supported+allowlisted lang is green (enabled), a
    /// supported-but-not-allowlisted lang is yellow, a known-but-unsupported
    /// programming language is a named red chip, and anything else folds into
    /// the single "other" bucket.
    #[test]
    fn language_census_classifies_green_yellow_red_and_other() {
        let dir = std::env::temp_dir().join(format!("ckg-census-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap(); // rust → green
        std::fs::write(dir.join("page.html"), "<h1>hi</h1>\n").unwrap(); // html → yellow (off by default)
        std::fs::write(dir.join("main.zig"), "pub fn main() void {}\n").unwrap(); // zig → red (named)
        std::fs::write(dir.join("data.bin"), "\0\0\0").unwrap(); // unknown → other
        std::fs::write(dir.join("notes.unknownext"), "x\n").unwrap(); // unknown → other

        let snap = GraphSettings::default(); // rust on, html off
        let census = language_census(&dir, &snap, ".ckg-test");

        let get = |key: &str| census.iter().find(|e| e.key == key).cloned();

        let rust = get("rust").expect("rust present");
        assert!(rust.supported && rust.enabled, "rust green: {rust:?}");
        assert_eq!(rust.files, 1);
        assert_eq!(rust.label, "Rust");

        let html = get("html").expect("html present");
        assert!(html.supported && !html.enabled, "html yellow: {html:?}");

        let zig = get("zig").expect("zig present");
        assert!(!zig.supported && !zig.enabled, "zig red: {zig:?}");
        assert_eq!(zig.label, "Zig");

        let other = get("other").expect("other bucket present");
        assert!(!other.supported, "other red: {other:?}");
        assert_eq!(other.files, 2, "bin + unknownext fold into other");

        // Green sorts ahead of the "other" bucket, which is always last.
        assert_eq!(census.first().map(|e| e.key.as_str()), Some("rust"));
        assert_eq!(census.last().map(|e| e.key.as_str()), Some("other"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebuild_indexes_markdown_docs_and_honors_index_docs_toggle() {
        let dir = std::env::temp_dir().join(format!("ckg-md-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("src.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(
            dir.join("docs/guide.md"),
            "# Guide\n\nHow to configure the widget frobnicator.\n",
        )
        .unwrap();
        let sub = ".ckg-test";

        // index_docs on (default): the markdown chunk is searchable.
        let snap_on = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");
        build_tree(&idx, &dir, &snap_on, sub).expect("rebuild");
        let hits = idx.search_docs("frobnicator", 10, 200).expect("search");
        assert!(hits.iter().any(|h| h.source_path == "docs/guide.md"));

        // index_docs off: markdown is skipped (the file row is gone after a
        // clean rebuild), so the doc search no longer matches.
        let snap_off = GraphSettings {
            index_docs: false,
            ..GraphSettings::default()
        };
        build_tree(&idx, &dir, &snap_off, sub).expect("rebuild2");
        assert!(idx.search_docs("frobnicator", 10, 200).unwrap().is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_code_max_chunks_caps_total_across_a_rebuild() {
        // Two files, each with one chunk-eligible function. A budget of 1
        // must cap the project-wide `code_chunk` total at 1, regardless of
        // which file the walk visits first.
        let dir = std::env::temp_dir().join(format!("ckg-codecap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/a.rs"),
            "pub fn alpha(a: i32, b: i32) -> i32 {\n    let c = a + b;\n    c\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/b.rs"),
            "pub fn beta(a: i32, b: i32) -> i32 {\n    let c = a * b;\n    c\n}\n",
        )
        .unwrap();

        let sub = ".ckg-test";
        let snap = GraphSettings {
            semantic_code_max_chunks: 1,
            ..GraphSettings::default()
        };
        let idx = GraphIndex::open(&dir, sub).expect("open");
        build_tree(&idx, &dir, &snap, sub).expect("rebuild");

        // `total` from `code_embedding_coverage` is epoch-independent (a plain
        // `count(*code_chunk{id})`), so any epoch string works here.
        let (_, total) = idx.code_embedding_coverage("any").expect("coverage");
        assert_eq!(
            total, 1,
            "the project-wide cap trims to the configured budget"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lang_for_honors_configured_languages() {
        use std::path::PathBuf;
        let all = GraphSettings::default().languages;
        // A configured language resolves; an unknown extension doesn't.
        assert_eq!(lang_for(&PathBuf::from("src/a.rs"), &all), Some(Lang::Rust));
        assert_eq!(lang_for(&PathBuf::from("a.bin"), &all), None);
        // A recognized language that the user didn't opt into is filtered out.
        let only_rust = vec!["rust".to_string()];
        assert_eq!(lang_for(&PathBuf::from("a.py"), &only_rust), None);
        assert_eq!(
            lang_for(&PathBuf::from("a.rs"), &only_rust),
            Some(Lang::Rust)
        );
    }

    /// V12 Phase F (6c): the `graph-analyses` event only fires when the
    /// dead-exports/import-cycles counts actually changed since the last
    /// stored pass — first-ever pass (`None` stored) counts as a change, an
    /// identical repeat does not, and any different count does.
    #[test]
    fn analyses_changed_only_on_first_seen_or_different_counts() {
        assert!(
            analyses_changed(None, "3,1"),
            "first pass is always new information"
        );
        assert!(
            !analyses_changed(Some("3,1"), "3,1"),
            "identical counts: no event"
        );
        assert!(
            analyses_changed(Some("3,1"), "4,1"),
            "dead-export count grew"
        );
        assert!(
            analyses_changed(Some("3,1"), "3,0"),
            "cycle count shrank — still a change"
        );
    }

    // ── V30 Phase C: index-completion push gate ─────────────────────────────

    /// The duration half of `announce_index_complete`'s gate. Delivering a push
    /// to an idle Claude tab starts a model turn, so only builds expensive
    /// enough to have been worth waiting on may announce themselves.
    #[test]
    fn index_push_worthy_only_past_the_duration_floor() {
        let user = |ms| index_push_worthy(true, RebuildOrigin::User, ms);
        assert!(!user(0), "an instant rebuild is not news");
        assert!(
            !user(GRAPH_PUSH_MIN_BUILD_MS - 1),
            "just under the floor must stay silent"
        );
        assert!(user(GRAPH_PUSH_MIN_BUILD_MS), "the floor itself qualifies");
        assert!(
            user(GRAPH_PUSH_MIN_BUILD_MS * 10),
            "a five-minute build definitely qualifies"
        );
        assert_eq!(
            GRAPH_PUSH_MIN_BUILD_MS, 30_000,
            "the milestone fixes this floor at 30s — changing it is a spec decision"
        );
    }

    /// The ORIGIN half (review M2): an automatic rebuild never announces itself,
    /// however long it took. Four automatic paths reach `spawn_rebuild` —
    /// startup, the settings-enable watcher, watcher-overflow recovery, the
    /// schema-migration repair, and the incremental walk's `DirWalk::TooBig`
    /// escalation — so without this gate an app launch on a big repo (or a large
    /// `git checkout`) started a model turn in every channel-armed tab.
    #[test]
    fn index_push_worthy_rejects_automatic_rebuilds() {
        for ms in [0, GRAPH_PUSH_MIN_BUILD_MS, GRAPH_PUSH_MIN_BUILD_MS * 100] {
            assert!(
                !index_push_worthy(true, RebuildOrigin::Automatic, ms),
                "an automatic rebuild must never push (elapsed {ms}ms)"
            );
        }
        assert!(
            index_push_worthy(true, RebuildOrigin::User, GRAPH_PUSH_MIN_BUILD_MS),
            "…while the same build a user asked for still does"
        );
    }

    /// Review M6: "off means off" app-side. `offload.session_push` is read live
    /// at fire time and dominates every other input — the child-side capability
    /// declaration is latched until the tab restarts, so without this a
    /// toggled-off feature kept pushing into running tabs.
    #[test]
    fn index_push_worthy_honours_a_live_settings_toggle() {
        assert!(
            !index_push_worthy(false, RebuildOrigin::User, GRAPH_PUSH_MIN_BUILD_MS * 10),
            "session_push off ⇒ no push, however expensive or user-requested"
        );
    }
}
