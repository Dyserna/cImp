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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, Mutex as TokioMutex, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::settings::{
    BackendTier, OffloadBackend, OffloadBackendKind, OffloadSettings, SettingsHandle, ToolScope,
};

use super::agent::{self, AgentConfig, HostRouter, OffloadTask, RunTrace, ThinkingMode};
use super::mcp_host::{host_config_sig, Consumer, McpHost, McpServerHealth};
use super::metrics::{BackendDashboard, CallRecord, MetricsPoller, RunRecord, ServerMetrics};
use super::remote::RemoteBackend;
use super::router::{self, BackendView, RouteError, TierHint};
use super::server::{LlamaServer, ServerCommand};
use super::supervisor::OffloadSupervisor;
use super::tools::{self, ToolCtx};
use super::Backend;

/// Per-backend cap on retained offload run records (newest first).
const RUN_LOG_CAP: usize = 30;

/// Ceiling on the auto-sized global gate so a wildly-configured pool can't
/// open thousands of concurrent loops.
const GLOBAL_CONCURRENCY_MAX: u32 = 32;

/// Aggregate offload-service status surfaced to Settings: the honest global
/// in-flight count (the warm-pool fix) and per-MCP-server health rows.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ServiceStatus {
    pub global_in_flight: u32,
    pub global_cap: u32,
    /// Tasks waiting for a slot (queue depth) right now.
    pub queue_depth: u32,
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
    /// Whether this backend is remote (LAN or cloud). Gates whether the
    /// offload worker may use the local-data graph tools when running here.
    is_remote: bool,
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
    /// Current global cap. Atomic because it's reconciled at runtime when the
    /// user changes backends / `global_concurrency` (see `reconcile_global_cap`),
    /// alongside resizing `global_gate`'s permits.
    global_cap: AtomicU32,
    /// Tasks currently *waiting* on the global gate (entered `run` but not yet
    /// holding a permit). Drives the `max_queue_depth` fast-reject and the
    /// live "N queued" dashboard readout. Incremented right before the gate
    /// acquire and decremented the instant it resolves (slot, timeout, or
    /// closed), so it counts only genuine waiters, never running tasks.
    queue_depth: AtomicU32,
    /// Warm remote backend handles, keyed by backend name (so `in_flight`
    /// persists across calls). Rebuilt when a backend's config changes.
    remote_pool: TokioMutex<HashMap<String, Arc<RemoteBackend>>>,
    /// Transient local handles for servers NOT managed by our supervisor
    /// (e.g. a llama-server the user launched themselves), keyed by backend
    /// name. Cached for the same reason as `remote_pool`: a fresh handle per
    /// call resets its slot gate, so `in_flight` would always read 0 and the
    /// router would never see the backend as busy. Rebuilt when the base URL
    /// changes.
    local_pool: TokioMutex<HashMap<String, Arc<LlamaServer>>>,
    /// Long-timeout client for the agent loop's chat-completions calls
    /// (health/`/props` probes use each handle's own short-timeout client).
    client: reqwest::Client,
    /// Capability-change pulses relayed to the loopback `/events` stream.
    change_tx: broadcast::Sender<()>,
    /// For emitting the `offload-server-metrics` dashboard event.
    app: AppHandle,
    /// Latest per-backend dashboard snapshot (initial fill for the IPC; the
    /// poller pushes live ones via the event). One row per enabled backend,
    /// Local first then Remote.
    latest_metrics: StdMutex<Vec<BackendDashboard>>,
    /// Serializes global-cap reconciliation so a concurrent grow + shrink
    /// can't interleave and leave the gate sized inconsistently with
    /// `global_cap` (see `reconcile_global_cap`).
    cap_reconcile_lock: TokioMutex<()>,
    /// Serializes MCP-host reconciliation. `warm_host` is now driven from
    /// several places — before each run, the 12s health watch, and the live
    /// `offload_reload_mcp` IPC — so without this two concurrent reconciles
    /// could both observe a newly-added server as missing and connect it
    /// twice. Held across the whole `host.reconcile` call.
    host_reconcile_lock: TokioMutex<()>,
    /// Signature of the last config `warm_host` reconciled against. Lets the
    /// per-run and health-watch `warm_host` calls skip the reconcile (and its
    /// lock hold) when nothing changed, so an unreachable server's connect
    /// attempt is paid once on the actual edit, not on every offload. Only
    /// touched while holding `host_reconcile_lock`.
    last_host_sig: StdMutex<Option<String>>,
    /// Set when a cap reconcile is wanted. Lets a single in-flight reconcile
    /// task absorb concurrent triggers (instead of one task piling up per
    /// trigger behind the lock during a slow shrink drain) and still converge
    /// to the latest config.
    reconcile_pending: AtomicBool,
    /// Per-backend offload run log (one `RunRecord` per `offload_task`, newest
    /// first, each grouping its LLM calls). Written as runs begin/finish and
    /// read by the dashboard emit loop, which stamps it onto each backend's
    /// metrics snapshot. Capped per backend.
    run_log: StdMutex<HashMap<String, VecDeque<RunRecord>>>,
    /// Monotonic id source for run records (deterministic; no wall-clock/RNG).
    run_id_seq: AtomicU64,
}

