//! Detection of in-tab prompt patterns (tool-use approvals, AskUserQuestion-
//! style multi-choice questions, future kinds). Drives `awaiting_permission`
//! and `awaiting_question` flags on `TabState`.
//!
//! Strategy: substring-match the rendered (ANSI-stripped) tail of the terminal
//! output against a user-editable list of [`PermissionPattern`] entries
//! loaded from `<exe-dir>/patterns.json` at startup. Each pattern lists
//! substrings that must ALL be present (`all_of`), substrings that must ALL be
//! absent (`none_of`), and a `kind` that determines which signal fires on the
//! absent→present (and present→absent) edges.
//!
//! # Where the rows come from
//!
//! **The engine is here; the grammar is not** (V40 locked decision 21). What a
//! pattern matches on is a transcription of somebody else's terminal chrome —
//! a footer composed at runtime from remappable chord labels, an option list,
//! a busy marker — and every word of it is a dependency on a product cImp does
//! not pin. Those rows are declared by the harness that owns them
//! (`HarnessPlugin::permission_patterns`, `harness/<id>/prompts.rs`), and
//! [`default_patterns`] is the neutral concatenation over the registry. The
//! reasoning behind each row — why the permission footer is matched loosely,
//! why the bare variant needs a second anchor, what re-capturing one costs —
//! travels with the row, in the plugin.
//!
//! Veto (`none_of`) terms are evaluated only from the start of the pattern's
//! own earliest `all_of` marker onward, not over the whole tail: the tail is a
//! scrollback window, so stale menu chrome scrolled ABOVE a live approval
//! prompt must not suppress it.
//!
//! Pattern characterization tip: enable `RUST_LOG=perm_capture=debug` to dump
//! the rendered tail the detector matches against; pick distinctive substrings
//! from the dump and add them to patterns.json. The `all_of` array supports
//! stacking a chrome marker (e.g. `to cancel ·`) with a content marker (e.g.
//! `Question:`) so two patterns can share the same prompt UI yet route to
//! different signals, and `none_of` subtracts the shapes that would otherwise
//! collide (that is how the permission patterns stay off the question menu).

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
    /// `Detected` maps to `HarnessOutputStarted`, `Resolved` to
    /// `HarnessOutputStopped`. Content-based so a thinking pause (no output
    /// for >0.5s) no longer collapses the avatar to Idle mid-work.
    Working,
}

// `PartialEq` is load-bearing for the loader: `patterns_file` compares a
// freshly parsed patterns.json against snapshots of the default sets shipped
// by earlier releases to decide whether the file is still pristine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Veto substrings: if ANY of these is present in the rendered tail the
    /// pattern does not match, however well `all_of` fits. Lets a loose
    /// chrome marker be subtracted down to one prompt shape — the permission
    /// patterns use it to stay off Claude's select menus, whose footers share
    /// the `… to cancel` hint but add `to select` / `to navigate`. Empty
    /// entries are ignored (a stray `""` must not disable the pattern), and
    /// the field is optional in patterns.json so hand-edited files written
    /// before it existed keep loading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub none_of: Vec<String>,
    /// When `true`, the pattern is loaded but never tested. Lets the
    /// shipped patterns.json carry template/example entries that the user
    /// flips on after capturing the live chrome.
    #[serde(default)]
    pub disabled: bool,
}

/// One plugin-declared prompt row — the `'static` twin of
/// [`PermissionPattern`] (V40 locked decision 21).
///
/// `PermissionPattern` owns `String`s because it is what the user's
/// `patterns.json` deserializes into; a plugin's rows are compile-time data, so
/// they are declared as slices and converted at the one place they enter the
/// detector. Same fields, same order, same meaning — the conversion is
/// mechanical on purpose, so "the shipped file is what the plugins declare"
/// stays a byte-level property.
#[derive(Debug, Clone, Copy)]
pub struct PatternSpec {
    /// See [`PermissionPattern::name`].
    pub name: &'static str,
    /// See [`PermissionPattern::kind`].
    pub kind: PatternKind,
    /// See [`PermissionPattern::all_of`].
    pub all_of: &'static [&'static str],
    /// See [`PermissionPattern::none_of`].
    pub none_of: &'static [&'static str],
    /// See [`PermissionPattern::disabled`].
    pub disabled: bool,
}

