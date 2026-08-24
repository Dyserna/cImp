//! The per-tab delegation controls: the read-only lock, the Manual role, and
//! the facade-backend knobs.
//!
//! ## What the A1-3 delegation run found
//!
//! Three commands, and between them the two cross-tab rules the frontend
//! cannot enforce for itself — which is exactly why they are backend commands
//! and not ordinary settings fields:
//!
//! * **At most one Manual tab per harness, and setting it MOVES it.** A radio
//!   button whose group spans tabs: the previous holder drops to `None`, both
//!   writes land in ONE settings mutation, and the displaced id comes back for
//!   the toast. See [`DelegationControls::set_role`] for why it moves rather
//!   than refusing.
//! * **Lock first, persist second.** [`DelegationControls::set_read_only`]
//!   takes the runtime lock before it writes settings, so there is no window in
//!   which the UI says "read-only" and the PTY still accepts keys.
//!
//! Neither had a test. The move rule in particular is enforced inside a
//! `settings.mutate` closure, so "can a broadcast reader ever observe two
//! Manual tabs of one harness?" was a question only a running app could answer
//! — and the answer mattered enough that the closure carries a comment saying
//! so. The tests at the foot of this module answer it with a `SettingsHandle`
//! on the stack.

use crate::error::{AppError, AppResult};
use crate::settings::{
    AiToolTabConfig, DelegationBackend, DelegationRole, SettingsHandle, TabConfig,
};
use crate::state::{ReadOnlyTabs, TabId, TabKind};
use crate::tabs::TabRegistryHandle;

/// V39 Phase B: what [`DelegationControls::set_role`] did, for the UI's toast.
///
/// `displaced` is the id of the tab that LOST the Manual role to this call
/// (locked decision 8's move rule) — `None` when nothing moved. Returned rather
/// than only recorded because the losing tab may not be visible, and a role
/// that moved silently is a `delegate_task_*` tool that started driving a
/// different tab with nothing on screen saying so.
#[derive(Debug, serde::Serialize)]
pub struct RoleChange {
    pub tab: String,
    pub role: DelegationRole,
    pub displaced: Option<String>,
}

/// The per-tab delegation controls, over borrowed handles — same shape and
/// rationale as [`TabService`](crate::service::tabs::TabService).
pub struct DelegationControls<'a> {
    settings: &'a SettingsHandle,
    registry: &'a TabRegistryHandle,
    read_only: &'a ReadOnlyTabs,
}

impl<'a> DelegationControls<'a> {
    pub fn new(
        settings: &'a SettingsHandle,
        registry: &'a TabRegistryHandle,
        read_only: &'a ReadOnlyTabs,
    ) -> Self {
        Self {
            settings,
            registry,
            read_only,
        }
    }

    /// The shared precondition of all three controls: this is an AI tab, and
    /// it is one settings knows about.
    ///
    /// Both halves are needed and they fail differently. `kind()` asks the
    /// harness registry what an id means, so it rejects a Shell or Preview tab;
    /// `find_tab` rejects an AI-shaped id with no config, which is what a stale
    /// frontend or a hand-edited settings file produces. `not_an_ai_tab` is the
    /// whole second clause of the wrong-kind refusal — each control words it
    /// differently and the popover shows it verbatim, so it is passed rather
    /// than assembled.
    fn ai_tab(&self, tab: &TabId, not_an_ai_tab: &str) -> AppResult<AiToolTabConfig> {
        if tab.kind() != TabKind::AiTool {
            return Err(AppError::Ipc(format!(
                "tab `{}` is not an AI tab; {not_an_ai_tab}",
                tab.as_str()
            )));
        }
        match self.settings.current().find_tab(tab.as_str()) {
            Some(TabConfig::AiTool(cfg)) => Ok(cfg.clone()),
            _ => Err(AppError::Ipc(format!("unknown AI tab `{}`", tab.as_str()))),
        }
    }

