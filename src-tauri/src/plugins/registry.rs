//! V38 Phase B — the **registry**: what a manifest declares, joined with what
//! the user configured and which project is open, resolved into the one answer
//! every pipeline needs — *can I run this, and with what?*
//!
//! Phase A's loader says which manifests exist. The settings container says
//! which of their tools the user wants, where their binaries live, and what
//! their declared variables are set to. Neither alone is a decision. This module
//! is the join, and it is the ONLY place that join happens: Phase C's audit
//! fan-out, Phase D's `run_check` set and `run_command` allowlist all read
//! [`effective_tools`] rather than each re-deriving "enabled" from two flags and
//! a path map. Three copies of that rule is three chances for one of them to
//! forget the plugin-level switch.
//!
//! Pure and synchronous by construction — a `PluginSet`, a
//! [`ToolPluginsSettings`] and an optional project root in, a `Vec` out. No
//! globals, no I/O, no clock. That is what makes the layering testable at all:
//! "disabling a plugin disables its tools without clearing their own flags" is
//! an assertion here rather than a thing to click through.
//!
//! # The rules it resolves, and why each is where it is
//!
//! * **Enabled** = the plugin's switch AND the tool's. The plugin switch is a
//!   group operation (decision 9) that deliberately does NOT write through to
//!   the per-tool flags, so re-enabling a plugin restores exactly the selection
//!   the user had rather than turning everything on.
//! * **Path** = this project's entry, else the machine-wide entry, else unset.
//!   A tool with no path is **inert**: nothing is bundled, cImp never guesses a
//!   binary for a plugin (decision 7), and "no path" is a configuration state,
//!   not an error.
//! * **Runnable** = enabled AND path set. The two are separate questions and
//!   collapsing them would make "why is my enabled tool not running?" unanswerable.
//! * **Variables** = the manifest's declared defaults, overlaid by the user's
//!   values, **for declared names only**. A stored value whose name the manifest
//!   no longer declares is kept in settings (a plugin mid-upgrade) but never
//!   substituted — `{var:NAME}` can only name a declared variable, so an
//!   undeclared value has nowhere to go.
//! * **Parameters** are dropped unless the manifest sets `parameters_allowed`.
//!   Stored state outliving a manifest change must not become argv on a tool
//!   whose author never opted into an appendable command line.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::loader::PluginSet;
use super::manifest::{Provenance, ToolKind, ToolManifest};
use crate::settings::ToolPluginsSettings;

/// Where a tool's effective binary path came from — rendered as the settings
/// pane's inherited/overridden chip, and worth knowing at a spawn site too when
/// a run has to explain which configuration it used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathScope {
    /// This project overrides the machine-wide entry.
    Project,
    /// The machine-wide entry, with no project override.
    Global,
    /// No path anywhere — the tool is inert.
    Unset,
}

/// One tool, fully resolved against user state and a project.
///
/// Carries the manifest **by value**: a caller resolving a tool is about to
/// build an argv from it, and handing back a borrow would tie every consumer's
/// lifetime to a `PluginSet` snapshot it has no other reason to hold.
#[derive(Clone, Debug, Serialize)]
pub struct EffectiveTool {
    /// `name@version/tool-id` — the globally unique id, and the key both path
    /// maps use.
    pub tool_key: String,
    /// `name@version`.
    pub plugin_key: String,
    /// The plugin's display name (`label`, else `name`).
    pub plugin_label: String,
    /// The tool's manifest-local id.
    pub tool_id: String,
    /// The category the manifest files this tool under. Presentation only — no
    /// pipeline may branch on it (decision 2: kind ⊥ category).
    pub category_id: String,
    /// Loader-stamped; the security gates key off this and never off a name.
    pub provenance: Provenance,
    pub manifest: ToolManifest,
    /// `plugin_enabled && tool_enabled` — the answer callers want.
    pub enabled: bool,
    /// The two halves, kept so the settings pane can render a tool that is off
    /// *because its plugin is* differently from one the user switched off.
    pub plugin_enabled: bool,
    pub tool_enabled: bool,
    /// The resolved binary. `None` ⇒ inert.
    pub path: Option<String>,
    pub path_scope: PathScope,
    /// User override, else the manifest's, else `None` (the pipeline's default).
    pub timeout_secs: Option<u64>,
    /// Declared defaults overlaid by user values, declared names only.
    pub variables: BTreeMap<String, String>,
    /// Appended argv, empty unless the manifest allows parameters.
    pub parameters: Vec<String>,
}

