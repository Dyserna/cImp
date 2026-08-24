//! The offload use cases: the local `llama-server` pool, the warm MCP host,
//! and the injection-detection bundle's updater.
//!
//! ## What the A1-3 offload run found
//!
//! Eighteen commands over two managed handles ([`OffloadSupervisor`] for the
//! process pool, [`OffloadService`] for the warm MCP host and the dashboard)
//! plus one `AppState` reach (the settings snapshot the detection updater is
//! gated on). Most of the eighteen are one accessor on a handle Tauri already
//! injected, and stay one call here too. What was *not* reachable outside a
//! WebView — and is what this module exists for — is the shaping around them:
//!
//! * [`updates_allowed`] — the detection updater's three-refusal gate. Three
//!   settings states, three different sentences, and the difference between
//!   them is the difference between pointing the user at a switch that helps
//!   and one that will not. It had no test, because its only caller was a
//!   `#[tauri::command]`.
//! * [`parse_components`] / [`parse_component`] — "which component did the
//!   button name?", including the empty-string-means-all case the frontend
//!   relies on.
//! * [`test_task`] — the canned instruction the Settings "Test offload" button
//!   falls back to when its box is empty.
//! * [`derive_local_provider`] — V40 decision 26's registry lookup, whose whole
//!   point is that it REFUSES rather than picking when more than one harness
//!   declares a config writer.
//!
//! ## What did NOT move
//!
//! The two "reveal this folder" commands (`detection_open_rules_folder` and
//! its `content_open_folder` twin) stay at the wire boundary, noted there:
//! each body is a `create_dir_all` and a platform `cfg!` handing a
//! cImp-computed path to the host file manager, so there is nothing to assert
//! that does not assert `explorer.exe` — and
//! [`spawn_ledger`](crate::spawn_ledger)'s row of record names
//! `ipc/commands.rs` as their spawn site. Moving an audited spawn to buy no
//! testability is a worse trade than leaving it where the ledger says it is.
//!
//! The latch commands and `injection_status` stay too: each is one call on the
//! `AppHandle` Tauri injected, and the reason it needs that handle — the latch
//! scope resolves through the V28 live-session registry — is exactly what V42
//! #114 (loopback: latch + discovery) exists to unpick. Putting a service in
//! front of a seam that is about to move would have to be undone by it.

use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::offload::detection::updater::manifest::Component;
use crate::offload::detection::{updater, DetectionStatus};
use crate::offload::metrics::BackendDashboard;
use crate::offload::service::ServiceStatus;
use crate::offload::supervisor::{BackendStatus, StartCause, StopCause};
use crate::offload::{OffloadService, OffloadState, OffloadSupervisor};
use crate::settings::{LocalProviderBlock, Settings};

/// The local-server / backend-pool use cases, over a borrowed supervisor
/// handle.
///
/// Borrowed rather than owned for the reason
/// [`TabService`](crate::service::tabs::TabService) is: constructing one at the
/// top of an IPC command is free, and a test can hand out a reference to a
/// handle it owns on the stack.
///
/// The methods deliberately do NOT reuse the supervisor's own `status` /
/// `statuses` spelling. `spawn_gate`'s call-site scan reads a bare `.status()`
/// in a file that constructs a process as a possible ungated
/// `Command::status()`, and `ipc/commands.rs` constructs one (the folder
/// opener); two `CALL_EXEMPT` rows carried that collision. Naming each use case
/// for what it answers retires both rows instead of moving them.
pub struct OffloadServerUseCases<'a> {
    supervisor: &'a Arc<OffloadSupervisor>,
}

impl<'a> OffloadServerUseCases<'a> {
    pub fn new(supervisor: &'a Arc<OffloadSupervisor>) -> Self {
        Self { supervisor }
    }

    /// The **primary** local backend's state (the legacy single-status
    /// readout): state plus discovered `n_ctx`/slots/in-flight.
    pub async fn primary_state(&self) -> OffloadState {
        self.supervisor.status().await
    }

    /// Per-backend state for every enabled backend in the pool (Local
    /// process+health and Remote health-probe).
    pub async fn backend_states(&self) -> Vec<BackendStatus> {
        self.supervisor.statuses().await
    }

