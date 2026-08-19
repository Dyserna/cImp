//! V38 — the **starter plugin pack**, pinned against the real loader.
//!
//! The seven files in the repo's `plugins/` folder are the framework's first
//! outside user: no built-in-only fields, no `cimp-` prefix, nothing embedded.
//! They ship next to the executable exactly the way `themes/` and `palettes/`
//! do (`build.rs::copy_theming_assets` for a dev build,
//! `.github/workflows/release.yml` for the portable zips), which means the
//! copy a user gets is *these bytes* — so a validator tightening that would
//! reject one of them must fail here, at `cargo test`, and not on a stranger's
//! machine at startup.
//!
//! What this module asserts is deliberately the whole chain rather than "the
//! JSON parses":
//!
//! 1. the REAL [`loader::scan_dir`] over the REAL directory, as
//!    [`Provenance::User`] — the trust level a dropped-in file gets, so every
//!    user-only rule (the reserved prefix, the built-in-only fields, the SARIF
//!    requirement) is in force;
//! 2. every `check`-kind tool rendered through the REAL
//!    [`crate::checks::plugin::effective_checks`] and then through
//!    [`crate::checks::CheckDef::validate`] — a manifest that loads but cannot render is a
//!    plugin that is advertised broken, which is worse than one that never
//!    loaded;
//! 3. every declared `runtime` naming a row in `sandbox::RUNTIME_PROFILES`, so
//!    the pack cannot ask for a profile cImp does not own;
//! 4. the pack's own scope rules — no `audit`/`security` kinds (cImp's own
//!    fourteen scanners cover that pipeline and a user plugin's findings would
//!    have to be SARIF), and no `parameters_allowed` on a `command`-kind tool
//!    (nothing reads it there; see the report gap list).

use std::path::{Path, PathBuf};

use crate::plugins::loader::{self, PluginSet};
use crate::plugins::manifest::{ManifestParser, Provenance, RuntimeReq, ToolKind};
use crate::settings::Settings;

/// The shipped pack, relative to this crate. `CARGO_MANIFEST_DIR` is
/// `src-tauri/`, and the folder lives at the repo root beside `themes/` — the
/// same relationship `build.rs` walks when it copies it next to the exe.
fn pack_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri has a parent")
        .join("plugins")
}

/// Scan it the way cImp scans a user's folder.
fn pack() -> PluginSet {
    let dir = pack_dir();
    assert!(
        dir.is_dir(),
        "the starter pack directory is missing: {}",
        dir.display()
    );
    loader::scan_dir(&dir, Provenance::User)
}

/// The seven plugins, by identity. Spelled out rather than counted so that a
/// renamed or re-versioned file fails as *which one moved* — these strings are
/// also the settings keys' prefix, and a version bump orphans stored state.
const EXPECTED: [&str; 7] = [
    "dotnet-toolchain@1.0.0",
    "go-toolchain@1.0.0",
    "java-toolchain@1.0.0",
    "js-ts-toolchain@1.0.0",
    "python-toolchain@1.0.0",
    "rust-toolchain@1.0.0",
    "source-control@1.0.0",
];

#[test]
fn the_shipped_pack_loads_clean_as_a_user_plugin() {
    let set = pack();
    assert!(
        set.errors.is_empty(),
        "the shipped pack must load with no rejections; got: {:#?}",
        set.errors
    );
    let keys: Vec<&str> = set.plugins.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(
        keys, EXPECTED,
        "the shipped pack's identities changed — these strings are the prefix of every settings \
         key the pack owns, so a rename or a version bump orphans stored paths and variables"
    );
}