/// RAII guard for the `queue_depth` waiter counter. Increments on `enter` and
/// decrements on `Drop`, so the count is corrected on *every* exit from the
/// gate-acquire await — including a cancelled/dropped `run` future (a client
/// disconnect or aborted `offload_task`), which a bare `fetch_sub` after the
/// await would leak, permanently inflating `queued()` and wedging the
/// `max_queue_depth` fast-reject.
struct QueueGuard<'a>(&'a AtomicU32);

impl<'a> QueueGuard<'a> {
    fn enter(counter: &'a AtomicU32) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        QueueGuard(counter)
    }
}

impl Drop for QueueGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl OffloadService {
    /// Construct the service. Sizes the global gate from config (or the
    /// explicit `global_concurrency` override) and wires the MCP host's
    /// change channel into the service's own.
    pub fn new(
        app: AppHandle,
        settings: SettingsHandle,
        supervisor: Arc<OffloadSupervisor>,
    ) -> Arc<Self> {
        let snap = settings.current().offload;
        let global_cap = compute_global_cap(&snap);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(snap.offload_timeout_secs.max(30)))
            // Bound connection establishment separately from the overall
            // request budget so a dead endpoint fails fast instead of hanging
            // for the whole offload timeout.
            .connect_timeout(Duration::from_secs(5))
            // Don't reuse idle keep-alive connections. Against a local
            // single-slot llama-server the pool buys almost nothing, and a
            // socket the server closed between requests gets reused and fails
            // the next send as "error sending request for url …". Opening a
            // fresh connection per request removes that whole failure mode.
            .pool_max_idle_per_host(0)
            .build()
            .unwrap_or_default();
        let host = McpHost::new();
        let (change_tx, _) = broadcast::channel(16);

