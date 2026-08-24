//! V33 — the runtime-profile table and the inference over it.
//!
//! **R17 (V42): moved out of `sandbox/mod.rs` verbatim.** That module was
//! 4 113 lines of three unrelated concerns; this is the largest of them — a
//! data table plus the pure functions that read it. Not one line of logic
//! changed in the move, and `sandbox/mod.rs` re-exports every name here, so
//! every `crate::sandbox::…` path a caller already used still resolves. The
//! section comment the table shipped under follows verbatim.
//!
//! ── the runtime profile table (V33, 2026-08-19) ─────────────────────────────
//!
//! **The problem this table generalizes.** A sandboxed child is granted its own
//! install directory and the project root, and nothing else. That is enough for
//! a self-contained binary (`git.exe`, `typos.exe`) and it is *never* enough for
//! a program that is really a front end to a RUNTIME: a pip console-script stub
//! loads `python3XX.dll` and the standard library from the install root one
//! directory up; a rustup shim resolves `rustc` through `RUSTUP_HOME`; a
//! `node_modules\.bin\*.cmd` shim starts `node.exe` from somewhere else
//! entirely; a JVM launcher cannot start without the runtime image beside it.
//! Worse, the engine redirects `HOME`/`USERPROFILE` into the sandbox root
//! (windows::prepare_blocking), so a runtime whose state pointer is UNSET
//! resolves it against an empty scratch directory and a runtime whose pointer IS
//! set names a directory the container was never granted. Either way the tool
//! starts and then dies for a reason that looks nothing like a sandbox — often
//! with both streams empty (see [`record_silent_exit`]).
//!
//! Until 2026-08-19 this module answered that with exactly two hardcoded
//! special cases: `interpreter_root` (the Python `Scripts` convention) and
//! `toolchain_state` (the rustup convention). Both were right and neither
//! generalized. The table below is those two rules plus the rest of the S1
//! addendum's toolchain matrix
//! (`docs/reviews/SPIKE-S1-appcontainer-2026-08-15.md`), in the house shape
//! [`child_env::CHILD_ENV`] and [`GRANT_REFUSAL_RULES`] already use: data in
//! code, one row per runtime, a reason on every widening, and a reviewer of the
//! diff that adds a row sees the pattern and the justification together.
//!
//! **Two rules from S1 shape every row.** (a) *Install location decides the
//! grant*: anything under `Program Files` or `%SystemRoot%` is already readable
//! by `ALL APPLICATION PACKAGES`, so those rows cost nothing
//! (`windows::is_app_package_readable` short-circuits them); a user-owned tree
//! costs one RX ACE; an Administrators-owned tree cannot be granted unelevated
//! at all and degrades through the loud ladder (`grant_dir` errors →
//! `prepare` errors → [`Plan::Plain`] → the child runs unsandboxed and says so).
//! (b) *State directories get env-redirected*: a cache or scratch directory
//! moves INTO the sandbox root ([`RuntimeEnv::Scratch`]), while read-only state
//! a tool must actually find keeps pointing at the real thing
//! ([`RuntimeEnv::Dir`]) and is granted read+execute beside the pointer.
//!
//! **What was measured before this shipped, and what was reasoned.** S1 supplies
//! the in-container half (go/dotnet/clang/python/java all execute under
//! AppContainer; npm needs `--preserve-symlinks`; `DOTNET_CLI_HOME` and the Go
//! cache trio must be redirected). What S1 did NOT establish is that this
//! table's exact composition is one a runtime accepts, so every row's variables
//! were run against the real toolchains on this machine on 2026-08-19 — outside
//! the container, which is where an environment contract can be falsified
//! without stamping anything: `go env` confirmed that an explicit `GOMODCACHE`
//! really does survive a redirected `GOPATH` (the split this table depends on,
//! and the one thing here that could not be deduced), `node -e` accepted
//! `NODE_OPTIONS`, `dotnet --version` ran with `DOTNET_CLI_HOME` redirected,
//! `java -version` with `JAVA_HOME` re-asserted, and `python -c` with
//! `PYTHONPYCACHEPREFIX` pointed into a scratch tree. What remains reasoned
//! rather than measured is whether a read-only NuGet/module cache is *enough*
//! for a restore inside the boundary — a restore that needs to WRITE one fails
//! with a denial the classifier recognizes, which is the honest outcome either
//! way.
//!
//! **What no row may ever do.** Grant a volume root, a user-profile root,
//! `%SystemRoot%` or a credential store — not because a row would want to, but
//! because every path here is INFERRED from the machine (an environment
//! variable, a directory name) rather than read from a reviewed constant, and an
//! inference is exactly the kind of input that should not be trusted with a
//! durable inheritable ACE. So every path the table produces goes through
//! [`extra_grant_refusal`], the same screen the settings-supplied rows get.
//! Defence in depth on purpose: cImp-derived grants are not screened anywhere
//! else, and these are the cImp-derived grants that a hostile environment
//! variable can steer.
//!
//! **Machine-wide ACL weakening is not on the menu.** A runtime that cannot work
//! without `C:\`, `C:\Users` or `%USERPROFILE%` being opened stays unsupported
//! and says so in its row's gap text: widening those would widen the boundary
//! for every AppContainer on the machine, browser renderers included, which is a
//! far larger change than anything cImp is entitled to make on a tool's behalf.

