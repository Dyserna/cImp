//! V21 F6 — native `run_check` for the offload worker. Lets the worker *prove*
//! claims about the code ("this compiles", "these tests fail", "the lint is
//! clean") by running one of the project's **configured** check commands and
//! getting back deduplicated, structured diagnostics — a tool observation
//! Feature 4 can then count as evidence, rather than plausible text.
//!
//! No new execution surface: this dispatches to the SAME checks entry point the
//! V12 `run_check` MCP tool uses ([`crate::graph::offload_run_check`] →
//! `graph::run_check_tool`), so `CheckDef` resolution, the parser/dedup
//! machinery, the bounded report, its own timeout, and the activity-ring
//! recording are all shared. The command itself is fixed by the user's project
//! config and is never model-supplied — a `run_check` call only *selects* a
//! configured check by name.
//!
//! Gate: like the MCP surface (`graph/mcp.rs`), this is advertised only when the
//! top-level `checks` setting is non-empty. A fresh project with no `checks`
//! configured sees no `run_check` on either surface, by design (the
//! discoverability fix — a checks editor + auto-detect — is milestone V22).

use crate::offload::openai::ToolDef;

use super::ToolCtx;

/// The `run_check` tool descriptor for the worker. Reuses the shared
/// [`crate::graph::run_check_spec`] name + schema (so the worker's surface can't
/// drift from the MCP one), with a worker-tailored description steering the
/// model to verify build/test/lint claims *before* stating them.
pub fn def() -> ToolDef {
    let spec = crate::graph::run_check_spec();
    ToolDef::function(
        spec.name,
        "Run one of this project's configured checker commands (build / typecheck / lint / test) \
         and get back DEDUPLICATED, STRUCTURED diagnostics. Use this to VERIFY a build/test/lint \
         claim with a real observation before you state it — never assert \"this compiles\", \"the \
         tests pass\", or \"the lint is clean\" without running the relevant check and reporting \
         what it returned. `name` selects among the project's configured checks (omit it when only \
         one is configured; an unknown or omitted-with-multiple name returns the list of configured \
         names). The command is fixed by the user's project config — never model-supplied. \
         `changed_only: true` filters diagnostics to files touched since HEAD. If the check times \
         out, the result says so — report that as unverified, do not guess the outcome.",
        spec.parameters,
    )
}

/// Execute a worker `run_check` call: resolve the project root from the
/// confinement roots and run the configured check via the shared entry point.
pub async fn execute(args: serde_json::Value, ctx: &ToolCtx) -> Result<String, String> {
    crate::graph::offload_run_check(&ctx.allowed_roots, &args).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::OffloadToolToggles;

    #[test]
    fn def_reuses_the_shared_spec_name_and_schema() {
        let spec = crate::graph::run_check_spec();
        let d = def();
        assert_eq!(d.function.name, spec.name);
        assert_eq!(d.function.parameters, spec.parameters);
        // The worker description steers toward verification-before-assertion.
        assert!(
            d.function.description.contains("VERIFY"),
            "{}",
            d.function.description
        );
    }

    #[test]
    fn advertised_only_when_toggled_on_and_checks_configured() {
        // The toggle defaults on; the checks-configured gate is what actually
        // decides advertisement (tested via the pure `enabled_defs_inner`).
        let mut toggles = OffloadToolToggles::default();
        assert!(toggles.run_check, "run_check toggle defaults on");

        // Toggle on + checks configured ⇒ advertised.
        assert!(
            super::super::enabled_defs_inner(&toggles, true)
                .iter()
                .any(|d| d.function.name == "run_check"),
            "run_check must be advertised when its toggle is on and checks are configured"
        );
        // Checks configured but toggle off ⇒ not advertised.
        toggles.run_check = false;
        assert!(
            !super::super::enabled_defs_inner(&toggles, true)
                .iter()
                .any(|d| d.function.name == "run_check"),
            "run_check must not be advertised when its toggle is off"
        );
        // Toggle on but no checks configured ⇒ not advertised (the gate).
        toggles.run_check = true;
        assert!(
            !super::super::enabled_defs_inner(&toggles, false)
                .iter()
                .any(|d| d.function.name == "run_check"),
            "run_check must not be advertised when no checks are configured"
        );
    }

    #[tokio::test]
    async fn dispatch_routes_to_run_check() {
        // In the test environment the top-level `checks` are empty, so the shared
        // entry point returns the "not configured" guidance — but reaching that
        // (rather than "unknown native tool") proves `dispatch` routes the name.
        let root = std::env::temp_dir();
        let ctx = ToolCtx::new(vec![root.clone()], vec![], vec![], &root);
        let out = super::super::dispatch("run_check", serde_json::json!({}), &ctx)
            .await
            .expect("run_check dispatch should not error");
        assert!(
            !out.contains("unknown native tool"),
            "dispatch did not route run_check: {out}"
        );
        assert!(
            out.contains("not configured"),
            "expected the not-configured guidance: {out}"
        );
    }
}
