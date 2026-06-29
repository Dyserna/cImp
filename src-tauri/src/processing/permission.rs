//! Detection of in-tab prompt patterns (tool-use approvals, AskUserQuestion-
//! style multi-choice questions, future kinds). Drives `awaiting_permission`
//! and `awaiting_question` flags on `TabState`.
//!
//! Strategy: substring-match the rendered (ANSI-stripped) tail of the terminal
//! output against a user-editable list of [`PermissionPattern`] entries
//! loaded from `<exe-dir>/patterns.json` at startup. Each pattern lists one
//! or more substrings that must ALL be present (`all_of`) and a `kind` that
//! determines which signal fires on the absent→present (and present→absent)
//! edges.
//!
//! Pattern characterization tip: enable `RUST_LOG=perm_capture=debug` to dump
//! the rendered tail the detector matches against; pick distinctive substrings
//! from the dump and add them to patterns.json. The `all_of` array supports
//! stacking a chrome marker (e.g. `Esc to cancel · Tab to amend`) with a
//! content marker (e.g. `Question:`) so two patterns can share the same
//! prompt UI yet route to different signals.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    /// Tool-use approval / file-edit / bash-command prompts. Sets
    /// `awaiting_permission` on the tab and fires the
    /// `awaiting_permission` notification template.
    Permission,
    /// AskUserQuestion-style multi-option prompts where Claude is asking
    /// the user to choose between options. Sets `awaiting_question` and
    /// fires the `question` notification template.
    Question,
    /// Claude is actively generating — its "busy" chrome (the
    /// `esc to interrupt` footer) is on screen. Drives the avatar's
    /// Thinking↔Idle activity instead of the byte-silence timer:
    /// `Detected` maps to `ClaudeOutputStarted`, `Resolved` to
    /// `ClaudeOutputStopped`. Content-based so a thinking pause (no output
    /// for >0.5s) no longer collapses the avatar to Idle mid-work.
    Working,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionPattern {
    /// Stable identifier for logging / debugging. Not user-visible.
    pub name: String,
    /// What kind of prompt this matches; determines which signal fires.
    pub kind: PatternKind,
    /// All listed substrings must be present in the rendered tail for the
    /// pattern to match. The substrings are searched independently; order
    /// doesn't matter. Useful when a chrome marker is shared across kinds
    /// and a content marker is needed to distinguish them.
    pub all_of: Vec<String>,
    /// When `true`, the pattern is loaded but never tested. Lets the
    /// shipped patterns.json carry template/example entries that the user
    /// flips on after capturing the live chrome.
    #[serde(default)]
    pub disabled: bool,
}

