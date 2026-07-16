//! V8-02 remote offload backend — a LAN box or a cloud API cImp only
//! health-checks and connects to over HTTP.
//!
//! Unlike [`LlamaServer`](super::LlamaServer), cImp does **not** own the
//! process: there is no command to spawn, no PTY, and no read-only tab —
//! a remote backend surfaces only as a Settings status line. It exposes
//! the same [`Backend`](super::Backend) accessors so the router treats it
//! uniformly:
//!
//! - `base_url`/`auth` come from config (`OffloadBackendKind::Remote`).
//! - readiness is observed by polling `GET /health`.
//! - the context window is discovered from `GET /props` (`n_ctx`) when the
//!   endpoint exposes it (a remote `llama-server` does), else falls back to
//!   the configured `declared_context` (many cloud APIs don't expose
//!   `/props`).
//! - concurrency is gated by a semaphore sized to `slots` (declared; a
//!   remote endpoint doesn't tell us its `-np`, so the user sizes it — or
//!   it defaults to 1).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::debug;

use crate::error::{AppError, AppResult};
use crate::settings::{BackendTier, ToolScope};

use super::Backend;

/// A remote OpenAI-compatible backend (LAN or cloud).
#[allow(dead_code)] // some fields feed the warm-pool followup (see impl note)
pub struct RemoteBackend {
    name: String,
    base_url: String,
    /// Bearer token for the endpoint (cloud APIs); empty = none.
    auth_token: String,
    is_cloud: bool,
    tier: BackendTier,
    tool_scope: ToolScope,
    /// Context window to assume when `/props` is unavailable.
    declared_context: Option<u32>,
    /// Fallback parallel capacity, used until `/props` reveals the real
    /// `total_slots` (a remote doesn't report `-np` on the command line, but
    /// a llama-server *does* expose `total_slots` on `/props`).
    declared_slots: u32,
    ready: AtomicBool,
    /// Discovered `n_ctx` from `/props`; `0` means not-yet-known (the
    /// accessor then falls back to `declared_context`).
    n_ctx: AtomicU32,
    /// Discovered `total_slots` from `/props`; `0` means not-yet-known (the
    /// accessor then falls back to `declared_slots`). The gate grows to
    /// match as bigger values are discovered.
    slots: AtomicU32,
    /// Set when `/props` reports *fewer* slots than the gate is sized for
    /// (the server was restarted with a smaller `-np`). A live semaphore can't
    /// be safely shrunk, so instead of mis-scheduling against the stale larger
    /// gate we mark the handle stale; `resolve_remote` then rebuilds it.
    needs_rebuild: AtomicBool,
    gate: Arc<Semaphore>,
    client: reqwest::Client,
}