use std::path::{Path, PathBuf};

use super::{ends_with, extra_grant_refusal, lower_components, RuntimeSelect};

/// The machine a runtime rule may look at — **injected, never read directly**,
/// so every rule below is a pure function that both platforms' test runs can
/// drive with a synthetic machine and no filesystem. Exactly the discipline
/// [`extra_grant_refusal`] and [`child_env::minimal_env`] already follow.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Machine<'a> {
    /// One environment variable's value.
    ///
    /// Production reads the **composed child environment first, cImp's own
    /// process environment second** (`windows::prepare_blocking`). Both halves
    /// are load-bearing: the child's copy is what the tool will actually see,
    /// so a seam that forced `CARGO_HOME` wins; but the child's environment is
    /// the C2 *ceiling* ([`child_env::CHILD_ENV`]) and most runtime pointers —
    /// `JAVA_HOME`, `GOPATH`, `NUGET_PACKAGES` — are deliberately not on it, so
    /// a table that read only the child's copy would be blind to every runtime
    /// except rust and npm. Reading cImp's copy is not a hole: the value is
    /// re-asserted onto the child through a reviewed row with a reason, which
    /// is precisely the shape the C2 table exists to force.
    pub env: &'a dyn Fn(&str) -> Option<std::ffi::OsString>,
    /// Does this path name an existing DIRECTORY?
    ///
    /// A parameter for the same reason: "the user has a `.rustup`" is a fact
    /// about a machine, not about a convention, and no rule here may invent
    /// state. A pointer to a directory that does not exist is neither granted
    /// nor set — stamping (or naming) a path the user never created would be
    /// cImp manufacturing state rather than reaching the state that is there.
    pub is_dir: &'a dyn Fn(&Path) -> bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Machine<'_> {
    /// One variable as a `PathBuf`, empty values dropped.
    fn path_var(&self, name: &str) -> Option<PathBuf> {
        let v = (self.env)(name)?;
        (!v.is_empty()).then(|| PathBuf::from(v))
    }
    /// One variable as a directory that EXISTS.
    fn dir_var(&self, name: &str) -> Option<PathBuf> {
        self.path_var(name).filter(|p| (self.is_dir)(p))
    }
    /// The user's profile directory, Windows spelling first.
    fn home(&self) -> Option<PathBuf> {
        self.path_var("USERPROFILE").or_else(|| self.path_var("HOME"))
    }
    /// The Windows install directory, for the refusal screen.
    fn system_root(&self) -> Option<PathBuf> {
        self.path_var("SystemRoot")
    }
}

/// The program a grant is being inferred from, pre-chewed into the two forms
/// every rule matches on.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Program<'a> {
    /// The program's own file name, lowercased (`node.exe`).
    pub file: String,
    /// The directory it lives in — the one the engine always grants R+X, so a
    /// row never has to ask for it.
    pub dir: &'a Path,
    /// `dir`'s components, lowercased. Precomputed because every
    /// [`Detect::DirTail`] arm of every row reads it.
    dir_comps: Vec<String>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl<'a> Program<'a> {
    /// `None` for a program with no directory or a non-UTF-8 name — neither can
    /// be matched against a rule, and guessing is how a rule fires on the wrong
    /// tree.
    pub fn at(program: &'a Path) -> Option<Self> {
        let file = program.file_name()?.to_str()?.to_ascii_lowercase();
        let dir = program.parent()?;
        if dir.as_os_str().is_empty() {
            return None;
        }
        Some(Self {
            file,
            dir,
            dir_comps: lower_components(dir),
        })
    }

    /// Is the program's own directory named exactly this (lowercase)?
    fn dir_named(&self, name: &str) -> bool {
        self.dir_comps.last().is_some_and(|c| c == name)
    }
}

/// How a [`RuntimeProfile`] recognizes that a program belongs to its runtime.
///
/// Both arms are *layout* facts, never tool identities: a rule keyed on "this
/// is semgrep" would have to be extended for every tool that ever ships, while
/// a rule keyed on "this is a pip console-script stub" already covers the ones
/// nobody has installed yet. Where a row does name a specific launcher
/// (`pmd.bat`, `golangci-lint.exe`) it says in its reason that the row is about
/// what that launcher STARTS, not about the tool itself.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Detect {
    /// The program's file name is one of these, compared lowercased. A single
    /// `*` is a wildcard for the varying middle of a family name
    /// (`python*.exe` matches `python3.14.exe`, `python.exe`, `python3.exe`).
    /// Both the `.exe` and bare spellings are listed where a row is meant to
    /// fire on POSIX too.
    Program(&'static [&'static str]),
    /// The program's directory chain ENDS in these components, outermost
    /// first — `&[".cargo", "bin"]` matches `…\.cargo\bin` and nothing else.
    ///
    /// The trailing-components form rather than "the parent is called X",
    /// because the narrowness is the whole point: `bin` alone would fire for
    /// `C:\Program Files\Git\usr\bin` and `/usr/bin`, and the rule behind it
    /// would then grant `/usr` on the strength of a directory name.
    DirTail(&'static [&'static str]),
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Detect {
    fn matches(&self, p: &Program) -> bool {
        match self {
            Detect::Program(names) => names.iter().any(|n| glob1(n, &p.file)),
            Detect::DirTail(tail) => ends_with(&p.dir_comps, tail),
        }
    }
}

