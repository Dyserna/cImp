//! V8-03 app-side offload service — the long-lived home of the loop, the
//! backend pool, the router, and the global concurrency gate.
//!
//! This is V8-02's `mcp.rs::run_offload` relocated into the Tauri app, now
//! reading **live** state instead of re-probing per call:
//!
//! - Local backends contribute their live [`LlamaServer`] handle (held by
//!   the [`OffloadSupervisor`]), so `in_flight`/`n_ctx`/slots are honest and
//!   acquiring a slot reflects every other in-flight offload across all
//!   Claude tabs — which is why V8-02's **spill-on-busy finally fires in
//!   production** (the per-call child always saw `in_flight == 0`).
//! - Remote backends are cached warm handles ([`RemoteBackend`]) so their
//!   slot accounting persists across calls too.
//! - A single **global semaphore** caps total offloads in flight; the
//!   chosen backend's own slot is acquired underneath it.
//! - Tools come from the warm [`McpHost`] (native baseline + the user's tool
//!   servers, namespaced and read-class) merged and scoped per backend.
//!
//! The self-contained per-call child ([`super::mcp`]) stays as the headless
//! fallback (native-only, no warm host) for when the app isn't running; both
//! paths share the pure router ([`super::router`]) and the agent loop
//! ([`super::agent`]) so they can't drift on routing or loop semantics.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, Mutex as TokioMutex, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::settings::{
    BackendTier, OffloadBackend, OffloadBackendKind, OffloadSettings, SettingsHandle, ToolScope,
};

use super::agent::{self, AgentConfig, HostRouter, OffloadTask, ThinkingMode};
use super::mcp_host::{McpHost, McpServerHealth};
use super::remote::RemoteBackend;
use super::router::{self, BackendView, RouteError, TierHint};
use super::server::{LlamaServer, ServerCommand};
use super::supervisor::OffloadSupervisor;
use super::tools::{self, ToolCtx};
use super::Backend;

/// Ceiling on the auto-sized global gate so a wildly-configured pool can't
/// open thousands of concurrent loops.
const GLOBAL_CONCURRENCY_MAX: u32 = 32;

/// Aggregate offload-service status surfaced to Settings: the honest global
/// in-flight count (the warm-pool fix) and per-MCP-server health rows.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ServiceStatus {
    pub global_in_flight: u32,
    pub global_cap: u32,
    pub mcp_servers: Vec<McpServerHealth>,
}

/// A live handle for slot acquisition on the chosen backend.
enum Handle {
    Local(Arc<LlamaServer>),
    Remote(Arc<RemoteBackend>),
}

impl Handle {
    async fn acquire_slot(&self, timeout: Duration) -> AppResult<OwnedSemaphorePermit> {
        match self {
            Handle::Local(s) => s.acquire_slot(timeout).await,
            Handle::Remote(r) => r.acquire_slot(timeout).await,
        }
    }
}

/// One resolved pool member: its config-derived identity plus a live handle
/// and the freshly-probed routing state.
struct PoolEntry {
    name: String,
    base_url: String,
    auth_token: Option<String>,
    cloud_blocked: bool,
    tier: BackendTier,
    tool_scope: ToolScope,
    handle: Handle,
    ready: bool,
    n_ctx: Option<u32>,
    slots: u32,
    in_flight: u32,
}

/// The app-owned offload service. Held in `AppState` beside the supervisor.
pub struct OffloadService {
    settings: SettingsHandle,
    supervisor: Arc<OffloadSupervisor>,
    host: Arc<McpHost>,
    /// Total offloads in flight across the whole app.
    global_gate: Arc<Semaphore>,
    global_cap: u32,
    /// Warm remote backend handles, keyed by backend name (so `in_flight`
    /// persists across calls). Rebuilt when a backend's config changes.
    remote_pool: TokioMutex<HashMap<String, Arc<RemoteBackend>>>,
    /// Long-timeout client for the agent loop's chat-completions calls
    /// (health/`/props` probes use each handle's own short-timeout client).
    client: reqwest::Client,
    /// Capability-change pulses relayed to the loopback `/events` stream.
    change_tx: broadcast::Sender<()>,
}

impl OffloadService {
    /// Construct the service. Sizes the global gate from config (or the
    /// explicit `global_concurrency` override) and wires the MCP host's
    /// change channel into the service's own.
    pub fn new(settings: SettingsHandle, supervisor: Arc<OffloadSupervisor>) -> Arc<Self> {
        let snap = settings.current().offload;
        let global_cap = compute_global_cap(&snap);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(snap.offload_timeout_secs.max(30)))
            .build()
            .unwrap_or_default();
        let host = McpHost::new();
        let (change_tx, _) = broadcast::channel(16);

