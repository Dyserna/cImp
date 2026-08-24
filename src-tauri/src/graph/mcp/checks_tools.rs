//! `run_check` / `run_command` — the two tools this MCP surface serves that are
//! **independent of the graph** (V12 Phase A; V38 F-3 for the second).
//!
//! They are advertised beside the `graph_*` set and dispatched through the same
//! two entry points, but nothing about them is graph-shaped: each needs a
//! project ROOT and no built index, each runs a user-vetted command, and each
//! reads [`crate::checks`] rather than the graph store. V42 R8 gave them their
//! own file so the module boundary says so.
//!
//! What lives here: the two specs (both project-scoped — their `enum`s carry
//! this project's configured names), the two hashes that let the surface
//! fingerprint notice a change to either ([`checks_sig`] / [`commands_sig`],
//! read by [`super::surface::SurfaceFingerprint`]), the execution path, the
//! report renderer, and the worker-native [`offload_run_check`].

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::tools::GraphToolSpec;
use super::{activity_response, current_settings, db_subdir, find_graph_root, limits};

/// Hash the EFFECTIVE check names, in order — every input [`run_check_spec`]
/// reads. Process-local memo key only, so `DefaultHasher`'s
/// unstable-across-releases hash is fine; it never leaves this process.
pub(super) fn checks_sig(settings: &crate::settings::Settings) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let names = crate::checks::plugin::effective_check_names(settings);
    names.len().hash(&mut h);
    for name in &names {
        name.hash(&mut h);
    }
    h.finish()
}

/// **V38 F-3 — whether `run_command` is listed for this consumer at all.**
///
/// Two gates, ANDed, and the second is why the first can default to on:
///
/// * the user's per-consumer exposure switch, and
/// * a non-empty runnable set — a tool advertised with an empty `tool` enum is
///   a capability nobody can use and a schema nobody can satisfy, which is
///   strictly worse than an absent tool.
///
/// Pure, and separate from [`tools_for`], so both directions of both gates are
/// assertable without a plugins directory or a live settings file.
pub(super) fn commands_advertised(
    settings: &crate::settings::Settings,
    consumer: &str,
    names: &[String],
) -> bool {
    commands_exposed_to(settings, consumer) && !names.is_empty()
}

/// Whether `run_command` is advertised to the harness behind this `--consumer`
/// token.
///
/// V40 Phase B replaced `ToolPluginsSettings::commands_exposed_to`, whose rule
/// was "anything not OpenCode is Claude" — so a token nobody registered was
/// answered out of Claude's switch, fail-OPEN, on a question about whether a
/// model may run commands. An unregistered token is **not exposed**: the same
/// direction Phase A took for `audit::runner::consumer_exposed`, and the same
/// reason.
fn commands_exposed_to(settings: &crate::settings::Settings, consumer: &str) -> bool {
    crate::harness::HarnessId::from_consumer(consumer)
        .is_some_and(|h| settings.harness_settings(h).expose_commands)
}

/// The advertised `tool` enum for `run_command` — the runnable `command`-kind
/// registry entries, by their advertised names. Empty ⇒ the tool is not
/// advertised at all (see [`tools_for`]).
pub(super) fn command_tool_names(settings: &crate::settings::Settings) -> Vec<String> {
    crate::offload::tools::run_command::advertised_commands_live(settings)
        .into_iter()
        .map(|c| c.name)
        .collect()
}

/// Hash both halves of `run_command`'s advertisement — see
/// [`SurfaceFingerprint::commands_sig`]. Process-local memo/pulse key only, so
/// `DefaultHasher`'s unstable-across-releases hash is fine.
pub(super) fn commands_sig(settings: &crate::settings::Settings) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Every registered harness's switch, in registry order — a fixed pair here
    // meant a third harness's flip moved no fingerprint and re-advertised
    // nothing.
    for harness in crate::harness::registry::all() {
        settings.harness_settings(harness).expose_commands.hash(&mut h);
    }
    let names = command_tool_names(settings);
    names.len().hash(&mut h);
    for name in &names {
        name.hash(&mut h);
    }
    h.finish()
}

/// The `run_check` tool spec (V12 Phase A), advertised only when `checks` is
/// non-empty (see [`tools`]) — independent of the graph tool set.
///
/// The **schema is project-scoped**: `name`'s `enum` carries this project's
/// actual check names, and `name` is `required` whenever more than one is
/// configured. Prose alone did not carry that — a static `"required": []` plus
/// "omit it when only one is configured" left the caller no way to know that
/// *this* project configures three, and the live activity log showed
/// `run_check {changed_only: true}` failing on it repeatedly. A schema the
/// caller cannot satisfy by reading it is a defect in the schema, not the
/// caller. Consumers of the resulting spec text/enum must fold the check names
/// into their cache key — see [`SurfaceFingerprint`].
pub fn run_check_spec() -> GraphToolSpec {
    run_check_spec_for(&current_settings())
}

/// [`run_check_spec`] for one settings snapshot, resolving the effective names
/// itself. Kept because several callers (and every pre-V38 test) hold a
/// `Settings` and nothing else.
fn run_check_spec_for(settings: &crate::settings::Settings) -> GraphToolSpec {
    run_check_spec_from(&crate::checks::plugin::effective_check_names(settings))
}