        let service = Arc::new(Self {
            settings,
            supervisor,
            host,
            global_gate: Arc::new(Semaphore::new(global_cap as usize)),
            global_cap: AtomicU32::new(global_cap),
            queue_depth: AtomicU32::new(0),
            remote_pool: TokioMutex::new(HashMap::new()),
            local_pool: TokioMutex::new(HashMap::new()),
            client,
            change_tx,
            app,
            latest_metrics: StdMutex::new(Vec::new()),
            cap_reconcile_lock: TokioMutex::new(()),
            host_reconcile_lock: TokioMutex::new(()),
            last_host_sig: StdMutex::new(None),
            reconcile_pending: AtomicBool::new(false),
            run_log: StdMutex::new(HashMap::new()),
            run_id_seq: AtomicU64::new(1),
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
            .load(Ordering::Relaxed)
            .saturating_sub(self.global_gate.available_permits() as u32)
    }

    /// Tasks currently waiting for a slot (queue depth) across the app.
    pub fn queued(&self) -> u32 {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Bring `global_gate`/`global_cap` in line with current config. The gate
    /// is sized once at construction, but the user can add/remove backends or
    /// change `global_concurrency` at runtime; without this the app-wide cap
    /// stays frozen at the startup value until restart. Grows by adding
    /// permits; shrinks by acquiring+forgetting the excess as it frees.
    fn reconcile_global_cap(self: &Arc<Self>, snap: &OffloadSettings) {
        // Cheap no-op guard so the common "nothing changed" call doesn't spawn
        // a task.
        if compute_global_cap(snap) == self.global_cap.load(Ordering::Relaxed) {
            return;
        }
        // Flag the work and ensure exactly ONE reconcile task runs. A concurrent
        // caller that finds the lock held just leaves `reconcile_pending` set;
        // the running task re-checks it and loops. Without this, the health
        // watch (every 12s) would spawn a fresh task on each tick during a slow
        // shrink drain (up to offload_timeout_secs), piling up behind the lock
        // and re-draining redundantly.
        self.reconcile_pending.store(true, Ordering::Relaxed);
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            // try_lock, not lock: if a reconcile is already running it will see
            // `reconcile_pending` and converge, so we don't queue another.
            let _guard = match this.cap_reconcile_lock.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            loop {
                // Claim the pending work before reading the target, so a change
                // that lands after this point re-sets the flag and loops us.
                this.reconcile_pending.store(false, Ordering::Relaxed);
                // Recompute from LIVE settings each pass — a config change during
                // a long drain is then honored without waiting for the next tick.
                let target = compute_global_cap(&this.settings.current().offload);
                let current = this.global_cap.load(Ordering::Relaxed);
                if target > current {
                    this.global_gate.add_permits((target - current) as usize);
                    // Publish only after the permits exist (avoid transient
                    // over-report of global_in_flight = cap - available).
                    this.global_cap.store(target, Ordering::Relaxed);
                    tracing::debug!(from = current, to = target, "offload: resized global gate");
                } else if target < current {
                    // Reclaim the excess permits as in-flight tasks release them.
                    let remove = (current - target) as usize;
                    for _ in 0..remove {
                        match this.global_gate.clone().acquire_owned().await {
                            Ok(p) => {
                                p.forget();
                                // Decrement the published cap in lock-step with
                                // each reclaimed permit so global_in_flight
                                // (cap - available) stays accurate throughout
                                // the drain — neither saturating to 0 (store
                                // target up front) nor over-reporting by the
                                // permits still being reclaimed (store at the end).
                                this.global_cap.fetch_sub(1, Ordering::Relaxed);
                            }
                            Err(_) => break, // semaphore closed
                        }
                    }
                    // Land exactly on target even if the semaphore closed mid-drain.
                    this.global_cap.store(target, Ordering::Relaxed);
                    tracing::debug!(from = current, to = target, "offload: resized global gate");
                }
                // Exit only when nothing new arrived while we worked.
                if !this.reconcile_pending.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
    }

    /// Bring the warm MCP host in line with current config. Cheap when the
    /// pool is already warm; called before each run and at startup.
    pub async fn warm_host(&self) {
        // Serialize against other reconcile callers (pre-run, health watch,
        // live reload IPC) so a freshly-added server isn't connected twice.
        let _guard = self.host_reconcile_lock.lock().await;
        let snap = self.settings.current().offload;
        // Keep the host up when offload is enabled OR a server is exposed to
        // Claude Code (Claude reaches it over the loopback independent of
        // offload). Only tear it down when neither consumer needs it.
        if !snap.mcp_host_needed() {
            self.host.shutdown().await;
            *self.last_host_sig.lock().unwrap() = None;
            return;
        }
        let roots = effective_roots(&snap);
        // Skip the reconcile (and its connect attempts) when the desired host
        // config is unchanged since the last one — the common case on the
        // per-run hot path and the 12s health watch.
        let sig = host_config_sig(&snap.mcp_servers, &roots);
        if self.last_host_sig.lock().unwrap().as_deref() == Some(sig.as_str()) {
            return;
        }
        self.host.reconcile(&snap.mcp_servers, &roots).await;
        *self.last_host_sig.lock().unwrap() = Some(sig);
    }

    /// Aggregate service status for the Settings readout: the honest global
    /// in-flight count and per-MCP-server health.
    pub async fn status(&self) -> ServiceStatus {
        ServiceStatus {
            global_in_flight: self.global_in_flight(),
            global_cap: self.global_cap.load(Ordering::Relaxed),
            queue_depth: self.queued(),
            mcp_servers: self.host.health().await,
        }
    }

    /// A proxied consumer's MCP tools as MCP `tools/list` descriptors
    /// (`{name, description, inputSchema}`), for the per-session child to merge
    /// into the tools it advertises. Empty when no server is exposed to that
    /// consumer. V19: parameterized by `Consumer` so the same loopback serves
    /// Claude and OpenCode children from their respective access flags.
    pub async fn mcp_tool_descriptors(&self, consumer: Consumer) -> Vec<serde_json::Value> {
        let defs = match consumer {
            Consumer::Opencode => self.host.tool_defs_for_opencode().await,
            // This loopback proxy only serves the interactive Claude/OpenCode
            // children; the offload worker reaches the host in-process, never via
            // this route. An unexpected `offload` query value therefore can't
            // arrive in practice — fall back to the Claude set (the conservative,
            // claude_access-guarded default) rather than leaking the offload set.
            Consumer::Claude | Consumer::Offload => self.host.tool_defs_for_claude().await,
        };
        defs.into_iter()
            .map(|d| {
                serde_json::json!({
                    "name": d.function.name,
                    "description": d.function.description,
                    "inputSchema": d.function.parameters,
                })
            })
            .collect()
    }

    /// Execute one proxied-consumer MCP tool call. Guarded — refuses any tool
    /// not offered by a server exposed to `consumer`.
    pub async fn mcp_call(
        &self,
        consumer: Consumer,
        name: &str,
        args: serde_json::Value,
    ) -> Result<String, String> {
        match consumer {
            Consumer::Opencode => self.host.call_for_opencode(name, args).await,
            // See `mcp_tool_descriptors`: `offload` never legitimately reaches
            // this proxy; fall back to the Claude-guarded set.
            Consumer::Claude | Consumer::Offload => self.host.call_for_claude(name, args).await,
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
        // The working directory of the *calling session* (the repo Claude Code
        // is running in), forwarded by the MCP child over the loopback. Used as
        // the native-tool root when no explicit `allowed_roots` is configured,
        // so an offload from a session in repo A reads repo A — not the app's
        // own launch directory. `None` falls back to the app's cwd.
        session_cwd: Option<PathBuf>,
        // Trips when the calling session goes away (loopback disconnect) so the
        // in-flight chat stream is dropped and llama-server frees the slot
        // instead of running an orphan to completion.
        cancel: CancellationToken,
    ) -> AppResult<String> {
        let snap = self.settings.current().offload;
        if !snap.enabled {
            return Err(AppError::OffloadNotReady(
                "offload is disabled — enable it in cImp settings".into(),
            ));
        }
        let timeout = Duration::from_secs(snap.offload_timeout_secs.max(30));
        let overall_deadline = Instant::now() + timeout;

        // Keep the warm tool-server pool current before routing.
        self.warm_host().await;

        // Optional fast-reject backpressure: when the pool is saturated (no
        // free permit) and the configured number of tasks are already waiting,
        // refuse immediately instead of stacking another long wait. `None`
        // keeps the old unbounded blocking-queue behavior. The check is
        // lock-free, so it's a *soft* cap — a burst of simultaneous arrivals
        // can overshoot `max` by the burst size before any of them increment
        // the counter; that's acceptable for backpressure. Snapshot `queued()`
        // once so the condition and the error message can't disagree.
        if let Some(max) = snap.max_queue_depth {
            let waiting = self.queued();
            if self.global_gate.available_permits() == 0 && waiting >= max {
                return Err(AppError::OffloadNotReady(format!(
                    "offload queue full — {} task(s) already waiting on {} busy slot(s); try again shortly",
                    waiting,
                    self.global_cap.load(Ordering::Relaxed)
                )));
            }
        }

        // Global gate first: bound total in-flight across the whole app.
        // Count as a waiter while blocked so `queued()` (and the fast-reject
        // above) see genuine backpressure. The guard decrements on drop —
        // when the acquire resolves *and* if this future is cancelled mid-wait
        // (client disconnect / aborted offload) — so the counter can't leak.
        let queue_guard = QueueGuard::enter(&self.queue_depth);
        let acquired =
            tokio::time::timeout(timeout, self.global_gate.clone().acquire_owned()).await;
        drop(queue_guard);
        let _global = match acquired {
            Ok(Ok(p)) => p,
            Ok(Err(_)) => return Err(AppError::Offload("global offload gate closed".into())),
            Err(_) => {
                return Err(AppError::OffloadNotReady(format!(
                    "all {} global offload slots busy — timed out after {}s",
                    self.global_cap.load(Ordering::Relaxed),
                    timeout.as_secs()
                )))
            }
        };

        // Resolve + probe the pool from live state. This is the one path that
        // may lazy-start a cold Local backend (start-on-first-offload).
        let pool = self.resolve_pool(&snap, true).await;
        if pool.is_empty() {
            return Err(AppError::OffloadNotReady(
                "no offload backend is configured — add one in cImp Settings → Offload".into(),
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

        // Open a run-log record for this offload (one row per `offload_task`,
        // grouping every LLM call) so the dashboard can show it live as
        // `running` and color it on completion.
        let mut backend_name = views[chosen].name.clone();
        // Index of the backend actually serving the run; advances on fail-over so
        // the On→Auto retry below targets the live backend, not the failed one.
        let mut active = chosen;
        let run_id = self.run_id_seq.fetch_add(1, Ordering::Relaxed);
        let run_started = now_ms();
        self.run_begin(&backend_name, run_id, &instructions, thinking);
        let mut trace = RunTrace::default();

        // First attempt with the requested thinking mode.
        let first = self
            .run_on(&pool[chosen], &views[chosen], &snap, &instructions, context.clone(), thinking, session_cwd.clone(), overall_deadline, Some(&mut trace), &cancel)
            .await;

        // One fail-over on a connection-class failure: drop the failed
        // backend and re-select among the rest.
        let mut result = match first {
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
                        // Move the run record to the backend that will actually
                        // run it, so it's not mis-grouped under the failed one.
                        let new_name = pool[next].name.clone();
                        self.run_rekey(&backend_name, &new_name, run_id);
                        backend_name = new_name;
                        active = next;
                        self.run_on(&pool[next], &views[next], &snap, &instructions, context.clone(), thinking, session_cwd.clone(), overall_deadline, Some(&mut trace), &cancel)
                            .await
                    }
                    _ => Err(e),
                }
            }
            Err(e) => Err(e),
        };

        // On→Auto retry: a `thinking:on` run that produced no usable answer
        // (the model spent its output budget thinking) gets ONE more shot with
        // `auto` — thinking only on the plan + final, quiet on ingestion, which
        // is what avoids the runaway. Not retried with `off`: thinking was
        // explicitly wanted. If auto also fails, the run is marked failed.
        let mut recovered = false;
        if matches!(result, Err(AppError::OffloadNoAnswer(_))) && thinking == ThinkingMode::On {
            warn!(
                target: "offload",
                run_id,
                "offload: thinking:on produced no answer; retrying once with auto"
            );
            // Retry on the backend that actually served the first attempt
            // (`active` — fail-over may have advanced it past `chosen`), not the
            // originally-selected one, which may be the backend that just failed.
            let retry_deadline = Instant::now() + timeout;
            let pre_retry = trace.calls.len();
            result = self
                .run_on(&pool[active], &views[active], &snap, &instructions, context.clone(), ThinkingMode::Auto, session_cwd.clone(), retry_deadline, Some(&mut trace), &cancel)
                .await;
            recovered = result.is_ok();
            // The retry's agent loop numbers its calls from step 0 again; offset
            // them so they continue after the first attempt's steps instead of
            // colliding with them in the run log.
            let base = trace.calls[..pre_retry]
                .iter()
                .map(|c| c.step)
                .max()
                .map_or(0, |m| m + 1);
            for c in trace.calls[pre_retry..].iter_mut() {
                c.step += base;
            }
        }

        // Close the run-log record: color by outcome (failed = red, recovered
        // = amber, success = normal).
        let outcome = match (&result, recovered) {
            (Ok(_), true) => "recovered",
            (Ok(_), false) => "success",
            (Err(_), _) => "failed",
        };
        self.run_finish(&backend_name, run_id, outcome, std::mem::take(&mut trace.calls));

        // Mirror the completed run into the unified, persistent tool-activity
        // store (the Tool Activity tab's feed): full instruction text (+ any
        // caller-supplied context) as the request, the synthesized answer (or
        // the error) as the response.
        let request = match context.as_deref() {
            Some(ctx) if !ctx.is_empty() => {
                format!("{instructions}\n\n--- context ---\n{ctx}")
            }
            _ => instructions.clone(),
        };
        let response = match &result {
            Ok(text) => text.clone(),
            Err(e) => format!("[error] {e}"),
        };
        crate::activity::record_bg(crate::activity::ActivityRecord {
            entry: crate::activity::ActivityEntry::new(
                crate::activity::ActivityKind::Offload,
                run_started,
                session_cwd
                    .as_deref()
                    .map(crate::activity::root_key)
                    .unwrap_or_default(),
                backend_name.clone(),
                "offload_task".to_string(),
                instruction_headline(&instructions),
                result.as_ref().map(|t| t.chars().count()).unwrap_or(0),
                now_ms().saturating_sub(run_started),
                result.is_ok(),
            ),
            request,
            response,
        });
        result
    }

    /// Insert a `running` run record at the front of a backend's run log.
    fn run_begin(&self, backend: &str, id: u64, instructions: &str, thinking: ThinkingMode) {
        let rec = RunRecord {
            id,
            instructions: instruction_headline(instructions),
            thinking: thinking_label(thinking).into(),
            started_ms: now_ms(),
            ended_ms: 0,
            outcome: "running".into(),
            calls: Vec::new(),
        };
        let mut log = self.run_log.lock().unwrap();
        let dq = log.entry(backend.to_string()).or_default();
        dq.push_front(rec);
        while dq.len() > RUN_LOG_CAP {
            dq.pop_back();
        }
    }

    /// Move a run record to another backend after a fail-over re-route, so it's
    /// grouped under the backend that actually ran it (not the one that failed).
    fn run_rekey(&self, old: &str, new: &str, id: u64) {
        if old == new {
            return;
        }
        let mut log = self.run_log.lock().unwrap();
        let rec = log
            .get_mut(old)
            .and_then(|dq| dq.iter().position(|r| r.id == id).and_then(|i| dq.remove(i)));
        if let Some(rec) = rec {
            let dq = log.entry(new.to_string()).or_default();
            dq.push_front(rec);
            while dq.len() > RUN_LOG_CAP {
                dq.pop_back();
            }
        }
    }

    /// Finalize a run record with its captured calls + final outcome.
    fn run_finish(&self, backend: &str, id: u64, outcome: &str, calls: Vec<CallRecord>) {
        let mut log = self.run_log.lock().unwrap();
        if let Some(dq) = log.get_mut(backend) {
            if let Some(rec) = dq.iter_mut().find(|r| r.id == id) {
                rec.calls = calls;
                rec.outcome = outcome.into();
                rec.ended_ms = now_ms();
            }
        }
    }

    /// Snapshot **every** backend's run log (each newest first) under a single
    /// lock acquisition. Taking it once — rather than once per row — means a
    /// concurrent fail-over re-key can't relocate a run between two per-row reads
    /// and make it appear under two backends in the same dashboard emit.
    fn run_logs_snapshot(&self) -> HashMap<String, Vec<RunRecord>> {
        self.run_log
            .lock()
            .unwrap()
            .iter()
            .map(|(k, dq)| (k.clone(), dq.iter().cloned().collect()))
            .collect()
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
        session_cwd: Option<PathBuf>,
        deadline: Instant,
        trace: Option<&mut RunTrace>,
        cancel: &CancellationToken,
    ) -> AppResult<String> {
        let slot_timeout = deadline.saturating_duration_since(Instant::now());
        let _slot = entry.handle.acquire_slot(slot_timeout).await?;

        // Prefer the calling session's cwd (forwarded over the loopback) so
        // tool resolution and the empty-`allowed_roots` fallback target the
        // repo Claude is actually in, not the app's launch dir.
        let cwd = session_cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let roots = if snap.allowed_roots.is_empty() {
            vec![cwd.clone()]
        } else {
            snap.allowed_roots.clone()
        };
        let ctx = ToolCtx::new(
            roots,
            snap.command_allowlist.clone(),
            snap.command_policies.clone(),
            &cwd,
        );
        let mut native_defs = tools::enabled_defs(&snap.tools);
        // V9-01: offer the graph tools to the worker when the feature is on AND
        // either this backend is local or the user opted a remote backend in
        // (a remote — LAN or cloud — would receive the project's code
        // structure). `allow_graph` re-gates dispatch as defense-in-depth.
        let graph = self.settings.current().graph;
        let allow_graph = worker_graph_allowed(
            graph.enabled,
            entry.is_remote,
            graph.allow_remote_worker_access,
        );
        if allow_graph {
            native_defs.extend(tools::graph_tools::defs());
        }
        let mcp_defs = self.host.tool_defs_for_offload().await;
        let router = HostRouter::new(
            native_defs,
            mcp_defs,
            ctx,
            self.host.clone(),
            entry.tool_scope.clone(),
            allow_graph,
        );
        let cfg = AgentConfig {
            base_url: entry.base_url.clone(),
            model: None,
            max_steps: snap.max_steps.max(1),
            budget_tokens: view.per_slot_budget(),
            n_ctx: view.n_ctx,
            slots: view.slots,
            per_tool_result_token_cap: snap.per_tool_result_token_cap.max(256),
            auth_token: entry.auth_token.clone(),
            per_call_timeout: Duration::from_secs(snap.offload_timeout_secs.max(30)),
        };
        let task = OffloadTask {
            instructions: instructions.to_string(),
            context,
            thinking,
        };
        agent::run(&self.client, &cfg, &router, task, deadline, trace, cancel).await
    }

    /// Resolve the enabled backend pool from live state: live local handles
    /// from the supervisor (real `in_flight`/`n_ctx`), warm remote handles
    /// from the cache (health-refreshed). Probes concurrently.
    ///
    /// `lazy_start` gates the "warm a not-yet-running Local backend" spawn — it
    /// is `true` only for an actual offload [`run`](Self::run) (the documented
    /// "start on first offload" behavior when autostart is off), and `false`
    /// for [`describe`](Self::describe) and the health watcher, which must
    /// never load a multi-GB model just to report capabilities or poll health.
    async fn resolve_pool(&self, snap: &OffloadSettings, lazy_start: bool) -> Vec<PoolEntry> {
        let backends: Vec<OffloadBackend> = snap
            .effective_backends()
            .into_iter()
            .filter(|b| b.enabled)
            .collect();

        let mut entries = Vec::with_capacity(backends.len());
        for b in &backends {
            match &b.kind {
                OffloadBackendKind::Local { server_command, .. } => {
                    if let Some(e) = self.resolve_local(b, server_command, lazy_start).await {
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

        // Prune cached handles for backends that no longer exist (renamed,
        // removed, or disabled) so the pools don't grow unbounded across config
        // edits — mirroring the metrics poller's `retain`. Each lock is taken
        // and dropped separately, so there's no nesting/ordering hazard.
        let live: std::collections::HashSet<&str> =
            backends.iter().map(|b| b.name.as_str()).collect();
        self.local_pool.lock().await.retain(|k, _| live.contains(k.as_str()));
        self.remote_pool.lock().await.retain(|k, _| live.contains(k.as_str()));

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
        lazy_start: bool,
    ) -> Option<PoolEntry> {
        if let Some(server) = self.supervisor.running_server(&b.name).await {
            // Refresh health/props on the warm handle. Checked before parsing
            // the configured command so a server launched via the Start
            // popup's command override (possibly on a different host/port, or
            // with the configured command since edited) is still routed to at
            // the URL it actually listens on.
            let ready = server.health_check().await;
            if ready {
                let _ = server.refresh_props().await;
            }
            return Some(PoolEntry {
                name: b.name.clone(),
                base_url: server.base_url(),
                auth_token: None,
                cloud_blocked: false,
                tier: b.tier,
                tool_scope: b.tool_scope.clone(),
                ready,
                n_ctx: server.n_ctx().or(b.declared_context),
                slots: server.slots(),
                in_flight: server.in_flight(),
                is_remote: false,
                handle: Handle::Local(server),
            });
        }

        // Not running: everything below works off the configured command.
        if server_command.trim().is_empty() {
            return None;
        }
        let cmd = ServerCommand::parse(server_command).ok()?;
        let base_url = cmd.base_url();

        // On an actual offload, warm it for next time (the "start on first
        // offload" behavior); NEVER from describe()/health polling, so
        // listing capabilities or a background probe can't load a multi-GB
        // model the user didn't ask to start.
        if lazy_start {
            let supervisor = self.supervisor.clone();
            let name = b.name.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = supervisor.start_backend(&name, None).await {
                    debug!(backend = %name, error = %e, "offload: lazy start failed");
                }
            });
        }
        // Try a transient handle for a server the user may have launched
        // themselves (or one already warming from a prior offload). Cache it by
        // name (reused while the base URL is unchanged) so its slot gate — and
        // thus `in_flight` — persists across calls; a fresh handle each time
        // would always report 0 in-flight and the router would never throttle.
        let transient = {
            let mut pool = self.local_pool.lock().await;
            let reuse = pool.get(&b.name).filter(|h| h.base_url() == base_url);
            match reuse {
                Some(h) => h.clone(),
                None => {
                    let h = Arc::new(
                        LlamaServer::with_config(&b.name, server_command, b.tier, b.tool_scope.clone())
                            .ok()?,
                    );
                    pool.insert(b.name.clone(), h.clone());
                    h
                }
            }
        };
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
            is_remote: false,
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
        // Fallback slot count until `refresh_props` below discovers the
        // endpoint's real `total_slots` (a llama-server reports it; cloud
        // APIs don't, and keep this single slot).
        let slots_decl = 1u32;

        // Reuse a cached handle when the URL/auth/cloud signature matches; a
        // fresh handle would reset `in_flight`. The signature fingerprints the
        // actual token *value*, not just its presence — otherwise rotating a
        // bearer token from one non-empty value to another (a common cloud key
        // rotation) would match the cached signature and keep reusing the
        // handle built with the stale token (its reqwest client + health/props
        // probe would authenticate with the old credential).
        // Trim the trailing slash to match `remote_sig`, which fingerprints the
        // handle's already-trimmed `base_url()`. Without this, a configured URL
        // with a trailing slash never matches its cached handle's signature →
        // every resolve rebuilds the handle, resetting `in_flight` to 0 so the
        // router believes the backend is perpetually idle and never spills.
        let sig = format!(
            "{}|{}|{is_cloud}",
            base_url.trim_end_matches('/'),
            token_fp(auth_token)
        );
        let handle = {
            let mut pool = self.remote_pool.lock().await;
            let reuse = pool.get(&b.name).filter(|h| h.base_url() == base_url.trim_end_matches('/'));
            match reuse {
                // Don't reuse a handle flagged for rebuild (the remote came
                // back with fewer slots than its gate is sized for).
                Some(h) if remote_sig(h, is_cloud) == sig && !h.needs_rebuild() => h.clone(),
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
            is_remote: true,
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
                    context. (No offload backend is configured/enabled — set one up in cImp \
                    Settings → Offload.)"
                .to_string();
        }

        // describe() must never load a model — capability reporting only.
        let pool = self.resolve_pool(&snap, false).await;
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

        // Advertise parallelism + live capacity so Opus knows it can fan out
        // (issue several `offload_task` calls at once) and roughly how many run
        // concurrently before further calls queue. `global_cap` = summed
        // per-backend slots (or the `global_concurrency` override).
        let cap = self.global_cap.load(Ordering::Relaxed);
        let in_flight = self.global_in_flight();
        let parallel_note = format!(
            " You can run offloads in parallel: issue multiple offload_task calls at once to fan \
             out independent subtasks — up to {cap} run concurrently ({in_flight} busy now), and \
             further calls queue for a slot rather than failing."
        );

        format!(
            "Delegate a token-heavy subtask (broad codebase search, large-file/log summarization, \
             web research) to a local/remote model to conserve this session's context. Pass a \
             self-contained instruction; you get back only the synthesized result. Backends: {}. \
             {tools_note}. Pass `tier` (fast|quality) to bias the choice; local-file tasks run on \
             a backend with file access (never a cloud backend).{parallel_note}",
            parts.join("; ")
        )
    }

    /// Tear down the warm pool (app exit / disable).
    pub async fn shutdown(&self) {
        self.host.shutdown().await;
    }

    /// The latest per-backend dashboard snapshot (initial fill for the Offload
    /// Server tab; live updates arrive via the `offload-server-metrics`
    /// event). One row per enabled backend, Local first then Remote.
    pub fn server_metrics(&self) -> Vec<BackendDashboard> {
        self.latest_metrics.lock().unwrap().clone()
    }

    /// Build one dashboard row for a backend. Local owned servers and
    /// reachable LAN `llama-server`s get a live `/slots`+`/metrics` poll
    /// (history accumulates in the per-name [`MetricsPoller`]); cloud and
    /// unreachable backends get a status-only row carrying just their
    /// context/slot headline.
    async fn backend_dashboard(
        &self,
        b: &OffloadBackend,
        pollers: &mut HashMap<String, MetricsPoller>,
        in_flight: u32,
        cap: u32,
    ) -> BackendDashboard {
        match &b.kind {
            OffloadBackendKind::Local { .. } => {
                let (state, metrics) = match self.supervisor.running_server(&b.name).await {
                    Some(server) if server.is_ready() => {
                        let poller = pollers
                            .entry(b.name.clone())
                            .or_insert_with(MetricsPoller::new);
                        let m = poller
                            .poll(
                                &server.base_url(),
                                None,
                                server.slots(),
                                server.n_ctx(),
                                in_flight,
                                cap,
                            )
                            .await;
                        ("ready", m)
                    }
                    Some(_) => ("starting", ServerMetrics::offline(in_flight, cap)),
                    None => ("stopped", ServerMetrics::offline(in_flight, cap)),
                };
                BackendDashboard {
                    name: b.name.clone(),
                    kind: "local".into(),
                    state: state.into(),
                    metrics,
                }
            }
            OffloadBackendKind::Remote {
                base_url,
                auth_token,
                is_cloud,
                ..
            } => {
                let kind = if *is_cloud { "cloud" } else { "lan" };
                if b.cloud_blocked() {
                    return BackendDashboard {
                        name: b.name.clone(),
                        kind: kind.into(),
                        state: "blocked".into(),
                        metrics: ServerMetrics::offline(in_flight, cap),
                    };
                }
                let entry = self
                    .resolve_remote(b, base_url, auth_token, *is_cloud)
                    .await;
                let (state, metrics) = match entry {
                    Some(e) if e.ready && !*is_cloud => {
                        // A LAN llama-server — poll it live, just like Local.
                        let auth = e.auth_token.clone();
                        let poller = pollers
                            .entry(b.name.clone())
                            .or_insert_with(MetricsPoller::new);
                        let m = poller
                            .poll(&e.base_url, auth.as_deref(), e.slots, e.n_ctx, in_flight, cap)
                            .await;
                        ("ready", m)
                    }
                    // A reachable cloud endpoint (no `/slots`): status-only
                    // headline carrying its context/slot count.
                    Some(e) if e.ready => (
                        "ready",
                        ServerMetrics::status_only(e.slots, e.n_ctx, in_flight, cap),
                    ),
                    _ => ("unreachable", ServerMetrics::offline(in_flight, cap)),
                };
                BackendDashboard {
                    name: b.name.clone(),
                    kind: kind.into(),
                    state: state.into(),
                    metrics,
                }
            }
        }
    }

    /// Spawn the Offload Server dashboard poller: every ~600ms, build one
    /// dashboard row per enabled backend — Local owned servers and reachable
    /// LAN `llama-server`s are polled live (`/slots` + `/metrics`); cloud and
    /// down backends carry a status-only row — then cache the set and emit
    /// `offload-server-metrics`. A per-backend [`MetricsPoller`] (keyed by
    /// name) accumulates each one's tokens/sec + history independently.
    pub fn spawn_metrics_poller(self: &Arc<Self>) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut pollers: HashMap<String, MetricsPoller> = HashMap::new();
            loop {
                tokio::time::sleep(Duration::from_millis(600)).await;
                let snap = this.settings.current().offload;
                let in_flight = this.global_in_flight();
                let cap = this.global_cap.load(Ordering::Relaxed);

                let mut rows: Vec<BackendDashboard> = Vec::new();
                if snap.enabled {
                    for b in snap.effective_backends().into_iter().filter(|b| b.enabled) {
                        let row = this
                            .backend_dashboard(&b, &mut pollers, in_flight, cap)
                            .await;
                        rows.push(row);
                    }
                }

                // Drop pollers for backends that vanished from the config so
                // their history doesn't leak across a rename/removal.
                let live: std::collections::HashSet<&str> =
                    rows.iter().map(|r| r.name.as_str()).collect();
                pollers.retain(|k, _| live.contains(k.as_str()));

                // Stamp the app-wide queue depth onto every row (it's a global,
                // not per-backend, figure — the dashboard shows it once).
                let queued = this.queued();
                // One atomic snapshot of all backends' run logs (a fail-over
                // re-key between per-row reads could otherwise show one run under
                // two backends at once).
                let mut run_logs = this.run_logs_snapshot();
                for r in rows.iter_mut() {
                    r.metrics.queue_depth = queued;
                    // Attach this backend's offload run log (the poller tracks
                    // slot activity; the service owns task-level run outcomes).
                    r.metrics.runs = run_logs.remove(&r.name).unwrap_or_default();
                }

                *this.latest_metrics.lock().unwrap() = rows.clone();
                let _ = this.app.emit("offload-server-metrics", &rows);
            }
        });
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
                // Keep the app-wide gate sized to current config (backends or
                // global_concurrency may have changed since startup).
                this.reconcile_global_cap(&snap);
                // Keep the MCP host membership live as a safety net: the live
                // `offload_reload_mcp` IPC reconciles instantly on edit, but
                // this also catches changes made another way (e.g. a direct
                // settings.json edit) within one watch tick — no restart.
                this.warm_host().await;
                if !snap.enabled {
                    last.clear();
                    continue;
                }
                // Health polling only — must not lazy-start a cold backend.
                let pool = this.resolve_pool(&snap, false).await;
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
    format!("{}|{}|{is_cloud}", h.base_url(), token_fp(h.auth_token().unwrap_or("")))
}

/// Non-cryptographic fingerprint of an auth token, used only to detect a
/// change in the token *value* between settings snapshots so a rotated token
/// forces a handle rebuild. Not stored or logged — just compared in-process.
fn token_fp(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut h);
    h.finish()
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

/// Whether the offload worker may use the code-graph tools on the chosen
/// backend: the feature must be on, and either the backend is local or the
/// user opted remote workers in. A remote backend (LAN *or* cloud) would
/// receive the project's code structure, so it's denied by default — the cloud
/// Opus session and a local worker are unaffected.
fn worker_graph_allowed(graph_enabled: bool, is_remote: bool, allow_remote: bool) -> bool {
    graph_enabled && (!is_remote || allow_remote)
}

/// The 160-char instruction headline shared by the run log's row and the
/// activity feed's target column — one definition so the two can't drift.
fn instruction_headline(instructions: &str) -> String {
    instructions.chars().take(160).collect()
}

/// The run log's thinking-mode label (`"on"`/`"off"`/`"auto"`).
fn thinking_label(m: ThinkingMode) -> &'static str {
    match m {
        ThinkingMode::On => "on",
        ThinkingMode::Off => "off",
        ThinkingMode::Auto => "auto",
    }
}

/// Current wall-clock as epoch millis (formatted on the frontend).
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Whether an agent-loop error looks like a transport/connection failure (so
/// a fail-over re-route is worth trying). Mirrors the child's heuristic.
fn is_connection_error(e: &str) -> bool {
    let e = e.to_lowercase();
    e.contains("chat request failed")
        || e.contains("chat stream failed")
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
                show_command_on_start: false,
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

    #[test]
    fn worker_graph_gate_truth_table() {
        // Feature off → never.
        assert!(!worker_graph_allowed(false, false, false));
        assert!(!worker_graph_allowed(false, true, true));
        // Local backend → always when enabled, regardless of the remote opt-in.
        assert!(worker_graph_allowed(true, false, false));
        assert!(worker_graph_allowed(true, false, true));
        // Remote backend → only with the explicit opt-in.
        assert!(!worker_graph_allowed(true, true, false));
        assert!(worker_graph_allowed(true, true, true));
    }
}
