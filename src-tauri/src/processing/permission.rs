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
/// covers Claude Code chrome and Aider's prompts.
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
        // Question template: ships disabled because the content marker
        // varies across Claude Code releases. Capture the live chrome with
        // `RUST_LOG=perm_capture=debug`, fill in the marker, and flip
        // `disabled` to false. Patterns are tested in declaration order
        // so listing this entry BEFORE the bare permission pattern (when
        // enabled) makes the more-specific kind win for shared chrome.
        PermissionPattern {
            name: "claude_question_template".to_string(),
            kind: PatternKind::Question,
            all_of: vec![
                "Esc to cancel · Tab to amend".to_string(),
                "<replace with a substring unique to question prompts>".to_string(),
            ],
            disabled: true,
        },
        // Aider's "Apply edits?" prompt. Aider prints a (Y)es/(N)o/etc.
        // option line; the trailing `(Y)es` is the most stable marker
        // across versions.
        PermissionPattern {
            name: "aider_apply_edits".to_string(),
            kind: PatternKind::Permission,
            all_of: vec![
                "Apply edits?".to_string(),
                "(Y)es".to_string(),
            ],
            disabled: false,
        },
        // Aider's "Add file to chat?" prompt. The phrasing is "Add … to
        // the chat?" — the open-ended middle is matched by absence of a
        // substring constraint there.
        PermissionPattern {
            name: "aider_add_to_chat".to_string(),
            kind: PatternKind::Permission,
            all_of: vec![
                "Add ".to_string(),
                " to the chat?".to_string(),
            ],
            disabled: false,
        },
        // Aider's "Run shell command?" confirmation. Used when the user
        // (or the model) proposes a shell action via /run.
        PermissionPattern {
            name: "aider_run_shell".to_string(),
            kind: PatternKind::Permission,
            all_of: vec!["Run shell command?".to_string()],
            disabled: false,
        },
    ]
}

/// Empty pattern list for tabs that don't run detection (Shell tabs).
/// Kept as a named helper for callers that prefer the intent over a bare
/// `Vec::new()`. Test code uses it; production code constructs via
/// `Vec::new()` inline.
#[allow(dead_code)]
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
    /// Currently-detected pattern name per kind. Tracked per kind so a
    /// permission edge and a question edge don't compete — both can be
    /// "in flight" at once if upstream UI ever interleaves them.
    last_detected: HashMap<PatternKind, String>,
}

impl PermissionDetector {
    pub fn new(patterns: Vec<PermissionPattern>) -> Self {
        Self {
            patterns,
            last_detected: HashMap::new(),
        }
    }

    /// Scan `rendered` for any configured pattern and return every kind-
    /// scoped edge transition since the last call. Patterns are tested in
    /// declaration order; the first match per kind wins (so list more-
    /// specific patterns first when stacking shared-chrome variants).
    pub fn check(&mut self, rendered: &str) -> Vec<PatternTransition> {
        // Find the winning pattern per kind in this scan.
        let mut hits: HashMap<PatternKind, &PermissionPattern> = HashMap::new();
        for p in &self.patterns {
            if p.disabled || p.all_of.is_empty() {
                continue;
            }
            if hits.contains_key(&p.kind) {
                continue;
            }
            if p.all_of.iter().all(|s| !s.is_empty() && rendered.contains(s.as_str())) {
                hits.insert(p.kind, p);
            }
        }

        let mut out = Vec::new();
        for kind in [PatternKind::Permission, PatternKind::Question] {
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
    /// the state manager when input-driven clearing has already updated
    /// `awaiting_*`, so the detector doesn't re-emit a redundant Resolved
    /// on the next tick if the prompt text is still on screen.
    #[allow(dead_code)]
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
}