/// The genuinely pure half: names in, schema out.
///
/// V38 split this off `run_check_spec_for` because the effective set now costs
/// a registry join, and [`tools`] needs both the names (to decide whether to
/// advertise at all) and the spec. Two reads could disagree — a plugin enabled
/// between them would advertise a `run_check` whose enum did not mention it.
pub(super) fn run_check_spec_from(names: &[String]) -> GraphToolSpec {
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut name_prop = serde_json::Map::new();
    name_prop.insert("type".into(), Value::String("string".into()));
    name_prop.insert(
        "description".into(),
        Value::String(if names.len() > 1 {
            format!(
                "REQUIRED — which configured check to run. This project configures {}: {}.",
                names.len(),
                names.join(", ")
            )
        } else {
            "Which configured check to run. Omit if only one is configured.".to_string()
        }),
    );
    if !names.is_empty() {
        name_prop.insert(
            "enum".into(),
            Value::Array(names.iter().map(|n| Value::String((*n).into())).collect()),
        );
    }
    GraphToolSpec {
        name: "run_check",
        description: "Run one of this project's configured checker commands (build / typecheck / \
            lint / test) and get back DEDUPLICATED, STRUCTURED diagnostics instead of a raw dump — \
            the cheap way to see what broke after an edit. `name` selects among the project's \
            configured checks — the `name` enum in this schema is the exact list, and `name` is \
            REQUIRED when the project configures more than one (calling without it just returns \
            the list, costing a round trip). The command itself is fixed by the user's project \
            config — never model-supplied. `changed_only: true` filters diagnostics to files \
            touched since HEAD (pairs well with editing loops).",
        parameters: json!({
            "type": "object",
            "properties": {
                "name": Value::Object(name_prop),
                "changed_only": { "type": "boolean", "description": "Filter diagnostics to files changed since HEAD. Default false." }
            },
            "required": if names.len() > 1 { json!(["name"]) } else { json!([]) }
        }),
    }
}

/// **V38 F-3 — the `run_command` tool spec for the harness surface.**
///
/// One tool, whose `tool` parameter is an enum of this project's runnable
/// `command`-kind registry entries. The shape mirrors [`run_check_spec_from`]
/// deliberately: a project-scoped enum the caller can read, `required` because
/// there is no sole-entry fallback, and a description that says what the caller
/// cannot otherwise know — that the binary is the user's, that there is no
/// shell, and that the working directory is not theirs to choose.
///
/// The pure half again (names in, schema out), for the same reason: [`tools_for`]
/// needs the names to decide whether to advertise at all, and computing them
/// twice would let a path configured between the two reads advertise a tool
/// whose enum does not mention it.
pub(super) fn run_command_spec_from(names: &[String]) -> GraphToolSpec {
    let enum_values: Vec<Value> = names.iter().map(|n| Value::String(n.clone())).collect();
    GraphToolSpec {
        name: "run_command",
        description: "Run one of this project's registered command tools (a plugin `command`-kind \
            entry the user enabled and pointed at a binary) and get back its exit code and \
            output. `tool` selects among them — the `tool` enum in this schema is the exact \
            list — and `args` is passed to the program as an ARGV vector: there is no shell, so \
            no redirection, pipes, globs or `&&`. Which binary runs is fixed by the user's \
            configuration and is never model-supplied, and the command always runs in the \
            project root.",
        parameters: json!({
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "description": format!(
                        "REQUIRED — which registered command tool to run. This project has {}: {}.",
                        names.len(),
                        names.join(", ")
                    ),
                    "enum": Value::Array(enum_values),
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments, one array element per argv entry (no shell)."
                }
            },
            "required": ["tool"]
        }),
    }
}

/// **Dispatch the two rootless tools**, or `None` when `name` is neither.
///
/// `run_check` (V12 Phase A) and `run_command` (V38 F-3) need a project ROOT and
/// no built code graph, so both entry points that serve them — the headless MCP
/// child (`mcp::tools::handle_call`) and the warm loopback route
/// (`GraphService::run_graph_tool`) — special-cased the same two names before
/// they opened an index. Three things differed between those copies, and every
/// one of them is a parameter here rather than a merged behaviour:
///
/// * **the root.** `resolve_root` is a closure, not a `&Path`, for both reasons
///   it has to be. The two callers resolve differently and deliberately: the
///   headless path takes [`super::headless_project_root`]'s marker walk because
///   there is no app to ask (#104), the warm path takes
///   [`super::warm_project_root`] because the route already resolved the calling
///   tab's project. And neither may pay an ancestor walk for a `graph_*` call
///   that never reaches this arm.
/// * **`source` and `tab`.** Attribution is the caller's fact, not this
///   function's: a headless child derives both from its own argv, the loopback
///   is handed the calling tab's.
/// * **`consumer`.** Only `run_command` reads it (its per-consumer exposure
///   switch), and it is threaded rather than derived so the warm path answers
///   out of the APP's live settings — which is what makes unchecking the box
///   take effect on a running tab instead of at its next restart.
///
/// Routing them in one place is also what the class-table scanner reads now:
/// `offload::toolclass`'s `DISPATCH_SITES` had a row for each entry point and
/// has one for this function, because there is one function.
pub(crate) async fn dispatch_rootless(
    name: &str,
    resolve_root: impl FnOnce() -> PathBuf,
    settings: &crate::settings::Settings,
    consumer: &str,
    source: &str,
    args: &Value,
    tab: &crate::activity::Attribution,
) -> Option<Result<String, String>> {
    if name == "run_check" {
        return Some(run_check_tool(&resolve_root(), settings, source, args, tab.clone()).await);
    }
    if name == "run_command" {
        return Some(
            run_command_tool(&resolve_root(), settings, consumer, source, args, tab.clone()).await,
        );
    }
    None
}

