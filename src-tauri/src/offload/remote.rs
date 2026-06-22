//! V8-02 remote offload backend — a LAN box or a cloud API ccImp only
//! health-checks and connects to over HTTP.
//!
//! Unlike [`LlamaServer`](super::LlamaServer), ccImp does **not** own the
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
    /// Declared parallel capacity (the remote doesn't report `-np`).
    slots: u32,
    ready: AtomicBool,
    /// Discovered `n_ctx` from `/props`; `0` means not-yet-known (the
    /// accessor then falls back to `declared_context`).
    n_ctx: AtomicU32,
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
        let slots = slots.max(1);
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
            slots,
            ready: AtomicBool::new(false),
            n_ctx: AtomicU32::new(0),
            gate: Arc::new(Semaphore::new(slots as usize)),
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
    /// Cloud endpoints often lack `/health`; a 404/401 still proves the
    /// host is reachable, so we treat any HTTP *response* (even an error
    /// status) as "reachable" for cloud and only require 2xx for LAN
    /// llama-server, which does expose `/health`.
    pub async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url);
        let resp = self.with_auth(self.client.get(&url)).send().await;
        let ok = match resp {
            Ok(r) => {
                // LAN llama-server: require 2xx. Cloud: any response means
                // the host answered (the real auth/route check happens on
                // the first chat call).
                self.is_cloud || r.status().is_success()
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
        Ok(())
    }

    /// The shared HTTP client (reused for the agent loop's chat calls).
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Per-request working token budget: `(n_ctx / slots) * high_water/100`.
    pub fn per_slot_budget(&self, high_water_pct: u8) -> Option<u32> {
        let n = self.n_ctx()?;
        let per_slot = n / self.slots.max(1);
        Some(per_slot.saturating_mul(high_water_pct.min(100) as u32) / 100)
    }

    /// Acquire one slot, waiting up to `timeout`.
    pub async fn acquire_slot(&self, timeout: Duration) -> AppResult<OwnedSemaphorePermit> {
        match tokio::time::timeout(timeout, self.gate.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(AppError::Offload("offload concurrency gate closed".into())),
            Err(_) => Err(AppError::OffloadNotReady(format!(
                "all {} slot(s) on remote backend `{}` busy — timed out after {}s",
                self.slots,
                self.name,
                timeout.as_secs()
            ))),
        }
    }

    pub fn mark_stopped(&self) {
        self.ready.store(false, Ordering::Relaxed);
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
        self.slots
    }
    fn in_flight(&self) -> u32 {
        self.slots
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
    fn per_slot_budget_divides_by_declared_slots() {
        let b = RemoteBackend::new(
            "lan", "http://x", "", false, BackendTier::Fast, ToolScope::All, Some(16_000), 1,
        )
        .unwrap();
        assert_eq!(b.per_slot_budget(80), Some(12_800));
    }
}