/// Built-in fallback patterns. Used when patterns.json is missing/corrupt
/// at load time so detection still works against the four AI builtins on
/// fresh installs and after a hand-edit accident. Mirrors the layout the
/// loader writes to disk on first launch. Pattern matching is pure
/// substring containment against the ANSI-stripped tail; the same shape
/// covers Claude Code chrome and OpenCode's `--mini` prompts (the latter
/// shipped as disabled templates until characterized live — V19 task A4).
pub fn default_patterns() -> Vec<PermissionPattern> {
    vec![
        PermissionPattern {
            name: "claude_permission".to_string(),
            kind: PatternKind::Permission,
            // `·` is U+00B7 (middle dot). Cell-rendered tail preserves it.
            all_of: vec!["Esc to cancel · Tab to amend".to_string()],
            disabled: false,
        },
        // Second permission pattern — disabled by default. Lives in the
        // file purely as a worked example of multi-pattern declaration:
        // patterns of the same `kind` act as alternatives (OR), so adding
        // more entries here covers additional prompt shapes that the
        // primary pattern misses. Set `disabled: false` and replace the
        // placeholder substring(s) to enable.
        PermissionPattern {
            name: "claude_permission_alt_example".to_string(),
            kind: PatternKind::Permission,
            all_of: vec![
                "<replace with a substring unique to this prompt shape>".to_string(),
            ],
            disabled: true,
        },
        // Claude Code's AskUserQuestion prompt. Unlike the permission
        // prompt it renders its own footer ("Enter to select · ↑/↓ to
        // navigate · Esc to cancel") and always appends a free-text
        // "Type something" choice. Neither marker appears in permission
        // prompts or normal output, and the pair is independent of the
        // specific question text, so it identifies the question UI for any
        // AskUserQuestion. Both sit at the bottom of the box, so they
        // survive the rendered-tail window even when a long question
        // scrolls off the top. Captured 2026-06-09 against Claude Code's
        // current chrome; re-capture with RUST_LOG=perm_capture=debug if a
        // future release changes the footer or option wording.
        PermissionPattern {
            name: "claude_question".to_string(),
            kind: PatternKind::Question,
            all_of: vec![
                "Enter to select".to_string(),
                "Type something".to_string(),
            ],
            disabled: false,
        },
        // Claude's "busy" footer, shown only while a request is in flight
        // (and never during a permission/question prompt, where Claude is
        // waiting on the user). Drives the avatar's Thinking state: present
        // = working, absent = done. Lowercase "esc" distinguishes it from
        // the permission prompt's "Esc to cancel". If a future Claude Code
        // release reworks this footer, re-capture with
        // RUST_LOG=perm_capture=debug and update the marker here.
        PermissionPattern {
            name: "claude_working".to_string(),
            kind: PatternKind::Working,
            all_of: vec!["esc to interrupt".to_string()],
            disabled: false,
        },
        // OpenCode (`opencode --mini`) permission prompt. OpenCode asks for
        // tool/edit/bash approval with its own inline footer. The exact marker
        // substring must be captured live against `opencode --mini` (the
        // alternate-screen TUI is never launched) — run a permission-triggering
        // action with `RUST_LOG=perm_capture=debug` and replace the placeholder
        // below with a distinctive substring from the dumped tail (e.g. the
        // footer chrome or the "Allow"/"Deny" option line). Shipped `disabled`
        // until characterized (V19 task A4) so a wrong guess can't mis-fire;
        // flip to `disabled: false` once the real marker is in place.
        PermissionPattern {
            name: "opencode_permission".to_string(),
            kind: PatternKind::Permission,
            all_of: vec![
                "<replace with a substring unique to opencode --mini's permission prompt>".to_string(),
            ],
            disabled: true,
        },
        // OpenCode's "busy"/working footer while a request is in flight (drives
        // the avatar's Thinking↔Idle state, like `claude_working`). Capture the
        // live `--mini` working chrome the same way and replace the placeholder;
        // shipped `disabled` until characterized (V19 task A4).
        PermissionPattern {
            name: "opencode_working".to_string(),
            kind: PatternKind::Working,
            all_of: vec![
                "<replace with a substring unique to opencode --mini's working footer>".to_string(),
            ],
            disabled: true,
        },
    ]
}

/// Empty pattern list for tabs that don't run detection (Shell tabs).
/// Test-only helper; production code constructs via `Vec::new()` inline.
#[cfg(test)]
pub fn no_patterns() -> Vec<PermissionPattern> {
    Vec::new()
}

/// Edge transition emitted by [`PermissionDetector::check`]. Each call may
/// produce zero, one, or two transitions — one per kind that flipped.
#[derive(Debug, PartialEq, Eq)]
pub enum PatternTransition {
    Detected {
        kind: PatternKind,
        pattern_name: String,
    },
    Resolved {
        kind: PatternKind,
        pattern_name: String,
    },
}

pub struct PermissionDetector {
    patterns: Vec<PermissionPattern>,
    /// Whitespace-normalized `all_of` substrings, index-aligned with
    /// `patterns`. Precomputed once so `check` doesn't re-normalize the
    /// (fixed) pattern set on every tick. See [`normalize_ws`].
    norm_all_of: Vec<Vec<String>>,
    /// Currently-detected pattern name per kind. Tracked per kind so a
    /// permission edge and a question edge don't compete — both can be
    /// "in flight" at once if upstream UI ever interleaves them.
    last_detected: HashMap<PatternKind, String>,
}

/// Collapse every run of whitespace (spaces, tabs, and the `\n` the screen
/// uses to join rows) to a single space and trim the ends. Applied to both
/// the rendered tail and each pattern substring so a marker still matches
/// when the TUI wraps it across two rows or pads it with variable-width
/// `\x1b[<n>C` cursor-forward gaps (which the cell renderer fills with a
/// variable number of spaces). The middle dot `·` (U+00B7) used in
/// `cancel · Tab` is not whitespace, so separators are preserved.
fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

impl PermissionDetector {
    pub fn new(patterns: Vec<PermissionPattern>) -> Self {
        let norm_all_of = patterns
            .iter()
            .map(|p| p.all_of.iter().map(|s| normalize_ws(s)).collect())
            .collect();
        Self {
            patterns,
            norm_all_of,
            last_detected: HashMap::new(),
        }
    }

