//! The **one** place a live Tauri app becomes the handles a loopback route
//! handler runs on (V42 Phase A2).
//!
//! ## What this replaces
//!
//! Twenty-one `AppHandle::try_state::<T>()` calls spread across the eleven
//! route-family files, resolving four things: the app's settings (through
//! `live_settings`, which the module docs already called "the one `AppHandle` →
//! `Settings` point"), the warm code graph, the Workbench service and the Code
//! Audit runner. Each was a service-locator call in the middle of a security
//! boundary — code that has to be readable as "what may this request have?" and
//! was instead also answering "is that subsystem up?".
//!
//! They are one trait now, with one implementation, in this file — which is
//! deliberately **not** part of the route surface
//! ([`ROUTE_SOURCES`](super::loopback::ROUTE_SOURCES)). The route files hold no
//! lookup at all, and
//! `loopback::tests::no_route_file_reaches_into_managed_state` fails if one
//! comes back.
//!
//! ## Why the lookups stayed lazy
//!
//! The obvious move — resolve everything once when the listener binds — would
//! have changed behaviour. `Loopback::start` can run *before* `wire_graph` and
//! `wire_workbench` have managed their services (the runtime starts from
//! `wire_offload`, and again from a settings watcher long afterwards), so a
//! snapshot taken at bind time could hold `None` forever for a service that
//! came up a millisecond later. [`RouteServices`] is therefore an interface,
//! not a struct: the Tauri implementation asks at request time exactly as the
//! handlers used to, and a test implementation answers from whatever it was
//! built with. That is dependency inversion rather than a relocated locator —
//! the routes declare what they need, and *how* it is found is the composition
//! root's business.
//!
//! ## The residual, stated
//!
//! [`RouteCtx`] still carries an `AppHandle`, reachable as [`RouteCtx::app`].
//! Three onward calls need one and have not been inverted: the taint latch's
//! scope resolution (`offload::latch`), the project-root resolution
//! (`offload::discovery`), and a harness plugin's own route handler
//! (`harness::plugin::RouteHandler` is `fn(&AppHandle, &Request)`, a declared
//! plugin contract). Those are V42 #114/#115's seam, not this phase's, and
//! every `ctx.app()` in a route file is a marker of one.

use std::sync::Arc;

use tauri::{AppHandle, Manager};

use crate::service::host::CoreHost;

/// What a loopback route handler may reach beyond its own request.
///
/// Four capabilities, each `Option` because each is genuinely optional: a build
/// with the graph disabled, a request that arrives before a service is
/// managed, or a test that supplies none of them. `None` means what an
/// unresolved `try_state` meant — the route takes its documented no-capability
/// path, never an error.
pub trait RouteServices: Send + Sync {
    /// The core handles (settings, tabs, the state-signal channel, the TTS
    /// queue, the launch directory). `None` = the tab layer is not up.
    fn core(&self) -> Option<CoreHost>;
    /// The warm per-project code graph.
    fn graph(&self) -> Option<Arc<crate::graph::GraphService>>;
    /// The Workbench service (shadow repo, checkpoints, fs batches).
    fn workbench(&self) -> Option<Arc<crate::workbench::WorkbenchService>>;
    /// The Code Audit scan runner.
    fn audit(&self) -> Option<Arc<crate::audit::AuditState>>;
}

/// The live-app implementation: the twenty-one lookups, in one place.
pub struct TauriRouteServices {
    app: AppHandle,
    /// Handed in by the composition root when there is one — the loopback
    /// listener is started from `wiring`, which already holds these. `None` on
    /// the adapter path ([`RouteCtx::from_app`]), where all a caller has is an
    /// `AppHandle` and the core is resolved the way the routes used to resolve
    /// it.
    core: Option<CoreHost>,
}

impl TauriRouteServices {
    pub fn new(app: AppHandle, core: Option<CoreHost>) -> Self {
        Self { app, core }
    }
}

impl RouteServices for TauriRouteServices {
    fn core(&self) -> Option<CoreHost> {
        if let Some(core) = self.core.clone() {
            return Some(core);
        }
        // The adapter path. `AppState` is managed before `setup` runs, so this
        // resolves for every request a live app can serve; `None` is the same
        // "not up yet" the handlers' own `try_state` misses meant.
        let state = self.app.try_state::<crate::ipc::AppState>()?;
        Some(CoreHost {
            events: Arc::new(crate::service::sink::TauriEventSink::new(self.app.clone())),
            settings: state.settings.clone(),
            tabs: state.tabs.clone(),
            read_only: state.read_only.clone(),
            tab_activity: state.tab_activity.clone(),
            input_lengths: state.input_lengths.clone(),
            tts_segments: state.tts_segments.clone(),
            state_signals: state.state_signals.clone(),
            launch_cwd: state.launch.cwd.clone(),
            invocation_args: Arc::new(state.launch.extra_args.clone()),
        })
    }

    fn graph(&self) -> Option<Arc<crate::graph::GraphService>> {
        self.app
            .try_state::<Arc<crate::graph::GraphService>>()
            .map(|s| s.inner().clone())
    }