        let service = Arc::new(Self {
            settings,
            supervisor,
            host,
            global_gate: Arc::new(Semaphore::new(global_cap as usize)),
            global_cap,
            remote_pool: TokioMutex::new(HashMap::new()),
            client,
            change_tx,
        });

        // Relay MCP-host change pulses into the service change channel so a
        // tool server connecting/dropping reaches `/events`.
        {
            let mut host_rx = service.host.subscribe();
            let out = service.change_tx.clone();
            tauri::async_runtime::spawn(async move {
                while host_rx.recv().await.is_ok() {
                    let _ = out.send(());
                }
            });
        }
        service
    }

    /// Subscribe to capability-change pulses (a backend or tool server went
    /// up/down, a server connected/dropped). The loopback `/events` stream
    /// relays each as a `tools/list_changed` to Claude.
    pub fn subscribe_changes(&self) -> broadcast::Receiver<()> {
        self.change_tx.subscribe()
    }

    /// Emit a capability-change pulse.
    fn signal_change(&self) {
        let _ = self.change_tx.send(());
    }

    /// Total offloads currently in flight across the app (global gate).
    pub fn global_in_flight(&self) -> u32 {
        self.global_cap
            .saturating_sub(self.global_gate.available_permits() as u32)
    }

    /// Bring the warm MCP host in line with current config. Cheap when the
    /// pool is already warm; called before each run and at startup.
    pub async fn warm_host(&self) {
        let snap = self.settings.current().offload;
        if !snap.enabled {
            self.host.shutdown().await;
            return;
        }
        let roots = effective_roots(&snap);
        self.host.reconcile(&snap.mcp_servers, &roots).await;
    }

    /// Aggregate service status for the Settings readout: the honest global
    /// in-flight count and per-MCP-server health.
    pub async fn status(&self) -> ServiceStatus {
        ServiceStatus {
            global_in_flight: self.global_in_flight(),
            global_cap: self.global_cap,
            mcp_servers: self.host.health().await,
        }
    }

    /// Run one offload task end-to-end against the live pool and return the
    /// synthesized answer. Acquires the global permit *and* the chosen
    /// backend's slot, so `in_flight` is honest and the global gate queues a
    /// busy pool coherently.
    pub async fn run(
        &self,
        instructions: String,
        context: Option<String>,
        thinking: ThinkingMode,
        tier: TierHint,
    ) -> AppResult<String> {
        let snap = self.settings.current().offload;
        if !snap.enabled {
            return Err(AppError::OffloadNotReady(
                "offload is disabled — enable it in ccImp settings".into(),
            ));
        }
        let timeout = Duration::from_secs(snap.offload_timeout_secs.max(30));
        let overall_deadline = Instant::now() + timeout;

        // Keep the warm tool-server pool current before routing.
        self.warm_host().await;

        // Global gate first: bound total in-flight across the whole app.
        let _global = match tokio::time::timeout(timeout, self.global_gate.clone().acquire_owned())
            .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(_)) => return Err(AppError::Offload("global offload gate closed".into())),
            Err(_) => {
                return Err(AppError::OffloadNotReady(format!(
                    "all {} global offload slots busy — timed out after {}s",
                    self.global_cap,
                    timeout.as_secs()
                )))
            }
        };

        // Resolve + probe the pool from live state.
        let pool = self.resolve_pool(&snap).await;
        if pool.is_empty() {
            return Err(AppError::OffloadNotReady(
                "no offload backend is configured — add one in ccImp Settings → Offload".into(),
            ));
        }

        let views: Vec<BackendView> = pool
            .iter()
            .map(|p| BackendView {
                name: p.name.clone(),
                ready: p.ready,
                cloud_blocked: p.cloud_blocked,
                n_ctx: p.n_ctx,
                slots: p.slots,
                in_flight: p.in_flight,
                tier: p.tier,
                tool_scope: p.tool_scope.clone(),
                budget_high_water_pct: snap.budget_high_water_pct,
            })
            .collect();

        let req = router::analyze_task(&instructions, context.as_deref(), tier);
        let chosen = router::select(&views, &req).map_err(|e: RouteError| {
            AppError::OffloadNotReady(e.to_string())
        })?;
        info!(
            target: "offload",
            task_chars = instructions.len(),
            est_ctx = req.estimated_context,
            tier = ?req.tier_hint,
            backend = %views[chosen].name,
            global_in_flight = self.global_in_flight(),
            "offload: routed task → backend `{}` (app service)",
            views[chosen].name
        );

        let result = self
            .run_on(&pool[chosen], &views[chosen], &snap, &instructions, context.clone(), thinking, overall_deadline)
            .await;

        // One fail-over on a connection-class failure: drop the failed
        // backend and re-select among the rest.
        match result {
            Ok(text) => Ok(text),
            Err(e) if is_connection_error(&e.to_string()) && views.len() > 1 => {
                let mut alt = views.clone();
                alt[chosen].ready = false;
                match router::select(&alt, &req) {
                    Ok(next) if next != chosen => {
                        warn!(
                            failed = %pool[chosen].name,
                            reroute = %pool[next].name,
                            "offload: re-routing after connection failure (app service)"
                        );
                        self.run_on(&pool[next], &views[next], &snap, &instructions, context, thinking, overall_deadline)
                            .await
                    }
                    _ => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Run the agent loop against one chosen backend: acquire its slot,
    /// build the host-aware scoped router, and drive [`agent::run`].
    #[allow(clippy::too_many_arguments)]
    async fn run_on(
        &self,
        entry: &PoolEntry,
        view: &BackendView,
        snap: &OffloadSettings,
        instructions: &str,
        context: Option<String>,
        thinking: ThinkingMode,
        deadline: Instant,
    ) -> AppResult<String> {
        let slot_timeout = deadline.saturating_duration_since(Instant::now());
        let _slot = entry.handle.acquire_slot(slot_timeout).await?;

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let ctx = ToolCtx::new(
            effective_roots(snap),
            snap.command_allowlist.clone(),
            &cwd,
        );
        let native_defs = tools::enabled_defs(&snap.tools);
        let mcp_defs = self.host.tool_defs().await;
        let router = HostRouter::new(
            native_defs,
            mcp_defs,
            ctx,
            self.host.clone(),
            entry.tool_scope.clone(),
        );
        let cfg = AgentConfig {
            base_url: entry.base_url.clone(),
            model: None,
            max_steps: snap.max_steps.max(1),
            budget_tokens: view.per_slot_budget(),
            per_tool_result_token_cap: snap.per_tool_result_token_cap.max(256),
            auth_token: entry.auth_token.clone(),
        };
        let task = OffloadTask {
            instructions: instructions.to_string(),
            context,
            thinking,
        };
        agent::run(&self.client, &cfg, &router, task, deadline).await
    }

    /// Resolve the enabled backend pool from live state: live local handles
    /// from the supervisor (real `in_flight`/`n_ctx`), warm remote handles
    /// from the cache (health-refreshed). Probes concurrently.
    async fn resolve_pool(&self, snap: &OffloadSettings) -> Vec<PoolEntry> {
        let backends: Vec<OffloadBackend> = snap
            .effective_backends()
            .into_iter()
            .filter(|b| b.enabled)
            .collect();

        let mut entries = Vec::with_capacity(backends.len());
        for b in &backends {
            match &b.kind {
                OffloadBackendKind::Local { server_command, .. } => {
                    if let Some(e) = self.resolve_local(b, server_command).await {
                        entries.push(e);
                    }
                }
                OffloadBackendKind::Remote {
                    base_url,
                    auth_token,
                    is_cloud,
                    ..
                } => {
                    if let Some(e) = self
                        .resolve_remote(b, base_url, auth_token, *is_cloud)
                        .await
                    {
                        entries.push(e);
                    }
                }
            }
        }
        entries
    }

    /// Resolve one Local backend to a live handle. Prefers the supervisor's
    /// running server (honest `in_flight`); if it isn't running, kicks a
    /// lazy non-blocking start and probes the URL once (so a user-launched
    /// `llama-server` still works this call).
    async fn resolve_local(
        &self,
        b: &OffloadBackend,
        server_command: &str,
    ) -> Option<PoolEntry> {
        if server_command.trim().is_empty() {
            return None;
        }
        let cmd = ServerCommand::parse(server_command).ok()?;
        let base_url = cmd.base_url();

        if let Some(server) = self.supervisor.running_server(&b.name).await {
            // Refresh health/props on the warm handle.
            let ready = server.health_check().await;
            if ready {
                let _ = server.refresh_props().await;
            }
            return Some(PoolEntry {
                name: b.name.clone(),
                base_url,
                auth_token: None,
                cloud_blocked: false,
                tier: b.tier,
                tool_scope: b.tool_scope.clone(),
                ready,
                n_ctx: server.n_ctx().or(b.declared_context),
                slots: server.slots(),
                in_flight: server.in_flight(),
                handle: Handle::Local(server),
            });
        }

        // Not running: warm it for next time, and try a transient handle for
        // a server the user may have launched themselves.
        let supervisor = self.supervisor.clone();
        let name = b.name.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = supervisor.start_backend(&name).await {
                debug!(backend = %name, error = %e, "offload: lazy start failed");
            }
        });

        let transient = Arc::new(
            LlamaServer::with_config(&b.name, server_command, b.tier, b.tool_scope.clone()).ok()?,
        );
        let ready = transient.health_check().await;
        if ready {
            let _ = transient.refresh_props().await;
        }
        Some(PoolEntry {
            name: b.name.clone(),
            base_url,
            auth_token: None,
            cloud_blocked: false,
            tier: b.tier,
            tool_scope: b.tool_scope.clone(),
            ready,
            n_ctx: transient.n_ctx().or(b.declared_context),
            slots: transient.slots(),
            in_flight: transient.in_flight(),
            handle: Handle::Local(transient),
        })
    }

    /// Resolve one Remote backend to a warm, health-refreshed handle (cached
    /// so `in_flight` persists across calls).
    async fn resolve_remote(
        &self,
        b: &OffloadBackend,
        base_url: &str,
        auth_token: &str,
        is_cloud: bool,
    ) -> Option<PoolEntry> {
        if base_url.trim().is_empty() {
            return None;
        }
        let cloud_blocked = b.cloud_blocked();
        let slots_decl = 1u32;

        // Reuse a cached handle when the URL/auth/cloud signature matches; a
        // fresh handle would reset `in_flight`.
        let sig = format!("{base_url}|{}|{is_cloud}", auth_token.is_empty());
        let handle = {
            let mut pool = self.remote_pool.lock().await;
            let reuse = pool.get(&b.name).filter(|h| h.base_url() == base_url.trim_end_matches('/'));
            match reuse {
                Some(h) if remote_sig(h, is_cloud) == sig => h.clone(),
                _ => {
                    let h = Arc::new(
                        RemoteBackend::new(
                            &b.name,
                            base_url,
                            auth_token,
                            is_cloud,
                            b.tier,
                            b.tool_scope.clone(),
                            b.declared_context,
                            slots_decl,
                        )
                        .ok()?,
                    );
                    pool.insert(b.name.clone(), h.clone());
                    h
                }
            }
        };

        let ready = if cloud_blocked {
            false
        } else {
            let ok = handle.health_check().await;
            if ok {
                let _ = handle.refresh_props().await;
            }
            ok
        };

        Some(PoolEntry {
            name: b.name.clone(),
            base_url: handle.base_url(),
            auth_token: if auth_token.is_empty() {
                None
            } else {
                Some(auth_token.to_string())
            },
            cloud_blocked,
            tier: b.tier,
            tool_scope: b.tool_scope.clone(),
            ready,
            n_ctx: handle.n_ctx(),
            slots: handle.slots(),
            in_flight: handle.in_flight(),
            handle: Handle::Remote(handle),
        })
    }

    /// Render the live capability description for the `offload_task` tool.
    /// Unlike the child's config-derived renderer, this reflects **live**
    /// backend readiness and **healthy** MCP servers.
    pub async fn describe(&self) -> String {
        let snap = self.settings.current().offload;
        let backends: Vec<OffloadBackend> = snap
            .effective_backends()
            .into_iter()
            .filter(|b| b.enabled)
            .collect();
        if backends.is_empty() {
            return "Delegate a token-heavy subtask to a local model to conserve this session's \
                    context. (No offload backend is configured/enabled — set one up in ccImp \
                    Settings → Offload.)"
                .to_string();
        }

        let pool = self.resolve_pool(&snap).await;
        let ready_names: Vec<&str> = pool
            .iter()
            .filter(|p| p.ready)
            .map(|p| p.name.as_str())
            .collect();
        let healthy_servers = self.host.healthy_names().await;

        let parts: Vec<String> = backends
            .iter()
            .map(|b| {
                let live = if ready_names.contains(&b.name.as_str()) {
                    "ready"
                } else if b.cloud_blocked() {
                    "needs consent"
                } else {
                    "down"
                };
                format!("{} [{live}]", b.name)
            })
            .collect();

        let tools_note = if healthy_servers.is_empty() {
            "native tools only".to_string()
        } else {
            format!("tool servers up: {}", healthy_servers.join(", "))
        };

        format!(
            "Delegate a token-heavy subtask (broad codebase search, large-file/log summarization, \
             web research) to a local/remote model to conserve this session's context. Pass a \
             self-contained instruction; you get back only the synthesized result. Backends: {}. \
             {tools_note}. Pass `tier` (fast|quality) to bias the choice; local-file tasks run on \
             a backend with file access (never a cloud backend).",
            parts.join("; ")
        )
    }

    /// Tear down the warm pool (app exit / disable).
    pub async fn shutdown(&self) {
        self.host.shutdown().await;
    }

    /// Spawn a lightweight health watcher: periodically re-resolves the pool
    /// and fires a capability-change pulse when the ready-set changes, so
    /// `/events` (and thus Claude's `tools/list_changed`) tracks a backend
    /// going down/up — not just MCP-server membership.
    pub fn spawn_health_watch(self: &Arc<Self>) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut last: Vec<String> = Vec::new();
            loop {
                tokio::time::sleep(Duration::from_secs(12)).await;
                let snap = this.settings.current().offload;
                if !snap.enabled {
                    last.clear();
                    continue;
                }
                let pool = this.resolve_pool(&snap).await;
                let mut ready: Vec<String> =
                    pool.iter().filter(|p| p.ready).map(|p| p.name.clone()).collect();
                ready.sort();
                if ready != last {
                    last = ready;
                    this.signal_change();
                }
            }
        });
    }
}

