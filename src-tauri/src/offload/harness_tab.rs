//! V39 Phase C — **the facade offload backend**: an open AI tab, dressed as a
//! server.
//!
//! Locked decision 3's second driver mode. A tab whose `delegation_role` is
//! `RemoteOffload` is synthesized into the offload pool
//! ([`crate::settings::Settings::effective_offload_backends`]) as an
//! [`OffloadBackendKind::HarnessTab`](crate::settings::OffloadBackendKind), and
//! this is the [`Backend`] impl the router then treats exactly like a LAN box.
//!
//! # What is different from [`RemoteBackend`](super::RemoteBackend), and why
//!
//! * **There is no endpoint.** [`Backend::base_url`] answers an opaque
//!   `harness-tab://<tab id>` that nothing dials — it exists because the V21 F5
//!   escalation guard identifies a *server instance* by URL, and two facades
//!   must read as two instances. Nothing HTTP ever sees it: the facade run
//!   bypasses the agent loop entirely (`service::run_on`).
//! * **Readiness is live, not health-checked** (the doc's "Readiness" note).
//!   There is no probe, no `offload_server` row, no `mcp_health` row: a facade
//!   is ready iff the tab is a worker *right now*, which
//!   [`crate::delegation::worker_ready`] answers from the same five preflight
//!   checks a delegation would run. A not-ready facade is simply not routed to.
//! * **One slot, and the truth about it lives elsewhere.** [`Backend::slots`]
//!   is 1 (locked decision 9) and [`Backend::in_flight`] is read from the
//!   *delegation registry* plus the tab's own activity, not from this handle's
//!   semaphore — a tab busy with an EXPLICIT `delegate_task_*` call, or with
//!   its own user's turn, is busy, and a router that could not see that would
//!   route onto a worker whose next answer is a refusal.
//!
//! The semaphore is still real: it is what makes two concurrent facade routes
//! to one tab queue for the slot rather than race into the engine's atomic
//! claim and have the loser refused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{AppError, AppResult};
use crate::settings::{BackendTier, ToolScope};
use crate::state::TabId;

use super::Backend;

/// The scheme of the opaque per-facade [`Backend::base_url`]. Never dialled;
/// see the module docs.
const FACADE_URL_SCHEME: &str = "harness-tab://";

/// The context window assumed for a facade whose user declared none.
///
/// **200k tokens**, and it is a *routing* number rather than a measurement:
/// cImp cannot ask a harness tab how much window it has left, and the two
/// harnesses cImp drives today front models in the 200k class. The doc's
/// instruction is "a generous default", and generous is the safe direction
/// here — the failure modes are asymmetric. Under-declare and the router quietly
/// steers big tasks away from a worker that would have handled them (a silent
/// loss of capability, with nothing on screen to explain it); over-declare and
/// the worker's own harness reports its own context error, visibly, in its own
/// tab, where the user is already looking.
///
/// A user who knows better sets `declared_context` in the tab's delegation
/// popover, exactly as they would for an HTTP backend.
const DEFAULT_FACADE_CONTEXT: u32 = 200_000;

/// One `RemoteOffload` tab, as a [`Backend`].
pub struct HarnessTabBackend {
    /// The user-chosen backend name. **The only identity a driver ever sees.**
    name: String,
    /// The worker tab. Internal — see [`crate::settings::OffloadBackendKind`].
    tab: TabId,
    tier: BackendTier,
    tool_scope: ToolScope,
    declared_context: Option<u32>,
    ready: AtomicBool,
    /// The worker's user was busy at the last probe — mid-turn, on a prompt, or
    /// with text typed and not sent. Read by [`Backend::in_flight`], never by
    /// [`Backend::is_ready`]: a tab whose user is typing is BUSY, not broken.
    busy: AtomicBool,
    /// One permit. Not the source of truth for `in_flight` (see the module
    /// docs) — the thing that makes a second concurrent route *wait* instead of
    /// being refused.
    gate: Arc<Semaphore>,
}

impl HarnessTabBackend {
    pub fn new(
        name: &str,
        tab: &TabId,
        tier: BackendTier,
        tool_scope: ToolScope,
        declared_context: Option<u32>,
    ) -> Self {
        Self {
            name: name.to_string(),
            tab: tab.clone(),
            tier,
            tool_scope,
            declared_context,
            ready: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            gate: Arc::new(Semaphore::new(1)),
        }
    }

    /// The worker tab this facade drives.
    pub fn tab(&self) -> &TabId {
        &self.tab
    }

    /// The user's declared window, unresolved — `None` means "use the default".
    /// The *resolved* value is [`Backend::n_ctx`]; this one exists so a cached
    /// handle can be compared against the config it was built from.
    pub fn declared_context(&self) -> Option<u32> {
        self.declared_context
    }

