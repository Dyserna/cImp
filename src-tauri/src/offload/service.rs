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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::settings::{
    BackendTier, OffloadBackend, OffloadBackendKind, OffloadSettings, SettingsHandle, ToolScope,
};

use super::agent::{self, AgentConfig, HostRouter, OffloadTask, RunTrace, ThinkingMode};
use super::mcp_host::{
    host_config_sig, Consumer, McpHost, McpServerHealth, McpSurfaceFingerprint,
};
use super::metrics::{BackendDashboard, CallRecord, MetricsPoller, RunRecord, ServerMetrics};
use super::outbound;
use super::remote::RemoteBackend;
use super::router::{self, BackendView, RouteError, TierHint};
use super::server::{LlamaServer, ServerCommand};
use super::supervisor::OffloadSupervisor;
use super::toolclass::{Profile, PROFILE_TOOL_NOTE};
use super::tools::{self, ToolCtx};
use super::Backend;

/// V32 Phase G: the offload worker's injection scope (locked decision 16's
/// `offload-worker` pseudo-scope). A const so every worker-side resolution names
/// the same scope — the worker is a task-scoped service with no tab, and a
/// resolution that reached for either app-level scope instead
/// (`Scope::AppWide`, `Scope::UnknownCaller`) would silently ignore the worker's
/// own override row.
const WORKER: crate::settings::injection::Scope<'static> =
    crate::settings::injection::Scope::OffloadWorker;

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
    ///
    /// V37 C5: nothing sends on this directly any more. Every producer goes
    /// through `pulse_tx` and the ONE gate task ([`run_pulse_gate`]) owns this
    /// half, so debouncing and surface suppression cannot be bypassed by a new
    /// call site.
    change_tx: broadcast::Sender<()>,
    /// V37 C5: the intake side of the pulse gate. Unbounded because a dropped
    /// pulse is a surface that never propagates, and the gate collapses bursts
    /// anyway — the queue only ever holds one debounce window's worth.
    pulse_tx: mpsc::UnboundedSender<PulseSource>,
    /// V30 Phase B: the live per-tab `--offload-mcp` children, for addressed
    /// session pushes. Separate from `change_tx` on purpose — that broadcast
    /// stays the un-addressed capability pulse every subscriber gets.
    pushes: Arc<PushRegistry>,
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

// ── V30 Phase B — the tab-addressed push bus ─────────────────────────────────
//
// One app-side registry of the live `--offload-mcp` children (one per tab), so
// backend code can address a `<channel>` message at every armed session
// ([`PushRegistry::push_broadcast`]) or at one tab
// ([`PushRegistry::push_to_tab`]). The wire ride is the existing
// `GET /events` SSE stream (`loopback::handle_events`), which gains an
// `event: push` frame beside the unchanged `event: change` capability pulse;
// the app sends the SEMANTIC payload and the child owns the JSON-RPC framing.
//
// **Instance scoping is inherent — do NOT add cross-instance logic.** Every
// running cImp instance binds its own ephemeral loopback port with its own
// per-launch bearer token, and this registry hangs off *that* instance's
// `OffloadService`. A child can only appear here by connecting to THIS
// instance's `/events` with THIS instance's token, so the tab ids in this map
// are already this-instance-only — which is exactly milestone invariant 3
// ("pushes are instance-scoped"), satisfied by construction rather than by a
// pid/root match.

/// Per-subscriber pending-push capacity. Pushes are best-effort notify-only
/// (milestone invariant 2: every push has a pull twin), so a wedged or slow
/// child must never back-pressure a producer — a full queue drops the notice
/// with a warn instead.
const PUSH_QUEUE_CAP: usize = 32;

/// Whether a channel `meta` key satisfies the client's `^[a-zA-Z_][a-zA-Z0-9_]*$`
/// filter.
///
/// Claude Code **silently drops** keys that don't match (Phase 0 contract
/// summary), so a typo'd key would vanish with no signal anywhere. We validate
/// at the write boundary instead and warn — the repo principle "validate at the
/// parse boundary anyway", applied to the producing side.
pub fn valid_meta_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One semantic push payload: the `<channel>` body plus its attributes.
///
/// This is the wire type of the SSE `event: push` frame's `data` — serialized
/// by the app ([`crate::offload::loopback`]) and deserialized by the child
/// ([`crate::offload::mcp`]), so both halves share one definition and cannot
/// drift.
///
/// # The channel-content invariant (V32 locked decision 9), as a type
///
/// Push content may only ever carry **text this application composed itself**.
/// Never LLM output, never a scanner finding message, never fetched page
/// content, never a tool result. It is not "just another injection surface":
/// every other path by which untrusted text reaches a model is *pull* — the
/// model asked, and the answer lands inside a turn a user started. A push
/// delivers a `<channel source="cimp-offload">` message that **starts** a turn
/// on an idle session. Untrusted text there stops being ordinary indirect
/// injection and becomes autonomous, turn-starting injection.
///
/// `offload.session_push` is OFF by decision (2026-08-06) and the V30 code is
/// released but dormant, so nothing exercises these producers today — a future
/// one that started interpolating a tool result would break no test, produce no
/// symptom, and ship.
///
/// The invariant is therefore carried by this type's **shape** (#47) rather
/// than by a source scan over its call sites, which is what watched it until
/// then:
///
/// - [`content`](Self::content) is private and [`new`](Self::new) is its only
///   constructor. No struct literal, no `Default`, no `Deserialize` shortcut.
/// - `new` takes a `&'static str` **template** plus its runtime values and
///   interpolates them itself. A producer cannot hand it a `String` at all, so
///   `PushNotice::new(format!("…{answer}"), …)` — the failure mode decision 9
///   names — is a compile error rather than something a reviewer has to catch.
/// - Deserialization goes through [`PushNoticeWire`] and a validating
///   `TryFrom`, which rejects blank content and applies the same meta-key
///   contract as the constructor.
///
/// **Correction 2026-08-08 (#48).** The third bullet used to end "so the SSE
/// path cannot mint one the constructor would have refused". It can, and the
/// distinction is worth stating precisely because decision 9 is read off this
/// comment. `TryFrom<PushNoticeWire>` enforces exactly two things — non-blank
/// `content`, and `keep_valid_meta` over the attributes — and **nothing** about
/// the static-template property, because there is no `&'static str` anywhere on
/// that path: `serde_json::from_value(json!({"content": worker_answer}))`
/// parses for any non-blank string. So three of the four construction paths
/// (struct literal, `Default`, a composed `String` argument) are **compile
/// errors**, and the fourth is **validated**. What bounds it is the provenance
/// of the frames, not the shape of the value: `offload::mcp::channel_params` parses cImp's
/// own `GET /events` stream, fetched from the loopback with the per-launch
/// bearer token behind the same `authorized` check as every other route — i.e.
/// local same-user, the bound decision 3 already states for the whole loopback,
/// not "app-composed". Adequate while `offload.session_push` is off; not a
/// type-level invariant, and not to be described as one.
///
/// What the type does **not** decide is whether an individual interpolated
/// value is app-owned: `args` are runtime `&str`, and "this count came from our
/// own indexer" is a judgement about provenance, not a property of a type. The
/// sentence a push makes is pinned; the values in its slots are still a
/// reviewer's call. Keep them to counts, durations, configured paths and fixed
/// tool names.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "PushNoticeWire")]
pub struct PushNotice {
    /// The message text the model sees inside `<channel source="…">…</channel>`.
    /// Private: see the type docs.
    content: String,
    /// String attributes rendered onto the `<channel>` tag. Keys are guaranteed
    /// valid by [`PushNotice::new`] and re-checked on the deserialize path.
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
}

/// The on-the-wire shape of a [`PushNotice`], and the only door into one that
/// does not run [`PushNotice::new`].
///
/// It exists so the deserialize path has somewhere to be validated. The two
/// halves of the push wire are different processes and can be different builds
/// (a child outlives a settings change; an old exe can be talking to a new
/// app), so what arrives is checked here rather than assumed — the repo
/// principle "validate at the parse boundary anyway".
#[derive(serde::Deserialize)]
pub struct PushNoticeWire {
    #[serde(default)]
    content: String,
    #[serde(default)]
    meta: BTreeMap<String, String>,
}

/// Why a wire payload could not become a [`PushNotice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushNoticeError {
    /// Content was absent, empty, or whitespace only. "Empty is not absent": an
    /// empty `<channel>` message would cost the session a turn and say nothing,
    /// so a blank notice is rejected at the parse boundary rather than
    /// delivered and ignored somewhere downstream.
    EmptyContent,
}

impl std::fmt::Display for PushNoticeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushNoticeError::EmptyContent => f.write_str("push notice carries no content"),
        }
    }
}

impl TryFrom<PushNoticeWire> for PushNotice {
    type Error = PushNoticeError;

