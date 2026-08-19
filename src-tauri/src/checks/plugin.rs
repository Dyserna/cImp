//! V38 Phase D — the **effective check set**: this project's configured
//! [`CheckDef`]s plus the `check`-kind tools its enabled plugins contribute,
//! resolved into one list `run_check` advertises, selects from and runs.
//!
//! The audit umbrellas got their plugin twin in Phase C ([`crate::audit::runnable`]);
//! this is the same move for the diagnostics pipeline, and deliberately a
//! *thinner* one. A plugin check does not get a second runner: it is rendered
//! into a `CheckDef` and handed to the existing [`crate::checks::run`], so the
//! shell spawn, the confinement, the timeout floor, the output caps, the parser
//! and dedup machinery, the `changed_only` filter and the V33 sandbox all apply
//! to both populations by construction rather than by care.
//!
//! What this module owns, and why each piece is here:
//!
//! * **Naming.** A plugin check needs a `name` the model can type. It is the
//!   manifest-local tool id — short, and what the plugin author already chose —
//!   *unless* something already answers to that name, in which case the full
//!   `name@version/tool-id` key does. **A configured check is never shadowed**:
//!   the user's own `checks` array is laid down first and a plugin renames
//!   itself around it, because a plugin that could take over the name `cargo`
//!   could make `run_check{name:"cargo"}` run anything at all.
//! * **Rendering.** The manifest's `cmd` is a template. `{root}` and
//!   `{var:NAME}` are substituted ONCE (`audit::runnable::render_argv`'s rule,
//!   for the same reason — see [`SHELL_UNSAFE`]), the program token is replaced
//!   by the configured binary, and the user's `parameters` are appended.
//! * **Screening.** A check runs through the platform SHELL, which the audit
//!   path does not. Variable values and parameters ride the project overlay, so
//!   they are attacker-reachable text being placed into a shell command line —
//!   the one place V38 could turn a declarative manifest into arbitrary code
//!   execution. [`screen_shell_value`] is the boundary.
//! * **Where it runs (V38 F-1).** A check admitted by a marker the census found
//!   in a SUBDIRECTORY runs in that subdirectory. The census matches
//!   recursively, so gating on `Cargo.toml` admitted the starter pack's rust
//!   checks in a repo whose only manifest is `src-tauri/Cargo.toml` — and then
//!   ran them at the root, where `cargo build` exits 101 before doing any work.
//!   [`derived_cwd`] is that fix, and it is deliberately the ONLY inference
//!   here: a manifest-declared `cwd` still wins outright.
//! * **Posture.** The manifest's `runtime`/`sandbox`/`extra_grants` travel with
//!   the rendered def as a [`crate::plugins::posture::ToolPosture`], applied at
//!   the spawn by the same four rules the audit runner uses.
//!
//! What it deliberately does NOT own: which project. Both the advertised set and
//! the dispatched set resolve the registry against the **launch cwd** (Phase C's
//! rule, and what `plugins_project_key` shows in Settings), while the check
//! itself runs against whatever root `run_check` resolved. Keying the registry
//! on the run root instead would let the advertised name list and the runnable
//! name list disagree whenever a graph root is an ancestor of the cwd.

use std::collections::BTreeMap;
use std::path::Path;

use super::CheckDef;
use crate::audit::census::{self, Census};
use crate::plugins::loader::PluginSet;
use crate::plugins::manifest::{ManifestParser, SandboxReq, ToolKind};
use crate::plugins::posture::ToolPosture;
use crate::plugins::registry::{self, EffectiveTool};
use crate::sandbox::SandboxCfg;
use crate::settings::Settings;

/// Characters a substituted VALUE may never contain.
///
/// # This is the shell-injection boundary, and it exists only on this seam
///
/// An audit tool's `{var:NAME}` value becomes one element of an argv vector that
/// is spawned directly — whatever it contains is one argument, and there is no
/// interpreter to re-read it. A CHECK is different in kind: `checks::run` hands
/// its `cmd` to `cmd.exe /C` or `sh -c`, because that is what a check has always
/// been (a command line the operator typed). Substituting into it is therefore
/// substituting into shell SOURCE.
///
/// And the values are not the operator's. Decision 10 puts variable values and
/// CLI parameters in `.cimp/config.json` — inside the project root, which the
/// sandbox grants FULL, which means a compromised repo or a compromised model
/// can write them. `ruleset = "x & calc.exe"` would then be command chaining,
/// from a file cImp deliberately lets the project own.
///
/// Quoting is not the fix. `cmd.exe`'s quoting rules are not
/// `CommandLineToArgvW`'s, `^` escapes survive quotes, `%VAR%` expands inside
/// them, and getting it right on two shells for every value shape is exactly
/// the kind of "we handled it" that ships a hole. Refusing the run is the
/// honest answer, and it costs nothing real: a value that needs `&`, `|`, `$`
/// or a quote in a linter's ruleset name is not a use case, it is an attack.
///
/// Everything else is allowed, including spaces, backslashes and glob
/// characters — a Windows path is a legitimate value, and a glob only changes
/// which files a tool reads, inside a root it can already read.
// `pub(crate)` so `plugins::spec` can pin it against docs/TOOL-PLUGINS.md.
pub(crate) const SHELL_UNSAFE: &[char] = &[
    '&', '|', ';', '<', '>', '(', ')', '`', '$', '"', '\'', '^', '%', '!',
];