impl EffectiveTool {
    pub fn kind(&self) -> ToolKind {
        self.manifest.kind
    }

    /// Enabled, **and** cImp knows what to spawn. The predicate every pipeline
    /// filters on; see the module docs for why it is not the same as `enabled`.
    ///
    /// The built-in arm is the one place decision 10's "no automatic PATH
    /// resolution" gives way, and it is narrow on purpose: a built-in tool's
    /// manifest names a bare COMMAND cImp has shipped support for since V23
    /// (`gitleaks`, `ruff`, …), so "no path configured" means "resolve it the
    /// way this tool has always been resolved" rather than "inert". The rule
    /// protects a user from cImp guessing a binary for a definition a stranger
    /// wrote; it was never an argument for making the fourteen shipped scanners
    /// stop working on upgrade.
    pub fn runnable(&self) -> bool {
        self.enabled && (self.is_provider() || self.path.is_some() || self.resolves_by_name())
    }

    /// V38 Phase F — whether this is a **tier-2** tool: its findings come from
    /// an MCP server the user configured, not from a binary cImp spawns.
    ///
    /// A provider tool needs no path, so `enabled` is the whole of `runnable`
    /// for it. Whether the referenced SERVER exists, is enabled and answers is
    /// deliberately NOT asked here: V37's contract C4 makes dispatch the
    /// enforcement point and advertisement a courtesy, and a registry-level
    /// existence gate would drop an enabled tool out of the fan-out silently —
    /// exactly the "configured capability vanishes with no explanation" this
    /// milestone refuses everywhere else. A missing or disabled server surfaces
    /// as a failed CHIP with the server's own reason.
    pub fn is_provider(&self) -> bool {
        self.manifest.provider.is_some()
    }

    /// Whether this tool can be found without a configured path — built-in
    /// provenance AND a declared command name. Both halves: provenance is the
    /// gate, and the name is what there is to resolve.
    pub fn resolves_by_name(&self) -> bool {
        self.provenance == Provenance::Builtin && self.manifest.command.is_some()
    }

    /// The bare command NAME the Settings **Detect probe** searches `ebin` →
    /// `PATH` for, or `None` when nothing safe can be derived. See
    /// [`probe_command_name`] for the rules and for why this is not
    /// [`Self::resolves_by_name`].
    pub fn probe_name(&self) -> Option<String> {
        probe_command_name(&self.manifest, self.provenance)
    }
}

/// The bare command name a **Detect probe** may look for on this machine, for
/// any tool — built-in or dropped in a folder — or `None` when the manifest
/// gives us nothing a name search could honestly use.
///
/// # Why this is not `resolves_by_name`
///
/// The two answer different questions, and V38 shipped with only the first.
/// [`EffectiveTool::resolves_by_name`] is a RUN-TIME trust rule: may cImp spawn
/// this tool with no stored path? For a user plugin the answer stays **no**
/// (decision 7/10) — nothing here changes what a check or `run_command`
/// launches, and a path-less user tool is still inert.
///
/// This one is a UI question: the user pressed Detect with an empty box, and
/// the only useful thing that button can do is *look*. Gating the look on the
/// run-time rule made Detect answer "not found on PATH or ebin" for every
/// starter-pack tool without ever searching PATH — an answer that was both
/// useless and untrue. Searching costs nothing and grants nothing: the result
/// is displayed, and it becomes a configured path only when the *user's* click
/// stores one, which is the same consent a Browse… would have given.
///
/// # The rules, in order
///
/// 1. A **built-in** whose manifest names a `command` — the pre-V38 behaviour,
///    and the only one that also governs run time.
/// 2. A **check**-kind tool — the first whitespace-separated token of its
///    `cmd`, which is by construction the program the check line runs
///    (`cargo build --message-format=json` → `cargo`).
/// 3. A **command**-kind tool — its `id`. The kind exists to expose one CLI
///    through `run_command`, and the id is the name that CLI is known by
///    (`git`, `npm`, `dotnet`); the shipped pack is written that way throughout.
/// 4. Anything else — `None`. An audit/security tool's argv is a template with
///    no program in it (decision 7 keeps the executable out of the manifest),
///    so there is no name to guess and guessing the id would probe some
///    unrelated binary that happened to share it.
///
/// # The guard
///
/// Whatever the rules produce must be a **bare name**: non-empty after trim and
/// free of `/`, `\`, `:` and `..`. A manifest is attacker-controlled input
/// (`plugins::manifest`'s premise), and `id`/`cmd` are NOT screened the way
/// `command` is — `command` is refused outright in a scanned file, so a hostile
/// author's only route here is rule 2 or 3. Without the guard a `cmd` of
/// `..\..\Users\me\evil.exe --x` would aim the probe at an arbitrary file and
/// turn a Detect click into an execution primitive. With it, resolution can only
/// go through the existing `ebin` → `PATH` ladder, which searches directories
/// cImp and the user already trust. Refused rather than sanitized: a name we had
/// to rewrite is not the name the author wrote, and "which binary ran" is not a
/// question to answer by guessing.
pub fn probe_command_name(manifest: &ToolManifest, provenance: Provenance) -> Option<String> {
    let candidate = match (provenance, manifest.command.as_deref()) {
        (Provenance::Builtin, Some(command)) => command,
        _ => match manifest.kind {
            ToolKind::Check => manifest.cmd.as_deref()?.split_whitespace().next()?,
            ToolKind::Command => manifest.id.as_str(),
            ToolKind::Audit | ToolKind::Security => return None,
        },
    };
    bare_name(candidate)
}

