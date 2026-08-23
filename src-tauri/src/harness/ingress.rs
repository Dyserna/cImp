//! V40 Phase C, locked decisions 15 and 22 — **the neutral lookups over the
//! harnesses' own loopback ingress.**
//!
//! Core's router used to hold twelve `("POST", "/claude/hook/*")` arms, its
//! `note_chp` observer used to ask `claude_hook::is_hook_route(route)` before
//! deciding where a request's identity lived, and its drift ledger's key space
//! was a `const DRIFT_SHIMS: [&str; 10]` transcribed from one harness's shim
//! binaries. Three questions, one harness's answer hard-coded into each.
//!
//! They are the same three questions asked of every registered plugin here:
//!
//! * [`route`] — *does anybody serve this method+path?* Consulted **after**
//!   every CHP-neutral arm, so a plugin cannot shadow `/session/hello`.
//! * [`identity_of_request`] — *does this request carry its identity outside
//!   its body?* `None` from all of them means "read the CHP envelope", which is
//!   what core does for every ordinary caller.
//! * [`drift_tokens`] — *what may a payload-drift row be keyed by?* The union
//!   over the registry, so the ledger's key space is still `&'static str` and
//!   still bounded, but by declaration rather than by a core constant.
//!
//! And one derived number:
//!
//! * [`hook_reply_budget`] — how long core may spend on work an out-of-process
//!   caller is *waiting* for, computed as `min(declared timeouts) − margin`
//!   rather than hand-computed from two artifacts' timers.
//!
//! Same shape and the same fail-closed direction as [`super::native`]: a source
//! nobody registered gets nothing, never another harness's answer.

use std::time::Duration;

use super::plugin::{RequestIdentity, Route};
use super::registry;

/// The plugin route serving `method` + `path`, if any.
///
/// Registry order decides ties, and a tie is a bug in the two plugins rather
/// than in this lookup: [`tests::no_two_plugins_claim_one_route`] refuses to let
/// two harnesses declare the same method+path, because "whichever registered
/// first wins" is not an answer anybody could reason about at a wire boundary.
pub fn route(method: &str, path: &str) -> Option<&'static Route> {
    registry::all()
        .filter_map(|h| h.plugin())
        .flat_map(|p| p.routes())
        .find(|r| r.method == method && r.path == path)
}

/// The identity `req` carries outside its body, from whichever plugin claims
/// the route — or `None`, meaning core should read the CHP envelope.
pub fn identity_of_request(
    route: &str,
    req: &crate::offload::loopback::Request,
) -> Option<RequestIdentity> {
    registry::all()
        .filter_map(|h| h.plugin())
        .find_map(|p| p.identity_of_request(route, req))
}

/// Every payload-drift ledger token any registered harness may report under.
///
/// `&'static str` throughout, which is the bound itself rather than a check
/// that implements it: a caller-supplied string cannot become a ledger key, so
/// the key space is `sum(declared) + 1` (the sentinel) by construction.
pub fn drift_tokens() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = registry::all()
        .filter_map(|h| h.plugin())
        .flat_map(|p| p.drift_vocabulary())
        .copied()
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// **The harness an identity-less body on `route` came from** (locked decision
/// 22).
///
/// [`super::DEFAULT_HARNESS`] unless some plugin claims the route through
/// [`super::HarnessPlugin::legacy_wire_default_routes`]. This replaced thirteen
/// `unwrap_or("claude")` literals and two `unwrap_or("opencode")` ones spread
/// across the loopback handlers, each carrying (at best) its own copy of the
/// rationale.
pub fn wire_default(route: &str) -> super::HarnessId {
    registry::all()
        .find(|h| {
            h.plugin()
                .is_some_and(|p| p.legacy_wire_default_routes().contains(&route))
        })
        .unwrap_or(super::DEFAULT_HARNESS)
}

/// The `missing` entry a **quiet** capability's drift row carries.
///
/// Parenthesized like every other cImp-authored sentinel so it cannot be
/// confused with a field name off the wire. Core's, not a harness's: what it
/// says is that no push arrived, which is a statement about the transport
/// rather than about anybody's payload. Spelled exactly as the constant it
/// replaced (`claude::hook::MISSING_PUSH`) so existing rows keep their text.
pub const MISSING_PUSH: &str = "(no push — the hook stopped firing)";

