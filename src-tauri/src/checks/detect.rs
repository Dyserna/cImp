//! V22 Phase D — language auto-detection + auto-configure.
//!
//! Zero-effort `run_check` setup: look at what the project is written in and
//! propose a working [`CheckDef`] set the user can approve with one click (or,
//! opt-in via `checks_auto_configure`, apply automatically on first index).
//!
//! Two evidence sources, merged:
//!   1. **Code-graph language stats** ([`crate::graph`]'s per-language indexed
//!      file counts, threaded in as `&[LangStat]`) — corroborates an ecosystem
//!      and annotates the proposal; can also surface a language the marker scan
//!      couldn't wire (a deeply nested crate, no manifest at the top) as a
//!      *greyed* proposal the user configures by hand.
//!   2. **Marker files** ([`scan_markers`]) — the graph-less fallback and the
//!      thing that actually makes a concrete command runnable (a nested
//!      `Cargo.toml` gives us the `cwd` a workspace-less `cargo check` needs).
//!
//! Every candidate is validated before it's offered as applicable: the tool
//! binary must resolve (`ebin`/PATH, via [`crate::pty::resolve::resolve_command`])
//! and its marker evidence must be present. A candidate that fails either is
//! still returned — with `valid = false` and a `reason` — so the Phase E editor
//! can grey it out instead of silently dropping (or silently applying) it.
//!
//! Detection is pure filesystem + PATH work: fast, bounded (a depth-2 walk that
//! skips `node_modules`/`target`/… ), and never touches the network. The preset
//! catalog is data-driven ([`presets_for`]) so a new ecosystem is a table entry,
//! not a new branch scattered through `detect`.

use std::path::Path;

use serde::Serialize;

use super::{CheckDef, ParserKind};

/// One indexed-language count from the code graph, decoupled from
/// `graph::index::LangCount` so `checks` doesn't take a dependency on the graph
/// crate. The IPC layer converts `LangCount → LangStat`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LangStat {
    /// Stored lowercase language tag (`"rust"`, `"typescript"`, …).
    pub lang: String,
    pub files: u64,
}

/// One proposed check, plus why it was proposed and whether it can actually run.
/// Serialized straight to the Phase E editor (`checks_detect` IPC).
#[derive(Clone, Debug, Serialize)]
pub struct Proposal {
    /// The candidate check (its `cmd`/`parser`/`cwd`/… ready to apply as-is).
    pub check: CheckDef,
    /// Human label for the ecosystem this came from (`"Rust"`, `"Go"`, … ).
    pub ecosystem: String,
    /// What triggered it — the marker file(s) and/or the graph stat.
    pub evidence: String,
    /// `true` iff the machine could validate it (marker present + binary on
    /// PATH). Invalid ones are shown greyed, never auto-applied.
    pub valid: bool,
    /// Why an invalid proposal can't run (`"go not found on PATH"`, … ). `None`
    /// when `valid`.
    pub reason: Option<String>,
}

/// The ecosystems auto-detection knows how to wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eco {
    Rust,
    TsJs,
    Go,
    Python,
    Dotnet,
    Jvm,
}

impl Eco {
    /// Human label for the [`Proposal::ecosystem`] field.
    fn label(self) -> &'static str {
        match self {
            Eco::Rust => "Rust",
            Eco::TsJs => "TypeScript/JavaScript",
            Eco::Go => "Go",
            Eco::Python => "Python",
            Eco::Dotnet => ".NET",
            Eco::Jvm => "Java/JVM",
        }
    }

    /// The manifest this ecosystem's commands need — named in the greyed
    /// "graph saw the language but no marker" reason.
    fn manifest(self) -> &'static str {
        match self {
            Eco::Rust => "Cargo.toml",
            Eco::TsJs => "package.json/tsconfig.json",
            Eco::Go => "go.mod",
            Eco::Python => "pyproject.toml",
            Eco::Dotnet => "a .csproj/.sln",
            Eco::Jvm => "pom.xml/build.gradle",
        }
    }

    /// Which graph language tag(s) count toward this ecosystem's file total.
    fn lang_tags(self) -> &'static [&'static str] {
        match self {
            Eco::Rust => &["rust"],
            Eco::TsJs => &["typescript", "javascript"],
            Eco::Go => &["go"],
            Eco::Python => &["python"],
            Eco::Dotnet => &["csharp"],
            Eco::Jvm => &["java", "kotlin", "scala"],
        }
    }
}

