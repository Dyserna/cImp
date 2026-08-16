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
//! There is still deliberately **no runtime consumer** — the live probe (Phase
//! D), the Advisor and feature gating (Phase E) and the Settings *Harness
//! health* panel (Phase G) are what the registry is being seeded for. Until
//! they land, the tests in `contract.rs` and `canary.rs` are the consumers:
//! they are what makes an unrecorded fragile dependency, or a silently
//! regressed reader, a build failure instead of a comment.
//!
//! Design: `docs/MILESTONE-V35-harness-resilience.md` (the why and the locked
//! decisions), `docs/DESIGN-harness-capability-matrix.md` (the types and the
//! seed rows), `docs/DESIGN-harness-drift-canaries.md` (the canary layers).

#[cfg(test)]
mod canary;
pub mod contract;