/// Dispatch `run_command` on the harness surface: run the named registry entry
/// at the project root and record the call, in the same shape and the same lane
/// as [`run_check_tool`].
///
/// Records the row itself for the same reason `run_check` does — it bypasses
/// [`dispatch_recorded`], which requires an open [`GraphIndex`] this tool has no
/// use for. `ActivityKind::Graph` and the `Graph` screen are the lane both share
/// (they are the same MCP surface); nothing about a command run is graph-shaped
/// except where it is served from.
async fn run_command_tool(
    root: &Path,
    settings: &crate::settings::Settings,
    consumer: &str,
    source: &str,
    args: &Value,
    tab: crate::activity::Attribution,
) -> Result<String, String> {
    let started = crate::activity::now_ms();
    let result = run_command_inner(root, settings, consumer, args).await;
    crate::activity::record_bg(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Graph,
            started,
            crate::activity::root_key(root),
            source.to_string(),
            "run_command".to_string(),
            args.get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            result.as_ref().map(|t| t.chars().count()).unwrap_or(0),
            crate::activity::now_ms().saturating_sub(started),
            result.is_ok(),
            tab,
            None,
            None,
            None,
        ),
        request: serde_json::to_string_pretty(args).unwrap_or_default(),
        response: activity_response(&result),
    });
    result
}

async fn run_command_inner(
    root: &Path,
    settings: &crate::settings::Settings,
    consumer: &str,
    args: &Value,
) -> Result<String, String> {
    // The exposure switch is re-checked at DISPATCH, not only at advertisement.
    // A tab holds the tool list it was given at connect (OpenCode caches it
    // outright), so a user who unchecks the box would otherwise keep serving
    // calls from every session already running — the `code_audit` exposure
    // flags' per-run re-check, for the same reason.
    if !commands_exposed_to(settings, consumer) {
        return Err(
            "run_command is not exposed to this consumer — re-enable it under Settings → Tool \
             Plugins if you meant to allow it"
                .to_string(),
        );
    }
    let tool = args.get("tool").and_then(|v| v.as_str()).unwrap_or("");
    // Model-supplied argv, taken as strings and nothing else: a non-string
    // element is refused rather than stringified, because `["--flag", 7]` is a
    // caller mistake and guessing at it is how an argument silently changes
    // meaning.
    let argv: Vec<String> = match args.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(
                            "run_command: every element of `args` must be a string".to_string()
                        )
                    }
                }
            }
            out
        }
        Some(_) => return Err("run_command: `args` must be an array of strings".to_string()),
    };
    crate::offload::tools::run_command::run_registered(root, settings, tool, &argv).await
}

/// Dispatch `run_check`: look up the named (or sole) configured [`CheckDef`],
/// run it, and format the result. Deliberately bypasses [`dispatch_recorded`]
/// (which requires an already-open [`GraphIndex`]) — `run_check` touches
/// neither the graph nor an index, so it can't be gated behind opening one
/// (V12 Phase A: the checks feature must not require the graph). Records the
/// call in the activity ring itself, in the same shape, so it still shows up
/// in the monitor tab.
async fn run_check_tool(
    root: &Path,
    settings: &crate::settings::Settings,
    source: &str,
    args: &Value,
    // #48 F-20: already classified by the entry point that knows the id's
    // provenance. See [`dispatch_recorded`].
    tab: crate::activity::Attribution,
) -> Result<String, String> {
    let started = crate::activity::now_ms();
    let result = run_check_inner(root, settings, args).await;
    crate::activity::record_bg(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Graph,
            started,
            crate::activity::root_key(root),
            source.to_string(),
            "run_check".to_string(),
            args.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            result.as_ref().map(|t| t.chars().count()).unwrap_or(0),
            crate::activity::now_ms().saturating_sub(started),
            result.is_ok(),
            tab,
            None,
            None,
            None,
        ),
        request: serde_json::to_string_pretty(args).unwrap_or_default(),
        response: activity_response(&result),
    });
    result
}