    /// Start one named Local backend (idempotent). `command_override` launches
    /// with that command instead of the configured one for this start only — it
    /// goes through the same parse/validation and is never persisted.
    pub async fn start_backend(
        &self,
        name: &str,
        command_override: Option<String>,
    ) -> AppResult<()> {
        self.supervisor
            .start_backend(name, command_override, StartCause::Ipc)
            .await
    }

    /// Stop one named Local backend (idempotent).
    pub async fn stop_backend(&self, name: &str) {
        self.supervisor.stop_backend(name, StopCause::Ipc).await;
    }

    /// Restart (Reset) one named Local backend.
    pub async fn restart_backend(&self, name: &str) -> AppResult<()> {
        self.supervisor.restart_backend(name).await
    }

    /// Start the offload `llama-server` (idempotent).
    pub async fn start_server(&self) -> AppResult<()> {
        self.supervisor.start(StartCause::Ipc).await
    }

    /// Stop the offload `llama-server` (idempotent).
    pub async fn stop_server(&self) {
        self.supervisor.stop(StopCause::Ipc).await;
    }

    /// Reset: kill + respawn with the current `server_command` (re-health,
    /// re-read `n_ctx`/`np`).
    pub async fn restart_server(&self) -> AppResult<()> {
        self.supervisor.restart().await
    }

    /// Run the canned offload task against the local server and return its
    /// answer — the Settings "Test offload" button. See [`test_task`] for what
    /// an empty instruction box sends.
    pub async fn run_test(&self, instructions: String) -> AppResult<String> {
        self.supervisor
            .run_task(
                test_task(instructions),
                crate::offload::agent::ThinkingMode::Auto,
            )
            .await
    }

    /// Buffered `llama-server` output for a backend (primary when `name` is
    /// omitted) — the read-only log panel's initial fill.
    pub fn server_log(&self, name: Option<String>) -> Vec<String> {
        self.supervisor.server_logs(name)
    }
}

/// What the "Test offload" button actually sends: the user's text when they
/// typed any, else the canned reachability probe.
///
/// Blank-but-present is the case worth a name. The box starts empty and the
/// button is live, so `""` has to mean "ask the default question" rather than
/// "run an empty task" — and whitespace-only has to mean the same thing, or a
/// stray space changes what the button does.
pub fn test_task(instructions: String) -> String {
    if instructions.trim().is_empty() {
        "Briefly confirm you are reachable and list the tools available to you.".to_string()
    } else {
        instructions
    }
}

/// The warm offload-service use cases: the MCP host and the server dashboard.
pub struct OffloadServiceUseCases<'a> {
    service: &'a Arc<OffloadService>,
}

impl<'a> OffloadServiceUseCases<'a> {
    pub fn new(service: &'a Arc<OffloadService>) -> Self {
        Self { service }
    }

    /// Aggregate offload-service state — the honest global in-flight count and
    /// the per-MCP-server health rows.
    pub async fn aggregate_state(&self) -> ServiceStatus {
        self.service.status().await
    }

    /// Reconcile the warm MCP host against the *current* settings, then report
    /// the fresh state. Cheap when the pool is already warm (unchanged servers
    /// are kept), which is what lets the Settings MCP editor call it after
    /// every add/remove/enable/disable instead of asking for a restart.
    pub async fn reload_mcp(&self) -> ServiceStatus {
        self.service.warm_host().await;
        self.service.status().await
    }

    /// Latest Offload Server dashboard snapshot, one row per enabled backend.
    /// Empty before the first poll.
    pub fn server_metrics(&self) -> Vec<BackendDashboard> {
        self.service.server_metrics()
    }

    /// Rescan `<exe-dir>/plugins/` and announce that the native tool surface
    /// may have moved — the manual **Rescan** action (V38 decision 8).
    ///
    /// **The two steps are one use case, and the second is the whole point.** A
    /// rescan can add, remove or rename every `check`-kind tool `run_check`
    /// advertises, and it writes no settings — so without the ask, nothing
    /// would tell a live session its tool list moved. It is only an ASK: the
    /// pulse gate compares the surface fingerprint and stays silent when
    /// nothing actually changed, which is why this call is unconditional here
    /// and conditional there.
    ///
    /// V42 Phase A2. A1 left this at the wire boundary and said why: the rule
    /// needs an [`OffloadService`], whose constructor took an `AppHandle`. It
    /// does not any more.
    pub async fn rescan_plugins(
        &self,
        store: Arc<crate::plugins::PluginStore>,
    ) -> AppResult<Arc<crate::plugins::PluginSet>> {
        // On the blocking pool because it walks a directory and reads every
        // file in it (`audit_refresh_census`'s precedent).
        let set = crate::service::on_blocking_pool(move || store.rescan()).await?;
        self.service.signal_native_change();
        Ok(set)
    }
}