/// One check `run_check` can select, whatever it came from.
#[derive(Clone, Debug)]
pub struct EffectiveCheck {
    /// What [`crate::checks::run`] runs. `name` is the advertised id — already
    /// disambiguated, so `settings.checks` and the plugin set never collide.
    pub def: CheckDef,
    /// `None` for a check from the project's own `checks` array.
    pub plugin: Option<PluginCheck>,
}

impl EffectiveCheck {
    /// The posture this check's spawn runs under: the manifest's for a plugin
    /// check, the historical `optional`/infer/no-extra-grants one otherwise.
    pub fn posture(&self, seam: &str, root: &Path, cfg: &SandboxCfg) -> ToolPosture {
        match &self.plugin {
            None => ToolPosture::default(),
            Some(p) => ToolPosture::resolve(
                seam,
                root,
                cfg,
                p.runtime,
                p.sandbox,
                &p.extra_grants,
            ),
        }
    }
}

/// The plugin half of an [`EffectiveCheck`].
#[derive(Clone, Debug)]
pub struct PluginCheck {
    /// `name@version/tool-id` — the registry identity, for error text and for
    /// telling a renamed check apart from a configured one of the same name.
    pub tool_key: String,
    /// The manifest's `label`, for user-facing messages.
    pub label: String,
    pub runtime: crate::plugins::manifest::RuntimeReq,
    pub sandbox: SandboxReq,
    pub extra_grants: Vec<String>,
    /// Set when this check is advertised but CANNOT run, with the reason.
    ///
    /// Advertised anyway, on purpose: the tool is enabled and has a path, so the
    /// user believes it exists. Dropping it from the enum would make a
    /// configured capability vanish with no explanation anywhere; the Phase C
    /// analogue is `ToolState::failed_to_plan`, which keeps a broken plugin tool
    /// visible as a failed chip rather than absent. The cost is one round trip
    /// that returns a real reason instead of "no configured check named …".
    pub error: Option<String>,
}

/// The effective set for a project — configured checks first, then plugin
/// checks in registry order (plugin key, then manifest tool order).
///
/// Pure: a settings snapshot, a plugin set, and two roots in; a `Vec` out. No
/// globals and no clock, so every rule below — naming, collision, screening,
/// posture — is assertable without a `PluginStore`, an `AppHandle` or a disk.
///
/// * `registry_root` keys the per-project binary-path map (the launch cwd).
/// * `run_root` is where the check will actually execute — the value `{root}`
///   renders to. The two are the same everywhere except a `run_check` call whose
///   graph root is an ancestor of the cwd.
/// * `census` is the project shape `run_root` was walked for, and it is what
///   makes a manifest's `applicability` a GATE here rather than a decoration.
///
/// # Why applicability gates the ADVERTISED set and not only the run
///
/// V38 Phase F closed the framework gap the plugin-pack pass found: the field
/// validated and nothing read it on this kind. It is applied at exactly one
/// point — this function — which is both the advertised name list
/// ([`effective_check_names`]) and the dispatched one, so a name a model can see
/// is always a name that would run. Gating only the run would have advertised
/// `maven-test` in a Gradle project and answered the call with a refusal, which
/// is a worse answer than not offering it.
///
/// The consequence, stated rather than hidden: the advertised `run_check` schema
/// is now project-SHAPE dependent as well as project-CONFIG dependent. Adding a
/// `pom.xml` moves the surface, and nothing watches the filesystem for that — the
/// census ages out in ≤60s and the next advertisement picks it up. That is the
/// same eventual consistency the audit chips have had since V25.
pub fn effective_checks(
    settings: &Settings,
    set: &PluginSet,
    registry_root: Option<&Path>,
    run_root: &Path,
    census: &Census,
) -> Vec<EffectiveCheck> {
    let mut out: Vec<EffectiveCheck> = settings
        .checks
        .iter()
        .map(|def| EffectiveCheck {
            def: def.clone(),
            plugin: None,
        })
        .collect();

    for tool in registry::runnable_tools(set, &settings.tool_plugins, registry_root) {
        if tool.kind() != ToolKind::Check {
            continue;
        }
        // The manifest's project-shape gate, applied through the SAME function
        // the audit fan-out applies (`Census::admits`) — one rule, so a tool
        // cannot be applicable under an umbrella and inapplicable here.
        if !census.admits(&tool.manifest.applicability) {
            continue;
        }
        let taken: Vec<&str> = out.iter().map(|c| c.def.name.as_str()).collect();
        let name = registry::advertised_name(&tool, &taken);
        out.push(build(&tool, name, run_root, census));
    }
    out
}

/// [`effective_checks`] against the live plugin set and this process's project.
///
/// The registry root is `current_dir()`: in the app that is the launch cwd (the
/// audit fan-out's root "by construction", `main.rs`), and in an
/// `--offload-mcp` child it is the tab's project. Both are the directory
/// Settings shows per-project paths for.
pub fn effective_checks_live(settings: &Settings, run_root: &Path) -> Vec<EffectiveCheck> {
    let cwd = std::env::current_dir().ok();
    effective_checks(
        settings,
        &crate::plugins::snapshot_or_scan(),
        cwd.as_deref(),
        run_root,
        // The CACHED census (≤60 s TTL, bounded walk): in the app this is the
        // very walk the audit runner already paid for on the same root, and in
        // an `--offload-mcp` child it is one bounded walk per minute per root —
        // the same order of cost as the once-per-process plugin scan directly
        // above it. Taking a fresh one per advertisement would put a filesystem
        // walk on `tools/list`.
        &census::cached(run_root),
    )
}