    fn try_from(wire: PushNoticeWire) -> Result<Self, Self::Error> {
        if wire.content.trim().is_empty() {
            return Err(PushNoticeError::EmptyContent);
        }
        Ok(Self {
            content: wire.content,
            meta: keep_valid_meta(wire.meta),
        })
    }
}

/// The slot [`PushNotice::new`] fills from `args`, left to right.
const PUSH_SLOT: &str = "{}";

impl PushNotice {
    /// Build a notice from a **static template** and the runtime values that go
    /// in its slots, dropping (with a warn) any meta key the client would
    /// silently discard.
    ///
    /// `template` is `&'static str` and the interpolation happens in here: that
    /// is the whole enforcement mechanism for locked decision 9 (see the type
    /// docs). Each `{}` is replaced by the corresponding entry of `args`, in
    /// order.
    ///
    /// **A count mismatch is a producer bug, and it now has a consumer (#48).**
    /// In release it still warns and fills what it can, because a malformed
    /// notice must not take down the scan or index that was only announcing
    /// itself. But "warn and ship it anyway" was the whole of it, and the three
    /// failure shapes are all silent to a reader: too few args leaves a hole in
    /// a sentence, a surplus arg is dropped, and a leftover *named* slot
    /// (`{done}`, `{project}` — both real templates used those spellings before
    /// #47 rewrote them) is emitted literally, since only the bare `{}` is a
    /// slot here. The `debug_assert!` makes every one of them a test failure at
    /// zero production cost.
    ///
    /// It lives here rather than in [`interpolate`] so that function stays a
    /// pure, lenient primitive its own degradation test can still exercise in
    /// both profiles.
    ///
    /// `meta` lands in a `BTreeMap` so the rendered attribute order is stable —
    /// a push is user-visible text, and a wobbling attribute order would make
    /// transcripts diff-noisy for no reason.
    pub fn new<I, K, V>(template: &'static str, args: &[&str], meta: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        let mut kept = BTreeMap::new();
        for (k, v) in meta {
            let k = k.as_ref();
            if valid_meta_key(k) {
                kept.insert(k.to_string(), v.into());
            } else {
                warn!(
                    key = %k,
                    "offload push: dropping channel meta key — must match ^[a-zA-Z_][a-zA-Z0-9_]*$"
                );
            }
        }
        let (content, slots) = interpolate(template, args);
        debug_assert_eq!(
            slots,
            args.len(),
            "offload push: template slot/argument count mismatch — `{template}` has {slots} `{{}}` \
             slot(s) and was given {} argument(s), so the notice text is malformed. Fix the \
             producer (a leftover NAMED slot like `{{done}}` is not a slot here).",
            args.len()
        );
        Self {
            content,
            meta: kept,
        }
    }

    /// The `<channel>` body. Read-only by construction — [`new`](Self::new) is
    /// the only way to set it.
    pub fn content(&self) -> &str {
        &self.content
    }
}

/// Drop every meta key the client would silently discard. Shared by the
/// constructor and the deserialize path so both halves of the wire agree.
fn keep_valid_meta(meta: BTreeMap<String, String>) -> BTreeMap<String, String> {
    meta.into_iter()
        .filter(|(k, _)| {
            let ok = valid_meta_key(k);
            if !ok {
                warn!(
                    key = %k,
                    "offload push: dropping channel meta key — must match ^[a-zA-Z_][a-zA-Z0-9_]*$"
                );
            }
            ok
        })
        .collect()
}

/// Fill `template`'s `{}` slots from `args`, in order. Returns the filled text
/// and the number of slots the template actually had.
///
/// Deliberately lenient on a count mismatch (a missing arg leaves the slot
/// empty, a surplus arg is dropped) and loud about it: this runs on the
/// announce path of a finished index or scan, and a producer's formatting bug
/// must not become that operation's failure. The slot count rides out (#48) so
/// [`PushNotice::new`] can turn the same mismatch into a `debug_assert!` — a
/// signal with a consumer — while this stays a pure function whose degradation
/// behaviour is testable in both build profiles.
fn interpolate(template: &'static str, args: &[&str]) -> (String, usize) {
    let mut out =
        String::with_capacity(template.len() + args.iter().map(|a| a.len()).sum::<usize>());
    let mut rest = template;
    let mut slots = 0usize;
    while let Some(at) = rest.find(PUSH_SLOT) {
        out.push_str(&rest[..at]);
        if let Some(arg) = args.get(slots) {
            out.push_str(arg);
        }
        slots += 1;
        rest = &rest[at + PUSH_SLOT.len()..];
    }
    out.push_str(rest);
    if slots != args.len() {
        warn!(
            slots,
            args = args.len(),
            "offload push: template slot/argument count mismatch — the notice text is malformed"
        );
    }
    (out, slots)
}

/// One live `/events` subscriber — a per-tab `--offload-mcp` child holding an
/// open SSE stream against this instance.
#[derive(Debug)]
pub struct PushSubscriber {
    /// The cImp tab this child was spawned for (`--tab`), when it has one. A
    /// hand-spawned or pre-V28 child sends none and is simply not addressable.
    pub tab: Option<String>,
    /// `claude` / `opencode` — which agent this child serves.
    pub consumer: String,
    /// Whether this child ACTUALLY declared `claude/channel` at `initialize`.
    /// Reported by the child from what it really put on the wire, not
    /// re-derived from settings here: the settings could have changed after the
    /// handshake, and a push to a host that never negotiated channels is
    /// silently dropped client-side (Phase 0, T6).
    pub channels: bool,
    /// Bounded ([`PUSH_QUEUE_CAP`]) queue toward that child's SSE writer.
    pub tx: mpsc::Sender<PushNotice>,
}

/// The live-children registry. Split out of [`OffloadService`] only so it can
/// be exercised without a Tauri `AppHandle`; the service owns exactly one and
/// re-exports the public API.
#[derive(Debug, Default)]
pub struct PushRegistry {
    subs: StdMutex<HashMap<u64, PushSubscriber>>,
    /// Monotonic subscriber id (never reused within an app run).
    seq: AtomicU64,
}

impl PushRegistry {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a subscriber, returning its RAII guard and the receiving half
    /// of its queue. **Deregistration is the guard's `Drop`** — the SSE loop
    /// holds it for the connection's lifetime, so every exit path (clean close,
    /// write error, keep-alive failure, task cancellation, panic unwind) removes
    /// the entry. Mirrors `harness/claude/read.rs::LiveSessionGuard`.
    pub fn register(
        self: &Arc<Self>,
        tab: Option<String>,
        consumer: String,
        channels: bool,
    ) -> (PushGuard, mpsc::Receiver<PushNotice>) {
        let (tx, rx) = mpsc::channel(PUSH_QUEUE_CAP);
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        self.subs.lock().unwrap_or_else(|p| p.into_inner()).insert(
            id,
            PushSubscriber {
                tab,
                consumer,
                channels,
                tx,
            },
        );
        (
            PushGuard {
                registry: self.clone(),
                id,
            },
            rx,
        )
    }

    /// Drop one subscriber (called only by [`PushGuard::drop`]).
    fn unregister(&self, id: u64) {
        self.subs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }

    /// Live subscriber count. Test-only: the diagnostic wrapper on
    /// `OffloadService` went away with the Phase 0 `/push_test` rig, and the
    /// registry's real consumers count deliveries, not subscribers.
    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        self.subs.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Deliver to every channel-capable subscriber matching `pick`. Returns how
    /// many accepted it. `try_send` throughout: a full queue drops the notice
    /// with a warn rather than blocking the producer.
    fn deliver(&self, notice: PushNotice, pick: impl Fn(&PushSubscriber) -> bool) -> usize {
        let subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        let mut delivered = 0usize;
        for s in subs.values() {
            if !s.channels || !pick(s) {
                continue;
            }
            match s.tx.try_send(notice.clone()) {
                Ok(()) => delivered += 1,
                Err(mpsc::error::TrySendError::Full(_)) => warn!(
                    tab = ?s.tab,
                    consumer = %s.consumer,
                    "offload push: subscriber queue full — dropping notice (pushes are best-effort)"
                ),
                Err(mpsc::error::TrySendError::Closed(_)) => debug!(
                    tab = ?s.tab,
                    "offload push: subscriber closed before its guard ran — dropping notice"
                ),
            }
        }
        delivered
    }

    /// Push to the child serving `tab`.
    ///
    /// **No production caller today** — deliberately, and this is the honest
    /// note the review asked for. The milestone's Phase C sketch pairs this with
    /// "origin-tab on `RunBody`/`/audit/run`", i.e. addressing a completion
    /// notice back at the agent tab that started the work; but the two producers
    /// that shipped are both GUI-initiated (nobody's tab), and the per-call
    /// completion notices that WOULD have used it (offload-task stragglers) were
    /// dropped at spike close in favour of Claude Code's native
    /// auto-backgrounding (decision 2). The audit runner's echo guard makes the
    /// remaining candidate a non-starter: an agent-initiated scan must not push
    /// at all, so there is no origin tab left to address.
    ///
    /// Kept — rather than deleted — because it is the addressing half of the
    /// bus's contract (`tab=` on `/events`, `PushSubscriber.tab`) and is pinned
    /// by tests; the next producer that is agent-initiated needs exactly this.
    /// `cfg(test)` so it costs nothing shipped: drop the attribute the moment a
    /// real caller exists.
    #[cfg(test)]
    pub fn push_to_tab(&self, tab: &str, notice: PushNotice) -> bool {
        self.deliver(notice, |s| s.tab.as_deref() == Some(tab)) > 0
    }

