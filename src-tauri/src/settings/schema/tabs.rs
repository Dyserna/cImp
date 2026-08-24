//! Session state, the persisted layout tree, and the per-tab configs.
//!
//! Split out of `schema.rs` by V42 R10; see the module docs in `mod.rs`.

use super::*;

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct SessionState {
    pub active_tab_id: Option<String>,
}

/// Persisted layout state. Mirrors the frontend's `LayoutState` 1:1 — the
/// `type` discriminator on `LayoutNodePersisted` matches the frontend's
/// `'split' | 'pane'` shape, so serialize/deserialize is identity work
/// across the IPC boundary.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct LayoutPersisted {
    // HAND-KEPT SEAM (V42 Phase E, locked decision 7): the frontend's
    // `LayoutNode` union stays authoritative — the layout engine constructs
    // and rewrites those nodes, while `LayoutNodePersisted` only round-trips
    // them. Pointing at it keeps ONE union in the codebase instead of two.
    #[cfg_attr(test, ts(type = "import('../../layout/types').LayoutNode"))]
    pub tree: LayoutNodePersisted,
    pub focused_pane_id: String,
}

/// Recursive layout-tree node. Splits are internal (two children + ratio +
/// direction); panes are leaves (ordered tab id list + per-pane active tab
/// id).
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNodePersisted {
    Split {
        id: String,
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNodePersisted>,
        second: Box<LayoutNodePersisted>,
    },
    Pane {
        id: String,
        tab_ids: Vec<String>,
        active_tab_id: Option<String>,
    },
}

/// Direction of a Split node. Naming matches CSS flexbox: `Horizontal`
/// arranges children side-by-side (vertical splitter between them);
/// `Vertical` stacks them top-to-bottom. See DESIGN.md for the
/// rationale for this convention.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// A named layout preset. The tree is the layout-only payload — focus and
/// the live `focused_pane_id` are intentionally not persisted with the
/// preset, since restoring a preset is "set up panes this way" and focus
/// follows the user's next click.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct LayoutPreset {
    pub name: String,
    /// RFC 3339 / ISO 8601 timestamp (UTC, second precision). Used to
    /// order the popover's "Recent presets" list. Renames do not refresh
    /// this — it remains the original creation time.
    pub created_at: String,
    // HAND-KEPT SEAM: see `LayoutPersisted::tree`.
    #[cfg_attr(test, ts(type = "import('../../layout/types').LayoutNode"))]
    pub tree: LayoutNodePersisted,
}

/// Tolerant deserializer for `Settings::layout`. Parses to a generic
/// `Value` first, then attempts the typed conversion; any failure (a
/// malformed/partial node, a `Split` missing `ratio`, a hand-edit that
/// broke the tree) degrades to `None` with a warning instead of failing
/// the whole `Settings` parse. The frontend rebuilds a default single-pane
/// tree when the layout is `None`, so the user loses only the broken layout
/// — not their entire per-folder overlay.
pub(super) fn deserialize_lenient_layout<'de, D>(d: D) -> Result<Option<LayoutPersisted>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(d)?;
    match raw {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(val) => match serde_json::from_value::<LayoutPersisted>(val) {
            Ok(layout) => Ok(Some(layout)),
            Err(e) => {
                tracing::warn!(error = %e, "settings: malformed layout dropped to None");
                Ok(None)
            }
        },
    }
}

/// Tolerant deserializer for `Settings::layout_presets`. Drops individual
/// malformed presets (keeping the valid ones) and tolerates the field not
/// being an array at all, rather than aborting the entire settings load.
pub(super) fn deserialize_lenient_presets<'de, D>(d: D) -> Result<Vec<LayoutPreset>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(d)?;
    let serde_json::Value::Array(items) = raw else {
        if !raw.is_null() {
            tracing::warn!("settings: layout_presets was not an array; ignoring");
        }
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match serde_json::from_value::<LayoutPreset>(item) {
            Ok(p) => out.push(p),
            Err(e) => tracing::warn!(error = %e, "settings: malformed layout preset dropped"),
        }
    }
    Ok(out)
}

/// Discriminated tab config. The `kind` field is the JSON discriminator
/// (`"ai_tool"`, `"shell"`, or — V14 Phase F — `"preview"`), produced by
/// serde's internally-tagged representation. Each variant carries the fields
/// specific to its kind — AI tabs have `tts_injection` and three notification
/// slots; Shell tabs have two notification slots and no TTS hook; Preview
/// tabs have neither (no PTY at all — `url`/`device_width`/`auto_reload`
/// drive an embedded child webview instead, see `crate::preview`).
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum TabConfig {
    AiTool(AiToolTabConfig),
    Shell(ShellTabConfig),
    Preview(PreviewTabConfig),
}