#[test]
fn the_pack_declares_no_findings_tools_and_no_builtin_only_shapes() {
    let set = pack();
    for p in &set.plugins {
        assert_eq!(
            p.provenance,
            Provenance::User,
            "the pack is scanned, never embedded"
        );
        for t in &p.manifest.tools {
            assert!(
                matches!(t.kind, ToolKind::Check | ToolKind::Command),
                "{}/{}: the pack is deliberately free of `audit`/`security` tools — cImp's own \
                 fourteen scanners are that pipeline's population, and a user plugin's findings \
                 would have to arrive as SARIF from a wrapper the pack does not ship",
                p.key,
                t.id
            );
            // Refused at load for a user plugin anyway; asserted so the pack
            // never becomes the reason someone relaxes that rule.
            assert!(t.ingest.is_none() && t.command.is_none());
            assert!(t.project_local_bin.is_none() && t.dir_argv.is_empty());
            if t.kind == ToolKind::Command {
                assert!(
                    !t.parameters_allowed,
                    "{}/{}: `run_command` takes its arguments from the caller and never appends \
                     stored parameters, so offering the field on this kind would render a \
                     settings input that goes nowhere",
                    p.key,
                    t.id
                );
            }
        }
    }
}

#[test]
fn every_declared_runtime_names_a_profile_cimp_owns() {
    let set = pack();
    for p in &set.plugins {
        for t in &p.manifest.tools {
            let id = match t.runtime {
                // `auto` IS the inference and `none` is the positive "single
                // static binary" statement; neither selects a profile row.
                RuntimeReq::Auto | RuntimeReq::None => continue,
                other => other.as_str(),
            };
            assert!(
                crate::sandbox::RUNTIME_PROFILES.iter().any(|r| r.id == id),
                "{}/{} declares runtime `{id}`, which is not a row in `RUNTIME_PROFILES` — the \
                 manifest may only request grants from a table cImp owns",
                p.key,
                t.id
            );
        }
    }
}

#[test]
fn every_check_kind_tool_renders_a_valid_checkdef() {
    let set = pack();

    // Every tool pointed at a binary, machine-scope — the state in which a
    // check is runnable. A fake path is enough: rendering never touches disk.
    let mut settings = Settings::default();
    let program = if cfg!(windows) {
        "C:\\tools\\pack-probe.exe"
    } else {
        "/usr/bin/pack-probe"
    };
    let mut check_tools = 0usize;
    for p in &set.plugins {
        for t in &p.manifest.tools {
            settings
                .tool_plugins
                .global_paths
                .insert(p.tool_key(&t.id), program.to_string());
            if t.kind == ToolKind::Check {
                check_tools += 1;
                // Every check-kind parser word must resolve in the DIAGNOSTICS
                // namespace. A findings-only name here is refused at load, but
                // asserting it keeps the pack honest about which namespace it
                // is writing in.
                if let Some(parser) = t.parser {
                    assert!(
                        matches!(parser, ManifestParser::Kind(_)),
                        "{}/{}: a check's `parser` selects a diagnostics decoder",
                        p.key,
                        t.id
                    );
                }
            }
        }
    }
    assert!(check_tools >= 12, "the pack's check population shrank unexpectedly ({check_tools})");

    let root = pack_dir();
    // A census that reports EVERY marker the pack gates on, so this test judges
    // rendering rather than applicability (which has its own test below). The
    // pack directory itself has no `pom.xml`; without this the Maven, Gradle,
    // Cargo, Go, npm and .NET checks would all be gated out and the test would
    // pass by measuring nothing.
    let all_markers = crate::audit::census::Census::from_block(
        &[],
        &crate::audit::census::MARKERS
            .iter()
            .map(|m| (*m).to_string())
            .collect::<Vec<_>>(),
    );
    let effective =
        crate::checks::plugin::effective_checks(&settings, &set, None, &root, &all_markers);
    let rendered: Vec<_> = effective.iter().filter(|c| c.plugin.is_some()).collect();
    assert_eq!(
        rendered.len(),
        check_tools,
        "every check-kind tool with a path must reach the effective set"
    );

    for c in rendered {
        let pc = c.plugin.as_ref().expect("filtered");
        assert!(
            pc.error.is_none(),
            "{} is advertised BROKEN: {}",
            pc.tool_key,
            pc.error.clone().unwrap_or_default()
        );
        assert!(
            c.def.cmd.contains(program),
            "{}: the first token must be a program PLACEHOLDER the configured binary replaces \
             (rendered: `{}`)",
            pc.tool_key,
            c.def.cmd
        );
        c.def.validate().unwrap_or_else(|e| {
            panic!("{}: rendered CheckDef fails validation: {e}", pc.tool_key)
        });
        // A timeout the author chose, above the pipeline's floor. `None` would
        // silently mean 120s, which is wrong for every build and test tool in
        // this pack.
        let secs = c.def.timeout_secs;
        assert!(
            (crate::checks::MIN_TIMEOUT_SECS..=86_400).contains(&secs),
            "{}: timeout {secs}s is outside the sane band",
            pc.tool_key
        );
        // `regex-custom` is only as good as its pattern, and an uncompilable
        // one yields ZERO diagnostics — which reads exactly like a clean run.
        if c.def.parser == crate::checks::ParserKind::RegexCustom {
            let pat = c.def.pattern.as_deref().unwrap_or_default();
            let re = regex::Regex::new(pat).unwrap_or_else(|e| {
                panic!("{}: `pattern` does not compile: {e}", pc.tool_key)
            });
            for group in ["file", "line", "message"] {
                assert!(
                    re.capture_names().flatten().any(|n| n == group),
                    "{}: `regex-custom` needs a `{group}` capture group; without it the parser \
                     drops every line and the check reports a clean run",
                    pc.tool_key
                );
            }
        }
    }
}

