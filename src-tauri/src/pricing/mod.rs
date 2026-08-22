//! **Provider price knowledge** — the seeded `$/MTok` table and its top-up
//! watermark.
//!
//! V40 locked decision 29 ruled this seam explicitly, against the obvious
//! reading. The rows are `claude-*` model prefixes and the cache-write column
//! is pinned to the 1-hour-TTL rate *Claude Code sessions* use, so it looks
//! like harness knowledge — and it is not. It is **provider** knowledge:
//! `anthropic/claude-opus-4-8` shows up in an OpenCode session's model id just
//! as readily as in a Claude Code transcript, the Copilot rows belong to no
//! harness cImp ships, and a build with both harnesses deleted would still need
//! this table to price a session it read off disk.
//!
//! So it does **not** live behind `HarnessPlugin` (a plugin that owned it would
//! have to be asked "what does Anthropic charge" by a code path that has no
//! harness in hand), and it does not live in `settings/schema.rs` either, where
//! it was the largest single block of vendor data inside the persisted-shape
//! module. It lives here, on its own, with the two functions that are its whole
//! interface:
//!
//! * [`default_llm_pricing`] — what a **fresh** install is seeded with.
//! * [`pricing_rows_since`] — what an **existing** install is topped up with,
//!   keyed by [`PRICING_GENERATION`]. The two are cross-checked by
//!   `settings::schema`'s F-19 tripwire, which is what keeps a new model from
//!   reaching fresh installs only.
//!
//! The persisted row type itself ([`crate::settings::LlmPricingModel`]) stays in
//! `settings/schema.rs`: it is an on-disk shape the user edits in Settings, not
//! vendor data.

use crate::settings::LlmPricingModel;

/// Fresh-install price seeds (as of 2026-07): Anthropic API list prices
/// and GitHub Copilot's per-token prices from its June 2026 usage-based
/// billing (the Sonnet 5 row is the promo rate that runs through
/// 2026-08-31). V16 decision (2026-07-12): the Anthropic rows seed cache
/// write at the **1-hour-TTL 2x-input rate** — Claude Code sessions use the
/// 1h cache, so the 5-minute tier's 1.25x would undersell what those
/// sessions actually pay (cache read stays 0.1x input). Copilot's
/// OpenAI/Google rows had no published cache-write premium, so those seed
/// cache_write = input; all values are plain editable rows, not constants
/// the app depends on. Anthropic rows carry a `model_prefix` so the Usage
/// view's cost mode can auto-match transcript model ids (longest prefix
/// wins); Copilot rows are manual-pick only (Copilot sessions never appear
/// in the Claude transcript tap).
pub fn default_llm_pricing() -> Vec<LlmPricingModel> {
    vec![
        row(
            "Anthropic",
            "Claude Fable 5",
            "claude-fable-5",
            [10.0, 20.0, 1.0, 50.0],
        ),
        // F-19 (2026-08-10): absent until rc.2, which is why a session on the
        // default model priced at $0. Same list rates as the Opus 4.x rows.
        row(
            "Anthropic",
            "Claude Opus 5",
            "claude-opus-5",
            [5.0, 10.0, 0.5, 25.0],
        ),
        row(
            "Anthropic",
            "Claude Opus 4.8",
            "claude-opus-4-8",
            [5.0, 10.0, 0.5, 25.0],
        ),
        row(
            "Anthropic",
            "Claude Opus 4.7",
            "claude-opus-4-7",
            [5.0, 10.0, 0.5, 25.0],
        ),
        row(
            "Anthropic",
            "Claude Opus 4.6",
            "claude-opus-4-6",
            [5.0, 10.0, 0.5, 25.0],
        ),
        row(
            "Anthropic",
            "Claude Sonnet 5",
            "claude-sonnet-5",
            [3.0, 6.0, 0.3, 15.0],
        ),
        row(
            "Anthropic",
            "Claude Sonnet 4.6",
            "claude-sonnet-4-6",
            [3.0, 6.0, 0.3, 15.0],
        ),
        row(
            "Anthropic",
            "Claude Haiku 4.5",
            "claude-haiku-4-5",
            [1.0, 2.0, 0.1, 5.0],
        ),
        row(
            "Copilot",
            "Claude Sonnet 5 (promo)",
            "",
            [2.0, 2.5, 0.2, 10.0],
        ),
        row("Copilot", "Claude Sonnet 4.6", "", [3.0, 3.75, 0.3, 15.0]),
        row("Copilot", "Claude Opus 4.8", "", [5.0, 6.25, 0.5, 25.0]),
        row("Copilot", "Claude Haiku 4.5", "", [1.0, 1.25, 0.1, 5.0]),
        row("Copilot", "GPT-5 mini", "", [0.25, 0.25, 0.025, 2.0]),
        row("Copilot", "GPT-5.4", "", [2.5, 2.5, 0.25, 15.0]),
        row("Copilot", "GPT-5.5", "", [5.0, 5.0, 0.5, 30.0]),
        row("Copilot", "Gemini 2.5 Pro", "", [1.25, 1.25, 0.31, 10.0]),
        row("Copilot", "Gemini 3.5 Flash", "", [1.5, 1.5, 0.38, 9.0]),
    ]
}

