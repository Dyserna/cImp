//! V35 — the harness seam.
//!
//! cImp rides two user-installed, aggressively self-updating CLIs it does not
//! pin. V16 built drift *detection* (eight statistical rules in
//! [`crate::advisor`], all of them lagging); V35 builds *declaration* — one
//! machine-readable list of what cImp actually depends on, ranked by the seam
//! it sits in rather than by the feature it serves.
//!
//! Phase A ships exactly one thing: [`contract`], the registry plus the
//! consistency tests that keep it from rotting. There is deliberately **no
//! runtime consumer yet** — the canary suite (Phase B–D), the Advisor and
//! feature gating (Phase E) and the Settings *Harness health* panel (Phase G)
//! are the consumers the registry is being seeded for. Until they land, the
//! tests in `contract.rs` are the consumers: they are what makes an unrecorded
//! fragile dependency a build failure instead of a comment.
//!
//! Design: `docs/MILESTONE-V35-harness-resilience.md` (the why and the locked
//! decisions), `docs/DESIGN-harness-capability-matrix.md` (the types and the
//! seed rows).

pub mod contract;
