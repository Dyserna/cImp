//! #48, finding F-10 — the **per-backend admission gate** every offload tool
//! call passes, in one place both routers reach.
//!
//! # What went wrong without it
//!
//! Three checks decide whether a tool name may run for the backend a task was
//! routed to, and all three exist because *an unadvertised name can still be
//! called by the model* — the A-1 threat model, whose base rate this milestone
//! measured (28 `ok:false` rows in 162 live calls, `a434d4f`):
//!
//! 1. the backend's [`ToolScope`] (V8-02, the cloud-privacy allow-list),
//! 2. `graph_*` behind the code-graph opt-in (V9-01), and
//! 3. `security_audit`/`quality_audit` behind the audit opt-in (V26).
//!
//! They were written as three `if`s inside **one of the two** `ToolRouter`
//! implementations. `HostRouter::call` had all three; `NativeRouter::call` had
//! only the first — and the first does not cover the other two, because
//! [`ToolScope::allows`] keys on the segment before `__` and
//! `LOCAL_DATA_TOOLS` names `read_file, list_dir, code_search, run_command,
//! filesystem, git` and neither the audit tools nor any `graph_*`. So
//! `allows_namespaced("graph_snippet")` answered `true` on a cloud backend and
//! `NativeRouter` dispatched it: repo source text to a remote model, from a
//! name the model was never offered.
//!
//! That is why the fix is a shared *function* and not a copied `if`. The scope
//! set cannot express rules 2 and 3 (a per-tool entry for every `graph_*`, or
//! prefix matching it does not have), so the rules have to live somewhere; the
//! defect was that "somewhere" was a router body.
//!
//! # Why a [`GatePass`] and not a `Result<(), String>`
//!
//! A shared function still has to be *called*. [`super::tools::dispatch`] takes
//! its tool name from a `GatePass`, which only [`BackendGate::admit`] can mint —
//! so a native dispatch that skipped the gate does not compile, in the same
//! spirit as the audit envelope's `RawReport` (#48, finding M-6). The pass
//! carries the name it was granted for, so it cannot be minted for `read_file`
//! and spent on `graph_snippet`.

use crate::settings::{Settings, ToolScope};

/// Proof that [`BackendGate::admit`] granted this tool name, and the only way
/// to name a tool at [`super::tools::dispatch`].
///
/// Deliberately not `Copy` and not constructible outside this module: it is
/// consumed by the dispatcher it authorizes.
pub struct GatePass<'a> {
    name: &'a str,
}

impl<'a> GatePass<'a> {
    /// The admitted tool name. Consumes the pass — one grant, one dispatch.
    pub fn name(self) -> &'a str {
        self.name
    }

    /// Mint a pass without a gate. **Tests only**, and it is `#[cfg(test)]`
    /// rather than `pub(crate)` so that no production path can reach it: the
    /// whole point of the type is that production code cannot make one.
    #[cfg(test)]
    pub fn for_test(name: &'a str) -> GatePass<'a> {
        GatePass { name }
    }
}

/// Everything a chosen backend is allowed to reach, resolved once per run.
///
/// Held by both `ToolRouter` implementations in place of the loose
/// `scope`/`allow_graph`/`allow_audit` fields they used to carry, so the
/// decision cannot be half-implemented in one of them again.
pub struct BackendGate {
    /// V8-02: the backend's allow-list over the tool pool.
    scope: ToolScope,
    /// V9-01: whether the code-graph tools may run for this backend (feature on
    /// AND local-or-opted-in-remote). A remote backend the user did not opt in
    /// must never receive the project's code structure or source text.
    allow_graph: bool,
    /// V26: whether the code-audit tools may run for this backend (feature on
    /// AND `expose_offload` AND a local backend). The scan runs locally either
    /// way, but its report is repo paths plus scanner messages that quote the
    /// offending source — local data, like the graph.
    allow_audit: bool,
}

impl BackendGate {
    /// Build a gate from already-resolved verdicts. Prefer
    /// [`for_worker`](Self::for_worker), which resolves them from settings so
    /// the three worker entry points cannot disagree; this constructor exists
    /// for callers that already hold the verdicts (and for tests).
    pub fn new(scope: ToolScope, allow_graph: bool, allow_audit: bool) -> Self {
        Self {
            scope,
            allow_graph,
            allow_audit,
        }
    }

    /// The worker's policy, resolved from one settings snapshot.
    ///
    /// `is_remote` is *LAN or cloud* — the same bit `OffloadService`'s pool
    /// entries carry — because both are "off this machine" for the two data
    /// boundaries below.
    pub fn for_worker(scope: ToolScope, is_remote: bool, settings: &Settings) -> Self {
        Self::new(
            scope,
            super::service::worker_graph_allowed(
                settings.graph.enabled,
                is_remote,
                settings.graph.allow_remote_worker_access,
            ),
            settings.code_audit.enabled && settings.code_audit.expose_offload && !is_remote,
        )
    }

    /// Whether the graph tools may be **advertised** to this backend's model.
    ///
    /// Advertisement only. Never re-derive an admission decision from this —
    /// that re-creates F-10 exactly: call [`admit`](Self::admit).
    pub fn graph_allowed(&self) -> bool {
        self.allow_graph
    }