/// V38 Phase F — the pack's project-shape gates actually bite.
///
/// The contract doc's worked example is "`pom.xml` → maven, `build.gradle` →
/// gradle", and until this phase `applicability` validated on a `check`-kind
/// tool and nothing read it. This asserts the promise end to end against the
/// SHIPPED manifests: a Maven-shaped project advertises the Maven checks and
/// not the Gradle ones, and the reverse.
#[test]
fn a_projects_shape_decides_which_of_the_packs_checks_exist() {
    let set = pack();
    let mut settings = Settings::default();
    let program = if cfg!(windows) {
        "C:\\tools\\pack-probe.exe"
    } else {
        "/usr/bin/pack-probe"
    };
    for p in &set.plugins {
        for t in &p.manifest.tools {
            settings
                .tool_plugins
                .global_paths
                .insert(p.tool_key(&t.id), program.to_string());
        }
    }
    let root = pack_dir();
    let names = |markers: &[&str]| -> Vec<String> {
        let census = crate::audit::census::Census::from_block(
            &[],
            &markers.iter().map(|m| (*m).to_string()).collect::<Vec<_>>(),
        );
        crate::checks::plugin::effective_checks(&settings, &set, None, &root, &census)
            .into_iter()
            .filter(|c| c.plugin.is_some())
            .map(|c| c.def.name)
            .collect()
    };

    let maven = names(&["pom.xml"]);
    assert!(maven.iter().any(|n| n == "maven-build"));
    assert!(maven.iter().any(|n| n == "maven-test"));
    assert!(
        !maven.iter().any(|n| n.starts_with("gradle")),
        "a Maven project must not advertise the Gradle checks: {maven:?}"
    );

    let gradle = names(&["build.gradle"]);
    assert!(gradle.iter().any(|n| n == "gradle-build"));
    assert!(
        !gradle.iter().any(|n| n.starts_with("maven")),
        "a Gradle project must not advertise the Maven checks: {gradle:?}"
    );

    // An ungated check (pytest declares no `applicability` — Python has no
    // census token) is present in both, which is what "no gate = always
    // applicable" has to mean for the rule to be safe to adopt.
    for shape in [&maven, &gradle] {
        assert!(
            shape.iter().any(|n| n == "pytest"),
            "an ungated check must survive every project shape: {shape:?}"
        );
    }
}