/// Stable signature of a cached remote handle for reuse comparison.
fn remote_sig(h: &RemoteBackend, is_cloud: bool) -> String {
    format!("{}|{}|{is_cloud}", h.base_url(), h.auth_token().is_none())
}

/// The offload `allowed_roots`, falling back to the launch project root when
/// empty (matching [`ToolCtx::new`]).
fn effective_roots(snap: &OffloadSettings) -> Vec<PathBuf> {
    if snap.allowed_roots.is_empty() {
        vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
    } else {
        snap.allowed_roots.clone()
    }
}

/// Compute the global concurrency cap: the explicit override, else the
/// summed per-backend slot counts, clamped to `[1, GLOBAL_CONCURRENCY_MAX]`.
fn compute_global_cap(snap: &OffloadSettings) -> u32 {
    if let Some(n) = snap.global_concurrency {
        return n.clamp(1, GLOBAL_CONCURRENCY_MAX);
    }
    let sum: u32 = snap
        .effective_backends()
        .iter()
        .filter(|b| b.enabled)
        .map(|b| match &b.kind {
            OffloadBackendKind::Local { server_command, .. } => ServerCommand::parse(server_command)
                .map(|c| c.parallel.max(1))
                .unwrap_or(1),
            OffloadBackendKind::Remote { .. } => 1,
        })
        .sum();
    sum.clamp(1, GLOBAL_CONCURRENCY_MAX)
}

