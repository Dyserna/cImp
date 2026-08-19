//! V8-01 native baseline offload tools — built into cImp, zero
//! external deps, so offload works before any MCP server is installed:
//!
//! - [`read_file`] — bounded line/byte reads within an `allowed_root`.
//! - [`code_search`] — literal/substring search across an `allowed_root`
//!   (the deep-search case that motivated this milestone).
//! - [`run_command`] — allowlisted, read-only command execution.
//!
//! Dispatch is function-based ([`dispatch`]) rather than a trait so the
//! agent loop can route a model `tool_call` to its owner without a
//! `dyn`-async dance. Each tool module exposes a `def()` ([`ToolDef`])
//! and an `execute`. All file access is confined to [`ToolCtx::allowed_roots`].

use std::path::{Path, PathBuf};

use crate::offload::backend_gate::GatePass;
use crate::offload::openai::ToolDef;
use crate::settings::{CommandPolicy, OffloadToolToggles};

pub mod audit_tools;
pub mod code_search;
pub mod graph_tools;
pub mod list_dir;
pub mod read_file;
pub mod run_check;
pub mod run_command;

/// V33 Phase F: everything the worker seam needs to take a Workbench checkpoint
/// before a filesystem-mutating tool call.
///
/// **Why this is a struct on [`ToolCtx`] and not three loose fields.** `ToolCtx`
/// carried no project root, no tab and no service handle at all — the worker
/// never needed to reach outside its allowed roots. Adding the capability as one
/// `Option` makes the "this dispatch cannot checkpoint" case a single
///, greppable state (the headless MCP child, every unit test) instead of three
/// independently-`None` fields whose combinations nobody enumerated.
#[derive(Clone)]
pub struct ToolCheckpoint {
    /// The project root whose shadow repo the checkpoint lands in — the calling
    /// session's cwd, the same value that seeds `allowed_roots`' fallback.
    pub root: PathBuf,
    /// The cImp tab this offload task was requested from, if the request
    /// carried one. `None` keys the tab-less throttle bucket (shared with the
    /// burst trigger) and records a checkpoint with no tab — honest, since the
    /// worker is not a tab.
    pub tab: Option<String>,
    /// The Workbench service. Present only in-process; see this type's doc.
    pub workbench: std::sync::Arc<crate::workbench::WorkbenchService>,
}

/// `WorkbenchService` is not `Debug` (it owns a Tauri `AppHandle`), and
/// `ToolCtx` derives `Debug` — so the handle is rendered as a marker rather than
/// dropping `Debug` from the whole context, which several error paths format.
impl std::fmt::Debug for ToolCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCheckpoint")
            .field("root", &self.root)
            .field("tab", &self.tab)
            .finish_non_exhaustive()
    }
}

/// Shared execution context for native tools: the roots file access is
/// confined to and the allowlist `run_command` is gated by.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    /// Roots that `read_file`/`code_search`/`run_command` are confined
    /// to. Guaranteed non-empty by the constructor (falls back to the
    /// launch project root).
    pub allowed_roots: Vec<PathBuf>,
    /// Programs `run_command` may execute (matched by program name).
    /// Empty = nothing runnable (deny by default).
    pub command_allowlist: Vec<String>,
    /// Per-program security policies `run_command` enforces on top of the
    /// allowlist (denied flags/subcommands + spawn env). See [`CommandPolicy`].
    pub command_policies: Vec<CommandPolicy>,
    /// V33 Phase F: the pre-mutation checkpoint capability, or `None` when this
    /// dispatch cannot take one.
    ///
    /// `None` is not a failure mode to fix — it is the honest answer on the two
    /// paths that have no app to snapshot into: the headless `--offload-mcp`
    /// child (a separate process, which runs the agent loop only when no app
    /// instance is serving the loopback) and unit tests. [`ToolCtx::new`]
    /// therefore builds it `None` and callers opt in with
    /// [`with_checkpoint`](Self::with_checkpoint), so a new construction site
    /// silently gets the safe shape rather than a wrong one.
    pub checkpoint: Option<ToolCheckpoint>,
    /// V33 Phase A: the OS-sandbox configuration `run_command` spawns under.
    ///
    /// Carried here rather than read from a global for the same reason
    /// `command_allowlist` is: the headless `--offload-mcp` child and every
    /// unit test get exactly what their constructor passed. [`ToolCtx::new`]
    /// builds it disabled — the safe, honest shape — and the in-app path opts
    /// in from settings via [`with_sandbox`](Self::with_sandbox), so a new
    /// construction site cannot silently claim to be sandboxing.
    pub sandbox: crate::sandbox::SandboxCfg,
}