// ── V21 / V40 Phase E: derive a harness's local-provider block ────────────

/// Derive a harness's local-provider block from a Local backend's server
/// command (the Settings "Add to OpenCode" button). Pure — parses and
/// validates only; the caller persists the returned snapshot through the
/// ordinary settings save.
///
/// **V40 Phase E (locked decision 26).** This used to call
/// `offload::server::derive_opencode_provider` — core holding one harness's
/// config writer. It asks the registry now, through
/// [`crate::harness::plugin::ConfigWriter`].
///
/// `harness` is optional for wire compatibility with the pre-V40 frontend:
/// when it is absent the registry answers, and it **refuses if more than one
/// harness declares a writer** rather than picking the first. A silently-chosen
/// harness here would write one product's provider block into another's
/// settings, which is precisely the class of defect V40 removed.
pub fn derive_local_provider(
    harness: Option<&str>,
    server_command: &str,
) -> AppResult<LocalProviderBlock> {
    let writers: Vec<crate::harness::HarnessId> = match harness.map(str::trim) {
        Some(h) if !h.is_empty() => vec![crate::harness::HarnessId::from_id(h)
            .ok_or_else(|| AppError::Offload(format!("{h:?} names no registered harness")))?],
        _ => crate::harness::registry::all()
            .filter(|h| h.plugin().is_some_and(|p| p.config_writer().is_some()))
            .collect(),
    };
    let [only] = writers[..] else {
        return Err(AppError::Offload(format!(
            "which harness should this provider be written for? {} of them accept one — name it",
            writers.len()
        )));
    };
    let writer = only
        .plugin()
        .and_then(|p| p.config_writer())
        .ok_or_else(|| {
            AppError::Offload(format!(
                "{} is not configured through a provider block cImp writes",
                only.label()
            ))
        })?;
    writer.derive_local_provider(server_command)
}

// ── V32 Phase C/C3: injection-detection status and its updater ────────────

/// How much of the injection-detection surface is actually live — signature
/// rule files loaded/failed, whether the classifier's weights are installed,
/// and (C3) the updater's installed/available versions, last check and
/// per-component modes.
///
/// `reload = true` recompiles the rules from disk first, which is what the
/// "Reload rules" button calls after the user edits a file in
/// `detection/rules.d/local/`. Both paths do blocking file I/O and (on reload)
/// a YARA compile, so they run on the blocking pool rather than the async
/// runtime's worker.
pub async fn detection_status(settings: Settings, reload: bool) -> AppResult<DetectionStatus> {
    tokio::task::spawn_blocking(move || {
        if reload {
            crate::offload::detection::reload(&settings)
        } else {
            crate::offload::detection::status(&settings)
        }
    })
    .await
    .map_err(|e| AppError::Offload(format!("detection status task failed: {e}")))
}

/// Run an update check right now for one component (or every component when
/// the caller named none), then report the refreshed status.
///
/// `apply = true` is the "Apply" button: it overrides a `check-only` mode for
/// this one run so an explicit click can take an offered update without the
/// user flipping a setting and waiting for a tick. It never overrides `off` —
/// a component the user turned off stays off.
///
/// The whole run (network + validation + swap) is awaited, because the caller
/// is a button whose next action is to re-render the result.
///
/// Refused outright when the detection feature does not resolve on (#48); see
/// [`updates_allowed`] for why the gate lives here and not only in the Svelte
/// `disabled` attribute.
pub async fn detection_check_now(
    settings: &Settings,
    component: Option<&str>,
    apply: bool,
) -> AppResult<DetectionStatus> {
    updates_allowed(settings)?;
    let components = parse_components(component)?;
    updater::run_live(&components, settings, apply).await;
    Ok(crate::offload::detection::status(settings))
}

