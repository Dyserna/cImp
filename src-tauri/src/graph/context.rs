//! V10 Phase D — context retrieval core.
//!
//! Given a user prompt, rank the project's files by relevance and build a
//! budget-bounded **digest** (an outline of the top files' signatures, not their
//! full contents) to prepend to the agent's turn. Deliberately **synchronous**
//! and structural — it reuses warm-index reads (`find_symbol` / `references` /
//! `search_docs` / `outline`) plus the session working set, with no per-prompt
//! embedding round-trip, so injection stays fast enough to sit in front of every
//! turn. Both agents (Claude hook, OpenCode plugin) share this core; only the
//! injection shim differs.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use super::index::GraphIndex;

/// The result of a retrieval: the rendered markdown to inject plus accounting.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RetrieveResult {
    /// Markdown digest to prepend, or empty when nothing cleared the threshold.
    pub context_md: String,
    /// Files whose full digests were included (project-relative). Dedup-demoted
    /// files (one-line "unchanged" reminders) are **not** listed here.
    pub files_used: Vec<String>,
    pub chars: usize,
    /// Rough token estimate (`chars / 4`) for the UI's honesty readout.
    pub tokens_est: usize,
    /// V11 Phase C: measured characters of digests suppressed by dedup this turn
    /// (files already injected unchanged). Honest accounting — the actual digest
    /// chars we did *not* re-send, not a fabricated savings figure.
    pub deduped_chars: usize,
    /// V11 Phase F: `(file, content_hash)` pairs that ranked in but had neither an
    /// outline digest nor a cached local-model digest. The service enqueues these
    /// for background digest generation. Internal plumbing, not serialized.
    #[serde(skip)]
    pub digest_misses: Vec<(String, String)>,
}

/// Extract candidate search terms from a prompt: identifier-like and path-like
/// tokens of length ≥ 3, minus a small stopword set. Case-folded, deduped,
/// order-preserving.
pub fn extract_terms(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in prompt.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '"' | '\'' | '`' | '=' | '<' | '>' | '{' | '}' | '[' | ']' | '!' | '?' | ':')) {
        let tok = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '/' && c != '.');
        if tok.len() < 3 {
            continue;
        }
        // Keep path-like and identifier-like tokens; drop pure numbers.
        let looks_useful = tok.chars().any(|c| c.is_alphabetic());
        if !looks_useful {
            continue;
        }
        let lower = tok.to_ascii_lowercase();
        if is_stopword(&lower) {
            continue;
        }
        if seen.insert(lower.clone()) {
            out.push(tok.to_string());
        }
        if out.len() >= 24 {
            break;
        }
    }
    out
}

fn is_stopword(w: &str) -> bool {
    matches!(
        w,
        "the" | "and" | "for" | "with" | "this" | "that" | "what" | "where" | "when" | "how"
            | "why" | "does" | "did" | "can" | "could" | "would" | "should" | "are" | "was"
            | "were" | "you" | "your" | "please" | "have" | "has" | "into" | "from"
            | "code" | "file" | "files" | "function" | "class" | "add" | "fix" | "make" | "use"
            | "using" | "them" | "then" | "there" | "here" | "about" | "which" | "some" | "any"
            | "all" | "get" | "set" | "not" | "but" | "out" | "run" | "let" | "new"
    )
}

/// Weight of a session working-set entry, by its last event kind.
pub fn session_weight(last_kind: &str) -> f64 {
    match last_kind {
        "edit" => 4.0,
        "query" => 2.5,
        _ => 2.0,
    }
}