/// A one-`*` glob, both sides already lowercase. Deliberately not a regex and
/// deliberately not multi-`*`: the only variation these names have is a version
/// in the middle, and a richer matcher is a richer way to fire on the wrong
/// program.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn glob1(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len()
                && name.starts_with(pre)
                && name.ends_with(post)
        }
    }
}

/// What one environment pointer a runtime needs must be set to.
///
/// The three arms are the whole design: a pointer either names REAL state the
/// tool has to find (and which therefore also needs a grant), or it names
/// scratch that must be REDIRECTED into the sandbox's one writable place, or it
/// is not a path at all. Collapsing them would lose exactly the distinction
/// that makes the boundary work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum RuntimeEnv {
    /// A real directory on the machine. Always paired with a grant for the same
    /// directory — a pointer the container cannot read is worse than no pointer.
    Dir(PathBuf),
    /// A subdirectory of [`SANDBOX_SCRATCH_DIR`] inside the sandbox root, for
    /// caches and scratch the tool WRITES. Resolved by
    /// [`compose_env_overrides`] once the root's drive letter exists.
    Scratch(&'static str),
    /// A literal value that is not a path — a flag string.
    ///
    /// **Never sourced from cImp's own environment**, which is the entire
    /// reason `NODE_OPTIONS` can appear here while [`child_env::CHILD_ENV`]
    /// deliberately refuses to pass it through: the C2 omission is about not
    /// INHERITING a variable that names files the child would then load, and
    /// this is a reviewed constant with a measurement behind it.
    Literal(&'static str),
}

/// One directory a runtime needs granted read+execute, with the reason the
/// user's grant row prints beside the path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeGrant {
    pub dir: PathBuf,
    pub why: &'static str,
}

/// One environment pointer a runtime needs set on the child.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeVar {
    pub name: &'static str,
    pub value: RuntimeEnv,
    pub why: &'static str,
}

/// A need this boundary does **not** meet, stated rather than dropped.
///
/// Decision 5's loud-degradation rule applied one level down: "the sandbox is
/// on" and "the sandbox is on and this runtime is missing half of what it
/// needs" are two different states, and a user whose tool exits 1 with no
/// output has no other way to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeGap {
    /// What is missing — a path, or the thing that could not be inferred.
    pub what: String,
    pub why: &'static str,
}

/// Everything one runtime asks for, before the refusal screen runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeNeeds {
    pub grants: Vec<RuntimeGrant>,
    pub env: Vec<RuntimeVar>,
    pub gaps: Vec<RuntimeGap>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl RuntimeNeeds {
    /// Real state: grant the directory AND point its variable at it. Skipped
    /// entirely when the directory does not exist (see [`Machine::is_dir`]).
    fn state(&mut self, m: &Machine, dir: PathBuf, name: &'static str, why: &'static str) {
        if !(m.is_dir)(&dir) {
            return;
        }
        self.env.push(RuntimeVar {
            name,
            value: RuntimeEnv::Dir(dir.clone()),
            why,
        });
        self.grants.push(RuntimeGrant { dir, why });
    }
    /// A tree to grant with no pointer of its own (a runtime image, an
    /// interpreter root the launcher finds by relative path).
    fn tree(&mut self, m: &Machine, dir: PathBuf, why: &'static str) {
        if (m.is_dir)(&dir) {
            self.grants.push(RuntimeGrant { dir, why });
        }
    }
    /// Scratch redirected into the sandbox root. No existence check and no
    /// grant: the root is already granted read+write and the tool creates the
    /// directory itself on first use.
    fn scratch(&mut self, name: &'static str, sub: &'static str, why: &'static str) {
        self.env.push(RuntimeVar {
            name,
            value: RuntimeEnv::Scratch(sub),
            why,
        });
    }
    /// A non-path constant.
    fn literal(&mut self, name: &'static str, value: &'static str, why: &'static str) {
        self.env.push(RuntimeVar {
            name,
            value: RuntimeEnv::Literal(value),
            why,
        });
    }
    /// A need that cannot be met — recorded, never silent.
    fn gap(&mut self, what: impl Into<String>, why: &'static str) {
        self.gaps.push(RuntimeGap {
            what: what.into(),
            why,
        });
    }
    fn is_empty(&self) -> bool {
        self.grants.is_empty() && self.env.is_empty() && self.gaps.is_empty()
    }
}