fn row(provider: &str, model: &str, prefix: &str, prices: [f64; 4]) -> LlmPricingModel {
    LlmPricingModel {
        provider: provider.to_string(),
        model: model.to_string(),
        model_prefix: prefix.to_string(),
        input: prices[0],
        cache_write: prices[1],
        cache_read: prices[2],
        output: prices[3],
    }
}

/// Which batch of built-in price rows this build knows about. **Bump this by
/// one, and extend [`pricing_rows_since`], every time a model is added to
/// [`default_llm_pricing`]** — otherwise the new row reaches fresh installs
/// only and every existing install silently prices that model at $0 (F-19).
///
/// Generation 0 is the pre-2026-08-10 table (no `claude-opus-5`); generation 1
/// adds it.
pub const PRICING_GENERATION: u32 = 1;

/// The built-in rows introduced *after* generation `since` — the top-up set
/// for an install whose stored table predates this build.
///
/// Deliberately NOT "every built-in row the stored table is missing". The
/// price table is user-owned: a row the user deleted must stay deleted, and
/// only the watermark can tell "deleted" apart from "never shipped". Callers
/// additionally skip any row whose `model_prefix` the stored table already
/// carries, so a hand-added row is topped up to a no-op rather than a
/// duplicate — which is exactly the state the user who reported F-19 is in.
pub fn pricing_rows_since(since: u32) -> Vec<LlmPricingModel> {
    let mut out = Vec::new();
    if since < 1 {
        out.push(row(
            "Anthropic",
            "Claude Opus 5",
            "claude-opus-5",
            [5.0, 10.0, 0.5, 25.0],
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **F-19 tripwire. If you added a model to [`default_llm_pricing`] and
    /// landed here, that is this test working:** bump [`PRICING_GENERATION`],
    /// add the row to [`pricing_rows_since`] under the new generation, and add
    /// its prefix to `GEN_0` below only if it shipped before 2026-08-10.
    ///
    /// A row added to `default_llm_pricing` alone reaches **fresh installs
    /// only** — every existing install keeps its stored table and prices that
    /// model at $0, with no error anywhere. That is exactly how the missing
    /// `claude-opus-5` row survived into a release candidate, and nothing about
    /// adding the next model makes it more noticeable.
    #[test]
    fn every_built_in_priced_model_is_reachable_by_existing_installs() {
        /// Prefixed rows that shipped before the watermark existed. Installs
        /// that predate F-19 already have these, so they are NOT in
        /// `pricing_rows_since` — adding one here retroactively would mean
        /// resurrecting it for users who deleted it.
        const GEN_0: &[&str] = &[
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        ];

        let shipped: Vec<String> = default_llm_pricing()
            .into_iter()
            .map(|r| r.model_prefix)
            .filter(|p| !p.is_empty())
            .collect();

        // Every prefix reaches an existing install via exactly one route.
        let migrated: Vec<String> = pricing_rows_since(0)
            .into_iter()
            .map(|r| r.model_prefix)
            .collect();
        for prefix in &shipped {
            let in_gen_0 = GEN_0.contains(&prefix.as_str());
            let in_migration = migrated.iter().any(|p| p == prefix);
            assert!(
                in_gen_0 || in_migration,
                "`{prefix}` is seeded into fresh installs by default_llm_pricing but is \
                 neither a pre-watermark row nor returned by pricing_rows_since(0) — every \
                 EXISTING install will price it at $0. Bump PRICING_GENERATION and add it \
                 to pricing_rows_since."
            );
            assert!(
                !(in_gen_0 && in_migration),
                "`{prefix}` is both a pre-watermark row and a migration row; the migration \
                 would re-add a row those installs may have deleted"
            );
        }

        // …and the migration never offers a row fresh installs don't get, or
        // the two populations end up with different tables.
        for prefix in &migrated {
            assert!(
                shipped.contains(prefix),
                "pricing_rows_since offers `{prefix}` to existing installs but \
                 default_llm_pricing doesn't give it to fresh ones"
            );
        }

        // Prices must agree between the two routes, for the same reason.
        for row in pricing_rows_since(0) {
            let shipped_row = default_llm_pricing()
                .into_iter()
                .find(|r| r.model_prefix == row.model_prefix)
                .expect("checked above");
            assert_eq!(
                (
                    shipped_row.input,
                    shipped_row.cache_write,
                    shipped_row.cache_read,
                    shipped_row.output
                ),
                (row.input, row.cache_write, row.cache_read, row.output),
                "`{}` is priced differently for fresh vs migrated installs",
                row.model_prefix
            );
        }
    }

    /// The default model has to be priced, or the Usage view's cost mode reads
    /// $0 for the sessions the user actually runs — the F-19 symptom.
    #[test]
    fn the_current_default_model_has_a_price_row() {
        let priced = default_llm_pricing()
            .into_iter()
            .any(|r| r.model_prefix == "claude-opus-5");
        assert!(priced, "no claude-opus-5 row in the seeded price table");
    }
}