    /// V39 Phase A: set or clear a tab's **user** read-only lock (locked
    /// decision 4's `ReadOnlySource::User`) — the Access radio in the tab's
    /// communication popover.
    ///
    /// Does two things, in this order: takes the runtime lock (so it is in
    /// force before this call returns, with no window in which the UI shows
    /// "read-only" and the PTY still accepts keys), then persists the flag so
    /// it survives a restart. The persisted write broadcasts
    /// `settings-changed`, which is how the frontend learns the new state —
    /// there is no separate event.
    ///
    /// Only ever sets `User`. The engine's `Driven` lock is not reachable from
    /// here: it belongs to a delegation's lifetime, and "Take over" (Phase B),
    /// not a radio button, is what ends one.
    pub fn set_read_only(&self, tab: &TabId, on: bool) -> AppResult<()> {
        self.ai_tab(tab, "the read-only lock applies to AI tabs only")?;
        self.read_only.set_user(tab, on);
        let id = tab.as_str().to_string();
        self.settings.mutate(move |snap| {
            if let Some(TabConfig::AiTool(cfg)) = snap.find_tab_mut(&id) {
                cfg.read_only = on;
            }
        });
        Ok(())
    }

    /// V39 Phase B (locked decision 8): set a tab's delegation role, enforcing
    /// **at most one Manual tab per harness**.
    ///
    /// The move rule, and why it is a move rather than a refusal: the user is
    /// choosing which tab `delegate_task_<harness>` drives, and there is
    /// exactly one answer per harness. A refusal ("tab A already holds it")
    /// would make the user go and clear A first for no reason anyone benefits
    /// from — so this is a radio button whose group spans tabs. The previous
    /// holder drops to `None`, both writes land in ONE settings mutation (so no
    /// broadcast can observe two Manual tabs of one harness), an Events row
    /// records the move, and the displaced id comes back for the toast.
    ///
    /// Refusals, each naming its condition: a reserved dashboard (no PTY, no
    /// harness), a tab that is not a configured AI tab, and a harness with no
    /// input profile (it could never be driven, so a role on it would be a
    /// control that does nothing).
    ///
    /// **Not spawn-baked** (decision 15). The persisted write broadcasts
    /// `settings-changed`, which is also what asks the offload service for a
    /// `tools/list_changed` pulse — `graph::native_surface_sig` hashes the
    /// Manual set, so the gate sees the move and every live session re-lists on
    /// its next turn without a restart.
    pub async fn set_role(&self, tab: &TabId, role: DelegationRole) -> AppResult<RoleChange> {
        if tab.is_reserved_dashboard() {
            return Err(AppError::Ipc(format!(
                "tab `{}` is an app-rendered dashboard, not a harness tab; it has no delegation \
                 role",
                tab.as_str()
            )));
        }
        let cfg = self.ai_tab(tab, "delegation roles apply to AI tabs only")?;
        let Some(agent) = crate::tabs::tab_consumer(&cfg) else {
            // V40 Phase A (locked decision 2): a tab whose command names no
            // registered harness is not a worker at all. It used to be
            // classified as OpenCode here, become eligible for that harness's
            // Manual slot, and be typed into with OpenCode's paste rules.
            return Err(AppError::Ipc(format!(
                "tab `{}` runs no registered harness, so cImp has no way to type a turn into \
                 it - it cannot hold a delegation role",
                tab.as_str()
            )));
        };
        if crate::harness::input_profile(agent).is_none() {
            return Err(AppError::Ipc(format!(
                "tab `{}` runs a harness with no input profile, so cImp could never type a turn \
                 into it — it cannot hold a delegation role",
                tab.as_str()
            )));
        }

        // Who currently holds Manual for this harness, if anyone. Read before
        // the mutation so the row and the return value name the same tab the
        // mutation is about to clear.
        let displaced: Option<(String, String)> = if role == DelegationRole::Manual {
            self.settings
                .current()
                .tabs
                .iter()
                .find_map(|t| match t {
                    TabConfig::AiTool(c)
                        if c.delegation_role == DelegationRole::Manual
                            && c.id != tab.as_str()
                            && crate::tabs::tab_consumer(c) == Some(agent) =>
                    {
                        Some((c.id.clone(), c.name.clone()))
                    }
                    _ => None,
                })
        } else {
            None
        };

        let id = tab.as_str().to_string();
        let losing = displaced.as_ref().map(|(id, _)| id.clone());
        let agent_for_mutate = agent;
        self.settings.mutate(move |snap| {
            // ONE mutation for both writes: a snapshot in which two tabs of one
            // harness hold Manual must never be observable by a broadcast reader.
            for t in snap.tabs.iter_mut() {
                let TabConfig::AiTool(c) = t else { continue };
                if c.id == id {
                    c.delegation_role = role;
                } else if role == DelegationRole::Manual
                    && c.delegation_role == DelegationRole::Manual
                    && crate::tabs::tab_consumer(c) == Some(agent_for_mutate)
                {
                    c.delegation_role = DelegationRole::None;
                }
            }
        });

        if let Some((lost_id, lost_name)) = &displaced {
            let taker = {
                let registry = self.registry.lock().await;
                registry
                    .name_of(tab)
                    .unwrap_or_else(|| tab.as_str().to_string())
            };
            crate::delegation::record_row(
                crate::delegation::transition::ROLE_MOVED,
                lost_name,
                Some(&format!(
                    "the Manual role for this harness moved to `{taker}`"
                )),
                agent,
                Some(tab.as_str()),
                true,
                0,
                String::new(),
                String::new(),
            );
            tracing::info!(from = %lost_id, to = %tab.as_str(), harness = %agent, "delegation: Manual role moved");
        }

        Ok(RoleChange {
            tab: tab.as_str().to_string(),
            role,
            displaced: losing,
        })
    }