impl ToolCtx {
    /// Build a context, falling back to `launch_root` when the
    /// configured `allowed_roots` is empty (the documented default).
    pub fn new(
        mut allowed_roots: Vec<PathBuf>,
        command_allowlist: Vec<String>,
        command_policies: Vec<CommandPolicy>,
        launch_root: &Path,
    ) -> Self {
        if allowed_roots.is_empty() {
            allowed_roots.push(launch_root.to_path_buf());
        }
        Self {
            allowed_roots,
            command_allowlist,
            command_policies,
            checkpoint: None,
            sandbox: crate::sandbox::SandboxCfg::disabled(),
        }
    }

    /// V33 Phase F: opt this context in to pre-mutation checkpoints. A named
    /// builder rather than a fifth positional argument to [`Self::new`] — the
    /// three existing ones are already two `Vec<String>`-shaped neighbours.
    pub fn with_checkpoint(mut self, checkpoint: Option<ToolCheckpoint>) -> Self {
        self.checkpoint = checkpoint;
        self
    }

    /// V33 Phase A: opt this context in to OS sandboxing. Same named-builder
    /// discipline as [`with_checkpoint`](Self::with_checkpoint), and the same
    /// reason: the default must be the honest one.
    pub fn with_sandbox(mut self, sandbox: crate::sandbox::SandboxCfg) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Resolve a model-supplied path and confine it to `allowed_roots`.
    /// Returns the canonical path on success, or an error string the
    /// loop feeds back to the model so it can correct itself. The path
    /// must exist (we canonicalize); for not-yet-existing paths this is
    /// the right behavior — offload is read-only.
    pub fn confine(&self, requested: &str) -> Result<PathBuf, String> {
        let raw = PathBuf::from(requested);
        // Candidate locations: an absolute request as-is, else the request
        // resolved against EACH root — a relative path may legitimately live
        // under any configured root, not just the first.
        let candidates: Vec<PathBuf> = if raw.is_absolute() {
            vec![raw]
        } else {
            self.allowed_roots.iter().map(|r| r.join(&raw)).collect()
        };
        // Collect every distinct in-root resolution. A relative path can
        // resolve under more than one root when roots overlap/nest; silently
        // returning the first is order-dependent and surprising, so flag the
        // ambiguity instead. The per-root canonicalize + boundary check is the
        // shared [`crate::fsutil::confine_existing`] core (target must exist —
        // offload is read-only); the multi-root/ambiguity policy stays here.
        // (An absolute request has a single candidate and can never be
        // ambiguous here.)
        let mut matches: Vec<PathBuf> = Vec::new();
        for cand in candidates {
            for root in &self.allowed_roots {
                if let Ok(canon) = crate::fsutil::confine_existing(root, &cand) {
                    if !matches.contains(&canon) {
                        matches.push(canon);
                    }
                    // This candidate is confined; don't double-count it across
                    // overlapping roots (it canonicalizes to one real path).
                    break;
                }
            }
        }
        match matches.len() {
            0 => Err(format!(
                "`{requested}` is outside the allowed roots ({} configured)",
                self.allowed_roots.len()
            )),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => Err(format!(
                "`{requested}` is ambiguous — it resolves to {n} different files across \
                 the configured roots. Pass an absolute path to disambiguate."
            )),
        }
    }
}

