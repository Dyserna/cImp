//! V8-02 offload router — picks **one** backend per `offload_task`.
//!
//! Routing is per-task, never per-step: the whole agent loop runs against
//! a single backend (conversation state lives on one server's slot, so
//! there is no mid-loop migration). The selection cascade, in order:
//!
//! 1. **Readiness + consent** — only backends that are healthy *and*, for
//!    cloud, consented-to are candidates.
//! 2. **Tool-need (hard filter)** — the task's required tools must be a
//!    subset of the backend's allowed tools. A "read these local files"
//!    task is therefore ineligible for a cloud backend whose scope denies
//!    `read_file` — the privacy boundary, enforced at routing *and* (in
//!    [`super::agent`]) in the `tools` array.
//! 3. **Required context** — a backend whose per-slot budget can't hold the
//!    estimated task input is filtered out (an 8 GB/16k box can't take a
//!    100k ingest). If *nothing* fits, the router degrades to the
//!    most-capable tool-eligible backend rather than failing.
//! 4. **Tier / availability** — among the survivors, prefer a backend with
//!    a **free slot** (spill instead of queuing), then one matching the
//!    desired tier (`fast`/`quality`, biased by Claude's `tier` hint or
//!    inferred from task size), then the largest budget. A down backend was
//!    already filtered in step 1, so this also gives fail-over for free.
//!
//! Selection is a pure function over [`BackendView`] snapshots so it is
//! trivially testable and shared by the MCP child (the real path) and the
//! Settings "Test offload" button. It returns the chosen *index* into the
//! caller's slice, which the caller maps back to its concrete backend.

use crate::settings::{BackendTier, ToolScope, LOCAL_DATA_TOOLS, WEB_DOCS_TOOLS};

/// Below this estimated input size, an `auto`-tier task is considered small
/// enough to prefer a fast backend (when one is eligible). Larger tasks
/// default to the quality tier.
const AUTO_FAST_MAX_CONTEXT: u32 = 6_000;

/// A snapshot of one backend's routing-relevant state. Built by the caller
/// from a live [`Backend`](super::Backend) + its config; the router never
/// touches the network.
#[derive(Clone, Debug)]
pub struct BackendView {
    pub name: String,
    /// Healthy per the last probe.
    pub ready: bool,
    /// Cloud backend whose consent toggle is off — never eligible.
    pub cloud_blocked: bool,
    /// Discovered or declared context window (`None` = unknown).
    pub n_ctx: Option<u32>,
    pub slots: u32,
    pub in_flight: u32,
    pub tier: BackendTier,
    pub tool_scope: ToolScope,
    /// Fraction of the per-slot window the loop works against (for the
    /// budget estimate).
    pub budget_high_water_pct: u8,
}

impl BackendView {
    /// Per-slot working budget `(n_ctx / slots) * high_water/100`, or
    /// `None` when `n_ctx` is unknown.
    pub fn per_slot_budget(&self) -> Option<u32> {
        let n = self.n_ctx?;
        let per_slot = n / self.slots.max(1);
        Some(per_slot.saturating_mul(self.budget_high_water_pct.min(100) as u32) / 100)
    }

    /// A slot is free right now (spill target).
    pub fn has_free_slot(&self) -> bool {
        self.in_flight < self.slots
    }

    /// Every required tool is allowed by this backend's scope. An empty
    /// requirement set is trivially satisfied.
    pub fn allows_all(&self, required: &[String]) -> bool {
        required
            .iter()
            .all(|t| self.tool_scope.allows_namespaced(t))
    }

    /// Whether the estimated input fits the per-slot budget. Unknown budget
    /// errs toward capability (`true`) — the agent loop still self-polices
    /// and degrades on real overflow.
    pub fn fits_context(&self, estimated: u32) -> bool {
        match self.per_slot_budget() {
            Some(b) => b >= estimated,
            None => true,
        }
    }
}

/// Claude's `tier` bias on `offload_task`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TierHint {
    /// Let the router infer from task size.
    Auto,
    /// Bias toward the fast/small backend.
    Fast,
    /// Bias toward the quality/large backend.
    Quality,
}

impl TierHint {
    pub fn parse(s: &str) -> Self {
        match s {
            "fast" => TierHint::Fast,
            "quality" => TierHint::Quality,
            _ => TierHint::Auto,
        }
    }
}