/// Restore a component's retained previous version — the Revert button.
/// Blocking (file moves plus a YARA recompile or an `ort` session rebuild), so
/// the swap runs on the blocking pool.
///
/// Gated exactly like [`detection_check_now`] (#48): with the detection feature
/// off, the updater does not swap bundles in either direction.
pub async fn detection_revert(settings: &Settings, component: &str) -> AppResult<DetectionStatus> {
    updates_allowed(settings)?;
    let c = parse_component(component)?;
    tokio::task::spawn_blocking(move || updater::revert_live(c))
        .await
        .map_err(|e| AppError::Offload(format!("detection revert task failed: {e}")))?;
    Ok(crate::offload::detection::status(settings))
}

/// Which components a check-now names: every one when it named none,
/// otherwise exactly the one it named.
///
/// `Some("")` folds into the "all" case deliberately — the frontend sends an
/// empty string for "every component", and an empty name is not a component
/// anyone could have meant. The error names what WAS expected, because the
/// button surfaces it verbatim.
pub fn parse_components(component: Option<&str>) -> AppResult<Vec<Component>> {
    match component {
        None | Some("") => Ok(Component::ALL.to_vec()),
        Some(name) => Ok(vec![Component::parse(name).ok_or_else(|| {
            AppError::Offload(format!(
                "unknown detection component `{name}` (expected \"rules\")"
            ))
        })?]),
    }
}

/// One named component, or an error naming what was expected.
///
/// Separate from [`parse_components`] because Revert has no "all" case:
/// reverting everything is not something the surface offers, so an absent name
/// here would be a caller bug rather than a shorthand — and the two errors
/// name different expected sets, which is the reason they are two functions
/// and not one with a flag.
pub fn parse_component(component: &str) -> AppResult<Component> {
    Component::parse(component).ok_or_else(|| {
        AppError::Offload(format!(
            "unknown detection component `{component}` (expected \"rules\" or \"classifier\")"
        ))
    })
}