/// The [`ToolDef`]s for the native tools enabled by `toggles`. Fed into
/// the chat request's `tools` array alongside any MCP-server tools.
///
/// `run_check` (V21 F6) additionally requires the project to have configured
/// `checks` — read live here, gated identically to the MCP surface
/// (`graph/mcp.rs`), so a fresh project sees no `run_check` on either side.
///
/// V38 Phase D: "configured" means the EFFECTIVE set, plugin `check`-kind tools
/// included, through the same `checks::plugin` join the MCP surface uses. Two
/// gates reading two different definitions of "has checks" would advertise the
/// tool on one leg and not the other for exactly the projects V38 exists to
/// serve — the ones whose checks come from a plugin rather than from an array
/// the user hand-wrote.
pub fn enabled_defs(toggles: &OffloadToolToggles) -> Vec<ToolDef> {
    let checks_configured = {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        !crate::checks::plugin::effective_check_names(&crate::settings::load_readonly(&cwd))
            .is_empty()
    };
    enabled_defs_inner(toggles, checks_configured)
}

/// The pure toggle→def mapping, split from the live `checks` read so the
/// `run_check` advertisement gate is testable without touching disk settings.
fn enabled_defs_inner(toggles: &OffloadToolToggles, checks_configured: bool) -> Vec<ToolDef> {
    let mut defs = Vec::new();
    if toggles.read_file {
        defs.push(read_file::def());
    }
    if toggles.list_dir {
        defs.push(list_dir::def());
    }
    if toggles.code_search {
        defs.push(code_search::def());
    }
    if toggles.run_command {
        defs.push(run_command::def());
    }
    // V21 F6: advertised only when checks are configured for the project root —
    // the tool can't do anything useful otherwise, and the gate matches the MCP
    // surface so exposure is consistent across both consumers.
    if toggles.run_check && checks_configured {
        defs.push(run_check::def());
    }
    defs
}