    /// **Write one tab's facade-backend knobs, and nothing else** (V39 review
    /// M-10).
    ///
    /// The popover used to save these through the ordinary whole-document
    /// `applySettings`: read the store, patch three fields, send the entire
    /// `Settings`. That is the `40d2b32` lost-update shape — a document written
    /// from a snapshot taken before some other write landed silently reverts it
    /// — and the write most likely to be in flight beside it is the ROLE radio
    /// one method above, which goes through [`set_role`](Self::set_role)
    /// precisely because only the backend can enforce its cross-tab rule.
    /// Typing a backend name could put the role back.
    ///
    /// So: one call, three fields, `settings.mutate` (which composes with a
    /// concurrent mutation instead of overwriting the document).
    ///
    /// **The role is deliberately not touched**, and neither is anything else
    /// on the tab: a user who sets a name, switches the role away and switches
    /// it back finds the knobs where they left them.
    pub fn set_backend(&self, tab: &TabId, backend: DelegationBackend) -> AppResult<()> {
        self.ai_tab(tab, "delegation backends are configured on AI tabs only")?;
        let backend = normalise_backend(backend);
        let id = tab.as_str().to_string();
        self.settings.mutate(move |snap| {
            if let Some(TabConfig::AiTool(cfg)) = snap.find_tab_mut(&id) {
                apply_backend_patch(cfg, backend);
            }
        });
        Ok(())
    }
}

/// The two "blank means unset" rules, at the parse boundary rather than at
/// every reader: a cleared text field arrives as `""` and a cleared number
/// field as `0`, and both mean "use the default".
pub(crate) fn normalise_backend(mut backend: DelegationBackend) -> DelegationBackend {
    backend.name = backend
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    backend.declared_context = backend.declared_context.filter(|n| *n > 0);
    backend
}