    /// Re-evaluate readiness from live tab state, and answer it.
    ///
    /// The [`crate::delegation::worker_ready`] call is the whole implementation
    /// on purpose: the rules for "is this tab a worker" are written once, in the
    /// engine, and both the router's question and the engine's own preflight run
    /// them. A second copy here would be a second answer.
    /// The reason is not stored, only logged: the surface that shows a user
    /// *why* a facade is down (`supervisor::statuses`) asks the engine itself,
    /// so keeping a second copy here would be a cache of a sentence that is free
    /// to recompute.
    pub async fn refresh_ready(&self, core: &crate::service::host::CoreHost) -> bool {
        // Asked on the same probe, so the router sees one consistent picture of
        // the tab rather than a readiness from now and a busy-ness from before.
        self.busy.store(
            crate::delegation::worker_busy(core, &self.tab).await,
            Ordering::Relaxed,
        );
        match crate::delegation::worker_ready(core, &self.tab).await {
            Ok(()) => {
                self.ready.store(true, Ordering::Relaxed);
                true
            }
            Err(reason) => {
                self.ready.store(false, Ordering::Relaxed);
                tracing::debug!(
                    backend = %self.name,
                    tab = %self.tab.as_str(),
                    reason = %reason,
                    "offload: facade backend is not ready"
                );
                false
            }
        }
    }

    /// Take the single slot, or fail after `timeout` with a message shaped like
    /// every other backend's.
    pub async fn acquire_slot(&self, timeout: Duration) -> AppResult<OwnedSemaphorePermit> {
        match tokio::time::timeout(timeout, self.gate.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(AppError::Offload("offload concurrency gate closed".into())),
            Err(_) => Err(AppError::OffloadNotReady(format!(
                "all 1 slot(s) on remote backend `{}` busy — timed out after {}s",
                self.name,
                timeout.as_secs()
            ))),
        }
    }
}

impl Backend for HarnessTabBackend {
    fn name(&self) -> &str {
        &self.name
    }
    fn base_url(&self) -> String {
        format!("{FACADE_URL_SCHEME}{}", self.tab.as_str())
    }
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
    fn n_ctx(&self) -> Option<u32> {
        Some(self.declared_context.unwrap_or(DEFAULT_FACADE_CONTEXT))
    }
    fn slots(&self) -> u32 {
        1
    }
    fn in_flight(&self) -> u32 {
        // Two ways the one slot is taken, and the router needs both:
        //
        // * a delegation is running on the tab — read from the REGISTRY, not
        //   from this handle's semaphore, because an explicit `delegate_task_*`
        //   call holds the tab without ever touching this handle;
        // * the tab's own user is mid-turn, on a prompt, or has text typed and
        //   not sent — which preflight would refuse, so a router that could not
        //   see it would send the task somewhere it is about to be turned away
        //   from instead of to the free backend beside it.
        (crate::delegation::is_driven(&self.tab) || self.busy.load(Ordering::Relaxed)) as u32
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

    fn backend(ctx: Option<u32>) -> HarnessTabBackend {
        HarnessTabBackend::new(
            "lan-worker-2",
            &TabId::from_str("tab-abc"),
            BackendTier::Quality,
            ToolScope::All,
            ctx,
        )
    }

    #[test]
    fn declared_context_wins_over_the_generous_default() {
        assert_eq!(backend(Some(32_000)).n_ctx(), Some(32_000));
        assert_eq!(backend(None).n_ctx(), Some(DEFAULT_FACADE_CONTEXT));
    }

    /// Locked decision 9: one delegation per worker. A facade is single-slot by
    /// construction, not by configuration — there is no `-np` to discover.
    #[test]
    fn a_facade_is_always_one_slot_and_starts_not_ready() {
        let b = backend(None);
        assert_eq!(b.slots(), 1);
        // Never ready until something has actually looked at the tab: a handle
        // that defaulted to ready would route the very first task at a tab
        // nobody has checked.
        assert!(!b.is_ready());
        assert_eq!(b.in_flight(), 0);
    }

    /// The escalation guard identifies a server instance by URL, so two facades
    /// must not share one — and the value must not be dialable.
    #[test]
    fn each_facade_has_its_own_opaque_base_url() {
        let a = backend(None);
        let b = HarnessTabBackend::new(
            "other",
            &TabId::from_str("tab-def"),
            BackendTier::Fast,
            ToolScope::All,
            None,
        );
        assert_ne!(a.base_url(), b.base_url());
        assert!(a.base_url().starts_with(FACADE_URL_SCHEME));
        assert!(!a.base_url().starts_with("http"));
    }
}
