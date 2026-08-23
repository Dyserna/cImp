//! **Retired harnesses: data that outlives the code.**
//!
//! V40 Phase C, locked decision 21. A harness cImp no longer runs can still
//! have left something on a user's disk that a live code path compares against.
//! This directory is where that data lives — no plugin, no descriptor, no
//! registry row, nothing that could make a retired harness look supported, and
//! nothing executable beyond the lookup that hands the rows over.
//!
//! One inhabitant so far: [`aider`].

pub mod aider;

/// Every retired harness's rows for one named `patterns.json` era, in the order
/// the seeder wrote them.
///
/// Consumed by `processing::patterns_file::legacy_default_sets` **after** the
/// registered plugins' rows, which is the order every shipped set had: a
/// retired harness's rows were appended to Claude's, never interleaved.
pub fn legacy_permission_patterns(
    era: &str,
) -> Vec<&'static crate::processing::permission::PatternSpec> {
    aider::legacy_patterns(era).iter().collect()
}