async fn run_check_inner(
    root: &Path,
    settings: &crate::settings::Settings,
    args: &Value,
) -> Result<String, String> {
    // V38 Phase D: the selectable set is `settings.checks` ∪ the enabled,
    // path-configured `check`-kind plugin tools — resolved HERE, from the same
    // `Settings` the advertised schema was built from, so the enum the caller
    // read and the list this function searches are the same list.
    let effective = crate::checks::plugin::effective_checks_live(settings, root);
    if effective.is_empty() {
        return Ok(
            "run_check is not configured for this project — add entries to the top-level `checks` \
             array in .cimp/config.json (each a { name, cmd, parser, timeout_secs }), or enable a \
             plugin that contributes checks in Settings → Tool Plugins."
                .to_string(),
        );
    }
    let requested = args
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let names = || {
        effective
            .iter()
            .map(|c| c.def.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let selected = if requested.is_empty() {
        match effective.as_slice() {
            [only] => only,
            // Informational, not a failure: the caller asked which checks exist
            // and gets the list. Returning `Err` here marked a well-formed
            // discovery call as a failed tool call in the activity feed (and in
            // the model's transcript) when nothing had actually gone wrong.
            // An UNKNOWN name below stays an error — that IS a caller mistake.
            _ => {
                return Ok(format!(
                    "run_check needs a `name` — this project has {} configured checks: {}. \
                     Re-call with one of those names.",
                    effective.len(),
                    names()
                ))
            }
        }
    } else {
        match effective.iter().find(|c| c.def.name == requested) {
            Some(c) => c,
            None => {
                return Err(format!(
                    "run_check: no configured check named `{requested}` — configured: {}",
                    names()
                ))
            }
        }
    };
    let def = &selected.def;
    // A plugin check that is advertised but could not be rendered (a hostile
    // overlay value, a manifest defect) fails HERE, with the reason. It stays in
    // the enum on purpose — see `PluginCheck::error`: a capability the user
    // enabled must not vanish without an explanation. The reason names the tool
    // the way SETTINGS does (label + registry key), because the advertised
    // `name` may have been disambiguated away from either.
    if let Some(p) = selected.plugin.as_ref() {
        if let Some(why) = p.error.as_deref() {
            return Err(crate::checks::auto::spawn_failure_line(
                &def.name,
                &format!("{} (plugin tool `{}`): {why}", p.label, p.tool_key),
            ));
        }
    }
    let changed_only = args
        .get("changed_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_rows = limits(settings).0;
    // V33: the OS sandbox for this seam, derived from the SAME settings
    // snapshot that selected the check. Every `run_check` caller reaches here
    // (MCP proxy, offload worker, IPC), so this is the one place the boundary
    // has to be resolved — decision 17's one switch, applied at the seam.
    let sandbox = crate::sandbox::SandboxCfg::from_settings(settings);
    // V38: a plugin check brings its manifest's posture; a configured one gets
    // `ToolPosture::default()`, which IS the pre-V38 behaviour.
    let posture = selected.posture(crate::sandbox::SEAM_RUN_CHECK, root, &sandbox);
    crate::checks::run_with_posture(root, def, changed_only, &sandbox, &posture)
        .await
        .map(|report| fmt_check_report(&report, max_rows))
        // V12 review: a check that fails to spawn/run must read as visibly
        // broken, not silently absent — same wording as the auto-check
        // aggregation path (`checks::auto::spawn_failure_line`).
        .map_err(|e| crate::checks::auto::spawn_failure_line(&def.name, &e.to_string()))
}

/// Render a [`crate::checks::CheckReport`] compactly: a header line (exit
/// code, duration, timeout flag) then one line per diagnostic group
/// (`severity · message (code folded in) · ×count · sample sites`), bounded
/// by `max_rows` like every other graph tool's result.
fn fmt_check_report(report: &crate::checks::CheckReport, max_rows: usize) -> String {
    let mut out = format!(
        "{} — exit {} · {} ms{}\n",
        report.name,
        report
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string()),
        report.duration_ms,
        // V21 F6: an explicit "unverified" cue on timeout, so the worker (and
        // Claude) treat an incomplete check as a non-result — composes with F2's
        // say-what-you-couldn't-verify rule rather than reading the partial
        // groups as the whole picture.
        if report.timed_out {
            " · TIMED OUT — the check did not finish; report this result as UNVERIFIED (only the partial output before the timeout was parsed)"
        } else {
            ""
        },
    );
    if report.groups.is_empty() {
        out.push_str("No diagnostics.");
        // V38 F-2: …and, when the run FAILED, the last lines of what it actually
        // printed. `exit 101 · 140 ms — No diagnostics.` was the whole answer a
        // live `run_check cargo-build` gave, while the line that explained it
        // ("could not find `Cargo.toml` in … or any parent directory") had been
        // parsed by a JSON parser, found not to be JSON, and dropped. The report
        // is only mute in this one branch, so this is the only place that needs
        // to speak.
        //
        // Labeled UNPARSED so the reader knows this is the tool's own text and
        // not cImp's structure: everything above this line came through a parser
        // and a dedup, and this did not.
        if let Some(tail) = report.raw_tail.as_deref() {
            out.push_str("\nraw output tail (unparsed):\n");
            out.push_str(tail);
        }
        return out;
    }
    let mut lines: Vec<String> = report
        .groups
        .iter()
        .take(max_rows)
        .map(|g| {
            let sites: Vec<String> = g.sites.iter().map(|(f, l)| format!("{f}:{l}")).collect();
            format!(
                "{} · {} · ×{} · {}",
                g.severity.as_str(),
                g.message,
                g.count,
                sites.join(", ")
            )
        })
        .collect();
    if report.groups.len() > max_rows {
        lines.push(format!(
            "… (+{} more groups)",
            report.groups.len() - max_rows
        ));
    }
    out.push_str(&lines.join("\n"));
    out
}

/// V21 F6: the worker-native `run_check`. Resolves the project root from the
/// offload confinement `roots` (the same posture as [`offload_query`]) and runs
/// the configured check through the **same** entry point the MCP surface uses
/// ([`run_check_tool`], source `"offload"`) — identical `CheckDef` resolution,
/// parser/dedup machinery, bounded report, and activity-ring recording. No new
/// execution surface: it only runs the project's user-vetted `checks` commands,
/// and returns the "not configured" guidance when the top-level `checks` array
/// is empty (the same gate that hides the tool from `enabled_defs`).
pub async fn offload_run_check(roots: &[PathBuf], args: &Value) -> Result<String, String> {
    let settings = current_settings();
    let sub = db_subdir(&settings);
    // A check needs a project root but not a built graph. Prefer the first root
    // that already has a graph.db (so a mixed setup agrees on "the project
    // root"), else fall back to the first configured root as-is.
    let root = roots
        .iter()
        .find_map(|r| find_graph_root(r, &sub))
        .or_else(|| roots.first().cloned())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    // The worker is not a tab — `Headless` is the positive claim, not missing
    // data. #48 F-20 deliberately did NOT change this; see `offload_query`.
    run_check_tool(
        &root,
        &settings,
        "offload",
        args,
        crate::activity::Attribution::Headless,
    )
    .await
}

#[cfg(test)]
mod run_check_tests {
    use super::super::surface;
    use super::{fmt_check_report, run_check_inner};
    use crate::checks::{CheckDef, CheckReport, DiagGroup, ParserKind, Severity};
    use crate::settings::Settings;
    use serde_json::json;

    fn def(name: &str, cmd: &str) -> CheckDef {
        CheckDef {
            name: name.to_string(),
            cmd: cmd.to_string(),
            parser: ParserKind::GenericGcc,
            timeout_secs: 30,
            ..Default::default()
        }
    }

    /// The schema must be self-sufficient: a caller that reads it and nothing
    /// else has to be able to produce a call this project accepts. With several
    /// checks configured that means `name` is `required` and its `enum` names
    /// them — the exact gap that made `run_check {changed_only: true}` the most
    /// frequent failed tool call in the live activity log.
    #[test]
    fn spec_requires_and_enumerates_name_when_several_checks_exist() {
        let settings = Settings {
            checks: vec![
                def("cargo-check", "cargo check"),
                def("cargo-test", "cargo test"),
                def("tsc", "tsc --noEmit"),
            ],
            ..Settings::default()
        };
        let spec = super::run_check_spec_for(&settings);
        assert_eq!(spec.name, "run_check");
        assert_eq!(spec.parameters["required"], json!(["name"]));
        assert_eq!(
            spec.parameters["properties"]["name"]["enum"],
            json!(["cargo-check", "cargo-test", "tsc"])
        );
    }

    /// A sole check keeps the historical ergonomics — `name` stays optional so
    /// the zero-arg call still works — but is still enumerated.
    #[test]
    fn spec_leaves_name_optional_for_a_sole_check() {
        let settings = Settings {
            checks: vec![def("only", "cargo check")],
            ..Settings::default()
        };
        let spec = super::run_check_spec_for(&settings);
        assert_eq!(spec.parameters["required"], json!([]));
        assert_eq!(
            spec.parameters["properties"]["name"]["enum"],
            json!(["only"])
        );
    }

    #[test]
    fn spec_omits_the_enum_when_no_checks_are_configured() {
        let spec = super::run_check_spec_for(&Settings::default());
        assert_eq!(spec.parameters["required"], json!([]));
        assert_eq!(spec.parameters["properties"]["name"]["enum"], json!(null));
    }

    // ── V38 F-3: the `run_command` surface ─────────────────────────────────

    /// **The listing gate, both directions of both halves.** The tool is hidden
    /// when the consumer's switch is off, and hidden when nothing is runnable —
    /// and the switches are per consumer, so one tab's choice never decides the
    /// other's.
    #[test]
    fn the_command_tool_is_listed_only_when_exposed_and_runnable() {
        let names = vec!["svn".to_string()];
        let s = Settings::default();
        assert!(super::commands_advertised(&s, "claude", &names));
        assert!(super::commands_advertised(&s, "opencode", &names));
        // Nothing runnable ⇒ hidden for everyone, whatever the switches say.
        assert!(!super::commands_advertised(&s, "claude", &[]));
        assert!(!super::commands_advertised(&s, "opencode", &[]));

        let mut off_for_claude = Settings::default();
        off_for_claude.harness_row("claude").expose_commands = false;
        assert!(!super::commands_advertised(&off_for_claude, "claude", &names));
        assert!(
            super::commands_advertised(&off_for_claude, "opencode", &names),
            "the two switches are independent"
        );

        let mut off_for_opencode = Settings::default();
        off_for_opencode.harness_row("opencode").expose_commands = false;
        assert!(!super::commands_advertised(&off_for_opencode, "opencode", &names));
        assert!(super::commands_advertised(&off_for_opencode, "claude", &names));

        // **V40 Phase B: an unrecognized consumer is NOT exposed.** It used
        // to read as Claude — so a token nobody registered was answered out of
        // Claude's switch, fail-OPEN, on a question about whether a model may
        // run commands. Both cases below are now `false` for the same reason,
        // which is the whole behaviour change: the answer no longer depends on
        // a harness the caller is not.
        assert!(!super::commands_advertised(&off_for_opencode, "", &names));
        assert!(!super::commands_advertised(&off_for_claude, "", &names));
        assert!(
            !super::commands_advertised(&Settings::default(), "not-a-harness", &names),
            "a typo'd consumer token must not inherit another harness's switch"
        );
    }

    /// The advertised schema enumerates exactly the runnable tools, and `tool`
    /// is REQUIRED — there is no sole-entry fallback on this surface.
    #[test]
    fn the_command_spec_enumerates_the_runnable_tools() {
        let spec = super::run_command_spec_from(&["svn".to_string(), "acme@1.0.0/hg".to_string()]);
        assert_eq!(spec.name, "run_command");
        assert_eq!(spec.parameters["required"], json!(["tool"]));
        assert_eq!(
            spec.parameters["properties"]["tool"]["enum"],
            json!(["svn", "acme@1.0.0/hg"])
        );
        // The description tells the caller the two things it cannot see: there
        // is no shell, and the working directory is not its to choose.
        assert!(spec.description.contains("no shell"), "{}", spec.description);
        assert!(
            spec.description.contains("project root"),
            "{}",
            spec.description
        );
        assert_eq!(
            spec.parameters["properties"]["args"]["items"]["type"],
            json!("string")
        );
    }

    /// **Every backend tier is its own fingerprint** (V39 review L-6).
    ///
    /// The tier used to be hashed as `is_fast` — lossless for exactly two
    /// variants, and silently collapsing the day a third is added. The symptom
    /// of a collapsed component is the one this type exists to prevent: the
    /// advertised `offload_task` prose moves (the tier is rendered into it) but
    /// the pulse gate sees no move, so every live session keeps the old list
    /// until its next restart.
    ///
    /// A tripwire as much as an assertion. The `match` below is exhaustive on
    /// purpose: a new variant stops this test COMPILING, which is the
    /// notification the bool never gave — and the distinctness assert is then
    /// what fails if the new variant hashes to an existing value.
    #[test]
    fn every_backend_tier_is_its_own_delegation_fingerprint() {
        use crate::settings::BackendTier;
        let tiers = [BackendTier::Fast, BackendTier::Quality];
        for tier in tiers {
            match tier {
                BackendTier::Fast | BackendTier::Quality => {}
            }
        }
        let sig_for = |tier: BackendTier| {
            let mut s = Settings::default();
            let mut tab = crate::settings::facade_tab("t1", "lan-worker-2");
            if let crate::settings::TabConfig::AiTool(c) = &mut tab {
                c.delegation_backend.tier = tier;
            }
            s.tabs.push(tab);
            surface::delegation_sig(&s)
        };
        let sigs: std::collections::HashSet<u64> = tiers.iter().copied().map(sig_for).collect();
        assert_eq!(
            sigs.len(),
            tiers.len(),
            "two tiers that advertise different prose must not hash alike"
        );
        // …and the fingerprint the pulse actually reads moves with it.
        assert_ne!(sig_for(BackendTier::Fast), sig_for(BackendTier::Quality));
    }

    /// The pulse gate must move when this surface moves: flipping either switch
    /// changes the advertised list for one consumer, and a fingerprint that
    /// stood still would leave every live session showing the old one.
    #[test]
    fn surface_fingerprint_tracks_the_command_exposure_switches() {
        let base = surface::SurfaceFingerprint::of(&Settings::default());
        let mut claude_off = Settings::default();
        claude_off.harness_row("claude").expose_commands = false;
        let mut opencode_off = Settings::default();
        opencode_off.harness_row("opencode").expose_commands = false;
        assert_ne!(base, surface::SurfaceFingerprint::of(&claude_off));
        assert_ne!(base, surface::SurfaceFingerprint::of(&opencode_off));
        assert_ne!(
            surface::SurfaceFingerprint::of(&claude_off),
            surface::SurfaceFingerprint::of(&opencode_off),
            "the two switches must not hash to the same value"
        );
        // …and the pulse's own value is derived from it, so it moves too.
        assert_ne!(
            surface::native_surface_sig(&Settings::default()),
            surface::native_surface_sig(&claude_off)
        );
    }

    /// The dispatch re-checks the exposure switch. A tab holds the tool list it
    /// was given at connect (OpenCode caches it outright), so unchecking the box
    /// has to stop the CALL, not only the next listing.
    #[tokio::test]
    async fn a_call_is_refused_when_the_consumer_is_not_exposed() {
        let mut settings = Settings::default();
        settings.harness_row("claude").expose_commands = false;
        let err = super::run_command_inner(
            &std::env::temp_dir(),
            &settings,
            "claude",
            &json!({ "tool": "svn" }),
        )
        .await
        .expect_err("an unexposed consumer must be refused at dispatch");
        assert!(err.contains("not exposed"), "{err}");
        assert!(err.contains("Tool Plugins"), "{err}");
    }

    /// `args` is an argv vector, and a non-string element is a caller mistake
    /// rather than something to stringify: guessing is how an argument silently
    /// changes meaning.
    #[tokio::test]
    async fn a_non_string_argv_element_is_refused() {
        let settings = Settings::default();
        let err = super::run_command_inner(
            &std::env::temp_dir(),
            &settings,
            "claude",
            &json!({ "tool": "svn", "args": ["--flag", 7] }),
        )
        .await
        .expect_err("a number is not an argv element");
        assert!(err.contains("must be a string"), "{err}");

        let err = super::run_command_inner(
            &std::env::temp_dir(),
            &settings,
            "claude",
            &json!({ "tool": "svn", "args": "--flag" }),
        )
        .await
        .expect_err("a bare string is not an argv vector");
        assert!(err.contains("array of strings"), "{err}");
    }

    /// The advertised bytes now depend on the check NAMES, so the memo key must
    /// too — renaming a check with the count unchanged has to invalidate the
    /// cache, which the old `has_checks: bool` could not see.
    #[test]
    fn surface_fingerprint_tracks_check_names_not_just_emptiness() {
        let with = |names: &[&str]| Settings {
            checks: names.iter().map(|n| def(n, "cargo check")).collect(),
            ..Settings::default()
        };
        let a = surface::SurfaceFingerprint::of(&with(&["cargo"]));
        let renamed = surface::SurfaceFingerprint::of(&with(&["tsc"]));
        let added = surface::SurfaceFingerprint::of(&with(&["cargo", "tsc"]));
        let none = surface::SurfaceFingerprint::of(&Settings::default());
        assert_ne!(a, renamed, "a rename must invalidate the surface cache");
        assert_ne!(a, added);
        assert_ne!(a, none);
        assert_eq!(a, surface::SurfaceFingerprint::of(&with(&["cargo"])));
    }

    /// A plugin-contributed name is advertised verbatim, `@` and `/` and all —
    /// the disambiguated key `checks::plugin` mints when a short id is taken.
    /// The schema is a project-scoped surface (the run_check note in the plan),
    /// so a name this build has never seen is expected, not exceptional.
    #[test]
    fn the_spec_advertises_the_names_it_is_given_including_plugin_keys() {
        let spec = super::run_check_spec_from(&[
            "cargo".to_string(),
            "acme@1.0.0/lint".to_string(),
        ]);
        assert_eq!(spec.parameters["required"], json!(["name"]));
        assert_eq!(
            spec.parameters["properties"]["name"]["enum"],
            json!(["cargo", "acme@1.0.0/lint"])
        );
    }

    /// **Invariant 10, as a test rather than a hope.** Three places decide what
    /// `run_check` looks like — the advertisement gate in `tools`, the schema in
    /// `run_check_spec_for`, and the memo key in `checks_sig` — and all three
    /// must read the EFFECTIVE set. A plugin check is not injectable into a unit
    /// test (it needs a scanned `plugins/` directory), so the wiring is pinned
    /// where it can be: at the source. The behaviour those three share is
    /// covered by `checks::plugin`'s own tests.
    ///
    /// Newline-agnostic on purpose — CI checks this tree out with CRLF.
    #[test]
    fn every_run_check_surface_reads_the_effective_set() {
        // V42 R8 split `mcp.rs` in two: `tools_for` is in `tools.rs`, the
        // other four signatures are in this file. Both are scanned, so the
        // invariant reads the same set of bodies it always did; a needle that
        // stopped being found panics with "re-point this test" below.
        let src = concat!(include_str!("tools.rs"), include_str!("checks_tools.rs"));
        let body_of = |sig: &str| -> String {
            let start = src
                .find(sig)
                .unwrap_or_else(|| panic!("`{sig}` is gone — re-point this test"));
            let rest = &src[start..];
            let end = rest.find("\n}").unwrap_or(rest.len());
            rest[..end].to_string()
        };
        // `tools_for` and not `tools`: V38 F-3 made the builder consumer-aware,
        // and `tools()` is now a one-line delegation to it. The invariant did not
        // move — the body that decides what is advertised did.
        for sig in ["fn checks_sig(", "pub fn tools_for("] {
            let body = body_of(sig);
            assert!(
                body.contains("effective_check_names"),
                "`{sig}` must read the effective check set (settings.checks ∪ plugin checks), \
                 or a plugin enable/rescan changes the advertised surface without moving the \
                 fingerprint that gates it"
            );
            // V38 F-3, the same invariant for the second project-dynamic tool:
            // the gate in `tools_for` and the memo/pulse key in `commands_sig`
            // must read the runnable `command`-kind set through ONE function, or
            // configuring a command tool's path moves the advertised bytes
            // without moving the fingerprint that would tell a live session.
            let sig = if sig == "fn checks_sig(" {
                "fn commands_sig("
            } else {
                sig
            };
            assert!(
                body_of(sig).contains("command_tool_names"),
                "`{sig}` must read the runnable command-tool set"
            );
        }
        assert!(
            body_of("fn run_check_spec_for(").contains("effective_check_names"),
            "the advertised schema must enumerate the effective set"
        );
        assert!(
            body_of("async fn run_check_inner(").contains("effective_checks_live"),
            "dispatch must select from the same set the schema advertised"
        );
    }

    #[tokio::test]
    async fn empty_config_reports_not_configured() {
        let settings = Settings::default();
        assert!(settings.checks.is_empty());
        let out = run_check_inner(&std::env::temp_dir(), &settings, &json!({}))
            .await
            .expect("ok result");
        assert!(out.contains("not configured"), "{out}");
        assert!(out.contains("checks"), "{out}");
    }

    #[tokio::test]
    async fn unknown_name_lists_configured_checks() {
        let settings = Settings {
            checks: vec![def("cargo", "cargo check")],
            ..Settings::default()
        };
        let err = run_check_inner(&std::env::temp_dir(), &settings, &json!({ "name": "nope" }))
            .await
            .expect_err("unknown name should error");
        assert!(err.contains("no configured check named `nope`"), "{err}");
        assert!(err.contains("cargo"), "{err}");
    }

    /// Omitting `name` is a DISCOVERY call, not a failure: it answers with the
    /// list. Returning `Err` here logged a well-formed call as a failed tool
    /// call in the activity feed and the model's transcript. (An unknown name
    /// stays an error — see `unknown_name_lists_configured_checks`.)
    #[tokio::test]
    async fn ambiguous_without_name_lists_configured_checks() {
        let settings = Settings {
            checks: vec![def("cargo", "cargo check"), def("tsc", "tsc --noEmit")],
            ..Settings::default()
        };
        let out = run_check_inner(&std::env::temp_dir(), &settings, &json!({}))
            .await
            .expect("omitted name should inform, not fail");
        assert!(out.contains("needs a `name`"), "{out}");
        assert!(out.contains("cargo") && out.contains("tsc"), "{out}");
    }

    #[tokio::test]
    async fn sole_configured_check_runs_without_a_name() {
        let cargo = which::which("cargo").expect("cargo on PATH");
        let settings = Settings {
            checks: vec![def("only", &format!("\"{}\" --version", cargo.display()))],
            ..Settings::default()
        };
        let out = run_check_inner(&std::env::temp_dir(), &settings, &json!({}))
            .await
            .expect("ok result");
        assert!(out.contains("only"), "{out}");
        assert!(out.contains("exit 0"), "{out}");
    }

    #[test]
    fn fmt_check_report_renders_header_groups_and_overflow() {
        let report = CheckReport {
            name: "cargo".to_string(),
            exit_code: Some(1),
            duration_ms: 42,
            timed_out: false,
            groups: vec![
                DiagGroup {
                    key: "k1".into(),
                    severity: Severity::Error,
                    message: "E0425: cannot find value ‹…› in this scope".into(),
                    count: 3,
                    sites: vec![("src/a.rs".into(), 10), ("src/b.rs".into(), 20)],
                },
                DiagGroup {
                    key: "k2".into(),
                    severity: Severity::Warning,
                    message: "unused import".into(),
                    count: 1,
                    sites: vec![("src/c.rs".into(), 1)],
                },
            ],
            stdout_bytes: 0,
            stderr_bytes: 0,
            raw_tail: None,
        };
        let out = fmt_check_report(&report, 1);
        assert!(out.starts_with("cargo — exit 1 · 42 ms"), "{out}");
        assert!(out.contains("error · E0425"), "{out}");
        assert!(out.contains("src/a.rs:10, src/b.rs:20"), "{out}");
        // Capped at max_rows=1: only the first group's line, plus an overflow note.
        assert!(!out.contains("unused import"), "{out}");
        assert!(out.contains("+1 more group"), "{out}");
    }

    #[test]
    fn fmt_check_report_no_diagnostics() {
        let report = CheckReport {
            name: "cargo".into(),
            exit_code: Some(0),
            duration_ms: 5,
            timed_out: false,
            groups: vec![],
            stdout_bytes: 0,
            stderr_bytes: 0,
            raw_tail: None,
        };
        let out = fmt_check_report(&report, 50);
        assert!(out.contains("No diagnostics."), "{out}");
    }

    /// V38 F-2 — the mute branch is the one that speaks. A failed check with no
    /// groups renders its raw tail under a label that says the text is the
    /// tool's own and not cImp's structure; a report without a tail is
    /// byte-identical to what it always was.
    #[test]
    fn fmt_check_report_shows_the_raw_tail_only_when_there_is_one() {
        let base = CheckReport {
            name: "cargo-build".into(),
            exit_code: Some(101),
            duration_ms: 140,
            timed_out: false,
            groups: vec![],
            stdout_bytes: 0,
            stderr_bytes: 96,
            raw_tail: Some(
                "error: could not find `Cargo.toml` in `C:\\proj` or any parent directory".into(),
            ),
        };
        let out = fmt_check_report(&base, 50);
        assert!(out.contains("No diagnostics."), "{out}");
        assert!(out.contains("raw output tail (unparsed):"), "{out}");
        assert!(out.contains("could not find `Cargo.toml`"), "{out}");

        let silent = CheckReport {
            raw_tail: None,
            ..base.clone()
        };
        assert!(!fmt_check_report(&silent, 50).contains("raw output tail"));

        // A report WITH diagnostics never grows the section: the tail exists to
        // fill a gap, and there is no gap here. (`run` would not populate it in
        // this shape either — this pins the renderer's half of the rule.)
        let with_groups = CheckReport {
            groups: vec![DiagGroup {
                key: "k".into(),
                severity: Severity::Error,
                message: "boom".into(),
                count: 1,
                sites: vec![("src/a.rs".into(), 1)],
            }],
            ..base
        };
        assert!(!fmt_check_report(&with_groups, 50).contains("raw output tail"));
    }

    #[test]
    fn fmt_check_report_flags_timeout() {
        let report = CheckReport {
            name: "slow".into(),
            exit_code: None,
            duration_ms: 10_000,
            timed_out: true,
            groups: vec![],
            stdout_bytes: 0,
            stderr_bytes: 0,
            raw_tail: None,
        };
        let out = fmt_check_report(&report, 50);
        assert!(out.contains("TIMED OUT"), "{out}");
        // V21 F6: a timed-out check must carry the "unverified" cue so the
        // worker reports it as a non-result (composes with F2).
        assert!(out.to_uppercase().contains("UNVERIFIED"), "{out}");
    }
}
