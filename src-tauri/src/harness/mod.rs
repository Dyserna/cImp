//! V35 — the harness seam.
//!
//! cImp rides two user-installed, aggressively self-updating CLIs it does not
//! pin. V16 built drift *detection* (eight statistical rules in
//! [`crate::advisor`], all of them lagging); V35 builds *declaration* — one
//! machine-readable list of what cImp actually depends on, ranked by the seam
//! it sits in rather than by the feature it serves.
//!
//! Phase A shipped [`contract`], the registry plus the consistency tests that
//! keep it from rotting. Phase B adds [`canary`]: the L1 fixture canaries for
//! the four Tier-C readers, which assert that a recorded input still produces
//! **substantive** output — not merely that it parses, since every one of those
//! readers is deliberately lenient and answers a rename with zeros and empty
//! strings rather than an error. (Phase B shipped it `#[cfg(test)]`; Phase F
//! made the four positive assertions runtime functions over embedded fixtures,
//! with the `cargo test` canaries as thin wrappers so the two can never check
//! different things.)
//!
//! Phase D adds [`probe`], the registry's **first runtime consumer**: the L2
//! live probe behind `cimp --harness-canary`, which drives the *installed*
//! CLIs instead of committed fixtures. L1 asks "do we still parse the shape we
//! recorded"; L2 asks "is the recorded shape still real". Neither subsumes the
//! other — a fixture keeps L1 green forever while upstream moves, and L2 cannot
//! run in CI nor tell a reader regression from an upstream change.
//!
//! Phase F adds [`verify`], which joins the two layers into one automatic run:
//! when the installed Claude Code version changes, the embedded L1 canaries and
//! the L2 probes run in the background and — if nothing FAILED —
//! `claude_last_verified` advances on its own. The version tripwire
//! (`drift.harness_version.v1`), which used to fire on every auto-update and so
//! trained the user to click *Mark verified* reflexively, becomes the
//! cannot-verify fallback; what a user sees instead is a notice naming the
//! capability that actually broke.
//!
//! Still to come: the Settings *Harness health* panel (Phase G) and the
//! capture-on-success corpus (Phase H).
//!
//! Design: `docs/MILESTONE-V35-harness-resilience.md` (the why and the locked
//! decisions), `docs/DESIGN-harness-capability-matrix.md` (the types and the
//! seed rows), `docs/DESIGN-harness-drift-canaries.md` (the canary layers, § 5
//! for Phase F's replacement of the noisy tripwire).

pub mod canary;
pub mod contract;
pub mod probe;
pub mod verify;
