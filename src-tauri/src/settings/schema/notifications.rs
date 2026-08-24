//! Notification slots, and the seeded builtin tabs they are seeded into.
//!
//! Split out of `schema.rs` by V42 R10; see the module docs in `mod.rs`.

use super::*;

/// One notification slot: a per-event `{ enabled, text }` pair. The
/// firing path requires both `enabled == true` AND a non-empty `text`
/// to dispatch — the empty-text suppression matches the pre-v1.11
/// convention so users who hand-edit a slot to `""` still see it
/// disabled.
///
/// Custom `Deserialize` accepts either a bare string (the v1.10-and-
/// earlier shape — empty string maps to `enabled: false`, non-empty to
/// `enabled: true`) or the v1.11 object shape, so a legacy file loads
/// without losing the user's text. On next save the file is rewritten
/// in the new shape.
#[derive(Clone, Serialize, Debug, Default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct NotificationSlot {
    pub enabled: bool,
    pub text: String,
}

impl NotificationSlot {
    /// A configured-and-enabled slot. Constructor for the builtin tab
    /// defaults so the call sites stay terse.
    pub fn enabled(text: impl Into<String>) -> Self {
        Self {
            enabled: true,
            text: text.into(),
        }
    }
}

impl<'de> Deserialize<'de> for NotificationSlot {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            // v1.10-and-earlier shape: bare string. Empty string was the
            // documented "leave blank to disable" convention, so map it
            // to `enabled: false`. Non-empty maps to `enabled: true` so
            // the upgrade path preserves prior firing behavior.
            serde_json::Value::String(s) => Ok(Self {
                enabled: !s.is_empty(),
                text: s,
            }),
            serde_json::Value::Object(_) => {
                #[derive(Deserialize)]
                struct Inner {
                    #[serde(default = "default_true")]
                    enabled: bool,
                    #[serde(default)]
                    text: String,
                }
                fn default_true() -> bool {
                    true
                }
                let inner: Inner = serde_json::from_value(v).map_err(D::Error::custom)?;
                Ok(Self {
                    enabled: inner.enabled,
                    text: inner.text,
                })
            }
            serde_json::Value::Null => Ok(Self::default()),
            _ => Err(D::Error::custom(
                "notification slot: expected string or { enabled, text } object",
            )),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct AiNotificationConfig {
    pub idle: NotificationSlot,
    pub awaiting_permission: NotificationSlot,
    /// Spoken when a `kind: question` pattern fires (AskUserQuestion-style
    /// multi-option prompts). Older settings files that pre-date this
    /// field deserialize to a default-disabled slot via
    /// `#[serde(default)]`; the integrity check at load doesn't backfill
    /// it, so users on the two AI builtins get the configured-defaults
    /// experience only on fresh installs. (See `default_claude_tab` and
    /// `default_claude_local_tab`.)
    pub question: NotificationSlot,
    pub error: NotificationSlot,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct ShellNotificationConfig {
    pub error: NotificationSlot,
    /// `{code}` placeholder is interpolated with the actual exit code in M4.
    pub exited: NotificationSlot,
}

impl Default for ShellNotificationConfig {
    fn default() -> Self {
        Self {
            error: NotificationSlot::enabled("Shell encountered an error"),
            exited: NotificationSlot::enabled("Shell exited (code {code})"),
        }
    }
}

// --- Builtin defaults -------------------------------------------------------
//
// Used by:
//   1. The migration step to fill in missing entries (e.g. a claude-local
//      tab absent from an upgraded settings file).
//   2. The integrity check at load time to restore deleted builtins.
//   3. `Settings::default()` to seed a fresh-install file before the first
//      save.

/// **The seeded config for ONE reserved AI tab, built from its declaration**
/// (V40 Phase I, issue #107 item 1).
///
/// This used to be three near-identical hand-written constructors plus a
/// `default_ai_tab` `match` over the three `AiTabId` variants — so a fourth
/// reserved tab needed a fourth constructor, and a third harness's tab had no
/// arm to be seeded from at all. The four things that actually differed between
/// them are the [`crate::harness::registry::BuiltinTab`] row's fields; every
/// other field below was already identical for all three, and the output for
/// `claude` / `claude-local` / `opencode` is byte-identical to what the three
/// constructors produced (pinned by `the_seeded_builtins_are_unchanged`).
///
/// The notification prose is seeded with the `{tab}` PLACEHOLDER — "{tab} is
/// idle" — which [`crate::notifications`] resolves to the tab's *current*
/// display name when the announcement is spoken. It used to be the name baked
/// in at seed time ("Claude is idle", "OpenCode is idle"), which went stale the
/// moment a tab was renamed or duplicated — a "Claude 2" tab clones this config
/// and would announce "Claude is idle". Schema step 37 → 38 rewrites the baked
/// form in existing files; the placeholder makes the clone correct by
/// construction.
fn ai_tab_from_spec(spec: &'static crate::harness::registry::BuiltinTab) -> TabConfig {
    let name = spec.name;
    TabConfig::AiTool(AiToolTabConfig {
        id: spec.id.to_string(),
        builtin: true,
        name: name.to_string(),
        command: spec.command.to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        // Every shipped AI harness accepts an instructions channel of some kind
        // (Claude's `--append-system-prompt`, OpenCode's
        // `OPENCODE_CONFIG_CONTENT`), so the TTS-markup convention applies and a
        // freshly seeded tab can speak. A harness that cannot take one leaves
        // the toggle on and injects nothing, which is inert rather than wrong.
        tts_injection: TtsInjection { enabled: true },
        notifications: AiNotificationConfig {
            idle: NotificationSlot::enabled("{tab} is idle".to_string()),
            awaiting_permission: NotificationSlot::enabled(
                "{tab} is awaiting permission".to_string(),
            ),
            question: NotificationSlot::enabled("{tab} has a question".to_string()),
            error: NotificationSlot::enabled("{tab} encountered an error".to_string()),
        },
        // Pre-dismissed so the overlay code can use a single per-tab
        // predicate. Aider used to fire a first-launch banner (V1.1)
        // but no AI builtin has since.
        first_launch_notice_dismissed: true,
        theme_override: None,
        background_override: None,
        use_local_provider: spec.local_provider,
        // V39 Phase A: a fresh tab accepts the keyboard. The read-only lock
        // is a deliberate user action, never a default.
        read_only: false,
        // V39 Phase B: a fresh tab is nobody's delegation target. Both roles
        // are opt-in, per tab, from that tab's own popover.
        delegation_role: DelegationRole::None,
        delegation_backend: DelegationBackend::default(),
        // V39: a newly created AI tab starts with every tab-scoped injection
        // control explicitly OFF. L1 and every L2 ship on; the per-tab row is
        // the switch the user reaches for, from this tab's shield badge. NOT
        // `Default::default()` — that is all-`Inherit`, which is what an
        // ABSENT cell in an existing settings file must keep meaning (schema
        // step 34 → 35).
        injection_overrides: crate::settings::injection::TabInjectionOverrides::all_off(),
    })
}

/// The seeded `claude` tab.
///
/// Kept as a named function because the **frozen** schema migrations construct
/// it by name (`settings::migration`'s v1 → v2 step embeds its JSON), and a
/// frozen step must keep saying what it always said. New code asks
/// [`default_ai_tab`] with the id it already holds.
pub fn default_claude_tab() -> TabConfig {
    default_ai_tab_by_id(CLAUDE_TAB_ID).expect("`claude` is a registered reserved tab")
}

/// V1.4-07: second Claude tab, preconfigured to talk to a local LLM
/// via the global `claude_local` provider settings. Replaces the
/// pre-V1.4-07 Aider builtin tab.
pub fn default_claude_local_tab() -> TabConfig {
    default_ai_tab_by_id(CLAUDE_LOCAL_TAB_ID).expect("`claude-local` is a registered reserved tab")
}

/// V19: OpenCode AI-tool tab using whatever provider OpenCode's own config
/// selects (cloud / API keys / project config) when `use_local_provider` is
/// off. TTS prompt injection is enabled by default: OpenCode accepts an
/// instructions file (injected via `OPENCODE_CONFIG_CONTENT`), so it honors
/// the TTS-markup convention and the tab can speak.
pub fn default_opencode_tab() -> TabConfig {
    default_ai_tab_by_id(OPENCODE_TAB_ID).expect("`opencode` is a registered reserved tab")
}

/// **TEST-ONLY**: one of the builtin AI tabs with its L3 injection row reset to
/// all-`Inherit`.
///
/// V39 ships a newly created tab with every tab-scoped injection cell `Off`
/// (`injection::TabInjectionOverrides::all_off`), which is the right posture for
/// a real tab and the wrong fixture for a test about the RESOLUTION RULE: a row
/// that already states every cell answers "off, decided at L3" before the rule
/// under test is reached. All-`Inherit` is also a real shape — it is exactly
/// what schema step 34 → 35 writes into every tab that predates V39, i.e. what
/// every upgraded install carries.
///
/// Lives here because `AiToolTabConfig::injection_overrides` is
/// `pub(in crate::settings)`: a test in `tabs::config` or `offload::loopback`
/// cannot reach the field, and the boundary that makes that true is worth more
/// than the convenience of a local fixture.
#[cfg(test)]
pub(crate) fn ai_tab_inheriting_injection(tab: TabConfig) -> TabConfig {
    let mut tab = tab;
    if let TabConfig::AiTool(c) = &mut tab {
        c.injection_overrides = crate::settings::injection::TabInjectionOverrides::default();
    }
    tab
}

/// Look up the default `TabConfig` for a reserved AI tab id. Used by
/// the integrity check and the lifecycle IPC when materializing a tab
/// the user just enabled.
/// **Test fixture** — one AI tab holding the V39 Remote-offload role, i.e. one
/// facade backend.
///
/// Lives beside the tab constructors rather than in a `mod tests`, because
/// three modules' tests need it (the pool, the cap, the child's prose) and a
/// fixture copied three times is three fixtures that can disagree about what a
/// facade tab looks like. Built from the default Claude tab so it carries no
/// harness literal of its own.
#[cfg(test)]
pub(crate) fn facade_tab(id: &str, backend_name: &str) -> TabConfig {
    let mut tab = default_claude_tab();
    if let TabConfig::AiTool(c) = &mut tab {
        c.id = id.to_string();
        c.builtin = false;
        c.name = format!("tab {id}");
        c.delegation_role = DelegationRole::RemoteOffload;
        c.delegation_backend = DelegationBackend {
            name: (!backend_name.is_empty()).then(|| backend_name.to_string()),
            tier: BackendTier::Quality,
            declared_context: None,
        };
    }
    tab
}

pub fn default_ai_tab(id: AiTabId) -> TabConfig {
    ai_tab_from_spec(id.spec())
}

/// The same, from a raw id — `None` for a string no descriptor claims.
///
/// The IPC "reset this tab to defaults" command and the three named
/// constructors above go through here; a caller that already holds an
/// [`AiTabId`] uses [`default_ai_tab`], which cannot fail.
pub fn default_ai_tab_by_id(id: &str) -> Option<TabConfig> {
    crate::harness::registry::builtin_tab(id).map(ai_tab_from_spec)
}

/// V9-01: the reserved, non-closable Code Graph monitor tab. A Shell-kind
/// entry with no command (never PTY-backed — its content is an app-rendered
/// dashboard of the graph indexer/embedder). Materialized/removed by the
/// integrity check per `graph.enabled`.
pub fn default_graph_monitor_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: GRAPH_MONITOR_TAB_ID.to_string(),
        builtin: true,
        name: "Code Intelligence".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// V13 Phase A: the reserved, non-closable Workbench tab. Same shape as the
/// Code Graph monitor tab — Shell-kind with no command (app-rendered, no
/// PTY). Materialized/removed by the integrity check per `workbench.enabled`.
pub fn default_workbench_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: WORKBENCH_TAB_ID.to_string(),
        builtin: true,
        name: "Workbench".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// The reserved, non-closable Tools tab (formerly "Tool Activity" — the
/// rename reaches existing installs via `sync_name`). Same shape as the Code
/// Graph monitor tab — Shell-kind with no command (app-rendered, no PTY).
/// Materialized/removed by the integrity check per `ui.tool_activity_tab`.
pub fn default_tool_activity_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: TOOL_ACTIVITY_TAB_ID.to_string(),
        builtin: true,
        name: "Tools".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// #51: the reserved, non-closable Events tab. Same shape as the Tool Activity
/// tab — Shell-kind with no command (app-rendered, no PTY).
/// Materialized/removed by the integrity check per `ui.events_tab`.
pub fn default_events_tab() -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: EVENTS_TAB_ID.to_string(),
        builtin: true,
        name: "Events".to_string(),
        command: String::new(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

/// Default Shell-1 entry. Takes the resolved platform default shell so the
/// `command` and `args` fields land on the right binary for the host. The
/// reserved id is just the seed value for the first shell tab on a fresh
/// install — it's a regular closable shell, not a builtin.
pub fn default_shell_1_tab(default_shell: &ShellSpec) -> TabConfig {
    TabConfig::Shell(ShellTabConfig {
        id: SHELL_DEFAULT_TAB_ID.to_string(),
        builtin: false,
        name: "Shell 1".to_string(),
        command: default_shell.command.to_string_lossy().into_owned(),
        args: default_shell.args.clone(),
        cwd: None,
        env: HashMap::new(),
        notifications: ShellNotificationConfig::default(),
        theme_override: None,
        background_override: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The three shipped built-ins are seeded byte-identically** (V40 Phase
    /// I).
    ///
    /// `default_ai_tab` is one generic constructor over the descriptor's
    /// `BuiltinTab` row now, where it used to be three hand-written functions.
    /// The seeded config is what lands in a fresh install's `settings.json` and
    /// what the frozen v1 → v2 migration embeds, so "generic" has to mean
    /// "identical", not "close enough".
    #[test]
    fn the_seeded_builtins_are_unchanged() {
        let expect = |id: &str, name: &str, command: &str, local: bool| {
            let TabConfig::AiTool(c) = default_ai_tab(ai_tab_id(id)) else {
                panic!("{id} is not an AI-tool tab");
            };
            assert_eq!(c.id, id);
            assert_eq!(c.name, name);
            assert_eq!(c.command, command);
            assert_eq!(c.use_local_provider, local);
            assert!(c.builtin);
            assert!(c.tts_injection.enabled);
            assert!(c.first_launch_notice_dismissed);
            assert!(c.args.is_empty() && c.cwd.is_none() && c.env.is_empty());
            // The prose is name-INDEPENDENT since schema 38: `{tab}` resolves
            // to the tab's live display name when the announcement is spoken,
            // so a rename or a duplicate stays correct without a reseed.
            assert_eq!(c.notifications.idle.text, "{tab} is idle");
            assert_eq!(
                c.notifications.awaiting_permission.text,
                "{tab} is awaiting permission"
            );
            assert_eq!(c.notifications.question.text, "{tab} has a question");
            assert_eq!(c.notifications.error.text, "{tab} encountered an error");
        };
        expect("claude", "Claude", "claude", false);
        expect("claude-local", "Claude (custom provider)", "claude", true);
        expect("opencode", "OpenCode", "opencode", false);
        // The named constructors the frozen migrations call still answer the
        // same thing as the generic one.
        // `TabConfig` has no `PartialEq`; its JSON is the shape that matters
        // anyway, since that is what lands on disk.
        let same = |a: TabConfig, b: TabConfig| {
            assert_eq!(
                serde_json::to_value(&a).unwrap(),
                serde_json::to_value(&b).unwrap()
            );
        };
        same(default_claude_tab(), default_ai_tab(ai_tab_id("claude")));
        same(
            default_claude_local_tab(),
            default_ai_tab(ai_tab_id("claude-local")),
        );
        same(default_opencode_tab(), default_ai_tab(ai_tab_id("opencode")));
        // `uses_local_provider` is the descriptor's field, not a `matches!`.
        assert!(ai_tab_id("claude-local").uses_local_provider());
        assert!(!ai_tab_id("claude").uses_local_provider());
        assert!(!ai_tab_id("opencode").uses_local_provider());
    }
}