impl TabConfig {
    pub fn id(&self) -> &str {
        match self {
            TabConfig::AiTool(c) => &c.id,
            TabConfig::Shell(c) => &c.id,
            TabConfig::Preview(c) => &c.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            TabConfig::AiTool(c) => &c.name,
            TabConfig::Shell(c) => &c.name,
            TabConfig::Preview(c) => &c.name,
        }
    }

    pub fn set_name(&mut self, name: String) {
        match self {
            TabConfig::AiTool(c) => c.name = name,
            TabConfig::Shell(c) => c.name = name,
            TabConfig::Preview(c) => c.name = name,
        }
    }

    pub fn builtin(&self) -> bool {
        match self {
            TabConfig::AiTool(c) => c.builtin,
            TabConfig::Shell(c) => c.builtin,
            TabConfig::Preview(c) => c.builtin,
        }
    }

    pub fn set_builtin(&mut self, value: bool) {
        match self {
            TabConfig::AiTool(c) => c.builtin = value,
            TabConfig::Shell(c) => c.builtin = value,
            TabConfig::Preview(c) => c.builtin = value,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AiToolTabFields", export_to = "settings.ts"))]
pub struct AiToolTabConfig {
    pub id: String,
    pub builtin: bool,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// `None` (the default for every builtin and every plain "+"-duplicated
    /// tab) ⇒ spawn with the app's launch directory, same as always. V13
    /// Phase D's "New tab in worktree…" flow
    /// (`ipc::tab_lifecycle::create_ai_tab_in_worktree`) is the one place
    /// that sets this — to the freshly created worktree's path — so the tab
    /// runs isolated from the main working tree. This field already existed
    /// (mirroring `ShellTabConfig::cwd`, wired into `build_ai_tool_spec`
    /// since V3) but was never set by any flow until Phase D; there is no
    /// user-facing "set a custom cwd" affordance for AI tabs, so a non-`None`
    /// value always means "this tab lives in a cImp-managed worktree" — shown
    /// read-only where the tab's Configure surface displays it.
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub tts_injection: TtsInjection,
    pub notifications: AiNotificationConfig,
    /// Carried over from earlier schemas where aider had a one-time
    /// first-launch banner. Aider is gone (V1.4-07) and Claude tabs
    /// pre-dismiss this; left in place to keep the wire format stable
    /// for users still loading older settings files.
    pub first_launch_notice_dismissed: bool,
    /// V1.4-01 per-tab terminal palette override. `None` means inherit
    /// the global `terminal.theme`; `Some(_)` replaces it with the
    /// override's bundled name (or Custom block) for this tab only.
    /// The override travels with the tab through drag-and-drop because
    /// it lives on the tab itself, not on a pane.
    pub theme_override: Option<TerminalThemeSettings>,
    /// V1.4-02 per-tab background override (three-state). `None` means
    /// inherit the global `terminal.background`; `Some(Disabled)` means
    /// opt out (theme bg only); `Some(Custom(cfg))` replaces the global
    /// background wholesale for this tab.
    // HAND-KEPT SEAM: `BackgroundOverride`'s (de)serialize is hand-written
    // (the literal `"disabled"` string OR a full config object), which no
    // derive expresses. `types.ts` derives its `BackgroundOverrideWire` alias
    // FROM this field, so the two cannot drift.
    #[cfg_attr(test, ts(type = "\"disabled\" | TerminalBackgroundSettings | null"))]
    pub background_override: Option<BackgroundOverride>,
    /// V1.4-07: when `true`, the launch-time env composition synthesizes
    /// `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` (and `ANTHROPIC_MODEL`
    /// if `claude_local.model_alias` is non-empty) from the global
    /// `claude_local` settings group. Per-tab `env` entries override
    /// synthesized values.
    pub use_local_provider: bool,
    /// V32 Phase G (locked decision 16): this tab's **L3** row — a tri-state
    /// `Inherit | On | Off` per injection-protection feature, defaulting to
    /// `Inherit` everywhere so an untouched tab behaves exactly as before.
    ///
    /// Only the features that HAVE a tab scope have a cell (see
    /// [`TabInjectionOverrides`](crate::settings::injection::TabInjectionOverrides)):
    /// the worker-only canary and the app-wide terminal-escape hygiene are
    /// structurally absent rather than present-and-ignored.
    ///
    /// Two of the cells (native-web visibility, consumer hygiene) are
    /// spawn-baked and therefore ride `spawn_inject_sig`, so flipping them
    /// raises the "restart the AI tab" hint; the rest take effect on the next
    /// call.
    ///
    /// `pub(in crate::settings)` (#44): an L3 cell answers a *different*
    /// question from `effective(feature, scope, settings)` — it ignores the
    /// global master and the app-wide flag — so reading one outside the resolver
    /// is the same defect as reading a raw L1/L2 switch, and is now the same
    /// compile error. Test code outside `crate::settings` writes cells through
    /// `Settings::set_tab_override_for_test`.
    pub(in crate::settings) injection_overrides: crate::settings::injection::TabInjectionOverrides,
    /// V39 Phase A (locked decision 4): the user's sticky **read-only** lock
    /// on this tab — the keyboard is refused, the tab keeps running.
    ///
    /// Only the `User` source is persisted. The engine's transient `Driven`
    /// lock lives in `state::ReadOnlyTabs` and is deliberately absent here:
    /// after a crash mid-delegation nothing is in flight, so a persisted
    /// `Driven` would be a lock with no owner to lift it.
    ///
    /// Additive `#[serde(default)]` (container level) ⇒ every existing
    /// settings file loads with the tab writable, which is the pre-V39
    /// behaviour. Read-only governs the *user's* keyboard only: a locked tab
    /// is still a valid delegation worker.
    ///
    /// **Not spawn-baked** — it is enforced per write in `pty_write`, so
    /// flipping it never asks for a tab restart (`spawn_inject_sig` has no
    /// slot for it, and a test pins that).
    pub read_only: bool,
    /// V39 Phase B (locked decision 8): what this tab is **for** in the
    /// delegation surface — the single source of truth for both driver modes.
    ///
    /// Persisted and restored at startup (in-flight state never is), and
    /// exclusive by construction: the roles are one enum, not two flags, so a
    /// tab cannot be both a `delegate_task_*` target and a facade backend.
    ///
    /// **Not spawn-baked** (decision 15): the `delegate_task_*` set rides the
    /// child proxy's live `tools/list` plus the V37 `list_changed` pulse, and
    /// the facade rides `offload_task`'s live description — so changing a role
    /// takes effect on the next turn without restarting either tab, and
    /// `spawn_inject_sig` has no slot for it (a test pins that).
    pub delegation_role: DelegationRole,
    /// V39 (locked decision 8): the per-backend knobs a
    /// [`DelegationRole::RemoteOffload`] tab is synthesized into
    /// `effective_backends()` with.
    ///
    /// Declared in Phase B and **consumed in Phase C**: the fields' defaults
    /// are already decided, and a container that arrives with the role it
    /// belongs to is one schema shape rather than two. Meaningless while the
    /// role is anything else — deliberately not enforced, because a user who
    /// sets a backend name, switches the role away and switches it back should
    /// find the name where they left it.
    pub delegation_backend: DelegationBackend,
}

/// V39 Phase B, locked decision 8 — **one exclusive role per tab**.
///
/// `None` is the default and the answer for every tab that has never been
/// touched: a tab becomes reachable by another harness only by an explicit user
/// action, on that tab.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum DelegationRole {
    /// Not a delegation target. The default.
    #[default]
    None,
    /// The target of `delegate_task_<harness>` for this tab's harness.
    ///
    /// **At most one Manual tab per harness** — setting Manual on a second tab
    /// of the same harness MOVES the role (the previous holder drops to
    /// `None`), which is enforced in `ipc::commands::tab_set_delegation_role`
    /// rather than by this type: an enum cannot express a cross-tab
    /// uniqueness rule, and a settings file hand-edited into two Manual tabs
    /// must load rather than fail.
    Manual,
    /// A facade offload backend (Phase C): the requesting harness sees a
    /// backend name, never a tab. **Any number** per harness.
    RemoteOffload,
}

/// V39, locked decision 8 — the per-tab knobs a `RemoteOffload` tab carries
/// into the offload backend list. Phase C reads them.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct DelegationBackend {
    /// The name the requesting harness sees (`lan-worker-2`), NEVER the tab
    /// name — decision 3's facade half is only a facade if the tab does not
    /// leak through it. `None` ⇒ [`facade_default_name`], an opaque per-tab
    /// name; it used to fall back to the tab's DISPLAY name, which put the tab
    /// into the asking model's prose (V39 review L-2).
    pub name: Option<String>,
    /// Router bias, exactly as a configured HTTP backend carries it.
    pub tier: BackendTier,
    /// The worker's usable context window, in tokens, if the user knows it.
    /// `None` ⇒ Phase C uses a generous default: a facade whose context is
    /// under-declared is routed away from work it could have done, and one
    /// that is over-declared fails visibly on the worker's own side.
    pub declared_context: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "ShellTabFields", export_to = "settings.ts"))]