/// [`effective_checks_live`] for a caller that only needs the NAMES — the
/// advertised `run_check` schema and its fingerprint.
///
/// Uses the cwd as the run root too: `{root}` does not reach a name, so the
/// rendering it feeds is discarded. Going through the one builder anyway is the
/// point — a second, name-only path is how an advertised name ends up naming a
/// check the dispatcher cannot find.
pub fn effective_check_names(settings: &Settings) -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    effective_checks_live(settings, &cwd)
        .into_iter()
        .map(|c| c.def.name)
        .collect()
}

/// **V38 F-1 — where a plugin check runs when its marker is not at the root.**
///
/// The defect this closes, in one line: `Census::admits` matches RECURSIVELY, so
/// `src-tauri/Cargo.toml` admitted the starter pack's rust checks in this very
/// repo, and the rendered `CheckDef` then ran `cargo build` at the project root,
/// where it exits 101 in 140 ms with "could not find `Cargo.toml`". The census
/// and the run cwd disagreed about which directory the tool applies to.
///
/// The precedence, in the order the code reads it:
///
/// 1. **A manifest-declared `cwd` always wins.** Explicit beats inferred — an
///    author who named a directory knows something the census cannot see, and a
///    heuristic that could override them would make the field unreliable.
/// 2. **A root hit means the root**, i.e. exactly the pre-F-1 behaviour. This is
///    the ordinary case and it must not move.
/// 3. Otherwise the **shallowest** directory a listed marker was seen in, ties
///    broken lexicographically ([`census::dir_is_better`], the same ordering the
///    census itself applied while walking).
///
/// # Limits, stated rather than discovered later
///
/// * **One check, one directory.** A monorepo with four `pom.xml`s gets one
///   maven check in one of them, not four. Multi-instance fan-out is a different
///   feature (it changes the advertised NAME set, not just the cwd) and is out
///   of scope here; § 3.5 of the contract doc says so too.
/// * **Markers only.** `extensions` carry no location — the census records which
///   extensions exist, not where — so a tool gated only on `extensions` keeps
///   running at the root. That is the honest answer: "some `.java` file exists
///   somewhere" does not name a build directory.
fn derived_cwd(
    m: &crate::plugins::manifest::ToolManifest,
    census: &Census,
) -> Option<String> {
    // Rule 1. `filter` rather than `is_some`: an empty string is how a manifest
    // spells "no cwd" once it has been through serde, and `run` treats it that
    // way too.
    if m.cwd.as_deref().filter(|s| !s.is_empty()).is_some() {
        return None;
    }
    let mut best: Option<&str> = None;
    for marker in &m.applicability.markers {
        let Some(dir) = census.marker_dir(marker) else {
            continue;
        };
        if best.is_none_or(|current| census::dir_is_better(dir, current)) {
            best = Some(dir);
        }
    }
    // Rule 2: the root is `Some("")` here, and `CheckDef::cwd = None` is how the
    // pipeline spells the root. Returning `Some("")` would be the same directory
    // written a second way, and every reader would have to know both.
    let best = best.filter(|d| !d.is_empty())?;
    // The derived value is cImp's own — a `strip_prefix` of one walked path
    // against the root it was walked from — so it is relative and `..`-free by
    // construction. It is checked anyway, by the SAME function `CheckDef::validate`
    // calls, because "by construction" is a claim about today's walker and the
    // confinement is a security property: a value that cannot pass the check is
    // dropped in favour of the root rather than handed to a spawn.
    match super::lexically_confined("cwd", best) {
        Ok(()) => Some(best.to_string()),
        Err(_) => None,
    }
}

/// Render one registry entry into an [`EffectiveCheck`].
///
/// Every failure produces an advertised-but-broken check carrying the reason,
/// never a silent omission — see [`PluginCheck::error`].
fn build(
    tool: &EffectiveTool,
    name: String,
    run_root: &Path,
    census: &Census,
) -> EffectiveCheck {
    let m = &tool.manifest;
    let plugin = |error: Option<String>| PluginCheck {
        tool_key: tool.tool_key.clone(),
        label: m.label.clone(),
        runtime: m.runtime,
        sandbox: m.sandbox,
        extra_grants: m.extra_grants.clone(),
        error,
    };
    // `runnable_tools` guarantees a path; `program` restates it rather than
    // unwrapping, so a caller that filtered differently gets a reason instead of
    // a panic.
    let Some(program) = tool.path.as_deref() else {
        return EffectiveCheck {
            def: CheckDef {
                name,
                ..CheckDef::default()
            },
            plugin: Some(plugin(Some("no binary path is configured".to_string()))),
        };
    };

    let parser = match m.parser {
        // Kind-disambiguated (ruling G2): a check's `parser` is a DIAGNOSTICS
        // parser. Validation already refused the findings namespace here, so the
        // `Legacy` arm is unreachable through the loader — and an error rather
        // than a silent default, because decoding output with the wrong parser
        // yields zero diagnostics, which reads exactly like a clean run.
        Some(ManifestParser::Kind(k)) => Ok(k),
        None => Ok(super::ParserKind::default()),
        Some(ManifestParser::Legacy(l)) => Err(format!(
            "its manifest names the `{}` parser, which decodes audit FINDINGS and not \
             diagnostics",
            l.as_str()
        )),
    };
    let cmd = m
        .cmd
        .as_deref()
        .ok_or_else(|| "its manifest declares no `cmd`".to_string())
        .and_then(|t| render_cmd(t, &tool.variables, run_root))
        .and_then(|rendered| inject_program(&rendered, program))
        .and_then(|with_program| append_parameters(with_program, &tool.parameters));

    match (parser, cmd) {
        (Ok(parser), Ok(cmd)) => EffectiveCheck {
            def: CheckDef {
                name,
                cmd,
                parser,
                // The pipeline default when the manifest and the user are both
                // silent — `CheckDef::default()`'s 120s, not a second constant.
                timeout_secs: tool.timeout_secs.unwrap_or(CheckDef::default().timeout_secs),
                // F-1: the manifest's directory, else the one its own
                // applicability marker was found in. See [`derived_cwd`].
                cwd: m.cwd.clone().or_else(|| derived_cwd(m, census)),
                env: m.env.clone(),
                report_file: m.report_file.clone(),
                pattern: m.pattern.clone(),
                // `auto` is the language-detection flag for `settings.checks`
                // upkeep (`detect::merge_auto` may refresh an `auto` entry). A
                // plugin check is never written to that array, so the honest
                // value is the hand-authored one.
                auto: false,
            },
            plugin: Some(plugin(None)),
        },
        (parser, cmd) => {
            let why = parser.err().or_else(|| cmd.err()).unwrap_or_default();
            EffectiveCheck {
                def: CheckDef {
                    name,
                    ..CheckDef::default()
                },
                plugin: Some(plugin(Some(why))),
            }
        }
    }
}