/// The updater's gate for the two manual commands above, resolved through the
/// same [`updater::updates_enabled`](crate::offload::detection::updater::updates_enabled)
/// the scheduler tick uses — one predicate, so a button and a tick can never
/// disagree about whether the feature is on.
///
/// An `Err` rather than a silently unchanged status: a security control that
/// does nothing when clicked, and says nothing about it, teaches the user to
/// distrust it (the same reasoning as `latch_override`'s verbatim errors).
///
/// **#48 (M-21): three refusals, because there are three states and they are
/// different statements.** The gate is unchanged — one predicate,
/// `updates_enabled`, so a button and a tick can still never disagree — but *why*
/// it said no is not always "detection is off". A worker-scope override leaves
/// this updater inert while injection detection is armed for the offload worker,
/// which keeps screening with the bundle already on disk; telling that user their
/// detection is switched off is a false claim about a running security layer, and
/// it is the claim they would act on.
///
/// The third case is M-21's residual, folded in with F-35: the **L1 master** is
/// off, which resolves detection off with it. Saying "injection detection is
/// switched off" there points the user at the wrong switch — the one they can
/// flip without effect until the master above it is back on. `SettingsApp.svelte`
/// had already added this distinction as a frontend refinement; the two surfaces
/// now single-source from the same three cases rather than the tooltip being
/// more specific than the error.
///
/// Checked in the frontend's order, which is also the only correct one: the
/// master-off case cannot collide with `worker_only_detection` (`decide`
/// short-circuits every feature to `false` with L1 off, so no scope is armed),
/// and the generic sentence keeps its parenthetical about the master because it
/// is still the fall-through for a state nobody positively identified.
///
/// **Reporting only, and asserted as such** — every branch still returns `Err`.
/// Reporting honesty must not become a new capability.
pub fn updates_allowed(settings: &Settings) -> AppResult<()> {
    if updater::updates_enabled(settings) {
        return Ok(());
    }
    if updater::worker_only_detection(settings) {
        return Err(AppError::Settings(
            "injection detection is switched off app-wide and for every AI tab, so the detection \
             updater will not check, apply or revert anything. It is still switched ON for the \
             offload worker, which keeps screening with the rule bundle already on disk — the \
             updater follows the app-wide answer, and one worker override does not start it. To \
             keep that bundle current, turn injection detection back on app-wide in \
             Settings → Injection protection."
                .to_string(),
        ));
    }
    if !crate::settings::injection::master_enabled(settings) {
        return Err(AppError::Settings(
            "injection protection is switched off at the master switch, which resolves injection \
             detection off with it — so the detection updater will not check, apply or revert \
             anything. Turn the master switch, and injection detection under it, back on in \
             Settings → Injection protection."
                .to_string(),
        ));
    }
    // Reached only when the worker's row is off too and the master is on, which
    // is what makes this sentence true rather than merely conventional.
    Err(AppError::Settings(
        "injection detection is switched off, so the detection updater will not check, apply or \
         revert anything. Turn it (and the injection-protection master above it) back on in \
         Settings → Injection protection."
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::host::testing::core_host;
    use crate::settings::SettingsHandle;

    /// A supervisor and a service over throwaway handles — **no Tauri app**.
    ///
    /// The fixture A1-3 could not write: both constructors took an `AppHandle`,
    /// so the status surface these two use cases expose was reachable only by
    /// opening the Offload pane in the running app. V42 Phase A2 replaced the
    /// handle with a [`CoreHost`](crate::service::host::CoreHost).
    ///
    /// The `_core` fields are held, not ignored: they own the receiving ends of
    /// the host's two channels (see `service::host::testing`).
    struct OffloadFixture {
        _core: crate::service::host::testing::TestCore,
        _scratch: std::path::PathBuf,
        supervisor: Arc<OffloadSupervisor>,
        service: Arc<OffloadService>,
    }

    impl Drop for OffloadFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self._scratch);
        }
    }

    impl OffloadFixture {
        fn new(settings: Settings) -> Self {
            let scratch =
                std::env::temp_dir().join(format!("cimp-offloadsvc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&scratch).expect("scratch dir");
            let handle = SettingsHandle::new(settings.clone(), settings, scratch.clone());
            let core = core_host(handle);
            let supervisor = OffloadSupervisor::new(core.host.clone());
            let service = OffloadService::new(core.host.clone(), supervisor.clone(), None);
            Self {
                _core: core,
                _scratch: scratch,
                supervisor,
                service,
            }
        }
    }

    /// **Previously "user opens the Offload pane".** The two status use cases
    /// the dashboard polls, answered by services built on the stack.
    ///
    /// What is pinned is the pair of answers that differ by one setting: the
    /// supervisor reports `Disabled` rather than `Stopped` when offload is off
    /// — the distinction the pane renders as "switched off" versus "not running
    /// yet", and the one a user reads before deciding whether to press Start —
    /// and the service's aggregate reports the configured global cap with
    /// nothing in flight and no queue.
    #[tokio::test]
    async fn the_offload_status_surfaces_answer_without_a_tauri_app() {
        let mut off = Settings::default();
        off.offload.enabled = false;
        let fixture = OffloadFixture::new(off);
        assert_eq!(
            OffloadServerUseCases::new(&fixture.supervisor)
                .primary_state()
                .await,
            OffloadState::Disabled,
            "offload off must read as Disabled, not as Stopped"
        );

        let mut on = Settings::default();
        on.offload.enabled = true;
        on.offload.global_concurrency = Some(3);
        let fixture = OffloadFixture::new(on);
        assert_eq!(
            OffloadServerUseCases::new(&fixture.supervisor)
                .primary_state()
                .await,
            OffloadState::Stopped,
            "enabled but unstarted must read as Stopped"
        );

        let status = OffloadServiceUseCases::new(&fixture.service)
            .aggregate_state()
            .await;
        assert_eq!(status.global_cap, 3, "the explicit override sizes the gate");
        assert_eq!(status.global_in_flight, 0);
        assert_eq!(status.queue_depth, 0);
        assert!(
            status.mcp_servers.is_empty(),
            "nothing was warmed, so no server can be healthy"
        );
    }

    #[test]
    fn an_empty_test_box_asks_the_canned_question() {
        assert!(test_task(String::new()).starts_with("Briefly confirm you are reachable"));
        assert!(test_task("   \n\t ".to_string()).starts_with("Briefly confirm"));
        assert_eq!(test_task("count to three".to_string()), "count to three");
        // Not trimmed: what the user typed is what the model is asked.
        assert_eq!(test_task("  hi  ".to_string()), "  hi  ");
    }

    #[test]
    fn a_named_component_wins_and_an_unknown_one_says_what_was_expected() {
        assert_eq!(parse_components(None).unwrap(), Component::ALL.to_vec());
        assert_eq!(parse_components(Some("")).unwrap(), Component::ALL.to_vec());
        assert_eq!(
            parse_components(Some("rules")).unwrap(),
            vec![Component::Rules]
        );
        let err = parse_components(Some("nope")).unwrap_err().to_string();
        assert!(err.contains("nope") && err.contains("rules"), "{err}");
        // Revert has no "all" case, so its parser rejects the empty name the
        // check-now parser accepts.
        assert!(parse_component("").is_err());
        assert_eq!(parse_component("rules").unwrap(), Component::Rules);
    }

    /// **#48 (M-21): the manual buttons' refusal names the layer that is off.**
    ///
    /// The gate is unchanged and stays app-scoped — a worker-only override does
    /// not start the updater — so both cases below still refuse. What is asserted
    /// is the sentence: a user whose offload worker is screening every fetched
    /// page must not be told their injection detection is switched off, because
    /// that is a false statement about a running security layer and it is the one
    /// they would act on.
    ///
    /// Moved here with [`updates_allowed`] itself (V42 Phase A1-3) — the
    /// assertions are unchanged; only the path to the function is.
    #[test]
    fn the_updater_refusal_does_not_call_a_running_layer_off() {
        use crate::settings::injection::{Feature, Override};

        // Detection off everywhere: the plain sentence, and it is true.
        let mut off = Settings::default();
        off.set_l2_for_test(Feature::Detection, false);
        let plain = updates_allowed(&off).expect_err("the updater is off");
        let plain = plain.to_string();
        assert!(
            plain.contains("injection detection is switched off,"),
            "{plain}"
        );
        assert!(!plain.contains("offload worker"), "nothing is running: {plain}");

        // M-21's state: off app-wide, ON for the offload worker. Still refused —
        // the scope semantics are deliberate — but for the reason that is true.
        let mut worker = off.clone();
        worker
            .set_worker_override_for_test(Feature::Detection, Override::On)
            .expect("detection has a worker row");
        assert!(
            updates_allowed(&worker).is_err(),
            "reporting honesty must not become a new capability"
        );
        let named = updates_allowed(&worker)
            .expect_err("still refused")
            .to_string();
        assert!(
            named.contains("still switched ON for the offload worker"),
            "the running layer must be named: {named}"
        );
        assert!(
            !named.contains("injection detection is switched off,"),
            "the false claim must not survive beside the true one: {named}"
        );
        // #48 F-35, M-21's residual: the THIRD state. The L1 master is off,
        // which resolves detection off with it — so "injection detection is
        // switched off" points at the wrong switch, the one the user can flip
        // with no effect until the master above it is back on. The frontend had
        // already made this distinction (`detectionUpdatesOffReason`); the two
        // surfaces now single-source from the same three cases.
        let mut master = Settings::default();
        master.set_master_for_test(false);
        assert!(
            updates_allowed(&master).is_err(),
            "reporting honesty must not become a new capability"
        );
        let l1 = updates_allowed(&master)
            .expect_err("still refused")
            .to_string();
        assert!(
            l1.contains("master switch"),
            "the switch that is actually off must be named: {l1}"
        );
        assert!(
            !l1.contains("injection detection is switched off,"),
            "the sentence that points at the wrong switch must not survive: {l1}"
        );
        assert!(
            !l1.contains("offload worker"),
            "an L1 off arms nothing anywhere: {l1}"
        );

        // All three refusals point at a section the sidebar has (F-18's tripwire
        // holds the pointer itself; this holds that the new sentences carry one).
        for r in [&plain, &named, &l1] {
            assert!(r.contains("Injection protection"), "{r}");
        }
    }

    /// Naming no harness is only unambiguous while exactly one harness declares
    /// a config writer. The refusal is the feature: writing one product's
    /// provider block into another's settings is the defect decision 26 removed.
    #[test]
    fn an_unknown_harness_is_refused_by_name() {
        let err = derive_local_provider(Some("not-a-harness"), "llama-server --port 1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not-a-harness"), "{err}");
        assert!(err.contains("no registered harness"), "{err}");
    }
}