/// Write the knobs onto one tab's config. **Only** the knobs — separated out so
/// a test can state that.
pub(crate) fn apply_backend_patch(cfg: &mut AiToolTabConfig, backend: DelegationBackend) {
    cfg.delegation_backend = backend;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use crate::state::StateSignal;
    use tokio::sync::mpsc;

    /// **The facade knobs are a NARROW write** (V39 review M-10).
    ///
    /// The popover's old path sent the whole `Settings` document, which can
    /// revert a role change that landed after its snapshot was taken (the
    /// `40d2b32` class). What replaces it must touch the three knobs and
    /// nothing else — least of all `delegation_role`, whose cross-tab rule
    /// only `tab_set_delegation_role` enforces.
    #[test]
    fn the_backend_patch_touches_the_knobs_and_nothing_else() {
        use crate::settings::{BackendTier, DelegationBackend, DelegationRole, TabConfig};
        let mut tab = crate::settings::default_claude_tab();
        let TabConfig::AiTool(cfg) = &mut tab else {
            panic!("an AI tab");
        };
        cfg.delegation_role = DelegationRole::Manual;
        cfg.read_only = true;
        cfg.name = "api-work".to_string();
        let before = cfg.clone();

        apply_backend_patch(
            cfg,
            DelegationBackend {
                name: Some("lan-worker-2".to_string()),
                tier: BackendTier::Fast,
                declared_context: Some(128_000),
            },
        );

        assert_eq!(cfg.delegation_backend.name.as_deref(), Some("lan-worker-2"));
        assert_eq!(cfg.delegation_backend.tier, BackendTier::Fast);
        assert_eq!(cfg.delegation_backend.declared_context, Some(128_000));
        assert_eq!(
            cfg.delegation_role, before.delegation_role,
            "the role is the one field a knob write must never move"
        );
        assert_eq!(cfg.read_only, before.read_only);
        assert_eq!(cfg.name, before.name);
        assert_eq!(cfg.command, before.command);
    }

    /// Blank is unset, at the boundary: a cleared text field arrives as `""`
    /// and a cleared number field as `0`.
    #[test]
    fn a_cleared_knob_is_stored_as_absent_not_as_blank() {
        use crate::settings::DelegationBackend;
        let out = normalise_backend(DelegationBackend {
            name: Some("   ".to_string()),
            declared_context: Some(0),
            ..Default::default()
        });
        assert_eq!(out.name, None);
        assert_eq!(out.declared_context, None);
        let kept = normalise_backend(DelegationBackend {
            name: Some("  lan-worker-2 ".to_string()),
            declared_context: Some(64_000),
            ..Default::default()
        });
        assert_eq!(kept.name.as_deref(), Some("lan-worker-2"));
        assert_eq!(kept.declared_context, Some(64_000));
    }

    /// Everything [`DelegationControls`] borrows, owned on the stack, seeded
    /// with two tabs of the same harness so the move rule has something to
    /// move.
    struct Fixture {
        settings: SettingsHandle,
        registry: TabRegistryHandle,
        read_only: ReadOnlyTabs,
        _signals: mpsc::Sender<StateSignal>,
        _rx: mpsc::Receiver<StateSignal>,
        _scratch: ScratchDir,
    }

    /// A throwaway settings directory, for the same reason
    /// [`crate::service::tabs`]'s tests have one: these tests DO mutate
    /// settings, so the debounced saver must write somewhere disposable.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("cimp-delegsvc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The harness every seeded tab runs. Taken from the registry rather than
    /// spelled, so this fixture does not pin one product's id: what the move
    /// rule is about is "same harness", and the test needs any harness that
    /// HAS an input profile — a harness cImp cannot type into refuses a role
    /// outright, which is a different rule.
    fn drivable_harness() -> Option<&'static str> {
        crate::harness::registry::all()
            .filter_map(|h| h.id())
            .find(|id| crate::harness::input_profile(id).is_some())
    }

    impl Fixture {
        // `AiToolTabConfig` has private fields (the injection overrides), so a
        // struct literal with `..Default::default()` does not compile here and
        // the seed is built by assignment instead.
        #[allow(clippy::field_reassign_with_default)]
        fn new(harness: &'static str) -> Self {
            use crate::state::{TabKind, TabMeta};
            let scratch = ScratchDir::new();
            let mut defaults = Settings::default();
            defaults.tabs = ["ai-one", "ai-two"]
                .into_iter()
                .map(|id| {
                    let mut cfg = AiToolTabConfig::default();
                    cfg.id = id.to_string();
                    cfg.name = id.to_string();
                    cfg.command = harness.to_string();
                    TabConfig::AiTool(cfg)
                })
                .collect();
            let settings = SettingsHandle::new(defaults.clone(), defaults, scratch.0.clone());
            let (signals, rx) = mpsc::channel::<StateSignal>(16);
            let seed = TabId::from_str("ai-one");
            let registry = std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::tabs::TabRegistry::new(
                    vec![
                        TabMeta {
                            id: TabId::from_str("ai-one"),
                            kind: TabKind::AiTool,
                            name: "ai-one".to_string(),
                        },
                        TabMeta {
                            id: TabId::from_str("ai-two"),
                            kind: TabKind::AiTool,
                            name: "ai-two".to_string(),
                        },
                    ],
                    seed.clone(),
                    std::sync::Arc::new(std::sync::RwLock::new(seed)),
                    signals.clone(),
                    std::sync::Arc::new(Vec::new()),
                ),
            ));
            Self {
                settings,
                registry,
                read_only: ReadOnlyTabs::default(),
                _signals: signals,
                _rx: rx,
                _scratch: scratch,
            }
        }

        fn controls(&self) -> DelegationControls<'_> {
            DelegationControls::new(&self.settings, &self.registry, &self.read_only)
        }

        fn role_of(&self, id: &str) -> DelegationRole {
            match self.settings.current().find_tab(id) {
                Some(TabConfig::AiTool(c)) => c.delegation_role,
                _ => panic!("{id} is not an AI tab"),
            }
        }
    }

    /// **Manual is a radio button whose group spans tabs** (V39 locked
    /// decision 8).
    ///
    /// Previously "click Manual on tab B in the app and check tab A's popover":
    /// setting Manual on a second tab of the same harness MOVES it — the
    /// previous holder drops to `None`, and the displaced id comes back so the
    /// toast can name a tab that may not be on screen.
    ///
    /// The invariant underneath is the one the mutation's comment states: at no
    /// point may two tabs of one harness both read Manual. Asserted after the
    /// move rather than during it, because a single `settings.mutate` is what
    /// makes "during" unobservable — which is the property.
    #[tokio::test]
    async fn manual_moves_between_tabs_of_one_harness_and_never_doubles_up() {
        let Some(harness) = drivable_harness() else {
            return; // no drivable harness in this build; nothing to move
        };
        let f = Fixture::new(harness);

        let first = f
            .controls()
            .set_role(&TabId::from_str("ai-one"), DelegationRole::Manual)
            .await
            .expect("the first Manual claims a free slot");
        assert_eq!(first.displaced, None, "nothing held it yet");
        assert_eq!(f.role_of("ai-one"), DelegationRole::Manual);

        let moved = f
            .controls()
            .set_role(&TabId::from_str("ai-two"), DelegationRole::Manual)
            .await
            .expect("the second Manual moves it");
        assert_eq!(
            moved.displaced.as_deref(),
            Some("ai-one"),
            "the toast has to be able to name the tab that lost it"
        );
        assert_eq!(f.role_of("ai-two"), DelegationRole::Manual);
        assert_eq!(
            f.role_of("ai-one"),
            DelegationRole::None,
            "two Manual tabs of one harness must never be observable"
        );

        // Clearing is not a move: nothing is displaced by dropping to None.
        let cleared = f
            .controls()
            .set_role(&TabId::from_str("ai-two"), DelegationRole::None)
            .await
            .expect("clearing");
        assert_eq!(cleared.displaced, None);
        assert_eq!(f.role_of("ai-two"), DelegationRole::None);
    }

    /// **A dashboard has no delegation role, and the refusal says which
    /// condition it hit.** The popover shows these strings verbatim, so a
    /// generic one leaves the user with a control that did nothing and no
    /// reason why.
    #[tokio::test]
    async fn the_refusals_name_their_condition() {
        let Some(harness) = drivable_harness() else {
            return;
        };
        let f = Fixture::new(harness);

        let dash = f
            .controls()
            .set_role(&TabId::Workbench, DelegationRole::Manual)
            .await
            .expect_err("a dashboard is not a harness tab");
        assert!(dash.to_string().contains("app-rendered dashboard"), "{dash}");

        let unknown = f
            .controls()
            .set_role(&TabId::from_str("ai-nope"), DelegationRole::Manual)
            .await
            .expect_err("an id settings does not know");
        assert!(unknown.to_string().contains("unknown AI tab"), "{unknown}");

        let wrong_kind = f
            .controls()
            .set_read_only(&TabId::Shell("shell-1".to_string()), true)
            .expect_err("a shell has no read-only lock");
        assert!(
            wrong_kind.to_string().contains("is not an AI tab"),
            "{wrong_kind}"
        );
    }

    /// **The lock is in force before the call returns.** The runtime map is
    /// what `PtyService::write` consults, and it is written BEFORE the
    /// persisted flag — a settings save is debounced, and a window in which the
    /// UI says read-only while the PTY still takes keys is the whole thing this
    /// ordering prevents.
    #[tokio::test]
    async fn the_runtime_lock_is_set_before_the_call_returns() {
        let Some(harness) = drivable_harness() else {
            return;
        };
        let f = Fixture::new(harness);
        let tab = TabId::from_str("ai-one");

        f.controls().set_read_only(&tab, true).expect("lock");
        assert!(
            f.read_only.read_only(&tab).is_some(),
            "the runtime lock must be in force on return, not after a save"
        );
        match f.settings.current().find_tab("ai-one") {
            Some(TabConfig::AiTool(c)) => assert!(c.read_only, "and it must survive a restart"),
            _ => panic!("seed tab lost"),
        }

        f.controls().set_read_only(&tab, false).expect("unlock");
        assert!(f.read_only.read_only(&tab).is_none());
    }
}
