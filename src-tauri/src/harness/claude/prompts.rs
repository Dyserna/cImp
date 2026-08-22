//! V40 Phase C, locked decision 21 — **Claude Code's TUI prompt grammar.**
//!
//! Every string in this file is a transcription of somebody else's terminal
//! chrome. `PermissionDetector` (`processing/permission.rs`) is the engine —
//! substring matching, veto scoping, the per-kind edge machine — and it is
//! harness-neutral; what it matches *on* is a dependency on a product cImp
//! neither pins nor ships, which is why the rows live here and reach core
//! through `HarnessPlugin::permission_patterns`.
//!
//! # Why the permission footer is matched loosely
//!
//! Claude Code composes the permission prompt's footer at runtime from
//! *remappable* chord labels (`~/.claude/keybindings.json` can turn `Esc` and
//! `Tab` into any other chord text), the `… to amend` segment is conditional
//! (it can be absent entirely), and an extra `… to explain` segment may be
//! appended. So the old exact marker [`CLAUDE_FOOTER`] only ever described the
//! default-keybindings, amend-enabled case. What is stable is the *grammar*:
//! `<chord> to cancel [· <chord> to amend] [· <chord> to explain]` — the cancel
//! hint is always the FIRST segment.
//!
//! The obvious relaxation (match bare `to cancel`) is unsafe twice over:
//! Claude's select-menu chrome ends with `… · Esc to cancel` (last segment),
//! and ordinary assistant prose ("press Ctrl+C to cancel the build") contains
//! the phrase too. The shipped defaults therefore express the permission
//! prompt as two OR'd patterns, each anchored on structure the menus and prose
//! don't have, and both refusing to match when menu-only verbs (`to select`,
//! `to navigate`) are on screen:
//!
//! 1. `claude_permission` — `to cancel ·`: the cancel hint is followed by
//!    another footer segment, which only happens when cancel comes FIRST.
//!    Chord-rebind proof and amend/explain agnostic.
//! 2. `claude_permission_bare` — `to cancel` + `1. Yes 2.`: covers the footer
//!    shape where cancel is the only segment, using the prompt's numbered
//!    Yes/No option list as the corroborating anchor. The anchor spans the
//!    first TWO option lines (they are adjacent after whitespace
//!    normalization), which is structure Claude's own prose does not have — a
//!    plain `1. Yes` also appears in a numbered list the model writes, and
//!    such a list can easily mention "to cancel" too.
//!
//! # Why the screen scraper counts characters, not bytes
//!
//! `processing::screen`'s rendered-tail window is sized in **chars** because
//! Claude's prompt chrome is full of multibyte glyphs: a byte window would cut
//! a footer mid-glyph and a marker would stop matching for reasons that have
//! nothing to do with the prompt. That is a fact about this TUI, recorded here
//! beside the markers it protects; the window itself stays neutral machinery.
//!
//! Pattern characterization: `RUST_LOG=perm_capture=debug` dumps the rendered
//! tail the detector matches against.

use crate::processing::permission::{PatternKind, PatternSpec};

/// The literal footer marker cImp shipped **before** the grammar-based match.
///
/// Production data, not a test fixture: it is the `all_of` of the
/// `claude_permission` row in every default set from v0.4.0 to v0.21.x, and
/// pristine-file reconciliation still compares against those sets. Kept in the
/// harness whose footer it transcribes, so a reader looking for "what did we
/// depend on" finds the retired dependency in the same place as the live one.
pub const CLAUDE_FOOTER: &str = "Esc to cancel · Tab to amend";

/// The placeholder the pre-V19 default sets shipped as a worked example.
const ALT_EXAMPLE: &str = "<replace with a substring unique to this prompt shape>";