/// The margin core keeps under the shortest declared reply timeout.
///
/// ~200 ms is what the hand-computed 1800 ms left, and the reason is unchanged:
/// the reply still has to be written, read across a socket and acted on before
/// the caller's own timer fires. Kept as a named constant so the derivation
/// reads as `min − margin` rather than as a number.
pub const HOOK_REPLY_MARGIN: Duration = Duration::from_millis(200);

/// The fallback budget when **no** registered harness declares a reply timeout.
///
/// Not reachable with either shipped plugin; it exists so the derivation has a
/// total answer rather than an `unwrap`, and it is deliberately the smallest of
/// the two shipped values minus the margin — a harness that declares nothing
/// gets the most conservative budget, not the most generous.
const NO_DECLARATION_BUDGET: Duration = Duration::from_millis(1800);

/// **How long core may hold a caller that is waiting for its reply**, derived
/// from what the harnesses declare (locked decision 22).
///
/// `min(every declared `hook_reply_timeout`) − `[`HOOK_REPLY_MARGIN`]. The
/// ordering is the whole point: the harness starts the tool the instant its hook
/// stops waiting, so if the caller's timer fired first the app would still be
/// staging *into* the tool call while believing it had a valid pre-tool
/// checkpoint. Keeping the app's budget under the *shortest* caller's makes the
/// app's own answer the one that decides, for every harness at once.
///
/// This replaced a `const TOOL_CHECKPOINT_BUDGET: Duration =
/// Duration::from_millis(1800)` hand-computed from two artifacts' timers and
/// asserted against them by a cross-file test. Same number today — pinned by
/// [`tests::the_derived_budget_is_the_1800_ms_the_shipped_plugins_imply`] — but
/// a third harness with a shorter timer now lowers it by being registered.
pub fn hook_reply_budget() -> Duration {
    registry::all()
        .filter_map(|h| h.plugin())
        .filter_map(|p| p.hook_reply_timeout())
        .min()
        .map(|t| t.saturating_sub(HOOK_REPLY_MARGIN))
        .unwrap_or(NO_DECLARATION_BUDGET)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// **1800 ms, and where it comes from.**
    ///
    /// Claude's deleted `--checkpoint-beacon` shim read its reply with a 2 s
    /// deadline, and that ceiling is what its `type: "http"` successor still
    /// expresses; OpenCode's generated plugin posts its checkpoint beacon under
    /// `AbortSignal.timeout(2000)`. `min(2000, 2000) − 200` is the number the
    /// old constant was hand-computed to be, so this phase changes the
    /// derivation and not the behaviour.
    #[test]
    fn the_derived_budget_is_the_1800_ms_the_shipped_plugins_imply() {
        assert_eq!(
            hook_reply_budget(),
            Duration::from_millis(1800),
            "the pre-tool budget changed. If a plugin's declared reply timeout moved, that is \
             the fix landing correctly — update this number and re-read the ordering argument on \
             `hook_reply_budget`. If nothing moved, a plugin stopped declaring one."
        );
    }

    /// Every declared timeout is strictly larger than the budget core takes,
    /// which is the ordering the whole mechanism rests on.
    #[test]
    fn every_declared_timeout_outlasts_the_budget() {
        let budget = hook_reply_budget();
        for h in registry::all() {
            let Some(p) = h.plugin() else { continue };
            let Some(t) = p.hook_reply_timeout() else {
                continue;
            };
            assert!(
                t > budget,
                "{h}: declares a {t:?} reply timeout, which is not longer than core's {budget:?} \
                 budget — the caller would abandon the reply while the app was still working, \
                 and the app would believe it had answered in time"
            );
        }
    }

    /// Two harnesses claiming one path would make the answer depend on registry
    /// order, which is not something a wire boundary may depend on.
    #[test]
    fn no_two_plugins_claim_one_route() {
        let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
        for h in registry::all() {
            let Some(p) = h.plugin() else { continue };
            for r in p.routes() {
                assert!(
                    seen.insert((r.method, r.path)),
                    "{h}: two registered plugins both serve `{} {}`",
                    r.method,
                    r.path
                );
            }
        }
    }

    /// A plugin route may not shadow a route core serves itself — core's
    /// `match` wins, so the plugin's handler would simply never run and the
    /// harness would look like it had declared something that does nothing.
    #[test]
    fn no_plugin_route_shadows_a_core_route() {
        let core = crate::offload::loopback::core_route_paths();
        assert!(
            core.len() >= 10,
            "the core route scan collapsed to {} paths — it would pass by finding nothing",
            core.len()
        );
        for h in registry::all() {
            let Some(p) = h.plugin() else { continue };
            for r in p.routes() {
                assert!(
                    !core.contains(r.path),
                    "{h}: declares `{}`, which core already serves — core's `match` wins, so \
                     this handler would never run",
                    r.path
                );
            }
        }
    }

    /// **Every inverted wire default names a route core actually serves**, and
    /// resolves to the harness that claimed it.
    ///
    /// The failure this guards is silent in both directions: rename the route
    /// and `wire_default` falls back to `DEFAULT_HARNESS`, so an identity-less
    /// OpenCode body would be attributed to Claude and a `/latch/state` answer
    /// would gate the wrong tab's native tools; claim a route nobody serves and
    /// the declaration is decoration. Neither shows up as an error anywhere.
    #[test]
    fn every_inverted_wire_default_names_a_route_that_exists() {
        let core = crate::offload::loopback::core_route_paths();
        let mut claimed = 0usize;
        for h in registry::all() {
            let Some(p) = h.plugin() else { continue };
            for r in p.legacy_wire_default_routes() {
                assert!(
                    core.contains(r) || route("POST", r).is_some(),
                    "{h}: claims `{r}` as its identity-less default, and nothing serves it"
                );
                assert_eq!(
                    wire_default(r),
                    h,
                    "`{r}` resolves to a harness other than the one that claimed it"
                );
                claimed += 1;
            }
        }
        assert!(
            claimed >= 2,
            "the two inverted defaults (`/memory/event`, `/latch/state`) stopped being declared              — they would silently read as the DEFAULT harness, which is what locked decision 22              calls the load-bearing asymmetry"
        );
        // …and everything else takes the documented compatibility default.
        for neutral in ["/context/retrieve", "/latch/beacon", "/mcp/call"] {
            assert_eq!(wire_default(neutral), super::super::DEFAULT_HARNESS);
        }
    }

    /// The drift key space is bounded and non-empty, and nothing in it is
    /// caller-supplied.
    #[test]
    fn the_drift_vocabulary_is_declared_and_deduplicated() {
        let tokens = drift_tokens();
        assert!(
            !tokens.is_empty(),
            "no registered harness declares a drift vocabulary — every payload-drift report \
             would fall into the one sentinel bucket and stop naming a capability"
        );
        let unique: BTreeSet<&str> = tokens.iter().copied().collect();
        assert_eq!(unique.len(), tokens.len(), "drift_tokens returned duplicates");
        for t in &tokens {
            assert!(!t.trim().is_empty(), "an empty drift token would key the ledger on nothing");
        }
    }

    /// **The drift tokens are PERSISTED ledger keys, pinned from outside the
    /// file that declares them** (V40 review finding W-1, parity lens).
    ///
    /// The declaring module's own test scans its own source for each token's
    /// literal — which the `pub const` definition satisfies, so the check has
    /// held nothing since it was written: renaming `"compact_hook"` would pass
    /// it. These strings key `drift.payload.v1` rows and the notices a user has
    /// DISMISSED, so a rename orphans the stored records and resurrects every
    /// dismissal at once. Written out here, in a different file, so a rename is
    /// a decision somebody takes rather than one that happens.
    ///
    /// Adding a harness ADDS tokens and that is ordinary; this asserts the
    /// existing spellings are still spelled.
    #[test]
    fn the_persisted_drift_token_spellings_are_pinned() {
        let tokens: BTreeSet<&str> = drift_tokens().into_iter().collect();
        for expected in [
            "checkpoint_beacon",
            "compact_hook",
            "context_hook",
            "notify_hook",
            "post_edit_hook",
            "read_hook",
            "stop_hook",
            "subagent_hook",
            "taint_beacon",
            "tool_result_hook",
        ] {
            assert!(
                tokens.contains(expected),
                "`{expected}` is a persisted drift-ledger key and a dismissal signature; it is                  no longer declared by any harness, so every row and every dismissal stored                  under it is orphaned"
            );
        }
    }
}
