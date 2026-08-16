//! V35 — the harness seam.
//!
//! cImp rides two user-installed, aggressively self-updating CLIs it does not
//! pin. V16 built drift *detection* (eight statistical rules in
//! [`crate::advisor`], all of them lagging); V35 builds *declaration* — one
//! machine-readable list of what cImp actually depends on, ranked by the seam
//! it sits in rather than by the feature it serves.
//!
//! Phase A shipped [`contract`], the registry plus the consistency tests that
//! keep it from rotting. Phase B adds `canary` (test-only): the L1 fixture
//! canaries for the four Tier-C readers, which assert that a recorded input
//! still produces **substantive** output — not merely that it parses, since
//! every one of those readers is deliberately lenient and answers a rename with
//! zeros and empty strings rather than an error.
//!
//! Phase D adds [`probe`], the registry's **first runtime consumer**: the L2
//! live probe behind `cimp --harness-canary`, which drives the *installed*
//! CLIs instead of committed fixtures. L1 asks "do we still parse the shape we
//! recorded"; L2 asks "is the recorded shape still real". Neither subsumes the
//! other — a fixture keeps L1 green forever while upstream moves, and L2 cannot
//! run in CI nor tell a reader regression from an upstream change.
//!
//! Still to come: the Advisor and feature gating (Phase E) and the Settings
//! *Harness health* panel (Phase G). Until they land, the tests in
//! `contract.rs` and `canary.rs` remain the consumers that make an unrecorded
//! fragile dependency, or a silently regressed reader, a build failure instead
//! of a comment.
//!
//! Design: `docs/MILESTONE-V35-harness-resilience.md` (the why and the locked
//! decisions), `docs/DESIGN-harness-capability-matrix.md` (the types and the
//! seed rows), `docs/DESIGN-harness-drift-canaries.md` (the canary layers).

#[cfg(test)]
mod canary;
pub mod contract;
pub mod probe;
