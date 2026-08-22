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
//! Phase J adds [`claude::hook`] and deletes five binaries. Claude's L1 was the
//! odd one out: where OpenCode gets one generated plugin that speaks CHP
//! directly, Claude got five stateless shim executables that existed only to
//! carry a payload from stdin to a socket and a reply back to stdout. Claude
//! Code 2.1.63's `type: "http"` hooks let the harness POST that payload itself,
//! so the shims are gone and their payload mechanics moved into this module —
//! on the receiving end of the same wire. Claude now sends a `/session/hello`
//! too, which is what turns Phase I's staleness detection on for its tabs.
//!
//! Phase K moves the whole surface in here and locks the shape with tests
//! ([`layering`]). Until it, "harness knowledge" was spread across nine
//! locations, none named for it — `oob/{claude,opencode,mod}.rs`,
//! `statusline/mod.rs`, `tabs/config.rs`, `offload/toolclass.rs` — so the
//! layering existed only in a design document, which is the state in which a
//! layering rots. It is now a directory a contributor can be pointed at, with
//! one sub-directory per harness ([`claude`], [`opencode`]) and a
//! harness-neutral core beside them. Nothing about behaviour changed: every
//! moved function, string and test kept its exact text, and the four tests in
//! [`layering`] are what stop the next feature from putting a harness literal
//! back outside this tree. `README.md` in this directory is the entry point for
//! "I want to add a harness".
//!
//! Design: `docs/MILESTONE-V35-harness-resilience.md` (the why and the locked
//! decisions), `docs/DESIGN-harness-capability-matrix.md` (the types and the
//! seed rows), `docs/DESIGN-harness-drift-canaries.md` (the canary layers, § 5
//! for Phase F's replacement of the noisy tripwire, § 3.2 and § 4.1 for the
//! capture dir's trust boundary),
//! `docs/DESIGN-harness-plugin-architecture.md` § 7 step 2 (Phase J), §§ 4 and
//! 4.1 (Phase K: the target tree and the layering tests) and § 5.1 (Phase M:
//! the emitted artifact as a template file — [`render`]).

pub mod _retired;
pub mod canary;
pub mod capture;
pub mod chp;
pub mod claude;
pub mod contract;
pub mod health;
pub mod info;
pub mod ingress;
pub mod instructions;

#[cfg(test)]
mod layering;
pub mod native;
pub mod opencode;
pub mod plugin;
pub mod probe;
pub mod reader;
pub mod registry;
pub mod render;
pub mod verify;

pub use plugin::{HarnessPlugin, InputProfile};
pub use reader::{spawn, OobContext, OobSpec};
pub use registry::{HarnessId, PerHarness, DEFAULT_HARNESS, HARNESSES};
#[cfg(test)]
pub use registry::per_harness_for_test;

/// **The harness-neutral input-profile lookup** (V39 locked decision 16, moved
/// behind the plugin by V40 Phase A, amendment 0-a).
///
/// `id` is the CHP `agent` discriminator. `None` is a first-class answer, not an
/// error: it is what makes "a harness without an `input.rs` is not a valid
/// worker" a fail-closed property instead of a panic — and it is now also the
/// answer for a harness nobody registered, which used to be OpenCode's.
pub fn input_profile(id: &str) -> Option<InputProfile> {
    HarnessId::from_id(id)?.plugin()?.input_profile()
}

/// **Whether `model` is a harness's fabricated pseudo-model** rather than a
/// model anybody ran (V40 Phase D, locked decision 20).
///
/// A union over every registered harness's declared sentinels, because a usage
/// row records the model id but not which harness's vocabulary it came from,
/// and the two shipped harnesses record the SAME provider's model ids — the id
/// is provider knowledge, and only the sentinel is a harness's. Iterating the
/// registry is what stops this from being the `"<synthetic>"` literal it was in
/// two `graph/index.rs` queries.
///
/// Empty ⇒ nothing is a sentinel, which is the fail-open direction on purpose:
/// a harness that declares none must not have its real models filtered out.
pub fn is_model_sentinel(model: &str) -> bool {
    registry::all()
        .filter_map(|h| h.plugin())
        .filter_map(|p| p.usage_source())
        .any(|s| s.model_sentinels().contains(&model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin::PasteMode;

    /// The sentinel filter answers from DECLARATIONS, and answers `false` for
    /// a real model id — the fail-open direction, since a wrongly filtered
    /// model silently disappears from every cost readout.
    #[test]
    fn model_sentinels_come_from_the_plugins() {
        assert!(
            registry::all()
                .filter_map(|h| h.plugin())
                .filter_map(|p| p.usage_source())
                .any(|s| !s.model_sentinels().is_empty()),
            "at least one shipped harness declares a pseudo-model; if that stops being true, \
             the two graph queries that filter on it have nothing to filter and should say so"
        );
        for real in ["claude-opus-4-8", "anthropic/claude-sonnet-4-5", "", "synthetic"] {
            assert!(!is_model_sentinel(real), "{real:?} is a model, not a sentinel");
        }
    }

    /// **Every harness the registry knows has a profile, and an unknown id has
    /// none.** The second half is the fail-closed direction: a tab pointed at
    /// some other CLI must not inherit another harness's paste rules.
    #[test]
    fn every_registry_harness_declares_a_profile_and_nothing_else_does() {
        let ids = registry::harness_ids();
        assert!(!ids.is_empty(), "the registry names at least one harness");
        for id in &ids {
            assert!(
                input_profile(id).is_some(),
                "harness `{id}` has registry rows but no input profile — it cannot be a \
                 delegation worker, and the gate must be the thing that says so"
            );
        }
        for unknown in ["", "aider", "Claude", "claude.exe", "opencode-2"] {
            assert!(
                input_profile(unknown).is_none(),
                "`{unknown}` resolved to a profile it must not have"
            );
        }
    }

    /// Both shipped profiles submit with CR and paste bracketed. Pinned because
    /// the engine's write order (paste → settle → submit) is only correct for a
    /// profile shaped this way; a future `Raw` harness needs the engine's
    /// ordering test re-read, not just a new constant.
    #[test]
    fn the_shipped_profiles_are_bracketed_and_submit_with_cr() {
        for id in registry::harness_ids() {
            let p = input_profile(id).expect("declared above");
            assert_eq!(p.paste, PasteMode::Bracketed, "{id}");
            assert_eq!(p.submit, b"\r", "{id}");
            assert!(p.settle_ms > 0, "{id}: a zero settle is the defect this field exists for");
            assert!(p.max_paste_bytes >= 4096, "{id}: the bound must admit a real request");
        }
    }
}