/// One runtime, its detection and what it needs.
///
/// `needs` is a function rather than more data because the answers are
/// *derived* — from the program's own directory, from an environment pointer,
/// from what exists on the machine — while the parts a reviewer has to check
/// (which programs this fires for, and why the row exists at all) are data.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeProfile {
    /// The runtime's name in a grant row ("… — for node").
    pub id: &'static str,
    /// Any one match fires the row.
    pub detect: &'static [Detect],
    pub needs: fn(&Program, &Machine) -> RuntimeNeeds,
    /// Why this row exists, for the reviewer of the diff that changes it.
    pub why: &'static str,
}

/// The subdirectory of the sandbox root every redirected cache lands under.
///
/// One directory rather than six, and named after cImp so nobody wonders what
/// put it in their repository. It appears in the project root because the
/// project root is the only place a sandboxed child may write — the same
/// reason `TEMP`/`TMP` already point at the mapped drive root.
///
/// **It lives UNDER `.cimp/`, and that is load-bearing rather than tidy.**
/// cImp already writes `.cimp/` in every project (`config.json`,
/// `shadow.git`, the graph store) and projects ignore it as one rule — this
/// repo's own `.gitignore` carries `**/.cimp/`. A sibling top-level directory
/// would be a SECOND thing every user has to learn to ignore, and would show
/// up as untracked noise in `git status` the first time anyone enables
/// sandboxing. `sandbox::tabs::scratch_dir` already made this choice for the
/// per-tab `TEMP` (`.cimp/sandbox-tmp/<tab>`); this is the same rule for the
/// runtime caches, so the two cannot drift apart.
#[cfg_attr(not(windows), allow(dead_code))]
pub const SANDBOX_SCRATCH_DIR: &str = ".cimp/sandbox-cache";

/// **The table.** Order is presentation only; every matching row applies.
#[cfg_attr(not(windows), allow(dead_code))]
pub const RUNTIME_PROFILES: &[RuntimeProfile] = &[
    RuntimeProfile {
        id: "rust",
        // rustup's published layout: the shims live in `<CARGO_HOME>\bin`, and
        // `bin` ALONE is not the convention — the parent must be `.cargo`, or
        // this would grant `C:\Program Files\Git\usr` and `/usr` on the
        // strength of a directory name.
        detect: &[Detect::DirTail(&[".cargo", "bin"])],
        needs: rust_needs,
        why: "a rustup shim is a launcher: measured 2026-08-18, a sandboxed `cargo` with only \
              `…\\.cargo\\bin` granted dies on `could not create home directory: …\\.rustup`, and \
              with both homes granted it resolves offline and compiles",
    },
    RuntimeProfile {
        id: "python",
        // Either end of the convention: the interpreter itself, or one of the
        // console-script stubs `pip` writes next to it.
        detect: &[
            Detect::Program(&["python*.exe", "pythonw*.exe", "python", "python3"]),
            Detect::DirTail(&["scripts"]),
        ],
        needs: python_needs,
        why: "a pip console-script `.exe` is a stub that loads `python3XX.dll` and the standard \
              library from the install root one directory up — live rc.9, `audit:semgrep` was \
              granted only its `Scripts` directory and exited 1 with BOTH streams empty",
    },
    RuntimeProfile {
        id: "node",
        detect: &[
            Detect::Program(&[
                "node.exe", "node", "npm.cmd", "npx.cmd", "pnpm.cmd", "yarn.cmd", "npm", "npx",
                "pnpm", "yarn",
            ]),
            Detect::DirTail(&["node_modules", ".bin"]),
        ],
        needs: node_needs,
        why: "a `node_modules\\.bin` shim resolves INSIDE the project root (already granted full \
              access) but starts `node.exe`, which does not — and S1 measured npm needing \
              `--preserve-symlinks` to resolve its own shims through the boundary",
    },
    RuntimeProfile {
        id: "java",
        detect: &[Detect::Program(&[
            "java.exe", "javaw.exe", "javac.exe", "jar.exe", "jshell.exe", "javadoc.exe", "java",
            "javac", "pmd.bat", "pmd.cmd", "pmd",
        ])],
        needs: java_needs,
        why: "a JVM cannot start without the runtime image (`lib\\modules`, `conf`, the JNI DLLs) \
              beside its launcher; `pmd.bat` is in this list because of what it STARTS — it is a \
              JVM launcher script, not a special-cased tool",
    },
    RuntimeProfile {
        id: "dotnet",
        detect: &[Detect::Program(&["dotnet.exe", "dotnet"])],
        needs: dotnet_needs,
        why: "S1 measured the .NET SDK working under AppContainer once `DOTNET_CLI_HOME` is \
              redirected into the root; the package cache is the other half, because a restore \
              that cannot read it re-downloads a graph the boundary denies egress for",
    },
    RuntimeProfile {
        id: "go",
        detect: &[Detect::Program(&[
            "go.exe",
            "gofmt.exe",
            "go",
            "gofmt",
            "golangci-lint.exe",
            "golangci-lint",
        ])],
        needs: go_needs,
        why: "S1 verified a full `go build` inside the container with GOCACHE/GOPATH/GOTMPDIR \
              redirected into the root; `golangci-lint` is listed because it drives the same \
              toolchain and writes the same caches",
    },
    RuntimeProfile {
        id: "windows-store-alias",
        detect: &[Detect::DirTail(&["microsoft", "windowsapps"])],
        needs: store_alias_needs,
        why: "S1: the Store interpreter aliases are reparse points in unlistable profile \
              territory — the container's PATH search never resolves them and no grant fixes it, \
              so this row exists ONLY to say so out loud",
    },
];

