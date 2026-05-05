//! Detection of in-tab permission prompts (e.g. Claude Code's tool-use
//! approval UI). Drives the `awaiting_permission` flag on `TabState`.
//!
//! Strategy: simple substring match against the rendered (ANSI-stripped) tail
//! of the terminal output. Brittle to upstream prompt changes — patterns are a
//! single `const` slice, easy to update when Claude Code's UI shifts.
//!
//! Patterns characterized against Claude Code's file-create prompt on
//! 2026-05-05 (rendered tail capture). The prompt UI footer
//! `Esc to cancel · Tab to amend` is consistent across multi-choice approval
//! prompts and not part of normal output, so it's the primary match. The `·`
//! is U+00B7. If Claude Code revises its prompt chrome in a future release,
//! refresh these patterns by re-running with RUST_LOG=perm_capture=debug and
//! grabbing distinctive substrings from the rendered tail.

#[derive(Debug, Clone, Copy)]
pub struct PermissionPattern {
    /// Stable identifier for logging / debugging. Not user-visible.
    pub name: &'static str,
    /// Substring searched for in the rendered tail. Pure ASCII recommended;
    /// even when the upstream UI wraps text in ANSI styling, the cell-rendered
    /// tail strips that out.
    pub substring: &'static str,
    #[allow(dead_code)]
    pub description: &'static str,
}

/// Claude Code permission prompt patterns. Order doesn't matter — first match wins.
pub const CLAUDE_PERMISSION_PATTERNS: &[PermissionPattern] = &[
    PermissionPattern {
        name: "prompt_footer",
        // `·` is U+00B7 (middle dot). Cell-rendered tail preserves it verbatim.
        substring: "Esc to cancel · Tab to amend",
        description: "Footer chrome on multi-choice approval prompts",
    },
];

/// Empty pattern slice for tabs with no detector configured (e.g. aider in v2;
/// patterns deferred until characterized).
pub const NO_PATTERNS: &[PermissionPattern] = &[];

#[derive(Debug, PartialEq, Eq)]
pub enum PermissionDetectorResult {
    None,
    Detected { pattern_name: &'static str },
    Resolved,
}

pub struct PermissionDetector {
    patterns: &'static [PermissionPattern],
    last_detected: Option<&'static str>,
}

impl PermissionDetector {
    pub fn new(patterns: &'static [PermissionPattern]) -> Self {
        Self {
            patterns,
            last_detected: None,
        }
    }

    /// Scan `rendered` for any configured pattern. Returns `Detected` on the
    /// rising edge (None → match), `Resolved` on the falling edge (match →
    /// None), and `None` while steady. Repeated detections of the same pattern
    /// produce a single `Detected` until a `Resolved` clears state.
    pub fn check(&mut self, rendered: &str) -> PermissionDetectorResult {
        if self.patterns.is_empty() {
            return PermissionDetectorResult::None;
        }
        let hit = self
            .patterns
            .iter()
            .find(|p| rendered.contains(p.substring))
            .map(|p| p.name);

        match (self.last_detected, hit) {
            (None, Some(name)) => {
                self.last_detected = Some(name);
                PermissionDetectorResult::Detected { pattern_name: name }
            }
            (Some(_), None) => {
                self.last_detected = None;
                PermissionDetectorResult::Resolved
            }
            _ => PermissionDetectorResult::None,
        }
    }

    /// Force-clear without emitting. Used when input-driven clearing has
    /// already informed the state manager and we want to avoid a redundant
    /// `Resolved` on the next check if the prompt text is still on screen.
    #[allow(dead_code)]
    pub fn force_clear(&mut self) {
        self.last_detected = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATTERNS: &[PermissionPattern] = &[
        PermissionPattern {
            name: "test_pattern",
            substring: "Do you want to proceed?",
            description: "test",
        },
    ];

    #[test]
    fn empty_patterns_never_detect() {
        let mut d = PermissionDetector::new(NO_PATTERNS);
        assert_eq!(d.check("Do you want to proceed?"), PermissionDetectorResult::None);
    }

    #[test]
    fn no_match_returns_none() {
        let mut d = PermissionDetector::new(PATTERNS);
        assert_eq!(d.check("normal output"), PermissionDetectorResult::None);
    }

    #[test]
    fn first_match_detects() {
        let mut d = PermissionDetector::new(PATTERNS);
        assert_eq!(
            d.check("foo Do you want to proceed? bar"),
            PermissionDetectorResult::Detected { pattern_name: "test_pattern" },
        );
    }

    #[test]
    fn repeated_match_silent() {
        let mut d = PermissionDetector::new(PATTERNS);
        let _ = d.check("Do you want to proceed?");
        assert_eq!(
            d.check("Do you want to proceed?"),
            PermissionDetectorResult::None,
        );
    }

    #[test]
    fn loss_of_match_resolves() {
        let mut d = PermissionDetector::new(PATTERNS);
        let _ = d.check("Do you want to proceed?");
        assert_eq!(d.check("after the prompt"), PermissionDetectorResult::Resolved);
    }

    #[test]
    fn detect_resolve_detect_cycle() {
        let mut d = PermissionDetector::new(PATTERNS);
        assert!(matches!(
            d.check("Do you want to proceed?"),
            PermissionDetectorResult::Detected { .. }
        ));
        assert_eq!(d.check("done"), PermissionDetectorResult::Resolved);
        assert!(matches!(
            d.check("Do you want to proceed?"),
            PermissionDetectorResult::Detected { .. }
        ));
    }

    #[test]
    fn force_clear_silences_next_resolve() {
        let mut d = PermissionDetector::new(PATTERNS);
        let _ = d.check("Do you want to proceed?");
        d.force_clear();
        assert_eq!(d.check("done"), PermissionDetectorResult::None);
    }
}
