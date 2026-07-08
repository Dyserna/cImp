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

use serde::Serialize;

use super::index::GraphIndex;

/// The result of a retrieval: the rendered markdown to inject plus accounting.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RetrieveResult {
    /// Markdown digest to prepend, or empty when nothing cleared the threshold.
    pub context_md: String,
    /// Files whose digests were included (project-relative).
    pub files_used: Vec<String>,
    pub chars: usize,
    /// Rough token estimate (`chars / 4`) for the UI's honesty readout.
    pub tokens_est: usize,
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
pub fn build_context(
    idx: &GraphIndex,
    prompt: &str,
    session_files: &[(String, f64)],
    per_file_chars: usize,
    turn_budget_chars: usize,
    min_score: f64,
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

    // Budget-pack digests (outline signatures, not whole files).
    let mut used: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut budget = turn_budget_chars;
    for (file, _s) in ranked {
        if budget < 80 {
            break;
        }
        let digest = file_digest(idx, &file, per_file_chars.min(budget));
        if digest.is_empty() {
            continue;
        }
        let line = format!("- `{file}` — {digest}");
        budget = budget.saturating_sub(line.chars().count());
        lines.push(line);
        used.push(file);
        if used.len() >= 12 {
            break;
        }
    }
    if used.is_empty() {
        return RetrieveResult::default();
    }

    let mut md = String::from("## Relevant context (cImp)\n");
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
    joined
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
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

        // A prompt mentioning an indexed symbol injects a digest.
        let r = build_context(&idx, "please refactor build_widget", &[], 800, 6000, 3.0);
        assert!(r.chars > 0, "matching prompt injects context");
        assert!(r.files_used.contains(&"src/widget.rs".to_string()));
        assert!(r.context_md.contains("src/widget.rs"));

        // A meta prompt with no matching terms injects nothing (below threshold).
        let empty = build_context(&idx, "hi there, thanks!", &[], 800, 6000, 3.0);
        assert_eq!(empty.chars, 0);
        assert!(empty.files_used.is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