/// rustup: `<CARGO_HOME>\bin\<shim>.exe`, whose sibling `RUSTUP_HOME` defaults
/// to the same profile directory. An explicitly-set pointer wins over the
/// convention, both halves independently, so a user who moved either home is
/// served by the same rule.
#[cfg_attr(not(windows), allow(dead_code))]
fn rust_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    let Some(cargo_home) = p.dir.parent() else {
        return n;
    };
    n.state(
        m,
        m.path_var("CARGO_HOME")
            .unwrap_or_else(|| cargo_home.to_path_buf()),
        "CARGO_HOME",
        "the crate cache and registry index this toolchain reads",
    );
    // `<profile>` — the directory `.cargo` sits in, which is where rustup puts
    // `.rustup` too, because both default to the same `$HOME`.
    if let Some(rustup) = m
        .path_var("RUSTUP_HOME")
        .or_else(|| cargo_home.parent().map(|p| p.join(".rustup")))
    {
        n.state(
            m,
            rustup,
            "RUSTUP_HOME",
            "the toolchains the rustup shim resolves rustc through",
        );
    }
    n
}

/// Python: the install root behind a `Scripts` launcher directory, plus the two
/// caches that would otherwise be written outside the boundary.
///
/// The root's grant is **inheritable**, so `Lib`, `DLLs` and `site-packages`
/// under it are covered by that one ACE — listing them as rows of their own
/// would stamp three more durable changes on the user's machine to reach
/// directories the first grant already reaches.
#[cfg_attr(not(windows), allow(dead_code))]
fn python_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    // The install root: `…\Scripts\tool.exe` → its parent; `…\python.exe` →
    // its own directory, which the engine ALREADY grants, so only the first
    // case asks for anything. That asymmetry is deliberate and it is what keeps
    // this row off `…\Microsoft\WindowsApps\python.exe`: an alias directory is
    // not an install root and must not collect an ACE on the strength of
    // holding a file called `python.exe`.
    let scripts = p.dir_named("scripts");
    let root = if scripts { p.dir.parent() } else { Some(p.dir) };
    if let Some(root) = root {
        // A `Scripts` directory sitting at a volume root would yield the volume
        // itself; the answer there is no grant, not the whole drive. (The
        // refusal screen would catch it too — this keeps the row from ever
        // asking.)
        if root.parent().is_some() {
            if scripts {
                n.tree(
                    m,
                    root.to_path_buf(),
                    "the interpreter root a pip console-script stub loads `python3XX.dll` and the \
                     standard library from",
                );
            }
            // A Windows virtual environment has `Scripts` and `Lib` but no
            // `DLLs` — its interpreter is a shim onto a BASE install named by
            // `pyvenv.cfg`'s `home`, which is not derivable from any path or
            // pointer this rule is allowed to read. Saying so is the whole
            // point: without the base install granted the stub exits silently,
            // which is the exact failure this table was built to stop being
            // invisible.
            if (m.is_dir)(&root.join("Lib")) && !(m.is_dir)(&root.join("DLLs")) {
                n.gap(
                    root.display().to_string(),
                    "this looks like a virtual environment (a `Lib` but no `DLLs`): its BASE \
                     interpreter is named by `pyvenv.cfg`'s `home` and is NOT granted. If the \
                     tool exits with no output, add that directory under \
                     Settings ▸ Sandboxing ▸ extra grants",
                );
            }
        }
    }
    n.scratch(
        "PYTHONPYCACHEPREFIX",
        "pycache",
        "so the interpreter's bytecode cache lands in the sandbox's one writable place instead of \
         being denied beside a read-only standard library",
    );
    n.scratch(
        "PIP_CACHE_DIR",
        "pip",
        "pip's download cache — the real one lives in the profile, which the boundary does not \
         open for writing",
    );
    n
}