    /// Scan `rendered` for any configured pattern and return every kind-
    /// scoped edge transition since the last call. Patterns are tested in
    /// declaration order; the first match per kind wins (so list more-
    /// specific patterns first when stacking shared-chrome variants).
    pub fn check(&mut self, rendered: &str) -> Vec<PatternTransition> {
        // Normalize the rendered tail once so a marker that the TUI wrapped
        // across rows (rows are joined with `\n`) or padded with variable
        // cursor-forward gaps still matches the normalized pattern.
        let haystack = normalize_ws(rendered);

        // Find the winning pattern per kind in this scan.
        let mut hits: HashMap<PatternKind, &PermissionPattern> = HashMap::new();
        for (idx, p) in self.patterns.iter().enumerate() {
            if p.disabled || p.all_of.is_empty() {
                continue;
            }
            if hits.contains_key(&p.kind) {
                continue;
            }
            let needles = &self.norm_all_of[idx];
            if needles
                .iter()
                .all(|s| !s.is_empty() && haystack.contains(s.as_str()))
            {
                hits.insert(p.kind, p);
            }
        }

        let mut out = Vec::new();
        for kind in [
            PatternKind::Permission,
            PatternKind::Question,
            PatternKind::Working,
        ] {
            let prev = self.last_detected.get(&kind).cloned();
            let now = hits.get(&kind).map(|p| p.name.clone());
            match (prev, now) {
                (None, Some(name)) => {
                    self.last_detected.insert(kind, name.clone());
                    out.push(PatternTransition::Detected { kind, pattern_name: name });
                }
                (Some(prev_name), None) => {
                    self.last_detected.remove(&kind);
                    out.push(PatternTransition::Resolved {
                        kind,
                        pattern_name: prev_name,
                    });
                }
                (Some(prev_name), Some(new_name)) if prev_name != new_name => {
                    // Same kind, different pattern — emit a clean Resolved
                    // then Detected so downstream state stays in sync.
                    self.last_detected.insert(kind, new_name.clone());
                    out.push(PatternTransition::Resolved {
                        kind,
                        pattern_name: prev_name,
                    });
                    out.push(PatternTransition::Detected {
                        kind,
                        pattern_name: new_name,
                    });
                }
                _ => {}
            }
        }
        out
    }

    /// Force-clear recorded state for one kind without emitting. Used by
    /// the PTY processor when input-driven clearing has already updated
    /// `awaiting_*`, so the detector doesn't re-emit a redundant Resolved
    /// on the next tick if the prompt text is still on screen.
    pub fn force_clear(&mut self, kind: PatternKind) {
        self.last_detected.remove(&kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perm(name: &str, all_of: &[&str]) -> PermissionPattern {
        PermissionPattern {
            name: name.to_string(),
            kind: PatternKind::Permission,
            all_of: all_of.iter().map(|s| s.to_string()).collect(),
            disabled: false,
        }
    }

    fn quest(name: &str, all_of: &[&str]) -> PermissionPattern {
        PermissionPattern {
            name: name.to_string(),
            kind: PatternKind::Question,
            all_of: all_of.iter().map(|s| s.to_string()).collect(),
            disabled: false,
        }
    }

    #[test]
    fn empty_patterns_never_detect() {
        let mut d = PermissionDetector::new(no_patterns());
        assert!(d.check("Do you want to proceed?").is_empty());
    }

    #[test]
    fn marker_matches_when_wrapped_across_rows() {
        // The footer wrapped: "Esc to cancel ·" on one rendered row,
        // "Tab to amend" on the next — rows joined with '\n'. Normalization
        // collapses the newline to a space so the single-line pattern matches.
        let mut d = PermissionDetector::new(default_patterns());
        let wrapped = "some prompt body\nEsc to cancel ·\nTab to amend";
        let out = d.check(wrapped);
        assert!(
            out.iter().any(|t| matches!(
                t,
                PatternTransition::Detected { kind: PatternKind::Permission, .. }
            )),
            "wrapped permission footer should still be detected"
        );
    }

    #[test]
    fn marker_matches_with_padded_whitespace() {
        // Variable-width cursor-forward gaps render as runs of spaces.
        let mut d = PermissionDetector::new(default_patterns());
        let padded = "Esc to cancel   ·    Tab to amend";
        let out = d.check(padded);
        assert!(out.iter().any(|t| matches!(
            t,
            PatternTransition::Detected { kind: PatternKind::Permission, .. }
        )));
    }

    #[test]
    fn normalize_ws_collapses_runs_and_newlines() {
        assert_eq!(normalize_ws("a  b\n\tc "), "a b c");
        assert_eq!(normalize_ws("  leading and trailing  "), "leading and trailing");
        assert_eq!(normalize_ws("cancel · Tab"), "cancel · Tab");
    }

    #[test]
    fn no_match_returns_no_transitions() {
        let mut d = PermissionDetector::new(vec![perm("test", &["Do you want to proceed?"])]);
        assert!(d.check("normal output").is_empty());
    }

    #[test]
    fn first_match_emits_detected() {
        let mut d = PermissionDetector::new(vec![perm("test", &["Do you want to proceed?"])]);
        let out = d.check("foo Do you want to proceed? bar");
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            PatternTransition::Detected { kind: PatternKind::Permission, pattern_name } if pattern_name == "test"
        ));
    }