impl PatternSpec {
    /// The owned row the detector and the on-disk file speak.
    pub fn to_pattern(self) -> PermissionPattern {
        PermissionPattern {
            name: self.name.to_string(),
            kind: self.kind,
            all_of: self.all_of.iter().map(|s| (*s).to_string()).collect(),
            none_of: self.none_of.iter().map(|s| (*s).to_string()).collect(),
            disabled: self.disabled,
        }
    }
}

/// Built-in fallback patterns: **every registered harness's declared rows, in
/// registry order.**
///
/// Used when patterns.json is missing/corrupt at load time so detection still
/// works on fresh installs and after a hand-edit accident, and written verbatim
/// as the seed on first launch. Pattern matching is pure substring containment
/// against the ANSI-stripped tail.
///
/// **V40 locked decision 21.** This used to be a `vec![]` of six literals, four
/// transcribed from Claude Code's TUI and two placeholder rows for OpenCode's —
/// core production code holding one harness's terminal grammar. The rows moved
/// to `harness/<id>/prompts.rs` behind
/// [`crate::harness::HarnessPlugin::permission_patterns`] with their reasoning
/// intact; what is left here is the concatenation, which is the only part that
/// is true of harnesses in general. A harness added later contributes its rows
/// by being registered, and the seeded file grows by that fact alone.
///
/// Order is the registry's order, and it is load-bearing: the shipped
/// `scripts/patterns.default.json` is compared byte for byte against what this
/// composes (`the_shipped_seed_is_byte_identical`), and the detector tests
/// patterns in declaration order.
pub fn default_patterns() -> Vec<PermissionPattern> {
    crate::harness::registry::all()
        .filter_map(|h| h.plugin())
        .flat_map(|p| p.permission_patterns())
        .map(|p| p.to_pattern())
        .collect()
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
    /// Whitespace-normalized `none_of` substrings, index-aligned with
    /// `patterns`. Normalized the same way as `all_of` so a veto marker also
    /// survives a wrapped/padded render — a menu footer that wraps must still
    /// suppress the permission patterns.
    norm_none_of: Vec<Vec<String>>,
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
        let norm_none_of = patterns
            .iter()
            .map(|p| p.none_of.iter().map(|s| normalize_ws(s)).collect())
            .collect();
        Self {
            patterns,
            norm_all_of,
            norm_none_of,
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
            // Position of each `all_of` hit, so the veto below can be scoped to
            // the region this pattern actually matched. `None` from any needle
            // (absent or empty) fails the pattern outright.
            let mut match_start = usize::MAX;
            let mut all_present = true;
            for s in needles {
                match (!s.is_empty())
                    .then(|| haystack.find(s.as_str()))
                    .flatten()
                {
                    Some(at) => match_start = match_start.min(at),
                    None => {
                        all_present = false;
                        break;
                    }
                }
            }
            if !all_present {
                continue;
            }
            // Vetoes are checked after `all_of` so an empty `none_of` (the
            // common case) costs nothing. An empty veto string is skipped
            // rather than treated as "always present" — otherwise a stray
            // `""` in a hand-edited patterns.json would silently disable the
            // pattern, the same defense `all_of` already has.
            //
            // SCOPE (2026-08-05 review, LOW): a veto only counts from the start
            // of this pattern's own earliest marker onward, not across the whole
            // ~10-row tail. The tail is a scrollback window: an approval prompt
            // is routinely painted UNDER the leftovers of a model picker whose
            // footer still reads "↑/↓ to navigate · Esc to cancel", and a
            // whole-tail veto let that dead chrome suppress a live prompt (no
            // badge, no TTS). Chrome that belongs to the matched prompt sits at
            // or after where its markers begin — the option list comes first,
            // the footer under it — so this keeps every real
            // menu-must-not-fire-Permission case vetoed (the menu's own verbs
            // are inside its own region) while ignoring strictly-older rows.
            let region = &haystack[match_start..];
            let vetoes = &self.norm_none_of[idx];
            if vetoes
                .iter()
                .any(|s| !s.is_empty() && region.contains(s.as_str()))
            {
                continue;
            }
            hits.insert(p.kind, p);
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
                    out.push(PatternTransition::Detected {
                        kind,
                        pattern_name: name,
                    });
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
            none_of: Vec::new(),
            disabled: false,
        }
    }

    fn perm_veto(name: &str, all_of: &[&str], none_of: &[&str]) -> PermissionPattern {
        PermissionPattern {
            none_of: none_of.iter().map(|s| s.to_string()).collect(),
            ..perm(name, all_of)
        }
    }

    fn quest(name: &str, all_of: &[&str]) -> PermissionPattern {
        PermissionPattern {
            name: name.to_string(),
            kind: PatternKind::Question,
            all_of: all_of.iter().map(|s| s.to_string()).collect(),
            none_of: Vec::new(),
            disabled: false,
        }
    }

    /// Did this scan produce a `Detected` for the permission kind?
    fn detected_permission(out: &[PatternTransition]) -> bool {
        out.iter().any(|t| {
            matches!(
                t,
                PatternTransition::Detected {
                    kind: PatternKind::Permission,
                    ..
                }
            )
        })
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
                PatternTransition::Detected {
                    kind: PatternKind::Permission,
                    ..
                }
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
            PatternTransition::Detected {
                kind: PatternKind::Permission,
                ..
            }
        )));
    }

    #[test]
    fn normalize_ws_collapses_runs_and_newlines() {
        assert_eq!(normalize_ws("a  b\n\tc "), "a b c");
        assert_eq!(
            normalize_ws("  leading and trailing  "),
            "leading and trailing"
        );
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
            PatternTransition::Resolved {
                kind: PatternKind::Permission,
                ..
            }
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

    /// M11 (2026-08-05 review): the other half of `force_clear` — with the
    /// prompt STILL on screen, dropping the latch must make the next scan
    /// re-emit `Detected`. This is what stops a hook-driven `PermissionDenied`
    /// (which clears `awaiting_permission` eagerly, possibly while a genuine
    /// approval prompt is up) from stranding the badge/TTS: the detector is
    /// edge-triggered, so without the clear a still-matching screen emits
    /// nothing at all, forever.
    #[test]
    fn force_clear_lets_a_still_visible_prompt_be_redetected() {
        let mut d = PermissionDetector::new(default_patterns());
        let out = d.check(PERMISSION_TAIL);
        assert!(detected_permission(&out), "first detection: {out:?}");
        // Same screen again ⇒ latched, nothing emitted (this is the trap).
        assert!(
            d.check(PERMISSION_TAIL).is_empty(),
            "a latched pattern must stay silent while it keeps matching"
        );
        // The hook path clears the latch …
        d.force_clear(PatternKind::Permission);
        // … and the very next scan of the unchanged screen re-raises it.
        let again = d.check(PERMISSION_TAIL);
        assert!(
            detected_permission(&again),
            "prompt still on screen must be re-detected after force_clear: {again:?}"
        );
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
    fn none_of_vetoes_an_otherwise_matching_pattern() {
        let mut d = PermissionDetector::new(vec![perm_veto("veto", &["alpha"], &["beta"])]);
        // `all_of` satisfied but a veto substring is on screen → no match.
        assert!(d.check("alpha and beta together").is_empty());
        // Veto gone → matches.
        let out = d.check("alpha alone");
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], PatternTransition::Detected { .. }));
    }

    #[test]
    fn none_of_is_whitespace_normalized_like_all_of() {
        // A veto marker that the TUI wrapped across rows must still veto.
        let mut d = PermissionDetector::new(vec![perm_veto("veto", &["alpha"], &["beta gamma"])]);
        assert!(d.check("alpha\nbeta\ngamma").is_empty());
    }

    #[test]
    fn empty_none_of_entry_does_not_disable_pattern() {
        // A stray "" in a hand-edited patterns.json must not veto everything.
        let mut d = PermissionDetector::new(vec![perm_veto("veto", &["alpha"], &["", "beta"])]);
        let out = d.check("alpha alone");
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
            PatternTransition::Detected {
                kind: PatternKind::Permission,
                ..
            }
        ));

        // Now add the content marker — question kind also fires; existing
        // permission stays detected (no transition emitted on a steady
        // permission match).
        let out2 = d.check("rendered: chrome with Question: present");
        assert_eq!(out2.len(), 1);
        assert!(matches!(
            &out2[0],
            PatternTransition::Detected {
                kind: PatternKind::Question,
                ..
            }
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
        let patterns = vec![perm("first_perm", &["X"]), perm("second_perm", &["X"])];
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

    // ── the rendered-tail corpus (V40 Phase C, locked decision 28) ─────────
    //
    // These were four `const &str` literals here, captured from Claude Code
    // 2.1.221 via `RUST_LOG=perm_capture=debug` on 2026-06-09. They are
    // RECORDED INPUTS — the same class as the transcript lines the canaries
    // run on — so they live in the corpus beside the CLI version they came
    // from, under `fixtures/harness/claude/2.1.221/tui/`, with a MANIFEST
    // stating how they were captured and what was redacted. `include_str!`
    // embeds them so the test reads the committed bytes; `.gitattributes`
    // pins them LF, for the same reason the plugin goldens are pinned.
    const PERMISSION_TAIL: &str =
        include_str!("../../fixtures/harness/claude/2.1.221/tui/permission-prompt.txt");
    const QUESTION_TAIL: &str =
        include_str!("../../fixtures/harness/claude/2.1.221/tui/question-menu.txt");
    /// A Yes/No AskUserQuestion: the option list looks exactly like a
    /// permission prompt's (`1. Yes` / `2. No`) and the footer carries the same
    /// cancel hint. Only the menu verbs tell them apart — this is the shape a
    /// naive relaxation to bare `Esc to cancel` would break on.
    const YES_NO_QUESTION_TAIL: &str =
        include_str!("../../fixtures/harness/claude/2.1.221/tui/question-yes-no.txt");
    /// The working-state footer while Claude is generating. Note lowercase
    /// `esc`, distinct from the permission prompt's `Esc to cancel`.
    const WORKING_TAIL: &str =
        include_str!("../../fixtures/harness/claude/2.1.221/tui/working-footer.txt");

    /// The footer line of [`PERMISSION_TAIL`] as 2.1.221 composed it, with
    /// default keybindings, amend enabled and explain appended.
    const CAPTURED_FOOTER: &str = "Esc to cancel · Tab to amend · ctrl+e to explain\n";

    /// Everything above the footer in [`PERMISSION_TAIL`].
    ///
    /// Derived rather than stored: a fixture whose last byte is a significant
    /// trailing space is one an editor silently breaks. Combined with
    /// [`permission_tail`] to exercise the footer variants Claude Code 2.1.221
    /// can compose: remapped chord labels, the optional amend segment, and the
    /// optional appended explain segment.
    fn permission_body() -> &'static str {
        PERMISSION_TAIL
            .strip_suffix(CAPTURED_FOOTER)
            .expect("the captured permission tail ends with the footer it was captured with")
    }

    fn permission_tail(footer: &str) -> String {
        format!("{}{footer}\n", permission_body())
    }
    #[test]
    fn shipped_defaults_detect_question_not_permission() {
        // The AskUserQuestion box must fire the Question kind only — its
        // footer shares the cancel hint with the permission prompt, but puts
        // it last (nothing follows it, so no `to cancel ·`) and adds the menu
        // verbs the permission patterns veto on.
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

    #[test]
    fn shipped_defaults_match_every_footer_variant() {
        // The footer is composed at runtime: chord labels come from
        // ~/.claude/keybindings.json, `… to amend` is conditional and
        // `… to explain` may be appended. Every reachable shape must fire
        // Permission exactly once.
        for footer in [
            // (a) default Windows chrome.
            "Esc to cancel · Tab to amend",
            // (b) amend segment absent.
            "Esc to cancel",
            // (b+c) amend absent, explain appended.
            "Esc to cancel · ctrl+e to explain",
            // (c) both trailing segments present (as in PERMISSION_TAIL).
            "Esc to cancel · Tab to amend · ctrl+e to explain",
            // Rebound chords — the labels are user-remappable, the verbs
            // are not, so nothing here may depend on "Esc"/"Tab".
            "ctrl+q to cancel · shift+tab to amend",
            "ctrl+q to cancel",
        ] {
            let mut d = PermissionDetector::new(default_patterns());
            let out = d.check(&permission_tail(footer));
            assert_eq!(out.len(), 1, "footer {footer:?} → {out:?}");
            assert!(
                detected_permission(&out),
                "footer {footer:?} should fire Permission: {out:?}"
            );
        }
    }

    #[test]
    fn relaxed_footer_variants_survive_wrap_and_padding() {
        // Wrap/padding tolerance must hold for the relaxed markers too, not
        // just the old literal one: the `·` separator the primary pattern
        // anchors on is routinely split across rendered rows.
        for tail in [
            "some prompt body\nEsc to cancel ·\nctrl+e to explain",
            "some prompt body\nctrl+q to cancel\n·   shift+tab to amend",
            "some prompt body\n > 1. Yes\n  2. No\n\n   ctrl+q    to    cancel   ",
        ] {
            let mut d = PermissionDetector::new(default_patterns());
            let out = d.check(tail);
            assert!(detected_permission(&out), "tail {tail:?} → {out:?}");
        }
    }

    #[test]
    fn yes_no_question_menu_does_not_fire_permission() {
        let mut d = PermissionDetector::new(default_patterns());
        let out = d.check(YES_NO_QUESTION_TAIL);
        assert_eq!(out.len(), 1, "expected exactly one transition: {out:?}");
        assert!(
            !detected_permission(&out),
            "menu footer must not fire Permission: {out:?}"
        );
        assert!(matches!(
            &out[0],
            PatternTransition::Detected {
                kind: PatternKind::Question,
                ..
            }
        ));
    }

    #[test]
    fn menu_footers_never_fire_permission() {
        // Menu chrome variants that carry the cancel hint but aren't
        // approval prompts (no "Type something", so not even Question):
        // a plain picker and one that leads with the cancel segment.
        for tail in [
            "  ❯ 1. Sonnet\n    2. Opus\n\n  Enter to select · ↑/↓ to navigate · Esc to cancel\n",
            "  ❯ 1. Sonnet\n    2. Opus\n\n  Esc to cancel · Enter to select\n",
            "  ❯ 1. Yes\n    2. No\n\n  ↑/↓ to navigate · Esc to cancel\n",
        ] {
            let mut d = PermissionDetector::new(default_patterns());
            assert!(
                d.check(tail).is_empty(),
                "menu tail {tail:?} must produce no transition"
            );
        }
    }

    #[test]
    fn assistant_prose_mentioning_cancel_does_not_fire_permission() {
        // The scanned tail is the visible transcript, so Claude's own words
        // land in it. "to cancel" alone is therefore not a safe marker; the
        // shipped patterns require footer structure (a following `·`) or the
        // numbered option list.
        for tail in [
            "I'll wire the abort controller so the caller can pass a token to cancel \
             the request mid-flight.\n\n> \n  ? for shortcuts\n",
            "Run the build with --watch; press Ctrl+C to cancel.\n\n> \n  ? for shortcuts\n",
            "Here's the plan:\n  1. Add a cancel button to the toolbar\n  2. Wire the abort \
             signal so users can click to cancel\n\n> \n  ? for shortcuts\n",
        ] {
            let mut d = PermissionDetector::new(default_patterns());
            let out = d.check(tail);
            assert!(
                !detected_permission(&out),
                "prose tail {tail:?} must not fire Permission: {out:?}"
            );
        }
    }

    /// 2026-08-05 review (LOW): the combination the prose tests above omitted —
    /// Claude's own answer carrying BOTH a numbered "1. Yes" item AND the
    /// phrase "to cancel" inside the same ~1000-char tail. With
    /// `claude_permission_bare` anchored on a bare "1. Yes" this fired a
    /// permission badge + announcement on ordinary output; the option-line
    /// adjacency marker ("1. Yes 2.") is what tells prompt chrome from prose.
    #[test]
    fn prose_with_a_numbered_yes_and_a_cancel_mention_does_not_fire_permission() {
        for tail in [
            // A model answering two questions in a numbered list.
            "Two answers:\n  1. Yes, the abort controller is wired — call it to cancel the \
             in-flight request.\n  2. No, the retry budget is unchanged.\n\n> \n  ? for shortcuts\n",
            // Same shape with the cancel mention below the list.
            "Plan:\n  1. Yes/No prompt for the destructive branch\n  2. Wire the abort signal\n\n\
             Press Ctrl+C to cancel at any point.\n\n> \n  ? for shortcuts\n",
        ] {
            let mut d = PermissionDetector::new(default_patterns());
            let out = d.check(tail);
            assert!(
                !detected_permission(&out),
                "prose tail {tail:?} must not fire Permission: {out:?}"
            );
        }
        // …while the real cancel-only-footer prompt the pattern exists for
        // still fires (the option lines are adjacent).
        let mut d = PermissionDetector::new(default_patterns());
        let out = d.check(&permission_tail("Esc to cancel"));
        assert!(detected_permission(&out), "real bare prompt: {out:?}");
        assert!(
            matches!(&out[0], PatternTransition::Detected { pattern_name, .. }
                if pattern_name == "claude_permission_bare"),
            "the bare pattern is the one that must carry it: {out:?}"
        );
    }

    /// 2026-08-05 review (LOW): `none_of` used to be evaluated against the
    /// whole rendered tail, so a picker's footer still visible ABOVE a freshly
    /// painted approval prompt suppressed it — no badge, no announcement. The
    /// veto is now scoped to the region from the pattern's own earliest marker
    /// onward.
    #[test]
    fn stale_menu_chrome_above_a_prompt_does_not_veto_it() {
        // A model picker's leftovers, then the approval prompt under them.
        let picker = "  ❯ 1. Sonnet\n    2. Opus\n\n  Enter to select · ↑/↓ to navigate · \
                      Esc to cancel\n\n";
        for footer in ["Esc to cancel · Tab to amend", "Esc to cancel"] {
            let tail = format!("{picker}{}", permission_tail(footer));
            let mut d = PermissionDetector::new(default_patterns());
            let out = d.check(&tail);
            assert!(
                detected_permission(&out),
                "menu chrome above the prompt must not veto it (footer {footer:?}): {out:?}"
            );
        }
        // The veto itself is intact: the same picker chrome ALONE still fires
        // nothing (its own verbs sit inside its own match region).
        let mut d = PermissionDetector::new(default_patterns());
        assert!(
            d.check(picker).is_empty(),
            "picker alone must still be vetoed"
        );
        // And a picker whose footer leads with the cancel segment stays vetoed
        // too — the trailing menu verb is after the marker, so in region.
        let mut d2 = PermissionDetector::new(default_patterns());
        assert!(d2
            .check("  ❯ 1. Yes\n    2. No\n\n  Esc to cancel · Enter to select\n")
            .is_empty());
    }

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
            PatternTransition::Resolved {
                kind: PatternKind::Working,
                ..
            }
        ));
    }

    #[test]
    fn working_marker_absent_from_permission_and_question() {
        // The Working marker must not trip on the other two prompt UIs —
        // when Claude is waiting on the user it is not "working".
        let mut d = PermissionDetector::new(default_patterns());
        assert!(!matches!(
            d.check(PERMISSION_TAIL).first(),
            Some(PatternTransition::Detected {
                kind: PatternKind::Working,
                ..
            })
        ));
        let mut d2 = PermissionDetector::new(default_patterns());
        assert!(!matches!(
            d2.check(QUESTION_TAIL).first(),
            Some(PatternTransition::Detected {
                kind: PatternKind::Working,
                ..
            })
        ));
    }
}