/// Route a native `tool_call` to its executor. `args` is the parsed
/// arguments object. Returns the tool result (or an error string the
/// loop surfaces to the model as a `role: tool` message — never a panic).
///
/// # The name comes from a [`GatePass`], not from the caller (#48, F-10)
///
/// A tool name reaching this function has been admitted by
/// [`BackendGate::admit`](crate::offload::backend_gate::BackendGate::admit) —
/// the backend's tool scope plus the code-graph and code-audit opt-ins. Taking
/// the name *out of the pass* rather than beside it is what makes that
/// structural: a router that skipped the gate has nothing to pass here and does
/// not compile, and a pass minted for one name cannot be spent on another.
///
/// # Every arm here needs a `toolclass::TABLE` row (#48, M-2)
///
/// This match is one of the two native dispatch surfaces the taint latch gates.
/// A capability tool added below with no row in
/// [`TABLE`](crate::offload::toolclass::TABLE) classifies EXTERNAL, and on a
/// native route an EXTERNAL classification is waved past the latch — so
/// `table_matches_the_native_dispatch_surface` scans this function's own source
/// and fails the build for a missing row.
///
/// # V33 Phase F — the pre-mutation checkpoint fires HERE
///
/// The `mutates_fs` column of that same table decides it, so a future mutating
/// tool declares its class and its need for a checkpoint in one reviewed place
/// and this function needs no edit. Today exactly one routed tool qualifies
/// (`run_command`); the check is written against the table rather than against
/// that name so it does not have to be remembered.
///
/// This is the ONE Phase F seam where the "immediately before" ordering is
/// exact: nothing has been spawned yet when the checkpoint is awaited, and the
/// await is inside the same task. The Claude `PreToolUse` shim cannot make that
/// promise (its fail-open contract forbids waiting on the app — see
/// `checkpoint_beacon`'s module doc).
///
/// **`/graph_run` and `/mcp/call` are deliberately NOT wired**, and this is the
/// note that records why so it is not re-raised as an omission: neither route
/// serves a tool with `mutates_fs: true`. `/graph_run` serves the read-only
/// `graph_*` surface, and `/mcp/call` serves proxied MCP servers whose tools are
/// outside this filesystem by construction (`mutates_fs` answers `false` for
/// every unknown name, which is exactly the right answer there). Wiring them
/// would add a `mutates_fs` lookup to two hot routes that would fire zero
/// checkpoints. If a mutating tool is ever added to either surface, its `TABLE`
/// row is what makes that visible — and the fire seam is what would then need
/// adding, not the row.
pub async fn dispatch(
    pass: GatePass<'_>,
    args: serde_json::Value,
    ctx: &ToolCtx,
) -> Result<String, String> {
    let name = pass.name();
    // Before ANY executor runs, and after the gate has admitted the call (the
    // pass is proof of that): a refused call must leave no checkpoint blaming
    // a tool that never ran.
    if crate::offload::toolclass::mutates_fs(name) {
        if let Some(cp) = &ctx.checkpoint {
            // Awaited: see this function's doc. Infallible by construction —
            // `on_tool` swallows its own errors so a shadow-repo problem can
            // never fail a tool call the user asked for.
            //
            // `None` deadline, unlike the two out-of-process seams: this one is
            // in-process and nothing here gives up waiting, so the snapshot has
            // no reason to abandon itself — the executor below simply does not
            // start until it is done. The pre-tool budget exists only where a
            // caller stops waiting while the agent's tool runs anyway
            // (`loopback::TOOL_CHECKPOINT_BUDGET`). The returned "did it settle"
            // flag has no consumer here for the same reason: there is no miss
            // this seam can produce.
            let _ = cp
                .workbench
                .on_tool(
                    &cp.root,
                    crate::workbench::shadow::Origin::new(
                        Some("offload".to_string()),
                        None,
                        cp.tab.clone(),
                    ),
                    &format!("offload:{name}"),
                    None,
                )
                .await;
        }
    }
    match name {
        "read_file" => read_file::execute(args, ctx).await,
        "list_dir" => list_dir::execute(args, ctx).await,
        "code_search" => code_search::execute(args, ctx).await,
        "run_command" => run_command::execute(args, ctx).await,
        // V21 F6: worker-native `run_check` — routes to the SAME checks entry
        // point the MCP handler uses (via `crate::graph::offload_run_check`),
        // beside the `graph_` route below because both share the graph module's
        // project-root resolution + activity recording.
        "run_check" => run_check::execute(args, ctx).await,
        // V26 code-audit tools — the two fixed names route to the same executor
        // (which maps each to its category). Advertised only when the service
        // decided to offer them (enabled + `expose_offload` + local backend)
        // and re-gated in the router's `call` via `allow_audit`, exactly like
        // the graph tools — the scan runs locally either way, but its report
        // is local data that must not reach an opted-out or remote backend.
        "security_audit" | "quality_audit" => audit_tools::execute(name, args, ctx).await,
        // V9-01 graph tools (advertised only when the service decided to offer
        // them — feature on + local-or-opted-in remote — and re-gated in the
        // router's `call`).
        n if n.starts_with("graph_") => graph_tools::dispatch(name, args, ctx).await,
        other => Err(format!("unknown native tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> ToolCtx {
        let cwd = std::env::current_dir().unwrap();
        ToolCtx::new(vec![cwd.clone()], vec![], vec![], &cwd)
    }

    /// V26: both audit tool names route into `audit_tools::execute` rather than
    /// falling through to the unknown-tool arm. In a unit test the audit global
    /// is never set, so a correctly-routed call errors — all this test pins is
    /// that it is NOT the unknown-tool error (the exact executor message is
    /// owned and pinned by `audit_tools`'s own test, not double-pinned here).
    #[tokio::test]
    async fn dispatch_routes_the_audit_tools() {
        for name in ["security_audit", "quality_audit"] {
            let err = dispatch(GatePass::for_test(name), serde_json::json!({}), &test_ctx())
                .await
                .expect_err("no audit global in a unit test → error");
            assert!(
                !err.contains("unknown native tool"),
                "`{name}` must route to audit_tools::execute, got: {err}"
            );
        }
    }
}