/// Substitute `{root}` and `{var:NAME}` into a check's command template —
/// **once**, left to right, never over the result.
///
/// Reuses `audit::runnable`'s substituter rather than re-deriving it: the
/// single-pass rule is a security property (a value must never be re-scanned
/// for tokens), and two implementations of a security property is one
/// implementation plus a liability. `{report}` is refused at validation for this
/// kind, so the report slot is empty here by contract.
fn render_cmd(
    template: &str,
    variables: &BTreeMap<String, String>,
    run_root: &Path,
) -> Result<String, String> {
    for (name, value) in variables {
        screen_shell_value(&format!("the variable `{name}`"), value)?;
    }
    Ok(
        crate::audit::runnable::render_argv(
            std::slice::from_ref(&template.to_string()),
            variables,
            run_root,
            None,
        )
        .pop()
        .unwrap_or_default(),
    )
}

/// Refuse a value that would stop being data once it reached a shell.
/// See [`SHELL_UNSAFE`] for why this is a refusal and not an escaping routine.
fn screen_shell_value(what: &str, value: &str) -> Result<(), String> {
    if let Some(bad) = value.chars().find(|c| {
        SHELL_UNSAFE.contains(c) || c.is_control()
    }) {
        return Err(format!(
            "{what} contains `{}`, which the shell would read as syntax rather than as text. A \
             check's command line runs through the platform shell, and these values can be set \
             per project (`.cimp/config.json`) — so cImp refuses the run instead of quoting \
             around it. Remove the character, or move the behaviour into the plugin's `cmd`.",
            bad.escape_default()
        ));
    }
    Ok(())
}

/// Replace the command line's program token with the configured binary.
///
/// **Decision 7 is what makes this necessary**: a manifest never names an
/// executable, the user supplies every path, and a check's `cmd` is a whole
/// command line whose first token is the program. So the template's `gradle` is
/// a placeholder for "the gradle the user pointed at", and rendering it verbatim
/// would resolve through PATH — the one thing decision 7 says cImp never does
/// for a plugin.
///
/// The rewrite is byte-exact behind the token (`split_first_shell_token` returns
/// where the rest begins), so arguments are never re-quoted or re-parsed. On
/// Windows the quoted drive-qualified head is what `cmd.exe` needs on the plain
/// path, and the SANDBOXED path already rewrites exactly this shape into a bare
/// file name plus a PATH prefix (`sandboxed_raw_tail`) — the drive-designator
/// finding from the rc.9 bisect. Both paths are therefore already handled.
fn inject_program(cmd: &str, program: &str) -> Result<String, String> {
    if program.contains('"') {
        return Err(format!(
            "its configured path contains a quote character (`{program}`), which cannot be \
             placed into a shell command line"
        ));
    }
    let Some((_, rest)) = super::split_first_shell_token(cmd) else {
        return Err(format!(
            "its manifest's `cmd` (`{cmd}`) does not begin with a plain program name, so cImp \
             cannot substitute the binary you configured for it"
        ));
    };
    Ok(format!("\"{program}\"{}", &cmd[rest..]))
}