/// Rank files for `prompt` and build the injectable digest. `session_files` is
/// the current session's working set as `(project_relative_path, weight)` (empty
/// when session inclusion is off). Budgets and the min-score gate come from
/// settings. Everything reuses warm-index reads and never blocks a rebuild.
#[allow(clippy::too_many_arguments)]
pub fn build_context(
    idx: &GraphIndex,
    prompt: &str,
    session_files: &[(String, f64)],
    per_file_chars: usize,
    turn_budget_chars: usize,
    min_score: f64,
    // V11 Phase C dedup: files already injected this session as
    // `path → (content_hash_at_injection, turn_injected)`, the current turn, and
    // the TTL in turns. `dedup_ttl == 0` or an empty snapshot ⇒ no dedup (the
    // preview path passes both empty/0, so its behaviour is unchanged).
    injected: &HashMap<String, (String, u32)>,
    current_turn: u32,
    dedup_ttl: u32,
    // V11 Phase F: when on, files with no outline digest fall back to a cached
    // local-model digest (miss ⇒ recorded in `digest_misses` for background compute).
    llm_digests: bool,
) -> RetrieveResult {
    let terms = extract_terms(prompt);
    if terms.is_empty() && session_files.is_empty() {
        return RetrieveResult::default();
    }

    // Score every candidate file.
    let mut score: HashMap<String, f64> = HashMap::new();
    for term in &terms {
        // Definitions: strong signal. Indexed point lookup by name.
        if let Ok(hits) = idx.find_symbol(term) {
            for h in hits.iter().take(20) {
                *score.entry(h.file.clone()).or_default() += 3.0;
            }
        }
        // Use sites: weaker, capped so a ubiquitous name can't dominate.
        if let Ok(refs) = idx.references(term) {
            for r in refs.iter().take(30) {
                *score.entry(r.file.clone()).or_default() += 1.0;
            }
        }
    }
    // Docs: ONE scan of doc_chunk for all terms (not one full scan per term).
    if let Ok(doc_hits) = idx.doc_source_hits(&terms) {
        for (path, hits) in doc_hits {
            *score.entry(path).or_default() += 2.0 * hits as f64;
        }
    }
    // Session recency boost.
    for (path, weight) in session_files {
        *score.entry(path.clone()).or_default() += weight;
    }

    // V12 Phase D: churn boost — files touched recently in git are more
    // likely to be what "this project" currently means. ONE fetch of the
    // 30-day churn set (not a per-candidate DB query in this loop), then a
    // cheap in-memory lookup per already-scored candidate. `+3` within 7
    // days, `+1` within 30 (beside the term/doc/session weights above); a
    // file outside the 30-day window (or with no git history at all) simply
    // isn't in the map and gets no boost.
    let churn: HashMap<String, i64> = idx
        .recent_changes(30, None, 5_000)
        .unwrap_or_default()
        .into_iter()
        .map(|c| (c.file, c.last_ts))
        .collect();
    if !churn.is_empty() {
        let now = super::gitmeta::now_ts();
        for (file, s) in score.iter_mut() {
            if let Some(&last_ts) = churn.get(file) {
                let age_days = (now - last_ts) / 86_400;
                if age_days <= 7 {
                    *s += 3.0;
                } else if age_days <= 30 {
                    *s += 1.0;
                }
            }
        }
    }

    // V12 Phase E: project-fact boost — a durable fact that mentions a
    // candidate file's stem (e.g. a fact naming "GraphService" boosts
    // `graph/service.rs`) is a signal the file matters to this project beyond
    // one prompt's term match. ONE fetch of the live fact set (not a
    // per-candidate query), then a cheap substring check per already-scored
    // candidate. Facts themselves are never injected per-turn here — only
    // this small ranking nudge; they surface in full via `context_recall` and
    // (pinned-only) launch-time promotion (V12 spec, Feature 5).
    let facts: Vec<String> = idx
        .list_project_facts(false, 100)
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.text)
        .collect();
    if !facts.is_empty() {
        for (file, s) in score.iter_mut() {
            let stem = Path::new(file)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !stem.is_empty() && facts.iter().any(|f| f.contains(stem)) {
                *s += 2.0;
            }
        }
    }

    // Rank; bail when the best file is below the threshold (meta prompts).
    let mut ranked: Vec<(String, f64)> = score.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    if ranked.first().map(|(_, s)| *s < min_score).unwrap_or(true) {
        return RetrieveResult::default();
    }

    // Budget-pack digests (outline signatures, not whole files). Full-digest
    // lines come first; dedup "unchanged" reminders are appended after them.
    let mut used: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut reminders: Vec<String> = Vec::new();
    let mut deduped_chars = 0usize;
    let mut digest_misses: Vec<(String, String)> = Vec::new();
    let mut budget = turn_budget_chars;
    for (file, _s) in ranked {
        // Hard cap on total emitted lines (full digests + reminders) so a broad
        // prompt on a big project can't produce an enormous block.
        if used.len() + reminders.len() >= 24 {
            break;
        }
        // Once the full-digest cap is hit and there's no room even for a cheap
        // reminder, there's nothing left to add — stop scanning.
        if used.len() >= 12 && budget < 80 {
            break;
        }
        // V11 Phase C: was this file already injected this session, and is it
        // still unchanged and within the TTL window? Compare the content hash at
        // injection against the current indexed hash (a re-index updates it).
        let prev = injected.get(&file);
        let (suppress, changed) = if dedup_ttl > 0 {
            match prev {
                Some((prev_hash, prev_turn)) => {
                    let cur = idx.stored_file_hash(&file).ok().flatten();
                    let unchanged = cur.as_deref() == Some(prev_hash.as_str());
                    // `current_turn < prev_turn` only after a restart reset the
                    // in-memory counter — treat that as expired (re-inject).
                    let within = current_turn >= *prev_turn && current_turn - *prev_turn <= dedup_ttl;
                    (unchanged && within, !unchanged)
                }
                None => (false, false),
            }
        } else {
            (false, false)
        };

        if suppress {
            // Demote to a one-line reminder; measure the digest chars we saved.
            let saved = file_digest(idx, &file, per_file_chars);
            deduped_chars += saved.chars().count();
            let prev_turn = prev.map(|(_, t)| *t).unwrap_or(0);
            let line = format!("- `{file}` — injected turn {prev_turn}, unchanged");
            let cost = line.chars().count();
            if budget >= cost {
                budget -= cost;
                reminders.push(line);
            }
            continue;
        }

        // A non-suppressed file needs a full digest; skip it (but keep scanning
        // for more reminders) once the full-digest cap or budget is exhausted.
        if used.len() >= 12 || budget < 80 {
            continue;
        }
        let mut digest = file_digest(idx, &file, per_file_chars.min(budget));
        if digest.is_empty() && llm_digests {
            // No outline (docs/configs/long scripts): use a cached local-model
            // digest if present, else record a miss for background compute.
            if let Ok(Some(h)) = idx.stored_file_hash(&file) {
                match idx.get_digest(&file, &h) {
                    Ok(Some(text)) => digest = text,
                    _ => digest_misses.push((file.clone(), h)),
                }
            }
        }
        if digest.is_empty() {
            continue;
        }
        let tag = if changed { " (updated)" } else { "" };
        let line = format!("- `{file}` — {digest}{tag}");
        budget = budget.saturating_sub(line.chars().count());
        lines.push(line);
        used.push(file);
    }
    if used.is_empty() && reminders.is_empty() {
        return RetrieveResult::default();
    }

    let mut md = String::from("## Relevant context (cImp)\n");
    lines.extend(reminders);
    md.push_str(&lines.join("\n"));
    if !session_files.is_empty() {
        let ws: Vec<&str> = session_files.iter().take(6).map(|(p, _)| p.as_str()).collect();
        if !ws.is_empty() {
            md.push_str(&format!("\n\n_Session working set: {}_", ws.join(", ")));
        }
    }
    let chars = md.chars().count();
    RetrieveResult {
        context_md: md,
        files_used: used,
        chars,
        tokens_est: chars / 4,
        deduped_chars,
        digest_misses,
    }
}