// Several accessors (`acquire_slot`, `per_slot_budget`, `client`,
// `poll_until_ready`, …) complete the `Backend` seam for the warm-pool
// target design (V8-01 open decision), where the long-lived app holds
// `RemoteBackend` instances and runs the agent loop against them. The
// current per-call child uses inline probing instead, so those are not yet
// wired — kept here so V8-02's followup is additive, not a rewrite.
#[allow(dead_code)]
impl RemoteBackend {
    /// Build a remote backend. `slots` is the declared parallel capacity
    /// (≥1). Does not contact the endpoint — call [`Self::health_check`]
    /// or [`Self::poll_until_ready`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &str,
        base_url: &str,
        auth_token: &str,
        is_cloud: bool,
        tier: BackendTier,
        tool_scope: ToolScope,
        declared_context: Option<u32>,
        slots: u32,
    ) -> AppResult<Self> {
        let declared_slots = slots.max(1);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AppError::Offload(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_token: auth_token.to_string(),
            is_cloud,
            tier,
            tool_scope,
            declared_context,
            declared_slots,
            ready: AtomicBool::new(false),
            n_ctx: AtomicU32::new(0),
            slots: AtomicU32::new(0),
            needs_rebuild: AtomicBool::new(false),
            gate: Arc::new(Semaphore::new(declared_slots as usize)),
            client,
        })
    }

    pub fn is_cloud(&self) -> bool {
        self.is_cloud
    }

    /// The bearer token (empty = none). Used by the agent loop to set
    /// `Authorization` on chat requests.
    pub fn auth_token(&self) -> Option<&str> {
        if self.auth_token.is_empty() {
            None
        } else {
            Some(&self.auth_token)
        }
    }

    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.auth_token() {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    /// One `GET /health` probe. `true` iff the endpoint answered 2xx.
    /// Cloud endpoints often lack `/health`; a 404/405 still proves the
    /// host is reachable, so for cloud we accept any response EXCEPT the ones
    /// that mean it's actually unusable — a bad token (401/403) or a server
    /// error (5xx). LAN llama-server exposes `/health`, so we require 2xx there.
    pub async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url);
        let resp = self.with_auth(self.client.get(&url)).send().await;
        let ok = match resp {
            Ok(r) => {
                let status = r.status();
                if self.is_cloud {
                    // Host answered; only reject the statuses that prove it
                    // can't serve us. The route/auth detail is re-checked on
                    // the first chat call.
                    !(status == reqwest::StatusCode::UNAUTHORIZED
                        || status == reqwest::StatusCode::FORBIDDEN
                        || status.is_server_error())
                } else {
                    status.is_success()
                }
            }
            Err(_) => false,
        };
        self.ready.store(ok, Ordering::Relaxed);
        ok
    }

    /// Poll `/health` until ready or `timeout` elapses, then read `/props`
    /// once to cache `n_ctx` (best-effort — cloud endpoints may not expose
    /// it, in which case `declared_context` is used).
    pub async fn poll_until_ready(&self, timeout: Duration) -> AppResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.health_check().await {
                let _ = self.refresh_props().await; // best-effort
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(AppError::OffloadNotReady(format!(
                    "remote backend `{}` at {} did not become healthy within {}s",
                    self.name,
                    self.base_url,
                    timeout.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// `GET /props` and cache `n_ctx` if reported. Returns `Ok(())` even
    /// when the endpoint has no `/props` (the accessor falls back to
    /// `declared_context`); only a transport failure is an error.
    pub async fn refresh_props(&self) -> AppResult<()> {
        let url = format!("{}/props", self.base_url);
        let resp = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| AppError::Offload(format!("/props request failed: {e}")))?;
        if !resp.status().is_success() {
            return Ok(()); // no /props on this endpoint — keep declared_context
        }
        let v: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        let n = v
            .get("default_generation_settings")
            .and_then(|g| g.get("n_ctx"))
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("n_ctx").and_then(|x| x.as_u64()));
        if let Some(n) = n {
            self.n_ctx.store(n as u32, Ordering::Relaxed);
            debug!(backend = %self.name, n_ctx = n, "offload: discovered remote context window");
        }
        // A llama-server reports its real parallel capacity (`-np`) here, so
        // the status line and the concurrency gate reflect the box instead
        // of the assumed single slot. Cloud APIs omit it → keep declared.
        if let Some(t) = v.get("total_slots").and_then(|x| x.as_u64()) {
            self.note_total_slots(t as u32);
        }
        Ok(())
    }

    /// Record a discovered `total_slots`, growing the concurrency gate to
    /// match. Only ever grows: a running server's `-np` is fixed, and
    /// shrinking a live semaphore safely is fiddly. Idempotent.
    fn note_total_slots(&self, t: u32) {
        let t = t.max(1);
        // CAS loop so concurrent callers (the metrics poller and a live `run()`
        // both refresh `/props` on the same shared `Arc`) can't each add the
        // same delta and over-grow the gate. Only the thread that successfully
        // swaps `slots` prev->t adds the matching permits; losers retry against
        // the fresh value and find nothing left to grow.
        //
        // We only ever grow: a live `tokio::Semaphore` can't be safely shrunk,
        // so if a re-fetched `/props` reports *fewer* slots (server restarted
        // with a smaller `-np`) we leave BOTH the gate and `self.slots`
        // unchanged. Swapping `slots` down while the gate keeps the larger
        // permit count would make `slots()`/`in_flight()` disagree with the
        // real concurrency the gate allows, and the router would mis-schedule.
        loop {
            let prev = self.slots.load(Ordering::Relaxed);
            let effective_prev = if prev == 0 { self.declared_slots } else { prev };
            if t < effective_prev {
                // Server came back with a smaller `-np`. We can't shrink the
                // live gate; mark the handle for rebuild so the next resolve
                // constructs a correctly-sized backend instead of letting the
                // router over-schedule against the stale larger gate.
                self.needs_rebuild.store(true, Ordering::Relaxed);
                break;
            }
            if t == effective_prev {
                break;
            }
            if self
                .slots
                .compare_exchange(prev, t, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.gate.add_permits((t - effective_prev) as usize);
                break;
            }
            // Lost the race; another thread bumped `slots`. Retry.
        }
        debug!(backend = %self.name, total_slots = t, "offload: discovered remote slot count");
    }

    /// The shared HTTP client (reused for the agent loop's chat calls).
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Per-request working token budget: `n_ctx * high_water/100`. A remote
    /// `llama-server`'s `/props` n_ctx is already per-slot (and `declared_context`
    /// is a single endpoint's usable window), so — like the Local backend — we
    /// don't divide by `slots` again.
    pub fn per_slot_budget(&self, high_water_pct: u8) -> Option<u32> {
        let n = self.n_ctx()?;
        Some(n.saturating_mul(high_water_pct.min(100) as u32) / 100)
    }

    /// Acquire one slot, waiting up to `timeout`.
    pub async fn acquire_slot(&self, timeout: Duration) -> AppResult<OwnedSemaphorePermit> {
        match tokio::time::timeout(timeout, self.gate.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(AppError::Offload("offload concurrency gate closed".into())),
            Err(_) => Err(AppError::OffloadNotReady(format!(
                "all {} slot(s) on remote backend `{}` busy — timed out after {}s",
                self.slots(),
                self.name,
                timeout.as_secs()
            ))),
        }
    }

    pub fn mark_stopped(&self) {
        self.ready.store(false, Ordering::Relaxed);
    }

    /// Whether this handle should be discarded and rebuilt (the remote came
    /// back with fewer slots than the live gate is sized for).
    pub fn needs_rebuild(&self) -> bool {
        self.needs_rebuild.load(Ordering::Relaxed)
    }
}

impl Backend for RemoteBackend {
    fn name(&self) -> &str {
        &self.name
    }
    fn base_url(&self) -> String {
        self.base_url.clone()
    }
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
    fn n_ctx(&self) -> Option<u32> {
        match self.n_ctx.load(Ordering::Relaxed) {
            0 => self.declared_context,
            n => Some(n),
        }
    }
    fn slots(&self) -> u32 {
        match self.slots.load(Ordering::Relaxed) {
            0 => self.declared_slots,
            n => n,
        }
    }
    fn in_flight(&self) -> u32 {
        self.slots()
            .saturating_sub(self.gate.available_permits() as u32)
    }
    fn tier(&self) -> BackendTier {
        self.tier
    }
    fn tool_scope(&self) -> &ToolScope {
        &self.tool_scope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_context_used_until_props_discovered() {
        let b = RemoteBackend::new(
            "cloud",
            "https://api.example.com/v1",
            "secret",
            true,
            BackendTier::Quality,
            ToolScope::All,
            Some(32_000),
            1,
        )
        .unwrap();
        // Before /props: declared.
        assert_eq!(b.n_ctx(), Some(32_000));
        // After /props reports a value: discovered wins.
        b.n_ctx.store(128_000, Ordering::Relaxed);
        assert_eq!(b.n_ctx(), Some(128_000));
    }

    #[test]
    fn trailing_slash_trimmed_from_base_url() {
        let b = RemoteBackend::new(
            "lan",
            "http://192.168.1.5:8080/",
            "",
            false,
            BackendTier::Fast,
            ToolScope::All,
            None,
            1,
        )
        .unwrap();
        assert_eq!(b.base_url(), "http://192.168.1.5:8080");
        assert!(b.auth_token().is_none());
    }

    #[test]
    fn total_slots_discovery_grows_slots_and_gate() {
        let b = RemoteBackend::new(
            "lan",
            "http://x",
            "",
            false,
            BackendTier::Fast,
            ToolScope::All,
            Some(60_000),
            1,
        )
        .unwrap();
        // Before /props: the assumed single slot.
        assert_eq!(b.slots(), 1);
        assert_eq!(b.in_flight(), 0);
        // llama-server reports `-np 2` via `/props.total_slots`.
        b.note_total_slots(2);
        assert_eq!(b.slots(), 2);
        assert_eq!(b.gate.available_permits(), 2);
        // Idempotent: a repeat probe doesn't keep growing the gate.
        b.note_total_slots(2);
        assert_eq!(b.slots(), 2);
        assert_eq!(b.gate.available_permits(), 2);
        // A later probe reporting FEWER slots must not shrink either the
        // reported count or the gate: a live semaphore can't be shrunk safely,
        // so lowering `slots()` alone would make it disagree with the gate.
        b.note_total_slots(1);
        assert_eq!(b.slots(), 2);
        assert_eq!(b.gate.available_permits(), 2);
    }

    #[test]
    fn per_slot_budget_uses_n_ctx_directly() {
        let b = RemoteBackend::new(
            "lan",
            "http://x",
            "",
            false,
            BackendTier::Fast,
            ToolScope::All,
            Some(16_000),
            1,
        )
        .unwrap();
        // n_ctx is per-slot already → 16000 * 80% = 12800.
        assert_eq!(b.per_slot_budget(80), Some(12_800));
    }
}