/// What the router needs to know about a task. Built heuristically by
/// [`analyze_task`] from the instruction text, or constructed directly in
/// tests.
#[derive(Clone, Debug)]
pub struct RouteRequest {
    /// Tools the task is estimated to need (hard filter; native names or
    /// MCP-server names).
    pub required_tools: Vec<String>,
    /// Estimated input size in tokens.
    pub estimated_context: u32,
    /// Claude's tier bias.
    pub tier_hint: TierHint,
}

/// Why no backend could be chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    /// No backend is enabled + healthy + (for cloud) consented.
    NoBackendReady,
    /// Backends are ready, but none allows the task's required tools — e.g.
    /// a local-file task with only a cloud backend (which denies file
    /// tools) available. The caller surfaces this to Claude as a clear
    /// "can't run this here" result, never silently sending local data to
    /// cloud.
    NoToolMatch,
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NoBackendReady => {
                write!(f, "no offload backend is ready (enable/start one, or grant cloud consent)")
            }
            RouteError::NoToolMatch => write!(
                f,
                "this task needs local-data tools, but no ready backend allows them \
                 (a cloud backend cannot read your local files) — enable a local or LAN backend"
            ),
        }
    }
}

/// Infer the desired tier for an `auto` hint from task size. Small inputs →
/// fast; larger → quality. Explicit hints override this.
fn desired_tier(req: &RouteRequest) -> BackendTier {
    match req.tier_hint {
        TierHint::Fast => BackendTier::Fast,
        TierHint::Quality => BackendTier::Quality,
        TierHint::Auto => {
            if req.estimated_context <= AUTO_FAST_MAX_CONTEXT {
                BackendTier::Fast
            } else {
                BackendTier::Quality
            }
        }
    }
}

/// Select one backend for `req`. Returns the index into `backends`, or a
/// [`RouteError`] when nothing is eligible. Pure — no I/O.
pub fn select(backends: &[BackendView], req: &RouteRequest) -> Result<usize, RouteError> {
    // Step 1: readiness + consent.
    let base: Vec<usize> = (0..backends.len())
        .filter(|&i| backends[i].ready && !backends[i].cloud_blocked)
        .collect();
    if base.is_empty() {
        return Err(RouteError::NoBackendReady);
    }

    // Step 2: tool-need hard filter (the privacy boundary).
    let tool_ok: Vec<usize> = base
        .iter()
        .copied()
        .filter(|&i| backends[i].allows_all(&req.required_tools))
        .collect();
    if tool_ok.is_empty() {
        return Err(RouteError::NoToolMatch);
    }

    // Step 3: required-context filter. If nothing fits, degrade to the full
    // tool-eligible set (the ordering below then picks the largest budget).
    let ctx_ok: Vec<usize> = tool_ok
        .iter()
        .copied()
        .filter(|&i| backends[i].fits_context(req.estimated_context))
        .collect();
    let candidates = if ctx_ok.is_empty() { &tool_ok } else { &ctx_ok };

    // Step 4: tier / availability ordering. Higher tuple = better:
    //   (free_slot, tier_match, fits_ctx, budget)
    // free_slot first so a full preferred backend spills to a free eligible
    // one instead of queuing; tier_match next so among free backends the
    // desired tier wins; fits_ctx + budget break remaining ties toward the
    // most-capable choice.
    let want = desired_tier(req);
    let best = candidates
        .iter()
        .copied()
        .max_by_key(|&i| {
            let b = &backends[i];
            (
                b.has_free_slot() as u8,
                (b.tier == want) as u8,
                b.fits_context(req.estimated_context) as u8,
                b.per_slot_budget().unwrap_or(0),
            )
        })
        .expect("candidates is non-empty");
    Ok(best)
}

/// Keyword signals that a task touches the user's local machine (files,
/// code, repo, commands) — its results must not leave the box, so the
/// router requires a local-data tool (which excludes cloud backends).
const LOCAL_SIGNALS: &[&str] = &[
    "file", "read ", "open ", "path", "directory", "folder", "code",
    "grep", "function", "class", "module", "repo", "repository", "log file",
    "build", "compile", "unit test", "command", "run ", "git ", "diff", "blame",
    "callsite", "call site", "implementation", "codebase",
];