pub struct ShellTabConfig {
    pub id: String,
    pub builtin: bool,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub notifications: ShellNotificationConfig,
    /// V1.4-01 per-tab terminal palette override. `None` means inherit
    /// the global `terminal.theme`; `Some(_)` replaces it for this tab.
    /// See `AiToolTabConfig::theme_override` for the full rationale.
    pub theme_override: Option<TerminalThemeSettings>,
    /// V1.4-02 per-tab background override (three-state). See
    /// `AiToolTabConfig::background_override`.
    // HAND-KEPT SEAM: `BackgroundOverride`'s (de)serialize is hand-written
    // (the literal `"disabled"` string OR a full config object), which no
    // derive expresses. `types.ts` derives its `BackgroundOverrideWire` alias
    // FROM this field, so the two cannot drift.
    #[cfg_attr(test, ts(type = "\"disabled\" | TerminalBackgroundSettings | null"))]
    pub background_override: Option<BackgroundOverride>,
}

/// V14 Phase F: a user-created Preview tab — an embedded, localhost-scoped
/// child webview, not a subprocess. No `command`/`args`/`cwd`/`env`/PTY
/// fields at all (unlike `AiToolTabConfig`/`ShellTabConfig`) since there is
/// nothing to spawn; `crate::preview` manages the child webview keyed by
/// tab id, reading `url`/`device_width`/`auto_reload` from here.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "PreviewTabFields", export_to = "settings.ts"))]
pub struct PreviewTabConfig {
    pub id: String,
    /// Always `false` in practice — Preview has no reserved/builtin instance
    /// (every one is user-created via the `+` menu), but the field exists so
    /// `TabConfig`'s shared accessors (`builtin()`/`set_builtin()`) stay
    /// uniform across variants.
    pub builtin: bool,
    pub name: String,
    pub url: String,
    /// `None` ⇒ the toolbar's "Desktop" preset (fill the available rect, no
    /// letterboxing). `Some(w)` ⇒ letterbox to a fixed CSS-pixel width (the
    /// mobile/tablet presets) — see `preview::policy` for the shared
    /// device-preset table for the rect math.
    pub device_width: Option<u32>,
    /// Reload after a ~1s quiet period following a `fs-batch` event (V13),
    /// while the tab is visible. Off by default — a dev server's own HMR
    /// usually already handles this, so auto-reload is an opt-in belt for
    /// setups without it.
    pub auto_reload: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_layout_drops_to_none_without_losing_rest_of_settings() {
        // A Split node missing `ratio` is invalid, but it must not take the
        // whole Settings parse down with it. The layout degrades to None and
        // a sibling field (session.active_tab_id) still loads.
        let v = json!({
            "session": { "active_tab_id": "claude" },
            "layout": {
                "tree": {
                    "type": "split",
                    "id": "s1",
                    "direction": "horizontal",
                    "first": { "type": "pane", "id": "p1", "tab_ids": ["claude"], "active_tab_id": "claude" },
                    "second": { "type": "pane", "id": "p2", "tab_ids": [], "active_tab_id": null }
                },
                "focused_pane_id": "p1"
            }
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert!(
            parsed.layout.is_none(),
            "malformed layout should drop to None"
        );
        assert_eq!(parsed.session.active_tab_id.as_deref(), Some("claude"));
    }

    #[test]
    fn valid_layout_still_parses() {
        let v = json!({
            "layout": {
                "tree": { "type": "pane", "id": "p1", "tab_ids": ["claude"], "active_tab_id": "claude" },
                "focused_pane_id": "p1"
            }
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert!(parsed.layout.is_some(), "valid layout should parse");
    }

    #[test]
    fn malformed_preset_is_dropped_individually() {
        // First preset is valid; second is missing `tree`. Keep the good one.
        let v = json!({
            "layout_presets": [
                {
                    "name": "good",
                    "created_at": "2026-01-01T00:00:00Z",
                    "tree": { "type": "pane", "id": "p1", "tab_ids": [], "active_tab_id": null }
                },
                { "name": "bad", "created_at": "2026-01-01T00:00:00Z" }
            ]
        });
        let parsed: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.layout_presets.len(), 1);
        assert_eq!(parsed.layout_presets[0].name, "good");
    }
}