/// A one-line digest of `file`: its top outline signatures, joined and capped at
/// `max_chars`. Empty when the file has no indexed symbols.
fn file_digest(idx: &GraphIndex, file: &str, max_chars: usize) -> String {
    let Ok(syms) = idx.outline(file) else { return String::new() };
    if syms.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut total = 0usize;
    for s in syms.iter().take(8) {
        let sig = if s.signature.trim().is_empty() {
            format!("{} {}", s.kind, s.name)
        } else {
            s.signature.trim().to_string()
        };
        let sig = first_line(&sig);
        total += sig.chars().count() + 2;
        parts.push(sig);
        if total >= max_chars {
            break;
        }
    }
    let mut joined = parts.join("; ");
    if joined.chars().count() > max_chars {
        joined = joined.chars().take(max_chars.saturating_sub(1)).collect::<String>();
        joined.push('…');
    }
    // V12 Phase D: optional "last change" trailer when git history exists for
    // this file. Appended AFTER the max_chars truncation above (it's a small,
    // separately-capped addition, not squeezed out of the signature budget) —
    // the caller already measures the actual returned string length before
    // debiting its own budget, so this is accounted for correctly there.
    if let Some((last_ts, subject, _touches)) = idx.commit_touch(file).ok().flatten() {
        let age = super::gitmeta::relative_age(super::gitmeta::now_ts(), last_ts);
        let subject = super::gitmeta::truncate_subject(&subject, 60);
        joined.push_str(&format!(" — last change: \"{subject}\" ({age})"));
    }
    joined
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// V11 Phase B — a once-per-session **project map**: the most call-central files
/// with their top exported signatures, greedily packed to `budget_chars`. Unlike
/// [`build_context`] (per-prompt *relevance*), this is *orientation* — it's
/// stable across a session. `session_boost` is the session working set
/// (`(path, weight)`); when non-empty, session-hot files are lifted up the
/// ranking so the map foregrounds what this session is touching. Returns an
/// empty string when the graph has no call edges yet (nothing to orient around).
pub fn repo_map(idx: &GraphIndex, budget_chars: usize, session_boost: &[(String, f64)]) -> String {
    let Ok(central) = idx.file_centrality(200) else {
        return String::new();
    };
    if central.is_empty() {
        return String::new();
    }

    // Fold in the session working set: centrality + boost, re-ranked.
    let ranked: Vec<String> = if session_boost.is_empty() {
        central.into_iter().map(|(f, _)| f).collect()
    } else {
        let boost: HashMap<&str, f64> =
            session_boost.iter().map(|(p, w)| (p.as_str(), *w)).collect();
        let mut scored: Vec<(String, f64)> = central
            .into_iter()
            .map(|(f, c)| {
                let b = boost.get(f.as_str()).copied().unwrap_or(0.0);
                (f, c as f64 + b)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.into_iter().map(|(f, _)| f).collect()
    };

    let mut out = String::from("## Project map (cImp)\n");
    let mut budget = budget_chars;
    for file in ranked {
        if budget < 40 {
            break;
        }
        let sigs = public_signatures(idx, &file, budget.min(240));
        if sigs.is_empty() {
            continue;
        }
        let line = format!("- `{file}` — {sigs}\n");
        let cost = line.chars().count();
        if cost > budget {
            break;
        }
        budget -= cost;
        out.push_str(&line);
    }
    // Only the header ⇒ nothing packed; treat as empty.
    if out.trim_end() == "## Project map (cImp)" {
        return String::new();
    }
    out
}

/// V11 Phase E — the reminder text for a redundant `Read`. Always usable
/// content (never a bare refusal, since the agent's context may have been
/// compacted away): the file's outline signatures + an escape-hatch line telling
/// it how to force a real read. In `substitute` mode it also carries the most
/// relevant symbol body (the symbol enclosing `offset`, else the file's top
/// public symbol), sliced from disk and byte-capped.
pub fn read_advice(
    idx: &GraphIndex,
    root: &Path,
    rel_file: &str,
    offset: Option<u32>,
    substitute: bool,
    max_body_bytes: usize,
) -> String {
    let outline = idx.outline(rel_file).unwrap_or_default();
    let sigs: Vec<String> = outline
        .iter()
        .take(10)
        .map(|s| {
            let sig = if s.signature.trim().is_empty() {
                format!("{} {}", s.kind, s.name)
            } else {
                s.signature.trim().to_string()
            };
            first_line(&sig)
        })
        .collect();
    let mut text = format!(
        "`{rel_file}` is unchanged since you read it this session. Outline: {}. \
         Re-read with Read({{file, offset, limit}}) if you need the exact text.",
        sigs.join("; ")
    );
    if substitute {
        if let Some(body) = substitute_body(idx, root, rel_file, offset, max_body_bytes / 2) {
            text.push_str(&format!("\n\n{body}"));
        }
    }
    text
}

/// Slice the most relevant symbol body from disk for the read advisor's
/// substitute mode. `None` when the file can't be read or has no suitable symbol.
fn substitute_body(
    idx: &GraphIndex,
    root: &Path,
    rel_file: &str,
    offset: Option<u32>,
    max_bytes: usize,
) -> Option<String> {
    let hit = match offset {
        Some(line) => idx.symbol_at(rel_file, line).ok().flatten()?,
        None => {
            let mut syms = idx.outline(rel_file).ok()?;
            syms.sort_by(|a, b| {
                (b.visibility == "public")
                    .cmp(&(a.visibility == "public"))
                    .then(a.start_line.cmp(&b.start_line))
            });
            syms.into_iter().next()?
        }
    };
    let content = std::fs::read_to_string(root.join(rel_file)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let first = (hit.start_line as usize).saturating_sub(1);
    let last = (hit.end_line as usize).min(lines.len());
    if first >= last {
        return None;
    }
    let mut body = lines[first..last].join("\n");
    if body.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n… [truncated]");
    }
    Some(format!("{}:{}-{} · {}\n{}", hit.file, hit.start_line, hit.end_line, hit.kind, body))
}

/// V11 Phase D — the block fed to a compaction so the session's working context
/// survives the summary: the ranked working set (top ~10, one line each), pinned
/// notes verbatim, and unpinned notes as one-line digests. Hard-capped at ~2000
/// chars. Empty when there's no session activity to carry.
pub fn compaction_block(idx: &GraphIndex, session_id: Option<&str>) -> String {
    let Some(sid) = session_id.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let ws = idx.mem_working_set(sid, 10).unwrap_or_default();
    let notes = idx.mem_notes(sid).unwrap_or_default();
    if ws.is_empty() && notes.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    if !ws.is_empty() {
        out.push_str("## Session working set (carry across compaction)\n");
        for e in ws.iter().take(10) {
            let syms = if e.top_symbols.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.top_symbols.join(", "))
            };
            out.push_str(&format!("- {} — {}× (last {}){}\n", e.path, e.touches, e.last_kind, syms));
        }
    }
    let pinned: Vec<&str> = notes.iter().filter(|n| n.pinned).map(|n| n.text.as_str()).collect();
    let unpinned: Vec<&str> = notes.iter().filter(|n| !n.pinned).map(|n| n.text.as_str()).collect();
    if !pinned.is_empty() {
        out.push_str("\n## Pinned notes (keep verbatim)\n");
        for t in &pinned {
            out.push_str(&format!("- {t}\n"));
        }
    }
    if !unpinned.is_empty() {
        out.push_str("\n## Other session notes\n");
        for t in unpinned.iter().take(8) {
            out.push_str(&format!("- {}\n", first_line(t)));
        }
    }

    // Hard cap so a busy session can't bloat the compaction prompt.
    if out.chars().count() > 2000 {
        out = out.chars().take(1999).collect::<String>();
        out.push('…');
    }
    out
}

/// The top exported signatures of `file` — `visibility == public` first, then
/// source order — joined and capped at `max_chars`. Empty when the file has no
/// indexed symbols.
fn public_signatures(idx: &GraphIndex, file: &str, max_chars: usize) -> String {
    let Ok(mut syms) = idx.outline(file) else {
        return String::new();
    };
    if syms.is_empty() {
        return String::new();
    }
    syms.sort_by(|a, b| {
        let pub_a = a.visibility == "public";
        let pub_b = b.visibility == "public";
        pub_b.cmp(&pub_a).then_with(|| a.start_line.cmp(&b.start_line))
    });
    let mut parts: Vec<String> = Vec::new();
    let mut total = 0usize;
    for s in syms.iter().take(6) {
        let sig = if s.signature.trim().is_empty() {
            format!("{} {}", s.kind, s.name)
        } else {
            s.signature.trim().to_string()
        };
        let sig = first_line(&sig);
        total += sig.chars().count() + 2;
        parts.push(sig);
        if total >= max_chars {
            break;
        }
    }
    let mut joined = parts.join("; ");
    if joined.chars().count() > max_chars {
        joined = joined.chars().take(max_chars.saturating_sub(1)).collect::<String>();
        joined.push('…');
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_terms_filters_stopwords_and_short_tokens() {
        let terms = extract_terms("How does GraphService handle the retrieve() path?");
        assert!(terms.iter().any(|t| t == "GraphService"));
        assert!(terms.iter().any(|t| t == "retrieve"));
        assert!(!terms.iter().any(|t| t == "the" || t == "how" || t == "does"));
    }

    #[test]
    fn empty_prompt_yields_no_terms() {
        assert!(extract_terms("  ??  ").is_empty());
    }

    #[test]
    fn build_context_ranks_and_gates() {
        use crate::graph::{parse_file, Lang};
        let dir = std::env::temp_dir().join(format!("ckg-ctx-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file(
            "src/widget.rs",
            "pub fn build_widget() {}\npub struct Widget { x: i32 }\n",
            Lang::Rust,
        ))
        .expect("index");

        let no_dedup = HashMap::new();
        // A prompt mentioning an indexed symbol injects a digest.
        let r = build_context(&idx, "please refactor build_widget", &[], 800, 6000, 3.0, &no_dedup, 0, 0, false);
        assert!(r.chars > 0, "matching prompt injects context");
        assert!(r.files_used.contains(&"src/widget.rs".to_string()));
        assert!(r.context_md.contains("src/widget.rs"));

        // A meta prompt with no matching terms injects nothing (below threshold).
        let empty = build_context(&idx, "hi there, thanks!", &[], 800, 6000, 3.0, &no_dedup, 0, 0, false);
        assert_eq!(empty.chars, 0);
        assert!(empty.files_used.is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn churn_boost_promotes_a_recently_touched_file_over_an_equal_score_peer() {
        use crate::graph::{parse_file, Lang};
        let dir = std::env::temp_dir().join(format!("ckg-churn-boost-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // Two files, each defining a same-named symbol — `find_symbol("widget")`
        // hits both, so their base term-match score is identical.
        idx.index_file_graph(&parse_file("src/hot.rs", "pub fn widget() {}\n", Lang::Rust))
            .expect("index hot");
        idx.index_file_graph(&parse_file("src/cold.rs", "pub fn widget() {}\n", Lang::Rust))
            .expect("index cold");

        // Only hot.rs has recent git history (within the 7-day boost tier).
        let now = super::super::gitmeta::now_ts();
        idx.put_commit_touches(&[super::super::gitmeta::FileChurn {
            file: "src/hot.rs".to_string(),
            last_ts: now - 86_400, // 1 day ago
            last_subject: "fix: widget cache".to_string(),
            touches_90d: 4,
        }])
        .expect("put churn");

        let no_dedup = HashMap::new();
        let r = build_context(&idx, "widget", &[], 800, 6000, 0.5, &no_dedup, 0, 0, false);
        // Both files clear the (low) threshold and are ranked; the churned
        // file must come first despite an identical base term-match score.
        let pos_hot = r.files_used.iter().position(|f| f == "src/hot.rs");
        let pos_cold = r.files_used.iter().position(|f| f == "src/cold.rs");
        assert!(pos_hot.is_some() && pos_cold.is_some(), "{:?}", r.files_used);
        assert!(pos_hot < pos_cold, "churned file should rank first: {:?}", r.files_used);
        // The digest trailer surfaces the commit subject + a relative age.
        assert!(r.context_md.contains("fix: widget cache"), "{}", r.context_md);
        assert!(r.context_md.contains("1d ago"), "{}", r.context_md);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_fact_boost_promotes_a_mentioned_file_over_an_equal_score_peer() {
        use crate::graph::{parse_file, Lang};
        let dir = std::env::temp_dir().join(format!("ckg-fact-boost-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // Two files, each defining a same-named symbol — identical base
        // term-match score, same as the churn-boost test's fixture.
        idx.index_file_graph(&parse_file("src/hot.rs", "pub fn widget() {}\n", Lang::Rust))
            .expect("index hot");
        idx.index_file_graph(&parse_file("src/cold.rs", "pub fn widget() {}\n", Lang::Rust))
            .expect("index cold");

        // A fact naming "hot" (hot.rs's stem) should boost only that file.
        idx.add_project_fact("f1", "hot handles the retry-cache gotcha", "s1", 1, false)
            .expect("add fact");

        let no_dedup = HashMap::new();
        let r = build_context(&idx, "widget", &[], 800, 6000, 0.5, &no_dedup, 0, 0, false);
        let pos_hot = r.files_used.iter().position(|f| f == "src/hot.rs");
        let pos_cold = r.files_used.iter().position(|f| f == "src/cold.rs");
        assert!(pos_hot.is_some() && pos_cold.is_some(), "{:?}", r.files_used);
        assert!(pos_hot < pos_cold, "the fact-mentioned file should rank first: {:?}", r.files_used);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_map_ranks_by_centrality_and_respects_budget() {
        use crate::graph::{parse_file, Lang};
        let dir = std::env::temp_dir().join(format!("ckg-map-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // `hub` is called from two files; `leaf` from none → hub outranks leaf.
        idx.index_file_graph(&parse_file("src/hub.rs", "pub fn hub() {}\npub fn leaf() {}\n", Lang::Rust))
            .expect("index hub");
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn a() { hub() }\n", Lang::Rust))
            .expect("index a");
        idx.index_file_graph(&parse_file("src/b.rs", "pub fn b() { hub() }\n", Lang::Rust))
            .expect("index b");

        let central = idx.file_centrality(10).expect("centrality");
        assert_eq!(central.first().map(|(f, _)| f.as_str()), Some("src/hub.rs"));
        assert!(central.iter().find(|(f, _)| f == "src/hub.rs").unwrap().1 >= 2);

        // A generous budget renders the map with the central file + a signature.
        let map = repo_map(&idx, 4000, &[]);
        assert!(map.contains("## Project map (cImp)"));
        assert!(map.contains("src/hub.rs"));
        assert!(map.contains("fn hub"));

        // A tiny budget still produces at most the header (never panics/overflows).
        let tiny = repo_map(&idx, 10, &[]);
        assert!(tiny.is_empty() || tiny.starts_with("## Project map"));

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_suppresses_unchanged_reinjects_changed_and_honors_ttl() {
        use crate::graph::{parse_file, Lang};
        let dir = std::env::temp_dir().join(format!("ckg-dedup-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let fg = parse_file(
            "src/widget.rs",
            "pub fn build_widget() {}\npub struct Widget { x: i32 }\n",
            Lang::Rust,
        );
        idx.index_file_graph(&fg).expect("index");
        let hash = idx.stored_file_hash("src/widget.rs").unwrap().unwrap();
        let prompt = "refactor build_widget";

        // Turn 1: nothing injected before → full digest, nothing deduped.
        let empty = HashMap::new();
        let r1 = build_context(&idx, prompt, &[], 800, 6000, 3.0, &empty, 1, 10, false);
        assert!(r1.files_used.contains(&"src/widget.rs".to_string()));
        assert_eq!(r1.deduped_chars, 0);

        // Turn 2: already injected, unchanged, within TTL → suppressed to a
        // one-line reminder with measured savings.
        let mut snap = HashMap::new();
        snap.insert("src/widget.rs".to_string(), (hash.clone(), 1u32));
        let r2 = build_context(&idx, prompt, &[], 800, 6000, 3.0, &snap, 2, 10, false);
        assert!(!r2.files_used.contains(&"src/widget.rs".to_string()), "not re-sent in full");
        assert!(r2.deduped_chars > 0, "measured suppression");
        assert!(r2.context_md.contains("unchanged"));

        // TTL expired → re-injected in full.
        let r3 = build_context(&idx, prompt, &[], 800, 6000, 3.0, &snap, 100, 10, false);
        assert!(r3.files_used.contains(&"src/widget.rs".to_string()));

        // ttl = 0 disables dedup entirely.
        let r4 = build_context(&idx, prompt, &[], 800, 6000, 3.0, &snap, 2, 0, false);
        assert!(r4.files_used.contains(&"src/widget.rs".to_string()));

        // Changed file (snapshot hash differs) → re-injected, tagged (updated).
        let mut snap_changed = HashMap::new();
        snap_changed.insert("src/widget.rs".to_string(), ("deadbeef".to_string(), 1u32));
        let r5 = build_context(&idx, prompt, &[], 800, 6000, 3.0, &snap_changed, 2, 10, false);
        assert!(r5.files_used.contains(&"src/widget.rs".to_string()));
        assert!(r5.context_md.contains("(updated)"));

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_block_carries_working_set_and_pinned_notes() {
        use crate::graph::{parse_file, Lang};
        let dir = std::env::temp_dir().join(format!("ckg-compact-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn f() {}\n", Lang::Rust))
            .expect("index");
        let sid = "sess-compact";
        idx.record_mem_event(sid, "claude", "edit", "src/a.rs", Some("f"), Some(1), 1_000, None)
            .expect("event");
        idx.mem_add_note("n1", sid, "chose FNV hashing for stability", 1_001, true)
            .expect("pinned note");
        idx.mem_add_note("n2", sid, "todo: revisit the cache eviction later", 1_002, false)
            .expect("note");

        let block = compaction_block(&idx, Some(sid));
        assert!(block.contains("src/a.rs"), "working set: {block}");
        assert!(block.contains("chose FNV hashing for stability"), "pinned verbatim: {block}");
        assert!(block.contains("Pinned notes"));
        assert!(block.chars().count() <= 2000, "hard cap respected");

        // No session id ⇒ empty (never fabricates a block).
        assert!(compaction_block(&idx, None).is_empty());
        // An unknown session has no working set, but pinned notes are
        // project-wide and are still carried through — that's intended.
        let unknown = compaction_block(&idx, Some("nope"));
        assert!(!unknown.contains("src/a.rs"), "no working set for an unknown session");
        assert!(unknown.contains("chose FNV hashing for stability"), "pinned notes still carried");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_advice_advises_with_outline_and_substitutes_a_body() {
        use crate::graph::{parse_file, Lang};
        let dir = std::env::temp_dir().join(format!("ckg-advice-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let src = "pub fn alpha() -> i32 {\n    let x = 1;\n    x + 1\n}\npub fn beta() {}\n";
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), src).unwrap();
        idx.index_file_graph(&parse_file("src/a.rs", src, Lang::Rust)).expect("index");

        // Advise mode: outline + escape hatch, and never a raw body.
        let advise = read_advice(&idx, &dir, "src/a.rs", None, false, 16_384);
        assert!(advise.contains("unchanged since you read it"), "{advise}");
        assert!(advise.contains("Re-read with Read"), "escape hatch: {advise}");
        assert!(advise.contains("alpha"), "outline signature: {advise}");
        assert!(!advise.contains("let x = 1;"), "no body in advise mode: {advise}");

        // Substitute mode with an offset inside alpha: carries the body too.
        let sub = read_advice(&idx, &dir, "src/a.rs", Some(2), true, 16_384);
        assert!(sub.contains("let x = 1;"), "substitute includes the body: {sub}");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