/// Node: the runtime a JS tool shim starts, its cache, and the symlink flags S1
/// measured npm needing.
#[cfg_attr(not(windows), allow(dead_code))]
fn node_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    // `node.exe` itself: its own directory is already granted, so there is
    // nothing to add. Anything else (a `.cmd` shim, a `node_modules\.bin`
    // entry) starts a node that lives somewhere the boundary has never heard
    // of, and the only pointer to it that cImp is allowed to read is npm's own
    // prefix.
    let is_node = p.file == "node.exe" || p.file == "node";
    if !is_node {
        match m
            .dir_var("npm_config_prefix")
            .or_else(|| m.dir_var("NPM_CONFIG_PREFIX"))
        {
            Some(prefix) => n.tree(
                m,
                prefix,
                "the Node runtime this shim starts — npm's global prefix, which is where its \
                 `node.exe` and global packages live",
            ),
            None => n.gap(
                "node.exe",
                "the Node runtime this shim starts could not be inferred (no `npm_config_prefix` \
                 is set). If node is under Program Files it is already readable; otherwise add \
                 its directory under Settings ▸ Sandboxing ▸ extra grants. cImp will NOT grant \
                 `%USERPROFILE%` or a volume root to find it",
            ),
        }
    }
    n.scratch(
        "npm_config_cache",
        "npm",
        "npm's cache — it is written on every install, and the real one lives in the profile the \
         boundary keeps read-only",
    );
    n.literal(
        "NODE_OPTIONS",
        "--preserve-symlinks --preserve-symlinks-main",
        "S1-measured: npm's shims are symlinks, and resolving through them walks the container \
         into the ancestor-canonicalization wall unless node keeps the link path",
    );
    n
}

/// Java: the JDK/JRE tree behind a launcher in `bin`, or the one `JAVA_HOME`
/// already names.
#[cfg_attr(not(windows), allow(dead_code))]
fn java_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    // Only a real JVM launcher may derive its home from its own layout. A
    // launcher SCRIPT (`pmd.bat`) also lives in a `bin`, and deriving from it
    // would set `JAVA_HOME` to the tool's own directory — which is not merely
    // useless, it is actively wrong.
    let is_jvm = matches!(
        p.file.as_str(),
        "java.exe"
            | "javaw.exe"
            | "javac.exe"
            | "jar.exe"
            | "jshell.exe"
            | "javadoc.exe"
            | "java"
            | "javac"
    );
    let home = if is_jvm && p.dir_named("bin") {
        p.dir.parent().map(Path::to_path_buf)
    } else {
        None
    }
    .or_else(|| m.dir_var("JAVA_HOME"));
    match home {
        Some(home) => n.state(
            m,
            home,
            "JAVA_HOME",
            "the JDK/JRE tree — `lib\\modules` (the runtime image), `conf` and the JNI DLLs \
             beside the launcher, without which a JVM does not start",
        ),
        None => n.gap(
            "JAVA_HOME",
            "this is a JVM launcher and no JDK could be inferred — its own directory is granted \
             but the runtime it starts is not. Set JAVA_HOME, or add the JDK directory under \
             Settings ▸ Sandboxing ▸ extra grants",
        ),
    }
    n
}

/// .NET: the SDK's CLI state moves into the root; the package cache does not.
///
/// **The judgement call, stated.** `NUGET_PACKAGES` gets a read+execute grant
/// on the REAL cache rather than a redirect into the sandbox root, because a
/// redirect makes every restore start from an empty cache — and the boundary
/// denies egress by default, so "start from empty" means "fail". Read-only is
/// enough for restore-from-cache; a restore that must ADD a package fails with
/// a denial the classifier recognizes, which is the honest outcome for an
/// offline boundary. The cost is one first-time ACL walk of the package cache,
/// and it was measured before this row shipped (2026-08-19, this machine):
/// `~\.nuget\packages` is 1,472 files / 859 directories and the Go module cache
/// is 10,463 / 1,402 — both an order of magnitude *below* the `.rustup` tree
/// (54,457 files) whose ~10 s stamp the ladder already absorbs inside
/// [`PREPARE_BACKSTOP`]. A machine with a far larger cache degrades the way any
/// other slow grant does: unsandboxed, loudly, once.
#[cfg_attr(not(windows), allow(dead_code))]
fn dotnet_needs(_p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    if let Some(pkgs) = m.path_var("NUGET_PACKAGES").or_else(|| {
        m.home()
            .map(|h| h.join(".nuget").join("packages"))
    }) {
        n.state(
            m,
            pkgs,
            "NUGET_PACKAGES",
            "the global package cache `dotnet restore` reads — read-only, because redirecting it \
             into the root would make every restore re-download a graph the boundary denies \
             egress for",
        );
    }
    n.scratch(
        "DOTNET_CLI_HOME",
        "dotnet",
        "S1-named: the SDK writes its first-run sentinel and extracted bundles here, and the real \
         one is in the profile the boundary keeps read-only",
    );
    n.literal(
        "DOTNET_CLI_TELEMETRY_OPTOUT",
        "1",
        "the telemetry uploader is the one part of a build that reaches the network; with egress \
         denied its retries would be the loudest thing in the log and none of it is wanted",
    );
    n
}