/// Append the user's extra CLI parameters, screened and space-quoted.
///
/// Verbatim in ORDER, like the audit path's `extra_args` successor — the
/// difference is only that a shell command line has to be given the separator
/// the argv vector got for free. A parameter containing a space is wrapped in
/// double quotes, which is safe precisely because [`screen_shell_value`] has
/// already refused every parameter that could contain one of its own.
fn append_parameters(mut cmd: String, parameters: &[String]) -> Result<String, String> {
    for p in parameters {
        screen_shell_value("an extra CLI parameter", p)?;
        if p.is_empty() {
            continue;
        }
        cmd.push(' ');
        if p.contains(' ') {
            cmd.push('"');
            cmd.push_str(p);
            cmd.push('"');
        } else {
            cmd.push_str(p);
        }
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::loader::scan_dir;
    use crate::plugins::manifest::Provenance;
    use crate::settings::{PluginState, ToolState};
    use std::path::PathBuf;

    /// One plugin with two check tools and a command tool, built through the
    /// real loader so every assertion below runs against a VALIDATED manifest.
    fn fixture(extra_tool_id: &str) -> (PluginSet, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cimp-checkplugin-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("acme.json"),
            format!(
                r#"{{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "categories": [{{ "id": "c", "label": "C", "tools": ["lint", "{extra_tool_id}", "sh"] }}],
              "tools": [
                {{
                  "id": "lint", "label": "Acme Lint", "kind": "check",
                  "cmd": "acmelint --profile {{var:profile}} --root \"{{root}}\"",
                  "parser": "generic-gcc",
                  "variables": [{{ "name": "profile", "label": "Profile", "default": "ci" }}],
                  "parameters_allowed": true,
                  "timeout_secs": 300,
                  "runtime": "node",
                  "sandbox": "optional"
                }},
                {{ "id": "{extra_tool_id}", "label": "Acme Types", "kind": "check", "cmd": "acmetypes" }},
                {{ "id": "sh", "label": "Acme Shell", "kind": "command" }}
              ]
            }}"#
            ),
        )
        .expect("write manifest");
        let set = scan_dir(&dir, Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        (set, dir)
    }

    fn configured(cfg: &mut Settings, tool_id: &str, path: &str) {
        cfg.tool_plugins.plugins.insert(
            "acme@1.0.0".to_string(),
            PluginState {
                enabled: true,
                tools: BTreeMap::from([(tool_id.to_string(), ToolState::default())]),
            },
        );
        cfg.tool_plugins
            .global_paths
            .insert(format!("acme@1.0.0/{tool_id}"), path.to_string());
    }

    fn find<'a>(v: &'a [EffectiveCheck], name: &str) -> &'a EffectiveCheck {
        v.iter()
            .find(|c| c.def.name == name)
            .unwrap_or_else(|| panic!("no check `{name}` in {:?}", v.iter().map(|c| &c.def.name)))
    }

    /// The happy path end to end: the manifest id becomes the advertised name,
    /// the template's tokens are substituted, the CONFIGURED binary replaces the
    /// program token, and the user's parameters are appended.
    #[test]
    fn a_plugin_check_renders_into_a_runnable_checkdef() {
        let (set, dir) = fixture("types");
        let mut s = Settings::default();
        configured(&mut s, "lint", "C:\\tools\\acmelint.exe");
        s.tool_plugins
            .plugins
            .get_mut("acme@1.0.0")
            .unwrap()
            .tools
            .get_mut("lint")
            .unwrap()
            .parameters = vec!["--quiet".into(), "src dir".into()];

        let root = PathBuf::from("C:\\proj");
        let checks = effective_checks(&s, &set, None, &root, &Census::default());
        let lint = find(&checks, "lint");
        assert_eq!(
            lint.def.cmd,
            "\"C:\\tools\\acmelint.exe\" --profile ci --root \"C:\\proj\" --quiet \"src dir\""
        );
        assert_eq!(lint.def.timeout_secs, 300);
        assert_eq!(lint.def.parser, super::super::ParserKind::GenericGcc);
        let p = lint.plugin.as_ref().expect("plugin metadata");
        assert!(p.error.is_none(), "{:?}", p.error);
        assert_eq!(p.tool_key, "acme@1.0.0/lint");
        // The `command`-kind tool is Phase D's OTHER population, not this one.
        assert!(checks.iter().all(|c| c.def.name != "sh"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plugin tool with no configured path is not a check at all — the
    /// registry's runnable filter drops it before naming, so it cannot occupy
    /// the name a later, working tool would have taken.
    #[test]
    fn an_unconfigured_plugin_contributes_no_checks() {
        let (set, dir) = fixture("types");
        let s = Settings::default();
        assert!(effective_checks(&s, &set, None, Path::new("C:\\proj"), &Census::default()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A plugin can never take a configured check's name.** The user's array
    /// is laid down first; the plugin renames itself to its fully-qualified key.
    #[test]
    fn a_configured_check_is_never_shadowed() {
        let (set, dir) = fixture("types");
        let mut s = Settings::default();
        s.checks.push(CheckDef {
            name: "lint".to_string(),
            cmd: "the users own linter".to_string(),
            ..CheckDef::default()
        });
        configured(&mut s, "lint", "C:\\tools\\acmelint.exe");

        let checks = effective_checks(&s, &set, None, Path::new("C:\\proj"), &Census::default());
        assert_eq!(checks.len(), 2);
        assert!(
            find(&checks, "lint").plugin.is_none(),
            "the configured check keeps the name it had"
        );
        assert_eq!(find(&checks, "lint").def.cmd, "the users own linter");
        let renamed = find(&checks, "acme@1.0.0/lint");
        assert_eq!(
            renamed.plugin.as_ref().unwrap().tool_key,
            "acme@1.0.0/lint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two plugin tools whose ids collide (here: a manifest id equal to another
    /// tool's) both stay reachable, deterministically.
    #[test]
    fn two_plugin_checks_of_the_same_id_both_stay_reachable() {
        // A second plugin declaring the SAME tool id as the first.
        let (mut set, dir) = fixture("types");
        let dir2 = std::env::temp_dir().join(format!("cimp-checkplugin2-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir2);
        std::fs::create_dir_all(&dir2).expect("temp dir");
        std::fs::write(
            dir2.join("other.json"),
            r#"{
              "manifest_version": 1,
              "name": "other",
              "version": "2.0.0",
              "categories": [{ "id": "c", "label": "C", "tools": ["lint"] }],
              "tools": [{ "id": "lint", "label": "Other Lint", "kind": "check", "cmd": "otherlint" }]
            }"#,
        )
        .expect("write manifest");
        let second = scan_dir(&dir2, Provenance::User);
        set.plugins.extend(second.plugins);

        let mut s = Settings::default();
        configured(&mut s, "lint", "C:\\tools\\acmelint.exe");
        s.tool_plugins.plugins.insert(
            "other@2.0.0".to_string(),
            PluginState {
                enabled: true,
                tools: BTreeMap::from([("lint".to_string(), ToolState::default())]),
            },
        );
        s.tool_plugins
            .global_paths
            .insert("other@2.0.0/lint".to_string(), "C:\\tools\\other.exe".into());

        let checks = effective_checks(&s, &set, None, Path::new("C:\\proj"), &Census::default());
        assert_eq!(checks.len(), 2);
        // First in registry order keeps the short id; the second qualifies.
        assert_eq!(checks[0].def.name, "lint");
        assert_eq!(checks[1].def.name, "other@2.0.0/lint");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// **The injection boundary.** A variable value carrying shell syntax
    /// refuses the run with a reason — it is never quoted around, and it never
    /// reaches a command line.
    #[test]
    fn an_overlay_supplied_value_cannot_inject_shell_syntax() {
        for hostile in ["ci & calc.exe", "ci`whoami`", "ci$(id)", "ci\" && echo x", "ci%PATH%"] {
            let (set, dir) = fixture("types");
            let mut s = Settings::default();
            configured(&mut s, "lint", "C:\\tools\\acmelint.exe");
            s.tool_plugins
                .plugins
                .get_mut("acme@1.0.0")
                .unwrap()
                .tools
                .get_mut("lint")
                .unwrap()
                .variables
                .insert("profile".to_string(), hostile.to_string());

            let checks = effective_checks(&s, &set, None, Path::new("C:\\proj"), &Census::default());
            let lint = find(&checks, "lint");
            let err = lint
                .plugin
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .unwrap_or_else(|| panic!("`{hostile}` must be refused, got: {}", lint.def.cmd));
            assert!(err.contains("shell would read as syntax"), "{err}");
            assert!(
                lint.def.cmd.is_empty(),
                "a refused check must carry no command line: {}",
                lint.def.cmd
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The same boundary applies to appended parameters, which ride the same
    /// overlay.
    #[test]
    fn an_overlay_supplied_parameter_cannot_inject_shell_syntax() {
        let (set, dir) = fixture("types");
        let mut s = Settings::default();
        configured(&mut s, "lint", "C:\\tools\\acmelint.exe");
        s.tool_plugins
            .plugins
            .get_mut("acme@1.0.0")
            .unwrap()
            .tools
            .get_mut("lint")
            .unwrap()
            .parameters = vec!["--x; rm -rf /".into()];

        let lint = find(
            &effective_checks(&s, &set, None, Path::new("C:\\proj"), &Census::default()),
            "lint",
        )
        .clone();
        assert!(lint.plugin.unwrap().error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Values with spaces, backslashes and globs are legitimate and must keep
    /// working — the screen is a syntax boundary, not a "no punctuation" rule.
    #[test]
    fn ordinary_values_still_substitute() {
        let (set, dir) = fixture("types");
        let mut s = Settings::default();
        configured(&mut s, "lint", "C:\\Program Files\\acme\\acmelint.exe");
        s.tool_plugins
            .plugins
            .get_mut("acme@1.0.0")
            .unwrap()
            .tools
            .get_mut("lint")
            .unwrap()
            .variables
            .insert("profile".to_string(), "C:\\rules\\*.yml".to_string());

        let lint = find(
            &effective_checks(&s, &set, None, Path::new("C:\\proj"), &Census::default()),
            "lint",
        )
        .clone();
        assert!(lint.plugin.unwrap().error.is_none());
        assert_eq!(
            lint.def.cmd,
            "\"C:\\Program Files\\acme\\acmelint.exe\" --profile C:\\rules\\*.yml --root \"C:\\proj\""
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A configured check's posture is the pre-plugin one, whatever a plugin
    /// alongside it declares — the two populations must not bleed.
    #[test]
    fn a_configured_check_keeps_the_pre_plugin_posture() {
        let s = Settings::default();
        let c = EffectiveCheck {
            def: CheckDef::default(),
            plugin: None,
        };
        let cfg = SandboxCfg::disabled();
        let p = c.posture("run_check", Path::new("."), &cfg);
        assert_eq!(p.sandbox, SandboxReq::Optional);
        assert!(p.rows.is_empty());
        let _ = s;
    }

    /// **The posture reaches the spawn.** A plugin check declaring
    /// `sandbox: required` is NOT run when the boundary cannot be provided —
    /// including when the user switched the sandbox off, the case a plugin
    /// author cannot see. Run through the real `checks::run_with_posture`, so
    /// this asserts the wiring and not just the helper.
    #[tokio::test]
    async fn a_required_sandbox_plugin_check_refuses_to_run_unprotected() {
        let def = CheckDef {
            name: "acme-lint".to_string(),
            cmd: "echo hi".to_string(),
            ..CheckDef::default()
        };
        let posture = ToolPosture {
            sandbox: SandboxReq::Required,
            ..ToolPosture::default()
        };
        let err = super::super::run_with_posture(
            &std::env::temp_dir(),
            &def,
            false,
            &SandboxCfg::disabled(),
            &posture,
        )
        .await
        .expect_err("`required` must refuse rather than run unprotected");
        assert!(err.to_string().contains("sandbox: required"), "{err}");

        // …and the same check with the DEFAULT posture (every configured check)
        // runs exactly as it always has, sandbox off and all.
        super::super::run_with_posture(
            &std::env::temp_dir(),
            &def,
            false,
            &SandboxCfg::disabled(),
            &ToolPosture::default(),
        )
        .await
        .expect("a configured check still runs with the sandbox off");
    }

    /// The program token is replaced byte-exactly behind its own end — the
    /// arguments are never re-quoted, whatever they contain.
    #[test]
    fn injecting_the_program_leaves_the_arguments_untouched() {
        assert_eq!(
            inject_program("tool --a \"b c\" | grep x", "C:\\p q\\tool.exe").unwrap(),
            "\"C:\\p q\\tool.exe\" --a \"b c\" | grep x"
        );
        // A line that does not begin with a plain program name has nothing to
        // substitute into, and says so instead of running the wrong thing.
        assert!(inject_program("FOO=1 tool", "C:\\tool.exe").is_err());
        assert!(inject_program("", "C:\\tool.exe").is_err());
    }
}

#[cfg(test)]
mod applicability_tests {
    use super::*;
    use crate::plugins::loader::scan_dir;
    use crate::plugins::manifest::Provenance;
    use crate::settings::{PluginState, ToolState};
    use std::path::PathBuf;

    /// Two check tools gated on different project shapes, plus one ungated —
    /// the § 8 worked example, written as a manifest.
    fn gated_fixture() -> (PluginSet, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cimp-checkgate-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("acme.json"),
            r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "categories": [{ "id": "c", "label": "C", "tools": ["mvn-build", "gr-build", "any"] }],
              "tools": [
                { "id": "mvn-build", "label": "Maven", "kind": "check", "cmd": "mvn compile",
                  "applicability": { "markers": ["pom.xml"] } },
                { "id": "gr-build", "label": "Gradle", "kind": "check", "cmd": "gradle build",
                  "applicability": { "markers": ["build.gradle"] } },
                { "id": "any", "label": "Anywhere", "kind": "check", "cmd": "anytool" }
              ]
            }"#,
        )
        .expect("write manifest");
        let set = scan_dir(&dir, Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        (set, dir)
    }

    fn all_configured() -> Settings {
        let mut cfg = Settings::default();
        let mut tools = BTreeMap::new();
        for id in ["mvn-build", "gr-build", "any"] {
            tools.insert(id.to_string(), ToolState::default());
            cfg.tool_plugins
                .global_paths
                .insert(format!("acme@1.0.0/{id}"), "C:\\tools\\t.exe".to_string());
        }
        cfg.tool_plugins.plugins.insert(
            "acme@1.0.0".to_string(),
            PluginState {
                enabled: true,
                tools,
            },
        );
        cfg
    }

    fn names(set: &PluginSet, cfg: &Settings, markers: &[&str]) -> Vec<String> {
        let census = crate::audit::census::Census::from_block(
            &[],
            &markers.iter().map(|m| (*m).to_string()).collect::<Vec<_>>(),
        );
        effective_checks(cfg, set, None, &PathBuf::from("C:\\proj"), &census)
            .into_iter()
            .filter(|c| c.plugin.is_some())
            .map(|c| c.def.name)
            .collect()
    }

    /// V38 Phase F — `applicability` is a GATE on `check`-kind tools, not a
    /// decoration. Before this phase the field validated and nothing read it,
    /// which made the contract doc's "`pom.xml` → maven" promise untrue.
    ///
    /// Both directions and the ungated case, because "no gate = always
    /// applicable" is the half that makes adopting the rule safe.
    #[test]
    fn a_gated_check_is_advertised_only_where_its_marker_is() {
        let (set, dir) = gated_fixture();
        let cfg = all_configured();

        let maven = names(&set, &cfg, &["pom.xml"]);
        assert!(maven.contains(&"mvn-build".to_string()), "{maven:?}");
        assert!(!maven.contains(&"gr-build".to_string()), "{maven:?}");
        assert!(maven.contains(&"any".to_string()), "{maven:?}");

        let gradle = names(&set, &cfg, &["build.gradle"]);
        assert!(gradle.contains(&"gr-build".to_string()), "{gradle:?}");
        assert!(!gradle.contains(&"mvn-build".to_string()), "{gradle:?}");

        // Neither marker: only the ungated tool survives. A census that saw
        // nothing is not a licence to advertise everything.
        let neither = names(&set, &cfg, &[]);
        assert_eq!(neither, vec!["any".to_string()], "{neither:?}");

        // Both: the two gated tools coexist and the harness picks — decision 6,
        // "cImp does not arbitrate".
        let both = names(&set, &cfg, &["pom.xml", "build.gradle"]);
        assert_eq!(both.len(), 3, "{both:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **V38 F-1 — a check whose marker is nested runs where the marker is.**
    ///
    /// The live defect, as a test: the census admits a `Cargo.toml`-gated check
    /// in a repo whose only manifest is `src-tauri/Cargo.toml`, and before this
    /// the rendered def carried no `cwd` at all — so `cargo build` ran at the
    /// root and exited 101 in 140 ms.
    #[test]
    fn a_check_admitted_by_a_nested_marker_runs_in_that_directory() {
        let (set, dir) = gated_fixture();
        let cfg = all_configured();
        let census =
            crate::audit::census::Census::from_parts_with_dirs(&[], &[("pom.xml", "services/api")]);
        let checks = effective_checks(&cfg, &set, None, &PathBuf::from("C:\\proj"), &census);
        let mvn = checks
            .iter()
            .find(|c| c.def.name == "mvn-build")
            .expect("the gated check is advertised");
        assert_eq!(mvn.def.cwd.as_deref(), Some("services/api"));
        // The UNGATED check is untouched: no marker, no inference, and "runs at
        // the root" stays the default for everything that does not ask.
        let any = checks.iter().find(|c| c.def.name == "any").expect("ungated");
        assert_eq!(any.def.cwd, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A marker at the ROOT means the root — the pre-F-1 behaviour, unmoved.
    /// `cwd: None` and `cwd: Some("")` are the same directory, and the pipeline
    /// spells it exactly one way.
    #[test]
    fn a_root_marker_still_runs_at_the_root() {
        let (set, dir) = gated_fixture();
        let cfg = all_configured();
        let census = crate::audit::census::Census::from_parts_with_dirs(&[], &[("pom.xml", "")]);
        let checks = effective_checks(&cfg, &set, None, &PathBuf::from("C:\\proj"), &census);
        assert_eq!(
            checks
                .iter()
                .find(|c| c.def.name == "mvn-build")
                .expect("advertised")
                .def
                .cwd,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Explicit beats inferred.** A manifest that names a `cwd` keeps it, even
    /// when the census would have derived a different one — the author knows
    /// something the walk cannot see, and an inference that could override them
    /// would make the field unreliable.
    #[test]
    fn a_manifest_declared_cwd_wins_over_the_derived_one() {
        let dir = std::env::temp_dir().join(format!("cimp-checkcwd-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("acme.json"),
            r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "categories": [{ "id": "c", "label": "C", "tools": ["mvn-build"] }],
              "tools": [
                { "id": "mvn-build", "label": "Maven", "kind": "check", "cmd": "mvn compile",
                  "cwd": "declared/here",
                  "applicability": { "markers": ["pom.xml"] } }
              ]
            }"#,
        )
        .expect("write manifest");
        let set = scan_dir(&dir, Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);

        let mut cfg = Settings::default();
        cfg.tool_plugins.plugins.insert(
            "acme@1.0.0".to_string(),
            PluginState {
                enabled: true,
                tools: BTreeMap::from([("mvn-build".to_string(), ToolState::default())]),
            },
        );
        cfg.tool_plugins
            .global_paths
            .insert("acme@1.0.0/mvn-build".to_string(), "C:\\t\\mvn.exe".to_string());

        let census = crate::audit::census::Census::from_parts_with_dirs(
            &[],
            &[("pom.xml", "services/api")],
        );
        let checks = effective_checks(&cfg, &set, None, &PathBuf::from("C:\\proj"), &census);
        assert_eq!(checks[0].def.cwd.as_deref(), Some("declared/here"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Several markers, several hits: the SHALLOWEST wins and equal depths break
    /// lexicographically, so two walks of the same tree cannot render two
    /// different commands.
    #[test]
    fn the_derived_directory_is_deterministic() {
        let (set, dir) = gated_fixture();
        let cfg = all_configured();
        // `gr-build` gates on `build.gradle` alone; give the census two hits for
        // the OTHER marker so the tie-break is what decides `mvn-build`.
        let deep = crate::audit::census::Census::from_parts_with_dirs(
            &[],
            &[("pom.xml", "b/one"), ("build.gradle", "a")],
        );
        let checks = effective_checks(&cfg, &set, None, &PathBuf::from("C:\\proj"), &deep);
        let find = |n: &str| {
            checks
                .iter()
                .find(|c| c.def.name == n)
                .unwrap_or_else(|| panic!("{n}"))
                .def
                .cwd
                .clone()
        };
        assert_eq!(find("mvn-build").as_deref(), Some("b/one"));
        assert_eq!(find("gr-build").as_deref(), Some("a"), "shallower wins");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The derived value is confined by the SAME rule `CheckDef::validate`
    /// applies, so a rendered plugin check can never carry a `cwd` the pipeline
    /// would refuse at spawn.
    #[test]
    fn every_derived_cwd_passes_the_checkdef_confinement() {
        let (set, dir) = gated_fixture();
        let cfg = all_configured();
        let census = crate::audit::census::Census::from_parts_with_dirs(
            &[],
            &[("pom.xml", "services/api"), ("build.gradle", "tools")],
        );
        for c in effective_checks(&cfg, &set, None, &PathBuf::from("C:\\proj"), &census) {
            c.def
                .validate()
                .unwrap_or_else(|e| panic!("`{}` rendered an invalid def: {e}", c.def.name));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One rule, two populations. The audit fan-out asks `Census::admits` and so
    /// does this pipeline, so a manifest gate cannot mean one thing under an
    /// umbrella and another under `run_check`.
    #[test]
    fn the_gate_is_the_same_function_the_audit_fanout_uses() {
        use crate::plugins::manifest::Applicability;
        let census = crate::audit::census::Census::from_block(
            &[],
            &["pom.xml".to_string()],
        );
        let gate = Applicability {
            extensions: Vec::new(),
            markers: vec!["pom.xml".to_string()],
        };
        assert!(census.admits(&gate));
        assert!(
            census.admits(&Applicability::default()),
            "no gate = always applicable"
        );
        assert!(!census.admits(&Applicability {
            extensions: Vec::new(),
            markers: vec!["build.gradle".to_string()],
        }));
    }
}
