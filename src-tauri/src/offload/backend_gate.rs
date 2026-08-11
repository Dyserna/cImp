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
//! # Rule 4 — `run_check` (finding F-12)
//!
//! `run_check` executes the project's **configured** build/test/lint commands
//! and returns their output, which quotes source. It was in neither
//! `LOCAL_DATA_TOOLS` nor any re-gate, and — unlike F-10 — it did not need a
//! hallucinated name: with the offload toggle on and `checks` configured it was
//! **advertised** in the tool specs a cloud backend received. That hands a third
//! party arbitrary local command execution against the user's repo.
//!
//! The fix has two halves and **neither is sufficient alone**:
//!
//! - `run_check` joined [`crate::settings::LOCAL_DATA_TOOLS`], so a **new** cloud
//!   backend's default scope denies it (rule 1 catches it), and the v29 → v30
//!   migration backfills it into an existing recognizable "web/docs only"
//!   exclusion list.
//! - …but an **already configured** backend can carry `ToolScope::All` (every
//!   LAN backend, by default) or a hand-picked `AllExcept` that does not name it,
//!   and no scope edit reaches those. So rule 4 below is enforced **at call
//!   time**, keyed on the same `is_remote` bit rules 2 and 3 use, with
//!   [`crate::settings::Settings::checks_allow_remote_worker`] as the opt-in.
//!   This is F-10's shape restated — *the helper is right, the call site is
//!   missing* — and a gate consulted only when a backend is **configured** would
//!   not have closed it.
//!
//! Advertisement is derived from the same verdict (both routers filter their
//! defs through [`BackendGate::admits`]), so `run_check` cannot be offered by one
//! rule and refused by another.
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
    /// F-12: whether `run_check` may run for this backend (local, OR the user
    /// opted this project's checks in to a remote worker). It executes the
    /// project's configured commands and returns output that quotes source.
    allow_run_check: bool,
}

/// F-12: whether the offload worker may run the project's configured checks on
/// the chosen backend — local always, remote only on the user's explicit opt-in.
///
/// `is_remote` is *LAN or cloud*, the same bit
/// [`super::service::worker_graph_allowed`] takes, because both are "off this
/// machine" for the boundary this guards. Named and separate so the rule is
/// stated once and testable without a `Settings`.
///
/// Deliberately **not** conditioned on `checks` being non-empty or on
/// `offload.tools.run_check`: those are *advertisement* conditions applied
/// upstream in `tools::enabled_defs`, and folding them in here would turn a
/// local project with no checks configured into a security-flavoured refusal
/// instead of the "no checks configured" guidance the shared entry point already
/// returns. Advertisement stays a subset of admission either way.
pub(super) fn worker_run_check_allowed(is_remote: bool, allow_remote: bool) -> bool {
    !is_remote || allow_remote
}

impl BackendGate {
    /// Build a gate from already-resolved verdicts. Prefer
    /// [`for_worker`](Self::for_worker), which resolves them from settings so
    /// the three worker entry points cannot disagree; this constructor exists
    /// for callers that already hold the verdicts (and for tests).
    ///
    /// Every verdict is a **positional required argument** on purpose: adding a
    /// rule to [`admit`](Self::admit) breaks every existing call site, so a new
    /// admission decision cannot be added with a silently permissive default
    /// anywhere (F-12 grew rule 4 this way).
    pub fn new(
        scope: ToolScope,
        allow_graph: bool,
        allow_audit: bool,
        allow_run_check: bool,
    ) -> Self {
        Self {
            scope,
            allow_graph,
            allow_audit,
            allow_run_check,
        }
    }