/// How deep the marker walk recurses: root (depth 0) plus up to two nested
/// levels — enough to catch this repo's `src-tauri/Cargo.toml` and typical
/// monorepo `packages/*/` layouts, without an unbounded tree walk.
const MAX_DEPTH: usize = 2;

/// Cap on bytes read from a manifest we peek inside (`pyproject.toml` for
/// `[tool.ruff]`, `package.json` for a `jest`/`vitest` mention) — these files
/// are tiny; the cap just bounds a pathological input.
const MANIFEST_PEEK_BYTES: u64 = 256 * 1024;

/// Marker-file scan result. Anchors are the ROOT-RELATIVE directory (forward
/// slashes, `""` for the project root) of the shallowest manifest found for an
/// ecosystem; `None` means the ecosystem's marker was not seen at all. The
/// booleans gate the optional sub-checks (a `tsc` proposal needs a `tsconfig`,
/// an `eslint` proposal needs an eslint config, … ).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Markers {
    rust_anchor: Option<String>,
    js_anchor: Option<String>,
    tsconfig: bool,
    eslint: bool,
    jest: bool,
    vitest: bool,
    go_anchor: Option<String>,
    py_anchor: Option<String>,
    ruff: bool,
    pytest: bool,
    dotnet_anchor: Option<String>,
    maven_anchor: Option<String>,
    gradle_anchor: Option<String>,
}

/// Set `anchor` to `rel` only if unset — since [`scan_dir`] visits a parent
/// before its children, this keeps the *shallowest* manifest directory.
fn set_anchor(anchor: &mut Option<String>, rel: &str) {
    if anchor.is_none() {
        *anchor = Some(rel.to_string());
    }
}