    fn workbench(&self) -> Option<Arc<crate::workbench::WorkbenchService>> {
        self.app
            .try_state::<Arc<crate::workbench::WorkbenchService>>()
            .map(|s| s.inner().clone())
    }

    fn audit(&self) -> Option<Arc<crate::audit::AuditState>> {
        self.app
            .try_state::<Arc<crate::audit::AuditState>>()
            .map(|s| s.inner().clone())
    }
}

/// Everything one loopback route handler is given besides its request.
///
/// Passed where an `AppHandle` used to be. The handlers ask it for what they
/// need by name (`ctx.settings()`, `ctx.graph()`, …) instead of asking a
/// managed-state table for a type, and [`Self::app`] is the declared residual —
/// see the module docs.
#[derive(Clone)]
pub struct RouteCtx {
    /// `None` only in a test context ([`testing::route_ctx`]), which cannot
    /// build an `AppHandle` — this crate has no `tauri::test` mock. Every
    /// production constructor supplies one, so [`Self::app`]'s `expect` is
    /// unreachable outside a test, and inside one it names the seam that still
    /// needs a handle instead of leaving the core untestable.
    app: Option<AppHandle>,
    services: Arc<dyn RouteServices>,
}

impl RouteCtx {
    pub fn new(app: AppHandle, services: Arc<dyn RouteServices>) -> Self {
        Self {
            app: Some(app),
            services,
        }
    }

    /// The adapter for a caller that has only an `AppHandle`: a harness
    /// plugin's route handler, whose signature is a declared plugin contract
    /// (`harness::plugin::RouteHandler`). Those handlers reach the same
    /// `*_core` functions the loopback's own routes do, so they need the same
    /// context; building it here is what keeps the lookups out of the route
    /// files rather than pushing them into `harness::claude::hook`.
    pub fn from_app(app: &AppHandle) -> Self {
        Self::new(
            app.clone(),
            Arc::new(TauriRouteServices::new(app.clone(), None)),
        )
    }

    /// **The declared residual.** The Tauri handle, for the three onward calls
    /// that still take one — see the module docs. Not a lookup handle: a
    /// `try_state` in a route file fails
    /// `loopback::tests::no_route_file_reaches_into_managed_state`.
    pub fn app(&self) -> &AppHandle {
        self.app.as_ref().expect(
            "this route reaches a seam that still takes an `AppHandle` (the taint latch, the \
             project-root resolution, or a harness plugin's own route handler — V42 #114/#115), \
             so it cannot run against a test context yet",
        )
    }

    /// The core handles. `None` = the tab layer is not up.
    pub fn core(&self) -> Option<CoreHost> {
        self.services.core()
    }

    /// **The one settings read.** V32 Phase G's `live_settings`, unchanged in
    /// everything but where the handle comes from: still the single point every
    /// gated handler resolves its snapshot through, so two neighbours cannot
    /// answer against different snapshots, and still falling back to
    /// `Settings::default()` — all protection ON — because a request arriving
    /// before the tab layer is up must not be the moment containment lapses.
    pub fn settings(&self) -> crate::settings::Settings {
        self.services
            .core()
            .map(|c| c.settings.current())
            .unwrap_or_default()
    }

    /// The warm per-project code graph.
    pub fn graph(&self) -> Option<Arc<crate::graph::GraphService>> {
        self.services.graph()
    }

    /// The Workbench service.
    pub fn workbench(&self) -> Option<Arc<crate::workbench::WorkbenchService>> {
        self.services.workbench()
    }

    /// The Code Audit scan runner.
    pub fn audit(&self) -> Option<Arc<crate::audit::AuditState>> {
        self.services.audit()
    }
}

/// A [`RouteCtx`] with no Tauri app behind it — what A2 buys.
///
/// Before this phase a route core could only be asserted against its own
/// source text, because every one of them took an `AppHandle` and this crate
/// has no `tauri::test` mock. A core that reaches only for injected handles
/// can now be RUN.
#[cfg(test)]
pub mod testing {
    use super::*;

    /// Answers from whatever it was built with, and nothing else.
    #[derive(Default)]
    pub struct FakeRouteServices {
        pub core: Option<CoreHost>,
        pub graph: Option<Arc<crate::graph::GraphService>>,
        pub workbench: Option<Arc<crate::workbench::WorkbenchService>>,
        pub audit: Option<Arc<crate::audit::AuditState>>,
    }

    impl RouteServices for FakeRouteServices {
        fn core(&self) -> Option<CoreHost> {
            self.core.clone()
        }
        fn graph(&self) -> Option<Arc<crate::graph::GraphService>> {
            self.graph.clone()
        }
        fn workbench(&self) -> Option<Arc<crate::workbench::WorkbenchService>> {
            self.workbench.clone()
        }
        fn audit(&self) -> Option<Arc<crate::audit::AuditState>> {
            self.audit.clone()
        }
    }

    /// A context over `services` and no app. A core that reaches
    /// [`RouteCtx::app`] panics with the message that names its seam, which is
    /// the honest answer: that core is not headless yet.
    pub fn route_ctx(services: FakeRouteServices) -> RouteCtx {
        RouteCtx {
            app: None,
            services: Arc::new(services),
        }
    }
}