/// The guard half of [`probe_command_name`], split out so the rule reads as one
/// line at its single call site: a name, or nothing.
fn bare_name(candidate: &str) -> Option<String> {
    let name = candidate.trim();
    if name.is_empty() || name.contains(['/', '\\', ':']) || name.contains("..") {
        return None;
    }
    Some(name.to_string())
}

/// The key a project's path overrides are stored under.
///
/// [`crate::activity::root_key`] rather than a fresh normalizer: the same
/// directory reaches cImp in several spellings (the launch cwd, a
/// `find_graph_root` ancestor walk, drive-letter vs. verbatim `\\?\` on
/// Windows), and that function is the existing answer to making those compare
/// equal — it canonicalizes and memoizes. A second spelling rule here would
/// silently stop matching the first.
pub fn project_key(root: &Path) -> String {
    crate::activity::root_key(root)
}

/// Every tool of every loaded plugin, resolved — including the disabled and the
/// path-less ones, because the settings pane has to render exactly those.
///
/// `project_root` is the project whose path overrides apply; `None` resolves
/// against the machine-wide map alone (a consumer with no project in hand, e.g.
/// a global settings view).
///
/// Order follows the `PluginSet` (sorted by plugin key) and then the manifest's
/// own tool order, so a settings list and a fan-out report agree on sequence
/// without either sorting again.
pub fn effective_tools(
    set: &PluginSet,
    cfg: &ToolPluginsSettings,
    project_root: Option<&Path>,
) -> Vec<EffectiveTool> {
    let project = project_root.map(project_key);
    let project_paths = project
        .as_ref()
        .and_then(|key| cfg.project_paths.get(key.as_str()));

    let mut out = Vec::new();
    for plugin in &set.plugins {
        let state = cfg.plugins.get(&plugin.key);
        // A plugin with no stored state is a plugin the user has never touched,
        // which is ON — the same answer `PluginState::default()` gives, reached
        // here without inserting anything into a map we only have by reference.
        let plugin_enabled = state.is_none_or(|s| s.enabled);
        let plugin_label = plugin
            .manifest
            .label
            .clone()
            .unwrap_or_else(|| plugin.manifest.name.clone());

        for tool in &plugin.manifest.tools {
            let tool_key = plugin.tool_key(&tool.id);
            let tool_state = state.and_then(|s| s.tools.get(&tool.id));
            // No stored state ⇒ the manifest's own default, which is `true` for
            // everything except a tool its author marked expensive enough that
            // nobody should get it by accident (`dotnet-analyzers` runs a real
            // build; `semgrep-quality` fetches a ruleset over the network).
            // Once a state exists it wins in BOTH directions — the field is a
            // default, not a lock.
            let tool_enabled = tool_state.map_or(tool.enabled_by_default, |t| t.enabled);

            let (path, path_scope) = match project_paths
                .and_then(|m| m.get(&tool_key))
                .filter(|p| !p.trim().is_empty())
            {
                Some(p) => (Some(p.clone()), PathScope::Project),
                None => match cfg
                    .global_paths
                    .get(&tool_key)
                    .filter(|p| !p.trim().is_empty())
                {
                    Some(p) => (Some(p.clone()), PathScope::Global),
                    None => (None, PathScope::Unset),
                },
            };

            // Declared defaults first, then the user's values on top — and only
            // for names the manifest still declares (see the module docs).
            let mut variables: BTreeMap<String, String> = tool
                .variables
                .iter()
                .filter_map(|v| v.default.clone().map(|d| (v.name.clone(), d)))
                .collect();
            if let Some(t) = tool_state {
                for v in &tool.variables {
                    // A BLANK stored value is not an override — it is the shape
                    // a cleared input leaves, exactly as for a path. Treating it
                    // as a value would render `--config ""` on the next scan and
                    // the user would have no way back to the declared default
                    // short of deleting a settings key by hand. (This is also
                    // what makes the v33 migration of `code_audit.tools[].ruleset`
                    // exact: an empty legacy ruleset meant "use the built-in
                    // default", which is the same thing as no value at all.)
                    match t.variables.get(&v.name) {
                        Some(value) if !value.trim().is_empty() => {
                            variables.insert(v.name.clone(), value.clone());
                        }
                        _ => {}
                    }
                }
            }

            out.push(EffectiveTool {
                category_id: category_of(plugin, &tool.id),
                tool_key,
                plugin_key: plugin.key.clone(),
                plugin_label: plugin_label.clone(),
                tool_id: tool.id.clone(),
                provenance: plugin.provenance,
                enabled: plugin_enabled && tool_enabled,
                plugin_enabled,
                tool_enabled,
                path,
                path_scope,
                timeout_secs: tool_state
                    .and_then(|t| t.timeout_secs)
                    .or(tool.timeout_secs),
                variables,
                parameters: if tool.parameters_allowed {
                    tool_state.map(|t| t.parameters.clone()).unwrap_or_default()
                } else {
                    Vec::new()
                },
                manifest: tool.clone(),
            });
        }
    }
    out
}