/// Go: everything Go writes moves into the root; the module cache it READS
/// stays where it is.
#[cfg_attr(not(windows), allow(dead_code))]
fn go_needs(p: &Program, m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    // `go.exe` in a `bin` implies GOROOT one level up. On the standard install
    // that is `C:\Program Files\Go`, which ALL APPLICATION PACKAGES already
    // reads — the grant is then a no-op and costs nothing.
    let is_go = matches!(p.file.as_str(), "go.exe" | "gofmt.exe" | "go" | "gofmt");
    let goroot = if is_go && p.dir_named("bin") {
        p.dir.parent().map(Path::to_path_buf)
    } else {
        None
    }
    .or_else(|| m.dir_var("GOROOT"));
    if let Some(goroot) = goroot {
        if goroot.parent().is_some() {
            n.tree(
                m,
                goroot,
                "the Go toolchain tree — `pkg`, `src` and the compiler binaries the driver execs",
            );
        }
    } else if !is_go {
        n.gap(
            "GOROOT",
            "this tool drives the Go toolchain and no GOROOT could be inferred. If Go is under \
             Program Files it is already readable; otherwise set GOROOT or add the directory \
             under Settings ▸ Sandboxing ▸ extra grants",
        );
    }
    // The module cache is READ by an offline build and Go marks its contents
    // read-only itself, so a read+execute grant on the real one is both
    // sufficient and honest — while GOPATH below still moves, so anything Go
    // WRITES (`go install` output, the build cache) lands inside the root.
    if let Some(modcache) = m.path_var("GOMODCACHE").or_else(|| {
        m.path_var("GOPATH")
            .or_else(|| m.home().map(|h| h.join("go")))
            .map(|g| g.join("pkg").join("mod"))
    }) {
        n.state(
            m,
            modcache,
            "GOMODCACHE",
            "the module cache an offline `go build` resolves its dependencies from — read-only, \
             which is how Go marks it anyway",
        );
    }
    n.scratch(
        "GOCACHE",
        "gocache",
        "S1-verified: the build cache is written on every compile and must be inside the one \
         writable place",
    );
    n.scratch(
        "GOTMPDIR",
        "gotmp",
        "S1-verified: the linker's scratch, for the same reason",
    );
    n.scratch(
        "GOPATH",
        "gopath",
        "S1-verified: everything else Go writes (`go install` output, `bin`) lands here; the \
         module cache is pointed back at the real one by GOMODCACHE above",
    );
    n
}

/// The Store aliases: a row whose entire content is a gap.
#[cfg_attr(not(windows), allow(dead_code))]
fn store_alias_needs(p: &Program, _m: &Machine) -> RuntimeNeeds {
    let mut n = RuntimeNeeds::default();
    n.gap(
        p.dir.display().to_string(),
        "an app-execution-alias directory: the alias is a reparse point in unlistable profile \
         territory, so a sandboxed PATH search never resolves it. S1 measured this and found no \
         workaround short of installing a real interpreter — no grant fixes it, and cImp will not \
         open the profile to try",
    );
    n
}

/// One runtime's needs after the screen, ready for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RuntimeMatch {
    pub runtime: &'static str,
    pub why: &'static str,
    pub needs: RuntimeNeeds,
}

/// **The entry point.** Everything [`RUNTIME_PROFILES`] infers for one program,
/// screened.
///
/// Pure and IO-free — `m` is the whole of the machine this is allowed to see —
/// so the engines can be trusted with it and the tests can drive it on either
/// platform without a filesystem.
///
/// # The screen
///
/// Every path a row produces goes through [`extra_grant_refusal`], the same
/// rules a settings-supplied grant row gets, and a refused path is DROPPED —
/// with a [`RuntimeGap`] the engine records — while every other grant from the
/// same row still applies. An environment pointer that named a refused
/// directory is dropped with it: handing a tool a pointer to a directory the
/// container cannot read only converts a clean failure into a confusing one.
///
/// This is defence in depth and it is deliberate. [`GrantHints`] rows are cImp
/// constants a reviewer approved; these paths are INFERRED from environment
/// variables and directory names, which is exactly the class of input that
/// should not be trusted with a durable inheritable ACE.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn runtime_needs(program: &Path, m: &Machine) -> Vec<RuntimeMatch> {
    let Some(p) = Program::at(program) else {
        return Vec::new();
    };
    let detected: Vec<&'static RuntimeProfile> = RUNTIME_PROFILES
        .iter()
        .filter(|profile| profile.detect.iter().any(|d| d.matches(&p)))
        .collect();
    screened_needs(&detected, &p, m)
}

/// Which runtime profiles **detection** fires for, by id — the raw answer,
/// before any screening drops a refused grant.
///
/// V38 Phase C's declaration/inference cross-check reads this: a manifest that
/// declares `runtime: node` for a program detection sees as `python` is drift
/// worth a row, and the comparison has to be about what fired, not about what
/// survived the screen (a profile whose every grant was refused still fired).
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn inferred_runtime_ids(program: &Path, m: &Machine) -> Vec<&'static str> {
    let _ = m;
    let Some(p) = Program::at(program) else {
        return Vec::new();
    };
    RUNTIME_PROFILES
        .iter()
        .filter(|profile| profile.detect.iter().any(|d| d.matches(&p)))
        .map(|profile| profile.id)
        .collect()
}