    /// Push to every channel-capable child of this instance.
    pub fn push_broadcast(&self, notice: PushNotice) -> usize {
        self.deliver(notice, |_| true)
    }
}

/// RAII deregistration handle for a [`PushRegistry`] entry. Held by the SSE
/// loop; dropping it removes the subscriber.
pub struct PushGuard {
    registry: Arc<PushRegistry>,
    id: u64,
}

impl Drop for PushGuard {
    fn drop(&mut self) {
        self.registry.unregister(self.id);
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
        let (pulse_tx, pulse_rx) = mpsc::unbounded_channel();

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
            pulse_tx,
            pushes: PushRegistry::new(),
            app,
            latest_metrics: StdMutex::new(Vec::new()),
            cap_reconcile_lock: TokioMutex::new(()),
            host_reconcile_lock: TokioMutex::new(()),
            last_host_sig: StdMutex::new(None),
            reconcile_pending: AtomicBool::new(false),
            run_log: StdMutex::new(HashMap::new()),
            run_id_seq: AtomicU64::new(1),
        });

        // V37 C5: the ONE pulse gate. Every producer feeds `pulse_tx`; this task
        // is the only thing that ever sends on `change_tx`. It starts seeded
        // with the empty-host fingerprint, which is exactly what `host` is
        // right now — so the first reconcile that connects anything reads as a
        // move, and one that connects nothing does not.
        {
            let host = service.host.clone();
            let out = service.change_tx.clone();
            tauri::async_runtime::spawn(run_pulse_gate(
                host,
                out,
                pulse_rx,
                PULSE_DEBOUNCE,
                McpSurfaceFingerprint::empty(),
            ));
        }

        // V38 Phase F (V37's E-1): the detection-change watcher. The connect-time
        // tool screen runs once per connection, so without this a rules-bundle
        // update or a detection toggle leaves a live surface screened against
        // the rules of whenever each server happened to connect.
        //
        // TWO edges, because the fact arrives two ways: a settings save (the
        // toggles) and a rules reload (the bundle, which changes no setting at
        // all). Only a settings save is compared — the config either moved or it
        // did not — while a reload is taken at face value, since "the rules are
        // different now" is exactly what it means and cImp does not hash them.
        {
            let this = service.clone();
            let mut settings_rx = service.settings.subscribe();
            let mut rules_rx = super::detection::subscribe_rules_reload();
            let mut last = super::detection::Config::from_settings(
                &service.settings.current(),
                crate::settings::injection::Scope::AppWide,
            );
            tauri::async_runtime::spawn(async move {
                loop {
                    let changed = tokio::select! {
                        s = settings_rx.recv() => match s {
                            Ok(s) => {
                                let now = super::detection::Config::from_settings(
                                    &s,
                                    crate::settings::injection::Scope::AppWide,
                                );
                                let moved = now != last;
                                last = now;
                                moved
                            }
                            // A LAGGED receiver dropped frames, and one of them
                            // can be the very edge this task exists for — the
                            // standard broadcast pattern in this file: treat it
                            // as "changed, re-check" against the authoritative
                            // current settings rather than skipping past it.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let now = super::detection::Config::from_settings(
                                    &this.settings.current(),
                                    crate::settings::injection::Scope::AppWide,
                                );
                                let moved = now != last;
                                last = now;
                                moved
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                        r = rules_rx.recv() => match r {
                            // Lagged is the same answer as Ok here: N coalesced
                            // reloads and one reload ask for the same work,
                            // because the screen reads the CURRENT rules either
                            // way.
                            Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                    };
                    if changed {
                        this.rescreen_mcp_surface().await;
                    }
                }
            });
        }

        // Relay MCP-host change pulses (reconcile added/dropped a connection, a
        // server died mid-call) into the gate.
        {
            let mut host_rx = service.host.subscribe();
            let pulses = service.pulse_tx.clone();
            tauri::async_runtime::spawn(async move {
                while host_rx.recv().await.is_ok() {
                    let _ = pulses.send(PulseSource::Host);
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

    /// Ask for a capability-change pulse on behalf of something that is NOT
    /// the MCP host — today only [`Self::spawn_health_watch`]'s backend
    /// ready-set comparison.
    ///
    /// Named for its source rather than its effect because the source is what
    /// the gate needs: [`PulseSource::Backend`] is never surface-suppressed (a
    /// backend going up/down moves the child's `offload_task` description, not
    /// the MCP surface), while a host pulse is. A future producer must pick a
    /// source deliberately instead of inheriting whichever one a generic
    /// `signal_change` happened to use.
    fn signal_backend_change(&self) {
        let _ = self.pulse_tx.send(PulseSource::Backend);
    }

    // ── V30 Phase B: session push (see the `PushRegistry` section above) ──

    /// Register a `/events` subscriber. Called by
    /// [`loopback::handle_events`](super::loopback) after auth; the returned
    /// [`PushGuard`] must be held for the SSE connection's lifetime (its `Drop`
    /// is the only deregistration path).
    pub fn register_push_subscriber(
        &self,
        tab: Option<String>,
        consumer: String,
        channels: bool,
    ) -> (PushGuard, mpsc::Receiver<PushNotice>) {
        self.pushes.register(tab, consumer, channels)
    }

    // The `push_to_tab` / `push_broadcast` / `push_subscriber_count` wrappers
    // that used to sit here were pure indirection for the Phase 0 `/push_test`
    // rig and went with it. Producers take the registry itself (see
    // `push_registry` below), which is the only send half they need.

    /// A handle on the push bus for **producers that are not the offload
    /// service** (V30 Phase C: the graph indexer, the audit runner). Handing out
    /// the registry rather than an `Arc<OffloadService>` is deliberate: those
    /// services need exactly the send half, and holding the whole service would
    /// make the Arc graph circular (the service reaches the graph service via
    /// its tools). The registry itself holds no back-reference to anything.
    /// The warm MCP host.
    ///
    /// V38 Phase F: the audit runner takes this (and nothing else of the offload
    /// layer) so a **tier-2 provider** tool can be dispatched under V37's
    /// enforcement. Handing out the host rather than the service is what keeps
    /// it a one-way edge — the host holds no reference back, so there is no
    /// cycle and no way for the audit side to reach the run queue.
    pub fn mcp_host(&self) -> Arc<McpHost> {
        self.host.clone()
    }

    pub fn push_registry(&self) -> Arc<PushRegistry> {
        self.pushes.clone()
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
        let cur = self.settings.current();
        let snap = cur.offload.clone();
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
        let sig = host_config_sig(
            &snap.mcp_servers,
            &snap.mcp_categories,
            &snap.mcp_activation,
            &roots,
        );
        if self.last_host_sig.lock().unwrap().as_deref() == Some(sig.as_str()) {
            return;
        }
        self.host
            .reconcile(
                &snap.mcp_servers,
                &snap.mcp_categories,
                &snap.mcp_activation,
                &roots,
                // V37 contract C9. `AppWide` because the warm host is one pool
                // shared by every Claude tab, every OpenCode tab and the worker:
                // there is no single scope whose per-tab override could speak for
                // it, and `AppWide` is the scope whose documented meaning is
                // exactly "what the application is configured to do".
                super::detection::Config::from_settings(
                    &cur,
                    crate::settings::injection::Scope::AppWide,
                ),
            )
            .await;
        *self.last_host_sig.lock().unwrap() = Some(sig);
    }

    /// V38 Phase F (V37's E-1) — re-screen the live MCP surface against the
    /// CURRENT detection config, drop-only, and pulse if anything went.
    ///
    /// The pulse source is [`PulseSource::Host`] and that is exact: a withheld
    /// tool is a change to what every consumer is advertised, so the host's own
    /// surface fingerprint is the right thing to judge it by — and it will
    /// judge a real drop as a move, because the drop is one.
    pub async fn rescreen_mcp_surface(&self) {
        let cfg = super::detection::Config::from_settings(
            &self.settings.current(),
            crate::settings::injection::Scope::AppWide,
        );
        if self.host.rescreen(cfg).await {
            let _ = self.pulse_tx.send(PulseSource::Host);
        }
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
            //
            // V38 Phase F: `Audit` lands here for the same reason and never in
            // practice — the audit fan-out advertises nothing and calls a name
            // its manifest already fixed, so it has no `tools/list` at all.
            Consumer::Claude | Consumer::Offload | Consumer::Audit => {
                self.host.tool_defs_for_claude().await
            }
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
    /// not offered by a server exposed to `consumer`. Recorded in the Tool
    /// Activity feed (`kind: "mcp"`) by the host; `cwd` is the calling
    /// session's working directory when the child sent one, attributing the
    /// row to that project.
    ///
    /// V32 Phase C: `scope` names the calling tab's contaminated scope
    /// (`agent:tab`, or a fail-open placeholder when the child sent no tab
    /// identity) for the SSRF screen's `injection_flag` row, and the screen's
    /// carve-out [`Policy`](super::outbound::Policy) is derived from the live
    /// settings snapshot here — the endpoints the user configured are the only
    /// private addresses an external tool may be pointed at.
    ///
    /// V32 Phase G: `tab` is the calling tab's id (absent on a pre-V28 child),
    /// which is what turns `scope`'s human label into a resolvable
    /// [`Scope`](crate::settings::injection::Scope) for the SSRF guard's
    /// three-level switch. Without it the call resolves at
    /// [`Scope::UnknownCaller`](crate::settings::injection::Scope::UnknownCaller)
    /// — the app-wide answer plus any configured tab's L3 `On`, the same
    /// fail-open shape the latch takes for an identity-less call.
    ///
    /// #48: `audit` is the calling tab session's claim ledger, threaded from
    /// the loopback handler (which owns the registry the ledger lives in) to
    /// the SSRF chokepoint. It is passed through rather than resolved here
    /// because the handler already holds it.
    ///
    /// **KNOWN GAP, stated 2026-08-08 (#48, review finding G-4).** The
    /// settings snapshot is *not* threaded the same way, though two comments in
    /// this file and one in `loopback.rs` said it was. The `self.settings.current()`
    /// below is a **second, independent read**: the handler resolves the latch,
    /// the budget, the detection config and the envelope from its own snapshot,
    /// and the SSRF [`outbound::Policy`] is built from this one. A settings save
    /// landing between them leaves a call admitted under posture A and screened
    /// under posture B. The window is sub-millisecond and both postures are the
    /// user's own, so the practical impact is benign — but it is a stated
    /// cross-module invariant that does not hold, which is precisely the class
    /// this milestone's own principles single out. Closing it is one extra
    /// parameter (`snap: &Settings`) from the handler; recorded in the V32
    /// spec's Accepted residuals rather than changed in a documentation pass.
    // See `McpHost::call_recorded`, whose argument list this forwards verbatim.
    #[allow(clippy::too_many_arguments)]
    /// #48 F-20: `tab_attr` is a SECOND, differently-typed reading of the same
    /// fact as `tab`, and the two are deliberately not merged. `tab` answers
    /// "which scope do the injection features resolve at", where an unrecognized
    /// id and no id both correctly mean `Scope::UnknownCaller` — the scope whose
    /// whole definition is "a real tab we cannot name" (#48 F-35; it was called
    /// `Scope::App` when this sentence was written, and this site is the
    /// clearest statement of that question anywhere in the tree). `tab_attr`
    /// answers "which
    /// tab does this row belong to", where those two cases MUST stay distinct
    /// (`Unrecognized` vs `Headless`). Collapsing them is the defect F-20 filed.
    ///
    /// #48 M-17: the error half is a [`HostError`], not a `String` — a failed MCP
    /// call carries the remote server's own `error.message`, and the two authors'
    /// bytes stay apart until a caller states which reader it is composing for.
    #[allow(clippy::too_many_arguments)]
    pub async fn mcp_call(
        &self,
        consumer: Consumer,
        name: &str,
        args: serde_json::Value,
        cwd: Option<&Path>,
        scope: &str,
        tab: Option<&str>,
        tab_attr: crate::activity::Attribution,
        audit: &dyn outbound::ScopeAudit,
    ) -> Result<String, super::mcp_host::HostError> {
        // See `mcp_tool_descriptors`: `offload` never legitimately reaches
        // this proxy; fall back to the Claude-guarded set.
        let consumer = match consumer {
            Consumer::Opencode => Consumer::Opencode,
            // V38 Phase F: `Audit` cannot reach this route either — the fan-out
            // calls the host directly, in-process. Folded onto the Claude-guarded
            // default with the worker, so a stray query value can never widen a
            // grant by naming a consumer this proxy does not serve.
            Consumer::Claude | Consumer::Offload | Consumer::Audit => Consumer::Claude,
        };
        let snap = self.settings.current();
        let agent = crate::graph::source_for_consumer(consumer.source());
        let policy = outbound::Policy::from_settings(
            &snap,
            crate::settings::injection::Scope::for_tab(agent, tab),
        );
        self.host
            .call_recorded(consumer, cwd, name, args, scope, tab_attr, &policy, audit)
            .await
    }

    // V32 Phase G removed the `external_budget_limits` / `detection_config`
    // accessors that used to live here, cutting four independent
    // `settings.current()` reads through this service down to one. The
    // loopback's `/mcp/call` handler takes a snapshot per call and resolves the
    // latch, the budget, the detection config and the envelope from it — a
    // mid-call settings save must not leave a result screened under one posture
    // and wrapped under another.
    //
    // Corrected 2026-08-08 (#48, G-4): this used to claim the SSRF *policy* was
    // resolved from that same snapshot. It is not — `mcp_call` above takes its
    // own read for it, so "ONE settings snapshot per call" is one short of
    // true. See the KNOWN GAP paragraph on `mcp_call`.
    //
    // The worker's copies are still snapshotted in [`Self::run`], from the
    // `cur` read it already takes.

    /// Run one offload task end-to-end against the live pool and return the
    /// synthesized answer. Acquires the global permit *and* the chosen
    /// backend's slot, so `in_flight` is honest and the global gate queues a
    /// busy pool coherently.
    // The parameter list is this crate's offload-request contract (each arg is
    // documented inline); reshaping it touches every caller and the MCP bridge.
    #[allow(clippy::too_many_arguments)]
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
        // V33 Phase F: the cImp TAB this offload was requested from, as the
        // `/run` body asserted it. Used for ONE thing — attributing the
        // pre-mutation checkpoint the worker takes before `run_command` — and
        // deliberately not for routing, budgets or gating, all of which resolve
        // the tab through `latch_scope` at the loopback instead. `None` (an
        // older MCP child, a tab-less caller) records a checkpoint with no tab,
        // which is the honest answer and the pre-V33 row.
        tab: Option<&str>,
        // V21 F9: optional JSON Schema — when set, the worker's final-synthesis
        // turn is grammar-constrained to matching JSON (threaded to `run_on` →
        // `OffloadTask::schema`). `None` leaves the answer free-form.
        schema: Option<serde_json::Value>,
        // V32 Phase A: the caller-declared task shape (`research`/`code`),
        // already validated at the loopback parse boundary. Pre-applies the
        // taint latch in the agent loop so the blocked class is never
        // advertised, not even on turn 1. `None` latches dynamically.
        profile: Option<Profile>,
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
                "no offload backend is configured — add one in cImp Settings → Offload task tools"
                    .into(),
            ));
        }

        let views: Vec<BackendView> = pool
            .iter()
            .map(|p| BackendView {
                name: p.name.clone(),
                base_url: p.base_url.clone(),
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
        let chosen = router::select(&views, &req)
            .map_err(|e: RouteError| AppError::OffloadNotReady(e.to_string()))?;
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
        // #48 (finding M-1): ONE scope for this whole `offload_task` — its
        // EXTERNAL budget, its `injection_flag` scope id, its latch-refusal claim
        // and its SSRF/unscreened ledger. Constructed HERE, *outside* the
        // fail-over / thinking-retry / escalation ladder below, because each of
        // those enters `run_on` again: a budget rebuilt per attempt was a cap the
        // task could reset simply by failing (the documented 40 calls / 4 MiB was
        // really up to 160 / 16 MiB), and a scope id minted per attempt scattered
        // one task's audit rows across four uncorrelatable ids.
        let mut task_scope = agent::TaskScope::for_task();

        // First attempt with the requested thinking mode.
        let first = self
            .run_on(
                &pool[chosen],
                &views[chosen],
                &snap,
                &instructions,
                context.clone(),
                thinking,
                session_cwd.clone(),
                tab,
                schema.clone(),
                profile,
                overall_deadline,
                Some(&mut trace),
                &cancel,
                &mut task_scope,
            )
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
                        self.run_on(
                            &pool[next],
                            &views[next],
                            &snap,
                            &instructions,
                            context.clone(),
                            thinking,
                            session_cwd.clone(),
                            tab,
                            schema.clone(),
                            profile,
                            overall_deadline,
                            Some(&mut trace),
                            &cancel,
                            &mut task_scope,
                        )
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
                .run_on(
                    &pool[active],
                    &views[active],
                    &snap,
                    &instructions,
                    context.clone(),
                    ThinkingMode::Auto,
                    session_cwd.clone(),
                    tab,
                    schema.clone(),
                    profile,
                    retry_deadline,
                    Some(&mut trace),
                    &cancel,
                    &mut task_scope,
                )
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

        // V21 F5 — tier escalation: a fast-tier run that came back only
        // partially verified gets ONE re-run on a distinct, ready quality
        // backend, and the better answer wins (quality wins; a failed escalation
        // keeps the fast answer). Structurally inert unless a second, quality-tier
        // backend exists (zero-config setups never escalate) and gated by
        // `escalate_partial` (default on). At most one escalation per task (this
        // runs once, not in a loop); `escalation_target` also blocks
        // quality→quality and same-instance re-runs.
        let mut escalated_from: Option<&str> = None;
        let partial = snap.escalate_partial
            && result
                .as_ref()
                .map(|t| agent::answer_verified_level(t) == agent::VerifiedLevel::Partially)
                .unwrap_or(false);
        if partial {
            if let Some(q) = router::escalation_target(&views, &req, active) {
                info!(
                    target: "offload",
                    run_id,
                    from = %views[active].name,
                    to = %views[q].name,
                    "offload: escalating partially-verified fast-tier answer to the quality backend"
                );
                let esc_deadline = Instant::now() + timeout;
                let pre_esc = trace.calls.len();
                let esc = self
                    .run_on(
                        &pool[q],
                        &views[q],
                        &snap,
                        &instructions,
                        context.clone(),
                        thinking,
                        session_cwd.clone(),
                        tab,
                        schema.clone(),
                        profile,
                        esc_deadline,
                        Some(&mut trace),
                        &cancel,
                        &mut task_scope,
                    )
                    .await;
                // The escalation's agent loop numbers its calls from step 0 again;
                // offset them so they continue after the earlier steps in the run
                // log rather than colliding (mirrors the On→Auto retry above).
                let base = trace.calls[..pre_esc]
                    .iter()
                    .map(|c| c.step)
                    .max()
                    .map_or(0, |m| m + 1);
                for c in trace.calls[pre_esc..].iter_mut() {
                    c.step += base;
                }
                match esc {
                    Ok(q_text) => {
                        // Quality wins (ties to quality). Label the extra cost.
                        result = Ok(agent::append_escalation_note(&q_text));
                        escalated_from = Some("fast");
                    }
                    Err(e) => warn!(
                        target: "offload",
                        run_id,
                        error = %e,
                        "offload: quality escalation failed; keeping the fast answer"
                    ),
                }
            }
        }

        // Close the run-log record: color by outcome (failed = red, recovered
        // = amber, success = normal).
        let outcome = match (&result, recovered) {
            (Ok(_), true) => "recovered",
            (Ok(_), false) => "success",
            (Err(_), _) => "failed",
        };
        self.run_finish(
            &backend_name,
            run_id,
            outcome,
            escalated_from,
            std::mem::take(&mut trace.calls),
        );

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
                // A worker run is real work with no tab behind it.
                crate::activity::Attribution::Headless,
                None,
                None,
                None,
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
            escalated_from: None,
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
        let rec = log.get_mut(old).and_then(|dq| {
            dq.iter()
                .position(|r| r.id == id)
                .and_then(|i| dq.remove(i))
        });
        if let Some(rec) = rec {
            let dq = log.entry(new.to_string()).or_default();
            dq.push_front(rec);
            while dq.len() > RUN_LOG_CAP {
                dq.pop_back();
            }
        }
    }

    /// Finalize a run record with its captured calls + final outcome. V21 F5:
    /// `escalated_from` labels a run that was re-run on the quality backend after
    /// a partial fast-tier answer (visible cost in the run log).
    fn run_finish(
        &self,
        backend: &str,
        id: u64,
        outcome: &str,
        escalated_from: Option<&str>,
        calls: Vec<CallRecord>,
    ) {
        let mut log = self.run_log.lock().unwrap();
        if let Some(dq) = log.get_mut(backend) {
            if let Some(rec) = dq.iter_mut().find(|r| r.id == id) {
                rec.calls = calls;
                rec.outcome = outcome.into();
                rec.escalated_from = escalated_from.map(|s| s.to_string());
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
    ///
    /// **This is ONE attempt at the task, not the task** (#48/M-1). [`Self::run`]
    /// calls it up to four times — fail-over, the thinking retry, tier escalation
    /// — which is why `scope` is borrowed from the caller and nothing per-task is
    /// built in here.
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
        // V33 Phase F: the requesting tab, for the pre-mutation checkpoint's
        // attribution only. See [`Self::run`]'s parameter of the same name.
        tab: Option<&str>,
        schema: Option<serde_json::Value>,
        // V32 Phase A: pre-applies the agent loop's taint latch (see
        // `agent::OffloadTask::profile`).
        profile: Option<Profile>,
        deadline: Instant,
        trace: Option<&mut RunTrace>,
        cancel: &CancellationToken,
        // #48/M-1: this task's shared budget / scope id / audit ledger, minted
        // once by the caller outside the retry ladder. Borrowed, never built here.
        scope: &mut agent::TaskScope,
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
        // V33 Phase F: this is the ONE in-app worker path, so it is the one
        // that can reach `WorkbenchService` and take a pre-mutation checkpoint
        // before `run_command` (the only routed tool with `mutates_fs: true`).
        // The root is `cwd` — the CALLING session's directory when it forwarded
        // one, which is the repo whose shadow repo must hold the rewind point,
        // not the app's launch dir. `try_state` because the service is
        // constructed unconditionally at startup but this code also runs in
        // contexts (tests) where it is not registered; `None` there simply
        // means "no checkpoint", never a failed tool call.
        let checkpoint = self
            .app
            .try_state::<std::sync::Arc<crate::workbench::WorkbenchService>>()
            .map(|w| tools::ToolCheckpoint {
                root: cwd.clone(),
                tab: tab.map(str::to_string),
                workbench: w.inner().clone(),
            });
        let ctx = ToolCtx::new(
            roots,
            snap.command_allowlist.clone(),
            snap.command_policies.clone(),
            &cwd,
        )
        .with_checkpoint(checkpoint)
        // V33 Phase A: the in-app worker is the one path that both knows the
        // live settings and spawns `run_command` children, so it is where the
        // OS sandbox is opted in. The headless `--offload-mcp` child keeps
        // `SandboxCfg::disabled()` and says so out loud at spawn time
        // (`SkipReason::Unavailable`), rather than claiming a boundary it has
        // no settings snapshot to configure.
        .with_sandbox(crate::sandbox::SandboxCfg::from_settings(
            &self.settings.current(),
        ));
        let mut native_defs = tools::enabled_defs(&snap.tools);
        // One settings snapshot for both feature gates below.
        let cur = self.settings.current();
        // #48, finding F-10: the backend's whole admission policy — tool scope
        // plus the V9-01 graph and V26 audit opt-ins — resolved once, in the
        // one constructor the headless child and the self-test also use, and
        // handed to the router as the thing it re-checks per call. What is
        // ADVERTISED below is derived from the same value, so a tool can no
        // longer be offered by one rule and refused by another.
        let gate = super::backend_gate::BackendGate::for_worker(
            entry.tool_scope.clone(),
            entry.is_remote,
            &cur,
        );
        // V9-01: offer the graph tools to the worker when the feature is on AND
        // either this backend is local or the user opted a remote backend in
        // (a remote — LAN or cloud — would receive the project's code
        // structure). The gate re-gates dispatch as defense-in-depth.
        if gate.graph_allowed() {
            native_defs.extend(tools::graph_tools::defs());
        }
        // V26: offer the code-audit tools to the worker when the feature is
        // enabled AND the offload consumer is opted in (`expose_offload`,
        // default true) AND the backend is local. The scan itself always runs
        // locally inside the app (via the process-global `AuditState`), but its
        // *report* — repo file:line paths plus scanner messages that can quote
        // the offending code — is local data, so it must not cross to a
        // remote/LAN/cloud backend: the same boundary `worker_graph_allowed`
        // enforces for the graph tools. The gate re-gates dispatch as
        // defense-in-depth (advertisement alone doesn't stop a hallucinated
        // call), and `begin_scan` re-enforces the master switch.
        if gate.audit_allowed() {
            native_defs.extend(tools::audit_tools::defs());
        }
        let mcp_defs = self.host.tool_defs_for_offload().await;
        // V32 Phase C: one scope id, shared by the router (whose SSRF rows the
        // shared chokepoint writes) and the loop (whose budget / canary / latch
        // rows it writes), so every `injection_flag` row from this task
        // correlates. #48/M-1: it is the TASK's id and it arrives in `scope` —
        // minting it here made it the *attempt*'s, so fail-over, the thinking
        // retry and tier escalation each wrote under a different scope and a
        // reader could not tell they belonged to one `offload_task`. The SSRF
        // carve-out policy is still snapshotted here beside the tool surface,
        // from the same settings read.
        //
        // #48/M-5: ONE reading of the result cap for both the loop (which
        // truncates with it) and the router (which tells the detection boundary
        // how much of a result the model will actually see). Two readings of
        // `per_tool_result_token_cap.max(256)` could drift, and the drift would
        // silently make the "unscreened" notice wrong again.
        let result_cap_tokens = snap.per_tool_result_token_cap.max(256);
        let router = HostRouter::new(
            native_defs,
            mcp_defs,
            ctx,
            self.host.clone(),
            gate,
            scope,
            outbound::Policy::from_settings(&cur, WORKER),
            super::detection::Config::from_settings(&cur, WORKER),
            crate::settings::injection::effective(
                crate::settings::injection::Feature::Spotlighting,
                WORKER,
                &cur,
            ),
            result_cap_tokens,
        );
        let cfg = AgentConfig {
            base_url: entry.base_url.clone(),
            model: None,
            max_steps: snap.max_steps.max(1),
            budget_tokens: view.per_slot_budget(),
            n_ctx: view.n_ctx,
            slots: view.slots,
            per_tool_result_token_cap: result_cap_tokens,
            auth_token: entry.auth_token.clone(),
            per_call_timeout: Duration::from_secs(snap.offload_timeout_secs.max(30)),
            // V32 Phase G: every worker-side control resolves at the
            // `offload-worker` pseudo-scope, from the SAME settings snapshot as
            // the policy and detection config above — one task, one posture, for
            // the run's whole life.
            external_budget: crate::settings::injection::budget_limits(&cur, WORKER),
            latch_active: crate::settings::injection::effective(
                crate::settings::injection::Feature::TaintLatch,
                WORKER,
                &cur,
            ),
            canary_active: crate::settings::injection::effective(
                crate::settings::injection::Feature::Canary,
                WORKER,
                &cur,
            ),
        };
        let task = OffloadTask {
            instructions: instructions.to_string(),
            context,
            thinking,
            schema,
            profile,
        };
        agent::run(
            &self.client,
            &cfg,
            &router,
            task,
            deadline,
            trace,
            cancel,
            scope,
        )
        .await
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
                OffloadBackendKind::Local {
                    server_command,
                    auth_token,
                    ..
                } => {
                    if let Some(e) = self
                        .resolve_local(b, server_command, auth_token, lazy_start)
                        .await
                    {
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
        self.local_pool
            .lock()
            .await
            .retain(|k, _| live.contains(k.as_str()));
        self.remote_pool
            .lock()
            .await
            .retain(|k, _| live.contains(k.as_str()));

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
        auth_token: &str,
        lazy_start: bool,
    ) -> Option<PoolEntry> {
        // V33 Phase E. Read from the LIVE config on every resolve, not from the
        // handle: the supervisor's warm `LlamaServer` baked its probe token at
        // start, and the chat call must use whatever the user has configured
        // now. Empty ⇒ `None` ⇒ no `Authorization` header at all (an empty
        // bearer is worse than none).
        let chat_auth = (!auth_token.is_empty()).then(|| auth_token.to_string());
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
                auth_token: chat_auth,
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
                if let Err(e) = supervisor
                    .start_backend(&name, None, super::supervisor::StartCause::Lazy)
                    .await
                {
                    debug!(backend = %name, error = %e, "offload: lazy start failed");
                }
            });
        }
        // Try a transient handle for a server the user may have launched
        // themselves (or one already warming from a prior offload). Cache it by
        // name (reused while the base URL is unchanged) so its slot gate — and
        // thus `in_flight` — persists across calls; a fresh handle each time
        // would always report 0 in-flight and the router would never throttle.
        //
        // V33 Phase E: the reuse predicate also fingerprints the token, for the
        // reason `resolve_remote` spells out — a rotated credential must force a
        // rebuild, or the cached handle keeps probing with the stale one.
        let transient = {
            let mut pool = self.local_pool.lock().await;
            let reuse = pool.get(&b.name).filter(|h| {
                h.base_url() == base_url && h.auth_token().unwrap_or("") == auth_token
            });
            match reuse {
                Some(h) => h.clone(),
                None => {
                    let h = Arc::new(
                        LlamaServer::with_config(
                            &b.name,
                            server_command,
                            auth_token,
                            b.tier,
                            b.tool_scope.clone(),
                        )
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
            auth_token: chat_auth,
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
            let reuse = pool
                .get(&b.name)
                .filter(|h| h.base_url() == base_url.trim_end_matches('/'));
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
                    Settings → Offload task tools.)"
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
             a backend with file access (never a cloud backend).{parallel_note}{PROFILE_TOOL_NOTE}",
            parts.join("; ")
        )
    }

    /// Tear down the warm pool (app exit / disable).
    pub async fn shutdown(&self) {
        self.host.shutdown().await;
    }

    /// The latest per-backend dashboard snapshot (initial fill for the Offload
    /// server dashboard; live updates arrive via the `offload-server-metrics`
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
                // V33 Phase E: the poller hits `/slots` + `/metrics` + `/props`
                // on the SAME server the probes do, so it needs the same
                // credential. It passed `None` before the Local backend had a
                // token — leaving it would make the dashboard silently read
                // "offline" on a keyed server that the router is happily using.
                // V33 stage 3: resolved through `effective_auth_token`, so it
                // inherits the `--api-key` fallback rather than re-deciding.
                let auth_token = b.kind.effective_auth_token();
                let (state, metrics) = match self.supervisor.running_server(&b.name).await {
                    Some(server) if server.is_ready() => {
                        let poller = pollers
                            .entry(b.name.clone())
                            .or_insert_with(MetricsPoller::new);
                        let m = poller
                            .poll(
                                &server.base_url(),
                                (!auth_token.is_empty()).then_some(auth_token.as_str()),
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
                            .poll(
                                &e.base_url,
                                auth.as_deref(),
                                e.slots,
                                e.n_ctx,
                                in_flight,
                                cap,
                            )
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

    /// V37 contract C6 — spawn the MCP-server health checker.
    ///
    /// One task for the whole pool, on its own cadence
    /// (`offload.mcp_health_interval_secs`, `0` = off). Deliberately a SECOND
    /// task rather than another job inside [`Self::spawn_health_watch`], and the
    /// separation is the contract: that watcher's whole purpose is to call
    /// `warm_host` (and therefore `reconcile`, under `host_reconcile_lock`),
    /// while this one must never reconcile at all — it probes the live pool and
    /// records. Folding them together would tie the probe cadence to the
    /// reconcile cadence and put the checker behind a lock every offload run
    /// takes.
    ///
    /// The cadence is re-read every iteration, so editing it in Settings takes
    /// effect on the next tick with no restart. The interval is clamped to
    /// [`MCP_HEALTH_MIN_SECS`]..=[`MCP_HEALTH_MAX_SECS`] and the per-probe
    /// timeout is derived from it ([`mcp_probe_timeout`]), so "well under the
    /// cadence" is a property of the code rather than of the user's number.
    pub fn spawn_mcp_health_watch(self: &Arc<Self>) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let configured = this.settings.current().offload.mcp_health_interval_secs;
                // Off. Still ticks, so turning it back on does not need a
                // restart — it just does nothing while it is off.
                let interval = if configured == 0 {
                    Duration::from_secs(MCP_HEALTH_MAX_SECS as u64)
                } else {
                    Duration::from_secs(
                        configured.clamp(MCP_HEALTH_MIN_SECS, MCP_HEALTH_MAX_SECS) as u64
                    )
                };
                // Sleep FIRST: at launch the pool is still connecting, and a
                // probe against a half-warm host would report a connect that
                // has not finished as a failure.
                tokio::time::sleep(interval).await;
                // ONE settings read per sweep: the probe below takes real time,
                // and the registry the retry filters against must be the same
                // snapshot as the detection config it screens with.
                let cur = this.settings.current();
                let snap = cur.offload.clone();
                if snap.mcp_health_interval_secs == 0 || !snap.mcp_host_needed() {
                    continue;
                }
                this.host.probe_health(mcp_probe_timeout(interval)).await;
                // V37 Phase E: one reconnect attempt per server this lane has
                // already reported down, AFTER the probe — so a server that came
                // back on its own is seen by the probe (which mints the ordinary
                // recovery row) and is no longer a candidate here. The checker
                // itself still never reconciles; this is a per-server replace
                // guarded at the swap, not a pool rebuild. See
                // `McpHost::retry_unhealthy` for why it takes no reconcile lock.
                this.host
                    .retry_unhealthy(
                        &snap.mcp_servers,
                        &snap.mcp_categories,
                        &snap.mcp_activation,
                        super::detection::Config::from_settings(
                            &cur,
                            crate::settings::injection::Scope::AppWide,
                        ),
                    )
                    .await;
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
                let mut ready: Vec<String> = pool
                    .iter()
                    .filter(|p| p.ready)
                    .map(|p| p.name.clone())
                    .collect();
                ready.sort();
                if ready != last {
                    last = ready;
                    this.signal_backend_change();
                }
            }
        });
    }
}

/// Floor under the MCP health cadence. Not a preference — a guard: the setting
/// is hand-editable, and a `1` there would put a `tools/list` on every HTTP
/// server every second forever.
const MCP_HEALTH_MIN_SECS: u32 = 5;
/// Ceiling under the same clamp, and the idle tick when the checker is off. Also
/// the longest a cadence edit can take to be noticed.
const MCP_HEALTH_MAX_SECS: u32 = 3600;

/// The per-probe timeout for one health sweep: a third of the cadence, bounded
/// to a sane window.
///
/// Derived rather than configured because the invariant contract C6 states is a
/// RELATION ("per-check timeout well under the cadence"), and a second setting
/// would let a user express a pair that violates it — a 60s timeout on a 10s
/// cadence, where every sweep overlaps the next.
fn mcp_probe_timeout(interval: Duration) -> Duration {
    Duration::from_secs((interval.as_secs() / 3).clamp(2, 10))
}

/// V37 contract C5 — where a capability-change pulse came from, and therefore
/// what the gate is allowed to do with it.
///
/// The distinction exists because the `change` frame is broader than the MCP
/// surface. The per-session child's `tools/list` is `offload_task`/
/// `offload_batch` (whose live description names the reachable backends) +
/// the graph tools + the proxied MCP surface, so "did the MCP surface move" is
/// the right suppression question for exactly one of the three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PulseSource {
    /// The MCP host: `reconcile` added/dropped a connection, or a server died
    /// mid-call. Suppressed when no consumer's advertised surface moved.
    Host,
    /// A producer whose change is invisible in the MCP surface — today the
    /// backend ready-set watcher, which already does its own did-it-move
    /// comparison before it asks. Never suppressed here; suppressing it would
    /// silently undo `spawn_health_watch`'s documented purpose.
    Backend,
}

/// How long the gate collects pulses before deciding. The UI batches a category
/// toggle into ONE settings write (Phase D), but a reconcile still signals per
/// connection edge, and `warm_host` can be driven from three places at once —
/// so a single user action legitimately produces a burst. Tests supply their
/// own window and assert ONE pulse per action; nothing depends on this value.
const PULSE_DEBOUNCE: Duration = Duration::from_millis(300);

/// The ONE place a `change` pulse is decided: debounce, then suppress.
///
/// Every producer (`reconcile`'s connection edges, a server dying mid-call, the
/// backend ready-set watcher) funnels into `rx`; `out` is the broadcast the
/// loopback `/events` stream — the sole consumer of the `change` frame — reads.
/// Concentrating it here rather than at each producer is what makes
/// "one user action, one `tools/list_changed`" a property of the system instead
/// of a property every call site has to remember.
///
/// Order matters: the fingerprint is read AFTER the window closes, so a burst
/// is judged by where the host ended up, not by any intermediate state it
/// passed through (a reconnect that tears a server down and brings it back with
/// the same tools is correctly silent).
async fn run_pulse_gate(
    host: Arc<McpHost>,
    out: broadcast::Sender<()>,
    mut rx: mpsc::UnboundedReceiver<PulseSource>,
    window: Duration,
    seed: McpSurfaceFingerprint,
) {
    let mut last = seed;
    while let Some(first) = rx.recv().await {
        let mut from_host = first == PulseSource::Host;
        let mut unconditional = first == PulseSource::Backend;
        // Collect everything that arrives inside the window. The deadline runs
        // from the FIRST pulse, so a steady stream cannot postpone the emit
        // indefinitely.
        let deadline = tokio::time::sleep(window);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                more = rx.recv() => match more {
                    Some(PulseSource::Host) => from_host = true,
                    Some(PulseSource::Backend) => unconditional = true,
                    // Every producer is gone: decide on what we have, then the
                    // outer loop sees the same `None` and the task ends.
                    None => break,
                },
            }
        }
        let mut emit = unconditional;
        if from_host {
            let fingerprint = host.surface_fingerprint().await;
            if fingerprint != last {
                last = fingerprint;
                emit = true;
            }
        }
        if emit {
            let _ = out.send(());
        }
    }
}

/// Stable signature of a cached remote handle for reuse comparison.
fn remote_sig(h: &RemoteBackend, is_cloud: bool) -> String {
    format!(
        "{}|{}|{is_cloud}",
        h.base_url(),
        token_fp(h.auth_token().unwrap_or(""))
    )
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
            OffloadBackendKind::Local { server_command, .. } => {
                ServerCommand::parse(server_command)
                    .map(|c| c.parallel.max(1))
                    .unwrap_or(1)
            }
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
pub(super) fn worker_graph_allowed(
    graph_enabled: bool,
    is_remote: bool,
    allow_remote: bool,
) -> bool {
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
                auth_token: String::new(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn global_cap_sums_slots_and_clamps() {
        // Built by mutation rather than by functional update: `OffloadSettings`
        // carries fields that are `pub(in crate::settings)` (#44/#48 — the
        // injection hierarchy's inputs), and `..Default::default()` names every
        // field, private ones included (E0451).
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
        assert!(is_connection_error(
            "chat request failed: connection refused"
        ));
        assert!(is_connection_error("request timed out"));
        assert!(!is_connection_error("server returned no choices"));
    }

    // ── V30 Phase B: the push bus ────────────────────────────────────────

    /// The empty meta list, named so the turbofish-free call sites below stay
    /// readable (`[]` alone cannot infer `K`/`V`).
    fn no_meta() -> [(&'static str, &'static str); 0] {
        []
    }

    /// The client silently drops meta keys outside `^[a-zA-Z_][a-zA-Z0-9_]*$`,
    /// so the constructor rejects them at the write boundary instead.
    #[test]
    fn push_notice_validates_meta_keys() {
        for good in ["kind", "_seq", "a1", "Run_id_2", "_"] {
            assert!(valid_meta_key(good), "`{good}` should be accepted");
        }
        for bad in ["", "1kind", "run-id", "run.id", "run id", "kind!", "héllo"] {
            assert!(!valid_meta_key(bad), "`{bad}` should be rejected");
        }

        let notice = PushNotice::new(
            "audit finished",
            &[],
            [
                ("kind", "audit_done"),
                ("run-id", "7"), // hyphen → dropped
                ("2nd", "x"),    // leading digit → dropped
                ("_ok", "yes"),  // leading underscore → kept
            ],
        );
        assert_eq!(notice.content(), "audit finished");
        assert_eq!(
            notice.meta.keys().collect::<Vec<_>>(),
            vec!["_ok", "kind"],
            "only contract-valid keys survive, in stable (BTreeMap) order"
        );
    }

    /// The template fills left to right, and a producer's count mistake
    /// degrades rather than panics in a release build — this runs on the
    /// announce path of a finished scan.
    ///
    /// Asserted against [`interpolate`] rather than [`PushNotice::new`] for the
    /// mismatch cases, because `new` now `debug_assert!`s on them (#48) and the
    /// lenient behaviour is what ships when assertions are compiled out.
    #[test]
    fn push_notice_fills_template_slots_in_order() {
        let n = PushNotice::new("{} of {} in {}s", &["3 files", "/proj", "12"], no_meta());
        assert_eq!(n.content(), "3 files of /proj in 12s");
        // No slots, no args: the template is the content verbatim.
        assert_eq!(PushNotice::new("plain", &[], no_meta()).content(), "plain");
        // Surplus slot ⇒ empty; surplus arg ⇒ dropped. Both warn, and both
        // report the slot count the assertion in `new` checks against.
        assert_eq!(interpolate("a{}b{}", &["X"]), ("aXb".to_string(), 2));
        assert_eq!(interpolate("a{}b", &["X", "Y"]), ("aXb".to_string(), 1));
        // A leftover NAMED slot is not a slot: it survives verbatim, which is
        // the shape a reader never notices and the assertion below catches.
        assert_eq!(
            interpolate("done: {done}", &[]),
            ("done: {done}".to_string(), 0)
        );
    }

    /// #48: the slot/argument mismatch finally has a consumer. It warned and
    /// shipped the malformed notice, so a producer bug reached the model's
    /// transcript and nothing failed — the repo's "every quality signal needs a
    /// consumer" principle, applied at zero production cost.
    ///
    /// `cfg(debug_assertions)` because that is exactly the condition under
    /// which the assertion exists; a release test run must not demand a panic
    /// the build compiled out.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "slot/argument count mismatch")]
    fn a_push_template_with_the_wrong_argument_count_fails_the_suite() {
        let _ = PushNotice::new("a{}b{}", &["X"], no_meta());
    }

    /// The other direction, and the one a rename actually produces: a template
    /// whose slots were renamed to `{done}`-style names takes zero arguments
    /// and silently emits the braces.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "slot/argument count mismatch")]
    fn a_push_template_with_a_leftover_named_slot_fails_the_suite() {
        let _ = PushNotice::new("done: {done}", &["yes"], no_meta());
    }

    /// V32 locked decision 9, the half a type CAN state: the deserialize path
    /// is not a second constructor. It runs `TryFrom`, which rejects a notice
    /// with nothing to say ("empty is not absent" — an empty `<channel>`
    /// message costs the receiving session a turn) and drops the meta keys the
    /// client would silently discard, exactly as `new` does.
    #[test]
    fn push_notice_deserialization_is_validated() {
        let ok: PushNotice =
            serde_json::from_str(r#"{"content":"c","meta":{"ok_1":"a","bad-key":"b"}}"#)
                .expect("a well-formed notice deserializes");
        assert_eq!(ok.content(), "c");
        assert_eq!(
            ok.meta.keys().collect::<Vec<_>>(),
            vec!["ok_1"],
            "the parse boundary applies the same meta-key contract as the constructor"
        );

        for rejected in [
            r#"{"content":""}"#,
            r#"{"content":"   "}"#,
            r#"{"meta":{"kind":"x"}}"#,
        ] {
            assert!(
                serde_json::from_str::<PushNotice>(rejected).is_err(),
                "a notice with no content must not deserialize: {rejected}"
            );
        }

        // A real notice still round-trips byte-identically — the wire format is
        // shared with the child process and did not change.
        let n = PushNotice::new("hello {}", &["world"], [("kind", "t")]);
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, r#"{"content":"hello world","meta":{"kind":"t"}}"#);
        assert_eq!(serde_json::from_str::<PushNotice>(&json).unwrap(), n);
    }

    /// Registration is explicit; deregistration is RAII — the entry goes away
    /// when the guard drops, which is what the SSE loop relies on for every one
    /// of its exit paths.
    #[test]
    fn push_registry_deregisters_on_guard_drop() {
        let reg = PushRegistry::new();
        assert_eq!(reg.subscriber_count(), 0);
        let (g1, _rx1) = reg.register(Some("claude".into()), "claude".into(), true);
        let (g2, _rx2) = reg.register(Some("claude-2".into()), "claude".into(), true);
        assert_eq!(reg.subscriber_count(), 2);
        drop(g1);
        assert_eq!(reg.subscriber_count(), 1);
        drop(g2);
        assert_eq!(reg.subscriber_count(), 0);
        // Ids are monotonic, so a re-registered tab can never collide with the
        // stale entry of a connection that is still unwinding.
        let (g3, _rx3) = reg.register(Some("claude".into()), "claude".into(), true);
        assert_eq!(g3.id, 2);
    }

    /// A tab-addressed push reaches exactly the child of that tab — not its
    /// siblings, not a tab-less child, and not one that never declared the
    /// capability (a push to such a host is silently dropped client-side).
    #[tokio::test]
    async fn push_to_tab_addresses_only_that_tab() {
        let reg = PushRegistry::new();
        let (_g_a, mut rx_a) = reg.register(Some("claude".into()), "claude".into(), true);
        let (_g_b, mut rx_b) = reg.register(Some("claude-2".into()), "claude".into(), true);
        let (_g_nochan, mut rx_nochan) =
            reg.register(Some("claude-3".into()), "claude".into(), false);
        let (_g_anon, mut rx_anon) = reg.register(None, "claude".into(), true);

        assert!(reg.push_to_tab("claude", PushNotice::new("hi", &[], [("kind", "t")])));
        assert_eq!(rx_a.recv().await.map(|n| n.content().to_string()), Some("hi".to_string()));
        assert!(rx_b.try_recv().is_err(), "sibling tab must not receive it");
        assert!(
            rx_anon.try_recv().is_err(),
            "a tab-less child is unaddressed"
        );

        // The channels flag is respected even when the tab matches.
        assert!(!reg.push_to_tab("claude-3", PushNotice::new("x", &[], [] as [(&str, &str); 0])));
        assert!(rx_nochan.try_recv().is_err());

        // An unknown tab delivers to nobody and says so.
        assert!(!reg.push_to_tab("ghost", PushNotice::new("x", &[], [] as [(&str, &str); 0])));
    }

    /// Broadcast hits every channel-capable subscriber (tab-less ones included)
    /// and skips the rest.
    #[tokio::test]
    async fn push_broadcast_counts_channel_subscribers() {
        let reg = PushRegistry::new();
        let (_g_a, mut rx_a) = reg.register(Some("claude".into()), "claude".into(), true);
        let (_g_anon, mut rx_anon) = reg.register(None, "claude".into(), true);
        let (_g_nochan, mut rx_nochan) = reg.register(Some("oc".into()), "opencode".into(), false);

        assert_eq!(
            reg.push_broadcast(PushNotice::new("all", &[], [] as [(&str, &str); 0])),
            2
        );
        assert!(rx_a.recv().await.is_some());
        assert!(rx_anon.recv().await.is_some());
        assert!(rx_nochan.try_recv().is_err());
    }

    /// A wedged child must never back-pressure a producer: once its bounded
    /// queue is full the notice is dropped and the push reports non-delivery.
    #[test]
    fn push_drops_when_the_subscriber_queue_is_full() {
        let reg = PushRegistry::new();
        let (_g, _rx) = reg.register(Some("claude".into()), "claude".into(), true);
        let notice = || PushNotice::new("x", &[], [] as [(&str, &str); 0]);
        for i in 0..PUSH_QUEUE_CAP {
            assert!(reg.push_to_tab("claude", notice()), "push {i} should queue");
        }
        assert!(
            !reg.push_to_tab("claude", notice()),
            "the {}th push must drop, not block",
            PUSH_QUEUE_CAP + 1
        );
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
    // ── V37 C5: the pulse gate ───────────────────────────────────────────────

    /// A short window keeps the tests quick. Nothing below asserts the window's
    /// value — the contract is ONE pulse per action, whatever the constant is.
    const TEST_WINDOW: Duration = Duration::from_millis(30);

    /// Long enough for the gate to have closed its window and decided. Several
    /// windows wide so a scheduling hiccup cannot turn a real pulse into a
    /// missing one (which would make the test pass for the wrong reason).
    async fn settle() {
        tokio::time::sleep(TEST_WINDOW * 8).await;
    }

    fn spawn_gate(
        host: Arc<McpHost>,
    ) -> (
        mpsc::UnboundedSender<PulseSource>,
        broadcast::Receiver<()>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (out, out_rx) = broadcast::channel(64);
        tokio::spawn(run_pulse_gate(
            host,
            out,
            rx,
            TEST_WINDOW,
            McpSurfaceFingerprint::empty(),
        ));
        (tx, out_rx)
    }

    /// Drain what the gate emitted since the last check.
    fn pulses(rx: &mut broadcast::Receiver<()>) -> usize {
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    /// C5, the whole contract in one run: a burst coalesces, an unmoved surface
    /// is silent, and a surface that moves emits exactly once.
    #[tokio::test]
    async fn pulse_gate_coalesces_a_burst_and_suppresses_an_unmoved_surface() {
        let host = McpHost::new();
        let (tx, mut out) = spawn_gate(host.clone());

        // (b) A reconcile that changes connections without moving any
        //     consumer's advertised surface — here, the degenerate case: the
        //     host still advertises nothing, which is what the gate was seeded
        //     with. Zero pulses.
        for _ in 0..5 {
            tx.send(PulseSource::Host).unwrap();
        }
        settle().await;
        assert_eq!(pulses(&mut out), 0, "an unmoved surface must not pulse");

        // (a)+(c) A toggle that DOES move the surface, signalled as a burst the
        //     way `reconcile` signals per connection edge: exactly ONE pulse.
        host.insert_fake_server("alpha", true, true, true, "alpha__x")
            .await;
        for _ in 0..8 {
            tx.send(PulseSource::Host).unwrap();
        }
        settle().await;
        assert_eq!(
            pulses(&mut out),
            1,
            "a burst from one action must coalesce into ONE pulse"
        );

        // And the new surface becomes the baseline: repeating the burst with
        // nothing changed is silent again.
        for _ in 0..4 {
            tx.send(PulseSource::Host).unwrap();
        }
        settle().await;
        assert_eq!(pulses(&mut out), 0);
    }

    /// A pulse that is NOT about the MCP surface must never be suppressed by
    /// it. `spawn_health_watch` exists to tell `/events` about a backend going
    /// up/down — that moves the child's `offload_task` description, not the
    /// proxied tool list, so a surface fingerprint has nothing to say about it.
    #[tokio::test]
    async fn pulse_gate_never_suppresses_a_backend_pulse() {
        let host = McpHost::new();
        let (tx, mut out) = spawn_gate(host);

        tx.send(PulseSource::Backend).unwrap();
        settle().await;
        assert_eq!(pulses(&mut out), 1, "a backend pulse must always emit");

        // Mixed burst: the backend half carries it even though the host half
        // would have been suppressed, and it is still ONE pulse.
        tx.send(PulseSource::Host).unwrap();
        tx.send(PulseSource::Backend).unwrap();
        tx.send(PulseSource::Host).unwrap();
        settle().await;
        assert_eq!(pulses(&mut out), 1);
    }
}
