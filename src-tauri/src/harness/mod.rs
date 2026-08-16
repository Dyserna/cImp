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
//! Phase G adds [`health`], the registry's read-model for the Settings
//! *Harness health* panel: every row's tier, contract, degradation, coverage
//! marks and TCB flags, joined against the Phase E gate verdicts, the Phase F
//! record on disk and the last run made in this process — computed here so the
//! panel renders an answer instead of re-deriving one, and so "what is broken
//! right now" stops requiring a source read.
//!
//! Phase H adds [`capture`], the corpus: a probe run that found no drift files
//! the payloads it read — scrubbed, stamped with the CLI version it saw them on
//! — under the user's data directory, and `cimp --harness-capture` does the
//! same on demand including for a run that *did* drift (into a marked sibling
//! directory, never over the known-good one). It exists for a single moment:
//! when something breaks, the first diagnostic should be a diff between the last
//! known-good capture and today's, rather than reverse-engineering the shape
//! from symptoms.
//!
//! Phase I adds [`chp`] — **CHP, the cImp Harness Protocol**: the name, the
//! version and the capability handshake for the loopback wire both harnesses
//! have been speaking since V10. Phases A–H made the dependency surface
//! *enumerable*; this makes the seam itself *declared*, so everything above L2
//! types against a protocol instead of against harness-shaped Rust. Its runtime
//! consumer is stale-artifact detection: a plugin is written to disk at tab
//! launch and outlives the binary that wrote it, and `chp` is what turns that
//! mismatch from a mysterious functional failure (V32 hit it four times) into a
//! line in the *Harness health* panel. The wire contract is `docs/CHP.md`;
//! handlers stay in `offload/loopback.rs`.
//!
//! Design: `docs/MILESTONE-V35-harness-resilience.md` (the why and the locked
//! decisions), `docs/DESIGN-harness-capability-matrix.md` (the types and the
//! seed rows), `docs/DESIGN-harness-drift-canaries.md` (the canary layers, § 5
//! for Phase F's replacement of the noisy tripwire, § 3.2 and § 4.1 for the
//! capture dir's trust boundary).

pub mod canary;
pub mod capture;
pub mod chp;
pub mod contract;
pub mod health;
pub mod probe;
pub mod verify;