/// Which runtime profiles apply to one spawn — inference, a DECLARED profile,
/// or none at all (V38 Phase C's manifest `runtime` field).
///
/// `Profile` takes the row's `id` rather than a path for the reason the
/// manifest field is a closed enum at all: the value selects from a table cImp
/// owns, so the worst a lying manifest achieves is a grant the user can see
/// named at enable time. An id no row carries selects nothing — a manifest from
/// a newer cImp asks for a runtime this build has no rules for, and inventing
/// one would be worse than the gap.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn runtime_matches(select: &RuntimeSelect, program: &Path, m: &Machine) -> Vec<RuntimeMatch> {
    match select {
        RuntimeSelect::Infer => runtime_needs(program, m),
        RuntimeSelect::None => Vec::new(),
        RuntimeSelect::Profile(id) => {
            let Some(p) = Program::at(program) else {
                return Vec::new();
            };
            let declared: Vec<&'static RuntimeProfile> = RUNTIME_PROFILES
                .iter()
                .filter(|profile| profile.id == *id)
                .collect();
            screened_needs(&declared, &p, m)
        }
    }
}

/// The screen every profile's needs pass, whichever way the profile was chosen.
///
/// Split out of [`runtime_needs`] so a DECLARED profile cannot take a shorter
/// path to a grant than an inferred one: the manifest is attacker-controlled
/// input and its declaration selects a row, never a rule.
#[cfg_attr(not(windows), allow(dead_code))]
fn screened_needs(
    profiles: &[&'static RuntimeProfile],
    p: &Program,
    m: &Machine,
) -> Vec<RuntimeMatch> {
    let home = m.home();
    let system_root = m.system_root();
    let mut out: Vec<RuntimeMatch> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for profile in profiles {
        let RuntimeNeeds {
            grants,
            env,
            mut gaps,
        } = (profile.needs)(p, m);
        let mut kept = Vec::new();
        let mut refused: Vec<PathBuf> = Vec::new();
        for g in grants {
            match extra_grant_refusal(&g.dir, home.as_deref(), system_root.as_deref()) {
                Some(why) => {
                    gaps.push(RuntimeGap {
                        what: g.dir.display().to_string(),
                        why,
                    });
                    refused.push(g.dir);
                }
                // Two rows can derive the same directory (a Go tool and `go`
                // itself); one grant, one row.
                None if !seen.contains(&g.dir) => {
                    seen.push(g.dir.clone());
                    kept.push(g);
                }
                None => {}
            }
        }
        let env: Vec<RuntimeVar> = env
            .into_iter()
            .filter(|v| match &v.value {
                RuntimeEnv::Dir(d) => !refused.contains(d),
                _ => true,
            })
            .collect();
        let needs = RuntimeNeeds {
            grants: kept,
            env,
            gaps,
        };
        if !needs.is_empty() {
            out.push(RuntimeMatch {
                runtime: profile.id,
                why: profile.why,
                needs,
            });
        }
    }
    out
}

/// The complete environment the sandbox engine overrides on a child, **in the
/// one order that works**.
///
/// 1. `TEMP`/`TMP` — scratch into the mapped root, the only writable place;
/// 2. `HOME`/`USERPROFILE` — the home redirect, so a child that writes config
///    lands there too and `getcwd` stays shallow;
/// 3. every runtime pointer, LAST.
///
/// **(3) after (2) is the invariant**, not a detail: a toolchain resolving
/// `%USERPROFILE%\.cargo` after the redirect finds an empty scratch directory,
/// so the pointers that undo that must be applied where the redirect cannot
/// reach them. Same last-writer-wins composition the seams use
/// ([`child_env::ChildEnv`]), so a seam that forces one of these itself still
/// loses to the engine — which is correct: the engine is the half that knows
/// what the container can reach.
///
/// A free function here rather than inline in the engine so the order is a
/// property with a test on both platforms, instead of four lines in the middle
/// of a Win32 routine no Linux run ever compiles.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn compose_env_overrides(
    drive_root: &Path,
    matches: &[RuntimeMatch],
) -> Vec<(String, std::ffi::OsString)> {
    let mut out: Vec<(String, std::ffi::OsString)> = Vec::new();
    for name in ["TEMP", "TMP", "HOME", "USERPROFILE"] {
        out.push((name.to_string(), drive_root.as_os_str().to_os_string()));
    }
    let scratch = drive_root.join(SANDBOX_SCRATCH_DIR);
    for m in matches {
        for v in &m.needs.env {
            let value = match &v.value {
                RuntimeEnv::Dir(d) => d.as_os_str().to_os_string(),
                RuntimeEnv::Scratch(sub) => scratch.join(sub).into_os_string(),
                RuntimeEnv::Literal(s) => std::ffi::OsString::from(*s),
            };
            out.push((v.name.to_string(), value));
        }
    }
    out
}