    /// The worker's policy, resolved from one settings snapshot.
    ///
    /// `is_remote` is *LAN or cloud* — the same bit `OffloadService`'s pool
    /// entries carry — because both are "off this machine" for the three data
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
            // F-12. Resolved HERE — the one place all three worker entry points
            // (`OffloadService::run_on`, the headless child's `run_on_backend`,
            // the supervisor self-test) build their gate — so the opt-in reaches
            // an **already configured** backend on its very next call, with no
            // settings edit and no re-save of the backend.
            worker_run_check_allowed(is_remote, settings.checks_allow_remote_worker),
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
        // 4. F-12 — the `run_check` opt-in. Rule 1 already denies it for a
        //    backend whose scope names it, but a backend configured before this
        //    rule existed carries a scope that does not (`All`, or an
        //    `AllExcept` picked by hand), so this is the half that reaches
        //    those — and it must be here, at call time, not at the moment a
        //    backend is configured.
        //
        //    The refusal names the cause it actually checked (global principle
        //    3): this backend is off-machine and the project has not opted in.
        if name == "run_check" && !self.allow_run_check {
            return Err(format!(
                "tool `{name}` is not available on this backend (it executes this project's \
                 configured build/test/lint commands, and running the project's checks on a \
                 remote offload backend is off — enable it for this project in cImp Settings → \
                 Code Intelligence → Checks)"
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
        let gate = BackendGate::new(scope, false, false, false);
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
        let gate = BackendGate::new(ToolScope::All, true, true, true);
        for name in [
            "graph_snippet",
            "security_audit",
            "read_file",
            "run_check",
            "ddg__search",
        ] {
            let pass = gate.admit(name).expect("admitted");
            assert_eq!(pass.name(), name);
        }
    }

    /// The opt-ins are independent axes: graph off must not deny the audit
    /// tools, audit off must not deny the graph tools, and neither may deny (or
    /// admit) `run_check`.
    #[test]
    fn the_two_opt_ins_are_independent() {
        let graph_only = BackendGate::new(ToolScope::All, true, false, false);
        assert!(graph_only.admits("graph_snippet"));
        assert!(!graph_only.admits("security_audit"));
        assert!(!graph_only.admits("run_check"));
        assert!(graph_only.graph_allowed() && !graph_only.audit_allowed());

        let audit_only = BackendGate::new(ToolScope::All, false, true, false);
        assert!(!audit_only.admits("graph_snippet"));
        assert!(audit_only.admits("quality_audit"));
        assert!(!audit_only.admits("run_check"));
        assert!(!audit_only.graph_allowed() && audit_only.audit_allowed());

        let check_only = BackendGate::new(ToolScope::All, false, false, true);
        assert!(!check_only.admits("graph_snippet"));
        assert!(!check_only.admits("quality_audit"));
        assert!(check_only.admits("run_check"));
    }

    // ── #48, finding F-12 — `run_check` on a remote backend ────────────────

    /// **F-12's `LOCAL_DATA_TOOLS` half, restated as an executable claim.** A
    /// *newly* configured cloud backend takes `ToolScope::default_for(true)`,
    /// which is `AllExcept { LOCAL_DATA_TOOLS }` — so rule 1 alone must now deny
    /// `run_check`, and this fails if anyone removes it from that set.
    #[test]
    fn a_new_cloud_backends_default_scope_denies_run_check_on_rule_one_alone() {
        assert!(
            LOCAL_DATA_TOOLS.contains(&"run_check"),
            "F-12: `run_check` executes the project's configured commands — it belongs in \
             LOCAL_DATA_TOOLS ({LOCAL_DATA_TOOLS:?})"
        );
        let scope = cloud_scope();
        assert!(!scope.allows_namespaced("run_check"));
        // Rule 1 fires even with the opt-in ON: the user opted the *project* in,
        // not this backend's scope, and the scope is the narrower statement.
        let opted_in = BackendGate::new(scope, true, true, true);
        let err = opted_in
            .admit("run_check")
            .err()
            .expect("denied by the scope");
        assert!(err.contains("denied by its tool scope"), "{err}");
    }

    /// **F-12's call-time half, and why the scope half is not enough.** The
    /// backend an existing install actually has is `ToolScope::All` (the default
    /// for every non-cloud backend, and what a LAN box keeps) — a scope that
    /// admits `run_check` and that no edit to `LOCAL_DATA_TOOLS` and no migration
    /// of an `AllExcept` list will ever touch. Rule 4 is the only thing standing
    /// there, which is why it is enforced on every call rather than when the
    /// backend is configured.
    #[test]
    fn an_existing_remote_backend_with_an_untouched_scope_is_denied_at_call_time() {
        let legacy_scope = ToolScope::All;
        assert!(
            legacy_scope.allows_namespaced("run_check"),
            "the scope half cannot reach a ToolScope::All backend — that is the point"
        );
        let gate = BackendGate::new(legacy_scope, true, true, false);
        let err = gate
            .admit("run_check")
            .err()
            .expect("F-12 must refuse this");
        assert!(
            err.contains("configured build/test/lint commands"),
            "the refusal must name the cause it checked: {err}"
        );
        assert!(!gate.admits("run_check"));
        // Nothing else regressed: the same gate still admits the rest.
        assert!(gate.admits("read_file") && gate.admits("graph_snippet"));
    }

    /// `worker_run_check_allowed` — local always, remote only on the opt-in.
    /// The truth table `for_worker` resolves, stated where it can be read.
    #[test]
    fn worker_run_check_allowed_is_local_always_remote_on_opt_in() {
        assert!(worker_run_check_allowed(false, false));
        assert!(worker_run_check_allowed(false, true));
        assert!(!worker_run_check_allowed(true, false));
        assert!(worker_run_check_allowed(true, true));
    }

    /// `for_worker` resolves rule 4 from the real `Settings` field, in the one
    /// constructor all three worker entry points use — including the F-19 check
    /// that a config file predating the field lands on the *denied* side.
    #[test]
    fn for_worker_resolves_run_check_from_settings_and_defaults_to_denied() {
        // A settings file that predates the field (container-level
        // `#[serde(default)]` fills it) — the pre-existing-install case.
        let legacy: Settings = serde_json::from_str(r#"{"schema_version": 29}"#)
            .expect("a pre-F-12 settings file deserializes");
        assert!(
            !legacy.checks_allow_remote_worker,
            "F-19 trap: the additive field's default must be the SAFE value"
        );

        // Remote + not opted in ⇒ denied. `ToolScope::All` so only rule 4 can
        // be doing the work.
        let remote = BackendGate::for_worker(ToolScope::All, true, &legacy);
        assert!(!remote.admits("run_check"));
        // Local ⇒ allowed regardless of the opt-in.
        let local = BackendGate::for_worker(ToolScope::All, false, &legacy);
        assert!(local.admits("run_check"));

        // Remote + opted in ⇒ the opt-in actually permits it, on a backend that
        // was configured long before the flag existed.
        let mut opted = legacy.clone();
        opted.checks_allow_remote_worker = true;
        let remote_opted = BackendGate::for_worker(ToolScope::All, true, &opted);
        let pass = remote_opted
            .admit("run_check")
            .expect("the opt-in must actually permit it");
        assert_eq!(pass.name(), "run_check");
    }
}