/// An ecosystem anchor as a [`CheckDef::cwd`]: `None` for the project root,
/// else the relative subdir the manifest lives in (nested manifests / monorepos).
fn anchor_cwd(anchor: &Option<String>) -> Option<String> {
    match anchor {
        Some(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Walk `root` (bounded to [`MAX_DEPTH`], skipping the shared
/// [`crate::fsutil::SKIP_DIRS`] build/vendor set and hidden dirs) recording
/// every ecosystem marker. Cheap and dependency-free — a depth-first `read_dir`
/// recursion rather than pulling in the graph's gitignore-aware walker, since
/// markers are never gitignored and the fixed skip set is deterministic for the
/// tempdir test fixtures.
fn scan_markers(root: &Path) -> Markers {
    let mut m = Markers::default();
    scan_dir(root, root, 0, &mut m);
    m
}

fn scan_dir(root: &Path, dir: &Path, depth: usize, m: &mut Markers) {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            // A `tests/` directory is a pytest signal on its own.
            if name == "tests" {
                m.pytest = true;
            }
            if !name.starts_with('.') && !crate::fsutil::SKIP_DIRS.contains(&name.as_str()) {
                subdirs.push(entry.path());
            }
            continue;
        }
        classify_file(&rel_str, &name, &entry.path(), m);
    }
    if depth < MAX_DEPTH {
        // Deterministic order so the shallowest-of-equal-depth anchor is stable.
        subdirs.sort();
        for sd in subdirs {
            scan_dir(root, &sd, depth + 1, m);
        }
    }
}

/// Peek at a small manifest's text (bounded read, lossy UTF-8) so we can test
/// for embedded config (`[tool.ruff]`, a `"vitest"` dep). `None` on any error.
fn peek(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MANIFEST_PEEK_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Classify one file into the [`Markers`] set. `rel_dir` is the file's parent,
/// root-relative (`""` at the root); `name` is the file name; `path` the full
/// path (for the peek-inside cases).
fn classify_file(rel_dir: &str, name: &str, path: &Path, m: &mut Markers) {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cargo.toml" => set_anchor(&mut m.rust_anchor, rel_dir),
        "package.json" => {
            set_anchor(&mut m.js_anchor, rel_dir);
            // A jest/vitest config often lives only as a package.json key.
            if let Some(txt) = peek(path) {
                if txt.contains("\"vitest\"") {
                    m.vitest = true;
                }
                if txt.contains("\"jest\"") {
                    m.jest = true;
                }
            }
        }
        "go.mod" => set_anchor(&mut m.go_anchor, rel_dir),
        "pyproject.toml" => {
            set_anchor(&mut m.py_anchor, rel_dir);
            if let Some(txt) = peek(path) {
                if txt.contains("[tool.ruff") {
                    m.ruff = true;
                }
                if txt.contains("[tool.pytest") {
                    m.pytest = true;
                }
            }
        }
        "setup.cfg" => {
            set_anchor(&mut m.py_anchor, rel_dir);
            if let Some(txt) = peek(path) {
                if txt.contains("[tool:pytest]") {
                    m.pytest = true;
                }
            }
        }
        "ruff.toml" | ".ruff.toml" => m.ruff = true,
        "pytest.ini" | "conftest.py" | "tox.ini" => m.pytest = true,
        "pom.xml" => set_anchor(&mut m.maven_anchor, rel_dir),
        "build.gradle" | "build.gradle.kts" => set_anchor(&mut m.gradle_anchor, rel_dir),
        _ => {
            // Extension / prefix families.
            if lower.starts_with("tsconfig") && lower.ends_with(".json") {
                m.tsconfig = true;
                set_anchor(&mut m.js_anchor, rel_dir);
            } else if is_eslint_config(&lower) {
                m.eslint = true;
            } else if lower.starts_with("jest.config.") {
                m.jest = true;
            } else if lower.starts_with("vitest.config.") {
                m.vitest = true;
            } else if lower.ends_with(".sln") || lower.ends_with(".csproj") {
                set_anchor(&mut m.dotnet_anchor, rel_dir);
            }
        }
    }
}

/// Whether `name` (already lowercased) is an ESLint config file — the flat
/// (`eslint.config.js|mjs|cjs|ts`) or legacy (`.eslintrc`, `.eslintrc.json`, … )
/// forms.
fn is_eslint_config(name: &str) -> bool {
    name.starts_with("eslint.config.") || name == ".eslintrc" || name.starts_with(".eslintrc.")
}

/// Run detection with the real PATH resolver. See [`detect_with`].
pub fn detect(root: &Path, lang_stats: &[LangStat]) -> Vec<Proposal> {
    detect_with(root, lang_stats, &|name| {
        crate::pty::resolve_command(name).is_ok()
    })
}

/// Detection core with an injectable binary resolver (so tests can fake a
/// missing tool without touching the machine's real PATH). Returns marker-driven
/// candidates (validated against `resolves` + their marker) plus, for any graph
/// language present without a marker to wire it, one greyed proposal so the
/// signal isn't lost.
pub fn detect_with(
    root: &Path,
    lang_stats: &[LangStat],
    resolves: &dyn Fn(&str) -> bool,
) -> Vec<Proposal> {
    let markers = scan_markers(root);
    let mut out = Vec::new();

    for eco in [
        Eco::Rust,
        Eco::TsJs,
        Eco::Go,
        Eco::Python,
        Eco::Dotnet,
        Eco::Jvm,
    ] {
        let graph_note = graph_note_for(eco, lang_stats);
        for cand in presets_for(eco, &markers) {
            let mut evidence = cand.evidence;
            if let Some(note) = &graph_note {
                evidence = format!("{evidence} + {note}");
            }
            let (valid, reason) = validate(&cand.binary, resolves);
            out.push(Proposal {
                check: cand.check,
                ecosystem: eco.label().to_string(),
                evidence,
                valid,
                reason,
            });
        }
    }

    // Union with the graph: a language the graph indexed but whose ecosystem
    // has no marker (a manifest deeper than the walk, or simply absent at the
    // top) can't be wired to a runnable command — surface it greyed so the user
    // knows it was seen and can configure it by hand, rather than dropping it.
    for eco in [
        Eco::Rust,
        Eco::TsJs,
        Eco::Go,
        Eco::Python,
        Eco::Dotnet,
        Eco::Jvm,
    ] {
        if eco_has_anchor(eco, &markers) {
            continue; // already produced marker-driven proposals above
        }
        if let Some(files) = graph_files(eco, lang_stats) {
            out.push(Proposal {
                check: eco_primary_check(eco),
                ecosystem: eco.label().to_string(),
                evidence: format!("code graph: {files} {} files", primary_lang(eco)),
                valid: false,
                reason: Some(format!(
                    "{} detected by the code graph but no {} found near the project root — configure manually",
                    eco.label(),
                    eco.manifest()
                )),
            });
        }
    }

    out
}

/// `(binary_name, evidence)` bundle for one candidate before graph annotation /
/// validation. A candidate is only ever constructed once its gating marker was
/// found (presets emit nothing otherwise), so marker presence is implicit and
/// isn't carried on the struct.
struct Candidate {
    check: CheckDef,
    binary: String,
    evidence: String,
}

/// The preset catalog: ecosystem × markers → candidate [`CheckDef`]s. Data in,
/// candidates out — the whole per-ecosystem policy lives here (no `if`s sprayed
/// through [`detect_with`]). Only candidates whose gating marker is present are
/// emitted; PATH validation happens later in [`detect_with`].
fn presets_for(eco: Eco, m: &Markers) -> Vec<Candidate> {
    let mut v = Vec::new();
    match eco {
        Eco::Rust => {
            if m.rust_anchor.is_some() {
                let cwd = anchor_cwd(&m.rust_anchor);
                let where_ = cwd.as_deref().unwrap_or("project root");
                let ev = format!("Cargo.toml ({where_})");
                v.push(cand(
                    "cargo-check",
                    "cargo check --message-format=json",
                    ParserKind::CargoJson,
                    300,
                    cwd.clone(),
                    "cargo",
                    &ev,
                ));
                v.push(cand(
                    "cargo-test",
                    "cargo test",
                    ParserKind::CargoTest,
                    600,
                    cwd,
                    "cargo",
                    &ev,
                ));
            }
        }
        Eco::TsJs => {
            let cwd = anchor_cwd(&m.js_anchor);
            if m.tsconfig {
                v.push(cand(
                    "tsc",
                    "tsc --noEmit --pretty false",
                    ParserKind::Tsc,
                    300,
                    cwd.clone(),
                    "tsc",
                    "tsconfig.json",
                ));
            }
            if m.eslint {
                v.push(cand(
                    "eslint",
                    "eslint --format json .",
                    ParserKind::EslintJson,
                    300,
                    cwd.clone(),
                    "eslint",
                    "eslint config",
                ));
            }
            // Prefer vitest when both are present (a project migrating off jest
            // keeps the old config around); vitest's JSON reporter matches the
            // same `jest-json` parser shape.
            if m.vitest {
                v.push(cand(
                    "vitest",
                    "vitest run --reporter=json",
                    ParserKind::JestJson,
                    300,
                    cwd,
                    "vitest",
                    "vitest config",
                ));
            } else if m.jest {
                v.push(cand(
                    "jest",
                    "jest --json",
                    ParserKind::JestJson,
                    300,
                    cwd,
                    "jest",
                    "jest config",
                ));
            }
        }
        Eco::Go => {
            if m.go_anchor.is_some() {
                let cwd = anchor_cwd(&m.go_anchor);
                v.push(cand(
                    "go-vet",
                    "go vet ./...",
                    ParserKind::Go,
                    300,
                    cwd.clone(),
                    "go",
                    "go.mod",
                ));
                v.push(cand(
                    "go-test",
                    "go test -json ./...",
                    ParserKind::GoTestJson,
                    600,
                    cwd,
                    "go",
                    "go.mod",
                ));
            }
        }
        Eco::Python => {
            let cwd = anchor_cwd(&m.py_anchor);
            if m.ruff {
                v.push(cand(
                    "ruff",
                    "ruff check --output-format sarif .",
                    ParserKind::Sarif,
                    120,
                    cwd.clone(),
                    "ruff",
                    "ruff config",
                ));
            }
            if m.pytest {
                v.push(cand(
                    "pytest",
                    "pytest -q",
                    ParserKind::Pytest,
                    600,
                    cwd,
                    "pytest",
                    "pytest/tests",
                ));
            }
        }
        Eco::Dotnet => {
            if m.dotnet_anchor.is_some() {
                let cwd = anchor_cwd(&m.dotnet_anchor);
                v.push(cand(
                    "dotnet-build",
                    "dotnet build --nologo",
                    ParserKind::Dotnet,
                    600,
                    cwd,
                    "dotnet",
                    ".csproj/.sln",
                ));
            }
        }
        Eco::Jvm => {
            // Maven and Gradle both write JUnit XML per test class to a
            // conventional REPORT DIRECTORY (Surefire / Gradle test-results).
            // `junit-xml` reads a single file, so `report_file` is seeded with
            // that directory as a starting point the user narrows to one file
            // via the Phase E "Test" button. Prefer Maven when both exist.
            if m.maven_anchor.is_some() {
                let cwd = anchor_cwd(&m.maven_anchor);
                v.push(cand_report(
                    "mvn-test",
                    "mvn -q test",
                    ParserKind::JunitXml,
                    900,
                    cwd,
                    "target/surefire-reports",
                    "mvn",
                    "pom.xml",
                ));
            } else if m.gradle_anchor.is_some() {
                let cwd = anchor_cwd(&m.gradle_anchor);
                v.push(cand_report(
                    "gradle-test",
                    "gradle test",
                    ParserKind::JunitXml,
                    900,
                    cwd,
                    "build/test-results/test",
                    "gradle",
                    "build.gradle",
                ));
            }
        }
    }
    v
}

/// Build a plain candidate (no `report_file`).
fn cand(
    name: &str,
    cmd: &str,
    parser: ParserKind,
    timeout_secs: u64,
    cwd: Option<String>,
    binary: &str,
    evidence: &str,
) -> Candidate {
    Candidate {
        check: CheckDef {
            name: name.to_string(),
            cmd: cmd.to_string(),
            parser,
            timeout_secs,
            cwd,
            auto: true,
            ..Default::default()
        },
        binary: binary.to_string(),
        evidence: evidence.to_string(),
    }
}

/// Build a candidate whose parser reads a `report_file` (junit-xml).
#[allow(clippy::too_many_arguments)]
fn cand_report(
    name: &str,
    cmd: &str,
    parser: ParserKind,
    timeout_secs: u64,
    cwd: Option<String>,
    report_file: &str,
    binary: &str,
    evidence: &str,
) -> Candidate {
    let mut c = cand(name, cmd, parser, timeout_secs, cwd, binary, evidence);
    c.check.report_file = Some(report_file.to_string());
    c
}

/// Validate a candidate: its binary must resolve on PATH. (Marker presence is
/// already guaranteed — a candidate is only emitted once its gating marker was
/// found.) Returns `(valid, reason)` — `reason` is `Some` only when invalid.
fn validate(binary: &str, resolves: &dyn Fn(&str) -> bool) -> (bool, Option<String>) {
    if !resolves(binary) {
        return (false, Some(format!("{binary} not found on PATH")));
    }
    (true, None)
}

/// Whether the marker scan found this ecosystem's anchor (the primary manifest).
fn eco_has_anchor(eco: Eco, m: &Markers) -> bool {
    match eco {
        Eco::Rust => m.rust_anchor.is_some(),
        Eco::TsJs => m.js_anchor.is_some(),
        Eco::Go => m.go_anchor.is_some(),
        Eco::Python => m.py_anchor.is_some(),
        Eco::Dotnet => m.dotnet_anchor.is_some(),
        Eco::Jvm => m.maven_anchor.is_some() || m.gradle_anchor.is_some(),
    }
}

/// Total indexed files the graph attributes to this ecosystem, `None` if zero.
fn graph_files(eco: Eco, stats: &[LangStat]) -> Option<u64> {
    let sum: u64 = stats
        .iter()
        .filter(|s| eco.lang_tags().contains(&s.lang.as_str()))
        .map(|s| s.files)
        .sum();
    (sum > 0).then_some(sum)
}

/// The graph-corroboration evidence fragment for an ecosystem, `None` when the
/// graph attributes it no files.
fn graph_note_for(eco: Eco, stats: &[LangStat]) -> Option<String> {
    graph_files(eco, stats).map(|n| format!("code graph: {n} {} files", primary_lang(eco)))
}

/// The label used in graph-evidence text for an ecosystem's primary language.
fn primary_lang(eco: Eco) -> &'static str {
    match eco {
        Eco::Rust => "rust",
        Eco::TsJs => "typescript/javascript",
        Eco::Go => "go",
        Eco::Python => "python",
        Eco::Dotnet => "c#",
        Eco::Jvm => "jvm",
    }
}

/// The single representative check used for a greyed graph-only proposal.
fn eco_primary_check(eco: Eco) -> CheckDef {
    match eco {
        Eco::Rust => {
            cand(
                "cargo-check",
                "cargo check --message-format=json",
                ParserKind::CargoJson,
                300,
                None,
                "cargo",
                "",
            )
            .check
        }
        Eco::TsJs => {
            cand(
                "tsc",
                "tsc --noEmit --pretty false",
                ParserKind::Tsc,
                300,
                None,
                "tsc",
                "",
            )
            .check
        }
        Eco::Go => {
            cand(
                "go-vet",
                "go vet ./...",
                ParserKind::Go,
                300,
                None,
                "go",
                "",
            )
            .check
        }
        Eco::Python => {
            cand(
                "pytest",
                "pytest -q",
                ParserKind::Pytest,
                600,
                None,
                "pytest",
                "",
            )
            .check
        }
        Eco::Dotnet => {
            cand(
                "dotnet-build",
                "dotnet build --nologo",
                ParserKind::Dotnet,
                600,
                None,
                "dotnet",
                "",
            )
            .check
        }
        Eco::Jvm => {
            cand_report(
                "mvn-test",
                "mvn -q test",
                ParserKind::JunitXml,
                900,
                None,
                "target/surefire-reports",
                "mvn",
                "",
            )
            .check
        }
    }
}

/// Merge selected proposal checks into an existing `checks` list, respecting
/// the `auto` ownership rule (V22 Phase D):
///
/// - a new name is appended (with `auto = true`),
/// - an existing `auto == true` entry is UPDATED in place (re-detection may
///   refresh a machine-authored check),
/// - an existing `auto == false` entry is NEVER touched (user-created or
///   user-edited — it owns its name).
///
/// Returns the names actually written (added or updated) — the "applied set"
/// the Phase E chip reports. Idempotent: applying the same proposals twice
/// updates-in-place, so no duplicate names accumulate.
pub fn merge_auto(existing: &mut Vec<CheckDef>, incoming: Vec<CheckDef>) -> Vec<String> {
    let mut applied = Vec::new();
    for mut def in incoming {
        def.auto = true; // detection-authored, regardless of the source struct
        match existing.iter_mut().find(|e| e.name == def.name) {
            Some(cur) => {
                if cur.auto {
                    let name = def.name.clone();
                    *cur = def;
                    applied.push(name);
                }
                // auto == false ⇒ user-owned; leave it alone.
            }
            None => {
                let name = def.name.clone();
                existing.push(def);
                applied.push(name);
            }
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tempdir project fixture, cleaned on drop — the pattern the rest of the
    /// `checks` tests use for real trees.
    struct Fixture {
        root: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("checks-detect-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Fixture { root }
        }
        /// Write a file (creating parent dirs) with `contents`, path relative to root.
        fn file(&self, rel: &str, contents: &str) -> &Self {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, contents).unwrap();
            self
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Resolver that says every binary is present — isolates the marker/preset
    /// logic from whatever tools happen to be installed on the test machine.
    fn all_present(_: &str) -> bool {
        true
    }

    fn find<'a>(props: &'a [Proposal], name: &str) -> Option<&'a Proposal> {
        props.iter().find(|p| p.check.name == name)
    }

    #[test]
    fn nested_cargo_manifest_gets_cwd() {
        // This repo's shape: Cargo.toml at src-tauri/, not the root.
        let fx = Fixture::new();
        fx.file("src-tauri/Cargo.toml", "[package]\nname=\"x\"\n");
        let props = detect_with(&fx.root, &[], &all_present);

        let check = find(&props, "cargo-check").expect("cargo-check proposed");
        assert_eq!(
            check.check.cwd.as_deref(),
            Some("src-tauri"),
            "nested cwd points at the manifest dir"
        );
        assert_eq!(check.check.parser, ParserKind::CargoJson);
        assert!(check.valid);
        assert!(check.check.auto, "detected entries are marked auto");
        let test = find(&props, "cargo-test").expect("cargo-test proposed");
        assert_eq!(test.check.cwd.as_deref(), Some("src-tauri"));
        assert_eq!(test.check.parser, ParserKind::CargoTest);
    }

    #[test]
    fn root_cargo_manifest_has_no_cwd() {
        let fx = Fixture::new();
        fx.file("Cargo.toml", "[package]\nname=\"x\"\n");
        let props = detect_with(&fx.root, &[], &all_present);
        let check = find(&props, "cargo-check").expect("cargo-check proposed");
        assert_eq!(check.check.cwd, None, "a root manifest runs at the root");
    }

    #[test]
    fn ts_tsconfig_without_eslint_proposes_tsc_not_eslint() {
        let fx = Fixture::new();
        fx.file("package.json", "{}").file("tsconfig.json", "{}");
        let props = detect_with(&fx.root, &[], &all_present);
        assert!(
            find(&props, "tsc").is_some(),
            "tsc proposed when tsconfig present"
        );
        assert!(
            find(&props, "eslint").is_none(),
            "eslint NOT proposed without an eslint config"
        );
        assert!(find(&props, "jest").is_none() && find(&props, "vitest").is_none());
    }

    #[test]
    fn ts_eslint_and_vitest_gated_on_their_configs() {
        let fx = Fixture::new();
        fx.file("package.json", "{}")
            .file("tsconfig.json", "{}")
            .file(".eslintrc.json", "{}")
            .file("vitest.config.ts", "export default {}");
        let props = detect_with(&fx.root, &[], &all_present);
        assert!(find(&props, "tsc").is_some());
        assert!(
            find(&props, "eslint").is_some(),
            "eslint proposed when config present"
        );
        let vitest = find(&props, "vitest").expect("vitest proposed on vitest.config");
        assert_eq!(vitest.check.parser, ParserKind::JestJson);
        assert!(
            find(&props, "jest").is_none(),
            "vitest wins over jest when both/only-vitest present"
        );
    }

    #[test]
    fn go_fixture_proposes_vet_and_test() {
        let fx = Fixture::new();
        fx.file("go.mod", "module example.com/x\n");
        let props = detect_with(&fx.root, &[], &all_present);
        let vet = find(&props, "go-vet").expect("go vet proposed");
        assert_eq!(vet.check.parser, ParserKind::Go);
        let test = find(&props, "go-test").expect("go test proposed");
        assert_eq!(test.check.parser, ParserKind::GoTestJson);
    }

    #[test]
    fn python_pyproject_with_ruff_proposes_ruff_sarif_and_pytest() {
        let fx = Fixture::new();
        fx.file(
            "pyproject.toml",
            "[tool.ruff]\nline-length = 100\n[tool.pytest.ini_options]\naddopts = \"-q\"\n",
        );
        let props = detect_with(&fx.root, &[], &all_present);
        let ruff = find(&props, "ruff").expect("ruff proposed when [tool.ruff] present");
        assert_eq!(ruff.check.parser, ParserKind::Sarif);
        assert!(
            find(&props, "pytest").is_some(),
            "pytest proposed from [tool.pytest]"
        );
    }

    #[test]
    fn python_without_ruff_config_omits_ruff() {
        let fx = Fixture::new();
        fx.file("pyproject.toml", "[project]\nname = \"x\"\n");
        fx.file("tests/test_x.py", "def test_x():\n    assert True\n");
        let props = detect_with(&fx.root, &[], &all_present);
        assert!(
            find(&props, "ruff").is_none(),
            "no ruff config ⇒ no ruff proposal"
        );
        assert!(
            find(&props, "pytest").is_some(),
            "a tests/ dir triggers pytest"
        );
    }

    #[test]
    fn jvm_maven_seeds_report_file() {
        let fx = Fixture::new();
        fx.file("pom.xml", "<project></project>");
        let props = detect_with(&fx.root, &[], &all_present);
        let mvn = find(&props, "mvn-test").expect("mvn test proposed");
        assert_eq!(mvn.check.parser, ParserKind::JunitXml);
        assert_eq!(
            mvn.check.report_file.as_deref(),
            Some("target/surefire-reports")
        );
    }

    #[test]
    fn jvm_nested_maven_module_pairs_cwd_and_report_file_consistently() {
        // A pom.xml in a nested module (`backend/pom.xml`) ⇒ cwd="backend" and
        // the report dir is left UNPREFIXED ("target/surefire-reports"). Under
        // the cwd-relative `report_file` semantics that pairing is now correct:
        // the effective location is backend/target/surefire-reports — exactly
        // where `mvn` (run in `backend`) writes Surefire XML. Before the fix,
        // run() read the root's `target/surefire-reports` and always errored.
        let fx = Fixture::new();
        fx.file("backend/pom.xml", "<project></project>");
        let props = detect_with(&fx.root, &[], &all_present);
        let mvn = find(&props, "mvn-test").expect("mvn test proposed for nested module");
        assert_eq!(
            mvn.check.cwd.as_deref(),
            Some("backend"),
            "cwd anchors to the nested module dir"
        );
        assert_eq!(
            mvn.check.report_file.as_deref(),
            Some("target/surefire-reports"),
            "report_file stays cwd-relative (unprefixed), not root-prefixed"
        );
        // Effective (cwd-joined) location is the module's own report dir.
        let effective = format!(
            "{}/{}",
            mvn.check.cwd.as_deref().unwrap(),
            mvn.check.report_file.as_deref().unwrap()
        );
        assert_eq!(effective, "backend/target/surefire-reports");
    }

    #[test]
    fn dotnet_csproj_proposes_build() {
        let fx = Fixture::new();
        fx.file("App.csproj", "<Project></Project>");
        let props = detect_with(&fx.root, &[], &all_present);
        let d = find(&props, "dotnet-build").expect("dotnet build proposed");
        assert_eq!(d.check.parser, ParserKind::Dotnet);
    }

    #[test]
    fn path_validation_greys_a_missing_binary() {
        let fx = Fixture::new();
        fx.file("go.mod", "module x\n");
        // Everything present EXCEPT `go`.
        let resolver = |name: &str| name != "go";
        let props = detect_with(&fx.root, &[], &resolver);
        let vet = find(&props, "go-vet").expect("go vet still listed");
        assert!(!vet.valid, "go-vet is invalid when `go` is absent");
        assert_eq!(vet.reason.as_deref(), Some("go not found on PATH"));
    }

    #[test]
    fn graph_and_markers_union_annotates_evidence() {
        // Marker fixture (Rust) + graph stats (rust + an extra go with no marker).
        let fx = Fixture::new();
        fx.file("src-tauri/Cargo.toml", "[package]\nname=\"x\"\n");
        let stats = vec![
            LangStat {
                lang: "rust".into(),
                files: 812,
            },
            LangStat {
                lang: "go".into(),
                files: 3,
            },
        ];
        let props = detect_with(&fx.root, &stats, &all_present);

        // Rust proposal (marker-driven) carries the graph corroboration.
        let cargo = find(&props, "cargo-check").expect("cargo-check");
        assert!(
            cargo.evidence.contains("Cargo.toml"),
            "marker in evidence: {}",
            cargo.evidence
        );
        assert!(
            cargo.evidence.contains("812 rust files"),
            "graph note in evidence: {}",
            cargo.evidence
        );

        // Go is in the graph but has no go.mod ⇒ a greyed, graph-only proposal.
        let go = find(&props, "go-vet").expect("greyed go proposal from graph stats");
        assert!(!go.valid);
        assert!(
            go.reason.as_deref().unwrap().contains("go.mod"),
            "reason: {:?}",
            go.reason
        );
    }

    #[test]
    fn markers_only_when_no_graph() {
        let fx = Fixture::new();
        fx.file("Cargo.toml", "[package]\nname=\"x\"\n");
        let props = detect_with(&fx.root, &[], &all_present);
        // Same marker-driven proposals, and no greyed graph-only entries.
        assert!(find(&props, "cargo-check").is_some());
        assert!(
            !props.iter().any(|p| p.evidence.contains("code graph")),
            "no graph annotation without stats"
        );
        // No proposal for an ecosystem with neither marker nor graph.
        assert!(find(&props, "go-vet").is_none());
    }

    #[test]
    fn scan_skips_heavy_dirs() {
        let fx = Fixture::new();
        // A stray manifest buried in node_modules must NOT be picked up.
        fx.file("node_modules/dep/Cargo.toml", "[package]\nname=\"dep\"\n");
        fx.file("target/foo/go.mod", "module junk\n");
        let props = detect_with(&fx.root, &[], &all_present);
        assert!(
            find(&props, "cargo-check").is_none(),
            "node_modules manifest ignored"
        );
        assert!(find(&props, "go-vet").is_none(), "target manifest ignored");
    }

    #[test]
    fn merge_appends_new_and_is_idempotent() {
        let mut existing: Vec<CheckDef> = Vec::new();
        let incoming = vec![
            CheckDef {
                name: "cargo-check".into(),
                auto: true,
                ..Default::default()
            },
            CheckDef {
                name: "cargo-test".into(),
                auto: true,
                ..Default::default()
            },
        ];
        let applied = merge_auto(&mut existing, incoming.clone());
        assert_eq!(applied.len(), 2);
        assert_eq!(existing.len(), 2);
        // Apply the same set again ⇒ update-in-place, no duplicates.
        let applied2 = merge_auto(&mut existing, incoming);
        assert_eq!(applied2.len(), 2, "re-apply updates the two auto entries");
        assert_eq!(existing.len(), 2, "no duplicate names accumulate");
    }

    #[test]
    fn merge_never_overwrites_a_user_entry_but_updates_auto() {
        let mut existing = vec![
            // User-authored (or user-edited): auto == false, custom cmd.
            CheckDef {
                name: "cargo-check".into(),
                cmd: "cargo clippy".into(),
                auto: false,
                ..Default::default()
            },
            // Machine-authored earlier: auto == true.
            CheckDef {
                name: "cargo-test".into(),
                cmd: "cargo test --old".into(),
                auto: true,
                ..Default::default()
            },
        ];
        let incoming = vec![
            CheckDef {
                name: "cargo-check".into(),
                cmd: "cargo check".into(),
                ..Default::default()
            },
            CheckDef {
                name: "cargo-test".into(),
                cmd: "cargo test".into(),
                ..Default::default()
            },
        ];
        let applied = merge_auto(&mut existing, incoming);
        // The user entry was preserved; only the auto one updated.
        assert_eq!(applied, vec!["cargo-test".to_string()]);
        let user = existing.iter().find(|c| c.name == "cargo-check").unwrap();
        assert_eq!(user.cmd, "cargo clippy", "user entry untouched");
        assert!(!user.auto, "user entry stays auto == false");
        let auto = existing.iter().find(|c| c.name == "cargo-test").unwrap();
        assert_eq!(auto.cmd, "cargo test", "auto entry refreshed");
        assert!(auto.auto);
    }
}