    #[test]
    fn repeated_match_silent() {
        let mut d = PermissionDetector::new(vec![perm("test", &["Do you want to proceed?"])]);
        let _ = d.check("Do you want to proceed?");
        assert!(d.check("Do you want to proceed?").is_empty());
    }

    #[test]
    fn loss_of_match_resolves() {
        let mut d = PermissionDetector::new(vec![perm("test", &["Do you want to proceed?"])]);
        let _ = d.check("Do you want to proceed?");
        let out = d.check("after the prompt");
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            PatternTransition::Resolved { kind: PatternKind::Permission, .. }
        ));
    }

    #[test]
    fn detect_resolve_detect_cycle() {
        let mut d = PermissionDetector::new(vec![perm("test", &["Do you want to proceed?"])]);
        assert!(matches!(
            d.check("Do you want to proceed?")[0],
            PatternTransition::Detected { .. }
        ));
        assert!(matches!(
            d.check("done")[0],
            PatternTransition::Resolved { .. }
        ));
        assert!(matches!(
            d.check("Do you want to proceed?")[0],
            PatternTransition::Detected { .. }
        ));
    }

    #[test]
    fn force_clear_silences_next_resolve() {
        let mut d = PermissionDetector::new(vec![perm("test", &["Do you want to proceed?"])]);
        let _ = d.check("Do you want to proceed?");
        d.force_clear(PatternKind::Permission);
        assert!(d.check("done").is_empty());
    }

    #[test]
    fn disabled_pattern_never_detects() {
        let mut p = perm("test", &["X"]);
        p.disabled = true;
        let mut d = PermissionDetector::new(vec![p]);
        assert!(d.check("X").is_empty());
    }

    #[test]
    fn all_of_requires_every_substring() {
        let mut d = PermissionDetector::new(vec![perm("multi", &["alpha", "beta"])]);
        assert!(d.check("alpha alone").is_empty());
        assert!(d.check("beta alone").is_empty());
        let out = d.check("alpha and beta together");
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], PatternTransition::Detected { .. }));
    }

    #[test]
    fn permission_and_question_are_independent() {
        // Both kinds can be in flight simultaneously without colliding.
        let patterns = vec![
            perm("perm", &["chrome"]),
            quest("q", &["chrome", "Question:"]),
        ];
        // Note: question listed second — but `quest` requires both
        // substrings; with only "chrome" present, perm wins (first kind
        // check), and quest's required content marker is missing.
        let mut d = PermissionDetector::new(patterns);
        let out = d.check("rendered: chrome only");
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            PatternTransition::Detected { kind: PatternKind::Permission, .. }
        ));

        // Now add the content marker — question kind also fires; existing
        // permission stays detected (no transition emitted on a steady
        // permission match).
        let out2 = d.check("rendered: chrome with Question: present");
        assert_eq!(out2.len(), 1);
        assert!(matches!(
            &out2[0],
            PatternTransition::Detected { kind: PatternKind::Question, .. }
        ));

        // Drop both — both Resolved fire.
        let out3 = d.check("nothing here");
        assert_eq!(out3.len(), 2);
        let kinds: Vec<PatternKind> = out3
            .iter()
            .map(|t| match t {
                PatternTransition::Resolved { kind, .. } => *kind,
                _ => panic!("expected Resolved"),
            })
            .collect();
        assert!(kinds.contains(&PatternKind::Permission));
        assert!(kinds.contains(&PatternKind::Question));
    }

    #[test]
    fn first_match_per_kind_wins() {
        // Two permission patterns; first listed should win on a shared
        // match.
        let patterns = vec![
            perm("first_perm", &["X"]),
            perm("second_perm", &["X"]),
        ];
        let mut d = PermissionDetector::new(patterns);
        let out = d.check("X");
        assert_eq!(out.len(), 1);
        if let PatternTransition::Detected { pattern_name, .. } = &out[0] {
            assert_eq!(pattern_name, "first_perm");
        } else {
            panic!("expected Detected");
        }
    }

    #[test]
    fn empty_all_of_is_skipped() {
        // Defense against a hand-edited patterns.json with `all_of: []` —
        // such a pattern would match every render otherwise.
        let mut p = perm("bad", &[]);
        p.all_of.clear();
        let mut d = PermissionDetector::new(vec![p]);
        assert!(d.check("anything").is_empty());
    }

    #[test]
    fn empty_substring_is_ignored() {
        // Likewise an empty individual entry shouldn't make a multi-
        // substring pattern always-true.
        let mut d = PermissionDetector::new(vec![perm("partial", &["", "real"])]);
        assert!(d.check("real").is_empty());
    }

    // Rendered-tail fixtures captured from Claude Code via
    // RUST_LOG=perm_capture=debug on 2026-06-09. Trimmed to the bottom of
    // each prompt (what the detector's tail window actually sees) and with
    // the ANSI already stripped, as the detector receives it.
    const PERMISSION_TAIL: &str = "   New-Item -ItemType File -Path \"delete.me\"\n\n \
        Do you want to proceed?\n > 1. Yes\n  2. Yes, and don't ask again\n   3. No\n\n \
        Esc to cancel · Tab to amend · ctrl+e to explain\n";
    const QUESTION_TAIL: &str = "Which primary color do you prefer?\n\n> 1. Red\n     \
        A warm, high-energy primary color.\n  2. Green\n  3. Blue\n  4. Type something.\n  \
        5. Chat about this\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n";

    #[test]
    fn shipped_defaults_detect_question_not_permission() {
        // The AskUserQuestion box must fire the Question kind only — its
        // footer shares "Esc to cancel" with the permission prompt but lacks
        // "Tab to amend", so the permission pattern must not also trip.
        let mut d = PermissionDetector::new(default_patterns());
        let out = d.check(QUESTION_TAIL);
        assert_eq!(out.len(), 1, "expected exactly one transition: {out:?}");
        assert!(matches!(
            &out[0],
            PatternTransition::Detected { kind: PatternKind::Question, pattern_name }
                if pattern_name == "claude_question"
        ));
    }

    #[test]
    fn shipped_defaults_detect_permission_not_question() {
        // The permission prompt must fire Permission only — it contains
        // neither "Enter to select" nor "Type something".
        let mut d = PermissionDetector::new(default_patterns());
        let out = d.check(PERMISSION_TAIL);
        assert_eq!(out.len(), 1, "expected exactly one transition: {out:?}");
        assert!(matches!(
            &out[0],
            PatternTransition::Detected { kind: PatternKind::Permission, pattern_name }
                if pattern_name == "claude_permission"
        ));
    }

    // Working-state footer while Claude is generating. Note lowercase "esc",
    // distinct from the permission prompt's "Esc to cancel".
    const WORKING_TAIL: &str = "✢ Pouncing… (4s · ↓ 176 tokens · thinking)\n\n\
        ────────────────────\n> \n────────────────────\n  esc to interrupt\n";

    #[test]
    fn shipped_defaults_detect_working_marker() {
        let mut d = PermissionDetector::new(default_patterns());
        let out = d.check(WORKING_TAIL);
        assert_eq!(out.len(), 1, "expected exactly one transition: {out:?}");
        assert!(matches!(
            &out[0],
            PatternTransition::Detected { kind: PatternKind::Working, pattern_name }
                if pattern_name == "claude_working"
        ));
        // Footer gone (back to an idle prompt) → Working resolves, which is
        // what releases the avatar to Idle.
        let out2 = d.check("> \n  ? for shortcuts\n");
        assert_eq!(out2.len(), 1, "expected exactly one transition: {out2:?}");
        assert!(matches!(
            &out2[0],
            PatternTransition::Resolved { kind: PatternKind::Working, .. }
        ));
    }

    #[test]
    fn working_marker_absent_from_permission_and_question() {
        // The Working marker must not trip on the other two prompt UIs —
        // when Claude is waiting on the user it is not "working".
        let mut d = PermissionDetector::new(default_patterns());
        assert!(!matches!(
            d.check(PERMISSION_TAIL).first(),
            Some(PatternTransition::Detected { kind: PatternKind::Working, .. })
        ));
        let mut d2 = PermissionDetector::new(default_patterns());
        assert!(!matches!(
            d2.check(QUESTION_TAIL).first(),
            Some(PatternTransition::Detected { kind: PatternKind::Working, .. })
        ));
    }
}