/// The subset a pipeline may actually spawn: enabled, and with a path.
///
/// The convenience that keeps every consumer from writing the predicate itself —
/// which is how one of them would eventually write only half of it.
pub fn runnable_tools(
    set: &PluginSet,
    cfg: &ToolPluginsSettings,
    project_root: Option<&Path>,
) -> Vec<EffectiveTool> {
    effective_tools(set, cfg, project_root)
        .into_iter()
        .filter(EffectiveTool::runnable)
        .collect()
}

/// **The** model-facing name for one plugin tool, given what is already taken.
///
/// The manifest-local id first (short, authored, and what a user reading the
/// settings pane sees), then the fully-qualified `name@version/tool-id` key,
/// then the key with a counter. Total by construction: the loop cannot run out
/// of candidates, so there is no "and otherwise drop it" branch for a name clash
/// to fall into.
///
/// Shared rather than per-surface (V38 F-3 moved it here from
/// `checks::plugin`). Two surfaces now advertise plugin tools by name —
/// `run_check`'s `name` enum and `run_command`'s `tool` enum — and a second copy
/// of "which spelling wins a collision" is how one surface ends up advertising a
/// name the other cannot resolve. The `taken` list is the caller's, because what
/// counts as taken differs per surface: `run_check` lays down the user's own
/// `settings.checks` first (a plugin may never shadow a configured check), while
/// the command surface has only its own population to collide with.
pub fn advertised_name(tool: &EffectiveTool, taken: &[&str]) -> String {
    if !taken.contains(&tool.tool_id.as_str()) {
        return tool.tool_id.clone();
    }
    if !taken.contains(&tool.tool_key.as_str()) {
        return tool.tool_key.clone();
    }
    for n in 2u32.. {
        let candidate = format!("{}#{n}", tool.tool_key);
        if !taken.contains(&candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("the counter is unbounded")
}

/// The category a tool is filed under. Validation guarantees a partition (every
/// tool in exactly one), so the fallback can only be reached by a manifest that
/// did not come through [`super::manifest::validate`] — and an empty string is
/// the honest answer there rather than an invented category id.
fn category_of(plugin: &super::loader::LoadedPlugin, tool_id: &str) -> String {
    plugin
        .manifest
        .categories
        .iter()
        .find(|c| c.tools.iter().any(|t| t == tool_id))
        .map(|c| c.id.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::loader::scan_dir;
    use crate::settings::{PluginState, ToolState};
    use std::path::PathBuf;

    /// One plugin, two tools in two categories, one of them with a declared
    /// variable and appendable parameters. Built through the real loader so the
    /// registry is exercised against validated manifests only.
    fn fixture() -> (PluginSet, PathBuf) {
        // A fresh directory per test, keyed by UUID rather than by clock: these
        // tests run in parallel, and a millisecond-stamped name collided — one
        // test scanned a directory another had just deleted.
        let dir = std::env::temp_dir().join(format!("cimp-registry-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("acme.json"),
            r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "label": "Acme Tools",
              "categories": [
                { "id": "sec", "label": "Security", "tools": ["scan"] },
                { "id": "misc", "label": "Misc", "tools": ["fmt"] }
              ],
              "tools": [
                {
                  "id": "scan", "label": "Acme Scan", "kind": "security",
                  "argv": ["--rules", "{var:ruleset}", "{root}"],
                  "variables": [{ "name": "ruleset", "label": "Ruleset", "default": "p/default" }],
                  "parameters_allowed": true,
                  "timeout_secs": 300
                },
                { "id": "fmt", "label": "Acme Format", "kind": "command" }
              ]
            }"#,
        )
        .expect("write manifest");
        let set = scan_dir(&dir, Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        (set, dir)
    }

    fn find<'a>(tools: &'a [EffectiveTool], key: &str) -> &'a EffectiveTool {
        tools
            .iter()
            .find(|t| t.tool_key == key)
            .unwrap_or_else(|| panic!("no tool `{key}` in {:?}", tools.iter().map(|t| &t.tool_key)))
    }

    /// Untouched state = every tool enabled, every variable at its declared
    /// default, and nothing runnable, because nothing has a path yet. The last
    /// clause is the one that matters: "enabled by default" must not mean "runs
    /// by default" for a tool cImp has never been told where to find.
    #[test]
    fn an_unconfigured_plugin_is_enabled_but_not_runnable() {
        let (set, dir) = fixture();
        let cfg = ToolPluginsSettings::default();
        let tools = effective_tools(&set, &cfg, None);

        assert_eq!(tools.len(), 2);
        let scan = find(&tools, "acme@1.0.0/scan");
        assert!(scan.enabled);
        assert!(!scan.runnable(), "no path ⇒ inert");
        assert_eq!(scan.path_scope, PathScope::Unset);
        assert_eq!(scan.variables["ruleset"], "p/default");
        assert_eq!(scan.timeout_secs, Some(300), "the manifest's own value");
        assert_eq!(scan.category_id, "sec");
        assert_eq!(scan.plugin_label, "Acme Tools");
        assert_eq!(find(&tools, "acme@1.0.0/fmt").category_id, "misc");
        assert!(runnable_tools(&set, &cfg, None).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Decision 9's group operation: the plugin switch disables every tool and
    /// leaves their own flags alone, so flipping it back restores the exact
    /// selection rather than turning everything on.
    #[test]
    fn disabling_a_plugin_disables_its_tools_without_clearing_their_flags() {
        let (set, dir) = fixture();
        let mut cfg = ToolPluginsSettings::default();
        cfg.plugins.insert(
            "acme@1.0.0".to_string(),
            PluginState {
                enabled: false,
                tools: BTreeMap::from([(
                    "scan".to_string(),
                    ToolState {
                        enabled: true,
                        ..ToolState::default()
                    },
                )]),
            },
        );
        cfg.global_paths.insert(
            "acme@1.0.0/scan".to_string(),
            "C:\\bin\\acme.exe".to_string(),
        );

        let scan = find(&effective_tools(&set, &cfg, None), "acme@1.0.0/scan").clone();
        assert!(!scan.enabled, "the plugin switch wins");
        assert!(scan.tool_enabled, "…and the tool's own flag is untouched");
        assert!(!scan.plugin_enabled);
        assert!(
            !scan.runnable(),
            "a path does not make a disabled tool runnable"
        );

        // Flip the plugin back on: the stored per-tool selection is what returns.
        cfg.plugins.get_mut("acme@1.0.0").unwrap().enabled = true;
        let scan = find(&effective_tools(&set, &cfg, None), "acme@1.0.0/scan").clone();
        assert!(scan.enabled && scan.runnable());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Path precedence: this project's entry beats the machine-wide one, and an
    /// empty string is not a path (it is the shape a cleared input leaves).
    #[test]
    fn a_project_path_overrides_the_global_one() {
        let (set, dir) = fixture();
        let root = std::env::temp_dir();
        let mut cfg = ToolPluginsSettings::default();
        cfg.global_paths.insert(
            "acme@1.0.0/scan".to_string(),
            "C:\\global\\acme.exe".to_string(),
        );

        let scan = find(&effective_tools(&set, &cfg, Some(&root)), "acme@1.0.0/scan").clone();
        assert_eq!(scan.path.as_deref(), Some("C:\\global\\acme.exe"));
        assert_eq!(scan.path_scope, PathScope::Global);

        cfg.project_paths.insert(
            project_key(&root),
            BTreeMap::from([(
                "acme@1.0.0/scan".to_string(),
                "D:\\project\\acme.exe".to_string(),
            )]),
        );
        let scan = find(&effective_tools(&set, &cfg, Some(&root)), "acme@1.0.0/scan").clone();
        assert_eq!(scan.path.as_deref(), Some("D:\\project\\acme.exe"));
        assert_eq!(scan.path_scope, PathScope::Project);
        // …and another project (here: none at all) still sees the global one.
        assert_eq!(
            find(&effective_tools(&set, &cfg, None), "acme@1.0.0/scan").path_scope,
            PathScope::Global
        );

        // A cleared input stores "", which must read as "no override" rather
        // than as a path that resolves to nothing at spawn time.
        cfg.project_paths
            .get_mut(&project_key(&root))
            .unwrap()
            .insert("acme@1.0.0/scan".to_string(), "   ".to_string());
        assert_eq!(
            find(&effective_tools(&set, &cfg, Some(&root)), "acme@1.0.0/scan").path_scope,
            PathScope::Global
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Variables layer declared-default → user value, and ONLY for names the
    /// manifest still declares. Parameters are dropped for a tool whose author
    /// never opted into an appendable argv.
    #[test]
    fn variables_layer_over_defaults_and_stale_state_cannot_become_argv() {
        let (set, dir) = fixture();
        let mut cfg = ToolPluginsSettings::default();
        cfg.plugins.insert(
            "acme@1.0.0".to_string(),
            PluginState {
                enabled: true,
                tools: BTreeMap::from([
                    (
                        "scan".to_string(),
                        ToolState {
                            variables: BTreeMap::from([
                                ("ruleset".to_string(), "p/ci".to_string()),
                                // Declared by a PREVIOUS version of this plugin.
                                ("gone".to_string(), "boom".to_string()),
                            ]),
                            parameters: vec!["--exclude".into(), "vendor".into()],
                            timeout_secs: Some(900),
                            ..ToolState::default()
                        },
                    ),
                    (
                        // `fmt` does not set `parameters_allowed`.
                        "fmt".to_string(),
                        ToolState {
                            parameters: vec!["--rm-rf".into()],
                            ..ToolState::default()
                        },
                    ),
                ]),
            },
        );

        let tools = effective_tools(&set, &cfg, None);
        let scan = find(&tools, "acme@1.0.0/scan");
        assert_eq!(scan.variables["ruleset"], "p/ci", "user value wins");
        assert!(
            !scan.variables.contains_key("gone"),
            "a value the manifest no longer declares has nowhere to be substituted"
        );
        assert_eq!(scan.parameters, vec!["--exclude", "vendor"]);
        assert_eq!(scan.timeout_secs, Some(900), "user override beats manifest");

        assert!(
            find(&tools, "acme@1.0.0/fmt").parameters.is_empty(),
            "stored parameters must not become argv on a tool that never allowed them"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tool manifest straight from JSON, deliberately WITHOUT running
    /// [`super::super::manifest::validate`]: the guard's whole job is to hold
    /// for shapes the validator would have refused, and a fixture that could
    /// only carry validated values would never exercise it.
    fn tool_json(json: &str) -> ToolManifest {
        serde_json::from_str(json).expect("tool manifest")
    }

    /// Rule 1: a built-in's declared `command` is the name, and it wins over
    /// what the kind rules would have derived — it is the field whose entire
    /// purpose is naming the binary.
    #[test]
    fn a_builtin_command_is_the_probe_name() {
        let t = tool_json(
            r#"{ "id": "gitleaks-x", "label": "Gitleaks", "kind": "security",
                 "argv": ["{root}"], "command": "gitleaks" }"#,
        );
        assert_eq!(
            probe_command_name(&t, Provenance::Builtin).as_deref(),
            Some("gitleaks")
        );
        // …and the gate is PROVENANCE, not the presence of the field: a scanned
        // file may not carry `command` at all (the loader refuses it), so the
        // user arm falls through to the kind rules — which give nothing for a
        // security tool.
        assert_eq!(probe_command_name(&t, Provenance::User), None);
    }

    /// Rule 2: a check's `cmd` is a command LINE, and its first token is the
    /// program — the same split the check runner makes.
    #[test]
    fn a_check_probes_the_first_token_of_its_cmd() {
        for (cmd, want) in [
            ("cargo build --message-format=json", Some("cargo")),
            ("  npm   run lint  ", Some("npm")),
            ("git", Some("git")),
        ] {
            let t = tool_json(&format!(
                r#"{{ "id": "c", "label": "C", "kind": "check", "cmd": {} }}"#,
                serde_json::Value::String(cmd.to_string())
            ));
            assert_eq!(
                probe_command_name(&t, Provenance::User).as_deref(),
                want,
                "cmd `{cmd}`"
            );
        }
        // No `cmd` at all ⇒ nothing to split, and nothing to guess.
        let t = tool_json(r#"{ "id": "c", "label": "C", "kind": "check" }"#);
        assert_eq!(probe_command_name(&t, Provenance::User), None);
    }

    /// Rule 3: a command-kind tool's id IS the CLI's name — that is what the
    /// kind is for, and the shipped pack (`git`, `cargo`, `npm`, `dotnet`, …)
    /// is written that way throughout.
    #[test]
    fn a_command_tool_probes_its_id() {
        let t = tool_json(r#"{ "id": "git", "label": "git", "kind": "command" }"#);
        assert_eq!(
            probe_command_name(&t, Provenance::User).as_deref(),
            Some("git")
        );
    }

    /// Rule 4: an audit/security tool's argv is a template with no program in
    /// it (decision 7 keeps the executable out of the manifest), so there is
    /// nothing to search for — and probing its id would run some unrelated
    /// binary that happened to share the name.
    #[test]
    fn a_user_findings_tool_derives_no_name() {
        for kind in ["audit", "security"] {
            let t = tool_json(&format!(
                r#"{{ "id": "semgrep", "label": "S", "kind": "{kind}", "argv": ["{{root}}"] }}"#
            ));
            assert_eq!(probe_command_name(&t, Provenance::User), None, "{kind}");
        }
    }

    /// The guard: whatever the rules produce must be a BARE NAME, so a hostile
    /// manifest cannot aim the probe at a file of its choosing. Resolution goes
    /// only through the `ebin` → `PATH` ladder.
    #[test]
    fn the_guard_refuses_anything_that_is_not_a_bare_name() {
        for hostile in [
            "..\\evil.exe",
            "../evil.exe",
            "C:\\Windows\\System32\\calc.exe",
            "/usr/bin/id",
            "a/b",
            "a\\b",
            "..",
        ] {
            // Through rule 3 (the id), which a scanned file controls freely…
            let mut t = tool_json(r#"{ "id": "x", "label": "X", "kind": "command" }"#);
            t.id = hostile.to_string();
            assert_eq!(
                probe_command_name(&t, Provenance::User),
                None,
                "id `{hostile}`"
            );

            // …and through rule 2 (the cmd's first token).
            let mut c = tool_json(r#"{ "id": "x", "label": "X", "kind": "check" }"#);
            c.cmd = Some(format!("{hostile} --version"));
            assert_eq!(
                probe_command_name(&c, Provenance::User),
                None,
                "cmd `{hostile}`"
            );
        }

        // An id that is empty or only whitespace is not a name either. (The
        // `cmd` arm of this case is a blank command line, which has no first
        // token at all — see the check test.)
        for blank in ["", "   "] {
            let mut t = tool_json(r#"{ "id": "x", "label": "X", "kind": "command" }"#);
            t.id = blank.to_string();
            assert_eq!(probe_command_name(&t, Provenance::User), None, "id `{blank}`");
        }

        // A built-in `command` that fails the guard yields NOTHING rather than
        // falling back to the id: the author named a binary, and silently
        // probing a different one would be a worse answer than none.
        let mut b = tool_json(r#"{ "id": "cargo", "label": "C", "kind": "command" }"#);
        b.command = Some("..\\evil.exe".to_string());
        assert_eq!(probe_command_name(&b, Provenance::Builtin), None);
    }
}