/// Keyword signals that a task is purely web/docs research — safe for a
/// cloud backend if no local signal is also present.
const WEB_SIGNALS: &[&str] = &[
    "web", "online", "internet", "url", "http", "fetch", "website",
    "documentation", "docs for", "latest version", "release notes",
    "stack overflow", "google", "duckduckgo",
];

/// Heuristically build a [`RouteRequest`] from the task text. Estimation is
/// approximate (the real ingest size isn't known until tools read), so this
/// errs toward capability/privacy: an ambiguous task is assumed to need
/// local-data tools, which keeps it off cloud backends and on a capable
/// local/LAN one.
pub fn analyze_task(instructions: &str, context: Option<&str>, tier: TierHint) -> RouteRequest {
    let ctx_len = context.map(|c| c.len()).unwrap_or(0);
    let estimated_context = (((instructions.len() + ctx_len) / 4) as u32).max(256);

    let hay = format!(
        "{} {}",
        instructions.to_lowercase(),
        context.unwrap_or("").to_lowercase()
    );
    let local = LOCAL_SIGNALS.iter().any(|s| hay.contains(s));
    let web = WEB_SIGNALS.iter().any(|s| hay.contains(s));

    let mut required: Vec<String> = Vec::new();
    // Default-to-local for privacy: require a local-data tool unless the
    // task is *clearly* web-only (a web signal with no local signal). This
    // is what stops an ambiguous task from silently going to cloud.
    let needs_local = local || !web;
    if needs_local {
        required.push("read_file".to_string());
        if hay.contains("command") || hay.contains("build") || hay.contains("unit test")
            || hay.contains("run ") || hay.contains("compile")
        {
            required.push("run_command".to_string());
        }
        if hay.contains("git ") || hay.contains("commit") || hay.contains("blame")
            || hay.contains("diff")
        {
            required.push("git".to_string());
        }
    }
    debug_assert!(
        required.iter().all(|t| LOCAL_DATA_TOOLS.contains(&t.as_str()))
            || WEB_DOCS_TOOLS.is_empty(),
        "required tools should be local-data names"
    );

    RouteRequest {
        required_tools: required,
        estimated_context,
        tier_hint: tier,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(
        name: &str,
        ready: bool,
        tier: BackendTier,
        n_ctx: Option<u32>,
        slots: u32,
        in_flight: u32,
        scope: ToolScope,
    ) -> BackendView {
        BackendView {
            name: name.to_string(),
            ready,
            cloud_blocked: false,
            n_ctx,
            slots,
            in_flight,
            tier,
            tool_scope: scope,
            budget_high_water_pct: 80,
        }
    }

    fn req(tools: &[&str], ctx: u32, hint: TierHint) -> RouteRequest {
        RouteRequest {
            required_tools: tools.iter().map(|s| s.to_string()).collect(),
            estimated_context: ctx,
            tier_hint: hint,
        }
    }

    #[test]
    fn single_backend_is_a_noop() {
        let bs = vec![view("only", true, BackendTier::Quality, Some(100_000), 1, 0, ToolScope::All)];
        assert_eq!(select(&bs, &req(&[], 500, TierHint::Auto)).unwrap(), 0);
    }

    #[test]
    fn tiny_task_prefers_fast_backend() {
        let bs = vec![
            view("big", true, BackendTier::Quality, Some(150_000), 1, 0, ToolScope::All),
            view("fast", true, BackendTier::Fast, Some(16_000), 1, 0, ToolScope::All),
        ];
        // Auto + tiny input → fast tier.
        assert_eq!(select(&bs, &req(&[], 500, TierHint::Auto)).unwrap(), 1);
    }

    #[test]
    fn large_context_task_picks_the_big_backend() {
        let bs = vec![
            view("big", true, BackendTier::Quality, Some(150_000), 1, 0, ToolScope::All),
            view("fast", true, BackendTier::Fast, Some(16_000), 1, 0, ToolScope::All),
        ];
        // 100k estimate: the fast 16k box is filtered out by the context
        // budget, leaving only the big backend.
        let idx = select(&bs, &req(&[], 100_000, TierHint::Auto)).unwrap();
        assert_eq!(bs[idx].name, "big");
    }

    #[test]
    fn honors_explicit_tier_hints() {
        let bs = vec![
            view("big", true, BackendTier::Quality, Some(150_000), 1, 0, ToolScope::All),
            view("fast", true, BackendTier::Fast, Some(16_000), 1, 0, ToolScope::All),
        ];
        assert_eq!(bs[select(&bs, &req(&[], 500, TierHint::Quality)).unwrap()].name, "big");
        assert_eq!(bs[select(&bs, &req(&[], 500, TierHint::Fast)).unwrap()].name, "fast");
    }

    #[test]
    fn spills_to_second_eligible_when_preferred_is_full() {
        // Prefer quality, but the quality backend has no free slot → spill
        // to the fast one (which is eligible and free).
        let bs = vec![
            view("big", true, BackendTier::Quality, Some(150_000), 1, 1, ToolScope::All),
            view("fast", true, BackendTier::Fast, Some(16_000), 1, 0, ToolScope::All),
        ];
        let idx = select(&bs, &req(&[], 500, TierHint::Quality)).unwrap();
        assert_eq!(bs[idx].name, "fast");
    }

    #[test]
    fn fails_over_when_preferred_is_down() {
        let bs = vec![
            view("big", false, BackendTier::Quality, Some(150_000), 1, 0, ToolScope::All), // down
            view("fast", true, BackendTier::Fast, Some(16_000), 1, 0, ToolScope::All),
        ];
        // Quality requested but down → fail over to the ready fast backend.
        let idx = select(&bs, &req(&[], 500, TierHint::Quality)).unwrap();
        assert_eq!(bs[idx].name, "fast");
    }

    #[test]
    fn no_ready_backend_errors() {
        let bs = vec![view("x", false, BackendTier::Quality, Some(100_000), 1, 0, ToolScope::All)];
        assert_eq!(select(&bs, &req(&[], 500, TierHint::Auto)), Err(RouteError::NoBackendReady));
    }

    #[test]
    fn local_data_task_not_routed_to_cloud_only() {
        // The only ready backend is a cloud one that denies local-data
        // tools; a task requiring read_file gets NoToolMatch (never sent).
        let cloud = view(
            "cloud",
            true,
            BackendTier::Quality,
            Some(128_000),
            4,
            0,
            ToolScope::default_for(true),
        );
        let bs = vec![cloud];
        let r = select(&bs, &req(&["read_file"], 500, TierHint::Auto));
        assert_eq!(r, Err(RouteError::NoToolMatch));
    }

    #[test]
    fn cloud_eligible_for_web_only_task() {
        let cloud = view(
            "cloud",
            true,
            BackendTier::Quality,
            Some(128_000),
            4,
            0,
            ToolScope::default_for(true),
        );
        let bs = vec![cloud];
        // No required local tools → cloud is eligible.
        assert_eq!(select(&bs, &req(&[], 500, TierHint::Auto)).unwrap(), 0);
    }

    #[test]
    fn cloud_without_consent_is_skipped() {
        let mut cloud = view("cloud", true, BackendTier::Quality, Some(128_000), 4, 0, ToolScope::All);
        cloud.cloud_blocked = true;
        let bs = vec![cloud];
        assert_eq!(select(&bs, &req(&[], 500, TierHint::Auto)), Err(RouteError::NoBackendReady));
    }

    #[test]
    fn degrades_to_largest_when_nothing_fits() {
        // Both backends are too small for the estimate; pick the larger.
        let bs = vec![
            view("small", true, BackendTier::Fast, Some(8_000), 1, 0, ToolScope::All),
            view("medium", true, BackendTier::Quality, Some(20_000), 1, 0, ToolScope::All),
        ];
        let idx = select(&bs, &req(&[], 500_000, TierHint::Auto)).unwrap();
        assert_eq!(bs[idx].name, "medium");
    }

    #[test]
    fn analyze_flags_local_task() {
        let r = analyze_task("Find all call sites of foo in the codebase and summarize", None, TierHint::Auto);
        assert!(r.required_tools.contains(&"read_file".to_string()));
    }

    #[test]
    fn analyze_web_only_task_requires_no_local_tools() {
        let r = analyze_task("Search the web for the latest release notes of tokio", None, TierHint::Auto);
        assert!(r.required_tools.is_empty());
    }

    #[test]
    fn analyze_ambiguous_defaults_to_local() {
        // No clear web signal → treated as needing local data (privacy).
        let r = analyze_task("Summarize the following and list key points", None, TierHint::Auto);
        assert!(r.required_tools.contains(&"read_file".to_string()));
    }
}