/// Claude Code's shipped prompt rows, in the order the seed file writes them.
pub const PATTERNS: &[PatternSpec] = &[
    // Claude Code's tool-approval footer. Matched by grammar, not by the
    // literal default chrome: the chord labels are remappable via
    // ~/.claude/keybindings.json, the `… to amend` segment is optional and
    // a `… to explain` segment may be appended (verified against Claude
    // Code 2.1.221). What holds is that the cancel hint is the FIRST
    // segment, so something always follows it — hence `to cancel ·`.
    // Claude's select menus put their cancel hint LAST (no trailing `·`),
    // and `none_of` vetoes them outright. See the module docs.
    PatternSpec {
        name: "claude_permission",
        kind: PatternKind::Permission,
        // `·` is U+00B7 (middle dot). Cell-rendered tail preserves it.
        all_of: &["to cancel ·"],
        none_of: &["to select", "to navigate"],
        disabled: false,
    },
    // Second permission pattern — patterns of the same `kind` act as
    // alternatives (OR), so this one covers the shape the first misses:
    // a footer whose only segment is the cancel hint (no amend, no
    // explain), where there is no trailing `·` to anchor on. Bare
    // `to cancel` would also fire on assistant prose ("…press Ctrl+C to
    // cancel…"), so it is paired with the prompt's numbered option list.
    //
    // The option marker is `1. Yes 2.` — the FIRST TWO OPTION LINES, not a
    // bare `1. Yes` (2026-08-05 review, LOW). The tail is whitespace-
    // normalized before matching (`normalize_ws`), so consecutive option
    // rows join with a single space and this reads "option 1 is `Yes` and
    // option 2 starts immediately after". Claude's approval prompt always
    // renders >= 2 bare options (`1. Yes` / `2. Yes, and don't ask again` /
    // `3. No`), and the selector caret sits BEFORE `1.`, so every real
    // footer variant still matches. Assistant prose cannot: a numbered list
    // item reading "1. Yes, …" continues with its own text before the next
    // number, which breaks the adjacency — and prose carrying both
    // "1. Yes" and "to cancel" was the documented false-positive shape.
    // If a future release adds per-option description lines to the approval
    // prompt (as the AskUserQuestion box has), this marker stops matching —
    // re-capture with RUST_LOG=perm_capture=debug; the primary
    // `claude_permission` pattern covers every multi-segment footer meanwhile.
    //
    // Also the worked example for declaring extra prompt shapes: copy the
    // entry, change `name`, and edit `all_of`/`none_of`.
    PatternSpec {
        name: "claude_permission_bare",
        kind: PatternKind::Permission,
        all_of: &["to cancel", "1. Yes 2."],
        none_of: &["to select", "to navigate"],
        disabled: false,
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
    //
    // Cross-pattern invariant: "Enter to select" here and the permission
    // patterns' `none_of: ["to select", …]` are two views of the same
    // menu chrome. If this marker is ever re-captured, re-check that the
    // permission veto still names a substring the new menu footer has —
    // otherwise the question UI starts firing Permission as well.
    PatternSpec {
        name: "claude_question",
        kind: PatternKind::Question,
        all_of: &["Enter to select", "Type something"],
        none_of: &[],
        disabled: false,
    },
    // Claude's "busy" footer, shown only while a request is in flight
    // (and never during a permission/question prompt, where Claude is
    // waiting on the user). Drives the avatar's Thinking state: present
    // = working, absent = done. The verb distinguishes it from the
    // permission prompt: this footer offers "interrupt", never "cancel",
    // so it cannot trip the permission patterns (which key off
    // "to cancel"). If a future Claude Code release reworks this footer
    // — in particular if it starts saying "to cancel" — re-capture with
    // RUST_LOG=perm_capture=debug and update the marker here.
    PatternSpec {
        name: "claude_working",
        kind: PatternKind::Working,
        all_of: &["esc to interrupt"],
        none_of: &[],
        disabled: false,
    },
];

/// Claude's rows in one **named earlier era** of the shipped `patterns.json`.
///
/// The era labels are cImp's own release ranges (core owns the list of them),
/// but the rows are this harness's — which is what makes pristine-file
/// reconciliation of a file written in 2026-05 a question the plugin answers
/// rather than a table in core naming three harnesses, one of them retired.
///
/// Legacy rows predate the `none_of` field, which serde defaults to empty when
/// the key is absent, so every one is declared with an empty veto list — that
/// is exactly what a parsed legacy file yields.
pub fn legacy_patterns(era: &str) -> &'static [PatternSpec] {
    match era {
        "v0.22.0..v0.49.1" | "v0.7.0..v0.21.x" => V070,
        "v0.6.3..v0.6.x" | "v0.4.0" => V040,
        _ => &[],
    }
}

/// The v0.4.0 shape: the literal footer, the worked example, and a question
/// *template* nobody had characterized yet.
const V040: &[PatternSpec] = &[
    PatternSpec {
        name: "claude_permission",
        kind: PatternKind::Permission,
        all_of: &[CLAUDE_FOOTER],
        none_of: &[],
        disabled: false,
    },
    PatternSpec {
        name: "claude_permission_alt_example",
        kind: PatternKind::Permission,
        all_of: &[ALT_EXAMPLE],
        none_of: &[],
        disabled: true,
    },
    PatternSpec {
        name: "claude_question_template",
        kind: PatternKind::Question,
        all_of: &[
            CLAUDE_FOOTER,
            "<replace with a substring unique to question prompts>",
        ],
        none_of: &[],
        disabled: true,
    },
];

/// The v0.7.0 shape: the template replaced by the real `claude_question`, plus
/// `claude_working`. Still the literal footer for the permission row — the
/// grammar-based match is what the *current* set (`PATTERNS`) introduced.
const V070: &[PatternSpec] = &[
    PatternSpec {
        name: "claude_permission",
        kind: PatternKind::Permission,
        all_of: &[CLAUDE_FOOTER],
        none_of: &[],
        disabled: false,
    },
    PatternSpec {
        name: "claude_permission_alt_example",
        kind: PatternKind::Permission,
        all_of: &[ALT_EXAMPLE],
        none_of: &[],
        disabled: true,
    },
    PatternSpec {
        name: "claude_question",
        kind: PatternKind::Question,
        all_of: &["Enter to select", "Type something"],
        none_of: &[],
        disabled: false,
    },
    PatternSpec {
        name: "claude_working",
        kind: PatternKind::Working,
        all_of: &["esc to interrupt"],
        none_of: &[],
        disabled: false,
    },
];