    /// Whether the audit tools may be **advertised**. See
    /// [`graph_allowed`](Self::graph_allowed) for the advertisement-only rule.
    pub fn audit_allowed(&self) -> bool {
        self.allow_audit
    }

    /// Whether `name` would be admitted — the predicate the advertised surface
    /// is filtered by, so a tool cannot be offered and then refused.
    pub fn admits(&self, name: &str) -> bool {
        self.admit(name).is_ok()
    }

    /// **The gate.** Every check a tool name must pass before it may execute
    /// for this backend, in the order the router applied them, returning the
    /// proof [`super::tools::dispatch`] requires.
    ///
    /// `Err` is the message the loop feeds back to the model as a tool result.
    ///
    /// The `graph_` test is a bare `starts_with`, as `HostRouter` has always
    /// done it. A proxied id from a server literally named `graph` (`graph__x`)
    /// therefore also matches — which only ever *denies*, so it is left as-is
    /// rather than loosened.
    pub fn admit<'a>(&self, name: &'a str) -> Result<GatePass<'a>, String> {
        // 1. V8-02 — the backend's allow-list. Refuse even a hallucinated call
        //    (e.g. a cloud backend asking for `read_file`): the local file is
        //    never read for an out-of-scope backend.
        if !self.scope.allows_namespaced(name) {
            return Err(format!(
                "tool `{name}` is not available on this backend (denied by its tool scope)"
            ));
        }
        // 2. V9-01 — the code-graph opt-in.
        if name.starts_with("graph_") && !self.allow_graph {
            return Err(format!(
                "tool `{name}` is not available on this backend (code-graph access for a remote \
                 offload worker is off — enable it in cImp Settings → Code Graph)"
            ));
        }
        // 3. V26 — the code-audit opt-in.
        if matches!(name, "security_audit" | "quality_audit") && !self.allow_audit {
            return Err(format!(
                "tool `{name}` is not available on this backend (code audit is not exposed to \
                 the offload worker, or this backend is remote — see cImp Settings → Code Audit)"
            ));
        }
        Ok(GatePass { name })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::LOCAL_DATA_TOOLS;

    fn cloud_scope() -> ToolScope {
        ToolScope::default_for(true)
    }

    /// **F-10 restated as an executable claim.** The cloud default scope does
    /// not deny the graph or audit tools — it never has — so a gate that is
    /// only the scope check admits both. This is the assertion that would have
    /// failed for `NativeRouter` before the gate existed, and it fails again if
    /// anyone "simplifies" `admit` back down to the scope.
    #[test]
    fn the_scope_check_alone_does_not_cover_the_graph_and_audit_tools() {
        let scope = cloud_scope();
        for name in [
            "graph_snippet",
            "graph_repo_map",
            "security_audit",
            "quality_audit",
        ] {
            assert!(
                scope.allows_namespaced(name),
                "{name} is not in LOCAL_DATA_TOOLS ({LOCAL_DATA_TOOLS:?}), so the scope allows it"
            );
        }
        // …and the gate denies every one of them for an opted-out backend.
        let gate = BackendGate::new(scope, false, false);
        for name in [
            "graph_snippet",
            "graph_repo_map",
            "security_audit",
            "quality_audit",
        ] {
            let err = gate
                .admit(name)
                .err()
                .unwrap_or_else(|| panic!("{name} must be refused"));
            assert!(
                err.contains("not available on this backend"),
                "{name}: {err}"
            );
            assert!(!gate.admits(name), "{name}");
        }
        // The scope half still works: the local-data tools are denied by rule 1.
        for name in LOCAL_DATA_TOOLS {
            assert!(!gate.admits(name), "{name}");
        }
    }

    /// An opted-in backend admits them, and the pass carries the name it was
    /// granted for — a pass minted for one tool cannot be spent on another
    /// because the name comes out of the pass, not out of the call site.
    #[test]
    fn an_opted_in_backend_admits_and_the_pass_carries_its_name() {
        let gate = BackendGate::new(ToolScope::All, true, true);
        for name in [
            "graph_snippet",
            "security_audit",
            "read_file",
            "ddg__search",
        ] {
            let pass = gate.admit(name).expect("admitted");
            assert_eq!(pass.name(), name);
        }
    }

    /// The two opt-ins are independent axes: graph off must not deny the audit
    /// tools, and audit off must not deny the graph tools.
    #[test]
    fn the_two_opt_ins_are_independent() {
        let graph_only = BackendGate::new(ToolScope::All, true, false);
        assert!(graph_only.admits("graph_snippet"));
        assert!(!graph_only.admits("security_audit"));
        assert!(graph_only.graph_allowed() && !graph_only.audit_allowed());

        let audit_only = BackendGate::new(ToolScope::All, false, true);
        assert!(!audit_only.admits("graph_snippet"));
        assert!(audit_only.admits("quality_audit"));
        assert!(!audit_only.graph_allowed() && audit_only.audit_allowed());
    }
}