/// Whether an agent-loop error looks like a transport/connection failure (so
/// a fail-over re-route is worth trying). Mirrors the child's heuristic.
fn is_connection_error(e: &str) -> bool {
    let e = e.to_lowercase();
    e.contains("chat request failed")
        || e.contains("connection")
        || e.contains("timed out")
        || e.contains("timeout")
        || e.contains("refused")
        || e.contains("/props request failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{OffloadBackend, OffloadBackendKind};

    fn local_backend(name: &str, np: u32) -> OffloadBackend {
        OffloadBackend {
            name: name.into(),
            enabled: true,
            kind: OffloadBackendKind::Local {
                server_command: format!("llama-server --jinja -np {np}"),
                autostart: false,
            },
            ..Default::default()
        }
    }

    #[test]
    fn global_cap_sums_slots_and_clamps() {
        let mut snap = OffloadSettings::default();
        snap.backends = vec![local_backend("a", 4), local_backend("b", 2)];
        assert_eq!(compute_global_cap(&snap), 6);

        snap.global_concurrency = Some(100);
        assert_eq!(compute_global_cap(&snap), GLOBAL_CONCURRENCY_MAX);

        snap.global_concurrency = Some(0);
        assert_eq!(compute_global_cap(&snap), 1);
    }

    #[test]
    fn global_cap_defaults_to_one_when_empty() {
        let snap = OffloadSettings::default(); // no backends, empty command
        assert_eq!(compute_global_cap(&snap), 1);
    }

    #[test]
    fn connection_error_classification() {
        assert!(is_connection_error("chat request failed: connection refused"));
        assert!(is_connection_error("request timed out"));
        assert!(!is_connection_error("server returned no choices"));
    }
}
